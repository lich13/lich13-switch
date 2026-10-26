use super::*;
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tokio::sync::Semaphore;

const CREATED: &[u8] = b"data: {\"type\":\"response.created\",\"response\":{\"id\":\"fixture\",\"status\":\"in_progress\"}}\n\n";
const COMPACT: &[u8] = br#"{"model":"fixture-model","input":[{"role":"user","content":"fixture"},{"type":"compaction_trigger"}]}"#;

async fn configured(port: u16) -> (tempfile::TempDir, Gateway) {
    let (t, g) = fixture(vec![format!("http://127.0.0.1:{port}")]).await;
    let mut settings = g.view().settings;
    settings.first_byte_seconds = 1;
    settings.idle_seconds = 1;
    settings.retry_budget_seconds = 1;
    settings.max_retries = 0;
    update(&g, &t, Edit::Settings { settings });
    start(&g, &t).await;
    (t, g)
}
async fn executing(g: &Gateway) -> String {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Some(row) =
                g.0.pending_requests
                    .view()
                    .into_iter()
                    .find(|r| r.phase == "compaction")
            {
                assert_eq!(row.remaining_seconds, None);
                return row.request_id;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap()
}
async fn released(g: &Gateway) {
    tokio::time::timeout(Duration::from_secs(4), async {
        loop {
            if g.view().providers.iter().all(|p| p.active_requests == 0)
                && g.0.pending_requests.view().is_empty()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(g.view().waiting_requests, 0);
}
#[tokio::test]
async fn compaction_http_silent_headers_slow_json_sse_and_gzip_survive_all_response_deadlines() {
    for mode in 0..4 {
        let gate = Arc::new(Semaphore::new(0));
        let release = gate.clone();
        let port=server(move |_| {
            let release=release.clone();
            async move {
                let value=json!({"id":"fixture-completed","object":"response","status":"completed","output":[{"type":"compaction","encrypted_content":"fixture-window"}],"usage":{"input_tokens":3,"output_tokens":1}});
                let mut bytes=if mode==2 { format!("data: {}\n\n",json!({"type":"response.completed","response":value})).into_bytes() } else { value.to_string().into_bytes() };
                if mode==3 {
                    use std::io::Write;
                    let mut encoder=flate2::write::GzEncoder::new(Vec::new(),flate2::Compression::fast());
                    encoder.write_all(&bytes).unwrap(); bytes=encoder.finish().unwrap();
                }
                if mode==0 { let permit=release.acquire().await.unwrap(); permit.forget(); }
                let stream=async_stream::try_stream! {
                    let split=bytes.len()/2;
                    if mode==2 {yield Frame::data(Bytes::from_static(CREATED));}
                    yield Frame::data(Bytes::copy_from_slice(&bytes[..split]));
                    if mode!=0 {let permit=release.acquire().await.unwrap(); permit.forget();}
                    yield Frame::data(Bytes::copy_from_slice(&bytes[split..]));
                };
                Response::builder().header("content-type",if mode==2 {"text/event-stream"} else {"application/json"})
                    .header("content-encoding",if mode==3 {"gzip"} else {"identity"})
                    .body(StreamBody::new(stream).map_err(|e:std::io::Error|->connector::BoxError {e.into()}).boxed_unsync()).unwrap()
            }
        }).await;
        let (_t, g) = configured(port).await;
        let request_task = {
            let g = g.clone();
            tokio::spawn(async move {
                let response = request(
                    &g,
                    if mode == 2 {
                        "/v1/responses"
                    } else {
                        "/v1/responses/compact"
                    },
                    COMPACT.to_vec(),
                    vec![("content-type", "application/json")],
                )
                .await;
                let status = response.status();
                let body = response.into_body().collect().await.unwrap().to_bytes();
                (status, body)
            })
        };
        executing(&g).await;
        tokio::time::sleep(Duration::from_millis(1250)).await;
        assert!(
            !request_task.is_finished(),
            "mode {mode} ended during compaction"
        );
        assert_eq!(g.0.pending_requests.view()[0].remaining_seconds, None);
        gate.add_permits(1);
        let (status, body) = tokio::time::timeout(Duration::from_secs(4), request_task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(status, 200);
        let body = replay::decode_prefix(&body, if mode == 3 { "gzip" } else { "identity" }, 4096)
            .unwrap();
        assert!(String::from_utf8(body).unwrap().contains("fixture-window"));
        released(&g).await;
        g.stop().await.unwrap();
    }
}
#[tokio::test]
async fn compaction_cancel_disconnect_and_stop_release_pending_and_concurrency() {
    for action in 0..3 {
        let port = server(|_| async {
            let stream = async_stream::try_stream! {
                yield Frame::data(Bytes::from_static(CREATED));
                std::future::pending::<()>().await;
            };
            Response::builder()
                .header("content-type", "text/event-stream")
                .body(
                    StreamBody::new(stream)
                        .map_err(|e: std::io::Error| -> connector::BoxError { e.into() })
                        .boxed_unsync(),
                )
                .unwrap()
        })
        .await;
        let (_t, g) = configured(port).await;
        let (response, sender, connection) = request_with_reset(
            &g,
            "/v1/responses",
            COMPACT.to_vec(),
            vec![("content-type", "application/json")],
        )
        .await;
        let id = executing(&g).await;
        tokio::time::sleep(Duration::from_millis(1100)).await;
        match action {
            0 => {
                assert!(g.0.pending_requests.cancel(&id));
                let _ = response.into_body().collect().await;
                reset_and_drop(connection, sender, ()).await;
            }
            1 => reset_and_drop(connection, sender, response).await,
            _ => {
                g.stop().await.unwrap();
                reset_and_drop(connection, sender, response).await;
            }
        }
        released(&g).await;
        if action != 2 {
            g.stop().await.unwrap();
        }
    }
}
#[tokio::test]
async fn websocket_compaction_each_round_survives_silence_but_ordinary_round_still_times_out() {
    let gate = Arc::new(Semaphore::new(0));
    let release = gate.clone();
    let port=server(move |mut request| {
        let release=release.clone();async move {
            let (response,upgrade)=yawc::WebSocket::upgrade(&mut request).unwrap();
            tokio::spawn(async move {
                let mut peer=upgrade.await.unwrap();
                while let Some(frame)=peer.next().await {
                    if frame.opcode().is_control(){continue;}
                    let permit=release.acquire().await.unwrap();permit.forget();
                    if peer.send(yawc::Frame::text(json!({"type":"response.completed","response":{"id":"fixture","status":"completed","output":[{"type":"compaction","encrypted_content":"fixture-window"}]}}).to_string())).await.is_err(){break;}
                }
            });response.map(|_|replay::empty())
        }
    }).await;
    let (_t, g) = configured(port).await;
    let token = g.0.inner.lock().unwrap().store.local_token.clone();
    let mut peer = yawc::WebSocket::connect(
        format!("ws://127.0.0.1:{}/v1/responses", g.view().settings.port)
            .parse()
            .unwrap(),
    )
    .with_request(yawc::HttpRequest::builder().header("authorization", format!("Bearer {token}")))
    .await
    .unwrap();
    let mut payload: serde_json::Value = serde_json::from_slice(COMPACT).unwrap();
    payload["type"] = json!("response.create");
    for _ in 0..2 {
        peer.send(yawc::Frame::text(payload.to_string()))
            .await
            .unwrap();
        executing(&g).await;
        tokio::time::sleep(Duration::from_millis(1250)).await;
        assert_eq!(g.0.pending_requests.view()[0].remaining_seconds, None);
        gate.add_permits(1);
        let frame = tokio::time::timeout(Duration::from_secs(3), peer.next())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(frame.payload()).unwrap()["type"],
            "response.completed"
        );
    }
    payload["input"] = json!([{"type":"compaction","encrypted_content":"fixture-window"},{"role":"user","content":"fixture"}]);
    peer.send(yawc::Frame::text(payload.to_string()))
        .await
        .unwrap();
    let end = tokio::time::timeout(Duration::from_secs(4), peer.next())
        .await
        .unwrap();
    assert!(end.is_none_or(|frame| frame.opcode() == yawc::OpCode::Close));
    released(&g).await;
    g.stop().await.unwrap();
}
