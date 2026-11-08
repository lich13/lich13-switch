use crate::errors::code;
use crate::storage::{AppError, Result};
use std::path::Path;

// Normalize only after interrupted gateway transactions have settled.
pub(crate) fn normalize(value: &mut serde_json::Value) -> Result<bool> {
    let object = value
        .as_object_mut()
        .ok_or_else(|| AppError::new(code::STORE, "网关存储无效"))?;
    let schema = object.get("schema").and_then(|v| v.as_u64());
    if !matches!(schema, Some(1 | 2)) {
        return Err(AppError::new(code::STORE, "网关存储版本不受支持"));
    }
    let mut changed = object.remove("proxies").is_some() || schema != Some(2);
    if let Some(providers) = object.get_mut("providers").and_then(|v| v.as_array_mut()) {
        for provider in providers.iter_mut().filter_map(|p| p.as_object_mut()) {
            changed |= provider.remove("proxyId").is_some();
        }
    }
    if changed {
        object.insert("schema".into(), 2.into());
    }
    Ok(changed)
}
pub(crate) fn cleanup(data: &Path) -> Result<()> {
    let profiles = data.join("proxy-profiles.json");
    match std::fs::symlink_metadata(&profiles) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Ok(meta) if !meta.is_dir() => std::fs::remove_file(&profiles)
            .map_err(|_| AppError::new(code::CLEANUP, "无法移除旧代理文件，请检查权限后重试")),
        _ => Err(AppError::new(code::CLEANUP, "旧代理文件状态异常，未删除")),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn removes_embedded_and_shared_credentials_preserving_provider_and_resume() {
        let t = tempfile::tempdir().unwrap();
        let old = super::super::model::Store {
            schema: 1,
            ..Default::default()
        };
        let mut value = serde_json::to_value(old).unwrap();
        value["proxies"] = serde_json::json!([{"password":"retired-secret"}]);
        value["providers"] = serde_json::json!([{"id":"stable","name":"fixture","baseUrl":"https://example.invalid/v1","token":"keep","queued":false,"proxyId":"legacy","maxConcurrency":4,"allowedModels":["model"],"version":"v"}]);
        value["future"] = serde_json::json!({"keep":true});
        std::fs::write(t.path().join("gateway.json"), value.to_string()).unwrap();
        std::fs::write(t.path().join("proxy-profiles.json"), "retired-secret").unwrap();
        crate::migrations::gateway(t.path()).unwrap();
        let once = std::fs::read(t.path().join("gateway.json")).unwrap();
        crate::migrations::gateway(t.path()).unwrap();
        assert_eq!(once, std::fs::read(t.path().join("gateway.json")).unwrap());
        let next: serde_json::Value = serde_json::from_slice(&once).unwrap();
        assert_eq!(next["providers"][0]["id"], "stable");
        assert_eq!(next["providers"][0]["token"], "keep");
        assert_eq!(next["providers"][0]["maxConcurrency"], 4);
        assert_eq!(next["future"], value["future"]);
        assert!(next.get("proxies").is_none());
        assert!(next["providers"][0].get("proxyId").is_none());
        assert!(!t.path().join("proxy-profiles.json").exists());
    }
    #[test]
    fn pending_recovery_prevents_migration_and_cleanup_failure_can_retry() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("proxy-profiles.json");
        std::fs::write(&p, "private").unwrap();
        std::fs::write(t.path().join("gateway-recovery.json"), "pending").unwrap();
        assert!(crate::migrations::gateway(t.path()).is_err());
        assert!(p.exists());
        std::fs::remove_file(t.path().join("gateway-recovery.json")).unwrap();
        std::fs::remove_file(&p).unwrap();
        std::fs::create_dir(&p).unwrap();
        assert!(crate::migrations::gateway(t.path()).is_err());
        std::fs::remove_dir(&p).unwrap();
        crate::migrations::gateway(t.path()).unwrap();
    }
}
