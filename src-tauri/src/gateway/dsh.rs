//! Profile edits are syntax-tree edits. No YAML tag or authentication script is evaluated.
use super::{provider_protocol::Protocol, ClientId, Gateway, View};
use crate::{
    errors::code,
    storage::{self, AppError, Result},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    str::FromStr,
};
use yaml_edit::{AsYaml, Document, Mapping, Sequence, YamlFile, YamlNode};

mod selection;
pub use selection::current;
mod batch;
pub mod editor;
pub use batch::Batch;

const ROUTE: &str = "lich13-switch";
const KEY: &str = "LICH13_SWITCH_GATEWAY_KEY";
const PI: &str = "llm-pi-ai";
const DEFAULT: &str = "agent-default-model";
const PATCH: &str = "cordis.patch.yml";
const JOURNAL: &str = "dsh-recovery.json";
const LIMIT: u64 = 2 * 1024 * 1024;
fn error(message: &str) -> AppError {
    AppError::new(code::CONFIG, message)
}
fn conflict() -> AppError {
    AppError::new(code::CONFLICT, "DSH 受管条目已被外部修改，已保留当前文件")
}
fn text(path: &Path) -> Result<Option<String>> {
    storage::read_bounded(path, LIMIT)?
        .map(|v| String::from_utf8(v).map_err(|_| error("DSH 配置不是 UTF-8")))
        .transpose()
}
fn yaml(raw: &str) -> Result<Document> {
    let file =
        YamlFile::from_str(raw).map_err(|_| error("DSH YAML 格式不支持，请手动处理后重试"))?;
    if file.documents().count() != 1 {
        return Err(error("DSH 配置必须是单个 YAML 文档"));
    }
    file.document().ok_or_else(|| error("DSH 配置为空"))
}
fn patch_file(raw: &str) -> Result<(YamlFile, Sequence)> {
    let file = YamlFile::from_str(raw).map_err(|_| error("DSH YAML 格式不支持"))?;
    if file.documents().count() != 1 {
        return Err(error("DSH 配置必须是单个 YAML 文档"));
    }
    let seq = file
        .document()
        .and_then(|d| d.as_sequence())
        .ok_or_else(|| error("DSH profile patch 必须是 YAML 数组"))?;
    Ok((file, seq))
}
fn credential_file(raw: Option<&str>) -> Result<(YamlFile, Document)> {
    credentials(raw)?;
    let file = YamlFile::from_str(raw.unwrap_or("version: 1\nrefs: {}\n"))
        .map_err(|_| error("DSH 凭据 YAML 无效"))?;
    let doc = file.document().ok_or_else(|| error("DSH 凭据文档为空"))?;
    Ok((file, doc))
}
fn sequence(raw: &str) -> Result<Sequence> {
    yaml(raw)?
        .as_sequence()
        .ok_or_else(|| error("DSH profile patch 必须是 YAML 数组"))
}
fn string(node: Option<YamlNode>) -> Option<String> {
    node?.as_scalar().map(|s| s.as_string())
}
fn mapping(parent: &Mapping, key: &str) -> Result<Mapping> {
    if !parent.contains_key(key) {
        parent.set(key, Mapping::new());
    }
    parent
        .get_mapping(key)
        .ok_or_else(|| error("受管配置必须是普通 YAML 映射，不能是标签或别名"))
}
fn find_row(seq: &Sequence, id: &str) -> Result<Option<(usize, Mapping)>> {
    let mut found = None;
    for (index, node) in seq.values().enumerate() {
        let row = node
            .as_mapping()
            .ok_or_else(|| error("DSH patch 行必须是映射"))?;
        if string(row.get("id")).as_deref() == Some(id) {
            if found.is_some() {
                return Err(error("DSH 同一受管 ID 有多行补丁，请先合并"));
            }
            found = Some((index, row.clone()));
        }
        if row.get_sequence("insert").is_some_and(|s| {
            s.values()
                .any(|n| string(n.get("id")).as_deref() == Some(id))
        }) {
            return Err(error(
                "受管行位于 insert 补丁中，请先将其配置移至独立 ID 补丁",
            ));
        }
    }
    Ok(found)
}
fn row_config(seq: &Sequence, id: &str, inherited: Option<&Mapping>) -> Result<Mapping> {
    let row = find_row(seq, id)?
        .ok_or_else(|| error("缺少 DSH 配置行"))?
        .1;
    if row
        .get("disabled")
        .is_some_and(|n| n.to_bool() != Some(false))
    {
        return Err(error("DSH 受管插件被禁用或使用动态条件，请人工选择"));
    }
    if !row.contains_key("config") {
        row.set("config", inherited.cloned().unwrap_or_else(Mapping::new));
    }
    mapping(&row, "config")
}
fn editable_patch(raw: &str, make_default: bool) -> Result<(YamlFile, Sequence)> {
    let (_, seq) = patch_file(raw)?;
    // yaml-edit cannot append a block mapping to a flow sequence, and appending
    // after a trailing comment can join the new row to that comment. Insert
    // complete rows before the existing block instead, leaving its bytes intact.
    if seq.is_flow_style() && !seq.is_empty() {
        return Err(error("接入前请将 profile patch 展开为多行 YAML 数组"));
    }
    let mut rows = String::new();
    for id in [PI, DEFAULT] {
        if (id == PI || make_default) && find_row(&seq, id)?.is_none() {
            rows.push_str(&format!("- id: {id}\n"));
        }
    }
    if rows.is_empty() {
        return patch_file(raw);
    }
    let range = seq.as_node().unwrap().text_range();
    let start = usize::from(range.start());
    let end = if seq.is_empty() {
        usize::from(range.end())
    } else {
        start
    };
    let next = format!("{}{}{}", &raw[..start], rows, &raw[end..]);
    patch_file(&next)
}
fn profile_dir(home: &Path, profile: &str) -> Result<PathBuf> {
    if profile.is_empty()
        || profile.len() > 128
        || !profile
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.'))
        || profile == "."
        || profile == ".."
    {
        return Err(error("DSH profile 名称无效"));
    }
    let path = home.join("profiles").join(profile);
    if !path.is_dir() || !path.join("package.json").is_file() {
        return Err(error(
            "请选择已存在的 DSH profile；旧 config.yaml 请先通过 DSH 升级",
        ));
    }
    if std::fs::symlink_metadata(&path)
        .map_err(storage::io_error)?
        .file_type()
        .is_symlink()
    {
        return Err(error("DSH profile 不能是符号链接"));
    }
    editor::no_symlinks(home, &path)?;
    Ok(path)
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Preview {
    pub profiles: Vec<String>,
    pub profile: Option<String>,
    pub revision: String,
    pub models: Vec<String>,
    pub attached: bool,
    pub protocol: Protocol,
    pub model: Option<String>,
    pub make_default: bool,
    pub warning: Option<String>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Options {
    pub profile: String,
    pub protocol: Protocol,
    pub model: Option<String>,
    pub make_default: bool,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub options: Options,
    pub active: bool,
    patch: Change,
    environment: Change,
    credentials: Change,
    #[serde(default)]
    profiles: BTreeMap<String, ProfileSnapshot>,
}
#[derive(Clone, Serialize, Deserialize)]
struct ProfileSnapshot {
    options: Options,
    active: bool,
    patch: Change,
}
impl Snapshot {
    fn entries(&self) -> BTreeMap<String, ProfileSnapshot> {
        if self.profiles.is_empty() {
            BTreeMap::from([(
                self.options.profile.clone(),
                ProfileSnapshot {
                    options: self.options.clone(),
                    active: self.active,
                    patch: self.patch.clone(),
                },
            )])
        } else {
            self.profiles.clone()
        }
    }
    fn for_profile(&self, profile: &str) -> Option<Self> {
        self.entries().get(profile).map(|p| Self {
            options: p.options.clone(),
            active: p.active,
            patch: p.patch.clone(),
            environment: self.environment.clone(),
            credentials: self.credentials.clone(),
            profiles: BTreeMap::new(),
        })
    }
    fn active_profiles(&self) -> Vec<Self> {
        self.entries()
            .values()
            .filter(|p| p.active)
            .filter_map(|p| self.for_profile(&p.options.profile))
            .collect()
    }
}
#[derive(Clone, Serialize, Deserialize)]
struct Change {
    before: Option<String>,
    after: String,
}
impl Change {
    fn read(path: &Path) -> Result<Self> {
        let before = text(path)?;
        Ok(Self {
            after: before.clone().unwrap_or_default(),
            before,
        })
    }
}
struct Input {
    dir: PathBuf,
    patch: Change,
    environment: Change,
    credentials: Change,
    layers: Vec<(PathBuf, String)>,
    configs: BTreeMap<String, Mapping>,
    names: BTreeMap<String, String>,
}
impl Input {
    fn read(home: &Path, profile: &str) -> Result<Self> {
        let dir = profile_dir(home, profile)?;
        let mut input = Self {
            patch: Change::read(&dir.join(PATCH))?,
            environment: Change::read(&home.join(".env"))?,
            credentials: Change::read(&home.join(".credentials.yaml"))?,
            dir,
            layers: vec![],
            configs: BTreeMap::new(),
            names: BTreeMap::new(),
        };
        input.layers.push((
            input.dir.join("package.json"),
            text(&input.dir.join("package.json"))?.unwrap_or_default(),
        ));
        let manifest: serde_json::Value = serde_json::from_str(&input.layers[0].1)
            .map_err(|_| error("DSH profile manifest 无效"))?;
        let bundles = manifest
            .pointer("/dsh/profile/bundles")
            .and_then(|v| v.as_array())
            .ok_or_else(|| error("此 DSH profile 布局不支持，请先升级 DSH"))?;
        if bundles.len() > 128 {
            return Err(error("DSH bundle 数量超限"));
        }
        for bundle in bundles {
            input.bundle(
                home,
                bundle
                    .as_str()
                    .ok_or_else(|| error("DSH bundle 名称无效"))?,
            )?;
        }
        if !input.configs.contains_key(PI) {
            return Err(error(
                "当前 profile 未找到 llm-pi-ai，请在 DSH 启用对应 bundle",
            ));
        }
        for file in [PATCH, "settings.yaml"] {
            if let Some(raw) = text(&home.join(file))? {
                input.layers.push((home.join(file), raw));
            }
        }
        Ok(input)
    }
    fn bundle(&mut self, home: &Path, name: &str) -> Result<()> {
        if name.contains("..")
            || name.starts_with('/')
            || name.contains('\\')
            || name.split('/').count() > 2
        {
            return Err(error("DSH bundle 名称不支持"));
        }
        let mut roots = vec![
            self.dir.join("node_modules"),
            home.join("profiles/node_modules"),
            home.join("node_modules"),
            home.join("runtime/node_modules"),
            home.join("lib/node_modules"),
        ];
        if let Some(paths) = std::env::var_os("PATH") {
            for path in std::env::split_paths(&paths) {
                for file in ["dsh", "dsh.cmd"] {
                    if let Ok(binary) = std::fs::canonicalize(path.join(file)) {
                        for ancestor in binary.ancestors().take(6) {
                            roots.push(ancestor.join("node_modules"));
                            if ancestor.file_name().is_some_and(|v| v == "node_modules") {
                                roots.push(ancestor.to_owned());
                            }
                        }
                    }
                }
            }
        }
        let package = roots
            .iter()
            .map(|r| r.join(name))
            .find(|p| p.join("package.json").is_file())
            .ok_or_else(|| error("找不到 DSH bundle：请先通过 DSH 启动所选 profile 以安装依赖，并确认 DSH 可执行文件在 PATH 中"))?;
        let manifest_path = package.join("package.json");
        let raw = text(&manifest_path)?.ok_or_else(|| error("DSH bundle manifest 缺失"))?;
        let manifest: serde_json::Value =
            serde_json::from_str(&raw).map_err(|_| error("DSH bundle manifest 无效"))?;
        self.layers.push((manifest_path, raw));
        let files = manifest
            .pointer("/dsh/bundle/patch")
            .ok_or_else(|| error("DSH bundle patch 未声明"))?;
        let files = if let Some(path) = files.as_str() {
            vec![path]
        } else {
            files
                .as_array()
                .ok_or_else(|| error("DSH bundle patch 不支持"))?
                .iter()
                .map(|v| v.as_str().ok_or_else(|| error("DSH patch 路径无效")))
                .collect::<Result<Vec<_>>>()?
        };
        if files.len() > 64 {
            return Err(error("DSH patch 数量超限"));
        }
        for file in files {
            if Path::new(file).is_absolute()
                || Path::new(file)
                    .components()
                    .any(|c| matches!(c, std::path::Component::ParentDir))
            {
                return Err(error("DSH patch 路径越界"));
            }
            let path = package.join(file);
            let raw = text(&path)?.ok_or_else(|| error("DSH bundle patch 缺失"))?;
            self.read_layer(&sequence(&raw)?, 0)?;
            self.layers.push((path, raw));
        }
        Ok(())
    }
    fn read_layer(&mut self, seq: &Sequence, depth: usize) -> Result<()> {
        if depth > 8 {
            return Err(error("DSH patch 嵌套超限"));
        }
        for node in seq.values() {
            let row = node
                .as_mapping()
                .ok_or_else(|| error("DSH bundle patch 必须是映射"))?;
            if let Some(insert) = row.get_sequence("insert") {
                self.read_layer(&insert, depth + 1)?;
            }
            if let Some(id) = string(row.get("id")).filter(|s| [PI, DEFAULT].contains(&s.as_str()))
            {
                if let Some(name) = string(row.get("name")) {
                    if self.names.get(&id).is_some_and(|old| old != &name) {
                        continue;
                    }
                    self.names.insert(id.clone(), name);
                }
                if row
                    .get("disabled")
                    .is_some_and(|v| v.to_bool() != Some(false))
                {
                    return Err(error("DSH 模型插件被禁用或使用动态条件，需人工处理"));
                }
                if row.contains_key("config") {
                    let config = row
                        .get_mapping("config")
                        .ok_or_else(|| error("DSH 模型插件配置含动态标签，无法安全修改"))?;
                    self.configs.insert(id, config);
                } else {
                    self.configs.entry(id).or_default();
                }
            }
        }
        Ok(())
    }
    fn revision(&self) -> String {
        storage::digest(
            format!(
                "{:?}{:?}{:?}{:?}",
                self.patch.before, self.environment.before, self.credentials.before, self.layers
            )
            .as_bytes(),
        )
    }
    fn unchanged_layers(&self) -> Result<()> {
        for (p, raw) in &self.layers {
            if text(p)?.as_deref() != Some(raw) {
                return Err(conflict());
            }
        }
        Ok(())
    }
}
fn api(protocol: Protocol) -> &'static str {
    match protocol {
        Protocol::Responses => "openai-responses",
        Protocol::ChatCompletions => "openai-completions",
        Protocol::Messages => "anthropic-messages",
    }
}
fn env_value(raw: &str) -> Result<Option<String>> {
    let values: Vec<_> = raw
        .lines()
        .filter(|l| {
            l.trim_start()
                .strip_prefix("export ")
                .unwrap_or(l.trim_start())
                .split_once('=')
                .is_some_and(|(k, _)| k.trim() == KEY)
        })
        .collect();
    if values.len() > 1 {
        return Err(error("DSH .env 包含重复网关引用，请先合并"));
    }
    Ok(values.first().map(|s| s.to_string()))
}
fn env_set(raw: &str, line: Option<&str>) -> Result<String> {
    let old = env_value(raw)?;
    if old.as_deref() == line {
        return Ok(raw.into());
    }
    if let Some(old) = old {
        return Ok(raw.replacen(&old, line.unwrap_or(""), 1));
    }
    Ok(format!(
        "{raw}{}{}",
        if raw.is_empty() || raw.ends_with('\n') {
            ""
        } else {
            "\n"
        },
        line.map(|s| format!("{s}\n")).unwrap_or_default()
    ))
}
fn credentials(raw: Option<&str>) -> Result<Document> {
    let doc = yaml(raw.unwrap_or("version: 1\nrefs: {}\n"))?;
    if doc.get("version").and_then(|v| v.to_i64()) != Some(1) || doc.as_mapping().is_none() {
        return Err(error("DSH 凭据文件版本不支持，请通过 DSH 升级"));
    }
    Ok(doc)
}
#[cfg(test)]
fn prepare(
    input: &mut Input,
    options: Options,
    token: &str,
    port: u16,
    models: &[serde_json::Value],
    previous: Option<&Snapshot>,
) -> Result<Snapshot> {
    prepare_shared(input, options, token, port, models, previous, previous)
}
fn prepare_shared(
    input: &mut Input,
    options: Options,
    token: &str,
    port: u16,
    models: &[serde_json::Value],
    previous: Option<&Snapshot>,
    shared: Option<&Snapshot>,
) -> Result<Snapshot> {
    let (patch_file, patch) = editable_patch(
        input.patch.before.as_deref().unwrap_or("[]\n"),
        options.make_default,
    )?;
    let config = row_config(&patch, PI, input.configs.get(PI))?;
    let providers = mapping(&config, "providers")?;
    if providers.contains_key(ROUTE) && previous.is_none_or(|s| !s.active) {
        return Err(error(
            "DSH 已有同名 lich13-switch 路由，已保留，请先重命名或撤销原接入",
        ));
    }
    if let Some(previous) = previous.filter(|s| s.active) {
        check_owned(previous, input)?;
    }
    if let Some(shared) = shared.filter(|s| s.active) {
        check_shared(shared, input)?;
    }
    let model = options.model.as_deref().filter(|s| !s.is_empty());
    if model.is_some_and(|s| s.len() > 256 || s.chars().any(char::is_control)) {
        return Err(error("DSH 模型 ID 无效"));
    }
    let mut models = models.to_vec();
    if let Some(id) = model {
        if !models.iter().any(|m| m["id"] == id) {
            models.push(serde_json::json!({"id":id}));
        }
    }
    if models.is_empty() {
        return Err(error("所选协议没有有效模型，请手动填写明确的模型 ID"));
    }
    let base = if options.protocol == Protocol::Messages {
        format!("http://127.0.0.1:{port}")
    } else {
        format!("http://127.0.0.1:{port}/v1")
    };
    // JSON is a YAML subset; only the new owned route is serialized.
    let route = yaml(&serde_json::json!({"displayName":"lich13-switch","apiKeyEnv":KEY,"api":api(options.protocol),"baseURL":base,"models":models}).to_string())?.as_mapping().ok_or_else(||error("无法生成 DSH 路由"))?;
    providers.set(ROUTE, route);
    if options.make_default {
        let id = model.ok_or_else(|| error("设为默认时请选择一个明确模型"))?;
        if !input.configs.contains_key(DEFAULT) {
            return Err(error("此 profile 没有 agent-default-model"));
        }
        let default = row_config(&patch, DEFAULT, input.configs.get(DEFAULT))?;
        default.set("provider", ROUTE);
        default.set("model", id);
        default.remove("reasoningEffort");
    } else if let Some(old) = previous.filter(|s| s.active && s.options.make_default) {
        restore_default(
            &patch,
            &sequence(old.patch.before.as_deref().unwrap_or("[]"))?,
            &sequence(&old.patch.after)?,
            input.configs.get(DEFAULT),
        )?;
    }
    let (credential_file, doc) = credential_file(input.credentials.before.as_deref())?;
    let refs = mapping(&doc.as_mapping().unwrap(), "refs")?;
    if shared.is_none_or(|s| !s.active)
        && (refs.contains_key(KEY)
            || env_value(input.environment.before.as_deref().unwrap_or(""))?.is_some())
    {
        return Err(error("DSH 已有同名凭据引用，已保留，请先人工确认"));
    }
    refs.set(KEY, token);
    input.patch.after = patch_file.to_string();
    let generated = sequence(&input.patch.after)?;
    if !equal(owned_route(&generated)?, owned_route(&patch)?)
        || (options.make_default
            && find_row(&generated, DEFAULT)?.is_none_or(|(_, row)| {
                row.get_mapping("config").is_none_or(|config| {
                    string(config.get("provider")).as_deref() != Some(ROUTE)
                        || string(config.get("model")).as_deref() != model
                })
            }))
    {
        return Err(error("无法生成有效 DSH 补丁，原文件未修改"));
    }
    input.environment.after = env_set(
        input.environment.before.as_deref().unwrap_or(""),
        Some(&format!("{KEY}={token}")),
    )?;
    input.credentials.after = credential_file.to_string();
    let mut result = Snapshot {
        options,
        active: true,
        patch: input.patch.clone(),
        environment: input.environment.clone(),
        credentials: input.credentials.clone(),
        profiles: shared.map(Snapshot::entries).unwrap_or_default(),
    };
    if let Some(old) = previous.filter(|s| s.active) {
        result.patch.before.clone_from(&old.patch.before);
    }
    if let Some(old) = shared.filter(|s| s.active) {
        result
            .environment
            .before
            .clone_from(&old.environment.before);
        result
            .credentials
            .before
            .clone_from(&old.credentials.before);
    }
    result.profiles.insert(
        result.options.profile.clone(),
        ProfileSnapshot {
            options: result.options.clone(),
            active: true,
            patch: result.patch.clone(),
        },
    );
    Ok(result)
}
fn owned_route(seq: &Sequence) -> Result<Option<YamlNode>> {
    Ok(find_row(seq, PI)?
        .and_then(|(_, r)| r.get_mapping("config"))
        .and_then(|c| c.get_mapping("providers"))
        .and_then(|p| p.get(ROUTE)))
}
fn equal(a: Option<YamlNode>, b: Option<YamlNode>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => a.yaml_eq(&b),
        (None, None) => true,
        _ => false,
    }
}
fn check_owned(snapshot: &Snapshot, input: &Input) -> Result<()> {
    editor::protected_patch(snapshot, input.patch.before.as_deref().unwrap_or("[]"))?;
    check_shared(snapshot, input)
}
fn check_shared(snapshot: &Snapshot, input: &Input) -> Result<()> {
    if env_value(input.environment.before.as_deref().unwrap_or(""))?
        != env_value(&snapshot.environment.after)?
    {
        return Err(conflict());
    }
    let reference = |raw: Option<&str>| -> Result<Option<YamlNode>> {
        Ok(credentials(raw)?
            .get_mapping("refs")
            .and_then(|r| r.get(KEY)))
    };
    if !equal(
        reference(input.credentials.before.as_deref())?,
        reference(Some(&snapshot.credentials.after))?,
    ) {
        return Err(conflict());
    }
    Ok(())
}
fn restore_field(map: &Mapping, key: &str, original: Option<YamlNode>) {
    if let Some(value) = original {
        map.set(key, value);
    } else {
        map.remove(key);
    }
}
fn restore_default(
    current: &Sequence,
    before: &Sequence,
    after: &Sequence,
    inherited: Option<&Mapping>,
) -> Result<()> {
    let Some((index, row)) = find_row(current, DEFAULT)? else {
        return Err(conflict());
    };
    let old = find_row(before, DEFAULT)?;
    if old.is_none()
        && find_row(after, DEFAULT)?.is_some_and(|(_, r)| r.to_string() == row.to_string())
    {
        current.remove(index);
        return Ok(());
    }
    let config = mapping(&row, "config")?;
    let original = old.as_ref().and_then(|(_, r)| r.get_mapping("config"));
    if original.is_none()
        && find_row(after, DEFAULT)?
            .and_then(|(_, r)| r.get_mapping("config"))
            .is_some_and(|c| c.to_string() == config.to_string())
    {
        row.remove("config");
        return Ok(());
    }
    let original = original.as_ref().or(inherited);
    for key in ["provider", "model", "reasoningEffort"] {
        restore_field(&config, key, original.and_then(|c| c.get(key)));
    }
    Ok(())
}
fn restore(snapshot: &Snapshot, input: &mut Input) -> Result<()> {
    check_owned(snapshot, input)?;
    let (patch_file, current) = patch_file(input.patch.before.as_deref().unwrap_or("[]"))?;
    let before = sequence(snapshot.patch.before.as_deref().unwrap_or("[]"))?;
    let after = sequence(&snapshot.patch.after)?;
    let (index, row) = find_row(&current, PI)?.ok_or_else(conflict)?;
    let old = find_row(&before, PI)?;
    if old.is_none() && find_row(&after, PI)?.is_some_and(|(_, r)| r.to_string() == row.to_string())
    {
        current.remove(index);
    } else {
        let providers = mapping(&mapping(&row, "config")?, "providers")?;
        restore_field(&providers, ROUTE, owned_route(&before)?);
    }
    if snapshot.options.make_default {
        restore_default(&current, &before, &after, input.configs.get(DEFAULT))?;
    }
    input.patch.after = patch_file.to_string();
    if current.is_empty() {
        input.patch.after.insert_str(
            usize::from(current.as_node().unwrap().text_range().start()),
            "[]\n",
        );
    }
    sequence(&input.patch.after)?;
    input.environment.after = env_set(
        input.environment.before.as_deref().unwrap_or(""),
        env_value(snapshot.environment.before.as_deref().unwrap_or(""))?.as_deref(),
    )?;
    let (credential_file, doc) = credential_file(input.credentials.before.as_deref())?;
    let refs = mapping(&doc.as_mapping().unwrap(), "refs")?;
    restore_field(
        &refs,
        KEY,
        credentials(snapshot.credentials.before.as_deref())?
            .get_mapping("refs")
            .and_then(|r| r.get(KEY)),
    );
    input.credentials.after = credential_file.to_string();
    Ok(())
}
#[derive(Serialize, Deserialize)]
struct Transaction {
    version: u32,
    #[serde(default)]
    rollback: bool,
    home: PathBuf,
    profile: String,
    patch: Change,
    environment: Change,
    credentials: Change,
    store: Change,
}
impl Transaction {
    fn files(&self, data: &Path) -> Result<Vec<(PathBuf, &Change)>> {
        let dir = profile_dir(&self.home, &self.profile)?;
        Ok(vec![
            (dir.join(PATCH), &self.patch),
            (self.home.join(".env"), &self.environment),
            (self.home.join(".credentials.yaml"), &self.credentials),
            (data.join("gateway.json"), &self.store),
        ])
    }
    fn apply(&self, data: &Path) -> Result<()> {
        self.write(data, self.rollback)
    }
    fn write(&self, data: &Path, rollback: bool) -> Result<()> {
        if self.version != 1 || !self.home.is_absolute() {
            return Err(error("DSH 恢复事务无效"));
        }
        let files = self.files(data)?;
        for (path, change) in &files {
            let current = text(path)?;
            if current != change.before && current.as_deref() != Some(&change.after) {
                return Err(conflict());
            }
            if change.after.len() as u64 > LIMIT {
                return Err(error("DSH 配置超过大小限制"));
            }
        }
        let files: Vec<_> = if rollback {
            files.into_iter().rev().collect()
        } else {
            files
        };
        for (path, change) in files {
            let current = text(&path)?;
            if current != change.before && current.as_deref() != Some(&change.after) {
                return Err(conflict());
            }
            let desired = if rollback {
                change.before.as_deref()
            } else {
                Some(change.after.as_str())
            };
            if current.as_deref() != desired {
                if let Some(raw) = desired {
                    storage::atomic_write(
                        &path,
                        raw.as_bytes(),
                        Some(&storage::revision(current.as_deref().map(str::as_bytes))),
                    )?;
                } else {
                    std::fs::remove_file(&path).map_err(storage::io_error)?;
                    storage::sync_parent(&path)?;
                }
            }
        }
        std::fs::remove_file(data.join(JOURNAL)).map_err(storage::io_error)?;
        storage::sync_parent(&data.join(JOURNAL))
    }
}
pub fn recover(data: &Path) -> Result<()> {
    if let Some(raw) = storage::read_bounded(&data.join(JOURNAL), 16 * 1024 * 1024)? {
        let tx: Transaction =
            serde_json::from_slice(&raw).map_err(|_| error("DSH 恢复事务损坏，已保留文件"))?;
        tx.apply(data)?;
    }
    Ok(())
}
fn commit(
    data: &Path,
    home: &Path,
    input: &Input,
    store: &super::model::Store,
    expected: &str,
) -> Result<String> {
    input.unchanged_layers()?;
    let before = text(&data.join("gateway.json"))?;
    if storage::revision(before.as_deref().map(str::as_bytes)) != expected {
        return Err(conflict());
    }
    let after = serde_json::to_string_pretty(store).map_err(|_| error("无法生成 DSH 接入事务"))?;
    let tx = Transaction {
        version: 1,
        rollback: true,
        home: home.to_owned(),
        profile: input.dir.file_name().unwrap().to_string_lossy().into(),
        patch: input.patch.clone(),
        environment: input.environment.clone(),
        credentials: input.credentials.clone(),
        store: Change {
            before,
            after: after.clone(),
        },
    };
    if tx
        .files(data)?
        .iter()
        .any(|(_, change)| change.after.len() as u64 > LIMIT)
    {
        return Err(error("DSH 配置超过大小限制"));
    }
    let raw = serde_json::to_vec(&tx).map_err(|_| error("无法生成 DSH 接入事务"))?;
    storage::atomic_write_bounded(&data.join(JOURNAL), &raw, Some("missing"), 16 * 1024 * 1024)?;
    if let Err(failure) = tx.write(data, false) {
        if tx.write(data, true).is_err() {
            return Err(error(
                "DSH 写入失败，回滚遇到冲突；已保留恢复事务和外部修改",
            ));
        }
        return Err(failure);
    }
    Ok(storage::digest(after.as_bytes()))
}

impl Gateway {
    fn recover_dsh_state(&self) -> Result<()> {
        if self.0.data.join(JOURNAL).exists() {
            recover(&self.0.data)?;
            let (store, revision) = super::model::Store::load(&self.0.data.join("gateway.json"))?;
            let mut state = self.0.inner.lock().unwrap();
            self.replace_store(&mut state, store);
            state.revision = revision;
            state.upgrade_pending = false;
        }
        Ok(())
    }
    pub fn preview_dsh(
        &self,
        home: &Path,
        profile: Option<&str>,
        protocol: Protocol,
    ) -> Result<Preview> {
        let mut profiles = vec![];
        if home.join("profiles").is_dir() {
            for entry in std::fs::read_dir(home.join("profiles"))
                .map_err(storage::io_error)?
                .take(512)
            {
                let e = entry.map_err(storage::io_error)?;
                let name = e.file_name().to_string_lossy().into_owned();
                if profile_dir(home, &name).is_ok() {
                    profiles.push(name);
                }
            }
        }
        profiles.sort();
        let input = profile.map(|p| Input::read(home, p)).transpose()?;
        let store = self.0.inner.lock().unwrap().store.clone();
        let snapshot = store
            .dsh
            .as_ref()
            .and_then(|s| profile.and_then(|p| s.for_profile(p)));
        let current = profile.map(|profile| {
            selection::profile_current(
                home,
                profile,
                &store,
                &[self
                    .0
                    .registry
                    .listener
                    .view()
                    .map(|v| v.port)
                    .unwrap_or(43721)],
                &|name| std::env::var(name).ok(),
            )
        });
        Ok(Preview {
            revision: input.as_ref().map(Input::revision).unwrap_or_default(),
            profiles,
            profile: profile.map(str::to_owned),
            models: self.model_union(Some(protocol))?,
            attached: snapshot.as_ref().is_some_and(|s| s.active),
            protocol,
            model: snapshot.as_ref().and_then(|s| s.options.model.clone()),
            make_default: snapshot.as_ref().is_some_and(|s| s.options.make_default),
            warning: if home.join("config.yaml").is_file() && !home.join("profiles").is_dir() {
                Some("仅发现旧 config.yaml，请先通过 DSH 升级；不会自动迁移".into())
            } else {
                Some(format!(
                    "{}；默认模型仅影响新会话",
                    current
                        .and_then(|c| c.warning)
                        .unwrap_or_else(|| "启动参数、动态 YAML 和已保存的会话选择未确认".into())
                ))
            },
        })
    }
    pub async fn attach_dsh(
        &self,
        home: &Path,
        options: Options,
        expected: &str,
        preview: Option<&str>,
    ) -> Result<View> {
        if self.0.client != ClientId::Dsh {
            return Err(error("此操作仅适用于 DSH"));
        }
        let _listener = self.0.registry.lifecycle.lock().await;
        let _lifecycle = self.0.lifecycle.lock().await;
        self.attach_dsh_locked(home, options, expected, preview)
            .await
    }
    async fn attach_dsh_locked(
        &self,
        home: &Path,
        options: Options,
        expected: &str,
        preview: Option<&str>,
    ) -> Result<View> {
        self.recover_dsh_state()?;
        let (old, token) = {
            let s = self.0.inner.lock().unwrap();
            if self.view_revision(&s)? != expected {
                return Err(conflict());
            }
            (s.store.dsh.clone(), s.store.local_token.clone())
        };
        if old
            .as_ref()
            .is_some_and(|s| s.active && s.options.protocol != options.protocol)
        {
            return Err(error("请先停用全部 DSH profile，再切换协议"));
        }
        let listener = self.0.registry.listener.prepare().await?;
        self.0.registry.update_ports()?;
        let mut input = Input::read(home, &options.profile)?;
        if preview.is_some_and(|v| v != input.revision()) {
            return Err(conflict());
        }
        let models = self
            .model_union(Some(options.protocol))?
            .into_iter()
            .map(|id| {
                let mut value = serde_json::json!({"id":id});
                if let Some(meta) = self
                    .0
                    .usage
                    .lock()
                    .unwrap()
                    .as_ref()
                    .and_then(|u| u.prices().ok())
                    .and_then(|p| {
                        let (choices, _) = self.0.model_list.snapshot();
                        p.models_dev.metadata_from(&id, choices.source(&id))
                    })
                {
                    if let Some(name) = meta.name {
                        value["name"] = name.into();
                    }
                    if let Some(context) = meta.context {
                        value["contextWindow"] = context.into();
                    }
                    if let Some(output) = meta.output {
                        value["maxTokens"] = output.into();
                    }
                    if let Some(efforts) = meta.reasoning_efforts {
                        let efforts: BTreeMap<_, _> = efforts
                            .into_iter()
                            .filter_map(|(k, v)| {
                                if k == "none" {
                                    Some(("off".to_owned(), v))
                                } else if ["minimal", "low", "medium", "high", "xhigh", "max"]
                                    .contains(&k.as_str())
                                    && v.is_string()
                                {
                                    Some((k, v))
                                } else {
                                    None
                                }
                            })
                            .collect();
                        if !efforts.is_empty() {
                            value["reasoningEfforts"] =
                                serde_json::to_value(efforts).unwrap_or_default();
                        }
                    }
                }
                value
            })
            .collect::<Vec<_>>();
        let token = self.0.registry.unique_local_token(self.0.client, &token);
        let previous = old.as_ref().and_then(|s| s.for_profile(&options.profile));
        let snapshot = prepare_shared(
            &mut input,
            options.clone(),
            &token,
            listener.port,
            &models,
            previous.as_ref(),
            old.as_ref(),
        )?;
        let mut s = self.0.inner.lock().unwrap();
        if self.view_revision_with_listener(&s, &listener.previous_revision) != expected {
            return Err(conflict());
        }
        if s.store.providers.is_empty() {
            return Err(error("请先添加 DSH 供应商"));
        }
        let mut next = s.store.clone();
        next.local_token = token;
        next.dsh = Some(snapshot);
        next.initialized = true;
        next.resume = Some(super::model::Resume {
            home: home.to_owned(),
            desired: true,
            pair_hash: String::new(),
        });
        self.0.registry.listener.activate_with(
            self.0.client,
            listener.socket,
            self.0.registry.clone(),
            || {
                s.revision = commit(&self.0.data, home, &input, &next, &s.revision)?;
                self.replace_store(&mut s, next);
                if !s.running {
                    s.shutdown = Some(tokio::sync::watch::channel(false).0);
                }
                s.running = true;
                s.home = Some(home.to_owned());
                s.error = listener.notice;
                self.0.admission.configure(&s.store.providers, true);
                self.0
                    .admission
                    .configure_retention(s.store.settings.thread_idle_release_seconds);
                self.0.compaction.provider_names(&s.store.providers);
                Ok(())
            },
        )?;
        drop(s);
        self.0.registry.changed();
        self.try_view()
    }
    pub(super) async fn stop_dsh(&self, preserve: bool) -> Result<View> {
        let _listener = self.0.registry.lifecycle.lock().await;
        let _lifecycle = self.0.lifecycle.lock().await;
        self.recover_dsh_state()?;
        {
            let mut s = self.0.inner.lock().unwrap();
            let snapshots = s
                .store
                .dsh
                .as_ref()
                .map(Snapshot::active_profiles)
                .unwrap_or_default();
            for (index, snapshot) in snapshots.iter().enumerate() {
                let home = s
                    .home
                    .clone()
                    .or_else(|| s.store.resume.as_ref().map(|r| r.home.clone()))
                    .ok_or_else(|| error("DSH 恢复目录缺失"))?;
                let mut input = Input::read(&home, &snapshot.options.profile)?;
                restore(snapshot, &mut input)?;
                if index + 1 < snapshots.len() {
                    input.environment.after = input.environment.before.clone().unwrap_or_default();
                    input.credentials.after = input.credentials.before.clone().unwrap_or_default();
                }
                let mut next = s.store.clone();
                if let Some(dsh) = next.dsh.as_mut() {
                    dsh.profiles = dsh.entries();
                    if let Some(profile) = dsh.profiles.get_mut(&snapshot.options.profile) {
                        profile.active = false;
                    }
                    dsh.active = dsh.profiles.values().any(|p| p.active);
                }
                if let Some(resume) = next.resume.as_mut() {
                    resume.desired = preserve;
                }
                s.revision = commit(&self.0.data, &home, &input, &next, &s.revision)?;
                self.replace_store(&mut s, next);
            }
            if let Some(shutdown) = s.shutdown.take() {
                let _ = shutdown.send(true);
            }
            s.running = false;
            s.error = None;
            self.0.admission.configure(&s.store.providers, false);
        }
        self.0.registry.listener.deactivate(self.0.client).await?;
        self.0.clients.lock().unwrap().clear();
        self.0.registry.changed();
        self.try_view()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    const PATCH_TEXT: &str = "# profile comment\n- id: unrelated\n  config:\n    callback: !!js process.cwd() # keep tag\n- id: llm-pi-ai\n  config:\n    extra: keep\n    providers:\n      existing:\n        api: openai-completions\n        baseURL: https://example.invalid/v1 # keep endpoint\n        models: [{id: existing-model}]\n";
    pub(super) fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let profile = dir.path().join("profiles/default");
        fs::create_dir_all(&profile).unwrap();
        fs::write(
            profile.join("package.json"),
            r#"{"dsh":{"profile":{"bundles":["fixture-bundle"]}}}"#,
        )
        .unwrap();
        let bundle = profile.join("node_modules/fixture-bundle");
        fs::create_dir_all(&bundle).unwrap();
        fs::write(
            bundle.join("package.json"),
            r#"{"dsh":{"bundle":{"patch":"./cordis.patch.yml"}}}"#,
        )
        .unwrap();
        fs::write(bundle.join(PATCH),"- insert:\n    - id: llm-pi-ai\n      name: '@deepseek-ai/dsh-llm-pi-ai'\n      config:\n        providers:\n          inherited: {api: openai-completions, baseURL: 'https://example.invalid/v1'}\n    - id: agent-default-model\n      name: '@deepseek-ai/dsh-agent-default-model'\n      config: {provider: deepseek-official, model: native}\n").unwrap();
        fs::write(profile.join(PATCH), PATCH_TEXT).unwrap();
        fs::write(dir.path().join(".env"), "# keep env\nOTHER=fixture\n").unwrap();
        fs::write(
            dir.path().join(".credentials.yaml"),
            "version: 1\nrefs:\n  OTHER: fixture\nrecords: {} # keep OAuth records\n",
        )
        .unwrap();
        dir
    }
    fn options(protocol: Protocol) -> Options {
        Options {
            profile: "default".into(),
            protocol,
            model: Some("fixture-model".into()),
            make_default: true,
        }
    }
    #[test]
    fn three_protocols_preserve_yaml_and_restore_owned_fields() {
        for protocol in [
            Protocol::ChatCompletions,
            Protocol::Responses,
            Protocol::Messages,
        ] {
            let dir = fixture();
            let mut input = Input::read(dir.path(), "default").unwrap();
            let snap = prepare(
                &mut input,
                options(protocol),
                "fixture-local",
                43721,
                &[],
                None,
            )
            .unwrap();
            assert!(input.patch.after.contains("# profile comment"));
            for keep in [
                "!!js process.cwd()",
                "# keep endpoint",
                "extra: keep",
                "existing:",
            ] {
                assert!(
                    input.patch.after.contains(keep),
                    "lost {keep}: {}",
                    input.patch.after
                );
            }
            let route = owned_route(&sequence(&input.patch.after).unwrap())
                .unwrap()
                .unwrap();
            assert_eq!(string(route.get("api")).as_deref(), Some(api(protocol)));
            assert_eq!(
                string(route.get("baseURL")).unwrap().ends_with("/v1"),
                protocol != Protocol::Messages
            );
            assert!(input.credentials.after.contains("version: 1"));
            assert!(input.credentials.after.contains("# keep OAuth records"));
            for (p, c) in [
                (input.dir.join(PATCH), &input.patch),
                (dir.path().join(".env"), &input.environment),
                (dir.path().join(".credentials.yaml"), &input.credentials),
            ] {
                storage::atomic_write(&p, c.after.as_bytes(), None).unwrap();
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                for p in [".env", ".credentials.yaml"] {
                    assert_eq!(
                        fs::metadata(dir.path().join(p))
                            .unwrap()
                            .permissions()
                            .mode()
                            & 0o777,
                        0o600
                    );
                }
            }
            let current = fs::read_to_string(input.dir.join(PATCH))
                .unwrap()
                .replace("extra: keep", "extra: edited");
            fs::write(input.dir.join(PATCH), current).unwrap();
            let mut current = Input::read(dir.path(), "default").unwrap();
            restore(&snap, &mut current).unwrap();
            assert!(current.patch.after.contains("extra: edited"));
            assert!(!current.patch.after.contains("lich13-switch"));
            assert!(!current.environment.after.contains(KEY));
            assert!(!current.credentials.after.contains(KEY));
            assert!(current.credentials.after.contains("OTHER: fixture"));
            let rows = sequence(&current.patch.after).unwrap();
            assert!(find_row(&rows, DEFAULT).unwrap().is_none());
        }
    }
    #[test]
    fn collision_external_edit_dynamic_config_and_old_credentials_are_preserved() {
        let dir = fixture();
        let mut input = Input::read(dir.path(), "default").unwrap();
        let snapshot = prepare(
            &mut input,
            options(Protocol::ChatCompletions),
            "fixture-local",
            43721,
            &[],
            None,
        )
        .unwrap();
        input.patch.before = Some(input.patch.after.replace("fixture-model", "external-model"));
        input.environment.before = Some(input.environment.after.clone());
        input.credentials.before = Some(input.credentials.after.clone());
        assert!(restore(&snapshot, &mut input).is_err());
        assert!(prepare(
            &mut input,
            options(Protocol::ChatCompletions),
            "fixture-local",
            43721,
            &[],
            None
        )
        .is_err());
        let mut input = Input::read(dir.path(), "default").unwrap();
        input.patch.before = Some("- id: llm-pi-ai\n  config: !!js process.env.CONFIG\n".into());
        assert!(prepare(
            &mut input,
            options(Protocol::Messages),
            "fixture-local",
            43721,
            &[],
            None
        )
        .is_err());
        assert!(credentials(Some("refs: {}\n")).is_err());
        assert!(ClientId::Dsh.legacy_addresses(43721).is_empty());
    }
    #[tokio::test]
    async fn profile_attach_sync_stop_and_recovery_use_shared_listener() {
        let dir = fixture();
        let primary = Gateway::new(dir.path().join("data")).unwrap();
        let gateway = primary.dsh_companion(dir.path().join("data/dsh")).unwrap();
        let listener = gateway.try_listener_view().unwrap();
        let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = socket.local_addr().unwrap().port();
        drop(socket);
        gateway.set_listener_port(port, &listener.revision).unwrap();
        {
            let mut state = gateway.0.inner.lock().unwrap();
            let mut store = state.store.clone();
            for protocol in [Protocol::ChatCompletions, Protocol::Messages] {
                store
                    .edit(
                        super::super::Edit::SaveDshProvider {
                            id: None,
                            base_url: "https://example.invalid/v1".into(),
                            token: format!("fixture-{}", api(protocol)),
                            name: None,
                            protocol,
                        },
                        false,
                    )
                    .unwrap();
                let p = store.providers.last_mut().unwrap();
                p.queued = true;
                p.allowed_models = Some(vec![api(protocol).into()]);
            }
            state.revision = store
                .persist(&gateway.0.data.join("gateway.json"), &state.revision)
                .unwrap();
            state.store = store;
        }
        let preview = gateway
            .preview_dsh(dir.path(), Some("default"), Protocol::ChatCompletions)
            .unwrap();
        assert_eq!(preview.models, vec!["openai-completions"]);
        let view = gateway
            .attach_dsh(
                dir.path(),
                options(Protocol::ChatCompletions),
                &gateway.try_view().unwrap().revision,
                Some(&preview.revision),
            )
            .await
            .unwrap();
        assert!(view.running);
        assert_eq!(view.listener.running_clients, vec![ClientId::Dsh]);
        let preview = gateway
            .preview_dsh(dir.path(), Some("default"), Protocol::ChatCompletions)
            .unwrap();
        let view = gateway
            .attach_dsh(
                dir.path(),
                options(Protocol::ChatCompletions),
                &view.revision,
                Some(&preview.revision),
            )
            .await
            .unwrap();
        assert!(view.running);
        assert!(!gateway.stop().await.unwrap().running);
        assert!(
            !fs::read_to_string(dir.path().join("profiles/default/cordis.patch.yml"))
                .unwrap()
                .contains("lich13-switch")
        );
        assert!(!gateway.0.data.join(JOURNAL).exists());
    }
    #[test]
    fn partial_multi_file_transaction_recovers_without_erasing_external_changes() {
        let dir = fixture();
        let data = dir.path().join("data");
        storage::private_dir(&data).unwrap();
        let mut input = Input::read(dir.path(), "default").unwrap();
        let snapshot = prepare(
            &mut input,
            options(Protocol::ChatCompletions),
            "fixture-local",
            43721,
            &[],
            None,
        )
        .unwrap();
        let store = super::super::model::Store {
            dsh: Some(snapshot),
            ..Default::default()
        };
        let tx = Transaction {
            version: 1,
            rollback: false,
            home: dir.path().to_owned(),
            profile: "default".into(),
            patch: input.patch.clone(),
            environment: input.environment.clone(),
            credentials: input.credentials.clone(),
            store: Change {
                before: None,
                after: serde_json::to_string(&store).unwrap(),
            },
        };
        storage::atomic_write_bounded(
            &data.join(JOURNAL),
            &serde_json::to_vec(&tx).unwrap(),
            Some("missing"),
            16 * 1024 * 1024,
        )
        .unwrap();
        storage::atomic_write(&input.dir.join(PATCH), input.patch.after.as_bytes(), None).unwrap();
        fs::write(dir.path().join(".env"), "EXTERNAL=fixture\n").unwrap();
        assert!(recover(&data).is_err());
        assert_eq!(
            text(&dir.path().join(".env")).unwrap().as_deref(),
            Some("EXTERNAL=fixture\n")
        );
        assert!(data.join(JOURNAL).exists());
        fs::write(dir.path().join(".env"), input.environment.before.unwrap()).unwrap();
        {
            let _fault = storage::faults::inject(storage::faults::Point::Replace);
            assert!(recover(&data).is_err());
        }
        recover(&data).unwrap();
        assert!(!data.join(JOURNAL).exists());
        assert!(text(&data.join("gateway.json"))
            .unwrap()
            .unwrap()
            .contains("fixture-local"));
    }
    #[tokio::test]
    async fn dsh_protocols_forward_original_bytes_and_authenticate_locally() {
        use http_body_util::BodyExt;
        use hyper::{Request, Response};
        for protocol in [
            Protocol::ChatCompletions,
            Protocol::Responses,
            Protocol::Messages,
        ] {
            let upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let upstream_port = upstream.local_addr().unwrap().port();
            let task = tokio::spawn(async move {
                let (stream, _) = upstream.accept().await.unwrap();
                hyper::server::conn::http1::Builder::new()
                    .serve_connection(
                        hyper_util::rt::TokioIo::new(stream),
                        hyper::service::service_fn(
                            move |request: Request<hyper::body::Incoming>| async move {
                                assert!(protocol.accepts(request.uri().path()));
                                if protocol == Protocol::Messages {
                                    assert_eq!(request.headers()["x-api-key"], "fixture-upstream");
                                } else {
                                    assert_eq!(
                                        request.headers()["authorization"],
                                        "Bearer fixture-upstream"
                                    );
                                }
                                let bytes = request.into_body().collect().await.unwrap().to_bytes();
                                Ok::<_, std::convert::Infallible>(
                                    Response::builder()
                                        .header("content-type", "application/json")
                                        .body(super::super::replay::full(bytes))
                                        .unwrap(),
                                )
                            },
                        ),
                    )
                    .await
                    .unwrap();
            });
            let dir = fixture();
            let data = dir.path().join("data");
            storage::private_dir(&data).unwrap();
            let mut store = super::super::model::Store::default();
            store
                .edit(
                    super::super::Edit::SaveDshProvider {
                        id: None,
                        base_url: format!("http://127.0.0.1:{upstream_port}/v1"),
                        token: "fixture-upstream".into(),
                        name: None,
                        protocol,
                    },
                    false,
                )
                .unwrap();
            store
                .persist(&data.join("gateway.json"), "missing")
                .unwrap();
            let gateway = Gateway::new_client(data, ClientId::Dsh, None).unwrap();
            let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = socket.local_addr().unwrap().port();
            drop(socket);
            gateway
                .set_listener_port(port, &gateway.try_listener_view().unwrap().revision)
                .unwrap();
            let view = gateway
                .attach_dsh(
                    dir.path(),
                    options(protocol),
                    &gateway.try_view().unwrap().revision,
                    None,
                )
                .await
                .unwrap();
            let token = gateway.0.inner.lock().unwrap().store.local_token.clone();
            let client = reqwest::Client::builder().no_proxy().build().unwrap();
            let url = format!("http://127.0.0.1:{}/v1/models", view.listener.port);
            assert_eq!(client.get(&url).send().await.unwrap().status(), 401);
            let path = match protocol {
                Protocol::Responses => "/v1/responses",
                Protocol::ChatCompletions => "/v1/chat/completions",
                Protocol::Messages => "/v1/messages",
            };
            let body=br#"{ "model": "fixture-model", "messages": [], "reasoning_content": "fixture", "future": [1, 2] }"#;
            let response = client
                .post(format!("http://127.0.0.1:{}{path}", view.listener.port))
                .bearer_auth(&token)
                .header("content-type", "application/json")
                .body(body.to_vec())
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), 200);
            assert_eq!(response.bytes().await.unwrap().as_ref(), body);
            gateway.stop().await.unwrap();
            task.abort();
        }
    }
}

#[cfg(test)]
mod tests_v02515;
