//! Explicit auth/routing catalog, checked against CC Switch 5ae6ad38 live/floor.
//! This is not a CLAUDE_CODE_* prefix filter; unrelated preferences remain valid.
use serde_json::Value;
pub const ENV: &[&str] = &[
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_CUSTOM_HEADERS",
    "ANTHROPIC_MODEL",
    "ANTHROPIC_SMALL_FAST_MODEL",
    "ANTHROPIC_SMALL_FAST_MODEL_AWS_REGION",
    "ANTHROPIC_DEFAULT_OPUS_MODEL",
    "ANTHROPIC_DEFAULT_SONNET_MODEL",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL",
    "ANTHROPIC_DEFAULT_FABLE_MODEL",
    "ANTHROPIC_DEFAULT_OPUS_MODEL_DESCRIPTION",
    "ANTHROPIC_DEFAULT_SONNET_MODEL_DESCRIPTION",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL_DESCRIPTION",
    "ANTHROPIC_DEFAULT_OPUS_MODEL_NAME",
    "ANTHROPIC_DEFAULT_SONNET_MODEL_NAME",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL_NAME",
    "ANTHROPIC_VERTEX_PROJECT_ID",
    "ANTHROPIC_BEDROCK_BASE_URL",
    "ANTHROPIC_FOUNDRY_API_KEY",
    "ANTHROPIC_FOUNDRY_RESOURCE",
    "ANTHROPIC_FOUNDRY_BASE_URL",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
    "CLAUDE_CODE_SKIP_BEDROCK_AUTH",
    "CLAUDE_CODE_SKIP_VERTEX_AUTH",
    "CLAUDE_CODE_SKIP_FOUNDRY_AUTH",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "CLAUDE_CODE_OAUTH_TOKEN_FILE_DESCRIPTOR",
    "CLAUDE_CODE_API_KEY_FILE_DESCRIPTOR",
    "CLAUDE_CODE_MAX_OUTPUT_TOKENS",
    "CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS",
    "CLAUDE_CODE_DISABLE_1M_CONTEXT",
    "CLAUDE_CODE_SUBAGENT_MODEL",
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
    "AWS_BEARER_TOKEN_BEDROCK",
    "AWS_PROFILE",
    "AWS_REGION",
    "AWS_DEFAULT_REGION",
    "AWS_SHARED_CREDENTIALS_FILE",
    "AWS_CONFIG_FILE",
    "GOOGLE_APPLICATION_CREDENTIALS",
    "GOOGLE_CLOUD_PROJECT",
    "CLOUD_ML_REGION",
    "GOOGLE_CLOUD_REGION",
    "VERTEX_REGION_CLAUDE_3_5_HAIKU",
    "VERTEX_REGION_CLAUDE_3_5_SONNET",
    "VERTEX_REGION_CLAUDE_3_7_SONNET",
    "VERTEX_REGION_CLAUDE_4_0_OPUS",
    "VERTEX_REGION_CLAUDE_4_0_SONNET",
    "VERTEX_REGION_CLAUDE_4_1_OPUS",
];
pub const ROOT: &[&str] = &[
    "apiKeyHelper",
    "apiKey",
    "apiBaseUrl",
    "baseUrl",
    "model",
    "fallbackModel",
    "modelOverrides",
    "modelSettings",
    "awsAuthRefresh",
    "awsCredentialExport",
    "cloudAuthRefresh",
];
pub fn connection_fields(value: &Value) -> Vec<String> {
    let mut keys = ROOT
        .iter()
        .filter(|k| value.get(**k).is_some())
        .map(|k| k.to_string())
        .collect::<Vec<_>>();
    if let Some(env) = value.get("env").and_then(Value::as_object) {
        for key in ENV {
            if env.contains_key(*key) {
                keys.push(format!("env.{key}"));
            }
        }
    }
    keys
}
pub fn external() -> Vec<String> {
    ENV.iter()
        .filter(|k| std::env::var_os(k).is_some())
        .map(|k| format!("环境变量 {k}"))
        .collect()
}

pub fn official_preference(key: &str) -> bool {
    matches!(
        key,
        "model"
            | "fallbackModel"
            | "modelOverrides"
            | "modelSettings"
            | "env.CLAUDE_CODE_MAX_OUTPUT_TOKENS"
            | "env.CLAUDE_CODE_SUBAGENT_MODEL"
            | "env.CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS"
            | "env.CLAUDE_CODE_DISABLE_1M_CONTEXT"
    )
}
