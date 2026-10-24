// 覆盖 TCP/HTTP/SSE 的交付和终态证据。
//! v0.23 delivery regressions using synthetic protocol data and isolated TCP listeners.
#[path = "terminal_identity_tests.rs"]
mod v0231_terminal_tests;
use super::{
    connector::BoxError,
    protocol::{Protocol, Terminal},
    replay::{full, WireBody},
    Edit, Gateway, Settings,
};
use crate::usage::{
    model::{EndCause, Filter, Record},
    Service,
};
use bytes::Bytes;
use http_body_util::{BodyExt, StreamBody};
use hyper::{
    body::{Frame, Incoming},
    Request, Response,
};
use hyper_util::rt::TokioIo;
use std::{
    convert::Infallible,
    future::Future,
    io::Write,
    path::PathBuf,
    pin::Pin,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpSocket, TcpStream},
    sync::Notify,
    task::JoinHandle,
};

const WAIT: Duration = Duration::from_secs(5);
const HTTP_REQUEST: &[u8] =
    br#" {"model":"fixture-model","stream":false,"input":"fixture","unknown":9007199254740993} "#;
const SSE_REQUEST: &[u8] =
    br#" {"model":"fixture-model","stream":true,"input":"fixture","unknown":9007199254740993} "#;
const HTTP_RESPONSE: &[u8] = br#" {"id":"fixture-http-response","status":"completed","model":"fixture-model","usage":{"input_tokens":11,"output_tokens":2},"unknown":9007199254740993} "#;
const CREATED: &[u8] = b"data: {\"type\":\"response.created\",\"response\":{\"id\":\"fixture-open\",\"model\":\"fixture-model\"}}\n\n";
const COMPLETED: &[u8] = b"data: {\"type\":\"response.completed\",\"response\":{\"id\":\"fixture-complete\",\"model\":\"fixture-model\",\"usage\":{\"input_tokens\":11,\"output_tokens\":2}}}\n\n";
const CR_ONLY: &[u8] = b": fixture comment\r\revent: response.completed\rdata: {\"response\":{\"id\":\"fixture-cr\",\"model\":\"fixture-model\"}}\r\r: fixture tail\r\r";
const MIXED_ENDINGS: &[u8] = b": fixture comment\r\n\r\nevent: response.completed\r\ndata: {\"response\":{\ndata: \"id\":\"fixture-mixed\",\"model\":\"fixture-model\",\"unknown\":9007199254740993}}\r\n\n: fixture tail\n\n";

#[test]
fn cr_only_sse_terminal_is_observed_before_eof() {
    for width in [1, 2, 7, CR_ONLY.len()] {
        let mut protocol = Protocol::new(true);
        protocol.response(200, true, "identity");
        for bytes in CR_ONLY.chunks(width) {
            protocol.feed(bytes);
        }
        assert_eq!(
            protocol.terminal(),
            Some(Terminal::Success),
            "CR-only terminal must be recognized before EOF; chunk width {width}"
        );
        assert!(!protocol.observation.incomplete);
    }
}

#[test]
fn crlf_and_lf_sse_terminal_survives_every_chunk_boundary() {
    for split in 0..=MIXED_ENDINGS.len() {
        let mut protocol = Protocol::new(true);
        protocol.response(200, true, "identity");
        protocol.feed(&MIXED_ENDINGS[..split]);
        protocol.feed(&MIXED_ENDINGS[split..]);
        assert_eq!(
            protocol.terminal(),
            Some(Terminal::Success),
            "event field and multiline data must survive byte boundary {split}"
        );
        assert!(!protocol.observation.incomplete, "byte boundary {split}");
    }
}

#[derive(Clone, Default)]
struct Tasks(Arc<Mutex<(bool, Vec<JoinHandle<()>>)>>);

impl Tasks {
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

struct Server {
    port: u16,
    tasks: Tasks,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.tasks.close();
    }
}

async fn server<F, Fut>(handler: F) -> Server
where
    F: Fn(Request<Incoming>) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Response<WireBody>> + Send + 'static,
{
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
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
                    .await;
            });
        }
    });
    Server { port, tasks }
}

struct Fixture {
    gateway: Gateway,
    usage: Service,
    home: PathBuf,
    _dir: tempfile::TempDir,
}

impl Fixture {
    async fn new(upstream: &Server) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("fixture-home");
        std::fs::create_dir(&home).unwrap();
        let url = format!("http://127.0.0.1:{}", upstream.port);
        std::fs::write(
            home.join("config.toml"),
            format!(
                "model_provider='custom'\n[model_providers.custom]\nbase_url='{url}'\nexperimental_bearer_token='fixture-only-key'\nwire_api='responses'\n"
            ),
        )
        .unwrap();
        let gateway = Gateway::new(dir.path().join("gateway-data")).unwrap();
        let usage = Service::new(&dir.path().join("usage-data"));
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
                max_retries: 0,
                first_byte_seconds: 5,
                idle_seconds: 5,
                connect_seconds: 2,
                queue_seconds: 2,
                ..Settings::default()
            },
        });
        fixture.edit(Edit::SaveProvider {
            search_only: None,
            quota_access_token: None,
            id: None,
            base_url: url,
            token: "fixture-only-key".into(),
            name: Some("fixture-provider".into()),
        });
        drop(reservation);
        fixture
            .gateway
            .start(&fixture.gateway.view().revision, &fixture.home)
            .await
            .unwrap();
        fixture
    }

    fn edit(&self, edit: Edit) {
        self.gateway
            .edit(edit, &self.gateway.view().revision, &self.home)
            .unwrap();
    }

    fn token(&self) -> String {
        self.gateway
            .0
            .inner
            .lock()
            .unwrap()
            .store
            .local_token
            .clone()
    }

    async fn socket(&self) -> TcpStream {
        TcpStream::connect(("127.0.0.1", self.gateway.view().settings.port))
            .await
            .unwrap()
    }

    async fn stop(&self) {
        tokio::time::timeout(WAIT, self.gateway.stop())
            .await
            .unwrap()
            .unwrap();
    }
}

type Sender = hyper::client::conn::http1::SendRequest<WireBody>;

async fn connection(fixture: &Fixture) -> (Sender, JoinHandle<()>) {
    let (sender, connection) =
        hyper::client::conn::http1::handshake(TokioIo::new(fixture.socket().await))
            .await
            .unwrap();
    let task = tokio::spawn(async move {
        let _ = connection.await;
    });
    (sender, task)
}

async fn request(sender: &mut Sender, fixture: &Fixture, id: &str) -> Response<Incoming> {
    tokio::time::timeout(WAIT, sender.ready())
        .await
        .unwrap()
        .unwrap();
    let request = Request::builder()
        .method("POST")
        .uri("/v1/responses")
        .header("host", "fixture.invalid")
        .header("authorization", format!("Bearer {}", fixture.token()))
        .header("content-type", "application/json")
        .header("x-request-id", id)
        .body(full(SSE_REQUEST))
        .unwrap();
    let response = tokio::time::timeout(WAIT, sender.send_request(request))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(response.status(), 200);
    response
}

async fn body_bytes(response: Response<Incoming>) -> Bytes {
    tokio::time::timeout(WAIT, response.into_body().collect())
        .await
        .unwrap()
        .unwrap()
        .to_bytes()
}

async fn read_prefix(body: &mut Incoming, expected: &[u8]) {
    let bytes = tokio::time::timeout(WAIT, async {
        let mut bytes = Vec::new();
        while bytes.len() < expected.len() {
            let frame = body.frame().await.expect("fixture response ended").unwrap();
            if let Some(data) = frame.data_ref() {
                bytes.extend_from_slice(data);
            }
        }
        bytes
    })
    .await
    .expect("fixture response prefix was not received");
    assert_eq!(bytes, expected, "gateway changed the SSE prefix bytes");
}

fn records(service: &Service) -> Vec<Record> {
    service
        .query(|store| Ok(store.logs(&Filter::default())?.rows))
        .unwrap()
}

async fn recorded(service: &Service, count: usize) -> Vec<Record> {
    let mut changes = service.subscribe();
    tokio::time::timeout(WAIT, async {
        loop {
            let rows = records(service);
            if rows.len() >= count {
                assert_eq!(rows.len(), count);
                return rows;
            }
            changes.recv().await.expect("fixture usage writer stopped");
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "fixture usage record did not settle: expected {count}, got {}",
            records(service).len()
        )
    })
}

fn assert_success(record: &Record, expected_bytes: usize) {
    assert!(record.completed);
    assert_eq!(record.attempts.len(), 1);
    let attempt = record.final_attempt().unwrap();
    assert_eq!(attempt.outcome, "success");
    let end = attempt.end.as_ref().unwrap();
    assert_eq!(end.cause, EndCause::Completed);
    assert!(end.terminal_received);
    let transfer = attempt.transfer.as_ref().unwrap();
    assert!(transfer.terminal_handed_off);
    assert_eq!(transfer.upstream_bytes, expected_bytes as u64);
    assert_eq!(transfer.downstream_bytes, expected_bytes as u64);
    assert!(transfer.upstream_eof);
    assert_eq!(transfer.downstream_error, None);
}

async fn sse_wire_case(payload: &'static [u8]) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let observed = seen.clone();
    let upstream = server(move |request| {
        let observed = observed.clone();
        async move {
            let bytes = request.into_body().collect().await.unwrap().to_bytes();
            observed.lock().unwrap().push(bytes);
            let chunks = payload
                .chunks(1)
                .map(|bytes| Ok::<_, BoxError>(Frame::data(Bytes::copy_from_slice(bytes))))
                .collect::<Vec<_>>();
            Response::builder()
                .header("content-type", "text/event-stream; charset=utf-8")
                .body(StreamBody::new(futures_util::stream::iter(chunks)).boxed_unsync())
                .unwrap()
        }
    })
    .await;
    let fixture = Fixture::new(&upstream).await;
    let (mut sender, task) = connection(&fixture).await;
    let response = request(&mut sender, &fixture, "fixture-wire").await;
    assert_eq!(body_bytes(response).await.as_ref(), payload);
    assert_eq!(*seen.lock().unwrap(), vec![Bytes::from_static(SSE_REQUEST)]);
    let rows = recorded(&fixture.usage, 1).await;
    drop(sender);
    task.abort();
    let _ = task.await;
    fixture.stop().await;
    assert_success(&rows[0], payload.len());
}

#[tokio::test]
async fn cr_only_sse_preserves_wire_bytes_and_completed_usage() {
    sse_wire_case(CR_ONLY).await;
}

#[tokio::test]
async fn mixed_crlf_lf_sse_preserves_wire_bytes_and_completed_usage() {
    sse_wire_case(MIXED_ENDINGS).await;
}

fn decode_http_body(wire: &[u8]) -> Vec<u8> {
    let boundary = wire
        .windows(4)
        .position(|part| part == b"\r\n\r\n")
        .expect("gateway closed the connection without sending HTTP response headers");
    let headers = std::str::from_utf8(&wire[..boundary]).unwrap();
    assert!(
        headers.starts_with("HTTP/1.1 200"),
        "unexpected fixture status"
    );
    let mut body = &wire[boundary + 4..];
    if !headers
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked")
    {
        return body.to_vec();
    }
    let mut decoded = Vec::new();
    loop {
        let line = body.windows(2).position(|part| part == b"\r\n").unwrap();
        let length =
            usize::from_str_radix(std::str::from_utf8(&body[..line]).unwrap(), 16).unwrap();
        body = &body[line + 2..];
        if length == 0 {
            break;
        }
        assert!(body.len() >= length + 2, "truncated fixture HTTP chunk");
        decoded.extend_from_slice(&body[..length]);
        assert_eq!(&body[length..length + 2], b"\r\n");
        body = &body[length + 2..];
    }
    decoded
}

#[tokio::test]
async fn observed_tcp_peeks_without_consuming_and_returns_a_silent_peer_reset() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let mut client = TcpStream::connect(address).await.unwrap();
    let (server, _) = listener.accept().await.unwrap();
    let activity = Arc::new(super::wire_progress::Activity::default());
    let (mut observed, failed) = super::wire_progress::observed_tcp(server, activity);
    let monitor = tokio::spawn(failed);

    let request_bytes = b"fixture request bytes";
    client.write_all(request_bytes).await.unwrap();
    tokio::time::sleep(Duration::from_millis(60)).await;
    let mut received = vec![0; request_bytes.len()];
    tokio::time::timeout(WAIT, observed.read_exact(&mut received))
        .await
        .expect("socket observer consumed request bytes before the HTTP reader")
        .unwrap();
    assert_eq!(received, request_bytes);

    super::tests::set_reset_on_drop(&client);
    drop(client);
    let error = tokio::time::timeout(Duration::from_secs(2), monitor)
        .await
        .expect("RST did not wake the observer on an otherwise silent connection")
        .expect("socket observer task panicked");
    assert!(matches!(
        error.kind(),
        std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
    ));
}

#[tokio::test]
async fn observed_tcp_keeps_peer_write_half_close_neutral_and_records_eof_once() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let mut client = TcpStream::connect(address).await.unwrap();
    let (server, _) = listener.accept().await.unwrap();
    let activity = Arc::new(super::wire_progress::Activity::default());
    let (_observed, failed) = super::wire_progress::observed_tcp(server, activity.clone());
    let monitor = tokio::spawn(failed);

    client.shutdown().await.unwrap();
    tokio::time::timeout(WAIT, async {
        loop {
            if activity
                .snapshot(0)
                .events
                .iter()
                .any(|event| event.kind == "peer_read_eof")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("peer FIN was not recorded");
    tokio::time::sleep(Duration::from_millis(60)).await;
    let eof_events = activity
        .snapshot(0)
        .events
        .into_iter()
        .filter(|event| event.kind == "peer_read_eof")
        .count();
    assert_eq!(eof_events, 1);
    assert!(
        !monitor.is_finished(),
        "a legal FIN cancelled the connection"
    );
    monitor.abort();
    let _ = monitor.await;
}

#[tokio::test]
async fn http_write_half_close_still_delivers_the_complete_response() {
    let received = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let request_seen = Arc::new(Mutex::new(Vec::new()));
    let upstream = server({
        let received = received.clone();
        let release = release.clone();
        let request_seen = request_seen.clone();
        move |request| {
            let received = received.clone();
            let release = release.clone();
            let request_seen = request_seen.clone();
            async move {
                let bytes = request.into_body().collect().await.unwrap().to_bytes();
                request_seen.lock().unwrap().push(bytes);
                received.notify_one();
                release.notified().await;
                Response::builder()
                    .header("content-type", "application/json")
                    .body(full(HTTP_RESPONSE))
                    .unwrap()
            }
        }
    })
    .await;
    let fixture = Fixture::new(&upstream).await;
    let mut socket = fixture.socket().await;
    let headers = format!(
        "POST /v1/responses HTTP/1.1\r\nHost: fixture.invalid\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        fixture.token(),
        HTTP_REQUEST.len()
    );
    socket.write_all(headers.as_bytes()).await.unwrap();
    socket.write_all(HTTP_REQUEST).await.unwrap();
    tokio::time::timeout(WAIT, received.notified())
        .await
        .unwrap();
    // A completed request body followed by FIN closes only the client's write
    // half. The response remains deliberately pending while the FIN arrives.
    socket.shutdown().await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    release.notify_one();
    let mut wire = Vec::new();
    tokio::time::timeout(WAIT, socket.read_to_end(&mut wire))
        .await
        .expect("write-half-closed client did not receive response EOF")
        .unwrap();
    fixture.stop().await;
    assert_eq!(
        *request_seen.lock().unwrap(),
        vec![Bytes::from_static(HTTP_REQUEST)]
    );
    assert_eq!(decode_http_body(&wire), HTTP_RESPONSE);
    let rows = recorded(&fixture.usage, 1).await;
    assert_success(&rows[0], HTTP_RESPONSE.len());
}

struct Dropped(Arc<Notify>);

impl Drop for Dropped {
    fn drop(&mut self) {
        self.0.notify_one();
    }
}

fn open_stream(prefix: &'static [u8], release: Arc<Notify>, cancelled: Arc<Notify>) -> WireBody {
    let guard = Dropped(cancelled);
    let chunks = async_stream::try_stream! {
        let _guard = guard;
        yield Frame::data(Bytes::from_static(prefix));
        release.notified().await;
        // More writes expose a fully closed peer even when HTTP half-close is
        // supported. No terminal event is synthesized after cancellation.
        loop {
            yield Frame::data(Bytes::from_static(b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"fixture tail\"}\n\n"));
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
    };
    StreamBody::new(chunks)
        .map_err(|error: std::io::Error| -> BoxError { error.into() })
        .boxed_unsync()
}

async fn close_open_response(prefix: &'static [u8], terminal: bool) {
    let release = Arc::new(Notify::new());
    let cancelled = Arc::new(Notify::new());
    let upstream = server({
        let release = release.clone();
        let cancelled = cancelled.clone();
        move |request| {
            let release = release.clone();
            let cancelled = cancelled.clone();
            async move {
                request.into_body().collect().await.unwrap();
                Response::builder()
                    .header("content-type", "text/event-stream")
                    .body(open_stream(prefix, release, cancelled))
                    .unwrap()
            }
        }
    })
    .await;
    let fixture = Fixture::new(&upstream).await;
    let (mut sender, task) = connection(&fixture).await;
    let mut body = request(&mut sender, &fixture, "fixture-close")
        .await
        .into_body();
    read_prefix(&mut body, prefix).await;
    task.abort();
    let _ = task.await;
    drop(body);
    drop(sender);
    release.notify_one();
    tokio::time::timeout(WAIT, cancelled.notified())
        .await
        .expect("gateway did not cancel its upstream after the TCP client closed");
    let rows = recorded(&fixture.usage, 1).await;
    fixture.stop().await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].completed, terminal);
    let attempt = rows[0].final_attempt().unwrap();
    assert_eq!(
        attempt.outcome,
        if terminal { "success" } else { "unknown" }
    );
    let end = attempt.end.as_ref().unwrap();
    let transfer = attempt.transfer.as_ref().unwrap();
    assert_eq!(end.terminal_received, terminal);
    assert_eq!(
        end.cause,
        if terminal {
            EndCause::Completed
        } else {
            downstream_cause(transfer.downstream_error.as_deref().unwrap())
        }
    );
    assert_eq!(transfer.terminal_handed_off, terminal);
    assert!(!transfer.upstream_eof);
    assert!(transfer.downstream_error.is_some());
}

#[tokio::test]
async fn tcp_close_before_sse_terminal_records_real_client_disconnect() {
    close_open_response(CREATED, false).await;
}

#[tokio::test]
async fn tcp_close_after_sse_terminal_keeps_the_completed_request() {
    close_open_response(COMPLETED, true).await;
}

#[tokio::test]
async fn reused_connection_disconnect_does_not_rewrite_its_completed_request() {
    let count = Arc::new(AtomicUsize::new(0));
    let release = Arc::new(Notify::new());
    let cancelled = Arc::new(Notify::new());
    let upstream = server({
        let count = count.clone();
        let release = release.clone();
        let cancelled = cancelled.clone();
        move |request| {
            let count = count.clone();
            let release = release.clone();
            let cancelled = cancelled.clone();
            async move {
                request.into_body().collect().await.unwrap();
                let body = if count.fetch_add(1, Ordering::SeqCst) == 0 {
                    full(COMPLETED)
                } else {
                    open_stream(CREATED, release, cancelled)
                };
                Response::builder()
                    .header("content-type", "text/event-stream")
                    .body(body)
                    .unwrap()
            }
        }
    })
    .await;
    let fixture = Fixture::new(&upstream).await;
    // SendRequest owns one HTTP/1 connection, so the second request cannot
    // silently use a newly pooled TCP connection.
    let (mut sender, task) = connection(&fixture).await;
    let first = request(&mut sender, &fixture, "fixture-first-complete").await;
    assert_eq!(body_bytes(first).await.as_ref(), COMPLETED);
    let initial = recorded(&fixture.usage, 1).await;
    assert_success(&initial[0], COMPLETED.len());
    let saved = serde_json::to_value(&initial[0]).unwrap();
    let first_id = initial[0].id.clone();

    let mut second = request(&mut sender, &fixture, "fixture-second-cancel")
        .await
        .into_body();
    read_prefix(&mut second, CREATED).await;
    task.abort();
    let _ = task.await;
    drop(second);
    drop(sender);
    release.notify_one();
    tokio::time::timeout(WAIT, cancelled.notified())
        .await
        .unwrap();
    let final_rows = recorded(&fixture.usage, 2).await;
    fixture.stop().await;
    assert_eq!(count.load(Ordering::SeqCst), 2);
    let first = final_rows.iter().find(|row| row.id == first_id).unwrap();
    assert_eq!(serde_json::to_value(first).unwrap(), saved);
    assert_success(first, COMPLETED.len());
    let second = final_rows.iter().find(|row| row.id != first_id).unwrap();
    assert!(!second.completed);
    assert_eq!(second.final_attempt().unwrap().outcome, "unknown");
    assert_eq!(
        second.final_attempt().unwrap().end.as_ref().unwrap().cause,
        downstream_cause(
            second
                .final_attempt()
                .unwrap()
                .transfer
                .as_ref()
                .unwrap()
                .downstream_error
                .as_deref()
                .unwrap()
        )
    );
    let first_connection = first
        .final_attempt()
        .unwrap()
        .transfer
        .as_ref()
        .unwrap()
        .connection
        .as_ref()
        .unwrap();
    let second_connection = second
        .final_attempt()
        .unwrap()
        .transfer
        .as_ref()
        .unwrap()
        .connection
        .as_ref()
        .unwrap();
    assert_eq!(first_connection.id, second_connection.id);
    assert_eq!(first_connection.request_sequence, 1);
    assert_eq!(second_connection.request_sequence, 2);
    assert!(second_connection.read_bytes > first_connection.read_bytes);
    assert!(second_connection.socket_written_bytes > first_connection.socket_written_bytes);
    assert!(!first_connection
        .events
        .iter()
        .any(|event| event.kind == "response_body_cancelled"));
    assert!(second_connection
        .events
        .windows(2)
        .all(|events| events[0].sequence < events[1].sequence));
}

fn downstream_cause(error: &str) -> EndCause {
    match error {
        "RESPONSE_BODY_DROPPED" => EndCause::Unsettled,
        "DOWNSTREAM_WRITE_TIMEOUT" => EndCause::DownstreamWriteTimeout,
        "DOWNSTREAM_CONNECTION_ERROR"
        | "DOWNSTREAM_CANCELLED"
        | "DOWNSTREAM_WRITE_FAILED"
        | "DOWNSTREAM_PROTOCOL_ERROR" => EndCause::DownstreamTransportError,
        "DOWNSTREAM_CLOSED" | "DOWNSTREAM_PEER_RESET" => EndCause::ClientDisconnected,
        _ => panic!("unclassified fixture downstream error: {error}"),
    }
}

#[tokio::test]
async fn downstream_delivery_errors_persist_their_own_cause_without_overwriting_a_terminal() {
    for (code, error) in [
        (1, "RESPONSE_BODY_DROPPED"),
        (2, "DOWNSTREAM_CONNECTION_ERROR"),
        (3, "DOWNSTREAM_WRITE_TIMEOUT"),
        (4, "DOWNSTREAM_CANCELLED"),
        (5, "DOWNSTREAM_CLOSED"),
        (6, "DOWNSTREAM_PEER_RESET"),
        (7, "DOWNSTREAM_WRITE_FAILED"),
        (8, "DOWNSTREAM_PROTOCOL_ERROR"),
    ] {
        for terminal in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let usage = Service::new(dir.path());
            let trace = usage.begin("codex", Some("fixture-model"));
            let mut protocol = Protocol::new(true);
            protocol.attach_usage(trace.attempt("fixture-provider", true, "http"));
            protocol.response(200, true, "identity");
            let bytes = if terminal { COMPLETED } else { CREATED };
            protocol.feed(bytes);
            let delivery = protocol.delivery();
            delivery.handed(bytes.len(), terminal);
            delivery.failed(code);
            if code == 3 {
                protocol.timeout("downstream_write", 2);
            }
            protocol.finish(Some(200), "CLIENT_DISCONNECTED");
            assert!(
                !protocol.transport_failure(),
                "downstream loss is not upstream provider failure"
            );
            assert_eq!(
                protocol.terminal(),
                Some(if terminal {
                    Terminal::Success
                } else {
                    Terminal::Unknown
                })
            );
            drop(protocol);
            drop(trace);

            let rows = recorded(&usage, 1).await;
            assert_eq!(rows[0].completed, terminal);
            let attempt = rows[0].final_attempt().unwrap();
            assert_eq!(
                attempt.outcome,
                if terminal { "success" } else { "unknown" }
            );
            let end = attempt.end.as_ref().unwrap();
            assert_eq!(
                end.cause,
                if terminal {
                    EndCause::Completed
                } else {
                    downstream_cause(error)
                },
                "error {code}, terminal {terminal}"
            );
            assert_eq!(end.terminal_received, terminal);
            let transfer = attempt.transfer.as_ref().unwrap();
            assert_eq!(transfer.downstream_error.as_deref(), Some(error));
            assert_eq!(transfer.terminal_handed_off, terminal);
            assert!(!transfer.upstream_eof);
            if code == 3 {
                assert_eq!(transfer.timeout_phase.as_deref(), Some("downstream_write"));
                assert_eq!(transfer.timeout_seconds, Some(2));
            } else {
                assert_eq!(transfer.timeout_phase, None);
            }
        }
    }
}

fn fragmented(payload: Vec<u8>, width: usize) -> WireBody {
    let frames = payload
        .chunks(width)
        .map(|chunk| Ok::<_, BoxError>(Frame::data(Bytes::copy_from_slice(chunk))))
        .collect::<Vec<_>>();
    StreamBody::new(futures_util::stream::iter(frames)).boxed_unsync()
}

fn compressed(encoding: &str, payload: &[u8]) -> Vec<u8> {
    match encoding {
        "gzip" => {
            let mut encoder =
                flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
            encoder.write_all(payload).unwrap();
            encoder.finish().unwrap()
        }
        "deflate" => {
            let mut encoder =
                flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
            encoder.write_all(payload).unwrap();
            encoder.finish().unwrap()
        }
        "br" => {
            let mut encoded = Vec::new();
            {
                let mut encoder = brotli::CompressorWriter::new(&mut encoded, 4096, 3, 20);
                encoder.write_all(payload).unwrap();
            }
            encoded
        }
        "zstd" => zstd::encode_all(payload, 1).unwrap(),
        _ => panic!("unsupported fixture encoding"),
    }
}

#[tokio::test]
async fn compressed_sse_preserves_encoded_wire_bytes_and_terminal_usage_over_tcp() {
    let payload = [
        CREATED,
        COMPLETED,
        b": fixture compressed tail\r\n\r\ndata: [DONE]\r\n\r\n",
    ]
    .concat();
    for encoding in ["gzip", "deflate", "br", "zstd"] {
        let encoded = compressed(encoding, &payload);
        let upstream = server({
            let encoded = encoded.clone();
            move |request| {
                let encoded = encoded.clone();
                async move {
                    assert_eq!(
                        request.into_body().collect().await.unwrap().to_bytes(),
                        SSE_REQUEST
                    );
                    Response::builder()
                        .header("content-type", "text/event-stream")
                        .header("content-encoding", encoding)
                        .body(fragmented(encoded, 7))
                        .unwrap()
                }
            }
        })
        .await;
        let fixture = Fixture::new(&upstream).await;
        let (mut sender, task) = connection(&fixture).await;
        let response = request(&mut sender, &fixture, "fixture-compressed").await;
        assert_eq!(response.headers()["content-encoding"], encoding);
        assert_eq!(body_bytes(response).await.as_ref(), encoded.as_slice());
        let rows = recorded(&fixture.usage, 1).await;
        drop(sender);
        task.abort();
        let _ = task.await;
        fixture.stop().await;
        assert_success(&rows[0], encoded.len());
        assert_eq!(rows[0].final_attempt().unwrap().tokens.input, Some(11));
        assert_eq!(rows[0].final_attempt().unwrap().tokens.output, Some(2));
    }
}

#[tokio::test]
async fn large_sse_event_keeps_every_byte_and_terminal_metadata() {
    let delta = "fixture-\"-\\-多字节-".repeat(100_000);
    let event = serde_json::json!({"type":"response.output_text.delta", "delta":delta});
    let mut payload = CREATED.to_vec();
    payload.extend_from_slice(format!("data: {event}\r\n\r\n").as_bytes());
    payload.extend_from_slice(COMPLETED);
    assert!(payload.len() > 2 * 1024 * 1024);
    let upstream = server({
        let payload = payload.clone();
        move |request| {
            let payload = payload.clone();
            async move {
                request.into_body().collect().await.unwrap();
                Response::builder()
                    .header("content-type", "text/event-stream")
                    .body(fragmented(payload, 16 * 1024 + 1))
                    .unwrap()
            }
        }
    })
    .await;
    let fixture = Fixture::new(&upstream).await;
    let (mut sender, task) = connection(&fixture).await;
    let response = request(&mut sender, &fixture, "fixture-large-event").await;
    let actual = body_bytes(response).await;
    assert_eq!(actual.len(), payload.len());
    assert_eq!(
        crate::storage::digest(&actual),
        crate::storage::digest(&payload)
    );
    let rows = recorded(&fixture.usage, 1).await;
    drop(sender);
    task.abort();
    let _ = task.await;
    fixture.stop().await;
    assert_success(&rows[0], payload.len());
    assert_eq!(rows[0].final_attempt().unwrap().tokens.output, Some(2));
}

#[tokio::test]
async fn terminal_event_keeps_late_usage_and_comment_tail_until_upstream_eof() {
    const TAIL: &[u8] = b": fixture delayed tail\r\n\r\ndata: {\"type\":\"response.usage\",\"usage\":{\"input_tokens\":11,\"output_tokens\":5}}\r\n\r\ndata: [DONE]\r\n\r\n";
    let release = Arc::new(Notify::new());
    let upstream = server({
        let release = release.clone();
        move |request| {
            let release = release.clone();
            async move {
                request.into_body().collect().await.unwrap();
                let body = async_stream::try_stream! {
                    yield Frame::data(Bytes::from_static(COMPLETED));
                    release.notified().await;
                    yield Frame::data(Bytes::from_static(TAIL));
                };
                Response::builder()
                    .header("content-type", "text/event-stream")
                    .body(
                        StreamBody::new(body)
                            .map_err(|error: std::io::Error| -> BoxError { error.into() })
                            .boxed_unsync(),
                    )
                    .unwrap()
            }
        }
    })
    .await;
    let fixture = Fixture::new(&upstream).await;
    let (mut sender, task) = connection(&fixture).await;
    let mut body = request(&mut sender, &fixture, "fixture-delayed-tail")
        .await
        .into_body();
    read_prefix(&mut body, COMPLETED).await;
    assert!(
        records(&fixture.usage).is_empty(),
        "final usage cannot settle before the pending tail"
    );
    release.notify_one();
    let tail = tokio::time::timeout(WAIT, body.collect())
        .await
        .unwrap()
        .unwrap()
        .to_bytes();
    assert_eq!(tail.as_ref(), TAIL);
    let rows = recorded(&fixture.usage, 1).await;
    drop(sender);
    task.abort();
    let _ = task.await;
    fixture.stop().await;
    assert_success(&rows[0], COMPLETED.len() + TAIL.len());
    assert_eq!(rows[0].final_attempt().unwrap().tokens.output, Some(5));
}

#[tokio::test]
async fn slow_tcp_reader_backpressures_upstream_and_preserves_the_entire_stream() {
    let delta = format!(
        "data: {{\"type\":\"response.output_text.delta\",\"delta\":\"{}\"}}\n\n",
        "x".repeat(64 * 1024)
    );
    let chunk = Bytes::from(delta);
    const CHUNKS: usize = 160;
    let produced = Arc::new(AtomicUsize::new(0));
    let upstream = server({
        let produced = produced.clone();
        let chunk = chunk.clone();
        move |request| {
            let produced = produced.clone();
            let chunk = chunk.clone();
            async move {
                request.into_body().collect().await.unwrap();
                let body = async_stream::try_stream! {
                    yield Frame::data(Bytes::from_static(CREATED));
                    for _ in 0..CHUNKS {
                        produced.fetch_add(1, Ordering::SeqCst);
                        yield Frame::data(chunk.clone());
                    }
                    yield Frame::data(Bytes::from_static(COMPLETED));
                };
                Response::builder()
                    .header("content-type", "text/event-stream")
                    .body(
                        StreamBody::new(body)
                            .map_err(|error: std::io::Error| -> BoxError { error.into() })
                            .boxed_unsync(),
                    )
                    .unwrap()
            }
        }
    })
    .await;
    let fixture = Fixture::new(&upstream).await;
    let socket = TcpSocket::new_v4().unwrap();
    socket.set_recv_buffer_size(16 * 1024).unwrap();
    let address = ([127, 0, 0, 1], fixture.gateway.view().settings.port).into();
    let mut socket = socket.connect(address).await.unwrap();
    let headers = format!(
        "POST /v1/responses HTTP/1.1\r\nHost: fixture.invalid\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        fixture.token(), SSE_REQUEST.len()
    );
    socket.write_all(headers.as_bytes()).await.unwrap();
    socket.write_all(SSE_REQUEST).await.unwrap();
    let mut wire = Vec::new();
    let mut small = [0_u8; 512];
    tokio::time::timeout(WAIT, async {
        while !wire.windows(4).any(|part| part == b"\r\n\r\n") {
            let read = socket.read(&mut small).await.unwrap();
            assert!(read > 0, "fixture response ended before its headers");
            wire.extend_from_slice(&small[..read]);
        }
    })
    .await
    .unwrap();
    // A raw TCP socket has no background Hyper reader. The fixed receive window
    // fills while this consumer is paused, so unlimited upstream prefetch is observable.
    tokio::time::sleep(Duration::from_millis(150)).await;
    let paused = produced.load(Ordering::SeqCst);
    assert!(
        paused > 0 && paused < CHUNKS,
        "upstream produced {paused} chunks while the client was paused"
    );
    assert!(records(&fixture.usage).is_empty());
    tokio::time::timeout(WAIT, async {
        let mut buffer = vec![0_u8; 128 * 1024];
        loop {
            let read = socket.read(&mut buffer).await.unwrap();
            if read == 0 {
                break;
            }
            wire.extend_from_slice(&buffer[..read]);
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("slow reader did not receive the complete stream");
    let body = decode_http_body(&wire);
    let expected_bytes = CREATED.len() + CHUNKS * chunk.len() + COMPLETED.len();
    assert_eq!(body.len(), expected_bytes);
    assert!(body.starts_with(CREATED));
    assert!(body.ends_with(COMPLETED));
    assert!(body[CREATED.len()..body.len() - COMPLETED.len()]
        .chunks(chunk.len())
        .all(|part| part == chunk.as_ref()));
    assert_eq!(produced.load(Ordering::SeqCst), CHUNKS);
    let rows = recorded(&fixture.usage, 1).await;
    fixture.stop().await;
    assert_success(&rows[0], expected_bytes);
}

#[tokio::test]
async fn content_length_final_frame_release_race_keeps_each_reused_request_successful() {
    fn response(index: usize) -> Vec<u8> {
        std::str::from_utf8(COMPLETED)
            .unwrap()
            .replace("fixture-complete", &format!("fixture-release-{index:02}"))
            .into_bytes()
    }
    let count = Arc::new(AtomicUsize::new(0));
    let upstream = server(move |request| {
        let count = count.clone();
        async move {
            request.into_body().collect().await.unwrap();
            Response::builder()
                .header("content-type", "text/event-stream")
                .body(full(response(count.fetch_add(1, Ordering::SeqCst))))
                .unwrap()
        }
    })
    .await;
    let fixture = Fixture::new(&upstream).await;
    let (mut sender, task) = connection(&fixture).await;
    const REQUESTS: usize = 12;
    for index in 0..REQUESTS {
        let actual = request(&mut sender, &fixture, &format!("fixture-release-{index}")).await;
        assert_eq!(body_bytes(actual).await.as_ref(), response(index));
        // Immediately reuse the connection; finalization races with the next request.
    }
    drop(sender);
    task.abort();
    let _ = task.await;
    let rows = recorded(&fixture.usage, REQUESTS).await;
    fixture.stop().await;
    let mut connection_id = None;
    let mut sequences = Vec::new();
    for row in &rows {
        assert_success(row, response(0).len());
        let connection = row
            .final_attempt()
            .unwrap()
            .transfer
            .as_ref()
            .unwrap()
            .connection
            .as_ref()
            .unwrap();
        if let Some(id) = connection_id.as_ref() {
            assert_eq!(&connection.id, id);
        } else {
            connection_id = Some(connection.id.clone());
        }
        sequences.push(connection.request_sequence);
        assert!(connection
            .events
            .windows(2)
            .all(|events| events[0].sequence < events[1].sequence));
    }
    sequences.sort_unstable();
    assert_eq!(sequences, (1..=REQUESTS as u64).collect::<Vec<_>>());
}

#[derive(Clone, Copy)]
enum FailureStage {
    Write,
    Flush,
}

struct FailedSocket(FailureStage);

impl AsyncWrite for FailedSocket {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Poll::Ready(match self.0 {
            FailureStage::Write => Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe)),
            FailureStage::Flush => Ok(bytes.len()),
        })
    }
    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe)))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

#[tokio::test]
async fn completed_final_frame_still_records_a_later_socket_write_or_flush_failure() {
    use super::{
        protocol::Delivery,
        wire_progress::{Activity, Tracked},
    };
    for stage in [FailureStage::Write, FailureStage::Flush] {
        let activity = Arc::new(Activity::default());
        let previous = Arc::new(Delivery::default());
        previous.track(activity.clone());
        previous.handed(COMPLETED.len(), true);
        previous.body_complete();
        let mut healthy = Tracked::new(tokio::io::sink(), activity.clone());
        healthy.write_all(COMPLETED).await.unwrap();
        healthy.flush().await.unwrap();
        assert!(previous.is_settled());
        assert_eq!(previous.error.load(Ordering::Acquire), 0);
        let previous_bytes = activity.snapshot(1).socket_written_bytes;

        let delivery = Arc::new(Delivery::default());
        delivery.track(activity.clone());
        delivery.handed(COMPLETED.len(), true);
        delivery.body_complete();
        assert!(
            !delivery.is_settled(),
            "HTTP body completion precedes socket outcome"
        );
        let mut failed = Tracked::new(FailedSocket(stage), activity.clone());
        let error = match stage {
            FailureStage::Write => failed.write_all(COMPLETED).await.unwrap_err(),
            FailureStage::Flush => {
                failed.write_all(COMPLETED).await.unwrap();
                failed.flush().await.unwrap_err()
            }
        };
        assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
        tokio::time::timeout(WAIT, delivery.wait()).await.unwrap();
        assert!(delivery.finished.load(Ordering::Acquire));
        assert_eq!(delivery.error.load(Ordering::Acquire), 7);
        assert_eq!(
            previous.error.load(Ordering::Acquire),
            0,
            "later socket failure must not rewrite a settled request"
        );
        let connection = activity.snapshot(2);
        let accepted = if matches!(stage, FailureStage::Flush) {
            COMPLETED.len() as u64
        } else {
            0
        };
        assert_eq!(connection.socket_written_bytes, previous_bytes + accepted);
        assert!(connection
            .events
            .iter()
            .any(|event| event.kind == "socket_write_failed"));
        assert!(connection
            .events
            .windows(2)
            .all(|events| events[0].sequence < events[1].sequence));
    }
}

#[tokio::test]
async fn empty_content_length_response_has_no_body_drop_error_and_reuses_the_connection() {
    let count = Arc::new(AtomicUsize::new(0));
    let upstream = server({
        let count = count.clone();
        move |request| {
            let count = count.clone();
            async move {
                request.into_body().collect().await.unwrap();
                let empty = count.fetch_add(1, Ordering::SeqCst) == 0;
                Response::builder()
                    .header(
                        "content-type",
                        if empty {
                            "application/json"
                        } else {
                            "text/event-stream"
                        },
                    )
                    .body(full(if empty { &[][..] } else { COMPLETED }))
                    .unwrap()
            }
        }
    })
    .await;
    let fixture = Fixture::new(&upstream).await;
    let (mut sender, task) = connection(&fixture).await;
    let first = request(&mut sender, &fixture, "fixture-empty-response").await;
    assert_eq!(first.headers()["content-length"], "0");
    assert!(body_bytes(first).await.is_empty());
    let rows = recorded(&fixture.usage, 1).await;
    let first_id = rows[0].id.clone();
    let transfer = rows[0].final_attempt().unwrap().transfer.as_ref().unwrap();
    assert_eq!(transfer.upstream_bytes, 0);
    assert_eq!(transfer.downstream_bytes, 0);
    assert!(transfer.upstream_eof);
    assert_eq!(transfer.downstream_error, None);
    let saved = serde_json::to_value(&rows[0]).unwrap();

    let second = request(&mut sender, &fixture, "fixture-after-empty").await;
    assert_eq!(body_bytes(second).await.as_ref(), COMPLETED);
    drop(sender);
    task.abort();
    let _ = task.await;
    let rows = recorded(&fixture.usage, 2).await;
    fixture.stop().await;
    assert_eq!(
        serde_json::to_value(rows.iter().find(|row| row.id == first_id).unwrap()).unwrap(),
        saved
    );
    assert_success(
        rows.iter().find(|row| row.id != first_id).unwrap(),
        COMPLETED.len(),
    );
}
