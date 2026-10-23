//! v0.25.10 quota evidence and cancellable probe regressions.
//!
//! These tests stay below the quota service's public surface on purpose.  The
//! module is included from `quota_probe.rs`, so it can assert the private
//! decimal classifier and the service's single-flight state without adding a
//! product-facing hook.
use super::*;
use bytes::Bytes;
use hyper::{body::Incoming, Request, Response, StatusCode};
use hyper_util::{client::legacy::Client, rt::TokioExecutor};
use serde_json::{json, Value};
use std::{
    convert::Infallible,
    future::Future,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::sync::oneshot;

fn check(value: Value) -> QuotaCheck {
    sub2_evidence(&value)
}

fn verdict(value: Value) -> QuotaVerdict {
    check(value).verdict
}

fn http_client() -> HttpClient {
    Client::builder(TokioExecutor::new())
        .retry_canceled_requests(false)
        .build(crate::gateway::connector::Connector::new(
            Duration::from_secs(2),
            0,
        ))
}

fn query(base: String, id: &str, version: &str) -> Query {
    Query {
        quota_access_token: None,
        client_id: crate::gateway::ClientId::Codex,
        id: id.into(),
        version: version.into(),
        base,
        token: "fixture-token".into(),
        client: http_client(),
    }
}

struct LoopbackServer {
    port: u16,
    stop: Option<oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl LoopbackServer {
    async fn stop(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for LoopbackServer {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

async fn server<F, Fut>(handler: F) -> LoopbackServer
where
    F: Fn(Request<Incoming>) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Response<replay::WireBody>> + Send + 'static,
{
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (stop, mut stopped) = oneshot::channel();
    let handler = Arc::new(handler);
    let task = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut stopped => break,
                accepted = listener.accept() => {
                    let Ok((stream, _)) = accepted else { break };
                    let handler = handler.clone();
                    tokio::spawn(async move {
                        let _ = hyper::server::conn::http1::Builder::new()
                            .serve_connection(
                                hyper_util::rt::TokioIo::new(stream),
                                hyper::service::service_fn(move |request| {
                                    let handler = handler.clone();
                                    async move { Ok::<_, Infallible>(handler(request).await) }
                                }),
                            )
                            .await;
                    });
                }
            }
        }
    });
    LoopbackServer {
        port,
        stop: Some(stop),
        task: Some(task),
    }
}

fn json_response(status: StatusCode, body: Value) -> Response<replay::WireBody> {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(replay::full(Bytes::from(body.to_string())))
        .unwrap()
}

#[test]
fn decimal_evidence_preserves_zero_negative_tiny_and_huge_values() {
    assert_eq!(
        verdict(json!({"mode":"balance","balance":0})),
        QuotaVerdict::Exhausted
    );
    assert_eq!(
        verdict(json!({"mode":"wallet","balance":-0.0001})),
        QuotaVerdict::Exhausted
    );
    assert_eq!(
        verdict(json!({"mode":"wallet","balance":"1e-400"})),
        QuotaVerdict::Available
    );
    let raw_number: Value = serde_json::from_str(r#"{"mode":"wallet","balance":1e-400}"#).unwrap();
    assert_eq!(verdict(raw_number), QuotaVerdict::Available);
    assert_eq!(
        verdict(json!({"mode":"wallet","balance":"1e4097"})),
        QuotaVerdict::Unknown
    );
    assert_eq!(
        verdict(json!({"mode":"wallet","balance":"Infinity"})),
        QuotaVerdict::Unknown
    );
    assert_eq!(
        verdict(json!({"mode":"wallet","balance":"NaN"})),
        QuotaVerdict::Unknown
    );
}

#[test]
fn key_spending_windows_are_distinct_from_rpm_and_conflicts_are_unknown() {
    let available = check(json!({
        "mode":"quota_limited",
        "quota":{"limit":"10.0000000000000000001","used":"10"},
        "rate_limits":[{"window":"rpm","limit":1,"used":1}]
    }));
    assert_eq!(available.verdict, QuotaVerdict::Available);
    assert_eq!(available.basis, QuotaBasis::KeyQuota);

    let exhausted = check(json!({
        "mode":"quota_limited",
        "quota":{"limit":10,"used":2,"remaining":8},
        "rate_limits":[{"window":"5h","limit":1,"used":1}]
    }));
    assert_eq!(exhausted.verdict, QuotaVerdict::Exhausted);
    assert_eq!(exhausted.basis, QuotaBasis::SpendingWindow);

    let contradictory = check(json!({
        "mode":"quota_limited",
        "quota":{"limit":10,"used":1,"remaining":0}
    }));
    assert_eq!(contradictory.verdict, QuotaVerdict::Unknown);
    assert_eq!(contradictory.basis, QuotaBasis::KeyQuota);
}

#[test]
fn incomplete_and_multiple_subscriptions_never_authorize_from_one_plan() {
    let incomplete = check(json!({
        "mode":"subscription",
        "subscription":{"active":true,"daily_limit_usd":10},
        "remaining":0
    }));
    assert_eq!(incomplete.verdict, QuotaVerdict::Unknown);
    assert_eq!(incomplete.basis, QuotaBasis::Incomplete);

    let multiple = check(json!({
        "mode":"subscription",
        "subscriptions":[
            {"active":true,"daily_limit_usd":10,"daily_usage_usd":10},
            {"active":true,"daily_limit_usd":10,"daily_usage_usd":0}
        ],
        "remaining":0
    }));
    assert_eq!(multiple.verdict, QuotaVerdict::Unknown);
    assert_eq!(multiple.basis, QuotaBasis::Incomplete);

    let new_api = new_api_evidence(&json!({
        "data":{"unlimited_quota":false,"total_available":"1e-400"}
    }));
    assert_eq!(new_api.verdict, QuotaVerdict::Available);
    assert_eq!(new_api.basis, QuotaBasis::KeyQuota);
}

#[tokio::test]
async fn probe_uses_one_flight_and_does_not_touch_api_status() {
    let hits = Arc::new(AtomicUsize::new(0));
    let status_hits = Arc::new(AtomicUsize::new(0));
    let counted = hits.clone();
    let counted_status = status_hits.clone();
    let running_server = server(move |request| {
        let path = request.uri().path().to_owned();
        let counted = counted.clone();
        let counted_status = counted_status.clone();
        async move {
            if path.ends_with("/api/status") {
                counted_status.fetch_add(1, Ordering::SeqCst);
                return json_response(StatusCode::OK, json!({"unexpected":true}));
            }
            counted.fetch_add(1, Ordering::SeqCst);
            json_response(StatusCode::OK, json!({"mode":"wallet","balance":0}))
        }
    })
    .await;
    let service = Service::new();
    let base = format!("http://127.0.0.1:{}/v1", running_server.port);
    let (left, right) = tokio::join!(
        service.probe(query(base.clone(), "p", "v1")),
        service.probe(query(base, "p", "v1")),
    );
    assert_eq!(left.verdict, QuotaVerdict::Exhausted);
    assert_eq!(right.verdict, QuotaVerdict::Exhausted);
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    assert_eq!(status_hits.load(Ordering::SeqCst), 0);
    running_server.stop().await;
}

#[tokio::test(start_paused = true)]
async fn probe_deadline_includes_semaphore_queue() {
    let service = Service::new();
    let permits = [
        service.0.permits.acquire().await.unwrap(),
        service.0.permits.acquire().await.unwrap(),
        service.0.permits.acquire().await.unwrap(),
    ];
    let queued_service = service.clone();
    let task = tokio::spawn(async move {
        queued_service
            .probe(query("http://127.0.0.1:9/v1".into(), "queued", "v1"))
            .await
    });
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(9)).await;
    assert!(!task.is_finished());
    tokio::time::advance(Duration::from_secs(1)).await;
    let result = task.await.unwrap();
    assert_eq!(result.verdict, QuotaVerdict::Unknown);
    drop(permits);
}

#[tokio::test]
async fn last_probe_subscriber_cancellation_stops_before_semaphore_acquire() {
    let hits = Arc::new(AtomicUsize::new(0));
    let counted = hits.clone();
    let running_server = server(move |_| {
        counted.fetch_add(1, Ordering::SeqCst);
        async { json_response(StatusCode::OK, json!({"mode":"wallet","balance":1})) }
    })
    .await;
    let service = Service::new();
    let permits = [
        service.0.permits.acquire().await.unwrap(),
        service.0.permits.acquire().await.unwrap(),
        service.0.permits.acquire().await.unwrap(),
    ];
    let base = format!("http://127.0.0.1:{}/v1", running_server.port);
    let cancelled_service = service.clone();
    let task = tokio::spawn(async move {
        cancelled_service
            .probe(query(base, "cancelled", "v1"))
            .await
    });
    tokio::task::yield_now().await;
    task.abort();
    let _ = task.await;
    drop(permits);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(hits.load(Ordering::SeqCst), 0);
    assert!(!service.probe_view("cancelled", "v1").0);
    running_server.stop().await;
}

#[tokio::test]
async fn retain_aborts_an_inflight_probe_for_removed_credentials() {
    let hits = Arc::new(AtomicUsize::new(0));
    let counted = hits.clone();
    let running_server = server(move |_| {
        counted.fetch_add(1, Ordering::SeqCst);
        async { json_response(StatusCode::OK, json!({"mode":"wallet","balance":1})) }
    })
    .await;
    let service = Service::new();
    let permits = [
        service.0.permits.acquire().await.unwrap(),
        service.0.permits.acquire().await.unwrap(),
        service.0.permits.acquire().await.unwrap(),
    ];
    let base = format!("http://127.0.0.1:{}/v1", running_server.port);
    let retained_service = service.clone();
    let task =
        tokio::spawn(async move { retained_service.probe(query(base, "removed", "v1")).await });
    tokio::task::yield_now().await;
    service.retain(&HashMap::new());
    drop(permits);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(hits.load(Ordering::SeqCst), 0);
    assert_eq!(service.probe_view("removed", "v1"), (false, None));
    task.abort();
    let _ = task.await;
    running_server.stop().await;
}

#[tokio::test]
async fn probe_429_retry_after_throttles_start_and_is_shared_with_display_refresh() {
    let hits = Arc::new(AtomicUsize::new(0));
    let counted = hits.clone();
    let running_server = server(move |_| {
        let n = counted.fetch_add(1, Ordering::SeqCst);
        async move {
            if n == 0 {
                Response::builder()
                    .status(StatusCode::TOO_MANY_REQUESTS)
                    .header("retry-after", "60")
                    .body(replay::full(Bytes::from_static(b"limited")))
                    .unwrap()
            } else {
                json_response(StatusCode::OK, json!({"mode":"wallet","balance":1}))
            }
        }
    })
    .await;
    let service = Service::new();
    let base = format!("http://127.0.0.1:{}/v1", running_server.port);
    let first = service.probe(query(base.clone(), "backoff", "v1")).await;
    assert_eq!(first.verdict, QuotaVerdict::Unknown);
    let second = service.probe(query(base, "backoff", "v1")).await;
    assert_eq!(second.verdict, QuotaVerdict::Unknown);
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    running_server.stop().await;
}

#[tokio::test(start_paused = true)]
async fn probe_started_again_after_two_seconds_waits_for_fresh_ten_second_start() {
    let hits = Arc::new(AtomicUsize::new(0));
    let counted = hits.clone();
    let running_server = server(move |_| {
        let n = counted.fetch_add(1, Ordering::SeqCst);
        async move {
            json_response(
                StatusCode::OK,
                if n == 0 {
                    json!({"mode":"wallet","balance":0})
                } else {
                    json!({"mode":"wallet","balance":1})
                },
            )
        }
    })
    .await;
    let service = Service::new();
    let base = format!("http://127.0.0.1:{}/v1", running_server.port);
    tokio::time::resume();
    let first = service.probe(query(base.clone(), "repeat", "v1")).await;
    assert_eq!(first.verdict, QuotaVerdict::Exhausted);
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(2)).await;
    let second_service = service.clone();
    let second_task =
        tokio::spawn(async move { second_service.probe(query(base, "repeat", "v1")).await });
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(7)).await;
    assert!(!second_task.is_finished());
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    tokio::time::advance(Duration::from_secs(1)).await;
    // Network readiness uses the real reactor. Resume before awaiting it so
    // the paused runtime cannot jump to the ten-second query deadline.
    tokio::time::resume();
    let second = second_task.await.unwrap();
    assert_eq!(second.verdict, QuotaVerdict::Available);
    assert_eq!(hits.load(Ordering::SeqCst), 2);
    running_server.stop().await;
}

#[tokio::test]
async fn probe_uses_fresh_credentials_after_cache_changes_and_drops_old_versions() {
    let responses = Arc::new(AtomicUsize::new(0));
    let seen = responses.clone();
    let running_server = server(move |_| {
        let n = seen.fetch_add(1, Ordering::SeqCst);
        async move {
            json_response(
                StatusCode::OK,
                if n == 0 {
                    json!({"mode":"wallet","balance":0})
                } else {
                    json!({"mode":"wallet","balance":1})
                },
            )
        }
    })
    .await;
    let service = Service::new();
    let base = format!("http://127.0.0.1:{}/v1", running_server.port);
    let old = service.probe(query(base.clone(), "cached", "v1")).await;
    assert_eq!(old.verdict, QuotaVerdict::Exhausted);
    let old_view = QuotaView {
        provider_id: "cached".into(),
        version: "v1".into(),
        state: "ok".into(),
        source: Some("sub2api".into()),
        checked_at: Some(now()),
        success_at: Some(now()),
        retry_at: None,
        next_refresh_at: None,
        stale: false,
        error: None,
        key_status: None,
        plans: vec![],
        subscriptions: vec![],
        expires_at: None,
        expires_at_unix: None,
        today: None,
        total_usage: None,
    };
    service.0.entries.lock().unwrap().insert(
        "cached".into(),
        Entry {
            view: old_view,
            flight: None,
        },
    );
    let fresh = service.probe(query(base.clone(), "cached", "v2")).await;
    assert_eq!(fresh.verdict, QuotaVerdict::Available);
    service.retain(&HashMap::from([(
        String::from("cached"),
        String::from("v2"),
    )]));
    assert_eq!(service.probe_view("cached", "v1"), (false, None));
    assert!(service.cached("cached", "v1").is_none());
    running_server.stop().await;
}

#[tokio::test]
async fn probe_new_zero_rejects_after_old_positive_cache() {
    let responses = Arc::new(AtomicUsize::new(0));
    let seen = responses.clone();
    let running_server = server(move |_| {
        let n = seen.fetch_add(1, Ordering::SeqCst);
        async move {
            json_response(
                StatusCode::OK,
                if n == 0 {
                    json!({"mode":"wallet","balance":1})
                } else {
                    json!({"mode":"wallet","balance":0})
                },
            )
        }
    })
    .await;
    let service = Service::new();
    let base = format!("http://127.0.0.1:{}/v1", running_server.port);
    let old = service.probe(query(base.clone(), "positive", "v1")).await;
    assert_eq!(old.verdict, QuotaVerdict::Available);
    let fresh = service.probe(query(base, "positive", "v2")).await;
    assert_eq!(fresh.verdict, QuotaVerdict::Exhausted);
    assert_eq!(responses.load(Ordering::SeqCst), 2);
    running_server.stop().await;
}
