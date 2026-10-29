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
        search_only: false,
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
        dsh_models: None,
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

async fn fill_failed_rpm_requests(scheduler: &Scheduler, route: &Route, limit: u32) {
    for attempt in 0..limit {
        let mut budget = Budget::new(30);
        let mut admission = scheduler
            .acquire(std::slice::from_ref(route), false, 8, &mut budget)
            .await
            .unwrap_or_else(|error| panic!("RPM request {attempt} should fit: {error:?}"));
        admission.commit_rpm().await.expect("persisted RPM start");
        let mut completion = admission.take_completion().expect("request completion");
        completion.settle(Settlement::Failed);
        drop(admission);
    }
}

async fn wait_for_waiter_count(scheduler: &Scheduler, expected: usize) {
    for _ in 0..128 {
        if scheduler.counts().1 == expected {
            return;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(scheduler.counts().1, expected);
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
                search_only: None,
                quota_access_token: None,
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
                search_only: None,
                quota_access_token: None,
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
async fn rolling_budget_refills_only_after_the_oldest_start_expires() {
    tokio::time::pause();
    let mut ledger = Ledger::default();
    for attempt in 0..7 {
        assert_eq!(
            ledger.gate(PROVIDER, 7, 0),
            None,
            "available rolling-window budget should admit attempt {attempt}"
        );
        let token = ledger.commit(PROVIDER, 7).expect("start");
        ledger.settle(PROVIDER, &token, Settlement::Failed);
    }

    let (delay, reason) = gate_delay(&ledger, 7);
    assert_eq!(reason, crate::events::RpmWaitReason::SendWindow);
    assert_eq!(delay, WINDOW);
    tokio::time::advance(WINDOW - Duration::from_millis(1)).await;
    let (delay, reason) = gate_delay(&ledger, 7);
    assert_eq!(reason, crate::events::RpmWaitReason::SendWindow);
    assert_eq!(delay, Duration::from_millis(1));
    tokio::time::advance(Duration::from_millis(1)).await;
    ledger.prune();
    assert_eq!(ledger.gate(PROVIDER, 7, 0), None);
}

#[tokio::test]
async fn ten_rpm_rolling_budget_allows_immediate_starts_then_waits_for_61_seconds() {
    tokio::time::pause();
    let mut ledger = Ledger::default();

    for attempt in 0..10 {
        assert_eq!(
            ledger.gate(PROVIDER, 10, 0),
            None,
            "available rolling-window budget should admit attempt {attempt} immediately"
        );
        let token = ledger.commit(PROVIDER, 10).expect("reserved start");
        ledger.settle(PROVIDER, &token, Settlement::Failed);
    }

    let (delay, reason) = gate_delay(&ledger, 10);
    assert_eq!(reason, crate::events::RpmWaitReason::SendWindow);
    assert_eq!(delay, WINDOW);
}

#[tokio::test]
async fn pending_reservations_consume_only_the_remaining_rolling_budget() {
    tokio::time::pause();
    let mut ledger = Ledger::default();

    assert_eq!(ledger.gate(PROVIDER, 10, 9), None);
    let (delay, reason) = gate_delay_with_pending(&ledger, 10, 10);
    assert_eq!(reason, crate::events::RpmWaitReason::SendWindow);
    assert!(!delay.is_zero());
    assert!(delay <= Duration::from_secs(1));

    let first = ledger.commit(PROVIDER, 10).expect("committed start");
    ledger.settle(PROVIDER, &first, Settlement::Failed);
    assert_eq!(ledger.gate(PROVIDER, 10, 8), None);
    assert!(ledger.gate(PROVIDER, 10, 9).is_some());
}

fn gate_delay_with_pending(
    ledger: &Ledger,
    limit: u32,
    pending: usize,
) -> (Duration, crate::events::RpmWaitReason) {
    ledger
        .gate(PROVIDER, limit, pending)
        .expect("pending reservations should fill the budget")
}

#[tokio::test]
async fn canceled_pending_rpm_reservations_release_capacity_without_serializing_the_rest() {
    tokio::time::pause();
    let dir = tempfile::tempdir().unwrap();
    let scheduler = scheduler(&dir.path().join("rpm.json"));
    let p = provider("v1", true, 10);
    scheduler.configure(std::slice::from_ref(&p), true);
    let r = route(p);
    let mut held = Vec::new();

    for attempt in 0..10 {
        let mut budget = Budget::new(1);
        let admission = scheduler
            .acquire_for_immediate(
                std::slice::from_ref(&r),
                false,
                8,
                &mut budget,
                &Requirement::Resource,
                true,
            )
            .await
            .unwrap_or_else(|error| panic!("reservation {attempt} should fit: {error:?}"));
        held.push(admission);
    }

    assert_eq!(scheduler.rpm_status(PROVIDER, 10).0, 0);
    assert!(scheduler.rpm_status(PROVIDER, 10).2);
    let mut blocked_budget = Budget::new(1);
    let blocked = scheduler
        .acquire_for_immediate(
            std::slice::from_ref(&r),
            false,
            8,
            &mut blocked_budget,
            &Requirement::Resource,
            true,
        )
        .await;
    assert!(matches!(blocked, Err(Rejected::RateLimited(_))));

    drop(held.pop().expect("one pending reservation"));
    assert!(!scheduler.rpm_status(PROVIDER, 10).2);
    let mut replacement_budget = Budget::new(1);
    let replacement = scheduler
        .acquire_for_immediate(
            std::slice::from_ref(&r),
            false,
            8,
            &mut replacement_budget,
            &Requirement::Resource,
            true,
        )
        .await
        .expect("canceled reservation should free exactly one slot");
    held.push(replacement);

    drop(held);
    assert_eq!(scheduler.rpm_status(PROVIDER, 10), (0, 0, false, false));
}

#[tokio::test(start_paused = true)]
async fn rpm_wait_outlives_thirty_seconds_and_resumes_two_waiters_fifo_after_61_seconds() {
    let dir = tempfile::tempdir().unwrap();
    let scheduler = scheduler(&dir.path().join("rpm.json"));
    let p = provider("v1", true, 10);
    scheduler.configure(std::slice::from_ref(&p), true);
    let r = route(p);
    fill_failed_rpm_requests(&scheduler, &r, 10).await;
    assert_eq!(scheduler.rpm_status(PROVIDER, 10).0, 10);

    let order = Arc::new(Mutex::new(Vec::new()));
    let first_scheduler = scheduler.clone();
    let first_route = r.clone();
    let first_order = order.clone();
    let first = tokio::spawn(async move {
        let mut budget = Budget::new(30);
        let result = first_scheduler
            .acquire(std::slice::from_ref(&first_route), false, 8, &mut budget)
            .await;
        if result.is_ok() {
            first_order.lock().unwrap().push(1);
        }
        result
    });
    wait_for_waiter_count(&scheduler, 1).await;

    let second_scheduler = scheduler.clone();
    let second_route = r.clone();
    let second_order = order.clone();
    let second = tokio::spawn(async move {
        let mut budget = Budget::new(30);
        let result = second_scheduler
            .acquire(std::slice::from_ref(&second_route), false, 8, &mut budget)
            .await;
        if result.is_ok() {
            second_order.lock().unwrap().push(2);
        }
        result
    });
    wait_for_waiter_count(&scheduler, 2).await;

    tokio::time::advance(Duration::from_secs(31)).await;
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
    assert!(
        !first.is_finished(),
        "RPM wait must outlive the normal queue budget"
    );
    assert!(
        !second.is_finished(),
        "both RPM waiters should remain cancellable"
    );
    assert_eq!(scheduler.counts().1, 2);

    tokio::time::advance(WINDOW - Duration::from_secs(31)).await;
    tokio::time::advance(Duration::from_millis(250)).await;
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }
    assert!(
        first.is_finished(),
        "first waiter should resume when the window clears"
    );
    assert!(
        second.is_finished(),
        "second waiter should resume without another RPM interval"
    );
    assert_eq!(*order.lock().unwrap(), [1, 2]);
    let mut first = first.await.unwrap().expect("FIFO first admission");
    let mut second = second.await.unwrap().expect("FIFO second admission");
    first
        .commit_rpm()
        .await
        .expect("first resumed request should start");
    let mut first_completion = first.take_completion().expect("first request completion");
    first_completion.settle(Settlement::Failed);
    second
        .commit_rpm()
        .await
        .expect("second resumed request should start");
    let mut second_completion = second.take_completion().expect("second request completion");
    second_completion.settle(Settlement::Failed);
    drop((first, second));
    assert_eq!(scheduler.rpm_status(PROVIDER, 10).0, 2);
}

#[tokio::test(start_paused = true)]
async fn rpm_waiters_can_be_canceled_or_stopped_without_consuming_another_start() {
    let dir = tempfile::tempdir().unwrap();
    let scheduler = scheduler(&dir.path().join("rpm.json"));
    let p = provider("v1", true, 10);
    scheduler.configure(std::slice::from_ref(&p), true);
    let r = route(p.clone());
    fill_failed_rpm_requests(&scheduler, &r, 10).await;

    let cancel_scheduler = scheduler.clone();
    let cancel_route = r.clone();
    let canceled = tokio::spawn(async move {
        cancel_scheduler
            .acquire(
                std::slice::from_ref(&cancel_route),
                false,
                8,
                &mut Budget::new(30),
            )
            .await
    });
    wait_for_waiter_count(&scheduler, 1).await;
    canceled.abort();
    assert!(canceled.await.is_err());
    assert_eq!(scheduler.counts().1, 0);

    let stop_scheduler = scheduler.clone();
    let stop_route = r.clone();
    let stopped = tokio::spawn(async move {
        stop_scheduler
            .acquire(
                std::slice::from_ref(&stop_route),
                false,
                8,
                &mut Budget::new(30),
            )
            .await
    });
    wait_for_waiter_count(&scheduler, 1).await;
    scheduler.configure(std::slice::from_ref(&p), false);
    assert!(matches!(stopped.await.unwrap(), Err(Rejected::Stopped)));
    assert_eq!(scheduler.counts().1, 0);
    assert_eq!(scheduler.rpm_status(PROVIDER, 10).0, 10);
}

#[tokio::test(start_paused = true)]
async fn ordinary_concurrency_wait_still_times_out_after_thirty_seconds() {
    let dir = tempfile::tempdir().unwrap();
    let scheduler = scheduler(&dir.path().join("rpm.json"));
    let mut p = provider("v1", true, 0);
    p.max_concurrency = 1;
    scheduler.configure(std::slice::from_ref(&p), true);
    let r = route(p);

    let mut held_budget = Budget::new(30);
    let held = scheduler
        .acquire(std::slice::from_ref(&r), false, 8, &mut held_budget)
        .await
        .expect("first request occupies the only concurrency slot");

    let wait_scheduler = scheduler.clone();
    let wait_route = r.clone();
    let waiting = tokio::spawn(async move {
        wait_scheduler
            .acquire(
                std::slice::from_ref(&wait_route),
                false,
                8,
                &mut Budget::new(30),
            )
            .await
    });
    wait_for_waiter_count(&scheduler, 1).await;

    tokio::time::advance(Duration::from_secs(29)).await;
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
    assert!(!waiting.is_finished());

    tokio::time::advance(Duration::from_secs(1)).await;
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
    assert!(waiting.is_finished());
    assert!(matches!(waiting.await.unwrap(), Err(Rejected::Timeout)));
    assert_eq!(scheduler.counts().1, 0);
    drop(held);
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
async fn long_inflight_stream_keeps_a_completion_slot_until_terminal_then_for_61_seconds() {
    tokio::time::pause();
    let mut ledger = Ledger::default();
    let token = ledger.commit(PROVIDER, 1).expect("long stream start");

    tokio::time::advance(WINDOW).await;
    ledger.prune();

    let (delay, reason) = gate_delay(&ledger, 1);
    assert_eq!(reason, crate::events::RpmWaitReason::CompletionWindow);
    assert_eq!(delay, Duration::from_millis(200));

    tokio::time::advance(WINDOW + Duration::from_secs(1)).await;
    ledger.prune();
    let (delay, reason) = gate_delay(&ledger, 1);
    assert_eq!(reason, crate::events::RpmWaitReason::CompletionWindow);
    assert_eq!(delay, Duration::from_millis(200));

    ledger.settle(PROVIDER, &token, Settlement::Success);
    let (delay, reason) = gate_delay(&ledger, 1);
    assert_eq!(reason, crate::events::RpmWaitReason::CompletionWindow);
    assert_eq!(delay, WINDOW);

    tokio::time::advance(WINDOW - Duration::from_millis(1)).await;
    let (delay, reason) = gate_delay(&ledger, 1);
    assert_eq!(reason, crate::events::RpmWaitReason::CompletionWindow);
    assert_eq!(delay, Duration::from_millis(1));
    tokio::time::advance(Duration::from_millis(1)).await;
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
fn localized_platform_rpm_limits_are_classified_only_in_error_envelopes() {
    let messages = [
        "You have reached the request limit: maximum 10 requests in 1 minutes",
        "You have reached the total request limit: maximum 10 requests in 1 minutes, including failed attempts. Please check that your requests are correct",
        "您已达到请求数限制：1分钟内最多请求10次",
        "您已达到总请求数限制：1分钟内最多请求10次，包括失败次数，请检查您的请求是否正确",
        "您已達到請求數限制：1分鐘內最多請求10次",
        "您已達到總請求數限制：1分鐘內最多請求10次，包括失敗次數，請檢查您的請求是否正確",
    ];
    for message in messages {
        let error = json!({"error": {"message": message}});
        let details = super::super::failure_policy::details_value(&error, true);
        assert_eq!(
            details.error_basis,
            Some(crate::events::ErrorBasis::Rpm),
            "{message}"
        );
    }

    for code in ["GROUP_RPM_EXCEEDED", "USER_RPM_EXCEEDED"] {
        let error = json!({"error": {"code": code}});
        let details = super::super::failure_policy::details_value(&error, true);
        assert_eq!(
            details.error_basis,
            Some(crate::events::ErrorBasis::Rpm),
            "{code}"
        );
    }

    for message in [
        "The upstream is temporarily unavailable.",
        "You have not reached the request limit: maximum 10 requests in 1 minutes",
        "Documentation: You have reached the request limit: maximum 10 requests in 1 minutes",
        "You have reached the request limit: maximum 10 requests in one minute",
        "You have reached the request limit: maximum 0 requests in 1 minutes",
        "You have reached the request limit: maximum 10 requests in 0 minutes",
        "You have reached the request limit: maximum ten requests in 1 minutes",
        "You have reached the request limit: maximum 10 requests in many minutes",
        "You have reached the request limit: maximum 4294967296 requests in 1 minutes",
        "You have reached the request limit: maximum 10 requests in 4294967296 minutes",
    ] {
        let error = json!({"error": {"message": message}});
        let details = super::super::failure_policy::details_value(&error, true);
        assert_ne!(
            details.error_basis,
            Some(crate::events::ErrorBasis::Rpm),
            "{message}"
        );
    }

    let success = json!({"success": true, "message": messages[0]});
    let details = super::super::failure_policy::details_value(&success, true);
    assert_ne!(details.error_basis, Some(crate::events::ErrorBasis::Rpm));
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
    // One send does not fill a two-RPM send window, isolating the initialization guard.
    assert_eq!(
        ledger.gate(PROVIDER, 2, 0).unwrap().1,
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
    admission.commit_rpm().await.expect("unlimited commit");
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
    let path = dir.path().join("rpm.json");
    let scheduler = scheduler(&path);
    let p = provider("v1", true, 1);
    scheduler.configure(std::slice::from_ref(&p), true);
    assert!(!scheduler.rpm_status(PROVIDER, 1).3);
    // Obstruct the destination after a successful load, so this exercises the
    // pre-send save on every platform, not platform-specific ENOTDIR mapping.
    fs::create_dir(&path).unwrap();
    let mut budget = Budget::new(1);
    let mut admission = scheduler
        .acquire_for_immediate(
            &[route(p.clone())],
            false,
            8,
            &mut budget,
            &Requirement::Resource,
            true,
        )
        .await
        .expect("reservation before the first write");
    assert!(matches!(
        admission.commit_rpm().await,
        Err(Rejected::RateLedger)
    ));
    drop(admission);
    assert_eq!(scheduler.rpm_status(PROVIDER, 1).0, 0);
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
    assert_eq!(admission.commit_rpm().await, Err(Rejected::Unavailable));
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
    assert!(matches!(
        admission.commit_rpm().await,
        Err(Rejected::Unavailable)
    ));
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
    first.commit_rpm().await.unwrap();
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
    admission.commit_rpm().await.unwrap();
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
    first.commit_rpm().await.unwrap();
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
async fn http_retry_and_concurrent_requests_use_available_rolling_budget() {
    let upstream = http_loopback(true).await;
    let (dir, gateway) = started_http_gateway(upstream.port, 10, 1).await;
    let body = br#"{"model":"fixture-model","input":["fixture"],"future":{"keep":true}}"#.to_vec();
    let started = StdInstant::now();
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
    assert!(
        times.last().unwrap().duration_since(times[0]) < Duration::from_secs(5),
        "10 RPM should not impose a six-second delay while budget remains: {:?}",
        started.elapsed()
    );
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
    admission.commit_rpm().await.unwrap();
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

    assert!(scheduler.flush_rpm().await);
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
    admission.commit_rpm().await.unwrap();
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

    assert!(scheduler.flush_rpm().await);
    let (ledger, error) = Ledger::load(&path);
    assert!(!error);
    let p = ledger.providers.get(PROVIDER).expect("provider ledger");
    assert!(p.completed.is_empty());
    assert_eq!(p.inflight.len(), 1);
}

#[tokio::test]
async fn native_websocket_two_rounds_reuse_one_handshake_and_preserve_request_bytes() {
    let upstream = native_ws_loopback().await;
    let (dir, gateway) = started_native_gateway(upstream.port, 10).await;
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
    assert!(
        seen[1].0.duration_since(seen[0].0) < Duration::from_secs(5),
        "10 RPM should not impose a six-second delay between turns"
    );
    assert_eq!(gateway.view().providers[0].rpm_used, 2);

    client
        .send(yawc::Frame::close(1000.into(), "fixture done"))
        .await
        .ok();
    drop(client);
    gateway.stop().await.unwrap();
    drop(dir);
}

#[tokio::test]
async fn websocket_cancel_during_rpm_write_does_not_dispatch_and_keeps_connection_usable() {
    let upstream = native_ws_loopback().await;
    let (_dir, gateway) = started_native_gateway(upstream.port, 10).await;
    let mut client = gateway_ws_client(&gateway).await;
    let (ready, release) = gateway.0.admission.hold_rpm_write();
    client
        .send(yawc::Frame::text(NATIVE_CREATE.to_owned()))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), ready)
        .await
        .unwrap()
        .unwrap();
    client
        .send(yawc::Frame::text(
            r#"{"type":"response.cancel"}"#.to_owned(),
        ))
        .await
        .unwrap();
    assert!(gateway_ws_next(&mut client)
        .await
        .as_str()
        .contains("response.cancelled"));
    assert_eq!(upstream.seen.lock().unwrap().len(), 0);
    drop(release);
    assert!(gateway.0.admission.flush_rpm().await);
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
    assert_eq!(upstream.seen.lock().unwrap().len(), 1);
    drop(client);
    gateway.stop().await.unwrap();
}
