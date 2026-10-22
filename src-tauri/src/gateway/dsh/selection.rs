use super::*;
use crate::gateway::{
    current::{self, Current, Kind},
    model::Store,
    takeover::Pair,
};

pub(super) fn profiles(home: &Path) -> Result<Vec<String>> {
    let mut profiles = vec![];
    if home.join("profiles").is_dir() {
        for entry in std::fs::read_dir(home.join("profiles"))
            .map_err(storage::io_error)?
            .take(512)
        {
            let entry = entry.map_err(storage::io_error)?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if profile_dir(home, &name).is_ok() {
                profiles.push(name);
            }
        }
    }
    profiles.sort();
    Ok(profiles)
}

fn read_current(
    home: &Path,
    profile: &str,
    store: &Store,
    ports: &[u16],
    resolve: &impl Fn(&str) -> Option<String>,
) -> Result<Current> {
    let mut input = Input::read(home, profile)?;
    let mut source = format!("profile {profile} · bundle");
    if let Some(raw) = input.patch.before.clone() {
        input.read_layer(&sequence(&raw)?, 0)?;
        source = format!("profile {profile}");
    }
    let before = input.configs.clone();
    if let Some(raw) = text(&home.join(PATCH))? {
        input.read_layer(&sequence(&raw)?, 0)?;
    }
    let overridden =
        [PI, DEFAULT]
            .iter()
            .any(|id| match (before.get(*id), input.configs.get(*id)) {
                (Some(a), Some(b)) => !yaml_edit::yaml_eq(a, b),
                (None, None) => false,
                _ => true,
            });
    if overridden {
        source.push_str(" · home patch");
    }
    // Existing 0.1.x settings are an explicit persisted default; never create them.
    if let Some(raw) = text(&home.join("settings.yaml"))? {
        let doc = yaml(&raw)?;
        let root = doc
            .as_mapping()
            .ok_or_else(|| error("DSH settings 必须是 YAML 映射"))?;
        for id in [PI, DEFAULT] {
            if root.contains_key(id) {
                let value = root
                    .get_mapping(id)
                    .ok_or_else(|| error("DSH settings 模型配置无法静态解析"))?;
                input.configs.insert(id.into(), value);
                source.push_str(" · settings");
            }
        }
    }
    let default = input
        .configs
        .get(DEFAULT)
        .ok_or_else(|| error("未找到 DSH 默认模型"))?;
    let provider =
        string(default.get("provider")).ok_or_else(|| error("DSH 默认供应商无法静态解析"))?;
    let model = string(default.get("model")).and_then(|s| crate::events::safe_model(&s));
    let route = input
        .configs
        .get(PI)
        .and_then(|c| c.get_mapping("providers"))
        .and_then(|p| p.get_mapping(&provider));
    let mut warning = "按静态配置判断；启动参数、其他进程环境及项目覆盖未确认".to_owned();
    if overridden {
        warning = format!("home patch 覆盖了 profile 配置；{warning}");
    }
    let mut result = if provider == "deepseek-official" && route.is_none() {
        Current::new(Kind::Official)
    } else if let Some(route) = route {
        let base = string(route.get("baseURL"));
        if base.as_ref().is_some_and(|b| {
            url::Url::parse(b).is_ok_and(|u| u.host_str() == Some("api.deepseek.com"))
        }) {
            Current::new(Kind::Official)
        } else {
            let credential = credentials(input.credentials.before.as_deref())?;
            let record = credential
                .get_mapping("records")
                .and_then(|r| r.get_mapping(format!("llm-pi-ai/{provider}")));
            let mut token = string(route.get("apiKey"));
            if token.is_none() {
                token = record
                    .as_ref()
                    .filter(|r| string(r.get("kind")).as_deref() == Some("api-key"))
                    .and_then(|r| string(r.get("key")));
            }
            if token.is_none() {
                if let Some(key) = string(route.get("apiKeyEnv")) {
                    token = credential
                        .get_mapping("refs")
                        .and_then(|r| string(r.get(&key)));
                    // A launch/project environment is not observable in another process.
                    // Preserve a known mismatch; warn when a launch/project environment
                    // could differ from the visible fallback.
                    if token.is_none() {
                        let fallback = resolve(&key).or_else(|| {
                            super::super::grok_import::environment(
                                input.environment.before.as_deref().unwrap_or(""),
                            )
                            .ok()
                            .and_then(|v| v.get(&key).cloned())
                        });
                        if fallback.is_some() {
                            warning.push_str(
                                "；环境凭据匹配仅适用于当前可见环境，启动环境或项目可能覆盖",
                            );
                        }
                        token = fallback;
                    }
                }
            }
            if record
                .as_ref()
                .is_some_and(|r| string(r.get("kind")).as_deref() == Some("grant"))
            {
                Current::unknown("DSH 选择了授权记录，无法静态确定访问凭据")
            } else {
                current::pair(
                    ClientId::Dsh,
                    &Pair {
                        base_url: base,
                        token,
                    },
                    store,
                    ports,
                )
            }
        }
    } else {
        Current::unknown("DSH 所选供应商未在有效配置中定义")
    };
    result.model = model;
    result.model_source = Some(source);
    result.warning = Some(warning);
    Ok(result)
}

pub fn current(
    home: &Path,
    store: &Store,
    ports: &[u16],
    resolve: &impl Fn(&str) -> Option<String>,
) -> Current {
    let profiles = match profiles(home) {
        Ok(profiles) if !profiles.is_empty() => profiles,
        _ => return Current::unknown("尚无可用 DSH profile"),
    };
    let mut result = profile_current(home, &profiles[0], store, ports, resolve);
    for profile in &profiles[1..] {
        let other = profile_current(home, profile, store, ports, resolve);
        if other.state != result.state || other.model != result.model {
            return Current::unknown(
                "多个 DSH profile 的选择不同，启动 profile 无法确认；请在接入面板查看指定 profile",
            );
        }
        if other
            .warning
            .as_ref()
            .is_some_and(|w| w.contains("home patch"))
        {
            result.warning = other.warning;
        }
    }
    if profiles.len() > 1 {
        result.model_source = Some(format!("{} 个 profile 的静态选择一致", profiles.len()));
    }
    result
}

pub(super) fn profile_current(
    home: &Path,
    profile: &str,
    store: &Store,
    ports: &[u16],
    resolve: &impl Fn(&str) -> Option<String>,
) -> Current {
    read_current(home, profile, store, ports, resolve)
        .unwrap_or_else(|e| Current::unknown(&e.message))
}
