//! Real loopback TCP regressions for downstream cancellation.
use super::{Edit, Gateway};
use crate::{
    events::{
        Action, Category, ClassifiedRecord, Filter as EventFilter, Reason as EventReason,
        Service as EventService,
    },
    usage::{
        model::{EndCause, Filter as UsageFilter, Record as UsageRecord},
        Service as UsageService,
    },
};
use futures_util::{SinkExt, StreamExt};
use http_body_util::BodyExt;
use std::{
    io,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
};

const REQUEST_BODY: &[u8] = br#"{"model":"fixture-model","input":"fixture","stream":true}"#;
const WAIT: Duration = Duration::from_secs(4);
const FAST_RELEASE: Duration = Duration::from_millis(300);
const USAGE_TAIL: Duration = Duration::from_millis(1500);
const USAGE_TAIL_TOLERANCE: Duration = Duration::from_millis(500);

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
    settings.idle_seconds = 3;
    settings.total_seconds = 6;
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
    .expect("canceled request usage record was not written")
}

fn interruption_records(service: &EventService) -> Vec<ClassifiedRecord> {
    service
        .query(EventFilter {
            reason: Some(EventReason::RequestInterrupted),
            ..EventFilter::default()
        })
        .items
}

async fn wait_for_interruption(service: &EventService) -> ClassifiedRecord {
    let mut events = service.subscribe();
    tokio::time::timeout(WAIT, async {
        loop {
            let rows = interruption_records(service);
            if let Some(record) = rows.into_iter().next() {
                assert_eq!(interruption_records(service).len(), 1);
                return record;
            }
            let _ = events.recv().await;
        }
    })
    .await
    .expect("downstream interruption diagnostic was not written")
}

async fn assert_neutral_health(gateway: &Gateway) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while gateway
            .view()
            .providers
            .iter()
            .any(|provider| provider.active_requests != 0)
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("canceled request kept provider activity open");
    let providers = gateway.view().providers;
    assert!(providers
        .iter()
        .all(|provider| { provider.health.failures == 0 && provider.health.retry_in == 0 }));
    assert!(!gateway.has_health_wait());
}

fn assert_not_successful_usage(record: &UsageRecord) {
    let attempt = record
        .final_attempt()
        .expect("request should have one attempt");
    assert_ne!(attempt.outcome, "success");
    assert_ne!(
        attempt.end.as_ref().map(|end| end.cause),
        Some(EndCause::Completed)
    );
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

async fn wait_for_disconnect(mut stream: TcpStream) -> Instant {
    let mut bytes = [0u8; 256];
    loop {
        match stream.read(&mut bytes).await {
            Ok(0) => return Instant::now(),
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return Instant::now(),
        }
    }
}

async fn downstream_http(gateway: &Gateway, body: &[u8]) -> TcpStream {
    let (port, token) = local_auth(gateway);
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    #[allow(deprecated)]
    stream.set_linger(Some(Duration::ZERO)).unwrap();
    let head = format!(
        "POST /v1/responses HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await.unwrap();
    stream.write_all(body).await.unwrap();
    stream
}

async fn assert_no_retry(primary: &TcpListener, backup: &TcpListener, wait: Duration) {
    let accepted = tokio::time::timeout(wait, async {
        tokio::select! {
            result = primary.accept() => Some(("primary", result.unwrap())),
            result = backup.accept() => Some(("backup", result.unwrap())),
        }
    })
    .await;
    assert!(
        accepted.is_err(),
        "downstream cancellation opened another upstream connection"
    );
}

#[tokio::test]
async fn rst_before_upstream_headers_closes_the_real_upstream_socket_promptly() {
    let primary = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let primary_port = primary.local_addr().unwrap().port();
    let backup = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let backup_port = backup.local_addr().unwrap().port();
    let (seen_tx, seen_rx) = oneshot::channel();
    let (closed_tx, closed_rx) = oneshot::channel();
    let primary_task = tokio::spawn(async move {
        let (mut upstream, _) = primary.accept().await.unwrap();
        read_http_request(&mut upstream).await.unwrap();
        let _ = seen_tx.send(());
        let closed = wait_for_disconnect(upstream).await;
        let _ = closed_tx.send(closed);
        primary
    });
    let (_dir, gateway) = fixture(vec![
        format!("http://127.0.0.1:{primary_port}/v1"),
        format!("http://127.0.0.1:{backup_port}/v1"),
    ])
    .await;
    let (usage, events) = attach_observers(&_dir, &gateway);
    let client = downstream_http(&gateway, REQUEST_BODY).await;
    tokio::time::timeout(WAIT, seen_rx)
        .await
        .expect("upstream should receive the request before cancellation")
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let cancelled_at = Instant::now();
    drop(client);
    let closed_at = tokio::time::timeout(FAST_RELEASE, closed_rx)
        .await
        .expect("upstream TCP connection stayed open after client RST")
        .unwrap();
    let close_elapsed = closed_at.duration_since(cancelled_at);
    eprintln!("HTTP pre-header RST closed upstream after {close_elapsed:?}");
    assert!(close_elapsed < FAST_RELEASE);
    let primary = primary_task.await.unwrap();
    assert_no_retry(&primary, &backup, Duration::from_secs(2)).await;
    let record = wait_for_usage_record(&usage).await;
    assert_not_successful_usage(&record);
    let attempt = record.final_attempt().unwrap();
    assert_eq!(
        attempt.end.as_ref().map(|end| end.cause),
        Some(EndCause::ClientDisconnected)
    );
    let transfer = attempt.transfer.as_ref();
    eprintln!(
        "HTTP pre-header RST usage: outcome={}, end={:?}, status={:?}, upstream_bytes={}, downstream_bytes={}, downstream_error={:?}",
        attempt.outcome,
        attempt.end.as_ref().map(|end| end.cause),
        attempt.status,
        transfer.map_or(0, |transfer| transfer.upstream_bytes),
        transfer.map_or(0, |transfer| transfer.downstream_bytes),
        transfer.and_then(|transfer| transfer.downstream_error.as_deref())
    );
    let interruptions = interruption_records(&events);
    eprintln!(
        "HTTP pre-header RST interruption diagnostic: {:?}",
        interruptions.first().map(|record| (
            record.reason,
            record.action,
            record.details.local_code.as_deref(),
            record.details.counted_failure,
            record.category
        ))
    );
    assert!(interruptions
        .iter()
        .all(|record| record.details.counted_failure == Some(false)));
    assert_neutral_health(&gateway).await;
    gateway.stop().await.unwrap();
}

#[tokio::test]
async fn rst_after_two_sse_events_closes_the_real_upstream_without_retry() {
    let primary = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let primary_port = primary.local_addr().unwrap().port();
    let backup = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let backup_port = backup.local_addr().unwrap().port();
    let (seen_tx, seen_rx) = oneshot::channel();
    let (closed_tx, closed_rx) = oneshot::channel();
    let event_one = b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"one\"}\n\n";
    let event_two = b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"two\"}\n\n";
    let primary_task = tokio::spawn(async move {
        let (mut upstream, _) = primary.accept().await.unwrap();
        read_http_request(&mut upstream).await.unwrap();
        upstream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: keep-alive\r\n\r\n",
            )
            .await
            .unwrap();
        for event in [event_one.as_slice(), event_two.as_slice()] {
            let chunk = format!("{:X}\r\n", event.len());
            upstream.write_all(chunk.as_bytes()).await.unwrap();
            upstream.write_all(event).await.unwrap();
            upstream.write_all(b"\r\n").await.unwrap();
            upstream.flush().await.unwrap();
        }
        let _ = seen_tx.send(());
        let closed = wait_for_disconnect(upstream).await;
        let _ = closed_tx.send(closed);
        primary
    });
    let (_dir, gateway) = fixture(vec![
        format!("http://127.0.0.1:{primary_port}/v1"),
        format!("http://127.0.0.1:{backup_port}/v1"),
    ])
    .await;
    let (usage, events) = attach_observers(&_dir, &gateway);
    let (mut response, sender, task) = super::tests::request_with_reset(
        &gateway,
        "/v1/responses",
        REQUEST_BODY.to_vec(),
        vec![("content-type", "application/json")],
    )
    .await;
    assert_eq!(response.status(), 200);
    let mut body = Vec::new();
    tokio::time::timeout(WAIT, async {
        while String::from_utf8_lossy(&body)
            .matches("response.output_text.delta")
            .count()
            < 2
        {
            let frame = response
                .body_mut()
                .frame()
                .await
                .expect("SSE stream ended before two events")
                .expect("SSE frame should be readable");
            if let Some(data) = frame.data_ref() {
                body.extend_from_slice(data);
            }
            if String::from_utf8_lossy(&body)
                .matches("response.output_text.delta")
                .count()
                >= 2
            {
                break;
            }
        }
    })
    .await
    .expect("gateway should forward two SSE events");
    let text = String::from_utf8_lossy(&body);
    assert_eq!(text.matches("response.output_text.delta").count(), 2);
    tokio::time::timeout(WAIT, seen_rx)
        .await
        .expect("upstream should have written the two events")
        .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let cancelled_at = Instant::now();
    super::tests::reset_and_drop(task, sender, response).await;
    let closed_at = tokio::time::timeout(USAGE_TAIL + USAGE_TAIL_TOLERANCE, closed_rx)
        .await
        .expect("upstream TCP connection stayed open after streaming RST tail window")
        .unwrap();
    let close_elapsed = closed_at.duration_since(cancelled_at);
    eprintln!("HTTP two-event RST closed upstream after {close_elapsed:?}");
    assert!(close_elapsed <= USAGE_TAIL + USAGE_TAIL_TOLERANCE);
    let primary = primary_task.await.unwrap();
    assert_no_retry(&primary, &backup, Duration::from_secs(2)).await;
    let usage_record = wait_for_usage_record(&usage).await;
    assert_not_successful_usage(&usage_record);
    let usage_attempt = usage_record.final_attempt().unwrap();
    assert_eq!(usage_attempt.status, Some(200));
    assert!(matches!(
        usage_attempt.end.as_ref().map(|end| end.cause),
        Some(
            EndCause::ClientDisconnected | EndCause::DownstreamTransportError | EndCause::Unsettled
        )
    ));
    assert!(!usage_attempt.end.as_ref().unwrap().terminal_received);
    let transfer = usage_attempt.transfer.as_ref().unwrap();
    assert!(transfer.upstream_bytes > 0);
    assert!(transfer.downstream_bytes > 0);
    assert!(!transfer.terminal_handed_off);
    eprintln!(
        "HTTP two-event RST usage: outcome={}, end={:?}, upstream_bytes={}, downstream_bytes={}, downstream_error={:?}",
        usage_attempt.outcome,
        usage_attempt.end.as_ref().map(|end| end.cause),
        transfer.upstream_bytes,
        transfer.downstream_bytes,
        transfer.downstream_error
    );
    let diagnostic = wait_for_interruption(&events).await;
    assert_eq!(diagnostic.reason, EventReason::RequestInterrupted);
    assert_eq!(diagnostic.category, Category::OrdinaryError);
    assert_eq!(diagnostic.action, Action::NotRetried);
    assert_eq!(diagnostic.details.counted_failure, Some(false));
    assert!(
        !diagnostic
            .details
            .delivery
            .as_ref()
            .unwrap()
            .terminal_received
    );
    assert_neutral_health(&gateway).await;
    gateway.stop().await.unwrap();
}

#[tokio::test]
async fn fin_half_close_keeps_the_response_stream_readable() {
    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_port = upstream.local_addr().unwrap().port();
    let payload = b"data: {\"type\":\"response.completed\",\"response\":{\"id\":\"fixture-finished\",\"status\":\"completed\"}}\n\n";
    let upstream_task = tokio::spawn(async move {
        let (mut socket, _) = upstream.accept().await.unwrap();
        read_http_request(&mut socket).await.unwrap();
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            payload.len()
        );
        socket.write_all(response.as_bytes()).await.unwrap();
        socket.write_all(payload).await.unwrap();
        socket.flush().await.unwrap();
    });
    let (_dir, gateway) = fixture(vec![format!("http://127.0.0.1:{upstream_port}/v1")]).await;
    let (port, token) = local_auth(&gateway);
    let mut client = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let head = format!(
        "POST /v1/responses HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        REQUEST_BODY.len()
    );
    client.write_all(head.as_bytes()).await.unwrap();
    client.write_all(REQUEST_BODY).await.unwrap();
    client.shutdown().await.unwrap();
    let mut response = Vec::new();
    tokio::time::timeout(WAIT, client.read_to_end(&mut response))
        .await
        .expect("FIN half-close should still allow the response to arrive")
        .unwrap();
    assert!(response.starts_with(b"HTTP/1.1 200"));
    assert!(String::from_utf8_lossy(&response).contains("response.completed"));
    upstream_task.await.unwrap();
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

const CREATE: &str = r#"{"type":"response.create","model":"fixture-model","input":"fixture"}"#;

async fn assert_websocket_cancel_closes_upstream(explicit_close: bool, generation: bool) {
    let primary = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let primary_port = primary.local_addr().unwrap().port();
    let backup = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let backup_port = backup.local_addr().unwrap().port();
    let (request_tx, request_rx) = oneshot::channel();
    let (create_tx, create_rx) = oneshot::channel();
    let (closed_tx, closed_rx) = oneshot::channel();
    let primary_task = tokio::spawn(async move {
        let (socket, _) = primary.accept().await.unwrap();
        if !generation {
            let mut socket = socket;
            read_http_request(&mut socket).await.unwrap();
            let _ = request_tx.send(());
            let closed = wait_for_disconnect(socket).await;
            let _ = closed_tx.send(closed);
        } else {
            let mut websocket = tokio_tungstenite::accept_async(socket).await.unwrap();
            let _ = request_tx.send(());
            let first = tokio::time::timeout(WAIT, websocket.next())
                .await
                .expect("upstream should receive response.create")
                .expect("upstream WebSocket closed before response.create")
                .expect("upstream WebSocket frame should be valid");
            assert!(first.to_text().unwrap().contains("response.create"));
            let _ = create_tx.send(());
            websocket
                .send(tokio_tungstenite::tungstenite::Message::Ping(
                    vec![1, 2, 3].into(),
                ))
                .await
                .unwrap();
            loop {
                match websocket.next().await {
                    Some(Ok(tokio_tungstenite::tungstenite::Message::Close(_)))
                    | None
                    | Some(Err(_)) => {
                        let _ = closed_tx.send(Instant::now());
                        break;
                    }
                    Some(Ok(_)) => {}
                }
            }
        }
        primary
    });
    let (_dir, gateway) = fixture(vec![
        format!("http://127.0.0.1:{primary_port}/v1"),
        format!("http://127.0.0.1:{backup_port}/v1"),
    ])
    .await;
    let (usage, events) = attach_observers(&_dir, &gateway);
    let mut client = downstream_websocket(&gateway).await;
    client
        .send(yawc::Frame::text(CREATE.to_owned()))
        .await
        .unwrap();
    tokio::time::timeout(WAIT, request_rx)
        .await
        .expect("primary upstream should receive a real WebSocket handshake")
        .unwrap();
    if generation {
        tokio::time::timeout(WAIT, create_rx)
            .await
            .expect("primary upstream should receive response.create")
            .unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let cancelled_at = Instant::now();
    if explicit_close {
        client
            .send(yawc::Frame::close(1000.into(), "fixture cancel"))
            .await
            .unwrap();
    }
    drop(client);
    let close_window = if generation {
        USAGE_TAIL + USAGE_TAIL_TOLERANCE
    } else {
        FAST_RELEASE
    };
    let closed_at = tokio::time::timeout(close_window, closed_rx)
        .await
        .expect("upstream WebSocket stayed open after downstream Close/EOF")
        .unwrap();
    let close_elapsed = closed_at.duration_since(cancelled_at);
    eprintln!("WebSocket cancellation closed upstream after {close_elapsed:?}");
    assert!(close_elapsed <= close_window);
    let primary = primary_task.await.unwrap();
    assert_no_retry(&primary, &backup, Duration::from_secs(2)).await;
    if generation {
        let usage_record = wait_for_usage_record(&usage).await;
        assert_not_successful_usage(&usage_record);
        let attempt = usage_record.final_attempt().unwrap();
        assert_eq!(attempt.status, Some(101));
        assert!(!attempt.end.as_ref().unwrap().terminal_received);
        let diagnostic = wait_for_interruption(&events).await;
        assert_eq!(diagnostic.reason, EventReason::RequestInterrupted);
        assert_eq!(diagnostic.category, Category::OrdinaryError);
        assert_eq!(diagnostic.details.counted_failure, Some(false));
        assert!(
            !diagnostic
                .details
                .delivery
                .as_ref()
                .unwrap()
                .terminal_received
        );
        assert_neutral_health(&gateway).await;
    }
    gateway.stop().await.unwrap();
}

#[tokio::test]
async fn websocket_close_while_upstream_handshake_is_pending_cancels_socket() {
    assert_websocket_cancel_closes_upstream(true, false).await;
}

#[tokio::test]
async fn websocket_eof_while_upstream_handshake_is_pending_cancels_socket() {
    assert_websocket_cancel_closes_upstream(false, false).await;
}

#[tokio::test]
async fn websocket_close_during_generation_closes_upstream_socket() {
    assert_websocket_cancel_closes_upstream(true, true).await;
}

#[tokio::test]
async fn websocket_eof_during_generation_closes_upstream_socket() {
    assert_websocket_cancel_closes_upstream(false, true).await;
}

#[path = "usage_tail_tests.rs"]
mod usage_tail_tests;
