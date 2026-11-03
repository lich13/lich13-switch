use crate::{errors::code, storage::{self, AppError, Result}, Runtime};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::{fs, path::{Path, PathBuf}, sync::{Arc, LazyLock, Mutex}, time::{Duration, SystemTime, UNIX_EPOCH}};
use tauri_plugin_dialog::DialogExt;
use uuid::Uuid;

type R<'a> = tauri::State<'a, Arc<Runtime>>;
const VAULT_LIMIT: u64 = 4 * 1024 * 1024;
const CREDENTIAL_LIMIT: u64 = 256 * 1024;
static AUTH_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)] #[serde(rename_all = "snake_case")]
pub enum AccountKind { Oauth, Adc }
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)] #[serde(rename_all = "snake_case")]
pub enum AuthMode { Oauth, Adc, MixedAdcFileWithoutFlag, MixedFlagWithoutAdcFile }
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)] #[serde(rename_all = "snake_case")]
pub enum TargetMode { Oauth, Adc }
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)] #[serde(rename_all = "snake_case")]
pub enum AdcStatus { Active, Disabled, BackupOnly, Missing }
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)] #[serde(rename_all = "snake_case")]
pub enum AdcType { ServiceAccount, AuthorizedUser }
impl AdcType { pub fn as_str(self) -> &'static str { match self { Self::ServiceAccount => "service_account", Self::AuthorizedUser => "authorized_user" } } }
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)] #[serde(rename_all = "camelCase")]
pub struct AntigravityAccountItem {
    pub id: String, pub name: String, pub kind: AccountKind, pub source_type: String,
    pub email: Option<String>, pub project_id: Option<String>, pub current: bool, pub updated_at: u64, pub credential_revision: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)] #[serde(rename_all = "camelCase")]
pub struct AntigravityAuthState {
    pub accounts: Vec<AntigravityAccountItem>, pub mode: AuthMode, pub enable_adc: bool, pub oauth_present: bool,
    pub oauth_email: Option<String>, pub oauth_auth_method: Option<String>, pub adc_status: AdcStatus, pub adc_type: Option<AdcType>,
    pub adc_email: Option<String>, pub adc_project: Option<String>, #[serde(default)] pub app_restarted: bool, pub revision: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)] #[serde(rename_all = "camelCase")]
pub struct AddAntigravityAdcInput { pub name: Option<String>, pub json_text: String, pub quota_project_id: Option<String>, #[serde(default)] pub activate: bool }
#[derive(Clone, Debug, Serialize, Deserialize)] #[serde(rename_all = "camelCase")]
struct StoredEntry { id: String, name: String, kind: AccountKind, source_type: String, email: Option<String>, project_id: Option<String>, payload: String, updated_at: u64 }
#[derive(Clone, Debug, Serialize, Deserialize)] #[serde(rename_all = "camelCase")]
struct Vault { schema: u32, entries: Vec<StoredEntry>, #[serde(default)] observed_revisions: Vec<String> }
impl Default for Vault { fn default() -> Self { Self { schema: 1, entries: Vec::new(), observed_revisions: Vec::new() } } }
#[derive(Clone, Debug)]
pub struct Paths { pub gemini_home: PathBuf, pub gcloud_dir: PathBuf, pub agswitch_dir: PathBuf, pub data_dir: PathBuf }
impl Paths {
    pub fn resolve(data_dir: &Path) -> Result<Self> { Ok(Self::from_roots(&dirs::home_dir().ok_or_else(|| AppError::new(code::PATH, "无法定位用户主目录"))?, data_dir)) }
    pub fn from_roots(user_home: &Path, data_dir: &Path) -> Self { Self { gemini_home: user_home.join(".gemini"), gcloud_dir: user_home.join(".config/gcloud"), agswitch_dir: user_home.join(".config/agswitch"), data_dir: data_dir.to_path_buf() } }
    pub fn config_file(&self) -> PathBuf { self.gemini_home.join("config/config.json") }
    pub fn oauth_file(&self) -> PathBuf { self.gemini_home.join("jetski-standalone-oauth-token") }
    pub fn adc_file(&self) -> PathBuf { self.gcloud_dir.join("application_default_credentials.json") }
    pub fn adc_disabled_file(&self) -> PathBuf { self.gcloud_dir.join("application_default_credentials.json.disabled") }
    pub fn agswitch_adc_file(&self) -> PathBuf { self.agswitch_dir.join("adc.json") }
    pub fn vault_file(&self) -> PathBuf { self.data_dir.join("antigravity-accounts.json") }
    pub fn watched_files(&self) -> [PathBuf; 5] { [self.oauth_file(), self.config_file(), self.adc_file(), self.adc_disabled_file(), self.agswitch_adc_file()] }
}
pub trait AppLifecycle { fn is_running(&self) -> Result<bool>; fn quit(&self) -> Result<()>; fn launch(&self) -> Result<()>; }
#[derive(Default)] pub struct SystemAppLifecycle { #[cfg(target_os = "macos")] last_bundle: Mutex<Option<PathBuf>> }
#[cfg(target_os = "macos")]
pub(crate) fn parse_running_antigravity_ps(ps_output: &str) -> Vec<(i32, PathBuf)> {
    ps_output.lines().filter_map(|line| {
        let (pid_str, comm) = line.trim().split_once(char::is_whitespace)?;
        Some((pid_str.trim().parse::<i32>().ok()?, PathBuf::from(comm.trim().strip_suffix("/Contents/MacOS/Antigravity")?)))
    }).collect()
}
#[cfg(target_os = "macos")]
impl SystemAppLifecycle {
    fn detect_running(&self) -> Result<Vec<(i32, PathBuf)>> {
        let out = std::process::Command::new("ps").args(["-axo", "pid=,comm="]).output().map_err(storage::io_error)?;
        let list = parse_running_antigravity_ps(&String::from_utf8_lossy(&out.stdout));
        if let Some((_, b)) = list.first() { if let Ok(mut g) = self.last_bundle.lock() { *g = Some(b.clone()); } }
        Ok(list)
    }
    fn wait_until_stopped(&self, polls: usize) -> Result<bool> {
        for _ in 0..polls { if !self.is_running()? { return Ok(true); } std::thread::sleep(Duration::from_millis(100)); }
        Ok(false)
    }
}
impl AppLifecycle for SystemAppLifecycle {
    fn is_running(&self) -> Result<bool> {
        #[cfg(target_os = "macos")] { Ok(!self.detect_running()?.is_empty()) }
        #[cfg(not(target_os = "macos"))] { Ok(false) }
    }
    fn quit(&self) -> Result<()> {
        #[cfg(target_os = "macos")] {
            let running = self.detect_running()?;
            if running.is_empty() { return Ok(()); }
            let _ = std::process::Command::new("osascript").args(["-e", "tell application id \"com.google.antigravity\" to quit"]).output();
            if self.wait_until_stopped(25)? { return Ok(()); }
            for (pid, _) in &running { let _ = std::process::Command::new("kill").args(["-TERM", &pid.to_string()]).output(); }
            if self.wait_until_stopped(15)? { return Ok(()); }
            Err(AppError::new(code::PROCESS, "等待 Antigravity 退出超时，未修改任何凭据文件，请手动关闭后重试"))
        }
        #[cfg(not(target_os = "macos"))] { Ok(()) }
    }
    fn launch(&self) -> Result<()> {
        #[cfg(target_os = "macos")] {
            let mut cmd = std::process::Command::new("open");
            if let Some(p) = self.last_bundle.lock().ok().and_then(|g| g.clone()).filter(|p| p.exists()) { cmd.arg("-a").arg(p); } else { cmd.args(["-b", "com.google.antigravity"]); }
            if !cmd.status().map_err(storage::io_error)?.success() { return Err(AppError::new(code::PROCESS, "凭据已切换，但自动重新启动 Antigravity 失败，请手动打开应用")); }
        }
        Ok(())
    }
}
fn now_secs() -> u64 { SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0) }
fn clean_opt(raw: Option<&str>) -> Option<String> { raw.map(str::trim).filter(|s| !s.is_empty() && !s.chars().any(char::is_control)).map(str::to_owned) }
fn map_str(map: &Map<String, Value>, keys: &[&str]) -> Option<String> { keys.iter().find_map(|k| map.get(*k).and_then(Value::as_str)).and_then(|s| clean_opt(Some(s))) }
fn payload_digest(raw: &str) -> String { storage::digest(raw.trim().as_bytes()) }
fn with_lock<T>(f: impl FnOnce() -> Result<T>) -> Result<T> { let _g = AUTH_LOCK.lock().unwrap_or_else(|e| e.into_inner()); f() }
fn oauth_refresh_token(raw: &str) -> Option<String> {
    let v: Value = serde_json::from_str(raw.trim()).ok()?;
    let map = v.as_object()?;
    map_str(map, &["refresh_token"]).or_else(|| map.get("token")?.as_object().and_then(|t| map_str(t, &["refresh_token"])))
}
fn clean_label(raw: Option<&str>, fallback: &str) -> Result<String> {
    let target = raw.map(str::trim).filter(|s| !s.is_empty()).unwrap_or(fallback);
    if target.is_empty() || target.chars().count() > 100 || target.chars().any(char::is_control) {
        return Err(AppError::new(code::NAME, "名称需为 1–100 个字符，不能包含控制字符"));
    }
    Ok(target.to_owned())
}
pub(crate) fn write_private_file(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() { fs::create_dir_all(parent).map_err(storage::io_error)?; }
    storage::atomic_write(path, bytes, None)
}
fn read_text_optional(path: &Path) -> Result<Option<String>> {
    let Some(bytes) = storage::read_bounded(path, CREDENTIAL_LIMIT)? else { return Ok(None); };
    let text = String::from_utf8(bytes).map_err(|_| AppError::new(code::UTF8, "凭据文件不是有效的 UTF-8 文本"))?;
    Ok((!text.trim().is_empty()).then_some(text))
}
fn load_vault(paths: &Paths) -> Result<(Vault, String)> {
    storage::private_dir(&paths.data_dir)?;
    let loaded = storage::read_bounded(&paths.vault_file(), VAULT_LIMIT)?;
    let rev = storage::revision(loaded.as_deref());
    let vault = match loaded {
        Some(bytes) => serde_json::from_slice::<Vault>(&bytes).map_err(|_| AppError::new(code::STORE, "Antigravity 凭据库无法读取，原文件已保留"))?,
        None => Vault::default(),
    };
    if vault.schema != 1 { return Err(AppError::new(code::STORE_VERSION, "Antigravity 凭据库版本较新，请更新 Vela")); }
    Ok((vault, rev))
}
fn save_vault(paths: &Paths, vault: &Vault) -> Result<String> {
    storage::private_dir(&paths.data_dir)?;
    let bytes = serde_json::to_vec_pretty(vault).map_err(|_| AppError::new(code::STORE, "无法序列化 Antigravity 凭据库"))?;
    storage::atomic_write_bounded(&paths.vault_file(), &bytes, None, VAULT_LIMIT)?;
    Ok(storage::revision(Some(&bytes)))
}
fn decode_jwt_email(token: &str) -> Option<String> {
    let bytes = URL_SAFE_NO_PAD.decode(token.split('.').nth(1)?.trim_end_matches('=')).ok()?;
    clean_opt(serde_json::from_slice::<Value>(&bytes).ok()?.get("email").and_then(Value::as_str))
}
pub fn parse_oauth_metadata(raw: &str) -> Result<(String, Option<String>)> {
    let value: Value = serde_json::from_str(raw.trim()).map_err(|_| AppError::new(code::JSON, "OAuth 令牌文件不是有效的 JSON（请先在 Antigravity 中完成官方 OAuth 登录）"))?;
    let Some(map) = value.as_object() else { return Err(AppError::new(code::CONFIG_TYPE, "OAuth 令牌顶层必须是 JSON 对象")); };
    let nested = map.get("token").and_then(Value::as_object);
    if map_str(map, &["access_token", "refresh_token"]).is_none() && !nested.is_some_and(|t| map_str(t, &["access_token", "refresh_token"]).is_some()) {
        return Err(AppError::new(code::AUTH, "OAuth 令牌缺少有效令牌字段（access_token / refresh_token）"));
    }
    let email = map.get("user").and_then(Value::as_object).and_then(|u| map_str(u, &["email"]))
        .or_else(|| map_str(map, &["id_token_email", "email", "account", "client_email", "user"]))
        .or_else(|| nested.and_then(|t| map_str(t, &["email", "id_token_email"])))
        .or_else(|| map_str(map, &["id_token"]).or_else(|| nested.and_then(|t| map_str(t, &["id_token"]))).and_then(|jwt| decode_jwt_email(&jwt)));
    let method = map_str(map, &["auth_method", "provider"]).or_else(|| nested.and_then(|t| map_str(t, &["token_type"]))).unwrap_or_else(|| "oauth".to_owned());
    Ok((method, email))
}
pub fn parse_adc_metadata(raw: &str) -> Result<(AdcType, Option<String>, Option<String>, Value)> {
    let value: Value = serde_json::from_str(raw.trim()).map_err(|_| AppError::new(code::JSON, "ADC 凭据必须是有效的 JSON 对象"))?;
    let Some(map) = value.as_object() else { return Err(AppError::new(code::CONFIG_TYPE, "ADC 凭据顶层必须是 JSON 对象")); };
    let cred_type = match map_str(map, &["type"]).ok_or_else(|| AppError::new(code::AUTH, "ADC JSON 缺少 type 字段（需为 service_account 或 authorized_user）"))?.as_str() {
        "service_account" => {
            let pk = map.get("private_key").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).ok_or_else(|| AppError::new(code::AUTH, "service_account ADC 缺少 private_key 字段"))?;
            if !pk.contains("PRIVATE KEY") { return Err(AppError::new(code::AUTH, "service_account private_key 必须是有效的 PEM 私钥")); }
            if map_str(map, &["client_email"]).is_none() { return Err(AppError::new(code::AUTH, "service_account ADC 缺少 client_email 字段")); }
            if map_str(map, &["project_id", "quota_project_id"]).is_none() { return Err(AppError::new(code::AUTH, "service_account ADC 缺少 project_id 字段")); }
            AdcType::ServiceAccount
        }
        "authorized_user" => {
            for f in ["client_id", "client_secret", "refresh_token"] { if map_str(map, &[f]).is_none() { return Err(AppError::new(code::AUTH, &format!("authorized_user ADC 缺少必需字段：{f}"))); } }
            AdcType::AuthorizedUser
        }
        _ => return Err(AppError::new(code::AUTH, "不支持的 ADC 类型，仅支持 service_account 或 authorized_user")),
    };
    Ok((cred_type, map_str(map, &["client_email", "account", "email"]), map_str(map, &["quota_project_id", "project_id"]), value))
}
pub(crate) fn read_enable_adc(paths: &Paths) -> Result<(bool, Option<Vec<u8>>, Value)> {
    let raw = storage::read_bounded(&paths.config_file(), CREDENTIAL_LIMIT)?;
    let doc = match &raw {
        Some(bytes) => super::validate_text(&String::from_utf8(bytes.clone()).map_err(|_| AppError::new(code::UTF8, "Antigravity config.json 编码无效"))?)?,
        None => json!({}),
    };
    Ok((doc.get("userSettings").and_then(|u| u.get("enableAdc")).and_then(Value::as_bool).unwrap_or(false), raw, doc))
}
pub(crate) fn write_enable_adc(paths: &Paths, enable: bool) -> Result<()> {
    let (_, raw, mut doc) = read_enable_adc(paths)?;
    let obj = doc.as_object_mut().ok_or_else(|| AppError::new(code::CONFIG_TYPE, "config.json 顶层必须是对象"))?;
    obj.entry("userSettings").or_insert_with(|| json!({})).as_object_mut().ok_or_else(|| AppError::new(code::CONFIG_TYPE, "userSettings 必须是对象"))?.insert("enableAdc".into(), Value::Bool(enable));
    let formatted = format!("{}\n", serde_json::to_string_pretty(&doc).map_err(|_| AppError::new(code::JSON, "无法序列化 config.json"))?);
    super::validate_text(&formatted)?;
    if let Some(parent) = paths.config_file().parent() { fs::create_dir_all(parent).map_err(storage::io_error)?; }
    storage::atomic_write(&paths.config_file(), formatted.as_bytes(), Some(&storage::revision(raw.as_deref())))
}
fn remember_observed(vault: &mut Vault, digest: &str) {
    vault.observed_revisions.retain(|r| r != digest);
    if vault.observed_revisions.len() >= 64 { vault.observed_revisions.drain(..vault.observed_revisions.len() - 63); }
    vault.observed_revisions.push(digest.to_owned());
}
fn read_checked(path: &Path, clean: &mut bool) -> Option<String> {
    match read_text_optional(path) { Ok(v) => v, Err(_) => { *clean = false; None } }
}
fn state_with_health(paths: &Paths) -> Result<(AntigravityAuthState, bool)> {
    let (mut vault, mut vault_rev) = load_vault(paths)?;
    let (mut dirty, mut clean) = (false, true);
    let (mut oauth_auth_method, mut oauth_email, mut oauth_digest) = (None, None, None);
    if let Some(raw) = read_checked(&paths.oauth_file(), &mut clean) {
        if let Ok((auth_method, email)) = parse_oauth_metadata(&raw) {
            let (digest, disk_rt) = (payload_digest(&raw), oauth_refresh_token(&raw));
            let idx = vault.entries.iter().position(|e| {
                e.kind == AccountKind::Oauth && match (&e.email, &email) {
                    (Some(ae), Some(de)) => ae.eq_ignore_ascii_case(de),
                    (None, None) => payload_digest(&e.payload) == digest || (disk_rt.is_some() && oauth_refresh_token(&e.payload) == disk_rt),
                    _ => false,
                }
            }).or_else(|| email.is_none().then(|| vault.entries.iter().position(|e| e.kind == AccountKind::Oauth && e.email.is_none())).flatten());
            if let Some(i) = idx {
                let e = &mut vault.entries[i];
                if payload_digest(&e.payload) != digest {
                    e.payload = raw; e.email = email.clone(); e.source_type = auth_method.clone(); e.updated_at = now_secs(); dirty = true;
                }
            } else if !vault.observed_revisions.iter().any(|r| r == &digest) {
                let name = clean_label(None, email.as_deref().unwrap_or("OAuth 账号"))?;
                vault.entries.push(StoredEntry { id: Uuid::new_v4().to_string(), name, kind: AccountKind::Oauth, source_type: auth_method.clone(), email: email.clone(), project_id: None, payload: raw, updated_at: now_secs() });
                dirty = true;
            }
            oauth_auth_method = Some(auth_method); oauth_email = email; oauth_digest = Some(digest);
        } else { clean = false; }
    }
    let (mut adc_type, mut adc_email, mut adc_project, mut adc_digest) = (None, None, None, None);
    if let Some(adc_path) = [paths.adc_file(), paths.adc_disabled_file(), paths.agswitch_adc_file()].into_iter().find(|p| p.exists()) {
        if let Some(raw) = read_checked(&adc_path, &mut clean) {
            if let Ok((cred_type, email, project_id, _)) = parse_adc_metadata(&raw) {
                let digest = payload_digest(&raw);
                if !vault.entries.iter().any(|e| e.kind == AccountKind::Adc && payload_digest(&e.payload) == digest) && !vault.observed_revisions.iter().any(|r| r == &digest) {
                    let name = clean_label(None, project_id.as_deref().or(email.as_deref()).unwrap_or("ADC 凭据"))?;
                    vault.entries.push(StoredEntry { id: Uuid::new_v4().to_string(), name, kind: AccountKind::Adc, source_type: cred_type.as_str().to_owned(), email: email.clone(), project_id: project_id.clone(), payload: raw, updated_at: now_secs() });
                    dirty = true;
                }
                adc_type = Some(cred_type); adc_email = email; adc_project = project_id; adc_digest = Some(digest);
            } else { clean = false; }
        }
    }
    if dirty { vault_rev = save_vault(paths, &vault)?; }
    let (enable_adc, cfg_raw) = match read_enable_adc(paths) { Ok((en, raw, _)) => (en, raw), Err(_) => { clean = false; (false, None) } };
    let (adc_on, backup_on) = (paths.adc_file().exists(), paths.adc_disabled_file().exists() || paths.agswitch_adc_file().exists());
    let adc_status = if adc_on { if enable_adc { AdcStatus::Active } else { AdcStatus::Disabled } } else if backup_on { AdcStatus::BackupOnly } else { AdcStatus::Missing };
    let mode = match (enable_adc, adc_on) { (true, true) => AuthMode::Adc, (false, false) => AuthMode::Oauth, (false, true) => AuthMode::MixedAdcFileWithoutFlag, (true, false) => AuthMode::MixedFlagWithoutAdcFile };
    let accounts = vault.entries.iter().map(|e| {
        let cred_rev = payload_digest(&e.payload);
        let current = match e.kind {
            AccountKind::Oauth => oauth_digest.as_deref() == Some(cred_rev.as_str()) || matches!((&e.email, &oauth_email), (Some(ae), Some(de)) if ae.eq_ignore_ascii_case(de)),
            AccountKind::Adc => adc_digest.as_deref() == Some(cred_rev.as_str()),
        };
        AntigravityAccountItem { id: e.id.clone(), name: e.name.clone(), kind: e.kind, source_type: e.source_type.clone(), email: e.email.clone(), project_id: e.project_id.clone(), current, updated_at: e.updated_at, credential_revision: cred_rev }
    }).collect();
    let revision = storage::digest(format!("{vault_rev}:{}:{}:{:?}:{:?}", storage::revision(cfg_raw.as_deref()), oauth_digest.as_deref().unwrap_or("none"), adc_status, adc_digest).as_bytes());
    Ok((AntigravityAuthState { accounts, mode, enable_adc, oauth_present: oauth_digest.is_some(), oauth_email, oauth_auth_method, adc_status, adc_type, adc_email, adc_project, app_restarted: false, revision }, clean))
}
pub fn state_at(paths: &Paths) -> Result<AntigravityAuthState> { state_with_health(paths).map(|(s, _)| s) }
fn verify_revision(paths: &Paths, expected_revision: &str) -> Result<AntigravityAuthState> {
    let current = state_at(paths)?;
    (current.revision == expected_revision).then_some(current).ok_or_else(|| AppError::new(code::CONFLICT, "Antigravity 凭据状态已变更，请刷新后重试"))
}
fn restore_file(path: &Path, prev: Option<&[u8]>) -> Result<()> {
    match prev { Some(bytes) => write_private_file(path, bytes), None => fs::remove_file(path).or_else(|e| if e.kind() == std::io::ErrorKind::NotFound { Ok(()) } else { Err(storage::io_error(e)) }) }
}
fn run_with_rollback_and_lifecycle<F: FnOnce() -> Result<()>>(paths: &Paths, expected_revision: &str, lifecycle: &dyn AppLifecycle, mutate: F) -> Result<AntigravityAuthState> {
    with_lock(|| verify_revision(paths, expected_revision).map(|_| ()))?;
    let was_running = lifecycle.is_running()?;
    if was_running { lifecycle.quit()?; }
    let next = with_lock(|| {
        verify_revision(paths, expected_revision)?;
        let snapshots: Vec<_> = paths.watched_files().into_iter().map(|f| storage::read_optional(&f).map(|b| (f, b))).collect::<Result<_>>()?;
        if let Err(err) = mutate() {
            let residuals: Vec<_> = snapshots.iter().filter_map(|(f, p)| restore_file(f, p.as_deref()).err().map(|_| f.display().to_string())).collect();
            return Err(if residuals.is_empty() { err } else { AppError::new(&err.code, &format!("{}（回滚残留未恢复：{}）", err.message, residuals.join(", "))) });
        }
        state_at(paths)
    });
    match next {
        Ok(s) => { if was_running { lifecycle.launch()?; } Ok(AntigravityAuthState { app_restarted: was_running, ..s }) }
        Err(e) => { if was_running { let _ = lifecycle.launch(); } Err(e) }
    }
}
fn apply_mode_files(paths: &Paths, enable_adc: bool, vault: &Vault) -> Result<()> {
    let (adc_path, disabled_path, agswitch_path) = (paths.adc_file(), paths.adc_disabled_file(), paths.agswitch_adc_file());
    if !enable_adc {
        if let Some(adc_bytes) = storage::read_optional(&adc_path)? { write_private_file(&disabled_path, &adc_bytes)?; fs::remove_file(&adc_path).map_err(storage::io_error)?; }
        return write_enable_adc(paths, false);
    }
    if !adc_path.exists() {
        let source_bytes = storage::read_optional(&disabled_path)?.or(storage::read_optional(&agswitch_path)?).or_else(|| vault.entries.iter().find(|e| e.kind == AccountKind::Adc).map(|e| e.payload.as_bytes().to_vec())).ok_or_else(|| AppError::new(code::AUTH, "当前没有可用的 ADC 凭据，请先导入 ADC JSON"))?;
        parse_adc_metadata(&String::from_utf8(source_bytes.clone()).map_err(|_| AppError::new(code::UTF8, "ADC 凭据编码无效"))?)?;
        write_private_file(&adc_path, &source_bytes)?; write_private_file(&agswitch_path, &source_bytes)?;
        if disabled_path.exists() { fs::remove_file(&disabled_path).map_err(storage::io_error)?; }
    }
    write_enable_adc(paths, true)
}
pub fn add_adc_at(paths: &Paths, lifecycle: &dyn AppLifecycle, input: AddAntigravityAdcInput) -> Result<AntigravityAuthState> {
    let text = input.json_text.trim();
    if text.is_empty() { return Err(AppError::new(code::JSON, "请粘贴或导入有效的 ADC JSON 内容")); }
    let (cred_type, email, parsed_project, mut value) = parse_adc_metadata(text)?;
    let override_project = clean_opt(input.quota_project_id.as_deref());
    if let (Some(ref qp), Some(obj)) = (&override_project, value.as_object_mut()) { obj.insert("quota_project_id".into(), Value::String(qp.clone())); }
    let project_id = override_project.or(parsed_project);
    let payload = format!("{}\n", serde_json::to_string_pretty(&value).map_err(|_| AppError::new(code::JSON, "无法格式化 ADC JSON"))?);
    let digest = payload_digest(&payload);
    let label = clean_label(input.name.as_deref(), project_id.as_deref().or(email.as_deref()).unwrap_or("ADC 凭据"))?;
    let (id, state) = with_lock(|| {
        let (mut vault, _) = load_vault(paths)?;
        let now = now_secs();
        let id = if let Some(ex) = vault.entries.iter_mut().find(|e| e.kind == AccountKind::Adc && payload_digest(&e.payload) == digest) {
            if input.name.as_deref().is_some_and(|s| !s.trim().is_empty()) { ex.name = label; }
            ex.email = email; ex.project_id = project_id; ex.payload = payload; ex.updated_at = now; ex.id.clone()
        } else {
            let id = Uuid::new_v4().to_string();
            vault.entries.push(StoredEntry { id: id.clone(), name: label, kind: AccountKind::Adc, source_type: cred_type.as_str().to_owned(), email, project_id, payload, updated_at: now });
            id
        };
        vault.observed_revisions.retain(|r| r != &digest);
        save_vault(paths, &vault)?;
        Ok((id, state_at(paths)?))
    })?;
    if input.activate { switch_account_at(paths, lifecycle, &id, &state.revision) } else { Ok(state) }
}
pub fn switch_account_at(paths: &Paths, lifecycle: &dyn AppLifecycle, id: &str, expected_revision: &str) -> Result<AntigravityAuthState> {
    let (vault, entry) = with_lock(|| {
        let (v, _) = load_vault(paths)?;
        let e = v.entries.iter().find(|e| e.id == id).cloned().ok_or_else(|| AppError::new(code::ACCOUNT, "未找到指定的 Antigravity 凭据条目"))?;
        match e.kind { AccountKind::Oauth => { parse_oauth_metadata(&e.payload)?; } AccountKind::Adc => { parse_adc_metadata(&e.payload)?; } }
        Ok((v, e))
    })?;
    run_with_rollback_and_lifecycle(paths, expected_revision, lifecycle, || match entry.kind {
        AccountKind::Oauth => { write_private_file(&paths.oauth_file(), entry.payload.as_bytes())?; apply_mode_files(paths, false, &vault) }
        AccountKind::Adc => {
            write_private_file(&paths.adc_file(), entry.payload.as_bytes())?; write_private_file(&paths.agswitch_adc_file(), entry.payload.as_bytes())?;
            if paths.adc_disabled_file().exists() { fs::remove_file(paths.adc_disabled_file()).map_err(storage::io_error)?; }
            write_enable_adc(paths, true)
        }
    })
}
pub fn set_mode_at(paths: &Paths, lifecycle: &dyn AppLifecycle, mode: TargetMode, expected_revision: &str) -> Result<AntigravityAuthState> {
    let enable_adc = matches!(mode, TargetMode::Adc);
    let vault = with_lock(|| {
        if !enable_adc && !paths.oauth_file().exists() { return Err(AppError::new(code::AUTH, "未检测到 Antigravity OAuth 登录令牌，请先在 Antigravity 内登录账号")); }
        Ok(load_vault(paths)?.0)
    })?;
    run_with_rollback_and_lifecycle(paths, expected_revision, lifecycle, || apply_mode_files(paths, enable_adc, &vault))
}
pub fn rename_account_at(paths: &Paths, id: &str, name: &str, expected_revision: &str) -> Result<AntigravityAuthState> {
    with_lock(|| {
        verify_revision(paths, expected_revision)?;
        let (cleaned, (mut vault, _)) = (clean_label(Some(name), "")?, load_vault(paths)?);
        let entry = vault.entries.iter_mut().find(|e| e.id == id).ok_or_else(|| AppError::new(code::ACCOUNT, "未找到指定的凭据条目"))?;
        entry.name = cleaned; entry.updated_at = now_secs();
        save_vault(paths, &vault)?; state_at(paths)
    })
}
pub fn delete_account_at(paths: &Paths, id: &str, expected_revision: &str) -> Result<AntigravityAuthState> {
    with_lock(|| {
        if verify_revision(paths, expected_revision)?.accounts.iter().any(|a| a.id == id && a.current) { return Err(AppError::new(code::CONFLICT, "当前正在使用中的凭据不可直接删除，请先切换到其他凭据")); }
        let (mut vault, _) = load_vault(paths)?;
        let idx = vault.entries.iter().position(|e| e.id == id).ok_or_else(|| AppError::new(code::ACCOUNT, "未找到指定的凭据条目"))?;
        let removed = vault.entries.remove(idx);
        remember_observed(&mut vault, &payload_digest(&removed.payload));
        save_vault(paths, &vault)?; state_at(paths)
    })
}
pub fn disk_stamp() -> Option<String> {
    let mut stamp = String::new();
    for path in Paths::from_roots(&dirs::home_dir()?, Path::new("")).watched_files() {
        if let Ok(m) = fs::metadata(&path) { stamp.push_str(&format!("1:{}:{};", m.len(), m.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_nanos()).unwrap_or(0))); } else { stamp.push_str("0;"); }
    }
    Some(stamp)
}
pub fn poll_state(data_dir: &Path) -> Result<(AntigravityAuthState, bool)> { with_lock(|| state_with_health(&Paths::resolve(data_dir)?)) }
pub fn current_state(data_dir: &Path) -> Result<AntigravityAuthState> { poll_state(data_dir).map(|(s, _)| s) }
async fn blocking<T: Send + 'static>(msg: &'static str, f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    tauri::async_runtime::spawn_blocking(f).await.map_err(|_| AppError::new(code::PROCESS, msg))?
}
#[tauri::command] pub fn get_antigravity_auth_state(r: R<'_>) -> Result<AntigravityAuthState> { current_state(&r.data) }
#[tauri::command] pub async fn add_antigravity_adc(r: R<'_>, input: AddAntigravityAdcInput) -> Result<AntigravityAuthState> {
    let d = r.data.clone(); blocking("导入 ADC 任务异常中断", move || add_adc_at(&Paths::resolve(&d)?, &SystemAppLifecycle::default(), input)).await
}
#[tauri::command] pub async fn pick_antigravity_credential_file(app: tauri::AppHandle) -> Result<Option<String>> {
    let picked = tauri::async_runtime::spawn_blocking(move || app.dialog().file().add_filter("ADC JSON", &["json"]).blocking_pick_file()).await.map_err(|_| AppError::new(code::DIALOG, "无法打开文件选择器"))?;
    let Some(file_path) = picked else { return Ok(None); };
    let path = file_path.into_path().map_err(|_| AppError::new(code::PATH, "请选择本地文件"))?;
    let bytes = storage::read_bounded(&path, CREDENTIAL_LIMIT)?.ok_or_else(|| AppError::new(code::PATH, "所选文件不存在"))?;
    String::from_utf8(bytes).map(Some).map_err(|_| AppError::new(code::UTF8, "所选文件不是 UTF-8 文本"))
}
#[tauri::command] pub async fn switch_antigravity_account(r: R<'_>, id: String, expected_revision: String) -> Result<AntigravityAuthState> {
    let d = r.data.clone(); blocking("切换凭据任务异常中断", move || switch_account_at(&Paths::resolve(&d)?, &SystemAppLifecycle::default(), &id, &expected_revision)).await
}
#[tauri::command] pub async fn set_antigravity_auth_mode(r: R<'_>, mode: TargetMode, expected_revision: String) -> Result<AntigravityAuthState> {
    let d = r.data.clone(); blocking("切换认证模式任务异常中断", move || set_mode_at(&Paths::resolve(&d)?, &SystemAppLifecycle::default(), mode, &expected_revision)).await
}
#[tauri::command] pub fn rename_antigravity_account(r: R<'_>, id: String, name: String, expected_revision: String) -> Result<AntigravityAuthState> {
    rename_account_at(&Paths::resolve(&r.data)?, &id, &name, &expected_revision)
}
#[tauri::command] pub fn delete_antigravity_account(r: R<'_>, id: String, expected_revision: String) -> Result<AntigravityAuthState> {
    delete_account_at(&Paths::resolve(&r.data)?, &id, &expected_revision)
}
#[cfg(test)] #[path = "antigravity_auth_tests.rs"] mod tests;
