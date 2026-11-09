//! End-to-end checks for the v11-to-v12 usage storage migration.
//!
//! These fixtures are synthetic and intentionally use a private temporary
//! directory.  The tests exercise the public Store API; physical v12 tables
//! are not part of the contract.

use super::{model::*, pricing::Pricing, sessions, store::Store};
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::Write,
};

const DAY: i64 = 86_400_000;
const BASE: i64 = 20_000 * DAY;

fn attempt(
    id: &str,
    source: &str,
    at: i64,
    response_id: Option<&str>,
    input: u64,
    output: u64,
) -> Attempt {
    Attempt {
        id: id.into(),
        provider: (source == "proxy").then(|| "fixture-provider".into()),
        requested_model: Some("fixture-model".into()),
        response_model: Some("fixture-model".into()),
        pricing_model: Some("fixture-model".into()),
        response_id: response_id.map(str::to_owned),
        status: (source == "proxy").then_some(200),
        outcome: if source == "proxy" {
            "success"
        } else {
            "completed"
        }
        .into(),
        started_at: at,
        duration_ms: 100,
        first_token_ms: (source == "proxy").then_some(10),
        stream: source == "proxy",
        transport: if source == "proxy" { "http" } else { "session" }.into(),
        tokens: Tokens {
            input: Some(input),
            output: Some(output),
            ..Tokens::default()
        },
        ..Attempt::default()
    }
}

fn record(
    id: &str,
    source: &str,
    at: i64,
    response_id: Option<&str>,
    input: u64,
    output: u64,
) -> Record {
    Record {
        id: id.into(),
        client: "codex".into(),
        source: source.into(),
        started_at: at,
        finished_at: Some(at),
        attempts: vec![attempt(
            &format!("{id}-attempt"),
            source,
            at,
            response_id,
            input,
            output,
        )],
        completed: true,
        ..Record::default()
    }
}

fn priced_record(id: &str, event_at: i64, input: u64, output: u64, cost: &str) -> Record {
    let mut record = record(
        id,
        "proxy",
        event_at,
        Some(&format!("{id}-response")),
        input,
        output,
    );
    record.started_at = event_at - 100;
    record.finished_at = Some(event_at);
    let attempt = &mut record.attempts[0];
    attempt.started_at = event_at - 100;
    attempt.price = Some(PriceSnapshot {
        version: "fixture-v12-price".into(),
        source: "fixture".into(),
        model: "fixture-model".into(),
        multiplier: "1".into(),
        rates: BTreeMap::from([
            ("input".into(), "0.01".into()),
            ("output".into(), "0.02".into()),
        ]),
        cost: cost.into(),
        basis: Some(json!({
            "input_cost_per_token": "0.01",
            "output_cost_per_token": "0.02"
        })),
    });
    attempt.thread_hash = Some(format!("{id}-thread"));
    attempt.upstream_request_hash = Some(format!("{id}-upstream"));
    attempt.routing_evidence = Some(RoutingEvidence::default());
    attempt.transfer = Some(TransferMetadata {
        received_at: Some(event_at - 90),
        sent_at: event_at - 95,
        first_byte_at: Some(event_at - 80),
        terminal_at: Some(event_at),
        ended_at: event_at,
        upstream_bytes: 10,
        downstream_bytes: 20,
        terminal_handed_off: true,
        upstream_eof: true,
        client_request_hash: Some(format!("{id}-client")),
        response_hash: Some(format!("{id}-response-hash")),
        ..TransferMetadata::default()
    });
    record
}

fn local_boundary(at: i64, day_offset: i64) -> i64 {
    use chrono::TimeZone;
    let local = chrono::Local.timestamp_millis_opt(at).single().unwrap();
    let date = local.date_naive() + chrono::Duration::days(day_offset);
    chrono::Local
        .from_local_datetime(&date.and_hms_opt(0, 0, 0).unwrap())
        .earliest()
        .unwrap()
        .timestamp_millis()
}

fn all_time() -> Filter {
    Filter {
        start: Some(BASE - DAY),
        end: Some(BASE + 100 * DAY),
        ..Filter::default()
    }
}

fn page_value(store: &Store, filter: &Filter) -> Value {
    let page = store.logs(filter).unwrap();
    json!({
        "rows": page.rows,
        "total": page.total,
        "page": page.page,
        "detailSince": page.detail_since,
    })
}

fn dashboard_value(store: &Store, filter: &Filter) -> Value {
    let dashboard = store.dashboard(filter).unwrap();
    json!({
        "totals": dashboard.totals,
        "providers": dashboard.providers,
        "models": dashboard.models,
        "precision": dashboard.precision,
        "detailSince": dashboard.detail_since,
        "sources": dashboard.sources,
        "reviewCount": dashboard.review_count,
        "sourceHistoryIncomplete": dashboard.source_history_incomplete,
    })
}

fn views(store: &Store) -> Value {
    let filter = all_time();
    let proxy = Filter {
        source: Some("proxy".into()),
        ..filter.clone()
    };
    let sessions = Filter {
        source: Some("sessions".into()),
        ..filter.clone()
    };
    json!({
        "all": page_value(store, &filter),
        "proxy": page_value(store, &proxy),
        "sessions": page_value(store, &sessions),
        "dashboard": dashboard_value(store, &filter),
        "mergeProxy": store.detail("fixture-v12-merge-proxy").unwrap(),
        "mergeSession": store.detail("fixture-v12-merge-session").unwrap(),
    })
}

fn migrated_empty() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let old = Store::open_v11_fixture(dir.path()).unwrap();
    drop(old);
    let mut store = Store::open(dir.path()).unwrap();
    for _ in 0..100 {
        if !store.compact_storage_batch().unwrap() {
            store.reclaim_space().unwrap();
            return (dir, store);
        }
    }
    panic!("v12 migration did not finish");
}

fn compact_diagnostics_until_done(store: &mut Store) {
    for _ in 0..32 {
        if !store.compact_storage_batch().unwrap() {
            return;
        }
    }
    panic!("v12 diagnostic compaction did not finish");
}

#[test]
fn fresh_database_matches_migrated_schema_and_needs_no_historical_repairs() {
    let dir = tempfile::tempdir().unwrap();
    let mut fresh = Store::open(dir.path()).unwrap();
    let (_old_dir, mut migrated) = migrated_empty();
    let schema = |store: &Store| {
        store.connection.prepare("SELECT type,name,sql FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%' ORDER BY type,name").unwrap()
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?.replace([' ', '\n', '"'], ""))))
            .unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap()
    };
    assert_eq!(schema(&fresh), schema(&migrated));
    assert_eq!(
        fresh
            .connection
            .query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        13
    );
    let metadata = |store: &Store| {
        store
            .connection
            .prepare("SELECT key,value FROM metadata ORDER BY key")
            .unwrap()
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    };
    let before = metadata(&fresh);
    assert!(!fresh.compact_storage_batch().unwrap());
    assert!(!fresh.repair_metadata_batch().unwrap());
    assert!(!fresh.repair_correlations_batch().unwrap());
    fresh.reclaim_space().unwrap();
    assert!(fresh.maintenance_status().is_none());
    assert_eq!(metadata(&fresh), before);
    assert_eq!(
        before,
        vec![
            ("antigravity_fields_v02619".into(), "complete".into()),
            ("billing_success_completed".into(), "done".into()),
            ("correlation_zero_cache".into(), "done".into()),
            ("record_sequence".into(), "0".into()),
            ("storage_v12".into(), "ready".into()),
        ]
    );

    let gateway = priced_record("fixture-fresh", BASE, 100, 20, "1.4");
    let mut session = gateway.clone();
    session.id = "fixture-fresh-session".into();
    session.source = "codex".into();
    session.attempts[0].provider = None;
    for store in [&mut fresh, &mut migrated] {
        store
            .write_batch(&[gateway.clone(), session.clone()], None)
            .unwrap();
    }
    assert_eq!(
        page_value(&fresh, &all_time()),
        page_value(&migrated, &all_time())
    );
    assert_eq!(fresh.logs(&all_time()).unwrap().total, 1);
    assert_eq!(
        serde_json::to_value(fresh.detail(&gateway.id).unwrap()).unwrap(),
        serde_json::to_value(migrated.detail(&gateway.id).unwrap()).unwrap()
    );
    let totals = dashboard_value(&fresh, &all_time());
    for store in [&mut fresh, &mut migrated] {
        store
            .archive_batch(&[gateway.id.clone(), session.id.clone()])
            .unwrap();
    }
    assert_eq!(
        dashboard_value(&fresh, &all_time()),
        dashboard_value(&migrated, &all_time())
    );
    assert_eq!(
        dashboard_value(&fresh, &all_time())["totals"],
        totals["totals"]
    );
}

#[test]
fn existing_metrics_view_upgrades_and_skips_generation_for_zero_output() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let rows: Vec<_> = [Some(0), None, Some(20)]
        .into_iter()
        .enumerate()
        .map(|(i, output)| {
            let mut row = record(
                &format!("fixture-zero-output-{i}"),
                "antigravity",
                BASE,
                None,
                100,
                0,
            );
            row.client = "antigravity".into();
            row.attempts[0].tokens.output = output;
            row.attempts[0].generation_ms = Some(900);
            row
        })
        .collect();
    store.write_batch(&rows, None).unwrap();
    let sql: String = store
        .connection
        .query_row(
            "SELECT sql FROM sqlite_schema WHERE name='usage_metrics'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    // The previous view evaluated generation even when measured output was zero.
    let old = sql.replace(
        "0 measured_outputs,",
        "coalesce(output,0) measured_outputs,",
    );
    store
        .connection
        .execute_batch(&format!("DROP VIEW usage_metrics;{old};"))
        .unwrap();
    drop(store);
    let store = Store::open(dir.path()).unwrap();
    let totals = store.dashboard(&all_time()).unwrap().totals;
    assert_eq!(totals.tokens.output, Some(20));
    assert_eq!(totals.measured_outputs, 20);
    assert_eq!(totals.generation_ms, 2700);
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let capture = calls.clone();
    store
        .connection
        .create_scalar_function(
            "usage_generation_ms",
            2,
            rusqlite::functions::FunctionFlags::SQLITE_UTF8,
            move |_| {
                capture.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Ok(900_i64)
            },
        )
        .unwrap();
    let mut filter = all_time();
    filter.client = Some("antigravity".into());
    let totals = store.dashboard(&filter).unwrap().totals;
    assert_eq!(totals.measured_outputs, 20);
    assert_eq!(totals.generation_ms, 2700);
    assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 3);
}

#[test]
fn diagnostic_templates_are_inserted_only_when_used_and_reused_losslessly() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let tx = store.connection.unchecked_transaction().unwrap();
    let count = || {
        tx.query_row("SELECT count(*) FROM v12_templates", [], |r| {
            r.get::<_, u64>(0)
        })
        .unwrap()
    };
    let scalars = Value::Array(
        (0..150u64)
            .map(|i| json!(i.wrapping_mul(2654435761)))
            .collect(),
    );
    let id = super::storage_v12::save_archive(&tx, &scalars).unwrap();
    assert_eq!(super::storage_v12::load_archive(&tx, id).unwrap(), scalars);
    assert_eq!(count(), 0);
    for offset in [0, 1] {
        let fields = Value::Object(
            (0..150u64)
                .map(|i| {
                    (
                        format!("fixture-field-{i}"),
                        json!(i.wrapping_mul(2654435761) + offset),
                    )
                })
                .collect(),
        );
        let id = super::storage_v12::save_archive(&tx, &fields).unwrap();
        assert_eq!(super::storage_v12::load_archive(&tx, id).unwrap(), fields);
        assert_eq!(count(), 1);
    }
    tx.commit().unwrap();
}

#[test]
fn v12_ready_database_removes_legacy_links_on_open() {
    let (dir, store) = migrated_empty();
    let has_links = |store: &Store| {
        store
            .connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='v12_links')",
                [],
                |row| row.get::<_, bool>(0),
            )
            .unwrap()
    };
    assert!(!has_links(&store));
    store.connection.execute_batch("CREATE TABLE v12_links(owner INTEGER PRIMARY KEY,target BLOB NOT NULL); INSERT INTO v12_links VALUES(1,x'00'); INSERT OR REPLACE INTO metadata VALUES('storage_repack','1');").unwrap();
    drop(store);
    let reopened = Store::open(dir.path()).unwrap();
    assert!(!has_links(&reopened));
}

#[test]
fn v12_migration_preserves_raw_and_merged_views_across_restarts() {
    let dir = tempfile::tempdir().unwrap();
    let mut old = Store::open_v11_fixture(dir.path()).unwrap();
    let proxy = record(
        "fixture-v12-merge-proxy",
        "proxy",
        BASE,
        Some("fixture-v12-shared-response"),
        100,
        20,
    );
    let session = record(
        "fixture-v12-merge-session",
        "codex",
        BASE + 1,
        Some("fixture-v12-shared-response"),
        100,
        20,
    );
    old.write_batch(
        &[session, proxy],
        Some(("fixture-v12-cursor", "cursor-before")),
    )
    .unwrap();
    let before = views(&old);
    drop(old);

    let mut migrated = Store::open(dir.path()).unwrap();
    assert!(migrated.compact_storage_batch().unwrap());
    // A previous version may have left this redundant table in a pending copy.
    migrated.connection.execute_batch("CREATE TABLE v12_links(owner INTEGER PRIMARY KEY,target BLOB NOT NULL); INSERT INTO v12_links VALUES(1,x'00');").unwrap();
    assert_eq!(views(&migrated), before);
    drop(migrated);

    let mut resumed = Store::open(dir.path()).unwrap();
    while resumed.compact_storage_batch().unwrap() {}
    resumed.reclaim_space().unwrap();
    assert_eq!(views(&resumed), before);
    let links: bool = resumed
        .connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='v12_links')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(!links);
    assert_eq!(
        resumed.cursor("fixture-v12-cursor").unwrap().as_deref(),
        Some("cursor-before")
    );
}

#[test]
fn v12_migration_write_during_pending_copy_survives_restart() {
    let dir = tempfile::tempdir().unwrap();
    let mut old = Store::open_v11_fixture(dir.path()).unwrap();
    let first = record(
        "fixture-v12-pending-first",
        "codex",
        BASE,
        Some("fixture-v12-pending-first-response"),
        10,
        2,
    );
    old.write_batch(
        std::slice::from_ref(&first),
        Some(("fixture-v12-pending-cursor", "before")),
    )
    .unwrap();
    drop(old);

    let mut pending = Store::open(dir.path()).unwrap();
    let _ = pending.compact_storage_batch().unwrap();
    let late = record(
        "fixture-v12-pending-late",
        "codex",
        BASE + 1,
        Some("fixture-v12-pending-late-response"),
        20,
        3,
    );
    pending
        .write_batch(
            std::slice::from_ref(&late),
            Some(("fixture-v12-pending-cursor", "after")),
        )
        .unwrap();
    drop(pending);

    let mut resumed = Store::open(dir.path()).unwrap();
    while resumed.compact_storage_batch().unwrap() {}
    resumed.reclaim_space().unwrap();
    assert!(resumed.detail(&first.id).is_ok());
    assert!(resumed.detail(&late.id).is_ok());
    assert_eq!(resumed.logs(&all_time()).unwrap().total, 2);
    assert_eq!(
        resumed
            .cursor("fixture-v12-pending-cursor")
            .unwrap()
            .as_deref(),
        Some("after")
    );
}

#[test]
fn v12_low_space_defers_copy_but_keeps_old_writes_and_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let mut old = Store::open_v11_fixture(dir.path()).unwrap();
    let first = record(
        "fixture-v12-low-space-first",
        "codex",
        BASE,
        Some("fixture-v12-low-space-first-response"),
        10,
        2,
    );
    old.write_batch(
        std::slice::from_ref(&first),
        Some(("fixture-v12-low-space-cursor", "before")),
    )
    .unwrap();
    drop(old);

    let mut pending = Store::open(dir.path()).unwrap();
    let database_path = dir.path().join("usage.sqlite");
    let database = Connection::open(&database_path).unwrap();
    database
        .execute(
            "DELETE FROM metadata WHERE key IN ('repair_v020_after','finish_time_after','storage_records_after','storage_daily_after','storage_repack')",
            [],
        )
        .unwrap();
    database
        .execute(
            "INSERT OR REPLACE INTO metadata(key,value) VALUES('fixture_available_bytes','0')",
            [],
        )
        .unwrap();
    drop(database);

    assert!(pending.compact_storage_batch().is_err());
    assert_eq!(pending.logs(&all_time()).unwrap().total, 1);
    assert_eq!(
        pending
            .cursor("fixture-v12-low-space-cursor")
            .unwrap()
            .as_deref(),
        Some("before")
    );

    let late = record(
        "fixture-v12-low-space-late",
        "codex",
        BASE + 1,
        Some("fixture-v12-low-space-late-response"),
        20,
        3,
    );
    pending
        .write_batch(
            std::slice::from_ref(&late),
            Some(("fixture-v12-low-space-cursor", "after")),
        )
        .unwrap();
    drop(pending);

    let database = Connection::open(&database_path).unwrap();
    database
        .execute(
            "DELETE FROM metadata WHERE key='fixture_available_bytes'",
            [],
        )
        .unwrap();
    drop(database);

    let mut resumed = Store::open(dir.path()).unwrap();
    while resumed.compact_storage_batch().unwrap() {}
    resumed.reclaim_space().unwrap();
    assert!(resumed.detail(&first.id).is_ok());
    assert!(resumed.detail(&late.id).is_ok());
    assert_eq!(resumed.logs(&all_time()).unwrap().total, 2);
    assert_eq!(
        resumed
            .cursor("fixture-v12-low-space-cursor")
            .unwrap()
            .as_deref(),
        Some("after")
    );
}

#[test]
fn v12_migration_preserves_zero_tokens_and_price_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let mut old = Store::open_v11_fixture(dir.path()).unwrap();
    let priced = priced_record("fixture-v12-priced-zero", BASE, 0, 0, "0.1234");
    old.write_batch(std::slice::from_ref(&priced), None)
        .unwrap();
    let before = serde_json::to_value(old.detail(&priced.id).unwrap()).unwrap();
    drop(old);

    let mut migrated = Store::open(dir.path()).unwrap();
    while migrated.compact_storage_batch().unwrap() {}
    migrated.reclaim_space().unwrap();
    let after = serde_json::to_value(migrated.detail(&priced.id).unwrap()).unwrap();
    assert_eq!(after, before);
}

#[test]
fn v12_snapshot_stays_on_one_committed_view_while_writer_switches_rows() {
    let (dir, mut store) = migrated_empty();
    let initial = record(
        "fixture-v12-snapshot-initial",
        "codex",
        BASE,
        Some("fixture-v12-snapshot-initial-response"),
        10,
        2,
    );
    store
        .write_batch(std::slice::from_ref(&initial), None)
        .unwrap();
    drop(store);

    let reader = Store::reader(dir.path()).unwrap();
    let mut writer = Store::open(dir.path()).unwrap();
    reader
        .snapshot(|snapshot| {
            assert_eq!(snapshot.logs(&all_time()).unwrap().total, 1);
            let next = record(
                "fixture-v12-snapshot-next",
                "codex",
                BASE + 1,
                Some("fixture-v12-snapshot-next-response"),
                20,
                3,
            );
            writer.write_batch(std::slice::from_ref(&next), None)?;
            assert_eq!(snapshot.logs(&all_time()).unwrap().total, 1);
            Ok(())
        })
        .unwrap();
    assert_eq!(reader.logs(&all_time()).unwrap().total, 2);
}

#[test]
fn v12_migration_switch_keeps_old_reader_snapshot_until_commit() {
    let dir = tempfile::tempdir().unwrap();
    let mut old = Store::open_v11_fixture(dir.path()).unwrap();
    let row = record(
        "fixture-v12-switch-snapshot",
        "codex",
        BASE,
        Some("fixture-v12-switch-response"),
        10,
        2,
    );
    old.write_batch(std::slice::from_ref(&row), None).unwrap();
    let expected = serde_json::to_value(old.detail(&row.id).unwrap()).unwrap();
    drop(old);

    let mut writer = Store::open(dir.path()).unwrap();
    let reader = Store::reader(dir.path()).unwrap();
    reader
        .snapshot(|snapshot| {
            assert_eq!(snapshot.logs(&all_time()).unwrap().total, 1);
            assert_eq!(
                serde_json::to_value(snapshot.detail(&row.id).unwrap()).unwrap(),
                expected
            );
            for _ in 0..100 {
                if !writer.compact_storage_batch()? {
                    break;
                }
            }
            assert_eq!(snapshot.logs(&all_time()).unwrap().total, 1);
            assert_eq!(
                serde_json::to_value(snapshot.detail(&row.id).unwrap()).unwrap(),
                expected
            );
            Ok(())
        })
        .unwrap();
    assert_eq!(reader.logs(&all_time()).unwrap().total, 1);
    assert_eq!(
        serde_json::to_value(reader.detail(&row.id).unwrap()).unwrap(),
        expected
    );
}

#[test]
fn v12_batch_restart_preserves_queries_past_one_migration_batch() {
    let dir = tempfile::tempdir().unwrap();
    let mut old = Store::open_v11_fixture(dir.path()).unwrap();
    let rows = (0..2_001)
        .map(|index| {
            let response = format!("fixture-v12-response-{index}");
            record(
                &format!("fixture-v12-batch-{index}"),
                if index % 2 == 0 { "proxy" } else { "codex" },
                BASE + index as i64,
                Some(response.as_str()),
                10 + index,
                2,
            )
        })
        .collect::<Vec<_>>();
    old.write_batch(&rows, None).unwrap();
    let filter = Filter {
        start: Some(BASE - 1),
        end: Some(BASE + 2_001),
        ..Filter::default()
    };
    let before = json!({
        "total": old.logs(&filter).unwrap().total,
        "dashboard": dashboard_value(&old, &filter),
    });
    drop(old);

    let mut migrating = Store::open(dir.path()).unwrap();
    assert!(migrating.compact_storage_batch().unwrap());
    assert_eq!(
        json!({
            "total": migrating.logs(&filter).unwrap().total,
            "dashboard": dashboard_value(&migrating, &filter),
        }),
        before
    );
    drop(migrating);

    let mut resumed = Store::open(dir.path()).unwrap();
    while resumed.compact_storage_batch().unwrap() {}
    resumed.reclaim_space().unwrap();
    assert_eq!(
        json!({
            "total": resumed.logs(&filter).unwrap().total,
            "dashboard": dashboard_value(&resumed, &filter),
        }),
        before
    );
}

#[test]
fn v12_corrupt_old_codec_aborts_batch_and_keeps_cursor_and_body() {
    let dir = tempfile::tempdir().unwrap();
    let mut old = Store::open_v11_fixture(dir.path()).unwrap();
    let row = record(
        "fixture-v12-corrupt",
        "codex",
        BASE,
        Some("fixture-v12-corrupt-response"),
        10,
        2,
    );
    old.write_batch(
        std::slice::from_ref(&row),
        Some(("fixture-v12-corrupt-cursor", "cursor-before")),
    )
    .unwrap();
    drop(old);

    let database_path = dir.path().join("usage.sqlite");
    let database = Connection::open(&database_path).unwrap();
    database
        .execute(
            "UPDATE records SET body=?1 WHERE id=?2",
            params![
                r#"{"_usageCodec":"fixture-invalid","_usageBody":"x"}"#,
                row.id
            ],
        )
        .unwrap();
    drop(database);

    let mut migrating = Store::open(dir.path()).unwrap();
    assert!(migrating.compact_storage_batch().is_err());
    assert_eq!(
        migrating
            .cursor("fixture-v12-corrupt-cursor")
            .unwrap()
            .as_deref(),
        Some("cursor-before")
    );
    let database = Connection::open(database_path).unwrap();
    let body: String = database
        .query_row(
            "SELECT body FROM records WHERE id=?1",
            [row.id.as_str()],
            |result| result.get(0),
        )
        .unwrap();
    assert_eq!(
        body,
        r#"{"_usageCodec":"fixture-invalid","_usageBody":"x"}"#
    );
}

#[test]
fn v12_zero_and_ambiguous_merges_are_reversible_by_source_rebuild() {
    let (_dir, mut store) = migrated_empty();
    let zero_proxy = record(
        "fixture-v12-zero-proxy",
        "proxy",
        BASE,
        Some("fixture-v12-zero-response"),
        0,
        0,
    );
    let zero_session = record(
        "fixture-v12-zero-session",
        "codex",
        BASE + 1,
        Some("fixture-v12-zero-response"),
        100,
        7,
    );
    let ambiguous_a = record("fixture-v12-ambiguous-a", "proxy", BASE + 2, None, 20, 3);
    let ambiguous_b = record("fixture-v12-ambiguous-b", "proxy", BASE + 3, None, 20, 3);
    let ambiguous_session = record(
        "fixture-v12-ambiguous-session",
        "codex",
        BASE + 4,
        None,
        20,
        3,
    );
    store
        .write_batch(
            &[
                zero_proxy,
                zero_session,
                ambiguous_a,
                ambiguous_b,
                ambiguous_session,
            ],
            None,
        )
        .unwrap();

    assert_eq!(
        store
            .detail("fixture-v12-zero-proxy")
            .unwrap()
            .final_attempt()
            .unwrap()
            .tokens
            .input,
        Some(0)
    );
    assert_eq!(
        store
            .detail("fixture-v12-ambiguous-session")
            .unwrap()
            .deduplication,
        "ambiguous"
    );
    assert_eq!(store.dashboard(&all_time()).unwrap().review_count, 1);

    store.rebuild("codex", &[], &[]).unwrap();
    assert!(store.detail("fixture-v12-zero-session").is_err());
    assert!(store.detail("fixture-v12-ambiguous-session").is_err());
    assert_eq!(
        store
            .detail("fixture-v12-zero-proxy")
            .unwrap()
            .final_attempt()
            .unwrap()
            .tokens
            .input,
        Some(0)
    );
    assert_eq!(store.dashboard(&all_time()).unwrap().review_count, 0);
}

#[test]
fn v12_donor_cache_does_not_recompute_reported_input_or_gateway_view() {
    let (_dir, mut store) = migrated_empty();
    let response = "fixture-v12-cache-donor-response";
    let mut gateway = record(
        "fixture-v12-cache-original",
        "proxy",
        BASE,
        Some(response),
        10,
        2,
    );
    gateway.attempts[0].tokens.inclusive_input = Some(10);
    gateway.attempts[0].tokens.cache_read = None;
    gateway.attempts[0].tokens.cache_write = None;
    gateway.gateway_reported = gateway.final_attempt().cloned();
    let original_gateway = gateway.gateway_reported.clone();

    let mut donor = record(
        "fixture-v12-cache-donor",
        "codex",
        BASE + 1,
        Some(response),
        10,
        7,
    );
    donor.attempts[0].tokens.cache_read = Some(3);
    donor.attempts[0].tokens.cache_write = Some(0);

    store.write_batch(&[gateway.clone(), donor], None).unwrap();
    let merged = store.detail(&gateway.id).unwrap();
    let merged_attempt = merged.final_attempt().unwrap();
    assert_eq!(merged_attempt.tokens.input, Some(10));
    assert_eq!(merged_attempt.tokens.inclusive_input, Some(10));
    assert_eq!(merged_attempt.tokens.cache_read, Some(3));
    assert_eq!(merged_attempt.tokens.output, Some(2));
    assert_eq!(
        serde_json::to_value(merged.gateway_reported).unwrap(),
        serde_json::to_value(original_gateway).unwrap()
    );
}

#[test]
fn v12_filter_and_compaction_keep_seven_and_thirty_day_boundaries_inclusive() {
    let (_dir, mut store) = migrated_empty();
    let at = BASE + 80 * DAY + 1234;
    let cutoff_30 = local_boundary(at - 30 * DAY, 0);
    let cutoff_7 = local_boundary(at - 7 * DAY, 0);
    let rows = [
        record(
            "fixture-v12-before-30",
            "proxy",
            cutoff_30 - 1,
            Some("fixture-v12-before-30-response"),
            1,
            1,
        ),
        record(
            "fixture-v12-at-30",
            "proxy",
            cutoff_30,
            Some("fixture-v12-at-30-response"),
            2,
            1,
        ),
        record(
            "fixture-v12-at-7",
            "proxy",
            cutoff_7,
            Some("fixture-v12-at-7-response"),
            3,
            1,
        ),
        record(
            "fixture-v12-after-7",
            "proxy",
            cutoff_7 + 1,
            Some("fixture-v12-after-7-response"),
            4,
            1,
        ),
    ];
    store.write_batch(&rows, None).unwrap();

    let window = Filter {
        start: Some(cutoff_30),
        end: Some(cutoff_7),
        ..Filter::default()
    };
    let page = store.logs(&window).unwrap();
    assert_eq!(page.total, 2);
    assert_eq!(
        page.rows
            .iter()
            .map(|row| row.id.as_str())
            .collect::<Vec<_>>(),
        vec!["fixture-v12-at-7", "fixture-v12-at-30"]
    );
    assert_eq!(
        store
            .logs(&Filter {
                start: Some(cutoff_30),
                end: Some(cutoff_30),
                ..Filter::default()
            })
            .unwrap()
            .total,
        1
    );
    assert_eq!(
        store
            .logs(&Filter {
                start: Some(cutoff_7),
                end: Some(cutoff_7),
                ..Filter::default()
            })
            .unwrap()
            .total,
        1
    );

    store.compact(at).unwrap();
    assert_eq!(store.detail_since().unwrap(), cutoff_30);
    assert!(store.detail("fixture-v12-before-30").is_err());
    assert!(store.detail("fixture-v12-at-30").is_ok());
    assert!(store.detail("fixture-v12-at-7").is_ok());
    assert_eq!(store.dashboard(&all_time()).unwrap().totals.requests, 4);
}

#[test]
fn v12_seven_day_cold_archive_preserves_zero_tokens_and_price_snapshot() {
    let (_dir, mut store) = migrated_empty();
    let at = BASE + 80 * DAY;
    let cold = priced_record("fixture-v12-cold-zero", at - 8 * DAY, 0, 0, "0.1234");
    let hot = priced_record("fixture-v12-hot", at - 6 * DAY, 12, 3, "0.5678");
    store
        .write_batch(&[cold.clone(), hot.clone()], None)
        .unwrap();
    let cold_before = store.detail(&cold.id).unwrap();
    let hot_before = store.detail(&hot.id).unwrap();

    let before_version = super::query::version(&store.connection).unwrap();
    store.compact(at).unwrap();

    let cold_after = store.detail(&cold.id).unwrap();
    assert!(cold_after.detail_archived);
    assert_eq!(cold_after.tokens(), cold_before.tokens());
    assert_eq!(
        serde_json::to_value(&cold_after.attempts[0].price).unwrap(),
        serde_json::to_value(&cold_before.attempts[0].price).unwrap()
    );
    assert!(cold_after.attempts[0].thread_hash.is_none());
    assert!(cold_after.attempts[0].upstream_request_hash.is_none());
    let transfer = cold_after.attempts[0].transfer.as_ref().unwrap();
    assert!(transfer.client_request_hash.is_none());
    assert!(transfer.response_hash.is_none());
    assert!(!hot_before.detail_archived);

    let hot_after = store.detail(&hot.id).unwrap();
    assert!(!hot_after.detail_archived);
    assert_eq!(hot_after.tokens(), hot_before.tokens());
    assert_eq!(
        hot_after.attempts[0].thread_hash,
        hot_before.attempts[0].thread_hash
    );
    assert_eq!(
        serde_json::to_value(&hot_after.attempts[0].price).unwrap(),
        serde_json::to_value(&hot_before.attempts[0].price).unwrap()
    );
    let changed_version = super::query::version(&store.connection).unwrap();
    assert!(changed_version > before_version);
    // Advance/reset the cursor, then scan unchanged records again.
    for _ in 0..3 {
        super::storage_v12::cold_batch(&mut store.connection, at).unwrap();
        assert_eq!(
            super::query::version(&store.connection).unwrap(),
            changed_version
        );
    }
}

#[test]
fn v12_cold_archive_rejects_incomplete_transfer_states() {
    let (_dir, mut store) = migrated_empty();
    let at = BASE + 80 * DAY;
    let mut terminal_failed = priced_record(
        "fixture-v12-cold-terminal-status",
        at - 8 * DAY,
        10,
        2,
        "0.1",
    );
    terminal_failed.attempts[0]
        .transfer
        .as_mut()
        .unwrap()
        .terminal_status = Some("failed".into());

    let mut failed_event = priced_record(
        "fixture-v12-cold-terminal-event",
        at - 8 * DAY,
        10,
        2,
        "0.1",
    );
    failed_event.attempts[0]
        .transfer
        .as_mut()
        .unwrap()
        .terminal_event = Some("response.failed".into());

    let mut disconnected =
        priced_record("fixture-v12-cold-disconnected", at - 8 * DAY, 10, 2, "0.1");
    disconnected.attempts[0]
        .transfer
        .as_mut()
        .unwrap()
        .upstream_end = Some("DISCONNECTED".into());

    let mut not_handed_off = priced_record(
        "fixture-v12-cold-not-handed-off",
        at - 8 * DAY,
        10,
        2,
        "0.1",
    );
    not_handed_off.attempts[0]
        .transfer
        .as_mut()
        .unwrap()
        .terminal_handed_off = false;

    let mut abnormal_ws = priced_record("fixture-v12-cold-abnormal-ws", at - 8 * DAY, 10, 2, "0.1");
    abnormal_ws.attempts[0]
        .transfer
        .as_mut()
        .unwrap()
        .ws_close_code = Some(1006);

    let mut unpriced = priced_record("fixture-v12-cold-unpriced", at - 8 * DAY, 10, 2, "0.1");
    unpriced.attempts[0].price = None;

    let mut failed_attempt = priced_record(
        "fixture-v12-cold-failed-attempt",
        at - 8 * DAY,
        10,
        2,
        "0.1",
    );
    let mut retry = failed_attempt.attempts[0].clone();
    retry.id = "fixture-v12-cold-failed-attempt-retry".into();
    retry.status = Some(500);
    retry.outcome = "failed".into();
    failed_attempt.attempts.push(retry);

    let mut normal = priced_record("fixture-v12-cold-normal", at - 8 * DAY, 10, 2, "0.1");
    normal.attempts[0].transfer.as_mut().unwrap().ws_close_code = Some(1000);
    let recent = priced_record("fixture-v12-cold-recent", at - 6 * DAY, 10, 2, "0.1");
    store
        .write_batch(
            &[
                terminal_failed.clone(),
                failed_event.clone(),
                disconnected.clone(),
                not_handed_off.clone(),
                abnormal_ws.clone(),
                unpriced.clone(),
                failed_attempt.clone(),
                normal.clone(),
                recent.clone(),
            ],
            None,
        )
        .unwrap();

    store.compact(at).unwrap();

    for row in [
        terminal_failed,
        failed_event,
        disconnected,
        not_handed_off,
        abnormal_ws,
        unpriced,
        failed_attempt,
    ] {
        assert!(
            !store.detail(&row.id).unwrap().detail_archived,
            "{} must retain diagnostic detail",
            row.id
        );
    }
    assert!(store.detail(&normal.id).unwrap().detail_archived);
    assert!(!store.detail(&recent.id).unwrap().detail_archived);
}

#[test]
fn v12_short_hint_collisions_keep_full_record_and_entity_ids_distinct() {
    const FIRST: &str = "1975542fda9b754583df5cae86a69cdf9a1af17c55c9121e6194312e27659f6c";
    const SECOND: &str = "678b2eea8ee668c10b705efa4dce68ddde3011f715ad4070b31612700832e674";
    let (dir, mut store) = migrated_empty();

    let mut first = record(FIRST, "proxy", BASE, Some(FIRST), 10, 2);
    first.session_id = Some(FIRST.into());
    let mut second = record(SECOND, "proxy", BASE + 1, Some(SECOND), 20, 3);
    second.session_id = Some(SECOND.into());
    store
        .write_batch(&[first.clone(), second.clone()], None)
        .unwrap();

    let first_before = store.detail(FIRST).unwrap();
    let second_before = store.detail(SECOND).unwrap();
    assert_eq!(first_before.id, FIRST);
    assert_eq!(second_before.id, SECOND);
    assert_eq!(
        first_before.final_attempt().unwrap().response_id.as_deref(),
        Some(FIRST)
    );
    assert_eq!(
        second_before
            .final_attempt()
            .unwrap()
            .response_id
            .as_deref(),
        Some(SECOND)
    );
    assert_eq!(first_before.session_id.as_deref(), Some(FIRST));
    assert_eq!(second_before.session_id.as_deref(), Some(SECOND));

    let mut updated_first = first_before;
    updated_first.attempts[0].tokens.input = Some(777);
    updated_first.attempts[0].tokens.output = Some(9);
    store.write_batch(&[updated_first], None).unwrap();
    let first_after = store.detail(FIRST).unwrap();
    let second_after = store.detail(SECOND).unwrap();
    assert_eq!(first_after.attempts[0].tokens.input, Some(777));
    assert_eq!(first_after.attempts[0].tokens.output, Some(9));
    assert_eq!(
        second_after.attempts[0].tokens.input,
        second.attempts[0].tokens.input
    );
    assert_eq!(
        second_after.attempts[0].tokens.output,
        second.attempts[0].tokens.output
    );
    assert_eq!(store.logs(&all_time()).unwrap().total, 2);

    drop(store);
    let database = Connection::open(dir.path().join("usage.sqlite")).unwrap();
    super::storage_v12::register(&database).unwrap();
    let first_hint: i64 = database
        .query_row("SELECT usage_hint(usage_key(?1))", [FIRST], |row| {
            row.get(0)
        })
        .unwrap();
    let second_hint: i64 = database
        .query_row("SELECT usage_hint(usage_key(?1))", [SECOND], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(first_hint, i64::from(4_155_425_621_u32 & 0x7fff));
    assert_eq!(second_hint, first_hint);
    let records: i64 = database
        .query_row(
            "SELECT count(*) FROM v12_records WHERE usage_hint(key)=?1",
            [first_hint],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(records, 2);
    let responses: i64 = database
        .query_row(
            "SELECT count(*) FROM v12_records WHERE usage_hint(response)=?1",
            [first_hint],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(responses, 2);
    for id in [FIRST, SECOND] {
        let exact: i64 = database
            .query_row(
                "SELECT count(*) FROM v12_records WHERE response=usage_key(?1)",
                [id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            exact, 1,
            "response {id} must retain its complete identifier"
        );
    }
}

#[test]
fn v12_source_rebuild_invalidates_missing_file_cursor_without_touching_other_source() {
    let (dir, mut store) = migrated_empty();
    let pricing = Pricing::new(dir.path()).unwrap();
    let home = dir.path().join("fixture-v12-cursor-home");
    let codex_path = home.join("sessions").join("001-codex.jsonl");
    let claude_path = home.join("projects").join("fixture").join("claude.jsonl");
    fs::create_dir_all(codex_path.parent().unwrap()).unwrap();
    fs::create_dir_all(claude_path.parent().unwrap()).unwrap();
    let write_jsonl = |path: &std::path::Path, rows: &[Value]| {
        let mut file = File::create(path).unwrap();
        for row in rows {
            serde_json::to_writer(&mut file, row).unwrap();
            file.write_all(b"\n").unwrap();
        }
    };
    write_jsonl(
        &codex_path,
        &[
            json!({
                "type": "session_meta",
                "timestamp": BASE,
                "payload": {"id": "fixture-v12-cursor-codex"}
            }),
            json!({
                "type": "event_msg",
                "timestamp": BASE + 1,
                "payload": {
                    "type": "token_count",
                    "info": {
                        "model": "fixture-model",
                        "total_token_usage": {
                            "input_tokens": 10,
                            "cached_input_tokens": 0,
                            "output_tokens": 2
                        }
                    }
                }
            }),
        ],
    );
    write_jsonl(
        &claude_path,
        &[
            json!({
                "type": "user",
                "sessionId": "fixture-v12-cursor-claude",
                "uuid": "fixture-v12-cursor-claude-user",
                "timestamp": BASE + 10,
                "message": {"content": "fixture.example.invalid"}
            }),
            json!({
                "type": "assistant",
                "sessionId": "fixture-v12-cursor-claude",
                "uuid": "fixture-v12-cursor-claude-assistant",
                "parentUuid": "fixture-v12-cursor-claude-user",
                "timestamp": BASE + 11,
                "message": {
                    "id": "fixture-v12-cursor-claude-message",
                    "model": "fixture-model",
                    "stop_reason": "end_turn",
                    "usage": {
                        "input_tokens": 10,
                        "output_tokens": 2,
                        "cache_read_input_tokens": 0,
                        "cache_creation_input_tokens": 0
                    }
                }
            }),
        ],
    );

    sessions::sync(
        &mut store,
        &pricing,
        &Settings::default(),
        "codex",
        &home,
        false,
    )
    .unwrap();
    sessions::sync(
        &mut store,
        &pricing,
        &Settings::default(),
        "claude",
        &home,
        false,
    )
    .unwrap();
    let codex_cursor = safe_id(&format!("codex:{}", codex_path.to_string_lossy()));
    let claude_cursor = safe_id(&format!("claude:{}", claude_path.to_string_lossy()));
    let codex_before = store.cursor(&codex_cursor).unwrap().unwrap();
    let claude_before = store.cursor(&claude_cursor).unwrap().unwrap();
    let codex_before_page = store
        .logs(&Filter {
            client: Some("codex".into()),
            source: Some("sessions".into()),
            start: Some(BASE),
            end: Some(BASE + 100),
            ..Filter::default()
        })
        .unwrap();
    assert_eq!(codex_before_page.total, 1);
    let codex_id = codex_before_page.rows[0].id.clone();

    let moved_path = home.join("001-codex.jsonl");
    fs::rename(&codex_path, &moved_path).unwrap();
    sessions::sync(
        &mut store,
        &pricing,
        &Settings::default(),
        "codex",
        &home,
        true,
    )
    .unwrap();
    assert!(store.detail(&codex_id).is_err());
    assert!(store.cursor(&codex_cursor).unwrap().is_none());
    assert_eq!(
        store.cursor(&claude_cursor).unwrap().as_deref(),
        Some(claude_before.as_str())
    );

    fs::rename(&moved_path, &codex_path).unwrap();
    let restored = sessions::sync(
        &mut store,
        &pricing,
        &Settings::default(),
        "codex",
        &home,
        false,
    )
    .unwrap();
    assert_eq!(restored.imported, 1);
    assert_eq!(
        store
            .logs(&Filter {
                client: Some("codex".into()),
                source: Some("sessions".into()),
                start: Some(BASE),
                end: Some(BASE + 100),
                ..Filter::default()
            })
            .unwrap()
            .total,
        1
    );
    let codex_after = store.cursor(&codex_cursor).unwrap().unwrap();
    assert_ne!(codex_before, codex_after);

    let replay = sessions::sync(
        &mut store,
        &pricing,
        &Settings::default(),
        "codex",
        &home,
        false,
    )
    .unwrap();
    assert_eq!(replay.imported, 0);
    assert_eq!(
        store
            .logs(&Filter {
                client: Some("codex".into()),
                source: Some("sessions".into()),
                start: Some(BASE),
                end: Some(BASE + 100),
                ..Filter::default()
            })
            .unwrap()
            .total,
        1
    );
    sessions::sync(
        &mut store,
        &pricing,
        &Settings::default(),
        "claude",
        &home,
        false,
    )
    .unwrap();
    assert_eq!(
        store.cursor(&claude_cursor).unwrap().as_deref(),
        Some(claude_before.as_str())
    );
}

#[test]
fn v12_corrupt_narrow_body_keeps_the_record_and_isolated_detail_failure() {
    let (dir, mut store) = migrated_empty();
    let broken = record(
        "fixture-v12-narrow-corrupt",
        "codex",
        BASE,
        Some("fixture-v12-narrow-corrupt-response"),
        10,
        2,
    );
    let healthy = record(
        "fixture-v12-narrow-healthy",
        "codex",
        BASE + 1,
        Some("fixture-v12-narrow-healthy-response"),
        20,
        3,
    );
    store
        .write_batch(&[broken.clone(), healthy.clone()], None)
        .unwrap();
    drop(store);

    let database = Connection::open(dir.path().join("usage.sqlite")).unwrap();
    let rid: i64 = database
        .query_row(
            "SELECT rid FROM v12_records ORDER BY rid LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    database
        .execute(
            "UPDATE v12_records SET body=?1 WHERE rid=?2",
            params![vec![0xff_u8], rid],
        )
        .unwrap();
    drop(database);

    let reopened = Store::open(dir.path()).unwrap();
    assert!(reopened.detail(&broken.id).is_err());
    assert!(reopened.detail(&healthy.id).is_ok());
    drop(reopened);
    let database = Connection::open(dir.path().join("usage.sqlite")).unwrap();
    let body: Vec<u8> = database
        .query_row("SELECT body FROM v12_records WHERE rid=?1", [rid], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(body, vec![0xff_u8]);
}

#[test]
fn v12_reclaim_with_a_busy_writer_is_nonfatal() {
    let (dir, mut store) = migrated_empty();
    let row = record(
        "fixture-v12-busy-reclaim",
        "codex",
        BASE,
        Some("fixture-v12-busy-response"),
        10,
        2,
    );
    store.write_batch(std::slice::from_ref(&row), None).unwrap();
    let lock = Connection::open(dir.path().join("usage.sqlite")).unwrap();
    lock.execute_batch("BEGIN IMMEDIATE").unwrap();
    assert!(store.reclaim_space().is_ok());
    lock.execute_batch("ROLLBACK").unwrap();
    assert!(store.detail(&row.id).is_ok());
}

#[test]
fn v12_interrupted_rebuild_keeps_old_source_until_finish() {
    let (dir, mut store) = migrated_empty();
    let old = record(
        "fixture-v12-interrupted-old",
        "codex",
        BASE,
        Some("fixture-v12-interrupted-old-response"),
        10,
        2,
    );
    let other = record(
        "fixture-v12-interrupted-other",
        "claude",
        BASE + 1,
        Some("fixture-v12-interrupted-other-response"),
        10,
        2,
    );
    store
        .write_batch(
            &[old.clone(), other.clone()],
            Some(("fixture-v12-interrupted-cursor", "old")),
        )
        .unwrap();
    let before = json!({
        "total": store.logs(&Filter::default()).unwrap().total,
        "dashboard": dashboard_value(&store, &all_time()),
    });

    let replacement = record(
        "fixture-v12-interrupted-new",
        "codex",
        BASE + 2,
        Some("fixture-v12-interrupted-new-response"),
        20,
        3,
    );
    store.begin_rebuild("codex").unwrap();
    store
        .stage_rebuild(
            "codex",
            std::slice::from_ref(&replacement),
            Some(("fixture-v12-interrupted-cursor", "new")),
        )
        .unwrap();
    drop(store);

    let mut resumed = Store::open(dir.path()).unwrap();
    assert_eq!(
        json!({
            "total": resumed.logs(&Filter::default()).unwrap().total,
            "dashboard": dashboard_value(&resumed, &all_time()),
        }),
        before
    );
    assert!(resumed.detail(&old.id).is_ok());
    assert!(resumed.detail(&replacement.id).is_err());
    assert_eq!(
        resumed
            .cursor("fixture-v12-interrupted-cursor")
            .unwrap()
            .as_deref(),
        Some("old")
    );

    resumed.finish_rebuild("codex").unwrap();
    assert!(resumed.detail(&old.id).is_err());
    assert!(resumed.detail(&replacement.id).is_ok());
    assert!(resumed.detail(&other.id).is_ok());
    assert_eq!(
        resumed
            .cursor("fixture-v12-interrupted-cursor")
            .unwrap()
            .as_deref(),
        Some("new")
    );
}

#[test]
fn v12_diagnostic_block_compaction_preserves_detail_fields() {
    let (dir, mut store) = migrated_empty();
    let row = priced_record(
        "fixture-v12-diagnostic-block-equivalence",
        BASE,
        123,
        45,
        "0.12345",
    );
    store.write_batch(std::slice::from_ref(&row), None).unwrap();
    let before = serde_json::to_value(store.detail(&row.id).unwrap()).unwrap();

    let database_path = dir.path().join("usage.sqlite");
    let database = Connection::open(&database_path).unwrap();
    let blob_rows: i64 = database
        .query_row(
            "SELECT count(*) FROM (SELECT body FROM v12_records UNION ALL SELECT body FROM v12_attempt_rows UNION ALL SELECT body FROM v12_session_rows UNION ALL SELECT body FROM v12_original) WHERE typeof(body)='blob'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        blob_rows > 0,
        "at least one diagnostic payload must start unpacked"
    );
    drop(database);

    compact_diagnostics_until_done(&mut store);
    let after = serde_json::to_value(store.detail(&row.id).unwrap()).unwrap();
    assert_eq!(after, before);

    let database = Connection::open(database_path).unwrap();
    let blocks: i64 = database
        .query_row("SELECT count(*) FROM v12_diagnostics", [], |r| r.get(0))
        .unwrap();
    let packed_rows: i64 = database
        .query_row(
            "SELECT count(*) FROM (SELECT body FROM v12_records UNION ALL SELECT body FROM v12_attempt_rows UNION ALL SELECT body FROM v12_session_rows UNION ALL SELECT body FROM v12_original) WHERE typeof(body)='integer'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(blocks > 0);
    assert!(packed_rows > 0);
}

#[test]
fn v12_antigravity_reasoning_survives_compaction_and_old_snapshots() {
    let (dir, mut writer) = migrated_empty();
    let mut row = record("fixture-ag-reasoning", "antigravity", BASE, None, 100, 20);
    row.client = "antigravity".into();
    row.attempts[0].tokens.reasoning = Some(9);
    row.attempts[0].generation_ms = Some(900);
    super::antigravity::normalize_record(&mut row);
    writer
        .write_batch(std::slice::from_ref(&row), None)
        .unwrap();
    let reasoning = |store: &Store| {
        let mut values = Vec::new();
        super::storage_v12::scan_antigravity_attempts(
            &store.connection,
            BASE,
            BASE + 1000,
            |attempt| {
                values.push(attempt.tokens.reasoning);
                Ok(())
            },
        )
        .unwrap();
        values
    };
    assert_eq!(reasoning(&writer), vec![Some(9)]);
    compact_diagnostics_until_done(&mut writer);
    assert_eq!(reasoning(&writer), vec![Some(9)]);
    let generation = |store: &Store| -> u64 {
        store
            .connection
            .query_row(
                "SELECT coalesce(sum(generation_ms),0) FROM usage_metrics",
                [],
                |r| r.get(0),
            )
            .unwrap()
    };
    assert_eq!(generation(&writer), 900);
    let reader = Store::reader(dir.path()).unwrap();
    reader
        .snapshot(|snapshot| {
            assert_eq!(reasoning(snapshot), vec![Some(9)]);
            assert_eq!(generation(snapshot), 900);
            row.attempts[0].tokens.reasoning = Some(7);
            row.attempts[0].generation_ms = Some(700);
            writer.write_batch(std::slice::from_ref(&row), None)?;
            compact_diagnostics_until_done(&mut writer);
            assert_eq!(reasoning(&writer), vec![Some(7)]);
            assert_eq!(generation(&writer), 700);
            assert_eq!(generation(snapshot), 900);
            assert_eq!(reasoning(snapshot), vec![Some(9)]);
            Ok(())
        })
        .unwrap();
    assert_eq!(reasoning(&reader), vec![Some(7)]);
    assert_eq!(generation(&reader), 700);
}

#[test]
fn v12_diagnostic_slots_survive_update_delete_cold_and_prune() {
    let (dir, mut store) = migrated_empty();
    let at = BASE + 80 * DAY;
    let old_at = at - 8 * DAY;
    let mut deleted = priced_record("fixture-v12-diagnostic-delete", old_at, 10, 2, "0.1");
    deleted.client = "codex".into();
    deleted.source = "codex".into();
    let survivor = priced_record("fixture-v12-diagnostic-survivor", old_at, 20, 3, "0.2");
    store
        .write_batch(&[deleted.clone(), survivor.clone()], None)
        .unwrap();
    compact_diagnostics_until_done(&mut store);

    let survivor_before = store.detail(&survivor.id).unwrap();
    let mut replacement = deleted.clone();
    replacement.attempts[0].tokens.output = Some(9);
    replacement.attempts[0]
        .transfer
        .as_mut()
        .unwrap()
        .response_hash = Some("fixture-v12-diagnostic-updated-hash".into());
    store
        .write_batch(std::slice::from_ref(&replacement), None)
        .unwrap();
    compact_diagnostics_until_done(&mut store);
    assert_eq!(
        serde_json::to_value(store.detail(&survivor.id).unwrap()).unwrap(),
        serde_json::to_value(&survivor_before).unwrap()
    );

    store.rebuild("codex", &[], &[]).unwrap();
    assert!(store.detail(&deleted.id).is_err());
    assert_eq!(
        serde_json::to_value(store.detail(&survivor.id).unwrap()).unwrap(),
        serde_json::to_value(&survivor_before).unwrap()
    );

    store.compact(at).unwrap();
    let cold = store.detail(&survivor.id).unwrap();
    assert!(cold.detail_archived);
    assert_eq!(cold.tokens(), survivor_before.tokens());
    assert_eq!(
        serde_json::to_value(&cold.attempts[0].price).unwrap(),
        serde_json::to_value(&survivor_before.attempts[0].price).unwrap()
    );

    let database = Connection::open(dir.path().join("usage.sqlite")).unwrap();
    database
        .execute(
            "INSERT OR REPLACE INTO metadata(key,value) VALUES('storage_repack','1')",
            [],
        )
        .unwrap();
    drop(database);
    store.reclaim_space().unwrap();
    let after_prune = store.detail(&survivor.id).unwrap();
    assert_eq!(
        serde_json::to_value(&after_prune).unwrap(),
        serde_json::to_value(&cold).unwrap()
    );
    assert!(store.detail(&deleted.id).is_err());
}

#[test]
fn v12_old_snapshot_reads_a_retired_diagnostic_block() {
    let (dir, mut writer) = migrated_empty();
    let row = priced_record("fixture-v12-diagnostic-snapshot", BASE, 11, 4, "0.11");
    writer
        .write_batch(std::slice::from_ref(&row), None)
        .unwrap();
    compact_diagnostics_until_done(&mut writer);

    let reader = Store::reader(dir.path()).unwrap();
    let expected = serde_json::to_value(reader.detail(&row.id).unwrap()).unwrap();
    reader
        .snapshot(|snapshot| {
            assert_eq!(
                serde_json::to_value(snapshot.detail(&row.id).unwrap()).unwrap(),
                expected
            );
            let mut updated = row.clone();
            updated.attempts[0].transfer.as_mut().unwrap().response_hash =
                Some("fixture-v12-diagnostic-snapshot-new".into());
            writer.write_batch(std::slice::from_ref(&updated), None)?;
            compact_diagnostics_until_done(&mut writer);
            assert_eq!(
                serde_json::to_value(snapshot.detail(&row.id).unwrap()).unwrap(),
                expected
            );
            Ok(())
        })
        .unwrap();
    assert_ne!(
        serde_json::to_value(writer.detail(&row.id).unwrap()).unwrap(),
        expected
    );
}

#[test]
fn v12_corrupt_diagnostic_block_fails_read_and_maintenance_without_delete() {
    let (dir, mut store) = migrated_empty();
    let row = priced_record("fixture-v12-diagnostic-corrupt-block", BASE, 7, 2, "0.07");
    let survivor = priced_record(
        "fixture-v12-diagnostic-corrupt-survivor",
        BASE + 1,
        8,
        3,
        "0.08",
    );
    store
        .write_batch(&[row.clone(), survivor.clone()], None)
        .unwrap();
    compact_diagnostics_until_done(&mut store);

    let database_path = dir.path().join("usage.sqlite");
    let database = Connection::open(&database_path).unwrap();
    let block: i64 = database
        .query_row(
            "SELECT id FROM v12_diagnostics ORDER BY id LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    drop(database);

    let mut updated = row.clone();
    updated.attempts[0].transfer.as_mut().unwrap().response_hash =
        Some("fixture-v12-diagnostic-corrupt-updated".into());
    store
        .write_batch(std::slice::from_ref(&updated), None)
        .unwrap();
    compact_diagnostics_until_done(&mut store);
    drop(store);

    let database = Connection::open(&database_path).unwrap();
    database
        .execute(
            "UPDATE v12_diagnostics SET body=?1 WHERE id=?2",
            params![vec![0xff_u8], block],
        )
        .unwrap();
    drop(database);

    let reopened = Store::open(dir.path()).unwrap();
    assert!(reopened.detail(&survivor.id).is_err());
    let database = Connection::open(&database_path).unwrap();
    database
        .execute(
            "INSERT OR REPLACE INTO metadata(key,value) VALUES('storage_repack','1')",
            [],
        )
        .unwrap();
    drop(database);
    assert!(reopened.reclaim_space().is_err());
    drop(reopened);

    let database = Connection::open(database_path).unwrap();
    let body: Vec<u8> = database
        .query_row(
            "SELECT body FROM v12_diagnostics WHERE id=?1",
            [block],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(body, vec![0xff_u8]);
}

#[test]
fn v12_uuid_identifiers_survive_diagnostic_compaction_losslessly() {
    const RECORD_ID: &str = "123e4567-e89b-12d3-a456-426614174000";
    const RESPONSE_ID: &str = "123e4567-e89b-12d3-a456-426614174001";
    const SESSION_ID: &str = "123e4567-e89b-12d3-a456-426614174002";
    const ATTEMPT_ID: &str = "123e4567-e89b-12d3-a456-426614174003";

    let (_dir, mut store) = migrated_empty();
    let mut row = priced_record(RECORD_ID, BASE, 8, 3, "0.08");
    row.session_id = Some(SESSION_ID.into());
    row.attempts[0].id = ATTEMPT_ID.into();
    row.attempts[0].response_id = Some(RESPONSE_ID.into());
    row.attempts[0].thread_hash = Some("123e4567-e89b-12d3-a456-426614174004".into());
    store.write_batch(std::slice::from_ref(&row), None).unwrap();
    let before = store.detail(RECORD_ID).unwrap();
    compact_diagnostics_until_done(&mut store);
    let after = store.detail(RECORD_ID).unwrap();

    assert_eq!(after.id, RECORD_ID);
    assert_eq!(after.session_id.as_deref(), Some(SESSION_ID));
    assert_eq!(after.attempts[0].id, ATTEMPT_ID);
    assert_eq!(after.attempts[0].response_id.as_deref(), Some(RESPONSE_ID));
    assert_eq!(
        after.attempts[0].thread_hash.as_deref(),
        Some("123e4567-e89b-12d3-a456-426614174004")
    );
    assert_eq!(
        serde_json::to_value(&after).unwrap(),
        serde_json::to_value(&before).unwrap()
    );
}

#[test]
fn native_reads_backfill_counts_and_archive_match_legacy() {
    let dir = tempfile::tempdir().unwrap();
    let pricing = Pricing::new(&dir.path().join("prices")).unwrap();
    let rows: Vec<_> = (0..137)
        .map(|i| {
            let mut r = record(
                &format!("fixture-native-read-{i}"),
                "codex",
                BASE + i,
                None,
                100 + i as u64,
                20,
            );
            r.attempts[0].requested_model = Some("gpt-4o".into());
            r.attempts[0].response_model = Some("gpt-4o".into());
            r.attempts[0].pricing_model = Some("gpt-4o".into());
            r
        })
        .collect();
    let mut results = Vec::new();
    for native in [false, true] {
        let mut store = if native {
            Store::open(&dir.path().join("native"))
        } else {
            Store::open_v11_fixture(&dir.path().join("legacy"))
        }
        .unwrap();
        store.write_batch(&rows, None).unwrap();
        let table = if native { "v12_rows" } else { "records" };
        store
            .connection
            .execute(
                &format!("UPDATE {table} SET effective=2 WHERE time%3=0"),
                [],
            )
            .unwrap();
        store
            .connection
            .execute(
                &format!("UPDATE {table} SET effective=0 WHERE time%3=1"),
                [],
            )
            .unwrap();
        assert_eq!(store.source_counts("codex").unwrap(), (46, 46, 0));
        assert_eq!(store.source_counts("missing").unwrap(), (0, 0, 0));
        store.backfill(&pricing).unwrap();
        assert!(store.detail(&rows[0].id).unwrap().attempts[0]
            .price
            .is_some());
        let page = store
            .logs(&Filter {
                page: 2,
                ..Default::default()
            })
            .unwrap();
        let before = store.dashboard(&Filter::default()).unwrap().totals;
        for chunk in rows.chunks(50) {
            store
                .archive_batch(&chunk.iter().map(|r| r.id.clone()).collect::<Vec<_>>())
                .unwrap();
        }
        let after = store.dashboard(&Filter::default()).unwrap().totals;
        assert_eq!(
            serde_json::to_value(&before).unwrap(),
            serde_json::to_value(&after).unwrap()
        );
        results.push((
            serde_json::to_value(page.rows).unwrap(),
            serde_json::to_value(after).unwrap(),
        ));
    }
    assert_eq!(results[0], results[1]);
}

#[test]
fn native_archive_preserves_all_scopes_operations_and_receipts() {
    let dir = tempfile::tempdir().unwrap();
    let mut rows = Vec::new();
    for (i, source) in ["proxy", "codex", "grok"].into_iter().enumerate() {
        for (j, operation) in [
            Operation::Model,
            Operation::Compaction,
            Operation::WebSearch,
        ]
        .into_iter()
        .enumerate()
        {
            let n = i * 3 + j;
            let mut row = priced_record(
                &format!("fixture-archive-op-{n}"),
                BASE + (n % 2) as i64 * DAY,
                100 + n as u64,
                20,
                "0.012",
            );
            row.source = source.into();
            if source == "grok" {
                row.client = "grok".into();
            }
            row.attempts[0].operation = operation;
            rows.push(row);
        }
    }
    let mut duplicate = rows[0].clone();
    duplicate.id = "fixture-archive-duplicate".into();
    duplicate.source = "codex".into();
    duplicate.attempts[0].id = "fixture-duplicate-attempt".into();
    rows.push(duplicate);
    let mut results = Vec::new();
    for native in [false, true] {
        let root = dir.path().join(if native { "native" } else { "legacy" });
        let mut store = if native {
            Store::open(&root)
        } else {
            Store::open_v11_fixture(&root)
        }
        .unwrap();
        store.write_batch(&rows, None).unwrap();
        assert!(store
            .detail("fixture-archive-duplicate")
            .unwrap()
            .duplicate_of
            .is_some());
        let filters: Vec<_> = [None, Some("proxy"), Some("sessions"), Some("grok_sessions")]
            .into_iter()
            .flat_map(|scope| {
                [None, Some("model"), Some("compaction"), Some("web_search")]
                    .into_iter()
                    .map(move |op| Filter {
                        source: scope.map(str::to_owned),
                        operation: op.map(str::to_owned),
                        ..Filter::default()
                    })
            })
            .collect();
        let totals = |s: &Store| {
            filters
                .iter()
                .map(|f| serde_json::to_value(s.dashboard(f).unwrap().totals).unwrap())
                .collect::<Vec<_>>()
        };
        let before = totals(&store);
        store
            .archive_batch(&rows.iter().map(|r| r.id.clone()).collect::<Vec<_>>())
            .unwrap();
        assert_eq!(totals(&store), before);
        let receipts: Vec<(String, String, Option<String>, String)> = store
            .connection
            .prepare("SELECT id,operation,signature,source FROM receipts ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        for row in &rows {
            assert_eq!(
                receipts.iter().find(|r| r.0 == row.id).unwrap().1,
                row.final_attempt().unwrap().operation.as_str()
            );
        }
        results.push((before, receipts));
        if native {
            drop(store);
            assert_eq!(
                totals(&Store::open(&root).unwrap()),
                results.last().unwrap().0
            );
        }
    }
    assert_eq!(results[0], results[1]);
}

#[test]
fn migration_preserves_links_to_already_archived_records() {
    let dir = tempfile::tempdir().unwrap();
    let mut old = Store::open_v11_fixture(dir.path()).unwrap();
    let proxy = record(
        "fixture-archived-proxy",
        "proxy",
        BASE,
        Some("fixture-archived-response"),
        100,
        20,
    );
    let session = record(
        "fixture-archived-session",
        "codex",
        BASE + 1,
        Some("fixture-archived-response"),
        100,
        20,
    );
    old.write_batch(&[proxy.clone(), session.clone()], None)
        .unwrap();
    old.archive_batch(std::slice::from_ref(&proxy.id)).unwrap();
    assert_eq!(
        old.detail(&session.id).unwrap().duplicate_of.as_deref(),
        Some(proxy.id.as_str())
    );
    let snapshot = |store: &Store| json!({"page":page_value(store,&all_time()),"dashboard":dashboard_value(store,&all_time()),"detail":store.detail(&session.id).unwrap()});
    let before = snapshot(&old);
    drop(old);
    let mut migrating = Store::open(dir.path()).unwrap();
    assert!(migrating.compact_storage_batch().unwrap());
    drop(migrating);
    let mut resumed = Store::open(dir.path()).unwrap();
    while resumed.compact_storage_batch().unwrap() {}
    assert_eq!(
        resumed.detail(&session.id).unwrap().duplicate_of.as_deref(),
        Some(proxy.id.as_str())
    );
    assert_eq!(snapshot(&resumed), before);
}
