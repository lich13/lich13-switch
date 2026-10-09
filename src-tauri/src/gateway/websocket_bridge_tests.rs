use super::*;
use std::io::{self, Write};

async fn settled(f: impl Fn() -> bool) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while !f() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
}
async fn next(ws: &mut yawc::TcpWebSocket) -> yawc::Frame {
    tokio::time::timeout(Duration::from_secs(3), ws.next())
        .await
        .unwrap()
        .unwrap()
}
fn bridge(g: &Gateway, t: &tempfile::TempDir, index: usize) {
    update(
        g,
        t,
        Edit::WebsocketProvider {
            id: g.view().providers[index].id.clone(),
            supports_websocket: false,
        },
    );
}
fn automatic(g: &Gateway, t: &tempfile::TempDir) {
    update(
        g,
        t,
        Edit::Mode {
            mode: "auto".into(),
        },
    );
    let mut settings = g.view().settings;
    settings.capacity_retry_seconds = 1;
    settings.first_byte_seconds = 1;
    settings.idle_seconds = 1;
    update(g, t, Edit::Settings { settings });
}
const CREATE: &str = r#"{"type":"response.create","model":"exact-model","tools":[{"type":"function","name":"test"}],"reasoning":{"effort":"high"},"input":[{"type":"message","role":"user","content":[{"type":"input_image","image_url":"data:image/png;base64,aA=="}]}],"unknown":{"large":9007199254740993}}"#;
const COMPLETED: &str = r#"{"type":"response.completed","response":{"id":"fixture-response","status":"completed","model":"exact-model"}}"#;
fn sse(value: &str) -> Response<WireBody> {
    Response::builder()
        .header("content-type", "text/event-stream; charset=utf-8")
        .body(full(format!("data: {value}\n\n")))
        .unwrap()
}

#[tokio::test]
async fn bridge_storage_defaults_conflicts_persistence_and_config_contract() {
    let (t, g) = fixture(vec!["https://fixture.invalid/v1".into()]).await;
    assert!(g.view().providers[0].supports_websocket);
    let config = std::fs::read(t.path().join("config.toml")).unwrap();
    let auth = std::fs::read(t.path().join("auth.json")).unwrap();
    let old = g.view().revision;
    let id = g.view().providers[0].id.clone();
    bridge(&g, &t, 0);
    assert!(g
        .edit(
            Edit::WebsocketProvider {
                id: id.clone(),
                supports_websocket: true
            },
            &old,
            t.path()
        )
        .is_err());
    assert!(!g.view().providers[0].supports_websocket);
    drop(g);
    let g = Gateway::new(t.path().into()).unwrap();
    assert!(!g.view().providers[0].supports_websocket);
    drop(g);
    let path = t.path().join("gateway.json");
    let mut stored: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    stored["providers"][0]
        .as_object_mut()
        .unwrap()
        .remove("supportsWebsocket");
    std::fs::write(&path, serde_json::to_vec(&stored).unwrap()).unwrap();
    let g = Gateway::new(t.path().into()).unwrap();
    assert!(g.view().providers[0].supports_websocket);
    start(&g, &t).await;
    let running = std::fs::read(t.path().join("config.toml")).unwrap();
    bridge(&g, &t, 0);
    assert_eq!(
        std::fs::read(t.path().join("config.toml")).unwrap(),
        running
    );
    g.stop().await.unwrap();
    assert_eq!(std::fs::read(t.path().join("config.toml")).unwrap(), config);
    assert_eq!(std::fs::read(t.path().join("auth.json")).unwrap(), auth);
}

#[tokio::test]
async fn bridge_compressed_fragmented_sse_preserves_events_headers_path_and_payload() {
    let events = [
        r#"{"type":"response.created","response":{"id":"fixture-response"}}"#,
        r#"{"type":"response.reasoning_summary_text.delta","delta":"reason"}"#,
        r#"{"type":"response.function_call_arguments.delta","delta":"{\"q\":1}"}"#,
        r#"{"type":"response.image_generation_call.partial_image","partial_image_b64":"aA=="}"#,
        r#"{"type":"response.future","unknown":[1,2,3]}"#,
        COMPLETED,
    ];
    let raw = events
        .iter()
        .map(|e| format!("event: preserved\r\ndata: {e}\r\n\r\n"))
        .collect::<String>()
        .into_bytes();
    let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    gzip.write_all(&raw).unwrap();
    let mut deflate = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
    deflate.write_all(&raw).unwrap();
    let cases = [
        ("identity", raw.clone()),
        ("gzip", gzip.finish().unwrap()),
        ("deflate", deflate.finish().unwrap()),
        ("zstd", zstd::stream::encode_all(raw.as_slice(), 1).unwrap()),
    ];
    for (encoding, bytes) in cases {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let observed = seen.clone();
        let port = server(move |req| {
            let observed = observed.clone();
            let bytes = bytes.clone();
            async move {
                assert_eq!(req.method(), hyper::Method::POST);
                assert_eq!(req.uri().to_string(), "/deployment/v1/responses?future=keep");
                assert_eq!(req.headers()["authorization"], "Bearer upstream-fixture-token");
                assert!(!req.headers().contains_key("sec-websocket-key"));
                assert!(!req.headers().contains_key("upgrade"));
                let body = req.into_body().collect().await.unwrap().to_bytes();
                observed.lock().unwrap().push(body);
                let stream = async_stream::try_stream! {
                    for part in bytes.chunks(7) { yield Frame::data(Bytes::copy_from_slice(part)); tokio::task::yield_now().await; }
                };
                Response::builder().header("content-type", "text/event-stream").header("content-encoding", encoding).body(StreamBody::new(stream).map_err(|e: io::Error| -> connector::BoxError { e.into() }).boxed_unsync()).unwrap()
            }
        }).await;
        let (t, g) = fixture(vec![format!("http://127.0.0.1:{port}/deployment/v1")]).await;
        bridge(&g, &t, 0);
        start(&g, &t).await;
        let mut ws = concurrency::responses_client(&g).await;
        ws.send(yawc::Frame::text(CREATE)).await.unwrap();
        for expected in events {
            assert_eq!(
                next(&mut ws).await.payload().as_ref(),
                expected.as_bytes(),
                "{encoding}"
            );
        }
        settled(|| g.view().providers[0].active_requests == 0).await;
        let actual: serde_json::Value = serde_json::from_slice(&seen.lock().unwrap()[0]).unwrap();
        let mut expected: serde_json::Value = serde_json::from_str(CREATE).unwrap();
        expected.as_object_mut().unwrap().remove("type");
        expected["stream"] = true.into();
        assert_eq!(actual, expected);
        drop(ws);
        g.stop().await.unwrap();
    }
}

#[tokio::test]
async fn bridge_failover_only_before_output_and_incompatibility_is_neutral() {
    for (status, body, content_type, native) in [
        (404, "missing", "text/plain", true),
        (426, "upgrade unsupported", "text/plain", true),
        (405, "missing", "text/plain", false),
        (200, "{}", "application/json", false),
        (502, "failed", "text/plain", false),
        (429, "limited", "text/plain", false),
        (
            200,
            "data: {\"type\":\"error\",\"error\":{\"code\":\"rate_limit_exceeded\"}}\n\n",
            "text/event-stream",
            false,
        ),
    ] {
        let bad = server(move |_| async move {
            Response::builder()
                .status(status)
                .header("content-type", content_type)
                .body(full(body))
                .unwrap()
        })
        .await;
        let ok = server(|_| async { sse(COMPLETED) }).await;
        let (t, g) = fixture(vec![
            format!("http://127.0.0.1:{bad}/v1"),
            format!("http://127.0.0.1:{ok}/v1"),
        ])
        .await;
        if !native {
            bridge(&g, &t, 0);
        }
        bridge(&g, &t, 1);
        automatic(&g, &t);
        start(&g, &t).await;
        let mut ws = concurrency::responses_client(&g).await;
        ws.send(yawc::Frame::text(CREATE)).await.unwrap();
        assert_eq!(
            next(&mut ws).await.payload().as_ref(),
            COMPLETED.as_bytes(),
            "status {status}"
        );
        settled(|| g.view().providers.iter().all(|p| p.active_requests == 0)).await;
        if matches!(status, 404 | 405 | 426) || content_type == "application/json" {
            assert_eq!(g.view().providers[0].health.failures, 0);
        }
        drop(ws);
        g.stop().await.unwrap();
    }
    let first =
        server(|_| async { sse(r#"{"type":"response.created","response":{"id":"pinned"}}"#) })
            .await;
    let hits = Arc::new(AtomicUsize::new(0));
    let count = hits.clone();
    let backup = server(move |_| {
        count.fetch_add(1, Ordering::SeqCst);
        async { sse(COMPLETED) }
    })
    .await;
    let (t, g) = fixture(vec![
        format!("http://127.0.0.1:{first}"),
        format!("http://127.0.0.1:{backup}"),
    ])
    .await;
    bridge(&g, &t, 0);
    bridge(&g, &t, 1);
    automatic(&g, &t);
    start(&g, &t).await;
    let mut ws = concurrency::responses_client(&g).await;
    ws.send(yawc::Frame::text(CREATE)).await.unwrap();
    assert!(next(&mut ws).await.as_str().contains("response.created"));
    assert_eq!(next(&mut ws).await.close_code().map(u16::from), Some(1013));
    assert_eq!(hits.load(Ordering::SeqCst), 0);
    assert_eq!(g.view().providers[0].health.failures, 1);
    drop(ws);
    g.stop().await.unwrap();
}

#[tokio::test]
async fn bridge_all_unsupported_closes_1008_and_does_not_poison_health() {
    let native =
        server(|_| async { Response::builder().status(404).body(full("")).unwrap() }).await;
    let http = server(|_| async { Response::builder().status(405).body(full("")).unwrap() }).await;
    let (t, g) = fixture(vec![
        format!("http://127.0.0.1:{native}"),
        format!("http://127.0.0.1:{http}"),
    ])
    .await;
    bridge(&g, &t, 1);
    automatic(&g, &t);
    start(&g, &t).await;
    let mut ws = concurrency::responses_client(&g).await;
    ws.send(yawc::Frame::text(CREATE)).await.unwrap();
    let frame = next(&mut ws).await;
    assert_eq!(frame.close_code().map(u16::from), Some(1008));
    assert!(String::from_utf8_lossy(frame.payload()).contains("WS_UNSUPPORTED"));
    assert!(g.view().providers.iter().all(|p| p.health.failures == 0));
    drop(ws);
    g.stop().await.unwrap();
}

#[tokio::test]
async fn bridge_cancel_before_first_event_and_after_output_releases_http_and_allows_next_turn() {
    for output in [false, true] {
        let hits = Arc::new(AtomicUsize::new(0));
        let count = hits.clone();
        let port=server(move |_| {
            let n=count.fetch_add(1,Ordering::SeqCst);
            async move {
                if n>0 { return sse(COMPLETED); }
                let stream=async_stream::try_stream! {
                    if output { yield Frame::data(Bytes::from_static(b"data: {\"type\":\"response.created\",\"response\":{\"id\":\"cancelled\"}}\n\n")); }
                    std::future::pending::<()>().await;
                };
                Response::builder().header("content-type","text/event-stream").body(StreamBody::new(stream).map_err(|e:io::Error|->connector::BoxError{e.into()}).boxed_unsync()).unwrap()
            }
        }).await;
        let (t, g) = fixture(vec![format!("http://127.0.0.1:{port}")]).await;
        bridge(&g, &t, 0);
        start(&g, &t).await;
        let mut ws = concurrency::responses_client(&g).await;
        ws.send(yawc::Frame::text(CREATE)).await.unwrap();
        settled(|| hits.load(Ordering::SeqCst) == 1).await;
        if output {
            assert!(next(&mut ws).await.as_str().contains("response.created"));
        }
        ws.send(yawc::Frame::ping(b"ping".to_vec())).await.unwrap();
        ws.send(yawc::Frame::text(r#"{"type":"response.cancel"}"#))
            .await
            .unwrap();
        loop {
            let f = next(&mut ws).await;
            if f.opcode() == yawc::OpCode::Pong {
                continue;
            }
            assert!(f.as_str().contains("response.cancelled"));
            break;
        }
        settled(|| g.view().providers[0].active_requests == 0).await;
        assert_eq!(g.view().providers[0].health.failures, 0);
        ws.send(yawc::Frame::text(CREATE)).await.unwrap();
        assert_eq!(next(&mut ws).await.payload().as_ref(), COMPLETED.as_bytes());
        drop(ws);
        g.stop().await.unwrap();
    }
}

#[tokio::test]
async fn bridge_multiturn_stays_pinned_keeps_previous_id_and_rechecks_model_rules() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let observed = seen.clone();
    let first = server(move |req| {
        let observed = observed.clone();
        async move {
            let body = req.into_body().collect().await.unwrap().to_bytes();
            observed.lock().unwrap().push(body);
            sse(COMPLETED)
        }
    })
    .await;
    let hits = Arc::new(AtomicUsize::new(0));
    let count = hits.clone();
    let second = server(move |_| {
        count.fetch_add(1, Ordering::SeqCst);
        async { sse(COMPLETED) }
    })
    .await;
    let (t, g) = fixture(vec![
        format!("http://127.0.0.1:{first}"),
        format!("http://127.0.0.1:{second}"),
    ])
    .await;
    bridge(&g, &t, 0);
    bridge(&g, &t, 1);
    automatic(&g, &t);
    start(&g, &t).await;
    let mut ws = concurrency::responses_client(&g).await;
    ws.send(yawc::Frame::text(CREATE)).await.unwrap();
    assert_eq!(next(&mut ws).await.payload().as_ref(), COMPLETED.as_bytes());
    settled(|| g.view().providers[0].active_requests == 0).await;
    update(
        &g,
        &t,
        Edit::Reorder {
            ids: g
                .view()
                .providers
                .iter()
                .rev()
                .map(|p| p.id.clone())
                .collect(),
        },
    );
    let continuation = r#"{"type":"response.create","model":"exact-model","previous_response_id":"fixture-response","input":[{"type":"function_call_output","call_id":"call_1","output":"ok"}]}"#;
    ws.send(yawc::Frame::text(continuation)).await.unwrap();
    assert_eq!(next(&mut ws).await.payload().as_ref(), COMPLETED.as_bytes());
    assert_eq!(hits.load(Ordering::SeqCst), 0);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&seen.lock().unwrap()[1]).unwrap()
            ["previous_response_id"],
        "fixture-response"
    );
    let owner = g.view().providers[1].id.clone();
    update(
        &g,
        &t,
        Edit::ModelsProvider {
            id: owner,
            allowed_models: Some(vec!["other".into()]),
        },
    );
    ws.send(yawc::Frame::text(continuation)).await.unwrap();
    assert_eq!(next(&mut ws).await.close_code().map(u16::from), Some(1008));
    assert_eq!(seen.lock().unwrap().len(), 2);
    drop(ws);
    g.stop().await.unwrap();
}

#[tokio::test]
async fn bridge_capacity_wait_retries_sole_provider_without_opening() {
    let hits = Arc::new(AtomicUsize::new(0));
    let count = hits.clone();
    let port = server(move |_| {
        let n = count.fetch_add(1, Ordering::SeqCst);
        async move {
            if n == 0 {
                Response::builder()
                    .status(429)
                    .header("retry-after", "0")
                    .body(full("limited"))
                    .unwrap()
            } else {
                sse(COMPLETED)
            }
        }
    })
    .await;
    let (t, g) = fixture(vec![format!("http://127.0.0.1:{port}")]).await;
    bridge(&g, &t, 0);
    automatic(&g, &t);
    start(&g, &t).await;
    let mut ws = concurrency::responses_client(&g).await;
    let began = Instant::now();
    ws.send(yawc::Frame::text(CREATE)).await.unwrap();
    settled(|| !g.view().capacity_retries.is_empty()).await;
    assert_eq!(g.view().providers[0].active_requests, 0);
    assert_eq!(next(&mut ws).await.payload().as_ref(), COMPLETED.as_bytes());
    assert!(began.elapsed() >= Duration::from_millis(950));
    assert_eq!(hits.load(Ordering::SeqCst), 2);
    assert_eq!(
        g.view().providers[0].health.state,
        circuit::CircuitState::Closed
    );
    drop(ws);
    g.stop().await.unwrap();
}

#[tokio::test]
async fn native_capacity_is_turn_local_retryable_and_post_output_is_never_replayed() {
    const CREATED: &str = r#"{"type":"response.created","response":{"id":"already-output"}}"#;
    for (warmup, output, code) in [
        (false, false, "server_is_overloaded"),
        (true, false, "slow_down"),
        (true, false, "usage_limit_reached"),
        (false, true, "server_is_overloaded"),
        (false, false, "cyber_policy"),
    ] {
        let capacity = !output && code != "cyber_policy";
        let error = format!(r#"{{"type":"error","error":{{"code":"{code}"}}}}"#);
        let upstream_error = error.clone();
        let hits = Arc::new(AtomicUsize::new(0));
        let count = hits.clone();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let observed = seen.clone();
        let port = server(move |mut request| {
            let error = upstream_error.clone();
            let count = count.clone();
            let observed = observed.clone();
            async move {
                let (response, upgrade) = yawc::WebSocket::upgrade(&mut request).unwrap();
                tokio::spawn(async move {
                    let mut ws = upgrade.await.unwrap();
                    while let Some(frame) = ws.next().await {
                        if frame.opcode().is_control() {
                            continue;
                        }
                        observed.lock().unwrap().push(frame.payload().to_vec());
                        let n = count.fetch_add(1, Ordering::SeqCst);
                        if warmup && n == 0 {
                            ws.send(yawc::Frame::text(COMPLETED)).await.unwrap();
                            continue;
                        }
                        if capacity && n >= usize::from(warmup) + 3 {
                            ws.send(yawc::Frame::text(COMPLETED)).await.unwrap();
                            continue;
                        }
                        if output {
                            ws.send(yawc::Frame::text(CREATED)).await.unwrap();
                        }
                        ws.send(yawc::Frame::text(error.clone())).await.unwrap();
                    }
                });
                response.map(|_| replay::empty())
            }
        })
        .await;
        let (t, g) = fixture(vec![format!("http://127.0.0.1:{port}")]).await;
        automatic(&g, &t);
        let mut settings = g.view().settings;
        settings.max_retries = 0;
        update(&g, &t, Edit::Settings { settings });
        update(
            &g,
            &t,
            Edit::RpmProvider {
                id: g.view().providers[0].id.clone(),
                max_rpm: 20,
            },
        );
        start(&g, &t).await;
        let mut ws = concurrency::responses_client(&g).await;
        ws.send(yawc::Frame::text(CREATE)).await.unwrap();
        if warmup {
            assert_eq!(next(&mut ws).await.payload().as_ref(), COMPLETED.as_bytes());
            settled(|| g.view().providers[0].active_requests == 0).await;
            assert_eq!(hits.load(Ordering::SeqCst), 1);
            assert_eq!(g.view().providers[0].rpm_used, 1);
            assert_eq!(g.view().providers[0].health.failures, 0);
            assert!(g.view().websocket_retries.is_empty());
            ws.send(yawc::Frame::text(CREATE)).await.unwrap();
        }
        if output {
            assert_eq!(next(&mut ws).await.payload().as_ref(), CREATED.as_bytes());
        }
        if capacity {
            settled(|| !g.view().websocket_retries.is_empty()).await;
            let view = g.view();
            assert_eq!(view.providers[0].active_requests, 0, "{code}");
            assert_eq!(view.providers[0].health.failures, 0, "{code}");
            let received = tokio::time::timeout(Duration::from_secs(10), ws.next())
                .await
                .expect("capacity retries did not complete within ten seconds")
                .expect("downstream closed before capacity recovery");
            assert_eq!(received.payload().as_ref(), COMPLETED.as_bytes(), "{code}");
        } else {
            assert_eq!(next(&mut ws).await.payload().as_ref(), error.as_bytes());
        }
        settled(|| g.view().providers[0].active_requests == 0).await;
        let expected_requests = usize::from(warmup) + if capacity { 4 } else { 1 };
        assert_eq!(hits.load(Ordering::SeqCst), expected_requests, "{code}");
        let view = g.view();
        assert_eq!(view.providers[0].rpm_used, expected_requests, "{code}");
        assert_eq!(view.providers[0].active_requests, 0, "{code}");
        assert_eq!(view.providers[0].health.failures, 0, "{code}");
        assert!(view.websocket_retries.is_empty(), "{code}");
        {
            let requests = seen.lock().unwrap();
            assert_eq!(requests.len(), expected_requests, "{code}");
            for request in requests.iter() {
                assert_eq!(request.as_slice(), CREATE.as_bytes(), "{code}");
            }
        }
        drop(ws);
        g.stop().await.unwrap();
    }
}

#[tokio::test]
async fn bridge_timeout_stop_and_disconnect_drop_slots_without_replaying_output() {
    for kind in ["first_timeout", "idle_timeout", "stop", "disconnect"] {
        let hits = Arc::new(AtomicUsize::new(0));
        let count = hits.clone();
        let port=server(move |_| {
            count.fetch_add(1,Ordering::SeqCst);
            async move {
                let stream=async_stream::try_stream! {
                    if kind != "first_timeout" { yield Frame::data(Bytes::from_static(b"data: {\"type\":\"response.created\",\"response\":{\"id\":\"active\"}}\n\n")); }
                    std::future::pending::<()>().await;
                };
                Response::builder().header("content-type","text/event-stream").body(StreamBody::new(stream).map_err(|e:io::Error|->connector::BoxError{e.into()}).boxed_unsync()).unwrap()
            }
        }).await;
        let (t, g) = fixture(vec![format!("http://127.0.0.1:{port}")]).await;
        bridge(&g, &t, 0);
        automatic(&g, &t);
        let mut settings = g.view().settings;
        settings.max_retries = 0;
        update(&g, &t, Edit::Settings { settings });
        start(&g, &t).await;
        let mut ws = concurrency::responses_client(&g).await;
        ws.send(yawc::Frame::text(CREATE)).await.unwrap();
        settled(|| hits.load(Ordering::SeqCst) == 1).await;
        if kind != "first_timeout" {
            assert!(next(&mut ws).await.as_str().contains("response.created"));
        }
        if kind == "stop" {
            g.stop().await.unwrap();
            assert_eq!(next(&mut ws).await.close_code().map(u16::from), Some(1012));
        } else if kind != "disconnect" {
            assert_eq!(next(&mut ws).await.close_code().map(u16::from), Some(1013));
        }
        drop(ws);
        settled(|| g.view().providers[0].active_requests == 0).await;
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        if matches!(kind, "stop" | "disconnect") {
            assert_eq!(g.view().providers[0].health.failures, 0);
        }
        g.stop().await.unwrap();
    }
}

#[tokio::test]
async fn bridge_large_first_message_does_not_bypass_the_selected_transport() {
    let length = 17 * 1024 * 1024;
    let port = server(move |req| async move {
        assert_eq!(req.method(), hyper::Method::POST);
        let bytes = req.into_body().collect().await.unwrap().to_bytes();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["input"].as_str().unwrap().len(), length);
        sse(COMPLETED)
    })
    .await;
    let (t, g) = fixture(vec![format!("http://127.0.0.1:{port}")]).await;
    bridge(&g, &t, 0);
    start(&g, &t).await;
    let mut ws = concurrency::responses_client(&g).await;
    let payload=serde_json::json!({"type":"response.create","model":"exact-model","input":"x".repeat(length)}).to_string();
    ws.send(yawc::Frame::text(payload)).await.unwrap();
    let frame = tokio::time::timeout(Duration::from_secs(15), ws.next())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(frame.payload().as_ref(), COMPLETED.as_bytes());
    drop(ws);
    g.stop().await.unwrap();
}

#[tokio::test]
async fn native_early_disconnect_exhausts_zero_retries_without_switching_provider() {
    let hits = Arc::new(AtomicUsize::new(0));
    let observed = hits.clone();
    let port = server(move |mut request| {
        let observed = observed.clone();
        async move {
            let (response, upgrade) = yawc::WebSocket::upgrade(&mut request).unwrap();
            tokio::spawn(async move {
                let mut ws = upgrade.await.unwrap();
                while let Some(frame) = ws.next().await {
                    if frame.opcode().is_control() {
                        continue;
                    }
                    observed.fetch_add(1, Ordering::SeqCst);
                    break;
                }
            });
            response.map(|_| replay::empty())
        }
    })
    .await;
    let backup_hits = Arc::new(AtomicUsize::new(0));
    let observed = backup_hits.clone();
    let backup = server(move |_| {
        observed.fetch_add(1, Ordering::SeqCst);
        async { sse(COMPLETED) }
    })
    .await;
    let (t, g) = fixture(vec![
        format!("http://127.0.0.1:{port}"),
        format!("http://127.0.0.1:{backup}"),
    ])
    .await;
    bridge(&g, &t, 1);
    automatic(&g, &t);
    let mut settings = g.view().settings;
    settings.max_retries = 0;
    update(&g, &t, Edit::Settings { settings });
    start(&g, &t).await;
    let mut ws = concurrency::responses_client(&g).await;
    ws.send(yawc::Frame::text(CREATE)).await.unwrap();
    assert_eq!(next(&mut ws).await.close_code().map(u16::from), Some(1013));
    settled(|| g.view().providers[0].active_requests == 0).await;
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    assert_eq!(backup_hits.load(Ordering::SeqCst), 0);
    assert_eq!(g.view().providers[0].health.failures, 1);
    drop(ws);
    g.stop().await.unwrap();
}
