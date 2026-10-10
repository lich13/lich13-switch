use super::*;
use crate::events::{Action, Reason, Record};
use crate::gateway::circuit::{Circuit, CircuitState, Outcome};
use futures_util::{SinkExt, StreamExt};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

const CREATE: &str = r#"{"type":"response.create","model":"fixture-model","input":"followup"}"#;
const CREATED: &str =
    r#"{"type":"response.created","response":{"id":"fixture-response","model":"fixture-model"}}"#;
const COMPLETED: &str = r#"{"type":"response.completed","response":{"id":"fixture-response","model":"fixture-model","status":"completed"}}"#;

fn retry_settings(gateway: &Gateway, dir: &tempfile::TempDir) {
    update(
        gateway,
        dir,
        Edit::Mode {
            mode: "auto".into(),
        },
    );
    let mut settings = gateway.view().settings;
    settings.max_retries = 3;
    settings.websocket_retry_seconds = 1;
    settings.capacity_retry_seconds = 1;
    settings.rate_limit_seconds = 1;
    settings.first_byte_seconds = 3;
    settings.idle_seconds = 3;
    settings.total_seconds = 10;
    update(gateway, dir, Edit::Settings { settings });
    update(
        gateway,
        dir,
        Edit::RpmProvider {
            id: gateway.view().providers[0].id.clone(),
            max_rpm: 20,
        },
    );
}

async fn next_frame(ws: &mut yawc::TcpWebSocket) -> yawc::Frame {
    tokio::time::timeout(Duration::from_secs(8), ws.next())
        .await
        .expect("fixture websocket timed out")
        .expect("fixture websocket closed")
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

async fn responses_client(gateway: &Gateway, session: &str) -> yawc::TcpWebSocket {
    let (token, port) = {
        let state = gateway.0.inner.lock().unwrap();
        (state.store.local_token.clone(), state.store.settings.port)
    };
    yawc::WebSocket::connect(
        format!("ws://127.0.0.1:{port}/v1/responses?future=keep")
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

async fn native_upstream() -> (u16, Arc<AtomicUsize>, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let http_hits = Arc::new(AtomicUsize::new(0));
    let handshakes = Arc::new(AtomicUsize::new(0));
    let frames = Arc::new(AtomicUsize::new(0));
    let http_seen = http_hits.clone();
    let handshake_seen = handshakes.clone();
    let frame_seen = frames.clone();
    let port = server(move |mut request| {
        let http_seen = http_seen.clone();
        let handshake_seen = handshake_seen.clone();
        let frame_seen = frame_seen.clone();
        async move {
            if request.headers().get("upgrade").is_none() {
                http_seen.fetch_add(1, Ordering::SeqCst);
                let _ = request.into_body().collect().await;
                return Response::new(replay::full(r#"{"provider":"owner"}"#));
            }
            let attempt = handshake_seen.fetch_add(1, Ordering::SeqCst) + 1;
            if attempt == 1 {
                return json_response(500, r#"{"error":{"message":"fixture 500"}}"#, None);
            }
            if attempt == 2 {
                return json_response(507, r#"{"error":{"message":"fixture 507"}}"#, None);
            }
            let (response, upgrade) = yawc::WebSocket::upgrade_with_options(
                &mut request,
                yawc::Options::default().with_balanced_compression(),
            )
            .unwrap();
            tokio::spawn(async move {
                let Ok(mut socket) = upgrade.await else {
                    return;
                };
                while let Some(frame) = socket.next().await {
                    if frame.opcode() == yawc::OpCode::Close {
                        break;
                    }
                    if frame.opcode().is_control() {
                        continue;
                    }
                    frame_seen.fetch_add(1, Ordering::SeqCst);
                    let _ = socket.send(yawc::Frame::text(CREATED.to_owned())).await;
                    let _ = socket.send(yawc::Frame::text(COMPLETED.to_owned())).await;
                    break;
                }
            });
            response.map(|_| replay::empty())
        }
    })
    .await;
    (port, http_hits, handshakes, frames)
}

async fn bridge_upstream() -> (u16, Arc<AtomicUsize>) {
    let hits = Arc::new(AtomicUsize::new(0));
    let seen = hits.clone();
    let port = server(move |request| {
        let seen = seen.clone();
        async move {
            let attempt = seen.fetch_add(1, Ordering::SeqCst) + 1;
            let _ = request.into_body().collect().await;
            match attempt {
                1 => Response::new(replay::full(r#"{"provider":"owner"}"#)),
                2 => json_response(500, r#"{"error":{"message":"fixture 500"}}"#, None),
                3 => json_response(507, r#"{"error":{"message":"fixture 507"}}"#, None),
                _ => Response::builder()
                    .status(200)
                    .header("content-type", "text/event-stream")
                    .body(replay::full(format!(
                        "data: {CREATED}\n\ndata: {COMPLETED}\n\n"
                    )))
                    .unwrap(),
            }
        }
    })
    .await;
    (port, hits)
}

#[tokio::test]
async fn established_owner_native_websocket_retries_500_507_without_failover() {
    let (port, http_hits, handshakes, frames) = native_upstream().await;
    let backup_hits = Arc::new(AtomicUsize::new(0));
    let backup_seen = backup_hits.clone();
    let backup = server(move |_| {
        backup_seen.fetch_add(1, Ordering::SeqCst);
        async { json_response(200, r#"{"provider":"backup"}"#, None) }
    })
    .await;
    let (dir, gateway) = fixture(vec![
        format!("http://127.0.0.1:{port}/v1"),
        format!("http://127.0.0.1:{backup}/v1"),
    ])
    .await;
    update(
        &gateway,
        &dir,
        Edit::WebsocketProvider {
            id: gateway.view().providers[0].id.clone(),
            supports_websocket: true,
        },
    );
    retry_settings(&gateway, &dir);
    start(&gateway, &dir).await;

    let first = request(
        &gateway,
        "fixture-sticky-ws",
        br#"{"model":"fixture-model","input":[{"role":"user","content":"first"}]}"#.to_vec(),
    )
    .await;
    assert_eq!(first.status(), 200);
    first.into_body().collect().await.unwrap();

    let mut client = responses_client(&gateway, "fixture-sticky-ws").await;
    client
        .send(yawc::Frame::text(CREATE.to_owned()))
        .await
        .unwrap();
    assert_eq!(next_frame(&mut client).await.as_str(), CREATED);
    assert_eq!(next_frame(&mut client).await.as_str(), COMPLETED);
    settled(&gateway).await;
    assert_eq!(http_hits.load(Ordering::SeqCst), 1);
    assert_eq!(handshakes.load(Ordering::SeqCst), 3);
    assert_eq!(frames.load(Ordering::SeqCst), 1);
    assert_eq!(backup_hits.load(Ordering::SeqCst), 0);
    assert_eq!(gateway.view().providers[0].rpm_used, 4);
    assert_eq!(gateway.view().providers[1].rpm_used, 0);

    client
        .send(yawc::Frame::close(1000.into(), "fixture done"))
        .await
        .unwrap();
    drop(client);
    gateway.stop().await.unwrap();
}

#[tokio::test]
async fn established_owner_http_bridge_retries_500_507_before_first_event() {
    let (port, hits) = bridge_upstream().await;
    let backup_hits = Arc::new(AtomicUsize::new(0));
    let backup_seen = backup_hits.clone();
    let backup = server(move |_| {
        backup_seen.fetch_add(1, Ordering::SeqCst);
        async { json_response(200, r#"{"provider":"backup"}"#, None) }
    })
    .await;
    let (dir, gateway) = fixture(vec![
        format!("http://127.0.0.1:{port}/v1"),
        format!("http://127.0.0.1:{backup}/v1"),
    ])
    .await;
    update(
        &gateway,
        &dir,
        Edit::WebsocketProvider {
            id: gateway.view().providers[0].id.clone(),
            supports_websocket: false,
        },
    );
    retry_settings(&gateway, &dir);
    start(&gateway, &dir).await;

    let first = request(
        &gateway,
        "fixture-sticky-bridge",
        br#"{"model":"fixture-model","input":[{"role":"user","content":"first"}]}"#.to_vec(),
    )
    .await;
    assert_eq!(first.status(), 200);
    first.into_body().collect().await.unwrap();

    let mut client = responses_client(&gateway, "fixture-sticky-bridge").await;
    client
        .send(yawc::Frame::text(CREATE.to_owned()))
        .await
        .unwrap();
    assert_eq!(next_frame(&mut client).await.as_str(), CREATED);
    assert_eq!(next_frame(&mut client).await.as_str(), COMPLETED);
    settled(&gateway).await;
    assert_eq!(hits.load(Ordering::SeqCst), 4);
    assert_eq!(backup_hits.load(Ordering::SeqCst), 0);
    assert_eq!(gateway.view().providers[0].rpm_used, 4);
    assert_eq!(gateway.view().providers[1].rpm_used, 0);

    client
        .send(yawc::Frame::close(1000.into(), "fixture done"))
        .await
        .unwrap();
    drop(client);
    gateway.stop().await.unwrap();
}

fn finish_upstream_failure(circuit: &Circuit, cfg: &Settings, status: u16, attempt: u32) {
    let mut permit = circuit.acquire(false).expect("fixture circuit permit");
    permit.set_failure_event(Record::new(
        None,
        Some("fixture-provider"),
        Some("fixture-model"),
        Reason::UpstreamService,
        Action::Returned,
        Some(status),
        Some(attempt),
    ));
    permit.finish(Outcome::Failure(None), cfg);
}

#[test]
fn all_server_errors_share_six_failure_transient_threshold() {
    let cfg = Settings::default();
    for status in 500..=599 {
        let circuit = Circuit::default();
        for attempt in 1..=2 {
            finish_upstream_failure(&circuit, &cfg, status, attempt);
            let health = circuit.health();
            assert_eq!(health.state, CircuitState::Closed, "status {status}");
            assert!(health.available, "status {status}");
        }
    }

    let circuit = Circuit::default();
    for (index, status) in [500, 501, 502, 503, 507, 599].into_iter().enumerate() {
        finish_upstream_failure(&circuit, &cfg, status, (index + 1) as u32);
        let health = circuit.health();
        if index < 5 {
            assert_eq!(health.state, CircuitState::Closed, "status {status}");
            assert!(health.available, "status {status}");
        } else {
            assert_eq!(health.state, CircuitState::Open);
            assert!(!health.available);
            assert_eq!(health.failures, 6);
        }
    }
}
