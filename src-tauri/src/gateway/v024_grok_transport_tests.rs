//! Grok gateway transport integration tests use synthetic credentials and loopback upstreams.
use super::{
    connector::Connector,
    grok_config::{self, Protocol},
    model::Edit,
    replay::{self, WireBody},
    ClientId, Gateway, HttpClient,
};
use crate::usage::{
    model::{Filter, Record},
    Service,
};
use http_body_util::BodyExt;
use hyper::{body::Incoming, Request, Response, StatusCode};
use hyper_util::{client::legacy::Client, rt::TokioExecutor, rt::TokioIo};
use std::{
    convert::Infallible,
    future::Future,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::task::JoinHandle;

const WAIT: Duration = Duration::from_secs(5);
const ORIGINAL_TOKEN: &str = "fixture-original-grok-token";
const NEW_RESPONSES_TOKEN: &str = "fixture-new-responses-token";
const CHAT_TOKEN: &str = "fixture-chat-token";
const MESSAGES_TOKEN: &str = "fixture-messages-token";
const RESPONSES_RESPONSE_BODY: &[u8] =
    br#" { "id":"fixture-responses-response", "object":"response",
  "model":"fixture-grok-responses-upstream-model", "status":"completed", "output":[],
  "usage":{"input_tokens":100,"output_tokens":30,
    "input_tokens_details":{"cached_tokens":40},
    "output_tokens_details":{"reasoning_tokens":8}} } "#;
const CHAT_RESPONSE_BODY: &[u8] = br#" { "id":"fixture-chat-response", "object":"chat.completion",
  "model":"fixture-grok-chat-upstream-model",
  "choices":[{"index":0,"message":{"role":"assistant","content":"fixture-chat-output"},"finish_reason":"stop"}],
  "usage":{"prompt_tokens":100,"completion_tokens":30,
    "prompt_tokens_details":{"cached_tokens":40},
    "completion_tokens_details":{"reasoning_tokens":8}} } "#;
const MESSAGES_RESPONSE_BODY: &[u8] = br#" { "id":"fixture-messages-response", "type":"message",
  "model":"fixture-grok-messages-upstream-model", "role":"assistant", "content":[],
  "stop_reason":"end_turn",
  "usage":{"input_tokens":60,"output_tokens":30,"cache_read_input_tokens":40,
    "cache_creation_input_tokens":10} } "#;
const RESPONSES_REQUEST: &[u8] = br#" { "model" : "fixture-grok-responses-model", "input" : [{"role":"user","content":"fixture-responses"}], "future" : { "integer" : 9007199254740993 } } "#;
const CHAT_REQUEST: &[u8] = br#" { "model" : "fixture-grok-chat-model", "messages" : [{"role":"user","content":"fixture-chat"}], "future" : true } "#;
const MESSAGES_REQUEST: &[u8] = br#" { "model" : "fixture-grok-messages-model", "max_tokens" : 16, "messages" : [{"role":"user","content":"fixture-messages"}], "future" : true } "#;

#[derive(Clone, Debug)]
struct ObservedRequest {
    path_and_query: String,
    body: Vec<u8>,
    authorization: Option<String>,
    api_key: Option<String>,
    anthropic_version: Option<String>,
    model_private: Option<String>,
    provider_private: Option<String>,
}

#[derive(Clone, Default)]
struct TaskScope(Arc<Mutex<(bool, Vec<JoinHandle<()>>)>>);

impl TaskScope {
    fn spawn(&self, future: impl Future<Output = ()> + Send + 'static) {
        let mut state = self.0.lock().unwrap();
        if !state.0 {
            state.1.push(tokio::spawn(future));
        }
    }

    fn close(&self) {
        let mut state = self.0.lock().unwrap();
        state.0 = true;
        for task in state.1.drain(..) {
            task.abort();
        }
    }
}

struct LoopbackServer {
    port: u16,
    tasks: TaskScope,
}

impl LoopbackServer {
    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }
}

impl Drop for LoopbackServer {
    fn drop(&mut self) {
        self.tasks.close();
    }
}

async fn server<F, Fut>(handler: F) -> LoopbackServer
where
    F: Fn(Request<Incoming>) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Response<WireBody>> + Send + 'static,
{
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let tasks = TaskScope::default();
    let accept_tasks = tasks.clone();
    let handler = Arc::new(handler);
    tasks.spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let handler = handler.clone();
            accept_tasks.spawn(async move {
                let service = hyper::service::service_fn(move |request| {
                    let handler = handler.clone();
                    async move { Ok::<_, Infallible>(handler(request).await) }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });
    LoopbackServer { port, tasks }
}

async fn capturing_server(
    seen: Arc<Mutex<Vec<ObservedRequest>>>,
    response_body: &'static [u8],
) -> LoopbackServer {
    server(move |request| {
        let seen = seen.clone();
        let response_body = response_body.to_vec();
        async move {
            let (parts, body) = request.into_parts();
            let body = body.collect().await.unwrap().to_bytes();
            let header = |name: &str| {
                parts
                    .headers
                    .get(name)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned)
            };
            seen.lock().unwrap().push(ObservedRequest {
                path_and_query: parts
                    .uri
                    .path_and_query()
                    .map(|value| value.as_str().to_owned())
                    .unwrap_or_default(),
                body: body.to_vec(),
                authorization: header("authorization"),
                api_key: header("x-api-key"),
                anthropic_version: header("anthropic-version"),
                model_private: header("x-model-private"),
                provider_private: header("x-provider-private"),
            });
            Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "application/json")
                .body(replay::full(response_body))
                .unwrap()
        }
    })
    .await
}

struct Fixture {
    gateway: Gateway,
    usage: Service,
    home: PathBuf,
    _dir: tempfile::TempDir,
}

impl Fixture {
    async fn new(upstream: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("grok-home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("config.toml"), native_config(upstream)).unwrap();

        let primary = Gateway::new(dir.path().join("primary-data")).unwrap();
        let gateway = primary
            .grok_companion(dir.path().join("grok-gateway-data"))
            .unwrap();
        assert_eq!(gateway.client_id(), ClientId::Grok);
        let usage = Service::new(&dir.path().join("usage-data"));
        assert!(usage.state().error.is_none());
        gateway.set_usage(usage.clone());
        let reservation = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = reservation.local_addr().unwrap().port();
        let mut settings = gateway.view().settings;
        settings.port = port;
        settings.max_retries = 1;
        settings.connect_seconds = 2;
        settings.first_byte_seconds = 5;
        settings.idle_seconds = 5;
        settings.total_seconds = 15;
        settings.queue_seconds = 2;
        gateway
            .edit(Edit::Settings { settings }, &gateway.view().revision, &home)
            .unwrap();
        drop(reservation);
        gateway.import_initial(&home).unwrap();

        Self {
            gateway,
            usage,
            home,
            _dir: dir,
        }
    }

    fn edit(&self, edit: Edit) -> super::View {
        self.gateway
            .edit(edit, &self.gateway.view().revision, &self.home)
            .unwrap()
    }

    fn config_path(&self) -> PathBuf {
        self.home.join("config.toml")
    }

    async fn start(&self) {
        self.gateway
            .start(&self.gateway.view().revision, &self.home)
            .await
            .unwrap();
    }

    async fn stop(&self) {
        tokio::time::timeout(WAIT, self.gateway.stop())
            .await
            .unwrap()
            .unwrap();
    }
}

fn native_config(upstream: &str) -> String {
    format!(
        r#"
[models]
default = "fixture-selected"
extra_headers = {{ "x-model-private" = "fixture-model-private" }}

[model."fixture-selected"]
model_provider = "fixture-provider"
model = "fixture-grok-model"

[model_providers."fixture-provider"]
base_url = {}
api_key = "{ORIGINAL_TOKEN}"
api_backend = "responses"
extra_headers = {{ "authorization" = "Bearer fixture-stale-auth", "x-api-key" = "fixture-stale-api-key", "x-provider-private" = "fixture-provider-private" }}
query_params = {{ "profile" = "fixture-profile-query", "replace" = "fixture-profile-value" }}
"#,
        serde_json::to_string(upstream).unwrap()
    )
}

fn add_provider(fixture: &Fixture, base_url: String, token: &str, protocol: Protocol) {
    fixture.edit(Edit::SaveGrokProvider {
        id: None,
        base_url,
        token: token.to_owned(),
        name: Some(format!("fixture-{}", protocol.name())),
        protocol,
    });
}

async fn gateway_request(gateway: &Gateway, path: &str, body: &[u8]) -> Response<Incoming> {
    let client: HttpClient =
        Client::builder(TokioExecutor::new()).build(Connector::new(Duration::from_secs(2), 0));
    let (token, port) = {
        let state = gateway.0.inner.lock().unwrap();
        (state.store.local_token.clone(), state.store.settings.port)
    };
    let request = Request::builder()
        .method("POST")
        .uri(format!("http://127.0.0.1:{port}{path}"))
        .header("authorization", format!("Bearer {token}"))
        .header("x-api-key", "fixture-downstream-api-key")
        .header("content-type", "application/json")
        .body(replay::full(body.to_vec()))
        .unwrap();
    tokio::time::timeout(WAIT, client.request(request))
        .await
        .unwrap()
        .unwrap()
}

async fn response_bytes(response: Response<Incoming>) -> Vec<u8> {
    tokio::time::timeout(WAIT, response.into_body().collect())
        .await
        .unwrap()
        .unwrap()
        .to_bytes()
        .to_vec()
}

fn usage_records(service: &Service) -> Vec<Record> {
    service
        .query(|store| {
            let page = store.logs(&Filter::default())?;
            assert_eq!(page.total, page.rows.len() as u64);
            Ok(page.rows)
        })
        .unwrap()
}

async fn recorded(service: &Service, count: usize) -> Vec<Record> {
    let mut events = service.subscribe();
    tokio::time::timeout(WAIT, async {
        loop {
            let rows = usage_records(service);
            if rows.len() >= count {
                assert_eq!(rows.len(), count);
                assert!(service.state().error.is_none());
                return rows;
            }
            events.recv().await.expect("fixture usage writer stopped");
        }
    })
    .await
    .expect("fixture usage record was not written")
}

fn record_for_model<'a>(records: &'a [Record], model: &str) -> &'a Record {
    records
        .iter()
        .find(|record| {
            record
                .final_attempt()
                .and_then(|attempt| attempt.requested_model.as_deref())
                == Some(model)
        })
        .unwrap_or_else(|| panic!("fixture usage record for {model} was not found"))
}

struct ExpectedUsage<'a> {
    requested_model: &'a str,
    response_model: &'a str,
    input: u64,
    inclusive_input: Option<u64>,
    cache_read: Option<u64>,
    cache_write: Option<u64>,
    output: u64,
}

fn assert_usage_record(record: &Record, expected: ExpectedUsage<'_>) {
    assert_eq!(record.client, "grok");
    assert_eq!(record.source, "proxy");
    assert!(record.completed);
    assert_eq!(record.attempts.len(), 1);
    let attempt = record.final_attempt().unwrap();
    assert_eq!(attempt.status, Some(200));
    assert_eq!(attempt.outcome, "success");
    assert_eq!(attempt.transport, "http");
    assert_eq!(
        attempt.requested_model.as_deref(),
        Some(expected.requested_model)
    );
    assert_eq!(
        attempt.response_model.as_deref(),
        Some(expected.response_model)
    );
    assert_eq!(attempt.inclusive_input_tokens, expected.inclusive_input);
    assert_eq!(attempt.tokens.input, Some(expected.input));
    assert_eq!(attempt.tokens.cache_read, expected.cache_read);
    assert_eq!(attempt.tokens.cache_write, expected.cache_write);
    assert_eq!(attempt.tokens.output, Some(expected.output));
}

fn grok_connection(gateway: &Gateway, provider_id: &str) -> grok_config::Connection {
    gateway
        .0
        .inner
        .lock()
        .unwrap()
        .store
        .grok_connections
        .get(provider_id)
        .cloned()
        .unwrap()
}

fn requests(seen: &Arc<Mutex<Vec<ObservedRequest>>>) -> Vec<ObservedRequest> {
    seen.lock().unwrap().clone()
}

fn query_pairs(uri: &str) -> std::collections::BTreeMap<String, String> {
    url::Url::parse(&format!("http://fixture.invalid{uri}"))
        .unwrap()
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect()
}

fn assert_body_and_model(request: &ObservedRequest, original: &[u8], model: &str) {
    assert_eq!(request.body.as_slice(), original);
    let value: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
    assert_eq!(
        value.get("model").and_then(serde_json::Value::as_str),
        Some(model)
    );
}

#[tokio::test]
async fn grok_gateway_routes_each_native_protocol_with_private_connection_data() {
    let responses_seen = Arc::new(Mutex::new(Vec::new()));
    let chat_seen = Arc::new(Mutex::new(Vec::new()));
    let messages_seen = Arc::new(Mutex::new(Vec::new()));
    let responses_upstream =
        capturing_server(responses_seen.clone(), RESPONSES_RESPONSE_BODY).await;
    let chat_upstream = capturing_server(chat_seen.clone(), CHAT_RESPONSE_BODY).await;
    let messages_upstream = capturing_server(messages_seen.clone(), MESSAGES_RESPONSE_BODY).await;

    let fixture = Fixture::new(&responses_upstream.url("/v1")).await;
    let original_config = std::fs::read(fixture.config_path()).unwrap();
    let responses_id = fixture.gateway.view().providers[0].id.clone();
    fixture.edit(Edit::SaveGrokProvider {
        id: Some(responses_id),
        base_url: responses_upstream.url("/v1"),
        token: NEW_RESPONSES_TOKEN.into(),
        name: None,
        protocol: Protocol::Responses,
    });
    add_provider(
        &fixture,
        chat_upstream.url("/v1"),
        CHAT_TOKEN,
        Protocol::ChatCompletions,
    );
    add_provider(
        &fixture,
        messages_upstream.url("/v1"),
        MESSAGES_TOKEN,
        Protocol::Messages,
    );
    fixture.edit(Edit::Mode {
        mode: "auto".into(),
    });
    fixture.start().await;

    let response = gateway_request(
        &fixture.gateway,
        "/v1/responses?replace=fixture-client-value&keep=fixture-client-query",
        RESPONSES_REQUEST,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response_bytes(response).await.as_slice(),
        RESPONSES_RESPONSE_BODY
    );
    let rows = recorded(&fixture.usage, 1).await;
    assert_usage_record(
        record_for_model(&rows, "fixture-grok-responses-model"),
        ExpectedUsage {
            requested_model: "fixture-grok-responses-model",
            response_model: "fixture-grok-responses-upstream-model",
            input: 60,
            inclusive_input: Some(100),
            cache_read: Some(40),
            cache_write: None,
            output: 30,
        },
    );
    let responses = requests(&responses_seen);
    assert_eq!(responses.len(), 1);
    let request = &responses[0];
    assert_eq!(
        request.path_and_query.split('?').next(),
        Some("/v1/responses")
    );
    assert_body_and_model(request, RESPONSES_REQUEST, "fixture-grok-responses-model");
    assert_eq!(
        request.authorization.as_deref(),
        Some("Bearer fixture-new-responses-token")
    );
    assert_eq!(request.api_key, None);
    assert_eq!(
        request.model_private.as_deref(),
        Some("fixture-model-private")
    );
    assert_eq!(
        request.provider_private.as_deref(),
        Some("fixture-provider-private")
    );
    let query = query_pairs(&request.path_and_query);
    assert_eq!(query.len(), 3);
    assert_eq!(
        query.get("keep").map(String::as_str),
        Some("fixture-client-query")
    );
    assert_eq!(
        query.get("profile").map(String::as_str),
        Some("fixture-profile-query")
    );
    assert_eq!(
        query.get("replace").map(String::as_str),
        Some("fixture-profile-value")
    );
    assert!(requests(&chat_seen).is_empty());
    assert!(requests(&messages_seen).is_empty());

    let response = gateway_request(
        &fixture.gateway,
        "/v1/chat/completions?client=fixture-chat-query",
        CHAT_REQUEST,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response_bytes(response).await.as_slice(),
        CHAT_RESPONSE_BODY
    );
    let rows = recorded(&fixture.usage, 2).await;
    assert_usage_record(
        record_for_model(&rows, "fixture-grok-chat-model"),
        ExpectedUsage {
            requested_model: "fixture-grok-chat-model",
            response_model: "fixture-grok-chat-upstream-model",
            input: 60,
            inclusive_input: Some(100),
            cache_read: Some(40),
            cache_write: None,
            output: 30,
        },
    );
    let chat = requests(&chat_seen);
    assert_eq!(chat.len(), 1);
    assert_eq!(
        chat[0].path_and_query,
        "/v1/chat/completions?client=fixture-chat-query"
    );
    assert_body_and_model(&chat[0], CHAT_REQUEST, "fixture-grok-chat-model");
    assert_eq!(
        chat[0].authorization.as_deref(),
        Some("Bearer fixture-chat-token")
    );
    assert_eq!(chat[0].api_key, None);
    assert_eq!(chat[0].anthropic_version, None);
    assert_eq!(requests(&responses_seen).len(), 1);
    assert!(requests(&messages_seen).is_empty());

    let response = gateway_request(
        &fixture.gateway,
        "/v1/messages?client=fixture-messages-query",
        MESSAGES_REQUEST,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response_bytes(response).await.as_slice(),
        MESSAGES_RESPONSE_BODY
    );
    let rows = recorded(&fixture.usage, 3).await;
    assert_usage_record(
        record_for_model(&rows, "fixture-grok-messages-model"),
        ExpectedUsage {
            requested_model: "fixture-grok-messages-model",
            response_model: "fixture-grok-messages-upstream-model",
            input: 60,
            inclusive_input: None,
            cache_read: Some(40),
            cache_write: Some(10),
            output: 30,
        },
    );
    let messages = requests(&messages_seen);
    assert_eq!(messages.len(), 1);
    assert_eq!(
        messages[0].path_and_query,
        "/v1/messages?client=fixture-messages-query"
    );
    assert_body_and_model(
        &messages[0],
        MESSAGES_REQUEST,
        "fixture-grok-messages-model",
    );
    assert_eq!(messages[0].authorization, None);
    assert_eq!(
        messages[0].api_key.as_deref(),
        Some("fixture-messages-token")
    );
    assert_eq!(messages[0].anthropic_version.as_deref(), Some("2023-06-01"));
    assert_eq!(requests(&responses_seen).len(), 1);
    assert_eq!(requests(&chat_seen).len(), 1);

    fixture.stop().await;
    assert!(!fixture.gateway.view().running);
    assert_eq!(
        std::fs::read(fixture.config_path()).unwrap(),
        original_config
    );
}

#[tokio::test]
async fn grok_native_selection_and_gateway_stop_restore_config_without_overwriting_conflicts() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let upstream = capturing_server(seen, RESPONSES_RESPONSE_BODY).await;
    let fixture = Fixture::new(&upstream.url("/v1")).await;
    let config_path = fixture.config_path();
    let original_config = std::fs::read(&config_path).unwrap();
    let provider_id = fixture.gateway.view().providers[0].id.clone();

    fixture.edit(Edit::Select {
        id: provider_id.clone(),
    });
    assert!(fixture.gateway.view().grok_native_available);
    let direct_config = std::fs::read_to_string(&config_path).unwrap();
    assert_ne!(direct_config.as_bytes(), original_config.as_slice());
    let direct_doc = direct_config.parse::<toml_edit::DocumentMut>().unwrap();
    assert_eq!(
        direct_doc
            .get("models")
            .and_then(|models| models.get("default"))
            .and_then(toml_edit::Item::as_str),
        Some(grok_config::MANAGED)
    );

    let native = fixture.edit(Edit::GrokNative);
    assert!(!native.grok_native_available);
    assert_eq!(std::fs::read(&config_path).unwrap(), original_config);

    fixture.start().await;
    let active_config = std::fs::read_to_string(&config_path).unwrap();
    let active_doc = active_config.parse::<toml_edit::DocumentMut>().unwrap();
    assert_eq!(
        active_doc
            .get("models")
            .and_then(|models| models.get("default"))
            .and_then(toml_edit::Item::as_str),
        Some(grok_config::MANAGED)
    );

    let mut external_doc = active_config.parse::<toml_edit::DocumentMut>().unwrap();
    external_doc["model"][grok_config::MANAGED]["api_key"] =
        toml_edit::value("fixture-external-grok-token");
    let external_config = external_doc.to_string();
    std::fs::write(&config_path, &external_config).unwrap();
    let stop = tokio::time::timeout(WAIT, fixture.gateway.stop())
        .await
        .unwrap();
    let conflict = match stop {
        Ok(_) => panic!("stop must preserve an externally changed managed credential"),
        Err(error) => error,
    };
    assert_eq!(conflict.code, "CONFLICT");
    assert_eq!(
        std::fs::read_to_string(&config_path).unwrap(),
        external_config
    );
    assert!(fixture.gateway.view().running);

    std::fs::write(&config_path, &active_config).unwrap();
    fixture.stop().await;
    assert_eq!(std::fs::read(&config_path).unwrap(), original_config);
    assert!(!fixture.gateway.view().running);
}

#[tokio::test]
async fn grok_manual_messages_connection_survives_import_initial_and_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("grok-home");
    std::fs::create_dir_all(&home).unwrap();
    let official_config = native_config("https://fixture-native.example.invalid/v1");
    let official_connection = grok_config::profile(&official_config).unwrap().connection;
    assert_eq!(official_connection.protocol, Protocol::Responses);
    assert!(!official_connection.headers.is_empty());
    assert!(!official_connection.query.is_empty());
    std::fs::write(home.join("config.toml"), official_config).unwrap();

    let primary_data = dir.path().join("primary-data");
    let grok_data = dir.path().join("grok-gateway-data");
    let primary = Gateway::new(primary_data.clone()).unwrap();
    let gateway = primary.grok_companion(grok_data.clone()).unwrap();
    assert!(!gateway.0.inner.lock().unwrap().store.initialized);
    gateway
        .edit(
            Edit::SaveGrokProvider {
                id: None,
                base_url: "https://fixture-manual.example.invalid/v1".into(),
                token: "fixture-manual-messages-token".into(),
                name: Some("fixture-manual-messages".into()),
                protocol: Protocol::Messages,
            },
            &gateway.view().revision,
            &home,
        )
        .unwrap();
    let provider_id = gateway.view().providers[0].id.clone();
    let manual_connection = grok_connection(&gateway, &provider_id);
    assert_eq!(manual_connection.protocol, Protocol::Messages);
    assert!(manual_connection.headers.is_empty());
    assert!(manual_connection.query.is_empty());

    gateway.import_initial(&home).unwrap();
    assert_eq!(gateway.view().providers.len(), 1);
    let imported_connection = grok_connection(&gateway, &provider_id);
    assert_eq!(imported_connection.protocol, Protocol::Messages);
    assert!(imported_connection.headers.is_empty());
    assert!(imported_connection.query.is_empty());
    drop(gateway);
    drop(primary);

    let reopened_primary = Gateway::new(primary_data).unwrap();
    let reopened = reopened_primary.grok_companion(grok_data).unwrap();
    reopened.import_initial(&home).unwrap();
    assert_eq!(reopened.view().providers.len(), 1);
    let reopened_provider_id = reopened.view().providers[0].id.clone();
    assert_eq!(reopened_provider_id, provider_id);
    let reopened_connection = grok_connection(&reopened, &reopened_provider_id);
    assert_eq!(reopened_connection.protocol, Protocol::Messages);
    assert!(reopened_connection.headers.is_empty());
    assert!(reopened_connection.query.is_empty());
}
