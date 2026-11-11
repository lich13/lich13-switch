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

/// Persistent compatibility applied during `Store::load_migrated`.
pub(crate) fn gateway_fields(value: &mut serde_json::Value) -> bool {
    let mut changed = false;
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
