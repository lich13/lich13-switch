//! Responses uses per-generation slots, following Sub2API's BeforeTurn/AfterTurn
//! behavior (a3eb7ef3). Native payloads stay unchanged; HTTP bridges adapt only
//! the response.create envelope and never replay after downstream output.
use super::{
    admission::{Admission, Budget, CapacitySource, Rejected},
    circuit, forward,
    model::Settings,
    replay,
    routing::Requirement,
    Active, Gateway, Route,
};
use async_compression::tokio::bufread::{BrotliDecoder, GzipDecoder, ZlibDecoder, ZstdDecoder};
use base64::{engine::general_purpose::STANDARD, Engine};
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use http_body_util::BodyExt;
use hyper::{body::Incoming, header, HeaderMap, Request, Response, StatusCode, Uri};
use hyper_util::rt::TokioIo;
use sha1::{Digest, Sha1};
use std::{collections::HashMap, future::Future, io, time::Duration};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncWrite, BufReader},
    sync::{mpsc, oneshot, watch},
};
use tokio_util::io::StreamReader;
use yawc::{Frame, OpCode, Options, WebSocket};
type Failure = (u16, &'static str);
type BoxReader = Box<dyn AsyncBufRead + Send + Unpin>;
const MAX_MESSAGE: usize = 64 * 1024 * 1024;
const MAX_SSE_EVENT: usize = MAX_MESSAGE;
const KEEPALIVE: Duration = Duration::from_secs(20);
// Internal control result, consumed before writing a close frame.
const TURN_CANCELLED: Failure = (0, "turn cancelled before output");
fn options() -> Options {
    Options::default()
        .with_balanced_compression()
        .with_limits(MAX_MESSAGE, MAX_MESSAGE)
        .with_fragment_timeout(Duration::from_secs(120))
        .with_utf8()
}
struct Outgoing {
    frame: Frame,
    ack: oneshot::Sender<bool>,
}
struct Peer {
    incoming: mpsc::Receiver<Frame>,
    outgoing: mpsc::Sender<Outgoing>,
    closed: watch::Receiver<bool>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}
impl Peer {
    fn new<S: AsyncRead + AsyncWrite + Unpin + Send + 'static>(
        socket: WebSocket<S>,
        keepalive: Option<Duration>,
    ) -> Self {
        let (mut sink, mut stream) = socket.split();
        let (tx, incoming) = mpsc::channel(2);
        let (outgoing, mut rx) = mpsc::channel::<Outgoing>(1);
        let (closed_tx, closed) = watch::channel(false);
        let writer_closed = closed_tx.clone();
        let ping = Bytes::from(format!("lich13-switch:{}", uuid::Uuid::new_v4()));
        let pong = ping.clone();
        let reader = tokio::spawn(async move {
            while let Some(frame) = stream.next().await {
                // Consume only replies to our own heartbeat; arbitrary control
                // frames still pass through without touching the turn deadline.
                if frame.opcode() == OpCode::Pong && frame.payload() == &pong {
                    continue;
                }
                let close = frame.opcode() == OpCode::Close;
                if tx.send(frame).await.is_err() || close {
                    break;
                }
            }
            let _ = closed_tx.send(true);
        });
        let writer = tokio::spawn(async move {
            loop {
                let heartbeat = tokio::time::sleep(keepalive.unwrap_or(KEEPALIVE));
                let outgoing = tokio::select! {
                    outgoing = rx.recv() => outgoing,
                    _ = heartbeat, if keepalive.is_some() => {
                        if !matches!(tokio::time::timeout(Duration::from_secs(5), sink.send(Frame::ping(ping.clone()))).await, Ok(Ok(()))) {
                            break;
                        }
                        continue;
                    }
                };
                let Some(Outgoing { frame, ack }) = outgoing else {
                    break;
                };
                let ok = matches!(
                    tokio::time::timeout(Duration::from_secs(120), sink.send(frame)).await,
                    Ok(Ok(()))
                );
                let _ = ack.send(ok);
                if !ok {
                    break;
                }
            }
            let _ = writer_closed.send(true);
        });
        Self {
            incoming,
            outgoing,
            closed,
            tasks: vec![reader, writer],
        }
    }
    async fn send(&self, frame: Frame) -> Result<(), Failure> {
        let (ack, rx) = oneshot::channel();
        self.outgoing
            .send(Outgoing { frame, ack })
            .await
            .map_err(|_| (1011, "connection closed"))?;
        if rx.await.unwrap_or(false) {
            Ok(())
        } else {
            Err((1011, "connection write failed"))
        }
    }
    async fn close(&self, code: u16, reason: &str) {
        let _ = tokio::time::timeout(
            Duration::from_secs(2),
            self.send(Frame::close(code.into(), reason)),
        )
        .await;
    }
}
impl Drop for Peer {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}
enum Upstream {
    Native(Peer),
    Disconnected,
    Bridge(Option<BridgeTurn>),
}
struct BridgeTurn {
    first: Option<Result<Frame, Failure>>,
    incoming: mpsc::Receiver<Result<Frame, Failure>>,
    task: tokio::task::JoinHandle<()>,
}
impl BridgeTurn {
    fn cancel(&self) {
        self.task.abort();
    }
}
impl Drop for BridgeTurn {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn next_upstream(upstream: &mut Upstream) -> Option<Result<Frame, Failure>> {
    match upstream {
        Upstream::Native(peer) => {
            if *peer.closed.borrow() && peer.incoming.is_empty() {
                return None;
            }
            tokio::select! {
                biased;
                frame = peer.incoming.recv() => frame.map(Ok),
                _ = peer.closed.changed() => None,
            }
        }
        Upstream::Bridge(Some(turn)) => {
            if let Some(first) = turn.first.take() {
                Some(first)
            } else {
                turn.incoming.recv().await
            }
        }
        Upstream::Bridge(None) | Upstream::Disconnected => None,
    }
}
async fn send_to_upstream(upstream: &mut Upstream, frame: Frame) -> Result<(), Failure> {
    match upstream {
        Upstream::Native(peer) => peer.send(frame).await,
        Upstream::Bridge(Some(turn)) => {
            let Some(value) = value(&frame) else {
                return Err((1008, "bridge only accepts JSON response events"));
            };
            let kind = value.get("type").and_then(|v| v.as_str()).unwrap_or("");
            if kind == "response.cancel" {
                turn.cancel();
                return Ok(());
            }
            if kind == "response.create" {
                return Err((1013, "too many pending turns"));
            }
            Err((1008, "event is not supported by HTTP bridge"))
        }
        Upstream::Bridge(None) | Upstream::Disconnected => Err((1011, "bridge turn is not active")),
    }
}
fn unsupported_status(status: u16) -> bool {
    matches!(status, 400 | 404 | 405 | 406 | 426 | 501)
}
fn uses_native_websocket(client: super::ClientId, supports_websocket: bool) -> bool {
    client != super::ClientId::Codex || supports_websocket
}
#[derive(Clone, Debug)]
struct AttemptFailure {
    status: Option<u16>,
    retry: Option<Duration>,
    capacity: bool,
    unsupported: bool,
    model_unavailable: bool,
    context_incompatible: bool,
    model_payload: Option<Vec<u8>>,
    details: Box<crate::events::Details>,
}
impl AttemptFailure {
    fn hard_rejection(&self) -> bool {
        [
            &self.details.upstream_code,
            &self.details.upstream_type,
            &self.details.message,
        ]
        .into_iter()
        .flatten()
        .any(|value| super::upstream_error::permanent_rejection(value.as_bytes()))
    }
    fn transport_failure(&self) -> bool {
        self.status.is_none()
            || matches!(
                self.details.local_code.as_deref(),
                Some(
                    "CONNECT_TIMEOUT"
                        | "TLS_HANDSHAKE_FAILED"
                        | "CONNECTION_FAILED"
                        | "FIRST_BYTE_TIMEOUT"
                        | "STREAM_INTERRUPTED"
                        | "STREAM_TIMEOUT"
                        | "WS_UPGRADE_FAILED"
                )
            )
    }
    fn retryable(&self) -> bool {
        (self.hard_rejection() && self.status.is_some_and(|s| s >= 400))
            || self.transport_failure()
            || self.status.is_some_and(circuit::retryable)
    }
}
fn local_failure(code: &str) -> Box<crate::events::Details> {
    Box::new(crate::events::Details {
        local_code: Some(code.into()),
        ..Default::default()
    })
}
fn record_failure(
    permits: &mut forward::Permits,
    g: &Gateway,
    route: &Route,
    model: Option<&str>,
    failure: &AttemptFailure,
    action: crate::events::Action,
    attempt: usize,
) -> crate::events::Record {
    use crate::events::Reason;
    let reason = if failure.context_incompatible {
        Reason::ProtocolError
    } else if failure.model_unavailable {
        Reason::ModelUnavailable
    } else if failure.capacity {
        Reason::Capacity
    } else if failure.status == Some(429) {
        Reason::RateLimit
    } else if matches!(failure.status, Some(401 | 403)) {
        Reason::Authentication
    } else if failure.transport_failure() {
        Reason::Network
    } else if failure.status.is_some_and(|s| s < 400) || failure.unsupported {
        Reason::ProtocolError
    } else {
        Reason::UpstreamService
    };
    let mut details = (*failure.details).clone();
    details
        .phase
        .get_or_insert(crate::events::Phase::WsHandshake);
    details.counted_failure = Some(
        !failure.context_incompatible
            && !failure.hard_rejection()
            && !failure.model_unavailable
            && !failure.capacity
            && !failure.unsupported
            && failure.status != Some(429)
            && failure.retryable(),
    );
    permits.report(
        g,
        route,
        model,
        reason,
        action,
        failure.status,
        attempt,
        details,
    )
}
fn value(frame: &Frame) -> Option<serde_json::Value> {
    if matches!(frame.opcode(), OpCode::Text | OpCode::Binary) {
        serde_json::from_slice(frame.payload()).ok()
    } else {
        None
    }
}
fn creates(frame: &Frame) -> bool {
    value(frame).is_some_and(|v| v["type"] == "response.create")
}
fn rejected(reason: Rejected) -> Failure {
    match reason {
        Rejected::Model => (1008, "MODEL_NOT_ALLOWED or MODEL_UNDETERMINED"),
        Rejected::Stopped => (1012, "gateway stopped"),
        Rejected::Unavailable => (1013, "no available provider"),
        Rejected::Cooling(_) => (1013, "providers cooling down; retry later"),
        Rejected::Full | Rejected::Timeout => (1013, "provider concurrency full; retry later"),
        Rejected::RateLimited(_) => (1013, "provider RPM limit reached; retry later"),
        Rejected::RateLedger => (1011, "RPM state unavailable"),
    }
}
pub(super) fn accept(
    gateway: Gateway,
    mut request: Request<Incoming>,
    settings: Settings,
    manual: bool,
    ids: Vec<String>,
    routes: HashMap<String, Route>,
    active: Active,
) -> Response<replay::WireBody> {
    if request.method() != hyper::Method::GET {
        return forward::error(
            StatusCode::BAD_REQUEST,
            "WEBSOCKET",
            "WebSocket 握手必须使用 GET",
        );
    }
    let headers = request.headers().clone();
    let uri = request.uri().clone();
    let upgraded = WebSocket::upgrade_with_options(&mut request, options());
    let Ok((response, future)) = upgraded else {
        return forward::error(StatusCode::BAD_REQUEST, "WEBSOCKET", "WebSocket 握手无效");
    };
    let stop = gateway
        .0
        .inner
        .lock()
        .unwrap()
        .shutdown
        .as_ref()
        .map(|s| s.subscribe());
    tokio::spawn(async move {
        let _active = active;
        let Ok(socket) = future.await else {
            return;
        };
        let mut client = Peer::new(
            socket,
            (gateway.0.client == super::ClientId::Codex).then_some(KEEPALIVE),
        );
        let Some(mut stop) = stop else {
            return;
        };
        let result = tokio::select! {
            result = session(&gateway, &mut client, &settings, manual, ids, routes, headers, uri) => result,
            _ = stop.changed() => Err((1012, "gateway stopped")),
        };
        if let Err((code, reason)) = result {
            client.close(code, reason).await;
        }
    });
    response.map(|_| replay::empty())
}
#[allow(clippy::too_many_arguments)]
async fn take_slot(
    g: &Gateway,
    client: &mut Peer,
    routes: &[Route],
    manual: bool,
    settings: &Settings,
    budget: &mut Budget,
    requirement: &Requirement,
    immediate: bool,
) -> Result<Admission, Failure> {
    if *client.closed.borrow() {
        return Err((1000, "client closed"));
    }
    while_connecting(
        client,
        g.0.admission.acquire_for_immediate(
            routes,
            manual,
            settings.max_waiting,
            budget,
            requirement,
            immediate,
        ),
    )
    .await?
    .map_err(|reason| {
        if matches!(reason, Rejected::Model) {
            (1008, requirement.code())
        } else {
            rejected(reason)
        }
    })
}
async fn while_connecting<T>(
    client: &mut Peer,
    operation: impl Future<Output = T>,
) -> Result<T, Failure> {
    if *client.closed.borrow() {
        return Err((1000, "client closed"));
    }
    tokio::pin!(operation);
    loop {
        tokio::select! {
            biased;
            incoming = client.incoming.recv() => {
                let Some(frame) = incoming else { return Err((1000, "client closed")); };
                match frame.opcode() {
                    OpCode::Ping => client.send(Frame::pong(frame.payload().to_vec())).await?,
                    OpCode::Pong => (),
                    OpCode::Close => { client.send(frame).await?; return Err((1000, "client closed")); },
                    _ if value(&frame).is_some_and(|v| v["type"] == "response.cancel") => return Err(TURN_CANCELLED),
                    _ => return Err((1013, "too many pending turns")),
                }
            },
            result = &mut operation => return Ok(result),
            _ = client.closed.changed() => return Err((1000, "client closed")),
        }
    }
}
async fn cancellation(client: &Peer) -> Result<(), Failure> {
    client
        .send(Frame::text(r#"{"type":"response.cancelled"}"#))
        .await
}
async fn upstream_native(
    route: &Route,
    uri: &Uri,
    original: &HeaderMap,
    settings: &Settings,
) -> Result<Peer, AttemptFailure> {
    let mut request = Request::new(replay::empty());
    *request.method_mut() = hyper::Method::GET;
    *request.uri_mut() = forward::target_for(route.client_id, &route.provider.base_url, uri)
        .map_err(|_| AttemptFailure {
            status: None,
            retry: None,
            capacity: false,
            unsupported: false,
            model_unavailable: false,
            context_incompatible: false,
            model_payload: None,
            details: Default::default(),
        })?;
    *request.headers_mut() = original.clone();
    let headers = request.headers_mut();
    forward::clean_headers(headers, true);
    headers.remove(header::HOST);
    headers.remove(header::CONTENT_LENGTH);
    headers.remove("x-api-key");
    headers.insert(
        header::AUTHORIZATION,
        format!("Bearer {}", route.provider.token)
            .parse()
            .map_err(|_| AttemptFailure {
                status: None,
                retry: None,
                capacity: false,
                unsupported: false,
                model_unavailable: false,
                context_incompatible: false,
                model_payload: None,
                details: Default::default(),
            })?,
    );
    let key = STANDARD.encode(uuid::Uuid::new_v4().as_bytes());
    headers.insert(header::SEC_WEBSOCKET_KEY, key.parse().unwrap());
    headers.insert(
        header::SEC_WEBSOCKET_EXTENSIONS,
        "permessage-deflate; client_max_window_bits"
            .parse()
            .unwrap(),
    );
    let mut response = match tokio::time::timeout(
        Duration::from_secs(settings.first_byte_seconds),
        route.client.request(request),
    )
    .await
    {
        Ok(Ok(response)) => response,
        Ok(Err(error)) => {
            return Err(AttemptFailure {
                status: None,
                retry: None,
                capacity: false,
                unsupported: false,
                model_unavailable: false,
                context_incompatible: false,
                model_payload: None,
                details: local_failure(super::connector::diagnostic_code(&error)),
            })
        }
        Err(_) => {
            return Err(AttemptFailure {
                status: None,
                retry: None,
                capacity: false,
                unsupported: false,
                model_unavailable: false,
                context_incompatible: false,
                model_payload: None,
                details: local_failure("FIRST_BYTE_TIMEOUT"),
            })
        }
    };
    if response.status() != StatusCode::SWITCHING_PROTOCOLS {
        let status = response.status();
        let retry = response
            .headers()
            .get(header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(circuit::retry_after);
        let encoding = response
            .headers()
            .get(header::CONTENT_ENCODING)
            .and_then(|h| h.to_str().ok())
            .unwrap_or("identity")
            .to_owned();
        let prefix = tokio::time::timeout(
            Duration::from_secs(settings.first_byte_seconds),
            response_prefix(response.into_body(), 128 * 1024),
        )
        .await
        .unwrap_or_default();
        let decoded = replay::decode_prefix(&prefix, &encoding, 128 * 1024).unwrap_or_default();
        let model_unavailable = super::upstream_error::model_http(status.as_u16(), &decoded);
        let capacity = !model_unavailable
            && route.client_id == super::ClientId::Codex
            && forward::capacity_message(status, &decoded);
        return Err(AttemptFailure {
            status: Some(status.as_u16()),
            retry,
            capacity,
            details: Box::new(super::upstream_error::details_http(&decoded)),
            model_payload: model_unavailable.then(|| decoded.clone()),
            model_unavailable,
            context_incompatible: serde_json::from_slice(&decoded)
                .ok()
                .is_some_and(|v| super::compaction::incompatible(&v)),
            unsupported: !model_unavailable
                && !super::upstream_error::permanent_rejection(&decoded)
                && route.client_id == super::ClientId::Codex
                && unsupported_status(status.as_u16()),
        });
    }
    let expected = STANDARD.encode(Sha1::digest(
        format!("{key}258EAFA5-E914-47DA-95CA-C5AB0DC85B11").as_bytes(),
    ));
    if response
        .headers()
        .get(header::SEC_WEBSOCKET_ACCEPT)
        .and_then(|v| v.to_str().ok())
        != Some(&expected)
    {
        return Err(AttemptFailure {
            status: Some(response.status().as_u16()),
            retry: None,
            capacity: false,
            unsupported: true,
            model_unavailable: false,
            context_incompatible: false,
            model_payload: None,
            details: Default::default(),
        });
    }
    let extensions = response
        .headers()
        .get(header::SEC_WEBSOCKET_EXTENSIONS)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let io = hyper::upgrade::on(&mut response)
        .await
        .map_err(|_| AttemptFailure {
            status: Some(101),
            retry: None,
            capacity: false,
            unsupported: false,
            model_unavailable: false,
            context_incompatible: false,
            model_payload: None,
            details: local_failure("WS_UPGRADE_FAILED"),
        })?;
    let socket = WebSocket::from_stream_with_extensions(
        TokioIo::new(io),
        yawc::Role::Client,
        extensions.as_deref(),
        options(),
    )
    .map_err(|_| AttemptFailure {
        status: Some(101),
        retry: None,
        capacity: false,
        unsupported: true,
        model_unavailable: false,
        context_incompatible: false,
        model_payload: None,
        details: local_failure("WS_EXTENSION_UNSUPPORTED"),
    })?;
    Ok(Peer::new(
        socket,
        (route.client_id == super::ClientId::Codex).then_some(KEEPALIVE),
    ))
}
fn bridge_payload(frame: &Frame) -> Result<Bytes, Failure> {
    let mut value = value(frame).ok_or((1008, "invalid response.create"))?;
    let object = value
        .as_object_mut()
        .ok_or((1008, "response.create must be a JSON object"))?;
    if object.get("type").and_then(|v| v.as_str()) != Some("response.create") {
        return Err((1008, "expected response.create"));
    }
    object.remove("type");
    object.insert("stream".into(), serde_json::Value::Bool(true));
    serde_json::to_vec(&value)
        .map(Bytes::from)
        .map_err(|_| (1011, "cannot encode bridge request"))
}
async fn response_prefix(mut body: Incoming, limit: usize) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(limit.min(4096));
    while bytes.len() < limit {
        let Some(Ok(frame)) = body.frame().await else {
            break;
        };
        if let Ok(data) = frame.into_data() {
            let count = data.len().min(limit - bytes.len());
            bytes.extend_from_slice(&data[..count]);
        }
    }
    bytes
}
fn decoded_reader(body: Incoming, encoding: &str) -> Result<BoxReader, AttemptFailure> {
    let stream = body
        .into_data_stream()
        .map(|result| result.map_err(io::Error::other));
    decode_reader(
        Box::new(BufReader::new(StreamReader::new(stream))),
        encoding,
    )
}
fn decode_reader(reader: BoxReader, encoding: &str) -> Result<BoxReader, AttemptFailure> {
    let reader: BoxReader = match encoding.trim().to_ascii_lowercase().as_str() {
        "" | "identity" => Box::new(reader),
        "br" => Box::new(BufReader::new(BrotliDecoder::new(reader))),
        "gzip" => Box::new(BufReader::new(GzipDecoder::new(reader))),
        "deflate" => Box::new(BufReader::new(ZlibDecoder::new(reader))),
        "zstd" => Box::new(BufReader::new(ZstdDecoder::new(reader))),
        _ => {
            return Err(AttemptFailure {
                status: None,
                retry: None,
                capacity: false,
                unsupported: true,
                model_unavailable: false,
                context_incompatible: false,
                model_payload: None,
                details: Default::default(),
            })
        }
    };
    Ok(reader)
}
async fn send_bridge_event(
    data: &[u8],
    event: &str,
    sender: &mpsc::Sender<Result<Frame, Failure>>,
) -> Result<bool, Failure> {
    if data == b"[DONE]" {
        // A transport sentinel is not a Responses completion event.
        return Ok(true);
    }
    let mut value: serde_json::Value =
        serde_json::from_slice(data).map_err(|_| (1011, "bridge returned invalid SSE JSON"))?;
    let object = value
        .as_object_mut()
        .ok_or((1011, "bridge returned invalid SSE JSON"))?;
    let payload = if !object.contains_key("type") && !event.is_empty() {
        object.insert("type".into(), serde_json::Value::String(event.into()));
        serde_json::to_string(&value).map_err(|_| (1011, "bridge event encoding failed"))?
    } else {
        std::str::from_utf8(data)
            .map(str::to_owned)
            .map_err(|_| (1011, "bridge returned non-UTF8 SSE JSON"))?
    };
    sender
        .send(Ok(Frame::text(payload)))
        .await
        .map(|_| false)
        .map_err(|_| (1000, "bridge client closed"))
}
async fn bridge_events(mut reader: BoxReader, sender: mpsc::Sender<Result<Frame, Failure>>) {
    let mut line = Vec::with_capacity(256);
    let mut data = Vec::new();
    let mut event = String::new();
    loop {
        line.clear();
        let read = bounded_line(&mut reader, &mut line).await;
        let Ok(size) = read else {
            let _ = sender.send(Err(read.unwrap_err())).await;
            return;
        };
        if size == 0 {
            if !data.is_empty() {
                if let Err(error) = send_bridge_event(&data, &event, &sender).await {
                    let _ = sender.send(Err(error)).await;
                }
            }
            return;
        }
        if data.len() + line.len() > MAX_SSE_EVENT {
            let _ = sender.send(Err((1009, "bridge event too large"))).await;
            return;
        }
        let trimmed = line.strip_suffix(b"\n").unwrap_or(&line);
        let trimmed = trimmed.strip_suffix(b"\r").unwrap_or(trimmed);
        if trimmed.is_empty() {
            if !data.is_empty() {
                match send_bridge_event(&data, &event, &sender).await {
                    Ok(done) => {
                        data.clear();
                        event.clear();
                        if done {
                            return;
                        }
                    }
                    Err(error) => {
                        let _ = sender.send(Err(error)).await;
                        return;
                    }
                }
            }
            continue;
        }
        if let Some(value) = trimmed.strip_prefix(b"event:") {
            if value.len() <= 256 {
                event = String::from_utf8_lossy(value).trim().to_owned();
            }
        }
        if let Some(value) = trimmed.strip_prefix(b"data:") {
            let value = value.strip_prefix(b" ").unwrap_or(value);
            if !data.is_empty() {
                data.push(b'\n');
            }
            data.extend_from_slice(value);
        }
    }
}
async fn bounded_line(reader: &mut BoxReader, line: &mut Vec<u8>) -> Result<usize, Failure> {
    loop {
        let bytes = reader
            .fill_buf()
            .await
            .map_err(|_| (1013, "bridge response read failed"))?;
        if bytes.is_empty() {
            return Ok(line.len());
        }
        let count = bytes
            .iter()
            .position(|b| *b == b'\n')
            .map_or(bytes.len(), |n| n + 1);
        if line.len() + count > MAX_SSE_EVENT {
            return Err((1009, "bridge event too large"));
        }
        let end = bytes[count - 1] == b'\n';
        line.extend_from_slice(&bytes[..count]);
        reader.consume(count);
        if end {
            return Ok(line.len());
        }
    }
}

fn policy_rejection(frame: &Frame) -> bool {
    value(frame).is_some_and(|v| {
        v.pointer("/response/error/code")
            .or_else(|| v.pointer("/error/code"))
            .is_some_and(|code| code == "cyber_policy")
    })
}
fn first_event_failure(frame: &Frame) -> Option<AttemptFailure> {
    if policy_rejection(frame) {
        return None;
    }
    let v = value(frame)?;
    let mut observation = super::protocol::Observation::default();
    observation.value(&v);
    if !observation.compaction_incompatible
        && !matches!(
            observation.terminal,
            Some(super::protocol::Terminal::Failure | super::protocol::Terminal::ModelUnavailable)
        )
    {
        return None;
    }
    let error = v
        .pointer("/response/error")
        .or_else(|| v.get("error"))
        .unwrap_or(&v);
    let code = error
        .get("code")
        .or_else(|| error.get("type"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let status = v
        .get("status")
        .or_else(|| error.get("status"))
        .and_then(|v| v.as_u64())
        .and_then(|v| u16::try_from(v).ok())
        .unwrap_or(502);
    let capacity = !super::upstream_error::permanent_rejection(frame.payload())
        && (matches!(
            code,
            "rate_limit_exceeded"
                | "rate_limit_error"
                | "overloaded_error"
                | "model_capacity_exceeded"
                | "server_is_overloaded"
                | "slow_down"
                | "usage_limit_reached"
        ) || forward::capacity_message(
            StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
            frame.payload(),
        ));
    Some(AttemptFailure {
        status: Some(101),
        retry: None,
        capacity,
        unsupported: false,
        model_unavailable: super::upstream_error::model_error(&v),
        context_incompatible: super::compaction::incompatible(&v),
        model_payload: (super::upstream_error::model_error(&v)
            || super::compaction::incompatible(&v))
        .then(|| frame.payload().to_vec()),
        details: Box::new(super::upstream_error::details_value(&v, false)),
    })
}
async fn upstream_bridge(
    route: &Route,
    uri: &Uri,
    original: &HeaderMap,
    settings: &Settings,
    first: &Frame,
) -> Result<BridgeTurn, AttemptFailure> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(settings.first_byte_seconds);
    let payload = bridge_payload(first).map_err(|_| AttemptFailure {
        status: None,
        retry: None,
        capacity: false,
        unsupported: false,
        model_unavailable: false,
        context_incompatible: false,
        model_payload: None,
        details: Default::default(),
    })?;
    let mut request = Request::new(replay::full(payload.clone()));
    *request.method_mut() = hyper::Method::POST;
    *request.uri_mut() = forward::target_for(route.client_id, &route.provider.base_url, uri)
        .map_err(|_| AttemptFailure {
            status: None,
            retry: None,
            capacity: false,
            unsupported: false,
            model_unavailable: false,
            context_incompatible: false,
            model_payload: None,
            details: Default::default(),
        })?;
    *request.headers_mut() = original.clone();
    let headers = request.headers_mut();
    forward::clean_headers(headers, false);
    headers.remove(header::HOST);
    headers.remove(header::CONTENT_LENGTH);
    headers.remove(header::CONTENT_ENCODING);
    headers.remove(header::UPGRADE);
    headers.remove("x-api-key");
    for name in [
        "sec-websocket-key",
        "sec-websocket-version",
        "sec-websocket-extensions",
        "sec-websocket-protocol",
    ] {
        headers.remove(name);
    }
    headers.insert(
        header::AUTHORIZATION,
        format!("Bearer {}", route.provider.token)
            .parse()
            .map_err(|_| AttemptFailure {
                status: None,
                retry: None,
                capacity: false,
                unsupported: false,
                model_unavailable: false,
                context_incompatible: false,
                model_payload: None,
                details: Default::default(),
            })?,
    );
    headers.insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/json"),
    );
    headers.insert(
        header::ACCEPT,
        header::HeaderValue::from_static("text/event-stream"),
    );
    headers.insert(
        header::ACCEPT_ENCODING,
        header::HeaderValue::from_static("gzip, deflate, zstd, br"),
    );
    headers.insert(
        header::CONTENT_LENGTH,
        header::HeaderValue::from(payload.len() as u64),
    );
    let response = match tokio::time::timeout_at(deadline, route.client.request(request)).await {
        Ok(Ok(response)) => response,
        Ok(Err(error)) => {
            return Err(AttemptFailure {
                status: None,
                retry: None,
                capacity: false,
                unsupported: false,
                model_unavailable: false,
                context_incompatible: false,
                model_payload: None,
                details: local_failure(super::connector::diagnostic_code(&error)),
            });
        }
        Err(_) => {
            return Err(AttemptFailure {
                status: None,
                retry: None,
                capacity: false,
                unsupported: false,
                model_unavailable: false,
                context_incompatible: false,
                model_payload: None,
                details: local_failure("FIRST_BYTE_TIMEOUT"),
            })
        }
    };
    let status = response.status();
    let retry = response
        .headers()
        .get(header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(circuit::retry_after);
    if !status.is_success() {
        let encoding = response
            .headers()
            .get(header::CONTENT_ENCODING)
            .and_then(|h| h.to_str().ok())
            .unwrap_or("identity")
            .to_owned();
        let prefix =
            tokio::time::timeout_at(deadline, response_prefix(response.into_body(), 128 * 1024))
                .await
                .unwrap_or_default();
        let decoded = replay::decode_prefix(&prefix, &encoding, 128 * 1024).unwrap_or_default();
        return Err(AttemptFailure {
            status: Some(status.as_u16()),
            retry,
            capacity: route.client_id == super::ClientId::Codex
                && forward::capacity_message(status, &decoded),
            details: Box::new(super::upstream_error::details_http(&decoded)),
            model_payload: super::upstream_error::model_http(status.as_u16(), &decoded)
                .then(|| decoded.clone()),
            model_unavailable: super::upstream_error::model_http(status.as_u16(), &decoded),
            context_incompatible: serde_json::from_slice(&decoded)
                .ok()
                .is_some_and(|v| super::compaction::incompatible(&v)),
            unsupported: matches!(status.as_u16(), 404 | 405)
                && !super::upstream_error::permanent_rejection(&decoded)
                && !super::upstream_error::model_http(status.as_u16(), &decoded),
        });
    }
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    if !content_type
        .split(';')
        .next()
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("text/event-stream"))
    {
        return Err(AttemptFailure {
            status: Some(status.as_u16()),
            retry,
            capacity: false,
            unsupported: true,
            model_unavailable: false,
            context_incompatible: false,
            model_payload: None,
            details: Default::default(),
        });
    }
    let encoding = response
        .headers()
        .get(header::CONTENT_ENCODING)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("identity")
        .to_owned();
    let reader = decoded_reader(response.into_body(), &encoding)?;
    let (sender, incoming) = mpsc::channel(2);
    let task = tokio::spawn(bridge_events(reader, sender));
    // Own the worker before awaiting: cancelling this future also drops the
    // HTTP body, including while the worker is blocked by downstream pressure.
    let mut turn = BridgeTurn {
        first: None,
        incoming,
        task,
    };
    let first = match tokio::time::timeout_at(deadline, turn.incoming.recv()).await {
        Ok(Some(Ok(frame))) => frame,
        outcome => {
            let code = match outcome {
                Ok(Some(Err((1009, _)))) => "PROTOCOL_BUFFER_LIMIT",
                Err(_) => "FIRST_BYTE_TIMEOUT",
                _ => "STREAM_INTERRUPTED",
            };
            let mut details = local_failure(code);
            details.phase = Some(crate::events::Phase::Stream);
            return Err(AttemptFailure {
                status: Some(status.as_u16()),
                retry,
                capacity: false,
                unsupported: false,
                model_unavailable: false,
                context_incompatible: false,
                model_payload: None,
                details,
            });
        }
    };
    if let Some(mut failure) = first_event_failure(&first) {
        failure.retry = retry;
        failure.status = Some(status.as_u16());
        return Err(failure);
    }
    turn.first = Some(Ok(first));
    Ok(turn)
}
fn turn_model(
    g: &Gateway,
    frame: &Frame,
    last: Option<&str>,
    pinned: Option<&str>,
) -> Result<Option<String>, Failure> {
    let v = value(frame).ok_or((1008, "invalid response.create"))?;
    let previous = v.get("previous_response_id").and_then(|v| v.as_str());
    let remembered = previous.and_then(|id| {
        g.0.inner
            .lock()
            .unwrap()
            .affinity
            .get(id)
            .filter(|(_, _, at)| at.elapsed() < Duration::from_secs(3600))
            .cloned()
    });
    if let Some((owner, _, _)) = &remembered {
        if pinned.is_some_and(|p| p != owner) {
            return Err((1008, "response context belongs to another provider"));
        }
    }
    if let Some(model) = v.get("model") {
        return Ok(model
            .as_str()
            .filter(|m| !m.is_empty() && m.len() <= 256 && !m.chars().any(char::is_control))
            .map(str::to_owned));
    }
    Ok(remembered
        .and_then(|(_, model, _)| model)
        .or_else(|| last.map(str::to_owned)))
}
#[allow(clippy::too_many_arguments)]
async fn session(
    g: &Gateway,
    client: &mut Peer,
    cfg: &Settings,
    manual: bool,
    ids: Vec<String>,
    routes: HashMap<String, Route>,
    headers: HeaderMap,
    uri: Uri,
) -> Result<(), Failure> {
    let mut next_turn = None;
    let mut current_ids = ids;
    let mut current_routes = routes;
    let mut current_cfg = cfg.clone();
    loop {
        match session_once(
            g,
            client,
            &current_cfg,
            manual,
            current_ids.clone(),
            current_routes.clone(),
            headers.clone(),
            uri.clone(),
            &mut next_turn,
        )
        .await
        {
            Err((1999, "compaction handoff")) => {
                current_cfg = g.0.inner.lock().unwrap().store.settings.clone();
                current_ids = g.routing_ids(None);
                current_routes = current_ids
                    .iter()
                    .filter_map(|id| g.route(id).map(|route| (id.clone(), route)))
                    .collect();
            }
            Err(TURN_CANCELLED) => cancellation(client).await?,
            result => return result,
        }
    }
}
async fn drain_completed(
    upstream: &mut Upstream,
    protocol: &mut Option<super::protocol::Protocol>,
) {
    let Some(protocol) = protocol.as_mut().filter(|p| p.succeeded()) else {
        return;
    };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let mut bytes = 0usize;
    while bytes < 256 * 1024 {
        let Ok(Some(Ok(frame))) = tokio::time::timeout_at(deadline, next_upstream(upstream)).await
        else {
            break;
        };
        if frame.opcode() == OpCode::Close {
            break;
        }
        bytes = bytes.saturating_add(frame.payload().len());
        if let Some(value) = value(&frame) {
            protocol.value(&value);
        }
    }
}
// The terminal is settled once before reconnecting. A received application
// error remains neutral even when the peer closes immediately afterwards.
#[allow(clippy::too_many_arguments)]
fn disconnect_turn(
    g: &Gateway,
    route: &Route,
    cfg: &Settings,
    model: Option<&str>,
    turn: &mut Option<Admission>,
    protocol: &mut Option<super::protocol::Protocol>,
    received: bool,
    attempts: usize,
    ordinary_attempts: usize,
    can_replay: bool,
    close: Option<&Frame>,
    phase: crate::events::Phase,
    category: &str,
) -> bool {
    let Some(mut admission) = turn.take() else {
        return false;
    };
    let mut observed = protocol.take();
    if let Some(u) = &mut observed {
        u.finish(Some(101), category);
    }
    let terminal = observed.as_ref().and_then(|u| u.terminal());
    let transport = observed.as_ref().is_none_or(|u| u.transport_failure());
    let retry = route.client_id == super::ClientId::Codex
        && !received
        && can_replay
        && ordinary_attempts <= cfg.max_retries
        && transport;
    let mut details = observed
        .as_ref()
        .map(|u| u.observation.error.clone())
        .unwrap_or_default();
    details.phase = Some(phase);
    details.ws_close_code = close.and_then(close_code);
    if details.message.is_none() {
        details.message = close
            .and_then(|frame| std::str::from_utf8(frame.payload().get(2..)?).ok())
            .and_then(crate::events::safe_message);
    }
    details.counted_failure = Some(transport);
    details.wait_seconds = retry.then_some(cfg.websocket_retry_seconds);
    if transport && details.local_code.is_none() {
        details.local_code = Some(category.into());
    }
    let reason = if terminal == Some(super::protocol::Terminal::ModelUnavailable) {
        crate::events::Reason::ModelUnavailable
    } else if transport {
        crate::events::Reason::Network
    } else {
        crate::events::Reason::ProtocolError
    };
    admission.permits.report(
        g,
        route,
        model,
        reason,
        if retry {
            crate::events::Action::Reconnecting
        } else if received {
            crate::events::Action::NotRetried
        } else {
            crate::events::Action::Returned
        },
        Some(101),
        attempts,
        details,
    );
    if let Some(u) = &observed {
        forward::settle_protocol(u, &mut admission.permits, cfg, Some(101));
    } else {
        admission.permits.failure(cfg, None);
    }
    retry
}
fn close_code(frame: &Frame) -> Option<u16> {
    let bytes = frame.payload();
    (bytes.len() >= 2).then(|| u16::from_be_bytes([bytes[0], bytes[1]]))
}

#[allow(clippy::too_many_arguments)]
async fn session_once(
    g: &Gateway,
    client: &mut Peer,
    cfg: &Settings,
    manual: bool,
    mut ids: Vec<String>,
    mut routes: HashMap<String, Route>,
    headers: HeaderMap,
    uri: Uri,
    next_turn: &mut Option<Frame>,
) -> Result<(), Failure> {
    let first_deadline = tokio::time::Instant::now() + Duration::from_secs(cfg.first_byte_seconds);
    let first = if let Some(frame) = next_turn.take() {
        frame
    } else {
        loop {
            let frame = tokio::time::timeout_at(first_deadline, client.incoming.recv())
                .await
                .map_err(|_| (1008, "missing first response.create"))?
                .ok_or((1000, "client closed"))?;
            if frame.opcode() == OpCode::Close {
                return Ok(());
            }
            if frame.opcode() == OpCode::Ping {
                client.send(Frame::pong(frame.payload().to_vec())).await?;
                continue;
            }
            if frame.opcode() == OpCode::Pong {
                continue;
            }
            if !creates(&frame) {
                return Err((1008, "expected response.create"));
            }
            break frame;
        }
    };
    let mut pinned = manual.then(|| ids.first().cloned()).flatten();
    let mut unknown_affinity = false;
    if let Some(previous) = value(&first).and_then(|v| {
        v.get("previous_response_id")
            .and_then(|v| v.as_str())
            .map(str::to_owned)
    }) {
        let s = g.0.inner.lock().unwrap();
        if let Some((owner, _, _)) = s
            .affinity
            .get(&previous)
            .filter(|(_, _, at)| at.elapsed() < Duration::from_secs(3600))
        {
            pinned = Some(owner.clone());
            ids = vec![owner.clone()];
        } else {
            ids.truncate(1);
            unknown_affinity = true;
        }
    }
    let mut current_model = turn_model(g, &first, None, None)?;
    let requirement = Requirement::model(current_model.as_deref());
    let first_hints =
        value(&first).and_then(|v| super::replay::RequestHints::from_value(v, false).ok());
    let conversation = super::compaction::session(&headers);
    let mut ownership = if g.client_id() == super::ClientId::Codex && !manual && !unknown_affinity {
        let eligible: Vec<_> = ids
            .iter()
            .filter(|id| {
                routes
                    .get(*id)
                    .is_some_and(|r| requirement.allows(r.provider.allowed_models.as_deref()))
            })
            .filter_map(|id| routes.get(id).map(|r| r.provider.clone()))
            .collect();
        if let Some(session) = conversation.clone().filter(|_| !eligible.is_empty()) {
            Some(
                g.0.compaction
                    .prepare_policy(
                        session,
                        first_hints.as_ref(),
                        first_hints.as_ref().is_some_and(|h| {
                            h.previous_response_id.is_none() && !h.compaction_trigger
                        }),
                        &eligible,
                    )
                    .ok_or((1013, "CONVERSATION_OWNERSHIP"))?,
            )
        } else {
            None
        }
    } else {
        None
    };
    let mut budget = Budget::new(cfg.queue_seconds);
    if pinned.is_some() || unknown_affinity {
        budget.pin_provider();
    }
    let mut attempts = 0usize;
    let mut ordinary_attempts = 0usize;
    let mut transient_retries = 0;
    let mut retry_owner: Option<String> = None;
    let mut capacity_protected = false;
    let mut retry_capacity = false;
    let mut capacity_pending = false;
    let mut capacity_retry_after: Option<Duration> = None;
    let mut capacity_sources = Vec::new();
    let mut last_error: Option<crate::events::Record> = None;
    let mut last_model_payload: Option<Vec<u8>> = None;
    let mut previous_provider: Option<String> = None;
    let mut unsupported_seen = false;
    let mut non_unsupported_failure = false;
    let mut usage_trace = g.usage_operation(
        current_model.as_deref(),
        if first_hints.as_ref().is_some_and(|h| h.compaction_trigger) {
            crate::usage::model::Operation::Compaction
        } else {
            crate::usage::model::Operation::Model
        },
    );
    let (mut upstream, admission, initial_protocol) = loop {
        if ordinary_attempts > cfg.max_retries
            || (unknown_affinity && !capacity_protected && attempts > 0)
        {
            g.record_final(
                last_error.as_ref(),
                previous_provider.as_deref(),
                current_model.as_deref(),
                None,
                attempts,
            );
            if let Some(payload) = last_model_payload.take() {
                client.send(Frame::text(payload)).await?;
                return Err((1008, "MODEL_UNAVAILABLE"));
            }
            return Err(if unsupported_seen && !non_unsupported_failure {
                (1008, "WS_UNSUPPORTED")
            } else {
                (1013, "all providers failed")
            });
        }
        if ids.is_empty() {
            if !capacity_pending {
                g.record_final(
                    last_error.as_ref(),
                    previous_provider.as_deref(),
                    current_model.as_deref(),
                    None,
                    attempts,
                );
                if let Some(payload) = last_model_payload.take() {
                    client.send(Frame::text(payload)).await?;
                    return Err((1008, "MODEL_UNAVAILABLE"));
                }
                return Err(if unsupported_seen && !non_unsupported_failure {
                    (1008, "WS_UNSUPPORTED")
                } else {
                    (1013, "all providers failed")
                });
            }
            if *client.closed.borrow() {
                return Ok(());
            }
            while_connecting(
                client,
                g.0.admission.wait_capacity(
                    &capacity_sources,
                    forward::capacity_delay(cfg, capacity_retry_after),
                    cfg.max_waiting,
                ),
            )
            .await?
            .map_err(rejected)?;
            ids = g.routing_ids(pinned.as_deref());
            routes = ids
                .iter()
                .filter_map(|id| g.route(id).map(|r| (id.clone(), r)))
                .collect();
            if ids.is_empty() {
                return Err((1013, "provider queue is empty"));
            }
            capacity_pending = false;
            capacity_retry_after = None;
            capacity_sources.clear();
            continue;
        }
        let mut candidates: Vec<_> = ids
            .iter()
            .filter_map(|id| routes.get(id).cloned())
            .collect();
        if let Some(owner) = retry_owner.take() {
            candidates.retain(|route| route.provider.id == owner);
        } else if let Some(lease) = &ownership {
            candidates = lease.candidates(candidates);
        }
        let mut admission = match take_slot(
            g,
            client,
            &candidates,
            manual,
            cfg,
            &mut budget,
            &requirement,
            capacity_pending,
        )
        .await
        {
            Ok(admission) => admission,
            Err((code, reason))
                if capacity_pending && code == 1013 && reason != "RPM ledger unavailable" =>
            {
                ids.clear();
                continue;
            }
            Err(error) => {
                if let Some(payload) = last_model_payload.take() {
                    client.send(Frame::text(payload)).await?;
                    return Err((1008, "MODEL_UNAVAILABLE"));
                }
                return Err(error);
            }
        };
        if ownership
            .as_ref()
            .is_some_and(|lease| !lease.admitted(&admission.route.provider.id))
        {
            admission.permits.neutral(cfg);
            if g.0.compaction.error().is_some() {
                return Err((1013, "CONVERSATION_OWNERSHIP"));
            }
            continue;
        }
        if let Err(reason) = admission.commit_rpm() {
            return Err(rejected(reason));
        }
        ids.retain(|id| id != &admission.route.provider.id);
        attempts = attempts.saturating_add(1);
        ordinary_attempts = ordinary_attempts.saturating_add(1);
        if unknown_affinity {
            pinned = Some(admission.route.provider.id.clone());
        }
        let rerouted = previous_provider
            .as_deref()
            .is_some_and(|id| id != admission.route.provider.id);
        previous_provider = Some(admission.route.provider.id.clone());
        let mut attempt_protocol = super::protocol::Protocol::new(true);
        attempt_protocol.attach_ownership(ownership.clone(), &admission.route.provider.id);
        let native_mode = uses_native_websocket(
            admission.route.client_id,
            admission.route.provider.supports_websocket,
        );
        if let Some(trace) = &usage_trace {
            attempt_protocol.attach_usage(trace.attempt(
                &admission.route.provider.id,
                true,
                if native_mode { "websocket" } else { "bridge" },
            ));
        }
        let result = while_connecting(client, async {
            if uses_native_websocket(
                admission.route.client_id,
                admission.route.provider.supports_websocket,
            ) {
                upstream_native(&admission.route, &uri, &headers, cfg)
                    .await
                    .map(Upstream::Native)
            } else {
                upstream_bridge(&admission.route, &uri, &headers, cfg, &first)
                    .await
                    .map(|turn| Upstream::Bridge(Some(turn)))
            }
        })
        .await?;
        match result {
            Ok(upstream) => {
                if rerouted {
                    g.record(
                        Some(&admission.route.provider.id),
                        current_model.as_deref(),
                        crate::events::Reason::Failover,
                        crate::events::Action::Routed,
                        None,
                        Some(attempts.min(u32::MAX as usize) as u32),
                    );
                }
                attempt_protocol.websocket_status(if native_mode { 101 } else { 200 });
                break (upstream, admission, attempt_protocol);
            }
            Err(failure) => {
                attempt_protocol.finish(
                    failure.status,
                    if failure.transport_failure() {
                        "NETWORK"
                    } else {
                        "HTTP"
                    },
                );
                if failure.context_incompatible {
                    last_error = Some(record_failure(
                        &mut admission.permits,
                        g,
                        &admission.route,
                        current_model.as_deref(),
                        &failure,
                        crate::events::Action::TryingNext,
                        attempts,
                    ));
                    admission.permits.neutral(cfg);
                    if let Some(owner) = ownership
                        .as_ref()
                        .filter(|lease| lease.handoff)
                        .and_then(|lease| lease.owner.as_ref())
                        .filter(|id| ids.contains(id))
                    {
                        ids = vec![owner.clone()];
                        continue;
                    }
                    if let Some(payload) = failure.model_payload {
                        client.send(Frame::text(payload)).await?;
                    }
                    return Err((1008, "COMPACTION_CONTEXT_UNSUPPORTED"));
                }
                if !failure.model_unavailable {
                    last_model_payload = None;
                }
                if failure.unsupported {
                    unsupported_seen = true;
                    last_error = Some(record_failure(
                        &mut admission.permits,
                        g,
                        &admission.route,
                        current_model.as_deref(),
                        &failure,
                        crate::events::Action::TryingNext,
                        attempts,
                    ));
                    admission.permits.neutral(cfg);
                    continue;
                }
                non_unsupported_failure = true;
                if failure.capacity {
                    ordinary_attempts = ordinary_attempts.saturating_sub(1);
                    capacity_protected = true;
                    budget.protect_capacity();
                }
                let retryable =
                    failure.model_unavailable || failure.capacity || failure.retryable();
                let retry_delay = if admission.route.client_id == super::ClientId::Codex
                    && failure.status.is_some_and(circuit::is_server_error)
                    && !failure.model_unavailable
                    && !failure.capacity
                    && !failure.hard_rejection()
                    && !unknown_affinity
                    && ordinary_attempts <= cfg.max_retries
                    && (pinned.as_deref() == Some(&admission.route.provider.id)
                        || ownership
                            .as_ref()
                            .is_some_and(|lease| lease.owns(&admission.route.provider.id)))
                {
                    super::routing::transient_delay(&mut transient_retries, failure.retry)
                } else {
                    None
                };
                let mut failure = failure;
                failure.details.wait_seconds = retry_delay.map(|delay| delay.as_secs());
                last_error = Some(record_failure(
                    &mut admission.permits,
                    g,
                    &admission.route,
                    current_model.as_deref(),
                    &failure,
                    if retry_delay.is_some() {
                        crate::events::Action::RetryingSame
                    } else if !ids.is_empty()
                        && !unknown_affinity
                        && ordinary_attempts <= cfg.max_retries
                    {
                        crate::events::Action::TryingNext
                    } else if failure.capacity {
                        crate::events::Action::Waiting
                    } else {
                        crate::events::Action::Returned
                    },
                    attempts,
                ));
                if failure.model_unavailable {
                    admission.permits.neutral(cfg);
                    last_model_payload = failure.model_payload;
                    continue;
                }
                last_model_payload = None;
                if failure.hard_rejection() {
                    admission.permits.neutral(cfg);
                } else if failure.capacity {
                    admission.permits.capacity_limited(cfg, failure.retry);
                } else if failure.status == Some(429) {
                    admission.permits.rate_limited(cfg, failure.retry);
                } else if retryable {
                    admission.permits.failure(cfg, failure.retry);
                } else {
                    admission.permits.neutral(cfg);
                }

                if !retryable {
                    return Err((1008, "upstream rejected websocket handshake"));
                }
                if let Some(delay) = retry_delay {
                    let source = CapacitySource {
                        provider_id: admission.route.provider.id.clone(),
                        reset_generation: admission.reset_generation,
                    };
                    let provider_id = source.provider_id.clone();
                    drop(admission);
                    while_connecting(
                        client,
                        g.0.admission.wait_transient(source, delay, cfg.max_waiting),
                    )
                    .await?
                    .map_err(rejected)?;
                    if g.routing_ids(pinned.as_deref()).contains(&provider_id) {
                        ids.insert(0, provider_id.clone());
                        retry_owner = Some(provider_id);
                    }
                    continue;
                }
                if failure.capacity {
                    capacity_pending = true;
                    capacity_sources.push(CapacitySource {
                        provider_id: admission.route.provider.id.clone(),
                        reset_generation: admission.reset_generation,
                    });
                    capacity_retry_after = capacity_retry_after.max(failure.retry);
                }
            }
        }
    };
    let mut route = admission.route.clone();
    let mut turn = Some(admission);
    let mut replay_frame = first.clone();
    let mut retry_pending = false;
    let mut retry_immediately = false;
    let mut tail_deadline = None;
    let mut previous_usage: Option<(super::protocol::Protocol, tokio::time::Instant)> = None;
    let mut protocol = Some(initial_protocol);
    let mut pending: Option<Frame> = None;
    let mut confirmed_model: Option<String> = None;
    let mut received = false;
    let mut terminal_error = false;
    let mut deadline = tokio::time::Instant::now() + Duration::from_secs(cfg.first_byte_seconds);
    if let Upstream::Native(peer) = &mut upstream {
        if let Err(e) = peer.send(first).await {
            retry_pending = disconnect_turn(
                g,
                &route,
                cfg,
                current_model.as_deref(),
                &mut turn,
                &mut protocol,
                false,
                attempts,
                ordinary_attempts,
                !unknown_affinity,
                None,
                crate::events::Phase::WsSend,
                "NETWORK",
            );
            if !retry_pending {
                return Err(e);
            }
            upstream = Upstream::Disconnected;
        }
    }
    loop {
        if retry_pending {
            let delay = if retry_immediately {
                0
            } else if retry_capacity {
                cfg.capacity_retry_seconds
            } else {
                cfg.websocket_retry_seconds
            }
            .max(route.provider_circuit.health().retry_in);
            retry_immediately = false;
            let waited = while_connecting(
                client,
                g.0.admission.wait_websocket(
                    &route.provider.id,
                    Duration::from_secs(delay),
                    cfg.max_waiting,
                ),
            )
            .await;
            match waited {
                Err(TURN_CANCELLED) => {
                    retry_pending = false;
                    protocol = None;
                    usage_trace = None;
                    cancellation(client).await?;
                    continue;
                }
                Err(error) => return Err(error),
                Ok(result) => result.map_err(rejected)?,
            }
            current_model = turn_model(
                g,
                &replay_frame,
                confirmed_model.as_deref(),
                Some(&route.provider.id),
            )?;
            let mut budget = Budget::new(cfg.queue_seconds);
            budget.pin_provider();
            if capacity_protected {
                budget.protect_capacity();
            }
            let acquired = take_slot(
                g,
                client,
                std::slice::from_ref(&route),
                manual,
                cfg,
                &mut budget,
                &Requirement::model(current_model.as_deref()),
                false,
            )
            .await;
            let mut admission = match acquired {
                Err(TURN_CANCELLED) => {
                    retry_pending = false;
                    usage_trace = None;
                    cancellation(client).await?;
                    continue;
                }
                result => result?,
            };
            admission.commit_rpm().map_err(rejected)?;
            attempts = attempts.saturating_add(1);
            ordinary_attempts = ordinary_attempts.saturating_add(1);
            let mut next = super::protocol::Protocol::new(true);
            next.attach_ownership(ownership.clone(), &route.provider.id);
            if let Some(trace) = &usage_trace {
                next.attach_usage(trace.attempt(
                    &route.provider.id,
                    true,
                    if uses_native_websocket(route.client_id, route.provider.supports_websocket) {
                        "websocket"
                    } else {
                        "bridge"
                    },
                ));
            }
            protocol = Some(next);
            turn = Some(admission);
            let opened = while_connecting(client, async {
                if uses_native_websocket(route.client_id, route.provider.supports_websocket) {
                    upstream_native(&route, &uri, &headers, cfg)
                        .await
                        .map(Upstream::Native)
                } else {
                    upstream_bridge(&route, &uri, &headers, cfg, &replay_frame)
                        .await
                        .map(|turn| Upstream::Bridge(Some(turn)))
                }
            })
            .await;
            match opened {
                Err(TURN_CANCELLED) => {
                    turn = None;
                    protocol = None;
                    usage_trace = None;
                    retry_pending = false;
                    cancellation(client).await?;
                    continue;
                }
                Err(error) => return Err(error),
                Ok(Err(failure)) => {
                    let mut admission = turn.take().unwrap();
                    record_failure(
                        &mut admission.permits,
                        g,
                        &route,
                        current_model.as_deref(),
                        &failure,
                        if failure.capacity {
                            crate::events::Action::Waiting
                        } else if !failure.model_unavailable
                            && !failure.unsupported
                            && !failure.hard_rejection()
                            && failure.retryable()
                            && ordinary_attempts <= cfg.max_retries
                        {
                            crate::events::Action::Reconnecting
                        } else {
                            crate::events::Action::Returned
                        },
                        attempts,
                    );
                    if let Some(mut u) = protocol.take() {
                        u.finish(
                            failure.status,
                            if failure.transport_failure() {
                                "NETWORK"
                            } else {
                                "HTTP"
                            },
                        );
                    }
                    if failure.capacity {
                        ordinary_attempts = ordinary_attempts.saturating_sub(1);
                        capacity_protected = true;
                    }
                    retry_capacity = failure.capacity;
                    if failure.model_unavailable
                        || failure.unsupported
                        || failure.hard_rejection()
                        || (!failure.capacity && !failure.retryable())
                    {
                        admission.permits.neutral(cfg);
                        if let Some(payload) = failure.model_payload {
                            client.send(Frame::text(payload)).await?;
                        }
                        return Err((1008, "upstream rejected reconnected turn"));
                    }
                    if failure.capacity {
                        admission.permits.capacity_limited(cfg, failure.retry);
                    } else if failure.status == Some(429) {
                        admission.permits.rate_limited(cfg, failure.retry);
                    } else {
                        admission.permits.failure(cfg, failure.retry);
                    }
                    if ordinary_attempts > cfg.max_retries {
                        return Err((1013, "websocket reconnect retries exhausted"));
                    }
                    continue;
                }
                Ok(Ok(opened)) => {
                    if let Some(u) = &mut protocol {
                        u.websocket_status(if matches!(opened, Upstream::Native(_)) {
                            101
                        } else {
                            200
                        });
                    }
                    upstream = opened;
                }
            }
            retry_pending = false;
            retry_capacity = false;
            if let Err(error) = if matches!(upstream, Upstream::Native(_)) {
                send_to_upstream(&mut upstream, replay_frame.clone()).await
            } else {
                Ok(())
            } {
                retry_pending = disconnect_turn(
                    g,
                    &route,
                    cfg,
                    current_model.as_deref(),
                    &mut turn,
                    &mut protocol,
                    false,
                    attempts,
                    ordinary_attempts,
                    !unknown_affinity,
                    None,
                    crate::events::Phase::WsSend,
                    "NETWORK",
                );
                upstream = Upstream::Disconnected;
                if !retry_pending {
                    return Err(error);
                }
                continue;
            }
            received = false;
            deadline = tokio::time::Instant::now() + Duration::from_secs(cfg.first_byte_seconds);
        }
        if turn.is_none() {
            if let Some(frame) = pending.take() {
                if let Some(previous) = protocol.take().filter(|p| p.succeeded()) {
                    previous_usage = Some((
                        previous,
                        tokio::time::Instant::now() + Duration::from_secs(2),
                    ));
                }
                let model = turn_model(
                    g,
                    &frame,
                    confirmed_model.as_deref(),
                    Some(&route.provider.id),
                )?;
                let requirement = Requirement::model(model.as_deref());
                let hints = value(&frame)
                    .and_then(|v| super::replay::RequestHints::from_value(v, false).ok());
                let enabled = {
                    let state = g.0.inner.lock().unwrap();
                    state.store.mode == "auto"
                };
                if g.client_id() == super::ClientId::Codex && !manual && enabled {
                    let latest: Vec<_> = g
                        .routing_ids(None)
                        .into_iter()
                        .filter_map(|id| g.route(&id))
                        .filter(|route| {
                            requirement.allows(route.provider.allowed_models.as_deref())
                        })
                        .collect();
                    let safe_window = hints
                        .as_ref()
                        .is_some_and(|h| h.previous_response_id.is_none() && !h.compaction_trigger);
                    let boundary = safe_window
                        && conversation.as_ref().is_some_and(|session| {
                            g.0.compaction.boundary_matches(
                                session,
                                hints.as_ref().and_then(|h| h.compacted_window.as_deref()),
                            )
                        });
                    if boundary
                        && latest
                            .iter()
                            .take_while(|r| r.provider.id != route.provider.id)
                            .any(|r| r.provider.handoff_after_compaction)
                    {
                        *next_turn = Some(frame);
                        return Err((1999, "compaction handoff"));
                    }
                    let eligible: Vec<_> = latest.iter().map(|r| r.provider.clone()).collect();
                    ownership = if let Some(session) =
                        conversation.clone().filter(|_| !eligible.is_empty())
                    {
                        Some(
                            g.0.compaction
                                .prepare_policy(session, hints.as_ref(), safe_window, &eligible)
                                .ok_or((1013, "CONVERSATION_OWNERSHIP"))?,
                        )
                    } else {
                        None
                    };
                } else {
                    ownership = None;
                }
                tail_deadline = None;
                let mut budget = Budget::new(cfg.queue_seconds);
                let acquired = take_slot(
                    g,
                    client,
                    std::slice::from_ref(&route),
                    manual,
                    cfg,
                    &mut budget,
                    &requirement,
                    false,
                )
                .await;
                match acquired {
                    Ok(mut admission) => {
                        if let Err(reason) = admission.commit_rpm() {
                            return Err(rejected(reason));
                        }
                        turn = Some(admission)
                    }
                    Err(TURN_CANCELLED) => {
                        cancellation(client).await?;
                        continue;
                    }
                    Err(error) => return Err(error),
                }
                current_model = model;
                attempts = 1;
                ordinary_attempts = 1;
                capacity_protected = false;
                retry_capacity = false;
                terminal_error = false;
                replay_frame = frame.clone();
                usage_trace = g.usage_operation(
                    current_model.as_deref(),
                    if hints.as_ref().is_some_and(|h| h.compaction_trigger) {
                        crate::usage::model::Operation::Compaction
                    } else {
                        crate::usage::model::Operation::Model
                    },
                );
                let mut next_protocol = super::protocol::Protocol::new(true);
                next_protocol.attach_ownership(ownership.clone(), &route.provider.id);
                if let Some(trace) = &usage_trace {
                    next_protocol.attach_usage(trace.attempt(
                        &route.provider.id,
                        true,
                        if uses_native_websocket(route.client_id, route.provider.supports_websocket)
                        {
                            "websocket"
                        } else {
                            "bridge"
                        },
                    ));
                }
                next_protocol.websocket_status(
                    if uses_native_websocket(route.client_id, route.provider.supports_websocket) {
                        101
                    } else {
                        200
                    },
                );
                protocol = Some(next_protocol);
                received = false;
                deadline =
                    tokio::time::Instant::now() + Duration::from_secs(cfg.first_byte_seconds);
                if !uses_native_websocket(route.client_id, route.provider.supports_websocket)
                    && route.client_id == super::ClientId::Codex
                {
                    let opened = while_connecting(
                        client,
                        upstream_bridge(&route, &uri, &headers, cfg, &frame),
                    )
                    .await;
                    let opened = match opened {
                        Ok(result) => result,
                        Err(TURN_CANCELLED) => {
                            if let Some(mut admission) = turn.take() {
                                admission.permits.neutral(cfg);
                            }
                            protocol = None;
                            cancellation(client).await?;
                            continue;
                        }
                        Err(error) => return Err(error),
                    };
                    match opened {
                        Ok(bridge) => upstream = Upstream::Bridge(Some(bridge)),
                        Err(failure) => {
                            if let Some(u) = &mut protocol {
                                u.finish(failure.status, "HTTP");
                            }
                            if let Some(mut admission) = turn.take() {
                                record_failure(
                                    &mut admission.permits,
                                    g,
                                    &route,
                                    current_model.as_deref(),
                                    &failure,
                                    if failure.capacity {
                                        crate::events::Action::Waiting
                                    } else {
                                        crate::events::Action::Returned
                                    },
                                    attempts,
                                );
                                if failure.model_unavailable || failure.hard_rejection() {
                                    admission.permits.neutral(cfg);
                                } else if failure.capacity {
                                    admission.permits.capacity_limited(cfg, failure.retry);
                                } else if failure.status == Some(429) {
                                    admission.permits.rate_limited(cfg, failure.retry);
                                } else if failure.unsupported {
                                    admission.permits.neutral(cfg);
                                } else if failure.retryable() {
                                    admission.permits.failure(cfg, failure.retry);
                                } else {
                                    admission.permits.neutral(cfg);
                                }
                            }
                            if failure.capacity {
                                ordinary_attempts = ordinary_attempts.saturating_sub(1);
                                capacity_protected = true;
                                retry_capacity = true;
                                retry_pending = true;
                                protocol = None;
                                upstream = Upstream::Disconnected;
                                continue;
                            }
                            if let Some(payload) = failure.model_payload {
                                client.send(Frame::text(payload)).await?;
                            }
                            return Err(if failure.model_unavailable {
                                (1008, "MODEL_UNAVAILABLE")
                            } else if failure.unsupported {
                                (1008, "WS_UNSUPPORTED")
                            } else {
                                (1013, "bridge request failed")
                            });
                        }
                    }
                } else {
                    if matches!(upstream, Upstream::Disconnected) {
                        match while_connecting(client, upstream_native(&route, &uri, &headers, cfg))
                            .await?
                        {
                            Ok(peer) => upstream = Upstream::Native(peer),
                            Err(failure) => {
                                if let Some(mut admission) = turn.take() {
                                    record_failure(
                                        &mut admission.permits,
                                        g,
                                        &route,
                                        current_model.as_deref(),
                                        &failure,
                                        if failure.capacity {
                                            crate::events::Action::Waiting
                                        } else if !failure.model_unavailable
                                            && !failure.unsupported
                                            && !failure.hard_rejection()
                                            && failure.retryable()
                                            && ordinary_attempts <= cfg.max_retries
                                        {
                                            crate::events::Action::Reconnecting
                                        } else {
                                            crate::events::Action::Returned
                                        },
                                        attempts,
                                    );
                                    if let Some(mut u) = protocol.take() {
                                        u.finish(
                                            failure.status,
                                            if failure.transport_failure() {
                                                "NETWORK"
                                            } else {
                                                "HTTP"
                                            },
                                        );
                                    }
                                    if failure.model_unavailable
                                        || failure.unsupported
                                        || failure.hard_rejection()
                                    {
                                        admission.permits.neutral(cfg);
                                        return Err((1008, "upstream rejected websocket"));
                                    }
                                    if failure.capacity {
                                        admission.permits.capacity_limited(cfg, failure.retry);
                                    } else if failure.status == Some(429) {
                                        admission.permits.rate_limited(cfg, failure.retry);
                                    } else if failure.retryable() {
                                        admission.permits.failure(cfg, failure.retry);
                                    } else {
                                        admission.permits.neutral(cfg);
                                        return Err((1008, "upstream rejected websocket"));
                                    }
                                }
                                if failure.capacity {
                                    ordinary_attempts = ordinary_attempts.saturating_sub(1);
                                    capacity_protected = true;
                                }
                                retry_capacity = failure.capacity;
                                retry_pending = ordinary_attempts <= cfg.max_retries;
                                if !retry_pending {
                                    return Err((1013, "websocket reconnect retries exhausted"));
                                }
                                continue;
                            }
                        }
                    }
                    if let Err(e) = send_to_upstream(&mut upstream, frame).await {
                        retry_pending = disconnect_turn(
                            g,
                            &route,
                            cfg,
                            current_model.as_deref(),
                            &mut turn,
                            &mut protocol,
                            false,
                            attempts,
                            ordinary_attempts,
                            !unknown_affinity,
                            None,
                            crate::events::Phase::WsSend,
                            "NETWORK",
                        );
                        upstream = Upstream::Disconnected;
                        if !retry_pending {
                            return Err(e);
                        }
                        continue;
                    }
                }
            }
        }
        tokio::select! {
            biased;
            _ = async { if let Some((_, at)) = &previous_usage { tokio::time::sleep_until(*at).await } else { std::future::pending::<()>().await } } => { previous_usage = None; },
            _ = async { if let Some(at) = tail_deadline { tokio::time::sleep_until(at).await } else { std::future::pending::<()>().await } }, if turn.is_none() => {
                protocol = None;
                usage_trace = None;
                tail_deadline = None;
                if let Upstream::Bridge(active) = &mut upstream { active.take(); }
            },
            event = next_upstream(&mut upstream), if turn.is_some() || protocol.is_some() || matches!(upstream, Upstream::Native(_)) => {
                let disconnected = match &event { None => true, Some(Err(_)) => true, Some(Ok(frame)) => frame.opcode() == OpCode::Close };
                if disconnected {
                    let close = event.as_ref().and_then(|v| v.as_ref().ok()).filter(|f| f.opcode() == OpCode::Close);
                    // Only a successfully settled/cancelled turn may treat this
                    // as an idle disconnect. An error already sent downstream
                    // must retain the upstream close without a second failure.
                    if turn.is_none() && !terminal_error && route.client_id == super::ClientId::Codex {
                        upstream = if matches!(upstream, Upstream::Native(_)) { Upstream::Disconnected } else { Upstream::Bridge(None) };
                        protocol = None;
                        usage_trace = None;
                        tail_deadline = None;
                        continue;
                    }
                    let native = matches!(upstream, Upstream::Native(_));
                    let unfinished_native_turn = native && turn.is_some() && !received && route.client_id == super::ClientId::Codex;
                    retry_pending = disconnect_turn(g, &route, cfg, current_model.as_deref(), &mut turn, &mut protocol, received, attempts, ordinary_attempts, native && !unknown_affinity, close, crate::events::Phase::WsReceive, "STREAM_INTERRUPTED");
                    upstream = Upstream::Disconnected;
                    if retry_pending { continue; }
                    if unfinished_native_turn { return Err((1013, "websocket reconnect retries exhausted")); }
                    if let Some(Ok(frame)) = event { client.send(frame).await?; return Ok(()); }
                    return Err((1013, "upstream disconnected before completion"));
                }
                let frame = event.unwrap()?;
                if let Some(v) = value(&frame) {
                    let mut observed = super::protocol::Observation::default();
                    observed.value(&v);
                    let usage_only = matches!(v.get("type").and_then(|v| v.as_str()), Some("usage" | "usage.updated" | "response.usage" | "response.usage.updated"));
                    let belongs_to_previous = protocol.as_ref().and_then(|u| u.observation.response_id.as_ref()).map_or(usage_only, |id| Some(id) != observed.response_id.as_ref());
                    if let Some((previous, _)) = previous_usage.as_mut().filter(|(previous, _)| belongs_to_previous && observed.response_id.is_some() && previous.observation.response_id == observed.response_id) {
                        previous.value(&v);
                        client.send(frame).await?;
                        continue;
                    }
                }
                if turn.is_some() && !received && ownership.as_ref().is_some_and(|lease| lease.handoff && lease.owner.as_deref() != Some(&route.provider.id)) && value(&frame).is_some_and(|v| super::compaction::incompatible(&v)) {
                    if let Some(old) = ownership.as_ref().and_then(|lease| lease.owner.as_ref()).and_then(|id| g.route(id)).filter(|old| old.provider.queued && Requirement::model(current_model.as_deref()).allows(old.provider.allowed_models.as_deref())) {
                        if ordinary_attempts <= cfg.max_retries {
                            if let Some(mut admission) = turn.take() {
                                let details = value(&frame).map(|v| super::upstream_error::details_value(&v, false)).unwrap_or_default();
                                admission.permits.report(g, &route, current_model.as_deref(), crate::events::Reason::ProtocolError, crate::events::Action::TryingNext, Some(101), attempts, crate::events::Details { counted_failure: Some(false), ..details });
                                admission.permits.neutral(cfg);
                            }
                            if let Some(mut u) = protocol.take() { if let Some(v) = value(&frame) { u.value(&v); } u.finish(Some(101), "CLIENT_ERROR"); }
                            route = old;
                            upstream = Upstream::Disconnected;
                            retry_pending = true;
                            retry_immediately = true;
                            retry_capacity = false;
                            continue;
                        }
                    }
                }
                // After binding, capacity retries stay on this provider and
                // reuse the original turn only before any business event is sent.
                if route.client_id == super::ClientId::Codex && turn.is_some() && !received {
                    if let Some(failure) = first_event_failure(&frame).filter(|f| f.capacity) {
                        if let Some(mut admission) = turn.take() { record_failure(&mut admission.permits, g, &route, current_model.as_deref(), &failure, crate::events::Action::Waiting, attempts); admission.permits.capacity_limited(cfg, failure.retry); }
                        if let Some(mut u) = protocol.take() { if let Some(v) = value(&frame) { u.value(&v); } u.finish(Some(if matches!(upstream, Upstream::Native(_)) {101} else {200}), "UPSTREAM_ERROR"); }
                        ordinary_attempts = ordinary_attempts.saturating_sub(1);
                        capacity_protected = true; retry_capacity = true; retry_pending = true;
                        upstream = Upstream::Disconnected;
                        continue;
                    }
                }
                if !frame.opcode().is_control() {
                    received = true;
                    deadline = tokio::time::Instant::now() + Duration::from_secs(cfg.idle_seconds);
                }
                if let Some(v) = value(&frame) {
                    if let Some(u)=&mut protocol {u.value(&v);}
                    if let Some(id) = v.pointer("/response/id").or_else(|| v.get("response_id")).and_then(|v| v.as_str()) { g.remember_model(id, &route.provider.id, current_model.as_deref()); }
                    let kind = v.get("type").and_then(|v| v.as_str()).unwrap_or("");
                    if matches!(kind, "response.created" | "response.completed" | "response.done") { confirmed_model = current_model.clone(); }
                    if protocol.as_ref().is_some_and(|u| u.terminal().is_some()) || matches!(kind, "response.completed" | "response.done" | "response.failed" | "response.incomplete" | "response.cancelled" | "response.canceled" | "error") {
                        if let Some(mut admission) = turn.take() {
                            if let Some(u)=protocol.as_mut() {
                                u.finish(Some(if uses_native_websocket(route.client_id, route.provider.supports_websocket) { 101 } else { 200 }), "UPSTREAM_ERROR");
                                terminal_error = !matches!(u.terminal(), Some(super::protocol::Terminal::Success | super::protocol::Terminal::Limited | super::protocol::Terminal::Cancelled));
                                if route.client_id == super::ClientId::Codex && first_event_failure(&frame).is_some_and(|failure| failure.capacity) {
                                    if let Some(failure) = first_event_failure(&frame) { record_failure(&mut admission.permits, g, &route, current_model.as_deref(), &failure, crate::events::Action::Returned, attempts); }
                                    admission.permits.capacity_limited(cfg, None);
                                } else {
                                    forward::observe_protocol(u, &mut admission.permits, cfg, g, &route, current_model.as_deref(), Some(if uses_native_websocket(route.client_id, route.provider.supports_websocket) { 101 } else { 200 }), attempts);
                                }
                                if u.succeeded() { g.successful_response(&route.provider); }
                            }

                        }
                        if tail_deadline.is_none() { tail_deadline = Some(tokio::time::Instant::now() + Duration::from_secs(2)); }
                    }
                }
                if let Err(error) = client.send(frame).await {
                    if turn.is_none() { drain_completed(&mut upstream, &mut protocol).await; }
                    return Err(error);
                }
            },
            frame = client.incoming.recv() => {
                let Some(frame) = frame else { if turn.is_none() { drain_completed(&mut upstream, &mut protocol).await; } return Ok(()); };
                let closing = frame.opcode() == OpCode::Close;
                if frame.opcode() == OpCode::Ping && !matches!(upstream, Upstream::Native(_)) {
                    client.send(Frame::pong(frame.payload().to_vec())).await?;
                } else if frame.opcode() == OpCode::Pong && !matches!(upstream, Upstream::Native(_)) {
                    continue;
                } else if closing {
                    if turn.is_none() { drain_completed(&mut upstream, &mut protocol).await; return Ok(()); }
                    if let Upstream::Native(peer) = &upstream { peer.send(frame.clone()).await?; }
                    if let Upstream::Bridge(Some(bridge)) = &upstream {
                        bridge.cancel();
                    }
                    if let Some(mut admission) = turn.take() {
                        if let Some(mut u) = protocol.take() {
                            u.finish(None, "CANCELLED");
                            admission.permits.neutral(cfg);
                        }
                    }
                    return Ok(());
                } else if creates(&frame) {
                    if pending.is_some() { return Err((1013, "too many pending turns")); }
                    pending = Some(frame);
                } else if value(&frame).is_some_and(|v| v["type"] == "response.cancel")
                    && matches!(&upstream, Upstream::Bridge(_))
                {
                    if let Upstream::Bridge(Some(bridge)) = &upstream {
                        bridge.cancel();
                    }
                    cancellation(client).await?;
                    if let Some(mut admission) = turn.take() {
                        if let Some(mut u) = protocol.take() {
                            u.finish(Some(101), "CANCELLED");
                            admission.permits.neutral(cfg);
                        }
                    }
                    if let Upstream::Bridge(active) = &mut upstream {
                        active.take();
                    }
                    usage_trace = None;
                } else if let Err(e) = send_to_upstream(&mut upstream, frame).await {
                    if e.0 == 1008 {
                        if let Some(mut admission) = turn.take() {
                            admission.permits.report(g, &route, current_model.as_deref(), crate::events::Reason::ProtocolError, crate::events::Action::Returned, Some(101), attempts,
                                crate::events::Details { phase: Some(crate::events::Phase::WsSend), counted_failure: Some(false), local_code: Some("UNSUPPORTED_EVENT".into()), ..Default::default() });
                            admission.permits.neutral(cfg);
                        }
                    } else {
                        // A client control/message send failure cannot safely replay a generation.
                        disconnect_turn(g, &route, cfg, current_model.as_deref(), &mut turn, &mut protocol, received, attempts, ordinary_attempts, false, None, crate::events::Phase::WsSend, "NETWORK");
                    }
                    return Err(e);
                }
            },
            _ = client.closed.changed() => { if turn.is_none() { drain_completed(&mut upstream, &mut protocol).await; } return Ok(()); },
            _ = tokio::time::sleep_until(deadline), if turn.is_some() => {
                let native = matches!(upstream, Upstream::Native(_));
                retry_pending = disconnect_turn(g, &route, cfg, current_model.as_deref(), &mut turn, &mut protocol, received, attempts, ordinary_attempts, native && !unknown_affinity, None, crate::events::Phase::WsReceive, if received { "STREAM_TIMEOUT" } else { "FIRST_BYTE_TIMEOUT" });
                upstream = Upstream::Disconnected;
                if retry_pending { continue; }
                return Err((1013, "upstream timeout"));
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bridge_payload_only_changes_websocket_envelope() {
        let frame = Frame::text(
            r#"{"type":"response.create","model":"gpt-test","stream":false,"future":{"x":1}}"#,
        );
        let value: serde_json::Value =
            serde_json::from_slice(&bridge_payload(&frame).unwrap()).unwrap();
        assert_eq!(value["model"], "gpt-test");
        assert_eq!(value["future"]["x"], 1);
        assert_eq!(value["stream"], true);
        assert!(value.get("type").is_none());
    }

    #[tokio::test]
    async fn bridge_events_preserve_json_and_support_multiline_crlf() {
        let input = b"data: {\"type\":\"response.output_text.delta\",\"delta\":\r\ndata: \"delta\"}\r\n\r\ndata: [DONE]\r\n\r\n";
        let reader: BoxReader = Box::new(BufReader::new(std::io::Cursor::new(input.to_vec())));
        let (sender, mut receiver) = mpsc::channel(4);
        bridge_events(reader, sender).await;
        let first = receiver.recv().await.unwrap().unwrap();
        assert_eq!(
            first.payload().as_ref(),
            b"{\"type\":\"response.output_text.delta\",\"delta\":\n\"delta\"}"
        );
        assert!(receiver.recv().await.is_none());
    }

    #[test]
    fn claude_routes_are_always_native() {
        assert!(uses_native_websocket(super::super::ClientId::Claude, false));
        assert!(uses_native_websocket(super::super::ClientId::Codex, true));
        assert!(!uses_native_websocket(super::super::ClientId::Codex, false));
    }

    #[tokio::test]
    async fn bridge_event_limit_applies_before_newline_and_after_decompression() {
        use std::io::Write;
        let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gzip.write_all(&vec![b'x'; MAX_SSE_EVENT + 100]).unwrap();
        let reader: BoxReader =
            Box::new(BufReader::new(std::io::Cursor::new(gzip.finish().unwrap())));
        let (sender, mut incoming) = mpsc::channel(2);
        bridge_events(decode_reader(reader, "gzip").unwrap(), sender).await;
        assert!(matches!(incoming.recv().await, Some(Err((1009, _)))));
        assert!(incoming.recv().await.is_none());
    }

    #[tokio::test]
    async fn bridge_backpressure_is_bounded_and_dropping_turn_aborts_reader() {
        struct ReadGuard(std::sync::Arc<std::sync::atomic::AtomicBool>);
        impl Drop for ReadGuard {
            fn drop(&mut self) {
                self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let ended = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker_ended = ended.clone();
        let input =
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"x\"}\n\n".repeat(100);
        let reader: BoxReader = Box::new(BufReader::new(std::io::Cursor::new(input.into_bytes())));
        let (sender, incoming) = mpsc::channel(2);
        let task = tokio::spawn(async move {
            let _guard = ReadGuard(worker_ended);
            bridge_events(reader, sender).await;
        });
        let turn = BridgeTurn {
            first: None,
            incoming,
            task,
        };
        tokio::time::timeout(Duration::from_secs(1), async {
            while turn.incoming.len() < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(!turn.task.is_finished());
        drop(turn);
        tokio::time::timeout(Duration::from_secs(1), async {
            while !ended.load(std::sync::atomic::Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }
    #[tokio::test]
    async fn keepalive_preserves_control_frames_and_does_not_leak_its_own_pong() {
        let (a, b) = tokio::io::duplex(4096);
        let peer = WebSocket::from_stream(a, yawc::Role::Server, Options::default()).unwrap();
        let mut peer = Peer::new(peer, Some(Duration::from_millis(20)));
        let mut client = WebSocket::from_stream(b, yawc::Role::Client, Options::default()).unwrap();
        let ping = tokio::time::timeout(Duration::from_secs(1), client.next())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ping.opcode(), OpCode::Ping);
        client
            .send(Frame::pong(ping.payload().clone()))
            .await
            .unwrap();
        client.send(Frame::pong("unrelated-control")).await.unwrap();
        let got = tokio::time::timeout(Duration::from_secs(1), peer.incoming.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got.opcode(), OpCode::Pong);
        assert_eq!(got.payload().as_ref(), b"unrelated-control");
        // Data writes and heartbeat share one writer rather than racing sinks.
        peer.send(Frame::text("unchanged")).await.unwrap();
        loop {
            let got = client.next().await.unwrap();
            if got.opcode() == OpCode::Text {
                assert_eq!(got.payload().as_ref(), b"unchanged");
                break;
            }
        }
    }
}
