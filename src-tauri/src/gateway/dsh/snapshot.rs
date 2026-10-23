//! Long-lived snapshots contain only fields owned by the gateway. The existing
//! short-lived recovery transaction retains full files until commit/rollback.
use super::*;
use serde::{ser::Error as _, Serializer};
use serde_json::{json, Map, Value};

fn value(node: YamlNode) -> Result<Value> {
    if let Some(map) = node.as_mapping() {
        let mut result = Map::new();
        for (key, node) in map.iter() {
            let key = string(Some(key)).ok_or_else(|| error("DSH 受管字段含无效映射键"))?;
            result.insert(key, value(node)?);
        }
        return Ok(Value::Object(result));
    }
    if let Some(seq) = node.as_sequence() {
        return seq
            .values()
            .map(value)
            .collect::<Result<Vec<_>>>()
            .map(Value::Array);
    }
    let scalar = node
        .as_scalar()
        .ok_or_else(|| error("DSH 受管字段无法静态保存"))?;
    Ok(if scalar.is_null() {
        Value::Null
    } else if let Some(v) = scalar.as_bool() {
        v.into()
    } else if let Some(v) = scalar.as_i64() {
        v.into()
    } else if let Some(v) = scalar.as_f64() {
        json!(v)
    } else {
        scalar.as_string().into()
    })
}

fn config(id: &str, current: &Mapping) -> Result<Value> {
    let mut output = json!({});
    if id == PI {
        output["providers"] = json!({});
        if let Some(route) = current.get_mapping("providers").and_then(|m| m.get(ROUTE)) {
            output["providers"][ROUTE] = value(route)?;
        }
    } else {
        for key in ["provider", "model"] {
            if let Some(node) = current.get(key) {
                output[key] = value(node)?;
            }
        }
    }
    Ok(output)
}

fn project_patch(raw: &str) -> Result<String> {
    let seq = sequence(raw)?;
    let mut rows = vec![];
    for id in [PI, DEFAULT] {
        if let Some((_, row)) = find_row(&seq, id)? {
            let mut output = json!({"id":id});
            for key in ["name", "disabled"] {
                if let Some(node) = row.get(key) {
                    output[key] = value(node)?;
                }
            }
            if let Some(current) = row.get_mapping("config") {
                output["config"] = config(id, &current)?;
            }
            rows.push(output);
        }
    }
    Ok(json!(rows).to_string())
}
fn project_settings(raw: &str) -> Result<String> {
    let root = yaml(raw)?
        .as_mapping()
        .ok_or_else(|| error("DSH settings 必须是映射"))?;
    let mut output = json!({});
    for id in [PI, DEFAULT] {
        if let Some(current) = root.get_mapping(id) {
            output[id] = config(id, &current)?;
        }
    }
    Ok(output.to_string())
}
fn project_environment(raw: &str) -> Result<String> {
    Ok(env_value(raw)?
        .map(|v| format!("{v}\n"))
        .unwrap_or_default())
}
fn project_credentials(raw: &str) -> Result<String> {
    let mut output = json!({"version":1,"refs":{}});
    if let Some(node) = credentials(Some(raw))?
        .get_mapping("refs")
        .and_then(|m| m.get(KEY))
    {
        output["refs"][KEY] = value(node)?;
    }
    Ok(output.to_string())
}
fn projected(change: &Change, project: fn(&str) -> Result<String>) -> Result<Change> {
    Ok(Change {
        before: change.before.as_deref().map(project).transpose()?,
        after: project(&change.after)?,
    })
}
macro_rules! serializer {
    ($name:ident, $project:ident) => {
        pub(super) fn $name<S: Serializer>(
            change: &Change,
            serializer: S,
        ) -> std::result::Result<S::Ok, S::Error> {
            projected(change, $project)
                .map_err(S::Error::custom)?
                .serialize(serializer)
        }
    };
}
serializer!(patch, project_patch);
serializer!(environment, project_environment);
serializer!(credentials_change, project_credentials);
pub(super) fn home_patch<S: Serializer>(
    change: &Option<Change>,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error> {
    change
        .as_ref()
        .map(|v| projected(v, project_patch))
        .transpose()
        .map_err(S::Error::custom)?
        .serialize(serializer)
}
pub(super) fn settings<S: Serializer>(
    change: &Option<Change>,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error> {
    change
        .as_ref()
        .map(|v| projected(v, project_settings))
        .transpose()
        .map_err(S::Error::custom)?
        .serialize(serializer)
}
