use super::*;
use crate::usage::{model::*, pricing::Pricing, store::Store};

fn summary() -> Value {
    json!({"response":{"groups":[
        {"buckets":[{"bucketId":"gemini-5h","window":"5h","resetTime":"2026-01-08T05:00:00Z"},{"bucketId":"gemini-weekly","window":"weekly","resetTime":"2026-01-09T00:00:00Z"}]},
        {"buckets":[{"bucketId":"3p-5h","window":"5h","resetTime":"2026-01-08T06:00:00Z"},{"bucketId":"3p-weekly","window":"weekly","resetTime":"2026-01-10T00:00:00Z"}]}
    ]}})
}

#[test]
fn each_group_uses_its_observed_reset_and_half_open_window() {
    let p = parse(&summary());
    assert_eq!(p.len(), 4);
    let gemini = &p["gemini-5h"];
    let third_party = &p["3p-5h"];
    assert_eq!(gemini.end - gemini.start, 5 * 3_600_000);
    assert_eq!(third_party.start - gemini.start, 3_600_000);
    assert_eq!(
        p["gemini-weekly"].end - p["gemini-weekly"].start,
        7 * 86_400_000
    );
    assert_ne!(p["gemini-weekly"], p["3p-weekly"]);
    assert!(gemini.contains(gemini.start));
    assert!(!gemini.contains(gemini.start - 1));
    assert!(!gemini.contains(gemini.end));
    assert!(!gemini.current(gemini.end));
    let mut mismatched = summary();
    mismatched["response"]["groups"][0]["buckets"][0]["window"] = json!("weekly");
    assert!(!parse(&mismatched).contains_key("gemini-5h"));
    assert!(
        parse(&json!({"response":{"buckets":summary()["response"]["groups"][0]["buckets"]}}))
            .is_empty()
    );
}

#[test]
fn account_periods_ignore_history_filters_and_expire_without_extrapolation() {
    let periods = parse(&summary());
    let now = periods["gemini-5h"].start + 2 * 3_600_000;
    for native in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let mut store = if native {
            Store::open(dir.path())
        } else {
            Store::open_v11_fixture(dir.path())
        }
        .unwrap();
        if native {
            while store.compact_storage_batch().unwrap() {}
        }
        let profiles = [
            ("b", "oauth"),
            ("a", "oauth"),
            ("key", "vertex_key"),
            ("adc", "adc"),
        ];
        let entries: Vec<_> = profiles.iter().map(|(id, kind)| json!({"id":id,"name":format!("Fixture {id}"),"kind":kind,"sourceType":"fixture","payload":"{}","updatedAt":0})).collect();
        std::fs::write(
            dir.path().join("antigravity-accounts.json"),
            serde_json::to_vec(&json!({"schema":1,"entries":entries})).unwrap(),
        )
        .unwrap();
        store.save_antigravity_quota("a", &periods).unwrap();
        let writes = store.connection.total_changes();
        store.save_antigravity_quota("a", &periods).unwrap();
        assert_eq!(
            store.connection.total_changes(),
            writes,
            "unchanged boundaries must not write metadata"
        );
        let make = |id: &str, provider: &str, model: &str, at: i64| {
            let mut r = Record {
                id: id.into(),
                client: "antigravity".into(),
                source: "antigravity".into(),
                started_at: at,
                finished_at: Some(at),
                completed: true,
                attempts: vec![Attempt {
                    id: format!("{id}-attempt"),
                    provider: Some(provider.into()),
                    requested_model: Some(model.into()),
                    response_model: Some(model.into()),
                    pricing_model: Some(model.into()),
                    started_at: at,
                    outcome: "success".into(),
                    usage_status: "reported".into(),
                    tokens: Tokens {
                        input: Some(100),
                        output: Some(20),
                        ..Tokens::default()
                    },
                    ..Attempt::default()
                }],
                ..Record::default()
            };
            super::super::normalize_record(&mut r);
            r
        };
        let start = periods["gemini-5h"].start;
        store
            .write_batch(
                &[
                    make("before", "a", "gemini-3-flash", start - 1),
                    make("included", "a", "gemini-3-flash", start),
                    make("current", "a", "gemini-3-flash", now),
                    make("future", "a", "gemini-3-flash", now + 500),
                    make("reset", "a", "gemini-3-flash", periods["gemini-5h"].end),
                    make("claude-before", "a", "claude-opus-4-6", start),
                    make(
                        "claude-included",
                        "a",
                        "claude-opus-4-6",
                        periods["3p-5h"].start,
                    ),
                    make("other-account", "b", "gemini-3-flash", now),
                    make("key-current", "key", "gemini-3-flash", now),
                    make("key-old", "key", "gemini-3-flash", start - 1),
                    make("adc-current", "adc", "gemini-3-flash", now),
                ],
                None,
            )
            .unwrap();
        let prices = Pricing::new(dir.path()).unwrap();
        let filter = Filter {
            start: Some(now),
            end: Some(now + 1000),
            ..Filter::default()
        };
        let stats = store.antigravity_stats(&filter, &prices, "1", now).unwrap();
        assert_eq!(
            stats
                .accounts
                .iter()
                .map(|a| a.id.as_str())
                .collect::<Vec<_>>(),
            vec!["b", "a", "key", "adc"]
        );
        let a = stats.accounts.iter().find(|a| a.id == "a").unwrap();
        assert_eq!(a.total.requests, 0);
        assert_eq!(a.tracks[0].burst_5h.requests, 2);
        assert_eq!(a.tracks[0].burst_5h.tokens, 240);
        assert_eq!(a.tracks[0].weekly_7d.requests, 3);
        assert_eq!(a.tracks[1].burst_5h.requests, 1);
        assert_eq!(a.tracks[1].weekly_7d.requests, 2);
        let b = stats.accounts.iter().find(|a| a.id == "b").unwrap();
        assert!(b
            .tracks
            .iter()
            .all(|t| t.burst_5h.period.is_none() && t.weekly_7d.period.is_none()));
        for kind in ["key", "adc"] {
            let a = stats.accounts.iter().find(|a| a.id == kind).unwrap();
            assert!(a.tracks.is_empty());
            assert_eq!(a.total.requests, 1);
            assert_eq!(a.total.tokens, 120);
        }
        // A previously seen inactive account can use its still-current periods.
        store.save_antigravity_quota("b", &periods).unwrap();
        let other = store.antigravity_stats(&filter, &prices, "1", now).unwrap();
        assert_eq!(
            other.accounts.iter().find(|a| a.id == "b").unwrap().tracks[0]
                .burst_5h
                .requests,
            1
        );
        let expired_at = periods["3p-weekly"].end;
        let expired = store
            .antigravity_stats(&filter, &prices, "1", expired_at)
            .unwrap();
        assert!(expired.accounts.iter().flat_map(|a| &a.tracks).all(|t| t
            .burst_5h
            .period
            .is_none()
            && t.weekly_7d.period.is_none()));
    }
}

#[tokio::test]
async fn quota_requires_the_app_identity_to_match_the_selected_account() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        for method in ["GetUserStatus", "GetUserStatus", "RetrieveUserQuotaSummary"] {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            let len = socket.read(&mut request).await.unwrap();
            assert!(String::from_utf8_lossy(&request[..len]).contains(method));
            let value = if method == "GetUserStatus" {
                json!({"userStatus":{"email":"fixture@example.invalid"}})
            } else {
                summary()
            };
            let body = value.to_string();
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        }
    });
    let api = Api::from_single(
        super::super::loopback_client().unwrap(),
        origin,
        "fixture".into(),
    );
    assert!(api
        .quota_for("another@example.invalid")
        .await
        .unwrap()
        .is_none());
    assert_eq!(
        api.quota_for("FIXTURE@example.invalid")
            .await
            .unwrap()
            .unwrap(),
        parse(&summary())
    );
    server.await.unwrap();
}
