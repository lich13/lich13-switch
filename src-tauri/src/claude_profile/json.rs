//! Top-level field edits preserve every unrelated byte, including CRLF/indent.
use crate::{
    configuration,
    storage::{AppError, Result},
};
use serde_json::Value;
use std::ops::Range;
#[derive(Clone)]
struct Member {
    key: String,
    key_start: usize,
    value: Range<usize>,
}
fn invalid() -> AppError {
    AppError::new("CLAUDE_PROFILE", "Claude 配置结构无效")
}
fn whitespace(text: &str, at: &mut usize) {
    while text
        .as_bytes()
        .get(*at)
        .is_some_and(u8::is_ascii_whitespace)
    {
        *at += 1;
    }
}
fn one(text: &str, at: &mut usize) -> Result<Value> {
    let mut stream = serde_json::Deserializer::from_str(&text[*at..]).into_iter::<Value>();
    let value = stream.next().ok_or_else(invalid)?.map_err(|_| invalid())?;
    *at += stream.byte_offset();
    Ok(value)
}
fn members(text: &str) -> Result<Vec<Member>> {
    configuration::parse_object(text)?;
    let mut at = text.len() - text.trim_start().len() + 1;
    let mut out = Vec::new();
    loop {
        whitespace(text, &mut at);
        if text.as_bytes().get(at) == Some(&b'}') {
            break;
        }
        let key_start = at;
        let key = one(text, &mut at)?.as_str().ok_or_else(invalid)?.to_owned();
        whitespace(text, &mut at);
        if text.as_bytes().get(at) != Some(&b':') {
            return Err(invalid());
        }
        at += 1;
        whitespace(text, &mut at);
        let start = at;
        one(text, &mut at)?;
        out.push(Member {
            key,
            key_start,
            value: start..at,
        });
        whitespace(text, &mut at);
        if text.as_bytes().get(at) == Some(&b',') {
            at += 1;
        } else if text.as_bytes().get(at) != Some(&b'}') {
            return Err(invalid());
        }
    }
    Ok(out)
}
pub fn field(text: &str, key: &str) -> Result<Option<String>> {
    Ok(members(text)?
        .iter()
        .find(|m| m.key == key)
        .map(|m| text[m.value.clone()].to_owned()))
}
pub fn set(text: &str, key: &str, value: Option<&str>) -> Result<String> {
    let fields = members(text)?;
    if let Some(value) = value {
        let _: Value = serde_json::from_str(value).map_err(|_| invalid())?;
    }
    let mut out = text.to_owned();
    if let Some(i) = fields.iter().position(|m| m.key == key) {
        let m = &fields[i];
        if let Some(value) = value {
            out.replace_range(m.value.clone(), value);
        } else {
            let range = if let Some(next) = fields.get(i + 1) {
                m.key_start..next.key_start
            } else if i > 0 {
                fields[i - 1].value.end..m.value.end
            } else {
                m.key_start..m.value.end
            };
            out.replace_range(range, "");
        }
    } else if let Some(value) = value {
        let at = fields
            .last()
            .map(|m| m.value.end)
            .unwrap_or(text.len() - text.trim_start().len() + 1);
        let sep = if fields.is_empty() { "" } else { "," };
        let newline = if text.contains("\r\n") {
            "\r\n"
        } else if text.contains('\n') {
            "\n"
        } else {
            " "
        };
        let insertion = format!(
            "{sep}{newline}  {}: {value}",
            serde_json::to_string(key).map_err(|_| invalid())?
        );
        out.insert_str(at, &insertion);
    }
    configuration::parse_object(&out)?;
    Ok(out)
}
