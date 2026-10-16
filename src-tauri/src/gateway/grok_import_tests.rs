// 覆盖 Grok 配置发现、环境文件及导入事务。
use super::{
    grok_config::{self, Connection, Protocol},
    grok_import,
    takeover::Pair,
    Gateway, GrokImportPreview,
};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};
use toml_edit::value;

const IMPORT_CONFIG: &str = r#"
operator_setting = "fixture-keep"

[models]
default = "native-model"
extra_headers = { "x-route" = "fixture-route" }

[model.native-model]
model = "grok-4.5"
api_backend = "responses"
env_key = "XAI_API_KEY"
native_metadata = "fixture-native"

[model.other-model]
model = "grok-4.1"
api_backend = "responses"
base_url = "https://other.example.invalid/v1"
api_key = "fixture-other-key"
other_metadata = "fixture-other"
"#;

const IMPORT_ENV: &str = "# fixture-only values\nXAI_API_KEY=fixture-import-api-key\nGROK_MODELS_BASE_URL=https://api.example.invalid/v1\nKEEP_VALUE=fixture-retained\n";

struct Fixture {
    _temp: tempfile::TempDir,
    _primary: Gateway,
    gateway: Gateway,
    home: PathBuf,
    data: PathBuf,
}

impl Fixture {
    fn new(config: &str, environment: Option<&str>) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("grok-home");
        fs::create_dir_all(&home).unwrap();
        fs::write(home.join("config.toml"), config).unwrap();
        if let Some(environment) = environment {
            fs::write(home.join("endpoint.env"), environment).unwrap();
        }

        let data = temp.path().join("grok-store");
        let primary = Gateway::new(temp.path().join("codex-store")).unwrap();
        let gateway = primary.grok_companion(data.clone()).unwrap();
        Self {
            _temp: temp,
            _primary: primary,
            gateway,
            home,
            data,
        }
    }

    fn valid_preview(&self, selected: Option<&str>, use_environment: bool) -> GrokImportPreview {
        let preview = self
            .gateway
            .preview_grok_import_with_env(&self.home, selected, use_environment, &|_| None)
            .unwrap();
        assert!(
            preview.error.is_none(),
            "unexpected preview error: {:?}",
            preview.error
        );
        assert!(
            preview.token.is_some(),
            "a valid preview should be committable"
        );
        preview
    }

    fn commit(&self, preview: &GrokImportPreview) -> crate::storage::Result<super::View> {
        self.gateway.commit_grok_import(
            &self.home,
            preview.token.as_deref().unwrap(),
            &preview.revision,
        )
    }
}

struct RecoveryFixture {
    _temp: tempfile::TempDir,
    home: PathBuf,
    data: PathBuf,
}

impl RecoveryFixture {
    fn new(config: Option<&str>, environment: Option<&str>, store: Option<&str>) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("grok-home");
        let data = temp.path().join("grok-store");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&data).unwrap();
        for (path, content) in [
            (home.join("config.toml"), config),
            (home.join("endpoint.env"), environment),
            (data.join("gateway.json"), store),
        ] {
            if let Some(content) = content {
                fs::write(path, content).unwrap();
            }
        }
        Self {
            _temp: temp,
            home,
            data,
        }
    }

    fn write_journal(
        &self,
        config_before: Option<&str>,
        config_after: &str,
        environment: Option<(Option<&str>, &str)>,
        store_before: Option<&str>,
        store_after: &str,
    ) -> Vec<u8> {
        let environment = environment
            .map(|(before, after)| serde_json::json!({ "before": before, "after": after }));
        let journal = serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "home": self.home.display().to_string(),
            "config": { "before": config_before, "after": config_after },
            "environment": environment,
            "store": { "before": store_before, "after": store_after },
        }))
        .unwrap();
        fs::write(self.data.join("grok-import.json"), &journal).unwrap();
        journal
    }
}

fn optional_bytes(path: &Path) -> Option<Vec<u8>> {
    fs::read(path).ok()
}

fn committed_config(config: &str, environment: Option<&str>) -> String {
    let fixture = Fixture::new(config, environment);
    let preview = fixture.valid_preview(None, environment.is_some());
    fixture.commit(&preview).unwrap();
    fs::read_to_string(fixture.home.join("config.toml")).unwrap()
}

fn has_auth_headers(item: Option<&toml_edit::Item>) -> bool {
    item.and_then(toml_edit::Item::as_table_like)
        .is_some_and(|headers| {
            headers.iter().any(|(name, _)| {
                name.eq_ignore_ascii_case("authorization") || name.eq_ignore_ascii_case("x-api-key")
            })
        })
}

fn header_value(item: Option<&toml_edit::Item>, name: &str) -> Option<String> {
    item.and_then(toml_edit::Item::as_table_like)
        .and_then(|headers| headers.get(name))
        .and_then(toml_edit::Item::as_str)
        .map(str::to_owned)
}

#[test]
fn profile_with_env_resolves_sparse_model_from_injected_global_endpoint() {
    let config = r#"
[models]
default = "sparse-model"

[model.sparse-model]
api_backend = "responses"
env_key = "XAI_API_KEY"

[endpoints]
models_base_url = "https://global.example.invalid/v1"
"#;

    let profile = grok_config::profile_with_env(config, &|name| match name {
        "XAI_API_KEY" => Some("fixture-injected-api-key".into()),
        _ => None,
    })
    .unwrap();

    assert_eq!(profile.selected, "sparse-model");
    assert_eq!(profile.model, "sparse-model");
    assert_eq!(profile.connection.protocol, Protocol::Responses);
    assert_eq!(
        profile.pair.base_url.as_deref(),
        Some("https://global.example.invalid/v1")
    );
    assert_eq!(profile.address_source, Some("global"));
    assert_eq!(
        profile.pair.token.as_deref(),
        Some("fixture-injected-api-key")
    );
}

#[test]
fn profile_prefers_model_api_base_url_then_base_url_and_injected_auth_method() {
    let config = r#"
[models]
default = "custom-model"

[model.custom-model]
model = "grok-4.5"
api_backend = "responses"
api_base_url = "https://model-api.example.invalid/v1"
base_url = "https://model-base.example.invalid/v1"
api_key = "fixture-inline-key"
env_key = "XAI_API_KEY"

[endpoints]
models_base_url = "https://global.example.invalid/v1"
xai_api_base_url = "https://global-api.example.invalid/v1"

[auth]
preferred_method = "oidc"
"#;
    let injected = |name: &str| match name {
        "GROK_AUTH_PREFERRED_METHOD" => Some("api_key".into()),
        "GROK_MODELS_BASE_URL" => Some("https://environment.example.invalid/v1".into()),
        "GROK_XAI_API_BASE_URL" => Some("https://environment-api.example.invalid/v1".into()),
        "XAI_API_KEY" => Some("fixture-environment-key".into()),
        _ => None,
    };

    let profile = grok_config::profile_with_env(config, &injected).unwrap();
    assert_eq!(
        profile.pair.base_url.as_deref(),
        Some("https://model-api.example.invalid/v1")
    );
    assert_eq!(profile.address_source, Some("modelApi"));
    assert_eq!(profile.pair.token.as_deref(), Some("fixture-inline-key"));

    let without_api_base_url = config.replace(
        "api_base_url = \"https://model-api.example.invalid/v1\"\n",
        "",
    );
    let profile = grok_config::profile_with_env(&without_api_base_url, &injected).unwrap();
    assert_eq!(
        profile.pair.base_url.as_deref(),
        Some("https://model-base.example.invalid/v1")
    );
    assert_eq!(profile.address_source, Some("model"));

    let oidc = |name: &str| match name {
        "GROK_AUTH_PREFERRED_METHOD" => Some("oidc".into()),
        "XAI_API_KEY" => Some("fixture-environment-key".into()),
        _ => None,
    };
    let profile = grok_config::profile_with_env(config, &oidc).unwrap();
    assert_eq!(profile.pair.token.as_deref(), Some("fixture-inline-key"));

    let no_api_key = config.replace("api_key = \"fixture-inline-key\"\n", "");
    let profile = grok_config::profile_with_env(&no_api_key, &oidc).unwrap();
    assert_eq!(
        profile.pair.token.as_deref(),
        Some("fixture-environment-key")
    );

    let global_oidc = r#"
[models]
default = "global-fallback"

[model.global-fallback]
base_url = "https://global.example.invalid/v1"

[auth]
preferred_method = "oidc"
"#;
    let profile = grok_config::profile_with_env(global_oidc, &oidc).unwrap();
    assert!(profile.pair.token.is_none());
    assert_eq!(
        profile.credential_names,
        ["XAI_API_KEY", "GROK_CODE_XAI_API_KEY"]
    );
}

#[test]
fn selected_model_credentials_override_shared_provider_credentials() {
    let inline_override = r#"
[models]
default = "selected-model"

[model_providers.shared]
base_url = "https://shared.example.invalid/v1"
api_key = "fixture-provider-inline-key"
env_key = "PROVIDER_API_KEY"
api_backend = "messages"

[model.selected-model]
model_provider = "shared"
model = "grok-4.1"
api_key = "fixture-model-inline-key"
"#;
    let env = |name: &str| match name {
        "PROVIDER_API_KEY" => Some("fixture-provider-environment-key".into()),
        "MODEL_API_KEY" => Some("fixture-model-environment-key".into()),
        _ => None,
    };

    let profile = grok_config::profile_with_env(inline_override, &env).unwrap();
    assert_eq!(profile.connection.protocol, Protocol::Messages);
    assert_eq!(
        profile.pair.base_url.as_deref(),
        Some("https://shared.example.invalid/v1")
    );
    assert_eq!(
        profile.pair.token.as_deref(),
        Some("fixture-model-inline-key")
    );

    let environment_override = inline_override.replace(
        "api_key = \"fixture-model-inline-key\"",
        "env_key = \"MODEL_API_KEY\"",
    );
    let profile = grok_config::profile_with_env(&environment_override, &env).unwrap();
    assert_eq!(
        profile.pair.token.as_deref(),
        Some("fixture-model-environment-key")
    );
    assert_eq!(profile.credential_names, ["MODEL_API_KEY"]);
}

#[test]
fn endpoint_environment_parser_accepts_literals_and_never_executes_commands() {
    let parsed = grok_import::environment(
        "# fixture\nexport XAI_API_KEY=fixture-env-key\nGROK_MODELS_BASE_URL=\"https://proxy.example.invalid/v1\"\nGROK_AUTH_PREFERRED_METHOD='api_key' # literal\n",
    )
    .unwrap();
    assert_eq!(
        parsed.get("XAI_API_KEY").map(String::as_str),
        Some("fixture-env-key")
    );
    assert_eq!(
        parsed.get("GROK_MODELS_BASE_URL").map(String::as_str),
        Some("https://proxy.example.invalid/v1")
    );
    assert_eq!(
        parsed.get("GROK_AUTH_PREFERRED_METHOD").map(String::as_str),
        Some("api_key")
    );

    let temp = tempfile::tempdir().unwrap();
    let marker = temp.path().join("command-ran");
    assert!(!marker.exists());
    let command = format!("XAI_API_KEY=$(touch {})", marker.display());
    let error = grok_import::environment(&command).unwrap_err();
    assert_eq!(error.code, "GROK_CONFIG");
    assert!(
        !marker.exists(),
        "parsing the fixture must not execute its command"
    );

    for script in [
        format!("touch {}", marker.display()),
        format!("XAI_API_KEY=fixture; touch {}", marker.display()),
        format!("XAI_API_KEY=\"$(touch {})\"", marker.display()),
    ] {
        assert!(
            grok_import::environment(&script).is_err(),
            "accepted script: {script}"
        );
        assert!(
            !marker.exists(),
            "parsing the fixture must not execute its command"
        );
    }
}

#[test]
fn preview_and_cancel_leave_config_environment_and_store_untouched() {
    let fixture = Fixture::new(IMPORT_CONFIG, Some(IMPORT_ENV));
    let config_path = fixture.home.join("config.toml");
    let environment_path = fixture.home.join("endpoint.env");
    let store_path = fixture.data.join("gateway.json");
    let before_config = optional_bytes(&config_path);
    let before_environment = optional_bytes(&environment_path);
    let before_store = optional_bytes(&store_path);

    let preview = fixture.valid_preview(None, true);
    assert!(preview.environment_available);
    assert!(preview.environment_loaded);
    assert_eq!(optional_bytes(&config_path), before_config);
    assert_eq!(optional_bytes(&environment_path), before_environment);
    assert_eq!(optional_bytes(&store_path), before_store);
    assert!(!fixture.data.join("grok-import.json").exists());

    let token = preview.token.as_deref().unwrap();
    fixture.gateway.cancel_grok_import(Some(token));
    let error = fixture
        .commit(&preview)
        .err()
        .expect("a cancelled preview cannot be committed");
    assert_eq!(error.code, "GROK_CONFIG");
    assert_eq!(optional_bytes(&config_path), before_config);
    assert_eq!(optional_bytes(&environment_path), before_environment);
    assert_eq!(optional_bytes(&store_path), before_store);
    assert!(!fixture.data.join("grok-import.json").exists());
}

#[test]
fn commit_migrates_config_environment_and_store_once_without_duplicates() {
    let fixture = Fixture::new(IMPORT_CONFIG, Some(IMPORT_ENV));
    let config_path = fixture.home.join("config.toml");
    let environment_path = fixture.home.join("endpoint.env");
    let preview = fixture.valid_preview(None, true);
    assert!(preview.clear_environment.contains(&"XAI_API_KEY".into()));
    assert!(preview
        .clear_environment
        .contains(&"GROK_MODELS_BASE_URL".into()));

    let view = fixture.commit(&preview).unwrap();
    let config = fs::read_to_string(&config_path).unwrap();
    let document = config.parse::<toml_edit::DocumentMut>().unwrap();
    assert_eq!(
        document
            .get("models")
            .and_then(|models| models.get("default"))
            .and_then(|default| default.as_str()),
        Some(grok_config::MANAGED)
    );
    assert_eq!(
        document
            .get("operator_setting")
            .and_then(|value| value.as_str()),
        Some("fixture-keep")
    );
    let managed = document
        .get("model")
        .and_then(|models| models.get(grok_config::MANAGED))
        .expect("the gateway model should be written");
    assert_eq!(
        managed.get("base_url").and_then(|value| value.as_str()),
        Some("https://api.example.invalid/v1")
    );
    assert_eq!(
        managed.get("api_key").and_then(|value| value.as_str()),
        Some("fixture-import-api-key")
    );
    assert_eq!(
        managed.get("api_backend").and_then(|value| value.as_str()),
        Some("responses")
    );
    assert_eq!(
        document
            .get("model")
            .and_then(|models| models.get("other-model"))
            .and_then(|model| model.get("other_metadata"))
            .and_then(|value| value.as_str()),
        Some("fixture-other")
    );

    let environment = fs::read_to_string(&environment_path).unwrap();
    assert!(!environment.contains("XAI_API_KEY="));
    assert!(!environment.contains("GROK_MODELS_BASE_URL="));
    assert!(environment.contains("KEEP_VALUE=fixture-retained"));

    let store_path = fixture.data.join("gateway.json");
    let store: serde_json::Value = serde_json::from_slice(&fs::read(&store_path).unwrap()).unwrap();
    let providers = store["providers"].as_array().unwrap();
    assert_eq!(providers.len(), 1);
    let provider_id = store["selected"].as_str().unwrap();
    assert_eq!(providers[0]["id"].as_str(), Some(provider_id));
    assert_eq!(
        providers[0]["baseUrl"].as_str(),
        Some("https://api.example.invalid/v1")
    );
    assert_eq!(
        providers[0]["token"].as_str(),
        Some("fixture-import-api-key")
    );
    assert_eq!(store["grokApiModel"].as_str(), Some(grok_config::MANAGED));
    assert_eq!(
        store["grokConnections"][provider_id]["protocol"],
        "responses"
    );
    assert_eq!(view.providers.len(), 1);
    assert_eq!(view.selected.as_deref(), Some(provider_id));
    assert!(!fixture.data.join("grok-import.json").exists());

    let environment_after_first_commit = fs::read(&environment_path).unwrap();
    let second = fixture.valid_preview(Some(grok_config::MANAGED), false);
    fixture.commit(&second).unwrap();

    let final_store: serde_json::Value =
        serde_json::from_slice(&fs::read(&store_path).unwrap()).unwrap();
    assert_eq!(final_store["providers"].as_array().unwrap().len(), 1);
    assert_eq!(
        final_store["providers"][0]["id"].as_str(),
        Some(provider_id)
    );
    assert_eq!(
        fs::read(&environment_path).unwrap(),
        environment_after_first_commit
    );
    let final_config = fs::read_to_string(&config_path).unwrap();
    let final_document = final_config.parse::<toml_edit::DocumentMut>().unwrap();
    assert_eq!(
        final_document
            .get("model")
            .and_then(|models| models.as_table_like())
            .map(|models| models.iter().count()),
        Some(3)
    );
}

#[test]
fn commit_rejects_config_revision_conflict_without_partial_writes() {
    let fixture = Fixture::new(IMPORT_CONFIG, Some(IMPORT_ENV));
    let preview = fixture.valid_preview(None, true);
    let config_path = fixture.home.join("config.toml");
    let environment_path = fixture.home.join("endpoint.env");
    let store_path = fixture.data.join("gateway.json");
    let before_environment = optional_bytes(&environment_path);
    let before_store = optional_bytes(&store_path);

    let mut changed = fs::read(&config_path).unwrap();
    changed.extend_from_slice(b"\nexternal_edit = \"fixture-source-conflict\"\n");
    fs::write(&config_path, &changed).unwrap();

    let error = fixture
        .commit(&preview)
        .err()
        .expect("a changed source configuration must conflict");
    assert_eq!(error.code, "CONFLICT");
    assert_eq!(optional_bytes(&config_path), Some(changed));
    assert_eq!(optional_bytes(&environment_path), before_environment);
    assert_eq!(optional_bytes(&store_path), before_store);
    assert!(!fixture.data.join("grok-import.json").exists());
}

#[test]
fn commit_rejects_environment_revision_conflict_without_partial_writes() {
    let fixture = Fixture::new(IMPORT_CONFIG, Some(IMPORT_ENV));
    let preview = fixture.valid_preview(None, true);
    let config_path = fixture.home.join("config.toml");
    let environment_path = fixture.home.join("endpoint.env");
    let store_path = fixture.data.join("gateway.json");
    let before_config = optional_bytes(&config_path);
    let before_store = optional_bytes(&store_path);
    let changed = b"XAI_API_KEY=fixture-new-value\n";
    fs::write(&environment_path, changed).unwrap();

    let error = fixture
        .commit(&preview)
        .err()
        .expect("a changed endpoint.env must conflict");
    assert_eq!(error.code, "CONFLICT");
    assert_eq!(optional_bytes(&config_path), before_config);
    assert_eq!(optional_bytes(&environment_path), Some(changed.to_vec()));
    assert_eq!(optional_bytes(&store_path), before_store);
    assert!(!fixture.data.join("grok-import.json").exists());
}

#[test]
fn commit_rejects_provider_store_revision_conflict_without_partial_writes() {
    let fixture = Fixture::new(IMPORT_CONFIG, Some(IMPORT_ENV));
    let preview = fixture.valid_preview(None, true);
    let config_path = fixture.home.join("config.toml");
    let environment_path = fixture.home.join("endpoint.env");
    let store_path = fixture.data.join("gateway.json");
    let before_config = optional_bytes(&config_path);
    let before_environment = optional_bytes(&environment_path);
    let mut external = optional_bytes(&store_path).unwrap_or_else(|| b"missing".to_vec());
    external.extend_from_slice(b"\nexternal-store-edit");
    fs::write(&store_path, &external).unwrap();

    let error = fixture
        .commit(&preview)
        .err()
        .expect("an externally changed provider store must conflict");
    assert_eq!(error.code, "CONFLICT");
    assert_eq!(optional_bytes(&config_path), before_config);
    assert_eq!(optional_bytes(&environment_path), before_environment);
    assert_eq!(optional_bytes(&store_path), Some(external));
    assert!(!fixture.data.join("grok-import.json").exists());
}

#[test]
fn api_snapshot_preserves_native_selection_and_unowned_edits_on_restore() {
    let original = r#"
operator_setting = "fixture-original"

[models]
default = "grok-native-one"
extra_headers = { "x-route" = "fixture-route" }

[model.grok-native-one]
model = "grok-4.5"
api_backend = "responses"
base_url = "https://native-one.example.invalid/v1"
api_key = "fixture-native-one-key"
native_metadata = "fixture-one"

[model.grok-native-two]
model = "grok-4.6"
api_backend = "responses"
base_url = "https://native-two.example.invalid/v1"
api_key = "fixture-native-two-key"
native_metadata = "fixture-two"
"#;
    let target = Pair::new("http://127.0.0.1:43127/v1", "fixture-loopback-token");
    let connection = Connection {
        protocol: Protocol::Responses,
        headers: BTreeMap::from([("x-tenant".into(), "fixture-tenant".into())]),
        query: BTreeMap::from([("routing".into(), "fixture-route".into())]),
    };
    let snapshot = grok_config::Snapshot::api(original, &target, &connection, true).unwrap();
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("config.toml");
    fs::write(&path, original).unwrap();

    snapshot.apply(&path, None).unwrap();
    let applied = fs::read_to_string(&path).unwrap();
    let applied_document = applied.parse::<toml_edit::DocumentMut>().unwrap();
    assert_eq!(
        applied_document
            .get("models")
            .and_then(|models| models.get("default"))
            .and_then(|default| default.as_str()),
        Some("grok-native-one")
    );
    assert!(applied_document
        .get("model")
        .and_then(|models| models.get(grok_config::MANAGED))
        .is_some());

    let mut official_change = applied.parse::<toml_edit::DocumentMut>().unwrap();
    official_change["models"]["default"] = value("grok-native-two");
    official_change["operator_setting"] = value("fixture-external-edit");
    let official_change = official_change.to_string();
    fs::write(&path, &official_change).unwrap();
    assert!(snapshot.unchanged(&official_change).unwrap());

    snapshot.restore(&path).unwrap();
    let restored = fs::read_to_string(&path).unwrap();
    let restored_document = restored.parse::<toml_edit::DocumentMut>().unwrap();
    assert_eq!(
        restored_document
            .get("models")
            .and_then(|models| models.get("default"))
            .and_then(|default| default.as_str()),
        Some("grok-native-two")
    );
    assert_eq!(
        restored_document
            .get("operator_setting")
            .and_then(|value| value.as_str()),
        Some("fixture-external-edit")
    );
    assert_eq!(
        restored_document
            .get("model")
            .and_then(|models| models.get("grok-native-one"))
            .and_then(|model| model.get("native_metadata"))
            .and_then(|value| value.as_str()),
        Some("fixture-one")
    );
    assert_eq!(
        restored_document
            .get("model")
            .and_then(|models| models.get("grok-native-two"))
            .and_then(|model| model.get("native_metadata"))
            .and_then(|value| value.as_str()),
        Some("fixture-two")
    );
    assert!(restored_document
        .get("model")
        .and_then(|models| models.get(grok_config::MANAGED))
        .is_none());
}

#[test]
fn recover_finishes_interrupted_config_write_and_repeats_without_side_effects() {
    let config_before = "fixture-config-before";
    let config_after = "fixture-config-after";
    let environment_before = "XAI_API_KEY=fixture-before\nKEEP_VALUE=fixture-retained\n";
    let environment_after = "KEEP_VALUE=fixture-retained\n";
    let store_after = "{\"store\":\"fixture-after\"}";
    let fixture = RecoveryFixture::new(Some(config_before), Some(environment_before), None);
    let config_path = fixture.home.join("config.toml");
    let environment_path = fixture.home.join("endpoint.env");
    let store_path = fixture.data.join("gateway.json");

    // The config write reached disk, but environment and store writes did not.
    fs::write(&config_path, config_after).unwrap();
    fixture.write_journal(
        Some(config_before),
        config_after,
        Some((Some(environment_before), environment_after)),
        None,
        store_after,
    );

    grok_import::recover(&fixture.data).unwrap();
    assert_eq!(fs::read(&config_path).unwrap(), config_after.as_bytes());
    assert_eq!(
        fs::read(&environment_path).unwrap(),
        environment_after.as_bytes()
    );
    assert_eq!(fs::read(&store_path).unwrap(), store_after.as_bytes());
    assert!(!fixture.data.join("grok-import.json").exists());

    let after_first_recovery = (
        optional_bytes(&config_path),
        optional_bytes(&environment_path),
        optional_bytes(&store_path),
    );
    grok_import::recover(&fixture.data).unwrap();
    assert_eq!(
        (
            optional_bytes(&config_path),
            optional_bytes(&environment_path),
            optional_bytes(&store_path),
        ),
        after_first_recovery
    );
}

#[test]
fn recover_finishes_interrupted_environment_write_before_store() {
    let config_before = "fixture-config-before";
    let config_after = "fixture-config-after";
    let environment_before = "XAI_API_KEY=fixture-before\nKEEP_VALUE=fixture-retained\n";
    let environment_after = "KEEP_VALUE=fixture-retained\n";
    let store_before = "{\"store\":\"fixture-before\"}";
    let store_after = "{\"store\":\"fixture-after\"}";
    let fixture = RecoveryFixture::new(
        Some(config_before),
        Some(environment_before),
        Some(store_before),
    );
    let config_path = fixture.home.join("config.toml");
    let environment_path = fixture.home.join("endpoint.env");
    let store_path = fixture.data.join("gateway.json");

    // Both config and environment writes reached disk; the store write did not.
    fs::write(&config_path, config_after).unwrap();
    fs::write(&environment_path, environment_after).unwrap();
    fixture.write_journal(
        Some(config_before),
        config_after,
        Some((Some(environment_before), environment_after)),
        Some(store_before),
        store_after,
    );

    grok_import::recover(&fixture.data).unwrap();
    assert_eq!(fs::read(&config_path).unwrap(), config_after.as_bytes());
    assert_eq!(
        fs::read(&environment_path).unwrap(),
        environment_after.as_bytes()
    );
    assert_eq!(fs::read(&store_path).unwrap(), store_after.as_bytes());
    assert!(!fixture.data.join("grok-import.json").exists());
}

#[test]
fn recover_stops_all_writes_and_keeps_journal_on_any_external_conflict() {
    let config_before = "fixture-config-before";
    let config_after = "fixture-config-after";
    let environment_before = "KEEP_VALUE=fixture-before\n";
    let environment_after = "KEEP_VALUE=fixture-after\n";
    let store_before = "{\"store\":\"fixture-before\"}";
    let store_after = "{\"store\":\"fixture-after\"}";

    for conflicting_participant in ["config", "environment", "store"] {
        let fixture = RecoveryFixture::new(
            Some(config_before),
            Some(environment_before),
            Some(store_before),
        );
        let config_path = fixture.home.join("config.toml");
        let environment_path = fixture.home.join("endpoint.env");
        let store_path = fixture.data.join("gateway.json");
        let journal_path = fixture.data.join("grok-import.json");
        let journal = fixture.write_journal(
            Some(config_before),
            config_after,
            Some((Some(environment_before), environment_after)),
            Some(store_before),
            store_after,
        );
        let external = format!("fixture-external-{conflicting_participant}");
        let conflicting_path = match conflicting_participant {
            "config" => &config_path,
            "environment" => &environment_path,
            "store" => &store_path,
            _ => unreachable!(),
        };
        fs::write(conflicting_path, &external).unwrap();
        let before_recovery = (
            optional_bytes(&config_path),
            optional_bytes(&environment_path),
            optional_bytes(&store_path),
        );

        let result = grok_import::recover(&fixture.data);
        assert!(
            result.is_err(),
            "{conflicting_participant} conflict was ignored"
        );
        assert_eq!(
            result.err().unwrap().code,
            "CONFLICT",
            "unexpected error for {conflicting_participant} conflict"
        );
        assert_eq!(
            (
                optional_bytes(&config_path),
                optional_bytes(&environment_path),
                optional_bytes(&store_path),
            ),
            before_recovery,
            "recovery wrote a participant after a {conflicting_participant} conflict"
        );
        assert_eq!(
            optional_bytes(&journal_path),
            Some(journal),
            "the journal should remain after a {conflicting_participant} conflict"
        );
    }
}

#[test]
fn recover_preserves_a_damaged_journal_and_original_files() {
    let config = "fixture-config-original";
    let environment = "KEEP_VALUE=fixture-original\n";
    let fixture = RecoveryFixture::new(Some(config), Some(environment), None);
    let config_path = fixture.home.join("config.toml");
    let environment_path = fixture.home.join("endpoint.env");
    let journal_path = fixture.data.join("grok-import.json");
    let damaged_journal = b"{not-json";
    fs::write(&journal_path, damaged_journal).unwrap();

    let result = grok_import::recover(&fixture.data);
    assert!(result.is_err(), "a damaged journal must not be ignored");
    assert_eq!(
        optional_bytes(&journal_path),
        Some(damaged_journal.to_vec())
    );
    assert_eq!(fs::read(&config_path).unwrap(), config.as_bytes());
    assert_eq!(fs::read(&environment_path).unwrap(), environment.as_bytes());
}

#[test]
fn recover_accepts_a_journal_larger_than_two_mib_when_each_file_is_bounded() {
    let config_before = "b".repeat(1_100_000);
    let config_after = "a".repeat(1_100_000);
    let store_after = "{\"store\":\"fixture-large-journal\"}";
    let fixture = RecoveryFixture::new(Some(&config_before), None, None);
    let config_path = fixture.home.join("config.toml");
    let store_path = fixture.data.join("gateway.json");
    let journal =
        fixture.write_journal(Some(&config_before), &config_after, None, None, store_after);

    assert!(journal.len() > 2 * 1024 * 1024);
    assert!(config_before.len() <= 2 * 1024 * 1024);
    assert!(config_after.len() <= 2 * 1024 * 1024);
    assert!(store_after.len() <= 2 * 1024 * 1024);

    grok_import::recover(&fixture.data).unwrap();
    assert_eq!(fs::read(&config_path).unwrap(), config_after.as_bytes());
    assert_eq!(fs::read(&store_path).unwrap(), store_after.as_bytes());
    assert!(!fixture.data.join("grok-import.json").exists());
}

#[test]
fn preview_rejects_an_unknown_selected_model_without_creating_a_token() {
    let fixture = Fixture::new(IMPORT_CONFIG, None);
    let result = fixture.gateway.preview_grok_import_with_env(
        &fixture.home,
        Some("fixture-unlisted-model"),
        false,
        &|_| None,
    );

    assert!(result.is_err(), "an unknown model must not be inferred");
    assert_eq!(result.err().unwrap().code, "GROK_CONFIG");
    assert!(!fixture.data.join("grok-import.json").exists());
}

#[test]
fn preview_does_not_guess_a_missing_or_unsupported_protocol() {
    let sparse_config = r#"
[models]
default = "fixture-sparse-model"

[model.fixture-sparse-model]
base_url = "https://sparse.example.invalid/v1"
api_key = "fixture-sparse-key"
"#;
    let sparse = Fixture::new(sparse_config, None);
    let sparse_preview = sparse
        .gateway
        .preview_grok_import_with_env(&sparse.home, None, false, &|_| None)
        .unwrap();
    assert!(sparse_preview.token.is_none());
    assert!(sparse_preview
        .error
        .as_deref()
        .is_some_and(|message| message.contains("未声明协议")));
    let sparse_model = sparse_preview
        .models
        .iter()
        .find(|model| model.key == "fixture-sparse-model")
        .unwrap();
    assert!(sparse_model.protocol.is_none());
    assert!(sparse_model.credential_available);
    assert!(!sparse.data.join("grok-import.json").exists());

    let unsupported_config = r#"
[models]
default = "fixture-future-model"

[model.fixture-future-model]
model = "fixture-grok-model"
api_backend = "fixture-future-protocol"
base_url = "https://future.example.invalid/v1"
api_key = "fixture-future-key"
"#;
    let unsupported = Fixture::new(unsupported_config, None);
    let unsupported_preview = unsupported
        .gateway
        .preview_grok_import_with_env(&unsupported.home, None, false, &|_| None)
        .unwrap();
    assert!(unsupported_preview.token.is_none());
    assert!(unsupported_preview
        .error
        .as_deref()
        .is_some_and(|message| message.contains("协议不受支持")));
    let unsupported_model = unsupported_preview
        .models
        .iter()
        .find(|model| model.key == "fixture-future-model")
        .unwrap();
    assert!(unsupported_model.protocol.is_none());
    assert!(unsupported_model.error.is_some());
    assert!(!unsupported.data.join("grok-import.json").exists());
}

#[test]
fn imported_auth_headers_leave_native_config_and_preserve_other_fields() {
    let global_config = r#"
[models]
default = "fixture-native-model"
extra_headers = { "Authorization" = "Bearer fixture-global-auth", "X-Api-Key" = "fixture-global-x-api-key", "x-global-route" = "fixture-global-route" }

[model.fixture-native-model]
api_backend = "responses"
base_url = "https://global.example.invalid/v1"
api_key = "fixture-global-model-key"
native_metadata = "fixture-global-metadata"

[model.fixture-custom-model]
model = "grok-4.1"
api_backend = "responses"
base_url = "https://custom.example.invalid/v1"
api_key = "fixture-custom-model-key"
custom_metadata = "fixture-custom-metadata"
"#;
    let global_imported = committed_config(global_config, None);
    let global_doc = global_imported.parse::<toml_edit::DocumentMut>().unwrap();
    let global_headers = global_doc
        .get("models")
        .and_then(|models| models.get("extra_headers"));
    assert!(!has_auth_headers(global_headers));
    assert_eq!(
        header_value(global_headers, "x-global-route").as_deref(),
        Some("fixture-global-route")
    );
    let global_managed = global_doc
        .get("model")
        .and_then(|models| models.get(grok_config::MANAGED))
        .unwrap();
    let global_managed_headers = global_managed.get("extra_headers");
    assert_eq!(
        header_value(global_managed_headers, "authorization").as_deref(),
        Some("Bearer fixture-global-auth")
    );
    assert_eq!(
        header_value(global_managed_headers, "x-api-key").as_deref(),
        Some("fixture-global-x-api-key")
    );
    assert_eq!(
        header_value(global_managed_headers, "x-global-route").as_deref(),
        Some("fixture-global-route")
    );
    let global_native = global_doc
        .get("model")
        .and_then(|models| models.get("fixture-native-model"))
        .unwrap();
    assert_eq!(
        global_native
            .get("native_metadata")
            .and_then(toml_edit::Item::as_str),
        Some("fixture-global-metadata")
    );
    let global_custom = global_doc
        .get("model")
        .and_then(|models| models.get("fixture-custom-model"))
        .unwrap();
    assert_eq!(
        global_custom
            .get("custom_metadata")
            .and_then(toml_edit::Item::as_str),
        Some("fixture-custom-metadata")
    );

    let sparse_extra_headers_config = r#"
[models]
default = "fixture-sparse-model"

[model.fixture-sparse-model]
api_backend = "responses"
base_url = "https://sparse-extra.example.invalid/v1"
api_key = "fixture-sparse-extra-key"
extra_headers = { "aUtHoRiZaTiOn" = "Bearer fixture-sparse-auth", "X-API-KEY" = "fixture-sparse-x-api-key", "x-native-route" = "fixture-native-route" }
native_metadata = "fixture-sparse-extra-metadata"
"#;
    let sparse_extra_imported = committed_config(sparse_extra_headers_config, None);
    let sparse_extra_doc = sparse_extra_imported
        .parse::<toml_edit::DocumentMut>()
        .unwrap();
    let sparse_extra_native = sparse_extra_doc
        .get("model")
        .and_then(|models| models.get("fixture-sparse-model"))
        .unwrap();
    let sparse_native_headers = sparse_extra_native.get("extra_headers");
    assert!(!has_auth_headers(sparse_native_headers));
    assert_eq!(
        header_value(sparse_native_headers, "x-native-route").as_deref(),
        Some("fixture-native-route")
    );
    assert_eq!(
        sparse_extra_native
            .get("native_metadata")
            .and_then(toml_edit::Item::as_str),
        Some("fixture-sparse-extra-metadata")
    );
    let sparse_extra_managed = sparse_extra_doc
        .get("model")
        .and_then(|models| models.get(grok_config::MANAGED))
        .unwrap();
    let sparse_extra_managed_headers = sparse_extra_managed.get("extra_headers");
    assert_eq!(
        header_value(sparse_extra_managed_headers, "authorization").as_deref(),
        Some("Bearer fixture-sparse-auth")
    );
    assert_eq!(
        header_value(sparse_extra_managed_headers, "x-api-key").as_deref(),
        Some("fixture-sparse-x-api-key")
    );
    assert_eq!(
        header_value(sparse_extra_managed_headers, "x-native-route").as_deref(),
        Some("fixture-native-route")
    );

    let sparse_env_headers_config = r#"
[models]
default = "fixture-env-model"

[model.fixture-env-model]
api_backend = "responses"
base_url = "https://sparse-env.example.invalid/v1"
api_key = "fixture-sparse-env-key"
env_http_headers = { "AUTHORIZATION" = "FIXTURE_AUTH_HEADER", "X-Api-Key" = "FIXTURE_X_API_KEY_HEADER", "x-env-route" = "FIXTURE_ENV_ROUTE_HEADER" }
native_metadata = "fixture-env-metadata"
"#;
    let sparse_env = concat!(
        "FIXTURE_AUTH_HEADER=\"Bearer fixture-env-auth\"\n",
        "FIXTURE_X_API_KEY_HEADER=fixture-env-x-api-key\n",
        "FIXTURE_ENV_ROUTE_HEADER=fixture-env-route\n",
    );
    let sparse_env_imported = committed_config(sparse_env_headers_config, Some(sparse_env));
    let sparse_env_doc = sparse_env_imported
        .parse::<toml_edit::DocumentMut>()
        .unwrap();
    let sparse_env_native = sparse_env_doc
        .get("model")
        .and_then(|models| models.get("fixture-env-model"))
        .unwrap();
    let sparse_native_env_headers = sparse_env_native.get("env_http_headers");
    assert!(!has_auth_headers(sparse_native_env_headers));
    assert_eq!(
        header_value(sparse_native_env_headers, "x-env-route").as_deref(),
        Some("FIXTURE_ENV_ROUTE_HEADER")
    );
    assert_eq!(
        sparse_env_native
            .get("native_metadata")
            .and_then(toml_edit::Item::as_str),
        Some("fixture-env-metadata")
    );
    let sparse_env_managed = sparse_env_doc
        .get("model")
        .and_then(|models| models.get(grok_config::MANAGED))
        .unwrap();
    let sparse_env_managed_headers = sparse_env_managed.get("extra_headers");
    assert_eq!(
        header_value(sparse_env_managed_headers, "authorization").as_deref(),
        Some("Bearer fixture-env-auth")
    );
    assert_eq!(
        header_value(sparse_env_managed_headers, "x-api-key").as_deref(),
        Some("fixture-env-x-api-key")
    );
    assert_eq!(
        header_value(sparse_env_managed_headers, "x-env-route").as_deref(),
        Some("fixture-env-route")
    );
}

#[test]
fn repeated_import_normalizes_endpoint_and_refreshes_provider_version_for_connection_changes() {
    let environment = IMPORT_ENV.replace(
        "https://api.example.invalid/v1",
        "https://api.example.invalid/v1/",
    );
    let fixture = Fixture::new(IMPORT_CONFIG, Some(&environment));

    let first_preview = fixture.valid_preview(None, true);
    fixture.commit(&first_preview).unwrap();
    let (first_id, first_version, first_base_url) = {
        let state = fixture.gateway.0.inner.lock().unwrap();
        assert_eq!(state.store.providers.len(), 1);
        let provider = &state.store.providers[0];
        (
            provider.id.clone(),
            provider.version.clone(),
            provider.base_url.clone(),
        )
    };
    assert_eq!(first_base_url, "https://api.example.invalid/v1");

    let second_preview = fixture.valid_preview(None, true);
    fixture.commit(&second_preview).unwrap();
    let (second_id, second_version, second_count) = {
        let state = fixture.gateway.0.inner.lock().unwrap();
        (
            state.store.providers[0].id.clone(),
            state.store.providers[0].version.clone(),
            state.store.providers.len(),
        )
    };
    assert_eq!(second_count, 1);
    assert_eq!(second_id, first_id);
    assert_eq!(second_version, first_version);

    let config_path = fixture.home.join("config.toml");
    let mut document = fs::read_to_string(&config_path)
        .unwrap()
        .parse::<toml_edit::DocumentMut>()
        .unwrap();
    document["model"][grok_config::MANAGED]["extra_headers"]["x-import-version"] =
        value("fixture-updated-header");
    fs::write(&config_path, document.to_string()).unwrap();

    let changed_preview = fixture.valid_preview(None, true);
    fixture.commit(&changed_preview).unwrap();
    let (changed_id, changed_version, changed_count) = {
        let state = fixture.gateway.0.inner.lock().unwrap();
        (
            state.store.providers[0].id.clone(),
            state.store.providers[0].version.clone(),
            state.store.providers.len(),
        )
    };
    assert_eq!(changed_count, 1);
    assert_eq!(changed_id, first_id);
    assert_ne!(changed_version, second_version);
}
