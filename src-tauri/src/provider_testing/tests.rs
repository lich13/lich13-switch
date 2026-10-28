use super::*;
use crate::gateway::Edit;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{body::Incoming, Request as HttpRequest, Response};
use hyper_util::rt::TokioIo;
use std::{future::Future, time::Duration};

const INSTALLATION_ID: &str = "11111111-2222-4333-8444-555555555555";
const SESSION_ID: &str = "fixture-private-session";
const THREAD_ID: &str = "fixture-private-thread";

pub(super) fn fixture() -> (tempfile::TempDir, Service) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("installation_id"), INSTALLATION_ID).unwrap();
    let codex = Gateway::new(dir.path().join("codex")).unwrap();
    codex.observe_home(dir.path());
    let claude = codex.companion(dir.path().join("claude")).unwrap();
    let grok = codex.grok_companion(dir.path().join("grok")).unwrap();
    let dsh = codex.dsh_companion(dir.path().join("dsh")).unwrap();
    let usage = crate::usage::Service::new(dir.path());
    for gateway in [&codex, &claude, &grok, &dsh] {
        gateway.set_usage(usage.clone());
    }
    let service = Service::new(dir.path(), [codex, claude, grok, dsh]).unwrap();
    service
        .0
        .clock
        .store(now(), std::sync::atomic::Ordering::SeqCst);
    (dir, service)
}
fn seed_catalog(service: &Service, id: &str, models: &[&str]) {
    let endpoint = service
        .gateway(ClientId::Codex)
        .test_endpoint(id, None)
        .unwrap();
    let mut data = lock(&service.0.data);
    let mut next = data.saved.clone();
    next.models.insert(
        Service::models_key(ClientId::Codex, id, &endpoint),
        Models {
            models: models
                .iter()
                .map(|id| Model {
                    id: (*id).into(),
                    name: (*id).into(),
                })
                .collect(),
            default_model: models[0].into(),
            fetched_at: Some(service.now()),
            ..Default::default()
        },
    );
    service.save(&mut data, next).unwrap();
}
fn business_requests(service: &Service, provider: &str, count: usize, at: i64) {
    use crate::usage::model::{Attempt, Record};
    let records: Vec<_> = (0..120)
        .map(|i| {
            let time = at - if i < count { 1000 } else { 3_700_000 };
            Record {
                id: format!("fixture-{provider}-{i}"),
                client: "codex".into(),
                source: "proxy".into(),
                started_at: time,
                finished_at: Some(time),
                completed: true,
                attempts: vec![Attempt {
                    id: format!("fixture-attempt-{i}"),
                    provider: Some(provider.into()),
                    started_at: time,
                    status: Some(200),
                    outcome: "success".into(),
                    ..Default::default()
                }],
                ..Default::default()
            }
        })
        .collect();
    service
        .gateway(ClientId::Codex)
        .test_usage()
        .query(|store| store.write_batch(&records, None))
        .unwrap();
}

pub(super) async fn server<F, Fut>(handler: F) -> u16
where
    F: Fn(HttpRequest<Incoming>) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Response<Full<Bytes>>> + Send + 'static,
{
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let handler = Arc::new(handler);
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            let handler = handler.clone();
            tokio::spawn(async move {
                let service = hyper::service::service_fn(move |request| {
                    let handler = handler.clone();
                    async move { Ok::<_, std::convert::Infallible>(handler(request).await) }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(socket), service)
                    .with_upgrades()
                    .await;
            });
        }
    });
    port
}
pub(super) fn response(
    status: u16,
    content: &str,
    body: impl Into<Bytes>,
) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header("content-type", content)
        .body(Full::new(body.into()))
        .unwrap()
}
fn add(service: &Service, home: &Path, client: ClientId, port: u16, protocol: &str) -> String {
    let gateway = service.gateway(client);
    let args = json!({"op":match client { ClientId::Grok => "saveGrokProvider", ClientId::Dsh => "saveDshProvider", _ => "saveProvider" },
        "baseUrl":format!("http://127.0.0.1:{port}/deployment/v1"),"token":"fixture-test-token","name":"Fixture provider","protocol":protocol});
    gateway
        .edit(
            serde_json::from_value(args).unwrap(),
            &gateway.view().revision,
            home,
        )
        .unwrap();
    gateway.view().providers.last().unwrap().id.clone()
}
fn ua(service: &Service, client: ClientId) {
    if client == ClientId::Codex {
        identity(
            service.gateway(client),
            b"fixture-business/1.0 exact  UA",
            "fixture-business",
        );
    } else {
        service
            .gateway(client)
            .remember_test_user_agent(b"fixture-business/1.0 exact  UA");
    }
}
fn identity(gateway: &Gateway, raw: &[u8], originator: &str) {
    let mut headers = hyper::HeaderMap::new();
    headers.insert(
        "user-agent",
        hyper::header::HeaderValue::from_bytes(raw).unwrap(),
    );
    headers.insert("originator", originator.parse().unwrap());
    headers.insert("session-id", SESSION_ID.parse().unwrap());
    headers.insert("thread-id", THREAD_ID.parse().unwrap());
    gateway.remember_test_identity(&headers);
}
fn request(service: &Service, client: ClientId, id: &str) -> Request {
    Request {
        client_id: client,
        provider_id: id.into(),
        expected_revision: service.gateway(client).view().revision,
        model: "fixture-model".into(),
        endpoint: None,
        kind: Kind::Connection,
        concurrency: 1,
        prompt: None,
        tokenizer: false,
    }
}
async fn completed(service: &Service, id: &str) -> Detail {
    let mut events = service.subscribe();
    tokio::time::timeout(Duration::from_secs(12), async {
        loop {
            let detail = service.detail(id).unwrap();
            if detail.job.terminal() && !lock(&service.0.data).cancel.contains_key(id) {
                return detail;
            }
            let _ = events.recv().await;
        }
    })
    .await
    .expect("fixture test did not finish")
}
#[test]
fn analyzer_and_collector_match_reference_fixtures() {
    let fixture: Value = serde_json::from_str(include_str!("fixtures/modeltrace.json")).unwrap();
    let snapshot = modeltrace::bundled().unwrap();
    bank::validate(&snapshot.0).unwrap();
    assert_eq!(snapshot.1, fixture["modeltrace"]["bank_version"]);
    for case in fixture["modeltrace"]["validation_cases"]
        .as_array()
        .unwrap()
    {
        assert_eq!(
            json!(modeltrace::validate(case["input"].as_str().unwrap())),
            case["expected_numbers"],
            "{}",
            case["id"]
        );
    }
    for case in fixture["modeltrace"]["scoring_cases"].as_array().unwrap() {
        let numbers: Vec<usize> = serde_json::from_value(case["numbers"].clone()).unwrap();
        let actual = modeltrace::scores(&numbers, &snapshot.0);
        for (a, b) in actual
            .iter()
            .zip(case["expected_scores"].as_array().unwrap())
        {
            assert!((a - b.as_f64().unwrap()).abs() < 1e-12);
        }
    }
    for case in fixture["modeltrace"]["analysis_cases"].as_array().unwrap() {
        let outputs: Vec<String> = serde_json::from_value(case["outputs"].clone()).unwrap();
        let actual = modeltrace::analyze(&outputs, &snapshot).unwrap();
        assert_eq!(actual["prediction"], case["expected"]["prediction"]);
        assert_eq!(actual["used_outputs"], case["expected"]["used_outputs"]);
        if !actual.is_null() {
            assert!(
                (actual["probability"].as_f64().unwrap()
                    - case["expected"]["probability"].as_f64().unwrap())
                .abs()
                    < 1e-12
            );
        }
    }
    for case in fixture["collector_cases"].as_array().unwrap() {
        let mut collector =
            fingerprint_stream::Collector::new(case["expected_numbers"].as_u64().unwrap() as usize);
        let mut failure = None;
        for item in case["events"].as_array().unwrap() {
            if let Err(error) = collector.accept(
                &item["event"],
                item["name"].as_str().unwrap_or(""),
                item["whole"].as_bool().unwrap_or(false),
            ) {
                failure = Some(error);
                break;
            }
        }
        let actual = json!({"error":failure.map(|e|json!({"code":e.code,"retryable":e.retryable})),"text":collector.text(),"completed":collector.completed,
            "returned_model":collector.returned_model,"recognized":collector.recognized,"protocol":collector.protocol,"end_reason":collector.end_reason,"metadata":collector.metadata});
        assert_eq!(actual, case["expected"], "{}", case["id"]);
    }
    let challenges = modeltrace::challenges();
    assert_eq!(challenges.len(), 3);
    assert!(challenges
        .iter()
        .all(|(n, p)| (292..=332).contains(n) && p.contains("禁止调用")));
    assert_ne!(challenges[0].0, challenges[1].0);
    assert_ne!(challenges[1].0, challenges[2].0);
}

#[tokio::test]
async fn native_connection_protocols_keep_ua_mapping_and_business_accounting_separate() {
    for (client, protocol, path) in [
        (ClientId::Codex, "responses", "/responses"),
        (ClientId::Claude, "messages", "/messages"),
        (ClientId::Grok, "chat_completions", "/chat/completions"),
        (ClientId::Dsh, "chat_completions", "/chat/completions"),
    ] {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let capture = seen.clone();
        let port=server(move |r| { let capture=capture.clone(); async move {
            let headers=r.headers().clone(); let path=r.uri().path().to_owned();
            let body=r.into_body().collect().await.unwrap().to_bytes();
            lock(&capture).push((path,headers,serde_json::from_slice::<Value>(&body).unwrap()));
            response(200,"application/json",json!({"id":"fixture-response","model":"fixture-returned","status":"completed","output_text":"fixture private output","usage":{"input_tokens":2,"output_tokens":1}}).to_string())
        }}).await;
        let (dir, service) = fixture();
        let id = add(&service, dir.path(), client, port, protocol);
        assert!(service.start(request(&service, client, &id)).is_err());
        ua(&service, client);
        if client == ClientId::Dsh {
            let gateway = service.gateway(client);
            gateway.edit(serde_json::from_value(json!({"op":"dshModelsProvider","id":id,"mapping":{"fixture-model":"fixture-mapped"}})).unwrap(), &gateway.view().revision, dir.path()).unwrap();
        }
        let mut custom = request(&service, client, &id);
        custom.prompt = Some("fixture greeting\n第二行".into());
        service.set_prompt(custom.prompt.clone().unwrap()).unwrap();
        let job = service.start(custom).unwrap();
        let result = completed(&service, &job.id).await;
        assert_eq!(result.job.status, "completed", "{:?}", result.job.error);
        assert_eq!(
            result.job.returned_model.as_deref(),
            Some("fixture-returned")
        );
        let seen = lock(&seen);
        assert_eq!(seen.len(), 1);
        assert!(seen[0].0.ends_with(path));
        let messages = if protocol == "responses" {
            "input"
        } else {
            "messages"
        };
        assert_eq!(
            seen[0].2[messages][0]["content"][0]["text"],
            "fixture greeting\n第二行"
        );
        let reopened = Service::new(dir.path(), service.0.gateways.clone()).unwrap();
        assert_eq!(
            reopened.state(client, &id).unwrap().connection_prompt,
            "fixture greeting\n第二行"
        );
        assert_eq!(seen[0].1["user-agent"], "fixture-business/1.0 exact  UA");
        if client == ClientId::Codex {
            assert_eq!(seen[0].1["originator"], "fixture-business");
            assert_eq!(seen[0].1["x-codex-installation-id"], INSTALLATION_ID);
            assert_eq!(seen[0].1["session-id"], SESSION_ID);
            assert_eq!(seen[0].1["thread-id"], THREAD_ID);
            assert_eq!(
                seen[0].2["client_metadata"]["x-codex-installation-id"],
                INSTALLATION_ID
            );
        } else {
            assert!(!seen[0].1.contains_key("originator"));
            assert!(!seen[0].1.contains_key("x-codex-installation-id"));
            assert!(!seen[0].1.contains_key("session-id"));
            assert!(seen[0].2.get("client_metadata").is_none());
        }
        assert_eq!(
            seen[0].2["model"],
            if client == ClientId::Dsh {
                "fixture-mapped"
            } else {
                "fixture-model"
            }
        );
        if client == ClientId::Dsh {
            assert_eq!(seen[0].2["max_tokens"], 256);
            assert!(seen[0].2.get("max_completion_tokens").is_none());
        }
        if protocol == "messages" {
            assert_eq!(seen[0].1["x-api-key"], "fixture-test-token");
            assert!(seen[0].1.contains_key("anthropic-version"));
        }
        assert!(service
            .gateway(ClientId::Codex)
            .detection_request_counts(service.now())
            .unwrap()
            .is_empty());
        let saved = std::fs::read_to_string(dir.path().join("provider-tests.json")).unwrap();
        assert!(!saved.contains("fixture private output"));
        assert!(!saved.contains("fixture-test-token"));
        assert!(!saved.contains("fixture-business"));
        assert!(!saved.contains("fixture input"));
    }
}
#[tokio::test]
async fn global_prompt_survives_failure_and_restart_across_clients() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let capture = seen.clone();
    let port = server(move |r| {
        let capture = capture.clone();
        async move {
            let body = r.into_body().collect().await.unwrap().to_bytes();
            let value: Value = serde_json::from_slice(&body).unwrap();
            lock(&capture).push(value["input"][0]["content"][0]["text"].clone());
            response(
                403,
                "application/json",
                r#"{"error":{"message":"fixture denied"}}"#,
            )
        }
    })
    .await;
    let (dir, service) = fixture();
    let id = add(&service, dir.path(), ClientId::Codex, port, "responses");
    let other = add(&service, dir.path(), ClientId::Codex, port, "responses");
    let claude = add(&service, dir.path(), ClientId::Claude, port, "messages");
    ua(&service, ClientId::Codex);
    let revision = service.state(ClientId::Codex, &id).unwrap().revision;
    let mut custom = request(&service, ClientId::Codex, &id);
    custom.prompt = Some("fixture remembered prompt".into());
    service.set_prompt(custom.prompt.clone().unwrap()).unwrap();
    let job = service.start(custom).unwrap();
    assert_eq!(completed(&service, &job.id).await.job.status, "failed");
    let job = service
        .start(request(&service, ClientId::Codex, &id))
        .unwrap();
    completed(&service, &job.id).await;
    assert_eq!(*lock(&seen), vec![json!("fixture remembered prompt"); 2]);
    let reopened = Service::new(dir.path(), service.0.gateways.clone()).unwrap();
    let state = reopened.state(ClientId::Codex, &id).unwrap();
    assert_eq!(state.connection_prompt, "fixture remembered prompt");
    assert_eq!(state.revision, revision);
    assert!(reopened.detail(&job.id).unwrap().output.is_none());
    assert_eq!(
        reopened
            .state(ClientId::Codex, &other)
            .unwrap()
            .connection_prompt,
        "fixture remembered prompt"
    );
    assert_eq!(
        reopened
            .state(ClientId::Claude, &claude)
            .unwrap()
            .connection_prompt,
        "fixture remembered prompt"
    );

    let path = dir.path().join("provider-tests.json");
    let mut legacy: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    legacy.as_object_mut().unwrap().remove("connectionPrompt");
    std::fs::write(&path, legacy.to_string()).unwrap();
    let legacy = Service::new(dir.path(), service.0.gateways.clone()).unwrap();
    assert_eq!(
        legacy
            .state(ClientId::Codex, &id)
            .unwrap()
            .connection_prompt,
        DEFAULT_CONNECTION_PROMPT
    );
    assert_eq!(legacy.state(ClientId::Codex, &id).unwrap().jobs.len(), 2);
}

#[tokio::test]
async fn empty_prompts_and_failed_atomic_saves_never_dispatch_or_replace_saved_prompts() {
    let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let calls = hits.clone();
    let port = server(move |_| {
        calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        async {
            response(
                200,
                "application/json",
                r#"{"output_text":"fixture reply"}"#,
            )
        }
    })
    .await;
    let (dir, service) = fixture();
    let id = add(&service, dir.path(), ClientId::Codex, port, "responses");
    ua(&service, ClientId::Codex);
    let mut custom = request(&service, ClientId::Codex, &id);
    custom.prompt = Some(" \n\t ".into());
    assert!(service
        .start(custom.clone())
        .err()
        .unwrap()
        .message
        .contains("测试文案"));
    assert!(service.state(ClientId::Codex, &id).unwrap().jobs.is_empty());
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 0);
    custom.prompt = Some("fixture saved prompt".into());
    assert!(service.set_prompt(" \n\t ".into()).is_err());
    service.set_prompt(custom.prompt.clone().unwrap()).unwrap();
    let job = service.start(custom.clone()).unwrap();
    assert_eq!(completed(&service, &job.id).await.job.status, "completed");
    let path = dir.path().join("provider-tests.json");
    let mut externally_changed = std::fs::read_to_string(&path).unwrap();
    externally_changed.push(' ');
    std::fs::write(&path, &externally_changed).unwrap();
    custom.prompt = Some("fixture unsaved prompt".into());
    assert!(service.start(custom).is_err());
    tokio::task::yield_now().await;
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(service.state(ClientId::Codex, &id).unwrap().jobs.len(), 1);
    assert_eq!(
        service
            .state(ClientId::Codex, &id)
            .unwrap()
            .connection_prompt,
        "fixture saved prompt"
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), externally_changed);
}

#[tokio::test]
async fn catalog_is_manual_persistent_and_keeps_previous_success_on_failure() {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let hits = calls.clone();
    let port = server(move |request| { let hits = hits.clone(); async move {
        assert!(request.uri().path().ends_with("/models"));
        let n = hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        match n {
            0 => response(200, "application/json", r#"{"models":[{"slug":"fixture-model"},{"slug":"fixture-other"},{"slug":"fixture-image"}]}"#),
            1 => response(503, "text/plain", "fixture directory unavailable"),
            _ => response(200, "application/json", r#"{"data":[]}"#),
        }
    }}).await;
    let (dir, service) = fixture();
    let id = add(&service, dir.path(), ClientId::Codex, port, "responses");
    let gateway = service.gateway(ClientId::Codex);
    assert!(service
        .models(ClientId::Codex, &id, &gateway.view().revision, None, false)
        .await
        .unwrap()
        .models
        .is_empty());
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    ua(&service, ClientId::Codex);
    gateway
        .edit(
            Edit::ModelsProvider {
                id: id.clone(),
                allowed_models: Some(vec!["fixture-model".into()]),
            },
            &gateway.view().revision,
            dir.path(),
        )
        .unwrap();
    let models = service
        .models(ClientId::Codex, &id, &gateway.view().revision, None, true)
        .await
        .unwrap();
    assert_eq!(models.models.len(), 2);
    let reloaded = Service::new(dir.path(), service.0.gateways.clone()).unwrap();
    assert_eq!(
        reloaded
            .models(ClientId::Codex, &id, &gateway.view().revision, None, false)
            .await
            .unwrap()
            .models
            .len(),
        2
    );
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    let failed = reloaded
        .models(ClientId::Codex, &id, &gateway.view().revision, None, true)
        .await
        .unwrap();
    assert_eq!(failed.models.len(), 2);
    assert!(failed.error.is_some());
    assert_eq!(
        failed.error_detail.unwrap().body,
        "fixture directory unavailable"
    );
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    let saved = std::fs::read_to_string(dir.path().join("provider-tests.json")).unwrap();
    assert!(!saved.contains("fixture directory unavailable"));
    assert!(reloaded
        .models(ClientId::Codex, &id, &gateway.view().revision, None, false)
        .await
        .unwrap()
        .error
        .is_none());
    let empty = reloaded
        .models(ClientId::Codex, &id, &gateway.view().revision, None, true)
        .await
        .unwrap();
    assert!(empty.error.is_none());
    assert!(empty.models.is_empty());
    assert!(empty.default_model.is_empty());
    assert!(empty.fetched_at.is_some());
    let restarted = Service::new(dir.path(), service.0.gateways.clone()).unwrap();
    assert!(restarted
        .models(ClientId::Codex, &id, &gateway.view().revision, None, false)
        .await
        .unwrap()
        .models
        .is_empty());
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 3);
}
#[tokio::test]
async fn cancellation_restart_retires_appointments_and_preserves_history() {
    let port = server(|r| async {
        let _ = r.into_body().collect().await;
        tokio::time::sleep(Duration::from_secs(60)).await;
        response(200, "application/json", "{}")
    })
    .await;
    let (dir, service) = fixture();
    let id = add(&service, dir.path(), ClientId::Codex, port, "responses");
    ua(&service, ClientId::Codex);
    let job = service
        .start(request(&service, ClientId::Codex, &id))
        .unwrap();
    service.cancel(&job.id).unwrap();
    assert_eq!(completed(&service, &job.id).await.job.status, "cancelled");
    let path = dir.path().join("provider-tests.json");
    let mut saved: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    saved["jobs"][0]["status"] = json!("running");
    saved["jobs"][0]["canRetry"] = json!(true);
    saved["jobs"][0]["mode"] = json!("image");
    saved["schedules"] = json!([{"id":"fixture-pending","at":0,"status":"pending","providers":[{"clientId":"codex","providerId":id,"providerVersion":"fixture","model":"fixture"}]}]);
    saved["schema"] = json!(1);
    saved["detection"] = json!({"intervalMinutes":25});
    saved["history"] = json!([saved["jobs"][0].clone()]);
    std::fs::write(&path, serde_json::to_vec(&saved).unwrap()).unwrap();
    let reloaded = Service::new(dir.path(), service.0.gateways.clone()).unwrap();
    let state = reloaded.state(ClientId::Codex, &id).unwrap();
    assert_eq!(state.jobs[0].status, "interrupted");
    assert_eq!(state.jobs[0].mode, Mode::Image);
    assert!(!state.jobs[0].can_retry);
    assert_eq!(state.detection.interval_minutes, 25);
    assert_eq!(lock(&reloaded.0.data).saved.history.len(), 1);
    reloaded.tick(now(), false).unwrap();
    assert_eq!(reloaded.state(ClientId::Codex, &id).unwrap().jobs.len(), 1);
    let migrated: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert!(migrated.get("schedules").is_none());
    assert_eq!(migrated["bank"], saved["bank"]);
    assert_eq!(migrated["degraded"], saved["degraded"]);
}
#[test]
fn smart_tiers_config_versions_and_corrupt_bank_fallback() {
    for (count, want) in [
        (0, None),
        (1, Some(30)),
        (2, Some(30)),
        (3, Some(25)),
        (9, Some(25)),
        (10, Some(20)),
        (29, Some(20)),
        (30, Some(15)),
        (59, Some(15)),
        (60, Some(10)),
        (119, Some(10)),
        (120, Some(5)),
    ] {
        assert_eq!(scheduler::smart_interval(count, 5), want);
        assert_eq!(
            scheduler::smart_interval(count, 17),
            want.map(|v| v.max(17))
        );
    }
    assert_eq!(scheduler::smart_interval(1000, 20), Some(20));
    let (dir, service) = fixture();
    let id = add(&service, dir.path(), ClientId::Codex, 1, "responses");
    seed_catalog(&service, &id, &[DEFAULT_DETECTION_MODEL]);
    let state = service.state(ClientId::Codex, &id).unwrap();
    assert!(!state.detection.enabled);
    assert_eq!(state.detection.mode, "smart");
    let mut config = state.detection;
    config.interval_minutes = 0;
    assert!(service
        .set_detection(ClientId::Codex, &id, config.clone(), &state.revision)
        .is_err());
    config.interval_minutes = 15;
    config.enabled = true;
    service
        .set_detection(ClientId::Codex, &id, config, &state.revision)
        .unwrap();
    assert!(service
        .clear_mark(ClientId::Codex, &id, &state.revision)
        .is_err());
    let bad = SavedBank {
        data: Some(json!({"invalid":true})),
        version: Some(json!({"revision":"fixture"})),
        provenance: Some(json!({})),
        ..Default::default()
    };
    assert_eq!(
        bank::Resource::load(&bad).snapshot.unwrap().1,
        modeltrace::bundled().unwrap().1
    );
    assert!(bank::Resource::load(&bad).status(&bad, false)["error"].is_string());
    let same_version = SavedBank {
        version: Some(modeltrace::bundled().unwrap().1),
        ..bad
    };
    let status = bank::Resource::load(&same_version).status(&same_version, false);
    assert!(status["error"].is_string());
    assert_eq!(status["source"], "builtin");
}
fn sample(name: &str, index: usize) -> String {
    let fixture: Value = serde_json::from_str(include_str!("fixtures/runtime.json")).unwrap();
    fixture["modeltrace_runtime"][name]["outputs"][index]
        .as_str()
        .unwrap()
        .into()
}
fn fingerprint_response(text: &str) -> Response<Full<Bytes>> {
    response(
        200,
        "text/event-stream",
        format!(
            "data: {}\n\n",
            json!({"type":"response.completed","response":{"model":"fixture-returned","status":"completed","output":[{"type":"message","content":[{"type":"output_text","text":text}]}]}})
        ),
    )
}
fn fingerprint_request(service: &Service, id: &str, concurrency: usize) -> Request {
    let mut request = request(service, ClientId::Codex, id);
    request.kind = Kind::Fingerprint;
    request.model = "gpt-6-sol".into();
    request.concurrency = concurrency;
    seed_catalog(service, id, &[&request.model]);
    request
}
fn scheduled_fingerprint(service: &Service, home: &Path, id: &str) -> Job {
    let gateway = service.gateway(ClientId::Codex);
    gateway
        .edit(
            Edit::QueueProvider {
                id: id.into(),
                queued: true,
            },
            &gateway.view().revision,
            home,
        )
        .unwrap();
    seed_catalog(service, id, &["gpt-6-sol"]);
    let state = service.state(ClientId::Codex, id).unwrap();
    let detection = Detection {
        enabled: true,
        mode: "fixed".into(),
        interval_minutes: 1,
        model: "gpt-6-sol".into(),
        ..state.detection
    };
    service
        .set_detection(ClientId::Codex, id, detection, &state.revision)
        .unwrap();
    let at = service.now();
    service.tick(at, false).unwrap();
    service.tick(at + 60_000, false).unwrap();
    service
        .state(ClientId::Codex, id)
        .unwrap()
        .jobs
        .into_iter()
        .find(|job| job.automatic)
        .unwrap()
}

#[tokio::test]
async fn remembered_connection_prompts_do_not_change_manual_or_scheduled_challenges() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let capture = seen.clone();
    let port = server(move |r| {
        let capture = capture.clone();
        async move {
            let body = r.into_body().collect().await.unwrap().to_bytes();
            let value: Value = serde_json::from_slice(&body).unwrap();
            let text = value["input"][0]["content"][0]["text"].as_str().unwrap();
            lock(&capture).push(text.to_owned());
            if text == "fixture saved greeting" {
                response(
                    200,
                    "application/json",
                    r#"{"output_text":"fixture reply"}"#,
                )
            } else {
                fingerprint_response(&sample("above_99", 0))
            }
        }
    })
    .await;
    let (dir, service) = fixture();
    let id = add(&service, dir.path(), ClientId::Codex, port, "responses");
    ua(&service, ClientId::Codex);
    let mut connection = request(&service, ClientId::Codex, &id);
    connection.prompt = Some("fixture saved greeting".into());
    service
        .set_prompt(connection.prompt.clone().unwrap())
        .unwrap();
    let job = service.start(connection).unwrap();
    completed(&service, &job.id).await;
    let mut manual = fingerprint_request(&service, &id, 1);
    manual.prompt = Some("fixture ignored fingerprint prompt".into());
    let manual = service.start(manual).unwrap();
    assert_eq!(
        completed(&service, &manual.id).await.job.status,
        "completed"
    );
    let automatic = scheduled_fingerprint(&service, dir.path(), &id);
    assert_eq!(
        completed(&service, &automatic.id).await.job.status,
        "completed"
    );
    assert_eq!(
        service
            .state(ClientId::Codex, &id)
            .unwrap()
            .connection_prompt,
        "fixture saved greeting"
    );
    let seen = lock(&seen);
    assert!(seen.len() >= 3);
    for challenge in &seen[1..] {
        assert!(challenge.contains("整数"));
        assert!(!challenge.contains("fixture"));
    }
}

#[tokio::test]
async fn strict_client_gate_accepts_local_identity_for_every_codex_test_path() {
    for (user_agent, originator) in [
        ("codex_cli_rs/0.1.2 (fixture; arm64)", "codex_cli_rs"),
        (
            "Codex Desktop/0.1.2 (fixture; arm64) exact  spaces",
            "Codex Desktop",
        ),
    ] {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let hits = calls.clone();
        let port = server(move |r| {
            let hits = hits.clone();
            async move {
                hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                // All configured fingerprint signals are ANDed by Sub2API.
                let ua = r.headers().get("user-agent").unwrap().to_str().unwrap();
                let official = ua.starts_with("codex_cli_rs/0.1.2")
                    || ua.starts_with("Codex Desktop/0.1.2");
                let engine = r.headers().keys().any(|key| key.as_str().starts_with("x-codex-"));
                let session = r.headers().get("session-id").is_some_and(|v| v == SESSION_ID);
                let thread = r.headers().get("thread-id").is_some_and(|v| v == THREAD_ID);
                if !official || !engine || !session || !thread {
                    return response(403, "application/json", r#"{"error":{"message":"This account only allows Codex official clients","type":"forbidden_error"}}"#);
                }
                assert_eq!(ua, user_agent);
                assert_eq!(r.headers()["originator"], originator);
                assert_eq!(r.headers()["x-codex-installation-id"], INSTALLATION_ID);
                assert_eq!(r.headers()["authorization"], "Bearer fixture-test-token");
                if r.uri().path().ends_with("/models") {
                    response(200, "application/json", r#"{"data":[{"id":"gpt-6-sol"}]}"#)
                } else {
                    let body = r.into_body().collect().await.unwrap().to_bytes();
                    let body: Value = serde_json::from_slice(&body).unwrap();
                    if body["client_metadata"]["x-codex-installation-id"] != INSTALLATION_ID {
                        return response(403, "application/json", r#"{"error":{"message":"This account only allows Codex official clients"}}"#);
                    }
                    fingerprint_response(&sample("above_99", 0))
                }
            }
        }).await;
        let (dir, service) = fixture();
        let id = add(&service, dir.path(), ClientId::Codex, port, "responses");
        let gateway = service.gateway(ClientId::Codex);
        identity(gateway, user_agent.as_bytes(), originator);
        let target = gateway.test_target(&id, None).unwrap();
        let old = reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .post(target.url("/v1/responses").unwrap())
            .header("user-agent", user_agent)
            .json(&json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(old.status(), 403);
        for missing in ["session-id", "thread-id", "client_metadata"] {
            let mut headers = target.headers.clone();
            headers.remove(missing);
            let body = if missing == "client_metadata" {
                json!({})
            } else {
                json!({"client_metadata": target.codex_metadata().unwrap()})
            };
            let rejected = reqwest::Client::builder()
                .no_proxy()
                .build()
                .unwrap()
                .post(target.url("/v1/responses").unwrap())
                .headers(headers)
                .json(&body)
                .send()
                .await
                .unwrap();
            assert_eq!(rejected.status(), 403, "missing {missing}");
        }
        let models = service
            .models(ClientId::Codex, &id, &gateway.view().revision, None, true)
            .await
            .unwrap();
        assert!(models.error.is_none());
        assert_eq!(models.models[0].id, "gpt-6-sol");
        let connection = service
            .start(request(&service, ClientId::Codex, &id))
            .unwrap();
        assert_eq!(
            completed(&service, &connection.id).await.job.status,
            "completed"
        );
        let fingerprint = service
            .start(fingerprint_request(&service, &id, 1))
            .unwrap();
        assert_eq!(
            completed(&service, &fingerprint.id).await.job.status,
            "completed"
        );
        let automatic = scheduled_fingerprint(&service, dir.path(), &id);
        assert_eq!(
            completed(&service, &automatic.id).await.job.status,
            "completed"
        );
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 12);
        let state = service.state(ClientId::Codex, &id).unwrap();
        assert!(state.degraded.is_empty());
        assert!(gateway.test_candidates().unwrap()[0].queued);
        assert_eq!(gateway.successful_user_agent().unwrap(), user_agent);
        assert!(gateway
            .detection_request_counts(service.now())
            .unwrap()
            .is_empty());
        let saved = std::fs::read_to_string(dir.path().join("provider-tests.json")).unwrap();
        assert!(!saved.contains(INSTALLATION_ID));
        assert!(!saved.contains(user_agent));
        assert!(!saved.contains(SESSION_ID));
        assert!(!saved.contains(THREAD_ID));
    }
}

#[tokio::test]
async fn codex_identity_reads_selected_home_and_blocks_invalid_data_without_network() {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let hits = calls.clone();
    let port = server(move |_| {
        let hits = hits.clone();
        async move {
            hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            response(200, "application/json", "{}")
        }
    })
    .await;
    let (dir, service) = fixture();
    let id = add(&service, dir.path(), ClientId::Codex, port, "responses");
    let gateway = service.gateway(ClientId::Codex);
    identity(gateway, b"Codex Desktop/0.1.2 (fixture)", "Codex Desktop");
    seed_catalog(&service, &id, &["fixture-model"]);
    let selected = dir.path().join("selected-codex");
    std::fs::create_dir(&selected).unwrap();
    gateway.observe_home(&selected);
    assert!(gateway
        .test_target(&id, None)
        .err()
        .unwrap()
        .message
        .contains("正常 Codex 请求"));
    identity(gateway, b"Codex Desktop/0.1.2 (fixture)", "Codex Desktop");
    let file = selected.join("installation_id");
    let missing = gateway.test_target(&id, None).err().unwrap().message;
    assert!(missing.contains("未找到 Codex installation_id"));
    assert!(!file.exists());
    let selected_id = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";
    let raw = format!("{selected_id}\n");
    std::fs::write(&file, &raw).unwrap();
    let target = gateway.test_target(&id, None).unwrap();
    assert_eq!(target.headers["x-codex-installation-id"], selected_id);
    assert_eq!(target.headers["originator"], "Codex Desktop");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), raw);
    for invalid in [vec![], b"not-a-uuid".to_vec(), vec![0xff], vec![b'x'; 129]] {
        std::fs::write(&file, &invalid).unwrap();
        let error = service
            .start(request(&service, ClientId::Codex, &id))
            .err()
            .unwrap();
        assert!(error.message.contains("installation_id"));
        assert!(service
            .models(ClientId::Codex, &id, &gateway.view().revision, None, true)
            .await
            .is_err());
        let cached = service
            .models(ClientId::Codex, &id, &gateway.view().revision, None, false)
            .await
            .unwrap();
        assert_eq!(cached.models[0].id, "fixture-model");
        assert_eq!(std::fs::read(&file).unwrap(), invalid);
    }
    std::fs::remove_file(&file).unwrap();
    std::fs::create_dir(&file).unwrap();
    assert!(gateway
        .test_target(&id, None)
        .err()
        .unwrap()
        .message
        .contains("无法读取 Codex installation_id"));
    std::fs::remove_dir(&file).unwrap();
    std::fs::write(&file, selected_id).unwrap();
    for invalid in [
        b"missing-version".as_slice(),
        b"/0.1.2",
        b"codex_cli_rs/",
        b"codex\tcli/0.1.2",
    ] {
        identity(gateway, invalid, "codex_cli_rs");
        assert!(gateway
            .test_target(&id, None)
            .err()
            .unwrap()
            .message
            .contains("User-Agent 格式无效"));
    }
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
}

#[tokio::test]
async fn client_restrictions_keep_complete_redacted_errors_without_retry_or_degradation() {
    let error = json!({"error":{"message":format!("This account only allows Codex official clients; {INSTALLATION_ID}; {SESSION_ID}; {THREAD_ID}; fixture-test-token; fixture end-marker"),"type":"forbidden_error"}});
    let stream = format!(
        "event: response.failed\ndata: {}\n\n",
        json!({"type":"response.failed","response":{"status":"failed","error":error["error"]}})
    );
    for (status, mime, body) in [
        (403, "application/json", format!("{error}{stream}")),
        (200, "application/json", error.to_string()),
        (200, "text/event-stream", stream),
    ] {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let hits = calls.clone();
        let port = server(move |_| {
            let hits = hits.clone();
            let body = body.clone();
            async move {
                hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                response(status, mime, body)
            }
        })
        .await;
        let (dir, service) = fixture();
        let id = add(&service, dir.path(), ClientId::Codex, port, "responses");
        ua(&service, ClientId::Codex);
        let gateway = service.gateway(ClientId::Codex);
        let models = service
            .models(ClientId::Codex, &id, &gateway.view().revision, None, true)
            .await
            .unwrap();
        assert!(models.error.is_some());
        let detail = models.error_detail.unwrap();
        assert_eq!(detail.http_status, Some(status));
        assert!(detail.message.starts_with(errors::CLIENT_RESTRICTED));
        assert!(detail.body.contains("fixture end-marker"));
        assert!(!detail.body.contains(INSTALLATION_ID));
        let connection = service
            .start(request(&service, ClientId::Codex, &id))
            .unwrap();
        let done = completed(&service, &connection.id).await;
        assert_eq!(done.job.error.as_deref(), Some(errors::CLIENT_RESTRICTED));
        let detail = done.output.unwrap().error.unwrap();
        assert_eq!(detail.http_status, Some(status));
        assert!(detail.message.starts_with(errors::CLIENT_RESTRICTED));
        assert!(detail.body.contains("fixture end-marker"));
        assert!(!detail.body.contains(INSTALLATION_ID));
        assert!(!detail.body.contains("fixture-test-token"));
        assert!(!detail.truncated);
        let manual = service
            .start(fingerprint_request(&service, &id, 1))
            .unwrap();
        completed(&service, &manual.id).await;
        let automatic = scheduled_fingerprint(&service, dir.path(), &id);
        for job_id in [manual.id, automatic.id] {
            let done = completed(&service, &job_id).await;
            assert_eq!(done.job.status, "failed");
            assert_eq!(done.job.error.as_deref(), Some(errors::CLIENT_RESTRICTED));
            assert!(!done.job.can_retry);
            assert!(done.job.report.is_none());
            assert_eq!(done.job.groups.iter().map(|g| g.attempts).sum::<usize>(), 1);
            assert!(done.job.groups.iter().all(|g| !g.retryable));
            let detail = &done.output.as_ref().unwrap().group_errors[&0];
            assert_eq!(detail.http_status, Some(status));
            assert!(detail.message.starts_with(errors::CLIENT_RESTRICTED));
            assert!(detail.body.contains("fixture end-marker"));
            let serialized = serde_json::to_string(&done).unwrap();
            assert!(!serialized.contains(INSTALLATION_ID));
            assert!(!serialized.contains("fixture-test-token"));
            assert!(!serialized.contains(SESSION_ID));
            assert!(!serialized.contains(THREAD_ID));
            assert!(service.retry(&job_id).is_err());
        }
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 4);
        assert!(service
            .state(ClientId::Codex, &id)
            .unwrap()
            .degraded
            .is_empty());
        assert!(gateway.test_candidates().unwrap()[0].queued);
        let saved = std::fs::read_to_string(dir.path().join("provider-tests.json")).unwrap();
        assert!(!saved.contains(INSTALLATION_ID));
        assert!(!saved.contains("fixture end-marker"));
    }
    assert_eq!(
        fingerprint_stream::failure(
            403,
            &json!({"error":{"message":"fixture permission denied"}})
        )
        .code,
        "auth_or_quota"
    );
}

#[tokio::test]
async fn fingerprint_completes_all_three_groups_even_above_99() {
    for concurrency in [1, 3] {
        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = hits.clone();
        let barrier = Arc::new(tokio::sync::Barrier::new(concurrency));
        let port = server(move |r| {
            let count = count.clone();
            let barrier = barrier.clone();
            async move {
                let _ = r.into_body().collect().await.unwrap();
                let n = count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                barrier.wait().await;
                fingerprint_response(&sample(
                    if concurrency == 1 {
                        "above_99"
                    } else {
                        "below_99"
                    },
                    n,
                ))
            }
        })
        .await;
        let (dir, service) = fixture();
        let id = add(&service, dir.path(), ClientId::Codex, port, "responses");
        ua(&service, ClientId::Codex);
        let job = service
            .start(fingerprint_request(&service, &id, concurrency))
            .unwrap();
        let detail = completed(&service, &job.id).await;
        assert_eq!(detail.job.status, "completed");
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 3);
        assert!(detail.job.completion_reason.is_none());
        assert_eq!(detail.job.report.as_ref().unwrap()["used_outputs"], 3);
        assert!(detail.job.groups.iter().all(|g| g.attempts == 1));
        assert!(service
            .state(ClientId::Codex, &id)
            .unwrap()
            .degraded
            .is_empty());
        assert_eq!(
            service
                .gateway(ClientId::Codex)
                .detection_request_counts(service.now())
                .unwrap()
                .get(&id)
                .copied()
                .unwrap_or(0),
            0
        );
    }
}
#[tokio::test]
async fn failed_groups_retry_without_replaying_successful_groups() {
    let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let count = hits.clone();
    let port = server(move |r| {
        let count = count.clone();
        async move {
            assert_eq!(r.headers()["user-agent"], "fixture-business/1.0 exact  UA");
            assert_eq!(r.headers()["originator"], "fixture-business");
            assert_eq!(r.headers()["x-codex-installation-id"], INSTALLATION_ID);
            assert_eq!(r.headers()["session-id"], SESSION_ID);
            assert_eq!(r.headers()["thread-id"], THREAD_ID);
            let body = r.into_body().collect().await.unwrap().to_bytes();
            assert_eq!(
                serde_json::from_slice::<Value>(&body).unwrap()["client_metadata"]
                    ["x-codex-installation-id"],
                INSTALLATION_ID
            );
            let n = count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            fingerprint_response(&if n < 2 {
                "1, 2, 3".into()
            } else if n == 2 {
                sample("below_99", 0)
            } else {
                sample("above_99", 0)
            })
        }
    })
    .await;
    let (dir, service) = fixture();
    let id = add(&service, dir.path(), ClientId::Codex, port, "responses");
    ua(&service, ClientId::Codex);
    let job = service
        .start(fingerprint_request(&service, &id, 1))
        .unwrap();
    let before = completed(&service, &job.id).await;
    assert!(before.job.can_retry);
    let original_bank = lock(&service.0.data).contexts[&job.id]
        .bank
        .clone()
        .unwrap();
    replace_modeltrace_resource(&service);
    assert!(Arc::ptr_eq(
        lock(&service.0.data).contexts[&job.id]
            .bank
            .as_ref()
            .unwrap(),
        &original_bank
    ));
    assert_eq!(before.job.groups[2].status, "completed");
    let output = before.output.unwrap().groups[&2].clone();
    // A later business request must not change the identity of this job's retry.
    identity(
        service.gateway(ClientId::Codex),
        b"fixture-new/2.0",
        "fixture-new",
    );
    service.retry(&job.id).unwrap();
    let after = completed(&service, &job.id).await;
    assert_eq!(after.job.groups[2].attempts, 1);
    assert_eq!(after.output.unwrap().groups[&2], output);
    assert!(hits.load(std::sync::atomic::Ordering::SeqCst) > 3);
    assert!(after.job.groups[0].attempts > 1);
    assert_eq!(
        after.job.report.as_ref().unwrap()["bank_version"],
        original_bank.1
    );
    assert!(!after.job.can_retry);
    assert!(!lock(&service.0.data).contexts.contains_key(&job.id));
}
#[tokio::test]
async fn automatic_summary_dequeues_and_manual_tests_never_change_queue() {
    let port = server(|r| async {
        let _ = r.into_body().collect().await.unwrap();
        fingerprint_response(&sample("automatic_first_group", 0))
    })
    .await;
    let (dir, service) = fixture();
    let id = add(&service, dir.path(), ClientId::Codex, port, "responses");
    ua(&service, ClientId::Codex);
    let gateway = service.gateway(ClientId::Codex);
    gateway
        .edit(
            Edit::QueueProvider {
                id: id.clone(),
                queued: true,
            },
            &gateway.view().revision,
            dir.path(),
        )
        .unwrap();
    let manual = service
        .start(fingerprint_request(&service, &id, 1))
        .unwrap();
    completed(&service, &manual.id).await;
    assert!(service
        .state(ClientId::Codex, &id)
        .unwrap()
        .degraded
        .is_empty());
    assert!(gateway.test_candidates().unwrap()[0].queued);
    let state = service.state(ClientId::Codex, &id).unwrap();
    let mut detection = state.detection;
    detection.enabled = true;
    detection.mode = "fixed".into();
    detection.model = "gpt-6-sol".into();
    detection.interval_minutes = 1;
    seed_catalog(&service, &id, &[DEFAULT_DETECTION_MODEL, "gpt-6-sol"]);
    service
        .set_detection(ClientId::Codex, &id, detection, &state.revision)
        .unwrap();
    lock(&service.0.data)
        .saved
        .detection_timing
        .get_mut(&scope_key(ClientId::Codex, &id))
        .unwrap()
        .anchor_at = Some(service.now() - 60_000);
    let job = service
        .start_job(fingerprint_request(&service, &id, 1), true)
        .unwrap();
    let result = completed(&service, &job.id).await;
    assert!(result.job.completion_reason.is_none());
    assert_eq!(
        result.job.groups.iter().map(|g| g.attempts).sum::<usize>(),
        3
    );
    assert!(!gateway.test_candidates().unwrap()[0].queued);
    let state = service.state(ClientId::Codex, &id).unwrap();
    assert!(state.degraded[&id].queue_removed);
    assert_eq!(lock(&service.0.data).saved.history.len(), 1);
    {
        let mut data = lock(&service.0.data);
        let mut next = data.saved.clone();
        next.jobs.retain(|j| j.id != job.id);
        service.save(&mut data, next).unwrap();
    }
    assert!(service.detail(&job.id).unwrap().job.report.is_some());
    assert!(service.detail(&job.id).unwrap().output.is_none());
    service
        .clear_mark(ClientId::Codex, &id, &state.revision)
        .unwrap();
    assert!(!gateway.test_candidates().unwrap()[0].queued);
    assert!(service
        .state(ClientId::Codex, &id)
        .unwrap()
        .degraded
        .is_empty());
    let manual = service
        .start(fingerprint_request(&service, &id, 1))
        .unwrap();
    assert_eq!(
        completed(&service, &manual.id).await.job.status,
        "completed"
    );
    assert!(!gateway.test_candidates().unwrap()[0].queued);
}
#[tokio::test]
async fn a_provider_has_one_job_and_queued_cancellation_does_not_dispatch() {
    let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let count = hits.clone();
    let port = server(move |_| {
        let count = count.clone();
        async move {
            count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            response(200, "application/json", r#"{"output_text":"fixture"}"#)
        }
    })
    .await;
    let (dir, service) = fixture();
    let id = add(&service, dir.path(), ClientId::Codex, port, "responses");
    ua(&service, ClientId::Codex);
    let permit = service.0.jobs.clone().acquire_many_owned(2).await.unwrap();
    let request = request(&service, ClientId::Codex, &id);
    let job = service.start(request.clone()).unwrap();
    assert!(service.start(request).is_err());
    assert!(lock(&service.0.data).contexts[&job.id].bank.is_none());
    assert!(lock(&service.0.data).contexts[&job.id].lm.is_none());
    service.cancel(&job.id).unwrap();
    assert_eq!(completed(&service, &job.id).await.job.status, "cancelled");
    assert!(!lock(&service.0.data).contexts.contains_key(&job.id));
    assert!(service.detail(&job.id).unwrap().output.is_some());
    drop(permit);
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 0);
}
#[tokio::test]
async fn detection_resets_after_sleep_and_exits_without_accumulating_periods() {
    let (dir, service) = fixture();
    let id = add(&service, dir.path(), ClientId::Codex, 1, "responses");
    ua(&service, ClientId::Codex);
    let gateway = service.gateway(ClientId::Codex);
    gateway
        .edit(
            Edit::QueueProvider {
                id: id.clone(),
                queued: true,
            },
            &gateway.view().revision,
            dir.path(),
        )
        .unwrap();
    seed_catalog(&service, &id, &[DEFAULT_DETECTION_MODEL]);
    let state = service.state(ClientId::Codex, &id).unwrap();
    let mut detection = state.detection;
    detection.enabled = true;
    service
        .set_detection(ClientId::Codex, &id, detection.clone(), &state.revision)
        .unwrap();
    let at = service.now();
    service.tick(at, false).unwrap();
    assert_eq!(
        service
            .state(ClientId::Codex, &id)
            .unwrap()
            .detection_status[0]
            .status,
        "idle"
    );
    detection.mode = "fixed".into();
    detection.interval_minutes = 5;
    service
        .set_detection(
            ClientId::Codex,
            &id,
            detection,
            &service.state(ClientId::Codex, &id).unwrap().revision,
        )
        .unwrap();
    service.tick(at, false).unwrap();
    assert_eq!(
        service
            .state(ClientId::Codex, &id)
            .unwrap()
            .detection_status[0]
            .next_at,
        Some(at + 300_000)
    );
    service.tick(at + 3_600_000, true).unwrap();
    assert!(service.state(ClientId::Codex, &id).unwrap().jobs.is_empty());
    assert_eq!(
        service
            .state(ClientId::Codex, &id)
            .unwrap()
            .detection_status[0]
            .next_at,
        Some(at + 3_900_000)
    );
    gateway
        .edit(
            Edit::QueueProvider {
                id: id.clone(),
                queued: false,
            },
            &gateway.view().revision,
            dir.path(),
        )
        .unwrap();
    service.tick(at + 3_900_000, false).unwrap();
    assert!(service.state(ClientId::Codex, &id).unwrap().jobs.is_empty());
    gateway
        .edit(
            Edit::QueueProvider {
                id: id.clone(),
                queued: true,
            },
            &gateway.view().revision,
            dir.path(),
        )
        .unwrap();
    service.tick(at + 4_200_000, false).unwrap();
    assert_eq!(
        service
            .state(ClientId::Codex, &id)
            .unwrap()
            .detection_status[0]
            .next_at,
        Some(at + 4_500_000)
    );
    let permit = service.0.jobs.clone().acquire_many_owned(2).await.unwrap();
    service.tick(at + 4_500_000, false).unwrap();
    let idjob = service.state(ClientId::Codex, &id).unwrap().jobs[0]
        .id
        .clone();
    gateway
        .edit(
            Edit::QueueProvider {
                id: id.clone(),
                queued: false,
            },
            &gateway.view().revision,
            dir.path(),
        )
        .unwrap();
    service.tick(at + 4_500_001, false).unwrap();
    assert_eq!(completed(&service, &idjob).await.job.status, "cancelled");
    drop(permit);
}

#[tokio::test]
async fn search_only_connection_uses_ordinary_chat_and_streamed_text() {
    let port = server(|request| async {
        assert!(request.uri().path().ends_with("/responses"));
        let body: Value = serde_json::from_slice(&request.into_body().collect().await.unwrap().to_bytes()).unwrap();
        assert!(body.get("tools").is_none());
        assert_eq!(body["input"][0]["content"][0]["type"], "input_text");
        assert_eq!(body["stream"], true);
        response(200, "text/event-stream", concat!(
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hello\"}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"model\":\"fixture-returned\",\"status\":\"completed\"}}\n\n"
        ))
    }).await;
    let (dir, service) = fixture();
    let id = add(&service, dir.path(), ClientId::Codex, port, "responses");
    ua(&service, ClientId::Codex);
    let gateway = service.gateway(ClientId::Codex);
    let previous = gateway.view();
    gateway
        .edit(
            Edit::SaveProvider {
                id: Some(id.clone()),
                base_url: format!("http://127.0.0.1:{port}/v1"),
                token: String::new(),
                quota_access_token: None,
                name: None,
                search_only: Some(true),
            },
            &previous.revision,
            dir.path(),
        )
        .unwrap();
    assert!(gateway.view().providers[0].search_only);
    let job = service
        .start(request(&service, ClientId::Codex, &id))
        .unwrap();
    let detail = completed(&service, &job.id).await;
    assert_eq!(detail.job.mode, Mode::Text);
    assert_eq!(detail.job.status, "completed");
    assert_eq!(detail.output.unwrap().text, "Hello");
    assert_eq!(
        detail.job.returned_model.as_deref(),
        Some("fixture-returned")
    );
    assert_eq!(
        gateway
            .detection_request_counts(now())
            .unwrap()
            .get(&id)
            .copied()
            .unwrap_or(0),
        0
    );
    assert!(service
        .start(fingerprint_request(&service, &id, 1))
        .is_err());
}

#[tokio::test]
async fn defaults_use_each_provider_catalog_and_missing_catalog_uses_platform_default() {
    let (dir, service) = fixture();
    ua(&service, ClientId::Codex);
    let mut ids = vec![];
    for name in ["fixture-alpha", "fixture-beta"] {
        let port = server(move |r| async move {
            if r.uri().path().ends_with("/models") {
                response(
                    200,
                    "application/json",
                    json!({"data":[{"id":name}]}).to_string(),
                )
            } else {
                let body: Value =
                    serde_json::from_slice(&r.into_body().collect().await.unwrap().to_bytes())
                        .unwrap();
                assert_eq!(body["model"], name);
                response(200, "application/json", r#"{"output_text":"Hello"}"#)
            }
        })
        .await;
        ids.push(add(
            &service,
            dir.path(),
            ClientId::Codex,
            port,
            "responses",
        ));
    }
    for id in &ids {
        service
            .models(
                ClientId::Codex,
                id,
                &service.gateway(ClientId::Codex).view().revision,
                None,
                true,
            )
            .await
            .unwrap();
    }
    for (id, name) in ids.iter().zip(["fixture-alpha", "fixture-beta"]) {
        let mut r = request(&service, ClientId::Codex, id);
        r.model.clear();
        let job = service.start(r).unwrap();
        assert_eq!(job.requested_model, name);
        assert_eq!(completed(&service, &job.id).await.job.status, "completed");
    }
    lock(&service.0.data).saved.models.clear();
    let permit = service.0.jobs.clone().acquire_many_owned(2).await.unwrap();
    let mut r = request(&service, ClientId::Codex, &ids[0]);
    r.model.clear();
    let job = service.start(r).unwrap();
    assert_eq!(
        job.requested_model,
        connection::default_model(ClientId::Codex)
    );
    service.cancel(&job.id).unwrap();
    completed(&service, &job.id).await;
    drop(permit);
}

#[tokio::test]
async fn ordinary_chat_rejects_empty_and_incomplete_responses() {
    for (mime, body) in [
        ("application/json", r#"{"output":[]}"#),
        (
            "text/event-stream",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\n",
        ),
        ("application/json", r#"{"error":{"message":"fixture"}}"#),
    ] {
        let port = server(move |_| async move { response(200, mime, body) }).await;
        let (dir, service) = fixture();
        let id = add(&service, dir.path(), ClientId::Codex, port, "responses");
        ua(&service, ClientId::Codex);
        let job = service
            .start(request(&service, ClientId::Codex, &id))
            .unwrap();
        assert_eq!(completed(&service, &job.id).await.job.status, "failed");
    }
}

#[test]
fn retired_connection_options_are_not_accepted_by_the_request_contract() {
    let (_dir, service) = fixture();
    let original = serde_json::to_value(request(&service, ClientId::Codex, "fixture")).unwrap();
    for (field, value) in [
        ("mode", json!("search")),
        ("input", json!("custom")),
        ("media", json!({"dataUrl":"fixture"})),
    ] {
        let mut value_with_retired_option = original.clone();
        value_with_retired_option[field] = value;
        assert!(serde_json::from_value::<Request>(value_with_retired_option).is_err());
    }
}

fn enable_detection(service: &Service, home: &Path, id: &str, mode: &str, minimum: u32) {
    let gateway = service.gateway(ClientId::Codex);
    gateway
        .edit(
            Edit::QueueProvider {
                id: id.into(),
                queued: true,
            },
            &gateway.view().revision,
            home,
        )
        .unwrap();
    seed_catalog(service, id, &["gpt-6-sol"]);
    let state = service.state(ClientId::Codex, id).unwrap();
    service
        .set_detection(
            ClientId::Codex,
            id,
            Detection {
                enabled: true,
                mode: mode.into(),
                model: "gpt-6-sol".into(),
                interval_minutes: 1,
                min_interval_minutes: minimum,
                ..Default::default()
            },
            &state.revision,
        )
        .unwrap();
}
fn status(service: &Service, id: &str) -> DetectionStatus {
    service.state(ClientId::Codex, id).unwrap().detection_status[0].clone()
}
#[tokio::test]
async fn smart_schedule_is_independent_durable_and_sampling_does_not_move_deadlines() {
    let (dir, service) = fixture();
    let a = add(&service, dir.path(), ClientId::Codex, 1, "responses");
    let b = add(&service, dir.path(), ClientId::Codex, 2, "responses");
    let at = service.now();
    ua(&service, ClientId::Codex);
    business_requests(&service, &a, 1, at);
    business_requests(&service, &b, 120, at);
    enable_detection(&service, dir.path(), &a, "smart", 5);
    enable_detection(&service, dir.path(), &b, "smart", 17);
    service.tick(at, false).unwrap();
    assert_eq!(status(&service, &a).recent_requests, Some(1));
    assert_eq!(status(&service, &a).interval_minutes, Some(30));
    assert_eq!(status(&service, &a).next_at, Some(at + 1_800_000));
    assert_eq!(status(&service, &b).recent_requests, Some(120));
    assert_eq!(status(&service, &b).interval_minutes, Some(17));
    let b_version = service.state(ClientId::Codex, &b).unwrap().revision;
    let b_due = status(&service, &b).next_at;
    let a_version = service.state(ClientId::Codex, &a).unwrap().revision;
    business_requests(&service, &a, 65, at + 60_000);
    service.tick(at + 60_000, false).unwrap();
    assert_eq!(status(&service, &a).interval_minutes, Some(10));
    assert_eq!(status(&service, &a).next_at, Some(at + 600_000));
    assert_eq!(
        service.state(ClientId::Codex, &a).unwrap().revision,
        a_version
    );
    let mut config = service.state(ClientId::Codex, &a).unwrap().detection;
    config.min_interval_minutes = 20;
    service
        .set_detection(ClientId::Codex, &a, config, &a_version)
        .unwrap();
    service.tick(at + 61_000, false).unwrap();
    assert_eq!(status(&service, &a).next_at, Some(at + 1_200_000));
    assert_eq!(status(&service, &b).next_at, b_due);
    assert_eq!(
        service.state(ClientId::Codex, &b).unwrap().revision,
        b_version
    );
    let restored = Service::new(dir.path(), service.0.gateways.clone()).unwrap();
    restored.tick(at + 62_000, false).unwrap();
    assert_eq!(status(&restored, &a).next_at, Some(at + 1_200_000));
    assert_eq!(status(&restored, &a).recent_requests, Some(65));
    assert_eq!(status(&restored, &b).next_at, b_due);
    business_requests(&restored, &a, 0, at + 122_000);
    restored.tick(at + 122_000, false).unwrap();
    assert_eq!(status(&restored, &a).status, "idle");
    assert_eq!(status(&restored, &a).next_at, None);
    business_requests(&restored, &a, 3, at + 182_000);
    restored.tick(at + 182_000, false).unwrap();
    assert_eq!(
        status(&restored, &a).next_at,
        Some(at + 182_000 + 1_500_000)
    );
}
#[tokio::test]
async fn queued_smart_work_rechecks_calls_before_dispatch_without_using_cooldown() {
    let (dir, service) = fixture();
    let id = add(&service, dir.path(), ClientId::Codex, 1, "responses");
    ua(&service, ClientId::Codex);
    let at = service.now();
    business_requests(&service, &id, 120, at);
    enable_detection(&service, dir.path(), &id, "smart", 5);
    let permit = service.0.jobs.clone().acquire_many_owned(2).await.unwrap();
    service.tick(at + 300_000, false).unwrap();
    let job = service.state(ClientId::Codex, &id).unwrap().jobs[0].clone();
    assert_eq!(job.status, "queued");
    business_requests(&service, &id, 0, at + 300_000);
    drop(permit);
    let result = completed(&service, &job.id).await;
    assert_eq!(result.job.status, "cancelled");
    assert!(result.job.started_at.is_none());
    assert!(
        lock(&service.0.data).saved.detection_timing[&scope_key(ClientId::Codex, &id)]
            .last_started_at
            .is_none()
    );
    service.tick(at + 301_000, false).unwrap();
    assert_eq!(status(&service, &id).status, "idle");
}
#[tokio::test]
async fn started_work_survives_idle_sampling_and_cooldown_uses_start_not_completion() {
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let incoming = entered.clone();
    let gate = release.clone();
    let first = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let port = server(move |r| {
        let first = first.clone();
        let incoming = incoming.clone();
        let gate = gate.clone();
        async move {
            let _ = r.into_body().collect().await;
            if first.swap(false, std::sync::atomic::Ordering::SeqCst) {
                incoming.notify_one();
                gate.notified().await;
            }
            fingerprint_response(&sample("above_99", 0))
        }
    })
    .await;
    let (dir, service) = fixture();
    let at = service.now();
    let id = add(&service, dir.path(), ClientId::Codex, port, "responses");
    ua(&service, ClientId::Codex);
    business_requests(&service, &id, 120, at);
    enable_detection(&service, dir.path(), &id, "smart", 5);
    service.tick(at + 300_000, false).unwrap();
    tokio::time::timeout(Duration::from_secs(5), entered.notified())
        .await
        .unwrap();
    let job = service.state(ClientId::Codex, &id).unwrap().jobs[0].clone();
    assert_eq!(job.started_at, Some(at + 300_000));
    business_requests(&service, &id, 0, at + 420_000);
    service.tick(at + 420_000, false).unwrap();
    assert_eq!(service.detail(&job.id).unwrap().job.status, "running");
    assert_eq!(status(&service, &id).cooldown_until, Some(at + 600_000));
    release.notify_one();
    let ended = completed(&service, &job.id).await;
    assert_eq!(ended.job.status, "completed");
    assert_eq!(
        lock(&service.0.data).saved.detection_timing[&scope_key(ClientId::Codex, &id)].anchor_at,
        ended.job.completed_at
    );
    assert_eq!(
        lock(&service.0.data).saved.detection_timing[&scope_key(ClientId::Codex, &id)]
            .last_started_at,
        job.started_at
    );
    let restored = Service::new(dir.path(), service.0.gateways.clone()).unwrap();
    restored.tick(at + 421_000, false).unwrap();
    assert_eq!(status(&restored, &id).cooldown_until, Some(at + 600_000));
    assert_eq!(status(&restored, &id).status, "idle");
}
#[tokio::test]
async fn fixed_period_below_five_minutes_still_obeys_dispatch_cooldown() {
    let port = server(|r| async {
        let _ = r.into_body().collect().await;
        fingerprint_response(&sample("above_99", 0))
    })
    .await;
    let (dir, service) = fixture();
    let at = service.now();
    let id = add(&service, dir.path(), ClientId::Codex, port, "responses");
    ua(&service, ClientId::Codex);
    enable_detection(&service, dir.path(), &id, "fixed", 5);
    service.tick(at + 60_000, false).unwrap();
    let job = service.state(ClientId::Codex, &id).unwrap().jobs[0].clone();
    assert_eq!(completed(&service, &job.id).await.job.status, "completed");
    service.tick(at + 120_000, false).unwrap();
    assert_eq!(status(&service, &id).next_at, Some(at + 360_000));
    assert_eq!(service.state(ClientId::Codex, &id).unwrap().jobs.len(), 1);
    let state = service.state(ClientId::Codex, &id).unwrap();
    let mut config = state.detection;
    config.interval_minutes = 10;
    service
        .set_detection(ClientId::Codex, &id, config.clone(), &state.revision)
        .unwrap();
    service.tick(at + 121_000, false).unwrap();
    assert_eq!(status(&service, &id).next_at, Some(at + 660_000));
    config.interval_minutes = 1;
    let revision = service.state(ClientId::Codex, &id).unwrap().revision;
    service
        .set_detection(ClientId::Codex, &id, config, &revision)
        .unwrap();
    service.tick(at + 122_000, false).unwrap();
    assert_eq!(status(&service, &id).next_at, Some(at + 360_000));
    let permit = service.0.jobs.clone().acquire_many_owned(2).await.unwrap();
    service.tick(at + 359_999, false).unwrap();
    assert_eq!(service.state(ClientId::Codex, &id).unwrap().jobs.len(), 1);
    service.tick(at + 360_000, false).unwrap();
    let next = service.state(ClientId::Codex, &id).unwrap().jobs[0].clone();
    assert_ne!(next.id, job.id);
    service.cancel(&next.id).unwrap();
    completed(&service, &next.id).await;
    drop(permit);
}
#[tokio::test]
async fn statistics_failure_is_not_zero_and_never_dispatches() {
    let (dir, service) = fixture();
    let at = service.now();
    let id = add(&service, dir.path(), ClientId::Codex, 1, "responses");
    enable_detection(&service, dir.path(), &id, "smart", 5);
    let connection = rusqlite::Connection::open(dir.path().join("usage/usage.sqlite")).unwrap();
    let kind: String = connection
        .query_row(
            "SELECT type FROM sqlite_master WHERE name='records'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    connection
        .execute_batch(&format!(
            "DROP {} records",
            if kind == "view" { "VIEW" } else { "TABLE" }
        ))
        .unwrap();
    service.tick(at + 600_000, false).unwrap();
    let status = status(&service, &id);
    assert_eq!(status.status, "statsUnavailable");
    assert!(status.recent_requests.is_none());
    assert!(status.next_at.is_none());
    assert!(status.error.unwrap().contains("用量数据库操作失败"));
    assert!(service.state(ClientId::Codex, &id).unwrap().jobs.is_empty());
}
#[tokio::test]
async fn independent_migration_preserves_effective_controls_and_does_not_enable_new_providers() {
    let (dir, service) = fixture();
    let a = add(&service, dir.path(), ClientId::Codex, 1, "responses");
    let b = add(&service, dir.path(), ClientId::Codex, 2, "responses");
    let path = dir.path().join("provider-tests.json");
    let old = json!({"schema":1,"detection":{"enabled":true,"mode":"fixed","intervalMinutes":25,"minIntervalMinutes":12,"model":"fixture-global","providers":{a.clone():{"enabled":false,"model":"fixture-local"}}},"jobs":[],"history":[],"bank":{},"degraded":{}});
    std::fs::write(&path, serde_json::to_vec(&old).unwrap()).unwrap();
    let restored = Service::new(dir.path(), service.0.gateways.clone()).unwrap();
    let first = restored.state(ClientId::Codex, &a).unwrap().detection;
    let second = restored.state(ClientId::Codex, &b).unwrap().detection;
    assert!(!first.enabled);
    assert_eq!(first.model, "fixture-local");
    assert!(second.enabled);
    assert_eq!(second.interval_minutes, 25);
    assert_eq!(second.min_interval_minutes, 12);
    restored.tick(restored.now(), false).unwrap();
    assert!(restored.state(ClientId::Codex, &b).unwrap().jobs.is_empty());
    let c = add(&restored, dir.path(), ClientId::Codex, 3, "responses");
    assert!(
        !restored
            .state(ClientId::Codex, &c)
            .unwrap()
            .detection
            .enabled
    );
    let again = Service::new(dir.path(), restored.0.gateways.clone()).unwrap();
    assert!(!again.state(ClientId::Codex, &c).unwrap().detection.enabled);
    let mut disabled = second;
    disabled.enabled = false;
    again
        .set_detection(
            ClientId::Codex,
            &b,
            disabled,
            &again.state(ClientId::Codex, &b).unwrap().revision,
        )
        .unwrap();
    let raw: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(raw["schema"], 3);
    assert_eq!(raw["bank"], json!(SavedBank::default()));
}
#[tokio::test]
async fn endpoint_selection_is_fixed_and_catalogs_are_isolated() {
    let primary = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let hits = primary.clone();
    let primary_port = server(move |_| {
        let hits = hits.clone();
        async move {
            hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            response(
                200,
                "application/json",
                r#"{"data":[{"id":"fixture-primary"}]}"#,
            )
        }
    })
    .await;
    let auxiliary = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let hits = auxiliary.clone();
    let auxiliary_port = server(move |r| {
        let hits = hits.clone();
        async move {
            hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if r.uri().path().ends_with("/models") {
                response(
                    200,
                    "application/json",
                    r#"{"data":[{"id":"fixture-auxiliary"}]}"#,
                )
            } else {
                response(
                    503,
                    "application/json",
                    r#"{"error":{"message":"fixture auxiliary failed"}}"#,
                )
            }
        }
    })
    .await;
    let (dir, service) = fixture();
    let id = add(
        &service,
        dir.path(),
        ClientId::Codex,
        primary_port,
        "responses",
    );
    ua(&service, ClientId::Codex);
    let gateway = service.gateway(ClientId::Codex);
    let endpoint = format!("http://127.0.0.1:{auxiliary_port}/deployment/v1");
    gateway
        .edit(
            serde_json::from_value(json!({"op":"endpointsProvider","id":id,"baseUrls":[endpoint]}))
                .unwrap(),
            &gateway.view().revision,
            dir.path(),
        )
        .unwrap();
    let before = gateway.view().revision;
    let a = service
        .models(ClientId::Codex, &id, &before, None, true)
        .await
        .unwrap();
    let b = service
        .models(ClientId::Codex, &id, &before, Some(&endpoint), true)
        .await
        .unwrap();
    assert_eq!(a.default_model, "fixture-primary");
    assert_eq!(b.default_model, "fixture-auxiliary");
    let mut request = request(&service, ClientId::Codex, &id);
    request.model.clear();
    request.endpoint = Some(endpoint.clone());
    let job = service.start(request.clone()).unwrap();
    assert_eq!(job.requested_model, "fixture-auxiliary");
    assert_eq!(job.endpoint, Some(endpoint.clone()));
    let result = completed(&service, &job.id).await;
    assert_eq!(result.job.status, "failed");
    assert!(result
        .output
        .unwrap()
        .error
        .unwrap()
        .body
        .contains("fixture auxiliary failed"));
    assert_eq!(primary.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(auxiliary.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert_eq!(gateway.view().revision, before);
    request.endpoint = Some("http://127.0.0.1:1/unconfigured".into());
    assert!(service
        .start(request.clone())
        .err()
        .unwrap()
        .message
        .contains("未配置"));
    let cached = service
        .models(ClientId::Codex, &id, &before, None, false)
        .await
        .unwrap();
    assert_eq!(cached.default_model, "fixture-primary");
    let permit = service.0.jobs.clone().acquire_many_owned(2).await.unwrap();
    request.endpoint = Some(endpoint);
    let queued = service.start(request).unwrap();
    gateway
        .edit(
            Edit::EndpointsProvider {
                id: id.clone(),
                base_urls: vec![],
            },
            &gateway.view().revision,
            dir.path(),
        )
        .unwrap();
    drop(permit);
    let stopped = completed(&service, &queued.id).await;
    assert!(stopped.job.started_at.is_none());
    assert!(stopped.job.error.unwrap().contains("未配置"));
    assert_eq!(primary.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(auxiliary.load(std::sync::atomic::Ordering::SeqCst), 2);
}
#[tokio::test]
async fn connection_errors_keep_complete_body_for_all_protocols_and_do_not_persist_it() {
    for (client, protocol, status, mime, body) in [
        (ClientId::Codex, "responses", 400, "application/json", json!({"error":{"message":format!("fixture detail {} end-marker fixture-test-token", "x".repeat(40_000)),"code":"bad_model","api_key":"fixture-secret"}}).to_string()),
        (ClientId::Claude, "messages", 200, "text/event-stream", "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"fixture messages failure end-marker\"}}\n\n".into()),
        (ClientId::Grok, "chat_completions", 503, "text/html", "<h1>fixture upstream failed end-marker</h1>".into()),
        (ClientId::Dsh, "chat_completions", 200, "application/json", "{\"error\":{\"message\":\"fixture embedded failure end-marker\"}}".into()),
    ] {
        let port = server(move |_| { let body = body.clone(); async move { response(status, mime, body) }}).await;
        let (dir, service) = fixture(); let id = add(&service, dir.path(), client, port, protocol); ua(&service, client);
        let job = service.start(request(&service, client, &id)).unwrap();
        let done = completed(&service, &job.id).await;
        assert_eq!(done.job.status, "failed");
        let detail = done.output.unwrap().error.unwrap();
        assert_eq!(detail.http_status, Some(status)); assert!(detail.body.contains("end-marker")); assert!(!detail.truncated);
        assert!(!detail.body.contains("fixture-test-token")); assert!(!detail.body.contains("fixture-secret"));
        let saved = std::fs::read_to_string(dir.path().join("provider-tests.json")).unwrap(); assert!(!saved.contains("end-marker"));
    }
}
#[tokio::test]
async fn interrupted_response_keeps_network_reason_and_redacted_partial_body() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        for _ in 0..2 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = vec![];
            loop {
                let mut chunk = [0; 4096];
                let count = socket.read(&mut chunk).await.unwrap();
                assert!(count > 0);
                request.extend_from_slice(&chunk[..count]);
                if let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            line.strip_prefix("content-length:")?
                                .trim()
                                .parse::<usize>()
                                .ok()
                        })
                        .unwrap_or(0);
                    if request.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 500\r\nConnection: close\r\n\r\n{\"error\":\"fixture partial fixture-test-token").await.unwrap();
            socket.shutdown().await.unwrap();
        }
    });
    let (dir, service) = fixture();
    let id = add(&service, dir.path(), ClientId::Codex, port, "responses");
    ua(&service, ClientId::Codex);
    seed_catalog(&service, &id, &["fixture-model"]);
    let models = service
        .models(
            ClientId::Codex,
            &id,
            &service.gateway(ClientId::Codex).view().revision,
            None,
            true,
        )
        .await
        .unwrap();
    assert_eq!(models.models[0].id, "fixture-model");
    let job = service
        .start(request(&service, ClientId::Codex, &id))
        .unwrap();
    let done = completed(&service, &job.id).await;
    assert_eq!(done.job.status, "failed");
    for detail in [
        models.error_detail.unwrap(),
        done.output.unwrap().error.unwrap(),
    ] {
        assert_eq!(detail.http_status, Some(200));
        assert!(detail.truncated);
        assert!(detail.message.contains("body"));
        assert!(detail.body.contains("fixture partial"));
        assert!(!detail.body.contains("fixture-test-token"));
    }
    server.await.unwrap();
    assert!(
        !std::fs::read_to_string(dir.path().join("provider-tests.json"))
            .unwrap()
            .contains("fixture partial")
    );
}
#[tokio::test]
async fn fingerprint_errors_retain_plain_http_and_stream_errors_per_group() {
    for (status, mime, body) in [
        (401, "text/plain", format!("fixture authentication {} end-marker fixture-test-token", "x".repeat(40_000))),
        (200, "text/event-stream", "data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"invalid_api_key\",\"message\":\"fixture embedded end-marker\"}}}\n\n".into()),
        (200, "text/html", "<h1>fixture failure end-marker</h1>".into()),
        (200, "text/event-stream", "data: fixture malformed event end-marker\n\n".into()),
    ] {
        let port = server(move |_| { let body = body.clone(); async move { response(status, mime, body) }}).await;
        let (dir, service) = fixture(); let id = add(&service, dir.path(), ClientId::Codex, port, "responses"); ua(&service, ClientId::Codex);
        let job = service.start(fingerprint_request(&service, &id, 1)).unwrap(); let done = completed(&service, &job.id).await;
        assert_eq!(done.job.status, "failed");
        let output = done.output.unwrap();
        assert_eq!(output.group_errors.len(), 3);
        for detail in output.group_errors.values() { assert_eq!(detail.http_status, Some(status)); assert!(detail.body.contains("end-marker")); assert!(!detail.body.contains("fixture-test-token")); }
        assert!(!std::fs::read_to_string(dir.path().join("provider-tests.json")).unwrap().contains("end-marker"));
    }
}
#[tokio::test]
async fn manual_detection_changes_prevent_old_automatic_disposition() {
    let entered = Arc::new(tokio::sync::Notify::new());
    let incoming = entered.clone();
    let gate = Arc::new(tokio::sync::Notify::new());
    let wait = gate.clone();
    let port = server(move |_| {
        let incoming = incoming.clone();
        let wait = wait.clone();
        async move {
            incoming.notify_one();
            wait.notified().await;
            fingerprint_response(&sample("automatic_first_group", 0))
        }
    })
    .await;
    let (dir, service) = fixture();
    let at = service.now();
    let id = add(&service, dir.path(), ClientId::Codex, port, "responses");
    ua(&service, ClientId::Codex);
    enable_detection(&service, dir.path(), &id, "fixed", 5);
    service.tick(at + 60_000, false).unwrap();
    tokio::time::timeout(Duration::from_secs(5), entered.notified())
        .await
        .unwrap();
    let state = service.state(ClientId::Codex, &id).unwrap();
    let job = state.jobs[0].clone();
    let mut detection = state.detection;
    detection.enabled = false;
    service
        .set_detection(ClientId::Codex, &id, detection, &state.revision)
        .unwrap();
    gate.notify_waiters();
    let result = completed(&service, &job.id).await;
    assert_eq!(result.job.status, "cancelled");
    assert!(service
        .state(ClientId::Codex, &id)
        .unwrap()
        .degraded
        .is_empty());
    assert!(service.gateway(ClientId::Codex).test_candidates().unwrap()[0].queued);
}
#[tokio::test]
async fn aggregate_downgrade_is_guarded_and_marked_providers_never_recheck() {
    let (dir, service) = fixture();
    let at = service.now();
    let id = add(&service, dir.path(), ClientId::Codex, 1, "responses");
    ua(&service, ClientId::Codex);
    enable_detection(&service, dir.path(), &id, "fixed", 5);
    let permit = service.0.jobs.clone().acquire_many_owned(2).await.unwrap();
    service.tick(at + 60_000, false).unwrap();
    let job = service.state(ClientId::Codex, &id).unwrap().jobs[0].clone();
    let report = modeltrace::analyze(
        &vec![sample("automatic_first_group", 0); 3],
        &modeltrace::bundled().unwrap(),
    )
    .unwrap();
    service
        .update(&job.id, |job, _| {
            for group in &mut job.groups {
                group.status = "completed".into();
            }
            job.report = Some(report);
        })
        .unwrap();
    service.apply_degradation(&job.id);
    assert!(service.state(ClientId::Codex, &id).unwrap().degraded[&id].queue_removed);
    service.cancel(&job.id).unwrap();
    completed(&service, &job.id).await;
    drop(permit);
    let gateway = service.gateway(ClientId::Codex);
    gateway
        .edit(
            Edit::QueueProvider {
                id: id.clone(),
                queued: true,
            },
            &gateway.view().revision,
            dir.path(),
        )
        .unwrap();
    let jobs = service.state(ClientId::Codex, &id).unwrap().jobs.len();
    service.tick(at + 3_600_000, false).unwrap();
    assert_eq!(status(&service, &id).status, "degraded");
    assert_eq!(
        service.state(ClientId::Codex, &id).unwrap().jobs.len(),
        jobs
    );
}

fn seed_client_catalog(service: &Service, client: ClientId, id: &str, model: &str) {
    let endpoint = service.gateway(client).test_endpoint(id, None).unwrap();
    let mut data = lock(&service.0.data);
    let mut next = data.saved.clone();
    next.models.insert(
        Service::models_key(client, id, &endpoint),
        Models {
            models: vec![Model {
                id: model.into(),
                name: model.into(),
            }],
            default_model: model.into(),
            fetched_at: Some(service.now()),
            ..Default::default()
        },
    );
    service.save(&mut data, next).unwrap();
}
fn lm_sample(index: usize) -> String {
    let fixture: Value = serde_json::from_str(include_str!("fixtures/lm-analyze.json")).unwrap();
    fixture["cases"][0]["outputs"][index]["text"]
        .as_str()
        .unwrap()
        .into()
}
fn protocol_response(protocol: &str, text: &str, usage: Value) -> Response<Full<Bytes>> {
    let value = match protocol {
        "messages" => {
            json!({"type":"message","model":"fixture-returned","content":[{"type":"thinking","thinking":"hidden fixture"},{"type":"text","text":text}],"stop_reason":"end_turn","usage":usage})
        }
        "chat_completions" => {
            json!({"model":"fixture-returned","choices":[{"index":0,"message":{"content":text},"finish_reason":"stop"}],"usage":usage})
        }
        _ => {
            json!({"object":"response","model":"fixture-returned","status":"completed","output_text":text,"usage":usage})
        }
    };
    response(200, "application/json", value.to_string())
}
#[tokio::test]
async fn lm_single_and_three_samples_use_all_four_native_protocols_and_keep_snapshots() {
    for (client, protocol) in [
        (ClientId::Codex, "responses"),
        (ClientId::Claude, "messages"),
        (ClientId::Grok, "chat_completions"),
        (ClientId::Dsh, "chat_completions"),
    ] {
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let captured = bodies.clone();
        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = hits.clone();
        let port = server(move |r| {
            let captured = captured.clone();
            let count = count.clone();
            async move {
                let n = count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let path = r.uri().path().to_owned();
                let headers = r.headers().clone();
                let body: Value =
                    serde_json::from_slice(&r.into_body().collect().await.unwrap().to_bytes())
                        .unwrap();
                lock(&captured).push((path, headers, body));
                protocol_response(protocol, &lm_sample(n % 3), json!({}))
            }
        })
        .await;
        let (dir, service) = fixture();
        let id = add(&service, dir.path(), client, port, protocol);
        ua(&service, client);
        seed_client_catalog(&service, client, &id, "fixture-model");
        if client == ClientId::Dsh {
            let g = service.gateway(client);
            g.edit(serde_json::from_value(json!({"op":"dshModelsProvider","id":id,"mapping":{"fixture-model":"deepseek-v4-pro"}})).unwrap(),&g.view().revision,dir.path()).unwrap();
        }
        for samples in [1, 3] {
            service
                .set_platform(
                    client,
                    PlatformSettings {
                        engine: Engine::LmDetector,
                        lm_detector_samples: samples,
                        ..PlatformSettings::default()
                    },
                )
                .unwrap();
            let permits = service.0.jobs.clone().acquire_many_owned(2).await.unwrap();
            let mut req = request(&service, client, &id);
            req.kind = Kind::Fingerprint;
            req.concurrency = 3;
            let job = service.start(req).unwrap();
            assert_eq!(job.sample_count, samples);
            assert_eq!(job.engine, Engine::LmDetector);
            let resource = service.lm_resource().unwrap();
            {
                let data = lock(&service.0.data);
                let context = &data.contexts[&job.id];
                assert!(context.bank.is_none());
                assert!(Arc::ptr_eq(context.lm.as_ref().unwrap(), &resource));
            }
            // Reloading the shared cache cannot replace a queued job's resource.
            *lock(&service.0.lm) = None;
            assert!(!Arc::ptr_eq(&service.lm_resource().unwrap(), &resource));
            assert_eq!(
                job.resource_version["revision"],
                "567e11d65a48d4fbba2713cc4dfbe685cdd05a73"
            );
            service
                .set_platform(
                    client,
                    PlatformSettings {
                        engine: Engine::ModelTrace,
                        ..PlatformSettings::default()
                    },
                )
                .unwrap();
            assert_eq!(
                lock(&service.0.data).contexts[&job.id].request.concurrency,
                samples
            );
            drop(permits);
            let done = completed(&service, &job.id).await;
            assert_eq!(done.job.status, "completed", "{:?}", done.job.error);
            assert_eq!(done.job.report.as_ref().unwrap()["used_outputs"], samples);
            assert!(done.job.groups.iter().all(|g| g.status == "completed"));
            assert_eq!(
                done.job.report.as_ref().unwrap()["bank_version"],
                job.resource_version
            );
            assert!(!lock(&service.0.data).contexts.contains_key(&job.id));
            if samples == 1 {
                assert!(done.job.report.as_ref().unwrap()["probability"].is_null());
                assert_eq!(done.job.report.as_ref().unwrap()["initial"], true);
            }
            assert!(service.state(client, &id).unwrap().degraded.is_empty());
        }
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 4);
        for (path, headers, body) in lock(&bodies).iter() {
            assert!(path.ends_with(match protocol {
                "messages" => "/messages",
                "chat_completions" => "/chat/completions",
                _ => "/responses",
            }));
            assert_eq!(headers["user-agent"], "fixture-business/1.0 exact  UA");
            if protocol == "messages" {
                assert_eq!(body["max_tokens"], 8192);
            }
            if client == ClientId::Dsh {
                assert_eq!(body["model"], "deepseek-v4-pro");
            }
            if client == ClientId::Codex {
                assert_eq!(
                    body["client_metadata"]["x-codex-installation-id"],
                    INSTALLATION_ID
                );
            }
        }
    }
}
#[tokio::test]
async fn scheduled_single_sample_dequeues_but_partial_three_does_not() {
    for samples in [1, 3] {
        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = hits.clone();
        let port = server(move |_| {
            let count = count.clone();
            async move {
                let n = count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                protocol_response(
                    "messages",
                    &if n == 0 { lm_sample(0) } else { "1, 2".into() },
                    json!({}),
                )
            }
        })
        .await;
        let (dir, service) = fixture();
        let client = ClientId::Claude;
        let id = add(&service, dir.path(), client, port, "messages");
        ua(&service, client);
        let g = service.gateway(client);
        g.edit(
            Edit::QueueProvider {
                id: id.clone(),
                queued: true,
            },
            &g.view().revision,
            dir.path(),
        )
        .unwrap();
        seed_client_catalog(&service, client, &id, "claude-opus-4.8");
        service
            .set_platform(
                client,
                PlatformSettings {
                    engine: Engine::LmDetector,
                    lm_detector_samples: samples,
                    ..PlatformSettings::default()
                },
            )
            .unwrap();
        let state = service.state(client, &id).unwrap();
        service
            .set_detection(
                client,
                &id,
                Detection {
                    enabled: true,
                    model: "claude-opus-4.8".into(),
                    mode: "fixed".into(),
                    interval_minutes: 1,
                    ..state.detection
                },
                &state.revision,
            )
            .unwrap();
        let at = service.now();
        service.tick(at, false).unwrap();
        service.tick(at + 60_000, false).unwrap();
        let job = service.state(client, &id).unwrap().jobs[0].clone();
        let done = completed(&service, &job.id).await;
        assert_eq!(done.job.report.as_ref().unwrap()["used_outputs"], 1);
        let state = service.state(client, &id).unwrap();
        assert_eq!(state.degraded.is_empty(), samples == 3);
        assert_eq!(g.test_candidates().unwrap()[0].queued, samples == 3);
        if samples == 1 {
            assert!(state.degraded[&id].probability.is_none());
        }
        assert!(service.state(ClientId::Codex, &id).is_err());
    }
}

#[tokio::test]
async fn lm_mixed_evidence_only_dequeues_automatic_jobs_in_their_own_platform() {
    for client in [
        ClientId::Codex,
        ClientId::Claude,
        ClientId::Grok,
        ClientId::Dsh,
    ] {
        let (dir, service) = fixture();
        let protocol = if client == ClientId::Claude {
            "messages"
        } else if client == ClientId::Codex {
            "responses"
        } else {
            "chat_completions"
        };
        let id = add(&service, dir.path(), client, 1, protocol);
        ua(&service, client);
        let gateway = service.gateway(client);
        let requested = if client == ClientId::Dsh {
            "fixture-mapped-model"
        } else {
            "gpt-6-sol"
        };
        if client == ClientId::Dsh {
            gateway.edit(serde_json::from_value(json!({"op":"dshModelsProvider","id":id,"mapping":{(requested):"gpt-6-sol"}})).unwrap(), &gateway.view().revision, dir.path()).unwrap();
        }
        gateway
            .edit(
                Edit::QueueProvider {
                    id: id.clone(),
                    queued: true,
                },
                &gateway.view().revision,
                dir.path(),
            )
            .unwrap();
        seed_client_catalog(&service, client, &id, requested);
        service
            .set_platform(
                client,
                PlatformSettings {
                    engine: Engine::LmDetector,
                    lm_detector_samples: 3,
                    ..PlatformSettings::default()
                },
            )
            .unwrap();
        let state = service.state(client, &id).unwrap();
        service
            .set_detection(
                client,
                &id,
                Detection {
                    enabled: true,
                    model: requested.into(),
                    mode: "fixed".into(),
                    ..state.detection
                },
                &state.revision,
            )
            .unwrap();
        let permits = service.0.jobs.clone().acquire_many_owned(2).await.unwrap();
        for automatic in [false, true] {
            let mut req = request(&service, client, &id);
            req.kind = Kind::Fingerprint;
            req.model = requested.into();
            let job = service.start_job(req, automatic).unwrap();
            assert_eq!(job.forwarded_model, "gpt-6-sol");
            let report = |model: &str, count| json!({"prediction":model,"prediction_name":model,"used_outputs":count,"probability":null,"decision":if count == 1 {"partial"} else {"scored"}});
            service
                .update(&job.id, |job, _| {
                    job.report = Some(report("gpt-6-sol", 3));
                    for group in &mut job.groups {
                        group.status = "completed".into();
                        group.report = Some(report(
                            if group.index == 1 {
                                "grok-4.7"
                            } else {
                                "gpt-6-sol"
                            },
                            1,
                        ));
                    }
                    job.groups[0].report = Some(report("fixture-unknown", 1));
                })
                .unwrap();
            service.apply_degradation(&job.id);
            assert!(service.state(client, &id).unwrap().degraded.is_empty());
            assert!(gateway.test_candidates().unwrap()[0].queued);
            service
                .update(&job.id, |job, _| {
                    job.groups[0].report = Some(report("gpt-6-sol", 1))
                })
                .unwrap();
            service.apply_degradation(&job.id);
            let state = service.state(client, &id).unwrap();
            assert_eq!(state.degraded.is_empty(), !automatic);
            assert_eq!(gateway.test_candidates().unwrap()[0].queued, !automatic);
            if automatic {
                let mark = &state.degraded[&id];
                assert!(mark.queue_removed);
                assert_eq!(mark.predicted_model, "gpt-6-sol");
                assert!(matches!(mark.basis, Some(DegradationBasis::SampleFamily)));
                assert_eq!(mark.group_indexes, vec![2]);
                assert_eq!(
                    lock(&service.0.data)
                        .saved
                        .degraded
                        .keys()
                        .cloned()
                        .collect::<Vec<_>>(),
                    vec![scope_key(client, &id)]
                );
            }
            service.cancel(&job.id).unwrap();
            completed(&service, &job.id).await;
        }
        drop(permits);
    }
}
#[tokio::test]
async fn tokenizer_reads_late_usage_and_preserves_zero_and_message_cache_counts() {
    for (protocol,body,expected) in [
        ("chat_completions","data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"OK\"},\"finish_reason\":\"stop\"}]}\n\ndata: {\"choices\":[],\"usage\":{\"prompt_tokens\":0}}\n\ndata: [DONE]\n\n",Some(0)),
        ("messages","event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"model\":\"fixture\",\"usage\":{\"input_tokens\":2,\"cache_read_input_tokens\":7,\"cache_creation_input_tokens\":3}}}\n\nevent: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"hidden\"}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"OK\"}}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"input_tokens\":null},\"delta\":{\"stop_reason\":\"end_turn\"}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",Some(12)),
        ("responses","data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output_text\":\"OK\",\"usage\":{\"input_tokens\":17}}}\n\n",Some(17)),
        ("responses","data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output_text\":\"OK\"}}\n\n",None),
    ] {
        let port=server(move |_|async move{response(200,"text/event-stream",body)}).await;
        let target=fingerprint_stream::Target{url:format!("http://127.0.0.1:{port}"),headers:Default::default(),proxy:None,model:"fixture".into(),oauth:false,protocol:protocol.into(),client_metadata:None};
        let mut diag=json!({});
        let (text,_)=fingerprint_stream::execute_body(&target,&target.lm_payload("fixture").unwrap(),0,&target.client().unwrap(),None,&mut diag,true).await.unwrap();
        assert_eq!(text,"OK");assert_eq!(diag["input_tokens"].as_i64(),expected);
    }
}
#[tokio::test]
async fn tokenizer_failure_leaves_numeric_result_and_is_not_replayed_by_group_retry() {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = calls.clone();
    let port = server(move |_| {
        let seen = seen.clone();
        async move {
            let n = seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let text = if n == 0 { lm_sample(0) } else { "OK".into() };
            protocol_response("responses", &text, json!({}))
        }
    })
    .await;
    let (dir, service) = fixture();
    let id = add(&service, dir.path(), ClientId::Codex, port, "responses");
    ua(&service, ClientId::Codex);
    seed_client_catalog(&service, ClientId::Codex, &id, "fixture-model");
    service
        .set_platform(
            ClientId::Codex,
            PlatformSettings {
                engine: Engine::LmDetector,
                ..PlatformSettings::default()
            },
        )
        .unwrap();
    let mut req = request(&service, ClientId::Codex, &id);
    req.kind = Kind::Fingerprint;
    req.tokenizer = true;
    let job = service.start(req).unwrap();
    let done = completed(&service, &job.id).await;
    assert_eq!(done.job.status, "completed");
    assert_eq!(done.job.report.unwrap()["used_outputs"], 1);
    assert_eq!(done.job.tokenizer_result.unwrap()["status"], "failed");
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert!(!done.job.can_retry);
}
#[test]
fn schema_three_migrates_prompt_from_latest_connection_and_keeps_scoped_history() {
    let (dir, service) = fixture();
    let id = add(&service, dir.path(), ClientId::Codex, 1, "responses");
    let path = dir.path().join("provider-tests.json");
    let mut saved = json!(Persisted::default());
    saved["schema"] = 2.into();
    saved.as_object_mut().unwrap().remove("connectionPrompt");
    saved["connectionPrompts"] =
        json!({format!("Codex:{id}"):"old fixture", "Claude:other":"latest fixture"});
    let job = |client: &str, provider: &str, at: i64| json!({"id":format!("fixture-{at}"),"clientId":client,"providerId":provider,"providerName":"Fixture","providerVersion":"fixture","kind":"connection","mode":"text","requestedModel":"fixture-model","forwardedModel":"fixture-model","returnedModel":null,"status":"completed","createdAt":at,"startedAt":null,"completedAt":at,"elapsedMs":0,"firstByteMs":null,"error":null,"report":null,"groups":[],"automatic":false,"batchId":null,"canRetry":false,"completionReason":null});
    saved["jobs"] = json!([job("codex", &id, 1), job("claude", "other", 2)]);
    saved["detection"] = json!({id.clone():Detection{enabled:true,..Default::default()}});
    saved["detectionTiming"] = json!({id.clone():{"anchorAt":123,"lastStartedAt":100}});
    std::fs::write(&path, saved.to_string()).unwrap();
    let restored = Service::new(dir.path(), service.0.gateways.clone()).unwrap();
    let state = restored.state(ClientId::Codex, &id).unwrap();
    assert_eq!(state.connection_prompt, "latest fixture");
    assert_eq!(state.jobs[0].engine, Engine::ModelTrace);
    assert_eq!(state.jobs[0].sample_count, 3);
    assert!(state.detection.enabled);
    let saved = &lock(&restored.0.data).saved;
    assert_eq!(saved.schema, 3);
    assert_eq!(
        saved.detection_timing[&scope_key(ClientId::Codex, &id)].anchor_at,
        Some(123)
    );
    assert!(!saved
        .detection
        .contains_key(&scope_key(ClientId::Claude, &id)));
    let mut next = saved.clone();
    next.detection.insert(
        scope_key(ClientId::Claude, &id),
        Detection {
            model: "claude-opus-4.8".into(),
            ..Detection::for_client(ClientId::Claude)
        },
    );
    assert_eq!(
        config_revision(saved, ClientId::Codex, &id),
        config_revision(&next, ClientId::Codex, &id)
    );
    assert_ne!(
        config_revision(saved, ClientId::Claude, &id),
        config_revision(&next, ClientId::Claude, &id)
    );
}

#[tokio::test]
async fn automatic_identity_change_prevents_disposition_and_dequeue_failure_keeps_mark() {
    let (dir, service) = fixture();
    let at = service.now();
    let id = add(&service, dir.path(), ClientId::Codex, 1, "responses");
    ua(&service, ClientId::Codex);
    enable_detection(&service, dir.path(), &id, "fixed", 5);
    let permits = service.0.jobs.clone().acquire_many_owned(2).await.unwrap();
    service.tick(at + 60_000, false).unwrap();
    let job = service.state(ClientId::Codex, &id).unwrap().jobs[0].clone();
    let report = modeltrace::analyze(
        &vec![sample("automatic_first_group", 0); 3],
        &modeltrace::bundled().unwrap(),
    )
    .unwrap();
    service
        .update(&job.id, |job, _| {
            for group in &mut job.groups {
                group.status = "completed".into();
            }
            job.report = Some(report);
        })
        .unwrap();
    identity(
        service.gateway(ClientId::Codex),
        b"fixture-changed/2.0",
        "fixture-changed",
    );
    service.apply_degradation(&job.id);
    assert!(service
        .state(ClientId::Codex, &id)
        .unwrap()
        .degraded
        .is_empty());
    ua(&service, ClientId::Codex);
    let path = dir.path().join("codex/gateway.json");
    let mut raw = std::fs::read(&path).unwrap();
    raw.push(b' ');
    std::fs::write(&path, raw).unwrap();
    service.apply_degradation(&job.id);
    let state = service.state(ClientId::Codex, &id).unwrap();
    let mark = &state.degraded[&id];
    assert!(!mark.queue_removed);
    assert!(mark.error.is_some());
    assert!(service.gateway(ClientId::Codex).test_candidates().unwrap()[0].queued);
    service.cancel(&job.id).unwrap();
    completed(&service, &job.id).await;
    drop(permits);
}

#[test]
fn detection_controls_and_marks_are_isolated_for_identical_provider_ids() {
    let (dir, service) = fixture();
    let id = add(&service, dir.path(), ClientId::Codex, 1, "responses");
    add(&service, dir.path(), ClientId::Claude, 2, "messages");
    let path = dir.path().join("claude/gateway.json");
    let mut raw: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    raw["providers"][0]["id"] = id.clone().into();
    std::fs::write(&path, raw.to_string()).unwrap();
    let codex = Gateway::new(dir.path().join("codex")).unwrap();
    codex.observe_home(dir.path());
    let claude = codex.companion(dir.path().join("claude")).unwrap();
    let grok = codex.grok_companion(dir.path().join("grok")).unwrap();
    let dsh = codex.dsh_companion(dir.path().join("dsh")).unwrap();
    let service = Service::new(dir.path(), [codex, claude, grok, dsh]).unwrap();
    let codex = service.state(ClientId::Codex, &id).unwrap();
    let claude = service.state(ClientId::Claude, &id).unwrap();
    assert_eq!(codex.platform.engine, Engine::ModelTrace);
    assert_eq!(claude.platform.engine, Engine::LmDetector);
    assert!(claude.detection.model.is_empty());
    assert!(!claude.detection.enabled);
    service
        .set_detection(
            ClientId::Claude,
            &id,
            Detection {
                model: "claude-opus-4.8".into(),
                ..claude.detection
            },
            &claude.revision,
        )
        .unwrap();
    assert_eq!(
        service.state(ClientId::Codex, &id).unwrap().revision,
        codex.revision
    );
    {
        let mut data = lock(&service.0.data);
        let mut next = data.saved.clone();
        for client in [ClientId::Codex, ClientId::Claude] {
            next.degraded.insert(
                scope_key(client, &id),
                Degradation {
                    provider_id: id.clone(),
                    job_id: "fixture".into(),
                    detected_at: 1,
                    requested_model: "fixture".into(),
                    predicted_model: "fixture".into(),
                    probability: None,
                    engine: Engine::LmDetector,
                    queue_removed: false,
                    basis: None,
                    group_indexes: vec![],
                    error: None,
                },
            );
        }
        service.save(&mut data, next).unwrap();
    }
    let state = service.state(ClientId::Claude, &id).unwrap();
    service
        .clear_mark(ClientId::Claude, &id, &state.revision)
        .unwrap();
    assert!(service
        .state(ClientId::Claude, &id)
        .unwrap()
        .degraded
        .is_empty());
    assert!(!service
        .state(ClientId::Codex, &id)
        .unwrap()
        .degraded
        .is_empty());
}

#[test]
fn business_counts_are_isolated_for_each_client_with_the_same_provider_id() {
    use crate::usage::model::{Attempt, Record};
    let (_dir, service) = fixture();
    let at = service.now();
    let clients = [
        ClientId::Codex,
        ClientId::Claude,
        ClientId::Grok,
        ClientId::Dsh,
    ];
    let mut records = Vec::new();
    for (index, client) in clients.iter().enumerate() {
        for n in 0..=index {
            records.push(Record {
                id: format!("fixture-{}-{n}", client_key(*client)),
                client: client_key(*client).into(),
                source: "proxy".into(),
                started_at: at - 1000,
                finished_at: Some(at - 999),
                completed: true,
                attempts: vec![Attempt {
                    id: format!("fixture-attempt-{index}-{n}"),
                    provider: Some("fixture-shared".into()),
                    started_at: at - 1000,
                    status: Some(200),
                    outcome: "success".into(),
                    ..Default::default()
                }],
                ..Default::default()
            });
        }
    }
    service
        .gateway(ClientId::Codex)
        .test_usage()
        .query(|store| store.write_batch(&records, None))
        .unwrap();
    for (index, client) in clients.iter().enumerate() {
        assert_eq!(
            service
                .gateway(*client)
                .detection_request_counts(at)
                .unwrap()["fixture-shared"],
            (index + 1) as u64
        );
    }
}

#[tokio::test]
async fn cancelling_tokenizer_keeps_numeric_ranking_and_stops_following_requests() {
    let called = Arc::new(tokio::sync::Notify::new());
    let notify = called.clone();
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = calls.clone();
    let port = server(move |_| {
        let notify = notify.clone();
        let seen = seen.clone();
        async move {
            let n = seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if n == 0 {
                protocol_response("responses", &lm_sample(0), json!({}))
            } else {
                notify.notify_one();
                std::future::pending::<Response<Full<Bytes>>>().await
            }
        }
    })
    .await;
    let (dir, service) = fixture();
    let id = add(&service, dir.path(), ClientId::Codex, port, "responses");
    ua(&service, ClientId::Codex);
    seed_client_catalog(&service, ClientId::Codex, &id, "fixture-model");
    service
        .set_platform(
            ClientId::Codex,
            PlatformSettings {
                engine: Engine::LmDetector,
                ..Default::default()
            },
        )
        .unwrap();
    let mut req = request(&service, ClientId::Codex, &id);
    req.kind = Kind::Fingerprint;
    req.tokenizer = true;
    let job = service.start(req).unwrap();
    tokio::time::timeout(Duration::from_secs(10), called.notified())
        .await
        .unwrap();
    service.cancel(&job.id).unwrap();
    let done = completed(&service, &job.id).await;
    assert_eq!(done.job.status, "cancelled");
    assert_eq!(done.job.report.unwrap()["used_outputs"], 1);
    assert_eq!(done.job.tokenizer_result.unwrap()["status"], "cancelled");
    assert!(!lock(&service.0.data).contexts.contains_key(&job.id));
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
}

#[tokio::test]
async fn tokenizer_completes_serial_probes_with_two_controls_and_bounded_requests() {
    let bundle: Value = serde_json::from_reader(flate2::read::GzDecoder::new(
        include_bytes!("../../resources/lm-detector/bundle.json.gz").as_slice(),
    ))
    .unwrap();
    let bank = &bundle["tokenizer"];
    let prefix = bank["wrapper"]["prefix"].as_str().unwrap();
    let suffix = bank["wrapper"]["suffix"].as_str().unwrap();
    let mut counts = HashMap::from([(format!("{prefix}{suffix}"), 20i64)]);
    for (i, probe) in bank["probes"].as_array().unwrap().iter().enumerate() {
        counts.insert(
            format!("{prefix}{}{suffix}", probe["text"].as_str().unwrap()),
            20 + bank["classes"][0]["counts"][i].as_i64().unwrap(),
        );
    }
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = calls.clone();
    let counts = Arc::new(counts);
    let port = server(move |r| {
        let seen = seen.clone();
        let counts = counts.clone();
        async move {
            let n = seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if n == 0 {
                return protocol_response("responses", &lm_sample(0), json!({}));
            }
            assert!(n <= 22, "tokenizer exceeded its request bound");
            let body: Value =
                serde_json::from_slice(&r.into_body().collect().await.unwrap().to_bytes()).unwrap();
            let prompt = body["input"][0]["content"].as_str().unwrap();
            protocol_response("responses", "OK", json!({"input_tokens":counts[prompt]}))
        }
    })
    .await;
    let (dir, service) = fixture();
    let id = add(&service, dir.path(), ClientId::Codex, port, "responses");
    ua(&service, ClientId::Codex);
    seed_client_catalog(&service, ClientId::Codex, &id, "fixture-model");
    service
        .set_platform(
            ClientId::Codex,
            PlatformSettings {
                engine: Engine::LmDetector,
                ..Default::default()
            },
        )
        .unwrap();
    let mut req = request(&service, ClientId::Codex, &id);
    req.kind = Kind::Fingerprint;
    req.tokenizer = true;
    let job = service.start(req).unwrap();
    let done = completed(&service, &job.id).await;
    assert_eq!(done.job.status, "completed");
    assert_eq!(done.job.report.unwrap()["used_outputs"], 1);
    let result = done.job.tokenizer_result.unwrap();
    assert_eq!(result["status"], "completed");
    let steps = result["steps"].as_array().unwrap();
    assert!((6..=22).contains(&steps.len()));
    assert_eq!(steps.iter().filter(|s| s["probe"].is_null()).count(), 2);
    assert!(steps.first().unwrap()["probe"].is_null() && steps.last().unwrap()["probe"].is_null());
    assert_eq!(result["verdict"]["top"]["id"], "o200k");
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        steps.len() + 1
    );
}

fn replace_modeltrace_resource(service: &Service) {
    let mut data = lock(&service.0.data);
    let (bank, mut version) = data.bank.snapshot.as_ref().unwrap().as_ref().clone();
    version["revision"] = "fixture-updated-resource".into();
    let manifest = modeltrace::manifest();
    let saved = SavedBank {
        provenance: Some(json!({
            "core_blob": manifest["files"]["core"]["gitBlob"],
            "challenge_blob": manifest["files"]["challenge"]["gitBlob"],
            "stored_sha256": compat::sha256(bank.to_string()),
        })),
        data: Some(bank),
        version: Some(version),
        ..Default::default()
    };
    data.bank = bank::Resource::load(&saved);
}

#[tokio::test]
async fn modeltrace_shares_frozen_resources_for_running_and_queued_jobs() {
    let gate = Arc::new(Semaphore::new(0));
    let called = Arc::new(tokio::sync::Notify::new());
    let mut ports = vec![];
    for _ in 0..2 {
        let gate = gate.clone();
        let called = called.clone();
        ports.push(
            server(move |_| {
                let gate = gate.clone();
                let called = called.clone();
                async move {
                    called.notify_one();
                    gate.acquire().await.unwrap().forget();
                    fingerprint_response(&sample("above_99", 0))
                }
            })
            .await,
        );
    }
    let (dir, service) = fixture();
    let ids: Vec<_> = ports
        .into_iter()
        .map(|port| add(&service, dir.path(), ClientId::Codex, port, "responses"))
        .collect();
    ua(&service, ClientId::Codex);
    let permit = service.0.jobs.clone().acquire_owned().await.unwrap();
    let first = service
        .start(fingerprint_request(&service, &ids[0], 1))
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), called.notified())
        .await
        .unwrap();
    let second = service
        .start(fingerprint_request(&service, &ids[1], 1))
        .unwrap();
    let old = {
        let data = lock(&service.0.data);
        assert_eq!(
            data.saved
                .jobs
                .iter()
                .find(|j| j.id == first.id)
                .unwrap()
                .status,
            "running"
        );
        assert_eq!(
            data.saved
                .jobs
                .iter()
                .find(|j| j.id == second.id)
                .unwrap()
                .status,
            "queued"
        );
        let first_bank = data.contexts[&first.id].bank.as_ref().unwrap();
        let second_bank = data.contexts[&second.id].bank.as_ref().unwrap();
        assert!(Arc::ptr_eq(first_bank, second_bank));
        Arc::downgrade(first_bank)
    };
    replace_modeltrace_resource(&service);
    assert_eq!(
        service.state(ClientId::Codex, &ids[0]).unwrap().banks["modelTrace"]["version"]["revision"],
        "fixture-updated-resource"
    );
    drop(permit);
    gate.add_permits(6);
    for job in [&first, &second] {
        let done = completed(&service, &job.id).await;
        assert_eq!(done.job.status, "completed");
        assert_eq!(
            done.job.report.as_ref().unwrap()["bank_version"],
            job.resource_version
        );
        assert!(!lock(&service.0.data).contexts.contains_key(&job.id));
        assert!(!done.output.unwrap().groups.is_empty());
    }
    assert!(old.upgrade().is_none());
}

#[test]
fn scheduler_notifies_once_for_changes_and_skips_identical_ticks() {
    let (dir, service) = fixture();
    for client in [
        ClientId::Codex,
        ClientId::Claude,
        ClientId::Grok,
        ClientId::Dsh,
    ] {
        add(&service, dir.path(), client, 1, "chat_completions");
    }
    let mut events = service.subscribe();
    service.tick(service.now(), false).unwrap();
    assert!(events.try_recv().is_ok());
    assert!(matches!(
        events.try_recv(),
        Err(broadcast::error::TryRecvError::Empty)
    ));
    service.tick(service.now() + 5_000, false).unwrap();
    assert!(matches!(
        events.try_recv(),
        Err(broadcast::error::TryRecvError::Empty)
    ));
}

#[test]
fn state_wire_omits_unused_fields() {
    let (dir, service) = fixture();
    let id = add(&service, dir.path(), ClientId::Codex, 1, "responses");
    let state = serde_json::to_value(service.state(ClientId::Codex, &id).unwrap()).unwrap();
    assert!(state.get("bank").is_none());
    assert!(state.get("history").is_none());
    assert!(state["banks"]["modelTrace"]["models"].is_array());
    assert!(state["jobs"].is_array());
}

#[tokio::test]
async fn lm_retry_retains_its_resource_and_releases_context_after_success() {
    let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let count = hits.clone();
    let port = server(move |_| {
        let count = count.clone();
        async move {
            let n = count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            protocol_response(
                "responses",
                &if n == 0 {
                    "1, 2, 3".into()
                } else {
                    lm_sample(0)
                },
                json!({}),
            )
        }
    })
    .await;
    let (dir, service) = fixture();
    let id = add(&service, dir.path(), ClientId::Codex, port, "responses");
    ua(&service, ClientId::Codex);
    seed_client_catalog(&service, ClientId::Codex, &id, "fixture-model");
    service
        .set_platform(
            ClientId::Codex,
            PlatformSettings {
                engine: Engine::LmDetector,
                ..Default::default()
            },
        )
        .unwrap();
    let mut req = request(&service, ClientId::Codex, &id);
    req.kind = Kind::Fingerprint;
    let job = service.start(req).unwrap();
    assert!(completed(&service, &job.id).await.job.can_retry);
    let old = {
        let data = lock(&service.0.data);
        assert!(data.contexts[&job.id].bank.is_none());
        Arc::downgrade(data.contexts[&job.id].lm.as_ref().unwrap())
    };
    *lock(&service.0.lm) = None;
    let current = service.lm_resource().unwrap();
    assert!(!Arc::ptr_eq(&old.upgrade().unwrap(), &current));
    service.retry(&job.id).unwrap();
    let done = completed(&service, &job.id).await;
    assert_eq!(done.job.status, "completed");
    assert_eq!(
        done.job.report.as_ref().unwrap()["bank_version"],
        job.resource_version
    );
    assert!(done.output.is_some());
    assert!(!lock(&service.0.data).contexts.contains_key(&job.id));
    assert!(old.upgrade().is_none());
}
