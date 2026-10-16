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

fn same_credentials(left: &str, right: &str) -> bool {
    let unit = |raw: &str| {
        serde_json::from_str::<Value>(raw).ok().map(|v| {
            json!({
                "mode": v["auth_mode"], "key": v["OPENAI_API_KEY"],
                "id": v["tokens"]["id_token"], "access": v["tokens"]["access_token"],
                "refresh": v["tokens"]["refresh_token"], "workspace": v["tokens"]["account_id"]
            })
        })
    };
    unit(left) == unit(right)
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

    pub(super) fn normalize_accounts(&mut self) -> Result<()> {
        let Some(expected) = self.normalization_revision.borrow().clone() else {
            return Ok(());
        };
        let mut next = self.store.clone();
        let infos: Vec<_> = next
            .profiles
            .iter()
            .map(|p| auth_info(&p.auth))
            .collect::<Result<_>>()?;
        // Validate complete connected components: a subject-only credential
        // cannot bridge different stable users, even through multiple old rows.
        fn root(parents: &[usize], mut i: usize) -> usize {
            while parents[i] != i {
                i = parents[i];
            }
            i
        }
        let mut parents: Vec<_> = (0..infos.len()).collect();
        for (i, info) in infos.iter().enumerate() {
            for (j, other) in infos.iter().enumerate().take(i) {
                if info.matches(other) {
                    let a = root(&parents, i);
                    let b = root(&parents, j);
                    parents[a.max(b)] = a.min(b);
                }
            }
        }
        let mut users: BTreeMap<usize, std::collections::BTreeSet<&str>> = BTreeMap::new();
        for (i, info) in infos.iter().enumerate() {
            if let Some(user) = info.principal.as_ref().and_then(|p| p.user.as_deref()) {
                if users.entry(root(&parents, i)).or_default().insert(user)
                    && users[&root(&parents, i)].len() > 1
                {
                    return Err(conflict());
                }
            }
        }
        let mut merged: Vec<Profile> = Vec::new();
        let mut groups: BTreeMap<usize, usize> = BTreeMap::new();
        let mut aliases = BTreeMap::new();
        for (position, (mut profile, info)) in next.profiles.into_iter().zip(infos).enumerate() {
            exchange::rebind_sources(&mut profile.exchange, &profile.identity, &info.identity)?;
            profile.identity = info.identity.clone();
            if let Some(&index) = groups.get(&root(&parents, position)) {
                let kept = &mut merged[index];
                let use_next = older(&kept.auth, &profile.auth);
                if !use_next
                    && !older(&profile.auth, &kept.auth)
                    && !same_credentials(&kept.auth, &profile.auth)
                {
                    return Err(AppError::new(
                        code::CONFLICT,
                        "重复账号的凭据新旧无法确认，原记录已保留",
                    ));
                }
                let auth = if use_next {
                    replace_auth(&kept.auth, &profile.auth)?
                } else {
                    replace_auth(&profile.auth, &kept.auth)?
                };
                let mut extra = serde_json::to_value(&kept.extensions).map_err(|_| conflict())?;
                merge_extensions(
                    &mut extra,
                    &serde_json::to_value(&profile.extensions).map_err(|_| conflict())?,
                )?;
                kept.extensions = serde_json::from_value(extra).map_err(|_| conflict())?;
                let identity = if info.principal.as_ref().is_some_and(|p| p.user.is_some()) {
                    &info.identity
                } else {
                    &kept.identity
                }
                .clone();
                exchange::rebind_sources(&mut kept.exchange, &kept.identity, &identity)?;
                exchange::rebind_sources(&mut profile.exchange, &profile.identity, &identity)?;
                kept.identity = identity;
                exchange::merge_sources(&mut kept.exchange, &profile.exchange, use_next)?;
                aliases.insert(profile.id.clone(), kept.id.clone());
                if use_next {
                    kept.email = profile.email;
                    kept.updated_at = profile.updated_at;
                }
                kept.auth = auth;
                let final_identity = auth_info(&kept.auth)?.identity;
                exchange::rebind_sources(&mut kept.exchange, &kept.identity, &final_identity)?;
                kept.identity = final_identity;
            } else {
                groups.insert(root(&parents, position), merged.len());
                merged.push(profile);
            }
        }
        next.profiles = merged;
        let bytes = serde_json::to_vec_pretty(&next).map_err(|_| conflict())?;
        if serde_json::to_value(&next).ok() != serde_json::to_value(&self.store).ok() {
            let path = self.data_dir.join("accounts.json");
            let before = storage::read_bounded(&path, exchange::VAULT_LIMIT)?;
            if storage::revision(before.as_deref()) != expected {
                return Err(AppError::new(
                    code::CONFLICT,
                    "账号库已被外部修改，未合并旧记录",
                ));
            }
            let before = before.ok_or_else(conflict)?;
            // The official-mode reference is remapped first to an ID already in
            // both vault versions. A crash here is safe and normalization retries.
            let official = crate::official::remap_account(&self.data_dir, &aliases)?;
            if let Err(error) =
                storage::atomic_write_bounded(&path, &bytes, Some(&expected), exchange::VAULT_LIMIT)
            {
                // Directory sync can fail after rename. Roll back only our
                // exact revision, never a file changed by another writer.
                let current = storage::read_bounded(&path, exchange::VAULT_LIMIT)?;
                let revision = storage::revision(current.as_deref());
                let applied = storage::digest(&bytes);
                if revision == applied {
                    storage::atomic_write_bounded(
                        &path,
                        &before,
                        Some(&applied),
                        exchange::VAULT_LIMIT,
                    )?;
                } else if revision != expected {
                    return Err(AppError::new(
                        code::CONFLICT,
                        "账号库已被外部修改，未覆盖当前文件",
                    ));
                }
                if let Some((before, applied)) = official {
                    storage::atomic_write(
                        &self.data_dir.join("official-mode.json"),
                        &before,
                        Some(&applied),
                    )?;
                }
                return Err(error);
            }
            self.store = next;
            self.checked_auth = None;
        }
        self.normalization_revision.replace(None);
        Ok(())
    }
}
