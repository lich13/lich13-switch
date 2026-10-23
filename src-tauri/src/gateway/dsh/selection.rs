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

pub(super) struct Selection {
    pub current: Current,
    pub pair: Pair,
    pub provider: String,
    pub models: Vec<String>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileCurrent {
    pub profile: String,
    pub current: Current,
}
pub fn profile_states(
    home: &Path,
    store: &Store,
    ports: &[u16],
    resolve: &impl Fn(&str) -> Option<String>,
) -> Vec<ProfileCurrent> {
    profiles(home)
        .unwrap_or_default()
        .into_iter()
        .map(|profile| ProfileCurrent {
            current: profile_current(home, &profile, store, ports, resolve),
            profile,
        })
        .collect()
}

pub(super) fn read_current(
    home: &Path,
    profile: &str,
    store: &Store,
    ports: &[u16],
    resolve: &impl Fn(&str) -> Option<String>,
) -> Result<Selection> {
    read_provider(home, profile, store, ports, resolve, None)
}

pub(super) fn effective(home: &Path, profile: &str) -> Result<(Input, String, bool)> {
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
    Ok((input, source, overridden))
}

pub(super) fn read_provider(
    home: &Path,
    profile: &str,
    store: &Store,
    ports: &[u16],
    resolve: &impl Fn(&str) -> Option<String>,
    selected: Option<&str>,
) -> Result<Selection> {
    let (input, source, overridden) = effective(home, profile)?;
    let default = input.configs.get(DEFAULT);
    let provider = selected
        .map(str::to_owned)
        .or_else(|| default.and_then(|d| string(d.get("provider"))))
        .ok_or_else(|| error("DSH 默认供应商无法静态解析"))?;
    let model = default
        .and_then(|d| string(d.get("model")))
        .and_then(|s| crate::events::safe_model(&s));
    let route = input
        .configs
        .get(PI)
        .and_then(|c| c.get_mapping("providers"))
        .and_then(|p| p.get_mapping(&provider));
    let models = route
        .as_ref()
        .and_then(|r| r.get_sequence("models"))
        .map(|models| {
            models
                .values()
                .filter_map(|m| string(m.get("id")))
                .filter_map(|id| crate::events::safe_model(&id))
                .collect()
        })
        .unwrap_or_default();
    let mut warning = String::new();
    if overridden {
        warning = "home patch 覆盖了 profile 配置".into();
    }
    let mut selected_pair = Pair {
        base_url: None,
        token: None,
    };
    let mut credential_source = None;
    let protocol = route
        .as_ref()
        .and_then(|r| match string(r.get("api")).as_deref() {
            Some("openai-completions") => Some(Protocol::ChatCompletions),
            Some("openai-responses") => Some(Protocol::Responses),
            Some("anthropic-messages") => Some(Protocol::Messages),
            _ => None,
        });
    let provider_name = route
        .as_ref()
        .and_then(|r| string(r.get("displayName")))
        .unwrap_or_else(|| provider.clone());
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
            if token.is_some() {
                credential_source = Some("provider apiKey".into());
            }
            if token.is_none() {
                token = record
                    .as_ref()
                    .filter(|r| string(r.get("kind")).as_deref() == Some("api-key"))
                    .and_then(|r| string(r.get("key")));
                if token.is_some() {
                    credential_source = Some("凭据库 records".into());
                }
            }
            if token.is_none() {
                if let Some(key) = string(route.get("apiKeyEnv")) {
                    token = credential
                        .get_mapping("refs")
                        .and_then(|r| string(r.get(&key)));
                    if token.is_some() {
                        credential_source = Some("凭据库 refs".into());
                    }
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
                        if token.is_some() {
                            credential_source = Some("环境变量".into());
                        }
                    }
                }
            }
            if record
                .as_ref()
                .is_some_and(|r| string(r.get("kind")).as_deref() == Some("grant"))
            {
                Current::unknown("DSH 选择了授权记录，无法静态确定访问凭据")
            } else {
                selected_pair = Pair {
                    base_url: base,
                    token,
                };
                current::pair(ClientId::Dsh, &selected_pair, store, ports)
            }
        }
    } else {
        Current::unknown("DSH 所选供应商未在有效配置中定义")
    };
    result.model = model;
    result.model_source = Some(source);
    result.warning = (!warning.is_empty()).then_some(warning);
    result.provider_name = crate::events::safe_message(&provider_name);
    result.address_source = Some("llm-pi-ai.providers".into());
    result.address = super::super::grok_import::safe_address(selected_pair.base_url.as_deref());
    result.credential_source = credential_source;
    result.protocol = protocol;
    Ok(Selection {
        current: result,
        pair: selected_pair,
        provider,
        models,
    })
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
    let initial = match read_current(home, &profiles[0], store, ports, resolve) {
        Ok(selection) => selection,
        Err(error) => return Current::unknown(&error.message),
    };
    let signature = |s: &Selection| {
        (
            s.pair.base_url.clone(),
            s.pair.token.clone(),
            s.provider.clone(),
            s.current.protocol,
        )
    };
    let mut result = initial.current.clone();
    let mut mixed = false;
    for profile in &profiles[1..] {
        let selection = match read_current(home, profile, store, ports, resolve) {
            Ok(selection) => selection,
            Err(error) => return Current::unknown(&error.message),
        };
        let different = signature(&initial) != signature(&selection);
        let other = selection.current;
        if matches!(result.state, Kind::Unknown { .. }) {
            return result;
        }
        if matches!(other.state, Kind::Unknown { .. }) {
            return other;
        }
        if different || other.state != result.state {
            mixed = true;
        }
        if other.model != result.model {
            result.model = None;
        }
        if other
            .warning
            .as_ref()
            .is_some_and(|w| w.contains("home patch"))
        {
            result.warning = other.warning;
        }
    }
    if mixed {
        result = Current::new(Kind::Mixed);
    }
    if profiles.len() > 1 {
        result.model_source = Some(format!("{} 个 profile", profiles.len()));
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
        .map(|selection| selection.current)
        .unwrap_or_else(|e| Current::unknown(&e.message))
}
