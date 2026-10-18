//! Frozen v0.25.5 encoder for synthetic migration fixtures, never production writes.
use super::model::failure;
use crate::storage::{self, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use rusqlite::{params, Connection};
use serde::Serialize;
use serde_json::Value;
use std::io::Write;

const MAX_BODY: usize = 16 * 1024 * 1024;
const CODEC: &str = "zstd-dict-v1";
// Immutable, synthetic field names and empty shapes, never user records. A new
// dictionary must get its own codec version so old records remain readable.
const DICTIONARY: &[u8] = include_bytes!("body-dictionary-v1.json");

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
                            for key in ["operation", "compactionKind", "durationMs"] {
                                if let Some(field) = attempt.get(key) {
                                    fields.insert(key.into(), field.clone());
                                }
                            }
                            fields.insert(
                                "price".into(),
                                if attempt.get("price").is_some_and(Value::is_object) {
                                    serde_json::json!({})
                                } else {
                                    Value::Null
                                },
                            );
                            Value::Object(fields)
                        })
                        .collect(),
                ),
            );
        }
        if value.get("gatewayReported").is_some_and(Value::is_object) {
            projected.insert("gatewayReported".into(), serde_json::json!({}));
        }
        if let Some(sources) = value.get("mergedSources") {
            projected.insert("mergedSources".into(), sources.clone());
        }
    }
    Value::Object(projected)
}

fn pack(value: &Value) -> Result<String> {
    let plain = serde_json::to_string(value).map_err(|_| failure("用量元数据无效"))?;
    // Never make tiny records larger, and keep unusual legacy records readable.
    if plain.len() < 512 || plain.len() > MAX_BODY {
        return Ok(plain);
    }
    let mut encoder = zstd::stream::write::Encoder::with_dictionary(Vec::new(), 6, DICTIONARY)
        .map_err(|_| failure("用量记录压缩失败"))?;
    encoder
        .include_checksum(true)
        .map_err(|_| failure("用量记录压缩失败"))?;
    encoder
        .write_all(plain.as_bytes())
        .map_err(|_| failure("用量记录压缩失败"))?;
    let packed = encoder.finish().map_err(|_| failure("用量记录压缩失败"))?;
    let mut projected = projection(value);
    let object = projected.as_object_mut().expect("projection is an object");
    object.insert("_usageCodec".into(), Value::String(CODEC.into()));
    object.insert("_usageBody".into(), Value::String(STANDARD.encode(packed)));
    let encoded = serde_json::to_string(&projected).map_err(|_| failure("用量元数据无效"))?;
    Ok(if encoded.len() < plain.len() {
        encoded
    } else {
        plain
    })
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
    pack(&value)
}
