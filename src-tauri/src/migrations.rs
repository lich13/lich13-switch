//! Application migration entry points, in their original execution phases.
//! Data-specific schema/SQLite versions and native installer migration stay with
//! their owners; there is deliberately no additional migration state file.
use crate::{startup, storage::Result};
use std::path::{Path, PathBuf};

/// v0.26.14: migrate legacy app data directory (`com.lich13.gpt-switch`) into `~/.vela`.
pub fn migrate_to_vela_home(data: &Path, legacy_dirs: &[PathBuf]) -> Result<()> {
    crate::storage::private_dir(data)?;
    for legacy in legacy_dirs {
        if legacy == data || !legacy.exists() {
            continue;
        }
        migrate_tree(legacy, data)?;
    }
    Ok(())
}

fn migrate_tree(src: &Path, dst: &Path) -> Result<()> {
    crate::storage::private_dir(dst)?;
    let entries = std::fs::read_dir(src).map_err(crate::storage::io_error)?;
    for entry in entries {
        let entry = entry.map_err(crate::storage::io_error)?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        let src_path = entry.path();
        if name_str == ".DS_Store" || name_str.starts_with(".tmp") {
            let _ = std::fs::remove_file(&src_path);
            continue;
        }
        if name_str.starts_with("gateway-spool-") {
            let _ = std::fs::remove_dir_all(&src_path);
            continue;
        }
        let file_type = entry.file_type().map_err(crate::storage::io_error)?;
        let dst_path = dst.join(&name);
        if file_type.is_dir() {
            migrate_tree(&src_path, &dst_path)?;
        } else if file_type.is_file() {
            if !dst_path.exists() {
                copy_private_file(&src_path, &dst_path)?;
            }
            std::fs::remove_file(&src_path).map_err(crate::storage::io_error)?;
        }
    }
    let _ = std::fs::remove_dir(src);
    Ok(())
}

fn copy_private_file(src: &Path, dst: &Path) -> Result<()> {
    let parent = dst.parent().ok_or_else(|| {
        crate::storage::AppError::new(crate::errors::code::STORE, "无效的目标文件路径")
    })?;
    crate::storage::private_dir(parent)?;
    let temp = parent.join(format!(".vela-migrate-{}", uuid::Uuid::new_v4()));
    let copied = std::fs::copy(src, &temp).map_err(crate::storage::io_error)?;
    let expected = std::fs::metadata(src)
        .map_err(crate::storage::io_error)?
        .len();
    if copied != expected {
        let _ = std::fs::remove_file(&temp);
        return Err(crate::storage::AppError::new(
            crate::errors::code::STORE,
            "数据文件迁移校验不一致",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o600))
            .map_err(crate::storage::io_error)?;
    }
    std::fs::rename(&temp, dst).map_err(crate::storage::io_error)
}

/// v0.26.14: migrate legacy `lich13-switch` / `LICH13_SWITCH_GATEWAY_KEY` managed markers to `vela`.
pub fn migrate_legacy_markers(
    data: &Path,
    codex_home: &Path,
    grok_home: &Path,
    dsh_home: &Path,
) -> Result<()> {
    for rel in [
        "grok/gateway.json",
        "grok/gateway-recovery.json",
        "dsh/gateway.json",
        "dsh/dsh-recovery.json",
    ] {
        replace_text_file(
            &data.join(rel),
            &[
                ("lich13-switch", "vela"),
                ("LICH13_SWITCH_GATEWAY_KEY", "VELA_GATEWAY_KEY"),
            ],
        )?;
    }
    replace_text_file(&grok_home.join("config.toml"), &[("lich13-switch", "vela")])?;
    for path in [
        codex_home.join("config.toml"),
        data.join("previous-config.toml"),
    ] {
        replace_text_file(&path, &[("# lich13-switch:official ", "# vela:official ")])?;
    }
    for rel in [
        ".env",
        ".credentials.yaml",
        "cordis.patch.yml",
        "settings.yaml",
    ] {
        replace_text_file(
            &dsh_home.join(rel),
            &[
                ("lich13-switch", "vela"),
                ("LICH13_SWITCH_GATEWAY_KEY", "VELA_GATEWAY_KEY"),
            ],
        )?;
    }
    let profiles_dir = dsh_home.join("profiles");
    if profiles_dir.is_dir() {
        if let Ok(entries) = std::fs::read_dir(&profiles_dir) {
            for entry in entries.flatten() {
                let patch = entry.path().join("cordis.patch.yml");
                if patch.is_file() {
                    replace_text_file(
                        &patch,
                        &[
                            ("lich13-switch", "vela"),
                            ("LICH13_SWITCH_GATEWAY_KEY", "VELA_GATEWAY_KEY"),
                        ],
                    )?;
                }
            }
        }
    }
    Ok(())
}

fn replace_text_file(path: &Path, replacements: &[(&str, &str)]) -> Result<()> {
    let Some(raw) = crate::storage::read_optional(path)? else {
        return Ok(());
    };
    let Ok(text) = std::str::from_utf8(&raw) else {
        return Ok(());
    };
    let mut next = text.to_owned();
    for (from, to) in replacements {
        if next.contains(from) {
            next = next.replace(from, to);
        }
    }
    if next != text {
        crate::storage::atomic_write(path, next.as_bytes(), Some(&crate::storage::digest(&raw)))?;
    }
    Ok(())
}

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
