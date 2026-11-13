//! RPM send-window ledger and scheduler boundaries.
use super::*;
use crate::gateway::test_support::route;
use crate::gateway::{
    admission::{Budget, Rejected, Scheduler},
    connector::Connector,
    model::Provider,
    replay::{self, full, WireBody},
    routing::Requirement,
    Edit, Gateway, Route,
};
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use http_body_util::BodyExt;
use hyper::{body::Incoming, Request, Response, StatusCode};
use hyper_util::{
    client::legacy::Client,
    rt::{TokioExecutor, TokioIo},
};
use parking_lot::Mutex;
use serde_json::json;
use std::{
    fs,
    path::Path,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, Instant as StdInstant},
};
use tokio::sync::{broadcast, oneshot};

const PROVIDER: &str = "fixture-provider";
const NATIVE_CREATE: &str = r#"{"type":"response.create","model":"fixture-model","input":"fixture","future":{"keep":true}}"#;
const NATIVE_CREATED: &str =
    r#"{"type":"response.created","response":{"id":"fixture-rpm-ws","model":"fixture-model"}}"#;
const NATIVE_COMPLETED: &str = r#"{"type":"response.completed","response":{"id":"fixture-rpm-ws","model":"fixture-model","status":"completed"}}"#;

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

fn scheduler(_path: &Path) -> Scheduler {
    let (events, _) = broadcast::channel(32);
    Scheduler::new(events)
}

async fn fill_failed_rpm_requests(scheduler: &Scheduler, route: &Route, limit: u32) {
    for attempt in 0..limit {
        let mut budget = Budget::new(30);
        let mut admission = scheduler
            .acquire(std::slice::from_ref(route), false, 8, &mut budget)
            .await
            .unwrap_or_else(|error| panic!("RPM request {attempt} should fit: {error:?}"));
        admission.commit_rpm();
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
                                            let mut seen = shared.lock();
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
                                        seen.lock().push((StdInstant::now(), payload));
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
        let state = gateway.0.inner.lock();
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
        let state = gateway.0.inner.lock();
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

#[tokio::test(start_paused = true)]
async fn rolling_budget_refills_only_after_the_oldest_start_expires() {
    let mut ledger = Ledger::default();
    for attempt in 0..7 {
        assert_eq!(
            ledger.gate(PROVIDER, 7),
            None,
            "available rolling-window budget should admit attempt {attempt}"
        );
        ledger.charge(PROVIDER);
    }

    assert_eq!(ledger.gate(PROVIDER, 7), Some(WINDOW));
    assert_eq!(ledger.gate(PROVIDER, 0), None, "zero means unlimited");
    tokio::time::advance(WINDOW - Duration::from_millis(1)).await;
    assert_eq!(ledger.gate(PROVIDER, 7), Some(Duration::from_millis(1)));
    tokio::time::advance(Duration::from_millis(1)).await;
    assert_eq!(ledger.gate(PROVIDER, 7), None);
    assert_eq!(ledger.used(PROVIDER), 0);
}

#[tokio::test(start_paused = true)]
async fn refund_returns_only_the_unsent_start() {
    let mut ledger = Ledger::default();
    let first = ledger.charge(PROVIDER);
    tokio::time::advance(Duration::from_secs(10)).await;
    let second = ledger.charge(PROVIDER);
    assert_eq!(
        ledger.gate(PROVIDER, 2),
        Some(WINDOW - Duration::from_secs(10))
    );

    ledger.refund(PROVIDER, second);
    ledger.refund(PROVIDER, second);
    assert_eq!(ledger.used(PROVIDER), 1);
    assert_eq!(ledger.gate(PROVIDER, 2), None);
    assert_eq!(
        ledger.gate(PROVIDER, 1),
        Some(WINDOW - Duration::from_secs(10))
    );
    ledger.refund(PROVIDER, first);
    assert_eq!(ledger.used(PROVIDER), 0);
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

    assert_eq!(scheduler.rpm_status(PROVIDER, 10).0, 10);
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
    assert_eq!(scheduler.rpm_status(PROVIDER, 10), (0, 0, false));
}

#[tokio::test(start_paused = true)]
async fn rpm_waiters_resume_fifo_when_the_window_clears() {
    let dir = tempfile::tempdir().unwrap();
    let scheduler = scheduler(&dir.path().join("rpm.json"));
    let p = provider("v1", true, 10);
    scheduler.configure(std::slice::from_ref(&p), true);
    let r = route(p);
    fill_failed_rpm_requests(&scheduler, &r, 10).await;
    assert_eq!(scheduler.rpm_status(PROVIDER, 10).0, 10);

    let order = Arc::new(Mutex::new(Vec::new()));
    let mut waiters = Vec::new();
    for index in 1..=2 {
        let scheduler_copy = scheduler.clone();
        let route_copy = r.clone();
        let order = order.clone();
        waiters.push(tokio::spawn(async move {
            let mut budget = Budget::new(90);
            let result = scheduler_copy
                .acquire(std::slice::from_ref(&route_copy), false, 8, &mut budget)
                .await;
            if result.is_ok() {
                order.lock().push(index);
            }
            result
        }));
        wait_for_waiter_count(&scheduler, index).await;
    }

    tokio::time::advance(WINDOW - Duration::from_secs(1)).await;
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
    assert!(waiters.iter().all(|waiter| !waiter.is_finished()));
    assert_eq!(scheduler.counts().1, 2);

    tokio::time::advance(Duration::from_millis(1250)).await;
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }
    assert!(
        waiters.iter().all(|waiter| waiter.is_finished()),
        "both waiters should resume without another RPM interval"
    );
    assert_eq!(*order.lock(), [1, 2]);
    for waiter in waiters {
        let mut admission = waiter.await.unwrap().expect("FIFO admission");
        admission.commit_rpm();
    }
    assert_eq!(scheduler.rpm_status(PROVIDER, 10).0, 2);
}

#[tokio::test(start_paused = true)]
async fn rpm_wait_ends_with_the_waiting_budget() {
    let dir = tempfile::tempdir().unwrap();
    let scheduler = scheduler(&dir.path().join("rpm.json"));
    let p = provider("v1", true, 10);
    scheduler.configure(std::slice::from_ref(&p), true);
    let r = route(p);
    fill_failed_rpm_requests(&scheduler, &r, 10).await;

    let wait_scheduler = scheduler.clone();
    let waiting = tokio::spawn(async move {
        wait_scheduler
            .acquire(std::slice::from_ref(&r), false, 8, &mut Budget::new(30))
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
    assert!(matches!(
        waiting.await.unwrap(),
        Err(Rejected::RateLimited(seconds)) if seconds > 0
    ));
    assert_eq!(scheduler.counts().1, 0);
    assert_eq!(scheduler.rpm_status(PROVIDER, 10).0, 10);
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
    first.commit_rpm();
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
    first.commit_rpm();
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

    let seen = upstream.seen.lock().clone();
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

    let seen = upstream.seen.lock().clone();
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
