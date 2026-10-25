//! Regressions for replay boundaries after partial HTTP and native WebSocket output.
use super::{
    connector::Connector,
    replay::{full, WireBody},
    Edit, Gateway,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use http_body_util::BodyExt;
use hyper::{body::Incoming, Request, Response, StatusCode};
use hyper_util::{
    client::legacy::Client,
    rt::{TokioExecutor, TokioIo},
};
use serde_json::Value;
use sha1::{Digest, Sha1};
use std::{
    convert::Infallible,
    future::Future,
    io,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::JoinHandle,
};
use tokio_tungstenite::{
    tungstenite::{protocol::Role, Message as WsMessage},
    WebSocketStream,
};

const WAIT: Duration = Duration::from_secs(8);
const FIXTURE_TOKEN: &str = "fixture-only-token";
const PARTIAL_SSE: &[u8] =
    b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"fixture-partial-prefix\"}";
const SSE_CAPACITY: &[u8] = b"data: {\"type\":\"error\",\"error\":{\"code\":\"USAGE_LIMIT_EXCEEDED\",\"message\":\"fixture capacity\"}}\n\n";
const SSE_COMPLETED: &[u8] = b"data: {\"type\":\"response.completed\",\"response\":{\"id\":\"fixture-completed\",\"model\":\"fixture-model\",\"status\":\"completed\"}}\n\n";
const WS_CREATE: &str = r#"{"type":"response.create","model":"fixture-model","input":"fixture"}"#;
const WS_COMPLETED: &str = r#"{"type":"response.completed","response":{"id":"fixture-ws-response","model":"fixture-model","status":"completed"}}"#;

#[derive(Clone, Default)]
struct Tasks(Arc<Mutex<Vec<JoinHandle<()>>>>);

impl Tasks {
    fn spawn(&self, future: impl Future<Output = ()> + Send + 'static) {
        self.0.lock().unwrap().push(tokio::spawn(future));
    }

    fn close(&self) {
        let handles: Vec<_> = self.0.lock().unwrap().drain(..).collect();
        for handle in handles {
            handle.abort();
        }
    }
}

struct TestServer {
    port: u16,
    tasks: Tasks,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.tasks.close();
    }
}

async fn http_server<F, Fut>(handler: F) -> TestServer
where
    F: Fn(Request<Incoming>) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Response<WireBody>> + Send + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let tasks = Tasks::default();
    let accepted = tasks.clone();
    let handler = Arc::new(handler);
    tasks.spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let handler = handler.clone();
            accepted.spawn(async move {
                let service = hyper::service::service_fn(move |request| {
                    let handler = handler.clone();
                    async move { Ok::<_, Infallible>(handler(request).await) }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .with_upgrades()
                    .await;
            });
        }
    });
    TestServer { port, tasks }
}

async fn tcp_server<F, Fut>(handler: F) -> TestServer
where
    F: Fn(TcpStream) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let tasks = Tasks::default();
    let accepted = tasks.clone();
    let handler = Arc::new(handler);
    tasks.spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let handler = handler.clone();
            accepted.spawn(async move {
                handler(stream).await;
            });
        }
    });
    TestServer { port, tasks }
}

struct Fixture {
    dir: tempfile::TempDir,
    gateway: Gateway,
}

impl Fixture {
    async fn new(urls: Vec<String>, supports_websockets: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let first = urls
            .first()
            .cloned()
            .unwrap_or_else(|| "https://fixture.example.invalid/v1".into());
        std::fs::write(
            dir.path().join("config.toml"),
            format!(
                "model_provider='custom'\n[model_providers.custom]\nbase_url = {}\nexperimental_bearer_token = \"{FIXTURE_TOKEN}\"\nwire_api='responses'\nsupports_websockets = {supports_websockets}\n",
                serde_json::to_string(&first).unwrap()
            ),
        )
        .unwrap();
        let gateway = Gateway::new(dir.path().to_path_buf()).unwrap();
        let reservation = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = reservation.local_addr().unwrap().port();
        let mut settings = gateway.view().settings;
        settings.port = port;
        settings.max_retries = 1;
        settings.first_byte_seconds = 1;
        settings.idle_seconds = 1;
        settings.connect_seconds = 2;
        settings.queue_seconds = 2;
        settings.websocket_retry_seconds = 1;
        settings.capacity_retry_seconds = 1;
        gateway
            .edit(
                Edit::Settings { settings },
                &gateway.view().revision,
                dir.path(),
            )
            .unwrap();
        for (index, url) in urls.iter().enumerate() {
            gateway
                .edit(
                    Edit::SaveProvider {
                        search_only: None,
                        quota_access_token: None,
                        id: None,
                        base_url: url.clone(),
                        token: FIXTURE_TOKEN.into(),
                        name: Some(format!("fixture-provider-{}", index + 1)),
                    },
                    &gateway.view().revision,
                    dir.path(),
                )
                .unwrap();
        }
        gateway
            .edit(
                Edit::Mode {
                    mode: "auto".into(),
                },
                &gateway.view().revision,
                dir.path(),
            )
            .unwrap();
        drop(reservation);
        Self { dir, gateway }
    }

    async fn start(&self) {
        self.gateway
            .start(&self.gateway.view().revision, self.dir.path())
            .await
            .unwrap();
    }

    async fn stop(&self) {
        tokio::time::timeout(WAIT, self.gateway.stop())
            .await
            .expect("fixture gateway did not stop")
            .unwrap();
    }

    fn auth(&self) -> (String, u16) {
        let state = self.gateway.0.inner.lock().unwrap();
        (state.store.local_token.clone(), state.store.settings.port)
    }

    async fn request(&self, streaming: bool) -> Response<Incoming> {
        let client: Client<_, WireBody> =
            Client::builder(TokioExecutor::new()).build(Connector::new(Duration::from_secs(2), 0));
        let (token, port) = self.auth();
        let body = if streaming {
            br#"{"model":"fixture-model","stream":true,"input":"fixture"}"#.as_slice()
        } else {
            br#"{"model":"fixture-model","stream":false,"input":"fixture"}"#.as_slice()
        };
        let request = Request::builder()
            .method("POST")
            .uri(format!("http://127.0.0.1:{port}/v1/responses"))
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .header("thread-id", "fixture-partial-response")
            .body(full(Bytes::copy_from_slice(body)))
            .unwrap();
        client.request(request).await.unwrap()
    }

    async fn ws_client(&self) -> yawc::TcpWebSocket {
        let (token, port) = self.auth();
        yawc::WebSocket::connect(
            format!("ws://127.0.0.1:{port}/v1/responses?fixture=true")
                .parse()
                .unwrap(),
        )
        .with_options(yawc::Options::default().with_balanced_compression())
        .with_request(
            yawc::HttpRequest::builder()
                .header("authorization", format!("Bearer {token}"))
                .header("session_id", "fixture-partial-response-ws"),
        )
        .await
        .unwrap()
    }
}

fn sse_response(payload: &'static [u8]) -> Response<WireBody> {
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .body(full(Bytes::from_static(payload)))
        .unwrap()
}

fn json_response(payload: &'static [u8]) -> Response<WireBody> {
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/json")
        .body(full(Bytes::from_static(payload)))
        .unwrap()
}

fn model_list_response() -> Response<WireBody> {
    json_response(br#"{"object":"list","data":[{"id":"fixture-model","object":"model"}]}"#)
}

async fn read_http_request(stream: &mut TcpStream) -> io::Result<Vec<u8>> {
    let mut received = Vec::new();
    let mut chunk = [0; 2048];
    loop {
        let count = stream.read(&mut chunk).await?;
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "fixture request ended",
            ));
        }
        received.extend_from_slice(&chunk[..count]);
        let Some(boundary) = received.windows(4).position(|part| part == b"\r\n\r\n") else {
            continue;
        };
        let header = String::from_utf8_lossy(&received[..boundary]);
        let body_length = header
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .unwrap_or(0);
        if received.len() >= boundary + 4 + body_length {
            return Ok(received);
        }
    }
}

fn request_path(request: &[u8]) -> &str {
    std::str::from_utf8(request)
        .ok()
        .and_then(|request| request.lines().next())
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("")
}

async fn write_raw_response(
    stream: &mut TcpStream,
    content_type: &str,
    declared_length: usize,
    body: &[u8],
) -> io::Result<()> {
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {declared_length}\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.flush().await
}

#[allow(deprecated)]
fn reset_tcp(stream: &TcpStream) {
    stream.set_linger(Some(Duration::ZERO)).unwrap();
}

#[derive(Clone, Copy)]
enum HttpFault {
    PartialSse,
    EmptyResponse,
}

async fn fault_http_server(fault: HttpFault, requests: Arc<AtomicUsize>) -> TestServer {
    tcp_server(move |mut stream| {
        let requests = requests.clone();
        async move {
            let Ok(request) = read_http_request(&mut stream).await else {
                return;
            };
            let path = request_path(&request);
            if path.ends_with("/models") {
                let _ = write_raw_response(
                    &mut stream,
                    "application/json",
                    br#"{"object":"list","data":[{"id":"fixture-model","object":"model"}]}"#.len(),
                    br#"{"object":"list","data":[{"id":"fixture-model","object":"model"}]}"#,
                )
                .await;
                return;
            }
            if !path.ends_with("/responses") {
                return;
            }
            let attempt = requests.fetch_add(1, Ordering::SeqCst);
            if attempt == 0 {
                if matches!(fault, HttpFault::PartialSse) {
                    let expected = PARTIAL_SSE.len() + 128;
                    let _ =
                        write_raw_response(&mut stream, "text/event-stream", expected, PARTIAL_SSE)
                            .await;
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                reset_tcp(&stream);
                drop(stream);
            } else {
                let retry = br#"{"provider":"first-retry"}"#;
                let _ =
                    write_raw_response(&mut stream, "application/json", retry.len(), retry).await;
            }
        }
    })
    .await
}

async fn collect_body(response: Response<Incoming>) -> Bytes {
    tokio::time::timeout(WAIT, response.into_body().collect())
        .await
        .expect("fixture response body timed out")
        .unwrap()
        .to_bytes()
}

async fn body_until_error(response: Response<Incoming>) -> (Vec<u8>, Option<String>) {
    let mut body = response.into_body();
    let mut bytes = Vec::new();
    loop {
        match tokio::time::timeout(WAIT, body.frame())
            .await
            .expect("fixture response frame timed out")
        {
            Some(Ok(frame)) => {
                if let Some(data) = frame.data_ref() {
                    bytes.extend_from_slice(data);
                }
            }
            Some(Err(error)) => return (bytes, Some(error.to_string())),
            None => return (bytes, None),
        }
    }
}

async fn settled(gateway: &Gateway) {
    tokio::time::timeout(WAIT, async {
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

#[tokio::test]
async fn http_partial_sse_prefix_is_returned_with_error_without_replay_after_reset() {
    let first_requests = Arc::new(AtomicUsize::new(0));
    let first = fault_http_server(HttpFault::PartialSse, first_requests.clone()).await;
    let fallback_requests = Arc::new(AtomicUsize::new(0));
    let fallback_seen = fallback_requests.clone();
    let fallback = http_server(move |request| {
        let fallback_seen = fallback_seen.clone();
        async move {
            if request.method() == hyper::Method::GET && request.uri().path().ends_with("/models") {
                return model_list_response();
            }
            fallback_seen.fetch_add(1, Ordering::SeqCst);
            sse_response(SSE_COMPLETED)
        }
    })
    .await;
    let fixture = Fixture::new(
        vec![
            format!("http://127.0.0.1:{}/v1", first.port),
            format!("http://127.0.0.1:{}/v1", fallback.port),
        ],
        false,
    )
    .await;
    fixture.start().await;

    let response = fixture.request(true).await;
    assert_eq!(response.status(), StatusCode::OK);
    let (body, error) = body_until_error(response).await;
    settled(&fixture.gateway).await;
    fixture.stop().await;

    assert_eq!(
        body, PARTIAL_SSE,
        "the original incomplete SSE prefix must be preserved"
    );
    assert!(
        error.is_some(),
        "partial upstream output must end with a stream error"
    );
    assert_eq!(first_requests.load(Ordering::SeqCst), 1);
    assert_eq!(fallback_requests.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn http_zero_response_retries_the_same_provider_before_failover() {
    let first_requests = Arc::new(AtomicUsize::new(0));
    let first = fault_http_server(HttpFault::EmptyResponse, first_requests.clone()).await;
    let fallback_requests = Arc::new(AtomicUsize::new(0));
    let fallback_seen = fallback_requests.clone();
    let fallback = http_server(move |request| {
        let fallback_seen = fallback_seen.clone();
        async move {
            if request.method() == hyper::Method::GET && request.uri().path().ends_with("/models") {
                return model_list_response();
            }
            fallback_seen.fetch_add(1, Ordering::SeqCst);
            json_response(br#"{"provider":"fallback"}"#)
        }
    })
    .await;
    let fixture = Fixture::new(
        vec![
            format!("http://127.0.0.1:{}/v1", first.port),
            format!("http://127.0.0.1:{}/v1", fallback.port),
        ],
        false,
    )
    .await;
    fixture.start().await;

    let response = fixture.request(false).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = collect_body(response).await;
    settled(&fixture.gateway).await;
    fixture.stop().await;

    let value: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(value["provider"], "first-retry");
    assert_eq!(first_requests.load(Ordering::SeqCst), 2);
    assert_eq!(fallback_requests.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn http_first_sse_capacity_event_still_fails_over() {
    let first_requests = Arc::new(AtomicUsize::new(0));
    let first_seen = first_requests.clone();
    let first = http_server(move |request| {
        let first_seen = first_seen.clone();
        async move {
            if request.method() == hyper::Method::GET && request.uri().path().ends_with("/models") {
                return model_list_response();
            }
            first_seen.fetch_add(1, Ordering::SeqCst);
            sse_response(SSE_CAPACITY)
        }
    })
    .await;
    let fallback_requests = Arc::new(AtomicUsize::new(0));
    let fallback_seen = fallback_requests.clone();
    let fallback = http_server(move |request| {
        let fallback_seen = fallback_seen.clone();
        async move {
            if request.method() == hyper::Method::GET && request.uri().path().ends_with("/models") {
                return model_list_response();
            }
            fallback_seen.fetch_add(1, Ordering::SeqCst);
            sse_response(SSE_COMPLETED)
        }
    })
    .await;
    let fixture = Fixture::new(
        vec![
            format!("http://127.0.0.1:{}/v1", first.port),
            format!("http://127.0.0.1:{}/v1", fallback.port),
        ],
        false,
    )
    .await;
    fixture.start().await;

    let response = fixture.request(true).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = collect_body(response).await;
    settled(&fixture.gateway).await;
    fixture.stop().await;

    assert!(std::str::from_utf8(&body)
        .unwrap()
        .contains("response.completed"));
    assert!(!std::str::from_utf8(&body)
        .unwrap()
        .contains("USAGE_LIMIT_EXCEEDED"));
    assert_eq!(first_requests.load(Ordering::SeqCst), 1);
    assert_eq!(fallback_requests.load(Ordering::SeqCst), 1);
}

type FixtureWebSocket = WebSocketStream<TcpStream>;

async fn raw_http_headers(stream: &mut TcpStream) -> io::Result<Vec<u8>> {
    let mut received = Vec::new();
    let mut chunk = [0; 2048];
    loop {
        let count = stream.read(&mut chunk).await?;
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "fixture handshake ended",
            ));
        }
        received.extend_from_slice(&chunk[..count]);
        if let Some(end) = received.windows(4).position(|part| part == b"\r\n\r\n") {
            received.truncate(end + 4);
            return Ok(received);
        }
    }
}

async fn upgrade_or_models(mut stream: TcpStream) -> io::Result<Option<FixtureWebSocket>> {
    let headers = raw_http_headers(&mut stream).await?;
    let request = String::from_utf8_lossy(&headers);
    let path = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("");
    if path.ends_with("/models") {
        let body = br#"{"object":"list","data":[{"id":"fixture-model","object":"model"}]}"#;
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(head.as_bytes()).await?;
        stream.write_all(body).await?;
        return Ok(None);
    }
    let key = request.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("sec-websocket-key")
            .then_some(value.trim())
    });
    let Some(key) = key else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "fixture handshake omitted key",
        ));
    };
    let mut digest_input = key.as_bytes().to_vec();
    digest_input.extend_from_slice(b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
    let accept = STANDARD.encode(Sha1::digest(&digest_input));
    let response = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
    );
    stream.write_all(response.as_bytes()).await?;
    Ok(Some(
        WebSocketStream::from_raw_socket(stream, Role::Server, None).await,
    ))
}

async fn wait_for_create(ws: &mut FixtureWebSocket, creates: &AtomicUsize) -> bool {
    loop {
        let message = match tokio::time::timeout(WAIT, ws.next()).await {
            Ok(Some(Ok(message))) => message,
            _ => return false,
        };
        if let WsMessage::Text(text) = message {
            let raw = text.to_string();
            if serde_json::from_str::<Value>(&raw)
                .ok()
                .is_some_and(|value| value["type"] == "response.create")
            {
                creates.fetch_add(1, Ordering::SeqCst);
                return true;
            }
        }
    }
}

async fn wait_for_peer_close(ws: &mut FixtureWebSocket) {
    let _ = tokio::time::timeout(Duration::from_secs(8), async {
        while let Some(message) = ws.next().await {
            match message {
                Ok(WsMessage::Close(_)) | Err(_) => break,
                _ => {}
            }
        }
    })
    .await;
}

#[derive(Clone, Copy)]
enum WsFirstAttempt {
    PartialTimeout,
    PartialReset,
    PingReset,
}

async fn partial_ws_server(first_attempt: WsFirstAttempt, creates: Arc<AtomicUsize>) -> TestServer {
    let handshakes = Arc::new(AtomicUsize::new(0));
    tcp_server(move |stream| {
        let handshakes = handshakes.clone();
        let creates = creates.clone();
        async move {
            let Ok(Some(mut ws)) = upgrade_or_models(stream).await else {
                return;
            };
            let attempt = handshakes.fetch_add(1, Ordering::SeqCst) + 1;
            if !wait_for_create(&mut ws, &creates).await {
                return;
            }
            if attempt == 1 {
                match first_attempt {
                    WsFirstAttempt::PartialTimeout => {
                        let partial = b"{\"type\":\"response.created\",\"response\":{\"id\":\"fixture-partial";
                        ws.get_mut()
                            .write_all(&[0x81, 126, 0, 200])
                            .await
                            .unwrap();
                        ws.get_mut().write_all(partial).await.unwrap();
                        let _ = ws.get_mut().flush().await;
                        wait_for_peer_close(&mut ws).await;
                    }
                    WsFirstAttempt::PartialReset => {
                        let partial = b"{\"type\":\"response.created\",\"response\":{\"id\":\"fixture-partial";
                        ws.get_mut()
                            .write_all(&[0x81, 126, 0, 200])
                            .await
                            .unwrap();
                        ws.get_mut().write_all(partial).await.unwrap();
                        let _ = ws.get_mut().flush().await;
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        reset_tcp(ws.get_mut());
                    }
                    WsFirstAttempt::PingReset => {
                        ws.get_mut()
                            .write_all(&[0x89, 4, b'p', b'i', b'n', b'g'])
                            .await
                            .unwrap();
                        let _ = ws.get_mut().flush().await;
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        reset_tcp(ws.get_mut());
                    }
                }
            } else {
                let _ = ws
                    .send(WsMessage::Text(WS_COMPLETED.to_owned().into()))
                    .await;
                wait_for_peer_close(&mut ws).await;
            }
        }
    })
    .await
}

async fn next_client_frame(ws: &mut yawc::TcpWebSocket) -> yawc::Frame {
    loop {
        let frame = tokio::time::timeout(WAIT, ws.next())
            .await
            .expect("fixture downstream WebSocket timed out")
            .expect("fixture downstream WebSocket closed without a frame");
        if frame.opcode().is_control() && frame.opcode() != yawc::OpCode::Close {
            continue;
        }
        return frame;
    }
}

async fn partial_ws_case(first_attempt: WsFirstAttempt) {
    let creates = Arc::new(AtomicUsize::new(0));
    let upstream = partial_ws_server(first_attempt, creates.clone()).await;
    let fixture = Fixture::new(vec![format!("http://127.0.0.1:{}/v1", upstream.port)], true).await;
    fixture.start().await;
    let mut client = fixture.ws_client().await;
    client
        .send(yawc::Frame::text(WS_CREATE.to_owned()))
        .await
        .unwrap();

    let frame = next_client_frame(&mut client).await;
    client
        .send(yawc::Frame::close(1000.into(), "fixture complete"))
        .await
        .ok();
    fixture.stop().await;

    assert_eq!(frame.opcode(), yawc::OpCode::Close);
    assert_eq!(
        creates.load(Ordering::SeqCst),
        1,
        "response.create must not be replayed after partial data"
    );
}

#[tokio::test]
async fn websocket_partial_data_frame_timeout_does_not_replay_response_create() {
    partial_ws_case(WsFirstAttempt::PartialTimeout).await;
}

#[tokio::test]
async fn websocket_partial_data_frame_reset_does_not_replay_response_create() {
    partial_ws_case(WsFirstAttempt::PartialReset).await;
}

#[tokio::test]
async fn websocket_control_ping_does_not_block_safe_response_create_retry() {
    let creates = Arc::new(AtomicUsize::new(0));
    let upstream = partial_ws_server(WsFirstAttempt::PingReset, creates.clone()).await;
    let fixture = Fixture::new(vec![format!("http://127.0.0.1:{}/v1", upstream.port)], true).await;
    fixture.start().await;
    let mut client = fixture.ws_client().await;
    client
        .send(yawc::Frame::text(WS_CREATE.to_owned()))
        .await
        .unwrap();

    let frame = next_client_frame(&mut client).await;
    let is_completed =
        frame.opcode() == yawc::OpCode::Text && frame.as_str().contains("response.completed");
    client
        .send(yawc::Frame::close(1000.into(), "fixture complete"))
        .await
        .ok();
    fixture.stop().await;

    assert!(
        is_completed,
        "the control-frame-only disconnect must allow a safe retry"
    );
    assert_eq!(creates.load(Ordering::SeqCst), 2);
}

#[derive(Clone, Copy)]
enum ReusedTurnFault {
    FirstByteTimeout,
    ResetBeforeFirstByte,
}

async fn reused_turn_ws_server(
    creates: Arc<AtomicUsize>,
    second_turn_fault: ReusedTurnFault,
) -> TestServer {
    let handshakes = Arc::new(AtomicUsize::new(0));
    tcp_server(move |stream| {
        let handshakes = handshakes.clone();
        let creates = creates.clone();
        async move {
            let Ok(Some(mut ws)) = upgrade_or_models(stream).await else {
                return;
            };
            let connection = handshakes.fetch_add(1, Ordering::SeqCst) + 1;
            if !wait_for_create(&mut ws, &creates).await {
                return;
            }
            if connection == 1 {
                let _ = ws
                    .send(WsMessage::Text(WS_COMPLETED.to_owned().into()))
                    .await;
                if wait_for_create(&mut ws, &creates).await {
                    match second_turn_fault {
                        ReusedTurnFault::FirstByteTimeout => wait_for_peer_close(&mut ws).await,
                        ReusedTurnFault::ResetBeforeFirstByte => {
                            tokio::time::sleep(Duration::from_millis(100)).await;
                            reset_tcp(ws.get_mut());
                        }
                    }
                }
            } else {
                let _ = ws
                    .send(WsMessage::Text(WS_COMPLETED.to_owned().into()))
                    .await;
                wait_for_peer_close(&mut ws).await;
            }
        }
    })
    .await
}

#[tokio::test]
async fn websocket_reused_session_does_not_inherit_previous_turn_progress() {
    for second_turn_fault in [
        ReusedTurnFault::FirstByteTimeout,
        ReusedTurnFault::ResetBeforeFirstByte,
    ] {
        let creates = Arc::new(AtomicUsize::new(0));
        let upstream = reused_turn_ws_server(creates.clone(), second_turn_fault).await;
        let fixture =
            Fixture::new(vec![format!("http://127.0.0.1:{}/v1", upstream.port)], true).await;
        fixture.start().await;
        let mut client = fixture.ws_client().await;
        client
            .send(yawc::Frame::text(WS_CREATE.to_owned()))
            .await
            .unwrap();
        let first = next_client_frame(&mut client).await;
        assert!(first.as_str().contains("response.completed"));

        client
            .send(yawc::Frame::text(WS_CREATE.to_owned()))
            .await
            .unwrap();
        let second = next_client_frame(&mut client).await;
        client
            .send(yawc::Frame::close(1000.into(), "fixture complete"))
            .await
            .ok();
        fixture.stop().await;

        match second_turn_fault {
            ReusedTurnFault::FirstByteTimeout => {
                assert_eq!(second.opcode(), yawc::OpCode::Close);
                assert_eq!(second.close_code().map(u16::from), Some(1013));
                assert_eq!(
                    second.payload().get(2..).unwrap_or_default(),
                    "FIRST_BYTE_TIMEOUT".as_bytes()
                );
                assert_eq!(creates.load(Ordering::SeqCst), 2);
            }
            ReusedTurnFault::ResetBeforeFirstByte => {
                assert!(
                    second.opcode() == yawc::OpCode::Text
                        && second.as_str().contains("response.completed"),
                    "a connection reset before second-turn data should allow a safe retry"
                );
                assert_eq!(creates.load(Ordering::SeqCst), 3);
            }
        }
    }
}
