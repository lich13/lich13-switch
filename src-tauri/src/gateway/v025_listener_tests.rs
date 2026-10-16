use super::{grok_config::Protocol, listener, ClientId, Edit, Gateway};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{body::Incoming, Request, Response, StatusCode, Uri};
use hyper_util::rt::TokioIo;
use serde_json::{json, Value};
use std::{
    collections::HashSet,
    convert::Infallible,
    fs,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
    task::JoinHandle,
};

const MODEL: &str = "fixture-model";

fn free_port() -> u16 {
    std::net::TcpListener::bind(("127.0.0.1", 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn different_free_port(other: u16) -> u16 {
    loop {
        let port = free_port();
        if port != other {
            return port;
        }
    }
}

fn routed(uri: &str) -> Option<(ClientId, String)> {
    let uri: Uri = uri.parse().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let codex = Gateway::new(temp.path().join("data")).unwrap();
    let _claude = codex.companion(temp.path().join("data/claude")).unwrap();
    let _grok = codex.grok_companion(temp.path().join("data/grok")).unwrap();
    let selected = if uri.path().starts_with("/claude") {
        ClientId::Claude
    } else if uri.path().starts_with("/grok") {
        ClientId::Grok
    } else {
        ClientId::Codex
    };
    let token = local_token(&codex.0.registry.gateway(selected).unwrap());
    let mut headers = hyper::HeaderMap::new();
    headers.insert("authorization", format!("Bearer {token}").parse().unwrap());
    listener::route(&uri, &headers, &codex.0.registry)
        .ok()
        .map(|(client, uri)| {
            (
                client,
                uri.path_and_query()
                    .expect("routed URI should retain its path")
                    .as_str()
                    .to_owned(),
            )
        })
}

#[test]
fn routes_match_client_prefix_boundaries_and_preserve_opaque_queries() {
    for (request, client, expected) in [
        (
            "/codex/v1/responses?state=%2f&next=%2F+%2520&state=two",
            ClientId::Codex,
            "/v1/responses?state=%2f&next=%2F+%2520&state=two",
        ),
        (
            "/claude/v1/messages?api=one%2Ftwo&api=three+four",
            ClientId::Claude,
            "/v1/messages?api=one%2Ftwo&api=three+four",
        ),
        (
            "/grok/v1/models?x=%2f%2F&flag&empty=",
            ClientId::Grok,
            "/v1/models?x=%2f%2F&flag&empty=",
        ),
        (
            "/codex?sig=%2f&sig=%2F",
            ClientId::Codex,
            "/?sig=%2f&sig=%2F",
        ),
        ("/claude?sig=a+b", ClientId::Claude, "/?sig=a+b"),
        ("/grok?sig=%252f", ClientId::Grok, "/?sig=%252f"),
    ] {
        assert_eq!(
            routed(request),
            Some((client, expected.to_owned())),
            "{request}"
        );
    }

    for request in [
        "/codexish/v1/responses",
        "/claude2/v1/messages",
        "/grok-v1/responses",
        "/messages?model=fixture",
        "/responses?model=fixture",
    ] {
        assert_eq!(routed(request), None, "{request}");
    }
}

#[test]
fn unprefixed_v1_keeps_its_path_with_codex_credentials() {
    for request in ["/v1", "/v1/", "/v1/responses?x=%2f&x=%2F"] {
        assert_eq!(
            routed(request),
            Some((ClientId::Codex, request.to_owned())),
            "{request}"
        );
    }

    assert_eq!(routed("/messages"), None);
    assert_eq!(
        routed("/claude/messages"),
        Some((ClientId::Claude, "/messages".into()))
    );
    assert_eq!(
        routed("/grok/responses"),
        Some((ClientId::Grok, "/responses".into()))
    );
}

fn optional_bytes(path: &Path) -> Option<Vec<u8>> {
    fs::read(path).ok()
}

fn assert_same_listener(codex: &Gateway, claude: &Gateway, grok: &Gateway) {
    let expected = codex.listener_view();
    for peer in [claude, grok] {
        let view = peer.listener_view();
        assert_eq!(view.port, expected.port);
        assert_eq!(view.revision, expected.revision);
        assert_eq!(view.running_clients, expected.running_clients);
    }
}

#[test]
fn listener_inherits_legacy_codex_port_and_persists_its_independent_revision() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    let legacy_port = free_port();

    // Simulate an existing Codex-only install: its gateway.json has the chosen
    // port, while the shared-listener file is absent as it would be pre-upgrade.
    let legacy = Gateway::new(data.clone()).unwrap();
    let mut settings = legacy.view().settings;
    settings.port = legacy_port;
    legacy
        .edit(
            Edit::Settings { settings },
            &legacy.view().revision,
            temp.path(),
        )
        .unwrap();
    drop(legacy);
    fs::remove_file(data.join("gateway-listener.json")).unwrap();

    let codex = Gateway::new(data.clone()).unwrap();
    let claude = codex.companion(data.join("claude")).unwrap();
    let grok = codex.grok_companion(data.join("grok")).unwrap();
    assert_same_listener(&codex, &claude, &grok);
    assert_eq!(codex.listener_view().port, legacy_port);

    let listener_before = codex.listener_view();
    let codex_revision_before = codex.view().revision;
    let claude_revision_before = claude.view().revision;
    let grok_revision_before = grok.view().revision;
    assert_ne!(listener_before.revision, codex_revision_before);
    assert_ne!(codex_revision_before, claude_revision_before);
    assert_ne!(claude_revision_before, grok_revision_before);

    let stores_before = [
        optional_bytes(&data.join("gateway.json")),
        optional_bytes(&data.join("claude/gateway.json")),
        optional_bytes(&data.join("grok/gateway.json")),
    ];
    let next_port = different_free_port(legacy_port);

    // The listener revision is a separate concurrency token from each client's
    // editor revision; only the former can authorize a listener-port update.
    let conflict = codex
        .set_listener_port(next_port, &codex_revision_before)
        .err()
        .expect("client revision must not authorize a shared-listener update");
    assert_eq!(conflict.code, "CONFLICT");
    assert_eq!(codex.listener_view().revision, listener_before.revision);

    let updated = codex
        .set_listener_port(next_port, &listener_before.revision)
        .unwrap();
    assert_eq!(updated.port, next_port);
    assert_ne!(updated.revision, listener_before.revision);
    assert_same_listener(&codex, &claude, &grok);
    assert_ne!(codex.view().revision, codex_revision_before);
    assert_ne!(claude.view().revision, claude_revision_before);
    assert_ne!(grok.view().revision, grok_revision_before);
    assert_eq!(
        [
            optional_bytes(&data.join("gateway.json")),
            optional_bytes(&data.join("claude/gateway.json")),
            optional_bytes(&data.join("grok/gateway.json")),
        ],
        stores_before,
        "changing the shared port must not rewrite per-client stores"
    );

    let listener_file = fs::read(data.join("gateway-listener.json")).unwrap();
    let persisted: Value = serde_json::from_slice(&listener_file).unwrap();
    assert_eq!(persisted["version"], 2);
    assert_eq!(persisted["port"], next_port);
    let saved_revision = updated.revision;

    drop(grok);
    drop(claude);
    drop(codex);
    let reopened = Gateway::new(data).unwrap();
    assert_eq!(reopened.listener_view().port, next_port);
    assert_eq!(reopened.listener_view().revision, saved_revision);
}

#[derive(Clone, Debug)]
struct SeenRequest {
    path: String,
    model: Option<String>,
    authorization: Option<String>,
    api_key: Option<String>,
}

struct Upstream {
    port: u16,
    seen: Arc<Mutex<Vec<SeenRequest>>>,
    shutdown: Option<oneshot::Sender<()>>,
    task: JoinHandle<()>,
}

impl Upstream {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let captured = seen.clone();
        let (shutdown, mut shutdown_rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = &mut shutdown_rx => break,
                    accepted = listener.accept() => {
                        let Ok((socket, _)) = accepted else { break; };
                        let seen = captured.clone();
                        tokio::spawn(async move {
                            let service = hyper::service::service_fn(move |request: Request<Incoming>| {
                                let seen = seen.clone();
                                async move {
                                    let path = request.uri().path_and_query()
                                        .map(|part| part.as_str().to_owned())
                                        .unwrap_or_default();
                                    let authorization = request.headers().get("authorization").and_then(|h| h.to_str().ok()).map(str::to_owned);
                                    let api_key = request.headers().get("x-api-key").and_then(|h| h.to_str().ok()).map(str::to_owned);
                                    let body = request.into_body().collect().await
                                        .expect("fixture request body should be readable")
                                        .to_bytes();
                                    let model = serde_json::from_slice::<Value>(&body).ok()
                                        .and_then(|value| value.get("model")
                                            .and_then(Value::as_str).map(str::to_owned));
                                    seen.lock().unwrap().push(SeenRequest { path, model, authorization, api_key });
                                    Ok::<_, Infallible>(Response::builder()
                                        .status(StatusCode::OK)
                                        .header("content-type", "application/json")
                                        .body(Full::new(Bytes::from_static(b"{\"ok\":true}")))
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
            task,
        }
    }

    async fn stop(mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        let _ = self.task.await;
    }
}

fn seed_client_configs(codex_home: &Path, claude_home: &Path, grok_home: &Path) {
    fs::create_dir_all(codex_home).unwrap();
    fs::create_dir_all(claude_home).unwrap();
    fs::create_dir_all(grok_home).unwrap();
    fs::write(
        codex_home.join("config.toml"),
        "model_provider = 'custom'\n[model_providers.custom]\nbase_url = 'https://codex-original.example.invalid/v1'\nexperimental_bearer_token = 'fixture-original-codex-token'\nwire_api = 'responses'\n",
    )
    .unwrap();
    fs::write(
        claude_home.join("settings.json"),
        serde_json::to_vec(&json!({
            "env": {
                "ANTHROPIC_BASE_URL": "https://claude-original.example.invalid/v1",
                "ANTHROPIC_AUTH_TOKEN": "fixture-original-claude-token"
            }
        }))
        .unwrap(),
    )
    .unwrap();
    fs::write(
        grok_home.join("config.toml"),
        "[models]\ndefault = 'fixture-selected'\n[model.fixture-selected]\nmodel_provider = 'fixture-provider'\nmodel = 'fixture-model'\n[model_providers.fixture-provider]\nbase_url = 'https://grok-original.example.invalid/v1'\napi_key = 'fixture-original-grok-token'\napi_backend = 'responses'\n",
    )
    .unwrap();
}

fn add_provider(gateway: &Gateway, home: &Path, base_url: &str, token: &str) {
    let edit = match gateway.client_id() {
        ClientId::Grok => Edit::SaveGrokProvider {
            id: None,
            base_url: base_url.to_owned(),
            token: token.to_owned(),
            name: Some("fixture-provider".into()),
            protocol: Protocol::Responses,
        },
        ClientId::Codex | ClientId::Claude => Edit::SaveProvider {
            id: None,
            base_url: base_url.to_owned(),
            token: token.to_owned(),
            name: Some("fixture-provider".into()),
        },
    };
    gateway.edit(edit, &gateway.view().revision, home).unwrap();
    let id = gateway.view().providers[0].id.clone();
    gateway
        .edit(
            Edit::ModelsProvider {
                id,
                allowed_models: Some(vec![MODEL.into()]),
            },
            &gateway.view().revision,
            home,
        )
        .unwrap();
}

fn local_token(gateway: &Gateway) -> String {
    gateway.0.inner.lock().unwrap().store.local_token.clone()
}

async fn post(port: u16, path: &str, token: &str, body: &[u8]) -> u16 {
    let mut socket = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let head = format!(
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    socket.write_all(head.as_bytes()).await.unwrap();
    socket.write_all(body).await.unwrap();

    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), socket.read_to_end(&mut response))
        .await
        .expect("gateway should close the fixture TCP response")
        .unwrap();
    let header_end = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("gateway should return an HTTP response");
    let status_line = std::str::from_utf8(&response[..header_end])
        .unwrap()
        .lines()
        .next()
        .unwrap();
    status_line
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap()
}

async fn start(gateway: &Gateway, home: &Path) {
    gateway.start(&gateway.view().revision, home).await.unwrap();
}

#[tokio::test]
async fn shared_tcp_listener_isolates_three_clients_and_releases_after_last_stop() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    let codex = Gateway::new(data.clone()).unwrap();
    let claude = codex.companion(data.join("claude")).unwrap();
    let grok = codex.grok_companion(data.join("grok")).unwrap();
    let upstream = Upstream::start().await;
    let upstream_url = format!("http://127.0.0.1:{}/v1", upstream.port);
    let listener_port = different_free_port(upstream.port);
    codex
        .set_listener_port(listener_port, &codex.listener_view().revision)
        .unwrap();

    let codex_home = temp.path().join("home/codex");
    let claude_home = temp.path().join("home/claude");
    let grok_home = temp.path().join("home/grok");
    seed_client_configs(&codex_home, &claude_home, &grok_home);
    add_provider(
        &codex,
        &codex_home,
        &upstream_url,
        "fixture-codex-upstream-token",
    );
    add_provider(
        &claude,
        &claude_home,
        &upstream_url,
        "fixture-claude-upstream-token",
    );
    add_provider(
        &grok,
        &grok_home,
        &upstream_url,
        "fixture-grok-upstream-token",
    );

    let listener_config_before = fs::read(data.join("gateway-listener.json")).unwrap();
    let stores_before = [
        fs::read(data.join("gateway.json")).unwrap(),
        fs::read(data.join("claude/gateway.json")).unwrap(),
        fs::read(data.join("grok/gateway.json")).unwrap(),
    ];
    let client_configs_before = [
        fs::read(codex_home.join("config.toml")).unwrap(),
        fs::read(claude_home.join("settings.json")).unwrap(),
        fs::read(grok_home.join("config.toml")).unwrap(),
    ];

    // A failed bind must leave the listener file and every client config/store
    // byte-for-byte intact so the user can retry or choose another port.
    let occupied = TcpListener::bind(("127.0.0.1", listener_port))
        .await
        .unwrap();
    let error = codex
        .start(&codex.view().revision, &codex_home)
        .await
        .err()
        .expect("occupied shared port should reject gateway startup");
    assert_eq!(error.code, "PORT");
    assert!(codex.listener_view().running_clients.is_empty());
    assert_eq!(
        fs::read(data.join("gateway-listener.json")).unwrap(),
        listener_config_before
    );
    assert_eq!(
        [
            fs::read(data.join("gateway.json")).unwrap(),
            fs::read(data.join("claude/gateway.json")).unwrap(),
            fs::read(data.join("grok/gateway.json")).unwrap(),
        ],
        stores_before
    );
    assert_eq!(
        [
            fs::read(codex_home.join("config.toml")).unwrap(),
            fs::read(claude_home.join("settings.json")).unwrap(),
            fs::read(grok_home.join("config.toml")).unwrap(),
        ],
        client_configs_before
    );
    drop(occupied);

    start(&codex, &codex_home).await;
    start(&claude, &claude_home).await;
    start(&grok, &grok_home).await;
    let running: HashSet<_> = codex.listener_view().running_clients.into_iter().collect();
    assert_eq!(
        running,
        HashSet::from([ClientId::Codex, ClientId::Claude, ClientId::Grok])
    );
    assert_same_listener(&codex, &claude, &grok);

    let active_revision = codex.listener_view().revision;
    let port_error = codex
        .set_listener_port(different_free_port(listener_port), &active_revision)
        .err()
        .expect("shared port must remain immutable while any client is running");
    assert_eq!(port_error.code, "GATEWAY_ACTIVE");
    assert_eq!(
        fs::read(data.join("gateway-listener.json")).unwrap(),
        listener_config_before
    );

    let tokens = [
        local_token(&codex),
        local_token(&claude),
        local_token(&grok),
    ];
    assert_ne!(tokens[0], tokens[1]);
    assert_ne!(tokens[1], tokens[2]);
    assert_ne!(tokens[0], tokens[2]);
    let clients = [
        (ClientId::Codex, "/codex/v1/responses", tokens[0].clone()),
        (ClientId::Claude, "/claude/v1/messages", tokens[1].clone()),
        (ClientId::Grok, "/grok/v1/responses", tokens[2].clone()),
    ];
    let body = serde_json::to_vec(&json!({
        "model": MODEL,
        "input": [{"role": "user", "content": "fixture"}],
        "messages": [{"role": "user", "content": "fixture"}],
        "max_tokens": 4
    }))
    .unwrap();

    // Each client uses the same model name; only its own local token succeeds.
    for (target, path, _) in &clients {
        for (token_owner, _, token) in &clients {
            let status = post(listener_port, path, token, &body).await;
            assert_eq!(
                status,
                if target == token_owner { 200 } else { 401 },
                "target={target:?}, token_owner={token_owner:?}"
            );
        }
    }
    let requests = upstream.seen.lock().unwrap().clone();
    assert_eq!(
        requests.len(),
        3,
        "only authorized requests should reach upstream"
    );
    assert!(requests
        .iter()
        .all(|request| request.model.as_deref() == Some(MODEL)));
    let mut paths: Vec<_> = requests
        .iter()
        .map(|request| request.path.as_str())
        .collect();
    paths.sort_unstable();
    assert_eq!(paths, ["/v1/messages", "/v1/responses", "/v1/responses"]);

    for (index, path) in [
        "/v1/responses?fixture=%2f",
        "/v1/messages?fixture=%2F",
        "/v1/responses?fixture=a+b",
    ]
    .into_iter()
    .enumerate()
    {
        assert_eq!(post(listener_port, path, &tokens[index], &body).await, 200);
    }
    let requests = upstream.seen.lock().unwrap().clone();
    assert_eq!(requests.len(), 6);
    for (index, request) in requests[3..].iter().enumerate() {
        let client = ["codex", "claude", "grok"][index];
        assert_eq!(
            request.authorization.as_deref(),
            Some(format!("Bearer fixture-{client}-upstream-token").as_str())
        );
        assert!(request.api_key.is_none());
    }
    assert_eq!(requests[3].path, "/v1/responses?fixture=%2f");
    assert_eq!(requests[4].path, "/v1/messages?fixture=%2F");
    assert_eq!(requests[5].path, "/v1/responses?fixture=a+b");

    claude.stop().await.unwrap();
    let running: HashSet<_> = codex.listener_view().running_clients.into_iter().collect();
    assert_eq!(running, HashSet::from([ClientId::Codex, ClientId::Grok]));
    assert_eq!(
        post(listener_port, "/codex/v1/responses", &tokens[0], &body).await,
        200
    );
    assert_eq!(
        post(listener_port, "/grok/v1/responses", &tokens[2], &body).await,
        200
    );
    assert_eq!(
        post(listener_port, "/claude/v1/messages", &tokens[1], &body).await,
        503
    );

    codex.stop().await.unwrap();
    assert_eq!(codex.listener_view().running_clients, vec![ClientId::Grok]);
    grok.stop().await.unwrap();
    assert!(codex.listener_view().running_clients.is_empty());
    let released = TcpListener::bind(("127.0.0.1", listener_port))
        .await
        .expect("the shared port should be released after the last client stops");
    drop(released);

    let new_port = different_free_port(listener_port);
    let updated = codex
        .set_listener_port(new_port, &codex.listener_view().revision)
        .unwrap();
    assert_eq!(updated.port, new_port);
    upstream.stop().await;
}
