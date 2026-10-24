//! v0.25.9 native WebSocket owner handoff regression.
//!
//! The upstreams and client use loopback sockets and synthetic credentials only.

use super::*;
use crate::gateway::compaction::Registry;
use futures_util::{SinkExt, StreamExt};
use hyper::{body::Incoming, Request, Response};
use hyper_util::rt::TokioIo;
use serde_json::{json, Value};
use std::{
    convert::Infallible,
    future::Future,
    sync::{Arc, Mutex},
    time::Duration,
};

const OLD: &str = "old";
const NEW: &str = "new";
const SESSION: &str = "fixture-ws-current-owner";

async fn server<F, Fut>(handler: F) -> u16
where
    F: Fn(Request<Incoming>) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Response<replay::WireBody>> + Send + 'static,
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
                        TokioIo::new(stream),
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

async fn native_endpoint(name: &'static str) -> (u16, Arc<Mutex<Vec<Vec<u8>>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let captured = seen.clone();
    let port = server(move |mut request| {
        let captured = captured.clone();
        async move {
            let (response, upgrade) = yawc::WebSocket::upgrade(&mut request).unwrap();
            tokio::spawn(async move {
                let Ok(mut peer) = upgrade.await else { return };
                while let Some(frame) = peer.next().await {
                    if frame.opcode().is_control() {
                        if frame.opcode() == yawc::OpCode::Close {
                            break;
                        }
                        continue;
                    }
                    let bytes = frame.payload().to_vec();
                    captured.lock().unwrap().push(bytes);
                    let event = json!({
                        "type": "response.completed",
                        "response": {
                            "id": format!("fixture-response-{name}"),
                            "model": "fixture-model",
                            "status": "completed",
                            "provider": name,
                            "output": [],
                            "usage": {"input_tokens": 1, "output_tokens": 1}
                        }
                    })
                    .to_string();
                    if peer.send(yawc::Frame::text(event)).await.is_err() {
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
                search_only: None,
                quota_access_token: None,
                id: None,
                base_url: url,
                token: "fixture-token".into(),
                name: None,
            },
        );
    }
    update(
        &gateway,
        &dir,
        Edit::Mode {
            mode: "auto".into(),
        },
    );
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

async fn ws_turn(peer: &mut yawc::TcpWebSocket, body: &str) -> Value {
    peer.send(yawc::Frame::text(body.to_owned())).await.unwrap();
    let frame = tokio::time::timeout(Duration::from_secs(4), peer.next())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(frame.opcode(), yawc::OpCode::Text);
    serde_json::from_slice(frame.payload()).unwrap()
}

fn digest(value: &str) -> String {
    crate::storage::digest(value.as_bytes())
}

fn ids(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).into()).collect()
}

#[tokio::test]
async fn websocket_later_turn_uses_ledger_owner_after_external_handoff() {
    let (old_port, old_seen) = native_endpoint(OLD).await;
    let (new_port, new_seen) = native_endpoint(NEW).await;
    let (dir, gateway) = fixture(vec![
        format!("http://127.0.0.1:{old_port}/v1"),
        format!("http://127.0.0.1:{new_port}/v1"),
    ])
    .await;
    let provider_ids: Vec<_> = gateway
        .view()
        .providers
        .iter()
        .map(|provider| provider.id.clone())
        .collect();
    assert_eq!(provider_ids.len(), 2);
    let old_id = provider_ids[0].clone();
    let new_id = provider_ids[1].clone();
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
            .header("session_id", SESSION),
    )
    .await
    .unwrap();

    let ordinary = r#"{"type":"response.create","model":"fixture-model","input":"fixture"}"#;
    assert_eq!(
        ws_turn(&mut peer, ordinary).await["response"]["provider"],
        OLD
    );
    assert_eq!(old_seen.lock().unwrap().len(), 1);

    // Construct a legal boundary handoff in the ledger while the existing
    // native connection remains open.  The next turn has no cursor and must
    // follow this current owner rather than the old upstream connection.
    let session = digest(SESSION);
    let boundary = digest("fixture-external-handoff-boundary");
    let registry: &Arc<Registry> = &gateway.0.compaction;
    let marker = registry
        .prepare(
            session.clone(),
            None,
            false,
            &ids(&[old_id.as_str(), new_id.as_str()]),
        )
        .unwrap();
    marker.complete(&old_id, Some(&boundary));
    let handoff = registry
        .prepare(
            session.clone(),
            Some(&boundary),
            true,
            &ids(&[new_id.as_str(), old_id.as_str()]),
        )
        .unwrap();
    assert!(handoff.handoff);
    handoff.complete(&new_id, None);
    registry.flush();
    assert_eq!(registry.owner(&session).as_deref(), Some(new_id.as_str()));

    assert_eq!(
        ws_turn(&mut peer, ordinary).await["response"]["provider"],
        NEW
    );
    assert_eq!(
        gateway.0.compaction.owner(&session).as_deref(),
        Some(new_id.as_str())
    );
    assert_eq!(old_seen.lock().unwrap().len(), 1);
    assert_eq!(new_seen.lock().unwrap().len(), 1);
    assert_eq!(new_seen.lock().unwrap()[0], ordinary.as_bytes());

    drop(peer);
    gateway.stop().await.unwrap();
}
