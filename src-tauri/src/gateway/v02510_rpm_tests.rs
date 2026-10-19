//! v0.25.10 RPM ledger, scheduler and protocol settlement boundaries.
//!
//! This module is registered as a child of `rpm`, so the deterministic ledger
//! fixtures can inspect private stamps without changing the production API.
use super::*;
use crate::gateway::{
    admission::{Budget, Rejected, Scheduler},
    circuit::Circuit,
    connector::Connector,
    model::Provider,
    model_health::Models,
    protocol::Protocol,
    replay::{self, full, WireBody},
    routing::Requirement,
    ClientId, Edit, Gateway, Route,
};
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use http_body_util::BodyExt;
use hyper::{body::Incoming, Request, Response, StatusCode};
use hyper_util::{
    client::legacy::Client,
    rt::{TokioExecutor, TokioIo},
};
use serde_json::json;
use std::{
    fs,
    path::Path,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant as StdInstant},
};
use tokio::sync::{broadcast, oneshot};

const PROVIDER: &str = "fixture-provider";
const NATIVE_CREATE: &str = r#"{"type":"response.create","model":"fixture-model","input":"fixture","future":{"keep":true}}"#;
const NATIVE_CREATED: &str =
    r#"{"type":"response.created","response":{"id":"fixture-rpm-ws","model":"fixture-model"}}"#;
const NATIVE_COMPLETED: &str = r#"{"type":"response.completed","response":{"id":"fixture-rpm-ws","model":"fixture-model","status":"completed"}}"#;

fn wall_now() -> i64 {
    super::super::admission::now_wall_ms()
}

fn provider(version: &str, queued: bool, max_rpm: u32) -> Provider {
    Provider {
        id: PROVIDER.into(),
        name: PROVIDER.into(),
        base_url: "https://fixture.example.invalid/v1".into(),
        token: "fixture-token".into(),
        queued,
        version: version.into(),
        max_concurrency: 0,
        max_rpm,
        allowed_models: None,
        handoff_after_compaction: false,
        take_new_threads: false,
    }
}

fn provider_with_models(version: &str, max_rpm: u32) -> Provider {
    Provider {
        allowed_models: Some(vec!["fixture-model-a".into(), "fixture-model-b".into()]),
        ..provider(version, true, max_rpm)
    }
}

fn route(provider: Provider) -> Route {
    let client =
        Client::builder(TokioExecutor::new()).build(Connector::new(Duration::from_secs(1), 0));
    Route {
        grok: None,
        client_id: ClientId::Codex,
        provider,
        client,
        provider_circuit: Circuit::default(),
        model_circuits: Models::default(),
    }
}

fn scheduler(path: &Path) -> Scheduler {
    let (events, _) = broadcast::channel(32);
    Scheduler::new(events, path.to_path_buf())
}

fn gate_delay(ledger: &Ledger, limit: u32) -> (Duration, crate::events::RpmWaitReason) {
    ledger
        .gate(PROVIDER, limit, 0)
        .expect("fixture should be gated")
}

type ObservedRequests = Arc<Mutex<Vec<(StdInstant, Vec<u8>)>>>;

struct HttpLoopback {
    port: u16,
    seen: ObservedRequests,
    stop: Option<oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for HttpLoopback {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

async fn http_loopback(fail_first: bool) -> HttpLoopback {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let shared = seen.clone();
    let (stop, mut stopped) = oneshot::channel();
    let task = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut stopped => break,
                accepted = listener.accept() => {
                    let Ok((stream, _)) = accepted else { break };
                    let shared = shared.clone();
                    tokio::spawn(async move {
                        let _ = hyper::server::conn::http1::Builder::new()
                            .serve_connection(
                                hyper_util::rt::TokioIo::new(stream),
                                hyper::service::service_fn(move |request: Request<Incoming>| {
                                    let shared = shared.clone();
                                    async move {
                                        let body = request
                                            .into_body()
                                            .collect()
                                            .await
                                            .map(|bytes| bytes.to_bytes().to_vec())
                                            .unwrap_or_default();
                                        let attempt = {
                                            let mut seen = shared.lock().unwrap();
                                            let attempt = seen.len();
                                            seen.push((StdInstant::now(), body));
                                            attempt
                                        };
                                        let status = if fail_first && attempt == 0 {
                                            StatusCode::TOO_MANY_REQUESTS
                                        } else {
                                            StatusCode::OK
                                        };
                                        let body = if status == StatusCode::OK {
                                            Bytes::from_static(br#"{"ok":true}"#)
                                        } else {
                                            Bytes::from_static(b"Selected model is at capacity")
                                        };
                                        Ok::<_, std::convert::Infallible>(
                                            Response::builder()
                                                .status(status)
                                                .header("content-type", "application/json")
                                                .body(full(body))
                                                .unwrap(),
                                        )
                                    }
                                }),
                            )
                            .await;
                    });
                }
            }
        }
    });
    HttpLoopback {
        port,
        seen,
        stop: Some(stop),
        task: Some(task),
    }
}

struct NativeWsLoopback {
    port: u16,
    connections: Arc<AtomicUsize>,
    seen: ObservedRequests,
    stop: Option<oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for NativeWsLoopback {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

async fn native_ws_loopback() -> NativeWsLoopback {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let connections = Arc::new(AtomicUsize::new(0));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let shared_connections = connections.clone();
    let shared_seen = seen.clone();
    let (stop, mut stopped) = oneshot::channel();
    let task = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut stopped => break,
                accepted = listener.accept() => {
                    let Ok((stream, _)) = accepted else { break };
                    let connections = shared_connections.clone();
                    let seen = shared_seen.clone();
                    tokio::spawn(async move {
                        let service = hyper::service::service_fn(move |mut request: Request<Incoming>| {
                            let connections = connections.clone();
                            let seen = seen.clone();
                            async move {
                                let Ok((response, upgrade)) = yawc::WebSocket::upgrade_with_options(
                                    &mut request,
                                    yawc::Options::default().with_balanced_compression(),
                                ) else {
                                    return Ok::<_, std::convert::Infallible>(
                                        Response::builder()
                                            .status(StatusCode::BAD_REQUEST)
                                            .body(full(Bytes::new()))
                                            .unwrap(),
                                    );
                                };
                                connections.fetch_add(1, Ordering::SeqCst);
                                tokio::spawn(async move {
                                    let Ok(mut socket) = upgrade.await else { return };
                                    while let Some(frame) = socket.next().await {
                                        if frame.opcode() == yawc::OpCode::Close {
                                            break;
                                        }
                                        if frame.opcode().is_control() {
                                            continue;
                                        }
                                        let payload = frame.payload().to_vec();
                                        let is_create = serde_json::from_slice::<serde_json::Value>(&payload)
                                            .ok()
                                            .is_some_and(|value| value["type"] == "response.create");
                                        if !is_create {
                                            continue;
                                        }
                                        seen.lock().unwrap().push((StdInstant::now(), payload));
                                        if socket.send(yawc::Frame::text(NATIVE_CREATED.to_owned())).await.is_err() {
                                            break;
                                        }
                                        if socket.send(yawc::Frame::text(NATIVE_COMPLETED.to_owned())).await.is_err() {
                                            break;
                                        }
                                    }
                                });
                                Ok::<_, std::convert::Infallible>(response.map(|_| replay::empty()))
                            }
                        });
                        let _ = hyper::server::conn::http1::Builder::new()
                            .serve_connection(TokioIo::new(stream), service)
                            .with_upgrades()
                            .await;
                    });
                }
            }
        }
    });
    NativeWsLoopback {
        port,
        connections,
        seen,
        stop: Some(stop),
        task: Some(task),
    }
}

fn unused_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

async fn started_http_gateway(
    upstream_port: u16,
    max_rpm: u32,
    max_retries: usize,
) -> (tempfile::TempDir, Gateway) {
    let dir = tempfile::tempdir().unwrap();
    let base_url = format!("http://127.0.0.1:{upstream_port}/v1");
    fs::write(
        dir.path().join("config.toml"),
        format!(
            "model_provider='custom'\n[model_providers.custom]\nbase_url = {}\nexperimental_bearer_token = \"fixture-token\"\nwire_api='responses'\nsupports_websockets=false\n",
            serde_json::to_string(&base_url).unwrap()
        ),
    )
    .unwrap();
    let gateway = Gateway::new(dir.path().to_path_buf()).unwrap();
    let mut settings = gateway.view().settings;
    settings.port = unused_port();
    settings.max_retries = max_retries;
    settings.queue_seconds = 5;
    settings.capacity_retry_seconds = 1;
    settings.rate_limit_seconds = 1;
    gateway
        .edit(
            Edit::Settings { settings },
            &gateway.view().revision,
            dir.path(),
        )
        .unwrap();
    gateway
        .edit(
            Edit::SaveProvider {
                id: None,
                base_url,
                token: "fixture-token".into(),
                name: Some("fixture-rpm".into()),
            },
            &gateway.view().revision,
            dir.path(),
        )
        .unwrap();
    let id = gateway.view().providers[0].id.clone();
    gateway
        .edit(
            Edit::RpmProvider { id, max_rpm },
            &gateway.view().revision,
            dir.path(),
        )
        .unwrap();
    gateway
        .edit(
            Edit::Mode {
                mode: "auto".into(),
            },
            &gateway.view().revision,
            dir.path(),
        )
        .unwrap();
    gateway
        .start(&gateway.view().revision, dir.path())
        .await
        .unwrap();
    (dir, gateway)
}

async fn started_native_gateway(upstream_port: u16, max_rpm: u32) -> (tempfile::TempDir, Gateway) {
    let dir = tempfile::tempdir().unwrap();
    let base_url = format!("http://127.0.0.1:{upstream_port}");
    fs::write(
        dir.path().join("config.toml"),
        format!(
            "model_provider='custom'\n[model_providers.custom]\nbase_url = {}\nexperimental_bearer_token = \"fixture-token\"\nwire_api='responses'\nsupports_websockets=true\n",
            serde_json::to_string(&base_url).unwrap()
        ),
    )
    .unwrap();
    let gateway = Gateway::new(dir.path().to_path_buf()).unwrap();
    let mut settings = gateway.view().settings;
    settings.port = unused_port();
    settings.max_retries = 0;
    settings.queue_seconds = 5;
    settings.websocket_retry_seconds = 1;
    settings.first_byte_seconds = 3;
    settings.idle_seconds = 3;
    settings.total_seconds = 10;
    settings.connect_seconds = 2;
    gateway
        .edit(
            Edit::Settings { settings },
            &gateway.view().revision,
            dir.path(),
        )
        .unwrap();
    gateway
        .edit(
            Edit::SaveProvider {
                id: None,
                base_url,
                token: "fixture-token".into(),
                name: Some("fixture-rpm-ws".into()),
            },
            &gateway.view().revision,
            dir.path(),
        )
        .unwrap();
    let id = gateway.view().providers[0].id.clone();
    gateway
        .edit(
            Edit::RpmProvider { id, max_rpm },
            &gateway.view().revision,
            dir.path(),
        )
        .unwrap();
    gateway
        .edit(
            Edit::Mode {
                mode: "auto".into(),
            },
            &gateway.view().revision,
            dir.path(),
        )
        .unwrap();
    gateway
        .start(&gateway.view().revision, dir.path())
        .await
        .unwrap();
    (dir, gateway)
}

async fn gateway_http_request(gateway: &Gateway, body: Vec<u8>) -> (StatusCode, Bytes) {
    let client: Client<Connector, WireBody> =
        Client::builder(TokioExecutor::new()).build(Connector::new(Duration::from_secs(2), 0));
    let (token, port) = {
        let state = gateway.0.inner.lock().unwrap();
        (state.store.local_token.clone(), state.store.settings.port)
    };
    let request = Request::builder()
        .method("POST")
        .uri(format!("http://127.0.0.1:{port}/v1/responses"))
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(full(body))
        .unwrap();
    let response = client.request(request).await.unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, body)
}

async fn gateway_ws_client(gateway: &Gateway) -> yawc::TcpWebSocket {
    let (token, port) = {
        let state = gateway.0.inner.lock().unwrap();
        (state.store.local_token.clone(), state.store.settings.port)
    };
    yawc::WebSocket::connect(
        format!("ws://127.0.0.1:{port}/v1/responses?future=keep")
            .parse()
            .unwrap(),
    )
    .with_options(yawc::Options::default().with_balanced_compression())
    .with_request(yawc::HttpRequest::builder().header("authorization", format!("Bearer {token}")))
    .await
    .unwrap()
}

async fn gateway_ws_next(ws: &mut yawc::TcpWebSocket) -> yawc::Frame {
    loop {
        let frame = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("fixture downstream WebSocket timed out")
            .expect("fixture downstream WebSocket closed");
        if frame.opcode().is_control() && frame.opcode() != yawc::OpCode::Close {
            continue;
        }
        return frame;
    }
}

#[tokio::test]
async fn pacing_is_immediate_then_ceil_60_seconds_over_n_and_does_not_burst_after_idle() {
    tokio::time::pause();
    let mut ledger = Ledger::default();
    assert_eq!(ledger.gate(PROVIDER, 7, 0), None);
    let token = ledger.commit(PROVIDER, 7).expect("first start");
    ledger.settle(PROVIDER, &token, Settlement::Success);

    let expected = Duration::from_millis(60_000_u64.div_ceil(7));
    let (delay, reason) = gate_delay(&ledger, 7);
    assert_eq!(reason, crate::events::RpmWaitReason::Pacing);
    assert_eq!(delay, expected);

    tokio::time::advance(WINDOW + Duration::from_millis(1)).await;
    ledger.prune();
    assert_eq!(ledger.gate(PROVIDER, 7, 0), None);
    let second = ledger.commit(PROVIDER, 7).expect("post-window start");
    ledger.settle(PROVIDER, &second, Settlement::Success);
    let (delay, reason) = gate_delay(&ledger, 7);
    assert_eq!(reason, crate::events::RpmWaitReason::Pacing);
    assert_eq!(delay, expected);
}

#[tokio::test]
async fn send_window_expires_at_61_seconds_and_completion_and_inflight_are_independent() {
    tokio::time::pause();
    let mut ledger = Ledger::default();
    let token = ledger.commit(PROVIDER, 1).expect("start");
    ledger.settle(PROVIDER, &token, Settlement::Success);

    tokio::time::advance(Duration::from_secs(60) + Duration::from_millis(999)).await;
    let (delay, reason) = gate_delay(&ledger, 1);
    assert_eq!(reason, crate::events::RpmWaitReason::SendWindow);
    assert_eq!(delay, Duration::from_millis(1));
    tokio::time::advance(Duration::from_millis(1)).await;
    assert_eq!(ledger.gate(PROVIDER, 1, 0), None);

    let token = ledger.commit(PROVIDER, 1).expect("inflight start");
    let p = ledger.providers.get_mut(PROVIDER).unwrap();
    p.starts.clear();
    p.last = None;
    let (delay, reason) = gate_delay(&ledger, 1);
    assert_eq!(reason, crate::events::RpmWaitReason::CompletionWindow);
    assert_eq!(delay, Duration::from_millis(200));
    ledger.settle(PROVIDER, &token, Settlement::Uncertain);
    let (delay, reason) = gate_delay(&ledger, 1);
    assert_eq!(reason, crate::events::RpmWaitReason::CompletionWindow);
    assert_eq!(delay, WINDOW);

    let success = ledger.commit(PROVIDER, 1).expect("completed start");
    let p = ledger.providers.get_mut(PROVIDER).unwrap();
    p.starts.clear();
    p.last = None;
    ledger.settle(PROVIDER, &success, Settlement::Success);
    let (delay, reason) = gate_delay(&ledger, 1);
    assert_eq!(reason, crate::events::RpmWaitReason::CompletionWindow);
    assert_eq!(delay, WINDOW);
}

#[tokio::test]
async fn failed_settlement_keeps_send_start_but_releases_completion_slot() {
    tokio::time::pause();
    let mut ledger = Ledger::default();
    let token = ledger.commit(PROVIDER, 1).expect("start");
    ledger.settle(PROVIDER, &token, Settlement::Failed);
    let p = ledger.providers.get(PROVIDER).expect("provider ledger");
    assert_eq!(p.starts.len(), 1);
    assert!(p.completed.is_empty());
    assert!(p.inflight.is_empty());
    let (delay, reason) = gate_delay(&ledger, 1);
    assert_eq!(reason, crate::events::RpmWaitReason::SendWindow);
    assert_eq!(delay, WINDOW);
}

#[tokio::test]
async fn unknown_end_is_protected_for_61_seconds_then_pruned() {
    tokio::time::pause();
    let mut ledger = Ledger::default();
    let token = ledger.commit(PROVIDER, 1).expect("start");
    ledger.providers.get_mut(PROVIDER).unwrap().starts.clear();
    ledger.settle(PROVIDER, &token, Settlement::Uncertain);
    let (delay, reason) = gate_delay(&ledger, 1);
    assert_eq!(reason, crate::events::RpmWaitReason::CompletionWindow);
    assert_eq!(delay, WINDOW);
    tokio::time::advance(WINDOW).await;
    ledger.prune();
    assert_eq!(ledger.gate(PROVIDER, 1, 0), None);
}

#[tokio::test]
async fn out_of_order_long_stream_settlement_is_token_scoped_and_leak_free() {
    tokio::time::pause();
    let mut ledger = Ledger::default();
    let mut tokens = Vec::new();
    for _ in 0..128 {
        tokens.push(ledger.commit(PROVIDER, 128).expect("stream start"));
        tokio::time::advance(Duration::from_millis(1)).await;
    }
    for (index, token) in tokens.iter().enumerate().rev() {
        ledger.settle(
            PROVIDER,
            token,
            if index % 3 == 0 {
                Settlement::Failed
            } else {
                Settlement::Success
            },
        );
    }
    let p = ledger.providers.get(PROVIDER).expect("provider ledger");
    assert_eq!(p.starts.len(), 128);
    assert_eq!(p.completed.len(), 128 - 43);
    assert!(p.inflight.is_empty());
}

#[tokio::test]
async fn backoff_keeps_the_longest_deadline_and_is_shared_by_provider_models() {
    tokio::time::pause();
    let mut ledger = Ledger::default();
    ledger.backoff(PROVIDER, Duration::from_secs(5));
    ledger.backoff(PROVIDER, Duration::from_secs(2));
    let (delay, reason) = gate_delay(&ledger, 0);
    assert_eq!(reason, crate::events::RpmWaitReason::UpstreamBackoff);
    assert_eq!(delay, Duration::from_secs(5));
    ledger.backoff(PROVIDER, Duration::from_secs(90_000));
    let (delay, reason) = gate_delay(&ledger, 0);
    assert_eq!(reason, crate::events::RpmWaitReason::UpstreamBackoff);
    assert_eq!(delay, Duration::from_secs(86_400));
}

#[test]
fn legacy_ledger_preserves_starts_and_adds_a_61_second_initialization_guard() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rpm.json");
    let now = wall_now();
    fs::write(
        &path,
        serde_json::to_vec(&json!({
            "version": 1,
            "providers": {PROVIDER: [now]},
        }))
        .unwrap(),
    )
    .unwrap();
    let (ledger, error) = Ledger::load(&path);
    assert!(!error);
    assert_eq!(ledger.used(PROVIDER), 1);
    let init = ledger.legacy_until.expect("legacy initialization guard");
    assert!(init.at > tokio::time::Instant::now());
    assert!(init.at.duration_since(tokio::time::Instant::now()) <= WINDOW);
    assert_eq!(
        ledger.gate(PROVIDER, 1, 0).unwrap().1,
        crate::events::RpmWaitReason::CompletionWindow
    );
}

#[test]
fn recovery_preserves_crash_inflight_and_clamps_future_wall_clock_starts() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rpm.json");
    let now = wall_now();
    fs::write(
        &path,
        serde_json::to_vec(&json!({
            "version": 2,
            "providers": {PROVIDER: [now + 3_600_000]},
            "protection": {PROVIDER: {"inflight": {"crashed": null}}},
        }))
        .unwrap(),
    )
    .unwrap();
    let (mut ledger, error) = Ledger::load(&path);
    assert!(!error);
    assert_eq!(ledger.used(PROVIDER), 1);
    let p = ledger.providers.get_mut(PROVIDER).expect("provider ledger");
    assert_eq!(p.inflight.len(), 1);
    p.starts.clear();
    p.last = None;
    let (delay, reason) = gate_delay(&ledger, 1);
    assert_eq!(reason, crate::events::RpmWaitReason::CompletionWindow);
    assert!(delay <= WINDOW);
    assert!(delay >= WINDOW - Duration::from_secs(1));
}

#[tokio::test]
async fn unlimited_to_limited_revalidates_existing_start_and_enables_initialization_guard() {
    tokio::time::pause();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rpm.json");
    let scheduler = scheduler(&path);
    let unlimited = provider("v1", true, 0);
    scheduler.configure(std::slice::from_ref(&unlimited), true);
    let mut budget = Budget::new(1);
    let mut admission = scheduler
        .acquire(&[route(unlimited.clone())], false, 8, &mut budget)
        .await
        .expect("unlimited request admission");
    admission.commit_rpm().expect("unlimited commit");
    drop(admission);

    let limited = provider("v1", true, 1);
    scheduler.configure(std::slice::from_ref(&limited), true);
    let (_, retry, limited_now, error) = scheduler.rpm_status(PROVIDER, 1);
    assert!(limited_now);
    assert!(!error);
    assert!(retry >= WINDOW.as_secs());
}

#[tokio::test]
async fn persistence_failure_fails_closed_for_limited_rpm() {
    tokio::time::pause();
    let dir = tempfile::tempdir().unwrap();
    let parent_file = dir.path().join("parent-file");
    fs::write(&parent_file, b"fixture").unwrap();
    let path = parent_file.join("rpm.json");
    let scheduler = scheduler(&path);
    let p = provider("v1", true, 1);
    scheduler.configure(std::slice::from_ref(&p), true);
    let (_, _, _, error) = scheduler.rpm_status(PROVIDER, 1);
    assert!(error);
    let mut budget = Budget::new(1);
    let result = scheduler
        .acquire_for_immediate(
            &[route(p)],
            false,
            8,
            &mut budget,
            &Requirement::Resource,
            true,
        )
        .await;
    assert!(matches!(result, Err(Rejected::RateLedger)));
}

#[tokio::test]
async fn provider_credential_version_change_rejects_stale_uncommitted_reservation() {
    tokio::time::pause();
    let dir = tempfile::tempdir().unwrap();
    let scheduler = scheduler(&dir.path().join("rpm.json"));
    let original = provider("v1", true, 1);
    scheduler.configure(std::slice::from_ref(&original), true);
    let mut budget = Budget::new(1);
    let mut admission = scheduler
        .acquire(&[route(original)], false, 8, &mut budget)
        .await
        .expect("stale reservation admission");
    scheduler.configure(&[provider("v2", true, 1)], true);
    assert_eq!(admission.commit_rpm(), Err(Rejected::Unavailable));
    assert_eq!(scheduler.rpm_status(PROVIDER, 1).0, 0);
}

#[tokio::test]
async fn provider_dequeue_rejects_stale_uncommitted_reservation() {
    tokio::time::pause();
    let dir = tempfile::tempdir().unwrap();
    let scheduler = scheduler(&dir.path().join("rpm.json"));
    let original = provider("v1", true, 1);
    scheduler.configure(std::slice::from_ref(&original), true);
    let mut budget = Budget::new(1);
    let mut admission = scheduler
        .acquire(&[route(original)], false, 8, &mut budget)
        .await
        .expect("dequeued reservation admission");
    scheduler.configure(&[provider("v1", false, 1)], true);
    assert!(matches!(admission.commit_rpm(), Err(Rejected::Unavailable)));
    assert_eq!(scheduler.rpm_status(PROVIDER, 1).0, 0);
}

#[tokio::test]
async fn rpm_is_shared_across_models_of_one_provider() {
    tokio::time::pause();
    let dir = tempfile::tempdir().unwrap();
    let scheduler = scheduler(&dir.path().join("rpm.json"));
    let p = provider_with_models("v1", 1);
    scheduler.configure(std::slice::from_ref(&p), true);
    let mut first_budget = Budget::new(1);
    let mut first = scheduler
        .acquire_for(
            &[route(p.clone())],
            false,
            8,
            &mut first_budget,
            &Requirement::Model("fixture-model-a".into()),
        )
        .await
        .expect("first model admission");
    first.commit_rpm().unwrap();
    let mut completion = first.take_completion().expect("first completion");
    completion.settle(Settlement::Failed);
    drop(first);

    let mut second_budget = Budget::new(1);
    let result = scheduler
        .acquire_for_immediate(
            &[route(p)],
            false,
            8,
            &mut second_budget,
            &Requirement::Model("fixture-model-b".into()),
            true,
        )
        .await;
    assert!(matches!(result, Err(Rejected::RateLimited(seconds)) if seconds >= 60));
}

#[tokio::test]
async fn protocol_failure_releases_completion_but_keeps_send_start_and_output_bytes() {
    tokio::time::pause();
    let dir = tempfile::tempdir().unwrap();
    let scheduler = scheduler(&dir.path().join("rpm.json"));
    let p = provider("v1", true, 1);
    scheduler.configure(std::slice::from_ref(&p), true);
    let mut budget = Budget::new(1);
    let mut admission = scheduler
        .acquire(&[route(p)], false, 8, &mut budget)
        .await
        .expect("protocol admission");
    admission.commit_rpm().unwrap();
    let completion = admission.take_completion();
    drop(admission);

    let mut protocol = Protocol::new(false);
    protocol.attach_rpm(completion);
    protocol.delivery().handed(17, true);
    protocol.finish(Some(500), "HTTP");
    assert_eq!(
        protocol
            .delivery()
            .bytes
            .load(std::sync::atomic::Ordering::Relaxed),
        17
    );
    let (used, retry, limited, error) = scheduler.rpm_status(PROVIDER, 1);
    assert_eq!(used, 1);
    assert!(limited);
    assert!(retry >= WINDOW.as_secs());
    assert!(!error);
}

#[tokio::test]
async fn queued_waiter_cancellation_releases_fifo_slot() {
    tokio::time::pause();
    let dir = tempfile::tempdir().unwrap();
    let scheduler = scheduler(&dir.path().join("rpm.json"));
    let p = provider("v1", true, 1);
    scheduler.configure(std::slice::from_ref(&p), true);
    let r = route(p);
    let mut first_budget = Budget::new(1);
    let mut first = scheduler
        .acquire(std::slice::from_ref(&r), false, 8, &mut first_budget)
        .await
        .expect("first admission");
    first.commit_rpm().unwrap();
    let mut completion = first.take_completion().unwrap();
    completion.settle(Settlement::Failed);
    drop(first);

    let wait_scheduler = scheduler.clone();
    let wait_route = r.clone();
    let waiter = tokio::spawn(async move {
        let mut budget = Budget::new(30);
        wait_scheduler
            .acquire(&[wait_route], false, 8, &mut budget)
            .await
    });
    tokio::task::yield_now().await;
    assert_eq!(scheduler.counts().1, 1);
    waiter.abort();
    let _ = waiter.await;
    tokio::task::yield_now().await;
    assert_eq!(scheduler.counts().1, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_concurrent_and_retry_attempts_are_paced_and_preserve_request_bytes() {
    let upstream = http_loopback(true).await;
    let (dir, gateway) = started_http_gateway(upstream.port, 600, 1).await;
    let body = br#"{"model":"fixture-model","input":["fixture"],"future":{"keep":true}}"#.to_vec();
    let first = gateway_http_request(&gateway, body.clone());
    let second = gateway_http_request(&gateway, body.clone());
    let (first, second) = tokio::join!(first, second);
    assert_eq!(first.0, StatusCode::OK);
    assert_eq!(second.0, StatusCode::OK);
    assert_eq!(first.1, Bytes::from_static(br#"{"ok":true}"#));
    assert_eq!(second.1, Bytes::from_static(br#"{"ok":true}"#));

    let seen = upstream.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 3, "one retry plus the concurrent request");
    assert!(seen.iter().all(|(_, bytes)| bytes == &body));
    let mut times: Vec<_> = seen.iter().map(|(at, _)| *at).collect();
    times.sort();
    for pair in times.windows(2) {
        assert!(
            pair[1].duration_since(pair[0]) >= Duration::from_millis(70),
            "RPM 600 should pace attempts near 100ms: {:?}",
            pair[1].duration_since(pair[0])
        );
    }
    assert_eq!(gateway.view().providers[0].rpm_used, 3);
    gateway.stop().await.unwrap();
    drop(dir);
}

#[tokio::test]
async fn completion_window_settles_success_once_across_cancellation_tail() {
    tokio::time::pause();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rpm.json");
    let scheduler = scheduler(&path);
    let p = provider("v1", true, 1);
    scheduler.configure(std::slice::from_ref(&p), true);
    let mut budget = Budget::new(1);
    let mut admission = scheduler
        .acquire(&[route(p)], false, 8, &mut budget)
        .await
        .expect("completion fixture admission");
    admission.commit_rpm().unwrap();
    let completion = admission.take_completion();
    drop(admission);

    let mut protocol = Protocol::new(true);
    protocol.attach_rpm(completion);
    protocol.response(200, true, "identity");
    protocol.downstream_closed(&crate::gateway::wire_progress::Activity::default(), false);
    protocol.begin_usage_tail(false);
    protocol.feed(b"data: {\"type\":\"response.completed\",\"response\":{\"id\":\"fixture\",\"status\":\"completed\",\"usage\":{\"input_tokens\":2,\"output_tokens\":3}}}\n\n");
    assert!(protocol.terminal_observed());
    assert!(protocol.succeeded());
    assert!(!protocol.delivered_success());
    protocol.finish_usage_tail("upstream_eof");
    protocol.finish_usage_tail("upstream_eof");

    let (ledger, error) = Ledger::load(&path);
    assert!(!error);
    let p = ledger.providers.get(PROVIDER).expect("provider ledger");
    assert_eq!(p.completed.len(), 1);
    assert!(p.inflight.is_empty());
}

#[tokio::test]
async fn http_200_without_a_terminal_event_does_not_settle_as_success() {
    tokio::time::pause();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rpm.json");
    let scheduler = scheduler(&path);
    let p = provider("v1", true, 1);
    scheduler.configure(std::slice::from_ref(&p), true);
    let mut budget = Budget::new(1);
    let mut admission = scheduler
        .acquire(&[route(p)], false, 8, &mut budget)
        .await
        .expect("incomplete fixture admission");
    admission.commit_rpm().unwrap();
    let completion = admission.take_completion();
    drop(admission);

    let mut protocol = Protocol::new(true);
    protocol.attach_rpm(completion);
    protocol.response(200, true, "identity");
    protocol.feed(
        br#"event: response.created
data: {"type":"response.created","response":{"id":"fixture"}}

"#,
    );
    protocol.finish(Some(200), crate::errors::code::OK);
    assert!(!protocol.succeeded());
    assert_eq!(
        protocol.terminal(),
        Some(crate::gateway::protocol::Terminal::Failure)
    );

    let (ledger, error) = Ledger::load(&path);
    assert!(!error);
    let p = ledger.providers.get(PROVIDER).expect("provider ledger");
    assert!(p.completed.is_empty());
    assert_eq!(p.inflight.len(), 1);
}

#[tokio::test]
async fn native_websocket_two_rounds_reuse_one_handshake_and_preserve_request_bytes() {
    let upstream = native_ws_loopback().await;
    let (dir, gateway) = started_native_gateway(upstream.port, 600).await;
    let mut client = gateway_ws_client(&gateway).await;

    for _ in 0..2 {
        client
            .send(yawc::Frame::text(NATIVE_CREATE.to_owned()))
            .await
            .unwrap();
        assert!(gateway_ws_next(&mut client)
            .await
            .as_str()
            .contains("response.created"));
        assert!(gateway_ws_next(&mut client)
            .await
            .as_str()
            .contains("response.completed"));
    }

    let seen = upstream.seen.lock().unwrap().clone();
    assert_eq!(upstream.connections.load(Ordering::SeqCst), 1);
    assert_eq!(seen.len(), 2);
    assert!(seen
        .iter()
        .all(|(_, payload)| payload == NATIVE_CREATE.as_bytes()));
    assert!(seen[1].0.duration_since(seen[0].0) >= Duration::from_millis(70));
    assert_eq!(gateway.view().providers[0].rpm_used, 2);

    client
        .send(yawc::Frame::close(1000.into(), "fixture done"))
        .await
        .ok();
    drop(client);
    gateway.stop().await.unwrap();
    drop(dir);
}
