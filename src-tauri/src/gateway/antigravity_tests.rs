use super::*;
use crate::gateway::{grok_protocol_tests::server, Gateway};
use parking_lot::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn key(project: Option<&str>, location: &str) -> VertexKey {
    VertexKey {
        api_key: "fixture-vertex-key".into(),
        project_id: project.map(str::to_owned),
        location: location.into(),
        models: vec!["gemini-3-flash-preview".into(), "gemini-2.5-flash".into()],
    }
}
struct Fixture {
    _dir: tempfile::TempDir,
    gateway: Gateway,
    session: Arc<Session>,
}
impl Fixture {
    async fn new(key: VertexKey, origin: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let gateway = Gateway::new(dir.path().to_path_buf()).unwrap();
        let registry = gateway.registry();
        let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = socket.local_addr().unwrap().port();
        drop(socket);
        gateway
            .set_listener_port(port, &gateway.listener_view().revision)
            .unwrap();
        let prepared = registry.listener.prepare().await.unwrap();
        let mut session = Session::new(key, "fixture-local-token".into(), &registry);
        session.origin = Some(origin.into());
        let session = Arc::new(session);
        registry
            .listener
            .activate_antigravity(prepared.socket, registry.clone(), || {
                *registry.antigravity.lock() = Some(session.clone());
                Ok(())
            })
            .unwrap();
        Self {
            _dir: dir,
            gateway,
            session,
        }
    }
    fn url(&self, path: &str) -> String {
        format!(
            "http://127.0.0.1:{}/antigravity{path}",
            self.gateway.listener_view().port
        )
    }
    fn post(&self, path: &str) -> reqwest::RequestBuilder {
        reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .post(self.url(path))
            .bearer_auth("fixture-local-token")
    }
    async fn stop(&self) {
        stop(&self.gateway.registry()).await.unwrap();
    }
}
#[test]
fn vertex_endpoints_use_express_or_project_region() {
    assert_eq!(key(None,"us-central1").endpoint("gemini-2.5-flash","countTokens"), "https://aiplatform.googleapis.com/v1/publishers/google/models/gemini-2.5-flash:countTokens");
    assert_eq!(key(Some("fixture-project"),"global").endpoint("gemini-2.5-flash","generateContent"), "https://aiplatform.googleapis.com/v1/projects/fixture-project/locations/global/publishers/google/models/gemini-2.5-flash:generateContent");
    assert_eq!(key(Some("fixture-project"),"us-central1").endpoint("gemini-2.5-flash","generateContent"), "https://us-central1-aiplatform.googleapis.com/v1/projects/fixture-project/locations/us-central1/publishers/google/models/gemini-2.5-flash:generateContent");
}
#[tokio::test]
async fn forwards_paths_auth_bytes_catalog_telemetry_and_real_errors() {
    for project in [None, Some("fixture-project")] {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let captured = seen.clone();
        let upstream = server(move |request| {
            let captured = captured.clone();
            async move {
                let (parts, body) = request.into_parts();
                assert_eq!(parts.headers["x-goog-api-key"], "fixture-vertex-key");
                assert!(!parts.headers.contains_key("authorization"));
                assert_eq!(parts.headers["user-agent"], "fixture-antigravity");
                assert_eq!(parts.headers["x-goog-api-client"], "fixture-client");
                let body = body.collect().await.unwrap().to_bytes().to_vec();
                captured.lock().push((parts.uri.to_string(), body));
                Response::builder().status(403).header("content-type","application/json")
                    .body(replay::full(r#"{"error":{"status":"PERMISSION_DENIED","message":"fixture upstream denial"}}"#)).unwrap()
            }
        }).await;
        let fixture = Fixture::new(key(project, "global"), &upstream.url("")).await;
        for action in ["generateContent", "streamGenerateContent", "countTokens"] {
            let body = b" {\"contents\":[],\"fixture\":9007199254740993} ";
            let response = fixture
                .post(&format!("/v1beta/models/gemini-2.5-flash:{action}"))
                .header("user-agent", "fixture-antigravity")
                .header("x-goog-api-client", "fixture-client")
                .body(body.to_vec())
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), 403);
            assert!(response
                .text()
                .await
                .unwrap()
                .contains("fixture upstream denial"));
            let observed = seen.lock();
            let (path, bytes) = observed.last().unwrap();
            assert_eq!(bytes, body);
            assert!(path.contains(if project.is_some() {
                "/projects/fixture-project/locations/global/publishers/google/models/"
            } else {
                "/v1/publishers/google/models/"
            }));
            assert_eq!(
                path.ends_with("?alt=sse"),
                action == "streamGenerateContent"
            );
        }
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let catalog: serde_json::Value = client
            .get(fixture.url("/v1beta/models"))
            .bearer_auth("fixture-local-token")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            catalog["models"][0]["name"],
            "models/gemini-3-flash-preview"
        );
        assert!(!catalog.to_string().contains("fixture-vertex-key"));
        assert_eq!(
            fixture
                .post("/v1beta:recordCodeAssistMetrics")
                .body("{}")
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap(),
            "{}"
        );
        assert_eq!(
            client
                .get(fixture.url("/v1beta/models"))
                .send()
                .await
                .unwrap()
                .status(),
            401
        );
        for alias in ["/v1/models", "/models"] {
            assert_eq!(
                client
                    .get(fixture.url(alias))
                    .bearer_auth("fixture-local-token")
                    .send()
                    .await
                    .unwrap()
                    .status(),
                404
            );
        }
        assert_eq!(seen.lock().len(), 3);
        fixture.stop().await;
    }
}
#[tokio::test]
async fn streams_before_completion_and_cancel_releases_global_freeze() {
    let release = Arc::new(tokio::sync::Notify::new());
    let upstream_release = release.clone();
    let upstream = server(move |_| {
        let release = upstream_release.clone();
        async move {
            let frames = async_stream::try_stream! {
                yield hyper::body::Frame::data(bytes::Bytes::from_static(b"data: {\"fixture\":1}\n\n"));
                release.notified().await;
                yield hyper::body::Frame::data(bytes::Bytes::from_static(b"data: {\"fixture\":2}\n\n"));
            };
            Response::builder().header("content-type","text/event-stream").body(http_body_util::StreamBody::new(frames).boxed_unsync()).unwrap()
        }
    }).await;
    let fixture = Fixture::new(key(None, "global"), &upstream.url("")).await;
    let port = fixture.gateway.listener_view().port;
    let mut response = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    // A read-side FIN can be a valid HTTP half-close; cancellation uses RST.
    #[allow(deprecated)]
    response.set_linger(Some(Duration::ZERO)).unwrap();
    response.write_all(format!("POST /antigravity/v1beta/models/gemini-2.5-flash:streamGenerateContent HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer fixture-local-token\r\nContent-Length: 2\r\n\r\n{{}}").as_bytes()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(1), async {
        let mut received = Vec::new();
        while !received.windows(7).any(|part| part == b"fixture") {
            let mut bytes = [0; 1024];
            let count = response.read(&mut bytes).await.unwrap();
            assert_ne!(count, 0);
            received.extend_from_slice(&bytes[..count]);
        }
    })
    .await
    .unwrap();
    let gate = fixture.gateway.freeze_gate();
    assert_eq!(gate.set(true).unwrap().draining, 1);
    drop(response);
    tokio::time::timeout(Duration::from_secs(2), async {
        while gate.view().draining != 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    gate.set(false).unwrap();
    release.notify_waiters();
    fixture.stop().await;
}
#[tokio::test]
async fn shared_listener_stays_active_and_stopping_cancels_frozen_requests() {
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let upstream = server(move |_| {
        let count = count.clone();
        async move {
            count.fetch_add(1, Ordering::SeqCst);
            json_response(json!({}))
        }
    })
    .await;
    let fixture = Fixture::new(key(None, "global"), &upstream.url("")).await;
    let registry = fixture.gateway.registry();
    assert!(registry.listener.view().unwrap().antigravity_active);
    assert!(fixture
        .gateway
        .set_listener_port(43721, &fixture.gateway.listener_view().revision)
        .is_err());
    registry
        .listener
        .deactivate(crate::gateway::ClientId::Codex)
        .await
        .unwrap();
    let gate = fixture.gateway.freeze_gate();
    gate.set(true).unwrap();
    let request = fixture
        .post("/v1beta/models/gemini-2.5-flash:generateContent")
        .body("{}");
    let pending = tokio::spawn(async move { request.send().await.unwrap() });
    tokio::time::timeout(Duration::from_secs(2), async {
        while gate.view().queued != 1 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    fixture.session.stop();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), pending)
            .await
            .unwrap()
            .unwrap()
            .status(),
        503
    );
    fixture.stop().await;
    assert!(!registry.listener.view().unwrap().antigravity_active);
    assert_eq!(gate.view().queued, 0);
}
