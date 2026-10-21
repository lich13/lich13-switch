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
                        let routed = super::listener::route(request.uri(), request.headers(), &registry).and_then(|(client, uri)| {
                            *request.uri_mut() = uri;
                            registry.gateway(client).ok_or(super::listener::RouteError::NotFound)
                        });
                        let delivery = std::sync::Arc::new(super::protocol::Delivery::default());
                        delivery.track(activity.clone());
                        request.extensions_mut().insert(delivery);
                        request.extensions_mut().insert(ReceivedAt(crate::usage::model::now()));
                        request.extensions_mut().insert(activity.clone());
                        async move{Ok::<_,Infallible>(match routed {
                            Ok(g) => {
                                let mut client_stop = match g.request_state() {
                                    Ok(state) => state.shutdown.as_ref().map(watch::Sender::subscribe),
                                    Err(e) => return Ok(error(StatusCode::SERVICE_UNAVAILABLE, e.code.as_str(), &e.message)),
                                };
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
                        connection_activity.connection_failed(if connection_activity.error_code() != 0 { connection_activity.error_code() } else if error.is_canceled() { 4 } else if error.is_closed() || error.is_incomplete_message() { 5 } else if error.is_parse() { 8 } else { 2 });
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
    body: &mut Incoming,
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
    Response::builder().status(status).header(header::CONTENT_TYPE,"application/json")
        .body(replay::full(serde_json::to_vec(&serde_json::json!({"error":{"type":"gpt_switch_gateway","code":code,"message":message}})).unwrap())).unwrap()
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
fn authorized(headers: &HeaderMap, token: &str) -> bool {
    super::listener::authorized(headers, token)
}
pub(super) fn capacity_message(status: StatusCode, body: &[u8]) -> bool {
    if super::upstream_error::permanent_rejection(body) {
        return false;
    }
    if status == StatusCode::TOO_MANY_REQUESTS {
        return true;
    }
    if !circuit::retryable(status.as_u16()) {
        return false;
    }
    let text = String::from_utf8_lossy(body).to_ascii_lowercase();
    [
        "too many requests",
        "selected model is at capacity",
        "model is at capacity",
        "at capacity",
        "容量",
        "限流",
        "请求过多",
        "请求太多",
        "请求频率过高",
        "模型繁忙",
    ]
    .iter()
    .any(|needle| text.contains(needle))
}
pub(super) fn capacity_delay(settings: &Settings, retry: Option<Duration>) -> Duration {
    retry
        .unwrap_or_default()
        .max(Duration::from_secs(settings.capacity_retry_seconds))
}
async fn forward(gateway: Gateway, mut request: Request<Incoming>) -> Response<WireBody> {
    let (settings, mode, mut ids, token, running) = {
        let s = match gateway.request_state() {
            Ok(state) => state,
            Err(e) => return error(StatusCode::SERVICE_UNAVAILABLE, e.code.as_str(), &e.message),
        };
        let ids = if s.store.mode == "auto" {
            s.store
                .providers
                .iter()
                .filter(|p| p.queued)
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
        return error(StatusCode::SERVICE_UNAVAILABLE, code::STOPPED, "网关已停止");
    }
    if !authorized(request.headers(), &token) {
        return error(
            StatusCode::UNAUTHORIZED,
            code::LOCAL_AUTH,
            "本地网关认证失败",
        );
    }
    // Capture all route versions before receiving the request body: edits only affect new requests.
    let provider_ids: Vec<_> = match gateway.request_state() {
        Ok(state) => state,
        Err(e) => return error(StatusCode::SERVICE_UNAVAILABLE, e.code.as_str(), &e.message),
    }
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
    ids.retain(|id| routes.contains_key(id));
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
            let owner = match gateway.request_state() {
                Ok(state) => state,
                Err(e) => {
                    return error(StatusCode::SERVICE_UNAVAILABLE, e.code.as_str(), &e.message)
                }
            }
            .affinity
            .get(previous)
            .filter(|(_, _, at)| at.elapsed() < Duration::from_secs(3600))
            .cloned();
            if let Some((owner, previous_model, _)) = owner {
                pinned = Some(owner.clone());
                ids = vec![owner];
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
    let responses = matches!(
        parts.uri.path().trim_end_matches('/'),
        "/responses" | "/v1/responses" | "/responses/compact" | "/v1/responses/compact"
    );
    let thread = super::compaction::session(&parts.headers);
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
            ids = vec![owner.clone()];
            if hints.is_err() {
                // Metadata failure cannot move a conversation we already know.
                pinned = Some(owner);
            }
        }
    }
    let ownership = if gateway.client_id() == super::ClientId::Codex
        && mode == "auto"
        && responses
        && !unknown_affinity
        && !auxiliary
    {
        let queued = match super::compaction::queued(&gateway) {
            Ok(queued) => queued,
            Err(e) => return error(StatusCode::SERVICE_UNAVAILABLE, &e.code, &e.message),
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
                return error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    code::CONVERSATION_OWNERSHIP,
                    "线程归属记录暂不可用，请检查网关状态",
                );
            }
            lease
        } else {
            None
        }
    } else {
        None
    };
    let established_capacity_owner =
        routing::capacity_owner(gateway.client_id(), mode == "manual", ownership.as_deref());
    let usage_trace = if matches!(requirement, Requirement::Resource)
        && operation != crate::usage::model::Operation::WebSearch
    {
        None
    } else {
        gateway.usage_operation(model.as_deref(), operation)
    };
    let mut failover = super::failover::Tracker::new(usage_trace.clone());
    if gateway.client_id() == super::ClientId::Codex && mode == "auto" && responses {
        failover.diagnostics(&gateway);
        if let Some(log) = failover.routing() {
            log.pending(
                &parts.headers,
                hints.as_ref().ok(),
                ownership.as_deref(),
                continuation,
            );
        }
    }
    let mut last = None;
    let mut last_error: Option<super::health_settlement::Reported> = None;
    let mut attempted = 0usize;
    let mut ordinary_attempts = 0usize;
    let mut capacity_protected = false;
    let mut previous_provider: Option<String> = None;
    let mut last_category = code::NO_PROVIDER;
    let mut wait_budget = Budget::new(settings.queue_seconds);
    wait_budget.track(
        &gateway.0.admission,
        ownership.as_deref(),
        settings.thread_idle_release_seconds,
    );
    if pinned.is_some() || unknown_affinity {
        wait_budget.pin_provider();
    }
    let mut capacity_pending = false;
    let mut capacity_wait_owner: Option<String> = None;
    let mut hard_denied = std::collections::HashSet::new();
    let mut capacity_retry_after: Option<Duration> = None;
    let mut rpm_retry_after: Option<Duration> = None;
    let mut capacity_sources = Vec::new();
    let mut transient_retries = 0;
    let mut retry_owner: Option<String> = None;
    while ordinary_attempts <= settings.max_retries {
        // An unknown previous_response_id has no safe owner to replay against;
        // preserve the existing one-attempt rule instead of silently
        // repeating the request on an arbitrary provider.
        if ((unknown_affinity && !capacity_protected)
            || (gateway.client_id() == super::ClientId::Claude && continuation))
            && attempted > 0
        {
            break;
        }
        if ids.is_empty() {
            if capacity_pending && ordinary_attempts <= settings.max_retries {
                let delay = capacity_delay(&settings, capacity_retry_after);
                failover.wait(
                    if capacity_retry_after.is_some() {
                        super::failover::WaitKind::RetryAfter
                    } else {
                        super::failover::WaitKind::CapacityRetry
                    },
                    delay.as_secs(),
                );
                let _waiting_since = Instant::now();
                let owner_route = capacity_wait_owner
                    .as_deref()
                    .or_else(|| last_error.as_ref().and_then(|e| e.provider_id.as_deref()))
                    .and_then(|id| routes.get(id));
                let waited = gateway
                    .wait_capacity_checked(
                        &capacity_sources,
                        delay,
                        settings.max_waiting,
                        capacity_wait_owner.is_some(),
                        owner_route,
                        last_error.as_deref(),
                    )
                    .await;
                match waited {
                    Ok(true) => {
                        if let Some(route) = owner_route {
                            hard_denied.insert((
                                route.provider.id.clone(),
                                route.provider.version.clone(),
                            ));
                        }
                        capacity_wait_owner = None;
                        failover.failure(super::failover::Failure::AccountRejected);
                    }
                    Ok(false) => {}
                    Err(Rejected::Stopped) => {
                        failover.finish(super::failover::Exit::Stopped);
                        return error(StatusCode::SERVICE_UNAVAILABLE, code::STOPPED, "网关已停止");
                    }
                    Err(_) => break,
                }

                ids = gateway.routing_ids(pinned.as_deref());
                ids.retain(|id| {
                    gateway.route(id).is_some_and(|route| {
                        !hard_denied.contains(&(id.clone(), route.provider.version))
                    })
                });
                routes = ids
                    .iter()
                    .filter_map(|id| {
                        gateway
                            .route(id)
                            .filter(|r| r.accepts_protocol(parts.uri.path()))
                            .map(|r| (id.clone(), r))
                    })
                    .collect();
                if ids.is_empty() {
                    break;
                }
                retry_owner = capacity_wait_owner
                    .take()
                    .filter(|owner| ids.contains(owner));
                capacity_pending = false;
                capacity_retry_after = None;
                capacity_sources.clear();
                continue;
            }
            break;
        }
        let mut candidates: Vec<_> = ids
            .iter()
            .filter_map(|id| routes.get(id).cloned())
            .collect();
        let mut observed_candidates = candidates.clone();
        observed_candidates.extend(
            gateway
                .routing_ids(pinned.as_deref())
                .iter()
                .filter_map(|id| gateway.route(id))
                .filter(|r| !r.accepts_protocol(parts.uri.path())),
        );
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
            candidates = if attempted > 0 || !hard_denied.is_empty() {
                lease.candidates_after_failure(candidates, &requirement)
            } else {
                lease.candidates_for(candidates, &requirement)
            };
        } else if gateway.client_id() == super::ClientId::Codex
            && mode == "auto"
            && pinned.is_none()
            && !auxiliary
        {
            candidates = super::compaction::unidentified_candidates(candidates);
        }
        // A second request entering during its owner's capacity cooldown must
        // wait as well. Read the reset generation before the health snapshot.
        if attempted == 0 && candidates.len() == 1 {
            let owner = &candidates[0];
            let source = gateway.0.admission.capacity_source(&owner.provider.id);
            let health = owner.health_for(&requirement);
            if established_capacity_owner.as_deref() == Some(&owner.provider.id)
                && routing::capacity_cooling(&health)
            {
                wait_budget.protect_capacity();
                failover.wait(super::failover::WaitKind::CapacityRetry, health.retry_in);
                let cause = gateway.capacity_cause(&owner.provider.id, model.as_deref(), &health);
                match gateway
                    .wait_capacity_checked(
                        &[source],
                        Duration::from_secs(health.retry_in),
                        settings.max_waiting,
                        true,
                        Some(owner),
                        cause.as_ref(),
                    )
                    .await
                {
                    Ok(true) => {
                        hard_denied
                            .insert((owner.provider.id.clone(), owner.provider.version.clone()));
                        failover.failure(super::failover::Failure::AccountRejected);
                        last = Some(error(
                            StatusCode::TOO_MANY_REQUESTS,
                            code::HTTP,
                            "本次额度查询确认耗尽，没有可用的替代供应商",
                        ));
                        last_category = code::HTTP;
                        last_error = cause.map(Into::into);
                    }
                    Ok(false) => {}
                    Err(Rejected::Stopped) => {
                        return error(StatusCode::SERVICE_UNAVAILABLE, code::STOPPED, "网关已停止")
                    }
                    Err(_) => {
                        return error(
                            StatusCode::TOO_MANY_REQUESTS,
                            code::CAPACITY,
                            "等待队列已满",
                        )
                    }
                }
                ids = gateway.routing_ids(pinned.as_deref());
                ids.retain(|id| {
                    gateway
                        .route(id)
                        .is_some_and(|r| !hard_denied.contains(&(id.clone(), r.provider.version)))
                });
                routes = ids
                    .iter()
                    .filter_map(|id| {
                        gateway
                            .route(id)
                            .filter(|r| r.accepts_protocol(parts.uri.path()))
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
        } else if attempted > 0 || !hard_denied.is_empty() {
            super::failover::Stage::FailureFallback
        } else if ownership.as_ref().is_some_and(|l| l.handoff) {
            super::failover::Stage::ProactiveHandoff
        } else {
            super::failover::Stage::Initial
        };
        failover.candidates(
            &gateway,
            stage,
            &observed_candidates,
            &candidates,
            &requirement,
            parts.uri.path(),
            &wait_budget,
            mode == "manual",
        );
        if matches_model
            && !candidates
                .iter()
                .any(|r| requirement.allows(r.provider.allowed_models.as_deref()))
        {
            if attempted > 0 {
                if capacity_pending {
                    ids.clear();
                    continue;
                }
                break;
            }
            failover.finish(super::failover::Exit::NoEligibleProvider);
            gateway.record_policy_rejection(
                model.as_deref(),
                false,
                super::compaction::rejection_reason(thread.as_deref(), hints.as_ref().ok()),
            );
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                code::NO_ELIGIBLE_PROVIDER,
                "没有符合线程接管策略的供应商",
            );
        }
        if gateway.client_id() == super::ClientId::Codex
            && mode == "auto"
            && pinned.is_none()
            && !auxiliary
        {
            let (intake, owner) = ownership
                .as_ref()
                .map_or((super::compaction::Intake::Ordinary, None), |lease| {
                    lease.admission_policy()
                });
            wait_budget.set_intake(intake, owner);
        }
        let _admission_started = Instant::now();
        if attempted > 0 || !hard_denied.is_empty() {
            // Admission rechecks policy atomically. Its proactive priority gate
            // must also yield to the failure queue, without dropping the claim.
            wait_budget.handoff_owner = None;
        }
        let mut admission = match gateway
            .0
            .admission
            .acquire_for_immediate(
                &candidates,
                mode == "manual",
                settings.max_waiting,
                &mut wait_budget,
                &requirement,
                capacity_pending,
            )
            .await
            .inspect_err(|reason| failover.rejected(reason))
        {
            Ok(value) => value,
            Err(Rejected::Full | Rejected::Timeout) => {
                if attempted > 0 {
                    if capacity_pending {
                        ids.clear();
                        continue;
                    }
                    break;
                }
                let mut response = error(
                    StatusCode::TOO_MANY_REQUESTS,
                    code::CAPACITY,
                    "供应商并发已满，等待队列已满或等待超时",
                );
                response
                    .headers_mut()
                    .insert(header::RETRY_AFTER, header::HeaderValue::from_static("5"));
                return response;
            }
            Err(Rejected::Cooling(seconds)) => {
                if attempted > 0 {
                    if capacity_pending {
                        ids.clear();
                        continue;
                    }
                    break;
                }
                let mut response = error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    code::PROVIDERS_COOLING_DOWN,
                    "供应商正在冷却或恢复探测，请稍后重试",
                );
                response
                    .headers_mut()
                    .insert(header::RETRY_AFTER, seconds.max(1).into());
                return response;
            }
            Err(Rejected::RateLimited(seconds)) => {
                rpm_retry_after = Some(Duration::from_secs(seconds.max(1)));
                if attempted > 0 {
                    last_category = code::RPM_LIMIT;
                    break;
                }
                let mut response = error(
                    StatusCode::TOO_MANY_REQUESTS,
                    code::RPM_LIMIT,
                    "供应商已达到 RPM 上限，等待后重试",
                );
                response
                    .headers_mut()
                    .insert(header::RETRY_AFTER, seconds.max(1).into());
                return response;
            }
            Err(Rejected::RateLedger) => {
                return error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    code::RPM_LEDGER,
                    "RPM 状态暂不可用，请稍后重试",
                );
            }
            Err(Rejected::Stopped) => {
                return error(StatusCode::SERVICE_UNAVAILABLE, code::STOPPED, "网关已停止");
            }
            Err(Rejected::Model) => {
                if attempted > 0 {
                    if capacity_pending {
                        ids.clear();
                        continue;
                    }
                    break;
                }
                return error(
                    StatusCode::BAD_REQUEST,
                    requirement.code(),
                    requirement.message(),
                );
            }
            Err(Rejected::Policy) => {
                if attempted > 0 {
                    if capacity_pending {
                        ids.clear();
                        continue;
                    }
                    break;
                }
                gateway.record_policy_rejection(
                    model.as_deref(),
                    false,
                    super::compaction::rejection_reason(thread.as_deref(), hints.as_ref().ok()),
                );
                return error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    code::NO_ELIGIBLE_PROVIDER,
                    "没有符合线程接管策略的供应商",
                );
            }
            Err(Rejected::Unavailable) => {
                if capacity_pending {
                    ids.clear();
                    continue;
                }
                break;
            }
        };
        let route = match gateway
            .endpoint_route(
                admission.route.clone(),
                thread.as_deref(),
                hints
                    .as_ref()
                    .ok()
                    .and_then(|h| h.previous_response_id.as_deref()),
                ownership
                    .as_ref()
                    .is_some_and(|lease| lease.handoff || lease.establishing())
                    || hints.as_ref().is_ok_and(|h| h.first_turn),
            )
            .await
        {
            Ok(route) => route,
            Err(reason) => {
                failover.finish(super::failover::Exit::AdmissionRejected);
                admission.permits.neutral(&settings);
                return error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    &reason.code,
                    &reason.message,
                );
            }
        };
        let owner_admitted = ownership.as_ref().map_or(Ok(true), |lease| {
            lease.admit_request(&gateway, &route.provider, &requirement, pinned.is_some())
        });
        let owner_admitted = match owner_admitted {
            Ok(value) => value,
            Err(e) => {
                admission.permits.neutral(&settings);
                return error(StatusCode::SERVICE_UNAVAILABLE, &e.code, &e.message);
            }
        };
        if !owner_admitted {
            admission.permits.neutral(&settings);
            if gateway.0.compaction.error().is_some() {
                return error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    code::CONVERSATION_OWNERSHIP,
                    "线程归属记录暂不可用",
                );
            }
            continue;
        }
        if unknown_affinity {
            pinned = Some(route.provider.id.clone());
            wait_budget.pin_provider();
        }
        let reset_generation = admission.reset_generation;
        ids.retain(|id| id != &route.provider.id);
        let uri = match route.target(&parts.uri) {
            Ok(uri) => uri,
            Err(_) => continue,
        };
        let mut upstream = Request::new(replay.body());
        *upstream.method_mut() = parts.method.clone();
        *upstream.uri_mut() = uri;
        *upstream.headers_mut() = parts.headers.clone();
        clean_headers(upstream.headers_mut(), websocket);
        upstream.headers_mut().remove(header::HOST);
        upstream.headers_mut().remove(header::AUTHORIZATION);
        upstream.headers_mut().remove("x-api-key");
        let Ok(auth) = header::HeaderValue::from_str(&format!("Bearer {}", route.provider.token))
        else {
            continue;
        };
        upstream.headers_mut().insert(header::AUTHORIZATION, auth);
        if let Some(connection) = &route.grok {
            if connection
                .authenticate(upstream.headers_mut(), &route.provider.token)
                .is_err()
            {
                return error(
                    StatusCode::BAD_GATEWAY,
                    code::GROK_AUTH,
                    "Grok 供应商认证配置无效",
                );
            }
        }
        if !replay.has_trailers() {
            upstream.headers_mut().insert(
                header::CONTENT_LENGTH,
                header::HeaderValue::from(replay.length),
            );
        } else {
            upstream.headers_mut().remove(header::CONTENT_LENGTH);
        }
        if let Err(reason) = admission.commit_rpm() {
            admission.permits.neutral(&settings);
            return match reason {
                Rejected::RateLimited(seconds) => {
                    let mut response = error(
                        StatusCode::TOO_MANY_REQUESTS,
                        code::RPM_LIMIT,
                        "RPM 发送保护生效，请稍后重试",
                    );
                    response
                        .headers_mut()
                        .insert(header::RETRY_AFTER, seconds.max(1).into());
                    response
                }
                Rejected::Stopped => {
                    error(StatusCode::SERVICE_UNAVAILABLE, code::STOPPED, "网关已停止")
                }
                Rejected::Unavailable => error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    code::STALE,
                    "供应商配置已变化，请重试",
                ),
                _ => error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    code::RPM_LEDGER,
                    "RPM 状态暂不可用，请稍后重试",
                ),
            };
        }
        attempted = attempted.saturating_add(1);
        ordinary_attempts = ordinary_attempts.saturating_add(1);
        let rerouted = previous_provider
            .as_deref()
            .is_some_and(|id| id != admission.route.provider.id);
        previous_provider = Some(admission.route.provider.id.clone());
        let started = Instant::now();
        let mut protocol = super::protocol::Protocol::new(stream_hint);
        protocol.attach_rpm(admission.take_completion());
        protocol.set_operation(operation);
        protocol.diagnostics(&gateway, &route.provider.id, model.as_deref());
        protocol.attach_ownership(ownership.clone(), &route.provider.id);
        protocol.attach_routing_log(failover.routing());
        protocol.attach_thread(wait_budget.thread.clone());
        protocol.request_headers(&parts.headers);
        if let Some(at) = parts.extensions.get::<ReceivedAt>() {
            protocol.request_received(at.0);
        }
        if let Some(delivery) = parts
            .extensions
            .get::<std::sync::Arc<super::protocol::Delivery>>()
        {
            protocol.set_delivery(delivery.clone());
        }
        if let Some(trace) = &usage_trace {
            protocol.attach_usage(trace.attempt(
                &route.provider.id,
                stream_hint,
                if websocket { "websocket" } else { "http" },
            ));
        }
        protocol.context(
            thread.clone(),
            (gateway.client_id() == super::ClientId::Codex && mode == "auto" && responses).then(
                || {
                    admission
                        .handoff_wait
                        .or(
                            (new_thread_model_denied && !route.provider.take_new_threads)
                                .then_some(
                                    crate::usage::model::RoutingDecision::NewThreadModelDenied,
                                ),
                        )
                        .unwrap_or_else(|| {
                            super::compaction::decision(
                                thread.as_deref(),
                                hints.as_ref().ok(),
                                continuation,
                                ownership.as_deref(),
                                &route.provider.id,
                            )
                        })
                },
            ),
        );
        if gateway.client_id() == super::ClientId::Codex && mode == "auto" && responses {
            protocol.routing_evidence(hints.as_ref().ok());
        }
        protocol.watch_lifecycle(
            match gateway.request_state() {
                Ok(state) => state,
                Err(e) => {
                    return error(StatusCode::SERVICE_UNAVAILABLE, e.code.as_str(), &e.message)
                }
            }
            .shutdown
            .as_ref()
            .map(watch::Sender::subscribe),
            None,
        );
        let deadline =
            tokio::time::Instant::now() + Duration::from_secs(settings.first_byte_seconds);
        let response = tokio::time::timeout_at(deadline, route.client.request(upstream)).await;
        let mut response = match response {
            Ok(Ok(response)) => response,
            Ok(Err(e)) => {
                failover.failure(super::failover::Failure::Transport);
                let category = connector::classify(&e);
                last_category = match category {
                    Some(connector::ConnectError::Tls) => code::TLS,
                    _ => code::NETWORK,
                };
                protocol.finish(None, last_category);
                last_error = Some(failover.report_failure(Some(&protocol), || {
                    admission.permits.report(
                        &gateway,
                        &route,
                        model.as_deref(),
                        Reason::Network,
                        routing::fallback_action(
                            &gateway,
                            &route.provider.id,
                            ids.iter()
                                .filter_map(|id| routes.get(id).cloned())
                                .collect(),
                            &requirement,
                            parts.uri.path(),
                            ownership.as_deref(),
                            gateway.client_id() == super::ClientId::Codex && !auxiliary,
                            &wait_budget,
                            mode != "manual"
                                && pinned.is_none()
                                && !unknown_affinity
                                && ordinary_attempts <= settings.max_retries,
                        ),
                        None,
                        attempted,
                        crate::events::Details {
                            phase: Some(crate::events::Phase::Connect),
                            local_code: Some(connector::diagnostic_code(&e).into()),
                            counted_failure: Some(true),
                            ..Default::default()
                        },
                    )
                }));
                admission.permits.failure(&settings, None);
                continue;
            }
            Err(_) => {
                failover.failure(super::failover::Failure::Transport);
                failover.finish(super::failover::Exit::ReplayUnsafe);
                protocol.timeout("response_headers", settings.first_byte_seconds);
                protocol.finish(None, code::FIRST_BYTE_TIMEOUT);
                failover.report_failure(Some(&protocol), || {
                    admission.permits.report(
                        &gateway,
                        &route,
                        model.as_deref(),
                        Reason::Network,
                        Action::Returned,
                        None,
                        attempted,
                        crate::events::Details {
                            phase: Some(crate::events::Phase::Headers),
                            local_code: Some(code::FIRST_BYTE_TIMEOUT.into()),
                            counted_failure: Some(true),
                            ..protocol.observation.error.clone()
                        },
                    )
                });
                admission.permits.failure(&settings, None);
                // The HTTP client already owns the request. Not seeing headers
                // cannot establish that its body never reached the provider.
                return error(
                    StatusCode::GATEWAY_TIMEOUT,
                    code::FIRST_BYTE_TIMEOUT,
                    "上游首字节超时，请求可能已开始处理，未自动重放",
                );
            }
        };
        let status = response.status();
        let response_stream = response
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
        protocol.response(status.as_u16(), response_stream, encoding);
        protocol.response_headers(response.headers());
        if status == StatusCode::SWITCHING_PROTOCOLS && websocket {
            failover.finish(super::failover::Exit::Forwarded);
            admission.permits.success(&settings);
            let Admission { permits, slot, .. } = admission;
            let upstream_upgrade = hyper::upgrade::on(&mut response);
            let (mut response_parts, _) = response.into_parts();
            clean_headers(&mut response_parts.headers, true);
            let downstream = downstream_upgrade.expect("upgrade exists");
            let g = gateway.clone();
            let cfg = settings.clone();
            let provider_for_tunnel = route.provider.clone();
            tokio::spawn(async move {
                let _active = active;
                let _slot = slot;
                let mut permits = permits;
                let connected = tokio::try_join!(upstream_upgrade, downstream);
                if let Ok((a, b)) = connected {
                    // Opaque tunnel preserves every data/control/close frame, including binary payloads.
                    let result =
                        tokio::io::copy_bidirectional(&mut TokioIo::new(a), &mut TokioIo::new(b))
                            .await;
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
                gateway.record(
                    Some(&route.provider.id),
                    model.as_deref(),
                    Reason::Failover,
                    Action::Routed,
                    Some(status.as_u16()),
                    Some(attempted.min(u32::MAX as usize) as u32),
                );
            }
            return Response::from_parts(response_parts, replay::empty());
        }
        if status.as_u16() >= 400 {
            let cooldown = response
                .headers()
                .get(header::RETRY_AFTER)
                .and_then(|h| h.to_str().ok())
                .and_then(circuit::retry_after);
            let (mut response_parts, body) = response.into_parts();
            clean_headers(&mut response_parts.headers, false);
            let captured = Replay::capture_observed(
                body,
                gateway.0.spool.path(),
                Some(Duration::from_secs(settings.idle_seconds)),
                |bytes| protocol.feed(bytes),
            )
            .await;
            if captured
                .as_ref()
                .err()
                .and_then(|e| e.downcast_ref::<std::io::Error>())
                .is_some_and(|e| e.kind() == std::io::ErrorKind::TimedOut)
            {
                protocol.timeout("error_body_idle", settings.idle_seconds);
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
                _ => Vec::new(),
            };
            let cooldown = cooldown.max(super::upstream_error::retry_after_http(&decoded));
            let compact_error = ownership.as_ref().is_some_and(|lease| {
                lease.handoff && lease.owner.as_deref() != Some(&route.provider.id)
            }) && protocol.observation.compaction_incompatible;
            if compact_error {
                if let Some(owner) = ownership
                    .as_ref()
                    .and_then(|lease| lease.owner.as_ref())
                    .filter(|owner| ids.contains(owner))
                {
                    retry_owner = Some(owner.clone());
                }
            }
            let model_error = protocol.observation.terminal
                == Some(super::protocol::Terminal::ModelUnavailable)
                || super::upstream_error::model_http(status.as_u16(), &decoded);
            let mut details = super::upstream_error::details_http(&decoded);
            if details == crate::events::Details::default() {
                details = protocol.observation.error.clone();
            }
            if details.error_basis == Some(crate::events::ErrorBasis::Rpm) {
                gateway.0.admission.rpm_backoff(
                    &route.provider.id,
                    cooldown.unwrap_or(Duration::from_secs(settings.capacity_retry_seconds)),
                );
            }
            let hard_rejection = details.health_scope == Some(crate::events::HealthScope::Provider);
            if hard_rejection {
                hard_denied.insert((route.provider.id.clone(), route.provider.version.clone()));
            }
            let capacity = !model_error
                && !hard_rejection
                && route.client_id != super::ClientId::Claude
                && (protocol.observation.capacity_error || capacity_message(status, &decoded));
            let wait_same =
                capacity && established_capacity_owner.as_deref() == Some(&route.provider.id);
            if capacity && !websocket {
                ordinary_attempts = ordinary_attempts.saturating_sub(1);
                capacity_protected = true;
                wait_budget.protect_capacity();
            }
            let retryable = compact_error
                || model_error
                || hard_rejection
                || circuit::retryable(status.as_u16());
            let retry_delay = if route.client_id == super::ClientId::Codex
                && circuit::is_server_error(status.as_u16())
                && !compact_error
                && !model_error
                && !capacity
                && !hard_rejection
                && !unknown_affinity
                && ordinary_attempts <= settings.max_retries
                && (pinned.as_deref() == Some(&route.provider.id)
                    || ownership
                        .as_ref()
                        .is_some_and(|lease| lease.owns(&route.provider.id)))
            {
                routing::transient_delay(&mut transient_retries, cooldown)
            } else {
                None
            };
            failover.failure(if hard_rejection {
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
                && routing::fallback_action(
                    &gateway,
                    &route.provider.id,
                    ids.iter()
                        .filter_map(|id| routes.get(id).cloned())
                        .collect(),
                    &requirement,
                    parts.uri.path(),
                    ownership.as_deref(),
                    gateway.client_id() == super::ClientId::Codex && !auxiliary,
                    &wait_budget,
                    mode != "manual"
                        && pinned.is_none()
                        && !unknown_affinity
                        && ordinary_attempts <= settings.max_retries,
                ) == Action::TryingNext;
            let reason = if compact_error {
                Reason::ProtocolError
            } else if model_error {
                Reason::ModelUnavailable
            } else if details.error_basis == Some(crate::events::ErrorBasis::QuotaExhausted)
                || (hard_rejection && status == StatusCode::TOO_MANY_REQUESTS)
            {
                Reason::UpstreamService
            } else if status == StatusCode::TOO_MANY_REQUESTS {
                Reason::RateLimit
            } else if capacity {
                Reason::Capacity
            } else if matches!(status.as_u16(), 401 | 403) {
                Reason::Authentication
            } else {
                Reason::UpstreamService
            };
            details.phase = Some(crate::events::Phase::Response);
            details.counted_failure = Some(
                !compact_error
                    && !model_error
                    && !capacity
                    && (status.as_u16() != 429 || hard_rejection)
                    && (retryable || hard_rejection),
            );
            details.wait_seconds = if wait_same {
                Some(capacity_delay(&settings, cooldown).as_secs())
            } else {
                retry_delay.map(|delay| delay.as_secs())
            };
            protocol.finish(
                Some(status.as_u16()),
                if captured.is_ok() {
                    code::HTTP
                } else {
                    code::STREAM_INTERRUPTED
                },
            );
            last_error = Some(failover.report_failure(Some(&protocol), || {
                admission.permits.report(
                    &gateway,
                    &route,
                    model.as_deref(),
                    reason,
                    if retry_delay.is_some() || wait_same {
                        Action::RetryingSame
                    } else if will_retry {
                        Action::TryingNext
                    } else if capacity {
                        Action::Waiting
                    } else {
                        Action::Returned
                    },
                    Some(status.as_u16()),
                    attempted,
                    details,
                )
            }));
            if model_error || compact_error {
                admission.permits.neutral(&settings);
            } else if capacity {
                admission.permits.capacity_limited(&settings, cooldown);
            } else if hard_rejection {
                admission.permits.failure(&settings, cooldown);
            } else if status == StatusCode::TOO_MANY_REQUESTS {
                admission.permits.rate_limited(&settings, cooldown);
            } else if retryable || hard_rejection {
                admission.permits.failure(&settings, cooldown);
            } else {
                admission.permits.neutral(&settings);
            }
            if let Ok(body) = captured {
                last = Some(Response::from_parts(response_parts, body.body()));
            }
            last_category = code::HTTP;
            if !retryable {
                failover.finish(super::failover::Exit::PermanentRejection);
                return last.unwrap_or_else(|| {
                    error(
                        StatusCode::BAD_GATEWAY,
                        code::UPSTREAM_BODY,
                        "上游错误响应读取失败",
                    )
                });
            }
            if let Some(delay) = retry_delay {
                failover.wait(super::failover::WaitKind::TransientRetry, delay.as_secs());
                // Release the failed attempt before registering a bounded wait.
                // Keep the original spool, model, and response cursor untouched.
                drop(admission);
                let _waiting_since = Instant::now();
                match gateway
                    .0
                    .admission
                    .wait_transient(
                        CapacitySource {
                            provider_id: route.provider.id.clone(),
                            reset_generation,
                        },
                        delay,
                        settings.max_waiting,
                    )
                    .await
                {
                    Ok(()) => {}
                    Err(Rejected::Stopped) => {
                        failover.finish(super::failover::Exit::Stopped);
                        return error(StatusCode::SERVICE_UNAVAILABLE, code::STOPPED, "网关已停止");
                    }
                    Err(_) => break,
                }

                if gateway
                    .routing_ids(pinned.as_deref())
                    .contains(&route.provider.id)
                {
                    ids.insert(0, route.provider.id.clone());
                    retry_owner = Some(route.provider.id.clone());
                }
            }
            if capacity {
                if wait_same {
                    capacity_wait_owner = Some(route.provider.id.clone());
                    ids.clear();
                }
                capacity_pending = true;
                capacity_sources.push(CapacitySource {
                    provider_id: route.provider.id.clone(),
                    reset_generation,
                });
                capacity_retry_after = capacity_retry_after.max(cooldown);
            }
            continue;
        }
        let stream = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| {
                v.split(';')
                    .next()
                    .is_some_and(|v| v.trim().eq_ignore_ascii_case("text/event-stream"))
            });
        let (mut response_parts, mut body) = response.into_parts();
        clean_headers(&mut response_parts.headers, false);
        let mut first_deadline = tokio::time::Instant::now()
            + Duration::from_secs(settings.first_byte_seconds).saturating_sub(started.elapsed());
        // Buffer only the first complete SSE event (or bounded JSON); preserve
        // the original frames and encoding exactly, including comments/trailers.
        let json = response_parts
            .headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.to_ascii_lowercase().contains("json"));
        let mut prefix = Vec::new();
        let mut prefix_bytes = 0;
        let mut ended = false;
        let mut failed = false;
        let mut failure_kind = code::FIRST_BYTE_TIMEOUT;
        loop {
            match tokio::time::timeout_at(first_deadline, body.frame()).await {
                Ok(Some(Ok(frame))) => {
                    if let Some(bytes) = frame.data_ref() {
                        prefix_bytes += bytes.len();
                        protocol.feed(bytes);
                        if !bytes.is_empty() {
                            first_deadline = tokio::time::Instant::now()
                                + Duration::from_secs(settings.idle_seconds);
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
                    if ended
                        || prefix_bytes >= 2 * 1024 * 1024
                        || (!stream && !json)
                        || (stream && protocol.observation.first_event_model_error.is_some())
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
                        settings.first_byte_seconds
                    } else {
                        settings.idle_seconds
                    },
                );
            }
            protocol.finish(Some(status.as_u16()), failure_kind);
        }
        if failed
            && super::attempt::transport_retry(
                prefix_bytes != 0,
                protocol.transport_failure(),
                failure_kind,
            )
        {
            failover.failure(super::failover::Failure::Transport);
            last_error = Some(failover.report_failure(Some(&protocol), || {
                admission.permits.report(
                    &gateway,
                    &route,
                    model.as_deref(),
                    Reason::Network,
                    routing::fallback_action(
                        &gateway,
                        &route.provider.id,
                        ids.iter()
                            .filter_map(|id| routes.get(id).cloned())
                            .collect(),
                        &requirement,
                        parts.uri.path(),
                        ownership.as_deref(),
                        gateway.client_id() == super::ClientId::Codex && !auxiliary,
                        &wait_budget,
                        mode != "manual"
                            && pinned.is_none()
                            && !unknown_affinity
                            && ordinary_attempts <= settings.max_retries,
                    ),
                    Some(status.as_u16()),
                    attempted,
                    crate::events::Details {
                        phase: Some(crate::events::Phase::Response),
                        local_code: Some(failure_kind.into()),
                        counted_failure: Some(true),
                        ..protocol.observation.error.clone()
                    },
                )
            }));
            admission.permits.failure(&settings, None);
            last_category = failure_kind;
            continue;
        }
        ended |= failed;
        if ended {
            protocol.finish(
                Some(status.as_u16()),
                if status.is_success() {
                    code::OK
                } else {
                    code::HTTP
                },
            );
            if protocol.terminal_observed() {
                if let Some((_, terminal, business)) = prefix.last_mut() {
                    *terminal = true;
                    *business = true;
                }
            }
        }
        let initial_model_error = if stream {
            protocol.observation.first_event_model_error == Some(true)
        } else {
            ended && protocol.terminal() == Some(super::protocol::Terminal::ModelUnavailable)
        };
        let initial_compact_error = protocol.observation.compaction_incompatible
            && ownership.as_ref().is_some_and(|lease| {
                lease.handoff && lease.owner.as_deref() != Some(&route.provider.id)
            });
        if initial_compact_error {
            if let Some(owner) = ownership
                .as_ref()
                .and_then(|lease| lease.owner.as_ref())
                .filter(|owner| ids.contains(owner))
            {
                retry_owner = Some(owner.clone());
            }
        }
        let initial_hard_rejection = if stream {
            protocol.observation.first_event_account_error == Some(true)
        } else {
            ended
                && protocol.observation.error.health_scope
                    == Some(crate::events::HealthScope::Provider)
        };
        if initial_hard_rejection {
            hard_denied.insert((route.provider.id.clone(), route.provider.version.clone()));
        }
        let initial_capacity = !initial_hard_rejection
            && route.client_id != super::ClientId::Claude
            && !websocket
            && !initial_model_error
            && if stream {
                protocol.observation.first_event_capacity_error == Some(true)
            } else {
                ended && protocol.observation.capacity_error
            };
        let wait_same =
            initial_capacity && established_capacity_owner.as_deref() == Some(&route.provider.id);
        if initial_model_error
            || initial_capacity
            || initial_compact_error
            || initial_hard_rejection
        {
            failover.failure(if initial_hard_rejection {
                super::failover::Failure::AccountRejected
            } else if initial_capacity {
                super::failover::Failure::RateLimit
            } else if initial_model_error {
                super::failover::Failure::ModelUnavailable
            } else {
                super::failover::Failure::Protocol
            });
            let cooldown = response_parts
                .headers
                .get(header::RETRY_AFTER)
                .and_then(|h| h.to_str().ok())
                .and_then(circuit::retry_after)
                .max(protocol.observation.retry_after);
            if protocol.observation.error.error_basis == Some(crate::events::ErrorBasis::Rpm) {
                gateway.0.admission.rpm_backoff(
                    &route.provider.id,
                    cooldown.unwrap_or(Duration::from_secs(settings.capacity_retry_seconds)),
                );
            }
            if initial_capacity {
                if wait_same {
                    capacity_wait_owner = Some(route.provider.id.clone());
                    ids.clear();
                }
                ordinary_attempts = ordinary_attempts.saturating_sub(1);
                capacity_protected = true;
                wait_budget.protect_capacity();

                capacity_pending = true;
                capacity_sources.push(CapacitySource {
                    provider_id: route.provider.id.clone(),
                    reset_generation,
                });
                capacity_retry_after = capacity_retry_after.max(cooldown);
            }
            protocol.finish(Some(status.as_u16()), code::HTTP);
            let will_retry = routing::fallback_action(
                &gateway,
                &route.provider.id,
                ids.iter()
                    .filter_map(|id| routes.get(id).cloned())
                    .collect(),
                &requirement,
                parts.uri.path(),
                ownership.as_deref(),
                gateway.client_id() == super::ClientId::Codex && !auxiliary,
                &wait_budget,
                mode != "manual"
                    && pinned.is_none()
                    && !unknown_affinity
                    && ordinary_attempts <= settings.max_retries,
            ) == Action::TryingNext;
            let mut details = protocol.observation.error.clone();
            details.phase = Some(crate::events::Phase::Response);
            details.counted_failure = Some(initial_hard_rejection);
            if wait_same {
                details.wait_seconds = Some(capacity_delay(&settings, cooldown).as_secs());
            }
            last_error = Some(failover.report_failure(Some(&protocol), || {
                admission.permits.report(
                    &gateway,
                    &route,
                    model.as_deref(),
                    if initial_compact_error {
                        Reason::ProtocolError
                    } else if initial_hard_rejection {
                        Reason::UpstreamService
                    } else if initial_capacity {
                        Reason::Capacity
                    } else {
                        Reason::ModelUnavailable
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
                    attempted,
                    details,
                )
            }));
            if initial_capacity {
                admission.permits.capacity_limited(&settings, cooldown);
            } else if initial_hard_rejection {
                admission.permits.failure(&settings, cooldown);
            } else {
                admission.permits.neutral(&settings);
            }
            // Preserve a bounded error prefix. Dropping its unread tail cancels the
            // rejected attempt; if no alternative succeeds, return the original stream.
            let error_idle_seconds = settings.idle_seconds;
            let rejected = async_stream::try_stream! {
                for (frame, _, _) in prefix {yield frame;}
                loop {
                    match tokio::time::timeout(Duration::from_secs(error_idle_seconds),body.frame()).await {
                        Ok(Some(frame))=>yield frame.map_err(|e|Box::new(e) as BoxError)?,
                        Ok(None)=>break,
                        Err(_)=>Err::<(),BoxError>(std::io::Error::other("upstream error stream timeout").into())?,
                    }
                }
            };
            last = Some(Response::from_parts(
                response_parts,
                StreamBody::new(rejected).boxed_unsync(),
            ));
            last_category = if initial_hard_rejection {
                code::UPSTREAM_REJECTED
            } else if initial_capacity {
                code::CAPACITY
            } else {
                code::MODEL_UNAVAILABLE
            };
            continue;
        }
        let replay_unsafe = failed && failure_kind == code::FIRST_BYTE_TIMEOUT;
        if replay_unsafe {
            failover.failure(super::failover::Failure::Transport);
            protocol.associate_failure(failover.failure_generation());
        }
        failover.finish(if replay_unsafe {
            super::failover::Exit::ReplayUnsafe
        } else {
            super::failover::Exit::Forwarded
        });
        let Admission {
            mut permits, slot, ..
        } = admission;
        let g = gateway.clone();
        let cfg = settings.clone();
        let neutral = status.as_u16() >= 400;
        if rerouted {
            gateway.record(
                Some(&route.provider.id),
                model.as_deref(),
                Reason::Failover,
                Action::Routed,
                Some(status.as_u16()),
                Some(attempted.min(u32::MAX as usize) as u32),
            );
        }
        let delivery = protocol.delivery();
        let output_delivery = delivery.clone();
        let activity = parts
            .extensions
            .get::<std::sync::Arc<super::wire_progress::Activity>>()
            .cloned()
            .unwrap_or_default();
        let (sender, mut receiver) = tokio::sync::mpsc::channel::<ForwardFrame>(2);
        let empty = ended && !failed && prefix.is_empty();
        if empty {
            output_delivery.body_complete();
        }
        let mut shutdown = match gateway.request_state() {
            Ok(state) => state,
            Err(e) => return error(StatusCode::SERVICE_UNAVAILABLE, e.code.as_str(), &e.message),
        }
        .shutdown
        .as_ref()
        .map(watch::Sender::subscribe);
        tokio::spawn(async move {
            let mut active = Some(active);
            let mut slot = Some(slot);
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
                if protocol.succeeded() {
                    if protocol.delivered_success() {
                        if let Some(slot) = &mut slot {
                            slot.complete();
                        }
                    }
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
                    _ = stopping(&mut shutdown) => { protocol.finish(Some(status.as_u16()), code::GATEWAY_STOPPED); permits.neutral(&cfg); break; }
                    _ = sender.closed(), if !disconnected => { disconnected = true; continue; }
                    frame = tokio::time::timeout_at(limit, body.frame()) => frame,
                };
                match next {
                    Ok(Some(Ok(frame))) => {
                        if let Some(data) = frame.data_ref() {
                            protocol.feed(data);
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
            if protocol.succeeded() {
                if protocol.delivered_success() {
                    if let Some(slot) = &mut slot {
                        slot.complete();
                    }
                }
                g.persist_success(&route.provider).await;
            }
        });
        // Hyper may drop a Content-Length body immediately after its last frame,
        // without polling it again for None. Mark that frame before yielding it.
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
        return Response::from_parts(response_parts, StreamBody::new(output).boxed_unsync());
    }
    failover.finish(if ordinary_attempts > settings.max_retries {
        super::failover::Exit::AttemptsExhausted
    } else {
        super::failover::Exit::NoEligibleProvider
    });
    gateway.record_final(
        last_error.as_ref(),
        failover.failure_reported(),
        previous_provider.as_deref(),
        model.as_deref(),
        last.as_ref().map(|r| r.status().as_u16()),
        attempted,
    );
    last.unwrap_or_else(|| {
        if last_category == code::RPM_LIMIT {
            let mut response = error(
                StatusCode::TOO_MANY_REQUESTS,
                code::RPM_LIMIT,
                "供应商已达到 RPM 上限，等待后重试",
            );
            response.headers_mut().insert(
                header::RETRY_AFTER,
                rpm_retry_after
                    .unwrap_or(Duration::from_secs(1))
                    .as_secs()
                    .max(1)
                    .into(),
            );
            response
        } else {
            error(
                if attempted == 0 {
                    StatusCode::SERVICE_UNAVAILABLE
                } else {
                    StatusCode::BAD_GATEWAY
                },
                last_category,
                if attempted == 0 {
                    "没有可用供应商，请检查队列和熔断状态"
                } else {
                    "所有可用供应商均请求失败"
                },
            )
        }
    })
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
