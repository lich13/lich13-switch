use super::*;
use crate::gateway::{model::Provider, replay::RequestHints, routing::Requirement};
use serde_json::json;

fn provider(id: &str, model: Option<&str>, special: bool) -> Provider {
    Provider {
        id: id.into(),
        name: id.into(),
        base_url: "https://example.invalid/v1".into(),
        token: "fixture-token".into(),
        queued: true,
        version: "fixture-version".into(),
        max_concurrency: 0,
        max_rpm: 0,
        allowed_models: model.map(|m| vec![m.into()]),
        handoff_after_compaction: special,
        take_new_threads: special,
        search_only: false,
    }
}
fn hints(model: &str, compacted: bool) -> RequestHints {
    RequestHints::from_value(
        json!({"model":model, "input": if compacted {
            json!([{"type":"compaction","encrypted_content":"fixture-window"}])
        } else { json!([{"role":"user","content":"fixture"}]) }}),
        false,
    )
    .unwrap()
}
fn snapshot(registry: &Registry, key: &str) -> (Value, u64) {
    let state = registry.state.lock().unwrap();
    (
        serde_json::to_value(&state.entries[key]).unwrap(),
        state.sequence,
    )
}
fn establish(registry: &Arc<Registry>, key: &str, providers: &[Provider]) {
    let first = registry
        .prepare_request(
            key.into(),
            Some(&hints("fixture-main", false)),
            true,
            providers,
            &Requirement::model(Some("fixture-main")),
            None,
        )
        .unwrap();
    assert!(first.admitted("fixture-a"));
    first.complete("fixture-a", None);
}

#[test]
fn detour_never_edits_history_epoch_window_or_activity() {
    let dir = tempfile::tempdir().unwrap();
    let registry = Registry::new(dir.path().join("owners.json"));
    let key = storage::digest(b"fixture-thread");
    let providers = [
        provider("fixture-a", Some("fixture-main"), true),
        provider("fixture-b", None, false),
    ];
    establish(&registry, &key, &providers);
    let boundary = storage::digest(b"fixture-window");
    registry
        .prepare_request(
            key.clone(),
            Some(&hints("fixture-main", false)),
            true,
            &providers,
            &Requirement::model(Some("fixture-main")),
            None,
        )
        .unwrap()
        .complete("fixture-a", Some(&boundary));
    let before = snapshot(&registry, &key);
    let request = hints("fixture-side", true);
    for completed in [false, true] {
        let lease = registry
            .prepare_request(
                key.clone(),
                Some(&request),
                true,
                &providers,
                &Requirement::model(Some("fixture-side")),
                None,
            )
            .unwrap();
        assert!(lease.request_only());
        assert!(!lease.handoff);
        assert!(!lease.owns("fixture-a"));
        assert!(lease.admitted("fixture-b"));
        lease.output_started("fixture-b");
        if completed {
            lease.complete("fixture-b", Some(&storage::digest(b"fixture-other-window")));
            lease.complete("fixture-b", None);
        }
        drop(lease);
        assert_eq!(snapshot(&registry, &key), before);
        assert!(registry.boundary_matches(&key, Some(&boundary)));
    }
    assert!(registry.state.lock().unwrap().notifications.is_empty());
    registry.flush();
    let restarted = Registry::new(dir.path().join("owners.json"));
    assert_eq!(restarted.owner(&key).as_deref(), Some("fixture-a"));
    assert!(restarted.boundary_matches(&key, Some(&boundary)));
}

#[test]
fn detour_completion_cannot_undo_a_concurrent_legal_handoff() {
    let dir = tempfile::tempdir().unwrap();
    let registry = Registry::new(dir.path().join("owners.json"));
    let key = storage::digest(b"fixture-thread");
    let providers = [
        provider("fixture-a", Some("fixture-main"), true),
        provider("fixture-b", None, false),
    ];
    establish(&registry, &key, &providers);
    let readonly = registry
        .prepare_request(
            key.clone(),
            Some(&hints("fixture-side", false)),
            true,
            &providers,
            &Requirement::model(Some("fixture-side")),
            None,
        )
        .unwrap();
    let boundary = storage::digest(b"fixture-window");
    let main = registry
        .prepare_request(
            key.clone(),
            Some(&hints("fixture-main", false)),
            true,
            &providers,
            &Requirement::model(Some("fixture-main")),
            None,
        )
        .unwrap();
    main.complete("fixture-a", Some(&boundary));
    let mut reordered = vec![provider("fixture-c", None, true)];
    reordered.extend_from_slice(&providers);
    let handoff = registry
        .prepare_request(
            key.clone(),
            Some(&hints("fixture-main", true)),
            true,
            &reordered,
            &Requirement::model(Some("fixture-main")),
            None,
        )
        .unwrap();
    assert!(handoff.handoff);
    assert!(handoff.admitted("fixture-c"));
    handoff.output_started("fixture-c");
    handoff.complete("fixture-c", None);
    let after = snapshot(&registry, &key);
    readonly.output_started("fixture-b");
    readonly.complete("fixture-b", Some(&boundary));
    assert_eq!(snapshot(&registry, &key), after);
    assert_eq!(registry.owner(&key).as_deref(), Some("fixture-c"));
}

#[test]
fn only_explicit_model_mismatch_of_a_live_queued_owner_is_a_detour() {
    for (requirement, queued, present, expected) in [
        (Requirement::model(Some("fixture-side")), true, true, true),
        (Requirement::model(Some("fixture-main")), true, true, false),
        (Requirement::Unknown, true, true, false),
        (Requirement::Resource, true, true, false),
        (Requirement::model(Some("fixture-side")), false, true, false),
        (Requirement::model(Some("fixture-side")), true, false, false),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let registry = Registry::new(dir.path().join("owners.json"));
        let key = storage::digest(b"fixture-thread");
        let mut providers = vec![
            provider("fixture-a", Some("fixture-main"), true),
            provider("fixture-b", None, false),
        ];
        establish(&registry, &key, &providers);
        providers[0].queued = queued;
        if !present {
            providers.remove(0);
        }
        let lease = registry
            .prepare_request(
                key.clone(),
                Some(&hints("fixture-side", false)),
                false,
                &providers,
                &requirement,
                None,
            )
            .unwrap();
        assert_eq!(
            lease.request_only(),
            expected,
            "{requirement:?}, queued={queued}, present={present}"
        );
        if expected {
            assert_eq!(
                lease.decision(None, "fixture-b"),
                crate::usage::model::RoutingDecision::ModelDetourOwnerPreserved
            );
        }
    }
}

#[test]
fn admission_rechecks_changed_models_queue_and_receiver_flags() {
    let dir = tempfile::tempdir().unwrap();
    let gateway = crate::gateway::Gateway::new(dir.path().to_path_buf()).unwrap();
    let registry = &gateway.0.compaction;
    let key = storage::digest(b"fixture-thread");
    let providers = [
        provider("fixture-a", Some("fixture-main"), true),
        provider("fixture-b", None, false),
    ];
    gateway.0.inner.lock().unwrap().store.providers = providers.to_vec();
    establish(registry, &key, &providers);
    let main = Requirement::model(Some("fixture-main"));
    let pending = registry
        .prepare_request(
            key.clone(),
            Some(&hints("fixture-main", false)),
            true,
            &providers,
            &main,
            None,
        )
        .unwrap();
    gateway.0.inner.lock().unwrap().store.providers[0].allowed_models =
        Some(vec!["fixture-other".into()]);
    assert_eq!(
        pending
            .admit_request(&gateway, &providers[1], &main, false)
            .unwrap_err()
            .code,
        crate::errors::code::NO_ELIGIBLE_PROVIDER
    );
    drop(pending);

    let side = Requirement::model(Some("fixture-side"));
    let readonly = registry
        .prepare_request(
            key.clone(),
            Some(&hints("fixture-side", false)),
            true,
            &providers,
            &side,
            None,
        )
        .unwrap();
    assert!(readonly.request_only());
    let before = snapshot(registry, &key);
    // Expanding the owner's whitelist cannot upgrade the already-prepared
    // detour, and a newly special-only receiver no longer accepts it.
    {
        let mut state = gateway.0.inner.lock().unwrap();
        state.store.providers[0].allowed_models = None;
        state.store.providers[1].take_new_threads = true;
    }
    assert!(readonly
        .admit_request(&gateway, &providers[1], &side, false)
        .is_err());
    gateway.0.inner.lock().unwrap().store.providers[1].take_new_threads = false;
    assert!(readonly
        .admit_request(&gateway, &providers[1], &side, false)
        .unwrap());
    readonly.complete("fixture-b", None);
    assert_eq!(snapshot(registry, &key), before);
    gateway.0.inner.lock().unwrap().store.providers[1].queued = false;
    assert!(readonly
        .admit_request(&gateway, &providers[1], &side, false)
        .is_err());
    assert_eq!(snapshot(registry, &key), before);
}

#[test]
fn owner_removal_allows_new_fallback_but_never_upgrades_an_existing_detour() {
    let dir = tempfile::tempdir().unwrap();
    let registry = Registry::new(dir.path().join("owners.json"));
    let key = storage::digest(b"fixture-thread");
    let providers = [
        provider("fixture-a", Some("fixture-main"), true),
        provider("fixture-b", None, false),
    ];
    establish(&registry, &key, &providers);
    let side = Requirement::model(Some("fixture-side"));
    let readonly = registry
        .prepare_request(
            key.clone(),
            Some(&hints("fixture-side", false)),
            true,
            &providers,
            &side,
            None,
        )
        .unwrap();
    let new_request = registry
        .prepare_request(
            key.clone(),
            Some(&hints("fixture-main", false)),
            true,
            &providers[1..],
            &Requirement::model(Some("fixture-main")),
            None,
        )
        .unwrap();
    assert!(!new_request.request_only());
    new_request.complete("fixture-b", None);
    let after = snapshot(&registry, &key);
    readonly.complete("fixture-a", Some(&storage::digest(b"fixture-window")));
    assert_eq!(snapshot(&registry, &key), after);
    assert_eq!(registry.owner(&key).as_deref(), Some("fixture-b"));
    registry.state.lock().unwrap().entries.remove(&key);
    assert!(readonly.admitted("fixture-b"));
    readonly.output_started("fixture-b");
    readonly.complete("fixture-b", None);
    assert!(registry.owner(&key).is_none());
}

#[test]
fn cursor_on_a_detour_provider_cannot_acquire_conversation_write_authority() {
    let dir = tempfile::tempdir().unwrap();
    let registry = Registry::new(dir.path().join("owners.json"));
    let key = storage::digest(b"fixture-thread");
    let providers = [
        provider("fixture-a", Some("fixture-main"), true),
        provider("fixture-b", None, false),
    ];
    establish(&registry, &key, &providers);
    let before = snapshot(&registry, &key);
    let continuation = RequestHints::from_value(
        json!({"model":"fixture-main", "previous_response_id":"fixture-side-response", "input":[{"role":"user","content":"fixture continuation"}]}),
        false,
    )
    .unwrap();
    let lease = registry
        .prepare_request(
            key.clone(),
            Some(&continuation),
            false,
            &providers,
            &Requirement::model(Some("fixture-main")),
            Some("fixture-b"),
        )
        .unwrap();
    assert!(lease.request_only());
    assert_eq!(
        decision(
            Some(&key),
            Some(&continuation),
            true,
            Some(&lease),
            "fixture-b"
        ),
        crate::usage::model::RoutingDecision::CursorPinned
    );
    assert!(lease.admitted("fixture-b"));
    lease.output_started("fixture-b");
    lease.complete("fixture-b", Some(&storage::digest(b"fixture-side-window")));
    drop(lease);
    assert_eq!(snapshot(&registry, &key), before);
}
