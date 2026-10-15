//! Private, lossless Sub2API templates. Only the selected prototype is retained.
use crate::errors::code;
use crate::storage::{self, AppError, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    path::Path,
    sync::Mutex,
    time::{Duration, Instant},
};

pub const MAX_FILE: u64 = 8 * 1024 * 1024;

pub fn invalid(message: &str) -> AppError {
    AppError::new(code::EXCHANGE_TEMPLATE, message)
}

pub fn read(path: &Path) -> Result<Value> {
    let bytes = storage::read_bounded(path, MAX_FILE)?.ok_or_else(|| invalid("文件不存在"))?;
    let text = std::str::from_utf8(&bytes).map_err(|_| invalid("文件不是有效的 UTF-8 JSON"))?;
    crate::configuration::parse_object(text)
}

pub fn payload(value: &Value) -> &Value {
    if value.get("accounts").is_none() && value.get("data").is_some_and(Value::is_object) {
        &value["data"]
    } else {
        value
    }
}

pub fn payload_mut(value: &mut Value) -> &mut Value {
    if value.get("accounts").is_none() && value.get("data").is_some_and(Value::is_object) {
        &mut value["data"]
    } else {
        value
    }
}

pub fn validate(value: &Value) -> Result<()> {
    let value = payload(value);
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
        return Err(invalid("请选择含 accounts 的 Sub2API 文件（最多 1000 项）"));
    }
    Ok(())
}

#[derive(Clone, Debug, Serialize)]
pub struct Row {
    pub id: usize,
    pub name: String,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Prepared {
    pub ready: bool,
    pub token: Option<String>,
    pub rows: Vec<Row>,
}

impl Prepared {
    pub fn ready() -> Self {
        Self {
            ready: true,
            ..Self::default()
        }
    }
}

#[derive(Serialize, Deserialize)]
struct Saved {
    version: u32,
    platform: String,
    kind: String,
    document: Value,
}

struct Pending {
    token: String,
    document: Value,
    rows: Vec<Row>,
    platform: String,
    kind: String,
    at: Instant,
}

#[derive(Default)]
pub struct Service {
    pending: Mutex<Option<Pending>>,
}

/// OAuth defaults may contain behavior, never another account's identity/cache.
fn oauth_prototype(account: &Value) -> Value {
    let mut result = serde_json::json!({"name":"", "platform":"openai", "type":"oauth", "credentials":{}, "extra":{}});
    for key in [
        "concurrency",
        "priority",
        "rate_multiplier",
        "auto_pause_on_expired",
        "proxy_key",
    ] {
        if let Some(value) = account.get(key) {
            result[key] = value.clone();
        }
    }
    for key in ["model_mapping", "intercept_warmup_requests"] {
        if let Some(value) = account["credentials"].get(key) {
            result["credentials"][key] = value.clone();
        }
    }
    for key in [
        "auto_reset_credit_enabled",
        "auto_reset_credit_5h_threshold",
        "auto_reset_credit_7d_threshold",
        "codex_fingerprint_mode",
        "openai_compact_mode",
        "openai_long_context_billing_enabled",
        "openai_oauth_responses_websockets_v2_enabled",
        "openai_oauth_responses_websockets_v2_mode",
        "privacy_mode",
    ] {
        if let Some(value) = account["extra"].get(key) {
            result["extra"][key] = value.clone();
        }
    }
    result
}

pub fn oauth_envelope(document: &Value) -> Value {
    let mut clean = serde_json::json!({"proxies": []});
    for key in ["type", "version", "exported_at", "proxies"] {
        if let Some(value) = payload(document).get(key) {
            clean[key] = value.clone();
        }
    }
    clean
}

impl Service {
    pub fn prune(&self) {
        let mut pending = self.pending.lock().unwrap();
        if pending
            .as_ref()
            .is_some_and(|p| p.at.elapsed() >= Duration::from_secs(900))
        {
            pending.take();
        }
    }

    pub fn cancel(&self, token: &str) {
        let mut pending = self.pending.lock().unwrap();
        if pending.as_ref().is_some_and(|p| p.token == token) {
            pending.take();
        }
    }

    pub fn load(path: &Path, platform: &str, kind: &str) -> Result<Value> {
        let bytes = storage::read_bounded(path, MAX_FILE)?
            .ok_or_else(|| invalid("请先选择完整的 Sub2API 模板"))?;
        let saved: Saved = serde_json::from_slice(&bytes)
            .map_err(|_| invalid("旧模板未保留完整字段，请重新选择原始模板"))?;
        if saved.version != 2 || saved.platform != platform || saved.kind != kind {
            return Err(invalid("请重新选择当前账号类型的完整模板"));
        }
        validate(&saved.document)?;
        let accounts = payload(&saved.document)["accounts"].as_array().unwrap();
        if accounts.len() != 1
            || accounts[0]["platform"] != platform
            || accounts[0]["type"] != kind
            || !accounts[0]["credentials"].is_object()
        {
            return Err(invalid("模板原型无效，请重新选择"));
        }
        Ok(saved.document)
    }

    pub fn prepare(
        &self,
        document: Value,
        path: &Path,
        platform: &str,
        kind: &str,
    ) -> Result<Prepared> {
        validate(&document)?;
        let rows: Vec<_> = payload(&document)["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .enumerate()
            .filter(|(_, a)| {
                a["platform"] == platform && a["type"] == kind && a["credentials"].is_object()
            })
            .map(|(id, a)| Row {
                id,
                name: a["name"]
                    .as_str()
                    .filter(|s| s.len() <= 240 && !s.chars().any(char::is_control))
                    .unwrap_or("未命名账号")
                    .to_owned(),
            })
            .collect();
        if rows.is_empty() {
            return Err(invalid("模板中没有当前平台和账号类型的原型"));
        }
        if rows.len() == 1 {
            Self::save(document, rows[0].id, path, platform, kind)?;
            self.pending.lock().unwrap().take();
            return Ok(Prepared::ready());
        }
        let token = uuid::Uuid::new_v4().to_string();
        *self.pending.lock().unwrap() = Some(Pending {
            token: token.clone(),
            document,
            rows: rows.clone(),
            platform: platform.into(),
            kind: kind.into(),
            at: Instant::now(),
        });
        Ok(Prepared {
            ready: false,
            token: Some(token),
            rows,
        })
    }

    pub fn select(
        &self,
        token: &str,
        id: usize,
        path: &Path,
        platform: &str,
        kind: &str,
    ) -> Result<()> {
        let mut guard = self.pending.lock().unwrap();
        let pending = guard
            .as_ref()
            .filter(|p| {
                p.token == token
                    && p.platform == platform
                    && p.kind == kind
                    && p.at.elapsed() < Duration::from_secs(900)
            })
            .ok_or_else(|| invalid("模板选择已过期，请重新选择文件"))?;
        if !pending.rows.iter().any(|r| r.id == id) {
            return Err(invalid("请选择有效的模板账号"));
        }
        Self::save(pending.document.clone(), id, path, platform, kind)?;
        guard.take();
        Ok(())
    }

    fn save(mut document: Value, id: usize, path: &Path, platform: &str, kind: &str) -> Result<()> {
        let mut account = payload(&document)["accounts"][id].clone();
        if kind == "oauth" {
            account = oauth_prototype(&account);
            // Unlike the strict API Key template, reusable OAuth defaults must
            // not carry arbitrary metadata from the original account/envelope.
            document = oauth_envelope(&document);
        } else {
            account["name"] = Value::String(String::new());
            account["credentials"]["base_url"] = Value::String(String::new());
            account["credentials"]["api_key"] = Value::String(String::new());
        }
        payload_mut(&mut document)["accounts"] = Value::Array(vec![account]);
        let saved = Saved {
            version: 2,
            platform: platform.into(),
            kind: kind.into(),
            document,
        };
        let bytes = serde_json::to_vec(&saved).map_err(|_| invalid("模板无法保存"))?;
        storage::atomic_write_bounded(path, &bytes, None, MAX_FILE)
    }
}
