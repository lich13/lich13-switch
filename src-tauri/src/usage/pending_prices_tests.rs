use super::{
    model::{Attempt, PriceSnapshot, Record, Tokens},
    pending_prices,
    store::Store,
};
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use std::collections::BTreeMap;

fn database() -> Connection {
    let connection = Connection::open_in_memory().unwrap();
    connection
        .execute_batch("CREATE TABLE records(id TEXT PRIMARY KEY, body TEXT NOT NULL);")
        .unwrap();
    connection
}

fn body(price: Option<Value>) -> String {
    json!({ "attempts": [{ "price": price }] }).to_string()
}

fn pending_ids(connection: &Connection) -> Vec<String> {
    let mut statement = connection
        .prepare("SELECT id FROM pending_prices ORDER BY id")
        .unwrap();
    statement
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
}

#[test]
fn ensure_scans_legacy_records_without_changing_the_schema_version() {
    let connection = database();
    connection.pragma_update(None, "user_version", 7).unwrap();
    connection
        .execute(
            "INSERT INTO records(id,body) VALUES(?1,?2)",
            params!["legacy-unpriced", body(None)],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO records(id,body) VALUES(?1,?2)",
            params!["legacy-priced", body(Some(json!({ "cost": "1" })))],
        )
        .unwrap();

    pending_prices::ensure(&connection).unwrap();

    assert_eq!(pending_ids(&connection), ["legacy-unpriced"]);
    let version: i64 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, 7);
}

#[test]
fn triggers_track_insert_update_id_delete_and_replace_inside_one_transaction() {
    let mut connection = database();
    pending_prices::ensure(&connection).unwrap();
    let transaction = connection.transaction().unwrap();

    transaction
        .execute(
            "INSERT INTO records(id,body) VALUES(?1,?2)",
            params!["inserted", body(None)],
        )
        .unwrap();
    assert_eq!(pending_ids(&transaction), ["inserted"]);

    transaction
        .execute(
            "INSERT INTO records(id,body) VALUES(?1,?2)",
            params!["priced", body(Some(json!({ "cost": "1" })))],
        )
        .unwrap();
    assert_eq!(pending_ids(&transaction), ["inserted"]);

    transaction
        .execute(
            "UPDATE records SET body=?2 WHERE id=?1",
            params!["inserted", body(Some(json!({ "cost": "2" })))],
        )
        .unwrap();
    assert!(pending_ids(&transaction).is_empty());

    transaction
        .execute(
            "UPDATE records SET body=?2 WHERE id=?1",
            params!["inserted", body(None)],
        )
        .unwrap();
    transaction
        .execute(
            "UPDATE records SET id=?2 WHERE id=?1",
            params!["inserted", "renamed"],
        )
        .unwrap();
    assert_eq!(pending_ids(&transaction), ["renamed"]);

    transaction
        .execute("DELETE FROM records WHERE id='renamed'", [])
        .unwrap();
    assert!(pending_ids(&transaction).is_empty());

    transaction
        .execute(
            "INSERT OR REPLACE INTO records(id,body) VALUES(?1,?2)",
            params!["replacement", body(None)],
        )
        .unwrap();
    assert_eq!(pending_ids(&transaction), ["replacement"]);
    transaction
        .execute(
            "INSERT OR REPLACE INTO records(id,body) VALUES(?1,?2)",
            params!["replacement", body(Some(json!({ "cost": "3" })))],
        )
        .unwrap();
    assert!(pending_ids(&transaction).is_empty());
    transaction
        .execute(
            "INSERT OR REPLACE INTO records(id,body) VALUES(?1,?2)",
            params!["replacement", body(None)],
        )
        .unwrap();
    assert_eq!(pending_ids(&transaction), ["replacement"]);

    transaction.commit().unwrap();
}

#[test]
fn rolling_back_record_changes_also_rolls_back_the_pending_index() {
    let mut connection = database();
    pending_prices::ensure(&connection).unwrap();
    connection
        .execute(
            "INSERT INTO records(id,body) VALUES(?1,?2)",
            params!["stable", body(None)],
        )
        .unwrap();

    let transaction = connection.transaction().unwrap();
    transaction
        .execute(
            "UPDATE records SET body=?2 WHERE id=?1",
            params!["stable", body(Some(json!({ "cost": "4" })))],
        )
        .unwrap();
    transaction
        .execute(
            "INSERT INTO records(id,body) VALUES(?1,?2)",
            params!["rolled-back", body(None)],
        )
        .unwrap();
    assert_eq!(pending_ids(&transaction), ["rolled-back"]);
    transaction.rollback().unwrap();

    assert_eq!(pending_ids(&connection), ["stable"]);
    let stored: String = connection
        .query_row("SELECT body FROM records WHERE id='stable'", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&stored).unwrap()["attempts"][0]["price"],
        Value::Null
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT count(*) FROM records WHERE id='rolled-back'",
                [],
                |row| { row.get::<_, i64>(0) }
            )
            .unwrap(),
        0
    );
}

#[test]
fn repeated_ensure_does_not_rebuild_a_complete_index_and_missing_table_is_rebuilt() {
    let connection = database();
    connection
        .execute(
            "INSERT INTO records(id,body) VALUES(?1,?2)",
            params!["legacy", body(None)],
        )
        .unwrap();
    pending_prices::ensure(&connection).unwrap();
    connection
        .execute("DELETE FROM pending_prices WHERE id='legacy'", [])
        .unwrap();

    pending_prices::ensure(&connection).unwrap();
    assert!(pending_ids(&connection).is_empty());

    connection
        .execute_batch("DROP TABLE pending_prices;")
        .unwrap();
    pending_prices::ensure(&connection).unwrap();
    assert_eq!(pending_ids(&connection), ["legacy"]);
    let installed: i64 = connection
        .query_row(
            "SELECT count(*) FROM sqlite_schema WHERE (type='table' AND name='pending_prices') OR (type='trigger' AND name IN ('pending_prices_insert','pending_prices_update','pending_prices_delete'))",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(installed, 4);
}

#[test]
fn pending_select_scans_pending_ids_then_searches_the_records_primary_key() {
    let connection = database();
    pending_prices::ensure(&connection).unwrap();

    let sql = format!("EXPLAIN QUERY PLAN {}", pending_prices::SELECT);
    let mut statement = connection.prepare(&sql).unwrap();
    let details = statement
        .query_map([], |row| row.get::<_, String>(3))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();

    assert!(
        details.iter().any(|detail| detail == "SCAN p"),
        "{details:?}"
    );
    assert!(
        details
            .iter()
            .any(|detail| detail.contains("SEARCH r USING INDEX sqlite_autoindex_records_1 (id=?)")),
        "{details:?}"
    );
}

fn compressed_record(id: &str, priced: bool) -> Record {
    Record {
        id: id.into(),
        client: "codex".into(),
        source: "proxy".into(),
        started_at: 1_760_000_000_000,
        completed: true,
        attempts: vec![Attempt {
            id: format!("{id}-attempt"),
            provider: Some("fixture-provider".into()),
            requested_model: Some("fixture-model".into()),
            response_model: Some("fixture-model".into()),
            pricing_model: Some("fixture-model".into()),
            tokens: Tokens {
                input: Some(10),
                output: Some(2),
                ..Tokens::default()
            },
            outcome: "completed".into(),
            transport: "fixture-transport-chunk;".repeat(300),
            price: priced.then(|| PriceSnapshot {
                version: "fixture-v1".into(),
                source: "fixed".into(),
                model: "fixture-model".into(),
                multiplier: "2".into(),
                rates: BTreeMap::new(),
                cost: "1".into(),
                basis: None,
            }),
            ..Attempt::default()
        }],
        ..Record::default()
    }
}

#[test]
fn store_inserted_compressed_json_keeps_an_indexable_price_projection() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    store
        .write_batch(
            &[
                compressed_record("compressed-unpriced", false),
                compressed_record("compressed-priced", true),
            ],
            None,
        )
        .unwrap();
    drop(store);

    let connection = Connection::open(dir.path().join("usage.sqlite")).unwrap();
    let mut statement = connection
        .prepare("SELECT id,body FROM records WHERE id LIKE 'compressed-%' ORDER BY id")
        .unwrap();
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(rows.len(), 2);
    for (_, stored) in &rows {
        assert_eq!(
            serde_json::from_str::<Value>(stored).unwrap()["_usageCodec"],
            "zstd-dict-v1"
        );
    }
    assert_eq!(pending_ids_for_store(&connection), ["compressed-unpriced"]);
}

fn pending_ids_for_store(connection: &Connection) -> Vec<String> {
    let mut statement = connection
        .prepare("SELECT id FROM pending_prices ORDER BY id")
        .unwrap();
    statement
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
}
