//! Export owns a private disk spool. Source replay, CSV durability and cleanup
//! are ordered so a cancelled or incomplete export cannot remove accounting data.
use super::{
    bodies,
    model::*,
    pricing::Pricing,
    sessions::{self, Repository},
    store::{self, db, Store},
    Service,
};
use crate::storage::{self, Result};
use chrono::{Local, TimeZone};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::{
    io::{BufWriter, Write},
    path::{Path, PathBuf},
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
    fn contains(&self, r: &Record) -> bool {
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
        db(connection.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA cache_size=-2048; CREATE TABLE sources(id TEXT PRIMARY KEY,body TEXT NOT NULL); CREATE TABLE cursors(id TEXT PRIMARY KEY,body TEXT NOT NULL); CREATE TABLE cleanup_snapshot(id TEXT PRIMARY KEY,digest TEXT NOT NULL); CREATE TABLE bill(id TEXT PRIMARY KEY,body TEXT NOT NULL,effective INTEGER NOT NULL,original_digest TEXT);"))?;
        Ok(Self {
            connection,
            _directory: directory,
        })
    }
    fn source(&self, id: &str) -> Result<Option<Record>> {
        let body: Option<String> = db(self
            .connection
            .query_row("SELECT body FROM sources WHERE id=?1", [id], |r| r.get(0))
            .optional())?;
        body.map(|s| serde_json::from_str(&s).map_err(|_| failure("账单暂存记录无效")))
            .transpose()
    }
    fn capture(&mut self, records: &[Record]) -> Result<()> {
        let tx = db(self.connection.transaction())?;
        for r in records {
            let old: Option<String> = db(tx
                .query_row("SELECT body FROM sources WHERE id=?1", [&r.id], |row| {
                    row.get(0)
                })
                .optional())?;
            let old = old
                .map(|s| serde_json::from_str(&s).map_err(|_| failure("账单来源记录无效")))
                .transpose()?;
            let record = store::preserve_record(r, old)?;
            let body = serde_json::to_string(&record).map_err(|_| failure("账单来源记录无效"))?;
            db(tx.execute("INSERT INTO sources VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET body=excluded.body",params![record.id,body]))?;
        }
        db(tx.commit())
    }
}
impl Repository for Spool {
    fn requires_complete_history(&self) -> bool {
        true
    }
    fn cursor(&self, id: &str) -> Result<Option<String>> {
        db(self
            .connection
            .query_row("SELECT body FROM cursors WHERE id=?1", [id], |r| r.get(0))
            .optional())
    }
    fn generation(&self, _: &str) -> Result<u64> {
        Ok(0)
    }
    fn write_batch(&mut self, rows: &[Record], cursor: Option<(&str, &str)>) -> Result<()> {
        self.capture(rows)?;
        if let Some((id, body)) = cursor {
            db(self.connection.execute("INSERT INTO cursors VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET body=excluded.body",params![id,body]))?;
        }
        Ok(())
    }
    fn begin_rebuild(&mut self, _: &str) -> Result<()> {
        Ok(())
    }
    fn stage_rebuild(
        &mut self,
        _: &str,
        rows: &[Record],
        cursor: Option<(&str, &str)>,
    ) -> Result<()> {
        self.write_batch(rows, cursor)
    }
    fn finish_rebuild(&mut self, _: &str) -> Result<()> {
        Ok(())
    }
}
fn finished(r: &Record) -> bool {
    r.finished_at.is_some()
        && (r.source != "claude" || r.completed)
        && !r.attempts.is_empty()
        && r.final_attempt()
            .is_some_and(|a| !matches!(a.outcome.as_str(), "in_progress" | "pending"))
}
fn encoded(r: &Record) -> Result<String> {
    serde_json::to_string(r).map_err(|_| failure("账单记录无法序列化"))
}
fn append(spool: &Connection, r: &Record, effective: i64, digest: Option<&str>) -> Result<()> {
    db(spool.execute("INSERT INTO bill VALUES(?1,?2,?3,?4) ON CONFLICT(id) DO UPDATE SET body=excluded.body,effective=excluded.effective,original_digest=excluded.original_digest",params![r.id,encoded(r)?,effective,digest]))?;
    Ok(())
}
fn capture_cleanup_snapshot(store: &Store, spool: &Spool, request: &Request) -> Result<()> {
    let tx = db(spool.connection.unchecked_transaction())?;
    for client in &request.clients {
        let mut after = String::new();
        loop {
            let mut statement = db(store.connection.prepare("SELECT id,body,duplicate_of FROM records WHERE client=?1 AND time<=?2 AND id>?3 ORDER BY id LIMIT 128"))?;
            let rows = db(
                statement.query_map(params![client, request.end + 999, after], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<String>>(2)?,
                    ))
                }),
            )?;
            let rows = db(rows.collect::<rusqlite::Result<Vec<_>>>())?;
            if rows.is_empty() {
                break;
            }
            for (id, body, duplicate) in rows {
                after = id;
                let mut r: Record = bodies::decode(&store.connection, &body)?;
                r.duplicate_of = duplicate;
                if finished(&r) {
                    db(tx.execute(
                        "INSERT INTO cleanup_snapshot VALUES(?1,?2)",
                        params![r.id, storage::digest(encoded(&r)?.as_bytes())],
                    ))?;
                }
            }
        }
    }
    db(tx.commit())
}
fn snapshot(
    store: &Store,
    spool: &Spool,
    request: &Request,
    pricing: &Pricing,
    settings: &Settings,
) -> Result<u64> {
    let tx = db(spool.connection.unchecked_transaction())?;
    let mut pending = 0;
    for client in &request.clients {
        let mut after = String::new();
        loop {
            let mut statement=db(store.connection.prepare("SELECT id,body,effective,duplicate_of FROM records WHERE client=?1 AND time<=?2 AND id>?3 ORDER BY id LIMIT 128"))?;
            let rows = db(
                statement.query_map(params![client, request.end + 999, after], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, Option<String>>(3)?,
                    ))
                }),
            )?;
            let rows = db(rows.collect::<rusqlite::Result<Vec<_>>>())?;
            if rows.is_empty() {
                break;
            }
            for (id, body, effective, duplicate) in rows {
                after = id;
                let mut r: Record = bodies::decode(&store.connection, &body)?;
                r.duplicate_of = duplicate;
                if !finished(&r) {
                    pending += 1;
                    continue;
                }
                let digest = storage::digest(encoded(&r)?.as_bytes());
                let original: Option<String> = db(spool
                    .connection
                    .query_row(
                        "SELECT digest FROM cleanup_snapshot WHERE id=?1",
                        [&r.id],
                        |row| row.get(0),
                    )
                    .optional())?;
                let unchanged = original.as_deref() == Some(digest.as_str());
                if !unchanged {
                    pending += 1;
                }
                estimate_missing(&mut r, pricing, settings);
                append(
                    &spool.connection,
                    &r,
                    effective,
                    unchanged.then_some(digest.as_str()),
                )?;
            }
        }
        let mut statement=db(store.connection.prepare("SELECT id,source,effective FROM receipts WHERE client=?1 AND time BETWEEN ?2 AND ?3 ORDER BY id"))?;
        let rows = db(statement.query_map(
            params![client, request.start, request.end + 999],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            },
        ))?;
        for row in rows {
            let (id, source, effective) = db(row)?;
            let recovered = spool.source(&id)?;
            if source == "proxy" || recovered.is_none() {
                return Err(failure(
                    "所选时间段存在已归并或已删除的逐笔历史，来源无法完整恢复；未导出、未清理",
                ));
            }
            let mut r = recovered.unwrap();
            if !finished(&r) || !request.contains(&r) {
                return Err(failure("历史请求结束时间无法完整核对；未导出、未清理"));
            }
            estimate_missing(&mut r, pricing, settings);
            append(&spool.connection, &r, effective, None)?;
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
                speed(a.generation_ms),
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
fn cleanup(service: &Service, spool: &Spool, result: &mut Exported) -> Result<()> {
    let mut after = String::new();
    loop {
        let mut stmt=db(spool.connection.prepare("SELECT id,original_digest FROM bill WHERE original_digest IS NOT NULL AND id>?1 ORDER BY id LIMIT 128"))?;
        let rows = db(stmt.query_map([&after], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        }))?;
        let rows = db(rows.collect::<rusqlite::Result<Vec<_>>>())?;
        if rows.is_empty() {
            break;
        }
        after = rows.last().unwrap().0.clone();
        let cleaned = service.query(|store| {
            let mut ids = Vec::new();
            for (id, digest) in &rows {
                if let Some(r) = super::storage_v12::record(&store.connection, id)? {
                    // Updates and arrivals after the export snapshot remain live.
                    if storage::digest(encoded(&r)?.as_bytes()) == *digest {
                        ids.push(id.clone());
                    }
                }
            }
            store.archive_batch(&ids)?;
            Ok(ids.len() as u64)
        })?;
        result.cleaned += cleaned;
        result.retained += rows.len() as u64 - cleaned;
    }
    Ok(())
}
impl Service {
    pub fn export_bill(
        &self,
        request: &Request,
        roots: &[(String, PathBuf)],
        path: &Path,
    ) -> Result<Exported> {
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
            let mut spool = Spool::new(&self.0.directory)?;
            // Source replay can take time. Only details already stable when
            // export began are eligible for cleanup after the file is durable.
            self.read(|s| capture_cleanup_snapshot(s, &spool, request))?;
            for (client, home) in roots.iter().filter(|(c, _)| request.clients.contains(c)) {
                let report = if client == "antigravity" {
                    super::antigravity::sync(&mut spool, &pricing, &settings, home, false)?
                } else {
                    sessions::sync(&mut spool, &pricing, &settings, client, home, true)?
                };
                if report.errors > 0 {
                    return Err(failure("来源历史读取不完整，未导出、未清理"));
                }
            }
            // Newly discovered history uses the normal dedup path. Receipts
            // suppress already summarized records and retain existing snapshots.
            let mut after = String::new();
            loop {
                let mut statement = db(spool
                    .connection
                    .prepare("SELECT id,body FROM sources WHERE id>?1 ORDER BY id LIMIT 128"))?;
                let rows = db(statement.query_map([&after], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                }))?;
                let rows = db(rows.collect::<rusqlite::Result<Vec<_>>>())?;
                if rows.is_empty() {
                    break;
                }
                after = rows.last().unwrap().0.clone();
                let mut records = Vec::new();
                for (_, body) in rows {
                    records.push(
                        serde_json::from_str::<Record>(&body)
                            .map_err(|_| failure("账单来源记录无效"))?,
                    );
                }
                self.query(|s| s.write_batch(&records, None))?;
            }
            let pending = self.read(|s| snapshot(s, &spool, request, &pricing, &settings))?;
            let mut result = save_csv(&spool, request, path)?;
            result.retained = pending;
            if let Err(error) = cleanup(self, &spool, &mut result) {
                result.cleanup_error = Some(error.message);
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
    fn temporary_bill_sources_are_removed_when_export_ends() {
        let dir = tempfile::tempdir().unwrap();
        let mut spool = Spool::new(dir.path()).unwrap();
        spool.capture(&[row("temporary", "claude", BASE)]).unwrap();
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
        let pricing = service.prices().unwrap().frozen();
        service
            .read(|s| capture_cleanup_snapshot(s, &spool, &request))
            .unwrap();
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
        cleanup(&service, &spool, &mut exported).unwrap();
        assert_eq!(exported.cleaned, 46);
        assert_eq!(exported.retained, 1);
        assert!(service.read(|s| s.detail("before")).is_err());
        assert!(service.read(|s| s.detail("last-ms")).is_err());
        assert!(service.read(|s| s.detail("after")).is_ok());
        assert!(service.read(|s| s.detail(&changed.id)).is_ok());
        assert!(service.read(|s| s.detail(&new.id)).is_ok());
        service.query(|s| s.write_batch(&rows, None)).unwrap();
        let after = service
            .read(|s| s.dashboard(&Filter::default()))
            .unwrap()
            .totals;
        assert_eq!(after.requests, before.requests + 1);
        assert_eq!(after.attempts, before.attempts + 1);
        assert!(service.read(|s| s.detail("before")).is_err());
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
        let mut recovered = Spool::new(dir.path()).unwrap();
        assert!(service
            .read(|s| snapshot(s, &recovered, &request(), &prices, &service.settings()))
            .is_err());
        recovered.capture(std::slice::from_ref(&r)).unwrap();
        service
            .read(|s| snapshot(s, &recovered, &request(), &prices, &service.settings()))
            .unwrap();
        let mut csv = Vec::new();
        let result = write_csv(&recovered, &request(), &mut csv).unwrap();
        assert_eq!(result.requests, 1);
        assert!(service.read(|s| s.detail("preserved")).is_err());
        assert_eq!(
            service
                .read(|s| s.dashboard(&Filter::default()))
                .unwrap()
                .totals
                .requests,
            1
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
        service
            .query(|s| s.write_batch(&[stable, changed.clone()], None))
            .unwrap();
        service
            .read(|s| capture_cleanup_snapshot(s, &spool, &req))
            .unwrap();
        changed.attempts[0].tokens.output = Some(30);
        service
            .query(|s| s.write_batch(&[changed, row("new", "claude", BASE)], None))
            .unwrap();
        let pricing = service.prices().unwrap().frozen();
        let pending = service
            .read(|s| snapshot(s, &spool, &req, &pricing, &service.settings()))
            .unwrap();
        let mut result = save_csv(&spool, &req, &dir.path().join("bill.csv")).unwrap();
        result.retained = pending;
        cleanup(&service, &spool, &mut result).unwrap();
        assert_eq!(result.requests, 3);
        assert_eq!(result.cleaned, 1);
        assert_eq!(result.retained, 2);
        assert!(service.read(|s| s.detail("stable")).is_err());
        assert!(service.read(|s| s.detail("changed")).is_ok());
        assert!(service.read(|s| s.detail("new")).is_ok());
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
        let dir = tempfile::tempdir().unwrap();
        let spool = Spool::new(dir.path()).unwrap();
        let mut r = row("app", "antigravity", BASE + 999);
        let a = &mut r.attempts[0];
        a.duration_ms = 0;
        a.generation_ms = Some(800);
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
        assert_eq!(value("generation_ms"), "800");
        assert_eq!(value("generation_tokens_per_second"), "25.000000");
        assert_eq!(value("error_code"), "RESOURCE_EXHAUSTED");
        assert_eq!(value("model_display_name"), "Fixture Model");
        assert_eq!(value("reasoning_effort"), "high");
        assert_eq!(value("image_cache_read_tokens"), "7");
        assert!(value("status").is_empty());
    }
    #[test]
    fn partial_source_history_blocks_export_before_cleanup() {
        let dir = tempfile::tempdir().unwrap();
        let service = Service::new(&dir.path().join("store"));
        service
            .query(|s| s.write_batch(&[row("retained", "claude", BASE)], None))
            .unwrap();
        let home = dir.path().join("claude");
        std::fs::create_dir_all(home.join("projects/fixture")).unwrap();
        std::fs::write(
            home.join("projects/fixture/session.jsonl"),
            b"{\"type\":\"assistant\"",
        )
        .unwrap();
        let mut req = request();
        req.clients = vec!["claude".into()];
        let path = dir.path().join("bill.csv");
        assert!(service
            .export_bill(&req, &[("claude".into(), home)], &path)
            .is_err());
        assert!(!path.exists());
        assert!(service.read(|s| s.detail("retained")).is_ok());

        let history = dir
            .path()
            .join("antigravity/brain/fixture/.system_generated/logs/transcript.jsonl");
        std::fs::create_dir_all(history.parent().unwrap()).unwrap();
        std::fs::write(history, b"{\"input_tokens\":").unwrap();
        let mut spool = Spool::new(dir.path()).unwrap();
        assert!(super::super::antigravity::scan_files(
            &mut spool,
            dir.path(),
            &service.prices().unwrap().frozen(),
            &service.settings(),
            &mut sessions::Report::default(),
            false,
        )
        .is_err());
    }
    #[test]
    fn app_transcript_cannot_replace_api_attempts_or_their_snapshots() {
        let dir = tempfile::tempdir().unwrap();
        let mut spool = Spool::new(dir.path()).unwrap();
        let mut api = row("request", "antigravity", BASE);
        api.attempts[0].transport = "local_api".into();
        api.attempts[0].cost_multiplier = "1.25".into();
        let mut retry = api.attempts[0].clone();
        retry.id = "retry".into();
        retry.outcome = "failed".into();
        api.attempts.insert(0, retry);
        spool.capture(&[api]).unwrap();
        let mut transcript = row("request", "antigravity", BASE);
        transcript.attempts[0].transport = "history_file".into();
        transcript.attempts[0].tokens.output = Some(99);
        spool.capture(&[transcript]).unwrap();
        let saved = spool.source("request").unwrap().unwrap();
        assert_eq!(saved.attempts.len(), 2);
        assert_eq!(saved.attempts[0].outcome, "failed");
        assert_eq!(saved.attempts[1].tokens.output, Some(20));
        assert_eq!(saved.attempts[1].cost_multiplier, "1.25");
    }
}
