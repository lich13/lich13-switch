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
    pub fn fill_missing(&mut self, other: &Self) -> Vec<String> {
        let mut fields = Vec::new();
        macro_rules! fill { ($($f:ident),*) => {$(if self.$f.is_none() && other.$f.is_some() { self.$f=other.$f; fields.push(stringify!($f).to_owned()); })*}; }
        fill!(
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
        fields
    }
    pub fn cache_sample(&self) -> Option<(u64, u64)> {
        let (input, read, write) = (self.input?, self.cache_read?, self.cache_write?);
        Some((read, input.saturating_add(read).saturating_add(write)))
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
    )
    .or_else(|| {
        (!claude && (usage.get("input_tokens").is_some() || usage.get("prompt_tokens").is_some()))
            .then_some(0)
    });
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
    #[serde(skip)]
    pub inclusive_input: Option<u64>,
    #[serde(skip)]
    pub ended_early: bool,
    pub response_id: Option<String>,
    pub model: Option<String>,
    pub service_tier: Option<String>,
    pub first_token_ms: Option<u64>,
    pub parse_incomplete: bool,
    pub compaction_kind: Option<String>,
}
/// Only protocol envelopes can supply response identity; output/tool IDs cannot.
pub fn response_id(value: &Value) -> Option<&str> {
    let mut envelopes = vec![value];
    let mut cursor = 0;
    let mut found = None;
    while cursor < envelopes.len() && cursor < 16 {
        let node = envelopes[cursor];
        cursor += 1;
        let kind = node.get("type").and_then(Value::as_str).unwrap_or("");
        let object = node.get("object").and_then(Value::as_str).unwrap_or("");
        let explicit = node.get("response_id").and_then(Value::as_str);
        let id = explicit.or_else(|| {
            (kind.is_empty()
                || kind == "message"
                || matches!(
                    object,
                    "response"
                        | "response.compaction"
                        | "chat.completion"
                        | "chat.completion.chunk"
                ))
            .then(|| node.get("id").and_then(Value::as_str))
            .flatten()
        });
        if let Some(id) =
            id.filter(|id| !id.is_empty() && id.len() <= 256 && !id.chars().any(char::is_control))
        {
            found = Some(id);
        }
        for key in ["response", "message", "data", "result"] {
            if let Some(inner) = node.get(key).filter(|v| v.is_object()) {
                envelopes.push(inner);
            }
        }
    }
    found
}
impl Meter {
    pub fn observe(&mut self, outer: &Value, elapsed: u64) {
        let event = outer["type"].as_str().unwrap_or("");
        let terminal = matches!(
            event,
            "response.completed" | "response.done" | "message_stop"
        );
        let mut envelopes = vec![outer];
        let mut cursor = 0;
        while cursor < envelopes.len() && cursor < 16 {
            let v = envelopes[cursor];
            cursor += 1;
            let claude = event.starts_with("message_")
                || v["type"] == "message"
                || v["usage"].get("cache_creation_input_tokens").is_some()
                || v["usage"].get("cache_read_input_tokens").is_some();
            if let Some(u) = v.get("usage").filter(|u| u.is_object()).or_else(|| {
                matches!(
                    event,
                    "usage" | "usage.updated" | "response.usage" | "response.usage.updated"
                )
                .then_some(v)
            }) {
                let mut parsed = parse_tokens(u, claude);
                if !claude {
                    if let Some(raw) = count(u, &["input_tokens", "prompt_tokens"]) {
                        if !terminal || raw != 0 || self.inclusive_input.is_none_or(|v| v == 0) {
                            self.inclusive_input = Some(raw);
                        }
                    }
                }
                if terminal {
                    // Terminal envelopes sometimes contain zero placeholders for
                    // counters already reported by earlier cumulative events.
                    macro_rules! preserve { ($($field:ident),*) => {$(
                        if parsed.$field == Some(0) && self.tokens.$field.is_some_and(|v| v > 0) {
                            parsed.$field = None;
                        }
                    )*}; }
                    if count(u, &["input_tokens", "prompt_tokens"]) == Some(0)
                        && self.tokens.input.is_some_and(|v| v > 0)
                    {
                        parsed.input = None;
                    }
                    preserve!(
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
                self.tokens.merge(&parsed);
                // Some providers report cache details after the inclusive input
                // counter. Normalize against that same counter after merging.
                if !claude {
                    if let Some(raw) = self.inclusive_input {
                        self.tokens.input = Some(
                            raw.saturating_sub(self.tokens.cache_read.unwrap_or(0))
                                .saturating_sub(self.tokens.cache_write.unwrap_or(0)),
                        );
                    }
                }
            }
            if let Some(model) = model_id(v["model"].as_str()) {
                self.model = Some(model);
            }
            if let Some(tier) = v["service_tier"].as_str().filter(|s| {
                matches!(
                    *s,
                    "default" | "flex" | "priority" | "standard" | "batch" | "ultrafast"
                )
            }) {
                self.service_tier = Some(tier.into());
            }
            if v.get("output")
                .and_then(Value::as_array)
                .is_some_and(|items| {
                    items.iter().any(|i| {
                        matches!(
                            i["type"].as_str(),
                            Some("compaction" | "context_compaction")
                        )
                    })
                })
                || matches!(
                    v["item"]["type"].as_str(),
                    Some("compaction" | "context_compaction")
                )
            {
                self.compaction_kind = Some("server".into());
            }
            for key in ["response", "message", "data", "result"] {
                if let Some(inner) = v.get(key).filter(|v| v.is_object()) {
                    envelopes.push(inner);
                }
            }
        }
        if let Some(id) = response_id(outer) {
            self.response_id = Some(safe_id(id));
        }
        let nonempty = |v: &Value| v.as_str().is_some_and(|s| !s.is_empty());
        let generated = match event {
            "response.output_text.delta"
            | "response.reasoning_text.delta"
            | "response.reasoning_summary_text.delta"
            | "response.function_call_arguments.delta" => nonempty(&outer["delta"]),
            "content_block_delta" => ["text", "thinking", "partial_json"]
                .iter()
                .any(|key| nonempty(&outer["delta"][key])),
            "error" | "response.failed" => false,
            _ => outer["choices"].as_array().is_some_and(|choices| {
                choices.iter().any(|choice| {
                    let delta = &choice["delta"];
                    ["content", "reasoning_content", "reasoning"]
                        .iter()
                        .any(|key| nonempty(&delta[key]))
                        || nonempty(&delta["function_call"]["arguments"])
                        || delta["tool_calls"].as_array().is_some_and(|tools| {
                            tools
                                .iter()
                                .any(|tool| nonempty(&tool["function"]["arguments"]))
                        })
                })
            }),
        } && outer.get("error").is_none_or(Value::is_null);
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
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    #[default]
    Model,
    WebSearch,
    Compaction,
}
impl Operation {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Model => "model",
            Self::WebSearch => "web_search",
            Self::Compaction => "compaction",
        }
    }
    pub fn for_path(path: &str) -> Self {
        if matches!(
            path.trim_end_matches('/'),
            "/responses/compact" | "/v1/responses/compact"
        ) {
            Self::Compaction
        } else if matches!(
            path.trim_end_matches('/'),
            "/v1/alpha/search" | "/alpha/search"
        ) {
            Self::WebSearch
        } else {
            Self::Model
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Attempt {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inclusive_input_tokens: Option<u64>,
    #[serde(default)]
    pub compaction_kind: Option<String>,
    #[serde(default)]
    pub usage_status: String,
    #[serde(default)]
    pub usage_sources: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub pricing_basis: Option<String>,
    #[serde(default)]
    pub mapping_revision: Option<String>,
    #[serde(default = "one_attempt")]
    pub repeat_count: u64,
    #[serde(default)]
    pub compacted_unpriced: Option<u64>,
    #[serde(default)]
    pub operation: Operation,
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
fn one_attempt() -> u64 {
    1
}
impl Attempt {
    pub fn unpriced_count(&self) -> u64 {
        self.compacted_unpriced
            .unwrap_or(u64::from(self.price.is_none()))
    }
    pub fn same_dimensions(&self, other: &Self) -> bool {
        self.provider == other.provider
            && self.operation == other.operation
            && self.requested_model == other.requested_model
            && self.response_model == other.response_model
            && self.pricing_model == other.pricing_model
            && self.mapping_revision == other.mapping_revision
            && self.cost_multiplier == other.cost_multiplier
            && self.service_tier == other.service_tier
            && match (&self.price, &other.price) {
                (None, None) => true,
                (Some(a), Some(b)) => {
                    a.version == b.version
                        && a.source == b.source
                        && a.model == b.model
                        && a.multiplier == b.multiplier
                        && a.rates == b.rates
                        && a.basis == b.basis
                }
                _ => false,
            }
    }
    /// Bound retry metadata independently of the request lifetime. Preserve all
    /// reported consumption; mixed price/provider dimensions remain explicitly unknown.
    pub fn compact(&mut self, next: Self) {
        let same = self.same_dimensions(&next);
        let unpriced = self.unpriced_count().saturating_add(next.unpriced_count());
        let cost = self
            .price
            .as_ref()
            .and_then(|p| decimal(&p.cost))
            .unwrap_or_default()
            + next
                .price
                .as_ref()
                .and_then(|p| decimal(&p.cost))
                .unwrap_or_default();
        self.tokens.add(&next.tokens);
        self.repeat_count = self.repeat_count.saturating_add(next.repeat_count);
        self.compacted_unpriced = Some(unpriced);
        self.duration_ms = self.duration_ms.saturating_add(next.duration_ms);
        if self.provider != next.provider {
            self.provider = None;
        }
        if self.pricing_model != next.pricing_model {
            self.pricing_model = None;
        }
        if self.response_model != next.response_model {
            self.response_model = None;
        }
        self.response_id = None;
        if same {
            if let Some(price) = &mut self.price {
                price.cost = cost.to_string();
            }
            return;
        }
        self.mapping_revision = None;
        self.pricing_basis = Some("compacted".into());
        self.price = if self.price.is_some() || next.price.is_some() {
            Some(PriceSnapshot {
                version: "retry-summary-v1".into(),
                source: "compacted".into(),
                model: self.pricing_model.clone().unwrap_or_default(),
                multiplier: "1".into(),
                rates: Default::default(),
                cost: cost.to_string(),
                basis: Some(
                    serde_json::json!({"attempts": self.repeat_count, "unpriced": unpriced}),
                ),
            })
        } else {
            None
        };
    }
}
impl Default for Attempt {
    fn default() -> Self {
        Self {
            compaction_kind: None,
            usage_status: String::new(),
            usage_sources: Default::default(),
            inclusive_input_tokens: None,
            pricing_basis: None,
            mapping_revision: None,
            repeat_count: 1,
            compacted_unpriced: None,
            operation: Operation::Model,
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
impl Attempt {
    pub fn grouping_model(&self) -> Option<&str> {
        if self.operation == Operation::WebSearch {
            Some("web_search")
        } else {
            self.pricing_model.as_deref()
        }
    }
    pub fn search_price(&self) -> Option<PriceSnapshot> {
        if self.operation != Operation::WebSearch
            || !matches!(self.outcome.as_str(), "success" | "limited")
            || !self.status.is_some_and(|s| (200..300).contains(&s))
        {
            return None;
        }
        let multiplier = decimal(&self.cost_multiplier)?;
        Some(PriceSnapshot {
            version: "web-search-v1".into(),
            source: "endpoint".into(),
            model: "web_search".into(),
            multiplier: self.cost_multiplier.clone(),
            rates: std::collections::BTreeMap::from([("cost_per_request".into(), "0.01".into())]),
            cost: (Decimal::new(1, 2) * multiplier).normalize().to_string(),
            basis: Some(
                serde_json::json!({"operation":"web_search","unit":"request","quantity":1,"cost_per_request":"0.01"}),
            ),
        })
    }
    pub fn calculate_price(&self, pricing: &super::pricing::Pricing) -> Option<PriceSnapshot> {
        if self.operation == Operation::WebSearch {
            self.search_price()
        } else {
            pricing
                .quote(self.pricing_model.as_deref(), &self.cost_multiplier)
                .calculate(&self.tokens, self.service_tier.as_deref())
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
    /// The gateway's unmodified metadata when a matching local session supplies
    /// missing usage. Kept separately so rebuilding that source can undo the join.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gateway_reported: Option<Attempt>,
}
impl Record {
    /// Only real gateway generations supply latency samples, including merged records.
    pub fn measured_first_token_ms(&self) -> Option<u64> {
        let a = self.final_attempt()?;
        (self.source == "proxy" && a.stream && a.operation == Operation::Model)
            .then_some(a.first_token_ms)
            .flatten()
    }

    pub fn gateway_only(&mut self) {
        if let (Some(original), Some(last)) =
            (self.gateway_reported.take(), self.attempts.last_mut())
        {
            *last = original;
        }
    }

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
        if self.attempts.is_empty() || self.attempts.iter().any(|a| a.unpriced_count() > 0) {
            return None;
        }
        self.attempts.iter().try_fold(Decimal::ZERO, |sum, a| {
            sum.checked_add(decimal(&a.price.as_ref()?.cost)?)
        })
    }
    pub fn signature(&self) -> Option<String> {
        let a = self.final_attempt()?;
        if a.operation == Operation::WebSearch || a.tokens.total().is_none_or(|n| n == 0) {
            return None;
        }
        let model = a
            .response_model
            .as_ref()
            .or(a.requested_model.as_ref())
            .or(a.pricing_model.as_ref())?;
        let input = a.tokens.input?;
        let output = a.tokens.output?;
        // Codex sessions do not report cache writes; retain that uncertainty in
        // the record, but do not turn it into a different correlation key.
        let write = if self.client == "codex" {
            None
        } else {
            a.tokens.cache_write
        };
        Some(safe_id(&format!(
            "{}:{model}:{input}:{output}:{:?}:{write:?}",
            self.client, a.tokens.cache_read
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
    pub source: Option<String>,
    pub start: Option<i64>,
    pub end: Option<i64>,
    pub client: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub status: Option<String>,
    pub page: u32,
    pub sort: Option<String>,
    pub operation: Option<String>,
}
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Totals {
    pub requests: u64,
    #[serde(default)]
    pub attempts: u64,
    pub success: u64,
    pub status_known: u64,
    pub sessions: u64,
    pub tokens: Tokens,
    pub cost: String,
    pub unpriced: u64,
    pub duration_ms: u64,
    pub measured_outputs: u64,
    pub generation_ms: u64,
    #[serde(default)]
    pub first_token_sum_ms: u64,
    #[serde(default)]
    pub first_token_samples: u64,
    #[serde(default)]
    pub cache_read_eligible: u64,
    #[serde(default)]
    pub cache_input_eligible: u64,
}
impl Totals {
    pub fn add_record(&mut self, r: &Record) {
        self.requests += 1;
        self.attempts += r.attempts.iter().map(|a| a.repeat_count).sum::<u64>();
        self.tokens.add(&r.tokens());
        for attempt in &r.attempts {
            if let Some((read, input)) = attempt.tokens.cache_sample() {
                self.cache_read_eligible = self.cache_read_eligible.saturating_add(read);
                self.cache_input_eligible = self.cache_input_eligible.saturating_add(input);
            }
        }
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
            if let Some(first) = r.measured_first_token_ms() {
                self.first_token_sum_ms = self.first_token_sum_ms.saturating_add(first);
                self.first_token_samples += 1;
            }
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
        self.attempts += o.attempts;
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
        self.first_token_sum_ms = self.first_token_sum_ms.saturating_add(o.first_token_sum_ms);
        self.first_token_samples += o.first_token_samples;
        self.cache_read_eligible = self
            .cache_read_eligible
            .saturating_add(o.cache_read_eligible);
        self.cache_input_eligible = self
            .cache_input_eligible
            .saturating_add(o.cache_input_eligible);
    }
}
