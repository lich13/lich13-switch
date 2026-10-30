//! App configuration is independent of gateway clients and CLI configuration.
use crate::{
    auth_cache::Cache,
    configuration,
    errors::code,
    storage::{self, AppError, Result},
    Runtime,
};
use serde::Serialize;
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

type R<'a> = tauri::State<'a, Arc<Runtime>>;

const ENUM_PREFIXES: &[(&str, &str)] = &[
    ("themeMode", "THEME_MODE_"),
    ("conversationWidth", "CONVERSATION_WIDTH_"),
    ("artifactReviewMode", "ARTIFACT_REVIEW_MODE_"),
    ("autoExecutionPolicy", "CASCADE_COMMANDS_AUTO_EXECUTION_"),
    ("browserJsExecutionPolicy", "BROWSER_JS_EXECUTION_POLICY_"),
    ("nonWorkspaceFileAccessPolicy", "AGENT_SETTING_POLICY_"),
    ("permissionPreset", "AGENT_PERMISSION_PRESET_"),
];

fn valid_enum_value(value: &str, prefix: &str) -> bool {
    value.strip_prefix(prefix).is_some_and(|suffix| {
        !suffix.is_empty()
            && suffix
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
    })
}

const STRING_SETTINGS: &[&str] = &["remoteControlHostname"];

const BOOL_SETTINGS: &[&str] = &[
    "enableTerminalSandbox",
    "enableSoundsForSpecialEvents",
    "remoteControlEnabled",
    "permissionGrantsV2Migrated",
    "sandboxEnabledAtV2Migration",
];

pub fn home() -> Result<PathBuf> {
    dirs::home_dir()
        .map(|p| p.join(".gemini"))
        .ok_or_else(|| AppError::new(code::PATH, "无法定位 Antigravity 配置"))
}

fn backup(data: &Path) -> PathBuf {
    data.join("previous-antigravity-config.json")
}

#[cfg_attr(test, derive(Debug))]
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Document {
    client_id: &'static str,
    text: String,
    revision: String,
    path: String,
    guarded: bool,
    can_restore: bool,
}

pub fn validate_text(text: &str) -> Result<Value> {
    let value = configuration::parse_object(text)?;
    if value.get("plugins").is_some_and(|v| !v.is_object()) {
        return Err(AppError::new(code::CONFIG_TYPE, "plugins 必须是对象"));
    }
    if let Some(user_settings) = value.get("userSettings") {
        let Some(settings) = user_settings.as_object() else {
            return Err(AppError::new(code::CONFIG_TYPE, "userSettings 必须是对象"));
        };
        for (key, prefix) in ENUM_PREFIXES {
            if let Some(raw) = settings.get(*key) {
                let Some(val) = raw.as_str() else {
                    return Err(AppError::new(
                        code::CONFIG_TYPE,
                        &format!("userSettings.{key} 必须是字符串"),
                    ));
                };
                if !valid_enum_value(val, prefix) {
                    return Err(AppError::new(
                        code::CONFIG_TYPE,
                        &format!("userSettings.{key} 枚举值无效"),
                    ));
                }
            }
        }
        for key in STRING_SETTINGS {
            if settings.get(*key).is_some_and(|v| !v.is_string()) {
                return Err(AppError::new(
                    code::CONFIG_TYPE,
                    &format!("userSettings.{key} 必须是字符串"),
                ));
            }
        }
        for key in BOOL_SETTINGS {
            if settings.get(*key).is_some_and(|v| !v.is_boolean()) {
                return Err(AppError::new(
                    code::CONFIG_TYPE,
                    &format!("userSettings.{key} 必须是布尔值"),
                ));
            }
        }
    }
    Ok(value)
}

pub(crate) fn config_stamp(home: &Path, cache: &mut Cache<String>) -> Result<String> {
    let path = home.join("config/config.json");
    cache
        .read(&path, |raw| storage::revision(raw.as_deref()))
        .map(|rev| (*rev).clone())
}

fn read_at(home: &Path, data: &Path) -> Result<Document> {
    let path = home.join("config/config.json");
    let raw = storage::read_optional(&path)?;
    let revision = storage::revision(raw.as_deref());
    let text = String::from_utf8(raw.unwrap_or_else(|| b"{}\n".to_vec()))
        .map_err(|_| AppError::new(code::UTF8, "配置必须使用 UTF-8 编码"))?;
    Ok(Document {
        client_id: "antigravity",
        text,
        revision,
        path: path.to_string_lossy().into(),
        guarded: false,
        can_restore: backup(data).is_file(),
    })
}

fn save_at(home: &Path, data: &Path, text: &str, expected: &str) -> Result<Document> {
    validate_text(text)?;
    let path = home.join("config/config.json");
    let previous = storage::read_optional(&path)?;
    if storage::revision(previous.as_deref()) != expected {
        return Err(AppError::new(
            code::CONFLICT,
            "配置已被外部修改，草稿已保留",
        ));
    }
    if let Some(bytes) = previous {
        storage::atomic_write(&backup(data), &bytes, None)?;
    }
    // The shared atomic writer compares the bytes immediately before replacement.
    storage::atomic_write(&path, text.as_bytes(), Some(expected))?;
    read_at(home, data)
}

fn previous_at(data: &Path) -> Result<String> {
    let raw = storage::read_optional(&backup(data))?
        .ok_or_else(|| AppError::new(code::RESTORE, "没有可恢复的上次配置"))?;
    String::from_utf8(raw).map_err(|_| AppError::new(code::UTF8, "备份配置不是有效 UTF-8"))
}

#[tauri::command]
pub fn read_antigravity_config(r: R<'_>) -> Result<Document> {
    read_at(&home()?, &r.data)
}

#[tauri::command]
pub fn save_antigravity_config(
    r: R<'_>,
    text: String,
    expected_revision: String,
) -> Result<Document> {
    save_at(&home()?, &r.data, &text, &expected_revision)
}

#[tauri::command]
pub fn read_previous_antigravity_config(r: R<'_>) -> Result<String> {
    previous_at(&r.data)
}

#[tauri::command]
pub fn validate_antigravity_config(text: String) -> Result<()> {
    validate_text(&text).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_unknown_fields_and_rejects_external_changes() {
        let home = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let first = read_at(home.path(), data.path()).unwrap();
        assert!(!first.can_restore);
        assert_eq!(previous_at(data.path()).unwrap_err().code, code::RESTORE);

        let text = "{\"userSettings\":{\"themeMode\":\"THEME_MODE_DARK\",\"future\":false},\"plugins\":{\"example.invalid\":{\"enabled\":true}}}";
        let saved = save_at(home.path(), data.path(), text, &first.revision).unwrap();
        assert_eq!(saved.text, text);
        // First write created config.json from missing state, so no prior file existed yet.
        assert!(!saved.can_restore);

        let updated = "{\"userSettings\":{\"themeMode\":\"THEME_MODE_LIGHT\",\"enableTerminalSandbox\":true,\"future\":false},\"plugins\":{\"example.invalid\":{\"enabled\":true}}}";
        let saved_second = save_at(home.path(), data.path(), updated, &saved.revision).unwrap();
        assert!(saved_second.can_restore);
        assert_eq!(previous_at(data.path()).unwrap(), text);

        assert_eq!(
            save_at(home.path(), data.path(), "{}", &first.revision)
                .unwrap_err()
                .code,
            code::CONFLICT
        );
        assert_eq!(
            save_at(
                home.path(),
                data.path(),
                "{\"userSettings\":0}",
                &saved_second.revision
            )
            .unwrap_err()
            .code,
            code::CONFIG_TYPE
        );
        assert_eq!(read_at(home.path(), data.path()).unwrap().text, updated);
    }

    #[test]
    fn validates_known_field_types_enums_and_never_leaks_values_in_json_errors() {
        assert_eq!(
            validate_text("{\"plugins\":[]}").unwrap_err().code,
            code::CONFIG_TYPE
        );
        assert_eq!(
            validate_text("{\"userSettings\":{\"themeMode\":123}}")
                .unwrap_err()
                .code,
            code::CONFIG_TYPE
        );
        for invalid in [
            "{\"userSettings\":{\"themeMode\":\"dark\"}}",
            "{\"userSettings\":{\"themeMode\":\"THEME_MODE_\"}}",
            "{\"userSettings\":{\"themeMode\":\"THEME_MODE_dark\"}}",
            "{\"userSettings\":{\"conversationWidth\":\"wide\"}}",
            "{\"userSettings\":{\"artifactReviewMode\":\"always\"}}",
            "{\"userSettings\":{\"autoExecutionPolicy\":\"eager\"}}",
            "{\"userSettings\":{\"browserJsExecutionPolicy\":\"always_ask\"}}",
            "{\"userSettings\":{\"nonWorkspaceFileAccessPolicy\":\"allow\"}}",
            "{\"userSettings\":{\"permissionPreset\":\"turbo\"}}",
        ] {
            assert_eq!(validate_text(invalid).unwrap_err().code, code::CONFIG_TYPE);
        }
        assert!(validate_text(
            "{\"userSettings\":{\"themeMode\":\"THEME_MODE_INHERIT\",\"conversationWidth\":\"CONVERSATION_WIDTH_WIDE\",\"artifactReviewMode\":\"ARTIFACT_REVIEW_MODE_TURBO\",\"autoExecutionPolicy\":\"CASCADE_COMMANDS_AUTO_EXECUTION_EAGER\",\"browserJsExecutionPolicy\":\"BROWSER_JS_EXECUTION_POLICY_MODEL_DECIDES\",\"nonWorkspaceFileAccessPolicy\":\"AGENT_SETTING_POLICY_ALLOW\",\"permissionPreset\":\"AGENT_PERMISSION_PRESET_DEFAULT\"}}"
        )
        .is_ok());
        // Future official Antigravity enum variants with matching prefix remain valid and saveable.
        assert!(validate_text(
            "{\"userSettings\":{\"themeMode\":\"THEME_MODE_HIGH_CONTRAST_V2\",\"autoExecutionPolicy\":\"CASCADE_COMMANDS_AUTO_EXECUTION_SANDBOX_V3\",\"permissionPreset\":\"AGENT_PERMISSION_PRESET_STRICT_2027\"}}"
        )
        .is_ok());
        assert_eq!(
            validate_text("{\"userSettings\":{\"enableTerminalSandbox\":\"yes\"}}")
                .unwrap_err()
                .code,
            code::CONFIG_TYPE
        );
        let secret = "fixture-secret-token-value";
        let err = validate_text(&format!("{{\"token\":\"{secret}\",")).unwrap_err();
        assert_eq!(err.code, code::JSON);
        assert!(err.line.is_some());
        assert!(!err.message.contains(secret));
    }

    #[test]
    fn failed_backup_aborts_before_overwriting_live_config() {
        let home = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let first = read_at(home.path(), data.path()).unwrap();
        let initial = "{\"userSettings\":{\"themeMode\":\"THEME_MODE_DARK\"}}";
        let saved = save_at(home.path(), data.path(), initial, &first.revision).unwrap();

        // Replace backup file path with a directory so backup atomic_write fails.
        std::fs::create_dir_all(backup(data.path())).unwrap();
        let next = "{\"userSettings\":{\"themeMode\":\"THEME_MODE_LIGHT\"}}";
        assert!(save_at(home.path(), data.path(), next, &saved.revision).is_err());
        assert_eq!(read_at(home.path(), data.path()).unwrap().text, initial);
    }

    #[test]
    fn config_stamp_tracks_missing_and_updated_revisions() {
        let home = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let mut cache = Cache::default();
        let missing = config_stamp(home.path(), &mut cache).unwrap();
        let first = read_at(home.path(), data.path()).unwrap();
        assert_eq!(missing, first.revision);

        let saved = save_at(
            home.path(),
            data.path(),
            "{\"userSettings\":{\"conversationWidth\":\"CONVERSATION_WIDTH_WIDE\"}}",
            &first.revision,
        )
        .unwrap();
        let stamped = config_stamp(home.path(), &mut cache).unwrap();
        assert_eq!(stamped, saved.revision);
    }
}
