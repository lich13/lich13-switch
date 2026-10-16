//! Read-only model discovery. Shares connection policy, never business admission.
use super::{
    circuit, connector,
    quota::{now, Query},
    replay,
};
use crate::errors::code;
use crate::storage::{AppError, Result};
use http_body_util::BodyExt;
use hyper::{header, Request};
use serde::Serialize;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::{Mutex as AsyncMutex, Semaphore};

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct View {
    pub provider_id: String,
    pub version: String,
    pub models: Vec<String>,
    pub checked_at: Option<u64>,
    pub stale: bool,
    pub error: Option<String>,
    pub retry_at: Option<u64>,
}
struct Entry {
    view: View,
    completed: Option<Instant>,
}
pub struct Service {
    entries: Mutex<HashMap<String, Arc<AsyncMutex<Entry>>>>,
    permits: Semaphore,
}
impl Service {
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            permits: Semaphore::new(3),
        }
    }
    pub async fn query_with_connection(
        &self,
        input: Query,
        force: bool,
        connection: Option<&super::grok_config::Connection>,
    ) -> Result<View> {
        let began = Instant::now();
        let key = format!("{}:{}", input.id, input.version);
        let entry = {
            let mut entries = self.entries.lock().unwrap();
            entries.retain(|k, _| !k.starts_with(&format!("{}:", input.id)) || k == &key);
            entries
                .entry(key)
                .or_insert_with(|| {
                    Arc::new(AsyncMutex::new(Entry {
                        view: View {
                            provider_id: input.id.clone(),
                            version: input.version.clone(),
                            models: vec![],
                            checked_at: None,
                            stale: false,
                            error: None,
                            retry_at: None,
                        },
                        completed: None,
                    }))
                })
                .clone()
        };
        let mut entry = entry.lock().await;
        if entry.completed.is_some_and(|t| t >= began)
            || entry.view.retry_at.is_some_and(|t| t > now())
            || (!force
                && entry
                    .completed
                    .is_some_and(|t| t.elapsed() < Duration::from_secs(300)))
        {
            return Ok(entry.view.clone());
        }
        let _permit = self
            .permits
            .acquire()
            .await
            .map_err(|_| AppError::new(code::MODELS, "模型查询已停止"))?;
        let result = tokio::time::timeout(Duration::from_secs(10), fetch(&input, connection)).await;
        entry.view.checked_at = Some(now());
        entry.view.retry_at = None;
        match result {
            Ok(Ok(models)) => {
                entry.view.models = models;
                entry.view.stale = false;
                entry.view.error = None;
            }
            failure => {
                let (message, retry) = match failure {
                    Ok(Err(e)) => e,
                    _ => ("模型列表查询超时".into(), None),
                };
                entry.view.stale = true;
                entry.view.error = Some(message);
                entry.view.retry_at = retry;
            }
        }
        entry.completed = Some(Instant::now());
        Ok(entry.view.clone())
    }
}
type Failure = (String, Option<u64>);
async fn fetch(
    input: &Query,
    connection: Option<&super::grok_config::Connection>,
) -> std::result::Result<Vec<String>, Failure> {
    let fail = |message: &str| (message.to_owned(), None);
    let claude = input.client_id == super::ClientId::Claude
        || connection.is_some_and(|c| c.protocol == super::grok_config::Protocol::Messages);
    let target = super::forward::target(
        &input.base,
        &"/v1/models".parse().expect("static model path"),
    )
    .map_err(|_| fail("模型接口地址无效"))?;
    let target = if let Some(connection) = connection {
        connection
            .query_uri(target)
            .map_err(|_| fail("模型接口地址无效"))?
    } else {
        target
    };
    let mut url = url::Url::parse(&target.to_string()).map_err(|_| fail("模型接口地址无效"))?;
    let mut models = Vec::new();
    let mut total = 0usize;
    let mut cursors = std::collections::HashSet::new();
    loop {
        let mut builder = Request::builder()
            .method("GET")
            .uri(url.as_str())
            .header(header::AUTHORIZATION, format!("Bearer {}", input.token))
            .header(header::ACCEPT, "application/json")
            .header(header::ACCEPT_ENCODING, "identity");
        if claude {
            builder = builder.header("anthropic-version", "2023-06-01");
        }
        let mut request = builder
            .body(replay::empty())
            .map_err(|_| fail("供应商地址或 Key 格式无效"))?;
        if let Some(connection) = connection {
            connection
                .authenticate(request.headers_mut(), &input.token)
                .map_err(|_| fail("供应商请求头无效"))?;
        }
        let mut response = input.client.request(request).await.map_err(|e| {
            fail(
                &connector::classify(&e)
                    .map(|e| e.to_string())
                    .unwrap_or_else(|| "模型列表连接失败".into()),
            )
        })?;
        let status = response.status().as_u16();
        if status == 429 {
            let seconds = response
                .headers()
                .get(header::RETRY_AFTER)
                .and_then(|h| h.to_str().ok())
                .and_then(circuit::retry_after)
                .map(|d| d.as_secs().max(1))
                .unwrap_or(60);
            return Err(("模型列表查询被限流".into(), Some(now() + seconds)));
        }
        if (300..400).contains(&status) {
            return Err(fail("模型接口要求跳转，已停止发送凭据"));
        }
        if matches!(status, 401 | 403) {
            return Err(fail(&format!("模型列表认证失败（HTTP {status}）")));
        }
        if !(200..300).contains(&status) {
            return Err(fail(&format!("模型列表不可用（HTTP {status}）")));
        }
        let mut bytes = Vec::new();
        while let Some(frame) = response.body_mut().frame().await {
            let frame = frame.map_err(|_| fail("模型列表响应中断"))?;
            if let Some(data) = frame.data_ref() {
                if total + data.len() > 2_000_000 {
                    return Err(fail("模型列表超过 2 MB"));
                }
                total += data.len();
                bytes.extend_from_slice(data);
            }
        }
        let value: serde_json::Value =
            serde_json::from_slice(&bytes).map_err(|_| fail("模型接口没有返回有效 JSON"))?;
        let data = value
            .get("data")
            .and_then(|v| v.as_array())
            .ok_or_else(|| fail("无法识别模型列表，可手动添加模型 ID"))?;
        let page: Vec<String> = data
            .iter()
            .filter_map(|v| v.get("id").and_then(|v| v.as_str()))
            .filter(|id| {
                !id.is_empty()
                    && id.len() <= 256
                    && !id.chars().any(char::is_control)
                    && !id.contains(&input.token)
            })
            .map(str::to_owned)
            .collect();
        models.extend(page);
        if !claude || value.get("has_more").and_then(|v| v.as_bool()) != Some(true) {
            break;
        }
        let cursor = value
            .get("last_id")
            .and_then(|v| v.as_str())
            .filter(|v| !v.is_empty() && v.len() <= 256)
            .ok_or_else(|| fail("模型分页标记无效"))?;
        if !cursors.insert(cursor.to_owned()) || cursors.len() > 100 {
            return Err(fail("模型分页重复或超出上限"));
        }
        let pairs: Vec<_> = url
            .query_pairs()
            .filter(|(key, _)| key != "after_id")
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect();
        url.query_pairs_mut()
            .clear()
            .extend_pairs(pairs)
            .append_pair("after_id", cursor);
    }
    models.sort();
    models.dedup();
    Ok(models)
}
