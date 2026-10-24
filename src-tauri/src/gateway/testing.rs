//! Direct test snapshots never enter gateway forwarding or business accounting.
use super::*;
use hyper::{header, HeaderMap};

#[derive(Clone)]
pub(crate) struct Target {
    pub client_id: ClientId,
    pub provider_id: String,
    pub provider_name: String,
    pub version: String,
    pub search_only: bool,
    pub protocol: String,
    pub headers: HeaderMap,
    pub base_url: String,
    connection: provider_protocol::Connection,
    mapping: dsh::ModelMapping,
}
impl Target {
    pub fn redact(&self, text: &str) -> String {
        let mut result = text.to_owned();
        let mut secrets: Vec<&str> = self
            .headers
            .iter()
            .filter(|(name, _)| {
                matches!(
                    name.as_str(),
                    "authorization"
                        | "x-api-key"
                        | "cookie"
                        | "user-agent"
                        | "originator"
                        | "x-codex-installation-id"
                ) || self.connection.headers.contains_key(name.as_str())
            })
            .filter_map(|(_, value)| value.to_str().ok())
            .flat_map(|value| [value, value.strip_prefix("Bearer ").unwrap_or(value)])
            .chain(self.connection.query.values().map(String::as_str))
            .filter(|value| !value.is_empty())
            .collect();
        secrets.sort_by_key(|value| std::cmp::Reverse(value.len()));
        for secret in secrets {
            result = result.replace(secret, "[已隐藏]");
            let encoded: String = url::form_urlencoded::byte_serialize(secret.as_bytes()).collect();
            result = result.replace(&encoded, "[已隐藏]");
            if let Ok(escaped) = serde_json::to_string(secret) {
                result = result.replace(&escaped[1..escaped.len() - 1], "[已隐藏]");
            }
        }
        result
    }
    pub fn url(&self, path: &str) -> Result<String> {
        let uri = forward::target(
            &self.base_url,
            &path
                .parse()
                .map_err(|_| AppError::new(code::PROVIDER, "测试接口无效"))?,
        )
        .map_err(|_| AppError::new(code::PROVIDER, "测试接口地址无效"))?;
        Ok(self.connection.query_uri(uri)?.to_string())
    }
    pub fn model(&self, requested: &str) -> String {
        self.mapping
            .resolve(requested)
            .unwrap_or(requested)
            .to_owned()
    }
}
#[derive(Clone)]
pub(crate) struct Candidate {
    pub id: String,
    pub queued: bool,
    pub search_only: bool,
}
fn codex_identity(
    user_agent: &header::HeaderValue,
    home: Option<&Path>,
) -> Result<(header::HeaderValue, header::HeaderValue)> {
    let invalid_ua = || {
        AppError::new(
            code::STATE,
            "已保存的 Codex User-Agent 格式无效，请先通过 Codex 完成一次正常请求",
        )
    };
    let (originator, _) = user_agent
        .to_str()
        .ok()
        .and_then(|ua| ua.split_once('/'))
        .filter(|(name, version)| {
            !name.trim().is_empty()
                && !name.chars().any(char::is_control)
                && !version.trim().is_empty()
        })
        .ok_or_else(invalid_ua)?;
    let originator = header::HeaderValue::from_str(originator.trim()).map_err(|_| invalid_ua())?;
    let home = home.ok_or_else(|| AppError::new(code::STATE, "尚未选择 Codex 配置目录"))?;
    let raw = storage::read_bounded(&home.join("installation_id"), 128)
        .map_err(|error| {
            AppError::new(
                code::STATE,
                &format!("无法读取 Codex installation_id：{}", error.message),
            )
        })?
        .ok_or_else(|| {
            AppError::new(
                code::STATE,
                "未找到 Codex installation_id，请先启动 Codex 后重试",
            )
        })?;
    let installation = std::str::from_utf8(&raw)
        .ok()
        .map(str::trim)
        .filter(|value| uuid::Uuid::parse_str(value).is_ok())
        .and_then(|value| header::HeaderValue::from_str(value).ok())
        .ok_or_else(|| {
            AppError::new(
                code::STATE,
                "Codex installation_id 格式无效，请检查当前 Codex 目录后重试",
            )
        })?;
    Ok((originator, installation))
}
impl Gateway {
    pub(crate) fn test_endpoint(&self, id: &str, endpoint: Option<&str>) -> Result<String> {
        let state = self.request_state()?;
        let provider = state
            .store
            .providers
            .iter()
            .find(|p| p.id == id)
            .ok_or_else(|| AppError::new(code::PROVIDER, "供应商已删除，请刷新列表"))?;
        let selected = endpoint.unwrap_or(&provider.base_url).trim_end_matches('/');
        std::iter::once(&provider.base_url)
            .chain(state.store.provider_endpoints.get(id).into_iter().flatten())
            .find(|url| url.trim_end_matches('/') == selected)
            .map(|url| url.trim_end_matches('/').to_owned())
            .ok_or_else(|| AppError::new(code::PROVIDER, "测试端点未配置或已删除，请重新选择"))
    }
    pub(crate) fn test_target_at(
        &self,
        id: &str,
        expected: Option<&str>,
        endpoint: Option<&str>,
    ) -> Result<Target> {
        let mut target = self.test_target(id, expected)?;
        target.base_url = self.test_endpoint(id, endpoint)?;
        // Both reads must refer to the same provider configuration.
        self.verify_test_version(id, &target.version)?;
        Ok(target)
    }
    pub(crate) fn detection_request_counts(
        &self,
        at: i64,
    ) -> Result<std::collections::BTreeMap<String, u64>> {
        let usage = self
            .0
            .usage
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| AppError::new(code::STATE, "调用统计暂不可用，等待重新读取"))?;
        usage.recent_gateway_requests("codex", at)
    }
    pub(crate) fn test_candidates(&self) -> Result<Vec<Candidate>> {
        Ok(self
            .request_state()?
            .store
            .providers
            .iter()
            .map(|p| Candidate {
                id: p.id.clone(),
                queued: p.queued,
                search_only: p.search_only,
            })
            .collect())
    }
    pub(crate) fn test_target(&self, id: &str, expected: Option<&str>) -> Result<Target> {
        let user_agent = self.successful_user_agent()?;
        let state = self.request_state()?;
        if expected.is_some_and(|expected| {
            self.view_revision(&state)
                .map_or(true, |rev| rev != expected)
        }) {
            return Err(AppError::new(
                code::CONFLICT,
                "供应商配置已变化，请刷新测试窗口",
            ));
        }
        let provider = state
            .store
            .providers
            .iter()
            .find(|p| p.id == id)
            .ok_or_else(|| AppError::new(code::PROVIDER, "供应商已删除，请刷新列表"))?;
        let connection = match self.client_id() {
            ClientId::Claude => provider_protocol::Connection {
                protocol: provider_protocol::Protocol::Messages,
                ..Default::default()
            },
            ClientId::Grok | ClientId::Dsh => state
                .store
                .grok_connections
                .get(id)
                .cloned()
                .unwrap_or_default(),
            ClientId::Codex => provider_protocol::Connection::default(),
        };
        let mut headers = HeaderMap::new();
        connection.authenticate(&mut headers, &provider.token)?;
        if self.client_id() == ClientId::Codex {
            let (originator, installation) = codex_identity(&user_agent, state.home.as_deref())?;
            headers.insert("originator", originator);
            headers.insert("x-codex-installation-id", installation);
        }
        headers.insert(header::USER_AGENT, user_agent);
        headers.insert(
            header::ACCEPT_ENCODING,
            header::HeaderValue::from_static("identity"),
        );
        Ok(Target {
            client_id: self.client_id(),
            provider_id: id.into(),
            provider_name: provider.name.clone(),
            version: provider.version.clone(),
            search_only: provider.search_only,
            protocol: connection.protocol.name().into(),
            headers,
            base_url: provider.base_url.clone(),
            connection,
            mapping: if self.client_id() == ClientId::Dsh {
                state.store.dsh_models.get(id).cloned().unwrap_or_default()
            } else {
                Default::default()
            },
        })
    }
    pub(crate) fn verify_test_version(&self, id: &str, version: &str) -> Result<()> {
        if self
            .request_state()?
            .store
            .providers
            .iter()
            .any(|p| p.id == id && p.version == version)
        {
            Ok(())
        } else {
            Err(AppError::new(
                code::CONFLICT,
                "供应商已修改或删除，请重新测试",
            ))
        }
    }
    pub(crate) fn dequeue_degraded(&self, id: &str, version: &str) -> Result<()> {
        let mut state = self.request_state()?;
        let mut next = state.store.clone();
        let provider = next
            .providers
            .iter_mut()
            .find(|p| p.id == id && p.version == version)
            .ok_or_else(|| {
                AppError::new(
                    code::CONFLICT,
                    "供应商已修改，降智标记已保留，请手动检查队列",
                )
            })?;
        provider.queued = false;
        let revision = next.persist(&self.0.data.join("gateway.json"), &state.revision)?;
        self.replace_store(&mut state, next);
        state.revision = revision;
        self.0
            .admission
            .configure(&state.store.providers, state.running);
        drop(state);
        self.changed();
        Ok(())
    }
}
#[cfg(test)]
impl Gateway {
    pub(crate) fn test_usage(&self) -> crate::usage::Service {
        self.0.usage.lock().unwrap().clone().unwrap()
    }
    pub(crate) fn remember_test_user_agent(&self, raw: &[u8]) {
        self.remember_user_agent(raw);
    }
}
