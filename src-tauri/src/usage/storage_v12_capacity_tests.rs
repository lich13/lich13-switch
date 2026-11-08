//! Deterministic synthetic reconciliation fixtures; no user data is read.
use super::{bodies, model::*, store::Store};
use rusqlite::Connection;
use serde_json::{json, Value};
use std::collections::BTreeMap;
const FIXED_NOW: i64 = 20_090 * 86_400_000;

fn hash(kind: &str, index: u64) -> String {
    safe_id(&format!("fixture-v12-capacity-{kind}-{index:06}"))
}

fn fixture_uuid(kind: &str, index: u64) -> String {
    let digest = hash(kind, index);
    let mut bytes = [0u8; 16];
    for (i, pair) in digest.as_bytes().chunks_exact(2).take(16).enumerate() {
        bytes[i] = (pair[0] as char).to_digit(16).unwrap() as u8 * 16
            + (pair[1] as char).to_digit(16).unwrap() as u8;
    }
    bytes[6] = (bytes[6] & 15) | 64;
    bytes[8] = (bytes[8] & 63) | 128;
    uuid::Uuid::from_bytes(bytes).to_string()
}

fn record_id(source: &str, index: u64) -> String {
    if source == "proxy" {
        fixture_uuid("record", index)
    } else {
        hash("record", index)
    }
}

fn model(index: u64) -> String {
    [
        "fixture-gpt-4o",
        "fixture-claude-3-7",
        "fixture-o3-mini",
        "fixture-grok-4",
        "fixture-sonnet",
        "fixture-opus",
    ][(index % 6) as usize]
        .into()
}

fn provider(index: u64) -> String {
    [
        "fixture-openai",
        "fixture-anthropic",
        "fixture-vertex",
        "fixture-grok",
        "fixture-bedrock",
    ][(index % 5) as usize]
        .into()
}

fn basis(index: u64) -> Value {
    json!({
        "input_cost_per_token": "0.001",
        "output_cost_per_token": "0.002",
        "cache_read_cost_per_token": "0.0001",
        "cache_write_cost_per_token": "0.0002",
        "fixture": "v12-capacity",
        "basis_revision": index % 3,
    })
}

fn price(index: u64, model: &str) -> Option<PriceSnapshot> {
    if index % 41 == 0 {
        return None;
    }
    let cost = match index % 17 {
        0 => "0",
        1..=4 => "0.0007",
        _ => "0.003",
    };
    Some(PriceSnapshot {
        version: "fixture-v12-price".into(),
        source: "fixture".into(),
        model: model.into(),
        multiplier: if index % 13 == 0 { "1.25" } else { "1" }.into(),
        rates: BTreeMap::from([
            ("input".into(), "0.001".into()),
            ("output".into(), "0.002".into()),
        ]),
        cost: cost.into(),
        basis: Some(basis(index)),
    })
}

fn operation(index: u64) -> Operation {
    match index % 31 {
        0 => Operation::WebSearch,
        1 => Operation::Compaction,
        _ => Operation::Model,
    }
}

fn transfer(index: u64, at: i64, status: Option<u16>) -> TransferMetadata {
    TransferMetadata {
        connection: Some(ConnectionEvidence {
            id: fixture_uuid("connection", index),
            request_sequence: 1,
            read_bytes: 900 + index % 700,
            socket_written_bytes: 1_200 + index % 900,
            events: vec![
                TransportEvent {
                    sequence: 1,
                    at: at + 2,
                    kind: "headers_received".into(),
                },
                TransportEvent {
                    sequence: 2,
                    at: at + 11,
                    kind: if status == Some(200) {
                        "first_byte"
                    } else {
                        "upstream_error"
                    }
                    .into(),
                },
            ],
        }),
        received_at: Some(at.saturating_sub(2)),
        sent_at: at,
        first_byte_at: Some(at + 11),
        terminal_at: Some(at + 800),
        terminal_event: (status != Some(200)).then(|| "error".into()),
        terminal_status: status.map(|s| {
            if s == 200 {
                "completed"
            } else if s == 500 {
                "incomplete"
            } else {
                "failed"
            }
            .into()
        }),
        incomplete_reason: (status == Some(500)).then(|| "fixture_upstream_incomplete".into()),
        upstream_client_request_hash: Some(hash("upstream-client", index)),
        ended_at: at + 900,
        upstream_bytes: 1_000 + index % 2_000,
        downstream_bytes: 1_100 + index % 2_100,
        terminal_handed_off: status == Some(200),
        upstream_eof: status == Some(200),
        upstream_end: Some(
            match status {
                Some(200) => "EOF",
                Some(429) => "CONNECT_FAILED",
                _ => "TIMEOUT",
            }
            .into(),
        ),
        ws_close_code: None,
        downstream_error: (status == Some(429)).then(|| "fixture_capacity".into()),
        timeout_phase: (status == Some(500)).then(|| "response".into()),
        timeout_seconds: (status == Some(500)).then_some(30),
        last_progress_ms: Some(700),
        client_request_hash: Some(hash("client-request", index)),
        response_hash: Some(hash("response-body", index)),
    }
}

fn attempt(
    source: &str,
    index: u64,
    payload: u64,
    response_id: Option<String>,
    at: i64,
) -> Attempt {
    let operation = operation(payload);
    let model = model(payload);
    let is_proxy = source == "proxy";
    let is_grok = source == "grok";
    let status = if is_proxy {
        Some(match payload % 43 {
            0 => 429,
            1 => 500,
            _ => 200,
        })
    } else {
        None
    };
    let zero = payload % 53 == 0;
    let input = if zero {
        Some(0)
    } else {
        Some(64 + payload % 2_000)
    };
    let output = if zero {
        Some(0)
    } else {
        Some(8 + payload % 97)
    };
    let mut value = Attempt {
        id: if is_proxy {
            fixture_uuid("attempt", index)
        } else {
            record_id(source, index)
        },
        provider: is_grok
            .then(|| "fixture-grok-native".into())
            .or_else(|| is_proxy.then(|| provider(payload))),
        requested_model: Some(model.clone()),
        response_model: Some(model.clone()),
        pricing_model: Some(model.clone()),
        response_id,
        status,
        outcome: if is_proxy {
            match status {
                Some(429) => "capacity",
                Some(500) => "upstream_error",
                _ => "success",
            }
        } else {
            "completed"
        }
        .into(),
        started_at: at,
        duration_ms: 700 + payload % 1_200,
        first_token_ms: (is_proxy && operation == Operation::Model).then_some(80 + payload % 120),
        stream: is_proxy && operation == Operation::Model,
        transport: if is_proxy { "http" } else { "session" }.into(),
        tokens: Tokens {
            reasoning: None,
            image_cache_read: None,
            inclusive_input: Some(input.unwrap_or_default().saturating_add(payload % 9)),
            input,
            output,
            cache_read: Some(payload % 32),
            cache_write: is_grok.then_some(payload % 11),
            cache_write_5m: is_grok.then_some(payload % 7),
            cache_write_1h: is_grok.then_some(payload % 5),
            image_input: (payload % 67 == 0).then_some(1),
            image_output: (payload % 71 == 0).then_some(2),
            audio_input: (payload % 73 == 0).then_some(3),
            audio_output: (payload % 79 == 0).then_some(4),
        },
        service_tier: Some(
            if payload % 3 == 0 {
                "standard"
            } else {
                "priority"
            }
            .into(),
        ),
        price: price(payload, &model),
        ..Attempt::default()
    };
    if is_proxy {
        value.transfer = Some(transfer(payload, at, status));
        value.thread_hash = Some(hash("thread", payload));
        value.upstream_request_hash = Some(hash("upstream", payload));
    }
    if payload % 29 == 0 {
        value.end = Some(EndMetadata {
            cause: if status == Some(429) {
                EndCause::UpstreamRejected
            } else {
                EndCause::UpstreamIncomplete
            },
            terminal_received: status == Some(200),
            response_started: payload % 2 == 0,
        });
        value.usage_status = "parse_incomplete".into();
    }
    if !is_proxy && payload % 19 == 0 {
        value.routing_decision = Some(RoutingDecision::NewThreadQueueWaiting);
        value.routing_evidence = Some(RoutingEvidence {
            input: RoutingInput::FirstTurn,
            has_tool_declarations: payload % 2 == 0,
            restriction: Some(RoutingRestriction::QueueWaiting),
            ..Default::default()
        });
    }
    value.annotate_availability(source);
    value
}

fn record(source: &str, index: u64, payload: u64, response_id: Option<String>, at: i64) -> Record {
    let client = if source == "grok" { "grok" } else { "codex" };
    let mut attempts = Vec::with_capacity(if payload % 73 == 0 { 2 } else { 1 });
    if payload % 73 == 0 {
        let retry_at = at.saturating_sub(250);
        let mut retry = attempt(
            source,
            index.saturating_add(1_000_000),
            payload,
            None,
            retry_at,
        );
        retry.status = if source == "proxy" { Some(503) } else { None };
        retry.outcome = "retryable_error".into();
        retry.end = Some(EndMetadata {
            cause: EndCause::UpstreamDisconnected,
            terminal_received: false,
            response_started: true,
        });
        attempts.push(retry);
    }
    attempts.push(attempt(source, index, payload, response_id, at));
    Record {
        id: record_id(source, index),
        client: client.into(),
        source: source.into(),
        started_at: at,
        finished_at: Some(at + 900),
        session_id: (source != "proxy").then(|| hash("session", payload)),
        attempts,
        completed: source != "proxy" || payload % 29 != 0,
        estimated_speed: payload % 97 == 0,
        ..Record::default()
    }
}

#[test]
fn v12_fixture_reconcile_semantics_are_deterministic() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open_v11_fixture(dir.path()).unwrap();
    let proxy_exact = record("proxy", 1, 1, Some(hash("response", 1)), FIXED_NOW);
    let exact = record("codex", 2, 1, Some(hash("response", 1)), FIXED_NOW + 1_000);
    let proxy_strict = record("proxy", 7, 7, Some(hash("response", 7)), FIXED_NOW + 2_000);
    let strict = record("codex", 3, 7, None, FIXED_NOW + 3_000);
    let proxy_conflict = record("proxy", 8, 8, Some(hash("response", 8)), FIXED_NOW + 4_000);
    let conflict_a = record("codex", 4, 8, None, FIXED_NOW + 5_000);
    let conflict_b = record("codex", 5, 8, None, FIXED_NOW + 6_000);
    let grok = record("grok", 6, 9, None, FIXED_NOW + 7_000);
    store
        .write_batch(
            &[
                proxy_exact,
                exact,
                proxy_strict,
                strict,
                proxy_conflict,
                conflict_a,
                conflict_b,
                grok,
            ],
            None,
        )
        .unwrap();
    let connection = Connection::open(dir.path().join("usage.sqlite")).unwrap();
    let mut statement = connection
        .prepare("SELECT id,effective,body,duplicate_of FROM records ORDER BY id")
        .unwrap();
    let encoded_rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    drop(statement);
    let rows = encoded_rows
        .into_iter()
        .map(|(id, effective, body, target)| {
            let decoded: Record = bodies::decode(&connection, &body).unwrap();
            (id, effective, decoded.deduplication, target)
        })
        .collect::<Vec<_>>();
    assert!(rows.iter().any(|(_, effective, rule, target)| {
        *effective == 0 && rule == "response_id" && target.is_some()
    }));
    assert!(rows.iter().any(|(_, effective, rule, target)| {
        *effective == 0 && rule == "strict_match" && target.is_some()
    }));
    assert!(rows
        .iter()
        .any(|(_, effective, rule, _)| { *effective == 2 && rule == "ambiguous" }));
    assert!(rows
        .iter()
        .any(|(id, effective, _, _)| { id == &hash("record", 6) && *effective == 1 }));
}
