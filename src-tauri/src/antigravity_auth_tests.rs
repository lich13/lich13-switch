use super::*;

fn test_paths(root: &Path) -> Paths {
    Paths {
        gemini_home: root.join(".gemini"),
        gcloud_dir: root.join(".config/gcloud"),
        agswitch_dir: root.join(".config/agswitch"),
        data_dir: root.join("app-data"),
    }
}

#[test]
fn multi_oauth_and_multi_adc_support_json_and_key_switching_and_modes() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = test_paths(tmp.path());
    std::fs::create_dir_all(paths.config_file().parent().unwrap()).unwrap();
    storage::atomic_write(
        &paths.config_file(),
        b"{\"userSettings\":{\"themeMode\":\"THEME_MODE_DARK\"}}\n",
        None,
    )
    .unwrap();

    // 1. Add OAuth via JSON and via Key
    let s1 = add_oauth_at(
        &paths,
        AddOAuthInput {
            name: Some("Work OAuth".into()),
            input_mode: "json".into(),
            json_text: Some(
                "{\"access_token\":\"ya29.fixture-one\",\"refresh_token\":\"1//fixture-r1\",\"email\":\"work@example.invalid\"}"
                    .into(),
            ),
            key: None,
            refresh_token: None,
            email: None,
            activate: true,
        },
    )
    .unwrap();
    assert_eq!(s1.mode, "oauth");
    assert!(!s1.enable_adc);
    assert!(s1.oauth_present);
    assert!(s1.oauth_saved);
    assert_eq!(s1.accounts.len(), 1);
    assert!(s1.accounts[0].current);
    assert_eq!(
        s1.accounts[0].email.as_deref(),
        Some("work@example.invalid")
    );
    let oauth_id_1 = s1.accounts[0].id.clone();

    let s2 = add_oauth_at(
        &paths,
        AddOAuthInput {
            name: Some("Personal Token".into()),
            input_mode: "key".into(),
            json_text: None,
            key: Some("jetski-standalone-raw-key-two".into()),
            refresh_token: None,
            email: Some("personal@example.invalid".into()),
            activate: false,
        },
    )
    .unwrap();
    assert_eq!(s2.accounts.len(), 2);
    let oauth_id_2 = s2
        .accounts
        .iter()
        .find(|a| a.name == "Personal Token")
        .unwrap()
        .id
        .clone();

    // 2. Add ADC via JSON and via Key
    let s3 = add_adc_at(
        &paths,
        AddAdcInput {
            name: Some("GCP Alpha ADC".into()),
            input_mode: "json".into(),
            json_text: Some(
                "{\"type\":\"authorized_user\",\"client_id\":\"cid.example.invalid\",\"client_secret\":\"csec\",\"refresh_token\":\"1//adc-one\",\"quota_project_id\":\"proj-alpha\"}"
                    .into(),
            ),
            key: None,
            client_id: None,
            client_secret: None,
            project_id: None,
            client_email: None,
            activate: true,
        },
    )
    .unwrap();
    // Both OAuth and ADC are now present and enableAdc is true => mode is "both"
    assert_eq!(s3.mode, "both");
    assert!(s3.enable_adc);
    assert_eq!(s3.adc_status, "active");
    assert_eq!(s3.adc_project.as_deref(), Some("proj-alpha"));
    assert!(paths.agswitch_adc_file().is_file());
    let adc_id_1 = s3
        .accounts
        .iter()
        .find(|a| a.kind == "adc")
        .unwrap()
        .id
        .clone();

    let s4 = add_adc_at(
        &paths,
        AddAdcInput {
            name: Some("GCP Beta Key".into()),
            input_mode: "key".into(),
            json_text: None,
            key: Some("1//adc-key-two".into()),
            client_id: Some("cid-beta.example.invalid".into()),
            client_secret: Some("csec-beta".into()),
            project_id: Some("proj-beta".into()),
            client_email: None,
            activate: false,
        },
    )
    .unwrap();
    let adc_id_2 = s4
        .accounts
        .iter()
        .find(|a| a.name == "GCP Beta Key")
        .unwrap()
        .id
        .clone();

    // 3. Switch ADC to Beta
    let s5 = switch_adc_at(&paths, &adc_id_2, &s4.revision, true).unwrap();
    assert_eq!(s5.adc_project.as_deref(), Some("proj-beta"));
    assert!(
        s5.accounts
            .iter()
            .find(|a| a.id == adc_id_2)
            .unwrap()
            .current
    );

    // 4. Switch OAuth to Personal Token with disable_adc=true (switches to pure OAuth mode)
    let s6 = switch_oauth_at(&paths, &oauth_id_2, &s5.revision, true).unwrap();
    assert_eq!(s6.mode, "oauth");
    assert!(!s6.enable_adc);
    assert_eq!(s6.adc_status, "disabled");
    assert!(!paths.adc_file().exists());
    assert!(paths.adc_disabled_file().is_file());
    assert!(
        s6.accounts
            .iter()
            .find(|a| a.id == oauth_id_2)
            .unwrap()
            .current
    );
    // Unrelated userSettings.themeMode in config.json is preserved!
    let cfg_text = std::fs::read_to_string(paths.config_file()).unwrap();
    assert!(cfg_text.contains("THEME_MODE_DARK"));
    assert!(cfg_text.contains("\"enableAdc\": false"));

    // 5. Switch mode to "both" restores disabled ADC and sets enableAdc=true
    let s7 = set_mode_at(&paths, "both", &s6.revision).unwrap();
    assert_eq!(s7.mode, "both");
    assert!(s7.enable_adc);
    assert_eq!(s7.adc_status, "active");
    assert!(paths.adc_file().is_file());
    assert!(!paths.adc_disabled_file().exists());

    // Keep unused variable warning away and verify first OAuth & ADC are now inactive
    assert!(
        !s7.accounts
            .iter()
            .find(|a| a.id == oauth_id_1)
            .unwrap()
            .current
    );
    assert!(
        !s7.accounts
            .iter()
            .find(|a| a.id == adc_id_1)
            .unwrap()
            .current
    );
}

#[test]
fn delete_protection_rejects_unknown_or_active_targets_and_switch_rolls_back_on_failure() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = test_paths(tmp.path());

    let s1 = add_oauth_at(
        &paths,
        AddOAuthInput {
            name: Some("Active OAuth".into()),
            input_mode: "key".into(),
            json_text: None,
            key: Some("oauth-live-token".into()),
            refresh_token: None,
            email: None,
            activate: true,
        },
    )
    .unwrap();
    let active_oauth_id = s1.accounts[0].id.clone();

    let s2 = add_oauth_at(
        &paths,
        AddOAuthInput {
            name: Some("Standby OAuth".into()),
            input_mode: "key".into(),
            json_text: None,
            key: Some("oauth-standby-token".into()),
            refresh_token: None,
            email: None,
            activate: false,
        },
    )
    .unwrap();
    let standby_oauth_id = s2
        .accounts
        .iter()
        .find(|a| a.name == "Standby OAuth")
        .unwrap()
        .id
        .clone();

    // Out-of-bounds / unknown ID fails with code::ACCOUNT
    assert_eq!(
        delete_at(&paths, "non-existent-id", &s2.revision)
            .unwrap_err()
            .code,
        code::ACCOUNT
    );

    // Deleting currently active OAuth account fails with code::CONFLICT and keeps both accounts
    assert_eq!(
        delete_at(&paths, &active_oauth_id, &s2.revision)
            .unwrap_err()
            .code,
        code::CONFLICT
    );
    assert_eq!(state_at(&paths).unwrap().accounts.len(), 2);

    // Deleting inactive Standby OAuth succeeds
    let s3 = delete_at(&paths, &standby_oauth_id, &s2.revision).unwrap();
    assert_eq!(s3.accounts.len(), 1);
    assert_eq!(s3.accounts[0].id, active_oauth_id);

    // Partial failure during switch rolls back oauth_file when config.json write fails
    let s4 = add_oauth_at(
        &paths,
        AddOAuthInput {
            name: Some("Second OAuth".into()),
            input_mode: "key".into(),
            json_text: None,
            key: Some("oauth-second-token".into()),
            refresh_token: None,
            email: None,
            activate: false,
        },
    )
    .unwrap();
    let second_id = s4
        .accounts
        .iter()
        .find(|a| a.name == "Second OAuth")
        .unwrap()
        .id
        .clone();
    // Corrupt config.json into a directory so write_enable_adc fails after oauth_file was written
    if paths.config_file().exists() {
        std::fs::remove_file(paths.config_file()).unwrap();
    }
    std::fs::create_dir_all(paths.config_file()).unwrap();
    assert!(switch_oauth_at(&paths, &second_id, &s4.revision, true).is_err());
    // Live oauth_file must be rolled back to "oauth-live-token\n"
    assert_eq!(
        std::fs::read_to_string(paths.oauth_file()).unwrap(),
        "oauth-live-token\n"
    );
}

#[test]
fn backup_rotation_retains_at_most_ten_snapshots() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = test_paths(tmp.path());
    std::fs::create_dir_all(&paths.gemini_home).unwrap();
    storage::atomic_write(&paths.oauth_file(), b"tok\n", None).unwrap();
    for i in 0..13 {
        let snap = paths.backups_dir().join(format!("backup-{:04}", i));
        storage::private_dir(&snap).unwrap();
        storage::atomic_write(&snap.join("jetski-standalone-oauth-token"), b"tok\n", None).unwrap();
    }
    let count = create_backup_at(&paths).unwrap();
    assert_eq!(count, MAX_BACKUPS);
}
