//! Focused regression coverage for exact JSON numbers and the v13 storage guard.
//!
//! All databases in this module live below `tempfile`; no user usage database is
//! opened or copied.
use super::{body_wire, model::*, storage_v12, store::Store};
use rusqlite::Connection;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, path::Path};

fn number(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or_else(|error| panic!("invalid fixture {text}: {error}"))
}

fn number_signature(value: &Value) -> (String, bool, bool, bool) {
    let number = value.as_number().expect("fixture must be a JSON number");
    (
        number.to_string(),
        number.as_u64().is_some(),
        number.as_i64().is_some(),
        number.as_f64().is_some_and(f64::is_sign_negative),
    )
}

fn migrate_to_ready(store: &mut Store) {
    for _ in 0..128 {
        if !store.compact_storage_batch().unwrap() {
            store.reclaim_space().unwrap();
            return;
        }
    }
    panic!("v13 storage migration did not finish");
}

fn user_version(path: &Path) -> i64 {
    Connection::open(path.join("usage.sqlite"))
        .unwrap()
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap()
}

fn storage_state(path: &Path) -> Option<String> {
    Connection::open(path.join("usage.sqlite"))
        .unwrap()
        .query_row(
            "SELECT value FROM metadata WHERE key='storage_v12'",
            [],
            |row| row.get(0),
        )
        .ok()
}

fn attempt(id: &str, price: Option<PriceSnapshot>) -> Attempt {
    Attempt {
        id: id.into(),
        provider: Some("fixture-provider".into()),
        requested_model: Some("fixture-model".into()),
        response_model: Some("fixture-model".into()),
        pricing_model: Some("fixture-model".into()),
        response_id: Some(format!("{id}-response")),
        status: Some(200),
        outcome: "success".into(),
        started_at: 1_700_000_000_000,
        duration_ms: 100,
        first_token_ms: Some(10),
        stream: true,
        transport: "http".into(),
        tokens: Tokens {
            input: Some(10),
            output: Some(2),
            ..Tokens::default()
        },
        price,
        ..Attempt::default()
    }
}

fn record(id: &str, price: Option<PriceSnapshot>) -> Record {
    Record {
        id: id.into(),
        client: "codex".into(),
        source: "proxy".into(),
        started_at: 1_700_000_000_000,
        finished_at: Some(1_700_000_000_100),
        completed: true,
        attempts: vec![attempt(&format!("{id}-attempt"), price)],
        ..Record::default()
    }
}

fn priced_record(id: &str) -> Record {
    let basis = number(
        r#"{
        "input_cost_per_token": 0.000001000000000000000001,
        "output_cost_per_token": 1e-8,
        "cache_read_cost_per_token": -0.0,
        "large_exponent": 1e+300
    }"#,
    );
    let price = PriceSnapshot {
        version: "fixture-v13-price".into(),
        source: "fixture".into(),
        model: "fixture-model".into(),
        multiplier: "1.00".into(),
        rates: BTreeMap::from([
            ("input".into(), "0.000001".into()),
            ("output".into(), "0.00000001".into()),
        ]),
        cost: "0.000001000000000000000001".into(),
        basis: Some(basis),
    };
    record(id, Some(price))
}

#[test]
fn v13_preserves_decimal_text_type_and_negative_zero() {
    for text in [
        "0.000001",
        "1e-6",
        "1.00",
        "-0.0",
        "1.234567890123456789012345678901234567890123456789",
        "1e+300",
        "1e-300",
        "1e+1000",
        "1e-1000",
    ] {
        let original = number(text);
        let encoded = body_wire::encode_v13(&original);
        assert_eq!(encoded.first(), Some(&11), "tag for {text}");
        let decoded = body_wire::decode(&encoded).expect("v13 number must decode");
        assert_eq!(decoded, original, "value changed for {text}");
        assert_eq!(
            number_signature(&decoded),
            number_signature(&original),
            "number text/type changed for {text}"
        );
    }
}

#[test]
fn legacy_encoders_keep_their_existing_bytes() {
    assert_eq!(body_wire::encode(&Value::Null), vec![0]);
    assert_eq!(body_wire::encode(&Value::Bool(false)), vec![1]);
    assert_eq!(body_wire::encode(&Value::Bool(true)), vec![2]);
    assert_eq!(body_wire::encode(&number("300")), vec![3, 0xac, 0x02]);
    assert_eq!(body_wire::encode(&number("-300")), vec![4, 0xac, 0x02]);
    assert_eq!(
        body_wire::encode(&Value::String("hello".into())),
        b"\x06\x05hello"
    );

    let decimal = number("0.000001");
    let mut old_decimal = vec![5];
    old_decimal.extend_from_slice(&decimal.as_number().unwrap().as_f64().unwrap().to_le_bytes());
    assert_eq!(body_wire::encode(&decimal), old_decimal);
    assert_eq!(body_wire::encode_v12(&decimal), old_decimal);
    assert_ne!(body_wire::decode(&old_decimal).unwrap(), decimal);

    let uuid = Value::String("00000000-0000-4000-8000-000000000000".into());
    let mut v12_uuid = vec![10];
    v12_uuid.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0x40, 0, 0x80, 0, 0, 0, 0, 0, 0, 0]);
    assert_eq!(body_wire::encode_v12(&uuid), v12_uuid);
    assert_eq!(
        body_wire::encode(&uuid),
        [6, 36]
            .into_iter()
            .chain(uuid.as_str().unwrap().bytes())
            .collect::<Vec<_>>()
    );
}

#[test]
fn v13_rejects_invalid_numbers_unknown_tags_and_excessive_depth() {
    for text in ["", "01", "+1", "1.", "1e", "NaN", "Infinity", "1 2", "0x1"] {
        let mut bytes = vec![11, text.len() as u8];
        bytes.extend_from_slice(text.as_bytes());
        assert!(
            body_wire::decode(&bytes).is_none(),
            "accepted invalid number {text:?}"
        );
    }
    for bytes in [
        vec![12],
        vec![11, 1],
        vec![10, 0],
        vec![9, 1, 255, 1, 0],
        vec![9, 1, 1, 12],
    ] {
        assert!(
            body_wire::decode(&bytes).is_none(),
            "accepted malformed wire {bytes:?}"
        );
    }

    let mut nested = Vec::new();
    for _ in 0..129 {
        nested.extend_from_slice(&[8, 1]);
    }
    nested.push(0);
    assert!(
        body_wire::decode(&nested).is_none(),
        "depth limit was bypassed"
    );
}

#[test]
fn empty_database_installs_and_reenters_v13_without_downgrade() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    assert_eq!(user_version(dir.path()), 13);
    migrate_to_ready(&mut store);
    drop(store);
    assert_eq!(user_version(dir.path()), 13);
    assert_eq!(storage_state(dir.path()).as_deref(), Some("ready"));

    let mut reopened = Store::open(dir.path()).unwrap();
    assert_eq!(user_version(dir.path()), 13);
    assert!(!reopened.compact_storage_batch().unwrap());
    drop(reopened);
    assert_eq!(user_version(dir.path()), 13);
}

#[test]
fn v11_pending_and_ready_snapshots_survive_v13_guard_and_reentry() {
    let dir = tempfile::tempdir().unwrap();
    let mut old = Store::open_v11_fixture(dir.path()).unwrap();
    let source = priced_record("fixture-v13-migration");
    old.write_batch(std::slice::from_ref(&source), None)
        .unwrap();
    let before = serde_json::to_value(old.detail(&source.id).unwrap()).unwrap();
    drop(old);

    let pending = Store::open(dir.path()).unwrap();
    assert_eq!(user_version(dir.path()), 13);
    assert_eq!(storage_state(dir.path()).as_deref(), Some("copy"));
    assert_eq!(
        serde_json::to_value(pending.detail(&source.id).unwrap()).unwrap(),
        before
    );
    drop(pending);

    let c = Connection::open(dir.path().join("usage.sqlite")).unwrap();
    c.pragma_update(None, "user_version", 12).unwrap();
    drop(c);
    let mut resumed = Store::open(dir.path()).unwrap();
    assert_eq!(user_version(dir.path()), 13);
    assert_eq!(storage_state(dir.path()).as_deref(), Some("copy"));
    migrate_to_ready(&mut resumed);
    assert_eq!(user_version(dir.path()), 13);
    assert_eq!(storage_state(dir.path()).as_deref(), Some("ready"));
    assert_eq!(
        serde_json::to_value(resumed.detail(&source.id).unwrap()).unwrap(),
        before
    );
    drop(resumed);

    let ready_v12 = Store::open(dir.path()).unwrap();
    drop(ready_v12);
    let c = Connection::open(dir.path().join("usage.sqlite")).unwrap();
    c.pragma_update(None, "user_version", 12).unwrap();
    drop(c);
    let reopened = Store::open(dir.path()).unwrap();
    assert_eq!(user_version(dir.path()), 13);
    assert_eq!(
        serde_json::to_value(reopened.detail(&source.id).unwrap()).unwrap(),
        before
    );
}

fn has_price_basis(value: &Value) -> bool {
    match value {
        Value::Object(fields) => {
            fields
                .get("price")
                .and_then(Value::as_object)
                .is_some_and(|price| price.contains_key("basis"))
                || fields.values().any(has_price_basis)
        }
        Value::Array(values) => values.iter().any(has_price_basis),
        _ => false,
    }
}

fn replace_price_shape_with_v12(path: &Path) -> bool {
    let c = Connection::open(path.join("usage.sqlite")).unwrap();
    storage_v12::register(&c).unwrap();
    let mut statement = c.prepare("SELECT id,value FROM v12_dictionary").unwrap();
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?))
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    for (id, stored) in rows {
        if stored.len() < 32 || Sha256::digest(&stored[32..])[..] != stored[..32] {
            continue;
        }
        let Some(value) = body_wire::decode(&stored[32..]) else {
            continue;
        };
        if !has_price_basis(&value) {
            continue;
        }
        let legacy = body_wire::encode_v12(&value);
        let mut replacement = Sha256::digest(&legacy).to_vec();
        replacement.extend(legacy);
        c.execute(
            "UPDATE v12_dictionary SET value=?2 WHERE id=?1",
            rusqlite::params![id, replacement],
        )
        .unwrap();
        return true;
    }
    false
}

#[test]
fn active_v12_reader_accepts_legacy_tag5_shared_value_with_valid_checksum() {
    let dir = tempfile::tempdir().unwrap();
    let mut old = Store::open_v11_fixture(dir.path()).unwrap();
    let source = priced_record("fixture-v13-legacy-shared");
    old.write_batch(std::slice::from_ref(&source), None)
        .unwrap();
    drop(old);
    let mut store = Store::open(dir.path()).unwrap();
    migrate_to_ready(&mut store);
    drop(store);

    assert!(replace_price_shape_with_v12(dir.path()));
    let reopened = Store::open(dir.path()).unwrap();
    let loaded = reopened.detail(&source.id).unwrap();
    let basis = loaded.attempts[0]
        .price
        .as_ref()
        .and_then(|price| price.basis.as_ref())
        .expect("legacy shared value kept the pricing basis");
    assert!(basis["input_cost_per_token"].is_number());
    assert_eq!(
        loaded.attempts[0].price.as_ref().unwrap().cost,
        source.attempts[0].price.as_ref().unwrap().cost
    );
    assert_eq!(user_version(dir.path()), 13);
}

#[test]
fn pending_copy_rolls_back_on_corruption_and_can_resume_with_same_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let mut old = Store::open_v11_fixture(dir.path()).unwrap();
    let source = record("fixture-v13-rollback", None);
    old.write_batch(std::slice::from_ref(&source), None)
        .unwrap();
    let before = serde_json::to_value(old.detail(&source.id).unwrap()).unwrap();
    drop(old);

    let c = Connection::open(dir.path().join("usage.sqlite")).unwrap();
    let body: String = c
        .query_row(
            "SELECT body FROM records WHERE id=?1",
            [&source.id],
            |row| row.get(0),
        )
        .unwrap();
    c.execute("UPDATE records SET body='{}' WHERE id=?1", [&source.id])
        .unwrap();
    drop(c);

    let mut pending = Store::open(dir.path()).unwrap();
    assert!(pending.compact_storage_batch().is_err());
    drop(pending);
    assert_eq!(storage_state(dir.path()).as_deref(), Some("copy"));

    let c = Connection::open(dir.path().join("usage.sqlite")).unwrap();
    c.execute(
        "UPDATE records SET body=?2 WHERE id=?1",
        rusqlite::params![source.id, body],
    )
    .unwrap();
    drop(c);
    let mut resumed = Store::open(dir.path()).unwrap();
    migrate_to_ready(&mut resumed);
    assert_eq!(
        serde_json::to_value(resumed.detail(&source.id).unwrap()).unwrap(),
        before
    );
}

#[test]
fn future_database_versions_remain_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    drop(store);
    let c = Connection::open(dir.path().join("usage.sqlite")).unwrap();
    c.pragma_update(None, "user_version", 14).unwrap();
    drop(c);
    assert!(Store::open(dir.path()).is_err());
    assert_eq!(user_version(dir.path()), 14);
}
