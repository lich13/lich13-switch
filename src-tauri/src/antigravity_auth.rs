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
fn err(c: &str, msg: &str) -> AppError {
    AppError::new(c, msg)
}
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
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ActivationSpan {
    pub account_id: String,
    pub since_ms: i64,
}
#[derive(Clone, Debug, Default)]
pub struct ProviderTimeline {
    spans: Vec<ActivationSpan>,
}
impl ProviderTimeline {
    pub fn provider_at(&self, started_at: i64) -> Option<&str> {
        self.spans
            .iter()
            .rfind(|s| s.since_ms <= started_at)
            .map(|s| s.account_id.as_str())
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
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Vault {
    schema: u32,
    entries: Vec<StoredEntry>,
    #[serde(default)]
    observed_revisions: Vec<String>,
    #[serde(default)]
    active_oauth_id: Option<String>,
    #[serde(default)]
    activations: Vec<ActivationSpan>,
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
        Ok(Self::from_roots(
            &dirs::home_dir().ok_or_else(|| err(code::PATH, "无法定位用户主目录"))?,
            data_dir,
        ))
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
    pub fn watched_files(&self) -> [PathBuf; 5] {
        [
            self.oauth_file(),
            self.config_file(),
            self.adc_file(),
            self.adc_disabled_file(),
            self.agswitch_adc_file(),
        ]
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
            let (pid, comm) = line.trim().split_once(char::is_whitespace)?;
            let bundle = comm.trim().strip_suffix("/Contents/MacOS/Antigravity")?;
            Some((pid.trim().parse().ok()?, PathBuf::from(bundle)))
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
    fn wait_until_stopped(&self, polls: usize) -> Result<bool> {
        for _ in 0..polls {
            if !self.is_running()? {
                return Ok(true);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Ok(false)
    }
}
impl AppLifecycle for SystemAppLifecycle {
    fn is_running(&self) -> Result<bool> {
        #[cfg(target_os = "macos")]
        return Ok(!self.detect_running()?.is_empty());
        #[cfg(not(target_os = "macos"))]
        Ok(false)
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
            if self.wait_until_stopped(25)? {
                return Ok(());
            }
            for (pid, _) in &running {
                let _ = std::process::Command::new("kill")
                    .args(["-TERM", &pid.to_string()])
                    .output();
            }
            if !self.wait_until_stopped(15)? {
                return Err(err(
                    code::PROCESS,
                    "等待 Antigravity 退出超时，未修改任何凭据文件，请手动关闭后重试",
                ));
            }
        }
        Ok(())
    }
    fn launch(&self) -> Result<()> {
        #[cfg(target_os = "macos")]
        {
            let mut cmd = std::process::Command::new("open");
            if let Some(p) = self
                .last_bundle
                .lock()
                .ok()
                .and_then(|g| g.clone())
                .filter(|p| p.exists())
            {
                cmd.arg("-a").arg(p);
            } else {
                cmd.args(["-b", "com.google.antigravity"]);
            }
            if !cmd.status().map_err(storage::io_error)?.success() {
                return Err(err(
                    code::PROCESS,
                    "凭据已切换，但自动重新启动 Antigravity 失败，请手动打开应用",
                ));
            }
        }
        Ok(())
    }
}
fn epoch_dur(t: SystemTime) -> Duration {
    t.duration_since(UNIX_EPOCH).unwrap_or_default()
}
fn now_secs() -> u64 {
    epoch_dur(SystemTime::now()).as_secs()
}
fn now_ms() -> i64 {
    epoch_dur(SystemTime::now()).as_millis() as i64
}
fn file_mtime_ms(path: &Path) -> Option<i64> {
    Some(epoch_dur(fs::metadata(path).ok()?.modified().ok()?).as_millis() as i64)
}
fn clean_opt(raw: Option<&str>) -> Option<String> {
    raw.map(str::trim)
        .filter(|s| !s.is_empty() && !s.chars().any(char::is_control))
        .map(str::to_owned)
}
fn map_str(map: &Map<String, Value>, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|k| clean_opt(map.get(*k).and_then(Value::as_str)))
}
fn payload_digest(raw: &str) -> String {
    storage::digest(raw.trim().as_bytes())
}
fn with_lock<T>(f: impl FnOnce() -> Result<T>) -> Result<T> {
    let _g = AUTH_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    f()
}
fn oauth_refresh_token(raw: &str) -> Option<String> {
    let v: Value = serde_json::from_str(raw.trim()).ok()?;
    let map = v.as_object()?;
    map_str(map, &["refresh_token"])
        .or_else(|| map_str(map.get("token")?.as_object()?, &["refresh_token"]))
}
fn clean_label(raw: Option<&str>, fallback: &str) -> Result<String> {
    let t = raw
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(fallback);
    if t.is_empty() || t.chars().count() > 100 || t.chars().any(char::is_control) {
        return Err(err(code::NAME, "名称需为 1–100 个字符，不能包含控制字符"));
    }
    Ok(t.to_owned())
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
    let text =
        String::from_utf8(bytes).map_err(|_| err(code::UTF8, "凭据文件不是有效的 UTF-8 文本"))?;
    Ok((!text.trim().is_empty()).then_some(text))
}
fn load_vault(paths: &Paths) -> Result<(Vault, String)> {
    storage::private_dir(&paths.data_dir)?;
    let loaded = storage::read_bounded(&paths.vault_file(), VAULT_LIMIT)?;
    let rev = storage::revision(loaded.as_deref());
    let mut vault = match loaded {
        Some(b) => serde_json::from_slice(&b)
            .map_err(|_| err(code::STORE, "Antigravity 凭据库无法读取，原文件已保留"))?,
        None => Vault::default(),
    };
    if vault.schema == 0 {
        vault.schema = 1;
    } else if vault.schema != 1 {
        return Err(err(
            code::STORE_VERSION,
            "Antigravity 凭据库版本较新，请更新 Vela",
        ));
    }
    Ok((vault, rev))
}
fn save_vault(paths: &Paths, vault: &Vault) -> Result<String> {
    storage::private_dir(&paths.data_dir)?;
    let bytes = serde_json::to_vec_pretty(vault)
        .map_err(|_| err(code::STORE, "无法序列化 Antigravity 凭据库"))?;
    storage::atomic_write_bounded(&paths.vault_file(), &bytes, None, VAULT_LIMIT)?;
    Ok(storage::revision(Some(&bytes)))
}
fn decode_jwt_email(token: &str) -> Option<String> {
    let bytes = URL_SAFE_NO_PAD
        .decode(token.split('.').nth(1)?.trim_end_matches('='))
        .ok()?;
    let val = serde_json::from_slice::<Value>(&bytes).ok()?;
    clean_opt(val.get("email").and_then(Value::as_str))
}
pub fn parse_oauth_metadata(raw: &str) -> Result<(String, Option<String>)> {
    let value: Value = serde_json::from_str(raw.trim()).map_err(|_| {
        err(
            code::JSON,
            "OAuth 令牌文件不是有效的 JSON（请先在 Antigravity 中完成官方 OAuth 登录）",
        )
    })?;
    let Some(map) = value.as_object() else {
        return Err(err(code::CONFIG_TYPE, "OAuth 令牌顶层必须是 JSON 对象"));
    };
    let nested = map.get("token").and_then(Value::as_object);
    let has_tok = map_str(map, &["access_token", "refresh_token"]).is_some()
        || nested.is_some_and(|t| map_str(t, &["access_token", "refresh_token"]).is_some());
    if !has_tok {
        return Err(err(
            code::AUTH,
            "OAuth 令牌缺少有效令牌字段（access_token / refresh_token）",
        ));
    }
    let u_email = map
        .get("user")
        .and_then(Value::as_object)
        .and_then(|u| map_str(u, &["email"]));
    let top_email = map_str(
        map,
        &["id_token_email", "email", "account", "client_email", "user"],
    );
    let tok_email = nested.and_then(|t| map_str(t, &["email", "id_token_email"]));
    let jwt =
        map_str(map, &["id_token"]).or_else(|| nested.and_then(|t| map_str(t, &["id_token"])));
    let email = u_email
        .or(top_email)
        .or(tok_email)
        .or_else(|| decode_jwt_email(&jwt?));
    let method = map_str(map, &["auth_method", "provider"])
        .or_else(|| nested.and_then(|t| map_str(t, &["token_type"])))
        .unwrap_or_else(|| "oauth".to_owned());
    Ok((method, email))
}
pub fn parse_adc_metadata(raw: &str) -> Result<(AdcType, Option<String>, Option<String>, Value)> {
    let value: Value = serde_json::from_str(raw.trim())
        .map_err(|_| err(code::JSON, "ADC 凭据必须是有效的 JSON 对象"))?;
    let Some(map) = value.as_object() else {
        return Err(err(code::CONFIG_TYPE, "ADC 凭据顶层必须是 JSON 对象"));
    };
    let Some(raw_type) = map_str(map, &["type"]) else {
        return Err(err(
            code::AUTH,
            "ADC JSON 缺少 type 字段（需为 service_account 或 authorized_user）",
        ));
    };
    let cred_type = match raw_type.as_str() {
        "service_account" => {
            let pk = map
                .get("private_key")
                .and_then(Value::as_str)
                .map(str::trim)
                .unwrap_or("");
            if pk.is_empty() {
                return Err(err(code::AUTH, "service_account ADC 缺少 private_key 字段"));
            }
            if !pk.contains("PRIVATE KEY") {
                return Err(err(
                    code::AUTH,
                    "service_account private_key 必须是有效的 PEM 私钥",
                ));
            }
            if map_str(map, &["client_email"]).is_none() {
                return Err(err(
                    code::AUTH,
                    "service_account ADC 缺少 client_email 字段",
                ));
            }
            if map_str(map, &["project_id", "quota_project_id"]).is_none() {
                return Err(err(code::AUTH, "service_account ADC 缺少 project_id 字段"));
            }
            AdcType::ServiceAccount
        }
        "authorized_user" => {
            for f in ["client_id", "client_secret", "refresh_token"] {
                if map_str(map, &[f]).is_none() {
                    return Err(err(
                        code::AUTH,
                        &format!("authorized_user ADC 缺少必需字段：{f}"),
                    ));
                }
            }
            AdcType::AuthorizedUser
        }
        _ => {
            return Err(err(
                code::AUTH,
                "不支持的 ADC 类型，仅支持 service_account 或 authorized_user",
            ))
        }
    };
    Ok((
        cred_type,
        map_str(map, &["client_email", "account", "email"]),
        map_str(map, &["quota_project_id", "project_id"]),
        value,
    ))
}
pub(crate) fn read_enable_adc(paths: &Paths) -> Result<(bool, Option<Vec<u8>>, Value)> {
    let raw = storage::read_bounded(&paths.config_file(), CREDENTIAL_LIMIT)?;
    let doc = match &raw {
        Some(b) => super::validate_text(
            &String::from_utf8(b.clone())
                .map_err(|_| err(code::UTF8, "Antigravity config.json 编码无效"))?,
        )?,
        None => json!({}),
    };
    let en = doc["userSettings"]["enableAdc"].as_bool().unwrap_or(false);
    Ok((en, raw, doc))
}
pub(crate) fn write_enable_adc(paths: &Paths, enable: bool) -> Result<()> {
    let (_, raw, mut doc) = read_enable_adc(paths)?;
    let obj = doc
        .as_object_mut()
        .ok_or_else(|| err(code::CONFIG_TYPE, "config.json 顶层必须是对象"))?;
    obj.entry("userSettings")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| err(code::CONFIG_TYPE, "userSettings 必须是对象"))?
        .insert("enableAdc".into(), Value::Bool(enable));
    let pretty = serde_json::to_string_pretty(&doc)
        .map_err(|_| err(code::JSON, "无法序列化 config.json"))?;
    let formatted = format!("{pretty}\n");
    super::validate_text(&formatted)?;
    if let Some(p) = paths.config_file().parent() {
        fs::create_dir_all(p).map_err(storage::io_error)?;
    }
    let rev = storage::revision(raw.as_deref());
    storage::atomic_write(&paths.config_file(), formatted.as_bytes(), Some(&rev))
}
fn remember_observed(vault: &mut Vault, digest: &str) {
    vault.observed_revisions.retain(|r| r != digest);
    if vault.observed_revisions.len() >= 64 {
        let drop_n = vault.observed_revisions.len() - 63;
        vault.observed_revisions.drain(..drop_n);
    }
    vault.observed_revisions.push(digest.to_owned());
}
fn read_checked(path: &Path, clean: &mut bool) -> Option<String> {
    read_text_optional(path).unwrap_or_else(|_| {
        *clean = false;
        None
    })
}
fn adc_label(raw: Option<&str>, email: &Option<String>, proj: &Option<String>) -> Result<String> {
    clean_label(
        raw,
        proj.as_deref().or(email.as_deref()).unwrap_or("ADC 凭据"),
    )
}
impl Vault {
    fn index_of(&self, id: &str) -> Result<usize> {
        self.entries
            .iter()
            .position(|e| e.id == id)
            .ok_or_else(|| err(code::ACCOUNT, "未找到指定的 Antigravity 凭据条目"))
    }
    fn push_new(
        &mut self,
        name: String,
        kind: AccountKind,
        st: &str,
        meta: (Option<String>, Option<String>),
        payload: String,
    ) -> String {
        let id = Uuid::new_v4().to_string();
        self.entries.push(StoredEntry {
            id: id.clone(),
            name,
            kind,
            source_type: st.to_owned(),
            email: meta.0,
            project_id: meta.1,
            payload,
            updated_at: now_secs(),
        });
        id
    }
}
fn find_oauth_idx(
    v: &Vault,
    email: &Option<String>,
    dig: &str,
    rt: &Option<String>,
) -> Option<usize> {
    let ok_rt = |e: &StoredEntry| !matches!((oauth_refresh_token(&e.payload), rt), (Some(a), Some(b)) if &a != b);
    if let Some(i) = v.entries.iter().position(|e| {
        e.kind == AccountKind::Oauth
            && match (&e.email, email) {
                (Some(ae), Some(de)) => ae.eq_ignore_ascii_case(de),
                (None, None) => {
                    payload_digest(&e.payload) == dig
                        || (rt.is_some() && &oauth_refresh_token(&e.payload) == rt)
                }
                _ => false,
            }
    }) {
        return Some(i);
    }
    if email.is_some() {
        return None;
    }
    if let Some(id) = v.active_oauth_id.as_deref() {
        if let Some(i) = v.entries.iter().position(|e| {
            e.id == id && e.kind == AccountKind::Oauth && e.email.is_none() && ok_rt(e)
        }) {
            return Some(i);
        }
    }
    let mut it = v
        .entries
        .iter()
        .enumerate()
        .filter(|(_, e)| e.kind == AccountKind::Oauth && e.email.is_none());
    let (pos, entry) = it.next()?;
    (it.next().is_none() && ok_rt(entry)).then_some(pos)
}
fn entry_matches(e: &StoredEntry, digest: Option<&str>, email: Option<&str>) -> bool {
    digest == Some(payload_digest(&e.payload).as_str())
        || (e.kind == AccountKind::Oauth
            && matches!((e.email.as_deref(), email), (Some(a), Some(b)) if a.eq_ignore_ascii_case(b)))
}
fn state_with_health(paths: &Paths) -> Result<(AntigravityAuthState, bool)> {
    let (mut vault, mut vault_rev) = load_vault(paths)?;
    let (mut dirty, mut clean) = (false, true);
    let (mut oauth_auth_method, mut oauth_email, mut oauth_digest) = (None, None, None);
    if let Some(raw) = read_checked(&paths.oauth_file(), &mut clean) {
        match parse_oauth_metadata(&raw) {
            Ok((auth_method, email)) => {
                let (digest, disk_rt) = (payload_digest(&raw), oauth_refresh_token(&raw));
                if let Some(i) = find_oauth_idx(&vault, &email, &digest, &disk_rt) {
                    let entry_id = vault.entries[i].id.clone();
                    let e = &mut vault.entries[i];
                    if payload_digest(&e.payload) != digest {
                        e.payload = raw;
                        e.email = email.clone();
                        e.source_type = auth_method.clone();
                        e.updated_at = now_secs();
                        dirty = true;
                    }
                    if vault.active_oauth_id.as_deref() != Some(entry_id.as_str()) {
                        vault.active_oauth_id = Some(entry_id);
                        dirty = true;
                    }
                } else if !vault.observed_revisions.iter().any(|r| r == &digest) {
                    let name = clean_label(None, email.as_deref().unwrap_or("OAuth 账号"))?;
                    let meta = (email.clone(), None);
                    let id = vault.push_new(name, AccountKind::Oauth, &auth_method, meta, raw);
                    vault.active_oauth_id = Some(id);
                    dirty = true;
                }
                oauth_auth_method = Some(auth_method);
                oauth_email = email;
                oauth_digest = Some(digest);
            }
            Err(e) if e.code == code::JSON => clean = false,
            Err(_) => {}
        }
    }
    let (mut adc_type, mut adc_email, mut adc_project, mut adc_digest) = (None, None, None, None);
    let adc_opt = [
        paths.adc_file(),
        paths.adc_disabled_file(),
        paths.agswitch_adc_file(),
    ]
    .into_iter()
    .find(|p| p.exists());
    if let Some(adc_path) = adc_opt {
        if let Some(raw) = read_checked(&adc_path, &mut clean) {
            match parse_adc_metadata(&raw) {
                Ok((cred_type, email, project_id, _)) => {
                    let digest = payload_digest(&raw);
                    let known = vault.entries.iter().any(|e| {
                        e.kind == AccountKind::Adc && payload_digest(&e.payload) == digest
                    }) || vault.observed_revisions.iter().any(|r| r == &digest);
                    if !known {
                        let name = adc_label(None, &email, &project_id)?;
                        let meta = (email.clone(), project_id.clone());
                        vault.push_new(name, AccountKind::Adc, cred_type.as_str(), meta, raw);
                        dirty = true;
                    }
                    adc_type = Some(cred_type);
                    adc_email = email;
                    adc_project = project_id;
                    adc_digest = Some(digest);
                }
                Err(e) if e.code == code::JSON => clean = false,
                Err(_) => {}
            }
        }
    }
    let (enable_adc, cfg_raw, _) = read_enable_adc(paths).unwrap_or_else(|_| {
        clean = false;
        (false, None, json!({}))
    });
    let adc_on = paths.adc_file().exists();
    let backup_on = paths.adc_disabled_file().exists() || paths.agswitch_adc_file().exists();
    let adc_status = match (adc_on, enable_adc, backup_on) {
        (true, true, _) => AdcStatus::Active,
        (true, false, _) => AdcStatus::Disabled,
        (false, _, true) => AdcStatus::BackupOnly,
        (false, _, false) => AdcStatus::Missing,
    };
    let mode = match (enable_adc, adc_on) {
        (true, true) => AuthMode::Adc,
        (false, false) => AuthMode::Oauth,
        (false, true) => AuthMode::MixedAdcFileWithoutFlag,
        (true, false) => AuthMode::MixedFlagWithoutAdcFile,
    };
    let accounts: Vec<AntigravityAccountItem> = vault
        .entries
        .iter()
        .map(|e| {
            let current = match e.kind {
                AccountKind::Oauth => {
                    entry_matches(e, oauth_digest.as_deref(), oauth_email.as_deref())
                }
                AccountKind::Adc => entry_matches(e, adc_digest.as_deref(), None),
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
                credential_revision: payload_digest(&e.payload),
            }
        })
        .collect();
    let mut state = AntigravityAuthState {
        accounts,
        mode,
        enable_adc,
        oauth_present: oauth_digest.is_some(),
        oauth_email,
        oauth_auth_method,
        adc_status,
        adc_type,
        adc_email,
        adc_project,
        app_restarted: false,
        revision: String::new(),
    };
    if let Some(id) = active_account_id(&state) {
        let up = state
            .accounts
            .iter()
            .find(|a| a.id == id)
            .map_or(0, |a| a.updated_at);
        dirty |= push_span(&mut vault.activations, &id, up, now_ms());
    }
    if dirty {
        vault_rev = save_vault(paths, &vault)?;
    }
    let cfg_rev = storage::revision(cfg_raw.as_deref());
    let oauth_rev = oauth_digest.as_deref().unwrap_or("none");
    state.revision = storage::digest(
        format!("{vault_rev}:{cfg_rev}:{oauth_rev}:{adc_status:?}:{adc_digest:?}").as_bytes(),
    );
    Ok((state, clean))
}
pub fn state_at(paths: &Paths) -> Result<AntigravityAuthState> {
    state_with_health(paths).map(|(s, _)| s)
}
fn verify_revision(paths: &Paths, expected: &str) -> Result<AntigravityAuthState> {
    let current = state_at(paths)?;
    (current.revision == expected)
        .then_some(current)
        .ok_or_else(|| err(code::CONFLICT, "Antigravity 凭据状态已变更，请刷新后重试"))
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
fn with_lifecycle(
    paths: &Paths,
    rev: &str,
    lc: &dyn AppLifecycle,
    mutate: impl FnOnce() -> Result<()>,
) -> Result<AntigravityAuthState> {
    with_lock(|| verify_revision(paths, rev).map(|_| ()))?;
    let was_running = lc.is_running()?;
    if was_running {
        lc.quit()?;
    }
    let next = with_lock(|| {
        verify_revision(paths, rev)?;
        let snaps: Vec<_> = paths
            .watched_files()
            .into_iter()
            .map(|f| storage::read_optional(&f).map(|b| (f, b)))
            .collect::<Result<_>>()?;
        if let Err(e) = mutate() {
            let res: Vec<_> = snaps
                .iter()
                .filter_map(|(f, p)| {
                    restore_file(f, p.as_deref())
                        .err()
                        .map(|_| f.display().to_string())
                })
                .collect();
            let msg = format!("{}（回滚残留未恢复：{}）", e.message, res.join(", "));
            return Err(if res.is_empty() {
                e
            } else {
                err(&e.code, &msg)
            });
        }
        state_at(paths)
    });
    if was_running {
        if next.is_ok() {
            lc.launch()?;
        } else {
            let _ = lc.launch();
        }
    }
    next.map(|mut s| {
        s.app_restarted = was_running;
        s
    })
}
fn write_active_adc(paths: &Paths, bytes: &[u8]) -> Result<()> {
    write_private_file(&paths.adc_file(), bytes)?;
    write_private_file(&paths.agswitch_adc_file(), bytes)?;
    restore_file(&paths.adc_disabled_file(), None)?;
    write_enable_adc(paths, true)
}
fn apply_mode_files(p: &Paths, enable_adc: bool, vault: &Vault) -> Result<()> {
    if !enable_adc {
        if let Some(adc_bytes) = storage::read_optional(&p.adc_file())? {
            write_private_file(&p.adc_disabled_file(), &adc_bytes)?;
            fs::remove_file(p.adc_file()).map_err(storage::io_error)?;
        }
        return write_enable_adc(p, false);
    }
    if !p.adc_file().exists() {
        let v_adc = || {
            vault
                .entries
                .iter()
                .find(|e| e.kind == AccountKind::Adc)
                .map(|e| e.payload.as_bytes().to_vec())
        };
        let src = storage::read_optional(&p.adc_disabled_file())?
            .or(storage::read_optional(&p.agswitch_adc_file())?)
            .or_else(v_adc)
            .ok_or_else(|| err(code::AUTH, "当前没有可用的 ADC 凭据，请先导入 ADC JSON"))?;
        let s = std::str::from_utf8(&src).map_err(|_| err(code::UTF8, "ADC 凭据编码无效"))?;
        parse_adc_metadata(s)?;
        return write_active_adc(p, &src);
    }
    write_enable_adc(p, true)
}
pub fn add_adc_at(
    paths: &Paths,
    lc: &dyn AppLifecycle,
    input: AddAntigravityAdcInput,
) -> Result<AntigravityAuthState> {
    let text = input.json_text.trim();
    if text.is_empty() {
        return Err(err(code::JSON, "请粘贴或导入有效的 ADC JSON 内容"));
    }
    let (cred_type, email, parsed_project, mut value) = parse_adc_metadata(text)?;
    let override_project = clean_opt(input.quota_project_id.as_deref());
    if let (Some(ref qp), Some(obj)) = (&override_project, value.as_object_mut()) {
        obj.insert("quota_project_id".into(), Value::String(qp.clone()));
    }
    let project_id = override_project.or(parsed_project);
    let pretty =
        serde_json::to_string_pretty(&value).map_err(|_| err(code::JSON, "无法格式化 ADC JSON"))?;
    let payload = format!("{pretty}\n");
    let digest = payload_digest(&payload);
    let label = adc_label(input.name.as_deref(), &email, &project_id)?;
    let (id, state) = with_lock(|| {
        let (mut vault, _) = load_vault(paths)?;
        let id = if let Some(ex) = vault
            .entries
            .iter_mut()
            .find(|e| e.kind == AccountKind::Adc && payload_digest(&e.payload) == digest)
        {
            if input.name.as_deref().is_some_and(|s| !s.trim().is_empty()) {
                ex.name = label;
            }
            ex.email = email;
            ex.project_id = project_id;
            ex.payload = payload;
            ex.updated_at = now_secs();
            ex.id.clone()
        } else {
            vault.push_new(
                label,
                AccountKind::Adc,
                cred_type.as_str(),
                (email, project_id),
                payload,
            )
        };
        vault.observed_revisions.retain(|r| r != &digest);
        save_vault(paths, &vault)?;
        Ok((id, state_at(paths)?))
    })?;
    if input.activate {
        switch_account_at(paths, lc, &id, &state.revision)
    } else {
        Ok(state)
    }
}
pub fn switch_account_at(
    paths: &Paths,
    lc: &dyn AppLifecycle,
    id: &str,
    rev: &str,
) -> Result<AntigravityAuthState> {
    let (vault, entry) = with_lock(|| {
        let (v, _) = load_vault(paths)?;
        let e = v.entries[v.index_of(id)?].clone();
        match e.kind {
            AccountKind::Oauth => parse_oauth_metadata(&e.payload).map(|_| ())?,
            AccountKind::Adc => parse_adc_metadata(&e.payload).map(|_| ())?,
        }
        Ok((v, e))
    })?;
    with_lifecycle(paths, rev, lc, || match entry.kind {
        AccountKind::Oauth => {
            write_private_file(&paths.oauth_file(), entry.payload.as_bytes())?;
            apply_mode_files(paths, false, &vault)
        }
        AccountKind::Adc => write_active_adc(paths, entry.payload.as_bytes()),
    })
}
pub fn set_mode_at(
    paths: &Paths,
    lc: &dyn AppLifecycle,
    mode: TargetMode,
    rev: &str,
) -> Result<AntigravityAuthState> {
    let enable_adc = matches!(mode, TargetMode::Adc);
    let vault = with_lock(|| {
        if !enable_adc && !paths.oauth_file().exists() {
            return Err(err(
                code::AUTH,
                "未检测到 Antigravity OAuth 登录令牌，请先在 Antigravity 内登录账号",
            ));
        }
        Ok(load_vault(paths)?.0)
    })?;
    with_lifecycle(paths, rev, lc, || {
        apply_mode_files(paths, enable_adc, &vault)
    })
}
pub fn rename_account_at(
    paths: &Paths,
    id: &str,
    name: &str,
    rev: &str,
) -> Result<AntigravityAuthState> {
    with_lock(|| {
        verify_revision(paths, rev)?;
        let (cleaned, (mut vault, _)) = (clean_label(Some(name), "")?, load_vault(paths)?);
        let idx = vault.index_of(id)?;
        vault.entries[idx].name = cleaned;
        vault.entries[idx].updated_at = now_secs();
        save_vault(paths, &vault)?;
        state_at(paths)
    })
}
pub fn delete_account_at(paths: &Paths, id: &str, rev: &str) -> Result<AntigravityAuthState> {
    with_lock(|| {
        if verify_revision(paths, rev)?
            .accounts
            .iter()
            .any(|a| a.id == id && a.current)
        {
            return Err(err(
                code::CONFLICT,
                "当前正在使用中的凭据不可直接删除，请先切换到其他凭据",
            ));
        }
        let (mut vault, _) = load_vault(paths)?;
        let removed = vault.entries.remove(vault.index_of(id)?);
        remember_observed(&mut vault, &payload_digest(&removed.payload));
        if vault.active_oauth_id.as_deref() == Some(id) {
            vault.active_oauth_id = None;
        }
        save_vault(paths, &vault)?;
        state_at(paths)
    })
}
pub fn disk_stamp() -> Option<String> {
    let mut stamp = String::new();
    for path in Paths::from_roots(&dirs::home_dir()?, Path::new("")).watched_files() {
        if let Ok(m) = fs::metadata(&path) {
            let ns = m.modified().ok().map_or(0, |t| epoch_dur(t).as_nanos());
            stamp.push_str(&format!("1:{}:{ns};", m.len()));
        } else {
            stamp.push_str("0;");
        }
    }
    Some(stamp)
}
pub fn active_account_id(state: &AntigravityAuthState) -> Option<String> {
    let target = if state.enable_adc {
        AccountKind::Adc
    } else {
        AccountKind::Oauth
    };
    state
        .accounts
        .iter()
        .find(|a| a.current && a.kind == target)
        .or_else(|| state.accounts.iter().find(|a| a.current))
        .map(|a| a.id.clone())
}
fn push_span(spans: &mut Vec<ActivationSpan>, id: &str, updated_at: u64, fb_ms: i64) -> bool {
    if spans.last().map(|s| s.account_id.as_str()) == Some(id) {
        return false;
    }
    let since_ms = if spans.is_empty() {
        (updated_at as i64).saturating_mul(1000)
    } else {
        fb_ms.max(spans.last().map_or(0, |s| s.since_ms))
    };
    spans.push(ActivationSpan {
        account_id: id.to_owned(),
        since_ms,
    });
    true
}
pub fn provider_timeline(data_dir: &Path, gemini_home: Option<&Path>) -> ProviderTimeline {
    let v_file = data_dir.join("antigravity-accounts.json");
    let Ok(Some(b)) = storage::read_bounded(&v_file, VAULT_LIMIT) else {
        return ProviderTimeline::default();
    };
    let Ok(vault) = serde_json::from_slice::<Vault>(&b) else {
        return ProviderTimeline::default();
    };
    let mut spans = vault.activations;
    if let Some(g) = gemini_home {
        let p = Paths {
            gemini_home: g.to_path_buf(),
            ..Paths::from_roots(g.parent().unwrap_or(g), data_dir)
        };
        let en = read_enable_adc(&p).is_ok_and(|(en, _, _)| en);
        let f = if en { p.adc_file() } else { p.oauth_file() };
        if let Some(raw) = read_text_optional(&f).ok().flatten() {
            let d = payload_digest(&raw);
            let em = (!en).then(|| parse_oauth_metadata(&raw).ok()?.1).flatten();
            if let Some(e) = vault.entries.iter().find(|e| {
                (e.kind == AccountKind::Adc) == en && entry_matches(e, Some(&d), em.as_deref())
            }) {
                let mtime = file_mtime_ms(&p.config_file()).or_else(|| file_mtime_ms(&f));
                push_span(
                    &mut spans,
                    &e.id,
                    e.updated_at,
                    mtime.unwrap_or_else(now_ms),
                );
            }
        }
    }
    ProviderTimeline { spans }
}
pub fn poll_state(data_dir: &Path) -> Result<(AntigravityAuthState, bool)> {
    with_lock(|| state_with_health(&Paths::resolve(data_dir)?))
}
async fn run_blocking(
    r: R<'_>,
    sync: bool,
    msg: &'static str,
    f: impl FnOnce(Paths, SystemAppLifecycle) -> Result<AntigravityAuthState> + Send + 'static,
) -> Result<AntigravityAuthState> {
    let rt = r.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        if sync {
            if let Ok(home) = crate::antigravity::home() {
                let roots = vec![("antigravity".into(), Ok(home))];
                let _ = rt.usage.sync_roots(roots, Some("antigravity"));
            }
        }
        f(Paths::resolve(&rt.data)?, SystemAppLifecycle::default())
    })
    .await
    .map_err(|_| err(code::PROCESS, msg))?
}
#[tauri::command]
pub fn get_antigravity_auth_state(r: R<'_>) -> Result<AntigravityAuthState> {
    poll_state(&r.data).map(|(s, _)| s)
}
#[tauri::command]
pub async fn add_antigravity_adc(
    r: R<'_>,
    input: AddAntigravityAdcInput,
) -> Result<AntigravityAuthState> {
    let (sync, msg) = (input.activate, "导入 ADC 任务异常中断");
    run_blocking(r, sync, msg, move |p, l| add_adc_at(&p, &l, input)).await
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
    .map_err(|_| err(code::DIALOG, "无法打开文件选择器"))?;
    let Some(fp) = picked else {
        return Ok(None);
    };
    let path = fp
        .into_path()
        .map_err(|_| err(code::PATH, "请选择本地文件"))?;
    let bytes = storage::read_bounded(&path, CREDENTIAL_LIMIT)?
        .ok_or_else(|| err(code::PATH, "所选文件不存在"))?;
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| err(code::UTF8, "所选文件不是 UTF-8 文本"))
}
#[tauri::command]
pub async fn switch_antigravity_account(
    r: R<'_>,
    id: String,
    expected_revision: String,
) -> Result<AntigravityAuthState> {
    let msg = "切换凭据任务异常中断";
    run_blocking(r, true, msg, move |p, l| {
        switch_account_at(&p, &l, &id, &expected_revision)
    })
    .await
}
#[tauri::command]
pub async fn set_antigravity_auth_mode(
    r: R<'_>,
    mode: TargetMode,
    expected_revision: String,
) -> Result<AntigravityAuthState> {
    let msg = "切换认证模式任务异常中断";
    run_blocking(r, true, msg, move |p, l| {
        set_mode_at(&p, &l, mode, &expected_revision)
    })
    .await
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
