use super::{Edit, Gateway};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{body::Incoming, Request, Response};
use hyper_util::rt::TokioIo;
use std::{
    convert::Infallible,
    fs,
    net::TcpListener as StdTcpListener,
    panic::{catch_unwind, AssertUnwindSafe},
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::oneshot,
    task::JoinHandle,
};

struct Upstream {
    port: u16,
    seen: Arc<Mutex<Vec<(String, Option<String>)>>>,
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
}

impl Upstream {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&seen);
        let (shutdown, mut shutdown_rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = &mut shutdown_rx => break,
                    accepted = listener.accept() => {
                        let Ok((socket, _)) = accepted else { break };
                        let seen = Arc::clone(&captured);
                        tokio::spawn(async move {
                            let service = hyper::service::service_fn(move |request: Request<Incoming>| {
                                let seen = Arc::clone(&seen);
                                async move {
                                    let path = request.uri().path().to_owned();
                                    let authorization = request.headers()
                                        .get(hyper::header::AUTHORIZATION)
                                        .and_then(|value| value.to_str().ok())
                                        .map(str::to_owned);
                                    let _ = request.into_body().collect().await;
                                    seen.lock().unwrap().push((path, authorization));
                                    Ok::<_, Infallible>(Response::builder()
                                        .status(200)
                                        .header("content-type", "application/json")
                                        .body(Full::new(Bytes::from_static(b"{\"data\":[]}")))
                                        .unwrap())
                                }
                            });
                            let _ = hyper::server::conn::http1::Builder::new()
                                .serve_connection(TokioIo::new(socket), service)
                                .await;
                        });
                    }
                }
            }
        });
        Self {
            port,
            seen,
            shutdown: Some(shutdown),
            task: Some(task),
        }
    }

    fn requests(&self) -> Vec<(String, Option<String>)> {
        self.seen.lock().unwrap().clone()
    }

    async fn stop(mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for Upstream {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

fn free_port() -> u16 {
    StdTcpListener::bind(("127.0.0.1", 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn listener_port(excluded: &[u16]) -> u16 {
    loop {
        let port = free_port();
        if !excluded.contains(&port) {
            return port;
        }
    }
}

fn seed_configs(codex_home: &Path, claude_home: &Path) {
    fs::create_dir_all(codex_home).unwrap();
    fs::create_dir_all(claude_home).unwrap();
    fs::write(
        codex_home.join("config.toml"),
        "model_provider = 'custom'\n[model_providers.custom]\nbase_url = 'https://codex-original.example.invalid/v1'\nexperimental_bearer_token = 'fixture-original-codex-token'\nwire_api = 'responses'\n",
    )
    .unwrap();
    fs::write(
        claude_home.join("settings.json"),
        br#"{"env":{"ANTHROPIC_BASE_URL":"https://claude-original.example.invalid/v1","ANTHROPIC_AUTH_TOKEN":"fixture-original-claude-token"}}"#,
    )
    .unwrap();
}

fn add_provider(gateway: &Gateway, home: &Path, base_url: &str, token: &str) {
    gateway
        .edit(
            Edit::SaveProvider {
                id: None,
                base_url: base_url.to_owned(),
                token: token.to_owned(),
                name: Some("fixture-provider".into()),
            },
            &gateway.view().revision,
            home,
        )
        .unwrap();
    let id = gateway.view().providers[0].id.clone();
    gateway
        .edit(
            Edit::ModelsProvider {
                id,
                allowed_models: Some(vec!["fixture-model".into()]),
            },
            &gateway.view().revision,
            home,
        )
        .unwrap();
}

fn local_token(gateway: &Gateway) -> String {
    gateway.0.inner.lock().unwrap().store.local_token.clone()
}

async fn get(port: u16, path: &str, token: &str) -> u16 {
    let mut socket = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\nConnection: close\r\n\r\n"
    );
    socket.write_all(request.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), socket.read_to_end(&mut response))
        .await
        .expect("fixture gateway must finish the HTTP response")
        .unwrap();
    let header_end = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("fixture gateway must return an HTTP response");
    std::str::from_utf8(&response[..header_end])
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap()
}

#[tokio::test]
async fn poisoned_client_gets_503_while_peer_and_token_routing_remain_scoped() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("gateway-data");
    let codex = Gateway::new(data.clone()).unwrap();
    let claude = codex.companion(data.join("claude")).unwrap();
    let codex_upstream = Upstream::start().await;
    let claude_upstream = Upstream::start().await;
    let port = listener_port(&[codex_upstream.port, claude_upstream.port]);
    codex
        .set_listener_port(port, &codex.listener_view().revision)
        .unwrap();

    let codex_home = dir.path().join("home/codex");
    let claude_home = dir.path().join("home/claude");
    seed_configs(&codex_home, &claude_home);
    add_provider(
        &codex,
        &codex_home,
        &format!("http://127.0.0.1:{}/v1", codex_upstream.port),
        "fixture-codex-upstream-token",
    );
    add_provider(
        &claude,
        &claude_home,
        &format!("http://127.0.0.1:{}/v1", claude_upstream.port),
        "fixture-claude-upstream-token",
    );
    codex
        .start(&codex.view().revision, &codex_home)
        .await
        .unwrap();
    claude
        .start(&claude.view().revision, &claude_home)
        .await
        .unwrap();

    let codex_token = local_token(&codex);
    let claude_token = local_token(&claude);
    assert_ne!(codex_token, claude_token);
    assert_eq!(
        get(port, "/claude/v1/models", &codex_token).await,
        401,
        "one client's local token must not authorize its peer prefix"
    );
    assert!(claude_upstream.requests().is_empty());

    let saved_provider = codex.0.inner.lock().unwrap().store.providers[0].clone();
    let gateway_json_before = fs::read(data.join("gateway.json")).unwrap();
    let injected = catch_unwind(AssertUnwindSafe(|| {
        let _state = codex.0.inner.lock().unwrap();
        panic!("inject Codex request-state poison");
    }));
    assert!(injected.is_err());
    assert!(codex.0.inner.is_poisoned());

    let successful_response = catch_unwind(AssertUnwindSafe(|| {
        codex.successful_response(&saved_provider);
    }));
    assert!(successful_response.is_ok());
    assert_eq!(
        fs::read(data.join("gateway.json")).unwrap(),
        gateway_json_before,
        "successful-response persistence must not rewrite the store after request-state poison"
    );

    assert_eq!(get(port, "/codex/v1/models", &codex_token).await, 503);
    assert_eq!(get(port, "/claude/v1/models", &claude_token).await, 200);
    assert!(codex_upstream.requests().is_empty());
    assert_eq!(
        claude_upstream.requests(),
        vec![(
            "/v1/models".to_owned(),
            Some("Bearer fixture-claude-upstream-token".to_owned())
        )]
    );

    codex.0.inner.clear_poison();
    codex.stop().await.unwrap();
    claude.stop().await.unwrap();
    codex_upstream.stop().await;
    claude_upstream.stop().await;
}
