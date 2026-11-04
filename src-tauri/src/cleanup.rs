use crate::errors::code;
use crate::storage::{AppError, Result};
use std::path::Path;

const RETIRED_FILES: &[&str] = &[
    "usage.sqlite",
    "usage.sqlite-wal",
    "usage.sqlite-shm",
    "usage.sqlite-journal",
    "usage-settings.json",
    "model-pricing.json",
    "previous-auth.json",
];

const RETIRED_AGY_FILES: &[&str] = &[
    "gateway.json",
    "gateway-recovery.json",
    "gateway-api-write.json",
    "previous-config.json",
    "previous-settings.json",
];

/// Exact retired-module files only. Never follow links or remove a non-empty directory.
pub fn retired_statistics(data: &Path) -> Result<()> {
    for name in RETIRED_FILES {
        let path = data.join(name);
        match std::fs::symlink_metadata(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Ok(meta) if !meta.is_dir() => std::fs::remove_file(&path).map_err(|_| {
                AppError::new(
                    code::CLEANUP,
                    &format!("无法移除旧数据 {name}，请检查权限后重试"),
                )
            })?,
            _ => {
                return Err(AppError::new(
                    code::CLEANUP,
                    &format!("旧数据 {name} 无法清理，请检查文件状态"),
                ))
            }
        }
    }
    let agy_dir = data.join("agy");
    match std::fs::symlink_metadata(&agy_dir) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Ok(meta) if !meta.file_type().is_symlink() && meta.is_dir() => {
            for name in RETIRED_AGY_FILES {
                let path = agy_dir.join(name);
                match std::fs::symlink_metadata(&path) {
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                    Ok(m) if !m.is_dir() => std::fs::remove_file(&path).map_err(|_| {
                        AppError::new(
                            code::CLEANUP,
                            &format!("无法移除旧数据 agy/{name}，请检查权限后重试"),
                        )
                    })?,
                    _ => {
                        return Err(AppError::new(
                            code::CLEANUP,
                            &format!("旧数据 agy/{name} 无法清理，请检查文件状态"),
                        ))
                    }
                }
            }
            let _ = std::fs::remove_dir(&agy_dir);
        }
        _ => {
            return Err(AppError::new(
                code::CLEANUP,
                "旧数据 agy 无法清理，请检查目录状态",
            ))
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn removal_is_exact_idempotent_and_preserves_other_data() {
        let dir = tempfile::tempdir().unwrap();
        for name in RETIRED_FILES {
            std::fs::write(dir.path().join(name), b"retired").unwrap();
        }
        let agy = dir.path().join("agy");
        std::fs::create_dir(&agy).unwrap();
        for name in RETIRED_AGY_FILES {
            std::fs::write(agy.join(name), b"retired").unwrap();
        }
        std::fs::write(dir.path().join("gateway.json"), b"preserve").unwrap();
        retired_statistics(dir.path()).unwrap();
        retired_statistics(dir.path()).unwrap();
        assert!(!agy.exists());
        assert_eq!(
            std::fs::read(dir.path().join("gateway.json")).unwrap(),
            b"preserve"
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }
    #[test]
    fn directory_is_not_removed_and_failure_can_retry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usage.sqlite");
        std::fs::create_dir(&path).unwrap();
        assert!(retired_statistics(dir.path()).is_err());
        std::fs::remove_dir(path).unwrap();
        assert!(retired_statistics(dir.path()).is_ok());
    }
}
