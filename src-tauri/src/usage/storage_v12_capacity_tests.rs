//! Synthetic v11 storage fixtures for the v12 capacity gate.
//!
//! Every value in this module is generated from a fixed seed.  It never reads
//! a user's usage database, log, session, credential, or request body.
use super::{bodies, model::*, query, store::Store};
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use std::{collections::BTreeMap, fs, path::Path};

const DAY: i64 = 86_400_000;
const BASE: i64 = 20_000 * DAY;
const FIXED_NOW: i64 = BASE + 90 * DAY;
const CODEC_V11: &str = "zstd-compact-v2";
const CAPACITY_TARGET_BYTES: u64 = 80 * 1024 * 1024;

#[derive(Clone, Copy)]
struct Profile {
    proxy: u64,
    sessions: u64,
    no_response: u64,
    grok: u64,
}

const SMALL: Profile = Profile {
    proxy: 60_000,
    sessions: 40_000,
    no_response: 40_000,
    grok: 0,
};

const LARGE: Profile = Profile {
    proxy: 121_000,
    sessions: 136_000,
    no_response: 106_000,
    grok: 16_000,
};

fn hash(kind: &str, index: u64) -> String {
    safe_id(&format!("fixture-v12-capacity-{kind}-{index:06}"))
}

// Match the production ID generators without reading any real identifiers.
// Both the v11 baseline and v12 use these identical fixed-seed records.
fn fixture_uuid(kind: &str, index: u64) -> String {
    let digest = hash(kind, index);
    let mut bytes = [0u8; 16];
    for (i, pair) in digest.as_bytes().chunks_exact(2).take(16).enumerate() {
        bytes[i] = (pair[0] as char).to_digit(16).unwrap() as u8 * 16
            + (pair[1] as char).to_digit(16).unwrap() as u8;
    }
    bytes[6] = (bytes[6] & 15) | 64;
    bytes[8] = (bytes[8] & 63) | 128;
    uuid::Uuid::from_bytes(bytes).to_string()
}
fn record_id(source: &str, index: u64) -> String {
    if source == "proxy" {
        fixture_uuid("record", index)
    } else {
        hash("record", index)
    }
}

fn model(index: u64) -> String {
    [
        "fixture-gpt-4o",
        "fixture-claude-3-7",
        "fixture-o3-mini",
        "fixture-grok-4",
        "fixture-sonnet",
        "fixture-opus",
    ][(index % 6) as usize]
        .into()
}

fn provider(index: u64) -> String {
    [
        "fixture-openai",
        "fixture-anthropic",
        "fixture-vertex",
        "fixture-grok",
        "fixture-bedrock",
    ][(index % 5) as usize]
        .into()
}

fn basis(index: u64) -> Value {
    json!({
        "input_cost_per_token": "0.001",
        "output_cost_per_token": "0.002",
        "cache_read_cost_per_token": "0.0001",
        "cache_write_cost_per_token": "0.0002",
        "fixture": "v12-capacity",
        "basis_revision": index % 3,
    })
}

fn price(index: u64, model: &str) -> Option<PriceSnapshot> {
    if index % 41 == 0 {
        return None;
    }
    let cost = match index % 17 {
        0 => "0",
        1..=4 => "0.0007",
        _ => "0.003",
    };
    Some(PriceSnapshot {
        version: "fixture-v12-price".into(),
        source: "fixture".into(),
        model: model.into(),
        multiplier: if index % 13 == 0 { "1.25" } else { "1" }.into(),
        rates: BTreeMap::from([
            ("input".into(), "0.001".into()),
            ("output".into(), "0.002".into()),
        ]),
        cost: cost.into(),
        basis: Some(basis(index)),
    })
}

fn operation(index: u64) -> Operation {
    match index % 31 {
        0 => Operation::WebSearch,
        1 => Operation::Compaction,
        _ => Operation::Model,
    }
}

fn proxy_time(index: u64, proxy_count: u64) -> i64 {
    let hot = proxy_count * 2 / 3;
    let cold_count = proxy_count.saturating_sub(hot);
    let deep_count = (cold_count / 8).max(1);
    if index < hot {
        FIXED_NOW - (index % 7) as i64 * DAY - (index % 86_400) as i64
    } else if index - hot < cold_count.saturating_sub(deep_count) {
        // Most of the remaining rows are 8-29 days old and remain queryable
        // after the fixed 30-day cold cutoff.
        FIXED_NOW - (8 + (index - hot) % 22) as i64 * DAY - index as i64
    } else {
        // A smaller historical tail exercises daily and receipt projections.
        FIXED_NOW - (31 + (index - hot) % 5) as i64 * DAY - index as i64
    }
}

fn conflict_count(profile: Profile) -> u64 {
    if profile.grok == 0 {
        0
    } else {
        4_000.min(profile.no_response)
    }
}

fn proxy_index_for_session(index: u64, pairable: &[u64], no_response: u64, conflicts: u64) -> u64 {
    let unique = no_response.saturating_sub(conflicts);
    let offset = if index < unique {
        index
    } else {
        // The large profile reuses two thousand fingerprints for a deterministic
        // four-thousand-row conflict population; SMALL has no conflicts.
        unique + (index - unique) % (conflicts / 2).max(1)
    };
    pairable[offset as usize % pairable.len()]
}

fn pairable(index: u64) -> bool {
    operation(index) == Operation::Model && index % 43 != 0 && index % 47 != 0 && index % 59 != 0
}

fn transfer(index: u64, at: i64, status: Option<u16>) -> TransferMetadata {
    TransferMetadata {
        connection: Some(ConnectionEvidence {
            id: fixture_uuid("connection", index),
            request_sequence: 1,
            read_bytes: 900 + index % 700,
            socket_written_bytes: 1_200 + index % 900,
            events: vec![
                TransportEvent {
                    sequence: 1,
                    at: at + 2,
                    kind: "headers_received".into(),
                },
                TransportEvent {
                    sequence: 2,
                    at: at + 11,
                    kind: if status == Some(200) {
                        "first_byte"
                    } else {
                        "upstream_error"
                    }
                    .into(),
                },
            ],
        }),
        received_at: Some(at.saturating_sub(2)),
        sent_at: at,
        first_byte_at: Some(at + 11),
        terminal_at: Some(at + 800),
        terminal_event: (status != Some(200)).then(|| "error".into()),
        terminal_status: status.map(|s| {
            if s == 200 {
                "completed"
            } else if s == 500 {
                "incomplete"
            } else {
                "failed"
            }
            .into()
        }),
        incomplete_reason: (status == Some(500)).then(|| "fixture_upstream_incomplete".into()),
        upstream_client_request_hash: Some(hash("upstream-client", index)),
        ended_at: at + 900,
        upstream_bytes: 1_000 + index % 2_000,
        downstream_bytes: 1_100 + index % 2_100,
        terminal_handed_off: status == Some(200),
        upstream_eof: status == Some(200),
        upstream_end: Some(
            match status {
                Some(200) => "EOF",
                Some(429) => "CONNECT_FAILED",
                _ => "TIMEOUT",
            }
            .into(),
        ),
        ws_close_code: None,
        downstream_error: (status == Some(429)).then(|| "fixture_capacity".into()),
        timeout_phase: (status == Some(500)).then(|| "response".into()),
        timeout_seconds: (status == Some(500)).then_some(30),
        last_progress_ms: Some(700),
        client_request_hash: Some(hash("client-request", index)),
        response_hash: Some(hash("response-body", index)),
    }
}

fn attempt(
    source: &str,
    index: u64,
    payload: u64,
    response_id: Option<String>,
    at: i64,
) -> Attempt {
    let operation = operation(payload);
    let model = model(payload);
    let is_proxy = source == "proxy";
    let is_grok = source == "grok";
    let status = if is_proxy {
        Some(match payload % 43 {
            0 => 429,
            1 => 500,
            _ => 200,
        })
    } else {
        None
    };
    let zero = payload % 53 == 0;
    let input = if zero {
        Some(0)
    } else {
        Some(64 + payload % 2_000)
    };
    let output = if zero {
        Some(0)
    } else {
        Some(8 + payload % 97)
    };
    let mut value = Attempt {
        id: if is_proxy {
            fixture_uuid("attempt", index)
        } else {
            record_id(source, index)
        },
        provider: is_grok
            .then(|| "fixture-grok-native".into())
            .or_else(|| is_proxy.then(|| provider(payload))),
        requested_model: Some(model.clone()),
        response_model: Some(model.clone()),
        pricing_model: Some(model.clone()),
        response_id,
        status,
        outcome: if is_proxy {
            match status {
                Some(429) => "capacity",
                Some(500) => "upstream_error",
                _ => "success",
            }
        } else {
            "completed"
        }
        .into(),
        started_at: at,
        duration_ms: 700 + payload % 1_200,
        first_token_ms: (is_proxy && operation == Operation::Model).then_some(80 + payload % 120),
        stream: is_proxy && operation == Operation::Model,
        transport: if is_proxy { "http" } else { "session" }.into(),
        tokens: Tokens {
            reasoning: None,
            image_cache_read: None,
            inclusive_input: Some(input.unwrap_or_default().saturating_add(payload % 9)),
            input,
            output,
            cache_read: Some(payload % 32),
            cache_write: is_grok.then_some(payload % 11),
            cache_write_5m: is_grok.then_some(payload % 7),
            cache_write_1h: is_grok.then_some(payload % 5),
            image_input: (payload % 67 == 0).then_some(1),
            image_output: (payload % 71 == 0).then_some(2),
            audio_input: (payload % 73 == 0).then_some(3),
            audio_output: (payload % 79 == 0).then_some(4),
        },
        service_tier: Some(
            if payload % 3 == 0 {
                "standard"
            } else {
                "priority"
            }
            .into(),
        ),
        price: price(payload, &model),
        ..Attempt::default()
    };
    if is_proxy {
        value.transfer = Some(transfer(payload, at, status));
        value.thread_hash = Some(hash("thread", payload));
        value.upstream_request_hash = Some(hash("upstream", payload));
    }
    if payload % 29 == 0 {
        value.end = Some(EndMetadata {
            cause: if status == Some(429) {
                EndCause::UpstreamRejected
            } else {
                EndCause::UpstreamIncomplete
            },
            terminal_received: status == Some(200),
            response_started: payload % 2 == 0,
        });
        value.usage_status = "parse_incomplete".into();
    }
    if !is_proxy && payload % 19 == 0 {
        value.routing_decision = Some(RoutingDecision::NewThreadQueueWaiting);
        value.routing_evidence = Some(RoutingEvidence {
            input: RoutingInput::FirstTurn,
            has_tool_declarations: payload % 2 == 0,
            restriction: Some(RoutingRestriction::QueueWaiting),
            ..Default::default()
        });
    }
    value.annotate_availability(source);
    value
}

fn record(source: &str, index: u64, payload: u64, response_id: Option<String>, at: i64) -> Record {
    let client = if source == "grok" { "grok" } else { "codex" };
    let mut attempts = Vec::with_capacity(if payload % 73 == 0 { 2 } else { 1 });
    if payload % 73 == 0 {
        let retry_at = at.saturating_sub(250);
        let mut retry = attempt(
            source,
            index.saturating_add(1_000_000),
            payload,
            None,
            retry_at,
        );
        retry.status = if source == "proxy" { Some(503) } else { None };
        retry.outcome = "retryable_error".into();
        retry.end = Some(EndMetadata {
            cause: EndCause::UpstreamDisconnected,
            terminal_received: false,
            response_started: true,
        });
        attempts.push(retry);
    }
    attempts.push(attempt(source, index, payload, response_id, at));
    Record {
        id: record_id(source, index),
        client: client.into(),
        source: source.into(),
        started_at: at,
        finished_at: Some(at + 900),
        session_id: (source != "proxy").then(|| hash("session", payload)),
        attempts,
        completed: source != "proxy" || payload % 29 != 0,
        estimated_speed: payload % 97 == 0,
        ..Record::default()
    }
}

fn insert_legacy(
    tx: &rusqlite::Transaction<'_>,
    record: &Record,
    effective: i64,
    duplicate_of: Option<&str>,
    sequence: u64,
) {
    let mut stored = record.clone();
    stored.duplicate_of = duplicate_of.map(str::to_owned);
    stored.deduplication = if duplicate_of.is_some() {
        if stored
            .final_attempt()
            .and_then(|a| a.response_id.as_ref())
            .is_some()
        {
            "response_id"
        } else {
            "strict_match"
        }
    } else if effective == 2 {
        "ambiguous"
    } else {
        ""
    }
    .into();
    if duplicate_of.is_some() && stored.source != "proxy" {
        stored.merged_sources = vec![stored.source.clone(), "proxy".into()];
    }
    if stored.source == "proxy" && sequence % 17 == 0 {
        let mut gateway = stored.final_attempt().cloned().unwrap_or_default();
        gateway.tokens.input = Some(0);
        gateway.usage_status = "upstream_unreported".into();
        stored.gateway_reported = Some(gateway);
    }
    let body = bodies::encode(tx, &stored).unwrap();
    let a = stored.final_attempt();
    tx.execute(
        "INSERT INTO records(id,client,source,time,provider,model,status,response,signature,effective,duplicate_of,body,local_sequence) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
        params![
            stored.id,
            stored.client,
            stored.source,
            stored.event_time(),
            a.and_then(|v| v.provider.as_deref()),
            a.and_then(Attempt::grouping_model),
            a.and_then(|v| v.status),
            a.and_then(|v| v.response_id.as_deref()),
            stored.signature(),
            effective,
            duplicate_of,
            body,
            sequence,
        ],
    )
    .unwrap();
    query::project(tx, 0, &stored.id, &stored, 1, None).unwrap();
}

fn seed_v11(dir: &Path, profile: Profile) -> Store {
    let conflicts = conflict_count(profile);
    let pairable = (0..profile.proxy)
        .filter(|i| pairable(*i))
        .collect::<Vec<_>>();
    assert!(pairable.len() > profile.no_response as usize);
    // The large population is inserted in one SQL transaction to avoid
    // running one-to-one reconciliation hundreds of thousands of times.
    let store = Store::open_v11_fixture(dir).unwrap();

    let mut connection = Connection::open(dir.join("usage.sqlite")).unwrap();
    connection
        .busy_timeout(std::time::Duration::from_secs(30))
        .unwrap();
    let tx = connection.transaction().unwrap();
    let mut sequence = 1u64;
    for index in 0..profile.proxy {
        let at = proxy_time(index, profile.proxy);
        let response = Some(hash("response", index));
        let row = record("proxy", index, index, response, at);
        insert_legacy(&tx, &row, 1, None, sequence);
        sequence += 1;
        if (sequence - 1) % 20_000 == 0 {
            println!("v12 capacity seed rows={}", sequence - 1);
        }
    }
    let codex = profile.sessions.saturating_sub(profile.grok);
    for index in 0..codex {
        let payload = if index < profile.no_response {
            proxy_index_for_session(index, &pairable, profile.no_response, conflicts)
        } else {
            pairable[(index as usize + 17_003) % pairable.len()]
        };
        let proxy_at = proxy_time(payload, profile.proxy);
        let at = proxy_at.saturating_add((index % 3) as i64 * 1_000);
        let response = if index < profile.no_response {
            None
        } else if index % 7 == 0 {
            Some(hash("conflicting-session-response", index / 7))
        } else {
            Some(hash("response", payload))
        };
        let row = record("codex", profile.proxy + index, payload, response, at);
        let duplicate = if (index < profile.no_response
            && index < profile.no_response.saturating_sub(conflicts))
            || (index >= profile.no_response && index % 7 != 0)
        {
            Some(record_id("proxy", payload))
        } else {
            None
        };
        let effective = if duplicate.is_some() {
            0
        } else if index < profile.no_response {
            2
        } else {
            1
        };
        insert_legacy(&tx, &row, effective, duplicate.as_deref(), sequence);
        sequence += 1;
        if (sequence - 1) % 20_000 == 0 {
            println!("v12 capacity seed rows={}", sequence - 1);
        }
    }
    for index in 0..profile.grok {
        let payload = 700_000 + index;
        let at = proxy_time(index, profile.grok);
        let row = record("grok", profile.proxy + codex + index, payload, None, at);
        insert_legacy(&tx, &row, 1, None, sequence);
        sequence += 1;
        if (sequence - 1) % 20_000 == 0 {
            println!("v12 capacity seed rows={}", sequence - 1);
        }
    }
    query::changed(&tx).unwrap();
    tx.execute_batch(
        "DELETE FROM metadata WHERE key IN ('repair_v020_after','finish_time_after','storage_records_after','storage_daily_after');
         INSERT OR REPLACE INTO metadata(key,value)
         VALUES('record_sequence',(SELECT coalesce(max(local_sequence),0) FROM records));",
    )
    .unwrap();
    tx.commit().unwrap();
    query::compact_indexes(&connection).unwrap();
    store
}

#[derive(Debug, Clone, Copy, Default)]
struct FileBytes {
    db: u64,
    wal: u64,
}

impl FileBytes {
    fn total(self) -> u64 {
        self.db.saturating_add(self.wal)
    }
}

fn file_bytes(dir: &Path) -> FileBytes {
    let size = |suffix: &str| {
        fs::metadata(dir.join(format!("usage.sqlite{suffix}")))
            .map(|metadata| metadata.len())
            .unwrap_or_default()
    };
    FileBytes {
        db: size(""),
        wal: size("-wal"),
    }
}

fn sample_peak(peak: &mut FileBytes, dir: &Path) -> FileBytes {
    let current = file_bytes(dir);
    peak.db = peak.db.max(current.db);
    peak.wal = peak.wal.max(current.wal);
    current
}

fn migrate(store: &mut Store, dir: &Path, peak: &mut FileBytes) {
    sample_peak(peak, dir);
    let connection = Connection::open(dir.join("usage.sqlite")).unwrap();
    query::register(&connection).unwrap();
    let mut next_report = 20_000u64;
    for batch in 0..2_000 {
        if !store.compact_storage_batch().unwrap() {
            sample_peak(peak, dir);
            let copied = connection
                .query_row("SELECT count(*) FROM v12_records", [], |row| {
                    row.get::<_, u64>(0)
                })
                .unwrap();
            println!(
                "v12 capacity migration complete batches={} copied_rows={copied}",
                batch
            );
            store.reclaim_space().unwrap();
            sample_peak(peak, dir);
            println!(
                "v12 capacity migration file_peak_db={} file_peak_wal={} file_peak_total={}",
                peak.db,
                peak.wal,
                peak.total()
            );
            return;
        }
        sample_peak(peak, dir);
        let copied = connection
            .query_row("SELECT count(*) FROM v12_records", [], |row| {
                row.get::<_, u64>(0)
            })
            .unwrap();
        while copied >= next_report {
            println!("v12 capacity migration copied_rows={next_report}");
            next_report += 20_000;
        }
    }
    panic!("v12 storage migration did not finish");
}

fn compact_fixed(store: &mut Store, dir: &Path, peak: &mut FileBytes) {
    sample_peak(peak, dir);
    for _ in 0..2_000 {
        store.compact(FIXED_NOW).unwrap();
        sample_peak(peak, dir);
        let connection = Connection::open(dir.join("usage.sqlite")).unwrap();
        query::register(&connection).unwrap();
        let pending: bool = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM metadata WHERE key='cold_after')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        if !pending {
            println!(
                "v12 capacity fixed_compaction file_peak_db={} file_peak_wal={} file_peak_total={}",
                peak.db,
                peak.wal,
                peak.total()
            );
            return;
        }
    }
    panic!("fixed-time cold compaction did not finish");
}

fn views(store: &Store, phase: &str) -> Vec<Value> {
    [None, Some("proxy"), Some("sessions"), Some("grok_sessions")]
        .into_iter()
        .map(|source| {
            println!(
                "v12 capacity views phase={phase} source={}",
                source.unwrap_or("all")
            );
            let filter = Filter {
                source: source.map(str::to_owned),
                start: Some(BASE - DAY),
                end: Some(FIXED_NOW + DAY),
                ..Filter::default()
            };
            let dashboard = store.dashboard(&filter).unwrap();
            json!({
                "totals": dashboard.totals,
                "providers": dashboard.providers,
                "models": dashboard.models,
                "sources": dashboard.sources,
                "logs": store.logs(&filter).unwrap().total,
            })
        })
        .collect()
}

#[derive(Debug)]
struct TableStat {
    table: String,
    rows: u64,
    body_bytes: u64,
    key_bytes: u64,
}

#[derive(Debug)]
struct DbstatStat {
    name: String,
    bytes: u64,
    payload: u64,
    unused: u64,
}

#[derive(Debug)]
struct StorageSize {
    db: u64,
    wal: u64,
    rows: u64,
    duplicates: u64,
    daily: u64,
    receipts: u64,
    view_body: u64,
    v12_blob_body: u64,
    compressed: u64,
    shapes: u64,
    metrics: u64,
    dbstat_bytes: u64,
    dbstat_payload: u64,
    dbstat_unused: u64,
    dbstat_by_name: Vec<DbstatStat>,
    table_rows: Vec<(String, u64)>,
    table_stats: Vec<TableStat>,
}

fn storage_size(dir: &Path) -> StorageSize {
    let connection = Connection::open(dir.join("usage.sqlite")).unwrap();
    query::register(&connection).unwrap();
    connection
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .unwrap();
    let scalar = |sql: &str| {
        connection
            .query_row(sql, [], |row| row.get::<_, u64>(0))
            .unwrap()
    };
    let bytes = |suffix: &str| {
        fs::metadata(dir.join(format!("usage.sqlite{suffix}")))
            .map(|metadata| metadata.len())
            .unwrap_or_default()
    };
    let table_exists = |name: &str| {
        connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
                [name],
                |row| row.get::<_, bool>(0),
            )
            .unwrap()
    };
    let blob_sum = |table: &str, column: &str| {
        if table_exists(table) {
            scalar(&format!(
                "SELECT coalesce(sum(length({column})),0) FROM {table}"
            ))
        } else {
            0
        }
    };
    let v12_blob_body = [
        ("v12_dictionary", "value"),
        ("v12_rows", "body"),
        ("v12_attempt_rows", "body"),
        ("v12_session_rows", "body"),
        ("v12_diagnostics", "body"),
        ("v12_archives", "body"),
        ("v12_templates", "body"),
        ("v12_entities", "value"),
        ("v12_records", "body"),
        ("v12_attempts", "body"),
        ("v12_sessions", "body"),
        ("v12_original", "body"),
    ]
    .into_iter()
    .map(|(table, column)| blob_sum(table, column))
    .sum();
    let table_stats = [
        "v12_dictionary",
        "v12_templates",
        "v12_diagnostics",
        "v12_archives",
        "v12_dead_diagnostics",
        "v12_record_states",
        "v12_rows",
        "v12_links",
        "v12_dirty",
        "v12_measures",
        "v12_dimensions",
        "v12_attempt_rows",
        "v12_session_rows",
        "v12_original",
    ]
    .into_iter()
    .filter(|table| table_exists(table))
    .map(|table| {
        let (rows, body_bytes, key_bytes): (u64, u64, u64) = match table {
            "v12_dictionary" => connection
                .query_row(
                    "SELECT count(*), coalesce(sum(length(value)),0), 0 FROM v12_dictionary",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .unwrap(),
            "v12_templates" => connection
                .query_row(
                    "SELECT count(*), coalesce(sum(length(body)),0), 0 FROM v12_templates",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .unwrap(),
            "v12_rows" => connection
                .query_row(
                    "SELECT count(*), coalesce(sum(length(body)),0), coalesce(sum(length(key)),0) FROM v12_rows",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .unwrap(),
            "v12_attempt_rows" | "v12_session_rows" => connection
                .query_row(
                    &format!(
                        "SELECT count(*), coalesce(sum(length(body)),0), 0 FROM {table}"
                    ),
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .unwrap(),
            "v12_record_states" | "v12_measures" | "v12_dimensions" | "v12_dead_diagnostics" =>
                connection
                    .query_row(
                        &format!("SELECT count(*), 0, 0 FROM {table}"),
                        [],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                    )
                    .unwrap(),
            "v12_links" => connection
                .query_row(
                    "SELECT count(*), 0, coalesce(sum(length(target)),0) FROM v12_links",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .unwrap(),
            "v12_dirty" => connection
                .query_row(
                    "SELECT count(*), 0, coalesce(sum(length(id)),0) FROM v12_dirty",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .unwrap(),
            _ => connection
                .query_row(
                    &format!("SELECT count(*), coalesce(sum(length(body)),0), 0 FROM {table}"),
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .unwrap(),
        };
        TableStat {
            table: table.into(),
            rows,
            body_bytes,
            key_bytes,
        }
    })
    .collect();

    let table_names = connection
        .prepare(
            "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
        )
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    let table_rows = table_names
        .into_iter()
        .map(|table| {
            let quoted = format!("\"{}\"", table.replace('"', "\"\""));
            let rows = scalar(&format!("SELECT count(*) FROM {quoted}"));
            (table, rows)
        })
        .collect();

    let mut statement = connection
        .prepare(
            "SELECT name, coalesce(sum(pgsize),0), coalesce(sum(payload),0), coalesce(sum(unused),0) FROM dbstat GROUP BY name ORDER BY sum(pgsize) DESC",
        )
        .unwrap();
    let dbstat_by_name = statement
        .query_map([], |row| {
            Ok(DbstatStat {
                name: row.get(0)?,
                bytes: row.get(1)?,
                payload: row.get(2)?,
                unused: row.get(3)?,
            })
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    let dbstat_bytes = dbstat_by_name.iter().map(|stat| stat.bytes).sum();
    let dbstat_payload = dbstat_by_name.iter().map(|stat| stat.payload).sum();
    let dbstat_unused = dbstat_by_name.iter().map(|stat| stat.unused).sum();
    StorageSize {
        db: bytes(""),
        wal: bytes("-wal"),
        rows: scalar("SELECT count(*) FROM records"),
        duplicates: scalar("SELECT count(*) FROM records WHERE effective=0"),
        daily: scalar("SELECT count(*) FROM daily"),
        receipts: scalar("SELECT count(*) FROM receipts"),
        view_body: scalar("SELECT coalesce(sum(length(body)),0) FROM records"),
        v12_blob_body,
        compressed: scalar(&format!(
            "SELECT count(*) FROM records WHERE json_extract(body,'$._usageCodec')='{}'",
            CODEC_V11
        )),
        shapes: scalar("SELECT count(*) FROM body_shapes"),
        metrics: scalar("SELECT count(*) FROM usage_metrics"),
        dbstat_bytes,
        dbstat_payload,
        dbstat_unused,
        dbstat_by_name,
        table_rows,
        table_stats,
    }
}

fn run_capacity(profile: Profile) {
    let dir = tempfile::tempdir().unwrap();
    let mut legacy = seed_v11(dir.path(), profile);
    println!(
        "v12 capacity seed complete rows={}",
        profile.proxy + profile.sessions
    );
    let seed_files = file_bytes(dir.path());
    let seeded = storage_size(dir.path());
    let conflicts = conflict_count(profile);
    assert_eq!(seeded.rows, profile.proxy + profile.sessions);
    assert_eq!(
        seeded.duplicates,
        profile.no_response.saturating_sub(conflicts)
            + (profile.sessions - profile.grok - profile.no_response) / 7 * 6
    );
    assert!(
        seeded.compressed > 0,
        "v0.25.7 baseline must use the real codec"
    );
    let recent_proxy = Connection::open(dir.path().join("usage.sqlite"))
        .unwrap()
        .query_row(
            "SELECT count(*) FROM records WHERE source='proxy' AND time>=?1",
            [FIXED_NOW - 7 * DAY],
            |row| row.get::<_, u64>(0),
        )
        .unwrap();
    assert!(recent_proxy >= profile.proxy * 2 / 3 - 1_000);

    let mut baseline_peak = seed_files;
    legacy.compact(FIXED_NOW).unwrap();
    sample_peak(&mut baseline_peak, dir.path());
    legacy.reclaim_space().unwrap();
    sample_peak(&mut baseline_peak, dir.path());
    println!("v12 capacity baseline compact complete");
    let baseline_files = file_bytes(dir.path());
    let before_size = storage_size(dir.path());
    let before_views = views(&legacy, "baseline");
    drop(legacy);

    let mut migrated = Store::open(dir.path()).unwrap();
    let mut migration_peak = baseline_files;
    migrate(&mut migrated, dir.path(), &mut migration_peak);
    let migration_files = file_bytes(dir.path());
    println!("v12 capacity cutover migration complete");
    let mut fixed_peak = migration_files;
    compact_fixed(&mut migrated, dir.path(), &mut fixed_peak);
    let fixed_files = file_bytes(dir.path());
    println!("v12 capacity cutover fixed compaction complete");
    let mut reclaim_peak = fixed_files;
    migrated.reclaim_space().unwrap();
    sample_peak(&mut reclaim_peak, dir.path());
    let reclaim_files = file_bytes(dir.path());
    println!("v12 capacity cutover reclaim complete");
    let after_size = storage_size(dir.path());
    assert_eq!(
        before_views,
        views(&migrated, "cutover"),
        "v12 migration changed source views"
    );
    assert!(
        after_size.daily > 0,
        "historical daily rows must survive migration"
    );
    assert!(
        after_size.receipts > 0,
        "historical receipt keys must survive migration"
    );
    assert!(after_size.metrics > 0);
    println!(
        "v12 capacity profile proxy={} sessions={} before={before_size:?} after={after_size:?}",
        profile.proxy, profile.sessions
    );
    println!(
        "v12 storage bytes view_body={} v12_blob_body={} dbstat_bytes={} dbstat_payload={} dbstat_unused={} shapes={} metrics={}",
        after_size.view_body,
        after_size.v12_blob_body,
        after_size.dbstat_bytes,
        after_size.dbstat_payload,
        after_size.dbstat_unused,
        after_size.shapes,
        after_size.metrics
    );
    for stat in &after_size.dbstat_by_name {
        println!(
            "v12 dbstat name={} bytes={} payload={} unused={}",
            stat.name, stat.bytes, stat.payload, stat.unused
        );
    }
    for (table, rows) in &after_size.table_rows {
        println!("v12 table_rows name={} rows={}", table, rows);
    }
    for stat in &after_size.table_stats {
        println!(
            "v12 table={} rows={} body_bytes={} key_bytes={}",
            stat.table, stat.rows, stat.body_bytes, stat.key_bytes
        );
    }
    println!(
        "v12 capacity file_bytes seed={seed_files:?} baseline_peak={baseline_peak:?} baseline_final={baseline_files:?} migration_peak={migration_peak:?} migration_final={migration_files:?} fixed_peak={fixed_peak:?} fixed_final={fixed_files:?} reclaim_peak={reclaim_peak:?} final={reclaim_files:?}"
    );
    let before_bytes = before_size.db.saturating_add(before_size.wal);
    let after_bytes = after_size.db.saturating_add(after_size.wal);
    println!(
        "v12 capacity bytes before={} after={} reduction_percent={:.3}",
        before_bytes,
        after_bytes,
        100.0 * before_bytes.saturating_sub(after_bytes) as f64 / before_bytes.max(1) as f64
    );
    let within_target = after_bytes <= CAPACITY_TARGET_BYTES;
    let meets_reduction = after_bytes.saturating_mul(2) <= before_bytes;
    if !within_target || !meets_reduction {
        let retained = dir.keep();
        println!("v12 capacity retained_dir={}", retained.display());
        assert!(
            within_target,
            "v12 storage exceeds 80 MiB: {after_bytes} bytes"
        );
        assert!(
            meets_reduction,
            "v12 storage reduction is below 50 percent: before={before_bytes} after={after_bytes}"
        );
    }
}

#[test]
fn v12_fixture_reconcile_semantics_are_deterministic() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open_v11_fixture(dir.path()).unwrap();
    let proxy_exact = record("proxy", 1, 1, Some(hash("response", 1)), FIXED_NOW);
    let exact = record("codex", 2, 1, Some(hash("response", 1)), FIXED_NOW + 1_000);
    let proxy_strict = record("proxy", 7, 7, Some(hash("response", 7)), FIXED_NOW + 2_000);
    let strict = record("codex", 3, 7, None, FIXED_NOW + 3_000);
    let proxy_conflict = record("proxy", 8, 8, Some(hash("response", 8)), FIXED_NOW + 4_000);
    let conflict_a = record("codex", 4, 8, None, FIXED_NOW + 5_000);
    let conflict_b = record("codex", 5, 8, None, FIXED_NOW + 6_000);
    let grok = record("grok", 6, 9, None, FIXED_NOW + 7_000);
    store
        .write_batch(
            &[
                proxy_exact,
                exact,
                proxy_strict,
                strict,
                proxy_conflict,
                conflict_a,
                conflict_b,
                grok,
            ],
            None,
        )
        .unwrap();
    let connection = Connection::open(dir.path().join("usage.sqlite")).unwrap();
    let mut statement = connection
        .prepare("SELECT id,effective,body,duplicate_of FROM records ORDER BY id")
        .unwrap();
    let encoded_rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    drop(statement);
    let rows = encoded_rows
        .into_iter()
        .map(|(id, effective, body, target)| {
            let decoded: Record = bodies::decode(&connection, &body).unwrap();
            (id, effective, decoded.deduplication, target)
        })
        .collect::<Vec<_>>();
    assert!(rows.iter().any(|(_, effective, rule, target)| {
        *effective == 0 && rule == "response_id" && target.is_some()
    }));
    assert!(rows.iter().any(|(_, effective, rule, target)| {
        *effective == 0 && rule == "strict_match" && target.is_some()
    }));
    assert!(rows
        .iter()
        .any(|(_, effective, rule, _)| { *effective == 2 && rule == "ambiguous" }));
    assert!(rows
        .iter()
        .any(|(id, effective, _, _)| { id == &hash("record", 6) && *effective == 1 }));
}

#[test]
#[ignore = "100k synthetic v12 storage capacity gate; run explicitly per storage change"]
fn v12_hundred_thousand_rows_capacity_gate() {
    run_capacity(SMALL);
}

#[test]
#[ignore = "257k synthetic v12 storage capacity gate; run explicitly per storage change"]
fn v12_257k_rows_capacity_gate() {
    run_capacity(LARGE);
}
