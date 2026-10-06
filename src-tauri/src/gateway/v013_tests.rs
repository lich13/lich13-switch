use super::*;
use crate::gateway::{protocol::Terminal, upstream_error};
use bytes::Bytes;
use http_body_util::BodyExt;
use hyper::Response;
use std::{
    io::Write,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
};

fn auto(g: &Gateway, t: &tempfile::TempDir, max_retries: usize) {
    update(
        g,
        t,
        Edit::Mode {
            mode: "auto".into(),
        },
    );
    let mut settings = g.view().settings;
    settings.max_retries = max_retries;
    settings.connect_seconds = 1;
    settings.first_byte_seconds = 2;
    settings.total_seconds = 5;
    update(g, t, Edit::Settings { settings });
}

fn model_error(status: u16, message: &str) -> Response<WireBody> {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(full(
            serde_json::json!({
                "error": {"code": "model_not_found", "message": message}
            })
            .to_string(),
        ))
        .unwrap()
}

fn sse(value: &str) -> Response<WireBody> {
    Response::builder()
        .header("content-type", "text/event-stream")
        .body(full(format!("data: {value}\n\n")))
        .unwrap()
}

fn encoded(encoding: &str, raw: &[u8]) -> Vec<u8> {
    match encoding {
        "identity" => raw.to_vec(),
        "gzip" => {
            let mut encoder =
                flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
            encoder.write_all(raw).unwrap();
            encoder.finish().unwrap()
        }
        "deflate" => {
            let mut encoder =
                flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
            encoder.write_all(raw).unwrap();
            encoder.finish().unwrap()
        }
        "zstd" => zstd::stream::encode_all(raw, 1).unwrap(),
        _ => panic!("unknown fixture encoding"),
    }
}

async fn ws_next(ws: &mut yawc::TcpWebSocket) -> yawc::Frame {
    tokio::time::timeout(std::time::Duration::from_secs(3), ws.next())
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

#[tokio::test]
async fn http_200_application_failure_does_not_trip_provider_circuit() {
    let upstream = server(|_| async {
        Response::builder()
            .status(200)
            .header("content-type", "application/json")
            .body(full(
                serde_json::json!({
                    "error": {"code": "server_error", "message": "fixture application failure"}
                })
                .to_string(),
            ))
            .unwrap()
    })
    .await;
    let (t, g) = fixture(vec![format!("http://127.0.0.1:{upstream}/v1")]).await;
    auto(&g, &t, 1);
    start(&g, &t).await;
    let response = request(
        &g,
        "/v1/responses",
        br#"{"model":"exact-model","input":"fixture"}"#.to_vec(),
        vec![("content-type", "application/json")],
    )
    .await;
    assert_eq!(response.status(), 200);
    assert_eq!(g.view().providers[0].health.failures, 0);
    assert_eq!(
        g.view().providers[0].health.state,
        circuit::CircuitState::Closed
    );
    g.stop().await.unwrap();
}

#[tokio::test]
async fn model_errors_from_400_403_404_503_fail_over_without_circuit_breaking() {
    for (status, message) in [
        (400, "model exact-model does not exist"),
        (403, "model exact-model is not allowed to access"),
        (404, "no available channel for model exact-model"),
        (503, "模型无可用渠道"),
    ] {
        let bad_message = message.to_owned();
        let first = server(move |_| {
            let bad_message = bad_message.clone();
            async move { model_error(status, &bad_message) }
        })
        .await;
        let second = server(|_| async { Response::new(full("backup-ok")) }).await;
        let (t, g) = fixture(vec![
            format!("http://127.0.0.1:{first}/v1"),
            format!("http://127.0.0.1:{second}/v1"),
        ])
        .await;
        auto(&g, &t, 1);
        start(&g, &t).await;
        let response = request(
            &g,
            "/v1/responses",
            br#"{"model":"exact-model","input":"fixture"}"#.to_vec(),
            vec![("content-type", "application/json")],
        )
        .await;
        assert_eq!(response.status(), 200, "status {status}");
        assert_eq!(
            response.into_body().collect().await.unwrap().to_bytes(),
            "backup-ok",
            "status {status}"
        );
        let first_view = &g.view().providers[0];
        assert_eq!(first_view.health.failures, 0, "status {status}");
        assert_eq!(first_view.health.cooldown_reason, None, "status {status}");
        g.stop().await.unwrap();
    }
}

#[test]
fn model_http_classification_excludes_capacity_and_rate_limit_phrases() {
    assert!(upstream_error::model_http(
        404,
        br#"{"error":{"code":"model_not_found","message":"missing"}}"#,
    ));
    assert!(upstream_error::model_http(
        503,
        br#"{"error":{"message":"no available channel for model exact-model"}}"#,
    ));
    assert!(upstream_error::model_http(
        403,
        "中文模型无可用渠道".as_bytes()
    ));
    assert!(!upstream_error::model_http(
        503,
        br#"{"error":{"message":"model is at capacity"}}"#,
    ));
    assert!(!upstream_error::model_http(
        429,
        br#"{"error":{"code":"model_not_found"}}"#,
    ));
}

#[tokio::test]
async fn manual_mode_returns_original_model_error_bytes_without_trying_backup() {
    let original = br#" {"error":{"code":"model_not_found","message":"model exact-model does not exist"},"unknown":[1,2,3]}\n"#;
    let first = server({
        let original = original.to_vec();
        move |_| {
            let original = original.clone();
            async move {
                Response::builder()
                    .status(404)
                    .header("content-type", "application/json")
                    .header("x-fixture", "preserve")
                    .body(full(original))
                    .unwrap()
            }
        }
    })
    .await;
    let backup_hits = Arc::new(AtomicUsize::new(0));
    let backup_count = backup_hits.clone();
    let second = server(move |_| {
        backup_count.fetch_add(1, Ordering::SeqCst);
        async { Response::new(full("backup")) }
    })
    .await;
    let (t, g) = fixture(vec![
        format!("http://127.0.0.1:{first}/v1"),
        format!("http://127.0.0.1:{second}/v1"),
    ])
    .await;
    start(&g, &t).await;
    let response = request(
        &g,
        "/v1/responses",
        br#"{"model":"exact-model"}"#.to_vec(),
        vec![("content-type", "application/json")],
    )
    .await;
    assert_eq!(response.status(), 404);
    assert_eq!(response.headers()["x-fixture"], "preserve");
    assert_eq!(
        response.into_body().collect().await.unwrap().to_bytes(),
        original.as_slice()
    );
    assert_eq!(backup_hits.load(Ordering::SeqCst), 0);
    assert_eq!(g.view().providers[0].health.failures, 0);
    g.stop().await.unwrap();
}

#[tokio::test]
async fn previous_response_affinity_stays_on_owner_after_priority_changes() {
    let first_bodies = Arc::new(Mutex::new(Vec::<Bytes>::new()));
    let seen = first_bodies.clone();
    let first = server(move |req| {
        let seen = seen.clone();
        async move {
            let body = req.into_body().collect().await.unwrap().to_bytes();
            seen.lock().unwrap().push(body);
            Response::builder()
                .header("content-type", "application/json")
                .body(full(
                    r#"{"object":"response","id":"affinity-fixture","model":"exact-model","status":"completed"}"#,
                ))
                .unwrap()
        }
    })
    .await;
    let backup_hits = Arc::new(AtomicUsize::new(0));
    let backup_count = backup_hits.clone();
    let second = server(move |_| {
        backup_count.fetch_add(1, Ordering::SeqCst);
        async {
            Response::builder()
                .status(503)
                .body(full("wrong owner"))
                .unwrap()
        }
    })
    .await;
    let (t, g) = fixture(vec![
        format!("http://127.0.0.1:{first}/v1"),
        format!("http://127.0.0.1:{second}/v1"),
    ])
    .await;
    auto(&g, &t, 1);
    start(&g, &t).await;
    let first_response = request(
        &g,
        "/v1/responses",
        br#"{"model":"exact-model"}"#.to_vec(),
        vec![("content-type", "application/json")],
    )
    .await;
    assert_eq!(first_response.status(), 200);
    first_response.into_body().collect().await.unwrap();
    let second_response = request(
        &g,
        "/v1/responses",
        br#"{"previous_response_id":"affinity-fixture"}"#.to_vec(),
        vec![("content-type", "application/json")],
    )
    .await;
    assert_eq!(second_response.status(), 200);
    second_response.into_body().collect().await.unwrap();
    assert_eq!(backup_hits.load(Ordering::SeqCst), 0);
    assert_eq!(first_bodies.lock().unwrap().len(), 2);
    g.stop().await.unwrap();
}

#[tokio::test]
async fn later_sse_model_error_never_splices_backup_and_preserves_compressed_bytes() {
    let raw = concat!(
        "data: {\"type\":\"response.created\",\"response\":{\"id\":\"stream-fixture\"}}\n\n",
        "data: {\"type\":\"error\",\"error\":{\"code\":\"model_not_found\",\"message\":\"model exact-model does not exist\"}}\n\n"
    )
    .as_bytes()
    .to_vec();
    for encoding in ["identity", "gzip", "deflate", "zstd"] {
        let payload = encoded(encoding, &raw);
        let first = server({
            let payload = payload.clone();
            move |_| {
                let payload = payload.clone();
                async move {
                    Response::builder()
                        .header("content-type", "text/event-stream")
                        .header("content-encoding", encoding)
                        .body(full(payload))
                        .unwrap()
                }
            }
        })
        .await;
        let backup_hits = Arc::new(AtomicUsize::new(0));
        let backup_count = backup_hits.clone();
        let second = server(move |_| {
            backup_count.fetch_add(1, Ordering::SeqCst);
            async { sse(r#"{"type":"response.completed"}"#) }
        })
        .await;
        let (t, g) = fixture(vec![
            format!("http://127.0.0.1:{first}/v1"),
            format!("http://127.0.0.1:{second}/v1"),
        ])
        .await;
        auto(&g, &t, 1);
        start(&g, &t).await;
        let response = request(
            &g,
            "/v1/responses",
            br#"{"model":"exact-model","stream":true}"#.to_vec(),
            vec![
                ("content-type", "application/json"),
                ("accept", "text/event-stream"),
            ],
        )
        .await;
        assert_eq!(response.status(), 200, "{encoding}");
        assert_eq!(
            response.into_body().collect().await.unwrap().to_bytes(),
            payload,
            "{encoding}"
        );
        assert_eq!(backup_hits.load(Ordering::SeqCst), 0, "{encoding}");
        g.stop().await.unwrap();
    }
}

#[tokio::test]
async fn bridge_404_model_error_is_prioritized_before_unsupported_classification() {
    let first =
        server(|_| async { model_error(404, "no available channel for model exact-model") }).await;
    let backup_hits = Arc::new(AtomicUsize::new(0));
    let backup_count = backup_hits.clone();
    let second = server(move |_| {
        backup_count.fetch_add(1, Ordering::SeqCst);
        async { sse(r#"{"type":"response.completed"}"#) }
    })
    .await;
    let (t, g) = fixture(vec![
        format!("http://127.0.0.1:{first}/v1"),
        format!("http://127.0.0.1:{second}/v1"),
    ])
    .await;
    bridge(&g, &t, 0);
    bridge(&g, &t, 1);
    auto(&g, &t, 1);
    start(&g, &t).await;
    let mut ws = concurrency::responses_client(&g).await;
    ws.send(yawc::Frame::text(
        r#"{"type":"response.create","model":"exact-model"}"#,
    ))
    .await
    .unwrap();
    assert_eq!(
        ws_next(&mut ws).await.payload().as_ref(),
        br#"{"type":"response.completed"}"#
    );
    assert_eq!(backup_hits.load(Ordering::SeqCst), 1);
    assert_eq!(g.view().providers[0].health.failures, 0);
    drop(ws);
    g.stop().await.unwrap();
}

#[tokio::test]
async fn bridge_all_model_errors_returns_last_original_payload_without_poisoning_health() {
    let first_body = serde_json::to_vec(&serde_json::json!({
        "error": {"code": "model_not_found", "message": "p1 missing"}
    }))
    .unwrap();
    let second_body = serde_json::to_vec(&serde_json::json!({
        "error": {"code": "model_not_found", "message": "p2 missing"}
    }))
    .unwrap();
    let first = server({
        let body = first_body.clone();
        move |_| {
            let body = body.clone();
            async move {
                Response::builder()
                    .status(404)
                    .header("content-type", "application/json")
                    .body(full(body))
                    .unwrap()
            }
        }
    })
    .await;
    let second = server({
        let body = second_body.clone();
        move |_| {
            let body = body.clone();
            async move {
                Response::builder()
                    .status(404)
                    .header("content-type", "application/json")
                    .body(full(body))
                    .unwrap()
            }
        }
    })
    .await;
    let (t, g) = fixture(vec![
        format!("http://127.0.0.1:{first}/v1"),
        format!("http://127.0.0.1:{second}/v1"),
    ])
    .await;
    bridge(&g, &t, 0);
    bridge(&g, &t, 1);
    auto(&g, &t, 1);
    start(&g, &t).await;
    let mut ws = concurrency::responses_client(&g).await;
    ws.send(yawc::Frame::text(
        r#"{"type":"response.create","model":"exact-model"}"#,
    ))
    .await
    .unwrap();
    let payload = ws_next(&mut ws).await;
    assert_eq!(payload.payload(), second_body.as_slice());
    let close = ws_next(&mut ws).await;
    assert_eq!(close.close_code().map(u16::from), Some(1008));
    assert!(g.view().providers.iter().all(|p| p.health.failures == 0));
    drop(ws);
    g.stop().await.unwrap();
}

#[tokio::test]
async fn rate_limit_authentication_and_network_keep_distinct_health_policies() {
    let rate = server(|_| async {
        Response::builder()
            .status(429)
            .header("retry-after", "60")
            .body(full("limited"))
            .unwrap()
    })
    .await;
    let rate_backup = server(|_| async { Response::new(full("rate-backup")) }).await;
    let (t, g) = fixture(vec![
        format!("http://127.0.0.1:{rate}/v1"),
        format!("http://127.0.0.1:{rate_backup}/v1"),
    ])
    .await;
    auto(&g, &t, 1);
    start(&g, &t).await;
    let response = request(&g, "/v1/test", vec![], vec![]).await;
    assert_eq!(
        response.into_body().collect().await.unwrap().to_bytes(),
        "rate-backup"
    );
    let rate_health = &g.view().providers[0].health;
    assert_eq!(rate_health.failures, 0);
    assert_eq!(
        rate_health.cooldown_reason.as_deref(),
        Some("capacity_retry")
    );
    g.stop().await.unwrap();

    let auth = server(|_| async {
        Response::builder()
            .status(401)
            .header("content-type", "text/plain")
            .body(full("bad key"))
            .unwrap()
    })
    .await;
    let auth_backup = server(|_| async { Response::new(full("auth-backup")) }).await;
    let (t, g) = fixture(vec![
        format!("http://127.0.0.1:{auth}/v1"),
        format!("http://127.0.0.1:{auth_backup}/v1"),
    ])
    .await;
    auto(&g, &t, 1);
    start(&g, &t).await;
    let response = request(&g, "/v1/test", vec![], vec![]).await;
    assert_eq!(
        response.into_body().collect().await.unwrap().to_bytes(),
        "auth-backup"
    );
    assert!(g.view().providers[0].health.failures >= 1);
    g.stop().await.unwrap();

    let network_backup = server(|_| async { Response::new(full("network-backup")) }).await;
    let (t, g) = fixture(vec![
        "http://127.0.0.1:9/v1".into(),
        format!("http://127.0.0.1:{network_backup}/v1"),
    ])
    .await;
    auto(&g, &t, 1);
    start(&g, &t).await;
    let response = request(&g, "/v1/test", vec![], vec![]).await;
    assert_eq!(
        response.into_body().collect().await.unwrap().to_bytes(),
        "network-backup"
    );
    assert!(g.view().providers[0].health.failures >= 1);
    g.stop().await.unwrap();
}

#[test]
fn protocol_marks_later_model_error_without_losing_first_event_observation() {
    let mut protocol = crate::gateway::protocol::Protocol::new(true);
    protocol.response(200, true, "identity");
    protocol.feed(
        b"data: {\"type\":\"response.created\",\"response\":{\"id\":\"x\"}}\n\ndata: {\"type\":\"error\",\"error\":{\"code\":\"model_not_found\"}}\n\n",
    );
    protocol.finish(Some(200), "OK");
    assert_eq!(protocol.observation.first_event_model_error, Some(false));
    assert_eq!(protocol.terminal(), Some(Terminal::ModelUnavailable));
}
