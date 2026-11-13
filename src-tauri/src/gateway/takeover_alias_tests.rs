use super::{
    takeover::{self, Pair},
    ClientId, Edit, Gateway,
};
use crate::storage;
use serde_json::{json, Value};
use std::{fs, path::Path};

const LOCAL: &str = "gs_fixture_gateway_alias";
const UPSTREAM: &str = "fixture-upstream-key";
const BASE: &str = "https://provider.example.invalid/v1";

fn seed(client: ClientId, home: &Path) {
    fs::create_dir_all(home).unwrap();
    let text = match client {
            ClientId::Dsh => panic!("DSH profile path is covered separately"),
        ClientId::Codex => format!("# keep fixture\nmodel_provider = 'custom'\n[model_providers.custom]\nbase_url = '{BASE}'\nexperimental_bearer_token = '{UPSTREAM}'\nwire_api = 'responses'\n"),
        ClientId::Claude => serde_json::to_string_pretty(&json!({"fixture":"keep", "env":{"ANTHROPIC_BASE_URL":BASE,"ANTHROPIC_AUTH_TOKEN":UPSTREAM}})).unwrap(),
        ClientId::Grok => format!("# keep fixture\n[models]\ndefault = 'fixture-model'\n[model.fixture-model]\nmodel = 'fixture-model'\napi_backend = 'responses'\nbase_url = '{BASE}'\napi_key = '{UPSTREAM}'\n"),
    };
    fs::write(client.config(home), text).unwrap();
}

fn attach(client: ClientId, data: &Path, home: &Path, port: u16) {
    storage::private_dir(data).unwrap();
    let (revision, original) = takeover::read_for(client, home).unwrap();
    takeover::attach_store_for(client, data, home, port, LOCAL, original, &revision, None).unwrap();
}

#[test]
fn unknown_address_or_token_changes_keep_config_and_recovery_untouched() {
    for client in [ClientId::Codex, ClientId::Claude, ClientId::Grok] {
        for token_change in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let data = temp.path().join("data");
            let home = temp.path().join("client");
            seed(client, &home);
            attach(client, &data, &home, 41017);
            let text = fs::read_to_string(client.config(&home)).unwrap();
            let text = if token_change {
                text.replace(LOCAL, "gs_fixture_external")
            } else {
                text.replace(
                    &client.address(41017),
                    "https://external.example.invalid/v1",
                )
            };
            fs::write(client.config(&home), &text).unwrap();
            let recovery = fs::read(data.join("gateway-recovery.json")).unwrap();
            assert_eq!(takeover::recover(&data).err().unwrap().code, "CONFLICT");
            assert_eq!(fs::read_to_string(client.config(&home)).unwrap(), text);
            assert_eq!(
                fs::read(data.join("gateway-recovery.json")).unwrap(),
                recovery
            );
        }
    }
}

#[test]
fn grok_recovery_does_not_hide_changed_owned_model() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    let home = temp.path().join("client");
    seed(ClientId::Grok, &home);
    attach(ClientId::Grok, &data, &home, 41017);
    let text = fs::read_to_string(ClientId::Grok.config(&home)).unwrap();
    let mut doc = text.parse::<toml_edit::DocumentMut>().unwrap();
    doc["model"][super::grok_config::MANAGED]["model"] = toml_edit::value("fixture-other-model");
    let changed = doc.to_string();
    fs::write(ClientId::Grok.config(&home), &changed).unwrap();
    assert_eq!(takeover::recover(&data).err().unwrap().code, "CONFLICT");
    assert_eq!(
        fs::read_to_string(ClientId::Grok.config(&home)).unwrap(),
        changed
    );
}

fn gateway_for(client: ClientId, data: &Path) -> (Gateway, Gateway) {
    let root = Gateway::new(data.to_owned()).unwrap();
    let gateway = match client {
        ClientId::Dsh => panic!("DSH profile path is covered separately"),
        ClientId::Codex => root.clone(),
        ClientId::Claude => root.companion(data.join("claude")).unwrap(),
        ClientId::Grok => root.grok_companion(data.join("grok")).unwrap(),
    };
    (root, gateway)
}

#[test]
fn managed_addresses_require_a_known_client_path_port_and_exact_token() {
    for client in [ClientId::Codex, ClientId::Claude, ClientId::Grok] {
        for host in ["127.0.0.1", "localhost", "[::1]"] {
            let base = client.address(43721).replace("127.0.0.1", host);
            assert!(Pair::new(&base, LOCAL).is_gateway(client, LOCAL, &[43721]));
            assert!(!Pair::new(&base, "gs_fixture_other").is_gateway(client, LOCAL, &[43721]));
            assert!(!Pair::new(&format!("{base}?fixture=1"), LOCAL).is_gateway(
                client,
                LOCAL,
                &[43721]
            ));
        }
        for base in [
            "http://127.0.0.1.example.invalid:43721/v1",
            "http://fixture@localhost:43721/v1",
            "http://localhost:41019/v1",
            "https://localhost:43721/v1",
        ] {
            assert!(!Pair::new(base, LOCAL).is_gateway(client, LOCAL, &[43721]));
        }
    }
}

#[test]
fn grok_import_does_not_adopt_local_credentials_from_loopback_address() {
    for host in ["localhost", "[::1]", "127.0.0.1"] {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("client");
        seed(ClientId::Grok, &home);
        let path = ClientId::Grok.config(&home);
        let text = fs::read_to_string(&path)
            .unwrap()
            .replace(BASE, &format!("http://{host}:43721/v1"))
            .replace(UPSTREAM, LOCAL);
        fs::write(&path, &text).unwrap();
        let (_root, gateway) = gateway_for(ClientId::Grok, &temp.path().join("data"));
        let preview = gateway
            .preview_grok_import_with_env(&home, None, false, &|_| None)
            .unwrap();
        assert!(preview.token.is_none());
        assert_eq!(preview.error.as_deref(), Some("不能导入网关的本地凭据"));
        assert_eq!(fs::read_to_string(path).unwrap(), text);
        assert!(gateway.view().providers.is_empty());
    }
}

fn save_local(gateway: &Gateway, token: &str) {
    let mut state = gateway.0.inner.lock();
    state.editable_mut().store.local_token = token.to_owned();
    state.revision = state
        .store
        .persist(&gateway.0.data.join("gateway.json"), &state.revision)
        .unwrap();
}

fn provider(gateway: &Gateway, home: &Path) {
    gateway
        .edit(
            Edit::SaveProvider {
                search_only: None,
                quota_access_token: None,
                id: None,
                base_url: BASE.into(),
                token: UPSTREAM.into(),
                name: None,
            },
            &gateway.view().revision,
            home,
        )
        .unwrap();
}

#[tokio::test]
async fn collision_rotates_only_the_starting_client_in_the_config_transaction() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    let codex = Gateway::new(data.clone()).unwrap();
    let claude = codex.companion(data.join("claude")).unwrap();
    let codex_home = temp.path().join("codex");
    let claude_home = temp.path().join("claude");
    seed(ClientId::Codex, &codex_home);
    seed(ClientId::Claude, &claude_home);
    provider(&codex, &codex_home);
    provider(&claude, &claude_home);
    let held = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    codex
        .set_listener_port(
            held.local_addr().unwrap().port(),
            &codex.listener_view().revision,
        )
        .unwrap();
    drop(held);
    codex
        .start(&codex.view().revision, &codex_home)
        .await
        .unwrap();
    let token = codex.0.inner.lock().store.local_token.clone();
    let codex_config = fs::read(ClientId::Codex.config(&codex_home)).unwrap();
    save_local(&claude, &token);
    let store_before = fs::read(data.join("claude/gateway.json")).unwrap();
    let config_before = fs::read(ClientId::Claude.config(&claude_home)).unwrap();
    let failed = claude
        .start_checked(
            &claude.view().revision,
            &claude_home,
            Some("fixture-stale-revision"),
        )
        .await;
    assert_eq!(failed.err().unwrap().code, "CONFLICT");
    assert_eq!(
        fs::read(data.join("claude/gateway.json")).unwrap(),
        store_before
    );
    assert_eq!(
        fs::read(ClientId::Claude.config(&claude_home)).unwrap(),
        config_before
    );
    claude
        .start(&claude.view().revision, &claude_home)
        .await
        .unwrap();
    let new_token = claude.0.inner.lock().store.local_token.clone();
    assert_ne!(new_token, token);
    assert_eq!(codex.0.inner.lock().store.local_token, token);
    assert_eq!(
        fs::read(ClientId::Codex.config(&codex_home)).unwrap(),
        codex_config
    );
    let stored: Value =
        serde_json::from_slice(&fs::read(data.join("claude/gateway.json")).unwrap()).unwrap();
    assert_eq!(stored["localToken"], new_token);
    assert!(takeover::read_for(ClientId::Claude, &claude_home)
        .unwrap()
        .1
        .is_gateway(ClientId::Claude, &new_token, &[codex.listener_view().port]));
    claude.stop().await.unwrap();
    codex.stop().await.unwrap();
}
