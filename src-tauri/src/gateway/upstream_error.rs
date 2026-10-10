//! Inspect bounded error envelopes only; never scan successful output for keywords.
use crate::events::Details;
use serde_json::Value;
const LIMIT: usize = 128 * 1024;
pub fn capacity_message(message: &str) -> bool {
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
    if depth > 5 {
        return None;
    }
    if let Some(error) = v
        .get("error")
        .filter(|e| !e.is_null() && **e != Value::Bool(false))
    {
        return envelope(error, true, depth + 1).or(Some(error));
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
            || ["code", "type", "message", "detail"]
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
        ["code", "type", "message", "detail"]
            .into_iter()
            .filter_map(move |k| error.get(k).and_then(Value::as_str)),
    )
}
fn is_model(error: &Value) -> bool {
    if strings(error).any(capacity_message) {
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
        !strings(error).any(|s| permanent_rejection(s.as_bytes()))
            && strings(error).any(capacity_message)
    })
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
        upstream_code: error.get("code").and_then(|v| match v {
            Value::String(s) => Some(s.clone()),
            Value::Number(n) if n.is_i64() || n.is_u64() => Some(n.to_string()),
            _ => None,
        }),
        upstream_type: error.get("type").and_then(Value::as_str).map(str::to_owned),
        parameter: error
            .get("param")
            .and_then(Value::as_str)
            .map(str::to_owned),
        message: error
            .as_str()
            .or_else(|| error.get("message").and_then(Value::as_str))
            .or_else(|| error.get("detail").and_then(Value::as_str))
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
        Err(_) => Details {
            message: std::str::from_utf8(bytes)
                .ok()
                .and_then(crate::events::safe_message),
            ..Default::default()
        },
    }
}
/// Explicit billing/account denials cannot recover by retrying every minute.
/// Status 429 alone is deliberately insufficient to classify a permanent denial.
pub(super) fn permanent_rejection(body: &[u8]) -> bool {
    let text = String::from_utf8_lossy(&body[..body.len().min(128 * 1024)]).to_ascii_lowercase();
    [
        "insufficient_quota",
        "insufficient_balance",
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
        "账号已禁用",
        "账户已禁用",
    ]
    .iter()
    .any(|marker| text.contains(marker))
}
