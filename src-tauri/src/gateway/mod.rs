#[cfg(test)]
mod api_login_v020_tests;
mod claude_config;
mod codex_api;
pub(crate) use takeover::manages_auth;
mod client;
mod registry;
mod retired;
pub use client::{claude_home, ClientId};
mod admission;
pub mod catalog;
mod circuit;
mod compaction;
mod connector;
mod forward;
pub(crate) mod metadata;
mod model;
mod protocol;
#[cfg(test)]
mod protocol_tests;
mod quota;
mod replay;
mod routing;
mod upstream_error;
#[cfg(test)]
mod v019_usage_tests;
#[cfg(test)]
mod v020_tests;
mod websocket;
pub use quota::QuotaView;
mod takeover;
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
    pub id: String,
    pub name: String,
    base_url: String,
    queued: bool,
    health: Health,
    quota_version: String,
    quota: Option<QuotaView>,
    pub max_concurrency: u32,
    pub max_rpm: u32,
    pub active_requests: usize,
    pub rpm_used: usize,
    pub rpm_retry_in: u64,
    pub rpm_limited: bool,
    pub rpm_ledger_error: bool,
    pub allowed_models: Option<Vec<String>>,
    pub supports_websocket: bool,
    pub handoff_after_compaction: bool,
    pub take_new_threads: bool,
}
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct View {
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
    pub compaction_pending: Vec<String>,
    pub error: Option<String>,
    pub recovery_pending: bool,
}
struct Inner {
    store: Store,
    revision: String,
    running: bool,
    upgrade_pending: bool,
    shutdown: Option<watch::Sender<bool>>,
    listener_task: Option<tokio::task::JoinHandle<()>>,
    error: Option<String>,
    circuits: HashMap<String, Circuit>,
    affinity: HashMap<String, (String, Option<String>, Instant)>,
    last_successful: Option<String>,
    home: Option<PathBuf>,
}
struct Shared {
    client: ClientId,
    diagnostics: Mutex<Option<crate::events::Service>>,
    usage: Mutex<Option<crate::usage::Service>>,
    diagnostic_conflict: std::sync::atomic::AtomicBool,
    registry: Arc<registry::Registry>,
    inner: Mutex<Inner>,
    lifecycle: tokio::sync::Mutex<()>,
    clients: Mutex<HashMap<String, HttpClient>>,
    data: PathBuf,
    spool: tempfile::TempDir,
    active: AtomicUsize,
    events: broadcast::Sender<()>,
    quota: quota::Service,
    catalog: catalog::Service,
    admission: admission::Scheduler,
    compaction: Arc<compaction::Registry>,
}
#[derive(Clone)]
pub struct Gateway(Arc<Shared>);
#[derive(Clone)]
struct Route {
    client_id: ClientId,
    provider: Provider,
    client: HttpClient,
    provider_circuit: Circuit,
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
                if self.0.client == ClientId::Codex {
                    "codex"
                } else {
                    "claude"
                },
                model,
                operation,
            )
        })
    }

    pub fn report_diagnostics(&self) {
        use crate::events::{Action, Reason};
        let view = self.view();
        let circuits: Vec<_> = self
            .0
            .inner
            .lock()
            .unwrap()
            .circuits
            .values()
            .cloned()
            .collect();
        for circuit in circuits {
            if let Some(event) = circuit.take_open_event() {
                if let Some(events) = self.0.diagnostics.lock().unwrap().as_ref() {
                    events.emit(event);
                }
            }
        }
        let conflict = view.config_error.is_some() || view.error.is_some();
        if conflict && !self.0.diagnostic_conflict.swap(conflict, Ordering::Relaxed) {
            self.record(
                None,
                None,
                Reason::ConfigConflict,
                Action::Stopped,
                None,
                None,
            );
        } else if !conflict {
            self.0.diagnostic_conflict.store(false, Ordering::Relaxed);
        }
    }
    pub fn set_diagnostics(&self, events: crate::events::Service) {
        *self.0.diagnostics.lock().unwrap() = Some(events);
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
        if let Some(events) = self.0.diagnostics.lock().unwrap().as_ref() {
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
        cause: Option<&crate::events::Record>,
        provider: Option<&str>,
        model: Option<&str>,
        status: Option<u16>,
        attempt: usize,
    ) {
        use crate::events::{Action, Reason};
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
        if let Some(service) = self.0.diagnostics.lock().unwrap().as_ref() {
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
    fn new_client(
        data: PathBuf,
        client: ClientId,
        shared: Option<Arc<registry::Registry>>,
    ) -> Result<Self> {
        storage::private_dir(&data)?;
        let error = takeover::recover(&data)
            .and_then(|()| retired::migrate(&data))
            .err()
            .map(|e| e.message);
        let upgrade_pending = error.is_some();
        let (mut store, revision) = Store::load(&data.join("gateway.json"))?;
        if revision == "missing" {
            store.settings.port = client.port();
        }
        let registry = match shared {
            Some(n) => n,
            None => Arc::new(registry::Registry::default()),
        };
        let spool = tempfile::Builder::new()
            .prefix("gateway-spool-")
            .tempdir_in(&data)
            .map_err(storage::io_error)?;
        storage::protect(spool.path(), true)?;
        let (events, _) = broadcast::channel(32);
        let admission = admission::Scheduler::new(events.clone(), data.join("rpm-window.json"));
        admission.configure(&store.providers, false);
        let gateway = Self(Arc::new(Shared {
            client,
            diagnostics: Mutex::new(None),
            usage: Mutex::new(None),
            diagnostic_conflict: std::sync::atomic::AtomicBool::new(false),
            registry: registry.clone(),
            inner: Mutex::new(Inner {
                store,
                revision,
                running: false,
                upgrade_pending,
                shutdown: None,
                listener_task: None,
                error,
                circuits: HashMap::new(),
                affinity: HashMap::new(),
                last_successful: None,
                home: None,
            }),
            lifecycle: tokio::sync::Mutex::new(()),
            clients: Mutex::new(HashMap::new()),
            compaction: compaction::Registry::new(data.join("conversation-owners.json")),
            data,
            spool,
            active: AtomicUsize::new(0),
            events,
            quota: quota::Service::new(),
            catalog: catalog::Service::new(),
            admission,
        }));
        registry.register(&gateway.0);
        Ok(gateway)
    }
    pub fn import_initial(&self, home: &Path) -> Result<()> {
        let _guard = self.0.registry.mutation.lock().unwrap();
        let mut s = self.0.inner.lock().unwrap();
        s.home = Some(home.to_owned());
        if s.upgrade_pending {
            return Err(AppError::new("CLEANUP", "请先处理未完成的升级清理"));
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
        if next.providers.is_empty() {
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
                    id: None,
                    base_url,
                    token,
                    name: None,
                },
                false,
            )?;
            if self.0.client == ClientId::Codex {
                let websocket = match &next.connection {
                    codex_api::Connection::ApiKey { provider } => {
                        codex_api::websocket(home, provider)?
                    }
                    _ => takeover::websocket_support_for(self.0.client, home)?,
                }
                .unwrap_or(true);
                if let Some(provider) = next.providers.last_mut() {
                    provider.supports_websocket = websocket;
                }
            }
        }
        next.initialized = true;
        s.revision = next.persist(&self.0.data.join("gateway.json"), &s.revision)?;
        s.store = next;
        self.0.admission.configure(&s.store.providers, false);
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
                .ok_or_else(|| AppError::new("IMPORT", "配置缺少供应商地址"))?;
            let token = pair
                .token
                .filter(|s| !s.is_empty())
                .ok_or_else(|| AppError::new("IMPORT", "auth.json 缺少 API Key"))?;
            if token.starts_with("gs_") && base.contains("127.0.0.1") {
                return Err(AppError::new("MANAGED", "不能导入网关的本地凭据"));
            }
            Ok((base, token))
        } else {
            takeover::import_for(client, home)
        }
    }
    pub fn client_id(&self) -> ClientId {
        self.0.client
    }
    fn view_revision(&self, s: &Inner) -> String {
        // File revisions also include resume checkpoints, which change after successful
        // auto-routing. Only configuration/lifecycle changes invalidate an editor.
        // Keep s.revision separate for the on-disk transaction checks below.
        let editable = (
            self.0.client,
            &s.store.providers,
            &s.store.connection,
            &s.store.settings,
            &s.store.mode,
            &s.store.selected,
            &s.store.local_token,
            s.running,
            &s.home,
            s.upgrade_pending,
        );
        storage::digest(&serde_json::to_vec(&editable).expect("serializable gateway configuration"))
    }
    pub fn subscribe(&self) -> broadcast::Receiver<()> {
        self.0.events.subscribe()
    }
    fn changed(&self) {
        let _ = self.0.events.send(());
    }
    pub fn view(&self) -> View {
        let s = self.0.inner.lock().unwrap();
        let (occupied, waiting) = self.0.admission.counts();
        let health = |key: String| s.circuits.get(&key).cloned().unwrap_or_default().health();
        let config = s
            .home
            .as_ref()
            .map(|h| takeover::read_connection(self.0.client, h, &s.store.connection));
        let config_revision = config
            .as_ref()
            .and_then(|r| r.as_ref().ok())
            .map(|(r, _)| r.clone());
        let mut config_state = "unknown";
        let mut config_provider = None;
        if let Some(Ok((_, pair))) = &config {
            config_state = "unsaved";
            if pair
                == &takeover::Pair::new(
                    &self.0.client.address(s.store.settings.port),
                    &s.store.local_token,
                )
            {
                config_state = "gateway";
            } else if let Some(p) = s
                .store
                .providers
                .iter()
                .find(|p| pair == &takeover::Pair::new(&p.base_url, &p.token))
            {
                config_state = "provider";
                config_provider = Some(p.id.clone());
            }
        }
        View {
            client_id: self.0.client,
            config_revision,
            config_provider,
            config_state: config_state.into(),
            config_error: config.and_then(|r| r.err()).map(|e| e.message),
            connection_mode: match &s.store.connection {
                codex_api::Connection::Bearer => "bearer",
                _ => "apiKey",
            }
            .into(),
            config_warning: if self.0.client == ClientId::Claude {
                s.home.as_ref().and_then(|h| claude_config::warning(h))
            } else {
                None
            },
            last_successful: s.last_successful.clone(),
            revision: self.view_revision(&s),
            running: s.running,
            address: self.0.client.address(s.store.settings.port),
            mode: s.store.mode.clone(),
            selected: s.store.selected.clone(),
            settings: s.store.settings.clone(),
            providers: s
                .store
                .providers
                .iter()
                .map(|p| {
                    let (rpm_used, rpm_retry_in, rpm_limited, rpm_ledger_error) =
                        self.0.admission.rpm_status(&p.id, p.max_rpm);
                    ProviderView {
                        id: p.id.clone(),
                        name: p.name.clone(),
                        base_url: p.base_url.clone(),
                        queued: p.queued,
                        health: health(pkey(p)),
                        quota_version: Self::quota_version(&s.store, p),
                        max_concurrency: p.max_concurrency,
                        max_rpm: p.max_rpm,
                        allowed_models: p.allowed_models.clone(),
                        supports_websocket: p.supports_websocket,
                        handoff_after_compaction: p.handoff_after_compaction,
                        take_new_threads: p.take_new_threads,
                        active_requests: occupied.get(&p.id).copied().unwrap_or(0),
                        rpm_used,
                        rpm_retry_in,
                        rpm_limited,
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
            compaction_pending: if self.0.client == ClientId::Codex && s.store.mode == "auto" {
                self.0.compaction.pending_policy(
                    &s.store
                        .providers
                        .iter()
                        .filter(|p| p.queued)
                        .cloned()
                        .collect::<Vec<_>>(),
                )
            } else {
                vec![]
            },
            error: s.error.clone().or_else(|| self.0.compaction.error()),
            recovery_pending: s.upgrade_pending
                || self.0.data.join("gateway-recovery.json").exists(),
        }
    }
    pub fn observe_home(&self, home: &Path) {
        self.0.inner.lock().unwrap().home = Some(home.to_owned());
    }
    pub fn read_config(&self, home: &Path) -> Result<crate::configuration::Document> {
        let s = self.0.inner.lock().unwrap();
        crate::configuration::read(
            self.0.client,
            home,
            &self.0.data,
            s.running
                || self.0.data.join("gateway-recovery.json").exists()
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
            } else {
                current == takeover::pair_for(self.0.client, text)?
            };
            if !unchanged {
                return Err(AppError::new(
                    "GATEWAY_ACTIVE",
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
                .find(|p| Some(&p.id) == store.selected.as_ref())
        } else {
            last.and_then(|id| store.providers.iter().find(|p| p.id == id))
                .or_else(|| store.providers.iter().find(|p| p.queued))
        };
        p.ok_or_else(|| {
            AppError::new(
                "PROVIDER",
                "没有关闭网关时可用的供应商，请选择供应商或加入故障转移队列",
            )
        })
    }
    pub fn guarded_home(&self) -> bool {
        self.view().running || self.0.data.join("gateway-recovery.json").exists()
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
        let _network_guard = self.0.registry.mutation.lock().unwrap();
        let mut s = self.0.inner.lock().unwrap();
        if expected != self.view_revision(&s) {
            return Err(AppError::new(
                "CONFLICT",
                "网关设置已变化，请使用最新状态重试",
            ));
        }
        if let Edit::Reset { id } = &edit {
            if !s.store.providers.iter().any(|p| &p.id == id) {
                return Err(AppError::new("PROVIDER", "供应商不存在"));
            }
            let prefix = format!("provider:{id}:");
            let circuits: Vec<_> = s
                .circuits
                .iter()
                .filter(|(key, _)| key.starts_with(&prefix))
                .map(|(_, circuit)| circuit.clone())
                .collect();
            self.0.admission.reset_provider(id, &circuits);
            drop(s);
            self.changed();
            return Ok(self.view());
        }
        if !s.running && self.0.data.join("gateway-recovery.json").exists() {
            return Err(AppError::new("RECOVERY", "请先处理未完成的配置事务"));
        }
        if s.upgrade_pending {
            return Err(AppError::new("CLEANUP", "请先处理未完成的升级清理"));
        }
        let direct_select = !s.running && matches!(&edit, Edit::Select { .. });
        if direct_select && self.claude_official() {
            return Err(AppError::new(
                "CLAUDE_OFFICIAL",
                "请先切换到 Claude API 配置",
            ));
        }
        if direct_select
            && self.0.client == ClientId::Codex
            && crate::official::blocks_gateway(&self.0.data, home)
        {
            return Err(AppError::new(
                "OFFICIAL_MODE",
                "官方账号连接期间不能切换网关供应商",
            ));
        }
        let mut next = s.store.clone();
        match edit {
            Edit::Connection { mode } => {
                if self.0.client != ClientId::Codex || s.running {
                    return Err(AppError::new(
                        "GATEWAY_ACTIVE",
                        "请先关闭 Codex 网关再修改连接方式",
                    ));
                }
                if crate::official::blocks_gateway(&self.0.data, home) {
                    return Err(AppError::new("OFFICIAL_MODE", "请先关闭官方账号连接"));
                }
                next.connection = match mode.as_str() {
                    "auto" => codex_api::detect(home)?,
                    "apiKey" => codex_api::binding(home)?,
                    "bearer" => {
                        takeover::read_for(ClientId::Codex, home)?;
                        codex_api::Connection::Bearer
                    }
                    _ => return Err(AppError::new("SETTINGS", "连接方式无效")),
                };
            }
            Edit::Import => {
                let supports_websocket = match &next.connection {
                    codex_api::Connection::ApiKey { provider }
                        if self.0.client == ClientId::Codex =>
                    {
                        codex_api::websocket(home, provider)?
                    }
                    _ => takeover::websocket_support_for(self.0.client, home)?,
                }
                .unwrap_or(true);
                let (base_url, token) = Self::import_pair(self.0.client, home, &next.connection)?;
                next.edit(
                    Edit::SaveProvider {
                        id: None,
                        base_url,
                        token,
                        name: None,
                    },
                    s.running,
                )?;
                if let Some(provider) = next.providers.last_mut() {
                    provider.supports_websocket = supports_websocket;
                }
            }
            edit => next.edit(edit, s.running)?,
        }
        let mut ports = self.0.registry.ports.lock().unwrap().clone();
        ports.push(next.settings.port);
        for provider in &next.providers {
            for port in &ports {
                model::base_url(&provider.base_url, *port)?;
            }
        }
        // A port change must not turn another client's existing upstream into a loop.
        if next.settings.port != s.store.settings.port {
            for peer in self
                .0
                .registry
                .gateways
                .lock()
                .unwrap()
                .iter()
                .filter_map(std::sync::Weak::upgrade)
            {
                if !Arc::ptr_eq(&peer, &self.0) {
                    for p in &peer.inner.lock().unwrap().store.providers {
                        model::base_url(&p.base_url, next.settings.port)?;
                    }
                }
            }
        }
        let revision = if s.running || direct_select {
            let p = Self::exit_provider(&next, s.last_successful.as_deref())?;
            let target = takeover::Pair::new(&p.base_url, &p.token);
            next.resume = Some(model::Resume {
                home: home.to_owned(),
                desired: s.running,
                pair_hash: target.fingerprint(),
            });
            let path = self.0.data.join("gateway.json");
            let old = storage::read_optional(&path)?;
            if storage::revision(old.as_deref()) != s.revision {
                return Err(AppError::new("CONFLICT", "网关存储已被外部修改"));
            }
            let after = serde_json::to_string_pretty(&next)
                .map_err(|_| AppError::new("STORE", "无法写入网关设置"))?;
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
        } else {
            next.persist(&self.0.data.join("gateway.json"), &s.revision)?
        };
        if s.last_successful
            .as_ref()
            .is_some_and(|id| !next.providers.iter().any(|p| &p.id == id))
        {
            s.last_successful = None;
        }
        s.store = next;
        s.revision = revision;
        s.home = Some(home.to_owned());
        self.sync_single_protection(&s);
        self.0.admission.configure(&s.store.providers, s.running);
        self.0.quota.retain(
            &s.store
                .providers
                .iter()
                .map(|p| (p.id.clone(), Self::quota_version(&s.store, p)))
                .collect(),
        );
        drop(s);
        self.0.registry.update_ports();
        self.0.clients.lock().unwrap().clear();
        self.changed();
        Ok(self.view())
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
        if self.0.client == ClientId::Codex && crate::official::blocks_gateway(&self.0.data, home) {
            return Err(AppError::new(
                "OFFICIAL_MODE",
                "请先关闭官方账号连接，再启动 Codex 网关",
            ));
        }
        if self.claude_official() {
            return Err(AppError::new(
                "CLAUDE_OFFICIAL",
                "请先切换到 Claude API 配置，再启用网关",
            ));
        }
        let _guard = self.0.lifecycle.lock().await;
        let (port, token) = {
            let s = self.0.inner.lock().unwrap();
            if expected != self.view_revision(&s) {
                return Err(AppError::new("CONFLICT", "网关设置已变化，请重试"));
            }
            if s.running {
                drop(s);
                return Ok(self.view());
            }
            if s.upgrade_pending {
                return Err(AppError::new("CLEANUP", "请先处理未完成的升级清理"));
            }
            if s.store.providers.is_empty() {
                return Err(AppError::new("PROVIDER", "请先添加供应商"));
            }
            (s.store.settings.port, s.store.local_token.clone())
        };
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port))
            .await
            .map_err(|_| AppError::new("PORT", "无法绑定本地端口，请检查端口占用或修改端口"))?;
        {
            let s = self.0.inner.lock().unwrap();
            if self.view_revision(&s) != expected {
                return Err(AppError::new("CONFLICT", "绑定端口期间设置已变化，请重试"));
            }
        }
        let (tx, rx) = watch::channel(false);
        {
            let mut s = self.0.inner.lock().unwrap();
            if self.view_revision(&s) != expected {
                return Err(AppError::new("CONFLICT", "设置已变化，请重试"));
            }
            let p = Self::exit_provider(&s.store, None)?;
            let exit = takeover::Pair::new(&p.base_url, &p.token);
            let (current, _) = takeover::read_connection(self.0.client, home, &s.store.connection)?;
            let mut next = s.store.clone();
            next.resume = Some(model::Resume {
                home: home.to_owned(),
                desired: true,
                pair_hash: exit.fingerprint(),
            });
            let before = storage::read_optional(&self.0.data.join("gateway.json"))?;
            if storage::revision(before.as_deref()) != s.revision {
                return Err(AppError::new("CONFLICT", "网关存储已被外部修改"));
            }
            let after = serde_json::to_string_pretty(&next)
                .map_err(|_| AppError::new("STORE", "无法生成网关启动事务"))?;
            takeover::attach_store_for(
                self.0.client,
                &self.0.data,
                home,
                port,
                &token,
                exit,
                config_revision.unwrap_or(&current),
                Some((before.map(|b| String::from_utf8(b).unwrap()), after.clone())),
            )?;
            s.store = next;
            s.revision = storage::digest(after.as_bytes());
            s.home = Some(home.to_owned());
            s.last_successful = None;
            s.running = true;
            s.shutdown = Some(tx);
            s.error = None;
            self.0.admission.configure(&s.store.providers, true);
        }
        let gateway = self.clone();
        let task = tokio::spawn(async move {
            forward::serve(gateway, listener, rx).await;
        });
        self.0.inner.lock().unwrap().listener_task = Some(task);
        self.changed();
        Ok(self.view())
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
            (
                self.view_revision(&s),
                s.store.resume.clone(),
                s.store.connection.clone(),
            )
        };
        let Some(intent) = intent.filter(|i| i.desired) else {
            return Ok(());
        };
        let result = async {
            if intent.home != home {
                return Err(AppError::new(
                    "RESUME_CONFLICT",
                    "客户端目录已变化，自动恢复已停止",
                ));
            }
            let (config_revision, pair) =
                takeover::read_connection(self.0.client, home, &connection)?;
            if pair.fingerprint() != intent.pair_hash {
                return Err(AppError::new(
                    "RESUME_CONFLICT",
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
        let _guard = self.0.lifecycle.lock().await;
        let listener_task = {
            let mut s = self.0.inner.lock().unwrap();
            if let (Some(expected), Some(home)) = (expected_config, &s.home) {
                if takeover::read_connection(self.0.client, home, &s.store.connection)?.0
                    != expected
                {
                    return Err(AppError::new("CONFLICT", "配置已变化，请刷新后重试"));
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
                            .ok_or_else(|| AppError::new("STATE", "缺少运行目录"))?;
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
                                        "RECOVERY",
                                        "缺少停止目标且配置未恢复",
                                    ));
                                }
                                pair
                            }
                        };
                        intent.pair_hash = target.fingerprint();
                        let before = storage::read_optional(&self.0.data.join("gateway.json"))?;
                        if storage::revision(before.as_deref()) != s.revision {
                            return Err(AppError::new("CONFLICT", "网关存储已被外部修改"));
                        }
                        let after = serde_json::to_string_pretty(&next)
                            .map_err(|_| AppError::new("STORE", "无法生成停止事务"))?;
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
                    s.store = next;
                }
            }
            if let Err(e) =
                takeover::recover(&self.0.data).and_then(|()| retired::migrate(&self.0.data))
            {
                s.error = Some(e.message.clone());
                drop(s);
                self.changed();
                return Err(e);
            }
            // Recovery may have completed a store/config transaction interrupted by an I/O failure.
            let (mut store, revision) = Store::load(&self.0.data.join("gateway.json"))?;
            if revision == "missing" {
                store.settings.port = self.0.client.port();
            }
            s.store = store;
            s.revision = revision;
            if let Some(tx) = s.shutdown.take() {
                let _ = tx.send(true);
            }
            s.running = false;
            s.upgrade_pending = false;
            self.0.admission.configure(&s.store.providers, false);
            s.error = None;
            s.listener_task.take()
        };
        if let Some(task) = listener_task {
            let _ = task.await;
        }
        self.0.clients.lock().unwrap().clear();
        self.changed();
        Ok(self.view())
    }
    fn successful_response(&self, provider: &Provider) {
        let mut s = self.0.inner.lock().unwrap();
        if !s.running
            || !s
                .store
                .providers
                .iter()
                .any(|p| p.id == provider.id && p.version == provider.version)
        {
            return;
        }
        if s.store.mode == "auto" {
            let target = takeover::Pair::new(&provider.base_url, &provider.token);
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
                    .ok_or_else(|| AppError::new("STATE", "缺少运行目录"))?;
                let mut next = s.store.clone();
                next.resume = Some(model::Resume {
                    home: home.clone(),
                    desired: true,
                    pair_hash: target.fingerprint(),
                });
                let before = storage::read_optional(&self.0.data.join("gateway.json"))?;
                if storage::revision(before.as_deref()) != s.revision {
                    return Err(AppError::new("CONFLICT", "网关存储已被外部修改"));
                }
                let after = serde_json::to_string_pretty(&next)
                    .map_err(|_| AppError::new("STORE", "无法记录最近供应商"))?;
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
                s.store = next;
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
    fn quota_version(_store: &Store, p: &Provider) -> String {
        p.version.clone()
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
            return Err(AppError::new("STALE", "供应商配置已变化，已丢弃旧额度结果"));
        }
        Ok(result)
    }
    fn query_input(&self, id: &str) -> Result<quota::Query> {
        Ok({
            let s = self.0.inner.lock().unwrap();
            let p = s
                .store
                .providers
                .iter()
                .find(|p| p.id == id)
                .ok_or_else(|| AppError::new("PROVIDER", "供应商不存在"))?;
            quota::Query {
                client_id: self.0.client,
                id: p.id.clone(),
                version: Self::quota_version(&s.store, p),
                base: p.base_url.clone(),
                token: p.token.clone(),
                client: Client::builder(TokioExecutor::new())
                    .retry_canceled_requests(false)
                    .build(
                        Connector::new(Duration::from_secs(10), s.store.settings.port)
                            .with_ports(self.0.registry.ports.clone()),
                    ),
            }
        })
    }
    pub async fn list_models(&self, id: &str, force: bool) -> Result<catalog::View> {
        let input = self.query_input(id)?;
        let version = input.version.clone();
        let result = self.0.catalog.query(input, force).await?;
        let s = self.0.inner.lock().unwrap();
        if !s
            .store
            .providers
            .iter()
            .any(|p| p.id == id && Self::quota_version(&s.store, p) == version)
        {
            return Err(AppError::new("STALE", "供应商配置已变化，已丢弃旧模型列表"));
        }
        Ok(result)
    }
    fn sync_single_protection(&self, state: &Inner) {
        let mut queue = state.store.providers.iter().filter(|p| p.queued);
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
    }
    fn routing_ids(&self, pinned: Option<&str>) -> Vec<String> {
        let s = self.0.inner.lock().unwrap();
        if let Some(id) = pinned {
            s.store
                .providers
                .iter()
                .filter(|p| p.id == id)
                .map(|p| p.id.clone())
                .collect()
        } else if s.store.mode == "auto" {
            s.store
                .providers
                .iter()
                .filter(|p| p.queued)
                .map(|p| p.id.clone())
                .collect()
        } else {
            s.store.selected.iter().cloned().collect()
        }
    }
    fn route(&self, id: &str) -> Option<Route> {
        let mut s = self.0.inner.lock().unwrap();
        let provider = s.store.providers.iter().find(|p| p.id == id)?.clone();
        let provider_circuit = s.circuits.entry(pkey(&provider)).or_default().clone();
        self.sync_single_protection(&s);
        let key = format!("{}:{}", pkey(&provider), s.store.settings.connect_seconds);
        let mut clients = self.0.clients.lock().unwrap();
        let client = clients
            .entry(key)
            .or_insert_with(|| {
                Client::builder(TokioExecutor::new())
                    .pool_idle_timeout(Duration::from_secs(60))
                    .pool_max_idle_per_host(8)
                    .retry_canceled_requests(false)
                    .build(
                        Connector::new(
                            Duration::from_secs(s.store.settings.connect_seconds),
                            s.store.settings.port,
                        )
                        .with_ports(self.0.registry.ports.clone()),
                    )
            })
            .clone();
        Some(Route {
            client_id: self.0.client,
            provider,
            client,
            provider_circuit,
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
        let mut s = self.0.inner.lock().unwrap();
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
