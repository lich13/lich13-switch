use super::*;
use std::fs;

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
        let mut state = gateway.0.inner.lock();
        state.editable_mut().store.dsh_catalog =
            vec![serde_json::json!({"id":models::FLASH,"input":["text","image"]})];
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
    gateway.0.inner.lock().editable_mut().store.dsh = Some(saved);
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
    gateway.0.inner.lock().editable_mut().store.dsh = Some(read_saved());
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
    let mut old = serde_json::json!({"id":models::FLASH});
    models::normalize(&mut old);
    assert_eq!(old["input"], serde_json::json!(["text", "image"]));
    assert_eq!(old["reasoningEfforts"]["off"], "none");
    assert_eq!(old["reasoningEfforts"]["max"], "max");
    let once = old.clone();
    models::normalize(&mut old);
    assert_eq!(old, once);
    assert_eq!(models::catalog()[1]["input"], serde_json::json!(["text"]));
    let mut explicit = serde_json::json!({"id":"fixture-model", "input":["text"]});
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
