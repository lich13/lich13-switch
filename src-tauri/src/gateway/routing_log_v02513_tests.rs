//! v0.25.13 routing outcome and durable save ordering fixtures.
//!
//! These tests use only bounded fixture identifiers.  Diagnostics are checked
//! after both the ownership writer and the diagnostics queue have settled.

use super::super::{compaction::Registry, failover::Failure, protocol::Protocol};
use super::RoutingLog;
use crate::{
    events::{self, Action, Filter, Reason, Record, RoutingOutcome},
    gateway::{ClientId, Gateway},
    usage::model::{BoundaryState, RoutingDecision, RoutingEvidence, RoutingInput, ThreadIdentity},
    usage::{
        model::{Filter as UsageFilter, Record as UsageRecord},
        Service as UsageService,
    },
};
use std::{path::Path, sync::Arc, time::Duration};

const OLD: &str = "00000000-0000-0000-0000-000000000001";
const MODEL: &str = "fixture-model";

fn digest(value: &str) -> String {
    crate::storage::digest(value.as_bytes())
}

fn evidence(input: RoutingInput, boundary: BoundaryState) -> RoutingEvidence {
    RoutingEvidence {
        input,
        identity: Some(ThreadIdentity::Reliable),
        boundary: Some(boundary),
        ..Default::default()
    }
}

fn setup() -> (tempfile::TempDir, Gateway, events::Service) {
    let dir = tempfile::tempdir().unwrap();
    let gateway = Gateway::new(dir.path().join("gateway")).unwrap();
    let service = events::Service::new(&dir.path().join("events"));
    gateway.set_diagnostics(service.clone());
    gateway
        .0
        .compaction
        .notifications(ClientId::Codex, service.clone());
    (dir, gateway, service)
}

async fn diagnostic_barrier(
    service: &events::Service,
    receiver: &mut tokio::sync::broadcast::Receiver<events::Change>,
) -> Vec<events::Change> {
    let mut sentinel = Record::new(
        Some(ClientId::Codex),
        Some(OLD),
        Some(MODEL),
        Reason::Network,
        Action::Returned,
        None,
        None,
    );
    let sentinel_id = sentinel.id.clone();
    sentinel.details.message = Some(format!("v02513-diagnostic-barrier-{sentinel_id}"));
    service.emit(sentinel);

    tokio::time::timeout(Duration::from_secs(30), async {
        let mut changes = Vec::new();
        loop {
            let change = receiver
                .recv()
                .await
                .expect("diagnostic event channel closed before queue barrier");
            assert!(change.error.is_none(), "diagnostic persistence failed");
            if change
                .record
                .as_ref()
                .is_some_and(|record| record.id == sentinel_id)
            {
                assert!(!change.notify, "synthetic barrier must not notify");
                return changes;
            }
            changes.push(change);
        }
    })
    .await
    .expect("diagnostic queue barrier timed out")
}

fn route_rows(service: &events::Service) -> Vec<events::ClassifiedRecord> {
    let page = service.query(Filter {
        reason: Some(Reason::ThreadRouting),
        ..Default::default()
    });
    assert_eq!(page.error, None, "diagnostic query failed");
    page.items
}

fn persisted_rows(path: &Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(path.join("diagnostics.jsonl"))
        .expect("diagnostic JSONL should exist after queue barrier")
        .lines()
        .map(|line| serde_json::from_str(line).expect("diagnostic JSONL should be valid JSON"))
        .collect()
}

fn assert_no_routing_events(
    dir: &tempfile::TempDir,
    service: &events::Service,
    changes: &[events::Change],
) {
    assert_eq!(
        changes
            .iter()
            .filter(|change| change
                .record
                .as_ref()
                .is_some_and(|record| record.reason == Reason::ThreadRouting))
            .count(),
        0,
        "unexpected routing event in diagnostics queue"
    );
    assert!(changes.iter().all(|change| !change.notify));
    assert!(
        route_rows(service).is_empty(),
        "routing event reached memory"
    );
    assert!(persisted_rows(&dir.path().join("events"))
        .iter()
        .all(|row| {
            row.get("reason") != Some(&serde_json::Value::String("thread_routing".into()))
        }));
    let reopened = events::Service::new(&dir.path().join("events"));
    assert!(
        route_rows(&reopened).is_empty(),
        "routing event reached reopen"
    );
}

fn seed_boundary(registry: &Arc<Registry>, session: &str, boundary: &str) {
    let owner = registry
        .prepare(session.into(), None, false, &[OLD.into()])
        .expect("fixture owner lease");
    owner.complete(OLD, None);
    owner.complete(OLD, Some(boundary));
    registry.flush();
}

fn routing_log(
    gateway: &Gateway,
    decision: RoutingDecision,
    boundary: BoundaryState,
) -> Arc<RoutingLog> {
    let log = RoutingLog::new(gateway).expect("fixture routing log");
    log.context(
        OLD,
        Some(MODEL),
        decision,
        evidence(RoutingInput::Compaction, boundary),
        None,
    );
    log
}

#[tokio::test]
async fn boundary_not_observed_normal_close_is_silent_after_barrier() {
    let (dir, gateway, service) = setup();
    let mut receiver = service.subscribe();
    for _ in 0..5 {
        let log = routing_log(
            &gateway,
            RoutingDecision::BoundaryNotObserved,
            BoundaryState::NotObserved,
        );
        log.finish(Some(200), true);
        drop(log);
    }
    let changes = diagnostic_barrier(&service, &mut receiver).await;
    assert_no_routing_events(&dir, &service, &changes);
}

#[tokio::test]
async fn boundary_not_observed_identified_downstream_close_is_silent() {
    let (dir, gateway, service) = setup();
    let mut receiver = service.subscribe();
    for index in 0..5 {
        let log = routing_log(
            &gateway,
            RoutingDecision::BoundaryNotObserved,
            BoundaryState::NotObserved,
        );
        let mut protocol = Protocol::new(true);
        protocol.diagnostics(&gateway, OLD, Some(MODEL));
        protocol.attach_routing_log(Some(log.clone()));
        protocol.context(None, Some(RoutingDecision::BoundaryNotObserved));
        protocol.routing_evidence(None);
        protocol.response(200, true, "identity");
        let payload = format!(
            "data: {{\"type\":\"response.completed\",\"response\":{{\"id\":\"fixture-response-{index}\",\"status\":\"completed\"}}}}\n\n"
        );
        protocol.feed(payload.as_bytes());
        protocol.delivery().handed(payload.len(), true);
        protocol.delivery().failed(4);
        protocol.finish(Some(200), crate::errors::code::OK);
        drop(protocol);
        drop(log);
    }
    let changes = diagnostic_barrier(&service, &mut receiver).await;
    assert_no_routing_events(&dir, &service, &changes);
}

#[tokio::test]
async fn boundary_not_observed_duplicate_fault_is_silent_when_fault_is_logged() {
    let (dir, gateway, service) = setup();
    let mut receiver = service.subscribe();
    for index in 0..5 {
        let log = routing_log(
            &gateway,
            RoutingDecision::BoundaryNotObserved,
            BoundaryState::NotObserved,
        );
        let mut protocol = Protocol::new(true);
        protocol.diagnostics(&gateway, OLD, Some(MODEL));
        protocol.attach_routing_log(Some(log.clone()));
        protocol.context(None, Some(RoutingDecision::BoundaryNotObserved));
        protocol.routing_evidence(None);
        protocol.response(200, true, "identity");
        let payload = format!(
            "data: {{\"type\":\"response.completed\",\"response\":{{\"id\":\"fixture-fault-response-{index}\",\"status\":\"completed\"}}}}\n\n"
        );
        protocol.feed(payload.as_bytes());
        protocol.delivery().handed(payload.len(), true);
        protocol.delivery().failed(3);
        protocol.finish(Some(200), crate::errors::code::OK);
        drop(protocol);
        drop(log);
    }
    let changes = diagnostic_barrier(&service, &mut receiver).await;
    let interruptions = changes
        .iter()
        .filter(|change| {
            change
                .record
                .as_ref()
                .is_some_and(|record| record.reason == Reason::RequestInterrupted)
        })
        .count();
    assert_eq!(
        interruptions, 5,
        "the independently logged faults are missing"
    );
    assert_no_routing_events(&dir, &service, &changes);
}

#[tokio::test]
async fn same_owner_boundary_registered_save_success_is_silent() {
    let (dir, gateway, service) = setup();
    let registry = gateway.0.compaction.clone();
    let usage = UsageService::new(&dir.path().join("usage"));
    gateway.set_usage(usage.clone());
    let mut usage_events = usage.subscribe();
    let mut receiver = service.subscribe();

    for index in 0..5 {
        let session = digest(&format!("fixture-boundary-registered-session-{index}"));
        let first = digest(&format!("fixture-boundary-registered-first-{index}"));
        let next = digest(&format!("fixture-boundary-registered-next-{index}"));
        seed_boundary(&registry, &session, &first);
        let log = routing_log(
            &gateway,
            RoutingDecision::ExistingThread,
            BoundaryState::Fresh,
        );
        let lease = registry
            .prepare(session, None, false, &[OLD.into()])
            .expect("fixture same-owner lease");
        lease.attach_routing_log(log.clone());
        let trace = usage.begin("codex", Some(MODEL));
        let mut attempt = trace.attempt(OLD, true, "http");
        lease.attach_usage(attempt.routing_completion());
        log.finish(Some(200), true);
        lease.complete(OLD, Some(&next));
        drop(attempt);
        drop(trace);
        registry.flush();
        drop(log);
        drop(lease);
    }

    let rows = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let rows = usage_rows(&usage);
            if rows.len() >= 5 {
                return rows;
            }
            usage_events.recv().await.expect("usage writer closed");
        }
    })
    .await
    .expect("same-owner usage records did not settle");
    assert_eq!(rows.len(), 5);
    assert!(rows.iter().all(|row| {
        row.final_attempt()
            .and_then(|attempt| attempt.routing_outcome)
            == Some(RoutingOutcome::BoundaryRegistered)
    }));
    let changes = diagnostic_barrier(&service, &mut receiver).await;
    assert_no_routing_events(&dir, &service, &changes);
}

#[tokio::test]
async fn same_owner_boundary_registered_save_failure_is_visible() {
    let (dir, gateway, service) = setup();
    let registry = gateway.0.compaction.clone();
    let session = digest("fixture-boundary-save-failure-session");
    let first = digest("fixture-boundary-save-failure-first");
    let next = digest("fixture-boundary-save-failure-next");
    seed_boundary(&registry, &session, &first);
    let owners = dir.path().join("gateway/conversation-owners.json");
    std::fs::remove_file(&owners).unwrap();
    std::fs::create_dir(&owners).unwrap();
    let mut receiver = service.subscribe();

    let log = routing_log(
        &gateway,
        RoutingDecision::ExistingThread,
        BoundaryState::Fresh,
    );
    let lease = registry
        .prepare(session, None, false, &[OLD.into()])
        .expect("fixture same-owner lease");
    lease.attach_routing_log(log.clone());
    log.finish(Some(200), true);
    lease.complete(OLD, Some(&next));
    drop(log);
    drop(lease);
    registry.flush();
    let changes = diagnostic_barrier(&service, &mut receiver).await;
    let rows = route_rows(&service);
    assert_eq!(rows.len(), 1, "save failure should be visible exactly once");
    let routing = rows[0].record.details.routing.as_ref().unwrap();
    assert_eq!(routing.outcome, RoutingOutcome::SaveFailed);
    assert_eq!(rows[0].record.level, events::Level::Error);
    assert!(changes.iter().any(|change| {
        change
            .record
            .as_ref()
            .is_some_and(|record| record.reason == Reason::ThreadRouting)
    }));
    let persisted = persisted_rows(&dir.path().join("events"));
    assert_eq!(
        persisted
            .iter()
            .filter(|row| row["reason"] == "thread_routing")
            .count(),
        1
    );
    assert!(changes.iter().all(|change| {
        !change
            .record
            .as_ref()
            .is_some_and(|record| record.reason == Reason::ThreadHandoff)
    }));
}

#[tokio::test]
async fn late_fault_after_successful_boundary_save_remains_visible() {
    let (dir, gateway, service) = setup();
    let registry = gateway.0.compaction.clone();
    let session = digest("fixture-late-fault-session");
    let first = digest("fixture-late-fault-first");
    let next = digest("fixture-late-fault-next");
    seed_boundary(&registry, &session, &first);
    let mut receiver = service.subscribe();

    let log = routing_log(
        &gateway,
        RoutingDecision::ExistingThread,
        BoundaryState::Fresh,
    );
    let lease = registry
        .prepare(session, None, false, &[OLD.into()])
        .expect("fixture same-owner lease");
    lease.attach_routing_log(log.clone());
    log.finish(Some(200), true);
    lease.complete(OLD, Some(&next));
    // This fault arrives after the save was queued.  The ownership saver must
    // retain the Arc<RoutingLog> until its receipt settles before filtering.
    let generation = log.failure(Failure::Transport);
    assert!(generation > 0);
    drop(log);
    drop(lease);
    registry.flush();
    let changes = diagnostic_barrier(&service, &mut receiver).await;
    let rows = route_rows(&service);
    assert_eq!(rows.len(), 1, "late fault should keep one route record");
    let routing = rows[0].record.details.routing.as_ref().unwrap();
    assert_eq!(routing.outcome, RoutingOutcome::BoundaryRegistered);
    assert_eq!(routing.failure, Some(Failure::Transport));
    assert!(changes.iter().all(|change| !change.notify));
    let reopened = events::Service::new(&dir.path().join("events"));
    assert_eq!(route_rows(&reopened).len(), 1);
}

#[tokio::test]
async fn stale_saved_receipt_cannot_replace_the_newer_routing_outcome() {
    let (dir, gateway, service) = setup();
    let mut receiver = service.subscribe();
    let log = routing_log(
        &gateway,
        RoutingDecision::CompactionHandoff,
        BoundaryState::Claimed,
    );
    log.finish(Some(200), true);
    let older = log.begin_save();
    let newer = log.begin_save();
    let newer_scope = digest("fixture-newer-scope");
    let older_scope = digest("fixture-older-scope");
    log.saved(
        newer,
        RoutingOutcome::WindowReleased,
        &newer_scope,
        false,
        None,
    );
    log.saved(
        older,
        RoutingOutcome::OwnershipSaved,
        &older_scope,
        false,
        None,
    );
    drop(log);

    let changes = diagnostic_barrier(&service, &mut receiver).await;
    assert!(changes.iter().all(|change| !change.notify));
    let routes: Vec<_> = changes
        .into_iter()
        .filter_map(|change| change.record)
        .filter(|record| record.reason == Reason::ThreadRouting)
        .collect();
    assert_eq!(routes.len(), 1);
    let routing = routes[0].details.routing.as_ref().unwrap();
    assert_eq!(routing.outcome, RoutingOutcome::WindowReleased);
    assert_eq!(
        routes[0].details.handoff_scope.as_deref(),
        Some(newer_scope.as_str())
    );
    let persisted = persisted_rows(&dir.path().join("events"));
    let persisted_route = persisted
        .iter()
        .find(|row| row["reason"] == "thread_routing")
        .expect("newer routing outcome should persist");
    assert_eq!(
        persisted_route["details"]["routing"]["outcome"],
        "window_released"
    );
}

fn usage_rows(service: &UsageService) -> Vec<UsageRecord> {
    service
        .read(|store| Ok(store.logs(&UsageFilter::default())?.rows))
        .unwrap()
}

#[tokio::test]
async fn no_receiver_window_consumed_is_quiet_with_usage_outcome() {
    let (dir, gateway, service) = setup();
    let mut diagnostics = service.subscribe();
    let usage = UsageService::new(&dir.path().join("usage"));
    let mut usage_events = usage.subscribe();
    gateway.set_usage(usage.clone());
    for index in 0..5 {
        let session = digest(&format!("fixture-window-consumed-session-{index}"));
        let boundary = digest(&format!("fixture-window-consumed-boundary-{index}"));
        seed_boundary(&gateway.0.compaction, &session, &boundary);
        let lease = gateway
            .0
            .compaction
            .prepare(session.clone(), Some(&boundary), false, &[OLD.into()])
            .expect("fixture continuation lease");
        let log = routing_log(
            &gateway,
            RoutingDecision::BoundaryConsumed,
            BoundaryState::Fresh,
        );
        let mut protocol = Protocol::new(true);
        protocol.diagnostics(&gateway, OLD, Some(MODEL));
        protocol.attach_ownership(Some(lease), OLD);
        protocol.attach_routing_log(Some(log.clone()));
        let trace = usage.begin("codex", Some(MODEL));
        protocol.attach_usage(trace.attempt(OLD, true, "http"));
        protocol.context(Some(session), Some(RoutingDecision::BoundaryConsumed));
        protocol.routing_evidence(None);
        protocol.response(200, true, "identity");
        let payload = format!(
            "data: {{\"type\":\"response.completed\",\"response\":{{\"id\":\"fixture-window-response-{index}\",\"status\":\"completed\"}}}}\n\n"
        );
        protocol.feed(payload.as_bytes());
        protocol.delivery().handed(payload.len(), true);
        protocol.output_handed(true);
        protocol.finish(Some(200), crate::errors::code::OK);
        drop(protocol);
        drop(log);
        drop(trace);
        gateway.0.compaction.flush();
    }

    let rows = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let rows = usage_rows(&usage);
            if rows.len() >= 5 {
                return rows;
            }
            usage_events.recv().await.expect("usage writer closed");
        }
    })
    .await
    .expect("usage record did not settle");
    assert_eq!(rows.len(), 5);
    for row in rows {
        let attempt = row.final_attempt().expect("fixture usage attempt");
        let json = serde_json::to_value(attempt).unwrap();
        assert_eq!(json["routingOutcome"], "window_consumed");
    }
    let changes = diagnostic_barrier(&service, &mut diagnostics).await;
    assert_no_routing_events(&dir, &service, &changes);
}
