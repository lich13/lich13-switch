//! Export owns a private disk spool. Source replay, CSV durability and cleanup
//! are ordered so a cancelled or incomplete export cannot remove accounting data.
use super::{
    bodies,
    model::*,
    pricing::Pricing,
    store::{db, Store},
    Service,
};
use crate::storage::{self, Result};
use chrono::{Local, TimeZone};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::{
    io::{BufWriter, Write},
    path::Path,
};

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Request {
    pub clients: Vec<String>,
    pub start: i64,
    pub end: i64,
    pub timezone: String,
}
impl Request {
    pub fn validate(&self) -> Result<()> {
        if self.clients.is_empty()
            || self.clients.len() > 2
            || self
                .clients
                .iter()
                .any(|s| !matches!(s.as_str(), "claude" | "antigravity"))
            || self
                .clients
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                != self.clients.len()
            || self.start < 0
            || self.start > self.end
            || self.start % 1000 != 0
            || self.end % 1000 != 0
            || self.end.checked_add(999).is_none()
            || Local
                .timestamp_millis_opt(self.end + 999)
                .single()
                .is_none()
            || self.timezone.is_empty()
            || self.timezone.len() > 128
            || self.timezone.chars().any(char::is_control)
        {
            return Err(failure("请选择客户端及有效的秒级起止时间"));
        }
        Ok(())
    }
    pub(super) fn contains(&self, r: &Record) -> bool {
        self.clients.contains(&r.client)
            && r.event_time() >= self.start
            && r.event_time() <= self.end + 999
    }
}
#[derive(Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Exported {
    pub requests: u64,
    pub attempts: u64,
    pub cleaned: u64,
    pub retained: u64,
    pub cleanup_error: Option<String>,
}
struct Spool {
    // Rust drops fields in declaration order; close SQLite before removing its
    // directory so Windows does not leave an open-file spool behind.
    connection: Connection,
    _directory: storage::PrivateTempDir,
}
impl Spool {
    fn new(parent: &Path) -> Result<Self> {
        let directory = storage::private_tempdir(parent, "bill-")?;
        let path = directory.path().join("spool.sqlite3");
        let connection = db(Connection::open(&path))?;
        storage::protect(&path, false)?;
        db(connection.execute_batch(
            "PRAGMA journal_mode=DELETE; PRAGMA cache_size=-2048; CREATE TABLE bill(id TEXT PRIMARY KEY,body TEXT NOT NULL,effective INTEGER NOT NULL,original_digest TEXT);",
        ))?;
        Ok(Self {
            connection,
            _directory: directory,
        })
    }
}
pub(super) fn finished(r: &Record) -> bool {
    r.finished_at.is_some()
        && (r.source != "claude" || r.completed)
        && !r.attempts.is_empty()
        && r.final_attempt()
            .is_some_and(|a| !matches!(a.outcome.as_str(), "in_progress" | "pending"))
}
pub(super) fn encoded(r: &Record) -> Result<String> {
    serde_json::to_string(r).map_err(|_| failure("账单记录无法序列化"))
}
fn append(spool: &Connection, r: &Record, effective: i64, digest: Option<&str>) -> Result<()> {
    db(spool.execute("INSERT INTO bill VALUES(?1,?2,?3,?4) ON CONFLICT(id) DO UPDATE SET body=excluded.body,effective=excluded.effective,original_digest=excluded.original_digest",params![r.id,encoded(r)?,effective,digest]))?;
    Ok(())
}
fn snapshot(
    store: &Store,
    spool: &Spool,
    request: &Request,
    pricing: &Pricing,
    settings: &Settings,
) -> Result<u64> {
    let tx = db(spool.connection.unchecked_transaction())?;
    let native = super::storage_v12::active(&store.connection);
    let mut pending = 0;
    for client in &request.clients {
        let mut after = String::new();
        loop {
            let rows = if native {
                let mut statement = db(store.connection.prepare_cached(
                    "SELECT rid,effective FROM v12_rows NOT INDEXED WHERE rid>cast(?4 AS INTEGER) AND client=usage_label(?1) AND time BETWEEN ?2 AND ?3 ORDER BY rid LIMIT 128",
                ))?;
                let selected = db(statement.query_map(
                    params![client, request.start, request.end + 999, after],
                    |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
                ))?;
                db(selected.collect::<rusqlite::Result<Vec<_>>>())?
                    .into_iter()
                    .map(|(rid, effective)| {
                        Ok((
                            rid.to_string(),
                            super::storage_v12::load_record(&store.connection, rid)?,
                            effective,
                        ))
                    })
                    .collect::<Result<Vec<_>>>()?
            } else {
                let mut statement = db(store.connection.prepare_cached(
                    "SELECT id,body,effective,duplicate_of FROM records WHERE client=?1 AND time BETWEEN ?2 AND ?3 AND id>?4 ORDER BY id LIMIT 128",
                ))?;
                let selected = db(statement.query_map(
                    params![client, request.start, request.end + 999, after],
                    |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, i64>(2)?,
                            r.get::<_, Option<String>>(3)?,
                        ))
                    },
                ))?;
                db(selected.collect::<rusqlite::Result<Vec<_>>>())?
                    .into_iter()
                    .map(|(id, body, effective, duplicate)| {
                        let mut r: Record = bodies::decode(&store.connection, &body)?;
                        r.duplicate_of = duplicate;
                        Ok((id, r, effective))
                    })
                    .collect::<Result<Vec<_>>>()?
            };
            if rows.is_empty() {
                break;
            }
            for (id, mut r, effective) in rows {
                after = id;
                if !finished(&r) {
                    pending += 1;
                    continue;
                }
                let digest = storage::digest(encoded(&r)?.as_bytes());
                estimate_missing(&mut r, pricing, settings);
                append(&spool.connection, &r, effective, Some(&digest))?;
            }
        }
    }
    db(tx.commit())?;
    Ok(pending)
}
fn estimate_missing(r: &mut Record, pricing: &Pricing, settings: &Settings) {
    for a in &mut r.attempts {
        if a.price.is_none() {
            a.cost_multiplier = settings.multiplier.clone();
            a.pricing_model = a
                .response_model
                .clone()
                .or_else(|| a.pricing_model.clone())
                .or_else(|| a.requested_model.clone());
            a.price = pricing
                .quote_at(a.pricing_model.as_deref(), &a.cost_multiplier, a.started_at)
                .calculate(&a.tokens, a.price_tier());
            a.pricing_basis = Some("estimated_export_snapshot".into());
        }
    }
}
fn csv_row(w: &mut impl Write, fields: &[String]) -> Result<()> {
    for (i, field) in fields.iter().enumerate() {
        if i > 0 {
            w.write_all(b",").map_err(storage::io_error)?;
        }
        w.write_all(b"\"").map_err(storage::io_error)?;
        if field.starts_with(['=', '+', '-', '@', '\t', '\r']) {
            w.write_all(b"'").map_err(storage::io_error)?;
        }
        w.write_all(field.replace('"', "\"\"").as_bytes())
            .map_err(storage::io_error)?;
        w.write_all(b"\"").map_err(storage::io_error)?;
    }
    w.write_all(b"\r\n").map_err(storage::io_error)
}
fn local_time(at: i64) -> String {
    Local
        .timestamp_millis_opt(at)
        .single()
        .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Millis, false))
        .unwrap_or_default()
}
fn opt<T: ToString>(v: Option<T>) -> String {
    v.map(|v| v.to_string()).unwrap_or_default()
}
fn write_csv(spool: &Spool, request: &Request, w: &mut impl Write) -> Result<Exported> {
    let columns = [
        "request_id",
        "attempt_id",
        "attempt_index",
        "attempt_count",
        "repeat_count",
        "request_started_at",
        "request_ended_at",
        "attempt_started_at",
        "attempt_duration_ms",
        "timezone",
        "client",
        "source",
        "session_id",
        "duplicate_of",
        "included_in_totals",
        "request_completed",
        "status",
        "outcome",
        "provider",
        "requested_model",
        "upstream_model",
        "response_model",
        "pricing_model",
        "response_id",
        "upstream_request_id",
        "service_tier",
        "requested_speed",
        "response_speed",
        "input_tokens",
        "inclusive_input_tokens",
        "output_tokens",
        "reasoning_tokens",
        "cache_read_tokens",
        "cache_write_tokens",
        "cache_write_5m_tokens",
        "cache_write_1h_tokens",
        "image_input_tokens",
        "image_output_tokens",
        "audio_input_tokens",
        "audio_output_tokens",
        "unit_prices_json",
        "multiplier",
        "cost",
        "price_version",
        "price_source",
        "pricing_basis",
        "price_basis_json",
        "usage_status",
        "field_availability_json",
        "parse_reasons_json",
        "export_start",
        "export_end",
        "cleanup_through",
        "model_display_name",
        "api_provider",
        "first_token_ms",
        "generation_ms",
        "output_tokens_per_second",
        "generation_tokens_per_second",
        "error_code",
        "reasoning_effort",
        "image_cache_read_tokens",
    ];
    csv_row(w, &columns.map(str::to_owned))?;
    let mut result = Exported::default();
    let mut statement = db(spool
        .connection
        .prepare("SELECT body,effective FROM bill ORDER BY id"))?;
    let rows = db(statement.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))))?;
    for row in rows {
        let (body, effective) = db(row)?;
        let r: Record = serde_json::from_str(&body).map_err(|_| failure("账单暂存记录无效"))?;
        if !request.contains(&r) {
            continue;
        }
        if r.attempts.iter().any(|a| a.repeat_count > 1) {
            return Err(failure(
                "所选时间段含旧版合并的重试明细，无法恢复完整尝试；未导出、未清理",
            ));
        }
        result.requests += 1;
        for (index, a) in r.attempts.iter().enumerate() {
            let p = a.price.as_ref();
            let t = &a.tokens;
            let timing_known = a.availability.get("attempt_start").map_or_else(
                || {
                    !a.parse_reasons
                        .iter()
                        .any(|r| r == "attempt_time_unreported")
                },
                |v| v == "reported",
            );
            let mut values = vec![
                r.id.clone(),
                a.id.clone(),
                (index + 1).to_string(),
                r.attempts.len().to_string(),
                a.repeat_count.to_string(),
                local_time(r.started_at),
                local_time(r.event_time()),
                if timing_known {
                    local_time(a.started_at)
                } else {
                    String::new()
                },
                opt(a.measured_duration_ms()),
                request.timezone.clone(),
                r.client.clone(),
                r.source.clone(),
                opt(r.session_id.as_ref()),
                opt(r.duplicate_of.as_ref()),
                (effective == 1).to_string(),
                r.completed.to_string(),
                opt(a.status),
                a.outcome.clone(),
                opt(a.provider.as_ref()),
                opt(a.requested_model.as_ref()),
                opt(a.upstream_model.as_ref()),
                opt(a.response_model.as_ref()),
                opt(a.pricing_model.as_ref()),
                opt(a.response_id.as_ref()),
                opt(a.upstream_request_hash.as_ref()),
                opt(a.service_tier.as_ref()),
                opt(a.requested_speed.as_ref()),
                opt(a.response_speed.as_ref()),
            ];
            values.extend(
                [
                    t.input,
                    t.inclusive_input,
                    t.output,
                    t.reasoning,
                    t.cache_read,
                    t.cache_write,
                    t.cache_write_5m,
                    t.cache_write_1h,
                    t.image_input,
                    t.image_output,
                    t.audio_input,
                    t.audio_output,
                ]
                .map(opt),
            );
            values.extend([
                p.map(|p| serde_json::to_string(&p.rates).unwrap_or_default())
                    .unwrap_or_default(),
                a.cost_multiplier.clone(),
                p.map(|p| p.cost.clone()).unwrap_or_default(),
                p.map(|p| p.version.clone()).unwrap_or_default(),
                p.map(|p| p.source.clone()).unwrap_or_default(),
                a.pricing_basis.clone().unwrap_or_else(|| {
                    if p.is_some() {
                        "stored_snapshot".into()
                    } else {
                        "unknown".into()
                    }
                }),
                p.and_then(|p| p.basis.as_ref())
                    .map(ToString::to_string)
                    .unwrap_or_default(),
                a.usage_status.clone(),
                serde_json::to_string(&a.availability).unwrap_or_default(),
                serde_json::to_string(&a.parse_reasons).unwrap_or_default(),
                local_time(request.start),
                local_time(request.end),
                local_time(request.end),
            ]);
            let speed = |duration: Option<u64>| -> String {
                if a.output_reported == Some(false)
                    || t.image_output.unwrap_or(0) > 0
                    || a.price
                        .as_ref()
                        .and_then(|p| p.basis.as_ref())
                        .is_some_and(|b| b["unit"] == "image")
                {
                    return String::new();
                }
                duration
                    .filter(|n| *n > 0)
                    .zip(t.output)
                    .filter(|(_, n)| *n > 0)
                    .map(|(ms, output)| format!("{:.6}", output as f64 * 1000.0 / ms as f64))
                    .unwrap_or_default()
            };
            values.extend([
                opt(a.model_display_name.as_ref()),
                opt(a.api_provider.as_ref()),
                opt(a.first_token_ms),
                opt(a.generation_ms),
                speed(a.measured_duration_ms()),
                speed(a.generation_ms.filter(|ms| *ms > 50)),
                opt(a.error_code.as_ref()),
                opt(a.reasoning_effort.as_ref()),
                opt(t.image_cache_read),
            ]);
            csv_row(w, &values)?;
            result.attempts += 1;
        }
    }
    Ok(result)
}
fn save_csv(spool: &Spool, request: &Request, path: &Path) -> Result<Exported> {
    let parent = path
        .parent()
        .filter(|p| p.is_dir())
        .ok_or_else(|| failure("账单保存目录不存在"))?;
    let mut file = storage::private_tempfile(parent)?;
    let result = {
        let mut writer = BufWriter::new(file.as_file_mut());
        writer
            .write_all(&[0xef, 0xbb, 0xbf])
            .map_err(storage::io_error)?;
        let result = write_csv(spool, request, &mut writer)?;
        writer.flush().map_err(storage::io_error)?;
        result
    };
    file.as_file().sync_all().map_err(storage::io_error)?;
    file.persist(path)
        .map_err(|_| failure("账单保存失败，未清理任何明细"))?;
    #[cfg(unix)]
    std::fs::File::open(parent)
        .and_then(|f| f.sync_all())
        .map_err(|_| failure("账单目录写入未确认，未清理任何明细"))?;
    Ok(result)
}
fn cleanup(
    service: &Service,
    spool: &Spool,
    request: &Request,
    result: &mut Exported,
) -> Result<()> {
    let mut after = String::new();
    loop {
        let mut stmt = db(spool.connection.prepare(
            "SELECT id,original_digest FROM bill WHERE original_digest IS NOT NULL AND id>?1 ORDER BY id LIMIT 128",
        ))?;
        let rows = db(stmt.query_map([&after], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        }))?;
        let rows = db(rows.collect::<rusqlite::Result<Vec<_>>>())?;
        if rows.is_empty() {
            break;
        }
        after = rows.last().unwrap().0.clone();
        let (cleaned, retained) =
            service.query(|store| store.clean_exported_bill_batch(&rows, request))?;
        result.cleaned += cleaned;
        result.retained += retained;
    }
    service.query(|store| store.clean_exported_bill_daily(request))
}
impl Service {
    pub fn export_bill(&self, request: &Request, path: &Path) -> Result<Exported> {
        request.validate()?;
        let mut syncing = self.0.syncing.lock().unwrap();
        if *syncing {
            return Err(failure("用量正在同步，请完成后导出"));
        }
        *syncing = true;
        drop(syncing);
        let result = (|| {
            let pricing = self.prices()?.frozen();
            let settings = self.settings();
            let spool = Spool::new(&self.0.directory)?;
            let pending = self.read(|s| snapshot(s, &spool, request, &pricing, &settings))?;
            let mut result = save_csv(&spool, request, path)?;
            result.retained = pending;
            if let Err(err) = cleanup(self, &spool, request, &mut result) {
                result.cleanup_error = Some(err.message);
            }
            Ok(result)
        })();
        *self.0.syncing.lock().unwrap() = false;
        let _ = self.0.events.send(());
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const BASE: i64 = 1_740_000_000_000;
    #[test]
    fn native_link_digest_matches_cleanup_and_list_detail() {
        let dir = tempfile::tempdir().unwrap();
        let service = Service::new(dir.path());
        let linked = row("fixture-linked", "claude", BASE);
        service.query(|store| {
            store.write_batch(std::slice::from_ref(&linked), None)?;
            db(store.connection.execute("INSERT INTO v12_links(owner,target) SELECT rid,usage_key('fixture-target') FROM v12_rows WHERE key=usage_key(?1)", [&linked.id]))?;
            let detail = store.detail(&linked.id)?;
            assert_eq!(detail.duplicate_of.as_deref(), Some("fixture-target"));
            assert_eq!(store.logs(&Filter::default())?.rows[0].duplicate_of, detail.duplicate_of);
            Ok(())
        }).unwrap();
        let exported = service
            .export_bill(&request(), &dir.path().join("bill.csv"))
            .unwrap();
        assert_eq!(
            (exported.requests, exported.cleaned, exported.retained),
            (1, 1, 0)
        );
        assert!(exported.cleanup_error.is_none());
        assert!(std::fs::read_to_string(dir.path().join("bill.csv"))
            .unwrap()
            .contains("fixture-target"));
    }
    #[test]
    fn temporary_bill_sources_are_removed_when_export_ends() {
        let dir = tempfile::tempdir().unwrap();
        let spool = Spool::new(dir.path()).unwrap();
        append(
            &spool.connection,
            &row("temporary", "claude", BASE),
            1,
            None,
        )
        .unwrap();
        let path = spool._directory.path().to_path_buf();
        assert!(path.join("spool.sqlite3").is_file());
        drop(spool);
        assert!(!path.exists());
    }
    fn row(id: &str, client: &str, end: i64) -> Record {
        Record {
            id: id.into(),
            client: client.into(),
            source: client.into(),
            started_at: end - 1500,
            finished_at: Some(end),
            completed: true,
            attempts: vec![Attempt {
                id: format!("{id}-attempt"),
                started_at: end - 1500,
                duration_ms: 1500,
                outcome: "success".into(),
                tokens: Tokens {
                    input: Some(100),
                    output: Some(20),
                    ..Tokens::default()
                },
                ..Attempt::default()
            }],
            ..Record::default()
        }
    }
    fn request() -> Request {
        Request {
            clients: vec!["claude".into(), "antigravity".into()],
            start: BASE,
            end: BASE + 1000,
            timezone: "UTC".into(),
        }
    }
    #[test]
    fn retains_bill_details_until_export_and_counts_business_failures() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        let mut failed = row("failed", "claude", BASE);
        failed.source = "proxy".into();
        failed.completed = false;
        failed.attempts[0].status = Some(200);
        failed.attempts[0].outcome = "failed".into();
        let rows = [
            row("old-claude", "claude", BASE),
            row("old-app", "antigravity", BASE),
            row("old-codex", "codex", BASE),
            failed,
        ];
        store.write_batch(&rows, None).unwrap();
        store
            .connection
            .execute(
                "INSERT OR REPLACE INTO metadata VALUES('correlation_zero_cache','done')",
                [],
            )
            .unwrap();
        store.compact(BASE + 70 * 86_400_000).unwrap();
        assert!(store.detail("old-claude").is_ok());
        assert!(store.detail("old-app").is_ok());
        assert!(store.detail("old-codex").is_err());
        let dashboard = store
            .dashboard(&Filter {
                client: Some("claude".into()),
                ..Filter::default()
            })
            .unwrap();
        assert_eq!(dashboard.totals.success, 0);
        assert_eq!(dashboard.totals.status_known, 1);
    }
    #[test]
    fn full_csv_seconds_attempts_cleanup_and_concurrent_updates() {
        let dir = tempfile::tempdir().unwrap();
        let service = Service::new(dir.path());
        let spool = Spool::new(dir.path()).unwrap();
        let request = request();
        let mut rows = vec![
            row("before", "claude", BASE - 1),
            row("last-ms", "antigravity", BASE + 1999),
            row("after", "claude", BASE + 2000),
        ];
        for i in 0..45 {
            rows.push(row(&format!("row-{i:03}"), "claude", BASE + i));
        }
        rows[3].attempts.insert(
            0,
            Attempt {
                id: "retry-fixture".into(),
                outcome: "failed".into(),
                started_at: BASE - 2000,
                ..Attempt::default()
            },
        );
        service.query(|s| s.write_batch(&rows, None)).unwrap();
        let before = service
            .read(|s| s.dashboard(&Filter::default()))
            .unwrap()
            .totals;
        assert_eq!(before.requests, 48);
        let pricing = service.prices().unwrap().frozen();
        service
            .read(|s| snapshot(s, &spool, &request, &pricing, &service.settings()))
            .unwrap();
        let mut changed = rows[4].clone();
        changed.attempts[0].tokens.output = Some(99);
        let new = row("new-arrival", "claude", BASE);
        service
            .query(|s| s.write_batch(&[changed.clone(), new.clone()], None))
            .unwrap();
        let path = dir.path().join("bill.csv");
        let mut exported = save_csv(&spool, &request, &path).unwrap();
        assert_eq!(exported.requests, 46);
        assert_eq!(exported.attempts, 47);
        let csv = std::fs::read_to_string(path).unwrap();
        assert!(csv.contains("retry-fixture"));
        assert!(csv.contains("last-ms"));
        assert!(!csv.contains("before-attempt"));
        assert!(!csv.contains("after-attempt"));
        cleanup(&service, &spool, &request, &mut exported).unwrap();
        assert_eq!(exported.cleaned, 45);
        assert_eq!(exported.retained, 1);
        assert!(service.read(|s| s.detail("before")).is_ok());
        assert!(service.read(|s| s.detail("last-ms")).is_err());
        assert!(service.read(|s| s.detail("after")).is_ok());
        assert!(service.read(|s| s.detail(&changed.id)).is_ok());
        assert!(service.read(|s| s.detail(&new.id)).is_ok());
        service.query(|s| s.write_batch(&rows, None)).unwrap();
        let after = service
            .read(|s| s.dashboard(&Filter::default()))
            .unwrap()
            .totals;
        assert_eq!(after.requests, 4);
        assert_eq!(after.attempts, 4);
        assert!(service.read(|s| s.detail("before")).is_ok());
    }
    #[test]
    fn failed_save_and_history_gap_never_clear_details() {
        let dir = tempfile::tempdir().unwrap();
        let service = Service::new(dir.path());
        let spool = Spool::new(dir.path()).unwrap();
        let r = row("preserved", "claude", BASE);
        service
            .query(|s| s.write_batch(std::slice::from_ref(&r), None))
            .unwrap();
        let prices = service.prices().unwrap().frozen();
        service
            .read(|s| snapshot(s, &spool, &request(), &prices, &service.settings()))
            .unwrap();
        assert!(save_csv(&spool, &request(), &dir.path().join("missing/bill.csv")).is_err());
        assert!(service.read(|s| s.detail("preserved")).is_ok());
        service
            .query(|s| s.archive_batch(std::slice::from_ref(&r.id)))
            .unwrap();
        let recovered = Spool::new(dir.path()).unwrap();
        assert_eq!(
            service
                .read(|s| snapshot(s, &recovered, &request(), &prices, &service.settings()))
                .unwrap(),
            0
        );
    }
    #[test]
    fn arrivals_and_updates_during_source_replay_remain_for_next_export() {
        let dir = tempfile::tempdir().unwrap();
        let service = Service::new(dir.path());
        let spool = Spool::new(dir.path()).unwrap();
        let req = request();
        let stable = row("stable", "claude", BASE);
        let mut changed = row("changed", "claude", BASE);
        let mut in_progress = row("in-progress", "claude", BASE);
        in_progress.started_at = BASE;
        in_progress.finished_at = None;
        in_progress.completed = false;
        in_progress.attempts[0].outcome = "in_progress".into();
        let out_of_range = row("out-of-range", "claude", BASE - 5_000);
        service
            .query(|s| {
                s.write_batch(
                    &[
                        stable,
                        changed.clone(),
                        in_progress.clone(),
                        out_of_range.clone(),
                    ],
                    None,
                )
            })
            .unwrap();
        let pricing = service.prices().unwrap().frozen();
        let pending = service
            .read(|s| snapshot(s, &spool, &req, &pricing, &service.settings()))
            .unwrap();
        assert_eq!(pending, 1);
        changed.attempts[0].tokens.output = Some(30);
        service
            .query(|s| s.write_batch(&[changed, row("new", "claude", BASE)], None))
            .unwrap();
        let mut result = save_csv(&spool, &req, &dir.path().join("bill.csv")).unwrap();
        result.retained = pending;
        cleanup(&service, &spool, &req, &mut result).unwrap();
        assert_eq!(result.requests, 2);
        assert_eq!(result.cleaned, 1);
        assert_eq!(result.retained, 2);
        assert!(service.read(|s| s.detail("stable")).is_err());
        assert!(service.read(|s| s.detail("changed")).is_ok());
        assert!(service.read(|s| s.detail("in-progress")).is_ok());
        assert!(service.read(|s| s.detail("new")).is_ok());
        let out_digest = storage::digest(encoded(&out_of_range).unwrap().as_bytes());
        let in_prog_digest = storage::digest(encoded(&in_progress).unwrap().as_bytes());
        let (cleaned_extra, retained_extra) = service
            .query(|s| {
                s.clean_exported_bill_batch(
                    &[
                        (out_of_range.id.clone(), out_digest),
                        (in_progress.id.clone(), in_prog_digest),
                    ],
                    &req,
                )
            })
            .unwrap();
        assert_eq!((cleaned_extra, retained_extra), (0, 2));
        assert!(service.read(|s| s.detail("out-of-range")).is_ok());
        assert!(service.read(|s| s.detail("in-progress")).is_ok());

        in_progress.finished_at = Some(BASE);
        in_progress.completed = true;
        in_progress.attempts[0].outcome = "completed".into();
        service
            .query(|s| s.write_batch(std::slice::from_ref(&in_progress), None))
            .unwrap();
        let second = service
            .export_bill(&req, &dir.path().join("bill-2.csv"))
            .unwrap();
        assert_eq!(second.requests, 3);
        assert_eq!(second.cleaned, 3);
        assert_eq!(second.retained, 0);
        assert!(service.read(|s| s.detail("changed")).is_err());
        assert!(service.read(|s| s.detail("in-progress")).is_err());
        assert!(service.read(|s| s.detail("new")).is_err());
    }
    #[test]
    fn compacted_legacy_attempts_are_an_explicit_gap() {
        let dir = tempfile::tempdir().unwrap();
        let spool = Spool::new(dir.path()).unwrap();
        let mut r = row("legacy-retries", "claude", BASE);
        r.attempts[0].repeat_count = 3;
        append(&spool.connection, &r, 1, None).unwrap();
        let destination = dir.path().join("bill.csv");
        std::fs::write(&destination, "existing-bill").unwrap();
        assert!(save_csv(&spool, &request(), &destination).is_err());
        assert_eq!(
            std::fs::read_to_string(&destination).unwrap(),
            "existing-bill"
        );
    }
    #[test]
    fn csv_escapes_formulas_unknowns_and_newlines() {
        let mut out = Vec::new();
        csv_row(
            &mut out,
            &["=formula".into(), "a,\"b\"\nc".into(), String::new()],
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "\"'=formula\",\"a,\"\"b\"\"\nc\",\"\"\r\n"
        );
        let mut r = request();
        r.end += 1;
        assert!(r.validate().is_err());
    }
    #[test]
    fn app_csv_keeps_unknown_timing_separate_from_measured_generation() {
        for generation in [Some(0), Some(50), Some(51), Some(800), None] {
            let dir = tempfile::tempdir().unwrap();
            let spool = Spool::new(dir.path()).unwrap();
            let mut r = row("app", "antigravity", BASE + 999);
            let a = &mut r.attempts[0];
            a.duration_ms = 0;
            a.generation_ms = generation;
            a.model_display_name = Some("Fixture Model".into());
            a.reasoning_effort = Some("high".into());
            a.tokens.image_cache_read = Some(7);
            a.api_provider = Some("API_PROVIDER_FIXTURE".into());
            a.error_code = Some("RESOURCE_EXHAUSTED".into());
            a.outcome = "failed".into();
            a.availability
                .insert("duration".into(), "upstream_unreported".into());
            a.availability
                .insert("attempt_start".into(), "reported".into());
            a.parse_reasons.push("attempt_time_unreported".into());
            append(&spool.connection, &r, 1, None).unwrap();
            let mut out = Vec::new();
            let result = write_csv(&spool, &request(), &mut out).unwrap();
            assert_eq!((result.requests, result.attempts), (1, 1));
            let csv = String::from_utf8(out).unwrap();
            let mut lines = csv.lines();
            let fields = |line: &str| -> Vec<String> {
                let mut result = Vec::new();
                let mut field = String::new();
                let mut quoted = false;
                let mut chars = line.chars().peekable();
                while let Some(c) = chars.next() {
                    match c {
                        '"' if quoted && chars.peek() == Some(&'"') => {
                            field.push(c);
                            chars.next();
                        }
                        '"' => quoted = !quoted,
                        ',' if !quoted => result.push(std::mem::take(&mut field)),
                        _ => field.push(c),
                    }
                }
                result.push(field);
                result
            };
            let headers = fields(lines.next().unwrap());
            let values = fields(lines.next().unwrap());
            assert_eq!(headers.len(), values.len());
            let value = |key: &str| &values[headers.iter().position(|h| h == key).unwrap()];
            assert!(!value("attempt_started_at").is_empty());
            assert!(value("attempt_duration_ms").is_empty());
            assert!(value("output_tokens_per_second").is_empty());
            assert_eq!(
                value("generation_ms"),
                &generation.map_or(String::new(), |ms| ms.to_string())
            );
            assert_eq!(
                value("generation_tokens_per_second"),
                &generation
                    .filter(|ms| *ms > 50)
                    .map_or(String::new(), |ms| format!("{:.6}", 20_000.0 / ms as f64))
            );
            assert_eq!(value("error_code"), "RESOURCE_EXHAUSTED");
            assert_eq!(value("model_display_name"), "Fixture Model");
            assert_eq!(value("reasoning_effort"), "high");
            assert_eq!(value("image_cache_read_tokens"), "7");
            assert!(value("status").is_empty());
        }
    }
    #[test]
    fn export_bill_cleans_in_window_records_and_statistics_while_rejecting_cleaned_gaps() {
        let dir = tempfile::tempdir().unwrap();
        let service = Service::new(&dir.path().join("store"));

        // 1) An old record archived into daily + receipts on the same UTC day as BASE.
        let old_cleaned = row("old-cleaned", "antigravity", BASE + 100);
        // 2) A live record inside the export window.
        let live_in_window = row("live-in-window", "antigravity", BASE + 500);
        // 3) A history_file record inside the export window that must be exported and cleaned.
        let hist_in_window_id = safe_id("antigravity:request:cascade-keep:step:1");
        let mut hist_in_window = row(&hist_in_window_id, "antigravity", BASE + 800);
        hist_in_window.session_id = Some(safe_id("cascade-keep"));
        hist_in_window.attempts[0].transport = "history_file".into();

        service
            .query(|s| {
                s.write_batch(
                    &[
                        old_cleaned.clone(),
                        live_in_window.clone(),
                        hist_in_window.clone(),
                    ],
                    None,
                )?;
                s.archive_batch(std::slice::from_ref(&old_cleaned.id))
            })
            .unwrap();

        let bill_path = dir.path().join("fuzzy-bill.csv");
        let partial_day_range = Request {
            clients: vec!["claude".into(), "antigravity".into()],
            start: BASE,
            end: BASE + 1_000,
            timezone: "UTC".into(),
        };
        let exported = service.export_bill(&partial_day_range, &bill_path).unwrap();
        assert_eq!(exported.requests, 2);
        assert_eq!(exported.cleaned, 2);
        assert_eq!(exported.retained, 0);
        assert!(exported.cleanup_error.is_none());
        assert!(service.read(|s| s.detail(&live_in_window.id)).is_err());
        assert!(service.read(|s| s.detail(&hist_in_window_id)).is_err());
        // Partial-day export must NOT delete the daily aggregate row for the surrounding day.
        let daily_after_partial: i64 = service
            .read(|s| {
                Ok(s.connection
                    .query_row("SELECT COUNT(*) FROM daily", [], |r| r.get(0))
                    .unwrap())
            })
            .unwrap();
        assert_eq!(daily_after_partial, 2);

        // Full-day export covering the entire day bucket cleans legacy daily rows even when no live records remain.
        let full_day_range = Request {
            clients: vec!["claude".into(), "antigravity".into()],
            start: BASE - 86_400_000,
            end: BASE + 172_800_000 - 1_000,
            timezone: "UTC".into(),
        };
        let second = service
            .export_bill(&full_day_range, &dir.path().join("full-day-bill.csv"))
            .unwrap();
        assert_eq!(second.requests, 0);
        assert_eq!(second.cleaned, 0);
        assert_eq!(second.retained, 0);
        let daily_after_full: i64 = service
            .read(|s| {
                Ok(s.connection
                    .query_row("SELECT COUNT(*) FROM daily", [], |r| r.get(0))
                    .unwrap())
            })
            .unwrap();
        assert_eq!(daily_after_full, 0);
        let in_window_dashboard = service
            .read(|s| {
                s.dashboard(&Filter {
                    client: Some("antigravity".into()),
                    start: Some(BASE),
                    end: Some(BASE + 1_999),
                    ..Filter::default()
                })
            })
            .unwrap();
        assert_eq!(in_window_dashboard.totals.requests, 0);
    }

    #[test]
    fn numeric_native_and_legacy_pages_export_identical_csv() {
        let dir = tempfile::tempdir().unwrap();
        let mut legacy = Store::open_v11_fixture(&dir.path().join("legacy")).unwrap();
        let mut native = Store::open(&dir.path().join("native")).unwrap();
        let mut rows: Vec<_> = (0..273)
            .rev()
            .map(|i| {
                row(
                    &format!("fixture-{i:03}"),
                    if i % 2 == 0 { "claude" } else { "antigravity" },
                    BASE + i,
                )
            })
            .collect();
        rows.push(row("outside", "claude", BASE - 1));
        rows.push(row("other-client", "codex", BASE));
        let mut pending = row("pending", "claude", BASE);
        pending.started_at = BASE;
        pending.finished_at = None;
        pending.completed = false;
        rows.push(pending);
        let pricing = Pricing::new(dir.path()).unwrap();
        let prices = pricing.frozen();
        let mut csv = Vec::new();
        for store in [&mut legacy, &mut native] {
            store.write_batch(&rows, None).unwrap();
            let spool = Spool::new(dir.path()).unwrap();
            assert_eq!(
                snapshot(store, &spool, &request(), &prices, &Settings::default()).unwrap(),
                1
            );
            let mut bytes = Vec::new();
            let result = write_csv(&spool, &request(), &mut bytes).unwrap();
            assert_eq!((result.requests, result.attempts), (273, 273));
            csv.push(bytes);
        }
        assert_eq!(csv[0], csv[1]);
    }
}
