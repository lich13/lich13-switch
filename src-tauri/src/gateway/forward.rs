use super::{
    admission::{Admission, Budget, CapacitySource, Rejected},
    attempt::{DeliveryGuard, DownstreamEnd},
    circuit,
    connector::{self, BoxError},
    health_settlement::observe_protocol,
    model::Settings,
    replay::{self, Replay, WireBody},
    routing::{self, Requirement},
    Active, Gateway, Route,
};
use crate::errors::code;
use crate::events::{Action, Reason};
use bytes::Bytes;
use http_body_util::{BodyExt, StreamBody};
use hyper::body::Body as _;
use hyper::{body::Incoming, header, HeaderMap, Request, Response, StatusCode, Uri};
use hyper_util::rt::TokioIo;
use std::{
    convert::Infallible,
    time::{Duration, Instant},
};
use tokio::sync::watch;
#[cfg(test)]
#[path = "error_frame_delivery_tests.rs"]
mod error_frame_delivery_tests;
#[derive(Clone, Copy)]
struct ReceivedAt(i64);
pub async fn serve(
    registry: std::sync::Arc<super::registry::Registry>,
    listener: tokio::net::TcpListener,
    mut shutdown: watch::Receiver<bool>,
) {
    loop {
        tokio::select! {
            _=shutdown.changed()=>break,
            result=listener.accept()=>{
                let Ok((socket,_))=result else{break};let registry=registry.clone();let mut stop=shutdown.clone();
                tokio::spawn(async move{
                    let activity = std::sync::Arc::new(super::wire_progress::Activity::default());
                    let (socket, socket_error) = super::wire_progress::observed_tcp(socket, activity.clone());
                    let socket = super::wire_progress::Tracked::new(socket, activity.clone());
                    let connection_activity = activity.clone();
                    let service=hyper::service::service_fn(move|mut request: Request<Incoming>|{
                        let antigravity = request.uri().path().starts_with("/antigravity/");
                        let vertex = antigravity.then(|| (registry.antigravity.lock().clone(), registry.freeze.clone()));
                        let routed = (!antigravity).then(|| super::listener::route(request.uri(), request.headers(), &registry).and_then(|(client, uri)| {
                            *request.uri_mut() = uri;
                            registry.gateway(client).ok_or(super::listener::RouteError::NotFound)
                        }));
                        let delivery = std::sync::Arc::new(super::protocol::Delivery::default());
                        delivery.track(activity.clone());
                        request.extensions_mut().insert(delivery);
                        request.extensions_mut().insert(ReceivedAt(crate::usage::model::now()));
                        request.extensions_mut().insert(activity.clone());
                        async move{
                            if let Some((vertex, freeze)) = vertex {
                                return Ok::<_,Infallible>(super::antigravity::forward(vertex, freeze, request).await);
                            }
                            Ok::<_,Infallible>(match routed.expect("ordinary gateway route") {
                            Ok(g) => {
                                let mut client_stop = g.request_state().shutdown.as_ref().map(watch::Sender::subscribe);
                                tokio::select! {
                                    biased;
                                    _ = stopping(&mut client_stop) => error(StatusCode::SERVICE_UNAVAILABLE, code::GATEWAY_STOPPED, "客户端网关已停止"),
                                    response = forward(g,request) => response,
                                }
                            },
                            Err(super::listener::RouteError::NotFound) => error(StatusCode::NOT_FOUND, code::CLIENT, "客户端网关路径不存在"),
                            Err(super::listener::RouteError::Unauthorized) => error(StatusCode::UNAUTHORIZED, code::LOCAL_AUTH, "客户端网关凭据不匹配"),
                        })}
                    });
                    let connection=hyper::server::conn::http1::Builder::new().preserve_header_case(true).half_close(true)
                        .serve_connection(TokioIo::new(socket),service).with_upgrades();
                    tokio::pin!(connection);
                    let result = tokio::select!{
                        result= &mut connection=>result,
                        error=socket_error=>{connection_activity.failed(&error, false);return;},
                        _=stop.changed()=>{connection_activity.event("gateway_stopping");connection.as_mut().graceful_shutdown();connection.await}
                    };
                    if let Err(error) = result {
                        connection_activity.event(if error.is_parse() { "http_parse_failed" } else if error.is_canceled() { "http_cancelled" } else { "http_connection_failed" });
                        connection_activity.connection_failed(if connection_activity.error_code() != 0 { connection_activity.error_code() } else if error.is_canceled() { super::protocol::DownstreamError::Cancelled.into() } else if error.is_closed() || error.is_incomplete_message() { super::protocol::DownstreamError::Closed.into() } else if error.is_parse() { super::protocol::DownstreamError::ProtocolError.into() } else { super::protocol::DownstreamError::ConnectionError.into() });
                    }
                });
            }
        }
    }
}
type ForwardFrame = (
    Result<hyper::body::Frame<Bytes>, BoxError>,
    bool,
    bool,
    tokio::sync::oneshot::Sender<()>,
);
async fn usage_tail(
    body: &mut WireBody,
    protocol: &mut super::protocol::Protocol,
    shutdown: &mut Option<watch::Receiver<bool>>,
) {
    protocol.begin_usage_tail(false);
    let deadline = tokio::time::Instant::now() + super::model::USAGE_TAIL_DEADLINE;
    let end = loop {
        tokio::select! {
            biased;
            _ = stopping(shutdown) => break "gateway_stopping",
            _ = tokio::time::sleep_until(deadline) => break "usage_tail_deadline",
            next = body.frame() => match next {
                Some(Ok(frame)) => if let Some(data) = frame.data_ref() { protocol.feed(data); },
                None => break "upstream_eof",
                Some(Err(_)) => break "upstream_read_failed",
            }
        }
    };
    protocol.finish_usage_tail(end);
}
async fn finish_delivery(
    delivery: &super::protocol::Delivery,
    protocol: &mut super::protocol::Protocol,
    activity: &super::wire_progress::Activity,
    shutdown: &mut Option<watch::Receiver<bool>>,
    status: u16,
    seconds: u64,
) {
    tokio::select! {
        biased;
        _ = stopping(shutdown) => { protocol.finish(Some(status), code::GATEWAY_STOPPED); }
        result = super::wire_progress::writing(delivery.wait(), activity, seconds) => {
            if result.is_err() {
                DownstreamEnd::Timeout(seconds).record(delivery, protocol);
            }
            if delivery.error.load(std::sync::atomic::Ordering::Acquire) != 0 {
                protocol.finish(Some(status), code::CLIENT_DISCONNECTED);
            }
        }
    }
}
async fn send_frame(
    sender: &tokio::sync::mpsc::Sender<ForwardFrame>,
    frame: Result<hyper::body::Frame<Bytes>, BoxError>,
    terminal: bool,
    complete: bool,
    activity: &super::wire_progress::Activity,
    seconds: u64,
) -> Result<(), DownstreamEnd> {
    super::wire_progress::writing(
        async {
            let (ack, received) = tokio::sync::oneshot::channel();
            sender
                .send((frame, terminal, complete, ack))
                .await
                .map_err(|_| DownstreamEnd::Closed)?;
            received.await.map_err(|_| DownstreamEnd::Closed)
        },
        activity,
        seconds,
    )
    .await
    .map_err(|_| DownstreamEnd::Timeout(seconds))?
}

/// An error frame has the same delivery deadline as data. Its failed handoff
/// must settle delivery now, not start a second wait after discarding the error.
async fn send_error_frame(
    sender: &tokio::sync::mpsc::Sender<ForwardFrame>,
    protocol: &mut super::protocol::Protocol,
    activity: &super::wire_progress::Activity,
    shutdown: &mut Option<watch::Receiver<bool>>,
    status: u16,
    seconds: u64,
) -> bool {
    let delivery = protocol.delivery();
    tokio::select! {
        biased;
        _ = stopping(shutdown) => {
            delivery.event("gateway_stopping");
            delivery.settle();
            protocol.finish(Some(status), code::GATEWAY_STOPPED);
            false
        }
        result = send_frame(sender, Err(std::io::Error::other("上游流中断").into()), false, false, activity, seconds) => {
            if let Err(reason) = result {
                reason.record(&delivery, protocol);
                true
            } else {
                false
            }
        }
    }
}
pub(super) fn error(status: StatusCode, code: &str, message: &str) -> Response<WireBody> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(replay::full(
            serde_json::to_vec(
                &serde_json::json!({"error":{"type":"vela_gateway","code":code,"message":message}}),
            )
            .unwrap(),
        ))
        .unwrap()
}
pub(super) fn target(base: &str, incoming: &Uri) -> Result<Uri, BoxError> {
    let mut base = url::Url::parse(base)?;
    super::model::normalize_api_path(&mut base);
    let path = incoming.path();
    let suffix = path
        .strip_prefix("/v1/")
        .map(|s| format!("/{s}"))
        .unwrap_or_else(|| {
            if path == "/v1" {
                String::new()
            } else {
                path.into()
            }
        });
    format!(
        "{}{}{}",
        base.as_str().trim_end_matches('/'),
        suffix,
        incoming
            .query()
            .map(|q| format!("?{q}"))
            .unwrap_or_default()
    )
    .parse()
    .map_err(Into::into)
}
pub(super) fn clean_headers(headers: &mut HeaderMap, upgrade: bool) {
    let nominated: Vec<_> = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|s| s.split(',').map(str::trim).map(str::to_owned))
        .collect();
    for name in nominated {
        if !(upgrade && name.eq_ignore_ascii_case("upgrade")) {
            headers.remove(&name);
        }
    }
    for name in [
        "connection",
        "proxy-connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "transfer-encoding",
    ] {
        headers.remove(name);
    }
    if upgrade {
        headers.insert(
            header::CONNECTION,
            header::HeaderValue::from_static("upgrade"),
        );
    } else {
        headers.remove(header::UPGRADE);
    }
}
pub(super) fn capacity_delay(settings: &Settings, retry: Option<Duration>) -> Duration {
    retry.unwrap_or(Duration::from_secs(settings.capacity_retry_seconds))
}
/// Everything the relay task owns once the response head has been accepted.
struct Relay {
    active: Active,
    traffic: super::freeze::Ticket,
    slot: super::admission::Slot,
    protocol: super::protocol::Protocol,
    g: Gateway,
    route: Route,
    model: Option<String>,
    permits: super::health_settlement::Permits,
    cfg: Settings,
    status: StatusCode,
    attempted: usize,
    /// Frames read while inspecting the start of the response.
    prefix: Vec<(hyper::body::Frame<Bytes>, bool, bool)>,
    request_budget: super::wait_budget::WaitBudget,
    sender: tokio::sync::mpsc::Sender<ForwardFrame>,
    activity: std::sync::Arc<super::wire_progress::Activity>,
    shutdown: Option<watch::Receiver<bool>>,
    ended: bool,
    failed: bool,
    delivery: std::sync::Arc<super::protocol::Delivery>,
    dedicated_search: bool,
    body: WireBody,
    inspected_bytes: u64,
    neutral: bool,
    operation: crate::usage::model::Operation,
}
/// Relays the inspected prefix and the rest of the upstream body to the
/// client, settling usage, health and delivery when the stream ends.
async fn relay(relay: Relay) {
    let Relay {
        active,
        traffic,
        slot,
        mut protocol,
        g,
        route,
        model,
        mut permits,
        cfg,
        status,
        attempted,
        prefix,
        request_budget,
        sender,
        activity,
        mut shutdown,
        ended,
        failed,
        delivery,
        dedicated_search,
        mut body,
        mut inspected_bytes,
        neutral,
        operation,
    } = relay;
    let mut active = Some(active);
    let traffic = traffic;
    // The concurrency slot is held until the response has been relayed.
    let _slot = slot;
    remember(&protocol, &g, &route, model.as_deref());
    observe_protocol(
        &protocol,
        &mut permits,
        &cfg,
        &g,
        &route,
        model.as_deref(),
        Some(status.as_u16()),
        attempted,
    );
    let mut disconnected = false;
    let prefix_len = prefix.len();
    for (index, (frame, terminal, business)) in prefix.into_iter().enumerate() {
        tokio::select! {
            biased;
            _ = request_budget.cancelled() => { protocol.finish(Some(status.as_u16()), code::CANCELLED); permits.neutral(&cfg); let _ = send_error_frame(&sender, &mut protocol, &activity, &mut shutdown, status.as_u16(), cfg.idle_seconds).await; return; }
            _ = stopping(&mut shutdown) => { protocol.finish(Some(status.as_u16()), code::GATEWAY_STOPPED); permits.neutral(&cfg); return; }
            result = send_frame(&sender, Ok(frame), terminal, ended && !failed && index + 1 == prefix_len, &activity, cfg.idle_seconds) => { if let Err(reason) = result { reason.record(&delivery, &mut protocol); disconnected = true; break; } }
        }
        protocol.output_handed(business);
    }
    if ended {
        if failed && !disconnected {
            // Preserve the received prefix, then expose the transport failure.
            // A clean EOF here would hide an incomplete generation.
            disconnected = send_error_frame(
                &sender,
                &mut protocol,
                &activity,
                &mut shutdown,
                status.as_u16(),
                cfg.idle_seconds,
            )
            .await;
        }
        drop(sender);
        if disconnected {
            protocol.finish(Some(status.as_u16()), code::CLIENT_DISCONNECTED);
        } else {
            finish_delivery(
                &delivery,
                &mut protocol,
                &activity,
                &mut shutdown,
                status.as_u16(),
                cfg.idle_seconds,
            )
            .await;
        }
        if protocol.succeeded() && !dedicated_search {
            g.persist_success(&route.provider).await;
        }
        return;
    }
    loop {
        if disconnected || sender.is_closed() {
            disconnected = true;
            DownstreamEnd::Closed.record(&delivery, &mut protocol);
            active.take();
            permits.neutral(&cfg);
            usage_tail(&mut body, &mut protocol, &mut shutdown).await;
            break;
        }
        // Backpressure is awaited separately. Only time spent polling the
        // upstream measures its silence; completed events do not cut off tails.
        let limit = tokio::time::Instant::now() + Duration::from_secs(cfg.idle_seconds);
        let next = tokio::select! {
            biased;
            _ = request_budget.cancelled() => {
                protocol.finish(Some(status.as_u16()), code::CANCELLED);
                permits.neutral(&cfg);
                disconnected = send_error_frame(&sender, &mut protocol, &activity, &mut shutdown, status.as_u16(), cfg.idle_seconds).await;
                break;
            }
            _ = request_budget.wait() => {
                protocol.timeout("retry_budget", cfg.retry_budget_seconds);
                protocol.finish(Some(status.as_u16()), code::FIRST_BYTE_TIMEOUT);
                disconnected = send_error_frame(&sender, &mut protocol, &activity, &mut shutdown, status.as_u16(), cfg.idle_seconds).await;
                break;
            }
            _ = stopping(&mut shutdown) => { protocol.finish(Some(status.as_u16()), code::GATEWAY_STOPPED); permits.neutral(&cfg); break; }
            _ = sender.closed(), if !disconnected => { disconnected = true; continue; }
            frame = request_budget.response(limit, operation == crate::usage::model::Operation::Compaction, body.frame()) => frame,
        };
        match next {
            Ok(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    let skipped = inspected_bytes.min(data.len() as u64) as usize;
                    inspected_bytes -= skipped as u64;
                    if skipped < data.len() {
                        protocol.feed(&data[skipped..]);
                    }
                }
                let complete = body.is_end_stream();
                if complete {
                    protocol.finish(
                        Some(status.as_u16()),
                        if neutral { code::HTTP } else { code::OK },
                    );
                }
                remember(&protocol, &g, &route, model.as_deref());
                observe_protocol(
                    &protocol,
                    &mut permits,
                    &cfg,
                    &g,
                    &route,
                    model.as_deref(),
                    Some(status.as_u16()),
                    attempted,
                );
                if !disconnected {
                    tokio::select! {
                        biased;
                        _ = request_budget.cancelled() => { protocol.finish(Some(status.as_u16()), code::CANCELLED); permits.neutral(&cfg); disconnected = send_error_frame(&sender, &mut protocol, &activity, &mut shutdown, status.as_u16(), cfg.idle_seconds).await; break; }
                        _ = stopping(&mut shutdown) => { protocol.finish(Some(status.as_u16()), code::GATEWAY_STOPPED); permits.neutral(&cfg); break; }
                        result = send_frame(&sender, Ok(frame), protocol.terminal_observed(), complete, &activity, cfg.idle_seconds) => { if let Err(reason) = result { reason.record(&delivery, &mut protocol); disconnected = true; } }
                    }
                    if !disconnected {
                        protocol.output_handed(protocol.business_observed());
                    }
                }
                if complete {
                    break;
                }
            }
            Ok(None) => {
                protocol.finish(
                    Some(status.as_u16()),
                    if neutral { code::HTTP } else { code::OK },
                );
                // EOF may finish a JSON value or an SSE event without a
                // trailing newline. All preceding body frames were acknowledged.
                delivery.handed(0, protocol.terminal_observed());
                protocol.output_handed(protocol.business_observed());
                break;
            }
            result => {
                let category = if result.is_err() {
                    code::STREAM_TIMEOUT
                } else {
                    code::STREAM_INTERRUPTED
                };
                if result.is_err() {
                    protocol.timeout("stream_idle", cfg.idle_seconds);
                }
                protocol.finish(Some(status.as_u16()), category);
                if !disconnected && protocol.transport_failure() {
                    disconnected = send_error_frame(
                        &sender,
                        &mut protocol,
                        &activity,
                        &mut shutdown,
                        status.as_u16(),
                        cfg.idle_seconds,
                    )
                    .await;
                }
                break;
            }
        }
    }
    // Close the body channel before waiting: Hyper must see its end in
    // order to flush the final chunk/trailers. Socket completion is local
    // write evidence, never an acknowledgement by the client.
    traffic.finishing();
    drop(body);
    drop(sender);
    if disconnected {
        protocol.finish(Some(status.as_u16()), code::CLIENT_DISCONNECTED);
    } else {
        finish_delivery(
            &delivery,
            &mut protocol,
            &activity,
            &mut shutdown,
            status.as_u16(),
            cfg.idle_seconds,
        )
        .await;
    }
    if disconnected {
        return;
    }
    remember(&protocol, &g, &route, model.as_deref());
    observe_protocol(
        &protocol,
        &mut permits,
        &cfg,
        &g,
        &route,
        model.as_deref(),
        Some(status.as_u16()),
        attempted,
    );
    if protocol.succeeded() && !dedicated_search {
        g.persist_success(&route.provider).await;
    }
}
/// The client-facing body: frames from the relay task, with delivery marked
/// before each frame is yielded.
fn delivered(
    mut receiver: tokio::sync::mpsc::Receiver<ForwardFrame>,
    output_delivery: std::sync::Arc<super::protocol::Delivery>,
    empty: bool,
) -> WireBody {
    let guard = DeliveryGuard {
        delivery: output_delivery.clone(),
        complete: empty,
    };
    let output = async_stream::stream! {
        let mut guard = guard;
        while let Some((frame, terminal, complete, ack)) = receiver.recv().await {
            let bytes = frame.as_ref().ok().and_then(|f| f.data_ref()).map_or(0, Bytes::len);
            output_delivery.handed(bytes, terminal);
            if frame.is_err() {
                guard.complete = true;
                output_delivery.event("response_body_aborted_upstream");
                output_delivery.settle();
            } else if complete {
                guard.complete = true;
                output_delivery.body_complete();
            }
            let _ = ack.send(());
            yield frame;
        }
        guard.complete = true;
        output_delivery.body_complete();
    };
    StreamBody::new(output).boxed_unsync()
}
/// Why no upstream request was built for a candidate.
enum Unbuilt {
    /// The provider cannot take this request; try the next candidate.
    Skip,
    Reject(Box<Response<WireBody>>),
}
/// The request sent to one provider: the replayed body (with the model
/// rewritten when the provider maps it), the provider's address and its
/// credentials. Also returns the mapped upstream model, if any.
async fn upstream_request(
    route: &Route,
    parts: &hyper::http::request::Parts,
    replay: &Replay,
    model: Option<&str>,
    operation: crate::usage::model::Operation,
    request_budget: &super::wait_budget::WaitBudget,
    websocket: bool,
) -> Result<(Request<WireBody>, Option<String>), Unbuilt> {
    let uri = match route.target(&parts.uri) {
        Ok(uri) => uri,
        Err(_) => return Err(Unbuilt::Skip),
    };
    let upstream_model = route
        .dsh_models
        .as_ref()
        .and_then(|mapping| model.and_then(|m| mapping.resolve(m)))
        .map(str::to_owned);
    let mapped = if let Some(upstream_model) = upstream_model
        .as_ref()
        .filter(|upstream| Some(upstream.as_str()) != model)
    {
        match replay
            .mapped_model(
                parts
                    .headers
                    .get(header::CONTENT_ENCODING)
                    .and_then(|h| h.to_str().ok())
                    .unwrap_or("identity"),
                upstream_model,
            )
            .await
        {
            Ok(body) => Some(body),
            Err(_) => {
                return Err(Unbuilt::Reject(Box::new(error(
                    StatusCode::BAD_REQUEST,
                    code::CONFIG,
                    "DSH 模型映射需要有效的 JSON 请求",
                ))))
            }
        }
    } else {
        None
    };
    let mapped_length = mapped.as_ref().map(|b| b.len());
    let mut request_body = mapped.map(replay::full).unwrap_or_else(|| replay.body());
    if operation == crate::usage::model::Operation::Compaction {
        let budget = request_budget.clone();
        let identity = budget.deadline();
        let mut dispatched = false;
        request_body = request_body
            .map_frame(move |frame| {
                // Hyper polls the upload only after acquiring a connection.
                // Queueing and connection setup retain their original budget.
                if !dispatched {
                    dispatched = true;
                    budget.compaction_execution(identity, true);
                }
                frame
            })
            .boxed_unsync();
    }
    let mut upstream = Request::new(request_body);
    *upstream.method_mut() = parts.method.clone();
    *upstream.uri_mut() = uri;
    *upstream.headers_mut() = parts.headers.clone();
    clean_headers(upstream.headers_mut(), websocket);
    upstream.headers_mut().remove(header::HOST);
    upstream.headers_mut().remove(header::AUTHORIZATION);
    upstream.headers_mut().remove("x-api-key");
    upstream.headers_mut().remove("x-goog-api-key");
    let Ok(auth) = header::HeaderValue::from_str(&format!("Bearer {}", route.provider.token))
    else {
        return Err(Unbuilt::Skip);
    };
    upstream.headers_mut().insert(header::AUTHORIZATION, auth);
    if let Some(connection) = &route.grok {
        if connection
            .authenticate(upstream.headers_mut(), &route.provider.token)
            .is_err()
        {
            return Err(Unbuilt::Reject(Box::new(error(
                StatusCode::BAD_GATEWAY,
                code::GROK_AUTH,
                "供应商认证配置无效",
            ))));
        }
    }
    if let Some(length) = mapped_length {
        upstream.headers_mut().remove(header::CONTENT_ENCODING);
        upstream.headers_mut().remove(header::TRAILER);
        upstream
            .headers_mut()
            .insert(header::CONTENT_LENGTH, length.into());
    } else if !replay.has_trailers() {
        upstream.headers_mut().insert(
            header::CONTENT_LENGTH,
            header::HeaderValue::from(replay.length),
        );
    } else {
        upstream.headers_mut().remove(header::CONTENT_LENGTH);
    }
    Ok((upstream, upstream_model))
}
/// What is known about a request before the first attempt: what it asks for,
/// which conversation it belongs to and which providers may serve it.
struct Plan {
    hints: Result<replay::RequestHints, BoxError>,
    stream_hint: bool,
    model: Option<String>,
    /// The only provider allowed to serve this request, when one is fixed.
    pinned: Option<String>,
    /// A continuation whose owner is unknown: try one provider, never move.
    unknown_affinity: bool,
    continuation: bool,
    operation: crate::usage::model::Operation,
    requirement: Requirement,
    fixed_dsh: Option<String>,
    responses: bool,
    thread: Option<String>,
    thread_identity: crate::usage::model::ThreadIdentity,
    auxiliary: bool,
    owned_search: bool,
    ownership: Option<std::sync::Arc<super::compaction::Lease>>,
    dedicated_search: bool,
    established_capacity_owner: Option<String>,
    usage_trace: Option<crate::usage::Trace>,
}
/// Inspects the request and narrows `ids` to the providers it may use.
async fn plan(
    gateway: &Gateway,
    parts: &hyper::http::request::Parts,
    replay: &Replay,
    mode: &str,
    websocket: bool,
    ids: &mut Vec<String>,
    routes: &std::collections::HashMap<String, Route>,
) -> Result<Plan, Box<Response<WireBody>>> {
    let content_type = parts
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    let hints = if content_type.contains("json") || content_type.starts_with("multipart/form-data")
    {
        replay
            .inspect(
                parts
                    .headers
                    .get(header::CONTENT_ENCODING)
                    .and_then(|h| h.to_str().ok())
                    .unwrap_or("identity"),
                content_type,
            )
            .await
    } else {
        Ok(replay::RequestHints::default())
    };
    let stream_hint = websocket
        || hints.as_ref().is_ok_and(|h| h.stream)
        || parts
            .headers
            .get(header::ACCEPT)
            .is_some_and(|h| h.as_bytes().windows(17).any(|w| w == b"text/event-stream"));
    let mut model = hints.as_ref().ok().and_then(|h| h.model.clone());
    let compaction_trigger = hints.as_ref().is_ok_and(|h| h.compaction_trigger);
    let mut pinned = (mode == "manual").then(|| ids.first().cloned()).flatten();
    let mut unknown_affinity = false;
    let continuation = match &hints {
        Ok(hints) if hints.previous_response_id.is_some() => {
            let previous = hints.previous_response_id.as_ref().unwrap();
            let owner = gateway
                .request_state()
                .affinity
                .get(previous)
                .filter(|(_, _, at)| at.elapsed() < Duration::from_secs(3600))
                .cloned();
            if let Some((owner, previous_model, _)) = owner {
                pinned = Some(owner.clone());
                *ids = vec![owner];
                if model.is_none() {
                    model = previous_model;
                }
            } else {
                ids.truncate(1);
                unknown_affinity = true;
            }
            true
        }
        // The original bytes may contain an uninspectable continuation. Try only
        // one eligible (unrestricted) route, without cutting off earlier filters.
        Err(_) => {
            unknown_affinity = true;
            true
        }
        _ => false,
    };
    let operation = if compaction_trigger {
        crate::usage::model::Operation::Compaction
    } else {
        crate::usage::model::Operation::for_path(parts.uri.path())
    };
    let requirement = if operation == crate::usage::model::Operation::WebSearch
        || (!websocket && routing::resource(&parts.method, parts.uri.path()))
    {
        Requirement::Resource
    } else {
        Requirement::model(model.as_deref())
    };
    let fixed_dsh = if gateway.client_id() == super::ClientId::Dsh {
        let mut eligible = ids.iter().filter(|id| {
            routes
                .get(*id)
                .is_some_and(|route| requirement.allows(route.provider.allowed_models.as_deref()))
        });
        let first = eligible.next().cloned();
        if eligible.next().is_none() {
            first
        } else {
            None
        }
    } else {
        None
    };
    let responses = matches!(
        parts.uri.path().trim_end_matches('/'),
        "/responses" | "/v1/responses" | "/responses/compact" | "/v1/responses/compact"
    );
    let (thread, thread_identity) = if gateway.client_id() == super::ClientId::Codex
        && operation == crate::usage::model::Operation::WebSearch
        && !websocket
    {
        super::compaction::search_identity(&parts.headers)
    } else {
        super::compaction::identity(&parts.headers)
    };
    let auxiliary = hints.as_ref().is_ok_and(|h| h.auxiliary);
    if (auxiliary || hints.is_err())
        && gateway.client_id() == super::ClientId::Codex
        && mode == "auto"
    {
        if let Some(owner) = thread
            .as_deref()
            .and_then(|s| gateway.0.compaction.owner(s))
            .filter(|id| ids.contains(id))
        {
            *ids = vec![owner.clone()];
            if hints.is_err() {
                // Metadata failure cannot move a conversation we already know.
                pinned = Some(owner);
            }
        }
    }
    let owned_search = gateway.client_id() == super::ClientId::Codex
        && mode == "auto"
        && !websocket
        && operation == crate::usage::model::Operation::WebSearch
        && !unknown_affinity
        && pinned.is_none();
    let ownership = if owned_search {
        match thread
            .clone()
            .map(|session| gateway.0.compaction.prepare_search(session))
            .transpose()
        {
            Ok(lease) => lease.flatten(),
            Err(e) => {
                return Err(Box::new(error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    &e.code,
                    &e.message,
                )))
            }
        }
    } else if gateway.client_id() == super::ClientId::Codex
        && mode == "auto"
        && responses
        && !unknown_affinity
        && !auxiliary
    {
        let queued = match super::compaction::queued(gateway) {
            Ok(queued) => queued,
            Err(e) => {
                return Err(Box::new(error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    &e.code,
                    &e.message,
                )))
            }
        };
        if let Some(session) = thread.clone().filter(|_| {
            queued
                .iter()
                .any(|p| requirement.allows(p.allowed_models.as_deref()))
        }) {
            let lease = gateway.0.compaction.prepare_request(
                session,
                hints.as_ref().ok(),
                !continuation && operation == crate::usage::model::Operation::Model,
                &queued,
                &requirement,
                pinned.as_deref(),
            );
            if lease.is_none() {
                return Err(Box::new(error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    code::CONVERSATION_OWNERSHIP,
                    "线程归属记录暂不可用，请检查网关状态",
                )));
            }
            lease
        } else {
            None
        }
    } else {
        None
    };
    let search_backups = if mode == "auto"
        && gateway.client_id() == super::ClientId::Codex
        && operation == crate::usage::model::Operation::WebSearch
        && !websocket
    {
        gateway
            .search_backups()
            .into_iter()
            .filter(|id| routes.contains_key(id))
            .collect::<Vec<_>>()
    } else {
        vec![]
    };
    let dedicated_search = !search_backups.is_empty();
    if dedicated_search {
        ids.extend(search_backups);
    }
    let established_capacity_owner = if dedicated_search {
        None
    } else {
        routing::capacity_owner(gateway.client_id(), mode == "manual", ownership.as_deref())
    };
    let usage_trace = if matches!(requirement, Requirement::Resource)
        && operation != crate::usage::model::Operation::WebSearch
    {
        None
    } else {
        gateway.usage_operation(model.as_deref(), operation)
    };
    Ok(Plan {
        hints,
        stream_hint,
        model,
        pinned,
        unknown_affinity,
        continuation,
        operation,
        requirement,
        fixed_dsh,
        responses,
        thread,
        thread_identity,
        auxiliary,
        owned_search,
        ownership,
        dedicated_search,
        established_capacity_owner,
        usage_trace,
    })
}
fn stopped() -> Response<WireBody> {
    error(StatusCode::SERVICE_UNAVAILABLE, code::STOPPED, "网关已停止")
}
/// Waits out a transient retry delay, then reports whether the same provider
/// is still queued and available to be tried again first.
async fn transient_wait(
    gateway: &Gateway,
    route: &Route,
    reset_generation: u64,
    delay: Duration,
    max_waiting: usize,
    pinned: Option<&str>,
    requirement: &Requirement,
) -> Result<bool, Rejected> {
    gateway
        .0
        .admission
        .wait_transient(
            CapacitySource {
                provider_id: route.provider.id.clone(),
                reset_generation,
            },
            delay,
            max_waiting,
        )
        .await?;
    Ok(gateway.routing_ids(pinned).contains(&route.provider.id)
        && route.health_for(requirement).available)
}
/// How an attempt ends when it has no response to relay.
enum End {
    /// Go around the attempt loop again.
    Again,
    /// Leave the loop; the request is answered from what the attempts left.
    Stop,
    Reply(Box<Response<WireBody>>),
}
impl End {
    fn reply(response: Response<WireBody>) -> Self {
        Self::Reply(Box::new(response))
    }
}
/// Leaves the attempt loop the way `End` says.
macro_rules! end {
    ($end:expr) => {
        match $end {
            End::Again => continue,
            End::Stop => break,
            End::Reply(response) => return *response,
        }
    };
}
/// Takes the result of a stage, or leaves the attempt loop the way it ended.
macro_rules! step {
    ($stage:expr) => {
        match $stage {
            Ok(value) => value,
            Err(end) => end!(end),
        }
    };
}
/// One provider's turn at the request, from admission to its response head.
struct Attempt {
    admission: Admission,
    route: Route,
    protocol: super::protocol::Protocol,
    reset_generation: u64,
    /// This attempt went to a different provider than the previous one.
    rerouted: bool,
    started: Instant,
    search_deadline: tokio::time::Instant,
}
/// The start of an accepted response, read before anything reaches the client.
struct Opening {
    status: StatusCode,
    parts: hyper::http::response::Parts,
    body: WireBody,
    /// Each frame with whether a terminal event and business output were seen by then.
    prefix: Vec<(hyper::body::Frame<Bytes>, bool, bool)>,
    prefix_bytes: usize,
    ended: bool,
    failed: bool,
    failure_kind: &'static str,
    /// The start was spooled to disk; `body` replays it before the rest.
    preinspected: bool,
    inspected_bytes: u64,
}
/// One HTTP request across its attempts: what was planned for it, the queue it
/// still has, and what the attempts so far have left behind.
struct Exchange<'a> {
    gateway: &'a Gateway,
    settings: &'a Settings,
    mode: &'a str,
    parts: &'a hyper::http::request::Parts,
    replay: &'a Replay,
    request_budget: &'a super::wait_budget::WaitBudget,
    websocket: bool,
    active: Active,
    traffic: super::freeze::Ticket,
    downstream_upgrade: Option<hyper::upgrade::OnUpgrade>,
    plan: Plan,
    ids: Vec<String>,
    routes: std::collections::HashMap<String, Route>,
    failover: super::failover::Tracker,
    wait_budget: Budget,
    last: Option<Response<WireBody>>,
    last_error: Option<super::health_settlement::Reported>,
    last_category: &'static str,
    attempted: usize,
    ordinary_attempts: usize,
    capacity_protected: bool,
    previous_provider: Option<String>,
    capacity_pending: bool,
    capacity_wait_owner: Option<String>,
    hard_denied: std::collections::HashSet<(String, String)>,
    capacity_retry_after: Option<Duration>,
    rpm_retry_after: Option<Duration>,
    capacity_sources: Vec<CapacitySource>,
    transient_retries: u8,
    retry_owner: Option<String>,
    search_fallback: bool,
}
impl Exchange<'_> {
    /// The queue is empty: waits out a pending capacity cooldown and rebuilds
    /// the queue, or stops.
    async fn await_capacity(&mut self) -> End {
        if self.capacity_pending && self.ordinary_attempts <= self.settings.max_retries {
            let delay = capacity_delay(self.settings, self.capacity_retry_after);
            self.failover.wait(
                if self.capacity_retry_after.is_some() {
                    super::failover::WaitKind::RetryAfter
                } else {
                    super::failover::WaitKind::CapacityRetry
                },
                delay.as_secs(),
            );
            let owner_route = self
                .capacity_wait_owner
                .as_deref()
                .or_else(|| {
                    self.last_error
                        .as_ref()
                        .and_then(|e| e.provider_id.as_deref())
                })
                .and_then(|id| self.routes.get(id));
            self.request_budget
                .describe(None, self.plan.model.as_deref(), "capacity");
            let waited = self
                .gateway
                .wait_capacity_checked(
                    &self.capacity_sources,
                    delay,
                    self.settings.max_waiting,
                    self.capacity_wait_owner.is_some(),
                    owner_route,
                    self.last_error.as_deref(),
                )
                .await;
            match waited {
                Ok(true) => {
                    if let Some(route) = owner_route {
                        self.hard_denied
                            .insert((route.provider.id.clone(), route.provider.version.clone()));
                    }
                    self.capacity_wait_owner = None;
                    self.failover
                        .failure(super::failover::Failure::AccountRejected);
                }
                Ok(false) => {}
                Err(Rejected::Stopped) => {
                    self.failover.finish(super::failover::Exit::Stopped);
                    return End::reply(stopped());
                }
                Err(_) => return End::Stop,
            }

            (self.ids, self.routes) = super::attempt::queue(
                self.gateway,
                self.plan.pinned.as_deref(),
                &self.hard_denied,
                self.parts.uri.path(),
            );
            if self.ids.is_empty() {
                return End::Stop;
            }
            self.retry_owner = self
                .capacity_wait_owner
                .take()
                .filter(|owner| self.ids.contains(owner));
            self.capacity_pending = false;
            self.capacity_retry_after = None;
            self.capacity_sources.clear();
            return End::Again;
        }
        End::Stop
    }
    /// The providers this attempt may be admitted to, and whether a provider
    /// that takes new threads was passed over for not serving the model.
    async fn candidates(&mut self) -> Result<(Vec<Route>, bool), End> {
        let mut candidates: Vec<_> = self
            .ids
            .iter()
            .filter_map(|id| self.routes.get(id).cloned())
            .collect();
        if self.plan.dedicated_search {
            candidates.retain(|route| route.provider.search_only == self.search_fallback);
            if candidates.is_empty() {
                return Err(End::Stop);
            }
            if self.search_fallback {
                self.wait_budget.search_fallback();
            }
        }
        let mut observed_candidates = candidates.clone();
        observed_candidates.extend(
            self.gateway
                .routing_ids(self.plan.pinned.as_deref())
                .iter()
                .filter_map(|id| self.gateway.route(id))
                .filter(|r| !r.accepts_protocol(self.parts.uri.path())),
        );
        let matches_model = candidates.iter().any(|r| {
            self.plan
                .requirement
                .allows(r.provider.allowed_models.as_deref())
        });
        let new_thread_model_denied = self.plan.ownership.as_ref().is_some_and(|lease| {
            lease.admission_policy().0 == super::compaction::Intake::NewThread
        }) && candidates.iter().any(|r| {
            r.provider.take_new_threads
                && !self
                    .plan
                    .requirement
                    .allows(r.provider.allowed_models.as_deref())
        });
        if matches_model {
            candidates.retain(|r| {
                self.plan
                    .requirement
                    .allows(r.provider.allowed_models.as_deref())
            });
        }
        self.wait_budget.capacity_owner = None;
        if self.plan.dedicated_search && self.search_fallback {
            // Search backups do not inherit generation intake policies.
        } else if let Some(owner) = self.retry_owner.take() {
            candidates.retain(|route| route.provider.id == owner);
        } else if let Some(lease) = self
            .plan
            .ownership
            .as_ref()
            .filter(|_| self.plan.pinned.is_none())
        {
            candidates = if self.attempted > 0 || !self.hard_denied.is_empty() {
                lease.candidates_after_failure(candidates, &self.plan.requirement)
            } else {
                lease.candidates_for_admission(
                    candidates,
                    &self.plan.requirement,
                    &mut self.wait_budget,
                )
            };
        } else if self.gateway.client_id() == super::ClientId::Codex
            && self.mode == "auto"
            && self.plan.pinned.is_none()
            && !self.plan.auxiliary
        {
            candidates = super::compaction::unidentified_candidates(candidates);
        }
        if self.plan.dedicated_search && !self.search_fallback {
            candidates.truncate(1);
            if candidates
                .first()
                .is_some_and(|route| !route.health_for(&self.plan.requirement).available)
            {
                self.search_fallback = true;
                return Err(End::Again);
            }
        }
        // A second request entering during its owner's capacity cooldown must
        // wait as well. Read the reset generation before the health snapshot.
        if self.attempted == 0 && candidates.len() == 1 {
            let owner = &candidates[0];
            let source = self.gateway.0.admission.capacity_source(&owner.provider.id);
            let health = owner.health_for(&self.plan.requirement);
            if self.plan.established_capacity_owner.as_deref() == Some(&owner.provider.id)
                && routing::capacity_cooling(&health)
            {
                self.wait_budget.protect_capacity();
                self.failover
                    .wait(super::failover::WaitKind::CapacityRetry, health.retry_in);
                let cause = self.gateway.capacity_cause(
                    &owner.provider.id,
                    self.plan.model.as_deref(),
                    &health,
                );
                match self
                    .gateway
                    .wait_capacity_checked(
                        &[source],
                        Duration::from_secs(health.retry_in),
                        self.settings.max_waiting,
                        true,
                        Some(owner),
                        cause.as_ref(),
                    )
                    .await
                {
                    Ok(true) => {
                        self.hard_denied
                            .insert((owner.provider.id.clone(), owner.provider.version.clone()));
                        self.failover
                            .failure(super::failover::Failure::AccountRejected);
                        self.last = Some(error(
                            StatusCode::TOO_MANY_REQUESTS,
                            code::HTTP,
                            "本次额度查询确认耗尽，没有可用的替代供应商",
                        ));
                        self.last_category = code::HTTP;
                        self.last_error = cause.map(Into::into);
                    }
                    Ok(false) => {}
                    Err(Rejected::Stopped) => return Err(End::reply(stopped())),
                    Err(_) => {
                        return Err(End::reply(error(
                            StatusCode::TOO_MANY_REQUESTS,
                            code::CAPACITY,
                            "等待队列已满",
                        )))
                    }
                }
                (self.ids, self.routes) = super::attempt::queue(
                    self.gateway,
                    self.plan.pinned.as_deref(),
                    &self.hard_denied,
                    self.parts.uri.path(),
                );
                return Err(End::Again);
            }
        }
        let stage = if self.plan.unknown_affinity {
            super::failover::Stage::UnknownOwner
        } else if self.plan.pinned.is_some() {
            super::failover::Stage::CursorPinned
        } else if self.attempted > 0 || !self.hard_denied.is_empty() {
            super::failover::Stage::FailureFallback
        } else if self.plan.ownership.as_ref().is_some_and(|l| l.handoff) {
            super::failover::Stage::ProactiveHandoff
        } else {
            super::failover::Stage::Initial
        };
        self.failover.candidates(
            self.gateway,
            stage,
            &observed_candidates,
            &candidates,
            &self.plan.requirement,
            self.parts.uri.path(),
        );
        if matches_model
            && !candidates.iter().any(|r| {
                self.plan
                    .requirement
                    .allows(r.provider.allowed_models.as_deref())
            })
        {
            if self.attempted > 0 {
                if self.capacity_pending {
                    self.ids.clear();
                    return Err(End::Again);
                }
                return Err(End::Stop);
            }
            self.failover
                .finish(super::failover::Exit::NoEligibleProvider);
            self.gateway.record_policy_rejection(
                self.plan.model.as_deref(),
                false,
                super::compaction::rejection_reason(
                    self.plan.thread.as_deref(),
                    self.plan.hints.as_ref().ok(),
                ),
            );
            return Err(End::reply(error(
                StatusCode::SERVICE_UNAVAILABLE,
                code::NO_ELIGIBLE_PROVIDER,
                "没有符合线程接管策略的供应商",
            )));
        }
        if self.gateway.client_id() == super::ClientId::Codex
            && self.mode == "auto"
            && !self.plan.dedicated_search
            && self.plan.pinned.is_none()
            && !self.plan.auxiliary
        {
            let (intake, owner) = self
                .plan
                .ownership
                .as_ref()
                .map_or((super::compaction::Intake::Ordinary, None), |lease| {
                    lease.admission_policy()
                });
            self.wait_budget.set_intake(intake, owner);
        }
        if self.attempted > 0 || !self.hard_denied.is_empty() {
            // Admission rechecks policy atomically. Its proactive priority gate
            // must also yield to the failure queue, without dropping the claim.
            self.wait_budget.handoff_owner = None;
        }
        Ok((candidates, new_thread_model_denied))
    }
    /// Takes a slot at one of the candidates.
    async fn admit(&mut self, candidates: &[Route]) -> Result<Admission, End> {
        let admission = match self
            .gateway
            .0
            .admission
            .acquire_for_immediate(
                candidates,
                self.mode == "manual" && !(self.plan.dedicated_search && self.search_fallback),
                self.settings.max_waiting,
                &mut self.wait_budget,
                &self.plan.requirement,
                self.capacity_pending,
            )
            .await
            .inspect_err(|reason| self.failover.rejected(reason))
        {
            Ok(value) => value,
            Err(Rejected::Full | Rejected::Timeout) => {
                if self.attempted > 0 {
                    if self.capacity_pending {
                        self.ids.clear();
                        return Err(End::Again);
                    }
                    return Err(End::Stop);
                }
                let mut response = error(
                    StatusCode::TOO_MANY_REQUESTS,
                    code::CAPACITY,
                    "供应商并发已满，等待队列已满或等待超时",
                );
                response
                    .headers_mut()
                    .insert(header::RETRY_AFTER, header::HeaderValue::from_static("5"));
                return Err(End::reply(response));
            }
            Err(Rejected::Cooling(seconds)) => {
                if self.attempted > 0 {
                    if self.capacity_pending {
                        self.ids.clear();
                        return Err(End::Again);
                    }
                    return Err(End::Stop);
                }
                let mut response = error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    code::PROVIDERS_COOLING_DOWN,
                    "供应商正在冷却或恢复探测，请稍后重试",
                );
                response
                    .headers_mut()
                    .insert(header::RETRY_AFTER, seconds.max(1).into());
                return Err(End::reply(response));
            }
            Err(Rejected::RateLimited(seconds)) => {
                self.rpm_retry_after = Some(Duration::from_secs(seconds.max(1)));
                if self.attempted > 0 {
                    self.last_category = code::RPM_LIMIT;
                    return Err(End::Stop);
                }
                let mut response = error(
                    StatusCode::TOO_MANY_REQUESTS,
                    code::RPM_LIMIT,
                    "供应商已达到 RPM 上限，等待后重试",
                );
                response
                    .headers_mut()
                    .insert(header::RETRY_AFTER, seconds.max(1).into());
                return Err(End::reply(response));
            }
            Err(Rejected::Stopped) => {
                return Err(End::reply(stopped()));
            }
            Err(Rejected::Model) => {
                if self.attempted > 0 {
                    if self.capacity_pending {
                        self.ids.clear();
                        return Err(End::Again);
                    }
                    return Err(End::Stop);
                }
                return Err(End::reply(error(
                    StatusCode::BAD_REQUEST,
                    self.plan.requirement.code(),
                    if self.gateway.client_id() == super::ClientId::Dsh {
                        "没有供应商配置此 DSH 模型映射，请编辑 Flash / Pro 映射"
                    } else {
                        self.plan.requirement.message()
                    },
                )));
            }
            Err(Rejected::Policy) => {
                if self.attempted > 0 {
                    if self.capacity_pending {
                        self.ids.clear();
                        return Err(End::Again);
                    }
                    return Err(End::Stop);
                }
                self.gateway.record_policy_rejection(
                    self.plan.model.as_deref(),
                    false,
                    super::compaction::rejection_reason(
                        self.plan.thread.as_deref(),
                        self.plan.hints.as_ref().ok(),
                    ),
                );
                return Err(End::reply(error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    code::NO_ELIGIBLE_PROVIDER,
                    "没有符合线程接管策略的供应商",
                )));
            }
            Err(Rejected::Unavailable | Rejected::Reconnect) => {
                if self.capacity_pending {
                    self.ids.clear();
                    return Err(End::Again);
                }
                return Err(End::Stop);
            }
        };
        Ok(admission)
    }
    /// Settles the admitted provider's endpoint and the thread's claim on it.
    async fn endpoint(&mut self, admission: &mut Admission) -> Result<Route, End> {
        let route = match self
            .gateway
            .endpoint_route(
                admission.route.clone(),
                self.plan.thread.as_deref(),
                self.plan
                    .hints
                    .as_ref()
                    .ok()
                    .and_then(|h| h.previous_response_id.as_deref()),
                self.plan
                    .ownership
                    .as_ref()
                    .is_some_and(|lease| lease.handoff || lease.establishing())
                    || self.plan.hints.as_ref().is_ok_and(|h| h.first_turn),
            )
            .await
        {
            Ok(route) => route,
            Err(reason) => {
                self.failover
                    .finish(super::failover::Exit::AdmissionRejected);
                admission.permits.neutral(self.settings);
                return Err(End::reply(error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    &reason.code,
                    &reason.message,
                )));
            }
        };
        let owner_admitted = self.plan.ownership.as_ref().map_or(Ok(true), |lease| {
            lease.admit_request(
                self.gateway,
                &route.provider,
                &self.plan.requirement,
                self.plan.pinned.is_some(),
            )
        });
        let owner_admitted = match owner_admitted {
            Ok(value) => value,
            Err(e) => {
                admission.permits.neutral(self.settings);
                return Err(End::reply(error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    &e.code,
                    &e.message,
                )));
            }
        };
        if !owner_admitted {
            admission.permits.neutral(self.settings);
            if self.gateway.0.compaction.error().is_some() {
                return Err(End::reply(error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    code::CONVERSATION_OWNERSHIP,
                    "线程归属记录暂不可用",
                )));
            }
            return Err(End::Again);
        }
        if self.plan.unknown_affinity {
            self.plan.pinned = Some(route.provider.id.clone());
            self.wait_budget.pin_provider();
        }
        self.ids.retain(|id| id != &route.provider.id);
        Ok(route)
    }
    /// Counts the attempt and prepares the protocol observer for it.
    fn begin(
        &mut self,
        mut admission: Admission,
        route: Route,
        upstream_model: Option<String>,
        new_thread_model_denied: bool,
    ) -> Attempt {
        let reset_generation = admission.reset_generation;
        admission.commit_rpm();
        self.attempted = self.attempted.saturating_add(1);
        self.ordinary_attempts = self.ordinary_attempts.saturating_add(1);
        let rerouted = self
            .previous_provider
            .as_deref()
            .is_some_and(|id| id != admission.route.provider.id);
        self.previous_provider = Some(admission.route.provider.id.clone());
        let started = Instant::now();
        let mut protocol = super::protocol::Protocol::new(self.plan.stream_hint);
        protocol.attach_wait_budget(self.request_budget);
        protocol.attach_failure_rules(&self.settings.custom_failure_rules);
        self.request_budget.describe(
            Some(&route.provider.id),
            self.plan.model.as_deref(),
            "headers",
        );
        protocol.set_operation(self.plan.operation);
        protocol.diagnostics(self.gateway, &route.provider.id, self.plan.model.as_deref());
        protocol.attach_ownership(self.plan.ownership.clone(), &route.provider.id);
        protocol.attach_routing_log(self.failover.routing());
        protocol.request_headers(&self.parts.headers);
        if !routing::resource(&self.parts.method, self.parts.uri.path()) {
            protocol.business_request(self.gateway, &self.parts.headers);
        }
        protocol.request_speed(self.plan.hints.as_ref().ok().and_then(|h| h.speed.clone()));
        protocol.thread_identity(self.plan.thread_identity);
        if let Some(at) = self.parts.extensions.get::<ReceivedAt>() {
            protocol.request_received(at.0);
        }
        if let Some(delivery) = self
            .parts
            .extensions
            .get::<std::sync::Arc<super::protocol::Delivery>>()
        {
            protocol.set_delivery(delivery.clone());
        }
        if let Some(trace) = &self.plan.usage_trace {
            let mut attempt = trace.attempt(
                &route.provider.id,
                self.plan.stream_hint,
                if self.websocket { "websocket" } else { "http" },
            );
            attempt.upstream_model(upstream_model.clone());
            protocol.attach_usage(attempt);
        }
        protocol.context(
            self.plan.thread.clone(),
            (self.gateway.client_id() == super::ClientId::Codex
                && self.mode == "auto"
                && (self.plan.responses
                    || (self.plan.owned_search && self.plan.ownership.is_some())))
            .then(|| {
                admission
                    .handoff_wait
                    .or(
                        (new_thread_model_denied && !route.provider.take_new_threads)
                            .then_some(crate::usage::model::RoutingDecision::NewThreadModelDenied),
                    )
                    .unwrap_or_else(|| {
                        super::compaction::decision(
                            self.plan.thread.as_deref(),
                            self.plan.hints.as_ref().ok(),
                            self.plan.continuation,
                            self.plan.ownership.as_deref(),
                            &route.provider.id,
                        )
                    })
            }),
        );
        if self.gateway.client_id() == super::ClientId::Codex
            && self.mode == "auto"
            && (self.plan.responses || (self.plan.owned_search && self.plan.ownership.is_some()))
        {
            protocol.routing_evidence(self.plan.hints.as_ref().ok());
        }
        protocol.watch_lifecycle(
            self.gateway
                .request_state()
                .shutdown
                .as_ref()
                .map(watch::Sender::subscribe),
            None,
        );
        let search_deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        Attempt {
            admission,
            route,
            protocol,
            reset_generation,
            rerouted,
            started,
            search_deadline,
        }
    }
    /// The request never got a response head: the connection failed.
    async fn unreached(
        &mut self,
        attempt: Attempt,
        e: &(dyn std::error::Error + Send + Sync + 'static),
    ) -> End {
        let Attempt {
            mut admission,
            route,
            mut protocol,
            reset_generation,
            ..
        } = attempt;
        self.failover.failure(super::failover::Failure::Transport);
        let category = connector::classify(e);
        self.last_category = match category {
            Some(connector::ConnectError::Tls) => code::TLS,
            _ => code::NETWORK,
        };
        // Use the same owner and budget as 5xx for connection failures,
        // including a peer closing before response headers arrive.
        // Header timeouts still use the separate no-replay boundary.
        let retry_same = !self.plan.dedicated_search
            && matches!(
                category,
                None | Some(
                    connector::ConnectError::TargetConnect | connector::ConnectError::TargetTimeout
                )
            )
            && !self.plan.unknown_affinity
            && self.ordinary_attempts <= self.settings.max_retries
            && ((route.client_id == super::ClientId::Codex
                && (self.plan.pinned.as_deref() == Some(&route.provider.id)
                    || self
                        .plan
                        .ownership
                        .as_ref()
                        .is_some_and(|lease| lease.owns(&route.provider.id))))
                || self.plan.fixed_dsh.as_deref() == Some(&route.provider.id));
        protocol.finish(None, self.last_category);
        let mut event = admission.permits.prepare_report(
            self.gateway,
            &route,
            self.plan.model.as_deref(),
            Reason::Network,
            super::attempt::retained_fallback(
                self.gateway,
                &route.provider.id,
                &self.ids,
                &self.routes,
                &self.plan.requirement,
                self.parts.uri.path(),
                self.plan.ownership.as_deref(),
                self.gateway.client_id() == super::ClientId::Codex && !self.plan.auxiliary,
                super::attempt::may_fail_over(
                    self.mode == "manual",
                    self.plan.pinned.as_deref(),
                    self.plan.unknown_affinity,
                    self.ordinary_attempts,
                    self.settings.max_retries,
                ),
            ),
            None,
            self.attempted,
            crate::events::Details {
                phase: Some(crate::events::Phase::Connect),
                local_code: Some(connector::diagnostic_code(e).into()),
                counted_failure: Some(true),
                ..Default::default()
            },
        );
        if self.plan.dedicated_search {
            admission.permits.neutral(self.settings);
        } else {
            admission.permits.failure(self.settings, None);
        }
        let retry_delay = super::attempt::same_provider_delay(
            retry_same,
            &route.health_for(&self.plan.requirement),
            &mut self.transient_retries,
            None,
        );
        if let Some(delay) = retry_delay {
            event.action = Action::RetryingSame;
            event.details.wait_seconds = Some(delay.as_secs());
        }
        self.last_error = Some(self.failover.report_failure(Some(&protocol), || {
            super::health_settlement::publish(self.gateway, event)
        }));
        if let Some(delay) = retry_delay {
            self.failover
                .wait(super::failover::WaitKind::TransientRetry, delay.as_secs());
            drop(admission);
            match transient_wait(
                self.gateway,
                &route,
                reset_generation,
                delay,
                self.settings.max_waiting,
                self.plan.pinned.as_deref(),
                &self.plan.requirement,
            )
            .await
            {
                Ok(true) => {
                    self.ids.insert(0, route.provider.id.clone());
                    self.retry_owner = Some(route.provider.id.clone());
                }
                Ok(false) => {}
                Err(Rejected::Stopped) => {
                    self.failover.finish(super::failover::Exit::Stopped);
                    return End::reply(stopped());
                }
                Err(_) => return End::Stop,
            }
        }
        End::Again
    }
    /// No response head arrived in time.
    fn headers_timeout(&mut self, attempt: Attempt) -> End {
        let Attempt {
            mut admission,
            route,
            mut protocol,
            ..
        } = attempt;
        self.failover.failure(super::failover::Failure::Transport);
        let retry_dsh = self.plan.dedicated_search
            || route.client_id == super::ClientId::Dsh
                && super::attempt::retained_fallback(
                    self.gateway,
                    &route.provider.id,
                    &self.ids,
                    &self.routes,
                    &self.plan.requirement,
                    self.parts.uri.path(),
                    self.plan.ownership.as_deref(),
                    false,
                    super::attempt::may_fail_over(
                        self.mode == "manual",
                        self.plan.pinned.as_deref(),
                        self.plan.unknown_affinity,
                        self.ordinary_attempts,
                        self.settings.max_retries,
                    ),
                ) == Action::TryingNext;
        protocol.timeout(
            "response_headers",
            if self.plan.dedicated_search {
                30
            } else {
                self.settings.first_byte_seconds
            },
        );
        protocol.finish(None, code::FIRST_BYTE_TIMEOUT);
        self.last_error = Some(self.failover.report_failure(Some(&protocol), || {
            admission.permits.report(
                self.gateway,
                &route,
                self.plan.model.as_deref(),
                Reason::Network,
                if retry_dsh {
                    Action::TryingNext
                } else {
                    Action::Returned
                },
                None,
                self.attempted,
                crate::events::Details {
                    phase: Some(crate::events::Phase::Headers),
                    local_code: Some(code::FIRST_BYTE_TIMEOUT.into()),
                    counted_failure: Some(true),
                    ..protocol.observation.error.clone()
                },
            )
        }));
        if self.plan.dedicated_search {
            admission.permits.neutral(self.settings);
        } else {
            admission.permits.failure(self.settings, None);
        }
        if retry_dsh {
            self.last_category = code::FIRST_BYTE_TIMEOUT;
            return End::Again;
        }
        self.failover.finish(super::failover::Exit::ReplayUnsafe);
        // The HTTP client already owns the request. Not seeing headers
        // cannot establish that its body never reached the provider.
        End::reply(error(
            StatusCode::GATEWAY_TIMEOUT,
            code::FIRST_BYTE_TIMEOUT,
            "上游首字节超时，请求可能已开始处理，未自动重放",
        ))
    }
    /// The provider accepted a WebSocket upgrade: joins the two sockets.
    fn tunnel(mut self, attempt: Attempt, mut response: Response<Incoming>) -> Response<WireBody> {
        let Attempt {
            mut admission,
            route,
            mut protocol,
            rerouted,
            ..
        } = attempt;
        let status = response.status();
        let (active, traffic) = (self.active, self.traffic);
        self.failover.finish(super::failover::Exit::Forwarded);
        admission.permits.success(self.settings);
        let Admission { permits, slot, .. } = admission;
        let upstream_upgrade = hyper::upgrade::on(&mut response);
        let (mut response_parts, _) = response.into_parts();
        clean_headers(&mut response_parts.headers, true);
        let downstream = self.downstream_upgrade.expect("upgrade exists");
        let g = self.gateway.clone();
        let cfg = self.settings.clone();
        let provider_for_tunnel = route.provider.clone();
        tokio::spawn(async move {
            let _active = active;
            let _traffic = traffic;
            let _slot = slot;
            let mut permits = permits;
            let connected = tokio::try_join!(upstream_upgrade, downstream);
            if let Ok((a, b)) = connected {
                // Opaque tunnel preserves every data/control/close frame, including binary payloads.
                let result =
                    tokio::io::copy_bidirectional(&mut TokioIo::new(a), &mut TokioIo::new(b)).await;
                protocol.finish(
                    Some(101),
                    if result.is_ok() {
                        code::OK
                    } else {
                        code::STREAM_INTERRUPTED
                    },
                );
                if protocol.succeeded() {
                    g.persist_success(&provider_for_tunnel).await;
                }
                permits.neutral(&cfg);
            }
        });
        if rerouted {
            self.gateway.record(
                Some(&route.provider.id),
                self.plan.model.as_deref(),
                Reason::Failover,
                Action::Routed,
                Some(status.as_u16()),
                Some(self.attempted.min(u32::MAX as usize) as u32),
            );
        }
        Response::from_parts(response_parts, replay::empty())
    }
    /// The provider answered with an error status.
    async fn rejected(&mut self, attempt: Attempt, response: Response<Incoming>) -> End {
        let Attempt {
            mut admission,
            route,
            mut protocol,
            reset_generation,
            ..
        } = attempt;
        let status = response.status();
        let cooldown = response
            .headers()
            .get(header::RETRY_AFTER)
            .and_then(|h| h.to_str().ok())
            .and_then(circuit::retry_after);
        let (mut response_parts, body) = response.into_parts();
        clean_headers(&mut response_parts.headers, false);
        let captured = Replay::capture_observed(
            body,
            self.gateway.0.spool.path(),
            Some(Duration::from_secs(self.settings.idle_seconds)),
            |bytes| protocol.feed(bytes),
        )
        .await;
        if captured
            .as_ref()
            .err()
            .and_then(|e| e.downcast_ref::<std::io::Error>())
            .is_some_and(|e| e.kind() == std::io::ErrorKind::TimedOut)
        {
            protocol.timeout("error_body_idle", self.settings.idle_seconds);
        }
        let decoded = match &captured {
            Ok(body) => {
                let encoded = body.prefix(2 * 1024 * 1024).await.unwrap_or_default();
                protocol.finish(Some(status.as_u16()), code::HTTP);
                let encoding = response_parts
                    .headers
                    .get(header::CONTENT_ENCODING)
                    .and_then(|h| h.to_str().ok())
                    .unwrap_or("identity");
                replay::decode_prefix(&encoded, encoding, 128 * 1024).unwrap_or_default()
            }
            _ => {
                protocol.finish(Some(status.as_u16()), code::STREAM_INTERRUPTED);
                Vec::new()
            }
        };
        let (classified, server_retry) = super::failure_policy::http_with_rules(
            status.as_u16(),
            &response_parts.headers,
            &decoded,
            &self.settings.custom_failure_rules,
        );
        let mut cooldown = cooldown
            .max(server_retry)
            .max(protocol.observation.retry_after);
        let compact_error = self.plan.ownership.as_ref().is_some_and(|lease| {
            lease.handoff && lease.owner.as_deref() != Some(&route.provider.id)
        }) && protocol.observation.compaction_incompatible;
        if compact_error {
            if let Some(owner) = self
                .plan
                .ownership
                .as_ref()
                .and_then(|lease| lease.owner.as_ref())
                .filter(|owner| self.ids.contains(owner))
            {
                self.retry_owner = Some(owner.clone());
            }
        }
        let model_error = protocol.observation.terminal
            == Some(super::protocol::Terminal::ModelUnavailable)
            || super::failure_policy::model_http(status.as_u16(), &decoded);
        let mut details = if protocol.observation.error.raw.is_some() {
            protocol.observation.error.clone()
        } else {
            classified
        };
        if details == crate::events::Details::default() {
            details = protocol.observation.error.clone();
        }
        if route.client_id == super::ClientId::Dsh
            && !model_error
            && matches!(status.as_u16(), 401..=403)
        {
            details.health_scope = Some(crate::events::HealthScope::Provider);
        }
        let policy = super::failure_policy::decision(Some(status.as_u16()), &details);
        let hard_rejection = policy.hard;
        if hard_rejection {
            self.hard_denied
                .insert((route.provider.id.clone(), route.provider.version.clone()));
        }
        let capacity = !model_error && !hard_rejection && policy.temporary;
        let wait_same =
            capacity && self.plan.established_capacity_owner.as_deref() == Some(&route.provider.id);
        if capacity && !self.websocket && !self.plan.dedicated_search {
            self.ordinary_attempts = self.ordinary_attempts.saturating_sub(1);
            self.capacity_protected = true;
            self.wait_budget.protect_capacity();
        }
        let retryable = compact_error || model_error || hard_rejection || policy.retryable;
        let retry_same = circuit::is_server_error(status.as_u16())
            && !compact_error
            && !model_error
            && !capacity
            && !hard_rejection
            && !self.plan.unknown_affinity
            && self.ordinary_attempts <= self.settings.max_retries
            && ((route.client_id == super::ClientId::Codex
                && (self.plan.pinned.as_deref() == Some(&route.provider.id)
                    || self
                        .plan
                        .ownership
                        .as_ref()
                        .is_some_and(|lease| lease.owns(&route.provider.id))))
                || self.plan.fixed_dsh.as_deref() == Some(&route.provider.id));
        self.failover.failure(if hard_rejection {
            super::failover::Failure::AccountRejected
        } else if model_error {
            super::failover::Failure::ModelUnavailable
        } else if status == StatusCode::TOO_MANY_REQUESTS || capacity {
            super::failover::Failure::RateLimit
        } else if compact_error {
            super::failover::Failure::Protocol
        } else {
            super::failover::Failure::UpstreamService
        });
        let will_retry = retryable
            && ((self.plan.dedicated_search
                && self
                    .ids
                    .iter()
                    .any(|id| self.routes.get(id).is_some_and(|r| r.provider.search_only)))
                || super::attempt::retained_fallback(
                    self.gateway,
                    &route.provider.id,
                    &self.ids,
                    &self.routes,
                    &self.plan.requirement,
                    self.parts.uri.path(),
                    self.plan.ownership.as_deref(),
                    self.gateway.client_id() == super::ClientId::Codex && !self.plan.auxiliary,
                    super::attempt::may_fail_over(
                        self.mode == "manual",
                        self.plan.pinned.as_deref(),
                        self.plan.unknown_affinity,
                        self.ordinary_attempts,
                        self.settings.max_retries,
                    ),
                ) == Action::TryingNext);
        let reason = if compact_error {
            Reason::ProtocolError
        } else {
            policy.reason
        };
        details.phase = Some(crate::events::Phase::Response);
        details.counted_failure = Some(!compact_error && policy.counted_failure);
        details.wait_seconds =
            cooldown.map(|d| d.as_secs().saturating_add(u64::from(d.subsec_nanos() > 0)));
        protocol.finish(
            Some(status.as_u16()),
            if captured.is_ok() {
                code::HTTP
            } else {
                code::STREAM_INTERRUPTED
            },
        );
        let mut event = admission.permits.prepare_report(
            self.gateway,
            &route,
            self.plan.model.as_deref(),
            reason,
            if wait_same {
                Action::RetryingSame
            } else if will_retry {
                Action::TryingNext
            } else if capacity {
                Action::Waiting
            } else {
                Action::Returned
            },
            Some(status.as_u16()),
            self.attempted,
            details,
        );
        if model_error || compact_error {
            admission.permits.neutral(self.settings);
        } else if capacity {
            admission.permits.capacity_limited(self.settings, cooldown);
        } else if hard_rejection {
            admission.permits.failure(self.settings, cooldown);
        } else if status == StatusCode::TOO_MANY_REQUESTS {
            admission.permits.rate_limited(self.settings, cooldown);
        } else if retryable {
            admission.permits.failure(self.settings, cooldown);
        } else {
            admission.permits.neutral(self.settings);
        }
        let retry_delay = super::attempt::same_provider_delay(
            retry_same,
            &route.health_for(&self.plan.requirement),
            &mut self.transient_retries,
            cooldown,
        );
        if let Some(delay) = retry_delay {
            event.action = Action::RetryingSame;
            event.details.wait_seconds = Some(delay.as_secs());
        }
        self.last_error = Some(self.failover.report_failure(Some(&protocol), || {
            super::health_settlement::publish(self.gateway, event)
        }));
        if capacity {
            cooldown = cooldown.max(Some(Duration::from_secs(
                route.health_for(&self.plan.requirement).retry_in,
            )));
        }
        if !retryable {
            self.ids.clear();
        }
        if let Ok(body) = captured {
            self.last = Some(Response::from_parts(response_parts, body.body()));
        }
        self.last_category = code::HTTP;
        if !retryable {
            self.failover
                .finish(super::failover::Exit::PermanentRejection);
            return End::reply(self.last.take().unwrap_or_else(|| {
                error(
                    StatusCode::BAD_GATEWAY,
                    code::UPSTREAM_BODY,
                    "上游错误响应读取失败",
                )
            }));
        }
        if let Some(delay) = retry_delay {
            self.failover
                .wait(super::failover::WaitKind::TransientRetry, delay.as_secs());
            // Release the failed attempt before registering a bounded wait.
            // Keep the original spool, model, and response cursor untouched.
            drop(admission);
            self.request_budget
                .describe(None, self.plan.model.as_deref(), "retry");
            match transient_wait(
                self.gateway,
                &route,
                reset_generation,
                delay,
                self.settings.max_waiting,
                self.plan.pinned.as_deref(),
                &self.plan.requirement,
            )
            .await
            {
                Ok(true) => {
                    self.ids.insert(0, route.provider.id.clone());
                    self.retry_owner = Some(route.provider.id.clone());
                }
                Ok(false) => {}
                Err(Rejected::Stopped) => {
                    self.failover.finish(super::failover::Exit::Stopped);
                    return End::reply(stopped());
                }
                Err(_) => return End::Stop,
            }
        }
        if capacity && !self.plan.dedicated_search {
            if wait_same {
                self.capacity_wait_owner = Some(route.provider.id.clone());
                self.ids.clear();
            }
            self.capacity_pending = true;
            self.capacity_sources.push(CapacitySource {
                provider_id: route.provider.id.clone(),
                reset_generation,
            });
            self.capacity_retry_after = self.capacity_retry_after.max(cooldown);
        }
        End::Again
    }
    /// Reads the start of an accepted response until it shows output, a
    /// terminal outcome or a failure.
    async fn opening(
        &mut self,
        attempt: &mut Attempt,
        response: Response<Incoming>,
        stream: bool,
    ) -> Opening {
        let status = response.status();
        let protocol = &mut attempt.protocol;
        let (started, search_deadline) = (attempt.started, attempt.search_deadline);
        let (mut response_parts, body) = response.into_parts();
        let mut body = body
            .map_err(|error| Box::new(error) as BoxError)
            .boxed_unsync();
        clean_headers(&mut response_parts.headers, false);
        let mut first_deadline = tokio::time::Instant::now()
            + Duration::from_secs(self.settings.first_byte_seconds)
                .saturating_sub(started.elapsed());
        if self.plan.dedicated_search {
            first_deadline = search_deadline;
        }
        // Buffer through initialization and heartbeats until effective output or a
        // terminal outcome (or bounded JSON); preserve
        // the original frames and encoding exactly, including comments/trailers.
        let json = response_parts
            .headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.to_ascii_lowercase().contains("json"));
        let mut preinspected = false;
        let mut inspected_bytes = 0u64;
        let mut prefix = Vec::new();
        let mut prefix_bytes = 0;
        let mut ended = false;
        let mut failed = false;
        let mut failure_kind = code::FIRST_BYTE_TIMEOUT;
        loop {
            match self
                .request_budget
                .response(
                    first_deadline,
                    self.plan.operation == crate::usage::model::Operation::Compaction,
                    body.frame(),
                )
                .await
            {
                Ok(Some(Ok(frame))) => {
                    if let Some(bytes) = frame.data_ref() {
                        prefix_bytes += bytes.len();
                        protocol.feed(bytes);
                        if !stream
                            && !json
                            && !bytes.is_empty()
                            && !protocol.possible_bare_capacity()
                        {
                            protocol.effective_output();
                        }
                        if !self.plan.dedicated_search && !bytes.is_empty() {
                            first_deadline = tokio::time::Instant::now()
                                + Duration::from_secs(self.settings.idle_seconds);
                        }
                    }
                    prefix.push((
                        frame,
                        protocol.terminal().is_some(),
                        protocol.business_observed(),
                    ));
                    if body.is_end_stream() {
                        ended = true;
                    }
                    if !ended
                        && (prefix_bytes >= 2 * 1024 * 1024 || prefix.len() >= 4096)
                        && !protocol.search_content_observed()
                        && protocol.terminal().is_none()
                    {
                        let frames = std::mem::take(&mut prefix);
                        let mut observed = prefix_bytes;
                        let captured = {
                            let upstream = &mut body;
                            let remainder = async_stream::try_stream! {
                                for (frame, _, _) in frames { yield frame; }
                                while let Some(frame) = upstream.frame().await { yield frame.map_err(std::io::Error::other)?; }
                            };
                            let mut remainder = StreamBody::new(Box::pin(remainder))
                                .map_err(|error: std::io::Error| error);
                            Replay::capture_until(
                                &mut remainder,
                                self.gateway.0.spool.path(),
                                Some(Duration::from_secs(self.settings.idle_seconds)),
                                |bytes| {
                                    if observed > 0 {
                                        observed = observed.saturating_sub(bytes.len());
                                    } else {
                                        protocol.feed(bytes);
                                    }
                                    stream
                                        && (protocol.search_content_observed()
                                            || protocol.terminal().is_some())
                                },
                            )
                            .await
                        };
                        ended = body.is_end_stream();
                        match captured {
                            Ok(spooled) => {
                                inspected_bytes = spooled.length;
                                let mut buffered = spooled.body();
                                let mut upstream = body;
                                body = StreamBody::new(async_stream::try_stream! {
                                    while let Some(frame) = buffered.frame().await { yield frame?; }
                                    while let Some(frame) = upstream.frame().await { yield frame?; }
                                })
                                .boxed_unsync();
                                preinspected = true;
                            }
                            Err(_) => {
                                failed = true;
                                failure_kind = code::UPSTREAM_BODY;
                            }
                        }
                        break;
                    }
                    if ended
                        || (!stream && !json && !protocol.possible_bare_capacity())
                        || (stream
                            && (protocol.search_content_observed()
                                || protocol.terminal().is_some()))
                    {
                        break;
                    }
                }
                Ok(None) => {
                    ended = true;
                    break;
                }
                result => {
                    failed = true;
                    failure_kind = if result.is_err() && prefix_bytes != 0 {
                        code::STREAM_TIMEOUT
                    } else if result.is_err() {
                        code::FIRST_BYTE_TIMEOUT
                    } else {
                        code::STREAM_INTERRUPTED
                    };
                    break;
                }
            }
        }
        if failed {
            if failure_kind.ends_with("TIMEOUT") {
                protocol.timeout(
                    if prefix_bytes == 0 {
                        "first_byte"
                    } else {
                        "first_event_idle"
                    },
                    if prefix_bytes == 0 {
                        self.settings.first_byte_seconds
                    } else {
                        self.settings.idle_seconds
                    },
                );
            }
            protocol.finish(Some(status.as_u16()), failure_kind);
        }
        Opening {
            status,
            parts: response_parts,
            body,
            prefix,
            prefix_bytes,
            ended,
            failed,
            failure_kind,
            preinspected,
            inspected_bytes,
        }
    }
    /// The response broke off before any output, where a replay is still safe.
    async fn interrupted(
        &mut self,
        attempt: Attempt,
        status: StatusCode,
        failure_kind: &'static str,
    ) -> End {
        let Attempt {
            mut admission,
            route,
            protocol,
            reset_generation,
            ..
        } = attempt;
        self.failover.failure(super::failover::Failure::Transport);
        let fallback = super::attempt::retained_fallback(
            self.gateway,
            &route.provider.id,
            &self.ids,
            &self.routes,
            &self.plan.requirement,
            self.parts.uri.path(),
            self.plan.ownership.as_deref(),
            self.gateway.client_id() == super::ClientId::Codex && !self.plan.auxiliary,
            super::attempt::may_fail_over(
                self.mode == "manual",
                self.plan.pinned.as_deref(),
                self.plan.unknown_affinity,
                self.ordinary_attempts,
                self.settings.max_retries,
            ),
        );
        let mut event = admission.permits.prepare_report(
            self.gateway,
            &route,
            self.plan.model.as_deref(),
            Reason::Network,
            fallback,
            Some(status.as_u16()),
            self.attempted,
            crate::events::Details {
                phase: Some(crate::events::Phase::Response),
                local_code: Some(failure_kind.into()),
                counted_failure: Some(true),
                ..protocol.observation.error.clone()
            },
        );
        if self.plan.dedicated_search {
            admission.permits.neutral(self.settings);
        } else {
            admission.permits.failure(self.settings, None);
        }
        let retry_delay = if failure_kind == code::STREAM_INTERRUPTED
            && self.plan.fixed_dsh.as_deref() == Some(&route.provider.id)
            && !self.plan.unknown_affinity
            && self.ordinary_attempts <= self.settings.max_retries
            && route.health_for(&self.plan.requirement).available
        {
            routing::transient_delay(&mut self.transient_retries, None)
        } else {
            None
        };
        if let Some(delay) = retry_delay {
            event.action = Action::RetryingSame;
            event.details.wait_seconds = Some(delay.as_secs());
        }
        self.last_error = Some(self.failover.report_failure(Some(&protocol), || {
            super::health_settlement::publish(self.gateway, event)
        }));
        self.last_category = failure_kind;
        if let Some(delay) = retry_delay {
            self.failover
                .wait(super::failover::WaitKind::TransientRetry, delay.as_secs());
            drop(admission);
            match transient_wait(
                self.gateway,
                &route,
                reset_generation,
                delay,
                self.settings.max_waiting,
                self.plan.pinned.as_deref(),
                &self.plan.requirement,
            )
            .await
            {
                Ok(true) => {
                    self.ids.insert(0, route.provider.id.clone());
                    self.retry_owner = Some(route.provider.id.clone());
                }
                Ok(false) => {}
                Err(Rejected::Stopped) => {
                    self.failover.finish(super::failover::Exit::Stopped);
                    return End::reply(stopped());
                }
                Err(_) => return End::Stop,
            }
        } else if route.client_id == super::ClientId::Dsh && fallback != Action::TryingNext {
            self.failover.finish(super::failover::Exit::ReplayUnsafe);
            return End::reply(error(
                if failure_kind.ends_with("TIMEOUT") {
                    StatusCode::GATEWAY_TIMEOUT
                } else {
                    StatusCode::BAD_GATEWAY
                },
                failure_kind,
                if failure_kind.ends_with("TIMEOUT") {
                    "上游响应超时"
                } else {
                    "上游响应流中断"
                },
            ));
        }
        End::Again
    }
    /// Settles a response whose start is itself a rejection; any other
    /// response goes on to the client.
    async fn initial_rejection(
        &mut self,
        attempt: Attempt,
        opening: &mut Opening,
    ) -> Result<Attempt, End> {
        let Attempt {
            mut admission,
            route,
            mut protocol,
            reset_generation,
            rerouted,
            started,
            search_deadline,
        } = attempt;
        let (status, failed) = (opening.status, opening.failed);
        opening.ended |= failed;
        if opening.ended {
            protocol.finish(
                Some(status.as_u16()),
                if status.is_success() {
                    code::OK
                } else {
                    code::HTTP
                },
            );
            if protocol.terminal_observed() {
                if let Some((_, terminal, business)) = opening.prefix.last_mut() {
                    *terminal = true;
                    *business = true;
                }
            }
        }
        let before_output = !protocol.observation.meter.content_seen
            && !protocol.observation.search_content
            && !protocol.observation.opaque_output;
        let initial_model_error = before_output
            && protocol.terminal() == Some(super::protocol::Terminal::ModelUnavailable);
        let initial_compact_error = protocol.observation.compaction_incompatible
            && self.plan.ownership.as_ref().is_some_and(|lease| {
                lease.handoff && lease.owner.as_deref() != Some(&route.provider.id)
            });
        if initial_compact_error {
            if let Some(owner) = self
                .plan
                .ownership
                .as_ref()
                .and_then(|lease| lease.owner.as_ref())
                .filter(|owner| self.ids.contains(owner))
            {
                self.retry_owner = Some(owner.clone());
            }
        }
        let policy =
            super::failure_policy::decision(Some(status.as_u16()), &protocol.observation.error);
        let initial_hard_rejection = before_output && policy.hard;
        if initial_hard_rejection {
            self.hard_denied
                .insert((route.provider.id.clone(), route.provider.version.clone()));
        }
        let initial_capacity = before_output
            && !initial_hard_rejection
            && !self.websocket
            && !initial_model_error
            && policy.temporary;
        // This whole prefix is still private to the gateway. A body frame
        // can contain both generated deltas and a terminal service error;
        // observing those deltas is not proof that the client received them.
        let initial_service_error = (before_output || protocol.explicit_failure())
            && protocol.observation.error.error_basis
                == Some(crate::events::ErrorBasis::ServiceError);
        let initial_other_error = before_output && !failed && protocol.explicit_failure();
        let wait_same = initial_capacity
            && self.plan.established_capacity_owner.as_deref() == Some(&route.provider.id);
        if initial_model_error
            || initial_capacity
            || initial_compact_error
            || initial_hard_rejection
            || initial_service_error
            || initial_other_error
        {
            self.failover.failure(if initial_hard_rejection {
                super::failover::Failure::AccountRejected
            } else if initial_capacity {
                super::failover::Failure::RateLimit
            } else if initial_model_error {
                super::failover::Failure::ModelUnavailable
            } else if initial_service_error {
                super::failover::Failure::UpstreamService
            } else {
                super::failover::Failure::Protocol
            });
            let mut cooldown = opening
                .parts
                .headers
                .get(header::RETRY_AFTER)
                .and_then(|h| h.to_str().ok())
                .and_then(circuit::retry_after)
                .max(protocol.observation.retry_after);
            if initial_capacity && !self.plan.dedicated_search {
                if wait_same {
                    self.capacity_wait_owner = Some(route.provider.id.clone());
                    self.ids.clear();
                }
                self.ordinary_attempts = self.ordinary_attempts.saturating_sub(1);
                self.capacity_protected = true;
                self.wait_budget.protect_capacity();

                self.capacity_pending = true;
                self.capacity_sources.push(CapacitySource {
                    provider_id: route.provider.id.clone(),
                    reset_generation,
                });
                self.capacity_retry_after = self.capacity_retry_after.max(cooldown);
            }
            protocol.finish(Some(status.as_u16()), code::HTTP);
            let retry_same = initial_service_error
                && !self.plan.unknown_affinity
                && self.ordinary_attempts <= self.settings.max_retries
                && (self.plan.pinned.as_deref() == Some(&route.provider.id)
                    || self
                        .plan
                        .ownership
                        .as_ref()
                        .is_some_and(|lease| lease.owns(&route.provider.id))
                    || self.plan.fixed_dsh.as_deref() == Some(&route.provider.id));
            let will_retry = policy.retryable
                && ((self.plan.dedicated_search
                    && self
                        .ids
                        .iter()
                        .any(|id| self.routes.get(id).is_some_and(|r| r.provider.search_only)))
                    || super::attempt::retained_fallback(
                        self.gateway,
                        &route.provider.id,
                        &self.ids,
                        &self.routes,
                        &self.plan.requirement,
                        self.parts.uri.path(),
                        self.plan.ownership.as_deref(),
                        self.gateway.client_id() == super::ClientId::Codex && !self.plan.auxiliary,
                        super::attempt::may_fail_over(
                            self.mode == "manual",
                            self.plan.pinned.as_deref(),
                            self.plan.unknown_affinity,
                            self.ordinary_attempts,
                            self.settings.max_retries,
                        ),
                    ) == Action::TryingNext);
            let mut details = protocol.observation.error.clone();
            details.phase = Some(crate::events::Phase::Response);
            details.counted_failure = Some(!initial_compact_error && policy.counted_failure);
            details.wait_seconds =
                cooldown.map(|d| d.as_secs().saturating_add(u64::from(d.subsec_nanos() > 0)));
            let mut event = admission.permits.prepare_report(
                self.gateway,
                &route,
                self.plan.model.as_deref(),
                if initial_compact_error {
                    Reason::ProtocolError
                } else {
                    policy.reason
                },
                if wait_same {
                    Action::RetryingSame
                } else if will_retry {
                    Action::TryingNext
                } else if initial_capacity {
                    Action::Waiting
                } else {
                    Action::Returned
                },
                Some(status.as_u16()),
                self.attempted,
                details,
            );
            if initial_capacity {
                admission.permits.capacity_limited(self.settings, cooldown);
            } else if initial_hard_rejection || initial_service_error {
                admission.permits.failure(self.settings, cooldown);
            } else {
                admission.permits.neutral(self.settings);
            }
            let retry_delay = super::attempt::same_provider_delay(
                retry_same,
                &route.health_for(&self.plan.requirement),
                &mut self.transient_retries,
                cooldown,
            );
            if let Some(delay) = retry_delay {
                event.action = Action::RetryingSame;
                event.details.wait_seconds = Some(delay.as_secs());
            }
            self.last_error = Some(self.failover.report_failure(Some(&protocol), || {
                super::health_settlement::publish(self.gateway, event)
            }));
            if initial_capacity {
                cooldown = cooldown.max(Some(Duration::from_secs(
                    route.health_for(&self.plan.requirement).retry_in,
                )));
                self.capacity_retry_after = self.capacity_retry_after.max(cooldown);
            }
            // Rejected initialization frames are never a successful client response.
            // The upstream payload remains available in the error log.
            self.last = Some(error(
                StatusCode::BAD_GATEWAY,
                code::UPSTREAM_REJECTED,
                "上游业务请求失败，已结束本次重试",
            ));
            if !policy.retryable && !initial_compact_error {
                self.failover
                    .finish(super::failover::Exit::PermanentRejection);
                return Err(End::reply(self.last.take().unwrap()));
            }
            if let Some(delay) = retry_delay {
                self.failover
                    .wait(super::failover::WaitKind::TransientRetry, delay.as_secs());
                // Release the failed attempt before registering a bounded wait.
                // Keep the original spool, model, and response cursor untouched.
                drop(admission);
                self.request_budget
                    .describe(None, self.plan.model.as_deref(), "retry");
                match transient_wait(
                    self.gateway,
                    &route,
                    reset_generation,
                    delay,
                    self.settings.max_waiting,
                    self.plan.pinned.as_deref(),
                    &self.plan.requirement,
                )
                .await
                {
                    Ok(true) => {
                        self.ids.insert(0, route.provider.id.clone());
                        self.retry_owner = Some(route.provider.id.clone());
                    }
                    Ok(false) => {}
                    Err(Rejected::Stopped) => {
                        self.failover.finish(super::failover::Exit::Stopped);
                        return Err(End::reply(stopped()));
                    }
                    Err(_) => return Err(End::Stop),
                }
            } else if !will_retry && !initial_capacity {
                self.ids.clear();
            }
            self.last_category = if initial_hard_rejection || initial_service_error {
                code::UPSTREAM_REJECTED
            } else if initial_capacity {
                code::CAPACITY
            } else if initial_model_error {
                code::MODEL_UNAVAILABLE
            } else {
                code::UPSTREAM_REJECTED
            };
            return Err(End::Again);
        }
        Ok(Attempt {
            admission,
            route,
            protocol,
            reset_generation,
            rerouted,
            started,
            search_deadline,
        })
    }
    /// Starts the relay task for an accepted response and returns its head.
    fn hand_over(mut self, attempt: Attempt, opening: Opening) -> Response<WireBody> {
        let Attempt {
            admission,
            route,
            protocol,
            rerouted,
            ..
        } = attempt;
        let Opening {
            status,
            parts: response_parts,
            body,
            prefix,
            mut ended,
            failed,
            failure_kind,
            preinspected,
            inspected_bytes,
            ..
        } = opening;
        if preinspected {
            ended = false;
        }
        let replay_unsafe = failed && failure_kind == code::FIRST_BYTE_TIMEOUT;
        if replay_unsafe {
            self.failover.failure(super::failover::Failure::Transport);
            protocol.associate_failure(self.failover.failure_generation());
        }
        self.failover.finish(if replay_unsafe {
            super::failover::Exit::ReplayUnsafe
        } else {
            super::failover::Exit::Forwarded
        });
        let Admission { permits, slot, .. } = admission;
        let g = self.gateway.clone();
        let cfg = self.settings.clone();
        let neutral = status.as_u16() >= 400;
        if rerouted {
            self.gateway.record(
                Some(&route.provider.id),
                self.plan.model.as_deref(),
                Reason::Failover,
                Action::Routed,
                Some(status.as_u16()),
                Some(self.attempted.min(u32::MAX as usize) as u32),
            );
        }
        let delivery = protocol.delivery();
        let output_delivery = delivery.clone();
        let activity = self
            .parts
            .extensions
            .get::<std::sync::Arc<super::wire_progress::Activity>>()
            .cloned()
            .unwrap_or_default();
        let (sender, receiver) = tokio::sync::mpsc::channel::<ForwardFrame>(2);
        let empty = ended && !failed && prefix.is_empty();
        if empty {
            output_delivery.body_complete();
        }
        let shutdown = self
            .gateway
            .request_state()
            .shutdown
            .as_ref()
            .map(watch::Sender::subscribe);
        tokio::spawn(relay(Relay {
            active: self.active,
            traffic: self.traffic,
            slot,
            protocol,
            g,
            route,
            model: self.plan.model,
            permits,
            cfg,
            status,
            attempted: self.attempted,
            prefix,
            request_budget: self.request_budget.clone(),
            sender,
            activity,
            shutdown,
            ended,
            failed,
            delivery,
            dedicated_search: self.plan.dedicated_search,
            body,
            inspected_bytes,
            neutral,
            operation: self.plan.operation,
        }));
        // Hyper may drop a Content-Length body immediately after its last frame,
        // without polling it again for None. Mark that frame before yielding it.
        Response::from_parts(response_parts, delivered(receiver, output_delivery, empty))
    }
    /// No attempt produced a response to relay.
    fn exhausted(mut self) -> Response<WireBody> {
        self.failover
            .finish(if self.ordinary_attempts > self.settings.max_retries {
                super::failover::Exit::AttemptsExhausted
            } else {
                super::failover::Exit::NoEligibleProvider
            });
        self.gateway.record_final(
            self.last_error.as_ref(),
            self.failover.failure_reported(),
            self.previous_provider.as_deref(),
            self.plan.model.as_deref(),
            self.last.as_ref().map(|r| r.status().as_u16()),
            self.attempted,
        );
        self.last.unwrap_or_else(|| {
            if self.last_category == code::RPM_LIMIT {
                let mut response = error(
                    StatusCode::TOO_MANY_REQUESTS,
                    code::RPM_LIMIT,
                    "供应商已达到 RPM 上限，等待后重试",
                );
                response.headers_mut().insert(
                    header::RETRY_AFTER,
                    self.rpm_retry_after
                        .unwrap_or(Duration::from_secs(1))
                        .as_secs()
                        .max(1)
                        .into(),
                );
                response
            } else {
                error(
                    if self.attempted == 0 {
                        StatusCode::SERVICE_UNAVAILABLE
                    } else {
                        StatusCode::BAD_GATEWAY
                    },
                    self.last_category,
                    if self.attempted == 0 {
                        "没有可用供应商，请检查队列和熔断状态"
                    } else {
                        "所有可用供应商均请求失败"
                    },
                )
            }
        })
    }
}
async fn forward(gateway: Gateway, mut request: Request<Incoming>) -> Response<WireBody> {
    let (settings, mode, mut ids, token, running) = {
        let s = gateway.request_state();
        let ids = if s.store.mode == "auto" {
            s.store
                .providers
                .iter()
                .filter(|p| p.queued && !p.search_only)
                .map(|p| p.id.clone())
                .collect::<Vec<_>>()
        } else {
            s.store.selected.iter().cloned().collect()
        };
        (
            s.store.settings.clone(),
            s.store.mode.clone(),
            ids,
            s.store.local_token.clone(),
            s.running,
        )
    };
    if !running {
        return stopped();
    }
    if !super::listener::authorized(request.headers(), &token) {
        return error(
            StatusCode::UNAUTHORIZED,
            code::LOCAL_AUTH,
            "本地网关认证失败",
        );
    }
    if request.method() == hyper::Method::GET {
        if let Some(response) =
            super::catalog::local_list(&gateway, request.uri(), request.headers())
        {
            return response;
        }
    }
    // Capture all route versions before receiving the request body: edits only affect new requests.
    let provider_ids: Vec<_> = gateway
        .request_state()
        .store
        .providers
        .iter()
        .map(|p| p.id.clone())
        .collect();
    let mut routes: std::collections::HashMap<_, _> = provider_ids
        .into_iter()
        .filter_map(|id| gateway.route(&id).map(|r| (id, r)))
        .collect();
    routes.retain(|_, route| route.accepts_protocol(request.uri().path()));
    ids.retain(|id| {
        routes
            .get(id)
            .is_some_and(|route| !route.provider.search_only)
    });
    let active = Active::new(gateway.clone());
    let websocket = request
        .headers()
        .get(header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("websocket"));
    if websocket
        && matches!(
            request.uri().path().trim_end_matches('/'),
            "/responses" | "/v1/responses"
        )
    {
        return super::websocket::accept(
            gateway,
            request,
            settings,
            mode == "manual",
            ids,
            routes,
            active,
        );
    }
    let downstream_upgrade = websocket.then(|| hyper::upgrade::on(&mut request));
    let traffic = gateway.0.registry.freeze.request();
    let (parts, body) = request.into_parts();
    let replay = match Replay::capture_progress(
        body,
        gateway.0.spool.path(),
        Some(Duration::from_secs(settings.idle_seconds)),
    )
    .await
    {
        Ok(body) => body,
        _ => {
            return error(
                StatusCode::BAD_REQUEST,
                code::BODY,
                "请求未完整接收、超过 1 GiB 或已取消",
            );
        }
    };
    let request_budget = super::wait_budget::WaitBudget::tracked(
        settings.retry_budget_seconds,
        &gateway.0.pending_requests,
    );
    traffic.waiting();
    request_budget.begin();
    request_budget.attach_traffic(&traffic);
    let forwarding = async {
        let plan = match plan(
            &gateway, &parts, &replay, &mode, websocket, &mut ids, &routes,
        )
        .await
        {
            Ok(plan) => plan,
            Err(response) => return *response,
        };
        let mut failover = super::failover::Tracker::new(plan.usage_trace.clone());
        failover.attach_wait_budget(&request_budget);
        if gateway.client_id() == super::ClientId::Codex && mode == "auto" && plan.responses {
            failover.diagnostics(&gateway);
            if let Some(log) = failover.routing() {
                log.pending(
                    &parts.headers,
                    plan.hints.as_ref().ok(),
                    plan.ownership.as_deref(),
                    plan.continuation,
                );
            }
        }
        request_budget.describe(plan.pinned.as_deref(), plan.model.as_deref(), "admission");
        let mut wait_budget = Budget::new(settings.queue_seconds);
        wait_budget.traffic = Some(traffic.clone());
        wait_budget.follow(plan.ownership.as_deref());
        if plan.pinned.is_some() || plan.unknown_affinity {
            wait_budget.pin_provider();
        }
        let mut cx = Exchange {
            gateway: &gateway,
            settings: &settings,
            mode: &mode,
            parts: &parts,
            replay: &replay,
            request_budget: &request_budget,
            websocket,
            active,
            traffic,
            downstream_upgrade,
            plan,
            ids,
            routes,
            failover,
            wait_budget,
            last: None,
            last_error: None,
            last_category: code::NO_PROVIDER,
            attempted: 0,
            ordinary_attempts: 0,
            capacity_protected: false,
            previous_provider: None,
            capacity_pending: false,
            capacity_wait_owner: None,
            hard_denied: std::collections::HashSet::new(),
            capacity_retry_after: None,
            rpm_retry_after: None,
            capacity_sources: Vec::new(),
            transient_retries: 0,
            retry_owner: None,
            search_fallback: false,
        };
        while cx.plan.dedicated_search || cx.ordinary_attempts <= cx.settings.max_retries {
            cx.traffic.waiting();
            cx.search_fallback |= cx.attempted > 0;
            // An unknown previous_response_id has no safe owner to replay against;
            // preserve the existing one-attempt rule instead of silently
            // repeating the request on an arbitrary provider.
            if !cx.plan.dedicated_search
                && ((cx.plan.unknown_affinity && !cx.capacity_protected)
                    || (cx.gateway.client_id() == super::ClientId::Claude && cx.plan.continuation))
                && cx.attempted > 0
            {
                break;
            }
            if cx.ids.is_empty() {
                end!(cx.await_capacity().await)
            }
            let (candidates, new_thread_model_denied) = step!(cx.candidates().await);
            let mut admission = step!(cx.admit(&candidates).await);
            let route = step!(cx.endpoint(&mut admission).await);
            let (upstream, upstream_model) = match upstream_request(
                &route,
                cx.parts,
                cx.replay,
                cx.plan.model.as_deref(),
                cx.plan.operation,
                cx.request_budget,
                cx.websocket,
            )
            .await
            {
                Ok(built) => built,
                Err(Unbuilt::Skip) => continue,
                Err(Unbuilt::Reject(response)) => return *response,
            };
            let mut attempt = cx.begin(admission, route, upstream_model, new_thread_model_denied);
            let deadline = if cx.plan.dedicated_search {
                attempt.search_deadline
            } else {
                tokio::time::Instant::now() + Duration::from_secs(cx.settings.first_byte_seconds)
            };
            let response = cx
                .request_budget
                .response(
                    deadline,
                    cx.plan.operation == crate::usage::model::Operation::Compaction,
                    attempt.route.client.request(upstream),
                )
                .await;
            let response = match response {
                Ok(Ok(response)) => response,
                Ok(Err(e)) => end!(cx.unreached(attempt, &e).await),
                Err(_) => end!(cx.headers_timeout(attempt)),
            };
            let status = response.status();
            let stream = response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|h| h.to_str().ok())
                .is_some_and(|h| {
                    h.split(';')
                        .next()
                        .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("text/event-stream"))
                });
            let encoding = response
                .headers()
                .get(header::CONTENT_ENCODING)
                .and_then(|h| h.to_str().ok())
                .unwrap_or("");
            attempt.protocol.response(status.as_u16(), stream, encoding);
            attempt.protocol.response_headers(response.headers());
            if status == StatusCode::SWITCHING_PROTOCOLS && cx.websocket {
                return cx.tunnel(attempt, response);
            }
            if status.as_u16() >= 400 {
                end!(cx.rejected(attempt, response).await)
            }
            let mut opening = cx.opening(&mut attempt, response, stream).await;
            if opening.failed
                && (cx.plan.dedicated_search
                    || attempt.route.client_id == super::ClientId::Dsh
                    || super::attempt::transport_retry(
                        opening.prefix_bytes != 0,
                        attempt.protocol.transport_failure(),
                        opening.failure_kind,
                    ))
            {
                end!(
                    cx.interrupted(attempt, opening.status, opening.failure_kind)
                        .await
                )
            }
            let attempt = step!(cx.initial_rejection(attempt, &mut opening).await);
            return cx.hand_over(attempt, opening);
        }
        cx.exhausted()
    };
    tokio::select! {
        biased;
        _ = request_budget.cancelled() => error(StatusCode::from_u16(499).unwrap(), code::CANCELLED, "已手动中止等待"),
        _ = request_budget.wait() => error(StatusCode::GATEWAY_TIMEOUT, code::FIRST_BYTE_TIMEOUT, "等待与重试预算已耗尽"),
        response = forwarding => if request_budget.cancel_requested() {
            error(StatusCode::from_u16(499).unwrap(), code::CANCELLED, "已手动中止等待")
        } else { response },
    }
}
pub(super) async fn stopping(stop: &mut Option<watch::Receiver<bool>>) {
    if let Some(stop) = stop {
        if !*stop.borrow() {
            let _ = stop.changed().await;
        }
    } else {
        std::future::pending::<()>().await;
    }
}
fn remember(protocol: &super::protocol::Protocol, g: &Gateway, route: &Route, model: Option<&str>) {
    if let Some(id) = &protocol.observation.response_id {
        g.0.endpoints.remember(&route.provider, id);
        g.remember_model(
            id,
            &route.provider.id,
            model.or(protocol.observation.model.as_deref()),
        );
    }
}
