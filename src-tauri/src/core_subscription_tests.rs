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

fn without_credential_times(raw: &str) -> String {
    let mut value: serde_json::Value = serde_json::from_str(raw).unwrap();
    value.as_object_mut().unwrap().remove("last_refresh");
    for field in ["id_token", "access_token"] {
        let token = value["tokens"][field].as_str().unwrap();
        let payload = token.split('.').nth(1).unwrap();
        let bytes = URL_SAFE_NO_PAD.decode(payload).unwrap();
        let mut claims: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        claims.as_object_mut().unwrap().remove("iat");
        value["tokens"][field] = json!(synthetic_token(claims));
    }
    value.to_string()
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

fn arm_account_normalization(core: &super::Core) {
    let vault = std::fs::read(core.data_dir.join("accounts.json")).unwrap();
    core.normalization_revision
        .replace(Some(super::storage::revision(Some(&vault))));
}

#[allow(clippy::too_many_arguments)]
fn legacy_profile(
    core: &mut super::Core,
    id: &str,
    name: &str,
    subject: &str,
    workspace: &str,
    email: &str,
    auth: &str,
    updated_at: u64,
) {
    core.store.profiles.push(super::Profile {
        id: id.into(),
        name: name.into(),
        identity: super::storage::digest(format!("chatgpt:{subject}:{workspace}").as_bytes()),
        kind: "chatgpt".into(),
        email: Some(email.into()),
        auth: auth.into(),
        updated_at,
        exchange: Default::default(),
        extensions: std::collections::BTreeMap::new(),
    });
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
fn legacy_duplicate_profiles_keep_first_identity_and_choose_newest_token() {
    let (temp, mut core) = setup_core();
    let home = core.home();
    let app = temp.path().join("app");
    let before_raw = identity_credential(
        "fixture-before-legacy-subject",
        "before@example.invalid",
        Some("fixture-before-legacy-user"),
        Some("fixture-before-legacy-workspace"),
        Some("plus"),
        100,
        "legacy-before",
    );
    let newest_raw = identity_credential_with_refresh(
        "fixture-legacy-subject-first",
        "newest@example.invalid",
        Some("fixture-legacy-user"),
        Some("fixture-legacy-workspace"),
        Some("pro"),
        300,
        "legacy-newest-token",
        100,
    );
    let middle_raw = identity_credential(
        "fixture-middle-legacy-subject",
        "middle@example.invalid",
        Some("fixture-middle-legacy-user"),
        Some("fixture-middle-legacy-workspace"),
        Some("plus"),
        100,
        "legacy-middle",
    );
    let older_duplicate_raw = identity_credential_with_refresh(
        "fixture-legacy-subject-second",
        "older@example.invalid",
        Some("fixture-legacy-user"),
        Some("fixture-legacy-workspace"),
        Some("team"),
        200,
        "legacy-older-token",
        400,
    );
    let after_raw = identity_credential(
        "fixture-after-legacy-subject",
        "after@example.invalid",
        Some("fixture-after-legacy-user"),
        Some("fixture-after-legacy-workspace"),
        Some("plus"),
        100,
        "legacy-after",
    );
    legacy_profile(
        &mut core,
        "fixture-legacy-before-id",
        "Before legacy account",
        "fixture-before-legacy-subject",
        "fixture-before-legacy-workspace",
        "before@example.invalid",
        &before_raw,
        10,
    );
    legacy_profile(
        &mut core,
        "fixture-legacy-first-id",
        "Keep this name",
        "fixture-legacy-subject-first",
        "fixture-legacy-workspace",
        "newest@example.invalid",
        &newest_raw,
        20,
    );
    legacy_profile(
        &mut core,
        "fixture-legacy-middle-id",
        "Middle legacy account",
        "fixture-middle-legacy-subject",
        "fixture-middle-legacy-workspace",
        "middle@example.invalid",
        &middle_raw,
        30,
    );
    legacy_profile(
        &mut core,
        "fixture-legacy-second-id",
        "Remove duplicate name",
        "fixture-legacy-subject-second",
        "fixture-legacy-workspace",
        "older@example.invalid",
        &older_duplicate_raw,
        40,
    );
    legacy_profile(
        &mut core,
        "fixture-legacy-after-id",
        "After legacy account",
        "fixture-after-legacy-subject",
        "fixture-after-legacy-workspace",
        "after@example.invalid",
        &after_raw,
        50,
    );
    core.persist().unwrap();
    let config = b"# legacy fixture config\nmodel = 'fixture-model'\n";
    std::fs::write(home.join("config.toml"), config).unwrap();
    let auth_path = home.join("auth.json");
    std::fs::write(&auth_path, &older_duplicate_raw).unwrap();
    drop(core);

    let mut reopened = super::Core::new(app.clone(), home.clone()).unwrap();
    let state = reopened.state().unwrap();
    let expected_order = vec![
        "fixture-legacy-before-id",
        "fixture-legacy-first-id",
        "fixture-legacy-middle-id",
        "fixture-legacy-after-id",
    ];
    let actual_order: Vec<_> = state.accounts.iter().map(|a| a.id.as_str()).collect();
    assert_eq!(actual_order, expected_order);
    assert_eq!(state.accounts[1].name, "Keep this name");
    assert_eq!(
        state.accounts[1].email.as_deref(),
        Some("newest@example.invalid")
    );
    assert!(state.accounts[1].current);
    assert_eq!(state.accounts.iter().filter(|a| a.current).count(), 1);
    assert_eq!(state.auth_sync.as_ref().unwrap().state, "older");
    assert_eq!(
        profile(&reopened, "fixture-legacy-first-id").auth,
        newest_raw
    );
    assert_eq!(reopened.store.profiles.len(), 4);
    assert_eq!(
        std::fs::read(&auth_path).unwrap(),
        older_duplicate_raw.as_bytes()
    );
    assert_eq!(std::fs::read(home.join("config.toml")).unwrap(), config);
    let normalized_vault = std::fs::read(app.join("accounts.json")).unwrap();

    drop(reopened);
    let mut second_start = super::Core::new(app.clone(), home).unwrap();
    let second_state = second_start.state().unwrap();
    let second_order: Vec<_> = second_state
        .accounts
        .iter()
        .map(|a| a.id.as_str())
        .collect();
    assert_eq!(second_order, expected_order);
    assert_eq!(
        second_state.accounts.iter().filter(|a| a.current).count(),
        1
    );
    assert_eq!(
        std::fs::read(app.join("accounts.json")).unwrap(),
        normalized_vault
    );
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
fn legacy_duplicate_extension_conflict_returns_state_error_and_preserves_the_vault() {
    let (_temp, mut core) = setup_core();
    let old = identity_credential(
        "fixture-legacy-extension-old-subject",
        "same@example.invalid",
        Some("fixture-legacy-extension-user"),
        Some("fixture-legacy-extension-workspace"),
        Some("plus"),
        100,
        "legacy-extension",
    );
    let mut incoming: serde_json::Value = serde_json::from_str(&identity_credential(
        "fixture-legacy-extension-new-subject",
        "same@example.invalid",
        Some("fixture-legacy-extension-user"),
        Some("fixture-legacy-extension-workspace"),
        Some("pro"),
        200,
        "legacy-extension",
    ))
    .unwrap();
    incoming["future"]["marker"] = json!("fixture-conflicting-extension");
    let incoming = incoming.to_string();
    legacy_profile(
        &mut core,
        "fixture-legacy-extension-old-id",
        "Old legacy row",
        "fixture-legacy-extension-old-subject",
        "fixture-legacy-extension-workspace",
        "same@example.invalid",
        &old,
        10,
    );
    legacy_profile(
        &mut core,
        "fixture-legacy-extension-new-id",
        "New legacy row",
        "fixture-legacy-extension-new-subject",
        "fixture-legacy-extension-workspace",
        "same@example.invalid",
        &incoming,
        20,
    );
    core.persist().unwrap();
    arm_account_normalization(&core);
    let vault_path = core.data_dir.join("accounts.json");
    let original_vault = std::fs::read(&vault_path).unwrap();

    let state = core.state().unwrap();

    assert!(state
        .error
        .as_deref()
        .is_some_and(|error| error.contains("无法安全合并")));
    assert_eq!(core.store.profiles.len(), 2);
    assert_eq!(std::fs::read(&vault_path).unwrap(), original_vault);
}

#[test]
fn legacy_duplicate_with_unknown_token_times_returns_state_error_and_preserves_the_vault() {
    let (_temp, mut core) = setup_core();
    let old = without_credential_times(&identity_credential(
        "fixture-legacy-time-subject",
        "same@example.invalid",
        Some("fixture-legacy-time-user"),
        Some("fixture-legacy-time-workspace"),
        Some("plus"),
        100,
        "legacy-time-old",
    ));
    let incoming = without_credential_times(&identity_credential(
        "fixture-legacy-time-subject",
        "same@example.invalid",
        Some("fixture-legacy-time-user"),
        Some("fixture-legacy-time-workspace"),
        Some("pro"),
        200,
        "legacy-time-new",
    ));
    legacy_profile(
        &mut core,
        "fixture-legacy-time-old-id",
        "Old legacy row",
        "fixture-legacy-time-subject",
        "fixture-legacy-time-workspace",
        "same@example.invalid",
        &old,
        10,
    );
    legacy_profile(
        &mut core,
        "fixture-legacy-time-new-id",
        "New legacy row",
        "fixture-legacy-time-subject",
        "fixture-legacy-time-workspace",
        "same@example.invalid",
        &incoming,
        20,
    );
    core.persist().unwrap();
    arm_account_normalization(&core);
    let vault_path = core.data_dir.join("accounts.json");
    let original_vault = std::fs::read(&vault_path).unwrap();

    let state = core.state().unwrap();

    assert!(state
        .error
        .as_deref()
        .is_some_and(|error| error.contains("新旧无法确认")));
    assert_eq!(core.store.profiles.len(), 2);
    assert_eq!(std::fs::read(&vault_path).unwrap(), original_vault);
}

#[test]
fn external_vault_change_rejects_pending_account_normalization_without_overwrite() {
    let (_temp, mut core) = setup_core();
    let old = identity_credential(
        "fixture-external-vault-old-subject",
        "same@example.invalid",
        Some("fixture-external-vault-user"),
        Some("fixture-external-vault-workspace"),
        Some("plus"),
        100,
        "external-vault-old",
    );
    let incoming = identity_credential(
        "fixture-external-vault-new-subject",
        "same@example.invalid",
        Some("fixture-external-vault-user"),
        Some("fixture-external-vault-workspace"),
        Some("pro"),
        200,
        "external-vault-new",
    );
    legacy_profile(
        &mut core,
        "fixture-external-vault-old-id",
        "Old legacy row",
        "fixture-external-vault-old-subject",
        "fixture-external-vault-workspace",
        "same@example.invalid",
        &old,
        10,
    );
    legacy_profile(
        &mut core,
        "fixture-external-vault-new-id",
        "New legacy row",
        "fixture-external-vault-new-subject",
        "fixture-external-vault-workspace",
        "same@example.invalid",
        &incoming,
        20,
    );
    core.persist().unwrap();
    arm_account_normalization(&core);
    let vault_path = core.data_dir.join("accounts.json");
    let original_vault = std::fs::read(&vault_path).unwrap();
    let mut external_vault = original_vault;
    external_vault.push(b' ');
    std::fs::write(&vault_path, &external_vault).unwrap();

    let state = core.state().unwrap();

    assert!(state.error.is_some());
    assert_eq!(core.store.profiles.len(), 2);
    assert_eq!(std::fs::read(&vault_path).unwrap(), external_vault);
}

#[test]
fn account_normalization_storage_failures_preserve_vault_and_retry_merge() {
    let points = [
        ("ShortWrite", super::storage::faults::Point::ShortWrite),
        ("FileSync", super::storage::faults::Point::FileSync),
        ("Replace", super::storage::faults::Point::Replace),
        (
            "DirectorySync",
            super::storage::faults::Point::DirectorySync,
        ),
    ];

    for (label, point) in points {
        let (_temp, mut core) = setup_core();
        let old = identity_credential(
            "fixture-normalize-fault-old-subject",
            "same@example.invalid",
            Some("fixture-normalize-fault-user"),
            Some("fixture-normalize-fault-workspace"),
            Some("plus"),
            100,
            "normalize-fault-old",
        );
        let incoming = identity_credential(
            "fixture-normalize-fault-new-subject",
            "same@example.invalid",
            Some("fixture-normalize-fault-user"),
            Some("fixture-normalize-fault-workspace"),
            Some("pro"),
            200,
            "normalize-fault-new",
        );
        legacy_profile(
            &mut core,
            "fixture-normalize-fault-retained-id",
            "Older legacy row",
            "fixture-normalize-fault-old-subject",
            "fixture-normalize-fault-workspace",
            "same@example.invalid",
            &old,
            10,
        );
        legacy_profile(
            &mut core,
            "fixture-normalize-fault-duplicate-id",
            "Newer legacy row",
            "fixture-normalize-fault-new-subject",
            "fixture-normalize-fault-workspace",
            "same@example.invalid",
            &incoming,
            20,
        );
        core.persist().unwrap();
        arm_account_normalization(&core);
        let vault_path = core.data_dir.join("accounts.json");
        let original_vault = std::fs::read(&vault_path).unwrap();

        let failed = {
            let _fault = super::storage::faults::inject(point);
            core.state().unwrap()
        };

        assert!(failed.error.is_some(), "{label}");
        assert_eq!(core.store.profiles.len(), 2, "{label}");
        assert_eq!(
            std::fs::read(&vault_path).unwrap(),
            original_vault,
            "{label}"
        );

        let retried = core.state().unwrap();
        assert!(retried.error.is_none(), "{label}");
        assert_eq!(core.store.profiles.len(), 1, "{label}");
        assert_eq!(
            core.store.profiles[0].id, "fixture-normalize-fault-retained-id",
            "{label}"
        );
        let merged: serde_json::Value = serde_json::from_str(&core.store.profiles[0].auth).unwrap();
        let expected: serde_json::Value = serde_json::from_str(&incoming).unwrap();
        assert_eq!(
            merged["tokens"]["id_token"], expected["tokens"]["id_token"],
            "{label}"
        );
        assert_ne!(
            std::fs::read(&vault_path).unwrap(),
            original_vault,
            "{label}"
        );
    }
}

#[test]
fn legacy_duplicate_merge_remaps_the_official_mode_account_reference() {
    let (_temp, mut core) = setup_core();
    let home = core.home();
    let config = b"model_provider = 'custom'\n[model_providers.custom]\n";
    std::fs::write(home.join("config.toml"), config).unwrap();
    let official_record = crate::official::enable(
        &core.data_dir,
        &home,
        "fixture-official-duplicate-id",
        &super::storage::revision(Some(config)),
    )
    .unwrap();
    assert!(official_record.enabled);
    assert_eq!(
        official_record.account_id.as_deref(),
        Some("fixture-official-duplicate-id")
    );

    let old = identity_credential(
        "fixture-official-old-subject",
        "official@example.invalid",
        Some("fixture-official-user"),
        Some("fixture-official-workspace"),
        Some("plus"),
        100,
        "official-old",
    );
    let incoming = identity_credential(
        "fixture-official-new-subject",
        "official@example.invalid",
        Some("fixture-official-user"),
        Some("fixture-official-workspace"),
        Some("pro"),
        200,
        "official-new",
    );
    legacy_profile(
        &mut core,
        "fixture-official-retained-id",
        "Retained account",
        "fixture-official-old-subject",
        "fixture-official-workspace",
        "official@example.invalid",
        &old,
        10,
    );
    legacy_profile(
        &mut core,
        "fixture-official-duplicate-id",
        "Duplicate account",
        "fixture-official-new-subject",
        "fixture-official-workspace",
        "official@example.invalid",
        &incoming,
        20,
    );
    core.persist().unwrap();
    arm_account_normalization(&core);

    let state = core.state().unwrap();

    assert!(state.error.is_none());
    assert_eq!(core.store.profiles.len(), 1);
    assert_eq!(core.store.profiles[0].id, "fixture-official-retained-id");
    let official_after = crate::official::view(&core.data_dir, &home);
    assert!(official_after.enabled);
    assert_eq!(
        official_after.account_id.as_deref(),
        Some("fixture-official-retained-id")
    );
    assert!(official_after.error.is_none());
}

#[test]
fn official_account_remap_directory_sync_failure_keeps_rows_and_retries() {
    let (_temp, mut core) = setup_core();
    let home = core.home();
    let config = b"model_provider = 'custom'\n[model_providers.custom]\n";
    std::fs::write(home.join("config.toml"), config).unwrap();
    let official_record = crate::official::enable(
        &core.data_dir,
        &home,
        "fixture-official-fault-duplicate-id",
        &super::storage::revision(Some(config)),
    )
    .unwrap();
    assert!(official_record.enabled);

    let old = identity_credential(
        "fixture-official-fault-old-subject",
        "official@example.invalid",
        Some("fixture-official-fault-user"),
        Some("fixture-official-fault-workspace"),
        Some("plus"),
        100,
        "official-fault-old",
    );
    let incoming = identity_credential(
        "fixture-official-fault-new-subject",
        "official@example.invalid",
        Some("fixture-official-fault-user"),
        Some("fixture-official-fault-workspace"),
        Some("pro"),
        200,
        "official-fault-new",
    );
    legacy_profile(
        &mut core,
        "fixture-official-fault-retained-id",
        "Retained account",
        "fixture-official-fault-old-subject",
        "fixture-official-fault-workspace",
        "official@example.invalid",
        &old,
        10,
    );
    legacy_profile(
        &mut core,
        "fixture-official-fault-duplicate-id",
        "Duplicate account",
        "fixture-official-fault-new-subject",
        "fixture-official-fault-workspace",
        "official@example.invalid",
        &incoming,
        20,
    );
    core.persist().unwrap();
    arm_account_normalization(&core);
    let vault_path = core.data_dir.join("accounts.json");
    let original_vault = std::fs::read(&vault_path).unwrap();

    let failed = {
        let _fault = super::storage::faults::inject(super::storage::faults::Point::DirectorySync);
        core.state().unwrap()
    };

    assert!(failed.error.is_some());
    assert_eq!(core.store.profiles.len(), 2);
    assert_eq!(std::fs::read(&vault_path).unwrap(), original_vault);
    let official_after_failure = crate::official::view(&core.data_dir, &home);
    assert!(official_after_failure.enabled);
    assert_eq!(
        official_after_failure.account_id.as_deref(),
        Some("fixture-official-fault-duplicate-id")
    );
    assert!(official_after_failure.error.is_none());

    let retried = core.state().unwrap();
    assert!(retried.error.is_none());
    assert_eq!(core.store.profiles.len(), 1);
    assert_eq!(
        core.store.profiles[0].id,
        "fixture-official-fault-retained-id"
    );
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
fn core_state_caches_credential_metadata_and_invalidates_on_rotation_or_deletion() {
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

    let mut core = super::Core::new(data, home.clone()).unwrap();
    let id = core.import_raw(&auth_v1, Some("Primary".into())).unwrap();
    let saved = core.state().unwrap();
    let rev_v1 = saved.accounts[0].credential_revision.clone();
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
    assert_eq!(core.profile_meta_cache.len(), 1);

    // Expiration staleness is dynamically recomputed from cached exp without re-decoding JWT.
    core.profile_meta_cache
        .get_mut(&id)
        .unwrap()
        .subscription
        .as_mut()
        .unwrap()
        .exp = Some(super::now().saturating_sub(1));
    let expired_state = core.state().unwrap();
    assert_eq!(
        expired_state.accounts[0]
            .subscription
            .as_ref()
            .map(|s| s.stale),
        Some(true)
    );

    // Credential rotation invalidates the cached revision & plan immediately.
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

    // Deleting the profile prunes its cached metadata entry on the next state() call.
    core.delete(&id).unwrap();
    let after_delete = core.state().unwrap();
    assert!(after_delete.accounts.is_empty());
    assert!(core.profile_meta_cache.is_empty());
}
