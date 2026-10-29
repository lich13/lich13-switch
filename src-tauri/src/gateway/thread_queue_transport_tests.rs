use super::*;
use serde_json::json;
struct ConnectionTask(JoinHandle<()>);
impl Drop for ConnectionTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn http(g: Gateway, thread: &'static str, label: &'static str) -> (StatusCode, Vec<u8>) {
    http_path(g, thread, label, "/v1/responses").await
}
async fn http_path(
    g: Gateway,
    thread: &'static str,
    label: &'static str,
    path: &'static str,
) -> (StatusCode, Vec<u8>) {
    let port = g.listener_view().port;
    let token = g.0.inner.lock().unwrap().store.local_token.clone();
    let socket = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    #[allow(deprecated)]
    socket.set_linger(Some(Duration::ZERO)).unwrap();
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(socket))
        .await
        .unwrap();
    let task = tokio::spawn(async move {
        let _ = connection.await;
    });
    let _connection = ConnectionTask(task);
    let response = sender.send_request(Request::builder()
        .method("POST").uri(path)
        .header("host", format!("127.0.0.1:{port}"))
        .header("authorization", format!("Bearer {token}"))
        .header("thread-id", thread)
        .header("content-type", "application/json")
        .body(replay::full(json!({"model":MODEL,"input":"fixture","stream":true,"metadata":{"fixture":label}}).to_string())).unwrap()).await.unwrap();
    (response.status(), response_bytes(response).await)
}
async fn queued(g: &Gateway, count: usize) {
    tokio::time::timeout(WAIT, async {
        loop {
            if g.0
                .pending_requests
                .view()
                .iter()
                .filter(|r| r.phase == "thread_queue")
                .count()
                == count
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("thread wait should be visible");
}

#[tokio::test]
async fn http_fifo_waits_for_sse_completion_and_other_threads_run() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let release = Arc::new(Notify::new());
    let ready = Arc::new(Notify::new());
    let upstream = server({
        let seen = seen.clone(); let release = release.clone(); let ready = ready.clone();
        move |request| {
            let seen = seen.clone(); let release = release.clone(); let ready = ready.clone();
            async move {
                let bytes = request.into_body().collect().await.unwrap().to_bytes();
                let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                let label = body["metadata"]["fixture"].as_str().unwrap().to_owned();
                seen.lock().unwrap().push(label.clone());
                ready.notify_one();
                let chunks = stream::once(async {
                    Ok::<_, Infallible>(hyper::body::Frame::data(Bytes::from_static(b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"fixture\"}\n\n")))
                }).chain(stream::once(async move {
                    if label == "first" { release.notified().await; }
                    Ok::<_, Infallible>(hyper::body::Frame::data(Bytes::from_static(SSE_BODY)))
                }));
                Response::builder().header("content-type", "text/event-stream").body(StreamBody::new(chunks).map_err(|never| match never {}).boxed_unsync()).unwrap()
            }
        }
    }).await;
    let fixture = Fixture::new(&upstream.url("/v1")).await;
    fixture.start().await;
    let first = tokio::spawn(http(fixture.codex.clone(), "fixture-thread", "first"));
    tokio::time::timeout(WAIT, ready.notified()).await.unwrap();
    let second = tokio::spawn(http_path(
        fixture.codex.clone(),
        "fixture-thread",
        "second",
        "/v1/responses/compact",
    ));
    queued(&fixture.codex, 1).await;
    assert_eq!(fixture.codex.0.admission.counts().1, 1);
    let other = tokio::time::timeout(WAIT, http(fixture.codex.clone(), "fixture-other", "other"))
        .await
        .unwrap();
    assert_eq!(other.0, StatusCode::OK);
    assert_eq!(*seen.lock().unwrap(), ["first", "other"]);
    let cancel = tokio::time::timeout(
        WAIT,
        http_path(
            fixture.codex.clone(),
            "fixture-thread",
            "cancel",
            "/v1/responses/fixture/cancel",
        ),
    )
    .await
    .unwrap();
    assert_eq!(cancel.0, StatusCode::OK);
    assert_eq!(*seen.lock().unwrap(), ["first", "other", "cancel"]);
    release.notify_one();
    assert_eq!(
        tokio::time::timeout(WAIT, first).await.unwrap().unwrap().0,
        StatusCode::OK
    );
    assert_eq!(
        tokio::time::timeout(WAIT, second).await.unwrap().unwrap().0,
        StatusCode::OK
    );
    assert_eq!(
        *seen.lock().unwrap(),
        ["first", "other", "cancel", "second"]
    );
    queued(&fixture.codex, 0).await;
}

#[tokio::test]
async fn queued_http_cancel_and_disconnect_release_wait_slots() {
    let release = Arc::new(Notify::new());
    let ready = Arc::new(Notify::new());
    let upstream = server({
        let release = release.clone();
        let ready = ready.clone();
        move |request| {
            let release = release.clone();
            let ready = ready.clone();
            async move {
                let bytes = request.into_body().collect().await.unwrap().to_bytes();
                let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                if body["metadata"]["fixture"] == "first" {
                    ready.notify_one();
                    release.notified().await;
                }
                Response::builder()
                    .header("content-type", "text/event-stream")
                    .body(replay::full(SSE_BODY))
                    .unwrap()
            }
        }
    })
    .await;
    let fixture = Fixture::new(&upstream.url("/v1")).await;
    fixture.start().await;
    let first = tokio::spawn(http(fixture.codex.clone(), "fixture-thread", "first"));
    tokio::time::timeout(WAIT, ready.notified()).await.unwrap();
    let cancelled = tokio::spawn(http(fixture.codex.clone(), "fixture-thread", "cancelled"));
    queued(&fixture.codex, 1).await;
    let id = fixture
        .codex
        .0
        .pending_requests
        .view()
        .into_iter()
        .find(|r| r.phase == "thread_queue")
        .unwrap()
        .request_id;
    assert!(fixture.codex.0.pending_requests.cancel(&id));
    assert_eq!(
        tokio::time::timeout(WAIT, cancelled)
            .await
            .unwrap()
            .unwrap()
            .0
            .as_u16(),
        499
    );
    let disconnected = tokio::spawn(http(
        fixture.codex.clone(),
        "fixture-thread",
        "disconnected",
    ));
    queued(&fixture.codex, 1).await;
    disconnected.abort();
    let _ = disconnected.await;
    queued(&fixture.codex, 0).await;
    assert_eq!(fixture.codex.0.admission.counts().1, 0);
    release.notify_one();
    assert_eq!(
        tokio::time::timeout(WAIT, first).await.unwrap().unwrap().0,
        StatusCode::OK
    );
    assert_eq!(
        http(fixture.codex.clone(), "fixture-thread", "after")
            .await
            .0,
        StatusCode::OK
    );
    let held = tokio::spawn(http(fixture.codex.clone(), "fixture-thread", "first"));
    tokio::time::timeout(WAIT, ready.notified()).await.unwrap();
    let waiting = tokio::spawn(http(fixture.codex.clone(), "fixture-thread", "stopped"));
    queued(&fixture.codex, 1).await;
    tokio::time::timeout(WAIT, fixture.codex.stop())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        tokio::time::timeout(WAIT, waiting)
            .await
            .unwrap()
            .unwrap()
            .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(
        tokio::time::timeout(WAIT, held).await.unwrap().unwrap().0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(fixture.codex.0.admission.counts().1, 0);
}

#[tokio::test]
async fn websocket_and_http_share_fifo_across_connections_and_reused_turns() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let release = Arc::new(Notify::new());
    let ready = Arc::new(Notify::new());
    let tasks = TaskScope::default();
    let upstream = server({
        let seen = seen.clone(); let release = release.clone(); let ready = ready.clone(); let tasks = tasks.clone();
        move |mut request| {
            let seen = seen.clone(); let release = release.clone(); let ready = ready.clone(); let tasks = tasks.clone();
            async move {
                if request.headers().contains_key("upgrade") {
                    let (response, upgrade) = yawc::WebSocket::upgrade(&mut request).unwrap();
                    tasks.spawn(async move {
                        let mut socket = upgrade.await.unwrap();
                        while let Some(frame) = socket.next().await {
                            if frame.opcode() == yawc::OpCode::Close { break; }
                            if frame.opcode().is_control() { continue; }
                            let body: serde_json::Value = serde_json::from_slice(frame.payload()).unwrap();
                            let label = body["metadata"]["fixture"].as_str().unwrap().to_owned();
                            seen.lock().unwrap().push(label.clone());
                            ready.notify_one();
                            if label == "first" { release.notified().await; }
                            socket.send(yawc::Frame::text(json!({"type":"response.completed","response":{"id":format!("fixture-{label}"),"model":MODEL,"status":"completed","output":[],"usage":{"input_tokens":1,"output_tokens":1}}}).to_string())).await.unwrap();
                        }
                    });
                    response.map(|_| replay::empty())
                } else {
                    let body: serde_json::Value = serde_json::from_slice(&request.into_body().collect().await.unwrap().to_bytes()).unwrap();
                    seen.lock().unwrap().push(body["metadata"]["fixture"].as_str().unwrap().to_owned());
                    Response::builder().header("content-type", "text/event-stream").body(replay::full(SSE_BODY)).unwrap()
                }
            }
        }
    }).await;
    let fixture = Fixture::new(&upstream.url("/v1")).await;
    fixture.start().await;
    let open = || {
        yawc::WebSocket::connect(
            format!("ws://127.0.0.1:{}/v1/responses", fixture.listener_port)
                .parse()
                .unwrap(),
        )
        .with_request(
            yawc::HttpRequest::builder()
                .header(
                    "authorization",
                    format!("Bearer {}", fixture.local_token(&fixture.codex)),
                )
                .header("thread-id", "fixture-thread"),
        )
    };
    let create = |label: &str| {
        yawc::Frame::text(json!({"type":"response.create","model":MODEL,"input":"fixture","metadata":{"fixture":label}}).to_string())
    };
    let mut first = open().await.unwrap();
    first.send(create("first")).await.unwrap();
    tokio::time::timeout(WAIT, ready.notified()).await.unwrap();
    let second = tokio::spawn(http(fixture.codex.clone(), "fixture-thread", "second"));
    queued(&fixture.codex, 1).await;
    let mut third = open().await.unwrap();
    third.send(create("third")).await.unwrap();
    queued(&fixture.codex, 2).await;
    first.send(create("fourth")).await.unwrap();
    tokio::time::timeout(WAIT, async {
        while fixture.codex.0.admission.counts().1 != 3 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    first.send(create("fifth")).await.unwrap();
    queued(&fixture.codex, 4).await;
    let previous: std::collections::HashSet<_> = fixture
        .codex
        .0
        .pending_requests
        .view()
        .into_iter()
        .map(|r| r.request_id)
        .collect();
    first.send(create("cancelled-pending")).await.unwrap();
    queued(&fixture.codex, 5).await;
    let pending_id = fixture
        .codex
        .0
        .pending_requests
        .view()
        .into_iter()
        .find(|r| !previous.contains(&r.request_id))
        .unwrap()
        .request_id;
    assert!(fixture.codex.0.pending_requests.cancel(&pending_id));
    queued(&fixture.codex, 4).await;
    let previous: std::collections::HashSet<_> = fixture
        .codex
        .0
        .pending_requests
        .view()
        .into_iter()
        .map(|r| r.request_id)
        .collect();
    let mut cancelled = open().await.unwrap();
    cancelled
        .send(create("cancelled-connection"))
        .await
        .unwrap();
    queued(&fixture.codex, 5).await;
    let pending_id = fixture
        .codex
        .0
        .pending_requests
        .view()
        .into_iter()
        .find(|r| !previous.contains(&r.request_id))
        .unwrap()
        .request_id;
    assert!(fixture.codex.0.pending_requests.cancel(&pending_id));
    assert!(String::from_utf8_lossy(
        tokio::time::timeout(WAIT, cancelled.next())
            .await
            .unwrap()
            .unwrap()
            .payload()
    )
    .contains("response.cancelled"));
    drop(cancelled);
    queued(&fixture.codex, 4).await;
    assert_eq!(*seen.lock().unwrap(), ["first"]);
    release.notify_one();
    tokio::time::timeout(WAIT, async {
        assert_eq!(second.await.unwrap().0, StatusCode::OK);
        while !String::from_utf8_lossy(third.next().await.unwrap().payload())
            .contains("response.completed")
        {}
        let mut completed = 0;
        while completed < 3 {
            if String::from_utf8_lossy(first.next().await.unwrap().payload())
                .contains("response.completed")
            {
                completed += 1;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(
        *seen.lock().unwrap(),
        ["first", "second", "third", "fourth", "fifth"]
    );
    tasks.close();
}
