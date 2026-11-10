//! Private identity matching and revision-checked normalization of legacy vaults.
use super::*;

fn conflict() -> AppError {
    AppError::new(
        code::CONFLICT,
        "账号身份或扩展数据无法安全合并，原记录已保留",
    )
}

impl AuthInfo {
    pub(super) fn matches(&self, other: &Self) -> bool {
        if self.kind != other.kind || self.workspace != other.workspace {
            return false;
        }
        match (&self.principal, &other.principal) {
            (Some(a), Some(b)) => a.matches(b),
            (None, None) => self.identity == other.identity,
            _ => false,
        }
    }
}

/// Missing legacy user claims can match a subject, but must never bridge two
/// different stable users. More than one candidate is deliberately ambiguous.
fn matching(profiles: &[Profile], info: &AuthInfo) -> Result<Option<usize>> {
    let mut found = None;
    for (index, profile) in profiles.iter().enumerate() {
        if auth_info(&profile.auth)?.matches(info) {
            if found.is_some() {
                return Err(conflict());
            }
            found = Some(index);
        }
    }
    Ok(found)
}

pub(super) fn merge_extensions(target: &mut Value, source: &Value) -> Result<()> {
    if let (Some(target), Some(source)) = (target.as_object_mut(), source.as_object()) {
        for (key, value) in source {
            if let Some(previous) = target.get_mut(key) {
                merge_extensions(previous, value)?;
            } else {
                target.insert(key.clone(), value.clone());
            }
        }
        Ok(())
    } else if target == source {
        Ok(())
    } else {
        Err(conflict())
    }
}

fn extensions(raw: &str) -> Result<Value> {
    let mut value: Value = serde_json::from_str(raw).map_err(|_| conflict())?;
    let root = value.as_object_mut().ok_or_else(conflict)?;
    for key in ["auth_mode", "OPENAI_API_KEY", "last_refresh"] {
        root.remove(key);
    }
    if let Some(tokens) = root.get_mut("tokens").and_then(Value::as_object_mut) {
        for key in ["access_token", "refresh_token", "id_token", "account_id"] {
            tokens.remove(key);
        }
        if tokens.is_empty() {
            root.remove("tokens");
        }
    }
    Ok(value)
}

/// Replace all credential-owned fields as a unit; preserve only compatible
/// unknown fields. Never combine new tokens with an old workspace or API key.
pub(super) fn replace_auth(previous: &str, next: &str) -> Result<String> {
    let mut extra = extensions(previous)?;
    merge_extensions(&mut extra, &extensions(next)?)?;
    let mut value: Value = serde_json::from_str(next).map_err(|_| conflict())?;
    merge_extensions(&mut value, &extra)?;
    if value == serde_json::from_str::<Value>(next).map_err(|_| conflict())? {
        Ok(next.into())
    } else {
        serde_json::to_string_pretty(&value).map_err(|_| conflict())
    }
}

pub(super) fn replace_profile(profile: &mut Profile, raw: &str) -> Result<()> {
    let info = auth_info(raw)?;
    let auth = replace_auth(&profile.auth, raw)?;
    let mut exchange = profile.exchange.clone();
    exchange::rebind_sources(&mut exchange, &profile.identity, &info.identity)?;
    profile.auth = auth;
    profile.identity = info.identity;
    profile.email = info.email;
    profile.updated_at = now();
    profile.exchange = exchange;
    Ok(())
}

impl Core {
    pub(super) fn matching_profile(&self, info: &AuthInfo) -> Result<Option<usize>> {
        matching(&self.store.profiles, info)
    }
}
