// 覆盖失败后候选重算及低顺位供应商承接。
//! v0.24.0 hard-rejection failover regressions.
//!
//! Requests, providers, tokens, thread IDs, and compaction windows are
//! synthetic. Upstreams bind only to loopback.
use super::{
    model::Edit,
    replay::{self, WireBody},
    *,
};
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use http_body_util::{BodyExt, StreamBody};
use hyper::{
    body::{Frame, Incoming},
    Request, Response, StatusCode,
};
use hyper_util::{client::legacy::Client, rt::TokioExecutor};
use serde_json::{json, Value};
use std::{
    convert::Infallible,
    future::Future,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

const V024_WINDOW: &str = "fixture-v024-compaction-window";
const V024_USAGE_LIMIT_429: &[u8] =
    br#"{"error":{"code":"USAGE_LIMIT_EXCEEDED","message":"fixture usage limit"}}"#;
const V024_BALANCE_429: &[u8] =
    br#"{"error":{"code":"INSUFFICIENT_BALANCE","message":"fixture balance exhausted"}}"#;
const V024_COMPACTED_REQUEST: &[u8] = br#" {"model":"fixture-model","input":[{"type":"compaction","encrypted_content":"fixture-v024-compaction-window"},{"role":"user","content":"fixture-followup"}],"future":{"unchanged":true},"large":90071992547409931234567890} "#;
type V024HttpRequests = Arc<Mutex<Vec<(String, Vec<u8>)>>>;

fn v024_attach_usage(gateway: &Gateway, dir: &tempfile::TempDir) -> crate::usage::Service {
    let usage = crate::usage::Service::new(&dir.path().join("usage-data"));
    assert!(usage.state().error.is_none());
    gateway.set_usage(usage.clone());
    usage
}

async fn v024_wait_usage_record(
    usage: &crate::usage::Service,
    matches: impl Fn(&crate::usage::model::Record) -> bool,
) -> crate::usage::model::Record {
    let mut events = usage.subscribe();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let rows = usage
                .query(|store| Ok(store.logs(&crate::usage::model::Filter::default())?.rows))
                .unwrap();
            if let Some(record) = rows.into_iter().find(|record| matches(record)) {
                return record;
            }
            events.recv().await.expect("fixture usage writer stopped");
        }
    })
    .await
    .expect("fixture usage record was not written")
}

async fn v024_wait_failover_record(
    usage: &crate::usage::Service,
    first_provider: &str,
    fallback_provider: &str,
) -> crate::usage::model::Record {
    v024_wait_usage_record(usage, |record| {
        record.attempts.len() == 2
            && record.attempts[0].provider.as_deref() == Some(first_provider)
            && record.attempts[1].provider.as_deref() == Some(fallback_provider)
    })
    .await
}

async fn v024_wait_diagnostic_record(
    diagnostics: &crate::events::Service,
    provider_id: &str,
    matches: impl Fn(&crate::events::Record) -> bool,
) -> crate::events::Record {
    let mut events = diagnostics.subscribe();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let rows = diagnostics.query(crate::events::Filter::default());
            if let Some(record) = rows
                .items
                .into_iter()
                .map(|item| item.record)
                .find(|record| {
                    record.provider_id.as_deref() == Some(provider_id) && matches(record)
                })
            {
                return record;
            }
            events
                .recv()
                .await
                .expect("fixture diagnostic writer stopped");
        }
    })
    .await
    .expect("fixture diagnostic record was not written")
}

fn v024_assert_failover_evidence(
    record: &crate::usage::model::Record,
    stage: &str,
    failure: &str,
    exit: &str,
) {
    let evidence = record
        .failover_evidence
        .as_ref()
        .expect("failover usage record omitted its evidence");
    let value = serde_json::to_value(evidence).unwrap();
    assert_eq!(value["stage"], stage);
    assert_eq!(value["failure"], failure);
    assert_eq!(value["exit"], exit);

    let serialized = value.to_string();
    for private_fixture in [
        "fixture-token",
        "Bearer",
        "USAGE_LIMIT_EXCEEDED",
        "INSUFFICIENT_BALANCE",
        "fixture usage limit",
        "fixture balance exhausted",
        "fixture rate limit",
        "fixture-v024-compaction-window",
        "fixture-followup",
        "fixture-v024-ws-followup",
        "fixture-v024-sse-request",
        "fixture-v024-hard-429-request",
    ] {
        assert!(
            !serialized.contains(private_fixture),
            "failover evidence included fixture request or credential data"
        );
    }
}

async fn v024_server<F, Fut>(handler: F) -> u16
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

fn v024_json_response(status: StatusCode, body: impl Into<Bytes>) -> Response<WireBody> {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(replay::full(body))
        .unwrap()
}

fn v024_sse_response(status: StatusCode, body: impl Into<Bytes>) -> Response<WireBody> {
    Response::builder()
        .status(status)
        .header("content-type", "text/event-stream")
        .body(replay::full(body))
        .unwrap()
}

fn v024_completed_response(provider: &str, id: &str, output: Value) -> Response<WireBody> {
    v024_json_response(
        StatusCode::OK,
        json!({
            "id": id,
            "object": "response",
            "status": "completed",
            "provider": provider,
            "output": output,
        })
        .to_string(),
    )
}

async fn v024_fixture(urls: Vec<String>) -> (tempfile::TempDir, Gateway) {
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
    v024_update(&gateway, &dir, Edit::Settings { settings });
    for url in urls {
        v024_update(
            &gateway,
            &dir,
            Edit::SaveProvider {
                quota_access_token: None,
                id: None,
                base_url: url,
                token: "fixture-token".into(),
                name: None,
            },
        );
    }
    v024_update(
        &gateway,
        &dir,
        Edit::Mode {
            mode: "auto".into(),
        },
    );
    (dir, gateway)
}

fn v024_update(gateway: &Gateway, dir: &tempfile::TempDir, edit: Edit) {
    gateway
        .edit(edit, &gateway.view().revision, dir.path())
        .unwrap();
}

async fn v024_start(gateway: &Gateway, dir: &tempfile::TempDir) {
    gateway
        .start(&gateway.view().revision, dir.path())
        .await
        .unwrap();
}

async fn v024_request(
    gateway: &Gateway,
    path: &str,
    thread: &str,
    body: Vec<u8>,
) -> Response<Incoming> {
    let client: Client<_, WireBody> = Client::builder(TokioExecutor::new())
        .build(super::connector::Connector::new(Duration::from_secs(3), 0));
    let (token, port) = {
        let state = gateway.0.inner.lock().unwrap();
        (state.store.local_token.clone(), state.store.settings.port)
    };
    let request = Request::builder()
        .method("POST")
        .uri(format!("http://127.0.0.1:{port}{path}"))
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .header("thread-id", thread)
        .body(replay::full(body))
        .unwrap();
    client.request(request).await.unwrap()
}

async fn v024_body(response: Response<Incoming>) -> Bytes {
    response.into_body().collect().await.unwrap().to_bytes()
}

async fn v024_response_value(response: Response<Incoming>) -> Value {
    serde_json::from_slice(&v024_body(response).await).unwrap()
}

async fn v024_wait_settled(gateway: &Gateway) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while gateway
            .view()
            .providers
            .iter()
            .any(|provider| provider.active_requests != 0)
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("fixture request did not settle");
}

#[derive(Clone, Copy)]
enum V024NextResponse {
    HttpRejection(&'static [u8]),
    HttpTemporaryThenSuccess(&'static [u8]),
    IdleFailureAfterOutput,
}

struct V024SeededHttp {
    dir: tempfile::TempDir,
    gateway: Gateway,
    thread: String,
    session: String,
    boundary: String,
    owner_id: String,
    receiver_ids: Vec<String>,
    owner_seen: V024HttpRequests,
    receivers_seen: Vec<Arc<Mutex<Vec<Vec<u8>>>>>,
}

fn v024_delayed_stream_failure_response() -> Response<WireBody> {
    let frames = async_stream::try_stream! {
        yield Frame::data(Bytes::from_static(b"fixture-partial-output"));
        tokio::time::sleep(Duration::from_secs(3)).await;
        Err::<(), super::connector::BoxError>(
            std::io::Error::other("fixture stream failure").into(),
        )?;
    };
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/octet-stream")
        .body(StreamBody::new(frames).boxed_unsync())
        .unwrap()
}

async fn v024_seeded_http(
    thread: &str,
    receiver_count: usize,
    next_response: V024NextResponse,
) -> V024SeededHttp {
    let owner_seen = Arc::new(Mutex::new(Vec::<(String, Vec<u8>)>::new()));
    let owner_capture = owner_seen.clone();
    let temporary_attempts = Arc::new(AtomicUsize::new(0));
    let temporary_capture = temporary_attempts.clone();
    let owner_port = v024_server(move |request| {
        let owner_capture = owner_capture.clone();
        let temporary_capture = temporary_capture.clone();
        async move {
            let path = request.uri().path().to_owned();
            let method = request.method().clone();
            if method == hyper::Method::GET && path.ends_with("/models") {
                return v024_json_response(
                    StatusCode::OK,
                    r#"{"object":"list","data":[{"id":"fixture-model","object":"model"}]}"#,
                );
            }
            let body = request
                .into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes()
                .to_vec();
            if method == hyper::Method::POST {
                owner_capture
                    .lock()
                    .unwrap()
                    .push((path.clone(), body.clone()));
            }
            if path.ends_with("/responses/compact") {
                v024_completed_response(
                    "owner",
                    "fixture-v024-seeded-compaction-response",
                    json!([{"type":"compaction","encrypted_content":V024_WINDOW}]),
                )
            } else if body
                .windows(V024_WINDOW.len())
                .any(|window| window == V024_WINDOW.as_bytes())
            {
                match next_response {
                    V024NextResponse::HttpRejection(body) => {
                        v024_json_response(StatusCode::TOO_MANY_REQUESTS, body)
                    }
                    V024NextResponse::HttpTemporaryThenSuccess(body) => {
                        if temporary_capture.fetch_add(1, Ordering::SeqCst) == 0 {
                            v024_json_response(StatusCode::TOO_MANY_REQUESTS, body)
                        } else {
                            v024_completed_response(
                                "owner",
                                "fixture-v024-temporary-recovery-response",
                                json!([]),
                            )
                        }
                    }
                    V024NextResponse::IdleFailureAfterOutput => {
                        v024_delayed_stream_failure_response()
                    }
                }
            } else {
                v024_completed_response("owner", "fixture-v024-seeded-initial-response", json!([]))
            }
        }
    })
    .await;

    let mut urls = vec![format!("http://127.0.0.1:{owner_port}/v1")];
    let mut receivers_seen = Vec::with_capacity(receiver_count);
    for index in 0..receiver_count {
        let provider = format!("ordinary-{}", index + 1);
        let seen = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
        let capture = seen.clone();
        let port = v024_server(move |request| {
            let capture = capture.clone();
            let provider = provider.clone();
            async move {
                if request.method() == hyper::Method::GET
                    && request.uri().path().ends_with("/models")
                {
                    return v024_json_response(
                        StatusCode::OK,
                        r#"{"object":"list","data":[{"id":"fixture-model","object":"model"}]}"#,
                    );
                }
                let body = request
                    .into_body()
                    .collect()
                    .await
                    .unwrap()
                    .to_bytes()
                    .to_vec();
                capture.lock().unwrap().push(body);
                v024_completed_response(
                    &provider,
                    "fixture-v024-seeded-receiver-response",
                    json!([]),
                )
            }
        })
        .await;
        urls.push(format!("http://127.0.0.1:{port}/v1"));
        receivers_seen.push(seen);
    }

    let (dir, gateway) = v024_fixture(urls).await;
    let mut settings = gateway.view().settings;
    settings.max_retries = 3;
    settings.capacity_retry_seconds = 60;
    settings.queue_seconds = 2;
    settings.first_byte_seconds = 2;
    settings.idle_seconds = 2;
    v024_update(&gateway, &dir, Edit::Settings { settings });
    let providers = gateway.view().providers;
    assert_eq!(providers.len(), receiver_count + 1);
    let owner_id = providers[0].id.clone();
    let receiver_ids = providers[1..]
        .iter()
        .map(|provider| provider.id.clone())
        .collect();
    v024_start(&gateway, &dir).await;

    let session = crate::storage::digest(thread.as_bytes());
    let initial = v024_request(
        &gateway,
        "/v1/responses",
        thread,
        br#"{"model":"fixture-model","input":"fixture-v024-seed"}"#.to_vec(),
    )
    .await;
    assert_eq!(initial.status(), StatusCode::OK);
    assert_eq!(v024_response_value(initial).await["provider"], "owner");
    v024_wait_settled(&gateway).await;
    assert_eq!(
        gateway.0.compaction.owner(&session).as_deref(),
        Some(owner_id.as_str())
    );

    let compact = v024_request(
        &gateway,
        "/v1/responses/compact",
        thread,
        br#"{"model":"fixture-model","input":"fixture-v024-compact"}"#.to_vec(),
    )
    .await;
    assert_eq!(compact.status(), StatusCode::OK);
    assert_eq!(v024_response_value(compact).await["provider"], "owner");
    v024_wait_settled(&gateway).await;
    let boundary = crate::storage::digest(V024_WINDOW.as_bytes());
    assert!(gateway
        .0
        .compaction
        .boundary_matches(&session, Some(&boundary)));

    V024SeededHttp {
        dir,
        gateway,
        thread: thread.to_owned(),
        session,
        boundary,
        owner_id,
        receiver_ids,
        owner_seen,
        receivers_seen,
    }
}

fn v024_set_policy(
    fixture: &V024SeededHttp,
    provider_id: &str,
    handoff_after_compaction: bool,
    take_new_threads: bool,
) {
    v024_update(
        &fixture.gateway,
        &fixture.dir,
        Edit::TakeoverProvider {
            id: provider_id.to_owned(),
            handoff_after_compaction: Some(handoff_after_compaction),
            take_new_threads: Some(take_new_threads),
        },
    );
}

async fn v024_send_compacted_http(fixture: &V024SeededHttp) -> (StatusCode, Vec<u8>) {
    let response = tokio::time::timeout(
        Duration::from_secs(12),
        v024_request(
            &fixture.gateway,
            "/v1/responses",
            &fixture.thread,
            V024_COMPACTED_REQUEST.to_vec(),
        ),
    )
    .await
    .expect("compacted HTTP request exceeded its bounded deadline");
    let status = response.status();
    (status, v024_body(response).await.to_vec())
}

async fn v024_ws_endpoint(
    provider: &'static str,
    owner: bool,
    seen: Arc<Mutex<Vec<Vec<u8>>>>,
    error_status: Option<u16>,
    error_code: &'static str,
) -> u16 {
    v024_ws_endpoint_with_recovery(provider, owner, seen, error_status, error_code, false).await
}

async fn v024_ws_endpoint_with_recovery(
    provider: &'static str,
    owner: bool,
    seen: Arc<Mutex<Vec<Vec<u8>>>>,
    error_status: Option<u16>,
    error_code: &'static str,
    temporary_then_success: bool,
) -> u16 {
    let compacted_attempts = Arc::new(AtomicUsize::new(0));
    v024_server(move |mut request| {
        let seen = seen.clone();
        let compacted_attempts = compacted_attempts.clone();
        async move {
            if request.headers().get("upgrade").is_none() {
                let _ = request.into_body().collect().await;
                return v024_json_response(
                    StatusCode::NOT_IMPLEMENTED,
                    r#"{"error":{"code":"UNSUPPORTED_HTTP","message":"fixture websocket endpoint requires an upgrade"}}"#,
                );
            }
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
                    let Ok(value) = serde_json::from_slice::<Value>(&payload) else {
                        break;
                    };
                    seen.lock().unwrap().push(payload);
                    let input = value["input"].as_array();
                    let compact_trigger = input.is_some_and(|items| {
                        items
                            .iter()
                            .any(|item| item["type"] == "compaction_trigger")
                    });
                    let compacted = input
                        .is_some_and(|items| items.iter().any(|item| item["type"] == "compaction"));
                    let response = if owner
                        && compacted
                        && (!temporary_then_success
                            || compacted_attempts.fetch_add(1, Ordering::SeqCst) == 0)
                    {
                        let mut error = json!({
                            "type":"error",
                            "error":{
                                "code":error_code,
                                "message":match error_code {
                                    "USAGE_LIMIT_EXCEEDED" => "fixture usage limit",
                                    "insufficient_quota" | "INSUFFICIENT_BALANCE" => "fixture balance exhausted",
                                    _ => "fixture rate limit",
                                }
                            }
                        });
                        if let Some(status) = error_status {
                            error["status"] = json!(status);
                        }
                        error
                    } else {
                        let output = if owner && compact_trigger {
                            json!([{"type":"compaction","encrypted_content":V024_WINDOW}])
                        } else {
                            json!([])
                        };
                        json!({
                            "type":"response.completed",
                            "response":{
                                "id":format!("fixture-v024-ws-{provider}"),
                                "model":"fixture-model",
                                "status":"completed",
                                "provider":provider,
                                "output":output
                            }
                        })
                    };
                    if peer
                        .send(yawc::Frame::text(response.to_string()))
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

async fn v024_ws_turn(peer: &mut yawc::TcpWebSocket, body: String) -> Value {
    peer.send(yawc::Frame::text(body)).await.unwrap();
    let frame = tokio::time::timeout(Duration::from_secs(12), peer.next())
        .await
        .expect("fixture WebSocket turn exceeded its bounded deadline")
        .expect("fixture WebSocket closed before a response");
    assert_eq!(frame.opcode(), yawc::OpCode::Text);
    serde_json::from_slice(frame.payload()).unwrap()
}

async fn v024_compacted_websocket_fails_over(
    error_status: Option<u16>,
    error_code: &'static str,
    expected_failure: &str,
) {
    let owner_seen = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
    let owner_port =
        v024_ws_endpoint("owner", true, owner_seen.clone(), error_status, error_code).await;
    let receiver_seen = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
    let receiver_port = v024_ws_endpoint("ordinary", false, receiver_seen.clone(), None, "").await;
    let (dir, gateway) = v024_fixture(vec![
        format!("http://127.0.0.1:{owner_port}/v1"),
        format!("http://127.0.0.1:{receiver_port}/v1"),
    ])
    .await;
    let usage = v024_attach_usage(&gateway, &dir);
    let mut settings = gateway.view().settings;
    settings.max_retries = 3;
    settings.queue_seconds = 3;
    settings.first_byte_seconds = 3;
    settings.idle_seconds = 3;
    v024_update(&gateway, &dir, Edit::Settings { settings });
    let providers = gateway.view().providers;
    assert_eq!(providers.len(), 2);
    let owner_id = providers[0].id.clone();
    let receiver_id = providers[1].id.clone();
    assert!(!providers[1].handoff_after_compaction);
    assert!(!providers[1].take_new_threads);
    v024_start(&gateway, &dir).await;

    let thread = "fixture-v024-ws-compacted-thread";
    let session = crate::storage::digest(thread.as_bytes());
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
            .header("thread-id", thread),
    )
    .await
    .unwrap();

    let initial = json!({
        "type":"response.create",
        "model":"fixture-model",
        "input":[{"role":"user","content":"fixture-v024-ws-initial"}]
    })
    .to_string();
    assert_eq!(
        v024_ws_turn(&mut peer, initial).await["response"]["provider"],
        "owner"
    );
    let compact = json!({
        "type":"response.create",
        "model":"fixture-model",
        "input":[{"type":"compaction_trigger"}]
    })
    .to_string();
    assert_eq!(
        v024_ws_turn(&mut peer, compact).await["response"]["provider"],
        "owner"
    );
    v024_wait_settled(&gateway).await;
    let boundary = crate::storage::digest(V024_WINDOW.as_bytes());
    assert!(gateway
        .0
        .compaction
        .boundary_matches(&session, Some(&boundary)));

    let compacted = json!({
        "type":"response.create",
        "model":"fixture-model",
        "input":[
            {"type":"compaction","encrypted_content":V024_WINDOW},
            {"role":"user","content":"fixture-v024-ws-followup"}
        ],
        "future":{"unchanged":true}
    })
    .to_string();
    let response = v024_ws_turn(&mut peer, compacted).await;
    v024_wait_settled(&gateway).await;
    let owner_after = gateway.0.compaction.owner(&session);
    let boundary_fresh = gateway
        .0
        .compaction
        .boundary_matches(&session, Some(&boundary));
    let owner_frames = owner_seen.lock().unwrap().len();
    let receiver_frames = receiver_seen.lock().unwrap().clone();
    peer.send(yawc::Frame::close(1000.into(), "fixture done"))
        .await
        .unwrap();
    drop(peer);
    gateway.stop().await.unwrap();
    let failover_record = v024_wait_failover_record(&usage, &owner_id, &receiver_id).await;

    assert_eq!(response["response"]["provider"], "ordinary");
    assert_eq!(owner_frames, 3);
    assert_eq!(receiver_frames.len(), 1);
    assert_eq!(owner_after.as_deref(), Some(receiver_id.as_str()));
    assert!(!boundary_fresh);
    assert!(!owner_id.is_empty());
    v024_assert_failover_evidence(
        &failover_record,
        "failure_fallback",
        expected_failure,
        "forwarded",
    );
}

async fn v024_compacted_websocket_temporary_waits_on_owner(
    error_status: Option<u16>,
    error_code: &'static str,
) {
    let owner_seen = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
    let owner_port = v024_ws_endpoint_with_recovery(
        "owner",
        true,
        owner_seen.clone(),
        error_status,
        error_code,
        true,
    )
    .await;
    let receiver_seen = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
    let receiver_port = v024_ws_endpoint("ordinary", false, receiver_seen.clone(), None, "").await;
    let (dir, gateway) = v024_fixture(vec![
        format!("http://127.0.0.1:{owner_port}/v1"),
        format!("http://127.0.0.1:{receiver_port}/v1"),
    ])
    .await;
    let mut settings = gateway.view().settings;
    settings.max_retries = 3;
    settings.queue_seconds = 3;
    settings.first_byte_seconds = 3;
    settings.idle_seconds = 3;
    settings.websocket_retry_seconds = 1;
    settings.capacity_retry_seconds = 1;
    settings.rate_limit_seconds = 1;
    v024_update(&gateway, &dir, Edit::Settings { settings });
    let providers = gateway.view().providers;
    assert_eq!(providers.len(), 2);
    let owner_id = providers[0].id.clone();
    assert!(!providers[1].handoff_after_compaction);
    assert!(!providers[1].take_new_threads);
    v024_start(&gateway, &dir).await;

    let thread = "fixture-v024-ws-temporary-owner-thread";
    let session = crate::storage::digest(thread.as_bytes());
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
            .header("thread-id", thread),
    )
    .await
    .unwrap();

    let initial = json!({
        "type":"response.create",
        "model":"fixture-model",
        "input":[{"role":"user","content":"fixture-v024-ws-initial"}]
    })
    .to_string();
    assert_eq!(
        v024_ws_turn(&mut peer, initial).await["response"]["provider"],
        "owner"
    );
    let compact = json!({
        "type":"response.create",
        "model":"fixture-model",
        "input":[{"type":"compaction_trigger"}]
    })
    .to_string();
    assert_eq!(
        v024_ws_turn(&mut peer, compact).await["response"]["provider"],
        "owner"
    );
    v024_wait_settled(&gateway).await;
    let boundary = crate::storage::digest(V024_WINDOW.as_bytes());
    assert!(gateway
        .0
        .compaction
        .boundary_matches(&session, Some(&boundary)));

    let compacted = json!({
        "type":"response.create",
        "model":"fixture-model",
        "input":[
            {"type":"compaction","encrypted_content":V024_WINDOW},
            {"role":"user","content":"fixture-v024-ws-followup"}
        ],
        "future":{"unchanged":true}
    })
    .to_string();
    let started = std::time::Instant::now();
    let response = v024_ws_turn(&mut peer, compacted).await;
    let waited = started.elapsed();
    v024_wait_settled(&gateway).await;
    let owner_after = gateway.0.compaction.owner(&session);
    let boundary_fresh = gateway
        .0
        .compaction
        .boundary_matches(&session, Some(&boundary));
    let owner_frames = owner_seen.lock().unwrap().len();
    let receiver_frames = receiver_seen.lock().unwrap().clone();
    peer.send(yawc::Frame::close(1000.into(), "fixture done"))
        .await
        .unwrap();
    drop(peer);
    gateway.stop().await.unwrap();

    assert_eq!(response["response"]["provider"], "owner");
    assert!(
        waited >= Duration::from_millis(900),
        "owner waited only {waited:?}"
    );
    assert_eq!(
        owner_frames, 4,
        "owner must retry the temporary compacted turn"
    );
    assert_eq!(
        receiver_frames.len(),
        0,
        "temporary failure must not use backup"
    );
    assert_eq!(owner_after.as_deref(), Some(owner_id.as_str()));
    assert!(!boundary_fresh);
}

#[tokio::test]
async fn v024_compacted_http_account_rejection_fails_over_to_lower_priority_ordinary_provider() {
    let old_seen = Arc::new(Mutex::new(Vec::<(String, Vec<u8>)>::new()));
    let old_capture = old_seen.clone();
    let old_port = v024_server(move |request| {
        let old_capture = old_capture.clone();
        async move {
            let path = request.uri().path().to_owned();
            let method = request.method().clone();
            if method == hyper::Method::GET && path.ends_with("/models") {
                return v024_json_response(
                    StatusCode::OK,
                    r#"{"object":"list","data":[{"id":"fixture-model","object":"model"}]}"#,
                );
            }
            let body = request
                .into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes()
                .to_vec();
            if method == hyper::Method::POST {
                old_capture
                    .lock()
                    .unwrap()
                    .push((path.clone(), body.clone()));
            }
            if path.ends_with("/responses/compact") {
                v024_completed_response(
                    "owner",
                    "fixture-v024-compaction-response",
                    json!([{"type":"compaction","encrypted_content":V024_WINDOW}]),
                )
            } else if body
                .windows(V024_WINDOW.len())
                .any(|window| window == V024_WINDOW.as_bytes())
            {
                v024_json_response(StatusCode::TOO_MANY_REQUESTS, V024_BALANCE_429)
            } else {
                v024_completed_response("owner", "fixture-v024-initial-response", json!([]))
            }
        }
    })
    .await;

    let ordinary_seen = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
    let ordinary_capture = ordinary_seen.clone();
    let ordinary_port = v024_server(move |request| {
        let ordinary_capture = ordinary_capture.clone();
        async move {
            if request.method() == hyper::Method::GET && request.uri().path().ends_with("/models") {
                return v024_json_response(
                    StatusCode::OK,
                    r#"{"object":"list","data":[{"id":"fixture-model","object":"model"}]}"#,
                );
            }
            let body = request
                .into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes()
                .to_vec();
            ordinary_capture.lock().unwrap().push(body);
            v024_completed_response("ordinary", "fixture-v024-failover-response", json!([]))
        }
    })
    .await;

    let (dir, gateway) = v024_fixture(vec![
        format!("http://127.0.0.1:{old_port}/v1"),
        format!("http://127.0.0.1:{ordinary_port}/v1"),
    ])
    .await;
    let usage = v024_attach_usage(&gateway, &dir);
    let mut settings = gateway.view().settings;
    settings.max_retries = 3;
    settings.capacity_retry_seconds = 60;
    settings.queue_seconds = 2;
    settings.first_byte_seconds = 2;
    settings.idle_seconds = 2;
    v024_update(&gateway, &dir, Edit::Settings { settings });
    let providers = gateway.view().providers;
    assert_eq!(providers.len(), 2);
    let owner_id = providers[0].id.clone();
    let ordinary_id = providers[1].id.clone();
    assert!(!providers[1].handoff_after_compaction);
    assert!(!providers[1].take_new_threads);
    v024_start(&gateway, &dir).await;

    let thread = "fixture-v024-http-compacted-thread";
    let session = crate::storage::digest(thread.as_bytes());
    let initial = v024_request(
        &gateway,
        "/v1/responses",
        thread,
        br#"{"model":"fixture-model","input":"fixture-initial"}"#.to_vec(),
    )
    .await;
    assert_eq!(initial.status(), StatusCode::OK);
    assert_eq!(v024_response_value(initial).await["provider"], "owner");
    v024_wait_settled(&gateway).await;
    assert_eq!(
        gateway.0.compaction.owner(&session).as_deref(),
        Some(owner_id.as_str())
    );

    let compact = v024_request(
        &gateway,
        "/v1/responses/compact",
        thread,
        br#"{"model":"fixture-model","input":"fixture-compaction-request"}"#.to_vec(),
    )
    .await;
    assert_eq!(compact.status(), StatusCode::OK);
    assert_eq!(v024_response_value(compact).await["provider"], "owner");
    v024_wait_settled(&gateway).await;
    let boundary = crate::storage::digest(V024_WINDOW.as_bytes());
    assert!(gateway
        .0
        .compaction
        .boundary_matches(&session, Some(&boundary)));

    let compacted = br#" {"model":"fixture-model","input":[{"type":"compaction","encrypted_content":"fixture-v024-compaction-window"},{"role":"user","content":"fixture-followup"}],"future":{"unchanged":true},"large":90071992547409931234567890} "#.to_vec();
    let final_response = tokio::time::timeout(
        Duration::from_secs(12),
        v024_request(&gateway, "/v1/responses", thread, compacted.clone()),
    )
    .await
    .expect("compacted hard-429 request exceeded its bounded deadline");
    let final_status = final_response.status();
    let final_body = v024_body(final_response).await.to_vec();
    let failover_record = v024_wait_failover_record(&usage, &owner_id, &ordinary_id).await;
    let ordinary_requests = ordinary_seen.lock().unwrap().clone();
    let old_requests = old_seen.lock().unwrap().clone();
    let final_owner = gateway.0.compaction.owner(&session);
    let boundary_still_fresh = gateway
        .0
        .compaction
        .boundary_matches(&session, Some(&boundary));
    gateway.stop().await.unwrap();

    assert_eq!(
        ordinary_requests.len(),
        1,
        "lower-priority ordinary provider was not retried; downstream status={} body={:?}",
        final_status,
        String::from_utf8_lossy(&final_body)
    );
    assert_eq!(final_status, StatusCode::OK);
    let result: Value = serde_json::from_slice(&final_body).unwrap();
    assert_eq!(result["provider"], "ordinary");
    assert_eq!(old_requests.len(), 3);
    assert_eq!(old_requests[2].1, compacted);
    assert_eq!(ordinary_requests[0], compacted);
    assert_eq!(final_owner.as_deref(), Some(ordinary_id.as_str()));
    assert!(
        !boundary_still_fresh,
        "the handed-off window must be consumed"
    );
    assert!(!final_body
        .windows(V024_BALANCE_429.len())
        .any(|part| part == V024_BALANCE_429));
    v024_assert_failover_evidence(
        &failover_record,
        "failure_fallback",
        "account_rejected",
        "forwarded",
    );
}

#[tokio::test]
async fn v024_http_first_sse_account_rejection_fails_over_with_bounded_evidence() {
    let first_port = v024_server(|request| async move {
        if request.method() == hyper::Method::GET && request.uri().path().ends_with("/models") {
            return v024_json_response(
                StatusCode::OK,
                r#"{"object":"list","data":[{"id":"fixture-model","object":"model"}]}"#,
            );
        }
        v024_sse_response(
            StatusCode::OK,
            format!(
                "data: {}\n\n",
                json!({
                    "type":"error",
                    "error":{
                        "code":"INSUFFICIENT_BALANCE",
                        "message":"fixture balance exhausted"
                    }
                })
            ),
        )
    })
    .await;
    let second_port = v024_server(|request| async move {
        if request.method() == hyper::Method::GET && request.uri().path().ends_with("/models") {
            return v024_json_response(
                StatusCode::OK,
                r#"{"object":"list","data":[{"id":"fixture-model","object":"model"}]}"#,
            );
        }
        v024_completed_response("ordinary", "fixture-v024-sse-fallback", json!([]))
    })
    .await;
    let (dir, gateway) = v024_fixture(vec![
        format!("http://127.0.0.1:{first_port}/v1"),
        format!("http://127.0.0.1:{second_port}/v1"),
    ])
    .await;
    let usage = v024_attach_usage(&gateway, &dir);
    let diagnostics = crate::events::Service::new(dir.path());
    gateway.set_diagnostics(diagnostics.clone());
    let mut settings = gateway.view().settings;
    settings.max_retries = 1;
    v024_update(&gateway, &dir, Edit::Settings { settings });
    let providers = gateway.view().providers;
    let first_id = providers[0].id.clone();
    let fallback_id = providers[1].id.clone();
    v024_start(&gateway, &dir).await;

    let response = v024_request(
        &gateway,
        "/v1/responses",
        "fixture-v024-sse-account-rejection",
        br#"{"model":"fixture-model","input":"fixture-v024-sse-request"}"#.to_vec(),
    )
    .await;
    let status = response.status();
    let body = v024_body(response).await;
    v024_wait_settled(&gateway).await;
    let failover_record = v024_wait_failover_record(&usage, &first_id, &fallback_id).await;
    let rejection_event = v024_wait_diagnostic_record(&diagnostics, &first_id, |record| {
        record.reason == crate::events::Reason::UpstreamService && record.status == Some(200)
    })
    .await;
    let first_provider = gateway
        .view()
        .providers
        .into_iter()
        .find(|provider| provider.id == first_id)
        .expect("first fixture provider disappeared");
    gateway.stop().await.unwrap();

    assert_eq!(status, StatusCode::OK);
    let result: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(result["provider"], "ordinary");
    assert_eq!(
        rejection_event.details.counted_failure,
        Some(true),
        "the HTTP 200 first-event account rejection must be counted as a provider failure"
    );
    assert_eq!(
        rejection_event.details.health_scope,
        Some(crate::events::HealthScope::Provider)
    );
    assert_eq!(
        first_provider.health.failures, 1,
        "the first provider circuit must record the rejected account"
    );
    v024_assert_failover_evidence(
        &failover_record,
        "failure_fallback",
        "account_rejected",
        "forwarded",
    );
}

#[tokio::test]
async fn v024_http_insufficient_balance_429_without_backup_records_terminal_evidence() {
    let port = v024_server(|request| async move {
        if request.method() == hyper::Method::GET && request.uri().path().ends_with("/models") {
            return v024_json_response(
                StatusCode::OK,
                r#"{"object":"list","data":[{"id":"fixture-model","object":"model"}]}"#,
            );
        }
        v024_json_response(StatusCode::TOO_MANY_REQUESTS, V024_BALANCE_429)
    })
    .await;
    let (dir, gateway) = v024_fixture(vec![format!("http://127.0.0.1:{port}/v1")]).await;
    let usage = v024_attach_usage(&gateway, &dir);
    let mut settings = gateway.view().settings;
    settings.max_retries = 1;
    v024_update(&gateway, &dir, Edit::Settings { settings });
    let provider_id = gateway.view().providers[0].id.clone();
    v024_start(&gateway, &dir).await;

    let response = v024_request(
        &gateway,
        "/v1/responses",
        "fixture-v024-hard-429-without-backup",
        br#"{"model":"fixture-model","input":"fixture-v024-hard-429-request"}"#.to_vec(),
    )
    .await;
    let status = response.status();
    let body = v024_body(response).await;
    v024_wait_settled(&gateway).await;
    let rejection_record = v024_wait_usage_record(&usage, |record| {
        record.attempts.len() == 1
            && record.attempts[0].provider.as_deref() == Some(provider_id.as_str())
    })
    .await;
    gateway.stop().await.unwrap();

    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(body.as_ref(), V024_BALANCE_429);
    v024_assert_failover_evidence(
        &rejection_record,
        "initial",
        "account_rejected",
        "no_eligible_provider",
    );
}

#[tokio::test]
async fn v024_compacted_http_insufficient_balance_429_fails_over_to_ordinary_provider() {
    let fixture = v024_seeded_http(
        "fixture-v024-balance-thread",
        1,
        V024NextResponse::HttpRejection(V024_BALANCE_429),
    )
    .await;
    let (status, body) = v024_send_compacted_http(&fixture).await;
    v024_wait_settled(&fixture.gateway).await;
    let receiver_requests = fixture.receivers_seen[0].lock().unwrap().clone();
    let old_requests = fixture.owner_seen.lock().unwrap().clone();
    let owner = fixture.gateway.0.compaction.owner(&fixture.session);
    let boundary_fresh = fixture
        .gateway
        .0
        .compaction
        .boundary_matches(&fixture.session, Some(&fixture.boundary));
    fixture.gateway.stop().await.unwrap();

    assert_eq!(status, StatusCode::OK);
    assert_eq!(receiver_requests.len(), 1);
    assert_eq!(receiver_requests[0], V024_COMPACTED_REQUEST);
    assert_eq!(old_requests.len(), 3);
    assert_eq!(old_requests[2].1, V024_COMPACTED_REQUEST);
    let response: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(response["provider"], "ordinary-1");
    assert_eq!(owner.as_deref(), Some(fixture.receiver_ids[0].as_str()));
    assert!(!boundary_fresh);
}

#[tokio::test]
async fn v024_compacted_http_can_use_a_lower_priority_compaction_receiver() {
    let fixture = v024_seeded_http(
        "fixture-v024-lower-receiver-thread",
        1,
        V024NextResponse::HttpRejection(V024_BALANCE_429),
    )
    .await;
    let receiver_id = fixture.receiver_ids[0].clone();
    v024_set_policy(&fixture, &receiver_id, true, false);

    let (status, body) = v024_send_compacted_http(&fixture).await;
    v024_wait_settled(&fixture.gateway).await;
    let receiver_requests = fixture.receivers_seen[0].lock().unwrap().clone();
    let owner = fixture.gateway.0.compaction.owner(&fixture.session);
    let boundary_fresh = fixture
        .gateway
        .0
        .compaction
        .boundary_matches(&fixture.session, Some(&fixture.boundary));
    fixture.gateway.stop().await.unwrap();

    assert_eq!(status, StatusCode::OK);
    assert_eq!(receiver_requests.len(), 1);
    assert_eq!(receiver_requests[0], V024_COMPACTED_REQUEST);
    let response: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(response["provider"], "ordinary-1");
    assert_eq!(owner.as_deref(), Some(receiver_id.as_str()));
    assert!(!boundary_fresh);
}

#[tokio::test]
async fn v024_compacted_http_temporary_usage_limit_waits_on_owner_and_recovers_without_backup() {
    let fixture = v024_seeded_http(
        "fixture-v024-temporary-owner-thread",
        1,
        V024NextResponse::HttpTemporaryThenSuccess(V024_USAGE_LIMIT_429),
    )
    .await;
    let mut settings = fixture.gateway.view().settings;
    settings.rate_limit_seconds = 1;
    settings.capacity_retry_seconds = 1;
    v024_update(&fixture.gateway, &fixture.dir, Edit::Settings { settings });

    let started = std::time::Instant::now();
    let (status, body) = v024_send_compacted_http(&fixture).await;
    let waited = started.elapsed();
    v024_wait_settled(&fixture.gateway).await;
    let owner_requests = fixture.owner_seen.lock().unwrap().clone();
    let backup_requests = fixture.receivers_seen[0].lock().unwrap().clone();
    let owner = fixture.gateway.0.compaction.owner(&fixture.session);
    let boundary_fresh = fixture
        .gateway
        .0
        .compaction
        .boundary_matches(&fixture.session, Some(&fixture.boundary));
    fixture.gateway.stop().await.unwrap();

    assert_eq!(status, StatusCode::OK);
    assert!(
        waited >= Duration::from_millis(900),
        "owner waited only {waited:?}"
    );
    assert_eq!(
        backup_requests.len(),
        0,
        "temporary usage limit must not use backup"
    );
    assert_eq!(
        owner_requests.len(),
        4,
        "owner must retry after its cooldown"
    );
    assert_eq!(owner_requests[2].1, V024_COMPACTED_REQUEST);
    assert_eq!(owner_requests[3].1, V024_COMPACTED_REQUEST);
    let response: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(response["provider"], "owner");
    assert_eq!(owner.as_deref(), Some(fixture.owner_id.as_str()));
    assert!(!boundary_fresh);
}

#[tokio::test]
async fn v024_compacted_http_account_rejection_never_fails_over_to_a_new_thread_only_provider() {
    let fixture = v024_seeded_http(
        "fixture-v024-history-protection-thread",
        2,
        V024NextResponse::HttpRejection(V024_BALANCE_429),
    )
    .await;
    let new_only_id = fixture.receiver_ids[0].clone();
    v024_set_policy(&fixture, &new_only_id, false, true);
    let ids = vec![
        new_only_id.clone(),
        fixture.owner_id.clone(),
        fixture.receiver_ids[1].clone(),
    ];
    v024_update(&fixture.gateway, &fixture.dir, Edit::Reorder { ids });

    let (status, body) = v024_send_compacted_http(&fixture).await;
    v024_wait_settled(&fixture.gateway).await;
    let new_only_requests = fixture.receivers_seen[0].lock().unwrap().clone();
    let ordinary_requests = fixture.receivers_seen[1].lock().unwrap().clone();
    let owner = fixture.gateway.0.compaction.owner(&fixture.session);
    let boundary_fresh = fixture
        .gateway
        .0
        .compaction
        .boundary_matches(&fixture.session, Some(&fixture.boundary));
    fixture.gateway.stop().await.unwrap();

    assert_eq!(status, StatusCode::OK);
    assert!(new_only_requests.is_empty());
    assert_eq!(ordinary_requests.len(), 1);
    assert_eq!(ordinary_requests[0], V024_COMPACTED_REQUEST);
    let response: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(response["provider"], "ordinary-2");
    assert_eq!(owner.as_deref(), Some(fixture.receiver_ids[1].as_str()));
    assert!(!boundary_fresh);
}

#[tokio::test]
async fn v024_compacted_http_idle_failure_after_output_does_not_replay_to_backup() {
    let fixture = v024_seeded_http(
        "fixture-v024-no-replay-thread",
        1,
        V024NextResponse::IdleFailureAfterOutput,
    )
    .await;
    let mut settings = fixture.gateway.view().settings;
    settings.idle_seconds = 1;
    v024_update(&fixture.gateway, &fixture.dir, Edit::Settings { settings });
    let response = tokio::time::timeout(
        Duration::from_secs(12),
        v024_request(
            &fixture.gateway,
            "/v1/responses",
            &fixture.thread,
            V024_COMPACTED_REQUEST.to_vec(),
        ),
    )
    .await
    .expect("compacted stream request exceeded its bounded deadline");
    let status = response.status();
    let mut body = response.into_body();
    let first = tokio::time::timeout(Duration::from_secs(5), body.frame())
        .await
        .expect("first output frame timed out")
        .expect("response ended before its first output")
        .expect("first output frame failed");
    let first_bytes = first.into_data().unwrap().to_vec();
    let second = tokio::time::timeout(Duration::from_secs(5), body.frame())
        .await
        .expect("post-output idle failure did not settle")
        .expect("post-output idle failure did not reach the downstream body");
    let second_is_error = second.is_err();
    v024_wait_settled(&fixture.gateway).await;
    let receiver_requests = fixture.receivers_seen[0].lock().unwrap().clone();
    let owner = fixture.gateway.0.compaction.owner(&fixture.session);
    let boundary_fresh = fixture
        .gateway
        .0
        .compaction
        .boundary_matches(&fixture.session, Some(&fixture.boundary));
    fixture.gateway.stop().await.unwrap();

    assert_eq!(status, StatusCode::OK);
    assert_eq!(first_bytes, b"fixture-partial-output");
    assert!(
        second_is_error,
        "the fixture must fail after delivering output"
    );
    assert!(
        receiver_requests.is_empty(),
        "the request must not be replayed"
    );
    assert_eq!(owner.as_deref(), Some(fixture.owner_id.as_str()));
    assert!(!boundary_fresh);
}

#[tokio::test]
async fn v024_compacted_websocket_account_rejection_fails_over_to_ordinary_provider() {
    v024_compacted_websocket_fails_over(Some(429), "insufficient_quota", "account_rejected").await;
}

#[tokio::test]
async fn v024_compacted_websocket_usage_limit_waits_on_owner_and_recovers_without_backup() {
    v024_compacted_websocket_temporary_waits_on_owner(None, "USAGE_LIMIT_EXCEEDED").await;
}

#[tokio::test]
async fn v024_compacted_websocket_rate_limit_waits_on_owner_and_recovers_without_backup() {
    v024_compacted_websocket_temporary_waits_on_owner(Some(429), "rate_limit_exceeded").await;
}
