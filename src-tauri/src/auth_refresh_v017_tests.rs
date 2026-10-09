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

fn token(subject: &str, marker: &str, issued: i64) -> String {
    let claims = json!({
        "sub": subject,
        "email": "fixture@example.invalid",
        "iat": issued,
        "jti": marker
    });
    format!("e30.{}.fixture", URL_SAFE_NO_PAD.encode(claims.to_string()))
}

fn oauth(subject: &str, workspace: &str, marker: &str, issued: i64) -> String {
    json!({
        "auth_mode": "chatgpt",
        "tokens": {
            "account_id": workspace,
            "access_token": token(subject, &format!("access-{marker}"), issued),
            "id_token": token(subject, &format!("id-{marker}"), issued),
            "refresh_token": format!("fixture-refresh-{marker}")
        },
        "last_refresh": issued
    })
    .to_string()
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
fn different_subject_workspace_or_auth_kind_does_not_replace_or_add_an_account() {
    let (_temp, mut core) = setup();
    let at = now() as i64 - 60;
    let original = oauth("fixture-target", WORKSPACE, "saved", at);
    let id = core.import_raw(&original, Some("Target".into())).unwrap();
    core.switch_account(&id, "missing").unwrap();
    let target = target_for(&mut core, &id);
    let before = snapshot(&core);

    for incoming in [
        oauth("fixture-other", WORKSPACE, "other-subject", at),
        oauth(
            "fixture-target",
            "fixture-other-workspace",
            "other-workspace",
            at,
        ),
        json!({"auth_mode": "apikey", "OPENAI_API_KEY": "fixture-api-value"}).to_string(),
    ] {
        let error = core.complete_target_login(&incoming, &target).unwrap_err();
        assert_eq!(error.code, "LOGIN_IDENTITY");
        assert_eq!(snapshot(&core), before);
    }
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
