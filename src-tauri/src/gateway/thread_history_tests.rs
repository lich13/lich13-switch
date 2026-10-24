// 覆盖历史输入、压缩边界和线程接管保护。
//! v0.21.1 thread identity, warmup, and compaction handoff fixtures.
//!
//! All upstreams and request identities in this module are synthetic.  The
//! fixtures deliberately use the new header spellings so the HTTP and native
//! WebSocket paths exercise the same ownership key.
use super::*;
use crate::gateway::{
    compaction::{self, Intake, Registry},
    model::{Edit, Provider},
    replay,
};
use futures_util::{SinkExt, StreamExt};
use http_body_util::BodyExt;
use hyper::{
    body::Incoming,
    header::{HeaderMap, HeaderValue},
    Request, Response,
};
use hyper_util::{client::legacy::Client, rt::TokioExecutor};
use serde_json::{json, Value};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

const WINDOW: &str = "fixture-v0211-window";

fn provider_with_policy(id: &str, handoff: bool, take: bool) -> Provider {
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

fn new_headers(thread: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert("thread-id", HeaderValue::from_str(thread).unwrap());
    headers.insert(
        "session-id",
        HeaderValue::from_static("fixture-cache-affinity"),
    );
    headers
}

fn header_session(headers: &HeaderMap) -> String {
    compaction::session(headers).expect("fixture thread identity")
}

async fn request_with_headers(
    gateway: &Gateway,
    path: &str,
    body: Vec<u8>,
    headers: &HeaderMap,
) -> Response<Incoming> {
    let client: Client<_, replay::WireBody> = Client::builder(TokioExecutor::new()).build(
        super::super::connector::Connector::new(Duration::from_secs(2), 0),
    );
    let (token, port) = {
        let state = gateway.0.inner.lock().unwrap();
        (state.store.local_token.clone(), state.store.settings.port)
    };
    let mut request = Request::builder()
        .method("POST")
        .uri(format!("http://127.0.0.1:{port}{path}"))
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(replay::full(body))
        .unwrap();
    for (name, value) in headers {
        request.headers_mut().append(name.clone(), value.clone());
    }
    client.request(request).await.unwrap()
}

async fn response_value(response: Response<Incoming>) -> Value {
    serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap()
}

fn automatic(gateway: &Gateway, dir: &tempfile::TempDir) {
    update(
        gateway,
        dir,
        Edit::Mode {
            mode: "auto".into(),
        },
    );
}

fn enable_takeover(gateway: &Gateway, dir: &tempfile::TempDir) {
    for provider in gateway.view().providers {
        update(
            gateway,
            dir,
            Edit::TakeoverProvider {
                id: provider.id,
                handoff_after_compaction: Some(true),
                take_new_threads: Some(true),
            },
        );
    }
}

fn handoff_only_last(gateway: &Gateway, dir: &tempfile::TempDir) {
    let ids: Vec<_> = gateway
        .view()
        .providers
        .iter()
        .map(|provider| provider.id.clone())
        .collect();
    for (index, id) in ids.into_iter().enumerate() {
        update(
            gateway,
            dir,
            Edit::TakeoverProvider {
                id,
                handoff_after_compaction: Some(index == 1),
                take_new_threads: Some(false),
            },
        );
    }
}

fn promote_last(gateway: &Gateway, dir: &tempfile::TempDir) {
    let mut ids: Vec<_> = gateway
        .view()
        .providers
        .iter()
        .map(|provider| provider.id.clone())
        .collect();
    ids.reverse();
    update(gateway, dir, Edit::Reorder { ids });
}

async fn wait_settled(gateway: &Gateway) {
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
fn v0211_session_identity_prefers_thread_alias_and_ignores_cache_affinity() {
    let mut headers = new_headers("thread-a");
    headers.insert("session_id", HeaderValue::from_static("legacy-a"));
    assert_eq!(header_session(&headers), digest("thread-a"));

    let mut aliases = HeaderMap::new();
    aliases.insert("thread-id", HeaderValue::from_static("thread-a"));
    aliases.insert("thread_id", HeaderValue::from_static("thread-a"));
    aliases.insert("conversation_id", HeaderValue::from_static("thread-a"));
    aliases.insert("session-id", HeaderValue::from_static("shared-cache"));
    assert_eq!(header_session(&aliases), digest("thread-a"));

    let mut legacy = HeaderMap::new();
    legacy.insert("session_id", HeaderValue::from_static("legacy-thread"));
    assert_eq!(header_session(&legacy), digest("legacy-thread"));

    let mut ignored = HeaderMap::new();
    ignored.insert("session-id", HeaderValue::from_static("cache-only"));
    assert!(compaction::session(&ignored).is_none());
}

#[test]
fn v0211_invalid_or_conflicting_thread_aliases_fail_closed() {
    let mut conflicting = HeaderMap::new();
    conflicting.insert("thread-id", HeaderValue::from_static("thread-a"));
    conflicting.insert("thread_id", HeaderValue::from_static("thread-b"));
    assert!(compaction::session(&conflicting).is_none());

    let mut explicit_legacy_conflict = HeaderMap::new();
    explicit_legacy_conflict.insert("thread-id", HeaderValue::from_static("thread-a"));
    explicit_legacy_conflict.insert("session_id", HeaderValue::from_static("legacy-b"));
    assert_eq!(
        header_session(&explicit_legacy_conflict),
        digest("thread-a")
    );

    let mut empty = HeaderMap::new();
    empty.insert("thread-id", HeaderValue::from_static(""));
    assert!(compaction::session(&empty).is_none());

    let mut control = HeaderMap::new();
    control.insert(
        "thread-id",
        HeaderValue::from_bytes(b"thread-\xff-invalid").unwrap(),
    );
    assert!(compaction::session(&control).is_none());

    let mut oversized = HeaderMap::new();
    oversized.insert(
        "thread-id",
        HeaderValue::from_str(&"x".repeat(257)).unwrap(),
    );
    assert!(compaction::session(&oversized).is_none());
}

#[test]
fn v0211_same_cache_affinity_with_different_threads_isolated_in_registry() {
    let first = new_headers("thread-a");
    let second = new_headers("thread-b");
    let first_session = header_session(&first);
    let second_session = header_session(&second);
    assert_ne!(first_session, second_session);

    let dir = tempfile::tempdir().unwrap();
    let registry = Registry::new(dir.path().join("owners.json"));
    let providers = [
        provider_with_policy("old", false, false),
        provider_with_policy("new", false, false),
    ];
    let first_hints = hints(json!({"input":[{"role":"user","content":"a"}]}));
    let second_hints = hints(json!({"input":[{"role":"user","content":"b"}]}));
    let first_lease = registry
        .prepare_policy(first_session, Some(&first_hints), true, &providers)
        .unwrap();
    let second_lease = registry
        .prepare_policy(second_session, Some(&second_hints), true, &providers)
        .unwrap();
    assert!(first_lease.fresh);
    assert!(second_lease.fresh);
    assert!(first_lease.admitted("old"));
    assert!(second_lease.admitted("new"));
}

#[test]
fn v0211_multiple_initial_messages_prove_first_turn_but_auxiliary_does_not() {
    let first = hints(json!({
        "input":[
            {"role":"system","content":"fixture policy"},
            {"role":"developer","content":"fixture developer"},
            {"role":"user","content":"first"},
            {"role":"user","content":"second"}
        ]
    }));
    assert!(first.first_turn);
    assert!(!first.auxiliary);

    let auxiliary = hints(json!({
        "generate":false,
        "input":[
            {"role":"system","content":"fixture policy"},
            {"role":"user","content":"warmup"}
        ]
    }));
    assert!(auxiliary.auxiliary);
    assert!(!auxiliary.first_turn);

    let cursor = hints(json!({
        "previous_response_id":"fixture-old-response",
        "input":[{"role":"user","content":"continuation"}]
    }));
    assert!(!cursor.first_turn);
}

#[test]
fn v0211_unadmitted_history_cannot_be_upgraded_to_new_thread() {
    let dir = tempfile::tempdir().unwrap();
    let registry = Registry::new(dir.path().join("owners.json"));
    let session = digest("fixture-history-before-admission");
    let ordinary = provider_with_policy("ordinary", false, false);
    let new_only = provider_with_policy("new-only", false, true);
    let providers = [ordinary.clone(), new_only.clone()];

    // A history-shaped request can create an establishing entry, but it never
    // establishes the provider or records first-turn eligibility by itself.
    let history = hints(json!({
        "input":[
            {"role":"user","content":"prior"},
            {"role":"assistant","content":"answer"}
        ]
    }));
    assert!(!history.first_turn);
    let observed = registry
        .prepare_policy(session.clone(), Some(&history), true, &providers)
        .unwrap();
    assert!(observed.fresh);
    assert_eq!(observed.admission_policy().0, Intake::Ordinary);
    drop(observed);

    // A later user-only delta must not retroactively turn the same pending
    // history entry into a protected new-thread request.
    let user_only = hints(json!({
        "input":[{"role":"user","content":"delta"}]
    }));
    assert!(user_only.first_turn);
    let later = registry
        .prepare_policy(
            session,
            Some(&user_only),
            true,
            &[new_only.clone(), ordinary.clone()],
        )
        .unwrap();
    assert_eq!(later.admission_policy().0, Intake::Ordinary);
    assert_eq!(
        later.decision(Some(&user_only), "new-only",),
        crate::usage::model::RoutingDecision::OrdinaryQueue
    );

    // A first-turn lease prepared in parallel cannot seize a history-confirmed
    // session for the new-thread-only provider, but can fall back to ordinary.
    let parallel_session = digest("fixture-history-parallel-first");
    let first = hints(json!({
        "input":[{"role":"user","content":"first"}]
    }));
    let pending_first = registry
        .prepare_policy(
            parallel_session.clone(),
            Some(&first),
            true,
            &[new_only.clone(), ordinary.clone()],
        )
        .unwrap();
    assert_eq!(pending_first.admission_policy().0, Intake::NewThread);
    let history = hints(json!({
        "input":[
            {"role":"user","content":"prior"},
            {"role":"assistant","content":"answer"},
            {"role":"user","content":"continuation"}
        ]
    }));
    let history_lease = registry
        .prepare_policy(
            parallel_session,
            Some(&history),
            true,
            &[ordinary.clone(), new_only.clone()],
        )
        .unwrap();
    assert_eq!(history_lease.admission_policy().0, Intake::Ordinary);
    assert_eq!(pending_first.admission_policy().0, Intake::Ordinary);
    assert!(!pending_first.admitted("new-only"));
    assert!(history_lease.admitted("ordinary"));
    assert!(pending_first.admitted("ordinary"));
}

#[test]
fn v0211_unadmitted_first_turn_cannot_keep_new_thread_intake_for_history() {
    let dir = tempfile::tempdir().unwrap();
    let registry = Registry::new(dir.path().join("owners.json"));
    let session = digest("fixture-unadmitted-first-turn");
    let ordinary = provider_with_policy("ordinary", false, false);
    let new_only = provider_with_policy("new-only", false, true);
    let providers = [ordinary.clone(), new_only.clone()];

    // Prepare the first-turn lease but leave it unadmitted, as a competing
    // request would while the initial route is still waiting for a slot.
    let first = hints(json!({
        "input":[
            {"role":"system","content":"fixture policy"},
            {"role":"user","content":"first"}
        ]
    }));
    let pending = registry
        .prepare_policy(session.clone(), Some(&first), true, &providers)
        .unwrap();
    assert_eq!(pending.admission_policy(), (Intake::NewThread, None));
    assert!(pending.establishing());
    drop(pending);

    for value in [
        json!({"input":[{"role":"assistant","content":"answer"}]}),
        json!({
            "input":[
                {"role":"user","content":"prior"},
                {"role":"assistant","content":"answer"},
                {"role":"user","content":"history continuation"}
            ]
        }),
    ] {
        let history = hints(value);
        assert!(!history.first_turn);
        let lease = registry
            .prepare_policy(session.clone(), Some(&history), true, &providers)
            .unwrap();
        assert_eq!(lease.admission_policy(), (Intake::Ordinary, None));
        assert_eq!(
            lease.decision(Some(&history), "new-only"),
            crate::usage::model::RoutingDecision::OrdinaryQueue
        );
        drop(lease);
    }
}

#[test]
fn v0211_old_cursor_cannot_authorize_compaction_handoff() {
    let dir = tempfile::tempdir().unwrap();
    let registry = Registry::new(dir.path().join("owners.json"));
    let session = digest("fixture-cursor-thread");
    let old = provider_with_policy("old", true, false);
    let new = provider_with_policy("new", true, false);
    let initial = registry
        .prepare_policy(
            session.clone(),
            Some(&hints(json!({"input":"ordinary"}))),
            false,
            &[old.clone(), new.clone()],
        )
        .unwrap();
    assert!(initial.admitted("old"));
    let boundary = digest(WINDOW);
    initial.complete("old", Some(&boundary));

    let cursor = hints(json!({
        "previous_response_id":"fixture-old-response",
        "input":[{"type":"compaction","encrypted_content":WINDOW}]
    }));
    assert!(!cursor.first_turn);
    let lease = registry
        .prepare_policy(session, Some(&cursor), false, &[new, old])
        .unwrap();
    assert!(!lease.handoff);
    assert_eq!(lease.owner.as_deref(), Some("old"));
}

#[test]
fn v0211_parallel_compaction_boundary_claims_once() {
    let dir = tempfile::tempdir().unwrap();
    let registry = Registry::new(dir.path().join("owners.json"));
    let session = digest("fixture-parallel-thread");
    let old = provider_with_policy("old", true, false);
    let new = provider_with_policy("new", true, false);
    let initial = registry
        .prepare_policy(
            session.clone(),
            Some(&hints(json!({"input":"ordinary"}))),
            false,
            &[old.clone(), new.clone()],
        )
        .unwrap();
    assert!(initial.admitted("old"));
    let boundary = digest(WINDOW);
    initial.complete("old", Some(&boundary));

    let compacted = RequestHints {
        compacted_window: Some(boundary.clone()),
        ..Default::default()
    };
    let first = registry
        .prepare_policy(
            session.clone(),
            Some(&compacted),
            true,
            &[new.clone(), old.clone()],
        )
        .unwrap();
    let parallel = registry
        .prepare_policy(session.clone(), Some(&compacted), true, &[new, old])
        .unwrap();
    assert!(first.handoff);
    assert!(!parallel.handoff);
    assert_eq!(parallel.owner.as_deref(), Some("old"));

    first.complete("new", Some(&boundary));
    parallel.complete("old", Some(&boundary));
    assert!(!registry.boundary_matches(&session, Some(&boundary)));
}

#[test]
fn v0211_restart_restores_owner_and_compaction_boundary_from_thread_identity() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("owners.json");
    let session = header_session(&new_headers("fixture-restart-thread"));
    let old = provider_with_policy("old", true, false);
    let new = provider_with_policy("new", true, false);
    let boundary = digest(WINDOW);
    let registry = Registry::new(path.clone());
    let first = registry
        .prepare_policy(
            session.clone(),
            Some(&hints(json!({"input":"ordinary"}))),
            false,
            &[old.clone(), new.clone()],
        )
        .unwrap();
    assert!(first.admitted("old"));
    first.complete("old", Some(&boundary));
    registry.flush();
    drop(first);

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
    assert!(lease.handoff);
    assert_eq!(lease.owner.as_deref(), Some("old"));
}

#[tokio::test]
async fn v0211_http_generate_false_warmup_preserves_new_thread_eligibility() {
    let first =
        server(|_| async { Response::new(replay::full(r#"{"provider":"ordinary"}"#)) }).await;
    let second =
        server(|_| async { Response::new(replay::full(r#"{"provider":"new-thread"}"#)) }).await;
    let (dir, gateway) = fixture(vec![
        format!("http://127.0.0.1:{first}/v1"),
        format!("http://127.0.0.1:{second}/v1"),
    ])
    .await;
    automatic(&gateway, &dir);
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
    start(&gateway, &dir).await;

    let headers = new_headers("fixture-warmup-thread");
    let warmup = request_with_headers(
        &gateway,
        "/v1/responses",
        br#"{"model":"fixture-model","generate":false,"input":[{"role":"user","content":"warmup"}]}"#.to_vec(),
        &headers,
    )
    .await;
    assert_eq!(warmup.status(), 200);
    assert_eq!(response_value(warmup).await["provider"], "ordinary");

    let generated = request_with_headers(
        &gateway,
        "/v1/responses",
        br#"{"model":"fixture-model","generate":true,"input":[{"role":"user","content":"real"}]}"#
            .to_vec(),
        &headers,
    )
    .await;
    assert_eq!(generated.status(), 200);
    assert_eq!(response_value(generated).await["provider"], "new-thread");

    // Once a real turn has established the owner, the same auxiliary request
    // follows that owner even when queue order is restored.
    update(&gateway, &dir, Edit::Reorder { ids: ids.clone() });
    let owner_warmup = request_with_headers(
        &gateway,
        "/v1/responses",
        br#"{"model":"fixture-model","generate":false,"input":[{"role":"user","content":"owner warmup"}]}"#.to_vec(),
        &headers,
    )
    .await;
    assert_eq!(owner_warmup.status(), 200);
    assert_eq!(response_value(owner_warmup).await["provider"], "new-thread");
    gateway.stop().await.unwrap();
}

async fn compaction_endpoint(name: &'static str) -> (u16, Arc<Mutex<Vec<Vec<u8>>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let captured = seen.clone();
    let port = server(move |request| {
        let captured = captured.clone();
        async move {
            let compact = request.uri().path().ends_with("/compact");
            let bytes = request
                .into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes()
                .to_vec();
            captured.lock().unwrap().push(bytes);
            let output = if compact {
                json!([{"type":"compaction","encrypted_content":WINDOW}])
            } else {
                json!([])
            };
            Response::builder()
                .header("content-type", "application/json")
                .body(replay::full(
                    json!({
                        "id":format!("fixture-{name}"),
                        "object":"response",
                        "status":"completed",
                        "provider":name,
                        "output":output
                    })
                    .to_string(),
                ))
                .unwrap()
        }
    })
    .await;
    (port, seen)
}

#[tokio::test]
async fn v0211_http_window_handoff_uses_new_thread_header_and_preserves_bytes() {
    let (old_port, old_seen) = compaction_endpoint("old").await;
    let (new_port, new_seen) = compaction_endpoint("new").await;
    let (dir, gateway) = fixture(vec![
        format!("http://127.0.0.1:{old_port}/v1"),
        format!("http://127.0.0.1:{new_port}/v1"),
    ])
    .await;
    automatic(&gateway, &dir);
    handoff_only_last(&gateway, &dir);
    start(&gateway, &dir).await;
    let headers = new_headers("fixture-http-handoff");
    let ordinary = br#"{"model":"fixture-model","input":"ordinary"}"#.to_vec();
    let response = request_with_headers(&gateway, "/v1/responses", ordinary, &headers).await;
    assert_eq!(response.status(), 200);
    assert_eq!(response_value(response).await["provider"], "old");

    promote_last(&gateway, &dir);
    let compact = request_with_headers(
        &gateway,
        "/v1/responses/compact",
        br#"{"model":"fixture-model","input":"compact"}"#.to_vec(),
        &headers,
    )
    .await;
    assert_eq!(compact.status(), 200);
    assert_eq!(response_value(compact).await["provider"], "old");

    let window = br#" {"model":"fixture-model","input":[{"type":"compaction","encrypted_content":"fixture-v0211-window"},{"role":"user","content":"fixture"}],"future":{"unchanged":true},"large":90071992547409931234567890} "#.to_vec();
    let handed = request_with_headers(&gateway, "/v1/responses", window.clone(), &headers).await;
    assert_eq!(handed.status(), 200);
    assert_eq!(response_value(handed).await["provider"], "new");
    assert_eq!(new_seen.lock().unwrap().last(), Some(&window));
    assert!(old_seen.lock().unwrap().len() >= 2);
    gateway.stop().await.unwrap();
}

async fn native_endpoint(name: &'static str) -> (u16, Arc<Mutex<Vec<Vec<u8>>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let captured = seen.clone();
    let port = server(move |mut request| {
        let captured = captured.clone();
        async move {
            let (response, upgrade) = yawc::WebSocket::upgrade(&mut request).unwrap();
            tokio::spawn(async move {
                let Ok(mut peer) = upgrade.await else {
                    return;
                };
                while let Some(frame) = peer.next().await {
                    if frame.opcode().is_control() {
                        if frame.opcode() == yawc::OpCode::Close {
                            break;
                        }
                        continue;
                    }
                    let payload = frame.payload().to_vec();
                    let value: Value = serde_json::from_slice(&payload).unwrap();
                    captured.lock().unwrap().push(payload);
                    let compact = value["input"].as_array().is_some_and(|items| {
                        items
                            .iter()
                            .any(|item| item["type"] == "compaction_trigger")
                    });
                    let output = if compact {
                        json!([{"type":"compaction","encrypted_content":WINDOW}])
                    } else {
                        json!([])
                    };
                    let event = json!({
                        "type":"response.completed",
                        "response":{
                            "id":format!("fixture-native-{name}"),
                            "model":"fixture-model",
                            "status":"completed",
                            "provider":name,
                            "output":output
                        }
                    });
                    if peer
                        .send(yawc::Frame::text(event.to_string()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            });
            response.map(|_| replay::empty())
        }
    })
    .await;
    (port, seen)
}

async fn ws_turn(peer: &mut yawc::TcpWebSocket, body: String) -> Value {
    peer.send(yawc::Frame::text(body)).await.unwrap();
    let frame = tokio::time::timeout(Duration::from_secs(5), peer.next())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(frame.opcode(), yawc::OpCode::Text);
    serde_json::from_slice(frame.payload()).unwrap()
}

#[tokio::test]
async fn v0211_native_ws_reuses_thread_identity_across_turns_and_hands_off_once() {
    let (old_port, old_seen) = native_endpoint("old").await;
    let (new_port, new_seen) = native_endpoint("new").await;
    let (dir, gateway) = fixture(vec![
        format!("http://127.0.0.1:{old_port}/v1"),
        format!("http://127.0.0.1:{new_port}/v1"),
    ])
    .await;
    automatic(&gateway, &dir);
    enable_takeover(&gateway, &dir);
    start(&gateway, &dir).await;
    let token = gateway.0.inner.lock().unwrap().store.local_token.clone();
    let mut peer = yawc::WebSocket::connect(
        format!(
            "ws://127.0.0.1:{}/v1/responses",
            gateway.view().settings.port
        )
        .parse()
        .unwrap(),
    )
    .with_request(
        yawc::HttpRequest::builder()
            .header("authorization", format!("Bearer {token}"))
            .header("thread-id", "fixture-native-v0211")
            .header("session-id", "fixture-cache-affinity"),
    )
    .await
    .unwrap();

    let ordinary = r#"{"type":"response.create","model":"fixture-model","input":"ordinary"}"#;
    assert_eq!(
        ws_turn(&mut peer, ordinary.into()).await["response"]["provider"],
        "old"
    );
    promote_last(&gateway, &dir);
    assert_eq!(
        ws_turn(&mut peer, ordinary.into()).await["response"]["provider"],
        "old"
    );
    assert_eq!(
        ws_turn(
            &mut peer,
            json!({
                "type":"response.create",
                "model":"fixture-model",
                "input":[{"type":"compaction_trigger"}]
            })
            .to_string(),
        )
        .await["response"]["provider"],
        "old"
    );
    let window = json!({
        "type":"response.create",
        "model":"fixture-model",
        "input":[
            {"type":"compaction","encrypted_content":WINDOW},
            {"role":"user","content":"fixture"}
        ],
        "future":{"unchanged":true}
    })
    .to_string();
    assert_eq!(
        ws_turn(&mut peer, window.clone()).await["response"]["provider"],
        "new"
    );
    wait_settled(&gateway).await;
    assert_eq!(old_seen.lock().unwrap().len(), 3);
    assert_eq!(
        new_seen.lock().unwrap().last().map(Vec::as_slice),
        Some(window.as_bytes())
    );

    peer.send(yawc::Frame::close(1000.into(), "fixture done"))
        .await
        .unwrap();
    drop(peer);
    gateway.stop().await.unwrap();
}
