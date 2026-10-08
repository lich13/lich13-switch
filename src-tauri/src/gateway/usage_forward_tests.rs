//! Usage integration through isolated loopback gateways; all keys are fixtures.
use super::{
    connector::{BoxError, Connector},
    replay::{self, full, WireBody},
    ClientId, Edit, Gateway, HttpClient, Settings,
};
use crate::usage::{
    model::{safe_id, Filter, Record},
    Service,
};
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use http_body_util::{BodyExt, StreamBody};
use hyper::{
    body::{Frame, Incoming},
    Request, Response,
};
use hyper_util::{client::legacy::Client, rt::TokioExecutor, rt::TokioIo};
use serde_json::json;
use std::{
    convert::Infallible,
    future::Future,
    path::PathBuf,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::task::JoinHandle;

const WAIT: Duration = Duration::from_secs(5);
const FIXTURE_KEY: &str = "fixture-only-key";
const HTTP_REQUEST: &[u8] = br#" { "model" : "gpt-request-fixture", "stream": false,
  "input":[{"role":"user","content":"fixture"}], "unknown":9007199254740993 } "#;
const HTTP_RESPONSE: &[u8] = br#" { "id":"fixture-http-response", "object":"response",
 "model":"gpt-response-fixture", "status":"completed", "unknown":9007199254740993,
 "usage":{"input_tokens":100,"output_tokens":20,"input_tokens_details":{"cached_tokens":40}} } "#;
const WS_DELTA: &str = r#"{"type":"response.output_text.delta","delta":"fixture"}"#;

#[derive(Clone, Default)]
struct TaskScope(Arc<Mutex<(bool, Vec<JoinHandle<()>>)>>);

impl TaskScope {
    fn spawn(&self, future: impl Future<Output = ()> + Send + 'static) {
        let mut state = self.0.lock().unwrap();
        if !state.0 {
            state.1.push(tokio::spawn(future));
        }
    }

    fn close(&self) {
        let mut state = self.0.lock().unwrap();
        state.0 = true;
        for task in state.1.drain(..) {
            task.abort();
        }
    }
}

struct LoopbackServer {
    port: u16,
    tasks: TaskScope,
}

impl LoopbackServer {
    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }
}

impl Drop for LoopbackServer {
    fn drop(&mut self) {
        self.tasks.close();
    }
}

async fn server<F, Fut>(handler: F) -> LoopbackServer
where
    F: Fn(Request<Incoming>, TaskScope) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Response<WireBody>> + Send + 'static,
{
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let tasks = TaskScope::default();
    let accept_tasks = tasks.clone();
    let handler = Arc::new(handler);
    tasks.spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let handler = handler.clone();
            let connection_tasks = accept_tasks.clone();
            accept_tasks.spawn(async move {
                let service = hyper::service::service_fn(move |request| {
                    let handler = handler.clone();
                    let tasks = connection_tasks.clone();
                    async move { Ok::<_, Infallible>(handler(request, tasks).await) }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .with_upgrades()
                    .await;
            });
        }
    });
    LoopbackServer { port, tasks }
}

struct Fixture {
    gateway: Gateway,
    usage: Service,
    home: PathBuf,
    _dir: tempfile::TempDir,
}

impl Fixture {
    async fn new(client: ClientId, urls: &[String]) -> Self {
        assert!(!urls.is_empty());
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("client-home");
        std::fs::create_dir(&home).unwrap();
        let data = dir.path().join("gateway-data");
        let gateway = match client {
            ClientId::Codex => {
                let config = format!(
                    "model_provider='custom'\n[model_providers.custom]\nbase_url={}\nexperimental_bearer_token={}\nwire_api='responses'\n",
                    serde_json::to_string(&urls[0]).unwrap(),
                    serde_json::to_string(FIXTURE_KEY).unwrap()
                );
                std::fs::write(home.join("config.toml"), config).unwrap();
                Gateway::new(data).unwrap()
            }
            ClientId::Claude => {
                let settings = json!({"env": {
                    "ANTHROPIC_BASE_URL": urls[0], "ANTHROPIC_AUTH_TOKEN": FIXTURE_KEY
                }});
                std::fs::write(home.join("settings.json"), settings.to_string()).unwrap();
                Gateway::new(dir.path().join("codex-data"))
                    .unwrap()
                    .companion(data)
                    .unwrap()
            }
        };
        let usage = Service::new(&dir.path().join("usage-data"));
        assert!(usage.state().error.is_none());
        gateway.set_usage(usage.clone());
        let fixture = Self {
            gateway,
            usage,
            home,
            _dir: dir,
        };
        let reservation = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        fixture.edit(Edit::Settings {
            settings: Settings {
                port: reservation.local_addr().unwrap().port(),
                max_retries: 1,
                first_byte_seconds: 5,
                idle_seconds: 5,
                total_seconds: 10,
                connect_seconds: 2,
                queue_seconds: 2,
                ..Settings::default()
            },
        });
        for (index, url) in urls.iter().enumerate() {
            fixture.edit(Edit::SaveProvider {
                id: None,
                base_url: url.clone(),
                token: FIXTURE_KEY.into(),
                name: Some(format!("fixture-P{}", index + 1)),
            });
        }
        drop(reservation);
        fixture
    }

    fn edit(&self, edit: Edit) {
        self.gateway
            .edit(edit, &self.gateway.view().revision, &self.home)
            .unwrap();
    }

    async fn start(&self) {
        self.gateway
            .start(&self.gateway.view().revision, &self.home)
            .await
            .unwrap();
    }

    async fn stop(&self) {
        tokio::time::timeout(WAIT, self.gateway.stop())
            .await
            .unwrap()
            .unwrap();
    }
}

async fn request(gateway: &Gateway, path: &str, body: &[u8]) -> Response<Incoming> {
    let client: HttpClient =
        Client::builder(TokioExecutor::new()).build(Connector::new(Duration::from_secs(2), 0));
    let token = gateway.0.inner.lock().unwrap().store.local_token.clone();
    let request = Request::builder()
        .method("POST")
        .uri(format!(
            "http://127.0.0.1:{}{path}",
            gateway.view().settings.port
        ))
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(full(body.to_vec()))
        .unwrap();
    tokio::time::timeout(WAIT, client.request(request))
        .await
        .unwrap()
        .unwrap()
}

async fn response_bytes(response: Response<Incoming>) -> Bytes {
    tokio::time::timeout(WAIT, response.into_body().collect())
        .await
        .unwrap()
        .unwrap()
        .to_bytes()
}

fn fragmented(bytes: &[u8]) -> WireBody {
    let frames = bytes
        .chunks(7)
        .map(|chunk| Ok::<_, BoxError>(Frame::data(Bytes::copy_from_slice(chunk))))
        .collect::<Vec<_>>();
    StreamBody::new(futures_util::stream::iter(frames)).boxed_unsync()
}

fn records(service: &Service) -> Vec<Record> {
    service
        .query(|store| {
            let page = store.logs(&Filter::default())?;
            assert_eq!(page.total, page.rows.len() as u64);
            Ok(page.rows)
        })
        .unwrap()
}

async fn recorded(service: &Service, count: usize) -> Vec<Record> {
    let mut events = service.subscribe();
    tokio::time::timeout(WAIT, async {
        loop {
            let rows = records(service);
            if rows.len() >= count {
                assert_eq!(rows.len(), count);
                assert!(service.state().error.is_none());
                return rows;
            }
            events.recv().await.expect("fixture usage writer stopped");
        }
    })
    .await
    .expect("fixture usage record was not written")
}

async fn no_new_records(service: &Service, count: usize) {
    let mut events = service.subscribe();
    assert_eq!(records(service).len(), count);
    assert!(
        tokio::time::timeout(Duration::from_millis(100), events.recv())
            .await
            .is_err()
    );
    assert_eq!(records(service).len(), count);
}

async fn settled(check: impl Fn() -> bool) {
    tokio::time::timeout(WAIT, async {
        while !check() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("fixture gateway did not settle");
}

async fn websocket(gateway: &Gateway) -> yawc::TcpWebSocket {
    let token = gateway.0.inner.lock().unwrap().store.local_token.clone();
    tokio::time::timeout(
        WAIT,
        yawc::WebSocket::connect(
            format!(
                "ws://127.0.0.1:{}/v1/responses",
                gateway.view().settings.port
            )
            .parse()
            .unwrap(),
        )
        .with_request(
            yawc::HttpRequest::builder().header("authorization", format!("Bearer {token}")),
        ),
    )
    .await
    .unwrap()
    .unwrap()
}

async fn next_frame(socket: &mut yawc::TcpWebSocket) -> yawc::Frame {
    tokio::time::timeout(WAIT, socket.next())
        .await
        .unwrap()
        .expect("fixture websocket closed")
}

async fn next_text_frame(socket: &mut yawc::TcpWebSocket) -> yawc::Frame {
    tokio::time::timeout(WAIT, async {
        loop {
            let frame = socket
                .next()
                .await
                .expect("fixture websocket closed before text frame");
            match frame.opcode() {
                yawc::OpCode::Text => return frame,
                yawc::OpCode::Ping | yawc::OpCode::Pong => continue,
                yawc::OpCode::Close => panic!("fixture websocket sent Close before text frame"),
                yawc::OpCode::Binary => {
                    panic!("fixture websocket sent Binary instead of text frame")
                }
                opcode => panic!("fixture websocket sent unexpected opcode: {opcode:?}"),
            }
        }
    })
    .await
    .expect("fixture websocket did not send a text frame within WAIT")
}

fn completed(turn: u64) -> String {
    json!({"type":"response.completed","response":{
        "id": format!("fixture-ws-response-{turn}"),
        "model": format!("gpt-ws-{turn}-fixture"), "status":"completed",
        "usage":{"input_tokens":100 * turn,"output_tokens":10 * turn,
            "input_tokens_details":{"cached_tokens":40 * turn}}
    }})
    .to_string()
}

#[tokio::test]
async fn http_usage_preserves_request_and_response_bytes() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let observed = seen.clone();
    let upstream = server(move |request, _| {
        let observed = observed.clone();
        async move {
            let (parts, body) = request.into_parts();
            let bytes = body.collect().await.unwrap().to_bytes();
            observed
                .lock()
                .unwrap()
                .push((parts.uri.to_string(), bytes));
            Response::builder()
                .header("content-type", "application/json")
                .header("x-fixture", "preserved")
                .body(full(HTTP_RESPONSE))
                .unwrap()
        }
    })
    .await;
    let fixture = Fixture::new(ClientId::Codex, &[upstream.url("/v1")]).await;
    fixture.start().await;
    let response = request(
        &fixture.gateway,
        "/v1/responses?item=1&item=2",
        HTTP_REQUEST,
    )
    .await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["x-fixture"], "preserved");
    assert_eq!(response_bytes(response).await.as_ref(), HTTP_RESPONSE);
    assert_eq!(
        *seen.lock().unwrap(),
        vec![(
            "/v1/responses?item=1&item=2".into(),
            Bytes::from_static(HTTP_REQUEST)
        )]
    );

    let rows = recorded(&fixture.usage, 1).await;
    let row = &rows[0];
    assert_eq!(row.client, "codex");
    assert_eq!(row.source, "proxy");
    assert!(row.completed);
    assert_eq!(row.attempts.len(), 1);
    let attempt = &row.attempts[0];
    assert_eq!(
        attempt.provider.as_deref(),
        Some(fixture.gateway.view().providers[0].id.as_str())
    );
    assert_eq!(
        attempt.requested_model.as_deref(),
        Some("gpt-request-fixture")
    );
    assert_eq!(
        attempt.response_model.as_deref(),
        Some("gpt-response-fixture")
    );
    assert_eq!(attempt.response_id, Some(safe_id("fixture-http-response")));
    assert_eq!(attempt.status, Some(200));
    assert_eq!(attempt.outcome, "success");
    assert_eq!(attempt.transport, "http");
    assert!(!attempt.stream);
    assert_eq!(attempt.tokens.input, Some(60));
    assert_eq!(attempt.tokens.cache_read, Some(40));
    assert_eq!(attempt.tokens.output, Some(20));
    assert_eq!(attempt.tokens.total(), Some(120));
    fixture.stop().await;
}

#[tokio::test]
async fn claude_sse_preserves_wire_bytes_and_normalizes_cache_usage() {
    let body = br#" {"model":"claude-request-fixture","stream":true,"max_tokens":32,
        "messages":[{"role":"user","content":"fixture"}],"unknown":9007199254740993} "#;
    let sse = concat!(
        ": fixture keepalive\r\n\r\n",
        "event: message_start\r\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"fixture-claude-response\",\"type\":\"message\",\"model\":\"claude-response-fixture\",\"usage\":{\"input_tokens\":11,\"output_tokens\":0,\"cache_read_input_tokens\":30,\"cache_creation_input_tokens\":13,\"cache_creation\":{\"ephemeral_5m_input_tokens\":5,\"ephemeral_1h_input_tokens\":8}}}}\r\n\r\n",
        "event: content_block_delta\r\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"fixture\"}}\r\n\r\n",
        "event: message_delta\r\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":7}}\r\n\r\n",
        "event: message_stop\r\ndata: {\"type\":\"message_stop\"}\r\n\r\n"
    );
    let seen = Arc::new(Mutex::new(Vec::new()));
    let observed = seen.clone();
    let upstream = server(move |request, _| {
        let observed = observed.clone();
        async move {
            let (parts, body) = request.into_parts();
            let bytes = body.collect().await.unwrap().to_bytes();
            observed
                .lock()
                .unwrap()
                .push((parts.uri.to_string(), bytes));
            Response::builder()
                .header("content-type", "text/event-stream; charset=utf-8")
                .header("x-fixture", "sse-preserved")
                .body(fragmented(sse.as_bytes()))
                .unwrap()
        }
    })
    .await;
    let fixture = Fixture::new(ClientId::Claude, &[upstream.url("/deployment")]).await;
    fixture.start().await;
    let response = request(&fixture.gateway, "/v1/messages?beta=fixture", body).await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.headers()["content-type"],
        "text/event-stream; charset=utf-8"
    );
    assert_eq!(response.headers()["x-fixture"], "sse-preserved");
    assert_eq!(response_bytes(response).await.as_ref(), sse.as_bytes());
    assert_eq!(
        *seen.lock().unwrap(),
        vec![(
            "/deployment/v1/messages?beta=fixture".into(),
            Bytes::from_static(body)
        )]
    );

    let rows = recorded(&fixture.usage, 1).await;
    let row = &rows[0];
    assert_eq!(row.client, "claude");
    assert!(row.completed);
    assert_eq!(row.attempts.len(), 1);
    let attempt = &row.attempts[0];
    assert_eq!(attempt.transport, "http");
    assert!(attempt.stream);
    assert_eq!(attempt.status, Some(200));
    assert_eq!(attempt.outcome, "success");
    assert_eq!(
        attempt.requested_model.as_deref(),
        Some("claude-request-fixture")
    );
    assert_eq!(
        attempt.response_model.as_deref(),
        Some("claude-response-fixture")
    );
    assert_eq!(
        attempt.response_id,
        Some(safe_id("fixture-claude-response"))
    );
    assert_eq!(attempt.tokens.input, Some(11));
    assert_eq!(attempt.tokens.output, Some(7));
    assert_eq!(attempt.tokens.cache_read, Some(30));
    assert_eq!(attempt.tokens.cache_write, Some(13));
    assert_eq!(attempt.tokens.cache_write_5m, Some(5));
    assert_eq!(attempt.tokens.cache_write_1h, Some(8));
    assert_eq!(attempt.tokens.total(), Some(61));
    assert!(attempt.first_token_ms.is_some());
    fixture.stop().await;
}

#[tokio::test]
async fn failed_provider_then_success_keeps_two_attempts_in_one_record() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let first_seen = seen.clone();
    let first = server(move |request, _| {
        let first_seen = first_seen.clone();
        async move {
            let body = request.into_body().collect().await.unwrap().to_bytes();
            first_seen.lock().unwrap().push(("P1", body));
            Response::builder()
                .status(503)
                .header("content-type", "application/json")
                .body(full(
                    r#"{"error":{"type":"server_error","message":"fixture unavailable"}}"#,
                ))
                .unwrap()
        }
    })
    .await;
    let second_seen = seen.clone();
    let second = server(move |request, _| {
        let second_seen = second_seen.clone();
        async move {
            let body = request.into_body().collect().await.unwrap().to_bytes();
            second_seen.lock().unwrap().push(("P2", body));
            Response::builder()
                .header("content-type", "application/json")
                .body(full(HTTP_RESPONSE))
                .unwrap()
        }
    })
    .await;
    let fixture = Fixture::new(ClientId::Codex, &[first.url("/v1"), second.url("/v1")]).await;
    fixture.edit(Edit::Mode {
        mode: "auto".into(),
    });
    let provider_ids = fixture
        .gateway
        .view()
        .providers
        .iter()
        .map(|p| p.id.clone())
        .collect::<Vec<_>>();
    fixture.start().await;
    let response = request(&fixture.gateway, "/v1/responses", HTTP_REQUEST).await;
    assert_eq!(response.status(), 200);
    assert_eq!(response_bytes(response).await.as_ref(), HTTP_RESPONSE);
    assert_eq!(
        *seen.lock().unwrap(),
        vec![
            ("P1", Bytes::from_static(HTTP_REQUEST)),
            ("P2", Bytes::from_static(HTTP_REQUEST))
        ]
    );
    let rows = recorded(&fixture.usage, 1).await;
    let row = &rows[0];
    assert!(row.completed);
    assert_eq!(row.attempts.len(), 2);
    assert_ne!(row.attempts[0].id, row.attempts[1].id);
    for (attempt, provider) in row.attempts.iter().zip(&provider_ids) {
        assert_eq!(attempt.provider.as_ref(), Some(provider));
        assert_eq!(
            attempt.requested_model.as_deref(),
            Some("gpt-request-fixture")
        );
    }
    assert_eq!(row.attempts[0].status, Some(503));
    assert_eq!(row.attempts[0].outcome, "rejected");
    assert_eq!(row.attempts[0].tokens.total(), None);
    assert_eq!(row.attempts[1].status, Some(200));
    assert_eq!(row.attempts[1].outcome, "success");
    assert_eq!(row.tokens().total(), Some(120));
    let totals = fixture
        .usage
        .query(|store| Ok(store.dashboard(&Filter::default())?.totals))
        .unwrap();
    assert_eq!(totals.requests, 1);
    assert_eq!(totals.success, 1);
    fixture.stop().await;
}

#[tokio::test]
async fn responses_websocket_records_each_turn_without_idle_records() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let observed = seen.clone();
    let upstream = server(move |mut request, tasks| {
        let observed = observed.clone();
        async move {
            let (response, upgrade) = yawc::WebSocket::upgrade(&mut request).unwrap();
            tasks.spawn(async move {
                let mut socket = upgrade.await.unwrap();
                let mut turn = 0;
                while let Some(frame) = socket.next().await {
                    if frame.opcode() == yawc::OpCode::Close {
                        break;
                    }
                    if frame.opcode() == yawc::OpCode::Ping {
                        socket
                            .send(yawc::Frame::pong(frame.payload().to_vec()))
                            .await
                            .unwrap();
                        continue;
                    }
                    if frame.opcode().is_control() {
                        continue;
                    }
                    observed.lock().unwrap().push(frame.payload().to_vec());
                    turn += 1;
                    socket.send(yawc::Frame::text(WS_DELTA)).await.unwrap();
                    socket
                        .send(yawc::Frame::text(completed(turn)))
                        .await
                        .unwrap();
                }
            });
            response.map(|_| replay::empty())
        }
    })
    .await;
    let fixture = Fixture::new(ClientId::Codex, &[upstream.url("/v1")]).await;
    fixture.start().await;
    let mut socket = websocket(&fixture.gateway).await;
    socket
        .send(yawc::Frame::ping(b"fixture-idle".to_vec()))
        .await
        .unwrap();
    assert_eq!(next_frame(&mut socket).await.opcode(), yawc::OpCode::Pong);
    no_new_records(&fixture.usage, 0).await;
    let mut sent = Vec::new();
    for turn in 1..=2u64 {
        let mut create = json!({"type":"response.create","model":format!("gpt-ws-{turn}-fixture"),
            "input":"fixture", "unknown":9007199254740993u64});
        if turn > 1 {
            create["previous_response_id"] = json!("fixture-ws-response-1");
        }
        let text = create.to_string();
        sent.push(text.as_bytes().to_vec());
        socket.send(yawc::Frame::text(text)).await.unwrap();
        assert_eq!(
            next_text_frame(&mut socket).await.payload().as_ref(),
            WS_DELTA.as_bytes()
        );
        assert_eq!(
            next_text_frame(&mut socket).await.payload().as_ref(),
            completed(turn).as_bytes()
        );
        let rows = recorded(&fixture.usage, turn as usize).await;
        let response_id = safe_id(&format!("fixture-ws-response-{turn}"));
        let row = rows
            .iter()
            .find(|row| row.final_attempt().unwrap().response_id.as_ref() == Some(&response_id))
            .unwrap();
        assert!(row.completed);
        assert_eq!(row.attempts.len(), 1);
        let attempt = &row.attempts[0];
        assert_eq!(attempt.transport, "websocket");
        assert_eq!(attempt.status, Some(101));
        assert_eq!(attempt.outcome, "success");
        assert_eq!(
            attempt.requested_model,
            Some(format!("gpt-ws-{turn}-fixture"))
        );
        assert_eq!(attempt.tokens.input, Some(60 * turn));
        assert_eq!(attempt.tokens.cache_read, Some(40 * turn));
        assert_eq!(attempt.tokens.output, Some(10 * turn));
        assert!(attempt.first_token_ms.is_some());
        settled(|| fixture.gateway.view().providers[0].active_requests == 0).await;
        no_new_records(&fixture.usage, turn as usize).await;
    }
    assert_eq!(*seen.lock().unwrap(), sent);
    drop(socket);
    fixture.stop().await;
    no_new_records(&fixture.usage, 2).await;
}

#[tokio::test]
async fn cancelled_bridge_releases_slot_and_next_turn_records_independently() {
    let hits = Arc::new(AtomicUsize::new(0));
    let observed = hits.clone();
    let upstream = server(move |request, _| {
        let observed = observed.clone();
        async move {
            let _ = request.into_body().collect().await.unwrap();
            let turn = observed.fetch_add(1, Ordering::SeqCst);
            let body = if turn == 0 {
                let stream = async_stream::try_stream! {
                    yield Frame::data(Bytes::from_static(b"data: {\"type\":\"response.created\",\"response\":{\"id\":\"fixture-cancelled-response\"}}\n\n"));
                    yield Frame::data(Bytes::from(format!("data: {WS_DELTA}\n\n")));
                    std::future::pending::<()>().await;
                };
                StreamBody::new(stream).map_err(|error: std::io::Error| -> BoxError { error.into() }).boxed_unsync()
            } else {
                full(format!("data: {}\n\n", completed(2)))
            };
            Response::builder().header("content-type", "text/event-stream").body(body).unwrap()
        }
    }).await;
    let fixture = Fixture::new(ClientId::Codex, &[upstream.url("/v1")]).await;
    let provider = fixture.gateway.view().providers[0].id.clone();
    fixture.edit(Edit::WebsocketProvider {
        id: provider.clone(),
        supports_websocket: false,
    });
    fixture.edit(Edit::ConcurrencyProvider {
        id: provider,
        max_concurrency: 1,
    });
    fixture.start().await;
    let mut socket = websocket(&fixture.gateway).await;
    socket
        .send(yawc::Frame::text(
            r#"{"type":"response.create","model":"gpt-ws-1-fixture","input":"fixture"}"#,
        ))
        .await
        .unwrap();
    assert!(next_text_frame(&mut socket)
        .await
        .as_str()
        .contains("response.created"));
    assert_eq!(
        next_text_frame(&mut socket).await.payload().as_ref(),
        WS_DELTA.as_bytes()
    );
    assert_eq!(fixture.gateway.view().providers[0].active_requests, 1);
    no_new_records(&fixture.usage, 0).await;
    socket
        .send(yawc::Frame::text(r#"{"type":"response.cancel"}"#))
        .await
        .unwrap();
    assert!(next_text_frame(&mut socket)
        .await
        .as_str()
        .contains("response.cancelled"));
    settled(|| fixture.gateway.view().providers[0].active_requests == 0).await;
    let cancelled = recorded(&fixture.usage, 1).await;
    assert!(!cancelled[0].completed);
    assert_eq!(cancelled[0].attempts.len(), 1);
    assert_eq!(cancelled[0].attempts[0].outcome, "cancelled");
    assert_eq!(cancelled[0].attempts[0].transport, "bridge");
    assert_eq!(cancelled[0].attempts[0].tokens.total(), None);
    assert!(cancelled[0].attempts[0].first_token_ms.is_some());

    socket
        .send(yawc::Frame::text(
            r#"{"type":"response.create","model":"gpt-ws-2-fixture","input":"fixture"}"#,
        ))
        .await
        .unwrap();
    assert_eq!(
        next_text_frame(&mut socket).await.payload().as_ref(),
        completed(2).as_bytes()
    );
    let rows = recorded(&fixture.usage, 2).await;
    let success = rows.iter().find(|row| row.completed).unwrap();
    assert_ne!(success.id, cancelled[0].id);
    assert_eq!(success.attempts.len(), 1);
    assert_eq!(success.attempts[0].outcome, "success");
    assert_eq!(success.attempts[0].transport, "bridge");
    assert_eq!(success.attempts[0].status, Some(200));
    assert_eq!(success.attempts[0].tokens.total(), Some(220));
    settled(|| fixture.gateway.view().providers[0].active_requests == 0).await;
    assert_eq!(fixture.gateway.view().providers[0].health.failures, 0);
    assert_eq!(hits.load(Ordering::SeqCst), 2);
    drop(socket);
    fixture.stop().await;
    no_new_records(&fixture.usage, 2).await;
}

#[tokio::test]
async fn unavailable_usage_database_does_not_interrupt_http_or_sse() {
    let broken_dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(broken_dir.path().join("usage/usage.sqlite")).unwrap();
    let broken_usage = Service::new(broken_dir.path());
    assert!(broken_usage.state().error.is_some());
    assert!(broken_usage
        .query(|store| store.logs(&Filter::default()))
        .is_err());
    let hits = Arc::new(AtomicUsize::new(0));
    let observed = hits.clone();
    let upstream = server(move |request, _| {
        let observed = observed.clone();
        async move {
            let body = request.into_body().collect().await.unwrap().to_bytes();
            let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
            observed.fetch_add(1, Ordering::SeqCst);
            if value["stream"] == true {
                Response::builder()
                    .header("content-type", "text/event-stream")
                    .body(fragmented(format!("data: {}\n\n", completed(1)).as_bytes()))
                    .unwrap()
            } else {
                Response::builder()
                    .header("content-type", "application/json")
                    .body(full(HTTP_RESPONSE))
                    .unwrap()
            }
        }
    })
    .await;
    let fixture = Fixture::new(ClientId::Codex, &[upstream.url("/v1")]).await;
    fixture.gateway.set_usage(broken_usage.clone());
    fixture.start().await;
    let mut events = broken_usage.subscribe();
    for stream in [false, true] {
        let body =
            json!({"model":"gpt-ws-1-fixture","stream":stream,"input":"fixture"}).to_string();
        let response = request(&fixture.gateway, "/v1/responses", body.as_bytes()).await;
        assert_eq!(response.status(), 200);
        let expected = if stream {
            format!("data: {}\n\n", completed(1)).into_bytes()
        } else {
            HTTP_RESPONSE.to_vec()
        };
        assert_eq!(response_bytes(response).await.as_ref(), expected.as_slice());
        tokio::time::timeout(WAIT, events.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(broken_usage.state().error.is_some());
        settled(|| fixture.gateway.view().providers[0].active_requests == 0).await;
    }
    assert_eq!(hits.load(Ordering::SeqCst), 2);
    assert_eq!(fixture.gateway.view().providers[0].health.failures, 0);
    fixture.stop().await;
}
