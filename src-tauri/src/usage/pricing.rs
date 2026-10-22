//! Independent adapter for Sub2API's public price source (no provider credentials).
use super::model::*;
use crate::storage::{self, Result};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
};
const SOURCE:&str="https://raw.githubusercontent.com/Wei-Shaw/model-price-repo/main/model_prices_and_context_window.json";
const HASH:&str="https://raw.githubusercontent.com/Wei-Shaw/model-price-repo/main/model_prices_and_context_window.sha256";
const LIMIT: u64 = 8 * 1024 * 1024;
type Catalog = BTreeMap<String, Value>;

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderMapping {
    pub client: String,
    pub provider: String,
    pub enabled: bool,
    pub match_on: String,
    pub from_model: String,
    pub to_model: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Config {
    pub auto_update: bool,
    pub selected: Option<BTreeSet<String>>,
    pub excluded: BTreeSet<String>,
    pub fixed: Catalog,
    pub aliases: BTreeMap<String, String>,
    pub provider_mappings: Vec<ProviderMapping>,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            auto_update: true,
            selected: None,
            excluded: BTreeSet::new(),
            fixed: BTreeMap::new(),
            aliases: BTreeMap::new(),
            provider_mappings: Vec::new(),
        }
    }
}
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct Cache {
    catalog: Catalog,
    catalog_hash: String,
    hash: String,
    etag: Option<String>,
    checked_at: Option<i64>,
    updated_at: Option<i64>,
}
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct View {
    pub config: Config,
    pub models: Catalog,
    pub version: String,
    pub source: &'static str,
    pub checked_at: Option<i64>,
    pub updated_at: Option<i64>,
    pub error: Option<String>,
    pub syncing: bool,
    pub revision: String,
}
#[derive(Clone)]
pub struct Quote {
    pub model: String,
    pub data: Option<Value>,
    pub version: String,
    pub source: String,
    pub multiplier: String,
}
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PriceComparison {
    pub model: String,
    pub context: u64,
    pub reference_at: Option<i64>,
    pub source_model: Option<String>,
    pub fields: Vec<PriceDifference>,
    pub reason: Option<String>,
}
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PriceDifference {
    pub field: String,
    pub wei_shaw: String,
    pub models_dev: String,
    pub deviation_percent: Option<String>,
    pub level: String,
    pub context_threshold: Option<u64>,
}
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Reference {
    pub source: String,
    pub input_per_m: Option<String>,
    pub output_per_m: Option<String>,
    pub cache_read_per_m: Option<String>,
    pub cache_write_per_m: Option<String>,
    pub deviation: bool,
}
fn difference(
    field: &str,
    wei: Decimal,
    reference: Decimal,
    threshold: Option<u64>,
) -> PriceDifference {
    let ratio = (wei != Decimal::ZERO)
        .then(|| (reference - wei).abs().checked_div(wei))
        .flatten();
    let level = if wei == reference {
        "equal"
    } else if wei == Decimal::ZERO || reference == Decimal::ZERO {
        "zero_conflict"
    } else if ratio.is_some_and(|n| n >= Decimal::ONE) {
        "severe"
    } else if ratio.is_some_and(|n| n >= Decimal::new(25, 2)) {
        "deviation"
    } else {
        "close"
    };
    PriceDifference {
        field: field.into(),
        wei_shaw: wei.normalize().to_string(),
        models_dev: reference.normalize().to_string(),
        deviation_percent: (level != "zero_conflict")
            .then(|| ratio.unwrap_or_default().checked_mul(Decimal::from(100)))
            .flatten()
            .map(|n| n.normalize().to_string()),
        level: level.into(),
        context_threshold: threshold,
    }
}

#[derive(Clone)]
struct Inner {
    cache: Cache,
    config: Config,
    error: Option<String>,
    syncing: bool,
    revision: String,
    generation: u64,
}
#[derive(Clone)]
pub struct Pricing {
    pub models_dev: crate::models_dev::Catalog,
    path: PathBuf,
    cache_path: PathBuf,
    state: Arc<RwLock<Arc<Inner>>>,
    sync: Arc<tokio::sync::Mutex<()>>,
}
fn validate(c: &Catalog) -> Result<()> {
    if c.len() > 30000 || c.is_empty() {
        return Err(failure("价格库为空或超过限制"));
    }
    for (id, v) in c {
        if id == "sample_spec" {
            continue;
        }
        if (id.is_empty()
            || id.len() > 256
            || id.contains("://")
            || id.starts_with('/')
            || id.chars().any(char::is_control))
            || !v.is_object()
        {
            return Err(failure("价格模型无效"));
        }
        for (k, n) in v.as_object().unwrap() {
            if (k.contains("cost") || k.ends_with("multiplier"))
                && !n.is_null()
                && (n.is_number() || n.is_string())
                && decimal(&number_text(n)).is_none()
            {
                return Err(failure("价格必须为非负数"));
            }
        }
    }
    Ok(())
}
fn catalog_hash(c: &Catalog) -> String {
    storage::digest(serde_json::to_string(c).unwrap_or_default().as_bytes())
}
fn price_basis(data: &Value) -> Value {
    Value::Object(
        data.as_object()
            .into_iter()
            .flatten()
            .filter(|(key, value)| {
                matches!(
                    key.as_str(),
                    "models_dev_cost" | "models_dev_fast_cost" | "models_dev_match" | "price_match"
                ) || (key.contains("cost") || key.contains("threshold") || key.as_str() == "mode")
                    && (value.is_number() || value.is_string() || value.is_boolean())
            })
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
    )
}
fn number_text(v: &Value) -> String {
    v.as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| v.to_string())
}
fn valid_config(c: &Config) -> Result<()> {
    let mut keys = BTreeSet::new();
    if c.provider_mappings.len() > 4096
        || c.provider_mappings.iter().any(|rule| {
            !matches!(rule.client.as_str(), "codex" | "claude" | "grok" | "dsh")
                || rule.provider.is_empty()
                || rule.provider.len() > 128
                || !rule
                    .provider
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
                || !matches!(rule.match_on.as_str(), "request" | "response")
                || model_id(Some(&rule.from_model)).is_none()
                || model_id(Some(&rule.to_model)).is_none()
                || !keys.insert((
                    &rule.client,
                    &rule.provider,
                    &rule.match_on,
                    &rule.from_model,
                ))
        })
    {
        return Err(failure("供应商计价映射无效或重复"));
    }
    if !c.fixed.is_empty() {
        validate(&c.fixed)?;
    }
    if c.aliases
        .iter()
        .any(|(a, b)| model_id(Some(a)).is_none() || model_id(Some(b)).is_none())
        || c.excluded
            .iter()
            .chain(c.selected.iter().flatten())
            .any(|m| model_id(Some(m)).is_none())
    {
        return Err(failure("价格选择无效"));
    }
    Ok(())
}
impl Pricing {
    pub fn new(dir: &Path) -> Result<Self> {
        let path = dir.join("pricing.json");
        let cache_path = dir.join("price-cache.json");
        let saved = storage::read_bounded(&path, LIMIT)?;
        let decoded = saved
            .as_deref()
            .map(serde_json::from_slice::<Config>)
            .transpose();
        let (config, mut error) = match decoded {
            Ok(Some(c)) if valid_config(&c).is_ok() => (c, None),
            Ok(None) => (Config::default(), None),
            _ => (
                Config::default(),
                Some("定价文件无效，正在使用有效价格源".into()),
            ),
        };
        let seed = include_bytes!("../../resources/usage-prices.json");
        if storage::digest(seed) != include_str!("../../resources/usage-prices.sha256").trim() {
            return Err(failure("初始价格库校验失败"));
        }
        let cached = storage::read_bounded(&cache_path, LIMIT)
            .ok()
            .flatten()
            .and_then(|v| serde_json::from_slice::<Cache>(&v).ok())
            .filter(|v| validate(&v.catalog).is_ok() && v.catalog_hash == catalog_hash(&v.catalog));
        if cache_path.exists() && cached.is_none() {
            error = Some("价格缓存校验失败，正在使用初始价格库".into());
        }
        let seed_catalog: Catalog =
            serde_json::from_slice(seed).map_err(|_| failure("初始价格库无效"))?;
        let cache = cached.unwrap_or(Cache {
            catalog_hash: catalog_hash(&seed_catalog),
            catalog: seed_catalog,
            hash: storage::digest(seed),
            updated_at: chrono::DateTime::parse_from_rfc3339("2026-10-08T00:00:00Z")
                .ok()
                .map(|d| d.timestamp_millis()),
            ..Cache::default()
        });
        Ok(Self {
            models_dev: crate::models_dev::Catalog::new(dir),
            path,
            cache_path,
            state: Arc::new(RwLock::new(Arc::new(Inner {
                cache,
                config,
                error,
                syncing: false,
                generation: 0,
                revision: storage::revision(saved.as_deref()),
            }))),
            sync: Arc::new(tokio::sync::Mutex::new(())),
        })
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn frozen(&self) -> Self {
        Self {
            models_dev: self.models_dev.frozen(),
            path: self.path.clone(),
            cache_path: self.cache_path.clone(),
            state: Arc::new(RwLock::new(self.state.read().unwrap().clone())),
            sync: Arc::new(tokio::sync::Mutex::new(())),
        }
    }
    pub fn view(&self) -> View {
        let s = self.state.read().unwrap();
        let mut models = s.cache.catalog.clone();
        models.extend(s.config.fixed.clone());
        models.remove("sample_spec");
        View {
            config: s.config.clone(),
            models,
            version: s.cache.hash.clone(),
            source: "Sub2API · Wei-Shaw/model-price-repo",
            checked_at: s.cache.checked_at,
            updated_at: s.cache.updated_at,
            error: s.error.clone(),
            syncing: s.syncing,
            revision: s.revision.clone(),
        }
    }
    pub fn configure(&self, c: Config, expected: &str) -> Result<View> {
        valid_config(&c)?;
        let mut guard = self.state.write().unwrap();
        let s = Arc::make_mut(&mut guard);
        if s.revision != expected {
            return Err(failure("定价已变化，请重新读取后重试"));
        }
        let bytes = serde_json::to_vec_pretty(&c).map_err(|_| failure("定价无法保存"))?;
        storage::atomic_write_bounded(&self.path, &bytes, Some(expected), LIMIT)?;
        s.revision = storage::digest(&bytes);
        s.config = c;
        drop(guard);
        Ok(self.view())
    }
    pub fn reload(&self) -> Result<View> {
        let bytes =
            storage::read_bounded(&self.path, LIMIT)?.ok_or_else(|| failure("定价文件不存在"))?;
        let c: Config =
            serde_json::from_slice(&bytes).map_err(|_| failure("定价文件无效，已保留当前价格"))?;
        valid_config(&c)?;
        let mut guard = self.state.write().unwrap();
        let s = Arc::make_mut(&mut guard);
        s.config = c;
        s.revision = storage::digest(&bytes);
        drop(guard);
        Ok(self.view())
    }
    pub fn quote(&self, model: Option<&str>, multiplier: &str) -> Quote {
        let s = self.state.read().unwrap();
        let original = model.unwrap_or("");
        let mut canonical =
            if s.config.fixed.contains_key(original) || s.cache.catalog.contains_key(original) {
                original
            } else {
                s.config
                    .aliases
                    .get(original)
                    .map(String::as_str)
                    .unwrap_or(original)
            };
        let mut matched_by = "exact";
        if !s.config.fixed.contains_key(canonical) && !s.cache.catalog.contains_key(canonical) {
            let names = crate::models_dev::names(canonical, true);
            let matches: Vec<_> = s
                .cache
                .catalog
                .keys()
                .filter(|key| names.contains(&key.to_ascii_lowercase()))
                .collect();
            if matches.len() == 1 {
                canonical = matches[0];
                matched_by = "normalized";
            }
        }
        let fixed = s.config.fixed.get(canonical);
        let allowed = !s.config.excluded.contains(original)
            && !s.config.excluded.contains(canonical)
            && s.config
                .selected
                .as_ref()
                .is_none_or(|v| v.contains(canonical));
        let mut data = fixed
            .or_else(|| allowed.then(|| s.cache.catalog.get(canonical)).flatten())
            .cloned();
        if allowed && fixed.is_none() && !s.cache.catalog.contains_key(canonical) {
            if let Some((data, version)) = self.models_dev.price(canonical) {
                if !data["models_dev_match"]["model"]
                    .as_str()
                    .is_some_and(|model| s.config.excluded.contains(model))
                {
                    return Quote {
                        model: canonical.into(),
                        data: Some(data),
                        version,
                        source: "models.dev".into(),
                        multiplier: multiplier.into(),
                    };
                }
            }
        }
        if matched_by == "normalized" {
            if let Some(data) = data.as_mut().and_then(Value::as_object_mut) {
                data.insert(
                    "price_match".into(),
                    serde_json::json!({"matchedBy":matched_by,"model":canonical}),
                );
            }
        }
        Quote {
            model: canonical.into(),
            version: if let Some(v) = fixed {
                storage::digest(v.to_string().as_bytes())
            } else {
                s.cache.hash.clone()
            },
            source: if fixed.is_some() { "fixed" } else { "sub2api" }.into(),
            data,
            multiplier: multiplier.into(),
        }
    }
    pub fn reference(&self, model: &str, source: Option<&str>) -> Option<Reference> {
        let quote = self.quote(Some(model), "1");
        if quote.data.is_none() {
            let state = self.state.read().unwrap();
            if state.config.excluded.contains(model)
                || state.config.excluded.contains(&quote.model)
                || state
                    .config
                    .selected
                    .as_ref()
                    .is_some_and(|v| !v.contains(&quote.model))
            {
                return None;
            }
        }
        let (data, price_source) = if quote.source == "models.dev" || quote.data.is_none() {
            (
                self.models_dev.price_from(model, source)?.0,
                "models.dev".to_owned(),
            )
        } else {
            (quote.data?, quote.source)
        };
        let data = if price_source == "models.dev" {
            crate::models_dev::rates(&data, 0, false)?.0
        } else {
            data
        };
        let per_m = |key| {
            unit_rate(&data, key, 0, "").and_then(|(_, value, _)| {
                value
                    .checked_mul(Decimal::from(1_000_000))
                    .map(|v| v.normalize().to_string())
            })
        };
        Some(Reference {
            source: price_source,
            input_per_m: per_m("input_cost_per_token"),
            output_per_m: per_m("output_cost_per_token"),
            cache_read_per_m: per_m("cache_read_input_token_cost"),
            cache_write_per_m: per_m("cache_creation_input_token_cost"),
            deviation: self
                .compare_from(model, 0, source)
                .fields
                .iter()
                .any(|v| matches!(v.level.as_str(), "deviation" | "severe" | "zero_conflict")),
        })
    }
    pub fn compare(&self, model: &str, context: u64) -> PriceComparison {
        self.compare_from(model, context, None)
    }
    fn compare_from(&self, model: &str, context: u64, source: Option<&str>) -> PriceComparison {
        let mut result = PriceComparison {
            model: model.into(),
            context,
            reference_at: self.models_dev.view().updated_at,
            source_model: None,
            fields: vec![],
            reason: Some("当前参考：来源、模型或价格阶梯缺少可比依据".into()),
        };
        let state = self.state.read().unwrap();
        let Some(wei) = state.cache.catalog.get(model) else {
            return result;
        };
        let provider = wei["litellm_provider"].as_str();
        let qualified = provider.map(|provider| format!("{provider}/{model}"));
        let Some((data, _)) = self
            .models_dev
            .price_from(model, source.or(qualified.as_deref()))
        else {
            return result;
        };
        if source.is_none()
            && provider.is_some_and(|provider| {
                data["models_dev_match"]["providers"] != serde_json::json!([provider])
            })
        {
            return result;
        }
        result.source_model = data["models_dev_match"]["model"]
            .as_str()
            .map(str::to_owned);
        let Some((rates, threshold)) = crate::models_dev::rates(&data, context, false) else {
            return result;
        };
        for (field, key) in [
            ("input", "input_cost_per_token"),
            ("output", "output_cost_per_token"),
            ("cache_read", "cache_read_input_token_cost"),
            ("cache_write", "cache_creation_input_token_cost"),
        ] {
            let Some((_, price, wei_threshold)) = unit_rate(wei, key, context, "") else {
                continue;
            };
            let Some(reference) = rates.get(key).and_then(|v| decimal(&number_text(v))) else {
                continue;
            };
            if threshold != wei_threshold {
                continue;
            }
            result
                .fields
                .push(difference(field, price, reference, threshold));
        }
        if !result.fields.is_empty() {
            result.reason = None;
        }
        result
    }
    pub fn resolve(
        &self,
        client: &str,
        provider: Option<&str>,
        request: Option<&str>,
        response: Option<&str>,
        preference: &str,
    ) -> (Option<String>, String, Option<String>) {
        let s = self.state.read().unwrap();
        // Response-specific rules take precedence over request rules. A rule is
        // explicit user knowledge, never an inference about hidden upstream models.
        for match_on in ["response", "request"] {
            let visible = if match_on == "response" {
                response
            } else {
                request
            };
            if let Some(rule) = s.config.provider_mappings.iter().find(|rule| {
                rule.enabled
                    && rule.client == client
                    && Some(rule.provider.as_str()) == provider
                    && rule.match_on == match_on
                    && Some(rule.from_model.as_str()) == visible
            }) {
                return (
                    Some(rule.to_model.clone()),
                    "provider_mapping".into(),
                    Some(storage::digest(
                        serde_json::to_vec(rule).unwrap_or_default().as_slice(),
                    )),
                );
            }
        }
        let (model, basis) = if preference == "request" && request.is_some() {
            (request, "request")
        } else if response.is_some() {
            (response, "response")
        } else {
            (request, "request")
        };
        (model.map(str::to_owned), basis.into(), None)
    }
    pub async fn update(&self, force: bool) -> Result<View> {
        self.update_from(force, SOURCE, HASH).await
    }
    pub(super) async fn update_from(
        &self,
        force: bool,
        source: &str,
        hash_url: &str,
    ) -> Result<View> {
        let generation = self.state.read().unwrap().generation;
        let _guard = self.sync.lock().await;
        {
            let state = self.state.read().unwrap();
            if state.generation != generation {
                return Ok(self.view_unlocked(&state));
            }
        }
        {
            let s = self.state.read().unwrap();
            if !force
                && (!s.config.auto_update
                    || s.cache.checked_at.is_some_and(|t| now() - t < 600_000))
            {
                return Ok(self.view_unlocked(&s));
            }
        }
        Arc::make_mut(&mut self.state.write().unwrap()).syncing = true;
        let result = self.download(source, hash_url, force).await;
        {
            let mut guard = self.state.write().unwrap();
            let s = Arc::make_mut(&mut guard);
            s.syncing = false;
            s.generation = s.generation.wrapping_add(1);
            s.cache.checked_at = Some(now());
            match &result {
                Ok(()) => s.error = None,
                Err(e) => s.error = Some(e.message.clone()),
            };
        }
        result.map(|_| self.view())
    }
    fn view_unlocked(&self, s: &Inner) -> View {
        let mut models = s.cache.catalog.clone();
        models.extend(s.config.fixed.clone());
        models.remove("sample_spec");
        View {
            config: s.config.clone(),
            models,
            version: s.cache.hash.clone(),
            source: "Sub2API · Wei-Shaw/model-price-repo",
            checked_at: s.cache.checked_at,
            updated_at: s.cache.updated_at,
            error: s.error.clone(),
            syncing: s.syncing,
            revision: s.revision.clone(),
        }
    }
    async fn download(&self, source: &str, hash_url: &str, force: bool) -> Result<()> {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(20))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| failure("价格连接初始化失败"))?;
        let (etag, oldhash) = {
            let s = self.state.read().unwrap();
            (s.cache.etag.clone(), s.cache.hash.clone())
        };
        let hash_body = fetch(&client, hash_url, None, 1024).await?;
        let hash = String::from_utf8(hash_body.1).map_err(|_| failure("价格校验码无效"))?;
        let mut hash = hash
            .split_whitespace()
            .next()
            .filter(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()))
            .ok_or_else(|| failure("价格校验码无效"))?
            .to_ascii_lowercase();
        if !force && hash == oldhash {
            let mut cache = self.state.read().unwrap().cache.clone();
            cache.checked_at = Some(now());
            let bytes = serde_json::to_vec(&cache).map_err(|_| failure("价格缓存保存失败"))?;
            storage::atomic_write_bounded(&self.cache_path, &bytes, None, LIMIT)?;
            Arc::make_mut(&mut self.state.write().unwrap()).cache = cache;
            return Ok(());
        }
        let mut response = fetch(&client, source, etag.as_deref(), LIMIT).await?;
        if response.0 == 304 && hash != oldhash {
            response = fetch(&client, source, None, LIMIT).await?;
        }
        let mut cache = self.state.read().unwrap().cache.clone();
        if response.0 != 304 {
            if storage::digest(&response.1) != hash {
                // The two public objects may be published at slightly different times.
                // Retry the pair once; never install unverified data.
                let retry = fetch(&client, hash_url, None, 1024).await?;
                hash = String::from_utf8(retry.1)
                    .ok()
                    .and_then(|s| s.split_whitespace().next().map(str::to_owned))
                    .filter(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()))
                    .ok_or_else(|| failure("价格校验码无效"))?
                    .to_ascii_lowercase();
                response = fetch(&client, source, None, LIMIT).await?;
                if storage::digest(&response.1) != hash {
                    return Err(failure("价格哈希不匹配，已保留上次有效数据"));
                }
            }
            let catalog: Catalog =
                serde_json::from_slice(&response.1).map_err(|_| failure("价格数据无效"))?;
            validate(&catalog)?;
            if cache.hash != hash {
                cache.catalog_hash = catalog_hash(&catalog);
                cache.catalog = catalog;
                cache.hash = hash;
                cache.updated_at = Some(now());
            }
            cache.etag = response.2;
        }
        cache.checked_at = Some(now());
        let bytes = serde_json::to_vec(&cache).map_err(|_| failure("价格缓存保存失败"))?;
        storage::atomic_write_bounded(&self.cache_path, &bytes, None, LIMIT)?;
        Arc::make_mut(&mut self.state.write().unwrap()).cache = cache;
        Ok(())
    }
}
async fn fetch(
    client: &reqwest::Client,
    url: &str,
    etag: Option<&str>,
    limit: u64,
) -> Result<(u16, Vec<u8>, Option<String>)> {
    let mut request = client.get(url);
    if let Some(e) = etag {
        request = request.header(reqwest::header::IF_NONE_MATCH, e);
    }
    let mut r = request
        .send()
        .await
        .map_err(|_| failure("价格更新连接失败，正在使用缓存"))?;
    let status = r.status().as_u16();
    if status != 200 && status != 304 {
        return Err(failure("价格源暂不可用，正在使用缓存"));
    }
    let etag = r
        .headers()
        .get(reqwest::header::ETAG)
        .and_then(|h| h.to_str().ok())
        .filter(|s| s.len() < 512)
        .map(str::to_owned);
    let mut bytes = Vec::new();
    while let Some(chunk) = r.chunk().await.map_err(|_| failure("价格下载中断"))? {
        if bytes.len() as u64 + chunk.len() as u64 > limit {
            return Err(failure("价格响应超过限制"));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok((status, bytes, etag))
}
impl Quote {
    pub fn calculate(&self, t: &Tokens, tier: Option<&str>) -> Option<PriceSnapshot> {
        let original = self.data.as_ref()?;
        let context = t
            .input?
            .saturating_add(t.cache_read.unwrap_or(0))
            .saturating_add(t.cache_write.unwrap_or(0));
        let converted = if self.source == "models.dev" {
            Some(crate::models_dev::rates(
                original,
                context,
                tier == Some("fast"),
            )?)
        } else {
            None
        };
        let data = converted.as_ref().map(|(data, _)| data).unwrap_or(original);
        let suffix = match tier.unwrap_or("default") {
            "default" | "standard" => "",
            "batch" => "_batches",
            "flex" => "_flex",
            "priority" => "_priority",
            "ultrafast" => "_ultrafast",
            "fast" => "_fast",
            _ => return None,
        };
        let mut rates = BTreeMap::new();
        let mut cost = Decimal::ZERO;
        let mut charge = |name: &str, count: u64| -> Option<()> {
            if count == 0 {
                return Some(());
            }
            let (key, rate, _) = unit_rate(data, name, context, suffix)?;
            rates.insert(key, rate.to_string());
            cost = cost.checked_add(rate.checked_mul(Decimal::from(count))?)?;
            Some(())
        };
        let fresh = t.input?;
        let output = t.output?;
        // Media token counts are subsets of protocol input/output, never additional tokens.
        charge(
            "input_cost_per_token",
            fresh
                .checked_sub(t.image_input.unwrap_or(0))?
                .checked_sub(t.audio_input.unwrap_or(0))?,
        )?;
        charge(
            "output_cost_per_token",
            output
                .checked_sub(t.image_output.unwrap_or(0))?
                .checked_sub(t.audio_output.unwrap_or(0))?,
        )?;
        charge("input_cost_per_image_token", t.image_input.unwrap_or(0))?;
        charge("output_cost_per_image_token", t.image_output.unwrap_or(0))?;
        charge("input_cost_per_audio_token", t.audio_input.unwrap_or(0))?;
        charge("output_cost_per_audio_token", t.audio_output.unwrap_or(0))?;
        charge("cache_read_input_token_cost", t.cache_read.unwrap_or(0))?;
        let long = t.cache_write_1h.unwrap_or(0);
        charge("cache_creation_input_token_cost_above_1hr", long)?;
        charge(
            "cache_creation_input_token_cost",
            t.cache_write.unwrap_or(0).checked_sub(long)?,
        )?;
        if data["mode"].as_str().is_some_and(|s| {
            matches!(
                s,
                "image_generation" | "video_generation" | "audio_transcription"
            )
        }) {
            return None;
        }
        cost = cost.checked_mul(decimal(&self.multiplier)?)?;
        Some(PriceSnapshot {
            version: self.version.clone(),
            source: self.source.clone(),
            model: self.model.clone(),
            multiplier: self.multiplier.clone(),
            rates,
            cost: cost.normalize().to_string(),
            basis: Some({
                let mut basis = price_basis(original);
                if let Some((_, threshold)) = &converted {
                    basis["models_dev_context_threshold"] = serde_json::json!(threshold);
                }
                basis
            }),
        })
    }
}

fn unit_rate(
    data: &Value,
    name: &str,
    context: u64,
    suffix: &str,
) -> Option<(String, Decimal, Option<u64>)> {
    let mut key = format!("{name}{suffix}");
    let mut band = None;
    for threshold in [100_000, 200_000, 272_000] {
        let candidate = format!("{name}_above_{}k_tokens{suffix}", threshold / 1000);
        if context > threshold && data.get(&candidate).is_some() {
            key = candidate;
            band = Some(threshold);
        }
    }
    let mut rate = decimal(&number_text(data.get(&key)?))?;
    if let Some(threshold) = data["long_context_input_token_threshold"].as_u64() {
        let reached = if data["long_context_threshold_inclusive"]
            .as_bool()
            .unwrap_or(false)
        {
            context >= threshold
        } else {
            context > threshold
        };
        if reached {
            let multiplier = match name {
                "input_cost_per_token" => Some("long_context_input_cost_multiplier"),
                "output_cost_per_token" => Some("long_context_output_cost_multiplier"),
                "cache_read_input_token_cost" => Some("long_context_cache_read_cost_multiplier"),
                _ => None,
            };
            if let Some(v) = multiplier.and_then(|k| data.get(k)) {
                rate = rate.checked_mul(decimal(&number_text(v))?)?;
                band = Some(threshold);
            }
        }
    }
    Some((key, rate, band))
}

#[cfg(test)]
mod v02514_tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn deviations_include_boundaries_and_zero_conflicts() {
        for (a, b, level, percent) in [
            (4, 5, "deviation", Some("25")),
            (4, 8, "severe", Some("100")),
            (0, 0, "equal", Some("0")),
            (0, 1, "zero_conflict", None),
            (1, 0, "zero_conflict", None),
        ] {
            let difference = difference("input", Decimal::from(a), Decimal::from(b), None);
            assert_eq!(difference.level, level);
            assert_eq!(difference.deviation_percent.as_deref(), percent);
        }
    }
    #[test]
    fn fallback_respects_exclusions_partial_primary_tariffs_ttl_and_fast() {
        let dir = tempfile::tempdir().unwrap();
        let public = json!({"schema":1,"updated_at":1,"providers":{"fixture":{"name":"Fixture","models":{"model":{"cost":{"input":1,"output":2,"cache_write":3}},"primary":{"cost":{"input":1,"output":2}},"unpriced":{"cost":{"input":1,"output":2}}}}}});
        std::fs::write(dir.path().join("models-dev-index.json"), public.to_string()).unwrap();
        let p = Pricing::new(dir.path()).unwrap();
        let tokens = Tokens {
            input: Some(10),
            output: Some(20),
            reasoning: Some(15),
            ..Tokens::default()
        };
        let quote = p.quote(Some("model"), "1");
        assert_eq!(quote.source, "models.dev");
        let price = quote.calculate(&tokens, None).unwrap();
        assert_eq!(price.cost, "0.00005");
        assert!(quote.calculate(&tokens, Some("fast")).is_none());
        assert!(quote
            .calculate(
                &Tokens {
                    cache_write: Some(2),
                    cache_write_1h: Some(2),
                    ..tokens.clone()
                },
                None
            )
            .is_none());
        {
            let mut state = p.state.write().unwrap();
            let s = Arc::make_mut(&mut state);
            s.config.excluded.insert("model".into());
            s.cache
                .catalog
                .insert("primary".into(), json!({"input_cost_per_token":"0.1"}));
            s.cache.catalog.insert(
                "claude-opus-4-6".into(),
                json!({"input_cost_per_token":"0.1","output_cost_per_token":"0.2"}),
            );
        }
        assert!(p.quote(Some("model"), "1").data.is_none());
        assert!(p.quote(Some("fixture/model"), "1").data.is_none());
        assert!(p
            .quote(Some("primary"), "1")
            .calculate(&tokens, None)
            .is_none());
        assert_eq!(
            p.quote(Some("relay-claude-opus-4-6(high)"), "1").model,
            "claude-opus-4-6"
        );
        assert!(p
            .quote(Some("relay-claude-opus-4-6:free"), "1")
            .data
            .is_none());
    }
}
