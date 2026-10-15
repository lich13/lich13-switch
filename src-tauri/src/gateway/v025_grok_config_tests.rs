use super::grok_config;

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
