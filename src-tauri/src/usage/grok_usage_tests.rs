// 覆盖 Grok 多调用轮次及来源独立统计。
//! Grok ACP session usage fixtures. All sessions and counters are synthetic.
use super::{
    model::{safe_id, Attempt, Filter, Record, Settings, Tokens},
    pricing::Pricing,
    sessions,
    store::Store,
};
use serde_json::{json, Value};
use std::{fs, path::Path};

const DAY: i64 = 86_400_000;
const BASE: i64 = 1_700_000_000_000;

fn fixture() -> (tempfile::TempDir, Store, Pricing, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let store_dir = dir.path().join("usage-store");
    let pricing_dir = dir.path().join("pricing");
    let home = dir.path().join("fixture-home");
    fs::create_dir_all(&store_dir).unwrap();
    fs::create_dir_all(&pricing_dir).unwrap();
    fs::create_dir_all(&home).unwrap();
    (
        dir,
        Store::open(&store_dir).unwrap(),
        Pricing::new(&pricing_dir).unwrap(),
        home,
    )
}

fn update(at: i64, session: &str, prompt: &str, usage: Option<Value>) -> String {
    let mut body = json!({
        "sessionUpdate": "turn_completed",
        "prompt_id": prompt,
        "stop_reason": "end_turn",
        "_meta": {"modelId": "fixture-grok-model"}
    });
    if let Some(usage) = usage {
        body["usage"] = usage;
    }
    json!({
        "type": "session_update",
        "timestamp": at,
        "params": {"sessionId": session, "update": body}
    })
    .to_string()
}

fn write_update(home: &Path, relative: &str, line: &str) {
    let path = home.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, format!("{line}\n")).unwrap();
}

fn sync_grok(store: &mut Store, pricing: &Pricing, home: &Path) -> sessions::Report {
    sessions::sync(store, pricing, &Settings::default(), "grok", home, false).unwrap()
}

fn grok_id(session: &str, prompt: &str) -> String {
    let session = safe_id(session);
    safe_id(&format!("grok:{session}:{prompt}"))
}

fn source_filter(source: &str) -> Filter {
    Filter {
        source: Some(source.into()),
        ..Filter::default()
    }
}

#[derive(Clone, Copy)]
struct ExpectedGrokModel {
    model: &'static str,
    calls: u64,
    input: u64,
    cache_read: u64,
    cache_write: u64,
    output: u64,
}

fn model_source_filter(model: &str) -> Filter {
    Filter {
        source: Some("grok_sessions".into()),
        model: Some(model.into()),
        ..Filter::default()
    }
}

fn assert_grok_model_projection(store: &Store, expected: ExpectedGrokModel, detail: bool) {
    let filter = model_source_filter(expected.model);
    let page = store.logs(&filter).unwrap();
    assert_eq!(page.total, if detail { 1 } else { 0 });
    if detail {
        assert_eq!(page.rows.len(), 1);
        assert_eq!(page.rows[0].attempts.len(), 2);
        assert!(page.rows[0]
            .attempts
            .iter()
            .any(|attempt| attempt.response_model.as_deref() == Some(expected.model)));
    }

    let dashboard = store.dashboard(&filter).unwrap();
    assert_eq!(dashboard.totals.requests, 1);
    assert_eq!(dashboard.totals.attempts, expected.calls);
    assert_eq!(dashboard.totals.tokens.input, Some(expected.input));
    assert_eq!(
        dashboard.totals.tokens.cache_read,
        Some(expected.cache_read)
    );
    assert_eq!(
        dashboard.totals.tokens.cache_write,
        Some(expected.cache_write)
    );
    assert_eq!(dashboard.totals.tokens.output, Some(expected.output));
    assert_eq!(dashboard.models.len(), 1);
    let model = &dashboard.models[0];
    assert_eq!(model.id, expected.model);
    assert_eq!(model.totals.requests, 1);
    assert_eq!(model.totals.attempts, expected.calls);
    assert_eq!(model.totals.tokens.input, Some(expected.input));
    assert_eq!(model.totals.tokens.cache_read, Some(expected.cache_read));
    assert_eq!(model.totals.tokens.cache_write, Some(expected.cache_write));
    assert_eq!(model.totals.tokens.output, Some(expected.output));
}

fn fixture_proxy(at: i64) -> Record {
    let tokens = Tokens {
        inclusive_input: Some(300),
        input: Some(250),
        output: Some(10),
        cache_read: Some(30),
        cache_write: Some(20),
        ..Tokens::default()
    };
    Record {
        id: safe_id("fixture-proxy-grok-record"),
        client: "grok".into(),
        source: "proxy".into(),
        started_at: at,
        finished_at: Some(at),
        completed: true,
        attempts: vec![Attempt {
            id: safe_id("fixture-proxy-grok-attempt"),
            requested_model: Some("fixture-grok-model".into()),
            response_model: Some("fixture-grok-model".into()),
            pricing_model: Some("fixture-grok-model".into()),
            status: Some(200),
            outcome: "success".into(),
            started_at: at,
            transport: "http".into(),
            tokens,
            ..Attempt::default()
        }],
        ..Record::default()
    }
}

#[test]
fn completed_prompt_subtracts_cache_from_inclusive_input_without_adding_reasoning_to_output() {
    let (_dir, mut store, pricing, home) = fixture();
    let line = update(
        BASE,
        "fixture-grok-session-ordinary",
        "fixture-grok-prompt-ordinary",
        Some(json!({
            "modelCalls": 3,
            "inputTokens": 1_000,
            "cachedReadTokens": 200,
            "cacheCreationTokens": 100,
            "outputTokens": 90,
            "reasoning": 41
        })),
    );
    write_update(&home, "sessions/live/updates.jsonl", &line);

    let report = sync_grok(&mut store, &pricing, &home);
    assert_eq!(report.imported, 1);
    let page = store.logs(&source_filter("grok_sessions")).unwrap();
    assert_eq!(page.total, 1);
    let attempt = page.rows[0].final_attempt().unwrap();
    assert_eq!(page.rows[0].source, "grok");
    assert_eq!(attempt.transport, "session_prompt");
    assert_eq!(attempt.repeat_count, 3);
    assert_eq!(attempt.usage_status, "reported");
    assert_eq!(attempt.tokens.inclusive_input, Some(1_000));
    assert_eq!(attempt.tokens.input, Some(700));
    assert_eq!(attempt.tokens.cache_read, Some(200));
    assert_eq!(attempt.tokens.cache_write, Some(100));
    assert_eq!(attempt.tokens.output, Some(90));
}

#[test]
fn model_usage_dynamic_keys_become_per_model_attempts_with_reported_output_only() {
    let (dir, mut store, pricing, home) = fixture();
    let line = update(
        BASE + 1_000,
        "fixture-grok-session-multi",
        "fixture-grok-prompt-multi",
        Some(json!({
            "modelCalls": 3,
            "inputTokens": 1_000,
            "cachedReadTokens": 200,
            "cacheCreationTokens": 100,
            "outputTokens": 90,
            "reasoning": 40,
            "modelUsage": {
                "fixture-grok-a": {
                    "modelCalls": 2,
                    "inputTokens": 400,
                    "cachedReadTokens": 80,
                    "cacheCreationTokens": 20,
                    "outputTokens": 30,
                    "reasoning": 10
                },
                "fixture-grok-b": {
                    "modelCalls": 1,
                    "inputTokens": 600,
                    "cachedReadTokens": 120,
                    "cacheCreationTokens": 80,
                    "outputTokens": 60,
                    "reasoning": 30
                }
            }
        })),
    );
    write_update(&home, "sessions/live/updates.jsonl", &line);

    let report = sync_grok(&mut store, &pricing, &home);
    assert_eq!(report.imported, 1);
    let page = store.logs(&source_filter("grok_sessions")).unwrap();
    assert_eq!(page.total, 1);
    let record = &page.rows[0];
    assert_eq!(record.attempts.len(), 2);

    let a = record
        .attempts
        .iter()
        .find(|attempt| attempt.response_model.as_deref() == Some("fixture-grok-a"))
        .unwrap();
    assert_eq!(a.repeat_count, 2);
    assert_eq!(a.tokens.inclusive_input, Some(400));
    assert_eq!(a.tokens.input, Some(300));
    assert_eq!(a.tokens.cache_read, Some(80));
    assert_eq!(a.tokens.cache_write, Some(20));
    assert_eq!(a.tokens.output, Some(30));

    let b = record
        .attempts
        .iter()
        .find(|attempt| attempt.response_model.as_deref() == Some("fixture-grok-b"))
        .unwrap();
    assert_eq!(b.repeat_count, 1);
    assert_eq!(b.tokens.inclusive_input, Some(600));
    assert_eq!(b.tokens.input, Some(400));
    assert_eq!(b.tokens.cache_read, Some(120));
    assert_eq!(b.tokens.cache_write, Some(80));
    assert_eq!(b.tokens.output, Some(60));

    let total = record.tokens();
    assert_eq!(total.inclusive_input, Some(1_000));
    assert_eq!(total.input, Some(700));
    assert_eq!(total.cache_read, Some(200));
    assert_eq!(total.cache_write, Some(100));
    assert_eq!(total.output, Some(90));

    let model_a = ExpectedGrokModel {
        model: "fixture-grok-a",
        calls: 2,
        input: 300,
        cache_read: 80,
        cache_write: 20,
        output: 30,
    };
    let model_b = ExpectedGrokModel {
        model: "fixture-grok-b",
        calls: 1,
        input: 400,
        cache_read: 120,
        cache_write: 80,
        output: 60,
    };
    let source = source_filter("grok_sessions");
    let dashboard = store.dashboard(&source).unwrap();
    assert_eq!(dashboard.totals.requests, 1);
    assert_eq!(dashboard.totals.attempts, 3);
    assert_eq!(dashboard.totals.tokens.input, Some(700));
    assert_eq!(dashboard.totals.tokens.cache_read, Some(200));
    assert_eq!(dashboard.totals.tokens.cache_write, Some(100));
    assert_eq!(dashboard.totals.tokens.output, Some(90));

    assert_grok_model_projection(&store, model_a, true);
    assert_grok_model_projection(&store, model_b, true);

    store.compact(BASE + 40 * DAY).unwrap();
    assert_grok_model_projection(&store, model_a, false);
    assert_grok_model_projection(&store, model_b, false);

    drop(store);
    let reopened = Store::open(&dir.path().join("usage-store")).unwrap();
    assert_grok_model_projection(&reopened, model_a, false);
    assert_grok_model_projection(&reopened, model_b, false);
}

#[test]
fn missing_usage_stays_unknown_and_live_archived_duplicates_are_idempotent() {
    let (_dir, mut store, pricing, home) = fixture();
    let line = update(
        BASE + 2_000,
        "fixture-grok-session-missing",
        "fixture-grok-prompt-missing",
        None,
    );
    write_update(&home, "sessions/live/updates.jsonl", &line);
    write_update(&home, "archived_sessions/archive/updates.jsonl", &line);

    let first = sync_grok(&mut store, &pricing, &home);
    assert_eq!(first.total_files, 2);
    assert_eq!(first.imported, 2);
    let second = sync_grok(&mut store, &pricing, &home);
    assert_eq!(second.files, 2);
    assert_eq!(second.imported, 0);

    let page = store.logs(&source_filter("grok_sessions")).unwrap();
    assert_eq!(page.total, 1);
    let record = store
        .detail(&grok_id(
            "fixture-grok-session-missing",
            "fixture-grok-prompt-missing",
        ))
        .unwrap();
    let attempt = record.final_attempt().unwrap();
    assert_eq!(record.source, "grok");
    assert_eq!(attempt.usage_status, "missing");
    assert_eq!(attempt.tokens.total(), None);
    assert_eq!(attempt.tokens.inclusive_input, None);
    assert_eq!(attempt.tokens.input, None);
    assert_eq!(attempt.tokens.output, None);
}

#[test]
fn equal_proxy_fingerprint_and_time_remain_outside_grok_and_sessions_views() {
    let (_dir, mut store, pricing, home) = fixture();
    let at = BASE + 3_000;
    let line = update(
        at,
        "fixture-grok-session-proxy-peer",
        "fixture-grok-prompt-proxy-peer",
        Some(json!({
            "modelCalls": 1,
            "inputTokens": 300,
            "cachedReadTokens": 30,
            "cacheCreationTokens": 20,
            "outputTokens": 10
        })),
    );
    write_update(&home, "sessions/live/updates.jsonl", &line);
    sync_grok(&mut store, &pricing, &home);

    let grok = store
        .detail(&grok_id(
            "fixture-grok-session-proxy-peer",
            "fixture-grok-prompt-proxy-peer",
        ))
        .unwrap();
    let proxy = fixture_proxy(at);
    assert_eq!(grok.signature(), proxy.signature());
    assert_eq!(grok.event_time(), proxy.event_time());
    store
        .write_batch(std::slice::from_ref(&proxy), None)
        .unwrap();

    let grok_page = store.logs(&source_filter("grok_sessions")).unwrap();
    let proxy_page = store.logs(&source_filter("proxy")).unwrap();
    let sessions_page = store.logs(&source_filter("sessions")).unwrap();
    let default_page = store.logs(&Filter::default()).unwrap();
    assert_eq!(grok_page.total, 1);
    assert_eq!(grok_page.rows[0].id, grok.id);
    assert_eq!(proxy_page.total, 1);
    assert_eq!(proxy_page.rows[0].id, proxy.id);
    assert_eq!(sessions_page.total, 0);
    assert_eq!(default_page.total, 1);
    assert_eq!(default_page.rows[0].source, "proxy");

    let default = store.dashboard(&Filter::default()).unwrap();
    assert_eq!(default.totals.requests, 1);
    assert_eq!(default.sources.get("proxy"), Some(&1));
    assert!(!default.sources.contains_key("grok"));
    assert_eq!(
        store
            .dashboard(&source_filter("sessions"))
            .unwrap()
            .totals
            .requests,
        0
    );
    assert_eq!(
        store
            .dashboard(&source_filter("grok_sessions"))
            .unwrap()
            .totals
            .requests,
        1
    );
}

#[test]
fn grok_source_projection_survives_database_compaction_and_reopen() {
    let (dir, mut store, pricing, home) = fixture();
    let at = BASE + DAY;
    let line = update(
        at,
        "fixture-grok-session-compaction",
        "fixture-grok-prompt-compaction",
        Some(json!({
            "modelCalls": 1,
            "inputTokens": 100,
            "cachedReadTokens": 20,
            "cacheCreationTokens": 10,
            "outputTokens": 7
        })),
    );
    write_update(&home, "sessions/live/updates.jsonl", &line);
    sync_grok(&mut store, &pricing, &home);
    let grok_scope = source_filter("grok_sessions");
    assert_eq!(store.dashboard(&grok_scope).unwrap().totals.requests, 1);

    store.compact(BASE + 40 * DAY).unwrap();
    assert_eq!(store.logs(&grok_scope).unwrap().total, 0);
    let compacted = store.dashboard(&grok_scope).unwrap();
    assert_eq!(compacted.totals.requests, 1);
    assert_eq!(compacted.totals.tokens.input, Some(70));
    assert_eq!(compacted.totals.tokens.cache_read, Some(20));
    assert_eq!(compacted.totals.tokens.cache_write, Some(10));
    assert_eq!(compacted.totals.tokens.output, Some(7));

    drop(store);
    let reopened = Store::open(&dir.path().join("usage-store")).unwrap();
    let after_reopen = reopened.dashboard(&grok_scope).unwrap();
    assert_eq!(after_reopen.totals.requests, 1);
    assert_eq!(after_reopen.totals.tokens.input, Some(70));
    assert_eq!(after_reopen.totals.tokens.output, Some(7));
}
