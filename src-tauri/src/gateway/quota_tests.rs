use super::*;
use http_body_util::BodyExt;
use hyper::Response;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

fn quota_body() -> &'static str {
    r#"{"isValid":true,"quota":{"limit":10,"used":2,"remaining":8}}"#
}

#[tokio::test]
async fn quota_interval_defaults_to_sixty_seconds_and_can_be_changed_or_disabled() {
    let hits = Arc::new(AtomicUsize::new(0));
    let counted = hits.clone();
    let port = server(move |_| {
        counted.fetch_add(1, Ordering::SeqCst);
        async { Response::new(full(quota_body())) }
    })
    .await;
    let (_t, g) = fixture(vec![format!("http://127.0.0.1:{port}/v1")]).await;
    let id = g.view().providers[0].id.clone();
    let first = g.query_quota(&id, true).await.unwrap();
    let checked = first.checked_at.unwrap();
    assert!(first
        .next_refresh_at
        .is_some_and(|next| next >= checked + 60));
    assert!(first
        .next_refresh_at
        .is_some_and(|next| next <= checked + 61));
    assert_eq!(hits.load(Ordering::SeqCst), 1);

    g.set_quota_interval(0);
    let disabled = g.query_quota(&id, false).await.unwrap();
    assert_eq!(disabled.next_refresh_at, None);
    assert!(!disabled.stale);
    assert_eq!(hits.load(Ordering::SeqCst), 1);

    g.set_quota_interval(5);
    let changed = g.query_quota(&id, false).await.unwrap();
    assert!(changed
        .next_refresh_at
        .is_some_and(|next| next >= checked + 5));
    assert_eq!(hits.load(Ordering::SeqCst), 1);

    let forced = g.query_quota(&id, true).await.unwrap();
    assert_eq!(forced.state, "ok");
    assert_eq!(hits.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn quota_force_queries_bypass_cache_while_nonforced_queries_use_it() {
    let hits = Arc::new(AtomicUsize::new(0));
    let counted = hits.clone();
    let port = server(move |_| {
        counted.fetch_add(1, Ordering::SeqCst);
        async { Response::new(full(quota_body())) }
    })
    .await;
    let (_t, g) = fixture(vec![format!("http://127.0.0.1:{port}/v1")]).await;
    let id = g.view().providers[0].id.clone();
    g.set_quota_interval(3600);
    g.query_quota(&id, false).await.unwrap();
    g.query_quota(&id, false).await.unwrap();
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    g.query_quota(&id, true).await.unwrap();
    assert_eq!(hits.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn quota_retry_after_is_cached_and_concurrent_requests_share_one_flight() {
    let hits = Arc::new(AtomicUsize::new(0));
    let counted = hits.clone();
    let port = server(move |_| {
        let n = counted.fetch_add(1, Ordering::SeqCst);
        async move {
            if n == 0 {
                Response::builder()
                    .status(429)
                    .header("retry-after", "60")
                    .body(full("limited"))
                    .unwrap()
            } else {
                Response::new(full(quota_body()))
            }
        }
    })
    .await;
    let (_t, g) = fixture(vec![format!("http://127.0.0.1:{port}/v1")]).await;
    let id = g.view().providers[0].id.clone();
    let (a, b) = tokio::join!(g.query_quota(&id, true), g.query_quota(&id, true));
    let a = a.unwrap();
    let b = b.unwrap();
    assert_eq!(a.state, "error");
    assert_eq!(b.state, "error");
    assert!(a.retry_at.is_some());
    assert_eq!(hits.load(Ordering::SeqCst), 1);

    let cached = g.query_quota(&id, true).await.unwrap();
    assert_eq!(cached.state, "error");
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn quota_cache_is_invalidated_when_provider_version_changes() {
    let hits = Arc::new(AtomicUsize::new(0));
    let counted = hits.clone();
    let port = server(move |request| {
        let counted = counted.clone();
        async move {
            let _ = request.into_body().collect().await;
            counted.fetch_add(1, Ordering::SeqCst);
            Response::new(full(quota_body()))
        }
    })
    .await;
    let (t, g) = fixture(vec![format!("http://127.0.0.1:{port}/v1")]).await;
    let id = g.view().providers[0].id.clone();
    g.query_quota(&id, true).await.unwrap();
    update(
        &g,
        &t,
        Edit::SaveProvider {
            id: Some(id.clone()),
            base_url: format!("http://127.0.0.1:{port}/v1"),
            token: "changed-fixture-token".into(),
            name: None,
        },
    );
    g.query_quota(&id, false).await.unwrap();
    assert_eq!(hits.load(Ordering::SeqCst), 2);
}
