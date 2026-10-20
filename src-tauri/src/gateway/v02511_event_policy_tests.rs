//! v0.25.11 event policy regressions.
//!
//! These tests deliberately wait for the diagnostic writer's sentinel before
//! checking the in-memory journal or JSONL.  A short sleep cannot prove that
//! an earlier event was not still queued.

use super::{
    failover::Failure,
    protocol::{Protocol, Terminal},
    replay::RequestHints,
    routing_log::RoutingLog,
    ClientId, Gateway,
};
use crate::{
    events::{self, Filter as EventFilter, Reason as EventReason},
    usage::{
        model::{
            BoundaryState, Filter as UsageFilter, RoutingDecision, RoutingInput, ThreadIdentity,
        },
        Service as UsageService,
    },
};
use serde_json::{json, Value};
use std::time::Duration;

const OWNER: &str = "00000000-0000-0000-0000-000000000001";
const MODEL: &str = "fixture-model";

fn success_payload(response_id: &str) -> Vec<u8> {
    format!(
        "data: {{\"type\":\"response.completed\",\"response\":{{\"id\":\"{response_id}\",\"model\":\"{MODEL}\",\"status\":\"completed\"}}}}\n\n"
    )
    .into_bytes()
}

fn failure_payload(response_id: &str) -> Vec<u8> {
    format!(
        "data: {{\"type\":\"response.failed\",\"response\":{{\"id\":\"{response_id}\",\"model\":\"{MODEL}\",\"status\":\"failed\",\"error\":{{\"code\":\"server_error\"}}}}}}\n\n"
    )
    .into_bytes()
}

fn created_payload(response_id: &str) -> Vec<u8> {
    format!(
        "data: {{\"type\":\"response.created\",\"response\":{{\"id\":\"{response_id}\",\"model\":\"{MODEL}\"}}}}\n\n"
    )
    .into_bytes()
}

struct Fixture {
    dir: tempfile::TempDir,
    gateway: Gateway,
    events: events::Service,
    usage: UsageService,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let gateway = Gateway::new(dir.path().join("gateway")).unwrap();
        let events = events::Service::new(&dir.path().join("events"));
        let usage = UsageService::new(&dir.path().join("usage"));
        gateway.set_diagnostics(events.clone());
        gateway.set_usage(usage.clone());
        Self {
            dir,
            gateway,
            events,
            usage,
        }
    }
}

fn digest(value: &str) -> String {
    crate::storage::digest(value.as_bytes())
}

fn hints(boundary: &str) -> RequestHints {
    let mut hints = RequestHints::from_value(
        json!({
            "model": MODEL,
            "input": [{"type": "compaction", "encrypted_content": "fixture-window"}]
        }),
        false,
    )
    .unwrap();
    hints.compacted_window = Some(boundary.to_owned());
    hints.routing_evidence.input = RoutingInput::Compaction;
    hints.routing_evidence.identity = Some(ThreadIdentity::Reliable);
    hints.routing_evidence.boundary = Some(BoundaryState::Consumed);
    hints
}

fn seed_boundary(gateway: &Gateway, session: &str, boundary: &str) {
    let owner = gateway
        .0
        .compaction
        .prepare(session.to_owned(), None, false, &[OWNER.to_owned()])
        .expect("fixture owner lease");
    owner.complete(OWNER, None);
    owner.complete(OWNER, Some(boundary));
    gateway.0.compaction.flush();

    // A real later turn starts output and consumes the previously registered
    // window.  Measured requests below then observe BoundaryConsumed.
    let consumer = gateway
        .0
        .compaction
        .prepare(session.to_owned(), None, false, &[OWNER.to_owned()])
        .expect("fixture boundary consumer lease");
    consumer.output_started(OWNER);
    gateway.0.compaction.flush();
    drop(consumer);
    gateway.0.compaction.flush();
}

fn routed_protocol(
    fixture: &Fixture,
    thread_id: &str,
    boundary: &str,
) -> (Protocol, std::sync::Arc<RoutingLog>, crate::usage::Trace) {
    let session = digest(thread_id);
    let routing_hints = hints(boundary);
    let lease = fixture
        .gateway
        .0
        .compaction
        .prepare(session.clone(), Some(boundary), false, &[OWNER.to_owned()])
        .expect("fixture routed lease");
    let decision = lease.decision(Some(&routing_hints), OWNER);
    assert_eq!(decision, RoutingDecision::BoundaryConsumed);

    let log = RoutingLog::new(&fixture.gateway).expect("diagnostic routing log");
    let mut protocol = Protocol::new(true);
    protocol.diagnostics(&fixture.gateway, OWNER, Some(MODEL));
    let mut request_headers = hyper::HeaderMap::new();
    request_headers.insert("thread-id", thread_id.parse().unwrap());
    protocol.request_headers(&request_headers);
    protocol.attach_ownership(Some(lease), OWNER);
    protocol.attach_routing_log(Some(log.clone()));
    let trace = fixture.usage.begin("codex", Some(MODEL));
    protocol.attach_usage(trace.attempt(OWNER, true, "http"));
    protocol.context(Some(session), Some(decision));
    protocol.routing_evidence(Some(&routing_hints));
    (protocol, log, trace)
}

fn usage_rows(service: &UsageService) -> Vec<crate::usage::model::Record> {
    service
        .read(|store| Ok(store.logs(&UsageFilter::default())?.rows))
        .unwrap()
}

async fn wait_for_usage_rows(
    service: &UsageService,
    receiver: &mut tokio::sync::broadcast::Receiver<()>,
    count: usize,
) -> Vec<crate::usage::model::Record> {
    let waited = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let rows = usage_rows(service);
            if rows.len() >= count {
                return rows;
            }
            receiver.recv().await.expect("usage writer closed");
        }
    })
    .await;
    match waited {
        Ok(rows) => rows,
        Err(_) => {
            let rows = usage_rows(service);
            let state_error = service.state().error;
            panic!(
                "usage rows did not settle: expected {count}, actual {}, state.error={state_error:?}",
                rows.len()
            );
        }
    }
}

fn sentinel() -> events::Record {
    events::Record::new(
        Some(ClientId::Codex),
        Some("fixture-sentinel"),
        Some(MODEL),
        EventReason::ProtocolError,
        events::Action::Returned,
        Some(599),
        None,
    )
}

async fn wait_for_sentinel(
    receiver: &mut tokio::sync::broadcast::Receiver<events::Change>,
    id: &str,
) -> events::Change {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let change = receiver.recv().await.expect("diagnostic writer closed");
            assert!(change.error.is_none(), "diagnostic persistence failed");
            assert!(!change.notify, "synthetic diagnostics must never notify");
            if change.record.as_ref().is_some_and(|record| record.id == id) {
                return change;
            }
        }
    })
    .await
    .expect("diagnostic sentinel did not settle")
}

async fn settle_diagnostics(
    fixture: &Fixture,
    receiver: &mut tokio::sync::broadcast::Receiver<events::Change>,
) -> (Vec<events::ClassifiedRecord>, Vec<Value>) {
    let marker = sentinel();
    let marker_id = marker.id.clone();
    fixture.events.emit(marker);
    let change = wait_for_sentinel(receiver, &marker_id).await;
    assert!(!change.notify, "fixture sentinel must not notify");
    let page = fixture.events.query(EventFilter::default());
    let jsonl = std::fs::read_to_string(fixture.dir.path().join("events/diagnostics.jsonl"))
        .expect("diagnostic JSONL should be durable");
    let lines: Vec<Value> = jsonl
        .lines()
        .map(|line| serde_json::from_str(line).expect("diagnostic JSONL line should parse"))
        .collect();
    assert!(lines.iter().any(|line| line["id"] == marker_id));
    (page.items, lines)
}

#[tokio::test]
async fn consumed_boundary_owner_preserved_continuations_keep_usage_and_emit_no_routing_event() {
    let fixture = Fixture::new();
    let session = digest("fixture-boundary-session");
    let boundary = digest("fixture-boundary-window");
    seed_boundary(&fixture.gateway, &session, &boundary);
    let mut usage_notifications = fixture.usage.subscribe();
    let mut diagnostic_notifications = fixture.events.subscribe();
    const THREAD_ID: &str = "fixture-boundary-session";

    for attempt_index in 0..5 {
        let lease = fixture
            .gateway
            .0
            .compaction
            .prepare(session.clone(), Some(&boundary), false, &[OWNER.to_owned()])
            .expect("fixture continuation lease");
        let hints = hints(&boundary);
        let decision = lease.decision(Some(&hints), OWNER);
        assert_eq!(decision, RoutingDecision::BoundaryConsumed);
        let log = RoutingLog::new(&fixture.gateway).expect("diagnostic routing log");
        let mut protocol = Protocol::new(true);
        protocol.diagnostics(&fixture.gateway, OWNER, Some(MODEL));
        let mut request_headers = hyper::HeaderMap::new();
        request_headers.insert("thread-id", THREAD_ID.parse().unwrap());
        log.pending(&request_headers, Some(&hints), Some(&lease), false);
        protocol.request_headers(&request_headers);
        protocol.attach_ownership(Some(lease), OWNER);
        protocol.attach_routing_log(Some(log.clone()));
        let trace = fixture.usage.begin("codex", Some(MODEL));
        protocol.attach_usage(trace.attempt(OWNER, true, "http"));
        protocol.context(Some(session.clone()), Some(decision));
        protocol.routing_evidence(Some(&hints));
        protocol.response(200, true, "identity");
        let payload = success_payload(&format!("fixture-boundary-response-{attempt_index}"));
        protocol.feed(&payload);
        assert_eq!(protocol.terminal(), Some(Terminal::Success));
        protocol.delivery().handed(payload.len(), true);
        protocol.output_handed(true);
        protocol.finish(Some(200), "OK");
        drop(protocol);
        drop(log);
        drop(trace);
    }

    let rows = wait_for_usage_rows(&fixture.usage, &mut usage_notifications, 5).await;
    assert_eq!(rows.len(), 5);
    let mut expected_evidence = None;
    for row in rows {
        let attempt = row.final_attempt().expect("one fixture attempt");
        assert_eq!(attempt.provider.as_deref(), Some(OWNER));
        assert_eq!(attempt.thread_hash.as_deref(), Some(session.as_str()));
        assert_eq!(
            attempt.routing_decision,
            Some(RoutingDecision::BoundaryConsumed)
        );
        assert_eq!(
            attempt.routing_evidence.as_ref().and_then(|e| e.identity),
            Some(ThreadIdentity::Reliable)
        );
        assert_eq!(
            attempt.routing_evidence.as_ref().and_then(|e| e.boundary),
            Some(BoundaryState::Consumed)
        );
        let evidence = attempt
            .routing_evidence
            .as_ref()
            .expect("usage routing evidence")
            .clone();
        assert_eq!(evidence.input, RoutingInput::Compaction);
        assert_eq!(evidence.restriction, None);
        if let Some(expected) = &expected_evidence {
            assert_eq!(&evidence, expected, "continuation routing evidence changed");
        } else {
            expected_evidence = Some(evidence);
        }
    }

    fixture.gateway.0.compaction.flush();
    let verification_lease = fixture
        .gateway
        .0
        .compaction
        .prepare(session.clone(), Some(&boundary), false, &[OWNER.to_owned()])
        .expect("fixture verification lease");
    assert_eq!(
        fixture.gateway.0.compaction.owner(&session).as_deref(),
        Some(OWNER)
    );
    assert_eq!(
        verification_lease.boundary_state(Some(&boundary)),
        BoundaryState::Consumed
    );
    assert_eq!(
        verification_lease.decision(Some(&hints(&boundary)), OWNER),
        RoutingDecision::BoundaryConsumed
    );
    drop(verification_lease);
    fixture.gateway.0.compaction.flush();

    let (rows, persisted) = settle_diagnostics(&fixture, &mut diagnostic_notifications).await;
    assert_eq!(
        rows.iter()
            .filter(|row| row.record.reason == EventReason::ThreadRouting)
            .count(),
        0,
        "BoundaryConsumed + OwnerPreserved must be silent"
    );
    assert_eq!(
        persisted
            .iter()
            .filter(|line| line["reason"] == "thread_routing")
            .count(),
        0,
        "BoundaryConsumed continuation must not reach JSONL"
    );
}

#[tokio::test]
async fn identified_downstream_closes_do_not_emit_interruption_for_any_terminal_state() {
    let fixture = Fixture::new();
    let mut usage_notifications = fixture.usage.subscribe();
    let mut diagnostic_notifications = fixture.events.subscribe();
    let route_session = digest("fixture-close-routing-session");
    let route_boundary = digest("fixture-close-routing-window");
    seed_boundary(&fixture.gateway, &route_session, &route_boundary);
    let routing_hints = hints(&route_boundary);
    let mut expected_usage = 0;
    for error in [4_u8, 5, 6, 7] {
        for terminal_state in 0..3 {
            let lease = fixture
                .gateway
                .0
                .compaction
                .prepare(
                    route_session.clone(),
                    Some(&route_boundary),
                    false,
                    &[OWNER.to_owned()],
                )
                .expect("fixture close routing lease");
            let decision = lease.decision(Some(&routing_hints), OWNER);
            assert_eq!(decision, RoutingDecision::BoundaryConsumed);
            let log = RoutingLog::new(&fixture.gateway).expect("diagnostic routing log");
            let mut protocol = Protocol::new(true);
            protocol.diagnostics(&fixture.gateway, OWNER, Some(MODEL));
            let mut request_headers = hyper::HeaderMap::new();
            request_headers.insert(
                "thread-id",
                "fixture-close-routing-session".parse().unwrap(),
            );
            protocol.request_headers(&request_headers);
            protocol.attach_ownership(Some(lease), OWNER);
            protocol.attach_routing_log(Some(log.clone()));
            let trace = fixture.usage.begin("codex", Some(MODEL));
            protocol.attach_usage(trace.attempt(OWNER, true, "http"));
            protocol.context(Some(route_session.clone()), Some(decision));
            protocol.routing_evidence(Some(&routing_hints));
            protocol.response(200, true, "identity");
            match terminal_state {
                0 => {}
                1 => {
                    let payload = success_payload(&format!("fixture-close-success-{error}"));
                    protocol.feed(&payload);
                }
                2 => {
                    let payload = failure_payload(&format!("fixture-close-failure-{error}"));
                    protocol.feed(&payload);
                    log.failure(Failure::UpstreamService);
                }
                _ => unreachable!(),
            }
            protocol.delivery().failed(error);
            drop(protocol);
            drop(log);
            drop(trace);
            expected_usage += 1;
        }
    }

    let usages =
        wait_for_usage_rows(&fixture.usage, &mut usage_notifications, expected_usage).await;
    assert_eq!(usages.len(), expected_usage);
    fixture.gateway.0.compaction.flush();
    let (rows, persisted) = settle_diagnostics(&fixture, &mut diagnostic_notifications).await;
    assert_eq!(
        rows.iter()
            .filter(|row| row.record.reason == EventReason::RequestInterrupted)
            .count(),
        0,
        "identified downstream close must not become RequestInterrupted"
    );
    assert_eq!(
        rows.iter()
            .filter(|row| row.record.reason == EventReason::ThreadRouting)
            .count(),
        4,
        "terminal upstream failures must remain visible after a recognized close"
    );
    assert_eq!(
        persisted
            .iter()
            .filter(|line| line["reason"] == "request_interrupted")
            .count(),
        0,
        "recognized closes must not reach JSONL as interruptions"
    );
    assert_eq!(
        persisted
            .iter()
            .filter(|line| line["reason"] == "thread_routing")
            .count(),
        4,
        "terminal upstream failures must be durable after a recognized close"
    );
}

#[tokio::test]
async fn upstream_parse_transport_and_prior_admission_failures_survive_known_close() {
    let fixture = Fixture::new();
    let mut usage_notifications = fixture.usage.subscribe();
    let mut diagnostic_notifications = fixture.events.subscribe();
    let cases = [
        "fixture-terminal-failure-thread",
        "fixture-parse-failure-thread",
        "fixture-read-failure-thread",
        "fixture-admission-failure-thread",
    ];

    for (index, thread_id) in cases.into_iter().enumerate() {
        let session = digest(thread_id);
        let boundary = digest(&format!("fixture-failure-window-{index}"));
        seed_boundary(&fixture.gateway, &session, &boundary);
        let (mut protocol, log, trace) = routed_protocol(&fixture, thread_id, &boundary);
        protocol.response(200, true, "identity");

        match index {
            0 => {
                let payload = failure_payload(&format!("fixture-terminal-failure-{index}"));
                protocol.feed(&payload);
                log.failure(Failure::UpstreamService);
                protocol.delivery().failed(5);
            }
            1 => {
                let payload = created_payload(&format!("fixture-parse-failure-{index}"));
                protocol.feed(&payload);
                protocol.feed(b"data: {not-json}\n\n");
                assert!(protocol.observation.incomplete);
                protocol.delivery().failed(5);
            }
            2 => {
                let payload = created_payload(&format!("fixture-read-failure-{index}"));
                protocol.feed(&payload);
                protocol.delivery().failed(5);
                protocol.finish(Some(200), crate::errors::code::STREAM_INTERRUPTED);
                assert!(protocol.transport_failure());
            }
            3 => {
                // This sticky false result models an earlier admission failure.
                // A later recognized downstream close must not erase it.
                log.finish(Some(200), false);
                protocol.delivery().failed(5);
            }
            _ => unreachable!(),
        }
        drop(protocol);
        drop(log);
        drop(trace);
    }

    let usage = wait_for_usage_rows(&fixture.usage, &mut usage_notifications, 4).await;
    assert_eq!(usage.len(), 4);
    fixture.gateway.0.compaction.flush();
    let (rows, persisted) = settle_diagnostics(&fixture, &mut diagnostic_notifications).await;
    assert_eq!(
        rows.iter()
            .filter(|row| row.record.reason == EventReason::ThreadRouting)
            .count(),
        4,
        "each real fault must retain its routing record"
    );
    assert_eq!(
        rows.iter()
            .filter(|row| row.record.reason == EventReason::RequestInterrupted)
            .count(),
        0,
        "recognized downstream close must stay quiet even when another fault is retained"
    );
    assert_eq!(
        persisted
            .iter()
            .filter(|line| line["reason"] == "thread_routing")
            .count(),
        4,
        "real routing faults must reach durable JSONL"
    );
    assert_eq!(
        persisted
            .iter()
            .filter(|line| line["reason"] == "request_interrupted")
            .count(),
        0,
        "recognized close must not reach durable JSONL as an interruption"
    );
}
