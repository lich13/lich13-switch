//! Inspect bounded error envelopes only; never scan successful output for keywords.
use crate::events::{Details, ErrorBasis};
use serde_json::Value;
const LIMIT: usize = 128 * 1024;
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
    if message.len() > LIMIT || capacity_message(message) {
        return false;
    }
    let s = message.trim().to_lowercase();
    [
        "model_not_found",
        "unsupported_model",
        "model_not_supported",
        "model_not_available",
        "model_unavailable",
        "model_access_denied",
        "model_permission_denied",
        "invalid_model",
        "no_available_model",
        "model_not_exist",
        "model_not_exists",
    ]
    .contains(&s.as_str())
        || (s.contains("model")
            && [
                "does not exist",
                "not found",
                "not supported",
                "unsupported",
                "do not have access",
                "does not have access",
                "access denied",
                "don't have access",
                "no access to",
                "not allowed",
                "not permitted",
                "not available for",
                "not available to",
                "is not available",
                "no available channel",
                "no available provider",
                "no endpoints found",
                "no channel",
                "not enabled",
                "not authorized",
                "not entitled",
            ]
            .iter()
            .any(|p| s.contains(p)))
        || (s.contains("模型")
            && [
                "不存在",
                "未找到",
                "不支持",
                "无权",
                "没有权限",
                "未授权",
                "无权限",
                "无可用渠道",
                "没有可用渠道",
                "不可用渠道",
                "无可用通道",
                "无可用上游",
                "暂不可用",
                "未开通",
                "未启用",
                "不允许",
            ]
            .iter()
            .any(|p| s.contains(p)))
}
// Explicit wrappers only: never recurse into arbitrary user content.
fn envelope(v: &Value, http_error: bool, depth: usize) -> Option<&Value> {
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
        return envelope(error, true, depth + 1);
    }
    for key in ["response", "data", "result"] {
        if let Some(inner) = v.get(key).filter(|x| x.is_object()) {
            if let Some(error) = envelope(inner, http_error, depth + 1) {
                return Some(error);
            }
        }
    }
    let kind = v.get("type").and_then(Value::as_str).unwrap_or("");
    let explicit = kind == "error"
        || kind == "response.failed"
        || kind.ends_with("_error")
        || v.get("success") == Some(&Value::Bool(false))
        || v.get("status").is_some_and(|s| s == "failed");
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
    error
        .as_str()
        .into_iter()
        .chain(
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
        .filter(|s| s.len() <= LIMIT)
}

// Read only named error fields, before free-form message redaction removes quotes.
fn text_field<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    if text.len() > LIMIT {
        return None;
    }
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
pub(super) fn reason_from_body(body: &[u8]) -> Option<String> {
    if body.len() > LIMIT {
        return None;
    }
    match serde_json::from_slice::<Value>(body) {
        Ok(value) => envelope(&value, true, 0).and_then(known_reason),
        Err(_) => std::str::from_utf8(body).ok().and_then(|text| {
            let text = text.trim_start();
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
                    .find_map(|field| {
                        text_field(text, field).and_then(crate::events::safe_upstream_reason)
                    })
                })
                .flatten()
        }),
    }
}
pub(super) fn is_permanent_reason(reason: Option<&str>) -> bool {
    reason.is_some_and(|value| {
        [
            "DAILY_LIMIT_EXCEEDED",
            "WEEKLY_LIMIT_EXCEEDED",
            "MONTHLY_LIMIT_EXCEEDED",
            "USER_PLATFORM_DAILY_QUOTA_EXHAUSTED",
            "USER_PLATFORM_WEEKLY_QUOTA_EXHAUSTED",
            "USER_PLATFORM_MONTHLY_QUOTA_EXHAUSTED",
        ]
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
    envelope(value, false, 0).is_some_and(is_model)
}
pub fn temporary_capacity_event(value: &Value) -> bool {
    envelope(value, false, 0).is_some_and(|error| {
        !is_permanent_reason(known_reason(error).as_deref())
            && !strings(error).any(global_fault)
            && strings(error).any(capacity_message)
    })
}
/// Explicit retry/reset fields from a bounded error envelope. Free-form text
/// and successful response content are never interpreted as timing evidence.
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
        .map(|at| at.saturating_sub(now))
        .max();
    relative.max(absolute.map(|seconds| std::time::Duration::from_secs(seconds.min(86_400))))
}
pub(super) fn retry_after_http(bytes: &[u8]) -> Option<std::time::Duration> {
    if bytes.len() > LIMIT {
        return None;
    }
    let value = serde_json::from_slice::<Value>(bytes).ok()?;
    retry_after_fields(envelope(&value, true, 0)?)
}
pub fn model_http(status: u16, bytes: &[u8]) -> bool {
    if status < 400 || status == 429 || bytes.len() > LIMIT {
        return false;
    }
    match serde_json::from_slice::<Value>(bytes) {
        Ok(v) => envelope(&v, true, 0).is_some_and(is_model),
        Err(_) => std::str::from_utf8(bytes).is_ok_and(model_message),
    }
}
pub fn details_value(value: &Value, http_error: bool) -> Details {
    let Some(error) = envelope(value, http_error, 0) else {
        return Details::default();
    };
    Details {
        error_basis: error_basis(error),
        upstream_reason: known_reason(error),
        health_scope: (is_permanent_reason(known_reason(error).as_deref())
            || strings(error).any(global_fault))
        .then_some(crate::events::HealthScope::Provider),
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
pub fn details_http(bytes: &[u8]) -> Details {
    if bytes.len() > LIMIT {
        return Details::default();
    }
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
pub(super) fn permanent_rejection(body: &[u8]) -> bool {
    if body.len() > LIMIT {
        return false;
    }
    match serde_json::from_slice::<Value>(body) {
        Ok(value) => envelope(&value, true, 0).is_some_and(|e| {
            is_permanent_reason(known_reason(e).as_deref()) || strings(e).any(hard_message)
        }),
        Err(_) => std::str::from_utf8(body).is_ok_and(|text| {
            is_permanent_reason(reason_from_body(body).as_deref()) || hard_message(text)
        }),
    }
}
fn hard_message(text: &str) -> bool {
    if text.len() > LIMIT || crate::events::safe_message(text).is_none() {
        return false;
    }
    if quota_message(text) {
        return true;
    }
    let text = text.to_ascii_lowercase();
    [
        "insufficient_quota",
        "insufficient_balance",
        "insufficient_account_balance",
        "account_balance_insufficient",
        "api_key_quota_exhausted",
        "insufficient_user_quota",
        "balance_exhausted",
        "balance_not_enough",
        "credits_exhausted",
        "credit_limit_exceeded",
        "insufficient account balance",
        "insufficient credit balance",
        "account balance is insufficient",
        "credit balance exhausted",
        "balance has been exhausted",
        "run out of credits",
        "out of credit",
        "api key quota exhausted",
        "api key quota exceeded",
        "credit_balance_too_low",
        "billing_hard_limit_reached",
        "account_disabled",
        "account_deactivated",
        "insufficient credits",
        "insufficient balance",
        "credit balance is too low",
        "account has been disabled",
        "余额不足",
        "余额已耗尽",
        "余额耗尽",
        "余额用尽",
        "余额已用完",
        "账户欠费",
        "账号欠费",
        "账户额度已耗尽",
        "账户额度已用完",
        "api key 额度已用完",
        "api key额度已用完",
        "令牌额度已用完",
        "令牌额度不足",
        "账号已禁用",
        "账户已禁用",
    ]
    .iter()
    .any(|marker| text.contains(marker))
}

fn quota_message(text: &str) -> bool {
    if crate::events::safe_message(text).is_none() {
        return false;
    }
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
    if [
        "documentation:",
        "example:",
        "describes the",
        "not exceeded",
        "not exhausted",
        "not reached",
        "not yet",
        "未达到",
        "未耗尽",
        "尚未",
        "没有超",
        "未超",
        "并未",
    ]
    .iter()
    .any(|s| text.contains(s))
    {
        return false;
    }
    [
        "daily usage limit exceeded",
        "weekly usage limit exceeded",
        "monthly usage limit exceeded",
        "daily quota exhausted",
        "weekly quota exhausted",
        "monthly quota exhausted",
        "daily quota exceeded",
        "weekly quota exceeded",
        "monthly quota exceeded",
        "日额度已耗尽",
        "日额度已用尽",
        "日额度已用完",
        "每日额度已耗尽",
        "每日用量已达上限",
        "日用量已达上限",
        "每周额度已耗尽",
        "每月额度已耗尽",
        "周额度已耗尽",
        "周额度已用尽",
        "每周用量已达上限",
        "月额度已耗尽",
        "月额度已用尽",
        "每月用量已达上限",
    ]
    .iter()
    .any(|s| text.contains(s))
}
fn rpm_message(text: &str) -> bool {
    if crate::events::safe_message(text).is_none() {
        return false;
    }
    let s = text.to_ascii_lowercase();
    if [
        "not an rpm",
        "not a requests",
        "not exceeded",
        "not reached",
        "not been reached",
        "not yet",
        "documentation:",
        "example:",
        "未达到",
        "尚未",
        "没有超",
        "未超",
    ]
    .iter()
    .any(|negative| s.contains(negative))
    {
        return false;
    }
    [
        "group_rpm_exceeded",
        "user_rpm_exceeded",
        "requests-per-minute limit exceeded",
        "requests per minute limit exceeded",
        "requests per minute limit reached",
        "rpm limit exceeded",
        "rpm_limit_exceeded",
        "每分钟请求数超限",
        "每分钟请求次数已达上限",
    ]
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
fn error_basis(error: &Value) -> Option<ErrorBasis> {
    let reason = known_reason(error);
    if is_permanent_reason(reason.as_deref()) || strings(error).any(quota_message) {
        return Some(ErrorBasis::QuotaExhausted);
    }
    // Preserve existing balance/key denials without classifying authentication as quota.
    if strings(error).any(|s| {
        hard_message(s)
            && ![
                "account_disabled",
                "account_deactivated",
                "account has been disabled",
                "账号已禁用",
                "账户已禁用",
            ]
            .iter()
            .any(|m| s.to_lowercase().contains(m))
    }) {
        return Some(ErrorBasis::QuotaExhausted);
    }
    if strings(error).any(rpm_message) {
        return Some(ErrorBasis::Rpm);
    }
    if reason
        .as_deref()
        .is_some_and(|s| s.eq_ignore_ascii_case("USAGE_LIMIT_EXCEEDED"))
    {
        return Some(ErrorBasis::UsageLimit);
    }
    strings(error)
        .any(capacity_message)
        .then_some(ErrorBasis::Capacity)
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
