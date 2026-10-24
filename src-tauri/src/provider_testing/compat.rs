use serde_json::Value;
pub fn sha256(raw: impl AsRef<[u8]>) -> String {
    crate::storage::digest(raw.as_ref())
}
pub fn string(value: &Value) -> &str {
    value.as_str().unwrap_or("")
}
pub fn integer(value: &Value) -> Option<i64> {
    value.as_i64().or_else(|| value.as_str()?.parse().ok())
}
pub fn text(value: &Value) -> String {
    match value {
        Value::Null => "None".into(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::String(s) => s.clone(),
        _ => value.to_string(),
    }
}
pub fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(v) => *v,
        Value::Number(v) => v.as_f64().is_some_and(|n| n != 0.0),
        Value::String(v) => !v.is_empty(),
        Value::Array(v) => !v.is_empty(),
        Value::Object(v) => !v.is_empty(),
    }
}
pub fn clean(value: &Value, limit: usize) -> String {
    text(value)
        .chars()
        .filter(|c| !c.is_control())
        .take(limit)
        .collect()
}
