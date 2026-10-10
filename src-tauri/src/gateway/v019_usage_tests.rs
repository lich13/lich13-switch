//! v0.19 usage observation regressions. Fixtures contain no user data.
use super::protocol::{Observer, Protocol, Terminal};
use crate::usage::{
    model::{safe_id, Filter, Record},
    Service,
};
use flate2::{write::GzEncoder, Compression};
use serde_json::json;
use std::{io::Write, time::Duration};

const WAIT: Duration = Duration::from_secs(3);
const MAX_EVENT: usize = 2 * 1024 * 1024;

fn records(service: &Service) -> Vec<Record> {
    service
        .query(|store| Ok(store.logs(&Filter::default())?.rows))
        .unwrap()
}

async fn one_record(service: &Service) -> Record {
    let mut events = service.subscribe();
    tokio::time::timeout(WAIT, async {
        loop {
            if let [row] = records(service).as_slice() {
                return row.clone();
            }
            events.recv().await.expect("fixture usage writer stopped");
        }
    })
    .await
    .expect("fixture usage record was not written")
}

fn encode(payload: &[u8], encoding: &str) -> Vec<u8> {
    match encoding {
        "gzip" => {
            let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
            encoder.write_all(payload).unwrap();
            encoder.finish().unwrap()
        }
        "deflate" => {
            let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), Compression::fast());
            encoder.write_all(payload).unwrap();
            encoder.finish().unwrap()
        }
        "br" => {
            let mut encoder = brotli::CompressorWriter::new(Vec::new(), 4096, 5, 22);
            encoder.write_all(payload).unwrap();
            encoder.into_inner()
        }
        "zstd" => zstd::stream::encode_all(payload, 1).unwrap(),
        _ => payload.to_vec(),
    }
}

fn feed_fragmented(protocol: &mut Protocol, bytes: &[u8], size: usize) {
    for chunk in bytes.chunks(size) {
        protocol.feed(chunk);
    }
}

#[tokio::test]
async fn event_only_sse_fields_drive_meter_terminal_and_usage_without_final_newline() {
    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(dir.path());
    let trace = service.begin("codex", Some("fixture-request-model"));
    let mut protocol = Protocol::new(true);
    protocol.attach_usage(trace.attempt("fixture-provider", true, "http"));
    protocol.response(200, true, "identity");

    // The SSE event field is authoritative here; data objects intentionally omit
    // their type and the final data line has no trailing newline.
    let payload = concat!(
        "event: response.created\r\n",
        "data: {\"response\":{\"id\":\"fixture-event-response\",\"model\":\"fixture-event-model\"}}\r\n",
        "\r\n",
        "event: response.output_text.delta\r\n",
        "data: {\"delta\":\"fixture-token\"}\r\n",
        "\r\n",
        "event: response.completed\r\n",
        "data: {\"response\":{\"id\":\"fixture-event-response\",\"model\":\"fixture-event-model\",\"usage\":{\"input_tokens\":17,\"output_tokens\":0}}}"
    )
    .as_bytes();
    feed_fragmented(&mut protocol, payload, 5);
    protocol.finish(Some(200), "OK");

    assert_eq!(protocol.observation.terminal, Some(Terminal::Success));
    assert_eq!(
        protocol.observation.meter.response_id,
        Some(safe_id("fixture-event-response"))
    );
    assert_eq!(
        protocol.observation.meter.model.as_deref(),
        Some("fixture-event-model")
    );
    assert_eq!(protocol.observation.meter.tokens.input, Some(17));
    assert_eq!(protocol.observation.meter.tokens.output, Some(0));
    assert!(protocol.observation.meter.first_token_ms.is_some());

    drop(protocol);
    drop(trace);
    let row = one_record(&service).await;
    let attempt = row.final_attempt().unwrap();
    assert!(row.completed);
    assert_eq!(attempt.outcome, "success");
    assert_eq!(attempt.response_id, Some(safe_id("fixture-event-response")));
    assert_eq!(attempt.tokens.input, Some(17));
    assert_eq!(attempt.tokens.output, Some(0));
    assert!(attempt.first_token_ms.is_some());
}

#[tokio::test]
async fn fragmented_multiline_sse_keeps_fields_and_compressed_variants_record_usage() {
    let payload = concat!(
        "event: response.created\r\n",
        "data: {\"response\":{\"id\":\"fixture-compressed-response\",\"model\":\"fixture-compressed-model\"}}\r\n",
        "\r\n",
        "event: response.completed\r\n",
        "data: {\"response\":{\"id\":\"fixture-compressed-response\",\"model\":\"fixture-compressed-model\",\"usage\":{\"input_tokens\":100,\r\n",
        "data: \"output_tokens\":0}}}\r\n",
        "\r\n"
    )
    .as_bytes()
    .to_vec();

    for encoding in ["gzip", "deflate", "zstd", "br"] {
        let dir = tempfile::tempdir().unwrap();
        let service = Service::new(dir.path());
        let trace = service.begin("codex", Some("fixture-request-model"));
        let mut protocol = Protocol::new(true);
        protocol.attach_usage(trace.attempt("fixture-provider", true, "http"));
        protocol.response(200, true, encoding);
        let encoded = encode(&payload, encoding);
        feed_fragmented(&mut protocol, &encoded, 3);
        protocol.finish(Some(200), "OK");
        assert_eq!(
            protocol.observation.terminal,
            Some(Terminal::Success),
            "{encoding}"
        );
        assert_eq!(
            protocol.observation.meter.tokens.input,
            Some(100),
            "{encoding}"
        );
        assert_eq!(
            protocol.observation.meter.tokens.output,
            Some(0),
            "{encoding}"
        );
        drop(protocol);
        drop(trace);

        let row = one_record(&service).await;
        assert!(row.completed, "{encoding}");
        let attempt = row.final_attempt().unwrap();
        assert_eq!(
            attempt.response_id,
            Some(safe_id("fixture-compressed-response")),
            "{encoding}"
        );
        assert_eq!(attempt.tokens.input, Some(100), "{encoding}");
        assert_eq!(attempt.tokens.output, Some(0), "{encoding}");
    }
}

#[test]
fn oversized_output_event_does_not_hide_terminal_usage_after_the_two_mib_bound() {
    let mut protocol = Protocol::new(true);
    protocol.response(200, true, "identity");
    let large_delta = "x".repeat(MAX_EVENT);
    let oversized = format!(
        "data: {}\n\n",
        serde_json::to_string(&json!({
            "type": "response.output_text.delta",
            "delta": large_delta,
        }))
        .unwrap()
    );
    let tail = br#"data: {"type":"response.completed","response":{"id":"fixture-large-response","model":"fixture-large-model","usage":{"input_tokens":123,"output_tokens":0}}}

"#;
    feed_fragmented(&mut protocol, oversized.as_bytes(), 64 * 1024);
    feed_fragmented(&mut protocol, tail, 11);
    protocol.finish(Some(200), "OK");

    // The bounded projector consumes the large generated string without
    // retaining it, so the trailing terminal metadata remains complete.
    assert!(!protocol.observation.incomplete);
    assert_eq!(protocol.observation.terminal, Some(Terminal::Success));
    assert_eq!(protocol.observation.meter.tokens.input, Some(123));
    assert_eq!(protocol.observation.meter.tokens.output, Some(0));
    assert_eq!(
        protocol.observation.meter.response_id,
        Some(safe_id("fixture-large-response"))
    );
}

#[test]
fn terminal_placeholder_zero_does_not_erase_known_positive_usage() {
    let mut observer = Observer::new(true, "identity");
    observer.feed(
        br#"data: {"id":"fixture-positive-response","model":"fixture-model","usage":{"input_tokens":100,"output_tokens":7,"cache_read_input_tokens":20}}

"#,
    );
    observer.feed(
        br#"data: {"type":"response.completed","response":{"id":"fixture-positive-response","model":"fixture-model","usage":{"input_tokens":0,"output_tokens":0,"cache_read_input_tokens":0}}}

"#,
    );
    let observation = observer.snapshot(true);
    assert_eq!(observation.terminal, Some(Terminal::Success));
    assert_eq!(observation.meter.tokens.input, Some(100));
    assert_eq!(observation.meter.tokens.output, Some(7));
    assert_eq!(observation.meter.tokens.cache_read, Some(20));
    assert_eq!(observation.meter.tokens.inclusive_input, None);
}
