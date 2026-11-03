use super::*;
use std::sync::Mutex;
use tempfile::TempDir;

struct MockLifecycle {
    running: Mutex<bool>,
    events: Mutex<Vec<&'static str>>,
    fail_quit: bool,
    fail_launch: bool,
}

impl MockLifecycle {
    fn new(running: bool) -> Self {
        Self {
            running: Mutex::new(running),
            events: Mutex::new(Vec::new()),
            fail_quit: false,
            fail_launch: false,
        }
    }

    fn events(&self) -> Vec<&'static str> {
        self.events.lock().unwrap().clone()
    }
}

impl AppLifecycle for MockLifecycle {
    fn is_running(&self) -> Result<bool> {
        Ok(*self.running.lock().unwrap())
    }

    fn quit(&self) -> Result<()> {
        drop(AUTH_LOCK.lock().unwrap_or_else(|e| e.into_inner()));
        self.events.lock().unwrap().push("quit");
        if self.fail_quit {
            return Err(AppError::new(code::PROCESS, "模拟退出 Antigravity 失败"));
        }
        *self.running.lock().unwrap() = false;
        Ok(())
    }

    fn launch(&self) -> Result<()> {
        drop(AUTH_LOCK.lock().unwrap_or_else(|e| e.into_inner()));
        self.events.lock().unwrap().push("launch");
        if self.fail_launch {
            return Err(AppError::new(code::PROCESS, "模拟重启 Antigravity 失败"));
        }
        *self.running.lock().unwrap() = true;
        Ok(())
    }
}

fn setup_paths() -> (TempDir, Paths) {
    let tmp = TempDir::new().expect("tempdir");
    let user_home = tmp.path().join("home");
    let vela_data = tmp.path().join("vela");
    fs::create_dir_all(user_home.join(".gemini/config")).unwrap();
    fs::create_dir_all(user_home.join(".config/gcloud")).unwrap();
    fs::create_dir_all(user_home.join(".config/agswitch")).unwrap();
    fs::create_dir_all(&vela_data).unwrap();
    let paths = Paths::from_roots(&user_home, &vela_data);
    (tmp, paths)
}

#[test]
fn oauth_auto_recognizes_disk_login_and_deduplicates_on_token_refresh() {
    let (_tmp, paths) = setup_paths();
    let oauth_v1 = serde_json::json!({
        "auth_method": "google-oauth",
        "user": { "email": "user1@example.invalid" },
        "token": { "access_token": "ya29.v1", "refresh_token": "1//r1" }
    });
    write_private_file(
        &paths.oauth_file(),
        serde_json::to_string_pretty(&oauth_v1).unwrap().as_bytes(),
    )
    .unwrap();

    let s1 = state_at(&paths).unwrap();
    assert_eq!(s1.accounts.len(), 1);
    assert!(s1.accounts[0].current);
    assert_eq!(
        s1.accounts[0].email.as_deref(),
        Some("user1@example.invalid")
    );
    let rev1 = s1.accounts[0].credential_revision.clone();

    let oauth_v2 = serde_json::json!({
        "auth_method": "google-oauth",
        "user": { "email": "User1@Example.Invalid" },
        "token": { "access_token": "ya29.v2-refreshed", "refresh_token": "1//r1" }
    });
    write_private_file(
        &paths.oauth_file(),
        serde_json::to_string_pretty(&oauth_v2).unwrap().as_bytes(),
    )
    .unwrap();

    let s2 = state_at(&paths).unwrap();
    assert_eq!(s2.accounts.len(), 1);
    assert!(s2.accounts[0].current);
    assert_ne!(s2.accounts[0].credential_revision, rev1);

    let oauth_second = serde_json::json!({
        "auth_method": "jetski-sso",
        "email": "user2@example.invalid",
        "access_token": "ya29.user2"
    });
    write_private_file(
        &paths.oauth_file(),
        serde_json::to_string_pretty(&oauth_second)
            .unwrap()
            .as_bytes(),
    )
    .unwrap();

    let s3 = state_at(&paths).unwrap();
    assert_eq!(s3.accounts.len(), 2);
    assert_eq!(s3.accounts.iter().filter(|a| a.current).count(), 1);
    assert!(s3
        .accounts
        .iter()
        .any(|a| a.email.as_deref() == Some("user2@example.invalid") && a.current));
}

#[test]
fn oauth_validation_requires_access_or_refresh_token_and_disk_read_tolerates_corrupt_files() {
    let (_tmp, paths) = setup_paths();
    for bad in [
        serde_json::json!({ "id_token_email": "only-email@example.invalid" }),
        serde_json::json!({ "id_token": "eyJhbGciOiJub25lIn0.eyJlbWFpbCI6InVAZXhhbXBsZS5pbnZhbGlkIn0." }),
        serde_json::json!({ "oauth_token": "legacy-only" }),
    ] {
        let err = parse_oauth_metadata(&bad.to_string())
            .expect_err("must require access_token or refresh_token");
        assert_eq!(err.code, code::AUTH);
    }

    // Half-written OAuth file marks clean = false so polling retries on next tick.
    write_private_file(&paths.oauth_file(), b"{\"access_token\":\"half").unwrap();
    write_private_file(&paths.adc_file(), br#"{"type":"external_account"}"#).unwrap();
    let (state, clean) = state_with_health(&paths).unwrap();
    assert!(!clean);
    assert!(!state.oauth_present);
    assert!(state.accounts.is_empty());

    // Complete JSON with permanently unsupported schema (external_account ADC) marks clean = true
    // so disk_stamp advances and background polling does not spin on every tick.
    fs::remove_file(paths.oauth_file()).unwrap();
    let (state2, clean2) = state_with_health(&paths).unwrap();
    assert!(clean2);
    assert!(state2.accounts.is_empty());
}

#[test]
fn no_email_oauth_refresh_updates_in_place_without_overwriting_other_no_email_accounts() {
    let (_tmp, paths) = setup_paths();
    write_private_file(
        &paths.oauth_file(),
        br#"{"access_token":"ya29.no-email-1","refresh_token":"1//rt-1"}"#,
    )
    .unwrap();
    let s1 = state_at(&paths).unwrap();
    assert_eq!(s1.accounts.len(), 1);
    let id1 = s1.accounts[0].id.clone();

    // Refresh without email (even if refresh_token is omitted on refresh write) updates in place
    write_private_file(
        &paths.oauth_file(),
        br#"{"access_token":"ya29.no-email-2","refresh_token":"1//rt-1"}"#,
    )
    .unwrap();
    let s2 = state_at(&paths).unwrap();
    assert_eq!(s2.accounts.len(), 1);
    assert_eq!(s2.accounts[0].id, id1);
    assert!(s2.accounts[0].current);

    // A second no-email OAuth login with a different refresh_token must NOT overwrite id1
    write_private_file(
        &paths.oauth_file(),
        br#"{"access_token":"ya29.no-email-b","refresh_token":"1//rt-2"}"#,
    )
    .unwrap();
    let s3 = state_at(&paths).unwrap();
    assert_eq!(s3.accounts.len(), 2);
    let id2 = s3.accounts.iter().find(|a| a.id != id1).unwrap().id.clone();

    // Refreshing the currently active second no-email account without refresh_token updates id2, not id1
    write_private_file(
        &paths.oauth_file(),
        br#"{"access_token":"ya29.no-email-b-refreshed"}"#,
    )
    .unwrap();
    let s4 = state_at(&paths).unwrap();
    assert_eq!(s4.accounts.len(), 2);
    let (vault, _) = load_vault(&paths).unwrap();
    let entry1 = vault.entries.iter().find(|e| e.id == id1).unwrap();
    let entry2 = vault.entries.iter().find(|e| e.id == id2).unwrap();
    assert!(entry1.payload.contains("ya29.no-email-2"));
    assert!(entry2.payload.contains("ya29.no-email-b-refreshed"));
    assert!(vault.observed_revisions.is_empty());

    // Cap observed_revisions at 64 when deleting entries
    let mut v = vault;
    for i in 0..70 {
        remember_observed(&mut v, &format!("rev-{i}"));
    }
    assert_eq!(v.observed_revisions.len(), 64);
    assert_eq!(v.observed_revisions[0], "rev-6");
    assert_eq!(v.observed_revisions[63], "rev-69");
}

#[test]
fn adc_validation_and_auto_recognition_support_service_account_and_authorized_user() {
    let (_tmp, paths) = setup_paths();
    let lifecycle = MockLifecycle::new(false);

    let bad = add_adc_at(
        &paths,
        &lifecycle,
        AddAntigravityAdcInput {
            name: None,
            json_text: r#"{"type":"external_account"}"#.to_string(),
            quota_project_id: None,
            activate: false,
        },
    )
    .expect_err("external_account should be rejected");
    assert_eq!(bad.code, code::AUTH);

    let sa = serde_json::json!({
        "type": "service_account",
        "project_id": "gcp-prod-1",
        "private_key": "-----BEGIN PRIVATE KEY-----\nMIIB\n-----END PRIVATE KEY-----\n",
        "client_email": "bot@gcp-prod-1.iam.gserviceaccount.com"
    });
    let s1 = add_adc_at(
        &paths,
        &lifecycle,
        AddAntigravityAdcInput {
            name: Some("生产 SA".to_string()),
            json_text: sa.to_string(),
            quota_project_id: Some("override-proj".to_string()),
            activate: false,
        },
    )
    .unwrap();
    assert_eq!(s1.accounts.len(), 1);
    assert_eq!(s1.accounts[0].project_id.as_deref(), Some("override-proj"));

    let au = serde_json::json!({
        "type": "authorized_user",
        "client_id": "cid.apps.googleusercontent.com",
        "client_secret": "secret",
        "refresh_token": "1//refresh",
        "quota_project_id": "user-proj"
    });
    write_private_file(&paths.adc_file(), au.to_string().as_bytes()).unwrap();
    let s2 = state_at(&paths).unwrap();
    assert_eq!(s2.accounts.len(), 2);
    assert!(s2
        .accounts
        .iter()
        .any(|a| a.project_id.as_deref() == Some("user-proj") && a.current));
}

#[test]
fn auth_mode_distinguishes_two_mixed_states_and_target_mode_rejects_mixed() {
    let (_tmp, paths) = setup_paths();
    let au = serde_json::json!({
        "type": "authorized_user",
        "client_id": "cid",
        "client_secret": "sec",
        "refresh_token": "ref"
    });
    write_private_file(&paths.adc_file(), au.to_string().as_bytes()).unwrap();
    write_enable_adc(&paths, false).unwrap();

    let s1 = state_at(&paths).unwrap();
    assert_eq!(s1.mode, AuthMode::MixedAdcFileWithoutFlag);

    fs::remove_file(paths.adc_file()).unwrap();
    write_enable_adc(&paths, true).unwrap();
    let s2 = state_at(&paths).unwrap();
    assert_eq!(s2.mode, AuthMode::MixedFlagWithoutAdcFile);

    assert!(serde_json::from_str::<TargetMode>("\"mixed_adc_file_without_flag\"").is_err());
    assert!(serde_json::from_str::<TargetMode>("\"mixed_flag_without_adc_file\"").is_err());
}

#[test]
fn switch_account_switches_between_oauth_and_adc_and_restarts_running_app() {
    let (_tmp, paths) = setup_paths();
    let oauth1 = serde_json::json!({
        "email": "u1@example.invalid",
        "access_token": "tok-1"
    });
    write_private_file(&paths.oauth_file(), oauth1.to_string().as_bytes()).unwrap();
    let s0 = state_at(&paths).unwrap();
    let oauth1_id = s0.accounts[0].id.clone();

    let au = serde_json::json!({
        "type": "authorized_user",
        "client_id": "cid",
        "client_secret": "sec",
        "refresh_token": "ref",
        "quota_project_id": "p-1"
    });
    let lifecycle = MockLifecycle::new(true);
    let s1 = add_adc_at(
        &paths,
        &lifecycle,
        AddAntigravityAdcInput {
            name: Some("ADC 1".to_string()),
            json_text: au.to_string(),
            quota_project_id: None,
            activate: true,
        },
    )
    .unwrap();
    assert_eq!(s1.mode, AuthMode::Adc);
    assert!(s1.app_restarted);
    assert_eq!(lifecycle.events(), vec!["quit", "launch"]);

    let s2 = switch_account_at(&paths, &lifecycle, &oauth1_id, &s1.revision).unwrap();
    assert_eq!(s2.mode, AuthMode::Oauth);
    assert!(!paths.adc_file().exists());
    assert!(paths.adc_disabled_file().exists());
    assert!(s2.app_restarted);
}

#[test]
fn rename_and_delete_account_prevent_active_deletion_and_suppress_reimport() {
    let (_tmp, paths) = setup_paths();
    let lifecycle = MockLifecycle::new(false);
    let au = serde_json::json!({
        "type": "authorized_user",
        "client_id": "cid",
        "client_secret": "sec",
        "refresh_token": "ref"
    });
    write_private_file(&paths.adc_disabled_file(), au.to_string().as_bytes()).unwrap();
    let s1 = state_at(&paths).unwrap();
    assert_eq!(s1.accounts.len(), 1);
    let adc_id = s1.accounts[0].id.clone();

    let err = delete_account_at(&paths, &adc_id, &s1.revision).expect_err("cannot delete current");
    assert_eq!(err.code, code::CONFLICT);

    fs::remove_file(paths.adc_disabled_file()).unwrap();
    let s2 = state_at(&paths).unwrap();
    let s3 = rename_account_at(&paths, &adc_id, "新名称", &s2.revision).unwrap();
    assert_eq!(s3.accounts[0].name, "新名称");

    let s4 = delete_account_at(&paths, &adc_id, &s3.revision).unwrap();
    assert!(s4.accounts.is_empty());
    let _ = lifecycle;
}

#[test]
fn revision_conflict_and_quit_failure_preserve_disk_files() {
    let (_tmp, paths) = setup_paths();
    let oauth = serde_json::json!({
        "email": "u1@example.invalid",
        "access_token": "tok-1"
    });
    write_private_file(&paths.oauth_file(), oauth.to_string().as_bytes()).unwrap();
    let s1 = state_at(&paths).unwrap();

    let conflict = rename_account_at(&paths, &s1.accounts[0].id, "X", "stale-rev")
        .expect_err("stale revision must fail");
    assert_eq!(conflict.code, code::CONFLICT);

    let mut failing = MockLifecycle::new(true);
    failing.fail_quit = true;
    let res = set_mode_at(&paths, &failing, TargetMode::Adc, &s1.revision);
    assert!(res.is_err());
    assert!(!read_enable_adc(&paths).unwrap().0);
}

#[cfg(target_os = "macos")]
#[test]
fn macos_process_parser_matches_custom_install_paths_and_ignores_helpers() {
    let sample = "\
  101 /sbin/launchd
 4520 /Users/demo/Applications/Antigravity.app/Contents/MacOS/Antigravity
 4521 /Users/demo/Applications/Antigravity.app/Contents/Frameworks/Antigravity Helper (Renderer).app/Contents/MacOS/Antigravity Helper (Renderer)
 8890 /Volumes/External/Apps/Antigravity.app/Contents/MacOS/Antigravity
";
    let parsed = parse_running_antigravity_ps(sample);
    assert_eq!(
        parsed,
        vec![
            (
                4520,
                PathBuf::from("/Users/demo/Applications/Antigravity.app")
            ),
            (
                8890,
                PathBuf::from("/Volumes/External/Apps/Antigravity.app")
            ),
        ]
    );
}
