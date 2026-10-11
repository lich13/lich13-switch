use super::*;

const CONFIG: &[u8] =
    b"# fixture config\r\nmodel = 'fixture-model'\r\n[future]\r\nkeep = [1, 2]\r\n";
const WORKSPACE: &str = "fixture-workspace";

fn setup() -> (tempfile::TempDir, Core) {
    let temp = tempfile::tempdir().unwrap();
    let codex = temp.path().join("codex");
    std::fs::create_dir(&codex).unwrap();
    let core = Core::new(temp.path().join("app"), codex).unwrap();
    std::fs::write(core.home().join("config.toml"), CONFIG).unwrap();
    (temp, core)
}

fn token_with_identity(
    subject: &str,
    email: &str,
    marker: &str,
    issued: i64,
    identity: Option<(&str, &str)>,
) -> String {
    let mut claims = json!({
        "sub": subject,
        "email": email,
        "iat": issued,
        "jti": marker
    });
    if let Some((field, value)) = identity {
        claims["https://api.openai.com/auth"][field] = json!(value);
    }
    format!("e30.{}.fixture", URL_SAFE_NO_PAD.encode(claims.to_string()))
}

fn token(subject: &str, marker: &str, issued: i64) -> String {
    token_with_identity(subject, "fixture@example.invalid", marker, issued, None)
}

fn oauth_with_tokens(
    id_token: String,
    access_token: String,
    workspace: Option<&str>,
    marker: &str,
    issued: i64,
) -> String {
    let mut tokens = json!({
        "access_token": access_token,
        "id_token": id_token,
        "refresh_token": format!("fixture-refresh-{marker}")
    });
    if let Some(workspace) = workspace {
        tokens["account_id"] = json!(workspace);
    }
    json!({
        "auth_mode": "chatgpt",
        "tokens": tokens,
        "last_refresh": issued
    })
    .to_string()
}

fn oauth(subject: &str, workspace: &str, marker: &str, issued: i64) -> String {
    oauth_with_tokens(
        token(subject, &format!("id-{marker}"), issued),
        token(subject, &format!("access-{marker}"), issued),
        Some(workspace),
        marker,
        issued,
    )
}

fn oauth_user(
    subject: &str,
    user_field: &str,
    user: &str,
    email: &str,
    workspace: Option<&str>,
    marker: &str,
    issued: i64,
) -> String {
    let identity = Some((user_field, user));
    oauth_with_tokens(
        token_with_identity(subject, email, &format!("id-{marker}"), issued, identity),
        token_with_identity(
            subject,
            email,
            &format!("access-{marker}"),
            issued,
            identity,
        ),
        workspace,
        marker,
        issued,
    )
}

fn profile<'a>(core: &'a Core, id: &str) -> &'a Profile {
    core.store.profiles.iter().find(|p| p.id == id).unwrap()
}

fn target_for(core: &mut Core, id: &str) -> LoginTarget {
    let view = core.state().unwrap();
    let account = view.accounts.iter().find(|a| a.id == id).unwrap();
    assert_eq!(
        account.credential_revision,
        storage::digest(profile(core, id).auth.as_bytes())
    );
    core.login_target(id, &account.credential_revision).unwrap()
}

#[derive(Debug, PartialEq)]
struct Snapshot {
    store: Value,
    vault: Option<Vec<u8>>,
    auth: Option<Vec<u8>>,
    config: Option<Vec<u8>>,
    previous_auth: Option<Vec<u8>>,
    sync: Option<AuthSync>,
    checked_auth: Option<(String, String)>,
}

fn snapshot(core: &Core) -> Snapshot {
    Snapshot {
        store: serde_json::to_value(&core.store).unwrap(),
        vault: storage::read_optional(&core.data_dir.join("accounts.json")).unwrap(),
        auth: storage::read_optional(&core.home().join("auth.json")).unwrap(),
        config: storage::read_optional(&core.home().join("config.toml")).unwrap(),
        previous_auth: storage::read_optional(&core.data_dir.join("previous-auth.json")).unwrap(),
        sync: core.auth_sync.clone(),
        checked_auth: core.checked_auth.clone(),
    }
}

#[test]
fn current_target_accepts_older_official_login_and_preserves_extensions_name_id_and_order() {
    let (_temp, mut core) = setup();
    let at = now() as i64;
    let before_id = core
        .import_raw(
            &oauth("fixture-before", WORKSPACE, "before", at - 60),
            Some("Before".into()),
        )
        .unwrap();
    let mut original: Value =
        serde_json::from_str(&oauth("fixture-target", WORKSPACE, "saved", at - 60)).unwrap();
    original["future"] = json!({"keep": ["fixture", 7, true], "nested": {"keep": "old"}});
    original["tokens"]["future_token_field"] = json!({"keep": true});
    let original = original.to_string();
    let id = core
        .import_raw(&original, Some("Custom name".into()))
        .unwrap();
    let after_id = core
        .import_raw(
            &oauth("fixture-after", WORKSPACE, "after", at - 60),
            Some("After".into()),
        )
        .unwrap();
    core.switch_account(&id, "missing").unwrap();
    let target = target_for(&mut core, &id);
    let old_revision = storage::digest(original.as_bytes());
    let mut incoming: Value = serde_json::from_str(&oauth(
        "fixture-target",
        WORKSPACE,
        "official-login",
        at - 3600,
    ))
    .unwrap();
    incoming["future"] = json!({"nested": {"added": "new"}});
    let incoming_raw = incoming.to_string();
    assert!(older(&incoming_raw, &original));

    assert!(core.complete_target_login(&incoming_raw, &target).unwrap());

    let view = core.state().unwrap();
    let expected_order = vec![before_id, id.clone(), after_id];
    assert_eq!(
        view.accounts
            .iter()
            .map(|a| a.id.clone())
            .collect::<Vec<_>>(),
        expected_order
    );
    assert_eq!(view.accounts[1].name, "Custom name");
    assert!(view.accounts[1].current);
    assert_eq!(view.current_state, "saved");
    assert_eq!(view.auth_sync.as_ref().unwrap().state, "updated");
    assert_eq!(
        view.auth_sync.as_ref().unwrap().account_id.as_deref(),
        Some(id.as_str())
    );
    let saved = &profile(&core, &id).auth;
    let merged: Value = serde_json::from_str(saved).unwrap();
    for key in ["account_id", "access_token", "id_token", "refresh_token"] {
        assert_eq!(merged["tokens"][key], incoming["tokens"][key]);
    }
    assert_eq!(merged["last_refresh"], incoming["last_refresh"]);
    assert_eq!(merged["future"]["keep"], json!(["fixture", 7, true]));
    assert_eq!(
        merged["future"]["nested"],
        json!({"keep": "old", "added": "new"})
    );
    assert_eq!(
        merged["tokens"]["future_token_field"],
        json!({"keep": true})
    );
    assert_ne!(view.accounts[1].credential_revision, old_revision);
    assert_eq!(
        view.accounts[1].credential_revision,
        storage::digest(saved.as_bytes())
    );
    assert_eq!(
        std::fs::read(core.home().join("auth.json")).unwrap(),
        saved.as_bytes()
    );
    assert_eq!(
        std::fs::read(core.home().join("config.toml")).unwrap(),
        CONFIG
    );
    let persisted: Store =
        serde_json::from_slice(&std::fs::read(core.data_dir.join("accounts.json")).unwrap())
            .unwrap();
    assert_eq!(persisted.profiles[1].auth, *saved);

    let mut reopened = Core::new(core.data_dir.clone(), core.home()).unwrap();
    let reopened_view = reopened.state().unwrap();
    assert_eq!(
        reopened_view
            .accounts
            .iter()
            .map(|a| a.id.clone())
            .collect::<Vec<_>>(),
        expected_order
    );
    assert_eq!(reopened_view.accounts[1].name, "Custom name");
    assert!(reopened_view.accounts[1].current);
    assert_eq!(profile(&reopened, &id).auth, *saved);
    assert!(reopened_view
        .auth_sync
        .as_ref()
        .is_none_or(|s| s.state != "older"));
}

#[test]
fn non_current_target_updates_only_its_saved_credentials_even_without_a_current_file() {
    for has_current in [false, true] {
        let (_temp, mut core) = setup();
        let at = now() as i64;
        let active_id = core
            .import_raw(
                &oauth("fixture-active", WORKSPACE, "active", at - 60),
                Some("Active".into()),
            )
            .unwrap();
        if has_current {
            core.switch_account(&active_id, "missing").unwrap();
        }
        let id = core
            .import_raw(
                &oauth("fixture-target", WORKSPACE, "saved", at - 60),
                Some("Keep target name".into()),
            )
            .unwrap();
        let target = target_for(&mut core, &id);
        let before = snapshot(&core);
        let incoming = oauth("fixture-target", WORKSPACE, "official-login", at - 3600);

        assert!(!core.complete_target_login(&incoming, &target).unwrap());

        let view = core.state().unwrap();
        let after = snapshot(&core);
        assert_eq!(after.auth, before.auth);
        assert_eq!(after.config, before.config);
        assert_eq!(after.previous_auth, before.previous_auth);
        assert_eq!(after.store["profiles"][0], before.store["profiles"][0]);
        assert_eq!(view.accounts.len(), 2);
        assert_eq!(view.accounts[0].id, active_id);
        assert_eq!(view.accounts[0].current, has_current);
        assert_eq!(view.accounts[1].id, id);
        assert_eq!(view.accounts[1].name, "Keep target name");
        assert!(!view.accounts[1].current);
        assert_eq!(
            serde_json::from_str::<Value>(&profile(&core, &id).auth).unwrap(),
            serde_json::from_str::<Value>(&incoming).unwrap()
        );
        let reopened = Core::new(core.data_dir.clone(), core.home()).unwrap();
        assert_eq!(profile(&reopened, &id).auth, profile(&core, &id).auth);
        assert_eq!(profile(&reopened, &id).name, "Keep target name");
    }
}

#[test]
fn starting_targeted_login_rejects_api_keys_deleted_ids_and_stale_account_revisions() {
    let (_temp, mut core) = setup();
    let raw = oauth("fixture-target", WORKSPACE, "saved", now() as i64 - 60);
    let id = core.import_raw(&raw, Some("Target".into())).unwrap();
    let api_id = core
        .add_api_key("Fixture API", "fixture-api-value")
        .unwrap();
    let api_revision = storage::digest(profile(&core, &api_id).auth.as_bytes());
    let before = snapshot(&core);

    for (account, revision, expected_error) in [
        ("fixture-deleted", "missing", "ACCOUNT"),
        (id.as_str(), "fixture-stale-revision", "CONFLICT"),
        (api_id.as_str(), api_revision.as_str(), "LOGIN_AUTH"),
    ] {
        let error = core
            .login_target(account, revision)
            .err()
            .expect("login must be rejected");
        assert_eq!(error.code, expected_error);
        assert_eq!(snapshot(&core), before);
    }
}

#[test]
fn different_subject_does_not_replace_or_add_an_account() {
    let (_temp, mut core) = setup();
    let at = now() as i64 - 60;
    let original = oauth("fixture-target", WORKSPACE, "saved", at);
    let id = core.import_raw(&original, Some("Target".into())).unwrap();
    core.switch_account(&id, "missing").unwrap();
    let target = target_for(&mut core, &id);
    let before = snapshot(&core);

    let error = core
        .complete_target_login(
            &oauth("fixture-other", WORKSPACE, "other-subject", at),
            &target,
        )
        .unwrap_err();
    assert_eq!(error.code, "LOGIN_IDENTITY");
    assert_eq!(snapshot(&core), before);
}

#[test]
fn api_key_login_does_not_replace_a_chatgpt_target() {
    let (_temp, mut core) = setup();
    let at = now() as i64 - 60;
    let id = core
        .import_raw(
            &oauth("fixture-target", WORKSPACE, "saved", at),
            Some("Target".into()),
        )
        .unwrap();
    core.switch_account(&id, "missing").unwrap();
    let target = target_for(&mut core, &id);
    let before = snapshot(&core);

    let error = core
        .complete_target_login(
            &json!({"auth_mode": "apikey", "OPENAI_API_KEY": "fixture-api-value"}).to_string(),
            &target,
        )
        .unwrap_err();

    assert_eq!(error.code, "LOGIN_IDENTITY");
    assert_eq!(snapshot(&core), before);
}

#[test]
fn same_chatgpt_user_can_refresh_into_a_different_workspace() {
    let (_temp, mut core) = setup();
    let at = now() as i64 - 60;
    let before_id = core
        .import_raw(
            &oauth_user(
                "fixture-before-subject",
                "chatgpt_user_id",
                "fixture-before-user",
                "fixture@example.invalid",
                Some("fixture-workspace-before"),
                "before",
                at,
            ),
            Some("Before".into()),
        )
        .unwrap();
    let mut original: Value = serde_json::from_str(&oauth_user(
        "fixture-target-subject",
        "chatgpt_user_id",
        "fixture-user",
        "fixture@example.invalid",
        Some("fixture-workspace-old"),
        "saved",
        at,
    ))
    .unwrap();
    original["future"] = json!({"keep": ["fixture", 7, true], "nested": {"keep": "old"}});
    original["tokens"]["future_token_field"] = json!({"keep": true});
    let original = original.to_string();
    let id = core
        .import_raw(&original, Some("Target name".into()))
        .unwrap();
    let after_id = core
        .import_raw(
            &oauth_user(
                "fixture-after-subject",
                "chatgpt_user_id",
                "fixture-after-user",
                "fixture@example.invalid",
                Some("fixture-workspace-after"),
                "after",
                at,
            ),
            Some("After".into()),
        )
        .unwrap();
    core.switch_account(&id, "missing").unwrap();
    let target = target_for(&mut core, &id);
    let mut incoming: Value = serde_json::from_str(&oauth_user(
        "fixture-target-subject",
        "chatgpt_user_id",
        "fixture-user",
        "fixture@example.invalid",
        Some("fixture-workspace-new"),
        "different-workspace",
        at - 3600,
    ))
    .unwrap();
    incoming["future"] = json!({"nested": {"added": "new"}});
    let incoming = incoming.to_string();

    assert!(core.complete_target_login(&incoming, &target).unwrap());

    let view = core.state().unwrap();
    let profile = profile(&core, &id);
    let saved: Value = serde_json::from_str(&profile.auth).unwrap();
    assert_eq!(
        view.accounts
            .iter()
            .map(|account| account.id.clone())
            .collect::<Vec<_>>(),
        vec![before_id, id.clone(), after_id]
    );
    assert_eq!(profile.name, "Target name");
    assert_eq!(saved["tokens"]["account_id"], "fixture-workspace-new");
    assert_eq!(
        profile.identity,
        storage::digest(b"chatgpt:fixture-target-subject:fixture-workspace-new")
    );
    assert_eq!(saved["future"]["keep"], json!(["fixture", 7, true]));
    assert_eq!(
        saved["future"]["nested"],
        json!({"keep": "old", "added": "new"})
    );
    assert_eq!(saved["tokens"]["future_token_field"], json!({"keep": true}));
}

#[test]
fn matching_subject_fallback_allows_a_workspace_change_without_user_id_claims() {
    let (_temp, mut core) = setup();
    let at = now() as i64 - 60;
    let id = core
        .import_raw(
            &oauth(
                "fixture-subject-fallback",
                "fixture-workspace-old",
                "saved",
                at,
            ),
            Some("Target".into()),
        )
        .unwrap();
    core.switch_account(&id, "missing").unwrap();
    let target = target_for(&mut core, &id);
    let incoming = oauth(
        "fixture-subject-fallback",
        "fixture-workspace-new",
        "workspace-change",
        at - 3600,
    );

    assert!(core.complete_target_login(&incoming, &target).unwrap());

    let saved: Value = serde_json::from_str(&profile(&core, &id).auth).unwrap();
    assert_eq!(saved["tokens"]["account_id"], "fixture-workspace-new");
    assert_eq!(
        profile(&core, &id).identity,
        storage::digest(b"chatgpt:fixture-subject-fallback:fixture-workspace-new")
    );
}

#[test]
fn same_user_id_allows_a_subject_change_and_user_id_is_the_fallback_identity_field() {
    let (_temp, mut core) = setup();
    let at = now() as i64 - 60;
    let id = core
        .import_raw(
            &oauth_user(
                "fixture-old-subject",
                "user_id",
                "fixture-user",
                "fixture@example.invalid",
                Some("fixture-workspace-old"),
                "saved",
                at,
            ),
            Some("Keep this name".into()),
        )
        .unwrap();
    core.switch_account(&id, "missing").unwrap();
    let target = target_for(&mut core, &id);
    let incoming = oauth_user(
        "fixture-new-subject",
        "user_id",
        "fixture-user",
        "fixture@example.invalid",
        Some("fixture-workspace-new"),
        "subject-rotated",
        at - 3600,
    );

    assert!(core.complete_target_login(&incoming, &target).unwrap());

    let profile = profile(&core, &id);
    assert_eq!(profile.name, "Keep this name");
    assert_eq!(
        profile.identity,
        storage::digest(b"chatgpt:fixture-new-subject:fixture-workspace-new")
    );
    assert_eq!(
        serde_json::from_str::<Value>(&profile.auth).unwrap()["tokens"]["account_id"],
        "fixture-workspace-new"
    );
}

#[test]
fn same_email_with_a_different_user_id_does_not_replace_the_target() {
    let (_temp, mut core) = setup();
    let at = now() as i64 - 60;
    let id = core
        .import_raw(
            &oauth_user(
                "fixture-target-subject",
                "chatgpt_user_id",
                "fixture-user-a",
                "fixture@example.invalid",
                Some(WORKSPACE),
                "saved",
                at,
            ),
            Some("Target".into()),
        )
        .unwrap();
    core.switch_account(&id, "missing").unwrap();
    let target = target_for(&mut core, &id);
    let before = snapshot(&core);
    let incoming = oauth_user(
        "fixture-other-subject",
        "chatgpt_user_id",
        "fixture-user-b",
        "fixture@example.invalid",
        Some(WORKSPACE),
        "other-user",
        at,
    );

    let error = core.complete_target_login(&incoming, &target).unwrap_err();

    assert_eq!(error.code, "LOGIN_IDENTITY");
    assert_eq!(snapshot(&core), before);
}

#[test]
fn conflicting_id_and_access_token_identities_are_rejected() {
    let (_temp, mut core) = setup();
    let at = now() as i64 - 60;
    let id = core
        .import_raw(
            &oauth_user(
                "fixture-target-subject",
                "chatgpt_user_id",
                "fixture-user",
                "fixture@example.invalid",
                Some(WORKSPACE),
                "saved",
                at,
            ),
            Some("Target".into()),
        )
        .unwrap();
    core.switch_account(&id, "missing").unwrap();
    let target = target_for(&mut core, &id);
    let before = snapshot(&core);
    let incoming = oauth_with_tokens(
        token_with_identity(
            "fixture-target-subject",
            "fixture@example.invalid",
            "id-conflict",
            at,
            Some(("chatgpt_user_id", "fixture-user")),
        ),
        token_with_identity(
            "fixture-target-subject",
            "fixture@example.invalid",
            "access-conflict",
            at,
            Some(("chatgpt_user_id", "fixture-other-user")),
        ),
        Some(WORKSPACE),
        "identity-conflict",
        at,
    );

    let error = core.complete_target_login(&incoming, &target).unwrap_err();

    assert_eq!(error.code, "LOGIN_IDENTITY");
    assert_eq!(snapshot(&core), before);
}

#[test]
fn targeted_refresh_replaces_managed_workspace_credentials_without_reusing_missing_fields() {
    let (_temp, mut core) = setup();
    let at = now() as i64 - 60;
    let mut original: Value = serde_json::from_str(&oauth_user(
        "fixture-target-subject",
        "chatgpt_user_id",
        "fixture-user",
        "fixture@example.invalid",
        Some("fixture-workspace-old"),
        "saved",
        at,
    ))
    .unwrap();
    original["OPENAI_API_KEY"] = json!("fixture-old-api-key");
    original["tokens"]["legacy_workspace"] = json!("fixture-old-extension");
    original["tokens"]["account_id"] = json!("fixture-workspace-old");
    let original_tokens = original["tokens"].clone();
    let original = original.to_string();
    let id = core.import_raw(&original, Some("Target".into())).unwrap();
    core.switch_account(&id, "missing").unwrap();
    let target = target_for(&mut core, &id);
    let mut incoming: Value = serde_json::from_str(&oauth_user(
        "fixture-target-subject",
        "chatgpt_user_id",
        "fixture-user",
        "fixture@example.invalid",
        Some("fixture-workspace-new"),
        "replacement",
        at - 3600,
    ))
    .unwrap();
    incoming["tokens"]
        .as_object_mut()
        .unwrap()
        .remove("account_id");
    incoming.as_object_mut().unwrap().remove("last_refresh");
    let incoming = incoming.to_string();

    assert!(core.complete_target_login(&incoming, &target).unwrap());

    let saved: Value = serde_json::from_str(&profile(&core, &id).auth).unwrap();
    assert!(saved.get("OPENAI_API_KEY").is_none());
    assert!(saved.get("last_refresh").is_none());
    assert!(saved["tokens"].get("account_id").is_none());
    assert_eq!(saved["tokens"]["legacy_workspace"], "fixture-old-extension");
    assert_eq!(
        saved["tokens"]["refresh_token"],
        "fixture-refresh-replacement"
    );
    assert_ne!(saved["tokens"]["id_token"], original_tokens["id_token"]);
    assert_ne!(
        saved["tokens"]["access_token"],
        original_tokens["access_token"]
    );
}

#[test]
fn refresh_rejects_an_identity_already_saved_under_another_profile() {
    let (_temp, mut core) = setup();
    let at = now() as i64 - 60;
    let target_id = core
        .import_raw(
            &oauth_user(
                "fixture-target-subject",
                "chatgpt_user_id",
                "fixture-user",
                "fixture@example.invalid",
                Some("fixture-workspace-target"),
                "target",
                at,
            ),
            Some("Target".into()),
        )
        .unwrap();
    let other_id = core
        .import_raw(
            &oauth_user(
                "fixture-other-subject",
                "chatgpt_user_id",
                "fixture-user",
                "fixture@example.invalid",
                Some("fixture-workspace-other"),
                "other",
                at,
            ),
            Some("Other".into()),
        )
        .unwrap();
    assert_ne!(target_id, other_id);
    core.switch_account(&target_id, "missing").unwrap();
    let target = target_for(&mut core, &target_id);
    let before = snapshot(&core);
    for (subject, marker) in [
        ("fixture-other-subject", "duplicate-exact"),
        ("fixture-new-subject", "duplicate-user-workspace"),
    ] {
        let incoming = oauth_user(
            subject,
            "chatgpt_user_id",
            "fixture-user",
            "fixture@example.invalid",
            Some("fixture-workspace-other"),
            marker,
            at,
        );
        let error = core.complete_target_login(&incoming, &target).unwrap_err();

        assert_eq!(error.code, "LOGIN_DUPLICATE");
        assert_eq!(snapshot(&core), before);
    }
    assert_eq!(profile(&core, &target_id).name, "Target");
    assert_eq!(profile(&core, &other_id).name, "Other");
}

#[test]
fn cross_workspace_refresh_survives_restart_without_adding_a_duplicate_account() {
    let (temp, mut core) = setup();
    let at = now() as i64 - 60;
    let id = core
        .import_raw(
            &oauth_user(
                "fixture-target-subject",
                "chatgpt_user_id",
                "fixture-user",
                "fixture@example.invalid",
                Some("fixture-workspace-before"),
                "saved",
                at,
            ),
            Some("Keep target".into()),
        )
        .unwrap();
    core.switch_account(&id, "missing").unwrap();
    let target = target_for(&mut core, &id);
    let incoming = oauth_user(
        "fixture-target-subject",
        "chatgpt_user_id",
        "fixture-user",
        "fixture@example.invalid",
        Some("fixture-workspace-after"),
        "after-workspace",
        at - 3600,
    );

    assert!(core.complete_target_login(&incoming, &target).unwrap());
    assert_eq!(core.store.profiles.len(), 1);

    let mut reopened = Core::new(temp.path().join("app"), core.home()).unwrap();
    let view = reopened.state().unwrap();
    assert_eq!(view.accounts.len(), 1);
    assert_eq!(view.accounts[0].id, id);
    assert_eq!(view.accounts[0].name, "Keep target");
    assert!(view.accounts[0].current);
    assert_eq!(profile(&reopened, &id).auth, profile(&core, &id).auth);
    assert_eq!(reopened.state().unwrap().accounts.len(), 1);
}

#[test]
fn invalid_official_credentials_leave_the_target_and_current_file_unchanged() {
    let (_temp, mut core) = setup();
    let original = oauth("fixture-target", WORKSPACE, "saved", now() as i64 - 60);
    let id = core.import_raw(&original, Some("Target".into())).unwrap();
    core.switch_account(&id, "missing").unwrap();
    let target = target_for(&mut core, &id);
    let before = snapshot(&core);
    let mut incomplete: Value = serde_json::from_str(&original).unwrap();
    incomplete["tokens"]
        .as_object_mut()
        .unwrap()
        .remove("refresh_token");

    for incoming in ["{fixture-invalid-json".to_owned(), incomplete.to_string()] {
        assert!(core.complete_target_login(&incoming, &target).is_err());
        assert_eq!(snapshot(&core), before);
    }
}

#[test]
fn deleting_the_selected_id_prevents_login_from_resurrecting_or_replacing_it() {
    for readd_identity in [false, true] {
        let (_temp, mut core) = setup();
        let at = now() as i64 - 60;
        let original = oauth("fixture-target", WORKSPACE, "saved", at);
        let id = core.import_raw(&original, Some("Target".into())).unwrap();
        core.switch_account(&id, "missing").unwrap();
        let target = target_for(&mut core, &id);
        core.delete(&id).unwrap();
        if readd_identity {
            let replacement = core.import_raw(&original, Some("Re-added".into())).unwrap();
            assert_ne!(replacement, id);
        }
        let before = snapshot(&core);
        let incoming = oauth("fixture-target", WORKSPACE, "official-login", at);

        let error = core.complete_target_login(&incoming, &target).unwrap_err();

        assert_eq!(error.code, "ACCOUNT");
        assert_eq!(snapshot(&core), before);
        let mut reopened = Core::new(core.data_dir.clone(), core.home()).unwrap();
        let view = reopened.state().unwrap();
        assert_eq!(view.accounts.len(), usize::from(readd_identity));
        assert!(view.accounts.iter().all(|a| a.id != id));
        assert_eq!(
            std::fs::read(core.home().join("auth.json")).unwrap(),
            original.as_bytes()
        );
    }
}

#[test]
fn a_second_completed_refresh_invalidates_the_first_login_snapshot() {
    for is_current in [false, true] {
        let (_temp, mut core) = setup();
        let at = now() as i64 - 60;
        let id = core
            .import_raw(
                &oauth("fixture-target", WORKSPACE, "saved", at),
                Some("Target".into()),
            )
            .unwrap();
        if is_current {
            core.switch_account(&id, "missing").unwrap();
        }
        let pending = target_for(&mut core, &id);
        let winner = target_for(&mut core, &id);
        let winning_login = oauth("fixture-target", WORKSPACE, "winner", at);
        assert_eq!(
            core.complete_target_login(&winning_login, &winner).unwrap(),
            is_current
        );
        let before = snapshot(&core);
        let delayed_login = oauth("fixture-target", WORKSPACE, "delayed", at);

        let error = core
            .complete_target_login(&delayed_login, &pending)
            .unwrap_err();

        assert_eq!(error.code, "CONFLICT");
        assert_eq!(snapshot(&core), before);
    }
}

#[test]
fn an_import_during_login_preserves_the_concurrently_saved_credentials() {
    let (_temp, mut core) = setup();
    let at = now() as i64 - 60;
    let id = core
        .import_raw(
            &oauth("fixture-target", WORKSPACE, "saved", at),
            Some("Target".into()),
        )
        .unwrap();
    let target = target_for(&mut core, &id);
    let imported = oauth("fixture-target", WORKSPACE, "concurrent-import", at);
    assert_eq!(core.import_raw(&imported, None).unwrap(), id);
    let before = snapshot(&core);

    let error = core
        .complete_target_login(
            &oauth("fixture-target", WORKSPACE, "late-login", at),
            &target,
        )
        .unwrap_err();

    assert_eq!(error.code, "CONFLICT");
    assert_eq!(snapshot(&core), before);
    assert_eq!(profile(&core, &id).auth, imported);
}

#[test]
fn current_file_changes_abort_both_current_and_non_current_target_refreshes() {
    for is_current in [false, true] {
        for remove_file in [false, true] {
            let (_temp, mut core) = setup();
            let at = now() as i64 - 60;
            let id = core
                .import_raw(
                    &oauth("fixture-target", WORKSPACE, "saved", at),
                    Some("Target".into()),
                )
                .unwrap();
            let active_subject = if is_current {
                "fixture-target"
            } else {
                "fixture-active"
            };
            let active_id = if is_current {
                id.clone()
            } else {
                core.import_raw(
                    &oauth(active_subject, WORKSPACE, "active", at),
                    Some("Active".into()),
                )
                .unwrap()
            };
            core.switch_account(&active_id, "missing").unwrap();
            let target = target_for(&mut core, &id);
            let auth_path = core.home().join("auth.json");
            if remove_file {
                std::fs::remove_file(&auth_path).unwrap();
            } else {
                std::fs::write(
                    &auth_path,
                    oauth(active_subject, WORKSPACE, "external-write", at),
                )
                .unwrap();
            }
            let before = snapshot(&core);

            let error = core
                .complete_target_login(
                    &oauth("fixture-target", WORKSPACE, "official-login", at),
                    &target,
                )
                .unwrap_err();

            assert_eq!(error.code, "CONFLICT");
            assert_eq!(snapshot(&core), before);
        }
    }
}

#[test]
fn a_current_file_created_after_login_started_is_preserved() {
    let (_temp, mut core) = setup();
    let at = now() as i64 - 60;
    let id = core
        .import_raw(
            &oauth("fixture-target", WORKSPACE, "saved", at),
            Some("Target".into()),
        )
        .unwrap();
    let target = target_for(&mut core, &id);
    std::fs::write(
        core.home().join("auth.json"),
        oauth("fixture-external", WORKSPACE, "external-write", at),
    )
    .unwrap();
    let before = snapshot(&core);

    let error = core
        .complete_target_login(
            &oauth("fixture-target", WORKSPACE, "official-login", at),
            &target,
        )
        .unwrap_err();

    assert_eq!(error.code, "CONFLICT");
    assert_eq!(snapshot(&core), before);
}

#[test]
fn renaming_during_login_preserves_the_latest_name_and_existing_account_id() {
    let (_temp, mut core) = setup();
    let at = now() as i64 - 60;
    let id = core
        .import_raw(
            &oauth("fixture-target", WORKSPACE, "saved", at),
            Some("Original name".into()),
        )
        .unwrap();
    let target = target_for(&mut core, &id);
    core.rename(&id, "Renamed during login").unwrap();

    assert!(!core
        .complete_target_login(
            &oauth("fixture-target", WORKSPACE, "official-login", at),
            &target
        )
        .unwrap());

    assert_eq!(core.store.profiles.len(), 1);
    assert_eq!(profile(&core, &id).id, id);
    assert_eq!(profile(&core, &id).name, "Renamed during login");
    let reopened = Core::new(core.data_dir.clone(), core.home()).unwrap();
    assert_eq!(profile(&reopened, &id).name, "Renamed during login");
}

#[test]
fn a_failed_vault_write_rolls_back_memory_and_current_auth_for_either_target_kind() {
    for is_current in [false, true] {
        let (_temp, mut core) = setup();
        let at = now() as i64 - 60;
        let original = oauth("fixture-target", WORKSPACE, "saved", at);
        let id = core
            .import_raw(&original, Some("Keep name".into()))
            .unwrap();
        if is_current {
            core.switch_account(&id, "missing").unwrap();
        }
        let target = target_for(&mut core, &id);
        let before = snapshot(&core);
        let vault = core.data_dir.join("accounts.json");
        std::fs::remove_file(&vault).unwrap();
        std::fs::create_dir(&vault).unwrap();
        let incoming = oauth("fixture-target", WORKSPACE, "official-login", at);

        let error = core.complete_target_login(&incoming, &target).unwrap_err();

        assert_eq!(error.code, "FILE_TYPE");
        assert!(vault.is_dir());
        assert_eq!(serde_json::to_value(&core.store).unwrap(), before.store);
        assert_eq!(
            storage::read_optional(&core.home().join("auth.json")).unwrap(),
            before.auth
        );
        assert_eq!(
            storage::read_optional(&core.home().join("config.toml")).unwrap(),
            before.config
        );
        assert_eq!(
            storage::read_optional(&core.data_dir.join("previous-auth.json")).unwrap(),
            before.previous_auth
        );
        assert_eq!(core.auth_sync, before.sync);
        assert_eq!(core.checked_auth, before.checked_auth);
        std::fs::remove_dir(&vault).unwrap();
        std::fs::write(&vault, before.vault.as_ref().unwrap()).unwrap();
        assert_eq!(snapshot(&core), before);
        let reopened = Core::new(core.data_dir.clone(), core.home()).unwrap();
        assert_eq!(profile(&reopened, &id).auth, original);
        assert_eq!(profile(&reopened, &id).name, "Keep name");
    }
}
