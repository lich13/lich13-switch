// 覆盖终态身份、临时错误与 incomplete 分类。
//! v0.23.1 terminal lifecycle regressions; all request data is synthetic.
use super::*;
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;

fn sse_data(value: &Value) -> Vec<u8> {
    format!("data: {value}\n\n").into_bytes()
}

#[tokio::test]
async fn bare_error_waits_for_matching_completed_and_records_success() {
    const RESPONSE_ID: &str = "fixture-terminal-response";

    let dir = tempfile::tempdir().unwrap();
    let usage = Service::new(dir.path());
    let trace = usage.begin("codex", Some("fixture-model"));
    let mut protocol = Protocol::new(true);
    protocol.attach_usage(trace.attempt("fixture-provider", true, "http"));
    protocol.response(200, true, "identity");

    let created = sse_data(&serde_json::json!({
        "type": "response.created",
        "response": {"id": RESPONSE_ID, "model": "fixture-model"}
    }));
    let mut response_headers = hyper::HeaderMap::new();
    response_headers.insert("x-request-id", "fixture-upstream-request".parse().unwrap());
    response_headers.insert(
        "x-client-request-id",
        "fixture-upstream-client-request".parse().unwrap(),
    );
    protocol.response_headers(&response_headers);

    let error = sse_data(&serde_json::json!({
        "type": "error",
        "response_id": RESPONSE_ID,
        "error": {"code": "server_error", "message": "fixture-only event"}
    }));
    protocol.feed(&created);
    protocol.feed(&error);
    assert_eq!(
        protocol.observation.response_id.as_deref(),
        Some(RESPONSE_ID)
    );
    assert_eq!(
        protocol.terminal(),
        None,
        "a bare error with a reliable response identity is provisional"
    );

    let completed = sse_data(&serde_json::json!({
        "type": "response.completed",
        "response": {
            "id": RESPONSE_ID,
            "model": "fixture-model",
            "status": "completed",
            "usage": {"input_tokens": 7, "output_tokens": 2}
        }
    }));
    protocol.feed(&completed);
    assert_eq!(protocol.terminal(), Some(Terminal::Success));
    assert!(!protocol.transport_failure());
    protocol.finish(Some(200), "OK");
    drop(protocol);
    drop(trace);

    let rows = recorded(&usage, 1).await;
    assert!(rows[0].completed);
    let attempt = rows[0].final_attempt().unwrap();
    assert_eq!(attempt.outcome, "success");
    assert_eq!(
        attempt.response_id,
        Some(crate::usage::model::safe_id(RESPONSE_ID))
    );
    let end = attempt.end.as_ref().unwrap();
    assert_eq!(end.cause, EndCause::Completed);
    assert!(end.terminal_received);
    let transfer = attempt.transfer.as_ref().unwrap();
    assert_eq!(
        transfer.upstream_bytes,
        (created.len() + error.len() + completed.len()) as u64
    );
    assert_eq!(
        attempt.upstream_request_hash.as_deref(),
        Some(crate::storage::digest(b"fixture-upstream-request").as_str())
    );
    let transfer = serde_json::to_value(transfer).unwrap();
    assert_eq!(transfer["terminalEvent"], "response.completed");
    assert_eq!(transfer["terminalStatus"], "completed");
    assert_eq!(
        transfer["upstreamClientRequestHash"],
        serde_json::json!(crate::storage::digest(b"fixture-upstream-client-request"))
    );
}

#[tokio::test]
async fn unknown_incomplete_keeps_unknown_outcome_and_safe_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let usage = Service::new(dir.path());
    let trace = usage.begin("codex", Some("fixture-model"));
    let mut protocol = Protocol::new(true);
    protocol.attach_usage(trace.attempt("fixture-provider", true, "websocket"));
    protocol.websocket_status(101);

    let mut response_headers = hyper::HeaderMap::new();
    response_headers.insert("x-request-id", "fixture-upstream-request".parse().unwrap());
    response_headers.insert(
        "x-client-request-id",
        "fixture-upstream-client-request".parse().unwrap(),
    );
    protocol.response_headers(&response_headers);
    protocol.value(&serde_json::json!({
        "type": "response.incomplete",
        "response": {
            "id": "fixture-incomplete-response",
            "model": "fixture-model",
            "status": "incomplete",
            "incomplete_details": {"reason": "fixture-private-incomplete-reason"},
            "output": [{"type": "message", "content": [{
                "type": "output_text", "text": "fixture-private-response-body"
            }]}]
        }
    }));
    assert_eq!(protocol.terminal(), Some(Terminal::Unknown));
    protocol.finish(Some(101), "OK");
    drop(protocol);
    drop(trace);

    let rows = recorded(&usage, 1).await;
    assert!(!rows[0].completed);
    let attempt = rows[0].final_attempt().unwrap();
    assert_eq!(attempt.outcome, "unknown");
    let end = attempt.end.as_ref().unwrap();
    assert!(end.terminal_received);
    assert_eq!(
        serde_json::to_value(end.cause).unwrap(),
        serde_json::json!("upstream_incomplete")
    );
    let expected_upstream_hash = crate::storage::digest(b"fixture-upstream-request");
    assert_eq!(
        attempt.upstream_request_hash.as_deref(),
        Some(expected_upstream_hash.as_str())
    );

    let transfer = attempt.transfer.as_ref().unwrap();
    let transfer_value = serde_json::to_value(transfer).unwrap();
    assert_eq!(transfer_value["terminalEvent"], "response.incomplete");
    assert_eq!(transfer_value["terminalStatus"], "incomplete");
    assert_eq!(transfer_value["incompleteReason"], "unrecognized");
    assert_eq!(
        transfer_value["upstreamClientRequestHash"],
        serde_json::json!(crate::storage::digest(b"fixture-upstream-client-request"))
    );
    let encoded = serde_json::to_string(&rows[0]).unwrap();
    assert!(!encoded.contains("fixture-private-incomplete-reason"));
    assert!(!encoded.contains("fixture-private-response-body"));
    assert!(!encoded.contains("fixture-upstream-client-request"));
}

#[test]
fn incomplete_reason_metadata_keeps_only_the_allowlist_and_omits_missing_values() {
    for reason in [
        "max_output_tokens",
        "max_tokens",
        "content_filter",
        "server_error",
        "rate_limit_exceeded",
    ] {
        let mut observation = crate::gateway::protocol::Observation::default();
        observation.value(&serde_json::json!({
            "type": "response.incomplete",
            "response": {
                "id": "fixture-incomplete-reason-response",
                "status": "incomplete",
                "incomplete_details": {"reason": reason}
            }
        }));
        assert_eq!(observation.incomplete_reason.as_deref(), Some(reason));
    }

    let mut missing = crate::gateway::protocol::Observation::default();
    missing.value(&serde_json::json!({
        "type": "response.incomplete",
        "response": {
            "id": "fixture-incomplete-no-reason-response",
            "status": "incomplete"
        }
    }));
    assert_eq!(missing.incomplete_reason, None);
}

#[tokio::test]
async fn bare_error_then_eof_records_failure() {
    let dir = tempfile::tempdir().unwrap();
    let usage = Service::new(dir.path());
    let trace = usage.begin("codex", Some("fixture-model"));
    let mut protocol = Protocol::new(true);
    protocol.attach_usage(trace.attempt("fixture-provider", true, "http"));
    protocol.response(200, true, "identity");
    protocol.feed(&sse_data(&serde_json::json!({
        "type": "error",
        "response_id": "fixture-error-only-response",
        "error": {"code": "server_error", "message": "fixture-only error"}
    })));
    protocol.finish(Some(200), "OK");
    assert_eq!(protocol.terminal(), Some(Terminal::Failure));
    drop(protocol);
    drop(trace);

    let rows = recorded(&usage, 1).await;
    assert!(!rows[0].completed);
    assert_eq!(rows[0].final_attempt().unwrap().outcome, "failure");
}

#[test]
fn unrelated_or_missing_response_identity_cannot_resolve_a_bare_error() {
    for (error_id, label) in [
        (Some("fixture-pending-response"), "unrelated completion"),
        (None, "error without response identity"),
    ] {
        let mut protocol = Protocol::new(true);
        protocol.websocket_status(101);
        let mut error = serde_json::json!({
            "type": "error",
            "error": {"code": "server_error", "message": "fixture-only error"}
        });
        if let Some(id) = error_id {
            error["response_id"] = serde_json::json!(id);
        }
        protocol.value(&error);
        protocol.value(&serde_json::json!({
            "type": "response.completed",
            "response": {
                "id": "fixture-unrelated-completed-response",
                "model": "fixture-model",
                "status": "completed"
            }
        }));
        protocol.finish(Some(101), "OK");
        assert_ne!(
            protocol.terminal(),
            Some(Terminal::Success),
            "{label} must not settle the pending error as success"
        );
    }
}

#[test]
fn other_protocol_stop_events_cannot_resolve_a_pending_responses_error() {
    let cases = [
        ("message_stop", serde_json::json!({"type": "message_stop"})),
        (
            "all choices stop",
            serde_json::json!({"choices": [{"finish_reason": "stop"}]}),
        ),
        (
            "all choices length",
            serde_json::json!({"choices": [{"finish_reason": "length"}]}),
        ),
    ];
    let mut observed = Vec::new();

    for (label, followup) in cases {
        let mut protocol = Protocol::new(true);
        protocol.websocket_status(101);
        protocol.value(&serde_json::json!({
            "type": "response.created",
            "response": {"id": "fixture-pending-responses-error", "model": "fixture-model"}
        }));
        protocol.value(&serde_json::json!({
            "type": "error",
            "response_id": "fixture-pending-responses-error",
            "error": {"code": "server_error", "message": "fixture-only error"}
        }));
        assert_eq!(protocol.terminal(), None);

        protocol.value(&followup);
        let after_followup = protocol.terminal();
        protocol.finish(Some(101), "OK");
        let after_eof = protocol.terminal();
        observed.push((label, after_followup, after_eof));
    }

    assert!(
        observed.iter().all(|(_, after_followup, after_eof)| {
            after_followup.is_none() && *after_eof == Some(Terminal::Failure)
        }),
        "non-Responses stop events must preserve the pending error until EOF: {observed:?}"
    );
}

#[test]
fn missing_identity_completed_or_done_cannot_resolve_a_pending_error() {
    for followup in ["identity-less completion", "SSE done marker"] {
        let mut protocol = Protocol::new(true);
        protocol.response(200, true, "identity");
        protocol.feed(&sse_data(&serde_json::json!({
            "type": "error",
            "response_id": "fixture-pending-error-response",
            "error": {"code": "server_error", "message": "fixture-only error"}
        })));
        assert_eq!(protocol.terminal(), None);

        if followup == "identity-less completion" {
            protocol.feed(&sse_data(&serde_json::json!({
                "type": "response.completed",
                "response": {
                    "model": "fixture-model",
                    "status": "completed",
                    "usage": {"input_tokens": 2, "output_tokens": 1}
                }
            })));
        } else {
            protocol.feed(b"data: [DONE]\n\n");
        }
        assert_ne!(
            protocol.terminal(),
            Some(Terminal::Success),
            "{followup} must not wash out a pending error"
        );
        protocol.finish(Some(200), "OK");
        assert_eq!(
            protocol.terminal(),
            Some(Terminal::Failure),
            "{followup} must leave the pending error failed at EOF"
        );
    }
}

#[test]
fn explicit_failed_or_cancelled_terminal_is_not_replaced_by_completed() {
    for (event, status, expected) in [
        ("response.failed", "failed", Terminal::Failure),
        ("response.cancelled", "cancelled", Terminal::Cancelled),
    ] {
        let mut protocol = Protocol::new(true);
        protocol.websocket_status(101);
        protocol.value(&serde_json::json!({
            "type": event,
            "response": {
                "id": "fixture-explicit-terminal-response",
                "model": "fixture-model",
                "status": status,
                "error": {"code": "server_error", "message": "fixture-only terminal"}
            }
        }));
        protocol.value(&serde_json::json!({
            "type": "response.completed",
            "response": {
                "id": "fixture-explicit-terminal-response",
                "model": "fixture-model",
                "status": "completed"
            }
        }));
        protocol.finish(Some(101), "OK");
        assert_eq!(protocol.terminal(), Some(expected), "{event}");
    }
}

#[test]
fn first_event_without_identity_or_with_model_capacity_or_auth_error_keeps_classification() {
    let mut unidentified = Protocol::new(true);
    unidentified.websocket_status(101);
    unidentified.value(&serde_json::json!({
        "type": "error",
        "error": {"code": "server_error", "message": "fixture-only error"}
    }));
    assert_eq!(unidentified.terminal(), Some(Terminal::Failure));
    assert_eq!(unidentified.observation.response_id, None);

    let mut model = Protocol::new(true);
    model.websocket_status(101);
    model.value(&serde_json::json!({
        "type": "error",
        "response_id": "fixture-model-error-response",
        "error": {"code": "model_not_found", "message": "model not found"}
    }));
    assert_eq!(model.terminal(), Some(Terminal::ModelUnavailable));

    let mut capacity = Protocol::new(true);
    capacity.websocket_status(101);
    capacity.value(&serde_json::json!({
        "type": "error",
        "response_id": "fixture-capacity-error-response",
        "error": {"code": "rate_limit_exceeded", "message": "Too many requests"}
    }));
    assert_eq!(capacity.terminal(), Some(Terminal::Failure));
    assert!(capacity.observation.capacity_error);

    let mut auth = Protocol::new(true);
    auth.websocket_status(101);
    auth.value(&serde_json::json!({
        "type": "error",
        "response_id": "fixture-auth-error-response",
        "error": {"code": "invalid_api_key", "message": "fixture-only auth rejection"}
    }));
    assert_eq!(auth.terminal(), Some(Terminal::Failure));
    assert_eq!(
        auth.observation.error.health_scope,
        Some(crate::events::HealthScope::Provider)
    );
}

#[tokio::test]
async fn http_sse_bare_error_then_matching_completion_preserves_wire_bytes() {
    const RESPONSE_ID: &str = "fixture-http-terminal-response";
    let payload = format!(
        "data: {}\n\ndata: {}\n\n",
        serde_json::json!({
            "type": "error",
            "response_id": RESPONSE_ID,
            "error": {"code": "server_error", "message": "fixture-only SSE error"}
        }),
        serde_json::json!({
            "type": "response.completed",
            "response": {
                "id": RESPONSE_ID,
                "model": "fixture-model",
                "status": "completed",
                "usage": {"input_tokens": 5, "output_tokens": 2}
            }
        })
    )
    .into_bytes();
    let upstream_body = payload.clone();
    let upstream = server(move |request| {
        let upstream_body = upstream_body.clone();
        async move {
            let _ = request.into_body().collect().await.unwrap();
            Response::builder()
                .header("content-type", "text/event-stream; charset=utf-8")
                .body(fragmented(upstream_body, 1))
                .unwrap()
        }
    })
    .await;
    let fixture = Fixture::new(&upstream).await;
    let (mut sender, task) = connection(&fixture).await;
    let response = request(&mut sender, &fixture, "fixture-http-terminal").await;
    assert_eq!(body_bytes(response).await.as_ref(), payload.as_slice());

    let rows = recorded(&fixture.usage, 1).await;
    assert_success(&rows[0], payload.len());
    drop(sender);
    task.abort();
    let _ = task.await;
    fixture.stop().await;
}

#[tokio::test]
async fn http_client_close_after_pending_error_stops_upstream_without_success() {
    const PENDING_ERROR_PREFIX: &[u8] = b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"fixture\",\"response\":{\"id\":\"fixture-disconnect-pending-error\",\"model\":\"fixture-model\"}}\n\ndata: {\"type\":\"error\",\"response_id\":\"fixture-disconnect-pending-error\",\"error\":{\"code\":\"server_error\",\"message\":\"fixture-only error\"}}\n\n";

    let release = std::sync::Arc::new(tokio::sync::Notify::new());
    let cancelled = std::sync::Arc::new(tokio::sync::Notify::new());
    let upstream = server({
        let release = release.clone();
        let cancelled = cancelled.clone();
        move |request| {
            let release = release.clone();
            let cancelled = cancelled.clone();
            async move {
                let _ = request.into_body().collect().await.unwrap();
                Response::builder()
                    .header("content-type", "text/event-stream")
                    .body(open_stream(PENDING_ERROR_PREFIX, release, cancelled))
                    .unwrap()
            }
        }
    })
    .await;
    let fixture = Fixture::new(&upstream).await;
    let (mut sender, task) = connection(&fixture).await;
    let mut body = request(&mut sender, &fixture, "fixture-pending-error-close")
        .await
        .into_body();
    read_prefix(&mut body, PENDING_ERROR_PREFIX).await;
    assert_eq!(
        fixture.gateway.view().providers[0].active_requests,
        1,
        "the provisional error must keep the upstream turn active while its body is open"
    );

    task.abort();
    let _ = task.await;
    drop(body);
    drop(sender);
    release.notify_one();
    tokio::time::timeout(WAIT, cancelled.notified())
        .await
        .expect("gateway did not stop forwarding the open upstream after client close");
    wait_for_provider_idle(&fixture.gateway).await;

    let rows = recorded(&fixture.usage, 1).await;
    fixture.stop().await;
    assert_eq!(rows.len(), 1);
    assert!(!rows[0].completed);
    let attempt = rows[0].final_attempt().unwrap();
    assert_ne!(attempt.outcome, "success");
    assert!(attempt
        .transfer
        .as_ref()
        .unwrap()
        .downstream_error
        .is_some());
}

const WS_CREATED_1: &str = r#"{"type":"response.created","response":{"id":"fixture-ws-terminal-1","model":"fixture-model"}}"#;
const WS_ERROR_1: &str = r#"{"type":"error","response_id":"fixture-ws-terminal-1","error":{"code":"server_error","message":"fixture-only ws error"}}"#;
const WS_COMPLETED_1: &str = r#"{"type":"response.completed","response":{"id":"fixture-ws-terminal-1","model":"fixture-model","status":"completed","usage":{"input_tokens":7,"output_tokens":2}}}"#;
const WS_CREATED_2: &str = r#"{"type":"response.created","response":{"id":"fixture-ws-terminal-2","model":"fixture-model"}}"#;
const WS_COMPLETED_2: &str = r#"{"type":"response.completed","response":{"id":"fixture-ws-terminal-2","model":"fixture-model","status":"completed","usage":{"input_tokens":8,"output_tokens":3}}}"#;

async fn websocket_server<F, Fut>(handler: F) -> Server
where
    F: Fn(Request<Incoming>, Tasks) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Response<WireBody>> + Send + 'static,
{
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let tasks = Tasks::default();
    let accepted = tasks.clone();
    let handler = std::sync::Arc::new(handler);
    tasks.spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let handler = handler.clone();
            let connection_tasks = accepted.clone();
            accepted.spawn(async move {
                let service = hyper::service::service_fn(move |request| {
                    let handler = handler.clone();
                    let tasks = connection_tasks.clone();
                    async move { Ok::<_, std::convert::Infallible>(handler(request, tasks).await) }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .with_upgrades()
                    .await;
            });
        }
    });
    Server { port, tasks }
}

async fn websocket_client(fixture: &Fixture) -> yawc::TcpWebSocket {
    tokio::time::timeout(
        WAIT,
        yawc::WebSocket::connect(
            format!(
                "ws://127.0.0.1:{}/v1/responses",
                fixture.gateway.view().settings.port
            )
            .parse()
            .unwrap(),
        )
        .with_options(yawc::Options::default().with_balanced_compression())
        .with_request(
            yawc::HttpRequest::builder()
                .header("authorization", format!("Bearer {}", fixture.token()))
                .header("session_id", "fixture-terminal-websocket"),
        ),
    )
    .await
    .unwrap()
    .unwrap()
}

async fn next_websocket_text(socket: &mut yawc::TcpWebSocket) -> String {
    tokio::time::timeout(WAIT, async {
        loop {
            let frame = socket
                .next()
                .await
                .expect("fixture websocket closed before expected event");
            match frame.opcode() {
                yawc::OpCode::Text => return String::from_utf8(frame.payload().to_vec()).unwrap(),
                yawc::OpCode::Ping | yawc::OpCode::Pong => continue,
                yawc::OpCode::Close => panic!("fixture websocket sent Close before completion"),
                opcode => panic!("fixture websocket sent unexpected opcode: {opcode:?}"),
            }
        }
    })
    .await
    .expect("fixture websocket event timed out")
}

async fn wait_for_provider_idle(gateway: &Gateway) {
    tokio::time::timeout(WAIT, async {
        loop {
            if gateway
                .view()
                .providers
                .first()
                .is_some_and(|provider| provider.active_requests == 0)
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("fixture websocket turn did not release its active request");
}

#[tokio::test]
async fn native_websocket_bare_error_then_matching_completion_settles_once_and_reuses_connection() {
    let handshakes = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let handshake_count = handshakes.clone();
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Vec<u8>>::new()));
    let observed = seen.clone();
    let upstream = websocket_server(move |mut request, tasks| {
        let handshake_count = handshake_count.clone();
        let observed = observed.clone();
        async move {
            handshake_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let (mut response, upgrade) = yawc::WebSocket::upgrade(&mut request).unwrap();
            response.headers_mut().insert(
                "x-client-request-id",
                "fixture-upstream-client-request".parse().unwrap(),
            );
            tasks.spawn(async move {
                let Ok(mut socket) = upgrade.await else {
                    return;
                };
                let mut turn = 0;
                while let Some(frame) = socket.next().await {
                    if frame.opcode() == yawc::OpCode::Close {
                        break;
                    }
                    if frame.opcode() == yawc::OpCode::Ping {
                        let _ = socket
                            .send(yawc::Frame::pong(frame.payload().to_vec()))
                            .await;
                        continue;
                    }
                    if frame.opcode().is_control() {
                        continue;
                    }
                    observed.lock().unwrap().push(frame.payload().to_vec());
                    turn += 1;
                    let events: &[&str] = match turn {
                        1 => &[WS_CREATED_1, WS_ERROR_1, WS_COMPLETED_1],
                        2 => &[WS_CREATED_2, WS_COMPLETED_2],
                        _ => &[],
                    };
                    for event in events {
                        if socket
                            .send(yawc::Frame::text((*event).to_owned()))
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                }
            });
            response.map(|_| full(Bytes::new()))
        }
    })
    .await;
    let fixture = Fixture::new(&upstream).await;
    let mut client = websocket_client(&fixture).await;

    let first_create =
        r#"{"type":"response.create","model":"fixture-model","input":"fixture-first"}"#;
    client
        .send(yawc::Frame::text(first_create.to_owned()))
        .await
        .unwrap();
    for expected in [WS_CREATED_1, WS_ERROR_1, WS_COMPLETED_1] {
        assert_eq!(next_websocket_text(&mut client).await, expected);
    }
    let first_rows = recorded(&fixture.usage, 1).await;
    let first_id = crate::usage::model::safe_id("fixture-ws-terminal-1");
    let first = first_rows
        .iter()
        .find(|row| row.final_attempt().unwrap().response_id.as_deref() == Some(first_id.as_str()))
        .expect("first websocket turn was not recorded");
    assert!(first.completed);
    let first_attempt = first.final_attempt().unwrap();
    assert_eq!(first_attempt.outcome, "success");
    assert_eq!(
        first_attempt.end.as_ref().unwrap().cause,
        EndCause::Completed
    );
    let first_transfer = serde_json::to_value(first_attempt.transfer.as_ref().unwrap()).unwrap();
    assert_eq!(first_transfer["terminalEvent"], "response.completed");
    assert_eq!(
        first_transfer["upstreamClientRequestHash"],
        serde_json::json!(crate::storage::digest(b"fixture-upstream-client-request"))
    );
    wait_for_provider_idle(&fixture.gateway).await;

    let second_create = r#"{"type":"response.create","model":"fixture-model","input":"fixture-second","previous_response_id":"fixture-ws-terminal-1"}"#;
    client
        .send(yawc::Frame::text(second_create.to_owned()))
        .await
        .unwrap();
    for expected in [WS_CREATED_2, WS_COMPLETED_2] {
        assert_eq!(next_websocket_text(&mut client).await, expected);
    }
    let rows = recorded(&fixture.usage, 2).await;
    let second_id = crate::usage::model::safe_id("fixture-ws-terminal-2");
    let second = rows
        .iter()
        .find(|row| row.final_attempt().unwrap().response_id.as_deref() == Some(second_id.as_str()))
        .expect("second websocket turn was not recorded");
    assert!(second.completed);
    assert_eq!(second.final_attempt().unwrap().outcome, "success");
    wait_for_provider_idle(&fixture.gateway).await;
    assert_eq!(handshakes.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(seen.lock().unwrap().len(), 2);

    client
        .send(yawc::Frame::close(1000.into(), "fixture done"))
        .await
        .unwrap();
    drop(client);
    fixture.stop().await;
}
