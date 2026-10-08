pub mod commands;
pub mod model;
pub mod pricing;
mod sessions;
mod store;
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
    pub prices: Option<Pricing>,
    settings: RwLock<Settings>,
    path: PathBuf,
    writer: mpsc::SyncSender<Record>,
    error: Mutex<Option<String>>,
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
impl Service {
    pub fn new(data: &Path) -> Self {
        let dir = data.join("usage");
        let opened = store::Store::open(&dir);
        let mut error = opened.as_ref().err().map(|e| e.message.clone());
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
            prices: prices.ok(),
            settings: RwLock::new(settings),
            path,
            writer,
            error: Mutex::new(error),
            syncing: Mutex::new(false),
            report: Mutex::new(Default::default()),
            events,
        });
        let weak = Arc::downgrade(&inner);
        std::thread::spawn(move || {
            while let Ok(r) = receiver.recv() {
                let Some(inner) = weak.upgrade() else {
                    break;
                };
                let result = inner
                    .store
                    .lock()
                    .unwrap()
                    .as_mut()
                    .ok_or_else(|| failure("用量数据库不可用"))
                    .and_then(|store| store.write_batch(&[r], None));
                if let Err(e) = result {
                    *inner.error.lock().unwrap() = Some(e.message);
                }
                let _ = inner.events.send(());
            }
        });
        Self(inner)
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
            error: self.0.error.lock().unwrap().clone(),
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
                let report = self.query(|store| {
                    sessions::sync(store, pricing, &settings, client, root, rebuild.is_some())
                })?;
                self.0.report.lock().unwrap().insert(client.clone(), report);
            }
            self.query(|store| {
                store.backfill(pricing, &settings.multiplier)?;
                store.compact(now())
            })?;
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
        let view = self.prices()?.update(force).await?;
        let s = self.clone();
        tokio::task::spawn_blocking(move || {
            s.query(|store| store.backfill(s.prices()?, &s.settings().multiplier))
        })
        .await
        .map_err(|_| failure("价格补算中断"))??;
        let _ = self.0.events.send(());
        Ok(view)
    }
    pub fn begin(&self, client: &str, model: Option<&str>) -> Trace {
        Trace(Arc::new(TraceInner {
            service: self.clone(),
            client: client.into(),
            requested: model_id(model),
            id: uuid::Uuid::new_v4().to_string(),
            at: now(),
            attempts: Mutex::new(Vec::new()),
            recording: self.settings().recording,
        }))
    }
}
/// A trace lives until the downstream body / WS generation releases its final
/// attempt. Drop is fail-open, nonblocking and records cancellation only once.
#[derive(Clone)]
pub struct Trace(Arc<TraceInner>);
struct TraceInner {
    service: Service,
    client: String,
    requested: Option<String>,
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
        let record = Record {
            id: self.id.clone(),
            client: self.client.clone(),
            source: "proxy".into(),
            started_at: self.at,
            attempts: std::mem::take(attempts),
            completed,
            ..Record::default()
        };
        if self.service.0.writer.try_send(record).is_err() {
            *self.service.0.error.lock().unwrap() = Some("用量写入队列已满，部分记录未保存".into());
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
        self.attempt.tokens = m.tokens.clone();
        self.attempt.response_id = m.response_id.clone();
        self.attempt.response_model = m.model.clone();
        self.attempt.service_tier = m.service_tier.clone();
        self.attempt.first_token_ms = m.first_token_ms;
        self.attempt.status = status.or(self.attempt.status);
        if !self.done {
            if let Some(outcome) = outcome {
                self.attempt.outcome = outcome.into();
                self.attempt.duration_ms = self.started.elapsed().as_millis() as u64;
                self.done = true;
            }
        }
        self.attempt.pricing_model = if self.settings.pricing_model == "request" {
            self.attempt.requested_model.clone().or(m.model.clone())
        } else {
            m.model.clone().or(self.attempt.requested_model.clone())
        };
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
            .and_then(|q| q.calculate(&m.tokens, m.service_tier.as_deref()));
    }
}
impl Drop for AttemptTrace {
    fn drop(&mut self) {
        if !self.done {
            self.attempt.outcome = "cancelled".into();
            self.attempt.duration_ms = self.started.elapsed().as_millis() as u64;
        }
        self.trace
            .0
            .attempts
            .lock()
            .unwrap()
            .push(self.attempt.clone());
    }
}

#[cfg(test)]
mod pricing_tests;
#[cfg(test)]
mod tests;
