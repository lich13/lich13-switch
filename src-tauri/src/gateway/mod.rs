use crate::errors::code;
#[cfg(test)]
mod api_login_tests;
mod claude_config;
mod codex_api;
pub(crate) mod current;
pub(crate) mod dsh;
pub(crate) mod grok_config;
mod grok_import;
mod grok_layers;
mod provider_protocol;
pub use grok_import::Preview as GrokImportPreview;
#[cfg(test)]
mod grok_config_tests;
#[cfg(test)]
mod grok_import_tests;
#[cfg(test)]
mod shared_listener_tests;
#[cfg(test)]
mod shared_transport_tests;
pub(crate) use takeover::manages_auth;
mod client;
#[cfg(test)]
mod client_cancel_tests;
mod listener;
mod locks;
mod registry;
pub use listener::View as ListenerView;
mod retired;
pub use client::{claude_home, dsh_home, grok_home, ClientId};
pub(crate) use retired::migrate as retire_legacy_proxy;
mod admission;
mod attempt;
#[cfg(test)]
mod business_ua_tests;
pub mod catalog;
mod circuit;
mod compaction;
mod connector;
#[cfg(test)]
mod diagnostic_test_support;
mod endpoints;
pub(crate) mod exchange;
#[cfg(test)]
mod exchange_tests;
pub(crate) mod failover;
#[cfg(test)]
mod first_byte_timeout_tests;
#[cfg(test)]
mod first_turn_structure_tests;
mod forward;
#[cfg(test)]
mod handoff_v0259_tests;
mod health_settlement;
#[cfg(test)]
mod health_settlement_tests;
#[cfg(test)]
mod interruption_tests;
pub(crate) mod metadata;
mod model;
mod model_health;
#[cfg(test)]
mod model_isolation_tests;
pub(crate) mod model_list;
#[cfg(test)]
mod new_thread_policy_tests;
#[cfg(test)]
mod partial_response_tests;
mod protocol;
#[cfg(test)]
mod protocol_tests;
mod quota;
#[cfg(test)]
mod quota_routing_v0257_tests;
mod quota_wait;
mod replay;
#[cfg(test)]
mod replay_boundary_tests;
mod replay_policy;
mod routing;
mod routing_log;
mod rpm;
pub(crate) mod testing;
mod thread_capacity;
#[cfg(test)]
mod token_reveal_tests;
mod upstream_error;
#[cfg(test)]
mod upstream_reason_v0257_tests;
#[cfg(test)]
mod usage_stream_tests;
#[cfg(test)]
mod v02510_delivery_tests;
#[cfg(test)]
mod v02510_error_tests;
#[cfg(test)]
mod v02510_quota_routing_tests;
#[cfg(test)]
mod v02511_event_policy_tests;
mod wait_budget;
mod websocket;
mod wire_progress;
pub use quota::QuotaView;
mod takeover;
#[cfg(test)]
mod takeover_alias_tests;
pub(crate) use takeover::recover as recover_for_migration;
#[cfg(test)]
mod tests;
use crate::storage::{self, AppError, Result};
use circuit::{Circuit, Health};
use connector::Connector;
use hyper_util::{client::legacy::Client, rt::TokioExecutor};
pub use model::{Edit, Settings};
use model::{Provider, Store};
use replay::WireBody;
use serde::Serialize;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use tokio::sync::{broadcast, watch};
type HttpClient = Client<Connector, WireBody>;
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ProviderView {
    pub protocol: Option<grok_config::Protocol>,
    pub id: String,
    pub name: String,
    base_url: String,
    extra_base_urls: Vec<String>,
    endpoint_measurements: Vec<endpoints::Measurement>,
    preferred_base_url: String,
    queued: bool,
    health: Health,
    model_health: Vec<model_health::ModelHealth>,
    quota_version: String,
    has_quota_access_token: bool,
    quota: Option<QuotaView>,
    quota_checking: bool,
    quota_check: Option<crate::events::QuotaCheck>,
    pub max_concurrency: u32,
    pub max_rpm: u32,
    pub active_requests: usize,
    pub reserved_threads: usize,
    pub rpm_used: usize,
    pub rpm_retry_in: u64,
    pub rpm_limited: bool,
    pub rpm_wait_reason: Option<crate::events::RpmWaitReason>,
    pub rpm_ledger_error: bool,
    pub allowed_models: Option<Vec<String>>,
    pub dsh_models: Option<dsh::ModelMapping>,
    pub handoff_after_compaction: bool,
    pub take_new_threads: bool,
    pub search_only: bool,
}
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct View {
    pub listener: ListenerView,
    pub grok_models: Vec<grok_import::ModelView>,
    pub grok_managed: bool,
    pub dsh_profile: Option<dsh::Options>,
    pub dsh_profiles: Vec<dsh::ProfileCurrent>,
    pub client_id: ClientId,
    pub revision: String,
    pub running: bool,
    pub address: String,
    pub mode: String,
    pub selected: Option<String>,
    pub last_successful: Option<String>,
    pub config_revision: Option<String>,
    pub config_provider: Option<String>,
    pub config_state: String,
    pub current: current::Current,
    pub config_error: Option<String>,
    pub config_warning: Option<String>,
    pub connection_mode: String,
    pub providers: Vec<ProviderView>,
    pub settings: Settings,
    pub active_connections: usize,
    pub waiting_requests: usize,
    pub capacity_retries: Vec<admission::CapacityRetry>,
    pub websocket_retries: Vec<admission::CapacityRetry>,
    pub transient_retries: Vec<admission::CapacityRetry>,
    pub handoff_waiting: Vec<compaction::HandoffWaiting>,
    pub error: Option<String>,
    pub recovery_pending: bool,
}
struct Inner {
    store: Store,
    revision: String,
    running: bool,
    upgrade_error: Option<AppError>,
    shutdown: Option<watch::Sender<bool>>,
    error: Option<String>,
    circuits: HashMap<String, Circuit>,
    model_circuits: HashMap<String, model_health::Models>,
    affinity: HashMap<String, (String, Option<String>, Instant)>,
    last_successful: Option<String>,
    home: Option<PathBuf>,
}
struct Shared {
    // Scripted routing fixtures have no discovery endpoint. Catalog tests opt in
    // explicitly so automatic metadata requests cannot consume business scripts.
    #[cfg(test)]
    discover_new_providers: std::sync::atomic::AtomicBool,
    grok_import: grok_import::Service,
    client: ClientId,
    diagnostics: Mutex<Option<crate::events::Service>>,
    usage: Mutex<Option<crate::usage::Service>>,
    registry: Arc<registry::Registry>,
    inner: Mutex<Inner>,
    lifecycle: tokio::sync::Mutex<()>,
    clients: Mutex<HashMap<String, HttpClient>>,
    data: PathBuf,
    spool: storage::PrivateTempDir,
    active: AtomicUsize,
    events: broadcast::Sender<()>,
    quota: quota::Service,
    catalog: catalog::Service,
    model_list: model_list::Service,
    endpoints: endpoints::Service,
    exchange: exchange::Service,
    admission: admission::Scheduler,
    compaction: Arc<compaction::Registry>,
}
impl Drop for Shared {
    fn drop(&mut self) {
        self.registry.unregister(self);
    }
}
#[derive(Clone)]
pub struct Gateway(Arc<Shared>);
#[derive(Clone)]
struct Route {
    dsh_models: Option<dsh::ModelMapping>,
    grok: Option<grok_config::Connection>,
    client_id: ClientId,
    provider: Provider,
    client: HttpClient,
    provider_circuit: Circuit,
    model_circuits: model_health::Models,
}
impl Route {
    fn accepts_protocol(&self, path: &str) -> bool {
        self.grok.as_ref().is_none_or(|c| c.protocol.accepts(path))
    }
    fn target(
        &self,
        incoming: &hyper::Uri,
    ) -> std::result::Result<hyper::Uri, Box<dyn std::error::Error + Send + Sync>> {
        let uri = forward::target(&self.provider.base_url, incoming)?;
        if let Some(c) = &self.grok {
            c.query_uri(uri).map_err(|e| e.message.into())
        } else {
            Ok(uri)
        }
    }

    fn model_circuit(&self, requirement: &routing::Requirement) -> Option<Circuit> {
        let routing::Requirement::Model(model) = requirement else {
            return None;
        };
        let circuit = self.model_circuits.circuit(model);
        circuit.set_single_provider_protection(self.provider_circuit.protection());
        Some(circuit)
    }
    fn health_for(&self, requirement: &routing::Requirement) -> Health {
        let provider = self.provider_circuit.health();
        if !provider.available {
            return provider;
        }
        self.model_circuit(requirement)
            .map(|c| c.health())
            .unwrap_or(provider)
    }
}
fn pkey(p: &Provider) -> String {
    format!("provider:{}:{}", p.id, p.version)
}
impl Gateway {
    fn claude_official(&self) -> bool {
        self.0.client == ClientId::Claude
            && self
                .0
                .data
                .parent()
                .is_some_and(crate::claude_profile::blocks)
    }

    pub fn set_usage(&self, service: crate::usage::Service) {
        *self.0.usage.lock().unwrap() = Some(service);
    }
    fn usage_operation(
        &self,
        model: Option<&str>,
        operation: crate::usage::model::Operation,
    ) -> Option<crate::usage::Trace> {
        self.0.usage.lock().unwrap().as_ref().map(|s| {
            s.begin_operation(
                match self.0.client {
                    ClientId::Codex => "codex",
                    ClientId::Claude => "claude",
                    ClientId::Grok => "grok",
                    ClientId::Dsh => "dsh",
                },
                model,
                operation,
            )
        })
    }

    pub fn report_diagnostics(&self) {
        self.0.exchange.prune();
        let Ok(view) = self.try_view() else {
            return;
        };
        let circuits: Vec<_> = {
            let state = self.0.inner.lock().unwrap();
            state
                .circuits
                .values()
                .cloned()
                .chain(
                    state
                        .model_circuits
                        .values()
                        .flat_map(model_health::Models::circuits),
                )
                .collect()
        };
        for circuit in circuits {
            if let Some(mut event) = circuit.take_open_event() {
                event.notification_provider = view
                    .providers
                    .iter()
                    .find(|p| Some(&p.id) == event.provider_id.as_ref())
                    .and_then(|p| crate::events::safe_message(&p.name));
                if let Some(events) = locks::published(&self.0.diagnostics).as_ref() {
                    events.emit(event);
                }
            }
        }
    }
    pub fn set_diagnostics(&self, events: crate::events::Service) {
        self.0.endpoints.diagnostics(self.0.client, events.clone());
        self.0
            .compaction
            .notifications(self.0.client, events.clone());
        self.0
            .compaction
            .provider_names(&self.0.inner.lock().unwrap().store.providers);
        *locks::published(&self.0.diagnostics) = Some(events);
    }
    fn record(
        &self,
        provider: Option<&str>,
        model: Option<&str>,
        reason: crate::events::Reason,
        action: crate::events::Action,
        status: Option<u16>,
        attempt: Option<u32>,
    ) {
        if let Some(events) = locks::published(&self.0.diagnostics).as_ref() {
            events.emit(crate::events::Record::new(
                Some(self.0.client),
                provider,
                model,
                reason,
                action,
                status,
                attempt,
            ));
        }
    }

    fn record_final(
        &self,
        cause: Option<&health_settlement::Reported>,
        reported: bool,
        provider: Option<&str>,
        model: Option<&str>,
        status: Option<u16>,
        attempt: usize,
    ) {
        use crate::events::{Action, Reason};
        if reported && cause.is_some_and(|cause| cause.queued) {
            return;
        }
        let mut event = crate::events::Record::new(
            Some(self.0.client),
            provider,
            model,
            Reason::FailoverExhausted,
            Action::Returned,
            cause.and_then(|c| c.status).or(status),
            Some(attempt.min(u32::MAX as usize) as u32),
        );
        if let Some(cause) = cause {
            event.details = cause.details.clone();
            event.details.cause_id = Some(cause.id.clone());
        }
        if cause.is_none() {
            event.details.local_code = Some(code::NO_ELIGIBLE_PROVIDER.into());
            event.details.message = Some("没有符合当前路由条件的可用供应商".into());
        }
        if let Some(service) = locks::published(&self.0.diagnostics).as_ref() {
            service.emit(event);
        }
    }
    fn record_policy_rejection(&self, model: Option<&str>, websocket: bool, message: &'static str) {
        use crate::events::{Action, Details, Phase, Reason, Record};
        let mut event = Record::new(
            Some(self.0.client),
            None,
            model,
            Reason::ProtocolError,
            Action::Returned,
            (!websocket).then_some(503),
            Some(0),
        );
        event.details = Details {
            local_code: Some(code::NO_ELIGIBLE_PROVIDER.into()),
            message: Some(message.into()),
            phase: Some(Phase::Admission),
            counted_failure: Some(false),
            ..Default::default()
        };
        if let Some(service) = locks::published(&self.0.diagnostics).as_ref() {
            service.emit(event);
        }
    }
    pub fn set_quota_interval(&self, seconds: u64) {
        self.0.quota.set_interval(seconds);
    }
    pub fn new(data: PathBuf) -> Result<Self> {
        Self::new_client(data, ClientId::Codex, None)
    }
    pub fn companion(&self, data: PathBuf) -> Result<Self> {
        Self::new_client(data, ClientId::Claude, Some(self.0.registry.clone()))
    }
    pub fn grok_companion(&self, data: PathBuf) -> Result<Self> {
        Self::new_client(data, ClientId::Grok, Some(self.0.registry.clone()))
    }
    pub fn dsh_companion(&self, data: PathBuf) -> Result<Self> {
        Self::new_client(data, ClientId::Dsh, Some(self.0.registry.clone()))
    }
    fn new_client(
        data: PathBuf,
        client: ClientId,
        shared: Option<Arc<registry::Registry>>,
    ) -> Result<Self> {
        storage::private_dir(&data)?;
        let upgrade_error = crate::migrations::gateway(&data)
            .and_then(|()| grok_import::recover(&data))
            .and_then(|()| dsh::recover(&data))
            .and_then(|()| crate::migrations::gateway_state_after_recovery(&data))
            .err();
        let error = upgrade_error.as_ref().map(|e| e.message.clone());
        let (store, revision) = Store::load(&data.join("gateway.json"))?;
        let registry = match shared {
            Some(n) => n,
            None => Arc::new(registry::Registry::new(
                data.clone(),
                (revision != "missing").then_some(store.settings.port),
            )?),
        };
        let spool = storage::private_tempdir(&data, "gateway-spool-")?;
        let (events, _) = broadcast::channel(32);
        let admission = admission::Scheduler::new(events.clone(), data.join("rpm-window.json"));
        admission.configure(&store.providers, false);
        admission.configure_retention(store.settings.thread_idle_release_seconds);
        let local_token = store.local_token.clone();
        let gateway = Self(Arc::new(Shared {
            #[cfg(test)]
            discover_new_providers: std::sync::atomic::AtomicBool::new(false),
            grok_import: grok_import::Service::default(),
            client,
            diagnostics: Mutex::new(None),
            usage: Mutex::new(None),
            registry: registry.clone(),
            inner: Mutex::new(Inner {
                store,
                revision,
                running: false,
                upgrade_error,
                shutdown: None,
                error,
                circuits: HashMap::new(),
                model_circuits: HashMap::new(),
                affinity: HashMap::new(),
                last_successful: None,
                home: None,
            }),
            lifecycle: tokio::sync::Mutex::new(()),
            clients: Mutex::new(HashMap::new()),
            compaction: compaction::Registry::new(data.join("conversation-owners.json")),
            exchange: exchange::Service::default(),
            endpoints: endpoints::Service::new(data.join("endpoint-owners.json")),
            catalog: catalog::Service::new(data.join("provider-models.json")),
            model_list: model_list::Service::new(data.join("model-list.json")),
            data,
            spool,
            active: AtomicUsize::new(0),
            events,
            quota: quota::Service::new(),
            admission,
        }));
        registry.register(&gateway.0, &local_token)?;
        Ok(gateway)
    }
    fn replace_store(&self, state: &mut Inner, store: Store) {
        let added: Vec<_> = store
            .providers
            .iter()
            .filter(|p| !state.store.providers.iter().any(|old| old.id == p.id))
            .map(|p| p.id.clone())
            .collect();
        let removed: Vec<_> = state
            .store
            .providers
            .iter()
            .filter(|p| !store.providers.iter().any(|new| new.id == p.id))
            .map(|p| p.id.clone())
            .collect();
        if let Err(e) = self.0.catalog.remove(&removed) {
            state.error = Some(e.message);
        }
        if let Err(e) = self.0.model_list.forget(&removed) {
            state.error = Some(e.message);
        }
        state.store = store;
        self.0
            .registry
            .publish_token(&self.0, &state.store.local_token);
        #[cfg(test)]
        if !self.0.discover_new_providers.load(Ordering::Relaxed) {
            return;
        }
        if !added.is_empty() {
            let gateway = self.clone();
            tauri::async_runtime::spawn(async move {
                if let Err(e) = gateway.backfill_models().await {
                    gateway.0.inner.lock().unwrap().error = Some(e.message);
                    gateway.changed();
                }
            });
        }
    }

    pub fn import_initial(&self, home: &Path) -> Result<()> {
        let _guard = self.0.registry.mutation.lock().unwrap();
        let mut s = self.0.inner.lock().unwrap();
        s.home = Some(home.to_owned());
        // Grok discovery is read-only. Its explicit import preview owns migration.
        if matches!(self.0.client, ClientId::Grok | ClientId::Dsh) {
            return Ok(());
        }
        if let Some(error) = &s.upgrade_error {
            return Err(error.clone());
        }
        if s.store.initialized {
            return Ok(());
        }
        let mut next = s.store.clone();
        if self.0.client == ClientId::Codex {
            // Existing stores keep their historical Bearer behavior. Only a new
            // install imports the authentication route already in use.
            if s.revision != "missing" {
                return Ok(());
            }
            let Ok(connection) = codex_api::detect(home) else {
                return Ok(());
            };
            next.connection = connection;
        }
        let initial_import = next.providers.is_empty();
        if initial_import {
            if !self.0.client.config(home).exists() {
                return Ok(());
            }
            let (_, pair) = takeover::read_connection(self.0.client, home, &next.connection)?;
            if pair.base_url.as_deref().is_none_or(str::is_empty)
                && pair.token.as_deref().is_none_or(str::is_empty)
            {
                return Ok(());
            }
            let (base_url, token) = Self::import_pair(self.0.client, home, &next.connection)?;
            for port in self.0.registry.ports.lock().unwrap().iter() {
                model::base_url(&base_url, *port)?;
            }
            next.edit(
                Edit::SaveProvider {
                    search_only: None,
                    quota_access_token: None,
                    id: None,
                    base_url,
                    token,
                    name: None,
                },
                false,
            )?;
        }
        if self.0.client == ClientId::Grok && initial_import {
            if let Some(provider) = next.providers.last() {
                next.grok_connections.insert(
                    provider.id.clone(),
                    grok_config::profile(&grok_config::read(home)?.1)?.connection,
                );
            }
        }
        next.initialized = true;
        s.revision = next.persist(&self.0.data.join("gateway.json"), &s.revision)?;
        self.replace_store(&mut s, next);
        self.0.admission.configure(&s.store.providers, false);
        self.0
            .admission
            .configure_retention(s.store.settings.thread_idle_release_seconds);
        self.0.compaction.provider_names(&s.store.providers);
        Ok(())
    }
    fn import_pair(
        client: ClientId,
        home: &Path,
        connection: &codex_api::Connection,
    ) -> Result<(String, String)> {
        if let (ClientId::Codex, codex_api::Connection::ApiKey { provider }) = (client, connection)
        {
            let (_, pair) = codex_api::read(home, provider)?;
            let base = pair
                .base_url
                .filter(|s| !s.is_empty())
                .ok_or_else(|| AppError::new(code::IMPORT, "配置缺少供应商地址"))?;
            let token = pair
                .token
                .filter(|s| !s.is_empty())
                .ok_or_else(|| AppError::new(code::IMPORT, "auth.json 缺少 API Key"))?;
            if token.starts_with("gs_") && client::loopback_address(&base) {
                return Err(AppError::new(code::MANAGED, "不能导入网关的本地凭据"));
            }
            Ok((base, token))
        } else {
            takeover::import_for(client, home)
        }
    }
    pub fn client_id(&self) -> ClientId {
        self.0.client
    }
    pub fn reveal_provider_token(
        &self,
        window_label: &str,
        provider_id: &str,
        expected_revision: &str,
    ) -> Result<String> {
        if window_label != "main" {
            return Err(AppError::new(code::FORBIDDEN, "仅主窗口可查看供应商凭据"));
        }
        let state = self.0.inner.lock().unwrap();
        if expected_revision != self.view_revision(&state)?
            || storage::revision(
                storage::read_optional(&self.0.data.join("gateway.json"))?.as_deref(),
            ) != state.revision
        {
            return Err(AppError::new(
                code::CONFLICT,
                "供应商配置已变化，请重新打开编辑",
            ));
        }
        state
            .store
            .providers
            .iter()
            .find(|provider| provider.id == provider_id)
            .map(|provider| provider.token.clone())
            .ok_or_else(|| AppError::new(code::PROVIDER, "供应商不存在"))
    }
    fn view_revision(&self, s: &Inner) -> Result<String> {
        Ok(self.view_revision_with_listener(s, &self.try_listener_view()?.revision))
    }
    fn view_revision_with_listener(&self, s: &Inner, listener_revision: &str) -> String {
        // File revisions also include resume checkpoints, which change after successful
        // auto-routing. Only configuration/lifecycle changes invalidate an editor.
        // Keep s.revision separate for the on-disk transaction checks below.
        let editable = (
            self.0.client,
            (&s.store.providers, &s.store.quota_access_tokens),
            &s.store.provider_endpoints,
            &s.store.grok_connections,
            &s.store.dsh_models,
            &s.store.grok_api_model,
            &s.store.connection,
            &s.store.settings,
            &s.store.mode,
            &s.store.selected,
            &s.store.local_token,
            s.running,
            &s.home,
            s.upgrade_error.is_some(),
            // Hash the listener revision separately below (serde tuples are bounded).
        );
        storage::digest(
            &serde_json::to_vec(&(editable, listener_revision))
                .expect("serializable gateway configuration"),
        )
    }
    pub fn subscribe(&self) -> broadcast::Receiver<()> {
        self.0.events.subscribe()
    }
    pub fn has_health_wait(&self) -> bool {
        let state = self.0.inner.lock().unwrap();
        state.circuits.values().any(|c| c.health().retry_in > 0)
            || state
                .model_circuits
                .values()
                .any(|models| models.circuits().iter().any(|c| c.health().retry_in > 0))
    }
    fn changed(&self) {
        let _ = self.0.events.send(());
    }
    pub fn try_listener_view(&self) -> Result<ListenerView> {
        self.0.registry.listener.view()
    }
    pub fn set_listener_port(&self, port: u16, expected: &str) -> Result<ListenerView> {
        let _lifecycle = self
            .0
            .registry
            .lifecycle
            .try_lock()
            .map_err(|_| AppError::new(code::GATEWAY_ACTIVE, "网关正在启停，请稍后再试"))?;
        let _mutation = self.0.registry.mutation.lock().unwrap();
        for peer in self.0.registry.gateways() {
            let state = peer.inner.lock().unwrap();
            for provider in &state.store.providers {
                for url in Self::endpoint_urls(&state.store, provider) {
                    model::base_url(&url, port)?;
                }
            }
        }
        self.0.registry.listener.set_port(port, expected)?;
        self.0.registry.update_ports()?;
        self.0.registry.changed();
        self.try_listener_view()
    }
    #[cfg(test)]
    pub fn view(&self) -> View {
        self.try_view().expect("healthy test gateway")
    }
    #[cfg(test)]
    pub fn listener_view(&self) -> ListenerView {
        self.try_listener_view().expect("healthy test listener")
    }
    pub fn try_view(&self) -> Result<View> {
        let listener = self.0.registry.listener.view()?;
        let s = self.request_state()?;
        let (occupied, waiting) = self.0.admission.counts();
        let health = |key: String| s.circuits.get(&key).cloned().unwrap_or_default().health();
        let config = s
            .home
            .as_ref()
            .filter(|_| self.0.client != ClientId::Dsh)
            .map(|h| takeover::read_connection(self.0.client, h, &s.store.connection));
        let config_revision = config
            .as_ref()
            .and_then(|r| r.as_ref().ok())
            .map(|(r, _)| r.clone());
        let ports = [listener.port, s.store.settings.port];
        let mut current = match (self.0.client, s.home.as_ref()) {
            (ClientId::Grok, Some(home)) => {
                current::grok(home, &s.store, &ports, &|name| std::env::var(name).ok())
            }
            (ClientId::Dsh, Some(home)) => {
                dsh::current(home, &s.store, &ports, &|name| std::env::var(name).ok())
            }
            _ => match &config {
                Some(Ok((_, pair))) => current::pair(self.0.client, pair, &s.store, &ports),
                Some(Err(e)) => current::Current::unknown(&e.message),
                None => current::Current::unknown("尚未找到客户端配置"),
            },
        };
        if self.0.client == ClientId::Claude {
            current.warning = s.home.as_ref().and_then(|h| claude_config::warning(h));
        }
        let (config_state, config_provider) = current.legacy();
        Ok(View {
            listener: listener.clone(),
            grok_managed: s.store.grok_api_model.is_some(),
            grok_models: if self.0.client == ClientId::Grok {
                s.home
                    .as_ref()
                    .and_then(|home| grok_layers::read(home, &|name| std::env::var(name).ok()).ok())
                    .and_then(|effective| {
                        grok_import::discover(&effective.text, &|name| {
                            effective.resolve(name, &|name| std::env::var(name).ok())
                        })
                        .ok()
                    })
                    .unwrap_or_default()
            } else {
                Vec::new()
            },
            dsh_profile: s.store.dsh.as_ref().map(|d| d.options.clone()),
            dsh_profiles: if self.0.client == ClientId::Dsh {
                s.home
                    .as_ref()
                    .map(|home| {
                        dsh::profile_states(home, &s.store, &ports, &|name| {
                            std::env::var(name).ok()
                        })
                    })
                    .unwrap_or_default()
            } else {
                vec![]
            },
            client_id: self.0.client,
            config_revision,
            config_provider,
            config_state: config_state.into(),
            config_error: match &current.state {
                current::Kind::Unknown { reason } => Some(reason.clone()),
                _ => None,
            },
            connection_mode: match &s.store.connection {
                codex_api::Connection::Bearer => "bearer",
                _ => "apiKey",
            }
            .into(),
            config_warning: current.warning.clone(),
            current,
            last_successful: s.last_successful.clone(),
            revision: self.view_revision(&s)?,
            running: s.running,
            address: if self.0.client == ClientId::Dsh
                && s.store
                    .dsh
                    .as_ref()
                    .is_some_and(|d| d.options.protocol == grok_config::Protocol::Messages)
            {
                format!("http://127.0.0.1:{}", listener.port)
            } else {
                self.0.client.address(listener.port)
            },
            mode: s.store.mode.clone(),
            selected: s.store.selected.clone(),
            settings: Settings {
                port: listener.port,
                ..s.store.settings.clone()
            },
            providers: s
                .store
                .providers
                .iter()
                .map(|p| {
                    let (rpm_used, rpm_retry_in, rpm_limited, rpm_ledger_error) =
                        self.0.admission.rpm_status(&p.id, p.max_rpm);
                    let (quota_checking, quota_check) = self
                        .0
                        .quota
                        .probe_view(&p.id, &Self::quota_version(&s.store, p));
                    ProviderView {
                        protocol: (matches!(self.0.client, ClientId::Grok | ClientId::Dsh)).then(
                            || {
                                s.store
                                    .grok_connections
                                    .get(&p.id)
                                    .map(|c| c.protocol)
                                    .unwrap_or_default()
                            },
                        ),
                        id: p.id.clone(),
                        has_quota_access_token: s.store.quota_access_tokens.contains_key(&p.id),
                        name: p.name.clone(),
                        base_url: p.base_url.clone(),
                        extra_base_urls: s
                            .store
                            .provider_endpoints
                            .get(&p.id)
                            .cloned()
                            .unwrap_or_default(),
                        endpoint_measurements: self
                            .0
                            .endpoints
                            .measurements(p, &Self::endpoint_urls(&s.store, p)),
                        preferred_base_url: self
                            .0
                            .endpoints
                            .preferred(p, &Self::endpoint_urls(&s.store, p)),
                        queued: p.queued,
                        health: health(pkey(p)),
                        model_health: s
                            .model_circuits
                            .get(&pkey(p))
                            .map(|m| m.states())
                            .unwrap_or_default(),
                        quota_version: Self::quota_version(&s.store, p),
                        max_concurrency: p.max_concurrency,
                        max_rpm: p.max_rpm,
                        allowed_models: p.allowed_models.clone(),
                        dsh_models: (self.0.client == ClientId::Dsh)
                            .then(|| s.store.dsh_models.get(&p.id).cloned().unwrap_or_default()),
                        handoff_after_compaction: p.handoff_after_compaction,
                        take_new_threads: p.take_new_threads,
                        search_only: p.search_only,
                        active_requests: occupied.get(&p.id).copied().unwrap_or(0),
                        reserved_threads: if s.store.mode == "auto" {
                            self.0.admission.reserved(&p.id)
                        } else {
                            0
                        },
                        quota_checking,
                        quota_check,
                        rpm_used,
                        rpm_retry_in,
                        rpm_limited,
                        rpm_wait_reason: self.0.admission.rpm_wait_reason(&p.id, p.max_rpm),
                        rpm_ledger_error,
                        quota: self
                            .0
                            .quota
                            .cached(&p.id, &Self::quota_version(&s.store, p)),
                    }
                })
                .collect(),
            active_connections: self.0.active.load(Ordering::Relaxed),
            waiting_requests: waiting,
            capacity_retries: self.0.admission.capacity_retries(),
            websocket_retries: self.0.admission.websocket_retries(),
            transient_retries: self.0.admission.transient_retries(),
            handoff_waiting: if self.0.client == ClientId::Codex && s.store.mode == "auto" {
                self.0.compaction.waiting_policy(
                    &s.store
                        .providers
                        .iter()
                        .filter(|p| p.queued)
                        .cloned()
                        .collect::<Vec<_>>(),
                    &self.0.admission,
                )
            } else {
                vec![]
            },
            error: s
                .error
                .clone()
                .or_else(|| self.0.compaction.error())
                .or_else(|| {
                    self.0
                        .admission
                        .retention_error()
                        .then(|| "线程名额记录不可用，请检查私有数据目录".into())
                }),
            recovery_pending: s.upgrade_error.is_some()
                || self.0.data.join("gateway-recovery.json").exists()
                || self.0.data.join("grok-import.json").exists()
                || self.0.data.join("dsh-recovery.json").exists(),
        })
    }
    pub fn observe_home(&self, home: &Path) {
        let mut state = self.0.inner.lock().unwrap();
        if state
            .store
            .successful_codex_identity
            .as_ref()
            .is_some_and(|identity| !identity.matches_home(Some(home)))
        {
            let mut next = state.store.clone();
            next.successful_codex_identity = None;
            match next.persist(&self.0.data.join("gateway.json"), &state.revision) {
                Ok(revision) => {
                    state.store = next;
                    state.revision = revision;
                }
                Err(_) => {
                    state.error = Some("客户端身份失效状态保存失败，请重试切换 Codex 目录".into());
                }
            }
        }
        state.home = Some(home.to_owned());
    }
    pub fn read_config(&self, home: &Path) -> Result<crate::configuration::Document> {
        let s = self.0.inner.lock().unwrap();
        if self.0.client == ClientId::Dsh {
            return Err(AppError::new(code::CONFIG, "请在 DSH 接入面板选择 profile"));
        }
        crate::configuration::read(
            self.0.client,
            home,
            &self.0.data,
            s.running
                || self.0.data.join("gateway-recovery.json").exists()
                || self.0.data.join("grok-import.json").exists()
                || self.0.data.join("dsh-recovery.json").exists()
                || self.claude_official(),
        )
    }
    pub fn previous_config(&self) -> Result<String> {
        crate::configuration::previous(self.0.client, &self.0.data)
    }
    pub fn save_config(
        &self,
        home: &Path,
        text: &str,
        expected: &str,
    ) -> Result<crate::configuration::Document> {
        let s = self.0.inner.lock().unwrap();
        if self.0.data.join("grok-import.json").exists() {
            return Err(AppError::new(
                code::RECOVERY,
                "请先处理未完成的 Grok 导入事务",
            ));
        }
        if self.0.client == ClientId::Dsh {
            return Err(AppError::new(code::CONFIG, "请使用 DSH 接入面板"));
        }
        crate::configuration::validate(self.0.client, text)?;
        if self.0.client == ClientId::Claude {
            if let Some(data) = self.0.data.parent() {
                crate::claude_profile::guard_save(
                    data,
                    home,
                    &crate::claude_profile::user_home()?,
                    text,
                )?;
            }
        }
        let guarded = s.running || self.0.data.join("gateway-recovery.json").exists();
        if guarded {
            let current = takeover::read_connection(self.0.client, home, &s.store.connection)?.1;
            let unchanged = if let (ClientId::Codex, codex_api::Connection::ApiKey { provider }) =
                (self.0.client, &s.store.connection)
            {
                codex_api::base(text, provider)? == current.base_url
            } else if self.0.client == ClientId::Grok {
                takeover::grok_unchanged(&self.0.data, text)?
            } else {
                current == takeover::pair_for(self.0.client, text)?
            };
            if !unchanged {
                return Err(AppError::new(
                    code::GATEWAY_ACTIVE,
                    "请先停用网关并处理恢复事务，再修改连接地址或 Token",
                ));
            }
        }
        let result =
            crate::configuration::save(self.0.client, home, &self.0.data, text, expected, guarded)?;
        drop(s);
        self.observe_home(home);
        self.changed();
        Ok(result)
    }
    fn exit_provider<'a>(store: &'a Store, last: Option<&str>) -> Result<&'a Provider> {
        let p = if store.mode == "manual" {
            store
                .providers
                .iter()
                .find(|p| !p.search_only && Some(&p.id) == store.selected.as_ref())
        } else {
            last.and_then(|id| {
                store
                    .providers
                    .iter()
                    .find(|p| p.id == id && !p.search_only)
            })
            .or_else(|| store.providers.iter().find(|p| p.queued && !p.search_only))
        };
        p.ok_or_else(|| {
            AppError::new(
                code::PROVIDER,
                "没有关闭网关时可用的供应商，请选择供应商或加入故障转移队列",
            )
        })
    }
    pub fn guarded_home(&self) -> bool {
        self.try_view().is_err()
            || self.request_state().is_ok_and(|state| state.running)
            || self.0.data.join("gateway-recovery.json").exists()
            || self.0.data.join("grok-import.json").exists()
            || self.0.data.join("dsh-recovery.json").exists()
    }
    pub fn edit(&self, edit: Edit, expected: &str, home: &Path) -> Result<View> {
        self.edit_checked(edit, expected, home, None)
    }
    pub fn edit_checked(
        &self,
        edit: Edit,
        expected: &str,
        home: &Path,
        config_revision: Option<&str>,
    ) -> Result<View> {
        // Compatibility for callers of the pre-shared Settings API. The listener
        // file remains authoritative, including when the later client save fails.
        let port_change = match &edit {
            Edit::Settings { settings } if settings.port != self.try_listener_view()?.port => {
                Some(settings.port)
            }
            _ => None,
        };
        let _port_lifecycle =
            if port_change.is_some() {
                Some(
                    self.0.registry.lifecycle.try_lock().map_err(|_| {
                        AppError::new(code::GATEWAY_ACTIVE, "网关正在启停，请稍后再试")
                    })?,
                )
            } else {
                None
            };
        let _network_guard = self.0.registry.mutation.lock().unwrap();
        let mut s = self.0.inner.lock().unwrap();
        if expected != self.view_revision(&s)? {
            return Err(AppError::new(
                code::CONFLICT,
                "网关设置已变化，请使用最新状态重试",
            ));
        }
        if self.0.data.join("grok-import.json").exists() {
            return Err(AppError::new(
                code::RECOVERY,
                "请先处理未完成的 Grok 导入事务",
            ));
        }
        if self.0.client != ClientId::Codex
            && matches!(
                &edit,
                Edit::TakeoverProvider { .. }
                    | Edit::SaveProvider {
                        search_only: Some(_),
                        ..
                    }
            )
        {
            return Err(AppError::new(code::SETTINGS, "接管策略仅适用于 Codex"));
        }
        if self.0.client != ClientId::Codex
            && matches!(
                &edit,
                Edit::PolicyProvider {
                    handoff_after_compaction: Some(true),
                    ..
                } | Edit::PolicyProvider {
                    take_new_threads: Some(true),
                    ..
                }
            )
        {
            return Err(AppError::new(code::SETTINGS, "接管策略仅适用于 Codex"));
        }
        if self.0.client != ClientId::Grok && matches!(&edit, Edit::SaveGrokProvider { .. }) {
            return Err(AppError::new(code::CLIENT, "此操作仅适用于 Grok Build"));
        }
        if self.0.client != ClientId::Dsh
            && matches!(
                &edit,
                Edit::SaveDshProvider { .. } | Edit::DshModelsProvider { .. }
            )
        {
            return Err(AppError::new(code::CLIENT, "此操作仅适用于 DSH"));
        }
        if self.0.client == ClientId::Dsh
            && matches!(
                &edit,
                Edit::SaveProvider { .. }
                    | Edit::Import
                    | Edit::ImportProviders { .. }
                    | Edit::ImportLink { .. }
            )
        {
            return Err(AppError::new(
                code::CLIENT,
                "请在 DSH 供应商表单添加 Chat Completions 连接",
            ));
        }
        if let Edit::Reset { id } = &edit {
            if !s.store.providers.iter().any(|p| &p.id == id) {
                return Err(AppError::new(code::PROVIDER, "供应商不存在"));
            }
            let prefix = format!("provider:{id}:");
            let circuits: Vec<_> = s
                .circuits
                .iter()
                .filter(|(key, _)| key.starts_with(&prefix))
                .map(|(_, circuit)| circuit.clone())
                .chain(
                    s.model_circuits
                        .iter()
                        .filter(|(key, _)| key.starts_with(&prefix))
                        .flat_map(|(_, models)| models.circuits()),
                )
                .collect();
            self.0.admission.reset_provider(id, &circuits);
            drop(s);
            self.changed();
            return self.try_view();
        }
        if !s.running && self.0.data.join("gateway-recovery.json").exists() {
            return Err(AppError::new(code::RECOVERY, "请先处理未完成的配置事务"));
        }
        if let Some(error) = &s.upgrade_error {
            return Err(error.clone());
        }
        let direct_select = !s.running && matches!(&edit, Edit::Select { .. });
        if direct_select && self.claude_official() {
            return Err(AppError::new(
                code::CLAUDE_OFFICIAL,
                "请先切换到 Claude API 配置",
            ));
        }
        if direct_select
            && self.0.client == ClientId::Codex
            && crate::official::blocks_gateway(&self.0.data, home)
        {
            return Err(AppError::new(
                code::OFFICIAL_MODE,
                "官方账号连接期间不能切换网关供应商",
            ));
        }
        let sync_dsh_models = self.0.client == ClientId::Dsh
            && matches!(
                &edit,
                Edit::DshModelsProvider { .. } | Edit::Settings { .. }
            );
        let mut next = s.store.clone();
        if direct_select && self.0.client == ClientId::Grok {
            next.grok_api_model = Some(grok_config::MANAGED.into());
        }
        next.settings.port = self.try_listener_view()?.port;
        match edit {
            Edit::Connection { mode } => {
                if self.0.client != ClientId::Codex || s.running {
                    return Err(AppError::new(
                        code::GATEWAY_ACTIVE,
                        "请先关闭 Codex 网关再修改连接方式",
                    ));
                }
                if crate::official::blocks_gateway(&self.0.data, home) {
                    return Err(AppError::new(code::OFFICIAL_MODE, "请先关闭官方账号连接"));
                }
                next.connection = match mode.as_str() {
                    "auto" => codex_api::detect(home)?,
                    "apiKey" => codex_api::binding(home)?,
                    "bearer" => {
                        takeover::read_for(ClientId::Codex, home)?;
                        codex_api::Connection::Bearer
                    }
                    _ => return Err(AppError::new(code::SETTINGS, "连接方式无效")),
                };
            }
            Edit::Import => {
                if self.0.client == ClientId::Grok {
                    return Err(AppError::new(
                        code::GROK_CONFIG,
                        "请先预览 Grok 配置，再确认导入",
                    ));
                }
                let (base_url, token) = Self::import_pair(self.0.client, home, &next.connection)?;
                next.edit(
                    Edit::SaveProvider {
                        search_only: None,
                        quota_access_token: None,
                        id: None,
                        base_url,
                        token,
                        name: None,
                    },
                    s.running,
                )?;
                if self.0.client == ClientId::Grok {
                    if let Some(provider) = next.providers.last() {
                        next.grok_connections.insert(
                            provider.id.clone(),
                            grok_config::profile(&grok_config::read(home)?.1)?.connection,
                        );
                    }
                }
            }
            edit => next.edit(edit, s.running)?,
        }
        if self.0.client == ClientId::Grok && s.running {
            let active_protocol = grok_config::api_profile(&grok_config::read(home)?.1)?
                .connection
                .protocol;
            let selected_protocol =
                Self::exit_provider(&next, s.last_successful.as_deref()).map(|p| {
                    next.grok_connections
                        .get(&p.id)
                        .map(|c| c.protocol)
                        .unwrap_or_default()
                })?;
            if selected_protocol != active_protocol {
                return Err(AppError::new(
                    code::GROK_PROTOCOL,
                    "请先停用 Grok 网关，再切换 API 协议",
                ));
            }
        }
        let mut ports = self.0.registry.ports.lock().unwrap().clone();
        ports.push(next.settings.port);
        for provider in &next.providers {
            for port in &ports {
                for url in Self::endpoint_urls(&next, provider) {
                    model::base_url(&url, *port)?;
                }
            }
        }
        // A port change must not turn another client's existing upstream into a loop.
        if next.settings.port != s.store.settings.port {
            for peer in self.0.registry.gateways() {
                if !Arc::ptr_eq(&peer, &self.0) {
                    let peer = peer.inner.lock().unwrap();
                    for p in &peer.store.providers {
                        for url in Self::endpoint_urls(&peer.store, p) {
                            model::base_url(&url, next.settings.port)?;
                        }
                    }
                }
            }
        }
        if let Some(port) = port_change {
            self.0
                .registry
                .listener
                .set_port(port, &self.try_listener_view()?.revision)?;
        }
        let revision = if self.0.client != ClientId::Dsh && (s.running || direct_select) {
            let p = Self::exit_provider(&next, s.last_successful.as_deref())?;
            let target = takeover::Pair::new(&p.base_url, &p.token);
            let resume_target = if self.0.client == ClientId::Grok && s.running {
                takeover::exit_pair(&self.0.data)?
                    .ok_or_else(|| AppError::new(code::RECOVERY, "缺少 Grok 恢复目标"))?
            } else {
                target.clone()
            };
            next.resume = Some(model::Resume {
                home: home.to_owned(),
                desired: s.running,
                pair_hash: resume_target.fingerprint(),
            });
            let path = self.0.data.join("gateway.json");
            let old = storage::read_optional(&path)?;
            if storage::revision(old.as_deref()) != s.revision {
                return Err(AppError::new(code::CONFLICT, "网关存储已被外部修改"));
            }
            let after = serde_json::to_string_pretty(&next)
                .map_err(|_| AppError::new(code::STORE, "无法写入网关设置"))?;
            takeover::commit_store_for(
                self.0.client,
                &self.0.data,
                home,
                old.map(|v| String::from_utf8(v).unwrap()),
                after.clone(),
                target,
                s.running,
                config_revision,
            )?;
            storage::digest(after.as_bytes())
        } else if sync_dsh_models {
            dsh::commit_mapping(&self.0.data, home, &mut next, &s.revision)?
        } else {
            next.persist(&self.0.data.join("gateway.json"), &s.revision)?
        };
        if s.last_successful
            .as_ref()
            .is_some_and(|id| !next.providers.iter().any(|p| &p.id == id))
        {
            s.last_successful = None;
        }
        self.replace_store(&mut s, next);
        let keys: std::collections::HashSet<_> = s.store.providers.iter().map(pkey).collect();
        s.circuits.retain(|key, _| keys.contains(key));
        s.model_circuits.retain(|key, _| keys.contains(key));
        s.revision = revision;
        s.home = Some(home.to_owned());
        self.sync_single_protection(&s);
        self.0.admission.configure(&s.store.providers, s.running);
        self.0
            .admission
            .configure_retention(s.store.settings.thread_idle_release_seconds);
        self.0.compaction.provider_names(&s.store.providers);
        self.0.endpoints.retain(&s.store.providers);
        self.0.quota.retain(
            &s.store
                .providers
                .iter()
                .map(|p| (p.id.clone(), Self::quota_version(&s.store, p)))
                .collect(),
        );
        drop(s);
        self.0.registry.update_ports()?;
        self.0.clients.lock().unwrap().clear();
        self.0.registry.changed();
        self.try_view()
    }
    pub async fn start(&self, expected: &str, home: &Path) -> Result<View> {
        self.start_checked(expected, home, None).await
    }
    pub async fn start_checked(
        &self,
        expected: &str,
        home: &Path,
        config_revision: Option<&str>,
    ) -> Result<View> {
        if self.0.client == ClientId::Dsh {
            return self.restart_dsh(home, expected).await;
        }
        if self.0.client == ClientId::Codex && crate::official::blocks_gateway(&self.0.data, home) {
            return Err(AppError::new(
                code::OFFICIAL_MODE,
                "请先关闭官方账号连接，再启动 Codex 网关",
            ));
        }
        if self.claude_official() {
            return Err(AppError::new(
                code::CLAUDE_OFFICIAL,
                "请先切换到 Claude API 配置，再启用网关",
            ));
        }
        let _listener_guard = self.0.registry.lifecycle.lock().await;
        let _guard = self.0.lifecycle.lock().await;
        let token = {
            let s = self.0.inner.lock().unwrap();
            if expected != self.view_revision(&s)? {
                return Err(AppError::new(code::CONFLICT, "网关设置已变化，请重试"));
            }
            if s.running {
                drop(s);
                return self.try_view();
            }
            if let Some(error) = &s.upgrade_error {
                return Err(error.clone());
            }
            if self.0.data.join("grok-import.json").exists() {
                return Err(AppError::new(
                    code::RECOVERY,
                    "请先处理未完成的 Grok 导入事务",
                ));
            }
            if s.store.providers.is_empty() {
                return Err(AppError::new(code::PROVIDER, "请先添加供应商"));
            }
            s.store.local_token.clone()
        };
        let listener = self.0.registry.listener.prepare().await?;
        self.0.registry.update_ports()?;
        if listener.revision != listener.previous_revision {
            self.0.registry.changed();
        }
        self.0.registry.listener.check_revision()?;
        let token = self.0.registry.unique_local_token(self.0.client, &token);
        {
            let s = self.0.inner.lock().unwrap();
            if self.view_revision_with_listener(&s, &listener.previous_revision) != expected {
                return Err(AppError::new(
                    code::CONFLICT,
                    "绑定端口期间设置已变化，请重试",
                ));
            }
        }
        let (tx, _rx) = watch::channel(false);
        {
            let mut s = self.0.inner.lock().unwrap();
            if self.view_revision_with_listener(&s, &listener.previous_revision) != expected {
                return Err(AppError::new(code::CONFLICT, "设置已变化，请重试"));
            }
            let p = Self::exit_provider(&s.store, None)?;
            let exit = takeover::Pair::new(&p.base_url, &p.token);
            let (current, original_pair) =
                takeover::read_connection(self.0.client, home, &s.store.connection)?;
            if self.0.client == ClientId::Grok
                && original_pair.is_gateway(
                    self.0.client,
                    &s.store.local_token,
                    &[listener.port, s.store.settings.port],
                )
            {
                return Err(grok_config::missing_recovery(&grok_config::read(home)?.1));
            }
            let mut next = s.store.clone();
            next.local_token.clone_from(&token);
            for provider in &next.providers {
                for url in Self::endpoint_urls(&next, provider) {
                    model::base_url(&url, listener.port)?;
                }
            }
            if self.0.client == ClientId::Grok {
                next.grok_resume = Some(grok_config::fingerprint(&grok_config::read(home)?.1)?);
            }
            next.resume = Some(model::Resume {
                home: home.to_owned(),
                desired: true,
                pair_hash: if self.0.client == ClientId::Grok {
                    original_pair.fingerprint()
                } else {
                    exit.fingerprint()
                },
            });
            let before = storage::read_optional(&self.0.data.join("gateway.json"))?;
            if storage::revision(before.as_deref()) != s.revision {
                return Err(AppError::new(code::CONFLICT, "网关存储已被外部修改"));
            }
            let after = serde_json::to_string_pretty(&next)
                .map_err(|_| AppError::new(code::STORE, "无法生成网关启动事务"))?;
            self.0.registry.listener.activate_with(
                self.0.client,
                listener.socket,
                self.0.registry.clone(),
                || {
                    takeover::attach_store_for(
                        self.0.client,
                        &self.0.data,
                        home,
                        listener.port,
                        &token,
                        exit,
                        config_revision.unwrap_or(&current),
                        Some((before.map(|b| String::from_utf8(b).unwrap()), after.clone())),
                    )?;
                    self.replace_store(&mut s, next);
                    s.revision = storage::digest(after.as_bytes());
                    s.home = Some(home.to_owned());
                    s.last_successful = None;
                    s.running = true;
                    s.shutdown = Some(tx);
                    s.error = listener.notice;
                    self.0.admission.configure(&s.store.providers, true);
                    self.0
                        .admission
                        .configure_retention(s.store.settings.thread_idle_release_seconds);
                    self.0.compaction.provider_names(&s.store.providers);
                    Ok(())
                },
            )?;
        }
        self.0.registry.changed();
        self.try_view()
    }
    pub async fn stop(&self) -> Result<View> {
        self.stop_checked(None).await
    }
    pub async fn stop_checked(&self, expected_config: Option<&str>) -> Result<View> {
        self.stop_internal(expected_config, false).await
    }
    pub async fn stop_for_exit(&self) -> Result<View> {
        self.stop_internal(None, true).await
    }
    pub async fn resume(&self, home: &Path) -> Result<()> {
        if self.claude_official() {
            return Ok(());
        }
        if self.0.inner.lock().unwrap().running {
            return Ok(());
        }
        let (revision, intent, connection) = {
            let s = self.0.inner.lock().unwrap();
            if let Some(error) = &s.upgrade_error {
                return Err(error.clone());
            }
            (
                self.view_revision(&s)?,
                s.store.resume.clone(),
                s.store.connection.clone(),
            )
        };
        let Some(intent) = intent.filter(|i| i.desired) else {
            return Ok(());
        };
        if self.0.client == ClientId::Dsh {
            if intent.home != home {
                return Err(AppError::new(
                    code::RESUME_CONFLICT,
                    "DSH 目录已变化，自动恢复已停止",
                ));
            }
            return self.start(&revision, home).await.map(|_| ());
        }
        let result = async {
            if intent.home != home {
                return Err(AppError::new(
                    code::RESUME_CONFLICT,
                    "客户端目录已变化，自动恢复已停止",
                ));
            }
            let (config_revision, pair) =
                takeover::read_connection(self.0.client, home, &connection)?;
            if self.0.client == ClientId::Grok {
                let expected = self.0.inner.lock().unwrap().store.grok_resume.clone();
                if expected.as_deref()
                    != Some(&grok_config::fingerprint(&grok_config::read(home)?.1)?)
                {
                    return Err(AppError::new(
                        code::RESUME_CONFLICT,
                        "Grok 原生模型或连接已变化，自动恢复已停止",
                    ));
                }
            }
            let known_legacy = {
                let state = self.0.inner.lock().unwrap();
                self.0.client != ClientId::Grok
                    && pair.is_gateway(
                        self.0.client,
                        &state.store.local_token,
                        &[self.try_listener_view()?.port, state.store.settings.port],
                    )
                    && state.store.providers.iter().any(|provider| {
                        takeover::Pair::new(&provider.base_url, &provider.token).fingerprint()
                            == intent.pair_hash
                    })
            };
            if pair.fingerprint() != intent.pair_hash && !known_legacy {
                return Err(AppError::new(
                    code::RESUME_CONFLICT,
                    "受管地址或 Token 在退出后已被修改，自动恢复已停止",
                ));
            }
            self.start_checked(&revision, home, Some(&config_revision))
                .await
                .map(|_| ())
        }
        .await;
        if let Err(error) = &result {
            self.0.inner.lock().unwrap().error = Some(error.message.clone());
            self.changed();
        }
        result
    }
    async fn stop_internal(
        &self,
        expected_config: Option<&str>,
        preserve_intent: bool,
    ) -> Result<View> {
        if self.0.client == ClientId::Dsh {
            return self.stop_dsh(preserve_intent).await;
        }
        let _listener_guard = self.0.registry.lifecycle.lock().await;
        let _guard = self.0.lifecycle.lock().await;
        if let Err(error) = self.0.registry.listener.view() {
            // A poisoned shared lifecycle cannot identify its remaining clients
            // reliably. Cancel actual client resources before recovering files;
            // a configuration conflict must not strand the listener or requests.
            for peer in self.0.registry.gateways() {
                if let Ok(mut state) = peer.inner.lock() {
                    if let Some(shutdown) = state.shutdown.take() {
                        let _ = shutdown.send(true);
                    }
                    state.running = false;
                    state.error = Some(error.message.clone());
                    peer.admission.configure(&state.store.providers, false);
                }
            }
            let _ = self.0.registry.listener.deactivate(self.0.client).await;
            self.0.registry.changed();
            crate::migrations::gateway(&self.0.data)?;
            grok_import::recover(&self.0.data)?;
            return Err(error);
        }
        {
            let mut s = self.0.inner.lock().unwrap();
            if let (Some(expected), Some(home)) = (expected_config, &s.home) {
                if takeover::read_connection(self.0.client, home, &s.store.connection)?.0
                    != expected
                {
                    return Err(AppError::new(code::CONFLICT, "配置已变化，请刷新后重试"));
                }
            }
            if !preserve_intent {
                let mut next = s.store.clone();
                if let Some(intent) = next.resume.as_mut().filter(|r| r.desired) {
                    intent.desired = false;
                    if s.running {
                        let home = s
                            .home
                            .as_ref()
                            .ok_or_else(|| AppError::new(code::STATE, "缺少运行目录"))?;
                        let exit = takeover::exit_pair(&self.0.data)?;
                        let target = match exit.as_ref() {
                            Some(pair) => pair.clone(),
                            None => {
                                let (_, pair) = takeover::read_connection(
                                    self.0.client,
                                    home,
                                    &s.store.connection,
                                )?;
                                if pair.fingerprint() != intent.pair_hash {
                                    return Err(AppError::new(
                                        code::RECOVERY,
                                        "缺少停止目标且配置未恢复",
                                    ));
                                }
                                pair
                            }
                        };
                        intent.pair_hash = target.fingerprint();
                        let before = storage::read_optional(&self.0.data.join("gateway.json"))?;
                        if storage::revision(before.as_deref()) != s.revision {
                            return Err(AppError::new(code::CONFLICT, "网关存储已被外部修改"));
                        }
                        let after = serde_json::to_string_pretty(&next)
                            .map_err(|_| AppError::new(code::STORE, "无法生成停止事务"))?;
                        if exit.is_some() {
                            takeover::commit_store_for(
                                self.0.client,
                                &self.0.data,
                                home,
                                before.map(|b| String::from_utf8(b).unwrap()),
                                after.clone(),
                                target,
                                true,
                                expected_config,
                            )?;
                        } else {
                            storage::atomic_write(
                                &self.0.data.join("gateway.json"),
                                after.as_bytes(),
                                Some(&s.revision),
                            )?;
                        }
                        s.revision = storage::digest(after.as_bytes());
                    } else {
                        s.revision =
                            next.persist(&self.0.data.join("gateway.json"), &s.revision)?;
                    }
                    self.replace_store(&mut s, next);
                }
            }
            if let Err(e) = crate::migrations::gateway(&self.0.data)
                .and_then(|()| grok_import::recover(&self.0.data))
            {
                s.error = Some(e.message.clone());
                drop(s);
                self.changed();
                return Err(e);
            }
            // Recovery may have completed a store/config transaction interrupted by an I/O failure.
            let (store, revision) = Store::load(&self.0.data.join("gateway.json"))?;
            self.replace_store(&mut s, store);
            s.revision = revision;
            if let Some(tx) = s.shutdown.take() {
                let _ = tx.send(true);
            }
            s.running = false;
            s.upgrade_error = None;
            self.0.admission.configure(&s.store.providers, false);
            self.0
                .admission
                .configure_retention(s.store.settings.thread_idle_release_seconds);
            self.0.compaction.provider_names(&s.store.providers);
            s.error = None;
        };
        let stopped = self.0.registry.listener.deactivate(self.0.client).await;
        self.0.clients.lock().unwrap().clear();
        self.0.registry.changed();
        stopped?;
        self.try_view()
    }
    async fn persist_success(&self, provider: &Provider) {
        let gateway = self.clone();
        let provider = provider.clone();
        // Recovery writes happen after response delivery, outside async socket
        // polling. Await this bounded per-request job so shutdown keeps its order.
        let _ = tokio::task::spawn_blocking(move || gateway.successful_response(&provider)).await;
    }
    fn successful_response(&self, provider: &Provider) {
        let Ok(mut s) = self.request_state() else {
            return;
        };
        if !s.running
            || !s
                .store
                .providers
                .iter()
                .any(|p| p.id == provider.id && p.version == provider.version)
        {
            return;
        }
        if s.store.mode == "auto" && !matches!(self.0.client, ClientId::Grok | ClientId::Dsh) {
            // Routes may use an auxiliary URL. Client configuration and recovery
            // always restore the canonical primary URL saved by the user.
            let canonical = s
                .store
                .providers
                .iter()
                .find(|p| p.id == provider.id)
                .unwrap();
            let target = takeover::Pair::new(&canonical.base_url, &canonical.token);
            let update = (|| -> Result<()> {
                if s.store
                    .resume
                    .as_ref()
                    .is_some_and(|r| r.pair_hash == target.fingerprint())
                {
                    return Ok(());
                }
                let home = s
                    .home
                    .as_ref()
                    .ok_or_else(|| AppError::new(code::STATE, "缺少运行目录"))?;
                let mut next = s.store.clone();
                next.resume = Some(model::Resume {
                    home: home.clone(),
                    desired: true,
                    pair_hash: target.fingerprint(),
                });
                let before = storage::read_optional(&self.0.data.join("gateway.json"))?;
                if storage::revision(before.as_deref()) != s.revision {
                    return Err(AppError::new(code::CONFLICT, "网关存储已被外部修改"));
                }
                let after = serde_json::to_string_pretty(&next)
                    .map_err(|_| AppError::new(code::STORE, "无法记录最近供应商"))?;
                takeover::commit_store_for(
                    self.0.client,
                    &self.0.data,
                    home,
                    before.map(|b| String::from_utf8(b).unwrap()),
                    after.clone(),
                    target,
                    true,
                    None,
                )?;
                self.replace_store(&mut s, next);
                s.revision = storage::digest(after.as_bytes());
                Ok(())
            })();
            if let Err(e) = update {
                s.error = Some(e.message);
                drop(s);
                self.changed();
                return;
            }
        }
        s.last_successful = Some(provider.id.clone());
        drop(s);
        self.changed();
    }
    fn quota_version(store: &Store, p: &Provider) -> String {
        storage::digest(
            &serde_json::to_vec(&(
                &p.version,
                &p.base_url,
                store.quota_access_tokens.get(&p.id),
            ))
            .expect("quota credentials serialize"),
        )
    }
    pub fn quota_events(&self) -> broadcast::Receiver<QuotaView> {
        self.0.quota.subscribe()
    }
    pub async fn query_quota(&self, id: &str, force: bool) -> Result<QuotaView> {
        let input = self.query_input(id)?;
        let version = input.version.clone();
        let result = self.0.quota.query(input, force).await?;
        let s = self.0.inner.lock().unwrap();
        if !s
            .store
            .providers
            .iter()
            .any(|p| p.id == id && Self::quota_version(&s.store, p) == version)
        {
            return Err(AppError::new(
                code::STALE,
                "供应商配置已变化，已丢弃旧额度结果",
            ));
        }
        Ok(result)
    }
    fn query_input(&self, id: &str) -> Result<quota::Query> {
        self.query_snapshot(id, true).map(|(query, _)| query)
    }
    fn query_snapshot(
        &self,
        id: &str,
        quota: bool,
    ) -> Result<(quota::Query, Option<grok_config::Connection>)> {
        Ok({
            let s = self.0.inner.lock().unwrap();
            let p = s
                .store
                .providers
                .iter()
                .find(|p| p.id == id)
                .ok_or_else(|| AppError::new(code::PROVIDER, "供应商不存在"))?;
            let connection =
                (matches!(self.client_id(), ClientId::Grok | ClientId::Dsh)).then(|| {
                    s.store
                        .grok_connections
                        .get(id)
                        .cloned()
                        .unwrap_or_default()
                });
            let query = quota::Query {
                client_id: self.0.client,
                id: p.id.clone(),
                version: if quota {
                    Self::quota_version(&s.store, p)
                } else {
                    p.version.clone()
                },
                base: p.base_url.clone(),
                token: p.token.clone(),
                quota_access_token: if quota {
                    s.store.quota_access_tokens.get(id).cloned()
                } else {
                    None
                },
                client: Client::builder(TokioExecutor::new())
                    .retry_canceled_requests(false)
                    .build(
                        Connector::new(Duration::from_secs(10), self.try_listener_view()?.port)
                            .with_ports(self.0.registry.ports.clone()),
                    ),
            };
            (query, connection)
        })
    }
    pub async fn list_models(&self, id: &str, force: bool) -> Result<catalog::View> {
        let (input, connection) = self.query_snapshot(id, false)?;
        let version = input.version.clone();
        let mut result = self
            .0
            .catalog
            .query_with_connection(input, force, connection.as_ref())
            .await?;
        let s = self.0.inner.lock().unwrap();
        if !s
            .store
            .providers
            .iter()
            .any(|p| p.id == id && p.version == version)
        {
            return Err(AppError::new(
                code::STALE,
                "供应商配置已变化，已丢弃旧模型列表",
            ));
        }
        if force {
            self.0.catalog.commit(&result)?;
        }
        if let Some(usage) = self.0.usage.lock().unwrap().as_ref() {
            if let Ok(prices) = usage.prices() {
                let (choices, _) = self.0.model_list.snapshot();
                result.metadata = result
                    .models
                    .iter()
                    .filter_map(|id| {
                        prices
                            .models_dev
                            .metadata_from(id, choices.source(id))
                            .map(|m| (id.clone(), m))
                    })
                    .collect();
            }
        }
        Ok(result)
    }
    pub(crate) fn model_union(
        &self,
        protocol: Option<grok_config::Protocol>,
    ) -> Result<Vec<String>> {
        if self.client_id() == ClientId::Dsh {
            return Ok(dsh::models::for_store(&self.0.inner.lock().unwrap().store)
                .iter()
                .filter_map(|m| m["id"].as_str().map(str::to_owned))
                .collect());
        }

        Ok(self
            .union_models(protocol)?
            .into_iter()
            .map(|m| m.id)
            .collect())
    }

    fn sync_single_protection(&self, state: &Inner) {
        let mut queue = state
            .store
            .providers
            .iter()
            .filter(|p| p.queued && !p.search_only);
        let first = queue.next();
        let single = if self.0.client == ClientId::Codex
            && state.store.mode == "auto"
            && queue.next().is_none()
        {
            first.map(|p| format!("provider:{}:", p.id))
        } else {
            None
        };
        for (key, circuit) in &state.circuits {
            circuit.set_single_provider_protection(
                single
                    .as_ref()
                    .filter(|prefix| key.starts_with(prefix.as_str()))
                    .map(|_| Duration::from_secs(state.store.settings.capacity_retry_seconds)),
            );
        }
        for (key, models) in &state.model_circuits {
            let protection = single
                .as_ref()
                .filter(|prefix| key.starts_with(prefix.as_str()))
                .map(|_| Duration::from_secs(state.store.settings.capacity_retry_seconds));
            for circuit in models.circuits() {
                circuit.set_single_provider_protection(protection);
            }
        }
    }
    // Poison may follow a partial config/affinity transaction. Request paths
    // must fail closed instead of reusing that state or panicking again.
    fn request_state(&self) -> Result<std::sync::MutexGuard<'_, Inner>> {
        self.0
            .inner
            .lock()
            .map_err(|_| AppError::new(code::STORE, "网关状态不可用，请重新打开应用"))
    }
    fn routing_ids(&self, pinned: Option<&str>) -> Vec<String> {
        let Ok(s) = self.request_state() else {
            return vec![];
        };
        s.store
            .providers
            .iter()
            .filter(|p| {
                !p.search_only
                    && match pinned {
                        Some(id) => p.id == id,
                        None if s.store.mode == "auto" => p.queued,
                        None => s.store.selected.as_deref() == Some(p.id.as_str()),
                    }
            })
            .map(|p| p.id.clone())
            .collect()
    }
    fn search_backups(&self) -> Vec<String> {
        if self.client_id() != ClientId::Codex {
            return vec![];
        }
        self.request_state()
            .map(|s| {
                s.store
                    .providers
                    .iter()
                    .filter(|p| p.queued && p.search_only)
                    .map(|p| p.id.clone())
                    .collect()
            })
            .unwrap_or_default()
    }
    #[cfg(test)]
    pub(crate) fn successful_user_agent(&self) -> Result<hyper::header::HeaderValue> {
        self.request_state()?
            .store
            .successful_user_agent
            .as_deref()
            .and_then(|raw| hyper::header::HeaderValue::from_bytes(raw).ok())
            .filter(|v| !v.is_empty())
            .ok_or_else(|| {
                AppError::new(
                    code::STATE,
                    "请先通过该客户端完成一次正常请求，再运行供应商测试",
                )
            })
    }
    fn remember_business_identity(&self, identity: testing::BusinessIdentity) {
        let raw = identity.user_agent.as_slice();
        if raw.is_empty() || raw.len() > 8192 {
            return;
        }
        let Ok(mut s) = self.request_state() else {
            return;
        };
        let codex = identity
            .codex
            .filter(|identity| identity.matches_home(s.home.as_deref()));
        if s.store.successful_user_agent.as_deref() == Some(raw)
            && codex
                .as_ref()
                .is_none_or(|identity| s.store.successful_codex_identity.as_ref() == Some(identity))
        {
            return;
        }
        let mut next = s.store.clone();
        next.successful_user_agent = Some(raw.to_vec());
        if let Some(codex) = codex {
            next.successful_codex_identity = Some(codex);
        }
        match next.persist(&self.0.data.join("gateway.json"), &s.revision) {
            Ok(revision) => {
                s.store = next;
                s.revision = revision;
            }
            Err(_) => {
                s.error = Some("最近成功请求的客户端身份保存失败，请重试正常请求".into());
            }
        }
    }
    fn route(&self, id: &str) -> Option<Route> {
        let mut s = self.request_state().ok()?;
        let mut provider = s.store.providers.iter().find(|p| p.id == id)?.clone();
        let dsh_models = (self.0.client == ClientId::Dsh)
            .then(|| s.store.dsh_models.get(id).cloned().unwrap_or_default());
        if let Some(mapping) = &dsh_models {
            provider.allowed_models = Some(mapping.allowed());
        }
        let provider_circuit = s.circuits.entry(pkey(&provider)).or_default().clone();
        let model_circuits = s.model_circuits.entry(pkey(&provider)).or_default().clone();
        self.sync_single_protection(&s);
        let key = format!("{}:{}", pkey(&provider), s.store.settings.connect_seconds);
        let mut clients = locks::cache(&self.0.clients);
        let port = self.0.registry.listener.request_port().ok()?;
        let client = clients
            .entry(key)
            .or_insert_with(|| {
                Client::builder(TokioExecutor::new())
                    .pool_idle_timeout(Duration::from_secs(60))
                    .pool_max_idle_per_host(8)
                    .retry_canceled_requests(false)
                    .build(
                        Connector::new(Duration::from_secs(s.store.settings.connect_seconds), port)
                            .with_ports(self.0.registry.ports.clone()),
                    )
            })
            .clone();
        Some(Route {
            dsh_models,
            grok: (matches!(self.0.client, ClientId::Grok | ClientId::Dsh)).then(|| {
                s.store
                    .grok_connections
                    .get(&provider.id)
                    .cloned()
                    .unwrap_or_default()
            }),
            client_id: self.0.client,
            provider,
            client,
            provider_circuit,
            model_circuits,
        })
    }
    #[cfg(test)]
    fn remember(&self, id: &str, provider: &str) {
        self.remember_model(id, provider, None);
    }
    fn remember_model(&self, id: &str, provider: &str, model: Option<&str>) {
        if id.is_empty() || id.len() > 1024 {
            return;
        }
        let Ok(mut s) = self.request_state() else {
            return;
        };
        s.affinity
            .retain(|_, (_, _, at)| at.elapsed() < Duration::from_secs(3600));
        if s.affinity.len() >= 4096 {
            if let Some(old) = s
                .affinity
                .iter()
                .min_by_key(|(_, (_, _, at))| *at)
                .map(|(id, _)| id.clone())
            {
                s.affinity.remove(&old);
            }
        }
        s.affinity.insert(
            id.into(),
            (provider.into(), model.map(str::to_owned), Instant::now()),
        );
    }
}
struct Active(Gateway);
impl Active {
    fn new(g: Gateway) -> Self {
        g.0.active.fetch_add(1, Ordering::Relaxed);
        g.changed();
        Self(g)
    }
}
impl Drop for Active {
    fn drop(&mut self) {
        self.0 .0.active.fetch_sub(1, Ordering::Relaxed);
        self.0.changed();
    }
}

#[cfg(test)]
mod usage_protocol_tests;

#[cfg(test)]
mod usage_forward_tests;

#[cfg(test)]
mod transport_delivery_tests;

#[cfg(test)]
mod failover_evidence_tests;
#[cfg(test)]
mod failure_fallback_tests;
#[cfg(test)]
mod grok_protocol_tests;

#[cfg(test)]
mod request_state_poison_tests;
