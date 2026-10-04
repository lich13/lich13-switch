use crate::storage::{self, AppError, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Preferences {
    pub codex_home: String,
    #[serde(default = "crate::gateway::claude_home")]
    pub claude_home: String,
    pub cli_path: String,
    pub theme: String,
    #[serde(default = "default_quota_refresh")]
    pub quota_refresh_seconds: u64,
    #[serde(default = "default_notifications")]
    pub system_notifications: bool,
}
pub fn default_quota_refresh() -> u64 {
    60
}
pub fn default_notifications() -> bool {
    true
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Profile {
    id: String,
    name: String,
    identity: String,
    kind: String,
    email: Option<String>,
    auth: String,
    updated_at: u64,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Store {
    schema: u32,
    profiles: Vec<Profile>,
    preferences: Preferences,
    seen_roots: Vec<String>,
    #[serde(default)]
    observed_auth_revisions: BTreeMap<String, String>,
    #[serde(default)]
    auth_sync_version: u32,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Account {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub email: Option<String>,
    pub current: bool,
    pub updated_at: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AuthSource {
    pub provider: String,
    pub credential_store: String,
    pub inline_token: bool,
    pub env_key: bool,
    pub command_auth: bool,
    pub requires_openai_auth: bool,
    pub warning: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ViewState {
    pub accounts: Vec<Account>,
    pub auth_revision: String,
    pub config_revision: String,
    pub current_state: String,
    pub auth_source: AuthSource,
    pub preferences: Preferences,
    pub error: Option<String>,
    pub auth_sync: Option<AuthSync>,
    #[serde(default)]
    pub official_mode: crate::official::OfficialModeView,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigDocument {
    pub text: String,
    pub revision: String,
    pub path: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AuthSync {
    pub state: String,
    pub account_id: Option<String>,
    pub message: String,
    pub at: u64,
}
pub struct Core {
    data_dir: PathBuf,
    store: Store,
    auth_sync: Option<AuthSync>,
    checked_auth: Option<(String, String)>,
}
struct AuthInfo {
    identity: String,
    kind: String,
    email: Option<String>,
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn claims(token: &str) -> Option<Value> {
    serde_json::from_slice(&URL_SAFE_NO_PAD.decode(token.split('.').nth(1)?).ok()?).ok()
}
fn auth_info(raw: &str) -> Result<AuthInfo> {
    if raw.len() > 2 * 1024 * 1024 {
        return Err(AppError::new("AUTH", "凭据文件超过 2 MiB"));
    }
    let d: Value =
        serde_json::from_str(raw).map_err(|_| AppError::new("AUTH", "凭据不是有效的 JSON"))?;
    let mode = d.get("auth_mode").and_then(Value::as_str);
    if mode == Some("apikey")
        || (mode.is_none()
            && d.get("OPENAI_API_KEY")
                .and_then(Value::as_str)
                .is_some_and(|s| !s.trim().is_empty()))
    {
        let key = d
            .get("OPENAI_API_KEY")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| AppError::new("AUTH", "缺少有效的 OPENAI_API_KEY"))?;
        return Ok(AuthInfo {
            identity: storage::digest(format!("api:{key}").as_bytes()),
            kind: "apiKey".into(),
            email: None,
        });
    }
    if mode.is_some_and(|m| m != "chatgpt") {
        return Err(AppError::new(
            "AUTH",
            "此认证格式暂不支持，请导入 Codex 的 ChatGPT 或 API Key 凭据",
        ));
    }
    let tokens = d
        .get("tokens")
        .and_then(Value::as_object)
        .ok_or_else(|| AppError::new("AUTH", "缺少 ChatGPT tokens 或 OPENAI_API_KEY"))?;
    for field in ["access_token", "refresh_token", "id_token"] {
        if tokens
            .get(field)
            .and_then(Value::as_str)
            .is_none_or(|v| v.is_empty())
        {
            return Err(AppError::new("AUTH", "ChatGPT 凭据缺少必要的 Token 字段"));
        }
    }
    let id = claims(tokens["id_token"].as_str().unwrap_or_default()).unwrap_or(Value::Null);
    let access = claims(tokens["access_token"].as_str().unwrap_or_default()).unwrap_or(Value::Null);
    let subject = id
        .get("sub")
        .or_else(|| access.get("sub"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            AppError::new(
                "AUTH",
                "无法识别 ChatGPT 账号身份，请重新登录或导入完整凭据",
            )
        })?;
    let workspace = tokens
        .get("account_id")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let email = id.get("email").and_then(Value::as_str).map(str::to_owned);
    Ok(AuthInfo {
        identity: storage::digest(format!("chatgpt:{subject}:{workspace}").as_bytes()),
        kind: "chatgpt".into(),
        email,
    })
}
fn older(incoming: &str, stored: &str) -> bool {
    fn timestamps(raw: &str) -> (Option<i64>, Option<i64>) {
        let Ok(v) = serde_json::from_str::<Value>(raw) else {
            return (None, None);
        };
        let refresh = v
            .get("last_refresh")
            .and_then(|v| {
                v.as_i64()
                    .and_then(|n| n.checked_mul(1_000_000))
                    .or_else(|| {
                        v.as_str().and_then(|s| {
                            chrono::DateTime::parse_from_rfc3339(s)
                                .ok()
                                .map(|d| d.timestamp_micros())
                        })
                    })
            })
            .filter(|t| *t > 0);
        let issued = ["access_token", "id_token"].iter().find_map(|key| {
            v.get("tokens")
                .and_then(|v| v.get(key))
                .and_then(Value::as_str)
                .and_then(claims)
                .and_then(|v| v.get("iat").and_then(Value::as_i64))
                .filter(|t| *t > 0)
        });
        (refresh, issued)
    }
    let (a, ai) = timestamps(incoming);
    let (b, bi) = timestamps(stored);
    if let (Some(a), Some(b)) = (a, b) {
        if a != b {
            return a < b;
        }
    }
    matches!((ai, bi), (Some(a), Some(b)) if a < b)
}
pub fn validate_config(text: &str) -> Result<()> {
    if text.len() > 2 * 1024 * 1024 {
        return Err(AppError::new("CONFIG", "配置文件超过 2 MiB"));
    }
    text.parse::<toml::Table>().map(|_| ()).map_err(|e| {
        let offset = e.span().map(|s| s.start).unwrap_or(0).min(text.len());
        let prefix = text.get(..offset).unwrap_or_default();
        AppError {
            code: "TOML".into(),
            message: "TOML 语法错误，请检查标记位置附近的引号、括号或重复字段".into(),
            line: Some(prefix.bytes().filter(|b| *b == b'\n').count() + 1),
            column: Some(
                prefix
                    .rsplit('\n')
                    .next()
                    .unwrap_or_default()
                    .chars()
                    .count()
                    + 1,
            ),
        }
    })
}
fn source(bytes: Option<&[u8]>) -> AuthSource {
    let text = bytes
        .and_then(|b| std::str::from_utf8(b).ok())
        .unwrap_or_default();
    let parsed = text.parse::<toml::Table>();
    let d = parsed.as_ref().ok();
    let provider = d
        .and_then(|d| d.get("model_provider"))
        .and_then(toml::Value::as_str)
        .unwrap_or("openai")
        .to_string();
    let store = d
        .and_then(|d| d.get("cli_auth_credentials_store"))
        .and_then(toml::Value::as_str)
        .unwrap_or("file")
        .to_string();
    let p = d
        .and_then(|d| d.get("model_providers"))
        .and_then(|p| p.get(&provider));
    let inline = p
        .and_then(|p| p.get("experimental_bearer_token"))
        .and_then(toml::Value::as_str)
        .is_some_and(|s| !s.is_empty());
    let env = p
        .and_then(|p| p.get("env_key"))
        .and_then(toml::Value::as_str)
        .is_some_and(|s| !s.is_empty());
    let command = p.and_then(|p| p.get("auth")).is_some();
    let requires = p
        .and_then(|p| p.get("requires_openai_auth"))
        .and_then(toml::Value::as_bool)
        .unwrap_or(provider == "openai");
    let warning = if parsed.is_err() {
        Some("配置语法错误，无法确认认证来源".into())
    } else if store != "file" {
        Some("当前配置未使用文件认证；切换 auth.json 可能不会改变 Codex 的登录".into())
    } else if inline || command || (env && !requires) {
        Some("当前 provider 另有独立凭据；切换账号不会修改这些配置".into())
    } else if !requires {
        Some("当前 provider 不使用 OpenAI 登录；账号切换只更新 auth.json".into())
    } else {
        None
    };
    AuthSource {
        provider,
        credential_store: store,
        inline_token: inline,
        env_key: env,
        command_auth: command,
        requires_openai_auth: requires,
        warning,
    }
}
impl Core {
    pub fn new(data_dir: PathBuf, default_home: PathBuf) -> Result<Self> {
        storage::private_dir(&data_dir)?;
        let path = data_dir.join("accounts.json");
        let store = match storage::read_optional(&path)? {
            Some(bytes) => serde_json::from_slice::<Store>(&bytes)
                .map_err(|_| AppError::new("STORE", "账号库无法读取，原文件已保留"))?,
            None => Store {
                schema: 1,
                profiles: vec![],
                preferences: Preferences {
                    codex_home: default_home.to_string_lossy().into(),
                    claude_home: crate::gateway::claude_home(),
                    cli_path: String::new(),
                    theme: "system".into(),
                    quota_refresh_seconds: default_quota_refresh(),
                    system_notifications: true,
                },
                seen_roots: vec![],
                observed_auth_revisions: BTreeMap::new(),
                auth_sync_version: 1,
            },
        };
        if store.schema != 1 {
            return Err(AppError::new(
                "STORE_VERSION",
                "账号库版本较新，请更新 lich13-switch",
            ));
        }
        let mut core = Self {
            data_dir,
            store,
            auth_sync: None,
            checked_auth: None,
        };
        if core.store.auth_sync_version == 0 {
            core.store.observed_auth_revisions.clear();
            core.store.auth_sync_version = 1;
            core.persist()?;
        }
        // Scan on every startup; revisions suppress duplicate imports and re-adding a deletion.
        let _ = core.state();
        Ok(core)
    }
    fn persist(&self) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(&self.store)
            .map_err(|_| AppError::new("STORE", "无法保存账号库"))?;
        storage::atomic_write(&self.data_dir.join("accounts.json"), &bytes, None)
    }
    pub fn home(&self) -> PathBuf {
        PathBuf::from(&self.store.preferences.codex_home)
    }
    pub fn preferences(&self) -> Preferences {
        self.store.preferences.clone()
    }
    pub fn import_raw(&mut self, raw: &str, name: Option<String>) -> Result<String> {
        let info = auth_info(raw)?;
        let old = self.store.clone();
        if let Ok(Some(bytes)) = storage::read_optional(&self.home().join("auth.json")) {
            if std::str::from_utf8(&bytes)
                .ok()
                .and_then(|raw| auth_info(raw).ok())
                .is_some_and(|disk| disk.identity == info.identity)
            {
                self.store.observed_auth_revisions.insert(
                    self.store.preferences.codex_home.clone(),
                    storage::digest(&bytes),
                );
            }
        }
        let id = if let Some(p) = self
            .store
            .profiles
            .iter_mut()
            .find(|p| p.identity == info.identity)
        {
            p.auth = raw.into();
            p.updated_at = now();
            p.email = info.email;
            p.id.clone()
        } else {
            let id = uuid::Uuid::new_v4().to_string();
            let label = name
                .filter(|s| !s.trim().is_empty())
                .or(info.email.clone())
                .unwrap_or_else(|| {
                    if info.kind == "chatgpt" {
                        "ChatGPT 账号"
                    } else {
                        "API Key"
                    }
                    .into()
                });
            self.store.profiles.push(Profile {
                id: id.clone(),
                name: clean_name(&label)?,
                identity: info.identity,
                kind: info.kind,
                email: info.email,
                auth: raw.into(),
                updated_at: now(),
            });
            id
        };
        if let Err(e) = self.persist() {
            self.store = old;
            return Err(e);
        }
        Ok(id)
    }
    pub fn add_api_key(&mut self, name: &str, key: &str) -> Result<String> {
        if key.trim().is_empty() || key.contains(['\n', '\r']) {
            return Err(AppError::new("KEY", "请输入单行有效 API Key"));
        }
        let raw = serde_json::to_string_pretty(
            &json!({"auth_mode":"apikey","OPENAI_API_KEY":key.trim()}),
        )
        .unwrap();
        self.import_raw(&raw, Some(clean_name(name)?))
    }
    pub fn import_file(&mut self, path: &Path, name: Option<String>) -> Result<String> {
        let bytes = storage::read_optional(path)?
            .ok_or_else(|| AppError::new("MISSING", "所选文件不存在"))?;
        let raw = String::from_utf8(bytes)
            .map_err(|_| AppError::new("UTF8", "凭据文件必须使用 UTF-8 编码"))?;
        self.import_raw(&raw, name)
    }
    pub fn rename(&mut self, id: &str, name: &str) -> Result<()> {
        let label = clean_name(name)?;
        let old = self.store.clone();
        self.store
            .profiles
            .iter_mut()
            .find(|p| p.id == id)
            .ok_or_else(|| AppError::new("ACCOUNT", "账号不存在"))?
            .name = label;
        if let Err(e) = self.persist() {
            self.store = old;
            return Err(e);
        }
        Ok(())
    }
    pub fn delete(&mut self, id: &str) -> Result<()> {
        let old = self.store.clone();
        if let Ok(Some(bytes)) = storage::read_optional(&self.home().join("auth.json")) {
            if let Some(info) = std::str::from_utf8(&bytes)
                .ok()
                .and_then(|raw| auth_info(raw).ok())
            {
                if self
                    .store
                    .profiles
                    .iter()
                    .any(|p| p.id == id && p.identity == info.identity)
                {
                    self.store.observed_auth_revisions.insert(
                        self.store.preferences.codex_home.clone(),
                        storage::digest(&bytes),
                    );
                }
            }
        }
        self.store.profiles.retain(|p| p.id != id);
        if let Err(e) = self.persist() {
            self.store = old;
            return Err(e);
        }
        Ok(())
    }
    pub fn state(&mut self) -> Result<ViewState> {
        let auth = storage::read_optional(&self.home().join("auth.json"))?;
        let config = storage::read_optional(&self.home().join("config.toml"))?;
        let info = auth
            .as_deref()
            .and_then(|b| std::str::from_utf8(b).ok())
            .map(auth_info);
        let identity = info
            .as_ref()
            .and_then(|i| i.as_ref().ok())
            .map(|i| i.identity.as_str());
        let auth_revision = storage::revision(auth.as_deref());
        let root = self.store.preferences.codex_home.clone();
        let changed = self.store.observed_auth_revisions.get(&root) != Some(&auth_revision);
        let old = self.store.clone();
        if changed {
            // A valid intermediate JSON document can still be part of a non-atomic write.
            if self.checked_auth.as_ref() != Some(&(root.clone(), auth_revision.clone())) {
                std::thread::sleep(std::time::Duration::from_millis(60));
                if storage::read_optional(&self.home().join("auth.json"))? != auth {
                    return Err(AppError::new(
                        "AUTH_PENDING",
                        "账号文件正在写入，稍后自动重试",
                    ));
                }
                self.checked_auth = Some((root.clone(), auth_revision.clone()));
            }
            if let (Some(Ok(info)), Some(raw)) = (
                &info,
                auth.as_deref().and_then(|b| std::str::from_utf8(b).ok()),
            ) {
                let (kind, id, message) = if let Some(profile) = self
                    .store
                    .profiles
                    .iter_mut()
                    .find(|p| p.identity == info.identity)
                {
                    if older(raw, &profile.auth) {
                        (
                            "older",
                            Some(profile.id.clone()),
                            "检测到旧凭据，已保留已保存版本",
                        )
                    } else if raw != profile.auth {
                        profile.auth = raw.into();
                        profile.email = info.email.clone();
                        profile.updated_at = now();
                        ("updated", Some(profile.id.clone()), "账号凭据已更新")
                    } else {
                        ("synced", Some(profile.id.clone()), "")
                    }
                } else {
                    let id = uuid::Uuid::new_v4().to_string();
                    let label = info.email.clone().unwrap_or_else(|| {
                        if info.kind == "chatgpt" {
                            "ChatGPT 账号".into()
                        } else {
                            "API Key".into()
                        }
                    });
                    self.store.profiles.push(Profile {
                        id: id.clone(),
                        name: label.chars().filter(|c| !c.is_control()).take(80).collect(),
                        identity: info.identity.clone(),
                        kind: info.kind.clone(),
                        email: info.email.clone(),
                        auth: raw.into(),
                        updated_at: now(),
                    });
                    ("added", Some(id), "新账号已自动加入切换列表")
                };
                self.store
                    .observed_auth_revisions
                    .insert(root, auth_revision.clone());
                if let Err(e) = self.persist() {
                    self.store = old;
                    return Err(e);
                }
                // This is a distinct stable version, even when the action and
                // account match the previous update. Unchanged polls skip here.
                self.auth_sync = None;
                self.set_sync(kind, id, message);
            } else {
                self.set_sync(
                    if auth.is_none() { "missing" } else { "invalid" },
                    None,
                    if auth.is_none() {
                        "未找到 auth.json，已有账号已保留"
                    } else {
                        "auth.json 暂时无效，已有账号已保留"
                    },
                );
            }
        }
        if !changed {
            // A temporary invalid/missing file may recover to the already observed
            // bytes, so clear its error without importing or touching timestamps.
            if identity.is_some()
                && self
                    .auth_sync
                    .as_ref()
                    .is_some_and(|s| ["missing", "invalid"].contains(&s.state.as_str()))
            {
                self.set_sync("synced", None, "");
            }
            if let Some(profile) = self
                .store
                .profiles
                .iter()
                .find(|p| Some(p.identity.as_str()) == identity)
            {
                if auth
                    .as_deref()
                    .and_then(|b| std::str::from_utf8(b).ok())
                    .is_some_and(|raw| older(raw, &profile.auth))
                {
                    self.set_sync(
                        "older",
                        Some(profile.id.clone()),
                        "检测到旧凭据，已保留已保存版本",
                    );
                } else if self.auth_sync.as_ref().is_some_and(|s| s.state == "older") {
                    self.set_sync("synced", None, "");
                }
            }
        }
        let accounts: Vec<_> = self
            .store
            .profiles
            .iter()
            .map(|p| Account {
                id: p.id.clone(),
                name: p.name.clone(),
                kind: p.kind.clone(),
                email: p.email.clone(),
                current: Some(p.identity.as_str()) == identity,
                updated_at: p.updated_at,
            })
            .collect();
        let status = if auth.is_none() {
            "missing"
        } else if info.as_ref().is_none_or(|i| i.is_err()) {
            "invalid"
        } else if accounts.iter().any(|p| p.current) {
            "saved"
        } else {
            "unsaved"
        };
        Ok(ViewState {
            accounts,
            auth_revision,
            config_revision: storage::revision(config.as_deref()),
            current_state: status.into(),
            auth_source: source(config.as_deref()),
            preferences: self.preferences(),
            error: None,
            auth_sync: self.auth_sync.clone(),
            official_mode: crate::official::OfficialModeView::default(),
        })
    }
    fn set_sync(&mut self, state: &str, account_id: Option<String>, message: &str) {
        if self
            .auth_sync
            .as_ref()
            .is_some_and(|s| s.state == state && s.account_id == account_id && s.message == message)
        {
            return;
        }
        self.auth_sync = Some(AuthSync {
            state: state.into(),
            account_id,
            message: message.into(),
            at: now(),
        });
    }
    pub fn switch_account(&mut self, id: &str, expected: &str) -> Result<ViewState> {
        let state = self.state()?;
        if state.auth_revision != expected {
            return Err(AppError::new(
                "CONFLICT",
                "账号文件已变化，列表已刷新，请重新选择",
            ));
        }
        let profile = self
            .store
            .profiles
            .iter()
            .find(|p| p.id == id)
            .ok_or_else(|| AppError::new("ACCOUNT", "账号不存在"))?;
        auth_info(&profile.auth)?;
        let raw = profile.auth.clone();
        let path = self.home().join("auth.json");
        let previous = storage::read_optional(&path)?;
        if storage::revision(previous.as_deref()) != expected {
            return Err(AppError::new("CONFLICT", "账号文件已变化，请重新选择"));
        }
        if let Some(bytes) = previous {
            storage::atomic_write(&self.data_dir.join("previous-auth.json"), &bytes, None)?;
        }
        storage::atomic_write(&path, raw.as_bytes(), Some(expected))?;
        self.state()
    }
    pub fn read_config(&self) -> Result<ConfigDocument> {
        let path = self.home().join("config.toml");
        let bytes = storage::read_optional(&path)?;
        let revision = storage::revision(bytes.as_deref());
        let text = String::from_utf8(bytes.unwrap_or_default())
            .map_err(|_| AppError::new("UTF8", "配置必须使用 UTF-8 编码"))?;
        Ok(ConfigDocument {
            text,
            revision,
            path: path.to_string_lossy().into(),
        })
    }
    pub fn save_config(&self, text: &str, expected: &str) -> Result<ConfigDocument> {
        validate_config(text)?;
        let path = self.home().join("config.toml");
        let bytes = storage::read_optional(&path)?;
        if expected != storage::revision(bytes.as_deref()) {
            return Err(AppError::new(
                "CONFLICT",
                "配置已被其他程序修改。草稿已保留，请重新读取并合并后保存",
            ));
        }
        if let Some(bytes) = bytes {
            storage::atomic_write(&self.data_dir.join("previous-config.toml"), &bytes, None)?;
        }
        storage::atomic_write(&path, text.as_bytes(), Some(expected))?;
        self.read_config()
    }
    pub fn set_preferences(&mut self, prefs: Preferences) -> Result<ViewState> {
        if prefs.quota_refresh_seconds != 0 && !(10..=86400).contains(&prefs.quota_refresh_seconds)
        {
            return Err(AppError::new("PREFERENCES", "刷新间隔需为 10–86400 秒"));
        }
        if !Path::new(&prefs.claude_home).is_absolute() {
            return Err(AppError::new("PATH", "Claude 配置目录必须是绝对路径"));
        }
        if !["system", "dark", "light"].contains(&prefs.theme.as_str()) {
            return Err(AppError::new("THEME", "主题无效"));
        }
        let root = PathBuf::from(&prefs.codex_home);
        if !root.is_absolute() || !root.is_dir() {
            return Err(AppError::new("PATH", "请选择已存在的 Codex 配置目录"));
        }
        if !prefs.cli_path.is_empty()
            && (!Path::new(&prefs.cli_path).is_absolute() || !Path::new(&prefs.cli_path).is_file())
        {
            return Err(AppError::new(
                "CLI",
                "请选择 Codex CLI 可执行文件的绝对路径",
            ));
        }
        let old = self.store.clone();
        self.store.preferences = prefs;
        self.store.preferences.codex_home = std::fs::canonicalize(root)
            .map_err(storage::io_error)?
            .to_string_lossy()
            .into();
        if let Err(e) = self.persist() {
            self.store = old;
            return Err(e);
        }
        self.state()
    }
}
fn clean_name(name: &str) -> Result<String> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 100 || name.chars().any(char::is_control) {
        Err(AppError::new("NAME", "名称需为 1–100 个字符，不能包含换行"))
    } else {
        Ok(name.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn setup() -> (tempfile::TempDir, Core) {
        let t = tempfile::tempdir().unwrap();
        let h = t.path().join("codex");
        std::fs::create_dir(&h).unwrap();
        let c = Core::new(t.path().join("app"), h).unwrap();
        (t, c)
    }
    fn jwt(subject: &str) -> String {
        format!(
            "e30.{}.test",
            URL_SAFE_NO_PAD
                .encode(json!({"sub":subject,"email":"test@example.invalid"}).to_string())
        )
    }
    fn oauth(subject: &str, refresh: &str) -> String {
        json!({"auth_mode":"chatgpt","tokens":{"account_id":"workspace","id_token":jwt(subject),"access_token":jwt(subject),"refresh_token":refresh},"future":{"keep":true}}).to_string()
    }
    #[test]
    fn switches_preserve_config_and_unknown_auth_fields() {
        let (_t, mut c) = setup();
        let cfg = b"# keep\nmodel='test'\n[plugins.example]\nenabled=true\n";
        std::fs::write(c.home().join("config.toml"), cfg).unwrap();
        let a = c.import_raw(&oauth("a", "old"), Some("A".into())).unwrap();
        let b = c.add_api_key("B", "test-key").unwrap();
        for id in [&a, &b, &a] {
            let rev = c.state().unwrap().auth_revision;
            assert!(c
                .switch_account(id, &rev)
                .unwrap()
                .accounts
                .iter()
                .any(|p| p.current && p.id == *id));
            assert_eq!(std::fs::read(c.home().join("config.toml")).unwrap(), cfg);
        }
        let v: Value =
            serde_json::from_slice(&std::fs::read(c.home().join("auth.json")).unwrap()).unwrap();
        assert_eq!(v["future"]["keep"], true);
    }
    #[test]
    fn refreshed_tokens_survive_roundtrip_and_different_identity_is_not_overwritten() {
        let (_t, mut c) = setup();
        let a = c.import_raw(&oauth("a", "old"), None).unwrap();
        c.switch_account(&a, "missing").unwrap();
        std::fs::write(c.home().join("auth.json"), oauth("a", "new")).unwrap();
        c.state().unwrap();
        std::fs::write(c.home().join("auth.json"), oauth("b", "other")).unwrap();
        let s = c.state().unwrap();
        assert_eq!(s.current_state, "saved");
        assert_eq!(s.accounts.len(), 2);
        c.switch_account(&a, &s.auth_revision).unwrap();
        assert!(std::fs::read_to_string(c.home().join("auth.json"))
            .unwrap()
            .contains("new"));
    }
    #[test]
    fn explicit_new_login_is_not_replaced_by_unchanged_disk_credentials() {
        let (t, mut c) = setup();
        let a = c.import_raw(&oauth("a", "old"), None).unwrap();
        c.switch_account(&a, "missing").unwrap();
        let before = std::fs::read(c.home().join("auth.json")).unwrap();
        c.import_raw(&oauth("a", "new-login"), None).unwrap();
        c.state().unwrap();
        assert_eq!(std::fs::read(c.home().join("auth.json")).unwrap(), before);
        let mut reopened = Core::new(t.path().join("app"), c.home()).unwrap();
        let revision = reopened.state().unwrap().auth_revision;
        reopened.switch_account(&a, &revision).unwrap();
        assert_eq!(
            std::fs::read_to_string(c.home().join("auth.json")).unwrap(),
            oauth("a", "new-login")
        );
        std::fs::write(c.home().join("auth.json"), oauth("a", "external-refresh")).unwrap();
        reopened.state().unwrap();
        assert!(reopened.store.profiles[0].auth.contains("external-refresh"));
    }
    #[test]
    fn conflicts_do_not_overwrite_external_files() {
        let (_t, mut c) = setup();
        let a = c.add_api_key("A", "test").unwrap();
        std::fs::write(c.home().join("auth.json"), "external").unwrap();
        assert_eq!(
            c.switch_account(&a, "missing").unwrap_err().code,
            "CONFLICT"
        );
        assert_eq!(
            std::fs::read_to_string(c.home().join("auth.json")).unwrap(),
            "external"
        );
        let d = c.read_config().unwrap();
        std::fs::write(c.home().join("config.toml"), "model='external'").unwrap();
        assert!(c.save_config("model='draft'", &d.revision).is_err());
    }
    #[test]
    fn config_roundtrip_keeps_exact_text_and_auth() {
        let (_t, mut c) = setup();
        let a = c.add_api_key("A", "test").unwrap();
        c.switch_account(&a, "missing").unwrap();
        let before = c.state().unwrap().auth_revision;
        let text = "# comment\r\nmodel = 'gpt-example'\r\n[future]\r\nvalue = [1, 2]\r\n";
        let d = c.save_config(text, "missing").unwrap();
        assert_eq!(d.text, text);
        assert_eq!(before, c.state().unwrap().auth_revision);
        assert!(c.save_config("x=[", &d.revision).is_err());
        assert_eq!(c.read_config().unwrap().text, text);
    }
    #[test]
    fn import_validation_dedup_and_no_tokens_in_state() {
        let (_t, mut c) = setup();
        assert!(c.import_raw("{}", None).is_err());
        let a = c.add_api_key("A", "SENSITIVE_TEST_ONLY").unwrap();
        let b = c.add_api_key("B", "SENSITIVE_TEST_ONLY").unwrap();
        assert_eq!(a, b);
        assert_eq!(c.state().unwrap().accounts.len(), 1);
        assert!(!serde_json::to_string(&c.state().unwrap())
            .unwrap()
            .contains("SENSITIVE_TEST_ONLY"));
    }
    #[test]
    fn first_launch_only_imports_once() {
        let (t, mut c) = setup();
        let a = c.add_api_key("A", "test").unwrap();
        c.switch_account(&a, "missing").unwrap();
        c.delete(&a).unwrap();
        let mut reopened = Core::new(t.path().join("app"), c.home()).unwrap();
        assert!(reopened.state().unwrap().accounts.is_empty());
        assert_eq!(reopened.state().unwrap().current_state, "unsaved");
    }
    #[test]
    fn monitor_adds_and_updates_preserving_identity_name_and_unknown_fields() {
        let (_t, mut c) = setup();
        let path = c.home().join("auth.json");
        let cfg = b"# never change\nmodel='fixture'\n";
        std::fs::write(c.home().join("config.toml"), cfg).unwrap();
        std::fs::write(&path, oauth("a", "first")).unwrap();
        let first = c.state().unwrap();
        let id = first.accounts[0].id.clone();
        assert_eq!(first.auth_sync.unwrap().state, "added");
        c.rename(&id, "Personal name").unwrap();
        let raw = oauth("a", "refreshed");
        std::fs::write(&path, &raw).unwrap();
        let next = c.state().unwrap();
        assert_eq!(next.accounts[0].name, "Personal name");
        assert_eq!(next.accounts[0].id, id);
        assert_eq!(c.store.profiles[0].auth, raw);
        let stored = std::fs::read(c.data_dir.join("accounts.json")).unwrap();
        assert_eq!(c.state().unwrap(), next);
        assert_eq!(
            std::fs::read(c.data_dir.join("accounts.json")).unwrap(),
            stored
        );
        assert_eq!(std::fs::read(c.home().join("config.toml")).unwrap(), cfg);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), raw);
        let mut other: Value = serde_json::from_str(&oauth("a", "workspace2")).unwrap();
        other["tokens"]["account_id"] = json!("other-workspace");
        std::fs::write(&path, other.to_string()).unwrap();
        assert_eq!(c.state().unwrap().accounts.len(), 2);
    }
    #[test]
    fn old_credentials_are_retained_across_restart_and_can_be_explicitly_applied() {
        let (t, mut c) = setup();
        let path = c.home().join("auth.json");
        let version = |time: &str, token: &str| {
            let mut v: Value = serde_json::from_str(&oauth("a", token)).unwrap();
            v["last_refresh"] = json!(time);
            v.to_string()
        };
        let newer = version("2026-09-28T10:00:00Z", "newer");
        let old = version("2026-09-27T10:00:00Z", "old");
        std::fs::write(&path, &newer).unwrap();
        let id = c.state().unwrap().accounts[0].id.clone();
        std::fs::write(&path, &old).unwrap();
        assert_eq!(c.state().unwrap().auth_sync.unwrap().state, "older");
        assert_eq!(c.store.profiles[0].auth, newer);
        let mut c = Core::new(t.path().join("app"), c.home()).unwrap();
        let state = c.state().unwrap();
        assert_eq!(state.auth_sync.unwrap().state, "older");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), old);
        c.switch_account(&id, &state.auth_revision).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), newer);
        let issued = |n| {
            let token = format!(
                "e30.{}.test",
                URL_SAFE_NO_PAD.encode(json!({"sub":"a","iat":n}).to_string())
            );
            json!({"tokens":{"access_token":token}}).to_string()
        };
        assert!(older(&issued(100), &issued(200)));
        assert!(!older(&issued(300), &issued(200)));
        assert!(!older(&oauth("a", "no-time"), &newer));
        assert!(older(
            r#"{"last_refresh":"2026-09-28T01:00:00.001Z"}"#,
            r#"{"last_refresh":"2026-09-28T01:00:00.002Z"}"#,
        ));
    }
    #[test]
    fn invalid_files_deletion_and_failed_persistence_preserve_the_vault() {
        let (_t, mut c) = setup();
        let path = c.home().join("auth.json");
        std::fs::write(&path, oauth("a", "original")).unwrap();
        let first = c.state().unwrap();
        let id = first.accounts[0].id.clone();
        std::fs::write(&path, "{").unwrap();
        assert_eq!(c.state().unwrap().accounts.len(), 1);
        std::fs::remove_file(&path).unwrap();
        assert_eq!(c.state().unwrap().accounts.len(), 1);
        std::fs::write(&path, oauth("a", "original")).unwrap();
        let recovered = c.state().unwrap();
        assert_eq!(recovered.auth_sync.unwrap().state, "synced");
        assert_eq!(
            recovered.accounts[0].updated_at,
            first.accounts[0].updated_at
        );
        c.delete(&id).unwrap();
        assert!(c.state().unwrap().accounts.is_empty());
        std::fs::write(&path, oauth("a", "new-version")).unwrap();
        assert_eq!(c.state().unwrap().accounts.len(), 1);
        let stored = c.store.clone();
        let vault = c.data_dir.join("accounts.json");
        std::fs::remove_file(&vault).unwrap();
        std::fs::create_dir(&vault).unwrap();
        std::fs::write(&path, oauth("b", "second")).unwrap();
        assert!(c.state().is_err());
        assert_eq!(c.store.profiles.len(), stored.profiles.len());
        assert_eq!(
            c.store.observed_auth_revisions,
            stored.observed_auth_revisions
        );
        std::fs::remove_dir(&vault).unwrap();
        c.persist().unwrap();
        assert_eq!(c.state().unwrap().accounts.len(), 2);
    }
    #[test]
    fn upgrade_imports_previously_observed_unsaved_account_once() {
        let (t, mut c) = setup();
        let raw = oauth("a", "upgrade");
        std::fs::write(c.home().join("auth.json"), &raw).unwrap();
        c.store
            .observed_auth_revisions
            .insert(c.preferences().codex_home, storage::digest(raw.as_bytes()));
        c.store.auth_sync_version = 0;
        c.persist().unwrap();
        let mut next = Core::new(t.path().join("app"), c.home()).unwrap();
        assert_eq!(next.state().unwrap().accounts.len(), 1);
        drop(next);
        let mut next = Core::new(t.path().join("app"), c.home()).unwrap();
        assert_eq!(next.state().unwrap().accounts.len(), 1);
    }
    #[test]
    fn source_reports_independent_auth() {
        let s=source(Some(b"model_provider='custom'\n[model_providers.custom]\nrequires_openai_auth=true\nexperimental_bearer_token='secret'"));
        assert!(s.inline_token);
        assert!(s.warning.is_some());
        assert!(!serde_json::to_string(&s).unwrap().contains("secret"));
        assert_eq!(
            source(Some(b"cli_auth_credentials_store='keyring'")).credential_store,
            "keyring"
        );
    }
    #[test]
    fn writes_fail_without_changing_current() {
        let (_t, mut c) = setup();
        let a = c.add_api_key("A", "test").unwrap();
        std::fs::create_dir(c.home().join("auth.json")).unwrap();
        assert!(c.switch_account(&a, "missing").is_err());
        assert!(c.home().join("auth.json").is_dir());
    }
    #[test]
    fn invalid_toml_error_has_location_without_source() {
        let err = validate_config("model = 'secret\n").unwrap_err();
        assert!(err.line.is_some());
        assert!(!err.message.contains("secret"));
    }
    #[cfg(unix)]
    #[test]
    fn private_modes_and_symlink_protection() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let (t, mut c) = setup();
        let a = c.add_api_key("A", "test").unwrap();
        c.switch_account(&a, "missing").unwrap();
        assert_eq!(
            std::fs::metadata(c.home().join("auth.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(t.path().join("app"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        std::fs::remove_file(c.home().join("auth.json")).unwrap();
        symlink(t.path().join("victim"), c.home().join("auth.json")).unwrap();
        assert!(c.state().is_err());
        assert!(!t.path().join("victim").exists());
    }
}
