// 覆盖大请求投影、交接名额与流式尾部进展。
//! v0.21.2 metadata, handoff, and thread-capacity fixtures.
//!
//! Every request, provider, and opaque window in this module is synthetic.  The
//! integration fixtures use only loopback listeners or `example.invalid` URLs.
use super::{
    admission::Budget,
    compaction::{self, Intake, Registry},
    metadata::Projector,
    model::Provider,
    replay::RequestHints,
    routing::Requirement,
    wire_progress, *,
};
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use http_body_util::{BodyExt, StreamBody};
use hyper::{body::Frame, Response};
use serde_json::json;
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::io::AsyncWriteExt;

fn policy_provider(id: &str, handoff: bool, take: bool) -> Provider {
    Provider {
        id: id.into(),
        name: id.into(),
        base_url: format!("https://{id}.example.invalid/v1"),
        token: "fixture-token".into(),
        queued: true,
        version: format!("{id}-version"),
        max_concurrency: 0,
        max_rpm: 0,
        allowed_models: None,
        handoff_after_compaction: handoff,
        take_new_threads: take,
    }
}

fn projected_hints(raw: &[u8], chunk: usize) -> RequestHints {
    let mut projector = Projector::default();
    for part in raw.chunks(chunk.max(1)) {
        projector.feed(part);
    }
    RequestHints::from_value(
        projector.finish().expect("fixture metadata projection"),
        true,
    )
    .expect("fixture request hints")
}

fn compaction_payload(items: usize, encrypted: &str) -> Vec<u8> {
    let mut input = Vec::with_capacity(items + 1);
    for index in 0..items {
        input.push(json!({
            "role": "user",
            "content": format!("fixture-input-{index}"),
        }));
    }
    input.push(json!({
        "type": "compaction",
        "encrypted_content": encrypted,
    }));
    serde_json::to_vec(&json!({"model":"fixture-model","input":input})).unwrap()
}

#[test]
fn v0212_compaction_after_large_inputs_is_recognized_at_all_boundaries() {
    let encrypted = "fixture-window-after-large-input";
    let expected = crate::storage::digest(encrypted.as_bytes());
    for items in [0, 63, 64, 82, 299] {
        let raw = compaction_payload(items, encrypted);
        for chunk in [1, 7, 64, 4096] {
            let hints = projected_hints(&raw, chunk);
            assert_eq!(
                hints.compacted_window.as_deref(),
                Some(expected.as_str()),
                "input count {items}, chunk size {chunk}"
            );
        }
    }
}

#[test]
fn v0212_assistant_and_tool_after_the_first_64_items_never_prove_a_new_thread() {
    for role in ["assistant", "tool"] {
        let mut input = Vec::with_capacity(65);
        for _ in 0..64 {
            input.push(json!({"role":"user","content":"fixture-user"}));
        }
        input.push(json!({"role":role,"content":"fixture-tail"}));
        let raw = serde_json::to_vec(&json!({"model":"fixture-model","input":input})).unwrap();
        for chunk in [1, 13, 257] {
            let hints = projected_hints(&raw, chunk);
            assert!(!hints.first_turn, "role {role}, chunk size {chunk}");
            assert!(!hints.auxiliary);
            assert!(hints.compacted_window.is_none());
        }
    }
}

fn escaped_json_string(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len() + 32);
    for ch in value.chars() {
        match ch {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            'é' => escaped.push_str("\\u00e9"),
            '😀' => escaped.push_str("\\ud83d\\ude00"),
            other => escaped.push(other),
        }
    }
    escaped
}

#[test]
fn v0212_large_and_escaped_encrypted_content_have_the_same_decoded_digest() {
    let plain = format!("{}\\\"\\\n\\t é 😀", "fixture-large-window-".repeat(140));
    assert!(plain.len() > 2048);
    let plain_raw = serde_json::to_vec(&json!({
        "model":"fixture-model",
        "input":[{"type":"compaction","encrypted_content":plain}]
    }))
    .unwrap();
    let escaped = escaped_json_string(&plain);
    let escaped_raw = format!(
        "{{\"model\":\"fixture-model\",\"input\":[{{\"type\":\"compaction\",\"encrypted_content\":\"{escaped}\"}}]}}"
    );
    let expected = crate::storage::digest(plain.as_bytes());
    for chunk in [1, 2, 17, 257, 4096] {
        assert_eq!(
            projected_hints(&plain_raw, chunk)
                .compacted_window
                .as_deref(),
            Some(expected.as_str()),
            "plain digest at chunk size {chunk}"
        );
        assert_eq!(
            projected_hints(escaped_raw.as_bytes(), chunk)
                .compacted_window
                .as_deref(),
            Some(expected.as_str()),
            "escaped digest at chunk size {chunk}"
        );
    }
}

fn ordinary_hints() -> RequestHints {
    RequestHints::from_value(
        json!({"model":"fixture-model","input":[{"role":"user","content":"fixture"}]}),
        false,
    )
    .unwrap()
}

fn compacted_hints(boundary: &str) -> RequestHints {
    RequestHints {
        compacted_window: Some(boundary.to_owned()),
        ..Default::default()
    }
}

fn seed_boundary(
    registry: &Arc<Registry>,
    session: &str,
    owner: &Provider,
    other: &Provider,
    boundary: &str,
) {
    let first = registry
        .prepare_policy(
            session.to_owned(),
            Some(&ordinary_hints()),
            false,
            &[owner.clone(), other.clone()],
        )
        .unwrap();
    assert!(first.admitted(&owner.id));
    first.complete(&owner.id, Some(boundary));
}

#[test]
fn v0212_failed_same_boundary_handoff_does_not_let_the_original_provider_consume_it() {
    let dir = tempfile::tempdir().unwrap();
    let registry = Registry::new(dir.path().join("owners.json"));
    let old = policy_provider("fixture-old", true, false);
    let new = policy_provider("fixture-new", true, false);
    let session = crate::storage::digest(b"fixture-v0212-failed-handoff");
    let boundary = crate::storage::digest(b"fixture-v0212-window");
    seed_boundary(&registry, &session, &old, &new, &boundary);

    let compacted = compacted_hints(&boundary);
    let handoff = registry
        .prepare_policy(
            session.clone(),
            Some(&compacted),
            true,
            &[new.clone(), old.clone()],
        )
        .unwrap();
    assert!(handoff.handoff);
    assert_eq!(
        handoff.decision(Some(&compacted), &old.id),
        crate::usage::model::RoutingDecision::NoEligibleReceiver
    );
    assert_eq!(
        handoff.decision(Some(&compacted), &new.id),
        crate::usage::model::RoutingDecision::CompactionHandoff
    );
    drop(handoff);

    // A failed receiver releases only the claim. Retrying the same boundary
    // must claim it again, while the old owner remains ineligible for it.
    let retry = registry
        .prepare_policy(session, Some(&compacted), true, &[new.clone(), old.clone()])
        .unwrap();
    assert!(retry.handoff);
    assert_eq!(
        retry.decision(Some(&compacted), &old.id),
        crate::usage::model::RoutingDecision::NoEligibleReceiver
    );
}

#[test]
fn v0212_successful_handoff_continuation_is_existing_thread_not_window_mismatch() {
    let dir = tempfile::tempdir().unwrap();
    let registry = Registry::new(dir.path().join("owners.json"));
    let old = policy_provider("fixture-old", true, false);
    let new = policy_provider("fixture-new", true, false);
    let session = crate::storage::digest(b"fixture-v0212-successful-handoff");
    let boundary = crate::storage::digest(b"fixture-v0212-window");
    seed_boundary(&registry, &session, &old, &new, &boundary);

    let compacted = compacted_hints(&boundary);
    let handoff = registry
        .prepare_policy(
            session.clone(),
            Some(&compacted),
            true,
            &[new.clone(), old.clone()],
        )
        .unwrap();
    assert!(handoff.handoff);
    handoff.complete(&new.id, None);
    drop(handoff);
    assert!(!registry.boundary_matches(&session, Some(&boundary)));

    let continuation = registry
        .prepare_policy(session, Some(&compacted), true, &[new.clone(), old.clone()])
        .unwrap();
    assert!(!continuation.handoff);
    assert_eq!(continuation.owner.as_deref(), Some(new.id.as_str()));
    assert_eq!(
        continuation.decision(Some(&compacted), &new.id),
        crate::usage::model::RoutingDecision::ExistingThread
    );
}

#[test]
fn v0212_cursor_pinning_and_no_receiver_are_distinct_third_thread_decisions() {
    let dir = tempfile::tempdir().unwrap();
    let registry = Registry::new(dir.path().join("owners.json"));
    let owner = policy_provider("fixture-owner", false, false);
    let neutral = policy_provider("fixture-neutral", false, false);
    let handoff = policy_provider("fixture-handoff", true, false);

    let cursor_session = crate::storage::digest(b"fixture-v0212-cursor-thread");
    let cursor_boundary = crate::storage::digest(b"fixture-v0212-cursor-window");
    seed_boundary(
        &registry,
        &cursor_session,
        &owner,
        &handoff,
        &cursor_boundary,
    );
    let cursor = RequestHints {
        previous_response_id: Some("fixture-old-response".into()),
        compacted_window: Some(cursor_boundary),
        ..Default::default()
    };
    let cursor_lease = registry
        .prepare_policy(
            cursor_session.clone(),
            Some(&cursor),
            false,
            &[handoff.clone(), owner.clone()],
        )
        .unwrap();
    assert!(!cursor_lease.handoff);
    assert_eq!(
        compaction::decision(
            Some(&cursor_session),
            Some(&cursor),
            true,
            Some(&cursor_lease),
            &owner.id,
        ),
        crate::usage::model::RoutingDecision::CursorPinned
    );

    let no_receiver_session = crate::storage::digest(b"fixture-v0212-no-receiver-thread");
    let no_receiver_boundary = crate::storage::digest(b"fixture-v0212-no-receiver-window");
    seed_boundary(
        &registry,
        &no_receiver_session,
        &owner,
        &neutral,
        &no_receiver_boundary,
    );
    let no_receiver = compacted_hints(&no_receiver_boundary);
    let no_receiver_lease = registry
        .prepare_policy(
            no_receiver_session.clone(),
            Some(&no_receiver),
            true,
            &[neutral, owner.clone()],
        )
        .unwrap();
    assert!(no_receiver_lease.handoff);
    assert_eq!(
        compaction::decision(
            Some(&no_receiver_session),
            Some(&no_receiver),
            false,
            Some(&no_receiver_lease),
            &owner.id,
        ),
        crate::usage::model::RoutingDecision::NoEligibleReceiver
    );
}

#[test]
fn v0212_in_flight_and_missing_compaction_windows_are_not_mismatches() {
    let dir = tempfile::tempdir().unwrap();
    let registry = Registry::new(dir.path().join("owners.json"));
    let owner = policy_provider("fixture-owner", true, false);
    let receiver = policy_provider("fixture-receiver", true, false);
    let session = crate::storage::digest(b"fixture-v0212-window-states");
    let boundary = crate::storage::digest(b"fixture-v0212-window-state");
    seed_boundary(&registry, &session, &owner, &receiver, &boundary);

    let compacted = compacted_hints(&boundary);
    let handoff = registry
        .prepare_policy(
            session.clone(),
            Some(&compacted),
            true,
            &[receiver.clone(), owner.clone()],
        )
        .unwrap();
    assert!(handoff.handoff);

    let ordinary = registry
        .prepare_policy(
            session.clone(),
            Some(&compacted),
            false,
            &[owner.clone(), receiver.clone()],
        )
        .unwrap();
    assert!(!ordinary.handoff);
    assert_eq!(
        ordinary.decision(Some(&compacted), &owner.id),
        crate::usage::model::RoutingDecision::BoundaryInFlight
    );

    let missing = registry
        .prepare_policy(
            session.clone(),
            Some(&ordinary_hints()),
            false,
            &[owner.clone(), receiver.clone()],
        )
        .unwrap();
    assert_eq!(
        missing.decision(Some(&ordinary_hints()), &owner.id),
        crate::usage::model::RoutingDecision::CompactionWindowMissing
    );
    drop(missing);
    drop(ordinary);
    drop(handoff);
}

fn provider_routes(gateway: &Gateway) -> Vec<super::Route> {
    gateway
        .view()
        .providers
        .iter()
        .map(|provider| gateway.route(&provider.id).unwrap())
        .collect()
}

async fn seed_retained_owner(
    gateway: &Gateway,
    routes: &[super::Route],
    providers: &[Provider],
    session: &str,
) -> (String, String) {
    let owner = providers[0].clone();
    let other = providers[1].clone();
    let lease = gateway
        .0
        .compaction
        .prepare_policy(
            session.to_owned(),
            Some(&ordinary_hints()),
            false,
            &[owner.clone(), other],
        )
        .unwrap();
    let mut budget = Budget::new(5);
    budget.set_intake(Intake::Ordinary, None);
    budget.track(&gateway.0.admission, Some(&lease), 30);
    let mut admission = gateway
        .0
        .admission
        .acquire_for_immediate(
            &routes[0..1],
            false,
            10,
            &mut budget,
            &Requirement::Resource,
            true,
        )
        .await
        .unwrap();
    assert_eq!(admission.route.provider.id, owner.id);
    admission.commit_rpm().unwrap();
    admission.slot.complete();
    drop(admission);
    drop(budget);
    lease.complete(
        &owner.id,
        Some(&crate::storage::digest(b"fixture-v0212-capacity-window")),
    );
    let boundary = crate::storage::digest(b"fixture-v0212-capacity-window");
    (owner.id, boundary)
}

async fn configure_capacity_fixture(
    urls: Vec<String>,
) -> (tempfile::TempDir, Gateway, Vec<super::Route>, Vec<Provider>) {
    let (dir, gateway) = super::fixture(urls).await;
    for provider in gateway.view().providers {
        super::update(
            &gateway,
            &dir,
            Edit::ConcurrencyProvider {
                id: provider.id,
                max_concurrency: 1,
            },
        );
    }
    super::start(&gateway, &dir).await;
    gateway.0.admission.configure_retention(30);
    let routes = provider_routes(&gateway);
    let providers: Vec<_> = routes.iter().map(|route| route.provider.clone()).collect();
    (dir, gateway, routes, providers)
}

fn enable_receiver_before_owner(
    gateway: &Gateway,
    dir: &tempfile::TempDir,
    owner_id: &str,
    receiver_id: &str,
) -> (Vec<super::Route>, Vec<Provider>) {
    let revision = gateway.view().revision;
    gateway
        .edit(
            Edit::TakeoverProvider {
                id: receiver_id.to_owned(),
                handoff_after_compaction: Some(true),
                take_new_threads: None,
            },
            &revision,
            dir.path(),
        )
        .unwrap();
    let mut ids: Vec<_> = gateway
        .view()
        .providers
        .iter()
        .map(|p| p.id.clone())
        .collect();
    ids.retain(|id| id != owner_id && id != receiver_id);
    ids.insert(0, owner_id.to_owned());
    ids.insert(0, receiver_id.to_owned());
    gateway
        .edit(Edit::Reorder { ids }, &gateway.view().revision, dir.path())
        .unwrap();
    let routes = provider_routes(gateway);
    let providers = routes.iter().map(|route| route.provider.clone()).collect();
    (routes, providers)
}

#[tokio::test]
async fn v0212_failed_handoff_releases_receiver_reservation_but_keeps_owner_reservation() {
    let (dir, gateway) = super::fixture(vec![
        "https://fixture-old.example.invalid/v1".into(),
        "https://fixture-new.example.invalid/v1".into(),
    ])
    .await;
    for provider in gateway.view().providers {
        super::update(
            &gateway,
            &dir,
            Edit::ConcurrencyProvider {
                id: provider.id,
                max_concurrency: 1,
            },
        );
    }
    super::start(&gateway, &dir).await;
    gateway.0.admission.configure_retention(30);
    let routes = provider_routes(&gateway);
    let providers: Vec<_> = routes.iter().map(|route| route.provider.clone()).collect();
    let session = crate::storage::digest(b"fixture-v0212-capacity-failure");
    let (owner_id, boundary) = seed_retained_owner(&gateway, &routes, &providers, &session).await;
    let receiver_id = providers[1].id.clone();
    let (routes, providers) = enable_receiver_before_owner(&gateway, &dir, &owner_id, &receiver_id);
    assert_eq!(gateway.0.admission.reserved(&owner_id), 1);

    let compacted = compacted_hints(&boundary);
    let handoff = gateway
        .0
        .compaction
        .prepare_policy(
            session,
            Some(&compacted),
            true,
            &[providers[0].clone(), providers[1].clone()],
        )
        .unwrap();
    let mut budget = Budget::new(5);
    budget.set_intake(Intake::Compacted, Some(owner_id.clone()));
    budget.track(&gateway.0.admission, Some(&handoff), 30);
    assert!(handoff.handoff);
    let candidates = handoff.candidates(routes.clone());
    let mut admission = gateway
        .0
        .admission
        .acquire_for_immediate(
            &candidates,
            false,
            10,
            &mut budget,
            &Requirement::Resource,
            true,
        )
        .await
        .unwrap();
    assert_eq!(admission.route.provider.id, receiver_id);
    admission.commit_rpm().unwrap();
    // Dropping without completion models a receiver that failed before a
    // usable response. Its reservation must not block another request.
    drop(admission);
    drop(budget);
    drop(handoff);
    assert_eq!(gateway.0.admission.reserved(&receiver_id), 0);
    assert_eq!(gateway.0.admission.reserved(&owner_id), 1);
    let (_, waiting) = gateway.0.admission.counts();
    assert_eq!(waiting, 0);
    gateway.stop().await.unwrap();
}

#[tokio::test]
async fn v0212_successful_receiver_is_reserved_until_idle_retention_expires_and_counts_stay_active_only(
) {
    let (dir, gateway) = super::fixture(vec![
        "https://fixture-old.example.invalid/v1".into(),
        "https://fixture-new.example.invalid/v1".into(),
    ])
    .await;
    for provider in gateway.view().providers {
        super::update(
            &gateway,
            &dir,
            Edit::ConcurrencyProvider {
                id: provider.id,
                max_concurrency: 1,
            },
        );
    }
    super::start(&gateway, &dir).await;
    gateway.0.admission.configure_retention(30);
    let routes = provider_routes(&gateway);
    let providers: Vec<_> = routes.iter().map(|route| route.provider.clone()).collect();
    let session = crate::storage::digest(b"fixture-v0212-capacity-success");
    let (owner_id, boundary) = seed_retained_owner(&gateway, &routes, &providers, &session).await;
    let receiver_id = providers[1].id.clone();
    let (routes, providers) = enable_receiver_before_owner(&gateway, &dir, &owner_id, &receiver_id);
    let compacted = compacted_hints(&boundary);
    let handoff = gateway
        .0
        .compaction
        .prepare_policy(
            session,
            Some(&compacted),
            true,
            &[providers[0].clone(), providers[1].clone()],
        )
        .unwrap();
    let mut budget = Budget::new(5);
    budget.set_intake(Intake::Compacted, Some(owner_id.clone()));
    budget.track(&gateway.0.admission, Some(&handoff), 30);
    assert!(handoff.handoff);
    let candidates = handoff.candidates(routes.clone());
    let mut admission = gateway
        .0
        .admission
        .acquire_for_immediate(
            &candidates,
            false,
            10,
            &mut budget,
            &Requirement::Resource,
            true,
        )
        .await
        .unwrap();
    assert_eq!(admission.route.provider.id, receiver_id);
    admission.commit_rpm().unwrap();
    admission.slot.complete();
    drop(admission);
    drop(budget);
    drop(handoff);
    assert_eq!(gateway.0.admission.reserved(&receiver_id), 1);
    assert_eq!(gateway.0.admission.reserved(&owner_id), 1);
    let (active, waiting) = gateway.0.admission.counts();
    assert_eq!(waiting, 0);
    assert_eq!(active.get(&receiver_id).copied().unwrap_or_default(), 0);
    assert_eq!(active.get(&owner_id).copied().unwrap_or_default(), 0);

    gateway.0.admission.configure_retention(0);
    let _ = gateway.0.admission.counts();
    assert_eq!(gateway.0.admission.reserved(&receiver_id), 0);
    assert_eq!(gateway.0.admission.reserved(&owner_id), 0);
    gateway.stop().await.unwrap();
}

#[tokio::test]
async fn v0212_full_receiver_falls_back_to_owner_and_marks_capacity_wait() {
    let (_dir, gateway, routes, providers) = configure_capacity_fixture(vec![
        "https://fixture-old.example.invalid/v1".into(),
        "https://fixture-new.example.invalid/v1".into(),
    ])
    .await;
    let session = crate::storage::digest(b"fixture-v0212-capacity-fallback");
    let (owner_id, boundary) = seed_retained_owner(&gateway, &routes, &providers, &session).await;
    let receiver_id = providers[1].id.clone();
    let (routes, providers) =
        enable_receiver_before_owner(&gateway, &_dir, &owner_id, &receiver_id);

    // Occupy the preferred receiver with an unrelated request. The handoff
    // budget must continue on the retained owner instead of waiting forever.
    let mut receiver_budget = Budget::new(2);
    receiver_budget.set_intake(Intake::Compacted, Some(receiver_id.clone()));
    let mut receiver = gateway
        .0
        .admission
        .acquire_for_immediate(
            &routes[0..1],
            false,
            10,
            &mut receiver_budget,
            &Requirement::Resource,
            true,
        )
        .await
        .unwrap();
    receiver.commit_rpm().unwrap();
    assert_eq!(receiver.route.provider.id, receiver_id);

    let compacted = compacted_hints(&boundary);
    let handoff = gateway
        .0
        .compaction
        .prepare_policy(
            session.clone(),
            Some(&compacted),
            true,
            &[providers[0].clone(), providers[1].clone()],
        )
        .unwrap();
    let mut budget = Budget::new(2);
    budget.set_intake(Intake::Compacted, Some(owner_id.clone()));
    budget.track(&gateway.0.admission, Some(&handoff), 30);
    assert!(handoff.handoff);
    let candidates = handoff.candidates(routes.clone());
    let mut admission = gateway
        .0
        .admission
        .acquire_for_immediate(
            &candidates,
            false,
            10,
            &mut budget,
            &Requirement::Resource,
            true,
        )
        .await
        .unwrap();
    assert_eq!(admission.route.provider.id, owner_id);
    assert_eq!(
        admission.handoff_wait,
        Some(crate::usage::model::RoutingDecision::ReceiverCapacityFull)
    );
    admission.commit_rpm().unwrap();
    admission.slot.complete();
    drop(admission);
    drop(budget);
    handoff.complete(&owner_id, None);
    drop(handoff);
    drop(receiver);
    drop(receiver_budget);
    let continuation = gateway
        .0
        .compaction
        .prepare_policy(
            session,
            Some(&compacted),
            true,
            &[providers[0].clone(), providers[1].clone()],
        )
        .unwrap();
    assert!(continuation.handoff);
    assert_eq!(
        continuation.decision(Some(&compacted), &owner_id),
        crate::usage::model::RoutingDecision::NoEligibleReceiver
    );
    let mut continuation_budget = Budget::new(2);
    continuation_budget.set_intake(Intake::Compacted, Some(owner_id.clone()));
    continuation_budget.track(&gateway.0.admission, Some(&continuation), 30);
    let continuation_candidates = continuation.candidates(routes);
    let mut receiver_admission = gateway
        .0
        .admission
        .acquire_for_immediate(
            &continuation_candidates,
            false,
            10,
            &mut continuation_budget,
            &Requirement::Resource,
            true,
        )
        .await
        .unwrap();
    assert_eq!(receiver_admission.route.provider.id, receiver_id);
    receiver_admission.commit_rpm().unwrap();
    receiver_admission.slot.complete();
    drop(receiver_admission);
    drop(continuation_budget);
    continuation.complete(&receiver_id, None);
    gateway.stop().await.unwrap();
}

#[tokio::test]
async fn v0212_delayed_tail_and_slow_fragmented_sse_preserve_exact_bytes() {
    let frames: Vec<&'static [u8]> = vec![
        b"data: {\"type\":\"response.created\",\"response\":{\"id\":\"fixture-v0212\"}}\n\n",
        b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"a\"}\n\n",
        b"data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n",
        b": delayed-tail\r\n\r\n",
    ];
    let expected = frames.concat();
    let upstream = super::server(move |_| {
        let frames = frames.clone();
        async move {
            let body = async_stream::try_stream! {
                for frame in frames {
                    tokio::time::sleep(Duration::from_millis(15)).await;
                    yield Frame::data(Bytes::copy_from_slice(frame));
                }
            };
            Response::builder()
                .header("content-type", "text/event-stream")
                .body(StreamBody::new(body).boxed_unsync())
                .unwrap()
        }
    })
    .await;
    let (dir, gateway) = super::fixture(vec![format!("http://127.0.0.1:{upstream}/v1")]).await;
    super::start(&gateway, &dir).await;
    let response = super::request(
        &gateway,
        "fixture-v0212-stream",
        br#"{"model":"fixture-model","stream":true,"input":[{"role":"user","content":"fixture"}]}"#
            .to_vec(),
    )
    .await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.into_body().collect().await.unwrap().to_bytes(),
        expected
    );
    gateway.stop().await.unwrap();
}

#[tokio::test]
async fn v0212_http_completed_event_keeps_slot_until_delayed_tail_is_forwarded() {
    let completed = Bytes::from_static(
        b"data: {\"type\":\"response.completed\",\"response\":{\"id\":\"fixture-v0212-http-tail\",\"status\":\"completed\"}}\n\n",
    );
    let tail = Bytes::from_static(b": fixture-v0212-http-tail\r\n\r\n");
    let expected = [completed.as_ref(), tail.as_ref()].concat();
    let release_tail = std::sync::Arc::new(tokio::sync::Notify::new());
    let release_tail_copy = release_tail.clone();
    let completed_copy = completed.clone();
    let upstream = super::server(move |_| {
        let completed = completed_copy.clone();
        let tail = tail.clone();
        let release_tail = release_tail_copy.clone();
        async move {
            let body = async_stream::try_stream! {
                yield Frame::data(completed);
                release_tail.notified().await;
                yield Frame::data(tail);
            };
            Response::builder()
                .header("content-type", "text/event-stream")
                .body(StreamBody::new(body).boxed_unsync())
                .unwrap()
        }
    })
    .await;
    let (dir, gateway) = super::fixture(vec![format!("http://127.0.0.1:{upstream}/v1")]).await;
    for provider in gateway.view().providers {
        super::update(
            &gateway,
            &dir,
            Edit::ConcurrencyProvider {
                id: provider.id,
                max_concurrency: 1,
            },
        );
    }
    super::start(&gateway, &dir).await;
    gateway.0.admission.configure_retention(30);
    let provider_id = gateway.view().providers[0].id.clone();
    let response = super::request(
        &gateway,
        "fixture-v0212-http-tail",
        br#"{"model":"fixture-model","stream":true,"input":[{"role":"user","content":"fixture"}]}"#
            .to_vec(),
    )
    .await;
    assert_eq!(response.status(), 200);
    let mut body = response.into_body();
    let first = tokio::time::timeout(Duration::from_secs(2), body.frame())
        .await
        .expect("completed event was not forwarded")
        .expect("gateway ended before the completed event")
        .expect("gateway failed before the completed event");
    assert_eq!(
        first.data_ref().map(|bytes| bytes.as_ref()),
        Some(completed.as_ref())
    );
    let view = gateway.view();
    assert_eq!(
        view.providers
            .iter()
            .find(|provider| provider.id == provider_id)
            .map(|provider| provider.active_requests),
        Some(1),
    );

    let mut forwarded = first
        .into_data()
        .expect("completed event was not data")
        .to_vec();
    release_tail.notify_one();
    loop {
        match tokio::time::timeout(Duration::from_secs(2), body.frame())
            .await
            .expect("delayed tail was not forwarded")
        {
            Some(Ok(frame)) => {
                let data = frame
                    .into_data()
                    .expect("gateway forwarded a non-data frame");
                forwarded.extend_from_slice(data.as_ref());
            }
            Some(Err(error)) => panic!("gateway failed while forwarding delayed tail: {error}"),
            None => break,
        }
    }
    assert_eq!(forwarded, expected);
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let provider = gateway
                .view()
                .providers
                .into_iter()
                .find(|provider| provider.id == provider_id)
                .expect("fixture provider disappeared");
            if provider.active_requests == 0 && provider.reserved_threads == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("HTTP slot did not settle with one retained thread");
    gateway.stop().await.unwrap();
}

#[tokio::test]
async fn v0212_terminal_sse_forwards_a_large_delayed_tail_byte_for_byte() {
    let mut frames = vec![
        Bytes::from_static(
            b"data: {\"type\":\"response.created\",\"response\":{\"id\":\"fixture-v0212-tail\"}}\n\n",
        ),
        Bytes::from_static(b"data: [DONE]\n\n"),
    ];
    for index in 0..5 {
        let mut frame = format!(": fixture-tail-{index} ").into_bytes();
        frame.extend(std::iter::repeat_n(b'x', 56 * 1024));
        frame.extend_from_slice(b"\r\n\r\n");
        frames.push(Bytes::from(frame));
    }
    let expected = frames
        .iter()
        .flat_map(|frame| frame.iter().copied())
        .collect::<Vec<_>>();
    let upstream = super::server({
        let frames = frames.clone();
        move |_| {
            let frames = frames.clone();
            async move {
                let body = async_stream::try_stream! {
                    for (index, frame) in frames.into_iter().enumerate() {
                        if index >= 2 {
                            tokio::time::sleep(Duration::from_millis(450)).await;
                        }
                        yield Frame::data(frame);
                    }
                };
                Response::builder()
                    .header("content-type", "text/event-stream")
                    .body(StreamBody::new(body).boxed_unsync())
                    .unwrap()
            }
        }
    })
    .await;
    let (dir, gateway) = super::fixture(vec![format!("http://127.0.0.1:{upstream}/v1")]).await;
    super::start(&gateway, &dir).await;
    let response = super::request(
        &gateway,
        "fixture-v0212-large-tail",
        br#"{"model":"fixture-model","stream":true,"input":[{"role":"user","content":"fixture"}]}"#
            .to_vec(),
    )
    .await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.into_body().collect().await.unwrap().to_bytes(),
        expected.as_slice()
    );
    gateway.stop().await.unwrap();
}

#[tokio::test]
async fn v0212_first_event_fragments_use_idle_deadline_after_first_byte() {
    let event = b"data: {\"type\":\"response.created\",\"response\":{\"id\":\"fixture-v0212-slow-first\"}}\n\n";
    let split = vec![
        Bytes::copy_from_slice(&event[..12]),
        Bytes::copy_from_slice(&event[12..48]),
        Bytes::copy_from_slice(&event[48..]),
    ];
    let expected = split
        .iter()
        .flat_map(|frame| frame.iter().copied())
        .collect::<Vec<_>>();
    let upstream = super::server({
        let split = split.clone();
        move |_| {
            let split = split.clone();
            async move {
                let body = async_stream::try_stream! {
                    for (index, frame) in split.into_iter().enumerate() {
                        if index != 0 {
                            tokio::time::sleep(Duration::from_millis(1_200)).await;
                        }
                        yield Frame::data(frame);
                    }
                };
                Response::builder()
                    .header("content-type", "text/event-stream")
                    .body(StreamBody::new(body).boxed_unsync())
                    .unwrap()
            }
        }
    })
    .await;
    let (dir, gateway) = super::fixture(vec![format!("http://127.0.0.1:{upstream}/v1")]).await;
    let mut settings = gateway.view().settings;
    settings.first_byte_seconds = 1;
    settings.idle_seconds = 2;
    settings.max_retries = 0;
    super::update(&gateway, &dir, Edit::Settings { settings });
    super::start(&gateway, &dir).await;
    let response = super::request(
        &gateway,
        "fixture-v0212-slow-first",
        br#"{"model":"fixture-model","stream":true,"input":[{"role":"user","content":"fixture"}]}"#
            .to_vec(),
    )
    .await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.into_body().collect().await.unwrap().to_bytes(),
        expected.as_slice()
    );
    gateway.stop().await.unwrap();
}

#[tokio::test]
async fn v0212_model_error_tail_uses_rolling_idle_deadline() {
    let frames = vec![
        Bytes::from_static(
            b"data: {\"type\":\"response.failed\",\"error\":{\"code\":\"model_not_found\",\"message\":\"fixture\"}}\n\n",
        ),
        Bytes::from_static(b": fixture-error-tail-1\r\n\r\n"),
        Bytes::from_static(b": fixture-error-tail-2\r\n\r\n"),
        Bytes::from_static(b": fixture-error-tail-3\r\n\r\n"),
    ];
    let expected = frames
        .iter()
        .flat_map(|frame| frame.iter().copied())
        .collect::<Vec<_>>();
    let upstream = super::server({
        let frames = frames.clone();
        move |_| {
            let frames = frames.clone();
            async move {
                let body = async_stream::try_stream! {
                    for (index, frame) in frames.into_iter().enumerate() {
                        if index != 0 {
                            tokio::time::sleep(Duration::from_millis(700)).await;
                        }
                        yield Frame::data(frame);
                    }
                };
                Response::builder()
                    .status(404)
                    .header("content-type", "text/event-stream")
                    .body(StreamBody::new(body).boxed_unsync())
                    .unwrap()
            }
        }
    })
    .await;
    let (dir, gateway) = super::fixture(vec![format!("http://127.0.0.1:{upstream}/v1")]).await;
    let mut settings = gateway.view().settings;
    settings.idle_seconds = 1;
    settings.max_retries = 0;
    super::update(&gateway, &dir, Edit::Settings { settings });
    super::start(&gateway, &dir).await;
    let response = super::request(
        &gateway,
        "fixture-v0212-error-tail",
        br#"{"model":"fixture-model","stream":true,"input":[{"role":"user","content":"fixture"}]}"#
            .to_vec(),
    )
    .await;
    assert_eq!(response.status(), 404);
    assert_eq!(
        response.into_body().collect().await.unwrap().to_bytes(),
        expected.as_slice()
    );
    gateway.stop().await.unwrap();
}

#[tokio::test]
async fn v0212_wire_progress_ignores_control_frames_and_records_split_data_frames() {
    let activity = Arc::new(wire_progress::Activity::default());
    activity.websocket();
    let (mut sender, receiver) = tokio::io::duplex(128);
    let mut tracked = wire_progress::Tracked::new(receiver, activity.clone());
    sender.write_all(&[0x89, 0x00]).await.unwrap();
    let mut control = [0; 2];
    tokio::io::AsyncReadExt::read_exact(&mut tracked, &mut control)
        .await
        .unwrap();
    assert!(activity.read_at().is_none());

    sender.write_all(&[0x81]).await.unwrap();
    sender.write_all(&[0x05, b'f', b'i']).await.unwrap();
    sender.write_all(b"xtu").await.unwrap();
    let mut data = [0; 7];
    tokio::io::AsyncReadExt::read_exact(&mut tracked, &mut data)
        .await
        .unwrap();
    assert_eq!(&data, &[0x81, 0x05, b'f', b'i', b'x', b't', b'u']);
    assert!(activity.read_at().is_some());
}

#[tokio::test]
async fn v0212_socket_write_progress_extends_the_idle_deadline_until_the_tail_finishes() {
    let activity = Arc::new(wire_progress::Activity::default());
    let writer = wire_progress::Tracked::new(tokio::io::sink(), activity.clone());
    let result = wire_progress::writing(
        async move {
            let mut writer = writer;
            for _ in 0..3 {
                writer.write_all(b"fixture-progress").await.unwrap();
                tokio::time::sleep(Duration::from_millis(550)).await;
            }
            Ok::<_, ()>(())
        },
        &activity,
        1,
    )
    .await;
    assert!(result.is_ok(), "write progress must refresh the deadline");
    assert!(activity.write_at() > tokio::time::Instant::now() - Duration::from_secs(3));
}

#[tokio::test]
async fn v0212_ws_reconnect_allows_slow_handshake_before_first_event_deadline() {
    let created = r#"{"type":"response.created","response":{"id":"fixture-v0212-ws"}}"#;
    let completed = r#"{"type":"response.completed","response":{"id":"fixture-v0212-ws","status":"completed"}}"#;
    let handshakes = Arc::new(AtomicUsize::new(0));
    let frames = Arc::new(AtomicUsize::new(0));
    let handshakes_copy = handshakes.clone();
    let frames_copy = frames.clone();
    let upstream = super::server(move |mut request| {
        let handshakes = handshakes_copy.clone();
        let frames = frames_copy.clone();
        async move {
            let attempt = handshakes.fetch_add(1, Ordering::SeqCst) + 1;
            if attempt == 2 {
                tokio::time::sleep(Duration::from_millis(1_200)).await;
            }
            let (response, upgrade) = yawc::WebSocket::upgrade_with_options(
                &mut request,
                yawc::Options::default().with_balanced_compression(),
            )
            .unwrap();
            tokio::spawn(async move {
                let Ok(mut socket) = upgrade.await else {
                    return;
                };
                while let Some(frame) = socket.next().await {
                    if frame.opcode() == yawc::OpCode::Close {
                        break;
                    }
                    if frame.opcode().is_control() {
                        continue;
                    }
                    frames.fetch_add(1, Ordering::SeqCst);
                    if socket
                        .send(yawc::Frame::text(created.to_owned()))
                        .await
                        .is_err()
                    {
                        return;
                    }
                    let _ = socket.send(yawc::Frame::text(completed.to_owned())).await;
                    break;
                }
            });
            response.map(|_| replay::empty())
        }
    })
    .await;
    let (dir, gateway) = super::fixture(vec![format!("http://127.0.0.1:{upstream}/v1")]).await;
    let mut settings = gateway.view().settings;
    settings.first_byte_seconds = 1;
    settings.connect_seconds = 3;
    settings.websocket_retry_seconds = 1;
    settings.max_retries = 0;
    super::update(&gateway, &dir, Edit::Settings { settings });
    super::update(
        &gateway,
        &dir,
        Edit::RpmProvider {
            id: gateway.view().providers[0].id.clone(),
            max_rpm: 10,
        },
    );
    super::start(&gateway, &dir).await;

    let token = gateway.0.inner.lock().unwrap().store.local_token.clone();
    let mut client = yawc::WebSocket::connect(
        format!(
            "ws://127.0.0.1:{}/v1/responses",
            gateway.view().settings.port
        )
        .parse()
        .unwrap(),
    )
    .with_request(
        yawc::HttpRequest::builder()
            .header("authorization", format!("Bearer {token}"))
            .header("session_id", "fixture-v0212-ws"),
    )
    .await
    .unwrap();

    for expected in ["response.created", "response.completed"] {
        if expected == "response.created" {
            client
                .send(yawc::Frame::text(
                    r#"{"type":"response.create","model":"fixture-model","input":"first"}"#
                        .to_owned(),
                ))
                .await
                .unwrap();
        }
        let frame = tokio::time::timeout(Duration::from_secs(5), client.next())
            .await
            .expect("fixture websocket timed out")
            .expect("fixture websocket closed");
        assert_eq!(frame.opcode(), yawc::OpCode::Text);
        let value: serde_json::Value = serde_json::from_slice(frame.payload()).unwrap();
        assert_eq!(value["type"], expected);
    }

    client
        .send(yawc::Frame::text(
            r#"{"type":"response.create","model":"fixture-model","input":"second"}"#.to_owned(),
        ))
        .await
        .unwrap();
    for expected in ["response.created", "response.completed"] {
        let frame = tokio::time::timeout(Duration::from_secs(5), client.next())
            .await
            .expect("fixture websocket timed out during reconnect")
            .expect("fixture websocket closed during reconnect");
        assert_eq!(frame.opcode(), yawc::OpCode::Text);
        let value: serde_json::Value = serde_json::from_slice(frame.payload()).unwrap();
        assert_eq!(value["type"], expected);
    }

    assert_eq!(handshakes.load(Ordering::SeqCst), 2);
    assert_eq!(frames.load(Ordering::SeqCst), 2);
    tokio::time::timeout(Duration::from_secs(3), async {
        while gateway.view().providers[0].active_requests != 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("websocket request slot did not settle");
    let view = gateway.view();
    assert_eq!(view.providers[0].active_requests, 0);
    assert_eq!(view.providers[0].health.failures, 0);
    assert_eq!(view.providers[0].rpm_used, 2);

    client
        .send(yawc::Frame::close(1000.into(), "fixture done"))
        .await
        .unwrap();
    drop(client);
    gateway.stop().await.unwrap();
}
