//! v0.25.9 lifecycle composition checks for conversation ownership.
//!
//! These fixtures use only hashed thread/window identifiers and loopback-safe
//! provider metadata.  The module is included below `compaction.rs`, so it can
//! inspect the in-memory ledger without widening the production API.

use super::*;
use crate::gateway::{
    model::Provider,
    protocol::{Protocol, Terminal},
    replay::RequestHints,
    routing::Requirement,
};
use crate::usage::model::Operation;
use serde_json::{json, Value};
use std::sync::Arc;

const OLD: &str = "fixture-old";
const NEW: &str = "fixture-new";

fn digest(value: &str) -> String {
    crate::storage::digest(value.as_bytes())
}

fn ids(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).into()).collect()
}

fn provider(id: &str, allowed_models: Option<&str>, handoff: bool) -> Provider {
    Provider {
        id: id.into(),
        name: id.into(),
        base_url: format!("https://{id}.example.invalid/v1"),
        token: "fixture-token".into(),
        queued: true,
        version: format!("{id}-fixture-version"),
        max_concurrency: 0,
        max_rpm: 0,
        allowed_models: allowed_models.map(|model| vec![model.into()]),
        handoff_after_compaction: handoff,
        take_new_threads: handoff,
    }
}

fn hints(value: Value) -> RequestHints {
    RequestHints::from_value(value, false).expect("fixture request hints")
}

fn compacted_hints(boundary: &str) -> RequestHints {
    let opaque = format!("fixture-opaque-{boundary}");
    let value = json!({
        "model": "fixture-main",
        "input": [{"type": "compaction", "encrypted_content": opaque}]
    });
    let mut result = hints(value);
    // The production projector supplies this value for projected request
    // bodies.  Tests use the digest directly to keep the fixture bounded.
    result.compacted_window = Some(digest(&format!("fixture-opaque-{boundary}")));
    result
}

fn providers() -> Vec<Provider> {
    vec![
        provider(NEW, None, true),
        provider(OLD, Some("fixture-main"), true),
    ]
}

fn seeded_boundary(registry: &Arc<Registry>, session: &str, boundary: &str) {
    let owner = registry
        .prepare(session.into(), None, false, &ids(&[OLD, NEW]))
        .expect("fixture owner lease");
    owner.complete(OLD, None);
    owner.complete(OLD, Some(boundary));
    registry.flush();
}

fn state_snapshot(registry: &Registry, session: &str) -> (Value, u64) {
    let state = registry.state.lock().expect("fixture ledger lock");
    (
        serde_json::to_value(state.entries.get(session).expect("fixture entry")).unwrap(),
        state.sequence,
    )
}

fn live_boundary(registry: &Registry, session: &str) -> (Option<String>, bool, Option<u64>) {
    let state = registry.state.lock().expect("fixture ledger lock");
    let entry = state.entries.get(session).expect("fixture entry");
    (entry.boundary.clone(), entry.boundary_fresh, entry.claim)
}

#[test]
fn drop_releases_claim_and_persists_before_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("owners.json");
    let registry = Registry::new(path.clone());
    let session = digest("fixture-drop-session");
    let boundary = digest("fixture-drop-boundary");
    seeded_boundary(&registry, &session, &boundary);

    let lease = registry
        .prepare(session.clone(), Some(&boundary), true, &ids(&[NEW, OLD]))
        .unwrap();
    assert!(lease.handoff);
    registry.flush();
    drop(lease);
    registry.flush();
    drop(registry);

    let restored = Registry::new(path);
    let retry = restored
        .prepare(session.clone(), Some(&boundary), true, &ids(&[NEW, OLD]))
        .unwrap();
    assert!(retry.handoff, "Drop must durably release the claim");
}

#[test]
fn interrupted_claim_is_consumed_on_restart_and_cannot_be_replayed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("owners.json");
    let registry = Registry::new(path.clone());
    let session = digest("fixture-crash-session");
    let boundary = digest("fixture-crash-boundary");
    seeded_boundary(&registry, &session, &boundary);

    let lease = registry
        .prepare(session.clone(), Some(&boundary), true, &ids(&[NEW, OLD]))
        .unwrap();
    assert!(lease.handoff);
    registry.flush();
    // Capture the durable in-flight claim before normal teardown.  Loading
    // those bytes in a fresh Registry simulates process loss without leaking
    // the original registry's persistence worker.
    let crash_snapshot = std::fs::read(&path).unwrap();
    drop(lease);
    registry.flush();
    drop(registry);
    let crash_path = dir.path().join("crash-snapshot.json");
    std::fs::write(&crash_path, crash_snapshot).unwrap();

    let restored = Registry::new(crash_path);
    assert!(!restored.boundary_matches(&session, Some(&boundary)));
    let retry = restored
        .prepare(session.clone(), Some(&boundary), true, &ids(&[NEW, OLD]))
        .unwrap();
    assert!(!retry.handoff, "an interrupted claim must not revive");
}

#[test]
fn live_receiver_policy_rejects_disabled_or_reordered_handoff_admission() {
    let dir = tempfile::tempdir().unwrap();
    let gateway = crate::gateway::Gateway::new(dir.path().to_path_buf()).unwrap();
    let registry = &gateway.0.compaction;
    let session = digest("fixture-live-receiver-session");
    let boundary = digest("fixture-live-receiver-boundary");
    seeded_boundary(registry, &session, &boundary);
    let queued = vec![
        provider(NEW, None, true),
        provider(OLD, Some("fixture-main"), true),
    ];
    gateway.0.inner.lock().unwrap().store.providers = queued.clone();
    let mut compacted = compacted_hints(&boundary);
    compacted.compacted_window = Some(boundary.clone());
    let lease = registry
        .prepare_request(
            session.clone(),
            Some(&compacted),
            true,
            &queued,
            &Requirement::Resource,
            None,
        )
        .unwrap();
    assert!(lease.handoff);

    gateway.0.inner.lock().unwrap().store.providers[0].handoff_after_compaction = false;
    let disabled = lease
        .admit_request(&gateway, &queued[0], &Requirement::Resource, false)
        .unwrap_err();
    assert_eq!(disabled.code, crate::errors::code::NO_ELIGIBLE_PROVIDER);

    let mut reordered = queued.clone();
    reordered.swap(0, 1);
    gateway.0.inner.lock().unwrap().store.providers = reordered;
    let after_owner = lease
        .admit_request(&gateway, &queued[0], &Requirement::Resource, false)
        .unwrap_err();
    assert_eq!(after_owner.code, crate::errors::code::NO_ELIGIBLE_PROVIDER);
}

#[test]
fn stale_cancelled_drop_cannot_clear_a_later_claim_in_same_generation() {
    let dir = tempfile::tempdir().unwrap();
    let registry = Registry::new(dir.path().join("owners.json"));
    let session = digest("fixture-same-generation-session");
    let boundary = digest("fixture-same-generation-boundary");
    seeded_boundary(&registry, &session, &boundary);

    let stale = registry
        .prepare(session.clone(), Some(&boundary), true, &ids(&[NEW, OLD]))
        .unwrap();
    assert!(stale.handoff);
    // Cancel the old claim while retaining its Lease object.  The next
    // request claims the same epoch and boundary generation.
    {
        let mut state = registry.state.lock().unwrap();
        state.entries.get_mut(&session).unwrap().claim = None;
    }
    let current = registry
        .prepare(session.clone(), Some(&boundary), true, &ids(&[NEW, OLD]))
        .unwrap();
    assert!(current.handoff);
    drop(stale);
    let state = registry.state.lock().unwrap();
    assert_eq!(
        state.entries.get(&session).unwrap().claim,
        Some(current.sequence)
    );
}

#[test]
fn response_created_is_real_generation_output_and_invalidates_old_window() {
    let dir = tempfile::tempdir().unwrap();
    let registry = Registry::new(dir.path().join("owners.json"));
    let session = digest("fixture-created-session");
    let boundary = digest("fixture-created-boundary");
    seeded_boundary(&registry, &session, &boundary);

    let lease = registry
        .prepare(session.clone(), Some(&boundary), false, &ids(&[OLD, NEW]))
        .unwrap();
    assert!(!lease.handoff);
    let mut protocol = Protocol::new(true);
    protocol.attach_ownership(Some(lease.clone()), OLD);
    protocol.websocket_status(101);
    protocol.value(&json!({
        "type": "response.created",
        "response": {"id": "fixture-response", "model": "fixture-main"}
    }));
    protocol.delivery().handed(1, false);
    protocol.output_handed(true);
    assert!(!registry.boundary_matches(&session, Some(&boundary)));
    assert!(!protocol.delivered_success());
}

#[test]
fn compaction_operation_keeps_old_window_until_success_builds_new_window() {
    let dir = tempfile::tempdir().unwrap();
    let registry = Registry::new(dir.path().join("owners.json"));
    let session = digest("fixture-compaction-session");
    let boundary = digest("fixture-compaction-boundary");
    seeded_boundary(&registry, &session, &boundary);
    let lease = registry
        .prepare(session.clone(), Some(&boundary), true, &ids(&[NEW, OLD]))
        .unwrap();
    assert!(lease.handoff);

    let mut protocol = Protocol::new(true);
    protocol.set_operation(Operation::Compaction);
    protocol.attach_ownership(Some(lease), NEW);
    protocol.websocket_status(101);
    protocol.value(&json!({
        "type": "response.created",
        "response": {"id": "fixture-compaction-response"}
    }));
    protocol.delivery().handed(1, false);
    protocol.output_handed(true);
    let (live, fresh, claim) = live_boundary(&registry, &session);
    assert_eq!(live.as_deref(), Some(boundary.as_str()));
    assert!(fresh);
    assert!(
        claim.is_some(),
        "compaction output keeps its claim in flight"
    );

    let opaque = "fixture-new-compaction-opaque";
    protocol.value(&json!({
        "type": "response.completed",
        "response": {
            "status": "completed",
            "output": [{"type": "compaction", "encrypted_content": opaque}]
        }
    }));
    protocol.delivery().handed(1, true);
    protocol.output_handed(true);
    drop(protocol);
    registry.flush();

    let new_boundary = digest(opaque);
    assert_eq!(registry.owner(&session).as_deref(), Some(NEW));
    assert!(!registry.boundary_matches(&session, Some(&boundary)));
    assert!(registry.boundary_matches(&session, Some(&new_boundary)));
}

#[test]
fn unhanded_failure_releases_claim_without_consuming_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let registry = Registry::new(dir.path().join("owners.json"));
    let session = digest("fixture-failed-session");
    let boundary = digest("fixture-failed-boundary");
    seeded_boundary(&registry, &session, &boundary);
    let lease = registry
        .prepare(session.clone(), Some(&boundary), true, &ids(&[NEW, OLD]))
        .unwrap();
    assert!(lease.handoff);

    let mut protocol = Protocol::new(true);
    protocol.attach_ownership(Some(lease.clone()), NEW);
    protocol.websocket_status(101);
    protocol.value(&json!({
        "type": "response.failed",
        "response": {"error": {"code": "server_error"}}
    }));
    assert_eq!(protocol.terminal(), Some(Terminal::Failure));
    // No downstream acknowledgement means no business output.  Dropping the
    // lease must only release the claim, leaving the window reusable.
    drop(protocol);
    drop(lease);
    registry.flush();
    assert!(registry.boundary_matches(&session, Some(&boundary)));
}

#[test]
fn ordinary_output_invalidates_concurrent_handoff_claim_and_stale_lease_cannot_rebind() {
    let dir = tempfile::tempdir().unwrap();
    let registry = Registry::new(dir.path().join("owners.json"));
    let session = digest("fixture-concurrent-session");
    let boundary = digest("fixture-concurrent-boundary");
    seeded_boundary(&registry, &session, &boundary);

    let pending = registry
        .prepare(session.clone(), Some(&boundary), true, &ids(&[NEW, OLD]))
        .unwrap();
    assert!(pending.handoff);
    let ordinary = registry
        .prepare(session.clone(), None, false, &ids(&[OLD, NEW]))
        .unwrap();
    ordinary.output_started(OLD);
    assert!(!registry.boundary_matches(&session, Some(&boundary)));
    pending.output_started(NEW);
    pending.complete(NEW, None);
    assert_eq!(registry.owner(&session).as_deref(), Some(OLD));

    let retry = registry
        .prepare(session.clone(), Some(&boundary), true, &ids(&[NEW, OLD]))
        .unwrap();
    assert!(
        !retry.handoff,
        "the late claim must not retake a spent window"
    );
}

#[test]
fn old_epoch_lease_cannot_write_after_a_legal_handoff() {
    let dir = tempfile::tempdir().unwrap();
    let registry = Registry::new(dir.path().join("owners.json"));
    let session = digest("fixture-epoch-session");
    let first_boundary = digest("fixture-epoch-boundary");
    let stale_boundary = digest("fixture-stale-boundary");
    let stale = registry
        .prepare(session.clone(), None, false, &ids(&[OLD, NEW]))
        .unwrap();
    stale.complete(OLD, None);
    stale.complete(OLD, Some(&first_boundary));

    let handoff = registry
        .prepare(
            session.clone(),
            Some(&first_boundary),
            true,
            &ids(&[NEW, OLD]),
        )
        .unwrap();
    assert!(handoff.handoff);
    handoff.complete(NEW, None);
    assert_eq!(registry.owner(&session).as_deref(), Some(NEW));

    stale.output_started(OLD);
    stale.complete(OLD, Some(&stale_boundary));
    assert_eq!(registry.owner(&session).as_deref(), Some(NEW));
    assert!(!registry.boundary_matches(&session, Some(&stale_boundary)));
}

#[test]
fn auxiliary_and_model_detour_requests_remain_request_only_and_do_not_mutate_ledger() {
    let dir = tempfile::tempdir().unwrap();
    let registry = Registry::new(dir.path().join("owners.json"));
    let session = digest("fixture-request-only-session");
    let owner = registry
        .prepare(session.clone(), None, false, &ids(&[OLD, NEW]))
        .unwrap();
    owner.complete(OLD, None);
    let before = state_snapshot(&registry, &session);
    let queued = providers();

    let auxiliary = hints(json!({"generate": false, "input": ""}));
    assert!(registry
        .prepare_request(
            session.clone(),
            Some(&auxiliary),
            true,
            &queued,
            &Requirement::Resource,
            None,
        )
        .is_none());
    assert_eq!(state_snapshot(&registry, &session), before);

    let detour = hints(json!({
        "model": "fixture-side",
        "input": [{"role": "assistant", "content": "fixture"}]
    }));
    let detour_lease = registry
        .prepare_request(
            session.clone(),
            Some(&detour),
            true,
            &queued,
            &Requirement::Model("fixture-side".into()),
            None,
        )
        .unwrap();
    assert!(detour_lease.request_only());
    detour_lease.output_started(NEW);
    detour_lease.complete(NEW, Some(&digest("fixture-detour-boundary")));
    drop(detour_lease);
    assert_eq!(state_snapshot(&registry, &session), before);
}

#[test]
fn fixed_cursor_keeps_ledger_owner_when_that_owner_left_queue() {
    let dir = tempfile::tempdir().unwrap();
    let registry = Registry::new(dir.path().join("owners.json"));
    let session = digest("fixture-cursor-session");
    let owner = registry
        .prepare(session.clone(), None, false, &ids(&[OLD, NEW]))
        .unwrap();
    owner.complete(OLD, None);
    let before = state_snapshot(&registry, &session);

    let cursor = hints(json!({
        "previous_response_id": "fixture-response-id",
        "input": [{"role": "assistant", "content": "fixture"}]
    }));
    let lease = registry
        .prepare_request(
            session.clone(),
            Some(&cursor),
            true,
            &[provider(NEW, None, true)],
            &Requirement::Resource,
            Some(NEW),
        )
        .unwrap();
    assert!(lease.request_only());
    assert_eq!(lease.owner.as_deref(), Some(OLD));
    lease.output_started(NEW);
    lease.complete(NEW, Some(&digest("fixture-cursor-boundary")));
    drop(lease);
    assert_eq!(registry.owner(&session).as_deref(), Some(OLD));
    assert_eq!(state_snapshot(&registry, &session), before);
}

#[test]
fn removed_owner_still_allows_fresh_matching_compaction_receiver() {
    let dir = tempfile::tempdir().unwrap();
    let registry = Registry::new(dir.path().join("owners.json"));
    let session = digest("fixture-removed-owner-session");
    let boundary = digest("fixture-removed-owner-boundary");
    seeded_boundary(&registry, &session, &boundary);
    let queued = vec![provider(NEW, None, true)];
    let compacted = compacted_hints(&boundary);
    // Replace the synthetic projection with the exact seeded digest.  This is
    // the same value a projected request carries after SHA-256 validation.
    let mut compacted = compacted;
    compacted.compacted_window = Some(boundary.clone());
    let lease = registry
        .prepare_request(
            session.clone(),
            Some(&compacted),
            true,
            &queued,
            &Requirement::Resource,
            None,
        )
        .unwrap();
    assert!(lease.handoff);
    assert_eq!(lease.owner.as_deref(), Some(OLD));
    assert!(lease.admitted(NEW));
    lease.complete(NEW, None);
    assert_eq!(registry.owner(&session).as_deref(), Some(NEW));
}
