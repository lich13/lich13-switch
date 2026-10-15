//! Bearer mode owns two `custom` values; API mode owns auth.json and the active base_url.
//! Edits use parser byte spans: unrelated TOML is never serialized again.
use super::{
    codex_api::{self, Connection},
    ClientId,
};
use crate::errors::code;
use crate::storage::{self, AppError, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
};
use toml_edit::{Document, Item};
const FILE: &str = "gateway-recovery.json";
const KEYS: [&str; 2] = ["base_url", "experimental_bearer_token"];
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Pair {
    pub base_url: Option<String>,
    pub token: Option<String>,
}
impl Pair {
    pub fn fingerprint(&self) -> String {
        storage::digest(&serde_json::to_vec(self).expect("credential pair serializes"))
    }
    pub fn new(base: &str, token: &str) -> Self {
        Self {
            base_url: Some(base.into()),
            token: Some(token.into()),
        }
    }
}
#[derive(Serialize, Deserialize)]
struct Journal {
    #[serde(default)]
    client: ClientId,
    version: u32,
    #[serde(default)]
    connection: Connection,
    #[serde(default)]
    grok: Option<super::grok_config::Snapshot>,
    path: PathBuf,
    before: Pair,
    applied: Pair,
    exit: Pair,
    live: bool,
    #[serde(default)]
    config_applied: bool,
    #[serde(default)]
    store_before: Option<String>,
    #[serde(default)]
    store_after: Option<String>,
}
fn parse(text: &str) -> Result<Document<&str>> {
    Document::parse(text)
        .map_err(|_| AppError::new(code::TOML, "配置不是有效的 TOML，请先修复配置"))
}
fn custom<'a>(doc: &'a Document<&str>) -> Result<&'a Item> {
    let profile = doc.get("profile").and_then(Item::as_str);
    let selector = profile
        .and_then(|p| doc.get("profiles")?.get(p)?.get("model_provider"))
        .or_else(|| doc.get("model_provider"))
        .and_then(Item::as_str);
    if selector != Some("custom") {
        return Err(AppError::new(
            code::CUSTOM,
            "当前生效的 provider 不是 custom，请在配置编辑器中处理",
        ));
    }
    doc.get("model_providers")
        .and_then(|p| p.get("custom"))
        .filter(|p| p.as_table_like().is_some())
        .ok_or_else(|| {
            AppError::new(
                code::CUSTOM,
                "缺少现有 model_providers.custom，请在配置编辑器中处理",
            )
        })
}
fn pair(doc: &Document<&str>) -> Result<Pair> {
    let p = custom(doc)?;
    let read = |key| -> Result<Option<String>> {
        p.get(key)
            .map(|v| {
                v.as_str().map(str::to_owned).ok_or_else(|| {
                    AppError::new(code::CUSTOM, "custom 的地址和 Token 必须是字符串")
                })
            })
            .transpose()
    };
    Ok(Pair {
        base_url: read(KEYS[0])?,
        token: read(KEYS[1])?,
    })
}
pub fn read_connection(
    client: ClientId,
    home: &Path,
    connection: &Connection,
) -> Result<(String, Pair)> {
    match (client, connection) {
        (ClientId::Codex, Connection::ApiKey { provider }) => codex_api::read(home, provider),
        _ => read_for(client, home),
    }
}
fn stored_connection(store: Option<&str>) -> Result<Connection> {
    let Some(store) = store else {
        return Ok(Connection::Bearer);
    };
    let value: serde_json::Value =
        serde_json::from_str(store).map_err(|_| AppError::new(code::STORE, "网关连接方式无效"))?;
    value
        .get("connection")
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .map(|v| v.unwrap_or_default())
        .map_err(|_| AppError::new(code::STORE, "网关连接方式无效"))
}
fn write_record(
    data: &Path,
    record: &Journal,
    target: &Pair,
    expected: Option<&str>,
    allowed: Option<&[&Pair]>,
) -> Result<()> {
    if let Some(snapshot) = &record.grok {
        if record.live && target != &record.applied {
            return snapshot.restore(&record.path);
        }
        return snapshot.apply(&record.path, expected);
    }
    if let (ClientId::Codex, Connection::ApiKey { provider }) = (record.client, &record.connection)
    {
        let home = record
            .path
            .parent()
            .ok_or_else(|| AppError::new(code::RECOVERY, "API 恢复路径无效"))?;
        codex_api::write(data, home, provider, target, expected, allowed)
    } else {
        write_pair(record.client, &record.path, target, expected, allowed)
    }
}
/// Account synchronization must never adopt the gateway's local API credential.
/// An unresolved transaction also owns auth.json until recovery succeeds.
pub(crate) fn manages_auth(data: &Path, home: &Path) -> bool {
    match load(data) {
        Ok(Some(record)) => {
            record.client == ClientId::Codex
                && matches!(record.connection, Connection::ApiKey { .. })
                && record.path == home.join("config.toml")
        }
        Ok(None) => data.join("gateway-api-write.json").exists(),
        Err(_) => true,
    }
}
pub fn read_for(client: ClientId, home: &Path) -> Result<(String, Pair)> {
    let raw = storage::read_optional(&client.config(home))?;
    if raw.is_none() && client == ClientId::Codex {
        return Err(AppError::new(
            code::CUSTOM,
            "缺少 config.toml，请先在配置编辑器中设置 custom",
        ));
    }
    let text = std::str::from_utf8(raw.as_deref().unwrap_or(if client == ClientId::Claude {
        b"{}"
    } else {
        b""
    }))
    .map_err(|_| AppError::new(code::CONFIG, "配置不是 UTF-8"))?;
    Ok((storage::revision(raw.as_deref()), pair_for(client, text)?))
}
pub(super) fn pair_for(client: ClientId, text: &str) -> Result<Pair> {
    match client {
        ClientId::Codex => pair(&parse(text)?),
        ClientId::Claude => super::claude_config::pair(text),
        ClientId::Grok => super::grok_config::pair(text),
    }
}
#[cfg(test)]
pub fn read(home: &Path) -> Result<(String, Pair)> {
    read_for(ClientId::Codex, home)
}
pub fn patch(text: &str, target: &Pair) -> Result<String> {
    let doc = parse(text)?;
    let p = custom(&doc)?;
    pair(&doc)?;
    let mut edits = vec![];
    let mut missing = vec![];
    for (key, next) in KEYS.into_iter().zip([&target.base_url, &target.token]) {
        let next = next
            .as_ref()
            .ok_or_else(|| AppError::new(code::CUSTOM, "供应商地址和 Token 不完整"))?;
        let encoded = toml_edit::Value::from(next.clone()).to_string();
        if let Some(item) = p.get(key) {
            if item.as_str() != Some(next) {
                let span = item
                    .as_value()
                    .and_then(|v| v.span())
                    .ok_or_else(|| AppError::new(code::TOML, "无法定位配置字段"))?;
                edits.push((span, encoded));
            }
        } else {
            missing.push((key, encoded));
        }
    }
    if !missing.is_empty() {
        let parent = doc.get("model_providers").unwrap();
        let (container, prefix) = if p.as_table().is_some_and(|t| t.is_implicit())
            || p.as_inline_table().is_some_and(|t| t.is_dotted())
        {
            if parent.as_table().is_some_and(|t| t.is_implicit()) {
                (doc.as_item(), "model_providers.custom.")
            } else {
                (parent, "custom.")
            }
        } else {
            (p, "")
        };
        let items: Vec<_> = missing
            .iter()
            .map(|(k, v)| format!("{prefix}{k} = {v}"))
            .collect();
        if let Some(t) = container.as_inline_table() {
            let span = t
                .span()
                .ok_or_else(|| AppError::new(code::TOML, "无法定位内联表"))?;
            let at = span.end - 1;
            let join = if t.is_empty() { "" } else { ", " };
            edits.push((at..at, format!("{join}{}", items.join(", "))));
        } else {
            let newline = if text.contains("\r\n") { "\r\n" } else { "\n" };
            let end = if prefix == "model_providers.custom." {
                0
            } else {
                let span = container
                    .as_table()
                    .and_then(|t| t.span())
                    .ok_or_else(|| AppError::new(code::TOML, "无法定位 custom 表"))?;
                text[span.end..]
                    .find('\n')
                    .map(|i| span.end + i + 1)
                    .unwrap_or(text.len())
            };
            let lead = if end > 0 && !text[..end].ends_with('\n') {
                newline
            } else {
                ""
            };
            edits.push((end..end, format!("{lead}{}{newline}", items.join(newline))));
        }
    }
    edits.sort_by_key(|(s, _)| std::cmp::Reverse(s.start));
    let mut output = text.to_owned();
    for (span, value) in edits {
        output.replace_range(span, &value);
    }
    if pair(&parse(&output)?)? != *target {
        return Err(AppError::new(code::VERIFY, "两字段配置验证失败"));
    }
    Ok(output)
}
fn write_pair(
    client: ClientId,
    path: &Path,
    target: &Pair,
    expected: Option<&str>,
    allowed: Option<&[&Pair]>,
) -> Result<()> {
    let raw = storage::read_optional(path)?;
    if raw.is_none() && client == ClientId::Codex {
        return Err(AppError::new(code::CONFLICT, "配置已被移除"));
    }
    let revision = storage::revision(raw.as_deref());
    if expected.is_some_and(|e| e != revision) {
        return Err(AppError::new(code::CONFLICT, "配置已变化，请刷新后重试"));
    }
    let text = std::str::from_utf8(raw.as_deref().unwrap_or(if client == ClientId::Claude {
        b"{}"
    } else {
        b""
    }))
    .map_err(|_| AppError::new(code::TOML, "配置不是 UTF-8"))?;
    let current = pair_for(client, text)?;
    if allowed.is_some_and(|pairs| !pairs.contains(&&current)) {
        return Err(AppError::new(
            code::CONFLICT,
            "受管地址或 Token 已被外部修改，已保留现场与事务记录",
        ));
    }
    let output = match client {
        ClientId::Codex => patch(text, target)?,
        ClientId::Claude => super::claude_config::patch(text, target)?,
        ClientId::Grok => super::grok_config::Snapshot::new(text, target)?
            .applied()
            .to_owned(),
    };
    if raw.is_none() || output != text {
        storage::atomic_write(path, output.as_bytes(), Some(&revision))?;
    }
    Ok(())
}
fn save(data: &Path, record: &Journal) -> Result<()> {
    let path = data.join(FILE);
    let prior = storage::read_optional(&path)?;
    storage::atomic_write(
        &path,
        &serde_json::to_vec(record)
            .map_err(|_| AppError::new(code::RECOVERY, "无法生成事务记录"))?,
        Some(&storage::revision(prior.as_deref())),
    )
}
fn load(data: &Path) -> Result<Option<Journal>> {
    let Some(raw) = storage::read_optional(&data.join(FILE))? else {
        return Ok(None);
    };
    let value: serde_json::Value = serde_json::from_slice(&raw)
        .map_err(|_| AppError::new(code::RECOVERY, "恢复记录损坏，请保留并检查"))?;
    if value.get("version").and_then(|v| v.as_u64()) != Some(2) {
        return Err(AppError::new(
            code::RECOVERY,
            "旧版接管记录尚未解除；已保留配置，请在配置编辑器中处理",
        ));
    }
    serde_json::from_value(value)
        .map(Some)
        .map_err(|_| AppError::new(code::RECOVERY, "恢复记录格式无效"))
}
fn complete_store(data: &Path, record: &mut Journal) -> Result<()> {
    if let Some(after) = &record.store_after {
        let path = data.join("gateway.json");
        let current = storage::read_optional(&path)?;
        if current.as_deref() != Some(after.as_bytes()) {
            storage::atomic_write(
                &path,
                after.as_bytes(),
                Some(&storage::revision(
                    record.store_before.as_deref().map(str::as_bytes),
                )),
            )?;
        }
        record.store_before = None;
        record.store_after = None;
        save(data, record)?;
    }
    Ok(())
}
pub fn import_for(client: ClientId, home: &Path) -> Result<(String, String)> {
    let (_, p) = read_for(client, home)?;
    let base = p
        .base_url
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::new(code::IMPORT, "配置缺少供应商地址"))?;
    let token = p
        .token
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::new(code::IMPORT, "配置缺少供应商 Token"))?;
    if token.starts_with("gs_") && base.contains("127.0.0.1") {
        return Err(AppError::new(code::MANAGED, "不能导入网关的本地凭据"));
    }
    Ok((base, token))
}
#[cfg(test)]
pub fn attach(
    data: &Path,
    home: &Path,
    port: u16,
    token: &str,
    exit: Pair,
    expected: &str,
) -> Result<()> {
    attach_store_for(
        ClientId::Codex,
        data,
        home,
        port,
        token,
        exit,
        expected,
        None,
    )
}
#[allow(clippy::too_many_arguments)]
pub fn attach_store_for(
    client: ClientId,
    data: &Path,
    home: &Path,
    port: u16,
    token: &str,
    exit: Pair,
    expected: &str,
    store: Option<(Option<String>, String)>,
) -> Result<()> {
    if data.join(FILE).exists() {
        return Err(AppError::new(code::RECOVERY, "请先处理现有配置事务"));
    }
    let connection = stored_connection(store.as_ref().map(|(_, after)| after.as_str()))?;
    let (revision, before) = read_connection(client, home, &connection)?;
    if revision != expected {
        return Err(AppError::new(code::CONFLICT, "配置已变化，请刷新后重试"));
    }
    let mut record = Journal {
        version: 2,
        grok: grok_snapshot(
            client,
            home,
            &Pair::new(&client.address(port), token),
            store.as_ref().map(|(_, s)| s.as_str()),
            Some(&exit),
        )?,
        connection,
        client,
        path: client.config(home),
        exit: if client == ClientId::Grok {
            before.clone()
        } else {
            exit
        },
        before,
        applied: Pair::new(&client.address(port), token),
        live: true,
        config_applied: false,
        store_before: store.as_ref().and_then(|(before, _)| before.clone()),
        store_after: store.map(|(_, after)| after),
    };
    save(data, &record)?;
    if let Err(e) = write_record(data, &record, &record.applied, Some(expected), None) {
        if !data.join("gateway-api-write.json").exists()
            && read_connection(client, home, &record.connection)
                .is_ok_and(|(_, p)| p != record.applied)
        {
            fs::remove_file(data.join(FILE)).map_err(storage::io_error)?;
        }
        return Err(e);
    }
    record.config_applied = true;
    save(data, &record)?;
    complete_store(data, &mut record)
}
fn grok_snapshot(
    client: ClientId,
    home: &Path,
    target: &Pair,
    store: Option<&str>,
    provider_pair: Option<&Pair>,
) -> Result<Option<super::grok_config::Snapshot>> {
    if client != ClientId::Grok {
        return Ok(None);
    }
    let store = store
        .map(serde_json::from_str::<super::model::Store>)
        .transpose()
        .map_err(|_| AppError::new(code::STORE, "Grok 网关配置无效"))?;
    let connection = store
        .as_ref()
        .and_then(|s| {
            s.providers
                .iter()
                .find(|p| {
                    provider_pair.is_some_and(|pair| {
                        pair.base_url.as_deref() == Some(&p.base_url)
                            && pair.token.as_deref() == Some(&p.token)
                    })
                })
                .or_else(|| {
                    s.selected
                        .as_ref()
                        .and_then(|id| s.providers.iter().find(|p| &p.id == id))
                })
                .and_then(|p| s.grok_connections.get(&p.id))
        })
        .cloned()
        .unwrap_or_default();
    let text = super::grok_config::read(home)?.1;
    Ok(Some(
        if store.as_ref().is_some_and(|s| s.grok_api_model.is_some()) {
            super::grok_config::Snapshot::api(&text, target, &connection, provider_pair.is_none())?
        } else if provider_pair.is_some() {
            super::grok_config::Snapshot::with_protocol(&text, target, Some(connection.protocol))?
        } else {
            super::grok_config::Snapshot::with_connection(&text, target, &connection)?
        },
    ))
}
pub fn grok_unchanged(data: &Path, text: &str) -> Result<bool> {
    load(data)?
        .and_then(|r| r.grok)
        .ok_or_else(|| AppError::new(code::RECOVERY, "缺少 Grok 配置事务"))?
        .unchanged(text)
}
pub fn restore_grok_native(
    data: &Path,
    home: &Path,
    snapshot: super::grok_config::Snapshot,
    before_store: String,
    after_store: String,
    expected: &str,
) -> Result<()> {
    if data.join(FILE).exists() {
        return Err(AppError::new(code::RECOVERY, "请先停止网关并处理配置事务"));
    }
    let (revision, current) = read_for(ClientId::Grok, home)?;
    if revision != expected {
        return Err(AppError::new(code::CONFLICT, "Grok 配置已变化"));
    }
    let record = Journal {
        client: ClientId::Grok,
        version: 2,
        connection: Connection::Bearer,
        path: ClientId::Grok.config(home),
        before: current.clone(),
        applied: current,
        exit: snapshot.original_pair()?,
        live: true,
        config_applied: true,
        store_before: Some(before_store),
        store_after: Some(after_store),
        grok: Some(snapshot),
    };
    // The same recovery journal completes both files if a write is interrupted.
    save(data, &record)?;
    detach(data)
}
pub fn exit_pair(data: &Path) -> Result<Option<Pair>> {
    Ok(load(data)?.map(|r| r.exit))
}
#[allow(clippy::too_many_arguments)]
pub fn commit_store_for(
    client: ClientId,
    data: &Path,
    home: &Path,
    before_store: Option<String>,
    after_store: String,
    target: Pair,
    running: bool,
    expected_config: Option<&str>,
) -> Result<()> {
    let mut record = if running {
        let record =
            load(data)?.ok_or_else(|| AppError::new(code::RECOVERY, "缺少网关事务记录"))?;
        if record.store_after.is_some() {
            return Err(AppError::new(
                code::RECOVERY,
                "请先停止网关并完成待处理事务",
            ));
        }
        let (_, current) = read_connection(client, home, &record.connection)?;
        if current != record.applied
            || (client == ClientId::Grok
                && !grok_unchanged(data, &super::grok_config::read(home)?.1)?)
        {
            return Err(AppError::new(
                code::CONFLICT,
                "受管地址或 Token 已被外部修改",
            ));
        }
        record
    } else {
        if data.join(FILE).exists() {
            return Err(AppError::new(code::RECOVERY, "请先处理待恢复的配置事务"));
        }
        let connection = stored_connection(Some(&after_store))?;
        let (revision, before) = read_connection(client, home, &connection)?;
        if expected_config.is_some_and(|e| e != revision) {
            return Err(AppError::new(code::CONFLICT, "配置已变化，请刷新后重试"));
        }
        Journal {
            version: 2,
            grok: grok_snapshot(client, home, &target, Some(&after_store), None)?,
            connection,
            client,
            path: client.config(home),
            before,
            applied: target.clone(),
            exit: target.clone(),
            live: false,
            config_applied: false,
            store_before: None,
            store_after: None,
        }
    };
    if client != ClientId::Grok || !running {
        record.exit = target;
    }
    record.store_before = before_store;
    record.store_after = Some(after_store);
    save(data, &record)?;
    if !running {
        write_record(
            data,
            &record,
            &record.applied,
            expected_config,
            Some(&[&record.before, &record.applied]),
        )?;
    }
    record.config_applied = true;
    save(data, &record)?;
    complete_store(data, &mut record)?;
    if !running {
        fs::remove_file(data.join(FILE)).map_err(storage::io_error)?;
    }
    Ok(())
}
#[cfg(test)]
pub fn update_exit(data: &Path, target: Pair) -> Result<()> {
    let mut record =
        load(data)?.ok_or_else(|| AppError::new(code::RECOVERY, "缺少网关事务记录"))?;
    if record.store_after.is_some() {
        return Err(AppError::new(code::RECOVERY, "供应商事务尚未完成"));
    }
    if record.exit != target {
        record.exit = target;
        save(data, &record)?;
    }
    Ok(())
}
/// Startup, normal stop and exit all finish the same transaction, preserving unrelated edits.
pub fn detach(data: &Path) -> Result<()> {
    let Some(mut record) = load(data)? else {
        return Ok(());
    };
    let allowed = if record.config_applied {
        vec![&record.applied, &record.exit]
    } else {
        vec![&record.before, &record.applied, &record.exit]
    };
    if let Some(snapshot) = &record.grok {
        if record.live {
            snapshot.restore(&record.path)?;
        } else {
            snapshot.apply(&record.path, None)?;
        }
    } else {
        write_record(data, &record, &record.exit, None, Some(&allowed))?;
    }
    complete_store(data, &mut record)?;
    fs::remove_file(data.join(FILE)).map_err(storage::io_error)
}
pub fn recover(data: &Path) -> Result<()> {
    codex_api::recover_write(data)?;
    let Some(raw) = storage::read_optional(&data.join(FILE))? else {
        return Ok(());
    };
    let v: serde_json::Value =
        serde_json::from_slice(&raw).map_err(|_| AppError::new(code::RECOVERY, "恢复记录损坏"))?;
    if v.get("version").and_then(|v| v.as_u64()) == Some(2) {
        return detach(data);
    }
    // v0.2 could leave a journal after the user already restored custom. Do not replay it.
    let path = v
        .get("path")
        .and_then(|v| v.as_str())
        .map(Path::new)
        .ok_or_else(|| AppError::new(code::RECOVERY, "旧恢复记录无效"))?;
    let text = fs::read_to_string(path).map_err(storage::io_error)?;
    let doc = parse(&text)?;
    let current = pair(&doc)?;
    let applied = v
        .get("applied")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AppError::new(code::RECOVERY, "旧恢复记录无效"))?;
    let old = parse(applied)?;
    let managed = old
        .get("model_providers")
        .and_then(|p| p.get("gpt_switch_gateway"))
        .ok_or_else(|| AppError::new(code::RECOVERY, "无法确认旧恢复记录的来源，已保留现场"))?;
    let still_managed = doc
        .get("model_providers")
        .and_then(|p| p.get("gpt_switch_gateway"))
        .is_some()
        || managed.get("base_url").and_then(Item::as_str) == current.base_url.as_deref()
        || managed
            .get("experimental_bearer_token")
            .and_then(Item::as_str)
            == current.token.as_deref();
    if still_managed {
        return Err(AppError::new(
            code::RECOVERY,
            "旧版接管尚未解除，已保留配置，请通过配置编辑器处理",
        ));
    }
    fs::remove_file(data.join(FILE)).map_err(storage::io_error)
}

#[cfg(test)]
#[allow(dead_code)]
pub fn import(home: &Path) -> Result<(String, String)> {
    import_for(ClientId::Codex, home)
}

#[cfg(test)]
mod tests {
    use super::*;
    const ORIGINAL: &str = "# settings\r\nmodel_provider='custom'\r\nmodel='unchanged'\r\n[model_providers.custom] # comment\r\nbase_url = 'https://original.test/v1' # address\r\nexperimental_bearer_token = 'old' # key\r\nsupports_websockets = false\r\nwire_api = 'responses'\r\n[other]\r\na=42\r\n";
    pub fn masked(text: &str) -> String {
        let doc = parse(text).unwrap();
        let p = custom(&doc).unwrap();
        let mut spans: Vec<_> = KEYS
            .iter()
            .filter_map(|k| p.get(k)?.as_value()?.span())
            .collect();
        spans.sort_by_key(|s| std::cmp::Reverse(s.start));
        let mut s = text.to_owned();
        for span in spans {
            s.replace_range(span, "<value>");
        }
        s
    }
    #[test]
    fn replacements_are_byte_exact_outside_two_values() {
        for text in [ORIGINAL,"profile='work'\nprofiles={work={model_provider='custom',model='keep'}}\nmodel_providers = { custom = { base_url='https://x.test', experimental_bearer_token='a', other=true }, spare={x=1} }\n"] {
            let out=patch(text,&Pair::new("https://next.test/prefix/v1","a\\b\"c\n" )).unwrap();
            assert_eq!(masked(text),masked(&out));
            assert_eq!(pair(&parse(&out).unwrap()).unwrap().token.as_deref(),Some("a\\b\"c\n"));
        }
    }
    #[test]
    fn missing_fields_insert_into_existing_normal_inline_and_dotted_tables() {
        for text in ["model_provider='custom'\r\n[model_providers.custom] # keep\r\nname='stay'\r\n[next]\r\nx=1", "model_provider='custom'\nmodel_providers={custom={name='stay'},spare={x=1}}", "model_provider='custom'\n[model_providers]\ncustom.name='stay'\n", "model_provider='custom'\nmodel_providers.custom.name='stay'\n", "model_provider='custom'\nmodel_providers={custom.name='stay'}\n"] {
            let out=patch(text,&Pair::new("https://x.test","k")).unwrap();
            assert_eq!(pair(&parse(&out).unwrap()).unwrap().token.as_deref(),Some("k"));
            assert!(out.contains("name='stay'"));
        }
        for text in [
            "",
            "model_provider='other'\n[model_providers.custom]\nx=1",
            "model_provider='custom'\n",
            "model_provider='custom'\n[model_providers.custom]\nbase_url=42",
        ] {
            assert!(patch(text, &Pair::new("https://x.test", "k")).is_err());
        }
    }
    #[test]
    fn stop_writes_current_provider_preserving_unrelated_edits_and_auth() {
        let t = tempfile::tempdir().unwrap();
        let path = t.path().join("config.toml");
        fs::write(&path, ORIGINAL).unwrap();
        fs::write(t.path().join("auth.json"), "untouched").unwrap();
        attach(
            t.path(),
            t.path(),
            15722,
            "local",
            Pair::new("https://a.test", "a"),
            &read(t.path()).unwrap().0,
        )
        .unwrap();
        let live = fs::read_to_string(&path)
            .unwrap()
            .replace("model='unchanged'", "model='external'");
        fs::write(&path, &live).unwrap();
        update_exit(t.path(), Pair::new("https://b.test", "b")).unwrap();
        detach(t.path()).unwrap();
        let output = fs::read_to_string(&path).unwrap();
        assert_eq!(masked(&output), masked(&live));
        assert_eq!(
            read(t.path()).unwrap().1.base_url.as_deref(),
            Some("https://b.test")
        );
        assert_eq!(
            fs::read_to_string(t.path().join("auth.json")).unwrap(),
            "untouched"
        );
        assert!(!t.path().join(FILE).exists());
    }
    #[test]
    fn owned_field_conflict_and_revision_failure_preserve_scene() {
        let t = tempfile::tempdir().unwrap();
        let path = t.path().join("config.toml");
        fs::write(&path, ORIGINAL).unwrap();
        assert!(attach(
            t.path(),
            t.path(),
            15722,
            "local",
            Pair::new("https://a.test", "a"),
            "stale"
        )
        .is_err());
        assert!(!t.path().join(FILE).exists());
        attach(
            t.path(),
            t.path(),
            15722,
            "local",
            Pair::new("https://a.test", "a"),
            &read(t.path()).unwrap().0,
        )
        .unwrap();
        let changed = fs::read_to_string(&path)
            .unwrap()
            .replace("local", "externally-edited");
        fs::write(&path, &changed).unwrap();
        assert!(detach(t.path()).is_err());
        assert_eq!(fs::read_to_string(path).unwrap(), changed);
        assert!(t.path().join(FILE).exists());
    }
    #[test]
    fn crash_between_config_and_store_finishes_transaction_and_protects_permissions() {
        let t = tempfile::tempdir().unwrap();
        let path = t.path().join("config.toml");
        fs::write(&path, ORIGINAL).unwrap();
        let old = Pair::new("https://original.test/v1", "old");
        let new = Pair::new("https://next.test", "new");
        let mut record = Journal {
            client: ClientId::Codex,
            connection: Connection::Bearer,
            grok: None,
            version: 2,
            path: path.clone(),
            before: old,
            applied: new.clone(),
            exit: new.clone(),
            live: false,
            config_applied: false,
            store_before: None,
            store_after: Some("{\"selected\":\"new\"}".into()),
        };
        save(t.path(), &record).unwrap();
        write_pair(ClientId::Codex, &path, &new, None, None).unwrap();
        record.config_applied = true;
        save(t.path(), &record).unwrap();
        recover(t.path()).unwrap();
        assert_eq!(
            fs::read_to_string(t.path().join("gateway.json")).unwrap(),
            "{\"selected\":\"new\"}"
        );
        assert_eq!(
            masked(&fs::read_to_string(&path).unwrap()),
            masked(ORIGINAL)
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }
    #[test]
    fn legacy_already_restored_keeps_current_credentials_and_active_legacy_is_blocked() {
        let t = tempfile::tempdir().unwrap();
        let path = t.path().join("config.toml");
        fs::write(&path, ORIGINAL).unwrap();
        let applied="model_provider='gpt_switch_gateway'\n[model_providers.gpt_switch_gateway]\nbase_url='http://127.0.0.1:15722/v1'\nexperimental_bearer_token='gs_local'\n";
        let old =
            serde_json::json!({"path":path,"original":ORIGINAL,"applied":applied,"profile":null});
        fs::write(t.path().join(FILE), old.to_string()).unwrap();
        recover(t.path()).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), ORIGINAL);
        fs::write(t.path().join(FILE), old.to_string()).unwrap();
        fs::write(&path, applied).unwrap();
        assert!(recover(t.path()).is_err());
        assert_eq!(fs::read_to_string(path).unwrap(), applied);
    }
    #[test]
    fn journal_write_failure_never_changes_config() {
        let t = tempfile::tempdir().unwrap();
        let path = t.path().join("config.toml");
        fs::write(&path, ORIGINAL).unwrap();
        fs::create_dir(t.path().join(FILE)).unwrap();
        assert!(attach(
            t.path(),
            t.path(),
            15722,
            "local",
            Pair::new("https://a.test", "a"),
            &read(t.path()).unwrap().0
        )
        .is_err());
        assert_eq!(fs::read_to_string(path).unwrap(), ORIGINAL);
    }
}
