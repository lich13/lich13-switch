// End-to-end transport and configuration tests live here; all credentials are fixtures.
use super::*;
#[test]
fn codex_capacity_messages_are_detected_without_matching_normal_errors() {
    assert!(forward::capacity_message(
        hyper::StatusCode::TOO_MANY_REQUESTS,
        b"any body"
    ));
    assert!(forward::capacity_message(
        hyper::StatusCode::SERVICE_UNAVAILABLE,
        b"Selected model is at capacity. Please try a different model."
    ));
    assert!(forward::capacity_message(
        hyper::StatusCode::BAD_GATEWAY,
        "模型当前限流".as_bytes()
    ));
    assert!(!forward::capacity_message(
        hyper::StatusCode::BAD_REQUEST,
        b"model is at capacity"
    ));
}

#[test]
fn provider_secrets_never_appear_in_view() {
    let temp = tempfile::tempdir().unwrap();
    let g = Gateway::new(temp.path().to_path_buf()).unwrap();
    let update = |edit| g.edit(edit, &g.view().revision, temp.path()).unwrap();
    update(Edit::SaveProvider {
        id: None,
        base_url: "https://example.test/custom/v1".into(),
        token: "private-fixture-token".into(),
        name: None,
    });
    let json = serde_json::to_string(&g.view()).unwrap();
    assert!(!json.contains("private-fixture"));
    assert!(!json.contains("localToken"));
}
#[test]
fn url_suffix_and_hop_headers() {
    assert_eq!(
        forward::target(
            "https://example.test/api/v1/",
            &"/v1/responses/compact?x=1&x=2".parse().unwrap()
        )
        .unwrap()
        .to_string(),
        "https://example.test/api/v1/responses/compact?x=1&x=2"
    );
    let mut h = hyper::HeaderMap::new();
    h.insert("connection", "x-private, keep-alive".parse().unwrap());
    h.insert("x-private", "secret".parse().unwrap());
    h.append("x-repeat", "one".parse().unwrap());
    h.append("x-repeat", "two".parse().unwrap());
    forward::clean_headers(&mut h, false);
    assert!(!h.contains_key("x-private"));
    assert_eq!(h.get_all("x-repeat").iter().count(), 2);
}

use super::replay::{full, WireBody};
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use http_body_util::{BodyExt, StreamBody};
use hyper::{
    body::{Frame, Incoming},
    Request, Response,
};
use hyper_util::rt::TokioIo;
use std::{
    convert::Infallible,
    future::Future,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
async fn server<F, Fut>(handler: F) -> u16
where
    F: Fn(Request<Incoming>) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Response<WireBody>> + Send + 'static,
{
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let handler = Arc::new(handler);
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let handler = handler.clone();
            tokio::spawn(async move {
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(
                        TokioIo::new(stream),
                        hyper::service::service_fn(move |req| {
                            let handler = handler.clone();
                            async move { Ok::<_, Infallible>(handler(req).await) }
                        }),
                    )
                    .with_upgrades()
                    .await;
            });
        }
    });
    port
}
async fn fixture(urls: Vec<String>) -> (tempfile::TempDir, Gateway) {
    let t = tempfile::tempdir().unwrap();
    let first = urls
        .first()
        .cloned()
        .unwrap_or_else(|| "https://example.test/v1".into());
    let config=format!("# fixture\nmodel_provider='custom'\n[model_providers.custom]\nbase_url = {}\nexperimental_bearer_token = \"upstream-fixture-token\"\nwire_api='responses'\nsupports_websockets=false\n",serde_json::to_string(&first).unwrap());
    std::fs::write(t.path().join("config.toml"), config).unwrap();
    let g = Gateway::new(t.path().to_path_buf()).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    update(
        &g,
        &t,
        Edit::Settings {
            settings: Settings {
                port,
                ..Default::default()
            },
        },
    );
    for url in urls {
        update(
            &g,
            &t,
            Edit::SaveProvider {
                id: None,
                base_url: url,
                token: "upstream-fixture-token".into(),
                name: None,
            },
        );
    }
    std::fs::write(t.path().join("auth.json"), "unchanged-auth").unwrap();
    (t, g)
}

#[tokio::test]
async fn stopping_releases_listener_before_restart_returns_to_caller() {
    let (t, g) = fixture(vec!["https://example.test/v1".into()]).await;
    let config = std::fs::read(t.path().join("config.toml")).ok();
    let auth = std::fs::read(t.path().join("auth.json")).ok();
    for _ in 0..4 {
        start(&g, &t).await;
        g.stop().await.unwrap();
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", g.view().settings.port))
            .await
            .expect("stop must release the listening port before returning");
        drop(listener);
        assert_eq!(std::fs::read(t.path().join("config.toml")).ok(), config);
        assert_eq!(std::fs::read(t.path().join("auth.json")).ok(), auth);
    }
}

#[tokio::test]
async fn codex_capacity_429_fails_over_to_next_provider_before_returning() {
    let first_hits = Arc::new(AtomicUsize::new(0));
    let first_counter = first_hits.clone();
    let first = server(move |_| {
        first_counter.fetch_add(1, Ordering::SeqCst);
        async {
            Response::builder()
                .status(429)
                .body(full("Selected model is at capacity"))
                .unwrap()
        }
    })
    .await;
    let second = server(|_| async { Response::new(full("backup-ok")) }).await;
    let (t, g) = fixture(vec![
        format!("http://127.0.0.1:{first}/v1"),
        format!("http://127.0.0.1:{second}/v1"),
    ])
    .await;
    update(
        &g,
        &t,
        Edit::Mode {
            mode: "auto".into(),
        },
    );
    start(&g, &t).await;
    let body = br#"{"model":"gpt-test","input":"preserve"}"#.to_vec();
    let response = request(
        &g,
        "/v1/responses",
        body,
        vec![("content-type", "application/json")],
    )
    .await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.into_body().collect().await.unwrap().to_bytes(),
        "backup-ok"
    );
    assert_eq!(first_hits.load(Ordering::SeqCst), 1);
    assert_eq!(
        g.view().providers[0].health.cooldown_reason.as_deref(),
        Some("capacity_retry")
    );
    g.stop().await.unwrap();
}

#[tokio::test]
async fn codex_single_provider_capacity_waits_and_never_opens() {
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = hits.clone();
    let upstream = server(move |_| {
        let n = counter.fetch_add(1, Ordering::SeqCst);
        async move {
            if n == 0 {
                Response::builder()
                    .status(429)
                    .body(full("Selected model is at capacity"))
                    .unwrap()
            } else {
                Response::new(full("recovered"))
            }
        }
    })
    .await;
    let (t, g) = fixture(vec![format!("http://127.0.0.1:{upstream}/v1")]).await;
    let mut settings = g.view().settings;
    settings.capacity_retry_seconds = 1;
    settings.max_retries = 1;
    update(&g, &t, Edit::Settings { settings });
    update(
        &g,
        &t,
        Edit::Mode {
            mode: "auto".into(),
        },
    );
    start(&g, &t).await;
    let response = request(
        &g,
        "/v1/responses",
        br#"{"model":"gpt-test"}"#.to_vec(),
        vec![("content-type", "application/json")],
    )
    .await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.into_body().collect().await.unwrap().to_bytes(),
        "recovered"
    );
    let provider = &g.view().providers[0];
    assert_eq!(provider.health.state, super::circuit::CircuitState::Closed);
    assert!(!provider.health.protected_single_provider);
    assert_eq!(hits.load(Ordering::SeqCst), 2);
    g.stop().await.unwrap();
}

fn update(g: &Gateway, t: &tempfile::TempDir, edit: Edit) {
    g.edit(edit, &g.view().revision, t.path()).unwrap();
}
async fn start(g: &Gateway, t: &tempfile::TempDir) {
    g.start(&g.view().revision, t.path()).await.unwrap();
}
async fn request(
    g: &Gateway,
    path: &str,
    body: Vec<u8>,
    headers: Vec<(&str, &str)>,
) -> Response<Incoming> {
    let client: HttpClient =
        Client::builder(TokioExecutor::new()).build(Connector::new(Duration::from_secs(2), 0));
    let (token, port) = {
        let s = g.0.inner.lock().unwrap();
        (s.store.local_token.clone(), s.store.settings.port)
    };
    let mut req = Request::builder()
        .method("POST")
        .uri(format!("http://127.0.0.1:{port}{path}"))
        .header("authorization", format!("Bearer {token}"));
    for (k, v) in headers {
        req = req.header(k, v);
    }
    client.request(req.body(full(body)).unwrap()).await.unwrap()
}
#[tokio::test]
async fn unknown_paths_query_duplicate_headers_and_original_payloads() {
    let seen = Arc::new(Mutex::new(vec![]));
    let copy = seen.clone();
    let port = server(move |req| {
        let copy = copy.clone();
        async move {
            let (parts, body) = req.into_parts();
            let bytes = body.collect().await.unwrap().to_bytes();
            assert_eq!(
                parts.headers["authorization"],
                "Bearer upstream-fixture-token"
            );
            assert!(!parts.headers.contains_key("proxy-authorization"));
            copy.lock().unwrap().push((
                parts.uri.to_string(),
                parts.headers.get_all("x-repeat").iter().count(),
                bytes.clone(),
            ));
            Response::builder()
                .header("x-repeat", "one")
                .header("x-repeat", "two")
                .body(full(bytes))
                .unwrap()
        }
    })
    .await;
    let (t, g) = fixture(vec![format!("http://127.0.0.1:{port}/sub/v1")]).await;
    start(&g, &t).await;
    use std::io::Write;
    let mut gzip = flate2::write::GzEncoder::new(vec![], flate2::Compression::default());
    gzip.write_all(b"{\"unknown\":true,\"model\":\"untouched\"}")
        .unwrap();
    let cases=vec![(vec![0,255,0,128,10],"application/octet-stream","identity"),
        (b"--boundary\r\nContent-Disposition: form-data; name=\"file\"\r\n\r\nraw\0\xff\r\n--boundary--\r\n".to_vec(),"multipart/form-data; boundary=boundary","identity"),
        (gzip.finish().unwrap(),"application/json","gzip"),
        (vec![7u8;3*1024*1024],"application/octet-stream","identity")];
    for (data, content, encoding) in cases {
        let response = request(
            &g,
            "/v1/future/path?x=1&x=2&raw=%2F",
            data.clone(),
            vec![
                ("content-type", content),
                ("content-encoding", encoding),
                ("x-repeat", "a"),
                ("x-repeat", "b"),
            ],
        )
        .await;
        assert_eq!(response.headers().get_all("x-repeat").iter().count(), 2);
        assert_eq!(
            response.into_body().collect().await.unwrap().to_bytes(),
            data
        );
    }
    assert!(seen
        .lock()
        .unwrap()
        .iter()
        .all(|(path, headers, _)| path == "/sub/v1/future/path?x=1&x=2&raw=%2F" && *headers == 2));
    assert_eq!(std::fs::read_dir(g.0.spool.path()).unwrap().count(), 0);
    g.stop().await.unwrap();
    assert_eq!(
        std::fs::read_to_string(t.path().join("auth.json")).unwrap(),
        "unchanged-auth"
    );
}
#[tokio::test]
async fn queue_four_attempt_limit_terminal_errors_and_original_last_error() {
    let hits = Arc::new(Mutex::new(vec![]));
    let mut urls = vec![];
    for i in 0..5 {
        let hits = hits.clone();
        let port = server(move |req| {
            let hits = hits.clone();
            async move {
                let path = req.uri().path().to_owned();
                let _ = req.into_body().collect().await;
                hits.lock().unwrap().push(i);
                Response::builder()
                    .status(if path.ends_with("terminal") { 422 } else { 503 })
                    .header("x-source", i.to_string())
                    .body(full(format!("upstream-{i}")))
                    .unwrap()
            }
        })
        .await;
        urls.push(format!("http://127.0.0.1:{port}"));
    }
    let (t, g) = fixture(urls).await;
    update(
        &g,
        &t,
        Edit::Mode {
            mode: "auto".into(),
        },
    );
    start(&g, &t).await;
    let response = request(&g, "/v1/unknown", vec![], vec![]).await;
    assert_eq!(response.status(), 503);
    assert_eq!(
        response.into_body().collect().await.unwrap().to_bytes(),
        "upstream-3"
    );
    assert_eq!(*hits.lock().unwrap(), vec![0, 1, 2, 3]);
    hits.lock().unwrap().clear();
    let response = request(&g, "/v1/terminal", vec![], vec![]).await;
    assert_eq!(response.status(), 422);
    response.into_body().collect().await.unwrap();
    assert_eq!(*hits.lock().unwrap(), vec![0]);
    assert_eq!(g.view().providers[0].health.failures, 1);
    g.stop().await.unwrap();
}
#[tokio::test]
async fn streaming_first_byte_retry_no_splicing_and_cancel_releases_activity() {
    let hits = Arc::new(AtomicUsize::new(0));
    let hits2 = hits.clone();
    let slow=server(|_|async{Response::builder().header("content-type","text/event-stream").body(StreamBody::new(async_stream::try_stream!{
        tokio::time::sleep(Duration::from_secs(2)).await;yield Frame::data(Bytes::from_static(b"data: late\n\n"));
    }).boxed_unsync()).unwrap()}).await;
    let fast=server(move|_|{hits2.fetch_add(1,Ordering::Relaxed);async{Response::builder().header("content-type","text/event-stream").body(StreamBody::new(async_stream::try_stream!{
        yield Frame::data(Bytes::from_static(b"data: {\"response\":{\"id\":\"resp_test\"}}\n\n"));
        tokio::time::sleep(Duration::from_millis(20)).await;yield Frame::data(Bytes::from_static(b"data: tail\n\n"));
    }).boxed_unsync()).unwrap()}}).await;
    let (t, g) = fixture(vec![
        format!("http://127.0.0.1:{slow}"),
        format!("http://127.0.0.1:{fast}"),
    ])
    .await;
    update(
        &g,
        &t,
        Edit::Settings {
            settings: Settings {
                first_byte_seconds: 1,
                ..g.view().settings
            },
        },
    );
    update(
        &g,
        &t,
        Edit::Mode {
            mode: "auto".into(),
        },
    );
    start(&g, &t).await;
    let response = request(&g, "/v1/responses", vec![], vec![]).await;
    assert_eq!(
        response.into_body().collect().await.unwrap().to_bytes(),
        "data: {\"response\":{\"id\":\"resp_test\"}}\n\ndata: tail\n\n"
    );
    let before = g.view().providers[0].health.requests;
    let response = request(
        &g,
        "/v1/responses",
        b"{\"previous_response_id\":\"resp_test\"}".to_vec(),
        vec![("content-type", "application/json")],
    )
    .await;
    response.into_body().collect().await.unwrap();
    assert_eq!(g.view().providers[0].health.requests, before);
    assert_eq!(hits.load(Ordering::Relaxed), 2);
    let never=server(|_|async{Response::builder().header("content-type","text/event-stream").body(StreamBody::new(async_stream::try_stream!{
        yield Frame::data(Bytes::from_static(b"data: first\n\n"));tokio::time::sleep(Duration::from_secs(20)).await;yield Frame::data(Bytes::new());
    }).boxed_unsync()).unwrap()}).await;
    let first = g.view().providers[0].id.clone();
    update(
        &g,
        &t,
        Edit::SaveProvider {
            id: Some(first),
            base_url: format!("http://127.0.0.1:{never}"),
            token: String::new(),
            name: None,
        },
    );
    let mut response = request(&g, "/v1/responses", vec![], vec![]).await;
    response.body_mut().frame().await.unwrap().unwrap();
    drop(response);
    for _ in 0..20 {
        if g.view().active_connections == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(g.view().active_connections, 0);
    assert_eq!(hits.load(Ordering::Relaxed), 2);
    g.stop().await.unwrap();
}
#[tokio::test]
async fn port_conflict_startup_recovery_and_single_candidate_breaker() {
    let port = server(|_| async {
        Response::builder()
            .status(502)
            .header("retry-after", "120")
            .body(full("upstream-502"))
            .unwrap()
    })
    .await;
    let (t, g) = fixture(vec![format!("http://127.0.0.1:{port}")]).await;
    let mut settings = g.view().settings;
    settings.failure_threshold = 1;
    settings.transient_failure_threshold = 1;
    settings.max_retries = 0;
    settings.queue_seconds = 1;
    update(&g, &t, Edit::Settings { settings });
    let occupied = tokio::net::TcpListener::bind(("127.0.0.1", g.view().settings.port))
        .await
        .unwrap();
    assert!(g.start(&g.view().revision, t.path()).await.is_err());
    assert!(takeover::read(t.path()).is_ok());
    drop(occupied);
    update(
        &g,
        &t,
        Edit::Mode {
            mode: "auto".into(),
        },
    );
    start(&g, &t).await;
    let first_status =
        tokio::time::timeout(Duration::from_secs(5), request(&g, "/v1/a", vec![], vec![]))
            .await
            .expect("upstream failure response timed out")
            .status();
    assert_eq!(first_status, 502);
    let cooldown_status =
        tokio::time::timeout(Duration::from_secs(5), request(&g, "/v1/a", vec![], vec![]))
            .await
            .expect("cooldown response timed out")
            .status();
    assert_eq!(cooldown_status, 503);
    let view = g.view();
    let health = &view.providers[0].health;
    assert!(health.retry_in >= 118);
    assert_eq!(health.failures, 1);
    assert_eq!(health.state, super::circuit::CircuitState::Closed);
    assert!(health.protected_single_provider);
    g.stop().await.unwrap();
    let (_, pair) = takeover::read(t.path()).unwrap();
    takeover::attach(
        t.path(),
        t.path(),
        g.view().settings.port,
        "fixture-crash",
        pair,
        &takeover::read(t.path()).unwrap().0,
    )
    .unwrap();
    let recovered = Gateway::new(t.path().to_path_buf()).unwrap();
    assert!(!recovered.guarded_home());
    assert!(takeover::read(t.path()).is_ok());
}

#[tokio::test]
async fn priority_recovers_on_next_request_and_unknown_cursor_never_retries() {
    let hits = Arc::new(AtomicUsize::new(0));
    let x = hits.clone();
    let first = server(move |_| {
        let n = x.fetch_add(1, Ordering::Relaxed);
        async move {
            Response::builder()
                .status(if n == 0 { 503 } else { 200 })
                .body(full("first"))
                .unwrap()
        }
    })
    .await;
    let second = server(|_| async { Response::new(full("second")) }).await;
    let (t, g) = fixture(vec![
        format!("http://127.0.0.1:{first}"),
        format!("http://127.0.0.1:{second}"),
    ])
    .await;
    update(
        &g,
        &t,
        Edit::Mode {
            mode: "auto".into(),
        },
    );
    start(&g, &t).await;
    assert_eq!(
        request(&g, "/v1/new", vec![], vec![])
            .await
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes(),
        "second"
    );
    assert_eq!(
        request(&g, "/v1/new", vec![], vec![])
            .await
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes(),
        "first"
    );
    hits.store(0, Ordering::Relaxed);
    let r = request(
        &g,
        "/v1/responses",
        b"{\"previous_response_id\":\"unknown\"}".to_vec(),
        vec![("content-type", "application/json")],
    )
    .await;
    assert_eq!(r.status(), 503);
    assert_eq!(hits.load(Ordering::Relaxed), 1);
    g.stop().await.unwrap();
}
#[tokio::test]
async fn idle_timeout_after_commit_never_splices_backup_and_slow_consumer_is_bounded() {
    let first=server(|_|async{Response::builder().header("content-type","text/event-stream").body(StreamBody::new(async_stream::try_stream!{
        yield Frame::data(Bytes::from_static(b"data: original\n\n"));tokio::time::sleep(Duration::from_secs(3)).await;yield Frame::data(Bytes::from_static(b"data: late\n\n"));
    }).boxed_unsync()).unwrap()}).await;
    let hits = Arc::new(AtomicUsize::new(0));
    let copy = hits.clone();
    let second = server(move |_| {
        copy.fetch_add(1, Ordering::Relaxed);
        async { Response::new(full("backup")) }
    })
    .await;
    let (t, g) = fixture(vec![
        format!("http://127.0.0.1:{first}"),
        format!("http://127.0.0.1:{second}"),
    ])
    .await;
    update(
        &g,
        &t,
        Edit::Settings {
            settings: Settings {
                idle_seconds: 1,
                ..g.view().settings
            },
        },
    );
    update(
        &g,
        &t,
        Edit::Mode {
            mode: "auto".into(),
        },
    );
    start(&g, &t).await;
    let mut r = request(&g, "/v1/stream", vec![], vec![]).await;
    assert_eq!(
        r.body_mut()
            .frame()
            .await
            .unwrap()
            .unwrap()
            .into_data()
            .unwrap(),
        "data: original\n\n"
    );
    assert!(r.body_mut().frame().await.unwrap().is_err());
    assert_eq!(hits.load(Ordering::Relaxed), 0);
    g.stop().await.unwrap();

    let produced = Arc::new(AtomicUsize::new(0));
    let count = produced.clone();
    let port=server(move|_|{let count=count.clone();async move{Response::builder().header("content-type","application/octet-stream").body(StreamBody::new(async_stream::try_stream!{
        for _ in 0..1024{count.fetch_add(1,Ordering::Relaxed);yield Frame::data(Bytes::from(vec![1u8;64*1024]));}
    }).boxed_unsync()).unwrap()}}).await;
    let (t, g) = fixture(vec![format!("http://127.0.0.1:{port}")]).await;
    start(&g, &t).await;
    let r = request(&g, "/v1/binary", vec![], vec![]).await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(
        produced.load(Ordering::Relaxed) < 1024,
        "downstream backpressure must bound upstream reads"
    );
    drop(r);
    g.stop().await.unwrap();
}
#[tokio::test]
#[ignore = "Uses the user's installed Codex CLI with an isolated home and an explicitly supplied private fixture"]
async fn real_codex_cli_with_isolated_home() {
    let path = std::env::var_os("GPT_SWITCH_REAL_FIXTURE").expect("private fixture required");
    let private: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    let cli = std::env::var_os("GPT_SWITCH_CODEX_CLI").expect("official CLI path required");
    let (t, g) = fixture(vec![]).await;
    update(
        &g,
        &t,
        Edit::SaveProvider {
            id: None,
            base_url: private["baseUrl"].as_str().unwrap().into(),
            token: private["token"].as_str().unwrap().into(),
            name: None,
        },
    );
    // A valid disposable auth file proves the CLI used the managed provider, never the real account.
    let auth = b"{\"auth_mode\":\"apikey\",\"OPENAI_API_KEY\":\"fixture-only\"}";
    storage::atomic_write(&t.path().join("auth.json"), auth, None).unwrap();
    let config=format!("model = {}\nmodel_provider = 'custom'\n[model_providers.custom]\nname = 'Original'\nwire_api = 'responses'\nsupports_websockets = false\nbase_url = {}\nexperimental_bearer_token = {}\n",serde_json::to_string(private["model"].as_str().unwrap()).unwrap(),serde_json::to_string(private["baseUrl"].as_str().unwrap()).unwrap(),serde_json::to_string(private["token"].as_str().unwrap()).unwrap());
    storage::atomic_write(&t.path().join("config.toml"), config.as_bytes(), None).unwrap();
    start(&g, &t).await;
    let result = tokio::time::timeout(
        Duration::from_secs(120),
        tokio::process::Command::new(cli)
            .args([
                "exec",
                "--ephemeral",
                "--skip-git-repo-check",
                "--json",
                "Reply only: gateway-cli-ok. Do not use tools.",
            ])
            .env("CODEX_HOME", t.path())
            .env_remove("OPENAI_API_KEY")
            .current_dir(t.path())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    // Never print CLI output: it can contain private upstream data.
    assert!(
        result.status.success(),
        "isolated official CLI failed; output intentionally withheld"
    );
    assert!(
        String::from_utf8_lossy(&result.stdout).contains("gateway-cli-ok"),
        "CLI completion missing"
    );
    let view = g.view();
    assert!(view.providers[0].health.requests > 0);
    assert_eq!(
        view.last_successful.as_deref(),
        Some(view.providers[0].id.as_str())
    );
    g.stop().await.unwrap();
    assert_eq!(
        storage::read_optional(&t.path().join("auth.json"))
            .unwrap()
            .unwrap(),
        auth
    );
    assert_eq!(
        storage::read_optional(&t.path().join("config.toml"))
            .unwrap()
            .unwrap(),
        config.as_bytes()
    );
    println!("Official Codex CLI completed with isolated home; auth/config restored");
}

#[tokio::test]
async fn compressed_and_spooled_json_replay_exactly_across_failover() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut urls = Vec::new();
    for status in [503, 200] {
        let seen = seen.clone();
        let port = server(move |r| {
            let seen = seen.clone();
            async move {
                let encoding = r.headers().get("content-encoding").cloned();
                let bytes = r.into_body().collect().await.unwrap().to_bytes();
                seen.lock().unwrap().push(storage::digest(&bytes));
                let mut response = Response::builder().status(status);
                if let Some(encoding) = encoding {
                    response = response.header("content-encoding", encoding);
                }
                response.body(full(bytes)).unwrap()
            }
        })
        .await;
        urls.push(format!("http://127.0.0.1:{port}/v1"));
    }
    let (t, g) = fixture(urls).await;
    update(
        &g,
        &t,
        Edit::Mode {
            mode: "auto".into(),
        },
    );
    start(&g, &t).await;
    let json = serde_json::to_vec(
        &serde_json::json!({"input":"x".repeat(3*1024*1024),"unknown":{"keep":true}}),
    )
    .unwrap();
    let zstd = zstd::stream::encode_all(json.as_slice(), 1).unwrap();
    for (body, encoding) in [(json, "identity"), (zstd, "zstd")] {
        let digest = storage::digest(&body);
        let response = request(
            &g,
            "/v1/future",
            body.clone(),
            vec![
                ("content-type", "application/json"),
                ("content-encoding", encoding),
            ],
        )
        .await;
        assert_eq!(response.status(), 200);
        assert_eq!(response.headers()["content-encoding"], encoding);
        assert_eq!(
            response.into_body().collect().await.unwrap().to_bytes(),
            body
        );
        assert_eq!(
            seen.lock().unwrap().drain(..).collect::<Vec<_>>(),
            vec![digest.clone(), digest]
        );
    }
    assert_eq!(std::fs::read_dir(g.0.spool.path()).unwrap().count(), 0);
    g.stop().await.unwrap();
}

#[tokio::test]
async fn custom_only_switch_while_stopped_running_and_auto_stop() {
    let ok = server(|_| async { Response::new(full("ok")) }).await;
    let (t, g) = fixture(vec![
        "https://a.test/v1".into(),
        format!("http://127.0.0.1:{ok}"),
    ])
    .await;
    let b = g.view().providers[1].id.clone();
    let path = t.path().join("config.toml");
    let initial = std::fs::read_to_string(&path).unwrap();
    let old_rev = takeover::read(t.path()).unwrap().0;
    assert!(g
        .edit_checked(
            Edit::Select { id: b.clone() },
            &g.view().revision,
            t.path(),
            Some("stale")
        )
        .is_err());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), initial);
    g.edit_checked(
        Edit::Select { id: b.clone() },
        &g.view().revision,
        t.path(),
        Some(&old_rev),
    )
    .unwrap();
    assert_eq!(g.view().config_provider.as_deref(), Some(b.as_str()));
    let direct = std::fs::read_to_string(&path).unwrap();
    assert_eq!(
        direct,
        initial.replace("https://a.test/v1", &format!("http://127.0.0.1:{ok}"))
    );
    start(&g, &t).await;
    let live = std::fs::read(&path).unwrap();
    let a = g.view().providers[0].id.clone();
    update(&g, &t, Edit::Select { id: a });
    assert_eq!(std::fs::read(&path).unwrap(), live);
    update(
        &g,
        &t,
        Edit::Mode {
            mode: "auto".into(),
        },
    );
    update(
        &g,
        &t,
        Edit::QueueProvider {
            id: g.view().providers[0].id.clone(),
            queued: false,
        },
    );
    let response = request(&g, "/v1/new", vec![], vec![]).await;
    assert_eq!(response.status(), 200);
    response.into_body().collect().await.unwrap();
    assert_eq!(g.view().last_successful.as_deref(), Some(b.as_str()));
    assert_eq!(std::fs::read(&path).unwrap(), live);
    g.stop().await.unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), direct);
    assert_eq!(
        std::fs::read_to_string(t.path().join("auth.json")).unwrap(),
        "unchanged-auth"
    );
}
#[tokio::test]
async fn running_cannot_delete_final_exit_provider_and_crash_uses_updated_target() {
    let (t, g) = fixture(vec!["https://a.test/v1".into(), "https://b.test/v1".into()]).await;
    start(&g, &t).await;
    let a = g.view().providers[0].id.clone();
    let b = g.view().providers[1].id.clone();
    update(&g, &t, Edit::Select { id: b.clone() });
    update(&g, &t, Edit::DeleteProvider { id: a });
    assert!(g
        .edit(Edit::DeleteProvider { id: b }, &g.view().revision, t.path())
        .is_err());
    takeover::recover(t.path()).unwrap();
    assert_eq!(
        takeover::read(t.path()).unwrap().1.base_url.as_deref(),
        Some("https://b.test/v1")
    );
    g.stop().await.unwrap();
}
#[tokio::test]
async fn quota_sub2_priority_singleflight_cache_and_health_isolation() {
    let hits = Arc::new(AtomicUsize::new(0));
    let counted = hits.clone();
    let port = server(move |req| {
        let hits = counted.clone();
        async move {
            hits.fetch_add(1, Ordering::Relaxed);
            assert_eq!(req.uri().path(), "/prefix/v1/usage");
            assert_eq!(
                req.headers()["authorization"],
                "Bearer upstream-fixture-token"
            );
            tokio::time::sleep(Duration::from_millis(30)).await;
            Response::new(full(
                r#"{"isValid":true,"quota":{"limit":10,"used":2,"remaining":8}}"#,
            ))
        }
    })
    .await;
    let (t, g) = fixture(vec![format!("http://127.0.0.1:{port}/prefix/v1")]).await;
    let p = g.view().providers[0].id.clone();
    let config = std::fs::read(t.path().join("config.toml")).unwrap();
    let (a, b) = tokio::join!(g.query_quota(&p, true), g.query_quota(&p, true));
    assert_eq!(a.unwrap().plans[0].remaining, Some(8.));
    assert_eq!(b.unwrap().source.as_deref(), Some("sub2api"));
    assert_eq!(hits.load(Ordering::Relaxed), 1);
    g.query_quota(&p, false).await.unwrap();
    assert_eq!(hits.load(Ordering::Relaxed), 1);
    assert_eq!(g.view().providers[0].health.failures, 0);
    assert_eq!(g.view().providers[0].active_requests, 0);
    assert_eq!(g.view().last_successful, None);
    assert_eq!(g.view().providers[0].health.requests, 0);
    assert_eq!(std::fs::read(t.path().join("config.toml")).unwrap(), config);
}
#[tokio::test]
async fn quota_newapi_fallback_and_public_unit_request_has_no_key() {
    let paths = Arc::new(Mutex::new(vec![]));
    let seen = paths.clone();
    let port=server(move|r|{let seen=seen.clone();async move{
        let path=r.uri().path().to_owned();seen.lock().unwrap().push(path.clone());
        match path.as_str(){
            "/site/v1/usage"=>Response::builder().status(404).body(full("missing")).unwrap(),
            "/site/api/usage/token/"=>{assert!(r.headers().contains_key("authorization"));Response::new(full(r#"{"code":true,"data":{"object":"token_usage","total_available":400,"total_granted":1000,"total_used":600,"unlimited_quota":false,"expires_at":0}}"#))},
            "/site/api/status"=>{assert!(!r.headers().contains_key("authorization"));Response::new(full(r#"{"success":true,"data":{"quota_per_unit":100,"quota_display_type":"CNY","usd_exchange_rate":7}}"#))},
            _=>panic!("unexpected quota endpoint"),
        }
    }}).await;
    let (_t, g) = fixture(vec![format!("http://127.0.0.1:{port}/site/v1")]).await;
    let result = g
        .query_quota(&g.view().providers[0].id, true)
        .await
        .unwrap();
    assert_eq!(result.source.as_deref(), Some("newapi"));
    assert_eq!(result.plans[0].remaining, Some(28.));
    assert_eq!(result.plans[0].unit, "CNY");
    assert_eq!(paths.lock().unwrap().len(), 3);
}
#[tokio::test]
async fn quota_retry_after_keeps_last_success_and_does_not_fallback() {
    let hits = Arc::new(AtomicUsize::new(0));
    let seen = hits.clone();
    let port = server(move |_| {
        let seen = seen.clone();
        async move {
            if seen.fetch_add(1, Ordering::Relaxed) == 0 {
                Response::new(full(r#"{"balance":4.25}"#))
            } else {
                Response::builder()
                    .status(429)
                    .header("retry-after", "120")
                    .body(full("limited"))
                    .unwrap()
            }
        }
    })
    .await;
    let (_t, g) = fixture(vec![format!("http://127.0.0.1:{port}")]).await;
    let id = g.view().providers[0].id.clone();
    g.query_quota(&id, true).await.unwrap();
    let result = g.query_quota(&id, true).await.unwrap();
    assert!(result.stale);
    assert_eq!(result.state, "error");
    assert_eq!(result.plans[0].remaining, Some(4.25));
    assert!(result.retry_at.unwrap() >= quota::now() + 119);
    g.query_quota(&id, true).await.unwrap();
    assert_eq!(hits.load(Ordering::Relaxed), 2);
}
#[tokio::test]
async fn quota_redirect_oversize_auth_and_unsupported_are_distinct() {
    for (status, body, expected) in [
        (302, "redirect".to_owned(), "error"),
        (200, "x".repeat(2_000_001), "error"),
        (401, "invalid key".to_owned(), "error"),
        (200, "<html>login</html>".to_owned(), "unsupported"),
    ] {
        let hits = Arc::new(AtomicUsize::new(0));
        let seen = hits.clone();
        let port = server(move |_| {
            let seen = seen.clone();
            let body = body.clone();
            async move {
                seen.fetch_add(1, Ordering::Relaxed);
                Response::builder()
                    .status(status)
                    .header("location", "https://elsewhere.invalid/steal")
                    .body(full(body))
                    .unwrap()
            }
        })
        .await;
        let (_t, g) = fixture(vec![format!("http://127.0.0.1:{port}")]).await;
        let result = g
            .query_quota(&g.view().providers[0].id, true)
            .await
            .unwrap();
        assert_eq!(result.state, expected);
        assert!(result.plans.is_empty());
        assert_eq!(
            hits.load(Ordering::Relaxed),
            if status == 302 || result.error.as_ref().is_some_and(|e| e.contains("2 MB")) {
                1
            } else {
                2
            }
        );
    }
}

#[tokio::test]
async fn quota_late_response_is_discarded_after_provider_edit() {
    let entered = Arc::new(tokio::sync::Notify::new());
    let gate = Arc::new(tokio::sync::Notify::new());
    let e = entered.clone();
    let gate2 = gate.clone();
    let port = server(move |_| {
        let e = e.clone();
        let gate = gate2.clone();
        async move {
            e.notify_one();
            gate.notified().await;
            Response::new(full(r#"{"balance":123}"#))
        }
    })
    .await;
    let (t, g) = fixture(vec![format!("http://127.0.0.1:{port}")]).await;
    let id = g.view().providers[0].id.clone();
    let g2 = g.clone();
    let id2 = id.clone();
    let task = tokio::spawn(async move { g2.query_quota(&id2, true).await });
    entered.notified().await;
    update(
        &g,
        &t,
        Edit::SaveProvider {
            id: Some(id),
            base_url: format!("http://127.0.0.1:{port}"),
            token: "changed-key".into(),
            name: None,
        },
    );
    gate.notify_one();
    assert_eq!(
        task.await
            .unwrap()
            .err()
            .expect("stale result must be rejected")
            .code,
        "STALE"
    );
    assert!(g.view().providers[0].quota.is_none());
}

#[tokio::test]
async fn quota_concurrency_limit_and_echoed_secrets_are_filtered() {
    let active = Arc::new(AtomicUsize::new(0));
    let max = Arc::new(AtomicUsize::new(0));
    let a = active.clone();
    let m = max.clone();
    let port = server(move |_| {
        let a = a.clone();
        let m = m.clone();
        async move {
            let current = a.fetch_add(1, Ordering::SeqCst) + 1;
            m.fetch_max(current, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(80)).await;
            a.fetch_sub(1, Ordering::SeqCst);
            Response::new(full(r#"{"balance":5,"unit":"upstream-fixture-token"}"#))
        }
    })
    .await;
    let (_t, g) = fixture(
        (0..6)
            .map(|i| format!("http://127.0.0.1:{port}/site{i}"))
            .collect(),
    )
    .await;
    let ids: Vec<_> = g.view().providers.iter().map(|p| p.id.clone()).collect();
    let results =
        futures_util::future::join_all(ids.iter().map(|id| g.query_quota(id, true))).await;
    assert_eq!(max.load(Ordering::SeqCst), 3);
    for result in results {
        let view = result.unwrap();
        assert_eq!(view.state, "ok");
        assert!(!serde_json::to_string(&view)
            .unwrap()
            .contains("upstream-fixture-token"));
    }
}

#[tokio::test]
async fn quota_timeout_does_not_probe_another_protocol() {
    let hits = Arc::new(AtomicUsize::new(0));
    let seen = hits.clone();
    let port = server(move |_| {
        let seen = seen.clone();
        async move {
            seen.fetch_add(1, Ordering::Relaxed);
            tokio::time::sleep(Duration::from_secs(20)).await;
            Response::new(full(r#"{"balance":5}"#))
        }
    })
    .await;
    let (_t, g) = fixture(vec![format!("http://127.0.0.1:{port}")]).await;
    let start = Instant::now();
    let result = g
        .query_quota(&g.view().providers[0].id, true)
        .await
        .unwrap();
    assert_eq!(result.error.as_deref(), Some("额度查询超时"));
    assert!(start.elapsed() < Duration::from_secs(12));
    assert_eq!(hits.load(Ordering::Relaxed), 1);
    let view = g.view();
    assert_eq!(view.providers[0].health.requests, 0);
    assert_eq!(view.providers[0].health.failures, 0);
    assert_eq!(view.providers[0].active_requests, 0);
    assert_eq!(view.waiting_requests, 0);
}

#[tokio::test]
async fn recognized_expired_sub2_stops_protocol_probing() {
    let hits = Arc::new(AtomicUsize::new(0));
    let seen = hits.clone();
    let port = server(move |_| {
        let seen = seen.clone();
        async move {
            seen.fetch_add(1, Ordering::Relaxed);
            Response::builder()
                .status(403)
                .body(full(r#"{"status":"expired","isValid":false}"#))
                .unwrap()
        }
    })
    .await;
    let (_t, g) = fixture(vec![format!("http://127.0.0.1:{port}")]).await;
    let result = g
        .query_quota(&g.view().providers[0].id, true)
        .await
        .unwrap();
    assert_eq!(result.source.as_deref(), Some("sub2api"));
    assert_eq!(result.key_status.as_deref(), Some("已过期"));
    assert_eq!(hits.load(Ordering::Relaxed), 1);
}

#[path = "concurrency_tests.rs"]
mod concurrency;
#[path = "revision_tests.rs"]
mod revisions;

#[path = "model_tests.rs"]
mod models;

#[path = "v061_tests.rs"]
mod v061;

#[path = "import_tests.rs"]
mod imports;

#[path = "v080_tests.rs"]
mod v080;

#[path = "capacity_tests.rs"]
mod capacity;
#[path = "claude_tests.rs"]
mod claude_v080;
#[path = "websocket_bridge_tests.rs"]
mod websocket_bridge;

#[path = "v013_tests.rs"]
mod v013;

#[path = "quota_tests.rs"]
mod quota_v013;

#[path = "../events_tests.rs"]
mod events_v013;

#[path = "v016_tests.rs"]
mod v016;

#[path = "v017_tests.rs"]
mod v017;

#[path = "v019_compaction_tests.rs"]
mod v019_compaction;
