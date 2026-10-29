use crate::errors::code;
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
    AppError::new(code::USAGE, message)
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
    #[serde(default)]
    pub inclusive_input: Option<u64>,
    pub input: Option<u64>,
    pub output: Option<u64>,
    /// Reported subset of output, never added to total consumption.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<u64>,
    pub cache_read: Option<u64>,
    pub cache_write: Option<u64>,
    pub cache_write_5m: Option<u64>,
    pub cache_write_1h: Option<u64>,
    pub image_input: Option<u64>,
    /// Reported subset of cache_read, separate from fresh image input.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_cache_read: Option<u64>,
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
            inclusive_input,
            input,
            output,
            reasoning,
            cache_read,
            cache_write,
            cache_write_5m,
            cache_write_1h,
            image_input,
            image_cache_read,
            image_output,
            audio_input,
            audio_output
        );
    }
    pub fn fill_missing(&mut self, other: &Self) -> Vec<String> {
        let mut fields = Vec::new();
        macro_rules! fill { ($($f:ident),*) => {$(if self.$f.is_none() && other.$f.is_some() { self.$f=other.$f; fields.push(stringify!($f).to_owned()); })*}; }
        fill!(
            inclusive_input,
            input,
            output,
            reasoning,
            cache_read,
            cache_write,
            cache_write_5m,
            cache_write_1h,
            image_input,
            image_cache_read,
            image_output,
            audio_input,
            audio_output
        );
        fields
    }
    pub fn cache_sample(&self) -> Option<(u64, u64)> {
        let read = self.cache_read?;
        let input = self.inclusive_input.or_else(|| {
            Some(
                self.input?
                    .saturating_add(read)
                    .saturating_add(self.cache_write?),
            )
        })?;
        (read <= input).then_some((read, input))
    }
    pub fn add(&mut self, other: &Self) {
        macro_rules! add { ($($f:ident),*) => {$(self.$f=match(self.$f,other.$f){(None,None)=>None,(a,b)=>Some(a.unwrap_or(0).saturating_add(b.unwrap_or(0)))};)*}; }
        add!(
            inclusive_input,
            input,
            output,
            reasoning,
            cache_read,
            cache_write,
            cache_write_5m,
            cache_write_1h,
            image_input,
            image_cache_read,
            image_output,
            audio_input,
            audio_output
        );
    }
    pub fn delta(&self, old: &Self) -> Self {
        let mut next = self.clone();
        macro_rules! delta { ($($f:ident),*) => {$(next.$f=self.$f.map(|v|v.saturating_sub(old.$f.unwrap_or(0)));)*}; }
        delta!(
            inclusive_input,
            input,
            output,
            reasoning,
            cache_read,
            cache_write,
            cache_write_5m,
            cache_write_1h,
            image_input,
            image_cache_read,
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
    parse_usage(usage, claude, None).0
}

fn parse_usage(
    usage: &Value,
    claude: bool,
    previous_raw: Option<u64>,
) -> (Tokens, bool, Vec<String>) {
    let details = usage
        .get("input_tokens_details")
        .or_else(|| usage.get("prompt_tokens_details"))
        .unwrap_or(&Value::Null);
    let cache = count(
        usage,
        &[
            "cache_read_input_tokens",
            "cached_input_tokens",
            "prompt_cache_hit_tokens",
            "cached_tokens",
        ],
    )
    .or_else(|| count(details, &["cached_tokens"]));
    let reported_write = count(
        usage,
        &["cache_creation_input_tokens", "cache_creation_tokens"],
    )
    .or_else(|| count(details, &["cache_write_tokens"]));
    let raw = count(usage, &["input_tokens", "prompt_tokens"]);
    let create = &usage["cache_creation"];
    let short = count(create, &["ephemeral_5m_input_tokens"]);
    let long = count(create, &["ephemeral_1h_input_tokens"]);
    let subtotal = short
        .or(long)
        .map(|_| short.unwrap_or(0) + long.unwrap_or(0));
    let write = reported_write.or(subtotal);
    let mut reasons = Vec::new();
    if reported_write
        .zip(subtotal)
        .is_some_and(|(total, parts)| total < parts)
    {
        reasons.push("cache_write_total_below_ttl_details".into());
    }
    if let (Some(hit), Some(miss), Some(prompt)) = (
        count(usage, &["prompt_cache_hit_tokens"]),
        count(usage, &["prompt_cache_miss_tokens"]),
        count(usage, &["prompt_tokens"]),
    ) {
        if hit.checked_add(miss) != Some(prompt) {
            reasons.push("prompt_cache_total_mismatch".into());
        }
    }
    let anthropic_cache = usage.get("cache_read_input_tokens").is_some()
        || usage.get("cache_creation_input_tokens").is_some()
        || usage.get("cache_creation").is_some();
    let exclusive = claude
        || (anthropic_cache
            && raw
                .or(previous_raw)
                .is_some_and(|n| n < cache.unwrap_or(0) + write.unwrap_or(0)));
    let output_details = usage
        .get("output_tokens_details")
        .or_else(|| usage.get("completion_tokens_details"))
        .unwrap_or(&Value::Null);
    let output = count(usage, &["output_tokens", "completion_tokens"]);
    let reasoning = count(usage, &["reasoning_tokens"])
        .or_else(|| count(output_details, &["reasoning_tokens"]));
    let image_cache_read = details
        .get("cached_tokens_details")
        .and_then(|cached| count(cached, &["image_tokens"]))
        .or_else(|| count(details, &["cached_image_tokens"]));
    if reasoning
        .zip(output)
        .is_some_and(|(reasoning, output)| reasoning > output)
    {
        reasons.push("reasoning_exceeds_output".into());
    }
    (
        Tokens {
            inclusive_input: (!exclusive).then_some(raw).flatten(),
            input: raw.map(|v| {
                if exclusive {
                    v
                } else {
                    v.saturating_sub(cache.unwrap_or(0))
                        .saturating_sub(write.unwrap_or(0))
                }
            }),
            output,
            reasoning,
            cache_read: cache,
            cache_write: write,
            cache_write_5m: short,
            cache_write_1h: long,
            image_input: count(details, &["image_tokens"]).map(|n| {
                if exclusive {
                    n
                } else {
                    n.saturating_sub(image_cache_read.unwrap_or(0))
                }
            }),
            image_cache_read,
            image_output: count(output_details, &["image_tokens"]),
            audio_input: count(details, &["audio_tokens"]),
            audio_output: count(output_details, &["audio_tokens"]),
        },
        exclusive,
        reasons,
    )
}

#[derive(Clone, Default, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Meter {
    pub tokens: Tokens,
    #[serde(skip)]
    pub inclusive_input: Option<u64>,
    #[serde(skip)]
    pub ended_early: bool,
    #[serde(skip)]
    pub(crate) anthropic: Option<bool>,
    pub response_id: Option<String>,
    pub model: Option<String>,
    pub service_tier: Option<String>,
    pub first_token_ms: Option<u64>,
    pub parse_incomplete: bool,
    pub compaction_kind: Option<String>,
    pub requested_speed: Option<String>,
    pub response_speed: Option<String>,
    pub raw_input_tokens: Option<u64>,
    pub input_basis: Option<String>,
    pub output_reported: Option<bool>,
    pub parse_reasons: Vec<String>,
    #[serde(skip)]
    pub(crate) reported_cache_write: Option<u64>,
}

pub fn speed(value: &Value) -> Option<String> {
    value
        .as_str()
        .filter(|s| matches!(*s, "fast" | "standard"))
        .map(str::to_owned)
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
        let explicit = node
            .get("response_id")
            .or_else(|| node.get("responseId"))
            .and_then(Value::as_str);
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
        if let Some(id) = response_id(outer).map(safe_id) {
            if self.response_id.as_ref().is_some_and(|old| *old != id) {
                *self = Self {
                    anthropic: self.anthropic,
                    requested_speed: self.requested_speed.clone(),
                    ..Self::default()
                };
            }
        }
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
            if event.starts_with("message_") || v["type"] == "message" {
                self.anthropic = Some(true);
            } else if event.starts_with("response.")
                || v["object"] == "response"
                || v["object"]
                    .as_str()
                    .is_some_and(|v| v.starts_with("chat.completion"))
                || v.get("choices").is_some()
            {
                self.anthropic = Some(false);
            }
            let claude = self.anthropic.unwrap_or_else(|| {
                v["usage"].get("prompt_tokens").is_none()
                    && (v["usage"].get("cache_creation_input_tokens").is_some()
                        || v["usage"].get("cache_read_input_tokens").is_some())
            });
            if let Some(u) = v.get("usage").filter(|u| u.is_object()).or_else(|| {
                matches!(
                    event,
                    "usage" | "usage.updated" | "response.usage" | "response.usage.updated"
                )
                .then_some(v)
            }) {
                let (mut parsed, exclusive, reasons) =
                    parse_usage(u, claude, self.raw_input_tokens);
                for reason in reasons {
                    if !self.parse_reasons.contains(&reason) {
                        self.parse_reasons.push(reason);
                    }
                }
                self.parse_incomplete |= !self.parse_reasons.is_empty();
                let raw = count(u, &["input_tokens", "prompt_tokens"]);
                let reported_write =
                    count(u, &["cache_creation_input_tokens", "cache_creation_tokens"])
                        .or_else(|| count(&u["input_tokens_details"], &["cache_write_tokens"]))
                        .or_else(|| count(&u["prompt_tokens_details"], &["cache_write_tokens"]));
                if reported_write.is_some_and(|n| n > 0) || self.reported_cache_write.is_none() {
                    self.reported_cache_write = reported_write;
                }
                let zero_input_placeholder =
                    raw == Some(0) && self.raw_input_tokens.is_some_and(|v| v > 0);
                if !zero_input_placeholder && raw.is_some() {
                    self.raw_input_tokens = raw;
                }
                // A later relay cache event can establish an exclusive input
                // basis. Keep that basis for the rest of this response.
                let exclusive = exclusive || self.input_basis.as_deref() == Some("exclusive");
                self.input_basis = Some(if exclusive { "exclusive" } else { "inclusive" }.into());
                self.inclusive_input = (!exclusive).then_some(self.raw_input_tokens).flatten();
                if exclusive {
                    parsed.inclusive_input = None;
                    self.tokens.inclusive_input = None;
                }
                if zero_input_placeholder {
                    parsed.input = None;
                    parsed.inclusive_input = None;
                }
                // All usage envelopes are cumulative within this response.
                // Preserve reported positives against subsequent placeholder zeroes.
                macro_rules! preserve { ($($field:ident),*) => {$(
                    if parsed.$field == Some(0) && self.tokens.$field.is_some_and(|v| v > 0) { parsed.$field = None; }
                )*}; }
                preserve!(
                    inclusive_input,
                    input,
                    output,
                    reasoning,
                    cache_read,
                    cache_write,
                    cache_write_5m,
                    cache_write_1h,
                    image_input,
                    image_cache_read,
                    image_output,
                    audio_input,
                    audio_output
                );
                if parsed.output.is_some() {
                    self.output_reported = Some(
                        self.output_reported == Some(true)
                            || event != "message_start"
                            || parsed.output != Some(0),
                    );
                }
                if let Some(speed) = speed(&u["speed"]) {
                    self.response_speed = Some(speed);
                }
                self.tokens.merge(&parsed);
                if self.tokens.cache_write_5m.is_some() || self.tokens.cache_write_1h.is_some() {
                    let parts = self
                        .tokens
                        .cache_write_5m
                        .unwrap_or(0)
                        .saturating_add(self.tokens.cache_write_1h.unwrap_or(0));
                    if self.reported_cache_write.is_some_and(|n| n < parts) {
                        self.parse_incomplete = true;
                        if !self
                            .parse_reasons
                            .iter()
                            .any(|r| r == "cache_write_total_below_ttl_details")
                        {
                            self.parse_reasons
                                .push("cache_write_total_below_ttl_details".into());
                        }
                    }
                    self.tokens.cache_write = self.reported_cache_write.or(Some(parts));
                }
                // Some providers report cache details after the inclusive input
                // counter. Normalize against that same counter after merging.
                if !exclusive {
                    if let Some(raw) = self.inclusive_input {
                        self.tokens.input = Some(
                            raw.saturating_sub(self.tokens.cache_read.unwrap_or(0))
                                .saturating_sub(self.tokens.cache_write.unwrap_or(0)),
                        );
                    }
                } else if self.raw_input_tokens.is_some() {
                    self.tokens.input = self.raw_input_tokens;
                }
            }
            if let Some(u) = v.get("usageMetadata").filter(|u| u.is_object()) {
                let input = count(u, &["promptTokenCount"]);
                let cache = count(u, &["cachedContentTokenCount"]);
                let answer = count(u, &["candidatesTokenCount"]);
                let thoughts = count(u, &["thoughtsTokenCount"]);
                let parsed = Tokens {
                    inclusive_input: input,
                    input: input.map(|n| n.saturating_sub(cache.unwrap_or(0))),
                    cache_read: cache,
                    output: answer.map(|n| n.saturating_add(thoughts.unwrap_or(0))),
                    reasoning: thoughts,
                    ..Default::default()
                };
                self.raw_input_tokens = input.or(self.raw_input_tokens);
                self.inclusive_input = input.or(self.inclusive_input);
                self.input_basis = Some("inclusive".into());
                self.output_reported = answer.map(|_| true).or(self.output_reported);
                self.tokens.merge(&parsed);
                if cache.zip(input).is_some_and(|(c, i)| c > i) {
                    self.parse_incomplete = true;
                }
            }
            if let Some(model) =
                model_id(v["model"].as_str().or_else(|| v["modelVersion"].as_str()))
            {
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
        if terminal && self.tokens.output.is_some() {
            self.output_reported = Some(true);
        }
        if let Some(id) = response_id(outer) {
            self.response_id = Some(safe_id(id));
        }
        let nonempty = |v: &Value| v.as_str().is_some_and(|s| !s.is_empty());
        let generated = (outer
            .get("candidates")
            .and_then(Value::as_array)
            .is_some_and(|candidates| {
                candidates.iter().any(|c| {
                    c.pointer("/content/parts")
                        .and_then(Value::as_array)
                        .is_some_and(|parts| {
                            parts
                                .iter()
                                .any(|p| nonempty(&p["text"]) || p.get("functionCall").is_some())
                        })
                })
            })
            || match event {
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
            })
            && outer.get("error").is_none_or(Value::is_null);
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
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EndCause {
    Completed,
    ApplicationError,
    UpstreamIncomplete,
    UpstreamRejected,
    ClientCancelled,
    ClientDisconnected,
    DownstreamWriteTimeout,
    DownstreamTransportError,
    GatewayStopped,
    UpstreamDisconnected,
    ConnectFailed,
    FirstByteTimeout,
    StreamTimeout,
    ParseIncomplete,
    Unsettled,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EndMetadata {
    pub cause: EndCause,
    pub terminal_received: bool,
    pub response_started: bool,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RoutingDecision {
    ThreadUnidentified,
    NewThread,
    ExistingThread,
    ModelDetourOwnerPreserved,
    AwaitingCompaction,
    CompactionWindowMismatch,
    CursorPinned,
    NoEligibleReceiver,
    ReceiverCapacityFull,
    BoundaryInFlight,
    CompactionWindowMissing,
    BoundaryNotObserved,
    BoundaryConsumed,
    CompactionHandoff,
    Auxiliary,
    OrdinaryQueue,
    FirstTurnUnconfirmed,
    NewThreadCapacityFull,
    NewThreadModelDenied,
    NewThreadModelCooling,
    NewThreadProviderUnavailable,
    NewThreadQueueWaiting,
    #[serde(other)]
    Unknown,
}
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RoutingInput {
    FirstTurn,
    History,
    ResponseCursor,
    Compaction,
    Warmup,
    MissingInput,
    UnknownStructure,
    #[default]
    #[serde(other)]
    MetadataUnavailable,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RoutingRestriction {
    ModelMismatch,
    ModelCooling,
    ProviderUnavailable,
    CapacityFull,
    QueueWaiting,
    #[serde(other)]
    Unknown,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ThreadIdentity {
    Reliable,
    LegacySession,
    Missing,
    Conflict,
    #[serde(other)]
    Unknown,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BoundaryState {
    NotObserved,
    Fresh,
    Claimed,
    Consumed,
    Mismatch,
    #[serde(other)]
    Unknown,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RoutingEvidence {
    pub input: RoutingInput,
    pub has_tool_declarations: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub restriction: Option<RoutingRestriction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<ThreadIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boundary: Option<BoundaryState>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TransferMetadata {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connection: Option<ConnectionEvidence>,
    pub received_at: Option<i64>,
    pub sent_at: i64,
    pub first_byte_at: Option<i64>,
    pub terminal_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal_event: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal_status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub incomplete_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream_client_request_hash: Option<String>,
    pub ended_at: i64,
    pub upstream_bytes: u64,
    pub downstream_bytes: u64,
    pub terminal_handed_off: bool,
    pub upstream_eof: bool,
    pub upstream_end: Option<String>,
    pub ws_close_code: Option<u16>,
    pub downstream_error: Option<String>,
    pub timeout_phase: Option<String>,
    pub timeout_seconds: Option<u64>,
    pub last_progress_ms: Option<u64>,
    pub client_request_hash: Option<String>,
    pub response_hash: Option<String>,
}
/// Cumulative connection-level counters include HTTP framing and other requests.
/// A successful socket write is not proof that the peer application consumed bytes.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionEvidence {
    pub id: String,
    pub request_sequence: u64,
    pub read_bytes: u64,
    pub socket_written_bytes: u64,
    pub events: Vec<TransportEvent>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransportEvent {
    pub sequence: u64,
    pub at: i64,
    pub kind: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Attempt {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_speed: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_speed: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_basis: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_reported: Option<bool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parse_reasons: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transfer: Option<TransferMetadata>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end: Option<EndMetadata>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_request_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routing_decision: Option<RoutingDecision>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routing_evidence: Option<RoutingEvidence>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routing_outcome: Option<crate::events::RoutingOutcome>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inclusive_input_tokens: Option<u64>,
    #[serde(default)]
    pub compaction_kind: Option<String>,
    #[serde(default)]
    pub usage_status: String,
    #[serde(default)]
    pub availability: std::collections::BTreeMap<String, String>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_model: Option<String>,
    pub response_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
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
    pub fn measured_duration_ms(&self) -> Option<u64> {
        let known = self.availability.get("duration").map_or_else(
            || {
                !self
                    .parse_reasons
                    .iter()
                    .any(|r| r == "attempt_time_unreported")
            },
            |s| s == "reported",
        );
        known.then_some(self.duration_ms)
    }
    pub fn measured_generation_ms(&self) -> Option<u64> {
        self.generation_ms
            .or_else(|| {
                self.measured_duration_ms()?
                    .checked_sub(self.first_token_ms?)
            })
            .filter(|n| *n > 0)
    }
    pub fn annotate_availability(&mut self, source: &str) {
        if self.tokens.inclusive_input.is_none() {
            self.tokens.inclusive_input = self.inclusive_input_tokens;
        }
        let missing = match self.usage_status.as_str() {
            "parse_incomplete" => "parse_incomplete",
            "ended_early" => "ended_early",
            "" => "historical_missing",
            _ => "upstream_unreported",
        };
        for (key, value) in [
            ("input", self.tokens.input),
            ("output", self.tokens.output),
            ("reasoning", self.tokens.reasoning),
            ("cache_read", self.tokens.cache_read),
            ("cache_write", self.tokens.cache_write),
        ] {
            let status = if self.usage_sources.contains_key(key) {
                "session_supplemented"
            } else if value.is_some() {
                "reported"
            } else {
                missing
            };
            self.availability.insert(key.into(), status.into());
        }
        self.availability.insert(
            "first_token".into(),
            if !matches!(source, "proxy" | "antigravity")
                || !self.stream
                || self.operation != Operation::Model
            {
                "not_applicable"
            } else if self.first_token_ms.is_some() {
                "reported"
            } else {
                missing
            }
            .into(),
        );
    }
    pub fn unpriced_count(&self) -> u64 {
        self.compacted_unpriced
            .unwrap_or(u64::from(self.price.is_none()))
    }
    pub fn same_dimensions(&self, other: &Self) -> bool {
        self.provider == other.provider
            && self.operation == other.operation
            && self.requested_model == other.requested_model
            && self.upstream_model == other.upstream_model
            && self.response_model == other.response_model
            && self.pricing_model == other.pricing_model
            && self.mapping_revision == other.mapping_revision
            && self.cost_multiplier == other.cost_multiplier
            && self.service_tier == other.service_tier
            && self.requested_speed == other.requested_speed
            && self.response_speed == other.response_speed
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
        // A compressed history entry has no single transport ending or request ID.
        self.end = None;
        self.upstream_request_hash = None;
        if self.routing_outcome != next.routing_outcome {
            self.routing_outcome = Some(crate::events::RoutingOutcome::Unknown);
        }
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
            requested_speed: None,
            response_speed: None,
            raw_input_tokens: None,
            input_basis: None,
            output_reported: None,
            parse_reasons: Vec::new(),
            transfer: None,
            end: None,
            thread_hash: None,
            upstream_request_hash: None,
            routing_decision: None,
            routing_evidence: None,
            routing_outcome: None,
            compaction_kind: None,
            usage_status: String::new(),
            availability: Default::default(),
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
            upstream_model: None,
            response_model: None,
            model_display_name: None,
            reasoning_effort: None,
            api_provider: None,
            generation_ms: None,
            error_code: None,
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
    pub fn price_tier(&self) -> Option<&str> {
        if self
            .response_speed
            .as_deref()
            .or(self.requested_speed.as_deref())
            == Some("fast")
        {
            Some("fast")
        } else {
            self.service_tier.as_deref()
        }
    }
    /// None means no interruption evidence; only actual gateway endings qualify.
    pub fn interruption(&self) -> Option<&'static str> {
        let end = self.end.as_ref()?;
        if !matches!(
            end.cause,
            EndCause::ClientCancelled
                | EndCause::ClientDisconnected
                | EndCause::DownstreamWriteTimeout
                | EndCause::DownstreamTransportError
                | EndCause::GatewayStopped
                | EndCause::UpstreamDisconnected
                | EndCause::StreamTimeout
                | EndCause::FirstByteTimeout
        ) {
            return None;
        }
        let output = self
            .output_reported
            .unwrap_or(end.terminal_received || self.tokens.output.is_some_and(|n| n > 0));
        Some(
            if self.tokens.input.is_some()
                && self.tokens.output.is_some()
                && output
                && self.usage_status != "parse_incomplete"
                && self.parse_reasons.is_empty()
            {
                "reported"
            } else {
                "partial"
            },
        )
    }
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
        if self.transport == "session_prompt" {
            return None;
        }
        if self.operation == Operation::WebSearch {
            self.search_price()
        } else {
            pricing
                .quote_at(
                    self.pricing_model.as_deref(),
                    &self.cost_multiplier,
                    self.started_at,
                )
                .calculate(&self.tokens, self.price_tier())
        }
    }
}
#[derive(Clone, Default, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Record {
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub detail_archived: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failover_evidence: Option<crate::gateway::failover::Evidence>,
    pub id: String,
    pub client: String,
    pub source: String,
    pub started_at: i64,
    #[serde(default)]
    pub finished_at: Option<i64>,
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
    /// Protocol completion and local finish are real observations; old records
    /// without either retain their original event time, never a guessed duration.
    pub fn event_time(&self) -> i64 {
        if self.source != "proxy" {
            return self.finished_at.unwrap_or(self.started_at);
        }
        self.final_attempt()
            .and_then(|a| a.transfer.as_ref())
            .and_then(|t| {
                t.terminal_at.filter(|v| *v >= self.started_at).or_else(|| {
                    (t.ended_at >= self.started_at && t.ended_at > 0).then_some(t.ended_at)
                })
            })
            .or(self.finished_at)
            .unwrap_or(self.started_at)
    }

    /// Only explicitly measured generations supply latency samples.
    pub fn measured_first_token_ms(&self) -> Option<u64> {
        let a = self.final_attempt()?;
        (matches!(self.source.as_str(), "proxy" | "antigravity")
            && a.stream
            && a.operation == Operation::Model)
            .then_some(a.first_token_ms)
            .flatten()
    }

    pub fn known_success(&self) -> Option<bool> {
        let a = self.final_attempt()?;
        if let Some(status) = a.status {
            return Some(((200..300).contains(&status) || status == 101) && self.completed);
        }
        if self.source == "antigravity" {
            return match a.outcome.as_str() {
                "success" => Some(self.completed),
                "failed" | "cancelled" | "rejected" => Some(false),
                _ => None,
            };
        }
        None
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
        self.signature_with_cache_read(self.final_attempt()?.tokens.cache_read)
    }
    /// Lookup aliases only: the stored signature and raw usage retain unknown
    /// versus reported zero. A nonzero cache count never acquires an alias.
    pub(super) fn correlation_signatures(&self) -> [Option<String>; 2] {
        let alternate = self
            .final_attempt()
            .and_then(|a| match a.tokens.cache_read {
                None => self.signature_with_cache_read(Some(0)),
                Some(0) => self.signature_with_cache_read(None),
                Some(_) => None,
            });
        [self.signature(), alternate]
    }
    fn signature_with_cache_read(&self, cache_read: Option<u64>) -> Option<String> {
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
            self.client, cache_read
        )))
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Settings {
    pub log_columns: std::collections::BTreeMap<String, bool>,
    pub log_number_mode: String,
    pub recording: bool,
    pub auto_sync: bool,
    pub refresh_seconds: u32,
    pub multiplier: String,
    pub pricing_model: String,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            log_columns: Default::default(),
            log_number_mode: "compact".into(),
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
            || !matches!(self.log_number_mode.as_str(), "compact" | "exact")
            || self.log_columns.keys().any(|key| {
                !matches!(
                    key.as_str(),
                    "provider"
                        | "model"
                        | "input"
                        | "output"
                        | "cacheRead"
                        | "cacheWrite"
                        | "reasoning"
                        | "hitRate"
                        | "cost"
                        | "firstToken"
                        | "duration"
                        | "speed"
                        | "status"
                )
            })
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
            self.duration_ms += a.measured_duration_ms().unwrap_or(0);
            if let Some(first) = r.measured_first_token_ms() {
                self.first_token_sum_ms = self.first_token_sum_ms.saturating_add(first);
                self.first_token_samples += 1;
            }
            if let Some(success) = r.known_success() {
                self.status_known += 1;
                if success {
                    self.success += 1;
                }
            }
            if r.source != "proxy" {
                self.sessions += 1;
            }
            if let (Some(generation), Some(out)) = (a.measured_generation_ms(), a.tokens.output) {
                if generation > 0 {
                    self.generation_ms += generation;
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
