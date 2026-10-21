//! Shared native protocol/authentication without request body conversion.
use crate::{
    errors::code,
    storage::{AppError, Result},
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
fn error(message: &str) -> AppError {
    AppError::new(code::PROVIDER, message)
}
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
            hyper::header::HeaderValue::from_str(&token).map_err(|_| error("Token 格式无效"))?,
        );
        for (name, value) in &self.headers {
            headers.insert(
                hyper::header::HeaderName::from_bytes(name.as_bytes())
                    .map_err(|_| error("供应商请求头无效"))?,
                hyper::header::HeaderValue::from_str(value)
                    .map_err(|_| error("供应商请求头无效"))?,
            );
        }
        Ok(())
    }
    pub fn query_uri(&self, uri: hyper::Uri) -> Result<hyper::Uri> {
        if self.query.is_empty() {
            return Ok(uri);
        }
        let mut url = url::Url::parse(&uri.to_string()).map_err(|_| error("供应商接口地址无效"))?;
        let pairs: Vec<_> = url
            .query_pairs()
            .filter(|(k, _)| !self.query.contains_key(k.as_ref()))
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        url.query_pairs_mut()
            .clear()
            .extend_pairs(pairs)
            .extend_pairs(&self.query);
        url.as_str()
            .parse()
            .map_err(|_| error("供应商接口地址无效"))
    }
}
