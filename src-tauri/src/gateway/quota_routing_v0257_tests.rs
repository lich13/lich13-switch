//! v0.25.7 loopback regressions for quota and routing decisions.
//!
//! Upstreams are synthetic loopback HTTP/WebSocket servers.  The fixtures use
//! temporary gateway stores and never read or write an installed client store.
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
use tokio::sync::oneshot;

const CREATED: &str =
    r#"{"type":"response.created","response":{"id":"fixture-response","model":"fixture-model"}}"#;
const COMPLETED: &str = r#"{"type":"response.completed","response":{"id":"fixture-response","model":"fixture-model","status":"completed"}}"#;
const TEMPORARY_429: &str = r#"{"type":"error","error":{"code":"USAGE_LIMIT_EXCEEDED","message":"try again later","status":429}}"#;
const DAILY_LIMIT: &str =
    r#"{"error":{"code":"429","reason":"DAILY_LIMIT_EXCEEDED","message":"daily quota exhausted"}}"#;
const DAILY_LIMIT_TEXT: &str = "code=429 reason=\"DAILY_LIMIT_EXCEEDED\" daily quota exhausted";
const CREATE: &str = r#"{"type":"response.create","model":"fixture-model","input":[{"role":"user","content":"fixture"}]}"#;

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

fn sse_response(body: impl Into<Bytes>) -> Response<WireBody> {
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .body(replay::full(body.into()))
        .unwrap()
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
                let _ = request.into_body().collect().await;
                return json_response(StatusCode::OK, r#"{"provider":"fixture-http"}"#, None);
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
async fn http_daily_limit_switches_before_retry_after_and_preserves_single_provider_error() {
    for (body, content_type) in [
        (DAILY_LIMIT, "application/json"),
        (DAILY_LIMIT_TEXT, "text/plain"),
    ] {
        let primary_hits = Arc::new(AtomicUsize::new(0));
        let seen = primary_hits.clone();
        let primary = server(move |_| {
            seen.fetch_add(1, Ordering::SeqCst);
            let body = body.to_owned();
            let content_type = content_type.to_owned();
            async move {
                Response::builder()
                    .status(StatusCode::TOO_MANY_REQUESTS)
                    .header("content-type", content_type)
                    .header("retry-after", "30")
                    .body(replay::full(body))
                    .unwrap()
            }
        })
        .await;
        let backup_hits = Arc::new(AtomicUsize::new(0));
        let backup_seen = backup_hits.clone();
        let backup = server(move |_| {
            backup_seen.fetch_add(1, Ordering::SeqCst);
            async { json_response(StatusCode::OK, r#"{"provider":"backup"}"#, None) }
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
            "fixture-daily-limit-new-thread",
            br#"{"model":"fixture-model","input":[{"role":"user","content":"first"}]}"#,
        )
        .await;
        let elapsed = started.elapsed();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes()
                .as_ref(),
            br#"{"provider":"backup"}"#.as_slice()
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "daily quota waited {elapsed:?}"
        );
        assert_eq!(primary_hits.load(Ordering::SeqCst), 1);
        assert_eq!(backup_hits.load(Ordering::SeqCst), 1);
        gateway.stop().await.unwrap();
        primary.stop().await;
        backup.stop().await;
    }

    let primary = server(|_| async {
        Response::builder()
            .status(StatusCode::TOO_MANY_REQUESTS)
            .header("content-type", "application/json")
            .header("retry-after", "30")
            .body(replay::full(DAILY_LIMIT))
            .unwrap()
    })
    .await;
    let (dir, gateway) = fixture(vec![format!("http://127.0.0.1:{}/v1", primary.port)]).await;
    configure(&gateway, &dir);
    start(&gateway, &dir).await;
    let started = Instant::now();
    let response = request(
        &gateway,
        "fixture-daily-limit-no-backup",
        br#"{"model":"fixture-model","input":"first"}"#,
    )
    .await;
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(body.as_ref(), DAILY_LIMIT.as_bytes());
    gateway.stop().await.unwrap();
    primary.stop().await;
}

#[tokio::test]
async fn http_new_thread_temporary_429_changes_provider_but_established_owner_waits() {
    let primary_hits = Arc::new(Mutex::new(std::collections::HashMap::<String, usize>::new()));
    let primary_seen = primary_hits.clone();
    let primary = server(move |request| {
        let session = request
            .headers()
            .get("session_id")
            .and_then(|value| value.to_str().ok())
            .unwrap_or("unknown")
            .to_owned();
        let attempt = {
            let mut counts = primary_seen.lock().unwrap();
            let count = counts.entry(session.clone()).or_default();
            *count += 1;
            *count
        };
        async move {
            if (session == "fixture-temporary-new-thread" && attempt == 1)
                || (session == "fixture-temporary-established-owner" && attempt == 2)
            {
                json_response(StatusCode::TOO_MANY_REQUESTS, TEMPORARY_429, Some("1"))
            } else if session == "fixture-temporary-established-owner" && attempt >= 3 {
                json_response(StatusCode::OK, r#"{"provider":"owner","attempt":3}"#, None)
            } else {
                json_response(StatusCode::OK, r#"{"provider":"owner","attempt":1}"#, None)
            }
        }
    })
    .await;
    let backup_hits = Arc::new(AtomicUsize::new(0));
    let backup_seen = backup_hits.clone();
    let backup = server(move |_| {
        let attempt = backup_seen.fetch_add(1, Ordering::SeqCst) + 1;
        async move {
            json_response(
                StatusCode::OK,
                format!(r#"{{"provider":"backup","attempt":{attempt}}}"#),
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

    let new_thread = request(
        &gateway,
        "fixture-temporary-new-thread",
        br#"{"model":"fixture-model","input":[{"role":"user","content":"first"}]}"#,
    )
    .await;
    assert_eq!(new_thread.status(), StatusCode::OK);
    let new_value: Value =
        serde_json::from_slice(&new_thread.into_body().collect().await.unwrap().to_bytes())
            .unwrap();
    assert_eq!(new_value["provider"], "backup");
    assert_eq!(backup_hits.load(Ordering::SeqCst), 1);
    tokio::time::sleep(Duration::from_secs(2)).await;

    let owner_first = request(
        &gateway,
        "fixture-temporary-established-owner",
        br#"{"model":"fixture-model","input":[{"role":"user","content":"first"}]}"#,
    )
    .await;
    assert_eq!(owner_first.status(), StatusCode::OK);
    owner_first.into_body().collect().await.unwrap();
    let before = Instant::now();
    let owner_followup = request(
        &gateway,
        "fixture-temporary-established-owner",
        br#"{"model":"fixture-model","input":"followup"}"#,
    )
    .await;
    let waited = before.elapsed();
    assert_eq!(owner_followup.status(), StatusCode::OK);
    let owner_value: Value = serde_json::from_slice(
        &owner_followup
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes(),
    )
    .unwrap();
    assert_eq!(owner_value["provider"], "owner");
    assert!(
        waited >= Duration::from_millis(900),
        "owner waited only {waited:?}"
    );
    assert_eq!(backup_hits.load(Ordering::SeqCst), 1);
    let counts = primary_hits.lock().unwrap().clone();
    assert_eq!(counts.get("fixture-temporary-new-thread"), Some(&1));
    assert_eq!(counts.get("fixture-temporary-established-owner"), Some(&3));
    gateway.stop().await.unwrap();
    primary.stop().await;
    backup.stop().await;
}

#[tokio::test]
async fn sse_error_after_output_is_forwarded_without_replay_or_backup_request() {
    let primary_hits = Arc::new(AtomicUsize::new(0));
    let seen = primary_hits.clone();
    let primary = server(move |_| {
        seen.fetch_add(1, Ordering::SeqCst);
        async {
            sse_response(format!(
                "event: response.created\ndata: {CREATED}\n\nevent: error\ndata: {TEMPORARY_429}\n\n"
            ))
        }
    })
    .await;
    let backup_hits = Arc::new(AtomicUsize::new(0));
    let backup_seen = backup_hits.clone();
    let backup = server(move |_| {
        backup_seen.fetch_add(1, Ordering::SeqCst);
        async { json_response(StatusCode::OK, r#"{"provider":"backup"}"#, None) }
    })
    .await;
    let (dir, gateway) = fixture(vec![
        format!("http://127.0.0.1:{}/v1", primary.port),
        format!("http://127.0.0.1:{}/v1", backup.port),
    ])
    .await;
    configure(&gateway, &dir);
    start(&gateway, &dir).await;
    let response = request(
        &gateway,
        "fixture-sse-after-output",
        br#"{"model":"fixture-model","stream":true,"input":"fixture"}"#,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let text = String::from_utf8(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();
    assert!(text.contains("response.created"));
    assert!(text.contains("USAGE_LIMIT_EXCEEDED"));
    assert_eq!(primary_hits.load(Ordering::SeqCst), 1);
    assert_eq!(backup_hits.load(Ordering::SeqCst), 0);
    gateway.stop().await.unwrap();
    primary.stop().await;
    backup.stop().await;
}

#[tokio::test]
async fn websocket_handshake_daily_limit_fails_over_without_waiting() {
    let primary_hits = Arc::new(AtomicUsize::new(0));
    let primary_seen = primary_hits.clone();
    let primary = server(move |request| {
        primary_seen.fetch_add(1, Ordering::SeqCst);
        async move {
            if request.headers().get("upgrade").is_some() {
                return Response::builder()
                    .status(StatusCode::TOO_MANY_REQUESTS)
                    .header("content-type", "application/json")
                    .header("retry-after", "30")
                    .body(replay::full(DAILY_LIMIT))
                    .unwrap();
            }
            let _ = request.into_body().collect().await;
            json_response(StatusCode::OK, r#"{"provider":"primary-http"}"#, None)
        }
    })
    .await;
    let backup = native_ws_server(|_, mut socket| async move {
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
    let mut client = ws_client(&gateway, "fixture-ws-handshake-daily").await;
    client
        .send(yawc::Frame::text(CREATE.to_owned()))
        .await
        .unwrap();
    let first = next_frame(&mut client).await;
    let second = next_frame(&mut client).await;
    assert_eq!(first.as_str(), CREATED);
    assert_eq!(second.as_str(), COMPLETED);
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(primary_hits.load(Ordering::SeqCst), 1);
    client
        .send(yawc::Frame::close(1000.into(), "fixture done"))
        .await
        .unwrap();
    drop(client);
    gateway.stop().await.unwrap();
    primary.stop().await;
    backup.stop().await;
}

#[tokio::test]
async fn websocket_first_daily_error_switches_provider_before_retrying() {
    let primary = native_ws_server(|_, mut socket| async move {
        while let Some(frame) = socket.next().await {
            if frame.opcode() == yawc::OpCode::Close {
                break;
            }
            if frame.opcode().is_control() {
                continue;
            }
            let _ = socket.send(yawc::Frame::text(DAILY_LIMIT.to_owned())).await;
            break;
        }
    })
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
    let mut client = ws_client(&gateway, "fixture-ws-first-error").await;
    client
        .send(yawc::Frame::text(CREATE.to_owned()))
        .await
        .unwrap();
    assert_eq!(next_frame(&mut client).await.as_str(), CREATED);
    assert_eq!(next_frame(&mut client).await.as_str(), COMPLETED);
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

#[tokio::test]
async fn websocket_established_owner_temporary_429_retries_same_provider() {
    let primary = native_ws_server(|connection, mut socket| async move {
        let mut turns = 0usize;
        while let Some(frame) = socket.next().await {
            if frame.opcode() == yawc::OpCode::Close {
                break;
            }
            if frame.opcode().is_control() {
                continue;
            }
            turns += 1;
            if connection == 1 && turns == 2 {
                let _ = socket
                    .send(yawc::Frame::text(TEMPORARY_429.to_owned()))
                    .await;
                break;
            }
            let _ = socket.send(yawc::Frame::text(CREATED.to_owned())).await;
            let _ = socket.send(yawc::Frame::text(COMPLETED.to_owned())).await;
        }
    })
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
    let mut client = ws_client(&gateway, "fixture-ws-established-owner").await;
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
    assert!(started.elapsed() >= Duration::from_millis(900));
    assert_eq!(backup_hits.load(Ordering::SeqCst), 0);
    client
        .send(yawc::Frame::close(1000.into(), "fixture done"))
        .await
        .unwrap();
    drop(client);
    gateway.stop().await.unwrap();
    primary.stop().await;
    backup.stop().await;
}
