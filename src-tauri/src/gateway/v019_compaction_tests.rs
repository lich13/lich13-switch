//! Isolated protocol fixtures for compaction boundaries; never use production clients.
use super::*;
use serde_json::{json, Value};

const WINDOW: &str = "fixture-opaque-window";
fn automatic(g: &Gateway, t: &tempfile::TempDir) {
    update(
        g,
        t,
        Edit::Mode {
            mode: "auto".into(),
        },
    );
}
fn promote_second(g: &Gateway, t: &tempfile::TempDir) {
    let mut ids: Vec<_> = g.view().providers.iter().map(|p| p.id.clone()).collect();
    ids.reverse();
    update(g, t, Edit::Reorder { ids });
}
fn input() -> Vec<u8> {
    br#"{ "model":"fixture-model", "input":[{"type":"compaction","encrypted_content":"fixture-opaque-window"},{"role":"user","content":"fixture"}],"future":{"unchanged":true} }"#.to_vec()
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
            Response::builder().header("content-type", "application/json").body(full(json!({
                "id":format!("fixture-response-{name}"),"object":"response","model":"fixture-model","status":"completed","provider":name,
                "output":if compact { json!([{"type":"compaction","encrypted_content":WINDOW}]) } else { json!([]) },
                "usage":{"input_tokens":25,"output_tokens":3,"input_tokens_details":{"cached_tokens":20}}
            }).to_string())).unwrap()
        }
    }).await;
    (port, seen)
}

#[tokio::test]
async fn old_thread_stays_until_completed_compaction_new_thread_uses_latest_priority() {
    let (a, old) = endpoint("old", false).await;
    let (b, new) = endpoint("new", false).await;
    let (t, g) = fixture(vec![
        format!("http://127.0.0.1:{a}/v1"),
        format!("http://127.0.0.1:{b}/v1"),
    ])
    .await;
    let auth = std::fs::read(t.path().join("auth.json")).unwrap();
    automatic(&g, &t);
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
        "new"
    );
    assert_eq!(new.lock().unwrap().last().unwrap(), &bytes);
    assert_eq!(old.lock().unwrap().len(), 4);
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
    start(&g, &t).await;
    let ordinary = br#"{"model":"fixture-model","input":"fixture"}"#.to_vec();
    call(
        &g,
        "fixture-fallback",
        "/responses/compact",
        ordinary.clone(),
    )
    .await;
    promote_second(&g, &t);
    let bytes = input();
    assert_eq!(
        call(&g, "fixture-fallback", "/responses", bytes.clone()).await["provider"],
        "old"
    );
    assert_eq!(old.lock().unwrap().last().unwrap(), &bytes);
    assert_eq!(new.lock().unwrap().last().unwrap(), &bytes);
    assert_eq!(g.view().providers[0].health.failures, 0);
    assert_eq!(
        call(&g, "fixture-fallback", "/responses", ordinary).await["provider"],
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
    start(&g, &t).await;
    call(
        &g,
        "fixture-unconfirmed",
        "/responses",
        br#"{"model":"fixture-model"}"#.to_vec(),
    )
    .await;
    promote_second(&g, &t);
    assert_eq!(
        call(&g, "fixture-unconfirmed", "/responses", input()).await["provider"],
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
    start(&g, &t).await;
    let ordinary = br#"{"model":"fixture-model"}"#.to_vec();
    call(&g, "fixture-failed-compact", "/responses", ordinary.clone()).await;
    promote_second(&g, &t);
    let rejected = request(
        &g,
        "/responses/compact",
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
        call(&g, "fixture-failed-compact", "/responses", input()).await["provider"],
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
    for provider in g.view().providers {
        update(
            &g,
            &t,
            Edit::WebsocketProvider {
                id: provider.id,
                supports_websocket: true,
            },
        );
    }
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
    assert_eq!(ws_turn(&mut peer,json!({"type":"response.create","model":"fixture-model","input":[{"type":"compaction_trigger"}]}).to_string()).await["response"]["provider"],"old");
    let mut window: Value = serde_json::from_slice(&input()).unwrap();
    window["type"] = "response.create".into();
    let serialized = window.to_string();
    assert_eq!(
        ws_turn(&mut peer, serialized.clone()).await["response"]["provider"],
        "new"
    );
    assert_eq!(old.lock().unwrap().len(), 3);
    assert_eq!(new.lock().unwrap().len(), 1);
    assert_eq!(new.lock().unwrap()[0], serialized.as_bytes());
    drop(peer);
    g.stop().await.unwrap();
}

#[tokio::test]
async fn terminal_then_client_disconnect_collects_bounded_usage_tail_without_cancel_reclassification(
) {
    let port = server(|_| async {
        let chunks = async_stream::try_stream! {
            yield Frame::data(Bytes::from_static(b"event: response.completed\ndata: {\"response\":{\"id\":\"fixture-tail\",\"status\":\"completed\"}}\n\n"));
            tokio::time::sleep(Duration::from_millis(80)).await;
            yield Frame::data(Bytes::from_static(b"event: response.usage\ndata: {\"response_id\":\"fixture-tail\",\"usage\":{\"input_tokens\":25,\"output_tokens\":3,\"input_tokens_details\":{\"cached_tokens\":20}}}\n\n"));
        };
        Response::builder().header("content-type","text/event-stream").body(StreamBody::new(chunks).map_err(|e: std::io::Error|->connector::BoxError{e.into()}).boxed_unsync()).unwrap()
    }).await;
    let (t, g) = fixture(vec![format!("http://127.0.0.1:{port}")]).await;
    let usage = crate::usage::Service::new(t.path());
    g.set_usage(usage.clone());
    start(&g, &t).await;
    let response = request(
        &g,
        "/responses",
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
    drop(body);
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
    assert_eq!(attempt.tokens.input, Some(5));
    assert_eq!(attempt.tokens.cache_read, Some(20));
    assert_eq!(attempt.tokens.output, Some(3));
    assert_eq!(g.view().providers[0].active_requests, 0);
    g.stop().await.unwrap();
}
