//! Real loopback connection failures exercise the owner retry and replay boundary.
use super::*;

const FOLLOWUP: &[u8] = br#"{"model":"fixture-model","input":"followup"}"#;

async fn serve_once(listener: tokio::net::TcpListener, status: u16) {
    let (socket, _) = listener.accept().await.unwrap();
    drop(listener);
    let _ = hyper::server::conn::http1::Builder::new()
        .serve_connection(
            hyper_util::rt::TokioIo::new(socket),
            hyper::service::service_fn(move |request: Request<Incoming>| async move {
                let _ = request.into_body().collect().await.unwrap();
                Ok::<_, Infallible>(
                    Response::builder()
                        .status(status)
                        .header("content-type", "application/json")
                        .header("connection", "close")
                        .body(replay::full(if status == 200 {
                            r#"{"provider":"owner"}"#
                        } else {
                            r#"{"error":{"message":"fixture 502"}}"#
                        }))
                        .unwrap(),
                )
            }),
        )
        .await;
}

pub(super) async fn setup() -> (tempfile::TempDir, Gateway, u16, Arc<AtomicUsize>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let first = tokio::spawn(serve_once(listener, 200));
    let hits = Arc::new(AtomicUsize::new(0));
    let seen = hits.clone();
    let backup = server(move |_| {
        seen.fetch_add(1, Ordering::SeqCst);
        async { json_response(200, r#"{"provider":"backup"}"#, None) }
    })
    .await;
    let (dir, gateway) = fixture(vec![
        format!("http://127.0.0.1:{port}/v1"),
        format!("http://127.0.0.1:{backup}/v1"),
    ])
    .await;
    let mut settings = gateway.view().settings;
    settings.connect_seconds = 1;
    settings.first_byte_seconds = 2;
    settings.max_retries = 3;
    update(&gateway, &dir, Edit::Settings { settings });
    for id in gateway
        .view()
        .providers
        .iter()
        .map(|p| p.id.clone())
        .collect::<Vec<_>>()
    {
        update(&gateway, &dir, Edit::RpmProvider { id, max_rpm: 100 });
    }
    start(&gateway, &dir).await;
    let response = request(
        &gateway,
        "fixture-owner-network",
        br#"{"model":"fixture-model","input":[{"role":"user","content":"first"}]}"#.to_vec(),
    )
    .await;
    assert_eq!(response.status(), 200);
    response.into_body().collect().await.unwrap();
    first.await.unwrap();
    (dir, gateway, port, hits)
}

pub(super) async fn waiting(gateway: &Gateway) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while gateway.view().transient_retries.is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("owner retry was not registered");
    assert_eq!(gateway.view().providers[0].active_requests, 0);
}

async fn followup(gateway: &Gateway) -> Value {
    let r = request(gateway, "fixture-owner-network", FOLLOWUP.to_vec()).await;
    assert_eq!(r.status(), 200);
    serde_json::from_slice(&r.into_body().collect().await.unwrap().to_bytes()).unwrap()
}

async fn settled(gateway: &Gateway) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while gateway
            .view()
            .providers
            .iter()
            .any(|p| p.active_requests != 0)
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("request permits did not settle");
}

#[tokio::test]
async fn first_connection_failure_recovers_on_owner_without_touching_backup() {
    let (_dir, gateway, port, backup) = setup().await;
    let g = gateway.clone();
    let pending = tokio::spawn(async move { followup(&g).await });
    waiting(&gateway).await;
    assert_eq!(backup.load(Ordering::SeqCst), 0);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .unwrap();
    let restored = tokio::spawn(serve_once(listener, 200));
    assert_eq!(pending.await.unwrap()["provider"], "owner");
    restored.await.unwrap();
    assert_eq!(backup.load(Ordering::SeqCst), 0);
    assert_eq!(gateway.view().providers[0].rpm_used, 3);
    gateway.stop().await.unwrap();
}

#[tokio::test]
async fn persistent_connection_failures_retry_twice_before_safe_failover() {
    let (_dir, gateway, _port, backup) = setup().await;
    assert_eq!(followup(&gateway).await["provider"], "backup");
    assert_eq!(gateway.view().providers[0].rpm_used, 4);
    assert_eq!(gateway.view().providers[1].rpm_used, 1);
    assert_eq!(backup.load(Ordering::SeqCst), 1);
    settled(&gateway).await;
    assert!(gateway
        .view()
        .providers
        .iter()
        .all(|p| p.active_requests == 0));
    gateway.stop().await.unwrap();
}

#[tokio::test]
async fn owner_retries_obey_the_configured_attempt_limit() {
    let (dir, gateway, _port, backup) = setup().await;
    let mut settings = gateway.view().settings;
    settings.max_retries = 1;
    update(&gateway, &dir, Edit::Settings { settings });
    let r = request(&gateway, "fixture-owner-network", FOLLOWUP.to_vec()).await;
    assert!(r.status().is_server_error());
    r.into_body().collect().await.unwrap();
    assert_eq!(gateway.view().providers[0].rpm_used, 3);
    assert_eq!(backup.load(Ordering::SeqCst), 0);
    gateway.stop().await.unwrap();
}

#[tokio::test]
async fn owner_open_circuit_does_not_block_eligible_failover() {
    let (dir, gateway, _port, backup) = setup().await;
    let mut settings = gateway.view().settings;
    settings.failure_threshold = 1;
    update(&gateway, &dir, Edit::Settings { settings });
    assert_eq!(followup(&gateway).await["provider"], "backup");
    assert_eq!(gateway.view().providers[0].rpm_used, 2);
    assert_eq!(backup.load(Ordering::SeqCst), 1);
    gateway.stop().await.unwrap();
}

#[tokio::test]
async fn connection_and_5xx_failures_share_two_owner_retries() {
    let (_dir, gateway, port, backup) = setup().await;
    let g = gateway.clone();
    let pending = tokio::spawn(async move { followup(&g).await });
    waiting(&gateway).await;
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .unwrap();
    let middle = tokio::spawn(serve_once(listener, 502));
    assert_eq!(pending.await.unwrap()["provider"], "backup");
    middle.await.unwrap();
    assert_eq!(gateway.view().providers[0].rpm_used, 4);
    assert_eq!(backup.load(Ordering::SeqCst), 1);
    gateway.stop().await.unwrap();
}

#[tokio::test]
async fn cancellation_and_stop_during_owner_wait_do_not_send_another_attempt() {
    for stop in [false, true] {
        let (_dir, gateway, _port, backup) = setup().await;
        let (token, port) = {
            let state = gateway.0.inner.lock().unwrap();
            (state.store.local_token.clone(), state.store.settings.port)
        };
        let mut client = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        use tokio::io::AsyncWriteExt;
        client.write_all(format!("POST /v1/responses HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nsession_id: fixture-owner-network\r\nContent-Length: {}\r\n\r\n", FOLLOWUP.len()).as_bytes()).await.unwrap();
        client.write_all(FOLLOWUP).await.unwrap();
        waiting(&gateway).await;
        if stop {
            gateway.stop().await.unwrap();
        } else {
            #[allow(deprecated)]
            client.set_linger(Some(Duration::ZERO)).unwrap();
            drop(client);
            tokio::time::timeout(Duration::from_secs(2), async {
                while !gateway.view().transient_retries.is_empty() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("cancelled retry remained waiting");
            gateway.stop().await.unwrap();
        }
        assert_eq!(backup.load(Ordering::SeqCst), 0);
        assert_eq!(gateway.view().providers[0].rpm_used, 2, "stop={stop}");
        assert!(gateway
            .view()
            .providers
            .iter()
            .all(|p| p.active_requests == 0));
    }
}

#[tokio::test]
async fn dispatched_request_with_no_headers_is_not_replayed_on_owner_or_backup() {
    let (_dir, gateway, port, backup) = setup().await;
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .unwrap();
    let silent = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        use tokio::io::AsyncReadExt;
        let mut body = [0; 4096];
        let read = socket.read(&mut body).await.unwrap();
        assert!(read > 0);
        std::future::pending::<()>().await;
    });
    let r = request(&gateway, "fixture-owner-network", FOLLOWUP.to_vec()).await;
    assert_eq!(r.status(), 504);
    let body = r.into_body().collect().await.unwrap().to_bytes();
    assert!(String::from_utf8_lossy(&body).contains("未自动重放"));
    assert_eq!(backup.load(Ordering::SeqCst), 0);
    assert_eq!(gateway.view().providers[0].rpm_used, 2);
    silent.abort();
    let _ = silent.await;
    gateway.stop().await.unwrap();
}
