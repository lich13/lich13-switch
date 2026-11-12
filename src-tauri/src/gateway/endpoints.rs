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
pub struct Service {
    samples: Mutex<HashMap<String, Batch>>,
    flights: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    permits: tokio::sync::Semaphore,
    pins: Mutex<HashMap<String, Pin>>,
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
        let mut pins = storage::read_optional(&path)
            .ok()
            .flatten()
            .and_then(|raw| serde_json::from_slice::<HashMap<String, Pin>>(&raw).ok())
            .unwrap_or_default();
        trim_pins(&mut pins, PIN_RETENTION_MS);
        Self {
            samples: Mutex::new(HashMap::new()),
            flights: Mutex::new(HashMap::new()),
            permits: tokio::sync::Semaphore::new(4),
            pins: Mutex::new(pins),
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
            &self.samples.lock().unwrap(),
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
        self.wait_until_due(provider, urls, seconds) == Some(Duration::ZERO)
    }
    pub fn wait_until_due(
        &self,
        provider: &Provider,
        urls: &[String],
        seconds: u64,
    ) -> Option<Duration> {
        if urls.len() <= 1 {
            return None;
        }
        let interval = Duration::from_secs(seconds);
        let samples = self.samples.lock().unwrap();
        Some(
            match Self::fresh(&samples, &provider.id, &Self::config(provider, urls)) {
                Some(batch) => interval.saturating_sub(batch.checked.elapsed()),
                None => Duration::ZERO,
            },
        )
    }
    pub fn retain(&self, providers: &[Provider]) {
        let contains = |id: &String| providers.iter().any(|p| &p.id == id);
        self.samples.lock().unwrap().retain(|id, _| contains(id));
        self.flights.lock().unwrap().retain(|id, _| contains(id));
    }
    pub fn diagnostics(&self, client: super::ClientId, events: crate::events::Service) {
        *self.diagnostics.lock().unwrap() = Some((client, events));
    }
    fn refresh_failed(&self, provider: &Provider) {
        use crate::events::{Action, Details, ErrorCode, Level, Phase, Reason, Record};
        if let Some((client, events)) = self.diagnostics.lock().unwrap().as_ref() {
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
            event.error_code = ErrorCode::EndpointAffinity;
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
            let responses = self.responses.lock().unwrap();
            if let Some(pin) = responses
                .get(&pin_key(provider, "response", previous))
                .filter(|pin| now().saturating_sub(pin.updated) < 60 * 60 * 1000)
            {
                return resolve(&pin.endpoint);
            }
        }
        if let Some(thread) = thread {
            let key = pin_key(provider, "thread", thread);
            let mut pins = self.pins.lock().unwrap();
            if let Some(pin) = pins.get_mut(&key) {
                let url = resolve(&pin.endpoint)?;
                let time = now();
                if time.saturating_sub(pin.updated) >= PIN_REFRESH.as_millis() as u64 {
                    pin.updated = time;
                    if Self::write_pins(&self.path, &pins).is_err() {
                        drop(pins);
                        self.refresh_failed(provider);
                    }
                }
                return Ok(url);
            }
            let url = if previous.is_none() && fresh {
                self.preferred(provider, urls)
            } else {
                provider.base_url.clone()
            };
            let pin = Pin {
                endpoint: digest(&url),
                updated: now(),
            };
            let url = resolve(&pin.endpoint)?;
            Self::persist_pin(&self.path, &mut pins, &key, pin)?;
            return Ok(url);
        }
        Ok(if previous.is_none() {
            self.preferred(provider, urls)
        } else {
            provider.base_url.clone()
        })
    }
    fn write_pins(path: &std::path::Path, entries: &HashMap<String, Pin>) -> Result<()> {
        let raw = serde_json::to_vec(entries)
            .map_err(|_| AppError::new(code::ENDPOINT_AFFINITY, "端点归属记录无法保存"))?;
        storage::atomic_write(path, &raw, None)
            .map_err(|_| AppError::new(code::ENDPOINT_AFFINITY, "端点归属记录无法保存，未发送请求"))
    }
    fn persist_pin(
        path: &std::path::Path,
        pins: &mut HashMap<String, Pin>,
        key: &str,
        pin: Pin,
    ) -> Result<()> {
        let time = now();
        let needs_trim = pins.len() >= MAX_PINS
            || pins.values().any(|p| {
                time.saturating_sub(p.updated) >= PIN_RETENTION_MS
                    || p.updated > time.saturating_add(60_000)
            });
        let backup = needs_trim.then(|| pins.clone());
        if needs_trim {
            trim_pins(pins, PIN_RETENTION_MS);
        }
        pins.insert(key.to_owned(), pin);
        if let Err(err) = Self::write_pins(path, pins) {
            if let Some(backup) = backup {
                *pins = backup;
            } else {
                pins.remove(key);
            }
            return Err(err);
        }
        Ok(())
    }
    pub fn remember(&self, provider: &Provider, response: &str) {
        if response.is_empty() || response.len() > 1024 {
            return;
        }
        let mut pins = self.responses.lock().unwrap();
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
        .header(header::USER_AGENT, "vela/endpoint-probe")
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
            let s = self.request_state();
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
            .request_state()
            .store
            .providers
            .iter()
            .any(|p| p.id == id)
        {
            return Err(AppError::new(code::PROVIDER, "供应商不存在"));
        }
        let requested = Instant::now();
        let gate = self
            .0
            .endpoints
            .flights
            .lock()
            .unwrap()
            .entry(id.to_owned())
            .or_default()
            .clone();
        let _flight = gate.lock().await;
        let (provider, urls, seconds, port, mut stop) = {
            let s = self.request_state();
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
                self.try_listener_view()?.port,
                s.shutdown.as_ref().map(|s| s.subscribe()),
            )
        };
        let config = Service::config(&provider, &urls);
        let recent = Service::fresh(&self.0.endpoints.samples.lock().unwrap(), id, &config)
            .is_some_and(|b| b.checked >= requested);
        if recent || (!force && !self.0.endpoints.due(&provider, &urls, seconds)) {
            return self.try_view();
        }
        let client = Client::builder(TokioExecutor::new())
            .pool_max_idle_per_host(1)
            .retry_canceled_requests(false)
            .build(
                Connector::new(PROBE_TIMEOUT, port).with_shared_port(self.0.registry.port.clone()),
            );
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
        let s = self.request_state();
        if let Some(current) = s.store.providers.iter().find(|p| p.id == id) {
            if Service::config(current, &Self::endpoint_urls(&s.store, current)) == config {
                self.0.endpoints.samples.lock().unwrap().insert(
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
        self.try_view()
    }
    pub fn next_endpoint_probe_wait(&self) -> Option<Duration> {
        if self.0.registry.freeze.is_frozen() {
            return None;
        }
        let s = self.request_state();
        if !s.running {
            return None;
        }
        s.store
            .providers
            .iter()
            .filter_map(|p| {
                self.0.endpoints.wait_until_due(
                    p,
                    &Self::endpoint_urls(&s.store, p),
                    s.store.settings.endpoint_probe_seconds,
                )
            })
            .min()
    }
    pub async fn probe_due_endpoints(&self) {
        if self.0.registry.freeze.is_frozen() {
            return;
        }
        let ids = {
            let s = self.request_state();
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
