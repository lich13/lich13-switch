//! Only explicit error envelopes / bounded HTTP error bodies are inspected.
use serde_json::Value;
pub fn model_message(message: &str) -> bool {
    let s = message.to_lowercase();
    if [
        "at capacity",
        "too many requests",
        "rate limit",
        "overload",
        "容量",
        "限流",
        "繁忙",
    ]
    .iter()
    .any(|p| s.contains(p))
    {
        return false;
    }
    [
        "model_not_found",
        "unsupported_model",
        "model_not_supported",
        "model_not_available",
        "model_access_denied",
        "model_permission_denied",
        "invalid_model",
    ]
    .iter()
    .any(|p| s == *p)
        || (s.contains("model")
            && [
                "does not exist",
                "not found",
                "not supported",
                "unsupported model",
                "do not have access",
                "does not have access",
                "access denied",
                "don't have access",
                "no access to",
                "not allowed to access",
                "not available for your",
                "not available to your",
                "no available channel for model",
                "no channel supports",
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
            ]
            .iter()
            .any(|p| s.contains(p)))
}
pub fn model_error(value: &Value) -> bool {
    let envelope = value.get("response").unwrap_or(value);
    let Some(error) = envelope.get("error").filter(|e| !e.is_null()).or_else(|| {
        (envelope.get("type").and_then(Value::as_str) == Some("error")).then_some(envelope)
    }) else {
        return false;
    };
    if ["code", "type"]
        .iter()
        .filter_map(|k| error.get(k).and_then(Value::as_str))
        .any(|code| {
            matches!(
                code,
                "rate_limit_exceeded"
                    | "rate_limit_error"
                    | "overloaded_error"
                    | "model_capacity_exceeded"
                    | "server_is_overloaded"
                    | "usage_limit_reached"
            )
        })
    {
        return false;
    }
    if let Some(s) = error.as_str() {
        return model_message(s);
    }
    if error.get("type").and_then(Value::as_str) == Some("not_found_error")
        && error
            .get("message")
            .and_then(Value::as_str)
            .is_some_and(|m| m.trim_start().to_ascii_lowercase().starts_with("model:"))
    {
        return true;
    }
    ["code", "type", "message"]
        .iter()
        .filter_map(|k| error.get(k).and_then(Value::as_str))
        .any(|s| s.len() <= 128 * 1024 && model_message(s))
}
pub fn model_http(status: u16, bytes: &[u8]) -> bool {
    if status < 400 || status == 429 {
        return false;
    }
    let limited = &bytes[..bytes.len().min(128 * 1024)];
    match serde_json::from_slice(limited) {
        Ok(v) => model_error(&v),
        Err(_) => std::str::from_utf8(limited).is_ok_and(model_message),
    }
}
