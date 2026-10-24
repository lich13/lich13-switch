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
    base_url: String,
    connection: provider_protocol::Connection,
    mapping: dsh::ModelMapping,
}
impl Target {
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
impl Gateway {
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
    pub(crate) fn remember_test_user_agent(&self, raw: &[u8]) {
        self.remember_user_agent(raw);
    }
}
