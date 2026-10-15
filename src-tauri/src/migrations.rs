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
    crate::gateway::retire_legacy_proxy(data)
}

/// Product rename and old startup preferences: native state must be available,
/// and this runs in the existing background startup phase (never during smoke).
pub fn login_items(service: &startup::Service) -> Result<()> {
    service.migrate()
}

#[cfg(test)]
mod tests;
