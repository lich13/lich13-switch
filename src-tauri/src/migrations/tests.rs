use super::*;
use serde_json::{json, Value};
use std::{fs, path::Path};

const RETIRED_STATISTICS: &[&str] = &[
    "usage.sqlite",
    "usage.sqlite-wal",
    "usage.sqlite-shm",
    "usage.sqlite-journal",
    "usage-settings.json",
    "model-pricing.json",
];

fn legacy_gateway(data: &Path) -> Vec<u8> {
    let value = json!({
        "schema": 1,
        "providers": [{
            "id": "fixture-provider",
            "name": "fixture provider",
            "proxyId": "fixture-proxy",
            "futureProviderField": {"keep": true}
        }],
        "proxies": [{"id": "fixture-proxy", "label": "fixture-only"}],
        "futureRootField": {"keep": true}
    });
    let bytes = serde_json::to_vec_pretty(&value).unwrap();
    fs::write(data.join("gateway.json"), &bytes).unwrap();
    bytes
}

fn assert_recovery_failure_preserves_legacy_state(recovery: &[u8]) {
    let data = tempfile::tempdir().unwrap();
    let gateway_before = legacy_gateway(data.path());
    let proxy_profiles = data.path().join("proxy-profiles.json");
    fs::write(&proxy_profiles, b"fixture-only legacy proxy metadata").unwrap();
    let profiles_before = fs::read(&proxy_profiles).unwrap();
    let recovery_path = data.path().join("gateway-recovery.json");
    fs::write(&recovery_path, recovery).unwrap();

    assert!(gateway(data.path()).is_err());
    assert_eq!(
        fs::read(data.path().join("gateway.json")).unwrap(),
        gateway_before
    );
    assert_eq!(fs::read(&proxy_profiles).unwrap(), profiles_before);
    assert_eq!(fs::read(&recovery_path).unwrap(), recovery);
}

#[test]
fn before_services_is_idempotent_and_removes_only_exact_legacy_statistics_files() {
    let data = tempfile::tempdir().unwrap();
    for name in RETIRED_STATISTICS {
        fs::write(data.path().join(name), b"fixture-only retired statistics").unwrap();
    }
    let retained_files = [
        ("usage.sqlite.backup", b"backup".as_slice()),
        ("usage-settings.json.old", b"old settings".as_slice()),
        ("gateway.json", b"gateway state".as_slice()),
    ];
    for (name, bytes) in retained_files {
        fs::write(data.path().join(name), bytes).unwrap();
    }
    let archive = data.path().join("usage-archive");
    fs::create_dir(&archive).unwrap();
    fs::write(archive.join("keep.json"), b"archive fixture").unwrap();

    before_services(data.path()).unwrap();
    before_services(data.path()).unwrap();

    for name in RETIRED_STATISTICS {
        assert!(!data.path().join(name).exists(), "{name}");
    }
    for (name, bytes) in retained_files {
        assert_eq!(fs::read(data.path().join(name)).unwrap(), bytes, "{name}");
    }
    assert_eq!(
        fs::read(archive.join("keep.json")).unwrap(),
        b"archive fixture"
    );
}

#[test]
fn gateway_recovers_before_migrating_and_preserves_unknown_fields_idempotently() {
    let data = tempfile::tempdir().unwrap();
    let original = legacy_gateway(data.path());
    let proxy_profiles = data.path().join("proxy-profiles.json");
    fs::write(&proxy_profiles, b"fixture-only legacy proxy metadata").unwrap();

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
    assert!(
        !proxy_profiles.exists(),
        "legacy proxy profiles should be retired after recovery"
    );

    let migrated = fs::read(data.path().join("gateway.json")).unwrap();
    let value: Value = serde_json::from_slice(&migrated).unwrap();
    assert_eq!(value["schema"], 2);
    assert!(value.get("proxies").is_none());
    assert_eq!(value["providers"][0]["id"], "fixture-provider");
    assert!(value["providers"][0].get("proxyId").is_none());
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
fn corrupt_or_incomplete_recovery_does_not_delete_legacy_proxy_state() {
    for recovery in [b"{".as_slice(), br#"{"version":2}"#.as_slice()] {
        assert_recovery_failure_preserves_legacy_state(recovery);
    }
}
