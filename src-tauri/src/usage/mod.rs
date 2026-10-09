pub mod commands;
mod dedup;
pub mod model;
pub mod pricing;
mod query;
mod sessions;
mod store;
#[cfg(test)]
mod v015_tests;
#[cfg(test)]
mod v016_tests;
#[cfg(test)]
mod v017_tests;
#[cfg(test)]
mod v018_tests;
#[cfg(test)]
mod v019_tests;
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
    spool_lock: Mutex<()>,
    price_hint: tokio::sync::Notify,
    backfilling: std::sync::atomic::AtomicBool,
    error: Mutex<Option<String>>,
    write_error: Mutex<Option<String>>,
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
        let saved = storage::read_optional(&path);
        let settings = saved
            .ok()
            .flatten()
            .and_then(|v| serde_json::from_slice::<Settings>(&v).ok())
            .filter(|v| v.validate().is_ok())
            .unwrap_or_default();
        let (writer, receiver) = mpsc::sync_channel::<Record>(256);
        let (events, _) = broadcast::channel(32);
        let inner = Arc::new(Inner {
            store: Mutex::new(opened.ok()),
            overview_cache: Default::default(),
            prices: prices.ok(),
            settings: RwLock::new(settings),
            path,
            directory: dir,
            writer,
            spool_lock: Mutex::new(()),
            price_hint: tokio::sync::Notify::new(),
            backfilling: std::sync::atomic::AtomicBool::new(false),
            error: Mutex::new(error),
            write_error: Mutex::new(write_error),
            pending_error: Mutex::new(None),
            syncing: Mutex::new(false),
            report: Mutex::new(Default::default()),
            events,
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
            let result = service
                .query(|store| store.backfill(service.prices()?, &service.settings().multiplier));
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
        State {
            settings: self.settings(),
            syncing: *self.0.syncing.lock().unwrap(),
            reports: self.0.report.lock().unwrap().clone(),
            error: self
                .0
                .write_error
                .lock()
                .unwrap()
                .clone()
                .or_else(|| self.0.pending_error.lock().unwrap().clone())
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
    pub fn maintain(&self) -> Result<()> {
        let result = self.query(|store| store.compact(now()));
        if let Err(error) = &result {
            *self.0.error.lock().unwrap() = Some(error.message.clone());
            let _ = self.0.events.send(());
        }
        result
    }
    pub fn sync(&self, roots: &[(String, PathBuf)], rebuild: Option<&str>) -> Result<State> {
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
            for (client, root) in roots {
                if rebuild.is_some_and(|s| s != client) {
                    continue;
                }
                let reader = store::Store::reader(&self.0.directory)?;
                let repair = rebuild.is_some() || reader.needs_repair(client)?;
                let mut sink = SessionStore {
                    service: self.clone(),
                    reader,
                    client: client.clone(),
                };
                let mut report =
                    match sessions::sync(&mut sink, pricing, &settings, client, root, repair) {
                        Ok(report) => report,
                        Err(error) => {
                            let mut reports = self.0.report.lock().unwrap();
                            let report = reports.entry(client.clone()).or_default();
                            report.phase = "failed".into();
                            report.errors = report.errors.saturating_add(1);
                            return Err(error);
                        }
                    };
                let (merged, pending, historical) = sink.reader.source_counts(client)?;
                report.merged = merged;
                report.pending = pending;
                report.historical_before = historical;
                self.0.report.lock().unwrap().insert(client.clone(), report);
            }
            self.query(|store| store.compact(now()))?;
            Ok(())
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
        tokio::task::spawn_blocking(move || {
            s.query(|store| store.backfill(s.prices()?, &s.settings().multiplier))
        })
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
    fn write_batch(&mut self, rows: &[Record], cursor: Option<(&str, &str)>) -> Result<()> {
        for batch in rows.chunks(256) {
            self.service.query(|db| db.write_batch(batch, None))?;
        }
        if cursor.is_some() {
            self.service.query(|db| db.write_batch(&[], cursor))?;
        }
        Ok(())
    }
    fn rebuild(
        &mut self,
        source: &str,
        rows: &[Record],
        cursors: &[(String, String)],
    ) -> Result<()> {
        self.service.query(|db| db.rebuild(source, rows, cursors))
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
            if let Err(error) = self.service.0.persist_pending(&record) {
                *self.service.0.write_error.lock().unwrap() = Some(error.message);
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
}
impl Trace {
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
        }
    }
}
impl AttemptTrace {
    pub fn update(&mut self, m: &Meter, status: Option<u16>, outcome: Option<&str>) {
        self.attempt.tokens.merge(&m.tokens);
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
        let (model, basis, mapping) = self
            .prices
            .as_ref()
            .map(|p| {
                p.resolve(
                    &self.trace.0.client,
                    self.attempt.provider.as_deref(),
                    self.attempt.requested_model.as_deref(),
                    self.attempt.response_model.as_deref(),
                    &self.settings.pricing_model,
                )
            })
            .unwrap_or_else(|| {
                let requested = self.settings.pricing_model == "request"
                    && self.attempt.requested_model.is_some();
                let (model, basis) = if requested {
                    (self.attempt.requested_model.clone(), "request")
                } else if self.attempt.response_model.is_some() {
                    (self.attempt.response_model.clone(), "response")
                } else {
                    (self.attempt.requested_model.clone(), "request_fallback")
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
            .and_then(|q| q.calculate(&self.attempt.tokens, self.attempt.service_tier.as_deref()));
    }
}
impl Drop for AttemptTrace {
    fn drop(&mut self) {
        if !self.done {
            self.attempt.outcome = "cancelled".into();
            if self.attempt.tokens.total().is_none()
                && self.attempt.usage_status != "parse_incomplete"
            {
                self.attempt.usage_status = "ended_early".into();
            }
            self.attempt.duration_ms = self.started.elapsed().as_millis() as u64;
        }
        let mut attempts = self.trace.0.attempts.lock().unwrap();
        if attempts.len() >= 128 {
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
