//! Grok's native model configuration. Credentials and transaction snapshots stay in Rust.
use super::takeover::Pair;
use crate::errors::code;
use crate::storage::{self, AppError, Result};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::Path};
use toml_edit::{value, DocumentMut, Item, Table};

pub const MANAGED: &str = "lich13-switch";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    #[default]
    Responses,
    ChatCompletions,
    Messages,
}
impl Protocol {
    pub fn name(self) -> &'static str {
        match self {
            Self::Responses => "responses",
            Self::ChatCompletions => "chat_completions",
            Self::Messages => "messages",
        }
    }
    pub fn accepts(self, path: &str) -> bool {
        let path = path.strip_prefix("/v1").unwrap_or(path);
        (match self {
            Self::Responses => path == "/responses" || path.starts_with("/responses/"),
            Self::ChatCompletions => path == "/chat/completions",
            Self::Messages => path == "/messages" || path == "/messages/count_tokens",
        }) || path == "/models"
            || path.starts_with("/models/")
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Connection {
    pub protocol: Protocol,
    pub headers: BTreeMap<String, String>,
    pub query: BTreeMap<String, String>,
}
impl Connection {
    pub fn authenticate(&self, headers: &mut hyper::HeaderMap, token: &str) -> Result<()> {
        headers.remove("authorization");
        headers.remove("x-api-key");
        let (name, token) = if self.protocol == Protocol::Messages {
            if !headers.contains_key("anthropic-version") {
                headers.insert(
                    "anthropic-version",
                    hyper::header::HeaderValue::from_static("2023-06-01"),
                );
            }
            ("x-api-key", token.to_owned())
        } else {
            ("authorization", format!("Bearer {token}"))
        };
        headers.insert(
            hyper::header::HeaderName::from_static(name),
            hyper::header::HeaderValue::from_str(&token)
                .map_err(|_| error("Grok Token 格式无效"))?,
        );
        for (name, value) in &self.headers {
            headers.insert(
                hyper::header::HeaderName::from_bytes(name.as_bytes())
                    .map_err(|_| error("Grok 请求头无效"))?,
                hyper::header::HeaderValue::from_str(value)
                    .map_err(|_| error("Grok 请求头无效"))?,
            );
        }
        Ok(())
    }
    pub fn query_uri(&self, uri: hyper::Uri) -> Result<hyper::Uri> {
        if self.query.is_empty() {
            return Ok(uri);
        }
        let mut url = url::Url::parse(&uri.to_string()).map_err(|_| error("Grok 接口地址无效"))?;
        let pairs: Vec<_> = url
            .query_pairs()
            .filter(|(k, _)| !self.query.contains_key(k.as_ref()))
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        url.query_pairs_mut()
            .clear()
            .extend_pairs(pairs)
            .extend_pairs(&self.query);
        url.as_str().parse().map_err(|_| error("Grok 接口地址无效"))
    }
}

pub struct Profile {
    pub pair: Pair,
    pub connection: Connection,
    pub model: String,
    pub(super) table: Table,
    pub selected: String,
    pub address_source: Option<&'static str>,
    pub credential_names: Vec<String>,
    pub protocol_known: bool,
}

fn error(message: &str) -> AppError {
    AppError::new(code::GROK_CONFIG, message)
}
fn parse(text: &str) -> Result<DocumentMut> {
    text.parse().map_err(|_| error("Grok 配置不是有效的 TOML"))
}
pub fn validate(text: &str) -> Result<()> {
    let doc = parse(text)?;
    if doc
        .get("models")
        .is_some_and(|v| v.as_table_like().is_none())
        || doc
            .get("model")
            .is_some_and(|v| v.as_table_like().is_none())
        || doc
            .get("models")
            .and_then(|v| v.get("default"))
            .is_some_and(|v| !v.is_str())
    {
        return Err(error("Grok 模型配置类型无效"));
    }
    Ok(())
}
fn string(table: &Table, key: &str) -> Result<Option<String>> {
    table
        .get(key)
        .map(|v| {
            v.as_str()
                .map(str::to_owned)
                .ok_or_else(|| error("Grok 连接字段类型无效"))
        })
        .transpose()
}
fn environment_names(item: &Item) -> Result<Vec<&str>> {
    let valid = |name: &str| {
        !name.is_empty()
            && name.len() <= 128
            && name.bytes().enumerate().all(|(index, c)| {
                c == b'_' || c.is_ascii_alphabetic() || (index > 0 && c.is_ascii_digit())
            })
    };
    if let Some(name) = item.as_str() {
        if !valid(name) {
            return Err(error("Grok 环境变量名称无效"));
        }
        return Ok(vec![name]);
    }
    item.as_array()
        .map(|v| {
            v.iter()
                .map(|v| {
                    v.as_str()
                        .filter(|name| valid(name))
                        .ok_or_else(|| error("Grok 环境变量名称无效"))
                })
                .collect()
        })
        .unwrap_or_else(|| Err(error("Grok 环境变量名称无效")))
}
fn headers(
    item: Option<&Item>,
    env: bool,
    resolve: &impl Fn(&str) -> Option<String>,
    out: &mut BTreeMap<String, String>,
) -> Result<()> {
    let Some(item) = item else {
        return Ok(());
    };
    let table = item
        .as_table_like()
        .ok_or_else(|| error("Grok 请求头配置无效"))?;
    for (name, item) in table.iter() {
        let name = hyper::header::HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| error("Grok 请求头名称无效"))?;
        if matches!(
            name.as_str(),
            "host"
                | "content-length"
                | "transfer-encoding"
                | "connection"
                | "proxy-authorization"
                | "upgrade"
                | "trailer"
                | "te"
        ) {
            return Err(error("Grok 自定义请求头不能覆盖传输字段"));
        }
        let source = item
            .as_str()
            .ok_or_else(|| error("Grok 请求头必须是字符串"))?;
        let resolved = if env {
            resolve(source).filter(|s| !s.is_empty())
        } else {
            Some(source.to_owned())
        };
        if let Some(resolved) = resolved {
            if resolved.len() > 8192 || hyper::header::HeaderValue::from_str(&resolved).is_err() {
                return Err(error("Grok 请求头值无效"));
            }
            out.insert(name.to_string(), resolved);
        }
    }
    Ok(())
}
pub fn profile(text: &str) -> Result<Profile> {
    profile_with_env(text, &|name| std::env::var(name).ok())
}
pub fn profile_with_env(text: &str, resolve: &impl Fn(&str) -> Option<String>) -> Result<Profile> {
    validate(text)?;
    let doc = parse(text)?;
    let selected = doc
        .get("models")
        .and_then(|v| v.get("default"))
        .and_then(Item::as_str)
        .unwrap_or("");
    let selected_table = doc.get("model").and_then(|v| v.get(selected));
    let mut table = Table::new();
    if let Some(provider) = selected_table
        .and_then(|v| v.get("model_provider"))
        .and_then(Item::as_str)
    {
        table = doc
            .get("model_providers")
            .and_then(|v| v.get(provider))
            .and_then(Item::as_table)
            .cloned()
            .ok_or_else(|| error("Grok 当前模型引用的供应商不存在"))?;
    }
    if let Some(selected_table) = selected_table {
        let selected_table = selected_table
            .as_table_like()
            .ok_or_else(|| error("Grok 模型配置无效"))?;
        let own_auth = selected_table
            .get("api_key")
            .and_then(Item::as_str)
            .is_some_and(|v| !v.trim().is_empty())
            || selected_table
                .get("env_key")
                .map(environment_names)
                .transpose()?
                .is_some_and(|v| !v.is_empty())
            || selected_table.contains_key("auth_provider")
            || selected_table.contains_key("auth_provider_command");
        let inherited_auth = table.contains_key("api_key")
            || table.contains_key("env_key")
            || table.contains_key("auth_provider")
            || table.contains_key("auth");
        if own_auth {
            for key in [
                "api_key",
                "env_key",
                "auth_provider",
                "auth_provider_command",
                "auth",
            ] {
                table.remove(key);
            }
        }
        for (key, item) in selected_table.iter() {
            if matches!(key, "extra_headers" | "query_params" | "env_http_headers")
                && item.as_table_like().is_some_and(|t| t.is_empty())
            {
                continue;
            }
            if !own_auth && inherited_auth && matches!(key, "api_key" | "env_key" | "auth_provider")
            {
                continue;
            }
            table.insert(key, item.clone());
        }
    }
    if table.contains_key("auth_provider_command")
        || table.contains_key("auth_provider")
        || table.contains_key("auth")
    {
        return Err(error(
            "Grok 当前模型使用外部认证，请在原生客户端管理；不会执行认证命令",
        ));
    }
    if table.contains_key("mtls_cert_dir") {
        return Err(error("Grok 当前模型使用 mTLS，不能直接接管其认证配置"));
    }
    // Complete custom models have Grok's documented chat_completions default.
    // A sparse native override needs catalog evidence; discovery does not guess.
    let protocol_known = table.contains_key("api_backend") || table.contains_key("model");
    let protocol = match string(&table, "api_backend")?.as_deref() {
        Some("responses") => Protocol::Responses,
        Some("messages") => Protocol::Messages,
        Some("chat_completions") => Protocol::ChatCompletions,
        None if selected_table.is_none() => Protocol::Responses,
        None => Protocol::ChatCompletions,
        _ => return Err(error("Grok 当前 API 协议不受支持")),
    };
    let preferred = resolve("GROK_AUTH_PREFERRED_METHOD").or_else(|| {
        doc.get("auth")
            .and_then(|v| v.get("preferred_method"))
            .and_then(Item::as_str)
            .map(str::to_owned)
    });
    if preferred
        .as_deref()
        .is_some_and(|v| !matches!(v, "api_key" | "oidc"))
    {
        return Err(error("Grok 认证方式无法识别"));
    }
    let credential_names = table
        .get("env_key")
        .map(environment_names)
        .transpose()?
        .unwrap_or_else(|| {
            if table.contains_key("api_key") {
                Vec::new()
            } else {
                vec!["XAI_API_KEY", "GROK_CODE_XAI_API_KEY"]
            }
        })
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    // Grok's model credentials take precedence over the global login preference.
    // The preference only governs the global credential fallback.
    let model_auth = table.contains_key("api_key") || table.contains_key("env_key");
    let api_auth = model_auth || preferred.as_deref() != Some("oidc");
    let token = if api_auth {
        string(&table, "api_key")?
            .filter(|s| !s.is_empty())
            .or_else(|| {
                credential_names
                    .iter()
                    .find_map(|name| resolve(name).filter(|s| !s.is_empty()))
            })
    } else {
        None
    };
    let endpoint = |key: &str| {
        doc.get("endpoints")
            .and_then(|v| v.get(key))
            .and_then(Item::as_str)
            .map(str::to_owned)
    };
    let candidates = [
        (
            if api_auth {
                string(&table, "api_base_url")?
            } else {
                None
            },
            "modelApi",
        ),
        (string(&table, "base_url")?, "model"),
        (resolve("GROK_MODELS_BASE_URL"), "environment"),
        (endpoint("models_base_url"), "global"),
        (
            if api_auth {
                resolve("GROK_XAI_API_BASE_URL").or_else(|| endpoint("xai_api_base_url"))
            } else {
                None
            },
            "globalApi",
        ),
    ];
    let resolved = candidates
        .into_iter()
        .find(|(value, _)| value.as_ref().is_some_and(|v| !v.is_empty()));
    let (base_url, address_source) = resolved
        .map(|(value, source)| (value, Some(source)))
        .unwrap_or_default();
    let mut extra = BTreeMap::new();
    headers(
        doc.get("models").and_then(|v| v.get("extra_headers")),
        false,
        resolve,
        &mut extra,
    )?;
    headers(table.get("extra_headers"), false, resolve, &mut extra)?;
    headers(table.get("env_http_headers"), true, resolve, &mut extra)?;
    let mut query = BTreeMap::new();
    if let Some(item) = table.get("query_params") {
        for (key, value) in item
            .as_table_like()
            .ok_or_else(|| error("Grok 查询参数无效"))?
            .iter()
        {
            query.insert(
                key.into(),
                value
                    .as_str()
                    .ok_or_else(|| error("Grok 查询参数必须为字符串"))?
                    .into(),
            );
        }
    }
    let model = string(&table, "model")?.unwrap_or_else(|| selected.to_owned());
    Ok(Profile {
        pair: Pair { base_url, token },
        connection: Connection {
            protocol,
            headers: extra,
            query,
        },
        model,
        table,
        selected: selected.to_owned(),
        address_source,
        credential_names,
        protocol_known,
    })
}
pub fn pair(text: &str) -> Result<Pair> {
    Ok(api_profile(text)?.pair)
}
pub fn api_profile(text: &str) -> Result<Profile> {
    let mut doc = parse(text)?;
    if doc.get("model").and_then(|v| v.get(MANAGED)).is_some() {
        if !doc.contains_key("models") {
            doc["models"] = Item::Table(Table::new());
        }
        doc["models"]["default"] = value(MANAGED);
    }
    profile(&doc.to_string())
}
pub fn fingerprint(text: &str) -> Result<String> {
    let p = api_profile(text)?;
    Ok(storage::digest(
        &serde_json::to_vec(&(p.selected, p.model, p.pair, p.connection))
            .map_err(|_| error("Grok 配置摘要失败"))?,
    ))
}
pub fn read(home: &Path) -> Result<(String, String)> {
    let raw = storage::read_optional(&home.join("config.toml"))?;
    let revision = storage::revision(raw.as_deref());
    let text =
        String::from_utf8(raw.unwrap_or_default()).map_err(|_| error("Grok 配置不是 UTF-8"))?;
    Ok((revision, text))
}

/// Legacy snapshots own the selector too; independent API snapshots own only our model.
#[derive(Clone, Serialize, Deserialize)]
pub struct Snapshot {
    before: String,
    applied: String,
    #[serde(default)]
    independent: bool,
}
impl Snapshot {
    pub fn new(text: &str, target: &Pair) -> Result<Self> {
        Self::with_protocol(text, target, None)
    }
    pub fn with_protocol(text: &str, target: &Pair, protocol: Option<Protocol>) -> Result<Self> {
        Self::build(text, target, protocol, None, false)
    }
    pub fn with_connection(text: &str, target: &Pair, connection: &Connection) -> Result<Self> {
        Self::build(
            text,
            target,
            Some(connection.protocol),
            Some(connection),
            false,
        )
    }
    pub fn api(text: &str, target: &Pair, connection: &Connection, direct: bool) -> Result<Self> {
        Self::build(
            text,
            target,
            Some(connection.protocol),
            direct.then_some(connection),
            true,
        )
    }
    fn build(
        text: &str,
        target: &Pair,
        protocol: Option<Protocol>,
        connection: Option<&Connection>,
        independent: bool,
    ) -> Result<Self> {
        let profile = if independent {
            api_profile(text)?
        } else {
            profile(text)?
        };
        if profile.model.is_empty() {
            return Err(error("Grok 未明确选择模型，无法创建 API 模型入口"));
        }
        let mut doc = parse(text)?;
        let selected = doc
            .get("models")
            .and_then(|v| v.get("default"))
            .and_then(Item::as_str);
        if !independent
            && selected != Some(MANAGED)
            && doc.get("model").and_then(|v| v.get(MANAGED)).is_some()
        {
            return Err(error("Grok 已有同名受管模型，请先在配置编辑器中处理"));
        }
        let base = target
            .base_url
            .as_deref()
            .ok_or_else(|| error("Grok 供应商地址缺失"))?;
        let token = target
            .token
            .as_deref()
            .ok_or_else(|| error("Grok 供应商 Token 缺失"))?;
        let mut table = profile.table;
        for field in [
            "env_key",
            "model_provider",
            "env_http_headers",
            "api_base_url",
            "mtls_cert_dir",
            "query_params",
            "auth_provider_command",
        ] {
            table.remove(field);
        }
        table["model"] = value(profile.model);
        table["api_backend"] = value(protocol.unwrap_or(profile.connection.protocol).name());
        table["base_url"] = value(base);
        table["api_key"] = value(token);
        // Mask inherited authentication/tenant headers in the local client. The
        // selected upstream's resolved headers are supplied privately by Rust.
        let mut masked = toml_edit::InlineTable::new();
        for name in profile.connection.headers.keys() {
            let replacement = match name.as_str() {
                "authorization" => format!("Bearer {token}"),
                "x-api-key" => token.to_owned(),
                _ => String::new(),
            };
            masked.insert(name, replacement.into());
        }
        // Direct provider selection uses that provider's private connection.
        // Gateway attachment leaves these masked and injects them upstream.
        if let Some(connection) = connection {
            for (name, value) in &connection.headers {
                masked.insert(name, value.clone().into());
            }
            if !connection.query.is_empty() {
                let mut query = toml_edit::InlineTable::new();
                for (name, value) in &connection.query {
                    query.insert(name, value.clone().into());
                }
                table["query_params"] = value(query);
            }
        }
        table["extra_headers"] = value(masked);
        if !doc.contains_key("models") {
            doc["models"] = Item::Table(Table::new());
        }
        if !doc.contains_key("model") {
            doc["model"] = Item::Table(Table::new());
        }
        if independent {
            // Native model selection belongs to Grok, including while this API entry is active.
        } else if let Some(previous) = doc["models"].get("default").and_then(Item::as_value) {
            let mut selected = toml_edit::Value::from(MANAGED);
            *selected.decor_mut() = previous.decor().clone();
            doc["models"]["default"] = Item::Value(selected);
        } else {
            doc["models"]["default"] = value(MANAGED);
        }
        doc["model"][MANAGED] = Item::Table(table);
        Ok(Self {
            before: text.into(),
            applied: doc.to_string(),
            independent,
        })
    }
    fn owned(&self, text: &str) -> Result<(Option<String>, Option<String>)> {
        let doc = parse(text)?;
        Ok((
            (!self.independent)
                .then(|| {
                    doc.get("models")
                        .and_then(|v| v.get("default"))
                        .map(ToString::to_string)
                })
                .flatten(),
            doc.get("model")
                .and_then(|v| v.get(MANAGED))
                .map(ToString::to_string),
        ))
    }
    pub fn apply(&self, path: &Path, expected: Option<&str>) -> Result<()> {
        self.write_owned(path, &self.applied, expected)
    }
    pub fn restore(&self, path: &Path) -> Result<()> {
        self.write_owned(path, &self.before, None)
    }
    fn write_owned(&self, path: &Path, target: &str, expected: Option<&str>) -> Result<()> {
        let raw = storage::read_optional(path)?;
        let revision = storage::revision(raw.as_deref());
        if expected.is_some_and(|r| r != revision) {
            return Err(AppError::new(code::CONFLICT, "Grok 配置已变化"));
        }
        let current = std::str::from_utf8(raw.as_deref().unwrap_or_default())
            .map_err(|_| error("Grok 配置不是 UTF-8"))?;
        if current == target {
            return Ok(());
        }
        let owned = self.owned(current)?;
        if owned != self.owned(&self.applied)? && owned != self.owned(&self.before)? {
            return Err(AppError::new(
                code::CONFLICT,
                "Grok 受管模型已被外部修改，已保留当前配置",
            ));
        }
        let output = if current == self.before || current == self.applied {
            target.to_owned()
        } else {
            let desired = parse(target)?;
            let mut doc = parse(current)?;
            for (section, key) in [("models", "default"), ("model", MANAGED)] {
                if self.independent && section == "models" {
                    continue;
                }
                if let Some(next) = desired.get(section).and_then(|v| v.get(key)) {
                    if !doc.contains_key(section) {
                        doc[section] = Item::Table(Table::new());
                    }
                    doc[section][key] = next.clone();
                } else if let Some(table) = doc.get_mut(section).and_then(Item::as_table_like_mut) {
                    table.remove(key);
                }
            }
            doc.to_string()
        };
        storage::atomic_write(path, output.as_bytes(), Some(&revision))
    }
    pub fn unchanged(&self, text: &str) -> Result<bool> {
        Ok(self.owned(text)? == self.owned(&self.applied)?)
    }
    pub fn applied(&self) -> &str {
        &self.applied
    }
    pub fn retain_original(&mut self, original: &Self) {
        self.before.clone_from(&original.before);
    }
    pub fn original_pair(&self) -> Result<Pair> {
        pair(&self.before)
    }
}
