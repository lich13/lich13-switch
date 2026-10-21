//! v0.25.9 routing evidence and durable ownership journal coverage.
use super::*;
use crate::{
    events::{self, Action, Category, Reason, RoutingDetails, RoutingOutcome},
    gateway::{compaction::Registry, failover::Failure, replay::RequestHints, ClientId, Gateway},
    usage::model::{BoundaryState, RoutingDecision, RoutingEvidence, RoutingInput, ThreadIdentity},
};
use hyper::HeaderMap;
use serde_json::{json, Value};
use std::{
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

const OLD: &str = "00000000-0000-0000-0000-000000000001";
const NEW: &str = "00000000-0000-0000-0000-000000000002";
const ATTEMPT_ONE: &str = "00000000-0000-0000-0000-000000000011";
const ATTEMPT_TWO: &str = "00000000-0000-0000-0000-000000000012";
const SESSION: &str = "fixture-routing-session";
const BOUNDARY: &str = "fixture-routing-boundary";

fn digest(value: &str) -> String {
    crate::storage::digest(value.as_bytes())
}

fn ids(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).into()).collect()
}

fn evidence(identity: Option<ThreadIdentity>, boundary: Option<BoundaryState>) -> RoutingEvidence {
    RoutingEvidence {
        input: RoutingInput::History,
        has_tool_declarations: false,
        restriction: None,
        identity,
        boundary,
    }
}

fn setup() -> (tempfile::TempDir, Gateway, events::Service) {
    let dir = tempfile::tempdir().unwrap();
    let gateway = Gateway::new(dir.path().join("gateway")).unwrap();
    let events = events::Service::new(dir.path());
    gateway.set_diagnostics(events.clone());
    gateway
        .0
        .compaction
        .notifications(ClientId::Codex, events.clone());
    (dir, gateway, events)
}

fn seed_boundary(registry: &Arc<Registry>, session: &str, boundary: &str) {
    let owner = registry
        .prepare(session.to_owned(), None, false, &ids(&[OLD]))
        .expect("fixture owner lease");
    owner.complete(OLD, None);
    owner.complete(OLD, Some(boundary));
    registry.flush();
}

fn wait_for<F>(service: &events::Service, predicate: F) -> Vec<events::ClassifiedRecord>
where
    F: Fn(&[events::ClassifiedRecord]) -> bool,
{
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let page = service.query(events::Filter::default());
        if predicate(&page.items) {
            return page.items;
        }
        assert!(Instant::now() < deadline, "diagnostic event did not settle");
        thread::sleep(Duration::from_millis(10));
    }
}

fn route_record(
    decision: RoutingDecision,
    outcome: RoutingOutcome,
    attempt_id: Option<&str>,
) -> events::Record {
    let mut record = events::Record::new(
        Some(ClientId::Codex),
        Some(OLD),
        Some("fixture-model"),
        Reason::ThreadRouting,
        Action::Returned,
        None,
        Some(1),
    );
    record.details.routing = Some(RoutingDetails {
        decision,
        evidence: evidence(
            Some(ThreadIdentity::Reliable),
            Some(BoundaryState::NotObserved),
        ),
        outcome,
        attempt_id: attempt_id.map(str::to_owned),
        failure: None,
    });
    record
}

#[test]
fn identity_ignores_modern_session_id_uses_legacy_fallback_and_rejects_conflicts() {
    let mut modern = HeaderMap::new();
    modern.insert("session-id", "modern-affinity".parse().unwrap());
    assert_eq!(
        crate::gateway::compaction::identity(&modern),
        (None, ThreadIdentity::Missing)
    );

    let mut legacy = HeaderMap::new();
    legacy.insert("session_id", "legacy-session".parse().unwrap());
    assert_eq!(
        crate::gateway::compaction::identity(&legacy),
        (
            Some(digest("legacy-session")),
            ThreadIdentity::LegacySession
        )
    );

    let mut conflict = HeaderMap::new();
    conflict.append("thread-id", "thread-a".parse().unwrap());
    conflict.append("conversation_id", "thread-b".parse().unwrap());
    assert_eq!(
        crate::gateway::compaction::identity(&conflict),
        (None, ThreadIdentity::Conflict)
    );
}

#[test]
fn one_log_record_keeps_only_the_final_attempt_and_failure() {
    let (_dir, gateway, events) = setup();
    let log = RoutingLog::new(&gateway).unwrap();
    log.context(
        OLD,
        Some("fixture-model"),
        RoutingDecision::NewThread,
        evidence(Some(ThreadIdentity::Reliable), Some(BoundaryState::Fresh)),
        Some(ATTEMPT_ONE),
    );
    log.failure(Failure::Transport);
    log.context(
        OLD,
        Some("fixture-model"),
        RoutingDecision::ExistingThread,
        evidence(Some(ThreadIdentity::Reliable), Some(BoundaryState::Claimed)),
        Some(ATTEMPT_TWO),
    );
    let record = log
        .take(RoutingOutcome::Failed)
        .expect("final attempt should own one routing record");
    assert_eq!(record.attempt, Some(2));
    let routing = record.details.routing.as_ref().unwrap();
    assert_eq!(routing.attempt_id.as_deref(), Some(ATTEMPT_TWO));
    assert_eq!(routing.failure, Some(Failure::Transport));
    events.emit(record);

    let rows = wait_for(&events, |rows| rows.len() == 1);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].record.attempt, Some(2));
    assert_eq!(
        rows[0]
            .record
            .details
            .routing
            .as_ref()
            .and_then(|routing| routing.attempt_id.as_deref()),
        Some(ATTEMPT_TWO)
    );
}

#[test]
fn unchanged_continuation_does_not_create_a_second_routing_event() {
    let (_dir, gateway, events) = setup();
    let log = RoutingLog::new(&gateway).unwrap();
    log.context(
        OLD,
        Some("fixture-model"),
        RoutingDecision::NewThread,
        evidence(Some(ThreadIdentity::Reliable), Some(BoundaryState::Fresh)),
        Some(ATTEMPT_ONE),
    );
    log.context(
        OLD,
        Some("fixture-model"),
        RoutingDecision::ExistingThread,
        evidence(
            Some(ThreadIdentity::Reliable),
            Some(BoundaryState::NotObserved),
        ),
        Some(ATTEMPT_TWO),
    );
    events.emit(log.take(RoutingOutcome::OwnerPreserved).unwrap());

    let rows = wait_for(&events, |rows| rows.len() == 1);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].record.attempt, Some(2));
    assert_eq!(rows[0].record.count, 1);
    let routing = rows[0].record.details.routing.as_ref().unwrap();
    assert_eq!(routing.attempt_id.as_deref(), Some(ATTEMPT_TWO));
    assert_eq!(routing.decision, RoutingDecision::ExistingThread);
}

#[test]
fn successful_handoff_emits_exactly_one_saved_result() {
    let (_dir, gateway, events) = setup();
    let registry = gateway.0.compaction.clone();
    let session = digest(SESSION);
    let boundary = digest(BOUNDARY);
    seed_boundary(&registry, &session, &boundary);

    let log = RoutingLog::new(&gateway).unwrap();
    log.context(
        OLD,
        Some("fixture-model"),
        RoutingDecision::CompactionHandoff,
        evidence(Some(ThreadIdentity::Reliable), Some(BoundaryState::Claimed)),
        Some(ATTEMPT_TWO),
    );
    log.finish(Some(200), true);
    let lease = registry
        .prepare(session, Some(&boundary), true, &ids(&[NEW, OLD]))
        .expect("fixture handoff lease");
    lease.attach_routing_log(log);
    lease.complete(NEW, None);
    drop(lease);
    registry.flush();

    let rows = wait_for(&events, |rows| {
        rows.iter()
            .any(|row| row.record.reason == Reason::ThreadHandoff)
    });
    assert_eq!(
        rows.iter()
            .filter(|row| row.record.reason == Reason::ThreadHandoff)
            .count(),
        1
    );
    let row = rows
        .iter()
        .find(|row| row.record.reason == Reason::ThreadHandoff)
        .unwrap();
    assert_eq!(
        row.record
            .details
            .routing
            .as_ref()
            .map(|routing| routing.outcome),
        Some(RoutingOutcome::OwnershipSaved)
    );
}

#[test]
fn failed_ownership_save_emits_routing_error_without_thread_handoff() {
    let (dir, gateway, events) = setup();
    let registry = gateway.0.compaction.clone();
    let session = digest(SESSION);
    let boundary = digest(BOUNDARY);
    seed_boundary(&registry, &session, &boundary);

    let owner_path = dir.path().join("gateway/conversation-owners.json");
    std::fs::remove_file(&owner_path).unwrap();
    std::fs::create_dir(&owner_path).unwrap();

    let log = RoutingLog::new(&gateway).unwrap();
    log.context(
        OLD,
        Some("fixture-model"),
        RoutingDecision::CompactionHandoff,
        evidence(Some(ThreadIdentity::Reliable), Some(BoundaryState::Claimed)),
        Some(ATTEMPT_TWO),
    );
    log.finish(Some(200), true);
    let lease = registry
        .prepare(session, Some(&boundary), true, &ids(&[NEW, OLD]))
        .expect("fixture handoff lease");
    lease.attach_routing_log(log);
    lease.complete(NEW, None);
    drop(lease);
    registry.flush();

    let rows = wait_for(&events, |rows| {
        rows.iter().any(|row| {
            row.record.reason == Reason::ThreadRouting
                && row
                    .record
                    .details
                    .routing
                    .as_ref()
                    .map(|routing| routing.outcome == RoutingOutcome::SaveFailed)
                    .unwrap_or(false)
        })
    });
    assert!(!rows
        .iter()
        .any(|row| row.record.reason == Reason::ThreadHandoff));
    let row = rows
        .iter()
        .find(|row| row.record.reason == Reason::ThreadRouting)
        .unwrap();
    assert_eq!(row.record.level, events::Level::Error);
    assert_eq!(row.category, Category::OrdinaryError);
    assert_eq!(
        row.record
            .details
            .routing
            .as_ref()
            .map(|routing| routing.outcome),
        Some(RoutingOutcome::SaveFailed)
    );
}

#[test]
fn cancellation_release_is_logged_after_save_even_without_protocol_attachment() {
    let (_dir, gateway, events) = setup();
    let registry = gateway.0.compaction.clone();
    let session = digest(SESSION);
    let boundary = digest(BOUNDARY);
    seed_boundary(&registry, &session, &boundary);

    let log = RoutingLog::new(&gateway).unwrap();
    let lease = registry
        .prepare(session, Some(&boundary), true, &ids(&[NEW, OLD]))
        .expect("fixture handoff lease");
    let mut headers = HeaderMap::new();
    headers.insert("thread-id", SESSION.parse().unwrap());
    let hints = RequestHints::from_value(json!({"model":"fixture-model"}), false).unwrap();
    // pending() owns the pre-Protocol window; no Protocol.attach_routing_log call
    // is made here.  Lease::drop must retain it until its successful save.
    log.pending(&headers, Some(&hints), Some(&lease), false);
    log.finish(None, false);
    drop(log);
    drop(lease);
    registry.flush();

    let rows = wait_for(&events, |rows| {
        rows.iter().any(|row| {
            row.record
                .details
                .routing
                .as_ref()
                .map(|routing| routing.outcome == RoutingOutcome::WindowReleased)
                .unwrap_or(false)
        })
    });
    assert_eq!(
        rows.iter()
            .filter(|row| {
                row.record
                    .details
                    .routing
                    .as_ref()
                    .map(|routing| routing.outcome == RoutingOutcome::WindowReleased)
                    .unwrap_or(false)
            })
            .count(),
        1
    );
    let row = rows
        .iter()
        .find(|row| {
            row.record
                .details
                .routing
                .as_ref()
                .map(|routing| routing.outcome == RoutingOutcome::WindowReleased)
                .unwrap_or(false)
        })
        .unwrap();
    assert_eq!(
        row.record
            .details
            .routing
            .as_ref()
            .map(|routing| routing.decision),
        Some(RoutingDecision::CompactionHandoff)
    );
}

#[test]
fn legacy_details_unknown_enums_and_sensitive_attempt_ids_are_safe() {
    let dir = tempfile::tempdir().unwrap();
    let mut old = serde_json::to_value(route_record(
        RoutingDecision::ExistingThread,
        RoutingOutcome::OwnerPreserved,
        None,
    ))
    .unwrap();
    old.as_object_mut().unwrap().remove("details");
    std::fs::write(
        dir.path().join("diagnostics.jsonl"),
        format!("{}\n", serde_json::to_string(&old).unwrap()),
    )
    .unwrap();
    let legacy = events::Service::new(dir.path());
    let rows = legacy.query(events::Filter::default());
    assert_eq!(rows.total, 1);
    assert!(rows.items[0].record.details.routing.is_none());

    let mut unknown = serde_json::to_value(route_record(
        RoutingDecision::ExistingThread,
        RoutingOutcome::OwnerPreserved,
        Some("secret-attempt-id"),
    ))
    .unwrap();
    let routing = unknown["details"]["routing"].as_object_mut().unwrap();
    routing.insert("decision".into(), Value::String("future_decision".into()));
    routing.insert("outcome".into(), Value::String("future_outcome".into()));
    routing.insert(
        "attemptId".into(),
        Value::String("secret-attempt-id".into()),
    );
    let evidence = routing["evidence"].as_object_mut().unwrap();
    evidence.insert("input".into(), Value::String("future_input".into()));
    evidence.insert("identity".into(), Value::String("future_identity".into()));
    evidence.insert("boundary".into(), Value::String("future_boundary".into()));
    let unknown_dir = dir.path().join("unknown");
    std::fs::create_dir(&unknown_dir).unwrap();
    std::fs::write(
        unknown_dir.join("diagnostics.jsonl"),
        format!("{}\n", serde_json::to_string(&unknown).unwrap()),
    )
    .unwrap();
    let unknown_service = events::Service::new(&unknown_dir);
    let rows = wait_for(&unknown_service, |rows| rows.len() == 1);
    let routing = rows[0].record.details.routing.as_ref().unwrap();
    assert_eq!(routing.decision, RoutingDecision::Unknown);
    assert_eq!(routing.outcome, RoutingOutcome::Unknown);
    assert_eq!(routing.evidence.input, RoutingInput::MetadataUnavailable);
    assert_eq!(routing.evidence.identity, Some(ThreadIdentity::Unknown));
    assert_eq!(routing.evidence.boundary, Some(BoundaryState::Unknown));
    assert!(
        routing.attempt_id.is_none(),
        "sensitive non-UUID attempt id must be cleared"
    );
}

#[test]
fn different_routing_reasons_are_not_merged_by_event_service() {
    let dir = tempfile::tempdir().unwrap();
    let service = events::Service::new(dir.path());
    service.emit(route_record(
        RoutingDecision::BoundaryNotObserved,
        RoutingOutcome::Attempted,
        Some(ATTEMPT_ONE),
    ));
    service.emit(route_record(
        RoutingDecision::BoundaryConsumed,
        RoutingOutcome::Attempted,
        Some(ATTEMPT_TWO),
    ));
    let rows = wait_for(&service, |rows| rows.len() == 2);
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().any(|row| {
        row.record
            .details
            .routing
            .as_ref()
            .map(|routing| routing.decision == RoutingDecision::BoundaryNotObserved)
            .unwrap_or(false)
    }));
    assert!(rows.iter().any(|row| {
        row.record
            .details
            .routing
            .as_ref()
            .map(|routing| routing.decision == RoutingDecision::BoundaryConsumed)
            .unwrap_or(false)
    }));
}
