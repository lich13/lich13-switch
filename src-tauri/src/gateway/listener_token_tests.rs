use super::super::{ClientId, Edit, Gateway};
use super::{authorized, route, RouteError};
use hyper::{
    header::{self, HeaderValue},
    HeaderMap, Uri,
};
use std::{fs, net::TcpListener as StdTcpListener, path::Path, sync::mpsc, thread, time::Duration};
use tempfile::TempDir;

const CODEX_TOKEN: &str = "gs_fixture_codex_local_token";
const CLAUDE_TOKEN: &str = "gs_fixture_claude_local_token";
const GROK_TOKEN: &str = "gs_fixture_grok_local_token";
const DUPLICATE_TOKEN: &str = "gs_fixture_duplicate_local_token";
const RELOADED_TOKEN: &str = "gs_fixture_reloaded_local_token";
const STALE_OLD_TOKEN: &str = "gs_fixture_stale_old_local_token";
const CLIENTS: [(ClientId, &str); 3] = [
    (ClientId::Codex, CODEX_TOKEN),
    (ClientId::Claude, CLAUDE_TOKEN),
    (ClientId::Grok, GROK_TOKEN),
];

struct Fixture {
    _temp: TempDir,
    codex: Gateway,
    claude: Gateway,
    _grok: Gateway,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("gateway-data");
        let codex = Gateway::new(data.clone()).unwrap();
        let claude = codex.companion(data.join("claude")).unwrap();
        let grok = codex.grok_companion(data.join("grok")).unwrap();

        set_local_token(&codex, CODEX_TOKEN);
        set_local_token(&claude, CLAUDE_TOKEN);
        set_local_token(&grok, GROK_TOKEN);

        Self {
            _temp: temp,
            codex,
            claude,
            _grok: grok,
        }
    }
}

fn set_local_token(gateway: &Gateway, token: &str) {
    {
        gateway.0.inner.lock().unwrap().store.local_token = token.to_owned();
    }
    gateway.0.registry.publish_token(&gateway.0, token);
}

fn persist_local_token(gateway: &Gateway, token: &str) {
    set_local_token(gateway, token);
    let mut state = gateway.0.inner.lock().unwrap();
    state.revision = state
        .store
        .persist(&gateway.0.data.join("gateway.json"), &state.revision)
        .unwrap();
}

fn seed_codex_home(home: &Path) {
    fs::create_dir_all(home).unwrap();
    fs::write(
        home.join("config.toml"),
        "model_provider = 'custom'\n[model_providers.custom]\nbase_url = 'https://original.fixture.invalid/v1'\nexperimental_bearer_token = 'fixture-original-token'\nwire_api = 'responses'\n",
    )
    .unwrap();
}

fn add_codex_provider(gateway: &Gateway, home: &Path) {
    gateway
        .edit(
            Edit::SaveProvider {
                quota_access_token: None,
                id: None,
                base_url: "https://provider.fixture.invalid/v1".to_owned(),
                token: "fixture-upstream-token".to_owned(),
                name: Some("fixture-provider".to_owned()),
            },
            &gateway.view().revision,
            home,
        )
        .unwrap();
}

fn value(value: &str) -> HeaderValue {
    value.parse().unwrap()
}

fn headers(authorization: Option<&str>, api_key: Option<&str>) -> HeaderMap {
    let mut headers = HeaderMap::new();
    if let Some(authorization) = authorization {
        headers.insert(header::AUTHORIZATION, value(authorization));
    }
    if let Some(api_key) = api_key {
        headers.insert("x-api-key", value(api_key));
    }
    headers
}

fn bearer(token: &str) -> String {
    format!("Bearer {token}")
}

#[test]
fn bearer_scheme_is_case_insensitive_with_one_or_more_ascii_spaces_and_exact_token() {
    for scheme in ["Bearer", "bearer", "BEARER", "bEaReR"] {
        for spaces in [" ", "  ", "   "] {
            let authorization = format!("{scheme}{spaces}{CODEX_TOKEN}");
            assert!(
                authorized(&headers(Some(&authorization), None), CODEX_TOKEN),
                "scheme={scheme:?}, separator={spaces:?}"
            );
        }
    }

    for authorization in [
        format!("Bearer{CODEX_TOKEN}"),
        format!("Bearer\t{CODEX_TOKEN}"),
        format!("Bearer \t{CODEX_TOKEN}"),
        format!("Bearer {CODEX_TOKEN} "),
        format!("Bearer fixture-{CODEX_TOKEN}"),
        format!("Bearer {CODEX_TOKEN}-suffix"),
        format!("Basic {CODEX_TOKEN}"),
    ] {
        assert!(
            !authorized(&headers(Some(&authorization), None), CODEX_TOKEN),
            "authorization={authorization:?}"
        );
    }
}

fn route_with_headers(
    fixture: &Fixture,
    request: &str,
    headers: &HeaderMap,
) -> Result<(ClientId, String), RouteError> {
    let uri: Uri = request.parse().unwrap();
    route(&uri, headers, &fixture.codex.0.registry).map(|(client, uri)| {
        (
            client,
            uri.path_and_query()
                .expect("routed URI should retain its path")
                .as_str()
                .to_owned(),
        )
    })
}

fn route_with_token(
    gateway: &Gateway,
    request: &str,
    token: &str,
) -> Result<(ClientId, String), RouteError> {
    let authorization = bearer(token);
    let request_headers = headers(Some(&authorization), None);
    let uri: Uri = request.parse().unwrap();
    route(&uri, &request_headers, &gateway.0.registry).map(|(client, uri)| {
        (
            client,
            uri.path_and_query()
                .expect("routed URI should retain its path")
                .as_str()
                .to_owned(),
        )
    })
}

fn duplicate_token_gateways(temp: &TempDir) -> (Gateway, Gateway, std::path::PathBuf) {
    let data = temp.path().join("transaction-data");
    let codex = Gateway::new(data.clone()).unwrap();
    let claude = codex.companion(data.join("claude")).unwrap();
    let port_reservation = StdTcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = port_reservation.local_addr().unwrap().port();
    codex
        .set_listener_port(port, &codex.listener_view().revision)
        .unwrap();
    drop(port_reservation);
    set_local_token(&codex, DUPLICATE_TOKEN);
    set_local_token(&claude, DUPLICATE_TOKEN);

    let home = temp.path().join("codex-home");
    seed_codex_home(&home);
    add_codex_provider(&codex, &home);
    (codex, claude, home)
}

#[test]
fn unprefixed_paths_are_selected_by_exact_local_token_and_keep_the_query() {
    let fixture = Fixture::new();
    let requests = [
        (
            "/v1/responses?model=fixture%2Fshared&repeat=one&repeat=two",
            "/v1/responses?model=fixture%2Fshared&repeat=one&repeat=two",
        ),
        (
            "/v1/messages?model=fixture%2Fshared&repeat=one&repeat=two",
            "/v1/messages?model=fixture%2Fshared&repeat=one&repeat=two",
        ),
        (
            "/v1/messages/count_tokens?model=fixture%2Fshared&repeat=one&repeat=two",
            "/v1/messages/count_tokens?model=fixture%2Fshared&repeat=one&repeat=two",
        ),
    ];

    for (request, expected_uri) in requests {
        for (expected_client, token) in CLIENTS {
            let authorization = bearer(token);
            let accepted_forms = [
                ("authorization", headers(Some(&authorization), None)),
                ("x-api-key", headers(None, Some(token))),
                (
                    "matching-dual-headers",
                    headers(Some(&authorization), Some(token)),
                ),
            ];
            for (form, headers) in accepted_forms {
                assert_eq!(
                    route_with_headers(&fixture, request, &headers),
                    Ok((expected_client, expected_uri.to_owned())),
                    "request={request}, token_client={expected_client:?}, form={form}"
                );
            }
        }
    }
}

#[test]
fn legacy_prefixes_accept_only_their_client_token_and_preserve_query() {
    let fixture = Fixture::new();
    let requests = [
        (
            ClientId::Codex,
            "/codex/v1/responses?model=fixture&state=%2f&state=%2F",
            "/v1/responses?model=fixture&state=%2f&state=%2F",
        ),
        (
            ClientId::Claude,
            "/claude/messages/count_tokens?model=fixture&state=%2f&state=%2F",
            "/messages/count_tokens?model=fixture&state=%2f&state=%2F",
        ),
        (
            ClientId::Grok,
            "/grok/v1/responses?model=fixture&state=%2f&state=%2F",
            "/v1/responses?model=fixture&state=%2f&state=%2F",
        ),
    ];

    for (target, request, expected_uri) in requests {
        for (token_client, token) in CLIENTS {
            let authorization = bearer(token);
            let result =
                route_with_headers(&fixture, request, &headers(Some(&authorization), None));
            if token_client == target {
                assert_eq!(
                    result,
                    Ok((target, expected_uri.to_owned())),
                    "request={request}, token_client={token_client:?}"
                );
            } else {
                assert_eq!(
                    result,
                    Err(RouteError::Unauthorized),
                    "request={request}, token_client={token_client:?}"
                );
            }
        }
    }

    let claude_request = "/claude/messages?model=fixture&x=one%2Ftwo&x=three+four";
    assert_eq!(
        route_with_headers(&fixture, claude_request, &headers(None, Some(CLAUDE_TOKEN)),),
        Ok((
            ClientId::Claude,
            "/messages?model=fixture&x=one%2Ftwo&x=three+four".to_owned()
        ))
    );
}

#[test]
fn missing_wrong_duplicate_and_cross_client_credentials_are_unauthorized() {
    let fixture = Fixture::new();
    let request = "/v1/messages/count_tokens?model=fixture";

    assert_eq!(
        route_with_headers(&fixture, request, &HeaderMap::new()),
        Err(RouteError::Unauthorized)
    );
    assert_eq!(
        route_with_headers(
            &fixture,
            request,
            &headers(Some("Bearer gs_fixture_wrong_token"), None),
        ),
        Err(RouteError::Unauthorized)
    );
    assert_eq!(
        route_with_headers(
            &fixture,
            request,
            &headers(None, Some("gs_fixture_wrong_token")),
        ),
        Err(RouteError::Unauthorized)
    );

    let mut duplicate_authorization = headers(Some(&bearer(CODEX_TOKEN)), None);
    duplicate_authorization.append(header::AUTHORIZATION, value(&bearer(CODEX_TOKEN)));
    assert_eq!(
        route_with_headers(&fixture, request, &duplicate_authorization),
        Err(RouteError::Unauthorized),
        "repeated Authorization headers must be rejected even when identical"
    );

    let mut duplicate_api_key = headers(None, Some(CODEX_TOKEN));
    duplicate_api_key.append("x-api-key", value(CODEX_TOKEN));
    assert_eq!(
        route_with_headers(&fixture, request, &duplicate_api_key),
        Err(RouteError::Unauthorized),
        "repeated x-api-key headers must be rejected even when identical"
    );

    for (authorization_client, authorization_token) in CLIENTS {
        for (api_key_client, api_key_token) in CLIENTS {
            if authorization_client == api_key_client {
                continue;
            }
            let authorization = bearer(authorization_token);
            assert_eq!(
                route_with_headers(
                    &fixture,
                    request,
                    &headers(Some(&authorization), Some(api_key_token)),
                ),
                Err(RouteError::Unauthorized),
                "Authorization={authorization_client:?}, x-api-key={api_key_client:?}"
            );
        }
    }
}

#[tokio::test]
async fn route_does_not_wait_for_another_gateways_inner_lock() {
    let fixture = Fixture::new();
    let other = fixture.claude.0.clone();
    let (locked_tx, locked_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let holder = thread::spawn(move || {
        let _guard = other.inner.lock().unwrap();
        locked_tx.send(()).unwrap();
        release_rx.recv().unwrap();
    });
    locked_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("the peer lock holder should start");

    let uri: Uri = "/v1/responses?model=fixture".parse().unwrap();
    let authorization = bearer(CODEX_TOKEN);
    let request_headers = headers(Some(&authorization), None);
    let registry = fixture.codex.0.registry.clone();
    let mut route_task =
        tokio::task::spawn_blocking(move || route(&uri, &request_headers, &registry));
    let timely = tokio::time::timeout(Duration::from_millis(100), &mut route_task).await;

    release_tx.send(()).unwrap();
    holder.join().unwrap();
    let (finished_on_time, result) = match timely {
        Ok(Ok(result)) => (true, result),
        Ok(Err(error)) => panic!("routing task failed: {error}"),
        Err(_) => (false, route_task.await.unwrap()),
    };
    assert!(
        finished_on_time,
        "routing should finish within 100ms while a peer store is locked"
    );
    assert_eq!(
        result,
        Ok((
            ClientId::Codex,
            "/v1/responses?model=fixture".parse().unwrap()
        ))
    );
}

#[test]
fn a_token_shared_by_multiple_clients_is_ambiguous_on_unprefixed_routes() {
    let fixture = Fixture::new();
    set_local_token(&fixture.codex, "gs_fixture_shared_local_token");
    set_local_token(&fixture.claude, "gs_fixture_shared_local_token");

    for request in [
        "/v1/messages?model=fixture",
        "/v1/messages/count_tokens?model=fixture",
    ] {
        for credentials in [
            headers(Some(&bearer("gs_fixture_shared_local_token")), None),
            headers(None, Some("gs_fixture_shared_local_token")),
        ] {
            assert_eq!(
                route_with_headers(&fixture, request, &credentials),
                Err(RouteError::Unauthorized),
                "ambiguous token must not pick a client by path, model, or iteration order: {request}"
            );
        }
    }
}

#[test]
fn persisted_store_token_is_registered_after_gateway_reload() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("reload-data");
    let first = Gateway::new(data.clone()).unwrap();
    let old_token = first.0.inner.lock().unwrap().store.local_token.clone();
    let store_path = data.join("gateway.json");

    persist_local_token(&first, RELOADED_TOKEN);
    let (stored, _) = super::super::model::Store::load(&store_path).unwrap();
    assert_eq!(stored.local_token, RELOADED_TOKEN);
    drop(first);

    let reloaded = Gateway::new(data).unwrap();
    assert_eq!(
        route_with_token(&reloaded, "/v1/responses", RELOADED_TOKEN),
        Ok((ClientId::Codex, "/v1/responses".to_owned()))
    );
    assert_eq!(
        route_with_token(&reloaded, "/v1/responses", &old_token),
        Err(RouteError::Unauthorized)
    );
}

#[test]
fn unregistering_an_old_shared_does_not_remove_its_reregistered_replacement() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("lifecycle-data");
    let codex = Gateway::new(data.clone()).unwrap();
    let claude_data = data.join("claude");
    let old = codex.companion(claude_data.clone()).unwrap();
    persist_local_token(&old, CLAUDE_TOKEN);
    let registry = codex.0.registry.clone();
    let before_reregister = registry.tokens_snapshot();
    let old_weak = before_reregister
        .iter()
        .find(|entry| entry.client == ClientId::Claude)
        .expect("first Claude registration should be present")
        .shared
        .clone();

    let replacement = codex.companion(claude_data).unwrap();
    set_local_token(&old, STALE_OLD_TOKEN);
    assert_eq!(
        route_with_token(&codex, "/claude/messages", CLAUDE_TOKEN),
        Ok((ClientId::Claude, "/messages".to_owned())),
        "a stale instance must not replace its replacement's published token"
    );
    assert_eq!(
        route_with_token(&codex, "/claude/messages", STALE_OLD_TOKEN),
        Err(RouteError::Unauthorized)
    );
    drop(old);

    assert!(old_weak.upgrade().is_none(), "dropped Shared should expire");
    assert_eq!(
        registry
            .gateway(ClientId::Claude)
            .map(|gateway| gateway.client_id()),
        Some(ClientId::Claude),
        "unregistering the old instance must preserve its replacement"
    );
    assert_eq!(
        route_with_token(&codex, "/claude/messages", CLAUDE_TOKEN),
        Ok((ClientId::Claude, "/messages".to_owned()))
    );

    drop(replacement);
    assert!(registry.gateway(ClientId::Claude).is_none());
    assert_eq!(
        route_with_token(&codex, "/claude/messages", CLAUDE_TOKEN),
        Err(RouteError::NotFound)
    );
    assert_eq!(
        route_with_token(&codex, "/v1/messages", CLAUDE_TOKEN),
        Err(RouteError::Unauthorized),
        "an empty peer snapshot should fail closed on unprefixed routes"
    );
}

#[tokio::test]
async fn committed_token_rotation_rejects_old_token_and_routes_with_reloaded_store_token() {
    let temp = tempfile::tempdir().unwrap();
    let (codex, _claude, home) = duplicate_token_gateways(&temp);
    assert_eq!(
        route_with_token(&codex, "/codex/v1/responses", DUPLICATE_TOKEN),
        Ok((ClientId::Codex, "/v1/responses".to_owned()))
    );

    let expected = codex.view().revision;
    codex.start(&expected, &home).await.unwrap();

    let store_path = codex.0.data.join("gateway.json");
    let (stored, _) = super::super::model::Store::load(&store_path).unwrap();
    assert_ne!(stored.local_token, DUPLICATE_TOKEN);
    assert_eq!(
        route_with_token(&codex, "/codex/v1/responses", DUPLICATE_TOKEN),
        Err(RouteError::Unauthorized)
    );
    assert_eq!(
        route_with_token(&codex, "/codex/v1/responses", &stored.local_token),
        Ok((ClientId::Codex, "/v1/responses".to_owned()))
    );

    codex.stop().await.unwrap();
}

#[tokio::test]
async fn failed_token_store_transaction_keeps_the_published_token_unchanged() {
    let temp = tempfile::tempdir().unwrap();
    let (codex, _claude, home) = duplicate_token_gateways(&temp);
    let store_path = codex.0.data.join("gateway.json");
    let saved_store = fs::read(&store_path).unwrap();
    let expected = codex.view().revision;

    let error = {
        let _fault = crate::storage::faults::inject(crate::storage::faults::Point::Replace);
        match codex.start(&expected, &home).await {
            Ok(_) => panic!("failed store replacement should abort the token transaction"),
            Err(error) => error,
        }
    };

    assert_eq!(error.code, "IO");
    assert_eq!(fs::read(&store_path).unwrap(), saved_store);
    let (stored, _) = super::super::model::Store::load(&store_path).unwrap();
    assert_eq!(stored.local_token, DUPLICATE_TOKEN);
    assert_eq!(
        route_with_token(&codex, "/codex/v1/responses", DUPLICATE_TOKEN),
        Ok((ClientId::Codex, "/v1/responses".to_owned()))
    );
    assert!(codex.listener_view().running_clients.is_empty());
}
