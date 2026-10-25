use crate::errors::code;
use crate::storage::{self, AppError, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Provider {
    pub id: String,
    pub name: String,
    pub base_url: String,
    pub token: String,
    pub queued: bool,
    pub version: String,
    #[serde(default)]
    pub max_concurrency: u32,
    #[serde(default)]
    pub max_rpm: u32,
    #[serde(default)]
    pub allowed_models: Option<Vec<String>>,
    #[serde(default)]
    pub handoff_after_compaction: bool,
    #[serde(default)]
    pub take_new_threads: bool,
    #[serde(default)]
    pub search_only: bool,
}
// Existing defaults: three retries allow four total upstream attempts.
const DEFAULT_MAX_RETRIES: usize = 3;
const DEFAULT_FAILURE_THRESHOLD: u32 = 4;
const DEFAULT_SUCCESS_THRESHOLD: u32 = 2;
const DEFAULT_COOLDOWN_SECONDS: u64 = 60;
const DEFAULT_ERROR_RATE: f64 = 0.6;
const DEFAULT_MIN_REQUESTS: u32 = 10;
// These phase limits have different meanings even where the values match.
const DEFAULT_FIRST_BYTE_SECONDS: u64 = 300;
const DEFAULT_IDLE_SECONDS: u64 = 300;
const DEFAULT_RETRY_BUDGET_SECONDS: u64 = 300;
const DEFAULT_CONNECT_SECONDS: u64 = 300;
pub(super) const USAGE_TAIL_DEADLINE: std::time::Duration = std::time::Duration::from_millis(1500);

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    pub port: u16,
    pub max_retries: usize,
    pub failure_threshold: u32,
    #[serde(default = "default_transient_failure_threshold")]
    pub transient_failure_threshold: u32,
    pub success_threshold: u32,
    pub cooldown_seconds: u64,
    #[serde(default = "default_rate_limit_seconds")]
    pub rate_limit_seconds: u64,
    #[serde(default = "default_capacity_retry_seconds")]
    pub capacity_retry_seconds: u64,
    #[serde(default = "default_request_seconds")]
    pub websocket_retry_seconds: u64,
    #[serde(default = "default_thread_idle_seconds")]
    pub thread_idle_release_seconds: u64,
    #[serde(default = "default_endpoint_probe_seconds")]
    pub endpoint_probe_seconds: u64,
    pub error_rate: f64,
    pub min_requests: u32,
    pub first_byte_seconds: u64,
    pub idle_seconds: u64,
    #[serde(default = "default_request_seconds")]
    pub retry_budget_seconds: u64,
    pub connect_seconds: u64,
    #[serde(default = "default_wait_seconds")]
    pub queue_seconds: u64,
    #[serde(default = "default_max_waiting")]
    pub max_waiting: usize,
}
fn default_thread_idle_seconds() -> u64 {
    300
}
fn default_endpoint_probe_seconds() -> u64 {
    300
}
fn default_request_seconds() -> u64 {
    300
}
fn default_wait_seconds() -> u64 {
    default_request_seconds()
}
fn default_transient_failure_threshold() -> u32 {
    6
}
fn default_rate_limit_seconds() -> u64 {
    5
}
fn default_capacity_retry_seconds() -> u64 {
    60
}
fn default_max_waiting() -> usize {
    100
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            port: super::client::DEFAULT_SHARED_PORT,
            max_retries: DEFAULT_MAX_RETRIES,
            failure_threshold: DEFAULT_FAILURE_THRESHOLD,
            transient_failure_threshold: default_transient_failure_threshold(),
            success_threshold: DEFAULT_SUCCESS_THRESHOLD,
            cooldown_seconds: DEFAULT_COOLDOWN_SECONDS,
            rate_limit_seconds: default_rate_limit_seconds(),
            capacity_retry_seconds: default_capacity_retry_seconds(),
            websocket_retry_seconds: default_request_seconds(),
            thread_idle_release_seconds: default_thread_idle_seconds(),
            endpoint_probe_seconds: default_endpoint_probe_seconds(),
            error_rate: DEFAULT_ERROR_RATE,
            min_requests: DEFAULT_MIN_REQUESTS,
            first_byte_seconds: DEFAULT_FIRST_BYTE_SECONDS,
            idle_seconds: DEFAULT_IDLE_SECONDS,
            retry_budget_seconds: DEFAULT_RETRY_BUDGET_SECONDS,
            connect_seconds: DEFAULT_CONNECT_SECONDS,
            queue_seconds: default_wait_seconds(),
            max_waiting: default_max_waiting(),
        }
    }
}
impl Settings {
    pub fn validate(&self) -> Result<()> {
        if self.port == 0
            || self.max_retries > 15
            || self.failure_threshold == 0
            || !(1..=1000).contains(&self.transient_failure_threshold)
            || self.success_threshold == 0
            || self.min_requests == 0
            || self.max_waiting == 0
            || self.max_waiting > 10000
            || !(10..=86400).contains(&self.endpoint_probe_seconds)
            || !(0.01..=1.0).contains(&self.error_rate)
            || [
                self.cooldown_seconds,
                self.rate_limit_seconds,
                self.capacity_retry_seconds,
                self.websocket_retry_seconds,
                self.thread_idle_release_seconds,
                self.first_byte_seconds,
                self.idle_seconds,
                self.retry_budget_seconds,
                self.connect_seconds,
                self.queue_seconds,
            ]
            .iter()
            .any(|s| *s == 0 || *s > 86400)
        {
            return Err(AppError::new(code::SETTINGS, "端口、阈值和超时参数无效"));
        }
        Ok(())
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Store {
    pub schema: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub successful_user_agent: Option<Vec<u8>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub successful_codex_identity: Option<super::testing::CodexIdentity>,
    #[serde(default)]
    pub phase_timeouts: bool,
    #[serde(default)]
    pub initialized: bool,
    #[serde(default)]
    pub connection: super::codex_api::Connection,
    pub providers: Vec<Provider>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub quota_access_tokens: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub provider_endpoints: BTreeMap<String, Vec<String>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub grok_connections: BTreeMap<String, super::grok_config::Connection>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub dsh_models: BTreeMap<String, super::dsh::ModelMapping>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dsh_catalog: Vec<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grok_resume: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grok_api_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dsh: Option<super::dsh::Snapshot>,
    pub settings: Settings,
    pub mode: String,
    pub selected: Option<String>,
    pub local_token: String,
    #[serde(default)]
    pub resume: Option<Resume>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Resume {
    pub home: PathBuf,
    pub desired: bool,
    pub pair_hash: String,
}
pub(super) fn new_local_token() -> String {
    format!(
        "gs_{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}
impl Default for Store {
    fn default() -> Self {
        Self {
            schema: 2,
            successful_user_agent: None,
            successful_codex_identity: None,
            phase_timeouts: true,
            initialized: false,
            connection: Default::default(),
            providers: vec![],
            quota_access_tokens: BTreeMap::new(),
            provider_endpoints: BTreeMap::new(),
            grok_connections: BTreeMap::new(),
            dsh_models: BTreeMap::new(),
            dsh_catalog: vec![],
            grok_resume: None,
            grok_api_model: None,
            dsh: None,
            settings: Settings::default(),
            mode: "manual".into(),
            selected: None,
            resume: None,
            local_token: new_local_token(),
        }
    }
}
#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum Edit {
    #[serde(skip)]
    ImportLink {
        base_url: String,
        token: String,
        display_name: String,
    },
    SaveProvider {
        #[serde(default)]
        search_only: Option<bool>,
        id: Option<String>,
        base_url: String,
        token: String,
        #[serde(default)]
        quota_access_token: Option<String>,
        #[serde(default)]
        name: Option<String>,
    },
    SaveGrokProvider {
        id: Option<String>,
        base_url: String,
        token: String,
        #[serde(default)]
        quota_access_token: Option<String>,
        #[serde(default)]
        name: Option<String>,
        protocol: super::grok_config::Protocol,
    },
    SaveDshProvider {
        id: Option<String>,
        base_url: String,
        token: String,
        #[serde(default)]
        quota_access_token: Option<String>,
        #[serde(default)]
        name: Option<String>,
        protocol: super::grok_config::Protocol,
    },
    DshModelsProvider {
        id: String,
        mapping: super::dsh::ModelMapping,
    },
    RenameProvider {
        id: String,
        name: String,
    },
    EndpointsProvider {
        id: String,
        base_urls: Vec<String>,
    },
    SwapEndpoint {
        id: String,
        base_url: String,
    },
    DeleteProvider {
        id: String,
    },
    QueueProvider {
        id: String,
        queued: bool,
    },
    ConcurrencyProvider {
        id: String,
        max_concurrency: u32,
    },
    RpmProvider {
        id: String,
        max_rpm: u32,
    },
    ModelsProvider {
        id: String,
        allowed_models: Option<Vec<String>>,
    },
    PolicyProvider {
        id: String,
        allowed_models: Option<Vec<String>>,
        #[serde(default)]
        handoff_after_compaction: Option<bool>,
        #[serde(default)]
        take_new_threads: Option<bool>,
    },
    TakeoverProvider {
        id: String,
        #[serde(default)]
        handoff_after_compaction: Option<bool>,
        #[serde(default)]
        take_new_threads: Option<bool>,
    },
    Reorder {
        ids: Vec<String>,
    },
    Select {
        id: String,
    },
    Mode {
        mode: String,
    },
    Settings {
        settings: Settings,
    },
    Reset {
        id: String,
    },
    Connection {
        mode: String,
    },
    Import,
}
pub fn name(value: &str) -> Result<String> {
    let value = value.trim();
    if value.is_empty() || value.chars().count() > 120 || value.chars().any(char::is_control) {
        Err(AppError::new(code::NAME, "名称应为 1–120 个可见字符"))
    } else {
        Ok(value.to_owned())
    }
}
pub fn base_url(value: &str, port: u16) -> Result<url::Url> {
    let mut u = url::Url::parse(value.trim())
        .map_err(|_| AppError::new(code::URL, "请输入完整的 HTTP 或 HTTPS base_url"))?;
    if !["http", "https"].contains(&u.scheme())
        || u.host_str().is_none()
        || !u.username().is_empty()
        || u.password().is_some()
        || u.query().is_some()
        || u.fragment().is_some()
    {
        return Err(AppError::new(
            code::URL,
            "base_url 只接受不含认证、查询参数及片段的 HTTP(S) 地址",
        ));
    }
    let host = u.host_str().unwrap_or_default().trim_matches(['[', ']']);
    if (host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback()))
        && u.port_or_known_default() == Some(port)
    {
        return Err(AppError::new(code::LOOP, "上游不能指向本网关"));
    }
    normalize_api_path(&mut u);
    Ok(u)
}
pub(super) fn normalize_api_path(url: &mut url::Url) {
    let mut path = url.path().trim_end_matches('/').to_owned();
    for endpoint in [
        "/responses/compact",
        "/chat/completions",
        "/alpha/search",
        "/responses",
        "/messages",
        "/completions",
        "/embeddings",
        "/models",
    ] {
        if let Some(base) = path.strip_suffix(endpoint) {
            path = base.to_owned();
            break;
        }
    }
    let versioned = path.split('/').any(|segment| {
        segment
            .strip_prefix('v')
            .is_some_and(|version| version.as_bytes().first().is_some_and(u8::is_ascii_digit))
    });
    if !versioned {
        path.push_str("/v1");
    }
    url.set_path(&path);
}
fn quota_site(base: &str) -> String {
    base.trim_end_matches('/')
        .trim_end_matches("/v1")
        .to_owned()
}
impl Store {
    pub fn load(path: &Path) -> Result<(Self, String)> {
        let raw = storage::read_optional(path)?;
        let mut store: Self = match &raw {
            Some(raw) => {
                let invalid = || AppError::new(code::STORE, "网关存储无法读取，请保留原文件");
                let mut value: serde_json::Value =
                    serde_json::from_slice(raw).map_err(|_| invalid())?;
                crate::migrations::dsh_model_mappings(&mut value);
                crate::migrations::gateway_wait_budget(&mut value);
                serde_json::from_value(value).map_err(|_| invalid())?
            }
            None => Self::default(),
        };
        if !matches!(store.schema, 1 | 2) {
            return Err(AppError::new(code::STORE, "网关存储版本不受支持"));
        }
        store.phase_timeouts = true;
        for model in &mut store.dsh_catalog {
            super::dsh::models::normalize(model);
        }
        store.settings.validate()?;
        store
            .quota_access_tokens
            .retain(|id, token| !token.is_empty() && store.providers.iter().any(|p| &p.id == id));
        store
            .provider_endpoints
            .retain(|id, _| store.providers.iter().any(|p| &p.id == id));
        for p in &store.providers {
            if let Some(values) = store.provider_endpoints.get_mut(&p.id) {
                *values = endpoint_urls(&p.base_url, values, store.settings.port)?;
            }
        }
        Ok((store, storage::revision(raw.as_deref())))
    }
    pub fn persist(&self, path: &Path, expected: &str) -> Result<String> {
        let raw = serde_json::to_vec_pretty(self)
            .map_err(|_| AppError::new(code::STORE, "无法保存网关设置"))?;
        storage::atomic_write(path, &raw, Some(expected))?;
        Ok(storage::digest(&raw))
    }
    pub fn provider_mut(&mut self, id: &str) -> Result<&mut Provider> {
        self.providers
            .iter_mut()
            .find(|p| p.id == id)
            .ok_or_else(|| AppError::new(code::PROVIDER, "供应商不存在"))
    }
    pub fn edit(&mut self, edit: Edit, running: bool) -> Result<()> {
        if let Edit::SaveDshProvider { protocol, .. } = &edit {
            if *protocol != super::grok_config::Protocol::ChatCompletions {
                return Err(AppError::new(code::PROVIDER, "DSH 仅支持 Chat Completions"));
            }
        }
        match edit {
            Edit::DshModelsProvider { id, mut mapping } => {
                mapping.validate()?;
                let provider = self.provider_mut(&id)?;
                provider.allowed_models = Some(mapping.allowed());
                provider.version = uuid::Uuid::new_v4().to_string();
                self.dsh_models.insert(id, mapping);
            }
            Edit::SaveGrokProvider {
                id,
                base_url,
                token,
                quota_access_token,
                name,
                protocol,
            }
            | Edit::SaveDshProvider {
                id,
                base_url,
                token,
                quota_access_token,
                name,
                protocol,
            } => {
                let existing = id.clone();
                let replacing_token = !token.is_empty();
                self.edit(
                    Edit::SaveProvider {
                        search_only: None,
                        id,
                        base_url,
                        token,
                        quota_access_token,
                        name,
                    },
                    running,
                )?;
                let id = existing
                    .or_else(|| self.providers.last().map(|p| p.id.clone()))
                    .ok_or_else(|| AppError::new(code::PROVIDER, "供应商不存在"))?;
                let connection = self.grok_connections.entry(id.clone()).or_default();
                let mut connection_changed = false;
                if replacing_token {
                    // An explicitly edited credential replaces imported auth
                    // headers too; otherwise the old secret wins at dispatch.
                    connection_changed |= connection.headers.remove("authorization").is_some();
                    connection_changed |= connection.headers.remove("x-api-key").is_some();
                }
                if connection.protocol != protocol {
                    connection.protocol = protocol;
                    connection_changed = true;
                }
                if connection_changed {
                    self.provider_mut(&id)?.version = uuid::Uuid::new_v4().to_string();
                }
            }
            Edit::ImportLink {
                base_url: raw,
                token,
                display_name,
            } => {
                let url = base_url(&raw, self.settings.port)?;
                if self.providers.iter().any(|p| {
                    p.token == token
                        && base_url(&p.base_url, self.settings.port).is_ok_and(|u| {
                            u.as_str().trim_end_matches('/') == url.as_str().trim_end_matches('/')
                        })
                }) {
                    return Err(AppError::new(code::DUPLICATE, "该供应商已存在"));
                }
                let selected = self.selected.clone();
                self.edit(
                    Edit::SaveProvider {
                        search_only: None,
                        id: None,
                        base_url: raw,
                        token,
                        quota_access_token: None,
                        name: Some(display_name),
                    },
                    running,
                )?;
                self.selected = selected;
            }
            Edit::SaveProvider {
                search_only,
                id,
                base_url: raw,
                token,
                quota_access_token,
                name: display_name,
            } => {
                let u = base_url(&raw, self.settings.port)?;
                if quota_access_token.as_ref().is_some_and(|value| {
                    value.len() > 16384
                        || value.chars().any(char::is_control)
                        || (!value.is_empty() && value.trim() != value)
                }) {
                    return Err(AppError::new(code::TOKEN, "账号访问令牌格式无效"));
                }
                let existing_id = id.clone();
                if let Some(id) = &id {
                    let old = self.provider_mut(id)?.base_url.clone();
                    if quota_site(&old) != quota_site(u.as_str()) {
                        self.quota_access_tokens.remove(id);
                    }
                }
                if token.contains(['\r', '\n']) {
                    return Err(AppError::new(code::TOKEN, "Token 不得包含换行"));
                }
                let label = if let Some(value) = display_name
                    .as_deref()
                    .filter(|value| !value.trim().is_empty())
                {
                    name(value)?
                } else {
                    u.host_str().unwrap_or("Provider").to_owned()
                };
                if let Some(id) = id {
                    let p = self.provider_mut(&id)?;
                    let base_url = u.as_str().trim_end_matches('/');
                    let credentials_changed =
                        p.base_url != base_url || (!token.is_empty() && p.token != token);
                    let policy_changed = search_only.is_some_and(|value| value != p.search_only);
                    if let Some(value) = search_only {
                        p.search_only = value;
                    }
                    p.base_url = base_url.to_owned();
                    if !token.is_empty() {
                        p.token = token;
                    }
                    if display_name.is_some() {
                        p.name = label;
                    }
                    if credentials_changed || policy_changed {
                        p.version = uuid::Uuid::new_v4().to_string();
                    }
                } else {
                    if token.trim().is_empty() {
                        return Err(AppError::new(code::TOKEN, "请输入供应商 Token"));
                    }
                    let id = uuid::Uuid::new_v4().to_string();
                    self.providers.push(Provider {
                        id: id.clone(),
                        name: label,
                        base_url: u.as_str().trim_end_matches('/').into(),
                        token,
                        queued: true,
                        version: uuid::Uuid::new_v4().to_string(),
                        max_concurrency: 0,
                        max_rpm: 0,
                        allowed_models: None,
                        handoff_after_compaction: false,
                        take_new_threads: false,
                        search_only: search_only.unwrap_or(false),
                    });
                    if self.selected.is_none() {
                        self.selected = Some(id);
                    }
                }
                if let Some(value) = quota_access_token {
                    let id =
                        existing_id.unwrap_or_else(|| self.providers.last().unwrap().id.clone());
                    if value.is_empty() {
                        self.quota_access_tokens.remove(&id);
                    } else {
                        self.quota_access_tokens.insert(id, value);
                    }
                }
            }
            Edit::RenameProvider { id, name: value } => {
                self.provider_mut(&id)?.name = name(&value)?
            }
            Edit::EndpointsProvider { id, base_urls } => {
                let primary = self.provider_mut(&id)?.base_url.clone();
                let values = endpoint_urls(&primary, &base_urls, self.settings.port)?;
                if values.is_empty() {
                    self.provider_endpoints.remove(&id);
                    self.grok_connections.remove(&id);
                } else {
                    self.provider_endpoints.insert(id, values);
                }
            }
            Edit::SwapEndpoint {
                id,
                base_url: selected,
            } => {
                let primary = self.provider_mut(&id)?.base_url.clone();
                let mut values = self
                    .provider_endpoints
                    .get(&id)
                    .cloned()
                    .unwrap_or_default();
                let index = values
                    .iter()
                    .position(|url| url == &selected)
                    .ok_or_else(|| AppError::new(code::ENDPOINT, "辅助端点已变化，请刷新后重试"))?;
                values[index] = primary;
                if quota_site(&values[index]) != quota_site(&selected) {
                    self.quota_access_tokens.remove(&id);
                }
                // Swapping roles does not change credentials or the endpoint set.
                // Keep the provider version so existing conversations retain affinity.
                self.provider_mut(&id)?.base_url = selected;
                self.provider_endpoints.insert(id, values);
            }
            Edit::DeleteProvider { id } => {
                self.quota_access_tokens.remove(&id);
                self.providers.retain(|p| p.id != id);
                self.dsh_models.remove(&id);
                self.provider_endpoints.remove(&id);
                if self.selected.as_deref() == Some(&id) {
                    self.selected = self.providers.first().map(|p| p.id.clone());
                }
            }
            Edit::QueueProvider { id, queued } => self.provider_mut(&id)?.queued = queued,
            Edit::ConcurrencyProvider {
                id,
                max_concurrency,
            } => {
                if max_concurrency > 100000 {
                    return Err(AppError::new(
                        code::CONCURRENCY,
                        "并发上限应为 0–100000，0 表示不限",
                    ));
                }
                self.provider_mut(&id)?.max_concurrency = max_concurrency;
            }
            Edit::RpmProvider { id, max_rpm } => {
                if max_rpm > 100000 {
                    return Err(AppError::new(
                        code::RPM,
                        "RPM 上限应为 0–100000，0 表示不限",
                    ));
                }
                self.provider_mut(&id)?.max_rpm = max_rpm;
            }
            Edit::ModelsProvider { id, allowed_models } => {
                let allowed = allowed_models
                    .map(|models| {
                        let mut result = Vec::new();
                        for model in models {
                            let model = model.trim();
                            if model.is_empty()
                                || model.len() > 256
                                || model.chars().any(char::is_control)
                                || model.contains('*')
                            {
                                return Err(AppError::new(
                                    code::MODELS,
                                    "模型 ID 应为 1–256 字节，不能包含控制字符或通配符",
                                ));
                            }
                            if !result.iter().any(|m| m == model) {
                                result.push(model.to_owned());
                            }
                        }
                        if result.is_empty() || result.len() > 4096 {
                            return Err(AppError::new(code::MODELS, "白名单应包含 1–4096 个模型"));
                        }
                        Ok(result)
                    })
                    .transpose()?;
                self.provider_mut(&id)?.allowed_models = allowed;
            }
            Edit::PolicyProvider {
                id,
                allowed_models,
                handoff_after_compaction,
                take_new_threads,
            } => {
                self.edit(
                    Edit::ModelsProvider {
                        id: id.clone(),
                        allowed_models,
                    },
                    running,
                )?;
                let provider = self.provider_mut(&id)?;
                if let Some(value) = handoff_after_compaction {
                    provider.handoff_after_compaction = value;
                }
                if let Some(value) = take_new_threads {
                    provider.take_new_threads = value;
                }
            }
            Edit::TakeoverProvider {
                id,
                handoff_after_compaction,
                take_new_threads,
            } => {
                if handoff_after_compaction.is_none() && take_new_threads.is_none() {
                    return Err(AppError::new(code::SETTINGS, "请选择要修改的接管开关"));
                }
                let provider = self.provider_mut(&id)?;
                if let Some(value) = handoff_after_compaction {
                    provider.handoff_after_compaction = value;
                }
                if let Some(value) = take_new_threads {
                    provider.take_new_threads = value;
                }
            }
            Edit::Reorder { ids } => {
                let mut sorted = ids.clone();
                sorted.sort();
                sorted.dedup();
                if sorted.len() != self.providers.len()
                    || ids.len() != sorted.len()
                    || sorted
                        .iter()
                        .any(|id| !self.providers.iter().any(|p| &p.id == id))
                {
                    return Err(AppError::new(
                        code::QUEUE,
                        "排序必须包含每个供应商且不能重复",
                    ));
                }
                self.providers
                    .sort_by_key(|p| ids.iter().position(|id| id == &p.id).unwrap_or(usize::MAX));
            }
            Edit::Select { id } => {
                if self.provider_mut(&id)?.search_only {
                    return Err(AppError::new(
                        code::SETTINGS,
                        "仅网络搜索供应商不能用于普通生成",
                    ));
                }
                self.selected = Some(id);
                self.mode = "manual".into();
            }
            Edit::Mode { mode } => {
                if !["manual", "auto"].contains(&mode.as_str()) {
                    return Err(AppError::new(code::MODE, "模式无效"));
                }
                self.mode = mode;
            }
            Edit::Settings { settings } => {
                settings.validate()?;
                if running && settings.port != self.settings.port {
                    return Err(AppError::new(code::RUNNING, "修改端口前请先停用网关"));
                }
                for p in &self.providers {
                    base_url(&p.base_url, settings.port)?;
                }
                self.settings = settings;
            }
            Edit::Import | Edit::Reset { .. } | Edit::Connection { .. } => {
                unreachable!("handled by gateway")
            }
        }
        for provider in &self.providers {
            if let Some(urls) = self.provider_endpoints.get_mut(&provider.id) {
                *urls = endpoint_urls(&provider.base_url, urls, self.settings.port)?;
            }
        }
        Ok(())
    }
}

pub fn endpoint_urls(primary: &str, values: &[String], port: u16) -> Result<Vec<String>> {
    if values.len() > 8 {
        return Err(AppError::new(code::ENDPOINT, "最多添加 8 个辅助端点"));
    }
    let canonical = |raw: &str| -> Result<String> {
        if raw.len() > 2048 || raw.chars().any(char::is_control) {
            return Err(AppError::new(code::ENDPOINT, "端点地址过长或含有无效字符"));
        }
        Ok(base_url(raw, port)?
            .as_str()
            .trim_end_matches('/')
            .to_owned())
    };
    let main = canonical(primary)?;
    let mut result = Vec::new();
    for value in values {
        let url = canonical(value)?;
        if url != main && !result.contains(&url) {
            result.push(url);
        }
    }
    Ok(result)
}
