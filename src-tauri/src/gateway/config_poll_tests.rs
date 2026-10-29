use super::*;

#[test]
fn cached_poll_tracks_replacement_guard_and_invalid_text_without_weakening_save_conflicts() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let g = Gateway::new(dir.path().join("data")).unwrap();
    let path = ClientId::Codex.config(&home);
    storage::atomic_write(&path, b"model='fixture-one'", None).unwrap();
    let mut cache = crate::auth_cache::Cache::default();
    let first = g.config_stamp(&home, &mut cache).unwrap();
    assert_eq!(first.1, g.read_config(&home).unwrap().revision);
    assert_eq!(first, g.config_stamp(&home, &mut cache).unwrap());
    storage::atomic_write(&path, b"model='fixture-two'", None).unwrap();
    let second = g.config_stamp(&home, &mut cache).unwrap();
    assert_ne!(first.1, second.1);
    assert!(crate::configuration::save(
        ClientId::Codex,
        &home,
        &g.0.data,
        "model='fixture-three'",
        &first.1,
        false
    )
    .is_err());
    g.0.inner.lock().unwrap().running = true;
    assert!(g.config_stamp(&home, &mut cache).unwrap().2);
    storage::atomic_write(&path, &[0xff], None).unwrap();
    assert!(g.config_stamp(&home, &mut cache).is_err());
    std::fs::remove_file(&path).unwrap();
    assert_eq!(g.config_stamp(&home, &mut cache).unwrap().1, "missing");
}
