//! Real loopback TCP regressions for first-byte retry boundaries.
use super::{Edit, Gateway};
use crate::{
    events::{
        Action, ClassifiedRecord, Filter as EventFilter, Phase, Reason as EventReason,
        Service as EventService,
    },
    usage::{
        model::{EndCause, Filter as UsageFilter, Record as UsageRecord},
        Service as UsageService,
    },
};
use futures_util::{SinkExt, StreamExt};
use std::{
    io,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

const REQUEST_BODY: &[u8] = br#"{"model":"fixture-model","input":"fixture","stream":true}"#;
const WAIT: Duration = Duration::from_secs(5);
const COMPLETED: &[u8] =
    b"data: {\"type\":\"response.completed\",\"response\":{\"id\":\"fixture-completed\",\"status\":\"completed\"}}\n\n";

async fn fixture(urls: Vec<String>) -> (tempfile::TempDir, Gateway) {
    let dir = tempfile::tempdir().unwrap();
    let primary = urls.first().cloned().unwrap();
    std::fs::write(
        dir.path().join("config.toml"),
        format!(
            "# fixture\nmodel_provider='custom'\n[model_providers.custom]\nbase_url = {}\nexperimental_bearer_token = 'fixture-original-token'\nwire_api='responses'\nsupports_websockets=true\n",
            serde_json::to_string(&primary).unwrap()
        ),
    )
    .unwrap();
    let gateway = Gateway::new(dir.path().to_path_buf()).unwrap();
    let reservation = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = reservation.local_addr().unwrap().port();
    drop(reservation);
    let mut settings = gateway.view().settings;
    settings.port = port;
    settings.max_retries = 1;
    settings.first_byte_seconds = 1;
    settings.idle_seconds = 5;
    settings.total_seconds = 10;
    settings.connect_seconds = 2;
    settings.queue_seconds = 1;
    settings.websocket_retry_seconds = 1;
    edit(&gateway, &dir, Edit::Settings { settings });
    for url in urls {
        edit(
            &gateway,
            &dir,
            Edit::SaveProvider {
                id: None,
                base_url: url,
                token: "fixture-upstream-token".into(),
                name: Some("fixture-upstream".into()),
            },
        );
    }
    edit(
        &gateway,
        &dir,
        Edit::Mode {
            mode: "auto".into(),
        },
    );
    std::fs::write(dir.path().join("auth.json"), "fixture-only").unwrap();
    gateway
        .start(&gateway.view().revision, dir.path())
        .await
        .unwrap();
    (dir, gateway)
}

fn edit(gateway: &Gateway, dir: &tempfile::TempDir, edit: Edit) {
    gateway
        .edit(edit, &gateway.view().revision, dir.path())
        .unwrap();
}

fn local_auth(gateway: &Gateway) -> (u16, String) {
    let port = gateway.view().settings.port;
    let token = gateway.0.inner.lock().unwrap().store.local_token.clone();
    (port, token)
}

fn attach_observers(dir: &tempfile::TempDir, gateway: &Gateway) -> (UsageService, EventService) {
    let usage = UsageService::new(&dir.path().join("usage"));
    let events = EventService::new(&dir.path().join("events"));
    gateway.set_usage(usage.clone());
    gateway.set_diagnostics(events.clone());
    (usage, events)
}

fn usage_records(service: &UsageService) -> Vec<UsageRecord> {
    service
        .query(|store| {
            let page = store.logs(&UsageFilter::default())?;
            assert_eq!(page.total, page.rows.len() as u64);
            Ok(page.rows)
        })
        .unwrap()
}

async fn wait_for_usage_record(service: &UsageService) -> UsageRecord {
    let mut events = service.subscribe();
    tokio::time::timeout(WAIT, async {
        loop {
            let rows = usage_records(service);
            if let Some(record) = rows.into_iter().next() {
                assert_eq!(usage_records(service).len(), 1);
                assert!(service.state().error.is_none());
                return record;
            }
            let _ = events.recv().await;
        }
    })
    .await
    .expect("timed request usage record was not written")
}

fn network_diagnostics(service: &EventService) -> Vec<ClassifiedRecord> {
    service
        .query(EventFilter {
            reason: Some(EventReason::Network),
            ..EventFilter::default()
        })
        .items
}

async fn wait_for_timeout_diagnostic(service: &EventService) -> ClassifiedRecord {
    let mut events = service.subscribe();
    tokio::time::timeout(WAIT, async {
        loop {
            let rows = network_diagnostics(service);
            if let Some(record) = rows
                .into_iter()
                .find(|record| record.details.local_code.as_deref() == Some("FIRST_BYTE_TIMEOUT"))
            {
                return record;
            }
            let _ = events.recv().await;
        }
    })
    .await
    .expect("first-byte timeout diagnostic was not written")
}

fn assert_timeout_usage(record: &UsageRecord, status: Option<u16>, phase: &str) {
    let attempt = record
        .final_attempt()
        .expect("request should have one attempt");
    assert_eq!(attempt.outcome, "failure");
    assert_eq!(attempt.status, status);
    let end = attempt.end.as_ref().expect("timeout should settle usage");
    assert_eq!(end.cause, EndCause::FirstByteTimeout);
    assert!(!end.terminal_received);
    assert_eq!(
        attempt
            .transfer
            .as_ref()
            .and_then(|transfer| transfer.timeout_phase.as_deref()),
        Some(phase)
    );
    assert_eq!(
        attempt
            .transfer
            .as_ref()
            .and_then(|transfer| transfer.timeout_seconds),
        Some(1)
    );
}

fn assert_timeout_diagnostic(record: &ClassifiedRecord, status: Option<u16>, phase: Phase) {
    assert_eq!(record.reason, EventReason::Network);
    assert_eq!(record.status, status);
    assert_eq!(record.action, Action::Returned);
    assert_eq!(
        record.details.timeout_phase.as_deref(),
        Some(match phase {
            Phase::Headers => "response_headers",
            Phase::Response | Phase::Stream => "first_byte",
            Phase::WsReceive => "ws_first_byte",
            _ => panic!("unexpected timeout phase: {phase:?}"),
        })
    );
    assert_eq!(record.details.timeout_seconds, Some(1));
    assert!(record.details.counted_failure.is_some());
}

async fn read_http_request(stream: &mut TcpStream) -> io::Result<()> {
    let mut header = Vec::new();
    let mut byte = [0u8; 1];
    while !header.windows(4).any(|window| window == b"\r\n\r\n") {
        if stream.read(&mut byte).await? == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "request ended in headers",
            ));
        }
        header.push(byte[0]);
        if header.len() > 32 * 1024 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "request headers too large",
            ));
        }
    }
    let header = String::from_utf8_lossy(&header).to_ascii_lowercase();
    let length = header
        .lines()
        .find_map(|line| line.strip_prefix("content-length:"))
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    if length != 0 {
        let mut body = vec![0; length];
        stream.read_exact(&mut body).await?;
    }
    Ok(())
}

async fn request_bytes(gateway: &Gateway, path: &str, body: &[u8]) -> Vec<u8> {
    let (port, token) = local_auth(gateway);
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let head = format!(
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await.unwrap();
    stream.write_all(body).await.unwrap();
    let mut response = Vec::new();
    tokio::time::timeout(WAIT, stream.read_to_end(&mut response))
        .await
        .expect("gateway should finish the request after the first-byte deadline")
        .unwrap();
    response
}

async fn respond(stream: &mut TcpStream, status: &str, content_type: &str, body: &[u8]) {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await.unwrap();
    stream.write_all(body).await.unwrap();
    stream.flush().await.unwrap();
}

async fn wait_for_hit(listener: TcpListener, hits: Arc<AtomicUsize>, response: &'static [u8]) {
    if let Ok(Ok((mut stream, _))) =
        tokio::time::timeout(Duration::from_secs(4), listener.accept()).await
    {
        hits.fetch_add(1, Ordering::SeqCst);
        if read_http_request(&mut stream).await.is_ok() {
            respond(&mut stream, "200 OK", "text/event-stream", response).await;
        }
    }
}

async fn observe_http_requests(
    listener: TcpListener,
    hits: Arc<AtomicUsize>,
    seen_tx: tokio::sync::oneshot::Sender<Instant>,
) {
    let mut seen_tx = Some(seen_tx);
    loop {
        let (mut stream, _) = listener.accept().await.unwrap();
        let attempt = hits.fetch_add(1, Ordering::SeqCst);
        if read_http_request(&mut stream).await.is_ok() {
            if attempt == 0 {
                if let Some(tx) = seen_tx.take() {
                    let _ = tx.send(Instant::now());
                }
            }
            tokio::spawn(async move {
                let mut bytes = [0u8; 256];
                loop {
                    match stream.read(&mut bytes).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                }
            });
        }
    }
}

async fn serve_empty_200_then_count_retries(
    listener: TcpListener,
    hits: Arc<AtomicUsize>,
    seen_tx: tokio::sync::oneshot::Sender<Instant>,
) {
    let (mut stream, _) = listener.accept().await.unwrap();
    hits.fetch_add(1, Ordering::SeqCst);
    read_http_request(&mut stream).await.unwrap();
    stream
        .write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: keep-alive\r\n\r\n",
        )
        .await
        .unwrap();
    stream.flush().await.unwrap();
    let _ = seen_tx.send(Instant::now());
    tokio::spawn(async move {
        let mut byte = [0u8; 1];
        while stream.read(&mut byte).await.unwrap_or(0) != 0 {}
    });

    loop {
        let (mut retry, _) = listener.accept().await.unwrap();
        hits.fetch_add(1, Ordering::SeqCst);
        if read_http_request(&mut retry).await.is_ok() {
            tokio::spawn(async move {
                let mut byte = [0u8; 1];
                while retry.read(&mut byte).await.unwrap_or(0) != 0 {}
            });
        }
    }
}

fn response_status(response: &[u8]) -> Option<u16> {
    let first = std::str::from_utf8(response.split(|byte| *byte == b'\n').next()?).ok()?;
    first.split_whitespace().nth(1)?.parse().ok()
}

#[tokio::test]
async fn request_received_before_delayed_headers_does_not_fail_over_on_timeout() {
    let primary = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let primary_port = primary.local_addr().unwrap().port();
    let backup = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let backup_port = backup.local_addr().unwrap().port();
    let backup_hits = Arc::new(AtomicUsize::new(0));
    let backup_task = tokio::spawn(wait_for_hit(backup, backup_hits.clone(), COMPLETED));
    let primary_hits = Arc::new(AtomicUsize::new(0));
    let (seen_tx, seen_rx) = tokio::sync::oneshot::channel();
    let primary_task = tokio::spawn(observe_http_requests(
        primary,
        primary_hits.clone(),
        seen_tx,
    ));
    let (_dir, gateway) = fixture(vec![
        format!("http://127.0.0.1:{primary_port}/v1"),
        format!("http://127.0.0.1:{backup_port}/v1"),
    ])
    .await;
    let (usage, events) = attach_observers(&_dir, &gateway);
    let started = Instant::now();
    let response = request_bytes(&gateway, "/v1/responses", REQUEST_BODY).await;
    let elapsed = started.elapsed();
    let upstream_received_at = tokio::time::timeout(WAIT, seen_rx)
        .await
        .expect("primary upstream should receive the request")
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let status = response_status(&response);
    eprintln!(
        "HTTP delayed-header timeout after {elapsed:?}; upstream received request at +{:?}; primary_hits={}, backup_hits={}, status={status:?}",
        upstream_received_at.duration_since(started),
        primary_hits.load(Ordering::SeqCst),
        backup_hits.load(Ordering::SeqCst)
    );
    assert!(elapsed >= Duration::from_millis(800));
    assert!(elapsed < Duration::from_secs(3));
    assert_eq!(
        status,
        Some(504),
        "a dispatched request that times out before headers must return the timeout response"
    );
    assert!(String::from_utf8_lossy(&response).contains("FIRST_BYTE_TIMEOUT"));
    assert_eq!(primary_hits.load(Ordering::SeqCst), 1);
    assert_eq!(
        backup_hits.load(Ordering::SeqCst),
        0,
        "a request already received by the upstream must not be retried on backup after header timeout"
    );
    let usage_record = wait_for_usage_record(&usage).await;
    assert_timeout_usage(&usage_record, None, "response_headers");
    let diagnostic = wait_for_timeout_diagnostic(&events).await;
    assert_timeout_diagnostic(&diagnostic, None, Phase::Headers);
    primary_task.abort();
    backup_task.abort();
    gateway.stop().await.unwrap();
}

#[tokio::test]
async fn http_200_without_business_body_does_not_fail_over_on_first_byte_timeout() {
    let primary = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let primary_port = primary.local_addr().unwrap().port();
    let backup = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let backup_port = backup.local_addr().unwrap().port();
    let backup_hits = Arc::new(AtomicUsize::new(0));
    let backup_task = tokio::spawn(wait_for_hit(backup, backup_hits.clone(), COMPLETED));
    let primary_hits = Arc::new(AtomicUsize::new(0));
    let (seen_tx, seen_rx) = tokio::sync::oneshot::channel();
    let primary_task = tokio::spawn(serve_empty_200_then_count_retries(
        primary,
        primary_hits.clone(),
        seen_tx,
    ));
    let (_dir, gateway) = fixture(vec![
        format!("http://127.0.0.1:{primary_port}/v1"),
        format!("http://127.0.0.1:{backup_port}/v1"),
    ])
    .await;
    let (usage, events) = attach_observers(&_dir, &gateway);
    let started = Instant::now();
    let response = request_bytes(&gateway, "/v1/responses", REQUEST_BODY).await;
    let elapsed = started.elapsed();
    let headers_at = tokio::time::timeout(WAIT, seen_rx)
        .await
        .expect("primary upstream should return HTTP 200 headers")
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let status = response_status(&response);
    eprintln!(
        "HTTP 200 empty-body timeout after {elapsed:?}; upstream headers at +{:?}; primary_hits={}, backup_hits={}, status={status:?}",
        headers_at.duration_since(started),
        primary_hits.load(Ordering::SeqCst),
        backup_hits.load(Ordering::SeqCst)
    );
    assert!(elapsed >= Duration::from_millis(800));
    assert!(elapsed < Duration::from_secs(3));
    assert_eq!(
        status,
        Some(200),
        "once received, the upstream HTTP status remains the response status"
    );
    let response_text = String::from_utf8_lossy(&response);
    assert!(!response_text.contains("response.completed"));
    assert!(!response_text.contains("response.failed"));
    assert!(
        !response.ends_with(b"0\r\n\r\n"),
        "the stream must end as interrupted rather than as a clean empty response"
    );
    assert_eq!(
        backup_hits.load(Ordering::SeqCst),
        0,
        "HTTP headers alone are not business output and must not permit failover"
    );
    assert_eq!(primary_hits.load(Ordering::SeqCst), 1);
    let usage_record = wait_for_usage_record(&usage).await;
    assert_timeout_usage(&usage_record, Some(200), "first_byte");
    let diagnostic = wait_for_timeout_diagnostic(&events).await;
    assert_timeout_diagnostic(&diagnostic, Some(200), Phase::Stream);
    primary_task.abort();
    backup_task.abort();
    gateway.stop().await.unwrap();
}

async fn downstream_websocket(gateway: &Gateway) -> yawc::TcpWebSocket {
    let (port, token) = local_auth(gateway);
    yawc::WebSocket::connect(
        format!("ws://127.0.0.1:{port}/v1/responses")
            .parse()
            .unwrap(),
    )
    .with_options(yawc::Options::default().with_balanced_compression())
    .with_request(yawc::HttpRequest::builder().header("authorization", format!("Bearer {token}")))
    .await
    .unwrap()
}

async fn serve_websocket_attempts(
    listener: TcpListener,
    handshakes: Arc<AtomicUsize>,
    creates: Arc<AtomicUsize>,
    close_frames: Arc<AtomicUsize>,
    eof_count: Arc<AtomicUsize>,
    handshake_errors: Arc<AtomicUsize>,
    create_tx: tokio::sync::oneshot::Sender<Instant>,
) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(4);
    let mut create_tx = Some(create_tx);
    while let Ok(Ok((stream, _))) = tokio::time::timeout_at(deadline, listener.accept()).await {
        let mut websocket = match tokio_tungstenite::accept_async(stream).await {
            Ok(websocket) => {
                handshakes.fetch_add(1, Ordering::SeqCst);
                websocket
            }
            Err(_) => {
                handshake_errors.fetch_add(1, Ordering::SeqCst);
                continue;
            }
        };
        loop {
            let frame = match tokio::time::timeout_at(deadline, websocket.next()).await {
                Ok(Some(Ok(frame))) => frame,
                _ => break,
            };
            if matches!(&frame, tokio_tungstenite::tungstenite::Message::Text(_))
                && frame
                    .to_text()
                    .unwrap_or_default()
                    .contains("response.create")
            {
                creates.fetch_add(1, Ordering::SeqCst);
                if let Some(tx) = create_tx.take() {
                    let _ = tx.send(Instant::now());
                }
                websocket
                    .send(tokio_tungstenite::tungstenite::Message::Ping(
                        vec![7, 8, 9].into(),
                    ))
                    .await
                    .unwrap();
                break;
            }
        }
        loop {
            match tokio::time::timeout_at(deadline, websocket.next()).await {
                Ok(Some(Ok(tokio_tungstenite::tungstenite::Message::Close(_)))) => {
                    close_frames.fetch_add(1, Ordering::SeqCst);
                    break;
                }
                Ok(None) | Ok(Some(Err(_))) => {
                    eof_count.fetch_add(1, Ordering::SeqCst);
                    break;
                }
                Ok(Some(Ok(_))) => {}
                Err(_) => break,
            }
        }
    }
}

#[tokio::test]
async fn websocket_response_create_without_business_frame_does_not_fail_over() {
    let primary = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let primary_port = primary.local_addr().unwrap().port();
    let backup = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let backup_port = backup.local_addr().unwrap().port();
    let backup_hits = Arc::new(AtomicUsize::new(0));
    let backup_server_hits = backup_hits.clone();
    let (backup_seen_tx, mut backup_seen_rx) = tokio::sync::oneshot::channel();
    let backup_task = tokio::spawn(async move {
        if let Ok(Ok((stream, _))) =
            tokio::time::timeout(Duration::from_secs(4), backup.accept()).await
        {
            let _ = backup_seen_tx.send(());
            if let Ok(mut websocket) = tokio_tungstenite::accept_async(stream).await {
                backup_server_hits.fetch_add(1, Ordering::SeqCst);
                while let Some(message) = websocket.next().await {
                    if matches!(
                        message,
                        Ok(tokio_tungstenite::tungstenite::Message::Text(_))
                    ) {
                        let _ = websocket
                            .send(tokio_tungstenite::tungstenite::Message::Text(
                                String::from_utf8_lossy(COMPLETED).into_owned().into(),
                            ))
                            .await;
                        break;
                    }
                }
            }
        }
    });
    let primary_handshakes = Arc::new(AtomicUsize::new(0));
    let primary_creates = Arc::new(AtomicUsize::new(0));
    let primary_close_frames = Arc::new(AtomicUsize::new(0));
    let primary_eofs = Arc::new(AtomicUsize::new(0));
    let primary_handshake_errors = Arc::new(AtomicUsize::new(0));
    let (create_tx, create_rx) = tokio::sync::oneshot::channel();
    let primary_task = tokio::spawn(serve_websocket_attempts(
        primary,
        primary_handshakes.clone(),
        primary_creates.clone(),
        primary_close_frames.clone(),
        primary_eofs.clone(),
        primary_handshake_errors.clone(),
        create_tx,
    ));
    let (_dir, gateway) = fixture(vec![
        format!("http://127.0.0.1:{primary_port}/v1"),
        format!("http://127.0.0.1:{backup_port}/v1"),
    ])
    .await;
    let (usage, events) = attach_observers(&_dir, &gateway);
    let mut client = downstream_websocket(&gateway).await;
    client
        .send(yawc::Frame::text(
            r#"{"type":"response.create","model":"fixture-model","input":"fixture"}"#.to_owned(),
        ))
        .await
        .unwrap();
    let first_create_at = tokio::time::timeout(WAIT, create_rx)
        .await
        .expect("primary should receive response.create before the timeout")
        .unwrap();
    let control = tokio::time::timeout(WAIT, client.next())
        .await
        .expect("upstream control frame should be forwarded")
        .expect("gateway closed before forwarding the control frame");
    assert_eq!(control.opcode(), yawc::OpCode::Ping);
    assert_eq!(control.payload().as_ref(), &[7, 8, 9]);
    let deadline = tokio::time::Instant::from_std(first_create_at + Duration::from_secs(4));
    let mut close_elapsed = None;
    let mut unexpected_business_frame = false;
    loop {
        match tokio::time::timeout_at(deadline, client.next()).await {
            Ok(Some(frame)) if frame.opcode() == yawc::OpCode::Close => {
                close_elapsed = Some(Instant::now().duration_since(first_create_at));
                break;
            }
            Ok(Some(frame)) if frame.opcode().is_control() => {}
            Ok(Some(_)) => {
                unexpected_business_frame = true;
                break;
            }
            Ok(None) | Err(_) => break,
        }
    }
    tokio::time::sleep(Duration::from_millis(1_200)).await;
    let handshakes = primary_handshakes.load(Ordering::SeqCst);
    let creates = primary_creates.load(Ordering::SeqCst);
    let closes = primary_close_frames.load(Ordering::SeqCst);
    let eofs = primary_eofs.load(Ordering::SeqCst);
    let handshake_errors = primary_handshake_errors.load(Ordering::SeqCst);
    eprintln!(
        "WS first-byte timeout: close_elapsed={close_elapsed:?}, primary_handshakes={handshakes}, response.create={creates}, upstream_close_frames={closes}, upstream_eofs={eofs}, handshake_errors={handshake_errors}, backup_hits={}",
        backup_hits.load(Ordering::SeqCst)
    );
    let usage_record = wait_for_usage_record(&usage).await;
    assert_timeout_usage(&usage_record, Some(101), "ws_first_byte");
    let diagnostic = wait_for_timeout_diagnostic(&events).await;
    assert_timeout_diagnostic(&diagnostic, Some(101), Phase::WsReceive);
    assert!(
        close_elapsed.is_some_and(|elapsed| {
            elapsed >= Duration::from_millis(800) && elapsed < Duration::from_millis(1_500)
        }),
        "the WebSocket should close at the one-second first-byte deadline"
    );
    assert!(!unexpected_business_frame);
    assert_eq!(
        handshakes, 1,
        "a timed-out WS turn must not reconnect upstream"
    );
    assert_eq!(creates, 1, "response.create must not be replayed");
    assert_eq!(handshake_errors, 0);
    assert!(
        closes + eofs >= 1,
        "upstream connection should close after timeout"
    );
    assert_eq!(
        backup_hits.load(Ordering::SeqCst),
        0,
        "response.create already reached an upstream; first-byte timeout must not replay it"
    );
    assert!(
        backup_seen_rx.try_recv().is_err(),
        "backup WebSocket handshake must not start"
    );
    let attempt = usage_record.final_attempt().unwrap();
    assert_eq!(attempt.status, Some(101));
    assert!(!attempt.end.as_ref().unwrap().terminal_received);
    assert!(!attempt.transfer.as_ref().unwrap().terminal_handed_off);
    drop(client);
    primary_task.abort();
    backup_task.abort();
    gateway.stop().await.unwrap();
}

#[tokio::test]
async fn connection_failure_before_request_is_still_allowed_to_fail_over() {
    let reservation = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let unavailable_port = reservation.local_addr().unwrap().port();
    drop(reservation);
    let backup = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let backup_port = backup.local_addr().unwrap().port();
    let hits = Arc::new(AtomicUsize::new(0));
    let server_hits = hits.clone();
    let backup_task = tokio::spawn(async move {
        let (mut stream, _) = tokio::time::timeout(WAIT, backup.accept())
            .await
            .expect("fallback provider should receive a request")
            .unwrap();
        server_hits.fetch_add(1, Ordering::SeqCst);
        read_http_request(&mut stream).await.unwrap();
        respond(
            &mut stream,
            "200 OK",
            "application/json",
            br#"{"provider":"backup"}"#,
        )
        .await;
    });
    let (_dir, gateway) = fixture(vec![
        format!("http://127.0.0.1:{unavailable_port}/v1"),
        format!("http://127.0.0.1:{backup_port}/v1"),
    ])
    .await;
    let response = request_bytes(&gateway, "/v1/responses", REQUEST_BODY).await;
    assert_eq!(response_status(&response), Some(200));
    assert!(String::from_utf8_lossy(&response).contains("backup"));
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    backup_task.await.unwrap();
    gateway.stop().await.unwrap();
}
