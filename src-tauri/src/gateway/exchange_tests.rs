//! Sub2API exchange and endpoint regression fixtures.
//!
//! All URLs and credentials below are synthetic.  The tests exercise the
//! Rust-facing exchange contract without reading any user-selected files.
use super::{forward, model, Edit, Gateway};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

fn account(
    name: &str,
    platform: &str,
    kind: &str,
    base_url: &str,
    key: &str,
    concurrency: u64,
) -> Value {
    json!({
        "name": name,
        "platform": platform,
        "type": kind,
        "credentials": {"base_url": base_url, "api_key": key},
        "concurrency": concurrency,
        "priority": 17,
        "rate_multiplier": 1.25,
        "auto_pause_on_expired": false,
        "proxy_key": ""
    })
}

fn write_fixture(dir: &Path, accounts: Vec<Value>) -> PathBuf {
    let path = dir.join("sub2api-fixture.json");
    let value = json!({
        "type": "sub2api-data",
        "version": 1,
        "exported_at": "2026-10-13T00:00:00Z",
        "proxies": [],
        "accounts": accounts
    });
    std::fs::write(&path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
    path
}

fn fixture_gateway() -> (tempfile::TempDir, Gateway) {
    let data = tempfile::tempdir().unwrap();
    let gateway = Gateway::new(data.path().to_path_buf()).unwrap();
    (data, gateway)
}

fn fixture_accounts() -> Vec<Value> {
    vec![
        account(
            "Fixture Alpha",
            "openai",
            "apikey",
            "https://alpha.example.invalid/api",
            "fixture-key-alpha",
            4,
        ),
        account(
            "Fixture Beta",
            "openai",
            "apikey",
            "https://beta.example.invalid/v1",
            "fixture-key-beta",
            8,
        ),
    ]
}

#[test]
fn sub2api_preview_commit_export_round_trip_supports_partial_and_all_selection() {
    let (data, gateway) = fixture_gateway();
    let source = write_fixture(data.path(), fixture_accounts());
    let preview = gateway.exchange_preview(&source).unwrap();
    assert_eq!(preview.rows.len(), 2);
    assert!(preview.rows.iter().all(|row| row.error.is_none()));
    assert_eq!(preview.unsupported_proxies, 0);

    let partial = gateway
        .exchange_commit(&preview.token, &[1], &preview.revision, data.path())
        .unwrap();
    assert_eq!(partial.providers.len(), 1);
    assert_eq!(partial.providers[0].name, "Fixture Beta");
    assert_eq!(
        partial.providers[0].base_url,
        "https://beta.example.invalid/v1"
    );

    let (data_all, gateway_all) = fixture_gateway();
    let source_all = write_fixture(data_all.path(), fixture_accounts());
    let preview_all = gateway_all.exchange_preview(&source_all).unwrap();
    let template = preview_all.template.as_ref().unwrap();
    assert!(!template.ready);
    gateway_all
        .exchange_select_template(template.token.as_deref().unwrap(), 1)
        .unwrap();
    let all = gateway_all
        .exchange_commit(
            &preview_all.token,
            &[0, 1],
            &preview_all.revision,
            data_all.path(),
        )
        .unwrap();
    assert_eq!(all.providers.len(), 2);

    let output = data_all.path().join("sub2api-export.json");
    gateway_all
        .exchange_export(
            &all.providers
                .iter()
                .map(|p| p.id.clone())
                .collect::<Vec<_>>(),
            &all.revision,
            &output,
        )
        .unwrap();
    let exported: Value = serde_json::from_slice(&std::fs::read(&output).unwrap()).unwrap();
    assert_eq!(exported["type"], "sub2api-data");
    assert_eq!(exported["version"], 1);
    assert_eq!(exported["accounts"].as_array().unwrap().len(), 2);
    assert!(exported["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .all(|item| item["type"] == "apikey" && item["platform"] == "openai"));
}

#[test]
fn sub2api_preview_rejects_invalid_type_duplicate_and_cross_client_accounts() {
    let (data, gateway) = fixture_gateway();
    let mut accounts = fixture_accounts();
    accounts.push(account(
        "Fixture OAuth",
        "openai",
        "oauth",
        "https://oauth.example.invalid/v1",
        "fixture-key-oauth",
        1,
    ));
    accounts.push(account(
        "Fixture Claude",
        "anthropic",
        "apikey",
        "https://claude.example.invalid/v1",
        "fixture-key-claude",
        1,
    ));
    let duplicate = accounts[0].clone();
    accounts.push(duplicate);
    let source = write_fixture(data.path(), accounts);
    let preview = gateway.exchange_preview(&source).unwrap();
    assert_eq!(preview.rows.len(), 5);
    assert!(preview.rows[0].error.is_none());
    assert!(preview.rows[1].error.is_none());
    assert!(preview.rows[2]
        .error
        .as_deref()
        .is_some_and(|message| message.contains("API Key")));
    assert!(preview.rows[3]
        .error
        .as_deref()
        .is_some_and(|message| message.contains("当前客户端")));
    assert!(preview.rows[4]
        .error
        .as_deref()
        .is_some_and(|message| message.contains("相同地址")));

    let empty = gateway.exchange_commit(&preview.token, &[], &preview.revision, data.path());
    let empty_error = match empty {
        Ok(_) => panic!("empty import selection must fail"),
        Err(error) => error,
    };
    assert_eq!(empty_error.code, "SUB2API_FILE");
    assert!(gateway.view().providers.is_empty());

    let duplicate_id =
        gateway.exchange_commit(&preview.token, &[0, 0], &preview.revision, data.path());
    let duplicate_error = match duplicate_id {
        Ok(_) => panic!("duplicate import selection must fail"),
        Err(error) => error,
    };
    assert_eq!(duplicate_error.message, "选择项重复");

    let invalid_id = gateway.exchange_commit(&preview.token, &[2], &preview.revision, data.path());
    let invalid_error = match invalid_id {
        Ok(_) => panic!("invalid import selection must fail"),
        Err(error) => error,
    };
    assert_eq!(invalid_error.message, "选择项无法导入");
}

#[test]
fn sub2api_exchange_rejects_revision_conflicts_and_export_selection_errors() {
    let (data, gateway) = fixture_gateway();
    let source = write_fixture(data.path(), fixture_accounts());
    let preview = gateway.exchange_preview(&source).unwrap();
    gateway
        .exchange_select_template(
            preview.template.as_ref().unwrap().token.as_deref().unwrap(),
            0,
        )
        .unwrap();
    gateway
        .edit(
            Edit::SaveProvider {
                search_only: None,
                quota_access_token: None,
                id: None,
                base_url: "https://existing.example.invalid/v1".into(),
                token: "fixture-existing-key".into(),
                name: Some("Existing fixture".into()),
            },
            &gateway.view().revision,
            data.path(),
        )
        .unwrap();
    let conflict = gateway.exchange_commit(&preview.token, &[0], &preview.revision, data.path());
    let conflict_error = match conflict {
        Ok(_) => panic!("stale exchange preview must fail"),
        Err(error) => error,
    };
    assert_eq!(conflict_error.code, "CONFLICT");

    let current = gateway.view();
    let output = data.path().join("invalid-export.json");
    let empty = gateway.exchange_export(&[], &current.revision, &output);
    assert_eq!(empty.unwrap_err().code, "SUB2API_FILE");
    let duplicate = gateway.exchange_export(
        &[
            current.providers[0].id.clone(),
            current.providers[0].id.clone(),
        ],
        &current.revision,
        &output,
    );
    assert_eq!(duplicate.unwrap_err().message, "选择项重复");
    let missing =
        gateway.exchange_export(&["fixture-missing-id".into()], &current.revision, &output);
    assert_eq!(missing.unwrap_err().message, "供应商已删除");
}

#[test]
fn sub2api_template_preserves_selected_prototype_and_masks_only_connection_fields() {
    let (data, gateway) = fixture_gateway();
    let source = write_fixture(data.path(), fixture_accounts());
    let prepared = gateway.exchange_template(&source).unwrap();
    assert!(!prepared.ready);
    gateway
        .exchange_select_template(prepared.token.as_deref().unwrap(), 0)
        .unwrap();
    let stored = data.path().join("sub2api-template.json");
    let template: Value = serde_json::from_slice(&std::fs::read(stored).unwrap()).unwrap();
    assert_eq!(template["version"], 2);
    let saved = &template["document"]["accounts"][0];
    let mut expected = fixture_accounts()[0].clone();
    expected["name"] = json!("");
    expected["credentials"]["base_url"] = json!("");
    expected["credentials"]["api_key"] = json!("");
    assert_eq!(saved, &expected);
    assert_eq!(
        template["document"]["accounts"].as_array().unwrap().len(),
        1
    );
    let text = serde_json::to_string(&template).unwrap();
    for secret in [
        "Fixture Alpha",
        "alpha.example.invalid",
        "fixture-key-alpha",
    ] {
        assert!(!text.contains(secret), "template leaked {secret}");
    }

    let invalid = write_fixture(
        data.path(),
        vec![account(
            "Fixture OAuth",
            "openai",
            "oauth",
            "https://oauth.example.invalid/v1",
            "fixture-key-oauth",
            1,
        )],
    );
    let result = gateway.exchange_template(&invalid);
    assert_eq!(result.unwrap_err().code, "EXCHANGE_TEMPLATE");
}

#[test]
fn base_url_normalization_covers_versions_prefixes_and_rejected_query() {
    let cases = [
        (
            "https://one.example.invalid",
            "https://one.example.invalid/v1",
        ),
        (
            "https://two.example.invalid/api",
            "https://two.example.invalid/api/v1",
        ),
        (
            "https://three.example.invalid/api/v2",
            "https://three.example.invalid/api/v2",
        ),
        (
            "https://four.example.invalid/api/v1/responses",
            "https://four.example.invalid/api/v1",
        ),
        (
            "https://five.example.invalid/api/v1///",
            "https://five.example.invalid/api/v1",
        ),
    ];
    for (raw, expected) in cases {
        assert_eq!(model::base_url(raw, 15722).unwrap().as_str(), expected);
    }
    for raw in [
        "https://bad.example.invalid/v1?token=fixture",
        "https://bad.example.invalid/v1#fragment",
        "ftp://bad.example.invalid/v1",
    ] {
        assert_eq!(model::base_url(raw, 15722).unwrap_err().code, "URL");
    }
}

#[test]
fn forwarding_target_matrix_keeps_prefix_version_query_for_both_clients() {
    let cases = [
        (
            "https://one.example.invalid/api",
            "/v1/responses?x=1&x=2",
            "https://one.example.invalid/api/v1/responses?x=1&x=2",
        ),
        (
            "https://two.example.invalid/api/v2",
            "/v1/chat/completions?model=fixture-model",
            "https://two.example.invalid/api/v2/chat/completions?model=fixture-model",
        ),
        (
            "https://three.example.invalid/root/v1/",
            "/v1/messages?stream=true",
            "https://three.example.invalid/root/v1/messages?stream=true",
        ),
        (
            "https://four.example.invalid/root/v1",
            "/models?limit=2",
            "https://four.example.invalid/root/v1/models?limit=2",
        ),
    ];
    for (base, path, expected) in cases {
        let uri = path.parse().unwrap();
        let target = forward::target(base, &uri).unwrap();
        assert_eq!(target.to_string(), expected);
    }
}
