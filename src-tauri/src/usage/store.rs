use super::{
    dedup,
    model::*,
    pricing::{Pricing, Quote},
    query,
};
use crate::storage::{self, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::Path,
    sync::{Arc, Mutex},
};
const DAY: i64 = 86_400_000;
fn db<T>(r: rusqlite::Result<T>) -> Result<T> {
    r.map_err(|_| failure("用量数据库操作失败"))
}
pub struct Store {
    connection: Connection,
    overview_cache: OverviewCache,
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
impl Store {
    pub fn open(dir: &Path) -> Result<Self> {
        storage::private_dir(dir)?;
        let path = dir.join("usage.sqlite");
        if std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(failure("用量数据库路径无效"));
        }
        let mut connection = db(Connection::open(&path))?;
        storage::protect(&path, false)?;
        let version: i64 = db(connection.query_row("PRAGMA user_version", [], |row| row.get(0)))?;
        if version > 7 {
            return Err(failure("用量数据库来自更高版本，已保留原文件"));
        }
        db(connection.busy_timeout(std::time::Duration::from_secs(3)))?;
        db(connection.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;
          CREATE TABLE IF NOT EXISTS records(id TEXT PRIMARY KEY,client TEXT NOT NULL,source TEXT NOT NULL,time INTEGER NOT NULL,provider TEXT,model TEXT,status INTEGER,response TEXT,signature TEXT,effective INTEGER NOT NULL DEFAULT 1,duplicate_of TEXT,body TEXT NOT NULL);
          CREATE INDEX IF NOT EXISTS usage_time ON records(time,effective);
          CREATE INDEX IF NOT EXISTS usage_response ON records(client,response);
          CREATE INDEX IF NOT EXISTS usage_signature ON records(client,signature,time);
          CREATE TABLE IF NOT EXISTS cursors(id TEXT PRIMARY KEY,body TEXT NOT NULL);
          CREATE TABLE IF NOT EXISTS daily(id TEXT PRIMARY KEY,day INTEGER NOT NULL,body TEXT NOT NULL);
          CREATE TABLE IF NOT EXISTS metadata(key TEXT PRIMARY KEY,value TEXT NOT NULL);
          CREATE TABLE IF NOT EXISTS receipts(id TEXT PRIMARY KEY,client TEXT NOT NULL,source TEXT NOT NULL,time INTEGER NOT NULL,provider TEXT,response TEXT,signature TEXT,ended INTEGER NOT NULL,effective INTEGER NOT NULL);
          CREATE INDEX IF NOT EXISTS receipt_response ON receipts(client,response);
          CREATE INDEX IF NOT EXISTS receipt_signature ON receipts(client,signature,ended);
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
                    let record: Record = serde_json::from_str(&body)
                        .map_err(|_| failure("旧用量记录无效，已保留原数据库"))?;
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
                let daily: Daily = serde_json::from_str(&body)
                    .map_err(|_| failure("旧汇总无效，已保留原数据库"))?;
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
        for suffix in ["-wal", "-shm"] {
            let file = dir.join(format!("usage.sqlite{suffix}"));
            if file.exists() {
                storage::protect(&file, false)?;
            }
        }
        Ok(Self {
            connection,
            overview_cache: Default::default(),
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
        })
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
    pub fn needs_repair(&self, source: &str) -> Result<bool> {
        db(self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM metadata WHERE key=?1 AND value='1')",
            [format!("repair_{source}")],
            |r| r.get(0),
        ))
    }
    pub fn source_counts(&self, source: &str) -> Result<(u64, u64, i64)> {
        let merged = db(self.connection.query_row(
            "SELECT count(*) FROM records WHERE source=?1 AND effective=0",
            [source],
            |r| r.get(0),
        ))?;
        let pending = db(self.connection.query_row(
            "SELECT count(*) FROM records WHERE source=?1 AND effective=2",
            [source],
            |r| r.get(0),
        ))?;
        let historical = db(self.connection.query_row(
            "SELECT coalesce((SELECT cast(value as integer) FROM metadata WHERE key=?1),0)",
            [format!("historical_{source}")],
            |r| r.get(0),
        ))?;
        Ok((merged, pending, historical))
    }
    pub fn cursor(&self, id: &str) -> Result<Option<String>> {
        db(self
            .connection
            .query_row("SELECT body FROM cursors WHERE id=?1", [id], |r| r.get(0))
            .optional())
    }
    pub fn write_batch(&mut self, records: &[Record], cursor: Option<(&str, &str)>) -> Result<()> {
        let tx = db(self.connection.transaction())?;
        for r in records {
            insert(&tx, r)?;
        }
        if let Some((id, body)) = cursor {
            db(tx.execute("INSERT INTO cursors(id,body) VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET body=excluded.body",params![id,body]))?;
        }
        if !records.is_empty() {
            query::changed(&tx)?;
        }
        db(tx.commit())
    }
    pub fn detail(&self, id: &str) -> Result<Record> {
        let body: String =
            db(self
                .connection
                .query_row("SELECT body FROM records WHERE id=?1", [id], |r| r.get(0)))?;
        serde_json::from_str(&body).map_err(|_| failure("记录无效"))
    }
    fn records(&self, f: &Filter, with_status: bool) -> Result<Vec<Record>> {
        let (condition, args) = query::conditions(f, "r", with_status);
        let eligible = query::eligible(f, "r", false);
        let mut statement = db(self.connection.prepare(&format!("SELECT body,duplicate_of FROM records r WHERE {eligible} AND {condition} ORDER BY time DESC,id DESC")))?;
        let rows = db(
            statement.query_map(rusqlite::params_from_iter(args.iter()), |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
            }),
        )?;
        let mut out = Vec::new();
        for row in rows {
            let (body, duplicate) = db(row)?;
            let mut r: Record = serde_json::from_str(&body).map_err(|_| failure("记录无效"))?;
            r.duplicate_of = duplicate;
            if f.source.as_deref() == Some("proxy") {
                r.gateway_only();
            }
            if !with_status
                || status_matches(
                    r.final_attempt().and_then(|a| a.status),
                    f.status.as_deref(),
                )
            {
                out.push(r);
            }
        }
        Ok(out)
    }
    pub fn logs(&self, f: &Filter) -> Result<Page> {
        let (condition, args) = query::conditions(f, "r", true);
        // Ambiguous records remain inspectable, but are excluded from aggregates.
        let eligible = query::eligible(f, "r", true);
        let where_sql = format!("{eligible} AND {condition}");
        let total: u64 = db(self.connection.query_row(
            &format!("SELECT count(*) FROM records r WHERE {where_sql}"),
            rusqlite::params_from_iter(args.iter()),
            |r| r.get(0),
        ))?;
        let page = f.page.max(1).min((total.div_ceil(20) as u32).max(1));
        let mut stmt=db(self.connection.prepare(&format!("SELECT body,duplicate_of FROM records r WHERE {where_sql} ORDER BY time DESC,id DESC LIMIT 20 OFFSET {}",(page-1)*20)))?;
        let rows = db(
            stmt.query_map(rusqlite::params_from_iter(args.iter()), |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
            }),
        )?;
        let mut records = Vec::new();
        for row in rows {
            let (body, duplicate) = db(row)?;
            let mut r: Record = serde_json::from_str(&body).map_err(|_| failure("用量记录无效"))?;
            r.duplicate_of = duplicate;
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
    pub fn dashboard(&self, f: &Filter) -> Result<Dashboard> {
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
    pub fn compact(&mut self, at: i64) -> Result<()> {
        let cutoff = query::bucket(at - 30 * DAY, DAY);
        if cutoff <= self.detail_since()?
            && !db(self.connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM records WHERE time<?1)",
                [cutoff],
                |r| r.get::<_, bool>(0),
            ))?
        {
            return Ok(());
        }
        let mut records = Vec::new();
        for scope in [None, Some("proxy"), Some("sessions")] {
            let selected = self.records(
                &Filter {
                    end: Some(cutoff - 1),
                    source: scope.map(str::to_owned),
                    ..Filter::default()
                },
                false,
            )?;
            records.extend(selected.into_iter().map(|r| (scope.map(str::to_owned), r)));
        }
        let tx = db(self.connection.transaction())?;
        for (scope, mut r) in records {
            let day = query::bucket(r.started_at, DAY);
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
            r.duplicate_of = None;
            r.gateway_reported = None;
            for a in &mut r.attempts {
                a.id.clear();
                a.response_id = None;
                a.started_at = day;
                a.duration_ms = 0;
                a.first_token_ms = None;
            }
            // Exact token/tier/price dimensions are retained for safe future backfill.
            let key =
                safe_id(&serde_json::to_string(&(&scope, &r)).map_err(|_| failure("汇总失败"))?);
            let old: Option<String> = db(tx
                .query_row("SELECT body FROM daily WHERE id=?1", [&key], |row| {
                    row.get(0)
                })
                .optional())?;
            let mut d = if let Some(body) = old {
                serde_json::from_str::<Daily>(&body).map_err(|_| failure("汇总无效"))?
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
            db(tx.execute("INSERT INTO daily(id,day,body) VALUES(?1,?2,?3) ON CONFLICT(id) DO UPDATE SET body=excluded.body",params![key,day,serde_json::to_string(&d).map_err(|_|failure("汇总失败"))?]))?;
            query::project(&tx, 1, &key, &d.example, d.totals.requests, Some(&d.totals))?;
        }
        // Keep only irreversible correlation keys after detail expiry. They prevent
        // archived files, late imports and source rebuilds from duplicating daily totals.
        db(tx.execute("INSERT OR IGNORE INTO receipts(id,client,source,time,provider,response,signature,ended,effective,operation) SELECT id,client,source,time,provider,response,signature,time+coalesce(json_extract(body,'$.attempts[#-1].durationMs'),0),effective,coalesce(json_extract(body,'$.attempts[#-1].operation'),'model') FROM records WHERE time<?1",[cutoff]))?;
        db(tx.execute("DELETE FROM usage_metrics WHERE kind IN (0,2) AND owner IN (SELECT id FROM records WHERE time<?1)",[cutoff]))?;
        db(tx.execute("DELETE FROM records WHERE time<?1", [cutoff]))?;
        query::changed(&tx)?;
        db(tx.execute("INSERT INTO metadata(key,value) VALUES('detail_since',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",[cutoff.to_string()]))?;
        db(tx.commit())
    }
    pub fn backfill(&mut self, pricing: &Pricing, _multiplier: &str) -> Result<()> {
        let mut stmt = db(self.connection.prepare("SELECT body FROM records WHERE EXISTS(SELECT 1 FROM json_each(body,'$.attempts') WHERE json_extract(value,'$.price') IS NULL)"))?;
        let all = db(stmt.query_map([], |r| r.get::<_, String>(0)))?
            .collect::<rusqlite::Result<Vec<_>>>();
        let all = db(all)?;
        drop(stmt);
        let tx = db(self.connection.transaction())?;
        let mut any_changed = false;
        for body in all {
            let mut r: Record = serde_json::from_str(&body).map_err(|_| failure("记录无效"))?;
            let mut changed = false;
            for a in &mut r.attempts {
                if a.price.is_none() && a.compacted_unpriced.is_none() {
                    a.price = a.calculate_price(pricing);
                    changed |= a.price.is_some();
                }
            }
            if changed {
                any_changed = true;
                query::project(&tx, 0, &r.id, &r, 1, None)?;
                db(tx.execute(
                    "UPDATE records SET body=?2 WHERE id=?1",
                    params![
                        r.id,
                        serde_json::to_string(&r).map_err(|_| failure("记录无效"))?
                    ],
                ))?;
            }
        }
        let mut stmt = db(
            tx.prepare("SELECT id,body FROM daily WHERE json_extract(body,'$.totals.unpriced')>0")
        )?;
        let daily =
            db(stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))))?
                .collect::<rusqlite::Result<Vec<_>>>();
        let daily = db(daily)?;
        drop(stmt);
        for (id, body) in daily {
            let mut d: Daily = serde_json::from_str(&body).map_err(|_| failure("汇总无效"))?;
            if d.totals.unpriced == 0 {
                continue;
            }
            let before = serde_json::to_string(&d.example).map_err(|_| failure("汇总无效"))?;
            for a in &mut d.example.attempts {
                if a.price.is_none() && a.compacted_unpriced.is_none() {
                    a.price = a.calculate_price(pricing);
                }
            }
            if serde_json::to_string(&d.example).map_err(|_| failure("汇总无效"))? == before {
                continue;
            }
            any_changed = true;
            let known = d
                .example
                .attempts
                .iter()
                .filter_map(|a| a.price.as_ref().and_then(|p| decimal(&p.cost)))
                .fold(rust_decimal::Decimal::ZERO, |sum, cost| sum + cost);
            d.totals.cost = (known * rust_decimal::Decimal::from(d.totals.requests)).to_string();
            if d.example.cost().is_some() {
                d.totals.unpriced = 0;
            }
            query::project(&tx, 1, &id, &d.example, d.totals.requests, Some(&d.totals))?;
            db(tx.execute(
                "UPDATE daily SET body=?2 WHERE id=?1",
                params![
                    id,
                    serde_json::to_string(&d).map_err(|_| failure("汇总无效"))?
                ],
            ))?;
        }
        if any_changed {
            query::changed(&tx)?;
        }
        db(tx.commit())
    }
    pub fn rebuild(
        &mut self,
        source: &str,
        records: &[Record],
        cursors: &[(String, String)],
    ) -> Result<()> {
        if !matches!(source, "codex" | "claude") {
            return Err(failure("只可重建会话来源"));
        }
        let tx = db(self.connection.transaction())?;
        // Staging is validated in the same transaction; an error leaves all old rows intact.
        db(tx.execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS rebuilding(body TEXT); DELETE FROM rebuilding;",
        ))?;
        let archived: bool = db(tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM receipts WHERE source=?1)",
            [source],
            |r| r.get(0),
        ))?;
        let cutoff: i64 = db(tx.query_row("SELECT coalesce((SELECT cast(value as integer) FROM metadata WHERE key='detail_since'),0)",[],|r|r.get(0)))?;
        // Old daily data no longer has per-message price snapshots. Preserve it
        // verbatim rather than pretending that current prices recreate history.
        let records = records
            .iter()
            .filter(|r| !archived || r.started_at >= cutoff)
            .map(|r| preserve_existing(&tx, r))
            .collect::<Result<Vec<_>>>()?;
        db(tx.execute(
            "INSERT OR REPLACE INTO metadata(key,value) VALUES(?1,?2)",
            params![
                format!("historical_{source}"),
                if archived { cutoff } else { 0 }
            ],
        ))?;
        for r in &records {
            if r.source != source {
                return Err(failure("重建来源不一致"));
            }
            db(tx.execute(
                "INSERT INTO rebuilding(body) VALUES(?1)",
                [serde_json::to_string(r).map_err(|_| failure("重建记录无效"))?],
            ))?;
        }
        dedup::reset_source_supplements(&tx, source)?;
        db(tx.execute("DELETE FROM usage_metrics WHERE kind IN (0,2) AND owner IN (SELECT id FROM records WHERE source=?1)",[source]))?;
        db(tx.execute("DELETE FROM records WHERE source=?1", [source]))?;
        db(tx.execute("UPDATE records SET effective=1,duplicate_of=NULL WHERE duplicate_of NOT IN (SELECT id FROM records UNION SELECT id FROM receipts)",[]))?;
        for r in &records {
            insert(&tx, r)?;
        }
        for (id, body) in cursors {
            db(tx.execute("INSERT INTO cursors(id,body) VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET body=excluded.body",params![id,body]))?;
        }
        db(tx.execute_batch("DROP TABLE rebuilding;"))?;
        db(tx.execute(
            "DELETE FROM metadata WHERE key=?1",
            [format!("repair_{source}")],
        ))?;
        query::changed(&tx)?;
        db(tx.commit())
    }
}
fn status_matches(status: Option<u16>, filter: Option<&str>) -> bool {
    match filter {
        None | Some("") => true,
        Some("none") => status.is_none(),
        Some("2xx") => status.is_some_and(|s| (200..300).contains(&s)),
        Some("4xx") => status.is_some_and(|s| (400..500).contains(&s)),
        Some("5xx") => status.is_some_and(|s| s >= 500),
        Some(n) => n.parse::<u16>().ok() == status,
    }
}
fn insert(tx: &rusqlite::Transaction<'_>, incoming: &Record) -> Result<()> {
    if db(tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM receipts WHERE id=?1)",
        [&incoming.id],
        |row| row.get::<_, bool>(0),
    ))? {
        return Ok(());
    }
    let previous: Option<String> = db(tx
        .query_row(
            "SELECT body FROM records WHERE id=?1",
            [&incoming.id],
            |r| r.get(0),
        )
        .optional())?;
    let r = preserve_existing(tx, incoming)?;
    let r = &r;
    let a = r.final_attempt();
    let response = a.and_then(|v| v.response_id.as_deref());
    let signature = r.signature();
    let provider = a.and_then(|v| v.provider.as_deref());
    let model = a.and_then(|v| v.grouping_model());
    let status = a.and_then(|v| v.status);
    let body = serde_json::to_string(r).map_err(|_| failure("用量元数据无效"))?;
    db(tx.execute("INSERT INTO records(id,client,source,time,provider,model,status,response,signature,body) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10) ON CONFLICT(id) DO UPDATE SET body=excluded.body,time=excluded.time,provider=excluded.provider,model=excluded.model,status=excluded.status,response=excluded.response,signature=excluded.signature,effective=1,duplicate_of=NULL",params![r.id,r.client,r.source,r.started_at,provider,model,status,response,signature,body]))?;
    query::project(tx, 0, &r.id, r, 1, None)?;
    if let Some(previous) = previous {
        let old: Record = serde_json::from_str(&previous).map_err(|_| failure("旧用量记录无效"))?;
        if old.signature() != r.signature() {
            dedup::reconcile(tx, &old)?;
        }
    }
    dedup::reconcile(tx, r)?;
    Ok(())
}

fn preserve_existing(tx: &rusqlite::Transaction<'_>, incoming: &Record) -> Result<Record> {
    let existing: Option<String> = db(tx
        .query_row(
            "SELECT body FROM records WHERE id=?1",
            [&incoming.id],
            |row| row.get(0),
        )
        .optional())?;
    let mut r = incoming.clone();
    if let Some(body) = existing {
        let old: Record = serde_json::from_str(&body).map_err(|_| failure("原用量记录无效"))?;
        if r.source != "proxy" {
            if (old.completed && !r.completed)
                || old.final_attempt().and_then(|a| a.tokens.output)
                    > r.final_attempt().and_then(|a| a.tokens.output)
            {
                return Ok(old);
            }
            r.started_at = r.started_at.min(old.started_at);
            for (a, b) in r.attempts.iter_mut().zip(&old.attempts) {
                a.cost_multiplier = b.cost_multiplier.clone();
                if let Some(price) = &b.price {
                    if a.tokens == b.tokens {
                        a.price = Some(price.clone());
                    } else if price.basis.is_some() {
                        a.price = Quote {
                            model: price.model.clone(),
                            data: price.basis.clone(),
                            version: price.version.clone(),
                            source: price.source.clone(),
                            multiplier: price.multiplier.clone(),
                        }
                        .calculate(&a.tokens, a.service_tier.as_deref());
                    } else {
                        a.price = None;
                    }
                }
            }
        }
    }
    Ok(r)
}
