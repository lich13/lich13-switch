//! Public, optional model metadata. No credentials and no scheduled refreshes.
use crate::{
    storage::{self, Result},
    usage::model::{decimal, failure, now},
};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};

const SOURCE: &str = "https://models.dev/api.json";
const DOWNLOAD_LIMIT: usize = 16 * 1024 * 1024;
const CACHE_LIMIT: u64 = 4 * 1024 * 1024;

#[derive(Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Model {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canonical_model_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<bool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasoning_options: Vec<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fast_cost: Option<Value>,
}
#[derive(Clone, Serialize, Deserialize)]
struct Provider {
    name: String,
    models: BTreeMap<String, Model>,
}
#[derive(Clone, Serialize, Deserialize)]
struct Index {
    schema: u8,
    updated_at: i64,
    providers: BTreeMap<String, Provider>,
}
#[derive(Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct View {
    pub updated_at: Option<i64>,
    pub version: Option<String>,
    pub providers: usize,
    pub models: usize,
    pub error: Option<String>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Metadata {
    pub sources: Vec<String>,
    pub model: String,
    pub matched_by: String,
    pub name: Option<String>,
    pub reasoning: Option<bool>,
    pub context: Option<u64>,
    pub output: Option<u64>,
    pub reasoning_efforts: Option<BTreeMap<String, Value>>,
}
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchHit {
    pub source: String,
    pub provider: String,
    pub provider_name: String,
    pub model: String,
    pub name: Option<String>,
    pub context: Option<u64>,
    pub output: Option<u64>,
    pub cost: Option<Value>,
}
#[derive(Serialize)]
pub struct Search {
    pub hits: Vec<SearchHit>,
    pub error: Option<String>,
}
#[derive(Clone, Default)]
struct State {
    index: Option<Arc<Index>>,
    version: Option<String>,
    error: Option<String>,
    attempted: Option<Instant>,
}
#[derive(Clone)]
pub struct Catalog {
    path: PathBuf,
    state: Arc<RwLock<State>>,
    sync: Arc<tokio::sync::Mutex<()>>,
}
fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 256 && !id.chars().any(char::is_control)
}
fn validate(index: &Index) -> bool {
    index.schema == 1
        && !index.providers.is_empty()
        && index.providers.len() <= 1024
        && index.providers.iter().all(|(id, p)| {
            valid_id(id) && p.models.len() <= 30000 && p.models.keys().all(|id| valid_id(id))
        })
}
fn crop(value: Value) -> Result<Index> {
    let root = value
        .as_object()
        .ok_or_else(|| failure("models.dev 格式无效"))?;
    let mut providers = BTreeMap::new();
    for (id, p) in root {
        let entries = p["models"]
            .as_object()
            .ok_or_else(|| failure("models.dev 模型列表无效"))?;
        let mut models = BTreeMap::new();
        for (key, v) in entries {
            let model_id = v["id"].as_str().unwrap_or(key);
            if !valid_id(model_id) || !v.is_object() {
                return Err(failure("models.dev 模型 ID 无效"));
            }
            let mut model: Model =
                serde_json::from_value(v.clone()).map_err(|_| failure("models.dev 元数据无效"))?;
            // A mode named fast can instead mean OpenAI priority. Only the
            // explicit Anthropic speed contract is a fast tariff here.
            let fast = &v["experimental"]["modes"]["fast"];
            model.fast_cost = (fast["provider"]["body"]["speed"] == "fast")
                .then(|| fast["cost"].clone())
                .filter(Value::is_object);
            if model
                .name
                .as_ref()
                .is_some_and(|s| s.len() > 512 || s.chars().any(char::is_control))
                || model
                    .canonical_model_id
                    .as_ref()
                    .is_some_and(|s| !valid_id(s))
                || model.reasoning_options.len() > 16
            {
                return Err(failure("models.dev 元数据超过限制"));
            }
            // Keep only data needed by the client metadata/price contract.
            if let Some(limit) = model.limit.take() {
                if !limit.is_object() {
                    return Err(failure("models.dev 限额格式无效"));
                }
                model.limit = Some(
                    json!({"context":limit["context"].as_u64(),"output":limit["output"].as_u64()}),
                );
            }
            models.insert(model_id.to_owned(), model);
        }
        if !models.is_empty() {
            providers.insert(
                id.clone(),
                Provider {
                    name: p["name"].as_str().unwrap_or(id).to_owned(),
                    models,
                },
            );
        }
    }
    let index = Index {
        schema: 1,
        updated_at: now(),
        providers,
    };
    if !validate(&index) {
        return Err(failure("models.dev 目录为空或超过限制"));
    }
    if serde_json::to_vec(&index)
        .map_err(|_| failure("models.dev 缓存无效"))?
        .len() as u64
        > CACHE_LIMIT
    {
        return Err(failure("models.dev 裁剪目录超过 4 MiB"));
    }
    Ok(index)
}
impl Catalog {
    pub fn new(dir: &Path) -> Self {
        let path = dir.join("models-dev-index.json");
        let loaded = storage::read_bounded(&path, CACHE_LIMIT).and_then(|bytes| {
            let index = bytes
                .as_ref()
                .map(|bytes| {
                    serde_json::from_slice::<Index>(bytes)
                        .map_err(|_| failure("models.dev 缓存不可用，可手动刷新"))
                })
                .transpose()?;
            if index.as_ref().is_some_and(|index| !validate(index)) {
                return Err(failure("models.dev 缓存不可用，可手动刷新"));
            }
            Ok((index, bytes.map(|bytes| storage::digest(&bytes))))
        });
        let state = match loaded {
            Ok((index, version)) => State {
                index: index.map(Arc::new),
                version,
                ..State::default()
            },
            Err(_) => State {
                error: Some("models.dev 缓存不可用，可手动刷新".into()),
                ..State::default()
            },
        };
        Self {
            path,
            state: Arc::new(RwLock::new(state)),
            sync: Arc::new(tokio::sync::Mutex::new(())),
        }
    }
    pub fn frozen(&self) -> Self {
        Self {
            path: self.path.clone(),
            state: Arc::new(RwLock::new(self.state.read().unwrap().clone())),
            sync: Arc::new(tokio::sync::Mutex::new(())),
        }
    }
    pub fn view(&self) -> View {
        let state = self.state.read().unwrap();
        View {
            updated_at: state.index.as_ref().map(|i| i.updated_at),
            version: state.version.clone(),
            providers: state.index.as_ref().map_or(0, |i| i.providers.len()),
            models: state
                .index
                .as_ref()
                .map_or(0, |i| i.providers.values().map(|p| p.models.len()).sum()),
            error: state.error.clone(),
        }
    }
    pub async fn refresh(&self, force: bool) -> Result<View> {
        self.refresh_from(force, SOURCE).await
    }
    async fn refresh_from(&self, force: bool, source: &str) -> Result<View> {
        let began = Instant::now();
        let _guard = self.sync.lock().await;
        let skip = {
            let state = self.state.read().unwrap();
            state.attempted.is_some_and(|t| t >= began)
                || (!force && (self.path.exists() || state.attempted.is_some()))
        };
        if skip {
            return Ok(self.view());
        }
        let result = download(source).await.and_then(|index| {
            let bytes = serde_json::to_vec(&index).map_err(|_| failure("models.dev 缓存无效"))?;
            storage::replace_cache_bounded(&self.path, &bytes, CACHE_LIMIT)?;
            Ok((index, storage::digest(&bytes)))
        });
        let mut state = self.state.write().unwrap();
        state.attempted = Some(Instant::now());
        match result {
            Ok((index, version)) => {
                state.index = Some(Arc::new(index));
                state.version = Some(version);
                state.error = None;
            }
            Err(e) => state.error = Some(e.message),
        }
        drop(state);
        Ok(self.view())
    }
    #[cfg(test)]
    pub fn metadata(&self, id: &str) -> Option<Metadata> {
        self.metadata_from(id, None)
    }
    pub fn metadata_from(&self, id: &str, source: Option<&str>) -> Option<Metadata> {
        let state = self.state.read().unwrap();
        let (matches, basis) = resolve_source(state.index.as_deref()?, id, false, source)?;
        let (_, first_id, first) = *matches.first()?;
        if matches.iter().any(|(_, _, m)| {
            m.name != first.name
                || m.limit != first.limit
                || m.reasoning != first.reasoning
                || m.reasoning_options != first.reasoning_options
        }) {
            return None;
        }
        let mut efforts = BTreeMap::new();
        for option in &first.reasoning_options {
            if option["type"] != "effort" {
                continue;
            }
            for effort in option["values"].as_array().into_iter().flatten() {
                if let Some(name) = effort.as_str().filter(|s| {
                    matches!(
                        *s,
                        "none"
                            | "minimal"
                            | "low"
                            | "medium"
                            | "high"
                            | "xhigh"
                            | "max"
                            | "default"
                    )
                }) {
                    efforts.insert(name.into(), effort.clone());
                } else if effort.is_null() {
                    efforts.insert("default".into(), Value::Null);
                }
            }
        }
        Some(Metadata {
            sources: matches.iter().map(|(p, _, _)| (*p).to_owned()).collect(),
            model: first_id.into(),
            matched_by: basis.into(),
            name: first.name.clone(),
            reasoning: first.reasoning,
            context: first.limit.as_ref().and_then(|l| l["context"].as_u64()),
            output: first.limit.as_ref().and_then(|l| l["output"].as_u64()),
            reasoning_efforts: (!efforts.is_empty()).then_some(efforts),
        })
    }
    pub fn price(&self, id: &str) -> Option<(Value, String)> {
        self.price_from(id, None)
    }
    pub fn price_from(&self, id: &str, source: Option<&str>) -> Option<(Value, String)> {
        let state = self.state.read().unwrap();
        let (matches, basis) = resolve_source(state.index.as_deref()?, id, true, source)?;
        let (_, source_model, first) = *matches.first()?;
        let cost = first.cost.as_ref()?;
        if matches
            .iter()
            .any(|(_, _, m)| m.cost.as_ref() != Some(cost) || m.fast_cost != first.fast_cost)
        {
            return None;
        }
        Some((
            json!({"models_dev_cost":cost,"models_dev_fast_cost":first.fast_cost,"models_dev_match":{
                "providers":matches.iter().map(|(p,_,_)|*p).collect::<Vec<_>>(),"model":source_model,"matchedBy":basis,
                "unit":"USD/token","sourceUnit":"USD/million_tokens"
            }}),
            state.version.clone()?,
        ))
    }
    pub fn has_source(&self, source: &str) -> bool {
        self.state
            .read()
            .unwrap()
            .index
            .as_ref()
            .is_some_and(|index| qualified(index, source).is_some())
    }
    pub fn search(&self, query: &str, limit: usize) -> Search {
        let state = self.state.read().unwrap();
        let Some(index) = &state.index else {
            return Search {
                hits: vec![],
                error: Some("模型索引不可用，请刷新 models.dev".into()),
            };
        };
        let query = query.trim().to_lowercase();
        let hits = index
            .providers
            .iter()
            .flat_map(|(provider, p)| {
                let query = &query;
                p.models
                    .iter()
                    .filter(move |(key, model)| {
                        [
                            provider.as_str(),
                            p.name.as_str(),
                            key.as_str(),
                            model.name.as_deref().unwrap_or(""),
                        ]
                        .iter()
                        .any(|s| s.to_lowercase().contains(query.as_str()))
                    })
                    .map(move |(key, model)| SearchHit {
                        source: format!("{provider}/{key}"),
                        provider: provider.clone(),
                        provider_name: p.name.clone(),
                        model: key.clone(),
                        name: model.name.clone(),
                        context: model.limit.as_ref().and_then(|v| v["context"].as_u64()),
                        output: model.limit.as_ref().and_then(|v| v["output"].as_u64()),
                        cost: model.cost.clone(),
                    })
            })
            .take(limit.min(50))
            .collect();
        Search {
            hits,
            error: state.error.clone(),
        }
    }
}
async fn download(source: &str) -> Result<Index> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .user_agent(concat!("lich13-switch/", env!("CARGO_PKG_VERSION")))
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|_| failure("models.dev 连接初始化失败"))?;
    let mut response = client
        .get(source)
        .header("accept-encoding", "identity")
        .send()
        .await
        .map_err(|_| failure("models.dev 下载失败"))?;
    if !response.status().is_success() {
        return Err(failure("models.dev 响应未成功，已保留原目录"));
    }
    let encoding = response
        .headers()
        .get("content-encoding")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("identity")
        .to_owned();
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| failure("models.dev 响应中断"))?
    {
        if bytes.len() + chunk.len() > DOWNLOAD_LIMIT {
            return Err(failure("models.dev 下载超过 16 MiB"));
        }
        bytes.extend_from_slice(&chunk);
    }
    use std::io::Read;
    let reader = std::io::Cursor::new(&bytes);
    let decoded: Box<dyn Read> = match encoding.as_str() {
        "identity" | "" => Box::new(reader),
        "gzip" => Box::new(flate2::read::GzDecoder::new(reader)),
        "br" => Box::new(brotli::Decompressor::new(reader, 4096)),
        "deflate" => Box::new(flate2::read::ZlibDecoder::new(reader)),
        "zstd" => Box::new(
            zstd::stream::read::Decoder::new(reader)
                .map_err(|_| failure("models.dev 压缩响应无效"))?,
        ),
        _ => return Err(failure("models.dev 压缩格式不支持")),
    };
    let mut plain = Vec::new();
    decoded
        .take((DOWNLOAD_LIMIT + 1) as u64)
        .read_to_end(&mut plain)
        .map_err(|_| failure("models.dev 压缩响应无效"))?;
    if plain.len() > DOWNLOAD_LIMIT {
        return Err(failure("models.dev 解压后超过 16 MiB"));
    }
    crop(serde_json::from_slice(&plain).map_err(|_| failure("models.dev 没有返回有效 JSON"))?)
}

/// Generate only reversible naming wrappers; callers still require a known,
/// unique catalog hit. A price-changing suffix is never stripped for pricing.
pub fn names(id: &str, pricing: bool) -> Vec<String> {
    let original = id.trim().to_ascii_lowercase();
    if pricing && original.contains(':') && !original.ends_with(":0") {
        return vec![original];
    }
    let mut names = vec![original];
    let mut n = 0;
    while n < names.len() && n < 32 {
        let name = names[n].clone();
        n += 1;
        let mut add = |s: &str| {
            let s = s.trim();
            if !s.is_empty() && !names.iter().any(|v| v == s) {
                names.push(s.to_owned());
            }
        };
        if let Some((_, bare)) = name.rsplit_once('/') {
            add(bare);
        }
        for suffix in [
            "[1m]",
            "[200k]",
            "(none)",
            "(minimal)",
            "(low)",
            "(medium)",
            "(high)",
            "(xhigh)",
            "(max)",
        ] {
            if let Some(rest) = name.strip_suffix(suffix) {
                add(rest);
            }
        }
        if !pricing {
            if let Some(rest) = name.strip_suffix(":free") {
                add(rest);
            }
        }
        for prefix in ["us.", "eu.", "apac.", "global.", "anthropic."] {
            if let Some(rest) = name.strip_prefix(prefix) {
                add(rest);
            }
        }
        if let Some(rest) = name
            .strip_suffix("-v1:0")
            .or_else(|| name.strip_suffix("-v2:0"))
        {
            add(rest);
        }
        if let Some((prefix, rest)) = name.split_once('-') {
            if (2..=24).contains(&prefix.len()) && prefix.bytes().all(|b| b.is_ascii_lowercase()) {
                add(rest);
            }
        }
        let dotted = name.replace('.', "-");
        if dotted != name {
            add(&dotted);
        }
    }
    names.truncate(64);
    names
}
type Matched<'a> = (&'a str, &'a str, &'a Model);
fn qualified<'a>(index: &'a Index, source: &str) -> Option<Matched<'a>> {
    let (provider, key) = source.split_once('/')?;
    let (provider, p) = index.providers.get_key_value(provider)?;
    let (key, model) = p.models.get_key_value(key)?;
    Some((provider, key, model))
}
fn resolve_source<'a>(
    index: &'a Index,
    id: &str,
    pricing: bool,
    source: Option<&str>,
) -> Option<(Vec<Matched<'a>>, &'static str)> {
    if let Some(source) = source {
        return Some((vec![qualified(index, source)?], "pinned"));
    }
    resolve(index, id, pricing)
}
fn resolve<'a>(
    index: &'a Index,
    id: &str,
    pricing: bool,
) -> Option<(Vec<Matched<'a>>, &'static str)> {
    let (matches, basis) = resolve_candidates(index, id, pricing)?;
    if matches.len() > 1 {
        let canonical: BTreeSet<_> = matches
            .iter()
            .filter_map(|(_, _, m)| m.canonical_model_id.as_deref())
            .collect();
        if canonical.len() == 1 {
            if let Some(first) = canonical.first().and_then(|id| qualified(index, id)) {
                return Some((vec![first], "canonical"));
            }
        }
        // Canonical declarations are preferred. Older catalog rows omit them;
        // recognize only established model families with an actual maker row.
        let first_party: Vec<_> = matches
            .iter()
            .copied()
            .filter(|(provider, key, model)| {
                model
                    .canonical_model_id
                    .as_deref()
                    .and_then(|id| id.split_once('/'))
                    .is_some_and(|(maker, _)| maker == *provider)
                    || match *provider {
                        "anthropic" => key.starts_with("claude-"),
                        "openai" => ["gpt-", "chatgpt-", "o1", "o3", "o4"]
                            .iter()
                            .any(|prefix| key.starts_with(prefix)),
                        "google" => key.starts_with("gemini-"),
                        "xai" => key.starts_with("grok-"),
                        "deepseek" => key.starts_with("deepseek-"),
                        "zhipuai" => key.starts_with("glm-"),
                        "moonshotai" => key.starts_with("kimi-"),
                        _ => false,
                    }
            })
            .collect();
        if first_party.len() == 1 {
            return Some((first_party, "first-party"));
        }
        return None;
    }
    Some((matches, basis))
}
fn resolve_candidates<'a>(
    index: &'a Index,
    id: &str,
    pricing: bool,
) -> Option<(Vec<Matched<'a>>, &'static str)> {
    let exact = |needle: &str, canonical: bool| {
        index
            .providers
            .iter()
            .flat_map(|(provider, p)| {
                p.models.iter().filter_map(move |(key, m)| {
                    let found = if canonical {
                        m.canonical_model_id.as_deref() == Some(needle)
                    } else {
                        key == needle || format!("{provider}/{key}") == needle
                    };
                    found.then_some((provider.as_str(), key.as_str(), m))
                })
            })
            .collect::<Vec<_>>()
    };
    // A qualified source wins over homonymous offerings elsewhere.
    if let Some((provider, key)) = id.split_once('/') {
        if let Some(p) = index.providers.get(provider) {
            if let Some((key, m)) = p
                .models
                .get_key_value(key)
                .or_else(|| p.models.get_key_value(id))
            {
                return Some((
                    vec![(index.providers.get_key_value(provider)?.0, key, m)],
                    "exact",
                ));
            }
        }
    }
    let matches = exact(id, false);
    if !matches.is_empty() {
        return Some((matches, "exact"));
    }
    if !pricing || !id.contains(":free") {
        let matches = exact(id, true);
        if !matches.is_empty() {
            return Some((matches, "canonical"));
        }
    }
    let variants = names(id, pricing);
    let mut found = Vec::new();
    for (provider, p) in &index.providers {
        for (key, m) in &p.models {
            if variants.iter().any(|name| {
                name == &key.to_ascii_lowercase()
                    || (!pricing
                        && m.canonical_model_id
                            .as_ref()
                            .is_some_and(|id| name == &id.to_ascii_lowercase()))
            }) {
                found.push((provider.as_str(), key.as_str(), m));
            }
        }
    }
    (!found.is_empty()).then_some((found, "normalized"))
}

/// Select one complete price band. Missing fields never borrow another band's
/// or another source's rates. All returned unit rates are exact USD/token.
pub fn rates(data: &Value, context: u64, fast: bool) -> Option<(Value, Option<u64>)> {
    let cost = data
        .get(if fast {
            "models_dev_fast_cost"
        } else {
            "models_dev_cost"
        })?
        .as_object()?;
    if cost.keys().any(|key| {
        !matches!(
            key.as_str(),
            "input"
                | "output"
                | "reasoning"
                | "cache_read"
                | "cache_write"
                | "input_audio"
                | "output_audio"
                | "tiers"
                | "context_over_200k"
        )
    }) {
        return None;
    }
    let mut band = cost;
    let mut threshold = None;
    if let Some(tiers) = cost.get("tiers") {
        let mut sizes = BTreeSet::new();
        for tier in tiers.as_array()? {
            let size = tier["tier"]["size"].as_u64()?;
            if tier["tier"]["type"]
                .as_str()
                .is_some_and(|v| v != "context")
                || !sizes.insert(size)
            {
                return None;
            }
            if context > size && threshold.is_none_or(|old| size > old) {
                band = tier.as_object()?;
                threshold = Some(size);
            }
        }
    } else if context > 200_000 {
        if let Some(large) = cost.get("context_over_200k") {
            band = large.as_object()?;
            threshold = Some(200_000);
        }
    }
    if let Some(reasoning) = band.get("reasoning") {
        if decimal(&reasoning.to_string())? != decimal(&band.get("output")?.to_string())? {
            return None;
        }
    }
    let mut result = serde_json::Map::new();
    for (name, key) in [
        ("input", "input_cost_per_token"),
        ("output", "output_cost_per_token"),
        ("cache_read", "cache_read_input_token_cost"),
        ("cache_write", "cache_creation_input_token_cost"),
        ("input_audio", "input_cost_per_audio_token"),
        ("output_audio", "output_cost_per_audio_token"),
    ] {
        if let Some(value) = band.get(name) {
            let text = value
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| value.to_string());
            result.insert(
                format!("{key}{}", if fast { "_fast" } else { "" }),
                json!(decimal(&text)?
                    .checked_div(Decimal::from(1_000_000))?
                    .normalize()
                    .to_string()),
            );
        }
    }
    Some((Value::Object(result), threshold))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn catalog(dir: &Path, source: Value) -> Catalog {
        let index = crop(source).unwrap();
        storage::atomic_write_bounded(
            &dir.join("models-dev-index.json"),
            &serde_json::to_vec(&index).unwrap(),
            None,
            CACHE_LIMIT,
        )
        .unwrap();
        Catalog::new(dir)
    }
    #[test]
    fn frozen_prices_share_the_index_and_survive_catalog_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let source = |price| json!({"fixture":{"models":{"fixture-model":{"cost":{"input":price,"output":2}}}}});
        let cat = catalog(dir.path(), source(1));
        let frozen = cat.frozen();
        assert!(Arc::ptr_eq(
            cat.state.read().unwrap().index.as_ref().unwrap(),
            frozen.state.read().unwrap().index.as_ref().unwrap()
        ));
        cat.state.write().unwrap().index = Some(Arc::new(crop(source(9)).unwrap()));
        assert_eq!(
            cat.price("fixture-model").unwrap().0["models_dev_cost"]["input"],
            9
        );
        assert_eq!(
            frozen.price("fixture-model").unwrap().0["models_dev_cost"]["input"],
            1
        );
    }
    #[test]
    fn identities_canonical_and_semantic_variants_do_not_mix_capabilities_or_prices() {
        let dir = tempfile::tempdir().unwrap();
        let cat = catalog(
            dir.path(),
            json!({
                "anthropic":{"models":{"claude-opus-4-6":{"name":"Opus","reasoning":true,"reasoning_options":[{"type":"effort","values":["low","high"]}],"limit":{"context":200000,"output":64000},"cost":{"input":5,"output":25}},"variant":{"canonical_model_id":"canonical","cost":{"input":2,"output":4}}}},
                "relay":{"models":{"claude-opus-4-6":{"name":"Relay","reasoning":true,"limit":{"context":100000,"output":8000},"cost":{"input":8,"output":30}}}}
            }),
        );
        assert_eq!(
            cat.metadata("claude-opus-4-6").unwrap().sources,
            vec!["anthropic"]
        );
        assert_eq!(
            cat.price("claude-opus-4-6").unwrap().0["models_dev_cost"]["input"],
            5
        );
        let model = cat.metadata("anthropic/claude-opus-4-6").unwrap();
        assert_eq!(model.context, Some(200000));
        assert_eq!(model.reasoning_efforts.unwrap().len(), 2);
        assert!(cat.price("anthropic/claude-opus-4-6:free").is_none());
        assert_eq!(
            cat.price("canonical").unwrap().0["models_dev_match"]["matchedBy"],
            "canonical"
        );
        assert!(cat.price("claude-opus-4-7").is_none());
        let solo = catalog(
            dir.path(),
            json!({"anthropic":{"models":{"claude-opus-4-6":{"reasoning":true,"cost":{"input":5,"output":25}}}}}),
        );
        for id in [
            "us.anthropic.claude-opus-4-6-v1:0",
            "relay/claude-opus-4-6[1m]",
            "relay-claude-opus-4-6(high)",
        ] {
            assert!(solo.price(id).is_some(), "{id}");
        }
        assert!(solo
            .metadata("claude-opus-4-6")
            .unwrap()
            .reasoning_efforts
            .is_none());
    }
    #[test]
    fn source_priority_search_and_ambiguity_use_one_resolution() {
        let dir = tempfile::tempdir().unwrap();
        let cat = catalog(
            dir.path(),
            json!({
                "maker":{"name":"Maker","models":{"native":{"canonical_model_id":"maker/native","name":"Native","cost":{"input":1,"output":2},"limit":{"context":1000}}}},
                "relay":{"models":{"alias":{"canonical_model_id":"maker/native","name":"Alias","cost":{"input":4,"output":8}},"shared":{"name":"Shared","cost":{"input":1,"output":2}}}},
                "other":{"models":{"alias":{"canonical_model_id":"maker/native","cost":{"input":9,"output":10}},"shared":{"name":"Shared","cost":{"input":1,"output":2}}}}
            }),
        );
        assert_eq!(cat.metadata("alias").unwrap().model, "native");
        assert_eq!(cat.price("alias").unwrap().0["models_dev_cost"]["input"], 1);
        assert!(cat.metadata("shared").is_none());
        assert!(cat.price("shared").is_none());
        assert_eq!(
            cat.metadata_from("shared", Some("relay/alias"))
                .unwrap()
                .name
                .as_deref(),
            Some("Alias")
        );
        assert_eq!(
            cat.price_from("alias", Some("other/alias")).unwrap().0["models_dev_cost"]["input"],
            9
        );
        assert!(cat.price_from("alias", Some("missing/source")).is_none());
        assert_eq!(
            cat.price("relay/alias").unwrap().0["models_dev_cost"]["input"],
            4
        );
        assert_eq!(cat.search("alias", 50).hits.len(), 2);
        assert_eq!(cat.search("", 1).hits.len(), 1);
        assert!(cat.search("no-match", 50).error.is_none());
        assert!(Catalog::new(&dir.path().join("empty"))
            .search("alias", 50)
            .error
            .is_some());
    }
    #[test]
    #[ignore = "explicit offline public index comparison"]
    fn offline_resolution_comparison() {
        let path = std::env::var("LICH13_MODELS_INDEX").expect("supply a public index snapshot");
        let index: Index = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert!(validate(&index));
        let ids: BTreeSet<_> = index
            .providers
            .values()
            .flat_map(|p| p.models.keys())
            .filter(|id| !id.contains('/'))
            .collect();
        let priced = |found: Option<(Vec<Matched<'_>>, &str)>| {
            found.is_some_and(|(matches, _)| {
                matches.first().is_some_and(|(_, _, first)| {
                    first.cost.is_some()
                        && matches
                            .iter()
                            .all(|(_, _, m)| m.cost == first.cost && m.fast_cost == first.fast_cost)
                })
            })
        };
        let before = ids
            .iter()
            .filter(|id| priced(resolve_candidates(&index, id, true)))
            .count();
        let after = ids
            .iter()
            .filter(|id| priced(resolve(&index, id, true)))
            .count();
        println!(
            "offline unqualified models={} priced_before={} priced_after={}",
            ids.len(),
            before,
            after
        );
    }
    #[test]
    fn exact_decimal_bands_and_unsupported_tariffs() {
        let data = json!({"models_dev_cost":{"input":0.1,"output":0.3,"tiers":[{"tier":{"type":"context","size":200000},"input":0.2,"output":0.6}]}});
        let (low, threshold) = rates(&data, 200000, false).unwrap();
        assert_eq!(low["input_cost_per_token"], "0.0000001");
        assert_eq!(threshold, None);
        let (high, threshold) = rates(&data, 200001, false).unwrap();
        assert_eq!(high["output_cost_per_token"], "0.0000006");
        assert_eq!(threshold, Some(200000));
        assert!(rates(&data, 1, true).is_none());
        assert!(rates(
            &json!({"models_dev_cost":{"input":1,"output":2,"reasoning":3}}),
            1,
            false
        )
        .is_none());
        assert!(rates(
            &json!({"models_dev_cost":{"input":1,"output":2,"image":5}}),
            1,
            false
        )
        .is_none());
        assert!(crop(json!({})).is_err());
        assert!(crop(json!({"x":{"models":[]}})).is_err());
    }
    #[tokio::test]
    async fn existing_cache_is_offline_and_corruption_is_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let cat = catalog(
            dir.path(),
            json!({"x":{"models":{"fixture":{"cost":{"input":1,"output":2}}}}}),
        );
        let before = std::fs::read(dir.path().join("models-dev-index.json")).unwrap();
        cat.refresh(false).await.unwrap();
        assert_eq!(
            std::fs::read(dir.path().join("models-dev-index.json")).unwrap(),
            before
        );
        std::fs::write(dir.path().join("models-dev-index.json"), "broken").unwrap();
        let corrupt = Catalog::new(dir.path());
        assert!(corrupt.view().error.is_some());
        corrupt.refresh(false).await.unwrap();
        assert_eq!(
            std::fs::read(dir.path().join("models-dev-index.json")).unwrap(),
            b"broken"
        );
    }
    #[tokio::test]
    async fn refresh_coalesces_and_rejects_failed_empty_invalid_and_compressed_oversize_responses()
    {
        use std::{
            io::Write,
            sync::atomic::{AtomicUsize, Ordering},
        };
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = format!("http://{}/api.json", listener.local_addr().unwrap());
        let hits = Arc::new(AtomicUsize::new(0));
        let count = hits.clone();
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(&vec![b'x'; DOWNLOAD_LIMIT + 1]).unwrap();
        let bomb = encoder.finish().unwrap();
        let task = tokio::spawn(async move {
            for index in 0..5 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = [0; 4096];
                let _ = socket.read(&mut request).await.unwrap();
                count.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(30)).await;
                let (status,encoding,body)=match index {0=>(200,"identity",br#"{"fixture":{"models":{"fixture-model":{"cost":{"input":1,"output":2}}}}}"#.to_vec()),1=>(500,"identity",b"error".to_vec()),2=>(200,"identity",b"{}".to_vec()),3=>(200,"identity",b"invalid".to_vec()),_=>(200,"gzip",bomb.clone())};
                socket.write_all(format!("HTTP/1.1 {status} Fixture\r\nContent-Length: {}\r\nContent-Encoding: {encoding}\r\nConnection: close\r\n\r\n",body.len()).as_bytes()).await.unwrap();
                socket.write_all(&body).await.unwrap();
            }
        });
        let dir = tempfile::tempdir().unwrap();
        let cat = Catalog::new(dir.path());
        let (a, b) = tokio::join!(
            cat.refresh_from(false, &address),
            cat.refresh_from(false, &address)
        );
        assert!(a.unwrap().error.is_none());
        assert!(b.unwrap().error.is_none());
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        let before = std::fs::read(dir.path().join("models-dev-index.json")).unwrap();
        for _ in 0..4 {
            assert!(cat
                .refresh_from(true, &address)
                .await
                .unwrap()
                .error
                .is_some());
            assert!(cat.price("fixture-model").is_some());
            assert_eq!(
                std::fs::read(dir.path().join("models-dev-index.json")).unwrap(),
                before
            );
        }
        task.await.unwrap();
    }
}
