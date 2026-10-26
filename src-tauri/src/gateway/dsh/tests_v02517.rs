use super::*;
use crate::gateway::{grok_protocol_tests::server, model::Store, replay, Edit};
use http_body_util::BodyExt;
use hyper::Response;
use serde_json::{json, Value};
use std::{
    fs,
    sync::{Arc, Mutex},
    time::Duration,
};

#[test]
fn discovers_all_profiles_deduplicates_connections_and_preserves_manual_mapping() {
    let (home, _parent, gateway) = super::tests_v02516::fixture();
    let desktop = "- id: llm-pi-ai\n  config:\n    providers:\n      first: {displayName: Desktop, api: openai-completions, baseURL: 'https://one.example.invalid/v1', apiKeyEnv: FIXTURE_KEY, models: [{id: DeepSeek-V4.1-Flash}, {id: DeepSeek-V4-Pro}]}\n      second: {api: openai-completions, baseURL: 'https://two.example.invalid/v1', apiKey: fixture-two, models: [{id: deepseek-flash}]}\n- id: agent-default-model\n  config: {provider: second, model: deepseek-flash}\n";
    fs::write(home.path().join("profiles/desktop").join(PATCH), desktop).unwrap();
    fs::write(
        home.path().join("profiles/headless").join(PATCH),
        desktop.replace("displayName: Desktop", "displayName: Headless"),
    )
    .unwrap();
    fs::write(home.path().join("profiles/web").join(PATCH), "- id: llm-pi-ai\n  config:\n    providers:\n      third: {api: openai-completions, baseURL: 'https://three.example.invalid/v1', apiKey: fixture-three, models: [{id: custom-pro}]}\n").unwrap();
    let preview = gateway
        .preview_dsh(home.path(), Some("missing"), Protocol::Responses)
        .unwrap();
    assert_eq!(preview.profile.as_deref(), Some("desktop"));
    assert_eq!(preview.protocol, Protocol::ChatCompletions);
    assert_eq!(preview.models, [models::FLASH, models::PRO]);
    assert_eq!(preview.providers.len(), 5);
    assert!(!serde_json::to_string(&preview)
        .unwrap()
        .contains("fixture-provider-secret"));
    let mut store = Store::default();
    import_selection(
        home.path(),
        &super::tests_v02516::options(),
        &mut store,
        43721,
    )
    .unwrap();
    assert_eq!(store.providers.len(), 3);
    assert_eq!(store.providers[0].name, "Desktop");
    let id = store.providers[0].id.clone();
    assert_eq!(
        store.dsh_models[&id].resolve(models::PRO),
        Some("DeepSeek-V4-Pro")
    );
    store
        .edit(
            Edit::DshModelsProvider {
                id: id.clone(),
                mapping: ModelMapping(BTreeMap::from([
                    (models::FLASH.into(), Some("manual-flash".into())),
                    (models::PRO.into(), None),
                ])),
            },
            false,
        )
        .unwrap();
    store.providers[0].name = "Manual name".into();
    let order = store
        .providers
        .iter()
        .map(|p| p.id.clone())
        .collect::<Vec<_>>();
    import_selection(
        home.path(),
        &super::tests_v02516::options(),
        &mut store,
        43721,
    )
    .unwrap();
    assert_eq!(
        store
            .providers
            .iter()
            .map(|p| p.id.clone())
            .collect::<Vec<_>>(),
        order
    );
    assert_eq!(store.providers[0].name, "Manual name");
    assert_eq!(
        store.dsh_models[&id].resolve(models::FLASH),
        Some("manual-flash")
    );
    assert_eq!(store.dsh_models[&id].resolve(models::PRO), None);
    assert!(store.dsh_models[&store.providers[2].id]
        .allowed()
        .is_empty());
}

#[test]
fn incompatible_protocol_is_explained_and_ambiguous_mapping_is_left_empty() {
    let (home, _parent, gateway) = super::tests_v02516::fixture();
    let path = home.path().join("profiles/desktop").join(PATCH);
    let raw = fs::read_to_string(&path)
        .unwrap()
        .replace("openai-completions", "openai-responses");
    fs::write(&path, raw).unwrap();
    let preview = gateway
        .preview_dsh(home.path(), None, Protocol::ChatCompletions)
        .unwrap();
    assert!(preview.providers[0]
        .error
        .as_ref()
        .unwrap()
        .contains("Chat Completions"));
    assert!(import_selection(
        home.path(),
        &super::tests_v02516::options(),
        &mut Store::default(),
        43721
    )
    .is_err());
    assert!(Store::default()
        .edit(
            Edit::SaveDshProvider {
                quota_access_token: None,
                id: None,
                name: None,
                base_url: "https://example.invalid/v1".into(),
                token: "fixture".into(),
                protocol: Protocol::Responses
            },
            false
        )
        .is_err());
    let mapping = ModelMapping::infer(&[
        "DeepSeek-V4.1-Flash".into(),
        "deepseek-flash".into(),
        "DeepSeek V4 Pro".into(),
    ]);
    assert_eq!(mapping.resolve(models::FLASH), None);
    assert_eq!(mapping.resolve(models::PRO), Some("DeepSeek V4 Pro"));
}

struct Fixture {
    home: tempfile::TempDir,
    gateway: Gateway,
    usage: crate::usage::Service,
}
impl Fixture {
    async fn new(bases: &[String], manual: bool) -> Self {
        let home = super::tests::fixture();
        let data = home.path().join("data");
        storage::private_dir(&data).unwrap();
        let mut store = Store {
            initialized: true,
            mode: if manual { "manual" } else { "auto" }.into(),
            ..Default::default()
        };
        store.settings.max_retries = 2;
        store.settings.first_byte_seconds = 1;
        store.settings.idle_seconds = 1;
        store.settings.queue_seconds = 1;
        store.settings.failure_threshold = 1;
        for (index, base) in bases.iter().enumerate() {
            store
                .edit(
                    Edit::SaveDshProvider {
                        quota_access_token: None,
                        id: None,
                        name: Some(format!("Fixture {index}")),
                        base_url: base.clone(),
                        token: "fixture-upstream".into(),
                        protocol: Protocol::ChatCompletions,
                    },
                    false,
                )
                .unwrap();
            let p = store.providers.last_mut().unwrap();
            p.queued = true;
            let mapping = ModelMapping(BTreeMap::from([
                (models::FLASH.into(), Some(format!("vendor-{index}-flash"))),
                (models::PRO.into(), Some(format!("vendor-{index}-pro"))),
            ]));
            p.allowed_models = Some(mapping.allowed());
            store.dsh_models.insert(p.id.clone(), mapping);
        }
        store.selected = Some(store.providers[0].id.clone());
        store
            .persist(&data.join("gateway.json"), "missing")
            .unwrap();
        let gateway = Gateway::new_client(data, ClientId::Dsh, None).unwrap();
        let usage = crate::usage::Service::new(&home.path().join("usage"));
        gateway.set_usage(usage.clone());
        let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = socket.local_addr().unwrap().port();
        drop(socket);
        gateway
            .set_listener_port(port, &gateway.try_listener_view().unwrap().revision)
            .unwrap();
        gateway
            .attach_dsh(
                home.path(),
                Options {
                    profile: "default".into(),
                    protocol: Protocol::ChatCompletions,
                    model: Some(models::FLASH.into()),
                    make_default: true,
                },
                &gateway.try_view().unwrap().revision,
                None,
            )
            .await
            .unwrap();
        Self {
            home,
            gateway,
            usage,
        }
    }
    async fn request(&self, model: &str, stream: bool) -> reqwest::Response {
        let token = self
            .gateway
            .0
            .inner
            .lock()
            .unwrap()
            .store
            .local_token
            .clone();
        reqwest::Client::builder().no_proxy().timeout(Duration::from_secs(8)).build().unwrap()
            .post(format!("http://127.0.0.1:{}/v1/chat/completions", self.gateway.listener_view().port))
            .bearer_auth(token).json(&json!({"model": model, "stream": stream, "messages": [{"role":"user","content":"fixture"}], "future": {"integer": 9007199254740993_u64}})).send().await.unwrap()
    }
}
fn success() -> Response<replay::WireBody> {
    Response::builder().header("content-type", "application/json").body(replay::full(br#"{"choices":[{"message":{"role":"assistant","content":"fixture"},"finish_reason":"stop"}],"usage":{"prompt_tokens":2,"completion_tokens":1}}"#.to_vec())).unwrap()
}

#[tokio::test]
async fn arbitrary_official_id_skips_unmapped_providers_and_remaps_each_retry() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut upstreams = vec![];
    for index in 0..3 {
        let seen = seen.clone();
        upstreams.push(
            server(move |request| {
                let seen = seen.clone();
                async move {
                    let body = request.into_body().collect().await.unwrap().to_bytes();
                    let value: Value = serde_json::from_slice(&body).unwrap();
                    seen.lock()
                        .unwrap()
                        .push((index, value["model"].as_str().unwrap().to_owned()));
                    if index == 1 {
                        Response::builder()
                            .status(503)
                            .body(replay::full("unavailable"))
                            .unwrap()
                    } else {
                        success()
                    }
                }
            })
            .await,
        );
    }
    let fixture = Fixture::new(
        &upstreams.iter().map(|s| s.url("/v1")).collect::<Vec<_>>(),
        false,
    )
    .await;
    for (index, provider) in fixture.gateway.view().providers.iter().enumerate() {
        fixture
            .gateway
            .edit(
                Edit::DshModelsProvider {
                    id: provider.id.clone(),
                    mapping: ModelMapping(BTreeMap::from([(
                        "deepseek-v5-preview".into(),
                        (index != 0).then(|| format!("vendor-{index}-future")),
                    )])),
                },
                &fixture.gateway.view().revision,
                fixture.home.path(),
            )
            .unwrap();
    }
    let response = fixture.request("deepseek-v5-preview", false).await;
    assert_eq!(response.status(), 200);
    response.bytes().await.unwrap();
    assert_eq!(
        *seen.lock().unwrap(),
        [(1, "vendor-1-future".into()), (2, "vendor-2-future".into())]
    );
    fixture.gateway.stop().await.unwrap();
}

#[tokio::test]
async fn retries_account_rate_service_model_and_first_byte_failures_with_each_providers_mapping() {
    for (status, code, delay) in [
        (402, "insufficient_balance", false),
        (401, "invalid_api_key", false),
        (403, "account_disabled", false),
        (429, "rate_limit", false),
        (503, "service_error", false),
        (404, "model_not_found", false),
        (200, "", true),
        (200, "body_timeout", false),
        (200, "comment_timeout", false),
    ] {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let first_seen = seen.clone();
        let first = server(move |request| {
            let seen = first_seen.clone();
            async move {
                assert_eq!(
                    request.headers()["authorization"],
                    "Bearer fixture-upstream"
                );
                let body = request.into_body().collect().await.unwrap().to_bytes();
                let value: Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(value["future"]["integer"].as_u64(), Some(9007199254740993));
                seen.lock()
                    .unwrap()
                    .push(value["model"].as_str().unwrap().to_owned());
                if delay {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
                if matches!(code, "body_timeout" | "comment_timeout") {
                    let frames = async_stream::try_stream! {
                        if code == "comment_timeout" {
                            yield hyper::body::Frame::data(bytes::Bytes::from_static(b": waiting\n\n"));
                        }
                        tokio::time::sleep(Duration::from_secs(2)).await;
                        yield hyper::body::Frame::data(bytes::Bytes::from_static(b"data: [DONE]\n\n"));
                    };
                    return Response::builder().header("content-type", "text/event-stream").body(http_body_util::StreamBody::new(frames).boxed_unsync()).unwrap();
                }
                Response::builder()
                    .status(status)
                    .header("content-type", "application/json")
                    .header("retry-after", "1")
                    .body(replay::full(
                        json!({"error":{"code":code,"message":code}}).to_string(),
                    ))
                    .unwrap()
            }
        })
        .await;
        let next_seen = seen.clone();
        let second = server(move |request| {
            let seen = next_seen.clone();
            async move {
                let body = request.into_body().collect().await.unwrap().to_bytes();
                let value: Value = serde_json::from_slice(&body).unwrap();
                seen.lock()
                    .unwrap()
                    .push(value["model"].as_str().unwrap().to_owned());
                success()
            }
        })
        .await;
        let fixture = Fixture::new(&[first.url("/v1"), second.url("/v1")], false).await;
        let response = fixture.request(models::PRO, false).await;
        assert_eq!(response.status(), 200, "{status}/{code}");
        response.bytes().await.unwrap();
        assert_eq!(
            *seen.lock().unwrap(),
            ["vendor-0-pro", "vendor-1-pro"],
            "{status}/{code}"
        );
        let mut changes = fixture.usage.subscribe();
        let records = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let rows = fixture
                    .usage
                    .query(|s| Ok(s.logs(&crate::usage::model::Filter::default())?.rows))
                    .unwrap();
                if !rows.is_empty() {
                    break rows;
                }
                changes.recv().await.unwrap();
            }
        })
        .await
        .unwrap();
        let last = records[0].attempts.last().unwrap();
        assert_eq!(last.requested_model.as_deref(), Some(models::PRO));
        assert_eq!(last.upstream_model.as_deref(), Some("vendor-1-pro"));
        assert_eq!(last.pricing_model.as_deref(), Some("vendor-1-pro"));
        fixture.gateway.stop().await.unwrap();
    }
}

#[tokio::test]
async fn manual_mode_and_empty_pro_mapping_do_not_fall_back_to_flash() {
    let seen = Arc::new(Mutex::new(0));
    let count = seen.clone();
    let upstream = server(move |_| {
        let count = count.clone();
        async move {
            *count.lock().unwrap() += 1;
            Response::builder()
                .status(401)
                .body(replay::full("unauthorized"))
                .unwrap()
        }
    })
    .await;
    let fixture = Fixture::new(&[upstream.url("/v1"), upstream.url("/alternate/v1")], true).await;
    let response = fixture.request(models::FLASH, false).await;
    assert_eq!(response.status(), 401);
    response.bytes().await.unwrap();
    assert_eq!(*seen.lock().unwrap(), 1);
    let providers = fixture.gateway.try_view().unwrap().providers;
    for p in providers {
        fixture
            .gateway
            .edit(
                Edit::DshModelsProvider {
                    id: p.id,
                    mapping: ModelMapping(BTreeMap::from([
                        (models::FLASH.into(), Some("flash-only".into())),
                        (models::PRO.into(), None),
                    ])),
                },
                &fixture.gateway.try_view().unwrap().revision,
                fixture.home.path(),
            )
            .unwrap();
    }
    let response = fixture.request(models::PRO, false).await;
    assert!(!response.status().is_success());
    response.bytes().await.unwrap();
    assert_eq!(*seen.lock().unwrap(), 1);
    fixture.gateway.stop().await.unwrap();
}

#[tokio::test]
async fn connection_failure_retries_but_output_followed_by_error_never_replays() {
    let hits = Arc::new(Mutex::new(0));
    let count = hits.clone();
    let second = server(move |_| {
        let count = count.clone();
        async move {
            *count.lock().unwrap() += 1;
            success()
        }
    })
    .await;
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = closed.local_addr().unwrap().port();
    drop(closed);
    let fixture = Fixture::new(
        &[format!("http://127.0.0.1:{port}/v1"), second.url("/v1")],
        false,
    )
    .await;
    let response = fixture.request(models::FLASH, false).await;
    assert_eq!(response.status(), 200);
    response.bytes().await.unwrap();
    assert_eq!(*hits.lock().unwrap(), 1);
    fixture.gateway.stop().await.unwrap();
    let first = server(|_| async {
        let frames = async_stream::try_stream! {
            yield hyper::body::Frame::data(bytes::Bytes::from_static(b"data: {\"choices\":[{\"delta\":{\"content\":\"fixture\"},\"index\":0}]}\n\n"));
            tokio::time::sleep(Duration::from_millis(20)).await;
            yield hyper::body::Frame::data(bytes::Bytes::from_static(b"data: {\"error\":{\"code\":\"insufficient_balance\"}}\n\n"));
        };
        Response::builder().header("content-type", "text/event-stream").body(http_body_util::StreamBody::new(frames).boxed_unsync()).unwrap()
    }).await;
    let fixture = Fixture::new(&[first.url("/v1"), second.url("/v1")], false).await;
    let response = fixture.request(models::FLASH, true).await;
    assert_eq!(response.status(), 200);
    let output = response.text().await.unwrap();
    assert!(output.contains("fixture") && output.contains("insufficient_balance"));
    assert_eq!(
        *hits.lock().unwrap(),
        1,
        "must not replay once business output was sent"
    );
    fixture.gateway.stop().await.unwrap();
}

#[tokio::test]
async fn a_model_error_isolated_to_pro_still_allows_flash_on_the_same_provider() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let first_seen = seen.clone();
    let first = server(move |request| {
        let seen = first_seen.clone();
        async move {
            let body = request.into_body().collect().await.unwrap().to_bytes();
            let value: Value = serde_json::from_slice(&body).unwrap();
            let model = value["model"].as_str().unwrap().to_owned();
            seen.lock().unwrap().push(model.clone());
            if model.ends_with("pro") {
                Response::builder()
                    .status(404)
                    .header("content-type", "application/json")
                    .body(replay::full(r#"{"error":{"code":"model_not_found"}}"#))
                    .unwrap()
            } else {
                success()
            }
        }
    })
    .await;
    let second = server(|_| async { success() }).await;
    let fixture = Fixture::new(&[first.url("/v1"), second.url("/v1")], false).await;
    for model in [models::PRO, models::FLASH] {
        let response = fixture.request(model, false).await;
        assert_eq!(response.status(), 200);
        response.bytes().await.unwrap();
    }
    assert_eq!(*seen.lock().unwrap(), ["vendor-0-pro", "vendor-0-flash"]);
    fixture.gateway.stop().await.unwrap();
}

#[tokio::test]
async fn exhausted_candidates_return_failure_within_the_retry_budget() {
    let calls = Arc::new(Mutex::new(0));
    let count = calls.clone();
    let upstream = server(move |_| {
        let count = count.clone();
        async move {
            *count.lock().unwrap() += 1;
            Response::builder()
                .status(429)
                .header("retry-after", "1")
                .header("content-type", "application/json")
                .body(replay::full(r#"{"error":{"code":"rate_limit"}}"#))
                .unwrap()
        }
    })
    .await;
    let fixture = Fixture::new(&[upstream.url("/v1"), upstream.url("/other/v1")], false).await;
    let mut settings = fixture.gateway.view().settings;
    settings.retry_budget_seconds = 1;
    fixture
        .gateway
        .edit(
            Edit::Settings { settings },
            &fixture.gateway.view().revision,
            fixture.home.path(),
        )
        .unwrap();
    let response = fixture.request(models::FLASH, false).await;
    assert_eq!(response.status(), 504);
    response.bytes().await.unwrap();
    assert_eq!(*calls.lock().unwrap(), 2);
    fixture.gateway.stop().await.unwrap();
}
