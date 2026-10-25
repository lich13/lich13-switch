//! Hold the HTTP consumer after its prefix so the following error frame cannot
//! be acknowledged. This exercises both forwarding paths, not a copied helper.
use super::super::{protocol::Delivery, wire_progress::Activity, Edit, Gateway};
use super::{forward, replay};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{body::Incoming, Request, Response};
use hyper_util::rt::TokioIo;
use std::{
    convert::Infallible,
    sync::{atomic::Ordering, Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{oneshot, Notify},
};

#[derive(Clone, Copy)]
enum ConsumerEnd {
    Stall,
    Close,
    Stop,
}

async fn run_case(streaming: bool, ending: ConsumerEnd) -> (u8, bool, Vec<String>) {
    let temp = tempfile::tempdir().unwrap();
    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_port = upstream.local_addr().unwrap().port();
    let prefix: &'static [u8] = if streaming {
        b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"fixture\"}\n\n"
    } else {
        b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"fixture\"}"
    };
    let fail_upstream = Arc::new(Notify::new());
    let fail_signal = fail_upstream.clone();
    let upstream_task = tokio::spawn(async move {
        let (mut socket, _) = upstream.accept().await.unwrap();
        let mut incoming = Vec::new();
        let mut chunk = [0; 1024];
        while !incoming.windows(4).any(|part| part == b"\r\n\r\n") {
            let count = socket.read(&mut chunk).await.unwrap();
            if count == 0 {
                return;
            }
            incoming.extend_from_slice(&chunk[..count]);
        }
        socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", prefix.len() + 20).as_bytes()).await.unwrap();
        socket.write_all(prefix).await.unwrap();
        if streaming {
            fail_signal.notified().await;
        }
        socket.shutdown().await.unwrap();
    });
    let base = format!("http://127.0.0.1:{upstream_port}/v1");
    std::fs::write(temp.path().join("config.toml"), format!("model_provider='custom'\n[model_providers.custom]\nbase_url='{base}'\nexperimental_bearer_token='fixture-upstream-token'\nwire_api='responses'\n")).unwrap();
    let gateway = Gateway::new(temp.path().join("data")).unwrap();
    let held = TcpListener::bind("127.0.0.1:0").await.unwrap();
    gateway
        .set_listener_port(
            held.local_addr().unwrap().port(),
            &gateway.listener_view().revision,
        )
        .unwrap();
    drop(held);
    gateway
        .edit(
            Edit::SaveProvider {
                search_only: None,
                quota_access_token: None,
                id: None,
                base_url: base,
                token: "fixture-upstream-token".into(),
                name: None,
            },
            &gateway.view().revision,
            temp.path(),
        )
        .unwrap();
    let mut settings = gateway.view().settings;
    settings.idle_seconds = 1;
    settings.max_retries = 0;
    gateway
        .edit(
            Edit::Settings { settings },
            &gateway.view().revision,
            temp.path(),
        )
        .unwrap();
    gateway
        .start(&gateway.view().revision, temp.path())
        .await
        .unwrap();
    let token = gateway.0.inner.lock().unwrap().store.local_token.clone();
    let delivery = Arc::new(Delivery::default());
    let activity = Arc::new(Activity::default());
    delivery.track(activity.clone());
    let held_body = Arc::new(Notify::new());
    let (seen, prefix_received) = oneshot::channel();
    let seen = Arc::new(Mutex::new(Some(seen)));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let captured = (
        gateway.clone(),
        delivery.clone(),
        activity.clone(),
        held_body.clone(),
    );
    let handler = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let service = hyper::service::service_fn(move |mut request: Request<Incoming>| {
            let (gateway, delivery, activity, held_body) = captured.clone();
            let seen = seen.clone();
            async move {
                request.extensions_mut().insert(delivery);
                request.extensions_mut().insert(activity);
                let response = forward(gateway, request).await;
                let mut body = response.into_body();
                let bytes = body.frame().await.unwrap().unwrap().into_data().unwrap();
                seen.lock()
                    .unwrap()
                    .take()
                    .unwrap()
                    .send(bytes.to_vec())
                    .unwrap();
                held_body.notified().await;
                drop(body);
                Ok::<_, Infallible>(Response::new(replay::full(Vec::new())))
            }
        });
        hyper::server::conn::http1::Builder::new()
            .serve_connection(TokioIo::new(socket), service)
            .await
            .unwrap();
    });
    let socket = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(socket))
        .await
        .unwrap();
    let connection = tokio::spawn(async move {
        let _ = connection.await;
    });
    let client = tokio::spawn(async move {
        let request = Request::builder()
            .method("POST")
            .uri("/v1/responses")
            .header("host", "localhost")
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .header("connection", "close")
            .body(Full::new(Bytes::from_static(
                b"{\"model\":\"fixture-model\",\"input\":\"fixture\"}",
            )))
            .unwrap();
        sender
            .send_request(request)
            .await
            .unwrap()
            .into_body()
            .collect()
            .await
            .unwrap();
    });
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), prefix_received)
            .await
            .unwrap()
            .unwrap(),
        prefix
    );
    fail_upstream.notify_one();
    match ending {
        ConsumerEnd::Stall => {}
        ConsumerEnd::Close => {
            held_body.notify_one();
        }
        ConsumerEnd::Stop => {
            gateway.stop().await.unwrap();
        }
    }
    // Observe settlement, not an assumed scheduling delay after notifying the
    // upstream. Assertions below still require exactly one first-timeout event.
    tokio::time::timeout(Duration::from_secs(5), async {
        while !delivery.is_settled() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("blocked error frame did not settle");
    let result = (
        delivery.error.load(Ordering::Acquire),
        delivery.is_settled(),
        activity
            .snapshot(1)
            .events
            .into_iter()
            .map(|event| event.kind)
            .collect(),
    );
    held_body.notify_one();
    tokio::time::timeout(Duration::from_secs(5), client)
        .await
        .unwrap()
        .unwrap();
    connection.await.unwrap();
    handler.await.unwrap();
    upstream_task.await.unwrap();
    gateway.stop().await.unwrap();
    result
}

#[tokio::test]
async fn error_frame_timeout_is_recorded_at_the_first_delivery_deadline() {
    for streaming in [false, true] {
        let (error, settled, events) = run_case(streaming, ConsumerEnd::Stall).await;
        assert_eq!(
            error, 3,
            "the error frame's first write timeout must be retained"
        );
        assert!(settled);
        assert_eq!(
            events
                .iter()
                .filter(|event| event.as_str() == "downstream_write_timeout")
                .count(),
            1
        );
    }
}

#[tokio::test]
async fn error_frame_consumer_drop_keeps_cancellation_evidence() {
    let (error, settled, events) = run_case(false, ConsumerEnd::Close).await;
    assert_eq!(error, 1);
    assert!(settled);
    assert!(events
        .iter()
        .any(|event| event == "response_body_cancelled"));
}

#[tokio::test]
async fn gateway_stop_settles_a_blocked_error_frame_without_a_write_timeout() {
    let (error, settled, events) = run_case(false, ConsumerEnd::Stop).await;
    assert_eq!(error, 0);
    assert!(settled);
    assert!(events.iter().any(|event| event == "gateway_stopping"));
    assert!(!events
        .iter()
        .any(|event| event == "downstream_write_timeout"));
}
