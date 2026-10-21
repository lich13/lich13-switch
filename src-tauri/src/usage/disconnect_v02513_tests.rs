//! v0.25.13 exact-response completion after a disconnected gateway attempt.
//! Every row is synthetic and uses loopback-free fixture metadata.
use super::{
    model::{safe_id, Attempt, Record, Tokens},
    store::Store,
};
use serde_json::json;
use std::collections::BTreeMap;

const BASE: i64 = 40_000 * 86_400_000;

fn price(cost: &str) -> super::model::PriceSnapshot {
    super::model::PriceSnapshot {
        version: "fixture-price-v02513".into(),
        source: "fixture-catalog".into(),
        model: "fixture-model".into(),
        multiplier: "1".into(),
        rates: BTreeMap::from([("input".into(), "0.00001".into())]),
        cost: cost.into(),
        basis: Some(json!({"input_cost_per_token":"0.00001"})),
    }
}

fn attempt(id: &str, source: &str, response: &str, tokens: Tokens) -> Attempt {
    Attempt {
        id: format!("{id}-attempt"),
        provider: (source == "proxy").then(|| "fixture-provider".into()),
        requested_model: Some("fixture-model".into()),
        response_model: Some("fixture-model".into()),
        pricing_model: Some("fixture-model".into()),
        response_id: Some(response.into()),
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

fn gateway(id: &str, response: &str, tokens: Tokens) -> Record {
    Record {
        id: id.into(),
        client: "codex".into(),
        source: "proxy".into(),
        started_at: BASE,
        completed: false,
        attempts: vec![attempt(id, "proxy", response, tokens)],
        ..Record::default()
    }
}

fn donor(id: &str, response: &str, tokens: Tokens) -> Record {
    let mut record = Record {
        id: id.into(),
        client: "codex".into(),
        source: "codex".into(),
        started_at: BASE,
        completed: false,
        attempts: vec![attempt(id, "codex", response, tokens)],
        ..Record::default()
    };
    let attempt = record.attempts.first_mut().unwrap();
    attempt.provider = None;
    attempt.status = None;
    attempt.stream = false;
    attempt.transport = "session".into();
    record
}

#[test]
fn exact_response_donors_fill_only_missing_fields_after_disconnect() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let response = safe_id("fixture-disconnect-exact-response");
    let mut gateway = gateway(
        "fixture-disconnect-gateway",
        &response,
        Tokens {
            input: Some(0),
            output: None,
            cache_read: Some(0),
            cache_write: None,
            image_input: None,
            audio_output: None,
            ..Tokens::default()
        },
    );
    gateway.attempts[0].outcome = "cancelled".into();
    gateway.attempts[0].usage_status = "ended_early".into();
    gateway.attempts[0].price = Some(price("0.00042"));
    let raw_gateway = {
        let mut raw = gateway.attempts[0].clone();
        raw.annotate_availability("proxy");
        raw
    };

    let donor_a = donor(
        "fixture-disconnect-donor-a",
        &response,
        Tokens {
            input: Some(100),
            output: Some(7),
            cache_read: Some(20),
            cache_write: Some(3),
            image_input: Some(2),
            audio_output: Some(1),
            ..Tokens::default()
        },
    );
    let donor_b = donor(
        "fixture-disconnect-donor-b",
        &response,
        Tokens {
            input: Some(100),
            output: Some(7),
            cache_read: Some(20),
            cache_write: Some(4),
            image_input: Some(2),
            audio_output: Some(9),
            ..Tokens::default()
        },
    );
    store
        .write_batch(&[gateway.clone(), donor_a, donor_b], None)
        .unwrap();

    let merged = store.detail(&gateway.id).unwrap();
    let attempt = merged.final_attempt().expect("merged gateway attempt");
    assert_eq!(attempt.response_id.as_deref(), Some(response.as_str()));
    assert_eq!(attempt.tokens.input, Some(0));
    assert_eq!(attempt.tokens.cache_read, Some(0));
    assert_eq!(attempt.tokens.output, Some(7));
    assert_eq!(attempt.tokens.image_input, Some(2));
    assert_eq!(
        attempt.tokens.cache_write, None,
        "conflicting donors must leave the field unknown"
    );
    assert_eq!(
        attempt.tokens.audio_output, None,
        "conflicting donors must leave the field unknown"
    );
    assert_eq!(
        attempt.usage_sources.get("output").map(String::as_str),
        Some("session")
    );
    assert_eq!(
        attempt.usage_sources.get("image_input").map(String::as_str),
        Some("session")
    );
    assert_eq!(
        serde_json::to_value(merged.gateway_reported.as_ref()).unwrap(),
        serde_json::to_value(Some(&raw_gateway)).unwrap()
    );
    assert_eq!(
        merged.tokens().output,
        Some(7),
        "raw gateway snapshot must not double count"
    );
    assert_eq!(
        merged.tokens().input,
        Some(0),
        "raw gateway zero must remain zero"
    );
}

#[test]
fn exact_response_donors_keep_consensus_and_raw_gateway_when_zero_tail_is_real() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let response = safe_id("fixture-disconnect-zero-response");
    let mut gateway = gateway(
        "fixture-disconnect-zero-gateway",
        &response,
        Tokens {
            input: Some(0),
            output: Some(0),
            cache_read: Some(0),
            cache_write: Some(0),
            ..Tokens::default()
        },
    );
    gateway.attempts[0].outcome = "cancelled".into();
    gateway.attempts[0].usage_status = "reported".into();
    let raw_gateway = {
        let mut raw = gateway.attempts[0].clone();
        raw.annotate_availability("proxy");
        raw
    };
    let donor_a = donor(
        "fixture-disconnect-zero-donor-a",
        &response,
        Tokens {
            input: Some(50),
            output: Some(7),
            cache_read: Some(10),
            cache_write: Some(2),
            ..Tokens::default()
        },
    );
    let donor_b = donor(
        "fixture-disconnect-zero-donor-b",
        &response,
        Tokens {
            input: Some(50),
            output: Some(7),
            cache_read: Some(10),
            cache_write: Some(2),
            ..Tokens::default()
        },
    );
    store
        .write_batch(&[gateway.clone(), donor_a, donor_b], None)
        .unwrap();

    let merged = store.detail(&gateway.id).unwrap();
    let attempt = merged.final_attempt().unwrap();
    assert_eq!(attempt.tokens.input, Some(0));
    assert_eq!(attempt.tokens.output, Some(0));
    assert_eq!(attempt.tokens.cache_read, Some(0));
    assert_eq!(attempt.tokens.cache_write, Some(0));
    assert_eq!(
        serde_json::to_value(merged.gateway_reported.as_ref()).unwrap(),
        serde_json::to_value(Some(&raw_gateway)).unwrap()
    );
    assert_eq!(merged.tokens().total(), Some(0));
}
