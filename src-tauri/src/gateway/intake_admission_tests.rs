// 覆盖普通、新线程及压缩请求的准入规则。
//! v0.21 strict takeover policy and native websocket compatibility fixtures.
//!
//! This file is registered by the v0.20 fixture module so it can reuse its
//! loopback server, gateway, and request helpers.
use super::*;
use crate::gateway::{
    admission::{Budget, Rejected},
    compaction::{Intake, Registry},
    model::{Edit, Provider, Store},
    routing::Requirement,
};
use futures_util::{SinkExt, StreamExt};
use http_body_util::BodyExt;
use hyper::Response;
use serde_json::{json, Value};
use std::{
    fs,
    net::TcpListener,
    sync::{Arc, Mutex},
    time::Duration,
};

fn policy_provider(id: &str, handoff: bool, take: bool) -> Provider {
    Provider {
        id: id.into(),
        name: id.into(),
        base_url: format!("https://{id}.example.invalid/v1"),
        token: "fixture-token".into(),
        queued: true,
        version: format!("{id}-version"),
        max_concurrency: 0,
        max_rpm: 0,
        allowed_models: None,
        handoff_after_compaction: handoff,
        take_new_threads: take,
        search_only: false,
    }
}

fn free_port() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.local_addr().unwrap().port()
}

async fn settled(gateway: &Gateway) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while gateway
            .view()
            .providers
            .iter()
            .any(|provider| provider.active_requests != 0)
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("fixture request did not settle");
}

#[test]
fn intake_flags_are_strict_with_neutral_provider_as_the_only_ordinary_fallback() {
    assert!(crate::gateway::compaction::accepts_flags(
        false,
        false,
        Intake::Ordinary
    ));
    assert!(crate::gateway::compaction::accepts_flags(
        false,
        false,
        Intake::NewThread
    ));
    assert!(crate::gateway::compaction::accepts_flags(
        false,
        false,
        Intake::Compacted
    ));

    assert!(!crate::gateway::compaction::accepts_flags(
        true,
        false,
        Intake::Ordinary
    ));
    assert!(!crate::gateway::compaction::accepts_flags(
        true,
        false,
        Intake::NewThread
    ));
    assert!(crate::gateway::compaction::accepts_flags(
        true,
        false,
        Intake::Compacted
    ));

    assert!(!crate::gateway::compaction::accepts_flags(
        false,
        true,
        Intake::Ordinary
    ));
    assert!(crate::gateway::compaction::accepts_flags(
        false,
        true,
        Intake::NewThread
    ));
    assert!(!crate::gateway::compaction::accepts_flags(
        false,
        true,
        Intake::Compacted
    ));

    assert!(!crate::gateway::compaction::accepts_flags(
        true,
        true,
        Intake::Ordinary
    ));
    assert!(crate::gateway::compaction::accepts_flags(
        true,
        true,
        Intake::NewThread
    ));
    assert!(crate::gateway::compaction::accepts_flags(
        true,
        true,
        Intake::Compacted
    ));
}

#[test]
fn registry_policy_distinguishes_new_threads_and_reliable_compaction_boundaries() {
    let dir = tempfile::tempdir().unwrap();
    let registry = Registry::new(dir.path().join("owners.json"));
    let neutral = policy_provider("neutral", false, false);
    let new_only = policy_provider("new-only", false, true);
    let compact_only = policy_provider("compact-only", true, false);
    let both = policy_provider("both", true, true);
    let providers = [
        neutral.clone(),
        new_only.clone(),
        compact_only.clone(),
        both.clone(),
    ];

    let ordinary = hints(json!({
        "input":[
            {"role":"user","content":"prior"},
            {"role":"assistant","content":"answer"},
            {"role":"user","content":"continuation"}
        ]
    }));
    let ordinary_lease = registry
        .prepare_policy(
            digest("fixture-ordinary"),
            Some(&ordinary),
            true,
            &providers,
        )
        .unwrap();
    assert_eq!(ordinary_lease.admission_policy(), (Intake::Ordinary, None));

    let first = hints(json!({
        "input":[{"role":"user","content":"first turn"}]
    }));
    let fresh = registry
        .prepare_policy(digest("fixture-new"), Some(&first), true, &providers)
        .unwrap();
    assert_eq!(fresh.admission_policy(), (Intake::NewThread, None));

    let session = digest("fixture-compact");
    let initial = registry
        .prepare_policy(session.clone(), Some(&ordinary), false, &providers)
        .unwrap();
    assert!(initial.admitted("neutral"));
    let boundary = digest("fixture-window");
    initial.complete("neutral", Some(&boundary));

    let without_boundary = registry
        .prepare_policy(
            session.clone(),
            Some(&RequestHints::default()),
            true,
            &[
                compact_only.clone(),
                neutral.clone(),
                both.clone(),
                new_only.clone(),
            ],
        )
        .unwrap();
    assert_eq!(without_boundary.admission_policy().0, Intake::Ordinary);
    assert_eq!(
        without_boundary.admission_policy().1.as_deref(),
        Some("neutral")
    );

    let compacted = RequestHints {
        compacted_window: Some(boundary),
        ..Default::default()
    };
    let handoff = registry
        .prepare_policy(
            session,
            Some(&compacted),
            true,
            &[compact_only, neutral, both, new_only],
        )
        .unwrap();
    assert_eq!(handoff.admission_policy().0, Intake::Compacted);
    assert_eq!(handoff.admission_policy().1.as_deref(), Some("neutral"));
}

#[test]
fn takeover_edit_updates_one_field_and_rejects_an_empty_operation() {
    let mut store = Store::default();
    store
        .edit(
            Edit::SaveProvider {
                quota_access_token: None,
                id: None,
                base_url: "https://fixture.example.invalid/v1".into(),
                token: "fixture-token".into(),
                name: Some("fixture".into()),
            },
            false,
        )
        .unwrap();
    let id = store.providers[0].id.clone();
    let error = store
        .edit(
            Edit::TakeoverProvider {
                id: id.clone(),
                handoff_after_compaction: None,
                take_new_threads: None,
            },
            false,
        )
        .unwrap_err();
    assert_eq!(error.code, "SETTINGS");

    store
        .edit(
            Edit::TakeoverProvider {
                id: id.clone(),
                handoff_after_compaction: Some(true),
                take_new_threads: None,
            },
            false,
        )
        .unwrap();
    assert!(store.providers[0].handoff_after_compaction);
    assert!(!store.providers[0].take_new_threads);

    store
        .edit(
            Edit::TakeoverProvider {
                id,
                handoff_after_compaction: None,
                take_new_threads: Some(true),
            },
            false,
        )
        .unwrap();
    assert!(store.providers[0].handoff_after_compaction);
    assert!(store.providers[0].take_new_threads);
}

#[tokio::test]
async fn ordinary_admission_filters_specialized_fallbacks() {
    let first =
        server(|_| async { Response::new(replay::full(r#"{"provider":"compact-only"}"#)) }).await;
    let second =
        server(|_| async { Response::new(replay::full(r#"{"provider":"neutral"}"#)) }).await;
    let (dir, gateway) = fixture(vec![
        format!("http://127.0.0.1:{first}/v1"),
        format!("http://127.0.0.1:{second}/v1"),
    ])
    .await;
    let ids: Vec<_> = gateway
        .view()
        .providers
        .iter()
        .map(|provider| provider.id.clone())
        .collect();
    update(
        &gateway,
        &dir,
        Edit::TakeoverProvider {
            id: ids[0].clone(),
            handoff_after_compaction: Some(true),
            take_new_threads: Some(false),
        },
    );
    start(&gateway, &dir).await;
    let response = request(
        &gateway,
        "fixture-ordinary-fallback",
        br#"{"model":"fixture-model","input":"ordinary"}"#.to_vec(),
    )
    .await;
    assert_eq!(response.status(), 200);
    let value: Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(value["provider"], "neutral");
    gateway.stop().await.unwrap();
}

#[tokio::test]
async fn queued_ordinary_request_is_rejected_when_policy_changes_to_compact_only() {
    let (dir, gateway) = fixture(vec![
        "https://p1.example.invalid/v1".into(),
        "https://p2.example.invalid/v1".into(),
    ])
    .await;
    for index in 0..2 {
        update(
            &gateway,
            &dir,
            Edit::ConcurrencyProvider {
                id: gateway.view().providers[index].id.clone(),
                max_concurrency: 1,
            },
        );
    }
    start(&gateway, &dir).await;
    let routes = gateway
        .view()
        .providers
        .iter()
        .map(|provider| gateway.route(&provider.id).unwrap())
        .collect::<Vec<_>>();
    let mut first_budget = Budget::new(3);
    first_budget.set_intake(Intake::Ordinary, None);
    let held_first = gateway
        .0
        .admission
        .acquire_for(
            &routes[0..1],
            false,
            10,
            &mut first_budget,
            &Requirement::Resource,
        )
        .await
        .unwrap();
    let mut second_budget = Budget::new(3);
    second_budget.set_intake(Intake::Ordinary, None);
    let held_second = gateway
        .0
        .admission
        .acquire_for(
            &routes[1..2],
            false,
            10,
            &mut second_budget,
            &Requirement::Resource,
        )
        .await
        .unwrap();

    let scheduler = gateway.0.admission.clone();
    let waiting_routes = routes.clone();
    let waiting = tokio::spawn(async move {
        let mut budget = Budget::new(10);
        budget.set_intake(Intake::Ordinary, None);
        scheduler
            .acquire_for(
                &waiting_routes,
                false,
                10,
                &mut budget,
                &Requirement::Resource,
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(2), async {
        while gateway.view().waiting_requests == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    for provider in gateway.view().providers {
        update(
            &gateway,
            &dir,
            Edit::TakeoverProvider {
                id: provider.id,
                handoff_after_compaction: Some(true),
                take_new_threads: Some(false),
            },
        );
    }
    assert!(matches!(waiting.await.unwrap(), Err(Rejected::Policy)));
    assert!(gateway
        .view()
        .providers
        .iter()
        .all(|provider| provider.rpm_used == 0));
    drop(held_first);
    drop(held_second);
    gateway.stop().await.unwrap();
}

#[tokio::test]
async fn protected_new_thread_capacity_prefers_new_only_and_does_not_bypass_its_rpm_limit() {
    let (dir, gateway) = fixture(vec![
        "https://ordinary.example.invalid/v1".into(),
        "https://new-only.example.invalid/v1".into(),
    ])
    .await;
    let ids: Vec<_> = gateway
        .view()
        .providers
        .iter()
        .map(|provider| provider.id.clone())
        .collect();
    update(
        &gateway,
        &dir,
        Edit::TakeoverProvider {
            id: ids[1].clone(),
            handoff_after_compaction: Some(false),
            take_new_threads: Some(true),
        },
    );
    update(
        &gateway,
        &dir,
        Edit::RpmProvider {
            id: ids[1].clone(),
            max_rpm: 1,
        },
    );
    start(&gateway, &dir).await;
    let routes = gateway
        .view()
        .providers
        .iter()
        .map(|provider| gateway.route(&provider.id).unwrap())
        .collect::<Vec<_>>();
    let mut first = Budget::new(3);
    first.set_intake(Intake::NewThread, None);
    first.protect_capacity();
    let mut admission = gateway
        .0
        .admission
        .acquire_for_immediate(&routes, false, 10, &mut first, &Requirement::Resource, true)
        .await
        .unwrap();
    assert_eq!(admission.route.provider.id, ids[1]);
    admission.commit_rpm().unwrap();
    drop(admission);

    let mut second = Budget::new(0);
    second.set_intake(Intake::NewThread, None);
    second.protect_capacity();
    let result = gateway
        .0
        .admission
        .acquire_for_immediate(
            &routes,
            false,
            10,
            &mut second,
            &Requirement::Resource,
            true,
        )
        .await;
    assert!(matches!(result, Err(Rejected::RateLimited(_))));
    assert_eq!(gateway.view().providers[0].rpm_used, 0);
    gateway.stop().await.unwrap();
}

#[tokio::test]
async fn pinning_a_previous_response_clears_takeover_filter_and_keeps_owner_route() {
    let (dir, gateway) = fixture(vec![
        "https://ordinary.example.invalid/v1".into(),
        "https://new-only.example.invalid/v1".into(),
    ])
    .await;
    let ids: Vec<_> = gateway
        .view()
        .providers
        .iter()
        .map(|provider| provider.id.clone())
        .collect();
    update(
        &gateway,
        &dir,
        Edit::TakeoverProvider {
            id: ids[0].clone(),
            handoff_after_compaction: Some(false),
            take_new_threads: Some(false),
        },
    );
    update(
        &gateway,
        &dir,
        Edit::TakeoverProvider {
            id: ids[1].clone(),
            handoff_after_compaction: Some(false),
            take_new_threads: Some(true),
        },
    );
    start(&gateway, &dir).await;
    let routes = gateway
        .view()
        .providers
        .iter()
        .map(|provider| gateway.route(&provider.id).unwrap())
        .collect::<Vec<_>>();
    let mut budget = Budget::new(3);
    budget.set_intake(Intake::NewThread, None);
    budget.pin_provider();
    let admission = gateway
        .0
        .admission
        .acquire_for_immediate(
            &routes[0..1],
            false,
            10,
            &mut budget,
            &Requirement::Resource,
            true,
        )
        .await
        .unwrap();
    assert_eq!(admission.route.provider.id, ids[0]);
    drop(admission);
    gateway.stop().await.unwrap();
}

#[tokio::test]
async fn capacity_wait_keeps_ordinary_owner_when_compact_only_fallback_is_ineligible() {
    let p1_hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let p1_seen = p1_hits.clone();
    let p1 = server(move |_| {
        let p1_seen = p1_seen.clone();
        async move {
            let attempt = p1_seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            if attempt <= 2 {
                Response::builder()
                    .status(429)
                    .header("retry-after", "0")
                    .body(replay::full("selected model is at capacity"))
                    .unwrap()
            } else {
                Response::new(replay::full(r#"{"provider":"ordinary-owner"}"#))
            }
        }
    })
    .await;
    let p2_hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let p2_seen = p2_hits.clone();
    let p2 = server(move |_| {
        let p2_seen = p2_seen.clone();
        async move {
            p2_seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Response::new(replay::full(r#"{"provider":"compact-only"}"#))
        }
    })
    .await;
    let (dir, gateway) = fixture(vec![
        format!("http://127.0.0.1:{p1}/v1"),
        format!("http://127.0.0.1:{p2}/v1"),
    ])
    .await;
    let ids: Vec<_> = gateway
        .view()
        .providers
        .iter()
        .map(|provider| provider.id.clone())
        .collect();
    update(
        &gateway,
        &dir,
        Edit::TakeoverProvider {
            id: ids[1].clone(),
            handoff_after_compaction: Some(true),
            take_new_threads: Some(false),
        },
    );
    let mut settings = gateway.view().settings;
    settings.capacity_retry_seconds = 1;
    settings.max_retries = 1;
    update(&gateway, &dir, Edit::Settings { settings });
    start(&gateway, &dir).await;
    let response = tokio::time::timeout(
        Duration::from_secs(8),
        request(
            &gateway,
            "fixture-capacity-owner",
            br#"{"model":"fixture-model","input":"ordinary"}"#.to_vec(),
        ),
    )
    .await
    .expect("capacity wait did not return");
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.into_body().collect().await.unwrap().to_bytes(),
        r#"{"provider":"ordinary-owner"}"#
    );
    assert!(p1_hits.load(std::sync::atomic::Ordering::SeqCst) >= 3);
    assert_eq!(p2_hits.load(std::sync::atomic::Ordering::SeqCst), 0);
    gateway.stop().await.unwrap();
}

#[tokio::test]
async fn legacy_provider_capability_is_ignored_and_native_upgrade_forwards_exact_payload() {
    let observed = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
    let captured = observed.clone();
    let upstream = server(move |mut request| {
        let captured = captured.clone();
        async move {
            assert!(request.headers().get("upgrade").is_some());
            let (response, upgrade) = yawc::WebSocket::upgrade(&mut request).unwrap();
            tokio::spawn(async move {
                let Ok(mut peer) = upgrade.await else {
                    return;
                };
                if let Some(frame) = peer.next().await {
                    if !frame.opcode().is_control() {
                        captured.lock().unwrap().push(frame.payload().to_vec());
                        let _ = peer
                            .send(yawc::Frame::text(
                                r#"{"type":"response.created","response":{"id":"fixture-native"}}"#,
                            ))
                            .await;
                        let _ = peer
                            .send(yawc::Frame::text(
                                r#"{"type":"response.completed","response":{"id":"fixture-native","status":"completed"}}"#,
                            ))
                            .await;
                    }
                }
            });
            response.map(|_| replay::empty())
        }
    })
    .await;
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("codex-home");
    let data = root.path().join("gateway-data");
    fs::create_dir(&home).unwrap();
    fs::create_dir(&data).unwrap();
    let base = format!("http://127.0.0.1:{upstream}/v1");
    fs::write(
        home.join("config.toml"),
        format!(
            "model_provider='custom'\n[model_providers.custom]\nbase_url='{}'\nexperimental_bearer_token='fixture-token'\nwire_api='responses'\nsupports_websockets=false\n",
            base
        ),
    )
    .unwrap();
    fs::write(
        home.join("auth.json"),
        br#"{"auth_mode":"apikey","OPENAI_API_KEY":"fixture-api-key"}"#,
    )
    .unwrap();
    let local_port = free_port();
    let mut store = Store {
        initialized: true,
        ..Default::default()
    };
    store.settings.port = local_port;
    store.providers = vec![policy_provider("legacy", false, false)];
    store.providers[0].base_url = base;
    let mut old = serde_json::to_value(&store).unwrap();
    old["providers"][0]["supportsWebsocket"] = false.into();
    fs::write(
        data.join("gateway.json"),
        serde_json::to_vec_pretty(&old).unwrap(),
    )
    .unwrap();

    let gateway = Gateway::new(data.clone()).unwrap();
    let view: Value = serde_json::to_value(gateway.view()).unwrap();
    assert!(view["providers"][0].get("supportsWebsocket").is_none());
    update(
        &gateway,
        &root,
        Edit::Mode {
            mode: "auto".into(),
        },
    );
    let saved: Value =
        serde_json::from_slice(&fs::read(data.join("gateway.json")).unwrap()).unwrap();
    assert!(saved["providers"][0].get("supportsWebsocket").is_none());
    let mut settings = gateway.view().settings;
    settings.websocket_retry_seconds = 7;
    update(&gateway, &root, Edit::Settings { settings });
    let persisted: Value =
        serde_json::from_slice(&fs::read(data.join("gateway.json")).unwrap()).unwrap();
    assert_eq!(persisted["settings"]["websocketRetrySeconds"], 7);

    gateway
        .start(&gateway.view().revision, &home)
        .await
        .unwrap();
    let (token, port) = {
        let state = gateway.0.inner.lock().unwrap();
        (state.store.local_token.clone(), state.store.settings.port)
    };
    let mut client = yawc::WebSocket::connect(
        format!("ws://127.0.0.1:{port}/v1/responses")
            .parse()
            .unwrap(),
    )
    .with_request(
        yawc::HttpRequest::builder()
            .header("authorization", format!("Bearer {token}"))
            .header("session_id", "fixture-legacy-native"),
    )
    .await
    .unwrap();
    let payload = br#" {"type":"response.create","model":"fixture-model","input":"fixture","future":{"unknown":true},"large":90071992547409931234567890} "#;
    client
        .send(yawc::Frame::text(
            String::from_utf8(payload.to_vec()).unwrap(),
        ))
        .await
        .unwrap();
    assert!(client
        .next()
        .await
        .unwrap()
        .as_str()
        .contains("response.created"));
    assert!(client
        .next()
        .await
        .unwrap()
        .as_str()
        .contains("response.completed"));
    settled(&gateway).await;
    assert_eq!(observed.lock().unwrap().as_slice(), &[payload.to_vec()]);
    client
        .send(yawc::Frame::close(1000.into(), "fixture done"))
        .await
        .unwrap();
    drop(client);
    gateway.stop().await.unwrap();
}
