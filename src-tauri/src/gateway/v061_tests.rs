use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
async fn settle(g: &Gateway) {
    for _ in 0..100 {
        let view = g.view();
        if view.active_connections == 0
            && view.waiting_requests == 0
            && view.providers.iter().all(|p| p.active_requests == 0)
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("fixture request did not finish");
}
#[tokio::test]
async fn incident_rate_limited_p1_waits_then_recovers_while_p2_is_open() {
    let hits = Arc::new(AtomicUsize::new(0));
    let h = hits.clone();
    let p1 = server(move |_| {
        let n = h.fetch_add(1, Ordering::SeqCst);
        async move {
            if n == 0 {
                Response::builder()
                    .status(429)
                    .header("retry-after", "1")
                    .body(full("rate limited"))
                    .unwrap()
            } else {
                Response::new(full("P1 recovered"))
            }
        }
    })
    .await;
    let backup_hits = Arc::new(AtomicUsize::new(0));
    let h = backup_hits.clone();
    let p2 = server(move |_| {
        h.fetch_add(1, Ordering::SeqCst);
        async {
            Response::builder()
                .status(502)
                .body(full("P2 unavailable"))
                .unwrap()
        }
    })
    .await;
    let (t, g) = fixture(vec![
        format!("http://127.0.0.1:{p1}/v1"),
        format!("http://127.0.0.1:{p2}/v1"),
    ])
    .await;
    let mut cfg = g.view().settings;
    cfg.failure_threshold = 1;
    cfg.transient_failure_threshold = 1;
    cfg.capacity_retry_seconds = 1;
    cfg.queue_seconds = 3;
    update(&g, &t, Edit::Settings { settings: cfg });
    update(
        &g,
        &t,
        Edit::Mode {
            mode: "auto".into(),
        },
    );
    let original_auth = std::fs::read(t.path().join("auth.json")).unwrap();
    start(&g, &t).await;
    let first = request(
        &g,
        "/v1/responses",
        br#"{"model":"gpt-test"}"#.to_vec(),
        vec![("content-type", "application/json")],
    )
    .await;
    assert_eq!(first.status(), 200);
    assert_eq!(
        first.into_body().collect().await.unwrap().to_bytes(),
        "P1 recovered"
    );
    settle(&g).await;
    let view = g.view();
    assert_eq!(view.providers[0].health.failures, 0);
    assert_eq!(view.providers[0].health.cooldown_reason, None);
    assert_eq!(
        view.providers[1].health.state,
        super::super::circuit::CircuitState::Open
    );
    let response = request(
        &g,
        "/v1/responses",
        br#"{"model":"gpt-test"}"#.to_vec(),
        vec![("content-type", "application/json")],
    )
    .await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.into_body().collect().await.unwrap().to_bytes(),
        "P1 recovered"
    );
    settle(&g).await;
    let view = g.view();
    let recovered = &view.providers[0];
    assert_eq!(recovered.health.failures, 0);
    assert_eq!(recovered.health.requests, 2);
    assert_eq!(recovered.health.cooldown_reason, None);
    assert!(recovered.health.available);
    assert!(!recovered.health.probe_in_flight);
    assert_eq!(view.last_successful.as_deref(), Some(recovered.id.as_str()));
    assert_eq!(
        view.providers[1].health.state,
        super::super::circuit::CircuitState::Open
    );
    assert_eq!(view.providers[1].health.failures, 1);
    assert_eq!(backup_hits.load(Ordering::SeqCst), 1);
    assert_eq!(hits.load(Ordering::SeqCst), 3);
    assert_eq!(
        std::fs::read(t.path().join("auth.json")).unwrap(),
        original_auth
    );
    g.stop().await.unwrap();
}
#[tokio::test]
async fn cooldown_timeout_returns_503_without_another_upstream_attempt() {
    let hits = Arc::new(AtomicUsize::new(0));
    let h = hits.clone();
    let p1 = server(move |_| {
        h.fetch_add(1, Ordering::SeqCst);
        async {
            Response::builder()
                .status(502)
                .header("retry-after", "120")
                .body(full("upstream-502"))
                .unwrap()
        }
    })
    .await;
    let (t, g) = fixture(vec![format!("http://127.0.0.1:{p1}/v1")]).await;
    let mut settings = g.view().settings;
    settings.max_retries = 0;
    settings.queue_seconds = 1;
    update(&g, &t, Edit::Settings { settings });
    start(&g, &t).await;
    let first = tokio::time::timeout(
        Duration::from_secs(5),
        request(&g, "/v1/responses", vec![], vec![]),
    )
    .await
    .expect("upstream failure response timed out");
    assert_eq!(first.status(), 502);
    assert_eq!(first.headers()["retry-after"], "120");
    assert_eq!(
        first.into_body().collect().await.unwrap().to_bytes(),
        "upstream-502"
    );
    let response = tokio::time::timeout(
        Duration::from_secs(5),
        request(&g, "/v1/responses", vec![], vec![]),
    )
    .await
    .expect("cooldown response timed out");
    assert_eq!(response.status(), 503);
    assert!(response.headers().contains_key("retry-after"));
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert!(String::from_utf8_lossy(&body).contains("PROVIDERS_COOLING_DOWN"));
    settle(&g).await;
    let view = g.view();
    let health = &view.providers[0].health;
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    assert_eq!(health.failures, 1);
    assert_eq!(health.requests, 1);
    assert_eq!(health.state, super::super::circuit::CircuitState::Closed);
    assert_eq!(health.cooldown_reason.as_deref(), Some("retry_after"));
    assert!(!health.available);
    assert!(!health.probe_in_flight);
    g.stop().await.unwrap();
}
#[tokio::test]
async fn failover_preserves_requests_returns_final_response_and_releases_both_slots() {
    let seen = Arc::new(Mutex::new(vec![]));
    let captured = seen.clone();
    let failed = server(move |req| {
        let captured = captured.clone();
        async move {
            let body = req.into_body().collect().await.unwrap().to_bytes();
            captured.lock().unwrap().push((0, body));
            Response::builder()
                .status(502)
                .header("x-upstream", "failed")
                .body(full(r#"{"error":{"code":"server_error"}}"#))
                .unwrap()
        }
    })
    .await;
    let final_body = r#"{"object":"response","id":"resp-final","model":"gpt-fixture","status":"completed","unknown":{"keep":true}}"#;
    let captured = seen.clone();
    let ok = server(move |req| {
        let captured = captured.clone();
        async move {
            let body = req.into_body().collect().await.unwrap().to_bytes();
            captured.lock().unwrap().push((1, body));
            Response::builder()
                .header("content-type", "application/json")
                .header("x-upstream", "final")
                .body(full(final_body))
                .unwrap()
        }
    })
    .await;
    let (t, g) = fixture(vec![
        format!("http://127.0.0.1:{failed}/v1"),
        format!("http://127.0.0.1:{ok}/v1"),
    ])
    .await;
    let mut settings = g.view().settings;
    settings.failure_threshold = 1;
    settings.transient_failure_threshold = 1;
    update(&g, &t, Edit::Settings { settings });
    update(
        &g,
        &t,
        Edit::Mode {
            mode: "auto".into(),
        },
    );
    start(&g, &t).await;
    let req = br#"{"model":"gpt-4o","unknown":{"keep":true}}"#.to_vec();
    let r = request(
        &g,
        "/v1/responses",
        req.clone(),
        vec![("content-type", "application/json")],
    )
    .await;
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["x-upstream"], "final");
    assert_eq!(
        r.into_body().collect().await.unwrap().to_bytes(),
        final_body
    );
    settle(&g).await;
    assert_eq!(
        seen.lock().unwrap().as_slice(),
        &[(0, Bytes::from(req.clone())), (1, Bytes::from(req))]
    );
    let view = g.view();
    assert_eq!(view.providers[0].health.requests, 1);
    assert_eq!(view.providers[0].health.failures, 1);
    assert_eq!(
        view.providers[0].health.state,
        super::super::circuit::CircuitState::Open
    );
    assert_eq!(view.providers[1].health.requests, 1);
    assert_eq!(view.providers[1].health.failures, 0);
    assert_eq!(
        view.last_successful.as_deref(),
        Some(view.providers[1].id.as_str())
    );
    g.stop().await.unwrap();
}
#[tokio::test]
async fn sse_completed_then_client_disconnect_is_success_and_in_band_error_is_neutral() {
    let p=server(|req|async move {
        let event=if req.uri().path().ends_with("failed") {b"data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"server_error\"}}}\n\n".as_slice()}
        else {b"data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-fixture\",\"model\":\"gpt-fixture\"}}\n\n".as_slice()};
        Response::builder().header("content-type","text/event-stream").body(StreamBody::new(async_stream::try_stream!{
            yield Frame::data(Bytes::copy_from_slice(event));
            std::future::pending::<()>().await;
            yield Frame::data(Bytes::new());
        }).map_err(|e:std::io::Error| -> connector::BoxError {Box::new(e)}).boxed_unsync()).unwrap()
    }).await;
    let (t, g) = fixture(vec![format!("http://127.0.0.1:{p}/v1")]).await;
    let mut settings = g.view().settings;
    settings.failure_threshold = 1;
    update(&g, &t, Edit::Settings { settings });
    start(&g, &t).await;
    for (path, terminal, failures, requests) in [
        ("/v1/responses", "response.completed", 0, 1),
        ("/v1/failed", "response.failed", 0, 1),
    ] {
        let mut r = request(&g, path, vec![], vec![]).await;
        assert_eq!(r.status(), 200);
        let frame = r.body_mut().frame().await.unwrap().unwrap();
        let event = frame.data_ref().unwrap().strip_prefix(b"data: ").unwrap();
        let event: serde_json::Value = serde_json::from_slice(event).unwrap();
        assert_eq!(event["type"], terminal);
        assert_eq!(g.view().providers[0].active_requests, 0);
        drop(r);
        settle(&g).await;
        let view = g.view();
        assert_eq!(view.providers[0].health.requests, requests);
        assert_eq!(view.providers[0].health.failures, failures);
        assert!(!view.providers[0].health.probe_in_flight);
    }
    assert_eq!(
        g.view().providers[0].health.state,
        super::super::circuit::CircuitState::Closed
    );
    g.stop().await.unwrap();
}
