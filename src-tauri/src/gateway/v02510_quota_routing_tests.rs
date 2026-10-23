//! v0.25.10 quota-triggered routing regressions.
//!
//! The servers below are loopback-only fixtures.  They exercise the shared
//! HTTP path while preserving the real request body and session headers.
use super::{
    model::Edit,
    replay::{self, WireBody},
    *,
};
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use http_body_util::BodyExt;
use hyper::{body::Incoming, Request, Response, StatusCode};
use hyper_util::{client::legacy::Client, rt::TokioExecutor};
use serde_json::Value;
use std::{
    convert::Infallible,
    future::Future,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use tokio::sync::{oneshot, Notify};

const CREATED: &str =
    r#"{"type":"response.created","response":{"id":"fixture-response","model":"fixture-model"}}"#;
const COMPLETED: &str = r#"{"type":"response.completed","response":{"id":"fixture-response","model":"fixture-model","status":"completed"}}"#;
const TEMPORARY_429: &str = r#"{"type":"error","error":{"code":"USAGE_LIMIT_EXCEEDED","message":"try again later","status":429}}"#;
const CREATE: &str = r#"{"type":"response.create","model":"fixture-model","input":[{"role":"user","content":"fixture"}]}"#;

async fn exercise_quota_wait(reply: &'static str, stop_while_querying: bool) {
    let attempts = Arc::new(AtomicUsize::new(0));
    let queries = Arc::new(AtomicUsize::new(0));
    let query_started = Arc::new(Notify::new());
    let attempts_seen = attempts.clone();
    let queries_seen = queries.clone();
    let started_seen = query_started.clone();
    let primary = server(move |request| {
        let attempts = attempts_seen.clone();
        let queries = queries_seen.clone();
        let started = started_seen.clone();
        async move {
            if request.method() == hyper::Method::GET {
                assert!(request.uri().path().ends_with("/v1/usage"));
                if queries.fetch_add(1, Ordering::SeqCst) == 0 {
                    // Seed a real old display response with zero balance.
                    return json_response(
                        StatusCode::OK,
                        r#"{"mode":"balance","balance":0}"#,
                        None,
                    );
                }
                started.notify_one();
                if stop_while_querying {
                    std::future::pending::<()>().await;
                }
                // Longer than the original one-second retry, so starting that
                // timer again after the query would miss the assertion below.
                tokio::time::sleep(Duration::from_millis(1100)).await;
                return json_response(StatusCode::OK, reply, None);
            }
            if attempts.fetch_add(1, Ordering::SeqCst) == 1 {
                return json_response(StatusCode::TOO_MANY_REQUESTS, TEMPORARY_429, None);
            }
            json_response(StatusCode::OK, r#"{"provider":"primary"}"#, None)
        }
    })
    .await;
    let backup_hits = Arc::new(AtomicUsize::new(0));
    let seen = backup_hits.clone();
    let backup = server(move |_| {
        seen.fetch_add(1, Ordering::SeqCst);
        async { json_response(StatusCode::OK, r#"{"provider":"backup"}"#, None) }
    })
    .await;
    let (dir, gateway) = fixture(vec![
        format!("http://127.0.0.1:{}/v1", primary.port),
        format!("http://127.0.0.1:{}/v1", backup.port),
    ])
    .await;
    configure(&gateway, &dir);
    let provider = gateway.view().providers[0].id.clone();
    gateway.0.quota.set_interval(0);
    let query = gateway.query_input(&provider).unwrap();
    let version = query.version.clone();
    let old = gateway.0.quota.query(query, true).await.unwrap();
    assert_eq!(old.state, "ok");
    assert_eq!(old.plans[0].remaining, Some(0.0));
    start(&gateway, &dir).await;
    let body = br#"{"model":"fixture-model","input":[{"role":"user","content":"fixture"}]}"#;
    request(&gateway, "fixture-quota-wait", body)
        .await
        .into_body()
        .collect()
        .await
        .unwrap();
    let pending = tokio::spawn({
        let gateway = gateway.clone();
        async move { request(&gateway, "fixture-quota-wait", body).await }
    });
    tokio::time::timeout(Duration::from_secs(3), query_started.notified())
        .await
        .expect("periodic refresh disabled must not disable an error probe");
    let began = Instant::now();
    let view = gateway.view();
    assert!(view.waiting_requests > 0);
    assert_eq!(view.providers[0].active_requests, 0);
    assert!(gateway.0.quota.probe_view(&provider, &version).0);
    if stop_while_querying {
        tokio::time::timeout(Duration::from_secs(2), gateway.stop())
            .await
            .unwrap()
            .unwrap();
    }
    let response = tokio::time::timeout(Duration::from_secs(2), pending)
        .await
        .expect("quota wait did not finish within its original deadline")
        .unwrap();
    if stop_while_querying {
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    } else {
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(
            serde_json::from_slice::<Value>(&bytes).unwrap()["provider"],
            "primary"
        );
        assert!(began.elapsed() < Duration::from_millis(1800));
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
        gateway.stop().await.unwrap();
    }
    assert_eq!(queries.load(Ordering::SeqCst), 2);
    assert_eq!(backup_hits.load(Ordering::SeqCst), 0);
    assert_eq!(gateway.view().waiting_requests, 0);
    assert!(!gateway.0.quota.probe_view(&provider, &version).0);
    primary.stop().await;
    backup.stop().await;
}

#[tokio::test]
async fn quota_wait_fresh_non_exhausted_evidence_keeps_owner_and_original_deadline() {
    for reply in [
        r#"{"mode":"wallet","balance":1e-400}"#,
        r#"{"mode":"wallet"}"#,
    ] {
        exercise_quota_wait(reply, false).await;
    }
}

#[tokio::test]
async fn quota_wait_stop_cancels_hanging_probe_and_releases_waiter() {
    exercise_quota_wait(r#"{}"#, true).await;
}

struct LoopbackServer {
    port: u16,
    stop: Option<oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl LoopbackServer {
    async fn stop(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for LoopbackServer {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

async fn server<F, Fut>(handler: F) -> LoopbackServer
where
    F: Fn(Request<Incoming>) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Response<WireBody>> + Send + 'static,
{
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (stop, mut stopped) = oneshot::channel();
    let handler = Arc::new(handler);
    let task = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut stopped => break,
                accepted = listener.accept() => {
                    let Ok((stream, _)) = accepted else { break };
                    let handler = handler.clone();
                    tokio::spawn(async move {
                        let _ = hyper::server::conn::http1::Builder::new()
                            .serve_connection(
                                hyper_util::rt::TokioIo::new(stream),
                                hyper::service::service_fn(move |request| {
                                    let handler = handler.clone();
                                    async move { Ok::<_, Infallible>(handler(request).await) }
                                }),
                            )
                            .with_upgrades()
                            .await;
                    });
                }
            }
        }
    });
    LoopbackServer {
        port,
        stop: Some(stop),
        task: Some(task),
    }
}

fn json_response(
    status: StatusCode,
    body: impl Into<Bytes>,
    retry_after: Option<&str>,
) -> Response<WireBody> {
    let mut response = Response::builder()
        .status(status)
        .header("content-type", "application/json");
    if let Some(retry_after) = retry_after {
        response = response.header("retry-after", retry_after);
    }
    response.body(replay::full(body.into())).unwrap()
}

fn sse_response(body: impl Into<Bytes>, retry_after: Option<&str>) -> Response<WireBody> {
    let mut response = Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream");
    if let Some(retry_after) = retry_after {
        response = response.header("retry-after", retry_after);
    }
    response.body(replay::full(body.into())).unwrap()
}

async fn fixture(urls: Vec<String>) -> (tempfile::TempDir, Gateway) {
    let dir = tempfile::tempdir().unwrap();
    let first = urls
        .first()
        .cloned()
        .unwrap_or_else(|| "https://fixture.example.invalid/v1".into());
    std::fs::write(
        dir.path().join("config.toml"),
        format!(
            "model_provider='custom'\n[model_providers.custom]\nbase_url = {}\nexperimental_bearer_token = \"fixture-token\"\nwire_api='responses'\nsupports_websockets=false\n",
            serde_json::to_string(&first).unwrap()
        ),
    )
    .unwrap();
    let gateway = Gateway::new(dir.path().to_path_buf()).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let mut settings = gateway.view().settings;
    settings.port = port;
    update(&gateway, &dir, Edit::Settings { settings });
    for url in urls {
        update(
            &gateway,
            &dir,
            Edit::SaveProvider {
                quota_access_token: None,
                id: None,
                base_url: url,
                token: "fixture-token".into(),
                name: None,
            },
        );
    }
    update(
        &gateway,
        &dir,
        Edit::Mode {
            mode: "auto".into(),
        },
    );
    (dir, gateway)
}

fn update(gateway: &Gateway, dir: &tempfile::TempDir, edit: Edit) {
    gateway
        .edit(edit, &gateway.view().revision, dir.path())
        .unwrap();
}

fn configure(gateway: &Gateway, dir: &tempfile::TempDir) {
    update(
        gateway,
        dir,
        Edit::Settings {
            settings: Settings {
                max_retries: 2,
                capacity_retry_seconds: 1,
                websocket_retry_seconds: 1,
                queue_seconds: 1,
                rate_limit_seconds: 1,
                first_byte_seconds: 3,
                idle_seconds: 3,
                total_seconds: 10,
                failure_threshold: 10,
                transient_failure_threshold: 10,
                ..gateway.view().settings
            },
        },
    );
    for provider in gateway.view().providers {
        update(
            gateway,
            dir,
            Edit::RpmProvider {
                id: provider.id,
                max_rpm: 100,
            },
        );
    }
}

async fn start(gateway: &Gateway, dir: &tempfile::TempDir) {
    gateway
        .start(&gateway.view().revision, dir.path())
        .await
        .unwrap();
}

async fn request(gateway: &Gateway, session: &str, body: impl AsRef<[u8]>) -> Response<Incoming> {
    let client: Client<_, WireBody> = Client::builder(TokioExecutor::new())
        .build(super::connector::Connector::new(Duration::from_secs(3), 0));
    let (token, port) = {
        let state = gateway.0.inner.lock().unwrap();
        (state.store.local_token.clone(), state.store.settings.port)
    };
    let req = Request::builder()
        .method("POST")
        .uri(format!("http://127.0.0.1:{port}/v1/responses"))
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .header("session_id", session)
        .body(replay::full(Bytes::copy_from_slice(body.as_ref())))
        .unwrap();
    client.request(req).await.unwrap()
}

async fn ws_client(gateway: &Gateway, session: &str) -> yawc::TcpWebSocket {
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
    .with_request(
        yawc::HttpRequest::builder()
            .header("authorization", format!("Bearer {token}"))
            .header("session_id", session),
    )
    .await
    .unwrap()
}

async fn next_frame(ws: &mut yawc::TcpWebSocket) -> yawc::Frame {
    tokio::time::timeout(Duration::from_secs(8), ws.next())
        .await
        .expect("fixture WebSocket timed out")
        .expect("fixture WebSocket closed")
}

async fn native_ws_server<F, Fut>(handler: F) -> LoopbackServer
where
    F: Fn(usize, yawc::HttpWebSocket) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let connections = Arc::new(AtomicUsize::new(0));
    let seen = connections.clone();
    let handler = Arc::new(handler);
    server(move |mut request| {
        let connection = seen.fetch_add(1, Ordering::SeqCst) + 1;
        let handler = handler.clone();
        async move {
            if request.headers().get("upgrade").is_none() {
                let path = request.uri().path().to_owned();
                let _ = request.into_body().collect().await;
                if path.ends_with("/usage") {
                    return json_response(
                        StatusCode::OK,
                        br#"{"mode":"wallet","balance":0}"#.as_slice(),
                        None,
                    );
                }
                return json_response(
                    StatusCode::OK,
                    br#"{"provider":"fixture-http"}"#.as_slice(),
                    None,
                );
            }
            let (response, upgrade) = yawc::WebSocket::upgrade_with_options(
                &mut request,
                yawc::Options::default().with_balanced_compression(),
            )
            .unwrap();
            tokio::spawn(async move {
                if let Ok(socket) = upgrade.await {
                    handler(connection, socket).await;
                }
            });
            response.map(|_| replay::empty())
        }
    })
    .await
}

async fn observed_native_ws_server<F, Fut>(
    quota_hits: Arc<AtomicUsize>,
    status_hits: Arc<AtomicUsize>,
    handler: F,
) -> LoopbackServer
where
    F: Fn(usize, yawc::HttpWebSocket) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let connections = Arc::new(AtomicUsize::new(0));
    let seen = connections.clone();
    let handler = Arc::new(handler);
    server(move |mut request| {
        let connection = seen.fetch_add(1, Ordering::SeqCst) + 1;
        let handler = handler.clone();
        let quota_hits = quota_hits.clone();
        let status_hits = status_hits.clone();
        async move {
            if request.headers().get("upgrade").is_none() {
                let path = request.uri().path().to_owned();
                let method = request.method().clone();
                let _ = request.into_body().collect().await;
                if method == hyper::Method::GET && path.ends_with("/usage") {
                    quota_hits.fetch_add(1, Ordering::SeqCst);
                    return json_response(
                        StatusCode::OK,
                        br#"{"mode":"wallet","balance":0}"#.as_slice(),
                        None,
                    );
                }
                if method == hyper::Method::GET && path.ends_with("/api/status") {
                    status_hits.fetch_add(1, Ordering::SeqCst);
                    return json_response(StatusCode::OK, br#"{}"#.as_slice(), None);
                }
                return json_response(
                    StatusCode::OK,
                    br#"{"provider":"fixture-http"}"#.as_slice(),
                    None,
                );
            }
            let (response, upgrade) = yawc::WebSocket::upgrade_with_options(
                &mut request,
                yawc::Options::default().with_balanced_compression(),
            )
            .unwrap();
            tokio::spawn(async move {
                if let Ok(socket) = upgrade.await {
                    handler(connection, socket).await;
                }
            });
            response.map(|_| replay::empty())
        }
    })
    .await
}

#[tokio::test]
async fn established_owner_second_request_probes_during_cooldown_before_long_wait() {
    let attempts = Arc::new(Mutex::new(std::collections::HashMap::<String, usize>::new()));
    let quota_started = Arc::new(Notify::new());
    let release_quota = Arc::new(Notify::new());
    let quota_hits = Arc::new(AtomicUsize::new(0));
    let api_status_hits = Arc::new(AtomicUsize::new(0));
    let attempts_seen = attempts.clone();
    let started_seen = quota_started.clone();
    let release_seen = release_quota.clone();
    let usage_hits = quota_hits.clone();
    let status_hits_for_server = api_status_hits.clone();
    let primary = server(move |request| {
        let path = request.uri().path().to_owned();
        let method = request.method().clone();
        let session = request
            .headers()
            .get("session_id")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("unknown")
            .to_owned();
        let attempts_seen = attempts_seen.clone();
        let started_seen = started_seen.clone();
        let release_seen = release_seen.clone();
        let usage_hits = usage_hits.clone();
        let status_hits = status_hits_for_server.clone();
        async move {
            if method == hyper::Method::GET && path.ends_with("/v1/usage") {
                usage_hits.fetch_add(1, Ordering::SeqCst);
                started_seen.notify_one();
                release_seen.notified().await;
                return json_response(
                    StatusCode::OK,
                    br#"{"mode":"wallet","balance":0}"#.as_slice(),
                    None,
                );
            }
            if method == hyper::Method::GET && path.ends_with("/api/status") {
                status_hits.fetch_add(1, Ordering::SeqCst);
                return json_response(StatusCode::OK, br#"{}"#.as_slice(), None);
            }
            let number = {
                let mut attempts = attempts_seen.lock().unwrap();
                let count = attempts.entry(session).or_default();
                *count += 1;
                *count
            };
            if number == 1 {
                json_response(StatusCode::OK, br#"{"provider":"primary"}"#.as_slice(), None)
            } else {
                json_response(
                    StatusCode::TOO_MANY_REQUESTS,
                    br#"{"error":{"code":"USAGE_LIMIT_EXCEEDED","message":"model is at capacity"}}"#.as_slice(),
                    Some("45"),
                )
            }
        }
    })
    .await;
    let backup_hits = Arc::new(AtomicUsize::new(0));
    let backup_seen = backup_hits.clone();
    let backup = server(move |_| {
        backup_seen.fetch_add(1, Ordering::SeqCst);
        async { json_response(StatusCode::OK, br#"{"provider":"backup"}"#.as_slice(), None) }
    })
    .await;
    let (dir, gateway) = fixture(vec![
        format!("http://127.0.0.1:{}/v1", primary.port),
        format!("http://127.0.0.1:{}/v1", backup.port),
    ])
    .await;
    configure(&gateway, &dir);
    start(&gateway, &dir).await;
    let body = br#"{"model":"fixture-model","input":[{"role":"user","content":"fixture"}]}"#;
    let owner = request(&gateway, "fixture-owner", body).await;
    assert_eq!(owner.status(), StatusCode::OK);
    owner.into_body().collect().await.unwrap();

    let first = tokio::spawn({
        let gateway = gateway.clone();
        async move { request(&gateway, "fixture-owner", body).await }
    });
    tokio::time::timeout(Duration::from_secs(2), quota_started.notified())
        .await
        .expect("first error did not start a fresh quota probe");
    let second_started = Instant::now();
    let second = tokio::spawn({
        let gateway = gateway.clone();
        async move { request(&gateway, "fixture-owner", body).await }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    release_quota.notify_waiters();
    let first_response = tokio::time::timeout(Duration::from_secs(5), first)
        .await
        .expect("first owner request did not finish")
        .unwrap();
    assert_eq!(first_response.status(), StatusCode::OK);
    first_response.into_body().collect().await.unwrap();
    let second_response = tokio::time::timeout(Duration::from_secs(5), second)
        .await
        .expect("second owner request waited through Retry-After instead of probing")
        .unwrap();
    assert_eq!(second_response.status(), StatusCode::OK);
    let second_body = second_response
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes();
    let second_value: Value = serde_json::from_slice(&second_body).unwrap();
    assert_eq!(second_value["provider"], "backup");
    assert!(second_started.elapsed() < Duration::from_secs(5));
    assert_eq!(backup_hits.load(Ordering::SeqCst), 2);
    assert_eq!(quota_hits.load(Ordering::SeqCst), 1);
    assert_eq!(api_status_hits.load(Ordering::SeqCst), 0);
    gateway.stop().await.unwrap();
    primary.stop().await;
    backup.stop().await;
}

#[tokio::test]
async fn permanent_daily_weekly_monthly_denials_fail_over_without_capacity_wait() {
    for (status, reason) in [
        (StatusCode::FORBIDDEN, "DAILY_LIMIT_EXCEEDED"),
        (StatusCode::TOO_MANY_REQUESTS, "WEEKLY_LIMIT_EXCEEDED"),
        (StatusCode::TOO_MANY_REQUESTS, "MONTHLY_LIMIT_EXCEEDED"),
    ] {
        let primary_hits = Arc::new(AtomicUsize::new(0));
        let primary_seen = primary_hits.clone();
        let primary = server(move |_| {
            primary_seen.fetch_add(1, Ordering::SeqCst);
            let body = format!(
                r#"{{"error":{{"code":"429","reason":"{reason}","message":"quota exhausted"}}}}"#
            );
            async move { json_response(status, body.into_bytes(), Some("45")) }
        })
        .await;
        let backup_hits = Arc::new(AtomicUsize::new(0));
        let backup_seen = backup_hits.clone();
        let backup = server(move |_| {
            backup_seen.fetch_add(1, Ordering::SeqCst);
            async { json_response(StatusCode::OK, br#"{"provider":"backup"}"#.as_slice(), None) }
        })
        .await;
        let (dir, gateway) = fixture(vec![
            format!("http://127.0.0.1:{}/v1", primary.port),
            format!("http://127.0.0.1:{}/v1", backup.port),
        ])
        .await;
        configure(&gateway, &dir);
        start(&gateway, &dir).await;
        let started = Instant::now();
        let response = request(
            &gateway,
            "fixture-permanent-denial",
            br#"{"model":"fixture-model","input":"fixture"}"#,
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let value: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["provider"], "backup");
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(primary_hits.load(Ordering::SeqCst), 1);
        assert_eq!(backup_hits.load(Ordering::SeqCst), 1);
        gateway.stop().await.unwrap();
        primary.stop().await;
        backup.stop().await;
    }
}

#[tokio::test]
async fn permanent_daily_denial_without_alternative_returns_immediately() {
    let primary = server(|_| async {
        json_response(
            StatusCode::FORBIDDEN,
            br#"{"error":{"code":"403","reason":"DAILY_LIMIT_EXCEEDED"}}"#.as_slice(),
            Some("45"),
        )
    })
    .await;
    let (dir, gateway) = fixture(vec![format!("http://127.0.0.1:{}/v1", primary.port)]).await;
    configure(&gateway, &dir);
    start(&gateway, &dir).await;
    let started = Instant::now();
    let response = request(
        &gateway,
        "fixture-no-alternative",
        br#"{"model":"fixture-model","input":"fixture"}"#,
    )
    .await;
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(
        body.as_ref(),
        br#"{"error":{"code":"403","reason":"DAILY_LIMIT_EXCEEDED"}}"#
    );
    gateway.stop().await.unwrap();
    primary.stop().await;
}

#[tokio::test]
async fn manual_mode_does_not_replay_permanent_denial_to_another_provider() {
    let backup_hits = Arc::new(AtomicUsize::new(0));
    let backup_seen = backup_hits.clone();
    let primary = server(|_| async {
        json_response(
            StatusCode::FORBIDDEN,
            br#"{"error":{"code":"403","reason":"DAILY_LIMIT_EXCEEDED"}}"#.as_slice(),
            Some("45"),
        )
    })
    .await;
    let backup = server(move |_| {
        backup_seen.fetch_add(1, Ordering::SeqCst);
        async { json_response(StatusCode::OK, br#"{"provider":"backup"}"#.as_slice(), None) }
    })
    .await;
    let (dir, gateway) = fixture(vec![
        format!("http://127.0.0.1:{}/v1", primary.port),
        format!("http://127.0.0.1:{}/v1", backup.port),
    ])
    .await;
    configure(&gateway, &dir);
    update(
        &gateway,
        &dir,
        Edit::Mode {
            mode: "manual".into(),
        },
    );
    start(&gateway, &dir).await;
    let started = Instant::now();
    let response = request(
        &gateway,
        "fixture-manual-permanent-denial",
        br#"{"model":"fixture-model","input":"fixture"}"#,
    )
    .await;
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(body
        .windows(b"DAILY_LIMIT_EXCEEDED".len())
        .any(|window| { window == b"DAILY_LIMIT_EXCEEDED" }));
    assert_eq!(backup_hits.load(Ordering::SeqCst), 0);
    gateway.stop().await.unwrap();
    primary.stop().await;
    backup.stop().await;
}

#[tokio::test]
async fn manual_usage_limit_cooldown_probes_without_switching_provider() {
    let quota_hits = Arc::new(AtomicUsize::new(0));
    let status_hits = Arc::new(AtomicUsize::new(0));
    let quota_seen = quota_hits.clone();
    let status_seen = status_hits.clone();
    let primary = server(move |request| {
        let path = request.uri().path().to_owned();
        let quota_seen = quota_seen.clone();
        let status_seen = status_seen.clone();
        async move {
            if request.method() == hyper::Method::GET && path.ends_with("/v1/usage") {
                quota_seen.fetch_add(1, Ordering::SeqCst);
                return json_response(
                    StatusCode::OK,
                    br#"{"mode":"wallet","balance":0}"#.as_slice(),
                    None,
                );
            }
            if request.method() == hyper::Method::GET && path.ends_with("/api/status") {
                status_seen.fetch_add(1, Ordering::SeqCst);
                return json_response(StatusCode::OK, br#"{}"#.as_slice(), None);
            }
            json_response(
                StatusCode::TOO_MANY_REQUESTS,
                br#"{"error":{"code":"USAGE_LIMIT_EXCEEDED","message":"model is at capacity"}}"#
                    .as_slice(),
                Some("45"),
            )
        }
    })
    .await;
    let backup_hits = Arc::new(AtomicUsize::new(0));
    let backup_seen = backup_hits.clone();
    let backup = server(move |_| {
        backup_seen.fetch_add(1, Ordering::SeqCst);
        async { json_response(StatusCode::OK, br#"{"provider":"backup"}"#.as_slice(), None) }
    })
    .await;
    let (dir, gateway) = fixture(vec![
        format!("http://127.0.0.1:{}/v1", primary.port),
        format!("http://127.0.0.1:{}/v1", backup.port),
    ])
    .await;
    configure(&gateway, &dir);
    update(
        &gateway,
        &dir,
        Edit::Mode {
            mode: "manual".into(),
        },
    );
    start(&gateway, &dir).await;
    let started = Instant::now();
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        request(
            &gateway,
            "fixture-manual-usage-limit",
            br#"{"model":"fixture-model","input":"fixture"}"#,
        ),
    )
    .await
    .expect("manual UsageLimit cooldown waited through Retry-After instead of probing");
    let body = result.into_body().collect().await.unwrap().to_bytes();
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(body
        .windows(b"USAGE_LIMIT_EXCEEDED".len())
        .any(|window| { window == b"USAGE_LIMIT_EXCEEDED" }));
    assert_eq!(quota_hits.load(Ordering::SeqCst), 1);
    assert_eq!(status_hits.load(Ordering::SeqCst), 0);
    assert_eq!(backup_hits.load(Ordering::SeqCst), 0);
    gateway.stop().await.unwrap();
    primary.stop().await;
    backup.stop().await;
}

#[tokio::test]
async fn pinned_cursor_usage_limit_probes_without_replaying_to_another_provider() {
    let quota_hits = Arc::new(AtomicUsize::new(0));
    let quota_seen = quota_hits.clone();
    let primary = server(move |request| {
        let path = request.uri().path().to_owned();
        let quota_seen = quota_seen.clone();
        async move {
            if request.method() == hyper::Method::GET && path.ends_with("/v1/usage") {
                quota_seen.fetch_add(1, Ordering::SeqCst);
                return json_response(
                    StatusCode::OK,
                    br#"{"mode":"wallet","balance":0}"#.as_slice(),
                    None,
                );
            }
            json_response(
                StatusCode::TOO_MANY_REQUESTS,
                br#"{"error":{"code":"USAGE_LIMIT_EXCEEDED","message":"model is at capacity"}}"#
                    .as_slice(),
                Some("45"),
            )
        }
    })
    .await;
    let backup_hits = Arc::new(AtomicUsize::new(0));
    let backup_seen = backup_hits.clone();
    let backup = server(move |_| {
        backup_seen.fetch_add(1, Ordering::SeqCst);
        async { json_response(StatusCode::OK, br#"{"provider":"backup"}"#.as_slice(), None) }
    })
    .await;
    let (dir, gateway) = fixture(vec![
        format!("http://127.0.0.1:{}/v1", primary.port),
        format!("http://127.0.0.1:{}/v1", backup.port),
    ])
    .await;
    configure(&gateway, &dir);
    let owner = gateway.view().providers[0].id.clone();
    gateway.remember("fixture-pinned-response", &owner);
    start(&gateway, &dir).await;
    let started = Instant::now();
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        request(
            &gateway,
            "fixture-pinned-usage-limit",
            br#"{"model":"fixture-model","previous_response_id":"fixture-pinned-response","input":"fixture"}"#,
        ),
    )
    .await
    .expect("pinned UsageLimit cooldown waited through Retry-After instead of probing");
    let body = result.into_body().collect().await.unwrap().to_bytes();
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(body
        .windows(b"USAGE_LIMIT_EXCEEDED".len())
        .any(|window| { window == b"USAGE_LIMIT_EXCEEDED" }));
    assert_eq!(quota_hits.load(Ordering::SeqCst), 1);
    assert_eq!(backup_hits.load(Ordering::SeqCst), 0);
    gateway.stop().await.unwrap();
    primary.stop().await;
    backup.stop().await;
}

#[tokio::test]
async fn sse_established_owner_usage_limit_probes_then_switches_provider() {
    let post_hits = Arc::new(AtomicUsize::new(0));
    let quota_hits = Arc::new(AtomicUsize::new(0));
    let status_hits = Arc::new(AtomicUsize::new(0));
    let post_seen = post_hits.clone();
    let quota_seen = quota_hits.clone();
    let status_seen = status_hits.clone();
    let primary = server(move |request| {
        let path = request.uri().path().to_owned();
        let method = request.method().clone();
        let post_seen = post_seen.clone();
        let quota_seen = quota_seen.clone();
        let status_seen = status_seen.clone();
        async move {
            if method == hyper::Method::GET && path.ends_with("/v1/usage") {
                quota_seen.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(1200)).await;
                return json_response(
                    StatusCode::OK,
                    br#"{"mode":"wallet","balance":0}"#.as_slice(),
                    None,
                );
            }
            if method == hyper::Method::GET && path.ends_with("/api/status") {
                status_seen.fetch_add(1, Ordering::SeqCst);
                return json_response(StatusCode::OK, br#"{}"#.as_slice(), None);
            }
            let number = post_seen.fetch_add(1, Ordering::SeqCst) + 1;
            if number == 1 {
                return json_response(
                    StatusCode::OK,
                    br#"{"provider":"primary"}"#.as_slice(),
                    None,
                );
            }
            sse_response(format!("event: error\ndata: {TEMPORARY_429}\n\n"), None)
        }
    })
    .await;
    let backup_hits = Arc::new(AtomicUsize::new(0));
    let backup_seen = backup_hits.clone();
    let backup = server(move |_| {
        backup_seen.fetch_add(1, Ordering::SeqCst);
        async {
            sse_response(
                format!(
                    "event: response.created\ndata: {CREATED}\n\nevent: response.completed\ndata: {COMPLETED}\n\n"
                ),
                None,
            )
        }
    })
    .await;
    let (dir, gateway) = fixture(vec![
        format!("http://127.0.0.1:{}/v1", primary.port),
        format!("http://127.0.0.1:{}/v1", backup.port),
    ])
    .await;
    configure(&gateway, &dir);
    start(&gateway, &dir).await;
    let first = request(
        &gateway,
        "fixture-sse-established-owner",
        br#"{"model":"fixture-model","input":"first"}"#,
    )
    .await;
    assert_eq!(first.status(), StatusCode::OK);
    first.into_body().collect().await.unwrap();
    let started = Instant::now();
    let second = tokio::time::timeout(
        Duration::from_secs(5),
        request(
            &gateway,
            "fixture-sse-established-owner",
            br#"{"model":"fixture-model","stream":true,"input":"second"}"#,
        ),
    )
    .await
    .expect("SSE UsageLimit cooldown waited instead of probing and switching");
    assert_eq!(second.status(), StatusCode::OK);
    let body = second.into_body().collect().await.unwrap().to_bytes();
    assert!(body
        .windows(b"response.completed".len())
        .any(|window| { window == b"response.completed" }));
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(post_hits.load(Ordering::SeqCst), 2);
    assert_eq!(quota_hits.load(Ordering::SeqCst), 1);
    assert_eq!(status_hits.load(Ordering::SeqCst), 0);
    assert_eq!(backup_hits.load(Ordering::SeqCst), 1);
    gateway.stop().await.unwrap();
    primary.stop().await;
    backup.stop().await;
}

#[tokio::test]
async fn websocket_established_owner_usage_limit_probes_then_switches_provider() {
    let quota_hits = Arc::new(AtomicUsize::new(0));
    let status_hits = Arc::new(AtomicUsize::new(0));
    let primary = observed_native_ws_server(
        quota_hits.clone(),
        status_hits.clone(),
        |_, mut socket| async move {
            let mut turns = 0usize;
            while let Some(frame) = socket.next().await {
                if frame.opcode() == yawc::OpCode::Close {
                    break;
                }
                if frame.opcode().is_control() {
                    continue;
                }
                turns += 1;
                if turns == 2 {
                    let _ = socket
                        .send(yawc::Frame::text(TEMPORARY_429.to_owned()))
                        .await;
                    break;
                }
                let _ = socket.send(yawc::Frame::text(CREATED.to_owned())).await;
                let _ = socket.send(yawc::Frame::text(COMPLETED.to_owned())).await;
            }
        },
    )
    .await;
    let backup_hits = Arc::new(AtomicUsize::new(0));
    let backup_seen = backup_hits.clone();
    let backup = native_ws_server(move |_, mut socket| {
        backup_seen.fetch_add(1, Ordering::SeqCst);
        async move {
            while let Some(frame) = socket.next().await {
                if frame.opcode() == yawc::OpCode::Close {
                    break;
                }
                if frame.opcode().is_control() {
                    continue;
                }
                let _ = socket.send(yawc::Frame::text(CREATED.to_owned())).await;
                let _ = socket.send(yawc::Frame::text(COMPLETED.to_owned())).await;
                break;
            }
        }
    })
    .await;
    let (dir, gateway) = fixture(vec![
        format!("http://127.0.0.1:{}/v1", primary.port),
        format!("http://127.0.0.1:{}/v1", backup.port),
    ])
    .await;
    configure(&gateway, &dir);
    start(&gateway, &dir).await;
    let mut client = ws_client(&gateway, "fixture-ws-established-owner-quota").await;
    client
        .send(yawc::Frame::text(CREATE.to_owned()))
        .await
        .unwrap();
    assert_eq!(next_frame(&mut client).await.as_str(), CREATED);
    assert_eq!(next_frame(&mut client).await.as_str(), COMPLETED);
    let started = Instant::now();
    client
        .send(yawc::Frame::text(CREATE.to_owned()))
        .await
        .unwrap();
    assert_eq!(next_frame(&mut client).await.as_str(), CREATED);
    assert_eq!(next_frame(&mut client).await.as_str(), COMPLETED);
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(quota_hits.load(Ordering::SeqCst), 1);
    assert_eq!(status_hits.load(Ordering::SeqCst), 0);
    assert_eq!(backup_hits.load(Ordering::SeqCst), 1);
    client
        .send(yawc::Frame::close(1000.into(), "fixture done"))
        .await
        .unwrap();
    drop(client);
    gateway.stop().await.unwrap();
    primary.stop().await;
    backup.stop().await;
}
