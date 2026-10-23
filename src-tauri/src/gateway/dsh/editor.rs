use super::*;
use std::collections::BTreeSet;

#[derive(Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FileKey {
    ProfilePatch,
    HomePatch,
    Settings,
    Environment,
    Credentials,
}
impl FileKey {
    fn writable(self) -> bool {
        matches!(self, Self::ProfilePatch | Self::HomePatch | Self::Settings)
    }
    fn name(self) -> &'static str {
        match self {
            Self::ProfilePatch => "profilePatch",
            Self::HomePatch => "homePatch",
            Self::Settings => "settings",
            Self::Environment => "environment",
            Self::Credentials => "credentials",
        }
    }
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileView {
    pub profiles: Vec<String>,
    pub profile: Option<String>,
    pub key: FileKey,
    pub files: Vec<FileKey>,
    pub text: String,
    pub revision: String,
    pub read_only: bool,
    pub can_restore: bool,
}

pub(super) fn no_symlinks(home: &Path, path: &Path) -> Result<()> {
    for component in path.ancestors() {
        match std::fs::symlink_metadata(component) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(error("DSH 配置路径不能经过符号链接"))
            }
            Ok(_) => (),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(storage::io_error(e)),
        }
        if component == home {
            break;
        }
    }
    Ok(())
}
fn path(home: &Path, profile: Option<&str>, key: FileKey) -> Result<PathBuf> {
    let path = match key {
        FileKey::ProfilePatch => {
            profile_dir(home, profile.ok_or_else(|| error("请选择 profile"))?)?.join(PATCH)
        }
        FileKey::HomePatch => home.join(PATCH),
        FileKey::Settings => {
            let path = home.join("settings.yaml");
            if !path.exists() {
                return Err(error("没有旧版 settings.yaml，不会创建此文件"));
            }
            path
        }
        FileKey::Environment => home.join(".env"),
        FileKey::Credentials => home.join(".credentials.yaml"),
    };
    no_symlinks(home, &path)?;
    Ok(path)
}
fn backup(data: &Path, profile: Option<&str>, key: FileKey) -> PathBuf {
    // The profile itself never becomes a path component in the private backup store.
    let owner = if key == FileKey::ProfilePatch {
        profile.unwrap_or("")
    } else {
        "home"
    };
    data.join("dsh-previous").join(format!(
        "{}-{}.yaml",
        key.name(),
        storage::digest(owner.as_bytes())
    ))
}
fn unique(node: YamlNode, depth: usize) -> Result<()> {
    if depth > 64 {
        return Err(error("DSH YAML 嵌套超限"));
    }
    if let Some(map) = node.as_mapping() {
        let mut keys = BTreeSet::new();
        for (key, value) in map.iter() {
            let key = key
                .as_scalar()
                .ok_or_else(|| error("DSH YAML 映射键必须是普通标量"))?
                .as_string();
            if !keys.insert(key) {
                return Err(error("DSH YAML 含重复键，请先合并"));
            }
            unique(value, depth + 1)?;
        }
    } else if let Some(seq) = node.as_sequence() {
        for value in seq.values() {
            unique(value, depth + 1)?;
        }
    }
    Ok(())
}
fn validate(key: FileKey, raw: &str) -> Result<()> {
    if raw.len() as u64 > LIMIT {
        return Err(error("DSH 配置超过大小限制"));
    }
    let doc = yaml(raw)?;
    match key {
        FileKey::ProfilePatch | FileKey::HomePatch => {
            let seq = doc
                .as_sequence()
                .ok_or_else(|| error("DSH patch 必须是 YAML 数组"))?;
            unique(YamlNode::Sequence(seq.clone()), 0)?;
            for id in [PI, DEFAULT] {
                find_row(&seq, id)?;
            }
        }
        FileKey::Settings => {
            let map = doc
                .as_mapping()
                .ok_or_else(|| error("DSH settings 必须是 YAML 映射"))?;
            unique(YamlNode::Mapping(map), 0)?;
        }
        _ => return Err(error("环境和凭据文件只读")),
    }
    Ok(())
}
fn mask(key: FileKey, raw: Option<&str>) -> String {
    if key == FileKey::Environment {
        let mut names: Vec<_> = raw
            .unwrap_or("")
            .lines()
            .filter_map(|line| {
                let line = line.trim().strip_prefix("export ").unwrap_or(line.trim());
                let (name, _) = line.split_once('=')?;
                let name = name.trim();
                (!name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'))
                    .then(|| format!("{name}=••••"))
            })
            .collect();
        names.sort();
        names.dedup();
        return names.join("\n");
    }
    let Ok(doc) = credentials(raw) else {
        return "凭据格式无法读取（内容已遮蔽）".into();
    };
    let mut rows = vec![];
    for field in ["refs", "records"] {
        if let Some(map) = doc.get_mapping(field) {
            for name in map
                .keys()
                .filter_map(|k| k.as_scalar().map(|s| s.as_string()))
            {
                if name.len() <= 160
                    && name
                        .chars()
                        .all(|c| c.is_alphanumeric() || "_-/.:".contains(c))
                {
                    rows.push(format!("{field}.{name}: ••••"));
                }
            }
        }
    }
    rows.sort();
    rows.join("\n")
}

pub(super) fn protected_patch(snapshot: &Snapshot, raw: &str) -> Result<()> {
    let current = sequence(raw)?;
    let applied = sequence(&snapshot.patch.after)?;
    check_route(owned_route(&current)?, owned_route(&applied)?)?;
    for id in [PI, DEFAULT] {
        if id == DEFAULT && !snapshot.options.make_default {
            continue;
        }
        for key in ["name", "disabled"] {
            if !equal(
                find_row(&current, id)?.and_then(|(_, r)| r.get(key)),
                find_row(&applied, id)?.and_then(|(_, r)| r.get(key)),
            ) {
                return Err(conflict());
            }
        }
    }
    if snapshot.options.make_default {
        let get = |s: &Sequence| -> Result<Option<YamlNode>> {
            Ok(find_row(s, DEFAULT)?
                .and_then(|(_, r)| r.get_mapping("config"))
                .and_then(|c| c.get("provider")))
        };
        if !equal(get(&current)?, get(&applied)?) {
            return Err(conflict_field("默认供应商"));
        }
    }
    Ok(())
}
fn protected_overlay(key: FileKey, before: &str, after: &str) -> Result<()> {
    // A new config replaces the whole effective plugin config, even if its
    // providers map does not mention our route. Check the candidate overlay.
    for id in [PI, DEFAULT] {
        let node = |raw: &str| -> Result<Option<YamlNode>> {
            let node = if key == FileKey::Settings {
                yaml(raw)?.get(id)
            } else {
                find_row(&sequence(raw)?, id)?.map(|(_, row)| YamlNode::Mapping(row))
            };
            if id == DEFAULT {
                if let Some(config) = node.as_ref().and_then(|n| n.as_mapping()).and_then(|m| {
                    if key == FileKey::Settings {
                        Some(m.clone())
                    } else {
                        m.get_mapping("config")
                    }
                }) {
                    config.remove("model");
                    config.remove("reasoningEffort");
                }
            }
            Ok(node)
        };
        if !equal(node(before)?, node(after)?) {
            return Err(error(
                "DSH 接入期间不能通过全局补丁或旧版 settings 覆盖受管连接；请先停用",
            ));
        }
    }
    Ok(())
}

impl Gateway {
    pub fn read_dsh_file(
        &self,
        home: &Path,
        profile: Option<&str>,
        key: FileKey,
    ) -> Result<FileView> {
        let mut profiles = selection::profiles(home)?;
        profiles.sort_by_key(|p| (p != "desktop", p.clone()));
        let profile = profile
            .filter(|p| profiles.iter().any(|item| item == p))
            .or_else(|| profiles.first().map(String::as_str));
        let path = path(home, profile, key)?;
        let raw = text(&path)?;
        let mut files = vec![FileKey::ProfilePatch];
        if home.join(PATCH).is_file() {
            files.push(FileKey::HomePatch);
        }
        if home.join("settings.yaml").exists() {
            files.push(FileKey::Settings);
        }
        if home.join(".env").is_file() {
            files.push(FileKey::Environment);
        }
        if home.join(".credentials.yaml").is_file() {
            files.push(FileKey::Credentials);
        }
        Ok(FileView {
            profiles: profiles.clone(),
            profile: profile.map(str::to_owned),
            key,
            files,
            revision: storage::revision(raw.as_deref().map(str::as_bytes)),
            text: if key.writable() {
                raw.unwrap_or_else(|| "[]\n".into())
            } else {
                mask(key, raw.as_deref())
            },
            read_only: !key.writable(),
            can_restore: key.writable() && backup(&self.0.data, profile, key).is_file(),
        })
    }
    pub fn previous_dsh_file(
        &self,
        home: &Path,
        profile: Option<&str>,
        key: FileKey,
    ) -> Result<String> {
        path(home, profile, key)?;
        if !key.writable() {
            return Err(error("环境和凭据文件只读"));
        }
        text(&backup(&self.0.data, profile, key))?.ok_or_else(|| error("此文件没有上一版备份"))
    }
    pub async fn save_dsh_file(
        &self,
        home: &Path,
        profile: Option<&str>,
        key: FileKey,
        raw: Option<&str>,
        expected: &str,
    ) -> Result<FileView> {
        let _listener = self.0.registry.lifecycle.lock().await;
        let _lifecycle = self.0.lifecycle.lock().await;
        if self.0.data.join(JOURNAL).exists() {
            return Err(error("DSH 存在待恢复事务，请先完成恢复"));
        }
        let path = path(home, profile, key)?;
        let restored;
        let raw = match raw {
            Some(value) => value,
            None => {
                restored = self.previous_dsh_file(home, profile, key)?;
                &restored
            }
        };
        validate(key, raw)?;
        let before = text(&path)?;
        if storage::revision(before.as_deref().map(str::as_bytes)) != expected {
            return Err(conflict());
        }
        let snapshot = self.0.inner.lock().unwrap().store.dsh.clone();
        if let Some(snapshot) = snapshot.filter(|s| s.active) {
            if key == FileKey::ProfilePatch {
                if let Some(active) = profile
                    .and_then(|p| snapshot.for_profile(p))
                    .filter(|p| p.active)
                {
                    protected_patch(&active, raw)?;
                }
            } else {
                protected_overlay(key, before.as_deref().unwrap_or("[]"), raw)?;
            }
        }
        if before.as_deref() != Some(raw) {
            let backup = backup(&self.0.data, profile, key);
            // Write the prior private content first. A failed save leaves the draft
            // and original file intact, and never puts credentials into diagnostics.
            if let Some(previous) = before.as_ref() {
                storage::atomic_write(&backup, previous.as_bytes(), None)?;
            }
            no_symlinks(home, &path)?;
            storage::atomic_write(&path, raw.as_bytes(), Some(expected))?;
        }
        self.0.registry.changed();
        self.read_dsh_file(home, profile, key)
    }
}
