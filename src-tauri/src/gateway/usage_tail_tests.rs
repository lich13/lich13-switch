//! Loopback regressions for bounded post-cancellation usage observation.
use super::*;
use crate::usage::model::{safe_id, EndCause, Record as UsageRecord, Tokens};
use futures_util::{SinkExt, StreamExt};
use http_body_util::BodyExt;
use hyper::{body::Incoming, Response};
use std::time::{Duration, Instant};
use tokio::{
    io::AsyncWriteExt,
    net::{TcpListener, TcpStream},
    sync::oneshot,
};

pub(super) const TAIL_RESPONSE_ID: &str = "fixture-tail-response";
pub(super) const TAIL_MODEL: &str = "fixture-tail-model";
pub(super) const TAIL_INCLUSIVE_INPUT: u64 = 42;
pub(super) const TAIL_INPUT: u64 = 34;
pub(super) const TAIL_OUTPUT: u64 = 17;
pub(super) const TAIL_CACHE_READ: u64 = 5;
pub(super) const TAIL_CACHE_WRITE: u64 = 3;
pub(super) const TAIL_AUDIO_OUTPUT: u64 = 0;
pub(super) const CANCEL_DELAY: Duration = Duration::from_millis(200);
pub(super) const TAIL_WINDOW: Duration = Duration::from_millis(1500);
pub(super) const TAIL_TOLERANCE: Duration = Duration::from_millis(500);

pub(super) const HTTP_FIRST: &[u8] = br#"data: {"type":"response.created","response":{"id":"fixture-tail-response","model":"fixture-tail-model"}}

"#;
const HTTP_PROGRESS: &[u8] = br#"data: {"type":"response.output_text.delta","response_id":"fixture-tail-response","delta":"fixture-progress"}

"#;
const HTTP_WRONG_ERROR: &[u8] = br#"data: {"type":"error","response":{"id":"fixture-other-response","usage":{"input_tokens":999,"output_tokens":999}},"error":{"code":"fixture_error","message":"fixture"}}

"#;
const HTTP_COMPLETED: &[u8] = br#"data: {"type":"response.completed","response":{"id":"fixture-tail-response","model":"fixture-tail-model","status":"completed","usage":{"input_tokens":42,"output_tokens":17,"cache_read_input_tokens":5,"cache_creation_input_tokens":3,"output_tokens_details":{"audio_tokens":0}}}}

"#;
const WS_WRONG_ERROR: &str = r#"{"type":"error","response":{"id":"fixture-other-response","usage":{"input_tokens":999,"output_tokens":999}},"error":{"code":"fixture_error","message":"fixture"}}"#;

fn expected_tail_tokens() -> Tokens {
    Tokens {
        inclusive_input: Some(TAIL_INCLUSIVE_INPUT),
        input: Some(TAIL_INPUT),
        output: Some(TAIL_OUTPUT),
        cache_read: Some(TAIL_CACHE_READ),
        cache_write: Some(TAIL_CACHE_WRITE),
        audio_output: Some(TAIL_AUDIO_OUTPUT),
        ..Tokens::default()
    }
}

async fn send_http_chunk(stream: &mut TcpStream, payload: &[u8]) -> std::io::Result<()> {
    stream
        .write_all(format!("{:X}\r\n", payload.len()).as_bytes())
        .await?;
    stream.write_all(payload).await?;
    stream.write_all(b"\r\n").await?;
    stream.flush().await
}

async fn first_http_data(response: &mut Response<Incoming>) -> Vec<u8> {
    let frame = tokio::time::timeout(WAIT, response.body_mut().frame())
        .await
        .expect("HTTP response did not produce its first frame")
        .expect("HTTP response ended before its first frame")
        .expect("HTTP response frame should be readable");
    frame
        .data_ref()
        .expect("HTTP response first frame should contain data")
        .to_vec()
}

async fn wait_for_tail(gateway: &Gateway, usage: &UsageService, websocket: bool) {
    tokio::time::timeout(Duration::from_millis(300), async {
        loop {
            let view = gateway.view();
            let tail_started = if websocket {
                view.providers
                    .iter()
                    .all(|provider| provider.active_requests == 0)
            } else {
                view.active_connections == 0
            };
            if tail_started {
                assert!(
                    usage_records(usage).is_empty(),
                    "usage must remain pending while cancellation tail is observed"
                );
                return;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("cancellation tail did not start before the bounded deadline");
}

pub(super) fn assert_tail_usage(record: &UsageRecord) {
    assert_tail_usage_with_handoff(record, false);
}

pub(super) fn assert_tail_usage_with_handoff(record: &UsageRecord, terminal_handed_off: bool) {
    assert_eq!(
        record.attempts.len(),
        1,
        "cancellation must persist one record"
    );
    let attempt = record
        .final_attempt()
        .expect("tail record should have one attempt");
    assert_eq!(
        attempt.response_id.as_deref(),
        Some(safe_id(TAIL_RESPONSE_ID).as_str())
    );
    assert_eq!(attempt.response_model.as_deref(), Some(TAIL_MODEL));
    let expected = expected_tail_tokens();
    assert_eq!(attempt.tokens.inclusive_input, expected.inclusive_input);
    assert_eq!(attempt.tokens.input, expected.input);
    assert_eq!(attempt.tokens.output, expected.output);
    assert_eq!(attempt.tokens.cache_read, expected.cache_read);
    assert_eq!(attempt.tokens.cache_write, expected.cache_write);
    assert_eq!(attempt.tokens.cache_write_5m, expected.cache_write_5m);
    assert_eq!(attempt.tokens.cache_write_1h, expected.cache_write_1h);
    assert_eq!(attempt.tokens.image_input, expected.image_input);
    assert_eq!(attempt.tokens.image_output, expected.image_output);
    assert_eq!(attempt.tokens.audio_input, expected.audio_input);
    assert_eq!(attempt.tokens.audio_output, expected.audio_output);
    assert_eq!(
        attempt.end.as_ref().map(|end| end.cause),
        Some(EndCause::Completed),
        "observed upstream terminal must remain a completed usage outcome"
    );
    let end = attempt
        .end
        .as_ref()
        .expect("tail record should have end metadata");
    assert!(end.terminal_received);
    let transfer = attempt
        .transfer
        .as_ref()
        .expect("tail record should have transfer evidence");
    assert!(
        transfer.downstream_error.is_some(),
        "downstream cancellation evidence must be retained beside terminal usage"
    );
    assert!(transfer.upstream_bytes > 0);
    assert!(transfer.downstream_bytes > 0);
    assert_eq!(transfer.terminal_handed_off, terminal_handed_off);
}

pub(super) async fn assert_tail_interruption(
    events: &EventService,
    receiver: &mut tokio::sync::broadcast::Receiver<crate::events::Change>,
    events_dir: &std::path::Path,
    usage: &UsageRecord,
) {
    let attempt = usage.final_attempt().expect("tail usage attempt");
    let transfer = attempt.transfer.as_ref().expect("tail delivery evidence");
    assert!(transfer.downstream_error.is_some());
    let diagnostic = super::assert_downstream_diagnostic_policy(
        events,
        receiver,
        events_dir,
        transfer.downstream_error.as_deref(),
    )
    .await;
    if let Some(record) = diagnostic {
        assert_eq!(record.reason, EventReason::RequestInterrupted);
        assert_eq!(record.action, Action::NotRetried);
        assert_eq!(record.details.counted_failure, Some(false));
        let delivery = record
            .details
            .delivery
            .as_ref()
            .expect("warning should preserve delivery evidence");
        assert!(delivery.terminal_received);
        assert!(!delivery.terminal_handed_off);
    }
}

async fn assert_interruption_without_terminal(
    events: &EventService,
    receiver: &mut tokio::sync::broadcast::Receiver<crate::events::Change>,
    events_dir: &std::path::Path,
    usage: &UsageRecord,
) {
    let attempt = usage.final_attempt().expect("canceled usage attempt");
    let end = attempt.end.as_ref().expect("canceled usage end metadata");
    assert!(!end.terminal_received);
    let transfer = attempt
        .transfer
        .as_ref()
        .expect("canceled usage transfer evidence");
    assert!(transfer.downstream_error.is_some());
    assert!(transfer.downstream_bytes > 0);
    assert!(!transfer.terminal_handed_off);
    let diagnostic = super::assert_downstream_diagnostic_policy(
        events,
        receiver,
        events_dir,
        transfer.downstream_error.as_deref(),
    )
    .await;
    if let Some(record) = diagnostic {
        assert_eq!(record.reason, EventReason::RequestInterrupted);
        assert_eq!(record.action, Action::NotRetried);
        assert_eq!(record.details.counted_failure, Some(false));
        let delivery = record
            .details
            .delivery
            .as_ref()
            .expect("warning should preserve delivery evidence");
        assert!(!delivery.terminal_received);
        assert!(!delivery.terminal_handed_off);
    }
}

async fn assert_gateway_stop_usage(
    events: &EventService,
    receiver: &mut tokio::sync::broadcast::Receiver<crate::events::Change>,
    events_dir: &std::path::Path,
    usage: &UsageRecord,
) {
    let attempt = usage.final_attempt().expect("gateway-stop usage attempt");
    let end = attempt.end.as_ref().expect("gateway-stop end metadata");
    assert_eq!(end.cause, EndCause::ClientDisconnected);
    assert!(!end.terminal_received);
    let transfer = attempt
        .transfer
        .as_ref()
        .expect("gateway-stop transfer evidence");
    assert!(transfer.downstream_error.is_some());
    // A settled WebSocket tail can omit its optional connection snapshot. The
    // caller verifies the real upstream socket closure within FAST_RELEASE.
    assert!(!transfer.terminal_handed_off);
    let diagnostic = super::assert_downstream_diagnostic_policy(
        events,
        receiver,
        events_dir,
        transfer.downstream_error.as_deref(),
    )
    .await;
    if let Some(record) = diagnostic {
        assert_eq!(record.reason, EventReason::RequestInterrupted);
        assert_eq!(record.details.counted_failure, Some(false));
        assert!(
            !record
                .details
                .delivery
                .as_ref()
                .expect("warning should preserve delivery evidence")
                .terminal_received
        );
    }
}

#[tokio::test]
async fn http_cancel_tail_keeps_matching_terminal_usage_until_the_fixed_deadline() {
    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_port = upstream.local_addr().unwrap().port();
    let (ready_tx, ready_rx) = oneshot::channel();
    let (cancel_tx, cancel_rx) = oneshot::channel();
    let (tail_tx, tail_rx) = oneshot::channel();
    let (closed_tx, closed_rx) = oneshot::channel();
    let upstream_task = tokio::spawn(async move {
        let (mut socket, _) = upstream.accept().await.unwrap();
        read_http_request(&mut socket).await.unwrap();
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: keep-alive\r\n\r\n",
            )
            .await
            .unwrap();
        send_http_chunk(&mut socket, HTTP_FIRST).await.unwrap();
        send_http_chunk(&mut socket, HTTP_PROGRESS).await.unwrap();
        let _ = ready_tx.send(());
        cancel_rx.await.unwrap();
        tokio::time::sleep(CANCEL_DELAY).await;
        send_http_chunk(&mut socket, HTTP_WRONG_ERROR)
            .await
            .unwrap();
        send_http_chunk(&mut socket, HTTP_COMPLETED).await.unwrap();
        let _ = tail_tx.send(());
        let closed = wait_for_disconnect(socket).await;
        let _ = closed_tx.send(closed);
    });

    let (dir, gateway) = fixture(vec![format!("http://127.0.0.1:{upstream_port}/v1")]).await;
    let (usage, events) = attach_observers(&dir, &gateway);
    let mut diagnostic_events = events.subscribe();
    let (mut response, sender, task) = super::super::tests::request_with_reset(
        &gateway,
        "/v1/responses",
        REQUEST_BODY.to_vec(),
        vec![("content-type", "application/json")],
    )
    .await;
    tokio::time::timeout(WAIT, ready_rx)
        .await
        .expect("upstream did not send its first response packet")
        .unwrap();
    let first = first_http_data(&mut response).await;
    assert!(String::from_utf8_lossy(&first).contains(TAIL_RESPONSE_ID));
    let cancelled_at = Instant::now();
    super::super::tests::reset_and_drop(task, sender, response).await;
    cancel_tx.send(()).unwrap();

    tokio::time::timeout(TAIL_WINDOW, tail_rx)
        .await
        .expect("upstream tail was not read before the bounded deadline")
        .unwrap();
    let closed_at = tokio::time::timeout(TAIL_WINDOW + TAIL_TOLERANCE, closed_rx)
        .await
        .expect("HTTP upstream stayed open after matching terminal usage")
        .unwrap();
    let elapsed = closed_at.duration_since(cancelled_at);
    assert!(
        elapsed >= TAIL_WINDOW.saturating_sub(Duration::from_millis(150)),
        "matching terminal usage must not release upstream early: {elapsed:?}"
    );
    assert!(
        elapsed <= TAIL_WINDOW + TAIL_TOLERANCE,
        "matching terminal usage exceeded the fixed tail window: {elapsed:?}"
    );

    upstream_task.await.unwrap();
    let record = wait_for_usage_record(&usage).await;
    assert_tail_usage(&record);
    assert_neutral_health(&gateway).await;
    assert_tail_interruption(
        &events,
        &mut diagnostic_events,
        &dir.path().join("events"),
        &record,
    )
    .await;
    gateway.stop().await.unwrap();
}

#[tokio::test]
async fn websocket_cancel_tail_keeps_matching_terminal_usage_until_the_fixed_deadline() {
    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_port = upstream.local_addr().unwrap().port();
    let (ready_tx, ready_rx) = oneshot::channel();
    let (cancel_tx, cancel_rx) = oneshot::channel();
    let (tail_tx, tail_rx) = oneshot::channel();
    let (closed_tx, closed_rx) = oneshot::channel();
    let upstream_task = tokio::spawn(async move {
        let (socket, _) = upstream.accept().await.unwrap();
        let mut websocket = tokio_tungstenite::accept_async(socket).await.unwrap();
        let first = tokio::time::timeout(WAIT, websocket.next())
            .await
            .expect("upstream did not receive response.create")
            .expect("upstream WebSocket ended before response.create")
            .expect("upstream WebSocket frame should be valid");
        assert!(first.to_text().unwrap().contains("response.create"));
        websocket
            .send(tokio_tungstenite::tungstenite::Message::Text(
                format!(
                    "{{\"type\":\"response.created\",\"response\":{{\"id\":\"{TAIL_RESPONSE_ID}\",\"model\":\"{TAIL_MODEL}\"}}}}"
                )
                .into(),
            ))
            .await
            .unwrap();
        websocket
            .send(tokio_tungstenite::tungstenite::Message::Text(
                format!(
                    "{{\"type\":\"response.output_text.delta\",\"response_id\":\"{TAIL_RESPONSE_ID}\",\"delta\":\"fixture-progress\"}}"
                )
                .into(),
            ))
            .await
            .unwrap();
        let _ = ready_tx.send(());
        cancel_rx.await.unwrap();
        tokio::time::sleep(CANCEL_DELAY).await;
        websocket
            .send(tokio_tungstenite::tungstenite::Message::Text(
                WS_WRONG_ERROR.to_owned().into(),
            ))
            .await
            .unwrap();
        websocket
            .send(tokio_tungstenite::tungstenite::Message::Text(
                format!(
                    "{{\"type\":\"response.completed\",\"response\":{{\"id\":\"{TAIL_RESPONSE_ID}\",\"model\":\"{TAIL_MODEL}\",\"status\":\"completed\",\"usage\":{{\"input_tokens\":{TAIL_INCLUSIVE_INPUT},\"output_tokens\":{TAIL_OUTPUT},\"cache_read_input_tokens\":{TAIL_CACHE_READ},\"cache_creation_input_tokens\":{TAIL_CACHE_WRITE},\"output_tokens_details\":{{\"audio_tokens\":{TAIL_AUDIO_OUTPUT}}}}}}}}}"
                )
                .into(),
            ))
            .await
            .unwrap();
        let _ = tail_tx.send(());
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
    });

    let (dir, gateway) = fixture(vec![format!("http://127.0.0.1:{upstream_port}/v1")]).await;
    let (usage, events) = attach_observers(&dir, &gateway);
    let mut diagnostic_events = events.subscribe();
    let mut client = downstream_websocket(&gateway).await;
    client
        .send(yawc::Frame::text(CREATE.to_owned()))
        .await
        .unwrap();
    tokio::time::timeout(WAIT, ready_rx)
        .await
        .expect("upstream did not send its first response packet")
        .unwrap();
    let first = tokio::time::timeout(WAIT, client.next())
        .await
        .expect("downstream WebSocket did not receive response.created")
        .expect("downstream WebSocket ended before response.created");
    assert!(first.as_str().contains(TAIL_RESPONSE_ID));
    let cancelled_at = Instant::now();
    client
        .send(yawc::Frame::close(1000.into(), "fixture cancel"))
        .await
        .unwrap();
    drop(client);
    cancel_tx.send(()).unwrap();

    tokio::time::timeout(TAIL_WINDOW, tail_rx)
        .await
        .expect("upstream WebSocket tail was not read before the bounded deadline")
        .unwrap();
    let closed_at = tokio::time::timeout(TAIL_WINDOW + TAIL_TOLERANCE, closed_rx)
        .await
        .expect("WebSocket upstream stayed open after matching terminal usage")
        .unwrap();
    let elapsed = closed_at.duration_since(cancelled_at);
    assert!(
        elapsed >= TAIL_WINDOW.saturating_sub(Duration::from_millis(150)),
        "matching terminal usage must not release upstream early: {elapsed:?}"
    );
    assert!(
        elapsed <= TAIL_WINDOW + TAIL_TOLERANCE,
        "matching terminal usage exceeded the fixed tail window: {elapsed:?}"
    );

    upstream_task.await.unwrap();
    let record = wait_for_usage_record(&usage).await;
    assert_tail_usage(&record);
    assert_neutral_health(&gateway).await;
    assert_tail_interruption(
        &events,
        &mut diagnostic_events,
        &dir.path().join("events"),
        &record,
    )
    .await;
    gateway.stop().await.unwrap();
}

#[tokio::test]
async fn http_cancel_tail_with_continuous_heartbeat_still_closes_within_two_seconds() {
    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_port = upstream.local_addr().unwrap().port();
    let (ready_tx, ready_rx) = oneshot::channel();
    let (cancel_tx, cancel_rx) = oneshot::channel();
    let (closed_tx, closed_rx) = oneshot::channel();
    let upstream_task = tokio::spawn(async move {
        let (mut socket, _) = upstream.accept().await.unwrap();
        read_http_request(&mut socket).await.unwrap();
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: keep-alive\r\n\r\n",
            )
            .await
            .unwrap();
        send_http_chunk(&mut socket, HTTP_FIRST).await.unwrap();
        let _ = ready_tx.send(());
        cancel_rx.await.unwrap();
        let deadline = tokio::time::Instant::now() + TAIL_WINDOW + TAIL_TOLERANCE;
        let mut buffer = [0u8; 1];
        loop {
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => break,
                // TCP writes may remain buffered after the peer closes. Observe EOF
                // while sending heartbeats instead of waiting for a later write error.
                read = socket.read(&mut buffer) => {
                    if !matches!(read, Ok(n) if n > 0) {
                        let _ = closed_tx.send(Instant::now());
                        break;
                    }
                }
                _ = tokio::time::sleep(Duration::from_millis(100)) => {
                    if send_http_chunk(&mut socket, b": fixture-heartbeat\n\n").await.is_err()
                        || send_http_chunk(&mut socket, HTTP_PROGRESS).await.is_err()
                    {
                        let _ = closed_tx.send(Instant::now());
                        break;
                    }
                }
            }
        }
    });

    let (dir, gateway) = fixture(vec![format!("http://127.0.0.1:{upstream_port}/v1")]).await;
    let (usage, events) = attach_observers(&dir, &gateway);
    let mut diagnostic_events = events.subscribe();
    let (mut response, sender, task) = super::super::tests::request_with_reset(
        &gateway,
        "/v1/responses",
        REQUEST_BODY.to_vec(),
        vec![("content-type", "application/json")],
    )
    .await;
    tokio::time::timeout(WAIT, ready_rx)
        .await
        .expect("upstream did not send its first response packet")
        .unwrap();
    let first = first_http_data(&mut response).await;
    assert!(String::from_utf8_lossy(&first).contains(TAIL_RESPONSE_ID));
    let cancelled_at = Instant::now();
    super::super::tests::reset_and_drop(task, sender, response).await;
    cancel_tx.send(()).unwrap();
    let closed_at = tokio::time::timeout(TAIL_WINDOW + TAIL_TOLERANCE, closed_rx)
        .await
        .expect("continuous upstream heartbeat kept the cancellation tail open")
        .unwrap();
    let elapsed = closed_at.duration_since(cancelled_at);
    assert!(
        elapsed <= TAIL_WINDOW + TAIL_TOLERANCE,
        "never-complete stream must close within two seconds: {elapsed:?}"
    );

    upstream_task.await.unwrap();
    let record = wait_for_usage_record(&usage).await;
    let attempt = record.final_attempt().unwrap();
    assert_ne!(attempt.outcome, "success");
    assert_ne!(
        attempt.end.as_ref().map(|end| end.cause),
        Some(EndCause::Completed)
    );
    assert_neutral_health(&gateway).await;
    assert_interruption_without_terminal(
        &events,
        &mut diagnostic_events,
        &dir.path().join("events"),
        &record,
    )
    .await;
    gateway.stop().await.unwrap();
}

#[tokio::test]
async fn websocket_cancel_tail_with_continuous_content_and_heartbeat_closes_within_two_seconds() {
    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_port = upstream.local_addr().unwrap().port();
    let (ready_tx, ready_rx) = oneshot::channel();
    let (cancel_tx, cancel_rx) = oneshot::channel();
    let (closed_tx, closed_rx) = oneshot::channel();
    let upstream_task = tokio::spawn(async move {
        let (socket, _) = upstream.accept().await.unwrap();
        let mut websocket = tokio_tungstenite::accept_async(socket).await.unwrap();
        let first = tokio::time::timeout(WAIT, websocket.next())
            .await
            .expect("upstream did not receive response.create")
            .expect("upstream WebSocket ended before response.create")
            .expect("upstream WebSocket frame should be valid");
        assert!(first.to_text().unwrap().contains("response.create"));
        websocket
            .send(tokio_tungstenite::tungstenite::Message::Text(
                format!(
                    "{{\"type\":\"response.created\",\"response\":{{\"id\":\"{TAIL_RESPONSE_ID}\",\"model\":\"{TAIL_MODEL}\"}}}}"
                )
                .into(),
            ))
            .await
            .unwrap();
        let _ = ready_tx.send(());
        cancel_rx.await.unwrap();
        let mut heartbeat = tokio::time::interval(Duration::from_millis(100));
        loop {
            // Observe EOF/Close while sending progress. Successful writes alone
            // do not prove the peer is still connected, especially on Windows.
            tokio::select! {
                biased;
                frame = websocket.next() => match frame {
                    Some(Ok(tokio_tungstenite::tungstenite::Message::Close(_)))
                    | None
                    | Some(Err(_)) => {
                        let _ = closed_tx.send(Instant::now());
                        break;
                    }
                    Some(Ok(_)) => continue,
                },
                _ = heartbeat.tick() => {}
            }
            if websocket
                .send(tokio_tungstenite::tungstenite::Message::Ping(
                    vec![1, 2, 3].into(),
                ))
                .await
                .is_err()
            {
                let _ = closed_tx.send(Instant::now());
                break;
            }
            let content = format!(
                "{{\"type\":\"response.output_text.delta\",\"delta\":\"fixture-progress\",\"response_id\":\"{TAIL_RESPONSE_ID}\"}}"
            );
            if websocket
                .send(tokio_tungstenite::tungstenite::Message::Text(
                    content.into(),
                ))
                .await
                .is_err()
            {
                let _ = closed_tx.send(Instant::now());
                break;
            }
        }
    });

    let (dir, gateway) = fixture(vec![format!("http://127.0.0.1:{upstream_port}/v1")]).await;
    let (usage, events) = attach_observers(&dir, &gateway);
    let mut diagnostic_events = events.subscribe();
    let mut client = downstream_websocket(&gateway).await;
    client
        .send(yawc::Frame::text(CREATE.to_owned()))
        .await
        .unwrap();
    tokio::time::timeout(WAIT, ready_rx)
        .await
        .expect("upstream did not send its first response packet")
        .unwrap();
    let first = tokio::time::timeout(WAIT, client.next())
        .await
        .expect("downstream WebSocket did not receive response.created")
        .expect("downstream WebSocket ended before response.created");
    assert!(first.as_str().contains(TAIL_RESPONSE_ID));
    let cancelled_at = Instant::now();
    client
        .send(yawc::Frame::close(1000.into(), "fixture cancel"))
        .await
        .unwrap();
    drop(client);
    cancel_tx.send(()).unwrap();

    let closed_at = tokio::time::timeout(TAIL_WINDOW + TAIL_TOLERANCE, closed_rx)
        .await
        .expect("continuous upstream WebSocket content kept the cancellation tail open")
        .unwrap();
    let elapsed = closed_at.duration_since(cancelled_at);
    assert!(
        elapsed <= TAIL_WINDOW + TAIL_TOLERANCE,
        "never-complete WebSocket stream must close within two seconds: {elapsed:?}"
    );

    upstream_task.await.unwrap();
    let record = wait_for_usage_record(&usage).await;
    let attempt = record.final_attempt().unwrap();
    assert_ne!(attempt.outcome, "success");
    assert_ne!(
        attempt.end.as_ref().map(|end| end.cause),
        Some(EndCause::Completed)
    );
    assert_neutral_health(&gateway).await;
    assert_interruption_without_terminal(
        &events,
        &mut diagnostic_events,
        &dir.path().join("events"),
        &record,
    )
    .await;
    gateway.stop().await.unwrap();
}

#[tokio::test]
async fn http_cancel_tail_upstream_read_error_keeps_disconnected_evidence() {
    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_port = upstream.local_addr().unwrap().port();
    let (ready_tx, ready_rx) = oneshot::channel();
    let (cancel_tx, cancel_rx) = oneshot::channel();
    let (error_tx, error_rx) = oneshot::channel();
    let upstream_task = tokio::spawn(async move {
        let (mut socket, _) = upstream.accept().await.unwrap();
        read_http_request(&mut socket).await.unwrap();
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: keep-alive\r\n\r\n",
            )
            .await
            .unwrap();
        send_http_chunk(&mut socket, HTTP_FIRST).await.unwrap();
        let _ = ready_tx.send(());
        cancel_rx.await.unwrap();
        #[allow(deprecated)]
        socket.set_linger(Some(Duration::ZERO)).unwrap();
        let _ = error_tx.send(());
        drop(socket);
    });

    let (dir, gateway) = fixture(vec![format!("http://127.0.0.1:{upstream_port}/v1")]).await;
    let (usage, events) = attach_observers(&dir, &gateway);
    let mut diagnostic_events = events.subscribe();
    let (mut response, sender, task) = super::super::tests::request_with_reset(
        &gateway,
        "/v1/responses",
        REQUEST_BODY.to_vec(),
        vec![("content-type", "application/json")],
    )
    .await;
    tokio::time::timeout(WAIT, ready_rx)
        .await
        .expect("upstream did not send its first response packet")
        .unwrap();
    let first = first_http_data(&mut response).await;
    assert!(String::from_utf8_lossy(&first).contains(TAIL_RESPONSE_ID));
    assert!(gateway.view().active_connections > 0);
    super::super::tests::reset_and_drop(task, sender, response).await;
    wait_for_tail(&gateway, &usage, false).await;
    cancel_tx.send(()).unwrap();
    tokio::time::timeout(FAST_RELEASE, error_rx)
        .await
        .expect("upstream RST fixture did not run")
        .unwrap();
    upstream_task.await.unwrap();

    let record = wait_for_usage_record(&usage).await;
    let attempt = record.final_attempt().unwrap();
    assert_ne!(attempt.outcome, "success");
    assert_ne!(
        attempt.end.as_ref().map(|end| end.cause),
        Some(EndCause::Completed)
    );
    let transfer = attempt.transfer.as_ref().unwrap();
    assert_eq!(transfer.upstream_end.as_deref(), Some("DISCONNECTED"));
    assert!(!transfer.upstream_eof);
    assert_neutral_health(&gateway).await;
    assert_interruption_without_terminal(
        &events,
        &mut diagnostic_events,
        &dir.path().join("events"),
        &record,
    )
    .await;
    gateway.stop().await.unwrap();
}

#[tokio::test]
async fn websocket_cancel_tail_upstream_read_error_keeps_disconnected_evidence() {
    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_port = upstream.local_addr().unwrap().port();
    let (ready_tx, ready_rx) = oneshot::channel();
    let (cancel_tx, cancel_rx) = oneshot::channel();
    let (error_tx, error_rx) = oneshot::channel();
    let upstream_task = tokio::spawn(async move {
        let (socket, _) = upstream.accept().await.unwrap();
        let mut websocket = tokio_tungstenite::accept_async(socket).await.unwrap();
        let first = tokio::time::timeout(WAIT, websocket.next())
            .await
            .expect("upstream did not receive response.create")
            .expect("upstream WebSocket ended before response.create")
            .expect("upstream WebSocket frame should be valid");
        assert!(first.to_text().unwrap().contains("response.create"));
        websocket
            .send(tokio_tungstenite::tungstenite::Message::Text(
                format!(
                    "{{\"type\":\"response.created\",\"response\":{{\"id\":\"{TAIL_RESPONSE_ID}\",\"model\":\"{TAIL_MODEL}\"}}}}"
                )
                .into(),
            ))
            .await
            .unwrap();
        let _ = ready_tx.send(());
        cancel_rx.await.unwrap();
        #[allow(deprecated)]
        websocket
            .get_mut()
            .set_linger(Some(Duration::ZERO))
            .unwrap();
        let _ = error_tx.send(());
        drop(websocket);
    });

    let (dir, gateway) = fixture(vec![format!("http://127.0.0.1:{upstream_port}/v1")]).await;
    let (usage, events) = attach_observers(&dir, &gateway);
    let mut diagnostic_events = events.subscribe();
    let mut client = downstream_websocket(&gateway).await;
    client
        .send(yawc::Frame::text(CREATE.to_owned()))
        .await
        .unwrap();
    tokio::time::timeout(WAIT, ready_rx)
        .await
        .expect("upstream did not send its first response packet")
        .unwrap();
    let first = tokio::time::timeout(WAIT, client.next())
        .await
        .expect("downstream WebSocket did not receive response.created")
        .expect("downstream WebSocket ended before response.created");
    assert!(first.as_str().contains(TAIL_RESPONSE_ID));
    assert!(gateway
        .view()
        .providers
        .iter()
        .any(|provider| provider.active_requests > 0));
    client
        .send(yawc::Frame::close(1000.into(), "fixture cancel"))
        .await
        .unwrap();
    drop(client);
    wait_for_tail(&gateway, &usage, true).await;
    cancel_tx.send(()).unwrap();
    tokio::time::timeout(FAST_RELEASE, error_rx)
        .await
        .expect("upstream WebSocket RST fixture did not run")
        .unwrap();
    upstream_task.await.unwrap();

    let record = wait_for_usage_record(&usage).await;
    let attempt = record.final_attempt().unwrap();
    assert_ne!(attempt.outcome, "success");
    assert_ne!(
        attempt.end.as_ref().map(|end| end.cause),
        Some(EndCause::Completed)
    );
    let transfer = attempt.transfer.as_ref().unwrap();
    assert_eq!(transfer.upstream_end.as_deref(), Some("DISCONNECTED"));
    assert!(!transfer.upstream_eof);
    assert_neutral_health(&gateway).await;
    assert_interruption_without_terminal(
        &events,
        &mut diagnostic_events,
        &dir.path().join("events"),
        &record,
    )
    .await;
    gateway.stop().await.unwrap();
}

#[tokio::test]
async fn http_gateway_stop_ends_tail_preserves_client_disconnect() {
    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_port = upstream.local_addr().unwrap().port();
    let (ready_tx, ready_rx) = oneshot::channel();
    let (closed_tx, closed_rx) = oneshot::channel();
    let upstream_task = tokio::spawn(async move {
        let (mut socket, _) = upstream.accept().await.unwrap();
        read_http_request(&mut socket).await.unwrap();
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: keep-alive\r\n\r\n",
            )
            .await
            .unwrap();
        send_http_chunk(&mut socket, HTTP_FIRST).await.unwrap();
        let _ = ready_tx.send(());
        let closed = wait_for_disconnect(socket).await;
        let _ = closed_tx.send(closed);
    });

    let (dir, gateway) = fixture(vec![format!("http://127.0.0.1:{upstream_port}/v1")]).await;
    let (usage, events) = attach_observers(&dir, &gateway);
    let mut diagnostic_events = events.subscribe();
    let (mut response, sender, task) = super::super::tests::request_with_reset(
        &gateway,
        "/v1/responses",
        REQUEST_BODY.to_vec(),
        vec![("content-type", "application/json")],
    )
    .await;
    tokio::time::timeout(WAIT, ready_rx)
        .await
        .expect("upstream did not send its first response packet")
        .unwrap();
    let first = first_http_data(&mut response).await;
    assert!(String::from_utf8_lossy(&first).contains(TAIL_RESPONSE_ID));
    assert!(gateway.view().active_connections > 0);
    super::super::tests::reset_and_drop(task, sender, response).await;
    wait_for_tail(&gateway, &usage, false).await;
    gateway.stop().await.unwrap();
    let _ = tokio::time::timeout(FAST_RELEASE, closed_rx)
        .await
        .expect("gateway stop did not close the HTTP upstream")
        .unwrap();
    upstream_task.await.unwrap();

    let record = wait_for_usage_record(&usage).await;
    let attempt = record.final_attempt().unwrap();
    assert_eq!(
        attempt.end.as_ref().map(|end| end.cause),
        Some(EndCause::ClientDisconnected)
    );
    assert_neutral_health(&gateway).await;
    assert_gateway_stop_usage(
        &events,
        &mut diagnostic_events,
        &dir.path().join("events"),
        &record,
    )
    .await;
}

#[tokio::test]
async fn websocket_gateway_stop_ends_tail_preserves_client_disconnect() {
    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_port = upstream.local_addr().unwrap().port();
    let (ready_tx, ready_rx) = oneshot::channel();
    let (closed_tx, closed_rx) = oneshot::channel();
    let upstream_task = tokio::spawn(async move {
        let (socket, _) = upstream.accept().await.unwrap();
        let mut websocket = tokio_tungstenite::accept_async(socket).await.unwrap();
        let first = tokio::time::timeout(WAIT, websocket.next())
            .await
            .expect("upstream did not receive response.create")
            .expect("upstream WebSocket ended before response.create")
            .expect("upstream WebSocket frame should be valid");
        assert!(first.to_text().unwrap().contains("response.create"));
        websocket
            .send(tokio_tungstenite::tungstenite::Message::Text(
                format!(
                    "{{\"type\":\"response.created\",\"response\":{{\"id\":\"{TAIL_RESPONSE_ID}\",\"model\":\"{TAIL_MODEL}\"}}}}"
                )
                .into(),
            ))
            .await
            .unwrap();
        let _ = ready_tx.send(());
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
    });

    let (dir, gateway) = fixture(vec![format!("http://127.0.0.1:{upstream_port}/v1")]).await;
    let (usage, events) = attach_observers(&dir, &gateway);
    let mut diagnostic_events = events.subscribe();
    let mut client = downstream_websocket(&gateway).await;
    client
        .send(yawc::Frame::text(CREATE.to_owned()))
        .await
        .unwrap();
    tokio::time::timeout(WAIT, ready_rx)
        .await
        .expect("upstream did not send its first response packet")
        .unwrap();
    let first = tokio::time::timeout(WAIT, client.next())
        .await
        .expect("downstream WebSocket did not receive response.created")
        .expect("downstream WebSocket ended before response.created");
    assert!(first.as_str().contains(TAIL_RESPONSE_ID));
    assert!(gateway
        .view()
        .providers
        .iter()
        .any(|provider| provider.active_requests > 0));
    drop(client);
    wait_for_tail(&gateway, &usage, true).await;
    gateway.stop().await.unwrap();
    let _ = tokio::time::timeout(FAST_RELEASE, closed_rx)
        .await
        .expect("gateway stop did not close the WebSocket upstream")
        .unwrap();
    upstream_task.await.unwrap();

    let record = wait_for_usage_record(&usage).await;
    let attempt = record.final_attempt().unwrap();
    assert_eq!(
        attempt.end.as_ref().map(|end| end.cause),
        Some(EndCause::ClientDisconnected)
    );
    assert_neutral_health(&gateway).await;
    assert_gateway_stop_usage(
        &events,
        &mut diagnostic_events,
        &dir.path().join("events"),
        &record,
    )
    .await;
}
