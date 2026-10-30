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

#[tokio::test]
async fn endpoint_probe_wake_ignores_request_events_and_wakes_on_store_or_lifecycle_change() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let g = Gateway::new(dir.path().join("data")).unwrap();

    // Request activity (Active::new / Active::drop -> g.changed()) must NOT wake the endpoint prober.
    {
        let _active = Active::new(g.clone());
    }
    assert!(tokio::time::timeout(
        std::time::Duration::from_millis(25),
        g.wait_endpoint_probe_wake(),
    )
    .await
    .is_err());

    // Store mutation wakes the dedicated prober immediately (even if notified before awaiting).
    let rev = g.view().revision;
    let args = serde_json::json!({
        "op": "saveProvider",
        "name": "Fixture",
        "baseUrl": "https://example.invalid/v1",
        "token": "fixture-token",
    });
    g.edit(serde_json::from_value(args).unwrap(), &rev, &home)
        .unwrap();
    tokio::time::timeout(
        std::time::Duration::from_millis(25),
        g.wait_endpoint_probe_wake(),
    )
    .await
    .expect("store mutation should wake endpoint prober");
}
