mod antigravity;
mod billing;
mod bodies;
mod body_wire;
pub mod commands;
#[cfg(test)]
mod decimal_v13_tests;
mod dedup;
#[cfg(test)]
mod disconnect_v02513_tests;
#[cfg(test)]
mod event_time_tests;
#[cfg(test)]
mod first_token_tests;
#[cfg(test)]
mod grok_usage_tests;
pub mod interruptions;
mod maintenance;
pub mod model;
mod pending_prices;
#[cfg(test)]
mod pending_prices_tests;
pub mod pricing;
#[cfg(test)]
mod pricing_mapping_tests;
mod query;
#[cfg(test)]
mod response_pricing_tests;
#[cfg(test)]
mod routing_outcome_v02513_tests;
mod sessions;
#[cfg(test)]
mod source_dedup_tests;
#[cfg(test)]
mod storage_migration_tests;
#[cfg(test)]
mod storage_v11_tests;
mod storage_v12;
#[cfg(test)]
mod storage_v12_capacity_tests;
#[cfg(test)]
mod storage_v12_tests;
mod store;
#[cfg(test)]
mod token_dimensions_tests;
#[cfg(test)]
mod usage_recovery_tests;
use crate::storage::{self, Result};
use model::*;
use pricing::{Pricing, Quote};
use serde::Serialize;
use std::{
    path::{Path, PathBuf},
    sync::{mpsc, Arc, Mutex, RwLock},
    time::Instant,
};
use tokio::sync::broadcast;

#[derive(Clone)]
pub struct Service(Arc<Inner>);
struct Inner {
    store: Mutex<Option<store::Store>>,
    overview_cache: store::OverviewCache,
    pub prices: Option<Pricing>,
    settings: RwLock<Settings>,
    path: PathBuf,
    directory: PathBuf,
    writer: mpsc::SyncSender<Record>,
    overflow: mpsc::SyncSender<Record>,
    dropped_records: std::sync::atomic::AtomicU64,
    spool_lock: Mutex<()>,
    price_hint: tokio::sync::Notify,
    backfilling: std::sync::atomic::AtomicBool,
    error: Mutex<Option<String>>,
    settings_error: Mutex<Option<String>>,
    write_error: Mutex<Option<String>>,
    repair_error: Mutex<Option<String>>,
    pending_error: Mutex<Option<String>>,
    syncing: Mutex<bool>,
    report: Mutex<std::collections::BTreeMap<String, sessions::Report>>,
    events: broadcast::Sender<()>,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct State {
    pub settings: Settings,
    pub syncing: bool,
    pub reports: std::collections::BTreeMap<String, sessions::Report>,
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub maintenance: Option<String>,
}
impl Inner {
    fn persist_pending(&self, record: &Record) -> Result<()> {
        let _guard = self.spool_lock.lock().unwrap();
        if record.id.is_empty()
            || record.id.len() > 64
            || !record
                .id
                .bytes()
                .all(|b| b.is_ascii_hexdigit() || b == b'-')
        {
            return Err(failure("用量待写记录标识无效"));
        }
        let dir = self.directory.join("pending");
        std::fs::create_dir_all(&dir).map_err(|_| failure("用量待写目录不可用"))?;
        storage::protect(&dir, true)?;
        let bytes = serde_json::to_vec(record).map_err(|_| failure("用量待写记录无效"))?;
        if bytes.len() > 2 * 1024 * 1024 {
            return Err(failure("用量元数据超过待写上限"));
        }
        let mut size = 0u64;
        let mut count = 0;
        for entry in std::fs::read_dir(&dir).map_err(|_| failure("用量待写目录不可读"))? {
            let entry = entry.map_err(|_| failure("用量待写目录无法完整读取"))?;
            size = size.saturating_add(
                entry
                    .metadata()
                    .map_err(|_| failure("用量待写记录不可读"))?
                    .len(),
            );
            count += 1;
        }
        let path = dir.join(format!("{}.json", record.id));
        if !path.exists()
            && (count >= 4096 || size.saturating_add(bytes.len() as u64) > 32 * 1024 * 1024)
        {
            return Err(failure("用量待写缓冲已满，请恢复数据库写入后重试同步"));
        }
        storage::atomic_write(&path, &bytes, None)
    }
    fn pending_records(&self, limit: usize) -> Vec<(PathBuf, Record)> {
        let _guard = self.spool_lock.lock().unwrap();
        *self.pending_error.lock().unwrap() = None;
        let entries = match std::fs::read_dir(self.directory.join("pending")) {
            Ok(entries) => entries,
            Err(error) => {
                if error.kind() != std::io::ErrorKind::NotFound {
                    *self.pending_error.lock().unwrap() = Some("用量待写目录无法读取".into());
                }
                return vec![];
            }
        };
        let mut records = Vec::new();
        for entry in entries {
            if records.len() >= limit {
                break;
            }
            let loaded = (|| -> Result<Option<(PathBuf, Record)>> {
                let entry = entry.map_err(|_| failure("用量待写目录无法完整读取"))?;
                let path = entry.path();
                if path.extension().and_then(|v| v.to_str()) != Some("json") {
                    return Ok(None);
                }
                if !entry
                    .file_type()
                    .map_err(|_| failure("用量待写记录不可读"))?
                    .is_file()
                {
                    return Err(failure("用量待写记录类型无效，已保留现场"));
                }
                let bytes = storage::read_bounded(&path, 2 * 1024 * 1024)?
                    .ok_or_else(|| failure("用量待写记录已被外部移除"))?;
                let record = serde_json::from_slice(&bytes)
                    .map_err(|_| failure("用量待写记录损坏，已保留现场"))?;
                Ok(Some((path, record)))
            })();
            match loaded {
                Ok(Some(record)) => records.push(record),
                Ok(None) => {}
                Err(_) => {
                    *self.pending_error.lock().unwrap() =
                        Some("用量待写记录无法读取，已保留现场".into())
                }
            }
        }
        records
    }
}
fn load_settings(path: &Path) -> (Settings, Option<String>) {
    let bytes = match storage::read_optional(path) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return (Settings::default(), None),
        Err(_) => {
            return (
                Settings::default(),
                Some("用量设置无法读取，原文件已保留".into()),
            )
        }
    };
    if let Ok(settings) = serde_json::from_slice::<Settings>(&bytes) {
        if settings.validate().is_ok() {
            return (settings, None);
        }
    }
    let message = if storage::preserve_corrupt(path, &bytes).is_ok() {
        "用量设置损坏，原文件和私有备份已保留，暂用默认设置"
    } else {
        "用量设置损坏，原文件已保留但备份失败，暂用默认设置"
    };
    (Settings::default(), Some(message.into()))
}
impl Service {
    pub fn new(data: &Path) -> Self {
        let dir = data.join("usage");
        let opened = store::Store::open(&dir);
        let write_error = opened.as_ref().err().map(|e| e.message.clone());
        let mut error = None;
        let prices = Pricing::new(&dir);
        if let Err(e) = &prices {
            error = Some(e.message.clone());
        }
        let path = dir.join("settings.json");
        let (settings, settings_error) = load_settings(&path);
        let (writer, receiver) = mpsc::sync_channel::<Record>(256);
        let (overflow, pending) = mpsc::sync_channel::<Record>(64);
        let (events, _) = broadcast::channel(32);
        let inner = Arc::new(Inner {
            store: Mutex::new(opened.ok()),
            overview_cache: Default::default(),
            prices: prices.ok(),
            settings: RwLock::new(settings),
            path,
            directory: dir,
            writer,
            overflow,
            dropped_records: std::sync::atomic::AtomicU64::new(0),
            spool_lock: Mutex::new(()),
            price_hint: tokio::sync::Notify::new(),
            backfilling: std::sync::atomic::AtomicBool::new(false),
            error: Mutex::new(error),
            settings_error: Mutex::new(settings_error),
            write_error: Mutex::new(write_error),
            repair_error: Mutex::new(None),
            pending_error: Mutex::new(None),
            syncing: Mutex::new(false),
            report: Mutex::new(Default::default()),
            events,
        });
        let spool = Arc::downgrade(&inner);
        std::thread::spawn(move || {
            while let Ok(record) = pending.recv() {
                let Some(inner) = spool.upgrade() else {
                    break;
                };
                let mut saved = false;
                for retry in 0..3 {
                    match inner.persist_pending(&record) {
                        Ok(()) => {
                            saved = true;
                            break;
                        }
                        Err(error) => {
                            *inner.pending_error.lock().unwrap() = Some(error.message);
                        }
                    }
                    std::thread::sleep(std::time::Duration::from_secs(1 << retry));
                }
                if !saved {
                    inner
                        .dropped_records
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                let _ = inner.events.send(());
            }
        });
        let weak = Arc::downgrade(&inner);
        std::thread::spawn(move || {
            loop {
                let first = receiver.recv_timeout(std::time::Duration::from_secs(1));
                let Some(inner) = weak.upgrade() else {
                    break;
                };
                let mut batch: Vec<Record> = first.ok().into_iter().collect();
                // Reserve half a batch for persisted retries so a continuous
                // stream of new requests cannot starve the overflow spool.
                batch.extend(receiver.try_iter().take(63));
                let old_pending_error = inner.pending_error.lock().unwrap().clone();
                let pending = inner.pending_records(128usize.saturating_sub(batch.len()));
                if old_pending_error != *inner.pending_error.lock().unwrap() {
                    let _ = inner.events.send(());
                }
                batch.extend(pending.iter().map(|(_, r)| r.clone()));
                // One bounded metadata batch per writer iteration, including idle periods.
                let repair = inner.store.lock().unwrap().as_mut().map(|store| {
                    let changed = store.repair_metadata_batch()? | store.repair_finish_times()?;
                    let migrated = store.compact_storage_batch()?;
                    let correlated = store.repair_correlations_batch()?;
                    if store.deferred_backfill() {
                        if let Some(pricing) = &inner.prices {
                            store.backfill(pricing)?;
                        }
                    }
                    Ok::<_, storage::AppError>(changed || migrated || correlated)
                });
                let repair_failed = repair
                    .as_ref()
                    .and_then(|v| v.as_ref().err())
                    .map(|e| e.message.clone());
                let repair_changed = *inner.repair_error.lock().unwrap() != repair_failed;
                *inner.repair_error.lock().unwrap() = repair_failed;
                if repair_changed || matches!(repair, Some(Ok(true))) {
                    let _ = inner.events.send(());
                }
                if batch.is_empty() {
                    continue;
                }
                let result = (|| {
                    let mut store = inner.store.lock().unwrap();
                    if store.is_none() {
                        *store = Some(store::Store::open(&inner.directory)?);
                    }
                    store
                        .as_mut()
                        .ok_or_else(|| failure("用量数据库不可用"))?
                        .write_batch(&batch, None)
                })();
                match result {
                    Ok(()) => {
                        *inner.write_error.lock().unwrap() = None;
                        let _guard = inner.spool_lock.lock().unwrap();
                        for (path, _) in pending {
                            let _ = std::fs::remove_file(path);
                        }
                    }
                    Err(e) => {
                        *inner.write_error.lock().unwrap() = Some(e.message);
                        for record in &batch {
                            if let Err(e) = inner.persist_pending(record) {
                                *inner.write_error.lock().unwrap() = Some(e.message);
                            }
                        }
                        std::thread::sleep(std::time::Duration::from_secs(1));
                    }
                }
                let _ = inner.events.send(());
            }
        });
        Self(inner)
    }
    pub fn schedule_price_backfill(&self) {
        use std::sync::atomic::Ordering;
        if self.0.backfilling.swap(true, Ordering::AcqRel) {
            return;
        }
        let service = self.clone();
        std::thread::spawn(move || {
            let result = service.query(|store| store.backfill(service.prices()?));
            if let Err(error) = result {
                *service.0.error.lock().unwrap() = Some(error.message);
            }
            service.0.backfilling.store(false, Ordering::Release);
            let _ = service.0.events.send(());
        });
    }
    pub async fn wait_price_hint(&self) {
        self.0.price_hint.notified().await;
    }
    pub fn subscribe(&self) -> broadcast::Receiver<()> {
        self.0.events.subscribe()
    }
    pub fn settings(&self) -> Settings {
        self.0.settings.read().unwrap().clone()
    }
    pub fn state(&self) -> State {
        let dropped = self
            .0
            .dropped_records
            .load(std::sync::atomic::Ordering::Relaxed);
        State {
            settings: self.settings(),
            maintenance: self.0.store.try_lock().ok().and_then(|store| store.as_ref().and_then(store::Store::maintenance_status)),
            syncing: *self.0.syncing.lock().unwrap(),
            reports: self.0.report.lock().unwrap().clone(),
            error: (dropped != 0).then(|| format!("{dropped} 条用量因写入拥塞未保存；请检查磁盘与数据库，并同步本机会话补齐可恢复记录"))
                .or_else(|| self
                .0
                .write_error
                .lock()
                .unwrap()
                .clone())
                .or_else(|| self.0.pending_error.lock().unwrap().clone())
                .or_else(|| self.0.repair_error.lock().unwrap().clone())
                .or_else(|| self.0.settings_error.lock().unwrap().clone())
                .or_else(|| self.0.error.lock().unwrap().clone()),
        }
    }
    pub fn configure(&self, settings: Settings) -> Result<State> {
        settings.validate()?;
        storage::atomic_write(
            &self.0.path,
            &serde_json::to_vec_pretty(&settings).map_err(|_| failure("设置无法保存"))?,
            None,
        )?;
        *self.0.settings.write().unwrap() = settings;
        *self.0.settings_error.lock().unwrap() = None;
        let _ = self.0.events.send(());
        Ok(self.state())
    }
    pub fn prices(&self) -> Result<&Pricing> {
        self.0
            .prices
            .as_ref()
            .ok_or_else(|| failure("价格库不可用"))
    }
    pub fn query<T>(&self, f: impl FnOnce(&mut store::Store) -> Result<T>) -> Result<T> {
        let mut guard = self.0.store.lock().unwrap();
        f(guard.as_mut().ok_or_else(|| failure("用量数据库不可用"))?)
    }
    /// Queries never hold the writer mutex. WAL provides a consistent snapshot
    /// while session parsing and short write batches continue independently.
    pub fn read<T>(&self, f: impl FnOnce(&store::Store) -> Result<T>) -> Result<T> {
        let reader =
            store::Store::reader_with_cache(&self.0.directory, self.0.overview_cache.clone())?;
        reader.snapshot(|db| f(db))
    }
    pub(crate) fn recent_gateway_requests(
        &self,
        client: &str,
        at: i64,
    ) -> Result<std::collections::BTreeMap<String, u64>> {
        self.read(|store| store.recent_gateway_requests(client, at))
    }
    pub fn maintain(&self) -> Result<()> {
        let result = self.query(|store| {
            store.compact(now())?;
            store.reclaim_space()
        });
        if let Err(error) = &result {
            *self.0.error.lock().unwrap() = Some(error.message.clone());
            let _ = self.0.events.send(());
        }
        result
    }
    pub fn sync_roots(
        &self,
        roots: Vec<(String, Result<PathBuf>)>,
        rebuild: Option<&str>,
    ) -> Result<State> {
        {
            let mut busy = self.0.syncing.lock().unwrap();
            if *busy {
                drop(busy);
                return Ok(self.state());
            }
            *busy = true;
        }
        let _ = self.0.events.send(());
        let result = (|| {
            let pricing = self.prices()?;
            let settings = self.settings();
            let mut first_error = None;
            for (client, root) in roots {
                if rebuild.is_some_and(|s| s != client) {
                    continue;
                }
                let outcome = (|| {
                    let root = root?;
                    let reader = store::Store::reader(&self.0.directory)?;
                    let repair = rebuild.is_some() || reader.needs_repair(&client)?;
                    let mut sink = SessionStore {
                        service: self.clone(),
                        reader,
                        client: client.clone(),
                    };
                    let mut report = if client == "antigravity" {
                        antigravity::sync(&mut sink, pricing, &settings, &root, repair)?
                    } else {
                        sessions::sync(&mut sink, pricing, &settings, &client, &root, repair)?
                    };
                    let (merged, pending, historical) = sink.reader.source_counts(&client)?;
                    report.merged = merged;
                    report.pending = pending;
                    report.historical_before = historical;
                    Ok::<_, crate::storage::AppError>(report)
                })();
                match outcome {
                    Ok(report) => {
                        self.0.report.lock().unwrap().insert(client.clone(), report);
                    }
                    Err(error) => {
                        let mut reports = self.0.report.lock().unwrap();
                        let report = reports.entry(client.clone()).or_default();
                        report.phase = "failed".into();
                        report.errors = report.errors.saturating_add(1);
                        first_error.get_or_insert(error);
                    }
                }
            }
            self.query(|store| store.compact(now()))?;
            first_error.map_or(Ok(()), Err)
        })();
        *self.0.syncing.lock().unwrap() = false;
        *self.0.error.lock().unwrap() = result
            .as_ref()
            .err()
            .map(|e: &crate::storage::AppError| e.message.clone());
        let _ = self.0.events.send(());
        result.map(|_| self.state())
    }
    pub async fn update_prices(&self, force: bool) -> Result<pricing::View> {
        let before = self.prices()?.view().version;
        let view = self.prices()?.update(force).await?;
        if !force && before == view.version {
            return Ok(view);
        }
        let s = self.clone();
        tokio::task::spawn_blocking(move || s.query(|store| store.backfill(s.prices()?)))
            .await
            .map_err(|_| failure("价格补算中断"))??;
        let _ = self.0.events.send(());
        Ok(view)
    }
    #[cfg(test)]
    pub fn begin(&self, client: &str, model: Option<&str>) -> Trace {
        self.begin_operation(client, model, Operation::Model)
    }
    pub fn begin_operation(
        &self,
        client: &str,
        model: Option<&str>,
        operation: Operation,
    ) -> Trace {
        Trace(Arc::new(TraceInner {
            service: self.clone(),
            client: client.into(),
            requested: model_id(model),
            operation,
            id: uuid::Uuid::new_v4().to_string(),
            at: now(),
            attempts: Mutex::new(Vec::new()),
            failover: Mutex::new(None),
            recording: self.settings().recording,
        }))
    }
}
struct SessionStore {
    service: Service,
    reader: store::Store,
    client: String,
}
impl sessions::Repository for SessionStore {
    fn cursor(&self, id: &str) -> Result<Option<String>> {
        self.reader.cursor(id)
    }
    fn generation(&self, source: &str) -> Result<u64> {
        self.reader.source_generation(source)
    }
    fn write_batch(&mut self, rows: &[Record], cursor: Option<(&str, &str)>) -> Result<()> {
        self.service.query(|db| db.write_batch(rows, cursor))
    }
    fn begin_rebuild(&mut self, source: &str) -> Result<()> {
        self.service.query(|db| db.begin_rebuild(source))
    }
    fn stage_rebuild(
        &mut self,
        source: &str,
        rows: &[Record],
        cursor: Option<(&str, &str)>,
    ) -> Result<()> {
        self.service
            .query(|db| db.stage_rebuild(source, rows, cursor))
    }
    fn finish_rebuild(&mut self, source: &str) -> Result<()> {
        self.service.query(|db| db.finish_rebuild(source))
    }
    fn progress(&mut self, report: &sessions::Report) {
        self.service
            .0
            .report
            .lock()
            .unwrap()
            .insert(self.client.clone(), report.clone());
        let _ = self.service.0.events.send(());
    }
}
/// A trace lives until the downstream body / WS generation releases its final
/// attempt. Drop never rejects forwarding; a full writer queue uses a bounded
/// private metadata spool, and cancellation is recorded only once.
#[derive(Clone)]
pub struct Trace(Arc<TraceInner>);
struct TraceInner {
    service: Service,
    client: String,
    requested: Option<String>,
    operation: Operation,
    id: String,
    at: i64,
    attempts: Mutex<Vec<Attempt>>,
    failover: Mutex<Option<crate::gateway::failover::Evidence>>,
    recording: bool,
}
impl Drop for TraceInner {
    fn drop(&mut self) {
        if !self.recording {
            return;
        }
        let attempts = self.attempts.get_mut().unwrap();
        if attempts.is_empty() {
            return;
        }
        let completed = attempts
            .last()
            .is_some_and(|a| matches!(a.outcome.as_str(), "success" | "limited"));
        if attempts
            .iter()
            .any(|a| a.price.is_none() && a.tokens.total().is_some())
        {
            self.service.0.price_hint.notify_one();
        }
        let record = Record {
            id: self.id.clone(),
            failover_evidence: self.failover.get_mut().unwrap().take(),
            client: self.client.clone(),
            source: "proxy".into(),
            started_at: self.at,
            attempts: std::mem::take(attempts),
            completed,
            ..Record::default()
        };
        if let Err(error) = self.service.0.writer.try_send(record) {
            let record = match error {
                mpsc::TrySendError::Full(r) | mpsc::TrySendError::Disconnected(r) => r,
            };
            if self.service.0.overflow.try_send(record).is_err() {
                self.service
                    .0
                    .dropped_records
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            let _ = self.service.0.events.send(());
        }
    }
}
pub struct AttemptTrace {
    trace: Trace,
    attempt: Attempt,
    started: Instant,
    prices: Option<Pricing>,
    settings: Settings,
    quote: Option<Quote>,
    done: bool,
    routing: Option<RoutingCompletion>,
}
/// Keeps the single logical record alive until an ownership save has settled.
/// Only routing metadata may change after the attempt's normal accounting point.
#[derive(Clone)]
pub(crate) struct RoutingCompletion(Arc<RoutingCompletionInner>);
struct RoutingCompletionInner {
    trace: Trace,
    id: String,
    state: Mutex<RoutingCompletionState>,
}
#[derive(Default)]
struct RoutingCompletionState {
    outcome: Option<crate::events::RoutingOutcome>,
    generation: u64,
}
impl RoutingCompletion {
    pub(crate) fn id(&self) -> &str {
        &self.0.id
    }
    pub(crate) fn begin_save(&self) -> u64 {
        let mut state = self.0.state.lock().unwrap();
        state.generation = state.generation.saturating_add(1);
        state.generation
    }
    pub(crate) fn saved(&self, generation: u64, outcome: crate::events::RoutingOutcome) {
        let mut state = self.0.state.lock().unwrap();
        if generation == state.generation || outcome == crate::events::RoutingOutcome::SaveFailed {
            self.update(&mut state, outcome);
        }
    }
    pub(crate) fn outcome(&self, outcome: crate::events::RoutingOutcome) {
        self.update(&mut self.0.state.lock().unwrap(), outcome);
    }
    pub(crate) fn finish(&self, outcome: crate::events::RoutingOutcome) {
        let mut state = self.0.state.lock().unwrap();
        if state.outcome.is_none() && state.generation == 0 {
            self.update(&mut state, outcome);
        }
    }
    fn update(&self, state: &mut RoutingCompletionState, outcome: crate::events::RoutingOutcome) {
        // A later success cannot hide a failed ownership write.
        if state.outcome == Some(crate::events::RoutingOutcome::SaveFailed) {
            return;
        }
        state.outcome = Some(outcome);
        let mut attempts = self.0.trace.0.attempts.lock().unwrap();
        if let Some(attempt) = attempts.iter_mut().find(|a| a.id == self.0.id) {
            // A bounded retry summary no longer describes one routing action.
            attempt.routing_outcome = Some(if attempt.repeat_count == 1 {
                outcome
            } else {
                crate::events::RoutingOutcome::Unknown
            });
        }
    }
}
impl Trace {
    pub fn failover(&self, evidence: crate::gateway::failover::Evidence) {
        *self.0.failover.lock().unwrap() = Some(evidence);
    }
    pub fn attempt(&self, provider: &str, stream: bool, transport: &str) -> AttemptTrace {
        let settings = self.0.service.settings();
        let prices = self.0.service.0.prices.as_ref().map(Pricing::frozen);
        let quote = prices
            .as_ref()
            .map(|p| p.quote(self.0.requested.as_deref(), &settings.multiplier));
        AttemptTrace {
            trace: self.clone(),
            attempt: Attempt {
                id: uuid::Uuid::new_v4().to_string(),
                provider: Some(provider.into()),
                requested_model: self.0.requested.clone(),
                started_at: now(),
                stream,
                transport: transport.into(),
                cost_multiplier: settings.multiplier.clone(),
                operation: self.0.operation,
                compaction_kind: (self.0.operation == Operation::Compaction)
                    .then(|| "request".into()),
                ..Attempt::default()
            },
            started: Instant::now(),
            prices,
            settings,
            quote,
            done: false,
            routing: None,
        }
    }
}
impl AttemptTrace {
    pub fn upstream_model(&mut self, model: Option<String>) {
        self.attempt.upstream_model = model_id(model.as_deref());
    }
    pub(crate) fn routing_completion(&mut self) -> RoutingCompletion {
        self.routing
            .get_or_insert_with(|| {
                RoutingCompletion(Arc::new(RoutingCompletionInner {
                    trace: self.trace.clone(),
                    id: self.attempt.id.clone(),
                    state: Mutex::default(),
                }))
            })
            .clone()
    }
    pub fn id(&self) -> &str {
        &self.attempt.id
    }
    pub fn transfer(&mut self, value: model::TransferMetadata) {
        self.attempt.transfer = Some(value);
    }
    pub fn end(&mut self, end: EndMetadata) {
        self.attempt.end = Some(end);
    }
    pub fn context(&mut self, thread: Option<String>, decision: Option<RoutingDecision>) {
        self.attempt.thread_hash =
            thread.filter(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()));
        self.attempt.routing_decision = decision;
    }
    pub fn routing_evidence(&mut self, mut evidence: model::RoutingEvidence) {
        use model::{RoutingDecision as D, RoutingRestriction as R};
        evidence.restriction = match self.attempt.routing_decision {
            Some(D::NewThreadModelDenied | D::ModelDetourOwnerPreserved) => Some(R::ModelMismatch),
            Some(D::NewThreadModelCooling) => Some(R::ModelCooling),
            Some(D::NewThreadProviderUnavailable) => Some(R::ProviderUnavailable),
            Some(D::NewThreadCapacityFull | D::ReceiverCapacityFull) => Some(R::CapacityFull),
            Some(D::NewThreadQueueWaiting) => Some(R::QueueWaiting),
            _ => None,
        };
        self.attempt.routing_evidence = Some(evidence);
    }
    pub fn upstream_request(&mut self, headers: &hyper::HeaderMap) {
        self.attempt.upstream_request_hash = ["x-request-id", "request-id", "openai-request-id"]
            .iter()
            .find_map(|key| headers.get(*key).and_then(|v| v.to_str().ok()))
            .filter(|v| !v.is_empty() && v.len() <= 512)
            .map(|v| storage::digest(v.as_bytes()));
    }
    pub fn anthropic(&self) -> Option<bool> {
        match self.trace.0.client.as_str() {
            "claude" => Some(true),
            "grok" | "dsh" => None,
            _ => Some(false),
        }
    }
    pub fn update(&mut self, m: &Meter, status: Option<u16>, outcome: Option<&str>) {
        self.attempt.tokens.merge(&m.tokens);
        self.attempt.requested_speed = m.requested_speed.clone();
        self.attempt.response_speed = m.response_speed.clone();
        self.attempt.raw_input_tokens = m.raw_input_tokens;
        self.attempt.input_basis = m.input_basis.clone();
        self.attempt.output_reported = m.output_reported;
        self.attempt.parse_reasons = m.parse_reasons.clone();
        if m.inclusive_input.is_some() {
            self.attempt.inclusive_input_tokens = m.inclusive_input;
        }
        if self.attempt.compaction_kind.is_none() {
            self.attempt.compaction_kind = m.compaction_kind.clone();
        }
        self.attempt.usage_status = if m.parse_incomplete {
            "parse_incomplete"
        } else if m.ended_early && self.attempt.tokens.total().is_none() {
            "ended_early"
        } else if self.attempt.tokens.input.is_some() && self.attempt.tokens.output.is_some() {
            "reported"
        } else if self.attempt.tokens.total().is_some() {
            "partial"
        } else {
            "upstream_unreported"
        }
        .into();
        if m.response_id.is_some() {
            self.attempt.response_id = m.response_id.clone();
        }
        if m.model.is_some() {
            self.attempt.response_model = m.model.clone();
        }
        if m.service_tier.is_some() {
            self.attempt.service_tier = m.service_tier.clone();
        }
        if self.attempt.first_token_ms.is_none()
            && self.attempt.stream
            && self.attempt.operation == Operation::Model
        {
            self.attempt.first_token_ms = m.first_token_ms;
        }
        self.attempt.status = status.or(self.attempt.status);
        if !self.done {
            if let Some(outcome) = outcome {
                self.attempt.outcome = outcome.into();
                self.attempt.duration_ms = self.started.elapsed().as_millis() as u64;
                self.done = true;
            }
        }
    }
    fn price(&mut self) {
        let (model, basis, mapping) = self
            .prices
            .as_ref()
            .map(|p| {
                p.resolve(
                    &self.trace.0.client,
                    self.attempt.provider.as_deref(),
                    self.attempt
                        .upstream_model
                        .as_deref()
                        .or(self.attempt.requested_model.as_deref()),
                    self.attempt.response_model.as_deref(),
                    &self.settings.pricing_model,
                )
            })
            .unwrap_or_else(|| {
                let requested = self.settings.pricing_model == "request"
                    && (self.attempt.upstream_model.is_some()
                        || self.attempt.requested_model.is_some());
                let (model, basis) = if requested {
                    (
                        self.attempt
                            .upstream_model
                            .clone()
                            .or_else(|| self.attempt.requested_model.clone()),
                        "request",
                    )
                } else if self.attempt.response_model.is_some() {
                    (self.attempt.response_model.clone(), "response")
                } else {
                    (
                        self.attempt
                            .upstream_model
                            .clone()
                            .or_else(|| self.attempt.requested_model.clone()),
                        "request_fallback",
                    )
                };
                (model, basis.into(), None)
            });
        self.attempt.pricing_model = model;
        self.attempt.pricing_basis = Some(basis);
        self.attempt.mapping_revision = mapping;
        if self.attempt.operation == Operation::WebSearch {
            self.attempt.price = self.attempt.search_price();
            return;
        }
        if self
            .quote
            .as_ref()
            .is_none_or(|q| Some(&q.model) != self.attempt.pricing_model.as_ref())
        {
            self.quote = self.prices.as_ref().map(|p| {
                p.quote(
                    self.attempt.pricing_model.as_deref(),
                    &self.settings.multiplier,
                )
            });
        }
        self.attempt.price = self
            .quote
            .as_ref()
            .and_then(|q| q.calculate(&self.attempt.tokens, self.attempt.price_tier()));
    }
}
impl Drop for AttemptTrace {
    fn drop(&mut self) {
        if !self.done {
            self.attempt.outcome = "unknown".into();
            self.attempt.end.get_or_insert(EndMetadata {
                cause: EndCause::Unsettled,
                terminal_received: false,
                response_started: self.attempt.status.is_some(),
            });
            if self.attempt.tokens.total().is_none()
                && self.attempt.usage_status != "parse_incomplete"
            {
                self.attempt.usage_status = "ended_early".into();
            }
            self.attempt.duration_ms = self.started.elapsed().as_millis() as u64;
        }
        self.attempt.duration_ms = self.started.elapsed().as_millis() as u64;
        self.price();
        self.attempt.annotate_availability("proxy");
        // Use the same lock order as save receipts. A receipt may arrive on
        // either side of this insertion without losing its outcome.
        let routing = self.routing.as_ref().map(|r| r.0.state.lock().unwrap());
        if let Some(routing) = &routing {
            self.attempt.routing_outcome = routing.outcome;
        }
        let mut attempts = self.trace.0.attempts.lock().unwrap();
        if self.trace.0.client != "claude" && attempts.len() >= 128 {
            let pair = (1..attempts.len())
                .find_map(|j| {
                    (0..j)
                        .find(|&i| attempts[i].same_dimensions(&attempts[j]))
                        .map(|i| (i, j))
                })
                .unwrap_or((0, 1));
            let next = attempts.remove(pair.1);
            attempts[pair.0].compact(next);
        }
        attempts.push(self.attempt.clone());
    }
}

#[cfg(test)]
mod pricing_tests;
#[cfg(test)]
mod tests;

#[cfg(test)]
mod metering_v02514_tests;
#[cfg(test)]
mod settings_tests;
