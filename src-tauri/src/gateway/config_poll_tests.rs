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
    {
        let mut state = g.0.inner.lock().unwrap();
        state.running = true;
        state.refresh_editable_digest(g.0.client);
    }
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

#[test]
fn try_view_config_cache_tracks_external_codex_and_claude_edits_immediately() {
    let dir = tempfile::tempdir().unwrap();
    let codex_home = dir.path().join("codex-home");
    let claude_home = dir.path().join("claude-home");
    std::fs::create_dir_all(&codex_home).unwrap();
    std::fs::create_dir_all(&claude_home).unwrap();

    let codex = Gateway::new(dir.path().join("codex-data")).unwrap();
    let claude = codex.companion(dir.path().join("claude-data")).unwrap();

    let codex_cfg = ClientId::Codex.config(&codex_home);
    storage::atomic_write(
        &codex_cfg,
        b"model_provider='custom'\n[model_providers.custom]\nbase_url='https://a.example.invalid/v1'\nexperimental_bearer_token='tok-a'\n",
        None,
    )
    .unwrap();
    let rev = codex.view().revision;
    codex
        .edit(
            Edit::SaveProvider {
                search_only: None,
                quota_access_token: None,
                id: None,
                base_url: "https://a.example.invalid/v1".into(),
                token: "tok-a".into(),
                name: Some("Alpha".into()),
            },
            &rev,
            &codex_home,
        )
        .unwrap();
    let rev = codex.view().revision;
    let second = codex
        .edit(
            Edit::SaveProvider {
                search_only: None,
                quota_access_token: None,
                id: None,
                base_url: "https://b.example.invalid/v1".into(),
                token: "tok-b".into(),
                name: Some("Beta".into()),
            },
            &rev,
            &codex_home,
        )
        .unwrap();
    let alpha_id = second.providers[0].id.clone();
    let beta_id = second.providers[1].id.clone();
    assert_eq!(second.config_provider.as_deref(), Some(alpha_id.as_str()));
    // Cached repeat view returns identical revision and config_revision.
    let cached_view = codex.try_view().unwrap();
    assert_eq!(cached_view.revision, second.revision);
    assert_eq!(cached_view.config_revision, second.config_revision);

    // External atomic modification on disk invalidates the fingerprint cache immediately.
    storage::atomic_write(
        &codex_cfg,
        b"model_provider='custom'\n[model_providers.custom]\nbase_url='https://b.example.invalid/v1'\nexperimental_bearer_token='tok-b'\n",
        None,
    )
    .unwrap();
    let updated_view = codex.try_view().unwrap();
    assert_ne!(updated_view.config_revision, cached_view.config_revision);
    assert_eq!(
        updated_view.config_provider.as_deref(),
        Some(beta_id.as_str())
    );

    // Claude settings.json cache also updates immediately on external edit.
    let claude_cfg = ClientId::Claude.config(&claude_home);
    storage::atomic_write(
        &claude_cfg,
        br#"{"env":{"ANTHROPIC_BASE_URL":"https://a.example.invalid","ANTHROPIC_AUTH_TOKEN":"c-a"}}"#,
        None,
    )
    .unwrap();
    let claude_rev = claude.view().revision;
    let claude_v1 = claude
        .edit(
            Edit::SaveProvider {
                search_only: None,
                quota_access_token: None,
                id: None,
                base_url: "https://a.example.invalid".into(),
                token: "c-a".into(),
                name: Some("ClaudeA".into()),
            },
            &claude_rev,
            &claude_home,
        )
        .unwrap();
    assert_eq!(claude_v1.config_state, "provider");
    storage::atomic_write(
        &claude_cfg,
        br#"{"env":{"ANTHROPIC_BASE_URL":"https://other.example.invalid","ANTHROPIC_AUTH_TOKEN":"c-other"}}"#,
        None,
    )
    .unwrap();
    let claude_v2 = claude.try_view().unwrap();
    assert_ne!(claude_v2.config_revision, claude_v1.config_revision);
    assert_eq!(claude_v2.config_state, "unsaved");
}
