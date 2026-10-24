use super::{types::TestError, Target};
use serde_json::Value;
use std::{error::Error, sync::LazyLock};

pub(super) const ERROR_LIMIT: usize = 64 * 1024 * 1024;
pub(super) const CLIENT_RESTRICTED: &str = "上游限制客户端身份";

pub(super) fn is_client_restricted(text: &str) -> bool {
    text.to_ascii_lowercase()
        .contains("this account only allows codex official clients")
}

pub(super) fn body(status: Option<u16>, text: &str, truncated: bool) -> TestError {
    let body = serde_json::from_str::<Value>(text)
        .ok()
        .and_then(|value| serde_json::to_string_pretty(&value).ok())
        .unwrap_or_else(|| text.to_owned());
    TestError {
        http_status: status,
        message: if is_client_restricted(text) {
            status.map_or_else(
                || CLIENT_RESTRICTED.into(),
                |status| format!("{CLIENT_RESTRICTED}（HTTP {status}）"),
            )
        } else {
            status.map_or_else(
                || "上游返回测试错误".into(),
                |status| format!("上游返回 HTTP {status}"),
            )
        },
        body,
        truncated,
    }
}
pub(super) fn network(error: reqwest::Error) -> TestError {
    let error = error.without_url();
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        message.push_str(": ");
        message.push_str(&cause.to_string());
        source = cause.source();
    }
    TestError {
        message,
        ..Default::default()
    }
}
pub(super) fn redact(mut detail: TestError, target: &Target) -> TestError {
    fn fields(value: &mut Value) {
        match value {
            Value::Object(object) => {
                for (key, value) in object {
                    if matches!(
                        key.to_ascii_lowercase().replace('-', "_").as_str(),
                        "authorization"
                            | "api_key"
                            | "apikey"
                            | "x_api_key"
                            | "token"
                            | "access_token"
                            | "refresh_token"
                            | "cookie"
                            | "set_cookie"
                            | "password"
                            | "secret"
                            | "x_codex_installation_id"
                    ) {
                        *value = Value::String("[已隐藏]".into());
                    } else {
                        fields(value);
                    }
                }
            }
            Value::Array(items) => items.iter_mut().for_each(fields),
            _ => (),
        }
    }
    static BEARER: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"(?i)Bearer\s+[A-Za-z0-9._~+/-]+=*").unwrap());
    detail.message = BEARER
        .replace_all(&target.redact(&detail.message), "Bearer [已隐藏]")
        .into_owned();
    detail.body = target.redact(&detail.body);
    if let Ok(mut value) = serde_json::from_str::<Value>(&detail.body) {
        fields(&mut value);
        detail.body = serde_json::to_string_pretty(&value).unwrap_or(detail.body);
    }
    detail.body = BEARER
        .replace_all(&detail.body, "Bearer [已隐藏]")
        .into_owned();
    detail
}
pub(super) async fn response(mut response: reqwest::Response) -> TestError {
    let status = response.status().as_u16();
    let mut bytes = vec![];
    let mut truncated = false;
    let mut read_error = None;
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                let room = ERROR_LIMIT - bytes.len();
                bytes.extend_from_slice(&chunk[..chunk.len().min(room)]);
                if chunk.len() > room {
                    truncated = true;
                    break;
                }
            }
            Ok(None) => break,
            Err(error) => {
                read_error = Some(network(error).message);
                break;
            }
        }
    }
    let mut detail = body(Some(status), &String::from_utf8_lossy(&bytes), truncated);
    if let Some(error) = read_error {
        detail
            .message
            .push_str(&format!("；读取错误响应中断：{error}"));
        detail.truncated = true;
    }
    detail
}
