//! v0.20 Codex API login and ownership regressions.
//!
//! Every fixture is isolated under a temporary directory.  The config and
//! auth files are compared byte-for-byte outside the fields owned by the API
//! gateway, so these tests also cover preservation of future fields.
use super::{
    codex_api::{self, Connection},
    model::{Provider, Store},
    takeover::{self, Pair},
    ClientId, Edit, Gateway,
};
use serde_json::{json, Value};
use std::{
    fs,
    net::TcpListener,
    path::{Path, PathBuf},
};

fn config_for(base: &str, requires: bool, top: &str, provider: &str) -> String {
    format!(
        "# keep this comment exactly\nprofile = \"work\"\n{top}[profiles.work]\nmodel_provider = \"named\"\n\n[model_providers.named]\nbase_url = \"{base}\"\nrequires_openai_auth = {requires}\nsupports_websockets = false\n{provider}\n\n[future]\npreserve = \"future-field\"\nnumber = 42\n",
    )
}

fn auth_for(key: &str, future: &str) -> Vec<u8> {
    serde_json::to_vec_pretty(&json!({
        "auth_mode": "apikey",
        "OPENAI_API_KEY": key,
        "unknown": {"keep": true, "nested": future},
        "future_array": ["fixture", 7, false]
    }))
    .unwrap()
}

fn home_with(config: &str, auth: &[u8]) -> (tempfile::TempDir, PathBuf) {
    let t = tempfile::tempdir().unwrap();
    let home = t.path().join("codex-home");
    fs::create_dir(&home).unwrap();
    fs::write(home.join("config.toml"), config).unwrap();
    fs::write(home.join("auth.json"), auth).unwrap();
    (t, home)
}

fn provider(id: &str, base: &str, token: &str) -> Provider {
    Provider {
        id: id.into(),
        name: id.into(),
        base_url: base.into(),
        token: token.into(),
        queued: true,
        version: format!("{id}-version"),
        max_concurrency: 0,
        max_rpm: 0,
        allowed_models: None,
        supports_websocket: false,
        handoff_after_compaction: true,
        take_new_threads: false,
    }
}

fn free_port() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.local_addr().unwrap().port()
}

fn edit(gateway: &Gateway, home: &Path, edit: Edit) -> super::View {
    let revision = gateway.view().revision;
    gateway.edit(edit, &revision, home).unwrap()
}

fn auth_value(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

fn mask_base(text: &str) -> String {
    let marker = "base_url = ";
    let marker_start = text.find(marker).expect("fixture base_url");
    let value_start = marker_start + marker.len();
    let quote = text[value_start..]
        .chars()
        .next()
        .expect("fixture base_url quote");
    assert_eq!(quote, '"');
    let content_start = value_start + quote.len_utf8();
    let content_end = content_start
        + text[content_start..]
            .find(quote)
            .expect("fixture base_url closing quote");
    let mut output = text.to_owned();
    output.replace_range(content_start..content_end, "<fixture-base>");
    output
}

#[test]
fn v020_api_import_initial_detects_named_profile_without_mutating_sources_and_restart_persists() {
    let config = config_for("https://api.first.example.invalid/v1", true, "", "");
    let auth = auth_for("fixture-api-key", "first");
    let (t, home) = home_with(&config, &auth);
    let data = t.path().join("gateway-data");
    let gateway = Gateway::new(data.clone()).unwrap();
    gateway.import_initial(&home).unwrap();

    assert_eq!(gateway.view().connection_mode, "apiKey");
    assert_eq!(gateway.view().providers.len(), 1);
    assert_eq!(
        gateway.view().providers[0].base_url,
        "https://api.first.example.invalid/v1"
    );
    assert_eq!(
        fs::read(home.join("config.toml")).unwrap(),
        config.as_bytes()
    );
    assert_eq!(fs::read(home.join("auth.json")).unwrap(), auth);

    let stored: Value =
        serde_json::from_slice(&fs::read(data.join("gateway.json")).unwrap()).unwrap();
    assert_eq!(stored["connection"]["mode"], "apiKey");
    assert_eq!(stored["connection"]["provider"], "named");
    assert!(stored["initialized"].as_bool().unwrap());
    let selected = gateway.view().selected.clone();
    drop(gateway);

    let restarted = Gateway::new(data).unwrap();
    restarted.import_initial(&home).unwrap();
    assert_eq!(restarted.view().connection_mode, "apiKey");
    assert_eq!(restarted.view().selected, selected);
    assert_eq!(
        restarted.view().providers[0].base_url,
        "https://api.first.example.invalid/v1"
    );
    assert_eq!(
        fs::read(home.join("config.toml")).unwrap(),
        config.as_bytes()
    );
    assert_eq!(fs::read(home.join("auth.json")).unwrap(), auth);
}

#[test]
fn v020_existing_store_keeps_bearer_connection_and_bytes() {
    let config = config_for("https://api.first.example.invalid/v1", true, "", "");
    let auth = auth_for("fixture-api-key", "existing");
    let (t, home) = home_with(&config, &auth);
    let data = t.path().join("gateway-data");
    fs::create_dir(&data).unwrap();

    let store = Store {
        initialized: true,
        connection: Connection::Bearer,
        providers: vec![provider(
            "legacy-provider",
            "https://legacy.example.invalid/v1",
            "legacy-fixture-token",
        )],
        selected: Some("legacy-provider".into()),
        ..Store::default()
    };
    let bytes = serde_json::to_vec_pretty(&store).unwrap();
    fs::write(data.join("gateway.json"), &bytes).unwrap();

    let gateway = Gateway::new(data.clone()).unwrap();
    gateway.import_initial(&home).unwrap();
    assert_eq!(gateway.view().connection_mode, "bearer");
    assert_eq!(gateway.view().providers.len(), 1);
    assert_eq!(fs::read(data.join("gateway.json")).unwrap(), bytes);
    assert_eq!(
        fs::read(home.join("config.toml")).unwrap(),
        config.as_bytes()
    );
    assert_eq!(fs::read(home.join("auth.json")).unwrap(), auth);
}

#[test]
fn v020_api_explicit_binding_rejects_independent_auth_routes_but_preserves_env_key() {
    let cases = [
        ("keyring", "cli_auth_credentials_store = 'keyring'\n", ""),
        ("auto", "cli_auth_credentials_store = 'auto'\n", ""),
        (
            "ephemeral",
            "cli_auth_credentials_store = 'ephemeral'\n",
            "",
        ),
        ("forced-chatgpt", "forced_login_method = 'chatgpt'\n", ""),
        ("provider-auth", "", "auth = 'fixture-provider-auth'\n"),
        ("inline-bearer", "", "auth = { type = 'bearer' }\n"),
        (
            "bearer",
            "",
            "experimental_bearer_token = 'fixture-bearer'\n",
        ),
        ("auth-command", "", "auth_command = 'fixture-command'\n"),
        ("exec", "", "exec = 'fixture-exec'\n"),
        (
            "authorization-header",
            "",
            "http_headers = { Authorization = 'Bearer fixture' }\n",
        ),
        (
            "env-api-key-header",
            "",
            "env_http_headers = { \"x-api-key\" = 'fixture-header' }\n",
        ),
    ];
    for (name, top, provider_fields) in cases {
        let config = config_for(
            "https://api.first.example.invalid/v1",
            true,
            top,
            provider_fields,
        );
        let auth = auth_for("fixture-api-key", name);
        let (t, home) = home_with(&config, &auth);
        let before_config = fs::read(home.join("config.toml")).unwrap();
        let before_auth = fs::read(home.join("auth.json")).unwrap();
        let error = codex_api::binding(&home).unwrap_err();
        assert_eq!(error.code, "CODEX_AUTH", "case {name}");
        assert_eq!(fs::read(home.join("config.toml")).unwrap(), before_config);
        assert_eq!(fs::read(home.join("auth.json")).unwrap(), before_auth);
        drop(t);
    }

    for (name, profile_field) in [
        (
            "active-profile-keyring",
            "cli_auth_credentials_store = 'keyring'\n",
        ),
        (
            "active-profile-forced-chatgpt",
            "forced_login_method = 'chatgpt'\n",
        ),
    ] {
        let config = config_for("https://api.first.example.invalid/v1", true, "", "").replace(
            "[profiles.work]\nmodel_provider",
            &format!("[profiles.work]\n{profile_field}model_provider"),
        );
        let auth = auth_for("fixture-api-key", name);
        let (t, home) = home_with(&config, &auth);
        assert!(codex_api::binding(&home).is_err(), "case {name}");
        assert_eq!(
            fs::read(home.join("config.toml")).unwrap(),
            config.as_bytes()
        );
        assert_eq!(fs::read(home.join("auth.json")).unwrap(), auth);
        drop(t);
    }

    let config = config_for(
        "https://api.first.example.invalid/v1",
        true,
        "",
        "env_key = 'OPENAI_API_KEY'\n",
    );
    let auth = auth_for("fixture-api-key", "env-key");
    let (t, home) = home_with(&config, &auth);
    assert_eq!(
        codex_api::binding(&home).unwrap(),
        Connection::ApiKey {
            provider: "named".into()
        }
    );
    assert_eq!(
        fs::read(home.join("config.toml")).unwrap(),
        config.as_bytes()
    );
    assert_eq!(fs::read(home.join("auth.json")).unwrap(), auth);
    drop(t);

    let config = config_for("https://api.first.example.invalid/v1", false, "", "");
    let auth = auth_for("fixture-api-key", "requires-false");
    let (t, home) = home_with(&config, &auth);
    assert!(codex_api::binding(&home).is_err());
    drop(t);
}

#[test]
fn v020_api_connection_revision_includes_config_and_auth_bytes() {
    let config = config_for("https://api.first.example.invalid/v1", true, "", "");
    let auth = auth_for("fixture-api-key", "revision-a");
    let (t, home) = home_with(&config, &auth);
    let connection = Connection::ApiKey {
        provider: "named".into(),
    };
    let (first_revision, first_pair) =
        takeover::read_connection(ClientId::Codex, &home, &connection).unwrap();

    fs::write(
        home.join("auth.json"),
        auth_for("fixture-api-key", "revision-b"),
    )
    .unwrap();
    let (auth_revision, auth_pair) =
        takeover::read_connection(ClientId::Codex, &home, &connection).unwrap();
    assert_ne!(first_revision, auth_revision);
    assert!(first_pair == auth_pair);

    let changed_config = config.replace(
        "preserve = \"future-field\"",
        "preserve = \"changed-field\"",
    );
    fs::write(home.join("config.toml"), changed_config).unwrap();
    let (config_revision, config_pair) =
        takeover::read_connection(ClientId::Codex, &home, &connection).unwrap();
    assert_ne!(auth_revision, config_revision);
    assert!(auth_pair == config_pair);
    drop(t);
}

#[tokio::test]
async fn v020_gateway_api_mode_owns_only_base_and_auth_through_select_start_switch_and_recovery() {
    let first_base = "https://api.first.example.invalid/v1";
    let second_base = "https://api.second.example.invalid/v1";
    let config = config_for(first_base, true, "", "");
    let auth = auth_for("first-key", "transition");
    let (t, home) = home_with(&config, &auth);
    let data = t.path().join("gateway-data");
    let gateway = Gateway::new(data.clone()).unwrap();
    gateway.import_initial(&home).unwrap();
    let first_id = gateway.view().providers[0].id.clone();

    let second = edit(
        &gateway,
        &home,
        Edit::SaveProvider {
            id: None,
            base_url: second_base.into(),
            token: "second-key".into(),
            name: Some("second".into()),
        },
    );
    let second_id = second.providers[1].id.clone();
    let source_mask = mask_base(&config);
    let before_select_auth = auth_value(&home.join("auth.json"));

    let selected = edit(
        &gateway,
        &home,
        Edit::Select {
            id: second_id.clone(),
        },
    );
    assert_eq!(selected.selected.as_deref(), Some(second_id.as_str()));
    assert_eq!(
        mask_base(&fs::read_to_string(home.join("config.toml")).unwrap()),
        source_mask
    );
    let selected_auth = auth_value(&home.join("auth.json"));
    assert_eq!(selected_auth["OPENAI_API_KEY"], "second-key");
    assert_eq!(selected_auth["unknown"], before_select_auth["unknown"]);
    assert_eq!(
        selected_auth["future_array"],
        before_select_auth["future_array"]
    );

    let mut settings = gateway.view().settings;
    settings.port = free_port();
    edit(&gateway, &home, Edit::Settings { settings });
    let started = gateway
        .start(&gateway.view().revision, &home)
        .await
        .unwrap();
    assert!(started.running);
    let running_config = fs::read_to_string(home.join("config.toml")).unwrap();
    let running_auth = auth_value(&home.join("auth.json"));
    assert_eq!(mask_base(&running_config), source_mask);
    assert_eq!(running_auth["unknown"], before_select_auth["unknown"]);
    assert_eq!(
        running_auth["future_array"],
        before_select_auth["future_array"]
    );
    let local_token: String =
        serde_json::from_slice::<Value>(&fs::read(data.join("gateway.json")).unwrap()).unwrap()
            ["localToken"]
            .as_str()
            .unwrap()
            .into();
    assert_eq!(running_auth["OPENAI_API_KEY"], local_token);
    let journal: Value =
        serde_json::from_slice(&fs::read(data.join("gateway-recovery.json")).unwrap()).unwrap();
    assert_eq!(journal["connection"]["mode"], "apiKey");
    assert_eq!(journal["connection"]["provider"], "named");

    let switched = edit(
        &gateway,
        &home,
        Edit::Select {
            id: first_id.clone(),
        },
    );
    assert_eq!(switched.selected.as_deref(), Some(first_id.as_str()));
    assert_eq!(
        fs::read_to_string(home.join("config.toml")).unwrap(),
        running_config
    );
    assert_eq!(auth_value(&home.join("auth.json")), running_auth);

    // Reopening the data directory recovers the persisted API journal before
    // the new handle returns. The old running handle is stopped afterwards so
    // its listener is released.
    let restarted = Gateway::new(data.clone()).unwrap();
    let recovered_config = fs::read_to_string(home.join("config.toml")).unwrap();
    let recovered_auth = auth_value(&home.join("auth.json"));
    assert_eq!(mask_base(&recovered_config), source_mask);
    assert_eq!(recovered_auth["OPENAI_API_KEY"], "first-key");
    assert_eq!(recovered_auth["unknown"], before_select_auth["unknown"]);
    assert!(!data.join("gateway-recovery.json").exists());
    assert_eq!(restarted.view().connection_mode, "apiKey");
    drop(restarted);
    gateway.stop().await.unwrap();

    let final_auth = auth_value(&home.join("auth.json"));
    assert_eq!(final_auth["OPENAI_API_KEY"], "first-key");
    assert_eq!(final_auth["unknown"], before_select_auth["unknown"]);
    assert_eq!(
        mask_base(&fs::read_to_string(home.join("config.toml")).unwrap()),
        source_mask
    );
}

#[test]
fn v020_api_external_auth_edit_conflict_does_not_overwrite_external_bytes() {
    let config = config_for("https://api.first.example.invalid/v1", true, "", "");
    let auth = auth_for("fixture-api-key", "before-conflict");
    let (t, home) = home_with(&config, &auth);
    let data = t.path().join("gateway-data");
    fs::create_dir(&data).unwrap();
    let connection = Connection::ApiKey {
        provider: "named".into(),
    };
    let (expected, current) =
        takeover::read_connection(ClientId::Codex, &home, &connection).unwrap();
    let external = auth_for("external-key", "external-edit");
    fs::write(home.join("auth.json"), &external).unwrap();
    let config_before = fs::read(home.join("config.toml")).unwrap();
    let error = codex_api::write(
        &data,
        &home,
        "named",
        &Pair::new("https://api.next.example.invalid/v1", "next-key"),
        Some(&expected),
        Some(&[&current]),
    )
    .unwrap_err();
    assert_eq!(error.code, "CONFLICT");
    assert_eq!(fs::read(home.join("auth.json")).unwrap(), external);
    assert_eq!(fs::read(home.join("config.toml")).unwrap(), config_before);
    assert!(!data.join("gateway-api-write.json").exists());
    drop(t);
}
