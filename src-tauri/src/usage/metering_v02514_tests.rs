use super::{model::*, pricing::Quote, store::Store};
use serde_json::json;

#[test]
fn relay_cache_reasoning_and_ttl_keep_reported_units() {
    let mut meter = Meter::default();
    meter.observe(&json!({"object":"chat.completion", "id":"fixture", "usage":{
        "prompt_tokens":100, "prompt_cache_hit_tokens":80, "prompt_cache_miss_tokens":20,
        "completion_tokens":40, "completion_tokens_details":{"reasoning_tokens":30,"cache_write_tokens":3}
    }}), 10);
    assert_eq!(meter.tokens.input, Some(20));
    assert_eq!(meter.tokens.reasoning, Some(30));
    assert_eq!(meter.tokens.total(), Some(140));
    meter.observe(&json!({"object":"chat.completion.chunk", "id":"fixture", "usage":{
        "prompt_tokens":0, "completion_tokens":0, "prompt_cache_hit_tokens":0, "reasoning_tokens":0
    }}), 20);
    assert_eq!(meter.tokens.input, Some(20));
    assert_eq!(meter.tokens.output, Some(40));
    assert_eq!(meter.tokens.reasoning, Some(30));
    meter.observe(
        &json!({"id":"fixture-next", "usage":{"input_tokens":0,"output_tokens":0}}),
        30,
    );
    assert_eq!(meter.tokens.total(), Some(0));
    assert_eq!(meter.tokens.reasoning, None);

    let mut relay = Meter::default();
    relay.observe(
        &json!({"object":"chat.completion.chunk","usage":{
            "prompt_tokens":2,"cache_read_input_tokens":20,"cache_creation_input_tokens":1,
            "cache_creation":{"ephemeral_5m_input_tokens":3,"ephemeral_1h_input_tokens":4}
        }}),
        1,
    );
    relay.observe(
        &json!({"object":"chat.completion.chunk","usage":{"completion_tokens":8}}),
        2,
    );
    assert_eq!(relay.tokens.input, Some(2));
    assert_eq!(relay.tokens.inclusive_input, None);
    assert_eq!(relay.tokens.cache_write, Some(1));
    assert!(relay.parse_incomplete);
    assert_eq!(relay.parse_reasons, ["cache_write_total_below_ttl_details"]);
    let mut mismatch = Meter::default();
    mismatch.observe(&json!({"usage":{"prompt_tokens":100,"prompt_cache_hit_tokens":80,"prompt_cache_miss_tokens":21}}), 1);
    assert!(mismatch.parse_incomplete);
    assert_eq!(mismatch.tokens.cache_read, Some(80));
    for key in ["input_tokens_details", "prompt_tokens_details"] {
        let usage = json!({"input_tokens":100, key:{"cache_write_tokens":20},"cached_tokens":10,"reasoning_tokens":3});
        let tokens = parse_tokens(&usage, false);
        assert_eq!(
            (
                tokens.input,
                tokens.cache_read,
                tokens.cache_write,
                tokens.reasoning
            ),
            (Some(70), Some(10), Some(20), Some(3))
        );
    }
    let mut late = Meter::default();
    late.observe(&json!({"type":"message_start","message":{"usage":{"input_tokens":2,"cache_creation_input_tokens":3,"output_tokens":0}}}),0);
    late.observe(
        &json!({"type":"message_delta","usage":{"cache_creation":{"ephemeral_5m_input_tokens":3}}}),
        1,
    );
    late.observe(&json!({"type":"message_delta","usage":{"cache_creation":{"ephemeral_1h_input_tokens":4},"output_tokens":5}}),2);
    assert_eq!(late.tokens.cache_write, Some(3));
    assert!(late.parse_incomplete);
}

#[test]
fn speed_confirmation_is_independent_of_tier_and_requires_fast_rates() {
    let mut a = Attempt {
        requested_speed: Some("fast".into()),
        service_tier: Some("priority".into()),
        tokens: Tokens {
            input: Some(10),
            output: Some(5),
            ..Tokens::default()
        },
        ..Attempt::default()
    };
    let quote = Quote {
        model: "fixture".into(),
        version: "fixture-v1".into(),
        source: "fixture".into(),
        multiplier: "1".into(),
        data: Some(
            json!({"input_cost_per_token_priority":"1","output_cost_per_token_priority":"2"}),
        ),
    };
    assert_eq!(
        quote.calculate(&a.tokens, a.price_tier()).unwrap().cost,
        "20"
    );
    a.response_speed = Some("standard".into());
    assert_eq!(a.price_tier(), Some("priority"));
    assert_eq!(
        quote.calculate(&a.tokens, a.price_tier()).unwrap().cost,
        "20"
    );
    let mut meter = Meter::default();
    meter.observe(&json!({"type":"message_start","message":{"usage":{"input_tokens":10,"output_tokens":0,"speed":"standard"}}}),0);
    a.output_reported = meter.output_reported;
    a.tokens = meter.tokens.clone();
    a.end = Some(EndMetadata {
        cause: EndCause::ClientDisconnected,
        terminal_received: false,
        response_started: true,
    });
    assert_eq!(a.interruption(), Some("partial"));
    meter.observe(
        &json!({"type":"message_delta","usage":{"output_tokens":0}}),
        1,
    );
    a.output_reported = meter.output_reported;
    assert_eq!(a.interruption(), Some("reported"));
    a.usage_status = "session_supplemented".into();
    a.parse_reasons.push("prompt_cache_total_mismatch".into());
    assert_eq!(a.interruption(), Some("partial"));
    assert_eq!(meter.response_speed.as_deref(), Some("standard"));
}

#[test]
fn reasoning_survives_original_multiple_attempts_and_reopen_without_schema_change() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let schema = || {
        let db = rusqlite::Connection::open(dir.path().join("usage.sqlite")).unwrap();
        let version: u32 = db
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        let mut stmt = db
            .prepare("SELECT name,sql FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap();
        let tables: Vec<(String, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        (version, tables)
    };
    let before = schema();
    assert_eq!(before.0, 13);
    let attempt = Attempt {
        id: "fixture-a".into(),
        tokens: Tokens {
            input: Some(10),
            output: Some(20),
            reasoning: Some(7),
            ..Tokens::default()
        },
        requested_speed: Some("fast".into()),
        response_speed: Some("standard".into()),
        raw_input_tokens: Some(10),
        input_basis: Some("exclusive".into()),
        parse_reasons: vec!["fixture_reason".into()],
        ..Attempt::default()
    };
    let mut second = attempt.clone();
    second.id = "fixture-b".into();
    second.tokens.reasoning = Some(9);
    let record = Record {
        id: "fixture-request".into(),
        client: "dsh".into(),
        source: "proxy".into(),
        started_at: now(),
        attempts: vec![attempt.clone(), second],
        gateway_reported: Some(attempt),
        ..Record::default()
    };
    super::storage_v12::save(&store.connection, &record, 1, 1).unwrap();
    drop(store);
    let reopened = Store::open(dir.path()).unwrap();
    let saved = reopened.detail("fixture-request").unwrap();
    assert_eq!(saved.client, "dsh");
    assert_eq!(
        saved
            .attempts
            .iter()
            .map(|a| a.tokens.reasoning)
            .collect::<Vec<_>>(),
        vec![Some(7), Some(9)]
    );
    let raw = saved.gateway_reported.unwrap();
    assert_eq!(raw.tokens.reasoning, Some(7));
    assert_eq!(raw.response_speed.as_deref(), Some("standard"));
    assert_eq!(raw.parse_reasons, ["fixture_reason"]);
    assert_eq!(schema(), before);
}

#[test]
fn interruption_statistics_count_logical_gateway_requests_and_unknowns() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let at = now();
    let reported = Attempt {
        id: "reported".into(),
        provider: Some("fixture-provider".into()),
        tokens: Tokens {
            input: Some(10),
            output: Some(5),
            ..Tokens::default()
        },
        output_reported: Some(true),
        end: Some(EndMetadata {
            cause: EndCause::ClientDisconnected,
            response_started: true,
            terminal_received: false,
        }),
        ..Attempt::default()
    };
    let partial = Attempt {
        output_reported: Some(false),
        tokens: Tokens {
            input: Some(10),
            output: Some(0),
            ..Tokens::default()
        },
        ..reported.clone()
    };
    for (i, (source, attempts)) in [
        ("proxy", vec![reported.clone(), reported]),
        ("proxy", vec![partial]),
        ("proxy", vec![Attempt::default()]),
        ("proxy", vec![]),
        ("claude", vec![Attempt::default()]),
    ]
    .into_iter()
    .enumerate()
    {
        let row = Record {
            id: format!("fixture-request-{i}"),
            client: "claude".into(),
            source: source.into(),
            started_at: at,
            finished_at: Some(at),
            attempts,
            ..Record::default()
        };
        super::storage_v12::save(&store.connection, &row, 1, i as i64 + 1).unwrap();
    }
    let summary = store
        .interruptions(&Filter {
            start: Some(at - 1000),
            end: Some(at + 1000),
            ..Filter::default()
        })
        .unwrap();
    assert_eq!(
        (
            summary.requests,
            summary.reported,
            summary.partial,
            summary.unknown
        ),
        (4, 1, 1, 2)
    );
    assert_eq!(summary.groups.iter().map(|g| g.reported).sum::<u64>(), 1);
    assert_eq!(summary.groups.iter().map(|g| g.unknown).sum::<u64>(), 2);
}

#[test]
fn reasoning_supplement_conflicts_and_cold_details_preserve_originals() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let at = now();
    let mut gateway = Record {
        id: "fixture-gateway".into(),
        client: "codex".into(),
        source: "proxy".into(),
        started_at: at - 8 * 86_400_000,
        completed: true,
        attempts: vec![Attempt {
            id: "fixture-attempt".into(),
            response_id: Some("fixture-response".into()),
            requested_model: Some("fixture-model".into()),
            response_model: Some("fixture-model".into()),
            pricing_model: Some("fixture-model".into()),
            tokens: Tokens {
                input: Some(10),
                output: Some(20),
                ..Tokens::default()
            },
            outcome: "success".into(),
            transport: "http".into(),
            status: Some(200),
            thread_hash: Some("fixture-thread-hash".into()),
            price: Some(PriceSnapshot {
                source: "fixture".into(),
                version: "fixture-v1".into(),
                model: "fixture-model".into(),
                multiplier: "1".into(),
                cost: "0.00005".into(),
                rates: Default::default(),
                basis: None,
            }),
            ..Attempt::default()
        }],
        ..Record::default()
    };
    gateway.attempts[0].started_at = gateway.started_at;
    let mut donor = gateway.clone();
    donor.id = "fixture-session".into();
    donor.source = "codex".into();
    donor.attempts[0].transport = "session".into();
    donor.attempts[0].tokens.reasoning = Some(7);
    store
        .write_batch(&[gateway.clone(), donor.clone()], None)
        .unwrap();
    let merged = store.detail(&gateway.id).unwrap();
    assert_eq!(merged.attempts[0].tokens.reasoning, Some(7));
    assert_eq!(
        merged.gateway_reported.as_ref().unwrap().tokens.reasoning,
        None
    );
    assert_eq!(
        merged.attempts[0]
            .usage_sources
            .get("reasoning")
            .map(String::as_str),
        Some("session")
    );
    assert_eq!(merged.attempts[0].price.as_ref().unwrap().cost, "0.00005");
    let mut conflict = donor.clone();
    conflict.id = "fixture-conflict".into();
    conflict.source = "claude".into();
    conflict.attempts[0].tokens.reasoning = Some(9);
    store.write_batch(&[conflict], None).unwrap();
    let conflicted = store.detail(&gateway.id).unwrap();
    assert_eq!(conflicted.attempts[0].tokens.reasoning, None);
    gateway.id = "fixture-cold".into();
    gateway.attempts[0].response_id = Some("fixture-cold-response".into());
    gateway.attempts[0].tokens.reasoning = Some(11);
    gateway.attempts[0].requested_speed = Some("fast".into());
    gateway.attempts[0].response_speed = Some("standard".into());
    store.write_batch(&[gateway.clone()], None).unwrap();
    store.compact(at).unwrap();
    drop(store);
    let store = Store::open(dir.path()).unwrap();
    let cold = store.detail(&gateway.id).unwrap();
    assert!(cold.detail_archived);
    assert_eq!(cold.attempts[0].tokens.reasoning, Some(11));
    assert_eq!(cold.attempts[0].response_speed.as_deref(), Some("standard"));
    assert_eq!(cold.attempts[0].price.as_ref().unwrap().cost, "0.00005");
    let mut original = store.detail("fixture-gateway").unwrap();
    original.gateway_only();
    assert_eq!(original.attempts[0].tokens.reasoning, None);
}
