use super::*;
use std::fs;

fn assert_default(settings: &Settings) {
    assert!(settings.recording);
    assert!(settings.auto_sync);
    assert_eq!(settings.refresh_seconds, 30);
    assert_eq!(settings.multiplier, "1");
    assert_eq!(settings.pricing_model, "response");
}

#[test]
fn missing_settings_use_defaults_without_creating_a_backup() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("settings.json");

    let (settings, error) = load_settings(&path);

    assert_default(&settings);
    assert!(error.is_none());
    assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 0);
}

#[test]
fn malformed_and_invalid_settings_are_preserved_with_digest_deduplication() {
    let temp = tempfile::tempdir().unwrap();
    let fixtures: [(&str, &[u8]); 2] = [
        ("malformed", b"{ synthetic invalid json"),
        ("invalid", br#"{"refreshSeconds":17}"#),
    ];

    for (name, bytes) in fixtures {
        let directory = temp.path().join(name);
        fs::create_dir(&directory).unwrap();
        let path = directory.join("settings.json");
        let backup = directory.join(format!("settings.json.corrupt-{}", storage::digest(bytes)));
        fs::write(&path, bytes).unwrap();

        for _ in 0..2 {
            let (settings, error) = load_settings(&path);
            assert_default(&settings);
            assert!(error.is_some());
            assert_eq!(fs::read(&path).unwrap(), bytes);
            assert_eq!(fs::read(&backup).unwrap(), bytes);
        }

        assert_eq!(fs::read_dir(directory).unwrap().count(), 2);
    }
}

#[test]
fn backup_collision_is_reported_and_keeps_both_existing_files() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("settings.json");
    let bytes = b"synthetic invalid settings";
    let backup = temp
        .path()
        .join(format!("settings.json.corrupt-{}", storage::digest(bytes)));
    fs::write(&path, bytes).unwrap();
    fs::write(&backup, b"different fixture bytes").unwrap();

    let (settings, error) = load_settings(&path);

    assert_default(&settings);
    assert!(error.unwrap().contains("备份失败"));
    assert_eq!(fs::read(&path).unwrap(), bytes);
    assert_eq!(fs::read(&backup).unwrap(), b"different fixture bytes");
}

#[cfg(unix)]
#[test]
fn settings_backup_symlink_is_not_followed_and_backup_failure_is_reported() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("settings.json");
    let bytes = b"synthetic invalid settings";
    let target = temp.path().join("symlink-target.fixture");
    let backup = temp
        .path()
        .join(format!("settings.json.corrupt-{}", storage::digest(bytes)));
    fs::write(&path, bytes).unwrap();
    fs::write(&target, b"symlink target fixture").unwrap();
    symlink(&target, &backup).unwrap();

    let (settings, error) = load_settings(&path);

    assert_default(&settings);
    assert!(error.unwrap().contains("备份失败"));
    assert_eq!(fs::read(&path).unwrap(), bytes);
    assert!(fs::symlink_metadata(&backup)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(fs::read(&target).unwrap(), b"symlink target fixture");
}

#[test]
fn successful_configure_clears_only_settings_error() {
    let temp = tempfile::tempdir().unwrap();
    let service = Service::new(temp.path());
    *service.0.settings_error.lock().unwrap() = Some("stale settings error".into());
    *service.0.error.lock().unwrap() = Some("unrelated service error".into());
    let settings = Settings {
        log_columns: Default::default(),
        log_number_mode: "compact".into(),
        recording: false,
        auto_sync: false,
        refresh_seconds: 5,
        multiplier: "2.5".into(),
        pricing_model: "request".into(),
    };

    let state = service.configure(settings.clone()).unwrap();

    assert!(service.0.settings_error.lock().unwrap().is_none());
    assert_eq!(state.error.as_deref(), Some("unrelated service error"));
    assert!(!state.settings.recording);
    assert!(!state.settings.auto_sync);
    assert_eq!(state.settings.refresh_seconds, 5);
    assert_eq!(state.settings.multiplier, "2.5");
    assert_eq!(state.settings.pricing_model, "request");
    let saved = fs::read(&service.0.path).unwrap();
    let saved: Settings = serde_json::from_slice(&saved).unwrap();
    assert!(!saved.recording);
    assert!(!saved.auto_sync);
    assert_eq!(saved.refresh_seconds, 5);
    assert_eq!(saved.multiplier, "2.5");
    assert_eq!(saved.pricing_model, "request");
}
