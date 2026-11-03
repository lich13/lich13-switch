use super::*;
use std::sync::Mutex;

fn test_paths(root: &Path) -> Paths {
    Paths {
        gemini_home: root.join(".gemini"),
        gcloud_dir: root.join(".config/gcloud"),
        agswitch_dir: root.join(".config/agswitch"),
        data_dir: root.join("app-data"),
    }
}

fn real_jetski_oauth_json(email: &str, auth_method: &str, access_token: &str) -> String {
    serde_json::to_string_pretty(&json!({
        "id_token_email": email,
        "auth_method": auth_method,
        "token": {
            "access_token": access_token,
            "refresh_token": format!("1//refresh-{access_token}"),
            "token_type": "Bearer",
            "expiry": "2026-12-31T00:00:00Z"
        }
    }))
    .unwrap()
}

fn valid_authorized_user_adc(project: &str, refresh: &str) -> String {
    serde_json::to_string_pretty(&json!({
        "type": "authorized_user",
        "client_id": "fixture-client.apps.googleusercontent.com",
        "client_secret": "GOCSPX-fixture-secret",
        "refresh_token": refresh,
        "quota_project_id": project
    }))
    .unwrap()
}

fn valid_service_account_adc(project: &str, email: &str) -> String {
    serde_json::to_string_pretty(&json!({
        "type": "service_account",
        "project_id": project,
        "private_key_id": "key-1",
        "private_key": "-----BEGIN PRIVATE KEY-----\nMIIEvgIBADANBgkqhkiG9w0BAQEFAASCBKgwggSkAgEAAoIBAQC7\n-----END PRIVATE KEY-----\n",
        "client_email": email,
        "token_uri": "https://oauth2.googleapis.com/token"
    }))
    .unwrap()
}

#[derive(Default)]
struct RecordingLifecycle {
    events: Mutex<Vec<&'static str>>,
    running: bool,
    fail_stop: bool,
}

impl AppLifecycle for RecordingLifecycle {
    fn stop_if_running(&self) -> Result<bool> {
        self.events.lock().unwrap().push("stop");
        if self.fail_stop {
            return Err(AppError::new(code::PROCESS, "模拟退出超时"));
        }
        Ok(self.running)
    }
    fn relaunch(&self) -> Result<()> {
        self.events.lock().unwrap().push("relaunch");
        Ok(())
    }
}

#[test]
fn real_jetski_oauth_and_strict_adc_switching_with_mixed_mode_detection_and_app_lifecycle() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = test_paths(tmp.path());
    std::fs::create_dir_all(paths.config_file().parent().unwrap()).unwrap();
    storage::atomic_write(
        &paths.config_file(),
        b"{\"userSettings\":{\"themeMode\":\"THEME_MODE_DARK\",\"enableAdc\":false}}\n",
        None,
    )
    .unwrap();

    // Write real Jetski OAuth token on disk and import from current login
    let oauth_raw_1 = real_jetski_oauth_json("work@example.invalid", "google-oauth", "ya29.work");
    storage::atomic_write(&paths.oauth_file(), oauth_raw_1.as_bytes(), None).unwrap();

    let s1 = import_current_oauth_at(&paths, Some("Work OAuth".into())).unwrap();
    assert_eq!(s1.mode, AuthMode::Oauth);
    assert!(!s1.enable_adc);
    assert!(s1.oauth_present);
    assert!(s1.oauth_saved);
    assert_eq!(s1.oauth_email.as_deref(), Some("work@example.invalid"));
    assert_eq!(s1.oauth_auth_method.as_deref(), Some("google-oauth"));
    assert_eq!(s1.accounts.len(), 1);
    assert_eq!(
        s1.accounts[0].email.as_deref(),
        Some("work@example.invalid")
    );
    assert_eq!(s1.accounts[0].source_type, "google-oauth");
    let oauth_id_1 = s1.accounts[0].id.clone();

    // Import second OAuth login from disk
    let oauth_raw_2 =
        real_jetski_oauth_json("personal@example.invalid", "jetski-sso", "ya29.personal");
    storage::atomic_write(&paths.oauth_file(), oauth_raw_2.as_bytes(), None).unwrap();
    let s2 = import_current_oauth_at(&paths, Some("Personal OAuth".into())).unwrap();
    assert_eq!(s2.accounts.len(), 2);
    let oauth_id_2 = s2
        .accounts
        .iter()
        .find(|a| a.name == "Personal OAuth")
        .unwrap()
        .id
        .clone();

    // Add valid authorized_user ADC and activate it with running app lifecycle
    let lifecycle = RecordingLifecycle {
        running: true,
        ..Default::default()
    };
    let s3 = add_adc_at_with_lifecycle(
        &paths,
        AddAdcInput {
            name: Some("GCP Alpha ADC".into()),
            json_text: valid_authorized_user_adc("proj-alpha", "1//adc-one"),
            quota_project_id: None,
            activate: true,
        },
        &lifecycle,
    )
    .unwrap();
    // Both OAuth file and ADC file exist on disk and enableAdc == true => normal AuthMode::Adc (NOT Mixed!)
    assert_eq!(s3.mode, AuthMode::Adc);
    assert!(s3.enable_adc);
    assert_eq!(s3.adc_status, AdcStatus::Active);
    assert_eq!(s3.adc_type, Some(AdcCredentialType::AuthorizedUser));
    assert_eq!(s3.adc_project.as_deref(), Some("proj-alpha"));
    assert!(s3.app_restarted);
    assert_eq!(*lifecycle.events.lock().unwrap(), vec!["stop", "relaunch"]);

    // Add valid service_account ADC with quota_project_id override
    let s4 = add_adc_at(
        &paths,
        AddAdcInput {
            name: Some("GCP SA Beta".into()),
            json_text: valid_service_account_adc("proj-raw", "sa@proj.iam.gserviceaccount.com"),
            quota_project_id: Some("proj-override".into()),
            activate: false,
        },
    )
    .unwrap();
    let adc_id_2 = s4
        .accounts
        .iter()
        .find(|a| a.name == "GCP SA Beta")
        .unwrap()
        .id
        .clone();
    let s5 = switch_adc_at(&paths, &adc_id_2, &s4.revision, true).unwrap();
    assert_eq!(s5.mode, AuthMode::Adc);
    assert_eq!(s5.adc_type, Some(AdcCredentialType::ServiceAccount));
    assert_eq!(s5.adc_project.as_deref(), Some("proj-override"));

    // Mixed mode is reported when enableAdc == false while ADC file still exists on disk
    write_enable_adc(&paths, false).unwrap();
    let mixed_state = state_at(&paths).unwrap();
    assert_eq!(mixed_state.mode, AuthMode::Mixed);
    assert!(set_mode_at(&paths, AuthMode::Mixed, &mixed_state.revision).is_err());

    // Switch back to OAuth mode disables active ADC file (.disabled) and sets enableAdc=false
    let s6 = switch_oauth_at(&paths, &oauth_id_1, &mixed_state.revision, true).unwrap();
    assert_eq!(s6.mode, AuthMode::Oauth);
    assert!(!s6.enable_adc);
    assert_eq!(s6.adc_status, AdcStatus::Disabled);
    assert!(!paths.adc_file().exists());
    assert!(paths.adc_disabled_file().is_file());
    assert_eq!(s6.oauth_email.as_deref(), Some("work@example.invalid"));
    assert!(
        !s6.accounts
            .iter()
            .find(|a| a.id == oauth_id_2)
            .unwrap()
            .current
    );

    // If app fails to stop, switching aborts before touching any credential files
    let failing_lifecycle = RecordingLifecycle {
        running: true,
        fail_stop: true,
        ..Default::default()
    };
    assert!(switch_oauth_at_with_lifecycle(
        &paths,
        &oauth_id_2,
        &s6.revision,
        true,
        &failing_lifecycle
    )
    .is_err());
    assert_eq!(
        state_at(&paths).unwrap().oauth_email.as_deref(),
        Some("work@example.invalid")
    );
}

#[test]
fn strict_adc_validation_rejects_incomplete_credentials_and_preserves_third_party_dir_permissions()
{
    let tmp = tempfile::tempdir().unwrap();
    let paths = test_paths(tmp.path());
    std::fs::create_dir_all(&paths.gemini_home).unwrap();
    std::fs::create_dir_all(&paths.gcloud_dir).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&paths.gemini_home, std::fs::Permissions::from_mode(0o755))
            .unwrap();
        std::fs::set_permissions(&paths.gcloud_dir, std::fs::Permissions::from_mode(0o755))
            .unwrap();
    }

    // Missing client_secret in authorized_user must fail
    assert!(add_adc_at(
        &paths,
        AddAdcInput {
            name: None,
            json_text:
                "{\"type\":\"authorized_user\",\"client_id\":\"cid\",\"refresh_token\":\"1//r\"}"
                    .into(),
            quota_project_id: None,
            activate: false,
        }
    )
    .is_err());

    // Missing token_uri in service_account must fail
    assert!(add_adc_at(
        &paths,
        AddAdcInput {
            name: None,
            json_text: "{\"type\":\"service_account\",\"private_key\":\"-----BEGIN PRIVATE KEY-----\\nabc\\n-----END PRIVATE KEY-----\",\"client_email\":\"a@b.c\"}".into(),
            quota_project_id: None,
            activate: false,
        }
    )
    .is_err());

    // Adding & activating valid ADC does not alter 0755 permissions on ~/.gemini or ~/.config/gcloud
    let s1 = add_adc_at(
        &paths,
        AddAdcInput {
            name: Some("Valid ADC".into()),
            json_text: valid_authorized_user_adc("proj-1", "1//r1"),
            quota_project_id: None,
            activate: true,
        },
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&paths.gemini_home)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
        assert_eq!(
            std::fs::metadata(&paths.gcloud_dir)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
    }

    // Remove all disk ADC files; set_mode_at(Adc) must NOT silently pick from vault
    std::fs::remove_file(paths.adc_file()).unwrap();
    std::fs::remove_file(paths.agswitch_adc_file()).unwrap();
    write_enable_adc(&paths, false).unwrap();
    let s2 = state_at(&paths).unwrap();
    assert_eq!(s2.accounts.len(), 1);
    assert!(set_mode_at(&paths, AuthMode::Adc, &s2.revision).is_err());
    let _ = s1;
}

#[test]
fn state_at_is_read_only_on_backups_and_delete_protection_rejects_active_or_unknown_ids() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = test_paths(tmp.path());
    std::fs::create_dir_all(&paths.gemini_home).unwrap();
    let oauth_1 = real_jetski_oauth_json("one@example.invalid", "oauth", "ya29.one");
    let oauth_2 = real_jetski_oauth_json("two@example.invalid", "oauth", "ya29.two");
    storage::atomic_write(&paths.oauth_file(), oauth_2.as_bytes(), None).unwrap();
    let s_standby = import_current_oauth_at(&paths, Some("Standby".into())).unwrap();
    let standby_id = s_standby.accounts[0].id.clone();

    storage::atomic_write(&paths.oauth_file(), oauth_1.as_bytes(), None).unwrap();
    let s_active = import_current_oauth_at(&paths, Some("Active".into())).unwrap();
    let active_id = s_active
        .accounts
        .iter()
        .find(|a| a.name == "Active")
        .unwrap()
        .id
        .clone();

    // Seed 13 backup directories: state_at must report 13 without pruning any; create_backup_at prunes to 10.
    for i in 0..13 {
        let snap = paths.backups_dir().join(format!("backup-{:04}", i));
        storage::private_dir(&snap).unwrap();
        storage::atomic_write(
            &snap.join("jetski-standalone-oauth-token"),
            oauth_1.as_bytes(),
            None,
        )
        .unwrap();
    }
    let read_only_state = state_at(&paths).unwrap();
    assert_eq!(read_only_state.backup_count, 13);
    assert_eq!(create_backup_at(&paths).unwrap(), MAX_BACKUPS);

    let rev = state_at(&paths).unwrap().revision;
    assert_eq!(
        delete_at(&paths, "non-existent", &rev).unwrap_err().code,
        code::ACCOUNT
    );
    assert_eq!(
        delete_at(&paths, &active_id, &rev).unwrap_err().code,
        code::CONFLICT
    );
    let after_del = delete_at(&paths, &standby_id, &rev).unwrap();
    assert_eq!(after_del.accounts.len(), 1);
    assert_eq!(after_del.accounts[0].id, active_id);
}
