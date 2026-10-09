use super::*;

fn setup() -> (tempfile::TempDir, Core) {
    let temp = tempfile::tempdir().unwrap();
    let codex = temp.path().join("codex");
    std::fs::create_dir(&codex).unwrap();
    let core = Core::new(temp.path().join("app"), codex).unwrap();
    (temp, core)
}

fn token(subject: &str, issued: Option<i64>) -> String {
    let mut claims = json!({"sub": subject});
    if let Some(issued) = issued {
        claims["iat"] = json!(issued);
    }
    format!("e30.{}.fixture", URL_SAFE_NO_PAD.encode(claims.to_string()))
}

fn oauth(
    subject: &str,
    marker: &str,
    access_issued: Option<i64>,
    id_issued: Option<i64>,
    refreshed: Value,
) -> String {
    json!({
        "auth_mode": "chatgpt",
        "tokens": {
            "account_id": "fixture-workspace",
            "access_token": token(subject, access_issued),
            "id_token": token(subject, id_issued),
            "refresh_token": format!("fixture-refresh-{marker}"),
            "future_token_field": {"keep": true}
        },
        "last_refresh": refreshed,
        "future": {"keep": ["fixture", 7, true]}
    })
    .to_string()
}

const CONFIG: &[u8] =
    b"# fixture config\r\nmodel = 'fixture-model'\r\n[future]\r\nkeep = [1, 2]\r\n";

#[test]
fn completed_same_identity_login_updates_current_file_and_clears_older_in_place() {
    let (_temp, mut core) = setup();
    let at = now() as i64;
    std::fs::write(core.home().join("config.toml"), CONFIG).unwrap();
    let before_id = core
        .import_raw(
            &oauth("fixture-before", "before", None, None, Value::Null),
            Some("Before".into()),
        )
        .unwrap();
    let old = oauth(
        "fixture-active",
        "old",
        Some(at - 600),
        Some(at - 600),
        json!(at - 600),
    );
    let active_id = core.import_raw(&old, Some("Custom name".into())).unwrap();
    let after_id = core
        .import_raw(
            &oauth("fixture-after", "after", None, None, Value::Null),
            Some("After".into()),
        )
        .unwrap();
    core.switch_account(&active_id, "missing").unwrap();

    let saved = oauth(
        "fixture-active",
        "saved",
        Some(at - 400),
        Some(at - 400),
        json!(at - 400),
    );
    assert_eq!(core.import_raw(&saved, None).unwrap(), active_id);
    assert_eq!(core.state().unwrap().auth_sync.unwrap().state, "older");

    let logged_in = oauth(
        "fixture-active",
        "completed-login",
        Some(at - 200),
        Some(at - 200),
        json!(at - 900),
    );
    assert!(core.complete_login(&logged_in).unwrap());

    let view = core.state().unwrap();
    let expected_order = vec![before_id, active_id.clone(), after_id];
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
        Some(active_id.as_str())
    );
    assert_eq!(core.store.profiles[1].auth, logged_in);
    assert_eq!(
        std::fs::read(core.home().join("auth.json")).unwrap(),
        logged_in.as_bytes()
    );
    assert_eq!(
        std::fs::read(core.home().join("config.toml")).unwrap(),
        CONFIG
    );
    assert_eq!(
        std::fs::read(core.data_dir.join("previous-auth.json")).unwrap(),
        old.as_bytes()
    );
    let saved_store: Store =
        serde_json::from_slice(&std::fs::read(core.data_dir.join("accounts.json")).unwrap())
            .unwrap();
    assert_eq!(saved_store.profiles[1].auth, logged_in);
    let parsed: Value = serde_json::from_str(&saved_store.profiles[1].auth).unwrap();
    assert_eq!(parsed["future"]["keep"], json!(["fixture", 7, true]));
    assert_eq!(parsed["tokens"]["future_token_field"]["keep"], true);

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
    assert!(reopened_view
        .auth_sync
        .as_ref()
        .is_none_or(|s| s.state != "older"));
    assert_eq!(reopened.store.profiles[1].auth, logged_in);
}

#[test]
fn completed_different_identity_login_saves_account_without_switching_current() {
    let (_temp, mut core) = setup();
    std::fs::write(core.home().join("config.toml"), CONFIG).unwrap();
    let current = oauth("fixture-active", "active", None, None, Value::Null);
    let active_id = core.import_raw(&current, Some("Active".into())).unwrap();
    let before = core.switch_account(&active_id, "missing").unwrap();
    let other = oauth("fixture-other", "new-login", None, None, Value::Null);

    assert!(!core.complete_login(&other).unwrap());

    let view = core.state().unwrap();
    assert_eq!(view.auth_revision, before.auth_revision);
    assert_eq!(view.accounts.len(), 2);
    assert_eq!(view.accounts[0].id, active_id);
    assert!(view.accounts[0].current);
    assert!(!view.accounts[1].current);
    assert_eq!(core.store.profiles[0].auth, current);
    assert_eq!(core.store.profiles[1].auth, other);
    assert_eq!(
        std::fs::read(core.home().join("auth.json")).unwrap(),
        current.as_bytes()
    );
    assert_eq!(
        std::fs::read(core.home().join("config.toml")).unwrap(),
        CONFIG
    );
    assert!(!core.data_dir.join("previous-auth.json").exists());
}

#[test]
fn observed_revision_with_newer_token_is_reprocessed_without_readding_deleted_identity() {
    let (_temp, mut core) = setup();
    let at = now() as i64;
    let saved = oauth(
        "fixture-active",
        "saved",
        Some(at - 400),
        Some(at - 400),
        json!(at - 100),
    );
    let id = core.import_raw(&saved, Some("Keep name".into())).unwrap();
    core.switch_account(&id, "missing").unwrap();

    let refreshed = oauth(
        "fixture-active",
        "external-refresh",
        Some(at - 200),
        Some(at - 300),
        json!(at - 900),
    );
    std::fs::write(core.home().join("auth.json"), &refreshed).unwrap();
    core.store.observed_auth_revisions.insert(
        core.preferences().codex_home,
        storage::digest(refreshed.as_bytes()),
    );
    core.set_sync(
        "older",
        Some(id.clone()),
        "fixture previously rejected revision",
    );
    core.persist().unwrap();

    let view = core.state().unwrap();
    assert_eq!(view.auth_sync.as_ref().unwrap().state, "updated");
    assert_eq!(view.accounts[0].id, id);
    assert_eq!(view.accounts[0].name, "Keep name");
    assert_eq!(core.store.profiles[0].auth, refreshed);
    let vault = std::fs::read(core.data_dir.join("accounts.json")).unwrap();
    assert_eq!(core.state().unwrap(), view);
    assert_eq!(
        std::fs::read(core.data_dir.join("accounts.json")).unwrap(),
        vault
    );

    core.delete(&id).unwrap();
    let deleted = core.state().unwrap();
    assert!(deleted.accounts.is_empty());
    assert_eq!(deleted.current_state, "unsaved");
    let mut reopened = Core::new(core.data_dir.clone(), core.home()).unwrap();
    assert!(reopened.state().unwrap().accounts.is_empty());
    assert_eq!(
        std::fs::read(core.home().join("auth.json")).unwrap(),
        refreshed.as_bytes()
    );
}

#[test]
fn genuinely_older_token_is_protected_even_with_newer_refresh_timestamp() {
    let (_temp, mut core) = setup();
    let at = now() as i64;
    let saved = oauth(
        "fixture-active",
        "newer-token",
        Some(at - 100),
        Some(at - 120),
        json!(at - 900),
    );
    let id = core.import_raw(&saved, Some("Keep name".into())).unwrap();
    core.switch_account(&id, "missing").unwrap();
    let incoming = oauth(
        "fixture-active",
        "older-token",
        Some(at - 300),
        Some(at - 400),
        json!(at - 10),
    );
    std::fs::write(core.home().join("auth.json"), &incoming).unwrap();

    let view = core.state().unwrap();
    assert_eq!(view.auth_sync.as_ref().unwrap().state, "older");
    assert_eq!(core.store.profiles[0].auth, saved);
    assert_eq!(
        std::fs::read(core.home().join("auth.json")).unwrap(),
        incoming.as_bytes()
    );
    let mut reopened = Core::new(core.data_dir.clone(), core.home()).unwrap();
    assert_eq!(reopened.state().unwrap().auth_sync.unwrap().state, "older");
    assert_eq!(reopened.store.profiles[0].auth, saved);
}

#[test]
fn token_order_uses_latest_iat_from_both_tokens_and_ignores_future_iat() {
    let at = now() as i64;
    let saved = oauth(
        "fixture-active",
        "saved",
        Some(at - 200),
        Some(at - 100),
        Value::Null,
    );
    let older_access = oauth(
        "fixture-active",
        "older-access",
        Some(at - 150),
        Some(at - 180),
        Value::Null,
    );
    assert!(older(&older_access, &saved));
    assert!(!token_is_newer(&older_access, &saved));
    let newer_access = oauth(
        "fixture-active",
        "newer-access",
        Some(at - 80),
        Some(at - 250),
        Value::Null,
    );
    assert!(!older(&newer_access, &saved));
    assert!(token_is_newer(&newer_access, &saved));

    for (access, id) in [(at + 3600, at - 300), (at - 300, at + 3600)] {
        let incoming = oauth(
            "fixture-active",
            "future-token",
            Some(access),
            Some(id),
            Value::Null,
        );
        assert!(older(&incoming, &saved));
        assert!(!token_is_newer(&incoming, &saved));
    }
}

#[test]
fn refresh_fallback_compares_seconds_milliseconds_and_rfc3339_consistently() {
    let at = now() as i64 - 1000;
    let rfc3339 = chrono::DateTime::from_timestamp(at, 0)
        .unwrap()
        .to_rfc3339();
    let versions = [json!(at), json!(at * 1000), json!(rfc3339)];
    for incoming in &versions {
        for stored in &versions {
            let incoming = json!({"last_refresh": incoming}).to_string();
            let stored = json!({"last_refresh": stored}).to_string();
            assert!(!older(&incoming, &stored));
            assert!(!older(&stored, &incoming));
        }
    }
    let seconds = json!({"last_refresh": at}).to_string();
    let newer_millisecond = json!({"last_refresh": at * 1000 + 1}).to_string();
    assert!(older(&seconds, &newer_millisecond));
    assert!(!older(&newer_millisecond, &seconds));

    let saved = oauth("fixture-active", "saved", Some(at), Some(at), json!(at));
    let without_iat = oauth(
        "fixture-active",
        "missing-iat",
        None,
        None,
        json!((at - 1) * 1000),
    );
    assert!(older(&without_iat, &saved));
    let same_iat_older_refresh = oauth(
        "fixture-active",
        "same-iat",
        Some(at),
        Some(at),
        json!(at - 1),
    );
    assert!(older(&same_iat_older_refresh, &saved));
}

#[test]
fn future_refresh_timestamps_do_not_make_usable_credentials_look_older() {
    let at = now() as i64;
    let normal = json!({"last_refresh": at - 100}).to_string();
    let future = at + 3600;
    let future_rfc3339 = chrono::DateTime::from_timestamp(future, 0)
        .unwrap()
        .to_rfc3339();
    for timestamp in [json!(future), json!(future * 1000), json!(future_rfc3339)] {
        let invalid = json!({"last_refresh": timestamp}).to_string();
        assert!(!older(&normal, &invalid));
        assert!(!older(&invalid, &normal));
    }
    let future_tokens = oauth(
        "fixture-active",
        "future-tokens",
        Some(future),
        Some(future),
        json!(at - 200),
    );
    assert!(older(&future_tokens, &normal));
}

#[test]
fn failed_vault_write_during_completed_login_preserves_current_auth_and_config() {
    let (_temp, mut core) = setup();
    std::fs::write(core.home().join("config.toml"), CONFIG).unwrap();
    let current = oauth("fixture-active", "current", None, None, Value::Null);
    let id = core.import_raw(&current, Some("Keep name".into())).unwrap();
    core.switch_account(&id, "missing").unwrap();
    let before_store = serde_json::to_value(&core.store).unwrap();
    let vault = core.data_dir.join("accounts.json");
    std::fs::remove_file(&vault).unwrap();
    std::fs::create_dir(&vault).unwrap();
    let logged_in = oauth("fixture-active", "completed-login", None, None, Value::Null);

    let error = core.complete_login(&logged_in).unwrap_err();

    assert_eq!(error.code, "FILE_TYPE");
    assert_eq!(serde_json::to_value(&core.store).unwrap(), before_store);
    assert!(vault.is_dir());
    assert_eq!(
        std::fs::read(core.home().join("auth.json")).unwrap(),
        current.as_bytes()
    );
    assert_eq!(
        std::fs::read(core.home().join("config.toml")).unwrap(),
        CONFIG
    );
    assert!(!core.data_dir.join("previous-auth.json").exists());
}
