//! v0.19 usage merge and aggregate regressions. Fixtures contain no user data.
use super::{
    model::{safe_id, Attempt, Meter, PriceSnapshot, Record, Tokens, Totals},
    pricing::Pricing,
    sessions,
    store::Store,
    Service,
};
use serde_json::json;
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::Path,
    thread,
    time::{Duration, Instant},
};

const BASE: i64 = 20_000 * 86_400_000;

#[test]
fn explicitly_settled_early_end_keeps_the_missing_usage_reason() {
    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(dir.path());
    let trace = service.begin("codex", None);
    let mut attempt = trace.attempt("fixture-provider", true, "http");
    let mut meter = Meter {
        ended_early: true,
        ..Meter::default()
    };
    attempt.update(&meter, Some(200), Some("cancelled"));
    assert_eq!(attempt.attempt.usage_status, "ended_early");
    meter.parse_incomplete = true;
    attempt.update(&meter, Some(200), None);
    assert_eq!(attempt.attempt.usage_status, "parse_incomplete");
}

fn write_jsonl(path: &Path, rows: &[serde_json::Value]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut file = fs::File::create(path).unwrap();
    for row in rows {
        serde_json::to_writer(&mut file, row).unwrap();
        file.write_all(b"\n").unwrap();
    }
}

fn append_bytes(path: &Path, bytes: &[u8]) {
    let mut file = OpenOptions::new().append(true).open(path).unwrap();
    file.write_all(bytes).unwrap();
}

fn session_filter() -> super::model::Filter {
    super::model::Filter {
        start: Some(BASE - 1),
        end: Some(BASE + 1),
        ..super::model::Filter::default()
    }
}

fn sync_fixture(
    store: &mut Store,
    pricing: &Pricing,
    client: &str,
    home: &Path,
) -> sessions::Report {
    sessions::sync(
        store,
        pricing,
        &super::model::Settings::default(),
        client,
        home,
        false,
    )
    .unwrap()
}

fn codex_meta() -> serde_json::Value {
    json!({
        "type": "session_meta",
        "timestamp": BASE,
        "payload": {"id": "fixture-v019-codex-session"}
    })
}

fn codex_turn_context() -> serde_json::Value {
    json!({
        "type": "turn_context",
        "timestamp": BASE + 1,
        "payload": {"model": "gpt-fixture"}
    })
}

fn codex_output(content: String) -> serde_json::Value {
    json!({
        "type": "response_item",
        "timestamp": BASE + 2,
        "payload": {"role": "assistant", "content": content}
    })
}

fn codex_count(at: i64, input: u64, cache: u64, output: u64) -> serde_json::Value {
    json!({
        "type": "event_msg",
        "timestamp": at,
        "payload": {
            "type": "token_count",
            "info": {
                "model": "gpt-fixture",
                "total_token_usage": {
                    "input_tokens": input,
                    "cached_input_tokens": cache,
                    "output_tokens": output
                },
                "last_token_usage": {
                    "input_tokens": input,
                    "cached_input_tokens": cache,
                    "output_tokens": output
                }
            }
        }
    })
}

fn claude_message(content: String, output: u64) -> serde_json::Value {
    json!({
        "type": "assistant",
        "sessionId": "fixture-v019-claude-session",
        "uuid": "fixture-v019-claude-uuid",
        "timestamp": BASE,
        "message": {
            "id": "fixture-v019-claude-message",
            "model": "claude-fixture",
            "content": content,
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 100,
                "output_tokens": output,
                "cache_read_input_tokens": 40,
                "cache_creation_input_tokens": 20
            }
        }
    })
}

fn attempt(id: &str, source: &str, tokens: Tokens) -> Attempt {
    Attempt {
        id: format!("{id}-attempt"),
        provider: (source == "proxy").then(|| "fixture-provider".into()),
        requested_model: Some("fixture-request-model".into()),
        response_model: Some("fixture-response-model".into()),
        pricing_model: Some("fixture-response-model".into()),
        response_id: Some(safe_id(id)),
        status: (source == "proxy").then_some(200),
        outcome: "success".into(),
        started_at: BASE,
        duration_ms: 100,
        stream: source == "proxy",
        transport: if source == "proxy" { "http" } else { "session" }.into(),
        tokens,
        ..Attempt::default()
    }
}

fn price_snapshot(cost: &str) -> PriceSnapshot {
    PriceSnapshot {
        version: "fixture-price-v1".into(),
        source: "fixture-catalog".into(),
        model: "fixture-response-model".into(),
        multiplier: "1.25".into(),
        rates: [("input".into(), "0.00001".into())].into_iter().collect(),
        cost: cost.into(),
        basis: Some(json!({"input_cost_per_token":"0.00001"})),
    }
}

fn record(id: &str, source: &str, tokens: Tokens) -> Record {
    Record {
        id: id.into(),
        client: if source == "proxy" {
            "codex".into()
        } else {
            source.into()
        },
        source: source.into(),
        started_at: BASE,
        completed: source == "proxy",
        attempts: vec![attempt(id, source, tokens)],
        ..Record::default()
    }
}

fn source_filter(source: &str) -> super::model::Filter {
    super::model::Filter {
        source: Some(source.into()),
        start: Some(BASE - 1),
        end: Some(BASE + 1),
        ..super::model::Filter::default()
    }
}

#[test]
fn protocol_tokens_keep_missing_and_real_zero_distinct() {
    let zero = super::model::parse_tokens(
        &json!({
            "input_tokens": 0,
            "output_tokens": 0,
            "cached_input_tokens": 0,
            "cache_creation_input_tokens": 0
        }),
        false,
    );
    assert_eq!(
        zero,
        Tokens {
            inclusive_input: Some(0),
            input: Some(0),
            output: Some(0),
            cache_read: Some(0),
            cache_write: Some(0),
            ..Tokens::default()
        }
    );

    let missing = super::model::parse_tokens(&json!({"output_tokens": 4}), false);
    assert_eq!(missing.input, None);
    assert_eq!(missing.cache_read, None);
    assert_eq!(missing.cache_write, None);
    assert_eq!(missing.output, Some(4));
}

#[test]
fn cache_details_arriving_after_inclusive_input_do_not_double_count_tokens() {
    let mut meter = Meter::default();
    meter.observe(&json!({"usage":{"input_tokens":100,"output_tokens":8}}), 1);
    meter.observe(
        &json!({"type":"response.usage","usage":{"input_tokens_details":{"cached_tokens":60}}}),
        2,
    );
    assert_eq!(meter.tokens.input, Some(40));
    assert_eq!(meter.tokens.total(), Some(108));
    meter.observe(&json!({"type":"response.completed","response":{"usage":{"input_tokens":0,"output_tokens":0}}}), 3);
    assert_eq!(meter.tokens.input, Some(40));
    assert_eq!(meter.tokens.total(), Some(108));
}

#[test]
fn exact_response_id_supplements_only_missing_token_fields() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let response = safe_id("fixture-v019-partial-response");
    let mut gateway = record(
        "fixture-v019-partial-gateway",
        "proxy",
        Tokens {
            inclusive_input: Some(100),
            input: Some(100),
            output: Some(7),
            cache_read: None,
            cache_write: Some(0),
            ..Tokens::default()
        },
    );
    gateway.attempts[0].response_id = Some(response.clone());
    gateway.attempts[0].inclusive_input_tokens = Some(100);
    gateway.attempts[0].price = Some(price_snapshot("0.00042"));

    let mut session = record(
        "fixture-v019-partial-session",
        "codex",
        Tokens {
            input: Some(80),
            output: Some(7),
            cache_read: Some(20),
            cache_write: Some(0),
            ..Tokens::default()
        },
    );
    session.completed = false;
    session.attempts[0].response_id = Some(response);
    session.attempts[0].provider = None;
    session.attempts[0].status = None;
    session.attempts[0].stream = false;
    session.attempts[0].transport = "session".into();
    session.attempts[0].price = Some(price_snapshot("0.00081"));

    store
        .write_batch(&[gateway.clone(), session.clone()], None)
        .unwrap();
    let merged = store.detail(&gateway.id).unwrap();
    let tokens = &merged.final_attempt().unwrap().tokens;
    assert_eq!(tokens.input, Some(80));
    assert_eq!(tokens.output, Some(7));
    assert_eq!(tokens.cache_read, Some(20));
    assert_eq!(tokens.cache_write, Some(0));
    assert_eq!(tokens.total(), Some(107));
    assert_eq!(merged.merged_sources, vec!["codex", "proxy"]);
    assert_eq!(merged.deduplication, "response_id");
    assert_eq!(
        merged.final_attempt().unwrap().usage_sources,
        std::collections::BTreeMap::from([
            ("cache_read".into(), "session".into()),
            ("input".into(), "session".into()),
        ])
    );
    assert_eq!(
        merged.final_attempt().unwrap().price.as_ref().unwrap().cost,
        "0.00042"
    );
    assert_eq!(
        merged.gateway_reported.as_ref().unwrap().tokens,
        gateway.attempts[0].tokens
    );
    assert_eq!(
        merged
            .gateway_reported
            .as_ref()
            .unwrap()
            .price
            .as_ref()
            .unwrap()
            .cost,
        "0.00042"
    );

    let gateway_totals = store.dashboard(&source_filter("proxy")).unwrap().totals;
    assert_eq!(gateway_totals.tokens.input, Some(100));
    assert_eq!(gateway_totals.tokens.cache_read, None);
    assert_eq!(gateway_totals.tokens.cache_write, Some(0));
    let session_view = store.detail("fixture-v019-partial-session").unwrap();
    assert_eq!(
        session_view.duplicate_of.as_deref(),
        Some(gateway.id.as_str())
    );
    assert_eq!(
        session_view
            .final_attempt()
            .unwrap()
            .price
            .as_ref()
            .unwrap()
            .cost,
        "0.00081"
    );

    store
        .write_batch(&[gateway.clone(), session.clone()], None)
        .unwrap();
    let repeated = store.detail(&gateway.id).unwrap();
    assert_eq!(repeated.tokens(), merged.tokens());
    assert_eq!(repeated.merged_sources, merged.merged_sources);
    assert_eq!(
        serde_json::to_value(repeated.gateway_reported).unwrap(),
        serde_json::to_value(merged.gateway_reported).unwrap()
    );
}

#[test]
fn exact_response_id_preserves_reported_zero_while_filling_other_missing_fields() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let response = safe_id("fixture-v019-zero-response");
    let mut gateway = record(
        "fixture-v019-zero-gateway",
        "proxy",
        Tokens {
            input: Some(0),
            output: None,
            cache_read: Some(0),
            cache_write: None,
            ..Tokens::default()
        },
    );
    gateway.attempts[0].response_id = Some(response.clone());
    gateway.attempts[0].price = None;

    let mut session = record(
        "fixture-v019-zero-session",
        "codex",
        Tokens {
            input: Some(100),
            output: Some(7),
            cache_read: Some(20),
            cache_write: Some(3),
            ..Tokens::default()
        },
    );
    session.completed = false;
    session.attempts[0].response_id = Some(response);
    session.attempts[0].provider = None;
    session.attempts[0].status = None;
    session.attempts[0].stream = false;
    session.attempts[0].transport = "session".into();
    session.attempts[0].price = None;

    store
        .write_batch(&[gateway.clone(), session], None)
        .unwrap();
    let merged = store.detail(&gateway.id).unwrap();
    let tokens = &merged.final_attempt().unwrap().tokens;
    assert_eq!(tokens.input, Some(0));
    assert_eq!(tokens.output, Some(7));
    assert_eq!(tokens.cache_read, Some(0));
    assert_eq!(tokens.cache_write, Some(3));
}

#[test]
fn exact_response_id_rejects_inconsistent_cache_donation() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let response = safe_id("fixture-v019-zero-response");
    let mut gateway = record(
        "fixture-v019-zero-gateway",
        "proxy",
        Tokens {
            input: Some(100),
            output: Some(7),
            cache_read: None,
            cache_write: Some(0),
            ..Tokens::default()
        },
    );
    gateway.attempts[0].response_id = Some(response.clone());
    gateway.attempts[0].inclusive_input_tokens = Some(100);
    gateway.attempts[0].price = Some(price_snapshot("0.00042"));

    let mut session = record(
        "fixture-v019-zero-session",
        "codex",
        Tokens {
            input: Some(100),
            output: Some(7),
            cache_read: Some(120),
            cache_write: Some(0),
            cache_write_5m: Some(5),
            cache_write_1h: Some(7),
            ..Tokens::default()
        },
    );
    session.completed = false;
    session.attempts[0].response_id = Some(response);
    session.attempts[0].provider = None;
    session.attempts[0].status = None;
    session.attempts[0].stream = false;
    session.attempts[0].transport = "session".into();
    session.attempts[0].price = Some(price_snapshot("0.00081"));

    store
        .write_batch(&[gateway.clone(), session], None)
        .unwrap();
    let merged = store.detail(&gateway.id).unwrap();
    let tokens = &merged.final_attempt().unwrap().tokens;
    assert_eq!(tokens.input, Some(100));
    assert_eq!(tokens.output, Some(7));
    assert_eq!(tokens.cache_read, None);
    assert_eq!(tokens.cache_write, Some(0));
    assert_eq!(tokens.cache_write_5m, None);
    assert_eq!(tokens.cache_write_1h, None);
    assert_eq!(tokens.total(), Some(107));
    assert_eq!(
        merged.final_attempt().unwrap().usage_sources,
        Default::default()
    );
}

#[test]
fn source_views_keep_gateway_only_and_combined_usage_separate_after_supplement() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let response = safe_id("fixture-v019-source-response");
    let mut gateway = record(
        "fixture-v019-source-gateway",
        "proxy",
        Tokens {
            input: None,
            output: Some(7),
            cache_read: None,
            cache_write: None,
            ..Tokens::default()
        },
    );
    gateway.attempts[0].response_id = Some(response.clone());
    gateway.attempts[0].price = None;
    let mut session = record(
        "fixture-v019-source-session",
        "codex",
        Tokens {
            input: Some(100),
            output: Some(7),
            cache_read: Some(20),
            cache_write: Some(3),
            ..Tokens::default()
        },
    );
    session.completed = false;
    session.attempts[0].response_id = Some(response);
    session.attempts[0].provider = None;
    session.attempts[0].status = None;
    session.attempts[0].stream = false;
    session.attempts[0].transport = "session".into();
    session.attempts[0].price = None;
    store
        .write_batch(&[gateway.clone(), session.clone()], None)
        .unwrap();

    let all = store
        .dashboard(&super::model::Filter {
            start: Some(BASE - 1),
            end: Some(BASE + 1),
            ..super::model::Filter::default()
        })
        .unwrap()
        .totals;
    assert_eq!(all.requests, 1);
    assert_eq!(all.tokens.input, Some(100));
    assert_eq!(all.tokens.cache_read, Some(20));

    let gateway_only = store.dashboard(&source_filter("proxy")).unwrap().totals;
    assert_eq!(gateway_only.requests, 1);
    assert_eq!(gateway_only.tokens.input, None);
    assert_eq!(gateway_only.tokens.output, Some(7));
    assert_eq!(gateway_only.tokens.cache_read, None);

    let sessions = store.dashboard(&source_filter("sessions")).unwrap().totals;
    assert_eq!(sessions.requests, 1);
    assert_eq!(sessions.tokens.input, Some(100));
    assert_eq!(sessions.tokens.cache_read, Some(20));
}

#[test]
fn cache_eligibility_omits_missing_classification_but_counts_real_zero() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let complete = record(
        "fixture-v019-cache-complete",
        "proxy",
        Tokens {
            input: Some(100),
            output: Some(1),
            cache_read: Some(20),
            cache_write: Some(10),
            ..Tokens::default()
        },
    );
    let zero = record(
        "fixture-v019-cache-zero",
        "proxy",
        Tokens {
            input: Some(50),
            output: Some(1),
            cache_read: Some(0),
            cache_write: Some(0),
            ..Tokens::default()
        },
    );
    let missing_cache = record(
        "fixture-v019-cache-missing-read",
        "proxy",
        Tokens {
            input: Some(200),
            output: Some(1),
            cache_read: None,
            cache_write: Some(0),
            ..Tokens::default()
        },
    );
    let missing_input = record(
        "fixture-v019-cache-missing-input",
        "proxy",
        Tokens {
            input: None,
            output: Some(1),
            cache_read: Some(9),
            cache_write: Some(0),
            ..Tokens::default()
        },
    );
    store
        .write_batch(&[complete, zero, missing_cache, missing_input], None)
        .unwrap();

    let totals = store
        .dashboard(&super::model::Filter {
            start: Some(BASE - 1),
            end: Some(BASE + 1),
            ..super::model::Filter::default()
        })
        .unwrap()
        .totals;
    assert_eq!(totals.cache_read_eligible, 20);
    assert_eq!(totals.cache_input_eligible, 180);
}

#[test]
fn cache_eligibility_is_zero_when_every_record_is_missing_a_classification() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    store
        .write_batch(
            &[record(
                "fixture-v019-cache-unknown",
                "proxy",
                Tokens {
                    input: Some(100),
                    output: Some(1),
                    cache_read: None,
                    cache_write: None,
                    ..Tokens::default()
                },
            )],
            None,
        )
        .unwrap();
    let totals = store.dashboard(&source_filter("proxy")).unwrap().totals;
    assert_eq!(totals.cache_read_eligible, 0);
    assert_eq!(totals.cache_input_eligible, 0);
}

#[test]
fn meter_observes_nested_usage_and_keeps_terminal_zero_out_of_known_values() {
    let mut meter = Meter::default();
    meter.observe(
        &json!({
            "type": "response.created",
            "response": {
                "id": "fixture-meter-response",
                "model": "fixture-meter-model",
                "usage": {"input_tokens": 100, "output_tokens": 7, "cached_input_tokens": 20}
            }
        }),
        0,
    );
    meter.observe(
        &json!({
            "type": "response.completed",
            "response": {
                "id": "fixture-meter-response",
                "model": "fixture-meter-model",
                "usage": {"input_tokens": 0, "output_tokens": 0, "cached_input_tokens": 0}
            }
        }),
        1,
    );
    assert_eq!(meter.response_id, Some(safe_id("fixture-meter-response")));
    assert_eq!(meter.model.as_deref(), Some("fixture-meter-model"));
    assert_eq!(meter.tokens.input, Some(80));
    assert_eq!(meter.tokens.output, Some(7));
    assert_eq!(meter.tokens.cache_read, Some(20));
}

#[test]
fn totals_keep_cache_eligibility_zero_when_no_complete_input_classification_exists() {
    let mut totals = Totals::default();
    totals.add_record(&record(
        "fixture-v019-totals-unknown",
        "proxy",
        Tokens {
            input: Some(9),
            output: Some(1),
            cache_read: Some(1),
            cache_write: None,
            ..Tokens::default()
        },
    ));
    assert_eq!(totals.cache_read_eligible, 0);
    assert_eq!(totals.cache_input_eligible, 0);
}

#[test]
fn claude_large_content_keeps_usage_metadata_after_the_two_mib_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let pricing = Pricing::new(dir.path()).unwrap();
    let path = dir.path().join("projects/fixture-v019/main.jsonl");
    write_jsonl(&path, &[claude_message("x".repeat(2 * 1024 * 1024 + 1), 9)]);

    let report = sync_fixture(&mut store, &pricing, "claude", dir.path());
    assert_eq!(report.imported, 1);
    let totals = store.dashboard(&session_filter()).unwrap().totals;
    assert_eq!(totals.requests, 1);
    assert_eq!(totals.tokens.input, Some(100));
    assert_eq!(totals.tokens.output, Some(9));
    assert_eq!(totals.tokens.cache_read, Some(40));
}

#[test]
fn codex_token_count_survives_a_large_assistant_output() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let pricing = Pricing::new(dir.path()).unwrap();
    let path = dir.path().join("sessions/fixture-v019.jsonl");
    write_jsonl(
        &path,
        &[
            codex_meta(),
            codex_turn_context(),
            codex_output("y".repeat(2 * 1024 * 1024 + 1)),
            codex_count(BASE + 3, 120, 20, 8),
        ],
    );

    let report = sync_fixture(&mut store, &pricing, "codex", dir.path());
    assert_eq!(report.imported, 1);
    let totals = store.dashboard(&session_filter()).unwrap().totals;
    assert_eq!(totals.requests, 1);
    assert_eq!(totals.tokens.input, Some(100));
    assert_eq!(totals.tokens.cache_read, Some(20));
    assert_eq!(totals.tokens.output, Some(8));
}

#[test]
fn codex_unterminated_tail_is_replayed_after_the_missing_half_arrives() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let pricing = Pricing::new(dir.path()).unwrap();
    let path = dir.path().join("sessions/fixture-v019-partial.jsonl");
    write_jsonl(&path, &[codex_meta(), codex_turn_context()]);
    let tail = serde_json::to_vec(&codex_count(BASE + 3, 64, 0, 5)).unwrap();
    let split = tail.len() / 2;
    append_bytes(&path, &tail[..split]);

    let first = sync_fixture(&mut store, &pricing, "codex", dir.path());
    assert_eq!(first.imported, 0);
    assert_eq!(
        store.dashboard(&session_filter()).unwrap().totals.requests,
        0
    );

    append_bytes(&path, &tail[split..]);
    append_bytes(&path, b"\n");
    let second = sync_fixture(&mut store, &pricing, "codex", dir.path());
    assert_eq!(second.imported, 1);
    let totals = store.dashboard(&session_filter()).unwrap().totals;
    assert_eq!(totals.requests, 1);
    assert_eq!(totals.tokens.output, Some(5));
}

#[test]
fn pending_usage_record_is_replayed_after_the_writer_recovers() {
    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(dir.path());
    let row = record(
        "f019-acde-1234",
        "proxy",
        Tokens {
            input: Some(11),
            output: Some(4),
            cache_read: Some(2),
            cache_write: Some(0),
            ..Tokens::default()
        },
    );
    // Drop the live connection to model a transient database outage. The
    // writer must reopen it and replay the durable pending record.
    service.0.store.lock().unwrap().take();
    service.0.persist_pending(&row).unwrap();
    let pending = dir
        .path()
        .join("usage")
        .join("pending")
        .join(format!("{}.json", row.id));

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(saved) = service.query(|store| store.detail(&row.id)) {
            assert_eq!(saved.tokens().input, Some(11));
            assert_eq!(saved.tokens().output, Some(4));
            if !pending.exists() {
                break;
            }
        }
        assert!(Instant::now() < deadline, "pending usage record was lost");
        thread::sleep(Duration::from_millis(50));
    }
}
