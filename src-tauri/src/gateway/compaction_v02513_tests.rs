//! v0.25.13 bounded compaction receiver and claim fixtures.

use super::*;
use crate::gateway::{
    circuit::{Circuit, Outcome},
    connector::Connector,
    model::{Provider, Settings},
    model_health::Models,
    replay::RequestHints,
    routing::Requirement,
    ClientId, Route,
};
use crate::usage::{model::Filter as UsageFilter, Service as UsageService};
use hyper_util::{client::legacy::Client, rt::TokioExecutor};
use serde_json::{json, Value};
use std::{sync::Arc, time::Duration};

const OLD: &str = "fixture-old";
const NEW: &str = "fixture-new";
const NEWER: &str = "fixture-newer";

fn digest(value: &str) -> String {
    crate::storage::digest(value.as_bytes())
}

fn ids(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).into()).collect()
}

fn provider(id: &str, queued: bool, allowed_models: Option<&str>, handoff: bool) -> Provider {
    Provider {
        id: id.into(),
        name: id.into(),
        base_url: format!("https://{id}.example.invalid/v1"),
        token: "fixture-token".into(),
        queued,
        version: format!("{id}-fixture-version"),
        max_concurrency: 0,
        max_rpm: 0,
        allowed_models: allowed_models.map(|model| vec![model.into()]),
        handoff_after_compaction: handoff,
        take_new_threads: handoff,
        search_only: false,
    }
}

fn hints(boundary: &str, model: &str) -> RequestHints {
    let mut hints = RequestHints::from_value(
        json!({
            "model": model,
            "input": [{"type": "compaction", "encrypted_content": "fixture-opaque"}]
        }),
        false,
    )
    .expect("fixture compaction hints");
    hints.compacted_window = Some(boundary.into());
    hints
}

fn seed_boundary(registry: &Arc<Registry>, session: &str, boundary: &str) {
    let owner = registry
        .prepare(session.into(), None, false, &ids(&[OLD]))
        .expect("fixture owner lease");
    owner.complete(OLD, None);
    owner.complete(OLD, Some(boundary));
    registry.flush();
}

fn route(provider: Provider) -> Route {
    let client =
        Client::builder(TokioExecutor::new()).build(Connector::new(Duration::from_secs(1), 0));
    Route {
        dsh_models: None,
        grok: None,
        client_id: ClientId::Codex,
        provider,
        client,
        provider_circuit: Circuit::default(),
        model_circuits: Models::default(),
    }
}

fn snapshot(registry: &Registry, session: &str) -> (Value, u64) {
    let state = registry.state.lock().unwrap();
    (
        serde_json::to_value(state.entries.get(session).expect("fixture entry")).unwrap(),
        state.sequence,
    )
}

fn usage_rows(service: &UsageService) -> Vec<crate::usage::model::Record> {
    service
        .read(|store| Ok(store.logs(&UsageFilter::default())?.rows))
        .unwrap()
}

async fn wait_for_usage_row(service: &UsageService) -> crate::usage::model::Record {
    let mut updates = service.subscribe();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(row) = usage_rows(service).into_iter().next() {
                return row;
            }
            updates.recv().await.expect("usage writer closed");
        }
    })
    .await
    .expect("usage receipt did not settle")
}

#[test]
fn no_receiver_does_not_claim_and_output_still_consumes_window() {
    let dir = tempfile::tempdir().unwrap();
    let registry = Registry::new(dir.path().join("owners.json"));
    let session = digest("fixture-no-receiver-session");
    let boundary = digest("fixture-no-receiver-boundary");
    seed_boundary(&registry, &session, &boundary);

    let lease = registry
        .prepare(session.clone(), Some(&boundary), true, &ids(&[OLD]))
        .expect("fixture owner lease");
    assert!(!lease.handoff, "an owner alone is not a receiver");
    let before = snapshot(&registry, &session).0;
    lease.output_started(OLD);
    registry.flush();
    let after = snapshot(&registry, &session).0;
    assert_ne!(before["boundary_fresh"], after["boundary_fresh"]);
    assert_eq!(after["boundary_fresh"], false);
    assert_eq!(after["claim"], Value::Null);
    assert!(!registry.boundary_matches(&session, Some(&boundary)));
}

#[test]
fn receiver_policy_filters_unqueued_disabled_late_and_model_mismatch() {
    let dir = tempfile::tempdir().unwrap();
    let gateway = crate::gateway::Gateway::new(dir.path().join("gateway")).unwrap();
    let registry = gateway.0.compaction.clone();

    let empty_cases = [
        vec![
            provider("fixture-unqueued", false, Some("fixture-main"), true),
            provider(OLD, true, Some("fixture-main"), true),
        ],
        vec![
            provider("fixture-disabled", true, Some("fixture-main"), false),
            provider(OLD, true, Some("fixture-main"), true),
        ],
        vec![
            provider(OLD, true, Some("fixture-main"), true),
            provider("fixture-after-owner", true, Some("fixture-main"), true),
        ],
        vec![
            provider("fixture-mismatch", true, Some("fixture-other"), true),
            provider(OLD, true, Some("fixture-main"), true),
        ],
    ];
    for (index, queued) in empty_cases.iter().enumerate() {
        let session = digest(&format!("fixture-receiver-policy-empty-session-{index}"));
        let boundary = digest(&format!("fixture-receiver-policy-empty-boundary-{index}"));
        seed_boundary(&registry, &session, &boundary);
        let lease = registry
            .prepare_request(
                session.clone(),
                Some(&hints(&boundary, "fixture-main")),
                true,
                queued,
                &Requirement::model(Some("fixture-main")),
                None,
            )
            .expect("fixture empty receiver lease");
        assert!(!lease.handoff, "ineligible receiver set must not claim");
        assert_eq!(snapshot(&registry, &session).0["claim"], Value::Null);
        drop(lease);
        registry.flush();
    }

    let session = digest("fixture-receiver-policy-session");
    let boundary = digest("fixture-receiver-policy-boundary");
    seed_boundary(&registry, &session, &boundary);

    let new = provider(NEW, true, Some("fixture-main"), true);
    let disabled = provider("fixture-disabled", true, Some("fixture-main"), false);
    let mismatch = provider("fixture-mismatch", true, Some("fixture-other"), true);
    let late = provider(NEWER, true, Some("fixture-main"), true);
    let old = provider(OLD, true, Some("fixture-main"), true);
    let queued = vec![new.clone(), disabled, mismatch, old.clone(), late];
    gateway
        .0
        .inner
        .lock()
        .unwrap()
        .editable_mut()
        .store
        .providers = queued.clone();
    let lease = registry
        .prepare_request(
            session,
            Some(&hints(&boundary, "fixture-main")),
            true,
            &queued,
            &Requirement::model(Some("fixture-main")),
            None,
        )
        .expect("fixture compacted lease");
    assert!(lease.handoff);
    assert_eq!(lease.receivers, vec![NEW.to_owned(), OLD.to_owned()]);

    gateway
        .0
        .inner
        .lock()
        .unwrap()
        .editable_mut()
        .store
        .providers
        .iter_mut()
        .find(|candidate| candidate.id == NEW)
        .unwrap()
        .queued = false;
    let error = lease
        .admit_request(
            &gateway,
            &new,
            &Requirement::model(Some("fixture-main")),
            false,
        )
        .expect_err("a receiver removed from the live queue must be rejected");
    assert_eq!(error.code, crate::errors::code::NO_ELIGIBLE_PROVIDER);
}

#[test]
fn removed_owner_with_only_new_receiver_can_handoff() {
    let dir = tempfile::tempdir().unwrap();
    let registry = Registry::new(dir.path().join("owners.json"));
    let session = digest("fixture-removed-owner-only-new-session");
    let boundary = digest("fixture-removed-owner-only-new-boundary");
    seed_boundary(&registry, &session, &boundary);
    let queued = vec![provider(NEW, true, Some("fixture-main"), true)];
    let lease = registry
        .prepare_request(
            session.clone(),
            Some(&hints(&boundary, "fixture-main")),
            true,
            &queued,
            &Requirement::model(Some("fixture-main")),
            None,
        )
        .expect("fixture compacted lease");
    assert!(lease.handoff);
    assert_eq!(lease.owner.as_deref(), Some(OLD));
    assert!(lease.admitted(NEW));
    lease.complete(NEW, None);
    assert_eq!(registry.owner(&session).as_deref(), Some(NEW));
}

#[test]
fn two_legal_receivers_remain_candidates_during_cooldown_and_capacity() {
    let dir = tempfile::tempdir().unwrap();
    let registry = Registry::new(dir.path().join("owners.json"));
    let session = digest("fixture-two-receivers-session");
    let boundary = digest("fixture-two-receivers-boundary");
    seed_boundary(&registry, &session, &boundary);
    let queued = vec![
        provider(NEW, true, None, true),
        provider(NEWER, true, None, true),
        provider(OLD, true, None, true),
    ];
    let lease = registry
        .prepare_request(
            session,
            Some(&hints(&boundary, "fixture-main")),
            true,
            &queued,
            &Requirement::Resource,
            None,
        )
        .expect("fixture compacted lease");
    assert!(lease.handoff);
    assert_eq!(lease.receivers, ids(&[NEW, NEWER, OLD]));

    let first = route(queued[0].clone());
    let second = route(queued[1].clone());
    first.provider_circuit.acquire(false).unwrap().finish(
        Outcome::CapacityLimited(Some(Duration::from_secs(60))),
        &Settings::default(),
    );
    second.provider_circuit.acquire(false).unwrap().finish(
        Outcome::RateLimited(Some(Duration::from_secs(60))),
        &Settings::default(),
    );
    assert!(!first.health_for(&Requirement::Resource).available);
    assert!(!second.health_for(&Requirement::Resource).available);
    let candidates = lease.candidates(vec![first, second, route(queued[2].clone())]);
    assert_eq!(
        candidates
            .iter()
            .map(|candidate| candidate.provider.id.as_str())
            .collect::<Vec<_>>(),
        vec![NEW, NEWER, OLD]
    );
}

#[test]
fn one_boundary_has_one_claim_and_old_lease_cannot_rebind_after_output() {
    let dir = tempfile::tempdir().unwrap();
    let registry = Registry::new(dir.path().join("owners.json"));
    let session = digest("fixture-single-claim-session");
    let boundary = digest("fixture-single-claim-boundary");
    let stale_boundary = digest("fixture-single-claim-stale-boundary");
    seed_boundary(&registry, &session, &boundary);

    let first = registry
        .prepare(session.clone(), Some(&boundary), true, &ids(&[NEW, OLD]))
        .expect("fixture first claim");
    assert!(first.handoff);
    let second = registry
        .prepare(session.clone(), Some(&boundary), true, &ids(&[NEW, OLD]))
        .expect("fixture second lease");
    assert!(!second.handoff, "the same boundary must have one claim");
    drop(second);

    let ordinary = registry
        .prepare(session.clone(), None, false, &ids(&[OLD, NEW]))
        .expect("fixture ordinary lease");
    ordinary.output_started(OLD);
    first.output_started(NEW);
    first.complete(NEW, Some(&stale_boundary));
    assert_eq!(registry.owner(&session).as_deref(), Some(OLD));
    assert!(!registry.boundary_matches(&session, Some(&stale_boundary)));
}

#[test]
fn compaction_receiver_claim_does_not_bind_receiver_before_output() {
    let dir = tempfile::tempdir().unwrap();
    let registry = Registry::new(dir.path().join("owners.json"));
    let session = digest("fixture-admission-before-output-session");
    let boundary = digest("fixture-admission-before-output-boundary");
    seed_boundary(&registry, &session, &boundary);
    let receiver = registry
        .prepare_request(
            session.clone(),
            Some(&hints(&boundary, "fixture-main")),
            true,
            &[
                provider(NEW, true, None, true),
                provider(OLD, true, None, true),
            ],
            &Requirement::Resource,
            None,
        )
        .expect("fixture compacted lease");
    assert!(receiver.handoff);
    assert_eq!(registry.owner(&session).as_deref(), Some(OLD));
    assert!(!receiver.owns(NEW));
    drop(receiver);
    registry.flush();
    assert!(registry.boundary_matches(&session, Some(&boundary)));
}

#[tokio::test]
async fn registry_drop_settles_unprocessed_usage_receipt_as_save_failed() {
    let dir = tempfile::tempdir().unwrap();
    let usage = UsageService::new(&dir.path().join("usage"));
    let trace = usage.begin("codex", Some("fixture-main"));
    let mut attempt = trace.attempt(OLD, true, "http");
    let attempt_id = attempt.id().to_owned();
    let completion = attempt.routing_completion();
    let generation = completion.begin_save();
    let registry = Registry::new(dir.path().join("owners.json"));
    {
        let mut state = registry.state.lock().unwrap();
        state.notifications.push(PendingReport {
            version: 1,
            routing: None,
            record: None,
            usage: Some((completion.clone(), generation)),
            outcome: crate::events::RoutingOutcome::OwnershipSaved,
            scope: "fixture-scope".into(),
            handoff: None,
        });
    }

    // The persistence worker may disappear with the registry. Registry::Drop
    // must still settle every queued receipt before the usage trace is released.
    drop(registry);
    drop(attempt);
    drop(completion);
    drop(trace);

    let row = wait_for_usage_row(&usage).await;
    assert_eq!(row.attempts.len(), 1);
    let attempt = row.final_attempt().expect("usage attempt");
    assert_eq!(attempt.id, attempt_id);
    assert_eq!(
        attempt.routing_outcome,
        Some(crate::events::RoutingOutcome::SaveFailed)
    );
}

#[tokio::test]
async fn disconnected_persistence_sender_rejects_pending_receipt_as_save_failed() {
    let dir = tempfile::tempdir().unwrap();
    let usage = UsageService::new(&dir.path().join("usage"));
    let trace = usage.begin("codex", Some("fixture-main"));
    let mut attempt = trace.attempt(OLD, true, "http");
    let attempt_id = attempt.id().to_owned();
    let completion = attempt.routing_completion();
    let generation = completion.begin_save();
    let (persist, receiver) = std::sync::mpsc::sync_channel(1);
    drop(receiver);
    let registry = Arc::new(Registry {
        path: dir.path().join("owners.json"),
        state: Mutex::new(State::default()),
        persist,
        notifications: Mutex::new(None),
        names: Mutex::new(std::collections::HashMap::new()),
    });
    {
        let mut state = registry.state.lock().unwrap();
        state.notifications.push(PendingReport {
            version: 1,
            routing: None,
            record: None,
            usage: Some((completion.clone(), generation)),
            outcome: crate::events::RoutingOutcome::OwnershipSaved,
            scope: "fixture-scope".into(),
            handoff: None,
        });
        registry.save(&mut state, true);
        assert_eq!(state.error.as_deref(), Some("线程归属记录保存失败"));
        assert!(state.notifications.is_empty());
        assert_eq!(state.settled_version, state.save_version);
    }

    drop(registry);
    drop(attempt);
    drop(completion);
    drop(trace);
    let row = wait_for_usage_row(&usage).await;
    assert_eq!(row.attempts.len(), 1);
    let attempt = row.final_attempt().expect("usage attempt");
    assert_eq!(attempt.id, attempt_id);
    assert_eq!(
        attempt.routing_outcome,
        Some(crate::events::RoutingOutcome::SaveFailed)
    );
}
