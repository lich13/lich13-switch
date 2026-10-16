use super::super::{ClientId, Gateway};
use super::{authorized, route, RouteError};
use hyper::{
    header::{self, HeaderValue},
    HeaderMap, Uri,
};
use tempfile::TempDir;

const CODEX_TOKEN: &str = "gs_fixture_codex_local_token";
const CLAUDE_TOKEN: &str = "gs_fixture_claude_local_token";
const GROK_TOKEN: &str = "gs_fixture_grok_local_token";
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
    gateway.0.inner.lock().unwrap().store.local_token = token.to_owned();
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
