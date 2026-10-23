//! v0.25.12 failover diagnostics regressions.
//!
//! These integration tests use only loopback fixtures and synthetic thread data.

use super::*;
use crate::events::{self, Action, Reason};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};
use std::time::Duration;

const MODEL: &str = "fixture-model";
const THREAD_ID: &str = "fixture-v02512-steady-thread";
const WINDOW: &str = "fixture-v02512-window";

fn digest(value: &str) -> String {
    crate::storage::digest(value.as_bytes())
}

fn seed_consumed_boundary(gateway: &Gateway, provider_id: &str) {
    let session = digest(THREAD_ID);
    let boundary = digest(WINDOW);
    let owner = gateway
        .0
        .compaction
        .prepare(session.clone(), None, false, &[provider_id.to_owned()])
        .expect("fixture owner lease");
    owner.complete(provider_id, None);
    owner.complete(provider_id, Some(&boundary));
    gateway.0.compaction.flush();

    let consumer = gateway
        .0
        .compaction
        .prepare(session, Some(&boundary), false, &[provider_id.to_owned()])
        .expect("fixture boundary consumer lease");
    consumer.output_started(provider_id);
    gateway.0.compaction.flush();
    drop(consumer);
    gateway.0.compaction.flush();
}

async fn settle_gateway(gateway: &Gateway) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let view = gateway.view();
            if view.active_connections == 0
                && view.waiting_requests == 0
                && view
                    .providers
                    .iter()
                    .all(|provider| provider.active_requests == 0)
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("fixture request did not settle");
}

async fn settle_events(
    gateway: &Gateway,
    service: &events::Service,
    receiver: &mut tokio::sync::broadcast::Receiver<events::Change>,
) -> Vec<events::Record> {
    settle_gateway(gateway).await;
    gateway.0.compaction.flush();
    let marker = events::Record::new(
        Some(crate::gateway::ClientId::Codex),
        Some("fixture-v02512-event-barrier"),
        Some("fixture-v02512-barrier-model"),
        Reason::ProtocolError,
        Action::Returned,
        Some(599),
        None,
    );
    let marker_id = marker.id.clone();
    service.emit(marker);
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let change = receiver.recv().await.expect("diagnostic writer closed");
            assert!(change.error.is_none(), "diagnostic persistence failed");
            assert!(!change.notify, "synthetic fixture events must not notify");
            if change
                .record
                .as_ref()
                .is_some_and(|record| record.id == marker_id)
            {
                return;
            }
        }
    })
    .await
    .expect("diagnostic event queue did not settle");

    let page = service.query(events::Filter::default());
    assert!(page.error.is_none(), "diagnostic query failed");
    page.items
        .into_iter()
        .map(|item| item.record)
        .filter(|record| record.id != marker_id)
        .collect()
}

async fn closed_loopback_url() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    format!("http://127.0.0.1:{port}/v1")
}

fn configure_auto(gateway: &Gateway, dir: &tempfile::TempDir, max_retries: usize) {
    update(
        gateway,
        dir,
        Edit::Mode {
            mode: "auto".into(),
        },
    );
    let mut settings = gateway.view().settings;
    settings.max_retries = max_retries;
    settings.queue_seconds = 1;
    settings.websocket_retry_seconds = 1;
    update(gateway, dir, Edit::Settings { settings });
}

fn diagnostic_sink(
    gateway: &Gateway,
    dir: &tempfile::TempDir,
) -> (
    events::Service,
    tokio::sync::broadcast::Receiver<events::Change>,
) {
    let service = events::Service::new(dir.path());
    let receiver = service.subscribe();
    gateway.set_diagnostics(service.clone());
    (service, receiver)
}

async fn post_responses(
    gateway: &Gateway,
    thread_id: Option<&str>,
    input: Value,
) -> Response<Incoming> {
    let mut headers = vec![("content-type", "application/json")];
    if let Some(thread_id) = thread_id {
        headers.push(("thread-id", thread_id));
    }
    request(
        gateway,
        "/v1/responses",
        serde_json::to_vec(&json!({"model": MODEL, "input": input})).unwrap(),
        headers,
    )
    .await
}

async fn assert_network_error_response(response: Response<Incoming>) {
    assert_eq!(response.status(), hyper::StatusCode::BAD_GATEWAY);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let wire_error: Value = serde_json::from_slice(&body)
        .expect("gateway error response must follow the JSON error() contract");
    assert_eq!(
        wire_error["error"]["code"].as_str(),
        Some(crate::errors::code::NETWORK),
        "unexpected synthetic 502 body: {}",
        String::from_utf8_lossy(&body)
    );
}

fn assert_no_false_next_or_aggregate(records: &[events::Record]) {
    assert_eq!(
        records
            .iter()
            .filter(|record| record.action == Action::TryingNext)
            .count(),
        0,
        "no eligible next provider must not be claimed"
    );
    assert_eq!(
        records
            .iter()
            .filter(|record| record.reason == Reason::FailoverExhausted)
            .count(),
        0,
        "independent upstream failures are not a failover summary"
    );
}

fn assert_provider_was_not_attempted_or_routed(records: &[events::Record], provider_id: &str) {
    assert!(!records.iter().any(|record| {
        record.provider_id.as_deref() == Some(provider_id)
            && matches!(record.reason, Reason::Network | Reason::Failover)
    }));
}

#[derive(Default)]
struct NativeWsSeen {
    handshakes: AtomicUsize,
    requests: Mutex<Vec<Value>>,
}

async fn native_response_server() -> (u16, Arc<NativeWsSeen>) {
    let seen = Arc::new(NativeWsSeen::default());
    let shared = seen.clone();
    let port = server(move |mut request| {
        let seen = shared.clone();
        async move {
            seen.handshakes.fetch_add(1, Ordering::SeqCst);
            let (response, upgrade) = yawc::WebSocket::upgrade_with_options(
                &mut request,
                yawc::Options::default().with_balanced_compression(),
            )
            .unwrap();
            tokio::spawn(async move {
                let Ok(mut upstream) = upgrade.await else {
                    return;
                };
                while let Some(frame) = upstream.next().await {
                    if frame.opcode() == yawc::OpCode::Close {
                        break;
                    }
                    if frame.opcode().is_control() {
                        continue;
                    }
                    let Ok(value) = serde_json::from_slice::<Value>(frame.payload()) else {
                        continue;
                    };
                    let turn = {
                        let mut requests = seen.requests.lock().unwrap();
                        requests.push(value);
                        requests.len()
                    };
                    let id = format!("fixture-v02512-ws-{turn}");
                    let created = json!({
                        "type": "response.created",
                        "response": {"id": id, "model": MODEL}
                    })
                    .to_string();
                    let completed = json!({
                        "type": "response.completed",
                        "response": {"id": id, "model": MODEL, "status": "completed"}
                    })
                    .to_string();
                    if upstream.send(yawc::Frame::text(created)).await.is_err()
                        || upstream.send(yawc::Frame::text(completed)).await.is_err()
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

async fn native_server_rejecting_second_handshake() -> (u16, Arc<NativeWsSeen>) {
    let seen = Arc::new(NativeWsSeen::default());
    let shared = seen.clone();
    let port = server(move |mut request| {
        let seen = shared.clone();
        async move {
            let handshake = seen.handshakes.fetch_add(1, Ordering::SeqCst) + 1;
            if handshake > 1 {
                return Response::builder()
                    .status(hyper::StatusCode::BAD_GATEWAY)
                    .body(full("fixture later-round handshake failure"))
                    .unwrap();
            }
            let (response, upgrade) = yawc::WebSocket::upgrade_with_options(
                &mut request,
                yawc::Options::default().with_balanced_compression(),
            )
            .unwrap();
            tokio::spawn(async move {
                let Ok(mut upstream) = upgrade.await else {
                    return;
                };
                while let Some(frame) = upstream.next().await {
                    if frame.opcode() == yawc::OpCode::Close {
                        break;
                    }
                    if frame.opcode().is_control() {
                        continue;
                    }
                    if let Ok(value) = serde_json::from_slice::<Value>(frame.payload()) {
                        seen.requests.lock().unwrap().push(value);
                    }
                    let id = "fixture-v02512-ws-first";
                    let created = json!({
                        "type": "response.created",
                        "response": {"id": id, "model": MODEL}
                    })
                    .to_string();
                    let completed = json!({
                        "type": "response.completed",
                        "response": {"id": id, "model": MODEL, "status": "completed"}
                    })
                    .to_string();
                    let _ = upstream.send(yawc::Frame::text(created)).await;
                    let _ = upstream.send(yawc::Frame::text(completed)).await;
                    let _ = upstream
                        .send(yawc::Frame::close(1000.into(), "fixture idle close"))
                        .await;
                    break;
                }
            });
            response.map(|_| replay::empty())
        }
    })
    .await;
    (port, seen)
}

async fn next_ws_frame(socket: &mut yawc::TcpWebSocket, phase: &'static str) -> yawc::Frame {
    tokio::time::timeout(Duration::from_secs(10), socket.next())
        .await
        .unwrap_or_else(|_| panic!("{phase}: websocket fixture timed out"))
        .expect("websocket closed without a frame")
}

fn response_create(input: &str) -> String {
    json!({"type":"response.create", "model":MODEL, "input":input}).to_string()
}

fn network_records(records: &[events::Record]) -> Vec<&events::Record> {
    let mut network: Vec<_> = records
        .iter()
        .filter(|record| record.reason == Reason::Network)
        .collect();
    network.sort_by_key(|record| record.attempt.unwrap_or(u32::MAX));
    network
}

fn assert_connect_failure(
    record: &events::Record,
    provider_id: &str,
    action: Action,
    phase: events::Phase,
) {
    assert_eq!(record.reason, Reason::Network);
    assert_eq!(record.provider_id.as_deref(), Some(provider_id));
    assert_eq!(record.action, action);
    assert_eq!(record.status, None);
    assert_eq!(record.details.phase, Some(phase));
    assert_eq!(
        record.details.local_code.as_deref(),
        Some(crate::errors::code::CONNECTION_FAILED)
    );
}

async fn next_ws_text(socket: &mut yawc::TcpWebSocket, phase: &'static str) -> Value {
    let frame = next_ws_frame(socket, phase).await;
    assert_eq!(
        frame.opcode(),
        yawc::OpCode::Text,
        "{phase}: expected text frame"
    );
    serde_json::from_slice(frame.payload()).unwrap_or_else(|error| {
        panic!("{phase}: native response fixture returned invalid JSON: {error}")
    })
}

async fn next_ws_close(socket: &mut yawc::TcpWebSocket, phase: &'static str) -> yawc::Frame {
    loop {
        let frame = next_ws_frame(socket, phase).await;
        if frame.opcode() == yawc::OpCode::Close {
            return frame;
        }
    }
}

#[tokio::test]
async fn http_single_queued_provider_connection_failure_has_no_false_next_or_steady_routing() {
    let (dir, gateway) = fixture(vec![closed_loopback_url().await]).await;
    configure_auto(&gateway, &dir, 0);
    let provider_id = gateway.view().providers[0].id.clone();
    seed_consumed_boundary(&gateway, &provider_id);

    let (diagnostics, mut notifications) = diagnostic_sink(&gateway, &dir);
    start(&gateway, &dir).await;

    let response = request(
        &gateway,
        "/v1/responses",
        serde_json::to_vec(&json!({
            "model": MODEL,
            "input": [{"type": "compaction", "encrypted_content": WINDOW}]
        }))
        .unwrap(),
        vec![
            ("content-type", "application/json"),
            ("thread-id", THREAD_ID),
        ],
    )
    .await;
    assert_eq!(response.status(), 502);
    let response_body = response.into_body().collect().await.unwrap().to_bytes();
    let wire_error: serde_json::Value = serde_json::from_slice(&response_body)
        .expect("gateway error response must follow the JSON error() contract");
    assert_eq!(
        wire_error["error"]["code"].as_str(),
        Some(crate::errors::code::NETWORK),
        "unexpected synthetic 502 body: {}",
        String::from_utf8_lossy(&response_body)
    );

    let records = settle_events(&gateway, &diagnostics, &mut notifications).await;
    let network: Vec<_> = records
        .iter()
        .filter(|record| record.reason == Reason::Network)
        .collect();
    assert_eq!(
        network.len(),
        1,
        "the connect failure must be recorded once"
    );
    assert_eq!(
        network[0].provider_id.as_deref(),
        Some(provider_id.as_str())
    );
    assert_eq!(network[0].action, Action::Returned);
    assert_eq!(network[0].status, None);
    assert_eq!(network[0].details.phase, Some(events::Phase::Connect));
    assert_eq!(
        network[0].details.local_code.as_deref(),
        Some(crate::errors::code::CONNECTION_FAILED)
    );
    assert_eq!(
        records
            .iter()
            .filter(|record| record.reason == Reason::FailoverExhausted)
            .count(),
        0,
        "one queued provider has no failover aggregate"
    );
    assert_eq!(
        records
            .iter()
            .filter(|record| record.action == Action::TryingNext)
            .count(),
        0,
        "an unavailable next provider must not be claimed"
    );
    assert_eq!(
        records
            .iter()
            .filter(|record| record.reason == Reason::ThreadRouting)
            .count(),
        0,
        "a consumed boundary with no ownership action stays quiet"
    );

    gateway.stop().await.unwrap();
}

#[tokio::test]
async fn http_manual_single_provider_connection_failure_returns_one_real_error_without_summary() {
    let (dir, gateway) = fixture(vec![closed_loopback_url().await]).await;
    let provider_id = gateway.view().providers[0].id.clone();
    update(
        &gateway,
        &dir,
        Edit::Mode {
            mode: "manual".into(),
        },
    );
    update(
        &gateway,
        &dir,
        Edit::Select {
            id: provider_id.clone(),
        },
    );
    let mut settings = gateway.view().settings;
    settings.max_retries = 0;
    update(&gateway, &dir, Edit::Settings { settings });
    let (diagnostics, mut notifications) = diagnostic_sink(&gateway, &dir);
    start(&gateway, &dir).await;

    let response = post_responses(&gateway, None, json!("fixture input")).await;
    assert_network_error_response(response).await;

    let records = settle_events(&gateway, &diagnostics, &mut notifications).await;
    let network = network_records(&records);
    assert_eq!(
        network.len(),
        1,
        "manual mode should record the request once"
    );
    assert_connect_failure(
        network[0],
        &provider_id,
        Action::Returned,
        events::Phase::Connect,
    );
    assert_eq!(network[0].attempt, Some(1));
    assert_no_false_next_or_aggregate(&records);

    gateway.stop().await.unwrap();
}

#[tokio::test]
async fn http_unqueued_backup_is_not_reported_as_a_next_provider() {
    let (dir, gateway) = fixture(vec![
        closed_loopback_url().await,
        closed_loopback_url().await,
    ])
    .await;
    configure_auto(&gateway, &dir, 1);
    let providers = gateway.view().providers;
    let primary_id = providers[0].id.clone();
    update(
        &gateway,
        &dir,
        Edit::QueueProvider {
            id: providers[1].id.clone(),
            queued: false,
        },
    );
    let (diagnostics, mut notifications) = diagnostic_sink(&gateway, &dir);
    start(&gateway, &dir).await;

    let response = post_responses(&gateway, None, json!("fixture input")).await;
    assert_network_error_response(response).await;

    let records = settle_events(&gateway, &diagnostics, &mut notifications).await;
    let network = network_records(&records);
    assert_eq!(network.len(), 1);
    assert_connect_failure(
        network[0],
        &primary_id,
        Action::Returned,
        events::Phase::Connect,
    );
    assert_provider_was_not_attempted_or_routed(&records, &providers[1].id);
    assert_no_false_next_or_aggregate(&records);

    gateway.stop().await.unwrap();
}

#[tokio::test]
async fn http_model_excluded_backup_is_not_reported_as_a_next_provider() {
    let (dir, gateway) = fixture(vec![
        closed_loopback_url().await,
        closed_loopback_url().await,
    ])
    .await;
    configure_auto(&gateway, &dir, 1);
    let providers = gateway.view().providers;
    let primary_id = providers[0].id.clone();
    update(
        &gateway,
        &dir,
        Edit::ModelsProvider {
            id: providers[1].id.clone(),
            allowed_models: Some(vec!["fixture-other-model".into()]),
        },
    );
    let (diagnostics, mut notifications) = diagnostic_sink(&gateway, &dir);
    start(&gateway, &dir).await;

    let response = post_responses(&gateway, None, json!("fixture input")).await;
    assert_network_error_response(response).await;

    let records = settle_events(&gateway, &diagnostics, &mut notifications).await;
    let network = network_records(&records);
    assert_eq!(network.len(), 1);
    assert_connect_failure(
        network[0],
        &primary_id,
        Action::Returned,
        events::Phase::Connect,
    );
    assert_provider_was_not_attempted_or_routed(&records, &providers[1].id);
    assert_no_false_next_or_aggregate(&records);

    gateway.stop().await.unwrap();
}

#[tokio::test]
async fn http_new_thread_policy_excluded_backup_is_not_reported_as_a_next_provider() {
    let (dir, gateway) = fixture(vec![
        closed_loopback_url().await,
        closed_loopback_url().await,
    ])
    .await;
    configure_auto(&gateway, &dir, 1);
    let providers = gateway.view().providers;
    let primary_id = providers[0].id.clone();
    update(
        &gateway,
        &dir,
        Edit::TakeoverProvider {
            id: primary_id.clone(),
            handoff_after_compaction: None,
            take_new_threads: Some(true),
        },
    );
    update(
        &gateway,
        &dir,
        Edit::TakeoverProvider {
            id: providers[1].id.clone(),
            handoff_after_compaction: Some(true),
            take_new_threads: Some(false),
        },
    );
    let (diagnostics, mut notifications) = diagnostic_sink(&gateway, &dir);
    start(&gateway, &dir).await;

    let response = post_responses(
        &gateway,
        Some("fixture-v02512-new-thread-policy"),
        json!([{"role":"user", "content":"fixture input"}]),
    )
    .await;
    assert_network_error_response(response).await;

    let records = settle_events(&gateway, &diagnostics, &mut notifications).await;
    let network = network_records(&records);
    assert_eq!(network.len(), 1);
    assert_connect_failure(
        network[0],
        &primary_id,
        Action::Returned,
        events::Phase::Connect,
    );
    assert_provider_was_not_attempted_or_routed(&records, &providers[1].id);
    assert_no_false_next_or_aggregate(&records);

    gateway.stop().await.unwrap();
}

#[tokio::test]
async fn http_second_provider_success_keeps_real_trying_next_and_failover_records() {
    let primary_url = closed_loopback_url().await;
    let backup_hits = Arc::new(AtomicUsize::new(0));
    let observed = backup_hits.clone();
    let backup_port = server(move |_| {
        observed.fetch_add(1, Ordering::SeqCst);
        async { Response::new(full("fixture backup success")) }
    })
    .await;
    let (dir, gateway) = fixture(vec![
        primary_url,
        format!("http://127.0.0.1:{backup_port}/v1"),
    ])
    .await;
    configure_auto(&gateway, &dir, 1);
    let providers = gateway.view().providers;
    let primary_id = providers[0].id.clone();
    let backup_id = providers[1].id.clone();
    let (diagnostics, mut notifications) = diagnostic_sink(&gateway, &dir);
    start(&gateway, &dir).await;

    let response = post_responses(&gateway, None, json!("fixture input")).await;
    assert_eq!(response.status(), hyper::StatusCode::OK);
    assert_eq!(
        response.into_body().collect().await.unwrap().to_bytes(),
        "fixture backup success"
    );
    assert_eq!(backup_hits.load(Ordering::SeqCst), 1);

    let records = settle_events(&gateway, &diagnostics, &mut notifications).await;
    let network = network_records(&records);
    assert_eq!(network.len(), 1);
    assert_connect_failure(
        network[0],
        &primary_id,
        Action::TryingNext,
        events::Phase::Connect,
    );
    let failovers: Vec<_> = records
        .iter()
        .filter(|record| record.reason == Reason::Failover)
        .collect();
    assert_eq!(failovers.len(), 1, "successful fallback stays visible");
    assert_eq!(
        failovers[0].provider_id.as_deref(),
        Some(backup_id.as_str())
    );
    assert_eq!(failovers[0].action, Action::Routed);
    assert_eq!(
        records
            .iter()
            .filter(|record| record.reason == Reason::FailoverExhausted)
            .count(),
        0
    );

    gateway.stop().await.unwrap();
}

#[tokio::test]
async fn http_two_connection_failures_are_recorded_without_an_aggregate() {
    let (dir, gateway) = fixture(vec![
        closed_loopback_url().await,
        closed_loopback_url().await,
    ])
    .await;
    configure_auto(&gateway, &dir, 1);
    let providers = gateway.view().providers;
    let first_id = providers[0].id.clone();
    let second_id = providers[1].id.clone();
    let (diagnostics, mut notifications) = diagnostic_sink(&gateway, &dir);
    start(&gateway, &dir).await;

    let response = post_responses(&gateway, None, json!("fixture input")).await;
    assert_network_error_response(response).await;

    let records = settle_events(&gateway, &diagnostics, &mut notifications).await;
    let network = network_records(&records);
    assert_eq!(
        network.len(),
        2,
        "each attempted upstream keeps its real error"
    );
    assert_connect_failure(
        network[0],
        &first_id,
        Action::TryingNext,
        events::Phase::Connect,
    );
    assert_connect_failure(
        network[1],
        &second_id,
        Action::Returned,
        events::Phase::Connect,
    );
    assert_eq!(
        records
            .iter()
            .filter(|record| record.reason == Reason::FailoverExhausted)
            .count(),
        0,
        "independent connection failures must not gain an aggregate"
    );

    gateway.stop().await.unwrap();
}

#[tokio::test]
async fn stale_backup_route_after_credential_change_is_not_reported_as_next() {
    let (dir, gateway) = fixture(vec![
        closed_loopback_url().await,
        closed_loopback_url().await,
    ])
    .await;
    configure_auto(&gateway, &dir, 1);
    let providers = gateway.view().providers;
    let primary_id = providers[0].id.clone();
    let backup_id = providers[1].id.clone();
    update(
        &gateway,
        &dir,
        Edit::TakeoverProvider {
            id: backup_id.clone(),
            handoff_after_compaction: Some(true),
            take_new_threads: Some(false),
        },
    );
    start(&gateway, &dir).await;

    let stale_backup = gateway.route(&backup_id).expect("backup route snapshot");
    let requirement = super::routing::Requirement::model(Some(MODEL));
    let budget = super::admission::Budget::new(1);
    assert_eq!(
        super::routing::fallback_action(
            &gateway,
            &primary_id,
            vec![stale_backup.clone()],
            &requirement,
            "/v1/responses",
            None,
            true,
            &budget,
            true,
        ),
        Action::Returned,
        "unidentified requests must retain the captured backup policy"
    );

    update(
        &gateway,
        &dir,
        Edit::TakeoverProvider {
            id: backup_id.clone(),
            handoff_after_compaction: Some(false),
            take_new_threads: Some(false),
        },
    );
    let current_backup = gateway
        .route(&backup_id)
        .expect("policy-updated backup route");
    assert_eq!(
        stale_backup.provider.version, current_backup.provider.version,
        "takeover policy edits do not replace the route generation"
    );
    assert_eq!(
        super::routing::fallback_action(
            &gateway,
            &primary_id,
            vec![stale_backup.clone()],
            &requirement,
            "/v1/responses",
            None,
            true,
            &budget,
            true,
        ),
        Action::Returned,
        "current permissive policy must not widen an already captured request"
    );

    update(
        &gateway,
        &dir,
        Edit::SaveProvider {
            quota_access_token: None,
            id: Some(backup_id.clone()),
            base_url: providers[1].base_url.clone(),
            token: "fixture-rotated-token".into(),
            name: None,
        },
    );
    let current_backup = gateway.route(&backup_id).expect("updated backup route");
    assert_ne!(
        stale_backup.provider.version, current_backup.provider.version,
        "credential edits invalidate retained route snapshots"
    );
    assert_eq!(
        super::routing::fallback_action(
            &gateway,
            &primary_id,
            vec![stale_backup],
            &requirement,
            "/v1/responses",
            None,
            true,
            &budget,
            true,
        ),
        Action::Returned,
        "an old route snapshot must not claim a configured backup"
    );

    gateway.stop().await.unwrap();
}

#[tokio::test]
async fn native_ws_single_provider_handshake_connection_failure_has_no_false_next_or_summary() {
    let (dir, gateway) = fixture(vec![closed_loopback_url().await]).await;
    configure_auto(&gateway, &dir, 0);
    let provider_id = gateway.view().providers[0].id.clone();
    let (diagnostics, mut notifications) = diagnostic_sink(&gateway, &dir);
    start(&gateway, &dir).await;

    let mut client = concurrency::responses_client(&gateway).await;
    client
        .send(yawc::Frame::text(response_create("fixture input")))
        .await
        .unwrap();
    let closed = next_ws_close(&mut client, "single provider handshake failure").await;
    assert_eq!(closed.close_code().map(u16::from), Some(1013));
    drop(client);

    let records = settle_events(&gateway, &diagnostics, &mut notifications).await;
    let network = network_records(&records);
    assert_eq!(network.len(), 1);
    assert_connect_failure(
        network[0],
        &provider_id,
        Action::Returned,
        events::Phase::WsHandshake,
    );
    assert_no_false_next_or_aggregate(&records);

    gateway.stop().await.unwrap();
}

#[tokio::test]
async fn native_ws_second_provider_success_preserves_failover_and_later_turns() {
    let primary_url = closed_loopback_url().await;
    let (backup_port, backup_seen) = native_response_server().await;
    let (dir, gateway) = fixture(vec![
        primary_url,
        format!("http://127.0.0.1:{backup_port}/v1"),
    ])
    .await;
    configure_auto(&gateway, &dir, 1);
    let providers = gateway.view().providers;
    let primary_id = providers[0].id.clone();
    let backup_id = providers[1].id.clone();
    let (diagnostics, mut notifications) = diagnostic_sink(&gateway, &dir);
    start(&gateway, &dir).await;

    let mut client = concurrency::responses_client(&gateway).await;
    for turn in ["first fixture turn", "later fixture turn"] {
        client
            .send(yawc::Frame::text(response_create(turn)))
            .await
            .unwrap();
        let created = next_ws_text(&mut client, "native failover response.created").await;
        assert_eq!(created["type"], "response.created");
        let completed = next_ws_text(&mut client, "native failover response.completed").await;
        assert_eq!(completed["type"], "response.completed");
    }
    assert_eq!(backup_seen.handshakes.load(Ordering::SeqCst), 1);
    assert_eq!(backup_seen.requests.lock().unwrap().len(), 2);
    client
        .send(yawc::Frame::close(1000.into(), "fixture done"))
        .await
        .unwrap();
    drop(client);

    let records = settle_events(&gateway, &diagnostics, &mut notifications).await;
    let network = network_records(&records);
    assert_eq!(network.len(), 1);
    assert_connect_failure(
        network[0],
        &primary_id,
        Action::TryingNext,
        events::Phase::WsHandshake,
    );
    let failovers: Vec<_> = records
        .iter()
        .filter(|record| record.reason == Reason::Failover)
        .collect();
    assert_eq!(failovers.len(), 1);
    assert_eq!(
        failovers[0].provider_id.as_deref(),
        Some(backup_id.as_str())
    );
    assert_eq!(failovers[0].action, Action::Routed);
    assert_eq!(
        records
            .iter()
            .filter(|record| record.reason == Reason::FailoverExhausted)
            .count(),
        0
    );

    gateway.stop().await.unwrap();
}

#[tokio::test]
async fn native_ws_later_round_handshake_failure_keeps_its_real_upstream_error() {
    let (server_port, seen) = native_server_rejecting_second_handshake().await;
    let (dir, gateway) = fixture(vec![format!("http://127.0.0.1:{server_port}/v1")]).await;
    configure_auto(&gateway, &dir, 0);
    let provider_id = gateway.view().providers[0].id.clone();
    let (diagnostics, mut notifications) = diagnostic_sink(&gateway, &dir);
    start(&gateway, &dir).await;

    let mut client = concurrency::responses_client(&gateway).await;
    client
        .send(yawc::Frame::text(response_create("first fixture turn")))
        .await
        .unwrap();
    assert_eq!(
        next_ws_text(&mut client, "first native round response.created").await["type"],
        "response.created"
    );
    assert_eq!(
        next_ws_text(&mut client, "first native round response.completed").await["type"],
        "response.completed"
    );

    client
        .send(yawc::Frame::text(response_create("later fixture turn")))
        .await
        .unwrap();
    let closed = next_ws_close(&mut client, "later native round handshake failure").await;
    assert_eq!(closed.close_code().map(u16::from), Some(1013));
    assert_eq!(seen.handshakes.load(Ordering::SeqCst), 2);
    drop(client);

    let records = settle_events(&gateway, &diagnostics, &mut notifications).await;
    let upstream: Vec<_> = records
        .iter()
        .filter(|record| record.reason == Reason::UpstreamService)
        .collect();
    assert_eq!(
        upstream.len(),
        1,
        "the later handshake rejection stays visible"
    );
    assert_eq!(
        upstream[0].provider_id.as_deref(),
        Some(provider_id.as_str())
    );
    assert_eq!(upstream[0].status, Some(502));
    assert_eq!(upstream[0].action, Action::Returned);
    assert_eq!(upstream[0].details.phase, Some(events::Phase::WsHandshake));
    assert_eq!(
        records
            .iter()
            .filter(|record| record.reason == Reason::FailoverExhausted)
            .count(),
        0
    );

    gateway.stop().await.unwrap();
}
