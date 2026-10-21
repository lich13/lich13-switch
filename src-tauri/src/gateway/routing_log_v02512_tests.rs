//! v0.25.12 successful routing event policy regressions.
//!
//! Each ordinary success is repeated five times. A diagnostic sentinel on the
//! same queue settles all earlier writes before memory, JSONL, reopen, and
//! broadcast notifications are checked.

use super::super::{
    failover::Failure,
    health_settlement::Reported,
    protocol::{Protocol, RoutingSettlement},
};
use super::RoutingLog;
use crate::{
    events::{self, Action, Filter, Reason, Record, RoutingOutcome},
    gateway::{ClientId, Gateway},
    usage::model::{
        BoundaryState, RoutingDecision, RoutingEvidence, RoutingInput, RoutingRestriction,
        ThreadIdentity,
    },
};
use std::{path::PathBuf, sync::Arc, time::Duration};

const PROVIDER: &str = "00000000-0000-0000-0000-000000000001";
const MODEL: &str = "fixture-model";
const REPEATS: usize = 5;

#[derive(Clone, Copy)]
struct SuccessCase {
    decision: RoutingDecision,
    input: RoutingInput,
    identity: ThreadIdentity,
    boundary: Option<BoundaryState>,
    restriction: Option<RoutingRestriction>,
}

impl SuccessCase {
    fn evidence(self) -> RoutingEvidence {
        RoutingEvidence {
            input: self.input,
            identity: Some(self.identity),
            boundary: self.boundary,
            restriction: self.restriction,
            ..Default::default()
        }
    }
}

async fn settle_diagnostic_queue(
    service: &events::Service,
    receiver: &mut tokio::sync::broadcast::Receiver<events::Change>,
) -> Vec<events::Change> {
    let mut sentinel = Record::new(
        Some(ClientId::Codex),
        Some(PROVIDER),
        Some(MODEL),
        Reason::Network,
        Action::Returned,
        None,
        None,
    );
    let sentinel_id = sentinel.id.clone();
    sentinel.details.message = Some(format!("v02512-diagnostic-barrier-{sentinel_id}"));
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

fn routing_count_in_memory(service: &events::Service) -> usize {
    let page = service.query(Filter {
        reason: Some(Reason::ThreadRouting),
        ..Default::default()
    });
    assert_eq!(page.error, None, "diagnostic query failed after barrier");
    page.total
}

fn read_persisted_records(events_dir: &std::path::Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(events_dir.join("diagnostics.jsonl"))
        .expect("diagnostic JSONL should be durable after barrier")
        .lines()
        .map(|line| serde_json::from_str(line).expect("diagnostic JSONL line should be valid JSON"))
        .collect()
}

struct DiagnosticFixture {
    _dir: tempfile::TempDir,
    events_dir: PathBuf,
    gateway: Gateway,
    service: events::Service,
    receiver: tokio::sync::broadcast::Receiver<events::Change>,
}

impl DiagnosticFixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let gateway = Gateway::new(dir.path().join("gateway")).unwrap();
        let events_dir = dir.path().join("events");
        let service = events::Service::new(&events_dir);
        gateway.set_diagnostics(service.clone());
        let receiver = service.subscribe();
        Self {
            _dir: dir,
            events_dir,
            gateway,
            service,
            receiver,
        }
    }

    fn log(&self, decision: RoutingDecision, evidence: RoutingEvidence) -> Arc<RoutingLog> {
        let log = RoutingLog::new(&self.gateway).expect("fixture diagnostic routing log");
        log.context(PROVIDER, Some(MODEL), decision, evidence, None);
        log
    }

    async fn settle(&mut self) -> DiagnosticSnapshot {
        let changes = settle_diagnostic_queue(&self.service, &mut self.receiver).await;
        let memory_page = self.service.query(Filter::default());
        assert_eq!(
            memory_page.error, None,
            "diagnostic query failed after barrier"
        );
        let persisted = read_persisted_records(&self.events_dir);
        let reopened = events::Service::new(&self.events_dir).query(Filter::default());
        assert_eq!(
            reopened.error, None,
            "reopened diagnostics could not be queried"
        );
        DiagnosticSnapshot {
            changes,
            memory: memory_page.items,
            persisted,
            reopened: reopened.items,
        }
    }
}

struct DiagnosticSnapshot {
    changes: Vec<events::Change>,
    memory: Vec<events::ClassifiedRecord>,
    persisted: Vec<serde_json::Value>,
    reopened: Vec<events::ClassifiedRecord>,
}

fn evidence(
    input: RoutingInput,
    identity: Option<ThreadIdentity>,
    boundary: Option<BoundaryState>,
    restriction: Option<RoutingRestriction>,
) -> RoutingEvidence {
    RoutingEvidence {
        input,
        identity,
        boundary,
        restriction,
        ..Default::default()
    }
}

fn record_count(records: &[events::ClassifiedRecord], reason: Reason) -> usize {
    records
        .iter()
        .filter(|row| row.record.reason == reason)
        .count()
}

fn persisted_count(records: &[serde_json::Value], reason: Reason) -> usize {
    let reason = serde_json::to_value(reason).unwrap();
    records
        .iter()
        .filter(|record| record["reason"] == reason)
        .count()
}

fn assert_reason_count(snapshot: &DiagnosticSnapshot, reason: Reason, expected: usize) {
    let changes = snapshot
        .changes
        .iter()
        .filter(|change| {
            change
                .record
                .as_ref()
                .is_some_and(|record| record.reason == reason)
        })
        .count();
    let counts = [
        changes,
        record_count(&snapshot.memory, reason),
        persisted_count(&snapshot.persisted, reason),
        record_count(&snapshot.reopened, reason),
    ];
    assert_eq!(counts, [expected; 4], "unexpected {reason:?} counts");
    let notifications = snapshot
        .changes
        .iter()
        .filter(|change| change.notify)
        .count();
    assert_eq!(notifications, 0, "synthetic routing diagnostics notified");
}

fn has_persisted_route(
    snapshot: &DiagnosticSnapshot,
    decision: RoutingDecision,
    identity: Option<ThreadIdentity>,
    restriction: Option<RoutingRestriction>,
    outcome: RoutingOutcome,
) -> bool {
    let decision = serde_json::to_value(decision).unwrap();
    let identity = identity.map_or(serde_json::Value::Null, |value| {
        serde_json::to_value(value).unwrap()
    });
    let restriction = restriction.map_or(serde_json::Value::Null, |value| {
        serde_json::to_value(value).unwrap()
    });
    let outcome = serde_json::to_value(outcome).unwrap();
    snapshot.persisted.iter().any(|record| {
        record["reason"] == "thread_routing"
            && record["details"]["routing"]["decision"] == decision
            && record["details"]["routing"]["evidence"]["identity"] == identity
            && record["details"]["routing"]["evidence"]["restriction"] == restriction
            && record["details"]["routing"]["outcome"] == outcome
    })
}

fn queue_failure_report(
    service: &events::Service,
    reason: Reason,
    status: Option<u16>,
) -> Reported {
    let record = Record::new(
        Some(ClientId::Codex),
        Some(PROVIDER),
        Some(MODEL),
        reason,
        Action::Returned,
        status,
        Some(1),
    );
    let queued = service.emit_checked(record.clone());
    Reported { record, queued }
}

fn row_has_route(
    rows: &[events::ClassifiedRecord],
    decision: RoutingDecision,
    identity: Option<ThreadIdentity>,
    restriction: Option<RoutingRestriction>,
    outcome: RoutingOutcome,
) -> bool {
    rows.iter().any(|row| {
        row.record.details.routing.as_ref().is_some_and(|routing| {
            routing.decision == decision
                && routing.evidence.identity == identity
                && routing.evidence.restriction == restriction
                && routing.outcome == outcome
        })
    })
}

fn broadcast_has_route(
    changes: &[events::Change],
    decision: RoutingDecision,
    identity: Option<ThreadIdentity>,
    restriction: Option<RoutingRestriction>,
    outcome: RoutingOutcome,
) -> bool {
    changes.iter().any(|change| {
        change
            .record
            .as_ref()
            .and_then(|record| record.details.routing.as_ref())
            .is_some_and(|routing| {
                routing.decision == decision
                    && routing.evidence.identity == identity
                    && routing.evidence.restriction == restriction
                    && routing.outcome == outcome
            })
    })
}

#[tokio::test]
async fn ordinary_successful_routing_results_are_silent_after_five_repeats() {
    let dir = tempfile::tempdir().unwrap();
    let gateway = Gateway::new(dir.path().join("gateway")).unwrap();
    let events_dir = dir.path().join("events");
    let service = events::Service::new(&events_dir);
    gateway.set_diagnostics(service.clone());
    let mut receiver = service.subscribe();

    let cases = [
        SuccessCase {
            decision: RoutingDecision::ExistingThread,
            input: RoutingInput::History,
            identity: ThreadIdentity::Reliable,
            boundary: Some(BoundaryState::NotObserved),
            restriction: None,
        },
        SuccessCase {
            decision: RoutingDecision::BoundaryConsumed,
            input: RoutingInput::Compaction,
            identity: ThreadIdentity::Reliable,
            boundary: Some(BoundaryState::Consumed),
            restriction: None,
        },
        SuccessCase {
            decision: RoutingDecision::NewThread,
            input: RoutingInput::FirstTurn,
            identity: ThreadIdentity::Reliable,
            boundary: Some(BoundaryState::Fresh),
            restriction: None,
        },
        SuccessCase {
            decision: RoutingDecision::FirstTurnUnconfirmed,
            input: RoutingInput::FirstTurn,
            identity: ThreadIdentity::Reliable,
            boundary: Some(BoundaryState::NotObserved),
            restriction: None,
        },
        SuccessCase {
            decision: RoutingDecision::OrdinaryQueue,
            input: RoutingInput::History,
            identity: ThreadIdentity::Reliable,
            boundary: Some(BoundaryState::NotObserved),
            restriction: None,
        },
        SuccessCase {
            decision: RoutingDecision::Auxiliary,
            input: RoutingInput::Warmup,
            identity: ThreadIdentity::Reliable,
            boundary: None,
            restriction: None,
        },
        SuccessCase {
            decision: RoutingDecision::ModelDetourOwnerPreserved,
            input: RoutingInput::History,
            identity: ThreadIdentity::Reliable,
            boundary: Some(BoundaryState::NotObserved),
            restriction: None,
        },
        SuccessCase {
            decision: RoutingDecision::ModelDetourOwnerPreserved,
            input: RoutingInput::History,
            identity: ThreadIdentity::Reliable,
            boundary: Some(BoundaryState::NotObserved),
            restriction: Some(RoutingRestriction::ModelMismatch),
        },
        SuccessCase {
            decision: RoutingDecision::ThreadUnidentified,
            input: RoutingInput::History,
            identity: ThreadIdentity::Missing,
            boundary: None,
            restriction: None,
        },
    ];

    for case in cases.iter().copied() {
        for _ in 0..REPEATS {
            let log = RoutingLog::new(&gateway).expect("fixture diagnostic routing log");
            log.context(PROVIDER, Some(MODEL), case.decision, case.evidence(), None);
            log.finish(Some(200), true);
            drop(log);
        }
    }

    let changes = settle_diagnostic_queue(&service, &mut receiver).await;
    let emitted_routing: Vec<_> = changes
        .iter()
        .filter_map(|change| {
            change.record.as_ref().and_then(|record| {
                (record.reason == Reason::ThreadRouting).then(|| {
                    format!(
                        "decision={:?}, evidence={:?}, notify={}",
                        record
                            .details
                            .routing
                            .as_ref()
                            .map(|routing| routing.decision),
                        record
                            .details
                            .routing
                            .as_ref()
                            .map(|routing| &routing.evidence),
                        change.notify
                    )
                })
            })
        })
        .collect();

    let in_memory_count = routing_count_in_memory(&service);
    let persisted = read_persisted_records(&events_dir);
    let persisted_routing_count = persisted
        .iter()
        .filter(|record| record["reason"] == "thread_routing")
        .count();
    let reopened = events::Service::new(&events_dir);
    let reopened_count = routing_count_in_memory(&reopened);

    let notification_count = changes.iter().filter(|change| change.notify).count();
    assert!(
        notification_count == 0
            && emitted_routing.is_empty()
            && in_memory_count == 0
            && persisted_routing_count == 0
            && reopened_count == 0,
        "ordinary success must stay silent across the settled queue: notifications={notification_count}, broadcast routing events={}, in-memory={in_memory_count}, persisted={persisted_routing_count}, reopened={reopened_count}; emitted={emitted_routing:?}",
        emitted_routing.len()
    );
}

#[tokio::test]
async fn queued_protocol_failure_settles_the_current_routing_generation_silently() {
    let mut fixture = DiagnosticFixture::new();
    let log = fixture.log(
        RoutingDecision::NewThread,
        evidence(
            RoutingInput::FirstTurn,
            Some(ThreadIdentity::Reliable),
            Some(BoundaryState::Fresh),
            None,
        ),
    );
    let generation = log.failure(Failure::Transport);
    assert_eq!(generation, 1, "first failure must receive generation one");

    let mut protocol = Protocol::new(false);
    protocol.attach_routing_log(Some(log.clone()));
    protocol.response(502, false, "identity");
    protocol.associate_failure(generation);
    protocol.feed(
        br#"{"error":{"type":"server_error","code":"fixture_upstream_error","message":"synthetic fixture failure"}}"#,
    );
    protocol.finish(Some(502), crate::errors::code::HTTP);
    let report = protocol.report_failure(|| {
        queue_failure_report(&fixture.service, Reason::UpstreamService, Some(502))
    });
    assert!(
        report.queued,
        "synthetic failure report should enter the queue"
    );
    assert_eq!(report.record.reason, Reason::UpstreamService);
    assert!(
        log.failure_reported(),
        "the queued report should cover generation one"
    );

    drop(protocol);
    drop(log);
    let snapshot = fixture.settle().await;
    assert_reason_count(&snapshot, Reason::UpstreamService, 1);
    assert_reason_count(&snapshot, Reason::ThreadRouting, 0);
    assert_reason_count(&snapshot, Reason::RequestInterrupted, 0);
}

#[tokio::test]
async fn a_repeated_identical_protocol_error_after_its_report_remains_uncovered() {
    let mut fixture = DiagnosticFixture::new();
    let log = fixture.log(
        RoutingDecision::NewThread,
        evidence(
            RoutingInput::FirstTurn,
            Some(ThreadIdentity::Reliable),
            Some(BoundaryState::Fresh),
            None,
        ),
    );
    let mut protocol = Protocol::new(true);
    protocol.attach_routing_log(Some(log.clone()));
    protocol.websocket_status(101);
    let repeated_error = serde_json::json!({
        "type": "error",
        "error": {
            "type": "server_error",
            "code": "fixture_protocol_error",
            "message": "synthetic repeated protocol failure"
        }
    });

    protocol.value(&repeated_error);
    let generation = log.failure(Failure::Protocol);
    assert_eq!(generation, 1);
    protocol.associate_failure(generation);
    let first_report = protocol
        .report_failure(|| queue_failure_report(&fixture.service, Reason::ProtocolError, None));
    assert!(
        first_report.queued,
        "first protocol error should be accepted"
    );
    assert!(log.failure_reported());

    protocol.value(&repeated_error);
    protocol.delivery().failed(5); // Known downstream close.
    drop(protocol);
    drop(log);

    let snapshot = fixture.settle().await;
    assert_reason_count(&snapshot, Reason::ProtocolError, 1);
    assert_reason_count(&snapshot, Reason::ThreadRouting, 1);
    assert_reason_count(&snapshot, Reason::RequestInterrupted, 0);
    let route = snapshot
        .reopened
        .iter()
        .find(|row| row.record.reason == Reason::ThreadRouting)
        .and_then(|row| row.record.details.routing.as_ref())
        .expect("second identical error should keep one routing result");
    assert_eq!(route.failure, Some(Failure::Protocol));
    assert_eq!(route.outcome, RoutingOutcome::Failed);
}

#[tokio::test]
async fn a_rejected_failure_report_does_not_cover_the_routing_generation() {
    let mut fixture = DiagnosticFixture::new();
    let mut tracker = super::super::failover::Tracker::new(None);
    tracker.diagnostics(&fixture.gateway);
    let log = tracker
        .routing()
        .expect("tracker should own routing diagnostics");
    log.context(
        PROVIDER,
        Some(MODEL),
        RoutingDecision::NewThread,
        evidence(
            RoutingInput::FirstTurn,
            Some(ThreadIdentity::Reliable),
            Some(BoundaryState::Fresh),
            None,
        ),
        None,
    );

    tracker.failure(Failure::Transport);
    let mut protocol = Protocol::new(false);
    protocol.attach_routing_log(Some(log.clone()));
    protocol.response(502, false, "identity");
    let report = tracker.report_failure(Some(&protocol), || Reported {
        record: Record::new(
            Some(ClientId::Codex),
            Some(PROVIDER),
            Some(MODEL),
            Reason::UpstreamService,
            Action::Returned,
            Some(502),
            Some(1),
        ),
        queued: false,
    });
    assert!(!report.queued, "fixture models a rejected queue receipt");
    assert!(
        !tracker.failure_reported(),
        "an unaccepted report cannot cover the current failure"
    );
    tracker.finish(super::super::failover::Exit::AttemptsExhausted);

    drop(protocol);
    drop(log);
    drop(tracker);
    let snapshot = fixture.settle().await;
    assert_reason_count(&snapshot, Reason::UpstreamService, 0);
    assert_reason_count(&snapshot, Reason::ThreadRouting, 1);
    let route = snapshot
        .reopened
        .iter()
        .find(|row| row.record.reason == Reason::ThreadRouting)
        .and_then(|row| row.record.details.routing.as_ref())
        .expect("unreported failure should remain after reopen");
    assert_eq!(route.failure, Some(Failure::Transport));
    assert_eq!(route.outcome, RoutingOutcome::Failed);
}

#[tokio::test]
async fn a_late_report_for_an_old_failure_generation_cannot_hide_the_new_failure() {
    let mut fixture = DiagnosticFixture::new();
    let log = fixture.log(
        RoutingDecision::NewThread,
        evidence(
            RoutingInput::FirstTurn,
            Some(ThreadIdentity::Reliable),
            Some(BoundaryState::Fresh),
            None,
        ),
    );

    let first_generation = log.failure(Failure::Transport);
    assert_eq!(first_generation, 1);
    let first_report = queue_failure_report(&fixture.service, Reason::ProtocolError, Some(502));
    assert!(
        first_report.queued,
        "first synthetic report should be accepted"
    );
    log.reported(first_generation);
    assert!(log.failure_reported());

    let second_generation = log.failure(Failure::Protocol);
    assert_eq!(second_generation, 2);
    assert!(!log.failure_reported(), "a new failure starts uncovered");
    log.reported(first_generation);
    assert!(
        !log.failure_reported(),
        "a delayed generation-one receipt must not cover generation two"
    );
    log.settle(Some(502), RoutingSettlement::ReportedFailure);
    drop(log);

    let snapshot = fixture.settle().await;
    assert_reason_count(&snapshot, Reason::ProtocolError, 1);
    assert_reason_count(&snapshot, Reason::ThreadRouting, 1);
    let memory_route = snapshot
        .memory
        .iter()
        .find(|row| row.record.reason == Reason::ThreadRouting)
        .and_then(|row| row.record.details.routing.as_ref())
        .expect("uncovered routing failure should remain in memory");
    assert_eq!(memory_route.failure, Some(Failure::Protocol));
    assert!(snapshot.persisted.iter().any(|record| {
        record["reason"] == "thread_routing"
            && record["details"]["routing"]["failure"] == "protocol"
    }));
    let reopened_route = snapshot
        .reopened
        .iter()
        .find(|row| row.record.reason == Reason::ThreadRouting)
        .and_then(|row| row.record.details.routing.as_ref())
        .expect("uncovered routing failure should survive reopen");
    assert_eq!(reopened_route.failure, Some(Failure::Protocol));
}

#[tokio::test]
async fn an_unknown_failure_from_finish_false_survives_a_later_downstream_close() {
    let mut fixture = DiagnosticFixture::new();
    let log = fixture.log(
        RoutingDecision::NewThread,
        evidence(
            RoutingInput::FirstTurn,
            Some(ThreadIdentity::Reliable),
            Some(BoundaryState::Fresh),
            None,
        ),
    );
    log.finish(Some(502), false);
    log.settle(Some(502), RoutingSettlement::DownstreamClosed);
    drop(log);

    let snapshot = fixture.settle().await;
    assert_reason_count(&snapshot, Reason::ThreadRouting, 1);
    assert!(row_has_route(
        &snapshot.memory,
        RoutingDecision::NewThread,
        Some(ThreadIdentity::Reliable),
        None,
        RoutingOutcome::Failed,
    ));
    assert!(has_persisted_route(
        &snapshot,
        RoutingDecision::NewThread,
        Some(ThreadIdentity::Reliable),
        None,
        RoutingOutcome::Failed,
    ));
    assert!(row_has_route(
        &snapshot.reopened,
        RoutingDecision::NewThread,
        Some(ThreadIdentity::Reliable),
        None,
        RoutingOutcome::Failed,
    ));
}

#[tokio::test]
async fn conflicts_unknowns_explicit_outcomes_and_policy_restrictions_remain_observable() {
    let mut fixture = DiagnosticFixture::new();
    let mut expected = Vec::new();

    let log = fixture.log(
        RoutingDecision::ThreadUnidentified,
        evidence(
            RoutingInput::History,
            Some(ThreadIdentity::Conflict),
            None,
            None,
        ),
    );
    log.finish(Some(200), true);
    drop(log);
    expected.push((
        RoutingDecision::ThreadUnidentified,
        Some(ThreadIdentity::Conflict),
        None,
        RoutingOutcome::OwnerPreserved,
    ));

    let log = fixture.log(
        RoutingDecision::ThreadUnidentified,
        evidence(
            RoutingInput::History,
            Some(ThreadIdentity::Unknown),
            None,
            None,
        ),
    );
    log.finish(Some(200), true);
    drop(log);
    expected.push((
        RoutingDecision::ThreadUnidentified,
        Some(ThreadIdentity::Unknown),
        None,
        RoutingOutcome::OwnerPreserved,
    ));

    let log = fixture.log(
        RoutingDecision::Unknown,
        evidence(
            RoutingInput::UnknownStructure,
            Some(ThreadIdentity::Unknown),
            Some(BoundaryState::Unknown),
            None,
        ),
    );
    log.finish(Some(200), true);
    drop(log);
    expected.push((
        RoutingDecision::Unknown,
        Some(ThreadIdentity::Unknown),
        None,
        RoutingOutcome::OwnerPreserved,
    ));

    for outcome in [
        RoutingOutcome::Attempted,
        RoutingOutcome::Unknown,
        RoutingOutcome::Failed,
    ] {
        let log = fixture.log(
            RoutingDecision::ExistingThread,
            evidence(
                RoutingInput::History,
                Some(ThreadIdentity::Reliable),
                Some(BoundaryState::NotObserved),
                None,
            ),
        );
        log.outcome(outcome);
        drop(log);
        expected.push((
            RoutingDecision::ExistingThread,
            Some(ThreadIdentity::Reliable),
            None,
            outcome,
        ));
    }

    let restrictions = [
        (
            RoutingDecision::NewThreadModelDenied,
            RoutingRestriction::ModelMismatch,
        ),
        (
            RoutingDecision::NewThreadModelCooling,
            RoutingRestriction::ModelCooling,
        ),
        (
            RoutingDecision::NewThreadProviderUnavailable,
            RoutingRestriction::ProviderUnavailable,
        ),
        (
            RoutingDecision::NewThreadCapacityFull,
            RoutingRestriction::CapacityFull,
        ),
        (
            RoutingDecision::NewThreadQueueWaiting,
            RoutingRestriction::QueueWaiting,
        ),
    ];
    for (decision, restriction) in restrictions {
        let log = fixture.log(
            decision,
            evidence(
                RoutingInput::FirstTurn,
                Some(ThreadIdentity::Reliable),
                Some(BoundaryState::Fresh),
                Some(restriction),
            ),
        );
        log.finish(Some(200), true);
        drop(log);
        expected.push((
            decision,
            Some(ThreadIdentity::Reliable),
            Some(restriction),
            RoutingOutcome::OwnerPreserved,
        ));
    }

    let log = fixture.log(
        RoutingDecision::ModelDetourOwnerPreserved,
        evidence(
            RoutingInput::History,
            Some(ThreadIdentity::Reliable),
            Some(BoundaryState::NotObserved),
            Some(RoutingRestriction::ModelCooling),
        ),
    );
    log.finish(Some(200), true);
    drop(log);
    expected.push((
        RoutingDecision::ModelDetourOwnerPreserved,
        Some(ThreadIdentity::Reliable),
        Some(RoutingRestriction::ModelCooling),
        RoutingOutcome::OwnerPreserved,
    ));

    let log = fixture.log(
        RoutingDecision::NewThread,
        evidence(
            RoutingInput::FirstTurn,
            Some(ThreadIdentity::Reliable),
            Some(BoundaryState::Fresh),
            Some(RoutingRestriction::Unknown),
        ),
    );
    log.finish(Some(200), true);
    drop(log);
    expected.push((
        RoutingDecision::NewThread,
        Some(ThreadIdentity::Reliable),
        Some(RoutingRestriction::Unknown),
        RoutingOutcome::OwnerPreserved,
    ));

    let log = fixture.log(
        RoutingDecision::BoundaryConsumed,
        evidence(
            RoutingInput::Compaction,
            Some(ThreadIdentity::Reliable),
            Some(BoundaryState::Consumed),
            None,
        ),
    );
    log.finish(Some(200), true);
    drop(log);

    for outcome in [
        RoutingOutcome::WindowConsumed,
        RoutingOutcome::WindowReleased,
        RoutingOutcome::BoundaryRegistered,
        RoutingOutcome::OwnershipSaved,
        RoutingOutcome::SaveFailed,
    ] {
        let log = fixture.log(
            RoutingDecision::ExistingThread,
            evidence(
                RoutingInput::History,
                Some(ThreadIdentity::Reliable),
                Some(BoundaryState::NotObserved),
                None,
            ),
        );
        fixture.service.emit(
            log.take(outcome)
                .expect("explicit routing result should be returned"),
        );
        drop(log);
        expected.push((
            RoutingDecision::ExistingThread,
            Some(ThreadIdentity::Reliable),
            None,
            outcome,
        ));
    }

    let snapshot = fixture.settle().await;
    assert_reason_count(&snapshot, Reason::ThreadRouting, expected.len());
    for (decision, identity, restriction, outcome) in expected {
        assert!(
            broadcast_has_route(&snapshot.changes, decision, identity, restriction, outcome),
            "explicit route {decision:?}/{identity:?}/{restriction:?}/{outcome:?} missing from broadcast"
        );
        assert!(
            row_has_route(&snapshot.memory, decision, identity, restriction, outcome),
            "explicit route {decision:?}/{identity:?}/{restriction:?}/{outcome:?} missing from memory"
        );
        assert!(
            has_persisted_route(&snapshot, decision, identity, restriction, outcome),
            "explicit route {decision:?}/{identity:?}/{restriction:?}/{outcome:?} missing from JSONL"
        );
        assert!(
            row_has_route(&snapshot.reopened, decision, identity, restriction, outcome),
            "explicit route {decision:?}/{identity:?}/{restriction:?}/{outcome:?} missing after reopen"
        );
    }
}
