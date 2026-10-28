//! v0.25.13 cancellation-tail boundaries. All upstream data is synthetic.
use super::usage_tail_tests::*;
use super::*;
use crate::gateway::protocol::{Protocol, Terminal};
use crate::usage::model::EndCause;
use futures_util::{SinkExt, StreamExt};
use http_body_util::BodyExt;
use hyper::{body::Incoming, Response};
use std::io::Write;
use std::time::{Duration, Instant};
use tokio::{
    io::AsyncWriteExt,
    net::{TcpListener, TcpStream},
    sync::oneshot,
};

const ZERO_COMPLETED_HTTP: &[u8] = br#"data: {"type":"response.completed","response":{"id":"fixture-tail-response","model":"fixture-tail-model","status":"completed","usage":{"input_tokens":42,"output_tokens":0,"cache_read_input_tokens":0,"cache_creation_input_tokens":0,"output_tokens_details":{"audio_tokens":0}}}}

"#;
const DELAYED_USAGE_HTTP: &[u8] = br#"data: {"type":"response.usage","response_id":"fixture-tail-response","usage":{"output_tokens":17,"cache_read_input_tokens":5,"cache_creation_input_tokens":3}}

"#;
const WRONG_USAGE_HTTP: &[u8] = br#"data: {"type":"response.usage","response_id":"fixture-other-response","usage":{"output_tokens":999,"cache_read_input_tokens":999,"cache_creation_input_tokens":999}}

"#;

async fn send_chunk(stream: &mut TcpStream, payload: &[u8]) -> std::io::Result<()> {
    stream
        .write_all(format!("{:X}\r\n", payload.len()).as_bytes())
        .await?;
    stream.write_all(payload).await?;
    stream.write_all(b"\r\n").await?;
    stream.flush().await
}

async fn read_until(response: &mut Response<Incoming>, needle: &str) -> Vec<u8> {
    let mut bytes = Vec::new();
    tokio::time::timeout(WAIT, async {
        while !String::from_utf8_lossy(&bytes).contains(needle) {
            let frame = response
                .body_mut()
                .frame()
                .await
                .expect("HTTP response ended before the expected event")
                .expect("HTTP response frame should be readable");
            if let Some(data) = frame.data_ref() {
                bytes.extend_from_slice(data);
            }
        }
    })
    .await
    .expect("HTTP response did not produce the expected event");
    bytes
}

fn assert_fixed_tail(elapsed: Duration) {
    assert!(
        elapsed >= TAIL_WINDOW.saturating_sub(Duration::from_millis(150)),
        "tail ended before the fixed 1500ms deadline: {elapsed:?}"
    );
    assert!(
        elapsed <= TAIL_WINDOW + TAIL_TOLERANCE,
        "tail exceeded the fixed deadline tolerance: {elapsed:?}"
    );
}

#[tokio::test]
async fn http_tail_reads_delayed_usage_after_completed_zero_usage_and_waits_deadline() {
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
        send_chunk(&mut socket, HTTP_FIRST).await.unwrap();
        send_chunk(&mut socket, ZERO_COMPLETED_HTTP).await.unwrap();
        let _ = ready_tx.send(());
        cancel_rx.await.unwrap();
        tokio::time::sleep(CANCEL_DELAY).await;
        let wrong = send_chunk(&mut socket, WRONG_USAGE_HTTP).await;
        let delayed = send_chunk(&mut socket, DELAYED_USAGE_HTTP).await;
        let _ = tail_tx.send(wrong.is_ok() && delayed.is_ok());
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
        .expect("upstream did not send the completed zero-usage prefix")
        .unwrap();
    let prefix = read_until(&mut response, "response.completed").await;
    assert!(String::from_utf8_lossy(&prefix).contains(TAIL_RESPONSE_ID));
    let cancelled_at = Instant::now();
    super::super::tests::reset_and_drop(task, sender, response).await;
    cancel_tx.send(()).unwrap();

    assert!(tokio::time::timeout(TAIL_WINDOW, tail_rx)
        .await
        .expect("tail did not reach the delayed usage frames")
        .expect("tail fixture task dropped before reporting its write"));
    let closed_at = tokio::time::timeout(TAIL_WINDOW + TAIL_TOLERANCE, closed_rx)
        .await
        .expect("HTTP tail stayed open beyond the bounded deadline")
        .unwrap();
    assert_fixed_tail(closed_at.duration_since(cancelled_at));

    upstream_task.await.unwrap();
    let record = wait_for_usage_record(&usage).await;
    assert_tail_usage_with_handoff(&record, true);
    assert_eq!(
        record
            .final_attempt()
            .unwrap()
            .end
            .as_ref()
            .map(|v| v.cause),
        Some(EndCause::Completed)
    );
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
async fn websocket_tail_reads_delayed_usage_after_completed_zero_usage_and_waits_deadline() {
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
                format!("{{\"type\":\"response.output_text.delta\",\"delta\":\"fixture\",\"response\":{{\"id\":\"{TAIL_RESPONSE_ID}\",\"model\":\"{TAIL_MODEL}\"}}}}").into(),
            ))
            .await
            .unwrap();
        websocket
            .send(tokio_tungstenite::tungstenite::Message::Text(
                format!("{{\"type\":\"response.completed\",\"response\":{{\"id\":\"{TAIL_RESPONSE_ID}\",\"model\":\"{TAIL_MODEL}\",\"status\":\"completed\",\"usage\":{{\"input_tokens\":42,\"output_tokens\":0,\"cache_read_input_tokens\":0,\"cache_creation_input_tokens\":0,\"output_tokens_details\":{{\"audio_tokens\":0}}}}}}}}").into(),
            ))
            .await
            .unwrap();
        let _ = ready_tx.send(());
        cancel_rx.await.unwrap();
        tokio::time::sleep(CANCEL_DELAY).await;
        let wrong = websocket
            .send(tokio_tungstenite::tungstenite::Message::Text(
                r#"{"type":"response.usage","response_id":"fixture-other-response","usage":{"output_tokens":999,"cache_read_input_tokens":999}}"#.to_owned().into(),
            ))
            .await;
        let delayed = websocket
            .send(tokio_tungstenite::tungstenite::Message::Text(
                format!("{{\"type\":\"response.usage\",\"response_id\":\"{TAIL_RESPONSE_ID}\",\"usage\":{{\"output_tokens\":17,\"cache_read_input_tokens\":5,\"cache_creation_input_tokens\":3}}}}").into(),
            ))
            .await;
        let _ = tail_tx.send(wrong.is_ok() && delayed.is_ok());
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
        .expect("upstream did not send the completed zero-usage prefix")
        .unwrap();
    let mut completed = false;
    while !completed {
        let frame = tokio::time::timeout(WAIT, client.next())
            .await
            .expect("downstream WebSocket ended before response.completed")
            .expect("downstream WebSocket did not produce a frame");
        completed = frame.as_str().contains("response.completed");
    }
    let cancelled_at = Instant::now();
    client
        .send(yawc::Frame::close(1000.into(), "fixture cancel"))
        .await
        .unwrap();
    drop(client);
    cancel_tx.send(()).unwrap();

    assert!(tokio::time::timeout(TAIL_WINDOW, tail_rx)
        .await
        .expect("WebSocket tail did not reach the delayed usage frames")
        .expect("WebSocket tail fixture dropped before reporting its write"));
    let closed_at = tokio::time::timeout(TAIL_WINDOW + TAIL_TOLERANCE, closed_rx)
        .await
        .expect("WebSocket tail stayed open beyond the bounded deadline")
        .unwrap();
    assert_fixed_tail(closed_at.duration_since(cancelled_at));

    upstream_task.await.unwrap();
    let record = wait_for_usage_record(&usage).await;
    assert_tail_usage_with_handoff(&record, true);
    assert_eq!(
        record
            .final_attempt()
            .unwrap()
            .end
            .as_ref()
            .map(|v| v.cause),
        Some(EndCause::Completed)
    );
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
async fn http_tail_returns_on_clean_upstream_eof_before_the_deadline() {
    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_port = upstream.local_addr().unwrap().port();
    let (ready_tx, ready_rx) = oneshot::channel();
    let (cancel_tx, cancel_rx) = oneshot::channel();
    let upstream_task = tokio::spawn(async move {
        let (mut socket, _) = upstream.accept().await.unwrap();
        read_http_request(&mut socket).await.unwrap();
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: keep-alive\r\n\r\n",
            )
            .await
            .unwrap();
        send_chunk(&mut socket, HTTP_FIRST).await.unwrap();
        let _ = ready_tx.send(());
        cancel_rx.await.unwrap();
        tokio::time::sleep(CANCEL_DELAY).await;
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
        .expect("upstream did not send the response prefix")
        .unwrap();
    let _ = read_until(&mut response, TAIL_RESPONSE_ID).await;
    let cancelled_at = Instant::now();
    super::super::tests::reset_and_drop(task, sender, response).await;
    cancel_tx.send(()).unwrap();
    upstream_task.await.unwrap();

    let _record = tokio::time::timeout(
        TAIL_WINDOW.saturating_sub(Duration::from_millis(300)),
        wait_for_usage_record(&usage),
    )
    .await
    .expect("clean upstream EOF did not finish the tail early");
    assert!(
        cancelled_at.elapsed() < TAIL_WINDOW.saturating_sub(Duration::from_millis(250)),
        "clean EOF should end the tail before its deadline"
    );
    assert_neutral_health(&gateway).await;
    let _ = events;
    let _ = &mut diagnostic_events;
    gateway.stop().await.unwrap();
}

fn gzip(raw: &[u8]) -> Vec<u8> {
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    encoder.write_all(raw).unwrap();
    encoder.finish().unwrap()
}

#[test]
fn usage_tail_accepts_fragmented_compressed_current_response_only() {
    let raw = br#"data: {"type":"response.created","response":{"id":"fixture-tail-response"}}

data: {"type":"response.completed","response":{"id":"fixture-tail-response","status":"completed","usage":{"input_tokens":42,"output_tokens":17,"cache_read_input_tokens":5,"cache_creation_input_tokens":3}}}

"#;
    for chunk_size in [1, 7, 19] {
        let bytes = gzip(raw);
        let mut protocol = Protocol::new(true);
        protocol.response(200, true, "gzip");
        protocol.begin_usage_tail(false);
        for chunk in bytes.chunks(chunk_size) {
            protocol.feed(chunk);
        }
        // A streaming decoder only flushes its final member at the upstream
        // EOF/deadline boundary; feeding the last compressed bytes is not EOF.
        protocol.finish_usage_tail("upstream_eof");
        assert_eq!(
            protocol.observation.response_id.as_deref(),
            Some(TAIL_RESPONSE_ID)
        );
        assert_eq!(protocol.terminal(), Some(Terminal::Success));
        assert_eq!(protocol.observation.meter.tokens.input, Some(34));
        assert_eq!(protocol.observation.meter.tokens.output, Some(17));
        assert_eq!(protocol.observation.meter.tokens.cache_read, Some(5));
        assert_eq!(protocol.observation.meter.tokens.cache_write, Some(3));
    }
}
