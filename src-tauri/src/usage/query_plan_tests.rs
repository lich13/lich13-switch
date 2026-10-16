//! Synthetic-only checks for the usage query plans and projection grains.
//! The trace records only SQL text needed for EXPLAIN; output is restricted to
//! plan nodes, index names, row counts, and synthetic storage size.

use super::super::model::{Attempt, Filter, Operation, Record, Tokens};
use super::*;
use rusqlite::{ffi, Connection};
use std::{
    collections::BTreeSet,
    ffi::CStr,
    os::raw::{c_int, c_void},
    panic::{catch_unwind, AssertUnwindSafe},
    ptr,
};

const DAY_MS: i64 = 86_400_000;
const BASE: i64 = 1_700_000_000_000;
const PLAN_ROWS: u64 = 6_000;

fn fixture_record(index: u64, source: &str, at: i64) -> Record {
    let operation = match index % 3 {
        1 => Operation::WebSearch,
        2 => Operation::Compaction,
        _ => Operation::Model,
    };
    let model = format!("fixture-model-{}", index % 8);
    let status: Option<u16> = (source == "proxy").then(|| match index % 4 {
        0 => 200,
        1 => 201,
        2 => 429,
        _ => 500,
    });
    Record {
        id: format!("fixture-query-{index:06}"),
        client: format!("fixture-client-{}", index % 8),
        source: source.into(),
        started_at: at,
        completed: true,
        attempts: vec![Attempt {
            id: format!("fixture-query-attempt-{index:06}"),
            provider: (source == "proxy").then(|| format!("fixture-provider-{}", index % 8)),
            requested_model: Some(model.clone()),
            response_model: Some(model.clone()),
            pricing_model: Some(model),
            response_id: Some(format!("fixture-query-response-{index:06}")),
            status,
            outcome: if status.is_some() {
                "success"
            } else {
                "session"
            }
            .into(),
            started_at: at,
            duration_ms: 500 + index % 200,
            first_token_ms: (source == "proxy").then_some(50),
            stream: source == "proxy",
            transport: if source == "proxy" { "http" } else { "session" }.into(),
            operation,
            tokens: Tokens {
                input: Some(100 + index % 100),
                output: Some(2 + index % 7),
                ..Tokens::default()
            },
            ..Attempt::default()
        }],
        ..Record::default()
    }
}

fn logical_record(id: &str, source: &str, response: &str, input: u64, at: i64) -> Record {
    Record {
        id: id.into(),
        client: "fixture-client".into(),
        source: source.into(),
        started_at: at,
        completed: true,
        attempts: vec![Attempt {
            id: format!("{id}-attempt"),
            provider: (source == "proxy").then(|| "fixture-provider".into()),
            requested_model: Some("fixture-model".into()),
            response_model: Some("fixture-model".into()),
            pricing_model: Some("fixture-model".into()),
            response_id: Some(response.into()),
            status: (source == "proxy").then_some(200),
            outcome: "success".into(),
            started_at: at,
            duration_ms: 1_000,
            first_token_ms: (source == "proxy").then_some(100),
            stream: source == "proxy",
            transport: if source == "proxy" { "http" } else { "session" }.into(),
            operation: Operation::Model,
            tokens: Tokens {
                input: Some(input),
                output: Some(2),
                ..Tokens::default()
            },
            ..Attempt::default()
        }],
        ..Record::default()
    }
}

#[derive(Default)]
struct TraceCapture {
    statements: Vec<String>,
    failed: bool,
}

struct SqliteAllocation(*mut std::os::raw::c_char);

impl Drop for SqliteAllocation {
    fn drop(&mut self) {
        unsafe { ffi::sqlite3_free(self.0.cast::<c_void>()) };
    }
}

fn plan_candidate(sql: &str) -> bool {
    let sql = sql.trim_start().to_ascii_lowercase();
    let touches_usage = [" records", " daily", " usage_metrics", " receipts"]
        .iter()
        .any(|table| sql.contains(table));
    touches_usage
        && (sql.starts_with("select ")
            || sql.starts_with("with ")
            || sql.starts_with("delete from records ")
            || sql.starts_with("delete from usage_metrics ")
            || sql.starts_with("insert or ignore into receipts(")
            || sql.starts_with("insert or ignore into receipts "))
}

unsafe extern "C" fn trace_statement(
    event: u32,
    context: *mut c_void,
    statement: *mut c_void,
    _detail: *mut c_void,
) -> c_int {
    if event != ffi::SQLITE_TRACE_STMT || context.is_null() || statement.is_null() {
        return ffi::SQLITE_OK;
    }
    let capture = context.cast::<TraceCapture>();
    let result = catch_unwind(AssertUnwindSafe(|| {
        let expanded = ffi::sqlite3_expanded_sql(statement.cast::<ffi::sqlite3_stmt>());
        if expanded.is_null() {
            (*capture).failed = true;
            return;
        }
        let expanded = SqliteAllocation(expanded);
        let sql = CStr::from_ptr(expanded.0).to_string_lossy().into_owned();
        if plan_candidate(&sql) {
            (*capture).statements.push(sql);
        }
    }));
    if result.is_err() {
        (*capture).failed = true;
    }
    ffi::SQLITE_OK
}

struct TraceGuard<'a> {
    store: &'a mut Store,
    capture: Box<TraceCapture>,
    active: bool,
}

impl<'a> TraceGuard<'a> {
    fn new(store: &'a mut Store) -> Self {
        let mut capture = Box::<TraceCapture>::default();
        let context = (&mut *capture as *mut TraceCapture).cast::<c_void>();
        let result = unsafe {
            ffi::sqlite3_trace_v2(
                store.connection.handle(),
                ffi::SQLITE_TRACE_STMT,
                Some(trace_statement),
                context,
            )
        };
        assert_eq!(result, ffi::SQLITE_OK, "could not enable SQLite trace");
        Self {
            store,
            capture,
            active: true,
        }
    }

    fn store(&mut self) -> &mut Store {
        self.store
    }

    fn disable(&mut self) {
        if !self.active {
            return;
        }
        unsafe {
            ffi::sqlite3_trace_v2(self.store.connection.handle(), 0, None, ptr::null_mut());
        }
        self.active = false;
    }

    fn finish(mut self) -> TraceCapture {
        self.disable();
        std::mem::take(&mut *self.capture)
    }
}

impl Drop for TraceGuard<'_> {
    fn drop(&mut self) {
        self.disable();
    }
}

fn trace_store<T>(store: &mut Store, action: impl FnOnce(&mut Store) -> T) -> (T, TraceCapture) {
    let mut trace = TraceGuard::new(store);
    let result = action(trace.store());
    let capture = trace.finish();
    (result, capture)
}

struct PlanObservation {
    kind: &'static str,
    indexes: Vec<String>,
}

fn query_kind(sql: &str) -> &'static str {
    let sql = sql.trim_start().to_ascii_lowercase();
    if sql.contains("order by time desc,local_sequence desc,id desc") {
        "logs-page-sort"
    } else if sql.contains("union all select 'provider'") {
        "dashboard-aggregates"
    } else if sql.starts_with("insert or ignore into receipts(")
        || sql.starts_with("insert or ignore into receipts ")
    {
        "prune-receipts"
    } else if sql.starts_with("delete from usage_metrics ") {
        "prune-metrics"
    } else if sql.starts_with("delete from records ") {
        "prune-records"
    } else if sql.contains("from receipts") {
        "receipt-dedup"
    } else if sql.contains("signature=") {
        "signature-dedup"
    } else if sql.contains("response=") {
        "response-dedup"
    } else if sql.contains("from usage_metrics") {
        "metrics-read"
    } else {
        "records-read"
    }
}

fn explain_captured(
    connection: &Connection,
    label: &str,
    capture: TraceCapture,
) -> Vec<PlanObservation> {
    assert!(!capture.failed, "SQLite trace callback failed for {label}");
    let mut observations = Vec::new();
    for sql in capture.statements {
        let kind = query_kind(&sql);
        let explain = format!("EXPLAIN QUERY PLAN {sql}");
        let mut statement = connection
            .prepare(&explain)
            .unwrap_or_else(|_| panic!("EXPLAIN QUERY PLAN could not prepare {kind} in {label}"));
        let details = statement
            .query_map([], |row| row.get::<_, String>(3))
            .unwrap_or_else(|_| panic!("EXPLAIN QUERY PLAN could not read {kind} in {label}"))
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap_or_else(|_| panic!("EXPLAIN QUERY PLAN returned an invalid row in {label}"));
        assert!(
            !details.is_empty(),
            "no query-plan rows for {kind} in {label}"
        );
        let indexes = details
            .iter()
            .filter(|detail| detail.to_ascii_lowercase().contains("using index"))
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let scans = details
            .iter()
            .filter(|detail| detail.to_ascii_lowercase().starts_with("scan "))
            .count();
        let temp_btrees = details
            .iter()
            .filter(|detail| detail.contains("USE TEMP B-TREE"))
            .count();
        println!(
            "usage_query_plan case={label} kind={kind} nodes={} index_nodes={} scans={} temp_btrees={temp_btrees}",
            details.len(),
            indexes.len(),
            scans,
        );
        for index in &indexes {
            println!("usage_query_plan_index case={label} plan={index}");
        }
        observations.push(PlanObservation { kind, indexes });
    }
    assert!(
        !observations.is_empty(),
        "no production usage SQL captured for {label}"
    );
    observations
}

fn assert_sql_contains(capture: &TraceCapture, needle: &str, label: &str) {
    assert!(
        capture.statements.iter().any(|sql| sql
            .to_ascii_lowercase()
            .contains(&needle.to_ascii_lowercase())),
        "expected production SQL shape {needle:?} was not captured for {label}"
    );
}

fn inspect_logs(store: &mut Store, label: &str, filter: &Filter) -> Vec<PlanObservation> {
    let (page, capture) = trace_store(store, |store| {
        store
            .logs(filter)
            .expect("synthetic logs query should succeed")
    });
    assert!(page.total <= PLAN_ROWS + 2, "fixture log count is bounded");
    explain_captured(&store.connection, label, capture)
}

fn inspect_dashboard(store: &mut Store, label: &str, filter: &Filter) -> Vec<PlanObservation> {
    let (dashboard, capture) = trace_store(store, |store| {
        store
            .dashboard(filter)
            .expect("synthetic dashboard query should succeed")
    });
    assert!(
        dashboard.totals.requests <= PLAN_ROWS + 2,
        "fixture total is bounded"
    );
    explain_captured(&store.connection, label, capture)
}

#[test]
fn sqlite_query_plans_cover_usage_reads_dedup_and_prune() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let recent_start = BASE + 15 * DAY_MS;
    let recent_end = recent_start + PLAN_ROWS as i64 * 1_000 + 1_000;
    for start in (0..PLAN_ROWS).step_by(256) {
        let end = (start + 256).min(PLAN_ROWS);
        let rows = (start..end)
            .map(|index| {
                let source = match index % 10 {
                    6..=8 => "proxy",
                    9 => "grok",
                    _ => "codex",
                };
                fixture_record(index, source, recent_start + index as i64 * 1_000)
            })
            .collect::<Vec<_>>();
        store.write_batch(&rows, None).unwrap();
    }
    store.connection.execute_batch("ANALYZE;").unwrap();
    println!(
        "usage_query_fixture records={PLAN_ROWS} body_bytes={}",
        store
            .connection
            .query_row(
                "SELECT coalesce(sum(length(body)),0) FROM records",
                [],
                |row| row.get::<_, u64>(0)
            )
            .unwrap()
    );

    let range = |source: Option<&str>| Filter {
        source: source.map(str::to_owned),
        start: Some(recent_start),
        end: Some(recent_end),
        page: 2,
        ..Filter::default()
    };
    let mut observations = Vec::new();

    let all_logs = range(None);
    let (page, captured) = trace_store(&mut store, |store| store.logs(&all_logs).unwrap());
    assert_eq!(page.rows.len(), 20);
    assert_sql_contains(
        &captured,
        "order by time desc,local_sequence desc,id desc",
        "logs-page-sort",
    );
    observations.extend(explain_captured(
        &store.connection,
        "logs-page-sort",
        captured,
    ));

    for (label, source) in [
        ("proxy", Some("proxy")),
        ("sessions", Some("sessions")),
        ("grok_sessions", Some("grok_sessions")),
    ] {
        observations.extend(inspect_logs(
            &mut store,
            &format!("logs-{label}"),
            &range(source),
        ));
        observations.extend(inspect_dashboard(
            &mut store,
            &format!("dashboard-{label}"),
            &range(source),
        ));
    }
    observations.extend(inspect_dashboard(&mut store, "dashboard-all", &range(None)));

    for (label, filter) in [
        (
            "client",
            Filter {
                client: Some("fixture-client-3".into()),
                ..range(None)
            },
        ),
        (
            "provider",
            Filter {
                source: Some("proxy".into()),
                provider: Some("fixture-provider-5".into()),
                ..range(Some("proxy"))
            },
        ),
        (
            "model",
            Filter {
                source: Some("sessions".into()),
                model: Some("fixture-model-4".into()),
                ..range(Some("sessions"))
            },
        ),
        (
            "operation",
            Filter {
                operation: Some("web_search".into()),
                ..range(None)
            },
        ),
        (
            "status",
            Filter {
                status: Some("2xx".into()),
                ..range(None)
            },
        ),
        (
            "combined",
            Filter {
                source: Some("proxy".into()),
                client: Some("fixture-client-5".into()),
                provider: Some("fixture-provider-5".into()),
                model: Some("fixture-model-5".into()),
                operation: Some("web_search".into()),
                status: Some("2xx".into()),
                ..range(Some("proxy"))
            },
        ),
    ] {
        observations.extend(inspect_logs(
            &mut store,
            &format!("logs-filter-{label}"),
            &filter,
        ));
        let mut dashboard_filter = filter.clone();
        dashboard_filter.status = None;
        observations.extend(inspect_dashboard(
            &mut store,
            &format!("dashboard-filter-{label}"),
            &dashboard_filter,
        ));
    }

    let old_proxy = logical_record(
        "fixture-pruned-proxy",
        "proxy",
        "fixture-pruned-response",
        19,
        BASE,
    );
    store.write_batch(&[old_proxy], None).unwrap();
    let (_, prune_capture) = trace_store(&mut store, |store| {
        store.compact(BASE + 40 * DAY_MS).unwrap()
    });
    assert_sql_contains(&prune_capture, "insert or ignore into receipts", "prune");
    assert_sql_contains(&prune_capture, "delete from records", "prune");
    observations.extend(explain_captured(&store.connection, "prune", prune_capture));

    let late_session = logical_record(
        "fixture-late-session",
        "codex",
        "fixture-pruned-response",
        19,
        recent_start,
    );
    let (_, dedup_capture) = trace_store(&mut store, |store| {
        store.write_batch(&[late_session], None).unwrap()
    });
    assert_sql_contains(&dedup_capture, "from receipts", "dedup-receipts");
    assert_sql_contains(&dedup_capture, "signature=", "dedup-signature");
    assert_sql_contains(&dedup_capture, "response=", "dedup-response");
    observations.extend(explain_captured(
        &store.connection,
        "dedup-after-prune",
        dedup_capture,
    ));

    let late_state: (i64, Option<String>) = store
        .connection
        .query_row(
            "SELECT effective,duplicate_of FROM records WHERE id='fixture-late-session'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(late_state, (0, Some("fixture-pruned-proxy".into())));
    let record_count: u64 = store
        .connection
        .query_row("SELECT count(*) FROM records", [], |row| row.get(0))
        .unwrap();
    let body_bytes: u64 = store
        .connection
        .query_row(
            "SELECT coalesce(sum(length(body)),0) FROM records",
            [],
            |row| row.get(0),
        )
        .unwrap();
    println!("usage_query_fixture_after_prune records={record_count} body_bytes={body_bytes}");

    assert!(observations
        .iter()
        .any(|plan| plan.kind == "logs-page-sort"));
    assert!(observations
        .iter()
        .any(|plan| plan.kind == "dashboard-aggregates"));
    assert!(observations
        .iter()
        .any(|plan| plan.kind == "prune-receipts"));
    assert!(observations.iter().any(|plan| plan.kind == "prune-records"));
    assert!(observations.iter().any(|plan| plan.kind == "receipt-dedup"));
    assert!(observations
        .iter()
        .any(|plan| plan.indexes.iter().any(|node| node.contains("usage_time"))));
}

#[test]
fn record_upsert_and_source_projection_grains_preserve_usage_once() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let first = logical_record(
        "fixture-upsert-record",
        "codex",
        "fixture-upsert-response",
        10,
        BASE,
    );
    store.write_batch(&[first.clone()], None).unwrap();
    let mut updated = first.clone();
    updated.attempts[0].tokens.input = Some(20);
    store.write_batch(&[updated.clone()], None).unwrap();
    store.write_batch(&[updated], None).unwrap();

    let proxy = logical_record(
        "fixture-proxy-record",
        "proxy",
        "fixture-cross-source-response",
        10,
        BASE,
    );
    let session = logical_record(
        "fixture-session-record",
        "codex",
        "fixture-cross-source-response",
        10,
        BASE,
    );
    let grok = logical_record(
        "fixture-grok-record",
        "grok",
        "fixture-grok-response",
        10,
        BASE,
    );
    store
        .write_batch(&[proxy.clone(), session.clone(), grok.clone()], None)
        .unwrap();

    let record_pk: i64 = store
        .connection
        .query_row(
            "SELECT pk FROM pragma_table_info('records') WHERE name='id'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(record_pk, 1, "record IDs are the table primary key");
    let (record_count, upsert_tokens): (u64, u64) = store
        .connection
        .query_row(
            "SELECT count(*),(SELECT input FROM usage_metrics WHERE kind=0 AND owner='fixture-upsert-record' AND ordinal=0) FROM records",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        record_count, 4,
        "same-ID writes upsert instead of duplicating"
    );
    assert_eq!(
        upsert_tokens, 20,
        "the latest same-ID usage is projected once"
    );

    let linked: (String, i64, String) = store
        .connection
        .query_row(
            "SELECT id,effective,duplicate_of FROM records WHERE id='fixture-session-record'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(linked, (session.id.clone(), 0, proxy.id.clone()));
    assert_ne!(
        proxy.id, session.id,
        "effective=0 retains a distinct source record"
    );

    let kind_counts = {
        let mut kind_statement = store
            .connection
            .prepare(
                "SELECT kind,count(*),coalesce(sum(requests),0) FROM usage_metrics GROUP BY kind ORDER BY kind",
            )
            .unwrap();
        kind_statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, u64>(1)?,
                    row.get::<_, u64>(2)?,
                ))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    };
    assert_eq!(kind_counts, vec![(0, 4, 4), (2, 1, 1)]);

    let view_requests = |store: &Store, source: Option<&str>| {
        store
            .dashboard(&Filter {
                source: source.map(str::to_owned),
                ..Filter::default()
            })
            .unwrap()
            .totals
            .requests
    };
    assert_eq!(
        view_requests(&store, None),
        2,
        "combined billing counts the pair once"
    );
    assert_eq!(view_requests(&store, Some("proxy")), 1);
    assert_eq!(
        view_requests(&store, Some("sessions")),
        2,
        "session view retains its own row"
    );
    assert_eq!(view_requests(&store, Some("grok_sessions")), 1);

    for (source, expected) in [
        (None, vec!["fixture-proxy-record", "fixture-upsert-record"]),
        (Some("proxy"), vec!["fixture-proxy-record"]),
        (
            Some("sessions"),
            vec!["fixture-session-record", "fixture-upsert-record"],
        ),
        (Some("grok_sessions"), vec!["fixture-grok-record"]),
    ] {
        let mut ids = store
            .logs(&Filter {
                source: source.map(str::to_owned),
                ..Filter::default()
            })
            .unwrap()
            .rows
            .into_iter()
            .map(|row| row.id)
            .collect::<Vec<_>>();
        ids.sort();
        let mut expected = expected;
        expected.sort();
        assert_eq!(ids, expected);
    }

    let before = [None, Some("proxy"), Some("sessions"), Some("grok_sessions")]
        .into_iter()
        .map(|source| (source, view_requests(&store, source)))
        .collect::<Vec<_>>();
    store.compact(BASE + 40 * DAY_MS).unwrap();
    let after = [None, Some("proxy"), Some("sessions"), Some("grok_sessions")]
        .into_iter()
        .map(|source| (source, view_requests(&store, source)))
        .collect::<Vec<_>>();
    assert_eq!(after, before, "daily migration preserves each source view");

    let (remaining_records, receipt_count): (u64, u64) = store
        .connection
        .query_row(
            "SELECT (SELECT count(*) FROM records),(SELECT count(*) FROM receipts)",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(remaining_records, 0);
    assert_eq!(
        receipt_count, 4,
        "prune keeps correlation receipts for every ID"
    );
    let kind_counts = {
        let mut kind_statement = store
            .connection
            .prepare(
                "SELECT kind,count(*),coalesce(sum(requests),0) FROM usage_metrics GROUP BY kind ORDER BY kind",
            )
            .unwrap();
        kind_statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, u64>(1)?,
                    row.get::<_, u64>(2)?,
                ))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    };
    assert_eq!(kind_counts, vec![(1, 6, 6)]);
}
