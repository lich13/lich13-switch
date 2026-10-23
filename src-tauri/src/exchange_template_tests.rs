//! Synthetic v0.23 template regressions. Never reads a user template.
use crate::exchange_template::{payload, payload_mut, Service};
use serde_json::{json, Value};
use std::fs;

fn api(name: &str, platform: &str) -> Value {
    json!({
        "name": name,
        "platform": platform,
        "type": "apikey",
        "credentials": {
            "base_url": "https://template.example.invalid/v1",
            "api_key": "fixture-template-key",
            "model_mapping": {"fixture-model": "fixture-upstream"},
            "unknown_credential": [null, false, 0, {"retain": "fixture"}]
        },
        "extra": {"future": {"keep": [1, 2, 3]}, "explicit_false": false},
        "concurrency": 7,
        "priority": 13,
        "rate_multiplier": 1.125,
        "auto_pause_on_expired": false,
        "expires_at": 1900000000_i64,
        "proxy_key": "fixture-proxy",
        "future_account": {"a": null, "b": []}
    })
}

fn envelope(accounts: Vec<Value>) -> Value {
    json!({
        "data": {
            "type": "sub2api-data",
            "version": 1,
            "exported_at": "2026-10-13T00:00:00Z",
            "accounts": accounts,
            "proxies": [{"proxy_key": "fixture-proxy", "name": "Fixture proxy", "protocol": "http", "host": "proxy.example.invalid", "port": 1080, "unknown_proxy": {"keep": true}}],
            "future_payload": [null, {"keep": "fixture"}]
        },
        "skip_default_group": true,
        "future_wrapper": {"keep": false, "empty": ""}
    })
}

fn masked(mut document: Value, index: usize) -> Value {
    let mut account = payload(&document)["accounts"][index].clone();
    account["name"] = json!("");
    account["credentials"]["base_url"] = json!("");
    account["credentials"]["api_key"] = json!("");
    payload_mut(&mut document)["accounts"] = json!([account]);
    document
}

#[test]
fn v023_template_masks_exactly_three_fields_and_preserves_complete_envelopes() {
    for wrapped in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("saved.json");
        let document = envelope(vec![api("Fixture template", "openai")]);
        let document = if wrapped {
            document
        } else {
            document["data"].clone()
        };
        let prepared = Service::default()
            .prepare(document.clone(), &path, "openai", "apikey")
            .unwrap();
        assert!(prepared.ready);
        assert!(prepared.token.is_none());
        assert!(prepared.rows.is_empty());
        assert_eq!(
            Service::load(&path, "openai", "apikey").unwrap(),
            masked(document, 0)
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }
}

#[test]
fn v023_template_requires_explicit_matching_prototype_and_consumes_selection() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("saved.json");
    let service = Service::default();
    let document = envelope(vec![
        api("Wrong platform", "anthropic"),
        api("First", "openai"),
        api("Second", "openai"),
    ]);
    let prepared = service
        .prepare(document.clone(), &path, "openai", "apikey")
        .unwrap();
    assert!(!prepared.ready);
    assert!(!path.exists());
    assert_eq!(
        prepared
            .rows
            .iter()
            .map(|r| (r.id, r.name.as_str()))
            .collect::<Vec<_>>(),
        vec![(1, "First"), (2, "Second")]
    );
    let token = prepared.token.unwrap();
    assert!(service
        .select(&token, 0, &path, "openai", "apikey")
        .is_err());
    assert!(service
        .select(&token, 2, &path, "anthropic", "apikey")
        .is_err());
    assert!(!path.exists());
    service
        .select(&token, 2, &path, "openai", "apikey")
        .unwrap();
    assert_eq!(
        Service::load(&path, "openai", "apikey").unwrap(),
        masked(document, 2)
    );
    assert!(service
        .select(&token, 1, &path, "openai", "apikey")
        .is_err());
}

#[test]
fn v023_template_cancel_and_replacement_preserve_last_saved_document() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("saved.json");
    let service = Service::default();
    service
        .prepare(
            envelope(vec![api("Saved", "openai")]),
            &path,
            "openai",
            "apikey",
        )
        .unwrap();
    let before = fs::read(&path).unwrap();
    let document = envelope(vec![api("First", "openai"), api("Second", "openai")]);
    let first = service
        .prepare(document.clone(), &path, "openai", "apikey")
        .unwrap()
        .token
        .unwrap();
    let second = service
        .prepare(document.clone(), &path, "openai", "apikey")
        .unwrap()
        .token
        .unwrap();
    assert!(service
        .select(&first, 0, &path, "openai", "apikey")
        .is_err());
    service.cancel(&first);
    service
        .select(&second, 1, &path, "openai", "apikey")
        .unwrap();
    assert_eq!(
        Service::load(&path, "openai", "apikey").unwrap(),
        masked(document.clone(), 1)
    );
    fs::write(&path, &before).unwrap();
    let third = service
        .prepare(document, &path, "openai", "apikey")
        .unwrap()
        .token
        .unwrap();
    service.cancel(&third);
    assert!(service
        .select(&third, 0, &path, "openai", "apikey")
        .is_err());
    assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn v023_template_legacy_and_wrong_kind_require_reselection_without_overwrite() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("saved.json");
    let legacy = br#"{"version":1,"platform":"openai","concurrency":4,"extra":{"keep":true}}"#;
    fs::write(&path, legacy).unwrap();
    let error = Service::load(&path, "openai", "apikey").unwrap_err();
    assert!(error.message.contains("旧模板"));
    assert_eq!(fs::read(&path).unwrap(), legacy);
    Service::default()
        .prepare(
            envelope(vec![api("Current", "openai")]),
            &path,
            "openai",
            "apikey",
        )
        .unwrap();
    let current = fs::read(&path).unwrap();
    assert!(Service::load(&path, "openai", "oauth")
        .unwrap_err()
        .message
        .contains("重新选择"));
    assert!(Service::load(&path, "anthropic", "apikey").is_err());
    assert_eq!(fs::read(&path).unwrap(), current);
}

#[test]
fn v023_template_oauth_keeps_behavior_but_drops_prototype_identity_and_cache() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("saved.json");
    let document = envelope(vec![json!({
        "name": "Prototype account",
        "platform": "openai",
        "type": "oauth",
        "credentials": {
            "id_token": "fixture-id", "access_token": "fixture-access", "refresh_token": "fixture-refresh",
            "chatgpt_account_id": "fixture-workspace", "email": "template@example.invalid",
            "model_mapping": {"fixture-model": "fixture-upstream"}, "intercept_warmup_requests": false,
            "unknown_owner": "prototype-only"
        },
        "concurrency": 8, "priority": 5, "rate_multiplier": 1.2, "auto_pause_on_expired": false,
        "proxy_key": "fixture-proxy",
        "extra": {"privacy_mode": "private", "codex_fingerprint_mode": "shared", "openai_compact_mode": "on", "credit_balance": 999, "cached_email": "template@example.invalid", "unknown_owner": true},
        "unknown_owner": {"subject": "prototype-only"}
    })]);
    Service::default()
        .prepare(document.clone(), &path, "openai", "oauth")
        .unwrap();
    let saved = Service::load(&path, "openai", "oauth").unwrap();
    let mut expected = json!({
        "type": "sub2api-data", "version": 1, "exported_at": "2026-10-13T00:00:00Z",
        "proxies": payload(&document)["proxies"]
    });
    payload_mut(&mut expected)["accounts"] = json!([{
        "name": "", "platform": "openai", "type": "oauth",
        "credentials": {"model_mapping": {"fixture-model": "fixture-upstream"}, "intercept_warmup_requests": false},
        "concurrency": 8, "priority": 5, "rate_multiplier": 1.2, "auto_pause_on_expired": false,
        "proxy_key": "fixture-proxy",
        "extra": {"privacy_mode": "private", "codex_fingerprint_mode": "shared", "openai_compact_mode": "on"}
    }]);
    assert_eq!(saved, expected);
}

#[test]
fn v023_template_preserves_unknown_large_integer_and_decimal_exactly() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("template.json");
    let mut document = envelope(vec![api("Fixture", "openai")]);
    let exact: Value = serde_json::from_str(
        r#"{"large":184467440737095516161234567890,"decimal":0.12345678901234567890123456789}"#,
    )
    .unwrap();
    payload_mut(&mut document)["accounts"][0]["unknown_numeric"] = exact;
    Service::default()
        .prepare(document.clone(), &path, "openai", "apikey")
        .unwrap();
    let saved = Service::load(&path, "openai", "apikey").unwrap();
    assert_eq!(saved, masked(document, 0));
    assert_eq!(
        payload(&saved)["accounts"][0]["unknown_numeric"]["large"].to_string(),
        "184467440737095516161234567890"
    );
    assert_eq!(
        payload(&saved)["accounts"][0]["unknown_numeric"]["decimal"].to_string(),
        "0.12345678901234567890123456789"
    );
}

#[test]
fn v023_template_gateway_export_changes_only_name_url_key_for_both_clients() {
    use crate::gateway::{Edit, Gateway};
    let temp = tempfile::tempdir().unwrap();
    let codex = Gateway::new(temp.path().join("codex-gateway")).unwrap();
    let claude = codex.companion(temp.path().join("claude-gateway")).unwrap();
    for (gateway, platform) in [(&codex, "openai"), (&claude, "anthropic")] {
        let document = envelope(vec![api("Prototype", platform)]);
        let source = temp.path().join(format!("{platform}-template.json"));
        fs::write(&source, serde_json::to_vec(&document).unwrap()).unwrap();
        assert!(gateway.exchange_template(&source).unwrap().ready);
        let view = gateway
            .edit(
                Edit::SaveProvider {
                    quota_access_token: None,
                    id: None,
                    name: Some(format!("Fixture {platform}")),
                    base_url: "https://provider.example.invalid/v1".into(),
                    token: "fixture-provider-key".into(),
                },
                &gateway.view().revision,
                &temp.path().join(format!("{platform}-home")),
            )
            .unwrap();
        let provider = &view.providers[0];
        let output = temp.path().join(format!("rust-api-{platform}.json"));
        gateway
            .exchange_export(std::slice::from_ref(&provider.id), &view.revision, &output)
            .unwrap();
        let bytes = fs::read(&output).unwrap();
        let exported: Value = serde_json::from_slice(&bytes).unwrap();
        let mut expected = document;
        let account = &mut payload_mut(&mut expected)["accounts"][0];
        account["name"] = json!(format!("Fixture {platform}"));
        account["credentials"]["base_url"] = json!("https://provider.example.invalid/v1");
        account["credentials"]["api_key"] = json!("fixture-provider-key");
        assert_eq!(exported, expected);
        if let Some(directory) = std::env::var_os("LICH13_INTEROP_FIXTURES") {
            let directory = std::path::PathBuf::from(directory);
            assert!(directory.is_absolute());
            fs::create_dir_all(&directory).unwrap();
            fs::write(directory.join(format!("rust-api-{platform}.json")), bytes).unwrap();
        }
    }
}
