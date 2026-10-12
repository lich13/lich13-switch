//! v0.22 usage event-time migration and pricing snapshot fixtures.
//!
//! Timestamps, model names, provider names, and price data are synthetic.
use super::{
    bodies,
    model::{Attempt, Filter, PriceSnapshot, Record, Tokens, TransferMetadata},
    store::Store,
};
use rusqlite::{params, Connection};
use serde_json::json;
use std::collections::BTreeMap;

fn price(cost: &str) -> PriceSnapshot {
    PriceSnapshot {
        version: "fixture-price-v022".into(),
        source: "fixture-catalog".into(),
        model: "fixture-model".into(),
        multiplier: "1.5".into(),
        rates: BTreeMap::from([("input".into(), "0.00001".into())]),
        cost: cost.into(),
        basis: Some(json!({"input_cost_per_token":"0.00001","fixture":true})),
    }
}

fn attempt(
    id: &str,
    source: &str,
    started_at: i64,
    terminal_at: Option<i64>,
    price: Option<PriceSnapshot>,
) -> Attempt {
    Attempt {
        id: format!("{id}-attempt"),
        provider: (source == "proxy").then(|| "fixture-provider".into()),
        requested_model: Some("fixture-model".into()),
        response_model: Some("fixture-model".into()),
        pricing_model: Some("fixture-model".into()),
        response_id: Some(format!("{id}-response")),
        status: (source == "proxy").then_some(200),
        outcome: "success".into(),
        started_at,
        duration_ms: 1_000,
        stream: source == "proxy",
        transport: if source == "proxy" { "http" } else { "session" }.into(),
        tokens: Tokens {
            input: Some(10),
            output: Some(2),
            inclusive_input: Some(10),
            ..Tokens::default()
        },
        transfer: (source == "proxy").then(|| TransferMetadata {
            sent_at: started_at,
            terminal_at,
            ended_at: terminal_at.unwrap_or_default(),
            terminal_handed_off: terminal_at.is_some(),
            ..TransferMetadata::default()
        }),
        price,
        ..Attempt::default()
    }
}

fn record(
    id: &str,
    source: &str,
    started_at: i64,
    finished_at: Option<i64>,
    terminal_at: Option<i64>,
    price: Option<PriceSnapshot>,
) -> Record {
    Record {
        id: id.into(),
        client: "codex".into(),
        source: source.into(),
        started_at,
        finished_at,
        completed: true,
        attempts: vec![attempt(id, source, started_at, terminal_at, price)],
        ..Record::default()
    }
}

#[test]
fn event_time_uses_terminal_finish_local_finish_then_legacy_start() {
    let terminal = record(
        "fixture-terminal",
        "proxy",
        1_000,
        Some(1_250),
        Some(1_300),
        None,
    );
    assert_eq!(terminal.event_time(), 1_300);

    let mut ended = record("fixture-ended", "proxy", 2_000, Some(2_250), None, None);
    ended.attempts[0].transfer.as_mut().unwrap().ended_at = 2_400;
    assert_eq!(ended.event_time(), 2_400);

    let local = record("fixture-local", "proxy", 3_000, Some(3_250), Some(0), None);
    assert_eq!(local.event_time(), 3_250);

    let legacy = record("fixture-legacy", "proxy", 4_000, None, Some(0), None);
    assert_eq!(legacy.event_time(), 4_000);

    let session = record("fixture-session", "codex", 5_000, Some(5_500), None, None);
    assert_eq!(session.event_time(), 5_500);
}

#[test]
fn store_filters_and_sorts_logs_by_finished_event_time() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let first = record(
        "fixture-finished-late",
        "codex",
        1_000,
        Some(3_000),
        None,
        None,
    );
    let second = record(
        "fixture-finished-early",
        "codex",
        2_000,
        Some(2_500),
        None,
        None,
    );
    store.write_batch(&[first, second], None).unwrap();

    let page = store.logs(&Filter::default()).unwrap();
    assert_eq!(
        page.rows
            .iter()
            .map(|row| row.id.as_str())
            .collect::<Vec<_>>(),
        ["fixture-finished-late", "fixture-finished-early",]
    );
    assert_eq!(page.rows[0].started_at, 1_000);

    let filtered = store
        .logs(&Filter {
            start: Some(2_750),
            end: Some(3_250),
            ..Filter::default()
        })
        .unwrap();
    assert_eq!(filtered.total, 1);
    assert_eq!(filtered.rows[0].id, "fixture-finished-late");
}

#[test]
fn old_records_migrate_in_batches_and_keep_started_at_and_price_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let priced = record(
        "fixture-migrate-priced",
        "proxy",
        10_000,
        Some(12_000),
        Some(12_000),
        Some(price("0.1234")),
    );
    let mut store = Store::open(dir.path()).unwrap();
    store
        .write_batch(std::slice::from_ref(&priced), None)
        .unwrap();
    drop(store);

    let database = dir.path().join("usage.sqlite");
    let connection = Connection::open(&database).unwrap();
    let body: String = connection
        .query_row(
            "SELECT body FROM records WHERE id=?1",
            ["fixture-migrate-priced"],
            |row| row.get(0),
        )
        .unwrap();
    let mut value = bodies::unpack(&body).unwrap();
    value.as_object_mut().unwrap().remove("finishedAt");
    connection
        .execute(
            "UPDATE records SET time=?2,body=?3 WHERE id=?1",
            params![
                "fixture-migrate-priced",
                10_000_i64,
                serde_json::to_string(&value).unwrap()
            ],
        )
        .unwrap();
    connection
        .execute_batch(
            "ALTER TABLE records DROP COLUMN local_sequence;
             DELETE FROM metadata WHERE key IN ('record_sequence','finish_time_after');
             PRAGMA user_version=9;",
        )
        .unwrap();
    drop(connection);

    let mut migrated = Store::open(dir.path()).unwrap();
    assert!(migrated.repair_finish_times().unwrap());
    assert!(!migrated.repair_finish_times().unwrap());
    let restored = migrated.detail("fixture-migrate-priced").unwrap();
    assert_eq!(restored.started_at, 10_000);
    assert_eq!(restored.finished_at, Some(12_000));
    let snapshot = restored.attempts[0].price.as_ref().unwrap();
    assert_eq!(snapshot.cost, "0.1234");
    assert_eq!(snapshot.multiplier, "1.5");
    assert_eq!(
        snapshot.basis,
        Some(json!({"input_cost_per_token":"0.00001","fixture":true}))
    );
}

#[test]
fn finish_time_repair_is_restartable_after_the_128_row_batch_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let rows = (0..130)
        .map(|index| {
            record(
                &format!("fixture-batch-{index:03}"),
                "proxy",
                20_000 + index,
                Some(21_000 + index),
                Some(21_000 + index),
                None,
            )
        })
        .collect::<Vec<_>>();
    store.write_batch(&rows, None).unwrap();
    drop(store);

    let connection = Connection::open(dir.path().join("usage.sqlite")).unwrap();
    connection
        .execute(
            "INSERT OR REPLACE INTO metadata(key,value) VALUES('finish_time_after','')",
            [],
        )
        .unwrap();
    drop(connection);

    let mut migrated = Store::open(dir.path()).unwrap();
    assert!(migrated.repair_finish_times().unwrap());
    assert!(migrated.repair_finish_times().unwrap());
    assert!(!migrated.repair_finish_times().unwrap());
    assert_eq!(
        migrated.detail("fixture-batch-129").unwrap().finished_at,
        Some(21_129)
    );
}

#[test]
fn compact_merges_same_day_finished_times_without_changing_totals_or_price() {
    const DAY: i64 = 86_400_000;
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let day = 40 * DAY;
    let first = record(
        "fixture-compact-first",
        "proxy",
        day + 1_000,
        Some(day + 20_000),
        Some(day + 20_000),
        Some(price("0.10")),
    );
    let second = record(
        "fixture-compact-second",
        "proxy",
        day + 2_000,
        Some(day + 70_000),
        Some(day + 70_000),
        Some(price("0.10")),
    );
    store.write_batch(&[first, second], None).unwrap();

    store.compact(80 * DAY).unwrap();

    let database = Connection::open(dir.path().join("usage.sqlite")).unwrap();
    let (count, body): (u64, String) = database
        .query_row(
            "SELECT count(*),body FROM daily WHERE json_extract(body,'$.scope')='all'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(count, 1);
    let daily = bodies::unpack(&body).unwrap();
    assert_eq!(daily["totals"]["requests"], 2);
    assert_eq!(daily["totals"]["cost"], "0.20");
    assert_eq!(daily["example"]["finishedAt"], serde_json::Value::Null);
}
