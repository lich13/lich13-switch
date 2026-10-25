//! One loopback listener; client state and cancellation remain in each Gateway.
use super::{client::DEFAULT_SHARED_PORT, forward, registry::Registry, ClientId};
use crate::{
    errors::code,
    storage::{self, AppError, Result},
};
use serde::{Deserialize, Serialize};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};
use tokio::{net::TcpListener, sync::watch, task::JoinHandle};

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Config {
    version: u32,
    port: u16,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct View {
    pub port: u16,
    pub revision: String,
    pub running_clients: Vec<ClientId>,
}

struct State {
    version: u32,
    port: u16,
    revision: String,
    clients: Vec<ClientId>,
    shutdown: Option<watch::Sender<bool>>,
    task: Option<JoinHandle<()>>,
}
pub struct Listener {
    path: PathBuf,
    state: Mutex<State>,
}
pub struct Prepared {
    pub socket: Option<TcpListener>,
    pub port: u16,
    pub revision: String,
    pub previous_revision: String,
    pub notice: Option<String>,
}
impl Listener {
    pub fn load(path: PathBuf, legacy_codex_port: Option<u16>) -> Result<Self> {
        let raw = storage::read_optional(&path)?;
        let config = match raw.as_deref() {
            Some(raw) => serde_json::from_slice::<Config>(raw)
                .map_err(|_| AppError::new(code::SETTINGS, "共享网关配置损坏，已保留原文件"))?,
            None => Config {
                version: 2,
                port: legacy_codex_port.unwrap_or(DEFAULT_SHARED_PORT),
            },
        };
        if !matches!(config.version, 1 | 2) || config.port == 0 {
            return Err(AppError::new(code::SETTINGS, "共享网关配置版本或端口无效"));
        }
        let revision = if raw.is_none() {
            let bytes = serde_json::to_vec_pretty(&config)
                .map_err(|_| AppError::new(code::STORE, "无法保存共享网关配置"))?;
            storage::atomic_write(&path, &bytes, Some("missing"))?;
            storage::digest(&bytes)
        } else {
            storage::revision(raw.as_deref())
        };
        Ok(Self {
            path,
            state: Mutex::new(State {
                version: config.version,
                port: config.port,
                revision,
                clients: Vec::new(),
                shutdown: None,
                task: None,
            }),
        })
    }
    pub(super) fn request_port(&self) -> Result<u16> {
        self.state
            .lock()
            .map(|state| state.port)
            .map_err(|_| AppError::new(code::STORE, "共享网关状态不可用，请重新打开应用"))
    }
    fn lock_state(&self) -> Result<std::sync::MutexGuard<'_, State>> {
        self.state
            .lock()
            .map_err(|_| AppError::new(code::STORE, "共享网关状态不可用，请重新打开应用"))
    }
    pub fn view(&self) -> Result<View> {
        let s = self.lock_state()?;
        Ok(View {
            port: s.port,
            revision: s.revision.clone(),
            running_clients: s.clients.clone(),
        })
    }
    pub fn check_revision(&self) -> Result<()> {
        if storage::revision(storage::read_optional(&self.path)?.as_deref())
            != self.view()?.revision
        {
            return Err(AppError::new(
                code::CONFLICT,
                "共享网关配置已被外部修改，请重新打开应用后重试",
            ));
        }
        Ok(())
    }
    pub fn set_port(&self, port: u16, expected: &str) -> Result<()> {
        let mut s = self.lock_state()?;
        if expected != s.revision {
            return Err(AppError::new(
                code::CONFLICT,
                "共享端口已变化，请刷新后重试",
            ));
        }
        if !s.clients.is_empty() {
            return Err(AppError::new(
                code::GATEWAY_ACTIVE,
                "请先停用全部客户端网关，再修改共享端口",
            ));
        }
        if port == 0 {
            return Err(AppError::new(code::SETTINGS, "本地端口必须为 1–65535"));
        }
        let bytes = serde_json::to_vec_pretty(&Config { version: 2, port })
            .map_err(|_| AppError::new(code::STORE, "无法保存共享网关配置"))?;
        storage::atomic_write(&self.path, &bytes, Some(&s.revision))?;
        s.port = port;
        s.version = 2;
        s.revision = storage::digest(&bytes);
        Ok(())
    }
    /// Caller holds Registry::lifecycle until config commit and activation finish.
    pub async fn prepare(&self) -> Result<Prepared> {
        self.prepare_at(
            DEFAULT_SHARED_PORT,
            ClientId::Codex.legacy_port().expect("Codex legacy port"),
        )
        .await
    }
    async fn prepare_at(&self, default_port: u16, legacy_default_port: u16) -> Result<Prepared> {
        self.check_revision()?;
        let view = self.view()?;
        let version = self.lock_state()?.version;
        if !view.running_clients.is_empty() {
            return Ok(Prepared {
                socket: None,
                port: view.port,
                revision: view.revision.clone(),
                previous_revision: view.revision,
                notice: None,
            });
        }
        let migrate_default = version == 1 && view.port == legacy_default_port;
        let target = if migrate_default {
            default_port
        } else {
            view.port
        };
        let (socket, mut notice) =
            match TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, target)).await {
                Ok(socket) => (socket, None),
                Err(_) if migrate_default => (
                    TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, view.port))
                        .await
                        .map_err(|_| Self::port_error())?,
                    Some("新默认端口不可用，已保留原共享端口".to_owned()),
                ),
                Err(_) => return Err(Self::port_error()),
            };
        self.check_revision()?;
        let port = socket.local_addr().map_err(storage::io_error)?.port();
        if migrate_default && port != view.port && notice.is_none() {
            notice = Some(format!(
                "共享端口已迁移到 {port}，请重新打开已启动的 Codex / Claude Code / Grok"
            ));
        }
        let revision = if version == 1 {
            let bytes = serde_json::to_vec_pretty(&Config { version: 2, port })
                .map_err(|_| AppError::new(code::STORE, "无法保存共享网关配置"))?;
            let mut state = self.lock_state()?;
            storage::atomic_write(&self.path, &bytes, Some(&view.revision))?;
            state.port = port;
            state.version = 2;
            state.revision = storage::digest(&bytes);
            state.revision.clone()
        } else {
            view.revision.clone()
        };
        Ok(Prepared {
            socket: Some(socket),
            port,
            revision,
            previous_revision: view.revision,
            notice,
        })
    }
    fn port_error() -> AppError {
        AppError::new(code::PORT, "无法绑定共享本地端口，请检查端口占用或修改端口")
    }
    /// Caller already holds the client's state and the shared lifecycle guard.
    /// No configuration commit can happen after a failed listener lock, and the
    /// guard closes the gap between committing config and activating its socket.
    pub fn activate_with(
        &self,
        client: ClientId,
        listener: Option<TcpListener>,
        registry: Arc<Registry>,
        commit: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        let mut s = self.lock_state()?;
        commit()?;
        if !s.clients.contains(&client) {
            s.clients.push(client);
        }
        if let Some(listener) = listener {
            let (shutdown, rx) = watch::channel(false);
            s.shutdown = Some(shutdown);
            s.task = Some(tokio::spawn(forward::serve(registry, listener, rx)));
        }
        Ok(())
    }
    #[cfg(test)]
    pub fn activate(
        &self,
        client: ClientId,
        listener: Option<TcpListener>,
        registry: Arc<Registry>,
    ) -> Result<()> {
        self.activate_with(client, listener, registry, || Ok(()))
    }
    pub async fn deactivate(&self, client: ClientId) -> Result<()> {
        let (task, poisoned) = {
            let (mut s, poisoned) = match self.state.lock() {
                Ok(state) => (state, false),
                // Only extract actual resource handles. Never trust or repair the
                // poisoned lifecycle metadata, and never clear the poison flag.
                Err(error) => (error.into_inner(), true),
            };
            if !poisoned {
                s.clients.retain(|id| *id != client);
            }
            let task = if poisoned || s.clients.is_empty() {
                if let Some(shutdown) = s.shutdown.take() {
                    let _ = shutdown.send(true);
                }
                s.task.take()
            } else {
                None
            };
            (task, poisoned)
        };
        if let Some(task) = task {
            let _ = task.await;
        }
        if poisoned {
            Err(AppError::new(
                code::STORE,
                "共享网关状态不可用，监听资源已停止，请重新打开应用",
            ))
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
#[path = "listener_migration_tests.rs"]
mod migration_tests;

#[derive(Debug, PartialEq, Eq)]
pub enum RouteError {
    NotFound,
    Unauthorized,
}

pub fn token_matches(actual: &[u8], expected: &[u8]) -> bool {
    !expected.is_empty()
        && actual.len() == expected.len()
        && actual
            .iter()
            .zip(expected)
            .fold(0u8, |difference, (a, b)| difference | (a ^ b))
            == 0
}

/// Both headers, when present, must identify the same client. Repeated values
/// are rejected before routing and again at the forwarding boundary.
pub fn authorized(headers: &hyper::HeaderMap, token: &str) -> bool {
    let bearer = headers.get_all(hyper::header::AUTHORIZATION);
    let api = headers.get_all("x-api-key");
    let google = headers.get_all("x-goog-api-key");
    let google_count = google.iter().count();
    let bearer_count = bearer.iter().count();
    let api_count = api.iter().count();
    if bearer_count > 1
        || api_count > 1
        || google_count > 1
        || bearer_count + api_count + google_count == 0
    {
        return false;
    }
    let bearer_matches = bearer.iter().next().is_none_or(|value| {
        let bytes = value.as_bytes();
        let Some(separator) = bytes.iter().position(|byte| *byte == b' ') else {
            return false;
        };
        if !bytes[..separator].eq_ignore_ascii_case(b"Bearer") {
            return false;
        }
        // RFC's auth scheme is case-insensitive; the credential is not. Only
        // ASCII SP belongs to the separator, never trailing token whitespace.
        let credential = &bytes[separator..];
        let start = credential
            .iter()
            .position(|byte| *byte != b' ')
            .unwrap_or(credential.len());
        token_matches(&credential[start..], token.as_bytes())
    });
    let api_matches = api
        .iter()
        .next()
        .is_none_or(|value| token_matches(value.as_bytes(), token.as_bytes()));
    bearer_matches
        && api_matches
        && google
            .iter()
            .next()
            .is_none_or(|v| token_matches(v.as_bytes(), token.as_bytes()))
}

/// Legacy prefixes are aliases; unprefixed routes use an exact local token.
/// Models and protocol paths never infer a client (Grok also supports messages).
pub fn route(
    uri: &hyper::Uri,
    headers: &hyper::HeaderMap,
    registry: &Registry,
) -> std::result::Result<(ClientId, hyper::Uri), RouteError> {
    let path = uri.path();
    let (explicit, prefix) = if path == "/codex" || path.starts_with("/codex/") {
        (Some(ClientId::Codex), "/codex")
    } else if path == "/claude" || path.starts_with("/claude/") {
        (Some(ClientId::Claude), "/claude")
    } else if path == "/grok" || path.starts_with("/grok/") {
        (Some(ClientId::Grok), "/grok")
    } else if path == "/v1"
        || path.starts_with("/v1/")
        || path.starts_with("/v1beta/")
        || path.starts_with("/v1alpha/")
        || path.trim_end_matches('/') == "/alpha/search"
    {
        (None, "")
    } else {
        return Err(RouteError::NotFound);
    };
    // Authentication must not wait for any client's configuration transaction.
    let snapshot = registry.tokens_snapshot();
    let gateways: Vec<_> = snapshot
        .iter()
        .filter(|entry| entry.shared.strong_count() > 0)
        .collect();
    if explicit.is_some_and(|client| !gateways.iter().any(|gateway| gateway.client == client)) {
        return Err(RouteError::NotFound);
    }
    let mut matched = None;
    for gateway in gateways {
        if explicit.is_some_and(|client| client != gateway.client) {
            continue;
        }
        if authorized(headers, &gateway.token) {
            if matched.is_some() {
                return Err(RouteError::Unauthorized);
            }
            matched = Some(gateway.client);
        }
    }
    let client = matched.ok_or(RouteError::Unauthorized)?;
    if ((path.starts_with("/v1beta/") || path.starts_with("/v1alpha/")) && client != ClientId::Agy)
        || (client == ClientId::Agy && !super::gemini::accepts(path))
    {
        return Err(RouteError::NotFound);
    }
    if path.trim_end_matches('/') == "/alpha/search" && client != ClientId::Codex {
        return Err(RouteError::NotFound);
    }
    let remaining = &uri.path_and_query().ok_or(RouteError::NotFound)?.as_str()[prefix.len()..];
    let remaining = if remaining.is_empty() {
        "/".to_owned()
    } else if remaining.starts_with('?') {
        format!("/{remaining}")
    } else {
        remaining.to_owned()
    };
    let mut parts = uri.clone().into_parts();
    parts.path_and_query = Some(remaining.parse().map_err(|_| RouteError::NotFound)?);
    Ok((
        client,
        hyper::Uri::from_parts(parts).map_err(|_| RouteError::NotFound)?,
    ))
}

#[cfg(test)]
#[path = "listener_token_tests.rs"]
mod token_tests;

#[cfg(test)]
#[path = "listener_fault_tests.rs"]
mod fault_tests;
