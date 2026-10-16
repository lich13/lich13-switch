// 覆盖 Grok 原生模型、连接来源及配置往返。
use super::{
    grok_config::{self, Protocol},
    takeover::Pair,
};
use std::fs;

const SOURCE_CONFIG: &str = r#"
# Keep this operator comment and unrelated setting.
operator_setting = "fixture-v1"

[models]
# Keep the selector comment.
default = "grok-4.5"
extra_headers = { "x-route" = "fixture-route" }

[model."grok-4.5"]
model_provider = "shared"
model = "grok-4.5"
local_metadata = "keep-selected-model"

[model."grok-4.1"]
model_provider = "other"
model = "grok-4.1"
api_key = "fixture-other-model-key"
local_metadata = "keep-other-model"

[model_providers.shared]
base_url = "https://provider.example.invalid/v1"
api_key = "fixture-original-upstream-key"
api_backend = "messages"
extra_headers = { "x-tenant" = "fixture-shared-tenant", "Authorization" = "Bearer fixture-shared-auth", "x-api-key" = "fixture-shared-api-key" }
provider_metadata = "keep-shared-provider"

[model_providers.other]
base_url = "https://other.example.invalid/v1"
api_key = "fixture-other-provider-key"
"#;

fn config_with_model_fields(fields: &str) -> String {
    format!("[models]\ndefault = \"grok-4.5\"\n\n[model.\"grok-4.5\"]\n{fields}\n")
}

fn missing_environment_names(count: usize) -> Vec<String> {
    (0..4096)
        .map(|index| format!("LICH13_SWITCH_GROK_CONFIG_TEST_MISSING_{index}"))
        .filter(|name| std::env::var_os(name).is_none())
        .take(count)
        .collect()
}

#[test]
fn parses_each_native_protocol_and_its_route() {
    for (name, expected, route) in [
        ("responses", Protocol::Responses, "/v1/responses"),
        (
            "chat_completions",
            Protocol::ChatCompletions,
            "/v1/chat/completions",
        ),
        ("messages", Protocol::Messages, "/v1/messages"),
    ] {
        let config = config_with_model_fields(&format!(
            "api_backend = \"{name}\"\nbase_url = \"https://api.example.invalid/v1\"\napi_key = \"fixture-api-key\""
        ));
        let profile = grok_config::profile(&config).unwrap();

        assert_eq!(profile.connection.protocol, expected, "{name}");
        assert!(profile.connection.protocol.accepts(route), "{name}");
        assert_eq!(
            profile.pair.base_url.as_deref(),
            Some("https://api.example.invalid/v1"),
            "{name}"
        );
        assert_eq!(
            profile.pair.token.as_deref(),
            Some("fixture-api-key"),
            "{name}"
        );
    }
}

#[test]
fn inline_api_key_precedes_missing_env_keys_without_changing_process_environment() {
    let missing = missing_environment_names(2);
    assert_eq!(
        missing.len(),
        2,
        "two missing fixture variable names are needed"
    );

    let config = format!(
        "[models]\ndefault = \"grok-4.5\"\n\n[model.\"grok-4.5\"]\napi_key = \"fixture-inline-api-key\"\nenv_key = [{:?}, {:?}]\n",
        missing[0], missing[1]
    );
    let profile = grok_config::profile(&config).unwrap();
    assert_eq!(
        profile.pair.token.as_deref(),
        Some("fixture-inline-api-key")
    );

    let env_only = format!(
        "[models]\ndefault = \"grok-4.5\"\n\n[model.\"grok-4.5\"]\nenv_key = [{:?}, {:?}]\n",
        missing[0], missing[1]
    );
    assert!(grok_config::pair(&env_only).unwrap().token.is_none());
}

#[test]
fn shared_provider_is_resolved_and_snapshot_preserves_unowned_toml() {
    let source_profile = grok_config::profile(SOURCE_CONFIG).unwrap();
    assert_eq!(
        source_profile.pair.base_url.as_deref(),
        Some("https://provider.example.invalid/v1")
    );
    assert_eq!(
        source_profile.pair.token.as_deref(),
        Some("fixture-original-upstream-key")
    );
    assert_eq!(source_profile.connection.protocol, Protocol::Messages);
    assert_eq!(
        source_profile
            .connection
            .headers
            .get("x-route")
            .map(String::as_str),
        Some("fixture-route")
    );
    assert_eq!(
        source_profile
            .connection
            .headers
            .get("x-tenant")
            .map(String::as_str),
        Some("fixture-shared-tenant")
    );
    assert_eq!(
        source_profile
            .connection
            .headers
            .get("authorization")
            .map(String::as_str),
        Some("Bearer fixture-shared-auth")
    );
    assert_eq!(
        source_profile
            .connection
            .headers
            .get("x-api-key")
            .map(String::as_str),
        Some("fixture-shared-api-key")
    );

    let target = Pair::new("http://127.0.0.1:43127/v1", "fixture-loopback-token");
    let snapshot =
        grok_config::Snapshot::with_protocol(SOURCE_CONFIG, &target, Some(Protocol::Responses))
            .unwrap();
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("config.toml");
    fs::write(&path, SOURCE_CONFIG).unwrap();

    snapshot.apply(&path, None).unwrap();
    let applied = fs::read_to_string(&path).unwrap();
    assert!(snapshot.unchanged(&applied).unwrap());
    assert!(applied.contains("# Keep this operator comment and unrelated setting."));
    assert!(applied.contains("# Keep the selector comment."));

    let applied_doc = applied.parse::<toml_edit::DocumentMut>().unwrap();
    assert_eq!(
        applied_doc
            .get("operator_setting")
            .and_then(|value| value.as_str()),
        Some("fixture-v1")
    );
    let managed = applied_doc
        .get("model")
        .and_then(|models| models.get(grok_config::MANAGED))
        .expect("managed model should be present");
    assert_eq!(
        managed.get("api_key").and_then(|value| value.as_str()),
        Some("fixture-loopback-token")
    );
    assert!(!managed
        .to_string()
        .contains("fixture-original-upstream-key"));
    assert!(managed.get("env_key").is_none());
    assert!(managed.get("model_provider").is_none());
    assert_eq!(
        managed
            .get("local_metadata")
            .and_then(|value| value.as_str()),
        Some("keep-selected-model")
    );
    let managed_headers = managed
        .get("extra_headers")
        .and_then(|headers| headers.as_table_like())
        .expect("managed headers should be present");
    assert_eq!(
        managed_headers
            .get("authorization")
            .and_then(|value| value.as_str()),
        Some("Bearer fixture-loopback-token")
    );
    assert_eq!(
        managed_headers
            .get("x-api-key")
            .and_then(|value| value.as_str()),
        Some("fixture-loopback-token")
    );

    let selected_model = applied_doc
        .get("model")
        .and_then(|models| models.get("grok-4.5"))
        .expect("original selected model should be preserved");
    assert_eq!(
        selected_model
            .get("local_metadata")
            .and_then(|value| value.as_str()),
        Some("keep-selected-model")
    );

    let other_model = applied_doc
        .get("model")
        .and_then(|models| models.get("grok-4.1"))
        .expect("other model should be preserved");
    assert_eq!(
        other_model.get("api_key").and_then(|value| value.as_str()),
        Some("fixture-other-model-key")
    );
    assert_eq!(
        other_model
            .get("local_metadata")
            .and_then(|value| value.as_str()),
        Some("keep-other-model")
    );
    let shared = applied_doc
        .get("model_providers")
        .and_then(|providers| providers.get("shared"))
        .expect("shared provider should be preserved");
    assert_eq!(
        shared.get("api_key").and_then(|value| value.as_str()),
        Some("fixture-original-upstream-key")
    );
    assert_eq!(
        shared
            .get("provider_metadata")
            .and_then(|value| value.as_str()),
        Some("keep-shared-provider")
    );
    let shared_headers = shared
        .get("extra_headers")
        .and_then(|headers| headers.as_table_like())
        .expect("shared provider headers should be preserved");
    assert_eq!(
        shared_headers
            .get("Authorization")
            .and_then(|value| value.as_str()),
        Some("Bearer fixture-shared-auth")
    );
    assert_eq!(
        shared_headers
            .get("x-api-key")
            .and_then(|value| value.as_str()),
        Some("fixture-shared-api-key")
    );

    let managed_profile = grok_config::profile(&applied).unwrap();
    assert_eq!(
        managed_profile.pair.base_url.as_deref(),
        Some("http://127.0.0.1:43127/v1")
    );
    assert_eq!(
        managed_profile.pair.token.as_deref(),
        Some("fixture-loopback-token")
    );
    assert_eq!(managed_profile.connection.protocol, Protocol::Responses);
    assert_eq!(
        managed_profile
            .connection
            .headers
            .get("authorization")
            .map(String::as_str),
        Some("Bearer fixture-loopback-token")
    );
    assert_eq!(
        managed_profile
            .connection
            .headers
            .get("x-api-key")
            .map(String::as_str),
        Some("fixture-loopback-token")
    );

    snapshot.restore(&path).unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), SOURCE_CONFIG);
}

#[test]
fn restore_keeps_external_edits_outside_the_managed_selector_and_model() {
    let target = Pair::new("http://127.0.0.1:43127/v1", "fixture-loopback-token");
    let original = config_with_model_fields(
        "api_backend = \"responses\"\nbase_url = \"https://upstream.example.invalid/v1\"\napi_key = \"fixture-original-key\"",
    );
    let snapshot = grok_config::Snapshot::new(&original, &target).unwrap();
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("config.toml");
    fs::write(&path, &original).unwrap();
    snapshot.apply(&path, None).unwrap();

    let mut external = fs::read_to_string(&path).unwrap();
    external.push_str("\n[operator]\nexternal_note = \"preserve this edit\"\n");
    fs::write(&path, &external).unwrap();
    assert!(snapshot.unchanged(&external).unwrap());

    snapshot.restore(&path).unwrap();
    let restored = fs::read_to_string(&path).unwrap();
    let restored_doc = restored.parse::<toml_edit::DocumentMut>().unwrap();
    assert_eq!(
        restored_doc
            .get("operator")
            .and_then(|operator| operator.get("external_note"))
            .and_then(|value| value.as_str()),
        Some("preserve this edit")
    );
    assert_eq!(
        restored_doc
            .get("models")
            .and_then(|models| models.get("default"))
            .and_then(|value| value.as_str()),
        Some("grok-4.5")
    );
    assert!(restored_doc
        .get("model")
        .and_then(|models| models.get(grok_config::MANAGED))
        .is_none());
    let restored_profile = grok_config::profile(&restored).unwrap();
    assert_eq!(
        restored_profile.pair.token.as_deref(),
        Some("fixture-original-key")
    );
}

#[test]
fn restore_reports_managed_external_edits_and_preserves_the_file() {
    let target = Pair::new("http://127.0.0.1:43127/v1", "fixture-loopback-token");
    let original = config_with_model_fields(
        "api_backend = \"responses\"\nbase_url = \"https://upstream.example.invalid/v1\"\napi_key = \"fixture-original-key\"",
    );
    let snapshot = grok_config::Snapshot::new(&original, &target).unwrap();
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("config.toml");
    fs::write(&path, &original).unwrap();
    snapshot.apply(&path, None).unwrap();

    let applied = fs::read_to_string(&path).unwrap();
    let edited = applied.replace("fixture-loopback-token", "fixture-external-token");
    assert_ne!(applied, edited);
    fs::write(&path, &edited).unwrap();

    let error = snapshot
        .restore(&path)
        .expect_err("managed edit must conflict");
    assert_eq!(error.code, "CONFLICT");
    assert_eq!(fs::read_to_string(&path).unwrap(), edited);
}

#[test]
fn missing_config_does_not_guess_a_native_model_or_credentials() {
    let temp = tempfile::tempdir().unwrap();
    let (revision, text) = grok_config::read(temp.path()).unwrap();

    assert_eq!(revision, "missing");
    assert!(text.is_empty());
    let profile = grok_config::profile_with_env(&text, &|_| None).unwrap();
    assert!(profile.model.is_empty());
    assert!(!profile.protocol_known);
    assert!(profile.pair.base_url.is_none());
    assert!(profile.pair.token.is_none());
}

#[test]
fn rejects_mtls_and_transport_owned_headers() {
    let mtls = config_with_model_fields("mtls_cert_dir = \"/fixtures/client-cert\"");
    let error = grok_config::profile(&mtls)
        .err()
        .expect("mTLS configuration must be rejected");
    assert_eq!(error.code, "GROK_CONFIG");

    for header in [
        "host",
        "content-length",
        "transfer-encoding",
        "connection",
        "proxy-authorization",
        "upgrade",
        "trailer",
        "te",
    ] {
        let config = format!(
            "[models]\ndefault = \"grok-4.5\"\n\n[model.\"grok-4.5\"]\nextra_headers = {{ {:?} = \"fixture-value\" }}\n",
            header
        );
        let error = grok_config::profile(&config)
            .err()
            .unwrap_or_else(|| panic!("transport-owned header must be rejected: {header}"));
        assert_eq!(error.code, "GROK_CONFIG", "{header}");
    }
}

#[test]
fn missing_grok_recovery_guides_managed_api_key_without_echoing_secrets() {
    let text = r#"
[models]
default = "official-model"

[model."official-model"]
model = "fixture-official-model"
base_url = "https://official.fixture.invalid/v1"
api_key = "fixture-official-key"

[model."lich13-switch"]
model = "fixture-managed-model"
api_base_url = "https://managed.fixture.invalid/v1"
api_key = "fixture-managed-api-key"
"#;

    let managed = grok_config::api_profile(text).unwrap();
    assert_eq!(managed.selected, grok_config::MANAGED);
    let message = grok_config::missing_recovery(text).message;

    assert!(message.contains("模型 lich13-switch"), "{message}");
    assert!(message.contains("api_base_url"), "{message}");
    assert!(message.contains("api_key"), "{message}");
    assert!(message.contains("当前选择的 Grok 目录"), "{message}");
    assert!(!message.contains("official-model"), "{message}");
    assert!(!message.contains("~/.grok"), "{message}");
    for private_value in [
        "https://official.fixture.invalid/v1",
        "fixture-official-key",
        "https://managed.fixture.invalid/v1",
        "fixture-managed-api-key",
    ] {
        assert!(
            !message.contains(private_value),
            "guidance must not echo {private_value:?}: {message}"
        );
    }
}

#[test]
fn missing_grok_recovery_guides_env_key_without_echoing_path_or_secrets() {
    let env_name = missing_environment_names(1)
        .into_iter()
        .next()
        .expect("a missing fixture environment variable should be available");
    let text = format!(
        "[models]\ndefault = \"official-model\"\n\n[model.\"official-model\"]\nmodel = \"fixture-official-model\"\nbase_url = \"https://official.fixture.invalid/v1\"\napi_key = \"fixture-official-key\"\n\n[model.\"lich13-switch\"]\nmodel = \"fixture-managed-model\"\nbase_url = \"https://managed-env.fixture.invalid/v1\"\nenv_key = {:?}\n",
        env_name
    );

    let managed = grok_config::api_profile(&text).unwrap();
    assert_eq!(managed.selected, grok_config::MANAGED);
    let message = grok_config::missing_recovery(&text).message;

    assert!(message.contains("模型 lich13-switch"), "{message}");
    assert!(message.contains("base_url"), "{message}");
    assert!(message.contains("env_key 引用的环境变量"), "{message}");
    assert!(message.contains("当前选择的 Grok 目录"), "{message}");
    assert!(!message.contains("official-model"), "{message}");
    assert!(!message.contains("~/.grok"), "{message}");
    for private_value in [
        "https://official.fixture.invalid/v1",
        "fixture-official-key",
        "https://managed-env.fixture.invalid/v1",
        env_name.as_str(),
    ] {
        assert!(
            !message.contains(private_value),
            "guidance must not echo {private_value:?}: {message}"
        );
    }
}

#[test]
fn resolves_global_models_base_url_for_sparse_selected_model() {
    let config = r#"
[models]
default = "fixture-model"

[model.fixture-model]
api_backend = "responses"
env_key = "XAI_API_KEY"

[endpoints]
models_base_url = "https://example.invalid/v1"

[auth]
preferred_method = "api_key"
"#;

    let profile = grok_config::profile_with_env(config, &|_| None)
        .expect("fixture config should parse without process environment values");

    assert_eq!(
        profile.pair.base_url.as_deref(),
        Some("https://example.invalid/v1")
    );
}
