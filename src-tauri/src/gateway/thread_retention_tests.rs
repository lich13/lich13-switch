// 覆盖线程并发名额预留、释放及持久化恢复。
//! v0.21.2 capacity-retention and delivery-gate fixtures.
//!
//! This module is registered below the v0.20 fixture module so the loopback
//! gateway helpers remain available. Every persisted value is synthetic.
use crate::gateway::{
    admission::{self, Budget, Rejected},
    compaction::Registry,
    model::{Edit, Provider, Settings, Store},
    protocol::Protocol,
    replay::RequestHints,
    routing::Requirement,
    thread_capacity::Retention,
};
use serde_json::{json, Value};

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
    }
}
fn ordinary_hints() -> RequestHints {
    RequestHints::from_value(
        json!({"model":"fixture-model","input":[{"role":"user","content":"fixture"}]}),
        false,
    )
    .expect("fixture request metadata")
}

fn compacted_hints(boundary: &str) -> RequestHints {
    RequestHints {
        compacted_window: Some(boundary.to_owned()),
        ..Default::default()
    }
}

fn saved_retention(thread: &str, provider: &str, age_seconds: i64, busy: bool) -> Vec<u8> {
    let idle_since = admission::now_wall_ms().saturating_sub(age_seconds.saturating_mul(1000));
    serde_json::to_vec(&json!([{
        "thread": thread,
        "provider": provider,
        "idle_since": idle_since,
        "busy": busy,
    }]))
    .unwrap()
}

fn snapshot_entry(bytes: &[u8], provider: &str) -> Value {
    serde_json::from_slice::<Value>(bytes)
        .unwrap()
        .as_array()
        .and_then(|items| items.iter().find(|item| item["provider"] == provider))
        .cloned()
        .expect("retention snapshot entry")
}

#[test]
fn settings_defaults_and_legacy_phase_timeout_migration_are_idempotent() {
    let defaults = Settings::default();
    assert_eq!(defaults.first_byte_seconds, 300);
    assert_eq!(defaults.idle_seconds, 300);
    assert_eq!(defaults.thread_idle_release_seconds, 300);

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("gateway.json");
    let mut value = serde_json::to_value(Store::default()).unwrap();
    value["settings"]
        .as_object_mut()
        .unwrap()
        .remove("retryBudgetSeconds");
    value["settings"]["totalSeconds"] = 600.into();
    value["phaseTimeouts"] = false.into();
    value["settings"]["firstByteSeconds"] = 60.into();
    value["settings"]["idleSeconds"] = 120.into();
    value["settings"]["threadIdleReleaseSeconds"] = 17.into();
    std::fs::write(&path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();

    let (migrated, old_revision) = Store::load(&path).unwrap();
    assert!(migrated.phase_timeouts);
    assert_eq!(migrated.settings.first_byte_seconds, 300);
    assert_eq!(migrated.settings.idle_seconds, 300);
    assert_eq!(migrated.settings.thread_idle_release_seconds, 17);
    let new_revision = migrated.persist(&path, &old_revision).unwrap();

    let (loaded, revision) = Store::load(&path).unwrap();
    assert_eq!(loaded.settings, migrated.settings);
    assert_eq!(loaded.phase_timeouts, migrated.phase_timeouts);
    assert_eq!(revision, new_revision);
    let bytes = std::fs::read(&path).unwrap();
    assert_eq!(loaded.persist(&path, &revision).unwrap(), new_revision);
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
}

#[test]
fn retention_pending_entries_survive_prune_until_finish() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("thread-reservations.json");
    let thread = crate::storage::digest(b"fixture-retention-pending");
    let provider = "fixture-owner";
    std::fs::write(&path, saved_retention(&thread, provider, 600, false)).unwrap();

    let mut retention = Retention::load(&path);
    retention.seconds = 3600;
    let request = retention.begin(&thread, Some(provider));
    retention.seconds = 0;
    assert!(
        !retention.prune(),
        "pending entry must be protected from expiry"
    );
    assert_eq!(retention.reserved(provider), 1);

    retention.finish(request);
    assert_eq!(retention.reserved(provider), 0);
}

#[test]
fn retention_counts_parallel_requests_and_starts_idle_after_the_last_finish() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("thread-reservations.json");
    let thread = crate::storage::digest(b"fixture-retention-parallel");
    let provider = "fixture-owner";
    std::fs::write(&path, saved_retention(&thread, provider, 600, false)).unwrap();

    let mut retention = Retention::load(&path);
    retention.seconds = 3600;
    let first = retention.begin(&thread, Some(provider));
    assert!(retention.admit(first, provider, true));
    let second = retention.begin(&thread, Some(provider));
    assert!(retention.admit(second, provider, true));
    assert_eq!(
        retention.reserved(provider),
        0,
        "active requests are not idle reservations"
    );

    retention.release(first, provider, true);
    retention.finish(first);
    assert_eq!(
        retention.reserved(provider),
        0,
        "the second request is still active"
    );

    retention.release(second, provider, true);
    assert!(snapshot_entry(&retention.snapshot(), provider)["busy"]
        .as_bool()
        .unwrap());
    retention.finish(second);
    assert!(!snapshot_entry(&retention.snapshot(), provider)["busy"]
        .as_bool()
        .unwrap());

    let third = retention.begin(&thread, Some(provider));
    assert!(retention.admit(third, provider, true));
    retention.release(third, provider, true);
    retention.finish(third);
    retention.seconds = 1;
    assert!(!retention.prune(), "a new request resets the idle window");
    assert_eq!(retention.reserved(provider), 1);
}

#[test]
fn retention_hold_is_idempotent_and_keeps_old_provider_pending() {
    let mut retention = Retention::default();
    retention.seconds = 30;
    let thread = crate::storage::digest(b"fixture-retention-hold");
    let provider = "fixture-old";
    let initial = retention.begin(&thread, None);
    assert!(retention.admit(initial, provider, true));
    retention.release(initial, provider, true);
    retention.finish(initial);

    let waiting = retention.begin(&thread, Some(provider));
    retention.hold(waiting, provider);
    retention.hold(waiting, provider);
    retention.seconds = 0;
    assert!(
        !retention.prune(),
        "held old-provider entry must remain pending"
    );
    assert_eq!(retention.reserved(provider), 1);
    retention.finish(waiting);
    assert_eq!(retention.reserved(provider), 0);
}

#[test]
fn retention_successful_handoff_keeps_both_sides_under_short_retention() {
    let mut retention = Retention::default();
    retention.seconds = 30;
    let thread = crate::storage::digest(b"fixture-retention-handoff");
    let owner = "fixture-old";
    let receiver = "fixture-new";

    let initial = retention.begin(&thread, None);
    assert!(retention.admit(initial, owner, true));
    retention.release(initial, owner, true);
    retention.finish(initial);

    let handoff = retention.begin(&thread, Some(owner));
    assert!(retention.admit(handoff, receiver, true));
    retention.release(handoff, receiver, true);
    retention.finish(handoff);
    assert_eq!(retention.reserved(owner), 1);
    assert_eq!(retention.reserved(receiver), 1);
    assert!(!retention.prune());
}

#[test]
fn retention_restart_turns_busy_snapshot_into_idle_reservation_without_active_request() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("thread-reservations.json");
    let thread = crate::storage::digest(b"fixture-retention-restart");
    let provider = "fixture-owner";

    let mut running = Retention::default();
    let request = running.begin(&thread, None);
    assert!(running.admit(request, provider, true));
    let busy = running.snapshot();
    assert!(snapshot_entry(&busy, provider)["busy"].as_bool().unwrap());
    std::fs::write(&path, busy).unwrap();

    let mut restarted = Retention::load(&path);
    assert_eq!(restarted.reserved(provider), 1);
    assert!(!snapshot_entry(&restarted.snapshot(), provider)["busy"]
        .as_bool()
        .unwrap());
    restarted.seconds = 0;
    assert!(restarted.prune());
    assert_eq!(restarted.reserved(provider), 0);
}

#[tokio::test]
async fn scheduler_different_threads_cannot_share_one_reserved_receiver() {
    let (dir, gateway) = super::fixture(vec![
        "https://fixture-owner.example.invalid/v1".into(),
        "https://fixture-receiver.example.invalid/v1".into(),
    ])
    .await;
    let receiver_id = gateway.view().providers[1].id.clone();
    super::update(
        &gateway,
        &dir,
        Edit::ConcurrencyProvider {
            id: receiver_id.clone(),
            max_concurrency: 1,
        },
    );
    super::start(&gateway, &dir).await;
    gateway.0.admission.configure_retention(30);
    let route = gateway.route(&receiver_id).unwrap();
    let provider = route.provider.clone();

    let first_lease = gateway
        .0
        .compaction
        .prepare_policy(
            crate::storage::digest(b"fixture-thread-one"),
            Some(&ordinary_hints()),
            false,
            std::slice::from_ref(&provider),
        )
        .unwrap();
    let mut first_budget = Budget::new(2);
    first_budget.track(&gateway.0.admission, Some(&first_lease), 30);
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
    first.commit_rpm().unwrap();
    first.slot.complete();
    drop(first);
    drop(first_budget);
    drop(first_lease);
    assert_eq!(gateway.0.admission.reserved(&receiver_id), 1);

    let second_lease = gateway
        .0
        .compaction
        .prepare_policy(
            crate::storage::digest(b"fixture-thread-two"),
            Some(&ordinary_hints()),
            false,
            std::slice::from_ref(&provider),
        )
        .unwrap();
    let mut second_budget = Budget::new(0);
    second_budget.track(&gateway.0.admission, Some(&second_lease), 30);
    let result = gateway
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
        .await;
    assert!(matches!(result, Err(Rejected::Timeout)));
    drop(second_budget);
    drop(second_lease);

    gateway.0.admission.configure_retention(0);
    let third_lease = gateway
        .0
        .compaction
        .prepare_policy(
            crate::storage::digest(b"fixture-thread-three"),
            Some(&ordinary_hints()),
            false,
            std::slice::from_ref(&provider),
        )
        .unwrap();
    let mut third_budget = Budget::new(2);
    third_budget.track(&gateway.0.admission, Some(&third_lease), 0);
    let mut third = gateway
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
        .await
        .unwrap();
    third.commit_rpm().unwrap();
    third.slot.complete();
    drop(third);
    drop(third_budget);
    drop(third_lease);
    gateway.stop().await.unwrap();
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
    first.commit_rpm().unwrap();
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
    second.commit_rpm().unwrap();
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
