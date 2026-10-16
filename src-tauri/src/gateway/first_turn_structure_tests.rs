// 覆盖客户端首次生成的初始化结构识别。
//! v0.23.3 Responses first-turn routing regressions.
//!
//! All request data, provider tokens, and upstream responses are synthetic;
//! upstreams bind only to loopback.
use super::{
    model::Edit,
    replay::{self, RequestHints, TurnEvidence, WireBody},
    *,
};
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use http_body_util::{BodyExt, Full};
use hyper::{body::Incoming, Request, Response, StatusCode};
use hyper_util::{client::legacy::Client, rt::TokioExecutor};
use serde_json::{json, Value};
use std::{
    convert::Infallible,
    future::Future,
    io::Write,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use crate::usage::model::RoutingInput;

async fn v0233_server<F, Fut>(handler: F) -> u16
where
    F: Fn(Request<Incoming>) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Response<WireBody>> + Send + 'static,
{
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let handler = Arc::new(handler);
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let handler = handler.clone();
            tokio::spawn(async move {
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(
                        hyper_util::rt::TokioIo::new(stream),
                        hyper::service::service_fn(move |request| {
                            let handler = handler.clone();
                            async move { Ok::<_, Infallible>(handler(request).await) }
                        }),
                    )
                    .with_upgrades()
                    .await;
            });
        }
    });
    port
}

fn v0233_json_response(body: impl Into<Bytes>) -> Response<WireBody> {
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/json")
        .body(replay::full(body))
        .unwrap()
}

async fn v0233_fixture(urls: Vec<String>) -> (tempfile::TempDir, Gateway) {
    let dir = tempfile::tempdir().unwrap();
    let first = urls
        .first()
        .cloned()
        .unwrap_or_else(|| "https://fixture.example.invalid/v1".into());
    std::fs::write(
        dir.path().join("config.toml"),
        format!(
            "model_provider='custom'\n[model_providers.custom]\nbase_url = {}\nexperimental_bearer_token = \"fixture-token\"\nwire_api='responses'\nsupports_websockets=false\n",
            serde_json::to_string(&first).unwrap()
        ),
    )
    .unwrap();
    let gateway = Gateway::new(dir.path().to_path_buf()).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let mut settings = gateway.view().settings;
    settings.port = port;
    v0233_update(&gateway, &dir, Edit::Settings { settings });
    for url in urls {
        v0233_update(
            &gateway,
            &dir,
            Edit::SaveProvider {
                id: None,
                base_url: url,
                token: "fixture-token".into(),
                name: None,
            },
        );
    }
    v0233_update(
        &gateway,
        &dir,
        Edit::Mode {
            mode: "auto".into(),
        },
    );
    (dir, gateway)
}

fn v0233_update(gateway: &Gateway, dir: &tempfile::TempDir, edit: Edit) {
    gateway
        .edit(edit, &gateway.view().revision, dir.path())
        .unwrap();
}

async fn v0233_start(gateway: &Gateway, dir: &tempfile::TempDir) {
    gateway
        .start(&gateway.view().revision, dir.path())
        .await
        .unwrap();
}

async fn v0233_request(gateway: &Gateway, thread: &str, body: Vec<u8>) -> Response<Incoming> {
    let client: Client<_, WireBody> = Client::builder(TokioExecutor::new())
        .build(super::connector::Connector::new(Duration::from_secs(3), 0));
    let (token, port) = {
        let state = gateway.0.inner.lock().unwrap();
        (state.store.local_token.clone(), state.store.settings.port)
    };
    let request = Request::builder()
        .method("POST")
        .uri(format!("http://127.0.0.1:{port}/v1/responses"))
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .header("thread-id", thread)
        .body(replay::full(body))
        .unwrap();
    client.request(request).await.unwrap()
}

async fn v0233_response_value(response: Response<Incoming>) -> Value {
    serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap()
}

fn v0233_ids(gateway: &Gateway) -> Vec<String> {
    gateway
        .view()
        .providers
        .iter()
        .map(|provider| provider.id.clone())
        .collect()
}

async fn v0233_inspect_bytes(bytes: Vec<u8>, encoding: &str, dir: &Path) -> RequestHints {
    let captured = replay::Replay::capture_progress(Full::new(Bytes::from(bytes)), dir, None)
        .await
        .unwrap();
    captured
        .inspect(encoding, "application/json")
        .await
        .unwrap()
}

fn v0233_split_tool_declaration_body() -> Vec<u8> {
    const READ_CHUNK: usize = 64 * 1024;
    let before_padding = br#"{"model":"fixture-model","padding":""#;
    let before_tool_type = br#"","input":[{"type":""#;
    let tool_type = b"additional_tools";
    let tool_start = READ_CHUNK - tool_type.len() / 2;
    let padding_len = tool_start - before_padding.len() - before_tool_type.len();
    let alphabet = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut padding = Vec::with_capacity(padding_len);
    let mut state = 0x4d595df4u32;
    for _ in 0..padding_len {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        padding.push(alphabet[state as usize % alphabet.len()]);
    }

    let mut body = Vec::with_capacity(READ_CHUNK + 512);
    body.extend_from_slice(before_padding);
    body.extend_from_slice(&padding);
    body.extend_from_slice(before_tool_type);
    body.extend_from_slice(tool_type);
    body.extend_from_slice(
        br#"","role":"developer","id":"synthetic","tools":[]},{"type":"message","role":"developer","content":"fixture init"},{"type":"message","role":"user","content":"fixture user"}]}"#,
    );
    body
}

fn v0233_gzip(bytes: &[u8]) -> Vec<u8> {
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    encoder.write_all(bytes).unwrap();
    encoder.finish().unwrap()
}

fn v0233_sse_bytes(provider: &str) -> Bytes {
    let event = json!({
        "type": "response.completed",
        "response": {
            "id": "fixture-v0233-sse-response",
            "model": "fixture-model",
            "status": "completed",
            "provider": provider,
            "output": []
        }
    })
    .to_string();
    Bytes::from(format!("event: response.completed\ndata: {event}\n\n"))
}

fn v0233_native_response(provider: &str) -> String {
    json!({
        "type": "response.completed",
        "response": {
            "id": "fixture-v0233-native-response",
            "model": "fixture-model",
            "status": "completed",
            "provider": provider,
            "output": []
        }
    })
    .to_string()
}

async fn v0233_sse_endpoint(provider: &'static str, seen: Arc<Mutex<Vec<Vec<u8>>>>) -> u16 {
    v0233_server(move |request| {
        let seen = seen.clone();
        let response = v0233_sse_bytes(provider);
        async move {
            let method = request.method().clone();
            let is_models =
                method == hyper::Method::GET && request.uri().path().ends_with("/models");
            let request = request.into_body().collect().await.unwrap().to_bytes();
            if method == hyper::Method::POST {
                seen.lock().unwrap().push(request.to_vec());
            }
            if is_models {
                Response::builder()
                    .status(StatusCode::OK)
                    .header("content-type", "application/json")
                    .body(replay::full(
                        r#"{"object":"list","data":[{"id":"fixture-model","object":"model"}]}"#,
                    ))
                    .unwrap()
            } else {
                Response::builder()
                    .status(StatusCode::OK)
                    .header("content-type", "text/event-stream")
                    .body(replay::full(response))
                    .unwrap()
            }
        }
    })
    .await
}

async fn v0233_native_endpoint(provider: &'static str, seen: Arc<Mutex<Vec<Vec<u8>>>>) -> u16 {
    v0233_server(move |mut request| {
        let seen = seen.clone();
        let response_text = v0233_native_response(provider);
        async move {
            let (handshake, upgrade) = yawc::WebSocket::upgrade(&mut request).unwrap();
            tokio::spawn(async move {
                let Ok(mut peer) = upgrade.await else {
                    return;
                };
                while let Some(frame) = peer.next().await {
                    if frame.opcode().is_control() {
                        if frame.opcode() == yawc::OpCode::Close {
                            break;
                        }
                        continue;
                    }
                    let payload = frame.payload().to_vec();
                    if serde_json::from_slice::<Value>(&payload).is_err() {
                        break;
                    }
                    seen.lock().unwrap().push(payload);
                    if peer
                        .send(yawc::Frame::text(response_text.clone()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            });
            handshake.map(|_| replay::empty())
        }
    })
    .await
}

async fn v0233_ws_turn(peer: &mut yawc::TcpWebSocket, body: &str) -> Vec<u8> {
    peer.send(yawc::Frame::text(body.to_owned())).await.unwrap();
    let frame = tokio::time::timeout(Duration::from_secs(5), peer.next())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(frame.opcode(), yawc::OpCode::Text);
    frame.payload().to_vec()
}

#[tokio::test]
async fn v0233_http_additional_tools_and_initial_messages_route_to_new_thread_provider() {
    let ordinary_port =
        v0233_server(|_| async { v0233_json_response(r#"{"provider":"ordinary"}"#) }).await;
    let new_thread_port =
        v0233_server(|_| async { v0233_json_response(r#"{"provider":"new-thread"}"#) }).await;
    let (dir, gateway) = v0233_fixture(vec![
        format!("http://127.0.0.1:{ordinary_port}/v1"),
        format!("http://127.0.0.1:{new_thread_port}/v1"),
    ])
    .await;
    let ids = v0233_ids(&gateway);
    v0233_update(
        &gateway,
        &dir,
        Edit::TakeoverProvider {
            id: ids[1].clone(),
            handoff_after_compaction: Some(false),
            take_new_threads: Some(true),
        },
    );
    v0233_start(&gateway, &dir).await;

    // Reproduce the Codex HTTP Replay shape: an additional developer tool
    // declaration followed by multiple initial developer/user messages.
    // Replay::inspect runs as part of the real gateway HTTP request below.
    let request = json!({
        "model": "fixture-model",
        "input": [
            {
                "type": "additional_tools",
                "role": "developer",
                "id": "synthetic",
                "tools": [{"type":"function","name":"fixture_tool"}]
            },
            {"type":"message","role":"developer","content":"fixture initialization"},
            {"type":"message","role":"user","content":"fixture first turn"}
        ]
    });
    let response = v0233_request(
        &gateway,
        "fixture-v0233-additional-tools-first-turn",
        serde_json::to_vec(&request).unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        v0233_response_value(response).await["provider"],
        "new-thread"
    );
    gateway.stop().await.unwrap();
}

#[tokio::test]
async fn v0233_replay_projection_recognizes_tool_declaration_across_chunks_and_gzip() {
    let dir = tempfile::tempdir().unwrap();
    let body = v0233_split_tool_declaration_body();
    let marker_start = body
        .windows(b"additional_tools".len())
        .position(|window| window == b"additional_tools")
        .unwrap();
    assert!(marker_start < 64 * 1024);
    assert!(marker_start + b"additional_tools".len() > 64 * 1024);

    for (encoding, encoded) in [("", body.clone()), ("gzip", v0233_gzip(&body))] {
        let hints = v0233_inspect_bytes(encoded, encoding, dir.path()).await;
        assert!(hints.first_turn, "encoding={encoding}");
        assert_eq!(hints.turn, TurnEvidence::First, "encoding={encoding}");
        assert_eq!(
            hints.routing_evidence.input,
            RoutingInput::FirstTurn,
            "encoding={encoding}"
        );
        assert!(
            hints.routing_evidence.has_tool_declarations,
            "encoding={encoding}"
        );
    }
}

#[tokio::test]
async fn v0233_replay_projection_keeps_history_and_unknown_shapes_protected() {
    let dir = tempfile::tempdir().unwrap();
    let cases = [
        (
            "unknown item type",
            json!({
                "model":"fixture-model",
                "input":[
                    {"type":"future_client_marker","role":"developer"},
                    {"type":"message","role":"user","content":"fixture"}
                ]
            }),
            RoutingInput::UnknownStructure,
            false,
        ),
        (
            "tool declaration with illegal role",
            json!({
                "model":"fixture-model",
                "input":[
                    {"type":"additional_tools","role":"tool","id":"synthetic","tools":[]},
                    {"type":"message","role":"user","content":"fixture"}
                ]
            }),
            RoutingInput::UnknownStructure,
            false,
        ),
        (
            "tool declaration without user message",
            json!({
                "model":"fixture-model",
                "input":[{"type":"additional_tools","role":"developer","id":"synthetic","tools":[]}]
            }),
            RoutingInput::UnknownStructure,
            true,
        ),
        (
            "assistant history",
            json!({
                "model":"fixture-model",
                "input":[
                    {"type":"message","role":"user","content":"prior"},
                    {"type":"message","role":"assistant","content":"fixture answer"},
                    {"type":"message","role":"user","content":"continuation"}
                ]
            }),
            RoutingInput::History,
            false,
        ),
        (
            "function history",
            json!({
                "model":"fixture-model",
                "input":[
                    {"type":"function_call","call_id":"fixture-call","name":"fixture_tool","arguments":"{}"},
                    {"type":"message","role":"user","content":"continuation"}
                ]
            }),
            RoutingInput::History,
            false,
        ),
        (
            "response cursor",
            json!({
                "model":"fixture-model",
                "previous_response_id":"fixture-previous-response",
                "input":[{"type":"message","role":"user","content":"continuation"}]
            }),
            RoutingInput::ResponseCursor,
            false,
        ),
        (
            "compaction context",
            json!({
                "model":"fixture-model",
                "input":[
                    {"type":"compaction","encrypted_content":"fixture-window"},
                    {"type":"message","role":"user","content":"continuation"}
                ]
            }),
            RoutingInput::Compaction,
            false,
        ),
        (
            "warmup",
            json!({
                "model":"fixture-model",
                "generate":false,
                "input":[{"type":"message","role":"user","content":"fixture warmup"}]
            }),
            RoutingInput::Warmup,
            false,
        ),
    ];

    for (name, value, expected_input, has_tools) in cases {
        let hints = v0233_inspect_bytes(serde_json::to_vec(&value).unwrap(), "", dir.path()).await;
        assert!(!hints.first_turn, "{name}");
        assert_ne!(hints.turn, TurnEvidence::First, "{name}");
        assert_eq!(hints.routing_evidence.input, expected_input, "{name}");
        assert_eq!(
            hints.routing_evidence.has_tool_declarations, has_tools,
            "{name}"
        );
    }
}

#[tokio::test]
async fn v0233_http_sse_route_preserves_request_and_response_bytes() {
    let ordinary_seen = Arc::new(Mutex::new(Vec::new()));
    let new_thread_seen = Arc::new(Mutex::new(Vec::new()));
    let ordinary_port = v0233_sse_endpoint("ordinary", ordinary_seen.clone()).await;
    let new_thread_port = v0233_sse_endpoint("new-thread", new_thread_seen.clone()).await;
    let (dir, gateway) = v0233_fixture(vec![
        format!("http://127.0.0.1:{ordinary_port}/v1"),
        format!("http://127.0.0.1:{new_thread_port}/v1"),
    ])
    .await;
    let ids = v0233_ids(&gateway);
    v0233_update(
        &gateway,
        &dir,
        Edit::TakeoverProvider {
            id: ids[1].clone(),
            handoff_after_compaction: Some(false),
            take_new_threads: Some(true),
        },
    );
    v0233_start(&gateway, &dir).await;

    let request = json!({
        "model": "fixture-model",
        "input": [
            {"type":"additional_tools","role":"developer","id":"synthetic","tools":[{"type":"function","name":"fixture_tool"}]},
            {"type":"message","role":"developer","content":"fixture init"},
            {"type":"message","role":"user","content":"fixture first turn"}
        ],
        "future_field": {"preserve": true}
    });
    let request_bytes = serde_json::to_vec(&request).unwrap();
    let expected_response = v0233_sse_bytes("new-thread");
    let response = v0233_request(
        &gateway,
        "fixture-v0233-http-sse-first-turn",
        request_bytes.clone(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let response_bytes = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(response_bytes, expected_response);
    assert_eq!(ordinary_seen.lock().unwrap().len(), 0);
    assert_eq!(new_thread_seen.lock().unwrap().as_slice(), &[request_bytes]);
    gateway.stop().await.unwrap();
}

#[tokio::test]
async fn v0233_native_ws_routes_new_thread_and_preserves_frame_bytes() {
    let ordinary_seen = Arc::new(Mutex::new(Vec::new()));
    let new_thread_seen = Arc::new(Mutex::new(Vec::new()));
    let ordinary_port = v0233_native_endpoint("ordinary", ordinary_seen.clone()).await;
    let new_thread_port = v0233_native_endpoint("new-thread", new_thread_seen.clone()).await;
    let (dir, gateway) = v0233_fixture(vec![
        format!("http://127.0.0.1:{ordinary_port}/v1"),
        format!("http://127.0.0.1:{new_thread_port}/v1"),
    ])
    .await;
    let ids = v0233_ids(&gateway);
    v0233_update(
        &gateway,
        &dir,
        Edit::TakeoverProvider {
            id: ids[1].clone(),
            handoff_after_compaction: Some(false),
            take_new_threads: Some(true),
        },
    );
    v0233_start(&gateway, &dir).await;

    let token = gateway.0.inner.lock().unwrap().store.local_token.clone();
    let mut peer = yawc::WebSocket::connect(
        format!(
            "ws://127.0.0.1:{}/v1/responses",
            gateway.view().settings.port
        )
        .parse()
        .unwrap(),
    )
    .with_request(
        yawc::HttpRequest::builder()
            .header("authorization", format!("Bearer {token}"))
            .header("thread-id", "fixture-v0233-native-first-turn"),
    )
    .await
    .unwrap();

    let request = r#" { "type":"response.create", "model":"fixture-model", "input":[ {"type":"additional_tools","role":"developer","id":"synthetic","tools":[{"type":"function","name":"fixture_tool"}]}, {"type":"message","role":"developer","content":"fixture init"}, {"type":"message","role":"user","content":"fixture first turn"} ], "future_field":{"preserve":true} } "#;
    let response = v0233_ws_turn(&mut peer, request).await;
    let response_value: Value = serde_json::from_slice(&response).unwrap();
    assert_eq!(response_value["type"], "response.completed");
    assert_eq!(response_value["response"]["provider"], "new-thread");
    assert_eq!(
        response.as_slice(),
        v0233_native_response("new-thread").as_bytes()
    );
    assert_eq!(ordinary_seen.lock().unwrap().len(), 0);
    assert_eq!(
        new_thread_seen.lock().unwrap().as_slice(),
        &[request.as_bytes().to_vec()]
    );

    peer.send(yawc::Frame::close(1000.into(), "fixture done"))
        .await
        .unwrap();
    drop(peer);
    gateway.stop().await.unwrap();
}
