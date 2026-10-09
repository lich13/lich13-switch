//! Indexed projections for queries. JSON bodies are read only for one detail/page.
use super::{
    model::*,
    store::{Dashboard, Group},
};
use crate::storage::Result;
use chrono::{Datelike, TimeZone};
use rusqlite::{
    functions::{Aggregate, Context, FunctionFlags},
    params,
    types::Value,
    Connection, Row,
};
use rust_decimal::Decimal;
use std::collections::BTreeMap;
const DAY: i64 = 86_400_000;
fn db<T>(value: rusqlite::Result<T>) -> Result<T> {
    value.map_err(|_| failure("用量查询失败"))
}
struct DecimalSum;
impl Aggregate<Decimal, String> for DecimalSum {
    fn init(&self, _: &mut Context<'_>) -> rusqlite::Result<Decimal> {
        Ok(Decimal::ZERO)
    }
    fn step(&self, ctx: &mut Context<'_>, total: &mut Decimal) -> rusqlite::Result<()> {
        // Borrow SQLite's text. Aggregating each panel must not allocate one
        // temporary String for every attempt (including the common zero cost).
        if let Some(value) = ctx
            .get_raw(0)
            .as_str()
            .ok()
            .filter(|v| *v != "0")
            .and_then(decimal)
        {
            *total = total
                .checked_add(value)
                .ok_or(rusqlite::Error::InvalidQuery)?;
        }
        Ok(())
    }
    fn finalize(&self, _: &mut Context<'_>, total: Option<Decimal>) -> rusqlite::Result<String> {
        let total = total.unwrap_or_default();
        Ok(if total.is_zero() {
            "0".into()
        } else {
            total.to_string()
        })
    }
}
pub fn bucket(at: i64, step: i64) -> i64 {
    let Some(local) = chrono::Local.timestamp_millis_opt(at).single() else {
        return at.div_euclid(step) * step;
    };
    if step == DAY {
        return chrono::Local
            .with_ymd_and_hms(local.year(), local.month(), local.day(), 0, 0, 0)
            .earliest()
            .map(|t| t.timestamp_millis())
            .unwrap_or(at);
    }
    let offset = i64::from(local.offset().local_minus_utc()) * 1000;
    (at + offset).div_euclid(step) * step - offset
}
pub fn register(c: &Connection) -> Result<()> {
    db(c.create_aggregate_function(
        "decimal_sum",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        DecimalSum,
    ))?;
    db(
        c.create_scalar_function("usage_day_end", 1, FunctionFlags::SQLITE_UTF8, |ctx| {
            let at: i64 = ctx.get(0)?;
            let next = chrono::Local
                .timestamp_millis_opt(at)
                .single()
                .and_then(|v| v.date_naive().succ_opt())
                .and_then(|date| date.and_hms_opt(0, 0, 0))
                .and_then(|t| chrono::Local.from_local_datetime(&t).earliest())
                .map(|t| t.timestamp_millis())
                .unwrap_or(at.saturating_add(DAY));
            Ok(next)
        }),
    )
}
pub fn schema(c: &Connection) -> Result<()> {
    db(c.execute_batch("CREATE TABLE IF NOT EXISTS usage_metrics(
      kind INTEGER NOT NULL,owner TEXT NOT NULL,ordinal INTEGER NOT NULL,
      provider TEXT,model TEXT,requests INTEGER NOT NULL,attempts INTEGER NOT NULL,
      success INTEGER NOT NULL,status_known INTEGER NOT NULL,sessions INTEGER NOT NULL,
      input INTEGER,output INTEGER,cache_read INTEGER,cache_write INTEGER,
      cache_write_5m INTEGER,cache_write_1h INTEGER,image_input INTEGER,image_output INTEGER,audio_input INTEGER,audio_output INTEGER,
      cost TEXT NOT NULL,unpriced INTEGER NOT NULL,duration_ms INTEGER NOT NULL,measured_outputs INTEGER NOT NULL,generation_ms INTEGER NOT NULL,
      PRIMARY KEY(kind,owner,ordinal));
      CREATE INDEX IF NOT EXISTS usage_filter_client ON records(client,time DESC,id DESC);
      CREATE INDEX IF NOT EXISTS usage_filter_provider ON records(provider,time DESC,id DESC);
      CREATE INDEX IF NOT EXISTS usage_filter_model ON records(model,time DESC,id DESC);
      CREATE INDEX IF NOT EXISTS usage_page ON records(effective,time DESC,id DESC);
      CREATE INDEX IF NOT EXISTS usage_filter_operation ON records(
        CASE WHEN json_extract(body,'$.attempts[#-1].operation')='web_search' THEN 'web_search'
        WHEN json_extract(body,'$.attempts[#-1].operation')='compaction' OR json_extract(body,'$.attempts[#-1].compactionKind') IS NOT NULL THEN 'compaction' ELSE 'model' END,
        time DESC,id DESC);
      CREATE INDEX IF NOT EXISTS usage_duplicate ON records(duplicate_of,source);
      CREATE INDEX IF NOT EXISTS daily_time ON daily(day);"))
}
pub fn project(
    c: &Connection,
    kind: i64,
    owner: &str,
    r: &Record,
    count: u64,
    historical: Option<&Totals>,
) -> Result<()> {
    if kind == 0 && r.source == "proxy" {
        let mut original = r.clone();
        original.gateway_only();
        project(c, 2, owner, &original, count, historical)?;
    }
    db(c.execute(
        "DELETE FROM usage_metrics WHERE kind=?1 AND owner=?2",
        params![kind, owner],
    ))?;
    let mut stmt=db(c.prepare_cached("INSERT INTO usage_metrics VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24,?25,?26,?27,?28,?29)"))?;
    for (i, a) in r.attempts.iter().enumerate() {
        let final_record = i + 1 == r.attempts.len();
        let known = final_record && a.status.is_some();
        let success = known
            && a.status
                .is_some_and(|s| (200..300).contains(&s) || s == 101 && r.completed);
        let duration = if final_record {
            historical.map_or(a.duration_ms * count, |t| t.duration_ms)
        } else {
            0
        };
        let generation = a
            .first_token_ms
            .filter(|t| a.duration_ms > *t)
            .map(|t| a.duration_ms - t);
        let measured = if final_record {
            historical.map_or(generation.and(a.tokens.output).unwrap_or(0) * count, |t| {
                t.measured_outputs
            })
        } else {
            0
        };
        let generation = if final_record {
            historical.map_or(generation.unwrap_or(0) * count, |t| t.generation_ms)
        } else {
            0
        };
        let (first_token_sum, first_token_samples) = if final_record {
            historical.map_or_else(
                || {
                    r.measured_first_token_ms()
                        .map_or((0, 0), |ms| (ms.saturating_mul(count), count))
                },
                |t| (t.first_token_sum_ms, t.first_token_samples),
            )
        } else {
            (0, 0)
        };
        let tokens = &a.tokens;
        let (cache_read, cache_input) = historical.map_or_else(
            || {
                tokens
                    .cache_sample()
                    .map(|(r, i)| (r.saturating_mul(count), i.saturating_mul(count)))
                    .unwrap_or_default()
            },
            |t| {
                if final_record {
                    (t.cache_read_eligible, t.cache_input_eligible)
                } else {
                    (0, 0)
                }
            },
        );
        let mul = |n: Option<u64>| n.map(|n| n.saturating_mul(count).min(i64::MAX as u64) as i64);
        let cost = (a
            .price
            .as_ref()
            .and_then(|p| decimal(&p.cost))
            .unwrap_or_default()
            * Decimal::from(count))
        .to_string();
        db(stmt.execute(params![
            kind,
            owner,
            i as i64,
            a.provider,
            a.grouping_model(),
            if final_record { count } else { 0 },
            count.saturating_mul(a.repeat_count),
            if success { count } else { 0 },
            if known { count } else { 0 },
            if final_record && r.source != "proxy" {
                count
            } else {
                0
            },
            mul(tokens.input),
            mul(tokens.output),
            mul(tokens.cache_read),
            mul(tokens.cache_write),
            mul(tokens.cache_write_5m),
            mul(tokens.cache_write_1h),
            mul(tokens.image_input),
            mul(tokens.image_output),
            mul(tokens.audio_input),
            mul(tokens.audio_output),
            cost,
            count.saturating_mul(a.unpriced_count()),
            duration,
            measured,
            generation,
            first_token_sum,
            first_token_samples,
            cache_read,
            cache_input
        ]))?;
    }
    Ok(())
}
pub fn eligible(f: &Filter, alias: &str, review: bool) -> String {
    let effective = if review {
        format!("{alias}.effective IN (1,2)")
    } else {
        format!("{alias}.effective=1")
    };
    match f.source.as_deref() {
        Some("proxy") => format!("{effective} AND {alias}.source='proxy'"),
        Some("sessions") => format!("{alias}.source<>'proxy' AND ({effective} OR ({alias}.effective=0 AND {alias}.duplicate_of IN (SELECT id FROM records WHERE source='proxy' UNION SELECT id FROM receipts WHERE source='proxy') AND NOT EXISTS(SELECT 1 FROM records peer WHERE peer.source<>'proxy' AND peer.duplicate_of={alias}.duplicate_of AND (peer.time<{alias}.time OR (peer.time={alias}.time AND peer.id<{alias}.id)))))"),
        _ => effective,
    }
}
pub fn conditions(f: &Filter, alias: &str, status: bool) -> (String, Vec<Value>) {
    let mut terms = vec![format!("{alias}.time>=?"), format!("{alias}.time<=?")];
    let mut args = vec![
        Value::Integer(f.start.unwrap_or(0)),
        Value::Integer(f.end.unwrap_or(i64::MAX)),
    ];
    for (column, value) in [
        ("client", &f.client),
        ("provider", &f.provider),
        ("model", &f.model),
    ] {
        if let Some(value) = value.as_ref().filter(|v| !v.is_empty()) {
            if column == "model" && f.source.as_deref() == Some("proxy") {
                terms.push(format!("EXISTS(SELECT 1 FROM usage_metrics original WHERE original.kind=2 AND original.owner={alias}.id AND original.requests>0 AND original.model=?)"));
            } else {
                terms.push(format!("{alias}.{column}=?"));
            }
            args.push(Value::Text(value.clone()));
        }
    }
    if let Some(operation) = f.operation.as_deref().filter(|v| !v.is_empty()) {
        terms.push(format!("CASE WHEN json_extract({alias}.body,'$.attempts[#-1].operation')='web_search' THEN 'web_search' WHEN json_extract({alias}.body,'$.attempts[#-1].operation')='compaction' OR json_extract({alias}.body,'$.attempts[#-1].compactionKind') IS NOT NULL THEN 'compaction' ELSE 'model' END=?"));
        args.push(Value::Text(operation.into()));
    }
    if status {
        match f.status.as_deref() {
            Some("none") => terms.push(format!("{alias}.status IS NULL")),
            Some("2xx") => terms.push(format!("{alias}.status BETWEEN 200 AND 299")),
            Some("4xx") => terms.push(format!("{alias}.status BETWEEN 400 AND 499")),
            Some("5xx") => terms.push(format!("{alias}.status BETWEEN 500 AND 599")),
            Some(v) if !v.is_empty() => {
                terms.push(format!("{alias}.status=?"));
                args.push(Value::Integer(v.parse().unwrap_or(-1)));
            }
            _ => (),
        }
    }
    (terms.join(" AND "), args)
}
const SUM:&str="coalesce(sum(requests),0),coalesce(sum(attempts),0),coalesce(sum(success),0),coalesce(sum(status_known),0),coalesce(sum(sessions),0),sum(input),sum(output),sum(cache_read),sum(cache_write),sum(cache_write_5m),sum(cache_write_1h),sum(image_input),sum(image_output),sum(audio_input),sum(audio_output),decimal_sum(cost),coalesce(sum(unpriced),0),coalesce(sum(duration_ms),0),coalesce(sum(measured_outputs),0),coalesce(sum(generation_ms),0),coalesce(sum(first_token_sum_ms),0),coalesce(sum(first_token_samples),0),coalesce(sum(cache_read_eligible),0),coalesce(sum(cache_input_eligible),0)";
fn totals(row: &Row<'_>, i: usize) -> rusqlite::Result<Totals> {
    Ok(Totals {
        requests: row.get(i)?,
        attempts: row.get(i + 1)?,
        success: row.get(i + 2)?,
        status_known: row.get(i + 3)?,
        sessions: row.get(i + 4)?,
        tokens: Tokens {
            input: row.get(i + 5)?,
            output: row.get(i + 6)?,
            cache_read: row.get(i + 7)?,
            cache_write: row.get(i + 8)?,
            cache_write_5m: row.get(i + 9)?,
            cache_write_1h: row.get(i + 10)?,
            image_input: row.get(i + 11)?,
            image_output: row.get(i + 12)?,
            audio_input: row.get(i + 13)?,
            audio_output: row.get(i + 14)?,
        },
        cost: row.get(i + 15)?,
        unpriced: row.get(i + 16)?,
        duration_ms: row.get(i + 17)?,
        measured_outputs: row.get(i + 18)?,
        generation_ms: row.get(i + 19)?,
        first_token_sum_ms: row.get(i + 20)?,
        first_token_samples: row.get(i + 21)?,
        cache_read_eligible: row.get(i + 22)?,
        cache_input_eligible: row.get(i + 23)?,
    })
}
fn selection(f: &Filter) -> (String, Vec<Value>) {
    let (condition, mut args) = conditions(f, "r", false);
    let mut daily = vec![
        "d.day>=?".to_string(),
        "usage_day_end(d.day)<=?".to_string(),
    ];
    args.push(Value::Integer(f.start.unwrap_or(0)));
    args.push(Value::Integer(f.end.unwrap_or_else(now).saturating_add(1)));
    for (key, value) in [
        ("client", &f.client),
        ("provider", &f.provider),
        ("model", &f.model),
    ] {
        if let Some(v) = value.as_ref().filter(|v| !v.is_empty()) {
            daily.push(format!("json_extract(d.body,'$.{key}')=?"));
            args.push(Value::Text(v.clone()));
        }
    }
    if let Some(operation) = f.operation.as_deref().filter(|v| !v.is_empty()) {
        daily.push("CASE WHEN json_extract(d.body,'$.example.attempts[#-1].operation')='web_search' THEN 'web_search' WHEN json_extract(d.body,'$.example.attempts[#-1].operation')='compaction' OR json_extract(d.body,'$.example.attempts[#-1].compactionKind') IS NOT NULL THEN 'compaction' ELSE 'model' END=?".into());
        args.push(Value::Text(operation.into()));
    }
    let eligible = eligible(f, "r", false);
    let kind = if f.source.as_deref() == Some("proxy") {
        2
    } else {
        0
    };
    daily.push("coalesce(json_extract(d.body,'$.scope'),'all')=?".into());
    args.push(Value::Text(
        f.source
            .as_deref()
            .filter(|s| matches!(*s, "proxy" | "sessions"))
            .unwrap_or("all")
            .into(),
    ));
    let cte=format!("WITH selected AS (SELECT {kind} kind,id owner,time,source FROM records r WHERE {eligible} AND {condition} UNION ALL SELECT 1,d.id,d.day,json_extract(d.body,'$.source') FROM daily d WHERE {}), m AS (SELECT s.time,s.source,x.* FROM selected s JOIN usage_metrics x ON x.kind=s.kind AND x.owner=s.owner) ",daily.join(" AND "));
    (cte, args)
}
pub fn dashboard(c: &Connection, f: &Filter, detail_since: i64) -> Result<Dashboard> {
    let start = f.start.unwrap_or(0);
    let end = f.end.unwrap_or_else(now);
    let (cte, args) = selection(f);
    // Each aggregate reads the same narrow materialized projection, rather than
    // repeating the indexed record/attempt join for every panel.
    let cte = cte.replace("m AS (", "m AS MATERIALIZED (");
    let rolled = "EXISTS(SELECT 1 FROM selected WHERE kind=1)";
    // Only the final attempt has a request count. Attribute a missing price to
    // that row once, consulting earlier attempts only if its own price exists.
    // This avoids sorting every request ID for a second logical aggregation.
    let total_sum=SUM.replace("coalesce(sum(unpriced),0)","coalesce(sum(CASE WHEN requests>0 AND (unpriced>0 OR EXISTS(SELECT 1 FROM usage_metrics a WHERE a.kind=m.kind AND a.owner=m.owner AND a.unpriced>0)) THEN requests ELSE 0 END),0)");
    // Source badges only use request counts, not a full cost/token aggregate.
    let source_sum="coalesce(sum(requests),0),0,0,0,0,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,'0',0,0,0,0,0,0,0,0";
    let sql=format!("{cte}
      SELECT 'totals','',{total_sum},{rolled} FROM m
      UNION ALL SELECT 'provider',coalesce(provider,'session'),{SUM},0 FROM m GROUP BY coalesce(provider,'session')
      UNION ALL SELECT 'model',coalesce(model,'unknown'),{SUM},0 FROM m GROUP BY coalesce(model,'unknown')
      UNION ALL SELECT 'source',source,{source_sum},0 FROM m GROUP BY source");
    let mut stmt = db(c.prepare(&sql))?;
    let rows = db(
        stmt.query_map(rusqlite::params_from_iter(args.iter()), |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                totals(r, 2)?,
                r.get::<_, bool>(26)?,
            ))
        }),
    )?;
    let mut total = Totals::default();
    let mut providers = Vec::new();
    let mut models = Vec::new();
    let mut sources = BTreeMap::new();
    let mut rolled = false;
    for row in rows {
        let (kind, id, value, flag) = db(row)?;
        match kind.as_str() {
            "totals" => {
                total = value;
                rolled = flag;
            }
            "provider" => providers.push(Group { id, totals: value }),
            "model" => models.push(Group { id, totals: value }),
            "source" => {
                sources.insert(id, value.requests);
            }
            _ => return Err(failure("用量分组无效")),
        }
    }
    let order = |a: &Group, b: &Group| {
        b.totals
            .requests
            .cmp(&a.totals.requests)
            .then_with(|| a.id.cmp(&b.id))
    };
    providers.sort_by(order);
    models.sort_by(order);
    let (condition, review_args) = conditions(f, "r", false);
    let eligible = eligible(f, "r", true);
    let review_count = db(c.query_row(
        &format!("SELECT count(*) FROM records r WHERE effective=2 AND {eligible} AND {condition}"),
        rusqlite::params_from_iter(review_args.iter()),
        |r| r.get(0),
    ))?;
    // Old daily totals have already lost the original source associations.
    // Keep them in the combined view and explicitly flag source-only history.
    let source_history_incomplete = if f.source.is_some() {
        db(c.query_row(
            "SELECT EXISTS(SELECT 1 FROM daily WHERE json_extract(body,'$.scope') IS NULL AND day>=?1 AND day<=?2)",
            params![start, end], |r| r.get(0),
        ))?
    } else {
        false
    };
    Ok(Dashboard {
        totals: total,
        providers,
        models,
        precision: if rolled { "day" } else { "millisecond" }.into(),
        detail_since,
        sources,
        review_count,
        source_history_incomplete,
        data_version: version(c)?,
    })
}

pub fn overview_key(c: &Connection, f: &Filter) -> Result<String> {
    let mut f = f.clone();
    f.status = None;
    f.page = 0;
    f.sort = None;
    // Advancing a live end time beyond the latest stored data does not change
    // its result. Real writes (including dedup/compaction) change data_version.
    if let Some(end) = f.end {
        let latest: i64 = db(
            c.query_row("SELECT coalesce(max(time),0) FROM records", [], |r| {
                r.get(0)
            }),
        )?;
        let last_day: Option<i64> =
            db(c.query_row("SELECT max(day) FROM daily", [], |r| r.get(0)))?;
        let daily_end = match last_day {
            Some(day) => {
                db(c.query_row("SELECT usage_day_end(?1)-1", [day], |r| r.get::<_, i64>(0)))?
            }
            None => 0,
        };
        f.end = Some(end.min(latest.max(daily_end)));
    }
    Ok(format!(
        "{}:{}:{}",
        version(c)?,
        chrono::Local::now().offset().local_minus_utc(),
        serde_json::to_string(&f).map_err(|_| failure("用量筛选无效"))?
    ))
}
pub fn version(c: &Connection) -> Result<u64> {
    db(c.query_row(
        "SELECT coalesce((SELECT value FROM metadata WHERE key='data_version'),'0')",
        [],
        |r| {
            let s: String = r.get(0)?;
            Ok(s.parse().unwrap_or(0))
        },
    ))
}
pub fn changed(c: &Connection) -> Result<()> {
    db(c.execute("INSERT INTO metadata(key,value) VALUES('data_version','1') ON CONFLICT(key) DO UPDATE SET value=cast(value as integer)+1",[])).map(|_|())
}
