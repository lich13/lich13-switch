//! Deterministic old-format fixtures; never open or copy a user's database.
use super::{bodies, model, model::*, query, store::Store};
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use std::{collections::BTreeMap, path::Path};

#[path = "storage_v10_fixture.rs"]
mod legacy;

const DAY: i64 = 86_400_000;
const BASE: i64 = 20_000 * DAY;

fn row(index: u64, session: bool) -> Record {
    let hash = |s: &str| safe_id(&format!("fixture-v11-{s}-{index}"));
    let id = hash(if session {
        "session-record"
    } else {
        "proxy-record"
    });
    let at = BASE + (index % 28) as i64 * DAY + index as i64;
    let response = hash("response");
    let model = format!("fixture-model-{}", index % 8);
    Record {
        id: id.clone(),
        client: "codex".into(),
        source: if session { "codex" } else { "proxy" }.into(),
        started_at: at,
        finished_at: Some(at + 1000),
        session_id: Some(hash("thread")),
        completed: true,
        duplicate_of: session.then(|| hash("proxy-record")),
        deduplication: if session { "response_id" } else { "" }.into(),
        merged_sources: if index < 40_000 {
            vec!["codex".into(), "proxy".into()]
        } else {
            vec![]
        },
        attempts: vec![Attempt {
            id,
            provider: (!session).then(|| format!("fixture-provider-{}", index % 3)),
            requested_model: Some(model.clone()),
            response_model: Some(model.clone()),
            pricing_model: Some(model.clone()),
            response_id: Some(response.clone()),
            status: (!session).then_some(200),
            outcome: if session { "completed" } else { "success" }.into(),
            started_at: at,
            duration_ms: 1000,
            first_token_ms: (!session).then_some(100),
            stream: !session,
            transport: if session { "session" } else { "http" }.into(),
            tokens: Tokens {
                input: Some(100 + index),
                output: Some(20 + index % 9),
                cache_read: Some(40),
                ..Tokens::default()
            },
            price: Some(PriceSnapshot {
                version: "fixture-price-v1".into(),
                source: "fixture".into(),
                model,
                multiplier: "1".into(),
                cost: "0.003".into(),
                rates: BTreeMap::from([("input".into(), "0.01".into())]),
                basis: Some(
                    json!({"input_cost_per_token":0.01,"output_cost_per_token":0.02,"cache_read_cost_per_token":0.001}),
                ),
            }),
            thread_hash: (!session).then(|| hash("thread")),
            upstream_request_hash: (!session).then(|| hash("upstream")),
            transfer: (!session).then(|| TransferMetadata {
                sent_at: at,
                received_at: Some(at - 1),
                first_byte_at: Some(at + 10),
                terminal_at: Some(at + 999),
                ended_at: at + 1000,
                upstream_bytes: 3456,
                downstream_bytes: 3456,
                terminal_handed_off: true,
                upstream_eof: true,
                client_request_hash: Some(hash("client")),
                response_hash: Some(response),
                ..TransferMetadata::default()
            }),
            ..Attempt::default()
        }],
        ..Record::default()
    }
}

fn legacy_database(dir: &Path, proxies: u64, duplicates: u64) {
    drop(Store::open_v11_fixture(dir).unwrap());
    let mut c = Connection::open(dir.join("usage.sqlite")).unwrap();
    let tx = c.transaction().unwrap();
    for i in 0..proxies {
        for session in [false, true]
            .into_iter()
            .filter(|session| !session || i < duplicates)
        {
            let mut r = row(i, session);
            r.attempts[0].annotate_availability(&r.source);
            if !session && i < duplicates {
                // Exact donors in v0.25.5 always retained the gateway snapshot,
                // including when the original and final values were identical.
                r.gateway_reported = r.final_attempt().cloned();
            }
            let a = r.final_attempt().unwrap();
            let body = legacy::encode(&tx, &r).unwrap();
            tx.execute("INSERT INTO records(id,client,source,time,provider,model,status,response,signature,effective,duplicate_of,body,local_sequence) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",params![r.id,r.client,r.source,r.event_time(),a.provider,a.grouping_model(),a.status,a.response_id,r.signature(),if session {0} else {1},r.duplicate_of,body,i*2+u64::from(session)]).unwrap();
            query::project(&tx, 0, &r.id, &r, 1, None).unwrap();
            if !session {
                query::project(&tx, 2, &r.id, &r, 1, None).unwrap();
            }
        }
    }
    tx.execute_batch("DELETE FROM metadata WHERE key IN ('storage_records_after','storage_daily_after','storage_repack','repair_v020_after','finish_time_after'); DROP TABLE body_shapes; PRAGMA user_version=10;").unwrap();
    tx.commit().unwrap();
    query::compact_indexes(&c).unwrap();
    c.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); VACUUM; PRAGMA wal_checkpoint(TRUNCATE);")
        .unwrap();
}

fn totals(store: &Store) -> Vec<Value> {
    [None, Some("proxy"), Some("sessions"), Some("grok_sessions")]
        .into_iter()
        .map(|source| {
            let filter = Filter {
                source: source.map(str::to_string),
                start: Some(BASE - DAY),
                end: Some(BASE + 60 * DAY),
                ..Filter::default()
            };
            let d = store.dashboard(&filter).unwrap();
            json!([
                d.totals,
                d.providers,
                d.models,
                d.sources,
                store.logs(&filter).unwrap().total
            ])
        })
        .collect()
}

fn migrate(store: &mut Store) {
    for _ in 0..1000 {
        if !store.compact_storage_batch().unwrap() {
            store.reclaim_space().unwrap();
            return;
        }
    }
    panic!("storage migration did not finish");
}

fn size(dir: &Path) -> Value {
    let c = Connection::open(dir.join("usage.sqlite")).unwrap();
    c.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)").unwrap();
    let scalar = |sql: &str| c.query_row(sql, [], |r| r.get::<_, u64>(0)).unwrap();
    let bytes = |suffix: &str| {
        std::fs::metadata(dir.join(format!("usage.sqlite{suffix}")))
            .map(|m| m.len())
            .unwrap_or(0)
    };
    json!({"db":bytes(""),"wal":bytes("-wal"),"rows":scalar("SELECT count(*) FROM records"),"duplicates":scalar("SELECT count(*) FROM records WHERE effective=0"),"body":scalar("SELECT coalesce(sum(length(body)),0) FROM records"),"duplicate_body":scalar("SELECT coalesce(sum(length(body)),0) FROM records WHERE effective=0"),"metrics":scalar("SELECT count(*) FROM usage_metrics"),"metric_pages":scalar("SELECT coalesce(sum(pgsize),0) FROM dbstat WHERE name='usage_metrics'")})
}

#[test]
fn v11_migration_resumes_and_preserves_all_source_views() {
    let dir = tempfile::tempdir().unwrap();
    legacy_database(dir.path(), 2400, 1600);
    let before_size = size(dir.path());
    let old = Store::reader(dir.path()).unwrap();
    let before = totals(&old);
    drop(old);
    let mut store = Store::open_v11_fixture(dir.path()).unwrap();
    assert!(store.compact_storage_batch().unwrap());
    let c = Connection::open(dir.path().join("usage.sqlite")).unwrap();
    let converted: u64 = c.query_row("SELECT count(*) FROM records WHERE json_extract(body,'$._usageCodec')='zstd-compact-v2'", [], |r| r.get(0)).unwrap();
    assert_eq!(converted, 2000);
    drop(c);
    assert_eq!(before, totals(&store));
    drop(store);
    let mut store = Store::open_v11_fixture(dir.path()).unwrap();
    migrate(&mut store);
    assert_eq!(before, totals(&store));
    let state = size(dir.path());
    println!("v11 small before={before_size} after={state}");
    assert_eq!(state["metrics"], 4000);
    let duplicate = store.detail(&row(0, true).id).unwrap();
    assert_eq!(duplicate.tokens(), row(0, true).tokens());
    store.compact(BASE + 60 * DAY).unwrap();
    assert_eq!(
        before.iter().map(|v| &v[0]).collect::<Vec<_>>(),
        totals(&store).iter().map(|v| &v[0]).collect::<Vec<_>>()
    );
}

#[test]
fn v11_body_delta_preserves_null_zero_and_gateway_original() {
    let dir = tempfile::tempdir().unwrap();
    drop(Store::open_v11_fixture(dir.path()).unwrap());
    let c = Connection::open(dir.path().join("usage.sqlite")).unwrap();
    let mut r = row(0, false);
    let mut original = r.attempts[0].clone();
    original.tokens.input = None;
    original.tokens.output = Some(0);
    original.price = None;
    r.gateway_reported = Some(original);
    let encoded = bodies::encode(&c, &r).unwrap();
    let decoded: Record = bodies::decode(&c, &encoded).unwrap();
    assert_eq!(
        serde_json::to_value(&r).unwrap(),
        serde_json::to_value(decoded).unwrap()
    );
}

#[test]
fn v11_shared_shapes_keep_reader_snapshot_and_reject_corruption() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open_v11_fixture(dir.path()).unwrap();
    let r = row(0, false);
    store.write_batch(std::slice::from_ref(&r), None).unwrap();
    let c = Connection::open(dir.path().join("usage.sqlite")).unwrap();
    c.execute_batch("BEGIN").unwrap();
    let body: String = c
        .query_row("SELECT body FROM records WHERE id=?1", [&r.id], |r| {
            r.get(0)
        })
        .unwrap();
    let before: Record = bodies::decode(&c, &body).unwrap();
    store.compact(BASE + 60 * DAY).unwrap();
    let after: Record = bodies::decode(&c, &body).unwrap();
    assert_eq!(
        serde_json::to_value(before).unwrap(),
        serde_json::to_value(after).unwrap()
    );
    c.execute_batch("COMMIT").unwrap();
    assert!(
        bodies::decode::<Record>(&c, &body).is_err(),
        "expired shape is reclaimed after the reader finishes"
    );
    let encoded = bodies::encode(&c, &r).unwrap();
    c.execute(
        "UPDATE body_shapes SET body='{}' WHERE id=json_extract(?1,'$._usageShape')",
        [&encoded],
    )
    .unwrap();
    assert!(bodies::decode::<Record>(&c, &encoded).is_err());
}

#[test]
fn v11_codec_waits_for_durable_database_version_guard() {
    let dir = tempfile::tempdir().unwrap();
    drop(Store::open_v11_fixture(dir.path()).unwrap());
    let c = Connection::open(dir.path().join("usage.sqlite")).unwrap();
    c.pragma_update(None, "user_version", 10).unwrap();
    let body = bodies::encode(&c, &row(0, false)).unwrap();
    assert!(serde_json::from_str::<Value>(&body)
        .unwrap()
        .get("_usageCodec")
        .is_none());
    assert!(bodies::decode::<Record>(&c, &body).is_ok());
}

#[test]
#[ignore = "100k synthetic capacity gate; run explicitly once per storage change"]
fn v11_hundred_thousand_rows_shrink_at_least_thirty_percent() {
    let dir = tempfile::tempdir().unwrap();
    legacy_database(dir.path(), 60_000, 40_000);
    let before_size = size(dir.path());
    let old = Store::reader(dir.path()).unwrap();
    let before = totals(&old);
    drop(old);
    let mut store = Store::open_v11_fixture(dir.path()).unwrap();
    migrate(&mut store);
    let after_size = size(dir.path());
    assert_eq!(before, totals(&store));
    let before_bytes = before_size["db"].as_u64().unwrap() + before_size["wal"].as_u64().unwrap();
    let after_bytes = after_size["db"].as_u64().unwrap() + after_size["wal"].as_u64().unwrap();
    println!(
        "v11 synthetic before={before_size} after={after_size} reduction_percent={:.3}",
        100.0 * (before_bytes - after_bytes) as f64 / before_bytes as f64
    );
    assert!(
        after_bytes * 100 <= before_bytes * 70,
        "storage reduction below 30 percent"
    );
}
