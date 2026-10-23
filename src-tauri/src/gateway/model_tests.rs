use super::*;
use crate::gateway::routing::Requirement;

async fn settle(g: &Gateway) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while g.view().providers.iter().any(|p| p.active_requests != 0) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("fixture request did not settle");
}

fn allow(g: &Gateway, t: &tempfile::TempDir, n: usize, models: Option<Vec<&str>>) {
    update(
        g,
        t,
        Edit::ModelsProvider {
            id: g.view().providers[n].id.clone(),
            allowed_models: models.map(|m| m.into_iter().map(str::to_owned).collect()),
        },
    );
}
async fn body(response: Response<Incoming>) -> Bytes {
    response.into_body().collect().await.unwrap().to_bytes()
}

#[test]
fn provider_display_name_is_trimmed_and_host_is_the_blank_fallback() {
    let mut store = Store::default();
    store
        .edit(
            Edit::SaveProvider {
                quota_access_token: None,
                id: None,
                base_url: "https://api.example.invalid/deployment".into(),
                token: "fixture-token".into(),
                name: Some("  主力供应商  ".into()),
            },
            false,
        )
        .unwrap();
    assert_eq!(store.providers[0].name, "主力供应商");

    store
        .edit(
            Edit::SaveProvider {
                quota_access_token: None,
                id: None,
                base_url: "https://fallback.example.invalid/v1".into(),
                token: "fixture-token-2".into(),
                name: Some("  ".into()),
            },
            false,
        )
        .unwrap();
    assert_eq!(store.providers[1].name, "fallback.example.invalid");

    let invalid = store.edit(
        Edit::SaveProvider {
            quota_access_token: None,
            id: None,
            base_url: "https://invalid.example.invalid".into(),
            token: "fixture-token-3".into(),
            name: Some("x".repeat(121)),
        },
        false,
    );
    assert_eq!(invalid.unwrap_err().code, "NAME");
}

#[tokio::test]
async fn exact_rules_filter_before_attempts_and_preserve_original_json_and_resources() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut urls = Vec::new();
    for n in 0..5 {
        let seen = seen.clone();
        let port = server(move |req| {
            let seen = seen.clone();
            async move {
                let payload = req.into_body().collect().await.unwrap().to_bytes();
                seen.lock().unwrap().push((n, payload.clone()));
                Response::new(full(payload))
            }
        })
        .await;
        urls.push(format!("http://127.0.0.1:{port}"));
    }
    let (t, g) = fixture(urls).await;
    for n in 0..4 {
        allow(&g, &t, n, Some(vec!["other"]));
    }
    allow(&g, &t, 4, Some(vec!["Exact"]));
    update(
        &g,
        &t,
        Edit::Mode {
            mode: "auto".into(),
        },
    );
    start(&g, &t).await;
    let config = std::fs::read(t.path().join("config.toml")).unwrap();
    let payload = b"{ \"model\" : \"Exact\", \"future\" : [3, 7] }";
    assert_eq!(
        body(
            request(
                &g,
                "/v1/responses",
                payload.to_vec(),
                vec![("content-type", "application/json")]
            )
            .await
        )
        .await
        .as_ref(),
        payload
    );
    settle(&g).await;
    assert_eq!(seen.lock().unwrap()[0].0, 4);
    assert_eq!(g.view().providers[4].active_requests, 0);
    assert!(g.view().providers[..4]
        .iter()
        .all(|p| p.health.requests == 0 && p.active_requests == 0));
    let denied = request(
        &g,
        "/v1/responses",
        br#"{"model":"exact"}"#.to_vec(),
        vec![("content-type", "application/json")],
    )
    .await;
    assert_eq!(denied.status(), 400);
    assert!(String::from_utf8(body(denied).await.to_vec())
        .unwrap()
        .contains("MODEL_NOT_ALLOWED"));
    let unknown = request(&g, "/codex/future", vec![0, 255], vec![]).await;
    assert_eq!(unknown.status(), 400);
    assert!(String::from_utf8(body(unknown).await.to_vec())
        .unwrap()
        .contains("MODEL_UNDETERMINED"));
    let resource = request(&g, "/v1/files", vec![0, 255], vec![]).await;
    assert_eq!(body(resource).await.as_ref(), &[0, 255]);
    assert_eq!(std::fs::read(t.path().join("config.toml")).unwrap(), config);
    assert_eq!(
        std::fs::read(t.path().join("auth.json")).unwrap(),
        b"unchanged-auth"
    );
    g.stop().await.unwrap();
}

#[tokio::test]
async fn multipart_compressed_hints_and_manual_or_context_restrictions() {
    let p = server(|r| async {
        Response::new(full(r.into_body().collect().await.unwrap().to_bytes()))
    })
    .await;
    let (t, g) = fixture(vec![
        format!("http://127.0.0.1:{p}"),
        format!("http://127.0.0.1:{p}"),
    ])
    .await;
    allow(&g, &t, 0, Some(vec!["a"]));
    allow(&g, &t, 1, Some(vec!["b"]));
    start(&g, &t).await;
    let denied = request(
        &g,
        "/v1/responses",
        br#"{"model":"b"}"#.to_vec(),
        vec![("content-type", "application/json")],
    )
    .await;
    assert_eq!(denied.status(), 400);
    let multipart = b"--test-boundary\r\nContent-Disposition: form-data; name=\"file\"; filename=\"binary\"\r\n\r\n\0\xff\r\n--test-boundary\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\na\r\n--test-boundary--\r\n";
    let response = request(
        &g,
        "/v1/images/edits",
        multipart.to_vec(),
        vec![(
            "content-type",
            "multipart/form-data; boundary=\"test-boundary\"",
        )],
    )
    .await;
    assert_eq!(response.status(), 200);
    assert_eq!(body(response).await.as_ref(), multipart);
    let payload = br#"{"model":"a","unknown":"keep"}"#;
    use std::io::Write;
    let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    gzip.write_all(payload).unwrap();
    let compressed = gzip.finish().unwrap();
    assert_eq!(
        body(
            request(
                &g,
                "/v1/responses",
                compressed.clone(),
                vec![
                    ("content-type", "application/json"),
                    ("content-encoding", "gzip")
                ]
            )
            .await
        )
        .await
        .as_ref(),
        compressed
    );
    let id = g.view().providers[0].id.clone();
    g.remember_model("context-a", &id, Some("a"));
    update(
        &g,
        &t,
        Edit::Mode {
            mode: "auto".into(),
        },
    );
    let inherited = request(
        &g,
        "/v1/responses",
        br#"{"previous_response_id":"context-a"}"#.to_vec(),
        vec![("content-type", "application/json")],
    )
    .await;
    assert_eq!(inherited.status(), 200);
    body(inherited).await;
    let denied = request(
        &g,
        "/v1/responses",
        br#"{"previous_response_id":"context-a","model":"b"}"#.to_vec(),
        vec![("content-type", "application/json")],
    )
    .await;
    assert_eq!(denied.status(), 400);
    g.stop().await.unwrap();
}

#[tokio::test]
async fn waiting_requests_recheck_rules_without_resetting_occupied_slots() {
    let (t, g) = fixture(vec!["https://example.invalid".into()]).await;
    let id = g.view().providers[0].id.clone();
    update(
        &g,
        &t,
        Edit::ConcurrencyProvider {
            id: id.clone(),
            max_concurrency: 1,
        },
    );
    allow(&g, &t, 0, Some(vec!["a"]));
    start(&g, &t).await;
    let routes = vec![g.route(&id).unwrap()];
    let held =
        g.0.admission
            .acquire_for(
                &routes,
                false,
                100,
                &mut admission::Budget::new(5),
                &Requirement::model(Some("a")),
            )
            .await
            .unwrap();
    let next = g.clone();
    let queued_routes = routes.clone();
    let wait = tokio::spawn(async move {
        next.0
            .admission
            .acquire_for(
                &queued_routes,
                false,
                100,
                &mut admission::Budget::new(5),
                &Requirement::model(Some("a")),
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(2), async {
        while g.view().waiting_requests != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    allow(&g, &t, 0, Some(vec!["b"]));
    assert!(matches!(
        wait.await.unwrap(),
        Err(admission::Rejected::Model)
    ));
    assert_eq!(g.view().providers[0].active_requests, 1);
    assert_eq!(g.view().providers[0].health.requests, 0);
    drop(held);
    assert_eq!(g.view().providers[0].active_requests, 0);
    g.stop().await.unwrap();
}

#[tokio::test]
async fn catalog_is_cached_singleflight_bounded_and_has_no_business_side_effects() {
    let count = Arc::new(AtomicUsize::new(0));
    let hits = count.clone();
    let port = server(move |r| { let hits = hits.clone(); async move {
        assert_eq!(r.uri().path(),"/tenant/v1/models");
        assert_eq!(r.headers()["authorization"],"Bearer upstream-fixture-token");
        hits.fetch_add(1,Ordering::Relaxed); tokio::time::sleep(Duration::from_millis(60)).await;
        Response::new(full(r#"{"data":[{"id":"b"},{"id":"a"},{"id":"a"},{"id":"upstream-fixture-token"}],"ignored":"secret"}"#))
    }}).await;
    let (t, g) = fixture(vec![format!("http://127.0.0.1:{port}/tenant/v1")]).await;
    let id = g.view().providers[0].id.clone();
    let auth = std::fs::read(t.path().join("auth.json")).unwrap();
    let config = std::fs::read(t.path().join("config.toml")).unwrap();
    assert!(g.list_models(&id, false).await.unwrap().models.is_empty());
    assert_eq!(
        count.load(Ordering::Relaxed),
        0,
        "opening a panel only reads cache"
    );
    let (a, b) = tokio::join!(g.list_models(&id, true), g.list_models(&id, true));
    assert_eq!(a.unwrap().models, vec!["a", "b"]);
    assert_eq!(b.unwrap().models, vec!["a", "b"]);
    g.list_models(&id, false).await.unwrap();
    assert_eq!(count.load(Ordering::Relaxed), 1);
    assert_eq!(g.view().providers[0].health.requests, 0);
    assert_eq!(g.view().active_connections, 0);
    assert_eq!(std::fs::read(t.path().join("auth.json")).unwrap(), auth);
    assert_eq!(std::fs::read(t.path().join("config.toml")).unwrap(), config);
}

#[tokio::test]
async fn v02514_new_provider_discovers_once_failed_refresh_keeps_old_and_deleted_result_is_discarded(
) {
    let hits = Arc::new(AtomicUsize::new(0));
    let count = hits.clone();
    let fail = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let failed = fail.clone();
    let port = server(move |_| {
        let count = count.clone();
        let failed = failed.clone();
        async move {
            count.fetch_add(1, Ordering::SeqCst);
            if failed.load(Ordering::SeqCst) {
                Response::builder()
                    .status(500)
                    .body(full("fixture error"))
                    .unwrap()
            } else {
                Response::new(full(r#"{"data":[{"id":"fixture-model"}]}"#))
            }
        }
    })
    .await;
    let (t, g) = fixture(vec![]).await;
    g.0.discover_new_providers.store(true, Ordering::Relaxed);
    update(
        &g,
        &t,
        Edit::SaveProvider {
            quota_access_token: None,
            id: None,
            base_url: format!("http://127.0.0.1:{port}"),
            token: "fixture-local".into(),
            name: None,
        },
    );
    let id = g.view().providers[0].id.clone();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if g.list_models(&id, false)
                .await
                .unwrap()
                .checked_at
                .is_some()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    for _ in 0..3 {
        assert_eq!(
            g.list_models(&id, false).await.unwrap().models,
            vec!["fixture-model"]
        );
    }
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    fail.store(true, Ordering::SeqCst);
    let old = g.list_models(&id, true).await.unwrap();
    assert!(old.error.is_some());
    assert_eq!(old.models, vec!["fixture-model"]);
    assert!(!old.stale);
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let entered = Arc::new(tokio::sync::Notify::new());
    let gate2 = gate.clone();
    let entered2 = entered.clone();
    let blocked = server(move |_| {
        let gate = gate2.clone();
        let entered = entered2.clone();
        async move {
            entered.notify_one();
            let _permit = gate.acquire().await.unwrap();
            Response::new(full(r#"{"data":[{"id":"late"}]}"#))
        }
    })
    .await;
    update(
        &g,
        &t,
        Edit::SaveProvider {
            quota_access_token: None,
            id: Some(id.clone()),
            base_url: format!("http://127.0.0.1:{blocked}"),
            token: "fixture-new".into(),
            name: None,
        },
    );
    assert!(g.list_models(&id, false).await.unwrap().stale);
    let next = g.clone();
    let queried = id.clone();
    let task = tokio::spawn(async move { next.list_models(&queried, true).await });
    entered.notified().await;
    update(&g, &t, Edit::DeleteProvider { id: id.clone() });
    gate.add_permits(1);
    assert_eq!(task.await.unwrap().unwrap_err().code, "STALE");
    let cache = super::super::catalog::Service::new(t.path().join("provider-models.json"));
    assert!(cache.cached(&id, "fixture").models.is_empty());
}

#[tokio::test]
async fn catalog_errors_and_changed_credentials_do_not_expose_raw_responses() {
    for status in [301, 401, 403, 429, 500] {
        let port = server(move |_| async move {
            Response::builder()
                .status(status)
                .header("retry-after", "120")
                .body(full("private-upstream-response"))
                .unwrap()
        })
        .await;
        let (t, g) = fixture(vec![format!("http://127.0.0.1:{port}")]).await;
        let id = g.view().providers[0].id.clone();
        let result = g.list_models(&id, true).await.unwrap();
        assert!(result.error.is_some());
        assert!(!serde_json::to_string(&result)
            .unwrap()
            .contains("private-upstream"));
        if status == 429 {
            assert!(result.retry_at.unwrap() >= quota::now() + 119);
        }
        assert_eq!(g.view().providers[0].health.failures, 0);
        drop(t);
    }
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let entered = Arc::new(tokio::sync::Notify::new());
    let r = gate.clone();
    let e = entered.clone();
    let port = server(move |_| {
        let r = r.clone();
        let e = e.clone();
        async move {
            e.notify_one();
            let _ = r.acquire().await.unwrap();
            Response::new(full(r#"{"data":[{"id":"old-model"}]}"#))
        }
    })
    .await;
    let (t, g) = fixture(vec![format!("http://127.0.0.1:{port}")]).await;
    let id = g.view().providers[0].id.clone();
    let next = g.clone();
    let task_id = id.clone();
    let task = tokio::spawn(async move { next.list_models(&task_id, true).await });
    entered.notified().await;
    update(
        &g,
        &t,
        Edit::SaveProvider {
            quota_access_token: None,
            id: Some(id),
            base_url: format!("http://127.0.0.1:{port}"),
            token: "changed-fixture".into(),
            name: None,
        },
    );
    gate.add_permits(1);
    assert_eq!(task.await.unwrap().unwrap_err().code, "STALE");
}

#[tokio::test]
async fn completed_http_response_keeps_model_affinity_and_health() {
    let port = server(|req| async {
        let _ = req.into_body().collect().await;
        Response::new(full(r#"{"id":"response-normal"}"#))
    })
    .await;
    let (t, g) = fixture(vec![format!("http://127.0.0.1:{port}")]).await;
    allow(&g, &t, 0, Some(vec!["allowed"]));
    start(&g, &t).await;
    let first = request(
        &g,
        "/v1/responses",
        br#"{"model":"allowed"}"#.to_vec(),
        vec![("content-type", "application/json")],
    )
    .await;
    body(first).await;
    assert_eq!(g.view().providers[0].health.requests, 1);
    assert_eq!(g.view().providers[0].health.failures, 0);
    let inherited = request(
        &g,
        "/v1/responses",
        br#"{"previous_response_id":"response-normal"}"#.to_vec(),
        vec![("content-type", "application/json")],
    )
    .await;
    assert_eq!(inherited.status(), 200);
    body(inherited).await;
    let unknown_ws = request(
        &g,
        "/codex/future/socket",
        vec![],
        vec![
            ("upgrade", "websocket"),
            ("connection", "upgrade"),
            ("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ=="),
            ("sec-websocket-version", "13"),
        ],
    )
    .await;
    assert_eq!(unknown_ws.status(), 400);
    assert!(String::from_utf8(body(unknown_ws).await.to_vec())
        .unwrap()
        .contains("MODEL_UNDETERMINED"));
    g.stop().await.unwrap();
}

#[tokio::test]
async fn catalog_limits_response_size_and_request_duration() {
    let port = server(|_| async { Response::new(full(vec![b'x'; 2 * 1024 * 1024 + 1])) }).await;
    let (_t, g) = fixture(vec![format!("http://127.0.0.1:{port}")]).await;
    let id = g.view().providers[0].id.clone();
    assert!(g
        .list_models(&id, true)
        .await
        .unwrap()
        .error
        .unwrap()
        .contains("2 MB"));
    let port = server(|_| async {
        tokio::time::sleep(Duration::from_secs(15)).await;
        Response::new(full("{}"))
    })
    .await;
    let (_t, g) = fixture(vec![format!("http://127.0.0.1:{port}")]).await;
    let id = g.view().providers[0].id.clone();
    assert!(g
        .list_models(&id, true)
        .await
        .unwrap()
        .error
        .unwrap()
        .contains("超时"));
}
