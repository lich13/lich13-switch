use super::*;
use http_body_util::BodyExt;
use serde_json::json;

async fn fixture() -> (tempfile::TempDir, Gateway, Gateway, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let root = Gateway::new(temp.path().join("data")).unwrap();
    let agy = root.agy_companion(temp.path().join("data/agy")).unwrap();
    let home = temp.path().join("data/agy/runtime");
    let reservation = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    root.set_listener_port(
        reservation.local_addr().unwrap().port(),
        &root.listener_view().revision,
    )
    .unwrap();
    (temp, root, agy, home)
}
fn save(gateway: &Gateway, home: &Path, base: &str) {
    gateway
        .edit(
            Edit::SaveProvider {
                id: None,
                name: Some("Fixture".into()),
                base_url: base.into(),
                token: "fixture-upstream".into(),
                quota_access_token: None,
                search_only: None,
            },
            &gateway.view().revision,
            home,
        )
        .unwrap();
}
#[tokio::test]
async fn native_stream_keeps_wire_bytes_and_isolates_authentication() {
    let (_temp, root, agy, home) = fixture().await;
    let upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!(
        "http://127.0.0.1:{}/prefix",
        upstream.local_addr().unwrap().port()
    );
    let (sent, received) = tokio::sync::oneshot::channel();
    let body = "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"fixture\",\"thoughtSignature\":\"opaque-fixture\"}]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":100,\"cachedContentTokenCount\":20,\"candidatesTokenCount\":5,\"thoughtsTokenCount\":7}}\n\n";
    let server = tokio::spawn(async move {
        let (socket, _) = upstream.accept().await.unwrap();
        let sent = Arc::new(Mutex::new(Some(sent)));
        let service =
            hyper::service::service_fn(move |request: hyper::Request<hyper::body::Incoming>| {
                let sent = sent.clone();
                async move {
                    let (parts, request) = request.into_parts();
                    let request = request.collect().await.unwrap().to_bytes();
                    sent.lock()
                        .unwrap()
                        .take()
                        .unwrap()
                        .send((parts.uri.to_string(), parts.headers, request))
                        .unwrap();
                    Ok::<_, std::convert::Infallible>(
                        hyper::Response::builder()
                            .header("content-type", "text/event-stream")
                            .body(replay::full(body))
                            .unwrap(),
                    )
                }
            });
        let _ = hyper::server::conn::http1::Builder::new()
            .serve_connection(hyper_util::rt::TokioIo::new(socket), service)
            .await;
    });
    save(&agy, &home, &base);
    agy.start(&agy.view().revision, &home).await.unwrap();
    assert_eq!(agy.view().listener.running_clients, [ClientId::Agy]);
    assert!(!root.view().running);
    assert!(!ClientId::Agy.config(&home).exists());
    let key = agy.0.inner.lock().unwrap().store.local_token.clone();
    let request = r#" {"contents":[{"role":"user","parts":[{"text":"fixture"},{"functionResponse":{"name":"fixture","response":{"ok":true}}}]}],"unknown":9007199254740993} "#;
    let http = reqwest::Client::builder().no_proxy().build().unwrap();
    let url = format!(
        "{}/v1beta/models/gemini-fixture:streamGenerateContent?alt=sse&key=fixture-local",
        agy.view().address
    );
    let response = http
        .post(&url)
        .header("x-goog-api-key", &key)
        .body(request)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await.unwrap(), body);
    let (path, headers, wire) = received.await.unwrap();
    assert_eq!(
        path,
        "/prefix/v1beta/models/gemini-fixture:streamGenerateContent?alt=sse"
    );
    assert_eq!(headers["x-goog-api-key"], "fixture-upstream");
    assert!(!headers.contains_key("authorization"));
    assert!(!headers.contains_key("x-api-key"));
    assert_eq!(wire.as_ref(), request.as_bytes());
    let wrong = http
        .post(&url)
        .header("x-goog-api-key", &key)
        .bearer_auth("fixture-wrong")
        .body(request)
        .send()
        .await
        .unwrap();
    assert_eq!(wrong.status(), 401);
    agy.stop().await.unwrap();
    assert!(
        !agy.0
            .inner
            .lock()
            .unwrap()
            .store
            .resume
            .as_ref()
            .unwrap()
            .desired
    );
    server.abort();
}
#[tokio::test]
async fn isolated_config_keeps_unknown_fields_and_sessions_across_switches() {
    let (_temp, _root, agy, home) = fixture().await;
    save(&agy, &home, "https://example.invalid/api");
    assert!(agy
        .prepare_agy(&home, "gemini-fixture", &agy.view().revision)
        .is_err());
    agy.start(&agy.view().revision, &home).await.unwrap();
    std::fs::create_dir_all(home.join("antigravity-cli/brain")).unwrap();
    let path = ClientId::Agy.config(&home);
    std::fs::write(
        &path,
        r#"{"future":{"preserve":true},"permissions":{"deny":["command(rm)"]}}"#,
    )
    .unwrap();
    agy.prepare_agy(&home, "gemini-fixture", &agy.view().revision)
        .unwrap();
    let config: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(config["future"]["preserve"], true);
    assert_eq!(config["modelProvider"], "gemini");
    assert!(!config.to_string().contains("fixture-upstream"));
    let local = agy.0.inner.lock().unwrap().store.local_token.clone();
    assert!(!config.to_string().contains(&local));
    let before = std::fs::read(&path).unwrap();
    assert!(agy
        .prepare_agy(&home, "../bad", &agy.view().revision)
        .is_err());
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert!(agy::guard_config(&home, "{}").is_err());
    agy.stop().await.unwrap();
    let id = agy.view().providers[0].id.clone();
    agy.edit(Edit::DeleteProvider { id }, &agy.view().revision, &home)
        .unwrap();
    assert!(home.join("antigravity-cli/brain").exists());
    assert_eq!(std::fs::read(&path).unwrap(), before);
}
#[test]
fn gemini_observation_normalizes_cumulative_usage_and_requires_a_terminal() {
    let mut p = protocol::Protocol::new(true);
    p.response(200, true, "");
    p.feed(b"data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"fixture\"}]}}]}\n\n");
    assert!(p.observation.terminal.is_none());
    let frame = json!({"candidates":[{"finishReason":"STOP"}],"modelVersion":"gemini-fixture","usageMetadata":{"promptTokenCount":100,"cachedContentTokenCount":20,"candidatesTokenCount":5,"thoughtsTokenCount":7}});
    let s = format!("data: {frame}\n\n");
    p.feed(s.as_bytes());
    p.feed(s.as_bytes());
    p.finish(Some(200), code::OK);
    assert_eq!(p.observation.terminal, Some(protocol::Terminal::Success));
    let tokens = &p.observation.meter.tokens;
    assert_eq!(
        (
            tokens.input,
            tokens.cache_read,
            tokens.output,
            tokens.reasoning
        ),
        (Some(80), Some(20), Some(12), Some(7))
    );
    assert_eq!(tokens.total(), Some(112));
    let mut interrupted = protocol::Protocol::new(true);
    interrupted.response(200, true, "");
    interrupted
        .feed(b"data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"partial\"}]}}]}\n\n");
    interrupted.finish(Some(200), code::OK);
    assert_ne!(
        interrupted.observation.terminal,
        Some(protocol::Terminal::Success)
    );
}
#[tokio::test]
async fn imports_are_scoped_by_target_and_protocol_without_activating() {
    let (_temp, root, agy, home) = fixture().await;
    let grok = root.grok_companion(root.0.data.join("grok")).unwrap();
    for protocol in [
        provider_protocol::Protocol::Responses,
        provider_protocol::Protocol::Messages,
    ] {
        grok.edit(
            Edit::ImportLink {
                base_url: "https://example.invalid/api".into(),
                token: "fixture-key".into(),
                display_name: "Fixture".into(),
                protocol,
                model: None,
            },
            &grok.view().revision,
            &home,
        )
        .unwrap();
    }
    assert_eq!(grok.view().providers.len(), 2);
    assert!(grok.view().selected.is_none());
    let invalid = Edit::ImportLink {
        base_url: "https://example.invalid".into(),
        token: "fixture-key".into(),
        display_name: "Fixture".into(),
        protocol: provider_protocol::Protocol::Responses,
        model: None,
    };
    assert!(agy.edit(invalid, &agy.view().revision, &home).is_err());
    assert!(!ClientId::Agy.config(&home).exists());
}

#[tokio::test]
async fn native_catalog_follows_page_tokens_and_preserves_model_names() {
    let (_temp, _root, agy, home) = fixture().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!(
        "http://127.0.0.1:{}/v1beta",
        listener.local_addr().unwrap().port()
    );
    let seen = Arc::new(Mutex::new(Vec::new()));
    let requests = seen.clone();
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let service = hyper::service::service_fn(
            move |request: hyper::Request<hyper::body::Incoming>| {
                let requests = requests.clone();
                async move {
                    assert_eq!(request.headers()["x-goog-api-key"], "fixture-upstream");
                    assert!(!request.headers().contains_key("authorization"));
                    let path = request.uri().to_string();
                    requests.lock().unwrap().push(path.clone());
                    let body = if path.contains("pageToken=") {
                        json!({"models":[{"name":"models/gemini-fixture-b"}]})
                    } else {
                        json!({"models":[{"name":"models/gemini-fixture-a"}],"nextPageToken":"next/fixture+="})
                    };
                    Ok::<_, std::convert::Infallible>(
                        hyper::Response::builder()
                            .header("content-type", "application/json")
                            .body(replay::full(body.to_string()))
                            .unwrap(),
                    )
                }
            },
        );
        let _ = hyper::server::conn::http1::Builder::new()
            .serve_connection(hyper_util::rt::TokioIo::new(socket), service)
            .await;
    });
    save(&agy, &home, &base);
    let models = agy
        .list_models(&agy.view().providers[0].id, true)
        .await
        .unwrap();
    assert_eq!(models.error, None);
    assert_eq!(models.models, ["gemini-fixture-a", "gemini-fixture-b"]);
    assert_eq!(
        *seen.lock().unwrap(),
        [
            "/v1beta/models",
            "/v1beta/models?pageToken=next%2Ffixture%2B%3D"
        ]
    );
    server.abort();
}
#[test]
fn native_nonstream_and_blocked_responses_settle_once() {
    let mut p = protocol::Protocol::new(false);
    p.response(200, false, "");
    p.feed(br#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":"fixture","args":{"path":"fixture"}},"thoughtSignature":"opaque"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":12,"candidatesTokenCount":4}}"#);
    p.finish(Some(200), code::OK);
    p.finish(Some(200), code::OK);
    assert_eq!(p.observation.meter.tokens.output, Some(4));
    assert_eq!(p.terminal(), Some(protocol::Terminal::Success));
    let mut blocked = protocol::Protocol::new(false);
    blocked.value(&json!({"promptFeedback":{"blockReason":"SAFETY"}}));
    blocked.finish(Some(200), code::OK);
    assert_eq!(blocked.terminal(), Some(protocol::Terminal::Rejected));
}
