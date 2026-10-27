//! Native WebSocket regressions for model detours on an owned thread.
//! All upstreams are loopback fixtures and use synthetic credentials.
use super::*;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

const SESSION: &str = "fixture-ws-model-detour";
const MAIN_WINDOW: &str = "fixture-main-window";
const SIDE_WINDOW: &str = "fixture-side-window";
// These tests assert routing and persistence, not wall-clock response speed.
const FIXTURE_WAIT: Duration = Duration::from_secs(30);

async fn native_endpoint(
    name: &'static str,
    main_window: Option<&'static str>,
    side_window: Option<&'static str>,
) -> (u16, Arc<Mutex<Vec<Vec<u8>>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let captured = seen.clone();
    let port = super::server(move |mut request| {
        let captured = captured.clone();
        async move {
            let (response, upgrade) = yawc::WebSocket::upgrade_with_options(
                &mut request,
                yawc::Options::default().with_balanced_compression(),
            )
            .unwrap();
            tokio::spawn(async move {
                let Ok(mut peer) = upgrade.await else {
                    return;
                };
                while let Some(frame) = peer.next().await {
                    if frame.opcode() == yawc::OpCode::Close {
                        break;
                    }
                    if frame.opcode().is_control() {
                        continue;
                    }
                    let payload = frame.payload().to_vec();
                    captured.lock().unwrap().push(payload.clone());
                    let request: Value = serde_json::from_slice(&payload).unwrap();
                    let model = request["model"].as_str().unwrap_or("fixture-main");
                    let response_id = match model {
                        "fixture-side" => "fixture-side-response",
                        _ => "fixture-main-response",
                    };
                    let window = match model {
                        "fixture-side" => side_window,
                        _ => main_window,
                    };
                    let output = window.map_or_else(
                        || json!([]),
                        |window| json!([{"type":"compaction","encrypted_content":window}]),
                    );
                    let created = json!({
                        "type":"response.created",
                        "response":{"id":response_id,"model":model}
                    })
                    .to_string();
                    let completed = json!({
                        "type":"response.completed",
                        "response":{
                            "id":response_id,
                            "model":model,
                            "status":"completed",
                            "provider":name,
                            "output":output,
                            "usage":{"input_tokens":2,"output_tokens":1}
                        }
                    })
                    .to_string();
                    if peer.send(yawc::Frame::text(created)).await.is_err()
                        || peer.send(yawc::Frame::text(completed)).await.is_err()
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

async fn connect(gateway: &Gateway, session: &str) -> yawc::TcpWebSocket {
    let (token, port) = {
        let state = gateway.0.inner.lock().unwrap();
        (state.store.local_token.clone(), state.store.settings.port)
    };
    yawc::WebSocket::connect(
        format!("ws://127.0.0.1:{port}/v1/responses")
            .parse()
            .unwrap(),
    )
    .with_options(yawc::Options::default().with_balanced_compression())
    .with_request(
        yawc::HttpRequest::builder()
            .header("authorization", format!("Bearer {token}"))
            .header("session_id", session),
    )
    .await
    .unwrap()
}

async fn complete_turn(peer: &mut yawc::TcpWebSocket, payload: &str) -> Value {
    peer.send(yawc::Frame::text(payload.to_owned()))
        .await
        .unwrap();
    let mut created = false;
    tokio::time::timeout(FIXTURE_WAIT, async {
        loop {
            let frame = peer.next().await.expect("gateway closed WebSocket");
            assert_ne!(frame.opcode(), yawc::OpCode::Close, "gateway rejected turn");
            if !matches!(frame.opcode(), yawc::OpCode::Text | yawc::OpCode::Binary) {
                continue;
            }
            let value: Value = serde_json::from_slice(frame.payload()).unwrap();
            match value["type"].as_str() {
                Some("response.created") => created = true,
                Some("response.completed") => {
                    assert!(created, "native fixture must preserve the created event");
                    break value;
                }
                Some("error" | "response.failed" | "response.incomplete") => {
                    panic!("native fixture turn failed: {value}");
                }
                _ => {}
            }
        }
    })
    .await
    .expect("native WebSocket turn timed out")
}

async fn reject_turn(peer: &mut yawc::TcpWebSocket, payload: &str) {
    peer.send(yawc::Frame::text(payload.to_owned()))
        .await
        .unwrap();
    tokio::time::timeout(FIXTURE_WAIT, async {
        loop {
            let Some(frame) = peer.next().await else {
                break;
            };
            if frame.opcode() == yawc::OpCode::Close {
                break;
            }
            if !matches!(frame.opcode(), yawc::OpCode::Text | yawc::OpCode::Binary) {
                continue;
            }
            let value: Value = serde_json::from_slice(frame.payload()).unwrap();
            if matches!(
                value["type"].as_str(),
                Some("error" | "response.failed" | "response.incomplete")
            ) || value.get("error").is_some()
            {
                break;
            }
            assert_ne!(
                value["type"].as_str(),
                Some("response.created"),
                "rejected turn must not start an upstream response"
            );
            assert_ne!(
                value["type"].as_str(),
                Some("response.completed"),
                "rejected turn must not complete through another provider"
            );
        }
    })
    .await
    .expect("gateway did not reject the turn")
}

async fn wait_until(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(FIXTURE_WAIT, async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("fixture state did not settle");
}

fn configure_models(gateway: &Gateway, dir: &tempfile::TempDir) -> (String, String) {
    let providers = gateway.view().providers;
    assert_eq!(providers.len(), 2);
    let owner = providers[0].id.clone();
    let side = providers[1].id.clone();
    super::update(
        gateway,
        dir,
        Edit::ModelsProvider {
            id: owner.clone(),
            allowed_models: Some(vec!["fixture-main".into()]),
        },
    );
    super::update(
        gateway,
        dir,
        Edit::ModelsProvider {
            id: side.clone(),
            allowed_models: Some(vec!["fixture-side".into()]),
        },
    );
    (owner, side)
}

fn owner_entry(dir: &tempfile::TempDir, session: &str) -> Value {
    let bytes = std::fs::read(dir.path().join("conversation-owners.json")).unwrap();
    let entries: Value = serde_json::from_slice(&bytes).unwrap();
    entries[session].clone()
}

fn reservation_entry(snapshot: &Value, session: &str, provider: &str) -> Value {
    snapshot
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["thread"] == session && entry["provider"] == provider)
        .cloned()
        .expect("thread reservation was not persisted")
}

async fn wait_for_reservation(dir: &tempfile::TempDir, session: &str, provider: &str) -> Value {
    tokio::time::timeout(FIXTURE_WAIT, async {
        loop {
            let snapshot = std::fs::read(dir.path().join("thread-reservations.json"))
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
            if let Some(snapshot) = snapshot {
                if snapshot.as_array().is_some_and(|entries| {
                    entries.iter().any(|entry| {
                        entry["thread"] == session
                            && entry["provider"] == provider
                            && entry["busy"] == false
                    })
                }) {
                    break snapshot;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("initial owner reservation was not persisted")
}

async fn wait_for_usage_record(
    usage: &crate::usage::Service,
    provider: &str,
    model: &str,
) -> crate::usage::model::Record {
    tokio::time::timeout(FIXTURE_WAIT, async {
        loop {
            let rows = usage
                .read(|store| Ok(store.logs(&crate::usage::model::Filter::default())?.rows))
                .unwrap();
            if let Some(record) = rows.into_iter().find(|record| {
                record.final_attempt().is_some_and(|attempt| {
                    attempt.provider.as_deref() == Some(provider)
                        && attempt.requested_model.as_deref() == Some(model)
                })
            }) {
                break record;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("model detour usage diagnostic was not persisted")
}

#[tokio::test]
async fn native_websocket_side_model_detour_preserves_owner_epoch_boundary_and_reservation() {
    let (port_a, seen_a) = native_endpoint("fixture-a", Some(MAIN_WINDOW), None).await;
    let (port_b, seen_b) = native_endpoint("fixture-b", None, Some(SIDE_WINDOW)).await;
    let (dir, gateway) = super::fixture(vec![
        format!("http://127.0.0.1:{port_a}/v1"),
        format!("http://127.0.0.1:{port_b}/v1"),
    ])
    .await;
    let (owner, side) = configure_models(&gateway, &dir);
    assert_eq!(gateway.view().settings.thread_idle_release_seconds, 300);
    let usage = crate::usage::Service::new(dir.path());
    gateway.set_usage(usage.clone());
    super::start(&gateway, &dir).await;

    let session_hash = crate::storage::digest(SESSION.as_bytes());
    let main_turn = r#"{ "type" : "response.create", "model" : "fixture-main", "input" : [{"role":"user","content":"fixture first"}], "future" : {"keep" : true} }"#;
    let side_turn = r#"{ "type" : "response.create", "model" : "fixture-side", "input" : [{"role":"user","content":"fixture first"},{"role":"assistant","content":"fixture answer"},{"role":"user","content":"fixture side"}], "future" : {"keep" : true} }"#;
    let side_followup = r#"{ "type" : "response.create", "model" : "fixture-side", "input" : [{"role":"user","content":"fixture first"},{"role":"assistant","content":"fixture answer"},{"role":"user","content":"fixture side"},{"role":"assistant","content":"fixture side answer"},{"role":"user","content":"fixture side followup"}], "future" : {"keep" : true} }"#;
    let mut owner_peer = connect(&gateway, SESSION).await;
    let first = complete_turn(&mut owner_peer, main_turn).await;
    assert_eq!(first["response"]["provider"], "fixture-a");
    assert_eq!(
        seen_a.lock().unwrap().as_slice()[0].as_slice(),
        main_turn.as_bytes()
    );
    wait_until(|| gateway.0.compaction.owner(&session_hash).as_deref() == Some(owner.as_str()))
        .await;
    let boundary = crate::storage::digest(MAIN_WINDOW.as_bytes());
    assert!(gateway
        .0
        .compaction
        .boundary_matches(&session_hash, Some(&boundary)));
    gateway.0.compaction.flush();
    let before_owner = owner_entry(&dir, &session_hash);
    assert_eq!(before_owner["provider"], owner);
    assert_eq!(before_owner["boundary"], boundary);

    wait_until(|| gateway.0.admission.reserved(&owner) == 1).await;
    assert_eq!(gateway.0.admission.reserved(&side), 0);
    let before_reservations = wait_for_reservation(&dir, &session_hash, &owner).await;
    let before_reservation = reservation_entry(&before_reservations, &session_hash, &owner);
    assert!(!before_reservation["idle_since"].is_null());

    let mut side_peer = connect(&gateway, SESSION).await;
    let detour = complete_turn(&mut side_peer, side_turn).await;
    assert_eq!(detour["response"]["provider"], "fixture-b");
    let side_followup_response = complete_turn(&mut side_peer, side_followup).await;
    assert_eq!(side_followup_response["response"]["provider"], "fixture-b");
    assert_eq!(seen_a.lock().unwrap().len(), 1);
    assert_eq!(seen_a.lock().unwrap()[0].as_slice(), main_turn.as_bytes());
    assert_eq!(seen_b.lock().unwrap().len(), 2);
    assert_eq!(seen_b.lock().unwrap()[0].as_slice(), side_turn.as_bytes());
    assert_eq!(
        seen_b.lock().unwrap()[1].as_slice(),
        side_followup.as_bytes()
    );
    wait_until(|| {
        gateway
            .view()
            .providers
            .iter()
            .all(|provider| provider.active_requests == 0)
    })
    .await;

    assert_eq!(
        gateway.0.compaction.owner(&session_hash).as_deref(),
        Some(owner.as_str())
    );
    assert!(gateway
        .0
        .compaction
        .boundary_matches(&session_hash, Some(&boundary)));
    gateway.0.compaction.flush();
    let after_owner = owner_entry(&dir, &session_hash);
    assert_eq!(after_owner["provider"], owner);
    assert_eq!(after_owner["epoch"], before_owner["epoch"]);
    assert_eq!(after_owner["boundary"], before_owner["boundary"]);
    assert_eq!(
        after_owner["boundary_sequence"],
        before_owner["boundary_sequence"]
    );

    assert_eq!(gateway.view().settings.thread_idle_release_seconds, 300);
    assert_eq!(gateway.0.admission.reserved(&owner), 1);
    assert_eq!(gateway.0.admission.reserved(&side), 0);
    let after_reservations = std::fs::read(dir.path().join("thread-reservations.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .unwrap();
    assert_eq!(after_reservations.as_array().unwrap().len(), 1);
    assert_eq!(
        reservation_entry(&after_reservations, &session_hash, &owner)["idle_since"],
        before_reservation["idle_since"]
    );

    let diagnostic = wait_for_usage_record(&usage, &side, "fixture-side").await;
    let attempt = diagnostic.final_attempt().unwrap();
    assert_eq!(
        serde_json::to_value(attempt.routing_decision).unwrap(),
        json!("model_detour_owner_preserved")
    );
    assert_eq!(
        attempt
            .routing_evidence
            .as_ref()
            .and_then(|evidence| evidence.restriction)
            .map(|restriction| serde_json::to_value(restriction).unwrap()),
        Some(json!("model_mismatch"))
    );

    let main_followup = r#"{ "type" : "response.create", "model" : "fixture-main", "input" : [{"role":"user","content":"fixture first"},{"role":"assistant","content":"fixture answer"},{"role":"user","content":"fixture main followup"}], "future" : {"keep" : true} }"#;
    let main_followup_response = complete_turn(&mut owner_peer, main_followup).await;
    assert_eq!(main_followup_response["response"]["provider"], "fixture-a");
    assert_eq!(seen_a.lock().unwrap().len(), 2);
    assert_eq!(
        seen_a.lock().unwrap()[1].as_slice(),
        main_followup.as_bytes()
    );

    side_peer
        .send(yawc::Frame::close(1000.into(), "fixture done"))
        .await
        .unwrap();
    drop(side_peer);
    owner_peer
        .send(yawc::Frame::close(1000.into(), "fixture done"))
        .await
        .unwrap();
    drop(owner_peer);
    gateway.stop().await.unwrap();
}

#[tokio::test]
async fn native_websocket_known_cursor_stays_on_its_owner_after_priority_changes() {
    let (port_a, seen_a) = native_endpoint("fixture-a", Some(MAIN_WINDOW), None).await;
    let (port_b, seen_b) = native_endpoint("fixture-b", None, Some(SIDE_WINDOW)).await;
    let (dir, gateway) = super::fixture(vec![
        format!("http://127.0.0.1:{port_a}/v1"),
        format!("http://127.0.0.1:{port_b}/v1"),
    ])
    .await;
    let (owner, side) = configure_models(&gateway, &dir);
    super::start(&gateway, &dir).await;
    let first_turn = r#"{"type":"response.create","model":"fixture-main","input":[{"role":"user","content":"fixture first"}]}"#;
    let mut first_peer = connect(&gateway, "fixture-known-cursor-thread").await;
    let first = complete_turn(&mut first_peer, first_turn).await;
    assert_eq!(first["response"]["provider"], "fixture-a");
    assert_eq!(seen_a.lock().unwrap().len(), 1);
    assert_eq!(seen_a.lock().unwrap()[0].as_slice(), first_turn.as_bytes());
    first_peer
        .send(yawc::Frame::close(1000.into(), "fixture done"))
        .await
        .unwrap();
    drop(first_peer);

    let mut ids = gateway
        .view()
        .providers
        .iter()
        .map(|provider| provider.id.clone())
        .collect::<Vec<_>>();
    ids.reverse();
    super::update(&gateway, &dir, Edit::Reorder { ids });
    let known_turn = format!(
        r#"{{"type":"response.create","model":"fixture-main","previous_response_id":"{}","input":[{{"role":"user","content":"fixture followup"}}]}}"#,
        first["response"]["id"].as_str().unwrap()
    );
    let mut known_peer = connect(&gateway, "fixture-known-cursor-thread").await;
    let known = complete_turn(&mut known_peer, &known_turn).await;
    assert_eq!(known["response"]["provider"], "fixture-a");
    assert_eq!(seen_a.lock().unwrap().len(), 2);
    assert_eq!(seen_a.lock().unwrap()[0].as_slice(), first_turn.as_bytes());
    assert_eq!(seen_a.lock().unwrap()[1].as_slice(), known_turn.as_bytes());
    assert!(seen_b.lock().unwrap().is_empty());
    assert_eq!(
        gateway
            .0
            .compaction
            .owner(&crate::storage::digest(b"fixture-known-cursor-thread"))
            .as_deref(),
        Some(owner.as_str())
    );
    assert_eq!(gateway.0.admission.reserved(&side), 0);

    known_peer
        .send(yawc::Frame::close(1000.into(), "fixture done"))
        .await
        .unwrap();
    drop(known_peer);
    gateway.stop().await.unwrap();
}

#[tokio::test]
async fn native_websocket_side_detour_followup_main_without_cursor_is_rejected_without_handoff() {
    let (port_a, seen_a) = native_endpoint("fixture-a", Some(MAIN_WINDOW), None).await;
    let (port_b, seen_b) = native_endpoint("fixture-b", None, Some(SIDE_WINDOW)).await;
    let (dir, gateway) = super::fixture(vec![
        format!("http://127.0.0.1:{port_a}/v1"),
        format!("http://127.0.0.1:{port_b}/v1"),
    ])
    .await;
    let (owner, side) = configure_models(&gateway, &dir);
    super::update(
        &gateway,
        &dir,
        Edit::ModelsProvider {
            id: side.clone(),
            allowed_models: None,
        },
    );
    super::start(&gateway, &dir).await;

    let session = "fixture-no-cursor-followup-thread";
    let session_hash = crate::storage::digest(session.as_bytes());
    let main_turn = r#"{"type":"response.create","model":"fixture-main","input":[{"role":"user","content":"fixture first"}]}"#;
    let side_turn = r#"{"type":"response.create","model":"fixture-side","input":[{"role":"user","content":"fixture first"},{"role":"assistant","content":"fixture main answer"},{"role":"user","content":"fixture side"}]}"#;
    let main_followup = r#"{"type":"response.create","model":"fixture-main","input":[{"role":"user","content":"fixture first"},{"role":"assistant","content":"fixture main answer"},{"role":"user","content":"fixture main followup"}]}"#;

    let mut owner_peer = connect(&gateway, session).await;
    let first = complete_turn(&mut owner_peer, main_turn).await;
    assert_eq!(first["response"]["provider"], "fixture-a");
    wait_until(|| gateway.0.compaction.owner(&session_hash).as_deref() == Some(owner.as_str()))
        .await;
    let boundary = crate::storage::digest(MAIN_WINDOW.as_bytes());
    gateway.0.compaction.flush();
    let before_owner = owner_entry(&dir, &session_hash);
    assert_eq!(before_owner["provider"], owner);
    assert_eq!(before_owner["boundary"], boundary);
    let before_reservations = wait_for_reservation(&dir, &session_hash, &owner).await;
    let before_reservation = reservation_entry(&before_reservations, &session_hash, &owner);
    assert!(!before_reservation["idle_since"].is_null());

    let mut side_peer = connect(&gateway, session).await;
    let side_response = complete_turn(&mut side_peer, side_turn).await;
    assert_eq!(side_response["response"]["provider"], "fixture-b");
    reject_turn(&mut side_peer, main_followup).await;
    wait_until(|| {
        gateway
            .view()
            .providers
            .iter()
            .all(|provider| provider.active_requests == 0)
    })
    .await;

    assert_eq!(seen_a.lock().unwrap().len(), 1);
    assert_eq!(seen_a.lock().unwrap()[0].as_slice(), main_turn.as_bytes());
    assert_eq!(seen_b.lock().unwrap().len(), 1);
    assert_eq!(seen_b.lock().unwrap()[0].as_slice(), side_turn.as_bytes());
    assert_eq!(
        gateway.0.compaction.owner(&session_hash).as_deref(),
        Some(owner.as_str())
    );
    assert!(gateway
        .0
        .compaction
        .boundary_matches(&session_hash, Some(&boundary)));
    gateway.0.compaction.flush();
    let after_owner = owner_entry(&dir, &session_hash);
    assert_eq!(after_owner["provider"], owner);
    assert_eq!(after_owner["epoch"], before_owner["epoch"]);
    assert_eq!(after_owner["boundary"], before_owner["boundary"]);
    assert_eq!(
        after_owner["boundary_sequence"],
        before_owner["boundary_sequence"]
    );
    assert_eq!(gateway.view().settings.thread_idle_release_seconds, 300);
    assert_eq!(gateway.0.admission.reserved(&owner), 1);
    assert_eq!(gateway.0.admission.reserved(&side), 0);
    let after_reservations: Value = serde_json::from_slice(
        &std::fs::read(dir.path().join("thread-reservations.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(after_reservations.as_array().unwrap().len(), 1);
    let after_reservation = reservation_entry(&after_reservations, &session_hash, &owner);
    assert_eq!(
        after_reservation["idle_since"],
        before_reservation["idle_since"]
    );

    // reject_turn already observed the server closing this connection.
    // Sending another close can fail with WSAECONNABORTED on Windows.
    drop(side_peer);
    owner_peer
        .send(yawc::Frame::close(1000.into(), "fixture done"))
        .await
        .unwrap();
    drop(owner_peer);
    gateway.stop().await.unwrap();
}

#[tokio::test]
async fn native_websocket_known_side_cursor_stays_pinned_without_rebinding_owner() {
    let (port_a, seen_a) = native_endpoint("fixture-a", Some(MAIN_WINDOW), None).await;
    let (port_b, seen_b) = native_endpoint("fixture-b", None, Some(SIDE_WINDOW)).await;
    let (dir, gateway) = super::fixture(vec![
        format!("http://127.0.0.1:{port_a}/v1"),
        format!("http://127.0.0.1:{port_b}/v1"),
    ])
    .await;
    let (owner, side) = configure_models(&gateway, &dir);
    super::update(
        &gateway,
        &dir,
        Edit::ModelsProvider {
            id: side.clone(),
            allowed_models: None,
        },
    );
    super::start(&gateway, &dir).await;

    let session = "fixture-known-side-cursor-thread";
    let session_hash = crate::storage::digest(session.as_bytes());
    let main_turn = r#"{"type":"response.create","model":"fixture-main","input":[{"role":"user","content":"fixture first"}]}"#;
    let side_turn = r#"{"type":"response.create","model":"fixture-side","input":[{"role":"user","content":"fixture first"},{"role":"assistant","content":"fixture main answer"},{"role":"user","content":"fixture side"}]}"#;

    let mut owner_peer = connect(&gateway, session).await;
    let first = complete_turn(&mut owner_peer, main_turn).await;
    assert_eq!(first["response"]["provider"], "fixture-a");
    wait_until(|| gateway.0.compaction.owner(&session_hash).as_deref() == Some(owner.as_str()))
        .await;
    let boundary = crate::storage::digest(MAIN_WINDOW.as_bytes());
    gateway.0.compaction.flush();
    let before_owner = owner_entry(&dir, &session_hash);
    assert_eq!(before_owner["provider"], owner);
    assert_eq!(before_owner["boundary"], boundary);
    let before_reservations = wait_for_reservation(&dir, &session_hash, &owner).await;
    let before_reservation = reservation_entry(&before_reservations, &session_hash, &owner);
    assert!(!before_reservation["idle_since"].is_null());

    let mut side_peer = connect(&gateway, session).await;
    let side_response = complete_turn(&mut side_peer, side_turn).await;
    assert_eq!(side_response["response"]["provider"], "fixture-b");
    let main_with_side_cursor = format!(
        r#"{{"type":"response.create","model":"fixture-main","previous_response_id":"{}","input":[{{"role":"user","content":"fixture main followup"}}]}}"#,
        side_response["response"]["id"].as_str().unwrap()
    );
    let cursor_response = complete_turn(&mut side_peer, &main_with_side_cursor).await;
    assert_eq!(cursor_response["response"]["provider"], "fixture-b");
    assert_eq!(seen_a.lock().unwrap().len(), 1);
    assert_eq!(seen_a.lock().unwrap()[0].as_slice(), main_turn.as_bytes());
    assert_eq!(seen_b.lock().unwrap().len(), 2);
    assert_eq!(seen_b.lock().unwrap()[0].as_slice(), side_turn.as_bytes());
    assert_eq!(
        seen_b.lock().unwrap()[1].as_slice(),
        main_with_side_cursor.as_bytes()
    );

    assert_eq!(
        gateway.0.compaction.owner(&session_hash).as_deref(),
        Some(owner.as_str())
    );
    assert!(gateway
        .0
        .compaction
        .boundary_matches(&session_hash, Some(&boundary)));
    gateway.0.compaction.flush();
    let after_owner = owner_entry(&dir, &session_hash);
    assert_eq!(after_owner["provider"], owner);
    assert_eq!(after_owner["epoch"], before_owner["epoch"]);
    assert_eq!(after_owner["boundary"], before_owner["boundary"]);
    assert_eq!(
        after_owner["boundary_sequence"],
        before_owner["boundary_sequence"]
    );
    assert_eq!(gateway.view().settings.thread_idle_release_seconds, 300);
    assert_eq!(gateway.0.admission.reserved(&owner), 1);
    assert_eq!(gateway.0.admission.reserved(&side), 0);
    let after_reservations: Value = serde_json::from_slice(
        &std::fs::read(dir.path().join("thread-reservations.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(after_reservations.as_array().unwrap().len(), 1);
    let after_reservation = reservation_entry(&after_reservations, &session_hash, &owner);
    assert_eq!(
        after_reservation["idle_since"],
        before_reservation["idle_since"]
    );

    side_peer
        .send(yawc::Frame::close(1000.into(), "fixture done"))
        .await
        .unwrap();
    drop(side_peer);
    owner_peer
        .send(yawc::Frame::close(1000.into(), "fixture done"))
        .await
        .unwrap();
    drop(owner_peer);
    gateway.stop().await.unwrap();
}

#[tokio::test]
async fn native_websocket_unknown_cursor_does_not_fall_through_to_another_provider() {
    let (port_a, seen_a) = native_endpoint("fixture-a", Some(MAIN_WINDOW), None).await;
    let (port_b, seen_b) = native_endpoint("fixture-b", None, Some(SIDE_WINDOW)).await;
    let (dir, gateway) = super::fixture(vec![
        format!("http://127.0.0.1:{port_a}/v1"),
        format!("http://127.0.0.1:{port_b}/v1"),
    ])
    .await;
    configure_models(&gateway, &dir);
    let mut ids = gateway
        .view()
        .providers
        .iter()
        .map(|provider| provider.id.clone())
        .collect::<Vec<_>>();
    ids.reverse();
    super::update(&gateway, &dir, Edit::Reorder { ids });
    super::start(&gateway, &dir).await;

    let mut peer = connect(&gateway, "fixture-unknown-cursor-thread").await;
    peer.send(yawc::Frame::text(
        r#"{"type":"response.create","model":"fixture-main","previous_response_id":"fixture-unknown-response","input":[{"role":"user","content":"fixture unknown cursor"}]}"#.to_owned(),
    ))
    .await
    .unwrap();
    let close = tokio::time::timeout(FIXTURE_WAIT, peer.next())
        .await
        .unwrap()
        .expect("unknown cursor socket closed without a frame");
    assert_eq!(close.opcode(), yawc::OpCode::Close);
    assert!(seen_a.lock().unwrap().is_empty());
    assert!(seen_b.lock().unwrap().is_empty());
    drop(peer);
    gateway.stop().await.unwrap();
}
