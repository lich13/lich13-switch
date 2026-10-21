//! Local effective configuration is for discovery only; writes always use the
//! original user document. Unsupported overlays remain explicit uncertainty.
use super::grok_config::MANAGED;
use crate::{
    errors::code,
    storage::{self, AppError, Result},
};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};
use toml_edit::{value, DocumentMut, Item, Table};

pub struct Effective {
    pub text: String,
    pub layers: Vec<String>,
    pub model_source: Option<String>,
    pub unknown: Vec<String>,
    pub files: Vec<(PathBuf, String)>,
    pub block: Option<String>,
    pub blocked_env: Vec<&'static str>,
}
fn error(s: &str) -> AppError {
    AppError::new(code::GROK_CONFIG, s)
}
fn merge(target: &mut dyn toml_edit::TableLike, source: &dyn toml_edit::TableLike) {
    for (key, item) in source.iter() {
        if let (Some(a), Some(b)) = (
            target.get_mut(key).and_then(Item::as_table_like_mut),
            item.as_table_like(),
        ) {
            merge(a, b);
        } else {
            target.insert(key, item.clone());
        }
    }
}
pub fn read(home: &Path, resolve: &impl Fn(&str) -> Option<String>) -> Result<Effective> {
    let system = if cfg!(all(unix, not(test))) {
        Some(Path::new("/etc/grok"))
    } else {
        None
    };
    read_at(home, system, resolve)
}
pub(super) fn read_at(
    home: &Path,
    system: Option<&Path>,
    resolve: &impl Fn(&str) -> Option<String>,
) -> Result<Effective> {
    let mut result = Effective {
        text: String::new(),
        layers: vec![],
        model_source: None,
        unknown: vec![
            "GROK_HOME、CLI -m、/model 临时选择、shell 环境及远端 campaign 未确认".into(),
        ],
        files: vec![],
        block: None,
        blocked_env: vec![],
    };
    let mut doc = DocumentMut::new();
    let mut inputs = vec![];
    if let Some(system) = system {
        inputs.push((system.join("managed_config.toml"), "系统 managed", false));
    }
    inputs.push((home.join("managed_config.toml"), "用户 managed", false));
    inputs.push((home.join("config.toml"), "用户配置", false));
    inputs.push((home.join("requirements.toml"), "用户 requirements", true));
    if let Some(system) = system {
        inputs.push((system.join("requirements.toml"), "系统 requirements", true));
    }
    let env_default = resolve("GROK_DEFAULT_MODEL").filter(|s| !s.is_empty());
    let mut env_applied = false;
    let mut requirements = DocumentMut::new();
    for (path, label, required) in inputs {
        if required && !env_applied {
            env_applied = true;
            if let Some(model) = &env_default {
                if !doc.contains_key("models") {
                    doc["models"] = Item::Table(Table::new());
                }
                doc["models"]["default"] = value(model);
                result.model_source = Some("环境 GROK_DEFAULT_MODEL".into());
            }
        }
        let raw = storage::read_optional(&path)?;
        result.files.push((path, storage::revision(raw.as_deref())));
        let Some(raw) = raw else {
            continue;
        };
        let layer = std::str::from_utf8(&raw)
            .map_err(|_| error("Grok 配置层不是 UTF-8"))?
            .parse::<DocumentMut>()
            .map_err(|_| error("Grok 配置层 TOML 无效，请在原生客户端核对"))?;
        result.layers.push(label.into());
        if layer.contains_key("version_overrides") {
            result.block = Some(format!(
                "{label} 包含 version_overrides，无法确认生效版本，请人工选择并核对原生 inspect"
            ));
        }
        if layer.get("models").and_then(|m| m.get("default")).is_some() {
            result.model_source = Some(label.into());
        }
        if required {
            merge(requirements.as_table_mut(), layer.as_table());
        }
        merge(doc.as_table_mut(), layer.as_table());
    }
    for (section, key, names) in [
        ("endpoints", "models_base_url", vec!["GROK_MODELS_BASE_URL"]),
        (
            "endpoints",
            "xai_api_base_url",
            vec!["GROK_XAI_API_BASE_URL"],
        ),
        (
            "auth",
            "preferred_method",
            vec!["GROK_AUTH_PREFERRED_METHOD"],
        ),
    ] {
        if requirements.get(section).and_then(|v| v.get(key)).is_some() {
            result.blocked_env.extend(names);
        }
    }
    if requirements
        .get("models")
        .and_then(|v| v.get("default"))
        .is_some_and(|v| v.as_str() != Some(MANAGED))
    {
        result.block = Some("requirements 固定了默认模型，受管默认选择无法生效".into());
    }
    if requirements
        .get("model")
        .and_then(|v| v.get(MANAGED))
        .is_some()
        || requirements
            .get("features")
            .and_then(|v| v.get("campaigns"))
            .is_some_and(|v| v.as_bool() != Some(false))
    {
        result.block = Some("requirements 覆盖受管模型或强制启用 campaign，无法接管".into());
    }
    if env_default.as_deref().is_some_and(|v| v != MANAGED)
        && !requirements
            .get("models")
            .is_some_and(|v| v.get("default").is_some())
    {
        result.block =
            Some("GROK_DEFAULT_MODEL 环境覆盖使受管默认选择无法生效，请先取消该环境覆盖".into());
    }
    for name in ["GROK_CONFIG", "GROK_CONFIG_PATH", "GROK_CAMPAIGNS_OVERRIDE"] {
        if resolve(name).is_some_and(|s| !s.is_empty()) {
            result.block = Some(format!(
                "检测到 {name}，无法确认其覆盖结果，请人工核对原生 inspect"
            ));
        }
    }
    result.text = doc.to_string();
    Ok(result)
}
pub fn unchanged(files: &[(PathBuf, String)]) -> Result<bool> {
    for (path, revision) in files {
        if storage::revision(storage::read_optional(path)?.as_deref()) != *revision {
            return Ok(false);
        }
    }
    Ok(true)
}
pub fn environment_snapshot(
    resolve: &impl Fn(&str) -> Option<String>,
) -> BTreeMap<String, Option<String>> {
    [
        "GROK_DEFAULT_MODEL",
        "GROK_CONFIG",
        "GROK_CONFIG_PATH",
        "GROK_CAMPAIGNS_OVERRIDE",
    ]
    .into_iter()
    .map(|name| (name.into(), resolve(name)))
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn requirements_win_environment_and_managed_layers_never_write_back() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("user");
        let system = root.path().join("system");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&system).unwrap();
        for (path, text) in [
            (
                system.join("managed_config.toml"),
                "[models]\ndefault='system'\n[model.system]\nmodel='system-id'\n",
            ),
            (
                home.join("managed_config.toml"),
                "[models]\ndefault='managed'\n",
            ),
            (
                home.join("config.toml"),
                "# keep user\n[models]\ndefault='user'\n",
            ),
            (
                home.join("requirements.toml"),
                "[models]\ndefault='requirement'\n",
            ),
            (
                system.join("requirements.toml"),
                "[models]\ndefault='system-required'\n[features]\ncampaigns=true\n",
            ),
        ] {
            std::fs::write(path, text).unwrap();
        }
        let effective = read_at(&home, Some(&system), &|key| {
            (key == "GROK_DEFAULT_MODEL").then(|| "environment".into())
        })
        .unwrap();
        assert_eq!(effective.model_source.as_deref(), Some("系统 requirements"));
        assert_eq!(effective.layers.len(), 5);
        assert!(effective.block.is_some());
        assert!(effective.text.contains("system-id"));
        assert!(effective.text.contains("system-required"));
        assert_eq!(
            std::fs::read_to_string(home.join("config.toml")).unwrap(),
            "# keep user\n[models]\ndefault='user'\n"
        );
        assert!(unchanged(&effective.files).unwrap());
        std::fs::write(home.join("managed_config.toml"), "# external").unwrap();
        assert!(!unchanged(&effective.files).unwrap());
    }
    #[test]
    fn missing_default_is_unknown_and_explicit_environment_blocks_management() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("config.toml"),
            "[model.fixture]\nmodel='fixture'\n",
        )
        .unwrap();
        let missing = read_at(root.path(), None, &|_| None).unwrap();
        assert!(missing.model_source.is_none());
        assert!(missing.block.is_none());
        let env = read_at(root.path(), None, &|name| {
            (name == "GROK_DEFAULT_MODEL").then(|| "fixture".into())
        })
        .unwrap();
        assert_eq!(env.model_source.as_deref(), Some("环境 GROK_DEFAULT_MODEL"));
        assert!(env.block.unwrap().contains("GROK_DEFAULT_MODEL"));
    }
}
