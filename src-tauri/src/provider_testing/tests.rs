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
    let claude = codex.companion(dir.path().join("claude")).unwrap();
    let grok = codex.grok_companion(dir.path().join("grok")).unwrap();
    let dsh = codex.dsh_companion(dir.path().join("dsh")).unwrap();
    let service = Service::new(dir.path(), [codex, claude, grok, dsh]).unwrap();
    (dir, service)
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
        assert_eq!(service.gateway(client).recent_business_requests(&id), 0);
        let saved = std::fs::read_to_string(dir.path().join("provider-tests.json")).unwrap();
        assert!(!saved.contains("fixture private output"));
        assert!(!saved.contains("fixture-test-token"));
        assert!(!saved.contains("fixture-business"));
        assert!(!saved.contains("fixture input"));
    }
}
#[tokio::test]
async fn catalog_ignores_route_whitelist_filters_media_and_forces_refresh() {
    let mode = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let switch = mode.clone();
    let port=server(move |request| { let switch=switch.clone(); async move {
        assert!(request.uri().path().ends_with("/models")); assert_eq!(request.headers()["user-agent"],"fixture-business/1.0 exact  UA");
        if switch.load(std::sync::atomic::Ordering::Relaxed) { response(503,"application/json","{}") }
        else { response(200,"application/json",r#"{"models":[{"slug":"fixture-model"},{"slug":"fixture-other"},{"slug":"fixture-image"}]}"#) }
    }}).await;
    let (dir, service) = fixture();
    let id = add(&service, dir.path(), ClientId::Codex, port, "responses");
    ua(&service, ClientId::Codex);
    let gateway = service.gateway(ClientId::Codex);
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
        .models(ClientId::Codex, &id, &gateway.view().revision, false)
        .await
        .unwrap();
    assert_eq!(models.models.len(), 2);
    mode.store(true, std::sync::atomic::Ordering::Relaxed);
    assert!(
        !service
            .models(ClientId::Codex, &id, &gateway.view().revision, false)
            .await
            .unwrap()
            .stale
    );
    // Explicit refresh bypasses the still-fresh cache, preserving it on failure.
    assert!(
        service
            .models(ClientId::Codex, &id, &gateway.view().revision, true)
            .await
            .unwrap()
            .stale
    );
    for (at, _) in lock(&service.0.data).models.values_mut() {
        *at = Instant::now() - Duration::from_secs(301);
    }
    assert!(service
        .models(ClientId::Codex, &id, &gateway.view().revision, true)
        .await
        .is_err());
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
    saved["detection"]["intervalMinutes"] = json!(25);
    saved["history"] = json!([saved["jobs"][0].clone()]);
    std::fs::write(&path, serde_json::to_vec(&saved).unwrap()).unwrap();
    let reloaded = Service::new(dir.path(), service.0.gateways.clone()).unwrap();
    let state = reloaded.state();
    assert_eq!(state.jobs[0].status, "interrupted");
    assert_eq!(state.jobs[0].mode, Mode::Image);
    assert!(!state.jobs[0].can_retry);
    assert_eq!(state.detection.interval_minutes, 25);
    assert_eq!(state.history.len(), 1);
    reloaded.tick(now(), false).unwrap();
    assert_eq!(reloaded.state().jobs.len(), 1);
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
        (3, Some(25)),
        (10, Some(20)),
        (30, Some(15)),
        (60, Some(10)),
        (120, Some(5)),
    ] {
        assert_eq!(scheduler::smart_interval(count, 5), want);
    }
    assert_eq!(scheduler::smart_interval(1000, 20), Some(20));
    let (_dir, service) = fixture();
    let state = service.state();
    assert!(!state.detection.enabled);
    assert_eq!(state.detection.mode, "smart");
    let mut config = state.detection;
    config.interval_minutes = 4;
    assert!(service
        .set_detection(config.clone(), &state.revision)
        .is_err());
    config.interval_minutes = 15;
    config.enabled = true;
    service.set_detection(config, &state.revision).unwrap();
    assert!(service.clear_mark("fixture", &state.revision).is_err());
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
        assert!(service.state().degraded.is_empty());
        assert_eq!(
            service
                .gateway(ClientId::Codex)
                .recent_business_requests(&id),
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
    assert!(service.state().degraded.is_empty());
    assert!(gateway.test_candidates().unwrap()[0].queued);
    let state = service.state();
    let mut detection = state.detection;
    detection.enabled = true;
    service.set_detection(detection, &state.revision).unwrap();
    let job = service
        .start_job(fingerprint_request(&service, &id, 1), true, None)
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
    let state = service.state();
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
    assert!(service.state().degraded.is_empty());
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
async fn batches_deduplicate_providers_and_stop_without_dispatch() {
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
    let batch = service.batch(vec![request.clone(), request]).unwrap();
    assert_eq!(batch["jobs"].as_array().unwrap().len(), 1);
    service.cancel_batch(batch["id"].as_str().unwrap()).unwrap();
    let job = batch["jobs"][0]["id"].as_str().unwrap();
    assert_eq!(completed(&service, job).await.job.status, "cancelled");
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
    let state = service.state();
    let mut detection = state.detection;
    detection.enabled = true;
    service
        .set_detection(detection.clone(), &state.revision)
        .unwrap();
    let at = now();
    service.tick(at, false).unwrap();
    assert_eq!(service.state().detection_status[0].status, "idle");
    detection.mode = "fixed".into();
    detection.interval_minutes = 5;
    service
        .set_detection(detection, &service.state().revision)
        .unwrap();
    service.tick(at, false).unwrap();
    assert_eq!(
        service.state().detection_status[0].next_at,
        Some(at + 300_000)
    );
    service.tick(at + 3_600_000, true).unwrap();
    assert!(service.state().jobs.is_empty());
    assert_eq!(
        service.state().detection_status[0].next_at,
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
    assert!(service.state().jobs.is_empty());
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
        service.state().detection_status[0].next_at,
        Some(at + 4_500_000)
    );
    let permit = service.0.jobs.clone().acquire_many_owned(2).await.unwrap();
    service.tick(at + 4_500_000, false).unwrap();
    let idjob = service.state().jobs[0].id.clone();
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
    assert_eq!(gateway.recent_business_requests(&id), 0);
    assert!(service
        .start(fingerprint_request(&service, &id, 1))
        .is_err());
}

#[tokio::test]
async fn batch_defaults_use_each_provider_catalog_and_missing_catalog_uses_platform_default() {
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
                false,
            )
            .await
            .unwrap();
    }
    let requests = ids
        .iter()
        .map(|id| {
            let mut r = request(&service, ClientId::Codex, id);
            r.model.clear();
            r
        })
        .collect();
    let batch = service.batch(requests).unwrap();
    for (i, name) in ["fixture-alpha", "fixture-beta"].iter().enumerate() {
        assert_eq!(batch["jobs"][i]["requestedModel"], *name);
        let id = batch["jobs"][i]["id"].as_str().unwrap();
        assert_eq!(completed(&service, id).await.job.status, "completed");
    }
    lock(&service.0.data).models.clear();
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
