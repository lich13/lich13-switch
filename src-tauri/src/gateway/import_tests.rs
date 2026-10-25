use super::*;

async fn settle(g: &Gateway) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while g.view().providers.iter().any(|p| p.active_requests != 0) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("fixture request did not settle");
}

fn import_link(base_url: &str, token: &str, display_name: &str) -> Edit {
    Edit::ImportLink {
        protocol: crate::gateway::provider_protocol::Protocol::Responses,
        model: None,
        base_url: base_url.into(),
        token: token.into(),
        display_name: display_name.into(),
    }
}

#[tokio::test]
async fn import_link_only_adds_and_keeps_the_existing_or_empty_selection() {
    for urls in [vec![], vec!["https://original.example.invalid/v1".into()]] {
        let (t, g) = fixture(urls).await;
        let before = g.view();
        let config = std::fs::read(t.path().join("config.toml")).unwrap();
        let auth = std::fs::read(t.path().join("auth.json")).unwrap();
        let view = g
            .edit(
                import_link(
                    "https://imported.example.invalid/v1",
                    "import-fixture-token",
                    "链接供应商",
                ),
                &before.revision,
                t.path(),
            )
            .unwrap();
        assert_eq!(view.providers.len(), before.providers.len() + 1);
        assert_eq!(view.selected, before.selected);
        assert_eq!(view.config_provider, before.config_provider);
        assert!(!view.running);
        let imported = view.providers.last().unwrap();
        assert_eq!(imported.name, "链接供应商");
        assert_eq!(imported.base_url, "https://imported.example.invalid/v1");
        assert_eq!(imported.health.requests, 0);
        assert!(!serde_json::to_string(&view)
            .unwrap()
            .contains("import-fixture-token"));
        assert_eq!(std::fs::read(t.path().join("config.toml")).unwrap(), config);
        assert_eq!(std::fs::read(t.path().join("auth.json")).unwrap(), auth);
        drop(g);
        let reopened = Gateway::new(t.path().to_path_buf()).unwrap().view();
        assert_eq!(reopened.selected, before.selected);
        assert_eq!(reopened.providers.len(), before.providers.len() + 1);
        assert_eq!(reopened.providers.last().unwrap().name, "链接供应商");
    }
}

#[tokio::test]
async fn running_gateway_import_keeps_config_auth_and_the_selected_upstream() {
    let original = server(|_| async { Response::new(full("original-upstream")) }).await;
    let imported_hits = Arc::new(AtomicUsize::new(0));
    let hits = imported_hits.clone();
    let imported = server(move |_| {
        hits.fetch_add(1, Ordering::SeqCst);
        async { Response::new(full("imported-upstream")) }
    })
    .await;
    let (t, g) = fixture(vec![format!("http://127.0.0.1:{original}/v1")]).await;
    start(&g, &t).await;
    let before = g.view();
    let config = std::fs::read(t.path().join("config.toml")).unwrap();
    let auth = std::fs::read(t.path().join("auth.json")).unwrap();
    let view = g
        .edit(
            import_link(
                &format!("http://127.0.0.1:{imported}/v1"),
                "import-fixture-token",
                "新链接供应商",
            ),
            &before.revision,
            t.path(),
        )
        .unwrap();
    assert!(view.running);
    assert_eq!(view.selected, before.selected);
    assert_eq!(view.mode, before.mode);
    assert_eq!(view.providers.len(), 2);
    assert_eq!(std::fs::read(t.path().join("config.toml")).unwrap(), config);
    assert_eq!(std::fs::read(t.path().join("auth.json")).unwrap(), auth);
    let response = request(&g, "/v1/responses", vec![], vec![]).await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.into_body().collect().await.unwrap().to_bytes(),
        "original-upstream"
    );
    settle(&g).await;
    assert_eq!(imported_hits.load(Ordering::SeqCst), 0);
    let view = g.view();
    assert_eq!(view.last_successful, before.selected);
    assert_eq!(view.providers[1].health.requests, 0);
    assert!(view.providers.iter().all(|p| p.active_requests == 0));
    assert_eq!(std::fs::read(t.path().join("config.toml")).unwrap(), config);
    assert_eq!(std::fs::read(t.path().join("auth.json")).unwrap(), auth);
    g.stop().await.unwrap();
}

#[tokio::test]
async fn duplicate_url_and_key_are_rejected_without_changing_the_store() {
    let (t, g) = fixture(vec!["https://duplicate.example.invalid/v1/".into()]).await;
    let before = g.view();
    let stored = std::fs::read(t.path().join("gateway.json")).unwrap();
    let error = g
        .edit(
            import_link(
                "https://duplicate.example.invalid/v1",
                "upstream-fixture-token",
                "重复链接",
            ),
            &before.revision,
            t.path(),
        )
        .err()
        .unwrap();
    assert_eq!(error.code, "DUPLICATE");
    assert_eq!(g.view().revision, before.revision);
    assert_eq!(g.view().providers.len(), 1);
    assert_eq!(g.view().selected, before.selected);
    assert_eq!(
        std::fs::read(t.path().join("gateway.json")).unwrap(),
        stored
    );
    let accepted = g
        .edit(
            import_link(
                "https://duplicate.example.invalid/v1",
                "different-fixture-token",
                "同域不同凭据",
            ),
            &before.revision,
            t.path(),
        )
        .unwrap();
    assert_eq!(accepted.providers.len(), 2);
    assert_eq!(accepted.selected, before.selected);
}

#[tokio::test]
async fn stale_import_revision_is_rejected_without_overwriting_newer_state() {
    let (t, g) = fixture(vec!["https://original.example.invalid/v1".into()]).await;
    let stale = g.view().revision;
    update(
        &g,
        &t,
        Edit::RenameProvider {
            id: g.view().providers[0].id.clone(),
            name: "较新名称".into(),
        },
    );
    let before = g.view();
    let stored = std::fs::read(t.path().join("gateway.json")).unwrap();
    let error = g
        .edit(
            import_link(
                "https://stale-import.example.invalid/v1",
                "stale-fixture-token",
                "过期确认",
            ),
            &stale,
            t.path(),
        )
        .err()
        .unwrap();
    assert_eq!(error.code, "CONFLICT");
    assert_eq!(g.view().revision, before.revision);
    assert_eq!(g.view().providers.len(), 1);
    assert_eq!(g.view().providers[0].name, "较新名称");
    assert_eq!(g.view().selected, before.selected);
    assert_eq!(
        std::fs::read(t.path().join("gateway.json")).unwrap(),
        stored
    );
}
