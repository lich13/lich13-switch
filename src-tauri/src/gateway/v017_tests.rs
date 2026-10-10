use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const CREATE: &str = r#"{ "type":"response.create", "model":"fixture-model", "input":["fixture"], "future":{"keep":true} }"#;
const CREATED: &str =
    r#"{"type":"response.created","response":{"id":"fixture-response","model":"fixture-model"}}"#;
const COMPLETED: &str = r#"{"type":"response.completed","response":{"id":"fixture-response","model":"fixture-model","status":"completed"}}"#;
const CAPACITY_EVENT: &str = r#"{"type":"error","error":{"code":"rate_limit_exceeded","message":"Selected model is at capacity","status":429}}"#;

fn configure(g: &Gateway, t: &tempfile::TempDir, retries: usize) {
    update(
        g,
        t,
        Edit::Mode {
            mode: "auto".into(),
        },
    );
    update(
        g,
        t,
        Edit::Settings {
            settings: Settings {
                max_retries: retries,
                capacity_retry_seconds: 1,
                websocket_retry_seconds: 1,
                queue_seconds: 1,
                rate_limit_seconds: 1,
                first_byte_seconds: 3,
                idle_seconds: 3,
                total_seconds: 3,
                failure_threshold: 10,
                ..g.view().settings
            },
        },
    );
    for provider in g.view().providers {
        update(
            g,
            t,
            Edit::RpmProvider {
                id: provider.id,
                max_rpm: 20,
            },
        );
    }
}

async fn until(condition: impl Fn() -> bool) {
    tokio::time::timeout(Duration::from_secs(4), async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("fixture state did not settle");
}

fn capacity_response() -> Response<WireBody> {
    Response::builder()
        .status(429)
        .header("retry-after", "0")
        .body(full("Selected model is at capacity"))
        .unwrap()
}

#[tokio::test]
async fn http_200_sse_capacity_before_output_retries_five_rounds_without_committing_error_events() {
    let hits = Arc::new(AtomicUsize::new(0));
    let count = hits.clone();
    let upstream = server(move |_| {
        let attempt = count.fetch_add(1, Ordering::SeqCst);
        async move {
            let body = if attempt < 5 {
                format!("event: error\ndata: {CAPACITY_EVENT}\n\n")
            } else {
                format!("event: response.created\ndata: {CREATED}\n\nevent: response.completed\ndata: {COMPLETED}\n\n")
            };
            Response::builder().header("content-type", "text/event-stream").body(full(body)).unwrap()
        }
    }).await;
    let (t, g) = fixture(vec![format!("http://127.0.0.1:{upstream}/v1")]).await;
    configure(&g, &t, 0);
    start(&g, &t).await;
    let response = tokio::time::timeout(
        Duration::from_secs(10),
        request(
            &g,
            "/v1/responses",
            br#"{"model":"fixture-model","stream":true}"#.to_vec(),
            vec![("content-type", "application/json")],
        ),
    )
    .await
    .expect("SSE capacity retries did not reach recovery");
    assert_eq!(response.status(), 200);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let text = std::str::from_utf8(&bytes).unwrap();
    assert!(text.contains("response.created"));
    assert!(text.contains("response.completed"));
    assert!(!text.contains("rate_limit_exceeded"));
    assert_eq!(hits.load(Ordering::SeqCst), 6);
    assert_eq!(g.view().providers[0].rpm_used, 6);
    assert_eq!(g.view().providers[0].health.failures, 0);
    g.stop().await.unwrap();
}

#[tokio::test]
async fn http_200_sse_created_before_capacity_error_is_forwarded_without_retry() {
    let hits = Arc::new(AtomicUsize::new(0));
    let count = hits.clone();
    let upstream = server(move |_| {
        count.fetch_add(1, Ordering::SeqCst);
        async {
            let body = format!(
                "event: response.created\ndata: {CREATED}\n\nevent: error\ndata: {CAPACITY_EVENT}\n\n"
            );
            Response::builder()
                .header("content-type", "text/event-stream")
                .body(full(body))
                .unwrap()
        }
    })
    .await;
    let (t, g) = fixture(vec![format!("http://127.0.0.1:{upstream}/v1")]).await;
    configure(&g, &t, 3);
    start(&g, &t).await;
    let response = tokio::time::timeout(
        Duration::from_secs(2),
        request(
            &g,
            "/v1/responses",
            br#"{"model":"fixture-model","stream":true}"#.to_vec(),
            vec![("content-type", "application/json")],
        ),
    )
    .await
    .expect("SSE response after response.created was retried or stalled");
    assert_eq!(response.status(), 200);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let text = std::str::from_utf8(&bytes).unwrap();
    assert!(text.contains("response.created"));
    assert!(text.contains("rate_limit_exceeded"));
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    assert!(g.view().capacity_retries.is_empty());
    assert_eq!(g.view().providers[0].health.failures, 0);
    g.stop().await.unwrap();
}

#[tokio::test]
async fn ordinary_and_permanent_http_200_sse_errors_remain_neutral_and_do_not_wait_forever() {
    for event in [
        r#"{"type":"error","error":{"code":"fixture_application_error","message":"fixture application failure"}}"#,
        r#"{"type":"error","error":{"code":"insufficient_quota","message":"insufficient balance","status":429}}"#,
    ] {
        let hits = Arc::new(AtomicUsize::new(0));
        let count = hits.clone();
        let upstream = server(move |_| {
            count.fetch_add(1, Ordering::SeqCst);
            async move {
                Response::builder()
                    .header("content-type", "text/event-stream")
                    .body(full(format!("event: error\ndata: {event}\n\n")))
                    .unwrap()
            }
        })
        .await;
        let (t, g) = fixture(vec![format!("http://127.0.0.1:{upstream}/v1")]).await;
        configure(&g, &t, 0);
        start(&g, &t).await;
        let response = tokio::time::timeout(
            Duration::from_secs(2),
            request(
                &g,
                "/v1/responses",
                br#"{"model":"fixture-model","stream":true}"#.to_vec(),
                vec![("content-type", "application/json")],
            ),
        )
        .await
        .expect("non-capacity SSE error entered an unlimited wait");
        assert_eq!(response.status(), 200);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        assert!(std::str::from_utf8(&bytes).unwrap().contains(event));
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        assert_eq!(g.view().providers[0].health.failures, 0);
        assert!(g.view().capacity_retries.is_empty());
        g.stop().await.unwrap();
    }
}

#[tokio::test]
async fn http_capacity_retries_beyond_five_rounds_even_with_zero_retry_budget() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let capture = seen.clone();
    let upstream = server(move |request| {
        let capture = capture.clone();
        async move {
            let bytes = request.into_body().collect().await.unwrap().to_bytes();
            let count = {
                let mut seen = capture.lock().unwrap();
                seen.push(bytes);
                seen.len()
            };
            if count <= 5 {
                capacity_response()
            } else {
                Response::new(full("fixture-recovered"))
            }
        }
    })
    .await;
    let (t, g) = fixture(vec![format!("http://127.0.0.1:{upstream}/v1")]).await;
    configure(&g, &t, 0);
    start(&g, &t).await;
    let original =
        br#"{ "model":"fixture-model", "input":["original"], "future":{"keep":true} }"#.to_vec();
    let response = tokio::time::timeout(
        Duration::from_secs(10),
        request(
            &g,
            "/v1/responses?future=keep",
            original.clone(),
            vec![("content-type", "application/json")],
        ),
    )
    .await
    .expect("capacity retries ended or stalled before recovery");
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.into_body().collect().await.unwrap().to_bytes(),
        "fixture-recovered"
    );
    let requests = seen.lock().unwrap().clone();
    assert_eq!(requests.len(), 6);
    assert!(requests
        .iter()
        .all(|body| body.as_ref() == original.as_slice()));
    let view = g.view();
    assert_eq!(view.providers[0].rpm_used, 6);
    assert_eq!(view.providers[0].health.failures, 0);
    assert_eq!(
        view.providers[0].health.state,
        circuit::CircuitState::Closed
    );
    assert_eq!(view.providers[0].active_requests, 0);
    assert_eq!(view.waiting_requests, 0);
    assert!(view.capacity_retries.is_empty());
    g.stop().await.unwrap();
}

#[tokio::test]
async fn capacity_protection_survives_a_later_cooldown_beyond_the_one_second_queue_budget() {
    let hits = Arc::new(AtomicUsize::new(0));
    let count = hits.clone();
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let upstream = server(move |_| {
        let count = count.clone();
        let barrier = barrier.clone();
        async move {
            let index = count.fetch_add(1, Ordering::SeqCst);
            if index < 2 {
                barrier.wait().await;
                if index == 1 {
                    tokio::time::sleep(Duration::from_millis(300)).await;
                }
                Response::builder()
                    .status(429)
                    .header("retry-after", if index == 0 { "0" } else { "3" })
                    .body(full("Selected model is at capacity"))
                    .unwrap()
            } else {
                Response::new(full("fixture-recovered"))
            }
        }
    })
    .await;
    let (t, g) = fixture(vec![format!("http://127.0.0.1:{upstream}/v1")]).await;
    configure(&g, &t, 0);
    start(&g, &t).await;
    let began = Instant::now();
    let tasks: Vec<_> = (0..2)
        .map(|_| {
            let g = g.clone();
            tokio::spawn(async move {
                request(
                    &g,
                    "/v1/responses",
                    br#"{"model":"fixture-model"}"#.to_vec(),
                    vec![("content-type", "application/json")],
                )
                .await
            })
        })
        .collect();
    for task in tasks {
        let response = tokio::time::timeout(Duration::from_secs(8), task)
            .await
            .expect("extended capacity cooldown did not recover")
            .unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(
            response.into_body().collect().await.unwrap().to_bytes(),
            "fixture-recovered"
        );
    }
    assert!(began.elapsed() >= Duration::from_secs(3));
    assert_eq!(hits.load(Ordering::SeqCst), 4);
    assert_eq!(g.view().providers[0].rpm_used, 4);
    assert_eq!(g.view().waiting_requests, 0);
    g.stop().await.unwrap();
}

#[tokio::test]
async fn capacity_rounds_do_not_reset_the_ordinary_failure_budget() {
    let limited = Arc::new(AtomicUsize::new(0));
    let count = limited.clone();
    let first = server(move |_| {
        count.fetch_add(1, Ordering::SeqCst);
        async { capacity_response() }
    })
    .await;
    let failed = Arc::new(AtomicUsize::new(0));
    let count = failed.clone();
    let second = server(move |_| {
        count.fetch_add(1, Ordering::SeqCst);
        async {
            Response::builder()
                .status(502)
                .header("x-fixture", "last-error")
                .body(full("fixture ordinary failure"))
                .unwrap()
        }
    })
    .await;
    let (t, g) = fixture(vec![
        format!("http://127.0.0.1:{first}/v1"),
        format!("http://127.0.0.1:{second}/v1"),
    ])
    .await;
    configure(&g, &t, 1);
    start(&g, &t).await;
    let response = tokio::time::timeout(
        Duration::from_secs(5),
        request(
            &g,
            "/v1/responses",
            br#"{"model":"fixture-model"}"#.to_vec(),
            vec![("content-type", "application/json")],
        ),
    )
    .await
    .expect("ordinary failures must still exhaust the retry budget");
    assert_eq!(response.status(), 502);
    assert_eq!(response.headers()["x-fixture"], "last-error");
    assert_eq!(
        response.into_body().collect().await.unwrap().to_bytes(),
        "fixture ordinary failure"
    );
    assert_eq!(limited.load(Ordering::SeqCst), 2);
    assert_eq!(failed.load(Ordering::SeqCst), 2);
    assert_eq!(g.view().providers[0].health.failures, 0);
    assert_eq!(g.view().providers[1].health.failures, 2);
    g.stop().await.unwrap();
}

#[tokio::test]
async fn permanent_429_errors_return_original_body_without_entering_capacity_wait() {
    for body in [
        r#"{"error":{"code":"insufficient_quota","message":"insufficient balance"}}"#,
        r#"{"error":{"code":"account_disabled","message":"account disabled"}}"#,
    ] {
        let hits = Arc::new(AtomicUsize::new(0));
        let count = hits.clone();
        let upstream = server(move |_| {
            count.fetch_add(1, Ordering::SeqCst);
            async move {
                Response::builder()
                    .status(429)
                    .header("x-original", "fixture")
                    .body(full(body))
                    .unwrap()
            }
        })
        .await;
        let (t, g) = fixture(vec![format!("http://127.0.0.1:{upstream}/v1")]).await;
        configure(&g, &t, 3);
        start(&g, &t).await;
        let response = tokio::time::timeout(
            Duration::from_secs(2),
            request(
                &g,
                "/v1/responses",
                br#"{"model":"fixture-model"}"#.to_vec(),
                vec![("content-type", "application/json")],
            ),
        )
        .await
        .expect("permanent rejection must not become an unlimited capacity retry");
        assert_eq!(response.status(), 429);
        assert_eq!(response.headers()["x-original"], "fixture");
        assert_eq!(
            response.into_body().collect().await.unwrap().to_bytes(),
            body
        );
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        assert!(g.view().capacity_retries.is_empty());
        assert_eq!(g.view().waiting_requests, 0);
        g.stop().await.unwrap();
    }
}

async fn raw_request(g: &Gateway) -> tokio::net::TcpStream {
    let token = g.0.inner.lock().unwrap().store.local_token.clone();
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", g.view().settings.port))
        .await
        .unwrap();
    let body = r#"{"model":"fixture-model"}"#;
    stream.write_all(format!("POST /v1/responses HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
    stream
}

#[tokio::test]
async fn unbounded_http_capacity_wait_releases_slots_and_obeys_cancel_and_gateway_stop() {
    for stop in [false, true] {
        let hits = Arc::new(AtomicUsize::new(0));
        let count = hits.clone();
        let upstream = server(move |_| {
            count.fetch_add(1, Ordering::SeqCst);
            async { capacity_response() }
        })
        .await;
        let (t, g) = fixture(vec![format!("http://127.0.0.1:{upstream}/v1")]).await;
        configure(&g, &t, 0);
        update(
            &g,
            &t,
            Edit::Settings {
                settings: Settings {
                    capacity_retry_seconds: 60,
                    ..g.view().settings
                },
            },
        );
        start(&g, &t).await;
        let mut stream = raw_request(&g).await;
        until(|| !g.view().capacity_retries.is_empty()).await;
        assert_eq!(g.view().waiting_requests, 1);
        assert_eq!(g.view().providers[0].active_requests, 0);
        assert_eq!(g.view().providers[0].rpm_used, 1);
        if stop {
            g.stop().await.unwrap();
            let mut response = Vec::new();
            tokio::time::timeout(Duration::from_secs(2), stream.read_to_end(&mut response))
                .await
                .unwrap()
                .unwrap();
            assert!(String::from_utf8_lossy(&response).contains("STOPPED"));
        }
        drop(stream);
        until(|| g.view().waiting_requests == 0 && g.view().active_connections == 0).await;
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        assert!(g.view().capacity_retries.is_empty());
        if !stop {
            g.stop().await.unwrap();
        }
    }
}

#[tokio::test]
async fn capacity_retries_wait_at_the_rpm_limit_without_an_extra_upstream_attempt() {
    let hits = Arc::new(AtomicUsize::new(0));
    let count = hits.clone();
    let upstream = server(move |_| {
        count.fetch_add(1, Ordering::SeqCst);
        async { capacity_response() }
    })
    .await;
    let (t, g) = fixture(vec![format!("http://127.0.0.1:{upstream}/v1")]).await;
    configure(&g, &t, 0);
    update(
        &g,
        &t,
        Edit::RpmProvider {
            id: g.view().providers[0].id.clone(),
            max_rpm: 1,
        },
    );
    start(&g, &t).await;
    let stream = raw_request(&g).await;
    until(|| !g.view().capacity_retries.is_empty()).await;
    until(|| {
        g.view().capacity_retries.is_empty()
            && g.view().waiting_requests == 1
            && g.view().providers[0].rpm_limited
    })
    .await;
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    assert_eq!(g.view().providers[0].active_requests, 0);
    assert_eq!(g.view().providers[0].health.failures, 0);
    drop(stream);
    until(|| g.view().waiting_requests == 0 && g.view().active_connections == 0).await;
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    g.stop().await.unwrap();
}

struct NativeSeen {
    connections: AtomicUsize,
    frames: Mutex<Vec<Vec<u8>>>,
}

async fn native_server<F, Fut>(handshake_rejections: usize, handler: F) -> (u16, Arc<NativeSeen>)
where
    F: Fn(usize, yawc::HttpWebSocket, Arc<NativeSeen>) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let seen = Arc::new(NativeSeen {
        connections: AtomicUsize::new(0),
        frames: Mutex::new(vec![]),
    });
    let shared = seen.clone();
    let handler = Arc::new(handler);
    let port = server(move |mut request| {
        let seen = shared.clone();
        let handler = handler.clone();
        async move {
            let connection = seen.connections.fetch_add(1, Ordering::SeqCst) + 1;
            if connection <= handshake_rejections {
                return capacity_response();
            }
            let (response, upgrade) = yawc::WebSocket::upgrade_with_options(
                &mut request,
                yawc::Options::default().with_balanced_compression(),
            )
            .unwrap();
            tokio::spawn(async move {
                if let Ok(ws) = upgrade.await {
                    handler(connection, ws, seen).await;
                }
            });
            response.map(|_| replay::empty())
        }
    })
    .await;
    (port, seen)
}

async fn next_ws<S>(socket: &mut S) -> Option<yawc::Frame>
where
    S: futures_util::Stream<Item = yawc::Frame> + Unpin,
{
    tokio::time::timeout(Duration::from_secs(10), socket.next())
        .await
        .expect("fixture websocket timed out")
}

async fn capture_request(socket: &mut yawc::HttpWebSocket, seen: &NativeSeen) {
    loop {
        let frame = next_ws(socket)
            .await
            .expect("upstream closed before response.create");
        if !frame.opcode().is_control() {
            seen.frames.lock().unwrap().push(frame.payload().to_vec());
            break;
        }
    }
}

async fn complete_native(mut socket: yawc::HttpWebSocket) {
    socket
        .send(yawc::Frame::text(CREATED.to_owned()))
        .await
        .unwrap();
    socket
        .send(yawc::Frame::text(COMPLETED.to_owned()))
        .await
        .unwrap();
    while let Some(frame) = socket.next().await {
        if frame.opcode() == yawc::OpCode::Close {
            break;
        }
    }
}

#[tokio::test]
async fn websocket_terminal_failure_keeps_provider_for_notification_coalescing() {
    use crate::events::{Action, Filter, Reason, Service};

    for (provider_count, retries) in [(2, 0), (1, 3)] {
        let hits = Arc::new(Mutex::new(Vec::new()));
        let observed = hits.clone();
        let port = server(move |request| {
            observed
                .lock()
                .unwrap()
                .push(request.uri().path().to_owned());
            async {
                Response::builder()
                    .status(502)
                    .body(full("fixture handshake rejection"))
                    .unwrap()
            }
        })
        .await;
        let mut urls = vec![format!("http://127.0.0.1:{port}/primary/v1")];
        if provider_count == 2 {
            urls.push(format!("http://127.0.0.1:{port}/backup/v1"));
        }
        let (t, g) = fixture(urls).await;
        configure(&g, &t, retries);
        update(
            &g,
            &t,
            Edit::Settings {
                settings: Settings {
                    transient_failure_threshold: 1,
                    ..g.view().settings
                },
            },
        );
        if provider_count == 2 {
            update(
                &g,
                &t,
                Edit::Settings {
                    settings: Settings {
                        failure_threshold: 1,
                        ..g.view().settings
                    },
                },
            );
        }
        let primary = g.view().providers[0].id.clone();
        let service = Service::new(t.path());
        g.set_diagnostics(service.clone());
        start(&g, &t).await;
        let mut client = concurrency::responses_client(&g).await;
        client
            .send(yawc::Frame::text(CREATE.to_owned()))
            .await
            .unwrap();
        let frame = next_ws(&mut client)
            .await
            .expect("terminal handshake failure did not close downstream");
        assert_eq!(frame.opcode(), yawc::OpCode::Close);
        assert_eq!(frame.close_code().map(u16::from), Some(1013));
        drop(client);
        until(|| {
            service
                .query(Filter::default())
                .items
                .iter()
                .any(|event| event.reason == Reason::FailoverExhausted)
        })
        .await;
        let events = service.query(Filter::default());
        assert!(events.error.is_none());
        let terminal = events
            .items
            .iter()
            .find(|event| event.reason == Reason::FailoverExhausted)
            .unwrap();
        assert_eq!(terminal.client_id, Some(ClientId::Codex));
        assert_eq!(terminal.provider_id.as_deref(), Some(primary.as_str()));
        assert_eq!(terminal.model.as_deref(), Some("fixture-model"));
        assert_eq!(terminal.action, Action::Returned);
        assert_eq!(terminal.attempt, Some(1));
        if provider_count == 2 {
            g.report_diagnostics();
            until(|| {
                service
                    .query(Filter::default())
                    .items
                    .iter()
                    .any(|event| event.reason == Reason::CircuitOpen)
            })
            .await;
            let events = service.query(Filter::default());
            assert!(events.error.is_none());
            let circuit = events
                .items
                .iter()
                .find(|event| event.reason == Reason::CircuitOpen)
                .unwrap();
            assert_eq!(circuit.client_id, terminal.client_id);
            assert_eq!(circuit.provider_id, terminal.provider_id);
            assert_eq!(circuit.model, terminal.model);
        }
        until(|| {
            let view = g.view();
            view.active_connections == 0
                && view.waiting_requests == 0
                && view
                    .providers
                    .iter()
                    .all(|provider| provider.active_requests == 0)
        })
        .await;
        assert_eq!(
            hits.lock().unwrap().as_slice(),
            &["/primary/v1/responses".to_owned()]
        );
        let view = g.view();
        assert_eq!(view.providers[0].rpm_used, 1);
        assert!(view
            .providers
            .iter()
            .skip(1)
            .all(|provider| provider.rpm_used == 0));
        g.stop().await.unwrap();
    }
}

#[tokio::test]
async fn websocket_handshake_capacity_can_recover_after_five_rejections_with_zero_budget() {
    let (port, seen) = native_server(5, |_connection, mut socket, seen| async move {
        capture_request(&mut socket, &seen).await;
        complete_native(socket).await;
    })
    .await;
    let (t, g) = fixture(vec![format!("http://127.0.0.1:{port}/v1")]).await;
    configure(&g, &t, 0);
    start(&g, &t).await;
    let mut client = concurrency::responses_client(&g).await;
    client
        .send(yawc::Frame::text(CREATE.to_owned()))
        .await
        .unwrap();
    assert_eq!(next_ws(&mut client).await.unwrap().as_str(), CREATED);
    assert_eq!(next_ws(&mut client).await.unwrap().as_str(), COMPLETED);
    assert_eq!(seen.connections.load(Ordering::SeqCst), 6);
    assert_eq!(seen.frames.lock().unwrap().as_slice(), &[CREATE.as_bytes()]);
    assert_eq!(g.view().providers[0].rpm_used, 6);
    assert_eq!(g.view().providers[0].health.failures, 0);
    client
        .send(yawc::Frame::close(1000.into(), "fixture done"))
        .await
        .unwrap();
    drop(client);
    g.stop().await.unwrap();
}

#[tokio::test]
async fn websocket_capacity_before_first_event_replays_exact_bytes_on_the_same_provider() {
    let (port, seen) = native_server(0, |connection, mut socket, seen| async move {
        capture_request(&mut socket, &seen).await;
        if connection <= 5 {
            socket
                .send(yawc::Frame::text(CAPACITY_EVENT.to_owned()))
                .await
                .unwrap();
        } else {
            complete_native(socket).await;
        }
    })
    .await;
    let (backup, backup_seen) = native_server(0, |_connection, mut socket, seen| async move {
        capture_request(&mut socket, &seen).await;
        complete_native(socket).await;
    })
    .await;
    let (t, g) = fixture(vec![
        format!("http://127.0.0.1:{port}/v1"),
        format!("http://127.0.0.1:{backup}/v1"),
    ])
    .await;
    configure(&g, &t, 0);
    start(&g, &t).await;
    let mut client = concurrency::responses_client(&g).await;
    client
        .send(yawc::Frame::text(CREATE.to_owned()))
        .await
        .unwrap();
    assert_eq!(next_ws(&mut client).await.unwrap().as_str(), CREATED);
    assert_eq!(next_ws(&mut client).await.unwrap().as_str(), COMPLETED);
    assert_eq!(seen.connections.load(Ordering::SeqCst), 6);
    assert_eq!(
        seen.frames.lock().unwrap().as_slice(),
        &[CREATE.as_bytes(); 6]
    );
    assert_eq!(backup_seen.connections.load(Ordering::SeqCst), 0);
    assert_eq!(g.view().providers[0].rpm_used, 6);
    assert_eq!(g.view().providers[1].rpm_used, 0);
    client
        .send(yawc::Frame::close(1000.into(), "fixture done"))
        .await
        .unwrap();
    drop(client);
    g.stop().await.unwrap();
}

#[tokio::test]
async fn websocket_capacity_after_response_created_is_delivered_without_replay() {
    let (port, seen) = native_server(0, |_connection, mut socket, seen| async move {
        capture_request(&mut socket, &seen).await;
        socket
            .send(yawc::Frame::text(CREATED.to_owned()))
            .await
            .unwrap();
        socket
            .send(yawc::Frame::text(CAPACITY_EVENT.to_owned()))
            .await
            .unwrap();
        let _ = socket
            .send(yawc::Frame::close(
                1011.into(),
                "fixture failure after output",
            ))
            .await;
    })
    .await;
    let (t, g) = fixture(vec![format!("http://127.0.0.1:{port}/v1")]).await;
    configure(&g, &t, 3);
    start(&g, &t).await;
    let mut client = concurrency::responses_client(&g).await;
    client
        .send(yawc::Frame::text(CREATE.to_owned()))
        .await
        .unwrap();
    assert_eq!(next_ws(&mut client).await.unwrap().as_str(), CREATED);
    assert_eq!(next_ws(&mut client).await.unwrap().as_str(), CAPACITY_EVENT);
    assert_eq!(
        next_ws(&mut client).await.unwrap().opcode(),
        yawc::OpCode::Close
    );
    assert_eq!(seen.connections.load(Ordering::SeqCst), 1);
    assert_eq!(seen.frames.lock().unwrap().len(), 1);
    assert_eq!(g.view().providers[0].rpm_used, 1);
    drop(client);
    g.stop().await.unwrap();
}

#[tokio::test]
async fn websocket_permanent_429_event_does_not_reconnect_forever() {
    let (port, seen) = native_server(0, |_connection, mut socket, seen| async move {
        capture_request(&mut socket, &seen).await;
        let _ = socket.send(yawc::Frame::text(r#"{"type":"error","error":{"code":"insufficient_quota","status":429,"message":"insufficient balance"}}"#.to_owned())).await;
    }).await;
    let (t, g) = fixture(vec![format!("http://127.0.0.1:{port}/v1")]).await;
    configure(&g, &t, 3);
    start(&g, &t).await;
    let mut client = concurrency::responses_client(&g).await;
    client
        .send(yawc::Frame::text(CREATE.to_owned()))
        .await
        .unwrap();
    let frame = tokio::time::timeout(Duration::from_secs(2), client.next())
        .await
        .expect("permanent rejection entered capacity retry")
        .unwrap();
    assert!(frame.as_str().contains("insufficient_quota"));
    assert_eq!(
        next_ws(&mut client).await.unwrap().opcode(),
        yawc::OpCode::Close
    );
    assert_eq!(seen.connections.load(Ordering::SeqCst), 1);
    assert_eq!(seen.frames.lock().unwrap().len(), 1);
    drop(client);
    g.stop().await.unwrap();
}

#[tokio::test]
async fn websocket_capacity_wait_obeys_response_cancel_and_gateway_stop_with_zero_budget() {
    for stop in [false, true] {
        let (port, seen) = native_server(0, |_connection, mut socket, seen| async move {
            capture_request(&mut socket, &seen).await;
            let _ = socket
                .send(yawc::Frame::text(CAPACITY_EVENT.to_owned()))
                .await;
        })
        .await;
        let (t, g) = fixture(vec![format!("http://127.0.0.1:{port}/v1")]).await;
        configure(&g, &t, 0);
        update(
            &g,
            &t,
            Edit::Settings {
                settings: Settings {
                    capacity_retry_seconds: 60,
                    ..g.view().settings
                },
            },
        );
        start(&g, &t).await;
        let mut client = concurrency::responses_client(&g).await;
        client
            .send(yawc::Frame::text(CREATE.to_owned()))
            .await
            .unwrap();
        until(|| g.view().waiting_requests == 1 && g.view().providers[0].active_requests == 0)
            .await;
        if stop {
            g.stop().await.unwrap();
        } else {
            client
                .send(yawc::Frame::text(
                    r#"{"type":"response.cancel"}"#.to_owned(),
                ))
                .await
                .unwrap();
        }
        let frame = tokio::time::timeout(Duration::from_secs(2), client.next())
            .await
            .expect("capacity wait ignored cancellation")
            .unwrap();
        if stop {
            assert_eq!(frame.opcode(), yawc::OpCode::Close);
            assert_eq!(frame.close_code().map(u16::from), Some(1012));
        } else {
            assert!(frame.as_str().contains("response.cancelled"));
        }
        drop(client);
        until(|| g.view().waiting_requests == 0 && g.view().active_connections == 0).await;
        assert_eq!(seen.connections.load(Ordering::SeqCst), 1);
        assert_eq!(g.view().providers[0].rpm_used, 1);
        if !stop {
            g.stop().await.unwrap();
        }
    }
}
