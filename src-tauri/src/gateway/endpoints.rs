//! Credential-free endpoint probes and bounded, hash-only conversation affinity.
use super::{connector, model::Provider, replay, Connector, Gateway, HttpClient, Route};
use crate::errors::code;
use crate::storage::{self, AppError, Result};
use futures_util::{stream, StreamExt};
use http_body_util::BodyExt;
use hyper::{header, Request};
use hyper_util::{client::legacy::Client, rt::TokioExecutor};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

fn poisoned_affinity() -> AppError {
    AppError::new(
        code::ENDPOINT_AFFINITY,
        "端点归属状态不可用，已停止自动切换",
    )
}

const PROBE_TIMEOUT: Duration = Duration::from_secs(8);
const MAX_PINS: usize = 8192;
const PIN_RETENTION_MS: u64 = 7 * 24 * 60 * 60 * 1000;
const PIN_REFRESH: Duration = Duration::from_secs(60);

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Measurement {
    pub base_url: String,
    pub latency_ms: Option<u64>,
    pub status: Option<u16>,
    pub error: Option<String>,
    pub checked_at: u64,
}
impl Measurement {
    fn usable(&self) -> bool {
        self.error.is_none()
            && self.latency_ms.is_some()
            && self.status.is_some_and(|s| {
                (200..500).contains(&s) && !matches!(s, 407 | 421 | 429) && !(300..400).contains(&s)
            })
    }
}
struct Batch {
    config: String,
    checked: Instant,
    rows: Vec<Measurement>,
}
#[derive(Clone, Serialize, Deserialize)]
struct Pin {
    endpoint: String,
    updated: u64,
}
#[derive(Default)]
struct Pins {
    entries: HashMap<String, Pin>,
    error: bool,
    // A failed first bind retains only its own choice, including when rename
    // succeeded but directory sync failed. Other keys never retry this write.
    pending: HashMap<String, Pin>,
    refresh_attempts: HashMap<String, Instant>,
}
pub struct Service {
    samples: Mutex<HashMap<String, Batch>>,
    flights: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    permits: tokio::sync::Semaphore,
    pins: Mutex<Pins>,
    write: Mutex<()>,
    diagnostics: Mutex<Option<(super::ClientId, crate::events::Service)>>,
    responses: Mutex<HashMap<String, Pin>>,
    path: PathBuf,
}
fn now() -> u64 {
    crate::usage::model::now().max(0) as u64
}
fn digest(value: &str) -> String {
    let normalized = url::Url::parse(value.trim())
        .map(|url| url.to_string())
        .unwrap_or_else(|_| value.trim().to_owned());
    storage::digest(normalized.trim_end_matches('/').as_bytes())
}
fn pin_key(provider: &Provider, kind: &str, id: &str) -> String {
    storage::digest(format!("{}\0{}\0{kind}\0{id}", provider.id, provider.version).as_bytes())
}
fn valid_digest(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}
fn trim_pins(entries: &mut HashMap<String, Pin>, retention: u64) {
    let time = now();
    entries.retain(|_, p| {
        time.saturating_sub(p.updated) < retention && p.updated <= time.saturating_add(60_000)
    });
    while entries.len() >= MAX_PINS {
        if let Some(key) = entries
            .iter()
            .min_by_key(|(_, p)| p.updated)
            .map(|(k, _)| k.clone())
        {
            entries.remove(&key);
        } else {
            break;
        }
    }
}
impl Service {
    pub fn new(path: PathBuf) -> Self {
        let mut pins = Pins::default();
        match storage::read_optional(&path) {
            Ok(None) => (),
            Ok(Some(raw)) if raw.len() <= 2 * 1024 * 1024 => {
                match serde_json::from_slice::<HashMap<String, Pin>>(&raw) {
                    Ok(mut entries)
                        if entries.len() <= MAX_PINS
                            && entries
                                .iter()
                                .all(|(k, p)| valid_digest(k) && valid_digest(&p.endpoint)) =>
                    {
                        trim_pins(&mut entries, PIN_RETENTION_MS);
                        pins.entries = entries;
                    }
                    _ => pins.error = true,
                }
            }
            _ => pins.error = true,
        }
        Self {
            samples: Mutex::new(HashMap::new()),
            flights: Mutex::new(HashMap::new()),
            permits: tokio::sync::Semaphore::new(4),
            pins: Mutex::new(pins),
            write: Mutex::new(()),
            diagnostics: Mutex::new(None),
            responses: Mutex::new(HashMap::new()),
            path,
        }
    }
    pub fn config(provider: &Provider, urls: &[String]) -> String {
        storage::digest(
            &serde_json::to_vec(&(&provider.version, urls)).expect("endpoint configuration"),
        )
    }
    fn fresh<'a>(samples: &'a HashMap<String, Batch>, id: &str, config: &str) -> Option<&'a Batch> {
        samples.get(id).filter(|b| b.config == config)
    }
    pub fn measurements(&self, provider: &Provider, urls: &[String]) -> Vec<Measurement> {
        Self::fresh(
            &super::locks::cache(&self.samples),
            &provider.id,
            &Self::config(provider, urls),
        )
        .map(|b| b.rows.clone())
        .unwrap_or_default()
    }
    pub fn preferred(&self, provider: &Provider, urls: &[String]) -> String {
        self.measurements(provider, urls)
            .iter()
            .filter(|m| m.usable())
            .min_by_key(|m| m.latency_ms)
            .map(|m| m.base_url.clone())
            .unwrap_or_else(|| provider.base_url.clone())
    }
    pub fn due(&self, provider: &Provider, urls: &[String], seconds: u64) -> bool {
        urls.len() > 1
            && Self::fresh(
                &super::locks::cache(&self.samples),
                &provider.id,
                &Self::config(provider, urls),
            )
            .is_none_or(|b| b.checked.elapsed() >= Duration::from_secs(seconds))
    }
    pub fn retain(&self, providers: &[Provider]) {
        let contains = |id: &String| providers.iter().any(|p| &p.id == id);
        super::locks::cache(&self.samples).retain(|id, _| contains(id));
        super::locks::cache(&self.flights).retain(|id, _| contains(id));
    }
    pub fn diagnostics(&self, client: super::ClientId, events: crate::events::Service) {
        *super::locks::published(&self.diagnostics) = Some((client, events));
    }
    fn refresh_failed(&self, provider: &Provider) {
        use crate::events::{Action, Details, ErrorCode, Level, Phase, Reason, Record};
        if let Some((client, events)) = super::locks::published(&self.diagnostics).as_ref() {
            let mut event = Record::new(
                Some(*client),
                Some(&provider.id),
                None,
                Reason::ConfigConflict,
                Action::Stopped,
                None,
                None,
            );
            event.level = Level::Warning;
            event.error_code = Some(ErrorCode::EndpointAffinity);
            event.details = Details {
                local_code: Some(code::ENDPOINT_AFFINITY.into()),
                message: Some("端点归属刷新保存失败，已沿用原归属".into()),
                phase: Some(Phase::Admission),
                counted_failure: Some(false),
                ..Default::default()
            };
            events.emit(event);
        }
    }
    // Called on a blocking worker only when choosing a connection, never per frame.
    fn select(
        &self,
        provider: &Provider,
        urls: &[String],
        thread: Option<&str>,
        previous: Option<&str>,
        fresh: bool,
    ) -> Result<String> {
        let resolve = |hash: &str| {
            urls.iter()
                .find(|url| digest(url) == hash)
                .cloned()
                .ok_or_else(|| {
                    AppError::new(
                        code::ENDPOINT_REMOVED,
                        "线程使用的端点已移除，请恢复该端点或重新开启线程",
                    )
                })
        };
        if let Some(previous) = previous {
            let responses = self.responses.lock().map_err(|_| poisoned_affinity())?;
            if let Some(pin) = responses
                .get(&pin_key(provider, "response", previous))
                .filter(|pin| now().saturating_sub(pin.updated) < 60 * 60 * 1000)
            {
                return resolve(&pin.endpoint);
            }
        }
        if let Some(thread) = thread {
            let key = pin_key(provider, "thread", thread);
            let existing = {
                let pins = self.pins.lock().map_err(|_| poisoned_affinity())?;
                if pins.error {
                    return Err(AppError::new(
                        code::ENDPOINT_AFFINITY,
                        "端点归属记录无法读取，已停止自动切换",
                    ));
                }
                pins.entries.get(&key).cloned()
            };
            if let Some(pin) = existing {
                let url = resolve(&pin.endpoint)?;
                if now().saturating_sub(pin.updated) >= PIN_REFRESH.as_millis() as u64 {
                    self.refresh(&key, provider);
                }
                return Ok(url);
            }
            // Only writers serialize. Existing ownership reads do not wait for
            // another key's fsync, and a failed write cannot poison their path.
            let _writer = self.write.lock().map_err(|_| poisoned_affinity())?;
            let pin = {
                let mut pins = self.pins.lock().map_err(|_| poisoned_affinity())?;
                if let Some(pin) = pins.entries.get(&key) {
                    return resolve(&pin.endpoint);
                }
                let time = now();
                pins.pending
                    .retain(|_, pin| time.saturating_sub(pin.updated) < PIN_RETENTION_MS);
                if let Some(pin) = pins.pending.get(&key) {
                    pin.clone()
                } else {
                    if pins.pending.len() >= MAX_PINS {
                        return Err(AppError::new(
                            code::ENDPOINT_AFFINITY,
                            "待保存端点归属已达上限，未发送请求",
                        ));
                    }
                    let url = if previous.is_none() && fresh {
                        self.preferred(provider, urls)
                    } else {
                        provider.base_url.clone()
                    };
                    let pin = Pin {
                        endpoint: digest(&url),
                        updated: time,
                    };
                    pins.pending.insert(key.clone(), pin.clone());
                    pin
                }
            };
            let url = resolve(&pin.endpoint)?;
            self.persist_pin(&key, pin, true)?;
            return Ok(url);
        }
        Ok(if previous.is_none() {
            self.preferred(provider, urls)
        } else {
            provider.base_url.clone()
        })
    }
    fn refresh(&self, key: &str, provider: &Provider) {
        let Ok(_writer) = self.write.try_lock() else {
            return;
        };
        let pin = {
            let Ok(mut pins) = self.pins.lock() else {
                return;
            };
            let Some(pin) = pins.entries.get(key).cloned() else {
                return;
            };
            if now().saturating_sub(pin.updated) < PIN_REFRESH.as_millis() as u64
                || pins
                    .refresh_attempts
                    .get(key)
                    .is_some_and(|last| last.elapsed() < PIN_REFRESH)
            {
                return;
            }
            pins.refresh_attempts.insert(key.to_owned(), Instant::now());
            Pin {
                updated: now(),
                ..pin
            }
        };
        if self.persist_pin(key, pin, false).is_err() {
            self.refresh_failed(provider);
        }
    }
    /// Caller owns the writer lock. Serialize a borrowed view, release the state
    /// lock for disk I/O, then publish only the successfully persisted changes.
    fn persist_pin(&self, key: &str, pin: Pin, trim: bool) -> Result<()> {
        let (raw, removed) = {
            let pins = self.pins.lock().map_err(|_| poisoned_affinity())?;
            let mut candidate: HashMap<&str, &Pin> = pins
                .entries
                .iter()
                .map(|(key, pin)| (key.as_str(), pin))
                .collect();
            if trim {
                let time = now();
                candidate.retain(|_, pin| {
                    time.saturating_sub(pin.updated) < PIN_RETENTION_MS
                        && pin.updated <= time.saturating_add(60_000)
                });
                while candidate.len() >= MAX_PINS {
                    let oldest = candidate
                        .iter()
                        .min_by_key(|(_, pin)| pin.updated)
                        .map(|(key, _)| *key);
                    if let Some(oldest) = oldest {
                        candidate.remove(oldest);
                    } else {
                        break;
                    }
                }
            }
            candidate.insert(key, &pin);
            let removed: Vec<_> = pins
                .entries
                .keys()
                .filter(|key| !candidate.contains_key(key.as_str()))
                .cloned()
                .collect();
            let raw = serde_json::to_vec(&candidate)
                .map_err(|_| AppError::new(code::ENDPOINT_AFFINITY, "端点归属记录无法保存"))?;
            (raw, removed)
        };
        storage::atomic_write(&self.path, &raw, None).map_err(|_| {
            AppError::new(code::ENDPOINT_AFFINITY, "端点归属记录无法保存，未发送请求")
        })?;
        let mut pins = self.pins.lock().map_err(|_| poisoned_affinity())?;
        for removed in removed {
            pins.entries.remove(&removed);
            pins.refresh_attempts.remove(&removed);
        }
        pins.entries.insert(key.to_owned(), pin);
        pins.pending.remove(key);
        Ok(())
    }
    pub fn remember(&self, provider: &Provider, response: &str) {
        if response.is_empty() || response.len() > 1024 {
            return;
        }
        let Ok(mut pins) = self.responses.lock() else {
            return;
        };
        trim_pins(&mut pins, 60 * 60 * 1000);
        pins.insert(
            pin_key(provider, "response", response),
            Pin {
                endpoint: digest(&provider.base_url),
                updated: now(),
            },
        );
    }
}

async fn request(
    client: &HttpClient,
    url: &str,
) -> std::result::Result<(u16, hyper::body::Incoming), &'static str> {
    let url = url::Url::parse(url).map_err(|_| "INVALID_ENDPOINT")?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("INVALID_ENDPOINT");
    }
    let request = Request::get(url.as_str())
        .header(header::USER_AGENT, "lich13-switch/endpoint-probe")
        .header(header::ACCEPT, "*/*")
        .body(replay::empty())
        .map_err(|_| "INVALID_ENDPOINT")?;
    let response = tokio::time::timeout(PROBE_TIMEOUT, client.request(request))
        .await
        .map_err(|_| "PROBE_TIMEOUT")?
        .map_err(|e| connector::diagnostic_code(&e))?;
    Ok((response.status().as_u16(), response.into_body()))
}
async fn measure(client: HttpClient, base_url: String) -> Measurement {
    // CC Switch uses a warmup GET, then time-to-response-headers. Drain only a
    // small warmup body to reuse a connection, never parse/store response text.
    if let Ok((_, mut body)) = request(&client, &base_url).await {
        let _ = tokio::time::timeout(Duration::from_millis(500), async {
            let mut bytes = 0;
            while let Some(Ok(frame)) = body.frame().await {
                bytes += frame.data_ref().map_or(0, |v| v.len());
                if bytes > 64 * 1024 {
                    break;
                }
            }
        })
        .await;
    }
    let started = Instant::now();
    let result = request(&client, &base_url).await;
    let latency = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
    match result {
        Ok((status, _)) => Measurement {
            base_url,
            latency_ms: Some(latency),
            status: Some(status),
            error: None,
            checked_at: now(),
        },
        Err(code) => Measurement {
            base_url,
            latency_ms: None,
            status: None,
            error: Some(code.into()),
            checked_at: now(),
        },
    }
}
impl Gateway {
    pub(super) fn endpoint_urls(store: &super::Store, provider: &Provider) -> Vec<String> {
        let mut urls = vec![provider.base_url.clone()];
        if let Some(extra) = store.provider_endpoints.get(&provider.id) {
            urls.extend(
                extra
                    .iter()
                    .filter(|url| digest(url) != digest(&provider.base_url))
                    .cloned(),
            );
        }
        urls
    }
    pub(super) async fn endpoint_route(
        &self,
        mut route: Route,
        thread: Option<&str>,
        previous: Option<&str>,
        fresh: bool,
    ) -> Result<Route> {
        let (provider, urls) = {
            let s = self.request_state()?;
            let provider = s
                .store
                .providers
                .iter()
                .find(|p| p.id == route.provider.id && p.version == route.provider.version)
                .ok_or_else(|| AppError::new(code::PROVIDER, "供应商配置已变化"))?
                .clone();
            let urls = Self::endpoint_urls(&s.store, &provider);
            (provider, urls)
        };
        let gateway = self.clone();
        let thread = thread.map(str::to_owned);
        let previous = previous.map(str::to_owned);
        route.provider.base_url = tokio::task::spawn_blocking(move || {
            gateway.0.endpoints.select(
                &provider,
                &urls,
                thread.as_deref(),
                previous.as_deref(),
                fresh,
            )
        })
        .await
        .map_err(|_| AppError::new(code::ENDPOINT_AFFINITY, "端点归属检查失败"))??;
        Ok(route)
    }
    pub async fn probe_endpoints(&self, id: &str, force: bool) -> Result<super::View> {
        if !self
            .request_state()?
            .store
            .providers
            .iter()
            .any(|p| p.id == id)
        {
            return Err(AppError::new(code::PROVIDER, "供应商不存在"));
        }
        let requested = Instant::now();
        let gate = super::locks::cache(&self.0.endpoints.flights)
            .entry(id.to_owned())
            .or_default()
            .clone();
        let _flight = gate.lock().await;
        let (provider, urls, seconds, port, mut stop) = {
            let s = self.request_state()?;
            let p = s
                .store
                .providers
                .iter()
                .find(|p| p.id == id)
                .ok_or_else(|| AppError::new(code::PROVIDER, "供应商不存在"))?
                .clone();
            let urls = Self::endpoint_urls(&s.store, &p);
            (
                p,
                urls,
                s.store.settings.endpoint_probe_seconds,
                self.listener_view().port,
                s.shutdown.as_ref().map(|s| s.subscribe()),
            )
        };
        let config = Service::config(&provider, &urls);
        let recent = Service::fresh(&super::locks::cache(&self.0.endpoints.samples), id, &config)
            .is_some_and(|b| b.checked >= requested);
        if recent || (!force && !self.0.endpoints.due(&provider, &urls, seconds)) {
            return Ok(self.view());
        }
        let client = Client::builder(TokioExecutor::new())
            .pool_max_idle_per_host(1)
            .retry_canceled_requests(false)
            .build(Connector::new(PROBE_TIMEOUT, port).with_ports(self.0.registry.ports.clone()));
        let probes = stream::iter(urls.iter().cloned())
            .map(|url| {
                let client = client.clone();
                async move {
                    let _permit = self
                        .0
                        .endpoints
                        .permits
                        .acquire()
                        .await
                        .expect("probe semaphore stays open");
                    measure(client, url).await
                }
            })
            .buffered(4)
            .collect::<Vec<_>>();
        let stopping = async {
            if let Some(stop) = &mut stop {
                if !*stop.borrow() {
                    let _ = stop.changed().await;
                }
            } else {
                std::future::pending::<()>().await;
            }
        };
        let rows = tokio::select! {
            rows = probes => rows,
            _ = stopping => return Err(AppError::new(code::GATEWAY_STOPPED, "网关已停止，测速已取消")),
        };
        let s = self.request_state()?;
        if let Some(current) = s.store.providers.iter().find(|p| p.id == id) {
            if Service::config(current, &Self::endpoint_urls(&s.store, current)) == config {
                super::locks::cache(&self.0.endpoints.samples).insert(
                    id.to_owned(),
                    Batch {
                        config,
                        checked: Instant::now(),
                        rows,
                    },
                );
            }
        }
        drop(s);
        self.changed();
        Ok(self.view())
    }
    pub async fn probe_due_endpoints(&self) {
        let ids = {
            let Ok(s) = self.request_state() else { return };
            if !s.running {
                return;
            }
            s.store
                .providers
                .iter()
                .filter(|p| {
                    self.0.endpoints.due(
                        p,
                        &Self::endpoint_urls(&s.store, p),
                        s.store.settings.endpoint_probe_seconds,
                    )
                })
                .map(|p| p.id.clone())
                .collect::<Vec<_>>()
        };
        stream::iter(ids)
            .for_each_concurrent(4, |id| async move {
                let _ = self.probe_endpoints(&id, false).await;
            })
            .await;
    }
}

#[cfg(test)]
#[path = "endpoints_io_tests.rs"]
mod io_tests;
#[cfg(test)]
#[path = "endpoints_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "endpoints_poison_tests.rs"]
mod poison_tests;
