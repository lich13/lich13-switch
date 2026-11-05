//! Antigravity's enterprise gateway protocol, isolated from provider scheduling.
use super::{
    connector::{BoxError, Connector},
    forward::{clean_headers, error},
    freeze, listener,
    registry::Registry,
    replay::{self, WireBody},
    HttpClient,
};
use crate::{
    errors::code,
    storage::{AppError, Result},
};
use http_body_util::BodyExt;
use hyper::body::Body as _;
use hyper::{body::Incoming, header, Method, Request, Response, StatusCode, Uri};
use hyper_util::{client::legacy::Client, rt::TokioExecutor};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{sync::Arc, time::Duration};
use tokio::sync::watch;

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct VertexKey {
    pub api_key: String,
    pub project_id: Option<String>,
    pub location: String,
    pub models: Vec<String>,
}
impl VertexKey {
    pub fn validate(&mut self) -> Result<()> {
        self.api_key = self.api_key.trim().to_owned();
        self.project_id = self
            .project_id
            .take()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty());
        self.location = self.location.trim().to_owned();
        if self.location.is_empty() {
            self.location = "global".into();
        }
        for model in &mut self.models {
            *model = model.trim().to_owned();
        }
        self.models.retain(|m| !m.is_empty());
        let mut seen = std::collections::HashSet::new();
        self.models.retain(|model| seen.insert(model.clone()));
        if self.api_key.is_empty() || header::HeaderValue::from_str(&self.api_key).is_err() {
            return Err(AppError::new(code::AUTH, "请输入有效的 Vertex API Key"));
        }
        if self.project_id.as_deref().is_some_and(|s| !segment(s)) || !segment(&self.location) {
            return Err(AppError::new(code::SETTINGS, "Project ID 或地区格式无效"));
        }
        if self.models.is_empty() || self.models.iter().any(|m| !segment(m)) {
            return Err(AppError::new(code::SETTINGS, "请填写有效的模型名称"));
        }
        Ok(())
    }
    fn endpoint(&self, model: &str, action: &str) -> String {
        let resource = if let Some(project) = &self.project_id {
            format!(
                "projects/{project}/locations/{}/publishers/google/models/{model}:{action}",
                self.location
            )
        } else {
            format!("publishers/google/models/{model}:{action}")
        };
        let host = if self.project_id.is_some() && self.location != "global" {
            format!("{}-aiplatform.googleapis.com", self.location)
        } else {
            "aiplatform.googleapis.com".into()
        };
        format!("https://{host}/v1/{resource}")
    }
}
fn segment(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
        && value != "."
        && value != ".."
}
pub(crate) struct Session {
    pub key: VertexKey,
    token: String,
    client: HttpClient,
    stop: watch::Sender<bool>,
    #[cfg(test)]
    origin: Option<String>,
}
impl Session {
    pub fn new(key: VertexKey, token: String, registry: &Registry) -> Self {
        let client = super::locks::published(&registry.antigravity)
            .as_ref()
            .map(|s| s.client.clone())
            .unwrap_or_else(|| {
                Client::builder(TokioExecutor::new())
                    .pool_idle_timeout(Duration::from_secs(60))
                    .pool_max_idle_per_host(8)
                    .retry_canceled_requests(false)
                    .build(
                        Connector::new(Duration::from_secs(300), 0)
                            .with_ports(registry.ports.clone()),
                    )
            });
        Self {
            key,
            token,
            client,
            stop: watch::channel(false).0,
            #[cfg(test)]
            origin: None,
        }
    }
    pub fn stop(&self) {
        self.stop.send_replace(true);
    }
}
pub(crate) async fn stop(registry: &Registry) -> Result<()> {
    if let Some(session) = super::locks::published(&registry.antigravity).take() {
        session.stop();
    }
    let result = registry.listener.deactivate_antigravity().await;
    registry.changed();
    result
}
fn json_response(value: serde_json::Value) -> Response<WireBody> {
    Response::builder()
        .header(header::CONTENT_TYPE, "application/json")
        .body(replay::full(value.to_string()))
        .expect("static response headers")
}
pub(super) async fn forward(
    session: Option<Arc<Session>>,
    freeze: freeze::Gate,
    request: Request<Incoming>,
) -> Response<WireBody> {
    let Some(session) = session else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            code::STOPPED,
            "Antigravity 转发已停止",
        );
    };
    let mut auth = request.headers().get_all(header::AUTHORIZATION).iter();
    if !auth
        .next()
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|token| listener::token_matches(token.as_bytes(), session.token.as_bytes()))
        || auth.next().is_some()
    {
        return error(
            StatusCode::UNAUTHORIZED,
            code::LOCAL_AUTH,
            "Antigravity 本地认证失败",
        );
    }
    let path = request
        .uri()
        .path()
        .strip_prefix("/antigravity")
        .unwrap_or("");
    if request.method() == Method::GET
        && matches!(path, "/v1beta/models" | "/v1/models" | "/models")
    {
        return json_response(
            json!({"models": session.key.models.iter().map(|model| json!({
            "name": format!("models/{model}"), "displayName": model,
            "supportedGenerationMethods": ["generateContent", "streamGenerateContent", "countTokens"]
        })).collect::<Vec<_>>()}),
        );
    }
    if request.method() == Method::POST && path.starts_with("/v1beta:") {
        return json_response(json!({}));
    }
    let inference = path
        .rsplit_once("/models/")
        .and_then(|(_, tail)| tail.split_once(':'));
    let Some((model, action)) = inference.filter(|(m, a)| {
        segment(m)
            && matches!(
                *a,
                "generateContent" | "streamGenerateContent" | "countTokens"
            )
    }) else {
        return error(
            StatusCode::NOT_FOUND,
            code::CLIENT,
            "Antigravity 请求路径不存在",
        );
    };
    if request.method() != Method::POST {
        return error(
            StatusCode::METHOD_NOT_ALLOWED,
            code::CLIENT,
            "此接口需要 POST 请求",
        );
    }
    let mut target = session.key.endpoint(model, action);
    #[cfg(test)]
    if let Some(origin) = &session.origin {
        target = format!("{origin}{}", target.parse::<Uri>().unwrap().path());
    }
    if action == "streamGenerateContent" {
        target.push_str("?alt=sse");
    }
    let uri: Uri = match target.parse() {
        Ok(uri) => uri,
        Err(_) => {
            return error(
                StatusCode::BAD_REQUEST,
                code::SETTINGS,
                "Vertex 请求地址无效",
            )
        }
    };
    let delivery = request
        .extensions()
        .get::<Arc<super::protocol::Delivery>>()
        .cloned()
        .unwrap_or_default();
    let activity = request
        .extensions()
        .get::<Arc<super::wire_progress::Activity>>()
        .cloned()
        .unwrap_or_default();
    let (mut parts, body) = request.into_parts();
    parts.uri = uri;
    clean_headers(&mut parts.headers, false);
    for name in [
        "authorization",
        "x-api-key",
        "x-goog-api-key",
        "cookie",
        "host",
    ] {
        parts.headers.remove(name);
    }
    parts.headers.insert(
        "x-goog-api-key",
        header::HeaderValue::from_str(&session.key.api_key).expect("validated API key"),
    );
    let request = Request::from_parts(
        parts,
        body.map_err(|e| -> BoxError { Box::new(e) }).boxed_unsync(),
    );
    let ticket = freeze.request();
    ticket.waiting();
    let mut changes = ticket.subscribe();
    let mut stop = session.stop.subscribe();
    loop {
        if *stop.borrow() {
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                code::STOPPED,
                "Antigravity 转发已停止",
            );
        }
        if ticket.dispatch() {
            break;
        }
        tokio::select! {
            _ = changes.changed() => {},
            _ = stop.changed() => return error(StatusCode::SERVICE_UNAVAILABLE, code::STOPPED, "Antigravity 转发已停止"),
        }
    }
    if *stop.borrow() {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            code::STOPPED,
            "Antigravity 转发已停止",
        );
    }
    let response = tokio::select! {
        _ = stop.changed() => return error(StatusCode::SERVICE_UNAVAILABLE, code::STOPPED, "Antigravity 转发已停止"),
        response = tokio::time::timeout(Duration::from_secs(300), session.client.request(request)) => response,
    };
    let response = match response {
        Ok(Ok(response)) => response,
        Ok(Err(_)) => {
            return error(
                StatusCode::BAD_GATEWAY,
                code::CONNECTION_FAILED,
                "无法连接 Vertex 上游",
            )
        }
        Err(_) => {
            return error(
                StatusCode::GATEWAY_TIMEOUT,
                code::FIRST_BYTE_TIMEOUT,
                "Vertex 响应超时",
            )
        }
    };
    let (mut parts, mut body) = response.into_parts();
    clean_headers(&mut parts.headers, false);
    ticket.streaming();
    let mut guard = super::attempt::DeliveryGuard {
        delivery: delivery.clone(),
        complete: false,
    };
    if body.is_end_stream() {
        guard.complete = true;
        complete_delivery(&ticket, &delivery, &activity, stop.clone());
    }
    let frames = async_stream::try_stream! {
        let _session = session;
        let mut guard = guard;
        loop {
            let next = tokio::select! {
                _ = stop.changed() => Err(std::io::Error::new(std::io::ErrorKind::Interrupted, "Antigravity forwarding stopped")),
                frame = tokio::time::timeout(Duration::from_secs(300), body.frame()) => frame.map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "Vertex stream timeout")),
            };
            match next? {
                Some(frame) => {
                    let frame = frame.map_err(|e| -> BoxError { Box::new(e) })?;
                    delivery.handed(frame.data_ref().map_or(0, bytes::Bytes::len), false);
                    if body.is_end_stream() && !guard.complete {
                        guard.complete = true;
                        complete_delivery(&ticket, &delivery, &activity, stop.clone());
                    }
                    yield frame;
                },
                None => break,
            }
        }
        if !guard.complete {
            guard.complete = true;
            complete_delivery(&ticket, &delivery, &activity, stop);
        }
    };
    Response::from_parts(
        parts,
        http_body_util::StreamBody::new(frames).boxed_unsync(),
    )
}

fn complete_delivery(
    ticket: &freeze::Ticket,
    delivery: &Arc<super::protocol::Delivery>,
    activity: &Arc<super::wire_progress::Activity>,
    mut stop: watch::Receiver<bool>,
) {
    delivery.body_complete();
    ticket.finishing();
    let (ticket, delivery, activity) = (ticket.clone(), delivery.clone(), activity.clone());
    tokio::spawn(async move {
        tokio::select! {
            _ = stop.changed() => {},
            _ = super::wire_progress::writing(delivery.wait(), &activity, 300) => {},
        }
        ticket.finish();
    });
}

#[cfg(test)]
#[path = "antigravity_tests.rs"]
mod tests;
