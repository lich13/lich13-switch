//! Codex-only official ChatGPT account mode.
//!
//! The mode owns only three standalone assignments in the existing
//! `model_providers.custom` table.  It comments those exact lines while the
//! official account is active and restores the original bytes when disabled.

use crate::errors::code;
use crate::storage::{self, AppError, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
};
use toml_edit::Document;

const FILE: &str = "official-mode.json";
const KEYS: [&str; 3] = [
    "base_url",
    "experimental_bearer_token",
    "supports_websockets",
];

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct OfficialModeView {
    pub enabled: bool,
    pub state: String,
    pub account_id: Option<String>,
    pub error: Option<String>,
}

impl Default for OfficialModeView {
    fn default() -> Self {
        Self {
            enabled: false,
            state: "disabled".into(),
            account_id: None,
            error: None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct FieldRecord {
    key: String,
    original: Option<String>,
    managed: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct Record {
    version: u32,
    enabled: bool,
    path: PathBuf,
    before_revision: String,
    managed_revision: Option<String>,
    fields: Vec<FieldRecord>,
    account_id: String,
    #[serde(default = "default_stage")]
    stage: String,
}

fn default_stage() -> String {
    "enabled".into()
}

#[derive(Clone)]
struct Line {
    start: usize,
    end: usize,
    body: String,
    raw: String,
}

struct Scan {
    lines: Vec<Line>,
    fields: Vec<Option<usize>>,
    comments: Vec<Option<usize>>,
}

fn lines(text: &str) -> Vec<Line> {
    let mut out = Vec::new();
    let mut start = 0;
    for (offset, ch) in text.char_indices() {
        if ch == '\n' {
            let end = offset + 1;
            let raw = text[start..end].to_owned();
            let body = raw
                .strip_suffix('\n')
                .unwrap_or(&raw)
                .strip_suffix('\r')
                .unwrap_or_else(|| raw.strip_suffix('\n').unwrap_or(&raw))
                .to_owned();
            out.push(Line {
                start,
                end,
                body,
                raw,
            });
            start = end;
        }
    }
    if start < text.len() {
        let raw = text[start..].to_owned();
        out.push(Line {
            start,
            end: text.len(),
            body: raw.clone(),
            raw,
        });
    }
    out
}

fn active_custom(text: &str) -> Result<()> {
    let doc = text
        .parse::<Document<String>>()
        .map_err(|_| AppError::new(code::TOML, "配置不是有效的 TOML，请先修复配置"))?;
    let profile = doc.get("profile").and_then(|v| v.as_str());
    let selector = profile
        .and_then(|p| doc.get("profiles")?.get(p)?.get("model_provider"))
        .or_else(|| doc.get("model_provider"))
        .and_then(|v| v.as_str());
    if selector != Some("custom") {
        return Err(AppError::new(
            code::CUSTOM,
            "当前生效的 provider 不是 custom，请在配置编辑器中处理",
        ));
    }
    let has_custom = doc
        .get("model_providers")
        .and_then(|v| v.get("custom"))
        .is_some_and(|v| v.as_table().is_some());
    if !has_custom {
        return Err(AppError::new(
            code::CUSTOM,
            "缺少现有 model_providers.custom，请在配置编辑器中处理",
        ));
    }
    Ok(())
}

fn assignment(body: &str, key: &str) -> bool {
    let trimmed = body.trim_start();
    if trimmed.starts_with('#') || !trimmed.starts_with(key) {
        return false;
    }
    let rest = &trimmed[key.len()..];
    rest.chars()
        .next()
        .is_some_and(|c| c.is_whitespace() || c == '=')
        && rest.trim_start().starts_with('=')
}

fn scan(text: &str) -> Result<Scan> {
    active_custom(text)?;
    let doc = text
        .parse::<Document<String>>()
        .map_err(|_| AppError::new(code::TOML, "配置不是有效的 TOML，请先修复配置"))?;
    let custom = doc
        .get("model_providers")
        .and_then(|v| v.get("custom"))
        .and_then(|v| v.as_table())
        .ok_or_else(|| AppError::new(code::OFFICIAL_MODE, "custom 不是独立表，无法安全接管"))?;
    let all = lines(text);
    let header = all
        .iter()
        .enumerate()
        .filter(|(_, line)| line.body.trim() == "[model_providers.custom]")
        .map(|(i, _)| i)
        .collect::<Vec<_>>();
    if header.len() != 1 {
        return Err(AppError::new(
            code::OFFICIAL_MODE,
            "custom 表必须是唯一的独立表，无法安全接管",
        ));
    }
    let start = header[0];
    let mut end = all.len();
    for (i, line) in all.iter().enumerate().skip(start + 1) {
        let t = line.body.trim();
        if t.starts_with('[') {
            end = i;
            break;
        }
    }
    let mut fields = vec![None; KEYS.len()];
    let mut comments = vec![None; KEYS.len()];
    for (n, key) in KEYS.iter().enumerate() {
        if let Some((parsed_key, item)) = custom.get_key_value(key) {
            if parsed_key.get() != *key {
                return Err(AppError::new(
                    code::OFFICIAL_MODE,
                    "受管字段使用了非标准键名",
                ));
            }
            let span = item
                .span()
                .ok_or_else(|| AppError::new(code::OFFICIAL_MODE, "无法定位受管字段"))?;
            let i = all
                .iter()
                .position(|line| span.start >= line.start && span.start < line.end)
                .ok_or_else(|| AppError::new(code::OFFICIAL_MODE, "无法定位受管字段行"))?;
            if i <= start || i >= end || span.end > all[i].end || !assignment(&all[i].body, key) {
                return Err(AppError::new(
                    code::OFFICIAL_MODE,
                    "受管字段必须是独立的单行赋值",
                ));
            }
            let valid = match *key {
                "supports_websockets" => item.as_value().and_then(|v| v.as_bool()).is_some(),
                _ => item.as_value().and_then(|v| v.as_str()).is_some(),
            };
            if !valid {
                return Err(AppError::new(code::OFFICIAL_MODE, "受管字段类型无效"));
            }
            fields[n] = Some(i);
        }
    }
    // Only recognize our own marker.  A generic commented assignment inside a
    // multiline string or a user's comment is never treated as managed state.
    for (i, line) in all.iter().enumerate().take(end).skip(start + 1) {
        let trimmed = line.body.trim_start();
        let Some(rest) = trimmed.strip_prefix("# lich13-switch:official ") else {
            continue;
        };
        for (n, key) in KEYS.iter().enumerate() {
            if assignment(rest, key) {
                if comments[n].is_some() || fields[n].is_some() {
                    return Err(AppError::new(
                        code::OFFICIAL_MODE,
                        "受管字段重复，无法安全接管",
                    ));
                }
                comments[n] = Some(i);
            }
        }
    }
    Ok(Scan {
        lines: all,
        fields,
        comments,
    })
}

fn comment_line(raw: &str) -> String {
    let newline = if raw.ends_with("\r\n") {
        "\r\n"
    } else if raw.ends_with('\n') {
        "\n"
    } else {
        ""
    };
    let body = raw.strip_suffix(newline).unwrap_or(raw);
    let indent = body.len() - body.trim_start().len();
    format!(
        "{}# lich13-switch:official {}{}",
        &body[..indent],
        &body[indent..],
        newline
    )
}

fn replace_lines(text: &str, scan: &Scan, replacements: &[(usize, String)]) -> String {
    let mut edits = replacements.to_vec();
    edits.sort_by_key(|(i, _)| std::cmp::Reverse(*i));
    let mut out = text.to_owned();
    for (i, value) in edits {
        out.replace_range(scan.lines[i].start..scan.lines[i].end, &value);
    }
    out
}

fn record_path(data: &Path) -> PathBuf {
    data.join(FILE)
}

fn load(data: &Path) -> Result<Option<Record>> {
    let Some(raw) = storage::read_optional(&record_path(data))? else {
        return Ok(None);
    };
    let record: Record = serde_json::from_slice(&raw)
        .map_err(|_| AppError::new(code::OFFICIAL_MODE, "官方账号事务记录损坏，请保留现场"))?;
    if record.version != 1
        || record.fields.len() != KEYS.len()
        || record.path.as_os_str().is_empty()
        || record
            .fields
            .iter()
            .enumerate()
            .any(|(i, field)| field.key != KEYS[i])
        || !["enabling", "enabled", "disabling", "conflict"].contains(&record.stage.as_str())
    {
        return Err(AppError::new(
            code::OFFICIAL_MODE,
            "官方账号事务记录格式无效，请保留现场",
        ));
    }
    Ok(Some(record))
}

fn save(data: &Path, record: &Record) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(record)
        .map_err(|_| AppError::new(code::OFFICIAL_MODE, "无法保存官方账号事务记录"))?;
    let prior = storage::read_optional(&record_path(data))?;
    storage::atomic_write(
        &record_path(data),
        &bytes,
        Some(&storage::revision(prior.as_deref())),
    )
}

fn verify_record(text: &str, record: &Record) -> Result<Scan> {
    let scan = scan(text)?;
    for (n, field) in record.fields.iter().enumerate() {
        match (
            &field.original,
            &field.managed,
            scan.fields[n],
            scan.comments[n],
        ) {
            (Some(_), Some(expected), None, Some(i)) if scan.lines[i].raw == *expected => (),
            (None, None, None, None) => (),
            _ => {
                return Err(AppError::new(
                    code::CONFLICT,
                    "官方账号受管字段已被外部修改，请先处理冲突",
                ))
            }
        }
    }
    Ok(scan)
}

pub fn view(data: &Path, home: &Path) -> OfficialModeView {
    let record = match load(data) {
        Ok(Some(record)) => record,
        Ok(None) => return OfficialModeView::default(),
        Err(e) => {
            return OfficialModeView {
                state: "unavailable".into(),
                error: Some(e.message),
                ..Default::default()
            }
        }
    };
    let raw = match storage::read_optional(&record.path) {
        Ok(Some(raw)) => raw,
        Ok(None) => {
            return OfficialModeView {
                enabled: record.enabled,
                state: "conflict".into(),
                account_id: Some(record.account_id),
                error: Some("Codex 配置不存在".into()),
            }
        }
        Err(e) => {
            return OfficialModeView {
                enabled: record.enabled,
                state: "unavailable".into(),
                account_id: Some(record.account_id),
                error: Some(e.message),
            }
        }
    };
    if record.path != home.join("config.toml") {
        return OfficialModeView {
            enabled: record.enabled,
            state: "conflict".into(),
            account_id: Some(record.account_id),
            error: Some("Codex 配置目录已变化".into()),
        };
    }
    let text = match String::from_utf8(raw) {
        Ok(v) => v,
        Err(_) => {
            return OfficialModeView {
                enabled: record.enabled,
                state: "conflict".into(),
                account_id: Some(record.account_id),
                error: Some("Codex 配置不是 UTF-8".into()),
            }
        }
    };
    if record.enabled && record.stage == "enabled" && verify_record(&text, &record).is_ok() {
        OfficialModeView {
            enabled: true,
            state: "enabled".into(),
            account_id: Some(record.account_id),
            error: None,
        }
    } else {
        OfficialModeView {
            enabled: record.enabled,
            state: "conflict".into(),
            account_id: Some(record.account_id),
            error: Some("官方账号配置已被外部修改".into()),
        }
    }
}

/// Both the old and retained IDs exist in the pre-migration vault. Updating
/// this reference before the vault is therefore safe even across interruption.
pub(crate) fn remap_account(
    data: &Path,
    aliases: &std::collections::BTreeMap<String, String>,
) -> Result<Option<(Vec<u8>, String)>> {
    if aliases.is_empty() {
        return Ok(None);
    }
    let path = data.join(FILE);
    let Some(before) = storage::read_optional(&path)? else {
        return Ok(None);
    };
    let mut value: serde_json::Value = serde_json::from_slice(&before)
        .map_err(|_| AppError::new(code::STORE, "官方账号记录无法读取，原文件已保留"))?;
    let Some(next) = value["accountId"].as_str().and_then(|id| aliases.get(id)) else {
        return Ok(None);
    };
    value["accountId"] = serde_json::Value::String(next.clone());
    let bytes = serde_json::to_vec_pretty(&value)
        .map_err(|_| AppError::new(code::STORE, "无法更新官方账号记录"))?;
    let expected = storage::digest(&before);
    let applied = storage::digest(&bytes);
    if let Err(error) = storage::atomic_write(&path, &bytes, Some(&expected)) {
        // A failed directory sync may follow a successful rename. Restore the
        // old reference only while the file still contains our exact write.
        let current = storage::read_optional(&path)?;
        let revision = storage::revision(current.as_deref());
        if revision == applied {
            storage::atomic_write(&path, &before, Some(&applied))?;
        } else if revision != expected {
            return Err(AppError::new(
                code::CONFLICT,
                "官方账号记录已被外部修改，未覆盖当前文件",
            ));
        }
        return Err(error);
    }
    Ok(Some((before, applied)))
}

pub fn blocks_gateway(data: &Path, home: &Path) -> bool {
    let _ = home;
    match load(data) {
        Ok(Some(_)) | Err(_) => true,
        Ok(None) => false,
    }
}

#[allow(dead_code)]
pub fn enable(
    data: &Path,
    home: &Path,
    account_id: &str,
    expected: &str,
) -> Result<OfficialModeView> {
    enable_with_switch(data, home, account_id, expected, || Ok(()))
}

pub fn enable_with_switch<F: FnOnce() -> Result<()>>(
    data: &Path,
    home: &Path,
    account_id: &str,
    expected: &str,
    switch_account: F,
) -> Result<OfficialModeView> {
    if load(data)?.is_some() {
        return Err(AppError::new(
            code::OFFICIAL_MODE,
            "已有官方账号事务，请先处理现有状态",
        ));
    }
    let path = home.join("config.toml");
    let prior = storage::read_optional(&path)?;
    if storage::revision(prior.as_deref()) != expected {
        return Err(AppError::new(
            code::CONFLICT,
            "Codex 配置已变化，请刷新后重试",
        ));
    }
    let raw = prior.ok_or_else(|| {
        AppError::new(
            code::CUSTOM,
            "缺少 config.toml，请先在配置编辑器中设置 custom",
        )
    })?;
    let text =
        String::from_utf8(raw).map_err(|_| AppError::new(code::CONFIG, "Codex 配置不是 UTF-8"))?;
    let scan = scan(&text)?;
    if scan.comments.iter().any(Option::is_some) {
        return Err(AppError::new(
            code::OFFICIAL_MODE,
            "受管字段已存在注释，无法安全接管",
        ));
    }
    let mut fields = Vec::with_capacity(KEYS.len());
    let mut replacements = Vec::new();
    for (n, key) in KEYS.iter().enumerate() {
        let original = scan.fields[n].map(|i| scan.lines[i].raw.clone());
        let managed = original.as_deref().map(comment_line);
        if let Some(i) = scan.fields[n] {
            replacements.push((i, managed.clone().unwrap_or_default()));
        }
        fields.push(FieldRecord {
            key: (*key).into(),
            original,
            managed,
        });
    }
    let output = replace_lines(&text, &scan, &replacements);
    let managed_revision = storage::revision(Some(output.as_bytes()));
    let mut record = Record {
        version: 1,
        enabled: false,
        path: path.clone(),
        before_revision: expected.into(),
        managed_revision: None,
        fields,
        account_id: account_id.into(),
        stage: "enabling".into(),
    };
    save(data, &record)?;
    if let Err(error) = storage::atomic_write(&path, output.as_bytes(), Some(expected)) {
        let _ = fs::remove_file(record_path(data));
        return Err(error);
    }
    let written = storage::read_optional(&path)?
        .ok_or_else(|| AppError::new(code::VERIFY, "官方配置写入后无法读取"))?;
    let written_text = String::from_utf8(written)
        .map_err(|_| AppError::new(code::VERIFY, "官方配置写入后不是 UTF-8"))?;
    if verify_record(&written_text, &record).is_err() {
        let rollback = restore_record(data, home, &record);
        let _ = fs::remove_file(record_path(data));
        return Err(rollback.err().unwrap_or_else(|| {
            AppError::new(code::VERIFY, "官方账号配置写入后校验失败，请处理现场")
        }));
    }
    if let Err(error) = switch_account() {
        match restore_record(data, home, &record) {
            Ok(()) => {
                let _ = fs::remove_file(record_path(data));
            }
            Err(rollback) => {
                record.stage = "conflict".into();
                let _ = save(data, &record);
                return Err(AppError::new(
                    code::CONFLICT,
                    &format!("账号切换失败，配置恢复失败：{}", rollback.message),
                ));
            }
        }
        return Err(error);
    }
    record.enabled = true;
    record.stage = "enabled".into();
    record.managed_revision = Some(managed_revision);
    if let Err(error) = save(data, &record) {
        match restore_record(data, home, &record) {
            Ok(()) => {
                let _ = fs::remove_file(record_path(data));
            }
            Err(rollback) => {
                record.stage = "conflict".into();
                let _ = save(data, &record);
                return Err(AppError::new(
                    code::CONFLICT,
                    &format!("官方模式记录失败，配置恢复失败：{}", rollback.message),
                ));
            }
        }
        return Err(error);
    }
    if view(data, home).enabled {
        Ok(view(data, home))
    } else {
        Err(AppError::new(
            code::VERIFY,
            "官方账号配置写入后校验失败，请处理现场",
        ))
    }
}

fn restore_record(_data: &Path, home: &Path, record: &Record) -> Result<()> {
    let path = home.join("config.toml");
    let raw = storage::read_optional(&path)?
        .ok_or_else(|| AppError::new(code::CONFLICT, "Codex 配置不存在，无法恢复"))?;
    let text = String::from_utf8(raw.clone())
        .map_err(|_| AppError::new(code::CONFIG, "Codex 配置不是 UTF-8"))?;
    let scan = verify_record(&text, record)?;
    let mut replacements = Vec::new();
    for (n, field) in record.fields.iter().enumerate() {
        if let (Some(i), Some(original)) = (scan.comments[n], &field.original) {
            replacements.push((i, original.clone()));
        }
    }
    let output = replace_lines(&text, &scan, &replacements);
    storage::atomic_write(
        &path,
        output.as_bytes(),
        Some(&storage::revision(Some(&raw))),
    )?;
    Ok(())
}

pub fn disable(data: &Path, home: &Path, expected: &str) -> Result<OfficialModeView> {
    let record =
        load(data)?.ok_or_else(|| AppError::new(code::OFFICIAL_MODE, "官方账号模式未启用"))?;
    let path = home.join("config.toml");
    if record.path != path {
        return Err(AppError::new(
            code::CONFLICT,
            "Codex 配置目录已变化，请处理官方模式冲突",
        ));
    }
    let raw = storage::read_optional(&path)?
        .ok_or_else(|| AppError::new(code::CONFLICT, "Codex 配置不存在，无法恢复"))?;
    let text =
        String::from_utf8(raw).map_err(|_| AppError::new(code::CONFIG, "Codex 配置不是 UTF-8"))?;
    let _ = expected; // unrelated edits are allowed; managed lines are checked below
    let managed_scan = verify_record(&text, &record)?;
    if record.stage == "conflict" {
        return Err(AppError::new(
            code::CONFLICT,
            "官方账号配置存在冲突，请先恢复受管字段",
        ));
    }
    let mut staged = record.clone();
    staged.stage = "disabling".into();
    save(data, &staged)?;
    let mut replacements = Vec::new();
    for (n, field) in record.fields.iter().enumerate() {
        if let (Some(i), Some(original)) = (managed_scan.comments[n], &field.original) {
            replacements.push((i, original.clone()));
        }
    }
    let output = replace_lines(&text, &managed_scan, &replacements);
    storage::atomic_write(
        &path,
        output.as_bytes(),
        Some(&storage::revision(Some(text.as_bytes()))),
    )?;
    let restored = storage::read_optional(&path)?
        .ok_or_else(|| AppError::new(code::VERIFY, "恢复后无法读取 Codex 配置"))?;
    let restored_text = String::from_utf8(restored)
        .map_err(|_| AppError::new(code::VERIFY, "恢复后配置不是 UTF-8"))?;
    let restored_scan = scan(&restored_text)?;
    for (n, field) in record.fields.iter().enumerate() {
        match (&field.original, restored_scan.fields[n]) {
            (Some(_), Some(_)) | (None, None) => {}
            _ => return Err(AppError::new(code::VERIFY, "官方账号配置恢复校验失败")),
        }
    }
    fs::remove_file(record_path(data)).map_err(storage::io_error)?;
    Ok(OfficialModeView::default())
}

pub fn guard_save(data: &Path, home: &Path, text: &str) -> Result<()> {
    let Some(record) = load(data)? else {
        return Ok(());
    };
    if !view(data, home).enabled {
        return Err(AppError::new(
            code::OFFICIAL_MODE,
            "官方账号事务存在冲突，请先关闭官方连接",
        ));
    }
    verify_record(text, &record).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    const TEXT: &str = "model_provider='custom'\r\n[model_providers.custom]\r\nbase_url = 'https://x.test' # keep\r\nexperimental_bearer_token = 'secret'\r\nsupports_websockets = true\r\nwire_api='responses'\r\n";
    #[test]
    fn comments_and_restores_exact_lines() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path().join("codex");
        let data = t.path().join("data");
        fs::create_dir_all(&home).unwrap();
        storage::private_dir(&data).unwrap();
        fs::write(home.join("config.toml"), TEXT).unwrap();
        let before = storage::revision(Some(TEXT.as_bytes()));
        let enabled = enable(&data, &home, "acct", &before).unwrap();
        assert!(enabled.enabled);
        let managed = fs::read_to_string(home.join("config.toml")).unwrap();
        assert!(managed.contains("# lich13-switch:official base_url = 'https://x.test' # keep"));
        disable(&data, &home, &storage::revision(Some(managed.as_bytes()))).unwrap();
        assert_eq!(fs::read_to_string(home.join("config.toml")).unwrap(), TEXT);
    }
    #[test]
    fn refuses_inline_custom_and_external_managed_edits() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path().join("codex");
        let data = t.path().join("data");
        fs::create_dir_all(&home).unwrap();
        storage::private_dir(&data).unwrap();
        let inline = "model_provider='custom'\nmodel_providers={custom={base_url='x',experimental_bearer_token='k'}}\n";
        fs::write(home.join("config.toml"), inline).unwrap();
        assert!(enable(
            &data,
            &home,
            "acct",
            &storage::revision(Some(inline.as_bytes()))
        )
        .is_err());
    }

    #[test]
    fn unrelated_edits_survive_enabled_mode_and_disable() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path().join("codex");
        let data = t.path().join("data");
        fs::create_dir_all(&home).unwrap();
        storage::private_dir(&data).unwrap();
        fs::write(home.join("config.toml"), TEXT).unwrap();
        let before = storage::revision(Some(TEXT.as_bytes()));
        enable(&data, &home, "acct", &before).unwrap();
        let mut changed = fs::read_to_string(home.join("config.toml")).unwrap();
        changed.push_str("\nuser_note = 'external'\n");
        fs::write(home.join("config.toml"), changed).unwrap();
        assert_eq!(view(&data, &home).state, "enabled");
        let revision = storage::revision(Some(&fs::read(home.join("config.toml")).unwrap()));
        disable(&data, &home, &revision).unwrap();
        let restored = fs::read_to_string(home.join("config.toml")).unwrap();
        assert!(restored.contains("base_url = 'https://x.test' # keep"));
        assert!(restored.contains("user_note = 'external'"));
    }

    #[test]
    fn rejects_multiline_controlled_assignment_and_rolls_back_failed_switch() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path().join("codex");
        let data = t.path().join("data");
        fs::create_dir_all(&home).unwrap();
        storage::private_dir(&data).unwrap();
        let multiline = "model_provider='custom'\n[model_providers.custom]\nbase_url = \"\"\"https://x.test\ncontinued\"\"\"\nexperimental_bearer_token = 'secret'\nsupports_websockets = true\n";
        fs::write(home.join("config.toml"), multiline).unwrap();
        assert!(enable(
            &data,
            &home,
            "acct",
            &storage::revision(Some(multiline.as_bytes()))
        )
        .is_err());

        fs::write(home.join("config.toml"), TEXT).unwrap();
        let before = storage::revision(Some(TEXT.as_bytes()));
        let error = enable_with_switch(&data, &home, "acct", &before, || {
            Err(AppError::new(code::ACCOUNT, "账号切换失败"))
        })
        .unwrap_err();
        assert_eq!(error.code, code::ACCOUNT);
        assert_eq!(fs::read_to_string(home.join("config.toml")).unwrap(), TEXT);
        assert!(!record_path(&data).exists());
    }
}
