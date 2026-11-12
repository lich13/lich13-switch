//! Read-only Antigravity app accounting. Only session titles persist in private cursors; no messages or API headers.
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

#[path = "antigravity_quota.rs"]
pub(crate) mod quota;
const LIMIT: usize = 32 * 1024 * 1024;
#[cfg(test)]
#[path = "antigravity_tests.rs"]
mod regression_tests;
#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct Cursor {
    session_id: String,
    title: Option<String>,
    modified: String,
    count: usize,
    covered: BTreeSet<u64>,
    last_model: Option<String>,
}
#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct FileCursor {
    offset: u64,
    tail: String,
    identity: String,
    modified: String,
    complete: bool,
    model: Option<String>,
    model_invalid: bool,
    purged_covered: usize,
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
        // Antigravity's local language server uses a self-signed loopback certificate; callers only connect to 127.0.0.1 with a CSRF token.
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

struct ModelCatalogEntry {
    aliases: &'static [&'static str],
    display_names: &'static [&'static str],
    canonical: &'static str,
    display_name: &'static str,
    effort: Option<&'static str>,
    thinking_only: bool,
}

const MODEL_CATALOG: &[ModelCatalogEntry] = &[
    ModelCatalogEntry {
        aliases: &["MODEL_PLACEHOLDER_M417"],
        display_names: &["gemini 4 argon (high)", "gemini 4 argon"],
        canonical: "gemini-4-argon",
        display_name: "Gemini 4 Argon",
        effort: Some("high"),
        thinking_only: false,
    },
    ModelCatalogEntry {
        aliases: &["MODEL_PLACEHOLDER_M402"],
        display_names: &["claude opus 5.5 (high)", "claude opus 5.5", "opus 5.5"],
        canonical: "claude-opus-5-5",
        display_name: "Claude Opus 5.5",
        effort: Some("high"),
        thinking_only: false,
    },
    ModelCatalogEntry {
        aliases: &["MODEL_PLACEHOLDER_M37", "MODEL_GOOGLE_GEMINI_3_1_PRO_HIGH"],
        display_names: &["gemini 3.1 pro"],
        canonical: "gemini-3.1-pro",
        display_name: "Gemini 3.1 Pro",
        effort: Some("high"),
        thinking_only: false,
    },
    ModelCatalogEntry {
        aliases: &["MODEL_PLACEHOLDER_M36", "MODEL_GOOGLE_GEMINI_3_1_PRO_LOW"],
        display_names: &[],
        canonical: "gemini-3.1-pro",
        display_name: "Gemini 3.1 Pro",
        effort: Some("low"),
        thinking_only: false,
    },
    ModelCatalogEntry {
        aliases: &["MODEL_GOOGLE_GEMINI_3_1_PRO"],
        display_names: &[],
        canonical: "gemini-3.1-pro",
        display_name: "Gemini 3.1 Pro",
        effort: None,
        thinking_only: false,
    },
    ModelCatalogEntry {
        aliases: &[
            "MODEL_PLACEHOLDER_M18",
            "MODEL_GOOGLE_GEMINI_3_FLASH",
            "MODEL_GOOGLE_GEMINI_3_0_FLASH",
        ],
        display_names: &["gemini 3 flash", "gemini 3.0 flash"],
        canonical: "gemini-3-flash",
        display_name: "Gemini 3 Flash",
        effort: None,
        thinking_only: false,
    },
    ModelCatalogEntry {
        aliases: &["MODEL_PLACEHOLDER_M19", "MODEL_GOOGLE_GEMINI_3_PRO_HIGH"],
        display_names: &["gemini 3 pro", "gemini 3.0 pro"],
        canonical: "gemini-3-pro-preview",
        display_name: "Gemini 3 Pro",
        effort: Some("high"),
        thinking_only: false,
    },
    ModelCatalogEntry {
        aliases: &["MODEL_PLACEHOLDER_M17", "MODEL_GOOGLE_GEMINI_3_PRO_LOW"],
        display_names: &[],
        canonical: "gemini-3-pro-preview",
        display_name: "Gemini 3 Pro",
        effort: Some("low"),
        thinking_only: false,
    },
    ModelCatalogEntry {
        aliases: &["MODEL_GOOGLE_GEMINI_3_PRO"],
        display_names: &[],
        canonical: "gemini-3-pro-preview",
        display_name: "Gemini 3 Pro",
        effort: None,
        thinking_only: false,
    },
    ModelCatalogEntry {
        aliases: &[
            "MODEL_PLACEHOLDER_M26",
            "MODEL_CLAUDE_4_6_OPUS_THINKING",
            "MODEL_CLAUDE_4_6_OPUS",
        ],
        display_names: &["claude opus 4.6", "claude 4.6 opus"],
        canonical: "claude-opus-4-6",
        display_name: "Claude Opus 4.6",
        effort: None,
        thinking_only: true,
    },
    ModelCatalogEntry {
        aliases: &[
            "MODEL_PLACEHOLDER_M35",
            "MODEL_CLAUDE_4_6_SONNET",
            "MODEL_CLAUDE_4_6_SONNET_THINKING",
        ],
        display_names: &["claude sonnet 4.6", "claude 4.6 sonnet"],
        canonical: "claude-sonnet-4-6",
        display_name: "Claude Sonnet 4.6",
        effort: None,
        thinking_only: true,
    },
    ModelCatalogEntry {
        aliases: &[
            "MODEL_PLACEHOLDER_M12",
            "MODEL_CLAUDE_4_5_OPUS",
            "MODEL_CLAUDE_4_5_OPUS_THINKING",
        ],
        display_names: &["claude opus 4.5", "claude 4.5 opus"],
        canonical: "claude-opus-4-5",
        display_name: "Claude Opus 4.5",
        effort: None,
        thinking_only: true,
    },
    ModelCatalogEntry {
        aliases: &[
            "MODEL_CLAUDE_4_5_SONNET",
            "MODEL_CLAUDE_4_5_SONNET_THINKING",
        ],
        display_names: &["claude sonnet 4.5", "claude 4.5 sonnet"],
        canonical: "claude-sonnet-4-5",
        display_name: "Claude Sonnet 4.5",
        effort: None,
        thinking_only: true,
    },
    ModelCatalogEntry {
        aliases: &["MODEL_CLAUDE_4_SONNET", "MODEL_CLAUDE_4_SONNET_THINKING"],
        display_names: &["claude sonnet 4", "claude 4 sonnet"],
        canonical: "claude-sonnet-4-20250514",
        display_name: "Claude Sonnet 4",
        effort: None,
        thinking_only: true,
    },
    ModelCatalogEntry {
        aliases: &[
            "MODEL_PLACEHOLDER_M8",
            "MODEL_CHAT_23310",
            "MODEL_GOOGLE_GEMINI_2_5_PRO",
            "MODEL_GOOGLE_GEMINI_2_5_PRO_THINKING",
        ],
        display_names: &["gemini 2.5 pro"],
        canonical: "gemini-2.5-pro",
        display_name: "Gemini 2.5 Pro",
        effort: None,
        thinking_only: true,
    },
    ModelCatalogEntry {
        aliases: &[
            "MODEL_PLACEHOLDER_M7",
            "MODEL_CHAT_20706",
            "MODEL_GOOGLE_GEMINI_2_5_FLASH",
            "MODEL_GOOGLE_GEMINI_2_5_FLASH_THINKING",
        ],
        display_names: &["gemini 2.5 flash"],
        canonical: "gemini-2.5-flash",
        display_name: "Gemini 2.5 Flash",
        effort: None,
        thinking_only: true,
    },
    ModelCatalogEntry {
        aliases: &["MODEL_PLACEHOLDER_M9", "MODEL_GOOGLE_GEMINI_2_5_FLASH_LITE"],
        display_names: &["gemini 2.5 flash lite", "gemini 2.5 flash-lite"],
        canonical: "gemini-2.5-flash-lite",
        display_name: "Gemini 2.5 Flash Lite",
        effort: None,
        thinking_only: false,
    },
    ModelCatalogEntry {
        aliases: &["MODEL_OPENAI_GPT_OSS_120B_MEDIUM"],
        display_names: &["gpt-oss 120b", "gpt oss 120b"],
        canonical: "gpt-oss-120b",
        display_name: "GPT-OSS 120B",
        effort: Some("medium"),
        thinking_only: false,
    },
    ModelCatalogEntry {
        aliases: &["MODEL_OPENAI_GPT_OSS_120B"],
        display_names: &[],
        canonical: "gpt-oss-120b",
        display_name: "GPT-OSS 120B",
        effort: None,
        thinking_only: false,
    },
];

fn resolve_model_alias(raw: Option<&str>) -> Option<ResolvedModelAlias> {
    let trimmed = raw?.trim();
    if trimmed.is_empty() {
        return None;
    }
    let key = trimmed.to_ascii_uppercase();
    let entry = MODEL_CATALOG
        .iter()
        .find(|item| item.aliases.contains(&key.as_str()))?;
    Some(ResolvedModelAlias {
        canonical: entry.canonical,
        display_name: entry.display_name,
        effort: entry.effort,
    })
}

fn resolve_display_name_model(display_name: &str) -> Option<&'static str> {
    let key = display_name.trim().to_ascii_lowercase();
    MODEL_CATALOG
        .iter()
        .find(|item| item.display_names.contains(&key.as_str()))
        .map(|item| item.canonical)
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
fn is_thinking_only_canonical_model(model: Option<&str>) -> bool {
    let Some(model) = model else {
        return false;
    };
    MODEL_CATALOG
        .iter()
        .any(|item| item.canonical == model && item.thinking_only)
}

fn strip_display_effort_suffix(name: &str) -> Option<(&str, Option<String>)> {
    let (base, suffix) = name.strip_suffix(')')?.rsplit_once(" (")?;
    let raw = suffix.to_ascii_lowercase();
    let effort = match raw.as_str() {
        "none" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max" | "ultra" => {
            Some(Some(raw))
        }
        "thinking" => Some(None),
        _ => None,
    }?;
    (!base.is_empty()).then_some((base, effort))
}

pub(super) fn normalize_attempt(a: &mut Attempt) -> bool {
    let mut changed = false;
    if let Some((name, effort)) = a
        .model_display_name
        .as_deref()
        .and_then(strip_display_effort_suffix)
        .map(|(n, e)| (n.to_owned(), e))
    {
        a.model_display_name = Some(name);
        match effort {
            Some(effort) => {
                a.reasoning_effort.get_or_insert(effort);
            }
            None if a.reasoning_effort.as_deref() == Some("high") => {
                a.reasoning_effort = None;
            }
            None => {}
        }
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
    if a.reasoning_effort.as_deref() == Some("high")
        && (is_thinking_only_canonical_model(a.response_model.as_deref())
            || is_thinking_only_canonical_model(a.requested_model.as_deref()))
    {
        a.reasoning_effort = None;
        changed = true;
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

    if let Some(inp) = a.tokens.input {
        if a.tokens.cache_read.is_none() {
            a.tokens.cache_read = Some(0);
            changed = true;
        }
        let inclusive = inp
            .saturating_add(a.tokens.cache_read.unwrap_or(0))
            .saturating_add(a.tokens.cache_write.unwrap_or(0));
        if a.tokens.inclusive_input != Some(inclusive) {
            a.tokens.inclusive_input = Some(inclusive);
            changed = true;
        }
        if a.inclusive_input_tokens != Some(inclusive) {
            a.inclusive_input_tokens = Some(inclusive);
            changed = true;
        }
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
    let fallback = r.attempts.iter().rev().find_map(|a| {
        let model = a
            .response_model
            .as_deref()
            .or(a.requested_model.as_deref())
            .or(a.pricing_model.as_deref())?;
        Some((
            a.requested_model.clone(),
            model.to_owned(),
            a.pricing_model.clone().or_else(|| Some(model.to_owned())),
        ))
    });
    if let Some((requested_model, response_model, pricing_model)) = fallback {
        for a in &mut r.attempts {
            if a.requested_model.is_none()
                && a.response_model.is_none()
                && a.pricing_model.is_none()
            {
                a.requested_model = requested_model.clone();
                a.response_model = Some(response_model.clone());
                if a.price.is_none() {
                    a.pricing_model = pricing_model.clone();
                }
                changed = true;
            }
        }
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
fn classify_error_text(raw: &str) -> Option<&'static str> {
    let lower = raw.to_ascii_lowercase();
    if lower.contains("stream was interrupted")
        || lower.contains("5m0s")
        || lower.contains("deadline_exceeded")
        || lower.contains("context deadline exceeded")
        || lower.contains("timed out")
    {
        Some("STREAM_TIMEOUT")
    } else if lower.contains("resource_exhausted")
        || lower.contains("quota")
        || lower.contains("rate limit")
        || lower.contains("too many requests")
        || lower.contains("429")
    {
        Some("RESOURCE_EXHAUSTED")
    } else if lower.contains("unauthenticated")
        || lower.contains("permission_denied")
        || lower.contains("invalid_grant")
        || lower.contains("401")
        || lower.contains("403")
    {
        Some("UNAUTHENTICATED")
    } else if lower.contains("unavailable")
        || lower.contains("internal")
        || lower.contains("500")
        || lower.contains("502")
        || lower.contains("503")
    {
        Some("UPSTREAM_ERROR")
    } else {
        None
    }
}
fn failed(v: &Value) -> bool {
    v.get("error")
        .is_some_and(|e| !e.is_null() && e != "" && e != &json!({}))
        || text(v, &["errorMessage", "errorCode", "failureReason"]).is_some()
        || v["status"]
            .as_str()
            .is_some_and(|s| s.eq_ignore_ascii_case("ERROR") || s.eq_ignore_ascii_case("FAILED"))
}
fn error_code(v: &Value) -> Option<String> {
    let e = &v["error"];
    if let Some(code) = [&e["code"], &e["type"], &e["status"], &v["errorCode"]]
        .into_iter()
        .find_map(|code| {
            code.as_i64()
                .map(|n| n.to_string())
                .or_else(|| label(code.as_str()))
        })
    {
        return Some(code);
    }
    for msg in [
        e.as_str(),
        e["message"].as_str(),
        v["errorMessage"].as_str(),
        v["failureReason"].as_str(),
        v["content"].as_str(),
    ]
    .into_iter()
    .flatten()
    {
        if let Some(classified) = classify_error_text(msg) {
            return Some(classified.into());
        }
    }
    failed(v).then(|| "ANTIGRAVITY_ERROR".into())
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
        let explicit_attempt_model = non_enum_model(text(u, &["model"]))
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
            });
        let alias_hint = resolve_model_alias(text(u, &["model"]))
            .or_else(|| resolve_model_alias(text(&info["usage"], &["model"])))
            .or_else(|| resolve_model_alias(text(info, &["responseModel"])))
            .or_else(|| {
                final_info
                    .then(|| resolve_model_alias(text(chat, &["responseModel"])))
                    .flatten()
            })
            .or_else(|| resolve_model_alias(text(chat, &["model"])))
            .or_else(|| {
                explicit_attempt_model
                    .is_none()
                    .then(|| {
                        resolve_model_alias(
                            text(info, &["modelDisplayName"])
                                .or_else(|| text(chat, &["modelDisplayName"]))
                                .or_else(|| {
                                    text(entry, &["StepMetadata", "chatModel", "modelDisplayName"])
                                }),
                        )
                    })
                    .flatten()
            });
        let response_model =
            explicit_attempt_model.or_else(|| alias_hint.map(|alias| alias.canonical.to_owned()));
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
    normalize_record(&mut r);
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
#[cfg(test)]
thread_local! {
    static TEST_ENDPOINT: std::cell::RefCell<Option<(String, String)>> = const { std::cell::RefCell::new(None) };
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
        #[cfg(test)]
        if let Some((origin, token)) = TEST_ENDPOINT.with(|endpoint| endpoint.borrow().clone()) {
            return Ok(Some(Self::from_single(loopback_client()?, origin, token)));
        }
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
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn collect_with_files(
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
        let timeline = store
            .data_dir()
            .map(|d| {
                crate::antigravity::auth::provider_timeline(
                    d,
                    brain.parent().and_then(Path::parent),
                )
            })
            .unwrap_or_default();
        report.total_files = summaries.len() as u64;
        for (id, (owner, summary)) in &summaries {
            match self
                .collect_session(
                    id, *owner, summary, store, pricing, settings, report, repair, &timeline,
                )
                .await
            {
                Ok(()) => report.files += 1,
                Err(_) => {
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
        timeline: &crate::antigravity::auth::ProviderTimeline,
    ) -> Result<()> {
        let Some(active_endpoint) = owner else {
            return Ok(());
        };
        let cursor_id = safe_id(&format!("antigravity:rpc:{id}"));
        let mut cursor: Cursor = if repair {
            Cursor::default()
        } else {
            store
                .cursor(&cursor_id)?
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_default()
        };
        let session_id = safe_id(id);
        let title = summary
            .get("summary")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        let metadata_changed = (cursor.title.is_some() && cursor.session_id != session_id)
            || (summary.get("summary").is_some() && cursor.title != title);
        cursor.session_id = session_id;
        if summary.get("summary").is_some() {
            cursor.title = title;
        }
        let modified = summary["lastModifiedTime"]
            .as_str()
            .filter(|s| !s.is_empty())
            .or_else(|| summary["localModifiedStamp"].as_str())
            .unwrap_or("");
        if !modified.is_empty() && cursor.modified == modified {
            if metadata_changed {
                let saved = serde_json::to_string(&cursor)
                    .map_err(|_| failure("Antigravity 同步位置无效"))?;
                store.write_batch(&[], Some((&cursor_id, &saved)))?;
            }
            return Ok(());
        }
        let total = number(&summary["stepCount"]);
        if total.is_some_and(|n| n > 100_000) {
            return Err(failure("Antigravity 会话步骤超过读取限制"));
        }
        let mut steps = Steps::default();
        let mut failed_steps = BTreeSet::new();
        let mut wrote_any = false;
        let mut fallback_loaded = false;
        let mut offset = cursor.count.saturating_sub(1);
        let mut stable = offset;
        loop {
            let meta_req =
                json!({"cascadeId":id,"generatorMetadataOffset":offset,"includeMessages":false});
            let page = self
                .call_on(
                    &self.endpoints[active_endpoint],
                    "GetCascadeTrajectoryGeneratorMetadata",
                    &meta_req,
                )
                .await?;
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
                    let mut indices: Vec<_> = entry["stepIndices"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(number)
                        .collect();
                    let parsed = match record(id, entry, &steps, offset) {
                        Ok(parsed) => parsed,
                        Err(_) => {
                            failed_steps.extend(indices);
                            for s in &failed_steps {
                                cursor.covered.remove(s);
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
                            if let Some(m) = r.final_attempt().and_then(|a| {
                                a.response_model
                                    .as_deref()
                                    .or(a.requested_model.as_deref())
                                    .or(a.pricing_model.as_deref())
                            }) {
                                cursor.last_model = Some(m.to_owned());
                            }
                            for a in &mut r.attempts {
                                if a.provider.is_none() {
                                    a.provider =
                                        timeline.provider_at(a.started_at).map(str::to_owned);
                                }
                            }
                            price(&mut r, pricing, settings);
                            rows.push(r);
                            complete
                        }
                        None => entry.get("chatModel").is_none(),
                    };
                    if let Some(exec_indices) = entry["executionId"]
                        .as_str()
                        .and_then(|id| steps.executions.get(id))
                    {
                        indices.extend(exec_indices.iter().copied());
                    }
                    if complete {
                        if stable == offset {
                            stable += 1;
                        }
                        for step_idx in indices.into_iter().filter(|i| !failed_steps.contains(i)) {
                            cursor.covered.insert(step_idx);
                        }
                    } else if entry.get("chatModel").is_some() {
                        let has_reported_tokens = entry["chatModel"].get("usage").is_some()
                            || entry["chatModel"]["retryInfos"]
                                .as_array()
                                .is_some_and(|a| !a.is_empty());
                        if has_reported_tokens {
                            failed_steps.extend(indices);
                            for s in &failed_steps {
                                cursor.covered.remove(s);
                            }
                            report.errors += 1;
                        } else {
                            for step_idx in
                                indices.into_iter().filter(|i| !failed_steps.contains(i))
                            {
                                cursor.covered.insert(step_idx);
                            }
                        }
                    }
                    offset += 1;
                }
                if !rows.is_empty() {
                    store.write_batch(&rows, None)?;
                    report.imported += rows.len() as u64;
                    wrote_any = true;
                }
            }
        }
        for s in &failed_steps {
            cursor.covered.remove(s);
        }
        let superseded: Vec<String> = if !wrote_any {
            Vec::new()
        } else {
            cursor
                .covered
                .iter()
                .map(|step_idx| safe_id(&format!("antigravity:request:{id}:step:{step_idx}")))
                .collect()
        };
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
        store.write_batch_with_superseded(&[], &superseded, Some((&cursor_id, &cursor)))?;
        Ok(())
    }
}

fn cascade_id<'a>(brain: &Path, path: &'a Path) -> Result<&'a str> {
    path.strip_prefix(brain)
        .ok()
        .and_then(|p| p.components().next())
        .and_then(|c| c.as_os_str().to_str())
        .ok_or_else(|| failure("Antigravity 历史路径无效"))
}

pub(super) fn collect_transcript_files(brain: &Path) -> Result<Vec<PathBuf>> {
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

pub(super) fn scan_file_list(
    store: &mut impl Repository,
    brain: &Path,
    files: &[PathBuf],
    pricing: &Pricing,
    settings: &Settings,
    report: &mut Report,
    repair: bool,
) -> Result<()> {
    let timeline = store
        .data_dir()
        .map(|d| {
            crate::antigravity::auth::provider_timeline(d, brain.parent().and_then(Path::parent))
        })
        .unwrap_or_default();
    for path in files {
        if scan_file(
            store, brain, path, pricing, settings, report, repair, &timeline,
        )
        .is_err()
        {
            report.errors += 1;
        }
    }
    Ok(())
}
fn extract_settings_model(content: &str) -> Option<String> {
    let scope = content
        .rsplit_once("</USER_REQUEST>")
        .map_or(content, |(_, tail)| tail);
    let (_, after_open) = scope.rsplit_once("<USER_SETTINGS_CHANGE>")?;
    let (inner, _) = after_open.split_once("</USER_SETTINGS_CHANGE>")?;
    for raw_line in inner.lines() {
        let line = raw_line.trim();
        let Some((sentence, _)) = line.split_once(". No need to comment on this change") else {
            continue;
        };
        let Some(rest) = sentence.strip_prefix("The user changed setting `Model Selection` from ")
        else {
            continue;
        };
        let Some((_, model)) = rest.rsplit_once(" to ") else {
            continue;
        };
        if let Some(candidate) = label(Some(model)).filter(|c| !c.eq_ignore_ascii_case("none")) {
            return Some(candidate);
        }
    }
    None
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
    timeline: &crate::antigravity::auth::ProviderTimeline,
) -> Result<()> {
    let key = safe_id(&format!("antigravity:file:{}", path.to_string_lossy()));
    let saved_raw = if repair { None } else { store.cursor(&key)? };
    let mut c: FileCursor = saved_raw
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_default();
    let cascade = cascade_id(brain, path)?;
    let rpc: Cursor = store
        .cursor(&safe_id(&format!("antigravity:rpc:{cascade}")))?
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let meta = std::fs::metadata(path).map_err(storage::io_error)?;
    let identity = file_identity(&meta);
    let modified = file_modified(&meta).unwrap_or_default();
    let needs_rpc_model = c.model.is_none() && !c.model_invalid && rpc.last_model.is_some();
    if !repair
        && !needs_rpc_model
        && c.complete
        && !modified.is_empty()
        && c.identity == identity
        && meta.len() == c.offset
        && c.modified == modified
        && file_settled(&meta)
    {
        if !rpc.covered.is_empty() && c.purged_covered != rpc.covered.len() {
            let superseded: Vec<String> = rpc
                .covered
                .iter()
                .map(|step_idx| safe_id(&format!("antigravity:request:{cascade}:step:{step_idx}")))
                .collect();
            c.purged_covered = rpc.covered.len();
            let cursor =
                serde_json::to_string(&c).map_err(|_| failure("Antigravity 历史同步位置无效"))?;
            store.write_batch_with_superseded(&[], &superseded, Some((&key, &cursor)))?;
        }
        return Ok(());
    }
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
    let valid_prefix = !needs_rpc_model && c.identity == identity && meta.len() >= c.offset && {
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
            partial_tail = true;
            break;
        }
        let mut in_progress_step = false;
        let parsed = (|| -> Result<Option<Record>> {
            let v: Value = serde_json::from_slice(&bytes)
                .map_err(|_| failure("Antigravity 历史文件存在无效完整行"))?;
            if v["type"].as_str() == Some("USER_INPUT") {
                if let Some(content) = v["content"].as_str() {
                    if let Some(model) = extract_settings_model(content) {
                        c.model = Some(model);
                        c.model_invalid = false;
                    } else if content.contains("<USER_SETTINGS_CHANGE>") {
                        c.model = None;
                        c.model_invalid = true;
                    }
                }
            }
            let has_usage = [
                "input_tokens",
                "output_tokens",
                "cache_read_tokens",
                "cache_write_tokens",
            ]
            .iter()
            .any(|k| number(&v[*k]).is_some());
            if has_usage && !number(&v["step_index"]).is_some_and(|i| rpc.covered.contains(&i)) {
                let is_planner = v["type"].as_str() == Some("PLANNER_RESPONSE");
                if is_planner
                    && matches!(
                        v["status"].as_str(),
                        Some("RUNNING" | "IN_PROGRESS" | "PENDING")
                    )
                {
                    in_progress_step = true;
                    return Ok(None);
                }
                let step_request = is_planner
                    .then(|| number(&v["step_index"]).map(|i| format!("{cascade}:step:{i}")))
                    .flatten();
                let explicit_model = text(&v, &["model", "response_model"]);
                let request = text(&v, &["request_id"]).or(step_request.as_deref());
                let inherited_rpc_model = (!c.model_invalid && c.model.is_none())
                    .then_some(rpc.last_model.as_deref())
                    .flatten();
                let model = explicit_model
                    .or(c.model.as_deref())
                    .or(inherited_rpc_model);
                let at = time(&v["completed_at"]).or_else(|| time(&v["created_at"]));
                let (Some(request), Some(at)) = (request, at) else {
                    return Err(failure(
                        "Antigravity 历史用量缺少请求标识、模型或时间，请启动 app 后重新同步",
                    ));
                };
                if model.is_none() && (!is_planner || c.model_invalid) {
                    return Err(failure(
                        "Antigravity 历史用量缺少请求标识、模型或时间，请启动 app 后重新同步",
                    ));
                }
                let id = safe_id(&format!("antigravity:request:{request}"));
                let alias = resolve_model_alias(model);
                let model_for_id = explicit_model
                    .or_else(|| alias.map(|a| a.canonical))
                    .or(inherited_rpc_model);
                let is_failed = failed(&v);
                let mut attempt = Attempt {
                    id: safe_id(&format!("{id}:final")),
                    provider: timeline.provider_at(at).map(str::to_owned),
                    response_id: text(&v, &["response_id", "message_id"]).map(safe_id),
                    requested_model: actual_model(model_for_id),
                    response_model: actual_model(model_for_id),
                    model_display_name: label(text(&v, &["model_display_name"]))
                        .or_else(|| alias.map(|a| a.display_name.to_owned()))
                        .or_else(|| explicit_model.is_none().then(|| c.model.clone()).flatten()),
                    reasoning_effort: alias.and_then(|a| a.effort).map(str::to_owned),
                    error_code: is_failed.then(|| error_code(&v)).flatten(),
                    started_at: at,
                    outcome: if is_failed { "failed" } else { "unknown" }.into(),
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
        if in_progress_step {
            partial_tail = true;
            break;
        }
        match parsed {
            Ok(Some(row)) => rows.push(row),
            Ok(None) => {}
            Err(_) => {
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
    let need_purge = !rpc.covered.is_empty() && c.purged_covered != rpc.covered.len();
    if c.model.is_none() && !c.model_invalid {
        c.model = rpc.last_model.clone();
    }
    c.complete = !gap && !partial_tail && c.offset == meta.len();
    c.modified = if c.complete { modified } else { String::new() };
    c.purged_covered = rpc.covered.len();
    let cursor = serde_json::to_string(&c).map_err(|_| failure("Antigravity 历史同步位置无效"))?;
    if !rows.is_empty() || need_purge || saved_raw.as_deref() != Some(cursor.as_str()) {
        let superseded: Vec<String> = if need_purge {
            rpc.covered
                .iter()
                .map(|step_idx| safe_id(&format!("antigravity:request:{cascade}:step:{step_idx}")))
                .collect()
        } else {
            Vec::new()
        };
        store.write_batch_with_superseded(&rows, &superseded, Some((&key, &cursor)))?;
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
                if let Some(data) = store.data_dir() {
                    match api.quota(data, home).await {
                        Ok(Some((id, periods))) => store.save_antigravity_quota(&id, &periods)?,
                        Ok(None) => {}
                        Err(_) => report.errors += 1,
                    }
                }
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
    pub reasoning_reported: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AntigravityTrackWindow {
    pub period: Option<quota::Period>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remaining_percent: Option<f64>,
    pub requests: u64,
    pub tokens: u64,
    pub reasoning_tokens: u64,
    pub cost_usd: String,
    pub unpriced_requests: u64,
    pub tokens_per_hour: f64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AntigravityQuotaTrack {
    pub id: String,
    pub label: String,
    pub burst_5h: AntigravityTrackWindow,
    pub weekly_7d: AntigravityTrackWindow,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AntigravityProviderGroup {
    pub id: String,
    pub requests: u64,
    pub tokens: u64,
    pub reasoning_tokens: u64,
    pub reasoning_reported: bool,
    pub cost_usd: String,
    pub unpriced_requests: u64,
    pub share_percent: f64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AntigravityFamilyGroup {
    pub family: String,
    pub label: String,
    pub requests: u64,
    pub tokens: u64,
    pub reasoning_tokens: u64,
    pub reasoning_reported: bool,
    pub cost_usd: String,
    pub share_percent: f64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AntigravityUsageStats {
    pub requests: u64,
    pub sessions: u64,
    pub cost_usd: String,
    pub unpriced_requests: u64,
    pub token_mix: AntigravityTokenMix,
    pub accounts: Vec<quota::AccountUsage>,
    pub quota_error: Option<String>,
    #[serde(default)]
    pub providers: Vec<AntigravityProviderGroup>,
    pub families: Vec<AntigravityFamilyGroup>,
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

fn fmt_usd(d: rust_decimal::Decimal) -> String {
    if d.is_zero() {
        "0".into()
    } else {
        d.normalize().to_string()
    }
}

fn round_pct(num: u64, den: u64) -> Option<f64> {
    (den > 0).then(|| ((num as f64 * 100.0) / den as f64 * 10.0).round() / 10.0)
}

#[derive(Default)]
struct WindowAcc {
    requests: u64,
    tokens: u64,
    reasoning_tokens: u64,
    cost: rust_decimal::Decimal,
    unpriced_requests: u64,
}

impl WindowAcc {
    fn into_view(self, hours: f64) -> AntigravityTrackWindow {
        AntigravityTrackWindow {
            period: None,
            remaining_percent: None,
            requests: self.requests,
            tokens: self.tokens,
            reasoning_tokens: self.reasoning_tokens,
            cost_usd: fmt_usd(self.cost),
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
struct ProviderAcc {
    requests: u64,
    tokens: u64,
    reasoning_tokens: u64,
    reasoning_reported: bool,
    cost: rust_decimal::Decimal,
    unpriced_requests: u64,
}

#[derive(Default)]
struct FamilyAcc {
    label: String,
    requests: u64,
    tokens: u64,
    reasoning_tokens: u64,
    reasoning_reported: bool,
    cost: rust_decimal::Decimal,
}

pub(super) struct UsageStatsAggregator {
    filter_start: i64,
    filter_end: i64,
    pub(super) accounts: BTreeMap<String, (usize, quota::AccountAccumulator)>,
    provider_filter: Option<String>,
    model_filter: Option<String>,
    session_filter: Option<String>,
    full_models: bool,
    reasoning_reported: bool,
    requests: u64,
    unpriced_requests: u64,
    total_cost: rust_decimal::Decimal,
    mix_input: u64,
    mix_cache_read: u64,
    mix_cache_write: u64,
    mix_output: u64,
    mix_visible_output: u64,
    mix_reasoning: u64,
    cache_read_eligible: u64,
    cache_input_eligible: u64,
    cache_savings: rust_decimal::Decimal,
    providers: BTreeMap<String, ProviderAcc>,
    families: BTreeMap<String, FamilyAcc>,
    sessions: BTreeSet<String>,
}

impl UsageStatsAggregator {
    pub(super) fn new(filter: &Filter, now_ms: i64) -> Self {
        let anchor = filter.end.unwrap_or(now_ms);
        Self {
            filter_start: filter.start.unwrap_or(0),
            filter_end: anchor,
            accounts: BTreeMap::new(),
            provider_filter: filter
                .provider
                .as_deref()
                .filter(|s| !s.is_empty())
                .map(str::to_owned),
            model_filter: filter
                .model
                .as_deref()
                .filter(|s| !s.is_empty())
                .map(str::to_owned),
            session_filter: filter
                .session_id
                .as_deref()
                .filter(|s| !s.is_empty())
                .map(str::to_owned),
            full_models: filter.client.as_deref() == Some("claude"),
            reasoning_reported: false,
            requests: 0,
            unpriced_requests: 0,
            total_cost: rust_decimal::Decimal::ZERO,
            mix_input: 0,
            mix_cache_read: 0,
            mix_cache_write: 0,
            mix_output: 0,
            mix_visible_output: 0,
            mix_reasoning: 0,
            cache_read_eligible: 0,
            cache_input_eligible: 0,
            cache_savings: rust_decimal::Decimal::ZERO,
            providers: BTreeMap::new(),
            families: BTreeMap::new(),
            sessions: BTreeSet::new(),
        }
    }

    pub(super) fn ingest(&mut self, row: super::storage_v12::AntigravityAttemptRow) {
        let matches_provider = match self.provider_filter.as_deref() {
            Some("session") => {
                row.final_provider.is_none() || row.final_provider.as_deref() == Some("session")
            }
            Some(pf) => row.final_provider.as_deref() == Some(pf),
            None => true,
        };
        let classified = (!self.full_models)
            .then(|| classify_model_family(row.model.as_deref(), row.display_name.as_deref()));
        let (resolved_model, family, family_label, track) = match &classified {
            Some((model, family, label, track)) => (model.as_deref(), *family, *label, *track),
            None => {
                let family = row.model.as_deref().unwrap_or("未识别");
                (row.model.as_deref(), family, family, None)
            }
        };
        let attempt_tokens = &row.tokens;
        let tok_total = attempt_tokens.total().unwrap_or(0);
        let tok_reasoning = row.tokens.reasoning.unwrap_or(0);
        let row_cost = row
            .cost
            .as_deref()
            .and_then(decimal)
            .unwrap_or(rust_decimal::Decimal::ZERO);
        let is_unpriced_final = row.is_final && row.request_unpriced;

        if let Some((_, account)) = row
            .provider
            .as_ref()
            .and_then(|id| self.accounts.get_mut(id))
        {
            account.ingest(
                row.time,
                track,
                &WindowAcc {
                    requests: u64::from(row.is_final),
                    tokens: tok_total,
                    reasoning_tokens: tok_reasoning,
                    cost: row_cost,
                    unpriced_requests: u64::from(row.provider_unpriced),
                },
                row.time >= self.filter_start && row.time <= self.filter_end,
            );
        }

        if row.time < self.filter_start || row.time > self.filter_end {
            return;
        }
        if let Some(wanted_session) = self.session_filter.as_deref() {
            if row.session_id.as_deref() != Some(wanted_session) {
                return;
            }
        }
        if let Some(wanted_model) = self.model_filter.as_deref() {
            if resolved_model != Some(wanted_model) && row.model.as_deref() != Some(wanted_model) {
                return;
            }
        }

        let prov_id = row
            .provider
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or("session")
            .to_owned();
        let prov = self.providers.entry(prov_id).or_default();
        if row.is_final {
            prov.requests += 1;
        }
        if row.provider_unpriced {
            prov.unpriced_requests += 1;
        }
        prov.tokens = prov.tokens.saturating_add(tok_total);
        prov.reasoning_tokens = prov.reasoning_tokens.saturating_add(tok_reasoning);
        prov.cost += row_cost;
        prov.reasoning_reported |= row.tokens.reasoning.is_some();

        if !matches_provider {
            return;
        }

        if row.is_final {
            self.requests += 1;
            if is_unpriced_final {
                self.unpriced_requests += 1;
            }
        }
        self.total_cost += row_cost;

        let inp = row.tokens.input.unwrap_or(0);
        let cr = row.tokens.cache_read.unwrap_or(0);
        let cw = row.tokens.cache_write.unwrap_or(0);
        let out = row.tokens.output.unwrap_or(0);
        let capped_reasoning = tok_reasoning.min(out);
        self.mix_input = self.mix_input.saturating_add(inp);
        self.mix_cache_read = self.mix_cache_read.saturating_add(cr);
        self.mix_cache_write = self.mix_cache_write.saturating_add(cw);
        self.mix_output = self.mix_output.saturating_add(out);
        self.mix_visible_output = self
            .mix_visible_output
            .saturating_add(out.saturating_sub(capped_reasoning));
        self.mix_reasoning = self.mix_reasoning.saturating_add(tok_reasoning);

        let cache_sample = attempt_tokens.cache_sample().or_else(|| {
            if self.full_models {
                return None;
            }
            let inp = attempt_tokens.input?;
            let read = attempt_tokens.cache_read.unwrap_or(0);
            let total_inp = inp
                .saturating_add(read)
                .saturating_add(attempt_tokens.cache_write.unwrap_or(0));
            (read <= total_inp).then_some((read, total_inp))
        });
        if let Some((elig_read, elig_input)) = cache_sample {
            self.cache_read_eligible = self.cache_read_eligible.saturating_add(elig_read);
            self.cache_input_eligible = self.cache_input_eligible.saturating_add(elig_input);
        }

        if cr > 0 {
            if let Some(savings) = row
                .cache_savings_rates
                .and_then(|rates| super::pricing::cache_savings(rates, cr))
            {
                self.cache_savings += savings;
            }
        }
        let reported_reasoning = row.tokens.reasoning.is_some();
        self.reasoning_reported |= reported_reasoning;
        let fam = self
            .families
            .entry(family.to_owned())
            .or_insert_with(|| FamilyAcc {
                label: family_label.to_owned(),
                ..FamilyAcc::default()
            });
        if row.is_final {
            fam.requests += 1;
        }
        fam.tokens = fam.tokens.saturating_add(tok_total);
        fam.reasoning_tokens = fam.reasoning_tokens.saturating_add(tok_reasoning);
        fam.cost += row_cost;
        fam.reasoning_reported |= reported_reasoning;

        if let Some(sid) = row.session_id.filter(|s| !s.is_empty()) {
            self.sessions.insert(sid);
        }
    }

    pub(super) fn finish(self) -> AntigravityUsageStats {
        let mix_total = self
            .mix_input
            .saturating_add(self.mix_cache_read)
            .saturating_add(self.mix_cache_write)
            .saturating_add(self.mix_output);
        let thinking_share_percent = self
            .reasoning_reported
            .then(|| round_pct(self.mix_reasoning.min(self.mix_output), self.mix_output))
            .flatten();
        let cache_hit_percent = round_pct(self.cache_read_eligible, self.cache_input_eligible);
        let providers_total_tokens: u64 = self.providers.values().map(|acc| acc.tokens).sum();

        let mut provider_rows: Vec<AntigravityProviderGroup> = self
            .providers
            .into_iter()
            .map(|(id, acc)| AntigravityProviderGroup {
                id,
                requests: acc.requests,
                tokens: acc.tokens,
                reasoning_tokens: acc.reasoning_tokens,
                reasoning_reported: acc.reasoning_reported,
                cost_usd: fmt_usd(acc.cost),
                unpriced_requests: acc.unpriced_requests,
                share_percent: round_pct(acc.tokens, providers_total_tokens).unwrap_or(0.0),
            })
            .collect();
        provider_rows.sort_by(|a, b| {
            b.tokens
                .cmp(&a.tokens)
                .then_with(|| b.requests.cmp(&a.requests))
                .then_with(|| a.id.cmp(&b.id))
        });

        let mut family_rows: Vec<AntigravityFamilyGroup> = self
            .families
            .into_iter()
            .map(|(family, acc)| AntigravityFamilyGroup {
                family: family.to_owned(),
                label: acc.label.to_owned(),
                requests: acc.requests,
                tokens: acc.tokens,
                reasoning_tokens: acc.reasoning_tokens,
                reasoning_reported: acc.reasoning_reported,
                cost_usd: fmt_usd(acc.cost),
                share_percent: round_pct(acc.tokens, mix_total).unwrap_or(0.0),
            })
            .collect();
        family_rows.sort_by(|a, b| {
            b.tokens
                .cmp(&a.tokens)
                .then_with(|| b.requests.cmp(&a.requests))
                .then_with(|| a.family.cmp(&b.family))
        });

        let total_sessions = self.sessions.len() as u64;

        AntigravityUsageStats {
            requests: self.requests,
            sessions: total_sessions,
            cost_usd: fmt_usd(self.total_cost),
            unpriced_requests: self.unpriced_requests,
            token_mix: AntigravityTokenMix {
                input: self.mix_input,
                cache_read: self.mix_cache_read,
                cache_write: self.mix_cache_write,
                output: self.mix_output,
                visible_output: self.mix_visible_output,
                reasoning: self.mix_reasoning,
                total: mix_total,
                thinking_share_percent,
                cache_hit_percent,
                cache_read_eligible: self.cache_read_eligible,
                cache_input_eligible: self.cache_input_eligible,
                cache_savings_usd: fmt_usd(self.cache_savings),
                reasoning_reported: self.reasoning_reported,
            },
            accounts: {
                let mut accounts: Vec<_> = self.accounts.into_values().collect();
                accounts.sort_by_key(|(order, _)| *order);
                accounts.into_iter().map(|(_, a)| a.finish()).collect()
            },
            quota_error: None,
            providers: provider_rows,
            families: family_rows,
        }
    }
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
        let brain = dir.path().join("antigravity/brain");
        let path = brain.join("cascade/.system_generated/logs/transcript.jsonl");
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
        let files = vec![path.clone()];
        scan_file_list(
            &mut store,
            &brain,
            &files,
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
        scan_file_list(
            &mut store,
            &brain,
            &files,
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
        assert!(scan_file_list(
            &mut store,
            &brain,
            &files,
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

    #[test]
    fn real_planner_response_transcript_imports_with_settings_model_for_export() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir
            .path()
            .join("antigravity/brain/cascade-real/.system_generated/logs/transcript.jsonl");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let rows = concat!(
            "{\"step_index\":0,\"source\":\"USER_EXPLICIT\",\"type\":\"USER_INPUT\",\"status\":\"DONE\",\"created_at\":\"2026-01-01T00:00:00Z\",\"content\":\"<USER_REQUEST>\\nhello\\n</USER_REQUEST>\\n<USER_SETTINGS_CHANGE>\\nThe user changed setting `Model Selection` from None to Gemini 3.1 Pro (High). No need to comment on this change.\\n</USER_SETTINGS_CHANGE>\"}\n",
            "{\"step_index\":1,\"source\":\"MODEL\",\"type\":\"PLANNER_RESPONSE\",\"status\":\"DONE\",\"created_at\":\"2026-01-01T00:00:01Z\",\"input_tokens\":1000,\"cache_read_tokens\":200,\"output_tokens\":80}\n",
            "{\"step_index\":2,\"source\":\"USER_EXPLICIT\",\"type\":\"USER_INPUT\",\"status\":\"DONE\",\"created_at\":\"2026-01-01T00:00:02Z\",\"content\":\"<USER_REQUEST>\\nswitch\\n</USER_REQUEST>\\n<USER_SETTINGS_CHANGE>\\nThe user changed setting `Model Selection` from Gemini 3.1 Pro (High) to Claude Opus 4.6 (Thinking). No need to comment on this change.\\n</USER_SETTINGS_CHANGE>\"}\n",
            "{\"step_index\":3,\"source\":\"MODEL\",\"type\":\"PLANNER_RESPONSE\",\"status\":\"DONE\",\"created_at\":\"2026-01-01T00:00:03Z\",\"input_tokens\":800,\"cache_read_tokens\":100,\"output_tokens\":60}\n",
        );
        std::fs::write(&path, rows).unwrap();
        let mut store = Store::open(&dir.path().join("db")).unwrap();
        let pricing = Pricing::new(&dir.path().join("db")).unwrap();
        let mut report = Report::default();
        scan_file_list(
            &mut store,
            &dir.path().join("antigravity/brain"),
            std::slice::from_ref(&path),
            &pricing,
            &Settings::default(),
            &mut report,
            false,
        )
        .unwrap();
        assert_eq!(report.errors, 0);
        assert_eq!(report.imported, 2);
        let gemini = store
            .detail(&safe_id("antigravity:request:cascade-real:step:1"))
            .unwrap();
        assert_eq!(
            gemini.attempts[0].model_display_name.as_deref(),
            Some("Gemini 3.1 Pro")
        );
        assert_eq!(gemini.attempts[0].reasoning_effort.as_deref(), Some("high"));
        assert_eq!(
            gemini.attempts[0].response_model.as_deref(),
            Some("gemini-3.1-pro")
        );
        assert!(gemini.attempts[0].price.is_some());

        let opus = store
            .detail(&safe_id("antigravity:request:cascade-real:step:3"))
            .unwrap();
        assert_eq!(
            opus.attempts[0].model_display_name.as_deref(),
            Some("Claude Opus 4.6")
        );
        assert_eq!(opus.attempts[0].reasoning_effort, None);
        assert_eq!(
            opus.attempts[0].response_model.as_deref(),
            Some("claude-opus-4-6")
        );
        assert!(opus.attempts[0].price.is_some());

        // Missing or arbitrary natural-language settings text must fail rather than fabricate "antigravity-unknown".
        let bad_path = dir.path().join(
            "antigravity/brain/cascade-missing-model/.system_generated/logs/transcript.jsonl",
        );
        std::fs::create_dir_all(bad_path.parent().unwrap()).unwrap();
        let bad_rows = concat!(
            "{\"step_index\":0,\"source\":\"USER_EXPLICIT\",\"type\":\"USER_INPUT\",\"status\":\"DONE\",\"created_at\":\"2026-01-01T00:00:00Z\",\"content\":\"<USER_REQUEST>\\nhello\\n</USER_REQUEST>\\n<USER_SETTINGS_CHANGE>\\nThe user changed something else to random-words.\\n</USER_SETTINGS_CHANGE>\"}\n",
            "{\"step_index\":1,\"source\":\"MODEL\",\"type\":\"PLANNER_RESPONSE\",\"status\":\"DONE\",\"created_at\":\"2026-01-01T00:00:01Z\",\"input_tokens\":500,\"output_tokens\":40}\n",
        );
        std::fs::write(&bad_path, bad_rows).unwrap();
        let mut bad_report = Report::default();
        scan_file_list(
            &mut store,
            &dir.path().join("antigravity/brain"),
            std::slice::from_ref(&bad_path),
            &pricing,
            &Settings::default(),
            &mut bad_report,
            false,
        )
        .unwrap();
        assert_eq!(bad_report.errors, 1);
        assert_eq!(bad_report.imported, 0);
        assert!(store
            .detail(&safe_id("antigravity:request:cascade-missing-model:step:1"))
            .is_err());
    }
}
