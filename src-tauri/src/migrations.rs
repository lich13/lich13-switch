//! Application migration entry points, in their original execution phases.
//! Data-specific schema/SQLite versions and native installer migration stay with
//! their owners; there is deliberately no additional migration state file.
use crate::{startup, storage::Result};
use std::path::Path;

/// v0.7.0: exact retired statistics files, before opening application services.
/// The existing retry command also calls this entry point.
pub fn before_services(data: &Path) -> Result<()> {
    crate::cleanup::retired_statistics(data)
}

/// v0.9.0: settle recovery before removing retired proxy fields. Called both
/// before loading a client gateway and by its existing recovery/retry path.
pub fn gateway(data: &Path) -> Result<()> {
    crate::gateway::recover_for_migration(data)?;
    crate::gateway::retire_legacy_proxy(data)?;
    let path = data.join("gateway.json");
    if let Some(raw) = crate::storage::read_optional(&path)? {
        let mut value: serde_json::Value = serde_json::from_slice(&raw).map_err(|_| {
            crate::storage::AppError::new(crate::errors::code::STORE, "网关存储无法读取")
        })?;
        if value
            .as_object_mut()
            .is_some_and(|v| v.remove("grokNative").is_some())
        {
            let next = serde_json::to_vec_pretty(&value).map_err(|_| {
                crate::storage::AppError::new(crate::errors::code::STORE, "网关存储无法保存")
            })?;
            crate::storage::atomic_write(&path, &next, Some(&crate::storage::digest(&raw)))?;
        }
    }
    Ok(())
}

/// Product rename and old startup preferences: native state must be available,
/// and this runs in the existing background startup phase (never during smoke).
pub fn login_items(service: &startup::Service) -> Result<()> {
    service.migrate()
}

#[cfg(test)]
mod tests;

/// v0.25.18: convert the two original slots to stable official model IDs.
pub(crate) fn dsh_model_mappings(value: &mut serde_json::Value) {
    if let Some(providers) = value.get_mut("dshModels").and_then(|v| v.as_object_mut()) {
        for mapping in providers.values_mut().filter_map(|v| v.as_object_mut()) {
            for (old, id) in [
                ("flash", crate::gateway::dsh::models::FLASH),
                ("pro", crate::gateway::dsh::models::PRO),
            ] {
                if let Some(model) = mapping.remove(old) {
                    mapping.entry(id).or_insert(model);
                }
            }
        }
    }
}

/// Missing retryBudgetSeconds identifies the old, ineffective totalSeconds setting.
/// No separate migration marker: once persisted, explicit user values remain intact.
pub(crate) fn gateway_wait_budget(value: &mut serde_json::Value) {
    let Some(settings) = value.get_mut("settings").and_then(|v| v.as_object_mut()) else {
        return;
    };
    if !settings.contains_key("retryBudgetSeconds") {
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
    settings.remove("totalSeconds");
}

/// Compact persistent state only after all interrupted file transactions finish.
pub(crate) fn gateway_state_after_recovery(data: &Path) -> Result<()> {
    let path = data.join("gateway.json");
    let Some(raw) = crate::storage::read_optional(&path)? else {
        return Ok(());
    };
    let invalid = || {
        crate::storage::AppError::new(crate::errors::code::STORE, "网关存储无法迁移，已保留原文件")
    };
    let mut value: serde_json::Value = serde_json::from_slice(&raw).map_err(|_| invalid())?;
    let before = value.clone();
    gateway_wait_budget(&mut value);
    dsh_model_mappings(&mut value);
    if let Some(models) = value.get_mut("dshCatalog").and_then(|v| v.as_array_mut()) {
        for model in models {
            crate::gateway::dsh::models::normalize(model);
        }
    }
    if let Some(snapshot) = value.get_mut("dsh").filter(|v| !v.is_null()) {
        let parsed: crate::gateway::dsh::Snapshot =
            serde_json::from_value(snapshot.clone()).map_err(|_| invalid())?;
        *snapshot = serde_json::to_value(parsed).map_err(|_| invalid())?;
    }
    if value != before {
        let next = serde_json::to_vec_pretty(&value).map_err(|_| invalid())?;
        crate::storage::atomic_write(&path, &next, Some(&crate::storage::digest(&raw)))?;
    }
    Ok(())
}
