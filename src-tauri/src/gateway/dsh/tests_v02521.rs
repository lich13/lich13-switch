use super::*;
use std::fs;

#[tokio::test]
async fn legacy_routes_and_full_snapshots_upgrade_without_false_conflicts() {
    let (home, _parent, gateway) = tests_v02516::fixture();
    gateway
        .attach_dsh_profiles(
            home.path(),
            Options {
                model: Some(models::FLASH.into()),
                ..tests_v02516::options()
            },
            true,
            &gateway.view().revision,
            None,
        )
        .await
        .unwrap();
    let data = home.path().join("data/dsh");
    let mut store = gateway.0.inner.lock().unwrap().store.clone();
    let snapshot = store.dsh.as_mut().unwrap();
    for (name, profile) in &mut snapshot.profiles {
        let (file, patch) = patch_file(&profile.patch.after).unwrap();
        let node = owned_route(&patch).unwrap().unwrap();
        let route = node.as_mapping().unwrap();
        for key in [
            "compat",
            "streamIdleTimeoutMs",
            "websocketConnectTimeoutMs",
            "retryPolicy",
        ] {
            route.remove(key);
        }
        for node in route.get_sequence("models").unwrap().values() {
            let model = node.as_mapping().unwrap();
            let input = model.get("input").unwrap();
            model.remove("input");
            model.set("inputModalities", input);
        }
        profile.patch.after = file.to_string();
        fs::write(
            home.path().join("profiles").join(name).join(PATCH),
            &profile.patch.after,
        )
        .unwrap();
    }
    snapshot.patch = snapshot.profiles[&snapshot.options.profile].patch.clone();
    let before = snapshot.credentials.before.clone();
    let after = snapshot.credentials.after.clone();
    let mut legacy = serde_json::to_value(&store).unwrap();
    legacy["dsh"]["credentials"] = serde_json::json!({"before":before, "after":after});
    legacy["dshCatalog"] =
        serde_json::json!([{"id":models::FLASH,"inputModalities":["text","image"]}]);
    legacy["settings"]
        .as_object_mut()
        .unwrap()
        .remove("retryBudgetSeconds");
    legacy["settings"]["totalSeconds"] = 600.into();
    legacy["futureSetting"] = "fixture-preserved".into();
    fs::write(
        data.join("gateway.json"),
        serde_json::to_vec(&legacy).unwrap(),
    )
    .unwrap();
    crate::migrations::gateway_state_after_recovery(&data).unwrap();
    let once = fs::read(data.join("gateway.json")).unwrap();
    crate::migrations::gateway_state_after_recovery(&data).unwrap();
    assert_eq!(once, fs::read(data.join("gateway.json")).unwrap());
    let migrated: serde_json::Value = serde_json::from_slice(&once).unwrap();
    assert_eq!(migrated["futureSetting"], "fixture-preserved");
    assert!(!migrated["dsh"]
        .to_string()
        .contains("fixture-provider-secret"));
    let (store, revision) = crate::gateway::model::Store::load(&data.join("gateway.json")).unwrap();
    for snapshot in store.dsh.as_ref().unwrap().active_profiles() {
        check_owned(
            &snapshot,
            &Input::read(home.path(), &snapshot.options.profile).unwrap(),
        )
        .unwrap();
    }
    {
        let mut state = gateway.0.inner.lock().unwrap();
        state.editable_mut().store = store;
        state.revision = revision;
    }
    gateway
        .restart_dsh(home.path(), &gateway.view().revision)
        .await
        .unwrap();
    for profile in ["desktop", "web", "headless"] {
        let input = Input::read(home.path(), profile).unwrap();
        let route = owned_route(&sequence(input.patch.before.as_deref().unwrap()).unwrap())
            .unwrap()
            .unwrap();
        assert!(route.get("compat").is_some());
        assert_eq!(
            route.get("streamIdleTimeoutMs").unwrap().to_i64(),
            Some(300000)
        );
        assert!(!route.to_string().contains("inputModalities"));
    }
    gateway.stop_dsh(false).await.unwrap();
    assert!(fs::read_to_string(home.path().join(".credentials.yaml"))
        .unwrap()
        .contains("fixture-provider-secret"));
}

#[tokio::test]
async fn saved_routes_keep_capabilities_budget_and_only_owned_snapshot_fields() {
    let (home, _parent, gateway) = tests_v02516::fixture();
    fs::write(home.path().join(".env"), "UNRELATED=fixture-env-secret\n").unwrap();
    let credential = home.path().join(".credentials.yaml");
    let original = fs::read_to_string(&credential).unwrap();
    fs::write(
        &credential,
        original.replace(
            "records: {}",
            "records: {unrelated: fixture-account-secret}",
        ),
    )
    .unwrap();
    {
        let mut state = gateway.0.inner.lock().unwrap();
        state.editable_mut().store.dsh_catalog =
            vec![serde_json::json!({"id":models::FLASH,"inputModalities":["text","image"]})];
    }
    gateway
        .attach_dsh_profiles(
            home.path(),
            Options {
                model: Some(models::FLASH.into()),
                ..tests_v02516::options()
            },
            true,
            &gateway.view().revision,
            None,
        )
        .await
        .unwrap();
    let data = home.path().join("data/dsh");
    let read_saved = || -> Snapshot {
        let raw: serde_json::Value =
            serde_json::from_slice(&fs::read(data.join("gateway.json")).unwrap()).unwrap();
        let snapshot = raw["dsh"].to_string();
        for secret in [
            "fixture-env-secret",
            "fixture-account-secret",
            "fixture-provider-secret",
        ] {
            assert!(!snapshot.contains(secret));
        }
        serde_json::from_value(raw["dsh"].clone()).unwrap()
    };
    let saved = read_saved();
    assert_eq!(saved.active_profiles().len(), 3);
    for snapshot in saved.active_profiles() {
        let input = Input::read(home.path(), &snapshot.options.profile).unwrap();
        check_owned(&snapshot, &input).unwrap();
        let route = owned_route(&sequence(input.patch.before.as_deref().unwrap()).unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(
            route.get("streamIdleTimeoutMs").unwrap().to_i64(),
            Some(300000)
        );
        assert_eq!(
            route
                .get("retryPolicy")
                .unwrap()
                .get("maxRetries")
                .unwrap()
                .to_i64(),
            Some(0)
        );
        assert_eq!(
            string(route.get("compat").unwrap().get("maxTokensField")).as_deref(),
            Some("max_tokens")
        );
        let expected = yaml(r#"{"compat":{"maxTokensField":"max_tokens","requiresReasoningContentOnAssistantMessages":true,"thinkingFormat":"deepseek","supportsStore":false,"supportsDeveloperRole":false}}"#).unwrap();
        assert!(equal(
            route.get("compat"),
            expected.as_mapping().unwrap().get("compat")
        ));
        let model = route
            .get("models")
            .unwrap()
            .as_sequence()
            .unwrap()
            .values()
            .next()
            .unwrap();
        assert!(model
            .get("input")
            .unwrap()
            .as_sequence()
            .unwrap()
            .values()
            .any(|v| string(Some(v)).as_deref() == Some("image")));
        assert!(model.get("inputModalities").is_none());
    }
    // Reload projected snapshots, then synchronize settings through the real edit transaction.
    gateway.0.inner.lock().unwrap().editable_mut().store.dsh = Some(saved);
    let mut settings = gateway.view().settings;
    settings.idle_seconds = 211;
    gateway
        .edit(
            crate::gateway::Edit::Settings { settings },
            &gateway.view().revision,
            home.path(),
        )
        .unwrap();
    for profile in ["desktop", "web", "headless"] {
        let input = Input::read(home.path(), profile).unwrap();
        let route = owned_route(&sequence(input.patch.before.as_deref().unwrap()).unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(
            route.get("streamIdleTimeoutMs").unwrap().to_i64(),
            Some(211000)
        );
    }
    gateway
        .restart_dsh(home.path(), &gateway.view().revision)
        .await
        .unwrap();
    gateway.0.inner.lock().unwrap().editable_mut().store.dsh = Some(read_saved());
    fs::write(
        &credential,
        fs::read_to_string(&credential)
            .unwrap()
            .replace("fixture-account-secret", "fixture-new-secret"),
    )
    .unwrap();
    gateway.stop_dsh(false).await.unwrap();
    let restored = fs::read_to_string(&credential).unwrap();
    assert!(restored.contains("fixture-new-secret"));
    assert!(!restored.contains(KEY));
    assert!(fs::read_to_string(home.path().join(".env"))
        .unwrap()
        .contains("fixture-env-secret"));
}

#[test]
fn catalog_normalization_is_idempotent_and_prefers_explicit_input() {
    let mut old = serde_json::json!({"id":models::FLASH, "inputModalities":["text","image"]});
    models::normalize(&mut old);
    assert_eq!(old["input"], serde_json::json!(["text", "image"]));
    assert_eq!(old["reasoningEfforts"]["off"], "none");
    assert_eq!(old["reasoningEfforts"]["max"], "max");
    assert!(old.get("inputModalities").is_none());
    let once = old.clone();
    models::normalize(&mut old);
    assert_eq!(old, once);
    assert_eq!(models::catalog()[1]["input"], serde_json::json!(["text"]));
    let mut explicit =
        serde_json::json!({"id":"fixture-model", "input":["text"], "inputModalities":["image"]});
    models::normalize(&mut explicit);
    assert_eq!(explicit["input"], serde_json::json!(["text"]));
    let mut custom_effort =
        serde_json::json!({"id":models::PRO, "reasoningEfforts":{"high":"high"}});
    models::normalize(&mut custom_effort);
    assert_eq!(
        custom_effort["reasoningEfforts"],
        serde_json::json!({"high":"high"})
    );
}
