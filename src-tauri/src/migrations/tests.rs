use super::*;
use serde_json::{json, Value};
use std::{fs, path::Path};

fn legacy_gateway(data: &Path) -> Vec<u8> {
    let mut value = serde_json::to_value(crate::gateway::Store::default()).unwrap();
    value.as_object_mut().unwrap().extend(
        json!({
            "schema": 1,
            "providers": [{
                "id": "fixture-provider",
                "name": "fixture provider",
                "baseUrl": "https://example.invalid/v1",
                "token": "fixture-token",
                "version": "fixture-version",
                "queued": false,
                "futureProviderField": {"keep": true}
            }],
            "futureRootField": {"keep": true}
        })
        .as_object()
        .unwrap()
        .clone(),
    );
    let bytes = serde_json::to_vec_pretty(&value).unwrap();
    fs::write(data.join("gateway.json"), &bytes).unwrap();
    bytes
}

#[test]
fn gateway_recovers_before_migrating_and_preserves_unknown_fields_idempotently() {
    let data = tempfile::tempdir().unwrap();
    let original = legacy_gateway(data.path());

    let config = data.path().join("config.toml");
    let base_url = "https://example.invalid/v1";
    let token = "fixture-token";
    fs::write(
        &config,
        format!(
            "model_provider = 'custom'\n[model_providers.custom]\nbase_url = '{base_url}'\nexperimental_bearer_token = '{token}'\n"
        ),
    )
    .unwrap();
    let pair = json!({"base_url": base_url, "token": token});
    let journal = json!({
        "version": 2,
        "path": config,
        "before": pair,
        "applied": pair,
        "exit": pair,
        "live": false
    });
    let recovery = data.path().join("gateway-recovery.json");
    fs::write(&recovery, serde_json::to_vec(&journal).unwrap()).unwrap();

    gateway(data.path()).unwrap();
    assert!(
        !recovery.exists(),
        "successful recovery should settle its journal"
    );

    let migrated = fs::read(data.path().join("gateway.json")).unwrap();
    let value: Value = serde_json::from_slice(&migrated).unwrap();
    assert_eq!(value["schema"], 2);
    assert_eq!(value["providers"][0]["id"], "fixture-provider");
    assert_eq!(value["providers"][0]["futureProviderField"]["keep"], true);
    assert_eq!(value["futureRootField"]["keep"], true);

    gateway(data.path()).unwrap();
    assert_eq!(
        fs::read(data.path().join("gateway.json")).unwrap(),
        migrated
    );
    assert_ne!(original, migrated);
}

#[test]
fn request_budget_migration_replaces_legacy_values_once() {
    let mut value = json!({"settings": {"totalSeconds":600, "firstByteSeconds":600, "idleSeconds":600, "connectSeconds":15, "queueSeconds":30, "websocketRetrySeconds":60, "cooldownSeconds":17}});
    gateway_wait_budget(&mut value);
    for key in [
        "firstByteSeconds",
        "idleSeconds",
        "connectSeconds",
        "queueSeconds",
        "websocketRetrySeconds",
        "retryBudgetSeconds",
    ] {
        assert_eq!(value["settings"][key], 300);
    }
    assert!(value["settings"].get("totalSeconds").is_none());
    assert_eq!(value["settings"]["cooldownSeconds"], 17);
    value["settings"]["firstByteSeconds"] = 91.into();
    let saved = value.clone();
    gateway_wait_budget(&mut value);
    assert_eq!(value, saved);
}
