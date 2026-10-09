use super::{
    model::*,
    pricing::{Config, Pricing, Quote},
    sessions,
    store::Store,
    Service,
};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

const DAY: i64 = 86_400_000;
const BASE: i64 = 20_000 * DAY;
const DEDUP_WINDOW: i64 = 10 * 60_000;
const OUTSIDE_DEDUP_WINDOW: i64 = DEDUP_WINDOW + 1;

fn local_boundary(at: i64, day_offset: i64, hour: u32) -> i64 {
    use chrono::TimeZone;
    let local = chrono::Local.timestamp_millis_opt(at).single().unwrap();
    let date = local.date_naive() + chrono::Duration::days(day_offset);
    chrono::Local
        .from_local_datetime(&date.and_hms_opt(hour, 0, 0).unwrap())
        .earliest()
        .unwrap()
        .timestamp_millis()
}

fn fixture() -> (tempfile::TempDir, Store, Pricing) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let prices = Pricing::new(dir.path()).unwrap();
    (dir, store, prices)
}

fn write_jsonl(path: &Path, rows: &[Value]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut bytes = Vec::new();
    for row in rows {
        serde_json::to_writer(&mut bytes, row).unwrap();
        bytes.push(b'\n');
    }
    fs::write(path, bytes).unwrap();
}

fn append_jsonl(path: &Path, rows: &[Value]) {
    let mut file = OpenOptions::new().append(true).open(path).unwrap();
    for row in rows {
        serde_json::to_writer(&mut file, row).unwrap();
        file.write_all(b"\n").unwrap();
    }
}

fn codex_meta(id: &str, parent: Option<&str>, at: i64) -> Value {
    json!({"type": "session_meta", "timestamp": at,
        "payload": {"id": id, "forked_from_id": parent}})
}

fn counters(input: u64, cached: u64, output: u64) -> Value {
    json!({"input_tokens": input, "cached_input_tokens": cached,
        "output_tokens": output})
}

fn codex_count(at: i64, total: Option<Value>, last: Option<Value>) -> Value {
    json!({"type": "event_msg", "timestamp": at, "payload": {
        "type": "token_count", "info": {"model": "gpt-fixture",
            "total_token_usage": total, "last_token_usage": last}}})
}

fn claude_user(uuid: &str, at: i64) -> Value {
    json!({"type": "user", "sessionId": "fixture-session", "uuid": uuid,
        "timestamp": at, "message": {"content": "fixture-content.example.invalid"}})
}

fn claude_message(
    id: &str,
    uuid: &str,
    parent: Option<&str>,
    at: i64,
    output: u64,
    done: bool,
) -> Value {
    json!({"type": "assistant", "sessionId": "fixture-session", "uuid": uuid,
        "parentUuid": parent, "timestamp": at, "message": {
            "id": id, "model": "claude-fixture", "stop_reason": if done {Some("end_turn")} else {None},
            "usage": {"input_tokens": 100, "output_tokens": output,
                "cache_read_input_tokens": 40, "cache_creation_input_tokens": 20,
                "cache_creation": {"ephemeral_5m_input_tokens": 20}}}})
}

fn import(store: &mut Store, prices: &Pricing, client: &str, home: &Path) -> sessions::Report {
    sessions::sync(store, prices, &Settings::default(), client, home, false).unwrap()
}

fn all_time() -> Filter {
    Filter {
        start: Some(BASE),
        end: Some(BASE + 100 * DAY),
        ..Filter::default()
    }
}

fn record(id: &str, client: &str, source: &str, at: i64) -> Record {
    Record {
        id: id.into(),
        client: client.into(),
        source: source.into(),
        started_at: at,
        completed: true,
        attempts: vec![Attempt {
            id: format!("{id}-attempt"),
            started_at: at,
            requested_model: Some("gpt-fixture".into()),
            response_model: Some("gpt-fixture".into()),
            pricing_model: Some("gpt-fixture".into()),
            cost_multiplier: "1".into(),
            provider: (source == "proxy").then(|| "fixture-provider".into()),
            status: (source == "proxy").then_some(200),
            tokens: Tokens {
                input: Some(100),
                output: Some(10),
                cache_read: Some(20),
                ..Tokens::default()
            },
            outcome: "completed".into(),
            transport: if source == "proxy" { "http" } else { "session" }.into(),
            ..Attempt::default()
        }],
        ..Record::default()
    }
}

fn quote(data: Value, multiplier: &str) -> Quote {
    Quote {
        model: "fixture-model".into(),
        data: Some(data),
        version: "fixture-v1".into(),
        source: "fixed".into(),
        multiplier: multiplier.into(),
    }
}

fn install_fixed(prices: &Pricing, entries: &[(&str, Value)]) {
    let view = prices.view();
    let mut config = view.config;
    config.auto_update = false;
    for (id, data) in entries {
        config.fixed.insert((*id).into(), data.clone());
    }
    prices.configure(config, &view.revision).unwrap();
}

fn cost(snapshot: Option<PriceSnapshot>) -> String {
    snapshot.expect("fixture should be fully priced").cost
}

fn price_snapshot(cost: &str) -> PriceSnapshot {
    PriceSnapshot {
        version: "fixture-price-v1".into(),
        source: "fixture-catalog".into(),
        model: "fixture-model".into(),
        multiplier: "1".into(),
        rates: BTreeMap::from([("input".into(), "0.00001".into())]),
        cost: cost.into(),
        basis: None,
    }
}

#[test]
fn protocol_cache_rules_keep_missing_and_zero_distinct() {
    let usage = json!({"input_tokens": 1000, "output_tokens": 40,
        "cache_read_input_tokens": 600, "cache_creation_input_tokens": 100,
        "cache_creation": {"ephemeral_5m_input_tokens": 80, "ephemeral_1h_input_tokens": 20}});
    let openai = parse_tokens(&usage, false);
    let claude = parse_tokens(&usage, true);
    assert_eq!(openai.input, Some(300));
    assert_eq!(openai.total(), Some(1040));
    assert_eq!(claude.input, Some(1000));
    assert_eq!(claude.total(), Some(1740));
    assert_eq!(claude.cache_write_5m, Some(80));
    assert_eq!(claude.cache_write_1h, Some(20));
    assert_eq!(parse_tokens(&json!({}), false).total(), None);
    assert_eq!(
        parse_tokens(&json!({"input_tokens": 0}), false).input,
        Some(0)
    );
    assert_eq!(
        parse_tokens(&json!({"input_tokens": -1}), false).input,
        None
    );
}

#[test]
fn codex_cumulative_cache_deltas_ignore_replays_but_keep_repeated_real_requests() {
    let (dir, mut store, prices) = fixture();
    let path = dir.path().join("home/sessions/fixture.jsonl");
    write_jsonl(
        &path,
        &[
            codex_meta("fixture-codex", None, BASE),
            json!({"type": "turn_context", "timestamp": BASE, "payload": {"model": "gpt-fixture"}}),
            codex_count(BASE + 100, Some(counters(1000, 600, 40)), None),
            codex_count(BASE + 150, Some(counters(1000, 600, 40)), None),
            codex_count(BASE + 200, Some(counters(1500, 900, 65)), None),
            codex_count(BASE + 300, None, Some(counters(100, 40, 10))),
            codex_count(BASE + 400, None, Some(counters(100, 40, 10))),
        ],
    );
    let original = fs::read(&path).unwrap();
    let report = import(&mut store, &prices, "codex", &dir.path().join("home"));
    assert_eq!((report.files, report.imported, report.errors), (1, 4, 0));
    let dashboard = store.dashboard(&all_time()).unwrap();
    assert_eq!(dashboard.totals.requests, 4);
    assert_eq!(dashboard.totals.tokens.input, Some(720));
    assert_eq!(dashboard.totals.tokens.cache_read, Some(980));
    assert_eq!(dashboard.totals.tokens.output, Some(85));
    assert_eq!(
        (
            dashboard.totals.status_known,
            dashboard.totals.success,
            dashboard.totals.sessions
        ),
        (0, 0, 4)
    );
    assert!(store
        .logs(&Filter::default())
        .unwrap()
        .rows
        .iter()
        .all(|r| r.final_attempt().unwrap().status.is_none()));
    assert_eq!(
        import(&mut store, &prices, "codex", &dir.path().join("home")).imported,
        0
    );
    assert_eq!(store.logs(&Filter::default()).unwrap().total, 4);
    assert_eq!(fs::read(path).unwrap(), original);
}

#[test]
fn codex_out_of_order_cumulative_counters_keep_the_high_water_mark() {
    let (dir, mut store, prices) = fixture();
    let root = dir.path().join("home");
    write_jsonl(
        &root.join("sessions/fixture.jsonl"),
        &[
            codex_meta("fixture-codex", None, BASE),
            codex_count(BASE + 100, Some(counters(1000, 600, 40)), None),
            codex_count(BASE + 200, Some(counters(800, 500, 35)), None),
            codex_count(BASE + 300, Some(counters(1500, 900, 70)), None),
        ],
    );
    assert_eq!(import(&mut store, &prices, "codex", &root).imported, 2);
    let totals = store.dashboard(&all_time()).unwrap().totals;
    assert_eq!(totals.tokens.input, Some(600));
    assert_eq!(totals.tokens.cache_read, Some(900));
    assert_eq!(totals.tokens.output, Some(70));
}

#[test]
fn codex_empty_last_usage_does_not_hide_valid_cumulative_usage() {
    let (dir, mut store, prices) = fixture();
    let root = dir.path().join("home");
    write_jsonl(
        &root.join("sessions/fixture.jsonl"),
        &[
            codex_meta("fixture-empty-snapshot", None, BASE),
            codex_count(BASE + 100, Some(counters(100, 40, 10)), Some(json!({}))),
            codex_count(BASE + 200, Some(json!({})), Some(counters(30, 10, 5))),
        ],
    );
    assert_eq!(import(&mut store, &prices, "codex", &root).imported, 2);
    let totals = store.dashboard(&all_time()).unwrap().totals;
    assert_eq!(totals.tokens.input, Some(80));
    assert_eq!(totals.tokens.cache_read, Some(50));
    assert_eq!(totals.tokens.output, Some(15));
}

#[test]
fn codex_rate_limit_lanes_skip_replayed_snapshots_without_losing_new_requests() {
    let (dir, mut store, prices) = fixture();
    let root = dir.path().join("home");
    let event = |at, lane: &str, input, cached, output| {
        let mut row = codex_count(
            at,
            Some(counters(input, cached, output)),
            Some(counters(100, 40, 10)),
        );
        row["payload"]["rate_limits"] = json!({"limit_id": lane});
        row
    };
    write_jsonl(
        &root.join("sessions/fixture.jsonl"),
        &[
            codex_meta("fixture-lanes", None, BASE),
            event(BASE + 100, "primary", 100, 40, 10),
            event(BASE + 200, "secondary", 200, 80, 20),
            event(BASE + 300, "primary", 100, 40, 10),
            event(BASE + 400, "secondary", 300, 120, 30),
        ],
    );
    assert_eq!(import(&mut store, &prices, "codex", &root).imported, 3);
    let totals = store.dashboard(&all_time()).unwrap().totals;
    assert_eq!(totals.tokens.input, Some(180));
    assert_eq!(totals.tokens.cache_read, Some(120));
    assert_eq!(totals.tokens.output, Some(30));
}

#[test]
fn codex_fork_skips_copied_prefix_and_finds_archived_parent() {
    let (dir, mut store, prices) = fixture();
    let root = dir.path().join("home");
    let first = codex_count(
        BASE + 100,
        Some(counters(1000, 600, 40)),
        Some(counters(1000, 600, 40)),
    );
    let second = codex_count(
        BASE + 200,
        Some(counters(1500, 900, 65)),
        Some(counters(500, 300, 25)),
    );
    write_jsonl(
        &root.join("archived_sessions/parent.jsonl"),
        &[
            codex_meta("fixture-parent", None, BASE),
            first.clone(),
            second.clone(),
        ],
    );
    write_jsonl(
        &root.join("sessions/fork.jsonl"),
        &[
            codex_meta("fixture-fork", Some("fixture-parent"), BASE + 300),
            first,
            second,
            codex_count(
                BASE + 400,
                Some(counters(1800, 1100, 85)),
                Some(counters(300, 200, 20)),
            ),
        ],
    );
    let report = import(&mut store, &prices, "codex", &root);
    assert_eq!((report.files, report.imported, report.errors), (2, 3, 0));
    let totals = store.dashboard(&all_time()).unwrap().totals;
    assert_eq!(totals.tokens.input, Some(700));
    assert_eq!(totals.tokens.cache_read, Some(1100));
    assert_eq!(totals.tokens.output, Some(85));
    let rows = store.logs(&Filter::default()).unwrap().rows;
    assert_eq!(
        rows.iter()
            .filter(|r| r.session_id.as_deref() == Some(safe_id("fixture-fork").as_str()))
            .count(),
        1
    );
    assert_eq!(import(&mut store, &prices, "codex", &root).imported, 0);
}

#[test]
fn claude_message_id_updates_replace_partial_usage_without_fabricating_http_status() {
    let (dir, mut store, prices) = fixture();
    let root = dir.path().join("home");
    let path = root.join("projects/fixture/main.jsonl");
    write_jsonl(
        &path,
        &[
            claude_user("fixture-user", BASE),
            claude_message(
                "fixture-message",
                "fixture-partial",
                Some("fixture-user"),
                BASE + 100,
                2,
                false,
            ),
        ],
    );
    assert_eq!(import(&mut store, &prices, "claude", &root).imported, 1);
    assert!(!store.logs(&Filter::default()).unwrap().rows[0].completed);
    append_jsonl(
        &path,
        &[
            claude_message(
                "fixture-message",
                "fixture-final",
                Some("fixture-partial"),
                BASE + 200,
                9,
                true,
            ),
            claude_message(
                "fixture-message",
                "fixture-late-partial",
                None,
                BASE + 300,
                99,
                false,
            ),
            claude_message(
                "fixture-message",
                "fixture-late-final",
                None,
                BASE + 400,
                8,
                true,
            ),
        ],
    );
    assert_eq!(import(&mut store, &prices, "claude", &root).imported, 1);
    let page = store.logs(&Filter::default()).unwrap();
    assert_eq!(page.total, 1);
    let row = &page.rows[0];
    assert!(row.completed);
    assert_eq!(row.final_attempt().unwrap().tokens.output, Some(9));
    assert_eq!(row.final_attempt().unwrap().duration_ms, 200);
    assert_eq!(row.final_attempt().unwrap().status, None);
    assert_eq!(
        row.final_attempt().unwrap().response_id,
        Some(safe_id("fixture-message"))
    );
    let body = serde_json::to_string(row).unwrap();
    assert!(!body.contains("fixture-content.example.invalid"));
    assert!(!body.contains("fixture-user"));
    let totals = store.dashboard(&all_time()).unwrap().totals;
    assert_eq!(
        (
            totals.requests,
            totals.sessions,
            totals.status_known,
            totals.success
        ),
        (1, 1, 0, 0)
    );
    assert_eq!(totals.tokens.input, Some(100));
    assert_eq!(totals.tokens.cache_read, Some(40));
    assert_eq!(totals.tokens.cache_write, Some(20));
    assert_eq!(import(&mut store, &prices, "claude", &root).imported, 0);
}

#[test]
fn claude_subagents_import_incrementally_with_independent_cursors() {
    let (dir, mut store, prices) = fixture();
    let root = dir.path().join("home");
    let main = root.join("projects/fixture/main.jsonl");
    let child = root.join("projects/fixture/main/subagents/agent-fixture.jsonl");
    write_jsonl(
        &main,
        &[claude_message(
            "fixture-main",
            "fixture-main-row",
            None,
            BASE + 100,
            5,
            true,
        )],
    );
    write_jsonl(
        &child,
        &[claude_message(
            "fixture-child-one",
            "fixture-child-row-one",
            None,
            BASE + 200,
            6,
            true,
        )],
    );
    let report = import(&mut store, &prices, "claude", &root);
    assert_eq!((report.files, report.imported), (2, 2));
    append_jsonl(
        &child,
        &[claude_message(
            "fixture-child-two",
            "fixture-child-row-two",
            None,
            BASE + 300,
            7,
            true,
        )],
    );
    let report = import(&mut store, &prices, "claude", &root);
    assert_eq!((report.files, report.imported), (2, 1));
    let totals = store.dashboard(&all_time()).unwrap().totals;
    assert_eq!(totals.requests, 3);
    assert_eq!(totals.tokens.output, Some(18));
    assert_eq!(import(&mut store, &prices, "claude", &root).imported, 0);
}

#[test]
fn claude_unterminated_line_is_retried_only_after_completion() {
    let (dir, mut store, prices) = fixture();
    let root = dir.path().join("home");
    let path = root.join("projects/fixture/main.jsonl");
    write_jsonl(
        &path,
        &[claude_message(
            "fixture-first",
            "fixture-row-first",
            None,
            BASE + 100,
            1,
            true,
        )],
    );
    let mut second = serde_json::to_vec(&claude_message(
        "fixture-second",
        "fixture-row-second",
        None,
        BASE + 200,
        2,
        true,
    ))
    .unwrap();
    second.push(b'\n');
    let split = second.len() / 2;
    OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(&second[..split])
        .unwrap();
    assert_eq!(import(&mut store, &prices, "claude", &root).imported, 1);
    assert_eq!(import(&mut store, &prices, "claude", &root).imported, 0);
    OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(&second[split..])
        .unwrap();
    assert_eq!(import(&mut store, &prices, "claude", &root).imported, 1);
    assert_eq!(store.logs(&Filter::default()).unwrap().total, 2);
}

#[test]
fn claude_truncation_and_same_length_replacement_reset_only_the_file_cursor() {
    let (dir, mut store, prices) = fixture();
    let root = dir.path().join("home");
    let path = root.join("projects/fixture/main.jsonl");
    write_jsonl(
        &path,
        &[
            claude_message("fixture-old", "fixture-old-row", None, BASE + 100, 1, true),
            json!({"type": "fixture-padding", "padding": "x".repeat(1024)}),
        ],
    );
    assert_eq!(import(&mut store, &prices, "claude", &root).imported, 1);
    write_jsonl(
        &path,
        &[claude_message(
            "fixture-new",
            "fixture-new-row",
            None,
            BASE + 200,
            2,
            true,
        )],
    );
    assert_eq!(import(&mut store, &prices, "claude", &root).imported, 1);
    let size = fs::metadata(&path).unwrap().len();
    write_jsonl(
        &path,
        &[claude_message(
            "fixture-end",
            "fixture-end-row",
            None,
            BASE + 300,
            3,
            true,
        )],
    );
    assert_eq!(fs::metadata(&path).unwrap().len(), size);
    assert_eq!(import(&mut store, &prices, "claude", &root).imported, 1);
    assert_eq!(store.logs(&Filter::default()).unwrap().total, 3);
    assert_eq!(import(&mut store, &prices, "claude", &root).imported, 0);
}

#[test]
fn failed_session_rebuild_preserves_previous_records_and_cursor() {
    let (dir, mut store, prices) = fixture();
    let root = dir.path().join("home");
    let path = root.join("sessions/a.jsonl");
    write_jsonl(
        &path,
        &[
            codex_meta("fixture-old", None, BASE),
            codex_count(BASE + 100, Some(counters(100, 40, 1)), None),
        ],
    );
    import(&mut store, &prices, "codex", &root);
    let key = safe_id(&format!("codex:{}", path.to_string_lossy()));
    let old_cursor = store.cursor(&key).unwrap();
    let old_id = store.logs(&Filter::default()).unwrap().rows[0].id.clone();
    write_jsonl(
        &path,
        &[
            codex_meta("fixture-new", None, BASE),
            codex_count(BASE + 200, Some(counters(200, 80, 2)), None),
        ],
    );
    write_jsonl(
        &root.join("sessions/z.jsonl"),
        &[
            codex_meta("fixture-fork", Some("fixture-missing-parent"), BASE + 300),
            codex_count(BASE + 400, Some(counters(300, 100, 3)), None),
        ],
    );
    assert!(sessions::sync(
        &mut store,
        &prices,
        &Settings::default(),
        "codex",
        &root,
        true
    )
    .is_err());
    assert_eq!(store.cursor(&key).unwrap(), old_cursor);
    let page = store.logs(&Filter::default()).unwrap();
    assert_eq!(page.total, 1);
    assert_eq!(page.rows[0].id, old_id);
}

#[test]
fn exact_response_id_deduplicates_across_sources_in_either_arrival_order() {
    for proxy_first in [false, true] {
        let (_dir, mut store, _prices) = fixture();
        let mut session = record("fixture-session", "codex", "codex", BASE + 100);
        let mut proxy = record("fixture-proxy", "codex", "proxy", BASE + 10_000);
        session.attempts[0].response_id = Some(safe_id("fixture-response"));
        proxy.attempts[0].response_id = session.attempts[0].response_id.clone();
        proxy.attempts[0].tokens.output = Some(15);
        let rows = if proxy_first {
            vec![proxy, session]
        } else {
            vec![session, proxy]
        };
        store.write_batch(&rows, None).unwrap();
        let page = store.logs(&Filter::default()).unwrap();
        assert_eq!(page.total, 1, "proxy_first={proxy_first}");
        assert_eq!(page.rows[0].id, "fixture-proxy");
        assert_eq!(
            store
                .detail("fixture-session")
                .unwrap()
                .final_attempt()
                .unwrap()
                .status,
            None
        );
        let totals = store.dashboard(&all_time()).unwrap().totals;
        assert_eq!(
            (totals.requests, totals.status_known, totals.success),
            (1, 1, 1)
        );
        assert_eq!(totals.tokens.output, Some(15));
    }
}

#[test]
fn late_session_usage_completes_gateway_details_and_rebuild_restores_gateway_snapshot() {
    let (_dir, mut store, _prices) = fixture();
    let response_id = safe_id("fixture-late-usage-response");
    let mut gateway = record("fixture-gateway-late-usage", "codex", "proxy", BASE + 100);
    gateway.attempts[0].response_id = Some(response_id.clone());
    gateway.attempts[0].tokens = Tokens::default();
    gateway.attempts[0].status = Some(201);
    gateway.attempts[0].duration_ms = 425;
    gateway.attempts[0].price = Some(price_snapshot("0.1234"));
    store
        .write_batch(std::slice::from_ref(&gateway), None)
        .unwrap();

    let original = store.detail(&gateway.id).unwrap();
    assert_eq!(
        original.final_attempt().unwrap().response_id,
        Some(response_id.clone())
    );
    assert_eq!(original.final_attempt().unwrap().tokens.total(), None);
    assert_eq!(original.final_attempt().unwrap().status, Some(201));
    assert_eq!(original.final_attempt().unwrap().duration_ms, 425);

    let mut session = record("fixture-session-late-usage", "codex", "codex", BASE + 110);
    session.attempts[0].response_id = Some(response_id);
    session.attempts[0].tokens = Tokens {
        input: Some(100),
        output: Some(7),
        cache_read: Some(20),
        ..Tokens::default()
    };
    session.attempts[0].price = Some(price_snapshot("9.8765"));
    store
        .write_batch(std::slice::from_ref(&session), None)
        .unwrap();

    let page = store.logs(&Filter::default()).unwrap();
    assert_eq!(page.total, 1);
    assert_eq!(page.rows[0].id, gateway.id);
    let listed_attempt = page.rows[0].final_attempt().unwrap();
    assert_eq!(listed_attempt.status, Some(201));
    assert_eq!(listed_attempt.duration_ms, 425);
    assert_eq!(listed_attempt.tokens.input, Some(100));
    assert_eq!(listed_attempt.tokens.output, Some(7));
    let completed = store.detail(&gateway.id).unwrap();
    let attempt = completed.final_attempt().unwrap();
    assert_eq!(attempt.status, Some(201));
    assert_eq!(attempt.duration_ms, 425);
    assert_eq!(attempt.tokens.input, Some(100));
    assert_eq!(attempt.tokens.output, Some(7));
    assert_eq!(attempt.tokens.cache_read, Some(20));
    assert_eq!(attempt.price.as_ref().unwrap().cost, "0.1234");

    store.rebuild("codex", &[], &[]).unwrap();
    assert!(store.detail(&session.id).is_err());
    let restored = store.detail(&gateway.id).unwrap();
    assert_eq!(store.logs(&Filter::default()).unwrap().total, 1);
    assert_eq!(restored.final_attempt().unwrap().status, Some(201));
    assert_eq!(restored.final_attempt().unwrap().duration_ms, 425);
    assert_eq!(restored.final_attempt().unwrap().tokens.total(), None);
    assert_eq!(
        restored
            .final_attempt()
            .unwrap()
            .price
            .as_ref()
            .unwrap()
            .cost,
        "0.1234"
    );
}

#[test]
fn session_usage_does_not_replace_reported_gateway_zero_tokens() {
    let (_dir, mut store, _prices) = fixture();
    let response_id = safe_id("fixture-zero-usage-response");
    let mut gateway = record("fixture-gateway-zero-usage", "codex", "proxy", BASE + 100);
    gateway.attempts[0].response_id = Some(response_id.clone());
    gateway.attempts[0].tokens = Tokens {
        input: Some(0),
        output: Some(0),
        cache_read: Some(0),
        ..Tokens::default()
    };
    let mut session = record(
        "fixture-session-nonzero-usage",
        "codex",
        "codex",
        BASE + 110,
    );
    session.attempts[0].response_id = Some(response_id);
    session.attempts[0].tokens = Tokens {
        input: Some(100),
        output: Some(7),
        cache_read: Some(20),
        ..Tokens::default()
    };

    store
        .write_batch(&[gateway.clone(), session], None)
        .unwrap();

    assert_eq!(store.logs(&Filter::default()).unwrap().total, 1);
    let merged = store.detail(&gateway.id).unwrap();
    assert_eq!(merged.final_attempt().unwrap().tokens.input, Some(0));
    assert_eq!(merged.final_attempt().unwrap().tokens.output, Some(0));
    assert_eq!(merged.final_attempt().unwrap().tokens.cache_read, Some(0));
}

#[test]
fn two_gateway_rows_without_usage_and_their_session_match_count_once() {
    let (_dir, mut store, _prices) = fixture();
    let response_id = safe_id("fixture-no-usage-response");
    let mut first = record("fixture-gateway-no-usage-a", "codex", "proxy", BASE + 100);
    first.attempts[0].response_id = Some(response_id.clone());
    first.attempts[0].tokens = Tokens::default();
    let mut second = record("fixture-gateway-no-usage-b", "codex", "proxy", BASE + 110);
    second.attempts[0].response_id = Some(response_id.clone());
    second.attempts[0].tokens = Tokens::default();
    let mut session = record("fixture-session-no-usage", "codex", "codex", BASE + 120);
    session.attempts[0].response_id = Some(response_id);
    session.attempts[0].tokens = Tokens::default();

    store.write_batch(&[first, second, session], None).unwrap();

    assert_eq!(store.logs(&Filter::default()).unwrap().total, 1);
    assert_eq!(
        store
            .detail("fixture-gateway-no-usage-a")
            .unwrap()
            .final_attempt()
            .unwrap()
            .tokens
            .total(),
        None
    );
    assert_eq!(store.dashboard(&all_time()).unwrap().totals.requests, 1);
}

#[test]
fn fallback_deduplication_requires_exact_tokens_model_client_and_ten_minute_window() {
    for (case, expected_total) in [
        ("exact", 1),
        ("pricing-only", 1),
        ("output", 2),
        ("cache", 2),
        ("model", 2),
        ("requested-model", 2),
        ("legacy-model", 2),
        ("time", 2),
        ("client", 2),
        ("same-source", 2),
        ("response-id", 2),
        ("unknown-model", 2),
        ("unknown-tokens", 2),
    ] {
        let (_dir, mut store, _prices) = fixture();
        let mut proxy = record("fixture-proxy", "codex", "proxy", BASE + 100);
        let mut session = record(
            "fixture-session",
            "codex",
            "codex",
            BASE + 100 + DEDUP_WINDOW,
        );
        proxy.attempts[0].response_id = Some(safe_id("fixture-proxy-response"));
        match case {
            "output" => session.attempts[0].tokens.output = Some(11),
            "cache" => session.attempts[0].tokens.cache_read = None,
            "model" => session.attempts[0].response_model = Some("gpt-other-fixture".into()),
            "pricing-only" => session.attempts[0].pricing_model = Some("gpt-other-fixture".into()),
            "requested-model" => {
                session.attempts[0].response_model = None;
                proxy.attempts[0].response_model = None;
                session.attempts[0].requested_model = Some("gpt-other-fixture".into());
            }
            "legacy-model" => {
                for attempt in [&mut session.attempts[0], &mut proxy.attempts[0]] {
                    attempt.response_model = None;
                    attempt.requested_model = None;
                }
                session.attempts[0].pricing_model = Some("gpt-other-fixture".into());
            }
            "time" => {
                session.started_at += 1;
                session.attempts[0].started_at += 1;
            }
            "client" => session.client = "claude".into(),
            "same-source" => session.source = "proxy".into(),
            "response-id" => {
                session.attempts[0].response_id = Some(safe_id("fixture-distinct-response"))
            }
            "unknown-model" => {
                for attempt in [&mut session.attempts[0], &mut proxy.attempts[0]] {
                    attempt.response_model = None;
                    attempt.requested_model = None;
                    attempt.pricing_model = None;
                }
            }
            "unknown-tokens" => {
                session.attempts[0].tokens = Tokens::default();
                proxy.attempts[0].tokens = Tokens::default();
            }
            _ => {}
        }
        store.write_batch(&[proxy, session], None).unwrap();
        assert_eq!(
            store.logs(&Filter::default()).unwrap().total,
            expected_total,
            "case={case}"
        );
    }
}

#[test]
fn ambiguous_token_matches_do_not_merge_two_real_requests() {
    let (_dir, mut store, _prices) = fixture();
    let first = record("fixture-first", "codex", "proxy", BASE + 100);
    let second = record("fixture-second", "codex", "proxy", BASE + 200);
    let session = record("fixture-session", "codex", "codex", BASE + 300);
    store.write_batch(&[first, second, session], None).unwrap();
    assert_eq!(store.logs(&Filter::default()).unwrap().total, 3);
}

#[test]
fn repeated_same_source_requests_with_distinct_response_ids_are_all_counted() {
    let (_dir, mut store, _prices) = fixture();
    let mut first = record("fixture-first", "codex", "proxy", BASE + 100);
    let mut second = record("fixture-second", "codex", "proxy", BASE + 200);
    first.attempts[0].response_id = Some(safe_id("fixture-first-response"));
    second.attempts[0].response_id = Some(safe_id("fixture-second-response"));
    store.write_batch(&[first, second], None).unwrap();
    assert_eq!(store.logs(&Filter::default()).unwrap().total, 2);
}

#[test]
fn store_rebuild_rolls_back_after_insert_failure_and_does_not_cross_sources() {
    let (dir, mut store, _prices) = fixture();
    let originals = vec![
        record("fixture-codex", "codex", "codex", BASE + 100),
        record("fixture-claude", "claude", "claude", BASE + 200),
        record(
            "fixture-proxy",
            "codex",
            "proxy",
            BASE + 100 + OUTSIDE_DEDUP_WINDOW,
        ),
    ];
    store
        .write_batch(&originals, Some(("fixture-cursor", "old-cursor")))
        .unwrap();
    let connection = rusqlite::Connection::open(dir.path().join("usage.sqlite")).unwrap();
    connection.execute_batch("CREATE TRIGGER fixture_reject BEFORE INSERT ON records WHEN NEW.id='fixture-reject' BEGIN SELECT RAISE(ABORT,'fixture rollback'); END;").unwrap();
    let replacement = record("fixture-reject", "codex", "codex", BASE + 20_000);
    assert!(store
        .rebuild(
            "codex",
            &[replacement],
            &[("fixture-cursor".into(), "new-cursor".into())]
        )
        .is_err());
    assert_eq!(store.logs(&Filter::default()).unwrap().total, 3);
    assert_eq!(
        store.cursor("fixture-cursor").unwrap().as_deref(),
        Some("old-cursor")
    );
    for original in originals {
        assert!(store.detail(&original.id).is_ok());
    }
    assert!(store.detail("fixture-reject").is_err());
}

#[test]
fn successful_rebuild_replaces_only_its_source_and_is_idempotent() {
    let (_dir, mut store, _prices) = fixture();
    store
        .write_batch(
            &[
                record("fixture-old", "codex", "codex", BASE + 100),
                record("fixture-claude", "claude", "claude", BASE + 200),
                record(
                    "fixture-proxy",
                    "codex",
                    "proxy",
                    BASE + 100 + 2 * OUTSIDE_DEDUP_WINDOW,
                ),
            ],
            None,
        )
        .unwrap();
    let replacement = record("fixture-new", "codex", "codex", BASE + 20_000);
    for _ in 0..2 {
        store
            .rebuild(
                "codex",
                std::slice::from_ref(&replacement),
                &[("fixture-cursor".into(), "fresh".into())],
            )
            .unwrap();
        assert_eq!(store.logs(&Filter::default()).unwrap().total, 3);
        assert!(store.detail("fixture-old").is_err());
        for id in ["fixture-new", "fixture-claude", "fixture-proxy"] {
            assert!(store.detail(id).is_ok());
        }
        assert_eq!(
            store.cursor("fixture-cursor").unwrap().as_deref(),
            Some("fresh")
        );
    }
    assert!(store.rebuild("proxy", &[], &[]).is_err());
    assert!(store
        .rebuild(
            "codex",
            &[record("fixture-wrong", "claude", "claude", BASE)],
            &[]
        )
        .is_err());
    assert_eq!(store.logs(&Filter::default()).unwrap().total, 3);
}

#[test]
fn thirty_day_compaction_keeps_boundary_details_and_does_not_double_count() {
    let (_dir, mut store, _prices) = fixture();
    let at = BASE + 40 * DAY + 43_210;
    let cutoff = local_boundary(at - 30 * DAY, 0, 0);
    store
        .write_batch(
            &[
                record("fixture-old", "codex", "codex", BASE + DAY + 100),
                record("fixture-before", "codex", "proxy", cutoff - 1),
                record("fixture-boundary", "claude", "claude", cutoff),
                record("fixture-recent", "codex", "proxy", at - 1),
            ],
            None,
        )
        .unwrap();
    for _ in 0..2 {
        store.compact(at).unwrap();
        assert_eq!(store.detail_since().unwrap(), cutoff);
        assert_eq!(store.logs(&Filter::default()).unwrap().total, 2);
        assert!(store.detail("fixture-old").is_err());
        assert!(store.detail("fixture-before").is_err());
        assert!(store.detail("fixture-boundary").is_ok());
        let dashboard = store.dashboard(&all_time()).unwrap();
        assert_eq!(dashboard.precision, "day");
        assert_eq!(dashboard.totals.requests, 4);
        assert_eq!(dashboard.totals.tokens.input, Some(400));
        assert_eq!(
            (
                dashboard.totals.status_known,
                dashboard.totals.success,
                dashboard.totals.sessions
            ),
            (2, 2, 2)
        );
    }
}

#[test]
fn rebuilding_compacted_source_replaces_its_daily_history_without_deleting_other_sources() {
    let (_dir, mut store, _prices) = fixture();
    let at = BASE + 40 * DAY;
    let codex = record("fixture-codex", "codex", "codex", BASE + DAY);
    store
        .write_batch(
            &[
                codex.clone(),
                record("fixture-claude", "claude", "claude", BASE + 2 * DAY),
                record("fixture-proxy", "codex", "proxy", BASE + 3 * DAY),
            ],
            None,
        )
        .unwrap();
    store.compact(at).unwrap();
    assert_eq!(store.logs(&Filter::default()).unwrap().total, 0);
    for _ in 0..2 {
        store
            .rebuild("codex", std::slice::from_ref(&codex), &[])
            .unwrap();
        store.compact(at).unwrap();
        let dashboard = store.dashboard(&all_time()).unwrap();
        assert_eq!(dashboard.totals.requests, 3);
        assert_eq!(dashboard.sources.get("codex"), Some(&1));
        assert_eq!(dashboard.sources.get("claude"), Some(&1));
        assert_eq!(dashboard.sources.get("proxy"), Some(&1));
    }
}

#[test]
fn zero_prices_are_known_but_missing_required_rates_remain_unpriced() {
    let tokens = Tokens {
        input: Some(10),
        output: Some(5),
        cache_read: Some(3),
        cache_write: Some(2),
        ..Tokens::default()
    };
    let zero = quote(
        json!({"input_cost_per_token": 0, "output_cost_per_token": "0",
        "cache_read_input_token_cost": 0, "cache_creation_input_token_cost": 0}),
        "1",
    );
    assert_eq!(cost(zero.calculate(&tokens, None)), "0");
    let missing = quote(json!({"input_cost_per_token": "0.01"}), "1");
    assert!(missing.calculate(&tokens, None).is_none());
    assert!(missing
        .calculate(
            &Tokens {
                input: None,
                output: Some(0),
                ..Tokens::default()
            },
            None
        )
        .is_none());
    assert_eq!(
        cost(missing.calculate(
            &Tokens {
                input: Some(10),
                output: Some(0),
                ..Tokens::default()
            },
            None
        )),
        "0.1"
    );
    assert_eq!(
        cost(
            quote(
                json!({"input_cost_per_token": 1, "output_cost_per_token": 2}),
                "0"
            )
            .calculate(
                &Tokens {
                    input: Some(10),
                    output: Some(5),
                    ..Tokens::default()
                },
                None
            )
        ),
        "0"
    );
}

#[test]
fn context_price_thresholds_are_strict_and_include_cached_input() {
    let prices = quote(
        json!({"input_cost_per_token": "0.01", "output_cost_per_token": "0.02",
        "input_cost_per_token_above_100k_tokens": "0.02", "input_cost_per_token_above_200k_tokens": "0.03",
        "input_cost_per_token_above_272k_tokens": "0.05", "cache_read_input_token_cost": "0.001"}),
        "1",
    );
    for (input, expected) in [
        (100_000, "1000"),
        (100_001, "2000.02"),
        (200_000, "4000"),
        (200_001, "6000.03"),
        (272_000, "8160"),
        (272_001, "13600.05"),
    ] {
        assert_eq!(
            cost(prices.calculate(
                &Tokens {
                    input: Some(input),
                    output: Some(0),
                    ..Tokens::default()
                },
                None
            )),
            expected
        );
    }
    assert_eq!(
        cost(prices.calculate(
            &Tokens {
                input: Some(50_000),
                output: Some(0),
                cache_read: Some(160_000),
                ..Tokens::default()
            },
            None
        )),
        "1660"
    );
}

#[test]
fn service_tier_prices_and_multiplier_use_exact_decimal_arithmetic() {
    let prices = quote(
        json!({"input_cost_per_token": "0.01", "output_cost_per_token": "0.02",
        "input_cost_per_token_priority": "0.03", "output_cost_per_token_priority": "0.04",
        "input_cost_per_token_batches": "0.005", "output_cost_per_token_batches": "0.01",
        "input_cost_per_token_flex": 0, "output_cost_per_token_flex": 0}),
        "1.5",
    );
    let tokens = Tokens {
        input: Some(100),
        output: Some(10),
        ..Tokens::default()
    };
    for (tier, expected) in [
        ("default", "1.8"),
        ("standard", "1.8"),
        ("priority", "5.1"),
        ("batch", "0.9"),
        ("flex", "0"),
    ] {
        let snapshot = prices.calculate(&tokens, Some(tier)).unwrap();
        assert_eq!(snapshot.cost, expected);
        assert_eq!(snapshot.multiplier, "1.5");
    }
    assert!(prices
        .calculate(&tokens, Some("unknown-fixture-tier"))
        .is_none());
    let incomplete = quote(
        json!({"input_cost_per_token": 1, "output_cost_per_token": 2,
        "input_cost_per_token_priority": 3}),
        "1",
    );
    assert!(incomplete.calculate(&tokens, Some("priority")).is_none());
}

#[test]
fn cache_durations_and_media_subsets_are_charged_once() {
    let tokens = Tokens {
        input: Some(100),
        output: Some(20),
        cache_read: Some(50),
        cache_write: Some(30),
        cache_write_5m: Some(20),
        cache_write_1h: Some(10),
        image_input: Some(10),
        image_output: Some(4),
        audio_input: Some(20),
        audio_output: Some(6),
    };
    let prices = quote(
        json!({"input_cost_per_token": "0.01", "output_cost_per_token": "0.02",
        "input_cost_per_image_token": "0.03", "output_cost_per_image_token": "0.04",
        "input_cost_per_audio_token": "0.05", "output_cost_per_audio_token": "0.06",
        "cache_read_input_token_cost": "0.001", "cache_creation_input_token_cost": "0.002",
        "cache_creation_input_token_cost_above_1hr": "0.004"}),
        "2",
    );
    assert_eq!(tokens.total(), Some(200));
    assert_eq!(cost(prices.calculate(&tokens, None)), "5.7");
    let mut invalid = tokens;
    invalid.image_input = Some(101);
    assert!(prices.calculate(&invalid, None).is_none());
    assert!(quote(
        json!({"input_cost_per_token": 1, "output_cost_per_token": 1,
        "mode": "image_generation"}),
        "1"
    )
    .calculate(
        &Tokens {
            input: Some(1),
            output: Some(1),
            ..Tokens::default()
        },
        None
    )
    .is_none());
}

#[test]
fn fixed_prices_override_catalog_selection_and_aliases_resolve_to_fixed_models() {
    let (dir, _store, prices) = fixture();
    let view = prices.view();
    let config = Config {
        auto_update: false,
        selected: Some(BTreeSet::new()),
        excluded: BTreeSet::from(["gpt-4o".into()]),
        fixed: BTreeMap::from([(
            "gpt-4o".into(),
            json!({"input_cost_per_token": "0.1", "output_cost_per_token": "0.2"}),
        )]),
        aliases: BTreeMap::from([("fixture-alias".into(), "gpt-4o".into())]),
        provider_mappings: Vec::new(),
    };
    let saved = prices.configure(config, &view.revision).unwrap();
    let tokens = Tokens {
        input: Some(10),
        output: Some(5),
        ..Tokens::default()
    };
    for model in ["gpt-4o", "fixture-alias"] {
        let snapshot = prices
            .quote(Some(model), "1")
            .calculate(&tokens, None)
            .unwrap();
        assert_eq!(snapshot.source, "fixed");
        assert_eq!(snapshot.model, "gpt-4o");
        assert_eq!(snapshot.cost, "2");
    }
    assert!(prices
        .quote(Some("fixture-unknown"), "1")
        .calculate(&tokens, None)
        .is_none());
    assert!(prices.configure(Config::default(), &view.revision).is_err());
    assert_eq!(prices.view().revision, saved.revision);
    let reopened = Pricing::new(dir.path()).unwrap();
    assert_eq!(
        cost(
            reopened
                .quote(Some("fixture-alias"), "1")
                .calculate(&tokens, None)
        ),
        "2"
    );
}

#[test]
fn backfill_preserves_priced_history_and_original_multipliers_in_details_and_daily_totals() {
    let (_dir, mut store, prices) = fixture();
    install_fixed(
        &prices,
        &[(
            "gpt-fixture",
            json!({"input_cost_per_token": "0.01", "output_cost_per_token": "0.02"}),
        )],
    );
    let at = BASE + 40 * DAY;
    let mut priced = record("fixture-priced", "codex", "proxy", at - 1000);
    priced.attempts[0].tokens.cache_read = None;
    priced.attempts[0].cost_multiplier = "2".into();
    priced.attempts[0].price = prices
        .quote(Some("gpt-fixture"), "2")
        .calculate(&priced.attempts[0].tokens, None);
    let historical_snapshot = serde_json::to_value(&priced.attempts[0].price).unwrap();
    assert_eq!(priced.cost().unwrap().to_string(), "2.4");
    let mut pending = record("fixture-pending", "claude", "claude", at - 2000);
    pending.attempts[0].pricing_model = Some("fixture-future-model".into());
    pending.attempts[0].cost_multiplier = "3".into();
    pending.attempts[0].tokens = Tokens {
        input: Some(10),
        output: Some(5),
        ..Tokens::default()
    };
    let mut old_pending = pending.clone();
    old_pending.id = "fixture-old-pending".into();
    old_pending.started_at = BASE + DAY;
    old_pending.attempts[0].started_at = old_pending.started_at;
    old_pending.attempts[0].cost_multiplier = "4".into();
    store
        .write_batch(&[priced, pending, old_pending], None)
        .unwrap();
    store.compact(at).unwrap();
    assert_eq!(store.dashboard(&all_time()).unwrap().totals.unpriced, 2);
    install_fixed(
        &prices,
        &[
            (
                "gpt-fixture",
                json!({"input_cost_per_token": 9, "output_cost_per_token": 9}),
            ),
            (
                "fixture-future-model",
                json!({"input_cost_per_token": "0.2", "output_cost_per_token": "0.4"}),
            ),
        ],
    );
    for multiplier in ["99", "100"] {
        store.backfill(&prices, multiplier).unwrap();
        assert_eq!(
            serde_json::to_value(&store.detail("fixture-priced").unwrap().attempts[0].price)
                .unwrap(),
            historical_snapshot
        );
        let pending = store.detail("fixture-pending").unwrap();
        assert_eq!(pending.cost().unwrap().to_string(), "12");
        assert_eq!(pending.attempts[0].price.as_ref().unwrap().multiplier, "3");
        let totals = store.dashboard(&all_time()).unwrap().totals;
        assert_eq!(totals.requests, 3);
        assert_eq!(totals.unpriced, 0);
        assert_eq!(totals.cost, "30.4");
    }
}

#[test]
fn service_query_reads_only_its_isolated_store() {
    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(dir.path());
    assert!(service.state().error.is_none());
    service
        .query(|store| {
            store.write_batch(
                &[record("fixture-service", "codex", "proxy", BASE + 100)],
                None,
            )
        })
        .unwrap();
    let page = service
        .query(|store| store.logs(&Filter::default()))
        .unwrap();
    assert_eq!(page.total, 1);
    assert_eq!(page.rows[0].id, "fixture-service");
    assert!(PathBuf::from(dir.path())
        .join("usage/usage.sqlite")
        .is_file());
}

#[test]
fn archived_proxy_receipts_survive_reimports_and_session_rebuilds() {
    for exact_response in [false, true] {
        let (dir, mut store, prices) = fixture();
        let root = dir.path().join("home");
        let at = BASE + DAY;
        let compact_at = BASE + 40 * DAY;
        let message = claude_message(
            "fixture-receipt-message",
            "fixture-receipt-row",
            None,
            at,
            9,
            true,
        );
        install_fixed(
            &prices,
            &[(
                "claude-fixture",
                json!({
                    "input_cost_per_token": "0.01", "output_cost_per_token": "0.02",
                    "cache_read_input_token_cost": "0.001", "cache_creation_input_token_cost": "0.002"
                }),
            )],
        );
        let mut proxy = record("fixture-archived-proxy", "claude", "proxy", at - 1000);
        let attempt = &mut proxy.attempts[0];
        attempt.duration_ms = 1000;
        attempt.requested_model = Some("claude-fixture".into());
        attempt.response_model = Some("claude-fixture".into());
        attempt.pricing_model = Some("claude-fixture".into());
        attempt.response_id = exact_response.then(|| safe_id("fixture-receipt-message"));
        attempt.tokens = parse_tokens(&message["message"]["usage"], true);
        attempt.price = prices
            .quote(Some("claude-fixture"), "1")
            .calculate(&attempt.tokens, None);
        store
            .write_batch(std::slice::from_ref(&proxy), None)
            .unwrap();
        store.compact(compact_at).unwrap();
        drop(store);
        let mut store = Store::open(dir.path()).unwrap();
        assert!(store.detail(&proxy.id).is_err());

        let path = root.join("projects/fixture/main.jsonl");
        write_jsonl(&path, std::slice::from_ref(&message));
        assert_eq!(import(&mut store, &prices, "claude", &root).imported, 1);
        store.compact(compact_at).unwrap();
        write_jsonl(&root.join("projects/fixture/reimport.jsonl"), &[message]);
        assert_eq!(import(&mut store, &prices, "claude", &root).imported, 1);
        assert_eq!(import(&mut store, &prices, "claude", &root).imported, 0);

        for _ in 0..2 {
            store
                .write_batch(std::slice::from_ref(&proxy), None)
                .unwrap();
            let report = sessions::sync(
                &mut store,
                &prices,
                &Settings::default(),
                "claude",
                &root,
                true,
            )
            .unwrap();
            assert_eq!((report.files, report.errors), (2, 0));
            store.compact(compact_at).unwrap();
            assert_eq!(store.logs(&Filter::default()).unwrap().total, 0);
            let dashboard = store.dashboard(&all_time()).unwrap();
            assert_eq!(
                dashboard.totals.requests, 1,
                "exact_response={exact_response}"
            );
            assert_eq!(dashboard.totals.tokens, proxy.tokens());
            assert_eq!(dashboard.totals.cost, "1.26");
            assert_eq!(dashboard.totals.unpriced, 0);
            assert_eq!(
                (
                    dashboard.totals.status_known,
                    dashboard.totals.success,
                    dashboard.totals.sessions
                ),
                (1, 1, 0)
            );
            assert_eq!(dashboard.sources, BTreeMap::from([("proxy".into(), 1)]));
        }
    }
}

#[test]
fn archived_codex_receipt_prevents_recount_when_source_moves_to_archive() {
    let (dir, mut store, prices) = fixture();
    let root = dir.path().join("home");
    let path = root.join("sessions/fixture.jsonl");
    write_jsonl(
        &path,
        &[
            codex_meta("fixture-archived-codex", None, BASE + DAY),
            codex_count(BASE + DAY + 100, Some(counters(1000, 600, 40)), None),
        ],
    );
    assert_eq!(import(&mut store, &prices, "codex", &root).imported, 1);
    let compact_at = BASE + 40 * DAY;
    store.compact(compact_at).unwrap();
    drop(store);
    let mut store = Store::open(dir.path()).unwrap();
    let archived = root.join("archived_sessions/fixture.jsonl");
    fs::create_dir_all(archived.parent().unwrap()).unwrap();
    fs::rename(path, archived).unwrap();
    assert_eq!(import(&mut store, &prices, "codex", &root).imported, 1);
    assert_eq!(store.logs(&Filter::default()).unwrap().total, 0);
    for rebuild in [false, true, true] {
        sessions::sync(
            &mut store,
            &prices,
            &Settings::default(),
            "codex",
            &root,
            rebuild,
        )
        .unwrap();
        store.compact(compact_at).unwrap();
        let totals = store.dashboard(&all_time()).unwrap().totals;
        assert_eq!(
            (totals.requests, totals.sessions, totals.status_known),
            (1, 1, 0)
        );
        assert_eq!(totals.tokens.input, Some(400));
        assert_eq!(totals.tokens.cache_read, Some(600));
        assert_eq!(totals.tokens.output, Some(40));
    }
}

#[test]
fn claude_message_growth_keeps_persisted_price_basis_and_multiplier() {
    let (dir, mut store, prices) = fixture();
    let root = dir.path().join("home");
    let path = root.join("projects/fixture/main.jsonl");
    install_fixed(
        &prices,
        &[(
            "claude-fixture",
            json!({
                "input_cost_per_token": "0.01", "output_cost_per_token": "0.02",
                "cache_read_input_token_cost": "0.001", "cache_creation_input_token_cost": "0.002"
            }),
        )],
    );
    write_jsonl(
        &path,
        &[
            claude_user("fixture-price-user", BASE),
            claude_message(
                "fixture-price-message",
                "fixture-price-partial",
                Some("fixture-price-user"),
                BASE + 100,
                2,
                false,
            ),
        ],
    );
    let initial_settings = Settings {
        multiplier: "2".into(),
        ..Settings::default()
    };
    sessions::sync(
        &mut store,
        &prices,
        &initial_settings,
        "claude",
        &root,
        false,
    )
    .unwrap();
    let original = store.logs(&Filter::default()).unwrap().rows.remove(0);
    let snapshot = original.attempts[0].price.clone().unwrap();
    assert_eq!(snapshot.cost, "2.24");
    assert!(snapshot.basis.is_some());
    drop(store);
    let mut store = Store::open(dir.path()).unwrap();
    install_fixed(
        &prices,
        &[(
            "claude-fixture",
            json!({
                "input_cost_per_token": "9", "output_cost_per_token": "9",
                "cache_read_input_token_cost": "9", "cache_creation_input_token_cost": "9"
            }),
        )],
    );
    let updated_settings = Settings {
        multiplier: "3".into(),
        ..Settings::default()
    };
    for (output, expected) in [(9, "2.52"), (12, "2.64")] {
        append_jsonl(
            &path,
            &[claude_message(
                "fixture-price-message",
                &format!("fixture-price-final-{output}"),
                None,
                BASE + output as i64 * 100,
                output,
                true,
            )],
        );
        let report = sessions::sync(
            &mut store,
            &prices,
            &updated_settings,
            "claude",
            &root,
            false,
        )
        .unwrap();
        assert_eq!(report.imported, 1);
        let row = store.detail(&original.id).unwrap();
        let price = row.attempts[0].price.as_ref().unwrap();
        assert!(row.completed);
        assert_eq!(row.attempts[0].tokens.output, Some(output));
        assert_eq!(row.attempts[0].cost_multiplier, "2");
        assert_eq!(price.cost, expected);
        assert_eq!(price.multiplier, snapshot.multiplier);
        assert_eq!(price.version, snapshot.version);
        assert_eq!(price.rates, snapshot.rates);
        assert_eq!(price.basis, snapshot.basis);
        assert_ne!(
            cost(
                prices
                    .quote(Some("claude-fixture"), "3")
                    .calculate(&row.tokens(), None)
            ),
            expected
        );
        let totals = store.dashboard(&all_time()).unwrap().totals;
        assert_eq!((totals.requests, totals.unpriced), (1, 0));
        assert_eq!(totals.cost, expected);
    }
}

#[test]
fn partially_unpriced_attempts_keep_known_cost_in_details_and_daily_totals() {
    let (_dir, mut store, prices) = fixture();
    let mut row = record("fixture-partial-price", "codex", "proxy", BASE + DAY);
    row.attempts[0].tokens.cache_read = None;
    row.attempts[0].price = quote(
        json!({"input_cost_per_token": "0.01", "output_cost_per_token": "0.02"}),
        "1",
    )
    .calculate(&row.attempts[0].tokens, None);
    for model in ["fixture-pending-one", "fixture-pending-two"] {
        row.attempts.push(Attempt {
            id: format!("fixture-attempt-{model}"),
            pricing_model: Some(model.into()),
            tokens: Tokens {
                input: Some(10),
                output: Some(5),
                ..Tokens::default()
            },
            status: Some(200),
            outcome: "success".into(),
            ..Attempt::default()
        });
    }
    assert!(row.cost().is_none());
    store.write_batch(&[row], None).unwrap();
    let totals = store.dashboard(&all_time()).unwrap().totals;
    assert_eq!((totals.requests, totals.unpriced), (1, 1));
    assert_eq!(totals.cost, "1.2");
    assert_eq!(totals.tokens.input, Some(120));
    assert_eq!(totals.tokens.output, Some(20));
    let compact_at = BASE + 40 * DAY;
    store.compact(compact_at).unwrap();
    assert_eq!(store.dashboard(&all_time()).unwrap().totals.cost, "1.2");
    install_fixed(
        &prices,
        &[(
            "fixture-pending-one",
            json!({"input_cost_per_token": "0.2", "output_cost_per_token": "0.4"}),
        )],
    );
    store.backfill(&prices, "1").unwrap();
    let totals = store.dashboard(&all_time()).unwrap().totals;
    assert_eq!((totals.requests, totals.unpriced), (1, 1));
    assert_eq!(totals.cost, "5.2");
    install_fixed(
        &prices,
        &[(
            "fixture-pending-two",
            json!({"input_cost_per_token": "0.3", "output_cost_per_token": "0.6"}),
        )],
    );
    for _ in 0..2 {
        store.backfill(&prices, "1").unwrap();
        let totals = store.dashboard(&all_time()).unwrap().totals;
        assert_eq!((totals.requests, totals.unpriced), (1, 0));
        assert_eq!(totals.cost, "11.2");
    }
}

#[test]
fn oversized_session_payload_is_ignored_and_later_usage_is_imported() {
    let (dir, mut store, prices) = fixture();
    let root = dir.path().join("home");
    let path = root.join("projects/fixture/main.jsonl");
    write_jsonl(
        &path,
        &[
            claude_message(
                "fixture-before-media",
                "fixture-before-row",
                None,
                BASE + 100,
                1,
                true,
            ),
            json!({"type": "tool_result", "timestamp": BASE + 200, "content": "x".repeat(2 * 1024 * 1024)}),
            claude_message(
                "fixture-after-media",
                "fixture-after-row",
                None,
                BASE + 300,
                2,
                true,
            ),
        ],
    );
    let original = fs::read(&path).unwrap();
    for rebuild in [false, true] {
        let report = sessions::sync(
            &mut store,
            &prices,
            &Settings::default(),
            "claude",
            &root,
            rebuild,
        )
        .unwrap();
        assert_eq!(
            (report.files, report.imported, report.skipped, report.errors),
            (1, 2, 0, 0)
        );
        let totals = store.dashboard(&all_time()).unwrap().totals;
        assert_eq!(totals.requests, 2);
        assert_eq!(totals.tokens.output, Some(3));
        assert_eq!(fs::read(&path).unwrap(), original);
    }
}

#[test]
fn maintenance_compacts_history_when_session_auto_sync_is_disabled() {
    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(dir.path());
    service
        .configure(Settings {
            auto_sync: false,
            ..Settings::default()
        })
        .unwrap();
    let at = now();
    let mut rows = [
        record("fixture-old-proxy", "codex", "proxy", at - 40 * DAY),
        record("fixture-old-session", "codex", "codex", at - 39 * DAY),
        record("fixture-recent-proxy", "codex", "proxy", at - DAY),
        record("fixture-recent-session", "claude", "claude", at - 2 * DAY),
    ];
    let price = quote(
        json!({
            "input_cost_per_token": "0.01", "output_cost_per_token": "0.02",
            "cache_read_input_token_cost": "0.001"
        }),
        "1",
    );
    for row in &mut rows {
        row.attempts[0].price = price.calculate(&row.attempts[0].tokens, None);
    }
    service
        .query(|store| store.write_batch(&rows, None))
        .unwrap();
    let filter = Filter {
        start: Some(at - 41 * DAY),
        end: Some(at + DAY),
        ..Filter::default()
    };
    let before = service.query(|store| store.dashboard(&filter)).unwrap();
    assert_eq!(
        (
            before.totals.requests,
            before.totals.sessions,
            before.totals.status_known,
            before.totals.success
        ),
        (4, 2, 2, 2)
    );
    assert_eq!(before.totals.cost, "4.88");
    assert_eq!(before.totals.unpriced, 0);
    assert_eq!(service.query(|store| store.logs(&filter)).unwrap().total, 4);

    for _ in 0..2 {
        service.maintain().unwrap();
        let page = service.query(|store| store.logs(&filter)).unwrap();
        assert_eq!(page.total, 2);
        for row in &rows[..2] {
            assert!(service.query(|store| store.detail(&row.id)).is_err());
            assert!(row.started_at < page.detail_since);
        }
        for row in &rows[2..] {
            let kept = service.query(|store| store.detail(&row.id)).unwrap();
            assert_eq!(
                serde_json::to_value(&kept).unwrap(),
                serde_json::to_value(row).unwrap()
            );
            assert!(row.started_at >= page.detail_since);
        }
        let after = service.query(|store| store.dashboard(&filter)).unwrap();
        assert_eq!(
            serde_json::to_value(&after.totals).unwrap(),
            serde_json::to_value(&before.totals).unwrap()
        );
        assert_eq!(
            after.sources,
            BTreeMap::from([
                ("proxy".into(), 2),
                ("codex".into(), 1),
                ("claude".into(), 1),
            ])
        );
        assert_eq!(after.precision, "day");
        let state = service.state();
        assert!(!state.settings.auto_sync);
        assert!(state.settings.recording);
        assert!(!state.syncing);
        assert!(state.reports.is_empty());
        assert!(state.error.is_none());
    }
}
