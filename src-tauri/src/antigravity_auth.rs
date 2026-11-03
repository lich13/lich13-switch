use crate::{
    errors::code,
    storage::{self, AppError, Result},
    Runtime,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Arc, LazyLock, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tauri_plugin_dialog::DialogExt;
use uuid::Uuid;

type R<'a> = tauri::State<'a, Arc<Runtime>>;

const VAULT_LIMIT: u64 = 4 * 1024 * 1024;
const CREDENTIAL_LIMIT: u64 = 256 * 1024;
static AUTH_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AccountKind {
    Oauth,
    Adc,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AuthMode {
    Oauth,
    Adc,
    MixedAdcFileWithoutFlag,
    MixedFlagWithoutAdcFile,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TargetMode {
    Oauth,
    Adc,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AdcStatus {
    Active,
    Disabled,
    BackupOnly,
    Missing,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AdcType {
    ServiceAccount,
    AuthorizedUser,
}

impl AdcType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ServiceAccount => "service_account",
            Self::AuthorizedUser => "authorized_user",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AntigravityAccountItem {
    pub id: String,
    pub name: String,
    pub kind: AccountKind,
    pub source_type: String,
    pub email: Option<String>,
    pub project_id: Option<String>,
    pub current: bool,
    pub updated_at: u64,
    pub credential_revision: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AntigravityAuthState {
    pub accounts: Vec<AntigravityAccountItem>,
    pub mode: AuthMode,
    pub enable_adc: bool,
    pub oauth_present: bool,
    pub oauth_email: Option<String>,
    pub oauth_auth_method: Option<String>,
    pub adc_status: AdcStatus,
    pub adc_type: Option<AdcType>,
    pub adc_email: Option<String>,
    pub adc_project: Option<String>,
    #[serde(default)]
    pub app_restarted: bool,
    pub revision: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddAntigravityAdcInput {
    pub name: Option<String>,
    pub json_text: String,
    pub quota_project_id: Option<String>,
    #[serde(default)]
    pub activate: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredEntry {
    id: String,
    name: String,
    kind: AccountKind,
    source_type: String,
    email: Option<String>,
    project_id: Option<String>,
    payload: String,
    updated_at: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Vault {
    schema: u32,
    entries: Vec<StoredEntry>,
    #[serde(default)]
    observed_revisions: Vec<String>,
}

impl Default for Vault {
    fn default() -> Self {
        Self {
            schema: 1,
            entries: Vec::new(),
            observed_revisions: Vec::new(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Paths {
    pub gemini_home: PathBuf,
    pub gcloud_dir: PathBuf,
    pub agswitch_dir: PathBuf,
    pub data_dir: PathBuf,
}

impl Paths {
    pub fn resolve(data_dir: &Path) -> Result<Self> {
        let user_home =
            dirs::home_dir().ok_or_else(|| AppError::new(code::PATH, "无法定位用户主目录"))?;
        Ok(Self::from_roots(&user_home, data_dir))
    }

    pub fn from_roots(user_home: &Path, data_dir: &Path) -> Self {
        Self {
            gemini_home: user_home.join(".gemini"),
            gcloud_dir: user_home.join(".config/gcloud"),
            agswitch_dir: user_home.join(".config/agswitch"),
            data_dir: data_dir.to_path_buf(),
        }
    }

    pub fn config_file(&self) -> PathBuf {
        self.gemini_home.join("config/config.json")
    }
    pub fn oauth_file(&self) -> PathBuf {
        self.gemini_home.join("jetski-standalone-oauth-token")
    }
    pub fn adc_file(&self) -> PathBuf {
        self.gcloud_dir.join("application_default_credentials.json")
    }
    pub fn adc_disabled_file(&self) -> PathBuf {
        self.gcloud_dir
            .join("application_default_credentials.json.disabled")
    }
    pub fn agswitch_adc_file(&self) -> PathBuf {
        self.agswitch_dir.join("adc.json")
    }
    pub fn vault_file(&self) -> PathBuf {
        self.data_dir.join("antigravity-accounts.json")
    }
}

pub trait AppLifecycle {
    fn is_running(&self) -> Result<bool>;
    fn quit(&self) -> Result<()>;
    fn launch(&self) -> Result<()>;
}

#[derive(Default)]
pub struct SystemAppLifecycle {
    #[cfg(target_os = "macos")]
    last_bundle: Mutex<Option<PathBuf>>,
}

#[cfg(target_os = "macos")]
pub(crate) fn parse_running_antigravity_ps(ps_output: &str) -> Vec<(i32, PathBuf)> {
    ps_output
        .lines()
        .filter_map(|line| {
            let (pid_str, comm) = line.trim().split_once(char::is_whitespace)?;
            let pid = pid_str.trim().parse::<i32>().ok()?;
            let bundle = comm.trim().strip_suffix("/Contents/MacOS/Antigravity")?;
            Some((pid, PathBuf::from(bundle)))
        })
        .collect()
}

#[cfg(target_os = "macos")]
impl SystemAppLifecycle {
    fn detect_running(&self) -> Result<Vec<(i32, PathBuf)>> {
        let out = std::process::Command::new("ps")
            .args(["-axo", "pid=,comm="])
            .output()
            .map_err(storage::io_error)?;
        let list = parse_running_antigravity_ps(&String::from_utf8_lossy(&out.stdout));
        if let Some((_, bundle)) = list.first() {
            if let Ok(mut guard) = self.last_bundle.lock() {
                *guard = Some(bundle.clone());
            }
        }
        Ok(list)
    }
}

impl AppLifecycle for SystemAppLifecycle {
    fn is_running(&self) -> Result<bool> {
        #[cfg(target_os = "macos")]
        {
            Ok(!self.detect_running()?.is_empty())
        }
        #[cfg(not(target_os = "macos"))]
        {
            Ok(false)
        }
    }

    fn quit(&self) -> Result<()> {
        #[cfg(target_os = "macos")]
        {
            let running = self.detect_running()?;
            if running.is_empty() {
                return Ok(());
            }
            let _ = std::process::Command::new("osascript")
                .args([
                    "-e",
                    "tell application id \"com.google.antigravity\" to quit",
                ])
                .output();
            for _ in 0..25 {
                if !self.is_running()? {
                    return Ok(());
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            for (pid, _) in &running {
                let _ = std::process::Command::new("kill")
                    .args(["-TERM", &pid.to_string()])
                    .output();
            }
            for _ in 0..15 {
                if !self.is_running()? {
                    return Ok(());
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(AppError::new(
                code::PROCESS,
                "等待 Antigravity 退出超时，未修改任何凭据文件，请手动关闭后重试",
            ))
        }
        #[cfg(not(target_os = "macos"))]
        {
            Ok(())
        }
    }

    fn launch(&self) -> Result<()> {
        #[cfg(target_os = "macos")]
        {
            let bundle = self.last_bundle.lock().ok().and_then(|g| g.clone());
            let mut cmd = std::process::Command::new("open");
            if let Some(app_path) = bundle.filter(|p| p.exists()) {
                cmd.arg("-a").arg(app_path);
            } else {
                cmd.args(["-b", "com.google.antigravity"]);
            }
            let status = cmd.status().map_err(storage::io_error)?;
            if !status.success() {
                return Err(AppError::new(
                    code::PROCESS,
                    "凭据已切换，但自动重新启动 Antigravity 失败，请手动打开应用",
                ));
            }
        }
        Ok(())
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn clean_opt(raw: Option<&str>) -> Option<String> {
    raw.map(str::trim)
        .filter(|s| !s.is_empty() && !s.chars().any(char::is_control))
        .map(str::to_owned)
}

fn map_str(map: &Map<String, Value>, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|k| map.get(*k).and_then(Value::as_str))
        .and_then(|s| clean_opt(Some(s)))
}

fn clean_label(raw: Option<&str>, fallback: &str) -> Result<String> {
    let target = raw
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(fallback);
    if target.is_empty() || target.chars().count() > 100 || target.chars().any(char::is_control) {
        return Err(AppError::new(
            code::NAME,
            "名称需为 1–100 个字符，不能包含控制字符",
        ));
    }
    Ok(target.to_owned())
}

fn payload_digest(raw: &str) -> String {
    storage::digest(raw.trim().as_bytes())
}

pub(crate) fn write_private_file(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(storage::io_error)?;
    }
    storage::atomic_write(path, bytes, None)
}

fn read_text_optional(path: &Path) -> Result<Option<String>> {
    let Some(bytes) = storage::read_bounded(path, CREDENTIAL_LIMIT)? else {
        return Ok(None);
    };
    let text = String::from_utf8(bytes)
        .map_err(|_| AppError::new(code::UTF8, "凭据文件不是有效的 UTF-8 文本"))?;
    if text.trim().is_empty() {
        return Ok(None);
    }
    Ok(Some(text))
}

fn load_vault(paths: &Paths) -> Result<(Vault, String)> {
    storage::private_dir(&paths.data_dir)?;
    let loaded = storage::read_bounded(&paths.vault_file(), VAULT_LIMIT)?;
    let rev = storage::revision(loaded.as_deref());
    let vault = match loaded {
        Some(bytes) => serde_json::from_slice::<Vault>(&bytes)
            .map_err(|_| AppError::new(code::STORE, "Antigravity 凭据库无法读取，原文件已保留"))?,
        None => Vault::default(),
    };
    if vault.schema != 1 {
        return Err(AppError::new(
            code::STORE_VERSION,
            "Antigravity 凭据库版本较新，请更新 Vela",
        ));
    }
    Ok((vault, rev))
}

fn save_vault(paths: &Paths, vault: &Vault) -> Result<()> {
    storage::private_dir(&paths.data_dir)?;
    let bytes = serde_json::to_vec_pretty(vault)
        .map_err(|_| AppError::new(code::STORE, "无法序列化 Antigravity 凭据库"))?;
    storage::atomic_write_bounded(&paths.vault_file(), &bytes, None, VAULT_LIMIT)
}

fn decode_jwt_email(token: &str) -> Option<String> {
    let payload_b64 = token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD
        .decode(payload_b64.trim_end_matches('='))
        .ok()?;
    let v: Value = serde_json::from_slice(&bytes).ok()?;
    clean_opt(v.get("email").and_then(Value::as_str))
}

pub fn parse_oauth_metadata(raw: &str) -> Result<(String, Option<String>)> {
    let value: Value = serde_json::from_str(raw.trim()).map_err(|_| {
        AppError::new(
            code::JSON,
            "OAuth 令牌文件不是有效的 JSON（请先在 Antigravity 中完成官方 OAuth 登录）",
        )
    })?;
    let Some(map) = value.as_object() else {
        return Err(AppError::new(
            code::CONFIG_TYPE,
            "OAuth 令牌顶层必须是 JSON 对象",
        ));
    };
    let nested_token = map.get("token").and_then(Value::as_object);
    let has_token = map_str(
        map,
        &["id_token", "access_token", "refresh_token", "oauth_token"],
    )
    .is_some()
        || nested_token
            .is_some_and(|t| map_str(t, &["access_token", "refresh_token", "id_token"]).is_some());
    if !has_token {
        return Err(AppError::new(
            code::AUTH,
            "OAuth 令牌缺少有效令牌字段（access_token / refresh_token / id_token）",
        ));
    }
    let email = map
        .get("user")
        .and_then(Value::as_object)
        .and_then(|u| map_str(u, &["email"]))
        .or_else(|| {
            map_str(
                map,
                &["id_token_email", "email", "account", "client_email", "user"],
            )
        })
        .or_else(|| nested_token.and_then(|t| map_str(t, &["email", "id_token_email"])))
        .or_else(|| {
            map_str(map, &["id_token"])
                .or_else(|| nested_token.and_then(|t| map_str(t, &["id_token"])))
                .and_then(|jwt| decode_jwt_email(&jwt))
        });
    let auth_method = map_str(map, &["auth_method", "provider"])
        .or_else(|| nested_token.and_then(|t| map_str(t, &["token_type"])))
        .unwrap_or_else(|| "oauth".to_owned());
    Ok((auth_method, email))
}

pub fn parse_adc_metadata(raw: &str) -> Result<(AdcType, Option<String>, Option<String>, Value)> {
    let value: Value = serde_json::from_str(raw.trim())
        .map_err(|_| AppError::new(code::JSON, "ADC 凭据必须是有效的 JSON 对象"))?;
    let Some(map) = value.as_object() else {
        return Err(AppError::new(
            code::CONFIG_TYPE,
            "ADC 凭据顶层必须是 JSON 对象",
        ));
    };
    let raw_type = map_str(map, &["type"]).ok_or_else(|| {
        AppError::new(
            code::AUTH,
            "ADC JSON 缺少 type 字段（需为 service_account 或 authorized_user）",
        )
    })?;
    let cred_type = match raw_type.as_str() {
        "service_account" => {
            let pk = map
                .get("private_key")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    AppError::new(code::AUTH, "service_account ADC 缺少 private_key 字段")
                })?;
            if !pk.contains("PRIVATE KEY") {
                return Err(AppError::new(
                    code::AUTH,
                    "service_account private_key 必须是有效的 PEM 私钥",
                ));
            }
            if map_str(map, &["client_email"]).is_none() {
                return Err(AppError::new(
                    code::AUTH,
                    "service_account ADC 缺少 client_email 字段",
                ));
            }
            if map_str(map, &["project_id", "quota_project_id"]).is_none() {
                return Err(AppError::new(
                    code::AUTH,
                    "service_account ADC 缺少 project_id 字段",
                ));
            }
            AdcType::ServiceAccount
        }
        "authorized_user" => {
            for field in ["client_id", "client_secret", "refresh_token"] {
                if map_str(map, &[field]).is_none() {
                    return Err(AppError::new(
                        code::AUTH,
                        &format!("authorized_user ADC 缺少必需字段：{field}"),
                    ));
                }
            }
            AdcType::AuthorizedUser
        }
        _ => {
            return Err(AppError::new(
                code::AUTH,
                "不支持的 ADC 类型，仅支持 service_account 或 authorized_user",
            ))
        }
    };
    let email = map_str(map, &["client_email", "account", "email"]);
    let project = map_str(map, &["quota_project_id", "project_id"]);
    Ok((cred_type, email, project, value))
}

pub(crate) fn read_enable_adc(paths: &Paths) -> Result<(bool, Option<Vec<u8>>, Value)> {
    let path = paths.config_file();
    let raw = storage::read_bounded(&path, CREDENTIAL_LIMIT)?;
    let doc = match &raw {
        Some(bytes) => {
            let text = String::from_utf8(bytes.clone())
                .map_err(|_| AppError::new(code::UTF8, "Antigravity config.json 编码无效"))?;
            super::validate_text(&text)?
        }
        None => json!({}),
    };
    let enabled = doc
        .get("userSettings")
        .and_then(|u| u.get("enableAdc"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Ok((enabled, raw, doc))
}

pub(crate) fn write_enable_adc(paths: &Paths, enable: bool) -> Result<()> {
    let path = paths.config_file();
    let (_, raw, mut doc) = read_enable_adc(paths)?;
    let expected = storage::revision(raw.as_deref());
    let obj = doc
        .as_object_mut()
        .ok_or_else(|| AppError::new(code::CONFIG_TYPE, "config.json 顶层必须是对象"))?;
    let user_settings = obj
        .entry("userSettings")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| AppError::new(code::CONFIG_TYPE, "userSettings 必须是对象"))?;
    user_settings.insert("enableAdc".into(), Value::Bool(enable));
    let mut formatted = serde_json::to_string_pretty(&doc)
        .map_err(|_| AppError::new(code::JSON, "无法序列化 config.json"))?;
    formatted.push('\n');
    super::validate_text(&formatted)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(storage::io_error)?;
    }
    storage::atomic_write(&path, formatted.as_bytes(), Some(&expected))
}

fn remember_observed(vault: &mut Vault, digest: &str) {
    if !vault.observed_revisions.iter().any(|r| r == digest) {
        vault.observed_revisions.push(digest.to_owned());
    }
}

fn sync_disk_credentials(paths: &Paths, vault: &mut Vault) -> Result<bool> {
    let mut dirty = false;
    if let Some(raw_oauth) = read_text_optional(&paths.oauth_file())? {
        let (auth_method, email) = parse_oauth_metadata(&raw_oauth)?;
        let digest = payload_digest(&raw_oauth);
        let idx = vault.entries.iter().position(|e| {
            e.kind == AccountKind::Oauth
                && match (&e.email, &email) {
                    (Some(ae), Some(de)) => ae.eq_ignore_ascii_case(de),
                    _ => payload_digest(&e.payload) == digest,
                }
        });
        if let Some(i) = idx {
            let entry = &mut vault.entries[i];
            if payload_digest(&entry.payload) != digest {
                entry.payload = raw_oauth;
                entry.email = email;
                entry.source_type = auth_method;
                entry.updated_at = now_secs();
                remember_observed(vault, &digest);
                dirty = true;
            }
        } else if !vault.observed_revisions.iter().any(|r| r == &digest) {
            let fallback = email.as_deref().unwrap_or("OAuth 账号");
            let name = clean_label(None, fallback)?;
            vault.entries.push(StoredEntry {
                id: Uuid::new_v4().to_string(),
                name,
                kind: AccountKind::Oauth,
                source_type: auth_method,
                email,
                project_id: None,
                payload: raw_oauth,
                updated_at: now_secs(),
            });
            remember_observed(vault, &digest);
            dirty = true;
        }
    }

    let disk_adc = read_text_optional(&paths.adc_file())?
        .or(read_text_optional(&paths.adc_disabled_file())?)
        .or(read_text_optional(&paths.agswitch_adc_file())?);
    if let Some(raw_adc) = disk_adc {
        let (cred_type, email, project_id, _) = parse_adc_metadata(&raw_adc)?;
        let digest = payload_digest(&raw_adc);
        let exists = vault
            .entries
            .iter()
            .any(|e| e.kind == AccountKind::Adc && payload_digest(&e.payload) == digest);
        if !exists && !vault.observed_revisions.iter().any(|r| r == &digest) {
            let fallback = project_id
                .as_deref()
                .or(email.as_deref())
                .unwrap_or("ADC 凭据");
            let name = clean_label(None, fallback)?;
            vault.entries.push(StoredEntry {
                id: Uuid::new_v4().to_string(),
                name,
                kind: AccountKind::Adc,
                source_type: cred_type.as_str().to_owned(),
                email,
                project_id,
                payload: raw_adc,
                updated_at: now_secs(),
            });
            remember_observed(vault, &digest);
            dirty = true;
        }
    }
    if dirty {
        save_vault(paths, vault)?;
    }
    Ok(dirty)
}

pub fn state_at(paths: &Paths) -> Result<AntigravityAuthState> {
    let (mut vault, mut vault_rev) = load_vault(paths)?;
    if sync_disk_credentials(paths, &mut vault)? {
        let (_, r) = load_vault(paths)?;
        vault_rev = r;
    }
    let (enable_adc, cfg_raw, _) = read_enable_adc(paths)?;
    let oauth_disk = read_text_optional(&paths.oauth_file())?;
    let adc_active = read_text_optional(&paths.adc_file())?;
    let adc_disabled = read_text_optional(&paths.adc_disabled_file())?;
    let adc_agswitch = read_text_optional(&paths.agswitch_adc_file())?;

    let (oauth_auth_method, oauth_email, oauth_digest) = match oauth_disk.as_deref() {
        Some(raw) => {
            let (m, e) = parse_oauth_metadata(raw)?;
            (Some(m), e, Some(payload_digest(raw)))
        }
        None => (None, None, None),
    };

    let adc_effective = adc_active
        .as_deref()
        .or(adc_disabled.as_deref())
        .or(adc_agswitch.as_deref());
    let (adc_type, adc_email, adc_project, adc_digest) = match adc_effective {
        Some(raw) => {
            let (t, e, p, _) = parse_adc_metadata(raw)?;
            (Some(t), e, p, Some(payload_digest(raw)))
        }
        None => (None, None, None, None),
    };

    let adc_status = if adc_active.is_some() {
        if enable_adc {
            AdcStatus::Active
        } else {
            AdcStatus::Disabled
        }
    } else if adc_disabled.is_some() || adc_agswitch.is_some() {
        AdcStatus::BackupOnly
    } else {
        AdcStatus::Missing
    };

    let mode = match (enable_adc, adc_active.is_some()) {
        (true, true) => AuthMode::Adc,
        (false, false) => AuthMode::Oauth,
        (false, true) => AuthMode::MixedAdcFileWithoutFlag,
        (true, false) => AuthMode::MixedFlagWithoutAdcFile,
    };

    let accounts: Vec<AntigravityAccountItem> = vault
        .entries
        .iter()
        .map(|e| {
            let cred_rev = payload_digest(&e.payload);
            let current = match e.kind {
                AccountKind::Oauth => {
                    oauth_digest.as_deref() == Some(cred_rev.as_str())
                        || match (&e.email, &oauth_email) {
                            (Some(ae), Some(de)) => ae.eq_ignore_ascii_case(de),
                            _ => false,
                        }
                }
                AccountKind::Adc => adc_digest.as_deref() == Some(cred_rev.as_str()),
            };
            AntigravityAccountItem {
                id: e.id.clone(),
                name: e.name.clone(),
                kind: e.kind,
                source_type: e.source_type.clone(),
                email: e.email.clone(),
                project_id: e.project_id.clone(),
                current,
                updated_at: e.updated_at,
                credential_revision: cred_rev,
            }
        })
        .collect();

    let rev_material = format!(
        "{vault_rev}:{}:{}:{:?}:{:?}",
        storage::revision(cfg_raw.as_deref()),
        oauth_digest.as_deref().unwrap_or("none"),
        adc_status,
        adc_digest
    );
    let revision = storage::digest(rev_material.as_bytes());

    Ok(AntigravityAuthState {
        accounts,
        mode,
        enable_adc,
        oauth_present: oauth_disk.is_some(),
        oauth_email,
        oauth_auth_method,
        adc_status,
        adc_type,
        adc_email,
        adc_project,
        app_restarted: false,
        revision,
    })
}

fn verify_revision(paths: &Paths, expected_revision: &str) -> Result<AntigravityAuthState> {
    let current = state_at(paths)?;
    if current.revision != expected_revision {
        return Err(AppError::new(
            code::CONFLICT,
            "Antigravity 凭据状态已变更，请刷新后重试",
        ));
    }
    Ok(current)
}

fn restore_file(path: &Path, prev: Option<&[u8]>) -> Result<()> {
    match prev {
        Some(bytes) => write_private_file(path, bytes),
        None => match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(storage::io_error(e)),
        },
    }
}

fn run_with_rollback_and_lifecycle<F>(
    paths: &Paths,
    expected_revision: &str,
    lifecycle: &dyn AppLifecycle,
    mutate: F,
) -> Result<AntigravityAuthState>
where
    F: FnOnce() -> Result<()>,
{
    verify_revision(paths, expected_revision)?;
    let tracked = [
        paths.oauth_file(),
        paths.adc_file(),
        paths.adc_disabled_file(),
        paths.agswitch_adc_file(),
        paths.config_file(),
    ];
    let mut snapshots = Vec::with_capacity(tracked.len());
    for file in &tracked {
        snapshots.push((file.clone(), storage::read_optional(file)?));
    }
    let was_running = lifecycle.is_running()?;
    if was_running {
        lifecycle.quit()?;
    }
    if let Err(err) = mutate() {
        let mut residuals = Vec::new();
        for (file, prev) in &snapshots {
            if restore_file(file, prev.as_deref()).is_err() {
                residuals.push(file.display().to_string());
            }
        }
        if was_running {
            let _ = lifecycle.launch();
        }
        if residuals.is_empty() {
            return Err(err);
        }
        return Err(AppError::new(
            &err.code,
            &format!(
                "{}（回滚残留未恢复：{}）",
                err.message,
                residuals.join(", ")
            ),
        ));
    }
    if was_running {
        lifecycle.launch()?;
    }
    let mut next = state_at(paths)?;
    next.app_restarted = was_running;
    Ok(next)
}

fn apply_mode_files(paths: &Paths, enable_adc: bool, vault: &Vault) -> Result<()> {
    let adc_path = paths.adc_file();
    let disabled_path = paths.adc_disabled_file();
    let agswitch_path = paths.agswitch_adc_file();
    if !enable_adc {
        if let Some(adc_bytes) = storage::read_optional(&adc_path)? {
            write_private_file(&disabled_path, &adc_bytes)?;
            fs::remove_file(&adc_path).map_err(storage::io_error)?;
        }
        write_enable_adc(paths, false)
    } else {
        if !adc_path.exists() {
            let source_bytes = storage::read_optional(&disabled_path)?
                .or(storage::read_optional(&agswitch_path)?)
                .or_else(|| {
                    vault
                        .entries
                        .iter()
                        .find(|e| e.kind == AccountKind::Adc)
                        .map(|e| e.payload.as_bytes().to_vec())
                })
                .ok_or_else(|| {
                    AppError::new(code::AUTH, "当前没有可用的 ADC 凭据，请先导入 ADC JSON")
                })?;
            let text = String::from_utf8(source_bytes.clone())
                .map_err(|_| AppError::new(code::UTF8, "ADC 凭据编码无效"))?;
            parse_adc_metadata(&text)?;
            write_private_file(&adc_path, &source_bytes)?;
            write_private_file(&agswitch_path, &source_bytes)?;
            if disabled_path.exists() {
                fs::remove_file(&disabled_path).map_err(storage::io_error)?;
            }
        }
        write_enable_adc(paths, true)
    }
}

pub fn add_adc_at(
    paths: &Paths,
    lifecycle: &dyn AppLifecycle,
    input: AddAntigravityAdcInput,
) -> Result<AntigravityAuthState> {
    let _guard = AUTH_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let text = input.json_text.trim();
    if text.is_empty() {
        return Err(AppError::new(
            code::JSON,
            "请粘贴或导入有效的 ADC JSON 内容",
        ));
    }
    let (cred_type, email, parsed_project, mut value) = parse_adc_metadata(text)?;
    let override_project = clean_opt(input.quota_project_id.as_deref());
    if let Some(ref qp) = override_project {
        if let Some(obj) = value.as_object_mut() {
            obj.insert("quota_project_id".into(), Value::String(qp.clone()));
        }
    }
    let project_id = override_project.or(parsed_project);
    let payload = format!(
        "{}\n",
        serde_json::to_string_pretty(&value)
            .map_err(|_| AppError::new(code::JSON, "无法格式化 ADC JSON"))?
    );
    let digest = payload_digest(&payload);
    let (mut vault, _) = load_vault(paths)?;
    let now = now_secs();
    let fallback = project_id
        .as_deref()
        .or(email.as_deref())
        .unwrap_or("ADC 凭据");
    let label = clean_label(input.name.as_deref(), fallback)?;
    let id = if let Some(existing) = vault
        .entries
        .iter_mut()
        .find(|e| e.kind == AccountKind::Adc && payload_digest(&e.payload) == digest)
    {
        if input.name.as_deref().is_some_and(|s| !s.trim().is_empty()) {
            existing.name = label;
        }
        existing.email = email;
        existing.project_id = project_id;
        existing.payload = payload;
        existing.updated_at = now;
        existing.id.clone()
    } else {
        let id = Uuid::new_v4().to_string();
        vault.entries.push(StoredEntry {
            id: id.clone(),
            name: label,
            kind: AccountKind::Adc,
            source_type: cred_type.as_str().to_owned(),
            email,
            project_id,
            payload,
            updated_at: now,
        });
        id
    };
    remember_observed(&mut vault, &digest);
    save_vault(paths, &vault)?;
    let state = state_at(paths)?;
    if input.activate {
        switch_account_unlocked(paths, lifecycle, &id, &state.revision)
    } else {
        Ok(state)
    }
}

fn switch_account_unlocked(
    paths: &Paths,
    lifecycle: &dyn AppLifecycle,
    id: &str,
    expected_revision: &str,
) -> Result<AntigravityAuthState> {
    let (vault, _) = load_vault(paths)?;
    let entry = vault
        .entries
        .iter()
        .find(|e| e.id == id)
        .cloned()
        .ok_or_else(|| AppError::new(code::ACCOUNT, "未找到指定的 Antigravity 凭据条目"))?;
    match entry.kind {
        AccountKind::Oauth => {
            parse_oauth_metadata(&entry.payload)?;
        }
        AccountKind::Adc => {
            parse_adc_metadata(&entry.payload)?;
        }
    }
    run_with_rollback_and_lifecycle(paths, expected_revision, lifecycle, || match entry.kind {
        AccountKind::Oauth => {
            write_private_file(&paths.oauth_file(), entry.payload.as_bytes())?;
            apply_mode_files(paths, false, &vault)
        }
        AccountKind::Adc => {
            write_private_file(&paths.adc_file(), entry.payload.as_bytes())?;
            write_private_file(&paths.agswitch_adc_file(), entry.payload.as_bytes())?;
            let disabled = paths.adc_disabled_file();
            if disabled.exists() {
                fs::remove_file(disabled).map_err(storage::io_error)?;
            }
            write_enable_adc(paths, true)
        }
    })
}

pub fn switch_account_at(
    paths: &Paths,
    lifecycle: &dyn AppLifecycle,
    id: &str,
    expected_revision: &str,
) -> Result<AntigravityAuthState> {
    let _guard = AUTH_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    switch_account_unlocked(paths, lifecycle, id, expected_revision)
}

pub fn set_mode_at(
    paths: &Paths,
    lifecycle: &dyn AppLifecycle,
    mode: TargetMode,
    expected_revision: &str,
) -> Result<AntigravityAuthState> {
    let _guard = AUTH_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (vault, _) = load_vault(paths)?;
    let enable_adc = matches!(mode, TargetMode::Adc);
    if !enable_adc && !paths.oauth_file().exists() {
        return Err(AppError::new(
            code::AUTH,
            "未检测到 Antigravity OAuth 登录令牌，请先在 Antigravity 内登录账号",
        ));
    }
    run_with_rollback_and_lifecycle(paths, expected_revision, lifecycle, || {
        apply_mode_files(paths, enable_adc, &vault)
    })
}

pub fn rename_account_at(
    paths: &Paths,
    id: &str,
    name: &str,
    expected_revision: &str,
) -> Result<AntigravityAuthState> {
    let _guard = AUTH_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    verify_revision(paths, expected_revision)?;
    let cleaned = clean_label(Some(name), "")?;
    let (mut vault, _) = load_vault(paths)?;
    let entry = vault
        .entries
        .iter_mut()
        .find(|e| e.id == id)
        .ok_or_else(|| AppError::new(code::ACCOUNT, "未找到指定的凭据条目"))?;
    entry.name = cleaned;
    entry.updated_at = now_secs();
    save_vault(paths, &vault)?;
    state_at(paths)
}

pub fn delete_account_at(
    paths: &Paths,
    id: &str,
    expected_revision: &str,
) -> Result<AntigravityAuthState> {
    let _guard = AUTH_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let current = verify_revision(paths, expected_revision)?;
    if current.accounts.iter().any(|a| a.id == id && a.current) {
        return Err(AppError::new(
            code::CONFLICT,
            "当前正在使用中的凭据不可直接删除，请先切换到其他凭据",
        ));
    }
    let (mut vault, _) = load_vault(paths)?;
    let idx = vault
        .entries
        .iter()
        .position(|e| e.id == id)
        .ok_or_else(|| AppError::new(code::ACCOUNT, "未找到指定的凭据条目"))?;
    let removed = vault.entries.remove(idx);
    remember_observed(&mut vault, &payload_digest(&removed.payload));
    save_vault(paths, &vault)?;
    state_at(paths)
}

pub fn disk_stamp() -> Option<String> {
    let user_home = dirs::home_dir()?;
    let files = [
        user_home.join(".gemini/jetski-standalone-oauth-token"),
        user_home.join(".gemini/config/config.json"),
        user_home.join(".config/gcloud/application_default_credentials.json"),
        user_home.join(".config/gcloud/application_default_credentials.json.disabled"),
        user_home.join(".config/agswitch/adc.json"),
    ];
    let mut stamp = String::new();
    for path in &files {
        if let Ok(meta) = fs::metadata(path) {
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            stamp.push_str(&format!("1:{}:{};", meta.len(), mtime));
        } else {
            stamp.push_str("0;");
        }
    }
    Some(stamp)
}

pub fn current_state(data_dir: &Path) -> Result<AntigravityAuthState> {
    let _guard = AUTH_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    state_at(&Paths::resolve(data_dir)?)
}

#[tauri::command]
pub fn get_antigravity_auth_state(r: R<'_>) -> Result<AntigravityAuthState> {
    current_state(&r.data)
}

#[tauri::command]
pub async fn add_antigravity_adc(
    r: R<'_>,
    input: AddAntigravityAdcInput,
) -> Result<AntigravityAuthState> {
    let data = r.data.clone();
    tauri::async_runtime::spawn_blocking(move || {
        add_adc_at(
            &Paths::resolve(&data)?,
            &SystemAppLifecycle::default(),
            input,
        )
    })
    .await
    .map_err(|_| AppError::new(code::PROCESS, "导入 ADC 任务异常中断"))?
}

#[tauri::command]
pub async fn pick_antigravity_credential_file(app: tauri::AppHandle) -> Result<Option<String>> {
    let picked = tauri::async_runtime::spawn_blocking(move || {
        app.dialog()
            .file()
            .add_filter("ADC JSON", &["json"])
            .blocking_pick_file()
    })
    .await
    .map_err(|_| AppError::new(code::DIALOG, "无法打开文件选择器"))?;
    let Some(file_path) = picked else {
        return Ok(None);
    };
    let path = file_path
        .into_path()
        .map_err(|_| AppError::new(code::PATH, "请选择本地文件"))?;
    let bytes = storage::read_bounded(&path, CREDENTIAL_LIMIT)?
        .ok_or_else(|| AppError::new(code::PATH, "所选文件不存在"))?;
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| AppError::new(code::UTF8, "所选文件不是 UTF-8 文本"))
}

#[tauri::command]
pub async fn switch_antigravity_account(
    r: R<'_>,
    id: String,
    expected_revision: String,
) -> Result<AntigravityAuthState> {
    let data = r.data.clone();
    tauri::async_runtime::spawn_blocking(move || {
        switch_account_at(
            &Paths::resolve(&data)?,
            &SystemAppLifecycle::default(),
            &id,
            &expected_revision,
        )
    })
    .await
    .map_err(|_| AppError::new(code::PROCESS, "切换凭据任务异常中断"))?
}

#[tauri::command]
pub async fn set_antigravity_auth_mode(
    r: R<'_>,
    mode: TargetMode,
    expected_revision: String,
) -> Result<AntigravityAuthState> {
    let data = r.data.clone();
    tauri::async_runtime::spawn_blocking(move || {
        set_mode_at(
            &Paths::resolve(&data)?,
            &SystemAppLifecycle::default(),
            mode,
            &expected_revision,
        )
    })
    .await
    .map_err(|_| AppError::new(code::PROCESS, "切换认证模式任务异常中断"))?
}

#[tauri::command]
pub fn rename_antigravity_account(
    r: R<'_>,
    id: String,
    name: String,
    expected_revision: String,
) -> Result<AntigravityAuthState> {
    rename_account_at(&Paths::resolve(&r.data)?, &id, &name, &expected_revision)
}

#[tauri::command]
pub fn delete_antigravity_account(
    r: R<'_>,
    id: String,
    expected_revision: String,
) -> Result<AntigravityAuthState> {
    delete_account_at(&Paths::resolve(&r.data)?, &id, &expected_revision)
}

#[cfg(test)]
#[path = "antigravity_auth_tests.rs"]
mod tests;
