//! Read-only Antigravity app accounting. No conversation text or API headers persist.
use super::{
    model::*,
    pricing::Pricing,
    sessions::{Report, Repository},
};
use crate::storage::{self, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::{BufRead, BufReader, Seek, SeekFrom},
    path::{Path, PathBuf},
};

const LIMIT: usize = 32 * 1024 * 1024;
#[cfg(test)]
#[path = "antigravity_tests.rs"]
mod regression_tests;
#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct Cursor {
    modified: String,
    count: usize,
    covered: BTreeSet<u64>,
}
#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct FileCursor {
    offset: u64,
    tail: String,
    identity: String,
    modified: String,
    complete: bool,
}
fn file_identity(meta: &std::fs::Metadata) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        format!("{}:{}", meta.dev(), meta.ino())
    }
    #[cfg(not(unix))]
    {
        format!("{:?}", meta.created().ok())
    }
}
fn file_modified(meta: &std::fs::Metadata) -> Option<String> {
    let modified = meta.modified().ok()?;
    let d = modified.duration_since(std::time::UNIX_EPOCH).ok()?;
    Some(format!("{}:{}", d.as_secs(), d.subsec_nanos()))
}
fn file_settled(meta: &std::fs::Metadata) -> bool {
    meta.modified()
        .ok()
        .and_then(|m| std::time::SystemTime::now().duration_since(m).ok())
        .is_some_and(|age| age >= std::time::Duration::from_secs(2))
}
fn local_file_stamp(path: &Path) -> String {
    match std::fs::metadata(path) {
        Ok(meta) => format!(
            "local:{}:{}:{}",
            file_identity(&meta),
            meta.len(),
            file_modified(&meta).unwrap_or_default()
        ),
        Err(_) => "local:missing".into(),
    }
}
fn loopback_client() -> Result<reqwest::Client> {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    if let Some(client) = CLIENT.get() {
        return Ok(client.clone());
    }
    let built = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .danger_accept_invalid_certs(true)
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .map_err(|_| failure("Antigravity 本地连接初始化失败"))?;
    let _ = CLIENT.set(built.clone());
    Ok(built)
}
fn number(v: &Value) -> Option<u64> {
    v.as_u64()
        .or_else(|| v.as_str()?.parse().ok())
        .filter(|n| *n <= 1_000_000_000_000)
}
fn time(v: &Value) -> Option<i64> {
    v.as_str()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.timestamp_millis())
}
fn text<'a>(v: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|k| v[*k].as_str().filter(|s| !s.is_empty()))
}
fn duration(v: &Value) -> Option<u64> {
    let seconds = v.as_f64().or_else(|| {
        v.as_str()?
            .trim()
            .trim_end_matches('s')
            .trim()
            .parse::<f64>()
            .ok()
    })?;
    (seconds.is_finite() && (0.0..=u64::MAX as f64 / 1000.0).contains(&seconds))
        .then_some((seconds * 1000.0).round() as u64)
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ResolvedModelAlias {
    canonical: &'static str,
    display_name: &'static str,
    effort: Option<&'static str>,
}

fn resolve_model_alias(raw: Option<&str>) -> Option<ResolvedModelAlias> {
    let trimmed = raw?.trim();
    if trimmed.is_empty() {
        return None;
    }
    let key = trimmed.to_ascii_uppercase();
    let (canonical, display_name, effort) = match key.as_str() {
        "MODEL_PLACEHOLDER_M37" | "MODEL_GOOGLE_GEMINI_3_1_PRO_HIGH" => {
            ("gemini-3.1-pro", "Gemini 3.1 Pro", Some("high"))
        }
        "MODEL_PLACEHOLDER_M36" | "MODEL_GOOGLE_GEMINI_3_1_PRO_LOW" => {
            ("gemini-3.1-pro", "Gemini 3.1 Pro", Some("low"))
        }
        "MODEL_GOOGLE_GEMINI_3_1_PRO" => ("gemini-3.1-pro", "Gemini 3.1 Pro", None),
        "MODEL_PLACEHOLDER_M18"
        | "MODEL_GOOGLE_GEMINI_3_FLASH"
        | "MODEL_GOOGLE_GEMINI_3_0_FLASH" => ("gemini-3-flash", "Gemini 3 Flash", None),
        "MODEL_PLACEHOLDER_M19" | "MODEL_GOOGLE_GEMINI_3_PRO_HIGH" => {
            ("gemini-3-pro-preview", "Gemini 3 Pro", Some("high"))
        }
        "MODEL_PLACEHOLDER_M17" | "MODEL_GOOGLE_GEMINI_3_PRO_LOW" => {
            ("gemini-3-pro-preview", "Gemini 3 Pro", Some("low"))
        }
        "MODEL_GOOGLE_GEMINI_3_PRO" => ("gemini-3-pro-preview", "Gemini 3 Pro", None),
        "MODEL_PLACEHOLDER_M26" | "MODEL_CLAUDE_4_6_OPUS_THINKING" => {
            ("claude-opus-4-6", "Claude Opus 4.6", Some("high"))
        }
        "MODEL_CLAUDE_4_6_OPUS" => ("claude-opus-4-6", "Claude Opus 4.6", None),
        "MODEL_PLACEHOLDER_M35" | "MODEL_CLAUDE_4_6_SONNET" => {
            ("claude-sonnet-4-6", "Claude Sonnet 4.6", None)
        }
        "MODEL_CLAUDE_4_6_SONNET_THINKING" => {
            ("claude-sonnet-4-6", "Claude Sonnet 4.6", Some("high"))
        }
        "MODEL_PLACEHOLDER_M12" | "MODEL_CLAUDE_4_5_OPUS" => {
            ("claude-opus-4-5", "Claude Opus 4.5", None)
        }
        "MODEL_CLAUDE_4_5_OPUS_THINKING" => ("claude-opus-4-5", "Claude Opus 4.5", Some("high")),
        "MODEL_CLAUDE_4_5_SONNET" => ("claude-sonnet-4-5", "Claude Sonnet 4.5", None),
        "MODEL_CLAUDE_4_5_SONNET_THINKING" => {
            ("claude-sonnet-4-5", "Claude Sonnet 4.5", Some("high"))
        }
        "MODEL_CLAUDE_4_SONNET" => ("claude-sonnet-4-20250514", "Claude Sonnet 4", None),
        "MODEL_CLAUDE_4_SONNET_THINKING" => {
            ("claude-sonnet-4-20250514", "Claude Sonnet 4", Some("high"))
        }
        "MODEL_PLACEHOLDER_M8" | "MODEL_CHAT_23310" | "MODEL_GOOGLE_GEMINI_2_5_PRO" => {
            ("gemini-2.5-pro", "Gemini 2.5 Pro", None)
        }
        "MODEL_GOOGLE_GEMINI_2_5_PRO_THINKING" => {
            ("gemini-2.5-pro", "Gemini 2.5 Pro", Some("high"))
        }
        "MODEL_PLACEHOLDER_M7" | "MODEL_CHAT_20706" | "MODEL_GOOGLE_GEMINI_2_5_FLASH" => {
            ("gemini-2.5-flash", "Gemini 2.5 Flash", None)
        }
        "MODEL_GOOGLE_GEMINI_2_5_FLASH_THINKING" => {
            ("gemini-2.5-flash", "Gemini 2.5 Flash", Some("high"))
        }
        "MODEL_PLACEHOLDER_M9" | "MODEL_GOOGLE_GEMINI_2_5_FLASH_LITE" => {
            ("gemini-2.5-flash-lite", "Gemini 2.5 Flash Lite", None)
        }
        "MODEL_OPENAI_GPT_OSS_120B_MEDIUM" => ("gpt-oss-120b", "GPT-OSS 120B", Some("medium")),
        "MODEL_OPENAI_GPT_OSS_120B" => ("gpt-oss-120b", "GPT-OSS 120B", None),
        _ => return None,
    };
    Some(ResolvedModelAlias {
        canonical,
        display_name,
        effort,
    })
}

fn resolve_display_name_model(display_name: &str) -> Option<&'static str> {
    match display_name.trim().to_ascii_lowercase().as_str() {
        "gemini 3.1 pro" => Some("gemini-3.1-pro"),
        "gemini 3 flash" | "gemini 3.0 flash" => Some("gemini-3-flash"),
        "gemini 3 pro" | "gemini 3.0 pro" => Some("gemini-3-pro-preview"),
        "gemini 2.5 pro" => Some("gemini-2.5-pro"),
        "gemini 2.5 flash" => Some("gemini-2.5-flash"),
        "gemini 2.5 flash lite" | "gemini 2.5 flash-lite" => Some("gemini-2.5-flash-lite"),
        "claude opus 4.6" | "claude 4.6 opus" => Some("claude-opus-4-6"),
        "claude sonnet 4.6" | "claude 4.6 sonnet" => Some("claude-sonnet-4-6"),
        "claude opus 4.5" | "claude 4.5 opus" => Some("claude-opus-4-5"),
        "claude sonnet 4.5" | "claude 4.5 sonnet" => Some("claude-sonnet-4-5"),
        "claude sonnet 4" | "claude 4 sonnet" => Some("claude-sonnet-4-20250514"),
        "gpt-oss 120b" | "gpt oss 120b" => Some("gpt-oss-120b"),
        _ => None,
    }
}

fn non_enum_model(v: Option<&str>) -> Option<String> {
    model_id(v).filter(|s| {
        let upper = s.to_ascii_uppercase();
        !upper.starts_with("MODEL_PLACEHOLDER_")
            && !upper.starts_with("MODEL_UNSPECIFIED")
            && resolve_model_alias(Some(s)).is_none()
    })
}

fn actual_model(v: Option<&str>) -> Option<String> {
    if let Some(alias) = resolve_model_alias(v) {
        return Some(alias.canonical.to_owned());
    }
    model_id(v)
        .filter(|s| !s.starts_with("MODEL_PLACEHOLDER_") && !s.starts_with("MODEL_UNSPECIFIED"))
}
fn label(v: Option<&str>) -> Option<String> {
    v.map(str::trim)
        .filter(|s| {
            !s.is_empty()
                && s.len() <= 120
                && !s.chars().any(char::is_control)
                && model_id(Some(&s.replace([' ', '(', ')'], ""))).is_some()
        })
        .map(str::to_owned)
}
pub(super) fn normalize_attempt(a: &mut Attempt) -> bool {
    let mut changed = false;
    if let Some((name, effort)) = a.model_display_name.as_deref().and_then(|name| {
        let (name, suffix) = name.strip_suffix(')')?.rsplit_once(" (")?;
        let raw = suffix.to_ascii_lowercase();
        let effort = match raw.as_str() {
            "none" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max" | "ultra" => Some(raw),
            "thinking" => Some("high".to_owned()),
            _ => None,
        }?;
        (!name.is_empty()).then(|| (name.to_owned(), effort))
    }) {
        a.model_display_name = Some(name);
        a.reasoning_effort.get_or_insert(effort);
        changed = true;
    }
    let mut resolved_alias = None;
    if let Some(alias) = resolve_model_alias(a.requested_model.as_deref()) {
        a.requested_model = Some(alias.canonical.to_owned());
        resolved_alias = Some(alias);
        changed = true;
    }
    if let Some(alias) = resolve_model_alias(a.response_model.as_deref()) {
        a.response_model = Some(alias.canonical.to_owned());
        resolved_alias = Some(alias);
        changed = true;
    }
    if let Some(alias) = resolve_model_alias(a.pricing_model.as_deref()) {
        if a.price.is_none() {
            a.pricing_model = Some(alias.canonical.to_owned());
            changed = true;
        }
        resolved_alias.get_or_insert(alias);
    }
    if let Some(alias) = resolved_alias {
        if a.model_display_name.is_none() {
            a.model_display_name = Some(alias.display_name.to_owned());
            changed = true;
        }
        if a.reasoning_effort.is_none() {
            if let Some(effort) = alias.effort {
                a.reasoning_effort = Some(effort.to_owned());
                changed = true;
            }
        }
        if a.price.is_none() && a.pricing_model.is_none() {
            a.pricing_model = a
                .response_model
                .clone()
                .or_else(|| a.requested_model.clone());
            changed = true;
        }
    }
    if a.requested_model.is_none() && a.response_model.is_none() {
        if let Some(canonical) = a
            .model_display_name
            .as_deref()
            .and_then(resolve_display_name_model)
        {
            a.response_model = Some(canonical.to_owned());
            if a.price.is_none() && a.pricing_model.is_none() {
                a.pricing_model = Some(canonical.to_owned());
            }
            changed = true;
        }
    }
    if let Some(total) = a
        .first_token_ms
        .zip(a.generation_ms)
        .and_then(|(f, g)| f.checked_add(g))
    {
        if a.duration_ms != total
            || a.availability
                .get("duration")
                .is_none_or(|s| s != "reported")
        {
            a.duration_ms = total;
            a.availability.insert("duration".into(), "reported".into());
            a.parse_reasons.retain(|s| s != "attempt_time_unreported");
            changed = true;
        }
    } else if a.duration_ms == 0
        && a.availability
            .get("duration")
            .is_none_or(|s| s == "reported")
    {
        a.availability
            .insert("duration".into(), "upstream_unreported".into());
        if !a
            .parse_reasons
            .iter()
            .any(|s| s == "attempt_time_unreported")
        {
            a.parse_reasons.push("attempt_time_unreported".into());
        }
        changed = true;
    }

    changed
}

pub(super) fn normalize_record(r: &mut Record) -> bool {
    if r.source != "antigravity" {
        return false;
    }
    let mut changed = false;
    for a in &mut r.attempts {
        changed |= normalize_attempt(a);
    }
    if let Some(a) = r.final_attempt() {
        if r.finished_at.is_some_and(|end| end <= a.started_at) && a.duration_ms > 0 {
            if let Some(end) = i64::try_from(a.duration_ms)
                .ok()
                .and_then(|d| a.started_at.checked_add(d))
            {
                r.finished_at = Some(end);
                changed = true;
            }
        }
    }
    changed
}
#[derive(Default)]
struct StepTime {
    start: Option<i64>,
    generated: Option<i64>,
    end: Option<i64>,
}
#[derive(Default)]
struct Steps {
    times: BTreeMap<u64, StepTime>,
    executions: BTreeMap<String, Vec<u64>>,
}
impl Steps {
    fn extend(&mut self, offset: u64, steps: &[Value]) {
        for (i, step) in steps.iter().enumerate() {
            let index = offset + i as u64;
            if self.times.contains_key(&index) {
                continue;
            }
            let m = &step["metadata"];
            if let Some(id) = m["executionId"].as_str() {
                self.executions.entry(id.into()).or_default().push(index);
            }
            self.times.insert(
                index,
                StepTime {
                    start: time(&m["startedAt"]).or_else(|| time(&m["createdAt"])),
                    generated: time(&m["finishedGeneratingAt"]),
                    end: time(&m["completedAt"]),
                },
            );
        }
    }
    fn related(&self, entry: &Value) -> Vec<&StepTime> {
        let indices: Vec<_> = entry["stepIndices"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(number)
            .collect();
        let indices = if indices.is_empty() {
            entry["executionId"]
                .as_str()
                .and_then(|id| self.executions.get(id))
                .map(Vec::as_slice)
                .unwrap_or_default()
        } else {
            &indices
        };
        indices.iter().filter_map(|i| self.times.get(i)).collect()
    }
}
#[cfg(test)]
impl From<&[Value]> for Steps {
    fn from(values: &[Value]) -> Self {
        let mut result = Self::default();
        result.extend(0, values);
        result
    }
}
fn tokens(u: &Value) -> Tokens {
    Tokens {
        input: number(&u["inputTokens"]),
        output: number(&u["outputTokens"]),
        cache_read: number(&u["cacheReadTokens"]),
        cache_write: number(&u["cacheWriteTokens"]),
        cache_write_5m: number(&u["cacheWrite5mTokens"]),
        cache_write_1h: number(&u["cacheWrite1hTokens"]),
        reasoning: number(&u["thinkingOutputTokens"]),
        ..Tokens::default()
    }
}
fn price(record: &mut Record, pricing: &Pricing, settings: &Settings) {
    for a in &mut record.attempts {
        a.cost_multiplier = settings.multiplier.clone();
        a.pricing_model = a
            .response_model
            .clone()
            .or_else(|| a.requested_model.clone());
        a.price = pricing
            .quote_at(a.pricing_model.as_deref(), &a.cost_multiplier, a.started_at)
            .calculate(&a.tokens, a.price_tier());
        a.pricing_basis = Some("estimated_import_snapshot".into());
        a.annotate_availability("antigravity");
    }
}
fn response_id(u: &Value) -> Option<&str> {
    text(u, &["responseId", "messageId"])
}
fn failed(v: &Value) -> bool {
    v.get("error")
        .is_some_and(|e| !e.is_null() && e != "" && e != &json!({}))
}
fn error_code(v: &Value) -> Option<String> {
    let e = &v["error"];
    [&e["code"], &e["type"], &v["errorCode"]]
        .into_iter()
        .find_map(|code| {
            code.as_i64()
                .map(|n| n.to_string())
                .or_else(|| label(code.as_str()))
        })
}

/// Request IDs separate calls sharing one execution. A final usage envelope
/// may mirror that request's last retry; equal counts alone never merge calls.
fn record(
    cascade: &str,
    entry: &Value,
    steps: &Steps,
    generator_index: usize,
) -> Result<Option<Record>> {
    let Some(chat) = entry.get("chatModel") else {
        return Ok(None);
    };
    let request_key = text(&chat["customMetadata"], &["request_id"]);
    let Some(key) = request_key.or_else(|| text(entry, &["executionId"])) else {
        if chat.get("usage").is_some()
            || chat["retryInfos"].as_array().is_some_and(|a| !a.is_empty())
        {
            return Err(failure("Antigravity 用量缺少可靠请求标识，未计入账单"));
        }
        return Ok(None);
    };
    let related = steps.related(entry);
    let start = related
        .iter()
        .filter_map(|s| s.start)
        .min()
        .or_else(|| time(&chat["startedAt"]));
    let finish = related.iter().filter_map(|s| s.generated).max();
    let end = related.iter().filter_map(|s| s.end).max();
    let id = if request_key.is_some() {
        safe_id(&format!("antigravity:request:{key}"))
    } else {
        safe_id(&format!("antigravity:{cascade}:{key}:{generator_index}"))
    };
    // Keep the retry identity used by earlier imports. A matching final envelope
    // supplements that attempt; it never creates another charge.
    let mut usages: Vec<(&Value, &Value, String)> = Vec::new();
    for (i, retry) in chat["retryInfos"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
    {
        let u = retry.get("usage").unwrap_or(&Value::Null);
        let attempt_key = response_id(u)
            .or_else(|| text(retry, &["traceId"]))
            .map(str::to_owned)
            .unwrap_or_else(|| format!("retry:{i}"));
        if let Some(existing) = usages
            .iter_mut()
            .find(|(other, _, _)| response_id(u).is_some() && response_id(u) == response_id(other))
        {
            existing.0 = u;
            existing.1 = retry;
        } else {
            usages.push((u, retry, attempt_key));
        }
    }
    let mut final_index = None;
    if let Some(u) = chat.get("usage").filter(|u| u.is_object()) {
        final_index = usages.iter().position(|(other, _, _)| {
            response_id(u).is_some() && response_id(u) == response_id(other)
        });
        if final_index.is_none()
            && request_key.is_some()
            && usages.last().is_some_and(|(last, _, _)| *last == u)
        {
            final_index = usages.len().checked_sub(1);
        }
        if let Some(index) = final_index {
            usages[index].0 = u;
        } else {
            if !usages.is_empty() && response_id(u).is_none() {
                return Err(failure(
                    "Antigravity 重试用量缺少响应标识，无法核对完整账单",
                ));
            }
            final_index = Some(usages.len());
            usages.push((u, chat, "final".into()));
        }
    }
    if chat.get("usage").is_none_or(Value::is_null) {
        final_index = usages.len().checked_sub(1);
    }
    if usages.is_empty() {
        usages.push((&Value::Null, chat, "final".into()));
        final_index = Some(0);
    }
    if final_index.is_some_and(|i| i + 1 != usages.len()) {
        return Err(failure("Antigravity 最终响应与重试顺序不一致，未计入账单"));
    }
    let Some(start) = start else {
        return Err(failure("Antigravity 用量缺少请求时间，未计入账单"));
    };
    let single = usages.len() == 1;
    let mut r = Record {
        id,
        client: "antigravity".into(),
        source: "antigravity".into(),
        started_at: start,
        session_id: Some(safe_id(cascade)),
        ..Record::default()
    };
    for (index, (u, info, attempt_key)) in usages.into_iter().enumerate() {
        let final_info = final_index == Some(index);
        let field = |key: &str| {
            info.get(key)
                .filter(|v| !v.is_null())
                .or_else(|| final_info.then(|| chat.get(key)).flatten())
                .unwrap_or(&Value::Null)
        };
        let first = duration(field("timeToFirstToken"));
        let generation = duration(field("streamingDuration"));
        let attempt_start = time(field("startedAt")).or_else(|| single.then_some(start));
        let reported_end =
            time(field("completedAt")).or_else(|| final_info.then_some(finish).flatten());
        let explicit_end =
            reported_end.filter(|end| attempt_start.is_none_or(|start| *end > start));
        let elapsed = first
            .zip(generation)
            .and_then(|(f, g)| f.checked_add(g))
            .or_else(|| {
                attempt_start
                    .zip(explicit_end)
                    .and_then(|(s, e)| u64::try_from(e.checked_sub(s)?).ok())
            });
        let is_failed = failed(info) || (final_info && failed(chat));
        let completed_at = explicit_end
            .or_else(|| {
                attempt_start
                    .zip(elapsed)
                    .and_then(|(s, d)| s.checked_add(i64::try_from(d).ok()?))
            })
            // A completion marker still establishes the outcome when its
            // clock has insufficient precision to measure elapsed time.
            .or_else(|| reported_end.filter(|end| attempt_start.is_none_or(|start| *end >= start)))
            .or_else(|| is_failed.then_some(end).flatten());
        let mut tokens = tokens(u);
        tokens.fill_missing(&self::tokens(&info["usage"]));
        let outcome = if is_failed {
            "failed"
        } else if completed_at.is_some() && tokens.input.is_some() && tokens.output.is_some() {
            "success"
        } else if completed_at.is_some() {
            "unknown"
        } else {
            "in_progress"
        };
        let alias_hint = resolve_model_alias(text(u, &["model"]))
            .or_else(|| resolve_model_alias(text(&info["usage"], &["model"])))
            .or_else(|| resolve_model_alias(text(info, &["responseModel"])))
            .or_else(|| {
                final_info
                    .then(|| resolve_model_alias(text(chat, &["responseModel"])))
                    .flatten()
            })
            .or_else(|| resolve_model_alias(text(chat, &["model"])));
        let response_model = non_enum_model(text(u, &["model"]))
            .or_else(|| non_enum_model(text(&info["usage"], &["model"])))
            .or_else(|| non_enum_model(text(info, &["responseModel"])))
            .or_else(|| {
                final_info
                    .then(|| non_enum_model(text(chat, &["responseModel"])))
                    .flatten()
            })
            .or_else(|| actual_model(text(u, &["model"])))
            .or_else(|| actual_model(text(&info["usage"], &["model"])))
            .or_else(|| actual_model(text(info, &["responseModel"])))
            .or_else(|| {
                final_info
                    .then(|| actual_model(text(chat, &["responseModel"])))
                    .flatten()
            })
            .or_else(|| alias_hint.map(|alias| alias.canonical.to_owned()));
        let mut a = Attempt {
            id: safe_id(&format!("{}:{attempt_key}", r.id)),
            response_id: response_id(u).map(safe_id),
            upstream_request_hash: text(info, &["traceId"]).map(safe_id),
            thread_hash: r.session_id.clone(),
            requested_model: actual_model(text(chat, &["model"])),
            response_model,
            model_display_name: final_info
                .then(|| {
                    label(text(chat, &["modelDisplayName"]))
                        .or_else(|| alias_hint.map(|alias| alias.display_name.to_owned()))
                })
                .flatten(),
            reasoning_effort: alias_hint.and_then(|alias| alias.effort).map(str::to_owned),
            api_provider: label(text(u, &["apiProvider"]))
                .or_else(|| label(text(&info["usage"], &["apiProvider"]))),
            error_code: error_code(info).or_else(|| final_info.then(|| error_code(chat)).flatten()),
            started_at: attempt_start.unwrap_or(start),
            duration_ms: elapsed.unwrap_or(0),
            first_token_ms: first,
            generation_ms: generation,
            stream: first.is_some() || generation.is_some(),
            transport: "local_api".into(),
            outcome: outcome.into(),
            usage_status: if tokens.input.is_some() && tokens.output.is_some() {
                "reported"
            } else {
                "partial"
            }
            .into(),
            raw_input_tokens: tokens.input,
            input_basis: Some("exclusive".into()),
            output_reported: Some(tokens.output.is_some()),
            tokens,
            service_tier: text(u, &["serviceTier"])
                .or_else(|| text(info, &["serviceTier"]))
                .or_else(|| final_info.then(|| text(chat, &["serviceTier"])).flatten())
                .map(str::to_owned),
            ..Attempt::default()
        };
        a.availability.insert(
            "duration".into(),
            if elapsed.is_some() {
                "reported"
            } else {
                "upstream_unreported"
            }
            .into(),
        );
        a.availability.insert(
            "attempt_start".into(),
            if attempt_start.is_some() {
                "reported"
            } else {
                "upstream_unreported"
            }
            .into(),
        );
        if elapsed.is_none() {
            a.parse_reasons.push("attempt_time_unreported".into());
        }
        if a.tokens
            .reasoning
            .zip(a.tokens.output)
            .is_some_and(|(thinking, output)| thinking > output)
        {
            a.parse_reasons.push("reasoning_exceeds_output".into());
        }
        normalize_attempt(&mut a);
        r.finished_at = completed_at;
        r.attempts.push(a);
    }
    r.completed = r.final_attempt().is_some_and(|a| a.outcome == "success");
    Ok(Some(r))
}

#[derive(Clone)]
struct Endpoint {
    origin: String,
    token: String,
}

fn parse_page_origins(pages: &Value) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut origins = Vec::new();
    for origin in pages
        .as_array()
        .into_iter()
        .flatten()
        .filter(|p| p["type"] == "page")
        .filter_map(|p| url::Url::parse(p["url"].as_str()?).ok())
        .filter(|u| {
            u.scheme() == "https"
                && u.host_str() == Some("127.0.0.1")
                && u.port().is_some()
                && u.username().is_empty()
                && u.password().is_none()
        })
        .map(|u| u.origin().ascii_serialization())
    {
        if seen.insert(origin.clone()) {
            origins.push(origin);
        }
    }
    origins
}

fn extract_csrf_token(html: &str) -> Result<String> {
    let raw = html
        .split("window.__APP_CONFIG__")
        .nth(1)
        .and_then(|s| s.split_once('=').map(|(_, s)| s))
        .ok_or_else(|| failure("Antigravity app 接口版本不支持"))?;
    let config = serde_json::Deserializer::from_str(raw.trim())
        .into_iter::<Value>()
        .next()
        .and_then(std::result::Result::ok)
        .ok_or_else(|| failure("Antigravity app 接口配置无效"))?;
    config["csrfToken"]
        .as_str()
        .filter(|t| !t.is_empty() && t.len() < 4096)
        .ok_or_else(|| failure("Antigravity app 只读接口认证不可用"))
        .map(str::to_owned)
}

fn summary_freshness_wins(candidate: &Value, current: &Value) -> bool {
    let cand_time = time(&candidate["lastModifiedTime"]);
    let curr_time = time(&current["lastModifiedTime"]);
    match (cand_time, curr_time) {
        (Some(a), Some(b)) if a != b => return a > b,
        (Some(_), None) => return true,
        (None, Some(_)) => return false,
        _ => {
            let cand_str = candidate["lastModifiedTime"].as_str().unwrap_or("");
            let curr_str = current["lastModifiedTime"].as_str().unwrap_or("");
            if cand_str != curr_str {
                return cand_str > curr_str;
            }
        }
    }
    number(&candidate["stepCount"]).unwrap_or(0) > number(&current["stepCount"]).unwrap_or(0)
}

struct Api {
    http: reqwest::Client,
    endpoints: Vec<Endpoint>,
}
impl Api {
    #[cfg(test)]
    fn from_single(http: reqwest::Client, origin: String, token: String) -> Self {
        Self {
            http,
            endpoints: vec![Endpoint { origin, token }],
        }
    }
    #[cfg(test)]
    fn from_endpoints(http: reqwest::Client, endpoints: Vec<(String, String)>) -> Self {
        Self {
            http,
            endpoints: endpoints
                .into_iter()
                .map(|(origin, token)| Endpoint { origin, token })
                .collect(),
        }
    }
    async fn bytes(request: reqwest::RequestBuilder) -> Result<Vec<u8>> {
        let mut response = request
            .send()
            .await
            .map_err(|_| failure("Antigravity 本地只读接口不可用，请启动 app 后同步"))?;
        if !response.status().is_success() {
            return Err(failure("Antigravity 本地接口拒绝读取，未更新同步位置"));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| failure("Antigravity 本地响应未完整读取"))?
        {
            if bytes.len().saturating_add(chunk.len()) > LIMIT {
                return Err(failure("Antigravity 单页用量超过读取限制"));
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }
    async fn connect() -> Result<Option<Self>> {
        let port_path = dirs::config_dir()
            .ok_or_else(|| failure("无法定位 Antigravity app"))?
            .join("Antigravity/DevToolsActivePort");
        let Some(bytes) = storage::read_bounded(&port_path, 4096)? else {
            return Ok(None);
        };
        let port = std::str::from_utf8(&bytes)
            .ok()
            .and_then(|s| s.lines().next())
            .and_then(|s| s.parse::<u16>().ok())
            .filter(|p| *p > 0)
            .ok_or_else(|| failure("Antigravity 本地端口无效"))?;
        let http = loopback_client()?;
        let pages: Value = serde_json::from_slice(
            &Self::bytes(http.get(format!("http://127.0.0.1:{port}/json/list"))).await?,
        )
        .map_err(|_| failure("Antigravity 窗口信息无效"))?;
        let origins = parse_page_origins(&pages);
        if origins.is_empty() {
            return Err(failure("未找到 Antigravity app 窗口"));
        }
        let mut endpoints = Vec::with_capacity(origins.len());
        let mut last_error = None;
        for origin in origins {
            match Self::bytes(http.get(format!("{origin}/"))).await {
                Ok(raw_html) => match String::from_utf8(raw_html)
                    .map_err(|_| failure("Antigravity app 配置不可读"))
                    .and_then(|html| extract_csrf_token(&html))
                {
                    Ok(token) => endpoints.push(Endpoint { origin, token }),
                    Err(err) => last_error = Some(err),
                },
                Err(err) => last_error = Some(err),
            }
        }
        if endpoints.is_empty() {
            return Err(last_error.unwrap_or_else(|| failure("Antigravity app 只读接口认证不可用")));
        }
        Ok(Some(Self { http, endpoints }))
    }
    async fn call_on(&self, endpoint: &Endpoint, method: &str, body: &Value) -> Result<Value> {
        let bytes = Self::bytes(
            self.http
                .post(format!(
                    "{}/exa.language_server_pb.LanguageServerService/{method}",
                    endpoint.origin
                ))
                .header("Connect-Protocol-Version", "1")
                .header("x-codeium-csrf-token", &endpoint.token)
                .json(body),
        )
        .await?;
        serde_json::from_slice(&bytes).map_err(|_| failure("Antigravity 用量响应格式无效"))
    }
    async fn call_on_timeout(
        &self,
        endpoint: &Endpoint,
        method: &str,
        body: &Value,
        timeout: std::time::Duration,
    ) -> Result<Value> {
        let bytes = Self::bytes(
            self.http
                .post(format!(
                    "{}/exa.language_server_pb.LanguageServerService/{method}",
                    endpoint.origin
                ))
                .timeout(timeout)
                .header("Connect-Protocol-Version", "1")
                .header("x-codeium-csrf-token", &endpoint.token)
                .json(body),
        )
        .await?;
        serde_json::from_slice(&bytes).map_err(|_| failure("Antigravity 用量响应格式无效"))
    }
    #[cfg(test)]
    async fn collect(
        &self,
        store: &mut impl Repository,
        home: &Path,
        pricing: &Pricing,
        settings: &Settings,
        report: &mut Report,
        repair: bool,
    ) -> Result<()> {
        let brain = home.join("antigravity/brain");
        let files = collect_transcript_files(&brain)?;
        self.collect_with_files(store, &brain, &files, pricing, settings, report, repair)
            .await
    }
    #[allow(clippy::too_many_arguments)]
    async fn collect_with_files(
        &self,
        store: &mut impl Repository,
        brain: &Path,
        files: &[PathBuf],
        pricing: &Pricing,
        settings: &Settings,
        report: &mut Report,
        repair: bool,
    ) -> Result<()> {
        let mut summaries: BTreeMap<String, (Option<usize>, Value)> = BTreeMap::new();
        let mut any_endpoint_ok = false;
        let mut last_error = None;
        let req = json!({"excludeSubtrajectories":false});
        for (idx, endpoint) in self.endpoints.iter().enumerate() {
            match self
                .call_on(endpoint, "GetAllCascadeTrajectories", &req)
                .await
            {
                Ok(all) => {
                    let Some(map) = all["trajectorySummaries"].as_object() else {
                        last_error = Some(failure("Antigravity 会话目录不完整"));
                        continue;
                    };
                    any_endpoint_ok = true;
                    for (id, summary) in map {
                        match summaries.get(id) {
                            Some((_, existing)) if !summary_freshness_wins(summary, existing) => {}
                            _ => {
                                summaries.insert(id.clone(), (Some(idx), summary.clone()));
                            }
                        }
                    }
                }
                Err(err) => last_error = Some(err),
            }
        }
        if !any_endpoint_ok {
            return Err(last_error.unwrap_or_else(|| failure("Antigravity 会话目录不完整")));
        }
        for path in files {
            let id = cascade_id(brain, path)?;
            let stamp = local_file_stamp(path);
            summaries
                .entry(id.to_owned())
                .or_insert_with(|| (None, json!({"localModifiedStamp": stamp})));
        }
        report.total_files = summaries.len() as u64;
        for (id, (owner, summary)) in &summaries {
            match self
                .collect_session(
                    id, *owner, summary, store, pricing, settings, report, repair,
                )
                .await
            {
                Ok(()) => report.files += 1,
                Err(error) => {
                    if store.requires_complete_history() {
                        return Err(error);
                    }
                    report.errors += 1;
                }
            }
            store.progress(report);
        }
        Ok(())
    }
    #[allow(clippy::too_many_arguments)]
    async fn collect_session(
        &self,
        id: &str,
        owner: Option<usize>,
        summary: &Value,
        store: &mut impl Repository,
        pricing: &Pricing,
        settings: &Settings,
        report: &mut Report,
        repair: bool,
    ) -> Result<()> {
        let cursor_id = safe_id(&format!("antigravity:rpc:{id}"));
        let mut cursor: Cursor = if repair {
            Cursor::default()
        } else {
            store
                .cursor(&cursor_id)?
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_default()
        };
        let modified = summary["lastModifiedTime"]
            .as_str()
            .filter(|s| !s.is_empty())
            .or_else(|| summary["localModifiedStamp"].as_str())
            .unwrap_or("");
        if !modified.is_empty() && cursor.modified == modified {
            return Ok(());
        }
        let total = number(&summary["stepCount"]);
        if total.is_some_and(|n| n > 100_000) {
            return Err(failure("Antigravity 会话步骤超过读取限制"));
        }
        let mut active_endpoint = owner.unwrap_or(0);
        let mut resolved_owner = owner.is_some();
        let mut steps = Steps::default();
        let mut fallback_loaded = false;
        let mut offset = cursor.count.saturating_sub(1);
        let mut stable = offset;
        loop {
            let meta_req =
                json!({"cascadeId":id,"generatorMetadataOffset":offset,"includeMessages":false});
            let page = if resolved_owner || self.endpoints.len() <= 1 {
                resolved_owner = true;
                self.call_on(
                    &self.endpoints[active_endpoint],
                    "GetCascadeTrajectoryGeneratorMetadata",
                    &meta_req,
                )
                .await?
            } else {
                let mut selected_page = None;
                let mut empty_fallback = None;
                let mut last_err = None;
                for (idx, endpoint) in self.endpoints.iter().enumerate() {
                    match self
                        .call_on(endpoint, "GetCascadeTrajectoryGeneratorMetadata", &meta_req)
                        .await
                    {
                        Ok(candidate) => {
                            let non_empty = candidate["generatorMetadata"]
                                .as_array()
                                .is_some_and(|a| !a.is_empty());
                            if non_empty {
                                active_endpoint = idx;
                                resolved_owner = true;
                                selected_page = Some(candidate);
                                break;
                            }
                            if empty_fallback.is_none()
                                && (candidate["generatorMetadata"].is_array()
                                    || candidate.as_object().is_some_and(|o| o.is_empty()))
                            {
                                empty_fallback = Some((idx, candidate));
                            }
                        }
                        Err(err) => last_err = Some(err),
                    }
                }
                if let Some(found) = selected_page {
                    found
                } else if let Some((idx, empty)) = empty_fallback {
                    active_endpoint = idx;
                    resolved_owner = true;
                    empty
                } else {
                    return Err(
                        last_err.unwrap_or_else(|| failure("Antigravity 历史用量读取不完整"))
                    );
                }
            };
            let entries = match page["generatorMetadata"].as_array() {
                Some(entries) => entries,
                None if page.as_object().is_some_and(|o| o.is_empty()) => break,
                None => return Err(failure("Antigravity 历史用量读取不完整")),
            };
            if entries.is_empty() {
                break;
            }
            if offset.saturating_add(entries.len()) > 100_000 {
                return Err(failure("Antigravity 会话用量超过读取限制"));
            }
            let fallback = entries.iter().any(|e| {
                e.get("chatModel").is_some()
                    && e["stepIndices"].as_array().is_none_or(|a| a.is_empty())
            });
            let required: BTreeSet<_> = entries
                .iter()
                .flat_map(|e| {
                    e["stepIndices"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(number)
                })
                .collect();
            if required
                .last()
                .is_some_and(|i| *i >= 100_000 || total.is_some_and(|n| *i >= n))
            {
                return Err(failure("Antigravity 用量关联步骤不完整"));
            }
            let missing = if fallback && !fallback_loaded {
                Some(0)
            } else {
                required
                    .iter()
                    .find(|i| !steps.times.contains_key(i))
                    .copied()
            };
            if let Some(mut step_offset) = missing {
                loop {
                    let step_req =
                        json!({"cascadeId":id,"stepOffset":step_offset,"trajectoryVerbosity":2});
                    let page = self
                        .call_on(
                            &self.endpoints[active_endpoint],
                            "GetCascadeTrajectorySteps",
                            &step_req,
                        )
                        .await?;
                    let next = page["steps"]
                        .as_array()
                        .map(Vec::as_slice)
                        .or_else(|| page.as_object().filter(|o| o.is_empty()).map(|_| &[][..]))
                        .ok_or_else(|| failure("Antigravity 历史步骤读取不完整"))?;
                    if next.is_empty() {
                        if required.iter().all(|i| steps.times.contains_key(i))
                            && total.is_none_or(|n| step_offset >= n)
                        {
                            break;
                        }
                        return Err(failure("Antigravity 历史步骤读取不完整"));
                    }
                    if step_offset.saturating_add(next.len() as u64) > 100_000 {
                        return Err(failure("Antigravity 会话步骤超过读取限制"));
                    }
                    steps.extend(step_offset, next);
                    step_offset += next.len() as u64;
                    if required.iter().all(|i| steps.times.contains_key(i))
                        && (!fallback || total.is_some_and(|n| step_offset >= n))
                    {
                        break;
                    }
                }
            }
            fallback_loaded |= fallback;
            for batch in entries.chunks(128) {
                let mut rows = Vec::new();
                for entry in batch {
                    let parsed = match record(id, entry, &steps, offset) {
                        Ok(parsed) => parsed,
                        Err(error) => {
                            if store.requires_complete_history() {
                                return Err(error);
                            }
                            report.errors += 1;
                            offset += 1;
                            continue;
                        }
                    };
                    let complete = match parsed {
                        Some(mut r) => {
                            let complete = r.finished_at.is_some()
                                && r.final_attempt()
                                    .is_some_and(|a| a.outcome != "in_progress");
                            price(&mut r, pricing, settings);
                            rows.push(r);
                            complete
                        }
                        None => entry.get("chatModel").is_none(),
                    };
                    if complete {
                        if stable == offset {
                            stable += 1;
                        }
                        let indices: Vec<_> = entry["stepIndices"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(number)
                            .collect();
                        if indices.is_empty() {
                            if let Some(indices) = entry["executionId"]
                                .as_str()
                                .and_then(|id| steps.executions.get(id))
                            {
                                cursor.covered.extend(indices);
                            }
                        } else {
                            cursor.covered.extend(indices);
                        }
                    } else if entry.get("chatModel").is_some() {
                        if store.requires_complete_history() {
                            return Err(failure("Antigravity 历史用量尚未完整，请稍后重试"));
                        }
                        report.errors += 1;
                    }
                    offset += 1;
                }
                store.write_batch(&rows, None)?;
                report.imported += rows.len() as u64;
            }
        }
        // Commit only after the whole session is readable. Failed sessions keep
        // their previous cursor, so already saved batches are safely replayed.
        cursor.count = stable;
        cursor.modified = if stable == offset {
            modified.into()
        } else {
            String::new()
        };
        let cursor =
            serde_json::to_string(&cursor).map_err(|_| failure("Antigravity 同步位置无效"))?;
        store.write_batch(&[], Some((&cursor_id, &cursor)))
    }
}

fn cascade_id<'a>(brain: &Path, path: &'a Path) -> Result<&'a str> {
    path.strip_prefix(brain)
        .ok()
        .and_then(|p| p.components().next())
        .and_then(|c| c.as_os_str().to_str())
        .ok_or_else(|| failure("Antigravity 历史路径无效"))
}

fn collect_transcript_files(brain: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    transcript_files(brain, &mut files, 0)?;
    Ok(files)
}

fn should_skip_transcript_subdir(depth: usize, name: &std::ffi::OsStr) -> bool {
    depth >= 1
        && matches!(
            name.to_str(),
            Some("scratch" | "node_modules" | "target" | ".git")
        )
}

fn transcript_files(root: &Path, out: &mut Vec<PathBuf>, depth: usize) -> Result<()> {
    if !root.exists() {
        return Ok(());
    }
    if depth > 8 || out.len() > 30_000 {
        return Err(failure("Antigravity 历史目录超过读取限制"));
    }
    for item in std::fs::read_dir(root).map_err(storage::io_error)? {
        let item = item.map_err(storage::io_error)?;
        let kind = item.file_type().map_err(storage::io_error)?;
        let name = item.file_name();
        if kind.is_dir() {
            if !should_skip_transcript_subdir(depth, &name) {
                transcript_files(&item.path(), out, depth + 1)?;
            }
        } else if kind.is_file() && name == "transcript.jsonl" {
            out.push(item.path());
        }
    }
    Ok(())
}
#[cfg(test)]
pub(super) fn scan_files(
    store: &mut impl Repository,
    home: &Path,
    pricing: &Pricing,
    settings: &Settings,
    report: &mut Report,
    repair: bool,
) -> Result<()> {
    let brain = home.join("antigravity/brain");
    let files = collect_transcript_files(&brain)?;
    scan_file_list(store, &brain, &files, pricing, settings, report, repair)
}
fn scan_file_list(
    store: &mut impl Repository,
    brain: &Path,
    files: &[PathBuf],
    pricing: &Pricing,
    settings: &Settings,
    report: &mut Report,
    repair: bool,
) -> Result<()> {
    for path in files {
        if let Err(error) = scan_file(store, brain, path, pricing, settings, report, repair) {
            if store.requires_complete_history() {
                return Err(error);
            }
            report.errors += 1;
        }
    }
    Ok(())
}
#[allow(clippy::too_many_arguments)]
fn scan_file(
    store: &mut impl Repository,
    brain: &Path,
    path: &Path,
    pricing: &Pricing,
    settings: &Settings,
    report: &mut Report,
    repair: bool,
) -> Result<()> {
    let key = safe_id(&format!("antigravity:file:{}", path.to_string_lossy()));
    let saved_raw = if repair { None } else { store.cursor(&key)? };
    let mut c: FileCursor = saved_raw
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_default();
    let meta = std::fs::metadata(path).map_err(storage::io_error)?;
    let identity = file_identity(&meta);
    let modified = file_modified(&meta).unwrap_or_default();
    if !repair
        && c.complete
        && !modified.is_empty()
        && c.identity == identity
        && meta.len() == c.offset
        && c.modified == modified
        && file_settled(&meta)
    {
        return Ok(());
    }
    let cascade = cascade_id(brain, path)?;
    let rpc: Cursor = store
        .cursor(&safe_id(&format!("antigravity:rpc:{cascade}")))?
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let mut file = File::open(path).map_err(storage::io_error)?;
    let meta = file.metadata().map_err(storage::io_error)?;
    let identity = file_identity(&meta);
    let modified = file_modified(&meta).unwrap_or_default();
    let tail = |file: &mut File, offset: u64| -> Result<String> {
        use std::io::Read;
        file.seek(SeekFrom::Start(offset.saturating_sub(4096)))
            .map_err(storage::io_error)?;
        let mut b = vec![0; offset.min(4096) as usize];
        file.read_exact(&mut b).map_err(storage::io_error)?;
        Ok(storage::digest(&b))
    };
    let mut verified_tail = None;
    let valid_prefix = c.identity == identity && meta.len() >= c.offset && {
        let digest = tail(&mut file, c.offset)?;
        let ok = digest == c.tail;
        if ok {
            verified_tail = Some(digest);
        }
        ok
    };
    if !valid_prefix {
        c = FileCursor::default();
        verified_tail = None;
    }
    c.identity = identity;
    let start_offset = c.offset;
    file.seek(SeekFrom::Start(c.offset))
        .map_err(storage::io_error)?;
    let mut reader = BufReader::new(file);
    let mut rows = Vec::new();
    let mut bytes = Vec::new();
    let mut gap = false;
    let mut partial_tail = false;
    loop {
        bytes.clear();
        use std::io::Read;
        let size = (&mut reader)
            .take(LIMIT as u64 + 1)
            .read_until(b'\n', &mut bytes)
            .map_err(storage::io_error)?;
        if size == 0 {
            break;
        }
        if size > LIMIT {
            return Err(failure("Antigravity 历史单行超过读取限制"));
        }
        if bytes.last() != Some(&b'\n') {
            if store.requires_complete_history() {
                return Err(failure(
                    "Antigravity 历史仍在写入，未导出、未清理，请稍后重试",
                ));
            }
            partial_tail = true;
            break;
        }
        let parsed = (|| -> Result<Option<Record>> {
            let v: Value = serde_json::from_slice(&bytes)
                .map_err(|_| failure("Antigravity 历史文件存在无效完整行"))?;
            let has_usage = [
                "input_tokens",
                "output_tokens",
                "cache_read_tokens",
                "cache_write_tokens",
            ]
            .iter()
            .any(|k| number(&v[*k]).is_some());
            if has_usage && !number(&v["step_index"]).is_some_and(|i| rpc.covered.contains(&i)) {
                let request = text(&v, &["request_id"]);
                let model = text(&v, &["model", "response_model"]);
                let at = time(&v["completed_at"]).or_else(|| time(&v["created_at"]));
                let (Some(request), Some(model), Some(at)) = (request, model, at) else {
                    return Err(failure(
                        "Antigravity 历史用量缺少请求标识、模型或时间，请启动 app 后重新同步",
                    ));
                };
                let id = safe_id(&format!("antigravity:request:{request}"));
                let alias = resolve_model_alias(Some(model));
                let mut attempt = Attempt {
                    id: safe_id(&format!("{id}:final")),
                    response_id: text(&v, &["response_id", "message_id"]).map(safe_id),
                    requested_model: actual_model(Some(model)),
                    response_model: actual_model(Some(model)),
                    model_display_name: label(text(&v, &["model_display_name"]))
                        .or_else(|| alias.map(|a| a.display_name.to_owned())),
                    reasoning_effort: alias.and_then(|a| a.effort).map(str::to_owned),
                    started_at: at,
                    outcome: "unknown".into(),
                    transport: "history_file".into(),
                    tokens: Tokens {
                        input: number(&v["input_tokens"]),
                        output: number(&v["output_tokens"]),
                        cache_read: number(&v["cache_read_tokens"]),
                        cache_write: number(&v["cache_write_tokens"]),
                        cache_write_5m: number(&v["cache_write_5m_tokens"]),
                        cache_write_1h: number(&v["cache_write_1h_tokens"]),
                        reasoning: number(&v["thinking_output_tokens"]),
                        ..Tokens::default()
                    },
                    usage_status: "reported".into(),
                    parse_reasons: vec!["attempt_time_unreported".into()],
                    ..Attempt::default()
                };
                normalize_attempt(&mut attempt);
                let mut r = Record {
                    id: id.clone(),
                    client: "antigravity".into(),
                    source: "antigravity".into(),
                    session_id: Some(safe_id(cascade)),
                    started_at: at,
                    finished_at: Some(at),
                    // A transcript usage row reports consumption, not the model's
                    // terminal outcome. Only the API can supplement that evidence.
                    completed: false,
                    attempts: vec![attempt],
                    ..Record::default()
                };
                price(&mut r, pricing, settings);
                return Ok(Some(r));
            }
            Ok(None)
        })();
        match parsed {
            Ok(Some(row)) => rows.push(row),
            Ok(None) => {}
            Err(error) => {
                if store.requires_complete_history() {
                    return Err(error);
                }
                gap = true;
                report.errors += 1;
            }
        }
        if !gap {
            c.offset = reader.stream_position().map_err(storage::io_error)?;
        }
        if rows.len() >= 128 {
            store.write_batch(&rows, None)?;
            report.imported += rows.len() as u64;
            rows.clear();
        }
    }
    report.imported += rows.len() as u64;
    c.tail = match verified_tail.filter(|_| c.offset == start_offset) {
        Some(digest) => digest,
        None => tail(reader.get_mut(), c.offset)?,
    };
    c.complete = !gap && !partial_tail && c.offset == meta.len();
    c.modified = if c.complete { modified } else { String::new() };
    let cursor = serde_json::to_string(&c).map_err(|_| failure("Antigravity 历史同步位置无效"))?;
    if !rows.is_empty() || saved_raw.as_deref() != Some(cursor.as_str()) {
        store.write_batch(&rows, Some((&key, &cursor)))?;
    }
    Ok(())
}

pub fn sync(
    store: &mut impl Repository,
    pricing: &Pricing,
    settings: &Settings,
    home: &Path,
    repair: bool,
) -> Result<Report> {
    let mut report = Report {
        phase: "syncing".into(),
        ..Report::default()
    };
    let pricing = pricing.frozen();
    let brain = home.join("antigravity/brain");
    let files = collect_transcript_files(&brain)?;
    let runtime = tokio::runtime::Handle::try_current()
        .map_err(|_| failure("Antigravity 同步运行时不可用"))?;
    let connected = runtime.block_on(async {
        match Api::connect().await? {
            Some(api) => {
                api.collect_with_files(
                    store,
                    &brain,
                    &files,
                    &pricing,
                    settings,
                    &mut report,
                    repair,
                )
                .await?;
                Ok(true)
            }
            None if repair => Err(failure("请启动 Antigravity app 后补全历史")),
            None => Ok(false),
        }
    });
    // Already collected details remain available while the app is closed. Any
    // new uncorrelated file usage makes a complete export fail explicitly.
    scan_file_list(
        store,
        &brain,
        &files,
        &pricing,
        settings,
        &mut report,
        repair,
    )?;
    connected?;
    report.phase = if report.errors == 0 {
        "done"
    } else {
        "partial"
    }
    .into();
    report.completed_at = Some(now());
    Ok(report)
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AntigravityTokenMix {
    pub input: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub output: u64,
    pub visible_output: u64,
    pub reasoning: u64,
    pub total: u64,
    pub thinking_share_percent: Option<f64>,
    pub cache_hit_percent: Option<f64>,
    pub cache_read_eligible: u64,
    pub cache_input_eligible: u64,
    pub cache_savings_usd: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AntigravityTrackWindow {
    pub requests: u64,
    pub tokens: u64,
    pub reasoning_tokens: u64,
    pub cost_usd: String,
    pub unpriced_requests: u64,
    pub tokens_per_hour: f64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AntigravityLiveQuota {
    pub label: String,
    pub remaining_percent: Option<f64>,
    pub used_percent: Option<f64>,
    pub reset_at: Option<i64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AntigravityQuotaTrack {
    pub id: String,
    pub label: String,
    pub burst_5h: AntigravityTrackWindow,
    pub weekly_7d: AntigravityTrackWindow,
    pub live_quota: Option<AntigravityLiveQuota>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AntigravityFamilyGroup {
    pub family: String,
    pub label: String,
    pub requests: u64,
    pub tokens: u64,
    pub reasoning_tokens: u64,
    pub cost_usd: String,
    pub share_percent: f64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AntigravitySessionRow {
    pub session_id: String,
    pub short_hash: String,
    pub requests: u64,
    pub primary_model: String,
    pub family: String,
    pub total_tokens: u64,
    pub reasoning_tokens: u64,
    pub cache_hit_percent: Option<f64>,
    pub duration_ms: u64,
    pub cost_usd: String,
    pub unpriced_requests: u64,
    pub started_at: i64,
    pub last_active_at: i64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AntigravityUsageStats {
    pub requests: u64,
    pub sessions: u64,
    pub cost_usd: String,
    pub unpriced_requests: u64,
    pub token_mix: AntigravityTokenMix,
    pub tracks: Vec<AntigravityQuotaTrack>,
    pub families: Vec<AntigravityFamilyGroup>,
    pub top_sessions: Vec<AntigravitySessionRow>,
    pub live_quotas: Vec<AntigravityLiveQuota>,
    pub live_connected: bool,
}

fn classify_model_family(
    model: Option<&str>,
    display_name: Option<&str>,
) -> (
    Option<String>,
    &'static str,
    &'static str,
    Option<&'static str>,
) {
    let resolved = model
        .and_then(|m| {
            resolve_model_alias(Some(m))
                .map(|a| a.canonical.to_owned())
                .or_else(|| (!m.trim().is_empty()).then(|| m.trim().to_owned()))
        })
        .or_else(|| {
            display_name
                .and_then(resolve_display_name_model)
                .map(str::to_owned)
        })
        .or_else(|| {
            display_name
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
        });
    let lower = resolved.as_deref().unwrap_or("").to_ascii_lowercase();
    let (family, family_label, track) = if lower.contains("gemini") {
        ("gemini", "Gemini", Some("gemini"))
    } else if lower.contains("claude")
        || lower.contains("sonnet")
        || lower.contains("opus")
        || lower.contains("haiku")
        || lower.contains("anthropic")
    {
        ("claude", "Claude", Some("claude_gpt"))
    } else if lower.contains("gpt")
        || lower.contains("openai")
        || lower.starts_with("o1")
        || lower.starts_with("o3")
        || lower.starts_with("o4")
    {
        ("gpt", "GPT / OpenAI", Some("claude_gpt"))
    } else {
        ("other", "其他 / 未识别", None)
    };
    (resolved, family, family_label, track)
}

fn sanitize_quota_label(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    let looks_like_path = trimmed.starts_with('/')
        || trimmed.starts_with("~/")
        || trimmed.contains('\\')
        || trimmed.contains("://")
        || (trimmed.len() >= 3
            && trimmed.as_bytes()[0].is_ascii_alphabetic()
            && trimmed.as_bytes()[1] == b':'
            && trimmed.as_bytes()[2] == b'/');
    if trimmed.is_empty()
        || trimmed.len() > 64
        || trimmed.chars().any(char::is_control)
        || trimmed.contains('@')
        || looks_like_path
    {
        return None;
    }
    let humanized = match trimmed {
        "geminiQuota" | "gemini_quota" | "geminiModels" => "Gemini",
        "claudeQuota" | "claude_quota" | "claudeModels" => "Claude / GPT",
        "premiumQuota" | "premium_quota" | "premiumModels" => "Premium",
        "flowCredits" | "flow_credits" => "Flow Credits",
        "promptCredits" | "prompt_credits" => "Prompt Credits",
        other => other,
    };
    Some(humanized.to_owned())
}

fn float_field(v: &Value, keys: &[&str]) -> Option<f64> {
    keys.iter().find_map(|k| {
        let cell = v.get(*k)?;
        cell.as_f64()
            .or_else(|| cell.as_str()?.trim().trim_end_matches('%').parse().ok())
            .filter(|n| n.is_finite() && *n >= 0.0)
    })
}

fn reset_timestamp(v: &Value) -> Option<i64> {
    for key in [
        "resetTime",
        "resetsAt",
        "nextResetTime",
        "windowEndTime",
        "expiresAt",
        "resetAt",
    ] {
        let Some(cell) = v.get(key) else {
            continue;
        };
        if let Some(ts) = time(cell) {
            return Some(ts);
        }
        if let Some(raw) = cell.as_i64().or_else(|| cell.as_str()?.parse().ok()) {
            if (1_577_836_800..=4_102_444_800).contains(&raw) {
                return Some(raw * 1000);
            }
            if (1_577_836_800_000..=4_102_444_800_000).contains(&raw) {
                return Some(raw);
            }
        }
    }
    None
}

fn parse_single_quota_entry(v: &Value, fallback_key: Option<&str>) -> Option<AntigravityLiveQuota> {
    let obj = v.as_object()?;
    let raw_label = text(
        v,
        &[
            "label",
            "displayName",
            "title",
            "name",
            "track",
            "modelFamily",
            "category",
            "quotaType",
            "modelOrAlias",
        ],
    )
    .or(fallback_key)?;
    let label = sanitize_quota_label(raw_label)?;
    let mut remaining =
        float_field(v, &["remainingPercentage", "remainingPercent"]).or_else(|| {
            float_field(v, &["remainingFraction", "remainingRatio"]).map(|f| {
                if f <= 1.0 {
                    f * 100.0
                } else {
                    f
                }
            })
        });
    let mut used = float_field(
        v,
        &[
            "usedPercentage",
            "usedPercent",
            "usagePercentage",
            "percentUsed",
        ],
    )
    .or_else(|| {
        float_field(v, &["usedFraction", "usageFraction", "usedRatio"]).map(|f| {
            if f <= 1.0 {
                f * 100.0
            } else {
                f
            }
        })
    });
    if remaining.is_none() && used.is_none() {
        let limit = float_field(
            v,
            &["limit", "max", "total", "quotaLimit", "monthlyLimit", "cap"],
        )
        .filter(|n| *n > 0.0);
        if let Some(limit) = limit {
            if let Some(rem_count) =
                float_field(v, &["remaining", "available", "left", "remainingCount"])
            {
                remaining = Some(((rem_count / limit) * 100.0).clamp(0.0, 100.0));
            } else if let Some(used_count) =
                float_field(v, &["used", "consumed", "usage", "usedCount"])
            {
                used = Some(((used_count / limit) * 100.0).clamp(0.0, 100.0));
            }
        }
    }
    if remaining.is_none() && used.is_none() && !obj.contains_key("quotaInfo") {
        return None;
    }
    if let Some(nested) = v.get("quotaInfo").filter(|q| q.is_object()) {
        if let Some(mut inner) = parse_single_quota_entry(nested, Some(&label)) {
            inner.label = label;
            if inner.reset_at.is_none() {
                inner.reset_at = reset_timestamp(v);
            }
            return Some(inner);
        }
    }
    let remaining_percent = remaining
        .map(|n| n.clamp(0.0, 100.0))
        .or_else(|| used.map(|u| (100.0 - u.clamp(0.0, 100.0)).clamp(0.0, 100.0)));
    let used_percent = used
        .map(|n| n.clamp(0.0, 100.0))
        .or_else(|| remaining_percent.map(|r| (100.0 - r).clamp(0.0, 100.0)));
    if remaining_percent.is_none() && used_percent.is_none() {
        return None;
    }
    Some(AntigravityLiveQuota {
        label,
        remaining_percent,
        used_percent,
        reset_at: reset_timestamp(v),
    })
}

pub(super) fn parse_quota_summary(root: &Value) -> Vec<AntigravityLiveQuota> {
    fn walk(
        node: &Value,
        key_hint: Option<&str>,
        depth: usize,
        out: &mut Vec<AntigravityLiveQuota>,
    ) {
        if depth > 5 || out.len() >= 16 {
            return;
        }
        match node {
            Value::Array(items) => {
                for item in items {
                    if let Some(parsed) = parse_single_quota_entry(item, None) {
                        out.push(parsed);
                    } else {
                        walk(item, None, depth + 1, out);
                    }
                }
            }
            Value::Object(map) => {
                if depth > 0 {
                    if let Some(parsed) = parse_single_quota_entry(node, key_hint) {
                        out.push(parsed);
                        return;
                    }
                }
                for (k, child) in map {
                    if matches!(
                        k.as_str(),
                        "email"
                            | "userEmail"
                            | "apiKey"
                            | "token"
                            | "accessToken"
                            | "refreshToken"
                            | "csrfToken"
                    ) {
                        continue;
                    }
                    let hint = matches!(
                        k.as_str(),
                        "geminiQuota"
                            | "gemini_quota"
                            | "geminiModels"
                            | "claudeQuota"
                            | "claude_quota"
                            | "claudeModels"
                            | "premiumQuota"
                            | "premium_quota"
                            | "premiumModels"
                            | "flowCredits"
                            | "promptCredits"
                    )
                    .then_some(k.as_str());
                    walk(child, hint, depth + 1, out);
                }
            }
            _ => {}
        }
    }
    let mut raw = Vec::new();
    walk(root, None, 0, &mut raw);
    let mut seen = BTreeSet::new();
    let mut deduped = Vec::new();
    for item in raw {
        let key = item.label.to_ascii_lowercase();
        if seen.insert(key) {
            deduped.push(item);
        }
    }
    deduped
}

async fn probe_live_quotas_with_api(api: &Api) -> (bool, Vec<AntigravityLiveQuota>) {
    let mut connected = false;
    let mut merged = Vec::new();
    let mut seen = BTreeSet::new();
    let body = json!({});
    let timeout = std::time::Duration::from_secs(2);
    for endpoint in &api.endpoints {
        if let Ok(payload) = api
            .call_on_timeout(endpoint, "RetrieveUserQuotaSummary", &body, timeout)
            .await
        {
            connected = true;
            for item in parse_quota_summary(&payload) {
                let key = item.label.to_ascii_lowercase();
                if seen.insert(key) {
                    merged.push(item);
                }
            }
            if !merged.is_empty() {
                break;
            }
        }
    }
    (connected, merged)
}

pub async fn probe_live_quotas() -> (bool, Vec<AntigravityLiveQuota>) {
    let connect = tokio::time::timeout(std::time::Duration::from_secs(2), Api::connect()).await;
    let Ok(Ok(Some(api))) = connect else {
        return (false, Vec::new());
    };
    probe_live_quotas_with_api(&api).await
}

fn match_track_live_quota(
    track_id: &str,
    live_quotas: &[AntigravityLiveQuota],
) -> Option<AntigravityLiveQuota> {
    live_quotas
        .iter()
        .find(|q| {
            let lower = q.label.to_ascii_lowercase();
            if track_id == "gemini" {
                lower.contains("gemini")
            } else {
                lower.contains("claude")
                    || lower.contains("sonnet")
                    || lower.contains("opus")
                    || lower.contains("gpt")
                    || lower.contains("openai")
                    || lower.contains("anthropic")
            }
        })
        .cloned()
}

pub(super) fn compute_usage_stats(
    rows: impl IntoIterator<Item = Result<super::storage_v12::AntigravityAttemptRow>>,
    filter: &Filter,
    pricing: &Pricing,
    multiplier: &str,
    now_ms: i64,
    live: (bool, Vec<AntigravityLiveQuota>),
) -> Result<AntigravityUsageStats> {
    use rust_decimal::Decimal;
    let filter_start = filter.start.unwrap_or(0);
    let filter_end = filter.end.unwrap_or(now_ms);
    let anchor = filter.end.unwrap_or(now_ms);
    let start_5h = anchor.saturating_sub(5 * 3_600_000);
    let start_7d = anchor.saturating_sub(7 * 86_400_000);
    let model_filter = filter.model.as_deref().filter(|s| !s.is_empty());
    let session_filter = filter.session_id.as_deref().filter(|s| !s.is_empty());

    #[derive(Default)]
    struct WindowAcc {
        requests: u64,
        tokens: u64,
        reasoning_tokens: u64,
        cost: Decimal,
        unpriced_requests: u64,
    }
    impl WindowAcc {
        fn into_view(self, hours: f64) -> AntigravityTrackWindow {
            AntigravityTrackWindow {
                requests: self.requests,
                tokens: self.tokens,
                reasoning_tokens: self.reasoning_tokens,
                cost_usd: if self.cost.is_zero() {
                    "0".into()
                } else {
                    self.cost.normalize().to_string()
                },
                unpriced_requests: self.unpriced_requests,
                tokens_per_hour: if hours > 0.0 {
                    ((self.tokens as f64 / hours) * 10.0).round() / 10.0
                } else {
                    0.0
                },
            }
        }
    }
    #[derive(Default)]
    struct FamilyAcc {
        label: &'static str,
        requests: u64,
        tokens: u64,
        reasoning_tokens: u64,
        cost: Decimal,
    }
    #[derive(Default)]
    struct SessionAcc {
        requests: u64,
        models: BTreeMap<String, u64>,
        family_counts: BTreeMap<&'static str, u64>,
        total_tokens: u64,
        reasoning_tokens: u64,
        cache_read_eligible: u64,
        cache_input_eligible: u64,
        duration_ms: u64,
        cost: Decimal,
        unpriced_requests: u64,
        started_at: Option<i64>,
        last_active_at: Option<i64>,
    }

    let mut requests = 0u64;
    let mut unpriced_requests = 0u64;
    let mut total_cost = Decimal::ZERO;
    let mut mix_input = 0u64;
    let mut mix_cache_read = 0u64;
    let mut mix_cache_write = 0u64;
    let mut mix_output = 0u64;
    let mut mix_visible_output = 0u64;
    let mut mix_reasoning = 0u64;
    let mut cache_read_eligible = 0u64;
    let mut cache_input_eligible = 0u64;
    let mut cache_savings = Decimal::ZERO;

    let mut gemini_5h = WindowAcc::default();
    let mut gemini_7d = WindowAcc::default();
    let mut claude_5h = WindowAcc::default();
    let mut claude_7d = WindowAcc::default();

    let mut families: BTreeMap<&'static str, FamilyAcc> = BTreeMap::new();
    let mut sessions: BTreeMap<String, SessionAcc> = BTreeMap::new();
    let mut quote_cache: BTreeMap<String, super::pricing::Quote> = BTreeMap::new();

    for item in rows {
        let row = item?;
        let (resolved_model, family, family_label, track) =
            classify_model_family(row.model.as_deref(), row.display_name.as_deref());
        let attempt_tokens = Tokens {
            inclusive_input: row.inclusive_input,
            input: row.input,
            output: row.output,
            reasoning: row.reasoning,
            cache_read: row.cache_read,
            cache_write: row.cache_write,
            ..Tokens::default()
        };
        let tok_total = attempt_tokens.total().unwrap_or(0);
        let tok_reasoning = row.reasoning.unwrap_or(0);
        let row_cost = row
            .cost
            .as_deref()
            .and_then(decimal)
            .unwrap_or(Decimal::ZERO);
        let is_unpriced_final = row.is_final && row.cost.is_none() && row.unpriced > 0;

        // Dual-Track 5h & 7d rolling windows relative to anchor
        if row.time >= start_7d && row.time <= anchor {
            if let Some(track_id) = track {
                let add_win = |win: &mut WindowAcc| {
                    if row.is_final {
                        win.requests += 1;
                        if is_unpriced_final {
                            win.unpriced_requests += 1;
                        }
                    }
                    win.tokens = win.tokens.saturating_add(tok_total);
                    win.reasoning_tokens = win.reasoning_tokens.saturating_add(tok_reasoning);
                    win.cost += row_cost;
                };
                if track_id == "gemini" {
                    add_win(&mut gemini_7d);
                    if row.time >= start_5h {
                        add_win(&mut gemini_5h);
                    }
                } else {
                    add_win(&mut claude_7d);
                    if row.time >= start_5h {
                        add_win(&mut claude_5h);
                    }
                }
            }
        }

        // Main filtered range breakdown
        if row.time < filter_start || row.time > filter_end {
            continue;
        }
        if let Some(wanted_session) = session_filter {
            if row.session_id.as_deref() != Some(wanted_session) {
                continue;
            }
        }
        if let Some(wanted_model) = model_filter {
            if resolved_model.as_deref() != Some(wanted_model)
                && row.model.as_deref() != Some(wanted_model)
            {
                continue;
            }
        }

        if row.is_final {
            requests += 1;
            if is_unpriced_final {
                unpriced_requests += 1;
            }
        }
        total_cost += row_cost;

        let inp = row.input.unwrap_or(0);
        let cr = row.cache_read.unwrap_or(0);
        let cw = row.cache_write.unwrap_or(0);
        let out = row.output.unwrap_or(0);
        let capped_reasoning = tok_reasoning.min(out);
        mix_input = mix_input.saturating_add(inp);
        mix_cache_read = mix_cache_read.saturating_add(cr);
        mix_cache_write = mix_cache_write.saturating_add(cw);
        mix_output = mix_output.saturating_add(out);
        mix_visible_output =
            mix_visible_output.saturating_add(out.saturating_sub(capped_reasoning));
        mix_reasoning = mix_reasoning.saturating_add(tok_reasoning);

        if let Some((elig_read, elig_input)) = attempt_tokens.cache_sample() {
            cache_read_eligible = cache_read_eligible.saturating_add(elig_read);
            cache_input_eligible = cache_input_eligible.saturating_add(elig_input);
        }

        if cr > 0 && row.cost.is_some() {
            if let Some(model_name) = resolved_model.as_deref() {
                let quote = quote_cache.entry(model_name.to_owned()).or_insert_with(|| {
                    pricing.quote_at(Some(model_name), multiplier, row.started_at)
                });
                let uncached_tokens = Tokens {
                    input: Some(inp.saturating_add(cr)),
                    output: Some(out),
                    cache_read: Some(0),
                    cache_write: row.cache_write,
                    ..Tokens::default()
                };
                if let Some(uncached_snap) = quote.calculate(&uncached_tokens, None) {
                    if let Some(uncached_cost) = decimal(&uncached_snap.cost) {
                        if uncached_cost > row_cost {
                            cache_savings += uncached_cost - row_cost;
                        }
                    }
                }
            }
        }

        let fam = families.entry(family).or_insert_with(|| FamilyAcc {
            label: family_label,
            ..FamilyAcc::default()
        });
        if row.is_final {
            fam.requests += 1;
        }
        fam.tokens = fam.tokens.saturating_add(tok_total);
        fam.reasoning_tokens = fam.reasoning_tokens.saturating_add(tok_reasoning);
        fam.cost += row_cost;

        if let Some(sid) = row.session_id.filter(|s| !s.is_empty()) {
            let sess = sessions.entry(sid).or_default();
            if row.is_final {
                sess.requests += 1;
                sess.duration_ms = sess.duration_ms.saturating_add(row.duration_ms);
                if is_unpriced_final {
                    sess.unpriced_requests += 1;
                }
            }
            if let Some(m) = resolved_model.as_deref() {
                *sess.models.entry(m.to_owned()).or_insert(0) += 1;
            }
            *sess.family_counts.entry(family).or_insert(0) += 1;
            sess.total_tokens = sess.total_tokens.saturating_add(tok_total);
            sess.reasoning_tokens = sess.reasoning_tokens.saturating_add(tok_reasoning);
            if let Some((er, ei)) = attempt_tokens.cache_sample() {
                sess.cache_read_eligible = sess.cache_read_eligible.saturating_add(er);
                sess.cache_input_eligible = sess.cache_input_eligible.saturating_add(ei);
            }
            sess.cost += row_cost;
            sess.started_at = Some(
                sess.started_at
                    .map_or(row.started_at, |prev| prev.min(row.started_at)),
            );
            sess.last_active_at = Some(
                sess.last_active_at
                    .map_or(row.time, |prev| prev.max(row.time)),
            );
        }
    }

    let mix_total = mix_input
        .saturating_add(mix_cache_read)
        .saturating_add(mix_cache_write)
        .saturating_add(mix_output);
    let thinking_share_percent = (mix_output > 0).then(|| {
        ((mix_reasoning.min(mix_output) as f64 * 100.0) / mix_output as f64 * 10.0).round() / 10.0
    });
    let cache_hit_percent = (cache_input_eligible > 0).then(|| {
        ((cache_read_eligible as f64 * 100.0) / cache_input_eligible as f64 * 10.0).round() / 10.0
    });

    let mut family_rows: Vec<AntigravityFamilyGroup> = families
        .into_iter()
        .map(|(family, acc)| AntigravityFamilyGroup {
            family: family.to_owned(),
            label: acc.label.to_owned(),
            requests: acc.requests,
            tokens: acc.tokens,
            reasoning_tokens: acc.reasoning_tokens,
            cost_usd: if acc.cost.is_zero() {
                "0".into()
            } else {
                acc.cost.normalize().to_string()
            },
            share_percent: if mix_total > 0 {
                ((acc.tokens as f64 * 100.0) / mix_total as f64 * 10.0).round() / 10.0
            } else {
                0.0
            },
        })
        .collect();
    family_rows.sort_by(|a, b| {
        b.tokens
            .cmp(&a.tokens)
            .then_with(|| b.requests.cmp(&a.requests))
            .then_with(|| a.family.cmp(&b.family))
    });

    let total_sessions = sessions.len() as u64;
    let mut top_sessions: Vec<AntigravitySessionRow> = sessions
        .into_iter()
        .map(|(session_id, acc)| {
            let short_hash: String = session_id.chars().take(8).collect();
            let primary_model = acc
                .models
                .into_iter()
                .max_by(|(ma, ca), (mb, cb)| ca.cmp(cb).then_with(|| mb.cmp(ma)))
                .map(|(m, _)| m)
                .unwrap_or_else(|| "未识别".into());
            let family = acc
                .family_counts
                .into_iter()
                .max_by(|(fa, ca), (fb, cb)| ca.cmp(cb).then_with(|| fb.cmp(fa)))
                .map(|(f, _)| f)
                .unwrap_or("other")
                .to_owned();
            let sess_cache_hit = (acc.cache_input_eligible > 0).then(|| {
                ((acc.cache_read_eligible as f64 * 100.0) / acc.cache_input_eligible as f64 * 10.0)
                    .round()
                    / 10.0
            });
            AntigravitySessionRow {
                session_id,
                short_hash,
                requests: acc.requests,
                primary_model,
                family,
                total_tokens: acc.total_tokens,
                reasoning_tokens: acc.reasoning_tokens,
                cache_hit_percent: sess_cache_hit,
                duration_ms: acc.duration_ms,
                cost_usd: if acc.cost.is_zero() {
                    "0".into()
                } else {
                    acc.cost.normalize().to_string()
                },
                unpriced_requests: acc.unpriced_requests,
                started_at: acc.started_at.unwrap_or(0),
                last_active_at: acc.last_active_at.unwrap_or(0),
            }
        })
        .collect();
    top_sessions.sort_by(|a, b| {
        b.total_tokens
            .cmp(&a.total_tokens)
            .then_with(|| b.requests.cmp(&a.requests))
            .then_with(|| b.last_active_at.cmp(&a.last_active_at))
            .then_with(|| a.session_id.cmp(&b.session_id))
    });
    top_sessions.truncate(20);

    let (live_connected, live_quotas) = live;
    let tracks = vec![
        AntigravityQuotaTrack {
            id: "gemini".into(),
            label: "Gemini".into(),
            burst_5h: gemini_5h.into_view(5.0),
            weekly_7d: gemini_7d.into_view(168.0),
            live_quota: match_track_live_quota("gemini", &live_quotas),
        },
        AntigravityQuotaTrack {
            id: "claude_gpt".into(),
            label: "Claude / GPT".into(),
            burst_5h: claude_5h.into_view(5.0),
            weekly_7d: claude_7d.into_view(168.0),
            live_quota: match_track_live_quota("claude_gpt", &live_quotas),
        },
    ];

    Ok(AntigravityUsageStats {
        requests,
        sessions: total_sessions,
        cost_usd: if total_cost.is_zero() {
            "0".into()
        } else {
            total_cost.normalize().to_string()
        },
        unpriced_requests,
        token_mix: AntigravityTokenMix {
            input: mix_input,
            cache_read: mix_cache_read,
            cache_write: mix_cache_write,
            output: mix_output,
            visible_output: mix_visible_output,
            reasoning: mix_reasoning,
            total: mix_total,
            thinking_share_percent,
            cache_hit_percent,
            cache_read_eligible,
            cache_input_eligible,
            cache_savings_usd: if cache_savings.is_zero() {
                "0".into()
            } else {
                cache_savings.normalize().to_string()
            },
        },
        tracks,
        families: family_rows,
        top_sessions,
        live_quotas,
        live_connected,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shared_execution_keeps_requests_separate_and_final_mirror_is_not_a_retry() {
        let steps = json!([{"metadata":{"executionId":"exec","startedAt":"2026-01-01T00:00:00Z","finishedGeneratingAt":"2026-01-01T00:00:03Z"}}]);
        let mut entry = json!({"executionId":"exec","stepIndices":[0],"chatModel":{"customMetadata":{"request_id":"request-one"},"model":"fixture-model","usage":{"inputTokens":"9","outputTokens":"3"},"retryInfos":[{"usage":{"inputTokens":"9","outputTokens":"3"}}]}});
        let one = record(
            "parent",
            &entry,
            &Steps::from(steps.as_array().unwrap().as_slice()),
            0,
        )
        .unwrap()
        .unwrap();
        assert_eq!(one.attempts.len(), 1);
        assert_eq!(one.tokens().total(), Some(12));
        assert_eq!(
            one.id,
            record(
                "child",
                &entry,
                &Steps::from(steps.as_array().unwrap().as_slice()),
                2
            )
            .unwrap()
            .unwrap()
            .id
        );
        entry["chatModel"]["customMetadata"]["request_id"] = json!("request-two");
        let two = record(
            "parent",
            &entry,
            &Steps::from(steps.as_array().unwrap().as_slice()),
            1,
        )
        .unwrap()
        .unwrap();
        assert_ne!(one.id, two.id);
    }
    #[test]
    fn retries_are_unique_and_reasoning_is_subset() {
        let steps = json!([{"metadata":{"executionId":"exec","startedAt":"2026-01-01T00:00:00Z","finishedGeneratingAt":"2026-01-01T00:00:03Z"}}]);
        let m = json!({"executionId":"exec","stepIndices":[0],"chatModel":{"model":"claude-fixture","usage":{"responseId":"last","inputTokens":"10","cacheReadTokens":"20","outputTokens":"5","thinkingOutputTokens":"3"},"retryInfos":[{"traceId":"first","error":"unavailable","usage":{"inputTokens":"2","outputTokens":"0"}},{"usage":{"responseId":"last","inputTokens":"10","cacheReadTokens":"20","outputTokens":"5","thinkingOutputTokens":"3"}}]}});
        let r = record(
            "cascade",
            &m,
            &Steps::from(steps.as_array().unwrap().as_slice()),
            0,
        )
        .unwrap()
        .unwrap();
        assert_eq!(r.attempts.len(), 2);
        assert_eq!(r.tokens().total(), Some(37));
        assert_eq!(r.tokens().reasoning, Some(3));
        assert!(r.completed);
        assert_eq!(r.attempts[0].outcome, "failed");
        assert_eq!(r.attempts[1].tokens.cache_write, None);
        assert_ne!(
            r.id,
            record(
                "child",
                &m,
                &Steps::from(steps.as_array().unwrap().as_slice()),
                0
            )
            .unwrap()
            .unwrap()
            .id
        );
    }
    #[test]
    fn estimates_and_ambiguous_retries_do_not_become_bills() {
        let steps = json!([{"metadata":{"executionId":"exec","createdAt":"2026-01-01T00:00:00Z"}}]);
        let m = json!({"executionId":"exec","chatModel":{"chatStartMetadata":{"contextWindowMetadata":{"estimatedTokensUsed":500}}}});
        let pending = record(
            "cascade",
            &m,
            &Steps::from(steps.as_array().unwrap().as_slice()),
            0,
        )
        .unwrap()
        .unwrap();
        assert_eq!(pending.tokens().total(), None);
        assert_eq!(pending.final_attempt().unwrap().outcome, "in_progress");
        let m = json!({"executionId":"exec","chatModel":{"usage":{"inputTokens":"5"},"retryInfos":[{"usage":{"inputTokens":"5"}}]}});
        assert!(record(
            "cascade",
            &m,
            &Steps::from(steps.as_array().unwrap().as_slice()),
            0
        )
        .is_err());
    }
}

#[cfg(test)]
mod history_tests {
    use super::super::store::Store;
    use super::*;
    #[test]
    fn history_is_incremental_and_correlates_rpc_steps_without_duplicates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir
            .path()
            .join("antigravity/brain/cascade/.system_generated/logs/transcript.jsonl");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut store = Store::open(&dir.path().join("db")).unwrap();
        let pricing = Pricing::new(&dir.path().join("db")).unwrap();
        let mut report = Report::default();
        let cursor = serde_json::to_string(&Cursor {
            covered: BTreeSet::from([3]),
            ..Cursor::default()
        })
        .unwrap();
        store
            .write_batch(&[], Some((&safe_id("antigravity:rpc:cascade"), &cursor)))
            .unwrap();
        let rows=concat!("{\"step_index\":3,\"input_tokens\":10,\"output_tokens\":2}\n", "{\"request_id\":\"request-fixture\",\"model\":\"fixture-model\",\"created_at\":\"2026-01-01T00:00:00Z\",\"input_tokens\":10,\"output_tokens\":3}\n");
        std::fs::write(&path, rows).unwrap();
        scan_files(
            &mut store,
            dir.path(),
            &pricing,
            &Settings::default(),
            &mut report,
            false,
        )
        .unwrap();
        assert_eq!(report.imported, 1);
        let saved = store
            .detail(&safe_id("antigravity:request:request-fixture"))
            .unwrap();
        assert_eq!(saved.known_success(), None);
        assert_eq!(saved.attempts[0].measured_duration_ms(), None);
        scan_files(
            &mut store,
            dir.path(),
            &pricing,
            &Settings::default(),
            &mut report,
            false,
        )
        .unwrap();
        assert_eq!(report.imported, 1);
        assert_eq!(
            store.dashboard(&Filter::default()).unwrap().totals.requests,
            1
        );
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(b"{\"step_index\":99,\"input_tokens\":10}\n")
            .unwrap();
        assert!(scan_files(
            &mut store,
            dir.path(),
            &pricing,
            &Settings::default(),
            &mut report,
            false,
        )
        .is_ok());
        assert_eq!(report.errors, 1);
        assert_eq!(
            store.dashboard(&Filter::default()).unwrap().totals.requests,
            1
        );
    }

    #[test]
    fn transcript_discovery_keeps_system_logs_while_skipping_scratch_and_build_trees() {
        let dir = tempfile::tempdir().unwrap();
        let brain = dir.path().join("antigravity/brain");
        let valid = brain.join("cascade-one/.system_generated/logs/transcript.jsonl");
        let scratch = brain.join("cascade-one/scratch/sub/transcript.jsonl");
        let modules = brain.join("cascade-one/node_modules/pkg/transcript.jsonl");
        let target_dir = brain.join("cascade-one/target/debug/transcript.jsonl");
        for path in [&valid, &scratch, &modules, &target_dir] {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "{}\n").unwrap();
        }
        let discovered = collect_transcript_files(&brain).unwrap();
        assert_eq!(discovered, vec![valid]);
    }
}
