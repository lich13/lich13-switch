// 覆盖官方套餐识别及刷新后的元数据。
//! v0.21 account subscription display metadata.
//!
//! The fixtures contain synthetic JWT payloads only.  These tests exercise the
//! pure decoder directly so they never write an auth file or call a network.
use super::account_subscription;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde_json::json;

fn id_token(auth: serde_json::Value, exp: Option<u64>) -> String {
    let mut claims = json!({"https://api.openai.com/auth": auth});
    if let Some(exp) = exp {
        claims["exp"] = exp.into();
    }
    format!(
        "e30.{}.fixture-signature",
        URL_SAFE_NO_PAD.encode(claims.to_string())
    )
}

fn chatgpt(auth: serde_json::Value, workspace: &str, exp: Option<u64>) -> String {
    json!({
        "auth_mode": "chatgpt",
        "tokens": {
            "account_id": workspace,
            "id_token": id_token(auth, exp),
            "access_token": "fixture-access-token",
            "refresh_token": "fixture-refresh-token"
        }
    })
    .to_string()
}

#[test]
fn matching_workspace_exposes_known_plan_and_expiry_only_as_stale_metadata() {
    let value = account_subscription(&chatgpt(
        json!({
            "chatgpt_account_id": "fixture-workspace",
            "chatgpt_plan_type": "plus"
        }),
        "fixture-workspace",
        Some(u64::MAX),
    ))
    .expect("chatgpt subscription metadata");
    assert_eq!(value.plan_type.as_deref(), Some("plus"));
    assert_eq!(value.label, "Plus");
    assert!(!value.stale);

    let expired = account_subscription(&chatgpt(
        json!({
            "chatgpt_account_id": "fixture-workspace",
            "chatgpt_plan_type": "pro"
        }),
        "fixture-workspace",
        Some(1),
    ))
    .unwrap();
    assert_eq!(expired.plan_type.as_deref(), Some("pro"));
    assert_eq!(expired.label, "Pro");
    assert!(expired.stale);
}

#[test]
fn workspace_mismatch_and_unknown_plan_never_fall_back_to_free() {
    let mismatch = account_subscription(&chatgpt(
        json!({
            "chatgpt_account_id": "other-workspace",
            "chatgpt_plan_type": "plus"
        }),
        "fixture-workspace",
        None,
    ))
    .unwrap();
    assert_eq!(mismatch.plan_type, None);
    assert_eq!(mismatch.label, "未识别");
    assert!(!mismatch.stale);

    let unknown = account_subscription(&chatgpt(
        json!({
            "chatgpt_account_id": "fixture-workspace",
            "chatgpt_plan_type": "fixture-custom"
        }),
        "fixture-workspace",
        None,
    ))
    .unwrap();
    assert_eq!(unknown.plan_type.as_deref(), Some("fixture-custom"));
    assert_eq!(unknown.label, "fixture-custom");
    assert_ne!(unknown.label, "Free");
}

#[test]
fn api_key_and_malformed_credentials_have_no_official_subscription() {
    for raw in [
        json!({
            "auth_mode": "apikey",
            "OPENAI_API_KEY": "fixture-api-key",
            "tokens": {"account_id": "fixture-workspace"}
        })
        .to_string(),
        json!({"OPENAI_API_KEY":"fixture-api-key"}).to_string(),
        "not-json".into(),
    ] {
        assert_eq!(account_subscription(&raw), None);
    }
}

#[test]
fn missing_or_unmatched_claims_are_read_only_unknown_metadata() {
    let no_plan = account_subscription(&chatgpt(
        json!({"chatgpt_account_id": "fixture-workspace"}),
        "fixture-workspace",
        None,
    ))
    .unwrap();
    assert_eq!(no_plan.plan_type, None);
    assert_eq!(no_plan.label, "未识别");

    let before = chatgpt(
        json!({
            "chatgpt_account_id": "fixture-workspace",
            "chatgpt_plan_type": "team"
        }),
        "fixture-workspace",
        None,
    );
    let copy = before.clone();
    let value = account_subscription(&before).unwrap();
    assert_eq!(value.label, "Team");
    assert_eq!(before, copy);
}
