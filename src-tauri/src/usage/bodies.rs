//! Compact immutable metadata without changing records, SQL filters or IPC shapes.
use super::model::failure;
use crate::storage::{self, Result};
use rusqlite::{params, Connection};
use serde::{de::DeserializeOwned, Serialize};
use serde_json::Value;

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
    if value.get("example").is_some() {
        let id = super::storage_v12::save_archive(connection, value)?;
        let mut projected = projection(value);
        projected["_usageArchive"] = id.into();
        return serde_json::to_string(&projected).map_err(|_| failure("用量归档元数据无效"));
    }
    serde_json::to_string(value).map_err(|_| failure("用量元数据无效"))
}

pub(super) fn unpack(connection: &Connection, body: &str) -> Result<Value> {
    let value: Value = serde_json::from_str(body).map_err(|_| failure("用量元数据无效"))?;
    if let Some(id) = value.get("_usageRow").and_then(Value::as_i64) {
        return super::storage_v12::load(connection, id);
    }
    if let Some(id) = value.get("_usageArchive").and_then(Value::as_i64) {
        return super::storage_v12::load_archive(connection, id);
    }
    if value.get("_usageCodec").is_some() {
        return Err(failure("用量记录编码不支持，原记录已保留"));
    }
    Ok(value)
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
        let basis = serde_json::from_str(&basis).map_err(|_| failure("价格快照无效"))?;
        let object = price.as_object_mut().expect("price object checked");
        object.remove("basisRef");
        object.insert("basis".into(), basis);
        Ok(())
    })?;
    serde_json::from_value(value).map_err(|_| failure("用量记录无效"))
}
