//! Accounting facts are stored once. Optional diagnostics use a bounded binary
//! codec; SQL views preserve the established query and detail contracts.
use super::{bodies, model::*, query};
use crate::storage::Result;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};

#[path = "storage_v12_diagnostics.rs"]
mod diagnostics;
use super::body_wire as wire;
const LIMIT: usize = 16 * 1024 * 1024;
const ATTEMPT_COLUMNS: [&str; 25] = [
    "owner",
    "ordinal",
    "shape",
    "body",
    "inclusive_input",
    "input",
    "output",
    "cache_read",
    "cache_write",
    "cache_write_5m",
    "cache_write_1h",
    "image_input",
    "image_output",
    "audio_input",
    "audio_output",
    "cost",
    "unpriced",
    "duration",
    "first_token",
    "repeat_count",
    "provider",
    "model",
    "status",
    "stream",
    "operation",
];
const TOKENS: [&str; 11] = [
    "inclusiveInput",
    "input",
    "output",
    "cacheRead",
    "cacheWrite",
    "cacheWrite5m",
    "cacheWrite1h",
    "imageInput",
    "imageOutput",
    "audioInput",
    "audioOutput",
];
fn db<T>(v: rusqlite::Result<T>) -> Result<T> {
    v.map_err(|error| {
        #[cfg(test)]
        eprintln!("v12 sqlite: {error}");
        let _ = error;
        failure("用量 v12 存储操作失败，原数据已保留")
    })
}
fn json_error(_: serde_json::Error) -> crate::storage::AppError {
    failure("用量 v12 元数据无效，原数据已保留")
}
pub(super) fn active(c: &Connection) -> bool {
    c.query_row(
        "SELECT value='ready' FROM metadata WHERE key='storage_v12'",
        [],
        |r| r.get(0),
    )
    .unwrap_or(false)
}
pub(super) fn pending(c: &Connection) -> bool {
    c.query_row(
        "SELECT value<>'ready' FROM metadata WHERE key='storage_v12'",
        [],
        |r| r.get(0),
    )
    .unwrap_or(false)
}
fn key(s: &str) -> Vec<u8> {
    if s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        let mut out = vec![1];
        for pair in s.as_bytes().chunks_exact(2) {
            out.push(
                (pair[0] as char).to_digit(16).unwrap() as u8 * 16
                    + (pair[1] as char).to_digit(16).unwrap() as u8,
            );
        }
        out
    } else if s.len() == 36 && uuid::Uuid::parse_str(s).is_ok_and(|u| u.to_string() == s) {
        let mut out = vec![2];
        out.extend(uuid::Uuid::parse_str(s).unwrap().as_bytes());
        out
    } else {
        let mut out = vec![0];
        out.extend(s.as_bytes());
        out
    }
}
fn text(bytes: &[u8]) -> Option<String> {
    match bytes.first()? {
        0 => String::from_utf8(bytes[1..].to_vec()).ok(),
        1 if bytes.len() == 33 => Some(bytes[1..].iter().map(|b| format!("{b:02x}")).collect()),
        2 if bytes.len() == 17 => uuid::Uuid::from_slice(&bytes[1..])
            .ok()
            .map(|u| u.to_string()),
        _ => None,
    }
}
fn hint(bytes: &[u8]) -> i64 {
    // This is only an index bucket. Every lookup also compares the complete
    // identifier; collisions never merge records or discard hash bytes.
    i64::from(
        bytes.iter().fold(2_166_136_261u32, |hash, b| {
            (hash ^ u32::from(*b)).wrapping_mul(16_777_619)
        }) & 0x7fff,
    )
}
fn label(value: rusqlite::types::Value) -> rusqlite::Result<rusqlite::types::Value> {
    use rusqlite::types::Value as Sql;
    const LABELS: [&str; 4] = ["proxy", "codex", "claude", "grok"];
    match value {
        Sql::Text(value) => Ok(LABELS
            .iter()
            .position(|s| *s == value)
            .map_or(Sql::Text(value), |i| Sql::Integer(i as i64))),
        Sql::Integer(index) => LABELS
            .get(usize::try_from(index).map_err(|_| rusqlite::Error::InvalidQuery)?)
            .map(|s| Sql::Text((*s).into()))
            .ok_or(rusqlite::Error::InvalidQuery),
        _ => Err(rusqlite::Error::InvalidQuery),
    }
}
fn label_text(value: rusqlite::types::Value) -> Result<String> {
    match value {
        rusqlite::types::Value::Text(text) => Ok(text),
        other => match db(label(other))? {
            rusqlite::types::Value::Text(text) => Ok(text),
            _ => Err(failure("用量来源无效")),
        },
    }
}
pub(super) fn register(c: &Connection) -> Result<()> {
    use rusqlite::functions::FunctionFlags;
    let flags = FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC;
    db(c.create_scalar_function("usage_hint", 1, flags, |ctx| {
        Ok(ctx.get::<Option<Vec<u8>>>(0)?.map(|b| hint(&b)))
    }))?;
    db(c.create_scalar_function("usage_label", 1, flags, |ctx| label(ctx.get(0)?)))?;
    db(
        c.create_scalar_function("usage_signature", 7, flags, |ctx| {
            let client = label_text(ctx.get(0)?).map_err(|_| rusqlite::Error::InvalidQuery)?;
            let shape: Option<Vec<u8>> = ctx.get(1)?;
            let input: Option<u64> = ctx.get(2)?;
            let output: Option<u64> = ctx.get(3)?;
            let read: Option<u64> = ctx.get(4)?;
            let write: Option<u64> = ctx.get(5)?;
            let operation: Option<i64> = ctx.get(6)?;
            let Some((shape, input, output)) =
                shape.zip(input).zip(output).map(|((s, i), o)| (s, i, o))
            else {
                return Ok(None::<String>);
            };
            if operation == Some(2)
                || input
                    .saturating_add(output)
                    .saturating_add(read.unwrap_or(0))
                    .saturating_add(write.unwrap_or(0))
                    == 0
            {
                return Ok(None);
            }
            let shape = shared_value(&shape).ok_or(rusqlite::Error::InvalidQuery)?;
            let model = ["responseModel", "requestedModel", "pricingModel"]
                .into_iter()
                .find_map(|key| shape[key].as_str());
            Ok(model.map(|model| {
                let write = if client == "codex" { None } else { write };
                safe_id(&format!(
                    "{client}:{model}:{input}:{output}:{read:?}:{write:?}"
                ))
            }))
        }),
    )?;
    db(c.create_scalar_function("usage_template", 1, flags, |ctx| {
        let body = match ctx.get::<rusqlite::types::Value>(0)? {
            rusqlite::types::Value::Blob(bytes) => Some(bytes),
            _ => None,
        };
        body.filter(|b| b.first() == Some(&4))
            .map(|body| {
                unpack(&body)
                    .ok()
                    .and_then(|v| v.as_array().and_then(|a| a.first()).and_then(Value::as_i64))
                    .ok_or(rusqlite::Error::InvalidQuery)
            })
            .transpose()
    }))?;
    db(c.create_scalar_function("usage_key", 1, flags, |ctx| {
        Ok(ctx.get::<Option<String>>(0)?.map(|s| key(&s)))
    }))?;
    db(c.create_scalar_function("usage_text", 1, flags, |ctx| {
        let b: Option<Vec<u8>> = ctx.get(0)?;
        b.map(|b| text(&b).ok_or(rusqlite::Error::InvalidQuery))
            .transpose()
    }))
}
fn pack(v: &Value) -> Result<Vec<u8>> {
    let bytes = wire::encode_v13(v);
    if v.as_object().is_some_and(|v| v.is_empty()) {
        return Ok(vec![0]);
    }
    if bytes.len() > LIMIT {
        return Err(failure("用量元数据超过安全上限"));
    }
    let mut encoder =
        zstd::stream::write::Encoder::with_dictionary(Vec::new(), 6, diagnostic_dictionary())
            .map_err(|_| failure("用量压缩失败"))?;
    encoder
        .include_checksum(true)
        .map_err(|_| failure("用量压缩失败"))?;
    encoder
        .write_all(&bytes)
        .map_err(|_| failure("用量压缩失败"))?;
    let compressed = encoder.finish().map_err(|_| failure("用量压缩失败"))?;
    let mut out = vec![2];
    out.extend(compressed);
    Ok(out)
}
fn unpack(bytes: &[u8]) -> Result<Value> {
    if bytes == [0] {
        return Ok(json!({}));
    }
    if !matches!(bytes.first(), Some(1..=4)) || bytes.len() > LIMIT {
        return Err(failure("用量诊断编码无效，原数据已保留"));
    }
    let decoder = zstd::stream::read::Decoder::with_dictionary(
        &bytes[1..],
        if bytes[0] == 1 {
            &[]
        } else {
            diagnostic_dictionary()
        },
    )
    .map_err(|_| failure("用量诊断损坏，原数据已保留"))?;
    let mut plain = Vec::new();
    decoder
        .take((LIMIT + 1) as u64)
        .read_to_end(&mut plain)
        .map_err(|_| failure("用量诊断损坏，原数据已保留"))?;
    if plain.len() > LIMIT {
        return Err(failure("用量诊断超过安全上限"));
    }
    wire::decode(&plain).ok_or_else(|| failure("用量诊断损坏，原数据已保留"))
}
fn diagnostic_dictionary() -> &'static [u8] {
    static DICTIONARY: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
    DICTIONARY.get_or_init(|| {
        let source: Value = serde_json::from_slice(include_bytes!("body-dictionary-v1.json"))
            .expect("immutable diagnostic dictionary");
        wire::encode(&source)
    })
}
fn relative_times(value: &mut Value, anchor: i64, restore: bool) -> Result<()> {
    match value {
        Value::Object(fields) => {
            for (key, v) in fields {
                if matches!(
                    key.as_str(),
                    "at" | "startedAt"
                        | "receivedAt"
                        | "sentAt"
                        | "firstByteAt"
                        | "terminalAt"
                        | "endedAt"
                ) {
                    if let Some(n) = v.as_i64() {
                        *v = json!(if restore {
                            n.checked_add(anchor)
                        } else {
                            n.checked_sub(anchor)
                        }
                        .ok_or_else(|| failure("用量事件时间超过安全范围"))?);
                    }
                } else {
                    relative_times(v, anchor, restore)?;
                }
            }
        }
        Value::Array(values) => {
            for v in values {
                relative_times(v, anchor, restore)?;
            }
        }
        _ => (),
    }
    Ok(())
}
// Diagnostic shapes repeat across requests, while their identifiers and counters
// do not. Keep the complete shape once and compress only its changing values.
// Identifiers still use the lossless wire encoding, including all hash bytes.
fn diagnostic_shape(value: &Value, field: &str, values: &mut Vec<Value>) -> Value {
    match value {
        Value::Number(_) => {
            let position = values.len();
            values.push(value.clone());
            json!(position)
        }
        Value::String(_) if field == "id" || field.ends_with("Id") || field.ends_with("Hash") => {
            let position = values.len();
            values.push(value.clone());
            json!(position)
        }
        Value::Object(object) => Value::Object(
            object
                .iter()
                .map(|(key, value)| (key.clone(), diagnostic_shape(value, key, values)))
                .collect(),
        ),
        Value::Array(array) => Value::Array(
            array
                .iter()
                .map(|value| diagnostic_shape(value, field, values))
                .collect(),
        ),
        _ => value.clone(),
    }
}
fn restore_diagnostic(shape: Value, values: &[Value]) -> Result<Value> {
    match shape {
        Value::Number(index) => index
            .as_u64()
            .and_then(|i| usize::try_from(i).ok())
            .and_then(|i| values.get(i))
            .cloned()
            .ok_or_else(|| failure("用量诊断字段缺失")),
        Value::Object(object) => object
            .into_iter()
            .map(|(key, value)| Ok((key, restore_diagnostic(value, values)?)))
            .collect::<Result<serde_json::Map<_, _>>>()
            .map(Value::Object),
        Value::Array(array) => array
            .into_iter()
            .map(|value| restore_diagnostic(value, values))
            .collect::<Result<Vec<_>>>()
            .map(Value::Array),
        other => Ok(other),
    }
}
fn pack_diagnostic(
    c: &Connection,
    mut value: Value,
    anchor: i64,
) -> Result<rusqlite::types::Value> {
    relative_times(&mut value, anchor, false)?;
    let mut bytes = pack(&value)?;
    if bytes[0] != 0 {
        bytes[0] = 3;
    }
    if bytes.len() > 96 {
        let mut values = Vec::new();
        let shape = diagnostic_shape(&value, "", &mut values);
        let shape = pack(&shape)?;
        let id = intern_blob(c, "v12_templates", "body", &shape)?;
        values.insert(0, json!(id));
        let mut compact = pack(&Value::Array(values))?;
        compact[0] = 4;
        if compact.len() < bytes.len() {
            bytes = compact;
        }
    }
    Ok(if bytes == [0] {
        rusqlite::types::Value::Integer(0)
    } else {
        rusqlite::types::Value::Blob(bytes)
    })
}
fn unpack_diagnostic(c: &Connection, body: rusqlite::types::Value, anchor: i64) -> Result<Value> {
    let (tag, mut v) = diagnostics::read(c, body)?;
    if tag == 4 {
        let values = v.as_array().ok_or_else(|| failure("用量诊断字段无效"))?;
        let id = values
            .first()
            .and_then(Value::as_i64)
            .ok_or_else(|| failure("用量诊断结构缺失"))?;
        let shape: Vec<u8> = db(db(
            c.prepare_cached("SELECT body FROM v12_templates WHERE id=?1")
        )?
        .query_row([id], |row| row.get(0)))?;
        v = restore_diagnostic(unpack(&shape)?, &values[1..])?;
    }
    if matches!(tag, 3 | 4) {
        relative_times(&mut v, anchor, true)?;
    }
    Ok(v)
}
fn intern(c: &Connection, v: &Value) -> Result<i64> {
    let payload = wire::encode_v13(v);
    if payload.len() > LIMIT {
        return Err(failure("用量共享元数据超过安全上限"));
    }
    let mut bytes = Sha256::digest(&payload).to_vec();
    bytes.extend(payload);
    intern_blob(c, "v12_dictionary", "value", &bytes)
}
fn intern_blob(c: &Connection, table: &str, column: &str, bytes: &[u8]) -> Result<i64> {
    if let Some(id) = db(db(c.prepare_cached(&format!(
        "SELECT id FROM {table} WHERE usage_hint({column})=usage_hint(?1) AND {column}=?1"
    )))?
    .query_row([bytes], |row| row.get(0))
    .optional())?
    {
        return Ok(id);
    }
    db(
        db(c.prepare_cached(&format!("INSERT INTO {table}({column}) VALUES(?1)")))?
            .execute([bytes]),
    )?;
    Ok(c.last_insert_rowid())
}

fn dictionary(c: &Connection, id: i64) -> Result<Value> {
    let bytes: Vec<u8> = db(c.query_row(
        "SELECT value FROM v12_dictionary WHERE id=?1",
        [id],
        |r| r.get(0),
    ))?;
    shared_value(&bytes).ok_or_else(|| failure("用量共享元数据损坏"))
}
fn shared_value(bytes: &[u8]) -> Option<Value> {
    if bytes.len() < 32
        || bytes.len() > LIMIT + 32
        || Sha256::digest(&bytes[32..])[..] != bytes[..32]
    {
        return None;
    }
    wire::decode(&bytes[32..])
}
fn intern_text(c: &Connection, v: Option<&str>) -> Result<Option<i64>> {
    v.map(|v| intern(c, &json!(v))).transpose()
}

pub(super) fn protect_encoding(c: &Connection) -> Result<()> {
    // Commit the reader guard before any tag-11 payload can be written, even
    // when resuming a pending copy or opening an already active v12 layout.
    let version: i64 = db(c.query_row("PRAGMA user_version", [], |row| row.get(0)))?;
    if version > 13 {
        return Err(failure("用量数据库来自更高版本，已保留原文件"));
    }
    if version == 13 {
        return Ok(());
    }
    let tx = db(c.unchecked_transaction())?;
    db(tx.execute_batch("PRAGMA user_version=13;"))?;
    db(tx.commit())
}

pub(super) fn install(c: &Connection) -> Result<()> {
    protect_encoding(c)?;
    if active(c) || pending(c) {
        return Ok(());
    }
    let tx = db(c.unchecked_transaction())?;
    db(tx.execute_batch("CREATE TABLE v12_dictionary(id INTEGER PRIMARY KEY,value BLOB NOT NULL);
      CREATE INDEX v12_dictionary_hint ON v12_dictionary(usage_hint(value));
      CREATE TABLE v12_templates(id INTEGER PRIMARY KEY,body BLOB NOT NULL);
      CREATE INDEX v12_templates_hint ON v12_templates(usage_hint(body));
      CREATE TABLE v12_diagnostics(id INTEGER PRIMARY KEY,body BLOB NOT NULL,templates TEXT NOT NULL);
      CREATE TABLE v12_archives(rid INTEGER PRIMARY KEY,body BLOB NOT NULL);
      CREATE TABLE v12_dead_diagnostics(block INTEGER NOT NULL,slot INTEGER NOT NULL,PRIMARY KEY(block,slot)) WITHOUT ROWID;
      CREATE TABLE v12_record_states(id INTEGER PRIMARY KEY,started INTEGER NOT NULL,finished INTEGER,completed INTEGER NOT NULL,last INTEGER NOT NULL,archived INTEGER NOT NULL,original INTEGER NOT NULL,compaction INTEGER NOT NULL,ended INTEGER NOT NULL);
      CREATE UNIQUE INDEX v12_record_state_values ON v12_record_states(started,finished,completed,last,archived,original,compaction,ended);
      CREATE TABLE v12_rows(rid INTEGER PRIMARY KEY,key BLOB NOT NULL,client BLOB NOT NULL,source BLOB NOT NULL,time INTEGER NOT NULL,response BLOB,signature INTEGER,effective INTEGER NOT NULL,duplicate INTEGER,archived_duplicate TEXT,sequence INTEGER NOT NULL,shape INTEGER NOT NULL,body BLOB NOT NULL,session BLOB,state INTEGER NOT NULL);
      CREATE VIEW v12_records AS SELECT r.rid,r.key,r.client,r.source,r.time,r.response,r.signature,r.effective,r.duplicate,r.archived_duplicate,r.sequence,s.started,s.finished,s.completed,s.last,r.shape,r.body,s.archived,s.original,s.compaction,s.ended,r.session FROM v12_rows r JOIN v12_record_states s ON s.id=r.state;
      CREATE INDEX v12_key_hint ON v12_rows(usage_hint(key));
      CREATE INDEX v12_time ON v12_rows(time);
      CREATE INDEX v12_response ON v12_rows(usage_hint(response)) WHERE response IS NOT NULL;
      CREATE INDEX v12_signature ON v12_rows(signature,source) WHERE signature IS NOT NULL;
      CREATE INDEX v12_duplicate ON v12_rows(duplicate) WHERE duplicate IS NOT NULL;
      CREATE INDEX v12_archived_duplicate ON v12_rows(archived_duplicate,time) WHERE archived_duplicate IS NOT NULL;
      CREATE TRIGGER v12_record_update INSTEAD OF UPDATE OF body,effective,duplicate,archived_duplicate ON v12_records BEGIN UPDATE v12_rows SET body=NEW.body,effective=NEW.effective,duplicate=NEW.duplicate,archived_duplicate=NEW.archived_duplicate WHERE rid=OLD.rid; END;
      CREATE TRIGGER v12_record_delete INSTEAD OF DELETE ON v12_records BEGIN DELETE FROM v12_rows WHERE rid=OLD.rid; END;
      CREATE TABLE v12_links(owner INTEGER PRIMARY KEY,target BLOB NOT NULL);
      CREATE TABLE v12_dirty(id TEXT PRIMARY KEY) WITHOUT ROWID;
      INSERT OR REPLACE INTO metadata VALUES('storage_v12','copy'),('storage_v12_after','');
      PRAGMA user_version=13;"))?;
    db(tx.execute_batch("CREATE TABLE v12_measures(id INTEGER PRIMARY KEY,hint INTEGER NOT NULL,
      inclusive_input INTEGER,input INTEGER,output INTEGER,cache_read INTEGER,cache_write INTEGER,cache_write_5m INTEGER,cache_write_1h INTEGER,image_input INTEGER,image_output INTEGER,audio_input INTEGER,audio_output INTEGER,
      cost TEXT,unpriced INTEGER NOT NULL,duration INTEGER NOT NULL,repeat_count INTEGER NOT NULL);
      CREATE INDEX v12_measure_hint ON v12_measures(hint);
      CREATE TABLE v12_dimensions(shape INTEGER PRIMARY KEY,provider INTEGER,model INTEGER,status INTEGER,stream INTEGER NOT NULL,operation INTEGER NOT NULL);"))?;
    for (table, view) in [
        ("v12_attempt_rows", "v12_attempts"),
        ("v12_session_rows", "v12_sessions"),
    ] {
        db(tx.execute_batch(&format!("CREATE TABLE {table}(owner INTEGER NOT NULL,ordinal INTEGER NOT NULL,shape INTEGER NOT NULL,body BLOB NOT NULL,measure INTEGER NOT NULL,first_token INTEGER,PRIMARY KEY(owner,ordinal)) WITHOUT ROWID;
          CREATE INDEX {table}_shape ON {table}(shape);
          CREATE VIEW {view} AS SELECT a.owner,a.ordinal,a.shape,a.body,m.inclusive_input,m.input,m.output,m.cache_read,m.cache_write,m.cache_write_5m,m.cache_write_1h,m.image_input,m.image_output,m.audio_input,m.audio_output,m.cost,m.unpriced,m.duration,a.first_token,m.repeat_count,d.provider,d.model,d.status,d.stream,d.operation FROM {table} a JOIN v12_measures m ON m.id=a.measure JOIN v12_dimensions d ON d.shape=a.shape;")))?;
    }
    db(tx.execute_batch("CREATE TABLE v12_original(owner INTEGER NOT NULL,ordinal INTEGER NOT NULL,shape INTEGER,body BLOB,
      inclusive_input INTEGER,input INTEGER,output INTEGER,cache_read INTEGER,cache_write INTEGER,cache_write_5m INTEGER,cache_write_1h INTEGER,image_input INTEGER,image_output INTEGER,audio_input INTEGER,audio_output INTEGER,
      cost TEXT,unpriced INTEGER,duration INTEGER,first_token INTEGER,repeat_count INTEGER,provider INTEGER,model INTEGER,status INTEGER,stream INTEGER,operation INTEGER,mask INTEGER NOT NULL DEFAULT 0,metrics_changed INTEGER NOT NULL DEFAULT 0,PRIMARY KEY(owner,ordinal)) WITHOUT ROWID;"))?;
    db(tx.execute_batch("CREATE VIEW v12_all_attempts AS SELECT * FROM v12_attempts UNION ALL SELECT * FROM v12_sessions;"))?;
    for table in [
        "v12_rows",
        "v12_archives",
        "v12_attempt_rows",
        "v12_session_rows",
        "v12_original",
    ] {
        let columns = if matches!(table, "v12_rows" | "v12_archives") {
            "rid"
        } else {
            "owner,ordinal"
        };
        db(tx.execute_batch(&format!("CREATE INDEX {table}_unpacked ON {table}({columns}) WHERE typeof(body)='blob' AND body<>x'00';")))?;
        db(tx.execute_batch(&format!("CREATE TRIGGER {table}_diagnostic_delete AFTER DELETE ON {table} WHEN typeof(OLD.body)='integer' AND OLD.body>=2048 BEGIN INSERT OR IGNORE INTO v12_dead_diagnostics VALUES(OLD.body/2048,OLD.body%2048); END;
          CREATE TRIGGER {table}_diagnostic_update AFTER UPDATE OF body ON {table} WHEN typeof(OLD.body)='integer' AND OLD.body>=2048 AND OLD.body IS NOT NEW.body BEGIN INSERT OR IGNORE INTO v12_dead_diagnostics VALUES(OLD.body/2048,OLD.body%2048); END;")))?;
    }
    let columns = ATTEMPT_COLUMNS
        .iter()
        .enumerate()
        .map(|(i, n)| {
            if i < 2 {
                format!("a.{n}")
            } else {
                format!(
                    "CASE WHEN o.mask & {} <> 0 THEN o.{n} ELSE a.{n} END {n}",
                    1i64 << i
                )
            }
        })
        .collect::<Vec<_>>()
        .join(",");
    db(tx.execute_batch(&format!("CREATE VIEW v12_raw_attempts AS SELECT {columns} FROM v12_all_attempts a LEFT JOIN v12_original o ON a.owner=o.owner AND a.ordinal=o.ordinal;")))?;
    for (name, event, id) in [
        ("insert", "INSERT", "NEW.id"),
        ("update", "UPDATE", "NEW.id"),
        ("delete", "DELETE", "OLD.id"),
    ] {
        db(tx.execute_batch(&format!("CREATE TRIGGER v12_track_{name} AFTER {event} ON records BEGIN INSERT OR IGNORE INTO v12_dirty VALUES({id}); END;")))?;
    }
    db(tx.commit())
}
fn record_state(c: &Connection, values: &[rusqlite::types::Value]) -> Result<i64> {
    const FIELDS: &str = "started,finished,completed,last,archived,original,compaction,ended";
    if let Some(id) = db(db(c.prepare_cached(&format!(
        "SELECT id FROM v12_record_states WHERE ({FIELDS}) IS (?1,?2,?3,?4,?5,?6,?7,?8)"
    )))?
    .query_row(rusqlite::params_from_iter(values), |r| r.get(0))
    .optional())?
    {
        return Ok(id);
    }
    db(db(c.prepare_cached("INSERT INTO v12_record_states(started,finished,completed,last,archived,original,compaction,ended) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)"))?
        .execute(rusqlite::params_from_iter(values)))?;
    Ok(c.last_insert_rowid())
}
fn identifier(value: Option<Vec<u8>>) -> Result<Option<String>> {
    value
        .map(|v| text(&v).ok_or_else(|| failure("用量关联标识无效")))
        .transpose()
}
fn measure(c: &Connection, a: &Attempt, n: &[Option<i64>]) -> Result<i64> {
    use rusqlite::types::Value as Sql;
    let mut values: Vec<Sql> = n
        .iter()
        .map(|n| n.map_or(Sql::Null, Sql::Integer))
        .collect();
    values.push(
        a.price
            .as_ref()
            .map_or(Sql::Null, |p| Sql::Text(p.cost.clone())),
    );
    for n in [a.unpriced_count(), a.duration_ms, a.repeat_count] {
        values.push(Sql::Integer(
            i64::try_from(n).map_err(|_| failure("用量计量超过安全范围"))?,
        ));
    }
    let fingerprint = hint(&wire::encode_v13(&json!([
        n,
        a.price.as_ref().map(|p| &p.cost),
        a.unpriced_count(),
        a.duration_ms,
        a.repeat_count
    ])));
    values.insert(0, Sql::Integer(fingerprint));
    const COLUMNS: &str = "inclusive_input,input,output,cache_read,cache_write,cache_write_5m,cache_write_1h,image_input,image_output,audio_input,audio_output,cost,unpriced,duration,repeat_count";
    // Hashes select candidates only. Compare every nullable field and the exact
    // decimal string before sharing a measurement; true zero remains distinct.
    let sql = format!("SELECT id FROM v12_measures WHERE hint=?1 AND ({COLUMNS}) IS (?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)");
    if let Some(id) = db(db(c.prepare_cached(&sql))?
        .query_row(rusqlite::params_from_iter(&values), |r| r.get(0))
        .optional())?
    {
        return Ok(id);
    }
    db(db(c.prepare_cached(&format!("INSERT INTO v12_measures(hint,{COLUMNS}) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)")))?.execute(rusqlite::params_from_iter(values)))?;
    Ok(c.last_insert_rowid())
}
fn save_attempt(
    c: &Connection,
    table: &str,
    owner: i64,
    ordinal: usize,
    a: &Attempt,
    r: &Record,
) -> Result<()> {
    let mut v = serde_json::to_value(a).map_err(json_error)?;
    let fields = v.as_object_mut().ok_or_else(|| failure("用量尝试无效"))?;
    fields.remove("tokens");
    fields.remove("durationMs");
    fields.remove("firstTokenMs");
    fields.remove("repeatCount");
    if a.id == r.id {
        fields.remove("id");
    }
    if a.started_at == r.started_at {
        fields.remove("startedAt");
    }
    if a.response_id == r.final_attempt().and_then(|v| v.response_id.clone()) {
        fields.remove("responseId");
    }
    if let Some(price) = fields.get_mut("price").and_then(Value::as_object_mut) {
        price.remove("cost");
    }
    let mut diagnostic = serde_json::Map::new();
    // Optional counters belong to the extensible diagnostic body. Do not extend
    // the v13 SQL measures or the fixed wire dictionaries.
    if let Some(reasoning) = a.tokens.reasoning {
        diagnostic.insert("reasoningTokens".into(), json!(reasoning));
    }
    if let Some(image_cached) = a.tokens.image_cache_read {
        diagnostic.insert("imageCacheReadTokens".into(), json!(image_cached));
    }
    for field in [
        "transfer",
        "end",
        "threadHash",
        "upstreamRequestHash",
        "routingDecision",
        "routingEvidence",
        "routingOutcome",
        "id",
        "startedAt",
        "responseId",
        "inclusiveInputTokens",
        "generationMs",
    ] {
        if let Some(value) = fields.remove(field) {
            diagnostic.insert(field.into(), value);
        }
    }
    let shape = intern(c, &v)?;
    let t = serde_json::to_value(&a.tokens).map_err(json_error)?;
    let n: Vec<Option<i64>> = TOKENS
        .iter()
        .map(|k| {
            t[*k]
                .as_u64()
                .map(|n| i64::try_from(n).map_err(|_| failure("用量计量超过安全范围")))
                .transpose()
        })
        .collect::<Result<_>>()?;
    let body = pack_diagnostic(c, Value::Object(diagnostic), r.started_at)?;
    let provider = intern_text(c, a.provider.as_deref())?;
    let model = intern_text(c, a.grouping_model())?;
    let operation = match a.operation {
        Operation::Model => 0,
        Operation::Compaction => 1,
        Operation::WebSearch => 2,
    };
    if table == "v12_original" {
        db(c.execute(&format!("INSERT OR REPLACE INTO {table}({}) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24,?25)", ATTEMPT_COLUMNS.join(",")),params![owner,ordinal as i64,shape,body,n[0],n[1],n[2],n[3],n[4],n[5],n[6],n[7],n[8],n[9],n[10],a.price.as_ref().map(|p|p.cost.as_str()),a.unpriced_count(),a.duration_ms,a.first_token_ms,a.repeat_count,provider,model,a.status,a.stream,operation]))?;
    } else {
        db(
            db(c.prepare_cached("INSERT OR IGNORE INTO v12_dimensions VALUES(?1,?2,?3,?4,?5,?6)"))?
                .execute(params![
                    shape, provider, model, a.status, a.stream, operation
                ]),
        )?;
        let measure = measure(c, a, &n)?;
        let table = if table == "v12_sessions" {
            "v12_session_rows"
        } else {
            "v12_attempt_rows"
        };
        db(db(c.prepare_cached(&format!(
            "INSERT OR REPLACE INTO {table} VALUES(?1,?2,?3,?4,?5,?6)"
        )))?
        .execute(params![
            owner,
            ordinal as i64,
            shape,
            body,
            measure,
            a.first_token_ms
        ]))?;
    }
    Ok(())
}
pub(super) fn save(c: &Connection, r: &Record, effective: i64, sequence: i64) -> Result<()> {
    let mut v = serde_json::to_value(r).map_err(json_error)?;
    let obj = v.as_object_mut().ok_or_else(|| failure("用量记录无效"))?;
    for k in [
        "id",
        "client",
        "source",
        "startedAt",
        "finishedAt",
        "attempts",
        "gatewayReported",
        "completed",
        "duplicateOf",
        "detailArchived",
        "sessionId",
    ] {
        obj.remove(k);
    }
    let mut diagnostic = serde_json::Map::new();
    if let Some(value) = obj
        .remove("failoverEvidence")
        .filter(|value| !value.is_null())
    {
        diagnostic.insert("failoverEvidence".into(), value);
    }
    let shape = intern(c, &v)?;
    let response = r
        .final_attempt()
        .and_then(|a| a.response_id.as_deref())
        .map(key);
    let sig = r.signature().map(|signature| hint(&key(&signature)));
    let session = r.session_id.as_deref().map(key);
    let previous = owner(c, &r.id)?;
    let time = r.event_time();
    let delta = |at: i64| {
        at.checked_sub(time)
            .ok_or_else(|| failure("用量时间超过安全范围"))
    };
    let ended = r
        .final_attempt()
        .map(|a| {
            a.started_at
                .saturating_add(a.duration_ms.min(i64::MAX as u64) as i64)
        })
        .unwrap_or(r.started_at);
    use rusqlite::types::Value as Sql;
    let state = record_state(
        c,
        &[
            Sql::Integer(delta(r.started_at)?),
            r.finished_at
                .map(delta)
                .transpose()?
                .map_or(Sql::Null, Sql::Integer),
            Sql::Integer(i64::from(r.completed)),
            Sql::Integer(r.attempts.len() as i64 - 1),
            Sql::Integer(i64::from(r.detail_archived)),
            Sql::Integer(i64::from(r.gateway_reported.is_some())),
            Sql::Integer(i64::from(
                r.final_attempt()
                    .is_some_and(|a| a.compaction_kind.is_some()),
            )),
            Sql::Integer(delta(ended)?),
        ],
    )?;
    db(c.execute("INSERT INTO v12_rows(key,client,source,time,response,signature,effective,sequence,shape,body,rid,session,state) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13) ON CONFLICT(rid) DO UPDATE SET client=excluded.client,source=excluded.source,time=excluded.time,response=excluded.response,signature=excluded.signature,effective=excluded.effective,shape=excluded.shape,body=excluded.body,session=excluded.session,state=excluded.state",params![key(&r.id),db(label(r.client.clone().into()))?,db(label(r.source.clone().into()))?,time,response,sig,effective,sequence,shape,pack_diagnostic(c,Value::Object(diagnostic),r.started_at)?,previous,session,state]))?;
    let owner = previous.unwrap_or_else(|| c.last_insert_rowid());
    for table in [
        "v12_attempt_rows",
        "v12_session_rows",
        "v12_original",
        "v12_links",
    ] {
        db(c.execute(&format!("DELETE FROM {table} WHERE owner=?1"), [owner]))?;
    }
    db(c.execute(
        "UPDATE v12_records SET duplicate=NULL,archived_duplicate=NULL WHERE rid=?1",
        [owner],
    ))?;
    if let Some(target) = &r.duplicate_of {
        let target_owner = self::owner(c, target)?;
        if target_owner.is_none() {
            db(c.execute(
                "INSERT INTO v12_links VALUES(?1,?2)",
                params![owner, key(target)],
            ))?;
        }
        db(c.execute(
            "UPDATE v12_records SET duplicate=?2,archived_duplicate=?3 WHERE rid=?1",
            params![
                owner,
                target_owner,
                target_owner.is_none().then_some(target)
            ],
        ))?;
    }
    let table = if matches!(r.source.as_str(), "proxy" | "grok") {
        "v12_attempts"
    } else {
        "v12_sessions"
    };
    for (i, a) in r.attempts.iter().enumerate() {
        save_attempt(c, table, owner, i, a, r)?;
    }
    if let Some(a) = &r.gateway_reported {
        // Original metadata is necessary even when its accounting projection agrees.
        save_attempt(
            c,
            "v12_original",
            owner,
            r.attempts.len().saturating_sub(1),
            a,
            r,
        )?;
        sparse_original(c, table, owner, r.attempts.len().saturating_sub(1) as i64)?;
    }
    Ok(())
}
fn sparse_original(c: &Connection, table: &str, owner: i64, ordinal: i64) -> Result<()> {
    let values = |table: &str| -> Result<Vec<rusqlite::types::Value>> {
        db(c.query_row(
            &format!(
                "SELECT {} FROM {table} WHERE owner=?1 AND ordinal=?2",
                ATTEMPT_COLUMNS.join(",")
            ),
            params![owner, ordinal],
            |r| (0..25).map(|i| r.get(i)).collect(),
        ))
    };
    let base = values(table)?;
    let mut original = values("v12_original")?;
    let mut mask = 0i64;
    for i in 2..25 {
        if original[i] == base[i] {
            original[i] = rusqlite::types::Value::Null;
        } else {
            mask |= 1 << i;
        }
    }
    if mask == 0 {
        db(c.execute("DELETE FROM v12_original WHERE owner=?1", [owner]))?;
    } else {
        original.push(mask.into());
        original.push(i64::from(mask & !15 != 0).into());
        let setters = ATTEMPT_COLUMNS
            .iter()
            .enumerate()
            .skip(2)
            .map(|(i, n)| format!("{n}=?{}", i + 1))
            .collect::<Vec<_>>()
            .join(",");
        db(c.execute(&format!("UPDATE v12_original SET {setters},mask=?26,metrics_changed=?27 WHERE owner=?1 AND ordinal=?2"),rusqlite::params_from_iter(original)))?;
    }
    Ok(())
}
fn load_attempt(c: &Connection, table: &str, owner: i64, ordinal: i64, r: &Value) -> Result<Value> {
    let mut stmt = db(c.prepare_cached(&format!(
        "SELECT * FROM {table} WHERE owner=?1 AND ordinal=?2"
    )))?;
    let mut rows = db(stmt.query(params![owner, ordinal]))?;
    let row = db(rows.next())?.ok_or_else(|| failure("用量尝试缺失"))?;
    let mut value = dictionary(c, db(row.get(2))?)?;
    let obj = value
        .as_object_mut()
        .ok_or_else(|| failure("用量尝试无效"))?;
    let extra = unpack_diagnostic(
        c,
        db(row.get(3))?,
        r["startedAt"]
            .as_i64()
            .ok_or_else(|| failure("用量开始时间缺失"))?,
    )?;
    obj.extend(
        extra
            .as_object()
            .ok_or_else(|| failure("用量诊断无效"))?
            .clone(),
    );
    obj.entry("id").or_insert_with(|| r["id"].clone());
    obj.entry("startedAt")
        .or_insert_with(|| r["startedAt"].clone());
    obj.entry("responseId")
        .or_insert_with(|| r["responseId"].clone());
    let mut tokens = serde_json::Map::new();
    if let Some(reasoning) = obj.remove("reasoningTokens") {
        tokens.insert("reasoning".into(), reasoning);
    }
    if let Some(image_cached) = obj.remove("imageCacheReadTokens") {
        tokens.insert("imageCacheRead".into(), image_cached);
    }
    for (i, k) in TOKENS.iter().enumerate() {
        tokens.insert((*k).into(), json!(db(row.get::<_, Option<u64>>(4 + i))?));
    }
    obj.insert("tokens".into(), Value::Object(tokens));
    let cost: Option<String> = db(row.get(15))?;
    if let Some(price) = obj.get_mut("price").and_then(Value::as_object_mut) {
        price.insert(
            "cost".into(),
            json!(cost.ok_or_else(|| failure("用量价格缺失"))?),
        );
    }
    obj.insert("durationMs".into(), json!(db(row.get::<_, u64>(17))?));
    obj.insert(
        "firstTokenMs".into(),
        json!(db(row.get::<_, Option<u64>>(18))?),
    );
    obj.insert("repeatCount".into(), json!(db(row.get::<_, u64>(19))?));
    Ok(value)
}
pub(super) fn load(c: &Connection, owner: i64) -> Result<Value> {
    let mut stmt = db(c.prepare_cached("SELECT * FROM v12_records WHERE rid=?1"))?;
    let mut rows = db(stmt.query([owner]))?;
    let row = db(rows.next())?.ok_or_else(|| failure("用量记录不存在"))?;
    let mut r = dictionary(c, db(row.get(15))?)?;
    let obj = r.as_object_mut().ok_or_else(|| failure("用量记录无效"))?;
    let time: i64 = db(row.get(4))?;
    let absolute = |delta: i64| {
        time.checked_add(delta)
            .ok_or_else(|| failure("用量时间超过安全范围"))
    };
    let started = absolute(db(row.get(11))?)?;
    let extra = unpack_diagnostic(c, db(row.get(16))?, started)?;
    obj.extend(
        extra
            .as_object()
            .ok_or_else(|| failure("用量诊断无效"))?
            .clone(),
    );
    obj.insert(
        "id".into(),
        json!(text(&db(row.get::<_, Vec<u8>>(1))?).ok_or_else(|| failure("用量标识无效"))?),
    );
    obj.insert("client".into(), json!(label_text(db(row.get(2))?)?));
    let source = label_text(db(row.get(3))?)?;
    obj.insert("source".into(), json!(source));
    obj.insert("startedAt".into(), json!(started));
    obj.insert(
        "finishedAt".into(),
        json!(db(row.get::<_, Option<i64>>(12))?
            .map(absolute)
            .transpose()?),
    );
    obj.insert("completed".into(), json!(db(row.get::<_, bool>(13))?));
    obj.insert("detailArchived".into(), json!(db(row.get::<_, bool>(17))?));
    obj.insert("responseId".into(), json!(identifier(db(row.get(5))?)?));
    obj.insert("sessionId".into(), json!(identifier(db(row.get(21))?)?));
    let target: Option<Vec<u8>> = db(c
        .query_row(
            "SELECT coalesce(d.key,l.target,usage_key(r.archived_duplicate)) FROM v12_records r LEFT JOIN v12_rows d ON d.rid=r.duplicate LEFT JOIN v12_links l ON l.owner=r.rid WHERE r.rid=?1",
            [owner],
            |r| r.get(0),
        )
        )?;
    obj.insert(
        "duplicateOf".into(),
        json!(target
            .as_deref()
            .map(|v| text(v).ok_or_else(|| failure("用量关联标识损坏")))
            .transpose()?),
    );
    let last: i64 = db(row.get(14))?;
    let table = if matches!(source.as_str(), "proxy" | "grok") {
        "v12_attempts"
    } else {
        "v12_sessions"
    };
    let attempts = (0..=last)
        .map(|i| load_attempt(c, table, owner, i, &r))
        .collect::<Result<Vec<_>>>()?;
    let original: bool = db(row.get(18))?;
    if original {
        r["gatewayReported"] = load_attempt(c, "v12_raw_attempts", owner, last, &r)?;
    }
    r["attempts"] = json!(attempts);
    r.as_object_mut().unwrap().remove("responseId");
    Ok(r)
}
pub(super) fn owner(c: &Connection, id: &str) -> Result<Option<i64>> {
    db(c.query_row(
        "SELECT rid FROM v12_records WHERE usage_hint(key)=usage_hint(?1) AND key=?1",
        [key(id)],
        |r| r.get(0),
    )
    .optional())
}
pub(super) fn delete(c: &Connection, id: &str) -> Result<()> {
    if let Some(owner) = owner(c, id)? {
        for table in [
            "v12_attempt_rows",
            "v12_session_rows",
            "v12_original",
            "v12_links",
        ] {
            db(c.execute(&format!("DELETE FROM {table} WHERE owner=?1"), [owner]))?;
        }
        db(c.execute("DELETE FROM v12_records WHERE rid=?1", [owner]))?;
    }
    Ok(())
}
pub(super) fn flush(c: &Connection) -> Result<()> {
    if !pending(c) {
        return Ok(());
    }
    if !has_room(c)? {
        // The old layout and this transactional dirty ledger remain authoritative.
        // A deferred migration must not disable otherwise successful usage writes.
        return Ok(());
    }
    while flush_batch(c)? {}
    Ok(())
}
fn flush_batch(c: &Connection) -> Result<bool> {
    let ids = {
        let mut stmt = db(c.prepare("SELECT id FROM v12_dirty ORDER BY id LIMIT 2000"))?;
        let values = db(stmt.query_map([], |r| r.get::<_, String>(0)))?;
        db(values.collect::<rusqlite::Result<Vec<_>>>())?
    };
    for id in ids {
        let old: Option<(String, i64, i64)> = db(c
            .query_row(
                "SELECT body,effective,local_sequence FROM records WHERE id=?1",
                [&id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional())?;
        if let Some((body, effective, sequence)) = old {
            let r = bodies::decode(c, &body)?;
            save(c, &r, effective, sequence)?;
            verify_record(c, &r)?;
        } else {
            delete(c, &id)?;
        }
        db(c.execute("DELETE FROM v12_dirty WHERE id=?1", [&id]))?;
    }
    db(c.query_row("SELECT EXISTS(SELECT 1 FROM v12_dirty)", [], |r| r.get(0)))
}

fn metric_select(table: &str, kind: i64) -> String {
    let generation = "CASE WHEN r.source=usage_label('antigravity') THEN coalesce(usage_generation_ms(a.body,r.time+r.started),duration-first_token) ELSE duration-first_token END";
    format!("SELECT {kind} kind,a.owner,ordinal,usage_value(p.value) provider,usage_value(m.value) model,
      CASE WHEN ordinal=r.last THEN 1 ELSE 0 END requests,repeat_count attempts,
      CASE WHEN ordinal=r.last AND ((status BETWEEN 200 AND 299 OR status=101) OR (r.source=usage_label('antigravity') AND usage_app_outcome(shape_value.value)=1)) AND r.completed THEN 1 ELSE 0 END success,
      CASE WHEN ordinal=r.last AND (status IS NOT NULL OR (r.source=usage_label('antigravity') AND usage_app_outcome(shape_value.value) IS NOT NULL)) THEN 1 ELSE 0 END status_known,
      CASE WHEN ordinal=r.last AND r.source<>0 THEN 1 ELSE 0 END sessions,
      input,output,cache_read,cache_write,cache_write_5m,cache_write_1h,image_input,image_output,audio_input,audio_output,
      coalesce(cost,'0') cost,unpriced,
      CASE WHEN ordinal=r.last THEN duration ELSE 0 END duration_ms,
      CASE WHEN ordinal=r.last AND {generation}>0 THEN coalesce(output,0) ELSE 0 END measured_outputs,
      CASE WHEN ordinal=r.last AND {generation}>0 THEN {generation} ELSE 0 END generation_ms,
      CASE WHEN ordinal=r.last AND (r.source=0 OR r.source=usage_label('antigravity')) AND stream AND operation=0 THEN coalesce(first_token,0) ELSE 0 END first_token_sum_ms,
      CASE WHEN ordinal=r.last AND (r.source=0 OR r.source=usage_label('antigravity')) AND stream AND operation=0 AND first_token IS NOT NULL THEN 1 ELSE 0 END first_token_samples,
      CASE WHEN cache_read IS NOT NULL AND cache_read<=coalesce(inclusive_input,input+cache_read+cache_write) THEN cache_read ELSE 0 END cache_read_eligible,
      CASE WHEN cache_read IS NOT NULL AND cache_read<=coalesce(inclusive_input,input+cache_read+cache_write) THEN coalesce(inclusive_input,input+cache_read+cache_write) ELSE 0 END cache_input_eligible
      FROM {table} a JOIN v12_records r ON r.rid=a.owner LEFT JOIN v12_dictionary p ON p.id=a.provider LEFT JOIN v12_dictionary m ON m.id=a.model LEFT JOIN v12_dictionary shape_value ON shape_value.id=a.shape")
}
fn create_views(c: &Connection) -> Result<()> {
    db(c.execute_batch("CREATE VIEW records AS SELECT usage_text(r.key) id,r.rid,r.client client_code,r.source source_code,usage_label(r.client) client,usage_label(r.source) source,r.time,usage_value(p.value) provider,usage_value(m.value) model,dim.status status,usage_text(r.response) response,usage_signature(r.client,s.value,measure.input,measure.output,measure.cache_read,measure.cache_write,dim.operation) signature,r.effective,r.duplicate,r.archived_duplicate,r.time+r.ended ended,coalesce(usage_text(d.key),r.archived_duplicate) duplicate_of,r.sequence local_sequence,
      json_object('_usageRow',r.rid,'attempts',json_array(json_object('operation',CASE dim.operation WHEN 1 THEN 'compaction' WHEN 2 THEN 'web_search' ELSE 'model' END,'compactionKind',CASE WHEN r.compaction THEN 'legacy' ELSE NULL END,'price',CASE WHEN measure.cost IS NULL THEN NULL ELSE json('{}') END)), 'gatewayReported',CASE WHEN r.original THEN json('{}') ELSE NULL END,'mergedSources',CASE WHEN EXISTS(SELECT 1 FROM v12_rows child WHERE child.duplicate=r.rid) THEN json('[\"session\"]') ELSE json('[]') END) body
      FROM v12_records r LEFT JOIN v12_attempt_rows a ON a.owner=r.rid AND a.ordinal=r.last LEFT JOIN v12_session_rows b ON b.owner=r.rid AND b.ordinal=r.last LEFT JOIN v12_dimensions dim ON dim.shape=coalesce(a.shape,b.shape) LEFT JOIN v12_measures measure ON measure.id=coalesce(a.measure,b.measure) LEFT JOIN v12_dictionary p ON p.id=dim.provider LEFT JOIN v12_dictionary m ON m.id=dim.model LEFT JOIN v12_dictionary s ON s.id=coalesce(a.shape,b.shape) LEFT JOIN v12_rows d ON d.rid=r.duplicate;") )?;
    let original = metric_select("v12_raw_attempts", 2)
        + " WHERE EXISTS(SELECT 1 FROM v12_original o WHERE o.owner=r.rid AND o.metrics_changed)";
    db(c.execute_batch(&format!("CREATE VIEW usage_metrics AS {} UNION ALL {} UNION ALL {original} UNION ALL SELECT * FROM usage_daily_metrics;",metric_select("v12_attempts",0),metric_select("v12_sessions",0))))?;
    for column in ["provider", "model", "operation"] {
        db(c.execute_batch(&format!(
            "CREATE INDEX v12_dimensions_{column} ON v12_dimensions({column});"
        )))?;
    }
    Ok(())
}
pub(super) fn register_values(c: &Connection) -> Result<()> {
    use rusqlite::functions::FunctionFlags;
    db(c.create_scalar_function(
        "usage_generation_ms",
        2,
        FunctionFlags::SQLITE_UTF8,
        |ctx| {
            // The borrowed handle stays on this SQLite callback's thread and
            // only reads the existing bounded diagnostic storage.
            let c = unsafe { ctx.get_connection()? };
            let value = unpack_diagnostic(&c, ctx.get(0)?, ctx.get(1)?)
                .map_err(|_| rusqlite::Error::InvalidQuery)?;
            Ok(value["generationMs"].as_u64())
        },
    ))?;
    db(c.create_scalar_function(
        "usage_app_outcome",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let bytes: Option<Vec<u8>> = ctx.get(0)?;
            Ok(bytes
                .and_then(|b| shared_value(&b))
                .and_then(|v| match v["outcome"].as_str() {
                    Some("success") => Some(1),
                    Some("failed" | "cancelled" | "rejected") => Some(0),
                    _ => None,
                }))
        },
    ))?;
    db(c.create_scalar_function(
        "usage_value",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let b: Option<Vec<u8>> = ctx.get(0)?;
            b.map(|b| {
                shared_value(&b)
                    .and_then(|v| v.as_str().map(str::to_string))
                    .ok_or(rusqlite::Error::InvalidQuery)
            })
            .transpose()
        },
    ))
}
fn has_room(c: &Connection) -> Result<bool> {
    super::maintenance::has_room(c, 2)
}
fn validate(c: &Connection) -> Result<()> {
    let missing:bool=db(c.query_row("SELECT EXISTS(SELECT 1 FROM records r WHERE NOT EXISTS(SELECT 1 FROM v12_records n WHERE usage_hint(n.key)=usage_hint(usage_key(r.id)) AND n.key=usage_key(r.id))) OR EXISTS(SELECT 1 FROM v12_records n WHERE NOT EXISTS(SELECT 1 FROM records r WHERE r.id=usage_text(n.key)))",[],|r|r.get(0)))?;
    if missing {
        return Err(failure("用量迁移记录集合核对失败，旧数据已保留"));
    }
    let metadata_differs: bool = db(c.query_row("WITH old AS (
      SELECT id,client,source,time,response,usage_hint(usage_key(signature)),effective,duplicate_of,local_sequence FROM records), new AS (
      SELECT usage_text(r.key),usage_label(r.client),usage_label(r.source),r.time,usage_text(r.response),r.signature,r.effective,coalesce(usage_text(d.key),usage_text(l.target),r.archived_duplicate),r.sequence FROM v12_records r LEFT JOIN v12_rows d ON d.rid=r.duplicate LEFT JOIN v12_links l ON l.owner=r.rid)
      SELECT EXISTS(SELECT * FROM old EXCEPT SELECT * FROM new) OR EXISTS(SELECT * FROM new EXCEPT SELECT * FROM old)",[],|r|r.get(0)))?;
    if metadata_differs {
        return Err(failure("用量迁移来源、顺序或关联核对失败，旧数据已保留"));
    }
    // Earlier batches may have been committed in another process. Re-read every
    // detail in this final transaction so a damaged body or missing shared shape
    // cannot pass the SQL facts comparison and discard the authoritative layout.
    // Keep one record in memory at a time, including all of its attempts.
    let mut details = db(c.prepare("SELECT rid FROM v12_rows ORDER BY rid"))?;
    let mut rows = db(details.query([]))?;
    while let Some(row) = db(rows.next())? {
        let _: Record = serde_json::from_value(load(c, db(row.get(0))?)?).map_err(json_error)?;
    }
    let bad:bool=db(c.query_row("SELECT EXISTS(SELECT 1 FROM v12_links l WHERE NOT EXISTS(SELECT 1 FROM v12_records r WHERE usage_hint(r.key)=usage_hint(l.target) AND r.key=l.target) AND NOT EXISTS(SELECT 1 FROM receipts r WHERE r.id=usage_text(l.target)))",[],|r|r.get(0)))?;
    if bad {
        return Err(failure("用量迁移关联核对失败，旧数据已保留"));
    }
    let columns = {
        let mut stmt = db(c.prepare("SELECT name FROM pragma_table_info('usage_metrics') WHERE name NOT IN ('kind','owner') ORDER BY cid"))?;
        let rows = db(stmt.query_map([], |r| r.get::<_, String>(0)))?;
        db(rows.collect::<rusqlite::Result<Vec<_>>>())?
    };
    let fields = columns
        .iter()
        .map(|n| format!("s.{n}"))
        .collect::<Vec<_>>()
        .join(",");
    // Compare facts per logical row and attempt, including NULL versus real zero.
    // Historical daily rows are retained unchanged and are not regenerated.
    for raw in [false, true] {
        let projected = metric_select(
            if raw {
                "v12_raw_attempts"
            } else {
                "v12_all_attempts"
            },
            0,
        );
        let shadow = format!("SELECT usage_text(r.key) owner,{fields} FROM ({projected}) s JOIN v12_records r ON r.rid=s.owner{}",if raw {" WHERE r.source=0"}else{""});
        let legacy = format!(
            "SELECT s.owner,{fields} FROM usage_metrics s{} WHERE {}",
            if raw {
                " JOIN records r ON r.id=s.owner"
            } else {
                ""
            },
            if raw {
                "r.source='proxy' AND s.kind=CASE WHEN EXISTS(SELECT 1 FROM usage_metrics g WHERE g.kind=2 AND g.owner=r.id) THEN 2 ELSE 0 END"
            } else {
                "s.kind=0"
            }
        );
        let differs: bool = db(c.query_row(&format!("WITH old AS ({legacy}), new AS ({shadow}) SELECT EXISTS(SELECT * FROM old EXCEPT SELECT * FROM new) OR EXISTS(SELECT * FROM new EXCEPT SELECT * FROM old)"),[],|r|r.get(0)))?;
        if differs {
            return Err(failure("用量迁移计量核对失败，旧数据已保留"));
        }
    }
    Ok(())
}
pub(super) fn copy_batch(c: &mut Connection) -> Result<bool> {
    if !pending(c) {
        return Ok(false);
    }
    if !has_room(c)? {
        return Err(failure("用量存储整理等待可用磁盘空间，现有记录继续保存"));
    }
    let tx = db(c.transaction())?;
    let after: String = db(tx.query_row(
        "SELECT value FROM metadata WHERE key='storage_v12_after'",
        [],
        |r| r.get(0),
    ))?;
    let rows = {
        let mut s=db(tx.prepare("SELECT id,body,effective,local_sequence FROM records WHERE id>?1 ORDER BY id LIMIT 2000"))?;
        let it = db(s.query_map([&after], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
            ))
        }))?;
        db(it.collect::<rusqlite::Result<Vec<_>>>())?
    };
    for (_, body, effective, sequence) in &rows {
        let r: Record = bodies::decode(&tx, body)?;
        save(&tx, &r, *effective, *sequence)?;
        verify_record(&tx, &r)?;
    }
    if let Some(last) = rows.last() {
        db(tx.execute(
            "UPDATE metadata SET value=?1 WHERE key='storage_v12_after'",
            [&last.0],
        ))?;
    } else {
        if flush_batch(&tx)? {
            db(tx.commit())?;
            return Ok(true);
        }
        validate(&tx)?;
        db(tx.execute_batch("UPDATE v12_records SET duplicate=(SELECT target.rid FROM v12_links l JOIN v12_records target ON usage_hint(target.key)=usage_hint(l.target) AND target.key=l.target WHERE l.owner=v12_records.rid),archived_duplicate=(SELECT usage_text(l.target) FROM v12_links l WHERE l.owner=v12_records.rid AND NOT EXISTS(SELECT 1 FROM v12_records target WHERE usage_hint(target.key)=usage_hint(l.target) AND target.key=l.target)) WHERE rid IN (SELECT owner FROM v12_links); DELETE FROM v12_links WHERE owner IN (SELECT rid FROM v12_records WHERE duplicate IS NOT NULL);
          DROP TRIGGER v12_track_insert; DROP TRIGGER v12_track_update; DROP TRIGGER v12_track_delete;
          DROP TRIGGER IF EXISTS pending_prices_insert; DROP TRIGGER IF EXISTS pending_prices_update; DROP TRIGGER IF EXISTS pending_prices_delete;
          DROP TABLE pending_prices;
          DELETE FROM usage_metrics WHERE kind<>1;
          ALTER TABLE usage_metrics RENAME TO usage_daily_metrics;
          DROP TABLE records;
          DROP TABLE v12_dirty;
          UPDATE metadata SET value='ready' WHERE key='storage_v12';
          DELETE FROM metadata WHERE key IN ('storage_v12_after','storage_records_after');
          INSERT OR REPLACE INTO metadata VALUES('storage_repack','1');"))?;
        create_views(&tx)?;
        db(tx.execute_batch("CREATE INDEX v12_daily_unpacked ON daily(id) WHERE json_extract(body,'$._usageArchive') IS NULL;"))?;
        query::changed(&tx)?;
    }
    db(tx.commit())?;
    Ok(true)
}
fn verify_record(c: &Connection, r: &Record) -> Result<()> {
    let id = owner(c, &r.id)?.ok_or_else(|| failure("用量迁移记录缺失"))?;
    let decoded: Record = serde_json::from_value(load(c, id)?).map_err(json_error)?;
    if serde_json::to_value(&decoded).map_err(json_error)?
        != serde_json::to_value(r).map_err(json_error)?
    {
        return Err(failure("用量迁移字段核对失败，旧数据已保留"));
    }
    Ok(())
}
pub(super) fn record(c: &Connection, id: &str) -> Result<Option<Record>> {
    if active(c) {
        owner(c, id)?
            .map(|id| serde_json::from_value(load(c, id)?).map_err(json_error))
            .transpose()
    } else {
        let body: Option<String> = db(c
            .query_row("SELECT body FROM records WHERE id=?1", [id], |r| r.get(0))
            .optional())?;
        body.map(|v| bodies::decode(c, &v)).transpose()
    }
}
pub(super) fn update(c: &Connection, r: &Record) -> Result<()> {
    if active(c) {
        let (effective, seq): (i64, i64) = db(c.query_row(
            "SELECT effective,sequence FROM v12_records WHERE usage_hint(key)=usage_hint(?1) AND key=?1",
            [key(&r.id)],
            |r| Ok((r.get(0)?, r.get(1)?)),
        ))?;
        save(c, r, effective, seq)
    } else {
        db(c.execute(
            "UPDATE records SET body=?2,signature=?3,model=?4,time=?5 WHERE id=?1",
            params![
                r.id,
                bodies::encode(c, r)?,
                r.signature(),
                r.final_attempt().and_then(|a| a.grouping_model()),
                r.event_time()
            ],
        ))?;
        Ok(())
    }
}
pub(super) fn candidates(c: &Connection, seed: &Record) -> Result<Vec<Record>> {
    let response = seed
        .final_attempt()
        .and_then(|a| a.response_id.as_deref())
        .map(key);
    let [sig, alternate] = seed
        .correlation_signatures()
        .map(|s| s.map(|v| hint(&key(&v))));
    // A session can only match a gateway. Use that equality in the indexed
    // lookup: a long session often contains thousands of identical deltas.
    let sources = if seed.source == "proxy" {
        "source NOT IN (0,3)"
    } else {
        "source=0"
    };
    let mut s=db(c.prepare(&format!("SELECT rid FROM v12_records WHERE usage_hint(key)=usage_hint(?1) AND key=?1 UNION SELECT rid FROM v12_records WHERE ?3 IS NOT NULL AND client=usage_label(?2) AND response IS NOT NULL AND usage_hint(response)=usage_hint(?3) AND response=?3 UNION SELECT rid FROM v12_records WHERE ?4 IS NOT NULL AND client=usage_label(?2) AND signature IN (?4,?8) AND {sources} AND source<>usage_label(?5) AND time BETWEEN ?6 AND ?7")))?;
    let rows = db(s.query_map(
        params![
            key(&seed.id),
            seed.client,
            response,
            sig,
            seed.source,
            seed.final_attempt()
                .map(|a| a.started_at.saturating_add(a.duration_ms as i64))
                .unwrap_or(seed.started_at)
                .saturating_sub(1_200_000 + 86_400_000),
            seed.final_attempt()
                .map(|a| a.started_at.saturating_add(a.duration_ms as i64))
                .unwrap_or(seed.started_at)
                .saturating_add(1_200_000),
            alternate
        ],
        |r| r.get::<_, i64>(0),
    ))?;
    rows.map(|r| serde_json::from_value(load(c, db(r)?)?).map_err(json_error))
        .collect()
}
pub(super) fn correlation_repair_rows(c: &Connection, after: i64) -> Result<Vec<(i64, Record)>> {
    let mut statement =
        db(c.prepare("SELECT rid FROM v12_records WHERE rid>?1 ORDER BY rid LIMIT 128"))?;
    let ids = db(statement.query_map([after], |r| r.get::<_, i64>(0)))?;
    ids.map(|id| {
        let id = db(id)?;
        Ok((
            id,
            serde_json::from_value(load(c, id)?).map_err(json_error)?,
        ))
    })
    .collect()
}
pub(super) fn peers(c: &Connection, id: &str) -> Result<Vec<Record>> {
    let Some(owner) = owner(c, id)? else {
        return Ok(vec![]);
    };
    let mut s = db(c.prepare(
        "SELECT rid FROM v12_records WHERE duplicate=?1 AND source<>0 ORDER BY usage_text(key)",
    ))?;
    let rows = db(s.query_map([owner], |r| r.get::<_, i64>(0)))?;
    rows.map(|r| serde_json::from_value(load(c, db(r)?)?).map_err(json_error))
        .collect()
}
pub(super) fn set_state(c: &Connection, r: &Record, effective: i64) -> Result<()> {
    let sequence: i64 = db(c.query_row(
        "SELECT sequence FROM v12_records WHERE usage_hint(key)=usage_hint(?1) AND key=?1",
        [key(&r.id)],
        |r| r.get(0),
    ))?;
    save(c, r, effective, sequence)
}
// archive_ids is a bounded, connection-local selection owned by Store::archive_batch.
pub(super) fn archive_selected(c: &Connection) -> Result<()> {
    db(c.execute("INSERT OR IGNORE INTO receipts(id,client,source,time,provider,response,signature,ended,effective,operation) SELECT id,client,source,time,provider,response,signature,ended,effective,coalesce(json_extract(body,'$.attempts[#-1].operation'),'model') FROM records WHERE rid IN (SELECT rid FROM archive_ids)",[]))?;
    for table in [
        "v12_attempt_rows",
        "v12_session_rows",
        "v12_original",
        "v12_links",
    ] {
        db(c.execute(
            &format!("DELETE FROM {table} WHERE owner IN (SELECT rid FROM archive_ids)"),
            [],
        ))?;
    }
    db(c.execute("UPDATE v12_records SET archived_duplicate=(SELECT usage_text(target.key) FROM v12_records target WHERE target.rid=v12_records.duplicate),duplicate=NULL WHERE duplicate IN (SELECT rid FROM archive_ids)",[]))?;
    db(c.execute(
        "DELETE FROM v12_records WHERE rid IN (SELECT rid FROM archive_ids)",
        [],
    ))?;
    Ok(())
}
pub(super) fn refresh_metrics(c: &Connection) -> Result<()> {
    if !active(c) {
        return Ok(());
    }
    let sql: String = db(c.query_row(
        "SELECT sql FROM sqlite_master WHERE name='usage_metrics'",
        [],
        |r| r.get(0),
    ))?;
    if sql.contains("usage_generation_ms") {
        return Ok(());
    }
    let original = metric_select("v12_raw_attempts", 2)
        + " WHERE EXISTS(SELECT 1 FROM v12_original o WHERE o.owner=r.rid AND o.metrics_changed)";
    db(c.execute_batch(&format!("DROP VIEW usage_metrics; CREATE VIEW usage_metrics AS {} UNION ALL {} UNION ALL {original} UNION ALL SELECT * FROM usage_daily_metrics;",metric_select("v12_attempts",0),metric_select("v12_sessions",0))))
}
pub(super) fn prune(c: &Connection) -> Result<()> {
    db(c.execute("DELETE FROM v12_archives WHERE rid NOT IN (SELECT json_extract(body,'$._usageArchive') FROM daily WHERE json_extract(body,'$._usageArchive') IS NOT NULL)",[]))?;
    diagnostics::prune(c)?;
    db(c.execute_batch("DELETE FROM v12_measures WHERE id NOT IN (SELECT measure FROM v12_attempt_rows UNION SELECT measure FROM v12_session_rows); DELETE FROM v12_dimensions WHERE shape NOT IN (SELECT shape FROM v12_attempt_rows UNION SELECT shape FROM v12_session_rows);"))?;
    db(c.execute_batch("DELETE FROM v12_dictionary WHERE id NOT IN (SELECT shape FROM v12_records UNION SELECT shape FROM v12_attempts UNION SELECT provider FROM v12_attempts WHERE provider IS NOT NULL UNION SELECT model FROM v12_attempts WHERE model IS NOT NULL UNION SELECT shape FROM v12_sessions UNION SELECT provider FROM v12_sessions WHERE provider IS NOT NULL UNION SELECT model FROM v12_sessions WHERE model IS NOT NULL UNION SELECT shape FROM v12_original WHERE shape IS NOT NULL UNION SELECT provider FROM v12_original WHERE provider IS NOT NULL UNION SELECT model FROM v12_original WHERE model IS NOT NULL);
      DELETE FROM v12_record_states WHERE id NOT IN (SELECT state FROM v12_rows); DELETE FROM v12_templates WHERE id NOT IN (SELECT usage_template(body) FROM v12_records WHERE usage_template(body) IS NOT NULL UNION SELECT usage_template(body) FROM v12_attempts WHERE usage_template(body) IS NOT NULL UNION SELECT usage_template(body) FROM v12_sessions WHERE usage_template(body) IS NOT NULL UNION SELECT usage_template(body) FROM v12_original WHERE usage_template(body) IS NOT NULL UNION SELECT usage_template(body) FROM v12_archives WHERE usage_template(body) IS NOT NULL UNION SELECT j.value FROM v12_diagnostics d,json_each(d.templates) j);"))?;
    Ok(())
}
pub(super) fn compact_diagnostics(c: &Connection) -> Result<bool> {
    if compact_archives(c)? {
        return Ok(true);
    }
    diagnostics::compact(c)
}
pub(super) fn packing_pending(c: &Connection) -> Result<bool> {
    if !active(c) {
        return Ok(false);
    }
    Ok(db(c.query_row(
        "SELECT EXISTS(SELECT 1 FROM daily WHERE json_extract(body,'$._usageArchive') IS NULL)",
        [],
        |r| r.get::<_, bool>(0),
    ))? || diagnostics::pending(c)?)
}
pub(super) fn save_archive(c: &Connection, value: &Value) -> Result<i64> {
    let body = pack_diagnostic(c, value.clone(), 0)?;
    db(c.execute("INSERT INTO v12_archives(body) VALUES(?1)", [body]))?;
    Ok(c.last_insert_rowid())
}
pub(super) fn load_archive(c: &Connection, id: i64) -> Result<Value> {
    let body = db(
        c.query_row("SELECT body FROM v12_archives WHERE rid=?1", [id], |r| {
            r.get(0)
        }),
    )?;
    unpack_diagnostic(c, body, 0)
}
fn compact_archives(c: &Connection) -> Result<bool> {
    let tx = db(c.unchecked_transaction())?;
    let rows = {
        let mut stmt=db(tx.prepare("SELECT id,body FROM daily WHERE json_extract(body,'$._usageArchive') IS NULL LIMIT 2000"))?;
        let rows =
            db(stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))))?;
        db(rows.collect::<rusqlite::Result<Vec<_>>>())?
    };
    if !rows.is_empty() && !has_room(&tx)? {
        return Err(failure("用量历史整理等待可用磁盘空间，原数据已保留"));
    }
    for (id, body) in &rows {
        let value: Value = bodies::decode(&tx, body)?;
        if value.get("example").is_none() {
            return Err(failure("用量历史归档缺少依据，原数据已保留"));
        }
        let packed = bodies::encode(&tx, &value)?;
        let restored: Value = bodies::decode(&tx, &packed)?;
        if restored != value {
            return Err(failure("用量历史归档核对失败，原数据已保留"));
        }
        db(tx.execute("UPDATE daily SET body=?2 WHERE id=?1", params![id, packed]))?;
    }
    db(tx.commit())?;
    Ok(!rows.is_empty())
}

pub(super) fn encode_stage(r: &Record) -> Result<Vec<u8>> {
    pack(&serde_json::to_value(r).map_err(json_error)?)
}
pub(super) fn decode_stage(bytes: &[u8]) -> Result<Record> {
    serde_json::from_value(unpack(bytes)?).map_err(json_error)
}
pub(super) fn remove_source(c: &Connection, source: &str) -> Result<()> {
    for table in [
        "v12_attempt_rows",
        "v12_session_rows",
        "v12_original",
        "v12_links",
    ] {
        db(c.execute(
            &format!(
                "DELETE FROM {table} WHERE owner IN (SELECT rid FROM v12_records WHERE source=usage_label(?1))"
            ),
            [source],
        ))?;
    }
    db(c.execute(
        "DELETE FROM v12_records WHERE source=usage_label(?1)",
        [source],
    ))?;
    db(c.execute("UPDATE v12_records SET effective=1,duplicate=NULL,archived_duplicate=NULL WHERE duplicate IS NOT NULL AND duplicate NOT IN (SELECT rid FROM v12_records)",[]))?;
    db(c.execute("DELETE FROM v12_links WHERE NOT EXISTS(SELECT 1 FROM v12_records r WHERE usage_hint(r.key)=usage_hint(v12_links.target) AND r.key=v12_links.target) AND NOT EXISTS(SELECT 1 FROM receipts r WHERE r.id=usage_text(v12_links.target))",[]))?;
    Ok(())
}
pub(super) fn cold_batch(c: &mut Connection, at: i64) -> Result<bool> {
    if !active(c) {
        return Ok(false);
    }
    let tx = db(c.transaction())?;
    let after: i64 = db(tx.query_row(
        "SELECT coalesce((SELECT cast(value AS INTEGER) FROM metadata WHERE key='cold_after'),0)",
        [],
        |r| r.get(0),
    ))?;
    let ids = {
        let mut s=db(tx.prepare("SELECT rid FROM v12_records WHERE rid>?1 AND time<?2 AND archived=0 AND effective=1 AND client NOT IN (usage_label('claude'),usage_label('antigravity')) ORDER BY rid LIMIT 2000"))?;
        let it = db(s.query_map(params![after, at - 7 * 86_400_000], |r| r.get::<_, i64>(0)))?;
        db(it.collect::<rusqlite::Result<Vec<_>>>())?
    };
    for id in &ids {
        let mut r: Record = serde_json::from_value(load(&tx, *id)?).map_err(json_error)?;
        let clean = |a: &Attempt| {
            a.tokens.input.is_some()
                && a.tokens.output.is_some()
                && a.price.is_some()
                && a.unpriced_count() == 0
                && !matches!(a.usage_status.as_str(), "parse_incomplete" | "ended_early")
                && a.status.is_none_or(|s| (200..300).contains(&s) || s == 101)
                && matches!(a.outcome.as_str(), "success" | "completed")
                && a.end
                    .as_ref()
                    .is_none_or(|e| e.cause == EndCause::Completed)
                && a.transfer.as_ref().is_none_or(|t| {
                    t.downstream_error.is_none()
                        && t.incomplete_reason.is_none()
                        && t.timeout_phase.is_none()
                        && t.terminal_handed_off
                        && t.terminal_status
                            .as_deref()
                            .is_none_or(|s| s == "completed")
                        && t.terminal_event.as_deref().is_none_or(|event| {
                            matches!(
                                event,
                                "response.completed" | "response.done" | "message_stop" | "message"
                            )
                        })
                        && t.upstream_end.as_deref().is_none_or(|end| end == "EOF")
                        && t.ws_close_code
                            .is_none_or(|code| matches!(code, 1000 | 1001))
                })
        };
        if r.attempts.iter().chain(r.gateway_reported.iter()).any(|a| {
            matches!(
                a.routing_outcome,
                Some(
                    crate::events::RoutingOutcome::SaveFailed
                        | crate::events::RoutingOutcome::Failed
                        | crate::events::RoutingOutcome::Attempted
                        | crate::events::RoutingOutcome::Unknown
                )
            )
        }) || !r.completed
            || !r.attempts.iter().all(clean)
            || r.gateway_reported.as_ref().is_some_and(|a| !clean(a))
            || r.failover_evidence
                .as_ref()
                .is_some_and(|v| v.failure.is_some())
        {
            continue;
        }
        r.finished_at = Some(r.event_time());
        let mut changed = false;
        for a in r.attempts.iter_mut().chain(r.gateway_reported.iter_mut()) {
            changed |= a.thread_hash.take().is_some();
            changed |= a.upstream_request_hash.take().is_some();
            changed |= a.routing_evidence.take().is_some();
            if let Some(t) = &mut a.transfer {
                changed |= t.connection.take().is_some();
                changed |= t.client_request_hash.take().is_some();
                changed |= t.upstream_client_request_hash.take().is_some();
                changed |= t.response_hash.take().is_some();
            }
        }
        if changed {
            r.detail_archived = true;
            update(&tx, &r)?;
        }
    }
    if let Some(last) = ids.last() {
        db(tx.execute(
            "INSERT OR REPLACE INTO metadata VALUES('cold_after',?1)",
            [last.to_string()],
        ))?;
    } else {
        db(tx.execute("DELETE FROM metadata WHERE key='cold_after'", []))?;
    }
    if !ids.is_empty() {
        query::changed(&tx)?;
    }
    db(tx.commit())?;
    compact_diagnostics(c)?;
    Ok(!ids.is_empty())
}
