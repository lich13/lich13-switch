use crate::gateway::protocol::{Observation, Observer, Protocol, Terminal};
use serde_json::json;
use std::io::Write;

const OBSERVATION_LIMIT: usize = 2 * 1024 * 1024;

fn encoded(encoding: &str, raw: &[u8]) -> Vec<u8> {
    match encoding {
        "identity" => raw.to_vec(),
        "gzip" => {
            let mut encoder = flate2::write::GzEncoder::new(vec![], flate2::Compression::fast());
            encoder.write_all(raw).unwrap();
            encoder.finish().unwrap()
        }
        "deflate" => {
            let mut encoder = flate2::write::ZlibEncoder::new(vec![], flate2::Compression::fast());
            encoder.write_all(raw).unwrap();
            encoder.finish().unwrap()
        }
        "zstd" => zstd::encode_all(raw, 1).unwrap(),
        _ => panic!("unsupported fixture encoding"),
    }
}

#[test]
fn fragmented_sse_preserves_envelope_metadata_and_multiline_events() {
    let raw = concat!(
        ": heartbeat\r\n",
        "event: response.created\r\n",
        "data: {\"type\":\"response.created\",\r\n",
        "data: \"response\":{\"id\":\"resp-first\",\"model\":\"model-first\"}}\r\n\r\n",
        "data: {\"type\":\"response.output_text.delta\",\"id\":\"output-item\",\"delta\":\"中文\"}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-first\",\"model\":\"model-final\"}}\n\n"
    );
    for chunk_size in [1, 3, 17] {
        let mut observer = Observer::new(true, "identity");
        for chunk in raw.as_bytes().chunks(chunk_size) {
            observer.feed(chunk);
        }
        let observation = observer.snapshot(false);
        assert_eq!(observation.response_id.as_deref(), Some("resp-first"));
        assert_eq!(observation.model.as_deref(), Some("model-final"));
        assert_eq!(observation.terminal, Some(Terminal::Success));
        assert!(observation.expects_terminal);
        assert!(!observation.incomplete);
    }
}

#[test]
fn json_is_observed_after_the_complete_document_and_finish_is_idempotent() {
    let raw = br#"{"object":"response","id":"resp-json","model":"fixture-model","status":"completed","output":[{"id":"tool-id"}]}"#;
    let mut protocol = Protocol::new(false);
    protocol.response(200, false, "");
    for chunk in raw.chunks(3) {
        protocol.feed(chunk);
        assert_eq!(protocol.terminal(), None);
    }
    protocol.finish(Some(200), "OK");
    assert_eq!(
        protocol.observation.response_id.as_deref(),
        Some("resp-json")
    );
    assert_eq!(protocol.observation.model.as_deref(), Some("fixture-model"));
    assert_eq!(protocol.terminal(), Some(Terminal::Success));
    assert!(protocol.succeeded());
    protocol.finish(Some(500), "NETWORK");
    assert_eq!(protocol.terminal(), Some(Terminal::Success));
}

#[test]
fn compressed_json_and_sse_support_gzip_deflate_and_zstd() {
    let json = br#"{"object":"response","id":"resp-compressed","model":"fixture-model","status":"completed"}"#;
    let sse = b"data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-compressed\",\"model\":\"fixture-model\"}}\n\n";
    for encoding in ["gzip", "deflate", "zstd"] {
        for (stream, raw) in [(false, json.as_slice()), (true, sse.as_slice())] {
            let bytes = encoded(encoding, raw);
            for chunk_size in [1, 7] {
                let mut protocol = Protocol::new(stream);
                protocol.response(200, stream, encoding);
                for chunk in bytes.chunks(chunk_size) {
                    protocol.feed(chunk);
                }
                protocol.finish(Some(200), "OK");
                assert_eq!(protocol.terminal(), Some(Terminal::Success), "{encoding}");
                assert_eq!(
                    protocol.observation.response_id.as_deref(),
                    Some("resp-compressed")
                );
                assert_eq!(protocol.observation.model.as_deref(), Some("fixture-model"));
                assert!(!protocol.observation.incomplete, "{encoding}");
            }
        }
    }
}

#[test]
fn completed_stream_remains_successful_after_disconnect_or_timeout() {
    for reason in ["CANCELLED", "STREAM_INTERRUPTED", "STREAM_TIMEOUT"] {
        let mut protocol = Protocol::new(true);
        protocol.response(200, true, "identity");
        for chunk in
            b"data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-done\"}}\n\n"
                .chunks(5)
        {
            protocol.feed(chunk);
        }
        assert!(protocol.succeeded());
        protocol.finish(Some(200), reason);
        assert_eq!(protocol.terminal(), Some(Terminal::Success), "{reason}");
        assert_eq!(
            protocol.observation.response_id.as_deref(),
            Some("resp-done")
        );
    }
}

#[test]
fn application_failures_and_transport_failures_are_distinguished() {
    let mut application = Protocol::new(false);
    application.response(200, false, "identity");
    application.feed(br#"{"error":{"code":"server_error"}}"#);
    application.finish(Some(200), "OK");
    assert_eq!(application.terminal(), Some(Terminal::Failure));
    assert!(!application.transport_failure());

    let mut transport = Protocol::new(true);
    transport.response(200, true, "identity");
    transport.finish(Some(200), "STREAM_TIMEOUT");
    assert!(transport.transport_failure());
}

#[test]
fn websocket_values_keep_failure_rejection_cancellation_and_limit_distinct() {
    for (value, expected) in [
        (json!({"status":"completed"}), Terminal::Success),
        (json!({"type":"message_stop"}), Terminal::Success),
        (
            json!({"type":"response.failed","response":{"error":{"code":"server_error"}}}),
            Terminal::Failure,
        ),
        (
            json!({"type":"error","error":{"code":"rate_limit_exceeded"}}),
            Terminal::Failure,
        ),
        (
            json!({"type":"error","error":{"type":"invalid_request_error"}}),
            Terminal::Rejected,
        ),
        (json!({"type":"response.cancelled"}), Terminal::Cancelled),
        (json!({"type":"response.canceled"}), Terminal::Cancelled),
        (json!({"status":"cancelled"}), Terminal::Cancelled),
        (
            json!({"type":"response.incomplete","response":{"incomplete_details":{"reason":"max_output_tokens"}}}),
            Terminal::Limited,
        ),
        (
            json!({"type":"response.incomplete","response":{"incomplete_details":{"reason":"content_filter"}}}),
            Terminal::Rejected,
        ),
        (
            json!({"type":"response.incomplete","response":{"incomplete_details":{"reason":"server_error"}}}),
            Terminal::Failure,
        ),
        (
            json!({"type":"response.incomplete","response":{"incomplete_details":{"reason":"future_reason"}}}),
            Terminal::Unknown,
        ),
    ] {
        let mut protocol = Protocol::new(true);
        protocol.value(&value);
        protocol.finish(Some(101), "UPSTREAM_ERROR");
        assert_eq!(protocol.terminal(), Some(expected), "{value}");
        assert_eq!(
            protocol.succeeded(),
            matches!(expected, Terminal::Success | Terminal::Limited)
        );
    }
}

#[test]
fn chat_completion_waits_for_all_choices_and_preserves_finish_category() {
    for (reason, expected) in [
        ("stop", Terminal::Success),
        ("length", Terminal::Limited),
        ("content_filter", Terminal::Rejected),
    ] {
        let mut observation = Observation::default();
        observation.value(&json!({"choices":[]}));
        assert_eq!(observation.terminal, None);
        observation.value(&json!({"choices":[{"finish_reason":"stop"},{"finish_reason":null}]}));
        assert_eq!(observation.terminal, None);
        assert!(observation.expects_terminal);
        observation.value(&json!({"choices":[{"finish_reason":"stop"},{"finish_reason":reason}]}));
        assert_eq!(observation.terminal, Some(expected));
    }
}

#[test]
fn first_terminal_survives_conflicting_duplicate_events_and_done_marker() {
    for (event, expected) in [
        ("response.completed", Terminal::Success),
        ("response.failed", Terminal::Failure),
        ("response.cancelled", Terminal::Cancelled),
    ] {
        let raw = format!(
            "data: {{\"type\":\"{event}\"}}\n\ndata: {{\"type\":\"response.completed\"}}\n\ndata: {{\"type\":\"response.failed\"}}\n\ndata: [DONE]\n\n"
        );
        let mut observer = Observer::new(true, "");
        for chunk in raw.as_bytes().chunks(3) {
            observer.feed(chunk);
        }
        assert_eq!(observer.snapshot(true).terminal, Some(expected));
        assert_eq!(observer.snapshot(true).terminal, Some(expected));
    }
}

#[test]
fn missing_terminal_and_transport_failure_do_not_become_successful() {
    for (reason, expected) in [
        ("OK", Terminal::Failure),
        ("CANCELLED", Terminal::Cancelled),
        ("NETWORK", Terminal::Failure),
        ("STREAM_TIMEOUT", Terminal::Failure),
        ("CLIENT_ERROR", Terminal::Rejected),
    ] {
        let mut protocol = Protocol::new(true);
        protocol.response(200, true, "");
        protocol.feed(
            b"data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp-unfinished\"}}\n\n",
        );
        protocol.finish(Some(200), reason);
        assert_eq!(protocol.terminal(), Some(expected), "{reason}");
        assert!(!protocol.succeeded());
    }
    let mut rejected = Protocol::new(false);
    rejected.response(400, false, "identity");
    rejected.feed(br#"{"status":"completed"}"#);
    rejected.finish(Some(400), "HTTP");
    assert_eq!(rejected.terminal(), Some(Terminal::Rejected));
    assert!(!rejected.succeeded());
}

#[test]
fn json_observation_limit_applies_after_decompression() {
    let mut at_limit =
        br#"{"object":"response","id":"resp-boundary","status":"completed"}"#.to_vec();
    at_limit.resize(OBSERVATION_LIMIT, b' ');
    let mut observer = Observer::new(false, "");
    observer.feed(&at_limit);
    let observation = observer.snapshot(true);
    assert_eq!(observation.terminal, Some(Terminal::Success));
    assert!(!observation.incomplete);

    let over_limit = serde_json::to_vec(&json!({
        "object":"response", "id":"resp-too-large", "status":"completed",
        "padding":"x".repeat(OBSERVATION_LIMIT)
    }))
    .unwrap();
    for encoding in ["identity", "gzip", "deflate", "zstd"] {
        let mut observer = Observer::new(false, encoding);
        for chunk in encoded(encoding, &over_limit).chunks(4096) {
            observer.feed(chunk);
        }
        let observation = observer.snapshot(true);
        assert!(observation.incomplete, "{encoding}");
        assert_eq!(observation.terminal, None, "{encoding}");
        assert_eq!(observation.response_id, None, "{encoding}");
    }
}

#[test]
fn oversized_sse_line_or_multiline_event_is_skipped_and_next_event_recovers() {
    let line_overflow = format!(
        "data: {{\"type\":\"response.failed\",\"padding\":\"{}\"}}\n\n",
        "x".repeat(OBSERVATION_LIMIT)
    );
    let event_overflow = format!(
        "data: {{\"type\":\"response.failed\",\"first\":\"{}\",\n\
         data: \"second\":\"{}\"}}\n\n",
        "x".repeat(OBSERVATION_LIMIT / 2),
        "y".repeat(OBSERVATION_LIMIT / 2)
    );
    for oversized in [line_overflow, event_overflow] {
        let mut observer = Observer::new(true, "");
        for chunk in oversized.as_bytes().chunks(4096) {
            observer.feed(chunk);
        }
        let observation = observer.snapshot(false);
        assert!(observation.incomplete);
        assert_eq!(observation.terminal, None);
        observer.feed(
            b"data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-recovered\"}}\n\n",
        );
        let observation = observer.snapshot(true);
        assert_eq!(observation.terminal, Some(Terminal::Success));
        assert_eq!(observation.response_id.as_deref(), Some("resp-recovered"));
        assert!(observation.incomplete);
    }
}

#[test]
fn unknown_or_invalid_encoding_marks_observation_incomplete() {
    for encoding in ["br", "gzip", "deflate", "zstd"] {
        let mut observer = Observer::new(false, encoding);
        observer.feed(b"invalid compressed response");
        let observation = observer.snapshot(true);
        assert!(observation.incomplete, "{encoding}");
        assert_eq!(observation.terminal, None);
        assert_eq!(observation.response_id, None);
    }
    let mut protocol = Protocol::new(true);
    protocol.response(200, true, "");
    protocol.feed(b"data: {\"type\":\"response.created\"}\n\n");
    protocol.feed(&vec![b'x'; OBSERVATION_LIMIT + 1]);
    protocol.finish(Some(200), "OK");
    assert!(protocol.observation.incomplete);
    assert_eq!(protocol.terminal(), Some(Terminal::Unknown));
    assert!(!protocol.succeeded());
}

#[test]
fn response_ids_and_models_come_from_bounded_current_protocol_envelopes() {
    let mut observation = Observation::default();
    observation.value(&json!({"type":"response.output_item.added","id":"tool-id","item":{"id":"nested-tool","model":"nested-model"}}));
    assert_eq!(observation.response_id, None);
    assert_eq!(observation.model, None);
    observation
        .value(&json!({"type":"response.created","response":{"id":"resp-1","model":"model-1"}}));
    observation.value(&json!({"type":"response.output_text.delta","id":"output-item","delta":"x"}));
    assert_eq!(observation.response_id.as_deref(), Some("resp-1"));
    assert_eq!(observation.model.as_deref(), Some("model-1"));
    for invalid in [String::new(), "invalid\nvalue".into(), "x".repeat(257)] {
        observation.value(&json!({"object":"response","id":invalid,"model":invalid}));
        assert_eq!(observation.response_id.as_deref(), Some("resp-1"));
        assert_eq!(observation.model.as_deref(), Some("model-1"));
    }
    observation.value(&json!({"type":"response.completed","model":"outer-model","response":{"id":"resp-1","model":"model-final","output":[{"id":"tool-id","model":"nested-model"}]}}));
    assert_eq!(observation.response_id.as_deref(), Some("resp-1"));
    assert_eq!(observation.model.as_deref(), Some("model-final"));
    let mut next_turn = Observation::default();
    next_turn
        .value(&json!({"object":"chat.completion.chunk","id":"completion-2","model":"model-2"}));
    assert_eq!(next_turn.response_id.as_deref(), Some("completion-2"));
    assert_eq!(next_turn.model.as_deref(), Some("model-2"));
    assert_eq!(next_turn.terminal, None);
    let mut plain_json = Observer::new(false, "identity");
    plain_json.feed(br#"{"id":"bare-response","model":"bare-model"}"#);
    let plain_json = plain_json.snapshot(true);
    assert_eq!(plain_json.response_id.as_deref(), Some("bare-response"));
    assert_eq!(plain_json.model.as_deref(), Some("bare-model"));
}

#[test]
fn accounting_payload_does_not_change_protocol_observation() {
    let plain = json!({"object":"response","id":"resp-metadata","model":"fixture-model","status":"completed"});
    let mut with_accounting = plain.clone();
    with_accounting["usage"] = json!({
        "input_tokens": 999999999, "output_tokens": 888888888,
        "input_tokens_details": {"cached_tokens": 777777777}
    });
    let mut expected = Observation::default();
    expected.value(&plain);
    let mut actual = Observation::default();
    actual.value(&with_accounting);
    assert_eq!(actual.response_id, expected.response_id);
    assert_eq!(actual.model, expected.model);
    assert_eq!(actual.terminal, expected.terminal);
    assert_eq!(actual.expects_terminal, expected.expects_terminal);
    assert_eq!(actual.incomplete, expected.incomplete);
}
