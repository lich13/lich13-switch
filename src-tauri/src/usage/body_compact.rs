//! Versioned, lossless storage shapes. These defaults belong to codec v2;
//! changing model defaults must not change how existing v2 bodies are decoded.
use serde_json::{json, Map, Value};

fn record_defaults() -> Value {
    json!({"id":"","client":"","source":"","startedAt":0,"finishedAt":null,
        "sessionId":null,"attempts":[],"completed":false,"estimatedSpeed":false,
        "duplicateOf":null,"deduplication":"","mergedSources":[]})
}

fn attempt_defaults(record: &Value) -> Value {
    json!({"id":record["id"],"provider":null,"requestedModel":null,"responseModel":null,
        "pricingModel":null,"responseId":null,"status":null,"outcome":"",
        "startedAt":record["startedAt"],"durationMs":0,"firstTokenMs":null,
        "stream":false,"transport":"","tokens":{"inclusiveInput":null,"input":null,
        "output":null,"cacheRead":null,"cacheWrite":null,"cacheWrite5m":null,
        "cacheWrite1h":null,"imageInput":null,"imageOutput":null,"audioInput":null,
        "audioOutput":null},"serviceTier":null,"price":null,"compactionKind":null,
        "usageStatus":"","availability":{},"usageSources":{},"pricingBasis":null,
        "mappingRevision":null,"repeatCount":1,"compactedUnpriced":null,
        "operation":"model","costMultiplier":"1"})
}

// Shared shapes contain stable metadata only. Request identities, counters,
// timings and diagnostic evidence always remain in each record's delta.
pub(super) fn shape(value: &Value) -> Value {
    if let Some(example) = value.get("example") {
        return shape(example);
    }
    fn pick(value: &Value, keys: &[&str]) -> Value {
        Value::Object(
            keys.iter()
                .filter_map(|key| value.get(*key).map(|v| ((*key).into(), v.clone())))
                .collect(),
        )
    }
    let mut shape = pick(
        value,
        &[
            "client",
            "source",
            "completed",
            "estimatedSpeed",
            "deduplication",
            "mergedSources",
        ],
    );
    shape["attempts"] = value
        .get("attempts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|attempt| {
            let mut shape = pick(
                attempt,
                &[
                    "provider",
                    "requestedModel",
                    "responseModel",
                    "pricingModel",
                    "status",
                    "outcome",
                    "stream",
                    "transport",
                    "serviceTier",
                    "compactionKind",
                    "usageStatus",
                    "availability",
                    "usageSources",
                    "pricingBasis",
                    "mappingRevision",
                    "operation",
                    "costMultiplier",
                ],
            );
            if let Some(price) = attempt.get("price").filter(|v| v.is_object()) {
                shape["price"] = pick(
                    price,
                    &[
                        "basisRef",
                        "model",
                        "multiplier",
                        "rates",
                        "source",
                        "version",
                    ],
                );
            }
            shape
        })
        .collect();
    shape
}
fn record_base(shape: &Value) -> Value {
    let mut base = record_defaults();
    if let Some(fields) = shape.as_object() {
        for (key, value) in fields {
            if key != "attempts" {
                base[key] = value.clone();
            }
        }
    }
    base
}
fn attempt_base(record: &Value, shape: &Value, ordinal: usize) -> Value {
    let mut base = attempt_defaults(record);
    if let Some(fields) = shape["attempts"].get(ordinal).and_then(Value::as_object) {
        for (key, value) in fields {
            base[key] = value.clone();
        }
    }
    base
}

// An object delta stores only changed members and explicit removed keys. It
// must distinguish absent, null and zero: each has different usage semantics.
fn difference(value: &Value, base: &Value) -> Value {
    let (Some(value), Some(base)) = (value.as_object(), base.as_object()) else {
        return json!([0, value]);
    };
    let mut fields = Map::new();
    for (key, value) in value {
        match base.get(key) {
            Some(old) if old == value => {}
            Some(old) => {
                fields.insert(key.clone(), difference(value, old));
            }
            None => {
                fields.insert(key.clone(), json!([0, value]));
            }
        }
    }
    let removed: Vec<_> = base
        .keys()
        .filter(|key| !value.contains_key(*key))
        .collect();
    json!([1, fields, removed])
}

fn restore(delta: &Value, base: &Value) -> Option<Value> {
    let parts = delta.as_array()?;
    if parts.first()?.as_u64() == Some(0) && parts.len() == 2 {
        return Some(parts[1].clone());
    }
    if parts.len() != 3 || parts[0].as_u64() != Some(1) {
        return None;
    }
    let fields = parts[1].as_object()?;
    let removed = parts[2].as_array()?;
    let mut value = base.as_object()?.clone();
    for key in removed {
        value.remove(key.as_str()?);
    }
    for (key, field) in fields {
        let next = restore(field, base.get(key).unwrap_or(&Value::Null))?;
        value.insert(key.clone(), next);
    }
    Some(Value::Object(value))
}

pub(super) fn compact(value: &Value, shape: &Value) -> Value {
    if value.get("example").is_some() {
        let mut daily = value.clone();
        daily["example"] = compact(&value["example"], shape);
        return daily;
    }
    let Some(attempts) = value.get("attempts").and_then(Value::as_array) else {
        return value.clone();
    };
    let mut record = value.clone();
    if let (Some(original), Some(last)) = (value.get("gatewayReported"), attempts.last()) {
        record["gatewayReported"] = difference(original, last);
    }
    record["attempts"] = attempts
        .iter()
        .enumerate()
        .map(|(i, a)| difference(a, &attempt_base(value, shape, i)))
        .collect();
    difference(&record, &record_base(shape))
}

pub(super) fn expand(value: &Value, shape: &Value) -> Option<Value> {
    if let Some(example) = value.get("example") {
        let mut daily = value.clone();
        daily["example"] = expand(example, shape)?;
        return Some(daily);
    }
    let mut record = restore(value, &record_base(shape))?;
    let attempts = record["attempts"]
        .as_array()?
        .iter()
        .enumerate()
        .map(|(i, a)| restore(a, &attempt_base(&record, shape, i)))
        .collect::<Option<Vec<_>>>()?;
    if let (Some(delta), Some(last)) = (record.get("gatewayReported"), attempts.last()) {
        record["gatewayReported"] = restore(delta, last)?;
    }
    record["attempts"] = Value::Array(attempts);
    Some(record)
}
