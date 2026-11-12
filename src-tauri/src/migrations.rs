//! Application migration entry points, in their original execution phases.
//! Data-specific schema/SQLite versions and native installer migration stay with
//! their owners; there is deliberately no additional migration state file.
use crate::storage::Result;
use std::path::Path;

/// Recover interrupted transactions before loading their final store.
pub(crate) fn gateway(data: &Path) -> Result<(crate::gateway::Store, String)> {
    crate::gateway::recover_transactions(data)?;
    crate::gateway::Store::load(&data.join("gateway.json"))
}

#[cfg(test)]
mod tests;
