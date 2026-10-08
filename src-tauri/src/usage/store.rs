use super::{
    model::*,
    pricing::{Pricing, Quote},
};
use crate::storage::{self, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::Path};
const DAY: i64 = 86_400_000;
fn db<T>(r: rusqlite::Result<T>) -> Result<T> {
    r.map_err(|_| failure("用量数据库操作失败"))
}
pub struct Store {
    connection: Connection,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Page {
    pub rows: Vec<Record>,
    pub total: u64,
    pub page: u32,
    pub detail_since: i64,
}
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Point {
    pub time: i64,
    pub totals: Totals,
}
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Group {
    pub id: String,
    pub totals: Totals,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Dashboard {
    pub totals: Totals,
    pub trend: Vec<Point>,
    pub trend_step_ms: i64,
    pub heatmap: Vec<Point>,
    pub providers: Vec<Group>,
    pub models: Vec<Group>,
    pub precision: String,
    pub detail_since: i64,
    pub sources: BTreeMap<String, u64>,
}
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Daily {
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
        let connection = db(Connection::open(&path))?;
        storage::protect(&path, false)?;
        let version: i64 = db(connection.query_row("PRAGMA user_version", [], |row| row.get(0)))?;
        if version > 2 {
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
          PRAGMA user_version=2;"))?;
        for suffix in ["-wal", "-shm"] {
            let file = dir.join(format!("usage.sqlite{suffix}"));
            if file.exists() {
                storage::protect(&file, false)?;
            }
        }
        Ok(Self { connection })
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
        let mut statement=db(self.connection.prepare("SELECT body,duplicate_of FROM records WHERE effective=1 AND time>=?1 AND time<=?2 AND (?3 IS NULL OR client=?3) AND (?4 IS NULL OR provider=?4) AND (?5 IS NULL OR model=?5) ORDER BY time DESC,id DESC"))?;
        let rows = db(statement.query_map(
            params![
                f.start.unwrap_or(0),
                f.end.unwrap_or(i64::MAX),
                f.client,
                f.provider,
                f.model
            ],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)),
        ))?;
        let mut out = Vec::new();
        for row in rows {
            let (body, duplicate) = db(row)?;
            let mut r: Record = serde_json::from_str(&body).map_err(|_| failure("记录无效"))?;
            r.duplicate_of = duplicate;
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
        let records = self.records(f, true)?;
        let total = records.len() as u64;
        let page = f.page.max(1).min((total.div_ceil(20) as u32).max(1));
        Ok(Page {
            rows: records
                .into_iter()
                .skip((page as usize - 1) * 20)
                .take(20)
                .collect(),
            total,
            page,
            detail_since: self.detail_since()?,
        })
    }
    pub fn dashboard(&self, f: &Filter) -> Result<Dashboard> {
        let records = self.records(f, false)?;
        let end = f.end.unwrap_or_else(now);
        let start = f
            .start
            .unwrap_or_else(|| records.last().map(|r| r.started_at).unwrap_or(end));
        let step = if end - start <= 2 * DAY {
            3_600_000
        } else {
            DAY
        };
        let mut totals = Totals::default();
        let mut trend = BTreeMap::<i64, Totals>::new();
        let mut heatmap = BTreeMap::<i64, Totals>::new();
        let mut providers = BTreeMap::<String, Totals>::new();
        let mut models = BTreeMap::<String, Totals>::new();
        let mut sources = BTreeMap::new();
        for r in records {
            let Some(a) = r.final_attempt() else {
                continue;
            };
            let mut t = Totals::default();
            t.add_record(&r);
            totals.add(&t);
            trend.entry(r.started_at / step * step).or_default().add(&t);
            heatmap.entry(r.started_at / DAY * DAY).or_default().add(&t);
            providers
                .entry(a.provider.clone().unwrap_or_else(|| "session".into()))
                .or_default()
                .add(&t);
            models
                .entry(a.pricing_model.clone().unwrap_or_else(|| "unknown".into()))
                .or_default()
                .add(&t);
            *sources.entry(r.source.clone()).or_insert(0) += 1;
        }
        let mut statement = db(self
            .connection
            .prepare("SELECT day,body FROM daily WHERE day>=?1 AND day+86400000<=?2"))?;
        let rows = db(statement
            .query_map(params![f.start.unwrap_or(0), end.saturating_add(1)], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            }))?;
        let mut rolled = false;
        for row in rows {
            let (day, body) = db(row)?;
            let d: Daily = serde_json::from_str(&body).map_err(|_| failure("日汇总无效"))?;
            if f.client.as_ref().is_some_and(|v| v != &d.client)
                || f.provider
                    .as_ref()
                    .is_some_and(|v| Some(v) != d.provider.as_ref())
                || f.model
                    .as_ref()
                    .is_some_and(|v| Some(v) != d.model.as_ref())
            {
                continue;
            }
            rolled = true;
            totals.add(&d.totals);
            trend.entry(day).or_default().add(&d.totals);
            heatmap.entry(day).or_default().add(&d.totals);
            providers
                .entry(d.provider.unwrap_or_else(|| "session".into()))
                .or_default()
                .add(&d.totals);
            models
                .entry(d.model.unwrap_or_else(|| "unknown".into()))
                .or_default()
                .add(&d.totals);
            *sources.entry(d.source).or_insert(0) += d.totals.requests;
        }
        Ok(Dashboard {
            totals,
            trend_step_ms: if rolled { DAY } else { step },
            trend: trend
                .into_iter()
                .map(|(time, totals)| Point { time, totals })
                .collect(),
            heatmap: heatmap
                .into_iter()
                .map(|(time, totals)| Point { time, totals })
                .collect(),
            providers: groups(providers),
            models: groups(models),
            precision: if rolled { "day" } else { "millisecond" }.into(),
            detail_since: self.detail_since()?,
            sources,
        })
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
        let cutoff = (at - 30 * DAY) / DAY * DAY;
        let records = self.records(
            &Filter {
                end: Some(cutoff - 1),
                ..Filter::default()
            },
            false,
        )?;
        let tx = db(self.connection.transaction())?;
        for mut r in records {
            let day = r.started_at / DAY * DAY;
            let Some(a) = r.final_attempt() else {
                continue;
            };
            let provider = a.provider.clone();
            let model = a.pricing_model.clone();
            let status = a.status;
            let mut t = Totals::default();
            t.add_record(&r);
            r.id.clear();
            r.session_id = None;
            r.started_at = day;
            r.duplicate_of = None;
            for a in &mut r.attempts {
                a.id.clear();
                a.response_id = None;
                a.started_at = day;
                a.duration_ms = 0;
                a.first_token_ms = None;
            }
            // Exact token/tier/price dimensions are retained for safe future backfill.
            let key = safe_id(&serde_json::to_string(&r).map_err(|_| failure("汇总失败"))?);
            let old: Option<String> = db(tx
                .query_row("SELECT body FROM daily WHERE id=?1", [&key], |row| {
                    row.get(0)
                })
                .optional())?;
            let mut d = if let Some(body) = old {
                serde_json::from_str::<Daily>(&body).map_err(|_| failure("汇总无效"))?
            } else {
                Daily {
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
        }
        // Keep only irreversible correlation keys after detail expiry. They prevent
        // archived files, late imports and source rebuilds from duplicating daily totals.
        db(tx.execute("INSERT OR IGNORE INTO receipts(id,client,source,time,provider,response,signature,ended,effective) SELECT id,client,source,time,provider,response,signature,time+coalesce(json_extract(body,'$.attempts[#-1].durationMs'),0),effective FROM records WHERE time<?1",[cutoff]))?;
        db(tx.execute("DELETE FROM records WHERE time<?1", [cutoff]))?;
        db(tx.execute("INSERT INTO metadata(key,value) VALUES('detail_since',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",[cutoff.to_string()]))?;
        db(tx.commit())
    }
    pub fn backfill(&mut self, pricing: &Pricing, _multiplier: &str) -> Result<()> {
        let mut stmt = db(self.connection.prepare("SELECT body FROM records"))?;
        let all = db(stmt.query_map([], |r| r.get::<_, String>(0)))?
            .collect::<rusqlite::Result<Vec<_>>>();
        let all = db(all)?;
        drop(stmt);
        let tx = db(self.connection.transaction())?;
        for body in all {
            let mut r: Record = serde_json::from_str(&body).map_err(|_| failure("记录无效"))?;
            let mut changed = false;
            for a in &mut r.attempts {
                if a.price.is_none() {
                    a.price = pricing
                        .quote(a.pricing_model.as_deref(), &a.cost_multiplier)
                        .calculate(&a.tokens, a.service_tier.as_deref());
                    changed |= a.price.is_some();
                }
            }
            if changed {
                db(tx.execute(
                    "UPDATE records SET body=?2 WHERE id=?1",
                    params![
                        r.id,
                        serde_json::to_string(&r).map_err(|_| failure("记录无效"))?
                    ],
                ))?;
            }
        }
        let mut stmt = db(tx.prepare("SELECT id,body FROM daily"))?;
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
            for a in &mut d.example.attempts {
                if a.price.is_none() {
                    a.price = pricing
                        .quote(a.pricing_model.as_deref(), &a.cost_multiplier)
                        .calculate(&a.tokens, a.service_tier.as_deref());
                }
            }
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
            db(tx.execute(
                "UPDATE daily SET body=?2 WHERE id=?1",
                params![
                    id,
                    serde_json::to_string(&d).map_err(|_| failure("汇总无效"))?
                ],
            ))?;
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
        for r in records {
            if r.source != source {
                return Err(failure("重建来源不一致"));
            }
            db(tx.execute(
                "INSERT INTO rebuilding(body) VALUES(?1)",
                [serde_json::to_string(r).map_err(|_| failure("重建记录无效"))?],
            ))?;
        }
        db(tx.execute("DELETE FROM records WHERE source=?1", [source]))?;
        db(tx.execute("DELETE FROM receipts WHERE source=?1", [source]))?;
        db(tx.execute(
            "DELETE FROM daily WHERE json_extract(body,'$.source')=?1",
            [source],
        ))?;
        db(tx.execute("UPDATE records SET effective=1,duplicate_of=NULL WHERE duplicate_of NOT IN (SELECT id FROM records)",[]))?;
        for r in records {
            insert(&tx, r)?;
        }
        for (id, body) in cursors {
            db(tx.execute("INSERT INTO cursors(id,body) VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET body=excluded.body",params![id,body]))?;
        }
        db(tx.execute_batch("DROP TABLE rebuilding;"))?;
        db(tx.commit())
    }
}
fn groups(map: BTreeMap<String, Totals>) -> Vec<Group> {
    let mut out: Vec<_> = map
        .into_iter()
        .map(|(id, totals)| Group { id, totals })
        .collect();
    out.sort_by(|a, b| b.totals.requests.cmp(&a.totals.requests));
    out
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
                return Ok(());
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
    let r = &r;
    let a = r.final_attempt();
    let response = a.and_then(|v| v.response_id.as_deref());
    let signature = r.signature();
    let provider = a.and_then(|v| v.provider.as_deref());
    let model = a.and_then(|v| v.pricing_model.as_deref());
    let status = a.and_then(|v| v.status);
    let body = serde_json::to_string(r).map_err(|_| failure("用量元数据无效"))?;
    db(tx.execute("INSERT INTO records(id,client,source,time,provider,model,status,response,signature,body) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10) ON CONFLICT(id) DO UPDATE SET body=excluded.body,time=excluded.time,provider=excluded.provider,model=excluded.model,status=excluded.status,response=excluded.response,signature=excluded.signature,effective=1,duplicate_of=NULL",params![r.id,r.client,r.source,r.started_at,provider,model,status,response,signature,body]))?;
    let mut archived_stmt=db(tx.prepare("SELECT id,response FROM receipts WHERE client=?1 AND effective=1 AND ((?2 IS NOT NULL AND response=?2 AND (provider IS NULL OR ?3 IS NULL OR provider=?3)) OR (source<>?4 AND (response IS NULL OR ?2 IS NULL) AND ?5 IS NOT NULL AND signature=?5 AND abs(ended-?6)<=2000))"))?;
    let archived = db(archived_stmt.query_map(
        params![
            r.client,
            response,
            provider,
            r.source,
            signature,
            r.started_at
                .saturating_add(a.map(|a| a.duration_ms as i64).unwrap_or(0))
        ],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
    ))?
    .collect::<rusqlite::Result<Vec<_>>>();
    let archived = db(archived)?;
    drop(archived_stmt);
    let archived_match = archived
        .iter()
        .find(|(_, key)| response.is_some() && key.as_deref() == response)
        .or_else(|| (archived.len() == 1).then(|| &archived[0]));
    if let Some((id, _)) = archived_match {
        db(tx.execute(
            "UPDATE records SET effective=0,duplicate_of=?2 WHERE id=?1",
            params![r.id, id],
        ))?;
        return Ok(());
    }
    let mut stmt=db(tx.prepare("SELECT id,source,response FROM records WHERE id<>?1 AND client=?2 AND effective=1 AND ((?3 IS NOT NULL AND response=?3 AND (provider IS NULL OR ?4 IS NULL OR provider=?4)) OR (source<>?5 AND (response IS NULL OR ?3 IS NULL) AND ?6 IS NOT NULL AND signature=?6 AND abs(time+coalesce(json_extract(body,'$.attempts[#-1].durationMs'),0)-?7)<=2000))"))?;
    let candidates = db(stmt.query_map(
        params![
            r.id,
            r.client,
            response,
            provider,
            r.source,
            signature,
            r.started_at
                .saturating_add(a.map(|a| a.duration_ms as i64).unwrap_or(0))
        ],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        },
    ))?
    .collect::<rusqlite::Result<Vec<_>>>();
    let candidates = db(candidates)?;
    drop(stmt);
    let exact: Vec<_> = candidates
        .iter()
        .filter(|(_, _, id)| response.is_some() && id.as_deref() == response)
        .collect();
    let matches = if exact.is_empty() {
        if candidates.len() == 1 {
            candidates.iter().collect()
        } else {
            Vec::new()
        }
    } else {
        exact
    };
    for (id, source, key) in matches {
        let rule = if response.is_some() && key.as_deref() == response {
            "response_id"
        } else {
            "strict_match"
        };
        if r.source == "proxy" && source != "proxy" {
            db(tx.execute(
                "UPDATE records SET effective=0,duplicate_of=?2 WHERE id=?1",
                params![id, r.id],
            ))?;
            mark_merged(tx, &r.id, source, rule)?;
        } else {
            db(tx.execute(
                "UPDATE records SET effective=0,duplicate_of=?2 WHERE id=?1",
                params![r.id, id],
            ))?;
            mark_merged(tx, id, &r.source, rule)?;
            break;
        }
    }
    Ok(())
}

fn mark_merged(tx: &rusqlite::Transaction<'_>, id: &str, source: &str, rule: &str) -> Result<()> {
    let body: String = db(
        tx.query_row("SELECT body FROM records WHERE id=?1", [id], |row| {
            row.get(0)
        }),
    )?;
    let mut record: Record = serde_json::from_str(&body).map_err(|_| failure("合并记录无效"))?;
    record.deduplication = rule.into();
    for item in [record.source.clone(), source.to_string()] {
        if !record.merged_sources.contains(&item) {
            record.merged_sources.push(item);
        }
    }
    db(tx.execute(
        "UPDATE records SET body=?2 WHERE id=?1",
        params![
            id,
            serde_json::to_string(&record).map_err(|_| failure("合并记录无效"))?
        ],
    ))?;
    Ok(())
}
