//! v0.21.2 delivery and handoff notification regressions.
//!
//! These fixtures exercise only protocol observations, in-memory ownership,
//! and the diagnostic notification worker.  They use synthetic IDs and
//! example.invalid endpoints; no user data or credentials are read.
use crate::events::{self, Action, Filter, Reason};
use crate::gateway::{
    compaction::Registry,
    protocol::{Protocol, Terminal},
    ClientId,
};
use serde_json::json;
use std::{sync::Arc, time::Duration};

const OLD: &str = "00000000-0000-0000-0000-000000000021";
const NEW: &str = "00000000-0000-0000-0000-000000000022";

fn seed_handoff(
    registry: &Arc<Registry>,
    session: &str,
    boundary: &str,
) -> Arc<crate::gateway::compaction::Lease> {
    let initial = registry
        .prepare(session.to_owned(), None, false, &[OLD.into(), NEW.into()])
        .expect("fixture initial lease");
    assert!(initial.admitted(OLD));
    initial.complete(OLD, Some(boundary));
    let handoff = registry
        .prepare(
            session.to_owned(),
            Some(boundary),
            true,
            &[NEW.into(), OLD.into()],
        )
        .expect("fixture handoff lease");
    assert!(handoff.handoff);
    handoff
}

fn handoff_events(service: &events::Service) -> Vec<events::Record> {
    service
        .query(Filter {
            reason: Some(Reason::ThreadHandoff),
            ..Default::default()
        })
        .items
}

async fn wait_for_handoff(
    receiver: &mut tokio::sync::broadcast::Receiver<events::Change>,
) -> events::Record {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match receiver.recv().await {
                Ok(change)
                    if change
                        .record
                        .as_ref()
                        .is_some_and(|r| r.reason == Reason::ThreadHandoff) =>
                {
                    return change.record.expect("handoff record");
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    panic!("diagnostic event stream closed")
                }
            }
        }
    })
    .await
    .expect("handoff notification was not emitted")
}

async fn assert_no_handoff(receiver: &mut tokio::sync::broadcast::Receiver<events::Change>) {
    let result = tokio::time::timeout(Duration::from_millis(120), receiver.recv()).await;
    if let Ok(Ok(change)) = result {
        assert_ne!(
            change.record.as_ref().map(|r| r.reason),
            Some(Reason::ThreadHandoff),
            "failed or non-moving ownership must not notify"
        );
    }
}

fn success_value() -> serde_json::Value {
    json!({
        "type": "response.completed",
        "response": {"status": "completed", "model": "fixture-model"}
    })
}

#[test]
fn json_and_sse_eof_without_trailing_blank_line_settle_after_downstream_delivery() {
    for (index, (stream, bytes)) in [
        (
            false,
            br#"{"type":"response.completed","response":{"status":"completed"}}"#.as_slice(),
        ),
        (
            true,
            br#"data: {"type":"response.completed","response":{"status":"completed"}}"#.as_slice(),
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let dir = tempfile::tempdir().unwrap();
        let registry = Registry::new(dir.path().join(format!("owners-{index}.json")));
        let session =
            crate::storage::digest(format!("fixture-delivery-session-{index}").as_bytes());
        let boundary =
            crate::storage::digest(format!("fixture-delivery-boundary-{index}").as_bytes());
        let handoff = seed_handoff(&registry, &session, &boundary);
        let mut protocol = Protocol::new(stream);
        protocol.attach_ownership(Some(handoff), NEW);
        protocol.response(200, stream, "identity");
        protocol.feed(bytes);
        protocol.finish(Some(200), "OK");
        assert_eq!(protocol.terminal(), Some(Terminal::Success));
        assert!(!protocol.delivered_success());
        protocol.commit_ownership();
        assert_eq!(registry.owner(&session).as_deref(), Some(OLD));
        protocol.delivery().handed(bytes.len(), true);
        assert!(protocol.delivered_success());
        protocol.commit_ownership();
        registry.flush();
        assert_eq!(registry.owner(&session).as_deref(), Some(NEW));
    }
}

#[test]
fn websocket_response_created_delivery_consumes_old_compaction_boundary_once() {
    let dir = tempfile::tempdir().unwrap();
    let registry = Registry::new(dir.path().join("owners.json"));
    let session = crate::storage::digest(b"fixture-ws-response-created-session");
    let boundary = crate::storage::digest(b"fixture-ws-response-created-boundary");
    let handoff = seed_handoff(&registry, &session, &boundary);

    let mut protocol = Protocol::new(true);
    protocol.attach_ownership(Some(handoff), NEW);
    protocol.websocket_status(101);
    protocol.value(&json!({
        "type": "response.created",
        "response": {"id": "fixture-response", "model": "fixture-model"}
    }));

    // The websocket text frame is business output only after the downstream
    // send succeeds. A control/non-business handoff must leave the boundary
    // reusable until the actual response.created frame is acknowledged.
    protocol.output_handed(false);
    drop(protocol);
    assert!(registry.boundary_matches(&session, Some(&boundary)));

    let handoff = registry
        .prepare(
            session.clone(),
            Some(&boundary),
            true,
            &[NEW.into(), OLD.into()],
        )
        .expect("fixture second handoff lease");
    assert!(handoff.handoff);
    let mut protocol = Protocol::new(true);
    protocol.attach_ownership(Some(handoff), NEW);
    protocol.websocket_status(101);
    protocol.value(&json!({
        "type": "response.created",
        "response": {"id": "fixture-response", "model": "fixture-model"}
    }));
    protocol.output_handed(true);
    drop(protocol);

    let retry = registry
        .prepare(session, Some(&boundary), true, &[NEW.into(), OLD.into()])
        .expect("fixture retry lease");
    assert!(
        !retry.handoff,
        "an acknowledged response.created must consume the old boundary"
    );
    assert_eq!(retry.owner.as_deref(), Some(OLD));
}

#[test]
fn limited_and_failed_terminals_never_count_as_delivered_success() {
    for (value, expected) in [
        (
            json!({
                "type": "response.incomplete",
                "response": {"incomplete_details": {"reason": "max_output_tokens"}}
            }),
            Terminal::Limited,
        ),
        (
            json!({
                "type": "response.failed",
                "response": {"error": {"code": "server_error"}}
            }),
            Terminal::Failure,
        ),
    ] {
        let mut protocol = Protocol::new(false);
        protocol.value(&value);
        assert_eq!(protocol.terminal(), Some(expected));
        protocol.delivery().handed(1, true);
        assert!(!protocol.delivered_success());
    }
}

#[tokio::test]
async fn failed_first_allocation_and_same_owner_fallback_emit_no_handoff_notification() {
    let dir = tempfile::tempdir().unwrap();
    let events = events::Service::new(dir.path());

    // A claimed handoff ending in an upstream failure must not leave a
    // success notification behind.
    let failed_registry = Registry::new(dir.path().join("failed-owners.json"));
    failed_registry.notifications(ClientId::Codex, events.clone());
    failed_registry.provider_names(&[
        super::provider(OLD, true, false),
        super::provider(NEW, true, false),
    ]);
    let mut failed_rx = events.subscribe();
    let failed_session = crate::storage::digest(b"fixture-failed-handoff-thread");
    let failed = seed_handoff(
        &failed_registry,
        &failed_session,
        &crate::storage::digest(b"fixture-failed-handoff-boundary"),
    );
    let mut failed_protocol = Protocol::new(false);
    failed_protocol.attach_ownership(Some(failed), NEW);
    failed_protocol.value(&json!({
        "type": "response.failed",
        "response": {"error": {"code": "server_error"}}
    }));
    failed_protocol.delivery().handed(1, true);
    assert_eq!(failed_protocol.terminal(), Some(Terminal::Failure));
    assert!(!failed_protocol.delivered_success());
    failed_protocol.commit_ownership();
    drop(failed_protocol);
    failed_registry.flush();
    assert_eq!(failed_registry.owner(&failed_session).as_deref(), Some(OLD));
    assert_no_handoff(&mut failed_rx).await;

    // The first ordinary allocation establishes ownership without a move.
    let ordinary_registry = Registry::new(dir.path().join("ordinary-owners.json"));
    ordinary_registry.notifications(ClientId::Codex, events.clone());
    let mut ordinary_rx = events.subscribe();
    let session = crate::storage::digest(b"fixture-first-ordinary-thread");
    let ordinary = ordinary_registry
        .prepare(session, None, false, &[OLD.into(), NEW.into()])
        .unwrap();
    assert!(ordinary.admitted(OLD));
    ordinary.complete(OLD, None);
    ordinary_registry.flush();
    assert_no_handoff(&mut ordinary_rx).await;

    // A handoff-shaped retry that stays on the old owner is a fallback, not a
    // successful cross-provider takeover.
    let fallback_registry = Registry::new(dir.path().join("fallback-owners.json"));
    fallback_registry.notifications(ClientId::Codex, events.clone());
    let mut fallback_rx = events.subscribe();
    let fallback = seed_handoff(
        &fallback_registry,
        &crate::storage::digest(b"fixture-fallback-thread"),
        &crate::storage::digest(b"fixture-fallback-boundary"),
    );
    fallback.complete(OLD, None);
    fallback_registry.flush();
    assert_no_handoff(&mut fallback_rx).await;
}

#[tokio::test]
async fn successful_cross_provider_commit_notifies_once_per_thread_and_keeps_threads_separate() {
    let dir = tempfile::tempdir().unwrap();
    let events = events::Service::new(dir.path());
    let registry = Registry::new(dir.path().join("owners.json"));
    registry.notifications(ClientId::Codex, events.clone());
    registry.provider_names(&[
        super::provider(OLD, true, false),
        super::provider(NEW, true, false),
    ]);
    let mut receiver = events.subscribe();

    let first_session = crate::storage::digest(b"fixture-handoff-thread-one");
    let first_boundary = crate::storage::digest(b"fixture-handoff-boundary-one");
    let first = seed_handoff(&registry, &first_session, &first_boundary);
    let mut protocol = Protocol::new(false);
    protocol.attach_ownership(Some(first), NEW);
    protocol.value(&success_value());
    protocol.commit_ownership();
    assert!(!protocol.delivered_success());
    protocol.delivery().handed(1, true);
    assert!(protocol.delivered_success());
    protocol.commit_ownership();
    // Repeated terminal observation for the same lease must stay one event.
    protocol.value(&success_value());
    protocol.commit_ownership();
    drop(protocol);
    registry.flush();

    let first_record = wait_for_handoff(&mut receiver).await;
    assert_eq!(first_record.client_id, Some(ClientId::Codex));
    assert_eq!(first_record.provider_id.as_deref(), Some(NEW));
    assert_eq!(first_record.action, Action::Routed);
    let first_scope = first_record.details.handoff_scope.clone().unwrap();
    assert_eq!(first_scope.len(), 64);
    assert!(first_scope.bytes().all(|byte| byte.is_ascii_hexdigit()));
    assert_no_handoff(&mut receiver).await;

    // A later compaction boundary on the same thread is a new takeover
    // generation and must receive its own deduplication scope.
    let next_boundary = crate::storage::digest(b"fixture-handoff-boundary-next");
    let continuation = registry
        .prepare(
            first_session.clone(),
            None,
            false,
            &[OLD.into(), NEW.into()],
        )
        .unwrap();
    assert_eq!(continuation.owner.as_deref(), Some(NEW));
    continuation.complete(NEW, Some(&next_boundary));
    let next_handoff = registry
        .prepare(
            first_session.clone(),
            Some(&next_boundary),
            true,
            &[OLD.into(), NEW.into()],
        )
        .unwrap();
    assert!(next_handoff.handoff);
    let mut next_protocol = Protocol::new(false);
    next_protocol.attach_ownership(Some(next_handoff), OLD);
    next_protocol.value(&success_value());
    next_protocol.delivery().handed(1, true);
    assert!(next_protocol.delivered_success());
    next_protocol.commit_ownership();
    drop(next_protocol);
    registry.flush();

    let next_record = wait_for_handoff(&mut receiver).await;
    let next_scope = next_record.details.handoff_scope.unwrap();
    assert_ne!(
        first_scope, next_scope,
        "a new boundary generation gets a new scope"
    );
    assert_no_handoff(&mut receiver).await;

    let second_session = crate::storage::digest(b"fixture-handoff-thread-two");
    let second_boundary = crate::storage::digest(b"fixture-handoff-boundary-two");
    let second = seed_handoff(&registry, &second_session, &second_boundary);
    let mut second_protocol = Protocol::new(false);
    second_protocol.attach_ownership(Some(second), NEW);
    second_protocol.value(&success_value());
    second_protocol.delivery().handed(1, true);
    assert!(second_protocol.delivered_success());
    second_protocol.commit_ownership();
    drop(second_protocol);
    registry.flush();

    let second_record = wait_for_handoff(&mut receiver).await;
    assert_eq!(second_record.provider_id.as_deref(), Some(NEW));
    let second_scope = second_record.details.handoff_scope.unwrap();
    assert_ne!(next_scope, second_scope, "different threads must not merge");
    assert_eq!(handoff_events(&events).len(), 3);
}
