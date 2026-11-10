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

fn active_custom(doc: &Document<String>) -> Result<()> {
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
    let doc = text
        .parse::<Document<String>>()
        .map_err(|_| AppError::new(code::TOML, "配置不是有效的 TOML，请先修复配置"))?;
    active_custom(&doc)?;
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
        let Some(rest) = trimmed.strip_prefix("# vela:official ") else {
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
        "{}# vela:official {}{}",
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

fn parse_record(raw: &[u8]) -> Result<Record> {
    let record: Record = serde_json::from_slice(raw)
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
    Ok(record)
}

fn load(data: &Path) -> Result<Option<Record>> {
    storage::read_optional(&record_path(data))?
        .as_deref()
        .map(parse_record)
        .transpose()
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

fn verify_scan(scan: &Scan, record: &Record) -> Result<()> {
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
    Ok(())
}

fn verify_record(text: &str, record: &Record) -> Result<Scan> {
    let scan = scan(text)?;
    verify_scan(&scan, record)?;
    Ok(scan)
}

fn matches_original(scan: &Scan, record: &Record) -> bool {
    record.fields.iter().enumerate().all(|(n, field)| {
        match (
            &field.original,
            &field.managed,
            scan.fields[n],
            scan.comments[n],
        ) {
            (Some(expected), Some(_), Some(i), None) => scan.lines[i].raw == *expected,
            (None, None, None, None) => true,
            _ => false,
        }
    })
}

#[derive(Default)]
pub struct ViewCache {
    record: crate::auth_cache::Cache<Option<Result<Record>>>,
    config: crate::auth_cache::Cache<Option<Result<Scan>>>,
}

impl ViewCache {
    pub fn view(&mut self, data: &Path, home: &Path) -> OfficialModeView {
        let rec_entry = match self
            .record
            .read(&record_path(data), |raw| raw.as_deref().map(parse_record))
        {
            Ok(entry) => entry,
            Err(e) => {
                return OfficialModeView {
                    state: "unavailable".into(),
                    error: Some(e.message),
                    ..Default::default()
                }
            }
        };
        let record = match rec_entry.as_ref() {
            Some(Ok(record)) => record,
            None => return OfficialModeView::default(),
            Some(Err(e)) => {
                return OfficialModeView {
                    state: "unavailable".into(),
                    error: Some(e.message.clone()),
                    ..Default::default()
                }
            }
        };
        let cfg_entry = match self.config.read(&record.path, |raw| {
            raw.map(|bytes| {
                String::from_utf8(bytes)
                    .map_err(|_| AppError::new(code::CONFLICT, "Codex 配置不是 UTF-8"))
                    .and_then(|text| scan(&text))
            })
        }) {
            Ok(entry) => entry,
            Err(e) => {
                return OfficialModeView {
                    enabled: record.enabled,
                    state: "unavailable".into(),
                    account_id: Some(record.account_id.clone()),
                    error: Some(e.message),
                }
            }
        };
        let Some(cfg_res) = cfg_entry.as_ref() else {
            return OfficialModeView {
                enabled: record.enabled,
                state: "conflict".into(),
                account_id: Some(record.account_id.clone()),
                error: Some("Codex 配置不存在".into()),
            };
        };
        if record.path != home.join("config.toml") {
            return OfficialModeView {
                enabled: record.enabled,
                state: "conflict".into(),
                account_id: Some(record.account_id.clone()),
                error: Some("Codex 配置目录已变化".into()),
            };
        }
        let scan = match cfg_res {
            Ok(scan) => scan,
            Err(e) => {
                return OfficialModeView {
                    enabled: record.enabled,
                    state: "conflict".into(),
                    account_id: Some(record.account_id.clone()),
                    error: Some(if e.message == "Codex 配置不是 UTF-8" {
                        "Codex 配置不是 UTF-8".into()
                    } else {
                        "官方账号配置已被外部修改".into()
                    }),
                };
            }
        };
        if record.enabled && record.stage == "enabled" && verify_scan(scan, record).is_ok() {
            OfficialModeView {
                enabled: true,
                state: "enabled".into(),
                account_id: Some(record.account_id.clone()),
                error: None,
            }
        } else if matches!(record.stage.as_str(), "disabling" | "enabling")
            && matches_original(scan, record)
        {
            let _ = fs::remove_file(record_path(data));
            OfficialModeView::default()
        } else {
            OfficialModeView {
                enabled: record.enabled,
                state: "conflict".into(),
                account_id: Some(record.account_id.clone()),
                error: Some("官方账号配置已被外部修改".into()),
            }
        }
    }
}

fn view(data: &Path, home: &Path) -> OfficialModeView {
    ViewCache::default().view(data, home)
}

fn cleanup_settled_stage(data: &Path, record: &Record) -> bool {
    if matches!(record.stage.as_str(), "disabling" | "enabling")
        && storage::read_optional(&record.path)
            .ok()
            .flatten()
            .and_then(|raw| String::from_utf8(raw).ok())
            .and_then(|text| scan(&text).ok())
            .is_some_and(|scan| matches_original(&scan, record))
    {
        let _ = fs::remove_file(record_path(data));
        true
    } else {
        false
    }
}

pub fn blocks_gateway(data: &Path) -> bool {
    match load(data) {
        Ok(Some(record)) => !cleanup_settled_stage(data, &record),
        Err(_) => true,
        Ok(None) => false,
    }
}

#[cfg(test)]
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
    if let Some(record) = load(data)? {
        if !cleanup_settled_stage(data, &record) {
            return Err(AppError::new(
                code::OFFICIAL_MODE,
                "已有官方账号事务，请先处理现有状态",
            ));
        }
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
    if let Err(error) = switch_account() {
        match restore_record(home, &record) {
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
        match restore_record(home, &record) {
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
    Ok(OfficialModeView {
        enabled: true,
        state: "enabled".into(),
        account_id: Some(record.account_id),
        error: None,
    })
}

fn restore_record(home: &Path, record: &Record) -> Result<()> {
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
    let current_scan = scan(&text)?;
    if matches!(record.stage.as_str(), "disabling" | "enabling")
        && matches_original(&current_scan, &record)
    {
        let _ = fs::remove_file(record_path(data));
        return Ok(OfficialModeView::default());
    }
    verify_scan(&current_scan, &record)?;
    if record.stage == "conflict" {
        return Err(AppError::new(
            code::CONFLICT,
            "官方账号配置存在冲突，请先恢复受管字段",
        ));
    }
    let mut staged = record.clone();
    staged.stage = "disabling".into();
    let mut replacements = Vec::new();
    for (n, field) in record.fields.iter().enumerate() {
        if let (Some(i), Some(original)) = (current_scan.comments[n], &field.original) {
            replacements.push((i, original.clone()));
        }
    }
    let output = replace_lines(&text, &current_scan, &replacements);
    save(data, &staged)?;
    storage::atomic_write(
        &path,
        output.as_bytes(),
        Some(&storage::revision(Some(text.as_bytes()))),
    )?;
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
        assert!(managed.contains("# vela:official base_url = 'https://x.test' # keep"));
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

    #[test]
    fn view_cache_reuses_parsed_entries_and_invalidates_on_config_or_record_change() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path().join("codex");
        let data = t.path().join("data");
        fs::create_dir_all(&home).unwrap();
        storage::private_dir(&data).unwrap();
        let cfg_path = home.join("config.toml");
        fs::write(&cfg_path, TEXT).unwrap();
        let before = storage::revision(Some(TEXT.as_bytes()));
        enable(&data, &home, "acct", &before).unwrap();

        let mut cache = ViewCache::default();
        let first = cache.view(&data, &home);
        assert!(first.enabled);
        assert_eq!(first.state, "enabled");
        let rec_v1 = cache
            .record
            .read(&record_path(&data), |_| {
                panic!("record cache should hit without re-parsing")
            })
            .unwrap();
        let cfg_v1 = cache
            .config
            .read(&cfg_path, |_| {
                panic!("config cache should hit without re-parsing")
            })
            .unwrap();
        let second = cache.view(&data, &home);
        assert_eq!(second, first);
        let rec_v2 = cache
            .record
            .read(&record_path(&data), |_| {
                panic!("record cache should still hit")
            })
            .unwrap();
        let cfg_v2 = cache
            .config
            .read(&cfg_path, |_| panic!("config cache should still hit"))
            .unwrap();
        assert!(std::sync::Arc::ptr_eq(&rec_v1, &rec_v2));
        assert!(std::sync::Arc::ptr_eq(&cfg_v1, &cfg_v2));

        // External modification of controlled field invalidates config cache immediately.
        let tampered = format!(
            "model_provider = 'tampered'\n{}",
            fs::read_to_string(&cfg_path).unwrap()
        );
        storage::atomic_write(&cfg_path, tampered.as_bytes(), None).unwrap();
        let conflict = cache.view(&data, &home);
        assert_eq!(conflict.state, "conflict");
        let cfg_v3 = cache
            .config
            .read(&cfg_path, |_| {
                panic!("updated config cache should hit after refresh")
            })
            .unwrap();
        assert!(!std::sync::Arc::ptr_eq(&cfg_v1, &cfg_v3));

        // Removing official-mode.json invalidates record cache and returns disabled view.
        fs::remove_file(record_path(&data)).unwrap();
        let disabled = cache.view(&data, &home);
        assert!(!disabled.enabled);
        assert_eq!(disabled.state, "disabled");
    }

    #[test]
    fn recovers_from_interrupted_disabling_or_enabling_when_config_matches_original() {
        for stage in ["disabling", "enabling"] {
            let t = tempfile::tempdir().unwrap();
            let home = t.path().join("codex");
            let data = t.path().join("data");
            fs::create_dir_all(&home).unwrap();
            storage::private_dir(&data).unwrap();
            let cfg_path = home.join("config.toml");
            fs::write(&cfg_path, TEXT).unwrap();
            let before = storage::revision(Some(TEXT.as_bytes()));
            enable(&data, &home, "acct", &before).unwrap();
            let mut record = load(&data).unwrap().unwrap();
            record.stage = stage.into();
            record.enabled = stage == "disabling";
            save(&data, &record).unwrap();
            fs::write(&cfg_path, TEXT).unwrap();
            let restored = disable(&data, &home, &before).unwrap();
            assert!(!restored.enabled);
            assert_eq!(restored.state, "disabled");
            assert!(!record_path(&data).exists());

            enable(&data, &home, "acct", &before).unwrap();
            let mut record = load(&data).unwrap().unwrap();
            record.stage = stage.into();
            record.enabled = stage == "disabling";
            save(&data, &record).unwrap();
            fs::write(&cfg_path, TEXT).unwrap();
            let mut cache = ViewCache::default();
            let viewed = cache.view(&data, &home);
            assert!(!viewed.enabled);
            assert_eq!(viewed.state, "disabled");
            assert!(!record_path(&data).exists());

            enable(&data, &home, "acct", &before).unwrap();
            let mut record = load(&data).unwrap().unwrap();
            record.stage = stage.into();
            record.enabled = stage == "disabling";
            save(&data, &record).unwrap();
            fs::write(&cfg_path, TEXT).unwrap();
            assert!(!blocks_gateway(&data));
            assert!(!record_path(&data).exists());
        }
    }
}
