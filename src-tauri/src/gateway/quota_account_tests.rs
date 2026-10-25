use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

const ACCOUNT_TOKEN: &str = "fixture-account-access-token";
const ACCOUNT: &str =
    r#"{"success":true,"data":{"quota":750000,"used_quota":250000,"request_count":12}}"#;
const STATUS: &str =
    r#"{"success":true,"data":{"quota_per_unit":500000,"quota_display_type":"USD"}}"#;
const KEY: &str = r#"{"code":true,"data":{"object":"token_usage","unlimited_quota":true,"total_available":100000000,"total_used":100000}}"#;

fn save_account(g: &Gateway, t: &tempfile::TempDir, token: Option<&str>, base: Option<&str>) {
    let p = &g.view().providers[0];
    update(
        g,
        t,
        Edit::SaveProvider {
            search_only: None,
            id: Some(p.id.clone()),
            base_url: base.unwrap_or(&p.base_url).into(),
            token: String::new(),
            quota_access_token: token.map(str::to_owned),
            name: None,
        },
    );
}

#[tokio::test]
async fn account_credentials_do_not_enter_model_queries_or_invalidate_their_cache() {
    let hits = Arc::new(AtomicUsize::new(0));
    let counted = hits.clone();
    let port = server(move |request| {
        counted.fetch_add(1, Ordering::SeqCst);
        async move {
            assert_eq!(request.uri().path(), "/v1/models");
            assert_eq!(
                request.headers().get("authorization").unwrap(),
                "Bearer upstream-fixture-token"
            );
            Response::new(full(r#"{"data":[{"id":"fixture-model"}]}"#))
        }
    })
    .await;
    let (t, g) = fixture(vec![format!("http://127.0.0.1:{port}/v1")]).await;
    save_account(&g, &t, Some(ACCOUNT_TOKEN), None);
    let id = g.view().providers[0].id.clone();
    let models = g.list_models(&id, true).await.unwrap();
    assert_eq!(models.models, ["fixture-model"]);
    save_account(&g, &t, Some("fixture-replacement-account-token"), None);
    let cached = g.list_models(&id, false).await.unwrap();
    assert!(!cached.stale);
    assert_eq!(cached.models, models.models);
    assert_eq!(cached.version, models.version);
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn account_balance_uses_separate_credentials_and_keeps_key_quota_secondary() {
    for unlimited in [false, true] {
        let port = server(move |req| async move {
            let auth = req.headers().get("authorization").and_then(|h| h.to_str().ok());
            let body = match req.uri().path() {
                "/site/api/user/self" => {
                    assert_eq!(auth, Some("Bearer fixture-account-access-token"));
                    ACCOUNT.to_owned()
                }
                "/site/api/status" => { assert_eq!(auth, None); STATUS.to_owned() }
                "/site/api/usage/token/" => {
                    assert_eq!(auth, Some("Bearer upstream-fixture-token"));
                    serde_json::json!({"code":true,"data":{"object":"token_usage","unlimited_quota":unlimited,"total_available":100000,"total_used":50000,"total_granted":150000}}).to_string()
                }
                _ => panic!("unexpected query endpoint"),
            };
            Response::new(full(body))
        }).await;
        let (t, g) = fixture(vec![format!("http://127.0.0.1:{port}/site/v1")]).await;
        save_account(&g, &t, Some(ACCOUNT_TOKEN), None);
        let q = g
            .query_quota(&g.view().providers[0].id, true)
            .await
            .unwrap();
        assert_eq!(q.state, "ok");
        assert_eq!(q.plans.len(), 2);
        assert_eq!(q.plans[0].name, "账户余额");
        assert_eq!(q.plans[0].remaining, Some(1.5));
        assert_eq!(q.plans[0].used, Some(0.5));
        assert_eq!(q.plans[0].unit, "USD");
        assert!(!q.plans[0].unlimited);
        assert_eq!(q.plans[1].name, "Key 额度");
        assert_eq!(q.plans[1].unlimited, unlimited);
        assert_eq!(q.total_usage.unwrap().requests, Some(12.));
    }
}

#[tokio::test]
async fn account_balance_remains_successful_when_optional_key_query_fails() {
    for status in [401, 403, 429, 500] {
        let hits = Arc::new(AtomicUsize::new(0));
        let counted = hits.clone();
        let port = server(move |req| {
            counted.fetch_add(1, Ordering::SeqCst);
            async move {
                match req.uri().path() {
                    "/api/user/self" => Response::new(full(ACCOUNT)),
                    "/api/status" => Response::new(full(STATUS)),
                    "/api/usage/token/" => Response::builder()
                        .status(status)
                        .header("retry-after", "60")
                        .body(full("unavailable"))
                        .unwrap(),
                    _ => panic!("unexpected query endpoint"),
                }
            }
        })
        .await;
        let (t, g) = fixture(vec![format!("http://127.0.0.1:{port}/v1")]).await;
        save_account(&g, &t, Some(ACCOUNT_TOKEN), None);
        let id = g.view().providers[0].id.clone();
        let q = g.query_quota(&id, true).await.unwrap();
        assert_eq!(q.state, "ok");
        assert!(!q.stale);
        assert_eq!(q.plans[0].remaining, Some(1.5));
        assert_eq!(q.plans[0].unit, "USD");
        assert!(q.error.unwrap().contains("Key 配额查询失败"));
        if status == 429 {
            assert!(q.retry_at.is_some());
            g.query_quota(&id, true).await.unwrap();
            assert_eq!(hits.load(Ordering::SeqCst), 3);
        }
    }
}

#[tokio::test]
async fn account_failures_preserve_old_balance_and_honor_backoff() {
    for status in [401, 403, 429, 503] {
        let hits = Arc::new(AtomicUsize::new(0));
        let counted = hits.clone();
        let port = server(move |req| {
            let counted = counted.clone();
            async move {
                match req.uri().path() {
                    "/api/user/self" if counted.fetch_add(1, Ordering::SeqCst) > 0 => {
                        Response::builder()
                            .status(status)
                            .header("retry-after", "60")
                            .body(full(format!("{{\"message\":\"{ACCOUNT_TOKEN}\"}}")))
                            .unwrap()
                    }
                    "/api/user/self" => Response::new(full(ACCOUNT)),
                    "/api/status" => Response::new(full(STATUS)),
                    "/api/usage/token/" => Response::new(full(KEY)),
                    _ => panic!("unexpected query endpoint"),
                }
            }
        })
        .await;
        let (t, g) = fixture(vec![format!("http://127.0.0.1:{port}/v1")]).await;
        save_account(&g, &t, Some(ACCOUNT_TOKEN), None);
        let id = g.view().providers[0].id.clone();
        let first = g.query_quota(&id, true).await.unwrap();
        let failed = g.query_quota(&id, true).await.unwrap();
        assert_eq!(failed.state, "error");
        assert!(failed.stale);
        assert_eq!(failed.success_at, first.success_at);
        assert_eq!(failed.plans[0].remaining, Some(1.5));
        assert!(!serde_json::to_string(&failed)
            .unwrap()
            .contains(ACCOUNT_TOKEN));
        assert!(failed.error.as_ref().unwrap().contains(match status {
            401 => "认证",
            403 => "权限",
            429 => "限流",
            _ => "不可用",
        }));
        if status == 429 {
            g.query_quota(&id, true).await.unwrap();
            assert_eq!(hits.load(Ordering::SeqCst), 2);
        }
    }
}

#[tokio::test]
async fn account_network_failure_keeps_the_last_balance() {
    let port = server(|req| async move {
        Response::new(full(match req.uri().path() {
            "/api/user/self" => ACCOUNT,
            "/api/status" => STATUS,
            "/api/usage/token/" => KEY,
            _ => panic!("unexpected query endpoint"),
        }))
    })
    .await;
    let (t, g) = fixture(vec![format!("http://127.0.0.1:{port}/v1")]).await;
    save_account(&g, &t, Some(ACCOUNT_TOKEN), None);
    let id = g.view().providers[0].id.clone();
    g.query_quota(&id, true).await.unwrap();
    let closed = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let closed_port = closed.local_addr().unwrap().port();
    drop(closed);
    let mut input = g.query_input(&id).unwrap();
    input.base = format!("http://127.0.0.1:{closed_port}/v1");
    let failed = g.0.quota.query(input, true).await.unwrap();
    assert_eq!(failed.state, "error");
    assert!(failed.stale);
    assert_eq!(failed.plans[0].remaining, Some(1.5));
    assert!(failed.error.is_some());
}

#[tokio::test]
async fn replacing_account_credentials_rejects_an_old_inflight_balance() {
    let entered = Arc::new(tokio::sync::Semaphore::new(0));
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let (arrived, finish) = (entered.clone(), release.clone());
    let port = server(move |req| {
        let (arrived, finish) = (arrived.clone(), finish.clone());
        async move {
            let body = match req.uri().path() {
                "/api/user/self" => {
                    arrived.add_permits(1);
                    finish.acquire().await.unwrap().forget();
                    ACCOUNT
                }
                "/api/status" => STATUS,
                "/api/usage/token/" => KEY,
                _ => panic!("unexpected query endpoint"),
            };
            Response::new(full(body))
        }
    })
    .await;
    let (t, g) = fixture(vec![format!("http://127.0.0.1:{port}/v1")]).await;
    save_account(&g, &t, Some(ACCOUNT_TOKEN), None);
    let id = g.view().providers[0].id.clone();
    let querying = g.clone();
    let flight = tokio::spawn(async move { querying.query_quota(&id, true).await });
    tokio::time::timeout(Duration::from_secs(3), entered.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
    save_account(&g, &t, Some("fixture-account-replacement"), None);
    release.add_permits(1);
    let failure = flight.await.unwrap().err().unwrap();
    assert_eq!(failure.code, code::STALE);
    assert!(g.view().providers[0].quota.is_none());
}

#[tokio::test]
async fn account_credentials_persist_clear_and_invalidate_cache_without_leaking_to_views() {
    let port = server(|req| async move {
        Response::new(full(match req.uri().path() {
            "/api/user/self" => ACCOUNT,
            "/api/status" => STATUS,
            "/api/usage/token/" => KEY,
            _ => panic!("unexpected query endpoint"),
        }))
    })
    .await;
    let (t, g) = fixture(vec![format!("http://127.0.0.1:{port}/v1")]).await;
    let id = g.view().providers[0].id.clone();
    assert!(!g.view().providers[0].has_quota_access_token);
    let initial_version = g.view().providers[0].quota_version.clone();
    save_account(&g, &t, Some(ACCOUNT_TOKEN), None);
    let version = g.view().providers[0].quota_version.clone();
    assert_ne!(version, initial_version);
    g.query_quota(&id, true).await.unwrap();
    save_account(&g, &t, None, None);
    assert_eq!(g.view().providers[0].quota_version, version);
    assert!(g.view().providers[0].quota.is_some());
    let (store, _) = Store::load(&g.0.data.join("gateway.json")).unwrap();
    assert_eq!(store.quota_access_tokens[&id], ACCOUNT_TOKEN);
    assert!(!serde_json::to_string(&g.view())
        .unwrap()
        .contains(ACCOUNT_TOKEN));
    save_account(&g, &t, Some("fixture-account-replacement"), None);
    assert_ne!(g.view().providers[0].quota_version, version);
    assert!(g.view().providers[0].quota.is_none());
    save_account(&g, &t, Some(""), None);
    assert!(!g.view().providers[0].has_quota_access_token);
    let (store, _) = Store::load(&g.0.data.join("gateway.json")).unwrap();
    assert!(store.quota_access_tokens.is_empty());
    save_account(&g, &t, Some(ACCOUNT_TOKEN), None);
    save_account(&g, &t, None, Some("https://other.example.invalid/v1"));
    assert!(!g.view().providers[0].has_quota_access_token);
    save_account(&g, &t, Some(ACCOUNT_TOKEN), None);
    update(
        &g,
        &t,
        Edit::EndpointsProvider {
            id: id.clone(),
            base_urls: vec!["https://secondary.example.invalid/v1".into()],
        },
    );
    update(
        &g,
        &t,
        Edit::SwapEndpoint {
            id: id.clone(),
            base_url: "https://secondary.example.invalid/v1".into(),
        },
    );
    assert!(!g.view().providers[0].has_quota_access_token);
    save_account(&g, &t, Some(ACCOUNT_TOKEN), None);
    update(&g, &t, Edit::DeleteProvider { id });
    let (store, _) = Store::load(&g.0.data.join("gateway.json")).unwrap();
    assert!(store.quota_access_tokens.is_empty());
}
