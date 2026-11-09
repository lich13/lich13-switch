// 覆盖用量存储迁移、重入与合成数据体积。
use super::{model::*, store::Store};
use base64::{engine::general_purpose::STANDARD, Engine};
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::Path,
    time::{Duration, Instant},
};

const DAY: i64 = 86_400_000;
const BASE: i64 = 20_000 * DAY;
const WAL_LIMIT: u64 = 8 * 1024 * 1024;

fn price(cost: &str, basis: Value) -> PriceSnapshot {
    PriceSnapshot {
        version: "fixture-v0212-price".into(),
        source: "fixture".into(),
        model: "fixture-model".into(),
        multiplier: "1".into(),
        rates: BTreeMap::from([("input".into(), "0.01".into())]),
        cost: cost.into(),
        basis: Some(basis),
    }
}

fn record(id: &str, source: &str, response_id: &str, price: Option<PriceSnapshot>) -> Record {
    Record {
        id: id.into(),
        client: "codex".into(),
        source: source.into(),
        started_at: BASE,
        completed: true,
        attempts: vec![Attempt {
            id: format!("{id}-attempt"),
            provider: (source == "proxy").then(|| "fixture-provider".into()),
            requested_model: Some("fixture-model".into()),
            response_model: Some("fixture-model".into()),
            pricing_model: Some("fixture-model".into()),
            response_id: Some(response_id.into()),
            status: (source == "proxy").then_some(200),
            outcome: "success".into(),
            started_at: BASE,
            duration_ms: 1_000,
            first_token_ms: (source == "proxy").then_some(123),
            stream: source == "proxy",
            transport: if source == "proxy" { "http" } else { "session" }.into(),
            tokens: Tokens {
                input: Some(10),
                output: Some(2),
                inclusive_input: Some(10),
                ..Tokens::default()
            },
            price,
            ..Attempt::default()
        }],
        ..Record::default()
    }
}

fn summary(store: &Store) -> (u64, u64, String, Tokens, u64, u64, BTreeMap<String, u64>) {
    let dashboard = store.dashboard(&Filter::default()).unwrap();
    let totals = dashboard.totals;
    (
        totals.requests,
        totals.attempts,
        totals.cost,
        totals.tokens,
        totals.first_token_sum_ms,
        totals.first_token_samples,
        dashboard.sources,
    )
}

fn file_len(path: &Path) -> u64 {
    fs::metadata(path).map_or(0, |metadata| metadata.len())
}

fn wal_len(dir: &Path) -> u64 {
    file_len(&dir.join("usage.sqlite-wal"))
}

fn complete_storage_migration(store: &mut Store) {
    while store.compact_storage_batch().unwrap() {}
}

fn legacy_zstd_wrapper(database: &Connection, body: &str) -> String {
    let mut wrapper: Value = serde_json::from_str(body).unwrap();
    let raw = serde_json::to_vec(&super::bodies::unpack(database, body).unwrap()).unwrap();
    let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), 6).unwrap();
    encoder.include_checksum(true).unwrap();
    encoder.write_all(&raw).unwrap();
    let packed = encoder.finish().unwrap();
    wrapper["_usageCodec"] = Value::String("zstd-v1".into());
    wrapper["_usageBody"] = Value::String(STANDARD.encode(packed));
    serde_json::to_string(&wrapper).unwrap()
}

fn restore_inline_basis(value: &mut Value, bases: &BTreeMap<String, Value>) {
    if let Some(example) = value.get_mut("example") {
        restore_inline_basis(example, bases);
    }
    if let Some(attempts) = value.get_mut("attempts").and_then(Value::as_array_mut) {
        for attempt in attempts {
            restore_price_basis(attempt.get_mut("price"), bases);
        }
    }
    if let Some(gateway) = value.get_mut("gatewayReported") {
        restore_price_basis(gateway.get_mut("price"), bases);
    }
}

fn restore_price_basis(price: Option<&mut Value>, bases: &BTreeMap<String, Value>) {
    let Some(price) = price.filter(|price| price.is_object()) else {
        return;
    };
    let Some(reference) = price.get("basisRef").and_then(Value::as_str) else {
        return;
    };
    let basis = bases
        .get(reference)
        .unwrap_or_else(|| panic!("fixture price basis {reference} exists"))
        .clone();
    let object = price.as_object_mut().unwrap();
    object.remove("basisRef");
    object.insert("basis".into(), basis);
}

fn downgrade_to_v8(dir: &Path, inline_bases: bool, mode_none: bool) {
    let database = Connection::open(dir.join("usage.sqlite")).unwrap();
    if inline_bases {
        let bases = {
            let mut statement = database
                .prepare("SELECT hash,body FROM price_bases")
                .unwrap();
            let rows = statement
                .query_map([], |row| {
                    let hash: String = row.get(0)?;
                    let body: String = row.get(1)?;
                    Ok((hash, serde_json::from_str::<Value>(&body).unwrap()))
                })
                .unwrap()
                .map(Result::unwrap)
                .collect::<BTreeMap<_, _>>();
            rows
        };
        let rows = {
            let mut statement = database.prepare("SELECT id,body FROM records").unwrap();
            let rows = statement
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .unwrap()
                .map(Result::unwrap)
                .collect::<Vec<_>>();
            rows
        };
        for (id, body) in rows {
            let mut value = super::bodies::unpack(&database, &body).unwrap();
            restore_inline_basis(&mut value, &bases);
            database
                .execute(
                    "UPDATE records SET body=?2 WHERE id=?1",
                    params![id, serde_json::to_string(&value).unwrap()],
                )
                .unwrap();
        }
        database.execute("DELETE FROM price_bases", []).unwrap();
    }
    database
        .execute(
            "DELETE FROM metadata WHERE key IN ('storage_records_after','storage_daily_after','storage_repack')",
            [],
        )
        .unwrap();
    database
        .execute_batch(
            "ALTER TABLE records DROP COLUMN local_sequence;
             PRAGMA user_version=8;",
        )
        .unwrap();
    if mode_none {
        database
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE); PRAGMA auto_vacuum=NONE; VACUUM;")
            .unwrap();
    }
}

#[test]
fn compressed_body_round_trip_restores_lossless_record() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open_v11_fixture(dir.path()).unwrap();
    let basis = json!({
        "input_cost_per_token": "0.01",
        "output_cost_per_token": "0.02",
        "fixture": "compressed-round-trip",
        "description": "x".repeat(1_800)
    });
    let mut expected = record(
        "fixture-compressed-round-trip",
        "proxy",
        "fixture-compressed-round-trip-response",
        Some(price("0.42", basis.clone())),
    );
    expected.attempts[0].transport = "fixture-compressible-payload".repeat(400);
    expected.attempts[0].transfer = Some(TransferMetadata {
        sent_at: BASE,
        received_at: Some(BASE + 10),
        upstream_bytes: 1234,
        downstream_bytes: 5678,
        response_hash: Some("fixture-round-trip-response-hash".into()),
        ..TransferMetadata::default()
    });
    store.write_batch(&[expected.clone()], None).unwrap();
    let expected_detail = store.detail(&expected.id).unwrap();

    let database = Connection::open(dir.path().join("usage.sqlite")).unwrap();
    let body: String = database
        .query_row(
            "SELECT body FROM records WHERE id=?1",
            [&expected.id],
            |row| row.get(0),
        )
        .unwrap();
    let encoded: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(encoded["_usageCodec"], "zstd-compact-v2");
    let unpacked = super::bodies::unpack(&database, &body).unwrap();
    assert!(unpacked["attempts"][0]["price"]["basisRef"].is_string());
    assert!(unpacked["attempts"][0]["price"].get("basis").is_none());
    drop(database);

    assert_eq!(
        serde_json::to_value(store.detail(&expected.id).unwrap()).unwrap(),
        serde_json::to_value(expected_detail).unwrap()
    );
    assert_eq!(
        store
            .detail("fixture-compressed-round-trip")
            .unwrap()
            .attempts[0]
            .price
            .as_ref()
            .unwrap()
            .basis,
        Some(basis)
    );
}

#[test]
fn compressed_body_checksum_corruption_is_rejected_and_retained() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open_v11_fixture(dir.path()).unwrap();
    let mut row = record(
        "fixture-compressed-corrupt",
        "proxy",
        "fixture-compressed-corrupt-response",
        Some(price(
            "0.42",
            json!({"fixture":"compressed-corrupt","description":"x".repeat(1_800)}),
        )),
    );
    row.attempts[0].transport = "fixture-corruptible-payload".repeat(400);
    store.write_batch(&[row.clone()], None).unwrap();

    let database = Connection::open(dir.path().join("usage.sqlite")).unwrap();
    let body: String = database
        .query_row("SELECT body FROM records WHERE id=?1", [&row.id], |r| {
            r.get(0)
        })
        .unwrap();
    let mut wrapper: Value = serde_json::from_str(&body).unwrap();
    let encoded = wrapper["_usageBody"].as_str().unwrap();
    let mut packed = STANDARD.decode(encoded).unwrap();
    let last = packed
        .last_mut()
        .expect("compressed fixture has a checksum");
    *last ^= 0x01;
    wrapper["_usageBody"] = Value::String(STANDARD.encode(packed));
    let damaged = serde_json::to_string(&wrapper).unwrap();
    assert!(super::bodies::unpack(&database, &damaged).is_err());
    database
        .execute(
            "UPDATE records SET body=?2 WHERE id=?1",
            params![&row.id, damaged],
        )
        .unwrap();
    drop(database);

    assert!(store.detail(&row.id).is_err());
    let retained = Connection::open(dir.path().join("usage.sqlite"))
        .unwrap()
        .query_row("SELECT body FROM records WHERE id=?1", [&row.id], |r| {
            r.get::<_, String>(0)
        })
        .unwrap();
    assert_eq!(retained, damaged);
}

#[test]
fn old_and_compressed_bodies_are_read_together() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open_v11_fixture(dir.path()).unwrap();
    let old = record(
        "fixture-old-body",
        "codex",
        "fixture-old-body-response",
        None,
    );
    let mut new = record(
        "fixture-new-body",
        "proxy",
        "fixture-new-body-response",
        Some(price(
            "0.42",
            json!({"fixture":"mixed-body","description":"x".repeat(1_800)}),
        )),
    );
    new.attempts[0].transport = "fixture-mixed-compressible-payload".repeat(400);
    store
        .write_batch(&[old.clone(), new.clone()], None)
        .unwrap();
    let old_detail = store.detail(&old.id).unwrap();
    let new_detail = store.detail(&new.id).unwrap();

    let database = Connection::open(dir.path().join("usage.sqlite")).unwrap();
    let old_body = serde_json::to_string(&serde_json::to_value(&old_detail).unwrap()).unwrap();
    database
        .execute(
            "UPDATE records SET body=?2 WHERE id=?1",
            params![&old.id, old_body],
        )
        .unwrap();
    let stored_old: String = database
        .query_row("SELECT body FROM records WHERE id=?1", [&old.id], |r| {
            r.get(0)
        })
        .unwrap();
    let stored_new: String = database
        .query_row("SELECT body FROM records WHERE id=?1", [&new.id], |r| {
            r.get(0)
        })
        .unwrap();
    assert!(serde_json::from_str::<Value>(&stored_old)
        .unwrap()
        .get("_usageCodec")
        .is_none());
    assert_eq!(
        serde_json::from_str::<Value>(&stored_new).unwrap()["_usageCodec"],
        "zstd-compact-v2"
    );
    drop(database);

    assert_eq!(
        serde_json::to_value(store.detail(&old.id).unwrap()).unwrap(),
        serde_json::to_value(old_detail).unwrap()
    );
    assert_eq!(
        serde_json::to_value(store.detail(&new.id).unwrap()).unwrap(),
        serde_json::to_value(new_detail).unwrap()
    );
}

#[test]
fn body_dictionary_v1_hash_is_stable() {
    let dictionary = include_bytes!("body-dictionary-v1.json");
    assert_eq!(dictionary.len(), 3_890);
    assert_eq!(
        crate::storage::digest(dictionary),
        "0161fdb7a571680619646b87ca0665e235c055a56c5c3f9e616ab49f73414e8c"
    );
}

#[test]
fn mixed_legacy_dictionary_and_plain_bodies_migrate_to_dictionary_codec() {
    let dir = tempfile::tempdir().unwrap();
    let basis_a = json!({
        "input_cost_per_token": "0.01",
        "output_cost_per_token": "0.02",
        "fixture": "legacy-zstd"
    });
    let basis_b = json!({
        "input_cost_per_token": "0.03",
        "output_cost_per_token": "0.04",
        "fixture": "dictionary-zstd"
    });
    let mut legacy = record(
        "fixture-legacy-zstd-body",
        "proxy",
        "fixture-legacy-zstd-response",
        Some(price("0.42", basis_a)),
    );
    legacy.attempts[0].transport = "fixture-legacy-zstd-payload".repeat(400);
    let mut dictionary = record(
        "fixture-dictionary-zstd-body",
        "proxy",
        "fixture-dictionary-zstd-response",
        Some(price("0.24", basis_b)),
    );
    dictionary.attempts[0].transport = "fixture-dictionary-zstd-payload".repeat(400);
    let plain = record(
        "fixture-plain-body",
        "codex",
        "fixture-plain-response",
        None,
    );
    let rows = vec![legacy.clone(), dictionary.clone(), plain.clone()];

    let mut store = Store::open_v11_fixture(dir.path()).unwrap();
    store.write_batch(&rows, None).unwrap();
    let before_details = rows
        .iter()
        .map(|row| {
            (
                row.id.clone(),
                serde_json::to_value(store.detail(&row.id).unwrap()).unwrap(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let filter = Filter {
        source: Some("proxy".into()),
        status: Some("2xx".into()),
        ..Filter::default()
    };
    let before_overview =
        serde_json::to_value(store.dashboard(&Filter::default()).unwrap()).unwrap();
    let before_filter = serde_json::to_value(store.dashboard(&filter).unwrap()).unwrap();
    let before_ids = store
        .logs(&filter)
        .unwrap()
        .rows
        .into_iter()
        .map(|row| row.id)
        .collect::<Vec<_>>();
    drop(store);

    let database = Connection::open(dir.path().join("usage.sqlite")).unwrap();
    let legacy_body: String = database
        .query_row(
            "SELECT body FROM records WHERE id=?1",
            [&legacy.id],
            |row| row.get(0),
        )
        .unwrap();
    let current_legacy: Value = serde_json::from_str(&legacy_body).unwrap();
    assert_eq!(current_legacy["_usageCodec"], "zstd-compact-v2");
    let legacy_body = legacy_zstd_wrapper(&database, &legacy_body);
    database
        .execute(
            "UPDATE records SET body=?2 WHERE id=?1",
            params![&legacy.id, legacy_body],
        )
        .unwrap();
    let plain_body = serde_json::to_string(&before_details[&plain.id]).unwrap();
    database
        .execute(
            "UPDATE records SET body=?2 WHERE id=?1",
            params![&plain.id, plain_body],
        )
        .unwrap();
    let stored_legacy: String = database
        .query_row(
            "SELECT body FROM records WHERE id=?1",
            [&legacy.id],
            |row| row.get(0),
        )
        .unwrap();
    let stored_dictionary: String = database
        .query_row(
            "SELECT body FROM records WHERE id=?1",
            [&dictionary.id],
            |row| row.get(0),
        )
        .unwrap();
    let stored_plain: String = database
        .query_row("SELECT body FROM records WHERE id=?1", [&plain.id], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&stored_legacy).unwrap()["_usageCodec"],
        "zstd-v1"
    );
    assert_eq!(
        serde_json::from_str::<Value>(&stored_dictionary).unwrap()["_usageCodec"],
        "zstd-compact-v2"
    );
    assert!(serde_json::from_str::<Value>(&stored_plain)
        .unwrap()
        .get("_usageCodec")
        .is_none());
    drop(database);

    let mixed = Store::open_v11_fixture(dir.path()).unwrap();
    for row in &rows {
        assert_eq!(
            serde_json::to_value(mixed.detail(&row.id).unwrap()).unwrap(),
            before_details[&row.id]
        );
    }
    assert_eq!(
        serde_json::to_value(mixed.dashboard(&Filter::default()).unwrap()).unwrap(),
        before_overview
    );
    assert_eq!(
        serde_json::to_value(mixed.dashboard(&filter).unwrap()).unwrap(),
        before_filter
    );
    assert_eq!(
        mixed
            .logs(&filter)
            .unwrap()
            .rows
            .into_iter()
            .map(|row| row.id)
            .collect::<Vec<_>>(),
        before_ids
    );
    drop(mixed);

    downgrade_to_v8(dir.path(), false, true);
    let mut migrated = Store::open_v11_fixture(dir.path()).unwrap();
    complete_storage_migration(&mut migrated);
    for row in &rows {
        assert_eq!(
            serde_json::to_value(migrated.detail(&row.id).unwrap()).unwrap(),
            before_details[&row.id]
        );
    }
    assert_eq!(
        serde_json::to_value(migrated.dashboard(&Filter::default()).unwrap()).unwrap(),
        before_overview
    );
    assert_eq!(
        serde_json::to_value(migrated.dashboard(&filter).unwrap()).unwrap(),
        before_filter
    );
    assert_eq!(
        migrated
            .logs(&filter)
            .unwrap()
            .rows
            .into_iter()
            .map(|row| row.id)
            .collect::<Vec<_>>(),
        before_ids
    );
    drop(migrated);

    let database = Connection::open(dir.path().join("usage.sqlite")).unwrap();
    let migrated_body: String = database
        .query_row(
            "SELECT body FROM records WHERE id=?1",
            [&legacy.id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&migrated_body).unwrap()["_usageCodec"],
        "zstd-compact-v2"
    );
}

#[test]
fn compressed_projection_keeps_operation_source_status_and_unpriced_filters_consistent() {
    let dir = tempfile::tempdir().unwrap();
    let basis = json!({
        "input_cost_per_token": "0.01",
        "output_cost_per_token": "0.02",
        "fixture": "projection",
        "description": "x".repeat(1_800)
    });
    let mut model = record(
        "fixture-projection-model",
        "proxy",
        "fixture-projection-model-response",
        Some(price("0.1", basis.clone())),
    );
    model.attempts[0].transport = "fixture-projection-payload".repeat(300);
    model.attempts[0].tokens.input = Some(10);

    let mut search = record(
        "fixture-projection-search",
        "proxy",
        "fixture-projection-search-response",
        Some(price("0.1", basis.clone())),
    );
    search.attempts[0].operation = Operation::WebSearch;
    search.attempts[0].status = Some(201);
    search.attempts[0].tokens.input = Some(11);
    search.attempts[0].transport = "fixture-projection-payload".repeat(300);

    let mut compaction = record(
        "fixture-projection-compaction",
        "codex",
        "fixture-projection-compaction-response",
        None,
    );
    compaction.attempts[0].operation = Operation::Compaction;
    compaction.attempts[0].tokens.input = Some(12);
    compaction.attempts[0].status = None;
    compaction.attempts[0].transport = "fixture-projection-payload".repeat(300);
    let mut store = Store::open_v11_fixture(dir.path()).unwrap();
    store
        .write_batch(&[model, search, compaction], None)
        .unwrap();

    let all = store.dashboard(&Filter::default()).unwrap();
    assert_eq!(all.totals.requests, 3);
    assert_eq!(all.totals.tokens.input, Some(33));
    assert_eq!(all.totals.unpriced, 1);
    assert_eq!(decimal(&all.totals.cost), decimal("0.2"));
    assert_eq!(all.sources.get("proxy"), Some(&2));
    assert_eq!(all.sources.get("codex"), Some(&1));

    let proxy = store
        .dashboard(&Filter {
            source: Some("proxy".into()),
            ..Filter::default()
        })
        .unwrap();
    assert_eq!(proxy.totals.requests, 2);
    assert_eq!(proxy.totals.tokens.input, Some(21));
    assert_eq!(proxy.totals.unpriced, 0);
    assert_eq!(decimal(&proxy.totals.cost), decimal("0.2"));
    let sessions = store
        .dashboard(&Filter {
            source: Some("sessions".into()),
            ..Filter::default()
        })
        .unwrap();
    assert_eq!(sessions.totals.requests, 1);
    assert_eq!(sessions.totals.tokens.input, Some(12));
    assert_eq!(sessions.totals.unpriced, 1);
    assert_eq!(decimal(&sessions.totals.cost), decimal("0"));

    for (operation, requests, input, cost, unpriced) in [
        ("model", 1, 10, "0.1", 0),
        ("web_search", 1, 11, "0.1", 0),
        ("compaction", 1, 12, "0", 1),
    ] {
        let dashboard = store
            .dashboard(&Filter {
                operation: Some(operation.into()),
                ..Filter::default()
            })
            .unwrap();
        assert_eq!(dashboard.totals.requests, requests, "{operation}");
        assert_eq!(dashboard.totals.tokens.input, Some(input), "{operation}");
        assert_eq!(dashboard.totals.unpriced, unpriced, "{operation}");
        assert_eq!(
            decimal(&dashboard.totals.cost),
            decimal(cost),
            "{operation}"
        );
    }

    assert_eq!(
        store
            .logs(&Filter {
                source: Some("proxy".into()),
                status: Some("2xx".into()),
                ..Filter::default()
            })
            .unwrap()
            .total,
        2
    );
    assert_eq!(
        store
            .logs(&Filter {
                source: Some("sessions".into()),
                status: Some("none".into()),
                ..Filter::default()
            })
            .unwrap()
            .total,
        1
    );
    let search_page = store
        .logs(&Filter {
            operation: Some("web_search".into()),
            ..Filter::default()
        })
        .unwrap();
    assert_eq!(search_page.total, 1);
    assert_eq!(
        search_page.rows[0].attempts[0].operation,
        Operation::WebSearch
    );
    assert_eq!(
        store.detail("fixture-projection-search").unwrap().attempts[0]
            .price
            .as_ref()
            .unwrap()
            .basis,
        Some(basis)
    );
}

#[test]
fn new_database_uses_incremental_vacuum_and_eight_mib_wal_limit() {
    let dir = tempfile::tempdir().unwrap();
    let _store = Store::open(dir.path()).unwrap();
    let database = Connection::open(dir.path().join("usage.sqlite")).unwrap();
    super::maintenance::configure(&database).unwrap();
    let mode: i64 = database
        .query_row("PRAGMA auto_vacuum", [], |row| row.get(0))
        .unwrap();
    let limit: i64 = database
        .query_row("PRAGMA journal_size_limit", [], |row| row.get(0))
        .unwrap();
    assert_eq!(mode, 2, "incremental auto-vacuum is enabled for a new DB");
    assert_eq!(limit, WAL_LIMIT as i64);
}

#[test]
fn repeated_price_bases_are_interned_without_merging_distinct_old_bases() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open_v11_fixture(dir.path()).unwrap();
    let basis_a = json!({"input_cost_per_token":"0.01","fixture":"basis-a"});
    let basis_b = json!({"input_cost_per_token":"0.01","fixture":"basis-b"});
    let rows = [
        record(
            "fixture-basis-a-1",
            "codex",
            "fixture-response-a1",
            Some(price("0.1", basis_a.clone())),
        ),
        record(
            "fixture-basis-a-2",
            "codex",
            "fixture-response-a2",
            Some(price("0.1", basis_a.clone())),
        ),
        record(
            "fixture-basis-b",
            "codex",
            "fixture-response-b",
            Some(price("0.1", basis_b.clone())),
        ),
    ];
    store.write_batch(&rows, None).unwrap();

    let database = Connection::open(dir.path().join("usage.sqlite")).unwrap();
    let count: u64 = database
        .query_row("SELECT count(*) FROM price_bases", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 2);
    let stored: String = database
        .query_row(
            "SELECT body FROM records WHERE id='fixture-basis-a-1'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let stored = super::bodies::unpack(&database, &stored).unwrap();
    assert!(stored["attempts"][0]["price"].get("basisRef").is_some());
    assert!(stored["attempts"][0]["price"].get("basis").is_none());
    drop(database);

    for (id, expected) in [
        ("fixture-basis-a-1", basis_a.clone()),
        ("fixture-basis-a-2", basis_a),
        ("fixture-basis-b", basis_b),
    ] {
        let detail = store.detail(id).unwrap();
        assert_eq!(
            detail.attempts[0].price.as_ref().unwrap().basis,
            Some(expected)
        );
    }
    drop(store);
    let reopened = Store::open_v11_fixture(dir.path()).unwrap();
    assert_eq!(
        reopened.detail("fixture-basis-a-1").unwrap().attempts[0]
            .price
            .as_ref()
            .unwrap()
            .basis,
        Some(json!({"input_cost_per_token":"0.01","fixture":"basis-a"}))
    );
}

#[test]
fn v8_price_basis_migration_resumes_after_restart_and_preserves_records_prices_and_dedup() {
    let dir = tempfile::tempdir().unwrap();
    let basis_a = json!({"input_cost_per_token":"0.01","fixture":"basis-a"});
    let basis_b = json!({"input_cost_per_token":"0.01","fixture":"basis-b"});
    let mut rows = Vec::new();
    for index in 0..260 {
        let id = format!("fixture-v0212-{index:03}");
        let basis = if index % 2 == 0 {
            basis_a.clone()
        } else {
            basis_b.clone()
        };
        let response_id = if index < 2 {
            "fixture-v0212-duplicate-response".to_owned()
        } else {
            format!("fixture-v0212-response-{index:03}")
        };
        let mut row = record(&id, "codex", &response_id, Some(price("0.1", basis)));
        if index == 0 {
            row.source = "proxy".into();
            row.attempts[0].provider = Some("fixture-provider".into());
            row.attempts[0].status = Some(200);
            row.attempts[0].stream = true;
            row.attempts[0].first_token_ms = Some(123);
        } else {
            row.attempts[0].provider = None;
            row.attempts[0].status = None;
            row.attempts[0].stream = false;
            row.attempts[0].first_token_ms = None;
        }
        rows.push(row);
    }

    let mut store = Store::open_v11_fixture(dir.path()).unwrap();
    store.write_batch(&rows, None).unwrap();
    let before_summary = summary(&store);
    assert_eq!(
        before_summary.0, 259,
        "the exact response pair is deduplicated"
    );
    let before_gateway = serde_json::to_value(store.detail(&rows[0].id).unwrap()).unwrap();
    let before_session = serde_json::to_value(store.detail(&rows[1].id).unwrap()).unwrap();
    assert_eq!(
        store.detail(&rows[1].id).unwrap().duplicate_of.as_deref(),
        Some(rows[0].id.as_str())
    );
    drop(store);

    downgrade_to_v8(dir.path(), true, true);
    let database = Connection::open(dir.path().join("usage.sqlite")).unwrap();
    let old_mode: i64 = database
        .query_row("PRAGMA auto_vacuum", [], |row| row.get(0))
        .unwrap();
    assert_eq!(old_mode, 0);
    drop(database);

    let mut migrated = Store::open_v11_fixture(dir.path()).unwrap();
    assert_eq!(summary(&migrated), before_summary);
    assert_eq!(
        serde_json::to_value(migrated.detail(&rows[0].id).unwrap()).unwrap(),
        before_gateway
    );
    assert_eq!(
        serde_json::to_value(migrated.detail(&rows[1].id).unwrap()).unwrap(),
        before_session
    );
    assert!(migrated.compact_storage_batch().unwrap());
    drop(migrated);

    let mut resumed = Store::open_v11_fixture(dir.path()).unwrap();
    complete_storage_migration(&mut resumed);
    assert_eq!(summary(&resumed), before_summary);
    assert_eq!(
        serde_json::to_value(resumed.detail(&rows[0].id).unwrap()).unwrap(),
        before_gateway
    );
    assert_eq!(
        serde_json::to_value(resumed.detail(&rows[1].id).unwrap()).unwrap(),
        before_session
    );
    resumed.reclaim_space().unwrap();
    let database = Connection::open(dir.path().join("usage.sqlite")).unwrap();
    let mode: i64 = database
        .query_row("PRAGMA auto_vacuum", [], |row| row.get(0))
        .unwrap();
    let bases: u64 = database
        .query_row("SELECT count(*) FROM price_bases", [], |row| row.get(0))
        .unwrap();
    assert_eq!(mode, 2, "the old mode-0 database is atomically converted");
    assert_eq!(
        bases, 2,
        "identical old bases share storage; distinct bases remain separate"
    );
    drop(database);
    drop(resumed);

    let restarted = Store::open_v11_fixture(dir.path()).unwrap();
    assert_eq!(summary(&restarted), before_summary);
    assert_eq!(
        serde_json::to_value(restarted.detail(&rows[0].id).unwrap()).unwrap(),
        before_gateway
    );
    assert_eq!(
        serde_json::to_value(restarted.detail(&rows[1].id).unwrap()).unwrap(),
        before_session
    );
}

#[test]
fn damaged_basis_aborts_the_whole_compaction_batch_and_keeps_its_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open_v11_fixture(dir.path()).unwrap();
    store
        .write_batch(
            &[
                record(
                    "fixture-damaged-a",
                    "codex",
                    "fixture-response-damaged-a",
                    Some(price("0.1", json!({"fixture":"recoverable"}))),
                ),
                record(
                    "fixture-damaged-b",
                    "codex",
                    "fixture-response-damaged-b",
                    Some(price("0.2", json!({"fixture":"missing"}))),
                ),
            ],
            None,
        )
        .unwrap();
    drop(store);

    let database = Connection::open(dir.path().join("usage.sqlite")).unwrap();
    let first_before: String = database
        .query_row(
            "SELECT body FROM records WHERE id='fixture-damaged-a'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let first_ref = super::bodies::unpack(&database, &first_before).unwrap()["attempts"][0]
        ["price"]["basisRef"]
        .as_str()
        .unwrap()
        .to_owned();
    let first_basis_body: String = database
        .query_row(
            "SELECT body FROM price_bases WHERE hash=?1",
            [first_ref.clone()],
            |row| row.get::<_, String>(0),
        )
        .unwrap();
    let first_basis: Value = serde_json::from_str(&first_basis_body).unwrap();
    let mut first_inline = super::bodies::unpack(&database, &first_before).unwrap();
    let first_price = first_inline["attempts"][0]["price"]
        .as_object_mut()
        .unwrap();
    first_price.remove("basisRef");
    first_price.insert("basis".into(), first_basis);
    database
        .execute(
            "UPDATE records SET body=?1 WHERE id='fixture-damaged-a'",
            [serde_json::to_string(&first_inline).unwrap()],
        )
        .unwrap();
    let second_body: String = database
        .query_row(
            "SELECT body FROM records WHERE id='fixture-damaged-b'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let second = super::bodies::unpack(&database, &second_body).unwrap();
    let second_ref = second["attempts"][0]["price"]["basisRef"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_ne!(first_ref, second_ref);
    database.execute("DELETE FROM price_bases", []).unwrap();
    database
        .execute(
            "DELETE FROM metadata WHERE key IN ('storage_records_after','storage_daily_after','storage_repack')",
            [],
        )
        .unwrap();
    database
        .execute_batch(
            "ALTER TABLE records DROP COLUMN local_sequence;
             DELETE FROM metadata WHERE key IN ('record_sequence','finish_time_after');
             PRAGMA user_version=8;",
        )
        .unwrap();
    drop(database);

    let mut migrated = Store::open_v11_fixture(dir.path()).unwrap();
    let database = Connection::open(dir.path().join("usage.sqlite")).unwrap();
    let first_before_batch: String = database
        .query_row(
            "SELECT body FROM records WHERE id='fixture-damaged-a'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let cursor_before: String = database
        .query_row(
            "SELECT value FROM metadata WHERE key='storage_records_after'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(cursor_before, "");
    drop(database);

    assert!(migrated.compact_storage_batch().is_err());
    let database = Connection::open(dir.path().join("usage.sqlite")).unwrap();
    let first_after: String = database
        .query_row(
            "SELECT body FROM records WHERE id='fixture-damaged-a'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let bases: u64 = database
        .query_row("SELECT count(*) FROM price_bases", [], |row| row.get(0))
        .unwrap();
    let cursor_after: String = database
        .query_row(
            "SELECT value FROM metadata WHERE key='storage_records_after'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        first_after, first_before_batch,
        "prior row update rolled back"
    );
    assert_eq!(
        bases, 0,
        "the first basis insertion rolled back with the damaged row"
    );
    assert_eq!(
        cursor_after, cursor_before,
        "migration cursor did not advance"
    );
    assert!(!first_before.is_empty());
}

#[test]
fn reclaim_releases_pages_after_deleting_large_isolated_fixture_batch() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    complete_storage_migration(&mut store);
    store.reclaim_space().unwrap();
    let before = Connection::open(dir.path().join("usage.sqlite")).unwrap();
    let initial_pages: u64 = before
        .query_row("PRAGMA page_count", [], |row| row.get(0))
        .unwrap();
    drop(before);

    let large_transport = "fixture-storage-payload".repeat(180);
    let rows = (0..384)
        .map(|index| {
            let mut row = record(
                &format!("fixture-reclaim-{index:04}"),
                "codex",
                &format!("fixture-reclaim-response-{index:04}"),
                None,
            );
            row.attempts[0].transport = large_transport.clone();
            row
        })
        .collect::<Vec<_>>();
    store.write_batch(&rows, None).unwrap();
    store.rebuild("codex", &[], &[]).unwrap();
    let deleted = Connection::open(dir.path().join("usage.sqlite")).unwrap();
    let before_reclaim: u64 = deleted
        .query_row("PRAGMA page_count", [], |row| row.get(0))
        .unwrap();
    let free_pages: u64 = deleted
        .query_row("PRAGMA freelist_count", [], |row| row.get(0))
        .unwrap();
    assert!(before_reclaim > initial_pages);
    assert!(free_pages > 0, "deletion leaves pages available to reclaim");
    drop(deleted);

    store.reclaim_space().unwrap();
    let after = Connection::open(dir.path().join("usage.sqlite")).unwrap();
    let after_pages: u64 = after
        .query_row("PRAGMA page_count", [], |row| row.get(0))
        .unwrap();
    assert!(
        after_pages < before_reclaim,
        "incremental vacuum releases fixture pages"
    );
    assert_eq!(store.logs(&Filter::default()).unwrap().total, 0);
}

#[test]
fn live_reader_snapshot_does_not_block_reclaim_and_wal_truncates_after_reader_closes() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    complete_storage_migration(&mut store);
    store.reclaim_space().unwrap();
    drop(store);

    let path = dir.path().join("usage.sqlite");
    let setup = Connection::open(&path).unwrap();
    setup
        .execute_batch(
            "PRAGMA wal_checkpoint(TRUNCATE);
             CREATE TABLE fixture_wal(id INTEGER PRIMARY KEY,body TEXT NOT NULL);",
        )
        .unwrap();
    drop(setup);

    let reader = Connection::open(&path).unwrap();
    reader.execute_batch("BEGIN DEFERRED;").unwrap();
    let before_count: u64 = reader
        .query_row("SELECT count(*) FROM fixture_wal", [], |row| row.get(0))
        .unwrap();
    assert_eq!(before_count, 0);

    let writer = Connection::open(&path).unwrap();
    writer
        .execute_batch("PRAGMA wal_autocheckpoint=0; BEGIN IMMEDIATE;")
        .unwrap();
    {
        let mut insert = writer
            .prepare("INSERT INTO fixture_wal(body) VALUES(?1)")
            .unwrap();
        let payload = "fixture-wal-page".repeat(1_024);
        for _ in 0..640 {
            insert.execute([&payload]).unwrap();
        }
    }
    writer.execute_batch("COMMIT;").unwrap();
    assert!(wal_len(dir.path()) >= WAL_LIMIT);

    let store = Store::open(dir.path()).unwrap();
    let started = Instant::now();
    store.reclaim_space().unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "active reader must not block reclaim"
    );
    let stable_snapshot: u64 = reader
        .query_row("SELECT count(*) FROM fixture_wal", [], |row| row.get(0))
        .unwrap();
    assert_eq!(stable_snapshot, 0, "the reader keeps its original snapshot");
    assert!(
        wal_len(dir.path()) >= WAL_LIMIT,
        "a pinned WAL is left intact"
    );

    reader.execute_batch("ROLLBACK;").unwrap();
    drop(reader);
    store.reclaim_space().unwrap();
    let verify = Connection::open(&path).unwrap();
    let after_count: u64 = verify
        .query_row("SELECT count(*) FROM fixture_wal", [], |row| row.get(0))
        .unwrap();
    assert_eq!(after_count, 640);
    drop(verify);
    assert!(
        wal_len(dir.path()) < WAL_LIMIT,
        "checkpoint can truncate after the reader closes"
    );
    drop(writer);
}

#[test]
fn unchanged_cursor_is_a_noop_for_data_version_and_wal() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    complete_storage_migration(&mut store);
    store
        .write_batch(
            &[record(
                "fixture-cursor-version",
                "codex",
                "fixture-cursor-response",
                None,
            )],
            Some(("fixture-cursor", "fixture-cursor-v1")),
        )
        .unwrap();
    let database = Connection::open(dir.path().join("usage.sqlite")).unwrap();
    database
        .query_row::<(i64, i64, i64), _, _>("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .unwrap();
    drop(database);
    let before_version = store.logs(&Filter::default()).unwrap().data_version;
    let before_wal = wal_len(dir.path());

    store
        .write_batch(&[], Some(("fixture-cursor", "fixture-cursor-v1")))
        .unwrap();

    let after_version = store.logs(&Filter::default()).unwrap().data_version;
    let after_wal = wal_len(dir.path());
    assert_eq!(after_version, before_version);
    assert_eq!(
        after_wal, before_wal,
        "an unchanged cursor appends no WAL frames"
    );
}

#[test]
fn daily_archive_drops_transfer_metadata_and_keeps_cost_and_first_token_totals() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let mut row = record(
        "fixture-daily-transfer",
        "proxy",
        "fixture-daily-response",
        Some(price(
            "0.42",
            json!({"input_cost_per_token":"0.01","fixture":"daily"}),
        )),
    );
    row.attempts[0].transfer = Some(TransferMetadata {
        sent_at: BASE,
        received_at: Some(BASE - 10),
        upstream_bytes: 1234,
        downstream_bytes: 5678,
        response_hash: Some("fixture-response-hash".into()),
        ..TransferMetadata::default()
    });
    store.write_batch(&[row], None).unwrap();
    store.compact(BASE + 40 * DAY).unwrap();

    let database = Connection::open(dir.path().join("usage.sqlite")).unwrap();
    let body: String = database
        .query_row("SELECT body FROM daily", [], |row| row.get(0))
        .unwrap();
    let body = super::bodies::unpack(&database, &body).unwrap();
    assert!(body["example"]["attempts"][0].get("transfer").is_none());
    drop(database);

    let totals = store.dashboard(&Filter::default()).unwrap().totals;
    assert_eq!(totals.requests, 1);
    assert_eq!(totals.first_token_sum_ms, 123);
    assert_eq!(totals.first_token_samples, 1);
    assert_eq!(totals.cost, "0.42");
    assert_eq!(totals.tokens.input, Some(10));
    assert_eq!(totals.tokens.output, Some(2));
}

#[test]
fn zstd_0133_saved_bodies_remain_readable_after_dependency_upgrade() {
    // Fixed synthetic frames generated by zstd 0.13.3 with checksums, not the current encoder.
    let database = Connection::open_in_memory().unwrap();
    let expected = json!({
        "fixture":"legacy-zstd-0.13.3",
        "attempts":[{"transport":"http","status":200,"upstream_error":"fixture ".repeat(100)}]
    });
    for (codec, packed) in [
        ("zstd-v1", "KLUv/QRYBQMAMgYVGoBL2igOgZKItfee9/z8YoLAi6IkkAcpEnJ6zaAxYtZFFQnKoX5z+SgVMO/6fX2cf8Lm65LLOL0ZgXmfCv7n4yQlgKSQ4ftAjsIhcGEz6npDcTATAgAVV1U8kAhmIKND1Q=="),
        ("zstd-dict-v1", "KLUv/QRY9QEAJAJ7ImZpeHR1cmxlZ2FjeS16c3RkLTAuMTMuMyIsZSAifV19CAAVV1XAQKCMGDdlLoKxHDCzEiDNLMywigsgo0PV"),
    ] {
        let body = json!({"_usageCodec":codec,"_usageBody":packed}).to_string();
        assert_eq!(super::bodies::unpack(&database,&body).unwrap(),expected);
    }
}
