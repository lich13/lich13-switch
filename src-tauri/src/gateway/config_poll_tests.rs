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
    let first_arc = cache
        .read(&path, |_| panic!("expected config_stamp cache hit"))
        .unwrap();
    assert_eq!(first.1, g.read_config(&home).unwrap().revision);
    assert_eq!(first, g.config_stamp(&home, &mut cache).unwrap());
    assert!(Arc::ptr_eq(
        &first_arc,
        &cache
            .read(&path, |_| panic!("expected config_stamp cache hit"))
            .unwrap()
    ));
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
        state.editable_mut().running = true;
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
    // Cached repeat view reuses the same Arc in view_config_cache.codex_bearer.
    let bearer_arc_v1 = codex
        .0
        .view_config_cache
        .lock()
        .unwrap()
        .codex_bearer
        .read(&codex_cfg, |_| panic!("expected codex_bearer cache hit"))
        .unwrap();
    let cached_view = codex.try_view().unwrap();
    assert!(Arc::ptr_eq(
        &bearer_arc_v1,
        &codex
            .0
            .view_config_cache
            .lock()
            .unwrap()
            .codex_bearer
            .read(&codex_cfg, |_| panic!("expected codex_bearer cache hit"))
            .unwrap()
    ));
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
    assert!(claude_v1.config_provider.is_some());
    storage::atomic_write(
        &claude_cfg,
        br#"{"env":{"ANTHROPIC_BASE_URL":"https://other.example.invalid","ANTHROPIC_AUTH_TOKEN":"c-other"}}"#,
        None,
    )
    .unwrap();
    let claude_v2 = claude.try_view().unwrap();
    assert_ne!(claude_v2.config_revision, claude_v1.config_revision);
    assert!(claude_v2.config_provider.is_none());
}

#[test]
fn editable_digest_auto_invalidates_on_editable_mutations_and_preserves_cache_on_resume_checkpoint()
{
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let g = Gateway::new(dir.path().join("data")).unwrap();

    let v0 = g.view().revision;
    {
        let s = g.0.inner.lock().unwrap();
        assert!(s.has_cached_editable_digest());
        assert_eq!(
            s.editable_digest(g.0.client),
            Inner::compute_editable_digest(
                g.0.client,
                &s.store,
                s.running,
                s.home.as_deref(),
                s.upgrade_error.is_some(),
            )
        );
    }

    // Resume checkpoint update preserves the cached editable_digest and view.revision.
    {
        let mut s = g.0.inner.lock().unwrap();
        let rev = s.revision.clone();
        s.save_resume_checkpoint(
            Some(model::Resume {
                home: home.clone(),
                desired: true,
                pair_hash: "fixture-hash".into(),
            }),
            rev,
        );
        assert!(s.has_cached_editable_digest());
    }
    assert_eq!(g.view().revision, v0);

    // Mutating any editable field via editable_mut() clears OnceLock and produces the new revision.
    {
        let mut s = g.0.inner.lock().unwrap();
        s.editable_mut().running = true;
        assert!(!s.has_cached_editable_digest());
    }
    let v_running = g.view().revision;
    assert_ne!(v_running, v0);

    {
        let mut s = g.0.inner.lock().unwrap();
        s.editable_mut().home = Some(home);
        assert!(!s.has_cached_editable_digest());
    }
    let v_home = g.view().revision;
    assert_ne!(v_home, v_running);

    {
        let mut s = g.0.inner.lock().unwrap();
        s.editable_mut().upgrade_error = Some(AppError::new(code::RECOVERY, "fixture"));
        assert!(!s.has_cached_editable_digest());
    }
    let v_err = g.view().revision;
    assert_ne!(v_err, v_home);
    {
        let s = g.0.inner.lock().unwrap();
        assert!(s.has_cached_editable_digest());
        assert_eq!(
            s.editable_digest(g.0.client),
            Inner::compute_editable_digest(
                g.0.client,
                &s.store,
                s.running,
                s.home.as_deref(),
                s.upgrade_error.is_some(),
            )
        );
    }
}

#[test]
fn try_view_config_cache_tracks_codex_api_key_and_grok_edits_and_provider_switches() {
    let dir = tempfile::tempdir().unwrap();
    let codex_home = dir.path().join("codex-api-home");
    let grok_home = dir.path().join("grok-home");
    std::fs::create_dir_all(&codex_home).unwrap();
    std::fs::create_dir_all(&grok_home).unwrap();

    let codex = Gateway::new(dir.path().join("codex-api-data")).unwrap();
    let grok = codex.grok_companion(dir.path().join("grok-data")).unwrap();

    // 1. Codex ApiKey mode: config.toml + auth.json + provider switch cache invalidation.
    let codex_cfg = codex_home.join("config.toml");
    let codex_auth = codex_home.join("auth.json");
    storage::atomic_write(
        &codex_cfg,
        b"model_provider='relay_a'\n[model_providers.relay_a]\nbase_url='https://api-a.example.invalid/v1'\nrequires_openai_auth=true\n[model_providers.relay_b]\nbase_url='https://api-b.example.invalid/v1'\nrequires_openai_auth=true\n",
        None,
    )
    .unwrap();
    storage::atomic_write(
        &codex_auth,
        br#"{"auth_mode":"apikey","OPENAI_API_KEY":"key-one"}"#,
        None,
    )
    .unwrap();
    codex.import_initial(&codex_home).unwrap();
    let v1 = codex.try_view().unwrap();
    assert_eq!(v1.connection_mode, "apiKey");
    assert!(v1.config_provider.is_some());
    assert_eq!(
        v1.config_revision.as_deref(),
        Some(codex_api::read(&codex_home, "relay_a").unwrap().0.as_str())
    );
    // Steady-state cache hit reuses the same Arc pointers in codex_api_toml and codex_api_auth.
    let (toml_arc_v1, auth_arc_v1) = {
        let mut vc = codex.0.view_config_cache.lock().unwrap();
        (
            vc.codex_api_toml
                .1
                .read(&codex_cfg, |_| panic!("expected codex_api_toml cache hit"))
                .unwrap(),
            vc.codex_api_auth
                .read(&codex_auth, |_| panic!("expected codex_api_auth cache hit"))
                .unwrap(),
        )
    };
    assert_eq!(
        codex.try_view().unwrap().config_revision,
        v1.config_revision
    );
    {
        let mut vc = codex.0.view_config_cache.lock().unwrap();
        assert!(Arc::ptr_eq(
            &toml_arc_v1,
            &vc.codex_api_toml
                .1
                .read(&codex_cfg, |_| panic!("expected codex_api_toml cache hit"))
                .unwrap()
        ));
        assert!(Arc::ptr_eq(
            &auth_arc_v1,
            &vc.codex_api_auth
                .read(&codex_auth, |_| panic!("expected codex_api_auth cache hit"))
                .unwrap()
        ));
    }

    // Modifying only auth.json invalidates codex_api_auth and updates config_revision.
    storage::atomic_write(
        &codex_auth,
        br#"{"auth_mode":"apikey","OPENAI_API_KEY":"key-two"}"#,
        None,
    )
    .unwrap();
    let v2 = codex.try_view().unwrap();
    assert_ne!(v2.config_revision, v1.config_revision);
    assert!(v2.config_provider.is_none());

    // Modifying config.toml and switching connection provider invalidates codex_api_toml cache.
    storage::atomic_write(
        &codex_cfg,
        b"model_provider='relay_b'\n[model_providers.relay_a]\nbase_url='https://api-a.example.invalid/v1'\nrequires_openai_auth=true\n[model_providers.relay_b]\nbase_url='https://api-b.example.invalid/v1'\nrequires_openai_auth=true\n",
        None,
    )
    .unwrap();
    {
        let mut s = codex.0.inner.lock().unwrap();
        s.editable_mut().store.connection = codex_api::Connection::ApiKey {
            provider: "relay_b".into(),
        };
    }
    let v3 = codex.try_view().unwrap();
    assert_ne!(v3.config_revision, v2.config_revision);
    assert_eq!(
        v3.config_revision.as_deref(),
        Some(codex_api::read(&codex_home, "relay_b").unwrap().0.as_str())
    );

    // 2. Grok config cache hit & external TOML invalidation.
    let grok_cfg = ClientId::Grok.config(&grok_home);
    storage::atomic_write(
        &grok_cfg,
        b"[models]\ndefault='custom'\n[model.custom]\nmodel='grok-3'\napi_backend='chat_completions'\nbase_url='https://grok-a.example.invalid/v1'\napi_key='grok-tok-a'\n",
        None,
    )
    .unwrap();
    let grok_rev = grok.view().revision;
    let grok_v1 = grok
        .edit(
            Edit::SaveGrokProvider {
                quota_access_token: None,
                id: None,
                base_url: "https://grok-a.example.invalid/v1".into(),
                token: "grok-tok-a".into(),
                name: Some("GrokA".into()),
                protocol: grok_config::Protocol::ChatCompletions,
            },
            &grok_rev,
            &grok_home,
        )
        .unwrap();
    assert!(grok_v1.config_revision.is_some());
    let grok_arc_v1 = grok
        .0
        .view_config_cache
        .lock()
        .unwrap()
        .grok
        .read(&grok_cfg, |_| panic!("expected grok cache hit"))
        .unwrap();
    assert_eq!(
        grok.try_view().unwrap().config_revision,
        grok_v1.config_revision
    );
    assert!(Arc::ptr_eq(
        &grok_arc_v1,
        &grok
            .0
            .view_config_cache
            .lock()
            .unwrap()
            .grok
            .read(&grok_cfg, |_| panic!("expected grok cache hit"))
            .unwrap()
    ));

    storage::atomic_write(
        &grok_cfg,
        b"[models]\ndefault='custom'\n[model.custom]\nmodel='grok-3'\napi_backend='chat_completions'\nbase_url='https://grok-b.example.invalid/v1'\napi_key='grok-tok-b'\n",
        None,
    )
    .unwrap();
    let grok_v2 = grok.try_view().unwrap();
    assert_ne!(grok_v2.config_revision, grok_v1.config_revision);
}
