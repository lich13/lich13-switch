// 覆盖压缩边界交接、归属及原始请求透传。
//! Isolated protocol fixtures for compaction boundaries; never use production clients.
use super::*;
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use tokio::sync::Notify;

const WINDOW: &str = "fixture-opaque-window";
const NEXT_WINDOW: &str = "fixture-next-opaque-window";

struct CancelGuard(Arc<Notify>);

impl Drop for CancelGuard {
    fn drop(&mut self) {
        self.0.notify_one();
    }
}

fn automatic(g: &Gateway, t: &tempfile::TempDir) {
    update(
        g,
        t,
        Edit::Mode {
            mode: "auto".into(),
        },
    );
}
fn takeover_eligible(g: &Gateway, t: &tempfile::TempDir) {
    for provider in g.view().providers {
        update(
            g,
            t,
            Edit::TakeoverProvider {
                id: provider.id,
                handoff_after_compaction: Some(true),
                take_new_threads: Some(true),
            },
        );
    }
}
fn handoff_only_second(g: &Gateway, t: &tempfile::TempDir) {
    let id = g.view().providers.last().unwrap().id.clone();
    update(
        g,
        t,
        Edit::TakeoverProvider {
            id,
            handoff_after_compaction: Some(true),
            take_new_threads: Some(false),
        },
    );
}
fn promote_second(g: &Gateway, t: &tempfile::TempDir) {
    let mut ids: Vec<_> = g.view().providers.iter().map(|p| p.id.clone()).collect();
    ids.reverse();
    update(g, t, Edit::Reorder { ids });
}
fn input() -> Vec<u8> {
    input_for(WINDOW)
}
fn input_for(window: &str) -> Vec<u8> {
    json!({
        "model":"fixture-model",
        "input":[
            {"type":"compaction","encrypted_content":window},
            {"role":"user","content":"fixture"}
        ],
        "future":{"unchanged":true}
    })
    .to_string()
    .into_bytes()
}
async fn call(g: &Gateway, session: &str, path: &str, body: Vec<u8>) -> Value {
    let response = request(
        g,
        path,
        body,
        vec![
            ("content-type", "application/json"),
            ("session_id", session),
        ],
    )
    .await;
    assert_eq!(response.status(), 200);
    serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap()
}
async fn endpoint(name: &'static str, reject_window: bool) -> (u16, Arc<Mutex<Vec<Vec<u8>>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let captured = seen.clone();
    let port = server(move |req| {
        let captured = captured.clone();
        async move {
            let compact = req.uri().path().ends_with("/compact");
            let bytes = req.into_body().collect().await.unwrap().to_bytes().to_vec();
            captured.lock().unwrap().push(bytes.clone());
            let value: Value = serde_json::from_slice(&bytes).unwrap();
            if reject_window && value["input"].is_array() {
                return Response::builder().status(400).header("content-type", "application/json")
                    .body(full(r#"{"error":{"code":"invalid_encrypted_content","message":"compaction context unsupported"}}"#)).unwrap();
            }
            let window = value
                .get("fixture_next_window")
                .and_then(Value::as_str)
                .unwrap_or(WINDOW);
            Response::builder().header("content-type", "application/json").body(full(json!({
                "id":format!("fixture-response-{name}"),"object":"response","model":"fixture-model","status":"completed","provider":name,
                "output":if compact { json!([{"type":"compaction","encrypted_content":window}]) } else { json!([]) },
                "usage":{"input_tokens":25,"output_tokens":3,"input_tokens_details":{"cached_tokens":20}}
            }).to_string())).unwrap()
        }
    }).await;
    (port, seen)
}

#[tokio::test]
async fn old_thread_does_not_revive_consumed_compaction_boundary() {
    let (a, old) = endpoint("old", false).await;
    let (b, new) = endpoint("new", false).await;
    let (t, g) = fixture(vec![
        format!("http://127.0.0.1:{a}/v1"),
        format!("http://127.0.0.1:{b}/v1"),
    ])
    .await;
    let auth = std::fs::read(t.path().join("auth.json")).unwrap();
    automatic(&g, &t);
    takeover_eligible(&g, &t);
    start(&g, &t).await;
    let config = std::fs::read(t.path().join("config.toml")).unwrap();
    let ordinary = br#"{"model":"fixture-model","input":"fixture"}"#.to_vec();
    assert_eq!(
        call(&g, "fixture-existing", "/v1/responses", ordinary.clone()).await["provider"],
        "old"
    );
    promote_second(&g, &t);
    assert_eq!(
        call(&g, "fixture-existing", "/v1/responses", ordinary.clone()).await["provider"],
        "old"
    );
    assert_eq!(
        call(&g, "fixture-new", "/v1/responses", ordinary.clone()).await["provider"],
        "new"
    );
    assert_eq!(
        call(&g, "fixture-existing", "/v1/responses/compact", ordinary).await["provider"],
        "old"
    );
    let pinned = json!({"model":"fixture-model","previous_response_id":"fixture-response-old","input":[{"type":"compaction","encrypted_content":WINDOW}]}).to_string().into_bytes();
    assert_eq!(
        call(&g, "fixture-existing", "/v1/responses", pinned).await["provider"],
        "old"
    );
    let bytes = input();
    assert_eq!(
        call(&g, "fixture-existing", "/v1/responses", bytes.clone()).await["provider"],
        "old"
    );
    assert_eq!(old.lock().unwrap().last().unwrap(), &bytes);
    assert_eq!(old.lock().unwrap().len(), 5);
    assert_eq!(new.lock().unwrap().len(), 1);

    // A subsequent compaction response with a different encrypted window is
    // new evidence; only that fresh boundary permits the next handoff.
    let refresh = json!({
        "model":"fixture-model",
        "fixture_next_window":NEXT_WINDOW,
        "input":"fixture"
    })
    .to_string()
    .into_bytes();
    assert_eq!(
        call(&g, "fixture-existing", "/v1/responses/compact", refresh).await["provider"],
        "old"
    );
    let next = input_for(NEXT_WINDOW);
    assert_eq!(
        call(&g, "fixture-existing", "/v1/responses", next.clone()).await["provider"],
        "new"
    );
    assert_eq!(new.lock().unwrap().last().unwrap(), &next);
    assert_eq!(old.lock().unwrap().len(), 6);
    assert_eq!(new.lock().unwrap().len(), 2);
    assert_eq!(std::fs::read(t.path().join("config.toml")).unwrap(), config);
    assert_eq!(std::fs::read(t.path().join("auth.json")).unwrap(), auth);
    g.stop().await.unwrap();
}

#[tokio::test]
async fn incompatible_compaction_context_falls_back_without_circuit_failure_or_body_rewrite() {
    let (a, old) = endpoint("old", false).await;
    let (b, new) = endpoint("new", true).await;
    let (t, g) = fixture(vec![
        format!("http://127.0.0.1:{a}"),
        format!("http://127.0.0.1:{b}"),
    ])
    .await;
    automatic(&g, &t);
    handoff_only_second(&g, &t);
    start(&g, &t).await;
    let ordinary = br#"{"model":"fixture-model","input":"fixture"}"#.to_vec();
    call(
        &g,
        "fixture-fallback",
        "/codex/responses/compact",
        ordinary.clone(),
    )
    .await;
    promote_second(&g, &t);
    let bytes = json!({
        "model":"fixture-model",
        "prompt_cache_key":"fixture-prompt-cache-key",
        "input":[
            {"type":"compaction","encrypted_content":WINDOW},
            {"role":"user","content":"fixture"}
        ],
        "future":{"unchanged":true}
    })
    .to_string()
    .into_bytes();
    assert_eq!(
        call(&g, "fixture-fallback", "/codex/responses", bytes.clone()).await["provider"],
        "old"
    );
    assert_eq!(old.lock().unwrap().last().unwrap(), &bytes);
    assert_eq!(new.lock().unwrap().last().unwrap(), &bytes);
    assert_eq!(g.view().providers[0].health.failures, 0);
    assert_eq!(
        call(&g, "fixture-fallback", "/codex/responses", ordinary).await["provider"],
        "old"
    );
    g.stop().await.unwrap();
}

#[tokio::test]
async fn unobserved_compaction_never_authorizes_handoff() {
    let (a, _) = endpoint("old", false).await;
    let (b, new) = endpoint("new", false).await;
    let (t, g) = fixture(vec![
        format!("http://127.0.0.1:{a}"),
        format!("http://127.0.0.1:{b}"),
    ])
    .await;
    automatic(&g, &t);
    handoff_only_second(&g, &t);
    start(&g, &t).await;
    call(
        &g,
        "fixture-unconfirmed",
        "/codex/responses",
        br#"{"model":"fixture-model"}"#.to_vec(),
    )
    .await;
    promote_second(&g, &t);
    assert_eq!(
        call(&g, "fixture-unconfirmed", "/codex/responses", input()).await["provider"],
        "old"
    );
    assert!(new.lock().unwrap().is_empty());
    g.stop().await.unwrap();
}

#[tokio::test]
async fn failed_compaction_with_an_output_item_does_not_authorize_handoff() {
    let old = server(|request| async move {
        let compact = request.uri().path().ends_with("/compact");
        Response::builder().status(if compact {400} else {200})
            .header("content-type","application/json")
            .body(full(if compact {
                json!({"error":{"code":"invalid_request","message":"fixture rejected"},"status":"failed","output":[{"type":"compaction","encrypted_content":WINDOW}]}).to_string()
            } else {
                json!({"id":"fixture-sticky","object":"response","status":"completed","provider":"old"}).to_string()
            })).unwrap()
    }).await;
    let (new, seen) = endpoint("new", false).await;
    let (t, g) = fixture(vec![
        format!("http://127.0.0.1:{old}"),
        format!("http://127.0.0.1:{new}"),
    ])
    .await;
    automatic(&g, &t);
    handoff_only_second(&g, &t);
    start(&g, &t).await;
    let ordinary = br#"{"model":"fixture-model"}"#.to_vec();
    call(
        &g,
        "fixture-failed-compact",
        "/codex/responses",
        ordinary.clone(),
    )
    .await;
    promote_second(&g, &t);
    let rejected = request(
        &g,
        "/codex/responses/compact",
        ordinary,
        vec![
            ("content-type", "application/json"),
            ("session_id", "fixture-failed-compact"),
        ],
    )
    .await;
    assert_eq!(rejected.status(), 400);
    rejected.into_body().collect().await.unwrap();
    assert_eq!(
        call(&g, "fixture-failed-compact", "/codex/responses", input()).await["provider"],
        "old"
    );
    assert!(seen.lock().unwrap().is_empty());
    g.stop().await.unwrap();
}

async fn native_endpoint(name: &'static str) -> (u16, Arc<Mutex<Vec<Vec<u8>>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let captured = seen.clone();
    let port=server(move |mut request| {
        let captured=captured.clone();
        async move {
            let (response,upgrade)=yawc::WebSocket::upgrade(&mut request).unwrap();
            tokio::spawn(async move {
                let mut peer=upgrade.await.unwrap();
                while let Some(frame)=peer.next().await {
                    if frame.opcode().is_control() { if frame.opcode()==yawc::OpCode::Close {break;} continue; }
                    captured.lock().unwrap().push(frame.payload().to_vec());
                    let request:Value=serde_json::from_slice(frame.payload()).unwrap();
                    let compact=request["input"].as_array().is_some_and(|items|items.iter().any(|i|i["type"]=="compaction_trigger"));
                    let event=json!({"type":"response.completed","response":{"id":format!("fixture-native-{name}"),"model":"fixture-model","status":"completed","provider":name,"output":if compact {json!([{"type":"compaction","encrypted_content":WINDOW}])}else{json!([])},"usage":{"input_tokens":25,"output_tokens":3}}}).to_string();
                    if peer.send(yawc::Frame::text(event)).await.is_err(){break;}
                }
            });
            response.map(|_|replay::empty())
        }
    }).await;
    (port, seen)
}
async fn ws_turn(peer: &mut yawc::TcpWebSocket, text: String) -> Value {
    peer.send(yawc::Frame::text(text)).await.unwrap();
    let frame = tokio::time::timeout(Duration::from_secs(4), peer.next())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(frame.opcode(), yawc::OpCode::Text);
    serde_json::from_slice(frame.payload()).unwrap()
}
#[tokio::test]
async fn websocket_handoff_changes_only_upstream_between_completed_turns() {
    let (a, old) = native_endpoint("old").await;
    let (b, new) = native_endpoint("new").await;
    let (t, g) = fixture(vec![
        format!("http://127.0.0.1:{a}"),
        format!("http://127.0.0.1:{b}"),
    ])
    .await;
    automatic(&g, &t);
    takeover_eligible(&g, &t);
    start(&g, &t).await;
    let token = g.0.inner.lock().unwrap().store.local_token.clone();
    let mut peer = yawc::WebSocket::connect(
        format!("ws://127.0.0.1:{}/v1/responses", g.view().settings.port)
            .parse()
            .unwrap(),
    )
    .with_request(
        yawc::HttpRequest::builder()
            .header("authorization", format!("Bearer {token}"))
            .header("session_id", "fixture-native-session"),
    )
    .await
    .unwrap();
    let ordinary =
        r#"{"type":"response.create","model":"fixture-model","input":"fixture"}"#.to_string();
    assert_eq!(
        ws_turn(&mut peer, ordinary.clone()).await["response"]["provider"],
        "old"
    );
    promote_second(&g, &t);
    assert_eq!(
        ws_turn(&mut peer, ordinary).await["response"]["provider"],
        "old"
    );
    let compaction_trigger = json!({
        "type":"response.create",
        "model":"fixture-model",
        "prompt_cache_key":"fixture-prompt-cache-key",
        "input":[{"type":"compaction_trigger"}]
    })
    .to_string();
    assert_eq!(
        ws_turn(&mut peer, compaction_trigger.clone()).await["response"]["provider"],
        "old"
    );
    let mut window: Value = serde_json::from_slice(&input()).unwrap();
    window["type"] = "response.create".into();
    window["prompt_cache_key"] = "fixture-prompt-cache-key".into();
    let serialized = window.to_string();
    assert_eq!(
        ws_turn(&mut peer, serialized.clone()).await["response"]["provider"],
        "new"
    );
    assert_eq!(old.lock().unwrap().len(), 3);
    assert_eq!(old.lock().unwrap()[2], compaction_trigger.as_bytes());
    assert_eq!(new.lock().unwrap().len(), 1);
    assert_eq!(new.lock().unwrap()[0], serialized.as_bytes());
    drop(peer);
    g.stop().await.unwrap();
}

#[tokio::test]
async fn terminal_then_client_disconnect_keeps_completed_without_usage_tail() {
    let cancelled = Arc::new(Notify::new());
    let tail_generated = Arc::new(AtomicBool::new(false));
    let tail_release = Arc::new(Notify::new());
    let cancelled_copy = cancelled.clone();
    let generated_copy = tail_generated.clone();
    let release_copy = tail_release.clone();
    let port = server(move |_| {
        let cancelled = cancelled_copy.clone();
        let generated = generated_copy.clone();
        let release = release_copy.clone();
        async move {
            let guard = CancelGuard(cancelled);
            let chunks = async_stream::try_stream! {
                let _guard = guard;
                yield Frame::data(Bytes::from_static(b"event: response.completed\ndata: {\"response\":{\"id\":\"fixture-tail\",\"status\":\"completed\"}}\n\n"));
                release.notified().await;
                generated.store(true, Ordering::SeqCst);
                yield Frame::data(Bytes::from_static(b"event: response.usage\ndata: {\"response_id\":\"fixture-tail\",\"usage\":{\"input_tokens\":25,\"output_tokens\":3,\"input_tokens_details\":{\"cached_tokens\":20}}}\n\n"));
            };
            Response::builder()
                .header("content-type", "text/event-stream")
                .body(
                    StreamBody::new(chunks)
                        .map_err(|e: std::io::Error| -> connector::BoxError { e.into() })
                        .boxed_unsync(),
                )
                .unwrap()
        }
    }).await;
    let (t, g) = fixture(vec![format!("http://127.0.0.1:{port}")]).await;
    let usage = crate::usage::Service::new(t.path());
    g.set_usage(usage.clone());
    start(&g, &t).await;
    let (response, sender, task) = super::tests::request_with_reset(
        &g,
        "/codex/responses",
        br#"{"model":"fixture-model","stream":true}"#.to_vec(),
        vec![("content-type", "application/json")],
    )
    .await;
    let mut body = response.into_body();
    assert!(body
        .frame()
        .await
        .unwrap()
        .unwrap()
        .data_ref()
        .unwrap()
        .windows(18)
        .any(|w| w == b"response.completed"));
    super::tests::reset_and_drop(task, sender, body).await;
    tokio::time::timeout(Duration::from_secs(3), cancelled.notified())
        .await
        .expect("upstream stream was not cancelled after client disconnect");
    let row = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let rows = usage
                .read(|store| Ok(store.logs(&crate::usage::model::Filter::default())?.rows))
                .unwrap();
            if let Some(row) = rows.into_iter().next() {
                break row;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let attempt = row.final_attempt().unwrap();
    assert!(row.completed);
    assert_eq!(attempt.outcome, "success");
    assert_eq!(attempt.tokens.input, None);
    assert_eq!(attempt.tokens.cache_read, None);
    assert_eq!(attempt.tokens.output, None);
    assert!(!tail_generated.load(Ordering::SeqCst));
    assert_eq!(g.view().providers[0].active_requests, 0);
    g.stop().await.unwrap();
}

fn history() -> Vec<u8> {
    json!({"model":"fixture-model","input":[
        {"role":"user","content":"fixture question"},
        {"role":"assistant","content":"fixture answer"},
        {"role":"user","content":"fixture followup"}
    ]})
    .to_string()
    .into_bytes()
}
async fn hold(g: &Gateway, id: &str) -> admission::Admission {
    g.0.admission
        .acquire(
            &[g.route(id).unwrap()],
            false,
            100,
            &mut admission::Budget::new(3),
        )
        .await
        .unwrap()
}
fn one_slot(g: &Gateway, t: &tempfile::TempDir) {
    let mut settings = g.view().settings;
    settings.thread_idle_release_seconds = 1;
    update(g, t, Edit::Settings { settings });
    for provider in g.view().providers {
        update(
            g,
            t,
            Edit::ConcurrencyProvider {
                id: provider.id,
                max_concurrency: 1,
            },
        );
    }
}
#[tokio::test]
async fn established_full_owner_uses_next_provider_only_after_capacity_release_and_success() {
    let (a, _) = endpoint("old", false).await;
    let (b, _) = endpoint("new", false).await;
    let (t, g) = fixture(vec![
        format!("http://127.0.0.1:{a}"),
        format!("http://127.0.0.1:{b}"),
    ])
    .await;
    automatic(&g, &t);
    one_slot(&g, &t);
    start(&g, &t).await;
    let session = "fixture-full-owner";
    call(
        &g,
        session,
        "/v1/responses",
        br#"{"model":"fixture-model","input":"fixture"}"#.to_vec(),
    )
    .await;
    let key = crate::storage::digest(session.as_bytes());
    let ids = g.routing_ids(None);
    let stale =
        g.0.compaction
            .prepare(key.clone(), None, false, &ids)
            .unwrap();
    let old = hold(&g, &ids[0]).await;
    let new = hold(&g, &ids[1]).await;
    let task = {
        let g = g.clone();
        tokio::spawn(async move { call(&g, session, "/v1/responses", history()).await })
    };
    tokio::time::timeout(Duration::from_secs(3), async {
        while g.view().waiting_requests != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(g.0.compaction.owner(&key), Some(ids[0].clone()));
    assert!(!task.is_finished());
    drop(new);
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(3), task)
            .await
            .unwrap()
            .unwrap()["provider"],
        "new"
    );
    tokio::time::timeout(Duration::from_secs(3), async {
        while g.0.compaction.owner(&key) != Some(ids[1].clone()) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    stale.complete(&ids[0], None);
    assert_eq!(g.0.compaction.owner(&key), Some(ids[1].clone()));
    drop(old);
    assert_eq!(
        call(&g, session, "/v1/responses", history()).await["provider"],
        "new"
    );
    assert!(g.view().providers.iter().all(|p| p.health.failures == 0));
    g.stop().await.unwrap();
}

#[tokio::test]
async fn full_owner_failed_receiver_and_unknown_history_do_not_move_ownership() {
    let (a, _) = endpoint("old", false).await;
    let b = server(|_| async {
        Response::builder()
            .status(400)
            .body(full(r#"{"error":{"message":"fixture rejected"}}"#))
            .unwrap()
    })
    .await;
    let (t, g) = fixture(vec![
        format!("http://127.0.0.1:{a}"),
        format!("http://127.0.0.1:{b}"),
    ])
    .await;
    automatic(&g, &t);
    one_slot(&g, &t);
    let mut settings = g.view().settings;
    settings.queue_seconds = 1;
    settings.max_retries = 0;
    update(&g, &t, Edit::Settings { settings });
    start(&g, &t).await;
    let session = "fixture-failed-receiver";
    call(
        &g,
        session,
        "/v1/responses",
        br#"{"model":"fixture-model","input":"fixture"}"#.to_vec(),
    )
    .await;
    let ids = g.routing_ids(None);
    let key = crate::storage::digest(session.as_bytes());
    let old = hold(&g, &ids[0]).await;
    let failed = request(
        &g,
        "/v1/responses",
        history(),
        vec![
            ("session_id", session),
            ("content-type", "application/json"),
        ],
    )
    .await;
    assert_eq!(failed.status(), 400);
    failed.into_body().collect().await.unwrap();
    assert_eq!(g.0.compaction.owner(&key), Some(ids[0].clone()));
    let unknown = request(
        &g,
        "/v1/responses",
        br#"{"model":"fixture-model","input":{"unknown":true}}"#.to_vec(),
        vec![
            ("session_id", session),
            ("content-type", "application/json"),
        ],
    )
    .await;
    assert_eq!(unknown.status(), 429);
    assert_eq!(g.0.compaction.owner(&key), Some(ids[0].clone()));
    drop(old);
    g.stop().await.unwrap();
}

#[tokio::test]
async fn websocket_existing_connection_reselects_before_sending_when_owner_is_full() {
    use futures_util::SinkExt;
    let (a, old_seen) = native_endpoint("old").await;
    let (b, new_seen) = native_endpoint("new").await;
    let (t, g) = fixture(vec![
        format!("http://127.0.0.1:{a}"),
        format!("http://127.0.0.1:{b}"),
    ])
    .await;
    automatic(&g, &t);
    one_slot(&g, &t);
    start(&g, &t).await;
    let token = g.0.inner.lock().unwrap().store.local_token.clone();
    let mut peer = yawc::WebSocket::connect(
        format!("ws://127.0.0.1:{}/v1/responses", g.view().settings.port)
            .parse()
            .unwrap(),
    )
    .with_request(
        yawc::HttpRequest::builder()
            .header("authorization", format!("Bearer {token}"))
            .header("session_id", "fixture-native-capacity"),
    )
    .await
    .unwrap();
    assert_eq!(
        ws_turn(
            &mut peer,
            r#"{"type":"response.create","model":"fixture-model","input":"fixture"}"#.into()
        )
        .await["response"]["provider"],
        "old"
    );
    let ids = g.routing_ids(None);
    let old = hold(&g, &ids[0]).await;
    let mut payload: Value = serde_json::from_slice(&history()).unwrap();
    payload["type"] = json!("response.create");
    assert_eq!(
        ws_turn(&mut peer, payload.to_string()).await["response"]["provider"],
        "new"
    );
    assert_eq!(old_seen.lock().unwrap().len(), 1);
    assert_eq!(new_seen.lock().unwrap().len(), 1);
    let key = crate::storage::digest(b"fixture-native-capacity");
    tokio::time::timeout(Duration::from_secs(3), async {
        while g.0.compaction.owner(&key) != Some(ids[1].clone()) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    drop(old);
    let _ = peer.close().await;
    g.stop().await.unwrap();
}
