//! Reversible Claude global profiles. OAuth stores never belong to this transaction.
mod catalog;
pub mod commands;
mod json;
pub(crate) mod login;
use crate::{
    configuration,
    storage::{self, AppError, Result},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};
const PROFILE: &str = "official-profile.json";
const JOURNAL: &str = "official-profile-transaction.json";
const LIMIT: u64 = 20 * 1024 * 1024;
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Official,
    Api,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
enum Role {
    Settings,
    Legacy,
    Plugin,
    Global,
}
impl Role {
    fn label(self) -> &'static str {
        match self {
            Self::Settings => "settings.json",
            Self::Legacy => "claude.json",
            Self::Plugin => "config.json",
            Self::Global => ".claude.json",
        }
    }
}
#[derive(Clone, Serialize, Deserialize)]
struct Profile {
    version: u32,
    home: PathBuf,
    mode: Mode,
    api_settings: Option<String>,
    api_legacy: Option<String>,
    official_settings: String,
    primary_key: Option<String>,
    api_approvals: Option<String>,
}
#[derive(Clone, Serialize, Deserialize)]
struct Change {
    role: Role,
    before: Option<String>,
    after: Option<String>,
}
#[derive(Serialize, Deserialize)]
struct Transaction {
    version: u32,
    home: PathBuf,
    before_profile: Option<String>,
    after_profile: String,
    changes: Vec<Change>,
}
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileView {
    role: &'static str,
    exists: bool,
    revision: String,
}
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct View {
    pub mode: Mode,
    pub revision: String,
    pub initialized: bool,
    pub conflict: Option<String>,
    pub files: Vec<FileView>,
    pub warnings: Vec<String>,
}
fn fail(message: &str) -> AppError {
    AppError::new("CLAUDE_PROFILE", message)
}
fn conflict() -> AppError {
    AppError::new(
        "CLAUDE_PROFILE_CONFLICT",
        "Claude 配置已被外部修改，请核对后重试",
    )
}
fn directory(data: &Path) -> PathBuf {
    data.join("claude")
}
fn path(home: &Path, user_home: &Path, role: Role) -> PathBuf {
    match role {
        Role::Settings => home.join("settings.json"),
        Role::Legacy => home.join("claude.json"),
        Role::Plugin => home.join("config.json"),
        Role::Global => {
            if home == user_home.join(".claude") {
                user_home.join(".claude.json")
            } else {
                home.join(".claude.json")
            }
        }
    }
}
fn read_text(path: &Path) -> Result<Option<String>> {
    storage::read_optional(path)?
        .map(|v| String::from_utf8(v).map_err(|_| fail("Claude 配置必须为 UTF-8")))
        .transpose()
}
fn revision(text: Option<&str>) -> String {
    storage::revision(text.map(str::as_bytes))
}
fn snapshot(home: &Path, user_home: &Path) -> Result<BTreeMap<Role, Option<String>>> {
    [Role::Settings, Role::Legacy, Role::Plugin, Role::Global]
        .into_iter()
        .map(|role| {
            let text = read_text(&path(home, user_home, role))?;
            if let Some(text) = &text {
                let value = configuration::parse_object(text)
                    .map_err(|e| fail(&format!("{}：{}", role.label(), e.message)))?;
                validate_role(role, &value)?;
            }
            Ok((role, text))
        })
        .collect()
}
fn validate_role(role: Role, value: &serde_json::Value) -> Result<()> {
    let valid = match role {
        Role::Settings | Role::Legacy => value.get("env").is_none_or(serde_json::Value::is_object),
        Role::Plugin => value
            .get("primaryApiKey")
            .is_none_or(serde_json::Value::is_string),
        Role::Global => {
            value.get("customApiKeyResponses").is_none_or(|v| {
                v.is_object()
                    && ["approved", "rejected"].iter().all(|k| {
                        v.get(k).is_none_or(|a| {
                            a.as_array()
                                .is_some_and(|a| a.iter().all(serde_json::Value::is_string))
                        })
                    })
            }) && value
                .get("hasCompletedOnboarding")
                .is_none_or(serde_json::Value::is_boolean)
        }
    };
    if !valid {
        return Err(fail(&format!("{}：相关字段类型无效", role.label())));
    }
    Ok(())
}
pub fn recovery_pending(data: &Path) -> bool {
    directory(data).join(JOURNAL).exists()
}
fn load(data: &Path, home: &Path) -> Result<Option<Profile>> {
    let bytes = storage::read_bounded(&directory(data).join(PROFILE), LIMIT)?;
    let profile = bytes
        .map(|b| {
            serde_json::from_slice::<Profile>(&b).map_err(|_| fail("Claude 配置档损坏，已保留现场"))
        })
        .transpose()?;
    if profile
        .as_ref()
        .is_some_and(|p| p.version != 1 || p.home != home)
    {
        return Err(conflict());
    }
    Ok(profile)
}
fn assert_official(files: &BTreeMap<Role, Option<String>>, p: &Profile) -> Result<()> {
    if files[&Role::Legacy].is_some() {
        return Err(conflict());
    }
    let settings = files[&Role::Settings].as_deref().ok_or_else(conflict)?;
    let value = configuration::parse_object(settings)?;
    // User may edit their official model/preferences. Authentication and routing
    // remain protected; baseline keys are compared, never all CLAUDE_CODE_*.
    let baseline = configuration::parse_object(&p.official_settings)?;
    let keys = catalog::connection_fields(&value);
    for key in keys {
        if catalog::official_preference(&key) {
            continue;
        }
        let ptr = format!("/{}", key.replace('.', "/"));
        if value.pointer(&ptr) != baseline.pointer(&ptr) {
            return Err(conflict());
        }
    }
    for (role, key) in [
        (Role::Plugin, "primaryApiKey"),
        (Role::Global, "customApiKeyResponses"),
    ] {
        if files[&role]
            .as_deref()
            .map(|s| json::field(s, key))
            .transpose()?
            .flatten()
            .is_some()
        {
            return Err(conflict());
        }
    }
    Ok(())
}
fn fingerprint(files: &BTreeMap<Role, Option<String>>, profile: Option<&[u8]>) -> String {
    let mut bytes = Vec::new();
    for text in files.values() {
        bytes.extend_from_slice(revision(text.as_deref()).as_bytes());
    }
    bytes.extend_from_slice(storage::revision(profile).as_bytes());
    storage::digest(&bytes)
}
pub fn view(data: &Path, home: &Path, user_home: &Path) -> Result<View> {
    let files = snapshot(home, user_home)?;
    let raw = storage::read_bounded(&directory(data).join(PROFILE), LIMIT)?;
    let p = load(data, home)?;
    let error = if directory(data).join(JOURNAL).exists() {
        Some("存在未完成的配置切换，请恢复事务".into())
    } else {
        p.as_ref()
            .filter(|p| p.mode == Mode::Official)
            .and_then(|p| assert_official(&files, p).err())
            .map(|e| e.message)
    };
    Ok(View {
        mode: p.as_ref().map(|p| p.mode).unwrap_or(Mode::Api),
        revision: fingerprint(&files, raw.as_deref()),
        initialized: p.is_some(),
        conflict: error,
        files: files
            .iter()
            .map(|(role, text)| FileView {
                role: role.label(),
                exists: text.is_some(),
                revision: revision(text.as_deref()),
            })
            .collect(),
        warnings: catalog::external(),
    })
}
pub fn blocks(data: &Path) -> bool {
    directory(data).join(JOURNAL).exists()
        || storage::read_bounded(&directory(data).join(PROFILE), LIMIT)
            .map(|b| {
                b.is_some_and(|b| {
                    serde_json::from_slice::<Profile>(&b)
                        .map(|p| p.mode == Mode::Official)
                        .unwrap_or(true)
                })
            })
            .unwrap_or(true)
}
pub fn guard_save(data: &Path, home: &Path, user_home: &Path, text: &str) -> Result<()> {
    if directory(data).join(JOURNAL).exists() {
        return Err(fail("请先恢复 Claude 配置切换事务"));
    }
    if let Some(p) = load(data, home)?.filter(|p| p.mode == Mode::Official) {
        let mut files = snapshot(home, user_home)?;
        files.insert(Role::Settings, Some(text.into()));
        assert_official(&files, &p)?;
    }
    Ok(())
}
fn set_file(path: &Path, before: Option<&str>, after: Option<&str>) -> Result<()> {
    let current = read_text(path)?;
    if current.as_deref() != before {
        return Err(conflict());
    }
    match after {
        Some(text) => storage::atomic_write(path, text.as_bytes(), Some(&revision(before))),
        None => {
            if before.is_some() {
                std::fs::remove_file(path).map_err(storage::io_error)?;
                #[cfg(unix)]
                std::fs::File::open(path.parent().ok_or_else(conflict)?)
                    .and_then(|f| f.sync_all())
                    .map_err(storage::io_error)?;
            }
            Ok(())
        }
    }?;
    if read_text(path)?.as_deref() != after {
        return Err(conflict());
    }
    Ok(())
}
fn write_profile(data: &Path, text: Option<&str>, expected: Option<&str>) -> Result<()> {
    let file = directory(data).join(PROFILE);
    match text {
        Some(text) => {
            storage::atomic_write_bounded(&file, text.as_bytes(), Some(&revision(expected)), LIMIT)
        }
        None => {
            if storage::read_bounded(&file, LIMIT)?.as_deref() != expected.map(str::as_bytes) {
                return Err(conflict());
            }
            if expected.is_some() {
                std::fs::remove_file(file).map_err(storage::io_error)?;
            }
            Ok(())
        }
    }
}
fn save_journal(data: &Path, tx: &Transaction) -> Result<()> {
    storage::private_dir(&directory(data))?;
    storage::atomic_write_bounded(
        &directory(data).join(JOURNAL),
        &serde_json::to_vec(tx).map_err(|_| fail("配置事务保存失败"))?,
        Some("missing"),
        LIMIT,
    )
}
fn rollback(data: &Path, user_home: &Path, tx: &Transaction) -> Result<()> {
    let mut failed = false;
    for step in tx.changes.iter().rev() {
        let target = path(&tx.home, user_home, step.role);
        match read_text(&target) {
            Ok(current) if current == step.before => (),
            Ok(current) if current == step.after => {
                if set_file(&target, current.as_deref(), step.before.as_deref()).is_err() {
                    failed = true
                }
            }
            _ => failed = true,
        }
    }
    let current = storage::read_bounded(&directory(data).join(PROFILE), LIMIT)?
        .map(|b| String::from_utf8(b).map_err(|_| conflict()))
        .transpose()?;
    if current.as_deref() == Some(&tx.after_profile) {
        if write_profile(data, tx.before_profile.as_deref(), current.as_deref()).is_err() {
            failed = true;
        }
    } else if current != tx.before_profile {
        failed = true;
    }
    if failed {
        return Err(AppError::new(
            "CLAUDE_PROFILE_CONFLICT",
            "部分配置发生外部变更，恢复记录已保留",
        ));
    }
    std::fs::remove_file(directory(data).join(JOURNAL)).map_err(storage::io_error)?;
    Ok(())
}
pub fn recover(data: &Path, home: &Path, user_home: &Path) -> Result<()> {
    let Some(bytes) = storage::read_bounded(&directory(data).join(JOURNAL), LIMIT)? else {
        return Ok(());
    };
    let tx: Transaction =
        serde_json::from_slice(&bytes).map_err(|_| fail("Claude 配置恢复记录无效"))?;
    if tx.version != 1 || tx.home != home {
        return Err(conflict());
    }
    rollback(data, user_home, &tx)
}
pub fn switch(
    data: &Path,
    home: &Path,
    user_home: &Path,
    target: Mode,
    expected: &str,
    logged_in: bool,
) -> Result<View> {
    let current = view(data, home, user_home)?;
    if current.revision != expected {
        return Err(conflict());
    }
    if current.conflict.is_some() {
        return Err(conflict());
    }
    if current.mode == target && current.initialized {
        return Ok(current);
    }
    let files = snapshot(home, user_home)?;
    let old = storage::read_bounded(&directory(data).join(PROFILE), LIMIT)?
        .map(|b| String::from_utf8(b).map_err(|_| fail("配置档无效")))
        .transpose()?;
    let mut p = load(data, home)?.unwrap_or(Profile {
        version: 1,
        home: home.into(),
        mode: Mode::Api,
        api_settings: None,
        api_legacy: None,
        official_settings: "{\n  \"env\": {}\n}\n".into(),
        primary_key: None,
        api_approvals: None,
    });
    let mut desired = files.clone();
    if target == Mode::Official {
        p.api_settings = files[&Role::Settings].clone();
        p.api_legacy = files[&Role::Legacy].clone();
        p.primary_key = files[&Role::Plugin]
            .as_deref()
            .map(|s| json::field(s, "primaryApiKey"))
            .transpose()?
            .flatten();
        p.api_approvals = files[&Role::Global]
            .as_deref()
            .map(|s| json::field(s, "customApiKeyResponses"))
            .transpose()?
            .flatten();
        desired.insert(Role::Settings, Some(p.official_settings.clone()));
        desired.insert(Role::Legacy, None);
        for (role, key) in [
            (Role::Plugin, "primaryApiKey"),
            (Role::Global, "customApiKeyResponses"),
        ] {
            if let Some(text) = &files[&role] {
                desired.insert(role, Some(json::set(text, key, None)?));
            }
        }
        // Remove only a stale skip-onboarding flag on first initialization. Never
        // reset a valid account's onboarding, machine ID, MCP or project trust.
        if old.is_none() && !logged_in {
            if let Some(text) = &desired[&Role::Global] {
                let v = configuration::parse_object(text)?;
                if v.get("oauthAccount").is_none() {
                    desired.insert(
                        Role::Global,
                        Some(json::set(text, "hasCompletedOnboarding", None)?),
                    );
                }
            }
        }
    } else {
        assert_official(&files, &p)?;
        p.official_settings = files[&Role::Settings].clone().ok_or_else(conflict)?;
        desired.insert(Role::Settings, p.api_settings.clone());
        desired.insert(Role::Legacy, p.api_legacy.clone());
        for (role, key, value) in [
            (Role::Plugin, "primaryApiKey", &p.primary_key),
            (Role::Global, "customApiKeyResponses", &p.api_approvals),
        ] {
            match &files[&role] {
                Some(text) => {
                    desired.insert(role, Some(json::set(text, key, value.as_deref())?));
                }
                None if value.is_some() => return Err(conflict()),
                None => (),
            }
        }
    }
    p.mode = target;
    let tx = Transaction {
        version: 1,
        home: home.into(),
        before_profile: old,
        after_profile: serde_json::to_string(&p).map_err(|_| fail("配置档无法保存"))?,
        changes: files
            .iter()
            .filter(|(role, before)| *before != &desired[role])
            .map(|(role, before)| Change {
                role: *role,
                before: before.clone(),
                after: desired[role].clone(),
            })
            .collect(),
    };
    save_journal(data, &tx)?;
    let result: Result<()> = (|| {
        for step in &tx.changes {
            set_file(
                &path(home, user_home, step.role),
                step.before.as_deref(),
                step.after.as_deref(),
            )?;
        }
        write_profile(data, Some(&tx.after_profile), tx.before_profile.as_deref())?;
        Ok(())
    })();
    if let Err(error) = result {
        rollback(data, user_home, &tx)?;
        return Err(error);
    }
    std::fs::remove_file(directory(data).join(JOURNAL)).map_err(storage::io_error)?;
    view(data, home, user_home)
}
pub fn user_home() -> Result<PathBuf> {
    dirs::home_dir().ok_or_else(|| fail("无法确定当前用户目录"))
}

#[cfg(test)]
mod tests;
