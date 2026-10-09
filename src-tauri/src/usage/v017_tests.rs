use super::{
    model::*,
    pricing::{Config, Pricing, ProviderMapping},
    store::Store,
    Service,
};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

const DAY: i64 = 86_400_000;
const BASE: i64 = 20_000 * DAY;

fn mapping(client: &str, provider: &str, match_on: &str, from: &str, to: &str) -> ProviderMapping {
    ProviderMapping {
        client: client.into(),
        provider: provider.into(),
        enabled: true,
        match_on: match_on.into(),
        from_model: from.into(),
        to_model: to.into(),
    }
}

fn rates(input: &str, output: &str) -> Value {
    json!({"input_cost_per_token": input, "output_cost_per_token": output})
}

fn configure(prices: &Pricing, rules: Vec<ProviderMapping>) {
    let view = prices.view();
    let mut config = Config {
        auto_update: false,
        provider_mappings: rules,
        ..view.config
    };
    config.fixed.extend(BTreeMap::from([
        ("fixture-visible".into(), rates("0.1", "0.2")),
        ("fixture-price-a".into(), rates("0.1", "0.2")),
        ("fixture-price-b".into(), rates("1", "2")),
        ("fixture-price-c".into(), rates("3", "4")),
    ]));
    prices.configure(config, &view.revision).unwrap();
}

fn meter(model: &str, input: u64, output: u64) -> Meter {
    Meter {
        model: Some(model.into()),
        tokens: Tokens {
            input: Some(input),
            output: Some(output),
            ..Tokens::default()
        },
        ..Meter::default()
    }
}

#[test]
fn provider_mappings_are_exact_and_isolated_by_client_provider_and_model_field() {
    let dir = tempfile::tempdir().unwrap();
    let pricing = Pricing::new(dir.path()).unwrap();
    let mut disabled = mapping(
        "codex",
        "provider-a",
        "response",
        "fixture-disabled",
        "fixture-price-c",
    );
    disabled.enabled = false;
    configure(
        &pricing,
        vec![
            mapping(
                "codex",
                "provider-a",
                "request",
                "fixture-shared",
                "fixture-price-a",
            ),
            mapping(
                "codex",
                "provider-b",
                "request",
                "fixture-shared",
                "fixture-price-b",
            ),
            mapping(
                "claude",
                "provider-a",
                "request",
                "fixture-shared",
                "fixture-price-c",
            ),
            mapping(
                "codex",
                "provider-a",
                "response",
                "fixture-answer",
                "fixture-price-b",
            ),
            disabled,
        ],
    );

    for (client, provider, request, response, preference, expected, basis) in [
        (
            "codex",
            Some("provider-a"),
            "fixture-shared",
            "fixture-visible",
            "response",
            "fixture-price-a",
            "provider_mapping",
        ),
        (
            "codex",
            Some("provider-b"),
            "fixture-shared",
            "fixture-visible",
            "response",
            "fixture-price-b",
            "provider_mapping",
        ),
        (
            "claude",
            Some("provider-a"),
            "fixture-shared",
            "fixture-visible",
            "response",
            "fixture-price-c",
            "provider_mapping",
        ),
        (
            "codex",
            Some("provider-a"),
            "fixture-shared",
            "fixture-answer",
            "request",
            "fixture-price-b",
            "provider_mapping",
        ),
        (
            "claude",
            Some("provider-b"),
            "fixture-shared",
            "fixture-visible",
            "response",
            "fixture-visible",
            "response",
        ),
        (
            "codex",
            None,
            "fixture-shared",
            "fixture-visible",
            "request",
            "fixture-shared",
            "request",
        ),
        (
            "codex",
            Some("provider-a"),
            "fixture-shared-extra",
            "fixture-visible",
            "request",
            "fixture-shared-extra",
            "request",
        ),
        (
            "codex",
            Some("provider-a"),
            "Fixture-shared",
            "fixture-visible",
            "response",
            "fixture-visible",
            "response",
        ),
        (
            "codex",
            Some("provider-a"),
            "fixture-unmapped",
            "fixture-disabled",
            "response",
            "fixture-disabled",
            "response",
        ),
    ] {
        let (model, actual_basis, revision) =
            pricing.resolve(client, provider, Some(request), Some(response), preference);
        assert_eq!(
            model.as_deref(),
            Some(expected),
            "{client} {provider:?} {request} {response}"
        );
        assert_eq!(actual_basis, basis);
        assert_eq!(revision.is_some(), basis == "provider_mapping");
    }
    assert_eq!(
        pricing.resolve(
            "codex",
            Some("provider-a"),
            Some("fixture-visible"),
            None,
            "response"
        ),
        (Some("fixture-visible".into()), "request".into(), None)
    );
    assert_eq!(
        pricing.resolve("codex", Some("provider-a"), None, None, "response"),
        (None, "request".into(), None)
    );
}

#[test]
fn mapping_and_price_snapshots_remain_frozen_for_an_inflight_attempt() {
    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(dir.path());
    configure(
        service.prices().unwrap(),
        vec![mapping(
            "codex",
            "provider-a",
            "request",
            "fixture-shared",
            "fixture-price-a",
        )],
    );
    let trace = service.begin("codex", Some("fixture-shared"));
    let mut attempt = trace.attempt("provider-a", true, "http");
    attempt.update(&meter("fixture-visible", 2, 1), Some(200), None);
    let first_revision = attempt.attempt.mapping_revision.clone().unwrap();
    let first_price_version = attempt.attempt.price.as_ref().unwrap().version.clone();

    configure(
        service.prices().unwrap(),
        vec![mapping(
            "codex",
            "provider-a",
            "request",
            "fixture-shared",
            "fixture-price-b",
        )],
    );
    attempt.update(&meter("fixture-visible", 2, 3), None, Some("success"));
    let saved = &attempt.attempt;
    assert_eq!(saved.requested_model.as_deref(), Some("fixture-shared"));
    assert_eq!(saved.response_model.as_deref(), Some("fixture-visible"));
    assert_eq!(saved.pricing_model.as_deref(), Some("fixture-price-a"));
    assert_eq!(
        saved.mapping_revision.as_deref(),
        Some(first_revision.as_str())
    );
    assert_eq!(saved.pricing_basis.as_deref(), Some("provider_mapping"));
    assert_eq!(saved.price.as_ref().unwrap().version, first_price_version);
    assert_eq!(decimal(&saved.price.as_ref().unwrap().cost), decimal("0.8"));

    let next_trace = service.begin("codex", Some("fixture-shared"));
    let mut next = next_trace.attempt("provider-a", false, "http");
    next.update(&meter("fixture-visible", 2, 3), Some(200), Some("success"));
    assert_eq!(
        next.attempt.pricing_model.as_deref(),
        Some("fixture-price-b")
    );
    assert_ne!(next.attempt.mapping_revision, saved.mapping_revision);
    assert_eq!(
        decimal(&next.attempt.price.as_ref().unwrap().cost),
        decimal("8")
    );

    let mut config = service.prices().unwrap().view().config;
    config.provider_mappings.push(mapping(
        "claude",
        "provider-other",
        "request",
        "fixture-shared",
        "fixture-price-c",
    ));
    let revision = service.prices().unwrap().view().revision;
    service
        .prices()
        .unwrap()
        .configure(config, &revision)
        .unwrap();
    assert_eq!(
        service
            .prices()
            .unwrap()
            .resolve(
                "codex",
                Some("provider-a"),
                Some("fixture-shared"),
                Some("fixture-visible"),
                "response"
            )
            .2,
        next.attempt.mapping_revision
    );
}

#[test]
fn missing_prices_do_not_guess_hidden_models_but_explicit_mappings_can_price_them() {
    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(dir.path());
    configure(service.prices().unwrap(), vec![]);
    let trace = service.begin("codex", Some("fixture-visible"));
    let mut missing = trace.attempt("provider-a", false, "http");
    missing.update(
        &meter("fixture-unlisted-response", 2, 3),
        Some(200),
        Some("success"),
    );
    assert_eq!(
        missing.attempt.pricing_model.as_deref(),
        Some("fixture-unlisted-response")
    );
    assert_eq!(missing.attempt.pricing_basis.as_deref(), Some("response"));
    assert!(missing.attempt.price.is_none());
    assert!(missing.attempt.mapping_revision.is_none());

    configure(
        service.prices().unwrap(),
        vec![mapping(
            "codex",
            "provider-a",
            "response",
            "fixture-unlisted-response",
            "fixture-price-a",
        )],
    );
    let mapped_trace = service.begin("codex", Some("fixture-visible"));
    let mut mapped = mapped_trace.attempt("provider-a", false, "http");
    mapped.update(
        &meter("fixture-unlisted-response", 2, 3),
        Some(200),
        Some("success"),
    );
    assert_eq!(
        mapped.attempt.response_model.as_deref(),
        Some("fixture-unlisted-response")
    );
    assert_eq!(
        mapped.attempt.pricing_model.as_deref(),
        Some("fixture-price-a")
    );
    assert_eq!(
        mapped.attempt.pricing_basis.as_deref(),
        Some("provider_mapping")
    );
    assert_eq!(
        decimal(&mapped.attempt.price.as_ref().unwrap().cost),
        decimal("0.8")
    );

    let visible_trace = service.begin("codex", Some("fixture-visible"));
    let mut visible = visible_trace.attempt("provider-a", false, "http");
    visible.update(&meter("fixture-visible", 2, 3), Some(200), Some("success"));
    assert_eq!(
        visible.attempt.pricing_model.as_deref(),
        Some("fixture-visible")
    );
    assert_eq!(visible.attempt.pricing_basis.as_deref(), Some("response"));
    assert!(visible.attempt.mapping_revision.is_none());
    assert_eq!(
        decimal(&visible.attempt.price.as_ref().unwrap().cost),
        decimal("0.8")
    );
}

#[test]
fn mapped_pricing_preserves_cache_dimensions_and_search_per_request_fees() {
    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(dir.path());
    configure(
        service.prices().unwrap(),
        vec![mapping(
            "codex",
            "provider-a",
            "request",
            "fixture-shared",
            "fixture-price-a",
        )],
    );
    let view = service.prices().unwrap().view();
    let mut config = view.config;
    config.fixed.insert(
        "fixture-price-a".into(),
        json!({
            "input_cost_per_token": "0.1", "output_cost_per_token": "0.2",
            "cache_read_input_token_cost": "0.01", "cache_creation_input_token_cost": "0.15",
            "cache_creation_input_token_cost_above_1hr": "0.3"
        }),
    );
    service
        .prices()
        .unwrap()
        .configure(config, &view.revision)
        .unwrap();
    let settings = Settings {
        multiplier: "2".into(),
        ..service.settings()
    };
    service.configure(settings).unwrap();
    let usage = Meter {
        model: Some("fixture-visible".into()),
        tokens: Tokens {
            input: Some(4),
            output: Some(2),
            cache_read: Some(3),
            cache_write: Some(5),
            cache_write_5m: Some(2),
            cache_write_1h: Some(3),
            ..Tokens::default()
        },
        ..Meter::default()
    };
    let trace = service.begin("codex", Some("fixture-shared"));
    let mut attempt = trace.attempt("provider-a", false, "http");
    attempt.update(&usage, Some(200), Some("success"));
    assert_eq!(attempt.attempt.tokens, usage.tokens);
    assert_eq!(
        decimal(&attempt.attempt.price.as_ref().unwrap().cost),
        decimal("4.06")
    );

    let search = service.begin_operation("codex", Some("fixture-shared"), Operation::WebSearch);
    let mut attempt = search.attempt("provider-a", false, "http");
    attempt.update(&usage, Some(200), Some("success"));
    attempt.update(&usage, None, None);
    let price = attempt.attempt.price.as_ref().unwrap();
    assert_eq!(attempt.attempt.grouping_model(), Some("web_search"));
    assert_eq!(decimal(&price.cost), decimal("0.02"));
    assert_eq!(price.basis.as_ref().unwrap()["quantity"], 1);
    assert_eq!(price.rates.len(), 1);
}

fn priced_attempt(id: &str, input: u64, output: u64) -> Attempt {
    let tokens = Tokens {
        input: Some(input),
        output: Some(output),
        ..Tokens::default()
    };
    let price = super::pricing::Quote {
        model: "fixture-visible".into(),
        data: Some(rates("0.1", "0.2")),
        version: "fixture-price-version".into(),
        source: "fixture".into(),
        multiplier: "1".into(),
    }
    .calculate(&tokens, None);
    Attempt {
        id: id.into(),
        provider: Some("provider-a".into()),
        requested_model: Some("fixture-visible".into()),
        response_model: Some("fixture-visible".into()),
        pricing_model: Some("fixture-visible".into()),
        pricing_basis: Some("response".into()),
        status: Some(200),
        outcome: "success".into(),
        started_at: BASE,
        duration_ms: 10,
        tokens,
        price,
        ..Attempt::default()
    }
}

fn row(id: &str, source: &str, attempt: Attempt) -> Record {
    Record {
        id: id.into(),
        client: if source == "claude" {
            "claude"
        } else {
            "codex"
        }
        .into(),
        source: source.into(),
        started_at: BASE,
        attempts: vec![attempt],
        completed: true,
        ..Record::default()
    }
}

#[test]
fn compacted_attempts_keep_exact_known_cost_tokens_and_unpriced_attempt_counts() {
    let mut original = vec![
        priced_attempt("fixture-first", 1, 2),
        priced_attempt("fixture-missing", 3, 4),
        priced_attempt("fixture-last", 5, 6),
    ];
    original[1].price = None;
    original[1].provider = Some("provider-other".into());
    original[1].pricing_model = Some("fixture-unpriced".into());
    let record = Record {
        attempts: original.clone(),
        ..row("fixture-request", "proxy", Attempt::default())
    };
    let mut before = Totals::default();
    before.add_record(&record);
    let mut compacted = original.remove(0);
    for next in original {
        compacted.compact(next);
    }
    assert_eq!(compacted.repeat_count, 3);
    assert_eq!(compacted.compacted_unpriced, Some(1));
    assert_eq!(compacted.tokens.input, Some(9));
    assert_eq!(compacted.tokens.output, Some(12));
    assert_eq!(
        decimal(&compacted.price.as_ref().unwrap().cost),
        decimal("2.2")
    );
    assert!(compacted.provider.is_none());
    assert!(compacted.pricing_model.is_none());
    assert!(compacted.mapping_revision.is_none());
    let record = row("fixture-request", "proxy", compacted);
    assert!(record.cost().is_none());
    let mut after = Totals::default();
    after.add_record(&record);
    assert_eq!(after.tokens, before.tokens);
    assert_eq!(decimal(&after.cost), decimal(&before.cost));
    assert_eq!((after.requests, after.attempts, after.unpriced), (1, 3, 1));
}

#[test]
fn hundreds_of_capacity_attempts_are_persisted_with_bounded_metadata_and_full_consumption() {
    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(dir.path());
    configure(service.prices().unwrap(), vec![]);
    let trace = service.begin("codex", Some("fixture-visible"));
    let id = trace.0.id.clone();
    for index in 0..512 {
        let mut attempt = trace.attempt("provider-a", false, "http");
        attempt.update(
            &meter("fixture-visible", 1, 2),
            Some(if index == 511 { 200 } else { 429 }),
            Some(if index == 511 { "success" } else { "capacity" }),
        );
    }
    drop(trace);
    let began = Instant::now();
    let record = loop {
        if let Ok(record) = service.read(|db| db.detail(&id)) {
            break record;
        }
        assert!(
            began.elapsed() < Duration::from_secs(3),
            "usage writer did not persist the retry record"
        );
        std::thread::sleep(Duration::from_millis(5));
    };
    assert!(record.completed);
    assert!(record.attempts.len() <= 128);
    assert_eq!(record.final_attempt().unwrap().status, Some(200));
    assert_eq!(
        record.attempts.iter().map(|a| a.repeat_count).sum::<u64>(),
        512
    );
    assert!(record
        .attempts
        .iter()
        .all(|a| a.provider.as_deref() == Some("provider-a")));
    assert!(record
        .attempts
        .iter()
        .all(|a| a.pricing_model.as_deref() == Some("fixture-visible")));
    assert!(record
        .attempts
        .iter()
        .all(|a| a.price.as_ref().is_some_and(|p| p.source == "fixed")));
    assert_eq!(record.tokens().input, Some(512));
    assert_eq!(record.tokens().output, Some(1024));
    assert_eq!(record.cost(), decimal("256"));
    let dashboard = service.read(|db| db.dashboard(&Filter::default())).unwrap();
    assert_eq!(
        (
            dashboard.totals.requests,
            dashboard.totals.attempts,
            dashboard.totals.unpriced
        ),
        (1, 512, 0)
    );
    assert_eq!(dashboard.totals.tokens, record.tokens());
    assert_eq!(decimal(&dashboard.totals.cost), decimal("256"));
}

#[test]
fn bounded_mixed_provider_retries_keep_per_provider_consumption_when_dimensions_repeat() {
    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(dir.path());
    configure(
        service.prices().unwrap(),
        vec![
            mapping(
                "codex",
                "provider-a",
                "request",
                "fixture-shared",
                "fixture-price-a",
            ),
            mapping(
                "codex",
                "provider-b",
                "request",
                "fixture-shared",
                "fixture-price-b",
            ),
        ],
    );
    let trace = service.begin("codex", Some("fixture-shared"));
    for index in 0..300 {
        let provider = if index % 2 == 0 {
            "provider-a"
        } else {
            "provider-b"
        };
        let mut attempt = trace.attempt(provider, false, "http");
        attempt.update(&meter("fixture-visible", 1, 2), Some(429), Some("capacity"));
    }
    let attempts = trace.0.attempts.lock().unwrap().clone();
    assert!(attempts.len() <= 128);
    assert_eq!(attempts.iter().map(|a| a.repeat_count).sum::<u64>(), 300);
    assert!(attempts
        .iter()
        .all(|a| a.provider.is_some() && a.mapping_revision.is_some()));
    let mut store = Store::open(&dir.path().join("fixture-projection")).unwrap();
    store
        .write_batch(
            &[Record {
                attempts,
                ..row("fixture-mixed-retries", "proxy", Attempt::default())
            }],
            None,
        )
        .unwrap();
    let dashboard = store.dashboard(&Filter::default()).unwrap();
    assert_eq!(dashboard.totals.attempts, 300);
    assert_eq!(dashboard.totals.tokens.input, Some(300));
    assert_eq!(dashboard.totals.tokens.output, Some(600));
    assert_eq!(decimal(&dashboard.totals.cost), decimal("825"));
    for (provider, cost) in [("provider-a", "75"), ("provider-b", "750")] {
        let group = dashboard
            .providers
            .iter()
            .find(|g| g.id == provider)
            .unwrap();
        assert_eq!(group.totals.attempts, 150);
        assert_eq!(
            (group.totals.tokens.input, group.totals.tokens.output),
            (Some(150), Some(300))
        );
        assert_eq!(decimal(&group.totals.cost), decimal(cost));
    }
}

#[test]
fn unique_retry_dimensions_still_bound_metadata_without_losing_known_or_unknown_costs() {
    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(dir.path());
    configure(service.prices().unwrap(), vec![]);
    let trace = service.begin("codex", Some("fixture-visible"));
    for index in 0..300 {
        let mut attempt = trace.attempt(&format!("fixture-provider-{index}"), false, "http");
        let model = if index % 3 == 0 {
            "fixture-unpriced"
        } else {
            "fixture-visible"
        };
        attempt.update(&meter(model, 1, 2), Some(429), Some("capacity"));
    }
    let attempts = trace.0.attempts.lock().unwrap().clone();
    assert!(attempts.len() <= 128);
    let record = Record {
        attempts,
        ..row("fixture-many-dimensions", "proxy", Attempt::default())
    };
    let mut totals = Totals::default();
    totals.add_record(&record);
    assert_eq!(
        (totals.requests, totals.attempts, totals.unpriced),
        (1, 300, 1)
    );
    assert_eq!(
        (totals.tokens.input, totals.tokens.output),
        (Some(300), Some(600))
    );
    assert_eq!(decimal(&totals.cost), decimal("100"));
    assert!(record.cost().is_none());
    assert!(record
        .attempts
        .iter()
        .any(|a| a.repeat_count > 1 && a.provider.is_none()));
    let mut store = Store::open(&dir.path().join("fixture-projection")).unwrap();
    store.write_batch(&[record], None).unwrap();
    for compact in [false, true] {
        if compact {
            store.compact(BASE + 40 * DAY).unwrap();
        }
        let d = store.dashboard(&Filter::default()).unwrap();
        assert_eq!(
            (d.totals.requests, d.totals.attempts, d.totals.unpriced),
            (1, 300, 1)
        );
        assert_eq!(d.totals.tokens, totals.tokens);
        assert_eq!(decimal(&d.totals.cost), decimal("100"));
    }
}

fn merged_sources(store: &mut Store) -> (Record, Record) {
    let mut gateway = row(
        "fixture-gateway-missing",
        "proxy",
        priced_attempt("fixture-gateway-attempt", 10, 5),
    );
    gateway.attempts[0].tokens = Tokens::default();
    gateway.attempts[0].price = None;
    gateway.attempts[0].response_id = Some("fixture-shared-response".into());
    let mut session = row(
        "fixture-session-a",
        "codex",
        priced_attempt("fixture-session-attempt", 10, 5),
    );
    session.attempts[0].status = None;
    session.attempts[0].provider = None;
    session.attempts[0].response_id = Some("fixture-shared-response".into());
    let mut replay = session.clone();
    replay.id = "fixture-session-b".into();
    replay.started_at += 1;
    let mut independent = row(
        "fixture-independent-gateway",
        "proxy",
        priced_attempt("fixture-independent-attempt", 2, 1),
    );
    independent.attempts[0].response_id = Some("fixture-independent-response".into());
    let mut claude = row(
        "fixture-claude-session",
        "claude",
        priced_attempt("fixture-claude-attempt", 3, 2),
    );
    claude.attempts[0].status = None;
    claude.attempts[0].provider = None;
    claude.attempts[0].response_id = Some("fixture-claude-response".into());
    store
        .write_batch(
            &[
                gateway.clone(),
                session.clone(),
                replay,
                independent,
                claude,
            ],
            None,
        )
        .unwrap();
    (gateway, session)
}

fn filter(source: Option<&str>) -> Filter {
    Filter {
        source: source.map(str::to_owned),
        ..Filter::default()
    }
}

fn check_source_totals(store: &Store) {
    for (source, requests, input, output, cost, unpriced) in [
        (None, 3, 15, 8, "3.1", 0),
        (Some("proxy"), 2, 2, 1, "0.4", 1),
        (Some("sessions"), 2, 13, 7, "2.7", 0),
        (Some("proxy"), 2, 2, 1, "0.4", 1),
        (None, 3, 15, 8, "3.1", 0),
    ] {
        let selected = filter(source);
        let d = store.dashboard(&selected).unwrap();
        assert_eq!(
            (d.totals.requests, d.totals.attempts, d.totals.unpriced),
            (requests, requests, unpriced),
            "{source:?}"
        );
        assert_eq!(
            (d.totals.tokens.input, d.totals.tokens.output),
            (Some(input), Some(output)),
            "{source:?}"
        );
        assert_eq!(decimal(&d.totals.cost), decimal(cost), "{source:?}");
        let mut heatmap = Totals::default();
        for point in store.heatmap(&selected).unwrap() {
            heatmap.add(&point.totals);
        }
        assert_eq!(heatmap.requests, requests, "{source:?}");
        assert_eq!(heatmap.tokens, d.totals.tokens, "{source:?}");
        assert_eq!(decimal(&heatmap.cost), decimal(cost), "{source:?}");
    }
}

#[test]
fn source_filters_distinguish_raw_gateway_and_deduplicated_local_sessions() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let (gateway, session) = merged_sources(&mut store);
    let merged = store.detail(&gateway.id).unwrap();
    assert_eq!(merged.tokens().input, Some(10));
    assert!(merged.gateway_reported.is_some());
    check_source_totals(&store);

    let proxy = store.logs(&filter(Some("proxy"))).unwrap();
    assert_eq!(proxy.total, 2);
    let raw = proxy.rows.iter().find(|r| r.id == gateway.id).unwrap();
    assert_eq!(raw.tokens().total(), None);
    assert!(raw.final_attempt().unwrap().price.is_none());
    let sessions = store.logs(&filter(Some("sessions"))).unwrap();
    assert_eq!(sessions.total, 2);
    assert_eq!(
        sessions.rows.iter().filter(|r| r.client == "codex").count(),
        1
    );
    assert!(sessions.rows.iter().any(|r| r.id == session.id));
    assert!(sessions.rows.iter().all(|r| r.source != "proxy"));
    let codex = Filter {
        client: Some("codex".into()),
        ..filter(Some("sessions"))
    };
    assert_eq!(store.dashboard(&codex).unwrap().totals.requests, 1);
    assert_eq!(store.logs(&codex).unwrap().total, 1);
}

#[test]
fn source_projections_survive_daily_compaction_and_database_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    merged_sources(&mut store);
    check_source_totals(&store);
    store.compact(BASE + 40 * DAY).unwrap();
    assert_eq!(store.logs(&Filter::default()).unwrap().total, 0);
    check_source_totals(&store);
    drop(store);
    check_source_totals(&Store::open(dir.path()).unwrap());
}

#[test]
fn rebuilding_sessions_removes_only_their_supplement_and_preserves_gateway_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let (gateway, session) = merged_sources(&mut store);
    let raw_before = store.logs(&filter(Some("proxy"))).unwrap().rows;
    store.rebuild("codex", &[], &[]).unwrap();
    let restored = store.detail(&gateway.id).unwrap();
    assert_eq!(restored.tokens(), gateway.tokens());
    assert_eq!(restored.final_attempt().unwrap().status, Some(200));
    assert!(restored.gateway_reported.is_none());
    assert!(restored.final_attempt().unwrap().price.is_none());
    assert_eq!(
        store
            .dashboard(&filter(Some("sessions")))
            .unwrap()
            .totals
            .requests,
        1
    );
    assert_eq!(
        store
            .dashboard(&filter(Some("proxy")))
            .unwrap()
            .totals
            .tokens
            .input,
        Some(2)
    );
    for previous in raw_before {
        let after = store
            .logs(&filter(Some("proxy")))
            .unwrap()
            .rows
            .into_iter()
            .find(|r| r.id == previous.id)
            .unwrap();
        assert_eq!(after.tokens(), previous.tokens());
        assert_eq!(after.cost(), previous.cost());
    }
    store
        .rebuild("codex", std::slice::from_ref(&session), &[])
        .unwrap();
    check_source_totals(&store);
}

#[test]
fn v4_migration_rebuilds_raw_gateway_projections_without_repricing_saved_attempts() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let (gateway, _) = merged_sources(&mut store);
    let snapshot = serde_json::to_value(
        store
            .detail(&gateway.id)
            .unwrap()
            .final_attempt()
            .unwrap()
            .price
            .as_ref()
            .unwrap(),
    )
    .unwrap();
    drop(store);
    let db = rusqlite::Connection::open(dir.path().join("usage.sqlite")).unwrap();
    db.execute_batch("DELETE FROM usage_metrics WHERE kind=2; PRAGMA user_version=4;")
        .unwrap();
    drop(db);
    let migrated = Store::open(dir.path()).unwrap();
    check_source_totals(&migrated);
    let detail = migrated.detail(&gateway.id).unwrap();
    assert_eq!(
        serde_json::to_value(detail.final_attempt().unwrap().price.as_ref().unwrap()).unwrap(),
        snapshot
    );
    let db = rusqlite::Connection::open(dir.path().join("usage.sqlite")).unwrap();
    let version: i64 = db
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, 6);
}

#[test]
fn compacted_unknown_prices_stay_unknown_after_backfill_and_daily_rollup() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let pricing = Pricing::new(dir.path()).unwrap();
    configure(&pricing, vec![]);
    let mut summary = priced_attempt("fixture-first", 1, 2);
    summary.price = None;
    let mut next = priced_attempt("fixture-second", 3, 4);
    next.price = None;
    summary.compact(next);
    let record = Record {
        attempts: vec![summary, priced_attempt("fixture-final", 5, 6)],
        ..row("fixture-compacted", "proxy", Attempt::default())
    };
    store
        .write_batch(std::slice::from_ref(&record), None)
        .unwrap();
    store.backfill(&pricing, "1").unwrap();
    let saved = store.detail(&record.id).unwrap();
    assert!(saved.attempts[0].price.is_none());
    assert_eq!(saved.attempts[0].compacted_unpriced, Some(2));
    for compact in [false, true] {
        if compact {
            store.compact(BASE + 40 * DAY).unwrap();
        }
        let d = store.dashboard(&Filter::default()).unwrap();
        assert_eq!(
            (d.totals.requests, d.totals.attempts, d.totals.unpriced),
            (1, 3, 1)
        );
        assert_eq!(
            (d.totals.tokens.input, d.totals.tokens.output),
            (Some(9), Some(12))
        );
        assert_eq!(decimal(&d.totals.cost), decimal("1.7"));
    }
}

#[test]
fn old_attempts_deserialize_as_one_attempt_without_a_compacted_price_claim() {
    let mut legacy = serde_json::to_value(priced_attempt("fixture-old", 1, 2)).unwrap();
    for key in [
        "repeatCount",
        "compactedUnpriced",
        "pricingBasis",
        "mappingRevision",
    ] {
        legacy.as_object_mut().unwrap().remove(key);
    }
    let attempt: Attempt = serde_json::from_value(legacy).unwrap();
    assert_eq!(attempt.repeat_count, 1);
    assert_eq!(attempt.compacted_unpriced, None);
    assert_eq!(attempt.unpriced_count(), 0);
    assert!(attempt.mapping_revision.is_none());
    assert!(attempt.pricing_basis.is_none());
}
