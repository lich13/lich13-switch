//! Shared classification, retry timing and health decisions for HTTP, SSE and WebSocket.
//! Inspect explicit error envelopes only; never scan successful output for keywords.
use super::failure_match::Source;
use crate::events::{Details, ErrorBasis, HealthScope, Reason};
use serde_json::Value;
use std::time::Duration;

#[derive(Clone, Copy)]
pub(super) struct Decision {
    pub temporary: bool,
    pub hard: bool,
    pub retryable: bool,
    pub counted_failure: bool,
    pub reason: Reason,
}

pub(super) fn hard_rejection(d: &Details) -> bool {
    matches!(
        d.error_basis,
        Some(
            ErrorBasis::QuotaExhausted
                | ErrorBasis::Billing
                | ErrorBasis::Authentication
                | ErrorBasis::Permission
        )
    )
}

pub(super) fn temporary(d: &Details) -> bool {
    matches!(
        d.error_basis,
        Some(
            ErrorBasis::Rpm
                | ErrorBasis::Rps
                | ErrorBasis::Tpm
                | ErrorBasis::InputTokens
                | ErrorBasis::OutputTokens
                | ErrorBasis::Concurrency
                | ErrorBasis::Acceleration
                | ErrorBasis::SpendingRate
                | ErrorBasis::UnknownRateLimit
                | ErrorBasis::Capacity
                | ErrorBasis::UsageLimit
        )
    )
}

pub(super) fn decision(status: Option<u16>, d: &Details) -> Decision {
    let hard = hard_rejection(d);
    let temporary = temporary(d);
    let model = d.error_basis == Some(ErrorBasis::ModelUnavailable);
    let rejected = matches!(
        d.error_basis,
        Some(
            ErrorBasis::InvalidRequest | ErrorBasis::RequestTooLarge | ErrorBasis::SafetyRejection
        )
    );
    let service = d.error_basis == Some(ErrorBasis::ServiceError);
    let retryable = !rejected
        && (hard || temporary || model || service || status.is_some_and(retryable_status));
    Decision {
        hard,
        temporary,
        retryable,
        counted_failure: retryable && !hard && !temporary && !model,
        reason: if model {
            Reason::ModelUnavailable
        } else if matches!(
            d.error_basis,
            Some(ErrorBasis::Authentication | ErrorBasis::Permission)
        ) {
            Reason::Authentication
        } else if d.error_basis == Some(ErrorBasis::Capacity) {
            Reason::Capacity
        } else if temporary {
            Reason::RateLimit
        } else if hard || service {
            Reason::UpstreamService
        } else if rejected || status.is_some_and(|s| s < 400) {
            Reason::ProtocolError
        } else {
            Reason::UpstreamService
        },
    }
}

pub(super) fn retryable_status(status: u16) -> bool {
    matches!(status, 401 | 402 | 403 | 408 | 425 | 429 | 500 | 502..=599)
}

pub(super) fn outcome(
    status: Option<u16>,
    d: &Details,
    retry: Option<Duration>,
) -> super::circuit::Outcome {
    use super::circuit::Outcome;
    let policy = decision(status, d);
    if policy.hard {
        Outcome::HardRejected(retry)
    } else if d.error_basis == Some(ErrorBasis::Capacity) {
        Outcome::CapacityLimited(retry)
    } else if policy.temporary {
        Outcome::RateLimited(retry)
    } else if policy.counted_failure {
        Outcome::Failure(retry)
    } else {
        Outcome::Neutral
    }
}

// Exponential local backoff never shortens a server deadline. A stable salt
// makes the calculation testable; callers obtain it once for each rejection.
pub(super) fn rate_delay(base: u64, streak: u32, server: Option<Duration>, salt: u16) -> Duration {
    let seconds = base
        .saturating_mul(1u64 << streak.saturating_sub(1).min(16))
        .min(base.max(60));
    let local = Duration::from_secs(seconds);
    let floor = local.max(server.unwrap_or_default());
    floor.saturating_add(Duration::from_millis(
        u64::from(salt % 101).saturating_mul(seconds),
    ))
}

fn structured_codes(error: &Value) -> impl Iterator<Item = &str> {
    error
        .pointer("/details/error_code")
        .and_then(Value::as_str)
        .into_iter()
        .chain(
            ["code", "reason", "errorKey"]
                .into_iter()
                .filter_map(|k| error.get(k).and_then(Value::as_str)),
        )
        .chain(
            error
                .get("details")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter(|d| {
                    d.get("@type").and_then(Value::as_str)
                        == Some("type.googleapis.com/google.rpc.ErrorInfo")
                })
                .filter_map(|d| d.get("reason").and_then(Value::as_str)),
        )
}

fn explicit_type(error: &Value) -> Option<ErrorBasis> {
    let kind = error.get("type").and_then(Value::as_str)?;
    // Generic wrappers do not override concrete reasons inside the envelope.
    (!matches!(
        kind,
        "invalid_request_error" | "invalid_request" | "rate_limit_error"
    ))
    .then(|| code_kind(kind))
    .flatten()
}

fn code_kind(code: &str) -> Option<ErrorBasis> {
    let code = code.to_ascii_lowercase();
    super::failure_rules::CODES
        .iter()
        .find(|r| r.value == code)
        .map(|r| r.category)
        .or_else(|| is_permanent_reason(Some(&code)).then_some(ErrorBasis::QuotaExhausted))
        .or_else(|| model_message(&code).then_some(ErrorBasis::ModelUnavailable))
}

fn positive_message(text: &str) -> bool {
    !super::failure_rules::QUOTA_NEGATIVES
        .iter()
        .any(|p| text.to_lowercase().contains(p))
}

/// Some compatible providers return this rejection without an error envelope.
/// Match the entire response, never quoted model output or an arbitrary substring.
pub(super) fn bare_capacity(text: &str) -> bool {
    let normalized = text
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    matches!(
        normalized.trim_end_matches('.'),
        "selected model is at capacity"
            | "selected model is at capacity. please try a different model"
    )
}

fn message_kind(text: &str) -> Option<ErrorBasis> {
    use ErrorBasis::*;
    if !positive_message(text) {
        return None;
    }
    let s = text.to_lowercase();
    let has = |words: &[&str]| words.iter().any(|w| s.contains(w));
    if has(super::failure_rules::MSG_0)
        || (has(super::failure_rules::MSG_1) && has(super::failure_rules::MSG_2))
    {
        return Some(RequestTooLarge);
    }
    if has(super::failure_rules::MSG_3) {
        return Some(SafetyRejection);
    }
    if model_message(text) {
        return Some(ModelUnavailable);
    }
    if quota_message(text) {
        return Some(QuotaExhausted);
    }
    if has(super::failure_rules::MSG_4) {
        return Some(SpendingRate);
    }
    if has(super::failure_rules::MSG_5) {
        return Some(QuotaExhausted);
    }
    if has(super::failure_rules::MSG_6) {
        return Some(Authentication);
    }
    if hard_message(text) {
        return Some(Billing);
    }
    if has(super::failure_rules::MSG_7) {
        return Some(Permission);
    }
    if has(super::failure_rules::MSG_8) {
        return Some(InputTokens);
    }
    if has(super::failure_rules::MSG_9) {
        return Some(OutputTokens);
    }
    if has(super::failure_rules::MSG_10) {
        return Some(Tpm);
    }
    if has(super::failure_rules::MSG_11) {
        return Some(Concurrency);
    }
    if has(super::failure_rules::MSG_12) {
        return Some(Rps);
    }
    if rpm_message(text) {
        return Some(Rpm);
    }
    if has(super::failure_rules::MSG_13) {
        return Some(Acceleration);
    }
    if s.trim() == "capacity" || has(super::failure_rules::MSG_14) {
        return Some(Capacity);
    }
    if has(super::failure_rules::MSG_15) {
        return Some(UsageLimit);
    }
    if has(super::failure_rules::MSG_16) {
        return Some(UnknownRateLimit);
    }
    None
}

fn quota_details(error: &Value) -> Option<(ErrorBasis, Option<HealthScope>)> {
    let mut found = None;
    for d in error
        .get("details")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .take(32)
    {
        if d.get("@type").and_then(Value::as_str)
            != Some("type.googleapis.com/google.rpc.QuotaFailure")
        {
            continue;
        }
        for v in d
            .get("violations")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .take(32)
        {
            let metric = ["quotaId", "quotaMetric"]
                .iter()
                .filter_map(|k| v.get(k).and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join(" ")
                .chars()
                .filter(char::is_ascii_alphanumeric)
                .collect::<String>()
                .to_ascii_lowercase();
            let scope =
                if v.pointer("/quotaDimensions/model").is_some() || metric.contains("permodel") {
                    HealthScope::Model
                } else {
                    HealthScope::Provider
                };
            let zero = v.get("quotaValue").is_some_and(|v| v == 0 || v == "0");
            let basis = if zero
                || ["perday", "perweek", "permonth"]
                    .iter()
                    .any(|p| metric.contains(p))
            {
                ErrorBasis::QuotaExhausted
            } else if metric.contains("spend") {
                ErrorBasis::SpendingRate
            } else if metric.contains("input") && metric.contains("token") {
                ErrorBasis::InputTokens
            } else if metric.contains("output") && metric.contains("token") {
                ErrorBasis::OutputTokens
            } else if metric.contains("token") {
                ErrorBasis::Tpm
            } else if metric.contains("persecond") {
                ErrorBasis::Rps
            } else if metric.contains("perminute") && metric.contains("request") {
                ErrorBasis::Rpm
            } else {
                continue;
            };
            // A hard window dominates a transient window; any project-wide
            // hard violation must not be narrowed to another model's metric.
            if found.is_none() || basis == ErrorBasis::QuotaExhausted {
                found = Some((basis, Some(scope)));
            }
            if found == Some((ErrorBasis::QuotaExhausted, Some(HealthScope::Provider))) {
                return found;
            }
        }
    }
    found
}

fn classification(error: &Value) -> (Option<ErrorBasis>, Option<HealthScope>) {
    let (basis, scope, _) = classification_trace(error);
    (basis, scope)
}

fn classification_trace(
    error: &Value,
) -> (Option<ErrorBasis>, Option<HealthScope>, Option<Source>) {
    let explicit = structured_codes(error)
        .find_map(code_kind)
        .or_else(|| {
            error
                .as_str()
                .and_then(|_| known_reason(error))
                .as_deref()
                .and_then(code_kind)
        })
        .map(|b| (b, Source::BuiltinCode));
    let quota = quota_details(error);
    let explicit = explicit.or_else(|| explicit_type(error).map(|b| (b, Source::BuiltinType)));
    let matched = explicit
        .or_else(|| quota.map(|q| (q.0, Source::BuiltinCondition)))
        .or_else(|| {
            error
                .as_str()
                .into_iter()
                .chain(
                    ["message", "detail", "errorMessage"]
                        .into_iter()
                        .filter_map(|k| error.get(k).and_then(Value::as_str)),
                )
                .find_map(message_kind)
                .map(|b| (b, Source::BuiltinMessage))
        })
        .or_else(|| {
            is_model(error).then_some((ErrorBasis::ModelUnavailable, Source::BuiltinCondition))
        })
        .or_else(|| {
            error
                .get("type")
                .and_then(Value::as_str)
                .and_then(code_kind)
                .map(|b| (b, Source::BuiltinType))
        })
        .or_else(|| {
            strings(error)
                .find_map(message_kind)
                .map(|b| (b, Source::BuiltinMessage))
        })
        .or_else(|| {
            let status = error.get("status").or_else(|| error.get("code"))?;
            let number = status
                .as_u64()
                .or_else(|| status.as_str().and_then(|s| s.parse().ok()));
            (match number {
                Some(401) => Some(ErrorBasis::Authentication),
                Some(402) => Some(ErrorBasis::Billing),
                Some(403) => Some(ErrorBasis::Permission),
                Some(429) => Some(ErrorBasis::UnknownRateLimit),
                Some(413 | 414) => Some(ErrorBasis::RequestTooLarge),
                Some(400 | 405 | 406 | 409 | 415 | 422 | 426 | 501) => {
                    Some(ErrorBasis::InvalidRequest)
                }
                Some(500 | 502..=599) => Some(ErrorBasis::ServiceError),
                _ => status.as_str().and_then(|value| {
                    super::failure_rules::STATUS_CODES
                        .iter()
                        .find(|c| c.value == value)
                        .map(|c| c.category)
                }),
            })
            .map(|b| (b, Source::Status))
        });
    let basis = matched.map(|m| m.0);
    let explicit = explicit.map(|m| m.0);
    let scope = if quota.is_some() && (explicit.is_none() || quota.map(|q| q.0) == explicit) {
        quota.and_then(|q| q.1)
    } else if matches!(basis, Some(ErrorBasis::ModelUnavailable))
        || (basis == Some(ErrorBasis::Permission)
            && (error.get("param").and_then(Value::as_str) == Some("model")
                || error
                    .get("message")
                    .and_then(Value::as_str)
                    .is_some_and(model_message)))
    {
        Some(HealthScope::Model)
    } else if matches!(
        basis,
        Some(
            ErrorBasis::InvalidRequest | ErrorBasis::RequestTooLarge | ErrorBasis::SafetyRejection
        )
    ) {
        Some(HealthScope::Request)
    } else if matches!(
        basis,
        Some(
            ErrorBasis::Rpm
                | ErrorBasis::Rps
                | ErrorBasis::QuotaExhausted
                | ErrorBasis::Billing
                | ErrorBasis::Authentication
                | ErrorBasis::Permission
        )
    ) || (basis.is_some()
        && error
            .get("message")
            .and_then(Value::as_str)
            .is_some_and(|message| {
                positive_message(message)
                    && [
                        "organization",
                        "project",
                        "workspace",
                        "account",
                        "组织",
                        "项目",
                        "账户",
                        "账号",
                    ]
                    .iter()
                    .any(|s| message.to_lowercase().contains(s))
            }))
    {
        Some(HealthScope::Provider)
    } else {
        None
    };
    (basis, scope, matched.map(|m| m.1))
}

#[cfg(test)]
pub(super) fn http(
    status: u16,
    headers: &hyper::HeaderMap,
    body: &[u8],
) -> (Details, Option<Duration>) {
    http_with_rules(status, headers, body, &[])
}
pub(super) fn http_with_rules(
    status: u16,
    headers: &hyper::HeaderMap,
    body: &[u8],
    rules: &[super::failure_rules::Rule],
) -> (Details, Option<Duration>) {
    let value = serde_json::from_slice::<Value>(body).unwrap_or_else(|_| {
        if status >= 400 {
            std::str::from_utf8(body)
                .map(|s| Value::String(s.into()))
                .unwrap_or_default()
        } else {
            Value::Null
        }
    });
    http_value_with_rules(status, headers, &value, rules)
}

pub(super) fn http_value_with_rules(
    status: u16,
    headers: &hyper::HeaderMap,
    value: &Value,
    rules: &[super::failure_rules::Rule],
) -> (Details, Option<Duration>) {
    let mut d = details_value(value, status >= 400);
    let structured = envelope(value, true, 0).and_then(|e| {
        structured_codes(e)
            .find_map(code_kind)
            .or_else(|| explicit_type(e))
            .or_else(|| quota_details(e).map(|q| q.0))
    });
    if !retryable_status(status) && status >= 400 && temporary(&d) && structured.is_none() {
        d.error_basis = Some(ErrorBasis::InvalidRequest);
        d.health_scope = Some(HealthScope::Request);
    }
    if d.error_basis.is_none() {
        d.error_basis = match status {
            401 => Some(ErrorBasis::Authentication),
            402 => Some(ErrorBasis::Billing),
            403 => Some(ErrorBasis::Permission),
            429 => Some(ErrorBasis::UnknownRateLimit),
            413 | 414 => Some(ErrorBasis::RequestTooLarge),
            400 | 405 | 406 | 409 | 415 | 422 | 426 | 501 => Some(ErrorBasis::InvalidRequest),
            _ => None,
        };
        if hard_rejection(&d) {
            d.health_scope = Some(HealthScope::Provider);
        }
    }
    refine_headers(&mut d, headers, structured.is_some());
    if let Some(error) = envelope(value, status >= 400, 0) {
        apply_custom(&mut d, error, headers, rules);
    }
    let retry = envelope(value, true, 0)
        .and_then(retry_after_fields)
        .max(retry_headers(headers, &d));
    if status < 400 && d.error_basis.is_none() && has_error(value) {
        d.error_basis = Some(ErrorBasis::ServiceError);
    }
    (d, retry)
}

fn refine_headers(d: &mut Details, headers: &hyper::HeaderMap, structured: bool) {
    // Only actual error envelopes can contribute limit evidence.
    if !structured && temporary(d) {
        let exhausted = |name: &str| {
            headers
                .get(name)
                .and_then(|h| h.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok())
                == Some(0)
        };
        let Some((_, basis)) = super::failure_rules::LIMIT_HEADERS
            .iter()
            .find(|(name, _)| exhausted(name))
        else {
            return;
        };
        d.error_basis = Some(*basis);
        if matches!(basis, ErrorBasis::Rpm | ErrorBasis::Rps) {
            d.health_scope = Some(HealthScope::Provider);
        }
        if !matches!(d.error_basis, Some(ErrorBasis::Rpm | ErrorBasis::Rps)) {
            d.health_scope = None;
        }
    }
}

pub(super) fn details_with_headers(
    value: &Value,
    headers: &hyper::HeaderMap,
    rules: &[super::failure_rules::Rule],
) -> (Details, Option<Duration>) {
    let mut details = details_value(value, false);
    let Some(error) = envelope(value, false, 0) else {
        return (details, None);
    };
    let structured = structured_codes(error)
        .find_map(code_kind)
        .or_else(|| explicit_type(error))
        .is_some()
        || quota_details(error).is_some();
    refine_headers(&mut details, headers, structured);
    apply_custom(&mut details, error, headers, rules);
    details.error_basis.get_or_insert(ErrorBasis::ServiceError);
    let wait = retry_after_fields(error).max(retry_headers(headers, &details));
    (details, wait)
}

pub(super) fn limit_headers(headers: &hyper::HeaderMap) -> hyper::HeaderMap {
    headers
        .iter()
        .filter(|(name, value)| {
            value.len() <= 80
                && (name.as_str() == "retry-after"
                    || name.as_str().starts_with("x-ratelimit-")
                    || name.as_str().starts_with("anthropic-ratelimit-"))
        })
        .take(32)
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect()
}

fn duration_value(value: &str) -> Option<Duration> {
    if value.is_empty() || value.len() > 80 {
        return None;
    }
    if let Some(v) = super::circuit::retry_after(value) {
        return Some(v);
    }
    if let Ok(at) = chrono::DateTime::parse_from_rfc3339(value) {
        return Some(Duration::from_millis(
            (at.timestamp_millis() - chrono::Utc::now().timestamp_millis()).max(0) as u64,
        ));
    }
    let mut rest = value;
    let mut total = Duration::ZERO;
    for _ in 0..8 {
        let end = rest.find(|c: char| !c.is_ascii_digit() && c != '.')?;
        let number = super::circuit::retry_seconds(&rest[..end])?;
        rest = &rest[end..];
        let (unit, scale) = [("ms", 0), ("s", 1), ("m", 60), ("h", 3600), ("d", 86400)]
            .into_iter()
            .find(|(u, _)| rest.starts_with(u))?;
        total = total.checked_add(if scale == 0 {
            number / 1000
        } else {
            number.checked_mul(scale)?
        })?;
        rest = &rest[unit.len()..];
        if rest.is_empty() {
            return Some(total);
        }
    }
    None
}

pub(super) fn retry_headers(headers: &hyper::HeaderMap, d: &Details) -> Option<Duration> {
    let mut retry = headers
        .get(hyper::header::RETRY_AFTER)
        .and_then(|h| h.to_str().ok())
        .and_then(super::circuit::retry_after);
    if !temporary(d) {
        return retry;
    }
    for (remaining, reset, basis) in [
        (
            "x-ratelimit-remaining-requests",
            "x-ratelimit-reset-requests",
            ErrorBasis::Rpm,
        ),
        (
            "x-ratelimit-remaining-tokens",
            "x-ratelimit-reset-tokens",
            ErrorBasis::Tpm,
        ),
        (
            "x-ratelimit-remaining-project-tokens",
            "x-ratelimit-reset-project-tokens",
            ErrorBasis::Tpm,
        ),
        (
            "anthropic-ratelimit-requests-remaining",
            "anthropic-ratelimit-requests-reset",
            ErrorBasis::Rpm,
        ),
        (
            "anthropic-ratelimit-tokens-remaining",
            "anthropic-ratelimit-tokens-reset",
            ErrorBasis::Tpm,
        ),
        (
            "anthropic-ratelimit-input-tokens-remaining",
            "anthropic-ratelimit-input-tokens-reset",
            ErrorBasis::InputTokens,
        ),
        (
            "anthropic-ratelimit-output-tokens-remaining",
            "anthropic-ratelimit-output-tokens-reset",
            ErrorBasis::OutputTokens,
        ),
    ] {
        if headers
            .get(remaining)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok())
            == Some(0)
            || (!headers.contains_key(remaining) && d.error_basis == Some(basis))
        {
            retry = retry.max(
                headers
                    .get(reset)
                    .and_then(|v| v.to_str().ok())
                    .and_then(duration_value),
            );
        }
    }
    retry
}
pub fn capacity_message(message: &str) -> bool {
    if rpm_message(message) {
        return true;
    }
    let s = message.to_lowercase();
    [
        "at capacity",
        "too many requests",
        "rate limit",
        "rate_limit",
        "overload",
        "model_capacity",
        "usage_limit",
        "slow_down",
        "容量",
        "限流",
        "繁忙",
        "负载过高",
    ]
    .iter()
    .any(|p| s.contains(p))
}
pub fn model_message(message: &str) -> bool {
    if !positive_message(message) || capacity_message(message) {
        return false;
    }
    let s = message.trim().to_lowercase();
    super::failure_rules::MODEL_CODES.contains(&s.as_str())
        || (s.contains("model") && super::failure_rules::MODEL_EN.iter().any(|p| s.contains(p)))
        || (s.contains("模型") && super::failure_rules::MODEL_ZH.iter().any(|p| s.contains(p)))
}
// Explicit wrappers only: never recurse into arbitrary user content.
pub(super) fn has_error(value: &Value) -> bool {
    envelope(value, false, 0).is_some()
}

pub(super) fn envelope(v: &Value, http_error: bool, depth: usize) -> Option<&Value> {
    if depth > 5
        || v.get("success") == Some(&Value::Bool(true))
        || v.get("status")
            .is_some_and(|s| s == "completed" || s == "succeeded")
        || v.get("type").is_some_and(|s| s == "response.completed")
    {
        return None;
    }
    if let Some(error) = v
        .get("error")
        .filter(|e| !e.is_null() && **e != Value::Bool(false))
    {
        return envelope(error, true, depth + 1).or(Some(error));
    }
    let kind = v.get("type").and_then(Value::as_str).unwrap_or("");
    let explicit = kind == "error"
        || kind == "response.failed"
        || kind.ends_with("_error")
        || v.get("success") == Some(&Value::Bool(false))
        || v.get("status").is_some_and(|s| s == "failed");
    for key in ["response", "data", "result"] {
        if let Some(inner) = v.get(key).filter(|x| x.is_object()) {
            if let Some(error) = envelope(inner, http_error || explicit, depth + 1) {
                return Some(error);
            }
        }
    }
    let bare = ![
        "output",
        "choices",
        "candidates",
        "content",
        "response",
        "result",
        "data",
    ]
    .iter()
    .any(|key| v.get(key).is_some())
        && v.as_str()
            .or_else(|| v.get("message").and_then(Value::as_str))
            .is_some_and(bare_capacity);
    let known_code = !["output", "choices", "candidates", "content"]
        .iter()
        .any(|key| v.get(key).is_some())
        && structured_codes(v).any(|code| code_kind(code).is_some());
    if explicit || bare || known_code {
        return Some(v);
    }
    if (http_error || explicit)
        && (v.is_string()
            || [
                "code",
                "type",
                "reason",
                "message",
                "detail",
                "errorKey",
                "errorMessage",
            ]
            .iter()
            .any(|k| v.get(k).is_some()))
    {
        Some(v)
    } else {
        None
    }
}
fn strings(error: &Value) -> impl Iterator<Item = &str> {
    error.as_str().into_iter().chain(
        [
            "code",
            "type",
            "reason",
            "message",
            "detail",
            "errorKey",
            "errorMessage",
        ]
        .into_iter()
        .filter_map(move |k| error.get(k).and_then(Value::as_str)),
    )
}

// Read named fields from explicit textual error envelopes.
fn text_field<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    let lower = text.to_ascii_lowercase();
    for (at, _) in lower.match_indices(&name.to_ascii_lowercase()) {
        if at > 0
            && text[..at]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_alphanumeric() || c == '_')
        {
            continue;
        }
        let rest = text[at + name.len()..].trim_start();
        let Some(rest) = rest.strip_prefix('=').or_else(|| rest.strip_prefix(':')) else {
            continue;
        };
        let rest = rest.trim_start();
        if let Some(quote) = rest.chars().next().filter(|c| matches!(c, '\'' | '"')) {
            return rest[1..].split_once(quote).map(|(value, _)| value);
        }
        return rest
            .split(|c: char| c.is_whitespace() || matches!(c, ',' | ';' | '}'))
            .next();
    }
    None
}
fn known_reason(error: &Value) -> Option<String> {
    let safe = crate::events::safe_upstream_reason;
    error
        .get("reason")
        .and_then(Value::as_str)
        .and_then(safe)
        .or_else(|| error.get("errorKey").and_then(Value::as_str).and_then(safe))
        .or_else(|| error.get("code").and_then(Value::as_str).and_then(safe))
        .or_else(|| error.get("type").and_then(Value::as_str).and_then(safe))
        .or_else(|| {
            ["message", "detail", "errorMessage"]
                .into_iter()
                .filter_map(|key| error.get(key).and_then(Value::as_str))
                .find_map(safe)
        })
        .or_else(|| {
            let text = error.as_str()?.trim_start();
            text.get(..6)
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case("error:"))
                .then(|| {
                    [
                        "reason",
                        "message",
                        "detail",
                        "errorMessage",
                        "code",
                        "type",
                    ]
                    .into_iter()
                    .find_map(|field| text_field(text, field).and_then(safe))
                })
                .flatten()
        })
}
pub(super) fn is_permanent_reason(reason: Option<&str>) -> bool {
    reason.is_some_and(|value| {
        super::failure_rules::PERMANENT_CODES
            .iter()
            .any(|known| value.eq_ignore_ascii_case(known))
    })
}
fn is_model(error: &Value) -> bool {
    if is_permanent_reason(known_reason(error).as_deref())
        || strings(error).any(|s| capacity_message(s) || global_fault(s))
    {
        return false;
    }
    if error
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|t| t.eq_ignore_ascii_case("not_found_error"))
        && error
            .get("message")
            .and_then(Value::as_str)
            .is_some_and(|m| m.trim_start().to_lowercase().starts_with("model:"))
    {
        return true;
    }
    strings(error).any(model_message)
}
pub fn model_error(value: &Value) -> bool {
    envelope(value, false, 0)
        .is_some_and(|e| classification(e).0 == Some(ErrorBasis::ModelUnavailable))
}
#[cfg(test)]
pub fn temporary_capacity_event(value: &Value) -> bool {
    temporary(&details_value(value, false))
}
/// Explicit retry/reset fields from an error envelope. Free-form text
/// and successful response content are never interpreted as timing evidence.
#[cfg(test)]
pub(super) fn retry_after_value(value: &Value) -> Option<std::time::Duration> {
    retry_after_fields(envelope(value, false, 0)?)
}
fn retry_after_fields(error: &Value) -> Option<std::time::Duration> {
    let number = |v: &Value| v.as_u64().or_else(|| v.as_str()?.parse().ok());
    let relative = ["retry_after", "retry_after_seconds", "resets_in_seconds"]
        .into_iter()
        .filter_map(|key| error.get(key))
        .filter_map(|v| {
            super::circuit::retry_seconds(
                &v.as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| v.to_string()),
            )
        })
        .max();
    let now = chrono::Utc::now().timestamp().max(0) as u64;
    let absolute = ["resets_at", "reset_at", "reset_at_unix"]
        .into_iter()
        .filter_map(|key| error.get(key))
        .filter_map(|value| {
            number(value).or_else(|| {
                chrono::DateTime::parse_from_rfc3339(value.as_str()?)
                    .ok()
                    .and_then(|date| u64::try_from(date.timestamp()).ok())
            })
        })
        .filter(|at| *at <= 253_402_300_799)
        .map(|at| at.saturating_sub(now))
        .max();
    let nested = error
        .get("details")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|d| {
            d.get("@type").and_then(Value::as_str)
                == Some("type.googleapis.com/google.rpc.RetryInfo")
        })
        .filter_map(|d| d.get("retryDelay").and_then(Value::as_str))
        .filter_map(duration_value)
        .max();
    relative.max(absolute.map(Duration::from_secs)).max(nested)
}
#[cfg(test)]
pub(super) fn retry_after_http(bytes: &[u8]) -> Option<std::time::Duration> {
    let value = serde_json::from_slice::<Value>(bytes).ok()?;
    retry_after_fields(envelope(&value, true, 0)?)
}
pub fn model_http(status: u16, bytes: &[u8]) -> bool {
    if status < 400 {
        return false;
    }
    match serde_json::from_slice::<Value>(bytes) {
        Ok(v) => envelope(&v, true, 0)
            .is_some_and(|e| classification(e).0 == Some(ErrorBasis::ModelUnavailable)),
        Err(_) => std::str::from_utf8(bytes).is_ok_and(model_message),
    }
}
pub fn details_value(value: &Value, http_error: bool) -> Details {
    let Some(error) = envelope(value, http_error, 0) else {
        return Details::default();
    };
    let (basis, scope) = classification(error);
    Details {
        error_basis: basis,
        upstream_reason: error
            .get("reason")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| structured_codes(error).find_map(crate::events::safe_upstream_reason))
            .or_else(|| known_reason(error)),
        health_scope: scope,
        upstream_code: error
            .get("code")
            .or_else(|| error.get("errorKey"))
            .and_then(|v| match v {
                Value::String(s) => Some(s.clone()),
                Value::Number(n) if n.is_i64() || n.is_u64() => Some(n.to_string()),
                _ => None,
            })
            .or_else(|| strings(error).find_map(|s| text_field(s, "code").map(str::to_owned))),
        upstream_type: error.get("type").and_then(Value::as_str).map(str::to_owned),
        parameter: error
            .get("param")
            .and_then(Value::as_str)
            .map(str::to_owned),
        message: error
            .as_str()
            .or_else(|| error.get("message").and_then(Value::as_str))
            .or_else(|| error.get("detail").and_then(Value::as_str))
            .or_else(|| error.get("errorMessage").and_then(Value::as_str))
            .map(str::to_owned),
        ..Details::default()
    }
    .sanitized()
}
#[cfg(test)]
pub fn details_http(bytes: &[u8]) -> Details {
    match serde_json::from_slice::<Value>(bytes) {
        Ok(v) => details_value(&v, true),
        Err(_) => std::str::from_utf8(bytes).map_or_else(
            |_| Details::default(),
            |text| details_value(&Value::String(text.into()), true),
        ),
    }
}
/// Explicit billing/account denials cannot recover by retrying every minute.
/// Status 429 alone is deliberately insufficient to classify a permanent denial.
#[cfg(test)]
pub(super) fn permanent_rejection(body: &[u8]) -> bool {
    hard_rejection(&details_http(body))
}

fn hard_message(text: &str) -> bool {
    if quota_message(text) {
        return true;
    }
    let text = text.to_ascii_lowercase();
    super::failure_rules::BILLING_WORDS
        .iter()
        .any(|marker| text.contains(marker))
}

fn quota_message(text: &str) -> bool {
    if is_permanent_reason(Some(text.trim())) {
        return true;
    }
    if text
        .trim_start()
        .get(..6)
        .is_some_and(|s| s.eq_ignore_ascii_case("error:"))
        && ["code", "type", "reason"]
            .iter()
            .any(|field| is_permanent_reason(text_field(text, field)))
    {
        return true;
    }
    let text = text.to_lowercase();
    if super::failure_rules::QUOTA_NEGATIVES
        .iter()
        .any(|s| text.contains(s))
    {
        return false;
    }
    super::failure_rules::QUOTA_WORDS
        .iter()
        .any(|s| text.contains(s))
}
fn rpm_message(text: &str) -> bool {
    let s = text.to_ascii_lowercase();
    if super::failure_rules::RPM_NEGATIVES
        .iter()
        .any(|negative| s.contains(negative))
    {
        return false;
    }
    super::failure_rules::RPM_WORDS
        .iter()
        .any(|m| s.contains(m))
        || minute_limit_message(s.trim())
}

// Explicit new-api total/success request-limit messages. Require the whole
// envelope message and positive numeric limits, not generic quota prose.
fn minute_limit_message(text: &str) -> bool {
    let count = |value: &str| {
        !value.is_empty()
            && value.bytes().all(|c| c.is_ascii_digit())
            && value.parse::<u32>().is_ok_and(|n| n > 0)
    };
    [
        (
            "you have reached the request limit: maximum ",
            " requests in ",
            " minutes",
        ),
        (
            "you have reached the total request limit: maximum ",
            " requests in ",
            " minutes, including failed attempts. please check that your requests are correct",
        ),
        ("您已达到请求数限制：", "分钟内最多请求", "次"),
        (
            "您已达到总请求数限制：",
            "分钟内最多请求",
            "次，包括失败次数，请检查您的请求是否正确",
        ),
        ("您已達到請求數限制：", "分鐘內最多請求", "次"),
        (
            "您已達到總請求數限制：",
            "分鐘內最多請求",
            "次，包括失敗次數，請檢查您的請求是否正確",
        ),
    ]
    .iter()
    .any(|(prefix, separator, suffix)| {
        text.strip_prefix(prefix)
            .and_then(|s| s.strip_suffix(suffix))
            .and_then(|s| s.split_once(separator))
            .is_some_and(|(a, b)| count(a) && count(b))
    })
}
// Only explicit account/key-wide evidence broadens health scope.
fn global_fault(text: &str) -> bool {
    if model_message(text) {
        return false;
    }
    let lower = text.to_lowercase();
    hard_message(text)
        || [
            "invalid_api_key",
            "invalid api key",
            "incorrect api key",
            "api_key_expired",
            "api_key_revoked",
            "api_key_disabled",
            "authentication_error",
            "account_suspended",
            "api key is invalid",
            "api key has expired",
            "密钥无效",
            "密钥已过期",
            "密钥已撤销",
        ]
        .iter()
        .any(|s| lower.contains(s))
}

fn vague(basis: ErrorBasis) -> bool {
    matches!(
        basis,
        ErrorBasis::Unknown | ErrorBasis::UnknownRateLimit | ErrorBasis::UsageLimit
    )
}
fn apply_custom(
    details: &mut Details,
    error: &Value,
    headers: &hyper::HeaderMap,
    rules: &[super::failure_rules::Rule],
) {
    use super::failure_rules::MatchKind;
    if rules.is_empty() {
        return;
    }
    let specific = structured_codes(error)
        .filter_map(code_kind)
        .chain(explicit_type(error))
        .chain(quota_details(error).map(|q| q.0))
        .chain(strings(error).filter_map(message_kind))
        .any(|b| !vague(b))
        || is_model(error)
        || error
            .get("status")
            .and_then(Value::as_str)
            .is_some_and(|status| {
                super::failure_rules::STATUS_CODES
                    .iter()
                    .any(|code| code.value == status && !vague(code.category))
            });
    let header_evidence = temporary(details)
        && super::failure_rules::LIMIT_HEADERS.iter().any(|(name, _)| {
            headers
                .get(*name)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                == Some(0)
        });
    if specific || header_evidence {
        return;
    }
    let codes: Vec<_> = [
        details.upstream_code.as_deref(),
        details.upstream_reason.as_deref(),
        details.upstream_type.as_deref(),
        error.get("status").and_then(Value::as_str),
    ]
    .into_iter()
    .flatten()
    .chain(structured_codes(error))
    .collect();
    let message = details.message.as_deref().unwrap_or("").to_lowercase();
    for kind in [MatchKind::Code, MatchKind::Keyword] {
        let matched: Vec<_> = rules
            .iter()
            .filter(|r| {
                r.match_kind == kind
                    && match kind {
                        MatchKind::Code => {
                            codes.iter().any(|code| code.eq_ignore_ascii_case(&r.value))
                        }
                        MatchKind::Keyword => {
                            !message.is_empty()
                                && positive_message(&message)
                                && message.contains(&r.value.to_lowercase())
                        }
                    }
            })
            .collect();
        let Some(first) = matched.first() else {
            continue;
        };
        if matched.iter().any(|r| r.category != first.category) {
            details.rule_conflict = Some(true);
            return;
        }
        details.error_basis = Some(first.category);
        details.custom_rule_id = Some(first.id.clone());
        details.health_scope = match first.category {
            ErrorBasis::InvalidRequest
            | ErrorBasis::RequestTooLarge
            | ErrorBasis::SafetyRejection => Some(HealthScope::Request),
            ErrorBasis::ModelUnavailable => Some(HealthScope::Model),
            ErrorBasis::Rpm
            | ErrorBasis::Rps
            | ErrorBasis::QuotaExhausted
            | ErrorBasis::Billing
            | ErrorBasis::Authentication
            | ErrorBasis::Permission => Some(HealthScope::Provider),
            _ => None,
        };
        return;
    }
}
pub(super) fn details_with_rules(
    value: &Value,
    http_error: bool,
    rules: &[super::failure_rules::Rule],
) -> Details {
    let mut details = details_value(value, http_error);
    if let Some(error) = envelope(value, http_error, 0) {
        apply_custom(&mut details, error, &hyper::HeaderMap::new(), rules);
        if !http_error {
            details.error_basis.get_or_insert(ErrorBasis::ServiceError);
        }
    }
    details
}

pub(super) fn inspect_match_headers(
    value: &Value,
    status: Option<u16>,
    headers: &hyper::HeaderMap,
    rules: &[super::failure_rules::Rule],
    catalog: &[super::failure_rules::Category],
) -> (Details, Option<Source>, String) {
    let mut details = match status {
        Some(status) => http_value_with_rules(status, headers, value, rules).0,
        None => details_with_headers(value, headers, rules).0,
    };
    // The live decision treats retryable HTTP failures without a finer basis as service failures.
    if details.error_basis.is_none() && decision(status, &details).counted_failure {
        details.error_basis = Some(ErrorBasis::ServiceError);
    }
    let Some(basis) = details.error_basis.filter(|b| *b != ErrorBasis::Unknown) else {
        return (details, None, String::new());
    };
    if let Some(rule) = rules
        .iter()
        .find(|r| Some(&r.id) == details.custom_rule_id.as_ref())
    {
        let source = match rule.match_kind {
            super::failure_rules::MatchKind::Code => Source::CustomCode,
            super::failure_rules::MatchKind::Keyword => Source::CustomKeyword,
        };
        return (details, Some(source), rule.value.clone());
    }
    let error = envelope(value, status.is_some_and(|s| s >= 400), 0);
    let trace = error.map(classification_trace);
    let source = trace
        .filter(|t| t.0 == Some(basis))
        .and_then(|t| t.2)
        .unwrap_or(Source::Status);
    let evidence = match source {
        Source::BuiltinCode => error
            .and_then(|e| structured_codes(e).find(|c| code_kind(c) == Some(basis)))
            .map(str::to_owned)
            .unwrap_or_else(|| "内置错误码条件".into()),
        Source::BuiltinType => details
            .upstream_type
            .clone()
            .unwrap_or_else(|| "内置错误类型".into()),
        Source::BuiltinCondition => "内置结构化条件".into(),
        Source::BuiltinMessage => {
            let text = error
                .and_then(|e| strings(e).find(|s| message_kind(s) == Some(basis)))
                .unwrap_or("")
                .to_lowercase();
            catalog
                .iter()
                .find(|c| c.category == basis)
                .and_then(|c| {
                    c.keywords.iter().find_map(|condition| {
                        let words: Option<Vec<_>> = condition
                            .all_of
                            .iter()
                            .map(|group| {
                                group
                                    .iter()
                                    .find(|word| {
                                        if condition.exact {
                                            text.trim() == **word
                                        } else {
                                            text.contains(**word)
                                        }
                                    })
                                    .copied()
                            })
                            .collect();
                        words.map(|words| words.join(" 且 "))
                    })
                })
                .unwrap_or_else(|| "内置消息判断条件".into())
        }
        Source::Status => {
            if trace.is_some_and(|t| t.0 == Some(basis) && t.2 == Some(Source::Status)) {
                details
                    .upstream_code
                    .as_ref()
                    .map(|s| format!("错误封套状态：{s}"))
                    .unwrap_or_else(|| "错误封套中的状态码".into())
            } else {
                status.map(|s| format!("HTTP {s}")).unwrap_or_default()
            }
        }
        Source::CustomCode | Source::CustomKeyword => unreachable!(),
    };
    (details, Some(source), evidence)
}
