//! Responses uses per-generation slots, following Sub2API's BeforeTurn/AfterTurn
//! behavior (a3eb7ef3). Native message payloads stay unchanged; retries never
//! replay a generation after downstream output.
use super::{
    admission::{Admission, Budget, CapacitySource, Rejected},
    circuit, forward,
    model::Settings,
    replay,
    routing::Requirement,
    Active, Gateway, Route,
};
use crate::errors::code;
use base64::{engine::general_purpose::STANDARD, Engine};
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use http_body_util::BodyExt;
use hyper::{body::Incoming, header, HeaderMap, Request, Response, StatusCode, Uri};
use hyper_util::rt::TokioIo;
use sha1::{Digest, Sha1};
use std::{collections::HashMap, future::Future, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::{mpsc, oneshot, watch},
};
use yawc::{Frame, OpCode, Options, WebSocket};
type Failure = (u16, &'static str);
const MAX_MESSAGE: usize = 64 * 1024 * 1024;
const KEEPALIVE: Duration = Duration::from_secs(20);
// Only bounds late usage observation after a settled turn. It never closes
// the upstream connection or limits forwarding / generation progress.
const USAGE_TAIL: Duration = Duration::from_secs(2);

async fn cancelled_usage_tail(
    upstream: &mut Upstream,
    protocol: &mut Option<super::protocol::Protocol>,
    activity: &super::wire_progress::Activity,
    writing: bool,
) {
    if let Some(protocol) = protocol {
        protocol.downstream_closed(activity, writing);
        protocol.begin_usage_tail(true);
        let mut stop = protocol.stop_signal();
        let deadline = tokio::time::Instant::now() + super::model::USAGE_TAIL_DEADLINE;
        let end = loop {
            if !matches!(upstream, Upstream::Native(_)) {
                break "response_consumer_closed";
            }
            tokio::select! {
                biased;
                _ = super::forward::stopping(&mut stop) => break "gateway_stopping",
                _ = tokio::time::sleep_until(deadline) => break "usage_tail_deadline",
                next = next_upstream(upstream) => match next {
                    Some(Ok(frame)) if frame.opcode() != OpCode::Close => {
                        if matches!(frame.opcode(), OpCode::Text | OpCode::Binary) {
                            protocol.websocket_progress(frame.payload().len());
                            if let Some(value) = value(&frame) { protocol.value(&value); }
                        }
                    }
                    Some(Ok(frame)) => {
                        protocol.websocket_close(close_code(&frame));
                        break "upstream_eof";
                    }
                    Some(Err(_)) => break "upstream_read_failed",
                    None => break if matches!(upstream, Upstream::Native(peer) if peer.activity.error_code() != 0) {
                        "upstream_read_failed"
                    } else { "upstream_eof" },
                }
            }
        };
        // Drop the socket before persisting the final usage/diagnostic snapshot.
        *upstream = Upstream::Disconnected;
        protocol.finish_usage_tail(end);
    } else {
        *upstream = Upstream::Disconnected;
    }
}
// Internal control result, consumed before writing a close frame.
const TURN_CANCELLED: Failure = (0, "turn cancelled before output");
fn admit_ownership(
    gateway: &Gateway,
    lease: Option<&super::compaction::Lease>,
    route: &Route,
    requirement: &Requirement,
    pinned: bool,
) -> Result<bool, Failure> {
    lease
        .map_or(Ok(true), |lease| {
            lease.admit_request(gateway, &route.provider, requirement, pinned)
        })
        .map_err(|error| {
            let reason = match error.code.as_str() {
                code::STORE => code::STORE,
                code::CONVERSATION_OWNERSHIP => code::CONVERSATION_OWNERSHIP,
                code::MODEL_NOT_ALLOWED => code::MODEL_NOT_ALLOWED,
                code::MODEL_UNDETERMINED => code::MODEL_UNDETERMINED,
                code::PROVIDER => code::PROVIDER,
                _ => code::NO_ELIGIBLE_PROVIDER,
            };
            (1013, reason)
        })
}
fn options() -> Options {
    Options::default()
        .with_balanced_compression()
        .with_limits(MAX_MESSAGE, MAX_MESSAGE)
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
    activity: std::sync::Arc<super::wire_progress::Activity>,
    // Only bounded request identifiers from the upstream handshake, not its
    // authentication or other response headers. Reused turns share these IDs.
    response_identifiers: HeaderMap,
}
impl Peer {
    fn new<S: AsyncRead + AsyncWrite + Unpin + Send + 'static>(
        socket: WebSocket<S>,
        keepalive: Option<Duration>,
        activity: std::sync::Arc<super::wire_progress::Activity>,
        idle_seconds: u64,
    ) -> Self {
        activity.websocket();
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
        let writer_activity = activity.clone();
        let writer = tokio::spawn(async move {
            loop {
                let heartbeat = tokio::time::sleep(keepalive.unwrap_or(KEEPALIVE));
                let outgoing = tokio::select! {
                    outgoing = rx.recv() => outgoing,
                    _ = heartbeat, if keepalive.is_some() => {
                        if !matches!(super::wire_progress::writing(sink.send(Frame::ping(ping.clone())), &writer_activity, idle_seconds).await, Ok(Ok(()))) {
                            break;
                        }
                        continue;
                    }
                };
                let Some(Outgoing { frame, ack }) = outgoing else {
                    break;
                };
                let ok = matches!(
                    super::wire_progress::writing(sink.send(frame), &writer_activity, idle_seconds)
                        .await,
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
            activity,
            response_identifiers: HeaderMap::new(),
        }
    }
    async fn send(&self, frame: Frame) -> Result<(), Failure> {
        let closing = frame.opcode() == OpCode::Close;
        let mut closed = self.closed.clone();
        let sending = async {
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
        };
        // A close reply may still be written after the reader observes Close.
        // Other backpressured writes must stop as soon as the peer disconnects.
        if closing {
            return sending.await;
        }
        if *closed.borrow() {
            return Err((1011, "connection closed"));
        }
        tokio::select! {
            biased;
            result = sending => result,
            _ = closed.changed() => Err((1011, "connection closed")),
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
}
fn response_data_started(upstream: &Upstream, received: bool, since: tokio::time::Instant) -> bool {
    received
        || matches!(upstream, Upstream::Native(peer) if peer.activity.read_at().is_some_and(|at| at >= since))
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
        Upstream::Disconnected => None,
    }
}
async fn send_to_upstream(upstream: &mut Upstream, frame: Frame) -> Result<(), Failure> {
    match upstream {
        Upstream::Native(peer) => peer.send(frame).await,
        Upstream::Disconnected => Err((1011, "upstream is disconnected")),
    }
}
fn unsupported_status(status: u16) -> bool {
    matches!(status, 400 | 404 | 405 | 406 | 426 | 501)
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
    fn classification(&self) -> super::failover::Failure {
        use super::failover::Failure;
        if self.hard_rejection() {
            Failure::AccountRejected
        } else if self.model_unavailable {
            Failure::ModelUnavailable
        } else if self.capacity || self.status == Some(429) {
            Failure::RateLimit
        } else if self.transport_failure() {
            Failure::Transport
        } else if self.context_incompatible || self.unsupported {
            Failure::Protocol
        } else {
            Failure::UpstreamService
        }
    }
    fn hard_rejection(&self) -> bool {
        super::failure_policy::hard_rejection(&self.details)
    }
    fn transport_failure(&self) -> bool {
        self.status.is_none()
            || matches!(
                self.details.local_code.as_deref(),
                Some(
                    code::CONNECT_TIMEOUT
                        | code::WS_HANDSHAKE_TIMEOUT
                        | code::TLS_HANDSHAKE_FAILED
                        | code::CONNECTION_FAILED
                        | code::FIRST_BYTE_TIMEOUT
                        | code::STREAM_INTERRUPTED
                        | code::STREAM_TIMEOUT
                        | code::WS_UPGRADE_FAILED
                )
            )
    }
    fn retryable(&self) -> bool {
        self.transport_failure()
            || self.model_unavailable
            || super::failure_policy::decision(self.status, &self.details).retryable
    }
}
fn local_failure(code: &str) -> Box<crate::events::Details> {
    Box::new(crate::events::Details {
        local_code: Some(code.into()),
        ..Default::default()
    })
}
fn record_failure(
    permits: &mut super::health_settlement::Permits,
    g: &Gateway,
    route: &Route,
    model: Option<&str>,
    failure: &AttemptFailure,
    action: crate::events::Action,
    attempt: usize,
) -> super::health_settlement::Reported {
    use crate::events::Reason;
    let reason = if failure.context_incompatible || failure.unsupported {
        Reason::ProtocolError
    } else if failure.model_unavailable {
        Reason::ModelUnavailable
    } else if failure.transport_failure() {
        Reason::Network
    } else {
        super::failure_policy::decision(failure.status, &failure.details).reason
    };
    let mut details = (*failure.details).clone();
    details.wait_seconds = failure
        .retry
        .map(|d| d.as_secs().saturating_add(u64::from(d.subsec_nanos() > 0)));
    details
        .phase
        .get_or_insert(crate::events::Phase::WsHandshake);
    details.counted_failure = Some(
        !failure.context_incompatible
            && !failure.model_unavailable
            && !failure.unsupported
            && (failure.transport_failure()
                || super::failure_policy::decision(failure.status, &details).counted_failure),
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
        Rejected::Policy => (1008, code::NO_ELIGIBLE_PROVIDER),
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
            code::WEBSOCKET,
            "WebSocket 握手必须使用 GET",
        );
    }
    let activity = request
        .extensions()
        .get::<std::sync::Arc<super::wire_progress::Activity>>()
        .cloned()
        .unwrap_or_default();
    let headers = request.headers().clone();
    let uri = request.uri().clone();
    let upgraded = WebSocket::upgrade_with_options(&mut request, options());
    let Ok((response, future)) = upgraded else {
        return forward::error(
            StatusCode::BAD_REQUEST,
            code::WEBSOCKET,
            "WebSocket 握手无效",
        );
    };
    let stop = match gateway.request_state() {
        Ok(state) => state,
        Err(e) => {
            return forward::error(StatusCode::SERVICE_UNAVAILABLE, e.code.as_str(), &e.message)
        }
    }
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
            activity,
            settings.idle_seconds,
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
    failover: &mut super::failover::Tracker,
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
        failover.rejected(&reason);
        if matches!(reason, Rejected::Model) {
            (1008, requirement.code())
        } else {
            if matches!(reason, Rejected::Policy) {
                g.record_policy_rejection(None, true, "没有符合线程接管策略的供应商");
            }
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
    *request.uri_mut() = route.target(uri).map_err(|_| AttemptFailure {
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
    if let Some(connection) = &route.grok {
        connection
            .authenticate(headers, &route.provider.token)
            .map_err(|_| AttemptFailure {
                status: Some(400),
                retry: None,
                capacity: false,
                unsupported: false,
                model_unavailable: false,
                context_incompatible: false,
                model_payload: None,
                details: local_failure(code::GROK_AUTH),
            })?;
    }
    let key = STANDARD.encode(uuid::Uuid::new_v4().as_bytes());
    headers.insert(header::SEC_WEBSOCKET_KEY, key.parse().unwrap());
    headers.insert(
        header::SEC_WEBSOCKET_EXTENSIONS,
        "permessage-deflate; client_max_window_bits"
            .parse()
            .unwrap(),
    );
    // Establishing the WebSocket is a connection operation, before a generation
    // frame has been sent. Its deadline must not consume the response budget.
    let mut response = match tokio::time::timeout(
        Duration::from_secs(settings.connect_seconds),
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
                details: Box::new(crate::events::Details {
                    local_code: Some(code::WS_HANDSHAKE_TIMEOUT.into()),
                    timeout_phase: Some("ws_handshake".into()),
                    timeout_seconds: Some(settings.connect_seconds),
                    ..Default::default()
                }),
            });
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
        let (response_parts, response_body) = response.into_parts();
        let prefix = response_prefix(response_body, 128 * 1024, settings).await;
        let decoded = replay::decode_prefix(&prefix, &encoding, 128 * 1024).unwrap_or_default();
        let (details, policy_retry) =
            super::failure_policy::http(status.as_u16(), &response_parts.headers, &decoded);
        let retry = retry.max(policy_retry);
        let global_rejection = super::failure_policy::hard_rejection(&details);
        let model_unavailable = super::failure_policy::model_http(status.as_u16(), &decoded);
        let capacity =
            !model_unavailable && !global_rejection && super::failure_policy::temporary(&details);
        return Err(AttemptFailure {
            status: Some(status.as_u16()),
            retry,
            capacity,
            details: Box::new(details),
            model_payload: model_unavailable.then(|| decoded.clone()),
            model_unavailable,
            context_incompatible: serde_json::from_slice(&decoded)
                .ok()
                .is_some_and(|v| super::compaction::incompatible(&v)),
            unsupported: !model_unavailable
                && !global_rejection
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
    let response_identifiers = [
        "x-request-id",
        "request-id",
        "openai-request-id",
        "x-client-request-id",
    ]
    .into_iter()
    .filter_map(|name| {
        response
            .headers()
            .get(name)
            .filter(|value| !value.is_empty() && value.len() <= 512)
            .map(|value| (header::HeaderName::from_static(name), value.clone()))
    })
    .collect();
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
            details: local_failure(code::WS_UPGRADE_FAILED),
        })?;
    let activity = std::sync::Arc::new(super::wire_progress::Activity::default());
    activity.websocket();
    let socket = WebSocket::from_stream_with_extensions(
        super::wire_progress::Tracked::new(TokioIo::new(io), activity.clone()),
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
        details: local_failure(code::WS_EXTENSION_UNSUPPORTED),
    })?;
    let mut peer = Peer::new(
        socket,
        (route.client_id == super::ClientId::Codex).then_some(KEEPALIVE),
        activity,
        settings.idle_seconds,
    );
    peer.response_identifiers = response_identifiers;
    Ok(peer)
}
async fn response_prefix(mut body: Incoming, limit: usize, settings: &Settings) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(limit.min(4096));
    let mut deadline =
        tokio::time::Instant::now() + Duration::from_secs(settings.first_byte_seconds);
    while bytes.len() < limit {
        let Ok(Some(Ok(frame))) = tokio::time::timeout_at(deadline, body.frame()).await else {
            break;
        };
        if let Ok(data) = frame.into_data() {
            let count = data.len().min(limit - bytes.len());
            bytes.extend_from_slice(&data[..count]);
            if count != 0 {
                deadline = tokio::time::Instant::now() + Duration::from_secs(settings.idle_seconds);
            }
        }
    }
    bytes
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
    let details = super::failure_policy::details_value(&v, false);
    let temporary = super::failure_policy::temporary_capacity_event(&v);
    if !observation.compaction_incompatible
        && !temporary
        && !super::failure_policy::hard_rejection(&details)
        && !matches!(
            observation.terminal,
            Some(super::protocol::Terminal::Failure | super::protocol::Terminal::ModelUnavailable)
        )
    {
        return None;
    }
    let capacity = temporary;
    Some(AttemptFailure {
        status: Some(101),
        retry: super::failure_policy::retry_after_value(&v),
        capacity,
        unsupported: false,
        model_unavailable: super::failure_policy::model_error(&v),
        context_incompatible: super::compaction::incompatible(&v),
        model_payload: (super::failure_policy::model_error(&v)
            || super::compaction::incompatible(&v))
        .then(|| frame.payload().to_vec()),
        details: Box::new(details),
    })
}
fn turn_model(
    g: &Gateway,
    frame: &Frame,
    last: Option<&str>,
    pinned: Option<&str>,
) -> Result<Option<String>, Failure> {
    let v = value(frame).ok_or((1008, "invalid response.create"))?;
    let previous = v.get("previous_response_id").and_then(|v| v.as_str());
    let remembered = if let Some(id) = previous {
        g.request_state()
            .map_err(|_| (1011, code::STORE))?
            .affinity
            .get(id)
            .filter(|(_, _, at)| at.elapsed() < Duration::from_secs(3600))
            .cloned()
    } else {
        None
    };
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
    let request_budget = super::wait_budget::WaitBudget::new(cfg.retry_budget_seconds);
    let forwarding = async {
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
                &request_budget,
            )
            .await
            {
                Err((1999, "compaction handoff")) => {
                    current_cfg = g
                        .request_state()
                        .map_err(|_| (1011, code::STORE))?
                        .store
                        .settings
                        .clone();
                    current_ids = g.routing_ids(None);
                    current_routes = current_ids
                        .iter()
                        .filter_map(|id| g.route(id).map(|route| (id.clone(), route)))
                        .collect();
                }
                Err(TURN_CANCELLED) => {
                    request_budget.complete(request_budget.deadline());
                    cancellation(client).await?;
                }
                result => return result,
            }
        }
    };
    tokio::select! {
        biased;
        _ = request_budget.wait() => Err((1013, code::FIRST_BYTE_TIMEOUT)),
        result = forwarding => result,
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
        u.websocket_close(close.and_then(close_code));
        u.finish(Some(101), category);
    }
    let terminal = observed.as_ref().and_then(|u| u.terminal());
    let transport = observed.as_ref().is_none_or(|u| u.transport_failure());
    let retry = super::attempt::transport_retry(
        received,
        route.client_id == super::ClientId::Codex
            && can_replay
            && ordinary_attempts <= cfg.max_retries
            && transport,
        category,
    );
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
    let emit = || {
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
        )
    };
    if let Some(u) = &observed {
        u.report_failure(emit);
    } else {
        emit();
    }
    if let Some(u) = &observed {
        super::health_settlement::settle_protocol(u, &mut admission.permits, cfg, Some(101));
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
    request_budget: &super::wait_budget::WaitBudget,
) -> Result<(), Failure> {
    let first_started = tokio::time::Instant::now();
    let mut first_deadline = first_started + Duration::from_secs(cfg.first_byte_seconds);
    let first = if let Some(frame) = next_turn.take() {
        frame
    } else {
        loop {
            let frame = match tokio::time::timeout_at(first_deadline, client.incoming.recv()).await
            {
                Ok(frame) => frame.ok_or((1000, "client closed"))?,
                Err(_) => {
                    if let Some(at) = client.activity.read_at().filter(|at| *at >= first_started) {
                        let next = at + Duration::from_secs(cfg.idle_seconds);
                        if next > tokio::time::Instant::now() {
                            first_deadline = next;
                            continue;
                        }
                    }
                    return Err((1008, "missing first response.create"));
                }
            };
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
            request_budget.begin();
            break frame;
        }
    };
    let mut turn_received_at = crate::usage::model::now();
    let mut pinned = manual.then(|| ids.first().cloned()).flatten();
    let mut unknown_affinity = false;
    if let Some(previous) = value(&first).and_then(|v| {
        v.get("previous_response_id")
            .and_then(|v| v.as_str())
            .map(str::to_owned)
    }) {
        let s = g.request_state().map_err(|_| (1011, code::STORE))?;
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
    let auxiliary = first_hints.as_ref().is_some_and(|h| h.auxiliary);
    if auxiliary && g.client_id() == super::ClientId::Codex && !manual {
        if let Some(owner) = conversation
            .as_deref()
            .and_then(|s| g.0.compaction.owner(s))
            .filter(|id| ids.contains(id))
        {
            ids = vec![owner];
        }
    }
    let mut ownership =
        if g.client_id() == super::ClientId::Codex && !manual && !unknown_affinity && !auxiliary {
            let queued = super::compaction::queued(g).map_err(|_| (1011, code::STORE))?;
            if let Some(session) = conversation.clone().filter(|_| {
                queued
                    .iter()
                    .any(|p| requirement.allows(p.allowed_models.as_deref()))
            }) {
                Some(
                    g.0.compaction
                        .prepare_request(
                            session,
                            first_hints.as_ref(),
                            first_hints.as_ref().is_some_and(|h| {
                                h.previous_response_id.is_none() && !h.compaction_trigger
                            }),
                            &queued,
                            &requirement,
                            pinned.as_deref(),
                        )
                        .ok_or((1013, code::CONVERSATION_OWNERSHIP))?,
                )
            } else {
                None
            }
        } else {
            None
        };
    let mut established_capacity_owner =
        super::routing::capacity_owner(g.client_id(), manual, ownership.as_deref());
    let mut budget = Budget::new(cfg.queue_seconds);
    budget.track(
        &g.0.admission,
        ownership.as_deref(),
        cfg.thread_idle_release_seconds,
    );
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
    let mut capacity_wait_owner: Option<String> = None;
    let mut hard_denied = std::collections::HashSet::new();
    let mut capacity_retry_after: Option<Duration> = None;
    let mut capacity_sources = Vec::new();
    let mut last_error: Option<super::health_settlement::Reported> = None;
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
    let mut failover = super::failover::Tracker::new(usage_trace.clone());
    if !manual && g.client_id() == super::ClientId::Codex {
        failover.diagnostics(g);
        if let Some(log) = failover.routing() {
            log.pending(
                &headers,
                first_hints.as_ref(),
                ownership.as_deref(),
                pinned.is_some() || unknown_affinity,
            );
        }
    }
    let (mut upstream, admission, initial_protocol) = loop {
        if ordinary_attempts > cfg.max_retries
            || (unknown_affinity && !capacity_protected && attempts > 0)
        {
            failover.finish(super::failover::Exit::AttemptsExhausted);
            g.record_final(
                last_error.as_ref(),
                failover.failure_reported(),
                previous_provider.as_deref(),
                current_model.as_deref(),
                None,
                attempts,
            );
            if let Some(payload) = last_model_payload.take() {
                client.send(Frame::text(payload)).await?;
                return Err((1008, code::MODEL_UNAVAILABLE));
            }
            return Err(if unsupported_seen && !non_unsupported_failure {
                (1008, code::WS_UNSUPPORTED)
            } else {
                (1013, "all providers failed")
            });
        }
        if ids.is_empty() {
            if !capacity_pending {
                failover.finish(super::failover::Exit::NoEligibleProvider);
                g.record_final(
                    last_error.as_ref(),
                    failover.failure_reported(),
                    previous_provider.as_deref(),
                    current_model.as_deref(),
                    None,
                    attempts,
                );
                if let Some(payload) = last_model_payload.take() {
                    client.send(Frame::text(payload)).await?;
                    return Err((1008, code::MODEL_UNAVAILABLE));
                }
                return Err(if unsupported_seen && !non_unsupported_failure {
                    (1008, code::WS_UNSUPPORTED)
                } else {
                    (1013, "all providers failed")
                });
            }
            if *client.closed.borrow() {
                return Ok(());
            }
            failover.wait(
                if capacity_retry_after.is_some() {
                    super::failover::WaitKind::RetryAfter
                } else {
                    super::failover::WaitKind::CapacityRetry
                },
                forward::capacity_delay(cfg, capacity_retry_after).as_secs(),
            );
            let owner_route = capacity_wait_owner
                .as_deref()
                .or_else(|| last_error.as_ref().and_then(|e| e.provider_id.as_deref()))
                .and_then(|id| routes.get(id));
            let exhausted = while_connecting(
                client,
                g.wait_capacity_checked(
                    &capacity_sources,
                    forward::capacity_delay(cfg, capacity_retry_after),
                    cfg.max_waiting,
                    capacity_wait_owner.is_some(),
                    owner_route,
                    last_error.as_deref(),
                ),
            )
            .await?
            .map_err(rejected)?;
            if exhausted {
                if let Some(route) = owner_route {
                    hard_denied.insert((route.provider.id.clone(), route.provider.version.clone()));
                }
                capacity_wait_owner = None;
                failover.failure(super::failover::Failure::AccountRejected);
            }
            ids = g.routing_ids(pinned.as_deref());
            ids.retain(|id| {
                g.route(id).is_some_and(|route| {
                    !hard_denied.contains(&(id.clone(), route.provider.version))
                })
            });
            routes = ids
                .iter()
                .filter_map(|id| {
                    g.route(id)
                        .filter(|r| r.accepts_protocol(uri.path()))
                        .map(|r| (id.clone(), r))
                })
                .collect();
            if ids.is_empty() {
                return Err((1013, "provider queue is empty"));
            }
            retry_owner = capacity_wait_owner
                .take()
                .filter(|owner| ids.contains(owner));
            capacity_pending = false;
            capacity_retry_after = None;
            capacity_sources.clear();
            continue;
        }
        let mut candidates: Vec<_> = ids
            .iter()
            .filter_map(|id| routes.get(id).cloned())
            .filter(|r| r.accepts_protocol(uri.path()))
            .collect();
        if attempts > 0 || !hard_denied.is_empty() {
            budget.handoff_owner = None;
        }
        let matches_model = candidates
            .iter()
            .any(|r| requirement.allows(r.provider.allowed_models.as_deref()));
        let new_thread_model_denied = ownership.as_ref().is_some_and(|lease| {
            lease.admission_policy().0 == super::compaction::Intake::NewThread
        }) && candidates.iter().any(|r| {
            r.provider.take_new_threads && !requirement.allows(r.provider.allowed_models.as_deref())
        });
        if matches_model {
            candidates.retain(|r| requirement.allows(r.provider.allowed_models.as_deref()));
        }
        if let Some(owner) = retry_owner.take() {
            candidates.retain(|route| route.provider.id == owner);
        } else if let Some(lease) = ownership.as_ref().filter(|_| pinned.is_none()) {
            candidates = if attempts > 0 || !hard_denied.is_empty() {
                lease.candidates_after_failure(candidates, &requirement)
            } else {
                lease.candidates_for(candidates, &requirement)
            };
        } else if g.client_id() == super::ClientId::Codex
            && !manual
            && pinned.is_none()
            && !auxiliary
        {
            candidates = super::compaction::unidentified_candidates(candidates);
        }
        if attempts == 0 && candidates.len() == 1 {
            let owner = &candidates[0];
            let source = g.0.admission.capacity_source(&owner.provider.id);
            let health = owner.health_for(&requirement);
            if established_capacity_owner.as_deref() == Some(&owner.provider.id)
                && super::routing::capacity_cooling(&health)
            {
                budget.protect_capacity();
                failover.wait(super::failover::WaitKind::CapacityRetry, health.retry_in);
                let cause = g.capacity_cause(&owner.provider.id, current_model.as_deref(), &health);
                let exhausted = while_connecting(
                    client,
                    g.wait_capacity_checked(
                        &[source],
                        Duration::from_secs(health.retry_in),
                        cfg.max_waiting,
                        true,
                        Some(owner),
                        cause.as_ref(),
                    ),
                )
                .await?
                .map_err(rejected)?;
                if exhausted {
                    hard_denied.insert((owner.provider.id.clone(), owner.provider.version.clone()));
                    failover.failure(super::failover::Failure::AccountRejected);
                    last_error = cause.map(Into::into);
                }
                ids = g.routing_ids(pinned.as_deref());
                ids.retain(|id| {
                    g.route(id)
                        .is_some_and(|r| !hard_denied.contains(&(id.clone(), r.provider.version)))
                });
                routes = ids
                    .iter()
                    .filter_map(|id| {
                        g.route(id)
                            .filter(|r| r.accepts_protocol(uri.path()))
                            .map(|r| (id.clone(), r))
                    })
                    .collect();
                continue;
            }
        }
        let stage = if unknown_affinity {
            super::failover::Stage::UnknownOwner
        } else if pinned.is_some() {
            super::failover::Stage::CursorPinned
        } else if attempts > 0 || !hard_denied.is_empty() {
            super::failover::Stage::FailureFallback
        } else if ownership.as_ref().is_some_and(|l| l.handoff) {
            super::failover::Stage::ProactiveHandoff
        } else {
            super::failover::Stage::Initial
        };
        let observed: Vec<_> = g
            .routing_ids(pinned.as_deref())
            .iter()
            .filter_map(|id| g.route(id))
            .filter(|r| ids.contains(&r.provider.id) || !r.accepts_protocol(uri.path()))
            .collect();
        failover.candidates(
            g,
            stage,
            &observed,
            &candidates,
            &requirement,
            uri.path(),
            &budget,
            manual,
        );
        if matches_model
            && !candidates
                .iter()
                .any(|r| requirement.allows(r.provider.allowed_models.as_deref()))
        {
            if attempts > 0 {
                ids.clear();
                continue;
            }
            failover.finish(super::failover::Exit::NoEligibleProvider);
            g.record_policy_rejection(
                current_model.as_deref(),
                true,
                super::compaction::rejection_reason(conversation.as_deref(), first_hints.as_ref()),
            );
            return Err((1008, code::NO_ELIGIBLE_PROVIDER));
        }
        if g.client_id() == super::ClientId::Codex && !manual && pinned.is_none() && !auxiliary {
            let (intake, owner) = ownership
                .as_ref()
                .map_or((super::compaction::Intake::Ordinary, None), |lease| {
                    lease.admission_policy()
                });
            budget.set_intake(intake, owner);
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
            &mut failover,
        )
        .await
        {
            Ok(admission) => admission,
            Err((code, reason))
                if capacity_pending
                    && ((code == 1013 && reason != "RPM ledger unavailable")
                        || (code == 1008 && reason == code::NO_ELIGIBLE_PROVIDER)) =>
            {
                ids.clear();
                continue;
            }
            Err(error) => {
                if let Some(payload) = last_model_payload.take() {
                    client.send(Frame::text(payload)).await?;
                    return Err((1008, code::MODEL_UNAVAILABLE));
                }
                return Err(error);
            }
        };
        if !admit_ownership(
            g,
            ownership.as_deref(),
            &admission.route,
            &requirement,
            pinned.is_some(),
        )? {
            admission.permits.neutral(cfg);
            if g.0.compaction.error().is_some() {
                return Err((1013, code::CONVERSATION_OWNERSHIP));
            }
            continue;
        }
        admission.route = g
            .endpoint_route(
                admission.route.clone(),
                conversation.as_deref(),
                first_hints
                    .as_ref()
                    .and_then(|h| h.previous_response_id.as_deref()),
                ownership
                    .as_ref()
                    .is_some_and(|lease| lease.handoff || lease.establishing())
                    || first_hints.as_ref().is_some_and(|h| h.first_turn),
            )
            .await
            .map_err(|_| (1013, code::ENDPOINT_AFFINITY))?;
        if let Err(reason) = admission.commit_rpm() {
            return Err(rejected(reason));
        }
        ids.retain(|id| id != &admission.route.provider.id);
        attempts = attempts.saturating_add(1);
        ordinary_attempts = ordinary_attempts.saturating_add(1);
        if unknown_affinity {
            pinned = Some(admission.route.provider.id.clone());
            budget.pin_provider();
        }
        let rerouted = previous_provider
            .as_deref()
            .is_some_and(|id| id != admission.route.provider.id);
        previous_provider = Some(admission.route.provider.id.clone());
        let mut attempt_protocol = super::protocol::Protocol::new(true);
        attempt_protocol.attach_wait_budget(request_budget);
        attempt_protocol.attach_rpm(admission.take_completion());
        attempt_protocol.diagnostics(g, &admission.route.provider.id, current_model.as_deref());
        attempt_protocol.attach_ownership(ownership.clone(), &admission.route.provider.id);
        attempt_protocol.attach_routing_log(failover.routing());
        attempt_protocol.attach_thread(budget.thread.clone());
        attempt_protocol.request_headers(&headers);
        attempt_protocol.business_request(g, &admission.route.provider.id, &headers);
        attempt_protocol.request_received(turn_received_at);
        attempt_protocol
            .request_speed(value(&first).and_then(|v| crate::usage::model::speed(&v["speed"])));
        if let Some(trace) = &usage_trace {
            attempt_protocol.attach_usage(trace.attempt(
                &admission.route.provider.id,
                true,
                "websocket",
            ));
        }
        attempt_protocol.context(
            conversation.clone(),
            (!manual).then(|| {
                admission
                    .handoff_wait
                    .or(
                        (new_thread_model_denied && !admission.route.provider.take_new_threads)
                            .then_some(crate::usage::model::RoutingDecision::NewThreadModelDenied),
                    )
                    .unwrap_or_else(|| {
                        super::compaction::decision(
                            conversation.as_deref(),
                            first_hints.as_ref(),
                            pinned.is_some() || unknown_affinity,
                            ownership.as_deref(),
                            &admission.route.provider.id,
                        )
                    })
            }),
        );
        if !manual {
            attempt_protocol.routing_evidence(first_hints.as_ref());
        }
        attempt_protocol.watch_lifecycle(
            g.request_state()
                .map_err(|_| (1011, code::STORE))?
                .shutdown
                .as_ref()
                .map(|s| s.subscribe()),
            Some(client.closed.clone()),
        );
        let result = while_connecting(client, async {
            upstream_native(&admission.route, &uri, &headers, cfg)
                .await
                .map(Upstream::Native)
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
                attempt_protocol.websocket_status(101);
                if let Upstream::Native(peer) = &upstream {
                    attempt_protocol.response_headers(&peer.response_identifiers);
                }
                break (upstream, admission, attempt_protocol);
            }
            Err(failure) => {
                failover.failure(failure.classification());
                attempt_protocol.finish(
                    failure.status,
                    if failure.transport_failure() {
                        code::NETWORK
                    } else {
                        code::HTTP
                    },
                );
                if failure.context_incompatible {
                    last_error = Some(failover.report_failure(Some(&attempt_protocol), || {
                        record_failure(
                            &mut admission.permits,
                            g,
                            &admission.route,
                            current_model.as_deref(),
                            &failure,
                            super::routing::fallback_action(
                                g,
                                &admission.route.provider.id,
                                ids.iter()
                                    .filter_map(|id| routes.get(id).cloned())
                                    .filter(|r| {
                                        !failure.context_incompatible
                                            || ownership
                                                .as_ref()
                                                .and_then(|lease| lease.owner.as_deref())
                                                == Some(r.provider.id.as_str())
                                    })
                                    .collect(),
                                &requirement,
                                uri.path(),
                                ownership.as_deref(),
                                g.client_id() == super::ClientId::Codex && !auxiliary,
                                &budget,
                                !manual
                                    && pinned.is_none()
                                    && !unknown_affinity
                                    && ordinary_attempts <= cfg.max_retries,
                            ),
                            attempts,
                        )
                    }));
                    admission.permits.neutral(cfg);
                    if let Some(owner) = ownership
                        .as_ref()
                        .filter(|lease| lease.handoff)
                        .and_then(|lease| lease.owner.as_ref())
                        .filter(|id| ids.contains(id))
                    {
                        retry_owner = Some(owner.clone());
                        continue;
                    }
                    if let Some(payload) = failure.model_payload {
                        client.send(Frame::text(payload)).await?;
                    }
                    return Err((1008, code::COMPACTION_CONTEXT_UNSUPPORTED));
                }
                if !failure.model_unavailable {
                    last_model_payload = None;
                }
                if failure.unsupported {
                    unsupported_seen = true;
                    last_error = Some(failover.report_failure(Some(&attempt_protocol), || {
                        record_failure(
                            &mut admission.permits,
                            g,
                            &admission.route,
                            current_model.as_deref(),
                            &failure,
                            super::routing::fallback_action(
                                g,
                                &admission.route.provider.id,
                                ids.iter()
                                    .filter_map(|id| routes.get(id).cloned())
                                    .filter(|r| {
                                        !failure.context_incompatible
                                            || ownership
                                                .as_ref()
                                                .and_then(|lease| lease.owner.as_deref())
                                                == Some(r.provider.id.as_str())
                                    })
                                    .collect(),
                                &requirement,
                                uri.path(),
                                ownership.as_deref(),
                                g.client_id() == super::ClientId::Codex && !auxiliary,
                                &budget,
                                !manual
                                    && pinned.is_none()
                                    && !unknown_affinity
                                    && ordinary_attempts <= cfg.max_retries,
                            ),
                            attempts,
                        )
                    }));
                    admission.permits.neutral(cfg);
                    continue;
                }
                non_unsupported_failure = true;
                if failure.hard_rejection() {
                    hard_denied.insert((
                        admission.route.provider.id.clone(),
                        admission.route.provider.version.clone(),
                    ));
                }
                if failure.capacity {
                    ordinary_attempts = ordinary_attempts.saturating_sub(1);
                    capacity_protected = true;
                    budget.protect_capacity();
                }
                let retryable =
                    failure.model_unavailable || failure.capacity || failure.retryable();
                let wait_same = failure.capacity
                    && established_capacity_owner.as_deref() == Some(&admission.route.provider.id);
                let retry_delay = if admission.route.client_id == super::ClientId::Codex
                    && (failure.status.is_some_and(circuit::is_server_error)
                        || matches!(
                            failure.details.local_code.as_deref(),
                            Some(
                                code::CONNECTION_FAILED
                                    | code::CONNECT_TIMEOUT
                                    | code::WS_HANDSHAKE_TIMEOUT
                            )
                        ))
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
                failure.details.wait_seconds = if wait_same {
                    Some(forward::capacity_delay(cfg, failure.retry).as_secs())
                } else {
                    retry_delay.map(|delay| delay.as_secs())
                };
                last_error = Some(failover.report_failure(Some(&attempt_protocol), || {
                    record_failure(
                        &mut admission.permits,
                        g,
                        &admission.route,
                        current_model.as_deref(),
                        &failure,
                        if retry_delay.is_some() || wait_same {
                            crate::events::Action::RetryingSame
                        } else if retryable
                            && super::routing::fallback_action(
                                g,
                                &admission.route.provider.id,
                                ids.iter()
                                    .filter_map(|id| routes.get(id).cloned())
                                    .filter(|r| {
                                        !failure.context_incompatible
                                            || ownership
                                                .as_ref()
                                                .and_then(|lease| lease.owner.as_deref())
                                                == Some(r.provider.id.as_str())
                                    })
                                    .collect(),
                                &requirement,
                                uri.path(),
                                ownership.as_deref(),
                                g.client_id() == super::ClientId::Codex && !auxiliary,
                                &budget,
                                !manual
                                    && pinned.is_none()
                                    && !unknown_affinity
                                    && ordinary_attempts <= cfg.max_retries,
                            ) == crate::events::Action::TryingNext
                        {
                            crate::events::Action::TryingNext
                        } else if failure.capacity {
                            crate::events::Action::Waiting
                        } else {
                            crate::events::Action::Returned
                        },
                        attempts,
                    )
                }));
                if failure.model_unavailable {
                    admission.permits.neutral(cfg);
                    last_model_payload = failure.model_payload;
                    continue;
                }
                last_model_payload = None;
                if failure.hard_rejection() {
                    admission.permits.failure(cfg, failure.retry);
                } else if failure.capacity {
                    admission.permits.capacity_limited(cfg, failure.retry);
                } else if failure.status == Some(429) {
                    admission.permits.rate_limited(cfg, failure.retry);
                } else if retryable {
                    admission.permits.failure(cfg, failure.retry);
                } else {
                    admission.permits.neutral(cfg);
                }

                if failure.capacity {
                    failure.retry = failure.retry.max(Some(Duration::from_secs(
                        admission.route.health_for(&requirement).retry_in,
                    )));
                }
                if !retryable {
                    failover.finish(super::failover::Exit::PermanentRejection);
                    return Err((1008, "upstream rejected websocket handshake"));
                }
                if let Some(delay) = retry_delay {
                    failover.wait(super::failover::WaitKind::TransientRetry, delay.as_secs());
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
                    if g.routing_ids(pinned.as_deref()).contains(&provider_id)
                        && routes
                            .get(&provider_id)
                            .is_some_and(|route| route.health_for(&requirement).available)
                    {
                        ids.insert(0, provider_id.clone());
                        retry_owner = Some(provider_id);
                    }
                    continue;
                }
                if failure.capacity {
                    if wait_same {
                        capacity_wait_owner = Some(admission.route.provider.id.clone());
                        ids.clear();
                    }
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
    // A connection established only for a model/cursor detour does not gain
    // authority to reopen arbitrary subsequent turns on a different provider.
    let connection_request_only = ownership.as_ref().is_some_and(|lease| lease.request_only());
    let mut thread_request = budget.thread.take();
    let mut turn = Some(admission);
    let mut replay_frame = first.clone();
    let mut retry_pending = false;
    let mut retry_immediately = false;
    let mut retry_fallback = false;
    let mut owner_capacity_wait: Option<CapacitySource> = None;
    let mut fallback_capacity = false;
    let mut turn_failed = std::collections::HashSet::new();
    let mut rejected_frame: Option<Frame> = None;
    let mut tail_deadline = None;
    let mut previous_usage: Option<(super::protocol::Protocol, tokio::time::Instant)> = None;
    let mut protocol = Some(initial_protocol);
    let mut pending: Option<Frame> = None;
    let mut pending_received_at = turn_received_at;
    let mut pending_started = tokio::time::Instant::now();
    let mut confirmed_model: Option<String> = None;
    let mut received = false;
    let mut terminal_error = false;
    let mut receive_started = tokio::time::Instant::now();
    if !admit_ownership(
        g,
        ownership.as_deref(),
        &route,
        &requirement,
        pinned.is_some(),
    )? {
        return Err((1013, code::CONVERSATION_OWNERSHIP));
    }
    if let Upstream::Native(peer) = &mut upstream {
        if let Err(e) = peer.send(first).await {
            retry_pending = disconnect_turn(
                g,
                &route,
                cfg,
                current_model.as_deref(),
                &mut turn,
                &mut protocol,
                peer.activity
                    .read_at()
                    .is_some_and(|at| at >= receive_started),
                attempts,
                ordinary_attempts,
                !unknown_affinity,
                None,
                crate::events::Phase::WsSend,
                code::NETWORK,
            );
            if !retry_pending {
                return Err(e);
            }
            upstream = Upstream::Disconnected;
        }
    }
    // Upload progress has its own deadline. Start response waiting only after
    // the first frame has been handed to the upstream socket.
    let mut deadline = tokio::time::Instant::now() + Duration::from_secs(cfg.first_byte_seconds);
    'turns: loop {
        // Do not let a queued create/retry or continuously ready upstream starve
        // an already-observed downstream close. The tail never starts a turn.
        if *client.closed.borrow() {
            if let Some(mut admission) = turn.take() {
                admission.permits.neutral(cfg);
            }
            cancelled_usage_tail(&mut upstream, &mut protocol, &client.activity, false).await;
            return Ok(());
        }
        if retry_pending {
            let requirement = Requirement::model(current_model.as_deref());
            let hints = value(&replay_frame)
                .and_then(|v| super::replay::RequestHints::from_value(v, false).ok());
            let movable = retry_fallback
                && owner_capacity_wait.is_none()
                && !manual
                && !unknown_affinity
                && hints
                    .as_ref()
                    .is_some_and(|h| h.previous_response_id.is_none());
            let candidates_for = |failed: &std::collections::HashSet<String>| {
                let candidates: Vec<_> = g
                    .routing_ids(None)
                    .iter()
                    .filter_map(|id| g.route(id))
                    .filter(|r| {
                        !failed.contains(&r.provider.id)
                            && !hard_denied
                                .contains(&(r.provider.id.clone(), r.provider.version.clone()))
                            && r.accepts_protocol(uri.path())
                            && requirement.allows(r.provider.allowed_models.as_deref())
                    })
                    .collect();
                if let Some(lease) = &ownership {
                    lease.candidates_after_failure(candidates, &requirement)
                } else if g.client_id() == super::ClientId::Codex {
                    super::compaction::unidentified_candidates(candidates)
                } else {
                    candidates
                }
            };
            let mut retry_routes = if movable {
                candidates_for(&turn_failed)
            } else if owner_capacity_wait.is_some() {
                g.route(&route.provider.id)
                    .filter(|r| {
                        r.provider.queued
                            && !r.provider.search_only
                            && requirement.allows(r.provider.allowed_models.as_deref())
                    })
                    .into_iter()
                    .collect()
            } else {
                vec![route.clone()]
            };
            let alternative = movable && !retry_routes.is_empty();
            if movable && retry_routes.is_empty() && (retry_capacity || fallback_capacity) {
                turn_failed.clear();
                retry_routes = candidates_for(&turn_failed);
            }
            if retry_fallback
                && (retry_routes.is_empty()
                    || (!movable
                        && hard_denied.contains(&(
                            route.provider.id.clone(),
                            route.provider.version.clone(),
                        )))
                    || ordinary_attempts > cfg.max_retries)
            {
                failover.finish(if ordinary_attempts > cfg.max_retries {
                    super::failover::Exit::AttemptsExhausted
                } else {
                    super::failover::Exit::NoEligibleProvider
                });
                if let Some(frame) = rejected_frame.take() {
                    client.send(frame).await?;
                }
                return Err((1013, "all eligible providers failed"));
            }
            let delay = if retry_immediately || retry_capacity {
                0
            } else {
                cfg.websocket_retry_seconds
            }
            .max(
                route
                    .health_for(&Requirement::model(current_model.as_deref()))
                    .retry_in,
            );
            retry_immediately = false;
            if !alternative {
                failover.wait(
                    if retry_capacity {
                        super::failover::WaitKind::CapacityRetry
                    } else {
                        super::failover::WaitKind::TransientRetry
                    },
                    delay,
                );
            }
            let waited = if alternative {
                Ok(Ok(false))
            } else {
                while_connecting(client, async {
                    if let Some(source) = &owner_capacity_wait {
                        g.wait_capacity_checked(
                            std::slice::from_ref(source),
                            Duration::from_secs(delay),
                            cfg.max_waiting,
                            true,
                            Some(&route),
                            last_error.as_deref(),
                        )
                        .await
                    } else if retry_capacity || fallback_capacity {
                        let source = g.0.admission.capacity_source(&route.provider.id);
                        g.wait_capacity_checked(
                            &[source],
                            Duration::from_secs(delay),
                            cfg.max_waiting,
                            false,
                            Some(&route),
                            last_error.as_deref(),
                        )
                        .await
                    } else {
                        g.0.admission
                            .wait_websocket(
                                &route.provider.id,
                                Duration::from_secs(delay),
                                cfg.max_waiting,
                            )
                            .await
                            .map(|()| false)
                    }
                })
                .await
            };
            let exhausted = match waited {
                Err(TURN_CANCELLED) => {
                    failover.finish(super::failover::Exit::Cancelled);
                    retry_pending = false;
                    thread_request = None;
                    protocol = None;
                    usage_trace = None;
                    ownership = None;
                    failover.detach();
                    request_budget.complete(request_budget.deadline());
                    cancellation(client).await?;
                    continue;
                }
                Err(error) => return Err(error),
                Ok(result) => result.map_err(rejected)?,
            };
            if exhausted {
                hard_denied.insert((route.provider.id.clone(), route.provider.version.clone()));
                owner_capacity_wait = None;
                retry_capacity = false;
                retry_fallback = true;
                retry_immediately = true;
                failover.failure(super::failover::Failure::AccountRejected);
                continue;
            }
            if owner_capacity_wait.is_some() {
                retry_routes = g
                    .route(&route.provider.id)
                    .filter(|r| {
                        r.provider.queued
                            && !r.provider.search_only
                            && requirement.allows(r.provider.allowed_models.as_deref())
                    })
                    .into_iter()
                    .collect();
                if retry_routes.is_empty() {
                    if let Some(frame) = rejected_frame.take() {
                        client.send(frame).await?;
                    }
                    return Err((1013, "thread owner is no longer available"));
                }
            }
            current_model = turn_model(
                g,
                &replay_frame,
                confirmed_model.as_deref(),
                Some(&route.provider.id),
            )?;
            let mut budget = Budget::new(cfg.queue_seconds);
            budget.thread = thread_request.clone();
            budget.handoff_owner = ownership
                .as_ref()
                .filter(|lease| lease.handoff)
                .and_then(|lease| lease.owner.clone());
            if movable {
                budget.handoff_owner = None;
                if let Some(lease) = &ownership {
                    let (intake, owner) = lease.admission_policy();
                    budget.set_intake(intake, owner);
                }
            } else {
                budget.pin_provider();
            }
            if capacity_protected {
                budget.protect_capacity();
            }
            let observed: Vec<_> = g
                .routing_ids(pinned.as_deref())
                .iter()
                .filter_map(|id| g.route(id))
                .collect();
            failover.candidates(
                g,
                if movable {
                    super::failover::Stage::FailureFallback
                } else if unknown_affinity {
                    super::failover::Stage::UnknownOwner
                } else {
                    super::failover::Stage::CursorPinned
                },
                &observed,
                &retry_routes,
                &requirement,
                uri.path(),
                &budget,
                manual,
            );
            let acquired = take_slot(
                g,
                client,
                &retry_routes,
                manual,
                cfg,
                &mut budget,
                &Requirement::model(current_model.as_deref()),
                false,
                &mut failover,
            )
            .await;
            let mut admission = match acquired {
                Err(TURN_CANCELLED) => {
                    failover.finish(super::failover::Exit::Cancelled);
                    retry_pending = false;
                    thread_request = None;
                    protocol = None;
                    usage_trace = None;
                    ownership = None;
                    failover.detach();
                    request_budget.complete(request_budget.deadline());
                    cancellation(client).await?;
                    continue;
                }
                result => result?,
            };
            if !admit_ownership(
                g,
                ownership.as_deref(),
                &admission.route,
                &requirement,
                !movable,
            )? {
                admission.permits.neutral(cfg);
                continue;
            }
            let previous = route.provider.id.clone();
            route = g
                .endpoint_route(
                    admission.route.clone(),
                    conversation.as_deref(),
                    None,
                    false,
                )
                .await
                .map_err(|_| (1013, code::ENDPOINT_AFFINITY))?;
            admission.route = route.clone();
            if previous != route.provider.id {
                g.record(
                    Some(&route.provider.id),
                    current_model.as_deref(),
                    crate::events::Reason::Failover,
                    crate::events::Action::Routed,
                    None,
                    None,
                );
            }
            admission.commit_rpm().map_err(rejected)?;
            attempts = attempts.saturating_add(1);
            ordinary_attempts = ordinary_attempts.saturating_add(1);
            let mut next = super::protocol::Protocol::new(true);
            next.attach_wait_budget(request_budget);
            next.attach_rpm(admission.take_completion());
            next.diagnostics(g, &route.provider.id, current_model.as_deref());
            next.attach_ownership(ownership.clone(), &route.provider.id);
            next.attach_routing_log(failover.routing());
            next.attach_thread(thread_request.clone());
            next.request_headers(&headers);
            next.business_request(g, &route.provider.id, &headers);
            next.request_received(turn_received_at);
            next.request_speed(
                value(&replay_frame).and_then(|v| crate::usage::model::speed(&v["speed"])),
            );
            if let Some(trace) = &usage_trace {
                next.attach_usage(trace.attempt(&route.provider.id, true, "websocket"));
            }
            let hints = value(&replay_frame)
                .and_then(|v| super::replay::RequestHints::from_value(v, false).ok());
            next.context(
                conversation.clone(),
                (!manual).then(|| {
                    super::compaction::decision(
                        conversation.as_deref(),
                        hints.as_ref(),
                        pinned.is_some() || unknown_affinity,
                        ownership.as_deref(),
                        &route.provider.id,
                    )
                }),
            );
            if !manual {
                next.routing_evidence(hints.as_ref());
            }
            next.watch_lifecycle(
                g.request_state()
                    .map_err(|_| (1011, code::STORE))?
                    .shutdown
                    .as_ref()
                    .map(|s| s.subscribe()),
                Some(client.closed.clone()),
            );
            protocol = Some(next);
            turn = Some(admission);
            let opened = while_connecting(client, async {
                upstream_native(&route, &uri, &headers, cfg)
                    .await
                    .map(Upstream::Native)
            })
            .await;
            match opened {
                Err(TURN_CANCELLED) => {
                    failover.finish(super::failover::Exit::Cancelled);
                    turn = None;
                    if let Some(u) = &mut protocol {
                        u.finish(None, code::CANCELLED);
                    }
                    protocol = None;
                    usage_trace = None;
                    ownership = None;
                    failover.detach();
                    retry_pending = false;
                    request_budget.complete(request_budget.deadline());
                    cancellation(client).await?;
                    continue;
                }
                Err(error) => return Err(error),
                Ok(Err(failure)) => {
                    failover.failure(failure.classification());
                    let mut admission = turn.take().unwrap();
                    let wait_same = failure.capacity
                        && established_capacity_owner.as_deref() == Some(&route.provider.id);
                    owner_capacity_wait = wait_same.then(|| CapacitySource {
                        provider_id: route.provider.id.clone(),
                        reset_generation: admission.reset_generation,
                    });
                    let can_fail_over = !wait_same
                        && retry_fallback
                        && !manual
                        && !unknown_affinity
                        && hints
                            .as_ref()
                            .is_some_and(|h| h.previous_response_id.is_none());
                    let mut failure = failure;
                    if wait_same {
                        failure.details.wait_seconds =
                            Some(forward::capacity_delay(cfg, failure.retry).as_secs());
                    }
                    if let Some(u) = protocol.as_mut() {
                        u.finish(
                            failure.status,
                            if failure.transport_failure() {
                                code::NETWORK
                            } else {
                                code::HTTP
                            },
                        );
                    }
                    last_error = Some(failover.report_failure(protocol.as_ref(), || {
                        record_failure(
                            &mut admission.permits,
                            g,
                            &route,
                            current_model.as_deref(),
                            &failure,
                            if wait_same {
                                crate::events::Action::RetryingSame
                            } else if failure.capacity {
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
                        )
                    }));
                    drop(protocol.take());
                    if failure.capacity {
                        ordinary_attempts = ordinary_attempts.saturating_sub(1);
                        capacity_protected = true;
                    }
                    fallback_capacity |= failure.capacity;
                    retry_capacity = fallback_capacity;
                    if can_fail_over
                        && (failure.capacity
                            || failure.hard_rejection()
                            || failure.model_unavailable
                            || failure.retryable())
                    {
                        turn_failed.insert(route.provider.id.clone());
                        if failure.hard_rejection() {
                            hard_denied.insert((
                                route.provider.id.clone(),
                                route.provider.version.clone(),
                            ));
                        }
                        if failure.capacity {
                            admission.permits.capacity_limited(cfg, failure.retry);
                        } else if failure.model_unavailable {
                            admission.permits.neutral(cfg);
                        } else {
                            admission.permits.failure(cfg, failure.retry);
                        }
                        continue;
                    }
                    if failure.model_unavailable
                        || failure.unsupported
                        || failure.hard_rejection()
                        || (!failure.capacity && !failure.retryable())
                    {
                        if failure.hard_rejection() {
                            admission.permits.failure(cfg, failure.retry);
                        } else {
                            admission.permits.neutral(cfg);
                        }
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
                        u.websocket_status(101);
                        if let Upstream::Native(peer) = &opened {
                            u.response_headers(&peer.response_identifiers);
                        }
                    }
                    upstream = opened;
                }
            }
            retry_pending = false;
            retry_capacity = false;
            owner_capacity_wait = None;
            receive_started = tokio::time::Instant::now();
            if !admit_ownership(g, ownership.as_deref(), &route, &requirement, !movable)? {
                return Err((1013, code::CONVERSATION_OWNERSHIP));
            }
            if let Err(error) = send_to_upstream(&mut upstream, replay_frame.clone()).await {
                retry_pending = disconnect_turn(
                    g,
                    &route,
                    cfg,
                    current_model.as_deref(),
                    &mut turn,
                    &mut protocol,
                    response_data_started(&upstream, false, receive_started),
                    attempts,
                    ordinary_attempts,
                    !unknown_affinity,
                    None,
                    crate::events::Phase::WsSend,
                    code::NETWORK,
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
                request_budget.begin_at(pending_started);
                turn_received_at = pending_received_at;
                if let Some(previous) = protocol.take().filter(|p| p.succeeded()) {
                    previous_usage = Some((previous, tokio::time::Instant::now() + USAGE_TAIL));
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
                    let state = g.request_state().map_err(|_| (1011, code::STORE))?;
                    state.store.mode == "auto"
                };
                if g.client_id() == super::ClientId::Codex
                    && !manual
                    && enabled
                    && !hints.as_ref().is_some_and(|h| h.auxiliary)
                {
                    let queued = super::compaction::queued(g).map_err(|_| (1011, code::STORE))?;
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
                    let owner_model_mismatch = conversation.as_deref().is_some_and(|session| {
                        g.0.compaction.model_detour(session, &queued, &requirement)
                    });
                    let boundary = safe_window
                        && !owner_model_mismatch
                        && conversation.as_ref().is_some_and(|session| {
                            g.0.compaction.boundary_matches(
                                session,
                                hints.as_ref().and_then(|h| h.compacted_window.as_deref()),
                            )
                        });
                    let current_owner = conversation
                        .as_deref()
                        .and_then(|session| g.0.compaction.owner(session));
                    if boundary
                        && latest
                            .iter()
                            .take_while(|r| {
                                current_owner.as_deref() != Some(r.provider.id.as_str())
                            })
                            .any(|r| r.provider.handoff_after_compaction)
                    {
                        *next_turn = Some(frame);
                        return Err((1999, "compaction handoff"));
                    }
                    ownership = if let Some(session) =
                        conversation.clone().filter(|_| !latest.is_empty())
                    {
                        Some(
                            g.0.compaction
                                .prepare_request(
                                    session,
                                    hints.as_ref(),
                                    safe_window,
                                    &queued,
                                    &requirement,
                                    hints.as_ref().and_then(|h| {
                                        h.previous_response_id
                                            .as_ref()
                                            .map(|_| route.provider.id.as_str())
                                    }),
                                )
                                .ok_or((1013, code::CONVERSATION_OWNERSHIP))?,
                        )
                    } else {
                        None
                    };
                    // An initial generate:false warmup never owns a thread. The
                    // first real generation must still pass the shared policy.
                    if ownership.as_ref().is_some_and(|lease| {
                        lease.establishing()
                            && lease
                                .candidates_for(latest.clone(), &requirement)
                                .first()
                                .is_some_and(|r| r.provider.id != route.provider.id)
                    }) {
                        *next_turn = Some(frame);
                        return Err((1999, "compaction handoff"));
                    }
                    // A connection opened for a model detour is not permission
                    // to keep unrelated turns on that provider. Respect a real
                    // response cursor. A new, parsed round without a cursor can
                    // reopen upstream at the ledger's current owner before it
                    // has sent any bytes; an uninspectable turn stays rejected.
                    if !hints
                        .as_ref()
                        .is_some_and(|h| h.previous_response_id.is_some())
                        && ownership.as_ref().is_some_and(|lease| {
                            !lease
                                .candidates_for(latest.clone(), &requirement)
                                .iter()
                                .any(|candidate| candidate.provider.id == route.provider.id)
                        })
                    {
                        if !connection_request_only
                            && hints.is_some()
                            && current_owner
                                .as_deref()
                                .is_some_and(|owner| owner != route.provider.id)
                            && ownership
                                .as_ref()
                                .is_some_and(|lease| !lease.request_only())
                        {
                            *next_turn = Some(frame);
                            return Err((1999, "compaction handoff"));
                        }
                        return Err((1013, code::NO_ELIGIBLE_PROVIDER));
                    }
                } else {
                    ownership = None;
                }
                established_capacity_owner =
                    super::routing::capacity_owner(g.client_id(), manual, ownership.as_deref());
                // A new turn on a persistent socket observes the same cooldown
                // as a separately connected request from the established thread.
                let mut quota_denied = false;
                let mut quota_cause = None;
                loop {
                    let source = g.0.admission.capacity_source(&route.provider.id);
                    let health = route.health_for(&requirement);
                    if established_capacity_owner.as_deref() != Some(&route.provider.id)
                        || !super::routing::capacity_cooling(&health)
                    {
                        break;
                    }
                    let cause = g.capacity_cause(&route.provider.id, model.as_deref(), &health);
                    let waited = while_connecting(
                        client,
                        g.wait_capacity_checked(
                            &[source],
                            Duration::from_secs(health.retry_in.max(1)),
                            cfg.max_waiting,
                            true,
                            Some(&route),
                            cause.as_ref(),
                        ),
                    )
                    .await;
                    match waited {
                        Err(TURN_CANCELLED) => {
                            request_budget.complete(request_budget.deadline());
                            cancellation(client).await?;
                            continue 'turns;
                        }
                        Err(error) => return Err(error),
                        Ok(result) => {
                            if result.map_err(rejected)? {
                                quota_denied = true;
                                quota_cause = cause;
                                break;
                            }
                        }
                    }
                    let refreshed = g
                        .route(&route.provider.id)
                        .filter(|r| {
                            r.provider.queued
                                && !r.provider.search_only
                                && requirement.allows(r.provider.allowed_models.as_deref())
                        })
                        .ok_or((1013, "thread owner is no longer available"))?;
                    if refreshed.provider.version != route.provider.version {
                        upstream = Upstream::Disconnected;
                    }
                    route = refreshed;
                }
                tail_deadline = None;
                let mut budget = Budget::new(cfg.queue_seconds);
                budget.track(
                    &g.0.admission,
                    ownership.as_deref(),
                    cfg.thread_idle_release_seconds,
                );
                thread_request = budget.thread.clone();
                if let Some(lease) = &ownership {
                    let (intake, owner) = lease.admission_policy();
                    budget.set_intake(intake, owner);
                }
                usage_trace = g.usage_operation(
                    model.as_deref(),
                    if hints.as_ref().is_some_and(|h| h.compaction_trigger) {
                        crate::usage::model::Operation::Compaction
                    } else {
                        crate::usage::model::Operation::Model
                    },
                );
                failover = super::failover::Tracker::new(usage_trace.clone());
                if !manual && g.client_id() == super::ClientId::Codex {
                    failover.diagnostics(g);
                    if let Some(log) = failover.routing() {
                        log.pending(
                            &headers,
                            hints.as_ref(),
                            ownership.as_deref(),
                            hints
                                .as_ref()
                                .is_some_and(|h| h.previous_response_id.is_some()),
                        );
                    }
                }
                failover.candidates(
                    g,
                    if hints
                        .as_ref()
                        .is_some_and(|h| h.previous_response_id.is_some())
                    {
                        super::failover::Stage::CursorPinned
                    } else {
                        super::failover::Stage::Initial
                    },
                    std::slice::from_ref(&route),
                    std::slice::from_ref(&route),
                    &requirement,
                    uri.path(),
                    &budget,
                    manual,
                );
                if quota_denied {
                    failover.failure(super::failover::Failure::AccountRejected);
                    current_model = model;
                    attempts = 0;
                    ordinary_attempts = 0;
                    capacity_protected = false;
                    retry_pending = true;
                    retry_capacity = false;
                    retry_fallback = true;
                    retry_immediately = true;
                    owner_capacity_wait = None;
                    fallback_capacity = false;
                    turn_failed.clear();
                    hard_denied.clear();
                    hard_denied.insert((route.provider.id.clone(), route.provider.version.clone()));
                    rejected_frame = None;
                    terminal_error = false;
                    replay_frame = frame.clone();
                    last_error = quota_cause.map(Into::into);
                    protocol = None;
                    upstream = Upstream::Disconnected;
                    continue 'turns;
                }
                let acquired = take_slot(
                    g,
                    client,
                    std::slice::from_ref(&route),
                    manual,
                    cfg,
                    &mut budget,
                    &requirement,
                    false,
                    &mut failover,
                )
                .await;
                match acquired {
                    Ok(mut admission) => {
                        if !admit_ownership(
                            g,
                            ownership.as_deref(),
                            &admission.route,
                            &requirement,
                            hints
                                .as_ref()
                                .is_some_and(|h| h.previous_response_id.is_some()),
                        )? {
                            admission.permits.neutral(cfg);
                            *next_turn = Some(frame);
                            return Err((1999, "compaction handoff"));
                        }
                        if let Err(reason) = admission.commit_rpm() {
                            return Err(rejected(reason));
                        }
                        turn = Some(admission)
                    }
                    Err(TURN_CANCELLED) => {
                        request_budget.complete(request_budget.deadline());
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
                retry_fallback = false;
                owner_capacity_wait = None;
                fallback_capacity = false;
                turn_failed.clear();
                hard_denied.clear();
                rejected_frame = None;
                terminal_error = false;
                replay_frame = frame.clone();
                let mut next_protocol = super::protocol::Protocol::new(true);
                next_protocol.attach_wait_budget(request_budget);
                next_protocol.attach_rpm(turn.as_mut().and_then(|a| a.take_completion()));
                next_protocol.diagnostics(g, &route.provider.id, current_model.as_deref());
                next_protocol.attach_ownership(ownership.clone(), &route.provider.id);
                next_protocol.attach_routing_log(failover.routing());
                next_protocol.attach_thread(thread_request.clone());
                next_protocol.request_headers(&headers);
                next_protocol.business_request(g, &route.provider.id, &headers);
                next_protocol.request_received(turn_received_at);
                next_protocol.request_speed(hints.as_ref().and_then(|h| h.speed.clone()));
                if let Some(trace) = &usage_trace {
                    next_protocol.attach_usage(trace.attempt(
                        &route.provider.id,
                        true,
                        "websocket",
                    ));
                }
                next_protocol.context(
                    conversation.clone(),
                    (!manual).then(|| {
                        super::compaction::decision(
                            conversation.as_deref(),
                            hints.as_ref(),
                            hints
                                .as_ref()
                                .is_some_and(|h| h.previous_response_id.is_some()),
                            ownership.as_deref(),
                            &route.provider.id,
                        )
                    }),
                );
                if !manual {
                    next_protocol.routing_evidence(hints.as_ref());
                }
                next_protocol.watch_lifecycle(
                    g.request_state()
                        .map_err(|_| (1011, code::STORE))?
                        .shutdown
                        .as_ref()
                        .map(|s| s.subscribe()),
                    Some(client.closed.clone()),
                );
                next_protocol.websocket_status(101);
                protocol = Some(next_protocol);
                received = false;
                receive_started = tokio::time::Instant::now();
                {
                    if matches!(upstream, Upstream::Disconnected) {
                        match while_connecting(client, upstream_native(&route, &uri, &headers, cfg))
                            .await?
                        {
                            Ok(peer) => upstream = Upstream::Native(peer),
                            Err(failure) => {
                                failover.failure(failure.classification());
                                let wait_same = failure.capacity
                                    && established_capacity_owner.as_deref()
                                        == Some(&route.provider.id);
                                owner_capacity_wait = if wait_same {
                                    turn.as_ref().map(|admission| CapacitySource {
                                        provider_id: route.provider.id.clone(),
                                        reset_generation: admission.reset_generation,
                                    })
                                } else {
                                    None
                                };
                                let mut failure = failure;
                                if wait_same {
                                    failure.details.wait_seconds =
                                        Some(forward::capacity_delay(cfg, failure.retry).as_secs());
                                }
                                let can_fail_over = !manual
                                    && !wait_same
                                    && !unknown_affinity
                                    && hints
                                        .as_ref()
                                        .is_some_and(|h| h.previous_response_id.is_none())
                                    && (failure.capacity
                                        || failure.hard_rejection()
                                        || failure.model_unavailable
                                        || failure.retryable());
                                if can_fail_over {
                                    turn_failed.insert(route.provider.id.clone());
                                    if failure.hard_rejection() {
                                        hard_denied.insert((
                                            route.provider.id.clone(),
                                            route.provider.version.clone(),
                                        ));
                                    }
                                }
                                if let Some(mut admission) = turn.take() {
                                    if let Some(u) = protocol.as_mut() {
                                        u.finish(
                                            failure.status,
                                            if failure.transport_failure() {
                                                code::NETWORK
                                            } else {
                                                code::HTTP
                                            },
                                        );
                                    }
                                    last_error = Some(failover.report_failure(protocol.as_ref(), || record_failure(
                                        &mut admission.permits,
                                        g,
                                        &route,
                                        current_model.as_deref(),
                                        &failure,
                                        if wait_same {
                                            crate::events::Action::RetryingSame
                                        } else if can_fail_over {
                                            super::routing::fallback_action(
                                            g, &route.provider.id,
                                            g.routing_ids(None).iter().filter_map(|id| g.route(id)).filter(|r| !turn_failed.contains(&r.provider.id) && !hard_denied.contains(&(r.provider.id.clone(), r.provider.version.clone()))).collect(),
                                            &Requirement::model(current_model.as_deref()), uri.path(), ownership.as_deref(),
                                            g.client_id() == super::ClientId::Codex,
                                            &{ let mut diagnostic_budget = Budget::new(cfg.queue_seconds); diagnostic_budget.thread = thread_request.clone(); diagnostic_budget },
                                            !manual && !unknown_affinity && pinned.is_none()
                                                && value(&replay_frame).and_then(|v| super::replay::RequestHints::from_value(v, false).ok()).is_some_and(|h| h.previous_response_id.is_none())
                                                && (failure.capacity || ordinary_attempts <= cfg.max_retries),
                                        )
                                        } else if failure.capacity {
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
                                    )));
                                    drop(protocol.take());
                                    if !can_fail_over
                                        && (failure.model_unavailable
                                            || failure.unsupported
                                            || failure.hard_rejection())
                                    {
                                        if failure.hard_rejection() {
                                            admission.permits.failure(cfg, failure.retry);
                                        } else {
                                            admission.permits.neutral(cfg);
                                        }
                                        return Err((1008, "upstream rejected websocket"));
                                    }
                                    if failure.hard_rejection() {
                                        admission.permits.failure(cfg, failure.retry);
                                    } else if failure.model_unavailable {
                                        admission.permits.neutral(cfg);
                                    } else if failure.capacity {
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
                                fallback_capacity |= failure.capacity;
                                retry_capacity = fallback_capacity;
                                retry_fallback = can_fail_over || wait_same;
                                retry_pending = ordinary_attempts <= cfg.max_retries;
                                if !retry_pending {
                                    return Err((1013, "websocket reconnect retries exhausted"));
                                }
                                continue;
                            }
                        }
                    }
                    if let (Some(u), Upstream::Native(peer)) = (&mut protocol, &upstream) {
                        u.response_headers(&peer.response_identifiers);
                    }
                    if !admit_ownership(
                        g,
                        ownership.as_deref(),
                        &route,
                        &requirement,
                        hints
                            .as_ref()
                            .is_some_and(|h| h.previous_response_id.is_some()),
                    )? {
                        return Err((1013, code::CONVERSATION_OWNERSHIP));
                    }
                    if let Err(e) = send_to_upstream(&mut upstream, frame).await {
                        retry_pending = disconnect_turn(
                            g,
                            &route,
                            cfg,
                            current_model.as_deref(),
                            &mut turn,
                            &mut protocol,
                            response_data_started(&upstream, false, receive_started),
                            attempts,
                            ordinary_attempts,
                            !unknown_affinity,
                            None,
                            crate::events::Phase::WsSend,
                            code::NETWORK,
                        );
                        upstream = Upstream::Disconnected;
                        if !retry_pending {
                            return Err(e);
                        }
                        continue;
                    }
                }
                deadline =
                    tokio::time::Instant::now() + Duration::from_secs(cfg.first_byte_seconds);
            }
        }
        tokio::select! {
            biased;
            _ = tokio::time::sleep_until(pending_started + Duration::from_secs(cfg.retry_budget_seconds)), if pending.is_some() => {
                return Err((1013, code::FIRST_BYTE_TIMEOUT));
            },
            _ = client.closed.changed() => {
                if let Some(mut admission) = turn.take() { admission.permits.neutral(cfg); }
                cancelled_usage_tail(&mut upstream, &mut protocol, &client.activity, false).await;
                return Ok(());
            },
            _ = async { if let Some((_, at)) = &previous_usage { tokio::time::sleep_until(*at).await } else { std::future::pending::<()>().await } } => { previous_usage = None; },
            _ = async { if let Some(at) = tail_deadline { tokio::time::sleep_until(at).await } else { std::future::pending::<()>().await } }, if turn.is_none() => {
                protocol = None;
                usage_trace = None; ownership = None; failover.detach();
                tail_deadline = None;
            },
            event = next_upstream(&mut upstream), if turn.is_some() || protocol.is_some() || matches!(upstream, Upstream::Native(_)) => {
                let disconnected = match &event { None => true, Some(Err(_)) => true, Some(Ok(frame)) => frame.opcode() == OpCode::Close };
                if disconnected {
                    let close = event.as_ref().and_then(|v| v.as_ref().ok()).filter(|f| f.opcode() == OpCode::Close);
                    // Only a successfully settled/cancelled turn may treat this
                    // as an idle disconnect. An error already sent downstream
                    // must retain the upstream close without a second failure.
                    if turn.is_none() && !terminal_error && route.client_id == super::ClientId::Codex {
                        upstream = Upstream::Disconnected;
                        protocol = None;
                        usage_trace = None; ownership = None; failover.detach();
                        tail_deadline = None;
                        continue;
                    }
                    let native = matches!(upstream, Upstream::Native(_));
                    let response_started = response_data_started(&upstream, received, receive_started);
                    let unfinished_native_turn = native && turn.is_some() && !response_started && route.client_id == super::ClientId::Codex;
                    retry_pending = disconnect_turn(g, &route, cfg, current_model.as_deref(), &mut turn, &mut protocol, response_started, attempts, ordinary_attempts, native && !unknown_affinity, close, crate::events::Phase::WsReceive, code::STREAM_INTERRUPTED);
                    upstream = Upstream::Disconnected;
                    if retry_pending { continue; }
                    if unfinished_native_turn { return Err((1013, "websocket reconnect retries exhausted")); }
                    if let Some(Ok(frame)) = event { client.send(frame).await?; return Ok(()); }
                    return Err((1013, "upstream disconnected before completion"));
                }
                let frame = event.unwrap()?;
                if matches!(frame.opcode(), OpCode::Text | OpCode::Binary) {
                    if let Some(u) = &mut protocol { u.websocket_progress(frame.payload().len()); }
                }
                let mut settled_turn = None;
                if let Some(v) = value(&frame) {
                    let mut observed = super::protocol::Observation::default();
                    observed.value(&v);
                    let usage_only = matches!(v.get("type").and_then(|v| v.as_str()), Some("usage" | "usage.updated" | "response.usage" | "response.usage.updated"));
                    let belongs_to_previous = protocol.as_ref().and_then(|u| u.observation.response_id.as_ref()).map_or(usage_only, |id| Some(id) != observed.response_id.as_ref());
                    if let Some((previous, _)) = previous_usage.as_mut().filter(|(previous, _)| belongs_to_previous && observed.response_id.is_some() && previous.observation.response_id == observed.response_id) {
                        previous.value(&v);
                        let sending = tokio::time::Instant::now();
                        if let Err(error) = client.send(frame).await {
                            cancelled_usage_tail(&mut upstream, &mut protocol, &client.activity, true).await;
                            return Err(error);
                        }
                        deadline += sending.elapsed();
                        continue;
                    }
                }
                if turn.is_some() && !received && ownership.as_ref().is_some_and(|lease| lease.handoff && lease.owner.as_deref() != Some(&route.provider.id)) && value(&frame).is_some_and(|v| super::compaction::incompatible(&v)) {
                    if let Some(old) = ownership.as_ref().and_then(|lease| lease.owner.as_ref()).and_then(|id| g.route(id)).filter(|old| old.provider.queued && !old.provider.search_only && Requirement::model(current_model.as_deref()).allows(old.provider.allowed_models.as_deref())) {
                        let old = g.endpoint_route(old, conversation.as_deref(), None, false).await.map_err(|_| (1013, code::ENDPOINT_AFFINITY))?;
                        if ordinary_attempts <= cfg.max_retries {
                            if let Some(u) = protocol.as_mut() { if let Some(v) = value(&frame) { u.value(&v); } u.finish(Some(101), code::CLIENT_ERROR); }
                            if let Some(mut admission) = turn.take() {
                                let details = value(&frame).map(|v| super::failure_policy::details_value(&v, false)).unwrap_or_default();
                                let emit = || admission.permits.report(g, &route, current_model.as_deref(), crate::events::Reason::ProtocolError, crate::events::Action::TryingNext, Some(101), attempts, crate::events::Details { counted_failure: Some(false), ..details });
                                if let Some(u) = &protocol { u.report_failure(emit); } else { emit(); }
                                admission.permits.neutral(cfg);
                            }
                            drop(protocol.take());
                            route = old;
                            upstream = Upstream::Disconnected;
                            retry_pending = true;
                            retry_immediately = true;
                            retry_capacity = false;
                            continue;
                        }
                    }
                }
                // A structured rejection before the first output is replay-safe.
                // Re-evaluate the same failure queue used by HTTP, retaining the
                // compaction claim and the exact original response.create frame.
                if turn.is_some() && !received {
                    if let Some(mut failure) = first_event_failure(&frame).filter(|f| f.capacity || f.hard_rejection() || f.model_unavailable || f.details.error_basis == Some(crate::events::ErrorBasis::ServiceError)) {
                        failover.failure(failure.classification());
                        let wait_same = failure.capacity && established_capacity_owner.as_deref() == Some(&route.provider.id);
                        owner_capacity_wait = if wait_same { turn.as_ref().map(|admission| CapacitySource { provider_id: route.provider.id.clone(), reset_generation: admission.reset_generation }) } else { None };
                        if wait_same { failure.details.wait_seconds = Some(forward::capacity_delay(cfg, failure.retry).as_secs()); }
                        turn_failed.insert(route.provider.id.clone());
                        if failure.hard_rejection() { hard_denied.insert((route.provider.id.clone(), route.provider.version.clone())); }
                        if let Some(u) = protocol.as_mut() { if let Some(v) = value(&frame) { u.value(&v); } u.finish(Some(101), code::UPSTREAM_ERROR); }
                        if let Some(mut admission) = turn.take() {
                            last_error = Some(failover.report_failure(protocol.as_ref(), || record_failure(&mut admission.permits, g, &route, current_model.as_deref(), &failure, if wait_same { crate::events::Action::RetryingSame } else { super::routing::fallback_action(
                                            g, &route.provider.id,
                                            g.routing_ids(None).iter().filter_map(|id| g.route(id)).filter(|r| !turn_failed.contains(&r.provider.id) && !hard_denied.contains(&(r.provider.id.clone(), r.provider.version.clone()))).collect(),
                                            &Requirement::model(current_model.as_deref()), uri.path(), ownership.as_deref(),
                                            g.client_id() == super::ClientId::Codex,
                                            &{ let mut diagnostic_budget = Budget::new(cfg.queue_seconds); diagnostic_budget.thread = thread_request.clone(); diagnostic_budget },
                                            !manual && !unknown_affinity && pinned.is_none()
                                                && value(&replay_frame).and_then(|v| super::replay::RequestHints::from_value(v, false).ok()).is_some_and(|h| h.previous_response_id.is_none())
                                                && (failure.capacity || ordinary_attempts <= cfg.max_retries),
                                        ) }, attempts)));
                            if failure.capacity { admission.permits.capacity_limited(cfg, failure.retry); }
                            else if failure.model_unavailable { admission.permits.neutral(cfg); }
                            else { admission.permits.failure(cfg, failure.retry); }
                        }
                        drop(protocol.take());
                        if failure.capacity { ordinary_attempts = ordinary_attempts.saturating_sub(1); capacity_protected = true; }
                        rejected_frame = Some(frame);
                        fallback_capacity |= failure.capacity;
                        retry_capacity = fallback_capacity;
                        retry_fallback = true;
                        retry_pending = true;
                        upstream = Upstream::Disconnected;
                        continue;
                    }
                }
                if !frame.opcode().is_control() {
                    received = true;
                    deadline = tokio::time::Instant::now() + Duration::from_secs(cfg.idle_seconds);
                }
                if let Some(v) = value(&frame) {
                    if let Some(u)=&mut protocol { u.value(&v); }
                    if let Some(id) = v.pointer("/response/id").or_else(|| v.get("response_id")).and_then(|v| v.as_str()) { g.0.endpoints.remember(&route.provider, id); g.remember_model(id, &route.provider.id, current_model.as_deref()); }
                    let kind = v.get("type").and_then(|v| v.as_str()).unwrap_or("");
                    if matches!(kind, "response.created" | "response.completed" | "response.done") { confirmed_model = current_model.clone(); }
                    if protocol.as_ref().map_or_else(|| matches!(kind, "response.completed" | "response.done" | "response.failed" | "response.incomplete" | "response.cancelled" | "response.canceled" | "error"), |u| u.terminal().is_some()) {
                        request_budget.complete(request_budget.deadline());
                        if let Some(mut admission) = turn.take() {
                            if let Some(u)=protocol.as_mut() {
                                u.finish(Some(101), code::UPSTREAM_ERROR);
                                terminal_error = !matches!(u.terminal(), Some(super::protocol::Terminal::Success | super::protocol::Terminal::Limited | super::protocol::Terminal::Cancelled));
                                if route.client_id == super::ClientId::Codex && first_event_failure(&frame).is_some_and(|failure| failure.capacity) {
                                    if let Some(failure) = first_event_failure(&frame) { u.report_failure(|| record_failure(&mut admission.permits, g, &route, current_model.as_deref(), &failure, crate::events::Action::Returned, attempts)); }
                                    admission.permits.capacity_limited(cfg, None);
                                } else {
                                    super::health_settlement::observe_protocol(u, &mut admission.permits, cfg, g, &route, current_model.as_deref(), Some(101), attempts);
                                }

                            }
                            settled_turn = Some(admission);
                        }
                        if tail_deadline.is_none() { tail_deadline = Some(tokio::time::Instant::now() + USAGE_TAIL); }
                    }
                }
                let frame_bytes = frame.payload().len();
                let business = !frame.opcode().is_control();
                if business { failover.finish(super::failover::Exit::Forwarded); }
                let sending = tokio::time::Instant::now();
                if let Err(error) = client.send(frame).await {
                    if let Some(mut admission) = turn.take() { admission.permits.neutral(cfg); }
                    cancelled_usage_tail(&mut upstream, &mut protocol, &client.activity, true).await;
                    return Err(error);
                }
                // Do not charge time blocked by the downstream reader to the
                // upstream silence timer. Peer::send has its own progress limit.
                deadline += sending.elapsed();
                if let Some(u) = &mut protocol {
                    u.delivery().handed(frame_bytes, u.terminal_observed());
                    u.output_handed(business);
                    if u.delivered_success() {
                        if let Some(admission) = &mut settled_turn {
                            admission.slot.complete();
                            g.persist_success(&route.provider).await;
                        }
                    }
                    if turn.is_none() { u.attach_thread(None); thread_request = None; }
                }
            },
            frame = client.incoming.recv() => {
                let Some(frame) = frame else {
                    if let Some(mut admission) = turn.take() { admission.permits.neutral(cfg); }
                    cancelled_usage_tail(&mut upstream, &mut protocol, &client.activity, false).await;
                    return Ok(());
                };
                let closing = frame.opcode() == OpCode::Close;
                if frame.opcode() == OpCode::Ping && !matches!(upstream, Upstream::Native(_)) {
                    client.send(Frame::pong(frame.payload().to_vec())).await?;
                } else if frame.opcode() == OpCode::Pong && !matches!(upstream, Upstream::Native(_)) {
                    continue;
                } else if closing {
                    if let Some(mut admission) = turn.take() { admission.permits.neutral(cfg); }
                    cancelled_usage_tail(&mut upstream, &mut protocol, &client.activity, false).await;
                    return Ok(());
                } else if creates(&frame) {
                    if pending.is_some() { return Err((1013, "too many pending turns")); }
                    pending_received_at = crate::usage::model::now();
                    pending_started = tokio::time::Instant::now();
                    pending = Some(frame);
                } else if let Err(e) = send_to_upstream(&mut upstream, frame).await {
                    if e.0 == 1008 {
                        if let Some(mut admission) = turn.take() {
                            admission.permits.report(g, &route, current_model.as_deref(), crate::events::Reason::ProtocolError, crate::events::Action::Returned, Some(101), attempts,
                                crate::events::Details { phase: Some(crate::events::Phase::WsSend), counted_failure: Some(false), local_code: Some(code::UNSUPPORTED_EVENT.into()), ..Default::default() });
                            admission.permits.neutral(cfg);
                        }
                    } else {
                        // A client control/message send failure cannot safely replay a generation.
                        disconnect_turn(g, &route, cfg, current_model.as_deref(), &mut turn, &mut protocol, received, attempts, ordinary_attempts, false, None, crate::events::Phase::WsSend, code::NETWORK);
                    }
                    return Err(e);
                }
            },
            _ = tokio::time::sleep_until(deadline), if turn.is_some() => {
                let mut partial_progress = None;
                if let Upstream::Native(peer) = &upstream {
                    if let Some(at) = peer.activity.read_at().filter(|at| *at >= receive_started) {
                        partial_progress = Some(at);
                        let next = at + Duration::from_secs(cfg.idle_seconds);
                        if next > tokio::time::Instant::now() { deadline = next; continue; }
                    }
                }
                let body_started = received || partial_progress.is_some();
                if !body_started {
                    failover.failure(super::failover::Failure::Transport);
                    failover.finish(super::failover::Exit::ReplayUnsafe);
                    if let Some(u) = &protocol { u.associate_failure(failover.failure_generation()); }
                }
                if let Some(u) = &mut protocol {
                    let phase = if body_started { "ws_idle" } else { "ws_first_byte" };
                    let seconds = if body_started { cfg.idle_seconds } else { cfg.first_byte_seconds };
                    if let Some(at) = partial_progress {
                        u.timeout_after(phase, seconds, at.elapsed());
                    } else {
                        u.timeout(phase, seconds);
                    }
                }

                let native = matches!(upstream, Upstream::Native(_));
                retry_pending = disconnect_turn(g, &route, cfg, current_model.as_deref(), &mut turn, &mut protocol, body_started, attempts, ordinary_attempts, native && !unknown_affinity, None, crate::events::Phase::WsReceive, if body_started { code::STREAM_TIMEOUT } else { code::FIRST_BYTE_TIMEOUT });
                upstream = Upstream::Disconnected;
                if retry_pending { continue; }
                return Err((1013, if body_started { "upstream timeout" } else { code::FIRST_BYTE_TIMEOUT }));
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn keepalive_preserves_control_frames_and_does_not_leak_its_own_pong() {
        let (a, b) = tokio::io::duplex(4096);
        let peer = WebSocket::from_stream(a, yawc::Role::Server, Options::default()).unwrap();
        let mut peer = Peer::new(
            peer,
            Some(Duration::from_millis(20)),
            Default::default(),
            600,
        );
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
