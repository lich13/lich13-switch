use super::{model::*, store::Store};
use base64::{engine::general_purpose::STANDARD, Engine};
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
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

fn capacity_record(index: u64, codex_count: u64, shared_basis: &Value) -> Record {
    let source = if index < codex_count { "codex" } else { "proxy" };
    let operation = match index % 10 {
        8 => Operation::WebSearch,
        9 => Operation::Compaction,
        _ => Operation::Model,
    };
    let input = 100 + index;
    let output = 2 + index % 17;
    let hash = |kind: &str| {
        crate::storage::digest(format!("fixture-v0212-capacity-{kind}-{index:06}").as_bytes())
    };
    let response_id = hash("response");
    let mut row = record(
        &hash("record"),
        source,
        &response_id,
        Some(price("0.003", shared_basis.clone())),
    );
    row.session_id = Some(hash("session"));
    row.started_at = BASE + (index / 4) as i64 * 1000;
    row.attempts[0].id = hash("attempt");
    row.attempts[0].started_at = row.started_at;
    row.attempts[0].operation = operation;
    row.attempts[0].tokens.input = Some(input);
    row.attempts[0].tokens.output = Some(output);
    row.attempts[0].tokens.inclusive_input = Some(input);
    row.attempts[0].transport = "http".into();
    if source == "proxy" {
        row.attempts[0].thread_hash = row.session_id.clone();
        row.attempts[0].upstream_request_hash = Some(hash("upstream"));
        row.attempts[0].transfer = Some(TransferMetadata {
            received_at: Some(row.started_at - 5),
            sent_at: row.started_at,
            first_byte_at: Some(row.started_at + 10),
            terminal_at: Some(row.started_at + 900),
            ended_at: row.started_at + 1000,
            upstream_bytes: 1000 + index % 1000,
            downstream_bytes: 1000 + index % 1000,
            terminal_handed_off: true,
            upstream_eof: true,
            client_request_hash: Some(hash("client")),
            response_hash: Some(response_id),
            ..TransferMetadata::default()
        });
        row.attempts[0].status = Some(200);
        row.attempts[0].first_token_ms = (operation == Operation::Model).then_some(80 + index % 13);
        row.attempts[0].stream = operation == Operation::Model;
    } else {
        row.attempts[0].provider = None;
        row.attempts[0].status = None;
        row.attempts[0].first_token_ms = None;
        row.attempts[0].stream = false;
        row.attempts[0].transport = "session".into();
    }
    row
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

#[derive(Default)]
struct SourceStorageProfile {
    count: u64,
    body_bytes: u64,
    projection_bytes: u64,
    raw_bytes: u64,
    base64_bytes: u64,
    compressed_bytes: u64,
}

impl SourceStorageProfile {
    fn average(value: u64, count: u64) -> u64 {
        value.checked_div(count).unwrap_or_default()
    }

    fn body_avg(&self) -> u64 {
        Self::average(self.body_bytes, self.count)
    }

    fn projection_avg(&self) -> u64 {
        Self::average(self.projection_bytes, self.count)
    }

    fn raw_avg(&self) -> u64 {
        Self::average(self.raw_bytes, self.count)
    }

    fn base64_avg(&self) -> u64 {
        Self::average(self.base64_bytes, self.count)
    }

    fn compressed_avg(&self) -> u64 {
        Self::average(self.compressed_bytes, self.count)
    }
}

fn source_storage_profiles(database: &Connection) -> BTreeMap<String, SourceStorageProfile> {
    let mut statement = database
        .prepare("SELECT source,body FROM records ORDER BY source,id")
        .unwrap();
    let rows = statement
        .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))
        .unwrap();
    let mut profiles: BTreeMap<String, SourceStorageProfile> = BTreeMap::new();
    for row in rows {
        let (source, body) = row.unwrap();
        let stored: Value = serde_json::from_str(&body).unwrap();
        let raw = super::bodies::unpack(&body).unwrap();
        let raw_bytes = serde_json::to_vec(&raw).unwrap().len() as u64;
        let mut projection = stored.clone();
        let (base64_bytes, compressed_bytes) = if let Some(object) = projection.as_object_mut() {
            let encoded = object
                .get("_usageBody")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let encoded_len = encoded.len() as u64;
            let packed = STANDARD.decode(encoded).unwrap_or_default();
            let packed_len = packed.len() as u64;
            object.remove("_usageBody");
            (encoded_len, packed_len)
        } else {
            (0, 0)
        };
        let projection_bytes = serde_json::to_vec(&projection).unwrap().len() as u64;
        let profile = profiles.entry(source).or_default();
        profile.count += 1;
        profile.body_bytes += body.len() as u64;
        profile.projection_bytes += projection_bytes;
        profile.raw_bytes += raw_bytes;
        profile.base64_bytes += base64_bytes;
        profile.compressed_bytes += compressed_bytes;
    }
    profiles
}

#[derive(Default)]
struct PageStorageProfile {
    records: u64,
    record_indexes: u64,
    metrics: u64,
    other: u64,
}

fn dbstat_storage_profile(database: &Connection) -> Option<PageStorageProfile> {
    let mut statement = database
        .prepare("SELECT name,sum(pgsize) FROM dbstat GROUP BY name")
        .ok()?;
    let rows = statement
        .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)))
        .ok()?;
    let mut profile = PageStorageProfile::default();
    for row in rows {
        let (name, bytes) = row.ok()?;
        let bytes = bytes.max(0) as u64;
        if name == "records" {
            profile.records += bytes;
        } else if name == "usage_metrics" || name.starts_with("sqlite_autoindex_usage_metrics") {
            profile.metrics += bytes;
        } else if name == "sqlite_autoindex_records_1" || name.starts_with("usage_") {
            profile.record_indexes += bytes;
        } else {
            profile.other += bytes;
        }
    }
    Some(profile)
}

fn scalar_u64(database: &Connection, sql: &str) -> u64 {
    database
        .query_row(sql, [], |row| row.get::<_, i64>(0))
        .unwrap()
        .max(0) as u64
}

fn length_storage_profile(database: &Connection) -> PageStorageProfile {
    PageStorageProfile {
        records: scalar_u64(database, "SELECT coalesce(sum(length(body)),0) FROM records"),
        record_indexes: scalar_u64(
            database,
            "SELECT coalesce(sum(length(id)+length(client)+length(source)+length(time)+coalesce(length(provider),0)+coalesce(length(model),0)+coalesce(length(status),0)+coalesce(length(response),0)+coalesce(length(signature),0)),0) FROM records",
        ),
        metrics: scalar_u64(
            database,
            "SELECT coalesce(sum(length(kind)+length(owner)+length(ordinal)+coalesce(length(provider),0)+coalesce(length(model),0)+length(requests)+length(attempts)+length(cost)),0) FROM usage_metrics",
        ),
        other: scalar_u64(database, "SELECT coalesce(sum(length(body)),0) FROM daily")
            + scalar_u64(database, "SELECT coalesce(sum(length(body)),0) FROM price_bases"),
    }
}

fn canonical_value(value: &Value) -> Value {
    match value {
        Value::Object(object) => {
            let mut keys = object.keys().collect::<Vec<_>>();
            keys.sort_unstable();
            let mut sorted = serde_json::Map::new();
            for key in keys {
                sorted.insert(key.clone(), canonical_value(object.get(key).unwrap()));
            }
            Value::Object(sorted)
        }
        Value::Array(items) => Value::Array(items.iter().map(canonical_value).collect()),
        other => other.clone(),
    }
}

fn replace_price_bases(value: &mut Value) {
    if let Some(attempts) = value.get_mut("attempts").and_then(Value::as_array_mut) {
        for attempt in attempts {
            if let Some(price) = attempt.get_mut("price").filter(|price| price.is_object()) {
                let Some(basis) = price.get("basis").cloned() else {
                    continue;
                };
                let body = serde_json::to_string(&basis).unwrap();
                let object = price.as_object_mut().unwrap();
                object.remove("basis");
                object.insert(
                    "basisRef".into(),
                    Value::String(crate::storage::digest(body.as_bytes())),
                );
            }
        }
    }
}

fn dictionary_fixture_samples() -> Vec<Vec<u8>> {
    let basis = json!({
        "input_cost_per_token": "0.001",
        "output_cost_per_token": "0.002",
        "fixture": "v0212-body-dictionary"
    });
    let mut codex = record(
        "fixture-dictionary-codex",
        "codex",
        "fixture-dictionary-response-codex",
        Some(price("0.003", basis.clone())),
    );
    codex.session_id = Some("fixture-dictionary-session-codex".into());
    codex.attempts[0].id = "fixture-dictionary-attempt-codex".into();
    codex.attempts[0].transfer = Some(TransferMetadata {
        sent_at: BASE,
        received_at: Some(BASE + 5),
        upstream_bytes: 100,
        downstream_bytes: 200,
        response_hash: Some("fixture-dictionary-transfer-codex".into()),
        ..TransferMetadata::default()
    });
    let mut proxy = record(
        "fixture-dictionary-proxy",
        "proxy",
        "fixture-dictionary-response-proxy",
        Some(price("0.003", basis)),
    );
    proxy.session_id = Some("fixture-dictionary-session-proxy".into());
    proxy.attempts[0].id = "fixture-dictionary-attempt-proxy".into();
    proxy.attempts[0].transfer = Some(TransferMetadata {
        sent_at: BASE,
        received_at: Some(BASE + 5),
        upstream_bytes: 300,
        downstream_bytes: 400,
        response_hash: Some("fixture-dictionary-transfer-proxy".into()),
        ..TransferMetadata::default()
    });
    [codex, proxy]
        .into_iter()
        .map(|record| {
            let mut value = serde_json::to_value(record).unwrap();
            replace_price_bases(&mut value);
            serde_json::to_vec(&canonical_value(&value)).unwrap()
        })
        .collect()
}

fn dictionary_compression_profile() -> (u64, u64, u64) {
    let dictionary = include_bytes!("body-dictionary-v1.json");
    let mut raw_bytes = 0;
    let mut plain_compressed_bytes = 0;
    let mut dictionary_compressed_bytes = 0;
    for raw in dictionary_fixture_samples() {
        raw_bytes += raw.len() as u64;
        let mut plain_encoder = zstd::stream::write::Encoder::new(Vec::new(), 6).unwrap();
        plain_encoder.include_checksum(true).unwrap();
        plain_encoder.write_all(&raw).unwrap();
        let plain = plain_encoder.finish().unwrap();
        plain_compressed_bytes += plain.len() as u64;

        let mut dictionary_encoder =
            zstd::stream::write::Encoder::with_dictionary(Vec::new(), 6, dictionary).unwrap();
        dictionary_encoder.include_checksum(true).unwrap();
        dictionary_encoder.write_all(&raw).unwrap();
        let compressed = dictionary_encoder.finish().unwrap();
        dictionary_compressed_bytes += compressed.len() as u64;

        let mut decoder =
            zstd::stream::read::Decoder::with_dictionary(compressed.as_slice(), dictionary).unwrap();
        let mut restored = Vec::new();
        decoder.take((raw.len() + 1) as u64).read_to_end(&mut restored).unwrap();
        assert_eq!(restored, raw);
    }
    (raw_bytes, plain_compressed_bytes, dictionary_compressed_bytes)
}

fn legacy_zstd_wrapper(body: &str) -> String {
    let mut wrapper: Value = serde_json::from_str(body).unwrap();
    let raw = serde_json::to_vec(&super::bodies::unpack(body).unwrap()).unwrap();
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
                .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))
                .unwrap()
                .map(Result::unwrap)
                .collect::<Vec<_>>();
            rows
        };
        for (id, body) in rows {
            let mut value = super::bodies::unpack(&body).unwrap();
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
    database.pragma_update(None, "user_version", 8).unwrap();
    if mode_none {
        database
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE); PRAGMA auto_vacuum=NONE; VACUUM;")
            .unwrap();
    }
}

#[test]
fn compressed_body_round_trip_restores_lossless_record() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
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
    assert_eq!(encoded["_usageCodec"], "zstd-dict-v1");
    let unpacked = super::bodies::unpack(&body).unwrap();
    assert_eq!(unpacked["attempts"][0]["price"]["basisRef"].is_string(), true);
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
    let mut store = Store::open(dir.path()).unwrap();
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
        .query_row("SELECT body FROM records WHERE id=?1", [&row.id], |r| r.get(0))
        .unwrap();
    let mut wrapper: Value = serde_json::from_str(&body).unwrap();
    let encoded = wrapper["_usageBody"].as_str().unwrap();
    let mut packed = STANDARD.decode(encoded).unwrap();
    let last = packed.last_mut().expect("compressed fixture has a checksum");
    *last ^= 0x01;
    wrapper["_usageBody"] = Value::String(STANDARD.encode(packed));
    let damaged = serde_json::to_string(&wrapper).unwrap();
    assert!(super::bodies::unpack(&damaged).is_err());
    database
        .execute("UPDATE records SET body=?2 WHERE id=?1", params![&row.id, damaged])
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
    let mut store = Store::open(dir.path()).unwrap();
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
    store.write_batch(&[old.clone(), new.clone()], None).unwrap();
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
        .query_row("SELECT body FROM records WHERE id=?1", [&old.id], |r| r.get(0))
        .unwrap();
    let stored_new: String = database
        .query_row("SELECT body FROM records WHERE id=?1", [&new.id], |r| r.get(0))
        .unwrap();
    assert!(serde_json::from_str::<Value>(&stored_old)
        .unwrap()
        .get("_usageCodec")
        .is_none());
    assert_eq!(
        serde_json::from_str::<Value>(&stored_new).unwrap()["_usageCodec"],
        "zstd-dict-v1"
    );
    drop(database);

    assert_eq!(serde_json::to_value(store.detail(&old.id).unwrap()).unwrap(), serde_json::to_value(old_detail).unwrap());
    assert_eq!(serde_json::to_value(store.detail(&new.id).unwrap()).unwrap(), serde_json::to_value(new_detail).unwrap());
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

    let mut store = Store::open(dir.path()).unwrap();
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
    assert_eq!(current_legacy["_usageCodec"], "zstd-dict-v1");
    let legacy_body = legacy_zstd_wrapper(&legacy_body);
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
        .query_row(
            "SELECT body FROM records WHERE id=?1",
            [&plain.id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(serde_json::from_str::<Value>(&stored_legacy).unwrap()["_usageCodec"], "zstd-v1");
    assert_eq!(
        serde_json::from_str::<Value>(&stored_dictionary).unwrap()["_usageCodec"],
        "zstd-dict-v1"
    );
    assert!(serde_json::from_str::<Value>(&stored_plain)
        .unwrap()
        .get("_usageCodec")
        .is_none());
    drop(database);

    let mixed = Store::open(dir.path()).unwrap();
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
    let mut migrated = Store::open(dir.path()).unwrap();
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
        "zstd-dict-v1"
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
    let mut store = Store::open(dir.path()).unwrap();
    store.write_batch(&[model, search, compaction], None).unwrap();

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
        assert_eq!(decimal(&dashboard.totals.cost), decimal(cost), "{operation}");
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
    assert_eq!(search_page.rows[0].attempts[0].operation, Operation::WebSearch);
    assert_eq!(
        store
            .detail("fixture-projection-search")
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
    let mut store = Store::open(dir.path()).unwrap();
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
    let stored = super::bodies::unpack(&stored).unwrap();
    assert!(stored["attempts"][0]["price"].get("basisRef").is_some());
    assert!(stored["attempts"][0]["price"].get("basis").is_none());
    drop(database);

    for (id, expected) in [
        ("fixture-basis-a-1", basis_a.clone()),
        ("fixture-basis-a-2", basis_a),
        ("fixture-basis-b", basis_b),
    ] {
        let detail = store.detail(id).unwrap();
        assert_eq!(detail.attempts[0].price.as_ref().unwrap().basis, Some(expected));
    }
    drop(store);
    let reopened = Store::open(dir.path()).unwrap();
    assert_eq!(
        reopened.detail("fixture-basis-a-1").unwrap().attempts[0]
            .price.as_ref().unwrap().basis,
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

    let mut store = Store::open(dir.path()).unwrap();
    store.write_batch(&rows, None).unwrap();
    let before_summary = summary(&store);
    assert_eq!(before_summary.0, 259, "the exact response pair is deduplicated");
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

    let mut migrated = Store::open(dir.path()).unwrap();
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

    let mut resumed = Store::open(dir.path()).unwrap();
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
    assert_eq!(bases, 2, "identical old bases share storage; distinct bases remain separate");
    drop(database);
    drop(resumed);

    let restarted = Store::open(dir.path()).unwrap();
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
    let mut store = Store::open(dir.path()).unwrap();
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
    let first_ref = super::bodies::unpack(&first_before).unwrap()["attempts"][0]["price"]["basisRef"]
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
    let mut first_inline = super::bodies::unpack(&first_before).unwrap();
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
    let second = super::bodies::unpack(&second_body).unwrap();
    let second_ref = second["attempts"][0]["price"]["basisRef"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_ne!(first_ref, second_ref);
    database
        .execute("DELETE FROM price_bases", [])
        .unwrap();
    database
        .execute(
            "DELETE FROM metadata WHERE key IN ('storage_records_after','storage_daily_after','storage_repack')",
            [],
        )
        .unwrap();
    database.pragma_update(None, "user_version", 8).unwrap();
    drop(database);

    let mut migrated = Store::open(dir.path()).unwrap();
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
    assert_eq!(first_after, first_before_batch, "prior row update rolled back");
    assert_eq!(bases, 0, "the first basis insertion rolled back with the damaged row");
    assert_eq!(cursor_after, cursor_before, "migration cursor did not advance");
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
    assert!(after_pages < before_reclaim, "incremental vacuum releases fixture pages");
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
    assert!(started.elapsed() < Duration::from_secs(2), "active reader must not block reclaim");
    let stable_snapshot: u64 = reader
        .query_row("SELECT count(*) FROM fixture_wal", [], |row| row.get(0))
        .unwrap();
    assert_eq!(stable_snapshot, 0, "the reader keeps its original snapshot");
    assert!(wal_len(dir.path()) >= WAL_LIMIT, "a pinned WAL is left intact");

    reader.execute_batch("ROLLBACK;").unwrap();
    drop(reader);
    store.reclaim_space().unwrap();
    let verify = Connection::open(&path).unwrap();
    let after_count: u64 = verify
        .query_row("SELECT count(*) FROM fixture_wal", [], |row| row.get(0))
        .unwrap();
    assert_eq!(after_count, 640);
    drop(verify);
    assert!(wal_len(dir.path()) < WAL_LIMIT, "checkpoint can truncate after the reader closes");
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
    assert_eq!(after_wal, before_wal, "an unchanged cursor appends no WAL frames");
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
    let body = super::bodies::unpack(&body).unwrap();
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
#[ignore]
fn v0212_hundred_thousand_records_fit_storage_target() {
    const TOTAL: u64 = 112_000;
    const CODEX: u64 = 67_200;
    const BATCH: usize = 256;
    const TARGET: u64 = 200 * 1024 * 1024;

    let dir = tempfile::tempdir().unwrap();
    let shared_basis = json!({
        "input_cost_per_token": "0.001",
        "output_cost_per_token": "0.002",
        "cache_read_cost_per_token": "0",
        "cache_write_cost_per_token": "0",
        "fixture": "v0212-capacity"
    });
    let mut store = Store::open(dir.path()).unwrap();
    let mut expected_input = 0u64;
    let mut expected_output = 0u64;
    let mut expected_first_token_sum = 0u64;
    let mut expected_first_token_samples = 0u64;
    let mut expected_sources = BTreeMap::<&str, (u64, u64, u64)>::new();
    let mut expected_operations = BTreeMap::<&str, (u64, u64, u64)>::new();
    let mut sample_ids = Vec::new();

    for start in (0..TOTAL as usize).step_by(BATCH) {
        let end = (start + BATCH).min(TOTAL as usize);
        let mut rows = Vec::with_capacity(end - start);
        for index in start..end {
            let index = index as u64;
            let source = if index < CODEX { "codex" } else { "proxy" };
            let operation = match index % 10 {
                8 => Operation::WebSearch,
                9 => Operation::Compaction,
                _ => Operation::Model,
            };
            let input = 100 + index;
            let output = 2 + index % 17;
            let row = capacity_record(index, CODEX, &shared_basis);
            if sample_ids.len() < 2 || index + 1 == CODEX || index + 1 == TOTAL {
                sample_ids.push(row.id.clone());
            }

            expected_input += input;
            expected_output += output;
            if source == "proxy" && operation == Operation::Model {
                expected_first_token_sum += 80 + index % 13;
                expected_first_token_samples += 1;
            }
            let source_totals = expected_sources.entry(source).or_default();
            source_totals.0 += 1;
            source_totals.1 += input;
            source_totals.2 += output;
            let operation_totals = expected_operations.entry(operation.as_str()).or_default();
            operation_totals.0 += 1;
            operation_totals.1 += input;
            operation_totals.2 += output;
            rows.push(row);
        }
        store.write_batch(&rows, None).unwrap();
    }

    complete_storage_migration(&mut store);
    store.reclaim_space().unwrap();

    let overview_started = Instant::now();
    let overview = store.dashboard(&Filter::default()).unwrap();
    let overview_elapsed = overview_started.elapsed();
    assert_eq!(overview.totals.requests, TOTAL);
    assert_eq!(overview.totals.attempts, TOTAL);
    assert_eq!(overview.totals.tokens.input, Some(expected_input));
    assert_eq!(overview.totals.tokens.output, Some(expected_output));
    assert_eq!(
        decimal(&overview.totals.cost).expect("overview cost should parse"),
        decimal("336").expect("expected overview cost should parse")
    );
    assert_eq!(overview.totals.unpriced, 0);
    assert_eq!(
        (overview.totals.first_token_sum_ms, overview.totals.first_token_samples),
        (expected_first_token_sum, expected_first_token_samples)
    );
    assert_eq!(overview.sources.get("codex"), Some(&CODEX));
    assert_eq!(overview.sources.get("proxy"), Some(&(TOTAL - CODEX)));

    for (source, (requests, input, output)) in expected_sources {
        let dashboard = store
            .dashboard(&Filter {
                source: Some(if source == "codex" { "sessions" } else { source }.into()),
                ..Filter::default()
            })
            .unwrap();
        assert_eq!(dashboard.totals.requests, requests, "{source} requests");
        assert_eq!(dashboard.totals.tokens.input, Some(input), "{source} input");
        assert_eq!(dashboard.totals.tokens.output, Some(output), "{source} output");
        assert_eq!(
            decimal(&dashboard.totals.cost).expect("source cost should parse"),
            decimal(if source == "codex" { "201.6" } else { "134.4" })
                .expect("expected source cost should parse")
        );
        assert_eq!(dashboard.totals.unpriced, 0, "{source} unpriced");
    }
    for (operation, (requests, input, output)) in expected_operations {
        let dashboard = store
            .dashboard(&Filter {
                operation: Some(operation.into()),
                ..Filter::default()
            })
            .unwrap();
        assert_eq!(dashboard.totals.requests, requests, "{operation} requests");
        assert_eq!(dashboard.totals.tokens.input, Some(input), "{operation} input");
        assert_eq!(dashboard.totals.tokens.output, Some(output), "{operation} output");
        assert_eq!(dashboard.totals.unpriced, 0, "{operation} unpriced");
    }

    let page_started = Instant::now();
    let page = store
        .logs(&Filter {
            page: 1,
            ..Filter::default()
        })
        .unwrap();
    let page_elapsed = page_started.elapsed();
    assert_eq!(page.total, TOTAL);
    assert_eq!(page.rows.len(), 20);
    assert!(page.rows.iter().all(|row| row.attempts[0].price.is_some()));
    assert!(page_elapsed < Duration::from_secs(30));

    let mut detail_elapsed = Duration::ZERO;
    for id in &sample_ids {
        let started = Instant::now();
        let detail = store.detail(id).unwrap();
        detail_elapsed += started.elapsed();
        assert_eq!(detail.attempts.len(), 1);
        assert_eq!(detail.attempts[0].price.as_ref().unwrap().basis, Some(shared_basis.clone()));
    }
    let database_bytes = file_len(&dir.path().join("usage.sqlite"));
    let wal_bytes = wal_len(dir.path());
    let total_bytes = database_bytes.saturating_add(wal_bytes);
    println!(
        "v0212 capacity db_bytes={database_bytes} wal_bytes={wal_bytes} total_bytes={total_bytes} target_200mib={} overview_ms={} page20_ms={} detail_ms={}",
        total_bytes <= TARGET,
        overview_elapsed.as_millis(),
        page_elapsed.as_millis(),
        detail_elapsed.as_millis()
    );
}

#[test]
fn v0212_storage_profile_2000_records() {
    const TOTAL: u64 = 2_000;
    const CODEX: u64 = 1_200;
    const BATCH: usize = 256;

    let dir = tempfile::tempdir().unwrap();
    let shared_basis = json!({
        "input_cost_per_token": "0.001",
        "output_cost_per_token": "0.002",
        "cache_read_cost_per_token": "0",
        "cache_write_cost_per_token": "0",
        "fixture": "v0212-storage-profile"
    });
    let mut store = Store::open(dir.path()).unwrap();
    for start in (0..TOTAL as usize).step_by(BATCH) {
        let end = (start + BATCH).min(TOTAL as usize);
        let rows = (start..end)
            .map(|index| capacity_record(index as u64, CODEX, &shared_basis))
            .collect::<Vec<_>>();
        store.write_batch(&rows, None).unwrap();
    }

    complete_storage_migration(&mut store);
    store.reclaim_space().unwrap();

    let overview = store.dashboard(&Filter::default()).unwrap();
    assert_eq!(overview.totals.requests, TOTAL);
    assert_eq!(overview.sources.get("codex"), Some(&CODEX));
    assert_eq!(overview.sources.get("proxy"), Some(&(TOTAL - CODEX)));

    let database = Connection::open(dir.path().join("usage.sqlite")).unwrap();
    let pending: u64 = database
        .query_row(
            "SELECT count(*) FROM metadata WHERE key IN ('storage_records_after','storage_daily_after')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(pending, 0);
    let records: u64 = database
        .query_row("SELECT count(*) FROM records", [], |row| row.get(0))
        .unwrap();
    assert_eq!(records, TOTAL);

    let mut metric_counts = BTreeMap::new();
    let mut statement = database
        .prepare("SELECT kind,count(*) FROM usage_metrics GROUP BY kind ORDER BY kind")
        .unwrap();
    for row in statement
        .query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, u64>(1)?)))
        .unwrap()
    {
        let (kind, count) = row.unwrap();
        metric_counts.insert(kind, count);
    }
    drop(statement);

    let profiles = source_storage_profiles(&database);
    let codex = profiles.get("codex").unwrap();
    let proxy = profiles.get("proxy").unwrap();
    assert_eq!(codex.count, CODEX);
    assert_eq!(proxy.count, TOTAL - CODEX);
    assert!(codex.compressed_bytes > 0);
    assert!(proxy.compressed_bytes > 0);

    let page_size = scalar_u64(&database, "PRAGMA page_size");
    let page_count = scalar_u64(&database, "PRAGMA page_count");
    let freelist_count = scalar_u64(&database, "PRAGMA freelist_count");
    let fallback_pages = length_storage_profile(&database);
    let dbstat_pages = dbstat_storage_profile(&database);
    let dbstat_available = dbstat_pages.is_some();
    let pages = dbstat_pages.as_ref().unwrap_or(&fallback_pages);
    let (dictionary_raw_bytes, dictionary_plain_bytes, dictionary_fixed_bytes) =
        dictionary_compression_profile();

    println!(
        "v0212 storage_profile rows={TOTAL} codex={} proxy={} page_size={} page_count={} freelist_count={} dbstat={} record_table_bytes={} record_index_bytes={} metrics_bytes={} other_bytes={} body_avg_codex={} body_avg_proxy={} projection_avg_codex={} projection_avg_proxy={} raw_json_avg_codex={} raw_json_avg_proxy={} base64_avg_codex={} base64_avg_proxy={} compressed_avg_codex={} compressed_avg_proxy={} metrics_kind0={} metrics_kind1={} metrics_kind2={} dictionary_raw_bytes={} dictionary_plain_zstd_bytes={} dictionary_fixed_zstd_bytes={}",
        codex.count,
        proxy.count,
        page_size,
        page_count,
        freelist_count,
        dbstat_available,
        pages.records,
        pages.record_indexes,
        pages.metrics,
        pages.other,
        codex.body_avg(),
        proxy.body_avg(),
        codex.projection_avg(),
        proxy.projection_avg(),
        codex.raw_avg(),
        proxy.raw_avg(),
        codex.base64_avg(),
        proxy.base64_avg(),
        codex.compressed_avg(),
        proxy.compressed_avg(),
        metric_counts.get(&0).copied().unwrap_or_default(),
        metric_counts.get(&1).copied().unwrap_or_default(),
        metric_counts.get(&2).copied().unwrap_or_default(),
        dictionary_raw_bytes,
        dictionary_plain_bytes,
        dictionary_fixed_bytes,
    );
}
