use crate::storage::{AppError, Result};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::str::FromStr;

fn one() -> String {
    "1".into()
}
pub fn now() -> i64 {
    chrono::Utc::now().timestamp_millis()
}
pub fn failure(message: &str) -> AppError {
    AppError::new("USAGE", message)
}
pub fn decimal(value: &str) -> Option<Decimal> {
    Decimal::from_str(value)
        .ok()
        .filter(|v| *v >= Decimal::ZERO)
}
pub fn safe_id(value: &str) -> String {
    crate::storage::digest(value.as_bytes())
}
pub fn model_id(value: Option<&str>) -> Option<String> {
    value.and_then(crate::events::safe_model)
}

/// Fresh input excludes cache reads/writes. None means unreported, never zero.
#[derive(Clone, Default, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tokens {
    pub input: Option<u64>,
    pub output: Option<u64>,
    pub cache_read: Option<u64>,
    pub cache_write: Option<u64>,
    pub cache_write_5m: Option<u64>,
    pub cache_write_1h: Option<u64>,
    pub image_input: Option<u64>,
    pub image_output: Option<u64>,
    pub audio_input: Option<u64>,
    pub audio_output: Option<u64>,
}
impl Tokens {
    pub fn total(&self) -> Option<u64> {
        [self.input, self.output, self.cache_read, self.cache_write]
            .into_iter()
            .flatten()
            .reduce(u64::saturating_add)
    }
    pub fn merge(&mut self, other: &Self) {
        macro_rules! merge { ($($f:ident),*) => {$(if other.$f.is_some() { self.$f=other.$f; })*}; }
        merge!(
            input,
            output,
            cache_read,
            cache_write,
            cache_write_5m,
            cache_write_1h,
            image_input,
            image_output,
            audio_input,
            audio_output
        );
    }
    pub fn add(&mut self, other: &Self) {
        macro_rules! add { ($($f:ident),*) => {$(self.$f=match(self.$f,other.$f){(None,None)=>None,(a,b)=>Some(a.unwrap_or(0).saturating_add(b.unwrap_or(0)))};)*}; }
        add!(
            input,
            output,
            cache_read,
            cache_write,
            cache_write_5m,
            cache_write_1h,
            image_input,
            image_output,
            audio_input,
            audio_output
        );
    }
    pub fn delta(&self, old: &Self) -> Self {
        let mut next = self.clone();
        macro_rules! delta { ($($f:ident),*) => {$(next.$f=self.$f.map(|v|v.saturating_sub(old.$f.unwrap_or(0)));)*}; }
        delta!(
            input,
            output,
            cache_read,
            cache_write,
            cache_write_5m,
            cache_write_1h,
            image_input,
            image_output,
            audio_input,
            audio_output
        );
        next
    }
}
fn count(v: &Value, names: &[&str]) -> Option<u64> {
    names
        .iter()
        .find_map(|n| v.get(n)?.as_u64())
        .filter(|v| *v <= 1_000_000_000_000)
}

/// Adapted from CC Switch's OpenAI-inclusive / Anthropic-exclusive cache rules.
pub fn parse_tokens(usage: &Value, claude: bool) -> Tokens {
    let details = usage
        .get("input_tokens_details")
        .or_else(|| usage.get("prompt_tokens_details"))
        .unwrap_or(&Value::Null);
    let cache = count(usage, &["cache_read_input_tokens", "cached_input_tokens"])
        .or_else(|| count(details, &["cached_tokens"]));
    let write = count(
        usage,
        &["cache_creation_input_tokens", "cache_creation_tokens"],
    );
    let raw = count(usage, &["input_tokens", "prompt_tokens"]);
    let create = &usage["cache_creation"];
    Tokens {
        input: raw.map(|v| {
            if claude {
                v
            } else {
                v.saturating_sub(cache.unwrap_or(0))
                    .saturating_sub(write.unwrap_or(0))
            }
        }),
        output: count(usage, &["output_tokens", "completion_tokens"]),
        cache_read: cache,
        cache_write: write,
        cache_write_5m: count(create, &["ephemeral_5m_input_tokens"]),
        cache_write_1h: count(create, &["ephemeral_1h_input_tokens"]),
        image_input: count(details, &["image_tokens"]),
        image_output: count(&usage["output_tokens_details"], &["image_tokens"]),
        audio_input: count(details, &["audio_tokens"]),
        audio_output: count(&usage["output_tokens_details"], &["audio_tokens"]),
    }
}

#[derive(Clone, Default, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Meter {
    pub tokens: Tokens,
    pub response_id: Option<String>,
    pub model: Option<String>,
    pub service_tier: Option<String>,
    pub first_token_ms: Option<u64>,
}
impl Meter {
    pub fn observe(&mut self, outer: &Value, elapsed: u64) {
        let v = outer
            .get("response")
            .or_else(|| outer.get("message"))
            .filter(|v| v.is_object())
            .unwrap_or(outer);
        let event = outer["type"].as_str().unwrap_or("");
        let claude = event.starts_with("message_")
            || v["type"] == "message"
            || v["usage"].get("cache_creation_input_tokens").is_some();
        if let Some(u) = v.get("usage").filter(|u| u.is_object()) {
            self.tokens.merge(&parse_tokens(u, claude));
        }
        if !std::ptr::eq(v, outer) {
            if let Some(u) = outer.get("usage") {
                self.tokens.merge(&parse_tokens(u, claude));
            }
        }
        if let Some(model) = model_id(v["model"].as_str()) {
            self.model = Some(model);
        }
        if let Some(id) = v["id"].as_str().filter(|s| s.len() <= 512) {
            self.response_id = Some(safe_id(id));
        }
        if let Some(tier) = v["service_tier"].as_str().filter(|s| {
            matches!(
                *s,
                "default" | "flex" | "priority" | "standard" | "batch" | "ultrafast"
            )
        }) {
            self.service_tier = Some(tier.into());
        }
        let generated = event == "response.output_text.delta"
            || event == "response.reasoning_text.delta"
            || event == "response.reasoning_summary_text.delta"
            || event == "response.function_call_arguments.delta"
            || event == "content_block_delta"
            || outer["choices"]
                .as_array()
                .is_some_and(|c| c.iter().any(|v| v["delta"]["content"].is_string()));
        if generated && self.first_token_ms.is_none() {
            self.first_token_ms = Some(elapsed);
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PriceSnapshot {
    pub version: String,
    pub source: String,
    pub model: String,
    pub multiplier: String,
    pub rates: std::collections::BTreeMap<String, String>,
    pub cost: String,
    #[serde(default)]
    pub basis: Option<Value>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Attempt {
    #[serde(default = "one")]
    pub cost_multiplier: String,
    pub id: String,
    pub provider: Option<String>,
    pub requested_model: Option<String>,
    pub response_model: Option<String>,
    pub pricing_model: Option<String>,
    pub response_id: Option<String>,
    pub status: Option<u16>,
    pub outcome: String,
    pub started_at: i64,
    pub duration_ms: u64,
    pub first_token_ms: Option<u64>,
    pub stream: bool,
    pub transport: String,
    pub tokens: Tokens,
    pub service_tier: Option<String>,
    pub price: Option<PriceSnapshot>,
}
impl Default for Attempt {
    fn default() -> Self {
        Self {
            cost_multiplier: one(),
            id: String::new(),
            provider: None,
            requested_model: None,
            response_model: None,
            pricing_model: None,
            response_id: None,
            status: None,
            outcome: String::new(),
            started_at: 0,
            duration_ms: 0,
            first_token_ms: None,
            stream: false,
            transport: String::new(),
            tokens: Tokens::default(),
            service_tier: None,
            price: None,
        }
    }
}
#[derive(Clone, Default, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Record {
    pub id: String,
    pub client: String,
    pub source: String,
    pub started_at: i64,
    pub session_id: Option<String>,
    pub attempts: Vec<Attempt>,
    pub completed: bool,
    #[serde(default)]
    pub estimated_speed: bool,
    #[serde(default)]
    pub duplicate_of: Option<String>,
    #[serde(default)]
    pub deduplication: String,
    #[serde(default)]
    pub merged_sources: Vec<String>,
}
impl Record {
    pub fn final_attempt(&self) -> Option<&Attempt> {
        self.attempts.last()
    }
    pub fn tokens(&self) -> Tokens {
        let mut t = Tokens::default();
        for a in &self.attempts {
            t.add(&a.tokens)
        }
        t
    }
    pub fn cost(&self) -> Option<Decimal> {
        if self.attempts.is_empty() || self.attempts.iter().any(|a| a.price.is_none()) {
            return None;
        }
        self.attempts.iter().try_fold(Decimal::ZERO, |sum, a| {
            sum.checked_add(decimal(&a.price.as_ref()?.cost)?)
        })
    }
    pub fn signature(&self) -> Option<String> {
        let a = self.final_attempt()?;
        if a.tokens.total().is_none_or(|n| n == 0) {
            return None;
        }
        let model = a.pricing_model.as_ref()?;
        Some(safe_id(&format!(
            "{}:{model}:{}",
            self.client,
            serde_json::to_string(&a.tokens).ok()?
        )))
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Settings {
    pub recording: bool,
    pub auto_sync: bool,
    pub refresh_seconds: u32,
    pub multiplier: String,
    pub pricing_model: String,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            recording: true,
            auto_sync: true,
            refresh_seconds: 30,
            multiplier: "1".into(),
            pricing_model: "response".into(),
        }
    }
}
impl Settings {
    pub fn validate(&self) -> Result<()> {
        if ![0, 5, 10, 30, 60].contains(&self.refresh_seconds)
            || !matches!(self.pricing_model.as_str(), "request" | "response")
            || decimal(&self.multiplier).is_none_or(|v| v > Decimal::from(10000))
        {
            return Err(failure("用量设置无效"));
        }
        Ok(())
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Filter {
    pub start: Option<i64>,
    pub end: Option<i64>,
    pub client: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub status: Option<String>,
    pub page: u32,
    pub sort: Option<String>,
}
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Totals {
    pub requests: u64,
    pub success: u64,
    pub status_known: u64,
    pub sessions: u64,
    pub tokens: Tokens,
    pub cost: String,
    pub unpriced: u64,
    pub duration_ms: u64,
    pub measured_outputs: u64,
    pub generation_ms: u64,
}
impl Totals {
    pub fn add_record(&mut self, r: &Record) {
        self.requests += 1;
        self.tokens.add(&r.tokens());
        let known = r
            .attempts
            .iter()
            .filter_map(|a| a.price.as_ref().and_then(|p| decimal(&p.cost)))
            .fold(Decimal::ZERO, |sum, cost| sum + cost);
        self.cost = (decimal(&self.cost).unwrap_or_default() + known).to_string();
        if r.cost().is_none() {
            self.unpriced += 1;
        }
        if let Some(a) = r.final_attempt() {
            self.duration_ms += a.duration_ms;
            if let Some(s) = a.status {
                self.status_known += 1;
                if (200..300).contains(&s) || s == 101 && r.completed {
                    self.success += 1;
                }
            } else if r.source != "proxy" {
                self.sessions += 1;
            }
            if let (Some(t), Some(out)) = (a.first_token_ms, a.tokens.output) {
                if a.duration_ms > t {
                    self.generation_ms += a.duration_ms - t;
                    self.measured_outputs += out;
                }
            }
        }
    }
    pub fn add(&mut self, o: &Self) {
        self.requests += o.requests;
        self.success += o.success;
        self.status_known += o.status_known;
        self.sessions += o.sessions;
        self.tokens.add(&o.tokens);
        self.cost = (decimal(&self.cost).unwrap_or_default()
            + decimal(&o.cost).unwrap_or_default())
        .to_string();
        self.unpriced += o.unpriced;
        self.duration_ms += o.duration_ms;
        self.measured_outputs += o.measured_outputs;
        self.generation_ms += o.generation_ms;
    }
}
