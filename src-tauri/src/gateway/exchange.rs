//! Sub2API account exchange. Secrets stay in Rust and user-selected files.
use super::{model, ClientId, Edit, Gateway, View};
use crate::storage::{self, AppError, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::HashSet,
    path::Path,
    sync::Mutex,
    time::{Duration, Instant},
};

const MAX_FILE: u64 = 8 * 1024 * 1024;
#[derive(Default)]
pub struct Service {
    pending: Mutex<Option<Pending>>,
}
impl Service {
    pub(super) fn prune(&self) {
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
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Template {
    version: u32,
    priority: u32,
    rate_multiplier: f64,
    auto_pause_on_expired: bool,
}
impl Default for Template {
    fn default() -> Self {
        Self {
            version: 1,
            priority: 50,
            rate_multiplier: 1.0,
            auto_pause_on_expired: true,
        }
    }
}
fn invalid(message: &str) -> AppError {
    AppError::new("SUB2API_FILE", message)
}
fn read(path: &Path) -> Result<Value> {
    if std::fs::metadata(path)
        .map_err(|_| invalid("文件无法读取"))?
        .len()
        > MAX_FILE
    {
        return Err(invalid("文件超过 8 MiB"));
    }
    let raw = std::fs::read_to_string(path).map_err(|_| invalid("文件不是有效的 UTF-8 JSON"))?;
    parse(&raw)
}
fn parse(raw: &str) -> Result<Value> {
    if raw.len() > MAX_FILE as usize {
        return Err(invalid("文件超过 8 MiB"));
    }
    let mut value = crate::configuration::parse_object(raw)?;
    if value.get("accounts").is_none() && value.get("data").is_some_and(Value::is_object) {
        value = value["data"].take();
    }
    if value
        .get("type")
        .is_some_and(|v| !matches!(v.as_str(), Some("sub2api-data" | "sub2api-bundle")))
        || value
            .get("version")
            .is_some_and(|v| !matches!(v.as_u64(), Some(0 | 1)))
    {
        return Err(invalid("Sub2API 文件版本不支持"));
    }
    if !value
        .get("accounts")
        .and_then(Value::as_array)
        .is_some_and(|a| !a.is_empty() && a.len() <= 1000)
        || value.get("proxies").is_some_and(|v| !v.is_array())
    {
        return Err(invalid(
            "请选择含 accounts 的 Sub2API 导出文件（最多 1000 项）",
        ));
    }
    Ok(value)
}
fn platform(client: ClientId) -> &'static str {
    if client == ClientId::Codex {
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
        self.template_path().is_file()
    }
    pub fn exchange_template(&self, path: &Path) -> Result<()> {
        let value = read(path)?;
        self.save_exchange_template(&value)
    }
    fn save_exchange_template(&self, value: &Value) -> Result<()> {
        let account = value["accounts"]
            .as_array()
            .and_then(|a| {
                a.iter()
                    .find(|v| provider(v, self.0.client, self.view().settings.port).is_ok())
            })
            .ok_or_else(|| invalid("模板没有当前客户端的 API Key 格式"))?;
        // Validate the schema, but never persist account names, URLs, keys,
        // mappings, proxy data, error rules or provider-specific flags.
        provider(account, self.0.client, self.view().settings.port)?;
        let mut template = Template::default();
        if let Some(v) = account.get("priority") {
            template.priority = v
                .as_u64()
                .filter(|n| *n <= u32::MAX as u64)
                .ok_or_else(|| invalid("模板优先级无效"))? as u32;
        }
        if let Some(v) = account.get("rate_multiplier").filter(|v| !v.is_null()) {
            template.rate_multiplier = v
                .as_f64()
                .filter(|n| n.is_finite() && *n >= 0.0 && *n <= 100000.0)
                .ok_or_else(|| invalid("模板倍率无效"))?;
        }
        if let Some(v) = account.get("auto_pause_on_expired") {
            template.auto_pause_on_expired =
                v.as_bool().ok_or_else(|| invalid("模板到期设置无效"))?;
        }
        storage::atomic_write(
            &self.template_path(),
            &serde_json::to_vec(&template).map_err(|_| invalid("模板无法保存"))?,
            None,
        )
    }
    pub fn exchange_preview(&self, path: &Path) -> Result<Preview> {
        let value = read(path)?;
        if !self.exchange_has_template() {
            self.save_exchange_template(&value)?;
        }
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
            unsupported_proxies: value
                .get("proxies")
                .and_then(Value::as_array)
                .map_or(0, Vec::len),
        })
    }
    pub fn exchange_cancel(&self, token: &str) {
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
        let raw = storage::read_optional(&self.template_path())?
            .ok_or_else(|| invalid("请先选择 Sub2API 模板"))?;
        let template: Template =
            serde_json::from_slice(&raw).map_err(|_| invalid("模板无效，请重新选择"))?;
        if template.version != 1 {
            return Err(invalid("模板版本不支持"));
        }
        let state = self.0.inner.lock().unwrap();
        if self.view_revision(&state) != expected {
            return Err(AppError::new(
                "CONFLICT",
                "网关设置已变化，请使用最新状态重试",
            ));
        }
        let mut seen = HashSet::new();
        if ids.is_empty() {
            return Err(invalid("请至少选择一项"));
        }
        let accounts = ids.iter().map(|id| {
            if !seen.insert(id) { return Err(invalid("选择项重复")); }
            let p = state.store.providers.iter().find(|p| &p.id == id).ok_or_else(|| invalid("供应商已删除"))?;
            Ok(json!({"name":p.name,"platform":platform(self.0.client),"type":"apikey",
                "credentials":{"api_key":p.token,"base_url":model::base_url(&p.base_url,state.store.settings.port)?.as_str().trim_end_matches('/')},
                "extra":{},"concurrency":p.max_concurrency,"priority":template.priority,
                "rate_multiplier":template.rate_multiplier,"auto_pause_on_expired":template.auto_pause_on_expired}))
        }).collect::<Result<Vec<_>>>()?;
        let content = serde_json::to_vec_pretty(&json!({"type":"sub2api-data","version":1,
            "exported_at":chrono::Utc::now().to_rfc3339(),"proxies":[],"accounts":accounts}))
        .map_err(|_| invalid("导出无法生成"))?;
        drop(state);
        storage::atomic_write(path, &content, None)
    }
}
