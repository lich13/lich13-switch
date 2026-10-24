//! Explicit, revision-checked Grok import. Raw configuration and credentials never leave Rust.
use super::{
    grok_config::{self, Profile, Protocol, MANAGED},
    model::Edit,
    ClientId, Gateway, View,
};
use crate::{
    errors::code,
    storage::{self, AppError, Result},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::Mutex,
};
use toml_edit::{value, DocumentMut, Item, Table};

const JOURNAL: &str = "grok-import.json";
const ENV_FILE: &str = "endpoint.env";
const FILE_LIMIT: u64 = 2 * 1024 * 1024;
// Three bounded files, before/after images, and JSON string escaping.
const JOURNAL_LIMIT: u64 = 80 * 1024 * 1024;
#[derive(Default)]
pub struct Service(Mutex<Option<Pending>>);
struct Pending {
    layers: Vec<(PathBuf, String)>,
    overlays: BTreeMap<String, Option<String>>,
    token: String,
    revision: String,
    home: PathBuf,
    config: String,
    environment: Option<String>,
    profile: Profile,
    after_config: String,
    after_environment: Option<String>,
}
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelView {
    pub key: String,
    pub model: String,
    pub protocol: Option<Protocol>,
    pub address: Option<String>,
    pub address_source: Option<String>,
    pub credential_available: bool,
    pub credential_source: Option<String>,
    pub missing_environment: Vec<String>,
    pub error: Option<String>,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Preview {
    pub layers: Vec<String>,
    pub model_source: Option<String>,
    pub unknown: Vec<String>,
    pub token: Option<String>,
    pub revision: String,
    pub models: Vec<ModelView>,
    pub selected: String,
    pub environment_available: bool,
    pub environment_loaded: bool,
    pub clear_environment: Vec<String>,
    pub error: Option<String>,
}
fn error(message: &str) -> AppError {
    AppError::new(code::GROK_CONFIG, message)
}

/// Only literal assignments are accepted. No shell, substitution, escaping, or expansion.
pub(super) fn environment(text: &str) -> Result<BTreeMap<String, String>> {
    let mut values = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line
            .strip_prefix("export ")
            .map(str::trim_start)
            .unwrap_or(line);
        let (name, raw) = line
            .split_once('=')
            .ok_or_else(|| error("环境文件只支持简单赋值，不会执行脚本"))?;
        let name = name.trim();
        if name.is_empty()
            || !name
                .bytes()
                .enumerate()
                .all(|(i, c)| c == b'_' || c.is_ascii_alphabetic() || (i > 0 && c.is_ascii_digit()))
        {
            return Err(error("环境文件变量名称无效"));
        }
        let raw = raw.trim();
        let literal = if let Some(quote) = raw.chars().next().filter(|c| matches!(c, '\'' | '"')) {
            let rest = &raw[1..];
            let end = rest
                .find(quote)
                .ok_or_else(|| error("环境文件引号未闭合"))?;
            let suffix = rest[end + 1..].trim();
            if !suffix.is_empty() && !suffix.starts_with('#') {
                return Err(error("环境文件包含非赋值语句"));
            }
            let text = &rest[..end];
            if quote == '"' && text.contains(['$', '`', '\\']) {
                return Err(error("环境文件不支持变量或命令展开"));
            }
            text
        } else {
            let text = raw
                .split_once(" #")
                .map(|(value, _)| value.trim_end())
                .unwrap_or(raw);
            if text
                .chars()
                .any(|c| c.is_whitespace() || "$`\\;|&()<>'\"".contains(c))
            {
                return Err(error("环境文件不支持命令或变量展开"));
            }
            text
        };
        if values.insert(name.to_owned(), literal.to_owned()).is_some() {
            return Err(error("环境文件包含重复变量，请先整理"));
        }
    }
    Ok(values)
}
pub(super) fn remove_environment(text: &str, names: &BTreeSet<String>) -> String {
    text.split_inclusive('\n')
        .filter(|line| {
            let line = line.trim_start();
            let line = line.strip_prefix("export ").unwrap_or(line).trim_start();
            !line
                .split_once('=')
                .is_some_and(|(name, _)| names.contains(name.trim()))
        })
        .collect()
}
fn document(text: &str) -> Result<DocumentMut> {
    text.parse().map_err(|_| error("Grok 配置不是有效的 TOML"))
}
fn set_selected(doc: &mut DocumentMut, selected: &str) {
    if !doc.contains_key("models") {
        doc["models"] = Item::Table(Table::new());
    }
    let decor = doc["models"]
        .get("default")
        .and_then(Item::as_value)
        .map(|v| v.decor().clone());
    let mut v = toml_edit::Value::from(selected);
    if let Some(decor) = decor {
        *v.decor_mut() = decor;
    }
    doc["models"]["default"] = Item::Value(v);
}
fn remove_auth_headers(item: Option<&mut Item>, migrated: Option<&BTreeMap<String, String>>) {
    if let Some(headers) = item.and_then(Item::as_table_like_mut) {
        let names: Vec<_> = headers
            .iter()
            .filter(|(name, value)| {
                (name.eq_ignore_ascii_case("authorization")
                    || name.eq_ignore_ascii_case("x-api-key"))
                    && migrated.is_none_or(|headers| {
                        headers.get(&name.to_ascii_lowercase()).map(String::as_str)
                            == value.as_str()
                    })
            })
            .map(|(name, _)| name.to_owned())
            .collect();
        for name in names {
            headers.remove(&name);
        }
    }
}
pub(super) fn safe_address(raw: Option<&str>) -> Option<String> {
    let mut url = url::Url::parse(raw?).ok()?;
    url.set_username("").ok()?;
    url.set_password(None).ok()?;
    url.set_query(None);
    url.set_fragment(None);
    Some(url.to_string())
}
fn model_view(key: String, result: &Result<Profile>) -> ModelView {
    match result {
        Ok(p) => ModelView {
            key,
            model: p.model.clone(),
            protocol: p.protocol_known.then_some(p.connection.protocol),
            address: safe_address(p.pair.base_url.as_deref()),
            address_source: p.address_source.map(str::to_owned),
            credential_available: p.pair.token.is_some(),
            credential_source: Some(
                if p.table
                    .get("api_key")
                    .and_then(Item::as_str)
                    .is_some_and(|v| !v.is_empty())
                {
                    "模型 api_key".into()
                } else {
                    format!("环境变量：{}", p.credential_names.join(" / "))
                },
            ),
            missing_environment: if p.pair.token.is_none() {
                p.credential_names.clone()
            } else {
                Vec::new()
            },
            error: None,
        },
        Err(e) => ModelView {
            model: key.clone(),
            key,
            protocol: None,
            address: None,
            address_source: None,
            credential_available: false,
            credential_source: None,
            missing_environment: Vec::new(),
            error: Some(e.message.clone()),
        },
    }
}
pub fn discover(text: &str, resolve: &impl Fn(&str) -> Option<String>) -> Result<Vec<ModelView>> {
    let doc = document(text)?;
    let mut keys: BTreeSet<String> = doc
        .get("model")
        .and_then(Item::as_table_like)
        .map(|t| t.iter().map(|(key, _)| key.to_owned()).collect())
        .unwrap_or_default();
    if let Some(selected) = doc
        .get("models")
        .and_then(|v| v.get("default"))
        .and_then(Item::as_str)
    {
        keys.insert(selected.to_owned());
    }
    keys.into_iter()
        .map(|key| {
            let mut selected = doc.clone();
            set_selected(&mut selected, &key);
            Ok(model_view(
                key,
                &grok_config::profile_with_env(&selected.to_string(), resolve),
            ))
        })
        .collect()
}

fn migrate(text: &str, p: &Profile, known_managed: bool) -> Result<String> {
    if p.selected.is_empty() || p.model.is_empty() {
        return Err(error("未明确选择 Grok 模型，请先在原生客户端选择"));
    }
    if p.pair.base_url.is_none() {
        return Err(error("未解析到当前模型的 API 地址"));
    }
    if p.pair.token.is_none() {
        return Err(error(
            "未读取到 API 凭据；可确认读取 endpoint.env，或在启动应用的环境中提供变量",
        ));
    }
    if !p.protocol_known {
        return Err(error(
            "当前稀疏模型未声明协议，无法确认其原生目录协议；请先明确 api_backend",
        ));
    }
    let mut doc = document(text)?;
    if doc.get("model").and_then(|v| v.get(MANAGED)).is_some() && !known_managed {
        return Err(error("已有同名模型且不属于本网关，已保留配置"));
    }
    let mut table = p.table.clone();
    for field in [
        "model_provider",
        "env_key",
        "env_http_headers",
        "auth_provider",
        "auth_provider_command",
        "api_base_url",
        "mtls_cert_dir",
    ] {
        table.remove(field);
    }
    table["model"] = value(&p.model);
    if !table.contains_key("name") {
        table["name"] = value(format!("API · {}", p.model));
    }
    table["api_backend"] = value(p.connection.protocol.name());
    table["base_url"] = value(p.pair.base_url.as_deref().unwrap());
    table["api_key"] = value(p.pair.token.as_deref().unwrap());
    if !p.connection.headers.is_empty() {
        let mut headers = toml_edit::InlineTable::new();
        for (name, value) in &p.connection.headers {
            headers.insert(name, value.clone().into());
        }
        table["extra_headers"] = value(headers);
    }
    if !doc.contains_key("model") {
        doc["model"] = Item::Table(Table::new());
    }
    // Sparse native model overrides are restored to native authentication. Full
    // custom model entries retain their independent credentials and parameters.
    if p.selected != MANAGED
        && doc["model"]
            .get(&p.selected)
            .is_some_and(|v| v.get("model").is_none())
    {
        if let Some(original) = doc["model"]
            .get_mut(&p.selected)
            .and_then(Item::as_table_like_mut)
        {
            for field in [
                "api_key",
                "env_key",
                "base_url",
                "api_base_url",
                "model_provider",
            ] {
                original.remove(field);
            }
            remove_auth_headers(original.get_mut("extra_headers"), None);
            remove_auth_headers(original.get_mut("env_http_headers"), None);
        }
    }
    // Authentication headers resolved into the independent API model must not
    // remain global and follow a later native official model selection.
    remove_auth_headers(
        doc.get_mut("models")
            .and_then(|v| v.get_mut("extra_headers")),
        Some(&p.connection.headers),
    );
    if let Some(endpoints) = doc.get_mut("endpoints").and_then(Item::as_table_like_mut) {
        for field in ["models_base_url", "xai_api_base_url"] {
            if endpoints.get(field).and_then(Item::as_str) == p.pair.base_url.as_deref() {
                endpoints.remove(field);
            }
        }
    }
    if let Some(auth) = doc.get_mut("auth").and_then(Item::as_table_like_mut) {
        if auth.get("preferred_method").and_then(Item::as_str) == Some("api_key") {
            auth.remove("preferred_method");
        }
    }
    doc["model"][MANAGED] = Item::Table(table);
    set_selected(&mut doc, MANAGED);
    if !doc.contains_key("features") {
        doc["features"] = Item::Table(Table::new());
    }
    doc["features"]["campaigns"] = value(false);
    Ok(doc.to_string())
}

#[derive(Serialize, Deserialize)]
struct Change {
    before: Option<String>,
    after: String,
}
#[derive(Serialize, Deserialize)]
struct Transaction {
    version: u32,
    home: PathBuf,
    config: Change,
    environment: Option<Change>,
    store: Change,
}
impl Transaction {
    fn files(&self, data: &Path) -> Vec<(PathBuf, &Change)> {
        let mut files = vec![(self.home.join("config.toml"), &self.config)];
        if let Some(env) = &self.environment {
            files.push((self.home.join(ENV_FILE), env));
        }
        files.push((data.join("gateway.json"), &self.store));
        files
    }
    fn apply(&self, data: &Path) -> Result<()> {
        if self.version != 1 || !self.home.is_absolute() {
            return Err(error("Grok 导入恢复记录无效"));
        }
        let files = self.files(data);
        // Preflight every participant before any further write; external edits
        // are never rolled back, even following an interrupted commit.
        for (path, change) in &files {
            if change.after.len() as u64 > FILE_LIMIT {
                return Err(AppError::new(code::FILE_SIZE, "Grok 导入文件超过允许大小"));
            }
            let raw = storage::read_optional(path)?;
            if raw.as_deref() != change.before.as_deref().map(str::as_bytes)
                && raw.as_deref() != Some(change.after.as_bytes())
            {
                return Err(AppError::new(
                    code::CONFLICT,
                    "Grok 导入文件被外部修改，已保留恢复事务和当前配置",
                ));
            }
        }
        for (path, change) in files {
            let raw = storage::read_optional(&path)?;
            if raw.as_deref() != Some(change.after.as_bytes()) {
                storage::atomic_write(
                    &path,
                    change.after.as_bytes(),
                    Some(&storage::revision(
                        change.before.as_deref().map(str::as_bytes),
                    )),
                )?;
            }
        }
        std::fs::remove_file(data.join(JOURNAL)).map_err(storage::io_error)?;
        storage::sync_parent(&data.join(JOURNAL))
    }
}
pub fn recover(data: &Path) -> Result<()> {
    if let Some(raw) = storage::read_bounded(&data.join(JOURNAL), JOURNAL_LIMIT)? {
        let transaction: Transaction = serde_json::from_slice(&raw)
            .map_err(|_| error("Grok 导入恢复记录损坏，已保留原文件"))?;
        transaction.apply(data)?;
    }
    Ok(())
}
impl Gateway {
    pub fn preview_grok_import(
        &self,
        home: &Path,
        selected: Option<&str>,
        use_environment: bool,
    ) -> Result<Preview> {
        self.preview_grok_import_with_env(home, selected, use_environment, &|name| {
            std::env::var(name).ok()
        })
    }
    pub(super) fn preview_grok_import_with_env(
        &self,
        home: &Path,
        selected: Option<&str>,
        use_environment: bool,
        inherited: &impl Fn(&str) -> Option<String>,
    ) -> Result<Preview> {
        if self.0.client != ClientId::Grok {
            return Err(error("此操作仅适用于 Grok Build"));
        }
        *self.0.grok_import.0.lock().unwrap() = None;
        let s = self.0.inner.lock().unwrap();
        if s.running
            || self.0.data.join("gateway-recovery.json").exists()
            || self.0.data.join(JOURNAL).exists()
        {
            return Err(error("请先停用 Grok 网关并完成配置恢复，再导入"));
        }
        let (_, config) = grok_config::read(home)?;
        let env_path = home.join(ENV_FILE);
        let available = std::fs::symlink_metadata(&env_path)
            .is_ok_and(|m| m.is_file() && !m.file_type().is_symlink());
        let effective =
            super::grok_layers::read_with_environment(home, use_environment, inherited)?;
        let env = effective.environment.clone();
        let resolve = |name: &str| effective.resolve(name, inherited);
        let mut doc = document(&effective.text)?;
        if let Some(selected) = selected {
            if doc.get("model").and_then(|v| v.get(selected)).is_none()
                && doc
                    .get("models")
                    .and_then(|v| v.get("default"))
                    .and_then(Item::as_str)
                    != Some(selected)
            {
                return Err(error("所选模型已不存在"));
            }
            set_selected(&mut doc, selected);
        }
        let mut preview = Preview {
            layers: effective.layers.clone(),
            model_source: if selected.is_some() {
                Some("手动选择".into())
            } else {
                effective.model_source.clone()
            },
            unknown: effective.unknown.clone(),
            token: None,
            revision: self.view_revision(&s)?,
            models: discover(&effective.text, &resolve)?,
            selected: doc
                .get("models")
                .and_then(|v| v.get("default"))
                .and_then(Item::as_str)
                .unwrap_or_default()
                .into(),
            environment_available: available,
            environment_loaded: env.is_some(),
            clear_environment: Vec::new(),
            error: None,
        };
        let prepared = (|| -> Result<Pending> {
            if let Some(message) = &effective.block {
                return Err(error(message));
            }
            if preview.selected.is_empty() {
                return Err(error("默认模型未确定，请选择要导入的模型"));
            }
            let p = grok_config::profile_with_env(&doc.to_string(), &resolve)?;
            if p.pair
                .token
                .as_deref()
                .is_some_and(|token| token.starts_with("gs_"))
                && p.pair
                    .base_url
                    .as_deref()
                    .is_some_and(super::client::loopback_address)
            {
                return Err(AppError::new(code::MANAGED, "不能导入网关的本地凭据"));
            }
            let after_config = migrate(&config, &p, s.store.grok_api_model.is_some())?;
            super::model::base_url(
                p.pair.base_url.as_deref().unwrap(),
                self.try_listener_view()?.port,
            )?;
            let mut clear: BTreeSet<String> = p
                .credential_names
                .iter()
                .filter(|name| resolve(name).as_ref() == p.pair.token.as_ref())
                .cloned()
                .collect();
            for name in ["GROK_MODELS_BASE_URL", "GROK_XAI_API_BASE_URL"] {
                if resolve(name).as_ref() == p.pair.base_url.as_ref() && p.pair.base_url.is_some() {
                    clear.insert(name.into());
                }
            }
            if resolve("GROK_AUTH_PREFERRED_METHOD").as_deref() == Some("api_key") {
                clear.insert("GROK_AUTH_PREFERRED_METHOD".into());
            }
            preview.clear_environment = clear.iter().cloned().collect();
            Ok(Pending {
                layers: effective.files.clone(),
                overlays: super::grok_layers::environment_snapshot(inherited),
                token: uuid::Uuid::new_v4().to_string(),
                revision: preview.revision.clone(),
                home: home.to_owned(),
                config,
                after_environment: env.as_deref().map(|text| remove_environment(text, &clear)),
                environment: env,
                profile: p,
                after_config,
            })
        })();
        match prepared {
            Ok(pending) => {
                preview.token = Some(pending.token.clone());
                *self.0.grok_import.0.lock().unwrap() = Some(pending);
            }
            Err(e) => preview.error = Some(e.message),
        }
        Ok(preview)
    }
    pub fn cancel_grok_import(&self, token: Option<&str>) {
        let mut pending = self.0.grok_import.0.lock().unwrap();
        if token.is_none()
            || pending
                .as_ref()
                .is_some_and(|p| Some(p.token.as_str()) == token)
        {
            *pending = None;
        }
    }
    pub fn commit_grok_import(&self, home: &Path, token: &str, expected: &str) -> Result<View> {
        let _lifecycle = self
            .0
            .lifecycle
            .try_lock()
            .map_err(|_| error("Grok 网关正在启停"))?;
        let _mutation = self.0.registry.mutation.lock().unwrap();
        let mut s = self.0.inner.lock().unwrap();
        let mut pending = self.0.grok_import.0.lock().unwrap();
        let p = pending
            .as_ref()
            .filter(|p| p.token == token)
            .ok_or_else(|| error("导入预览已失效，请重新预览"))?;
        if s.running
            || s.upgrade_error.is_some()
            || self.0.data.join(JOURNAL).exists()
            || self.0.data.join("gateway-recovery.json").exists()
            || p.home != home
            || expected != p.revision
            || expected != self.view_revision(&s)?
        {
            return Err(AppError::new(
                code::CONFLICT,
                "Grok 配置或供应商库已变化，请重新预览",
            ));
        }
        if grok_config::read(home)?.1 != p.config
            || !super::grok_layers::unchanged(&p.layers)?
            || super::grok_layers::environment_snapshot(&|name| std::env::var(name).ok())
                != p.overlays
            || (p.environment.is_some()
                && storage::read_optional(&home.join(ENV_FILE))?.as_deref()
                    != p.environment.as_deref().map(str::as_bytes))
        {
            return Err(AppError::new(
                code::CONFLICT,
                "Grok 配置或环境文件已变化，请重新预览",
            ));
        }
        let base = p
            .profile
            .pair
            .base_url
            .clone()
            .ok_or_else(|| error("API 地址缺失"))?;
        let base = super::model::base_url(&base, self.try_listener_view()?.port)?
            .as_str()
            .trim_end_matches('/')
            .to_owned();
        let token = p
            .profile
            .pair
            .token
            .clone()
            .ok_or_else(|| error("API 凭据缺失"))?;
        let mut next = s.store.clone();
        next.settings.port = self.try_listener_view()?.port;
        let id = if let Some(existing) = next
            .providers
            .iter()
            .find(|provider| provider.base_url == base && provider.token == token)
        {
            existing.id.clone()
        } else {
            next.edit(
                Edit::SaveProvider {
                    search_only: None,
                    quota_access_token: None,
                    id: None,
                    base_url: base,
                    token,
                    name: Some(format!("Grok API · {}", p.profile.model)),
                },
                false,
            )?;
            next.providers
                .last()
                .ok_or_else(|| error("未生成供应商"))?
                .id
                .clone()
        };
        let same_connection = next.grok_connections.get(&id).is_some_and(|connection| {
            connection.protocol == p.profile.connection.protocol
                && connection.headers == p.profile.connection.headers
                && connection.query == p.profile.connection.query
        });
        if !same_connection {
            next.providers
                .iter_mut()
                .find(|provider| provider.id == id)
                .ok_or_else(|| error("供应商已不存在"))?
                .version = uuid::Uuid::new_v4().to_string();
        }
        next.grok_connections
            .insert(id.clone(), p.profile.connection.clone());
        next.selected = Some(id);
        next.grok_api_model = Some(MANAGED.into());
        next.grok_resume = None;
        next.resume = None;
        next.initialized = true;
        let before = storage::read_optional(&self.0.data.join("gateway.json"))?;
        if storage::revision(before.as_deref()) != s.revision {
            return Err(AppError::new(code::CONFLICT, "供应商库已被外部修改"));
        }
        let after =
            serde_json::to_string_pretty(&next).map_err(|_| error("无法保存 Grok 供应商"))?;
        let transaction = Transaction {
            version: 1,
            home: home.to_owned(),
            config: Change {
                before: Some(p.config.clone()),
                after: p.after_config.clone(),
            },
            environment: p.after_environment.as_ref().map(|after| Change {
                before: p.environment.clone(),
                after: after.clone(),
            }),
            store: Change {
                before: before
                    .map(String::from_utf8)
                    .transpose()
                    .map_err(|_| error("供应商库不是 UTF-8"))?,
                after: after.clone(),
            },
        };
        // Reject oversized participants before creating a durable transaction.
        if transaction
            .files(&self.0.data)
            .iter()
            .any(|(_, change)| change.after.len() as u64 > FILE_LIMIT)
        {
            return Err(AppError::new(code::FILE_SIZE, "Grok 导入文件超过允许大小"));
        }
        storage::atomic_write_bounded(
            &self.0.data.join(JOURNAL),
            &serde_json::to_vec(&transaction).map_err(|_| error("无法建立 Grok 导入事务"))?,
            Some("missing"),
            JOURNAL_LIMIT,
        )?;
        if let Err(e) = transaction.apply(&self.0.data) {
            s.upgrade_error = Some(e.clone());
            s.error = Some(e.message.clone());
            drop(pending);
            drop(s);
            self.changed();
            return Err(e);
        }
        self.replace_store(&mut s, next);
        s.revision = storage::digest(after.as_bytes());
        s.home = Some(home.to_owned());
        self.0.admission.configure(&s.store.providers, false);
        *pending = None;
        drop(pending);
        drop(s);
        self.changed();
        self.try_view()
    }
}
