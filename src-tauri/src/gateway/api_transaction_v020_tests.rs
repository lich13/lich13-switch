use super::*;

fn config() -> &'static str {
    "model_provider='relay'\r\nmodel='keep'\r\n[model_providers.relay]\r\nbase_url='https://example.invalid/v1' # keep\r\nrequires_openai_auth=true\r\nsupports_websockets=false\r\n"
}
fn fixture() -> (tempfile::TempDir, Write) {
    let t = tempfile::tempdir().unwrap();
    let tx = Write {
        version: 1,
        home: t.path().join("home"),
        config_before: config().into(),
        config_after: patch_base(config(), "relay", "http://127.0.0.1:15722/v1").unwrap(),
        auth_before: Some(br#"{"OPENAI_API_KEY":"fixture-original","extra":17}"#.to_vec()),
        auth_after: br#"{"auth_mode":"apikey","OPENAI_API_KEY":"fixture-next","extra":17}"#
            .to_vec(),
    };
    fs::create_dir(&tx.home).unwrap();
    storage::atomic_write(
        &t.path().join(WRITE_FILE),
        &serde_json::to_vec(&tx).unwrap(),
        None,
    )
    .unwrap();
    (t, tx)
}

#[test]
fn v020_api_crash_restores_both_files_exactly_in_all_write_phases() {
    for phase in 0..3 {
        let (t, tx) = fixture();
        fs::write(
            tx.home.join("config.toml"),
            if phase > 0 {
                &tx.config_after
            } else {
                &tx.config_before
            },
        )
        .unwrap();
        fs::write(
            tx.home.join("auth.json"),
            if phase > 1 {
                &tx.auth_after
            } else {
                tx.auth_before.as_ref().unwrap()
            },
        )
        .unwrap();
        recover_write(t.path()).unwrap();
        recover_write(t.path()).unwrap();
        assert_eq!(
            fs::read(tx.home.join("config.toml")).unwrap(),
            tx.config_before.as_bytes()
        );
        assert_eq!(
            fs::read(tx.home.join("auth.json")).unwrap(),
            tx.auth_before.unwrap()
        );
        assert!(!t.path().join(WRITE_FILE).exists());
    }
}
#[test]
fn v020_api_partial_rollback_preserves_external_auth_and_restores_owned_config() {
    let (t, tx) = fixture();
    fs::write(tx.home.join("config.toml"), &tx.config_after).unwrap();
    let foreign = br#"{"auth_mode":"apikey","OPENAI_API_KEY":"fixture-external"}"#;
    fs::write(tx.home.join("auth.json"), foreign).unwrap();
    assert!(recover_write(t.path()).is_err());
    assert_eq!(fs::read(tx.home.join("auth.json")).unwrap(), foreign);
    assert_eq!(
        fs::read(tx.home.join("config.toml")).unwrap(),
        tx.config_before.as_bytes()
    );
    assert!(t.path().join(WRITE_FILE).exists());
}
#[test]
fn v020_api_auth_revision_conflict_does_not_touch_config() {
    let (t, tx) = fixture();
    fs::remove_file(t.path().join(WRITE_FILE)).unwrap();
    fs::write(tx.home.join("config.toml"), &tx.config_before).unwrap();
    fs::write(tx.home.join("auth.json"), tx.auth_before.as_ref().unwrap()).unwrap();
    let (revision, _) = read(&tx.home, "relay").unwrap();
    fs::write(tx.home.join("auth.json"), &tx.auth_after).unwrap();
    let error = write(
        t.path(),
        &tx.home,
        "relay",
        &Pair::new("https://next.example.invalid", "fixture-third"),
        Some(&revision),
        None,
    )
    .unwrap_err();
    assert_eq!(error.code, "CONFLICT");
    assert_eq!(
        fs::read(tx.home.join("config.toml")).unwrap(),
        tx.config_before.as_bytes()
    );
    assert_eq!(fs::read(tx.home.join("auth.json")).unwrap(), tx.auth_after);
    assert!(!t.path().join(WRITE_FILE).exists());
}
#[test]
fn v020_api_missing_base_only_inserts_into_the_active_table() {
    for input in [
        "model_provider='relay'\r\n[model_providers.relay]\r\nrequires_openai_auth=true\r\n[next]\r\nkeep=1\r\n",
        "model_provider='relay'\nmodel_providers={relay={requires_openai_auth=true},spare={base_url='keep'}}\n",
        "model_provider='relay'\n[model_providers]\nrelay.requires_openai_auth=true\n",
        "model_provider='relay'\nmodel_providers.relay.requires_openai_auth=true\n",
    ] {
        let output = patch_base(input, "relay", "https://example.invalid/prefix/v1").unwrap();
        let mut updated: toml::Table = output.parse().unwrap();
        updated["model_providers"]["relay"].as_table_mut().unwrap().remove("base_url");
        assert_eq!(updated, input.parse::<toml::Table>().unwrap());
        assert!(output.contains("requires_openai_auth=true"));
    }
}
#[test]
fn v020_api_auth_precedence_and_duplicates_do_not_guess_a_login() {
    for value in [
        r#"{"auth_mode":"chatgpt","OPENAI_API_KEY":"fixture-stale"}"#,
        r#"{"bedrock_api_key":"fixture-cloud","OPENAI_API_KEY":"fixture-stale"}"#,
        r#"{"auth_mode":"unknown","OPENAI_API_KEY":"fixture-stale"}"#,
    ] {
        assert_eq!(
            api_key(&auth(Some(value.as_bytes())).unwrap()).unwrap(),
            None
        );
    }
    assert!(auth(Some(br#"{"OPENAI_API_KEY":"a","OPENAI_API_KEY":"b"}"#)).is_err());
    assert_eq!(api_key(&auth(Some(br#"{"auth_mode":"apikey","OPENAI_API_KEY":"fixture-current","tokens":{"keep":true}}"#)).unwrap()).unwrap().as_deref(), Some("fixture-current"));
}
