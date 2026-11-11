use super::*;
use serde_json::{json, Value};
use std::{fs, path::Path};

fn saved_gateway(data: &Path) -> Vec<u8> {
    let mut value = serde_json::to_value(crate::gateway::Store::default()).unwrap();
    value.as_object_mut().unwrap().extend(
        json!({
            "schema": 2,
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
fn gateway_recovers_before_loading_and_preserves_unknown_fields_idempotently() {
    let data = tempfile::tempdir().unwrap();
    let original = saved_gateway(data.path());

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

    let loaded = fs::read(data.path().join("gateway.json")).unwrap();
    let value: Value = serde_json::from_slice(&loaded).unwrap();
    assert_eq!(value["schema"], 2);
    assert_eq!(value["providers"][0]["id"], "fixture-provider");
    assert_eq!(value["providers"][0]["futureProviderField"]["keep"], true);
    assert_eq!(value["futureRootField"]["keep"], true);
    assert_eq!(original, loaded);
}
