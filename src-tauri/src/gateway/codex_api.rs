//! File-backed Codex API authentication. No provider/model/auth-store rewrites.
use super::takeover::Pair;
use crate::storage::{self, AppError, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
};
use toml_edit::{Document, Item};

const WRITE_FILE: &str = "gateway-api-write.json";
const LIMIT: u64 = 2 * 1024 * 1024;

#[derive(Clone, Default, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "camelCase")]
pub enum Connection {
    #[default]
    Bearer,
    ApiKey {
        provider: String,
    },
}

fn invalid(message: &str) -> AppError {
    AppError::new("CODEX_AUTH", message)
}
fn document(text: &str) -> Result<Document<&str>> {
    Document::parse(text).map_err(|_| invalid("Codex 配置不是有效 TOML"))
}
fn selected(doc: &Document<&str>) -> Option<String> {
    doc.get("profile")
        .and_then(Item::as_str)
        .and_then(|profile| {
            doc.get("profiles")?
                .get(profile)?
                .get("model_provider")?
                .as_str()
        })
        .or_else(|| doc.get("model_provider").and_then(Item::as_str))
        .map(str::to_owned)
}
fn effective<'a>(doc: &'a Document<&str>, key: &str) -> Option<&'a Item> {
    doc.get("profile")
        .and_then(Item::as_str)
        .and_then(|profile| doc.get("profiles")?.get(profile)?.get(key))
        .or_else(|| doc.get(key))
}
fn provider<'a>(doc: &'a Document<&str>, id: &str) -> Result<&'a Item> {
    if selected(doc).as_deref() != Some(id) {
        return Err(invalid("当前 model_provider 已变化，请重新识别连接方式"));
    }
    doc.get("model_providers")
        .and_then(|p| p.get(id))
        .filter(|p| p.is_table() || p.is_inline_table())
        .ok_or_else(|| invalid("请保留当前供应商的 model_providers 配置"))
}
fn compatible(doc: &Document<&str>, id: &str) -> Result<()> {
    let p = provider(doc, id)?;
    if effective(doc, "cli_auth_credentials_store").is_some_and(|v| v.as_str() != Some("file")) {
        return Err(invalid("当前凭据存储不以 auth.json 为准，连接方式未更改"));
    }
    if effective(doc, "forced_login_method").and_then(Item::as_str) == Some("chatgpt") {
        return Err(invalid("当前配置限定 ChatGPT 登录，连接方式未更改"));
    }
    if p.get("requires_openai_auth").and_then(Item::as_bool) != Some(true) {
        return Err(invalid(
            "当前供应商未启用 requires_openai_auth，连接方式未更改",
        ));
    }
    if ["experimental_bearer_token", "auth", "auth_command", "exec"]
        .iter()
        .any(|k| {
            p.get(k)
                .is_some_and(|v| v.as_str().is_none_or(|s| !s.is_empty()))
        })
        || p.get("http_headers")
            .and_then(Item::as_table_like)
            .is_some_and(|t| {
                t.iter().any(|(k, _)| {
                    matches!(
                        k.to_ascii_lowercase().as_str(),
                        "authorization" | "x-api-key"
                    )
                })
            })
        || p.get("env_http_headers")
            .and_then(Item::as_table_like)
            .is_some_and(|t| {
                t.iter().any(|(k, _)| {
                    matches!(
                        k.to_ascii_lowercase().as_str(),
                        "authorization" | "x-api-key"
                    )
                })
            })
    {
        return Err(invalid("当前供应商有独立认证覆盖，请先在配置中处理"));
    }
    Ok(())
}
fn text(home: &Path) -> Result<String> {
    let raw = storage::read_bounded(&home.join("config.toml"), LIMIT)?
        .ok_or_else(|| invalid("未找到 Codex config.toml"))?;
    String::from_utf8(raw).map_err(|_| invalid("Codex 配置必须使用 UTF-8"))
}
fn auth(raw: Option<&[u8]>) -> Result<Value> {
    let text = std::str::from_utf8(raw.unwrap_or(b"{}"))
        .map_err(|_| invalid("auth.json 必须使用 UTF-8"))?;
    crate::configuration::parse_object(text)
        .map_err(|_| invalid("auth.json 无效或包含重复字段，未覆盖"))
}
fn api_key(value: &Value) -> Result<Option<String>> {
    let mode = value.get("auth_mode").filter(|v| !v.is_null());
    let api = mode.map_or_else(
        || {
            ![
                "personal_access_token",
                "bedrock_api_key",
                "bedrock_access_keys",
            ]
            .iter()
            .any(|k| value.get(k).is_some_and(|v| !v.is_null()))
        },
        |m| m.as_str() == Some("apikey"),
    );
    if !api {
        return Ok(None);
    }
    match value.get("OPENAI_API_KEY") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok((!s.trim().is_empty()).then(|| s.clone())),
        _ => Err(invalid("auth.json 的 OPENAI_API_KEY 必须是字符串")),
    }
}
pub fn binding(home: &Path) -> Result<Connection> {
    let text = text(home)?;
    let doc = document(&text)?;
    let id = selected(&doc).ok_or_else(|| invalid("未配置自定义供应商，已保留官方连接"))?;
    compatible(&doc, &id)?;
    Ok(Connection::ApiKey { provider: id })
}
/// Detection only reads files. An upgrade never calls this for an existing store.
pub fn detect(home: &Path) -> Result<Connection> {
    let config = text(home)?;
    let doc = document(&config)?;
    let id = selected(&doc).ok_or_else(|| invalid("当前使用官方连接，未自动导入"))?;
    let p = provider(&doc, &id)?;
    if id == "custom"
        && p.get("experimental_bearer_token")
            .and_then(Item::as_str)
            .is_some_and(|s| !s.is_empty())
    {
        return Ok(Connection::Bearer);
    }
    let connection = binding(home)?;
    let raw = storage::read_bounded(&home.join("auth.json"), LIMIT)?;
    if api_key(&auth(raw.as_deref())?)?.is_none() {
        return Err(invalid("未识别到普通 API 登录，现有认证已保留"));
    }
    Ok(connection)
}
pub fn base(text: &str, id: &str) -> Result<Option<String>> {
    let doc = document(text)?;
    compatible(&doc, id)?;
    match provider(&doc, id)?.get("base_url") {
        None => Ok(None),
        Some(v) => v
            .as_str()
            .map(|s| Some(s.to_owned()))
            .ok_or_else(|| invalid("base_url 必须是字符串")),
    }
}
pub fn read(home: &Path, id: &str) -> Result<(String, Pair)> {
    let config = text(home)?;
    let raw = storage::read_bounded(&home.join("auth.json"), LIMIT)?;
    let pair = Pair {
        base_url: base(&config, id)?,
        token: api_key(&auth(raw.as_deref())?)?,
    };
    let revision = storage::digest(
        &serde_json::to_vec(&(
            storage::digest(config.as_bytes()),
            storage::revision(raw.as_deref()),
        ))
        .unwrap(),
    );
    Ok((revision, pair))
}
pub fn websocket(home: &Path, id: &str) -> Result<Option<bool>> {
    let config = text(home)?;
    let doc = document(&config)?;
    match provider(&doc, id)?.get("supports_websockets") {
        None => Ok(None),
        Some(v) => v
            .as_bool()
            .map(Some)
            .ok_or_else(|| invalid("supports_websockets 必须是布尔值")),
    }
}
fn patch_base(config: &str, id: &str, value: &str) -> Result<String> {
    let doc = document(config)?;
    compatible(&doc, id)?;
    let p = provider(&doc, id)?;
    let mut output = config.to_owned();
    if let Some(item) = p.get("base_url") {
        if item.as_str() == Some(value) {
            return Ok(output);
        }
        let span = item
            .as_value()
            .and_then(|v| v.span())
            .ok_or_else(|| invalid("无法定位 base_url"))?;
        output.replace_range(span, &toml_edit::Value::from(value).to_string());
    } else {
        let parent = doc.get("model_providers").unwrap();
        let encoded_id = toml_edit::Key::new(id).to_string();
        let (container, prefix, root) = if p.as_table().is_some_and(|t| t.is_implicit())
            || p.as_inline_table().is_some_and(|t| t.is_dotted())
        {
            if parent.as_table().is_some_and(|t| t.is_implicit()) {
                (
                    doc.as_item(),
                    format!("model_providers.{encoded_id}."),
                    true,
                )
            } else {
                (parent, format!("{encoded_id}."), false)
            }
        } else {
            (p, String::new(), false)
        };
        let assignment = format!("{prefix}base_url = {}", toml_edit::Value::from(value));
        if let Some(table) = container.as_inline_table() {
            let span = table
                .span()
                .ok_or_else(|| invalid("无法定位供应商内联表"))?;
            output.insert_str(
                span.end - 1,
                &format!("{}{assignment}", if table.is_empty() { "" } else { ", " }),
            );
        } else {
            let newline = if config.contains("\r\n") {
                "\r\n"
            } else {
                "\n"
            };
            let end = if root {
                0
            } else {
                let span = container
                    .as_table()
                    .and_then(|t| t.span())
                    .ok_or_else(|| invalid("无法定位供应商表"))?;
                config[span.end..]
                    .find('\n')
                    .map(|i| span.end + i + 1)
                    .unwrap_or(config.len())
            };
            let lead = if end > 0 && !config[..end].ends_with('\n') {
                newline
            } else {
                ""
            };
            output.insert_str(end, &format!("{lead}{assignment}{newline}"));
        }
    }
    if base(&output, id)?.as_deref() != Some(value) {
        return Err(invalid("base_url 回读校验失败"));
    }
    Ok(output)
}

#[derive(Serialize, Deserialize)]
struct Write {
    version: u32,
    home: PathBuf,
    config_before: String,
    config_after: String,
    auth_before: Option<Vec<u8>>,
    auth_after: Vec<u8>,
}
fn restore(path: &Path, before: Option<&[u8]>, after: &[u8]) -> Result<()> {
    let current = storage::read_bounded(path, LIMIT)?;
    if current.as_deref() == before {
        return Ok(());
    }
    if current.as_deref() != Some(after) {
        return Err(invalid("API 连接事务遇到外部修改，已保留恢复记录"));
    }
    match before {
        Some(bytes) => storage::atomic_write(path, bytes, Some(&storage::digest(after))),
        None => fs::remove_file(path).map_err(storage::io_error),
    }
}
pub fn recover_write(data: &Path) -> Result<()> {
    let path = data.join(WRITE_FILE);
    let Some(raw) = storage::read_bounded(&path, LIMIT * 16)? else {
        return Ok(());
    };
    let tx: Write =
        serde_json::from_slice(&raw).map_err(|_| invalid("API 连接恢复记录损坏，已保留"))?;
    if tx.version != 1 {
        return Err(invalid("API 连接恢复记录版本不受支持，已保留"));
    }
    let auth = restore(
        &tx.home.join("auth.json"),
        tx.auth_before.as_deref(),
        &tx.auth_after,
    );
    let config = restore(
        &tx.home.join("config.toml"),
        Some(tx.config_before.as_bytes()),
        tx.config_after.as_bytes(),
    );
    auth.and(config)?;
    fs::remove_file(path).map_err(storage::io_error)
}
pub fn write(
    data: &Path,
    home: &Path,
    id: &str,
    target: &Pair,
    expected: Option<&str>,
    allowed: Option<&[&Pair]>,
) -> Result<()> {
    if data.join(WRITE_FILE).exists() {
        return Err(invalid("请先处理未完成的 API 连接事务"));
    }
    let config_before = text(home)?;
    let auth_before = storage::read_bounded(&home.join("auth.json"), LIMIT)?;
    let (revision, current) = read(home, id)?;
    if expected.is_some_and(|e| e != revision) || allowed.is_some_and(|a| !a.contains(&&current)) {
        return Err(AppError::new(
            "CONFLICT",
            "base_url 或 auth.json 已变化，请刷新后重试",
        ));
    }
    // Recheck the exact bytes used for both patches before publishing a journal.
    if config_before != text(home)?
        || auth_before != storage::read_bounded(&home.join("auth.json"), LIMIT)?
    {
        return Err(AppError::new("CONFLICT", "API 配置正在变化，请重试"));
    }
    let url = target
        .base_url
        .as_deref()
        .ok_or_else(|| invalid("缺少供应商地址"))?;
    let key = target
        .token
        .as_deref()
        .filter(|k| !k.is_empty())
        .ok_or_else(|| invalid("缺少 API Key"))?;
    let mut value = auth(auth_before.as_deref())?;
    value["auth_mode"] = "apikey".into();
    value["OPENAI_API_KEY"] = key.into();
    let tx = Write {
        version: 1,
        home: home.to_owned(),
        config_after: patch_base(&config_before, id, url)?,
        config_before,
        auth_after: serde_json::to_vec_pretty(&value).map_err(|_| invalid("无法生成 API 凭据"))?,
        auth_before,
    };
    storage::atomic_write_bounded(
        &data.join(WRITE_FILE),
        &serde_json::to_vec(&tx).map_err(|_| invalid("无法生成 API 事务"))?,
        Some("missing"),
        LIMIT * 16,
    )?;
    let result = (|| {
        storage::atomic_write(
            &home.join("config.toml"),
            tx.config_after.as_bytes(),
            Some(&storage::digest(tx.config_before.as_bytes())),
        )?;
        storage::atomic_write(
            &home.join("auth.json"),
            &tx.auth_after,
            Some(&storage::revision(tx.auth_before.as_deref())),
        )?;
        if read(home, id)?.1 != *target {
            return Err(invalid("API 连接回读校验失败"));
        }
        Ok(())
    })();
    if let Err(error) = result {
        return match recover_write(data) {
            Ok(()) => Err(error),
            Err(_) => Err(invalid("API 连接写入失败且恢复遇到冲突，已保留记录")),
        };
    }
    fs::remove_file(data.join(WRITE_FILE)).map_err(storage::io_error)
}

#[cfg(test)]
#[path = "api_transaction_v020_tests.rs"]
mod tests;
