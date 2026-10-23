use super::*;
use crate::gateway::admission::{Budget, Rejected};
use crate::gateway::routing::Requirement;

async fn until(f: impl Fn() -> bool) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while !f() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("state did not settle");
}
fn limit(g: &Gateway, t: &tempfile::TempDir, index: usize, max: u32) {
    update(
        g,
        t,
        Edit::ConcurrencyProvider {
            id: g.view().providers[index].id.clone(),
            max_concurrency: max,
        },
    );
}
fn rpm_limit(g: &Gateway, t: &tempfile::TempDir, index: usize, max: u32) {
    update(
        g,
        t,
        Edit::RpmProvider {
            id: g.view().providers[index].id.clone(),
            max_rpm: max,
        },
    );
}
fn routes(g: &Gateway) -> Vec<Route> {
    g.view()
        .providers
        .iter()
        .map(|p| g.route(&p.id).unwrap())
        .collect()
}
async fn slot(g: &Gateway, routes: &[Route]) -> admission::Admission {
    g.0.admission
        .acquire(routes, false, 100, &mut Budget::new(3))
        .await
        .unwrap()
}
async fn held_stream() -> (u16, Arc<tokio::sync::Semaphore>) {
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let r = release.clone();
    let port = server(move |_| {
        let r = r.clone();
        async move {
            let stream = async_stream::try_stream! {
                yield Frame::data(Bytes::from_static(b"data: first\n\n"));
                let _permit = r.acquire().await.unwrap();
                yield Frame::data(Bytes::from_static(b"data: last\n\n"));
            };
            Response::builder()
                .header("content-type", "text/event-stream")
                .body(
                    StreamBody::new(stream)
                        .map_err(|e: std::io::Error| -> connector::BoxError { e.into() })
                        .boxed_unsync(),
                )
                .unwrap()
        }
    })
    .await;
    (port, release)
}
#[tokio::test]
async fn capacity_spills_over_and_returns_to_priority_without_circuit_failures() {
    let (a, release) = held_stream().await;
    let b = server(|_| async { Response::new(full("backup")) }).await;
    let (t, g) = fixture(vec![
        format!("http://127.0.0.1:{a}"),
        format!("http://127.0.0.1:{b}"),
    ])
    .await;
    limit(&g, &t, 0, 1);
    update(
        &g,
        &t,
        Edit::Mode {
            mode: "auto".into(),
        },
    );
    start(&g, &t).await;
    let original = std::fs::read(t.path().join("config.toml")).unwrap();
    let first = request(&g, "/v1/responses", vec![], vec![]).await;
    assert_eq!(g.view().providers[0].active_requests, 1);
    let editor_revision = g.view().revision;
    let second = request(&g, "/v1/responses", vec![], vec![]).await;
    assert_eq!(
        second.into_body().collect().await.unwrap().to_bytes(),
        "backup"
    );
    assert_eq!(g.view().providers[0].health.failures, 0);
    assert_eq!(g.view().revision, editor_revision);
    let selected = g.view().selected;
    g.edit(
        Edit::SaveProvider {
            quota_access_token: None,
            id: None,
            base_url: "https://new.invalid/v1".into(),
            token: "fixture-new".into(),
            name: None,
        },
        &editor_revision,
        t.path(),
    )
    .unwrap();
    assert_eq!(g.view().selected, selected);
    assert_eq!(g.view().mode, "auto");
    assert_eq!(g.view().providers[0].active_requests, 1);
    let after_add = g.view().revision;
    release.add_permits(1);
    first.into_body().collect().await.unwrap();
    until(|| g.view().providers[0].active_requests == 0).await;
    assert_eq!(g.view().revision, after_add);
    let next = request(&g, "/v1/responses", vec![], vec![]).await;
    assert!(next
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .starts_with(b"data: first"));
    assert_eq!(
        std::fs::read(t.path().join("config.toml")).unwrap(),
        original
    );
    assert_eq!(
        std::fs::read(t.path().join("auth.json")).unwrap(),
        b"unchanged-auth"
    );
    g.stop().await.unwrap();
}

#[tokio::test]
async fn rpm_window_counts_upstream_attempts_without_touching_circuit_health() {
    let p = server(|_| async { Response::new(full("ok")) }).await;
    let (t, g) = fixture(vec![format!("http://127.0.0.1:{p}")]).await;
    rpm_limit(&g, &t, 0, 1);
    start(&g, &t).await;
    let routes = routes(&g);
    let mut first = slot(&g, &routes).await;
    first.commit_rpm().unwrap();
    drop(first);
    let mut budget = Budget::new(0);
    let result =
        g.0.admission
            .acquire_for_immediate(
                &routes,
                false,
                100,
                &mut budget,
                &Requirement::Resource,
                true,
            )
            .await;
    assert!(matches!(result, Err(Rejected::RateLimited(seconds)) if seconds > 0));
    let provider = &g.view().providers[0];
    assert_eq!(provider.max_rpm, 1);
    assert_eq!(provider.rpm_used, 1);
    assert!(provider.rpm_limited);
    assert_eq!(provider.health.failures, 0);
    assert_eq!(provider.health.state, circuit::CircuitState::Closed);
    g.stop().await.unwrap();
}

#[tokio::test]
async fn http_rpm_limit_waits_in_place_instead_of_spilling_to_the_next_provider() {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = calls.clone();
    let p1 = server(move |_| {
        let seen = seen.clone();
        async move {
            seen.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Response::new(full("primary"))
        }
    })
    .await;
    let p2 = server(|_| async { Response::new(full("backup")) }).await;
    let (t, g) = fixture(vec![
        format!("http://127.0.0.1:{p1}"),
        format!("http://127.0.0.1:{p2}"),
    ])
    .await;
    rpm_limit(&g, &t, 0, 1);
    update(
        &g,
        &t,
        Edit::Settings {
            settings: Settings {
                queue_seconds: 1,
                ..g.view().settings
            },
        },
    );
    update(
        &g,
        &t,
        Edit::Mode {
            mode: "auto".into(),
        },
    );
    start(&g, &t).await;
    let first = request(&g, "/v1/responses", vec![], vec![]).await;
    assert_eq!(first.status(), 200);
    assert_eq!(
        first.into_body().collect().await.unwrap().to_bytes(),
        "primary"
    );
    {
        let second = request(&g, "/v1/responses", vec![], vec![]);
        tokio::pin!(second);
        // The ordinary queue timeout must not end an RPM budget wait.
        assert!(
            tokio::time::timeout(Duration::from_millis(1_200), second.as_mut())
                .await
                .is_err()
        );
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);
        assert_eq!(g.view().providers[0].health.failures, 0);
        assert_eq!(g.view().waiting_requests, 1);
        assert_eq!(g.view().providers[1].active_requests, 0);
    }
    g.stop().await.unwrap();
}

#[tokio::test]
async fn rpm_reservations_are_persisted_across_gateway_restart() {
    let p = server(|_| async { Response::new(full("ok")) }).await;
    let (t, g) = fixture(vec![format!("http://127.0.0.1:{p}")]).await;
    rpm_limit(&g, &t, 0, 2);
    start(&g, &t).await;
    let routes = routes(&g);
    let mut first = slot(&g, &routes).await;
    first.commit_rpm().unwrap();
    drop(first);
    let raw = std::fs::read(t.path().join("rpm-window.json")).unwrap();
    assert!(String::from_utf8_lossy(&raw).contains("providers"));
    let second = Gateway::new(t.path().to_path_buf()).unwrap();
    second.import_initial(t.path()).unwrap();
    assert_eq!(second.view().providers[0].rpm_used, 1);
    assert_eq!(second.view().providers[0].max_rpm, 2);
    g.stop().await.unwrap();
}

#[tokio::test]
async fn rpm_windows_are_isolated_between_codex_and_claude() {
    let (t, codex, claude, home) =
        super::claude_v080::claude_fixture("https://shared.example.test", "fixture-key").await;
    codex
        .edit(
            Edit::SaveProvider {
                quota_access_token: None,
                id: None,
                base_url: "https://shared.example.test".into(),
                token: "fixture-key".into(),
                name: None,
            },
            &codex.view().revision,
            &t.path().join("codex-home"),
        )
        .unwrap();
    let codex_id = codex.view().providers[0].id.clone();
    codex
        .edit(
            Edit::RpmProvider {
                id: codex_id,
                max_rpm: 1,
            },
            &codex.view().revision,
            &t.path().join("codex-home"),
        )
        .unwrap();
    let claude_id = claude.view().providers[0].id.clone();
    claude
        .edit(
            Edit::RpmProvider {
                id: claude_id,
                max_rpm: 1,
            },
            &claude.view().revision,
            &home,
        )
        .unwrap();

    let codex_providers = codex.0.inner.lock().unwrap().store.providers.clone();
    let claude_providers = claude.0.inner.lock().unwrap().store.providers.clone();
    codex.0.admission.configure(&codex_providers, true);
    claude.0.admission.configure(&claude_providers, true);

    let mut codex_slot = slot(&codex, &routes(&codex)).await;
    codex_slot.commit_rpm().unwrap();
    drop(codex_slot);

    let mut claude_slot = slot(&claude, &routes(&claude)).await;
    claude_slot.commit_rpm().unwrap();
    drop(claude_slot);

    assert_eq!(codex.view().providers[0].rpm_used, 1);
    assert_eq!(claude.view().providers[0].rpm_used, 1);
    assert!(t.path().join("data/rpm-window.json").exists());
    assert!(t.path().join("data/claude/rpm-window.json").exists());
}
#[tokio::test]
async fn queue_is_fifo_bounded_cancelable_and_does_not_consume_retry_budget() {
    let (t, g) = fixture(vec!["https://a.invalid".into(), "https://b.invalid".into()]).await;
    limit(&g, &t, 0, 1);
    limit(&g, &t, 1, 1);
    start(&g, &t).await;
    let r = routes(&g);
    let held = slot(&g, &r[..1]).await;
    let scheduler = g.0.admission.clone();
    let first_routes = r[..1].to_vec();
    let first = tokio::spawn(async move {
        scheduler
            .acquire(&first_routes, false, 1, &mut Budget::new(10))
            .await
    });
    until(|| g.view().waiting_requests == 1).await;
    assert!(matches!(
        g.0.admission
            .acquire(&r[..1], false, 1, &mut Budget::new(3))
            .await,
        Err(Rejected::Full)
    ));
    let independent = slot(&g, &r[1..]).await;
    drop(independent);
    first.abort();
    let _ = first.await;
    until(|| g.view().waiting_requests == 0).await;
    let mut waiters = vec![];
    for _ in 0..3 {
        let scheduler = g.0.admission.clone();
        let rs = r[..1].to_vec();
        waiters.push(tokio::spawn(async move {
            scheduler.acquire(&rs, false, 3, &mut Budget::new(10)).await
        }));
        until(|| g.view().waiting_requests == waiters.len()).await;
    }
    drop(held);
    for task in waiters {
        let held = task.await.unwrap().unwrap();
        assert_eq!(g.view().providers[0].active_requests, 1);
        drop(held);
    }
    until(|| g.view().waiting_requests == 0).await;
    assert!(g
        .view()
        .providers
        .iter()
        .all(|p| p.health.requests == 0 && p.health.failures == 0));
    g.stop().await.unwrap();
}
#[tokio::test]
async fn dynamic_limits_keep_slots_across_route_edits_and_do_not_oversubscribe() {
    let (t, g) = fixture(vec!["https://a.invalid".into()]).await;
    limit(&g, &t, 0, 2);
    start(&g, &t).await;
    let r = routes(&g);
    let held1 = slot(&g, &r).await;
    let held2 = slot(&g, &r).await;
    limit(&g, &t, 0, 1);
    update(
        &g,
        &t,
        Edit::SaveProvider {
            quota_access_token: None,
            id: Some(r[0].provider.id.clone()),
            base_url: "https://changed.invalid".into(),
            token: "new-fixture".into(),
            name: None,
        },
    );
    assert_eq!(g.view().providers[0].active_requests, 2);
    let scheduler = g.0.admission.clone();
    let newer = routes(&g);
    let wait = tokio::spawn(async move {
        scheduler
            .acquire(&newer, false, 100, &mut Budget::new(10))
            .await
    });
    until(|| g.view().waiting_requests == 1).await;
    drop(held1);
    assert!(!wait.is_finished());
    drop(held2);
    let occupied = wait.await.unwrap().unwrap();
    let peak = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut jobs = vec![];
    for _ in 0..32 {
        let g = g.clone();
        let peak = peak.clone();
        jobs.push(tokio::spawn(async move {
            let rs = routes(&g);
            let permit = slot(&g, &rs).await;
            peak.fetch_max(g.view().providers[0].active_requests, Ordering::Relaxed);
            tokio::task::yield_now().await;
            drop(permit);
        }));
    }
    drop(occupied);
    for task in jobs {
        task.await.unwrap();
    }
    assert_eq!(peak.load(Ordering::Relaxed), 1);
    g.stop().await.unwrap();
}
#[tokio::test]
async fn panel_edits_preserve_live_slots_files_and_persist_queue_priority() {
    let a = server(|_| async { Response::new(full("a")) }).await;
    let b = server(|_| async { Response::new(full("b")) }).await;
    let c = server(|_| async { Response::new(full("c")) }).await;
    let (t, g) = fixture(vec![
        format!("http://127.0.0.1:{a}"),
        format!("http://127.0.0.1:{b}"),
        format!("http://127.0.0.1:{c}"),
    ])
    .await;
    let ids: Vec<_> = g.view().providers.iter().map(|p| p.id.clone()).collect();
    update(
        &g,
        &t,
        Edit::Mode {
            mode: "auto".into(),
        },
    );
    update(
        &g,
        &t,
        Edit::ModelsProvider {
            id: ids[2].clone(),
            allowed_models: Some(vec!["model-c".into()]),
        },
    );
    start(&g, &t).await;
    let config = std::fs::read(t.path().join("config.toml")).unwrap();
    let auth = std::fs::read(t.path().join("auth.json")).unwrap();
    let r = routes(&g);
    let held1 = slot(&g, &r[..1]).await;
    let held2 = slot(&g, &r[..1]).await;
    let stale = g.view().revision;
    limit(&g, &t, 0, 1);
    let order = vec![ids[2].clone(), ids[0].clone(), ids[1].clone()];
    assert_eq!(
        g.edit(Edit::Reorder { ids: order.clone() }, &stale, t.path())
            .err()
            .unwrap()
            .code,
        "CONFLICT"
    );
    update(&g, &t, Edit::Reorder { ids: order.clone() });
    update(
        &g,
        &t,
        Edit::SaveProvider {
            quota_access_token: None,
            id: Some(ids[0].clone()),
            base_url: g.view().providers[1].base_url.clone(),
            token: String::new(),
            name: None,
        },
    );
    assert_eq!(g.view().providers[1].active_requests, 2);
    assert_eq!(g.view().providers[1].max_concurrency, 1);
    assert_eq!(g.view().selected.as_ref(), Some(&ids[0]));
    assert_eq!(g.view().mode, "auto");
    drop((held1, held2));
    for (model, expected) in [("model-a", "a"), ("model-c", "c")] {
        let response = request(
            &g,
            "/v1/responses",
            format!("{{\"model\":\"{model}\"}}").into_bytes(),
            vec![("content-type", "application/json")],
        )
        .await;
        assert_eq!(
            response.into_body().collect().await.unwrap().to_bytes(),
            expected
        );
    }
    update(
        &g,
        &t,
        Edit::QueueProvider {
            id: ids[0].clone(),
            queued: false,
        },
    );
    let response = request(
        &g,
        "/v1/responses",
        br#"{"model":"model-a"}"#.to_vec(),
        vec![("content-type", "application/json")],
    )
    .await;
    assert_eq!(
        response.into_body().collect().await.unwrap().to_bytes(),
        "b"
    );
    assert_eq!(std::fs::read(t.path().join("config.toml")).unwrap(), config);
    assert_eq!(std::fs::read(t.path().join("auth.json")).unwrap(), auth);
    g.stop().await.unwrap();
    let stopped_config = std::fs::read(t.path().join("config.toml")).unwrap();
    update(
        &g,
        &t,
        Edit::ConcurrencyProvider {
            id: ids[0].clone(),
            max_concurrency: 100000,
        },
    );
    assert_eq!(
        std::fs::read(t.path().join("config.toml")).unwrap(),
        stopped_config
    );
    drop(g);
    let restored = Gateway::new(t.path().to_path_buf()).unwrap().view();
    assert_eq!(
        restored
            .providers
            .iter()
            .map(|p| p.id.clone())
            .collect::<Vec<_>>(),
        order
    );
    assert!(!restored.providers[1].queued);
    assert_eq!(restored.providers[1].max_concurrency, 100000);
    assert_eq!(restored.selected.as_ref(), Some(&ids[0]));
    assert_eq!(std::fs::read(t.path().join("auth.json")).unwrap(), auth);
}
#[tokio::test]
async fn capacity_timeout_and_stop_release_waiters_and_never_poison_health() {
    let (t, g) = fixture(vec!["https://a.invalid".into()]).await;
    limit(&g, &t, 0, 1);
    start(&g, &t).await;
    let rs = routes(&g);
    let held = slot(&g, &rs).await;
    assert!(matches!(
        g.0.admission
            .acquire(&rs, true, 1, &mut Budget::new(0))
            .await,
        Err(Rejected::Timeout)
    ));
    let g2 = g.clone();
    let waiting = tokio::spawn(async move {
        g2.0.admission
            .acquire(&rs, true, 1, &mut Budget::new(10))
            .await
    });
    until(|| g.view().waiting_requests == 1).await;
    g.stop().await.unwrap();
    assert!(matches!(waiting.await.unwrap(), Err(Rejected::Stopped)));
    drop(held);
    assert_eq!(g.view().waiting_requests, 0);
    assert_eq!(g.view().providers[0].health.requests, 0);
}
#[tokio::test]
async fn http_capacity_429_and_client_disconnect_release_the_slot() {
    let (port, _) = held_stream().await;
    let (t, g) = fixture(vec![format!("http://127.0.0.1:{port}")]).await;
    limit(&g, &t, 0, 1);
    update(
        &g,
        &t,
        Edit::Settings {
            settings: Settings {
                queue_seconds: 1,
                max_waiting: 1,
                ..g.view().settings
            },
        },
    );
    start(&g, &t).await;
    let (r, sender, task) =
        super::tests::request_with_reset(&g, "/v1/responses", vec![], vec![]).await;
    let second = request(&g, "/v1/responses", vec![], vec![]).await;
    assert_eq!(second.status(), 429);
    assert_eq!(second.headers()["retry-after"], "5");
    super::tests::reset_and_drop(task, sender, r).await;
    until(|| g.view().providers[0].active_requests == 0).await;
    g.stop().await.unwrap();
}
#[tokio::test]
async fn resume_restores_intent_and_current_pair_but_rejects_external_credentials() {
    let (t, g) = fixture(vec!["https://a.invalid".into(), "https://b.invalid".into()]).await;
    let auth = std::fs::read(t.path().join("auth.json")).unwrap();
    start(&g, &t).await;
    let second = g.view().providers[1].id.clone();
    update(&g, &t, Edit::Select { id: second });
    g.stop_for_exit().await.unwrap();
    drop(g);
    let path = t.path().join("config.toml");
    let original = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, format!("{original}\n# external harmless edit\n")).unwrap();
    let g = Gateway::new(t.path().to_path_buf()).unwrap();
    g.resume(t.path()).await.unwrap();
    assert!(g.view().running);
    g.stop_for_exit().await.unwrap();
    drop(g);
    let saved = std::fs::read_to_string(&path).unwrap();
    assert!(saved.contains("# external harmless edit"));
    std::fs::write(
        &path,
        saved.replace("https://b.invalid", "https://outside.invalid"),
    )
    .unwrap();
    let g = Gateway::new(t.path().to_path_buf()).unwrap();
    assert_eq!(
        g.resume(t.path()).await.unwrap_err().code,
        "RESUME_CONFLICT"
    );
    assert!(!g.view().running);
    assert!(std::fs::read_to_string(&path)
        .unwrap()
        .contains("https://outside.invalid"));
    assert_eq!(std::fs::read(t.path().join("auth.json")).unwrap(), auth);
    g.stop().await.unwrap();
    let next = Gateway::new(t.path().to_path_buf()).unwrap();
    next.resume(t.path()).await.unwrap();
    assert!(!next.view().running);
}
#[tokio::test]
async fn crash_recovery_keeps_auto_mode_last_success_as_resume_fingerprint() {
    let (t, g) = fixture(vec!["https://a.invalid".into(), "https://b.invalid".into()]).await;
    update(
        &g,
        &t,
        Edit::Mode {
            mode: "auto".into(),
        },
    );
    start(&g, &t).await;
    g.successful_response(&routes(&g)[1].provider);
    // Stop the listener without the exit handler, as in a process crash.
    g.0.registry
        .listener
        .deactivate(ClientId::Codex)
        .await
        .unwrap();
    drop(g);
    let next = Gateway::new(t.path().to_path_buf()).unwrap();
    assert!(std::fs::read_to_string(t.path().join("config.toml"))
        .unwrap()
        .contains("https://b.invalid"));
    next.resume(t.path()).await.unwrap();
    assert!(next.view().running);
    next.stop().await.unwrap();
}

pub(super) async fn response_ws_server(
) -> (u16, Arc<tokio::sync::Semaphore>, Arc<Mutex<Vec<Vec<u8>>>>) {
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let seen = Arc::new(Mutex::new(vec![]));
    let permits = release.clone();
    let observed = seen.clone();
    let port = server(move |mut request| {
        let permits = permits.clone(); let observed = observed.clone();
        async move {
            let (response, upgrade) = yawc::WebSocket::upgrade_with_options(&mut request, yawc::Options::default().with_balanced_compression()).unwrap();
            tokio::spawn(async move {
                let mut ws = upgrade.await.unwrap();
                while let Some(frame) = ws.next().await {
                    if frame.opcode() == yawc::OpCode::Close { break; }
                    if frame.opcode().is_control() { continue; }
                    let value: serde_json::Value = serde_json::from_slice(frame.payload()).unwrap();
                    if value["type"] == "response.create" {
                        observed.lock().unwrap().push(frame.payload().to_vec());
                        ws.send(yawc::Frame::text("{\"type\":\"response.created\",\"response\":{\"id\":\"resp-fixture\"}}".to_string())).await.unwrap();
                        permits.acquire().await.unwrap().forget();
                        ws.send(yawc::Frame::binary(frame.into_payload())).await.unwrap();
                        ws.send(yawc::Frame::text("{\"type\":\"response.completed\",\"response\":{\"id\":\"resp-fixture\",\"unknown\":42}}".to_string())).await.unwrap();
                    } else {
                        ws.send(frame).await.unwrap();
                    }
                }
            });
            response.map(|_| replay::empty())
        }
    }).await;
    (port, release, seen)
}

pub(super) async fn responses_client(g: &Gateway) -> yawc::TcpWebSocket {
    let token = g.0.inner.lock().unwrap().store.local_token.clone();
    yawc::WebSocket::connect(
        format!(
            "ws://127.0.0.1:{}/v1/responses?future=keep",
            g.view().settings.port
        )
        .parse()
        .unwrap(),
    )
    .with_options(yawc::Options::default().with_balanced_compression())
    .with_request(yawc::HttpRequest::builder().header("authorization", format!("Bearer {token}")))
    .await
    .unwrap()
}
async fn ws_next(ws: &mut yawc::TcpWebSocket) -> yawc::Frame {
    tokio::time::timeout(Duration::from_secs(3), ws.next())
        .await
        .unwrap()
        .unwrap()
}
#[tokio::test]
async fn responses_websocket_releases_each_turn_preserves_compressed_payload_and_spills_new_sessions(
) {
    let (a, ra, seen_a) = response_ws_server().await;
    let (b, rb, seen_b) = response_ws_server().await;
    let (t, g) = fixture(vec![
        format!("http://127.0.0.1:{a}"),
        format!("http://127.0.0.1:{b}"),
    ])
    .await;
    limit(&g, &t, 0, 1);
    limit(&g, &t, 1, 1);
    update(
        &g,
        &t,
        Edit::Mode {
            mode: "auto".into(),
        },
    );
    start(&g, &t).await;
    let mut first = responses_client(&g).await;
    let payload = format!(
        "{{ \"type\" : \"response.create\", \"unknown\":\"{}\", \"model\":\"unchanged\" }}",
        "中文 raw ".repeat(500)
    );
    first
        .send(yawc::Frame::text(payload.clone()))
        .await
        .unwrap();
    assert!(ws_next(&mut first)
        .await
        .as_str()
        .contains("response.created"));
    assert_eq!(g.view().providers[0].active_requests, 1);
    let mut second = responses_client(&g).await;
    second
        .send(yawc::Frame::text(payload.clone()))
        .await
        .unwrap();
    assert!(ws_next(&mut second)
        .await
        .as_str()
        .contains("response.created"));
    assert_eq!(g.view().providers[1].active_requests, 1);
    ra.add_permits(1);
    assert_eq!(ws_next(&mut first).await.payload(), payload.as_bytes());
    let completed: serde_json::Value =
        serde_json::from_slice(ws_next(&mut first).await.payload()).unwrap();
    assert_eq!(completed["type"], "response.completed");
    assert_eq!(completed["response"]["id"], "resp-fixture");
    until(|| g.view().providers[0].active_requests == 0).await;
    assert_eq!(g.view().providers[0].health.requests, 1);
    // An idle connection must not reserve provider capacity; the next turn reacquires it.
    first
        .send(yawc::Frame::text(payload.clone()))
        .await
        .unwrap();
    assert!(ws_next(&mut first)
        .await
        .as_str()
        .contains("response.created"));
    assert_eq!(g.view().providers[0].active_requests, 1);
    ra.add_permits(1);
    assert_eq!(ws_next(&mut first).await.payload(), payload.as_bytes());
    let completed: serde_json::Value =
        serde_json::from_slice(ws_next(&mut first).await.payload()).unwrap();
    assert_eq!(completed["type"], "response.completed");
    rb.add_permits(1);
    assert_eq!(ws_next(&mut second).await.payload(), payload.as_bytes());
    let completed: serde_json::Value =
        serde_json::from_slice(ws_next(&mut second).await.payload()).unwrap();
    assert_eq!(completed["type"], "response.completed");
    until(|| g.view().providers.iter().all(|p| p.active_requests == 0)).await;
    assert_eq!(
        seen_a.lock().unwrap().as_slice(),
        &[payload.as_bytes(), payload.as_bytes()]
    );
    assert_eq!(seen_b.lock().unwrap().as_slice(), &[payload.as_bytes()]);
    first.send(yawc::Frame::ping("control-ping")).await.unwrap();
    let pong = ws_next(&mut first).await;
    assert_eq!(pong.opcode(), yawc::OpCode::Pong);
    assert_eq!(pong.payload().as_ref(), b"control-ping");
    let view = g.view();
    assert_eq!(view.waiting_requests, 0);
    assert_eq!(view.providers[0].health.requests, 2);
    assert_eq!(view.providers[1].health.requests, 1);
    assert!(view
        .providers
        .iter()
        .all(|p| { p.health.failures == 0 && p.health.available && !p.health.probe_in_flight }));
    first
        .send(yawc::Frame::close(1000.into(), "done"))
        .await
        .unwrap();
    drop(first);
    drop(second);
    until(|| g.view().active_connections == 0).await;
    let view = g.view();
    assert_eq!(view.providers[0].health.requests, 2);
    assert_eq!(view.providers[1].health.requests, 1);
    assert!(view
        .providers
        .iter()
        .all(|p| p.active_requests == 0 && p.health.failures == 0));
    g.stop().await.unwrap();
}
#[tokio::test]
async fn responses_websocket_fragmented_messages_and_busy_pinned_turn_close_1013() {
    let (a, release, seen) = response_ws_server().await;
    let (t, g) = fixture(vec![format!("http://127.0.0.1:{a}")]).await;
    limit(&g, &t, 0, 1);
    update(
        &g,
        &t,
        Edit::Settings {
            settings: Settings {
                queue_seconds: 1,
                ..g.view().settings
            },
        },
    );
    start(&g, &t).await;
    use tokio_tungstenite::tungstenite::{
        client::IntoClientRequest,
        protocol::frame::{
            coding::{Data, OpCode as RawOp},
            Frame as RawFrame,
        },
        Message,
    };
    let mut req = format!("ws://127.0.0.1:{}/v1/responses", g.view().settings.port)
        .into_client_request()
        .unwrap();
    req.headers_mut().insert(
        "authorization",
        format!("Bearer {}", g.0.inner.lock().unwrap().store.local_token)
            .parse()
            .unwrap(),
    );
    let (mut ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    ws.send(Message::Frame(RawFrame::message(
        Bytes::from_static(b"{\"type\":\"response."),
        RawOp::Data(Data::Text),
        false,
    )))
    .await
    .unwrap();
    ws.send(Message::Frame(RawFrame::message(
        Bytes::from_static(b"create\",\"unknown\":true}"),
        RawOp::Data(Data::Continue),
        true,
    )))
    .await
    .unwrap();
    ws.next().await.unwrap().unwrap();
    release.add_permits(1);
    ws.next().await.unwrap().unwrap();
    ws.next().await.unwrap().unwrap();
    until(|| g.view().providers[0].active_requests == 0).await;
    assert_eq!(
        seen.lock().unwrap()[0],
        b"{\"type\":\"response.create\",\"unknown\":true}"
    );
    let held = slot(&g, &routes(&g)).await;
    ws.send(Message::Text("{\"type\":\"response.create\"}".into()))
        .await
        .unwrap();
    let close = tokio::time::timeout(Duration::from_secs(3), ws.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(matches!(close, Message::Close(Some(frame)) if u16::from(frame.code)==1013));
    drop(held);
    until(|| g.view().waiting_requests == 0).await;
    g.stop().await.unwrap();
}
#[tokio::test]
async fn websocket_waiting_disconnect_and_timeout_release_capacity() {
    let (a, _, _) = response_ws_server().await;
    let (t, g) = fixture(vec![format!("http://127.0.0.1:{a}")]).await;
    limit(&g, &t, 0, 1);
    update(
        &g,
        &t,
        Edit::Settings {
            settings: Settings {
                first_byte_seconds: 1,
                idle_seconds: 1,
                ..g.view().settings
            },
        },
    );
    start(&g, &t).await;
    let mut ws = responses_client(&g).await;
    ws.send(yawc::Frame::text(
        "{\"type\":\"response.create\"}".to_string(),
    ))
    .await
    .unwrap();
    ws_next(&mut ws).await;
    let mut waiting = responses_client(&g).await;
    waiting
        .send(yawc::Frame::text(
            "{\"type\":\"response.create\"}".to_string(),
        ))
        .await
        .unwrap();
    until(|| g.view().waiting_requests == 1).await;
    waiting
        .send(yawc::Frame::close(1000.into(), "cancel"))
        .await
        .unwrap();
    drop(waiting);
    until(|| g.view().waiting_requests == 0).await;
    let frame = ws_next(&mut ws).await;
    assert_eq!(frame.close_code().map(u16::from), Some(1013));
    until(|| g.view().providers[0].active_requests == 0).await;
    g.stop().await.unwrap();
}

#[tokio::test]
async fn context_affinity_waits_for_owner_even_with_idle_backup() {
    let (target, release) = held_stream().await;
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let count = calls.clone();
    let backup = server(move |_| {
        let count = count.clone();
        async move {
            count.fetch_add(1, Ordering::Relaxed);
            Response::new(full("backup"))
        }
    })
    .await;
    let (t, g) = fixture(vec![
        format!("http://127.0.0.1:{target}"),
        format!("http://127.0.0.1:{backup}"),
    ])
    .await;
    limit(&g, &t, 0, 1);
    update(
        &g,
        &t,
        Edit::Mode {
            mode: "auto".into(),
        },
    );
    update(
        &g,
        &t,
        Edit::Settings {
            settings: Settings {
                queue_seconds: 1,
                ..g.view().settings
            },
        },
    );
    start(&g, &t).await;
    g.remember("owned", &g.view().providers[0].id);
    let held = request(&g, "/v1/responses", vec![], vec![]).await;
    let pinned = request(
        &g,
        "/v1/responses",
        b"{\"previous_response_id\":\"owned\"}".to_vec(),
        vec![("content-type", "application/json")],
    )
    .await;
    assert_eq!(pinned.status(), 429);
    assert_eq!(calls.load(Ordering::Relaxed), 0);
    release.add_permits(1);
    held.into_body().collect().await.unwrap();
    g.stop().await.unwrap();
}
#[tokio::test]
async fn resume_port_failure_preserves_config_and_legacy_store_defaults() {
    let (t, g) = fixture(vec!["https://a.invalid".into()]).await;
    start(&g, &t).await;
    g.stop_for_exit().await.unwrap();
    let before = std::fs::read(t.path().join("config.toml")).unwrap();
    let port = tokio::net::TcpListener::bind(("127.0.0.1", g.view().settings.port))
        .await
        .unwrap();
    assert_eq!(g.resume(t.path()).await.unwrap_err().code, "PORT");
    assert_eq!(std::fs::read(t.path().join("config.toml")).unwrap(), before);
    drop(port);
    g.resume(t.path()).await.unwrap();
    g.stop().await.unwrap();
    drop(g);
    let path = t.path().join("gateway.json");
    let mut old: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    old.as_object_mut().unwrap().remove("resume");
    old["settings"]
        .as_object_mut()
        .unwrap()
        .remove("queueSeconds");
    old["settings"]
        .as_object_mut()
        .unwrap()
        .remove("maxWaiting");
    old["providers"][0]
        .as_object_mut()
        .unwrap()
        .remove("maxConcurrency");
    std::fs::write(&path, serde_json::to_vec(&old).unwrap()).unwrap();
    let next = Gateway::new(t.path().to_owned()).unwrap();
    assert_eq!(next.view().settings.queue_seconds, 30);
    assert_eq!(next.view().settings.max_waiting, 100);
    assert_eq!(next.view().providers[0].max_concurrency, 0);
    next.resume(t.path()).await.unwrap();
    assert!(!next.view().running);
}

#[tokio::test]
async fn websocket_model_rules_recheck_each_turn_and_keep_context_provider() {
    let (a, ra, seen_a) = response_ws_server().await;
    let (b, rb, seen_b) = response_ws_server().await;
    let (t, g) = fixture(vec![
        format!("http://127.0.0.1:{a}"),
        format!("http://127.0.0.1:{b}"),
    ])
    .await;
    let ids: Vec<_> = g.view().providers.iter().map(|p| p.id.clone()).collect();
    update(
        &g,
        &t,
        Edit::ModelsProvider {
            id: ids[0].clone(),
            allowed_models: Some(vec!["a".into()]),
        },
    );
    update(
        &g,
        &t,
        Edit::ModelsProvider {
            id: ids[1].clone(),
            allowed_models: Some(vec!["b".into()]),
        },
    );
    update(
        &g,
        &t,
        Edit::Mode {
            mode: "auto".into(),
        },
    );
    start(&g, &t).await;
    let mut ws = responses_client(&g).await;
    let create = r#"{"type":"response.create","model":"b","future":42}"#;
    ws.send(yawc::Frame::text(create)).await.unwrap();
    ws_next(&mut ws).await;
    assert_eq!(g.view().providers[0].active_requests, 0);
    assert_eq!(g.view().providers[1].active_requests, 1);
    rb.add_permits(1);
    assert_eq!(ws_next(&mut ws).await.payload(), create.as_bytes());
    ws_next(&mut ws).await;
    until(|| g.view().providers[1].active_requests == 0).await;
    ws.send(yawc::Frame::text(
        r#"{"type":"response.create","previous_response_id":"resp-fixture"}"#,
    ))
    .await
    .unwrap();
    ws_next(&mut ws).await;
    rb.add_permits(1);
    ws_next(&mut ws).await;
    ws_next(&mut ws).await;
    until(|| g.view().providers[1].active_requests == 0).await;
    update(
        &g,
        &t,
        Edit::ModelsProvider {
            id: ids[1].clone(),
            allowed_models: Some(vec!["c".into()]),
        },
    );
    ws.send(yawc::Frame::text(create)).await.unwrap();
    let closed = ws_next(&mut ws).await;
    assert_eq!(closed.opcode(), yawc::OpCode::Close);
    assert_eq!(&closed.payload()[..2], &1008u16.to_be_bytes());
    assert!(seen_a.lock().unwrap().is_empty());
    assert_eq!(seen_b.lock().unwrap().len(), 2);
    assert_eq!(g.view().providers[1].health.failures, 0);
    drop(ws);
    drop(ra);
    g.stop().await.unwrap();
}
