// 覆盖 Codex API 多文件写入、失败还原与外部冲突。
use super::*;
use std::fs;

fn config() -> &'static str {
    "model_provider='relay'\r\nmodel='keep'\r\n[model_providers.relay]\r\nbase_url='https://example.invalid/v1' # keep\r\nrequires_openai_auth=true\r\nsupports_websockets=false\r\n"
}
struct Fixture {
    home: std::path::PathBuf,
    config_before: String,
    auth_before: Vec<u8>,
    auth_after: Vec<u8>,
}
fn fixture() -> (tempfile::TempDir, Fixture) {
    let t = tempfile::tempdir().unwrap();
    let tx = Fixture {
        home: t.path().join("home"),
        config_before: config().into(),
        auth_before: br#"{"OPENAI_API_KEY":"fixture-original","extra":17}"#.to_vec(),
        auth_after: br#"{"auth_mode":"apikey","OPENAI_API_KEY":"fixture-next","extra":17}"#
            .to_vec(),
    };
    fs::create_dir(&tx.home).unwrap();
    (t, tx)
}

#[cfg(unix)]
#[test]
fn v020_api_failed_auth_write_restores_config() {
    use std::os::unix::fs::PermissionsExt;
    let (_t, tx) = fixture();
    fs::write(tx.home.join("config.toml"), &tx.config_before).unwrap();
    // auth.json as a directory makes the second write fail after config.toml was written.
    fs::create_dir(tx.home.join("auth.json")).unwrap();
    let _ = fs::set_permissions(tx.home.join("auth.json"), fs::Permissions::from_mode(0o700));
    assert!(write(
        &tx.home,
        "relay",
        &Pair::new("https://next.example.invalid", "fixture-third"),
        None,
        None,
    )
    .is_err());
    assert_eq!(
        fs::read(tx.home.join("config.toml")).unwrap(),
        tx.config_before.as_bytes()
    );
}
#[test]
fn v020_api_auth_revision_conflict_does_not_touch_config() {
    let (_t, tx) = fixture();
    fs::write(tx.home.join("config.toml"), &tx.config_before).unwrap();
    fs::write(tx.home.join("auth.json"), &tx.auth_before).unwrap();
    let (revision, _) = read(&tx.home, "relay").unwrap();
    fs::write(tx.home.join("auth.json"), &tx.auth_after).unwrap();
    let error = write(
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
