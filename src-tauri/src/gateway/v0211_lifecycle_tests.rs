//! v0.21.1 lifecycle and bounded usage metadata regressions.
//!
//! These tests stay at the protocol/usage boundary so they can exercise the
//! same lifecycle decisions used by HTTP and websocket forwarding without
//! opening a real listener.  All IDs and request data are synthetic fixtures.
use super::protocol::Protocol;
use crate::usage::{
    model::{EndCause, Filter, RoutingDecision},
    Service,
};
use bytes::Bytes;
use flate2::{write::GzEncoder, Compression};
use http_body_util::{BodyExt, StreamBody};
use hyper::{body::Frame, HeaderMap, Response};
use std::{
    io::Write,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};
use tokio::sync::{watch, Notify};

const COMPLETED: &[u8] =
    br#"data: {"type":"response.completed","response":{"id":"fixture-completed","status":"completed"}}

"#;

struct CancelGuard(Arc<Notify>);

impl Drop for CancelGuard {
    fn drop(&mut self) {
        self.0.notify_one();
    }
}

fn wait_for_record(service: &Service) -> crate::usage::model::Record {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Ok(page) = service.read(|store| store.logs(&Filter::default())) {
            if let Some(record) = page.rows.into_iter().next() {
                return record;
            }
        }
        assert!(Instant::now() < deadline, "usage record was not persisted");
        thread::sleep(Duration::from_millis(10));
    }
}

fn record_for<F>(configure: F) -> crate::usage::model::Record
where
    F: FnOnce(&mut Protocol),
{
    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(dir.path());
    let trace = service.begin("codex", Some("fixture-model"));
    let mut protocol = Protocol::new(true);
    protocol.attach_usage(trace.attempt("fixture-provider", true, "http"));
    protocol.response(200, true, "identity");
    configure(&mut protocol);
    drop(protocol);
    drop(trace);
    wait_for_record(&service)
}

fn feed_completed(protocol: &mut Protocol) {
    for chunk in COMPLETED.chunks(3) {
        protocol.feed(chunk);
    }
}

#[test]
fn completed_then_disconnect_cancel_or_stop_keeps_success_but_records_the_followup_cause() {
    for (reason, cause) in [
        ("CLIENT_DISCONNECTED", EndCause::Completed),
        ("CANCELLED", EndCause::Completed),
        ("GATEWAY_STOPPED", EndCause::Completed),
    ] {
        let record = record_for(|protocol| {
            feed_completed(protocol);
            protocol.finish(Some(200), "OK");
            protocol.finish(Some(200), reason);
        });
        let attempt = record.final_attempt().unwrap();
        assert!(record.completed, "{reason}");
        assert_eq!(attempt.outcome, "success", "{reason}");
        assert_eq!(attempt.end.as_ref().map(|end| end.cause), Some(cause));
        assert!(attempt.end.as_ref().unwrap().terminal_received);
        assert!(attempt.end.as_ref().unwrap().response_started);
    }
}

#[test]
fn lifecycle_watch_classifies_real_downstream_disconnect_and_gateway_stop_separately() {
    for (stop, downstream, cause, expected_outcome) in [
        (false, true, EndCause::ClientDisconnected, "unknown"),
        (true, true, EndCause::GatewayStopped, "cancelled"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let service = Service::new(dir.path());
        let trace = service.begin("codex", Some("fixture-model"));
        let mut protocol = Protocol::new(true);
        protocol.attach_usage(trace.attempt("fixture-provider", true, "http"));
        protocol.response(200, true, "identity");
        let (stop_tx, stop_rx) = watch::channel(stop);
        let (downstream_tx, downstream_rx) = watch::channel(downstream);
        protocol.watch_lifecycle(Some(stop_rx), Some(downstream_rx));
        drop(stop_tx);
        drop(downstream_tx);
        drop(protocol);
        drop(trace);
        let attempt = wait_for_record(&service).final_attempt().unwrap().clone();
        assert_eq!(attempt.outcome, expected_outcome);
        assert_eq!(attempt.end.as_ref().map(|end| end.cause), Some(cause));
        assert!(!attempt.end.as_ref().unwrap().terminal_received);
        assert!(attempt.end.as_ref().unwrap().response_started);
    }
}

#[test]
fn unsettled_drop_is_unknown_and_does_not_invent_client_cancellation() {
    let record = record_for(|_| {});
    let attempt = record.final_attempt().unwrap();
    assert!(!record.completed);
    assert_eq!(attempt.outcome, "unknown");
    assert_eq!(
        attempt.end.as_ref().map(|end| end.cause),
        Some(EndCause::Unsettled)
    );
    assert!(!attempt.end.as_ref().unwrap().terminal_received);
}

#[test]
fn timeout_and_upstream_disconnect_causes_are_not_user_cancellation() {
    for (reason, cause) in [
        ("FIRST_BYTE_TIMEOUT", EndCause::FirstByteTimeout),
        ("STREAM_TIMEOUT", EndCause::StreamTimeout),
        ("STREAM_INTERRUPTED", EndCause::UpstreamDisconnected),
    ] {
        let record = record_for(|protocol| protocol.finish(Some(200), reason));
        let attempt = record.final_attempt().unwrap();
        assert_eq!(attempt.outcome, "failure", "{reason}");
        assert_eq!(attempt.end.as_ref().map(|end| end.cause), Some(cause));
        assert!(!attempt.end.as_ref().unwrap().terminal_received);
    }
}

#[test]
fn compressed_sse_and_missing_final_newline_flush_terminal_before_recording_end() {
    let raw = b"event: response.completed\ndata: {\"response\":{\"id\":\"fixture-gzip\",\"status\":\"completed\"}}";
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(raw).unwrap();
    let encoded = encoder.finish().unwrap();
    let record = record_for(|protocol| {
        protocol.response(200, true, "gzip");
        for chunk in encoded.chunks(2) {
            protocol.feed(chunk);
        }
        protocol.finish(Some(200), "OK");
    });
    let attempt = record.final_attempt().unwrap();
    assert_eq!(attempt.outcome, "success");
    assert_eq!(
        attempt.end.as_ref().map(|end| end.cause),
        Some(EndCause::Completed)
    );
    assert!(attempt.end.as_ref().unwrap().terminal_received);
}

#[test]
fn token_survives_terminal_disconnect_and_is_written_once() {
    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(dir.path());
    let trace = service.begin("codex", Some("fixture-model"));
    let mut protocol = Protocol::new(true);
    protocol.attach_usage(trace.attempt("fixture-provider", true, "http"));
    protocol.response(200, true, "identity");
    protocol.feed(
        br#"data: {"type":"response.usage","usage":{"input_tokens":12,"output_tokens":4}}

"#,
    );
    feed_completed(&mut protocol);
    protocol.finish(Some(200), "OK");
    protocol.finish(Some(200), "CLIENT_DISCONNECTED");
    drop(protocol);
    drop(trace);
    let record = wait_for_record(&service);
    assert_eq!(record.attempts.len(), 1);
    let attempt = record.final_attempt().unwrap();
    assert_eq!(attempt.tokens.input, Some(12));
    assert_eq!(attempt.tokens.output, Some(4));
    assert_eq!(attempt.outcome, "success");
    assert_eq!(
        attempt.end.as_ref().map(|end| end.cause),
        Some(EndCause::Completed)
    );
}

#[test]
fn json_200_without_a_terminal_event_cannot_become_success_on_drop() {
    let record = record_for(|protocol| {
        protocol.response(200, false, "identity");
        protocol.feed(br#"{"object":"response","id":"fixture-open","status":"in_progress"}"#);
        protocol.finish(Some(200), "UNSETTLED");
    });
    let attempt = record.final_attempt().unwrap();
    assert!(!record.completed);
    assert_ne!(attempt.outcome, "success");
    assert_eq!(attempt.outcome, "unknown");
    assert_eq!(
        attempt.end.as_ref().map(|end| end.cause),
        Some(EndCause::Unsettled)
    );
}

#[test]
fn context_and_upstream_request_id_are_hashed_and_routing_decision_is_preserved() {
    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(dir.path());
    let trace = service.begin("codex", Some("fixture-model"));
    let mut protocol = Protocol::new(false);
    protocol.attach_usage(trace.attempt("fixture-provider", false, "http"));
    protocol.context(
        Some(crate::storage::digest(b"fixture-thread").to_string()),
        Some(RoutingDecision::ExistingThread),
    );
    let mut headers = HeaderMap::new();
    headers.insert("x-request-id", "fixture-upstream-request".parse().unwrap());
    protocol.response_headers(&headers);
    protocol.response(200, false, "identity");
    protocol.feed(br#"{"object":"response","status":"completed"}"#);
    protocol.finish(Some(200), "OK");
    drop(protocol);
    drop(trace);
    let attempt = wait_for_record(&service).final_attempt().unwrap().clone();
    let expected_thread_hash = crate::storage::digest(b"fixture-thread");
    assert_eq!(
        attempt.thread_hash.as_deref(),
        Some(expected_thread_hash.as_str())
    );
    assert_eq!(
        attempt.routing_decision,
        Some(RoutingDecision::ExistingThread)
    );
    assert_eq!(
        attempt.upstream_request_hash,
        Some(crate::storage::digest(b"fixture-upstream-request"))
    );
    assert_ne!(
        attempt.upstream_request_hash.as_deref(),
        Some("fixture-upstream-request")
    );
}

#[tokio::test]
async fn http_client_disconnect_preserves_forwarded_bytes_without_usage_tail() {
    let completed = Bytes::from_static(
        b"data: {\"type\":\"response.completed\",\"response\":{\"id\":\"fixture-http-tail\"}}\n\n",
    );
    let usage = Bytes::from_static(
        b"data: {\"response_id\":\"fixture-http-tail\",\"usage\":{\"input_tokens\":25,\"output_tokens\":3,\"input_tokens_details\":{\"cached_tokens\":20}}}\n\n",
    );
    let expected_completed = completed.clone();
    let cancelled = Arc::new(Notify::new());
    let tail_generated = Arc::new(AtomicBool::new(false));
    let tail_release = Arc::new(Notify::new());
    let cancelled_copy = cancelled.clone();
    let generated_copy = tail_generated.clone();
    let release_copy = tail_release.clone();
    let port = super::server(move |_| {
        let completed = completed.clone();
        let usage = usage.clone();
        let cancelled = cancelled_copy.clone();
        let generated = generated_copy.clone();
        let release = release_copy.clone();
        async move {
            let guard = CancelGuard(cancelled);
            let chunks = async_stream::try_stream! {
                let _guard = guard;
                yield Frame::data(completed);
                release.notified().await;
                generated.store(true, Ordering::SeqCst);
                yield Frame::data(usage);
            };
            Response::builder()
                .header("content-type", "text/event-stream")
                .body(
                    StreamBody::new(chunks)
                        .map_err(|error: std::io::Error| -> super::connector::BoxError {
                            error.into()
                        })
                        .boxed_unsync(),
                )
                .unwrap()
        }
    })
    .await;
    let (dir, gateway) = super::fixture(vec![format!("http://127.0.0.1:{port}")]).await;
    let service = Service::new(dir.path());
    gateway.set_usage(service.clone());
    super::start(&gateway, &dir).await;

    let response = super::request(
        &gateway,
        "fixture-http-disconnect",
        br#"{"model":"fixture-model","stream":true}"#.to_vec(),
    )
    .await;
    let mut body = response.into_body();
    let first = body.frame().await.unwrap().unwrap();
    assert_eq!(
        first.data_ref().map(|bytes| bytes.as_ref()),
        Some(expected_completed.as_ref())
    );
    drop(body);
    tokio::time::timeout(Duration::from_secs(3), cancelled.notified())
        .await
        .expect("upstream stream was not cancelled after client disconnect");

    let record = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Ok(page) = service.read(|store| store.logs(&Filter::default())) {
                if let Some(record) = page.rows.into_iter().next() {
                    break record;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("HTTP disconnect usage record did not settle");
    assert!(record.completed);
    let attempt = record.final_attempt().unwrap();
    assert_eq!(attempt.outcome, "success");
    assert_eq!(attempt.tokens.input, None);
    assert_eq!(attempt.tokens.cache_read, None);
    assert_eq!(attempt.tokens.output, None);
    assert_eq!(
        attempt.end.as_ref().map(|end| end.cause),
        Some(EndCause::Completed)
    );
    assert!(!tail_generated.load(Ordering::SeqCst));
    gateway.stop().await.unwrap();
}
