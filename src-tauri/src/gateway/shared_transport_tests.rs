// 覆盖共享入口的 HTTP/SSE/WS 原始字节透传。
//! Real loopback transport tests for the shared client listener.
use super::{replay, ClientId, Edit, Gateway};
use bytes::Bytes;
use futures_util::{stream, SinkExt, StreamExt};
use http_body_util::{BodyExt, StreamBody};
use hyper::{body::Incoming, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use std::{
    convert::Infallible,
    future::Future,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::Notify,
    task::JoinHandle,
};

const MODEL: &str = "fixture-shared-model";
const CODEX_UPSTREAM_TOKEN: &str = "fixture-codex-upstream-token";
const CLAUDE_UPSTREAM_TOKEN: &str = "fixture-claude-upstream-token";
const REQUEST_BODY: &[u8] = br#"{ "model" : "fixture-shared-model", "input" : [{"role":"user","content":"fixture"}], "future" : { "integer" : 9007199254740993 }, "stream" : true }"#;
const CLAUDE_REQUEST_BODY: &[u8] =
    br#"{"model":"fixture-shared-model","messages":[{"role":"user","content":"fixture"}]}"#;
const SSE_BODY: &[u8] = b": fixture-comment\r\n\r\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"fixture-shared-response\",\"model\":\"fixture-shared-model\",\"status\":\"completed\",\"output\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\r\n\r\n";
const JSON_BODY: &[u8] = br#"{"ok":true,"source":"fixture"}"#;
const WAIT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug)]
struct SeenRequest {
    path_and_query: String,
    body: Vec<u8>,
    authorization: Option<String>,
}

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
    F: Fn(Request<Incoming>) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Response<super::replay::WireBody>> + Send + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let tasks = TaskScope::default();
    let accept_tasks = tasks.clone();
    let handler = Arc::new(handler);
    tasks.spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let handler = handler.clone();
            accept_tasks.spawn(async move {
                let service = hyper::service::service_fn(move |request| {
                    let handler = handler.clone();
                    async move { Ok::<_, Infallible>(handler(request).await) }
                });
                let connection = hyper::server::conn::http1::Builder::new()
                    .preserve_header_case(true)
                    .half_close(true)
                    .serve_connection(TokioIo::new(stream), service)
                    .with_upgrades();
                let _ = connection.await;
            });
        }
    });
    LoopbackServer { port, tasks }
}

fn fragmented_body(chunks: &[&[u8]]) -> super::replay::WireBody {
    let frames = chunks
        .iter()
        .map(|chunk| Ok::<_, Infallible>(hyper::body::Frame::data(Bytes::copy_from_slice(chunk))))
        .collect::<Vec<_>>();
    StreamBody::new(stream::iter(frames))
        .map_err(|never| match never {})
        .boxed_unsync()
}

struct Fixture {
    codex: Gateway,
    claude: Gateway,
    codex_home: PathBuf,
    claude_home: PathBuf,
    listener_port: u16,
    _dir: tempfile::TempDir,
}

impl Fixture {
    async fn new(upstream: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let codex = Gateway::new(data.clone()).unwrap();
        let claude = codex.companion(data.join("claude")).unwrap();
        let reservation = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let listener_port = reservation.local_addr().unwrap().port();
        codex
            .set_listener_port(listener_port, &codex.listener_view().revision)
            .unwrap();
        drop(reservation);

        let codex_home = dir.path().join("home/codex");
        let claude_home = dir.path().join("home/claude");
        seed_configs(&codex_home, &claude_home);
        add_provider(&codex, &codex_home, upstream, CODEX_UPSTREAM_TOKEN);
        add_provider(&claude, &claude_home, upstream, CLAUDE_UPSTREAM_TOKEN);
        configure(&codex, &codex_home);
        configure(&claude, &claude_home);

        Self {
            codex,
            claude,
            codex_home,
            claude_home,
            listener_port,
            _dir: dir,
        }
    }

    async fn start(&self) {
        self.codex
            .start(&self.codex.view().revision, &self.codex_home)
            .await
            .unwrap();
        self.claude
            .start(&self.claude.view().revision, &self.claude_home)
            .await
            .unwrap();
        let running = self.codex.listener_view().running_clients;
        assert!(running.contains(&ClientId::Codex));
        assert!(running.contains(&ClientId::Claude));
        assert_eq!(self.claude.listener_view().port, self.listener_port);
    }

    fn local_token(&self, gateway: &Gateway) -> String {
        gateway.0.inner.lock().unwrap().store.local_token.clone()
    }
}

fn seed_configs(codex_home: &Path, claude_home: &Path) {
    std::fs::create_dir_all(codex_home).unwrap();
    std::fs::create_dir_all(claude_home).unwrap();
    std::fs::write(
        codex_home.join("config.toml"),
        "model_provider = 'custom'\n[model_providers.custom]\nbase_url = 'https://codex-original.example.invalid/v1'\nexperimental_bearer_token = 'fixture-original-codex-token'\nwire_api = 'responses'\n",
    )
    .unwrap();
    std::fs::write(
        claude_home.join("settings.json"),
        br#"{"env":{"ANTHROPIC_BASE_URL":"https://claude-original.example.invalid/v1","ANTHROPIC_AUTH_TOKEN":"fixture-original-claude-token"}}"#,
    )
    .unwrap();
}

fn add_provider(gateway: &Gateway, home: &Path, base_url: &str, token: &str) {
    gateway
        .edit(
            Edit::SaveProvider {
                id: None,
                base_url: base_url.to_owned(),
                token: token.to_owned(),
                name: Some("fixture-upstream".into()),
            },
            &gateway.view().revision,
            home,
        )
        .unwrap();
    let id = gateway.view().providers[0].id.clone();
    gateway
        .edit(
            Edit::ModelsProvider {
                id,
                allowed_models: Some(vec![MODEL.into()]),
            },
            &gateway.view().revision,
            home,
        )
        .unwrap();
}

fn configure(gateway: &Gateway, home: &Path) {
    let mut settings = gateway.view().settings;
    settings.max_retries = 1;
    settings.connect_seconds = 2;
    settings.first_byte_seconds = 30;
    settings.idle_seconds = 30;
    settings.total_seconds = 60;
    settings.queue_seconds = 2;
    gateway
        .edit(Edit::Settings { settings }, &gateway.view().revision, home)
        .unwrap();
}

fn auth(headers: &hyper::HeaderMap) -> Option<String> {
    headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

fn recorded(seen: &Arc<Mutex<Vec<SeenRequest>>>) -> Vec<SeenRequest> {
    seen.lock().unwrap().clone()
}

type Sender = hyper::client::conn::http1::SendRequest<super::replay::WireBody>;

async fn connection(port: u16) -> (Sender, JoinHandle<()>) {
    let socket = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let (sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(socket))
        .await
        .unwrap();
    let task = tokio::spawn(async move {
        let _ = connection.await;
    });
    (sender, task)
}

async fn send(
    sender: &mut Sender,
    port: u16,
    path: &str,
    token: &str,
    body: &[u8],
    accept: Option<&str>,
) -> Response<Incoming> {
    sender.ready().await.unwrap();
    let mut request = Request::builder()
        .method("POST")
        .uri(path)
        .header("host", format!("127.0.0.1:{port}"))
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json");
    if let Some(accept) = accept {
        request = request.header("accept", accept);
    }
    sender
        .send_request(
            request
                .body(replay::full(Bytes::copy_from_slice(body)))
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn response_bytes(response: Response<Incoming>) -> Vec<u8> {
    response
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec()
}

async fn one_request(gateway: &Gateway, path: &str, body: &[u8]) -> (StatusCode, Vec<u8>) {
    let port = gateway.listener_view().port;
    let token = gateway.0.inner.lock().unwrap().store.local_token.clone();
    let (mut sender, task) = connection(port).await;
    let response = tokio::time::timeout(WAIT, send(&mut sender, port, path, &token, body, None))
        .await
        .expect("fixture request should receive a response");
    let status = response.status();
    let body = response_bytes(response).await;
    drop(sender);
    task.abort();
    let _ = task.await;
    (status, body)
}

async fn wait_active(gateway: &Gateway) {
    tokio::time::timeout(WAIT, async {
        while gateway.view().active_connections == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("request should enter the gateway before stop");
}

async fn read_status(stream: &mut TcpStream) -> std::io::Result<u16> {
    let mut header = Vec::new();
    let mut byte = [0u8; 1];
    while !header.windows(4).any(|window| window == b"\r\n\r\n") {
        if stream.read(&mut byte).await? == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "gateway closed before returning a status line",
            ));
        }
        header.push(byte[0]);
        if header.len() > 16 * 1024 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "fixture response headers exceeded the test limit",
            ));
        }
    }
    let end = header
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .unwrap();
    std::str::from_utf8(&header[..end])
        .ok()
        .and_then(|text| text.lines().next())
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|status| status.parse().ok())
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid status line"))
}

#[tokio::test]
async fn shared_tcp_connection_reuses_client_auth_and_preserves_request_and_sse_bytes() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let upstream_seen = seen.clone();
    let upstream = server(move |request| {
        let seen = upstream_seen.clone();
        async move {
            let (parts, body) = request.into_parts();
            let body = body.collect().await.unwrap().to_bytes().to_vec();
            seen.lock().unwrap().push(SeenRequest {
                path_and_query: parts
                    .uri
                    .path_and_query()
                    .map(|value| value.as_str().to_owned())
                    .unwrap_or_default(),
                body,
                authorization: auth(&parts.headers),
            });
            if parts.uri.path().ends_with("/messages") {
                Response::builder()
                    .status(StatusCode::OK)
                    .header("content-type", "application/json")
                    .body(replay::full(JSON_BODY))
                    .unwrap()
            } else {
                let split = [
                    &SSE_BODY[..7],
                    &SSE_BODY[7..31],
                    &SSE_BODY[31..113],
                    &SSE_BODY[113..],
                ];
                Response::builder()
                    .status(StatusCode::OK)
                    .header("content-type", "text/event-stream; charset=utf-8")
                    .body(fragmented_body(&split))
                    .unwrap()
            }
        }
    })
    .await;
    let fixture = Fixture::new(&upstream.url("/v1")).await;
    fixture.start().await;
    let codex_token = fixture.local_token(&fixture.codex);
    let claude_token = fixture.local_token(&fixture.claude);
    assert_ne!(codex_token, claude_token);

    let (mut sender, task) = connection(fixture.listener_port).await;
    let response = send(
        &mut sender,
        fixture.listener_port,
        "/codex/v1/responses?state=%2f&next=%2F+%2520",
        &codex_token,
        REQUEST_BODY,
        Some("text/event-stream"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response_bytes(response).await, SSE_BODY);

    let response = send(
        &mut sender,
        fixture.listener_port,
        "/claude/v1/messages",
        &codex_token,
        REQUEST_BODY,
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let _ = response_bytes(response).await;

    let response = send(
        &mut sender,
        fixture.listener_port,
        "/claude/v1/messages",
        &claude_token,
        REQUEST_BODY,
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response_bytes(response).await, JSON_BODY);

    let response = send(
        &mut sender,
        fixture.listener_port,
        "/codex/v1/responses",
        &claude_token,
        REQUEST_BODY,
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let _ = response_bytes(response).await;

    let response = send(
        &mut sender,
        fixture.listener_port,
        "/codex/v1/responses",
        &codex_token,
        REQUEST_BODY,
        Some("text/event-stream"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response_bytes(response).await, SSE_BODY);
    drop(sender);
    task.abort();
    let _ = task.await;

    let requests = recorded(&seen);
    assert_eq!(
        requests.len(),
        3,
        "only the three authorized requests reach upstream"
    );
    assert_eq!(
        requests[0].path_and_query,
        "/v1/responses?state=%2f&next=%2F+%2520"
    );
    assert_eq!(requests[0].body, REQUEST_BODY);
    assert_eq!(
        requests[0].authorization.as_deref(),
        Some("Bearer fixture-codex-upstream-token")
    );
    assert_eq!(requests[1].path_and_query, "/v1/messages");
    assert_eq!(requests[1].body, REQUEST_BODY);
    assert_eq!(
        requests[1].authorization.as_deref(),
        Some("Bearer fixture-claude-upstream-token")
    );
    assert_eq!(requests[2].path_and_query, "/v1/responses");
    assert_eq!(requests[2].body, REQUEST_BODY);
}

#[tokio::test]
async fn stopping_claude_cancels_an_incomplete_upload_and_keeps_codex_live() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let upstream_seen = seen.clone();
    let upstream = server(move |request| {
        let seen = upstream_seen.clone();
        async move {
            let (parts, body) = request.into_parts();
            let body = body.collect().await.unwrap().to_bytes().to_vec();
            seen.lock().unwrap().push(SeenRequest {
                path_and_query: parts
                    .uri
                    .path_and_query()
                    .map(|value| value.as_str().to_owned())
                    .unwrap_or_default(),
                body,
                authorization: auth(&parts.headers),
            });
            Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "application/json")
                .body(replay::full(JSON_BODY))
                .unwrap()
        }
    })
    .await;
    let fixture = Fixture::new(&upstream.url("/v1")).await;
    fixture.start().await;
    let token = fixture.local_token(&fixture.claude);
    let mut upload = TcpStream::connect(("127.0.0.1", fixture.listener_port))
        .await
        .unwrap();
    let headers = format!(
        "POST /claude/v1/messages HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: 4096\r\n\r\n",
        fixture.listener_port
    );
    upload.write_all(headers.as_bytes()).await.unwrap();
    upload
        .write_all(b"{\"model\":\"fixture-shared-model\"")
        .await
        .unwrap();
    wait_active(&fixture.claude).await;

    tokio::time::timeout(WAIT, fixture.claude.stop())
        .await
        .expect("stopping one client should not wait for its incomplete body")
        .unwrap();
    let status = tokio::time::timeout(WAIT, read_status(&mut upload))
        .await
        .expect("the shared listener should cancel the incomplete upload")
        .unwrap();
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE.as_u16());
    drop(upload);
    assert!(
        recorded(&seen).is_empty(),
        "incomplete body must not reach upstream"
    );

    let (status, body) = one_request(
        &fixture.codex,
        "/codex/v1/responses",
        br#"{"model":"fixture-shared-model","input":"fixture"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, JSON_BODY);
    assert_eq!(
        fixture.codex.listener_view().running_clients,
        vec![ClientId::Codex]
    );
    assert_eq!(recorded(&seen).len(), 1);
}

#[tokio::test]
async fn stopping_claude_cancels_first_byte_wait_while_codex_keeps_serving() {
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let upstream_started = started.clone();
    let upstream_release = release.clone();
    let upstream_seen = seen.clone();
    let upstream = server(move |request| {
        let started = upstream_started.clone();
        let release = upstream_release.clone();
        let seen = upstream_seen.clone();
        async move {
            let (parts, body) = request.into_parts();
            let body = body.collect().await.unwrap().to_bytes().to_vec();
            let path = parts
                .uri
                .path_and_query()
                .map(|value| value.as_str().to_owned())
                .unwrap_or_default();
            seen.lock().unwrap().push(SeenRequest {
                path_and_query: path.clone(),
                body,
                authorization: auth(&parts.headers),
            });
            if path.ends_with("/messages") {
                started.notify_one();
                release.notified().await;
            }
            Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "application/json")
                .body(replay::full(JSON_BODY))
                .unwrap()
        }
    })
    .await;
    let fixture = Fixture::new(&upstream.url("/v1")).await;
    fixture.start().await;

    let (sender, connection_task) = connection(fixture.listener_port).await;
    let claude_token = fixture.local_token(&fixture.claude);
    let request = Request::builder()
        .method("POST")
        .uri("/claude/v1/messages")
        .header("host", format!("127.0.0.1:{}", fixture.listener_port))
        .header("authorization", format!("Bearer {claude_token}"))
        .header("content-type", "application/json")
        .body(replay::full(CLAUDE_REQUEST_BODY))
        .unwrap();
    let request_task = tokio::spawn(async move {
        let mut sender = sender;
        sender.ready().await.unwrap();
        let response = sender.send_request(request).await.unwrap();
        let status = response.status();
        let body = response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec();
        (status, body)
    });
    tokio::time::timeout(WAIT, started.notified())
        .await
        .expect("upstream should receive Claude request and hold its first byte");
    tokio::time::timeout(WAIT, fixture.claude.stop())
        .await
        .expect("stopping Claude should cancel its first-byte wait")
        .unwrap();
    let (status, _) = tokio::time::timeout(WAIT, request_task)
        .await
        .expect("Claude request should be released by its shutdown")
        .unwrap();
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    connection_task.abort();
    let _ = connection_task.await;
    release.notify_one();

    let (status, body) = one_request(
        &fixture.codex,
        "/codex/v1/responses",
        br#"{"model":"fixture-shared-model","input":"fixture"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, JSON_BODY);
    assert_eq!(
        fixture.codex.listener_view().running_clients,
        vec![ClientId::Codex]
    );
    let requests = recorded(&seen);
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].path_and_query, "/v1/messages");
    assert_eq!(requests[1].path_and_query, "/v1/responses");
}

#[tokio::test]
async fn stopping_claude_closes_its_active_websocket_and_keeps_codex_live() {
    let ws_ready = Arc::new(Notify::new());
    let ws_frame = Arc::new(Notify::new());
    let ws_closed = Arc::new(Notify::new());
    let socket_tasks = TaskScope::default();
    let ready_signal = ws_ready.clone();
    let frame_signal = ws_frame.clone();
    let closed_signal = ws_closed.clone();
    let scoped_tasks = socket_tasks.clone();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let upstream_seen = seen.clone();
    let upstream = server(move |mut request| {
        let ready = ready_signal.clone();
        let frame = frame_signal.clone();
        let closed = closed_signal.clone();
        let tasks = scoped_tasks.clone();
        let seen = upstream_seen.clone();
        async move {
            if request
                .headers()
                .get("upgrade")
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| value.eq_ignore_ascii_case("websocket"))
            {
                let (response, upgrade) = yawc::WebSocket::upgrade(&mut request).unwrap();
                let upstream_auth = auth(request.headers());
                tasks.spawn(async move {
                    let Ok(mut socket) = upgrade.await else {
                        closed.notify_one();
                        return;
                    };
                    ready.notify_one();
                    while let Some(message) = socket.next().await {
                        if message.opcode() == yawc::OpCode::Close {
                            break;
                        }
                        if message.opcode().is_control() {
                            continue;
                        }
                        frame.notify_one();
                    }
                    closed.notify_one();
                });
                seen.lock().unwrap().push(SeenRequest {
                    path_and_query: request
                        .uri()
                        .path_and_query()
                        .map(|value| value.as_str().to_owned())
                        .unwrap_or_default(),
                    body: Vec::new(),
                    authorization: upstream_auth,
                });
                response.map(|_| replay::empty())
            } else {
                let (parts, body) = request.into_parts();
                let body = body.collect().await.unwrap().to_bytes().to_vec();
                seen.lock().unwrap().push(SeenRequest {
                    path_and_query: parts
                        .uri
                        .path_and_query()
                        .map(|value| value.as_str().to_owned())
                        .unwrap_or_default(),
                    body,
                    authorization: auth(&parts.headers),
                });
                Response::builder()
                    .status(StatusCode::OK)
                    .header("content-type", "application/json")
                    .body(replay::full(JSON_BODY))
                    .unwrap()
            }
        }
    })
    .await;
    let fixture = Fixture::new(&upstream.url("/v1")).await;
    fixture.start().await;

    let claude_token = fixture.local_token(&fixture.claude);
    let mut socket = yawc::WebSocket::connect(
        format!(
            "ws://127.0.0.1:{}/claude/v1/responses",
            fixture.listener_port
        )
        .parse()
        .unwrap(),
    )
    .with_options(yawc::Options::default().with_balanced_compression())
    .with_request(
        yawc::HttpRequest::builder().header("authorization", format!("Bearer {claude_token}")),
    )
    .await
    .unwrap();
    tokio::time::timeout(
        WAIT,
        socket.send(yawc::Frame::text(
            r#"{"type":"response.create","model":"fixture-shared-model","input":"fixture"}"#
                .to_owned(),
        )),
    )
    .await
    .expect("fixture should send a create frame")
    .unwrap();
    tokio::time::timeout(WAIT, ws_ready.notified())
        .await
        .expect("upstream websocket should be established");
    tokio::time::timeout(WAIT, ws_frame.notified())
        .await
        .expect("active create frame should reach upstream websocket");

    tokio::time::timeout(WAIT, fixture.claude.stop())
        .await
        .expect("stopping Claude should close its websocket session")
        .unwrap();
    let close = tokio::time::timeout(WAIT, socket.next())
        .await
        .expect("stopped Claude websocket should receive a close frame")
        .expect("stopped Claude websocket should stay readable until close");
    assert_eq!(close.opcode(), yawc::OpCode::Close);
    tokio::time::timeout(WAIT, ws_closed.notified())
        .await
        .expect("stopping Claude should release its upstream websocket");
    socket_tasks.close();

    let (status, body) = one_request(
        &fixture.codex,
        "/codex/v1/responses",
        br#"{"model":"fixture-shared-model","input":"fixture"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, JSON_BODY);
    assert_eq!(
        fixture.codex.listener_view().running_clients,
        vec![ClientId::Codex]
    );
    let requests = recorded(&seen);
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].path_and_query, "/v1/responses");
    assert_eq!(
        requests[0].authorization.as_deref(),
        Some("Bearer fixture-claude-upstream-token")
    );
    assert_eq!(requests[1].path_and_query, "/v1/responses");
}
