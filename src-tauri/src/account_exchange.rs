//! Account exchange is deliberately separate from native authentication files.
use super::{auth_info, claims, clean_name, now, older, Core, Profile};
use crate::errors::code;
use crate::{
    exchange_template as template,
    storage::{self, AppError, Result},
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const AUTH_CLAIM: &str = "https://api.openai.com/auth";
pub const VAULT_LIMIT: u64 = 64 * 1024 * 1024;

#[derive(Clone, Serialize, Deserialize)]
pub struct Source {
    identity: String,
    account: Value,
    envelope: Option<Envelope>,
}
#[derive(Clone)]
struct Envelope(Arc<Value>);
impl std::ops::Deref for Envelope {
    type Target = Value;
    fn deref(&self) -> &Value {
        &self.0
    }
}
impl Serialize for Envelope {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        self.0.as_ref().serialize(serializer)
    }
}
impl<'de> Deserialize<'de> for Envelope {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        Value::deserialize(deserializer).map(|v| Self(Arc::new(v)))
    }
}
struct VaultWriter(Vec<u8>);
impl std::io::Write for VaultWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.0.len().saturating_add(bytes.len()) as u64 > VAULT_LIMIT {
            return Err(std::io::Error::other("vault size limit"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
struct Candidate {
    auth: String,
    name: String,
    identity: String,
    source: Option<(String, Source)>,
    duplicate: Option<(String, String)>,
}
struct Pending {
    token: String,
    at: Instant,
    revision: String,
    file_revision: String,
    candidates: Vec<Option<Candidate>>,
}
#[derive(Default)]
pub struct Service {
    pending: Option<Pending>,
    pub templates: template::Service,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Row {
    pub id: usize,
    pub name: String,
    pub format: String,
    pub identity_hint: Option<String>,
    pub duplicate: bool,
    pub error: Option<String>,
}
#[derive(Serialize)]
pub struct Preview {
    pub token: String,
    pub revision: String,
    pub rows: Vec<Row>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Selection {
    pub id: usize,
    pub update: bool,
}
#[derive(Serialize)]
pub struct ExportRow {
    pub id: String,
    pub name: String,
    pub error: Option<String>,
}
#[derive(Serialize)]
pub struct ExportPreview {
    pub revision: String,
    pub rows: Vec<ExportRow>,
}

fn invalid(message: &str) -> AppError {
    AppError::new(code::ACCOUNT_EXCHANGE, message)
}
fn conflict() -> AppError {
    AppError::new(code::CONFLICT, "账号库或凭据已变化，请重新预览后选择")
}
fn text(value: &Value) -> Option<&str> {
    value.as_str().filter(|s| !s.is_empty())
}
fn expiry(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .filter(|n| *n > 0 && *n < 100_000_000_000)
        .or_else(|| {
            let s = value.as_str()?;
            s.parse::<i64>()
                .ok()
                .filter(|n| *n > 0 && *n < 100_000_000_000)
                .or_else(|| {
                    chrono::DateTime::parse_from_rfc3339(s)
                        .ok()
                        .map(|d| d.timestamp())
                })
        })
}
fn datetime(seconds: i64) -> Result<String> {
    chrono::DateTime::from_timestamp(seconds, 0)
        .map(|v| v.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .ok_or_else(|| invalid("有效期无法转换"))
}
fn refresh_time(value: &Value) -> Result<Value> {
    if let Some(value) = value
        .as_str()
        .filter(|s| chrono::DateTime::parse_from_rfc3339(s).is_ok())
    {
        return Ok(json!(value));
    }
    if let Some(value) = value.as_i64().filter(|n| *n > 0) {
        let date = if value >= 1_000_000_000_000 {
            chrono::DateTime::from_timestamp_millis(value)
        } else {
            chrono::DateTime::from_timestamp(value, 0)
        };
        if let Some(date) = date {
            return Ok(json!(
                date.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true)
            ));
        }
    }
    Err(invalid("last_refresh 不是有效的秒、毫秒或 RFC3339 时间"))
}
fn token_claims(tokens: &Value) -> Result<(Value, Value)> {
    for key in ["id_token", "access_token", "refresh_token"] {
        if !text(&tokens[key])
            .is_some_and(|s| s.len() <= 512 * 1024 && !s.chars().any(char::is_control))
        {
            return Err(invalid(
                "缺少完整的 id_token、access_token 或 refresh_token；不会生成替代 Token",
            ));
        }
    }
    let id = claims(tokens["id_token"].as_str().unwrap())
        .ok_or_else(|| invalid("id_token 无法读取，需原始完整 Token"))?;
    let access = claims(tokens["access_token"].as_str().unwrap())
        .ok_or_else(|| invalid("access_token 无法读取"))?;
    let subject = text(&id["sub"]).ok_or_else(|| invalid("id_token 缺少账号身份"))?;
    if text(&access["sub"]).is_some_and(|s| s != subject) {
        return Err(invalid("两个 Token 的账号身份不一致"));
    }
    if text(&access["client_id"]).is_some_and(|s| s != CLIENT_ID) {
        return Err(invalid("此 OAuth client_id 与 Codex 刷新流程不兼容"));
    }
    Ok((id, access))
}
fn workspace(tokens: &Value, id: &Value, access: &Value) -> Result<String> {
    let account = text(&tokens["account_id"])
        .or_else(|| text(&id[AUTH_CLAIM]["chatgpt_account_id"]))
        .or_else(|| text(&access[AUTH_CLAIM]["chatgpt_account_id"]))
        .ok_or_else(|| invalid("缺少 ChatGPT account_id"))?;
    for claim in [id, access] {
        if text(&claim[AUTH_CLAIM]["chatgpt_account_id"]).is_some_and(|v| v != account) {
            return Err(invalid("账号 ID 与 Token 中的工作区不一致"));
        }
    }
    Ok(account.into())
}
fn parse_account(value: &Value, format: &str, envelope: Option<Envelope>) -> Result<Candidate> {
    let mut auth;
    let name;
    if format == "codex" {
        auth = value.clone();
        name = "ChatGPT 账号".to_owned();
    } else {
        let credentials = if format == "sub2api" {
            if value["platform"] != "openai" || value["type"] != "oauth" {
                return Err(invalid("此项不是 OpenAI OAuth 账号"));
            }
            &value["credentials"]
        } else {
            if value["type"] != "codex"
                || value["use_siwc"].as_bool() == Some(true)
                || value.get("siwc").is_some_and(|v| !v.is_null())
            {
                return Err(invalid("此项不是兼容的 Codex OAuth 账号"));
            }
            value
        };
        if text(&credentials["client_id"]).is_some_and(|v| v != CLIENT_ID) {
            return Err(invalid("此 OAuth client_id 不兼容"));
        }
        auth = json!({"auth_mode":"chatgpt", "tokens":{
            "id_token":credentials["id_token"], "access_token":credentials["access_token"],
            "refresh_token":credentials["refresh_token"],
            "account_id": if format == "sub2api" { &credentials["chatgpt_account_id"] } else { &credentials["account_id"] }
        }});
        if let Some(refresh) = value.get("last_refresh").filter(|v| !v.is_null()) {
            if refresh
                .as_str()
                .is_none_or(|s| chrono::DateTime::parse_from_rfc3339(s).is_err())
            {
                return Err(invalid("last_refresh 不是有效时间"));
            }
            auth["last_refresh"] = refresh.clone();
        }
        name = text(&value["name"])
            .map(clean_name)
            .transpose()?
            .unwrap_or_else(|| "ChatGPT 账号".into());
    }
    if auth_info(&auth.to_string())?.kind != "chatgpt" {
        return Err(invalid("此入口仅交换 ChatGPT/Codex 官方 OAuth 账号"));
    }
    let (id, access) = token_claims(&auth["tokens"])?;
    auth["tokens"]["account_id"] = Value::String(workspace(&auth["tokens"], &id, &access)?);
    let raw = auth.to_string();
    let info = auth_info(&raw)?;
    let source = (format != "codex").then(|| {
        (
            format.into(),
            Source {
                identity: info.identity.clone(),
                account: value.clone(),
                envelope,
            },
        )
    });
    Ok(Candidate {
        auth: raw,
        name,
        identity: info.identity,
        source,
        duplicate: None,
    })
}

impl Core {
    pub fn exchange_revision(&self) -> String {
        // Includes names, metadata and preferences as well as credential versions.
        storage::digest(&serde_json::to_vec(&self.store).unwrap_or_default())
    }
    pub fn exchange_cancel(&mut self, token: &str) {
        if self
            .exchange
            .pending
            .as_ref()
            .is_some_and(|p| p.token == token)
        {
            self.exchange.pending.take();
        }
        self.exchange.templates.cancel(token);
    }
    pub fn exchange_prune(&mut self) {
        if self
            .exchange
            .pending
            .as_ref()
            .is_some_and(|p| p.at.elapsed() >= Duration::from_secs(900))
        {
            self.exchange.pending.take();
        }
        self.exchange.templates.prune();
    }
    pub fn exchange_preview(&mut self, paths: &[PathBuf]) -> Result<Preview> {
        self.exchange.pending.take();
        if paths.is_empty() || paths.len() > 1000 {
            return Err(invalid("请选择 1–1000 个 JSON 文件"));
        }
        let mut rows = Vec::new();
        let mut candidates = Vec::new();
        let mut seen = HashSet::new();
        let mut total = 0;
        for path in paths {
            let doc = template::read(path)?;
            total += doc.to_string().len();
            if total > template::MAX_FILE as usize {
                return Err(invalid("本次选择的文件总计超过 8 MiB"));
            }
            let format = if template::payload(&doc).get("accounts").is_some() {
                "sub2api"
            } else if doc.get("type").is_some() {
                "cliproxyapi"
            } else {
                "codex"
            };
            let (items, envelope) = if format == "sub2api" {
                template::validate(&doc)?;
                let items = template::payload(&doc)["accounts"]
                    .as_array()
                    .unwrap()
                    .clone();
                let mut env = doc.clone();
                template::payload_mut(&mut env)["accounts"] = json!([]);
                (items, Some(Envelope(Arc::new(env))))
            } else {
                (vec![doc], None)
            };
            for item in items {
                if candidates.len() >= 1000 {
                    return Err(invalid("单次最多导入 1000 个账号"));
                }
                let mut parsed = parse_account(&item, format, envelope.clone());
                if let Ok(candidate) = parsed.as_mut() {
                    if !seen.insert(candidate.identity.clone()) {
                        parsed = Err(invalid("本次选择中有重复账号，请保留一项"));
                    } else if let Some(existing) = self
                        .store
                        .profiles
                        .iter()
                        .find(|p| p.identity == candidate.identity)
                    {
                        if older(&candidate.auth, &existing.auth) {
                            parsed = Err(invalid("导入凭据早于账号库版本，已阻止覆盖"));
                        } else {
                            candidate.duplicate = Some((
                                existing.id.clone(),
                                storage::digest(existing.auth.as_bytes()),
                            ));
                        }
                    }
                }
                let (name, hint, duplicate, error) = match &parsed {
                    Ok(c) => (
                        c.name.clone(),
                        Some(c.identity[..12].to_owned()),
                        c.duplicate.is_some(),
                        None,
                    ),
                    Err(e) => (
                        format!("第 {} 项", rows.len() + 1),
                        None,
                        false,
                        Some(e.message.clone()),
                    ),
                };
                rows.push(Row {
                    id: rows.len(),
                    name,
                    format: format.into(),
                    identity_hint: hint,
                    duplicate,
                    error,
                });
                candidates.push(parsed.ok());
            }
        }
        let token = uuid::Uuid::new_v4().to_string();
        let revision = self.exchange_revision();
        self.exchange.pending = Some(Pending {
            token: token.clone(),
            revision: revision.clone(),
            file_revision: storage::revision(
                storage::read_bounded(&self.data_dir.join("accounts.json"), VAULT_LIMIT)?
                    .as_deref(),
            ),
            at: Instant::now(),
            candidates,
        });
        Ok(Preview {
            token,
            revision,
            rows,
        })
    }
    pub fn exchange_commit(
        &mut self,
        token: &str,
        selected: &[Selection],
        expected: &str,
    ) -> Result<usize> {
        let pending = self
            .exchange
            .pending
            .as_ref()
            .filter(|p| p.token == token && p.at.elapsed() < Duration::from_secs(900))
            .ok_or_else(|| invalid("导入预览已过期，请重新选择文件"))?;
        if expected != pending.revision || expected != self.exchange_revision() {
            return Err(conflict());
        }
        if selected.is_empty() {
            return Err(invalid("请至少选择一个账号"));
        }
        let mut seen = HashSet::new();
        let mut next = self.store.clone();
        for selection in selected {
            if !seen.insert(selection.id) {
                return Err(invalid("选择项重复"));
            }
            let c = pending
                .candidates
                .get(selection.id)
                .and_then(Option::as_ref)
                .ok_or_else(|| invalid("选择项无法导入"))?;
            let info = auth_info(&c.auth)?;
            if info.identity != c.identity {
                return Err(conflict());
            }
            let profile = if let Some((id, revision)) = &c.duplicate {
                if !selection.update {
                    return Err(invalid("重复账号默认跳过；需明确选择更新"));
                }
                let existing = next
                    .profiles
                    .iter_mut()
                    .find(|p| p.id == *id && p.identity == c.identity)
                    .ok_or_else(conflict)?;
                if storage::digest(existing.auth.as_bytes()) != *revision
                    || older(&c.auth, &existing.auth)
                {
                    return Err(conflict());
                }
                existing.auth = c.auth.clone();
                existing.email = info.email;
                existing.updated_at = now();
                existing
            } else {
                if next.profiles.iter().any(|p| p.identity == c.identity) {
                    return Err(conflict());
                }
                next.profiles.push(Profile {
                    id: uuid::Uuid::new_v4().to_string(),
                    name: c.name.clone(),
                    identity: c.identity.clone(),
                    kind: info.kind,
                    email: info.email,
                    auth: c.auth.clone(),
                    updated_at: now(),
                    exchange: BTreeMap::new(),
                });
                next.profiles.last_mut().unwrap()
            };
            if let Some((format, source)) = &c.source {
                profile.exchange.insert(format.clone(), source.clone());
            }
        }
        if let Some(bytes) = storage::read_optional(&self.home().join("auth.json"))? {
            if std::str::from_utf8(&bytes)
                .ok()
                .and_then(|raw| auth_info(raw).ok())
                .is_some_and(|info| {
                    selected.iter().any(|s| {
                        pending.candidates[s.id]
                            .as_ref()
                            .is_some_and(|c| c.identity == info.identity)
                    })
                })
            {
                next.observed_auth_revisions
                    .insert(next.preferences.codex_home.clone(), storage::digest(&bytes));
            }
        }
        // Import is one vault transaction. In particular it does not activate or refresh auth.json.
        let mut bytes = VaultWriter(Vec::new());
        serde_json::to_writer_pretty(&mut bytes, &next)
            .map_err(|_| invalid("账号库超过 64 MiB，无法保存本次导入"))?;
        storage::atomic_write_bounded(
            &self.data_dir.join("accounts.json"),
            &bytes.0,
            Some(&pending.file_revision),
            VAULT_LIMIT,
        )?;
        self.store = next;
        self.exchange.pending.take();
        Ok(selected.len())
    }
    fn oauth_template_path(&self) -> PathBuf {
        self.data_dir.join("sub2api-oauth-template.json")
    }
    pub fn exchange_prepare_template(&self, path: &Path) -> Result<template::Prepared> {
        self.exchange.templates.prepare(
            template::read(path)?,
            &self.oauth_template_path(),
            "openai",
            "oauth",
        )
    }
    pub fn exchange_select_template(&self, token: &str, id: usize) -> Result<()> {
        self.exchange
            .templates
            .select(token, id, &self.oauth_template_path(), "openai", "oauth")
    }
    pub fn exchange_template_ready(&self, ids: &[String]) -> bool {
        template::Service::load(&self.oauth_template_path(), "openai", "oauth").is_ok()
            || (!ids.is_empty()
                && ids.iter().all(|id| {
                    self.store
                        .profiles
                        .iter()
                        .find(|p| p.id == *id)
                        .is_some_and(|p| source(p, "sub2api").is_some())
                }))
    }
    pub fn exchange_export_preview(&self) -> ExportPreview {
        ExportPreview {
            revision: self.exchange_revision(),
            rows: self
                .store
                .profiles
                .iter()
                .map(|p| ExportRow {
                    id: p.id.clone(),
                    name: p.name.clone(),
                    error: export_account(p, "cliproxyapi", None)
                        .err()
                        .map(|e| e.message),
                })
                .collect(),
        }
    }
    pub fn exchange_export(
        &self,
        ids: &[String],
        expected: &str,
        format: &str,
        path: &Path,
    ) -> Result<()> {
        if expected != self.exchange_revision() {
            return Err(conflict());
        }
        if !matches!(format, "sub2api" | "cliproxyapi") || ids.is_empty() {
            return Err(invalid("请选择账号和支持的导出格式"));
        }
        let mut seen = HashSet::new();
        let profiles = ids
            .iter()
            .map(|id| {
                if !seen.insert(id) {
                    return Err(invalid("选择项重复"));
                }
                self.store
                    .profiles
                    .iter()
                    .find(|p| p.id == *id)
                    .ok_or_else(conflict)
            })
            .collect::<Result<Vec<_>>>()?;
        if format == "cliproxyapi" {
            if !path.is_dir() {
                return Err(invalid("请选择导出目录"));
            }
            // Stage the whole batch before publishing. UUID names avoid overwriting user files.
            let staged = profiles
                .iter()
                .map(|p| {
                    let bytes = serde_json::to_vec_pretty(&export_account(p, format, None)?)
                        .map_err(|_| invalid("导出无法生成"))?;
                    let mut file = storage::private_tempfile(path)?;
                    use std::io::Write;
                    file.write_all(&bytes).map_err(storage::io_error)?;
                    file.as_file().sync_all().map_err(storage::io_error)?;
                    Ok((
                        file,
                        path.join(format!("codex-{}.json", uuid::Uuid::new_v4())),
                    ))
                })
                .collect::<Result<Vec<_>>>()?;
            let mut published = Vec::new();
            for (file, destination) in staged {
                if file.persist_noclobber(&destination).is_err() {
                    for owned in published {
                        let _ = std::fs::remove_file(owned);
                    }
                    return Err(invalid("导出保存失败，已撤回本次写入"));
                }
                published.push(destination);
            }
            return Ok(());
        }
        let saved = template::Service::load(&self.oauth_template_path(), "openai", "oauth").ok();
        let first_envelope = profiles
            .iter()
            .find_map(|p| source(p, format).and_then(|s| s.envelope.as_deref()));
        let all_same = first_envelope.is_some_and(|first| {
            profiles
                .iter()
                .all(|p| source(p, format).and_then(|s| s.envelope.as_deref()) == Some(first))
        });
        let mut document = if all_same {
            first_envelope.unwrap().clone()
        } else {
            saved
                .clone()
                .or_else(|| first_envelope.map(template::oauth_envelope))
                .ok_or_else(|| invalid("请先选择 OAuth 模板"))?
        };
        let prototype = saved.as_ref().map(|v| &template::payload(v)["accounts"][0]);
        let mut proxies = template::payload(&document)["proxies"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let accounts = profiles
            .iter()
            .map(|p| {
                let account = export_account(p, format, prototype)?;
                if let Some(env) = source(p, format).and_then(|s| s.envelope.as_deref()) {
                    for proxy in template::payload(env)["proxies"]
                        .as_array()
                        .into_iter()
                        .flatten()
                    {
                        if !proxies.contains(proxy) {
                            if proxies.iter().any(|existing| {
                                existing.get("proxy_key").is_some()
                                    && existing.get("proxy_key") == proxy.get("proxy_key")
                            }) {
                                return Err(invalid("来源代理定义冲突，请分批导出这些账号"));
                            }
                            proxies.push(proxy.clone());
                        }
                    }
                }
                Ok(account)
            })
            .collect::<Result<Vec<_>>>()?;
        template::payload_mut(&mut document)["accounts"] = Value::Array(accounts);
        if !all_same {
            template::payload_mut(&mut document)["proxies"] = Value::Array(proxies);
        }
        let bytes = serde_json::to_vec_pretty(&document).map_err(|_| invalid("导出无法生成"))?;
        storage::atomic_write_bounded(path, &bytes, None, VAULT_LIMIT)
    }
}
fn source<'a>(profile: &'a Profile, format: &str) -> Option<&'a Source> {
    profile
        .exchange
        .get(format)
        .filter(|s| s.identity == profile.identity)
}
fn export_account(profile: &Profile, format: &str, prototype: Option<&Value>) -> Result<Value> {
    if profile.kind != "chatgpt" {
        return Err(invalid("仅支持 ChatGPT/Codex 官方 OAuth 账号"));
    }
    let auth: Value =
        serde_json::from_str(&profile.auth).map_err(|_| invalid("账号凭据无法读取"))?;
    let tokens = &auth["tokens"];
    let (id, access) = token_claims(tokens)?;
    let account_id = workspace(tokens, &id, &access)?;
    let original = source(profile, format);
    let other = source(
        profile,
        if format == "sub2api" {
            "cliproxyapi"
        } else {
            "sub2api"
        },
    );
    let expires = expiry(&access["exp"])
        .or_else(|| {
            original
                .or(other)
                .filter(|s| {
                    s.account["access_token"] == tokens["access_token"]
                        || s.account["credentials"]["access_token"] == tokens["access_token"]
                })
                .and_then(|s| {
                    expiry(&s.account["credentials"]["expires_at"])
                        .or_else(|| expiry(&s.account["expired"]))
                })
        })
        .ok_or_else(|| invalid("缺少 access_token 有效期，无法可靠导出"))?;
    let email =
        text(&id["email"]).or_else(|| text(&access["https://api.openai.com/profile"]["email"]));
    let claim = |key: &str| {
        id[AUTH_CLAIM]
            .get(key)
            .or_else(|| access[AUTH_CLAIM].get(key))
            .cloned()
    };
    let organization = claim("organization_id").or_else(|| {
        let organizations = id[AUTH_CLAIM]["organizations"].as_array()?;
        let organization = if organizations.len() == 1 {
            organizations.first()
        } else {
            organizations.iter().find(|v| v["is_default"] == true)
        }?;
        organization.get("id").filter(|v| v.is_string()).cloned()
    });
    let mut out = original
        .map(|s| s.account.clone())
        .or_else(|| prototype.cloned())
        .unwrap_or_else(|| json!({}));
    if format == "cliproxyapi" {
        out["type"] = json!("codex");
        for key in ["id_token", "access_token", "refresh_token"] {
            out[key] = tokens[key].clone();
        }
        out["account_id"] = json!(account_id);
        out["expired"] = json!(datetime(expires)?);
        if let Some(value) = auth.get("last_refresh") {
            out["last_refresh"] = refresh_time(value)?;
        }
        if let Some(email) = email {
            out["email"] = json!(email);
        }
        if let Some(plan) = claim("chatgpt_plan_type") {
            out["plan_type"] = plan;
        }
    } else {
        if original.is_none() && prototype.is_none() {
            return Err(invalid("新账号需要独立的 OAuth 模板"));
        }
        out["name"] = json!(profile.name);
        out["platform"] = json!("openai");
        out["type"] = json!("oauth");
        if !out["credentials"].is_object() {
            out["credentials"] = json!({});
        }
        let c = &mut out["credentials"];
        for key in ["id_token", "access_token", "refresh_token"] {
            c[key] = tokens[key].clone();
        }
        c["chatgpt_account_id"] = json!(account_id);
        c["client_id"] = json!(CLIENT_ID);
        c["expires_at"] = json!(expires);
        if let Some(email) = email {
            c["email"] = json!(email);
        }
        for (from, to) in [
            ("chatgpt_user_id", "chatgpt_user_id"),
            ("organization_id", "organization_id"),
            ("chatgpt_plan_type", "plan_type"),
        ] {
            if let Some(value) = claim(from) {
                c[to] = value;
            }
        }
        if let Some(organization) = organization {
            c["organization_id"] = organization;
        }
    }
    Ok(out)
}
