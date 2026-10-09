use super::{model::*, pricing::Pricing, sessions, store::Store};
use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

const DAY: i64 = 86_400_000;
const BASE: i64 = 1_700_000_000_000;
const FIXTURE_CODEX_RESET: &str = include_str!("fixtures/v015/codex-counter-reset.jsonl");
const FIXTURE_CODEX_LANES: &str = include_str!("fixtures/v015/codex-multi-channel.jsonl");
const FIXTURE_CODEX_PARENT: &str = include_str!("fixtures/v015/codex-fork-parent.jsonl");
const FIXTURE_CODEX_FORK: &str = include_str!("fixtures/v015/codex-fork-child.jsonl");
const FIXTURE_CLAUDE_CACHE: &str = include_str!("fixtures/v015/claude-cache-completion.jsonl");
const FIXTURE_CLAUDE_SUBAGENT: &str =
    include_str!("fixtures/v015/claude-subagent-interrupted.jsonl");

fn fixture() -> (tempfile::TempDir, Store, Pricing) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let pricing = Pricing::new(dir.path()).unwrap();
    (dir, store, pricing)
}

fn all_time() -> Filter {
    Filter {
        start: Some(BASE - DAY),
        end: Some(BASE + 100 * DAY),
        ..Filter::default()
    }
}

fn write_fixture(root: &Path, relative: &str, contents: &str) -> PathBuf {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, contents).unwrap();
    path
}

fn sync_fixture(
    store: &mut Store,
    pricing: &Pricing,
    client: &str,
    root: &Path,
    relative: &str,
    contents: &str,
) -> sessions::Report {
    write_fixture(root, relative, contents);
    sessions::sync(store, pricing, &Settings::default(), client, root, false).unwrap()
}

#[allow(clippy::too_many_arguments)]
fn record(
    id: &str,
    client: &str,
    source: &str,
    at: i64,
    provider: Option<&str>,
    model: Option<&str>,
    tokens: Tokens,
    status: Option<u16>,
) -> Record {
    let attempt = Attempt {
        id: format!("{id}-attempt"),
        provider: provider.map(str::to_owned),
        requested_model: model.map(str::to_owned),
        response_model: model.map(str::to_owned),
        pricing_model: model.map(str::to_owned),
        status,
        outcome: if status.is_some_and(|s| (200..300).contains(&s)) {
            "completed"
        } else {
            "failed"
        }
        .into(),
        started_at: at,
        transport: if source == "proxy" { "http" } else { "session" }.into(),
        tokens,
        ..Attempt::default()
    };
    Record {
        id: id.into(),
        client: client.into(),
        source: source.into(),
        started_at: at,
        attempts: vec![attempt],
        completed: true,
        ..Record::default()
    }
}

fn usage(input: u64, output: u64) -> Tokens {
    Tokens {
        input: Some(input),
        output: Some(output),
        ..Tokens::default()
    }
}

fn codex_snapshot(at: i64, total: [u64; 3], last: [u64; 3]) -> String {
    serde_json::json!({
        "type": "event_msg",
        "timestamp": at,
        "payload": {
            "type": "token_count",
            "info": {
                "model": "gpt-fixture",
                "total_token_usage": {
                    "input_tokens": total[0],
                    "cached_input_tokens": total[1],
                    "output_tokens": total[2]
                },
                "last_token_usage": {
                    "input_tokens": last[0],
                    "cached_input_tokens": last[1],
                    "output_tokens": last[2]
                }
            }
        }
    })
    .to_string()
}

fn price_snapshot(cost: &str) -> PriceSnapshot {
    PriceSnapshot {
        version: "fixture-price-v1".into(),
        source: "fixture-catalog".into(),
        model: "fixture-model".into(),
        multiplier: "1.25".into(),
        rates: std::collections::BTreeMap::from([("input".into(), "0.00001".into())]),
        cost: cost.into(),
        basis: Some(serde_json::json!({"input_cost_per_token":"0.00001"})),
    }
}

#[test]
fn retry_usage_stays_with_the_p1_and_p2_attempts() {
    let (_dir, mut store, _pricing) = fixture();
    let mut request = record(
        "fixture-retry",
        "codex",
        "proxy",
        BASE,
        Some("fixture-p1"),
        Some("fixture-model"),
        usage(12, 2),
        Some(503),
    );
    request.attempts.push(Attempt {
        id: "fixture-retry-p2".into(),
        provider: Some("fixture-p2".into()),
        requested_model: Some("fixture-model".into()),
        response_model: Some("fixture-model".into()),
        pricing_model: Some("fixture-model".into()),
        status: Some(200),
        outcome: "completed".into(),
        started_at: BASE + 50,
        transport: "http".into(),
        tokens: usage(18, 3),
        ..Attempt::default()
    });
    store.write_batch(&[request], None).unwrap();

    let dashboard = store.dashboard(&all_time()).unwrap();
    assert_eq!(dashboard.totals.requests, 1);
    assert_eq!(dashboard.totals.attempts, 2);
    assert_eq!(dashboard.totals.tokens.input, Some(30));
    assert_eq!(dashboard.totals.tokens.output, Some(5));

    for (provider, input, output) in [("fixture-p1", 12, 2), ("fixture-p2", 18, 3)] {
        let group = dashboard
            .providers
            .iter()
            .find(|g| g.id == provider)
            .unwrap_or_else(|| panic!("missing provider group {provider}"));
        assert_eq!(group.totals.attempts, 1, "provider={provider}");
        assert_eq!(
            group.totals.tokens.input,
            Some(input),
            "provider={provider}"
        );
        assert_eq!(
            group.totals.tokens.output,
            Some(output),
            "provider={provider}"
        );
    }
}

#[test]
fn retry_pricing_counts_each_logical_request_once_and_keeps_provider_attribution() {
    let (_dir, mut store, _pricing) = fixture();
    let retry = |id: &str,
                 model: &str,
                 first_price: Option<PriceSnapshot>,
                 final_price: Option<PriceSnapshot>| {
        let first_provider = format!("{model}-p1");
        let final_provider = format!("{model}-p2");
        let mut request = record(
            id,
            "codex",
            "proxy",
            BASE,
            Some(&first_provider),
            Some(model),
            usage(12, 2),
            Some(503),
        );
        request.attempts[0].price = first_price;
        request.attempts.push(Attempt {
            id: format!("{id}-attempt-2"),
            provider: Some(final_provider),
            requested_model: Some(model.into()),
            response_model: Some(model.into()),
            pricing_model: Some(model.into()),
            status: Some(200),
            outcome: "completed".into(),
            started_at: BASE + 50,
            transport: "http".into(),
            tokens: usage(18, 3),
            price: final_price,
            ..Attempt::default()
        });
        request
    };

    let rows = [
        retry(
            "fixture-retry-unpriced-earlier",
            "fixture-retry-unpriced-earlier-model",
            None,
            Some(price_snapshot("0.25")),
        ),
        retry(
            "fixture-retry-unpriced-both",
            "fixture-retry-unpriced-both-model",
            None,
            None,
        ),
        retry(
            "fixture-retry-priced-both",
            "fixture-retry-priced-both-model",
            Some(price_snapshot("0.1")),
            Some(price_snapshot("0.2")),
        ),
        retry(
            "fixture-retry-zero-cost",
            "fixture-retry-zero-cost-model",
            Some(price_snapshot("0")),
            Some(price_snapshot("0")),
        ),
    ];
    store.write_batch(&rows, None).unwrap();

    for (model, total_unpriced, p1_unpriced, p2_unpriced) in [
        ("fixture-retry-unpriced-earlier-model", 1, 1, 0),
        ("fixture-retry-unpriced-both-model", 1, 1, 1),
        ("fixture-retry-priced-both-model", 0, 0, 0),
        ("fixture-retry-zero-cost-model", 0, 0, 0),
    ] {
        let dashboard = store
            .dashboard(&Filter {
                model: Some(model.into()),
                ..all_time()
            })
            .unwrap();
        assert_eq!(dashboard.totals.requests, 1, "model={model}");
        assert_eq!(dashboard.totals.attempts, 2, "model={model}");
        assert_eq!(dashboard.totals.unpriced, total_unpriced, "model={model}");
        assert_eq!(dashboard.sources.get("proxy"), Some(&1), "model={model}");

        for (suffix, expected) in [("p1", p1_unpriced), ("p2", p2_unpriced)] {
            let provider = format!("{model}-{suffix}");
            let group = dashboard
                .providers
                .iter()
                .find(|group| group.id == provider)
                .unwrap_or_else(|| panic!("missing provider group {provider}"));
            assert_eq!(group.totals.unpriced, expected, "provider={provider}");
        }
    }
}

#[test]
fn logs_page_and_filter_keep_the_requested_rows_and_order() {
    let (_dir, mut store, _pricing) = fixture();
    let rows: Vec<_> = (0..25)
        .map(|i| {
            record(
                &format!("fixture-request-{i}"),
                "codex",
                "proxy",
                BASE + i * 1000,
                Some(if i % 2 == 0 {
                    "fixture-p1"
                } else {
                    "fixture-p2"
                }),
                Some(if i % 3 == 0 {
                    "fixture-gpt"
                } else {
                    "fixture-claude"
                }),
                usage(10 + i as u64, 1),
                Some(if i % 4 == 0 { 503 } else { 200 }),
            )
        })
        .collect();
    store.write_batch(&rows, None).unwrap();

    let first = store.logs(&Filter::default()).unwrap();
    assert_eq!((first.total, first.page, first.rows.len()), (25, 1, 20));
    assert_eq!(first.rows[0].id, "fixture-request-24");
    let second = store
        .logs(&Filter {
            page: 2,
            ..Filter::default()
        })
        .unwrap();
    assert_eq!((second.total, second.page, second.rows.len()), (25, 2, 5));
    assert_eq!(second.rows[0].id, "fixture-request-4");
    assert_eq!(second.rows[4].id, "fixture-request-0");

    let filtered = store
        .logs(&Filter {
            start: Some(BASE + 10_000),
            end: Some(BASE + 20_000),
            client: Some("codex".into()),
            provider: Some("fixture-p1".into()),
            model: Some("fixture-gpt".into()),
            status: Some("5xx".into()),
            ..Filter::default()
        })
        .unwrap();
    assert_eq!(filtered.total, 1);
    assert_eq!(filtered.rows[0].id, "fixture-request-12");
}

#[test]
fn unique_cross_source_fallback_pairs_within_ten_minutes() {
    let (_dir, mut store, _pricing) = fixture();
    let mut proxy = record(
        "fixture-proxy-unique",
        "codex",
        "proxy",
        BASE,
        Some("fixture-p1"),
        Some("fixture-model"),
        usage(100, 20),
        Some(200),
    );
    let mut session = record(
        "fixture-session-unique",
        "codex",
        "codex",
        BASE + 599_999,
        None,
        Some("fixture-model"),
        usage(100, 20),
        None,
    );
    proxy.attempts[0].response_id = None;
    session.attempts[0].response_id = None;
    store.write_batch(&[proxy, session], None).unwrap();

    let page = store.logs(&Filter::default()).unwrap();
    assert_eq!(page.total, 1);
    let row = &page.rows[0];
    assert_eq!(row.id, "fixture-proxy-unique");
    assert_eq!(row.deduplication, "strict_match");
    assert!(row.merged_sources.contains(&"codex".into()));
    assert!(row.merged_sources.contains(&"proxy".into()));
}

#[test]
fn distinct_response_ids_block_time_based_cross_source_merging() {
    let (_dir, mut store, _pricing) = fixture();
    let mut proxy = record(
        "fixture-proxy-distinct",
        "codex",
        "proxy",
        BASE,
        Some("fixture-p1"),
        Some("fixture-model"),
        usage(100, 20),
        Some(200),
    );
    let mut session = record(
        "fixture-session-distinct",
        "codex",
        "codex",
        BASE + 100,
        None,
        Some("fixture-model"),
        usage(100, 20),
        None,
    );
    proxy.attempts[0].response_id = Some(safe_id("fixture-response-p1"));
    session.attempts[0].response_id = Some(safe_id("fixture-response-p2"));
    store.write_batch(&[proxy, session], None).unwrap();

    assert_eq!(store.logs(&Filter::default()).unwrap().total, 2);
}

#[test]
fn ambiguous_cross_source_candidate_is_reviewable_but_not_effective() {
    let (_dir, mut store, _pricing) = fixture();
    let first = record(
        "fixture-proxy-ambiguous-a",
        "codex",
        "proxy",
        BASE + 100,
        Some("fixture-p1"),
        Some("fixture-model"),
        usage(100, 20),
        Some(200),
    );
    let second = record(
        "fixture-proxy-ambiguous-b",
        "codex",
        "proxy",
        BASE + 200,
        Some("fixture-p1"),
        Some("fixture-model"),
        usage(100, 20),
        Some(200),
    );
    let session = record(
        "fixture-session-ambiguous",
        "codex",
        "codex",
        BASE + 300,
        None,
        Some("fixture-model"),
        usage(100, 20),
        None,
    );
    store.write_batch(&[first, second, session], None).unwrap();

    let ambiguous = store.detail("fixture-session-ambiguous").unwrap();
    assert_eq!(ambiguous.deduplication, "ambiguous");
    let dashboard = store.dashboard(&all_time()).unwrap();
    assert_eq!(dashboard.review_count, 1);
    assert_eq!(dashboard.totals.requests, 2);
    assert_eq!(dashboard.totals.tokens.input, Some(200));
    assert_eq!(dashboard.totals.tokens.output, Some(40));
}

#[test]
fn codex_total_counter_reset_starts_a_new_usage_delta() {
    let (dir, mut store, pricing) = fixture();
    let home = dir.path().join("home");
    let report = sync_fixture(
        &mut store,
        &pricing,
        "codex",
        &home,
        "sessions/reset.jsonl",
        FIXTURE_CODEX_RESET,
    );
    assert_eq!((report.imported, report.errors), (2, 0));

    let totals = store.dashboard(&all_time()).unwrap().totals;
    assert_eq!(totals.requests, 2);
    assert_eq!(totals.tokens.input, Some(450));
    assert_eq!(totals.tokens.cache_read, Some(650));
    assert_eq!(totals.tokens.output, Some(50));
}

#[test]
fn codex_multi_channel_snapshots_ignore_replays_and_keep_new_usage() {
    let (dir, mut store, pricing) = fixture();
    let report = sync_fixture(
        &mut store,
        &pricing,
        "codex",
        &dir.path().join("home"),
        "sessions/channels.jsonl",
        FIXTURE_CODEX_LANES,
    );
    assert_eq!((report.imported, report.errors), (3, 0));
    let totals = store.dashboard(&all_time()).unwrap().totals;
    assert_eq!(totals.requests, 3);
    assert_eq!(totals.tokens.input, Some(180));
    assert_eq!(totals.tokens.cache_read, Some(120));
    assert_eq!(totals.tokens.output, Some(30));
}

#[test]
fn codex_fork_skips_parent_prefix_and_counts_only_new_consumption() {
    let (dir, mut store, pricing) = fixture();
    let home = dir.path().join("home");
    write_fixture(
        &home,
        "sessions/2023/11/14/rollout-2023-11-14T22-13-20-00000000-0000-4000-8000-000000000001.jsonl",
        FIXTURE_CODEX_PARENT,
    );
    write_fixture(
        &home,
        "sessions/2023/11/14/rollout-2023-11-14T22-13-20-00000000-0000-4000-8000-000000000002.jsonl",
        FIXTURE_CODEX_FORK,
    );
    let report = sessions::sync(
        &mut store,
        &pricing,
        &Settings::default(),
        "codex",
        &home,
        false,
    )
    .unwrap();
    assert_eq!((report.imported, report.errors), (3, 0));

    let totals = store.dashboard(&all_time()).unwrap().totals;
    assert_eq!(totals.requests, 3);
    assert_eq!(totals.tokens.input, Some(140));
    assert_eq!(totals.tokens.cache_read, Some(110));
    assert_eq!(totals.tokens.output, Some(30));
}

#[test]
fn codex_fork_filtered_parent_events_use_subsequence_prefix_alignment() {
    let (dir, mut store, pricing) = fixture();
    let home = dir.path().join("home");
    let parent = [
        serde_json::json!({
            "type":"session_meta", "timestamp":BASE,
            "payload":{"id":"fixture-parent-subsequence"}
        })
        .to_string(),
        codex_snapshot(BASE + 100, [100, 50, 10], [100, 50, 10]),
        codex_snapshot(BASE + 200, [200, 100, 20], [100, 50, 10]),
        codex_snapshot(BASE + 300, [300, 150, 30], [100, 50, 10]),
    ]
    .join("\n")
        + "\n";
    let child = [
        serde_json::json!({
            "type":"session_meta", "timestamp":BASE + 350,
            "payload":{
                "id":"fixture-child-subsequence",
                "forked_from_id":"fixture-parent-subsequence"
            }
        })
        .to_string(),
        // Replayed child snapshots can be timestamped after the fork cutoff.
        codex_snapshot(BASE + 500, [100, 50, 10], [100, 50, 10]),
        codex_snapshot(BASE + 600, [300, 150, 30], [100, 50, 10]),
        codex_snapshot(BASE + 700, [450, 220, 45], [150, 70, 15]),
    ]
    .join("\n")
        + "\n";
    write_fixture(&home, "sessions/fixture-parent.jsonl", &parent);
    write_fixture(&home, "sessions/fixture-child.jsonl", &child);

    let report = sessions::sync(
        &mut store,
        &pricing,
        &Settings::default(),
        "codex",
        &home,
        false,
    )
    .unwrap();
    assert_eq!((report.imported, report.errors), (4, 0));

    let totals = store.dashboard(&all_time()).unwrap().totals;
    assert_eq!(totals.requests, 4);
    // The new 150 input includes 70 cached tokens; only 80 are fresh input.
    assert_eq!(totals.tokens.input, Some(230));
    assert_eq!(totals.tokens.cache_read, Some(220));
    assert_eq!(totals.tokens.output, Some(45));
    let child_row = store
        .logs(&Filter::default())
        .unwrap()
        .rows
        .into_iter()
        .find(|row| row.session_id.as_deref() == Some(&safe_id("fixture-child-subsequence")))
        .unwrap();
    assert_eq!(child_row.tokens().input, Some(80));
    assert_eq!(child_row.tokens().cache_read, Some(70));
    assert_eq!(child_row.tokens().output, Some(15));
}

#[test]
fn complete_json_without_newline_is_imported_once_for_codex_and_claude() {
    for client in ["codex", "claude"] {
        let (dir, mut store, pricing) = fixture();
        let home = dir.path().join("home");
        let (relative, contents) = if client == "codex" {
            (
                "sessions/fixture-no-final-newline.jsonl",
                format!(
                    "{}\n{}",
                    serde_json::json!({
                        "type":"session_meta", "timestamp":BASE,
                        "payload":{"id":"fixture-no-final-newline-codex"}
                    }),
                    codex_snapshot(BASE + 1, [40, 20, 5], [40, 20, 5])
                ),
            )
        } else {
            (
                "projects/fixture/no-final-newline.jsonl",
                serde_json::json!({
                    "type":"assistant", "uuid":"fixture-assistant-event",
                    "sessionId":"fixture-no-final-newline-claude", "timestamp":BASE,
                    "message":{
                        "id":"fixture-no-final-newline-message", "type":"message",
                        "model":"claude-fixture", "stop_reason":"end_turn",
                        "usage":{"input_tokens":8,"output_tokens":3,"cache_read_input_tokens":2}
                    }
                })
                .to_string(),
            )
        };
        let path = write_fixture(&home, relative, &contents);

        let first = sessions::sync(
            &mut store,
            &pricing,
            &Settings::default(),
            client,
            &home,
            false,
        )
        .unwrap();
        assert_eq!((first.imported, first.errors), (1, 0), "client={client}");
        assert_eq!(store.logs(&Filter::default()).unwrap().total, 1);

        let mut completed = fs::read(&path).unwrap();
        completed.push(b'\n');
        fs::write(&path, completed).unwrap();
        let appended = sessions::sync(
            &mut store,
            &pricing,
            &Settings::default(),
            client,
            &home,
            false,
        )
        .unwrap();
        assert_eq!(appended.errors, 0, "client={client}");

        let page = store.logs(&Filter::default()).unwrap();
        assert_eq!(page.total, 1, "client={client}");
        let totals = store.dashboard(&all_time()).unwrap().totals;
        assert_eq!(totals.requests, 1, "client={client}");
        if client == "codex" {
            assert_eq!(totals.tokens.input, Some(20));
            assert_eq!(totals.tokens.cache_read, Some(20));
            assert_eq!(totals.tokens.output, Some(5));
        } else {
            assert_eq!(totals.tokens.input, Some(8));
            assert_eq!(totals.tokens.cache_read, Some(2));
            assert_eq!(totals.tokens.output, Some(3));
        }
    }
}

#[test]
fn reader_keeps_a_committed_snapshot_while_writer_transaction_is_open() {
    let (dir, mut store, _pricing) = fixture();
    let initial = record(
        "fixture-reader-initial",
        "codex",
        "proxy",
        BASE,
        Some("fixture-provider"),
        Some("fixture-model"),
        usage(10, 1),
        Some(200),
    );
    store.write_batch(&[initial], None).unwrap();
    drop(store);

    let mut writer = rusqlite::Connection::open(dir.path().join("usage.sqlite")).unwrap();
    let reader = Store::reader(dir.path()).unwrap();
    reader
        .snapshot(|snapshot| {
            assert_eq!(snapshot.logs(&Filter::default()).unwrap().total, 1);
            let second = record(
                "fixture-reader-committed-later",
                "codex",
                "proxy",
                BASE + 1,
                Some("fixture-provider"),
                Some("fixture-model"),
                usage(20, 2),
                Some(200),
            );
            let tx = writer.transaction().unwrap();
            tx.execute(
                "INSERT INTO records(id,client,source,time,body) VALUES(?1,?2,?3,?4,?5)",
                rusqlite::params![
                    second.id,
                    second.client,
                    second.source,
                    second.started_at,
                    serde_json::to_string(&second).unwrap()
                ],
            )
            .unwrap();
            assert_eq!(snapshot.logs(&Filter::default()).unwrap().total, 1);
            tx.commit().unwrap();
            assert_eq!(snapshot.logs(&Filter::default()).unwrap().total, 1);
            Ok(())
        })
        .unwrap();
    assert_eq!(reader.logs(&Filter::default()).unwrap().total, 2);
}

#[test]
fn v2_migration_and_codex_rebuild_preserve_gateway_price_snapshots() {
    let (dir, mut store, _pricing) = fixture();
    let mut gateway = record(
        "fixture-v2-gateway-record",
        "codex",
        "proxy",
        BASE,
        Some("fixture-gateway-provider"),
        Some("fixture-model"),
        usage(25, 4),
        Some(200),
    );
    gateway.attempts[0].price = Some(price_snapshot("0.1234"));
    let mut session = record(
        "fixture-v2-codex-session",
        "codex",
        "codex",
        BASE + 1,
        None,
        Some("fixture-model"),
        usage(25, 4),
        None,
    );
    session.attempts[0].price = Some(price_snapshot("0.5678"));
    store
        .write_batch(&[gateway.clone(), session.clone()], None)
        .unwrap();
    drop(store);

    // Restore the v2 schema before exercising migration to the current version.
    let database = rusqlite::Connection::open(dir.path().join("usage.sqlite")).unwrap();
    database
        .execute_batch("ALTER TABLE receipts DROP COLUMN operation;")
        .unwrap();
    database.pragma_update(None, "user_version", 2).unwrap();
    drop(database);

    let mut migrated = Store::open(dir.path()).unwrap();
    let version: i64 = rusqlite::Connection::open(dir.path().join("usage.sqlite"))
        .unwrap()
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, 6);
    assert_eq!(
        migrated.detail(&gateway.id).unwrap().attempts[0]
            .price
            .as_ref()
            .unwrap()
            .cost,
        "0.1234"
    );
    assert_eq!(
        migrated.detail(&session.id).unwrap().attempts[0]
            .price
            .as_ref()
            .unwrap()
            .cost,
        "0.5678"
    );
    let gateway_after_migration = migrated.detail(&gateway.id).unwrap();
    let session_after_migration = migrated.detail(&session.id).unwrap();
    assert_eq!(
        gateway_after_migration.merged_sources,
        vec!["codex", "proxy"]
    );
    assert_eq!(gateway_after_migration.deduplication, "strict_match");
    assert_eq!(
        session_after_migration.duplicate_of.as_deref(),
        Some(gateway.id.as_str())
    );
    assert_eq!(session_after_migration.deduplication, "strict_match");
    assert_eq!(migrated.logs(&Filter::default()).unwrap().total, 1);

    migrated
        .rebuild("codex", std::slice::from_ref(&session), &[])
        .unwrap();
    let rebuilt_session = migrated.detail(&session.id).unwrap();
    let preserved_gateway = migrated.detail(&gateway.id).unwrap();
    assert_eq!(
        rebuilt_session.attempts[0].price.as_ref().unwrap().cost,
        "0.5678"
    );
    assert_eq!(
        preserved_gateway.attempts[0].price.as_ref().unwrap().cost,
        "0.1234"
    );
    assert_eq!(preserved_gateway.merged_sources, vec!["codex", "proxy"]);
    assert_eq!(preserved_gateway.deduplication, "strict_match");
    assert_eq!(
        rebuilt_session.duplicate_of.as_deref(),
        Some(gateway.id.as_str())
    );
    assert_eq!(rebuilt_session.deduplication, "strict_match");
    assert_eq!(migrated.logs(&Filter::default()).unwrap().total, 1);
}

#[test]
fn claude_same_output_completion_keeps_updated_cache_usage() {
    let (dir, mut store, pricing) = fixture();
    let report = sync_fixture(
        &mut store,
        &pricing,
        "claude",
        &dir.path().join("home"),
        "projects/fixture/main.jsonl",
        FIXTURE_CLAUDE_CACHE,
    );
    assert_eq!((report.imported, report.errors), (1, 0));

    let page = store.logs(&Filter::default()).unwrap();
    assert_eq!(page.total, 1);
    let row = &page.rows[0];
    assert!(row.completed);
    assert_eq!(row.final_attempt().unwrap().tokens.output, Some(8));
    assert_eq!(row.final_attempt().unwrap().tokens.cache_read, Some(60));
    assert_eq!(row.final_attempt().unwrap().tokens.cache_write, Some(25));
}

#[test]
fn claude_same_output_completion_updates_cache_during_incremental_sync() {
    let (dir, mut store, pricing) = fixture();
    let mut lines = FIXTURE_CLAUDE_CACHE.lines();
    let user_line = lines.next().unwrap();
    let partial_line = lines.next().unwrap();
    let completed_line = lines.next().unwrap();
    assert!(lines.next().is_none());

    let home = dir.path().join("home");
    let path = write_fixture(
        &home,
        "projects/fixture/incremental.jsonl",
        &format!("{user_line}\n{partial_line}\n"),
    );
    let first = sessions::sync(
        &mut store,
        &pricing,
        &Settings::default(),
        "claude",
        &home,
        false,
    )
    .unwrap();
    assert_eq!((first.imported, first.errors), (1, 0));

    let partial = store.logs(&Filter::default()).unwrap();
    assert_eq!(partial.total, 1);
    assert!(!partial.rows[0].completed);
    let attempt = partial.rows[0].final_attempt().unwrap();
    assert_eq!(attempt.tokens.output, Some(8));
    assert_eq!(attempt.tokens.cache_read, Some(20));
    assert_eq!(attempt.tokens.cache_write, Some(5));

    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    std::io::Write::write_all(&mut file, format!("{completed_line}\n").as_bytes()).unwrap();
    let second = sessions::sync(
        &mut store,
        &pricing,
        &Settings::default(),
        "claude",
        &home,
        false,
    )
    .unwrap();
    assert_eq!((second.imported, second.errors), (1, 0));

    let completed = store.logs(&Filter::default()).unwrap();
    assert_eq!(completed.total, 1);
    assert!(completed.rows[0].completed);
    let attempt = completed.rows[0].final_attempt().unwrap();
    assert_eq!(attempt.tokens.output, Some(8));
    assert_eq!(attempt.tokens.cache_read, Some(60));
    assert_eq!(attempt.tokens.cache_write, Some(25));
    assert_eq!(store.dashboard(&all_time()).unwrap().totals.requests, 1);
}

#[test]
fn claude_subagent_interruption_still_counts_reported_consumption() {
    let (dir, mut store, pricing) = fixture();
    let report = sync_fixture(
        &mut store,
        &pricing,
        "claude",
        &dir.path().join("home"),
        "projects/fixture/main/subagents/agent-fixture.jsonl",
        FIXTURE_CLAUDE_SUBAGENT,
    );
    assert_eq!((report.imported, report.errors), (1, 0));

    let page = store.logs(&Filter::default()).unwrap();
    assert_eq!(page.total, 1);
    let row = &page.rows[0];
    assert!(!row.completed);
    assert_eq!(row.final_attempt().unwrap().status, None);
    assert_eq!(row.final_attempt().unwrap().tokens.input, Some(100));
    assert_eq!(row.final_attempt().unwrap().tokens.cache_read, Some(40));
    assert_eq!(row.final_attempt().unwrap().tokens.cache_write, Some(20));
    assert_eq!(row.final_attempt().unwrap().tokens.output, Some(4));
    assert_eq!(store.dashboard(&all_time()).unwrap().totals.requests, 1);
}

#[test]
fn dashboard_cache_refreshes_after_writes_and_keeps_filters_isolated() {
    let (_dir, mut store, _pricing) = fixture();
    let initial = [
        record(
            "fixture-cache-codex-a",
            "codex",
            "proxy",
            BASE,
            Some("fixture-provider"),
            Some("fixture-model-a"),
            usage(10, 1),
            Some(200),
        ),
        record(
            "fixture-cache-claude-b",
            "claude",
            "proxy",
            BASE + 1,
            Some("fixture-provider"),
            Some("fixture-model-b"),
            usage(20, 2),
            Some(200),
        ),
    ];
    store.write_batch(&initial, None).unwrap();

    let codex_model_a = Filter {
        start: Some(BASE - DAY),
        end: Some(BASE + 100 * DAY),
        client: Some("codex".into()),
        model: Some("fixture-model-a".into()),
        ..Filter::default()
    };
    let cached_before_write = store.dashboard(&codex_model_a).unwrap();
    assert_eq!(cached_before_write.totals.requests, 1);

    let added = record(
        "fixture-cache-codex-a-added",
        "codex",
        "proxy",
        BASE + 2,
        Some("fixture-provider"),
        Some("fixture-model-a"),
        usage(30, 3),
        Some(200),
    );
    store.write_batch(&[added], None).unwrap();

    let refreshed = store.dashboard(&codex_model_a).unwrap();
    assert_eq!(refreshed.totals.requests, 2);
    assert!(refreshed.data_version > cached_before_write.data_version);
    assert_eq!(
        refreshed.data_version,
        store.logs(&codex_model_a).unwrap().data_version
    );

    let claude_model_a = Filter {
        client: Some("claude".into()),
        ..codex_model_a.clone()
    };
    let codex_model_b = Filter {
        model: Some("fixture-model-b".into()),
        ..codex_model_a.clone()
    };
    let claude_model_b = Filter {
        client: Some("claude".into()),
        model: Some("fixture-model-b".into()),
        ..codex_model_a
    };
    assert_eq!(store.dashboard(&claude_model_a).unwrap().totals.requests, 0);
    assert_eq!(store.dashboard(&codex_model_b).unwrap().totals.requests, 0);
    assert_eq!(store.dashboard(&claude_model_b).unwrap().totals.requests, 1);
}

#[test]
fn dashboard_cache_key_keeps_the_two_day_granularity_boundary() {
    let (_dir, mut store, _pricing) = fixture();
    let row = record(
        "fixture-cache-granularity",
        "codex",
        "proxy",
        BASE + 1,
        Some("fixture-provider"),
        Some("fixture-model"),
        usage(10, 1),
        Some(200),
    );
    store.write_batch(&[row], None).unwrap();

    let hourly = store
        .dashboard(&Filter {
            start: Some(BASE),
            end: Some(BASE + 2 * DAY),
            ..Filter::default()
        })
        .unwrap();
    let daily = store
        .dashboard(&Filter {
            start: Some(BASE),
            end: Some(BASE + 2 * DAY + 1),
            ..Filter::default()
        })
        .unwrap();

    assert_eq!(hourly.totals.requests, 1);
    assert_eq!(daily.totals.requests, 1);
    assert_eq!(hourly.trend_step_ms, 3_600_000);
    assert_eq!(daily.trend_step_ms, DAY);
}

#[test]
#[ignore = "100k-row performance baseline; run explicitly on a representative machine"]
fn hundred_thousand_rows_have_fast_first_page_and_cold_dashboard() {
    let (_dir, mut store, _pricing) = fixture();
    let rows: Vec<_> = (0..100_000)
        .map(|i| {
            let model = format!("fixture-model-{i}");
            record(
                &format!("fixture-perf-{i}"),
                "codex",
                "proxy",
                BASE + i as i64,
                Some("fixture-perf-provider"),
                Some(&model),
                usage(i as u64 + 1, 1),
                Some(200),
            )
        })
        .collect();
    store.write_batch(&rows, None).unwrap();

    let started = Instant::now();
    let page = store.logs(&all_time()).unwrap();
    let page_elapsed = started.elapsed();
    assert_eq!((page.total, page.rows.len()), (100_000, 20));
    assert!(
        page_elapsed < Duration::from_millis(500),
        "first page took {page_elapsed:?}"
    );

    let started = Instant::now();
    let dashboard = store.dashboard(&all_time()).unwrap();
    let dashboard_elapsed = started.elapsed();
    println!(
        "100k usage timings: first page {page_elapsed:?}, cold dashboard {dashboard_elapsed:?}"
    );
    assert_eq!(dashboard.totals.requests, 100_000);
    assert!(
        dashboard_elapsed < Duration::from_secs(2),
        "cold dashboard took {dashboard_elapsed:?}"
    );

    let started = Instant::now();
    let cached_dashboard = store.dashboard(&all_time()).unwrap();
    let cache_elapsed = started.elapsed();
    println!(
        "100k usage timings: first page {page_elapsed:?}, cold dashboard {dashboard_elapsed:?}, cached dashboard {cache_elapsed:?}"
    );
    assert_eq!(cached_dashboard.totals.requests, 100_000);
    assert_eq!(cached_dashboard.data_version, dashboard.data_version);
    assert!(
        cache_elapsed < Duration::from_millis(500),
        "cached dashboard took {cache_elapsed:?}"
    );
}
