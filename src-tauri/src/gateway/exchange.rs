//! Sub2API account exchange. Secrets stay in Rust and user-selected files.
use super::{model, ClientId, Edit, Gateway, View};
use crate::errors::code;
use crate::exchange_template::{self as template, Prepared};
use crate::storage::{self, AppError, Result};
use serde::Serialize;
use serde_json::Value;
use std::{
    collections::HashSet,
    path::Path,
    sync::Mutex,
    time::{Duration, Instant},
};

#[derive(Default)]
pub struct Service {
    pending: Mutex<Option<Pending>>,
    templates: template::Service,
}
impl Service {
    pub(super) fn prune(&self) {
        self.templates.prune();
        let mut pending = self.pending.lock().unwrap();
        if pending
            .as_ref()
            .is_some_and(|p| p.at.elapsed() >= Duration::from_secs(900))
        {
            pending.take();
        }
    }
}
struct Pending {
    token: String,
    at: Instant,
    providers: Vec<Option<model::Provider>>,
}
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Row {
    pub id: usize,
    pub name: String,
    pub base_url: Option<String>,
    pub concurrency: Option<u32>,
    pub error: Option<String>,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Preview {
    pub token: String,
    pub revision: String,
    pub rows: Vec<Row>,
    pub unsupported_proxies: usize,
    pub template: Option<Prepared>,
}
fn invalid(message: &str) -> AppError {
    AppError::new(code::SUB2API_FILE, message)
}
fn read(path: &Path) -> Result<Value> {
    let value = template::read(path)?;
    template::validate(&value)?;
    Ok(value)
}
fn platform(client: ClientId) -> &'static str {
    if client != ClientId::Claude {
        "openai"
    } else {
        "anthropic"
    }
}
fn provider(v: &Value, client: ClientId, port: u16) -> Result<model::Provider> {
    if v.get("type").and_then(Value::as_str) != Some("apikey") {
        return Err(invalid("仅支持 API Key，OAuth 账号不支持"));
    }
    if v.get("platform").and_then(Value::as_str) != Some(platform(client)) {
        return Err(invalid("不属于当前客户端"));
    }
    if v.get("proxy_key")
        .is_some_and(|v| v.as_str().is_some_and(|s| !s.is_empty()))
    {
        return Err(invalid("此账号绑定代理，不支持自动转换"));
    }
    let name = model::name(v.get("name").and_then(Value::as_str).unwrap_or(""))?;
    let credentials = v
        .get("credentials")
        .ok_or_else(|| invalid("缺少 API 凭据"))?;
    let url = model::base_url(
        credentials
            .get("base_url")
            .and_then(Value::as_str)
            .unwrap_or(""),
        port,
    )?;
    let token = credentials
        .get("api_key")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty() && s.len() <= 16384 && !s.chars().any(char::is_control))
        .ok_or_else(|| invalid("API Key 缺失或格式无效"))?;
    let concurrency = v
        .get("concurrency")
        .and_then(Value::as_u64)
        .filter(|n| *n <= 100000)
        .ok_or_else(|| invalid("并发必须为 0–100000 的整数"))?;
    Ok(model::Provider {
        id: uuid::Uuid::new_v4().to_string(),
        name,
        base_url: url.as_str().trim_end_matches('/').into(),
        token: token.into(),
        queued: false,
        version: uuid::Uuid::new_v4().to_string(),
        max_concurrency: concurrency as u32,
        max_rpm: 0,
        allowed_models: None,
        handoff_after_compaction: false,
        take_new_threads: false,
    })
}
impl Gateway {
    fn template_path(&self) -> std::path::PathBuf {
        self.0.data.join("sub2api-template.json")
    }
    pub fn exchange_has_template(&self) -> bool {
        template::Service::load(&self.template_path(), platform(self.0.client), "apikey").is_ok()
    }
    pub fn exchange_template(&self, path: &Path) -> Result<Prepared> {
        self.0.exchange.templates.prepare(
            read(path)?,
            &self.template_path(),
            platform(self.0.client),
            "apikey",
        )
    }
    pub fn exchange_select_template(&self, token: &str, id: usize) -> Result<()> {
        self.0.exchange.templates.select(
            token,
            id,
            &self.template_path(),
            platform(self.0.client),
            "apikey",
        )
    }
    pub fn exchange_preview(&self, path: &Path) -> Result<Preview> {
        let value = read(path)?;
        let prepared = if !self.exchange_has_template() {
            self.0
                .exchange
                .templates
                .prepare(
                    value.clone(),
                    &self.template_path(),
                    platform(self.0.client),
                    "apikey",
                )
                .ok()
        } else {
            None
        };
        let value = template::payload(&value);
        let state = self.0.inner.lock().unwrap();
        let mut seen: HashSet<_> = state
            .store
            .providers
            .iter()
            .map(|p| {
                (
                    model::base_url(&p.base_url, state.store.settings.port)
                        .map(|u| u.to_string())
                        .unwrap_or_else(|_| p.base_url.clone()),
                    p.token.clone(),
                )
            })
            .collect();
        let mut providers = Vec::new();
        let mut rows = Vec::new();
        for (id, raw) in value["accounts"].as_array().unwrap().iter().enumerate() {
            let parsed = provider(raw, self.0.client, state.store.settings.port).and_then(|p| {
                if !seen.insert((p.base_url.clone(), p.token.clone())) {
                    Err(invalid("相同地址和 Key 已存在"))
                } else {
                    Ok(p)
                }
            });
            let row = match &parsed {
                Ok(p) => Row {
                    id,
                    name: p.name.clone(),
                    base_url: Some(p.base_url.clone()),
                    concurrency: Some(p.max_concurrency),
                    error: None,
                },
                Err(e) => Row {
                    id,
                    name: raw
                        .get("name")
                        .and_then(Value::as_str)
                        .and_then(|s| model::name(s).ok())
                        .unwrap_or_else(|| format!("第 {} 项", id + 1)),
                    base_url: None,
                    concurrency: None,
                    error: Some(e.message.clone()),
                },
            };
            rows.push(row);
            providers.push(parsed.ok());
        }
        let token = uuid::Uuid::new_v4().to_string();
        let revision = self.view_revision(&state);
        *self.0.exchange.pending.lock().unwrap() = Some(Pending {
            token: token.clone(),
            at: Instant::now(),
            providers,
        });
        Ok(Preview {
            token,
            revision,
            rows,
            template: prepared,
            unsupported_proxies: value
                .get("proxies")
                .and_then(Value::as_array)
                .map_or(0, Vec::len),
        })
    }
    pub fn exchange_cancel(&self, token: &str) {
        self.0.exchange.templates.cancel(token);
        let mut pending = self.0.exchange.pending.lock().unwrap();
        if pending.as_ref().is_some_and(|p| p.token == token) {
            pending.take();
        }
    }
    pub fn exchange_commit(
        &self,
        token: &str,
        ids: &[usize],
        expected: &str,
        home: &Path,
    ) -> Result<View> {
        let providers = {
            let pending = self.0.exchange.pending.lock().unwrap();
            let p = pending
                .as_ref()
                .filter(|p| p.token == token && p.at.elapsed() < Duration::from_secs(900))
                .ok_or_else(|| invalid("导入预览已过期，请重新选择文件"))?;
            let mut seen = HashSet::new();
            if ids.is_empty() {
                return Err(invalid("请至少选择一项"));
            }
            ids.iter()
                .map(|id| {
                    if !seen.insert(id) {
                        return Err(invalid("选择项重复"));
                    }
                    p.providers
                        .get(*id)
                        .and_then(Clone::clone)
                        .ok_or_else(|| invalid("选择项无法导入"))
                })
                .collect::<Result<Vec<_>>>()?
        };
        let view = self.edit_checked(Edit::ImportProviders { providers }, expected, home, None)?;
        self.exchange_cancel(token);
        Ok(view)
    }
    pub fn exchange_export(&self, ids: &[String], expected: &str, path: &Path) -> Result<()> {
        let mut document =
            template::Service::load(&self.template_path(), platform(self.0.client), "apikey")?;
        let prototype = template::payload(&document)["accounts"][0].clone();
        let state = self.0.inner.lock().unwrap();
        if self.view_revision(&state) != expected {
            return Err(AppError::new(
                code::CONFLICT,
                "网关设置已变化，请使用最新状态重试",
            ));
        }
        let mut seen = HashSet::new();
        if ids.is_empty() {
            return Err(invalid("请至少选择一项"));
        }
        let accounts = ids
            .iter()
            .map(|id| {
                if !seen.insert(id) {
                    return Err(invalid("选择项重复"));
                }
                let p = state
                    .store
                    .providers
                    .iter()
                    .find(|p| &p.id == id)
                    .ok_or_else(|| invalid("供应商已删除"))?;
                if self.0.client == ClientId::Grok
                    && state
                        .store
                        .grok_connections
                        .get(id)
                        .is_some_and(|c| c.protocol == super::grok_config::Protocol::Messages)
                {
                    return Err(invalid("Grok Messages 供应商不能导出到 OpenAI 账号模板"));
                }
                let mut account = prototype.clone();
                account["name"] = Value::String(p.name.clone());
                account["credentials"]["base_url"] = Value::String(p.base_url.clone());
                account["credentials"]["api_key"] = Value::String(p.token.clone());
                Ok(account)
            })
            .collect::<Result<Vec<_>>>()?;
        template::payload_mut(&mut document)["accounts"] = Value::Array(accounts);
        let content = serde_json::to_vec_pretty(&document).map_err(|_| invalid("导出无法生成"))?;
        drop(state);
        storage::atomic_write_bounded(path, &content, None, crate::core::exchange::VAULT_LIMIT)
    }
}
