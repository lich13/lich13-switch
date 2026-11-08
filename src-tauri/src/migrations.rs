//! Application migration entry points, in their original execution phases.
//! Data-specific schema/SQLite versions and native installer migration stay with
//! their owners; there is deliberately no additional migration state file.
use crate::{startup, storage::Result};
use std::path::{Path, PathBuf};

/// Migrate legacy app data directory into `~/.vela` with SHA-256 content verification.
pub fn migrate_to_vela_home(data: &Path, legacy_dirs: &[PathBuf]) -> Result<()> {
    crate::storage::private_dir(data)?;
    let canonical_data = std::fs::canonicalize(data).unwrap_or_else(|_| data.to_path_buf());
    for legacy in legacy_dirs {
        let Ok(meta) = std::fs::symlink_metadata(legacy) else {
            continue;
        };
        if meta.file_type().is_symlink() || !meta.is_dir() {
            return Err(crate::storage::AppError::new(
                crate::errors::code::STORE,
                &format!("旧数据目录类型异常，已停止迁移：{}", legacy.display()),
            ));
        }
        let canonical_legacy =
            std::fs::canonicalize(legacy).unwrap_or_else(|_| legacy.to_path_buf());
        if canonical_legacy == canonical_data {
            continue;
        }
        migrate_tree(legacy, data, Path::new(""))?;
    }
    Ok(())
}

fn file_digest(path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let meta = std::fs::symlink_metadata(path).map_err(crate::storage::io_error)?;
    if meta.file_type().is_symlink() || !meta.is_file() {
        return Err(crate::storage::AppError::new(
            crate::errors::code::STORE,
            "迁移目标不是普通文件",
        ));
    }
    let mut file = std::fs::File::open(path).map_err(crate::storage::io_error)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf).map_err(crate::storage::io_error)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn migrate_tree(src: &Path, dst: &Path, rel_prefix: &Path) -> Result<()> {
    crate::storage::private_dir(dst)?;
    let mut entries: Vec<_> = std::fs::read_dir(src)
        .map_err(crate::storage::io_error)?
        .collect::<std::io::Result<Vec<_>>>()
        .map_err(crate::storage::io_error)?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        let src_path = entry.path();
        let rel_path = if rel_prefix.as_os_str().is_empty() {
            PathBuf::from(&name)
        } else {
            rel_prefix.join(&name)
        };
        let meta = std::fs::symlink_metadata(&src_path).map_err(crate::storage::io_error)?;
        if meta.file_type().is_symlink() {
            return Err(crate::storage::AppError::new(
                crate::errors::code::STORE,
                &format!(
                    "旧数据目录含符号链接，已停止迁移并保留残留：{}",
                    rel_path.display()
                ),
            ));
        }
        if name_str == ".DS_Store" || name_str.starts_with(".tmp") {
            std::fs::remove_file(&src_path).map_err(crate::storage::io_error)?;
            continue;
        }
        if (name_str.starts_with("gateway-spool-") || name_str.starts_with("bill-"))
            && meta.is_dir()
        {
            std::fs::remove_dir_all(&src_path).map_err(crate::storage::io_error)?;
            continue;
        }
        let dst_path = dst.join(&name);
        if meta.is_dir() {
            migrate_tree(&src_path, &dst_path, &rel_path)?;
        } else if meta.is_file() {
            let src_digest = file_digest(&src_path)?;
            if dst_path.exists() {
                let dst_digest = file_digest(&dst_path).map_err(|_| {
                    crate::storage::AppError::new(
                        crate::errors::code::CONFLICT,
                        &format!(
                            "数据迁移冲突：目标路径已存在且不可核验（{}），已保留旧目录原文件",
                            rel_path.display()
                        ),
                    )
                })?;
                if dst_digest != src_digest {
                    return Err(crate::storage::AppError::new(
                        crate::errors::code::CONFLICT,
                        &format!(
                            "数据迁移冲突：目标文件已存在且内容不一致（{}），已停止删除并保留旧目录原文件",
                            rel_path.display()
                        ),
                    ));
                }
                crate::storage::protect(&dst_path, false)?;
            } else {
                copy_verified_private_file(&src_path, &dst_path, &src_digest, &rel_path)?;
            }
            std::fs::remove_file(&src_path).map_err(|e| {
                crate::storage::AppError::new(
                    crate::errors::code::STORE,
                    &format!(
                        "旧数据文件清理失败，已停止后续删除（残留 {}）：{e}",
                        rel_path.display()
                    ),
                )
            })?;
        } else {
            return Err(crate::storage::AppError::new(
                crate::errors::code::STORE,
                &format!("不支持的数据文件类型（{}），已停止迁移", rel_path.display()),
            ));
        }
    }
    std::fs::remove_dir(src).map_err(|e| {
        crate::storage::AppError::new(
            crate::errors::code::STORE,
            &format!("旧数据目录尚未清空（{}）：{e}", src.display()),
        )
    })
}

fn copy_verified_private_file(
    src: &Path,
    dst: &Path,
    expected_digest: &str,
    rel_path: &Path,
) -> Result<()> {
    let parent = dst.parent().ok_or_else(|| {
        crate::storage::AppError::new(crate::errors::code::STORE, "无效的目标文件路径")
    })?;
    crate::storage::private_dir(parent)?;
    let temp = parent.join(format!(".vela-migrate-{}", uuid::Uuid::new_v4()));
    if let Err(e) = std::fs::copy(src, &temp) {
        let _ = std::fs::remove_file(&temp);
        return Err(crate::storage::io_error(e));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o600)) {
            let _ = std::fs::remove_file(&temp);
            return Err(crate::storage::io_error(e));
        }
    }
    let temp_digest = match file_digest(&temp) {
        Ok(d) => d,
        Err(e) => {
            let _ = std::fs::remove_file(&temp);
            return Err(e);
        }
    };
    if temp_digest != expected_digest {
        let _ = std::fs::remove_file(&temp);
        return Err(crate::storage::AppError::new(
            crate::errors::code::STORE,
            &format!("数据文件迁移摘要校验不一致（{}）", rel_path.display()),
        ));
    }
    if let Err(e) = std::fs::rename(&temp, dst) {
        let _ = std::fs::remove_file(&temp);
        return Err(crate::storage::io_error(e));
    }
    Ok(())
}

/// v0.7.0: exact retired statistics files, before opening application services.
/// The existing retry command also calls this entry point.
pub fn before_services(data: &Path) -> Result<()> {
    crate::cleanup::retired_statistics(data)
}

/// Recover interrupted transactions before normalizing and loading their final store.
pub(crate) fn gateway(data: &Path) -> Result<(crate::gateway::Store, String)> {
    crate::gateway::recover_for_migration(data)?;
    let loaded = crate::gateway::Store::load_migrated(&data.join("gateway.json"))?;
    crate::gateway::cleanup_legacy_proxy(data)?;
    Ok(loaded)
}

/// Product rename and old startup preferences: native state must be available,
/// and this runs in the existing background startup phase (never during smoke).
pub fn login_items(service: &startup::Service) -> Result<()> {
    service.migrate()
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

/// In-memory compatibility shared by ordinary loads and persistent upgrades.
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
    let mut changed = crate::gateway::normalize_legacy_proxy(value)?;
    changed |= value
        .as_object_mut()
        .is_some_and(|v| v.remove("grokNative").is_some());
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
