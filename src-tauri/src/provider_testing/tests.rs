use super::*;
use crate::gateway::Edit;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{body::Incoming, Request as HttpRequest, Response};
use hyper_util::rt::TokioIo;
use std::{future::Future, time::Duration};

fn fixture() -> (tempfile::TempDir, Service) {
    let dir = tempfile::tempdir().unwrap();
    let codex = Gateway::new(dir.path().join("codex")).unwrap();
    codex.set_usage(crate::usage::Service::new(dir.path()));
    let claude = codex.companion(dir.path().join("claude")).unwrap();
    let grok = codex.grok_companion(dir.path().join("grok")).unwrap();
    let dsh = codex.dsh_companion(dir.path().join("dsh")).unwrap();
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

async fn server<F, Fut>(handler: F) -> u16
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
fn response(status: u16, content: &str, body: impl Into<Bytes>) -> Response<Full<Bytes>> {
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
    service
        .gateway(client)
        .remember_test_user_agent(b"fixture-business/1.0 exact  UA");
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
    }
}
async fn completed(service: &Service, id: &str) -> Detail {
    let mut events = service.subscribe();
    tokio::time::timeout(Duration::from_secs(12), async {
        loop {
            let detail = service.detail(id).unwrap();
            if detail.job.terminal() {
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
        let job = service.start(request(&service, client, &id)).unwrap();
        let result = completed(&service, &job.id).await;
        assert_eq!(result.job.status, "completed", "{:?}", result.job.error);
        assert_eq!(
            result.job.returned_model.as_deref(),
            Some("fixture-returned")
        );
        let seen = lock(&seen);
        assert_eq!(seen.len(), 1);
        assert!(seen[0].0.ends_with(path));
        assert_eq!(seen[0].1["user-agent"], "fixture-business/1.0 exact  UA");
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
    assert_eq!(state.history.len(), 1);
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
        .set_detection(&id, config.clone(), &state.revision)
        .is_err());
    config.interval_minutes = 15;
    config.enabled = true;
    service.set_detection(&id, config, &state.revision).unwrap();
    assert!(service.clear_mark(&id, &state.revision).is_err());
    let bad = SavedBank {
        data: Some(json!({"invalid":true})),
        version: Some(json!({"revision":"fixture"})),
        provenance: Some(json!({})),
        ..Default::default()
    };
    assert_eq!(
        bank::snapshot(&bad).unwrap().1,
        modeltrace::bundled().unwrap().1
    );
    assert!(bank::status(&bad, false)["error"].is_string());
    let same_version = SavedBank {
        version: Some(modeltrace::bundled().unwrap().1),
        ..bad
    };
    let status = bank::status(&same_version, false);
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
#[tokio::test]
async fn fingerprint_stops_at_real_99_threshold_and_concurrency_is_one_to_three() {
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
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), concurrency);
        if concurrency == 1 {
            assert_eq!(
                detail.job.completion_reason.as_deref(),
                Some("confidence99")
            );
            assert_eq!(detail.job.groups[1].status, "skipped");
        } else {
            assert!((1..=3).contains(
                &detail.job.report.as_ref().unwrap()["used_outputs"]
                    .as_u64()
                    .unwrap()
            ));
            assert!(detail.job.groups.iter().all(|g| g.attempts == 1));
            if detail.job.completion_reason.is_some() {
                assert!(
                    detail.job.report.as_ref().unwrap()["probability"]
                        .as_f64()
                        .unwrap()
                        >= 0.99
                );
            }
        }
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
            let _ = r.into_body().collect().await.unwrap();
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
    assert_eq!(before.job.groups[2].status, "completed");
    let output = before.output.unwrap().groups[&2].clone();
    service.retry(&job.id).unwrap();
    let after = completed(&service, &job.id).await;
    assert_eq!(after.job.groups[2].attempts, 1);
    assert_eq!(after.output.unwrap().groups[&2], output);
    assert!(hits.load(std::sync::atomic::Ordering::SeqCst) > 3);
    assert!(after.job.groups[0].attempts > 1);
}
#[tokio::test]
async fn automatic_first_group_dequeues_and_manual_tests_never_change_queue() {
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
        .set_detection(&id, detection, &state.revision)
        .unwrap();
    lock(&service.0.data)
        .saved
        .detection_timing
        .get_mut(&id)
        .unwrap()
        .anchor_at = Some(service.now() - 60_000);
    let job = service
        .start_job(fingerprint_request(&service, &id, 1), true)
        .unwrap();
    let result = completed(&service, &job.id).await;
    assert_eq!(
        result.job.completion_reason.as_deref(),
        Some("firstGroupDowngrade")
    );
    assert_eq!(
        result.job.groups.iter().map(|g| g.attempts).sum::<usize>(),
        1
    );
    assert!(!gateway.test_candidates().unwrap()[0].queued);
    let state = service.state(ClientId::Codex, &id).unwrap();
    assert!(state.degraded[&id].queue_removed);
    assert_eq!(state.history.len(), 1);
    {
        let mut data = lock(&service.0.data);
        let mut next = data.saved.clone();
        next.jobs.retain(|j| j.id != job.id);
        service.save(&mut data, next).unwrap();
    }
    assert!(service.detail(&job.id).unwrap().job.report.is_some());
    assert!(service.detail(&job.id).unwrap().output.is_none());
    service.clear_mark(&id, &state.revision).unwrap();
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
    service.cancel(&job.id).unwrap();
    assert_eq!(completed(&service, &job.id).await.job.status, "cancelled");
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
        .set_detection(&id, detection.clone(), &state.revision)
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
    service.set_detection(&a, config, &a_version).unwrap();
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
    assert!(lock(&service.0.data).saved.detection_timing[&id]
        .last_started_at
        .is_none());
    service.tick(at + 301_000, false).unwrap();
    assert_eq!(status(&service, &id).status, "idle");
}
#[tokio::test]
async fn started_work_survives_idle_sampling_and_cooldown_uses_start_not_completion() {
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let incoming = entered.clone();
    let gate = release.clone();
    let port = server(move |r| {
        let incoming = incoming.clone();
        let gate = gate.clone();
        async move {
            let _ = r.into_body().collect().await;
            incoming.notify_one();
            gate.notified().await;
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
        lock(&service.0.data).saved.detection_timing[&id].anchor_at,
        ended.job.completed_at
    );
    assert_eq!(
        lock(&service.0.data).saved.detection_timing[&id].last_started_at,
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
        .set_detection(&id, config.clone(), &state.revision)
        .unwrap();
    service.tick(at + 121_000, false).unwrap();
    assert_eq!(status(&service, &id).next_at, Some(at + 660_000));
    config.interval_minutes = 1;
    let revision = service.state(ClientId::Codex, &id).unwrap().revision;
    service.set_detection(&id, config, &revision).unwrap();
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
            &b,
            disabled,
            &again.state(ClientId::Codex, &b).unwrap().revision,
        )
        .unwrap();
    let raw: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(raw["schema"], 2);
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
        .set_detection(&id, detection, &state.revision)
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
        &[sample("automatic_first_group", 0)],
        &modeltrace::bundled().unwrap(),
    )
    .unwrap();
    service
        .update(&job.id, |job, _| {
            job.groups[0].report = None;
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
