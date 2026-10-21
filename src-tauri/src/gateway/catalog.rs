//! Read-only model discovery. Shares connection policy, never business admission.
use super::{
    circuit, connector,
    quota::{now, Query},
    replay,
};
use crate::errors::code;
use crate::storage::{self, AppError, Result};
use http_body_util::BodyExt;
use hyper::{header, Request};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::{Mutex as AsyncMutex, Semaphore};

const LIMIT: usize = 2 * 1024 * 1024;
const UNAVAILABLE: &str = "模型缓存不可用，可手动刷新";
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct View {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, crate::models_dev::Metadata>,
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
    path: PathBuf,
    disk: Mutex<(File, bool)>,
}
#[derive(Clone, Serialize, Deserialize)]
struct File {
    version: u8,
    providers: BTreeMap<String, View>,
}
impl Default for File {
    fn default() -> Self {
        Self {
            version: 1,
            providers: BTreeMap::new(),
        }
    }
}
impl Service {
    pub fn new(path: PathBuf) -> Self {
        let loaded = storage::read_optional(&path).and_then(|bytes| {
            let file: File = bytes
                .map(|bytes| {
                    serde_json::from_slice(&bytes)
                        .map_err(|_| AppError::new(code::MODELS, UNAVAILABLE))
                })
                .transpose()?
                .unwrap_or_default();
            if file.version != 1
                || file.providers.iter().any(|(id, v)| {
                    id != &v.provider_id
                        || v.models.len() > 4096
                        || v.models.iter().any(|id| {
                            id.is_empty() || id.len() > 256 || id.chars().any(char::is_control)
                        })
                })
            {
                return Err(AppError::new(code::MODELS, UNAVAILABLE));
            }
            Ok(file)
        });
        let corrupt = loaded.is_err();
        Self {
            entries: Mutex::new(HashMap::new()),
            permits: Semaphore::new(3),
            path,
            disk: Mutex::new((loaded.unwrap_or_default(), corrupt)),
        }
    }
    pub fn cached(&self, id: &str, version: &str) -> View {
        let disk = self.disk.lock().unwrap();
        let mut view = disk.0.providers.get(id).cloned().unwrap_or(View {
            metadata: BTreeMap::new(),
            provider_id: id.into(),
            version: version.into(),
            models: vec![],
            checked_at: None,
            stale: false,
            error: None,
            retry_at: None,
        });
        view.stale = view.version != version;
        if disk.1 {
            view.error = Some(UNAVAILABLE.into());
            view.stale = true;
        }
        view
    }
    /// Called only while the gateway holds its current provider/configuration lock.
    pub fn commit(&self, view: &View) -> Result<()> {
        let mut disk = self.disk.lock().unwrap();
        if disk.1 && view.error.is_some() {
            return Ok(());
        }
        let mut next = disk.0.clone();
        next.providers
            .insert(view.provider_id.clone(), view.clone());
        self.write(&next)?;
        *disk = (next, false);
        Ok(())
    }
    fn write(&self, file: &File) -> Result<()> {
        let bytes = serde_json::to_vec(file)
            .map_err(|_| AppError::new(code::MODELS, "无法保存模型缓存"))?;
        storage::replace_cache_bounded(&self.path, &bytes, LIMIT as u64)
    }
    pub fn remove(&self, ids: &[String]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let mut disk = self.disk.lock().unwrap();
        // Keep a damaged file for recovery rather than replacing it with an empty cache.
        if disk.1 {
            return Ok(());
        }
        let mut next = disk.0.clone();
        next.providers.retain(|id, _| !ids.contains(id));
        self.write(&next)?;
        disk.0 = next;
        Ok(())
    }
    pub async fn query_with_connection(
        &self,
        input: Query,
        force: bool,
        connection: Option<&super::grok_config::Connection>,
    ) -> Result<View> {
        if !force {
            return Ok(self.cached(&input.id, &input.version));
        }
        let began = Instant::now();
        let key = format!("{}:{}", input.id, input.version);
        let entry = {
            let mut entries = self.entries.lock().unwrap();
            entries.retain(|k, _| !k.starts_with(&format!("{}:", input.id)) || k == &key);
            entries
                .entry(key)
                .or_insert_with(|| {
                    Arc::new(AsyncMutex::new(Entry {
                        view: self.cached(&input.id, &input.version),
                        completed: None,
                    }))
                })
                .clone()
        };
        let mut entry = entry.lock().await;
        if entry.completed.is_some_and(|t| t >= began)
            || entry.view.retry_at.is_some_and(|t| t > now())
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
                entry.view.version = input.version.clone();
                entry.view.models = models;
                entry.view.stale = false;
                entry.view.error = None;
            }
            failure => {
                let (message, retry) = match failure {
                    Ok(Err(e)) => e,
                    _ => ("模型列表查询超时".into(), None),
                };
                entry.view.stale = entry.view.version != input.version;
                entry.view.error = Some(message);
                entry.view.retry_at = retry;
            }
        }
        entry.completed = Some(Instant::now());
        Ok(entry.view.clone())
    }
}
type Failure = (String, Option<u64>);

pub(super) fn local_list(
    gateway: &super::Gateway,
    uri: &hyper::Uri,
    headers: &hyper::HeaderMap,
) -> Option<hyper::Response<replay::WireBody>> {
    if gateway.client_id() == super::ClientId::Codex
        || !matches!(uri.path(), "/models" | "/v1/models")
    {
        return None;
    }
    let pairs: Vec<_> = url::form_urlencoded::parse(uri.query().unwrap_or("").as_bytes()).collect();
    if pairs.iter().any(|(key, _)| key == "client_version") {
        return None;
    }
    let anthropic =
        gateway.client_id() == super::ClientId::Claude || headers.contains_key("anthropic-version");
    let selected_protocol = if gateway.client_id() == super::ClientId::Dsh {
        gateway
            .0
            .inner
            .lock()
            .unwrap()
            .store
            .dsh
            .as_ref()
            .map(|s| s.options.protocol)
    } else {
        None
    };
    let models = match gateway.model_union(selected_protocol) {
        Ok(models) if models.is_empty() => return None,
        Ok(models) => models,
        Err(e) => {
            return Some(super::forward::error(
                hyper::StatusCode::SERVICE_UNAVAILABLE,
                e.code.as_str(),
                &e.message,
            ))
        }
    };
    let mut value = if anthropic {
        match anthropic_list(&models, &pairs) {
            Ok(value) => value,
            Err(()) => {
                return Some(super::forward::error(
                    hyper::StatusCode::BAD_REQUEST,
                    code::MODELS,
                    "模型分页参数无效",
                ))
            }
        }
    } else {
        serde_json::json!({"object":"list", "data":models.iter().map(|id| serde_json::json!({
            "id":id,"object":"model","created":0,"owned_by":"lich13-switch"
        })).collect::<Vec<_>>()})
    };
    if anthropic {
        let usage = gateway.0.usage.lock().unwrap().clone();
        let prices = usage.as_ref().and_then(|u| u.prices().ok());
        if let Some(prices) = prices {
            for model in value["data"].as_array_mut().into_iter().flatten() {
                if let Some(name) = model["id"]
                    .as_str()
                    .and_then(|id| prices.models_dev.metadata(id))
                    .and_then(|m| m.name)
                {
                    model["display_name"] = name.into();
                }
            }
        }
    }
    Some(
        hyper::Response::builder()
            .header(header::CONTENT_TYPE, "application/json")
            .body(replay::full(
                serde_json::to_vec(&value).expect("model list JSON"),
            ))
            .expect("model list response"),
    )
}
fn anthropic_list(
    models: &[String],
    pairs: &[(std::borrow::Cow<'_, str>, std::borrow::Cow<'_, str>)],
) -> std::result::Result<serde_json::Value, ()> {
    let get = |key: &str| -> std::result::Result<Option<&str>, ()> {
        let mut values = pairs
            .iter()
            .filter(|(name, _)| name == key)
            .map(|(_, value)| value.as_ref());
        let first = values.next();
        if values.next().is_some() {
            return Err(());
        }
        Ok(first)
    };
    let limit = get("limit")?
        .map(str::parse::<usize>)
        .transpose()
        .map_err(|_| ())?
        .unwrap_or(1000);
    if !(1..=1000).contains(&limit) {
        return Err(());
    }
    let after = get("after_id")?;
    let before = get("before_id")?;
    if after.is_some() && before.is_some() {
        return Err(());
    }
    let (start, end) = if let Some(id) = before {
        let end = models.iter().position(|v| v == id).ok_or(())?;
        (end.saturating_sub(limit), end)
    } else {
        let start = after
            .map(|id| models.iter().position(|v| v == id).map(|n| n + 1).ok_or(()))
            .transpose()?
            .unwrap_or(0);
        (start, (start + limit).min(models.len()))
    };
    let page = &models[start..end];
    Ok(serde_json::json!({
        "data":page.iter().map(|id| serde_json::json!({"id":id,"type":"model","display_name":id,
            "created_at":"1970-01-01T00:00:00Z"})).collect::<Vec<_>>(),
        "first_id":page.first(), "last_id":page.last(), "has_more":if before.is_some(){start>0}else{end<models.len()}
    }))
}
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
        if models.len() > 4096 {
            return Err(fail("单供应商模型列表超过 4,096 个"));
        }
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

#[cfg(test)]
mod v02514_tests {
    use super::super::{
        model::{Edit, Store},
        ClientId, Gateway,
    };
    use super::*;
    #[test]
    fn corrupt_and_oversized_cache_is_preserved_until_success_and_reopens() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("provider-models.json");
        for bad in [b"broken".to_vec(), vec![b'x'; LIMIT + 1]] {
            std::fs::write(&path, &bad).unwrap();
            let service = Service::new(path.clone());
            let mut view = service.cached("provider", "v1");
            assert!(view.stale);
            assert_eq!(view.error.as_deref(), Some(UNAVAILABLE));
            service.commit(&view).unwrap();
            assert_eq!(std::fs::read(&path).unwrap(), bad);
            view.error = None;
            view.stale = false;
            view.models = vec!["Model-A".into(), "model-a".into()];
            view.checked_at = Some(1);
            service.commit(&view).unwrap();
            let reopened = Service::new(path.clone());
            assert_eq!(reopened.cached("provider", "v1").models, view.models);
            assert!(reopened.cached("provider", "v2").stale);
            reopened.remove(&["provider".into()]).unwrap();
            assert!(Service::new(path.clone())
                .cached("provider", "v1")
                .models
                .is_empty());
        }
    }
    #[tokio::test]
    async fn union_is_ordered_exact_and_client_isolated_with_paginated_anthropic_lists() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("claude");
        storage::private_dir(&data).unwrap();
        let mut store = Store::default();
        for (n, models) in [vec!["Model-A", "shared"], vec!["shared", "model-a"]]
            .into_iter()
            .enumerate()
        {
            store
                .edit(
                    Edit::SaveProvider {
                        id: None,
                        base_url: format!("https://p{n}.example.invalid"),
                        token: format!("fixture-{n}"),
                        name: None,
                    },
                    false,
                )
                .unwrap();
            let p = store.providers.last_mut().unwrap();
            p.queued = true;
            p.allowed_models = Some(models.into_iter().map(str::to_owned).collect());
        }
        store
            .persist(&data.join("gateway.json"), "missing")
            .unwrap();
        let gateway = Gateway::new_client(data, ClientId::Claude, None).unwrap();
        assert_eq!(
            gateway.model_union(None).unwrap(),
            ["Model-A", "shared", "model-a"]
        );
        let response = local_list(
            &gateway,
            &"/v1/models?limit=1&after_id=Model-A".parse().unwrap(),
            &hyper::HeaderMap::new(),
        )
        .unwrap();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["data"][0]["id"], "shared");
        assert_eq!(body["has_more"], true);
        assert_eq!(body["data"][0]["created_at"], "1970-01-01T00:00:00Z");
        for uri in ["/v1/models?client_version=1", "/v1/models/specific"] {
            assert!(
                local_list(&gateway, &uri.parse().unwrap(), &hyper::HeaderMap::new()).is_none()
            );
        }
        let codex = Gateway::new(dir.path().join("codex")).unwrap();
        assert!(codex.model_union(None).unwrap().is_empty());
        assert!(local_list(
            &codex,
            &"/v1/models".parse().unwrap(),
            &hyper::HeaderMap::new()
        )
        .is_none());
        assert_eq!(
            local_list(
                &gateway,
                &"/models?limit=0".parse().unwrap(),
                &hyper::HeaderMap::new()
            )
            .unwrap()
            .status(),
            400
        );
    }
}
