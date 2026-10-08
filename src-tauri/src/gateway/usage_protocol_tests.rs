use super::protocol::{Protocol, Terminal};
use crate::usage::{
    model::{safe_id, Filter, Record},
    Service,
};
use serde_json::{json, Value};
use std::{io::Write, time::Duration};

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

fn sse(values: &[Value]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for value in values {
        bytes.extend_from_slice(b"data: ");
        serde_json::to_writer(&mut bytes, value).unwrap();
        bytes.extend_from_slice(b"\n\n");
    }
    bytes.extend_from_slice(b"data: [DONE]\n\n");
    bytes
}

fn fixed_prices(service: &Service) {
    let prices = service.prices().unwrap();
    let view = prices.view();
    let mut config = view.config;
    config.auto_update = false;
    config.fixed.insert(
        "gpt-fixture".into(),
        json!({
            "input_cost_per_token": "0.01", "output_cost_per_token": "0.02",
            "cache_read_input_token_cost": "0.001"
        }),
    );
    prices.configure(config, &view.revision).unwrap();
}

async fn single_record(
    service: &Service,
    events: &mut tokio::sync::broadcast::Receiver<()>,
) -> Record {
    tokio::time::timeout(Duration::from_secs(5), events.recv())
        .await
        .expect("fixture writer did not finish")
        .expect("fixture writer notification closed");
    assert!(service.state().error.is_none());
    let page = service
        .query(|store| store.logs(&Filter::default()))
        .unwrap();
    assert_eq!(page.total, 1);
    let row = page.rows.into_iter().next().unwrap();
    assert_eq!(row.attempts.len(), 1);
    row
}

#[tokio::test]
async fn compressed_sse_usage_and_repeated_terminals_record_one_request() {
    let completed = json!({"type": "response.completed", "response": {
        "id": "fixture-compressed-response", "model": "gpt-fixture", "status": "completed",
        "usage": {"input_tokens": 1000, "output_tokens": 40,
            "input_tokens_details": {"cached_tokens": 600}}
    }});
    let bytes = sse(&[
        json!({"type": "response.created", "response": {
            "id": "fixture-compressed-response", "model": "gpt-fixture"
        }}),
        json!({"type": "response.output_text.delta", "delta": "fixture"}),
        completed.clone(),
        completed,
        json!({"type": "response.failed", "response": {
            "id": "fixture-compressed-response", "error": {"code": "server_error"}
        }}),
    ]);
    for encoding in ["identity", "gzip", "deflate", "zstd"] {
        for chunk_size in [1, 11] {
            let dir = tempfile::tempdir().unwrap();
            let service = Service::new(dir.path());
            fixed_prices(&service);
            let mut events = service.subscribe();
            let trace = service.begin("codex", Some("gpt-fixture"));
            let mut protocol = Protocol::new(true);
            protocol.attach_usage(trace.attempt("fixture-provider", true, "http"));
            protocol.response(200, true, encoding);
            for chunk in encoded(encoding, &bytes).chunks(chunk_size) {
                protocol.feed(chunk);
            }
            protocol.finish(Some(200), "OK");
            protocol.finish(Some(500), "STREAM_TIMEOUT");
            protocol.finish(None, "CANCELLED");
            assert_eq!(protocol.terminal(), Some(Terminal::Success), "{encoding}");
            assert!(protocol.succeeded());
            assert!(!protocol.transport_failure());
            assert!(!protocol.observation.incomplete);
            assert_eq!(protocol.observation.meter.tokens.input, Some(400));
            assert_eq!(protocol.observation.meter.tokens.cache_read, Some(600));
            assert_eq!(protocol.observation.meter.tokens.output, Some(40));
            drop(protocol);
            drop(trace);

            let row = single_record(&service, &mut events).await;
            let attempt = &row.attempts[0];
            assert!(row.completed);
            assert_eq!(attempt.status, Some(200));
            assert_eq!(attempt.outcome, "success");
            assert_eq!(
                attempt.response_id,
                Some(safe_id("fixture-compressed-response"))
            );
            assert_eq!(attempt.response_model.as_deref(), Some("gpt-fixture"));
            assert_eq!(attempt.tokens.total(), Some(1040));
            assert_eq!(attempt.tokens.output, Some(40));
            assert!(attempt.first_token_ms.is_some());
            assert_eq!(attempt.price.as_ref().unwrap().cost, "5.4");
            let totals = service
                .query(|store| Ok(store.dashboard(&Filter::default())?.totals))
                .unwrap();
            assert_eq!((totals.requests, totals.unpriced), (1, 0));
            assert_eq!(totals.cost, "5.4");
            assert_eq!(totals.tokens, attempt.tokens);
        }
    }
}

#[test]
fn claude_stream_merges_input_and_cache_once_while_output_grows() {
    let bytes = sse(&[
        json!({"type": "message_start", "message": {
            "id": "fixture-claude-response", "model": "claude-fixture", "type": "message",
            "usage": {"input_tokens": 100, "output_tokens": 0,
                "cache_read_input_tokens": 40, "cache_creation_input_tokens": 20,
                "cache_creation": {"ephemeral_5m_input_tokens": 20}}
        }}),
        json!({"type": "content_block_delta", "delta": {"type": "text_delta", "text": "fixture"}}),
        json!({"type": "message_delta", "usage": {"output_tokens": 2}}),
        json!({"type": "message_delta", "usage": {"output_tokens": 9}}),
        json!({"type": "message_stop"}),
        json!({"type": "message_stop"}),
    ]);
    for encoding in ["identity", "gzip", "deflate", "zstd"] {
        let mut protocol = Protocol::new(true);
        protocol.response(200, true, encoding);
        for chunk in encoded(encoding, &bytes).chunks(3) {
            protocol.feed(chunk);
        }
        protocol.finish(Some(200), "OK");
        let meter = &protocol.observation.meter;
        assert_eq!(meter.response_id, Some(safe_id("fixture-claude-response")));
        assert_eq!(meter.tokens.input, Some(100));
        assert_eq!(meter.tokens.cache_read, Some(40));
        assert_eq!(meter.tokens.cache_write, Some(20));
        assert_eq!(meter.tokens.cache_write_5m, Some(20));
        assert_eq!(meter.tokens.output, Some(9));
        assert_eq!(meter.tokens.total(), Some(169));
        assert!(meter.first_token_ms.is_some());
        assert_eq!(protocol.terminal(), Some(Terminal::Success));
        assert!(!protocol.transport_failure());
    }
}

#[tokio::test]
async fn accounting_and_missing_prices_do_not_change_protocol_health() {
    for (event, expected) in [
        ("response.completed", Terminal::Success),
        ("response.failed", Terminal::Failure),
        ("response.cancelled", Terminal::Cancelled),
    ] {
        let plain = json!({"type": event, "response": {
            "id": "fixture-health-response", "model": "fixture-unpriced-model",
            "error": {"code": "server_error"}
        }});
        let mut baseline = Protocol::new(true);
        baseline.websocket_status(101);
        baseline.value(&plain);
        baseline.finish(Some(101), "OK");
        assert_eq!(baseline.terminal(), Some(expected));
        for usage in [
            json!({}),
            json!({"input_tokens": -1, "output_tokens": "invalid"}),
            json!({"input_tokens": 100, "output_tokens": 9}),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let service = Service::new(dir.path());
            let mut events = service.subscribe();
            let trace = service.begin("codex", Some("fixture-unpriced-model"));
            let mut protocol = Protocol::new(true);
            protocol.attach_usage(trace.attempt("fixture-provider", true, "websocket"));
            protocol.websocket_status(101);
            let mut with_usage = plain.clone();
            with_usage["response"]["usage"] = usage.clone();
            protocol.value(&with_usage);
            protocol.value(&with_usage);
            protocol.value(&json!({"type": "response.completed"}));
            protocol.finish(Some(101), "OK");
            protocol.finish(None, "NETWORK");
            assert_eq!(protocol.terminal(), baseline.terminal());
            assert_eq!(protocol.succeeded(), baseline.succeeded());
            assert_eq!(protocol.transport_failure(), baseline.transport_failure());
            assert_eq!(
                protocol.observation.incomplete,
                baseline.observation.incomplete
            );
            drop(protocol);
            drop(trace);
            let row = single_record(&service, &mut events).await;
            assert_eq!(row.completed, expected == Terminal::Success);
            assert_eq!(row.attempts[0].status, Some(101));
            assert!(row.attempts[0].price.is_none());
            assert_eq!(
                row.attempts[0].tokens.output,
                usage["output_tokens"].as_u64()
            );
            let totals = service
                .query(|store| Ok(store.dashboard(&Filter::default())?.totals))
                .unwrap();
            assert_eq!((totals.requests, totals.unpriced), (1, 1));
            assert_eq!(totals.cost, "0");
        }
    }
}

#[tokio::test]
async fn repeated_cancellation_and_drop_keep_one_partial_usage_attempt() {
    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(dir.path());
    fixed_prices(&service);
    let mut events = service.subscribe();
    let trace = service.begin("codex", Some("gpt-fixture"));
    let mut protocol = Protocol::new(true);
    protocol.attach_usage(trace.attempt("fixture-provider", true, "http"));
    protocol.response(200, true, "identity");
    let bytes = sse(&[json!({"type": "response.created", "response": {
        "id": "fixture-cancelled-response", "model": "gpt-fixture",
        "usage": {"input_tokens": 100, "output_tokens": 2}
    }})]);
    // The upstream has not emitted either a terminal response or [DONE].
    let content_length = bytes.len() - b"data: [DONE]\n\n".len();
    protocol.feed(&bytes[..content_length]);
    assert_eq!(protocol.terminal(), None);
    protocol.finish(Some(200), "CANCELLED");
    protocol.finish(Some(200), "OK");
    protocol.finish(None, "STREAM_TIMEOUT");
    assert_eq!(protocol.terminal(), Some(Terminal::Cancelled));
    assert!(!protocol.succeeded());
    assert!(!protocol.transport_failure());
    drop(protocol);
    drop(trace);
    let row = single_record(&service, &mut events).await;
    assert!(!row.completed);
    assert_eq!(row.attempts[0].outcome, "cancelled");
    assert_eq!(row.attempts[0].tokens.input, Some(100));
    assert_eq!(row.attempts[0].tokens.output, Some(2));
    assert_eq!(row.attempts[0].price.as_ref().unwrap().cost, "1.04");
}
