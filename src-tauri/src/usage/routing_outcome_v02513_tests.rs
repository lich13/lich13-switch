//! v0.25.13 routing outcome persistence and completion-handle lifetimes.
//! All service data and identifiers are synthetic fixtures.
use super::{
    model::{
        Attempt, EndCause, EndMetadata, Meter, PriceSnapshot, Record, Tokens, TransferMetadata,
    },
    pricing::Config,
    Service,
};
use crate::events::RoutingOutcome;
use serde_json::json;
use std::{collections::BTreeMap, time::Duration};

const DAY: i64 = 86_400_000;
const BASE: i64 = 40_000 * DAY;

fn fixture_price() -> PriceSnapshot {
    PriceSnapshot {
        version: "fixture-routing-price".into(),
        source: "fixture-catalog".into(),
        model: "fixture-model".into(),
        multiplier: "1".into(),
        rates: BTreeMap::from([("input".into(), "0.00001".into())]),
        cost: "0.00042".into(),
        basis: Some(json!({"input_cost_per_token":"0.00001"})),
    }
}

fn configure_fixed_price(service: &Service) {
    let pricing = service.prices().unwrap();
    let view = pricing.view();
    let mut config: Config = view.config;
    config.auto_update = false;
    config.fixed.insert(
        "fixture-model".into(),
        json!({
            "input_cost_per_token": "0.1",
            "output_cost_per_token": "0.2"
        }),
    );
    pricing.configure(config, &view.revision).unwrap();
}

fn stored_attempt(id: &str, outcome: RoutingOutcome) -> Attempt {
    Attempt {
        id: format!("{id}-attempt"),
        provider: Some("fixture-provider".into()),
        requested_model: Some("fixture-model".into()),
        response_model: Some("fixture-model".into()),
        pricing_model: Some("fixture-model".into()),
        response_id: Some(format!("{id}-response")),
        status: Some(200),
        outcome: "success".into(),
        started_at: BASE,
        duration_ms: 321,
        stream: true,
        transport: "http".into(),
        tokens: Tokens {
            input: Some(3),
            output: Some(5),
            ..Tokens::default()
        },
        usage_status: "reported".into(),
        routing_outcome: Some(outcome),
        end: Some(EndMetadata {
            cause: EndCause::Completed,
            terminal_received: true,
            response_started: true,
        }),
        price: Some(fixture_price()),
        ..Attempt::default()
    }
}

fn record(id: &str, outcome: RoutingOutcome, at: i64) -> Record {
    let mut attempt = stored_attempt(id, outcome);
    attempt.started_at = at;
    Record {
        id: id.into(),
        client: "codex".into(),
        source: "proxy".into(),
        started_at: at,
        finished_at: Some(at + attempt.duration_ms as i64),
        attempts: vec![attempt],
        completed: true,
        ..Record::default()
    }
}

fn ready_store(dir: &std::path::Path) -> super::store::Store {
    let legacy = super::store::Store::open_v11_fixture(dir).unwrap();
    drop(legacy);
    let mut store = super::store::Store::open(dir).unwrap();
    for _ in 0..100 {
        if !store.compact_storage_batch().unwrap() {
            store.reclaim_space().unwrap();
            return store;
        }
    }
    panic!("v12 fixture migration did not finish");
}

async fn wait_for_record(service: &Service, id: &str) -> Record {
    let mut changes = service.subscribe();
    tokio::time::timeout(Duration::from_secs(4), async {
        loop {
            if let Ok(record) = service.read(|store| store.detail(id)) {
                return record;
            }
            let _ = changes.recv().await;
        }
    })
    .await
    .expect("routing fixture record was not persisted")
}

#[tokio::test]
async fn completion_handle_keeps_one_record_until_attempt_drop_and_preserves_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(dir.path());
    configure_fixed_price(&service);
    let trace = service.begin("codex", Some("fixture-model"));
    let id = trace.0.id.clone();
    let mut attempt = trace.attempt("fixture-provider", true, "http");
    let expected_event_time = attempt.attempt.started_at + 1_234;
    attempt.transfer(TransferMetadata {
        terminal_at: Some(expected_event_time),
        ended_at: expected_event_time,
        terminal_handed_off: true,
        ..TransferMetadata::default()
    });
    attempt.end(EndMetadata {
        cause: EndCause::Completed,
        terminal_received: true,
        response_started: true,
    });
    let meter = Meter {
        tokens: Tokens {
            input: Some(3),
            output: Some(5),
            ..Tokens::default()
        },
        ..Meter::default()
    };
    attempt.update(&meter, Some(200), Some("success"));
    let completion = attempt.routing_completion();
    let generation = completion.begin_save();

    // A receipt may arrive before AttemptTrace is dropped. It updates the
    // completion state and must not create a second record.
    completion.saved(generation, RoutingOutcome::OwnerPreserved);
    drop(attempt);
    let before = trace.0.attempts.lock().unwrap()[0].clone();
    completion.saved(generation, RoutingOutcome::OwnerPreserved);
    let after = trace.0.attempts.lock().unwrap()[0].clone();
    assert_eq!(before.duration_ms, after.duration_ms);
    assert_eq!(before.started_at, after.started_at);
    assert_eq!(
        serde_json::to_value(&before.price).unwrap(),
        serde_json::to_value(&after.price).unwrap()
    );
    assert!(
        before.price.is_some(),
        "fixed pricing fixture must survive Drop::price"
    );
    assert_eq!(
        before
            .transfer
            .as_ref()
            .map(|t| (t.terminal_at, t.ended_at)),
        after.transfer.as_ref().map(|t| (t.terminal_at, t.ended_at))
    );
    assert_eq!(before.routing_outcome, Some(RoutingOutcome::OwnerPreserved));
    assert_eq!(after.routing_outcome, Some(RoutingOutcome::OwnerPreserved));
    drop(completion);
    drop(trace);

    let row = wait_for_record(&service, &id).await;
    assert_eq!(
        row.attempts.len(),
        1,
        "one completion handle must yield one record"
    );
    assert_eq!(
        row.final_attempt().unwrap().routing_outcome,
        Some(RoutingOutcome::OwnerPreserved)
    );
    assert_eq!(row.final_attempt().unwrap().duration_ms, after.duration_ms);
    assert_eq!(
        serde_json::to_value(&row.final_attempt().unwrap().price).unwrap(),
        serde_json::to_value(&after.price).unwrap()
    );
    assert_eq!(row.event_time(), expected_event_time);
}

#[tokio::test]
async fn completion_receipts_are_generation_ordered_and_save_failed_is_sticky() {
    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(dir.path());
    let trace = service.begin("codex", Some("fixture-model"));
    let id = trace.0.id.clone();
    let mut attempt = trace.attempt("fixture-provider", true, "http");
    let completion = attempt.routing_completion();
    let first = completion.begin_save();
    let second = completion.begin_save();
    completion.saved(first, RoutingOutcome::OwnerPreserved);
    completion.saved(second, RoutingOutcome::SaveFailed);
    completion.saved(first, RoutingOutcome::OwnershipSaved);
    completion.saved(second, RoutingOutcome::OwnerPreserved);
    drop(attempt);
    assert_eq!(
        trace.0.attempts.lock().unwrap()[0].routing_outcome,
        Some(RoutingOutcome::SaveFailed),
        "stale or later success receipts must not hide SaveFailed"
    );
    drop(completion);
    drop(trace);
    let row = wait_for_record(&service, &id).await;
    assert_eq!(row.attempts.len(), 1);
    assert_eq!(
        row.final_attempt().unwrap().routing_outcome,
        Some(RoutingOutcome::SaveFailed)
    );
}

#[test]
fn compacting_attempts_with_different_routing_outcomes_is_unknown() {
    let mut first = stored_attempt("fixture-routing-compact-a", RoutingOutcome::OwnerPreserved);
    let second = stored_attempt("fixture-routing-compact-b", RoutingOutcome::SaveFailed);
    first.compact(second);
    assert_eq!(first.repeat_count, 2);
    assert_eq!(first.routing_outcome, Some(RoutingOutcome::Unknown));
}

#[test]
fn routing_outcome_roundtrips_old_missing_and_unknown_values() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = ready_store(dir.path());
    let saved = record(
        "fixture-routing-roundtrip",
        RoutingOutcome::SaveFailed,
        BASE,
    );
    store
        .write_batch(std::slice::from_ref(&saved), None)
        .unwrap();
    let loaded = store.detail(&saved.id).unwrap();
    assert_eq!(
        loaded.final_attempt().unwrap().routing_outcome,
        Some(RoutingOutcome::SaveFailed)
    );

    let mut legacy = serde_json::to_value(saved.final_attempt().unwrap()).unwrap();
    legacy.as_object_mut().unwrap().remove("routingOutcome");
    let decoded: Attempt = serde_json::from_value(legacy).unwrap();
    assert_eq!(decoded.routing_outcome, None);

    let mut future = serde_json::to_value(saved.final_attempt().unwrap()).unwrap();
    future["routingOutcome"] = json!("fixture-future-routing-outcome");
    let decoded: Attempt = serde_json::from_value(future).unwrap();
    assert_eq!(decoded.routing_outcome, Some(RoutingOutcome::Unknown));
}

#[test]
fn seven_day_cold_archive_preserves_clean_outcome_but_keeps_save_failed_hot() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = ready_store(dir.path());
    let at = BASE - 8 * DAY;
    let clean = record(
        "fixture-routing-cold-owner",
        RoutingOutcome::OwnerPreserved,
        at,
    );
    let failed = record(
        "fixture-routing-cold-failed",
        RoutingOutcome::SaveFailed,
        at,
    );
    let mut clean = clean;
    clean.attempts[0].thread_hash = Some("fixture-thread-hash".into());
    clean.attempts[0].routing_evidence = Some(Default::default());
    let mut failed = failed;
    failed.attempts[0].thread_hash = Some("fixture-failed-thread-hash".into());
    failed.attempts[0].routing_evidence = Some(Default::default());
    store
        .write_batch(&[clean.clone(), failed.clone()], None)
        .unwrap();
    store.compact(BASE).unwrap();

    let clean_after = store.detail(&clean.id).unwrap();
    assert_eq!(
        clean_after.final_attempt().unwrap().routing_outcome,
        Some(RoutingOutcome::OwnerPreserved)
    );
    assert!(
        clean_after.detail_archived,
        "clean seven-day records should be cold archived"
    );

    let failed_after = store.detail(&failed.id).unwrap();
    assert_eq!(
        failed_after.final_attempt().unwrap().routing_outcome,
        Some(RoutingOutcome::SaveFailed)
    );
    assert!(
        !failed_after.detail_archived,
        "SaveFailed records must not be cold archived after seven days"
    );
}
