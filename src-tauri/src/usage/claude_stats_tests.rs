use super::{model::*, pricing::Pricing, store::Store};

const AT: i64 = 1_793_900_000_000;

fn request(id: &str, source: &str, model: &str, operation: Operation) -> Record {
    Record {
        id: safe_id(id),
        client: "claude".into(),
        source: source.into(),
        session_id: Some(safe_id("session")),
        started_at: AT,
        finished_at: Some(AT + 1000),
        completed: true,
        attempts: vec![Attempt {
            id: safe_id(&format!("{id}-attempt")),
            provider: (source == "proxy").then(|| "last".into()),
            requested_model: Some(model.into()),
            response_model: Some(model.into()),
            pricing_model: Some(model.into()),
            response_id: Some(format!("response-{id}")),
            started_at: AT,
            duration_ms: 1000,
            status: Some(200),
            outcome: "success".into(),
            operation,
            tokens: Tokens {
                input: Some(100),
                output: Some(20),
                cache_read: Some(80),
                cache_write: Some(0),
                ..Default::default()
            },
            ..Default::default()
        }],
        ..Default::default()
    }
}

#[test]
fn independent_stats_keep_dedup_retries_model_ids_and_filters_aligned() {
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
        let prices = Pricing::new(dir.path()).unwrap();
        let mut gateway = request("gateway", "proxy", "claude-sonnet-4-6", Operation::Model);
        let mut duplicate = gateway.clone();
        duplicate.id = safe_id("duplicate");
        duplicate.source = "claude".into();
        duplicate.attempts[0].provider = None;
        let mut retry = gateway.attempts[0].clone();
        retry.id = safe_id("retry");
        retry.provider = Some("first".into());
        retry.response_id = Some("retry-response".into());
        retry.status = Some(503);
        gateway.attempts.insert(0, retry);
        let local = request(
            "local",
            "claude",
            "claude-sonnet-4-20250514",
            Operation::Model,
        );
        let compression = request(
            "compression",
            "proxy",
            "claude-sonnet-4-6",
            Operation::Compaction,
        );
        let search = request("search", "proxy", "web_search", Operation::WebSearch);
        let mut records = [gateway, duplicate, local, compression, search];
        for record in &mut records {
            for attempt in &mut record.attempts {
                attempt.price = prices
                    .quote(Some("claude-sonnet-4-6"), "2")
                    .calculate(&attempt.tokens, None);
            }
        }
        records[0].attempts[0].price = None;
        store.write_batch(&records, None).unwrap();
        if native {
            while store.compact_storage_batch().unwrap() {}
        }
        let filter = Filter {
            client: Some("claude".into()),
            end: Some(AT + 2000),
            ..Default::default()
        };
        let all = store.claude_stats(&filter, AT + 2000).unwrap();
        assert_eq!(all.requests, 4);
        assert_eq!(all.sessions, 1);
        assert_eq!(all.unpriced_requests, 1);
        assert_eq!(
            all.providers
                .iter()
                .find(|p| p.id == "first")
                .unwrap()
                .unpriced_requests,
            1
        );
        assert_eq!(
            all.providers
                .iter()
                .find(|p| p.id == "last")
                .unwrap()
                .unpriced_requests,
            0
        );
        assert!(!all.token_mix.reasoning_reported);
        assert_eq!(all.token_mix.thinking_share_percent, None);
        assert_eq!(
            all.families
                .iter()
                .filter(|f| f.family.starts_with("claude-sonnet"))
                .count(),
            2
        );
        assert!(all.accounts.is_empty());
        assert_eq!(all.token_mix.cache_savings_usd, None);
        for selected in [
            filter.clone(),
            Filter {
                provider: Some("last".into()),
                ..filter.clone()
            },
            Filter {
                provider: Some("session".into()),
                ..filter.clone()
            },
            Filter {
                provider: Some("first".into()),
                ..filter.clone()
            },
            Filter {
                operation: Some("model".into()),
                ..filter.clone()
            },
            Filter {
                operation: Some("compaction".into()),
                ..filter.clone()
            },
            Filter {
                operation: Some("web_search".into()),
                ..filter.clone()
            },
            Filter {
                session_id: Some(safe_id("session")),
                ..filter.clone()
            },
            Filter {
                start: Some(AT + 2001),
                ..filter.clone()
            },
        ] {
            let dashboard = store.dashboard(&selected).unwrap();
            let stats = store.claude_stats(&selected, AT + 2000).unwrap();
            assert_eq!(stats.requests, dashboard.totals.requests);
            assert_eq!(stats.unpriced_requests, dashboard.totals.unpriced);
            assert_eq!(decimal(&stats.cost_usd), decimal(&dashboard.totals.cost));
            assert_eq!(
                stats.token_mix.total,
                dashboard.totals.tokens.total().unwrap_or(0)
            );
        }
        // Endpoint ownership overrides a stale caller-side client selection.
        assert_eq!(
            store
                .claude_stats(
                    &Filter {
                        client: Some("codex".into()),
                        ..filter
                    },
                    AT + 2000
                )
                .unwrap()
                .requests,
            4
        );
    }
}

#[test]
fn saved_tariff_controls_savings_and_missing_basis_stays_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let prices = Pricing::new(dir.path()).unwrap();
    let mut row = request("saved", "claude", "claude-sonnet-4-6", Operation::Model);
    let attempt = &mut row.attempts[0];
    attempt.price = prices
        .quote(Some("claude-sonnet-4-6"), "2")
        .calculate(&attempt.tokens, Some("fast"));
    attempt.service_tier = Some("fast".into());
    let price = attempt.price.as_ref().unwrap();
    let expected =
        super::pricing::cache_savings(price, attempt.tokens.cache_read.unwrap()).unwrap();
    let cost = price.cost.clone();
    store.write_batch(std::slice::from_ref(&row), None).unwrap();
    let selected = Filter {
        end: Some(AT + 2000),
        ..Default::default()
    };
    let before = store.claude_stats(&selected, AT + 2000).unwrap();
    assert_eq!(
        before
            .token_mix
            .cache_savings_usd
            .as_deref()
            .and_then(decimal),
        Some(expected)
    );
    assert_eq!(before.cost_usd, cost);
    row.attempts[0].price.as_mut().unwrap().basis = None;
    row.attempts[0].price.as_mut().unwrap().rates.clear();
    let missing = tempfile::tempdir().unwrap();
    let mut store = Store::open(missing.path()).unwrap();
    store.write_batch(&[row], None).unwrap();
    let after = store.claude_stats(&selected, AT + 2000).unwrap();
    assert_eq!(after.token_mix.cache_savings_usd, None);
    assert_eq!(after.cost_usd, cost);
    assert_eq!(after.token_mix.total, before.token_mix.total);
}

#[test]
fn cache_savings_uses_applied_rates_for_compacted_retries() {
    use super::pricing::{cache_savings, Quote};
    let quote = Quote {
        model: "fixture-model".into(),
        data: Some(serde_json::json!({
            "input_cost_per_token": "0.01", "input_cost_per_token_above_200k_tokens": "0.02",
            "cache_read_input_token_cost": "0.001", "cache_read_input_token_cost_above_200k_tokens": "0.002",
            "output_cost_per_token": "0.02"
        })),
        version: "fixture".into(),
        source: "fixed".into(),
        multiplier: "2".into(),
    };
    let tokens = Tokens {
        input: Some(100_000),
        output: Some(0),
        cache_read: Some(50_000),
        ..Default::default()
    };
    let price = quote.calculate(&tokens, None).unwrap();
    let mut aggregate = tokens.clone();
    aggregate.add(&tokens);
    assert_eq!(
        cache_savings(&price, aggregate.cache_read.unwrap()),
        decimal("1800")
    );
    let zero_fresh = Tokens {
        input: Some(0),
        ..tokens
    };
    let saved = quote.calculate(&zero_fresh, None).unwrap();
    assert_eq!(cache_savings(&saved, zero_fresh.cache_read.unwrap()), None);
}

#[test]
fn missing_retries_preserve_reported_totals_and_unpriced_providers_count_once() {
    for native in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let mut store = if native {
            Store::open(dir.path())
        } else {
            Store::open_v11_fixture(dir.path())
        }
        .unwrap();
        let mut row = request(
            "missing-retries",
            "proxy",
            "claude-sonnet-4-6",
            Operation::Model,
        );
        let prices = Pricing::new(dir.path()).unwrap();
        row.attempts[0].price = prices
            .quote(Some("claude-sonnet-4-6"), "2")
            .calculate(&row.attempts[0].tokens, None);
        for index in 0..2 {
            let mut retry = row.attempts[0].clone();
            retry.id = safe_id(&format!("empty-{index}"));
            retry.provider = Some("first".into());
            retry.status = Some(503);
            retry.tokens = Tokens::default();
            retry.price = None;
            row.attempts.insert(0, retry);
        }
        store.write_batch(&[row], None).unwrap();
        if native {
            while store.compact_storage_batch().unwrap() {}
        }
        let stats = store.claude_stats(&Filter::default(), AT + 2000).unwrap();
        assert_eq!(stats.token_mix.input, 100);
        assert_eq!(stats.token_mix.cache_write, 0);
        assert!(stats.token_mix.unreported.is_empty());
        assert_eq!(stats.unpriced_requests, 1);
        assert_eq!(stats.top_sessions[0].unpriced_requests, 1);
        assert_eq!(
            stats
                .providers
                .iter()
                .find(|p| p.id == "first")
                .unwrap()
                .unpriced_requests,
            1
        );
        assert_eq!(
            stats
                .providers
                .iter()
                .find(|p| p.id == "last")
                .unwrap()
                .unpriced_requests,
            0
        );
        let mut absent = request("all-missing", "claude", "fixture-model", Operation::Model);
        absent.session_id = Some(safe_id("absent-session"));
        absent.attempts[0].tokens = Tokens::default();
        store.write_batch(&[absent], None).unwrap();
        let stats = store
            .claude_stats(
                &Filter {
                    session_id: Some(safe_id("absent-session")),
                    ..Default::default()
                },
                AT + 2000,
            )
            .unwrap();
        assert_eq!(stats.token_mix.unreported.len(), 4);
    }
}
