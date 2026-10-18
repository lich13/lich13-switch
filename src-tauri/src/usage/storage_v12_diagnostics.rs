//! Bounded, checksummed diagnostic blocks. Facts remain ordinary SQL columns;
//! only detail payloads share a compression frame. Updating or deleting a row
//! retires its old slot transactionally, without changing any surviving slot.
use super::*;
use rusqlite::types::Value as Sql;
use std::{cell::RefCell, collections::VecDeque};

type CachedBlock = (Vec<u8>, Value);
thread_local! {
    // Cache by the checked compressed bytes, not a database-local ID. This is
    // safe across connections, rollback, and readers holding an older snapshot.
    static CACHE: RefCell<VecDeque<CachedBlock>> = const { RefCell::new(VecDeque::new()) };
}

pub(super) fn read(c: &Connection, body: Sql) -> Result<(u8, Value)> {
    match body {
        Sql::Integer(0) => Ok((0, json!({}))),
        Sql::Blob(bytes) => Ok((bytes.first().copied().unwrap_or(255), unpack(&bytes)?)),
        Sql::Integer(reference) if reference >= 2048 => {
            let bytes: Vec<u8> = db(db(
                c.prepare_cached("SELECT body FROM v12_diagnostics WHERE id=?1")
            )?
            .query_row([reference / 2048], |r| r.get(0)))?;
            let entry = CACHE.with(|cache| -> Result<Value> {
                let mut cache = cache.borrow_mut();
                let index = if let Some(index) = cache.iter().position(|(b, _)| *b == bytes) {
                    index
                } else {
                    let value = unpack(&bytes)?;
                    if cache.len() == 4 {
                        cache.pop_front();
                    }
                    cache.push_back((bytes, value));
                    cache.len() - 1
                };
                cache[index]
                    .1
                    .as_array()
                    .and_then(|a| a.get((reference % 2048) as usize))
                    .cloned()
                    .ok_or_else(|| failure("用量诊断块缺失，原数据已保留"))
            })?;
            let a = entry
                .as_array()
                .filter(|a| a.len() == 2)
                .ok_or_else(|| failure("用量诊断块字段损坏，原数据已保留"))?;
            let tag = a[0]
                .as_u64()
                .filter(|tag| matches!(tag, 0..=4))
                .ok_or_else(|| failure("用量诊断块版本无效，原数据已保留"))?
                as u8;
            Ok((tag, a[1].clone()))
        }
        _ => Err(failure("用量诊断引用无效，原数据已保留")),
    }
}

fn templates(entries: &Value) -> Result<String> {
    let entries = entries
        .as_array()
        .ok_or_else(|| failure("用量诊断块无效"))?;
    let mut ids = std::collections::BTreeSet::new();
    for entry in entries.iter().filter(|entry| !entry.is_null()) {
        let a = entry
            .as_array()
            .filter(|a| a.len() == 2)
            .ok_or_else(|| failure("用量诊断块无效"))?;
        if a[0] == 4 {
            ids.insert(
                a[1].as_array()
                    .and_then(|v| v.first())
                    .and_then(Value::as_i64)
                    .ok_or_else(|| failure("用量诊断结构缺失"))?,
            );
        }
    }
    serde_json::to_string(&ids).map_err(json_error)
}

struct Pending {
    table: &'static str,
    owner: i64,
    ordinal: i64,
    value: Value,
}
fn write(c: &Connection, entries: &[Pending]) -> Result<()> {
    if entries.is_empty() {
        return Ok(());
    }
    let values = Value::Array(entries.iter().map(|p| p.value.clone()).collect());
    let bytes = pack(&values)?;
    if unpack(&bytes)? != values {
        return Err(failure("用量诊断块核对失败，原数据已保留"));
    }
    db(c.execute(
        "INSERT INTO v12_diagnostics(body,templates) VALUES(?1,?2)",
        params![bytes, templates(&values)?],
    ))?;
    let block = c
        .last_insert_rowid()
        .checked_mul(2048)
        .ok_or_else(|| failure("用量诊断引用超过安全上限"))?;
    for (slot, entry) in entries.iter().enumerate() {
        let where_key = if matches!(entry.table, "v12_rows" | "v12_archives") {
            "rid=?2 AND ?3=0"
        } else {
            "owner=?2 AND ordinal=?3"
        };
        db(c.execute(
            &format!("UPDATE {} SET body=?1 WHERE {where_key}", entry.table),
            params![block + slot as i64, entry.owner, entry.ordinal],
        ))?;
    }
    Ok(())
}

pub(super) fn compact(c: &Connection) -> Result<bool> {
    if !pending(c)? {
        return Ok(false);
    }
    if !has_room(c)? {
        return Err(failure("用量诊断整理等待可用磁盘空间，原数据已保留"));
    }
    let tx = db(c.unchecked_transaction())?;
    let mut pending = Vec::new();
    let mut count = 0;
    let mut size = 0;
    for table in [
        "v12_rows",
        "v12_archives",
        "v12_attempt_rows",
        "v12_session_rows",
        "v12_original",
    ] {
        let columns = if matches!(table, "v12_rows" | "v12_archives") {
            "rid,0"
        } else {
            "owner,ordinal"
        };
        let rows = {
            let mut stmt = db(tx.prepare(&format!("SELECT {columns},body FROM {table} WHERE typeof(body)='blob' AND body<>x'00' LIMIT ?1")))?;
            let rows = db(stmt.query_map([2000 - count], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, Vec<u8>>(2)?,
                ))
            }))?;
            db(rows.collect::<rusqlite::Result<Vec<_>>>())?
        };
        for (owner, ordinal, bytes) in rows {
            let value = json!([bytes.first().copied().unwrap_or(255), unpack(&bytes)?]);
            let len = wire::encode_v12(&value).len();
            if !pending.is_empty() && (pending.len() == 256 || size + len > 1024 * 1024) {
                write(&tx, &pending)?;
                pending.clear();
                size = 0;
            }
            pending.push(Pending {
                table,
                owner,
                ordinal,
                value,
            });
            size += len;
            count += 1;
        }
        if count == 2000 {
            break;
        }
    }
    write(&tx, &pending)?;
    if count > 0 {
        db(tx.execute(
            "INSERT OR REPLACE INTO metadata VALUES('storage_repack','1')",
            [],
        ))?;
    }
    db(tx.commit())?;
    Ok(count > 0)
}

pub(super) fn pending(c: &Connection) -> Result<bool> {
    for table in [
        "v12_rows",
        "v12_archives",
        "v12_attempt_rows",
        "v12_session_rows",
        "v12_original",
    ] {
        if db(c.query_row(
            &format!(
                "SELECT EXISTS(SELECT 1 FROM {table} WHERE typeof(body)='blob' AND body<>x'00')"
            ),
            [],
            |r| r.get::<_, bool>(0),
        ))? {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(super) fn prune(c: &Connection) -> Result<()> {
    // At most 2,000 retired slots are committed at a time. Surviving references
    // never move, including references visible to concurrent read snapshots.
    loop {
        let tx = db(c.unchecked_transaction())?;
        let retired = {
            let mut stmt = db(tx.prepare(
                "SELECT block,slot FROM v12_dead_diagnostics ORDER BY block,slot LIMIT 2000",
            ))?;
            let rows =
                db(stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, usize>(1)?))))?;
            db(rows.collect::<rusqlite::Result<Vec<_>>>())?
        };
        if retired.is_empty() {
            return Ok(());
        }
        let mut groups = std::collections::BTreeMap::<i64, Vec<usize>>::new();
        for (block, slot) in retired {
            groups.entry(block).or_default().push(slot);
        }
        for (block, slots) in groups {
            let bytes: Vec<u8> = db(tx.query_row(
                "SELECT body FROM v12_diagnostics WHERE id=?1",
                [block],
                |r| r.get(0),
            ))?;
            let mut value = unpack(&bytes)?;
            let array = value
                .as_array_mut()
                .ok_or_else(|| failure("用量诊断块无效"))?;
            for slot in &slots {
                *array
                    .get_mut(*slot)
                    .ok_or_else(|| failure("用量诊断块引用无效"))? = Value::Null;
            }
            if array.iter().all(Value::is_null) {
                db(tx.execute("DELETE FROM v12_diagnostics WHERE id=?1", [block]))?;
            } else {
                db(tx.execute(
                    "UPDATE v12_diagnostics SET body=?2,templates=?3 WHERE id=?1",
                    params![block, pack(&value)?, templates(&value)?],
                ))?;
            }
            for slot in slots {
                db(tx.execute(
                    "DELETE FROM v12_dead_diagnostics WHERE block=?1 AND slot=?2",
                    params![block, slot],
                ))?;
            }
        }
        db(tx.commit())?;
    }
}
