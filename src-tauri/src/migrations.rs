//! Application migration entry points, in their original execution phases.
//! Data-specific schema/SQLite versions and native installer migration stay with
//! their owners; there is deliberately no additional migration state file.
use crate::storage::Result;
use std::path::Path;

/// Recover interrupted transactions before normalizing and loading their final store.
pub(crate) fn gateway(data: &Path) -> Result<(crate::gateway::Store, String)> {
    crate::gateway::recover_for_migration(data)?;
    crate::gateway::Store::load_migrated(&data.join("gateway.json"))
}

#[cfg(test)]
mod tests;

/// v0.25.18: convert the two original slots to stable official model IDs.
pub(crate) fn dsh_model_mappings(value: &mut serde_json::Value) -> bool {
    let mut changed = false;
    if let Some(providers) = value.get_mut("dshModels").and_then(|v| v.as_object_mut()) {
        for mapping in providers.values_mut().filter_map(|v| v.as_object_mut()) {
            for (old, id) in [
                ("flash", crate::gateway::dsh::models::FLASH),
                ("pro", crate::gateway::dsh::models::PRO),
            ] {
                if let Some(model) = mapping.remove(old) {
                    mapping.entry(id).or_insert(model);
                    changed = true;
                }
            }
        }
    }
    changed
}

/// Missing retryBudgetSeconds identifies the old, ineffective totalSeconds setting.
/// No separate migration marker: once persisted, explicit user values remain intact.
pub(crate) fn gateway_wait_budget(value: &mut serde_json::Value) -> bool {
    let Some(settings) = value.get_mut("settings").and_then(|v| v.as_object_mut()) else {
        return false;
    };
    let changed = !settings.contains_key("retryBudgetSeconds");
    if changed {
        for key in [
            "connectSeconds",
            "firstByteSeconds",
            "idleSeconds",
            "queueSeconds",
            "websocketRetrySeconds",
            "retryBudgetSeconds",
        ] {
            settings.insert(key.into(), 300.into());
        }
    }
    settings.remove("totalSeconds").is_some() || changed
}

/// Persistent compatibility applied during `Store::load_migrated`.
pub(crate) fn gateway_fields(value: &mut serde_json::Value) -> bool {
    let mut changed = gateway_wait_budget(value);
    changed |= dsh_model_mappings(value);
    if let Some(models) = value.get_mut("dshCatalog").and_then(|v| v.as_array_mut()) {
        for model in models {
            changed |= crate::gateway::dsh::models::normalize(model);
        }
    }
    changed
}

/// Compact persistent state only after all interrupted file transactions finish.
pub(crate) fn gateway_state_after_recovery(value: &mut serde_json::Value) -> Result<bool> {
    let schema = value.get("schema").and_then(|v| v.as_u64());
    if !matches!(schema, Some(1 | 2)) {
        return Err(crate::storage::AppError::new(
            crate::errors::code::STORE,
            "网关存储版本不受支持",
        ));
    }
    let mut changed = schema != Some(2);
    if changed {
        if let Some(root) = value.as_object_mut() {
            root.insert("schema".into(), 2.into());
        }
    }
    changed |= gateway_fields(value);
    if let Some(snapshot) = value.get_mut("dsh").filter(|v| !v.is_null()) {
        let invalid = || {
            crate::storage::AppError::new(
                crate::errors::code::STORE,
                "网关存储无法迁移，已保留原文件",
            )
        };
        let parsed: crate::gateway::dsh::Snapshot =
            serde_json::from_value(snapshot.clone()).map_err(|_| invalid())?;
        let compact = serde_json::to_value(parsed).map_err(|_| invalid())?;
        changed |= *snapshot != compact;
        *snapshot = compact;
    }
    Ok(changed)
}
