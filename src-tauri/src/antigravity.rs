//! App configuration is independent of gateway clients and CLI configuration.
use crate::{
    configuration,
    errors::code,
    storage::{self, AppError, Result},
};
use serde::Serialize;
use std::path::{Path, PathBuf};

pub fn home() -> Result<PathBuf> {
    dirs::home_dir()
        .map(|p| p.join(".gemini"))
        .ok_or_else(|| AppError::new(code::PATH, "无法定位 Antigravity 配置"))
}
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
fn read_at(home: &Path) -> Result<Document> {
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
        can_restore: false,
    })
}
fn save_at(home: &Path, text: &str, expected: &str) -> Result<Document> {
    let value = configuration::parse_object(text)?;
    if value.get("userSettings").is_some_and(|v| !v.is_object()) {
        return Err(AppError::new(code::CONFIG_TYPE, "userSettings 必须是对象"));
    }
    let path = home.join("config/config.json");
    // The shared atomic writer compares the bytes immediately before replacement.
    storage::atomic_write(&path, text.as_bytes(), Some(expected))?;
    read_at(home)
}
#[tauri::command]
pub fn read_antigravity_config() -> Result<Document> {
    read_at(&home()?)
}
#[tauri::command]
pub fn save_antigravity_config(text: String, expected_revision: String) -> Result<Document> {
    save_at(&home()?, &text, &expected_revision)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preserves_unknown_fields_and_rejects_external_changes() {
        let dir = tempfile::tempdir().unwrap();
        let first = read_at(dir.path()).unwrap();
        let text = "{\"userSettings\":{\"future\":false},\"plugins\":{\"example.invalid\":[]}}";
        let saved = save_at(dir.path(), text, &first.revision).unwrap();
        assert_eq!(saved.text, text);
        assert!(save_at(dir.path(), "{}", &first.revision).is_err());
        assert!(save_at(dir.path(), "{\"userSettings\":0}", &saved.revision).is_err());
        assert_eq!(read_at(dir.path()).unwrap().text, text);
    }
}
