use super::{model::*, store::Store, Service};
use rusqlite::{params, Connection};
use serde_json::{json, Value};

const DAY: i64 = 86_400_000;
const BASE: i64 = 20_000 * DAY;

fn record(id: &str, first_token_ms: Option<u64>) -> Record {
    Record {
        id: id.into(),
        client: "codex".into(),
        source: "proxy".into(),
        started_at: BASE,
        completed: true,
        attempts: vec![Attempt {
            id: format!("{id}-attempt"),
            provider: Some("fixture-provider".into()),
            requested_model: Some("fixture-model".into()),
            response_model: Some("fixture-model".into()),
            pricing_model: Some("fixture-model".into()),
            response_id: Some(format!("{id}-response")),
            status: Some(200),
            outcome: "success".into(),
            started_at: BASE,
            duration_ms: 1_000,
            first_token_ms,
            stream: true,
            transport: "http".into(),
            tokens: Tokens {
                input: Some(10),
                output: Some(2),
                ..Tokens::default()
            },
            price: Some(PriceSnapshot {
                version: "fixture-price-v1".into(),
                source: "fixed".into(),
                model: "fixture-model".into(),
                multiplier: "1".into(),
                rates: [
                    ("input".into(), "0.1".into()),
                    ("output".into(), "0.2".into()),
                ]
                .into_iter()
                .collect(),
                cost: "1.4".into(),
                basis: Some(json!({
                    "input_cost_per_token": "0.1",
                    "output_cost_per_token": "0.2"
                })),
            }),
            ..Attempt::default()
        }],
        ..Record::default()
    }
}

fn filter(source: Option<&str>) -> Filter {
    Filter {
        source: source.map(str::to_owned),
        ..Filter::default()
    }
}

fn assert_first_token(totals: &Totals, sum: u64, samples: u64) {
    assert_eq!(totals.first_token_sum_ms, sum, "first-token sum");
    assert_eq!(totals.first_token_samples, samples, "first-token samples");
}

#[test]
fn meter_waits_through_empty_deltas_roles_heartbeats_errors_and_full_responses() {
    let ignored = [
        json!({"type": "response.created"}),
        json!({"type": "response.in_progress"}),
        json!({"type": "response.output_text.delta", "delta": ""}),
        json!({"type": "response.output_text.delta", "delta": null}),
        json!({"type": "response.reasoning_text.delta", "delta": ""}),
        json!({"type": "response.reasoning_summary_text.delta", "delta": ""}),
        json!({"type": "response.function_call_arguments.delta", "delta": ""}),
        json!({"type": "content_block_start", "content_block": {"type": "text", "text": ""}}),
        json!({"type": "content_block_delta", "delta": {"type": "text_delta", "text": ""}}),
        json!({"type": "content_block_delta", "delta": {"type": "thinking_delta", "thinking": ""}}),
        json!({"type": "content_block_delta", "delta": {"type": "input_json_delta", "partial_json": ""}}),
        json!({"type": "content_block_delta", "delta": {"type": "signature_delta", "signature": "fixture-signature"}}),
        json!({"type": "content_block_delta", "delta": {}}),
        json!({"type": "ping"}),
        json!({"type": "heartbeat"}),
        json!({"type": "error", "error": {"type": "fixture_error", "message": "fixture.example.invalid"}}),
        json!({"type": "response.failed", "response": {"error": {"code": "fixture_error"}}}),
        json!({"choices": [{"delta": {"role": "assistant"}}]}),
        json!({"choices": [{"delta": {"content": ""}}]}),
        json!({"choices": [{"delta": {"content": null}}]}),
        json!({"choices": [{"delta": {"reasoning_content": "", "reasoning": ""}}]}),
        json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "function": {"name": "fixture_tool", "arguments": ""}}]}}]}),
        json!({"choices": [{"delta": {"function_call": {"name": "fixture_tool", "arguments": ""}}}]}),
        json!({"choices": [{"delta": {}, "finish_reason": "stop"}]}),
        json!({"choices": [{"delta": {"content": "fixture"}}], "error": {"message": "fixture.example.invalid"}}),
        json!({"choices": [{"message": {"content": "fixture.example.invalid"}}], "usage": {"completion_tokens": 2}}),
        json!({"type": "response.completed", "response": {"output": [{"content": [{"type": "output_text", "text": "fixture.example.invalid"}]}], "usage": {"input_tokens": 10, "output_tokens": 2}}}),
    ];
    let mut meter = Meter::default();
    for (index, event) in ignored.iter().enumerate() {
        meter.observe(event, index as u64);
        assert_eq!(meter.first_token_ms, None, "{event}");
    }
    assert_eq!(meter.tokens.input, Some(10));
    assert_eq!(meter.tokens.output, Some(2));
    meter.observe(
        &json!({"type": "response.output_text.delta", "delta": "fixture.example.invalid"}),
        80,
    );
    assert_eq!(meter.first_token_ms, Some(80));
}

#[test]
fn meter_recognizes_nonempty_text_reasoning_and_tool_arguments_for_each_protocol() {
    let generated = [
        json!({"type": "response.output_text.delta", "delta": "fixture.example.invalid"}),
        json!({"type": "response.reasoning_text.delta", "delta": "fixture"}),
        json!({"type": "response.reasoning_summary_text.delta", "delta": "fixture"}),
        json!({"type": "response.function_call_arguments.delta", "delta": "{"}),
        json!({"type": "content_block_delta", "delta": {"type": "text_delta", "text": "fixture"}}),
        json!({"type": "content_block_delta", "delta": {"type": "thinking_delta", "thinking": "fixture"}}),
        json!({"type": "content_block_delta", "delta": {"type": "input_json_delta", "partial_json": "{"}}),
        json!({"choices": [{"delta": {"content": "fixture"}}]}),
        json!({"choices": [{"delta": {"reasoning_content": "fixture"}}]}),
        json!({"choices": [{"delta": {"reasoning": "fixture"}}]}),
        json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "function": {"arguments": "{"}}]}}]}),
        json!({"choices": [{"delta": {"function_call": {"arguments": "{"}}}]}),
        json!({"choices": [{"delta": {"role": "assistant"}}, {"delta": {"content": "fixture"}}]}),
        json!({"choices": [{"delta": {"tool_calls": [{"function": {"arguments": ""}}, {"function": {"arguments": "{"}}]}}]}),
        json!({"type": "response.output_text.delta", "delta": " "}),
    ];
    for event in generated {
        let mut meter = Meter::default();
        meter.observe(&event, 37);
        assert_eq!(meter.first_token_ms, Some(37), "{event}");
        meter.observe(&event, 91);
        meter.observe(&json!({"type": "ping"}), 100);
        assert_eq!(meter.first_token_ms, Some(37), "replayed {event}");
    }
}

#[test]
fn zero_millisecond_first_delta_is_a_sample_and_is_not_replaced_by_replays() {
    let mut meter = Meter::default();
    let event = json!({"type": "response.function_call_arguments.delta", "delta": "{"});
    meter.observe(&event, 0);
    meter.observe(&event, 500);
    meter.observe(&json!({"type": "response.completed"}), 700);
    assert_eq!(meter.first_token_ms, Some(0));

    let mut totals = Totals::default();
    totals.add_record(&record("fixture-zero", meter.first_token_ms));
    totals.add_record(&record("fixture-unknown", None));
    assert_first_token(&totals, 0, 1);
    assert_eq!(totals.requests, 2);
}

fn weighted_records() -> Vec<Record> {
    let mut retried = record("fixture-retried", Some(40));
    let mut prior = retried.attempts[0].clone();
    prior.id = "fixture-prior-attempt".into();
    prior.provider = Some("fixture-prior-provider".into());
    prior.pricing_model = Some("fixture-prior-model".into());
    prior.first_token_ms = Some(900);
    prior.duration_ms = 5_000;
    prior.status = Some(503);
    prior.outcome = "failed".into();
    prior.repeat_count = 4;
    // The request includes earlier attempts and waiting before this final send.
    retried.attempts[0].started_at += 60_000;
    retried.attempts.insert(0, prior);

    let zero = record("fixture-weighted-zero", Some(0));
    let missing = record("fixture-weighted-missing", None);
    let mut slow = record("fixture-weighted-slow", Some(260));
    slow.attempts[0].provider = Some("fixture-other-provider".into());
    slow.attempts[0].pricing_model = Some("fixture-other-model".into());
    vec![retried, zero, missing, slow]
}

#[test]
fn totals_weight_final_attempt_samples_without_wait_time_or_prior_attempts() {
    let records = weighted_records();
    let mut first_group = Totals::default();
    for record in &records[..3] {
        first_group.add_record(record);
    }
    assert_first_token(&first_group, 40, 2);
    let mut second_group = Totals::default();
    second_group.add_record(&records[3]);
    assert_first_token(&second_group, 260, 1);

    first_group.add(&second_group);
    assert_first_token(&first_group, 300, 3);
    assert_eq!(
        first_group.first_token_sum_ms / first_group.first_token_samples,
        100
    );
    assert_eq!((first_group.requests, first_group.attempts), (4, 8));
    assert_eq!(first_group.tokens.input, Some(50));
    assert_eq!(first_group.tokens.output, Some(10));
    assert_eq!(decimal(&first_group.cost), decimal("7"));
}

#[test]
fn session_nonstream_and_search_records_do_not_contribute_first_token_samples() {
    let mut rows = Vec::new();
    for source in ["codex", "claude"] {
        let mut local = record(&format!("fixture-{source}-session"), Some(100));
        local.source = source.into();
        local.client = source.into();
        local.attempts[0].provider = None;
        local.attempts[0].status = None;
        local.attempts[0].transport = "session".into();
        // Even legacy session metadata must not be interpreted as gateway timing.
        rows.push(local);
    }
    let mut nonstream = record("fixture-nonstream", Some(200));
    nonstream.attempts[0].stream = false;
    rows.push(nonstream);
    let mut search = record("fixture-search", Some(300));
    search.attempts[0].operation = Operation::WebSearch;
    rows.push(search);

    let mut totals = Totals::default();
    for record in &rows {
        totals.add_record(record);
    }
    assert_first_token(&totals, 0, 0);
    assert_eq!((totals.requests, totals.attempts), (4, 4));
    assert_eq!(decimal(&totals.cost), decimal("5.6"));

    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    store.write_batch(&rows, None).unwrap();
    for source in [None, Some("proxy"), Some("sessions")] {
        assert_first_token(&store.dashboard(&filter(source)).unwrap().totals, 0, 0);
    }
}

#[test]
fn cancellation_after_first_delta_keeps_the_sample_and_pre_delta_error_has_none() {
    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(dir.path());
    let trace = service.begin("codex", Some("fixture-model"));
    let mut attempt = trace.attempt("fixture-provider", true, "http");
    let mut meter = Meter::default();
    meter.observe(
        &json!({"type": "response.output_text.delta", "delta": "fixture.example.invalid"}),
        0,
    );
    attempt.update(&meter, Some(200), None);
    attempt.update(&Meter::default(), None, Some("cancelled"));
    assert_eq!(attempt.attempt.first_token_ms, Some(0));
    assert_eq!(attempt.attempt.outcome, "cancelled");
    let cancelled = Record {
        completed: false,
        attempts: vec![attempt.attempt.clone()],
        ..record("fixture-cancelled", None)
    };
    let mut totals = Totals::default();
    totals.add_record(&cancelled);
    assert_first_token(&totals, 0, 1);

    let failed_trace = service.begin("codex", Some("fixture-model"));
    let mut failed = failed_trace.attempt("fixture-provider", true, "http");
    let mut error_meter = Meter::default();
    error_meter.observe(
        &json!({"type": "error", "error": {"message": "fixture.example.invalid"}}),
        50,
    );
    failed.update(&error_meter, Some(503), Some("failed"));
    assert_eq!(failed.attempt.first_token_ms, None);
    let failed_record = Record {
        completed: false,
        attempts: vec![failed.attempt.clone()],
        ..record("fixture-failed-before-delta", None)
    };
    totals.add_record(&failed_record);
    assert_first_token(&totals, 0, 1);
    assert_eq!(totals.requests, 2);

    let mut store = Store::open(&dir.path().join("fixture-cancellation-projection")).unwrap();
    store
        .write_batch(&[cancelled, failed_record], None)
        .unwrap();
    let totals = store.dashboard(&Filter::default()).unwrap().totals;
    assert_first_token(&totals, 0, 1);
    assert_eq!(totals.requests, 2);
}

fn assert_weighted_dashboard(store: &Store) {
    let dashboard = store.dashboard(&Filter::default()).unwrap();
    assert_first_token(&dashboard.totals, 300, 3);
    assert_eq!(
        (dashboard.totals.requests, dashboard.totals.attempts),
        (4, 8)
    );
    assert_eq!(decimal(&dashboard.totals.cost), decimal("7"));
    assert_eq!(dashboard.totals.tokens.input, Some(50));
    assert_eq!(dashboard.totals.tokens.output, Some(10));

    for (provider, model, sum, samples, requests, attempts) in [
        ("fixture-provider", "fixture-model", 40, 2, 3, 3),
        (
            "fixture-other-provider",
            "fixture-other-model",
            260,
            1,
            1,
            1,
        ),
        ("fixture-prior-provider", "fixture-prior-model", 0, 0, 0, 4),
    ] {
        for (groups, id) in [(&dashboard.providers, provider), (&dashboard.models, model)] {
            let totals = &groups.iter().find(|group| group.id == id).unwrap().totals;
            assert_first_token(totals, sum, samples);
            assert_eq!(
                (totals.requests, totals.attempts),
                (requests, attempts),
                "{id}"
            );
        }
    }
}

#[test]
fn sql_groups_keep_final_attempt_weights_after_compaction() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    store.write_batch(&weighted_records(), None).unwrap();
    assert_weighted_dashboard(&store);
    store.compact(BASE + 40 * DAY).unwrap();
    assert_weighted_dashboard(&store);
    assert_eq!(store.logs(&Filter::default()).unwrap().total, 0);
    drop(store);
    assert_weighted_dashboard(&Store::open(dir.path()).unwrap());
}

#[test]
fn source_filters_never_borrow_gateway_timing_for_deduplicated_sessions() {
    for session_first in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        let mut gateway = record("fixture-merged-gateway", Some(80));
        gateway.attempts[0].tokens = Tokens::default();
        gateway.attempts[0].price = None;
        let mut session = record("fixture-session-a", None);
        session.source = "codex".into();
        session.attempts[0].provider = None;
        session.attempts[0].status = None;
        session.attempts[0].stream = false;
        session.attempts[0].transport = "session".into();
        session.attempts[0].response_id = gateway.attempts[0].response_id.clone();
        let mut replay = session.clone();
        replay.id = "fixture-session-b".into();
        replay.started_at += 1;
        let independent = record("fixture-independent-gateway", Some(0));
        let mut claude = record("fixture-independent-claude", None);
        claude.client = "claude".into();
        claude.source = "claude".into();
        claude.attempts[0].provider = None;
        claude.attempts[0].status = None;
        claude.attempts[0].stream = false;
        claude.attempts[0].transport = "session".into();

        let mut rows = vec![gateway.clone(), session, replay, independent, claude];
        if session_first {
            rows.swap(0, 1);
        }
        store.write_batch(&rows, None).unwrap();
        // Replaying the same persisted records also leaves the denominator intact.
        store.write_batch(&rows, None).unwrap();
        let merged = store.detail(&gateway.id).unwrap();
        assert!(merged.gateway_reported.is_some());
        assert_eq!(merged.final_attempt().unwrap().first_token_ms, Some(80));
        assert_eq!(merged.tokens().input, Some(10));

        for compact in [false, true] {
            if compact {
                store.compact(BASE + 40 * DAY).unwrap();
            }
            for (source, requests, sum, samples, cost, unpriced) in [
                (None, 3, 80, 2, "4.2", 0),
                (Some("proxy"), 2, 80, 2, "1.4", 1),
                (Some("sessions"), 2, 0, 0, "2.8", 0),
            ] {
                let dashboard = store.dashboard(&filter(source)).unwrap();
                assert_first_token(&dashboard.totals, sum, samples);
                assert_eq!(dashboard.totals.requests, requests, "{source:?}");
                assert_eq!(dashboard.totals.unpriced, unpriced, "{source:?}");
                assert_eq!(decimal(&dashboard.totals.cost), decimal(cost), "{source:?}");
            }
        }
    }
}

#[test]
fn daily_rollup_stores_sum_and_count_and_keeps_unknown_samples_out_of_the_divisor() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    store
        .write_batch(
            &[
                record("fixture-daily-zero", Some(0)),
                record("fixture-daily-known", Some(30)),
                record("fixture-daily-unknown", None),
            ],
            None,
        )
        .unwrap();
    store.compact(BASE + 40 * DAY).unwrap();
    assert_first_token(&store.dashboard(&Filter::default()).unwrap().totals, 30, 2);
    let database = Connection::open(dir.path().join("usage.sqlite")).unwrap();
    let (count, body): (u64, String) = database
        .query_row(
            "SELECT count(*),body FROM daily WHERE json_extract(body,'$.scope')='all'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(count, 1);
    let daily: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(daily["totals"]["requests"], 3);
    assert_eq!(daily["totals"]["firstTokenSumMs"], 30);
    assert_eq!(daily["totals"]["firstTokenSamples"], 2);
    assert!(daily["example"]["attempts"][0]["firstTokenMs"].is_null());
    drop(database);

    store
        .write_batch(&[record("fixture-daily-late", Some(70))], None)
        .unwrap();
    store.compact(BASE + 40 * DAY).unwrap();
    drop(store);
    let store = Store::open(dir.path()).unwrap();
    let totals = store.dashboard(&Filter::default()).unwrap().totals;
    assert_first_token(&totals, 100, 3);
    assert_eq!((totals.requests, totals.attempts), (4, 4));
    assert_eq!(decimal(&totals.cost), decimal("5.6"));
    assert_eq!(totals.tokens.input, Some(40));
    assert_eq!(totals.tokens.output, Some(8));
}

const V5_METRIC_COLUMNS: &str = "kind,owner,ordinal,provider,model,requests,attempts,success,status_known,sessions,input,output,cache_read,cache_write,cache_write_5m,cache_write_1h,image_input,image_output,audio_input,audio_output,cost,unpriced,duration_ms,measured_outputs,generation_ms";

fn downgrade_fixture_metrics_to_v5(database: &Connection) {
    database
        .execute_batch(&format!(
            "ALTER TABLE usage_metrics RENAME TO fixture_v018_metrics;
             CREATE TABLE usage_metrics(
               kind INTEGER NOT NULL,owner TEXT NOT NULL,ordinal INTEGER NOT NULL,
               provider TEXT,model TEXT,requests INTEGER NOT NULL,attempts INTEGER NOT NULL,
               success INTEGER NOT NULL,status_known INTEGER NOT NULL,sessions INTEGER NOT NULL,
               input INTEGER,output INTEGER,cache_read INTEGER,cache_write INTEGER,
               cache_write_5m INTEGER,cache_write_1h INTEGER,image_input INTEGER,image_output INTEGER,audio_input INTEGER,audio_output INTEGER,
               cost TEXT NOT NULL,unpriced INTEGER NOT NULL,duration_ms INTEGER NOT NULL,measured_outputs INTEGER NOT NULL,generation_ms INTEGER NOT NULL,
               PRIMARY KEY(kind,owner,ordinal));
             INSERT INTO usage_metrics({V5_METRIC_COLUMNS}) SELECT {V5_METRIC_COLUMNS} FROM fixture_v018_metrics;
             DROP TABLE fixture_v018_metrics;
             PRAGMA user_version=5;"
        ))
        .unwrap();
}

#[test]
fn v5_migration_backfills_details_but_never_invents_first_token_samples_for_old_daily_rows() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let mut final_attempt = record("fixture-migration-final", Some(40));
    let mut prior = final_attempt.attempts[0].clone();
    prior.id = "fixture-migration-prior".into();
    prior.first_token_ms = Some(900);
    final_attempt.attempts.insert(0, prior);
    let mut nonstream = record("fixture-migration-nonstream", Some(80));
    nonstream.attempts[0].stream = false;
    store
        .write_batch(
            &[
                final_attempt,
                record("fixture-migration-zero", Some(0)),
                nonstream,
            ],
            None,
        )
        .unwrap();
    drop(store);

    let database = Connection::open(dir.path().join("usage.sqlite")).unwrap();
    downgrade_fixture_metrics_to_v5(&database);
    let mut example = record("fixture-legacy-example", Some(10));
    example.started_at = super::query::bucket(BASE - 2 * DAY, DAY);
    example.attempts[0].started_at = example.started_at;
    let mut legacy_totals = Totals::default();
    legacy_totals.add_record(&example);
    legacy_totals.add_record(&example);
    let mut legacy_totals = serde_json::to_value(legacy_totals).unwrap();
    legacy_totals
        .as_object_mut()
        .unwrap()
        .remove("firstTokenSumMs");
    legacy_totals
        .as_object_mut()
        .unwrap()
        .remove("firstTokenSamples");
    // A representative example and old duration/throughput fields cannot recover
    // the number or sum of real first-token observations in historical requests.
    let daily = json!({
        "scope": "all",
        "client": "codex",
        "provider": "fixture-provider",
        "model": "fixture-model",
        "source": "proxy",
        "status": 200,
        "totals": legacy_totals,
        "example": example
    });
    database
        .execute(
            "INSERT INTO daily(id,day,body) VALUES(?1,?2,?3)",
            params![
                "fixture-legacy-daily",
                example.started_at,
                serde_json::to_string(&daily).unwrap()
            ],
        )
        .unwrap();
    drop(database);

    let store = Store::open(dir.path()).unwrap();
    let totals = store.dashboard(&Filter::default()).unwrap().totals;
    assert_first_token(&totals, 40, 2);
    assert_eq!((totals.requests, totals.attempts), (5, 6));
    assert_eq!(decimal(&totals.cost), decimal("8.4"));
    assert_eq!(totals.tokens.input, Some(60));
    assert_eq!(totals.tokens.output, Some(12));
    let historical = store
        .dashboard(&Filter {
            end: Some(BASE - 1),
            ..Filter::default()
        })
        .unwrap();
    assert_first_token(&historical.totals, 0, 0);
    assert_eq!(historical.totals.requests, 2);
    assert_eq!(decimal(&historical.totals.cost), decimal("2.8"));
    drop(store);

    let database = Connection::open(dir.path().join("usage.sqlite")).unwrap();
    let version: i64 = database
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, 7);
    drop(database);
    let totals = Store::open(dir.path())
        .unwrap()
        .dashboard(&Filter::default())
        .unwrap()
        .totals;
    assert_first_token(&totals, 40, 2);
    assert_eq!((totals.requests, totals.attempts), (5, 6));
}

#[test]
fn legacy_totals_default_to_no_samples_and_new_json_exposes_sum_and_count() {
    let mut encoded = serde_json::to_value(Totals::default()).unwrap();
    encoded.as_object_mut().unwrap().remove("firstTokenSumMs");
    encoded.as_object_mut().unwrap().remove("firstTokenSamples");
    let legacy: Totals = serde_json::from_value(encoded).unwrap();
    assert_first_token(&legacy, 0, 0);
    let mut measured = legacy;
    measured.add_record(&record("fixture-json-zero", Some(0)));
    measured.add_record(&record("fixture-json-known", Some(75)));
    let encoded = serde_json::to_value(measured).unwrap();
    assert_eq!(encoded["firstTokenSumMs"], 75);
    assert_eq!(encoded["firstTokenSamples"], 2);
}
