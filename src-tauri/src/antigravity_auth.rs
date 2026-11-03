use crate::{
    errors::code,
    storage::{self, AppError, Result},
    Runtime,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tauri_plugin_dialog::DialogExt;

type R<'a> = tauri::State<'a, Arc<Runtime>>;

const VAULT_LIMIT: u64 = 4 * 1024 * 1024;
const CREDENTIAL_LIMIT: u64 = 256 * 1024;
const MAX_BACKUPS: usize = 10;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AccountKind {
    Oauth,
    Adc,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AuthMode {
    Oauth,
    Adc,
    Mixed,
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
pub enum AdcCredentialType {
    ServiceAccount,
    AuthorizedUser,
}

impl AdcCredentialType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ServiceAccount => "service_account",
            Self::AuthorizedUser => "authorized_user",
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
        Ok(Self {
            gemini_home: user_home.join(".gemini"),
            gcloud_dir: user_home.join(".config/gcloud"),
            agswitch_dir: user_home.join(".config/agswitch"),
            data_dir: data_dir.to_path_buf(),
        })
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
    pub fn backups_dir(&self) -> PathBuf {
        self.data_dir.join("antigravity-backups")
    }
}

pub trait AppLifecycle {
    fn stop_if_running(&self) -> Result<bool>;
    fn relaunch(&self) -> Result<()>;
}

#[cfg(test)]
pub struct NoopLifecycle;

#[cfg(test)]
impl AppLifecycle for NoopLifecycle {
    fn stop_if_running(&self) -> Result<bool> {
        Ok(false)
    }
    fn relaunch(&self) -> Result<()> {
        Ok(())
    }
}

pub struct SystemAppLifecycle;

impl SystemAppLifecycle {
    #[cfg(target_os = "macos")]
    fn is_antigravity_running() -> bool {
        use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
        let mut sys = System::new();
        sys.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing().with_exe(UpdateKind::OnlyIfNotSet),
        );
        sys.processes().values().any(|p| {
            p.exe().is_some_and(|exe| {
                exe == Path::new("/Applications/Antigravity.app/Contents/MacOS/Antigravity")
            })
        })
    }
}

impl AppLifecycle for SystemAppLifecycle {
    fn stop_if_running(&self) -> Result<bool> {
        #[cfg(target_os = "macos")]
        {
            if !Self::is_antigravity_running() {
                return Ok(false);
            }
            let _ = std::process::Command::new("/usr/bin/osascript")
                .args([
                    "-e",
                    "tell application id \"com.google.antigravity\" to quit",
                ])
                .status();
            let graceful_deadline = Instant::now() + Duration::from_secs(3);
            while Instant::now() < graceful_deadline {
                if !Self::is_antigravity_running() {
                    return Ok(true);
                }
                std::thread::sleep(Duration::from_millis(120));
            }
            let _ = std::process::Command::new("/usr/bin/pkill")
                .args([
                    "-TERM",
                    "-f",
                    "/Applications/Antigravity.app/Contents/MacOS/Antigravity",
                ])
                .status();
            let term_deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < term_deadline {
                if !Self::is_antigravity_running() {
                    return Ok(true);
                }
                std::thread::sleep(Duration::from_millis(120));
            }
            Err(AppError::new(
                code::PROCESS,
                "等待 Antigravity.app 退出超时，未修改任何凭据文件，请先手动退出 Antigravity 后重试",
            ))
        }
        #[cfg(not(target_os = "macos"))]
        {
            Ok(false)
        }
    }

    fn relaunch(&self) -> Result<()> {
        #[cfg(target_os = "macos")]
        {
            let app_path = Path::new("/Applications/Antigravity.app");
            if !app_path.exists() {
                return Ok(());
            }
            let status = std::process::Command::new("/usr/bin/open")
                .arg("-a")
                .arg(app_path)
                .status()
                .map_err(storage::io_error)?;
            if !status.success() {
                return Err(AppError::new(
                    code::PROCESS,
                    "凭据已切换，但自动重新启动 Antigravity.app 失败，请手动打开应用",
                ));
            }
            Ok(())
        }
        #[cfg(not(target_os = "macos"))]
        {
            Ok(())
        }
    }
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
}

impl Default for Vault {
    fn default() -> Self {
        Self {
            schema: 1,
            entries: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
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

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AntigravityAuthState {
    pub accounts: Vec<AntigravityAccountItem>,
    pub mode: AuthMode,
    pub enable_adc: bool,
    pub oauth_present: bool,
    pub oauth_saved: bool,
    pub oauth_email: Option<String>,
    pub oauth_auth_method: Option<String>,
    pub adc_status: AdcStatus,
    pub adc_saved: bool,
    pub adc_type: Option<AdcCredentialType>,
    pub adc_email: Option<String>,
    pub adc_project: Option<String>,
    pub backup_count: usize,
    pub app_restarted: bool,
    pub revision: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddAdcInput {
    pub name: Option<String>,
    pub json_text: String,
    pub quota_project_id: Option<String>,
    pub activate: bool,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn clean_opt(raw: Option<&str>) -> Option<String> {
    raw.map(str::trim)
        .filter(|s| !s.is_empty() && !s.chars().any(char::is_control))
        .map(str::to_owned)
}

fn map_str(map: &serde_json::Map<String, Value>, keys: &[&str]) -> Option<String> {
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

fn ensure_parent_dir(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(storage::io_error)?;
    }
    Ok(())
}
fn load_vault(paths: &Paths) -> Result<(Vault, String)> {
    storage::private_dir(&paths.data_dir)?;
    let path = paths.vault_file();
    let loaded = storage::read_bounded(&path, VAULT_LIMIT)?;
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

fn save_vault(paths: &Paths, vault: &Vault, expected: Option<&str>) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(vault)
        .map_err(|_| AppError::new(code::STORE, "无法序列化 Antigravity 凭据库"))?;
    storage::atomic_write_bounded(&paths.vault_file(), &bytes, expected, VAULT_LIMIT)
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
        &[
            "id_token_email",
            "id_token",
            "access_token",
            "refresh_token",
        ],
    )
    .is_some()
        || nested_token
            .is_some_and(|t| map_str(t, &["access_token", "refresh_token", "id_token"]).is_some());
    if !has_token {
        return Err(AppError::new(
            code::AUTH,
            "OAuth 令牌缺少有效字段（id_token_email / token.access_token / refresh_token）",
        ));
    }
    let email = map_str(
        map,
        &["id_token_email", "email", "account", "client_email", "user"],
    )
    .or_else(|| nested_token.and_then(|t| map_str(t, &["email", "id_token_email"])))
    .or_else(|| {
        map_str(map, &["id_token"])
            .or_else(|| nested_token.and_then(|t| map_str(t, &["id_token"])))
            .and_then(|jwt| decode_jwt_email(&jwt))
    });
    let auth_method = map_str(map, &["auth_method"])
        .or_else(|| nested_token.and_then(|t| map_str(t, &["token_type"])))
        .unwrap_or_else(|| "oauth".to_owned());
    Ok((auth_method, email))
}

pub fn parse_adc_metadata(
    raw: &str,
) -> Result<(AdcCredentialType, Option<String>, Option<String>, Value)> {
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
            if map_str(map, &["token_uri"]).is_none() {
                return Err(AppError::new(
                    code::AUTH,
                    "service_account ADC 缺少 token_uri 字段",
                ));
            }
            AdcCredentialType::ServiceAccount
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
            AdcCredentialType::AuthorizedUser
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

fn read_enable_adc(paths: &Paths) -> Result<(bool, Option<Vec<u8>>, Value)> {
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

fn write_enable_adc(paths: &Paths, enable: bool) -> Result<()> {
    let path = paths.config_file();
    ensure_parent_dir(&path)?;
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
    storage::atomic_write(&path, formatted.as_bytes(), Some(&expected))
}

fn backup_dirs(backups_root: &Path) -> Result<Vec<PathBuf>> {
    if !backups_root.exists() {
        return Ok(Vec::new());
    }
    let mut dirs = Vec::new();
    for entry in std::fs::read_dir(backups_root).map_err(storage::io_error)? {
        let entry = entry.map_err(storage::io_error)?;
        let path = entry.path();
        if entry.file_type().map_err(storage::io_error)?.is_dir()
            && path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("backup-"))
        {
            dirs.push(path);
        }
    }
    dirs.sort();
    Ok(dirs)
}

fn count_backups(backups_root: &Path) -> Result<usize> {
    Ok(backup_dirs(backups_root)?.len())
}

fn prune_backups(backups_root: &Path) -> Result<usize> {
    let mut dirs = backup_dirs(backups_root)?;
    while dirs.len() > MAX_BACKUPS {
        let oldest = dirs.remove(0);
        for item in std::fs::read_dir(&oldest).map_err(storage::io_error)? {
            let item = item.map_err(storage::io_error)?;
            if item.file_type().map_err(storage::io_error)?.is_file() {
                std::fs::remove_file(item.path()).map_err(storage::io_error)?;
            }
        }
        std::fs::remove_dir(&oldest).map_err(storage::io_error)?;
    }
    Ok(dirs.len())
}

pub fn create_backup_at(paths: &Paths) -> Result<usize> {
    let backups_root = paths.backups_dir();
    storage::private_dir(&backups_root)?;
    let snap_dir = backups_root.join(format!("backup-{}", now_millis()));
    storage::private_dir(&snap_dir)?;
    for (src, name) in [
        (paths.config_file(), "config.json"),
        (paths.oauth_file(), "jetski-standalone-oauth-token"),
        (paths.adc_file(), "application_default_credentials.json"),
        (
            paths.adc_disabled_file(),
            "application_default_credentials.json.disabled",
        ),
        (paths.agswitch_adc_file(), "agswitch_adc.json"),
    ] {
        if let Some(bytes) = storage::read_bounded(&src, CREDENTIAL_LIMIT)? {
            storage::atomic_write(&snap_dir.join(name), &bytes, None)?;
        }
    }
    prune_backups(&backups_root)
}
pub fn state_at(paths: &Paths) -> Result<AntigravityAuthState> {
    let (vault, vault_rev) = load_vault(paths)?;
    let (enable_adc, cfg_raw, _) = read_enable_adc(paths)?;
    let oauth_disk = read_text_optional(&paths.oauth_file())?;
    let adc_active = read_text_optional(&paths.adc_file())?;
    let adc_disabled = read_text_optional(&paths.adc_disabled_file())?;
    let adc_agswitch = read_text_optional(&paths.agswitch_adc_file())?;

    let oauth_digest = oauth_disk.as_deref().map(payload_digest);
    let adc_effective = adc_active
        .as_deref()
        .or(adc_disabled.as_deref())
        .or(adc_agswitch.as_deref());
    let adc_digest = adc_effective.map(payload_digest);

    let adc_status = if adc_active.is_some() {
        AdcStatus::Active
    } else if adc_disabled.is_some() {
        AdcStatus::Disabled
    } else if adc_agswitch.is_some() {
        AdcStatus::BackupOnly
    } else {
        AdcStatus::Missing
    };

    let (oauth_auth_method, oauth_email) = oauth_disk
        .as_deref()
        .and_then(|raw| parse_oauth_metadata(raw).ok())
        .map(|(m, e)| (Some(m), e))
        .unwrap_or((None, None));

    let (adc_type, adc_email, adc_project) = adc_effective
        .and_then(|raw| parse_adc_metadata(raw).ok())
        .map(|(t, e, p, _)| (Some(t), e, p))
        .unwrap_or((None, None, None));

    let accounts: Vec<AntigravityAccountItem> = vault
        .entries
        .iter()
        .map(|e| {
            let cred_rev = payload_digest(&e.payload);
            let current = match e.kind {
                AccountKind::Oauth => oauth_digest.as_deref() == Some(cred_rev.as_str()),
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

    let oauth_saved = oauth_digest.as_ref().is_some_and(|d| {
        accounts
            .iter()
            .any(|a| a.kind == AccountKind::Oauth && &a.credential_revision == d)
    });
    let adc_saved = adc_digest.as_ref().is_some_and(|d| {
        accounts
            .iter()
            .any(|a| a.kind == AccountKind::Adc && &a.credential_revision == d)
    });

    let adc_file_exists = adc_active.is_some();
    let mode = if enable_adc != adc_file_exists {
        AuthMode::Mixed
    } else if enable_adc && adc_file_exists {
        AuthMode::Adc
    } else {
        AuthMode::Oauth
    };

    let backup_count = count_backups(&paths.backups_dir())?;
    let rev_material = format!(
        "{vault_rev}:{}:{}:{}:{}:{}",
        storage::revision(cfg_raw.as_deref()),
        oauth_digest.as_deref().unwrap_or("none"),
        adc_active
            .as_deref()
            .map(payload_digest)
            .as_deref()
            .unwrap_or("none"),
        adc_disabled
            .as_deref()
            .map(payload_digest)
            .as_deref()
            .unwrap_or("none"),
        adc_agswitch
            .as_deref()
            .map(payload_digest)
            .as_deref()
            .unwrap_or("none"),
    );

    Ok(AntigravityAuthState {
        accounts,
        mode,
        enable_adc,
        oauth_present: oauth_disk.is_some(),
        oauth_saved,
        oauth_email,
        oauth_auth_method,
        adc_status,
        adc_saved,
        adc_type,
        adc_email,
        adc_project,
        backup_count,
        app_restarted: false,
        revision: storage::digest(rev_material.as_bytes()),
    })
}

fn verify_revision(paths: &Paths, expected_revision: &str) -> Result<AntigravityAuthState> {
    let current = state_at(paths)?;
    if current.revision != expected_revision {
        return Err(AppError::new(
            code::CONFLICT,
            "Antigravity 凭据状态已变化，已刷新列表，请重试",
        ));
    }
    Ok(current)
}

fn restore_file(path: &Path, prev: Option<&[u8]>) -> Result<()> {
    match prev {
        Some(bytes) => {
            ensure_parent_dir(path)?;
            storage::atomic_write(path, bytes, None)
        }
        None => match std::fs::remove_file(path) {
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
    create_backup_at(paths)?;
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
    let was_running = lifecycle.stop_if_running()?;
    if let Err(err) = mutate() {
        let mut residuals = Vec::new();
        for (file, prev) in &snapshots {
            if restore_file(file, prev.as_deref()).is_err() {
                residuals.push(file.display().to_string());
            }
        }
        if was_running {
            let _ = lifecycle.relaunch();
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
        lifecycle.relaunch()?;
    }
    let mut next = state_at(paths)?;
    next.app_restarted = was_running;
    Ok(next)
}

fn upsert_entry(
    paths: &Paths,
    kind: AccountKind,
    name: Option<&str>,
    source_type: String,
    email: Option<String>,
    project_id: Option<String>,
    payload: String,
) -> Result<String> {
    let (mut vault, vault_rev) = load_vault(paths)?;
    let digest = payload_digest(&payload);
    let now = now_secs();
    if let Some(existing) = vault
        .entries
        .iter_mut()
        .find(|e| e.kind == kind && payload_digest(&e.payload) == digest)
    {
        if name.is_some_and(|s| !s.trim().is_empty()) {
            existing.name = clean_label(name, &existing.name)?;
        }
        if email.is_some() {
            existing.email = email;
        }
        if project_id.is_some() {
            existing.project_id = project_id;
        }
        existing.source_type = source_type;
        existing.payload = payload;
        existing.updated_at = now;
        let id = existing.id.clone();
        save_vault(paths, &vault, Some(&vault_rev))?;
        return Ok(id);
    }
    let fallback = match kind {
        AccountKind::Oauth => email.as_deref().unwrap_or("OAuth 账号"),
        AccountKind::Adc => project_id
            .as_deref()
            .or(email.as_deref())
            .unwrap_or("ADC 凭据"),
    };
    let label = clean_label(name, fallback)?;
    let id = uuid::Uuid::new_v4().to_string();
    vault.entries.push(StoredEntry {
        id: id.clone(),
        name: label,
        kind,
        source_type,
        email,
        project_id,
        payload,
        updated_at: now,
    });
    save_vault(paths, &vault, Some(&vault_rev))?;
    Ok(id)
}

pub fn import_current_oauth_at(
    paths: &Paths,
    name: Option<String>,
) -> Result<AntigravityAuthState> {
    let raw = read_text_optional(&paths.oauth_file())?.ok_or_else(|| {
        AppError::new(
            code::AUTH,
            "未检测到当前 OAuth 登录令牌（~/.gemini/jetski-standalone-oauth-token），请先在 Antigravity 中完成登录",
        )
    })?;
    let (auth_method, email) = parse_oauth_metadata(&raw)?;
    upsert_entry(
        paths,
        AccountKind::Oauth,
        name.as_deref(),
        auth_method,
        email,
        None,
        raw,
    )?;
    state_at(paths)
}

pub fn import_current_adc_at(paths: &Paths, name: Option<String>) -> Result<AntigravityAuthState> {
    let raw = read_text_optional(&paths.adc_file())?
        .or(read_text_optional(&paths.adc_disabled_file())?)
        .or(read_text_optional(&paths.agswitch_adc_file())?)
        .ok_or_else(|| AppError::new(code::AUTH, "未检测到当前磁盘上的 ADC 凭据文件"))?;
    let (cred_type, email, project_id, value) = parse_adc_metadata(&raw)?;
    let normalized = format!(
        "{}\n",
        serde_json::to_string_pretty(&value)
            .map_err(|_| AppError::new(code::JSON, "无法格式化 ADC JSON"))?
    );
    upsert_entry(
        paths,
        AccountKind::Adc,
        name.as_deref(),
        cred_type.as_str().to_owned(),
        email,
        project_id,
        normalized,
    )?;
    state_at(paths)
}

pub fn add_adc_at_with_lifecycle(
    paths: &Paths,
    input: AddAdcInput,
    lifecycle: &dyn AppLifecycle,
) -> Result<AntigravityAuthState> {
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
    let id = upsert_entry(
        paths,
        AccountKind::Adc,
        input.name.as_deref(),
        cred_type.as_str().to_owned(),
        email,
        project_id,
        payload,
    )?;
    let state = state_at(paths)?;
    if input.activate {
        switch_adc_at_with_lifecycle(paths, &id, &state.revision, true, lifecycle)
    } else {
        Ok(state)
    }
}

#[cfg(test)]
pub fn add_adc_at(paths: &Paths, input: AddAdcInput) -> Result<AntigravityAuthState> {
    add_adc_at_with_lifecycle(paths, input, &NoopLifecycle)
}

pub fn switch_oauth_at_with_lifecycle(
    paths: &Paths,
    id: &str,
    expected_revision: &str,
    disable_adc: bool,
    lifecycle: &dyn AppLifecycle,
) -> Result<AntigravityAuthState> {
    let (vault, _) = load_vault(paths)?;
    let entry = vault
        .entries
        .iter()
        .find(|e| e.id == id && e.kind == AccountKind::Oauth)
        .cloned()
        .ok_or_else(|| AppError::new(code::ACCOUNT, "OAuth 账号不存在"))?;
    run_with_rollback_and_lifecycle(paths, expected_revision, lifecycle, || {
        let oauth_path = paths.oauth_file();
        ensure_parent_dir(&oauth_path)?;
        storage::atomic_write(&oauth_path, entry.payload.as_bytes(), None)?;
        if disable_adc {
            let adc_path = paths.adc_file();
            if let Some(adc_bytes) = storage::read_optional(&adc_path)? {
                let disabled_path = paths.adc_disabled_file();
                ensure_parent_dir(&disabled_path)?;
                storage::atomic_write(&disabled_path, &adc_bytes, None)?;
                std::fs::remove_file(adc_path).map_err(storage::io_error)?;
            }
            write_enable_adc(paths, false)?;
        }
        Ok(())
    })
}

#[cfg(test)]
pub fn switch_oauth_at(
    paths: &Paths,
    id: &str,
    expected_revision: &str,
    disable_adc: bool,
) -> Result<AntigravityAuthState> {
    switch_oauth_at_with_lifecycle(paths, id, expected_revision, disable_adc, &NoopLifecycle)
}

pub fn switch_adc_at_with_lifecycle(
    paths: &Paths,
    id: &str,
    expected_revision: &str,
    enable_adc_in_config: bool,
    lifecycle: &dyn AppLifecycle,
) -> Result<AntigravityAuthState> {
    let (vault, _) = load_vault(paths)?;
    let entry = vault
        .entries
        .iter()
        .find(|e| e.id == id && e.kind == AccountKind::Adc)
        .cloned()
        .ok_or_else(|| AppError::new(code::ACCOUNT, "ADC 凭据不存在"))?;
    run_with_rollback_and_lifecycle(paths, expected_revision, lifecycle, || {
        let adc_path = paths.adc_file();
        let agswitch_path = paths.agswitch_adc_file();
        ensure_parent_dir(&adc_path)?;
        ensure_parent_dir(&agswitch_path)?;
        storage::atomic_write(&adc_path, entry.payload.as_bytes(), None)?;
        storage::atomic_write(&agswitch_path, entry.payload.as_bytes(), None)?;
        let disabled_path = paths.adc_disabled_file();
        if disabled_path.exists() {
            std::fs::remove_file(disabled_path).map_err(storage::io_error)?;
        }
        if enable_adc_in_config {
            write_enable_adc(paths, true)?;
        }
        Ok(())
    })
}

#[cfg(test)]
pub fn switch_adc_at(
    paths: &Paths,
    id: &str,
    expected_revision: &str,
    enable_adc_in_config: bool,
) -> Result<AntigravityAuthState> {
    switch_adc_at_with_lifecycle(
        paths,
        id,
        expected_revision,
        enable_adc_in_config,
        &NoopLifecycle,
    )
}

pub fn set_mode_at_with_lifecycle(
    paths: &Paths,
    mode: AuthMode,
    expected_revision: &str,
    lifecycle: &dyn AppLifecycle,
) -> Result<AntigravityAuthState> {
    if mode == AuthMode::Mixed {
        return Err(AppError::new(
            code::CONFIG_TYPE,
            "混合模式为配置与凭据文件不一致的告警状态，不可主动设置为目标模式",
        ));
    }
    run_with_rollback_and_lifecycle(paths, expected_revision, lifecycle, || {
        let adc_path = paths.adc_file();
        let disabled_path = paths.adc_disabled_file();
        let agswitch_path = paths.agswitch_adc_file();
        if mode == AuthMode::Oauth {
            if let Some(adc_bytes) = storage::read_optional(&adc_path)? {
                ensure_parent_dir(&disabled_path)?;
                storage::atomic_write(&disabled_path, &adc_bytes, None)?;
                std::fs::remove_file(adc_path).map_err(storage::io_error)?;
            }
            write_enable_adc(paths, false)?;
        } else {
            if !adc_path.exists() {
                let source_bytes = storage::read_optional(&disabled_path)?
                    .or(storage::read_optional(&agswitch_path)?)
                    .ok_or_else(|| {
                        AppError::new(
                            code::AUTH,
                            "未找到可恢复的本地 ADC 凭据，请在列表中选择目标 ADC 凭据进行切换",
                        )
                    })?;
                ensure_parent_dir(&adc_path)?;
                ensure_parent_dir(&agswitch_path)?;
                storage::atomic_write(&adc_path, &source_bytes, None)?;
                storage::atomic_write(&agswitch_path, &source_bytes, None)?;
                if disabled_path.exists() {
                    std::fs::remove_file(disabled_path).map_err(storage::io_error)?;
                }
            }
            write_enable_adc(paths, true)?;
        }
        Ok(())
    })
}

#[cfg(test)]
pub fn set_mode_at(
    paths: &Paths,
    mode: AuthMode,
    expected_revision: &str,
) -> Result<AntigravityAuthState> {
    set_mode_at_with_lifecycle(paths, mode, expected_revision, &NoopLifecycle)
}

pub fn rename_at(
    paths: &Paths,
    id: &str,
    name: &str,
    expected_revision: &str,
) -> Result<AntigravityAuthState> {
    verify_revision(paths, expected_revision)?;
    let cleaned = clean_label(Some(name), "")?;
    let (mut vault, vault_rev) = load_vault(paths)?;
    let entry = vault
        .entries
        .iter_mut()
        .find(|e| e.id == id)
        .ok_or_else(|| AppError::new(code::ACCOUNT, "凭据不存在"))?;
    entry.name = cleaned;
    entry.updated_at = now_secs();
    save_vault(paths, &vault, Some(&vault_rev))?;
    state_at(paths)
}

pub fn delete_at(paths: &Paths, id: &str, expected_revision: &str) -> Result<AntigravityAuthState> {
    verify_revision(paths, expected_revision)?;
    let (mut vault, vault_rev) = load_vault(paths)?;
    let idx = vault
        .entries
        .iter()
        .position(|e| e.id == id)
        .ok_or_else(|| AppError::new(code::ACCOUNT, "凭据不存在或已被删除"))?;
    let target = &vault.entries[idx];
    let target_digest = payload_digest(&target.payload);

    let live_oauth = read_text_optional(&paths.oauth_file())?.map(|s| payload_digest(&s));
    let live_adc = read_text_optional(&paths.adc_file())?.map(|s| payload_digest(&s));
    let in_use = match target.kind {
        AccountKind::Oauth => live_oauth.as_deref() == Some(target_digest.as_str()),
        AccountKind::Adc => live_adc.as_deref() == Some(target_digest.as_str()),
    };
    if in_use {
        return Err(AppError::new(
            code::CONFLICT,
            "该凭据当前正在使用中，请先切换到其他凭据或停用后再删除",
        ));
    }
    vault.entries.remove(idx);
    save_vault(paths, &vault, Some(&vault_rev))?;
    state_at(paths)
}

#[tauri::command]
pub fn get_antigravity_auth_state(r: R<'_>) -> Result<AntigravityAuthState> {
    state_at(&Paths::resolve(&r.data)?)
}

#[tauri::command]
pub fn import_current_antigravity_oauth(
    r: R<'_>,
    name: Option<String>,
) -> Result<AntigravityAuthState> {
    import_current_oauth_at(&Paths::resolve(&r.data)?, name)
}

#[tauri::command]
pub fn import_current_antigravity_adc(
    r: R<'_>,
    name: Option<String>,
) -> Result<AntigravityAuthState> {
    import_current_adc_at(&Paths::resolve(&r.data)?, name)
}

#[tauri::command]
pub async fn add_antigravity_adc(r: R<'_>, input: AddAdcInput) -> Result<AntigravityAuthState> {
    let data = r.data.clone();
    tauri::async_runtime::spawn_blocking(move || {
        add_adc_at_with_lifecycle(&Paths::resolve(&data)?, input, &SystemAppLifecycle)
    })
    .await
    .map_err(|_| AppError::new(code::PROCESS, "添加 ADC 凭据任务异常中断"))?
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
    let text = String::from_utf8(bytes)
        .map_err(|_| AppError::new(code::UTF8, "所选文件不是 UTF-8 文本"))?;
    Ok(Some(text))
}

#[tauri::command]
pub async fn switch_antigravity_oauth(
    r: R<'_>,
    id: String,
    expected_revision: String,
    disable_adc: bool,
) -> Result<AntigravityAuthState> {
    let data = r.data.clone();
    tauri::async_runtime::spawn_blocking(move || {
        switch_oauth_at_with_lifecycle(
            &Paths::resolve(&data)?,
            &id,
            &expected_revision,
            disable_adc,
            &SystemAppLifecycle,
        )
    })
    .await
    .map_err(|_| AppError::new(code::PROCESS, "切换 OAuth 账号任务异常中断"))?
}

#[tauri::command]
pub async fn switch_antigravity_adc(
    r: R<'_>,
    id: String,
    expected_revision: String,
    enable_adc: bool,
) -> Result<AntigravityAuthState> {
    let data = r.data.clone();
    tauri::async_runtime::spawn_blocking(move || {
        switch_adc_at_with_lifecycle(
            &Paths::resolve(&data)?,
            &id,
            &expected_revision,
            enable_adc,
            &SystemAppLifecycle,
        )
    })
    .await
    .map_err(|_| AppError::new(code::PROCESS, "切换 ADC 凭据任务异常中断"))?
}

#[tauri::command]
pub async fn set_antigravity_auth_mode(
    r: R<'_>,
    mode: AuthMode,
    expected_revision: String,
) -> Result<AntigravityAuthState> {
    let data = r.data.clone();
    tauri::async_runtime::spawn_blocking(move || {
        set_mode_at_with_lifecycle(
            &Paths::resolve(&data)?,
            mode,
            &expected_revision,
            &SystemAppLifecycle,
        )
    })
    .await
    .map_err(|_| AppError::new(code::PROCESS, "切换 Antigravity 模式任务异常中断"))?
}

#[tauri::command]
pub fn backup_antigravity_auth(r: R<'_>) -> Result<AntigravityAuthState> {
    let paths = Paths::resolve(&r.data)?;
    create_backup_at(&paths)?;
    state_at(&paths)
}

#[tauri::command]
pub fn rename_antigravity_account(
    r: R<'_>,
    id: String,
    name: String,
    expected_revision: String,
) -> Result<AntigravityAuthState> {
    rename_at(&Paths::resolve(&r.data)?, &id, &name, &expected_revision)
}

#[tauri::command]
pub fn delete_antigravity_account(
    r: R<'_>,
    id: String,
    expected_revision: String,
) -> Result<AntigravityAuthState> {
    delete_at(&Paths::resolve(&r.data)?, &id, &expected_revision)
}

#[cfg(test)]
#[path = "antigravity_auth_tests.rs"]
mod tests;
