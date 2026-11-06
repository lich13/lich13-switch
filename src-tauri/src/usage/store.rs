use super::{
    bodies, dedup,
    model::*,
    pricing::{Pricing, Quote},
    query, storage_v12,
};
use crate::storage::{self, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::{
    cell::Cell,
    collections::BTreeMap,
    path::Path,
    sync::{Arc, Mutex},
};
const DAY: i64 = 86_400_000;
#[derive(Default)]
pub(super) struct BackfillCursor {
    after: String,
    daily: bool,
}

pub(super) fn db<T>(r: rusqlite::Result<T>) -> Result<T> {
    r.map_err(|_| failure("用量数据库操作失败"))
}
pub struct Store {
    pub(super) connection: Connection,
    overview_cache: OverviewCache,
    maintenance_deferred: Cell<Option<&'static str>>,
}
pub type OverviewCache = Arc<Mutex<Option<(String, Dashboard)>>>;
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Page {
    pub rows: Vec<Record>,
    pub total: u64,
    pub page: u32,
    pub detail_since: i64,
    pub data_version: u64,
}
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Group {
    pub id: String,
    pub totals: Totals,
}
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Dashboard {
    pub totals: Totals,
    pub providers: Vec<Group>,
    pub models: Vec<Group>,
    pub precision: String,
    pub detail_since: i64,
    pub sources: BTreeMap<String, u64>,
    pub review_count: u64,
    pub source_history_incomplete: bool,
    pub data_version: u64,
}
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Daily {
    #[serde(default)]
    scope: Option<String>,
    client: String,
    provider: Option<String>,
    model: Option<String>,
    source: String,
    status: Option<u16>,
    totals: Totals,
    example: Record,
}
fn common_schema(c: &Connection, legacy_receipts: bool) -> Result<()> {
    let operation = if legacy_receipts {
        ""
    } else {
        ",operation TEXT NOT NULL DEFAULT 'model'"
    };
    db(c.execute_batch(&format!("          CREATE TABLE IF NOT EXISTS cursors(id TEXT PRIMARY KEY,body TEXT NOT NULL);
          CREATE TABLE IF NOT EXISTS daily(id TEXT PRIMARY KEY,day INTEGER NOT NULL,body TEXT NOT NULL);
          CREATE TABLE IF NOT EXISTS metadata(key TEXT PRIMARY KEY,value TEXT NOT NULL);
          CREATE TABLE IF NOT EXISTS price_bases(hash TEXT PRIMARY KEY,body TEXT NOT NULL);
          CREATE TABLE IF NOT EXISTS body_shapes(id INTEGER PRIMARY KEY,hash TEXT UNIQUE NOT NULL,body TEXT NOT NULL);
          CREATE TABLE IF NOT EXISTS receipts(id TEXT PRIMARY KEY,client TEXT NOT NULL,source TEXT NOT NULL,time INTEGER NOT NULL,provider TEXT,response TEXT,signature TEXT,ended INTEGER NOT NULL,effective INTEGER NOT NULL{operation});
          CREATE INDEX IF NOT EXISTS receipt_response ON receipts(client,response);
          CREATE INDEX IF NOT EXISTS receipt_signature ON receipts(client,signature,ended);
          CREATE INDEX IF NOT EXISTS daily_time ON daily(day);")))
}

fn protect_sidecars(dir: &Path) -> Result<()> {
    for suffix in ["-wal", "-shm"] {
        let file = dir.join(format!("usage.sqlite{suffix}"));
        if file.exists() {
            storage::protect(&file, false)?;
        }
    }
    Ok(())
}

impl Store {
    pub fn open(dir: &Path) -> Result<Self> {
        Self::open_layout(dir, false)
    }
    #[cfg(test)]
    pub fn open_v11_fixture(dir: &Path) -> Result<Self> {
        Self::open_layout(dir, true)
    }
    fn open_layout(dir: &Path, legacy: bool) -> Result<Self> {
        storage::private_dir(dir)?;
        let path = dir.join("usage.sqlite");
        if std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(failure("用量数据库路径无效"));
        }
        let mut connection = db(Connection::open(&path))?;
        storage::protect(&path, false)?;
        let version: i64 = db(connection.query_row("PRAGMA user_version", [], |row| row.get(0)))?;
        if version > 13 {
            return Err(failure("用量数据库来自更高版本，已保留原文件"));
        }
        super::maintenance::configure(&connection)?;
        db(connection.busy_timeout(std::time::Duration::from_secs(3)))?;
        query::register(&connection)?;
        if storage_v12::active(&connection) {
            storage_v12::protect_encoding(&connection)?;
            db(connection.execute_batch("DROP TABLE IF EXISTS v12_links;"))?;
            storage_v12::refresh_metrics(&connection)?;
            return Ok(Self {
                connection,
                overview_cache: Default::default(),
                maintenance_deferred: Cell::default(),
            });
        }
        let empty = version == 0
            && !legacy
            && db(connection.query_row(
                "SELECT NOT EXISTS(SELECT 1 FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%')",
                [],
                |row| row.get::<_, bool>(0),
            ))?;
        db(connection.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;"))?;
        if empty {
            let tx = db(connection.transaction())?;
            common_schema(&tx, false)?;
            storage_v12::initialize(&tx)?;
            db(tx.commit())?;
            protect_sidecars(dir)?;
            return Ok(Self {
                connection,
                overview_cache: Default::default(),
                maintenance_deferred: Cell::default(),
            });
        }
        common_schema(&connection, true)?;
        db(connection.execute_batch("CREATE TABLE IF NOT EXISTS records(id TEXT PRIMARY KEY,client TEXT NOT NULL,source TEXT NOT NULL,time INTEGER NOT NULL,provider TEXT,model TEXT,status INTEGER,response TEXT,signature TEXT,effective INTEGER NOT NULL DEFAULT 1,duplicate_of TEXT,body TEXT NOT NULL);
          CREATE INDEX IF NOT EXISTS usage_time ON records(time,effective);
          CREATE INDEX IF NOT EXISTS usage_response ON records(client,response);
          CREATE INDEX IF NOT EXISTS usage_signature ON records(client,signature,time);
"))?;
        query::register(&connection)?;
        query::schema(&connection)?;
        if version < 7 {
            let tx = db(connection.transaction())?;
            if version < 4 {
                db(tx.execute_batch(
                    "ALTER TABLE receipts ADD COLUMN operation TEXT NOT NULL DEFAULT 'model';",
                ))?;
            }
            for column in [
                "first_token_sum_ms",
                "first_token_samples",
                "cache_read_eligible",
                "cache_input_eligible",
            ] {
                let exists: bool = db(tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM pragma_table_info('usage_metrics') WHERE name=?1)",
                    [column],
                    |r| r.get(0),
                ))?;
                if !exists {
                    db(tx.execute_batch(&format!(
                        "ALTER TABLE usage_metrics ADD COLUMN {column} INTEGER NOT NULL DEFAULT 0;"
                    )))?;
                }
            }
            let mut after = String::new();
            loop {
                let rows = {
                    let mut stmt = db(
                        tx.prepare("SELECT id,body FROM records WHERE id>?1 ORDER BY id LIMIT 256")
                    )?;
                    let rows = db(stmt.query_map([&after], |r| {
                        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                    }))?;
                    db(rows.collect::<rusqlite::Result<Vec<_>>>())?
                };
                if rows.is_empty() {
                    break;
                }
                for (id, body) in rows {
                    let record: Record = bodies::decode(&tx, &body)?;
                    query::project(&tx, 0, &id, &record, 1, None)?;
                    if version < 7
                        && record.source == "proxy"
                        && record
                            .final_attempt()
                            .is_some_and(|a| a.response_id.is_some())
                    {
                        super::dedup::supplement(&tx, &id)?;
                    }
                    if version < 5 {
                        db(tx.execute(
                            "UPDATE records SET signature=?2 WHERE id=?1",
                            params![id, record.signature()],
                        ))?;
                    }
                    after = id;
                }
            }
            // Old daily summaries contain no reconstructible TTFT; serde defaults
            // their sample count to zero without altering stored prices or bodies.
            let rows = {
                let mut stmt = db(tx.prepare("SELECT id,body FROM daily"))?;
                let rows =
                    db(stmt
                        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))))?;
                db(rows.collect::<rusqlite::Result<Vec<_>>>())?
            };
            for (id, body) in rows {
                let daily: Daily = bodies::decode(&tx, &body)?;
                query::project(
                    &tx,
                    1,
                    &id,
                    &daily.example,
                    daily.totals.requests,
                    Some(&daily.totals),
                )?;
            }
            if version > 0 && version < 7 {
                db(tx.execute("INSERT OR REPLACE INTO metadata(key,value) VALUES('repair_codex','1'),('repair_claude','1')", []))?;
            }
            db(tx.execute_batch("PRAGMA user_version=7;"))?;
            query::changed(&tx)?;
            db(tx.commit())?;
        }
        if version < 8 {
            db(connection.execute_batch("BEGIN IMMEDIATE; INSERT OR IGNORE INTO metadata(key,value) VALUES('repair_v020_after',''),('repair_codex','1'),('repair_claude','1'); PRAGMA user_version=8; COMMIT;"))?;
        }
        if version < 9 {
            db(connection.execute_batch("BEGIN IMMEDIATE; INSERT OR IGNORE INTO metadata(key,value) VALUES('storage_records_after',''),('storage_daily_after',''),('storage_repack','1'); PRAGMA user_version=9; COMMIT;"))?;
        }
        protect_sidecars(dir)?;
        if version < 10 {
            db(connection.execute_batch("BEGIN IMMEDIATE; ALTER TABLE records ADD COLUMN local_sequence INTEGER NOT NULL DEFAULT 0; UPDATE records SET local_sequence=rowid; INSERT OR REPLACE INTO metadata(key,value) SELECT 'record_sequence',coalesce(max(local_sequence),0) FROM records; INSERT OR IGNORE INTO metadata(key,value) VALUES('finish_time_after',''); PRAGMA user_version=10; COMMIT;"))?;
        }
        if version < 11 {
            db(connection.execute_batch("BEGIN IMMEDIATE; INSERT OR REPLACE INTO metadata(key,value) VALUES('storage_records_after',''),('storage_daily_after',''),('storage_repack','1'); PRAGMA user_version=11; COMMIT;"))?;
        }
        super::pending_prices::ensure(&connection)?;
        if !legacy {
            storage_v12::install(&connection)?;
            if version == 0 {
                db(connection.execute("DELETE FROM metadata WHERE key IN ('storage_records_after','storage_daily_after','repair_v020_after','finish_time_after')", []))?;
                storage_v12::copy_batch(&mut connection)?;
            }
        }
        Ok(Self {
            connection,
            overview_cache: Default::default(),
            maintenance_deferred: Cell::default(),
        })
    }
    pub fn reader(dir: &Path) -> Result<Self> {
        let connection = db(Connection::open_with_flags(
            dir.join("usage.sqlite"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        ))?;
        query::register(&connection)?;
        db(connection.busy_timeout(std::time::Duration::from_secs(3)))?;
        Ok(Self {
            connection,
            overview_cache: Default::default(),
            maintenance_deferred: Cell::default(),
        })
    }
    pub fn reclaim_space(&self) -> Result<()> {
        let pending: bool = storage_v12::pending(&self.connection) || storage_v12::packing_pending(&self.connection)? || db(self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM metadata WHERE key IN ('storage_records_after','storage_daily_after'))",
            [], |row| row.get(0),
        ))?;
        let repack: bool = db(self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM metadata WHERE key='storage_repack')",
            [],
            |row| row.get(0),
        ))?;
        let result = super::maintenance::reclaim(&self.connection, !pending, repack)?;
        self.maintenance_deferred.set(match result {
            super::maintenance::Reclaim::Deferred(reason) => Some(reason),
            super::maintenance::Reclaim::CheckpointPending => {
                Some("用量记录已整理，空间回收等待查询结束")
            }
            _ => None,
        });
        if matches!(
            result,
            super::maintenance::Reclaim::Ready | super::maintenance::Reclaim::CheckpointPending
        ) && repack
        {
            db(self
                .connection
                .execute("DELETE FROM metadata WHERE key='storage_repack'", []))?;
        }
        Ok(())
    }
    pub fn maintenance_status(&self) -> Option<String> {
        if let Some(reason) = self.maintenance_deferred.get() {
            return Some(reason.into());
        }
        if storage_v12::pending(&self.connection) {
            let (done, total): (u64, u64) = self
                .connection
                .query_row(
                    "SELECT (SELECT count(*) FROM v12_records),(SELECT count(*) FROM records)",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .ok()?;
            Some(format!("用量存储整理中：{done} / {total}"))
        } else if self
            .connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM metadata WHERE key='storage_repack')",
                [],
                |r| r.get::<_, bool>(0),
            )
            .unwrap_or(false)
        {
            Some("用量存储正在回收空间".into())
        } else {
            None
        }
    }
    pub fn deferred_backfill(&self) -> bool {
        !storage_v12::pending(&self.connection)
            && self
                .connection
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM metadata WHERE key='backfill_after_storage')",
                    [],
                    |r| r.get::<_, bool>(0),
                )
                .unwrap_or(false)
    }
    fn repair_business_success_batch(&mut self) -> Result<bool> {
        let cursor: Option<String> = db(self
            .connection
            .query_row(
                "SELECT value FROM metadata WHERE key='billing_success_completed'",
                [],
                |r| r.get(0),
            )
            .optional())?;
        if cursor.as_deref() == Some("done") {
            return Ok(false);
        }
        let native = storage_v12::active(&self.connection);
        let cursor = cursor.unwrap_or_else(|| if native { "d:".into() } else { "r:".into() });
        let daily = native || cursor.starts_with("d:");
        let after = cursor.get(2..).unwrap_or("");
        let table = if daily { "daily" } else { "records" };
        let rows = {
            let mut s = db(self.connection.prepare(&format!(
                "SELECT id,body FROM {table} WHERE id>?1 ORDER BY id LIMIT 128"
            )))?;
            let rows = db(s.query_map([after], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            }))?;
            db(rows.collect::<rusqlite::Result<Vec<_>>>())?
        };
        let tx = db(self.connection.transaction())?;
        let metrics = if native {
            "usage_daily_metrics"
        } else {
            "usage_metrics"
        };
        let mut changed = false;
        for (id, body) in &rows {
            let failed = if daily {
                let mut d: Daily = bodies::decode(&tx, body)?;
                if !d.example.completed && d.totals.success > 0 {
                    d.totals.success = 0;
                    db(tx.execute(
                        "UPDATE daily SET body=?2 WHERE id=?1",
                        params![id, bodies::encode(&tx, &d)?],
                    ))?;
                }
                !d.example.completed
            } else {
                !bodies::decode::<Record>(&tx, body)?.completed
            };
            if failed {
                changed |= db(tx.execute(
                    &format!("UPDATE {metrics} SET success=0 WHERE owner=?1 AND success<>0"),
                    [id],
                ))? > 0;
            }
        }
        let next = rows
            .last()
            .map(|(id, _)| format!("{}:{id}", if daily { "d" } else { "r" }))
            .unwrap_or_else(|| if daily { "done".into() } else { "d:".into() });
        db(tx.execute(
            "INSERT OR REPLACE INTO metadata VALUES('billing_success_completed',?1)",
            [next],
        ))?;
        if changed {
            query::changed(&tx)?;
        }
        db(tx.commit())?;
        Ok(!daily || !rows.is_empty())
    }
    /// Restartable storage-only conversion. Prices and associations remain exact;
    /// identical gateway/source metric projections share the same stored rows.
    pub fn compact_storage_batch(&mut self) -> Result<bool> {
        let repaired_success = self.repair_business_success_batch()?;
        if storage_v12::active(&self.connection) {
            return storage_v12::compact_diagnostics(&self.connection)
                .map(|changed| changed || repaired_success);
        }
        if storage_v12::pending(&self.connection)
            && (self.repair_metadata_batch()? || self.repair_finish_times()?)
        {
            return Ok(true);
        }
        for (table, key) in [
            ("records", "storage_records_after"),
            ("daily", "storage_daily_after"),
        ] {
            let after: Option<String> = db(self
                .connection
                .query_row("SELECT value FROM metadata WHERE key=?1", [key], |row| {
                    row.get(0)
                })
                .optional())?;
            let Some(after) = after else {
                continue;
            };
            let tx = db(self.connection.transaction())?;
            let rows = {
                let mut statement = db(tx.prepare(&format!(
                    "SELECT id,body FROM {table} WHERE id>?1 ORDER BY id LIMIT 2000"
                )))?;
                let rows = db(statement.query_map([after], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                }))?;
                db(rows.collect::<rusqlite::Result<Vec<_>>>())?
            };
            if rows.is_empty() {
                db(tx.execute("DELETE FROM metadata WHERE key=?1", [key]))?;
            } else {
                for (id, body) in &rows {
                    let compact = if table == "records" {
                        let record = bodies::decode::<Record>(&tx, body)?;
                        query::project(&tx, 0, id, &record, 1, None)?;
                        bodies::encode(&tx, &record)?
                    } else {
                        bodies::encode(&tx, &bodies::decode::<Daily>(&tx, body)?)?
                    };
                    if &compact != body {
                        db(tx.execute(
                            &format!("UPDATE {table} SET body=?2 WHERE id=?1"),
                            params![id, compact],
                        ))?;
                    }
                }
                db(tx.execute(
                    "UPDATE metadata SET value=?2 WHERE key=?1",
                    params![key, rows.last().unwrap().0],
                ))?;
            }
            db(tx.commit())?;
            super::maintenance::reclaim(&self.connection, false, false)?;
            return Ok(true);
        }
        storage_v12::copy_batch(&mut self.connection)
    }
    pub fn repair_finish_times(&mut self) -> Result<bool> {
        if storage_v12::active(&self.connection) {
            return Ok(false);
        }
        let after: Option<String> = db(self
            .connection
            .query_row(
                "SELECT value FROM metadata WHERE key='finish_time_after'",
                [],
                |r| r.get(0),
            )
            .optional())?;
        let Some(after) = after else {
            return Ok(false);
        };
        let tx = db(self.connection.transaction())?;
        let rows = {
            let mut stmt =
                db(tx.prepare("SELECT id,body FROM records WHERE id>?1 ORDER BY id LIMIT 128"))?;
            let values = db(stmt.query_map([after], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            }))?;
            db(values.collect::<rusqlite::Result<Vec<_>>>())?
        };
        for (id, body) in &rows {
            let mut r: Record = bodies::decode(&tx, body)?;
            r.finished_at = Some(r.event_time());
            db(tx.execute(
                "UPDATE records SET time=?2,body=?3 WHERE id=?1",
                params![id, r.event_time(), bodies::encode(&tx, &r)?],
            ))?;
        }
        if let Some((last, _)) = rows.last() {
            db(tx.execute(
                "UPDATE metadata SET value=?1 WHERE key='finish_time_after'",
                [last],
            ))?;
        } else {
            db(tx.execute("DELETE FROM metadata WHERE key='finish_time_after'", []))?;
        }
        if !rows.is_empty() {
            query::changed(&tx)?;
        }
        db(tx.commit())?;
        Ok(!rows.is_empty())
    }
    pub fn reader_with_cache(dir: &Path, cache: OverviewCache) -> Result<Self> {
        let mut reader = Self::reader(dir)?;
        reader.overview_cache = cache;
        Ok(reader)
    }
    pub fn snapshot<T>(&self, f: impl FnOnce(&Self) -> Result<T>) -> Result<T> {
        db(self.connection.execute_batch("BEGIN DEFERRED;"))?;
        let result = f(self);
        let end = db(self.connection.execute_batch("ROLLBACK;"));
        result.and_then(|value| end.map(|()| value))
    }
    /// Bounded, restartable metadata repair. Old rate snapshots remain unchanged.
    pub fn repair_metadata_batch(&mut self) -> Result<bool> {
        if self.repair_antigravity_batch()? {
            return Ok(true);
        }
        if storage_v12::active(&self.connection) {
            return Ok(false);
        }
        let after: Option<String> = db(self
            .connection
            .query_row(
                "SELECT value FROM metadata WHERE key='repair_v020_after'",
                [],
                |r| r.get(0),
            )
            .optional())?;
        let Some(after) = after else {
            return Ok(false);
        };
        let tx = db(self.connection.transaction())?;
        let rows = {
            let mut statement =
                db(tx.prepare("SELECT id,body FROM records WHERE id>?1 ORDER BY id LIMIT 64"))?;
            let rows = db(statement.query_map([&after], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            }))?;
            db(rows.collect::<rusqlite::Result<Vec<_>>>())?
        };
        if rows.is_empty() {
            db(tx.execute("DELETE FROM metadata WHERE key='repair_v020_after'", []))?;
        } else {
            for (id, body) in &rows {
                let mut record: Record = bodies::decode(&tx, body)?;
                for attempt in &mut record.attempts {
                    attempt.annotate_availability(&record.source);
                }
                if let Some(attempt) = &mut record.gateway_reported {
                    attempt.annotate_availability("proxy");
                }
                db(tx.execute(
                    "UPDATE records SET body=?2 WHERE id=?1",
                    params![id, bodies::encode(&tx, &record)?],
                ))?;
                if record.source == "proxy" {
                    dedup::supplement(&tx, id)?;
                }
                let body: String =
                    db(tx.query_row("SELECT body FROM records WHERE id=?1", [id], |r| r.get(0)))?;
                let repaired: Record = bodies::decode(&tx, &body)?;
                query::project(&tx, 0, id, &repaired, 1, None)?;
            }
            db(tx.execute(
                "UPDATE metadata SET value=?1 WHERE key='repair_v020_after'",
                [&rows.last().unwrap().0],
            ))?;
            query::changed(&tx)?;
        }
        db(tx.commit())?;
        Ok(!rows.is_empty())
    }
    fn repair_antigravity_batch(&mut self) -> Result<bool> {
        if storage_v12::pending(&self.connection) {
            return Ok(false);
        }
        let progress: Option<String> = db(self
            .connection
            .query_row(
                "SELECT value FROM metadata WHERE key='antigravity_fields_v02619'",
                [],
                |r| r.get(0),
            )
            .optional())?;
        if progress.as_deref() == Some("complete") {
            return Ok(false);
        }
        let mut initialize = None;
        let after = if let Some(after) = progress.as_deref().and_then(|p| p.strip_prefix("after:"))
        {
            Some(after.to_owned())
        } else {
            // v0.26.19's "done" meant the old repair was reset, not completed.
            let legacy: Option<String> = if progress.as_deref() == Some("done") {
                db(self
                    .connection
                    .query_row(
                        "SELECT value FROM metadata WHERE key='antigravity_fields_v02614'",
                        [],
                        |r| r.get(0),
                    )
                    .optional())?
            } else {
                None
            };
            let next = match legacy.as_deref() {
                Some("done") => "complete".to_owned(),
                Some(after) => format!("after:{after}"),
                None => "after:".to_owned(),
            };
            initialize = Some(next);
            legacy
        };
        let ag_home = crate::antigravity::home().ok();
        let has_vault = self
            .data_dir()
            .is_some_and(|d| d.join("antigravity-accounts.json").exists());
        let timeline = self
            .data_dir()
            .map(|d| crate::antigravity::auth::provider_timeline(d, ag_home.as_deref()))
            .unwrap_or_default();
        let tx = db(self.connection.transaction())?;
        if let Some(next) = initialize {
            db(tx.execute(
                "INSERT OR REPLACE INTO metadata VALUES('antigravity_fields_v02619',?1)",
                [&next],
            ))?;
            db(tx.execute("DELETE FROM metadata WHERE key IN ('antigravity_fields_v02614','antigravity_fields_v02617','antigravity_fields_v02618')", []))?;
            if next == "complete" {
                db(tx.commit())?;
                return Ok(false);
            }
        }
        let rows = if storage_v12::active(&tx) {
            let mut stmt = db(tx.prepare("SELECT usage_text(key),rid FROM v12_rows WHERE source=usage_label('antigravity') AND usage_text(key)>?1 ORDER BY usage_text(key) LIMIT 128"))?;
            let rows = db(stmt.query_map([after.as_deref().unwrap_or("")], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
            }))?;
            db(rows.collect::<rusqlite::Result<Vec<_>>>())?
                .into_iter()
                .map(|(id, rid)| Ok((id, storage_v12::load_record(&tx, rid)?)))
                .collect::<Result<Vec<_>>>()?
        } else {
            let mut stmt = db(tx.prepare("SELECT id,body FROM records WHERE source='antigravity' AND id>?1 ORDER BY id LIMIT 128"))?;
            let rows = db(stmt.query_map([after.as_deref().unwrap_or("")], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            }))?;
            db(rows.collect::<rusqlite::Result<Vec<_>>>())?
                .into_iter()
                .map(|(id, body)| Ok((id, bodies::decode::<Record>(&tx, &body)?)))
                .collect::<Result<Vec<_>>>()?
        };
        if after.as_deref().unwrap_or("").is_empty() && !rows.is_empty() {
            db(tx.execute(
                "INSERT OR REPLACE INTO metadata VALUES('backfill_after_storage','1')",
                [],
            ))?;
        }
        let mut changed = false;
        for (id, record) in &rows {
            let mut r = record.clone();
            let normalized = super::antigravity::normalize_record(&mut r);
            let mut provider_changed = false;
            for a in &mut r.attempts {
                let expected = timeline.provider_at(a.started_at).map(str::to_owned);
                let legacy_placeholder = a.provider.as_deref() == Some("antigravity");
                if (has_vault || legacy_placeholder || expected.is_some()) && a.provider != expected
                {
                    a.provider = expected;
                    provider_changed = true;
                }
            }
            let generation_changed = r
                .attempts
                .iter()
                .any(|a| a.transport == "local_api" && a.generation_ms.is_none_or(|ms| ms <= 50));
            if normalized || generation_changed || provider_changed {
                query::project(&tx, 0, id, &r, 1, None)?;
                if normalized || provider_changed {
                    storage_v12::update(&tx, &r)?;
                }
                changed = true;
            }
        }
        db(tx.execute(
            "INSERT OR REPLACE INTO metadata(key,value) VALUES('antigravity_fields_v02619',?1)",
            [rows
                .last()
                .map_or_else(|| "complete".to_owned(), |(id, _)| format!("after:{id}"))],
        ))?;
        if changed {
            query::changed(&tx)?;
        }
        db(tx.commit())?;
        Ok(!rows.is_empty())
    }
    /// Revisit retained evidence once after the cache-read correlation change.
    /// Daily summaries have no one-to-one evidence and are deliberately untouched.
    pub fn repair_correlations_batch(&mut self) -> Result<bool> {
        if storage_v12::pending(&self.connection) {
            return Ok(false);
        }
        let cursor: Option<String> = db(self
            .connection
            .query_row(
                "SELECT value FROM metadata WHERE key='correlation_zero_cache'",
                [],
                |r| r.get(0),
            )
            .optional())?;
        if cursor.as_deref() == Some("done") {
            return Ok(false);
        }
        let compact = storage_v12::active(&self.connection);
        let prefix = if compact { "v12:" } else { "v11:" };
        let after = cursor
            .as_deref()
            .and_then(|s| s.strip_prefix(prefix))
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(0);
        let tx = db(self.connection.transaction())?;
        let rows = if compact {
            storage_v12::correlation_repair_rows(&tx, after)?
        } else {
            let mut statement = db(tx.prepare(
                "SELECT rowid,body FROM records WHERE rowid>?1 ORDER BY rowid LIMIT 128",
            ))?;
            let rows = db(statement.query_map([after], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            }))?;
            rows.map(|r| {
                let (id, body) = db(r)?;
                Ok((id, bodies::decode(&tx, &body)?))
            })
            .collect::<Result<Vec<(i64, Record)>>>()?
        };
        for (_, record) in &rows {
            if !matches!(record.source.as_str(), "proxy" | "grok")
                && record
                    .final_attempt()
                    .is_some_and(|a| a.tokens.cache_read.is_none_or(|n| n == 0))
            {
                dedup::reconcile(&tx, record)?;
            }
        }
        let next = if rows.len() < 128 {
            "done".into()
        } else {
            format!("{prefix}{}", rows.last().unwrap().0)
        };
        db(tx.execute(
            "INSERT OR REPLACE INTO metadata(key,value) VALUES('correlation_zero_cache',?1)",
            [next],
        ))?;
        if !rows.is_empty() {
            storage_v12::flush(&tx)?;
            query::changed(&tx)?;
        }
        db(tx.commit())?;
        Ok(!rows.is_empty())
    }
    pub fn needs_repair(&self, source: &str) -> Result<bool> {
        db(self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM metadata WHERE key=?1 AND value='1')",
            [format!("repair_{source}")],
            |r| r.get(0),
        ))
    }
    pub fn source_counts(&self, source: &str) -> Result<(u64, u64, i64)> {
        let sql = if storage_v12::active(&self.connection) {
            "SELECT count(*) FILTER (WHERE effective=0),count(*) FILTER (WHERE effective=2) FROM v12_rows WHERE source=usage_label(?1)"
        } else {
            "SELECT count(*) FILTER (WHERE effective=0),count(*) FILTER (WHERE effective=2) FROM records WHERE source=?1"
        };
        let (merged, pending) = db(self
            .connection
            .query_row(sql, [source], |r| Ok((r.get(0)?, r.get(1)?))))?;
        let historical = db(self.connection.query_row(
            "SELECT coalesce((SELECT cast(value as integer) FROM metadata WHERE key=?1),0)",
            [format!("historical_{source}")],
            |r| r.get(0),
        ))?;
        Ok((merged, pending, historical))
    }
    pub fn data_dir(&self) -> Option<&Path> {
        let dir = self.connection.path().and_then(|p| Path::new(p).parent())?;
        if dir.file_name().and_then(|n| n.to_str()) == Some("usage")
            && !dir.join("antigravity-accounts.json").exists()
        {
            dir.parent().or(Some(dir))
        } else {
            Some(dir)
        }
    }
    pub fn cursor(&self, id: &str) -> Result<Option<String>> {
        db(self
            .connection
            .query_row("SELECT body FROM cursors WHERE id=?1", [id], |r| r.get(0))
            .optional())
    }
    pub fn source_generation(&self, source: &str) -> Result<u64> {
        db(self.connection.query_row(
            "SELECT coalesce((SELECT cast(value AS INTEGER) FROM metadata WHERE key=?1),0)",
            [format!("source_generation_{source}")],
            |row| row.get(0),
        ))
    }
    pub fn write_batch(&mut self, records: &[Record], cursor: Option<(&str, &str)>) -> Result<()> {
        self.write_batch_with_superseded(records, &[], cursor)
    }
    pub fn write_batch_with_superseded(
        &mut self,
        records: &[Record],
        superseded_ids: &[String],
        cursor: Option<(&str, &str)>,
    ) -> Result<()> {
        if records.is_empty() && superseded_ids.is_empty() {
            match cursor {
                None => return Ok(()),
                Some((id, body)) if self.cursor(id)?.as_deref() == Some(body) => return Ok(()),
                _ => {}
            }
        }
        let tx = db(self.connection.transaction())?;
        let mut changed = false;
        for r in records {
            changed |= insert(&tx, r)?;
        }
        storage_v12::flush(&tx)?;
        changed |= Self::remove_superseded_in_tx(&tx, superseded_ids)?;
        if let Some((id, body)) = cursor {
            db(tx.execute("INSERT INTO cursors(id,body) VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET body=excluded.body WHERE cursors.body<>excluded.body",params![id,body]))?;
        }
        if changed {
            query::changed(&tx)?;
        }
        db(tx.commit())
    }
    pub fn detail(&self, id: &str) -> Result<Record> {
        // Keep the record and its immutable shared shape in one read snapshot,
        // including while background pruning removes unreferenced shapes.
        let _snapshot = self.read_snapshot()?;
        storage_v12::record(&self.connection, id)?.ok_or_else(|| failure("用量记录不存在"))
    }
    fn read_snapshot(&self) -> Result<Option<rusqlite::Transaction<'_>>> {
        self.connection
            .is_autocommit()
            .then(|| db(self.connection.unchecked_transaction()))
            .transpose()
    }
    pub fn logs(&self, f: &Filter) -> Result<Page> {
        let _snapshot = self.read_snapshot()?;
        let (condition, args) = query::conditions(f, "r", true);
        let condition = query::native_sql(&self.connection, condition);
        // Ambiguous records remain inspectable, but are excluded from aggregates.
        let eligible = query::eligible(&self.connection, f, "r", true);
        let where_sql = format!("{eligible} AND {condition}");
        let total: u64 = db(self.connection.query_row(
            &format!("SELECT count(*) FROM records r WHERE {where_sql}"),
            rusqlite::params_from_iter(args.iter()),
            |r| r.get(0),
        ))?;
        let page = f.page.max(1).min((total.div_ceil(20) as u32).max(1));
        let native = storage_v12::active(&self.connection);
        let projection = if native { "rid" } else { "body,duplicate_of" };
        let mut stmt=db(self.connection.prepare(&format!("SELECT {projection} FROM records r WHERE {where_sql} ORDER BY time DESC,local_sequence DESC,id DESC LIMIT 20 OFFSET {}",(page-1)*20)))?;
        let mut rows = db(stmt.query(rusqlite::params_from_iter(args.iter())))?;
        let mut records = Vec::new();
        while let Some(row) = db(rows.next())? {
            let mut r: Record = if native {
                storage_v12::load_record(&self.connection, db(row.get(0))?)?
            } else {
                let mut r: Record =
                    bodies::decode(&self.connection, &db(row.get::<_, String>(0))?)?;
                r.duplicate_of = db(row.get(1))?;
                r
            };
            if f.source.as_deref() == Some("proxy") {
                r.gateway_only();
            }
            records.push(r);
        }
        Ok(Page {
            rows: records,
            total,
            page,
            detail_since: self.detail_since()?,
            data_version: query::version(&self.connection)?,
        })
    }
    pub(crate) fn recent_gateway_requests(
        &self,
        client: &str,
        at: i64,
    ) -> Result<BTreeMap<String, u64>> {
        // A logical gateway record counts once, regardless of retry attempts or
        // correlated client-session records. The existing time index bounds this read.
        let sql = query::native_sql(&self.connection,
            "SELECT r.provider,count(*) FROM records r WHERE r.client=? AND r.source='proxy' AND r.effective=1 AND r.time>? AND r.time<=? AND r.provider IS NOT NULL GROUP BY r.provider".into());
        let mut stmt = db(self.connection.prepare(&sql))?;
        let rows = db(stmt.query_map(params![client, at - 3_600_000, at], |row| {
            Ok((row.get(0)?, row.get(1)?))
        }))?;
        db(rows.collect())
    }
    pub fn dashboard(&self, f: &Filter) -> Result<Dashboard> {
        let _snapshot = self.read_snapshot()?;
        let key = query::overview_key(&self.connection, f)?;
        if let Some((saved, view)) = self.overview_cache.lock().unwrap().as_ref() {
            if saved == &key {
                return Ok(view.clone());
            }
        }
        let view = query::dashboard(&self.connection, f, self.detail_since()?)?;
        *self.overview_cache.lock().unwrap() = Some((key, view.clone()));
        Ok(view)
    }
    pub(super) fn save_antigravity_quota(
        &mut self,
        id: &str,
        periods: &super::antigravity::quota::Periods,
    ) -> Result<()> {
        let key = format!("antigravity_quota:{id}");
        let body = serde_json::to_string(periods).map_err(|_| failure("当前周期无法保存"))?;
        let current: Option<String> = db(self
            .connection
            .query_row("SELECT value FROM metadata WHERE key=?1", [&key], |r| {
                r.get(0)
            })
            .optional())?;
        if current.as_deref() != Some(&body) {
            db(self.connection.execute("INSERT INTO metadata(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value", params![key, body]))?;
        }
        Ok(())
    }
    pub fn antigravity_stats(
        &self,
        f: &Filter,
        now_ms: i64,
    ) -> Result<super::antigravity::AntigravityUsageStats> {
        self.client_stats("antigravity", f, now_ms)
    }
    pub fn claude_stats(
        &self,
        f: &Filter,
        now_ms: i64,
    ) -> Result<super::antigravity::AntigravityUsageStats> {
        self.client_stats("claude", f, now_ms)
    }
    fn client_stats(
        &self,
        client: &str,
        f: &Filter,
        now_ms: i64,
    ) -> Result<super::antigravity::AntigravityUsageStats> {
        let f = &Filter {
            client: Some(client.into()),
            ..f.clone()
        };
        let _snapshot = self.read_snapshot()?;
        let mut agg = super::antigravity::UsageStatsAggregator::new(f, now_ms);
        let accounts = (client == "antigravity")
            .then(|| self.data_dir())
            .flatten()
            .map(crate::antigravity::auth::usage_accounts)
            .transpose()?
            .unwrap_or_default();
        let mut min_time = f.start.unwrap_or(0);
        let mut max_time = f.end.unwrap_or(now_ms);
        for (order, account) in accounts.into_iter().enumerate() {
            let raw: Option<String> =
                if account.kind == crate::antigravity::auth::AccountKind::Oauth {
                    db(self
                        .connection
                        .query_row(
                            "SELECT value FROM metadata WHERE key=?1",
                            [format!("antigravity_quota:{}", account.id)],
                            |r| r.get(0),
                        )
                        .optional())?
                } else {
                    None
                };
            let periods = raw
                .map(|s| serde_json::from_str(&s).map_err(|_| failure("已保存的账号周期无法读取")))
                .transpose()?
                .unwrap_or_default();
            let id = account.id.clone();
            let acc = super::antigravity::quota::AccountAccumulator::new(account, periods, now_ms);
            if let Some(start) = acc.earliest() {
                min_time = min_time.min(start);
                max_time = max_time.max(now_ms);
            }
            agg.accounts.insert(id, (order, acc));
        }
        if storage_v12::active(&self.connection) {
            storage_v12::scan_client_attempts(
                &self.connection,
                client,
                min_time,
                max_time,
                f.operation.as_deref(),
                |row| {
                    agg.ingest(row);
                    Ok(())
                },
            )?;
        } else {
            let mut stmt = db(self.connection.prepare(
                "SELECT body FROM records WHERE client=?3 AND effective=1 AND time>=?1 AND time<=?2 ORDER BY rowid",
            ))?;
            let mut rows = db(stmt.query(params![min_time, max_time, client]))?;
            while let Some(row) = db(rows.next())? {
                let body: String = db(row.get(0))?;
                let r: Record = bodies::decode(&self.connection, &body)?;
                let operation = r.final_attempt().map(|a| {
                    if a.operation == Operation::WebSearch {
                        "web_search"
                    } else if a.operation == Operation::Compaction || a.compaction_kind.is_some() {
                        "compaction"
                    } else {
                        "model"
                    }
                });
                if f.operation
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .is_some_and(|wanted| Some(wanted) != operation)
                {
                    continue;
                }
                let last_idx = r.attempts.len().saturating_sub(1);
                let mut attempts = Vec::with_capacity(r.attempts.len());
                let time = r.event_time();
                for (idx, a) in r.attempts.iter().enumerate() {
                    let is_final = idx == last_idx;
                    attempts.push(storage_v12::AntigravityAttemptRow {
                        final_provider: None,
                        session_id: r.session_id.clone(),
                        provider: a.provider.clone(),
                        time,
                        started_at: a.started_at,
                        is_final,
                        model: a
                            .grouping_model()
                            .map(str::to_owned)
                            .or_else(|| a.response_model.clone())
                            .or_else(|| a.requested_model.clone()),
                        display_name: a.model_display_name.clone(),
                        tokens: a.tokens.clone(),
                        cache_savings_rates: a.price.as_ref().and_then(|p| {
                            super::pricing::cache_savings_rates(&p.rates, &p.multiplier)
                        }),
                        cost: a.price.as_ref().map(|p| p.cost.clone()),
                        request_unpriced: a.unpriced_count() > 0,
                        provider_unpriced: false,
                        duration_ms: if is_final {
                            a.measured_duration_ms().unwrap_or(0)
                        } else {
                            0
                        },
                    });
                }
                storage_v12::visit_request_attempts(&mut attempts, |row| {
                    agg.ingest(row);
                    Ok(())
                })?;
            }
        }
        let mut stats = agg.finish();
        if !stats.top_sessions.is_empty() {
            let mut stmt = db(self.connection.prepare("SELECT json_extract(body,'$.session_id'),json_extract(body,'$.title') FROM cursors WHERE json_extract(body,'$.title') IS NOT NULL"))?;
            let titles =
                db(stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))))?;
            for title in titles {
                let (id, title) = db(title)?;
                if let Some(row) = stats.top_sessions.iter_mut().find(|r| r.session_id == id) {
                    row.title = Some(title);
                }
            }
        }
        Ok(stats)
    }
    pub fn detail_since(&self) -> Result<i64> {
        let value: Option<String> = db(self
            .connection
            .query_row(
                "SELECT value FROM metadata WHERE key='detail_since'",
                [],
                |r| r.get(0),
            )
            .optional())?;
        Ok(value.and_then(|s| s.parse().ok()).unwrap_or(0))
    }
    fn archive_details_batch(&mut self, at: i64) -> Result<bool> {
        storage_v12::cold_batch(&mut self.connection, at)
    }
    pub fn compact(&mut self, at: i64) -> Result<()> {
        if storage_v12::pending(&self.connection) {
            return Ok(());
        }
        self.repair_correlations_batch()?;
        if !db(self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM metadata WHERE key='correlation_zero_cache' AND value='done')",
            [], |r| r.get::<_, bool>(0),
        ))? {
            // Keep the remaining evidence until its bounded repair has run.
            return Ok(());
        }
        self.archive_details_batch(at)?;
        let cutoff = query::bucket(at - 30 * DAY, DAY);
        let tx = db(self.connection.transaction())?;
        loop {
            let ids = {
                let sql = if storage_v12::active(&tx) {
                    "SELECT usage_text(key) FROM v12_rows WHERE time<?1 AND client NOT IN (usage_label('claude'),usage_label('antigravity')) ORDER BY time,key LIMIT 128"
                } else {
                    "SELECT id FROM records WHERE time<?1 AND client NOT IN ('claude','antigravity') ORDER BY time,id LIMIT 128"
                };
                let mut statement = db(tx.prepare(sql))?;
                let rows = db(statement.query_map([cutoff], |r| r.get::<_, String>(0)))?;
                db(rows.collect::<rusqlite::Result<Vec<_>>>())?
            };
            if ids.is_empty() {
                break;
            }
            Self::archive_into(&tx, &ids)?;
        }
        bodies::prune_shapes(&tx)?;
        db(tx.execute("INSERT INTO metadata(key,value) VALUES('detail_since',?1) ON CONFLICT(key) DO UPDATE SET value=max(cast(value AS INTEGER),cast(excluded.value AS INTEGER))",[cutoff.to_string()]))?;
        db(tx.commit())
    }
    /// The exact set is selected while holding the writer lock. Each transaction
    /// accounts and deletes the same bounded set; no global age-based deletion.
    #[cfg(test)]
    pub(super) fn archive_batch(&mut self, ids: &[String]) -> Result<()> {
        let tx = db(self.connection.transaction())?;
        Self::archive_into(&tx, ids)?;
        db(tx.commit())
    }
    fn archive_into(tx: &rusqlite::Transaction<'_>, ids: &[String]) -> Result<()> {
        let native = storage_v12::active(tx);
        db(tx.execute_batch("CREATE TEMP TABLE IF NOT EXISTS archive_ids(id TEXT PRIMARY KEY,rid INTEGER); DELETE FROM archive_ids;"))?;
        let sql = if native {
            "INSERT OR IGNORE INTO archive_ids SELECT usage_text(key),rid FROM v12_rows WHERE usage_hint(key)=usage_hint(usage_key(?1)) AND key=usage_key(?1)"
        } else {
            "INSERT OR IGNORE INTO archive_ids SELECT id,NULL FROM records WHERE id=?1"
        };
        let mut selected = db(tx.prepare_cached(sql))?;
        for id in ids {
            db(selected.execute([id]))?;
        }
        drop(selected);
        let scopes = [None, Some("proxy"), Some("sessions"), Some("grok_sessions")];
        let eligible: Vec<_> = scopes
            .iter()
            .map(|scope| {
                let eligible = query::eligible(
                    tx,
                    &Filter {
                        source: scope.map(str::to_owned),
                        ..Filter::default()
                    },
                    "r",
                    false,
                );
                // CASE keeps eligibility's correlated checks short-circuited,
                // as they were when each scope had its own WHERE clause.
                format!("CASE WHEN {eligible} THEN 1 ELSE 0 END")
            })
            .collect();
        let (table, key, projection) = if native {
            ("(SELECT rid,usage_text(key) id,source source_code,time,effective,duplicate,archived_duplicate FROM v12_rows)", "rid", "rid")
        } else {
            ("records", "id", "body,duplicate_of")
        };
        let mut statement = db(tx.prepare(&format!(
            "SELECT {projection},{} FROM {table} r WHERE r.{key} IN (SELECT {key} FROM archive_ids)",
            eligible.join(",")
        )))?;
        let mut rows = db(statement.query([]))?;
        while let Some(row) = db(rows.next())? {
            let offset = if native { 1 } else { 2 };
            let selected = (0..scopes.len())
                .map(|i| db(row.get::<_, bool>(offset + i)))
                .collect::<Result<Vec<_>>>()?;
            if !selected.iter().any(|selected| *selected) {
                continue;
            }
            let record: Record = if native {
                storage_v12::load_record(tx, db(row.get(0))?)?
            } else {
                let mut r: Record = bodies::decode(tx, &db(row.get::<_, String>(0))?)?;
                r.duplicate_of = db(row.get(1))?;
                r
            };
            for (scope, selected) in scopes.into_iter().zip(selected) {
                if !selected {
                    continue;
                }
                let mut r = record.clone();
                if scope == Some("proxy") {
                    r.gateway_only();
                }
                let scope = scope.map(str::to_owned);
                let day = query::bucket(r.event_time(), DAY);
                let Some(a) = r.final_attempt() else {
                    continue;
                };
                let provider = a.provider.clone();
                let model = a.grouping_model().map(str::to_owned);
                let status = a.status;
                let mut t = Totals::default();
                t.add_record(&r);
                r.id.clear();
                r.session_id = None;
                r.started_at = day;
                // A per-request finish must not split otherwise identical daily buckets.
                r.finished_at = None;
                r.duplicate_of = None;
                r.gateway_reported = None;
                for a in &mut r.attempts {
                    a.id.clear();
                    a.response_id = None;
                    a.end = None;
                    a.transfer = None;
                    a.thread_hash = None;
                    a.upstream_request_hash = None;
                    a.routing_decision = None;
                    a.started_at = day;
                    a.duration_ms = 0;
                    a.first_token_ms = None;
                    // Per-request availability is not a billing dimension. Its
                    // first-token marker must not split otherwise identical days.
                    a.availability.clear();
                }
                // Exact token/tier/price dimensions are retained for safe future backfill.
                let key = safe_id(
                    &serde_json::to_string(&(&scope, &r)).map_err(|_| failure("汇总失败"))?,
                );
                let old: Option<String> = db(tx
                    .query_row("SELECT body FROM daily WHERE id=?1", [&key], |row| {
                        row.get(0)
                    })
                    .optional())?;
                let mut d = if let Some(body) = old {
                    bodies::decode::<Daily>(tx, &body)?
                } else {
                    Daily {
                        scope: Some(scope.unwrap_or_else(|| "all".into())),
                        client: r.client.clone(),
                        provider,
                        model,
                        source: r.source.clone(),
                        status,
                        totals: Totals::default(),
                        example: r,
                    }
                };
                d.totals.add(&t);
                db(tx.execute("INSERT INTO daily(id,day,body) VALUES(?1,?2,?3) ON CONFLICT(id) DO UPDATE SET body=excluded.body",params![key,day,bodies::encode(tx, &d)?]))?;
                query::project(tx, 1, &key, &d.example, d.totals.requests, Some(&d.totals))?;
            }
        }
        drop(rows);
        drop(statement);
        if native {
            storage_v12::archive_selected(tx)?;
        } else {
            db(tx.execute("INSERT OR IGNORE INTO receipts(id,client,source,time,provider,response,signature,ended,effective,operation) SELECT id,client,source,time,provider,response,signature,time,effective,coalesce(json_extract(body,'$.attempts[#-1].operation'),'model') FROM records WHERE id IN (SELECT id FROM archive_ids)",[]))?;
            db(tx.execute("DELETE FROM usage_metrics WHERE kind IN (0,2) AND owner IN (SELECT id FROM archive_ids)",[]))?;
            db(tx.execute(
                "DELETE FROM records WHERE id IN (SELECT id FROM archive_ids)",
                [],
            ))?;
        }
        query::changed(tx)
    }
    pub(super) fn clean_exported_bill_batch(
        &mut self,
        rows: &[(String, String)],
        request: &super::billing::Request,
    ) -> Result<(u64, u64)> {
        let tx = db(self.connection.transaction())?;
        let mut verified_ids = Vec::with_capacity(rows.len());
        let mut retained = 0u64;
        for (id, expected_digest) in rows {
            let Some(r) = storage_v12::record(&tx, id)? else {
                retained += 1;
                continue;
            };
            if !request.contains(&r)
                || !super::billing::finished(&r)
                || storage::digest(super::billing::encoded(&r)?.as_bytes()) != *expected_digest
            {
                retained += 1;
                continue;
            }
            verified_ids.push(id.clone());
        }
        let native = storage_v12::active(&tx);
        if !verified_ids.is_empty() {
            db(tx.execute_batch(
                "CREATE TEMP TABLE IF NOT EXISTS archive_ids(id TEXT PRIMARY KEY,rid INTEGER); DELETE FROM archive_ids;",
            ))?;
            let sql = if native {
                "INSERT OR IGNORE INTO archive_ids SELECT usage_text(key),rid FROM v12_rows WHERE usage_hint(key)=usage_hint(usage_key(?1)) AND key=usage_key(?1)"
            } else {
                "INSERT OR IGNORE INTO archive_ids SELECT id,NULL FROM records WHERE id=?1"
            };
            let mut selected = db(tx.prepare_cached(sql))?;
            for id in &verified_ids {
                db(selected.execute([id]))?;
            }
            drop(selected);
            if native {
                storage_v12::archive_selected(&tx)?;
            } else {
                db(tx.execute("INSERT OR IGNORE INTO receipts(id,client,source,time,provider,response,signature,ended,effective,operation) SELECT id,client,source,time,provider,response,signature,time,effective,coalesce(json_extract(body,'$.attempts[#-1].operation'),'model') FROM records WHERE id IN (SELECT id FROM archive_ids)", []))?;
                db(tx.execute("DELETE FROM usage_metrics WHERE kind IN (0,2) AND owner IN (SELECT id FROM archive_ids)", []))?;
                db(tx.execute(
                    "DELETE FROM records WHERE id IN (SELECT id FROM archive_ids)",
                    [],
                ))?;
            }
        }
        if !verified_ids.is_empty() {
            query::changed(&tx)?;
        }
        db(tx.commit())?;
        Ok((verified_ids.len() as u64, retained))
    }
    pub(super) fn clean_exported_bill_daily(
        &mut self,
        request: &super::billing::Request,
    ) -> Result<()> {
        let native = storage_v12::active(&self.connection);
        let tx = db(self.connection.transaction())?;
        let window_end = request.end.saturating_add(1_000);
        let daily_metrics = if native {
            "usage_daily_metrics"
        } else {
            "usage_metrics"
        };
        let mut daily_deleted = 0usize;
        for client in &request.clients {
            db(tx.execute(
                &format!("DELETE FROM {daily_metrics} WHERE kind=1 AND owner IN (SELECT id FROM daily WHERE day>=?1 AND day+?3<=?2 AND usage_day_end(day)<=?2 AND json_extract(body,'$.client')=?4)"),
                params![request.start, window_end, DAY, client],
            ))?;
            daily_deleted += db(tx.execute(
                "DELETE FROM daily WHERE day>=?1 AND day+?3<=?2 AND usage_day_end(day)<=?2 AND json_extract(body,'$.client')=?4",
                params![request.start, window_end, DAY, client],
            ))?;
        }
        if daily_deleted > 0 {
            query::changed(&tx)?;
        }
        db(tx.commit())?;
        Ok(())
    }
    #[cfg(test)]
    pub fn backfill(&mut self, pricing: &Pricing) -> Result<()> {
        let prices = pricing.frozen();
        let mut cursor = BackfillCursor::default();
        while self.backfill_batch(&prices, &mut cursor)? {}
        Ok(())
    }
    pub(super) fn backfill_batch(
        &mut self,
        pricing: &Pricing,
        cursor: &mut BackfillCursor,
    ) -> Result<bool> {
        if storage_v12::pending(&self.connection) {
            db(self.connection.execute(
                "INSERT OR REPLACE INTO metadata VALUES('backfill_after_storage','1')",
                [],
            ))?;
            return Ok(false);
        }
        let native = storage_v12::active(&self.connection);
        let tx = db(self.connection.transaction())?;
        let sql = if cursor.daily {
            "SELECT id,body FROM daily WHERE id>?1 AND json_extract(body,'$.totals.unpriced')>0 ORDER BY id LIMIT 128"
        } else if native {
            "SELECT cast(r.rid as text),r.rid FROM v12_rows r WHERE r.rid>cast(?1 as integer) AND (EXISTS(SELECT 1 FROM v12_attempts a WHERE a.owner=r.rid AND a.unpriced>0) OR EXISTS(SELECT 1 FROM v12_sessions a WHERE a.owner=r.rid AND a.unpriced>0)) ORDER BY r.rid LIMIT 128"
        } else {
            super::pending_prices::SELECT
        };
        let rows = {
            let mut stmt = db(tx.prepare_cached(sql))?;
            let rows = db(stmt.query_map([&cursor.after], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    if native && !cursor.daily {
                        None
                    } else {
                        Some(r.get::<_, String>(1)?)
                    },
                    if native && !cursor.daily {
                        Some(r.get::<_, i64>(1)?)
                    } else {
                        None
                    },
                ))
            }))?;
            db(rows.collect::<rusqlite::Result<Vec<_>>>())?
        };
        let mut changed = false;
        for (id, body, rid) in &rows {
            if !cursor.daily {
                let mut r: Record = if let Some(rid) = rid {
                    storage_v12::load_record(&tx, *rid)?
                } else {
                    bodies::decode(&tx, body.as_deref().unwrap())?
                };
                let mut updated = false;
                for a in &mut r.attempts {
                    if a.price.is_none() && a.compacted_unpriced.is_none() {
                        a.price = a.calculate_price(pricing);
                        updated |= a.price.is_some();
                    }
                }
                if updated {
                    query::project(&tx, 0, &r.id, &r, 1, None)?;
                    storage_v12::update(&tx, &r)?;
                    changed = true;
                }
            } else {
                let mut d: Daily = bodies::decode(&tx, body.as_deref().unwrap())?;
                let mut updated = false;
                for a in &mut d.example.attempts {
                    if a.price.is_none() && a.compacted_unpriced.is_none() {
                        a.price = a.calculate_price(pricing);
                        updated |= a.price.is_some();
                    }
                }
                if updated {
                    let known = d
                        .example
                        .attempts
                        .iter()
                        .filter_map(|a| a.price.as_ref().and_then(|p| decimal(&p.cost)))
                        .fold(rust_decimal::Decimal::ZERO, |sum, cost| sum + cost);
                    d.totals.cost =
                        (known * rust_decimal::Decimal::from(d.totals.requests)).to_string();
                    if d.example.cost().is_some() {
                        d.totals.unpriced = 0;
                    }
                    query::project(&tx, 1, id, &d.example, d.totals.requests, Some(&d.totals))?;
                    db(tx.execute(
                        "UPDATE daily SET body=?2 WHERE id=?1",
                        params![id, bodies::encode(&tx, &d)?],
                    ))?;
                    changed = true;
                }
            }
        }
        if changed {
            query::changed(&tx)?;
        }
        let done = rows.is_empty() && cursor.daily;
        if done {
            db(tx.execute(
                "DELETE FROM metadata WHERE key='backfill_after_storage'",
                [],
            ))?;
        }
        db(tx.commit())?;
        if let Some((last, _, _)) = rows.last() {
            cursor.after = last.clone();
        } else {
            cursor.after.clear();
            cursor.daily = true;
        }
        Ok(!done)
    }
    fn remove_superseded_in_tx(tx: &Connection, ids: &[String]) -> Result<bool> {
        if ids.is_empty() {
            return Ok(false);
        }
        let mut changed = false;
        let mut verified_sessions = std::collections::BTreeSet::new();
        for raw_id in ids {
            let id = raw_id.trim();
            if id.len() != 64 || !id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
                return Err(failure("待清理的历史记录标识越界或格式无效"));
            }
            let Some(existing) = storage_v12::record(tx, id)? else {
                continue;
            };
            if existing.source != "antigravity"
                || existing.attempts.is_empty()
                || !existing
                    .attempts
                    .iter()
                    .all(|a| a.transport == "history_file")
            {
                return Err(failure("拒绝删除非 Antigravity 历史文件来源的用量记录"));
            }
            let Some(session_id) = existing.session_id.as_deref() else {
                return Err(failure("待清理的历史记录缺少会话标识"));
            };
            if !verified_sessions.contains(session_id) {
                if !storage_v12::has_local_api_for_session(tx, session_id)? {
                    return Err(failure("未找到替代的本地接口用量记录，拒绝删除历史记录"));
                }
                verified_sessions.insert(session_id.to_owned());
            }
            if storage_v12::active(tx) {
                storage_v12::remove_record(tx, id)?;
            } else {
                db(tx.execute(
                    "DELETE FROM usage_metrics WHERE kind IN (0,2) AND owner=?1",
                    [id],
                ))?;
                db(tx.execute("DELETE FROM records WHERE id=?1", [id]))?;
            }
            changed = true;
        }
        Ok(changed)
    }
    pub fn remove_superseded(&mut self, ids: &[String]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let tx = db(self.connection.transaction())?;
        if Self::remove_superseded_in_tx(&tx, ids)? {
            query::changed(&tx)?;
        }
        db(tx.commit())
    }
    pub fn begin_rebuild(&mut self, source: &str) -> Result<()> {
        if !matches!(source, "codex" | "claude" | "grok") {
            return Err(failure("只可重建会话来源"));
        }
        if storage_v12::pending(&self.connection) {
            return Err(failure("用量存储整理中，请稍后重建来源"));
        }
        let tx = db(self.connection.transaction())?;
        db(tx.execute_batch("CREATE TABLE IF NOT EXISTS source_stage(source TEXT NOT NULL,id TEXT NOT NULL,body BLOB NOT NULL,PRIMARY KEY(source,id)) WITHOUT ROWID; CREATE TABLE IF NOT EXISTS source_stage_cursors(source TEXT NOT NULL,id TEXT NOT NULL,body TEXT NOT NULL,PRIMARY KEY(source,id)) WITHOUT ROWID;"))?;
        db(tx.execute("DELETE FROM source_stage WHERE source=?1", [source]))?;
        db(tx.execute("DELETE FROM source_stage_cursors WHERE source=?1", [source]))?;
        db(tx.commit())
    }
    pub fn stage_rebuild(
        &mut self,
        source: &str,
        records: &[Record],
        cursor: Option<(&str, &str)>,
    ) -> Result<()> {
        let tx = db(self.connection.transaction())?;
        for r in records {
            if r.source != source {
                return Err(failure("重建来源不一致"));
            }
            let mut r = preserve_existing(&tx, r)?;
            let staged: Option<Vec<u8>> = db(tx
                .query_row(
                    "SELECT body FROM source_stage WHERE source=?1 AND id=?2",
                    params![source, r.id],
                    |row| row.get(0),
                )
                .optional())?;
            if let Some(bytes) = staged {
                r = preserve_record(&r, Some(storage_v12::decode_stage(&bytes)?))?;
            }
            db(tx.execute("INSERT INTO source_stage VALUES(?1,?2,?3) ON CONFLICT(source,id) DO UPDATE SET body=excluded.body", params![source,r.id,storage_v12::encode_stage(&r)?]))?;
        }
        if let Some((id, body)) = cursor {
            db(tx.execute("INSERT INTO source_stage_cursors VALUES(?1,?2,?3) ON CONFLICT(source,id) DO UPDATE SET body=excluded.body",params![source,id,body]))?;
        }
        db(tx.commit())
    }
    pub fn finish_rebuild(&mut self, source: &str) -> Result<()> {
        if storage_v12::pending(&self.connection) {
            return Err(failure("用量存储整理中，请稍后重建来源"));
        }
        let tx = db(self.connection.transaction())?;
        let archived: bool = db(tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM receipts WHERE source=?1)",
            [source],
            |r| r.get(0),
        ))?;
        let cutoff:i64=db(tx.query_row("SELECT coalesce((SELECT cast(value AS INTEGER) FROM metadata WHERE key='detail_since'),0)",[],|r|r.get(0)))?;
        dedup::reset_source_supplements(&tx, source)?;
        if storage_v12::active(&tx) {
            storage_v12::remove_source(&tx, source)?;
        } else {
            db(tx.execute("DELETE FROM usage_metrics WHERE kind IN (0,2) AND owner IN (SELECT id FROM records WHERE source=?1)",[source]))?;
            db(tx.execute("DELETE FROM records WHERE source=?1", [source]))?;
            db(tx.execute("UPDATE records SET effective=1,duplicate_of=NULL WHERE duplicate_of NOT IN (SELECT id FROM records UNION SELECT id FROM receipts)",[]))?;
        }
        let mut after = String::new();
        loop {
            let rows = {
                let mut s=db(tx.prepare("SELECT id,body FROM source_stage WHERE source=?1 AND id>?2 ORDER BY id LIMIT 256"))?;
                let it = db(s.query_map(params![source, after], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?))
                }))?;
                db(it.collect::<rusqlite::Result<Vec<_>>>())?
            };
            if rows.is_empty() {
                break;
            }
            for (_, body) in &rows {
                let r = storage_v12::decode_stage(body)?;
                if matches!(source, "claude" | "antigravity") || !archived || r.started_at >= cutoff
                {
                    insert(&tx, &r)?;
                }
            }
            after = rows.last().unwrap().0.clone();
        }
        // Older cursors have no source tag. The generation invalidates those
        // safely if a missing file returns, without touching another source.
        db(tx.execute(
            "DELETE FROM cursors WHERE CASE WHEN json_valid(body) THEN json_extract(body,'$.source') END=?1",
            [source],
        ))?;
        db(tx.execute("INSERT INTO cursors SELECT id,body FROM source_stage_cursors WHERE source=?1 ON CONFLICT(id) DO UPDATE SET body=excluded.body",[source]))?;
        db(tx.execute("INSERT INTO metadata VALUES(?1,'1') ON CONFLICT(key) DO UPDATE SET value=cast(value AS INTEGER)+1",[format!("source_generation_{source}")]))?;
        db(tx.execute("DELETE FROM source_stage WHERE source=?1", [source]))?;
        db(tx.execute("DELETE FROM source_stage_cursors WHERE source=?1", [source]))?;
        db(tx.execute(
            "DELETE FROM metadata WHERE key=?1",
            [format!("repair_{source}")],
        ))?;
        db(tx.execute(
            "INSERT OR REPLACE INTO metadata VALUES(?1,?2)",
            params![
                format!("historical_{source}"),
                if archived { cutoff } else { 0 }
            ],
        ))?;
        query::changed(&tx)?;
        db(tx.commit())
    }
    #[cfg(test)]
    pub fn rebuild(
        &mut self,
        source: &str,
        records: &[Record],
        cursors: &[(String, String)],
    ) -> Result<()> {
        self.begin_rebuild(source)?;
        for batch in records.chunks(2000) {
            self.stage_rebuild(source, batch, None)?;
        }
        for (id, body) in cursors {
            self.stage_rebuild(source, &[], Some((id, body)))?;
        }
        self.finish_rebuild(source)
    }
}
fn insert(tx: &rusqlite::Transaction<'_>, incoming: &Record) -> Result<bool> {
    if db(tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM receipts WHERE id=?1)",
        [&incoming.id],
        |row| row.get::<_, bool>(0),
    ))? {
        return Ok(false);
    }
    let previous = storage_v12::record(tx, &incoming.id)?;
    let r = preserve_record(incoming, previous.clone())?;
    let mut r = r;
    if r.source != "antigravity" {
        r.finished_at = Some(r.event_time());
    }
    if r.source == "antigravity"
        && previous
            .as_ref()
            .is_some_and(|old| serde_json::to_value(old).ok() == serde_json::to_value(&r).ok())
    {
        return Ok(false);
    }
    dedup::prepare_session(tx, &mut r)?;
    let r = &r;
    let a = r.final_attempt();
    let response = a.and_then(|v| v.response_id.as_deref());
    let signature = r.signature();
    let provider = a.and_then(|v| v.provider.as_deref());
    let model = a.and_then(|v| v.grouping_model());
    let status = a.and_then(|v| v.status);
    if storage_v12::active(tx) {
        db(tx.execute("INSERT INTO metadata(key,value) VALUES('record_sequence','1') ON CONFLICT(key) DO UPDATE SET value=cast(value as integer)+1", []))?;
        let sequence: i64 = db(tx.query_row(
            "SELECT cast(value AS INTEGER) FROM metadata WHERE key='record_sequence'",
            [],
            |r| r.get(0),
        ))?;
        storage_v12::save(tx, r, 1, sequence)?;
    } else {
        let body = bodies::encode(tx, r)?;
        db(tx.execute("INSERT INTO metadata(key,value) VALUES('record_sequence','1') ON CONFLICT(key) DO UPDATE SET value=cast(value as integer)+1", []))?;
        db(tx.execute("INSERT INTO records(id,client,source,time,provider,model,status,response,signature,body,local_sequence) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,(SELECT cast(value as integer) FROM metadata WHERE key='record_sequence')) ON CONFLICT(id) DO UPDATE SET body=excluded.body,time=excluded.time,provider=excluded.provider,model=excluded.model,status=excluded.status,response=excluded.response,signature=excluded.signature,effective=1,duplicate_of=NULL",params![r.id,r.client,r.source,r.event_time(),provider,model,status,response,signature,body]))?;
    }
    query::project(tx, 0, &r.id, r, 1, None)?;
    if let Some(old) = previous {
        if old.signature() != r.signature() {
            dedup::reconcile(tx, &old)?;
        }
    }
    dedup::reconcile(tx, r)?;
    Ok(true)
}

fn preserve_existing(tx: &rusqlite::Transaction<'_>, incoming: &Record) -> Result<Record> {
    let existing = storage_v12::record(tx, &incoming.id)?;
    preserve_record(incoming, existing)
}
pub(super) fn preserve_record(incoming: &Record, existing: Option<Record>) -> Result<Record> {
    let mut r = incoming.clone();
    if let Some(old) = existing {
        if r.source == "antigravity"
            && old.attempts.iter().any(|a| a.transport == "local_api")
            && r.attempts.iter().all(|a| a.transport == "history_file")
        {
            // Transcript summaries do not replace the API's individual retries.
            return Ok(old);
        }
        if r.source != "proxy" {
            if (r.source != "antigravity" && old.completed && !r.completed)
                || old.final_attempt().and_then(|a| a.tokens.output)
                    > r.final_attempt().and_then(|a| a.tokens.output)
            {
                return Ok(old);
            }
            r.started_at = r.started_at.min(old.started_at);
            for (index, a) in r.attempts.iter_mut().enumerate() {
                let previous = if r.source == "antigravity" {
                    old.attempts.iter().find(|b| b.id == a.id)
                } else {
                    old.attempts.get(index)
                };
                let Some(b) = previous else {
                    continue;
                };
                if r.source == "antigravity" && a.provider.is_none() && b.provider.is_some() {
                    a.provider = b.provider.clone();
                }
                a.cost_multiplier = b.cost_multiplier.clone();
                if let Some(price) = &b.price {
                    a.price = if a.tokens == b.tokens {
                        Some(price.clone())
                    } else if price.basis.is_some() {
                        // Late session fragments keep the original unit prices,
                        // version and multiplier, even after automatic updates.
                        Quote {
                            model: price.model.clone(),
                            data: price.basis.clone(),
                            version: price.version.clone(),
                            source: price.source.clone(),
                            multiplier: price.multiplier.clone(),
                        }
                        .calculate(&a.tokens, a.price_tier())
                    } else {
                        Some(price.clone())
                    };
                    a.pricing_model = b.pricing_model.clone();
                    a.pricing_basis = b.pricing_basis.clone();
                    a.mapping_revision = b.mapping_revision.clone();
                }
            }
        }
    }
    for attempt in &mut r.attempts {
        attempt.annotate_availability(&r.source);
    }
    Ok(r)
}

#[cfg(test)]
#[path = "query_plan_tests.rs"]
mod query_plan_tests;
