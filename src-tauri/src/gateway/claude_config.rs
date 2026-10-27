//! Locate JSON values with serde's byte offsets; never reserialize unrelated settings.
use super::takeover::Pair;
use crate::errors::code;
use crate::storage::{AppError, Result};
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::ops::Range;

const KEYS: [&str; 2] = ["ANTHROPIC_BASE_URL", "ANTHROPIC_AUTH_TOKEN"];
struct Member {
    key: String,
    span: Range<usize>,
}
fn invalid() -> AppError {
    AppError::new(
        code::CLAUDE_CONFIG,
        "Claude settings.json 无效，请检查 JSON 和 env 两字段",
    )
}
fn skip(text: &str, at: &mut usize) {
    while text
        .as_bytes()
        .get(*at)
        .is_some_and(u8::is_ascii_whitespace)
    {
        *at += 1;
    }
}
fn one<T: DeserializeOwned>(text: &str, at: &mut usize) -> Result<T> {
    let mut stream = serde_json::Deserializer::from_str(&text[*at..]).into_iter::<T>();
    let value = stream.next().ok_or_else(invalid)?.map_err(|_| invalid())?;
    *at += stream.byte_offset();
    Ok(value)
}
fn object(text: &str, start: usize) -> Result<Vec<Member>> {
    if text.as_bytes().get(start) != Some(&b'{') {
        return Err(invalid());
    }
    let mut at = start + 1;
    let mut members = Vec::new();
    loop {
        skip(text, &mut at);
        if text.as_bytes().get(at) == Some(&b'}') {
            return Ok(members);
        }
        let key: String = one(text, &mut at)?;
        skip(text, &mut at);
        if text.as_bytes().get(at) != Some(&b':') {
            return Err(invalid());
        }
        at += 1;
        skip(text, &mut at);
        let begin = at;
        let _: Value = one(text, &mut at)?;
        members.push(Member {
            key,
            span: begin..at,
        });
        skip(text, &mut at);
        match text.as_bytes().get(at) {
            Some(b',') => at += 1,
            Some(b'}') => return Ok(members),
            _ => return Err(invalid()),
        }
    }
}
fn unique<'a>(members: &'a [Member], key: &str) -> Result<Option<&'a Member>> {
    let mut found = members.iter().filter(|m| m.key == key);
    let value = found.next();
    if found.next().is_some() {
        return Err(AppError::new(
            code::CLAUDE_CONFIG,
            "Claude 配置存在重复的受管字段",
        ));
    }
    Ok(value)
}
type ObjectLocations = (Vec<Member>, Option<Range<usize>>, Vec<Member>);
fn locations(text: &str) -> Result<ObjectLocations> {
    let _: Value = serde_json::from_str(text).map_err(|_| invalid())?;
    let start = text.len() - text.trim_start().len();
    let root = object(text, start)?;
    let env = unique(&root, "env")?.map(|m| m.span.clone());
    let fields = env
        .as_ref()
        .map(|r| object(text, r.start))
        .transpose()?
        .unwrap_or_default();
    for key in KEYS {
        if let Some(m) = unique(&fields, key)? {
            let _: String = serde_json::from_str(&text[m.span.clone()]).map_err(|_| invalid())?;
        }
    }
    Ok((root, env, fields))
}
pub fn pair(text: &str) -> Result<Pair> {
    let (_, _, fields) = locations(text)?;
    let value = |key| {
        unique(&fields, key)?
            .map(|m| serde_json::from_str(&text[m.span.clone()]).map_err(|_| invalid()))
            .transpose()
    };
    Ok(Pair {
        base_url: value(KEYS[0])?,
        token: value(KEYS[1])?,
    })
}
fn insertion(
    text: &str,
    start: usize,
    fields: &[Member],
    values: Vec<(String, String)>,
) -> (Range<usize>, String) {
    let at = fields.last().map(|m| m.span.end).unwrap_or(start + 1);
    let multiline = text.contains('\n');
    let newline = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let indent = if let Some(first) = fields.first() {
        let line = text[..first.span.start].rsplit('\n').next().unwrap_or("");
        line.chars()
            .take_while(|c| matches!(c, ' ' | '\t'))
            .collect::<String>()
    } else {
        let line = text[..start].rsplit('\n').next().unwrap_or("");
        format!(
            "{}  ",
            line.chars()
                .take_while(|c| matches!(c, ' ' | '\t'))
                .collect::<String>()
        )
    };
    let separator = if multiline {
        format!("{newline}{indent}")
    } else {
        " ".into()
    };
    let mut out = if fields.is_empty() {
        String::new()
    } else {
        ",".into()
    };
    out.push_str(&separator);
    out.push_str(
        &values
            .into_iter()
            .map(|(k, v)| format!("{}: {v}", serde_json::to_string(&k).unwrap()))
            .collect::<Vec<_>>()
            .join(&format!(",{separator}")),
    );
    (at..at, out)
}
pub fn patch(text: &str, target: &Pair) -> Result<String> {
    let (root, env, fields) = locations(text)?;
    let values = [target.base_url.as_ref(), target.token.as_ref()];
    if values.iter().any(|v| v.is_none()) {
        return Err(invalid());
    }
    let mut edits = Vec::new();
    let mut missing = Vec::new();
    for (key, value) in KEYS.into_iter().zip(values) {
        let encoded = serde_json::to_string(value.unwrap()).unwrap();
        if let Some(m) = unique(&fields, key)? {
            edits.push((m.span.clone(), encoded));
        } else {
            missing.push((key.to_owned(), encoded));
        }
    }
    if !missing.is_empty() {
        if let Some(env) = env {
            edits.push(insertion(text, env.start, &fields, missing));
        } else {
            let value = format!(
                "{{{}}}",
                missing
                    .into_iter()
                    .map(|(k, v)| format!("{}: {v}", serde_json::to_string(&k).unwrap()))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            edits.push(insertion(
                text,
                text.len() - text.trim_start().len(),
                &root,
                vec![("env".into(), value)],
            ));
        }
    }
    edits.sort_by_key(|(range, _)| std::cmp::Reverse(range.start));
    let mut out = text.to_owned();
    for (range, value) in edits {
        out.replace_range(range, &value);
    }
    if pair(&out)? != *target {
        return Err(invalid());
    }
    Ok(out)
}

// Report only identifiable overrides, never their values.
pub fn warning(bytes: &[u8]) -> Option<String> {
    let value: Value = serde_json::from_slice(bytes).ok()?;
    let mut keys = Vec::new();
    for key in [
        "ANTHROPIC_API_KEY",
        "CLAUDE_CODE_USE_BEDROCK",
        "CLAUDE_CODE_USE_VERTEX",
        "CLAUDE_CODE_USE_FOUNDRY",
    ] {
        if value
            .get("env")
            .and_then(|v| v.get(key))
            .and_then(Value::as_str)
            .is_some_and(|v| !v.is_empty() && v != "0")
        {
            keys.push(key);
        }
    }
    if value.get("apiKeyHelper").is_some() {
        keys.push("apiKeyHelper");
    }
    for key in KEYS {
        if std::env::var(key).is_ok_and(|v| !v.is_empty()) {
            keys.push(key);
        }
    }
    (!keys.is_empty()).then(|| format!("检测到其他认证来源：{}", keys.join("、")))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_two_value_replacement_and_missing_containers() {
        let old = "{\r\n  \"env\": {\"ANTHROPIC_BASE_URL\":\"old\", \"keep\": [1,{\"n\":true}], \"ANTHROPIC_AUTH_TOKEN\": \"key\"},\r\n  \"permissions\": {\"allow\": []}\r\n}\r\n";
        let target = Pair::new("https://example.invalid/site", "fixture-new");
        assert_eq!(
            patch(old, &target).unwrap(),
            old.replace("\"old\"", "\"https://example.invalid/site\"")
                .replace("\"key\"", "\"fixture-new\"")
        );
        for text in [
            "{}",
            "{\"model\":\"untouched\"}",
            "{\"env\": {}}",
            "{\"env\":{\"ANTHROPIC_BASE_URL\":\"old\"}}",
            "{\n  \"env\": {\n    \"OTHER\": \"keep\"\n  }\n}\n",
        ] {
            let out = patch(text, &target).unwrap();
            assert!(pair(&out).unwrap() == target);
        }
    }
    #[test]
    fn rejects_invalid_duplicate_or_non_string_controlled_fields() {
        for text in [
            "[]",
            "{",
            "{\"env\":null}",
            "{\"env\":{},\"env\":{}}",
            "{\"env\":{\"ANTHROPIC_AUTH_TOKEN\":0}}",
            "{\"env\":{\"ANTHROPIC_AUTH_TOKEN\":\"a\",\"ANTHROPIC_AUTH_TOKEN\":\"b\"}}",
        ] {
            assert!(patch(text, &Pair::new("a", "b")).is_err(), "{text}");
        }
    }
}
