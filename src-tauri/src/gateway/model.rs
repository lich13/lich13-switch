use crate::storage::{self, AppError, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

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
    #[serde(default = "default_websocket_support")]
    pub supports_websocket: bool,
}
fn default_websocket_support() -> bool {
    true
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    pub port: u16,
    pub max_retries: usize,
    pub failure_threshold: u32,
    pub success_threshold: u32,
    pub cooldown_seconds: u64,
    #[serde(default = "default_rate_limit_seconds")]
    pub rate_limit_seconds: u64,
    #[serde(default = "default_capacity_retry_seconds")]
    pub capacity_retry_seconds: u64,
    #[serde(default = "default_capacity_retry_seconds")]
    pub websocket_retry_seconds: u64,
    #[serde(default = "default_websocket_support")]
    pub handoff_after_compaction: bool,
    pub error_rate: f64,
    pub min_requests: u32,
    pub first_byte_seconds: u64,
    pub idle_seconds: u64,
    pub total_seconds: u64,
    pub connect_seconds: u64,
    #[serde(default = "default_wait_seconds")]
    pub queue_seconds: u64,
    #[serde(default = "default_max_waiting")]
    pub max_waiting: usize,
}
fn default_wait_seconds() -> u64 {
    30
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
            port: 15722,
            max_retries: 3,
            failure_threshold: 4,
            success_threshold: 2,
            cooldown_seconds: 60,
            rate_limit_seconds: default_rate_limit_seconds(),
            capacity_retry_seconds: default_capacity_retry_seconds(),
            websocket_retry_seconds: default_capacity_retry_seconds(),
            handoff_after_compaction: true,
            error_rate: 0.6,
            min_requests: 10,
            first_byte_seconds: 60,
            idle_seconds: 120,
            total_seconds: 600,
            connect_seconds: 15,
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
            || self.success_threshold == 0
            || self.min_requests == 0
            || self.max_waiting == 0
            || self.max_waiting > 10000
            || !(0.01..=1.0).contains(&self.error_rate)
            || [
                self.cooldown_seconds,
                self.rate_limit_seconds,
                self.capacity_retry_seconds,
                self.websocket_retry_seconds,
                self.first_byte_seconds,
                self.idle_seconds,
                self.total_seconds,
                self.connect_seconds,
                self.queue_seconds,
            ]
            .iter()
            .any(|s| *s == 0 || *s > 86400)
        {
            return Err(AppError::new("SETTINGS", "端口、阈值和超时参数无效"));
        }
        Ok(())
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Store {
    pub schema: u32,
    #[serde(default)]
    pub initialized: bool,
    pub providers: Vec<Provider>,
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
impl Default for Store {
    fn default() -> Self {
        Self {
            schema: 2,
            initialized: false,
            providers: vec![],
            settings: Settings::default(),
            mode: "manual".into(),
            selected: None,
            resume: None,
            local_token: format!(
                "gs_{}{}",
                uuid::Uuid::new_v4().simple(),
                uuid::Uuid::new_v4().simple()
            ),
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
        id: Option<String>,
        base_url: String,
        token: String,
        #[serde(default)]
        name: Option<String>,
    },
    RenameProvider {
        id: String,
        name: String,
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
        supports_websocket: bool,
        allowed_models: Option<Vec<String>>,
    },
    WebsocketProvider {
        id: String,
        supports_websocket: bool,
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
    Import,
}
pub fn name(value: &str) -> Result<String> {
    let value = value.trim();
    if value.is_empty() || value.chars().count() > 120 || value.chars().any(char::is_control) {
        Err(AppError::new("NAME", "名称应为 1–120 个可见字符"))
    } else {
        Ok(value.to_owned())
    }
}
pub fn base_url(value: &str, port: u16) -> Result<url::Url> {
    let u = url::Url::parse(value.trim())
        .map_err(|_| AppError::new("URL", "请输入完整的 HTTP 或 HTTPS base_url"))?;
    if !["http", "https"].contains(&u.scheme())
        || u.host_str().is_none()
        || !u.username().is_empty()
        || u.password().is_some()
        || u.query().is_some()
        || u.fragment().is_some()
    {
        return Err(AppError::new(
            "URL",
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
        return Err(AppError::new("LOOP", "上游不能指向本网关"));
    }
    Ok(u)
}
impl Store {
    pub fn load(path: &Path) -> Result<(Self, String)> {
        let raw = storage::read_optional(path)?;
        let store: Self = match &raw {
            Some(raw) => serde_json::from_slice(raw)
                .map_err(|_| AppError::new("STORE", "网关存储无法读取，请保留原文件"))?,
            None => Self::default(),
        };
        if !matches!(store.schema, 1 | 2) {
            return Err(AppError::new("STORE", "网关存储版本不受支持"));
        }
        store.settings.validate()?;
        Ok((store, storage::revision(raw.as_deref())))
    }
    pub fn persist(&self, path: &Path, expected: &str) -> Result<String> {
        let raw = serde_json::to_vec_pretty(self)
            .map_err(|_| AppError::new("STORE", "无法保存网关设置"))?;
        storage::atomic_write(path, &raw, Some(expected))?;
        Ok(storage::digest(&raw))
    }
    pub fn provider_mut(&mut self, id: &str) -> Result<&mut Provider> {
        self.providers
            .iter_mut()
            .find(|p| p.id == id)
            .ok_or_else(|| AppError::new("PROVIDER", "供应商不存在"))
    }
    pub fn edit(&mut self, edit: Edit, running: bool) -> Result<()> {
        match edit {
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
                    return Err(AppError::new("DUPLICATE", "该供应商已存在"));
                }
                let selected = self.selected.clone();
                self.edit(
                    Edit::SaveProvider {
                        id: None,
                        base_url: raw,
                        token,
                        name: Some(display_name),
                    },
                    running,
                )?;
                self.selected = selected;
            }
            Edit::SaveProvider {
                id,
                base_url: raw,
                token,
                name: display_name,
            } => {
                let u = base_url(&raw, self.settings.port)?;
                if token.contains(['\r', '\n']) {
                    return Err(AppError::new("TOKEN", "Token 不得包含换行"));
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
                    p.base_url = raw.trim().to_owned();
                    if !token.is_empty() {
                        p.token = token;
                    }
                    if display_name.is_some() {
                        p.name = label;
                    }
                    p.version = uuid::Uuid::new_v4().to_string();
                } else {
                    if token.trim().is_empty() {
                        return Err(AppError::new("TOKEN", "请输入供应商 Token"));
                    }
                    let id = uuid::Uuid::new_v4().to_string();
                    self.providers.push(Provider {
                        id: id.clone(),
                        name: label,
                        base_url: raw.trim().into(),
                        token,
                        queued: true,
                        version: uuid::Uuid::new_v4().to_string(),
                        max_concurrency: 0,
                        max_rpm: 0,
                        allowed_models: None,
                        supports_websocket: default_websocket_support(),
                    });
                    if self.selected.is_none() {
                        self.selected = Some(id);
                    }
                }
            }
            Edit::RenameProvider { id, name: value } => {
                self.provider_mut(&id)?.name = name(&value)?
            }
            Edit::DeleteProvider { id } => {
                self.providers.retain(|p| p.id != id);
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
                        "CONCURRENCY",
                        "并发上限应为 0–100000，0 表示不限",
                    ));
                }
                self.provider_mut(&id)?.max_concurrency = max_concurrency;
            }
            Edit::RpmProvider { id, max_rpm } => {
                if max_rpm > 100000 {
                    return Err(AppError::new("RPM", "RPM 上限应为 0–100000，0 表示不限"));
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
                                    "MODELS",
                                    "模型 ID 应为 1–256 字节，不能包含控制字符或通配符",
                                ));
                            }
                            if !result.iter().any(|m| m == model) {
                                result.push(model.to_owned());
                            }
                        }
                        if result.is_empty() || result.len() > 4096 {
                            return Err(AppError::new("MODELS", "白名单应包含 1–4096 个模型"));
                        }
                        Ok(result)
                    })
                    .transpose()?;
                self.provider_mut(&id)?.allowed_models = allowed;
            }
            Edit::PolicyProvider {
                id,
                supports_websocket,
                allowed_models,
            } => {
                self.edit(
                    Edit::ModelsProvider {
                        id: id.clone(),
                        allowed_models,
                    },
                    running,
                )?;
                self.provider_mut(&id)?.supports_websocket = supports_websocket;
            }
            Edit::WebsocketProvider {
                id,
                supports_websocket,
            } => {
                self.provider_mut(&id)?.supports_websocket = supports_websocket;
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
                    return Err(AppError::new("QUEUE", "排序必须包含每个供应商且不能重复"));
                }
                self.providers
                    .sort_by_key(|p| ids.iter().position(|id| id == &p.id).unwrap_or(usize::MAX));
            }
            Edit::Select { id } => {
                self.provider_mut(&id)?;
                self.selected = Some(id);
                self.mode = "manual".into();
            }
            Edit::Mode { mode } => {
                if !["manual", "auto"].contains(&mode.as_str()) {
                    return Err(AppError::new("MODE", "模式无效"));
                }
                self.mode = mode;
            }
            Edit::Settings { settings } => {
                settings.validate()?;
                if running && settings.port != self.settings.port {
                    return Err(AppError::new("RUNNING", "修改端口前请先停用网关"));
                }
                for p in &self.providers {
                    base_url(&p.base_url, settings.port)?;
                }
                self.settings = settings;
            }
            Edit::Import | Edit::Reset { .. } => unreachable!("handled by gateway"),
        }
        Ok(())
    }
}
