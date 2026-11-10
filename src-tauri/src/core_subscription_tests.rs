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

fn identity_claims(
    subject: &str,
    email: &str,
    user: Option<&str>,
    workspace: Option<&str>,
    plan: Option<&str>,
    issued: i64,
    marker: &str,
) -> serde_json::Value {
    let mut claims = json!({
        "sub": subject,
        "email": email,
        "iat": issued,
        "jti": marker
    });
    let mut auth = json!({});
    if let Some(user) = user {
        auth["chatgpt_user_id"] = json!(user);
    }
    if let Some(workspace) = workspace {
        auth["chatgpt_account_id"] = json!(workspace);
    }
    if let Some(plan) = plan {
        auth["chatgpt_plan_type"] = json!(plan);
    }
    claims["https://api.openai.com/auth"] = auth;
    claims
}

fn synthetic_token(claims: serde_json::Value) -> String {
    format!(
        "e30.{}.fixture-signature",
        URL_SAFE_NO_PAD.encode(claims.to_string())
    )
}

fn credential_from_claims(
    id_claims: serde_json::Value,
    access_claims: serde_json::Value,
    workspace: Option<&str>,
    marker: &str,
    last_refresh: i64,
) -> String {
    let mut tokens = json!({
        "id_token": synthetic_token(id_claims),
        "access_token": synthetic_token(access_claims),
        "refresh_token": format!("fixture-refresh-{marker}"),
        "fixture_token_field": "fixture-token-extension"
    });
    if let Some(workspace) = workspace {
        tokens["account_id"] = json!(workspace);
    }
    json!({
        "auth_mode": "chatgpt",
        "tokens": tokens,
        "last_refresh": last_refresh,
        "credential_marker": "fixture-credential-extension",
        "future": {"marker": "fixture-future-extension"}
    })
    .to_string()
}

fn identity_credential(
    subject: &str,
    email: &str,
    user: Option<&str>,
    workspace: Option<&str>,
    plan: Option<&str>,
    issued: i64,
    marker: &str,
) -> String {
    identity_credential_with_refresh(
        subject, email, user, workspace, plan, issued, marker, issued,
    )
}

#[allow(clippy::too_many_arguments)]
fn identity_credential_with_refresh(
    subject: &str,
    email: &str,
    user: Option<&str>,
    workspace: Option<&str>,
    plan: Option<&str>,
    issued: i64,
    marker: &str,
    last_refresh: i64,
) -> String {
    let claims = identity_claims(subject, email, user, workspace, plan, issued, marker);
    credential_from_claims(claims.clone(), claims, workspace, marker, last_refresh)
}

fn profile<'a>(core: &'a super::Core, id: &str) -> &'a super::Profile {
    core.store.profiles.iter().find(|p| p.id == id).unwrap()
}

fn setup_core() -> (tempfile::TempDir, super::Core) {
    let temp = tempfile::tempdir().unwrap();
    let codex = temp.path().join("codex");
    std::fs::create_dir(&codex).unwrap();
    let core = super::Core::new(temp.path().join("app"), codex).unwrap();
    (temp, core)
}

#[test]
fn rotated_subject_for_same_user_and_workspace_updates_the_existing_account() {
    let (_temp, mut core) = setup_core();
    let before = core
        .import_raw(
            &identity_credential(
                "fixture-sub-before",
                "before@example.invalid",
                Some("fixture-user-before"),
                Some("fixture-workspace-before"),
                Some("plus"),
                100,
                "before",
            ),
            Some("Before account".into()),
        )
        .unwrap();
    let original = identity_credential(
        "fixture-sub-old",
        "primary@example.invalid",
        Some("fixture-user-primary"),
        Some("fixture-workspace-primary"),
        Some("pro"),
        100,
        "old",
    );
    let primary = core
        .import_raw(&original, Some("Primary account".into()))
        .unwrap();
    let after = core
        .import_raw(
            &identity_credential(
                "fixture-sub-after",
                "after@example.invalid",
                Some("fixture-user-after"),
                Some("fixture-workspace-after"),
                Some("plus"),
                100,
                "after",
            ),
            Some("After account".into()),
        )
        .unwrap();
    let config = b"# fixture config\nmodel = 'fixture-model'\n";
    std::fs::write(core.home().join("config.toml"), config).unwrap();
    let auth_path = core.home().join("auth.json");
    std::fs::write(&auth_path, &original).unwrap();

    let initial = core.state().unwrap();
    let original_order: Vec<_> = initial.accounts.iter().map(|a| a.id.as_str()).collect();
    assert_eq!(
        original_order,
        vec![before.as_str(), primary.as_str(), after.as_str()]
    );
    assert_eq!(initial.accounts[1].name, "Primary account");

    let refreshed = identity_credential(
        "fixture-sub-rotated",
        "primary@example.invalid",
        Some("fixture-user-primary"),
        Some("fixture-workspace-primary"),
        Some("plus"),
        200,
        "rotated",
    );
    std::fs::write(&auth_path, &refreshed).unwrap();
    let updated = core.state().unwrap();

    let primary_accounts: Vec<_> = updated
        .accounts
        .iter()
        .filter(|account| account.email.as_deref() == Some("primary@example.invalid"))
        .collect();
    assert_eq!(
        primary_accounts.len(),
        1,
        "the refresh must not add a second account"
    );
    assert_eq!(updated.accounts.len(), 3);
    let updated_order: Vec<_> = updated.accounts.iter().map(|a| a.id.as_str()).collect();
    assert_eq!(updated_order, original_order);
    assert_eq!(updated.accounts[1].id, primary);
    assert_eq!(updated.accounts[1].name, "Primary account");
    assert_eq!(updated.accounts.iter().filter(|a| a.current).count(), 1);
    assert!(updated.accounts[1].current);
    assert_eq!(updated.current_state, "saved");
    assert_eq!(
        updated.accounts[1].subscription.as_ref().unwrap().label,
        "Plus"
    );
    assert_eq!(profile(&core, &primary).auth, refreshed);
    assert_eq!(std::fs::read(&auth_path).unwrap(), refreshed.as_bytes());
    assert_eq!(
        std::fs::read(core.home().join("config.toml")).unwrap(),
        config
    );
}

#[test]
fn user_and_workspace_both_participate_in_account_identity() {
    let (_temp, mut core) = setup_core();
    let first = core
        .import_raw(
            &identity_credential(
                "fixture-shared-subject",
                "shared@example.invalid",
                Some("fixture-user-one"),
                Some("fixture-workspace-one"),
                Some("plus"),
                100,
                "user-one",
            ),
            Some("User one".into()),
        )
        .unwrap();
    let other_user = core
        .import_raw(
            &identity_credential(
                "fixture-shared-subject",
                "shared@example.invalid",
                Some("fixture-user-two"),
                Some("fixture-workspace-one"),
                Some("pro"),
                200,
                "user-two",
            ),
            Some("User two".into()),
        )
        .unwrap();
    let other_workspace = core
        .import_raw(
            &identity_credential(
                "fixture-shared-subject",
                "workspace@example.invalid",
                Some("fixture-user-one"),
                Some("fixture-workspace-two"),
                Some("team"),
                300,
                "workspace-two",
            ),
            Some("Other workspace".into()),
        )
        .unwrap();

    assert_ne!(first, other_user);
    assert_ne!(first, other_workspace);
    assert_ne!(other_user, other_workspace);
    assert_eq!(core.store.profiles.len(), 3);
}

#[test]
fn accounts_with_the_same_email_but_different_users_remain_separate() {
    let (_temp, mut core) = setup_core();
    let first = core
        .import_raw(
            &identity_credential(
                "fixture-email-sub-one",
                "same@example.invalid",
                Some("fixture-email-user-one"),
                Some("fixture-email-workspace"),
                Some("plus"),
                100,
                "email-one",
            ),
            None,
        )
        .unwrap();
    let second = core
        .import_raw(
            &identity_credential(
                "fixture-email-sub-two",
                "same@example.invalid",
                Some("fixture-email-user-two"),
                Some("fixture-email-workspace"),
                Some("pro"),
                200,
                "email-two",
            ),
            None,
        )
        .unwrap();

    assert_ne!(first, second);
    assert_eq!(core.store.profiles.len(), 2);
}

#[test]
fn missing_user_id_falls_back_to_subject_and_does_not_merge_by_email() {
    let (_temp, mut core) = setup_core();
    let original = identity_credential(
        "fixture-fallback-subject",
        "before@example.invalid",
        None,
        Some("fixture-fallback-workspace"),
        Some("pro"),
        100,
        "fallback-old",
    );
    let id = core
        .import_raw(&original, Some("Fallback account".into()))
        .unwrap();
    let auth_path = core.home().join("auth.json");
    std::fs::write(&auth_path, &original).unwrap();
    assert!(core.state().unwrap().accounts[0].current);

    let refreshed = identity_credential(
        "fixture-fallback-subject",
        "after@example.invalid",
        None,
        Some("fixture-fallback-workspace"),
        Some("plus"),
        200,
        "fallback-new",
    );
    std::fs::write(&auth_path, &refreshed).unwrap();
    let updated = core.state().unwrap();
    assert_eq!(updated.accounts.len(), 1);
    assert_eq!(updated.accounts[0].id, id);
    assert_eq!(
        updated.accounts[0].email.as_deref(),
        Some("after@example.invalid")
    );
    assert!(updated.accounts[0].current);
    assert_eq!(profile(&core, &id).auth, refreshed);

    let other_subject = identity_credential(
        "fixture-other-fallback-subject",
        "after@example.invalid",
        None,
        Some("fixture-fallback-workspace"),
        Some("plus"),
        300,
        "fallback-other",
    );
    let other = core.import_raw(&other_subject, None).unwrap();
    assert_ne!(id, other);
    assert_eq!(core.state().unwrap().accounts.len(), 2);
}

#[test]
fn conflicting_current_token_users_use_auth_sync_invalid_without_replacing_the_vault() {
    let (_temp, mut core) = setup_core();
    let original = identity_credential(
        "fixture-conflict-subject",
        "conflict@example.invalid",
        Some("fixture-conflict-user"),
        Some("fixture-conflict-workspace"),
        Some("pro"),
        100,
        "conflict-original",
    );
    let id = core
        .import_raw(&original, Some("Conflict account".into()))
        .unwrap();
    let auth_path = core.home().join("auth.json");
    std::fs::write(&auth_path, &original).unwrap();
    core.state().unwrap();
    let vault_path = core.data_dir.join("accounts.json");
    let original_vault = std::fs::read(&vault_path).unwrap();
    let original_store = serde_json::to_value(&core.store).unwrap();
    let config = b"# conflict fixture config\nmodel = 'fixture-model'\n";
    std::fs::write(core.home().join("config.toml"), config).unwrap();

    let id_claims = identity_claims(
        "fixture-conflict-subject",
        "conflict@example.invalid",
        Some("fixture-conflict-user"),
        Some("fixture-conflict-workspace"),
        Some("plus"),
        200,
        "conflict-id-token",
    );
    let access_claims = identity_claims(
        "fixture-conflict-subject",
        "conflict@example.invalid",
        Some("fixture-other-conflict-user"),
        Some("fixture-conflict-workspace"),
        Some("plus"),
        200,
        "conflict-access-token",
    );
    let conflict = credential_from_claims(
        id_claims,
        access_claims,
        Some("fixture-conflict-workspace"),
        "identity-conflict",
        200,
    );
    std::fs::write(&auth_path, &conflict).unwrap();

    let state = core.state().unwrap();
    assert!(state.error.is_none());
    assert_eq!(state.current_state, "invalid");
    let sync = state.auth_sync.as_ref().unwrap();
    assert_eq!(sync.state, "invalid");
    assert!(sync.message.contains("auth.json 暂时无效"));
    assert_eq!(serde_json::to_value(&core.store).unwrap(), original_store);
    assert_eq!(std::fs::read(&vault_path).unwrap(), original_vault);
    assert_eq!(std::fs::read(&auth_path).unwrap(), conflict.as_bytes());
    assert_eq!(
        std::fs::read(core.home().join("config.toml")).unwrap(),
        config
    );
    assert_eq!(profile(&core, &id).auth, original);
}

#[test]
fn failed_state_persistence_keeps_the_in_memory_and_saved_account_data() {
    let (temp, mut core) = setup_core();
    let original = identity_credential(
        "fixture-storage-subject-old",
        "storage@example.invalid",
        Some("fixture-storage-user"),
        Some("fixture-storage-workspace"),
        Some("pro"),
        100,
        "storage-old",
    );
    let id = core
        .import_raw(&original, Some("Storage account".into()))
        .unwrap();
    let auth_path = core.home().join("auth.json");
    let config = b"# storage fixture config\nmodel = 'fixture-model'\n";
    std::fs::write(core.home().join("config.toml"), config).unwrap();
    std::fs::write(&auth_path, &original).unwrap();
    core.state().unwrap();
    let vault_path = core.data_dir.join("accounts.json");
    let original_vault = std::fs::read(&vault_path).unwrap();
    let original_store = serde_json::to_value(&core.store).unwrap();

    let refreshed = identity_credential(
        "fixture-storage-subject-new",
        "storage@example.invalid",
        Some("fixture-storage-user"),
        Some("fixture-storage-workspace"),
        Some("plus"),
        200,
        "storage-new",
    );
    std::fs::write(&auth_path, &refreshed).unwrap();
    let held_vault = temp.path().join("held-accounts.json");
    std::fs::rename(&vault_path, &held_vault).unwrap();
    std::fs::create_dir(&vault_path).unwrap();

    assert!(core.state().is_err());
    assert_eq!(serde_json::to_value(&core.store).unwrap(), original_store);
    assert_eq!(std::fs::read(&held_vault).unwrap(), original_vault);
    assert!(vault_path.is_dir());
    assert_eq!(std::fs::read(&auth_path).unwrap(), refreshed.as_bytes());
    assert_eq!(
        std::fs::read(core.home().join("config.toml")).unwrap(),
        config
    );
    assert_eq!(profile(&core, &id).auth, original);

    std::fs::remove_dir(&vault_path).unwrap();
    std::fs::rename(&held_vault, &vault_path).unwrap();
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

#[test]
fn core_state_caches_config_and_updates_credential_metadata_on_rotation_or_deletion() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let home = dir.path().join("codex-home");
    std::fs::create_dir_all(&home).unwrap();

    let auth_v1 = credential_from_claims(
        {
            let mut c = identity_claims(
                "sub-1",
                "user@example.invalid",
                Some("u-1"),
                Some("ws-1"),
                Some("plus"),
                1_700_000_000,
                "m1",
            );
            c["exp"] = json!(super::now() + 3600);
            c
        },
        identity_claims(
            "sub-1",
            "user@example.invalid",
            Some("u-1"),
            Some("ws-1"),
            Some("plus"),
            1_700_000_000,
            "m1a",
        ),
        Some("ws-1"),
        "m1",
        1_700_000_000,
    );
    std::fs::write(home.join("auth.json"), &auth_v1).unwrap();

    std::fs::write(
        home.join("config.toml"),
        "model_provider='openai'\ncli_auth_credentials_store='file'\n",
    )
    .unwrap();
    let mut core = super::Core::new(data, home.clone()).unwrap();
    let id = core.import_raw(&auth_v1, Some("Primary".into())).unwrap();
    let saved = core.state().unwrap();
    let rev_v1 = saved.accounts[0].credential_revision.clone();
    let cfg_rev_v1 = saved.config_revision.clone();
    assert_eq!(saved.auth_source.provider, "openai");
    assert!(!saved.auth_source.inline_token);
    assert_eq!(
        saved.accounts[0]
            .subscription
            .as_ref()
            .and_then(|s| s.plan_type.as_deref()),
        Some("plus")
    );
    assert_eq!(
        saved.accounts[0].subscription.as_ref().map(|s| s.stale),
        Some(false)
    );
    // Steady-state repeat poll hits config_cache and auth_cache (same Arc pointers) and returns identical revision and auth_source.
    let cfg_arc_v1 = core
        .config_cache
        .read(&home.join("config.toml"), |_| {
            panic!("expected config_cache hit")
        })
        .unwrap();
    let auth_arc_v1 = core
        .auth_cache
        .read(&home.join("auth.json"), |_| {
            panic!("expected auth_cache hit")
        })
        .unwrap();
    let cached_poll = core.state().unwrap();
    assert!(std::sync::Arc::ptr_eq(
        &cfg_arc_v1,
        &core
            .config_cache
            .read(&home.join("config.toml"), |_| {
                panic!("expected config_cache hit")
            })
            .unwrap()
    ));
    assert!(std::sync::Arc::ptr_eq(
        &auth_arc_v1,
        &core
            .auth_cache
            .read(&home.join("auth.json"), |_| {
                panic!("expected auth_cache hit")
            })
            .unwrap()
    ));
    assert_eq!(cached_poll.config_revision, cfg_rev_v1);
    assert_eq!(cached_poll.auth_source, saved.auth_source);

    // Modifying config.toml on disk invalidates config_cache immediately.
    crate::storage::atomic_write(
        &home.join("config.toml"),
        b"model_provider='custom'\n[model_providers.custom]\nexperimental_bearer_token='tok-fixture'\n",
        None,
    )
    .unwrap();
    let updated_cfg = core.state().unwrap();
    assert_ne!(updated_cfg.config_revision, cfg_rev_v1);
    assert_eq!(updated_cfg.auth_source.provider, "custom");
    assert!(updated_cfg.auth_source.inline_token);
    std::fs::remove_file(home.join("config.toml")).unwrap();
    let deleted_cfg = core.state().unwrap();
    assert_eq!(deleted_cfg.config_revision, "missing");
    assert_eq!(deleted_cfg.auth_source.provider, "openai");

    // Credential rotation updates revision & plan immediately without retaining duplicate auth cache.
    let auth_v2 = credential_from_claims(
        {
            let mut c = identity_claims(
                "sub-1",
                "user@example.invalid",
                Some("u-1"),
                Some("ws-1"),
                Some("pro"),
                1_700_000_100,
                "m2",
            );
            c["exp"] = json!(super::now() + 7200);
            c
        },
        identity_claims(
            "sub-1",
            "user@example.invalid",
            Some("u-1"),
            Some("ws-1"),
            Some("pro"),
            1_700_000_100,
            "m2a",
        ),
        Some("ws-1"),
        "m2",
        1_700_000_100,
    );
    std::fs::write(home.join("auth.json"), &auth_v2).unwrap();
    let rotated = core.state().unwrap();
    assert_ne!(rotated.accounts[0].credential_revision, rev_v1);
    assert_eq!(
        rotated.accounts[0]
            .subscription
            .as_ref()
            .and_then(|s| s.plan_type.as_deref()),
        Some("pro")
    );
    assert_eq!(
        rotated.accounts[0].subscription.as_ref().map(|s| s.stale),
        Some(false)
    );

    core.delete(&id).unwrap();
    let after_delete = core.state().unwrap();
    assert!(after_delete.accounts.is_empty());
}
