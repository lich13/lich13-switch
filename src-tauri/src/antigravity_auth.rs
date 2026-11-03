use crate::{
    errors::code,
    storage::{self, AppError, Result},
    Runtime,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use tauri_plugin_dialog::DialogExt;

type R<'a> = tauri::State<'a, Arc<Runtime>>;

const VAULT_LIMIT: u64 = 4 * 1024 * 1024;
const CREDENTIAL_LIMIT: u64 = 256 * 1024;
const MAX_BACKUPS: usize = 10;

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

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredEntry {
    id: String,
    name: String,
    kind: String, // "oauth" | "adc"
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
    pub kind: String,
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
    pub mode: String, // "oauth" | "adc" | "both"
    pub enable_adc: bool,
    pub oauth_present: bool,
    pub oauth_saved: bool,
    pub adc_status: String, // "active" | "disabled" | "backup_only" | "missing"
    pub adc_saved: bool,
    pub adc_type: Option<String>,
    pub adc_project: Option<String>,
    pub backup_count: usize,
    pub revision: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddOAuthInput {
    pub name: Option<String>,
    pub input_mode: String, // "json" | "key"
    pub json_text: Option<String>,
    pub key: Option<String>,
    pub refresh_token: Option<String>,
    pub email: Option<String>,
    pub activate: bool,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddAdcInput {
    pub name: Option<String>,
    pub input_mode: String, // "json" | "key"
    pub json_text: Option<String>,
    pub key: Option<String>,
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    pub project_id: Option<String>,
    pub client_email: Option<String>,
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

fn clean_label(raw: &str, fallback: &str) -> Result<String> {
    let trimmed = raw.trim();
    let target = if trimmed.is_empty() {
        fallback
    } else {
        trimmed
    };
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

fn parse_oauth_metadata(raw: &str) -> (String, Option<String>) {
    let trimmed = raw.trim();
    if let Ok(Value::Object(map)) = serde_json::from_str::<Value>(trimmed) {
        let email = ["email", "account", "client_email", "user"]
            .into_iter()
            .find_map(|k| map.get(k).and_then(Value::as_str))
            .map(str::trim)
            .filter(|s| !s.is_empty() && !s.chars().any(char::is_control))
            .map(str::to_owned);
        return ("oauth_json".into(), email);
    }
    ("oauth_token".into(), None)
}

fn parse_adc_metadata(raw: &str) -> Result<(String, Option<String>, Option<String>)> {
    let value: Value = serde_json::from_str(raw.trim())
        .map_err(|_| AppError::new(code::JSON, "ADC 凭据必须是有效的 JSON 对象"))?;
    let Some(map) = value.as_object() else {
        return Err(AppError::new(
            code::CONFIG_TYPE,
            "ADC 凭据顶层必须是 JSON 对象",
        ));
    };
    let cred_type = map
        .get("type")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("authorized_user")
        .to_owned();
    let has_material = [
        "refresh_token",
        "private_key",
        "api_key",
        "credential_source",
    ]
    .into_iter()
    .any(|k| {
        map.get(k)
            .is_some_and(|v| v.as_str().is_some_and(|s| !s.trim().is_empty()) || v.is_object())
    });
    if !has_material {
        return Err(AppError::new(
            code::AUTH,
            "ADC JSON 缺少有效凭据字段（refresh_token / private_key / api_key）",
        ));
    }
    let email = ["client_email", "account", "email"]
        .into_iter()
        .find_map(|k| map.get(k).and_then(Value::as_str))
        .map(str::trim)
        .filter(|s| !s.is_empty() && !s.chars().any(char::is_control))
        .map(str::to_owned);
    let project = ["project_id", "quota_project_id"]
        .into_iter()
        .find_map(|k| map.get(k).and_then(Value::as_str))
        .map(str::trim)
        .filter(|s| !s.is_empty() && !s.chars().any(char::is_control))
        .map(str::to_owned);
    Ok((cred_type, email, project))
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

fn prune_backups(backups_root: &Path) -> Result<usize> {
    if !backups_root.exists() {
        return Ok(0);
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
        "active"
    } else if adc_disabled.is_some() {
        "disabled"
    } else if adc_agswitch.is_some() {
        "backup_only"
    } else {
        "missing"
    };

    let (adc_type, adc_project) = adc_effective
        .and_then(|raw| parse_adc_metadata(raw).ok())
        .map(|(t, _, p)| (Some(t), p))
        .unwrap_or((None, None));

    let accounts: Vec<AntigravityAccountItem> = vault
        .entries
        .iter()
        .map(|e| {
            let cred_rev = payload_digest(&e.payload);
            let current = if e.kind == "oauth" {
                oauth_digest.as_deref() == Some(cred_rev.as_str())
            } else {
                adc_digest.as_deref() == Some(cred_rev.as_str())
            };
            AntigravityAccountItem {
                id: e.id.clone(),
                name: e.name.clone(),
                kind: e.kind.clone(),
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
            .any(|a| a.kind == "oauth" && &a.credential_revision == d)
    });
    let adc_saved = adc_digest.as_ref().is_some_and(|d| {
        accounts
            .iter()
            .any(|a| a.kind == "adc" && &a.credential_revision == d)
    });

    let mode = if enable_adc && adc_active.is_some() && oauth_disk.is_some() {
        "both"
    } else if enable_adc && adc_active.is_some() {
        "adc"
    } else {
        "oauth"
    };

    let backup_count = prune_backups(&paths.backups_dir()).unwrap_or(0);
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
        mode: mode.into(),
        enable_adc,
        oauth_present: oauth_disk.is_some(),
        oauth_saved,
        adc_status: adc_status.into(),
        adc_saved,
        adc_type,
        adc_project,
        backup_count,
        revision: storage::digest(rev_material.as_bytes()),
    })
}

fn upsert_entry(
    paths: &Paths,
    kind: &str,
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
        if let Some(n) = name.filter(|s| !s.trim().is_empty()) {
            existing.name = clean_label(n, &existing.name)?;
        }
        if email.is_some() {
            existing.email = email;
        }
        if project_id.is_some() {
            existing.project_id = project_id;
        }
        existing.payload = payload;
        existing.updated_at = now;
        let id = existing.id.clone();
        save_vault(paths, &vault, Some(&vault_rev))?;
        return Ok(id);
    }
    let fallback = if kind == "oauth" {
        email.as_deref().unwrap_or("OAuth 账号")
    } else {
        project_id
            .as_deref()
            .or(email.as_deref())
            .unwrap_or("ADC 凭据")
    };
    let label = clean_label(name.unwrap_or(""), fallback)?;
    let id = uuid::Uuid::new_v4().to_string();
    vault.entries.push(StoredEntry {
        id: id.clone(),
        name: label,
        kind: kind.to_owned(),
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
            "未检测到当前 OAuth 令牌文件（~/.gemini/jetski-standalone-oauth-token）",
        )
    })?;
    let (source_type, email) = parse_oauth_metadata(&raw);
    upsert_entry(
        paths,
        "oauth",
        name.as_deref(),
        source_type,
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
        .ok_or_else(|| AppError::new(code::AUTH, "未检测到当前 ADC 凭据文件"))?;
    let (source_type, email, project_id) = parse_adc_metadata(&raw)?;
    upsert_entry(
        paths,
        "adc",
        name.as_deref(),
        source_type,
        email,
        project_id,
        raw,
    )?;
    state_at(paths)
}

pub fn add_oauth_at(paths: &Paths, input: AddOAuthInput) -> Result<AntigravityAuthState> {
    let (payload, source_type, email) = match input.input_mode.as_str() {
        "json" => {
            let text = input
                .json_text
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| AppError::new(code::JSON, "请粘贴或导入 OAuth JSON 内容"))?;
            let value: Value = serde_json::from_str(text)
                .map_err(|_| AppError::new(code::JSON, "OAuth 凭据必须是有效的 JSON"))?;
            if !value.is_object() {
                return Err(AppError::new(
                    code::CONFIG_TYPE,
                    "OAuth JSON 顶层必须是对象",
                ));
            }
            let normalized = format!(
                "{}\n",
                serde_json::to_string_pretty(&value)
                    .map_err(|_| AppError::new(code::JSON, "无法格式化 OAuth JSON"))?
            );
            let (_, parsed_email) = parse_oauth_metadata(&normalized);
            let final_email = input
                .email
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty() && !s.chars().any(char::is_control))
                .map(str::to_owned)
                .or(parsed_email);
            (normalized, "oauth_json".to_owned(), final_email)
        }
        "key" => {
            let key = input
                .key
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty() && !s.chars().any(char::is_control))
                .ok_or_else(|| AppError::new(code::KEY, "请输入有效的 OAuth Token / Key"))?;
            let email = input
                .email
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty() && !s.chars().any(char::is_control))
                .map(str::to_owned);
            let refresh = input
                .refresh_token
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty() && !s.chars().any(char::is_control));
            if let Some(refresh_token) = refresh {
                let mut obj = json!({
                    "access_token": key,
                    "refresh_token": refresh_token,
                    "token_type": "Bearer",
                });
                if let Some(ref em) = email {
                    obj["email"] = Value::String(em.clone());
                }
                let formatted = format!(
                    "{}\n",
                    serde_json::to_string_pretty(&obj)
                        .map_err(|_| AppError::new(code::JSON, "无法生成 OAuth 凭据"))?
                );
                (formatted, "oauth_key".to_owned(), email)
            } else if let Ok(Value::Object(_)) = serde_json::from_str::<Value>(key) {
                let (st, parsed_email) = parse_oauth_metadata(key);
                (format!("{key}\n"), st, email.or(parsed_email))
            } else {
                (format!("{key}\n"), "oauth_key".to_owned(), email)
            }
        }
        _ => return Err(AppError::new(code::CONFIG_TYPE, "不支持的 OAuth 录入方式")),
    };
    let id = upsert_entry(
        paths,
        "oauth",
        input.name.as_deref(),
        source_type,
        email,
        None,
        payload,
    )?;
    let state = state_at(paths)?;
    if input.activate {
        switch_oauth_at(paths, &id, &state.revision, true)
    } else {
        Ok(state)
    }
}

pub fn add_adc_at(paths: &Paths, input: AddAdcInput) -> Result<AntigravityAuthState> {
    let payload = match input.input_mode.as_str() {
        "json" => {
            let text = input
                .json_text
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| AppError::new(code::JSON, "请粘贴或导入 ADC JSON 内容"))?;
            let _ = parse_adc_metadata(text)?;
            let value: Value = serde_json::from_str(text)
                .map_err(|_| AppError::new(code::JSON, "ADC JSON 格式无效"))?;
            format!(
                "{}\n",
                serde_json::to_string_pretty(&value)
                    .map_err(|_| AppError::new(code::JSON, "无法格式化 ADC JSON"))?
            )
        }
        "key" => {
            let key = input
                .key
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    AppError::new(
                        code::KEY,
                        "请输入有效的 ADC Key（Refresh Token / Private Key / API Key）",
                    )
                })?;
            let project_id = input
                .project_id
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty() && !s.chars().any(char::is_control));
            let client_email = input
                .client_email
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty() && !s.chars().any(char::is_control));
            let client_id = input
                .client_id
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty() && !s.chars().any(char::is_control));
            let client_secret = input
                .client_secret
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty() && !s.chars().any(char::is_control));

            let obj = if client_email.is_some() || key.contains("BEGIN PRIVATE KEY") {
                let mut m = json!({
                    "type": "service_account",
                    "private_key": key,
                });
                if let Some(ce) = client_email {
                    m["client_email"] = Value::String(ce.into());
                }
                if let Some(pid) = project_id {
                    m["project_id"] = Value::String(pid.into());
                    m["quota_project_id"] = Value::String(pid.into());
                }
                m
            } else if client_id.is_some() || client_secret.is_some() {
                let mut m = json!({
                    "type": "authorized_user",
                    "refresh_token": key,
                });
                if let Some(cid) = client_id {
                    m["client_id"] = Value::String(cid.into());
                }
                if let Some(cs) = client_secret {
                    m["client_secret"] = Value::String(cs.into());
                }
                if let Some(pid) = project_id {
                    m["quota_project_id"] = Value::String(pid.into());
                }
                m
            } else {
                let mut m = json!({
                    "type": "authorized_user",
                    "api_key": key,
                    "refresh_token": key,
                });
                if let Some(pid) = project_id {
                    m["quota_project_id"] = Value::String(pid.into());
                    m["project_id"] = Value::String(pid.into());
                }
                m
            };
            format!(
                "{}\n",
                serde_json::to_string_pretty(&obj)
                    .map_err(|_| AppError::new(code::JSON, "无法生成 ADC JSON"))?
            )
        }
        _ => return Err(AppError::new(code::CONFIG_TYPE, "不支持的 ADC 录入方式")),
    };
    let (source_type, email, project_id) = parse_adc_metadata(&payload)?;
    let id = upsert_entry(
        paths,
        "adc",
        input.name.as_deref(),
        source_type,
        email,
        project_id,
        payload,
    )?;
    let state = state_at(paths)?;
    if input.activate {
        switch_adc_at(paths, &id, &state.revision, true)
    } else {
        Ok(state)
    }
}

fn restore_file(path: &Path, prev: Option<&[u8]>) {
    match prev {
        Some(bytes) => {
            let _ = storage::atomic_write(path, bytes, None);
        }
        None => {
            let _ = std::fs::remove_file(path);
        }
    }
}

pub fn switch_oauth_at(
    paths: &Paths,
    id: &str,
    expected_revision: &str,
    disable_adc: bool,
) -> Result<AntigravityAuthState> {
    let current = state_at(paths)?;
    if current.revision != expected_revision {
        return Err(AppError::new(
            code::CONFLICT,
            "Antigravity 凭据状态已变化，已刷新列表，请重试",
        ));
    }
    let (vault, _) = load_vault(paths)?;
    let entry = vault
        .entries
        .iter()
        .find(|e| e.id == id && e.kind == "oauth")
        .ok_or_else(|| AppError::new(code::ACCOUNT, "OAuth 账号不存在"))?;

    create_backup_at(paths)?;
    let prev_oauth = storage::read_optional(&paths.oauth_file())?;
    let prev_adc = storage::read_optional(&paths.adc_file())?;
    let prev_disabled = storage::read_optional(&paths.adc_disabled_file())?;
    let prev_cfg = storage::read_optional(&paths.config_file())?;

    let step = (|| -> Result<()> {
        storage::private_dir(&paths.gemini_home)?;
        storage::atomic_write(&paths.oauth_file(), entry.payload.as_bytes(), None)?;
        if disable_adc {
            if let Some(ref adc_bytes) = prev_adc {
                storage::private_dir(&paths.gcloud_dir)?;
                storage::atomic_write(&paths.adc_disabled_file(), adc_bytes, None)?;
                std::fs::remove_file(&paths.adc_file()).map_err(storage::io_error)?;
            }
            write_enable_adc(paths, false)?;
        }
        Ok(())
    })();
    if let Err(err) = step {
        restore_file(&paths.oauth_file(), prev_oauth.as_deref());
        restore_file(&paths.adc_file(), prev_adc.as_deref());
        restore_file(&paths.adc_disabled_file(), prev_disabled.as_deref());
        restore_file(&paths.config_file(), prev_cfg.as_deref());
        return Err(err);
    }
    state_at(paths)
}

pub fn switch_adc_at(
    paths: &Paths,
    id: &str,
    expected_revision: &str,
    enable_adc_in_config: bool,
) -> Result<AntigravityAuthState> {
    let current = state_at(paths)?;
    if current.revision != expected_revision {
        return Err(AppError::new(
            code::CONFLICT,
            "Antigravity 凭据状态已变化，已刷新列表，请重试",
        ));
    }
    let (vault, _) = load_vault(paths)?;
    let entry = vault
        .entries
        .iter()
        .find(|e| e.id == id && e.kind == "adc")
        .ok_or_else(|| AppError::new(code::ACCOUNT, "ADC 凭据不存在"))?;

    create_backup_at(paths)?;
    let prev_adc = storage::read_optional(&paths.adc_file())?;
    let prev_disabled = storage::read_optional(&paths.adc_disabled_file())?;
    let prev_agswitch = storage::read_optional(&paths.agswitch_adc_file())?;
    let prev_cfg = storage::read_optional(&paths.config_file())?;

    let step = (|| -> Result<()> {
        storage::private_dir(&paths.gcloud_dir)?;
        storage::private_dir(&paths.agswitch_dir)?;
        storage::atomic_write(&paths.adc_file(), entry.payload.as_bytes(), None)?;
        storage::atomic_write(&paths.agswitch_adc_file(), entry.payload.as_bytes(), None)?;
        if paths.adc_disabled_file().exists() {
            std::fs::remove_file(&paths.adc_disabled_file()).map_err(storage::io_error)?;
        }
        if enable_adc_in_config {
            write_enable_adc(paths, true)?;
        }
        Ok(())
    })();
    if let Err(err) = step {
        restore_file(&paths.adc_file(), prev_adc.as_deref());
        restore_file(&paths.adc_disabled_file(), prev_disabled.as_deref());
        restore_file(&paths.agswitch_adc_file(), prev_agswitch.as_deref());
        restore_file(&paths.config_file(), prev_cfg.as_deref());
        return Err(err);
    }
    state_at(paths)
}

pub fn set_mode_at(
    paths: &Paths,
    mode: &str,
    expected_revision: &str,
) -> Result<AntigravityAuthState> {
    let current = state_at(paths)?;
    if current.revision != expected_revision {
        return Err(AppError::new(
            code::CONFLICT,
            "Antigravity 凭据状态已变化，请刷新后重试",
        ));
    }
    if !matches!(mode, "oauth" | "adc" | "both") {
        return Err(AppError::new(code::CONFIG_TYPE, "不支持的认证模式"));
    }
    create_backup_at(paths)?;
    let prev_adc = storage::read_optional(&paths.adc_file())?;
    let prev_disabled = storage::read_optional(&paths.adc_disabled_file())?;
    let prev_agswitch = storage::read_optional(&paths.agswitch_adc_file())?;
    let prev_cfg = storage::read_optional(&paths.config_file())?;

    let step = (|| -> Result<()> {
        if mode == "oauth" {
            if let Some(ref adc_bytes) = prev_adc {
                storage::private_dir(&paths.gcloud_dir)?;
                storage::atomic_write(&paths.adc_disabled_file(), adc_bytes, None)?;
                std::fs::remove_file(&paths.adc_file()).map_err(storage::io_error)?;
            }
            write_enable_adc(paths, false)?;
        } else {
            if prev_adc.is_none() {
                let source_bytes = prev_disabled
                    .clone()
                    .or(prev_agswitch.clone())
                    .or_else(|| {
                        load_vault(paths).ok().and_then(|(v, _)| {
                            v.entries
                                .into_iter()
                                .find(|e| e.kind == "adc")
                                .map(|e| e.payload.into_bytes())
                        })
                    })
                    .ok_or_else(|| {
                        AppError::new(code::AUTH, "未找到可启用的 ADC 凭据，请先添加或导入 ADC")
                    })?;
                storage::private_dir(&paths.gcloud_dir)?;
                storage::private_dir(&paths.agswitch_dir)?;
                storage::atomic_write(&paths.adc_file(), &source_bytes, None)?;
                storage::atomic_write(&paths.agswitch_adc_file(), &source_bytes, None)?;
                if paths.adc_disabled_file().exists() {
                    std::fs::remove_file(&paths.adc_disabled_file()).map_err(storage::io_error)?;
                }
            }
            write_enable_adc(paths, true)?;
        }
        Ok(())
    })();
    if let Err(err) = step {
        restore_file(&paths.adc_file(), prev_adc.as_deref());
        restore_file(&paths.adc_disabled_file(), prev_disabled.as_deref());
        restore_file(&paths.agswitch_adc_file(), prev_agswitch.as_deref());
        restore_file(&paths.config_file(), prev_cfg.as_deref());
        return Err(err);
    }
    state_at(paths)
}

pub fn rename_at(
    paths: &Paths,
    id: &str,
    name: &str,
    expected_revision: &str,
) -> Result<AntigravityAuthState> {
    let current = state_at(paths)?;
    if current.revision != expected_revision {
        return Err(AppError::new(
            code::CONFLICT,
            "Antigravity 凭据状态已变化，请刷新后重试",
        ));
    }
    let cleaned = clean_label(name, "")?;
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

/// Recomputes live occupancy on the backend before deletion: refuses out-of-bounds IDs
/// and refuses deleting a credential that currently matches the active OAuth or active ADC file.
pub fn delete_at(paths: &Paths, id: &str, expected_revision: &str) -> Result<AntigravityAuthState> {
    let current = state_at(paths)?;
    if current.revision != expected_revision {
        return Err(AppError::new(
            code::CONFLICT,
            "Antigravity 凭据状态已变化，请刷新后重试",
        ));
    }
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
    let in_use = if target.kind == "oauth" {
        live_oauth.as_deref() == Some(target_digest.as_str())
    } else {
        live_adc.as_deref() == Some(target_digest.as_str())
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
pub fn add_antigravity_oauth(r: R<'_>, input: AddOAuthInput) -> Result<AntigravityAuthState> {
    add_oauth_at(&Paths::resolve(&r.data)?, input)
}

#[tauri::command]
pub fn add_antigravity_adc(r: R<'_>, input: AddAdcInput) -> Result<AntigravityAuthState> {
    add_adc_at(&Paths::resolve(&r.data)?, input)
}

#[tauri::command]
pub async fn pick_antigravity_credential_file(app: tauri::AppHandle) -> Result<Option<String>> {
    let picked = tauri::async_runtime::spawn_blocking(move || {
        app.dialog()
            .file()
            .add_filter("Credential file", &["json", "txt", "token"])
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
pub fn switch_antigravity_oauth(
    r: R<'_>,
    id: String,
    expected_revision: String,
    disable_adc: bool,
) -> Result<AntigravityAuthState> {
    switch_oauth_at(
        &Paths::resolve(&r.data)?,
        &id,
        &expected_revision,
        disable_adc,
    )
}

#[tauri::command]
pub fn switch_antigravity_adc(
    r: R<'_>,
    id: String,
    expected_revision: String,
    enable_adc: bool,
) -> Result<AntigravityAuthState> {
    switch_adc_at(
        &Paths::resolve(&r.data)?,
        &id,
        &expected_revision,
        enable_adc,
    )
}

#[tauri::command]
pub fn set_antigravity_auth_mode(
    r: R<'_>,
    mode: String,
    expected_revision: String,
) -> Result<AntigravityAuthState> {
    set_mode_at(&Paths::resolve(&r.data)?, &mode, &expected_revision)
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
