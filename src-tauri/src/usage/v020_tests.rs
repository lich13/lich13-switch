//! v0.20 usage classification, metadata repair, and exact-response merge checks.
use super::{
    model::{safe_id, Attempt, Meter, PriceSnapshot, Record, Tokens},
    store::Store,
};
use serde_json::json;
use std::collections::BTreeMap;

const BASE: i64 = 40_000 * 86_400_000;

fn price(cost: &str) -> PriceSnapshot {
    PriceSnapshot {
        version: "fixture-price-v020".into(),
        source: "fixture-catalog".into(),
        model: "fixture-model".into(),
        multiplier: "1".into(),
        rates: BTreeMap::from([("input".into(), "0.00001".into())]),
        cost: cost.into(),
        basis: Some(json!({"input_cost_per_token":"0.00001"})),
    }
}

fn attempt(id: &str, source: &str, tokens: Tokens) -> Attempt {
    Attempt {
        id: format!("{id}-attempt"),
        provider: (source == "proxy").then(|| "fixture-provider".into()),
        requested_model: Some("fixture-model".into()),
        response_model: Some("fixture-model".into()),
        pricing_model: Some("fixture-model".into()),
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

fn record(id: &str, source: &str, tokens: Tokens) -> Record {
    Record {
        id: id.into(),
        client: "codex".into(),
        source: source.into(),
        started_at: BASE,
        completed: source == "proxy",
        attempts: vec![attempt(id, source, tokens)],
        ..Record::default()
    }
}

fn session(mut record: Record, response_id: &str) -> Record {
    record.completed = false;
    record.attempts[0].response_id = Some(response_id.into());
    record.attempts[0].provider = None;
    record.attempts[0].status = None;
    record.attempts[0].stream = false;
    record.attempts[0].transport = "session".into();
    record
}

#[test]
fn inclusive_input_makes_cache_sample_valid_without_cache_write() {
    let tokens = Tokens {
        inclusive_input: Some(100),
        input: Some(20),
        cache_read: Some(80),
        cache_write: None,
        ..Tokens::default()
    };
    assert_eq!(tokens.cache_sample(), Some((80, 100)));
    assert_eq!(tokens.total(), Some(100));
}

#[test]
fn cache_sample_remains_unknown_when_cache_read_or_input_total_is_missing() {
    assert_eq!(
        Tokens {
            input: Some(20),
            cache_read: None,
            ..Tokens::default()
        }
        .cache_sample(),
        None
    );
    assert_eq!(
        Tokens {
            cache_read: Some(80),
            cache_write: None,
            ..Tokens::default()
        }
        .cache_sample(),
        None
    );
}

#[test]
fn parse_tokens_keeps_openai_cache_write_unknown_and_keeps_claude_categories_exclusive() {
    let openai = super::model::parse_tokens(
        &json!({"input_tokens":100,"input_tokens_details":{"cached_tokens":80}}),
        false,
    );
    assert_eq!(openai.inclusive_input, Some(100));
    assert_eq!(openai.input, Some(20));
    assert_eq!(openai.cache_read, Some(80));
    assert_eq!(openai.cache_write, None);

    let claude = super::model::parse_tokens(
        &json!({"input_tokens":100,"cache_read_input_tokens":80}),
        true,
    );
    assert_eq!(claude.inclusive_input, None);
    assert_eq!(claude.input, Some(100));
    assert_eq!(claude.cache_read, Some(80));
}

#[test]
fn meter_recomputes_fresh_input_when_cache_details_arrive_after_inclusive_total() {
    let mut meter = Meter::default();
    meter.observe(&json!({"usage":{"input_tokens":100,"output_tokens":4}}), 1);
    meter.observe(
        &json!({"type":"response.usage","usage":{"input_tokens_details":{"cached_tokens":80}}}),
        2,
    );
    assert_eq!(meter.inclusive_input, Some(100));
    assert_eq!(meter.tokens.input, Some(20));
    assert_eq!(meter.tokens.cache_read, Some(80));
    assert_eq!(meter.tokens.cache_write, None);
}

#[test]
fn availability_distinguishes_reported_missing_and_non_applicable_fields() {
    let mut reported = attempt(
        "fixture-availability",
        "proxy",
        Tokens {
            input: Some(20),
            output: Some(4),
            cache_read: Some(80),
            ..Tokens::default()
        },
    );
    reported.stream = true;
    reported.first_token_ms = Some(12);
    reported.usage_status = "reported".into();
    reported.annotate_availability("proxy");
    assert_eq!(reported.availability["input"], "reported");
    assert_eq!(reported.availability["cache_write"], "upstream_unreported");
    assert_eq!(reported.availability["first_token"], "reported");

    let mut session_attempt = attempt("fixture-session", "codex", Tokens::default());
    session_attempt.usage_status = "ended_early".into();
    session_attempt.annotate_availability("codex");
    assert_eq!(session_attempt.availability["input"], "ended_early");
    assert_eq!(
        session_attempt.availability["first_token"],
        "not_applicable"
    );

    let mut historical = attempt("fixture-historical", "codex", Tokens::default());
    historical.annotate_availability("codex");
    assert_eq!(historical.availability["input"], "historical_missing");
    assert_eq!(historical.availability["cache_read"], "historical_missing");
}

#[test]
fn metadata_repair_processes_at_most_64_rows_and_clears_marker_after_restartable_batches() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let rows: Vec<_> = (0..65)
        .map(|i| {
            let mut row = record(
                &format!("fixture-row-{i:03}"),
                "proxy",
                Tokens {
                    input: Some(1),
                    output: Some(1),
                    ..Tokens::default()
                },
            );
            row.attempts[0].availability.clear();
            row
        })
        .collect();
    store.write_batch(&rows, None).unwrap();
    assert!(store.repair_metadata_batch().unwrap());
    assert!(store.repair_metadata_batch().unwrap());
    assert!(!store.repair_metadata_batch().unwrap());
    assert_eq!(
        store
            .detail("fixture-row-000")
            .unwrap()
            .final_attempt()
            .unwrap()
            .availability["input"],
        "reported"
    );
    assert_eq!(
        store
            .detail("fixture-row-064")
            .unwrap()
            .final_attempt()
            .unwrap()
            .availability["output"],
        "reported"
    );
}

#[test]
fn exact_response_donors_fill_agreeing_fields_and_preserve_gateway_price() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let response = safe_id("fixture-donor-agree");
    let mut gateway = record(
        "fixture-gateway-agree",
        "proxy",
        Tokens {
            input: Some(20),
            output: None,
            cache_read: None,
            cache_write: None,
            inclusive_input: Some(100),
            ..Tokens::default()
        },
    );
    gateway.attempts[0].response_id = Some(response.clone());
    gateway.attempts[0].price = Some(price("0.00042"));

    let mut donor_a = session(
        record(
            "fixture-session-a",
            "codex",
            Tokens {
                input: Some(20),
                output: Some(7),
                cache_read: Some(80),
                ..Tokens::default()
            },
        ),
        &response,
    );
    donor_a.attempts[0].price = Some(price("0.00081"));
    let donor_b = session(
        record(
            "fixture-session-b",
            "codex",
            Tokens {
                input: Some(20),
                output: Some(7),
                cache_read: Some(80),
                ..Tokens::default()
            },
        ),
        &response,
    );
    store
        .write_batch(&[gateway.clone(), donor_a, donor_b], None)
        .unwrap();
    let merged = store.detail(&gateway.id).unwrap();
    let attempt = merged.final_attempt().unwrap();
    assert_eq!(attempt.tokens.output, Some(7));
    assert_eq!(attempt.tokens.cache_read, Some(80));
    assert_eq!(attempt.tokens.cache_write, None);
    assert_eq!(attempt.price.as_ref().unwrap().cost, "0.00042");
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
}

#[test]
fn exact_response_donors_do_not_fill_a_field_when_sources_conflict() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let response = safe_id("fixture-donor-conflict");
    let mut gateway = record(
        "fixture-gateway-conflict",
        "proxy",
        Tokens {
            input: Some(20),
            output: None,
            cache_read: None,
            cache_write: None,
            ..Tokens::default()
        },
    );
    gateway.attempts[0].response_id = Some(response.clone());
    let donor_a = session(
        record(
            "fixture-session-conflict-a",
            "codex",
            Tokens {
                input: Some(20),
                output: Some(7),
                cache_read: Some(80),
                ..Tokens::default()
            },
        ),
        &response,
    );
    let donor_b = session(
        record(
            "fixture-session-conflict-b",
            "codex",
            Tokens {
                input: Some(20),
                output: Some(8),
                cache_read: Some(80),
                ..Tokens::default()
            },
        ),
        &response,
    );
    store
        .write_batch(&[gateway.clone(), donor_a, donor_b], None)
        .unwrap();
    let merged = store.detail(&gateway.id).unwrap();
    assert_eq!(merged.final_attempt().unwrap().tokens.output, None);
    assert_eq!(merged.final_attempt().unwrap().tokens.cache_read, Some(80));
}

#[test]
fn exact_response_donor_never_overwrites_a_real_zero_input() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let response = safe_id("fixture-zero-input");
    let mut gateway = record(
        "fixture-zero-gateway",
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
    let donor = session(
        record(
            "fixture-zero-session",
            "codex",
            Tokens {
                input: Some(100),
                output: Some(7),
                cache_read: Some(20),
                cache_write: Some(3),
                ..Tokens::default()
            },
        ),
        &response,
    );
    store.write_batch(&[gateway.clone(), donor], None).unwrap();
    let merged = store.detail(&gateway.id).unwrap();
    assert_eq!(merged.final_attempt().unwrap().tokens.input, Some(0));
    assert_eq!(merged.final_attempt().unwrap().tokens.cache_read, Some(0));
    assert_eq!(merged.final_attempt().unwrap().tokens.output, Some(7));
}

#[test]
fn inclusive_input_is_serialized_and_legacy_missing_fields_deserialize_as_none() {
    let tokens = Tokens {
        inclusive_input: Some(100),
        input: Some(20),
        cache_read: Some(80),
        ..Tokens::default()
    };
    let value = serde_json::to_value(&tokens).unwrap();
    assert_eq!(value["inclusiveInput"], 100);
    assert!(value["cacheWrite"].is_null());
    let decoded: Tokens = serde_json::from_value(json!({"input":20,"cacheRead":80})).unwrap();
    assert_eq!(decoded.inclusive_input, None);
    assert_eq!(decoded.cache_write, None);
}
