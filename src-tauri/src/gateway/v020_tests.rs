//! v0.20 policy, ownership, and first-admission regressions.
//!
//! These fixtures use loopback upstreams and synthetic tokens only.  The module
//! is intentionally self-contained so it does not couple the v0.20 checks to
//! the older versioned test helpers.
use super::{
    compaction::Registry,
    model::{Provider, Store},
    replay::{self, RequestHints, WireBody},
    *,
};
use crate::usage::Service;
use http_body_util::BodyExt;
use hyper::{body::Incoming, Request, Response};
use hyper_util::{client::legacy::Client, rt::TokioExecutor};
use serde_json::{json, Value};
use std::{
    convert::Infallible,
    future::Future,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

fn provider(id: &str, handoff: bool, take: bool) -> Provider {
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
        supports_websocket: false,
        handoff_after_compaction: handoff,
        take_new_threads: take,
    }
}

#[cfg(test)]
#[path = "v020_ws_tests.rs"]
mod websocket_tests;

fn hints(value: Value) -> RequestHints {
    RequestHints::from_value(value, false).expect("fixture request metadata")
}

fn digest(value: &str) -> String {
    crate::storage::digest(value.as_bytes())
}

#[test]
fn protocol_json_without_object_or_type_uses_openai_inclusive_cache_rules() {
    let codex_dir = tempfile::tempdir().unwrap();
    let codex_service = Service::new(codex_dir.path());
    let codex_trace = codex_service.begin("codex", Some("fixture-model"));
    let mut protocol = super::protocol::Protocol::new(false);
    protocol.attach_usage(codex_trace.attempt("fixture-provider", false, "http"));
    protocol.value(&json!({
        "id":"fixture-response",
        "usage":{"input_tokens":100,"cache_read_input_tokens":80,"output_tokens":4}
    }));
    assert_eq!(protocol.observation.meter.inclusive_input, Some(100));
    assert_eq!(protocol.observation.meter.tokens.input, Some(20));
    assert_eq!(protocol.observation.meter.tokens.cache_read, Some(80));
    assert_eq!(protocol.observation.meter.tokens.cache_write, None);

    let claude_dir = tempfile::tempdir().unwrap();
    let claude_service = Service::new(claude_dir.path());
    let claude_trace = claude_service.begin("claude", Some("fixture-model"));
    let mut claude = super::protocol::Protocol::new(false);
    claude.attach_usage(claude_trace.attempt("fixture-provider", false, "http"));
    claude.value(&json!({"message":{"usage":{"input_tokens":100,"cache_read_input_tokens":80}}}));
    assert_eq!(claude.observation.meter.inclusive_input, None);
    assert_eq!(claude.observation.meter.tokens.input, Some(100));
    assert_eq!(claude.observation.meter.tokens.cache_read, Some(80));
}

#[test]
fn legacy_global_handoff_flag_migrates_to_each_provider_without_staying_global() {
    let dir = tempfile::tempdir().unwrap();
    let mut value = serde_json::to_value(Store::default()).unwrap();
    value["settings"]["handoffAfterCompaction"] = false.into();
    value["providers"] = json!([{
        "id": "fixture-old",
        "name": "old",
        "baseUrl": "https://old.example.invalid/v1",
        "token": "fixture-token",
        "queued": true,
        "version": "fixture-version",
        "maxConcurrency": 0,
        "maxRpm": 0,
        "allowedModels": null,
        "supportsWebsocket": false
    }]);
    let path = dir.path().join("gateway.json");
    std::fs::write(&path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();

    let (store, revision) = Store::load(&path).unwrap();
    assert!(!store.providers[0].handoff_after_compaction);
    assert!(!store.providers[0].take_new_threads);
    assert_eq!(store.persist(&path, &revision).unwrap().len(), 64);
    let saved: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert!(saved["settings"].get("handoffAfterCompaction").is_none());
    assert_eq!(saved["providers"][0]["handoffAfterCompaction"], false);
}

#[test]
fn missing_legacy_handoff_flag_defaults_each_provider_to_enabled() {
    let dir = tempfile::tempdir().unwrap();
    let mut value = serde_json::to_value(Store::default()).unwrap();
    value["providers"] = json!([{
        "id": "fixture-provider",
        "name": "fixture",
        "baseUrl": "https://fixture.example.invalid/v1",
        "token": "fixture-token",
        "queued": true,
        "version": "fixture-version",
        "supportsWebsocket": true
    }]);
    let path = dir.path().join("gateway.json");
    std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    let (store, _) = Store::load(&path).unwrap();
    assert!(store.providers[0].handoff_after_compaction);
    assert!(!store.providers[0].take_new_threads);
}

#[test]
fn policy_edit_changes_only_the_requested_provider_flags() {
    let mut store = Store::default();
    store
        .edit(
            Edit::SaveProvider {
                id: None,
                base_url: "https://fixture.example.invalid/v1".into(),
                token: "fixture-token".into(),
                name: Some("fixture".into()),
            },
            false,
        )
        .unwrap();
    let id = store.providers[0].id.clone();
    store
        .edit(
            Edit::PolicyProvider {
                id: id.clone(),
                supports_websocket: false,
                allowed_models: Some(vec!["fixture-model".into()]),
                handoff_after_compaction: Some(false),
                take_new_threads: Some(true),
            },
            false,
        )
        .unwrap();
    let p = &store.providers[0];
    assert!(!p.supports_websocket);
    assert_eq!(
        p.allowed_models.as_deref(),
        Some(["fixture-model".into()].as_slice())
    );
    assert!(!p.handoff_after_compaction);
    assert!(p.take_new_threads);

    store
        .edit(
            Edit::PolicyProvider {
                id,
                supports_websocket: true,
                allowed_models: None,
                handoff_after_compaction: None,
                take_new_threads: None,
            },
            false,
        )
        .unwrap();
    let p = &store.providers[0];
    assert!(p.supports_websocket);
    assert_eq!(p.allowed_models, None);
    assert!(!p.handoff_after_compaction);
    assert!(p.take_new_threads);
}

#[test]
fn first_turn_hints_require_one_user_message_and_no_cursor_or_compaction() {
    assert!(
        hints(json!({"model":"fixture-model","input":[{"role":"user","content":"hi"}]})).first_turn
    );
    for value in [
        json!({"input":[{"role":"assistant","content":"hi"}]}),
        json!({"input":[{"type":"function_call","id":"fixture"}]}),
        json!({"input":[{"type":"compaction_trigger"}]}),
        json!({"previous_response_id":"fixture-response","input":"hi"}),
        json!({"input":[{"role":"user"},{"role":"user"}]}),
    ] {
        assert!(!hints(value).first_turn);
    }
}

#[test]
fn first_turn_policy_prefers_take_provider_but_keeps_queue_order_for_old_threads() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("owners.json");
    let registry = Registry::new(path);
    let providers = vec![provider("p1", true, false), provider("p2", true, true)];
    let first = hints(json!({"input":"first"}));
    let lease = registry
        .prepare_policy(digest("fixture-session"), Some(&first), true, &providers)
        .unwrap();
    assert_eq!(lease.owner.as_deref(), Some("p2"));
    assert!(lease.fresh);
    assert!(lease.admitted("p2"));

    let old = hints(json!({"input":"later"}));
    let next = registry
        .prepare_policy(digest("fixture-session"), Some(&old), true, &providers)
        .unwrap();
    assert_eq!(next.owner.as_deref(), Some("p2"));
    assert!(!next.handoff);
}

#[test]
fn completed_compaction_releases_only_the_newer_preferred_receivers() {
    let dir = tempfile::tempdir().unwrap();
    let registry = Registry::new(dir.path().join("owners.json"));
    let session = digest("fixture-session");
    let boundary = digest("window-hash");
    let old = provider("old", true, false);
    let new = provider("new", true, false);
    let providers = vec![old.clone(), new.clone()];
    let ordinary = hints(json!({"input":"hello"}));
    let initial = registry
        .prepare_policy(session.clone(), Some(&ordinary), false, &providers)
        .unwrap();
    assert!(initial.admitted("old"));
    initial.complete("old", Some(&boundary));

    let compacted = RequestHints {
        compacted_window: Some(boundary),
        ..Default::default()
    };
    let reordered = vec![new, old];
    let handoff = registry
        .prepare_policy(session, Some(&compacted), true, &reordered)
        .unwrap();
    assert_eq!(handoff.owner.as_deref(), Some("old"));
    assert!(handoff.handoff);
}

#[test]
fn receiver_flag_closed_keeps_compacted_thread_on_its_owner() {
    let dir = tempfile::tempdir().unwrap();
    let registry = Registry::new(dir.path().join("owners.json"));
    let session = digest("fixture-session");
    let boundary = digest("window-hash");
    let old = provider("old", true, false);
    let new = provider("new", false, false);
    let first = registry
        .prepare_policy(
            session.clone(),
            Some(&hints(json!({"input":"x"}))),
            false,
            &[old.clone(), new.clone()],
        )
        .unwrap();
    assert!(first.admitted("old"));
    first.complete("old", Some(&boundary));
    let compacted = RequestHints {
        compacted_window: Some(boundary),
        ..Default::default()
    };
    let lease = registry
        .prepare_policy(session, Some(&compacted), true, &[new, old])
        .unwrap();
    assert!(lease.handoff);
    assert_eq!(lease.owner.as_deref(), Some("old"));
}

#[test]
fn ownership_survives_registry_restart_and_keeps_boundary_hash() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("owners.json");
    let session = digest("fixture-session");
    let boundary = digest("window-hash");
    let old = provider("old", true, false);
    let new = provider("new", true, false);
    let first = Registry::new(path.clone())
        .prepare_policy(
            session.clone(),
            Some(&hints(json!({"input":"x"}))),
            false,
            &[old.clone(), new.clone()],
        )
        .unwrap();
    assert!(first.admitted("old"));
    first.complete("old", Some(&boundary));

    let restored = Registry::new(path);
    let lease = restored
        .prepare_policy(
            session,
            Some(&RequestHints {
                compacted_window: Some(boundary),
                ..Default::default()
            }),
            true,
            &[new, old],
        )
        .unwrap();
    assert!(!lease.fresh);
    assert_eq!(lease.owner.as_deref(), Some("old"));
    assert!(lease.handoff);
}

#[test]
fn concurrent_first_admission_binds_one_provider_and_rejects_the_other() {
    let dir = tempfile::tempdir().unwrap();
    let registry = Registry::new(dir.path().join("owners.json"));
    let session = digest("fixture-session");
    let providers = vec![provider("p1", true, false), provider("p2", true, false)];
    let first = hints(json!({"input":"first"}));
    let a = registry
        .prepare_policy(session.clone(), Some(&first), true, &providers)
        .unwrap();
    let b = registry
        .prepare_policy(session, Some(&first), true, &providers)
        .unwrap();
    assert!(a.admitted("p1"));
    assert!(!b.admitted("p2"));
    assert!(b.admitted("p1"));
}

#[test]
fn upstream_numeric_error_codes_are_retained_as_strings_and_local_codes_are_distinct() {
    let upstream = super::upstream_error::details_value(
        &json!({"error":{"code":42901,"message":"rate limited"}}),
        true,
    );
    assert_eq!(upstream.upstream_code.as_deref(), Some("42901"));
    let local = crate::events::Details {
        local_code: Some("FIRST_BYTE_TIMEOUT".into()),
        ..Default::default()
    };
    assert_ne!(upstream, local);
    assert_eq!(local.local_code.as_deref(), Some("FIRST_BYTE_TIMEOUT"));
}

async fn server<F, Fut>(handler: F) -> u16
where
    F: Fn(Request<Incoming>) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Response<WireBody>> + Send + 'static,
{
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let handler = Arc::new(handler);
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let handler = handler.clone();
            tokio::spawn(async move {
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(
                        hyper_util::rt::TokioIo::new(stream),
                        hyper::service::service_fn(move |request| {
                            let handler = handler.clone();
                            async move { Ok::<_, Infallible>(handler(request).await) }
                        }),
                    )
                    .with_upgrades()
                    .await;
            });
        }
    });
    port
}

fn json_response(status: u16, body: &str, retry_after: Option<&str>) -> Response<WireBody> {
    let mut response = Response::builder()
        .status(status)
        .header("content-type", "application/json");
    if let Some(retry_after) = retry_after {
        response = response.header("retry-after", retry_after);
    }
    response.body(replay::full(body.to_owned())).unwrap()
}

async fn fixture(urls: Vec<String>) -> (tempfile::TempDir, Gateway) {
    let dir = tempfile::tempdir().unwrap();
    let first = urls
        .first()
        .cloned()
        .unwrap_or_else(|| "https://fixture.example.invalid/v1".into());
    std::fs::write(
        dir.path().join("config.toml"),
        format!(
            "model_provider='custom'\n[model_providers.custom]\nbase_url = {}\nexperimental_bearer_token = \"fixture-token\"\nwire_api='responses'\nsupports_websockets=false\n",
            serde_json::to_string(&first).unwrap()
        ),
    )
    .unwrap();
    let gateway = Gateway::new(dir.path().to_path_buf()).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let mut settings = gateway.view().settings;
    settings.port = port;
    update(&gateway, &dir, Edit::Settings { settings });
    for url in urls {
        update(
            &gateway,
            &dir,
            Edit::SaveProvider {
                id: None,
                base_url: url,
                token: "fixture-token".into(),
                name: None,
            },
        );
    }
    gateway
        .edit(
            Edit::Mode {
                mode: "auto".into(),
            },
            &gateway.view().revision,
            dir.path(),
        )
        .unwrap();
    (dir, gateway)
}

fn update(gateway: &Gateway, dir: &tempfile::TempDir, edit: Edit) {
    gateway
        .edit(edit, &gateway.view().revision, dir.path())
        .unwrap();
}

async fn start(gateway: &Gateway, dir: &tempfile::TempDir) {
    gateway
        .start(&gateway.view().revision, dir.path())
        .await
        .unwrap();
}

async fn request(gateway: &Gateway, session: &str, body: Vec<u8>) -> Response<Incoming> {
    let client: Client<_, WireBody> = Client::builder(TokioExecutor::new())
        .build(super::connector::Connector::new(Duration::from_secs(2), 0));
    let (token, port) = {
        let state = gateway.0.inner.lock().unwrap();
        (state.store.local_token.clone(), state.store.settings.port)
    };
    let req = Request::builder()
        .method("POST")
        .uri(format!("http://127.0.0.1:{port}/v1/responses"))
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .header("session_id", session)
        .body(replay::full(body))
        .unwrap();
    client.request(req).await.unwrap()
}

#[tokio::test]
async fn http_first_turn_uses_take_provider_even_when_queue_order_is_older() {
    let first_hits = Arc::new(AtomicUsize::new(0));
    let first_counter = first_hits.clone();
    let first = server(move |_| {
        first_counter.fetch_add(1, Ordering::SeqCst);
        async { Response::new(replay::full(r#"{"provider":"p1"}"#)) }
    })
    .await;
    let second = server(|_| async { Response::new(replay::full(r#"{"provider":"p2"}"#)) }).await;
    let (dir, gateway) = fixture(vec![
        format!("http://127.0.0.1:{first}/v1"),
        format!("http://127.0.0.1:{second}/v1"),
    ])
    .await;
    let ids: Vec<_> = gateway
        .view()
        .providers
        .iter()
        .map(|p| p.id.clone())
        .collect();
    update(
        &gateway,
        &dir,
        Edit::PolicyProvider {
            id: ids[1].clone(),
            supports_websocket: false,
            allowed_models: None,
            handoff_after_compaction: Some(true),
            take_new_threads: Some(true),
        },
    );
    start(&gateway, &dir).await;
    let response = request(
        &gateway,
        "fixture-first",
        br#"{"model":"fixture-model","input":[{"role":"user","content":"hello"}]}"#.to_vec(),
    )
    .await;
    assert_eq!(response.status(), 200);
    let value: Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(value["provider"], "p2");
    assert_eq!(first_hits.load(Ordering::SeqCst), 0);
    gateway.stop().await.unwrap();
}

#[tokio::test]
async fn http_full_first_provider_falls_back_to_second_then_binds_new_session() {
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let entered_copy = entered.clone();
    let release_copy = release.clone();
    let first = server(move |_| {
        let entered = entered_copy.clone();
        let release = release_copy.clone();
        async move {
            entered.notify_one();
            release.notified().await;
            Response::new(replay::full(r#"{"provider":"p1"}"#))
        }
    })
    .await;
    let second = server(|_| async { Response::new(replay::full(r#"{"provider":"p2"}"#)) }).await;
    let (dir, gateway) = fixture(vec![
        format!("http://127.0.0.1:{first}/v1"),
        format!("http://127.0.0.1:{second}/v1"),
    ])
    .await;
    let ids: Vec<_> = gateway
        .view()
        .providers
        .iter()
        .map(|p| p.id.clone())
        .collect();
    update(
        &gateway,
        &dir,
        Edit::ConcurrencyProvider {
            id: ids[0].clone(),
            max_concurrency: 1,
        },
    );
    start(&gateway, &dir).await;
    let first_request = {
        let gateway = gateway.clone();
        tokio::spawn(async move {
            request(
                &gateway,
                "fixture-old",
                br#"{"model":"fixture-model","input":"old"}"#.to_vec(),
            )
            .await
        })
    };
    tokio::time::timeout(Duration::from_secs(2), entered.notified())
        .await
        .unwrap();
    let response = request(
        &gateway,
        "fixture-new",
        br#"{"model":"fixture-model","input":[{"role":"user","content":"new"}]}"#.to_vec(),
    )
    .await;
    let value: Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(value["provider"], "p2");
    release.notify_one();
    assert_eq!(first_request.await.unwrap().status(), 200);
    let again = request(
        &gateway,
        "fixture-new",
        br#"{"model":"fixture-model","input":"again"}"#.to_vec(),
    )
    .await;
    let again_value: Value =
        serde_json::from_slice(&again.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(again_value["provider"], "p2");
    gateway.stop().await.unwrap();
}

#[tokio::test]
async fn established_owner_retries_two_transient_failures_without_touching_backup() {
    let owner_hits = Arc::new(AtomicUsize::new(0));
    let owner_counter = owner_hits.clone();
    let owner = server(move |_| {
        let attempt = owner_counter.fetch_add(1, Ordering::SeqCst) + 1;
        async move {
            match attempt {
                1 | 4 => json_response(200, r#"{"provider":"owner"}"#, None),
                2 => json_response(502, r#"{"error":{"message":"fixture 502"}}"#, None),
                3 => json_response(503, r#"{"error":{"message":"fixture 503"}}"#, None),
                _ => json_response(200, r#"{"provider":"owner"}"#, None),
            }
        }
    })
    .await;
    let backup_hits = Arc::new(AtomicUsize::new(0));
    let backup_counter = backup_hits.clone();
    let backup = server(move |_| {
        backup_counter.fetch_add(1, Ordering::SeqCst);
        async { json_response(200, r#"{"provider":"backup"}"#, None) }
    })
    .await;
    let (dir, gateway) = fixture(vec![
        format!("http://127.0.0.1:{owner}/v1"),
        format!("http://127.0.0.1:{backup}/v1"),
    ])
    .await;
    let owner_id = gateway.view().providers[0].id.clone();
    update(
        &gateway,
        &dir,
        Edit::RpmProvider {
            id: owner_id,
            max_rpm: 10,
        },
    );
    start(&gateway, &dir).await;

    let first = request(
        &gateway,
        "fixture-sticky-transient",
        br#"{"model":"fixture-model","input":[{"role":"user","content":"first"}]}"#.to_vec(),
    )
    .await;
    assert_eq!(first.status(), 200);
    first.into_body().collect().await.unwrap();

    let retried = request(
        &gateway,
        "fixture-sticky-transient",
        br#"{"model":"fixture-model","input":"followup"}"#.to_vec(),
    )
    .await;
    assert_eq!(retried.status(), 200);
    let value: Value =
        serde_json::from_slice(&retried.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(value["provider"], "owner");
    assert_eq!(owner_hits.load(Ordering::SeqCst), 4);
    assert_eq!(backup_hits.load(Ordering::SeqCst), 0);
    assert_eq!(gateway.view().providers[0].rpm_used, 4);
    assert_eq!(gateway.view().providers[0].active_requests, 0);
    gateway.stop().await.unwrap();
}

#[tokio::test]
async fn transient_retry_after_keeps_concurrent_same_thread_on_owner() {
    let owner_hits = Arc::new(AtomicUsize::new(0));
    let owner_counter = owner_hits.clone();
    let owner = server(move |_| {
        let attempt = owner_counter.fetch_add(1, Ordering::SeqCst) + 1;
        async move {
            if attempt == 2 {
                json_response(
                    503,
                    r#"{"error":{"message":"fixture cooldown"}}"#,
                    Some("2"),
                )
            } else {
                json_response(200, r#"{"provider":"owner"}"#, None)
            }
        }
    })
    .await;
    let backup_hits = Arc::new(AtomicUsize::new(0));
    let backup_counter = backup_hits.clone();
    let backup = server(move |_| {
        backup_counter.fetch_add(1, Ordering::SeqCst);
        async { json_response(200, r#"{"provider":"backup"}"#, None) }
    })
    .await;
    let (dir, gateway) = fixture(vec![
        format!("http://127.0.0.1:{owner}/v1"),
        format!("http://127.0.0.1:{backup}/v1"),
    ])
    .await;
    start(&gateway, &dir).await;

    let first = request(
        &gateway,
        "fixture-cooldown-owner",
        br#"{"model":"fixture-model","input":[{"role":"user","content":"first"}]}"#.to_vec(),
    )
    .await;
    assert_eq!(first.status(), 200);
    first.into_body().collect().await.unwrap();

    let retrying = {
        let gateway = gateway.clone();
        tokio::spawn(async move {
            request(
                &gateway,
                "fixture-cooldown-owner",
                br#"{"model":"fixture-model","input":"retry"}"#.to_vec(),
            )
            .await
        })
    };
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if !gateway.view().transient_retries.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("owner transient wait was not registered");

    let concurrent = {
        let gateway = gateway.clone();
        tokio::spawn(async move {
            request(
                &gateway,
                "fixture-cooldown-owner",
                br#"{"model":"fixture-model","input":"concurrent"}"#.to_vec(),
            )
            .await
        })
    };
    let retried = tokio::time::timeout(Duration::from_secs(8), retrying)
        .await
        .expect("owner retry timed out")
        .unwrap();
    let concurrent = tokio::time::timeout(Duration::from_secs(8), concurrent)
        .await
        .expect("same-thread request timed out")
        .unwrap();
    assert_eq!(retried.status(), 200);
    assert_eq!(concurrent.status(), 200);
    retried.into_body().collect().await.unwrap();
    concurrent.into_body().collect().await.unwrap();
    assert_eq!(backup_hits.load(Ordering::SeqCst), 0);
    assert!(owner_hits.load(Ordering::SeqCst) >= 3);
    gateway.stop().await.unwrap();
}

#[tokio::test]
async fn hard_quota_rejection_falls_back_without_a_same_owner_retry() {
    let owner_hits = Arc::new(AtomicUsize::new(0));
    let owner_counter = owner_hits.clone();
    let owner = server(move |_| {
        let attempt = owner_counter.fetch_add(1, Ordering::SeqCst) + 1;
        async move {
            if attempt == 2 {
                json_response(
                    403,
                    r#"{"error":{"code":"insufficient_quota","message":"fixture quota"}}"#,
                    None,
                )
            } else {
                json_response(200, r#"{"provider":"owner"}"#, None)
            }
        }
    })
    .await;
    let backup_hits = Arc::new(AtomicUsize::new(0));
    let backup_counter = backup_hits.clone();
    let backup = server(move |_| {
        backup_counter.fetch_add(1, Ordering::SeqCst);
        async { json_response(200, r#"{"provider":"backup"}"#, None) }
    })
    .await;
    let (dir, gateway) = fixture(vec![
        format!("http://127.0.0.1:{owner}/v1"),
        format!("http://127.0.0.1:{backup}/v1"),
    ])
    .await;
    start(&gateway, &dir).await;

    let first = request(
        &gateway,
        "fixture-hard-quota",
        br#"{"model":"fixture-model","input":[{"role":"user","content":"first"}]}"#.to_vec(),
    )
    .await;
    assert_eq!(first.status(), 200);
    first.into_body().collect().await.unwrap();

    let response = request(
        &gateway,
        "fixture-hard-quota",
        br#"{"model":"fixture-model","input":"followup"}"#.to_vec(),
    )
    .await;
    assert_eq!(response.status(), 200);
    let value: Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(value["provider"], "backup");
    assert_eq!(owner_hits.load(Ordering::SeqCst), 2);
    assert_eq!(backup_hits.load(Ordering::SeqCst), 1);
    gateway.stop().await.unwrap();
}

#[tokio::test]
async fn merged_event_alias_resolves_and_final_event_keeps_canonical_cause() {
    let dir = tempfile::tempdir().unwrap();
    let service = crate::events::Service::new(dir.path());
    let provider_id = "00000000-0000-0000-0000-000000000020";
    let mut cause = crate::events::Record::new(
        Some(ClientId::Codex),
        Some(provider_id),
        Some("fixture-model"),
        crate::events::Reason::UpstreamService,
        crate::events::Action::RetryingSame,
        Some(503),
        Some(1),
    );
    cause.details.message = Some("fixture transient".into());
    let canonical_id = cause.id.clone();
    service.emit(cause);

    let wait_for = |service: crate::events::Service, id: String| async move {
        tokio::time::timeout(Duration::from_secs(3), async move {
            loop {
                if let Some(record) = service.detail(&id) {
                    break record;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("diagnostic record was not persisted")
    };
    wait_for(service.clone(), canonical_id.clone()).await;

    let mut duplicate = crate::events::Record::new(
        Some(ClientId::Codex),
        Some(provider_id),
        Some("fixture-model"),
        crate::events::Reason::UpstreamService,
        crate::events::Action::RetryingSame,
        Some(503),
        Some(2),
    );
    duplicate.details.message = Some("fixture transient".into());
    let alias_id = duplicate.id.clone();
    service.emit(duplicate);
    wait_for(service.clone(), alias_id.clone()).await;
    assert_eq!(service.detail(&alias_id).unwrap().id, canonical_id);

    let mut final_event = crate::events::Record::new(
        Some(ClientId::Codex),
        Some(provider_id),
        Some("fixture-model"),
        crate::events::Reason::FailoverExhausted,
        crate::events::Action::Returned,
        Some(503),
        Some(3),
    );
    final_event.details.cause_id = Some(alias_id.clone());
    let final_id = final_event.id.clone();
    service.emit(final_event);
    let final_record = wait_for(service.clone(), final_id).await;
    assert_eq!(
        final_record.details.cause_id.as_deref(),
        Some(canonical_id.as_str())
    );
    assert_eq!(service.query(crate::events::Filter::default()).total, 2);
}
