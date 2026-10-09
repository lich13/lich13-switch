use super::{
    admission::{Admission, Budget, CapacitySource, Rejected},
    circuit::{self, Outcome, Permit},
    connector::{self, BoxError},
    model::Settings,
    replay::{self, Replay, WireBody},
    routing::{self, Requirement},
    Active, Gateway, Route,
};
use crate::events::{Action, Reason};
use http_body_util::{BodyExt, StreamBody};
use hyper::body::Body as _;
use hyper::{body::Incoming, header, HeaderMap, Request, Response, StatusCode, Uri};
use hyper_util::rt::TokioIo;
use std::{
    convert::Infallible,
    time::{Duration, Instant},
};
use tokio::sync::watch;
pub async fn serve(
    gateway: Gateway,
    listener: tokio::net::TcpListener,
    mut shutdown: watch::Receiver<bool>,
) {
    loop {
        tokio::select! {
            _=shutdown.changed()=>break,
            result=listener.accept()=>{
                let Ok((socket,_))=result else{break};let g=gateway.clone();let mut stop=shutdown.clone();
                tokio::spawn(async move{
                    let service=hyper::service::service_fn(move|request|{let g=g.clone();async move{Ok::<_,Infallible>(forward(g,request).await)}});
                    let connection=hyper::server::conn::http1::Builder::new().preserve_header_case(true)
                        .serve_connection(TokioIo::new(socket),service).with_upgrades();
                    tokio::pin!(connection);
                    tokio::select!{_= &mut connection=>(),_=stop.changed()=>{connection.as_mut().graceful_shutdown();let _=connection.await;}}
                });
            }
        }
    }
}
pub(super) fn error(status: StatusCode, code: &str, message: &str) -> Response<WireBody> {
    Response::builder().status(status).header(header::CONTENT_TYPE,"application/json")
        .body(replay::full(serde_json::to_vec(&serde_json::json!({"error":{"type":"gpt_switch_gateway","code":code,"message":message}})).unwrap())).unwrap()
}
pub(super) fn target_for(
    client: super::ClientId,
    base: &str,
    incoming: &Uri,
) -> Result<Uri, BoxError> {
    if client == super::ClientId::Codex {
        return target(base, incoming);
    }
    format!(
        "{}{}",
        base.trim_end_matches('/'),
        incoming.path_and_query().map(|p| p.as_str()).unwrap_or("/")
    )
    .parse()
    .map_err(Into::into)
}
pub(super) fn target(base: &str, incoming: &Uri) -> Result<Uri, BoxError> {
    let path = incoming.path();
    let suffix = if path == "/v1" {
        ""
    } else if let Some(s) = path.strip_prefix("/v1/") {
        return format!(
            "{}/{}{}",
            base.trim_end_matches('/'),
            s,
            incoming
                .query()
                .map(|q| format!("?{q}"))
                .unwrap_or_default()
        )
        .parse()
        .map_err(Into::into);
    } else {
        path
    };
    format!(
        "{}{}{}",
        base.trim_end_matches('/'),
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
    if headers.get_all(header::AUTHORIZATION).iter().count() != 1 {
        return false;
    }
    let expected = format!("Bearer {token}");
    let actual = headers
        .get(header::AUTHORIZATION)
        .map(|h| h.as_bytes())
        .unwrap_or_default();
    actual.len() == expected.len()
        && actual
            .iter()
            .zip(expected.as_bytes())
            .fold(0u8, |v, (a, b)| v | (a ^ b))
            == 0
}
pub(super) fn capacity_message(status: StatusCode, body: &[u8]) -> bool {
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
    let began = Instant::now();
    let (settings, mode, mut ids, token, running) = {
        let s = gateway.0.inner.lock().unwrap();
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
        return error(StatusCode::SERVICE_UNAVAILABLE, "STOPPED", "网关已停止");
    }
    if !authorized(request.headers(), &token) {
        return error(StatusCode::UNAUTHORIZED, "LOCAL_AUTH", "本地网关认证失败");
    }
    // Capture all route versions before receiving the request body: edits only affect new requests.
    let provider_ids: Vec<_> = gateway
        .0
        .inner
        .lock()
        .unwrap()
        .store
        .providers
        .iter()
        .map(|p| p.id.clone())
        .collect();
    let mut routes: std::collections::HashMap<_, _> = provider_ids
        .into_iter()
        .filter_map(|id| gateway.route(&id).map(|r| (id, r)))
        .collect();
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
    let replay = match tokio::time::timeout(
        Duration::from_secs(settings.total_seconds),
        Replay::capture(body, gateway.0.spool.path()),
    )
    .await
    {
        Ok(Ok(body)) => body,
        _ => {
            return error(
                StatusCode::BAD_REQUEST,
                "BODY",
                "请求未完整接收、超过 1 GiB 或已取消",
            )
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
    let mut pinned = (mode == "manual").then(|| ids.first().cloned()).flatten();
    let mut unknown_affinity = false;
    let continuation = match hints {
        Ok(hints) if hints.previous_response_id.is_some() => {
            let previous = hints.previous_response_id.unwrap();
            let owner = gateway
                .0
                .inner
                .lock()
                .unwrap()
                .affinity
                .get(&previous)
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
    let operation = crate::usage::model::Operation::for_path(parts.uri.path());
    let requirement = if operation == crate::usage::model::Operation::WebSearch
        || (!websocket && routing::resource(&parts.method, parts.uri.path()))
    {
        Requirement::Resource
    } else {
        Requirement::model(model.as_deref())
    };
    let usage_trace = if matches!(requirement, Requirement::Resource)
        && operation != crate::usage::model::Operation::WebSearch
    {
        None
    } else {
        gateway.usage_operation(model.as_deref(), operation)
    };
    let mut last = None;
    let mut attempted = 0usize;
    let mut previous_provider: Option<String> = None;
    let mut last_category = "NO_PROVIDER";
    let mut wait_budget = Budget::new(settings.queue_seconds);
    let mut capacity_pending = false;
    let mut capacity_retry_after: Option<Duration> = None;
    let mut rpm_retry_after: Option<Duration> = None;
    let mut capacity_sources = Vec::new();
    let mut capacity_waited = Duration::ZERO;
    while attempted <= settings.max_retries {
        // An unknown previous_response_id has no safe owner to replay against;
        // preserve the existing one-attempt rule instead of silently
        // repeating the request on an arbitrary provider.
        if (unknown_affinity || (gateway.client_id() == super::ClientId::Claude && continuation))
            && attempted > 0
        {
            break;
        }
        if ids.is_empty() {
            if capacity_pending && attempted <= settings.max_retries {
                let delay = capacity_delay(&settings, capacity_retry_after);
                let waiting_since = Instant::now();
                match gateway
                    .0
                    .admission
                    .wait_capacity(&capacity_sources, delay, settings.max_waiting)
                    .await
                {
                    Ok(()) => {}
                    Err(Rejected::Stopped) => {
                        return error(StatusCode::SERVICE_UNAVAILABLE, "STOPPED", "网关已停止")
                    }
                    Err(_) => break,
                }
                capacity_waited += waiting_since.elapsed();
                ids = gateway.routing_ids(pinned.as_deref());
                routes = ids
                    .iter()
                    .filter_map(|id| gateway.route(id).map(|r| (id.clone(), r)))
                    .collect();
                capacity_pending = false;
                capacity_retry_after = None;
                capacity_sources.clear();
                continue;
            }
            break;
        }
        let candidates: Vec<_> = ids
            .iter()
            .filter_map(|id| routes.get(id).cloned())
            .collect();
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
                    "CAPACITY",
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
                    "PROVIDERS_COOLING_DOWN",
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
                    last_category = "RPM_LIMIT";
                    break;
                }
                let mut response = error(
                    StatusCode::TOO_MANY_REQUESTS,
                    "RPM_LIMIT",
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
                    "RPM_LEDGER",
                    "RPM 状态暂不可用，请稍后重试",
                )
            }
            Err(Rejected::Stopped) => {
                return error(StatusCode::SERVICE_UNAVAILABLE, "STOPPED", "网关已停止")
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
            Err(Rejected::Unavailable) => {
                if capacity_pending {
                    ids.clear();
                    continue;
                }
                break;
            }
        };
        let route = admission.route.clone();
        let reset_generation = admission.reset_generation;
        ids.retain(|id| id != &route.provider.id);
        let uri = match target_for(route.client_id, &route.provider.base_url, &parts.uri) {
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
        if !replay.has_trailers() {
            upstream.headers_mut().insert(
                header::CONTENT_LENGTH,
                header::HeaderValue::from(replay.length),
            );
        } else {
            upstream.headers_mut().remove(header::CONTENT_LENGTH);
        }
        if admission.commit_rpm().is_err() {
            admission.permits.neutral(&settings);
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "RPM_LEDGER",
                "RPM 状态暂不可用，请稍后重试",
            );
        }
        attempted += 1;
        let rerouted = previous_provider
            .as_deref()
            .is_some_and(|id| id != admission.route.provider.id);
        previous_provider = Some(admission.route.provider.id.clone());
        let started = Instant::now();
        let mut protocol = super::protocol::Protocol::new(stream_hint);
        if let Some(trace) = &usage_trace {
            protocol.attach_usage(trace.attempt(
                &route.provider.id,
                stream_hint,
                if websocket { "websocket" } else { "http" },
            ));
        }
        let deadline = tokio::time::Instant::now()
            + Duration::from_secs(if stream_hint {
                settings.first_byte_seconds
            } else {
                settings.total_seconds
            });
        let response = tokio::time::timeout_at(deadline, route.client.request(upstream)).await;
        let mut response = match response {
            Ok(Ok(response)) => response,
            Ok(Err(e)) => {
                let category = connector::classify(&e);
                last_category = match category {
                    Some(connector::ConnectError::Tls) => "TLS",
                    _ => "NETWORK",
                };
                admission.permits.report(
                    &gateway,
                    &route,
                    model.as_deref(),
                    Reason::Network,
                    Action::TryingNext,
                    None,
                    attempted,
                    crate::events::Details {
                        phase: Some(crate::events::Phase::Connect),
                        upstream_code: Some(last_category.into()),
                        counted_failure: Some(true),
                        ..Default::default()
                    },
                );
                admission.permits.failure(&settings, None);
                protocol.finish(None, last_category);
                continue;
            }
            Err(_) => {
                last_category = "FIRST_BYTE_TIMEOUT";
                admission.permits.report(
                    &gateway,
                    &route,
                    model.as_deref(),
                    Reason::Network,
                    Action::TryingNext,
                    None,
                    attempted,
                    crate::events::Details {
                        phase: Some(crate::events::Phase::Headers),
                        upstream_code: Some("FIRST_BYTE_TIMEOUT".into()),
                        counted_failure: Some(true),
                        ..Default::default()
                    },
                );
                admission.permits.failure(&settings, None);
                protocol.finish(None, last_category);
                continue;
            }
        };
        let status = response.status();
        let response_stream = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|h| h.to_str().ok())
            .is_some_and(|h| h.starts_with("text/event-stream"));
        let encoding = response
            .headers()
            .get(header::CONTENT_ENCODING)
            .and_then(|h| h.to_str().ok())
            .unwrap_or("");
        protocol.response(status.as_u16(), response_stream, encoding);
        if status == StatusCode::SWITCHING_PROTOCOLS && websocket {
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
                            "OK"
                        } else {
                            "STREAM_INTERRUPTED"
                        },
                    );
                    if protocol.succeeded() {
                        g.successful_response(&provider_for_tunnel);
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
                    Some(attempted as u32),
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
            let captured = tokio::time::timeout(
                Duration::from_secs(settings.total_seconds),
                Replay::capture(body, gateway.0.spool.path()),
            )
            .await;
            let decoded = match &captured {
                Ok(Ok(body)) => {
                    let encoded = body.prefix(2 * 1024 * 1024).await.unwrap_or_default();
                    // Error attempts may still report billable tokens. Observe
                    // their bounded original bytes without changing replay data.
                    protocol.feed(&encoded);
                    let encoding = response_parts
                        .headers
                        .get(header::CONTENT_ENCODING)
                        .and_then(|h| h.to_str().ok())
                        .unwrap_or("identity");
                    replay::decode_prefix(&encoded, encoding, 128 * 1024).unwrap_or_default()
                }
                _ => Vec::new(),
            };
            let model_error = super::upstream_error::model_http(status.as_u16(), &decoded);
            let capacity = !model_error
                && route.client_id == super::ClientId::Codex
                && capacity_message(status, &decoded);
            let retryable = model_error || circuit::retryable(status.as_u16());
            let will_retry = retryable
                && !ids.is_empty()
                && attempted <= settings.max_retries
                && !unknown_affinity;
            let reason = if model_error {
                Reason::ModelUnavailable
            } else if status == StatusCode::TOO_MANY_REQUESTS {
                Reason::RateLimit
            } else if capacity {
                Reason::Capacity
            } else if matches!(status.as_u16(), 401 | 403) {
                Reason::Authentication
            } else {
                Reason::UpstreamService
            };
            let mut details = super::upstream_error::details_http(&decoded);
            details.phase = Some(crate::events::Phase::Response);
            details.counted_failure =
                Some(!model_error && !capacity && status.as_u16() != 429 && retryable);
            admission.permits.report(
                &gateway,
                &route,
                model.as_deref(),
                reason,
                if will_retry {
                    Action::TryingNext
                } else if capacity {
                    Action::Waiting
                } else {
                    Action::Returned
                },
                Some(status.as_u16()),
                attempted,
                details,
            );
            if model_error {
                admission.permits.neutral(&settings);
            } else if capacity {
                admission.permits.capacity_limited(&settings, cooldown);
            } else if status == StatusCode::TOO_MANY_REQUESTS {
                admission.permits.rate_limited(&settings, cooldown);
            } else if retryable {
                admission.permits.failure(&settings, cooldown);
            } else {
                admission.permits.neutral(&settings);
            }
            if let Ok(Ok(body)) = captured {
                protocol.finish(Some(status.as_u16()), "HTTP");
                last = Some(Response::from_parts(response_parts, body.body()));
            } else {
                protocol.finish(Some(status.as_u16()), "STREAM_INTERRUPTED");
            }
            last_category = "HTTP";
            if !retryable {
                return last.unwrap_or_else(|| {
                    error(
                        StatusCode::BAD_GATEWAY,
                        "UPSTREAM_BODY",
                        "上游错误响应读取失败",
                    )
                });
            }
            if capacity {
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
            .is_some_and(|v| v.starts_with("text/event-stream"));
        let (mut response_parts, mut body) = response.into_parts();
        clean_headers(&mut response_parts.headers, false);
        let first_deadline = if stream {
            tokio::time::Instant::now()
                + Duration::from_secs(settings.first_byte_seconds).saturating_sub(started.elapsed())
        } else {
            deadline
        };
        // Buffer only the first complete SSE event (or bounded JSON); preserve
        // the original frames and encoding exactly, including comments/trailers.
        let json = response_parts
            .headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.contains("json"));
        let mut prefix = Vec::new();
        let mut prefix_bytes = 0;
        let mut ended = false;
        let mut failed = false;
        let mut failure_kind = "FIRST_BYTE_TIMEOUT";
        loop {
            match tokio::time::timeout_at(first_deadline, body.frame()).await {
                Ok(Some(Ok(frame))) => {
                    if let Some(bytes) = frame.data_ref() {
                        prefix_bytes += bytes.len();
                        protocol.feed(bytes);
                    }
                    prefix.push(frame);
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
                    failure_kind = if result.is_err() {
                        "FIRST_BYTE_TIMEOUT"
                    } else {
                        "STREAM_INTERRUPTED"
                    };
                    break;
                }
            }
        }
        if failed {
            protocol.finish(Some(status.as_u16()), failure_kind);
        }
        if failed && protocol.transport_failure() {
            admission.permits.report(
                &gateway,
                &route,
                model.as_deref(),
                Reason::Network,
                Action::TryingNext,
                Some(status.as_u16()),
                attempted,
                crate::events::Details {
                    phase: Some(crate::events::Phase::Response),
                    upstream_code: Some(failure_kind.into()),
                    counted_failure: Some(true),
                    ..Default::default()
                },
            );
            admission.permits.failure(&settings, None);
            last_category = failure_kind;
            continue;
        }
        ended |= failed;
        if ended {
            protocol.finish(
                Some(status.as_u16()),
                if status.is_success() { "OK" } else { "HTTP" },
            );
        }
        let initial_model_error = if stream {
            protocol.observation.first_event_model_error == Some(true)
        } else {
            ended && protocol.terminal() == Some(super::protocol::Terminal::ModelUnavailable)
        };
        if initial_model_error {
            admission.permits.neutral(&settings);
            let will_retry =
                !ids.is_empty() && attempted <= settings.max_retries && !unknown_affinity;
            let mut details = protocol.observation.error.clone();
            details.phase = Some(crate::events::Phase::Response);
            details.counted_failure = Some(false);
            admission.permits.report(
                &gateway,
                &route,
                model.as_deref(),
                Reason::ModelUnavailable,
                if will_retry {
                    Action::TryingNext
                } else {
                    Action::Returned
                },
                Some(status.as_u16()),
                attempted,
                details,
            );
            // Preserve a bounded error prefix. Dropping its unread tail cancels the
            // rejected attempt; if no alternative succeeds, return the original stream.
            let error_idle_seconds = settings.idle_seconds;
            let rejected = async_stream::try_stream! {
                for frame in prefix {yield frame;}
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
            last_category = "MODEL_UNAVAILABLE";
            continue;
        }
        let total_deadline = tokio::time::Instant::now()
            + Duration::from_secs(settings.total_seconds)
                .saturating_sub(began.elapsed().saturating_sub(capacity_waited));
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
                Some(attempted as u32),
            );
        }
        let output = async_stream::try_stream! {
            let _active=active;
            let _slot=slot;
            remember(&protocol,&g,&route,model.as_deref());
            observe_protocol(&protocol,&mut permits,&cfg,&g,&route,model.as_deref(),Some(status.as_u16()),attempted);
            if ended && protocol.succeeded(){g.successful_response(&route.provider);}
            for frame in prefix {yield frame;}
            if ended {return;}
            loop {
                let limit=if stream {tokio::time::Instant::now()+Duration::from_secs(cfg.idle_seconds)}else{total_deadline};
                match tokio::time::timeout_at(limit,body.frame()).await {
                    Ok(Some(Ok(frame)))=>{
                        if let Some(data)=frame.data_ref(){protocol.feed(data);remember(&protocol,&g,&route,model.as_deref());observe_protocol(&protocol,&mut permits,&cfg,&g,&route,model.as_deref(),Some(status.as_u16()),attempted);}
                        let complete=body.is_end_stream();
                        if complete {protocol.finish(Some(status.as_u16()),if neutral{"HTTP"}else{"OK"});remember(&protocol,&g,&route,model.as_deref());observe_protocol(&protocol,&mut permits,&cfg,&g,&route,model.as_deref(),Some(status.as_u16()),attempted);if protocol.succeeded() { g.successful_response(&route.provider); } }
                        yield frame;
                        if complete {break;}
                    }
                    Ok(None)=>{protocol.finish(Some(status.as_u16()),if neutral{"HTTP"}else{"OK"});remember(&protocol,&g,&route,model.as_deref());observe_protocol(&protocol,&mut permits,&cfg,&g,&route,model.as_deref(),Some(status.as_u16()),attempted);if protocol.succeeded() { g.successful_response(&route.provider); }
                        break;}
                    result=>{
                        let category=if result.is_err(){"STREAM_TIMEOUT"}else{"STREAM_INTERRUPTED"};
                        protocol.finish(Some(status.as_u16()),category);
                        observe_protocol(&protocol,&mut permits,&cfg,&g,&route,model.as_deref(),Some(status.as_u16()),attempted);
                        Err::<(),BoxError>(std::io::Error::other("上游流中断").into())?;
                    }
                }
            }
        };
        return Response::from_parts(response_parts, StreamBody::new(output).boxed_unsync());
    }
    gateway.record(
        None,
        model.as_deref(),
        Reason::FailoverExhausted,
        Action::Returned,
        last.as_ref().map(|r| r.status().as_u16()),
        Some(attempted as u32),
    );
    last.unwrap_or_else(|| {
        if last_category == "RPM_LIMIT" {
            let mut response = error(
                StatusCode::TOO_MANY_REQUESTS,
                "RPM_LIMIT",
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
fn remember(protocol: &super::protocol::Protocol, g: &Gateway, route: &Route, model: Option<&str>) {
    if let Some(id) = &protocol.observation.response_id {
        g.remember_model(
            id,
            &route.provider.id,
            protocol.observation.model.as_deref().or(model),
        );
    }
}
#[allow(clippy::too_many_arguments)]
pub(super) fn observe_protocol(
    protocol: &super::protocol::Protocol,
    permits: &mut Permits,
    cfg: &Settings,
    g: &Gateway,
    route: &Route,
    model: Option<&str>,
    status: Option<u16>,
    attempt: usize,
) {
    if permits.provider.is_some() {
        let reason = match protocol.terminal() {
            Some(super::protocol::Terminal::ModelUnavailable) => Some(Reason::ModelUnavailable),
            Some(super::protocol::Terminal::Failure) if protocol.transport_failure() => {
                Some(Reason::Network)
            }
            Some(super::protocol::Terminal::Failure) if status.is_some_and(|value| value < 400) => {
                Some(Reason::ProtocolError)
            }
            Some(super::protocol::Terminal::Failure) => Some(Reason::UpstreamService),
            Some(super::protocol::Terminal::Rejected)
                if protocol.observation.error != crate::events::Details::default() =>
            {
                Some(Reason::ProtocolError)
            }
            _ => None,
        };
        if let Some(reason) = reason {
            let mut details = protocol.observation.error.clone();
            details.phase = Some(crate::events::Phase::Stream);
            details.counted_failure =
                Some(reason == Reason::Network || reason == Reason::UpstreamService);
            permits.report(
                g,
                route,
                model,
                reason,
                Action::Returned,
                status,
                attempt,
                details,
            );
        }
    }
    settle_protocol(protocol, permits, cfg, status);
}
pub(super) fn settle_protocol(
    protocol: &super::protocol::Protocol,
    permits: &mut Permits,
    cfg: &Settings,
    status: Option<u16>,
) {
    use super::protocol::Terminal;
    match protocol.terminal() {
        Some(Terminal::Success | Terminal::Limited) => permits.success(cfg),
        Some(Terminal::Failure)
            if protocol.transport_failure() || status.is_none_or(|value| value >= 400) =>
        {
            permits.failure(cfg, None)
        }
        Some(Terminal::Failure) => permits.neutral(cfg),
        Some(_) => permits.neutral(cfg),
        None => (),
    }
}
pub(super) struct Permits {
    provider: Option<Permit>,
}
impl Permits {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn report(
        &mut self,
        g: &Gateway,
        route: &Route,
        model: Option<&str>,
        reason: Reason,
        action: Action,
        status: Option<u16>,
        attempt: usize,
        details: crate::events::Details,
    ) {
        let mut event = crate::events::Record::new(
            Some(g.0.client),
            Some(&route.provider.id),
            model,
            reason,
            action,
            status,
            Some(attempt as u32),
        );
        event.details = details.sanitized();
        if let Some(p) = &mut self.provider {
            p.set_failure_event(event.clone());
        }
        if let Some(service) = g.0.diagnostics.lock().unwrap().as_ref() {
            service.emit(event);
        }
    }

    pub(super) fn capacity_limited(&mut self, cfg: &Settings, retry: Option<Duration>) {
        if let Some(p) = self.provider.take() {
            p.finish(Outcome::CapacityLimited(retry), cfg);
        }
    }
    pub(super) fn rate_limited(&mut self, cfg: &Settings, retry: Option<Duration>) {
        if let Some(p) = self.provider.take() {
            p.finish(Outcome::RateLimited(retry), cfg);
        }
    }
    pub(super) fn acquire(route: &Route, manual: bool) -> Option<Self> {
        Some(Self {
            provider: Some(route.provider_circuit.acquire(manual)?),
        })
    }
    pub(super) fn success(&mut self, cfg: &Settings) {
        if let Some(p) = self.provider.take() {
            p.finish(Outcome::Success, cfg);
        }
    }
    pub(super) fn neutral(&mut self, cfg: &Settings) {
        if let Some(p) = self.provider.take() {
            p.finish(Outcome::Neutral, cfg);
        }
    }
    pub(super) fn failure(&mut self, cfg: &Settings, retry: Option<Duration>) {
        if let Some(p) = self.provider.take() {
            p.finish(Outcome::Failure(retry), cfg);
        }
    }
}
