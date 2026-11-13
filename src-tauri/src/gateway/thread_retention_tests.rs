// 覆盖并发上限调整与接管交付门槛。
//! Concurrency-limit and delivery-gate fixtures.
//!
//! This module is registered below the v0.20 fixture module so the loopback
//! gateway helpers remain available. Every persisted value is synthetic.
use crate::gateway::test_support::compacted_hints;
use crate::gateway::{
    admission::{Budget, Rejected},
    compaction::Registry,
    model::{Edit, Provider, Settings, Store},
    protocol::Protocol,
    replay::RequestHints,
    routing::Requirement,
};
use serde_json::json;

fn fixture_provider(id: &str, handoff: bool, take: bool) -> Provider {
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
        search_only: false,
    }
}
fn ordinary_hints() -> RequestHints {
    RequestHints::from_value(
        json!({"model":"fixture-model","input":[{"role":"user","content":"fixture"}]}),
        false,
    )
    .expect("fixture request metadata")
}

#[test]
fn settings_defaults_and_load_are_idempotent() {
    let defaults = Settings::default();
    assert_eq!(defaults.first_byte_seconds, 300);
    assert_eq!(defaults.idle_seconds, 300);

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("gateway.json");
    let mut value = serde_json::to_value(Store::default()).unwrap();
    value["settings"]["firstByteSeconds"] = 60.into();
    value["settings"]["idleSeconds"] = 120.into();
    std::fs::write(&path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();

    let (migrated, old_revision) = Store::load(&path).unwrap();
    assert_eq!(migrated.settings.first_byte_seconds, 60);
    assert_eq!(migrated.settings.idle_seconds, 120);
    let new_revision = migrated.persist(&path, &old_revision).unwrap();

    let (loaded, revision) = Store::load(&path).unwrap();
    assert_eq!(loaded.settings, migrated.settings);
    assert_eq!(revision, new_revision);
    let bytes = std::fs::read(&path).unwrap();
    assert_eq!(loaded.persist(&path, &revision).unwrap(), new_revision);
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
}

#[tokio::test]
async fn scheduler_lowering_limit_preserves_existing_active_slots() {
    let (dir, gateway) = super::fixture(vec![
        "https://fixture-active.example.invalid/v1".into(),
        "https://fixture-spare.example.invalid/v1".into(),
    ])
    .await;
    let active_id = gateway.view().providers[0].id.clone();
    super::update(
        &gateway,
        &dir,
        Edit::ConcurrencyProvider {
            id: active_id.clone(),
            max_concurrency: 2,
        },
    );
    super::start(&gateway, &dir).await;
    let route = gateway.route(&active_id).unwrap();
    let mut first_budget = Budget::new(2);
    let mut first = gateway
        .0
        .admission
        .acquire_for_immediate(
            std::slice::from_ref(&route),
            false,
            10,
            &mut first_budget,
            &Requirement::Resource,
            true,
        )
        .await
        .unwrap();
    first.commit_rpm();
    let mut second_budget = Budget::new(2);
    let mut second = gateway
        .0
        .admission
        .acquire_for_immediate(
            std::slice::from_ref(&route),
            false,
            10,
            &mut second_budget,
            &Requirement::Resource,
            true,
        )
        .await
        .unwrap();
    second.commit_rpm();
    assert_eq!(gateway.view().providers[0].active_requests, 2);

    super::update(
        &gateway,
        &dir,
        Edit::ConcurrencyProvider {
            id: active_id.clone(),
            max_concurrency: 1,
        },
    );
    assert_eq!(gateway.view().providers[0].active_requests, 2);
    let mut third_budget = Budget::new(0);
    let result = gateway
        .0
        .admission
        .acquire_for_immediate(
            std::slice::from_ref(&route),
            false,
            10,
            &mut third_budget,
            &Requirement::Resource,
            true,
        )
        .await;
    assert!(matches!(result, Err(Rejected::Timeout)));

    drop(first);
    drop(second);
    drop(first_budget);
    drop(second_budget);
    assert_eq!(gateway.view().providers[0].active_requests, 0);
    gateway.stop().await.unwrap();
}

#[test]
fn protocol_receiver_handoff_commits_only_after_terminal_delivery() {
    let dir = tempfile::tempdir().unwrap();
    let registry = Registry::new(dir.path().join("owners.json"));
    let old = fixture_provider("fixture-old", true, false);
    let new = fixture_provider("fixture-new", true, false);
    let session = crate::storage::digest(b"fixture-delivery-gate");
    let boundary = crate::storage::digest(b"fixture-delivery-window");
    let initial = registry
        .prepare_policy(
            session.clone(),
            Some(&ordinary_hints()),
            false,
            &[old.clone(), new.clone()],
        )
        .unwrap();
    assert!(initial.admitted(&old.id));
    initial.complete(&old.id, Some(&boundary));
    let handoff = registry
        .prepare_policy(
            session.clone(),
            Some(&compacted_hints(&boundary)),
            true,
            &[new.clone(), old.clone()],
        )
        .unwrap();
    assert!(handoff.handoff);
    assert_eq!(registry.owner(&session).as_deref(), Some(old.id.as_str()));

    let mut protocol = Protocol::new(true);
    protocol.attach_ownership(Some(handoff.clone()), &new.id);
    protocol.response(200, true, "identity");
    protocol.feed(
        br#"data: {"type":"response.completed","response":{"status":"completed"}}

"#,
    );
    assert_eq!(
        protocol.terminal(),
        Some(crate::gateway::protocol::Terminal::Success)
    );
    protocol.commit_ownership();
    assert_eq!(registry.owner(&session).as_deref(), Some(old.id.as_str()));

    protocol.delivery().handed(64, true);
    assert!(protocol.delivered_success());
    protocol.commit_ownership();
    registry.flush();
    assert_eq!(registry.owner(&session).as_deref(), Some(new.id.as_str()));
}
