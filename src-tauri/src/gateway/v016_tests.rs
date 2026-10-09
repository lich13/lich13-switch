use super::*;
use crate::gateway::upstream_error;
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use tokio::sync::Semaphore;

struct NativeState {
    connections: AtomicUsize,
    requests: std::sync::Mutex<Vec<Vec<u8>>>,
    request_seen: Semaphore,
}

async fn native_server<F, Fut>(handler: F) -> (u16, Arc<NativeState>)
where
    F: Fn(usize, yawc::HttpWebSocket, Arc<NativeState>) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let state = Arc::new(NativeState {
        connections: AtomicUsize::new(0),
        requests: std::sync::Mutex::new(Vec::new()),
        request_seen: Semaphore::new(0),
    });
    let shared_state = state.clone();
    let handler = Arc::new(handler);
    let port = server(move |mut request| {
        let state = shared_state.clone();
        let handler = handler.clone();
        async move {
            let (response, upgrade) = yawc::WebSocket::upgrade_with_options(
                &mut request,
                yawc::Options::default().with_balanced_compression(),
            )
            .unwrap();
            let connection = state.connections.fetch_add(1, Ordering::SeqCst) + 1;
            tokio::spawn(async move {
                if let Ok(socket) = upgrade.await {
                    handler(connection, socket, state).await;
                }
            });
            response.map(|_| replay::empty())
        }
    })
    .await;
    (port, state)
}

async fn native_next<S>(ws: &mut S) -> Option<yawc::Frame>
where
    S: futures_util::Stream<Item = yawc::Frame> + Unpin,
{
    tokio::time::timeout(Duration::from_secs(3), ws.next())
        .await
        .expect("native WebSocket fixture timed out")
}

fn record_native_request(state: &NativeState, frame: &yawc::Frame) {
    if !frame.opcode().is_control() {
        state
            .requests
            .lock()
            .unwrap()
            .push(frame.payload().to_vec());
        state.request_seen.add_permits(1);
    }
}

fn native_retry_settings(g: &Gateway, t: &tempfile::TempDir, max_retries: usize, max_rpm: u32) {
    update(
        g,
        t,
        Edit::Mode {
            mode: "auto".into(),
        },
    );
    let mut settings = g.view().settings;
    settings.max_retries = max_retries;
    settings.websocket_retry_seconds = 1;
    settings.capacity_retry_seconds = 1;
    settings.rate_limit_seconds = 1;
    settings.first_byte_seconds = 3;
    settings.idle_seconds = 3;
    update(g, t, Edit::Settings { settings });
    update(
        g,
        t,
        Edit::RpmProvider {
            id: g.view().providers[0].id.clone(),
            max_rpm,
        },
    );
}

async fn wait_native_request(state: &NativeState) {
    tokio::time::timeout(Duration::from_secs(3), state.request_seen.acquire())
        .await
        .expect("upstream did not receive response.create")
        .unwrap()
        .forget();
}

const NATIVE_CREATE: &str = r#"{"type":"response.create","model":"fixture-model","input":"fixture","future":{"keep":true}}"#;
const NATIVE_CREATED: &str = r#"{"type":"response.created","response":{"id":"fixture-native-response","model":"fixture-model"}}"#;
const NATIVE_COMPLETED: &str = r#"{"type":"response.completed","response":{"id":"fixture-native-response","model":"fixture-model","status":"completed"}}"#;

#[test]
fn websocket_retry_interval_defaults_to_sixty_seconds_and_accepts_one() {
    assert_eq!(Settings::default().websocket_retry_seconds, 60);
    let settings = Settings {
        websocket_retry_seconds: 1,
        ..Settings::default()
    };
    assert!(settings.validate().is_ok());
}

#[tokio::test]
async fn native_websocket_reconnects_and_replays_only_before_first_response_frame() {
    let (port, state) = native_server(|connection, mut ws, state| async move {
        if let Some(frame) = native_next(&mut ws).await {
            record_native_request(&state, &frame);
            if connection == 1 {
                let _ = ws
                    .send(yawc::Frame::close(1011.into(), "fixture disconnect"))
                    .await;
            } else {
                ws.send(yawc::Frame::text(NATIVE_CREATED.to_owned()))
                    .await
                    .unwrap();
                ws.send(yawc::Frame::text(NATIVE_COMPLETED.to_owned()))
                    .await
                    .unwrap();
                while let Some(frame) = ws.next().await {
                    if frame.opcode() == yawc::OpCode::Close {
                        break;
                    }
                }
            }
        }
    })
    .await;
    let (t, g) = fixture(vec![format!("http://127.0.0.1:{port}")]).await;
    native_retry_settings(&g, &t, 1, 2);
    start(&g, &t).await;
    let mut client = concurrency::responses_client(&g).await;
    client
        .send(yawc::Frame::text(NATIVE_CREATE.to_owned()))
        .await
        .unwrap();

    let created = native_next(&mut client).await.unwrap();
    assert!(created.as_str().contains("response.created"));
    let completed = native_next(&mut client).await.unwrap();
    assert!(completed.as_str().contains("response.completed"));
    assert_eq!(state.connections.load(Ordering::SeqCst), 2);
    assert_eq!(
        state.requests.lock().unwrap().as_slice(),
        &[NATIVE_CREATE.as_bytes(), NATIVE_CREATE.as_bytes()]
    );
    assert_eq!(g.view().providers[0].rpm_used, 2);

    client
        .send(yawc::Frame::close(1000.into(), "fixture done"))
        .await
        .unwrap();
    drop(client);
    g.stop().await.unwrap();
}

#[tokio::test]
async fn native_websocket_exhausts_retry_budget_without_switching_provider() {
    let (port, state) = native_server(|_connection, mut ws, state| async move {
        if let Some(frame) = native_next(&mut ws).await {
            record_native_request(&state, &frame);
            let _ = ws
                .send(yawc::Frame::close(1011.into(), "fixture disconnect"))
                .await;
        }
    })
    .await;
    let (backup, backup_state) = native_server(|_connection, mut ws, state| async move {
        if let Some(frame) = native_next(&mut ws).await {
            record_native_request(&state, &frame);
            let _ = ws
                .send(yawc::Frame::text(NATIVE_COMPLETED.to_owned()))
                .await;
        }
    })
    .await;
    let (t, g) = fixture(vec![
        format!("http://127.0.0.1:{port}"),
        format!("http://127.0.0.1:{backup}"),
    ])
    .await;
    native_retry_settings(&g, &t, 1, 10);
    start(&g, &t).await;
    let mut client = concurrency::responses_client(&g).await;
    client
        .send(yawc::Frame::text(NATIVE_CREATE.to_owned()))
        .await
        .unwrap();

    let closed = native_next(&mut client).await.unwrap();
    assert_eq!(closed.opcode(), yawc::OpCode::Close);
    assert_eq!(closed.close_code().map(u16::from), Some(1013));
    tokio::time::timeout(Duration::from_secs(3), async {
        while g.view().providers[0].active_requests != 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("exhausted native retries did not release the provider slot");
    assert_eq!(state.connections.load(Ordering::SeqCst), 2);
    assert_eq!(
        state.requests.lock().unwrap().as_slice(),
        &[NATIVE_CREATE.as_bytes(), NATIVE_CREATE.as_bytes()]
    );
    assert_eq!(g.view().providers[0].rpm_used, 2);
    assert_eq!(backup_state.connections.load(Ordering::SeqCst), 0);
    assert!(backup_state.requests.lock().unwrap().is_empty());
    assert_eq!(g.view().providers[1].rpm_used, 0);
    assert_eq!(g.view().providers[1].active_requests, 0);

    drop(client);
    g.stop().await.unwrap();
}

#[tokio::test]
async fn native_websocket_does_not_replay_after_response_created() {
    let (port, state) = native_server(|_connection, mut ws, state| async move {
        if let Some(frame) = native_next(&mut ws).await {
            record_native_request(&state, &frame);
            ws.send(yawc::Frame::text(NATIVE_CREATED.to_owned()))
                .await
                .unwrap();
            let _ = ws
                .send(yawc::Frame::close(1011.into(), "fixture disconnect"))
                .await;
        }
    })
    .await;
    let (t, g) = fixture(vec![format!("http://127.0.0.1:{port}")]).await;
    native_retry_settings(&g, &t, 1, 2);
    start(&g, &t).await;
    let mut client = concurrency::responses_client(&g).await;
    client
        .send(yawc::Frame::text(NATIVE_CREATE.to_owned()))
        .await
        .unwrap();

    let created = native_next(&mut client).await.unwrap();
    assert!(created.as_str().contains("response.created"));
    let closed = native_next(&mut client).await.unwrap();
    assert_eq!(closed.opcode(), yawc::OpCode::Close);
    assert_eq!(state.connections.load(Ordering::SeqCst), 1);
    assert_eq!(state.requests.lock().unwrap().len(), 1);
    assert_eq!(g.view().providers[0].rpm_used, 1);

    drop(client);
    g.stop().await.unwrap();
}

#[tokio::test]
async fn native_websocket_cancel_and_gateway_stop_interrupt_retry_wait() {
    for stop_gateway in [false, true] {
        let (port, state) = native_server(|connection, mut ws, state| async move {
            if let Some(frame) = native_next(&mut ws).await {
                record_native_request(&state, &frame);
                if connection == 1 {
                    let _ = ws
                        .send(yawc::Frame::close(1011.into(), "fixture disconnect"))
                        .await;
                } else {
                    ws.send(yawc::Frame::text(NATIVE_CREATED.to_owned()))
                        .await
                        .unwrap();
                    ws.send(yawc::Frame::text(NATIVE_COMPLETED.to_owned()))
                        .await
                        .unwrap();
                    while let Some(frame) = ws.next().await {
                        if frame.opcode() == yawc::OpCode::Close {
                            break;
                        }
                    }
                }
            }
        })
        .await;
        let (t, g) = fixture(vec![format!("http://127.0.0.1:{port}")]).await;
        native_retry_settings(&g, &t, 1, 2);
        start(&g, &t).await;
        let mut client = concurrency::responses_client(&g).await;
        client
            .send(yawc::Frame::text(NATIVE_CREATE.to_owned()))
            .await
            .unwrap();
        wait_native_request(&state).await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        if stop_gateway {
            g.stop().await.unwrap();
            let closed = tokio::time::timeout(Duration::from_millis(500), client.next())
                .await
                .expect("gateway stop did not interrupt websocket retry wait")
                .unwrap();
            assert_eq!(closed.opcode(), yawc::OpCode::Close);
            assert_eq!(closed.close_code().map(u16::from), Some(1012));
        } else {
            client
                .send(yawc::Frame::text(
                    r#"{"type":"response.cancel"}"#.to_owned(),
                ))
                .await
                .unwrap();
            let cancelled = tokio::time::timeout(Duration::from_millis(500), client.next())
                .await
                .expect("response.cancel did not interrupt websocket retry wait")
                .unwrap();
            assert!(cancelled.as_str().contains("response.cancelled"));
            assert_eq!(state.connections.load(Ordering::SeqCst), 1);
            assert_eq!(g.view().providers[0].rpm_used, 1);

            client
                .send(yawc::Frame::text(NATIVE_CREATE.to_owned()))
                .await
                .unwrap();
            assert!(native_next(&mut client)
                .await
                .unwrap()
                .as_str()
                .contains("response.created"));
            assert!(native_next(&mut client)
                .await
                .unwrap()
                .as_str()
                .contains("response.completed"));
            assert_eq!(state.connections.load(Ordering::SeqCst), 2);
            assert_eq!(g.view().providers[0].rpm_used, 2);
            client
                .send(yawc::Frame::close(1000.into(), "fixture done"))
                .await
                .unwrap();
            g.stop().await.unwrap();
        }
        drop(client);
        assert_eq!(
            state.connections.load(Ordering::SeqCst),
            if stop_gateway { 1 } else { 2 }
        );
    }
}

#[tokio::test]
async fn native_websocket_reconnects_on_the_next_turn_after_idle_upstream_close() {
    let (port, state) = native_server(|connection, mut ws, state| async move {
        if let Some(frame) = native_next(&mut ws).await {
            record_native_request(&state, &frame);
            ws.send(yawc::Frame::text(NATIVE_CREATED.to_owned()))
                .await
                .unwrap();
            ws.send(yawc::Frame::text(NATIVE_COMPLETED.to_owned()))
                .await
                .unwrap();
            if connection == 1 {
                let _ = ws
                    .send(yawc::Frame::close(1000.into(), "idle fixture close"))
                    .await;
            } else {
                while let Some(frame) = ws.next().await {
                    if frame.opcode() == yawc::OpCode::Close {
                        break;
                    }
                }
            }
        }
    })
    .await;
    let (t, g) = fixture(vec![format!("http://127.0.0.1:{port}")]).await;
    native_retry_settings(&g, &t, 1, 2);
    start(&g, &t).await;
    let mut client = concurrency::responses_client(&g).await;
    client
        .send(yawc::Frame::text(NATIVE_CREATE.to_owned()))
        .await
        .unwrap();
    assert!(native_next(&mut client)
        .await
        .unwrap()
        .as_str()
        .contains("response.created"));
    assert!(native_next(&mut client)
        .await
        .unwrap()
        .as_str()
        .contains("response.completed"));

    client
        .send(yawc::Frame::text(NATIVE_CREATE.to_owned()))
        .await
        .unwrap();
    assert!(native_next(&mut client)
        .await
        .unwrap()
        .as_str()
        .contains("response.created"));
    assert!(native_next(&mut client)
        .await
        .unwrap()
        .as_str()
        .contains("response.completed"));
    assert_eq!(state.connections.load(Ordering::SeqCst), 2);
    assert_eq!(state.requests.lock().unwrap().len(), 2);
    assert_eq!(g.view().providers[0].rpm_used, 2);

    client
        .send(yawc::Frame::close(1000.into(), "fixture done"))
        .await
        .unwrap();
    drop(client);
    g.stop().await.unwrap();
}

fn auto_retry(g: &Gateway, t: &tempfile::TempDir) {
    update(
        g,
        t,
        Edit::Mode {
            mode: "auto".into(),
        },
    );
    let mut settings = g.view().settings;
    settings.max_retries = 1;
    settings.connect_seconds = 1;
    settings.first_byte_seconds = 2;
    settings.total_seconds = 5;
    update(g, t, Edit::Settings { settings });
}

fn json_error(status: u16, value: Value) -> Response<WireBody> {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(full(serde_json::to_vec(&value).unwrap()))
        .unwrap()
}

#[test]
fn model_error_classification_handles_http_envelopes_and_excludes_auth_or_capacity() {
    let errors = [
        json!({"error": {"code": "model_not_found", "message": "missing"}}),
        json!({"response": {"error": {"code": "unsupported_model"}}}),
        json!({"data": {"error": {"code": "invalid_model"}}}),
        json!({"code": "model_not_found", "message": "model exact-model does not exist"}),
    ];
    for status in [400, 403, 404, 503] {
        for body in &errors {
            let bytes = serde_json::to_vec(body).unwrap();
            assert!(
                upstream_error::model_http(status, &bytes),
                "status={status} body={body}"
            );
        }
    }

    assert!(upstream_error::model_error(&json!({
        "data": {"error": {"message": "no available channel for model exact-model"}}
    })));
    assert!(upstream_error::model_http(
        503,
        "中文模型无可用渠道".as_bytes()
    ));
    assert!(!upstream_error::model_http(
        200,
        &serde_json::to_vec(&errors[0]).unwrap()
    ));
    assert!(!upstream_error::model_http(
        429,
        &serde_json::to_vec(&errors[0]).unwrap()
    ));
    assert!(!upstream_error::model_http(
        503,
        br#"{"error":{"code":"overloaded_error","message":"model unavailable"}}"#,
    ));
    assert!(!upstream_error::model_http(
        503,
        br#"{"error":{"message":"model is at capacity"}}"#,
    ));
    assert!(!upstream_error::model_http(
        403,
        br#"{"error":{"code":"invalid_api_key","message":"authentication failed for model exact-model"}}"#,
    ));
}

#[tokio::test]
async fn repeated_p1_model_errors_fail_over_to_p2_without_breaker_failures() {
    let p1_hits = Arc::new(AtomicUsize::new(0));
    let p1_count = p1_hits.clone();
    let p1 = server(move |_| {
        p1_count.fetch_add(1, Ordering::SeqCst);
        async {
            json_error(
                404,
                json!({"error": {"code": "model_not_found", "message": "missing exact-model"}}),
            )
        }
    })
    .await;
    let p2_hits = Arc::new(AtomicUsize::new(0));
    let p2_count = p2_hits.clone();
    let p2 = server(move |_| {
        p2_count.fetch_add(1, Ordering::SeqCst);
        async { Response::new(full("backup-ok")) }
    })
    .await;
    let (t, g) = fixture(vec![
        format!("http://127.0.0.1:{p1}/v1"),
        format!("http://127.0.0.1:{p2}/v1"),
    ])
    .await;
    auto_retry(&g, &t);
    start(&g, &t).await;

    for _ in 0..3 {
        let response = request(
            &g,
            "/v1/responses",
            br#"{"model":"exact-model","input":"fixture"}"#.to_vec(),
            vec![("content-type", "application/json")],
        )
        .await;
        assert_eq!(response.status(), 200);
        assert_eq!(
            response.into_body().collect().await.unwrap().to_bytes(),
            "backup-ok"
        );
    }

    assert_eq!(p1_hits.load(Ordering::SeqCst), 3);
    assert_eq!(p2_hits.load(Ordering::SeqCst), 3);
    let p1_health = &g.view().providers[0].health;
    assert_eq!(p1_health.failures, 0);
    assert_eq!(p1_health.cooldown_reason, None);
    assert_eq!(p1_health.state, circuit::CircuitState::Closed);
    g.stop().await.unwrap();
}

#[tokio::test]
async fn http_200_application_failures_do_not_accumulate_circuit_failures() {
    let hits = Arc::new(AtomicUsize::new(0));
    let count = hits.clone();
    let payload = json!({
        "type": "response.failed",
        "response": {
            "id": "fixture-response",
            "model": "fixture-model",
            "status": "failed",
            "error": {"code": "server_error", "message": "fixture application failure"}
        }
    });
    let body = serde_json::to_vec(&payload).unwrap();
    let upstream_body = body.clone();
    let upstream = server(move |_| {
        count.fetch_add(1, Ordering::SeqCst);
        let body = upstream_body.clone();
        async move {
            Response::builder()
                .header("content-type", "application/json")
                .body(full(body))
                .unwrap()
        }
    })
    .await;
    let (t, g) = fixture(vec![format!("http://127.0.0.1:{upstream}/v1")]).await;
    start(&g, &t).await;

    for _ in 0..3 {
        let response = request(
            &g,
            "/v1/responses",
            br#"{"model":"fixture-model"}"#.to_vec(),
            vec![("content-type", "application/json")],
        )
        .await;
        assert_eq!(response.status(), 200);
        assert_eq!(
            response.into_body().collect().await.unwrap().to_bytes(),
            body.as_slice()
        );
    }

    assert_eq!(hits.load(Ordering::SeqCst), 3);
    let health = &g.view().providers[0].health;
    assert_eq!(health.failures, 0);
    assert_eq!(health.state, circuit::CircuitState::Closed);
    g.stop().await.unwrap();
}

#[test]
fn sse_response_model_is_observed_from_first_and_final_events() {
    let mut protocol = protocol::Protocol::new(true);
    protocol.response(200, true, "identity");
    protocol.feed(
        b"data: {\"type\":\"response.created\",\"response\":{\"model\":\"fixture-first-model\"}}\n\n",
    );
    assert_eq!(
        protocol.observation.meter.model.as_deref(),
        Some("fixture-first-model")
    );

    protocol.feed(b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"fixture\"}\n\n");
    assert_eq!(
        protocol.observation.meter.model.as_deref(),
        Some("fixture-first-model")
    );

    protocol.feed(
        b"data: {\"type\":\"response.completed\",\"response\":{\"model\":\"fixture-final-model\",\"usage\":{\"input_tokens\":2,\"output_tokens\":1}}}\n\n",
    );
    assert_eq!(
        protocol.observation.meter.model.as_deref(),
        Some("fixture-final-model")
    );
    assert_eq!(protocol.observation.meter.tokens.input, Some(2));
}
