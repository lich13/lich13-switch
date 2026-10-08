use super::*;
use serde_json::{json, Value};
use std::{fs, path::PathBuf};

struct Fixture {
    _root: tempfile::TempDir,
    data: PathBuf,
    user: PathBuf,
    home: PathBuf,
}

fn fixture() -> Fixture {
    let root = tempfile::tempdir().unwrap();
    let user = root.path().join("user");
    let home = user.join(".claude");
    let data = root.path().join("data");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&data).unwrap();
    Fixture {
        _root: root,
        data,
        user,
        home,
    }
}

fn file(f: &Fixture, role: Role) -> PathBuf {
    path(&f.home, &f.user, role)
}

fn current(f: &Fixture) -> View {
    view(&f.data, &f.home, &f.user).unwrap()
}

fn switch(f: &Fixture, mode: Mode, logged_in: bool) -> Result<View> {
    let expected = current(f).revision;
    super::switch(&f.data, &f.home, &f.user, mode, &expected, logged_in)
}

fn read(f: &Fixture, role: Role) -> Option<String> {
    fs::read_to_string(file(f, role)).ok()
}

fn parsed(f: &Fixture, role: Role) -> Value {
    serde_json::from_str(read(f, role).as_deref().unwrap()).unwrap()
}

#[test]
fn api_and_legacy_files_round_trip_and_missing_files_stay_missing() {
    let f = fixture();
    let settings = "{\r\n  \"env\": {\r\n    \"ANTHROPIC_BASE_URL\": \"https://api.example.invalid/v1\"\r\n  },\r\n  \"model\": \"fixture-model\"\r\n}\r\n";
    let legacy =
        "{\r\n  \"apiKey\": \"fixture-legacy-key\",\r\n  \"future\": {\"keep\": true}\r\n}\r\n";
    fs::write(file(&f, Role::Settings), settings).unwrap();
    fs::write(file(&f, Role::Legacy), legacy).unwrap();

    let official = switch(&f, Mode::Official, true).unwrap();
    assert_eq!(official.mode, Mode::Official);
    assert_eq!(
        read(&f, Role::Settings).as_deref(),
        Some("{\n  \"env\": {}\n}\n")
    );
    assert!(read(&f, Role::Legacy).is_none());

    let api = switch(&f, Mode::Api, true).unwrap();
    assert_eq!(api.mode, Mode::Api);
    assert_eq!(read(&f, Role::Settings).as_deref(), Some(settings));
    assert_eq!(read(&f, Role::Legacy).as_deref(), Some(legacy));

    let empty = fixture();
    fs::write(
        file(&empty, Role::Global),
        r#"{"hasCompletedOnboarding":false,"keep":"fixture"}"#,
    )
    .unwrap();
    switch(&empty, Mode::Official, false).unwrap();
    assert_eq!(
        read(&empty, Role::Settings).as_deref(),
        Some("{\n  \"env\": {}\n}\n")
    );
    assert!(read(&empty, Role::Legacy).is_none());
    assert!(parsed(&empty, Role::Global)["hasCompletedOnboarding"].is_null());
    assert_eq!(parsed(&empty, Role::Global)["keep"], "fixture");
    switch(&empty, Mode::Api, true).unwrap();
    assert!(read(&empty, Role::Settings).is_none());
    assert!(read(&empty, Role::Legacy).is_none());
}

#[test]
fn first_unauthenticated_initialization_removes_a_stale_onboarding_skip() {
    let f = fixture();
    fs::write(
        file(&f, Role::Global),
        r#"{"hasCompletedOnboarding":true,"keep":"fixture"}"#,
    )
    .unwrap();

    switch(&f, Mode::Official, false).unwrap();

    let global = parsed(&f, Role::Global);
    assert!(global.get("hasCompletedOnboarding").is_none());
    assert_eq!(global["keep"], "fixture");
}

#[test]
fn a_valid_oauth_account_preserves_onboarding_state_when_login_probe_is_false() {
    let f = fixture();
    fs::write(
        file(&f, Role::Global),
        r#"{"hasCompletedOnboarding":true,"oauthAccount":{"accountUuid":"fixture-account"}}"#,
    )
    .unwrap();

    switch(&f, Mode::Official, false).unwrap();

    let global = parsed(&f, Role::Global);
    assert_eq!(global["hasCompletedOnboarding"], true);
    assert_eq!(global["oauthAccount"]["accountUuid"], "fixture-account");
}

#[test]
fn profile_switch_changes_only_managed_fields_and_keeps_oauth_state_and_credentials() {
    let f = fixture();
    let plugin = "{\r\n  \"primaryApiKey\" : \"https://api.example.invalid/key\",\r\n  \"other\"  : {\"keep\": 0}\r\n}\r\n";
    let global = "{\r\n  \"customApiKeyResponses\" : {\"approved\": [\"fixture-approved\"]},\r\n  \"hasCompletedOnboarding\" : true,\r\n  \"oauthAccount\" : {\"accountUuid\": \"fixture-account-before\"},\r\n  \"mcpServers\" : {\"fixture-server\": {\"command\": \"fixture\"}},\r\n  \"projects\" : {\"fixture-project-id\": {\"hasTrustDialogAccepted\": true}}\r\n}\r\n";
    fs::write(file(&f, Role::Plugin), plugin).unwrap();
    fs::write(file(&f, Role::Global), global).unwrap();
    let credentials = f.home.join(".credentials.json");
    let credential_bytes = br#"{"fixture":"oauth-store-must-not-change"}"#;
    fs::write(&credentials, credential_bytes).unwrap();
    let credential_hash = storage::digest(credential_bytes);

    switch(&f, Mode::Official, true).unwrap();
    assert_eq!(
        read(&f, Role::Settings).as_deref(),
        Some("{\n  \"env\": {}\n}\n")
    );
    assert!(
        json::field(read(&f, Role::Plugin).as_deref().unwrap(), "primaryApiKey")
            .unwrap()
            .is_none()
    );
    assert!(json::field(
        read(&f, Role::Global).as_deref().unwrap(),
        "customApiKeyResponses"
    )
    .unwrap()
    .is_none());
    let official_global = parsed(&f, Role::Global);
    assert_eq!(official_global["hasCompletedOnboarding"], true);
    assert_eq!(
        official_global["oauthAccount"]["accountUuid"],
        "fixture-account-before"
    );
    assert_eq!(
        official_global["mcpServers"]["fixture-server"]["command"],
        "fixture"
    );
    assert_eq!(
        official_global["projects"]["fixture-project-id"]["hasTrustDialogAccepted"],
        true
    );

    // Model a later official login and trust update while the API fields remain absent.
    fs::write(
        file(&f, Role::Plugin),
        "{\r\n  \"other\"  : {\"keep\": 0, \"officialEdit\": true}\r\n}\r\n",
    )
    .unwrap();
    fs::write(
        file(&f, Role::Global),
        "{\r\n  \"hasCompletedOnboarding\" : true,\r\n  \"oauthAccount\" : {\"accountUuid\": \"fixture-account-after-login\"},\r\n  \"mcpServers\" : {\"fixture-server\": {\"command\": \"official-update\"}},\r\n  \"projects\" : {\"fixture-project-id\": {\"hasTrustDialogAccepted\": false}}\r\n}\r\n",
    )
    .unwrap();
    switch(&f, Mode::Api, true).unwrap();

    let restored_plugin = parsed(&f, Role::Plugin);
    assert_eq!(
        restored_plugin["primaryApiKey"],
        "https://api.example.invalid/key"
    );
    assert_eq!(restored_plugin["other"]["keep"], 0);
    assert_eq!(restored_plugin["other"]["officialEdit"], true);
    let restored_global = parsed(&f, Role::Global);
    assert_eq!(
        restored_global["customApiKeyResponses"],
        json!({"approved": ["fixture-approved"]})
    );
    assert_eq!(restored_global["hasCompletedOnboarding"], true);
    assert_eq!(
        restored_global["oauthAccount"]["accountUuid"],
        "fixture-account-after-login"
    );
    assert_eq!(
        restored_global["mcpServers"]["fixture-server"]["command"],
        "official-update"
    );
    assert_eq!(
        restored_global["projects"]["fixture-project-id"]["hasTrustDialogAccepted"],
        false
    );
    assert!(read(&f, Role::Plugin).unwrap().contains("\r\n"));
    assert!(read(&f, Role::Global).unwrap().contains("\r\n"));
    assert_eq!(
        storage::digest(&fs::read(credentials).unwrap()),
        credential_hash
    );
}

#[test]
fn api_field_values_round_trip_and_absent_fields_are_not_created() {
    let f = fixture();
    fs::write(
        file(&f, Role::Plugin),
        r#"{"primaryApiKey":"https://api.example.invalid/key","keep":false}"#,
    )
    .unwrap();
    fs::write(
        file(&f, Role::Global),
        r#"{"customApiKeyResponses":{"approved":["fixture-approved"],"rejected":["fixture-rejected"]},"keep":0}"#,
    )
    .unwrap();
    switch(&f, Mode::Official, true).unwrap();
    switch(&f, Mode::Api, true).unwrap();
    assert_eq!(
        parsed(&f, Role::Plugin)["primaryApiKey"],
        "https://api.example.invalid/key"
    );
    assert_eq!(
        parsed(&f, Role::Global)["customApiKeyResponses"],
        json!({"approved": ["fixture-approved"], "rejected": ["fixture-rejected"]}),
    );

    let f = fixture();
    fs::write(file(&f, Role::Plugin), r#"{"keep":false}"#).unwrap();
    fs::write(file(&f, Role::Global), r#"{"keep":0}"#).unwrap();
    switch(&f, Mode::Official, true).unwrap();
    switch(&f, Mode::Api, true).unwrap();
    assert!(
        json::field(read(&f, Role::Plugin).as_deref().unwrap(), "primaryApiKey")
            .unwrap()
            .is_none()
    );
    assert!(json::field(
        read(&f, Role::Global).as_deref().unwrap(),
        "customApiKeyResponses"
    )
    .unwrap()
    .is_none());
}

#[test]
fn invalid_managed_field_shapes_are_rejected_without_writes() {
    for (role, invalid) in [
        (Role::Plugin, r#"{"primaryApiKey":{"fixture":"value"}}"#),
        (Role::Plugin, r#"{"primaryApiKey":null}"#),
        (
            Role::Global,
            r#"{"customApiKeyResponses":{"approved":true}}"#,
        ),
        (
            Role::Global,
            r#"{"customApiKeyResponses":{"rejected":null}}"#,
        ),
        (
            Role::Global,
            r#"{"customApiKeyResponses":{"approved":["fixture",1]}}"#,
        ),
        (Role::Global, r#"{"customApiKeyResponses":null}"#),
    ] {
        let f = fixture();
        fs::write(file(&f, Role::Settings), r#"{"env":{},"keep":"fixture"}"#).unwrap();
        fs::write(file(&f, role), invalid).unwrap();
        let before: Vec<_> = [Role::Settings, Role::Legacy, Role::Plugin, Role::Global]
            .iter()
            .map(|role| (*role, read(&f, *role)))
            .collect();

        let error =
            super::switch(&f.data, &f.home, &f.user, Mode::Official, "unused", true).unwrap_err();

        assert_eq!(error.code, "CLAUDE_PROFILE");
        for (role, text) in before {
            assert_eq!(read(&f, role), text, "modified {role:?} for {invalid}");
        }
        assert!(!directory(&f.data).join(PROFILE).exists());
        assert!(!directory(&f.data).join(JOURNAL).exists());
    }
}

#[test]
fn stale_revision_and_invalid_json_or_root_types_are_rejected_without_writes() {
    let f = fixture();
    let before = current(&f).revision;
    fs::write(file(&f, Role::Settings), r#"{"model":"external-edit"}"#).unwrap();
    let error = super::switch(&f.data, &f.home, &f.user, Mode::Official, &before, true)
        .err()
        .unwrap();
    assert_eq!(error.code, "CLAUDE_PROFILE_CONFLICT");
    assert_eq!(
        read(&f, Role::Settings).as_deref(),
        Some(r#"{"model":"external-edit"}"#)
    );

    for (role, invalid) in [
        (Role::Settings, r#"{"env":{},"env":{}}"#),
        (Role::Legacy, "[]"),
        (Role::Plugin, r#"{"nested":{"x":1,"x":2}}"#),
        (Role::Global, "null"),
    ] {
        let f = fixture();
        fs::write(file(&f, role), invalid).unwrap();
        assert!(
            view(&f.data, &f.home, &f.user).is_err(),
            "accepted invalid {role:?} JSON"
        );
    }
}

#[test]
fn official_profile_guard_rejects_api_fields_and_allows_model_preferences() {
    let f = fixture();
    fs::write(
        file(&f, Role::Settings),
        r#"{"env":{"ANTHROPIC_AUTH_TOKEN":"fixture-api-token"},"model":"fixture-api-model"}"#,
    )
    .unwrap();
    switch(&f, Mode::Official, true).unwrap();
    let official_settings = read(&f, Role::Settings).unwrap();

    let api_field = r#"{"env":{"ANTHROPIC_BASE_URL":"https://api.example.invalid/v1"}}"#;
    let error = guard_save(&f.data, &f.home, &f.user, api_field).unwrap_err();
    assert_eq!(error.code, "CLAUDE_PROFILE_CONFLICT");

    let model_preference = r#"{"env":{},"model":"fixture-official-model"}"#;
    assert!(guard_save(&f.data, &f.home, &f.user, model_preference).is_ok());
    assert_eq!(
        read(&f, Role::Settings).as_deref(),
        Some(official_settings.as_str())
    );
}

#[test]
fn an_external_plugin_api_key_conflicts_with_an_official_profile() {
    let f = fixture();
    switch(&f, Mode::Official, true).unwrap();
    let official_settings = read(&f, Role::Settings).unwrap();
    let external_plugin = r#"{"primaryApiKey":"fixture-external-key","keep":true}"#;
    fs::write(file(&f, Role::Plugin), external_plugin).unwrap();

    let state = current(&f);
    assert!(state.conflict.is_some());
    let error = switch(&f, Mode::Api, true).unwrap_err();

    assert_eq!(error.code, "CLAUDE_PROFILE_CONFLICT");
    assert_eq!(read(&f, Role::Plugin).as_deref(), Some(external_plugin));
    assert_eq!(
        read(&f, Role::Settings).as_deref(),
        Some(official_settings.as_str())
    );
}

fn crash_fixture() -> (Fixture, Transaction, BTreeMap<Role, String>) {
    let f = fixture();
    let before = BTreeMap::from([
        (
            Role::Settings,
            r#"{"env":{"ANTHROPIC_BASE_URL":"https://api.example.invalid"}}"#.to_owned(),
        ),
        (
            Role::Legacy,
            r#"{"apiKey":"fixture-legacy-key"}"#.to_owned(),
        ),
        (
            Role::Plugin,
            r#"{"primaryApiKey":"fixture-api-key","keep":1}"#.to_owned(),
        ),
        (
            Role::Global,
            r#"{"customApiKeyResponses":{"approved":["fixture-approved"],"rejected":[]},"keep":2}"#
                .to_owned(),
        ),
    ]);
    let after_profile = serde_json::to_string(&Profile {
        version: 1,
        home: f.home.clone(),
        mode: Mode::Official,
        api_settings: Some(before[&Role::Settings].clone()),
        api_legacy: Some(before[&Role::Legacy].clone()),
        official_settings: "{\n  \"env\": {}\n}\n".into(),
        primary_key: Some(r#""fixture-api-key""#.into()),
        api_approvals: Some(r#"{"approved":["fixture-approved"],"rejected":[]}"#.into()),
    })
    .unwrap();
    let mut after = before.clone();
    after.insert(Role::Settings, "{\n  \"env\": {}\n}\n".into());
    after.remove(&Role::Legacy);
    after.insert(Role::Plugin, r#"{"keep":1}"#.into());
    after.insert(Role::Global, r#"{"keep":2}"#.into());
    let changes = before
        .iter()
        .map(|(role, text)| Change {
            role: *role,
            before: Some(text.clone()),
            after: after.get(role).cloned(),
        })
        .collect();
    let tx = Transaction {
        version: 1,
        home: f.home.clone(),
        before_profile: None,
        after_profile,
        changes,
    };
    (f, tx, before)
}

fn install_crash_state(f: &Fixture, tx: &Transaction, files: &BTreeMap<Role, String>) {
    for (role, text) in files {
        fs::write(file(f, *role), text).unwrap();
    }
    let after: BTreeMap<Role, Option<String>> = tx
        .changes
        .iter()
        .map(|c| (c.role, c.after.clone()))
        .collect();
    for (role, text) in after {
        match text {
            Some(text) => fs::write(file(f, role), text).unwrap(),
            None => {
                fs::remove_file(file(f, role)).unwrap();
            }
        }
    }
    fs::create_dir_all(directory(&f.data)).unwrap();
    fs::write(directory(&f.data).join(PROFILE), &tx.after_profile).unwrap();
    fs::write(
        directory(&f.data).join(JOURNAL),
        serde_json::to_vec(tx).unwrap(),
    )
    .unwrap();
}

#[test]
fn private_journal_recovers_all_files_after_a_multi_file_crash() {
    let (f, tx, before) = crash_fixture();
    install_crash_state(&f, &tx, &before);
    recover(&f.data, &f.home, &f.user).unwrap();
    for (role, text) in before {
        assert_eq!(read(&f, role).as_deref(), Some(text.as_str()), "{role:?}");
    }
    assert!(!directory(&f.data).join(PROFILE).exists());
    assert!(!directory(&f.data).join(JOURNAL).exists());
}

#[test]
fn journal_recovery_preserves_an_external_edit_and_keeps_the_recovery_record() {
    let (f, tx, before) = crash_fixture();
    install_crash_state(&f, &tx, &before);
    fs::write(file(&f, Role::Global), r#"{"external":"preserve-me"}"#).unwrap();

    let error = recover(&f.data, &f.home, &f.user).unwrap_err();
    assert_eq!(error.code, "CLAUDE_PROFILE_CONFLICT");
    assert_eq!(
        read(&f, Role::Global).as_deref(),
        Some(r#"{"external":"preserve-me"}"#)
    );
    assert!(directory(&f.data).join(JOURNAL).exists());

    for change in &tx.changes {
        if change.role != Role::Global {
            assert_eq!(
                read(&f, change.role),
                change.before,
                "partial rollback {:#?}",
                change.role
            );
        }
    }
    assert!(!directory(&f.data).join(PROFILE).exists());

    let global_after = tx
        .changes
        .iter()
        .find(|change| change.role == Role::Global)
        .and_then(|change| change.after.as_deref())
        .unwrap();
    fs::write(file(&f, Role::Global), global_after).unwrap();
    recover(&f.data, &f.home, &f.user).unwrap();
    for (role, text) in &before {
        assert_eq!(read(&f, *role).as_deref(), Some(text.as_str()), "{role:?}");
    }
    assert!(!directory(&f.data).join(PROFILE).exists());
    assert!(!directory(&f.data).join(JOURNAL).exists());

    recover(&f.data, &f.home, &f.user).unwrap();
    for (role, text) in &before {
        assert_eq!(
            read(&f, *role).as_deref(),
            Some(text.as_str()),
            "repeat recover {role:?}"
        );
    }
}
