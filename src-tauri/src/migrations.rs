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
