//! v0.20 Core regressions for the ordinary Codex API login gateway.
//!
//! These tests start the real gateway against an isolated Codex home, then
//! exercise Core while the gateway owns the temporary API credential.  No
//! user configuration, database, or credential outside the fixture is read.

use super::*;
use crate::gateway::{Edit, Gateway, Settings};
use serde_json::json;
use std::{fs, net::TcpListener, path::PathBuf};

struct Fixture {
    home: PathBuf,
    data: PathBuf,
    gateway: Gateway,
    temp: tempfile::TempDir,
}

fn api_config() -> &'static str {
    "# fixture config\nmodel_provider = 'relay'\n\n[model_providers.relay]\nbase_url = 'https://api.example.invalid/v1'\nrequires_openai_auth = true\nsupports_websockets = false\n\n[future]\nkeep = 'fixture'\n"
}

fn api_auth(key: &str) -> Vec<u8> {
    serde_json::to_vec_pretty(&json!({
        "auth_mode": "apikey",
        "OPENAI_API_KEY": key,
        "future": {"keep": ["fixture", 7, true]}
    }))
    .unwrap()
}

fn jwt(subject: &str) -> String {
    format!(
        "e30.{}.fixture",
        URL_SAFE_NO_PAD.encode(
            json!({"sub": subject, "email": format!("{subject}@example.invalid")}).to_string()
        )
    )
}

fn chatgpt(subject: &str) -> String {
    json!({
        "auth_mode": "chatgpt",
        "tokens": {
            "account_id": "fixture-workspace",
            "access_token": jwt(subject),
            "id_token": jwt(subject),
            "refresh_token": format!("fixture-refresh-{subject}")
        },
        "future": {"keep": "fixture"}
    })
    .to_string()
}

fn free_port() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.local_addr().unwrap().port()
}

async fn gateway_fixture() -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("codex-home");
    fs::create_dir(&home).unwrap();
    fs::write(home.join("config.toml"), api_config()).unwrap();
    fs::write(home.join("auth.json"), api_auth("fixture-source-key")).unwrap();

    let data = temp.path().join("gateway-data");
    let gateway = Gateway::new(data.clone()).unwrap();
    gateway.import_initial(&home).unwrap();
    gateway
        .edit(
            Edit::Settings {
                settings: Settings {
                    port: free_port(),
                    ..Default::default()
                },
            },
            &gateway.view().revision,
            &home,
        )
        .unwrap();
    gateway
        .start(&gateway.view().revision, &home)
        .await
        .unwrap();

    Fixture {
        temp,
        home,
        data,
        gateway,
    }
}

#[tokio::test]
async fn v020_core_gateway_state_and_restart_do_not_import_temporary_api_key() {
    let fixture = gateway_fixture().await;
    let mut core = Core::new(fixture.data.clone(), fixture.home.clone()).unwrap();

    let first = core.state().unwrap();
    assert_eq!(first.current_state, "gateway");
    assert!(first.accounts.is_empty());
    assert_eq!(first.auth_sync, None);

    let saved_id = core
        .import_raw(&chatgpt("fixture-library"), Some("Fixture ChatGPT".into()))
        .unwrap();
    let active = core.state().unwrap();
    assert_eq!(active.current_state, "gateway");
    assert_eq!(active.accounts.len(), 1);
    assert_eq!(active.accounts[0].id, saved_id);
    assert_eq!(active.accounts[0].kind, "chatgpt");
    assert!(!active.accounts[0].current);

    drop(core);
    let mut reopened = Core::new(fixture.data.clone(), fixture.home.clone()).unwrap();
    let restarted = reopened.state().unwrap();
    assert_eq!(restarted.current_state, "gateway");
    assert_eq!(restarted.accounts.len(), 1);
    assert!(restarted
        .accounts
        .iter()
        .all(|account| account.kind != "apiKey"));
    assert_eq!(restarted.accounts[0].id, saved_id);

    fixture.gateway.stop().await.unwrap();
    drop(fixture.gateway);
    drop(fixture.temp);
}

#[tokio::test]
async fn v020_core_gateway_transaction_rejects_switch_and_import_file_without_changing_bytes() {
    let fixture = gateway_fixture().await;
    let mut core = Core::new(fixture.data.clone(), fixture.home.clone()).unwrap();
    let account = core
        .import_raw(
            &chatgpt("fixture-switch-target"),
            Some("Switch target".into()),
        )
        .unwrap();
    let expected = core.state().unwrap().auth_revision;
    let before_auth = fs::read(fixture.home.join("auth.json")).unwrap();
    let before_config = fs::read(fixture.home.join("config.toml")).unwrap();

    let error = core.switch_account(&account, &expected).unwrap_err();
    assert_eq!(error.code, "GATEWAY_ACTIVE");
    assert_eq!(
        fs::read(fixture.home.join("auth.json")).unwrap(),
        before_auth
    );
    assert_eq!(
        fs::read(fixture.home.join("config.toml")).unwrap(),
        before_config
    );

    let current_error = core
        .import_file(
            &fixture.home.join("auth.json"),
            Some("Current API key".into()),
        )
        .unwrap_err();
    assert_eq!(current_error.code, "MANAGED");

    let equivalent = fixture.home.join(".").join("auth.json");
    let equivalent_error = core
        .import_file(&equivalent, Some("Equivalent API key".into()))
        .unwrap_err();
    assert_eq!(equivalent_error.code, "MANAGED");
    assert_eq!(
        fs::read(fixture.home.join("auth.json")).unwrap(),
        before_auth
    );
    assert_eq!(
        fs::read(fixture.home.join("config.toml")).unwrap(),
        before_config
    );
    assert_eq!(core.state().unwrap().current_state, "gateway");

    fixture.gateway.stop().await.unwrap();
    drop(fixture.gateway);
    drop(fixture.temp);
}

#[tokio::test]
async fn v020_core_official_chatgpt_login_saves_new_account_without_overwriting_gateway_auth() {
    let fixture = gateway_fixture().await;
    let mut core = Core::new(fixture.data.clone(), fixture.home.clone()).unwrap();
    let before_auth = fs::read(fixture.home.join("auth.json")).unwrap();
    let before_config = fs::read(fixture.home.join("config.toml")).unwrap();
    let login = chatgpt("fixture-official-login");

    assert!(!core.complete_login(&login).unwrap());
    let state = core.state().unwrap();
    assert_eq!(state.current_state, "gateway");
    assert_eq!(state.accounts.len(), 1);
    assert_eq!(state.accounts[0].kind, "chatgpt");
    assert!(!state.accounts[0].current);
    assert_eq!(
        fs::read(fixture.home.join("auth.json")).unwrap(),
        before_auth
    );
    assert_eq!(
        fs::read(fixture.home.join("config.toml")).unwrap(),
        before_config
    );

    drop(core);
    let mut reopened = Core::new(fixture.data.clone(), fixture.home.clone()).unwrap();
    let restarted = reopened.state().unwrap();
    assert_eq!(restarted.current_state, "gateway");
    assert_eq!(restarted.accounts.len(), 1);
    assert_eq!(restarted.accounts[0].kind, "chatgpt");
    assert_eq!(
        fs::read(fixture.home.join("auth.json")).unwrap(),
        before_auth
    );

    fixture.gateway.stop().await.unwrap();
    drop(fixture.gateway);
    drop(fixture.temp);
}
