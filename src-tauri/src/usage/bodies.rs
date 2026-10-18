//! Compact immutable metadata without changing records, SQL filters or IPC shapes.
use super::model::failure;
use crate::storage::{self, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use rusqlite::{params, Connection};
use serde::{de::DeserializeOwned, Serialize};
use serde_json::Value;
use std::io::{Read, Write};

#[path = "body_compact.rs"]
mod compact;
use super::body_wire as wire;

const MAX_BODY: usize = 16 * 1024 * 1024;
const CODEC: &str = "zstd-compact-v2";
const DICTIONARY_CODEC: &str = "zstd-dict-v1";
const LEGACY_CODEC: &str = "zstd-v1";
// Immutable, synthetic field names and empty shapes, never user records. A new
// dictionary must get its own codec version so old records remain readable.
const DICTIONARY: &[u8] = include_bytes!("body-dictionary-v1.json");
fn compact_dictionary() -> &'static [u8] {
    static DICTIONARY_V2: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
    DICTIONARY_V2.get_or_init(|| {
        let examples: Vec<Value> =
            serde_json::from_slice(DICTIONARY).expect("immutable dictionary");
        examples
            .iter()
            .flat_map(|v| wire::encode(&compact::compact(v, &compact::shape(v))))
            .collect()
    })
}

// SQL needs only these small projections; pages/details restore the lossless
// payload. Keep JSON expressions indexable without decompressing every record.
fn projection(value: &Value) -> Value {
    let mut projected = serde_json::Map::new();
    if let Some(example) = value.get("example") {
        for key in ["scope", "client", "provider", "model", "source", "status"] {
            if let Some(field) = value.get(key) {
                projected.insert(key.into(), field.clone());
            }
        }
        projected.insert("example".into(), projection(example));
        projected.insert(
            "totals".into(),
            serde_json::json!({"unpriced": value.pointer("/totals/unpriced")}),
        );
    } else {
        if let Some(attempts) = value.get("attempts").and_then(Value::as_array) {
            projected.insert(
                "attempts".into(),
                Value::Array(
                    attempts
                        .iter()
                        .map(|attempt| {
                            let mut fields = serde_json::Map::new();
                            for key in ["operation", "compactionKind"] {
                                if let Some(field) = attempt
                                    .get(key)
                                    .filter(|v| !v.is_null() && v.as_str() != Some("model"))
                                {
                                    fields.insert(key.into(), field.clone());
                                }
                            }
                            if attempt.get("price").is_some_and(Value::is_object) {
                                fields.insert("price".into(), serde_json::json!({}));
                            }
                            Value::Object(fields)
                        })
                        .collect(),
                ),
            );
        }
        if value.get("gatewayReported").is_some_and(Value::is_object) {
            projected.insert("gatewayReported".into(), serde_json::json!({}));
        }
        if let Some(sources) = value
            .get("mergedSources")
            .filter(|v| v.as_array().is_some_and(|v| !v.is_empty()))
        {
            projected.insert("mergedSources".into(), sources.clone());
        }
    }
    Value::Object(projected)
}

fn pack(connection: &Connection, value: &Value) -> Result<String> {
    if value.get("example").is_some() && super::storage_v12::active(connection) {
        let id = super::storage_v12::save_archive(connection, value)?;
        let mut projected = projection(value);
        projected["_usageArchive"] = id.into();
        return serde_json::to_string(&projected).map_err(|_| failure("用量归档元数据无效"));
    }
    let plain = serde_json::to_string(value).map_err(|_| failure("用量元数据无效"))?;
    let version: i64 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(|_| failure("用量数据库版本读取失败"))?;
    // Never make tiny records larger, and keep unusual legacy records readable.
    // Earlier schema migrations may commit before v11 is installed. Do not
    // write a new codec until the old-application version guard is durable.
    if version < 11 || plain.len() < 512 || plain.len() > MAX_BODY {
        return Ok(plain);
    }
    let mut encoder =
        zstd::stream::write::Encoder::with_dictionary(Vec::new(), 6, compact_dictionary())
            .map_err(|_| failure("用量记录压缩失败"))?;
    encoder
        .include_checksum(true)
        .map_err(|_| failure("用量记录压缩失败"))?;
    let shape = compact::shape(value);
    let shape_body = serde_json::to_string(&shape).map_err(|_| failure("用量元数据无效"))?;
    let hash = storage::digest(shape_body.as_bytes());
    connection
        .execute(
            "INSERT OR IGNORE INTO body_shapes(hash,body) VALUES(?1,?2)",
            params![hash, shape_body],
        )
        .map_err(|_| failure("用量共享元数据保存失败"))?;
    let shape_id: i64 = connection
        .query_row("SELECT id FROM body_shapes WHERE hash=?1", [&hash], |row| {
            row.get(0)
        })
        .map_err(|_| failure("用量共享元数据读取失败"))?;
    let compact = wire::encode(&compact::compact(value, &shape));
    if compact.len() > MAX_BODY {
        return Ok(plain);
    }
    encoder
        .write_all(&compact)
        .map_err(|_| failure("用量记录压缩失败"))?;
    let packed = encoder.finish().map_err(|_| failure("用量记录压缩失败"))?;
    let mut projected = projection(value);
    let object = projected.as_object_mut().expect("projection is an object");
    object.insert("_usageCodec".into(), Value::String(CODEC.into()));
    object.insert("_usageShape".into(), shape_id.into());
    object.insert("_usageBody".into(), Value::String(STANDARD.encode(packed)));
    let encoded = serde_json::to_string(&projected).map_err(|_| failure("用量元数据无效"))?;
    Ok(if encoded.len() < plain.len() {
        encoded
    } else {
        plain
    })
}

pub(super) fn unpack(connection: &Connection, body: &str) -> Result<Value> {
    let value: Value = serde_json::from_str(body).map_err(|_| failure("用量元数据无效"))?;
    if let Some(id) = value.get("_usageRow").and_then(Value::as_i64) {
        return super::storage_v12::load(connection, id);
    }
    if let Some(id) = value.get("_usageArchive").and_then(Value::as_i64) {
        return super::storage_v12::load_archive(connection, id);
    }
    let Some(codec) = value.get("_usageCodec") else {
        return Ok(value);
    };
    let dictionary = match codec.as_str() {
        Some(CODEC) => compact_dictionary(),
        Some(DICTIONARY_CODEC) => DICTIONARY,
        Some(LEGACY_CODEC) => &[],
        _ => return Err(failure("用量记录编码不支持，原记录已保留")),
    };
    let encoded = value
        .get("_usageBody")
        .and_then(Value::as_str)
        .filter(|s| s.len() <= MAX_BODY * 2)
        .ok_or_else(|| failure("用量记录压缩数据无效，原记录已保留"))?;
    let packed = STANDARD
        .decode(encoded)
        .map_err(|_| failure("用量记录压缩数据无效，原记录已保留"))?;
    let decoder = zstd::stream::read::Decoder::with_dictionary(packed.as_slice(), dictionary)
        .map_err(|_| failure("用量记录解压失败，原记录已保留"))?;
    let mut plain = Vec::new();
    decoder
        .take((MAX_BODY + 1) as u64)
        .read_to_end(&mut plain)
        .map_err(|_| failure("用量记录解压失败，原记录已保留"))?;
    if plain.len() > MAX_BODY {
        return Err(failure("用量记录解压超过安全上限，原记录已保留"));
    }
    if codec.as_str() == Some(CODEC) {
        let unpacked =
            wire::decode(&plain).ok_or_else(|| failure("用量元数据无效，原记录已保留"))?;
        let shape_id = value
            .get("_usageShape")
            .and_then(Value::as_i64)
            .ok_or_else(|| failure("用量共享元数据无效，原记录已保留"))?;
        let (hash, shape): (String, String) = connection
            .query_row(
                "SELECT hash,body FROM body_shapes WHERE id=?1",
                [shape_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(|_| failure("用量共享元数据缺失，原记录已保留"))?;
        if storage::digest(shape.as_bytes()) != hash {
            return Err(failure("用量共享元数据损坏，原记录已保留"));
        }
        let shape: Value = serde_json::from_str(&shape)
            .map_err(|_| failure("用量共享元数据无效，原记录已保留"))?;
        compact::expand(&unpacked, &shape).ok_or_else(|| failure("用量元数据无效，原记录已保留"))
    } else {
        serde_json::from_slice(&plain).map_err(|_| failure("用量元数据无效，原记录已保留"))
    }
}

fn prices(value: &mut Value, apply: &mut impl FnMut(&mut Value) -> Result<()>) -> Result<()> {
    if let Some(example) = value.get_mut("example") {
        return prices(example, apply);
    }
    if let Some(attempts) = value.get_mut("attempts").and_then(Value::as_array_mut) {
        for attempt in attempts {
            if let Some(price) = attempt.get_mut("price").filter(|v| v.is_object()) {
                apply(price)?;
            }
        }
    }
    if let Some(price) = value
        .get_mut("gatewayReported")
        .and_then(|a| a.get_mut("price"))
        .filter(|v| v.is_object())
    {
        apply(price)?;
    }
    Ok(())
}

pub(super) fn encode(connection: &Connection, value: &impl Serialize) -> Result<String> {
    let mut value = serde_json::to_value(value).map_err(|_| failure("用量元数据无效"))?;
    // Duplicate session rows are evidence donors, not another gateway trace.
    // Retain every attempt's counters, identity, timing and price so a later
    // conflicting donor or source rebuild can still undo/re-evaluate the join.
    if value.get("duplicateOf").is_some_and(Value::is_string)
        && value
            .get("source")
            .and_then(Value::as_str)
            .is_some_and(|s| s != "proxy" && s != "grok")
    {
        if let Some(object) = value.as_object_mut() {
            object.remove("failoverEvidence");
            object.remove("gatewayReported");
        }
        if let Some(attempts) = value.get_mut("attempts").and_then(Value::as_array_mut) {
            for attempt in attempts {
                if let Some(object) = attempt.as_object_mut() {
                    for key in [
                        "transfer",
                        "end",
                        "threadHash",
                        "upstreamRequestHash",
                        "routingDecision",
                        "routingEvidence",
                    ] {
                        object.remove(key);
                    }
                }
            }
        }
    }
    prices(&mut value, &mut |price| {
        let Some(basis) = price.get("basis").filter(|v| !v.is_null()) else {
            return Ok(());
        };
        let body = serde_json::to_string(basis).map_err(|_| failure("价格快照无效"))?;
        let hash = storage::digest(body.as_bytes());
        connection
            .execute(
                "INSERT OR IGNORE INTO price_bases(hash,body) VALUES(?1,?2)",
                params![hash, body],
            )
            .map_err(|_| failure("价格快照保存失败"))?;
        let object = price.as_object_mut().expect("price object checked");
        object.remove("basis");
        object.insert("basisRef".into(), Value::String(hash));
        Ok(())
    })?;
    pack(connection, &value)
}

pub(super) fn prune_shapes(connection: &Connection) -> Result<()> {
    connection.execute("DELETE FROM body_shapes WHERE id NOT IN (SELECT json_extract(body,'$._usageShape') FROM records WHERE json_type(body,'$._usageShape')='integer' UNION SELECT json_extract(body,'$._usageShape') FROM daily WHERE json_type(body,'$._usageShape')='integer')", [])
        .map_err(|_| failure("用量共享元数据整理失败"))?;
    Ok(())
}

pub(super) fn decode<T: DeserializeOwned>(connection: &Connection, body: &str) -> Result<T> {
    let mut value = unpack(connection, body)?;
    prices(&mut value, &mut |price| {
        let Some(reference) = price.get("basisRef") else {
            return Ok(());
        };
        let reference = reference
            .as_str()
            .filter(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()))
            .ok_or_else(|| failure("价格快照引用无效，记录已保留"))?;
        let basis: String = connection
            .prepare_cached("SELECT body FROM price_bases WHERE hash=?1")
            .and_then(|mut statement| statement.query_row([reference], |row| row.get(0)))
            .map_err(|_| failure("价格快照缺失，记录已保留"))?;
        if storage::digest(basis.as_bytes()) != reference {
            return Err(failure("价格快照校验失败，记录已保留"));
        }
        let basis = serde_json::from_str(&basis).map_err(|_| failure("价格快照无效"))?;
        let object = price.as_object_mut().expect("price object checked");
        object.remove("basisRef");
        object.insert("basis".into(), basis);
        Ok(())
    })?;
    serde_json::from_value(value).map_err(|_| failure("用量元数据无效"))
}
