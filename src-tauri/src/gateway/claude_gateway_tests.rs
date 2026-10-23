// 覆盖 Claude 原生 Messages 网关及配置恢复。
use super::*;
use crate::gateway::admission::{Budget, Rejected};

const CODEX_TOKEN: &str = "codex-v080-fixture-token";
const CLAUDE_TOKEN: &str = "claude-v080-fixture-token";

struct DualFixture {
    _temp: tempfile::TempDir,
    data: PathBuf,
    codex_home: PathBuf,
    claude_home: PathBuf,
    codex: Gateway,
    claude: Gateway,
}

fn claude_settings(base: &str) -> String {
    format!(
        "{{\r\n  \"model\" : \"claude-fixture\",\r\n  \"env\": {{\r\n    \"ANTHROPIC_BASE_URL\" : {},\r\n    \"UNMANAGED\": {{\"enabled\":true,\"values\":[1, 2]}},\r\n    \"ANTHROPIC_AUTH_TOKEN\": \"{CLAUDE_TOKEN}\"\r\n  }},\r\n  \"permissions\": {{\"allow\":[\"Read\"], \"deny\":[]}},\r\n  \"custom\": {{\"label\":\"保留\",\"nested\":{{\"env\":\"untouched\"}}}}\r\n}}\r\n",
        serde_json::to_string(base).unwrap()
    )
}

fn edit_client(gateway: &Gateway, home: &Path, edit: Edit) -> View {
    gateway.edit(edit, &gateway.view().revision, home).unwrap()
}

async fn start_client(gateway: &Gateway, home: &Path) {
    gateway.start(&gateway.view().revision, home).await.unwrap();
}

async fn dual_fixture(codex_urls: Vec<String>, claude_urls: Vec<String>) -> DualFixture {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    let codex_home = temp.path().join("codex-home");
    let claude_home = temp.path().join("claude-home");
    std::fs::create_dir_all(&codex_home).unwrap();
    std::fs::create_dir_all(&claude_home).unwrap();
    let codex_base = codex_urls
        .first()
        .map(String::as_str)
        .unwrap_or("https://codex.fixture.invalid/v1");
    let claude_base = claude_urls
        .first()
        .map(String::as_str)
        .unwrap_or("https://claude.fixture.invalid/deployment");
    let config = format!(
        "# keep this comment\nmodel_provider = \"custom\"\nmodel = \"gpt-fixture\"\n[model_providers.custom]\nbase_url = {}\nexperimental_bearer_token = \"{CODEX_TOKEN}\"\nwire_api = \"responses\"\nsupports_websockets = false\n[unmanaged]\nlabel = \"保留\"\nvalues = [1, 2, 3]\n",
        serde_json::to_string(codex_base).unwrap()
    );
    std::fs::write(codex_home.join("config.toml"), config).unwrap();
    std::fs::write(
        codex_home.join("auth.json"),
        b"{\n  \"OPENAI_API_KEY\": \"v080-isolated-fake-auth\"\n}\n",
    )
    .unwrap();
    std::fs::write(
        claude_home.join("settings.json"),
        claude_settings(claude_base),
    )
    .unwrap();
    let codex = Gateway::new(data.clone()).unwrap();
    let claude = codex.companion(data.join("claude")).unwrap();
    assert_eq!(codex.view().client_id, ClientId::Codex);
    assert_eq!(claude.view().client_id, ClientId::Claude);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    for (gateway, home) in [(&codex, &codex_home), (&claude, &claude_home)] {
        edit_client(
            gateway,
            home,
            Edit::Settings {
                settings: Settings {
                    port: listener.local_addr().unwrap().port(),
                    max_retries: 0,
                    connect_seconds: 2,
                    first_byte_seconds: 2,
                    idle_seconds: 2,
                    total_seconds: 5,
                    queue_seconds: 3,
                    ..Default::default()
                },
            },
        );
    }
    for (gateway, home, urls, token) in [
        (&codex, &codex_home, codex_urls, CODEX_TOKEN),
        (&claude, &claude_home, claude_urls, CLAUDE_TOKEN),
    ] {
        for base_url in urls {
            edit_client(
                gateway,
                home,
                Edit::SaveProvider {
                    quota_access_token: None,
                    id: None,
                    base_url,
                    token: token.into(),
                    name: None,
                },
            );
        }
    }
    DualFixture {
        _temp: temp,
        data,
        codex_home,
        claude_home,
        codex,
        claude,
    }
}

fn file_hash(path: impl AsRef<Path>) -> String {
    storage::digest(&std::fs::read(path).unwrap())
}

fn config_hashes(fixture: &DualFixture) -> [String; 3] {
    [
        file_hash(fixture.codex_home.join("config.toml")),
        file_hash(fixture.codex_home.join("auth.json")),
        file_hash(fixture.claude_home.join("settings.json")),
    ]
}

async fn wait_until(test: impl Fn() -> bool) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while !test() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("gateway state did not settle");
}

async fn response_bytes(gateway: &Gateway, path: &str) -> Bytes {
    let response = request(gateway, path, vec![], vec![]).await;
    assert_eq!(response.status(), 200);
    response.into_body().collect().await.unwrap().to_bytes()
}

#[test]
fn claude_two_field_patch_round_trips_without_reserializing_unmanaged_settings() {
    let original = claude_settings("https://old.fixture.invalid/deployment");
    let before = claude_config::pair(&original).unwrap();
    let target = takeover::Pair::new(
        "https://new.fixture.invalid/other",
        "fixture-quote-\"-backslash-\\",
    );
    let patched = claude_config::patch(&original, &target).unwrap();
    let expected = original
        .replace(
            "\"https://old.fixture.invalid/deployment\"",
            "\"https://new.fixture.invalid/other\"",
        )
        .replace(
            &serde_json::to_string(CLAUDE_TOKEN).unwrap(),
            &serde_json::to_string(target.token.as_ref().unwrap()).unwrap(),
        );
    assert_eq!(patched, expected);
    assert!(claude_config::pair(&patched).unwrap() == target);
    let restored = claude_config::patch(&patched, &before).unwrap();
    assert_eq!(
        storage::digest(restored.as_bytes()),
        storage::digest(original.as_bytes())
    );
    assert_eq!(restored, original);
}

#[tokio::test]
async fn dual_hot_switch_preserves_live_config_hashes_and_stops_to_each_selected_pair() {
    let f = dual_fixture(
        vec![
            "https://codex-a.fixture.invalid/v1".into(),
            "https://codex-b.fixture.invalid/v1".into(),
        ],
        vec![
            "https://claude-a.fixture.invalid/deployment".into(),
            "https://claude-b.fixture.invalid/deployment/v1".into(),
        ],
    )
    .await;
    let original_codex = std::fs::read_to_string(f.codex_home.join("config.toml")).unwrap();
    let original_claude = std::fs::read_to_string(f.claude_home.join("settings.json")).unwrap();
    let initial_hashes = config_hashes(&f);
    tokio::join!(
        start_client(&f.codex, &f.codex_home),
        start_client(&f.claude, &f.claude_home)
    );
    let (_, live_claude_pair) = takeover::read_for(ClientId::Claude, &f.claude_home).unwrap();
    assert_eq!(
        live_claude_pair.base_url.as_deref(),
        Some(f.claude.view().address.as_str())
    );
    assert_eq!(
        std::fs::read_to_string(f.claude_home.join("settings.json")).unwrap(),
        claude_config::patch(&original_claude, &live_claude_pair).unwrap()
    );
    let live_hashes = config_hashes(&f);
    assert_ne!(live_hashes[0], initial_hashes[0]);
    assert_ne!(live_hashes[2], initial_hashes[2]);
    assert_eq!(live_hashes[1], initial_hashes[1]);
    for (gateway, home) in [(&f.codex, &f.codex_home), (&f.claude, &f.claude_home)] {
        let first = gateway.view().providers[0].id.clone();
        let second = gateway.view().providers[1].id.clone();
        for edit in [
            Edit::Select { id: second.clone() },
            Edit::Mode {
                mode: "auto".into(),
            },
            Edit::QueueProvider {
                id: first,
                queued: false,
            },
            Edit::ConcurrencyProvider {
                id: second,
                max_concurrency: 2,
            },
        ] {
            edit_client(gateway, home, edit);
            assert_eq!(config_hashes(&f), live_hashes);
        }
    }
    f.claude.stop().await.unwrap();
    assert!(f.codex.view().running);
    assert_eq!(file_hash(f.codex_home.join("config.toml")), live_hashes[0]);
    assert_eq!(
        std::fs::read_to_string(f.claude_home.join("settings.json")).unwrap(),
        claude_config::patch(
            &original_claude,
            &takeover::Pair::new(
                "https://claude-b.fixture.invalid/deployment/v1",
                CLAUDE_TOKEN,
            )
        )
        .unwrap()
    );
    f.codex.stop().await.unwrap();
    assert_eq!(
        std::fs::read_to_string(f.codex_home.join("config.toml")).unwrap(),
        takeover::patch(
            &original_codex,
            &takeover::Pair::new("https://codex-b.fixture.invalid/v1", CODEX_TOKEN)
        )
        .unwrap()
    );
    assert_eq!(file_hash(f.codex_home.join("auth.json")), initial_hashes[1]);
}

#[tokio::test]
async fn shared_listener_keeps_selection_queues_and_concurrency_independent_in_both_directions() {
    let upstream =
        server(|request| async move { Response::new(full(request.uri().path().to_owned())) }).await;
    let f = dual_fixture(
        vec![
            format!("http://127.0.0.1:{upstream}/codex-a/v1"),
            format!("http://127.0.0.1:{upstream}/codex-b/v1"),
        ],
        vec![
            format!("http://127.0.0.1:{upstream}/claude-a"),
            format!("http://127.0.0.1:{upstream}/claude-b"),
        ],
    )
    .await;
    assert_eq!(f.codex.view().settings.port, f.claude.view().settings.port);
    tokio::join!(
        start_client(&f.codex, &f.codex_home),
        start_client(&f.claude, &f.claude_home)
    );
    assert_eq!(
        response_bytes(&f.codex, "/v1/responses").await,
        "/codex-a/v1/responses"
    );
    assert_eq!(
        response_bytes(&f.claude, "/claude/v1/messages").await,
        "/claude-a/v1/messages"
    );
    let codex_first = f.codex.view().providers[0].id.clone();
    let codex_second = f.codex.view().providers[1].id.clone();
    let claude_first = f.claude.view().providers[0].id.clone();
    let claude_second = f.claude.view().providers[1].id.clone();
    edit_client(
        &f.codex,
        &f.codex_home,
        Edit::Select {
            id: codex_second.clone(),
        },
    );
    edit_client(
        &f.codex,
        &f.codex_home,
        Edit::QueueProvider {
            id: codex_first,
            queued: false,
        },
    );
    edit_client(
        &f.codex,
        &f.codex_home,
        Edit::ConcurrencyProvider {
            id: codex_second.clone(),
            max_concurrency: 1,
        },
    );
    let claude_unchanged = f.claude.view();
    assert_eq!(
        claude_unchanged.selected.as_deref(),
        Some(claude_first.as_str())
    );
    assert!(claude_unchanged
        .providers
        .iter()
        .all(|p| p.queued && p.max_concurrency == 0));
    edit_client(
        &f.claude,
        &f.claude_home,
        Edit::QueueProvider {
            id: claude_second,
            queued: false,
        },
    );
    edit_client(
        &f.claude,
        &f.claude_home,
        Edit::ConcurrencyProvider {
            id: claude_first.clone(),
            max_concurrency: 1,
        },
    );
    let codex_unchanged = f.codex.view();
    assert_eq!(
        codex_unchanged.selected.as_deref(),
        Some(codex_second.as_str())
    );
    assert!(!codex_unchanged.providers[0].queued);
    assert!(codex_unchanged.providers[1].queued);
    assert_eq!(codex_unchanged.providers[1].max_concurrency, 1);
    let codex_routes = vec![f.codex.route(&codex_second).unwrap()];
    let claude_routes = vec![f.claude.route(&claude_first).unwrap()];
    let held_codex = f
        .codex
        .0
        .admission
        .acquire(&codex_routes, false, 1, &mut Budget::new(3))
        .await
        .unwrap();
    let held_claude = f
        .claude
        .0
        .admission
        .acquire(&claude_routes, false, 1, &mut Budget::new(3))
        .await
        .unwrap();
    let codex_scheduler = f.codex.0.admission.clone();
    let codex_waiter = tokio::spawn(async move {
        codex_scheduler
            .acquire(&codex_routes, false, 1, &mut Budget::new(3))
            .await
    });
    let claude_scheduler = f.claude.0.admission.clone();
    let claude_waiter = tokio::spawn(async move {
        claude_scheduler
            .acquire(&claude_routes, false, 1, &mut Budget::new(3))
            .await
    });
    wait_until(|| f.codex.view().waiting_requests == 1 && f.claude.view().waiting_requests == 1)
        .await;
    assert_eq!(f.codex.view().providers[1].active_requests, 1);
    assert_eq!(f.claude.view().providers[0].active_requests, 1);
    f.codex.stop().await.unwrap();
    assert!(matches!(
        codex_waiter.await.unwrap(),
        Err(Rejected::Stopped)
    ));
    assert!(f.claude.view().running);
    assert_eq!(f.claude.view().waiting_requests, 1);
    assert!(!claude_waiter.is_finished());
    let shared_port = f.codex.view().settings.port;
    assert_eq!(shared_port, f.claude.view().settings.port);
    assert!(
        tokio::net::TcpListener::bind(("127.0.0.1", shared_port))
            .await
            .is_err(),
        "the shared listener must remain active while Claude is running"
    );
    drop(held_codex);
    drop(held_claude);
    let admitted = tokio::time::timeout(Duration::from_secs(3), claude_waiter)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    drop(admitted);
    wait_until(|| {
        f.claude.view().waiting_requests == 0 && f.claude.view().providers[0].active_requests == 0
    })
    .await;
    assert_eq!(
        response_bytes(&f.claude, "/claude/v1/messages").await,
        "/claude-a/v1/messages"
    );
    start_client(&f.codex, &f.codex_home).await;
    f.claude.stop().await.unwrap();
    assert!(f.codex.view().running);
    assert_eq!(
        response_bytes(&f.codex, "/v1/responses").await,
        "/codex-b/v1/responses"
    );
    f.codex.stop().await.unwrap();
    let released_listener = tokio::net::TcpListener::bind(("127.0.0.1", shared_port))
        .await
        .unwrap();
    drop(released_listener);
}

#[tokio::test]
async fn claude_messages_preserve_deployment_path_query_payload_and_anthropic_headers() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let captured = seen.clone();
    let reply = br#"{"id":"msg_fixture","type":"message","model":"claude-fixture","stop_reason":"end_turn","content":[{"type":"text","text":"ok"}]}"#;
    let upstream = server(move |request| {
        let captured = captured.clone();
        async move {
            let (parts, body) = request.into_parts();
            let payload = body.collect().await.unwrap().to_bytes();
            captured.lock().unwrap().push((
                parts.uri.path_and_query().unwrap().to_string(),
                parts.headers,
                payload,
            ));
            Response::builder()
                .header("content-type", "application/json")
                .header("x-trace", "fixture-trace")
                .body(full(reply.as_slice()))
                .unwrap()
        }
    })
    .await;
    let f = dual_fixture(
        vec![],
        vec![format!(
            "http://127.0.0.1:{upstream}/deployment/tenant%20one/"
        )],
    )
    .await;
    start_client(&f.claude, &f.claude_home).await;
    let claude_local_token = f.claude.0.inner.lock().unwrap().store.local_token.clone();
    let payload = br#"{ "model":"claude-fixture", "max_tokens":17, "messages":[{"role":"user","content":"unchanged"}], "metadata":{"fixture":true} }"#.to_vec();
    let response = request(
        &f.claude,
        "/claude/v1/messages?beta=true&tag=a%2Fb&tag=two",
        payload.clone(),
        vec![
            ("content-type", "application/json"),
            ("anthropic-version", "2023-06-01"),
            ("anthropic-beta", "fixture-beta"),
            ("x-api-key", claude_local_token.as_str()),
            ("proxy-authorization", "Basic fixture-local-proxy"),
            ("x-extra", "preserved"),
        ],
    )
    .await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["x-trace"], "fixture-trace");
    assert_eq!(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .as_ref(),
        reply
    );
    {
        let captured = seen.lock().unwrap();
        assert_eq!(captured.len(), 1);
        let (path, headers, actual_payload) = &captured[0];
        assert_eq!(
            path,
            "/deployment/tenant%20one/v1/messages?beta=true&tag=a%2Fb&tag=two"
        );
        assert_eq!(headers["authorization"], format!("Bearer {CLAUDE_TOKEN}"));
        assert!(!headers.contains_key("x-api-key"));
        assert!(!headers.contains_key("proxy-authorization"));
        assert_eq!(headers["anthropic-version"], "2023-06-01");
        assert_eq!(headers["anthropic-beta"], "fixture-beta");
        assert_eq!(headers["x-extra"], "preserved");
        assert_eq!(actual_payload.as_ref(), payload);
    }
    f.claude.stop().await.unwrap();
}

#[tokio::test]
async fn claude_message_stop_is_success_but_missing_terminal_and_abrupt_disconnect_fail() {
    const START: &[u8] = b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_fixture\",\"type\":\"message\",\"model\":\"claude-fixture\",\"stop_reason\":null}}\n\n";
    const STOP: &[u8] = b"event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let release_upstream = release.clone();
    let upstream = server(move |request| {
        let release = release_upstream.clone();
        async move {
            let case = request.uri().query().unwrap_or_default().to_owned();
            let stream = async_stream::try_stream! {
                yield Frame::data(Bytes::from_static(START));
                if case == "case=complete" {
                    yield Frame::data(Bytes::copy_from_slice(&STOP[..23]));
                    yield Frame::data(Bytes::copy_from_slice(&STOP[23..]));
                } else if case == "case=abrupt" {
                    let _permit = release.acquire().await.unwrap();
                    Err::<(), std::io::Error>(std::io::Error::new(std::io::ErrorKind::ConnectionReset, "fixture disconnect"))?;
                }
            };
            Response::builder().header("content-type", "text/event-stream")
                .body(StreamBody::new(stream).map_err(|error: std::io::Error| -> connector::BoxError { error.into() }).boxed_unsync()).unwrap()
        }
    }).await;
    let f = dual_fixture(vec![], vec![format!("http://127.0.0.1:{upstream}/claude")]).await;
    start_client(&f.claude, &f.claude_home).await;
    let fixture_body = || br#"{"model":"claude-fixture"}"#.to_vec();
    let complete_response = request(
        &f.claude,
        "/claude/v1/messages?case=complete",
        fixture_body(),
        vec![("content-type", "application/json")],
    )
    .await;
    assert_eq!(complete_response.status(), 200);
    let complete = complete_response
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes();
    assert_eq!(complete.as_ref(), [START, STOP].concat());
    wait_until(|| f.claude.view().providers[0].active_requests == 0).await;
    assert_eq!(f.claude.view().providers[0].health.requests, 1);
    assert_eq!(f.claude.view().providers[0].health.failures, 0);
    let model_health = |gateway: &Gateway| {
        gateway
            .route(&gateway.view().providers[0].id)
            .unwrap()
            .model_circuit(&super::super::routing::Requirement::Model(
                "claude-fixture".into(),
            ))
            .unwrap()
            .health()
    };
    assert_eq!(model_health(&f.claude).requests, 1);
    assert_eq!(model_health(&f.claude).failures, 0);
    let eof_response = request(
        &f.claude,
        "/claude/v1/messages?case=eof",
        fixture_body(),
        vec![("content-type", "application/json")],
    )
    .await;
    assert_eq!(eof_response.status(), 200);
    assert_eq!(
        eof_response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .as_ref(),
        START
    );
    wait_until(|| f.claude.view().providers[0].active_requests == 0).await;
    assert_eq!(f.claude.view().providers[0].health.requests, 1);
    assert_eq!(f.claude.view().providers[0].health.failures, 0);
    assert_eq!(model_health(&f.claude).requests, 2);
    assert_eq!(model_health(&f.claude).failures, 1);
    let mut interrupted = request(
        &f.claude,
        "/claude/v1/messages?case=abrupt",
        fixture_body(),
        vec![("content-type", "application/json")],
    )
    .await;
    assert_eq!(interrupted.status(), 200);
    let first = interrupted.body_mut().frame().await.unwrap().unwrap();
    assert_eq!(first.data_ref().unwrap().as_ref(), START);
    assert_eq!(f.claude.view().providers[0].active_requests, 1);
    release.add_permits(1);
    assert!(interrupted.into_body().collect().await.is_err());
    wait_until(|| f.claude.view().providers[0].active_requests == 0).await;
    assert_eq!(f.claude.view().providers[0].health.requests, 1);
    assert_eq!(f.claude.view().providers[0].health.failures, 0);
    assert_eq!(model_health(&f.claude).requests, 3);
    assert_eq!(model_health(&f.claude).failures, 2);
    f.claude.stop().await.unwrap();
}

#[tokio::test]
async fn peer_gateway_urls_and_port_changes_that_create_cross_client_loops_are_rejected() {
    let f = dual_fixture(
        vec!["https://codex.fixture.invalid/v1".into()],
        vec!["https://claude.fixture.invalid".into()],
    )
    .await;
    for (gateway, home, peer) in [
        (&f.codex, &f.codex_home, &f.claude),
        (&f.claude, &f.claude_home, &f.codex),
    ] {
        let revision = gateway.view().revision;
        let before = file_hash(gateway.0.data.join("gateway.json"));
        for host in ["127.0.0.1", "localhost", "[::1]"] {
            let error = gateway
                .edit(
                    Edit::SaveProvider {
                        quota_access_token: None,
                        id: None,
                        base_url: format!("http://{host}:{}/loop", peer.view().settings.port),
                        token: "loop-fixture".into(),
                        name: None,
                    },
                    &revision,
                    home,
                )
                .err()
                .unwrap();
            assert_eq!(error.code, "LOOP");
            assert_eq!(gateway.view().revision, revision);
            assert_eq!(gateway.view().providers.len(), 1);
            assert_eq!(file_hash(gateway.0.data.join("gateway.json")), before);
        }
    }
    let reserved = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_port = reserved.local_addr().unwrap().port();
    edit_client(
        &f.codex,
        &f.codex_home,
        Edit::SaveProvider {
            quota_access_token: None,
            id: None,
            base_url: format!("http://127.0.0.1:{upstream_port}/deployment"),
            token: "future-loop-fixture".into(),
            name: None,
        },
    );
    let before = f.claude.view();
    let mut settings = before.settings.clone();
    settings.port = upstream_port;
    let error = f
        .claude
        .edit(
            Edit::Settings { settings },
            &before.revision,
            &f.claude_home,
        )
        .err()
        .unwrap();
    assert_eq!(error.code, "LOOP");
    assert_eq!(f.claude.view().settings, before.settings);
    assert_eq!(f.claude.view().revision, before.revision);
}

#[tokio::test]
async fn initial_claude_import_is_read_only_and_deleted_providers_stay_deleted_after_restart() {
    let f = dual_fixture(vec![], vec![]).await;
    let before = config_hashes(&f);
    f.claude.import_initial(&f.claude_home).unwrap();
    let imported = f.claude.view();
    assert_eq!(imported.providers.len(), 1);
    assert_eq!(
        imported.providers[0].base_url,
        "https://claude.fixture.invalid/deployment/v1"
    );
    assert_eq!(
        imported.selected.as_deref(),
        Some(imported.providers[0].id.as_str())
    );
    assert_eq!(
        f.claude
            .query_input(&imported.providers[0].id)
            .unwrap()
            .token,
        CLAUDE_TOKEN
    );
    assert!(f.codex.view().providers.is_empty());
    assert_eq!(config_hashes(&f), before);
    let stored = file_hash(f.data.join("claude/gateway.json"));
    f.claude.import_initial(&f.claude_home).unwrap();
    assert_eq!(f.claude.view().revision, imported.revision);
    assert_eq!(file_hash(f.data.join("claude/gateway.json")), stored);
    edit_client(
        &f.claude,
        &f.claude_home,
        Edit::DeleteProvider {
            id: imported.providers[0].id.clone(),
        },
    );
    let deleted_revision = f.claude.view().revision;
    let deleted_store = file_hash(f.data.join("claude/gateway.json"));
    f.claude.import_initial(&f.claude_home).unwrap();
    assert!(f.claude.view().providers.is_empty());
    assert_eq!(f.claude.view().revision, deleted_revision);
    assert_eq!(config_hashes(&f), before);
    let DualFixture {
        _temp,
        data,
        codex_home,
        claude_home,
        codex,
        claude,
    } = f;
    drop(claude);
    let restarted = codex.companion(data.join("claude")).unwrap();
    restarted.import_initial(&claude_home).unwrap();
    assert!(restarted.view().providers.is_empty());
    assert!(restarted.view().selected.is_none());
    assert_eq!(file_hash(data.join("claude/gateway.json")), deleted_store);
    assert_eq!(file_hash(codex_home.join("config.toml")), before[0]);
    assert_eq!(file_hash(codex_home.join("auth.json")), before[1]);
    assert_eq!(file_hash(claude_home.join("settings.json")), before[2]);
}
