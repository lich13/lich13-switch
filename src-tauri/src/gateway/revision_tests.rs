use super::*;

#[tokio::test]
async fn resume_checkpoints_do_not_invalidate_editors_for_either_client() {
    let root = tempfile::tempdir().unwrap();
    let codex = Gateway::new(root.path().join("codex-data")).unwrap();
    let claude = codex.companion(root.path().join("claude-data")).unwrap();
    for g in [&codex, &claude] {
        let home = root.path().join(format!("{}-home", g.0.client.name()));
        std::fs::create_dir(&home).unwrap();
        let config = g.0.client.config(&home);
        std::fs::write(&config, if g.0.client == ClientId::Codex {
            "# keep\nmodel_provider='custom'\n[model_providers.custom]\nbase_url='https://a.invalid'\nexperimental_bearer_token='a'\n"
        } else {
            "{\n  \"env\": {\"ANTHROPIC_BASE_URL\":\"https://a.invalid\", \"ANTHROPIC_AUTH_TOKEN\":\"a\"},\n  \"language\":\"中文\"\n}"
        }).unwrap();
        std::fs::write(home.join("auth.json"), "unchanged-auth").unwrap();
        let edit = |e| g.edit(e, &g.view().revision, &home).unwrap();
        for (url, token) in [("https://a.invalid", "a"), ("https://b.invalid", "b")] {
            edit(Edit::SaveProvider {
                quota_access_token: None,
                id: None,
                base_url: url.into(),
                token: token.into(),
                name: None,
            });
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        edit(Edit::Settings {
            settings: Settings {
                port,
                ..Default::default()
            },
        });
        edit(Edit::Mode {
            mode: "auto".into(),
        });
        let stopped = g.view().revision;
        g.start(&stopped, &home).await.unwrap();
        let revision = g.view().revision;
        assert_ne!(revision, stopped);
        let live = std::fs::read(&config).unwrap();
        let providers = g.0.inner.lock().unwrap().store.providers.clone();
        let file_revision = g.0.inner.lock().unwrap().revision.clone();
        for p in [&providers[1], &providers[0], &providers[1]] {
            g.successful_response(p);
            assert_eq!(g.view().revision, revision);
            assert_eq!(std::fs::read(&config).unwrap(), live);
        }
        assert_ne!(g.0.inner.lock().unwrap().revision, file_revision);
        let peer = if g.0.client == ClientId::Codex {
            &claude
        } else {
            &codex
        };
        let peer_revision = peer.view().revision;
        let selected = g.view().selected;
        g.edit(
            Edit::SaveProvider {
                quota_access_token: None,
                id: None,
                base_url: "https://c.invalid/sub".into(),
                token: "c".into(),
                name: None,
            },
            &revision,
            &home,
        )
        .unwrap();
        assert_eq!(g.view().providers.len(), 3);
        assert_eq!(g.view().selected, selected);
        assert_eq!(g.view().mode, "auto");
        assert_eq!(peer.view().revision, peer_revision);
        assert_eq!(std::fs::read(&config).unwrap(), live);
        let running = g.view().revision;
        g.stop().await.unwrap();
        assert_ne!(g.view().revision, running);
        let (_, pair) = takeover::read_for(g.0.client, &home).unwrap();
        assert_eq!(pair.base_url.as_deref(), Some("https://b.invalid/v1"));
        assert_eq!(pair.token.as_deref(), Some("b"));
        assert_eq!(
            std::fs::read(home.join("auth.json")).unwrap(),
            b"unchanged-auth"
        );
    }
}

#[tokio::test]
async fn actual_edits_still_conflict_and_external_checkpoint_writes_are_not_ignored() {
    let (t, g) = fixture(vec!["https://a.invalid".into(), "https://b.invalid".into()]).await;
    let id = g.view().providers[0].id.clone();
    let mut settings = g.view().settings;
    settings.max_retries += 1;
    for change in [
        Edit::RenameProvider {
            id: id.clone(),
            name: "renamed".into(),
        },
        Edit::ConcurrencyProvider {
            id: id.clone(),
            max_concurrency: 3,
        },
        Edit::QueueProvider {
            id: id.clone(),
            queued: false,
        },
        Edit::ModelsProvider {
            id: id.clone(),
            allowed_models: Some(vec!["m".into()]),
        },
        Edit::Reorder {
            ids: g
                .view()
                .providers
                .iter()
                .rev()
                .map(|p| p.id.clone())
                .collect(),
        },
        Edit::Settings { settings },
        Edit::Mode {
            mode: "auto".into(),
        },
    ] {
        let old = g.view().revision;
        g.edit(change, &old, t.path()).unwrap();
        assert_ne!(old, g.view().revision);
        assert_eq!(
            g.edit(Edit::Reset { id: id.clone() }, &old, t.path())
                .err()
                .unwrap()
                .code,
            "CONFLICT"
        );
        let fresh = g.view().revision;
        g.edit(Edit::Reset { id: id.clone() }, &fresh, t.path())
            .unwrap();
        assert_eq!(g.view().revision, fresh);
    }
    // Even a disk edit to excluded metadata must still fail the file transaction check.
    let file = t.path().join("gateway.json");
    let mut value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    value["resume"] = serde_json::Value::Null;
    let external = serde_json::to_vec(&value).unwrap();
    std::fs::write(&file, &external).unwrap();
    let before = g.view().revision;
    let err = g
        .edit(
            Edit::RenameProvider {
                id,
                name: "must not overwrite".into(),
            },
            &before,
            t.path(),
        )
        .err()
        .unwrap();
    assert_eq!(err.code, "CONFLICT");
    assert_eq!(g.view().revision, before);
    assert_eq!(std::fs::read(file).unwrap(), external);
}
