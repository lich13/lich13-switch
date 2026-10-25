use crate::gateway::ClientId;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Engine {
    #[default]
    ModelTrace,
    LmDetector,
}
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PlatformSettings {
    pub engine: Engine,
    pub model_trace_samples: usize,
    pub lm_detector_samples: usize,
}
impl Default for PlatformSettings {
    fn default() -> Self {
        Self {
            engine: Engine::ModelTrace,
            model_trace_samples: 3,
            lm_detector_samples: 1,
        }
    }
}
impl PlatformSettings {
    pub fn for_client(client: ClientId) -> Self {
        Self {
            engine: if client == ClientId::Codex {
                Engine::ModelTrace
            } else {
                Engine::LmDetector
            },
            ..Self::default()
        }
    }
    pub fn samples(&self) -> usize {
        match self.engine {
            Engine::ModelTrace => self.model_trace_samples,
            Engine::LmDetector => self.lm_detector_samples,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Kind {
    #[default]
    Connection,
    Fingerprint,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
// Retained only to read historical job summaries. New tests always use Text.
pub enum Mode {
    #[default]
    Text,
    Compact,
    Image,
    Video,
    Search,
    Tts,
    Stt,
    Realtime,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Request {
    pub client_id: ClientId,
    pub provider_id: String,
    pub expected_revision: String,
    pub model: String,
    #[serde(default)]
    pub endpoint: Option<String>,
    #[serde(default)]
    pub kind: Kind,
    #[serde(default = "one")]
    pub concurrency: usize,
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub tokenizer: bool,
}
fn one() -> usize {
    1
}
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Group {
    pub index: usize,
    pub expected: usize,
    pub status: String,
    pub attempts: usize,
    pub elapsed_ms: u64,
    pub returned_model: Option<String>,
    pub report: Option<Value>,
    pub diagnostics: Value,
    pub error: Option<String>,
    pub retryable: bool,
    pub uncertain: bool,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Job {
    pub id: String,
    pub client_id: ClientId,
    pub provider_id: String,
    pub provider_name: String,
    pub provider_version: String,
    #[serde(default)]
    pub endpoint: Option<String>,
    pub kind: Kind,
    pub mode: Mode,
    pub requested_model: String,
    pub forwarded_model: String,
    pub returned_model: Option<String>,
    pub status: String,
    pub created_at: i64,
    pub started_at: Option<i64>,
    pub completed_at: Option<i64>,
    pub elapsed_ms: u64,
    pub first_byte_ms: Option<u64>,
    pub error: Option<String>,
    pub report: Option<Value>,
    pub groups: Vec<Group>,
    pub automatic: bool,
    pub batch_id: Option<String>,
    pub can_retry: bool,
    pub completion_reason: Option<String>,
    #[serde(default)]
    pub engine: Engine,
    #[serde(default = "three")]
    pub sample_count: usize,
    #[serde(default)]
    pub resource_version: Value,
    #[serde(default)]
    pub tokenizer: bool,
    #[serde(default)]
    pub tokenizer_result: Option<Value>,
}
fn three() -> usize {
    3
}
impl Job {
    pub fn terminal(&self) -> bool {
        !matches!(self.status.as_str(), "queued" | "running")
    }
}
#[derive(Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Output {
    pub text: String,
    pub groups: BTreeMap<usize, String>,
    pub diagnostics: Value,
    pub error: Option<TestError>,
    pub group_errors: BTreeMap<usize, TestError>,
}
#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TestError {
    pub http_status: Option<u16>,
    pub message: String,
    pub body: String,
    pub truncated: bool,
}
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Detail {
    pub job: Job,
    pub output: Option<Output>,
}
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Detection {
    pub enabled: bool,
    pub mode: String,
    pub interval_minutes: u32,
    pub min_interval_minutes: u32,
    pub model: String,
    pub endpoint: Option<String>,
}
impl Default for Detection {
    fn default() -> Self {
        Self {
            enabled: false,
            mode: "smart".into(),
            interval_minutes: 15,
            min_interval_minutes: 5,
            model: super::DEFAULT_DETECTION_MODEL.into(),
            endpoint: None,
        }
    }
}
impl Detection {
    pub fn for_client(client: ClientId) -> Self {
        Self {
            model: if client == ClientId::Codex {
                super::DEFAULT_DETECTION_MODEL.into()
            } else {
                String::new()
            },
            ..Self::default()
        }
    }
}
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct DetectionTiming {
    pub anchor_at: Option<i64>,
    pub last_started_at: Option<i64>,
    pub paused: bool,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DegradationBasis {
    Aggregate,
    SampleFamily,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Degradation {
    pub provider_id: String,
    pub job_id: String,
    pub detected_at: i64,
    pub requested_model: String,
    pub predicted_model: String,
    pub probability: Option<f64>,
    #[serde(default)]
    pub engine: Engine,
    #[serde(default)]
    pub basis: Option<DegradationBasis>,
    #[serde(default)]
    pub group_indexes: Vec<usize>,
    pub queue_removed: bool,
    pub error: Option<String>,
}
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SavedBank {
    pub data: Option<Value>,
    pub version: Option<Value>,
    pub checked_at: Option<i64>,
    pub error: Option<String>,
    pub provenance: Option<Value>,
}
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SavedLmBank {
    pub checked_at: Option<i64>,
    pub updated_at: Option<i64>,
    pub result: Option<String>,
    pub error: Option<String>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Persisted {
    pub schema: u32,
    pub jobs: Vec<Job>,
    pub connection_prompt: String,
    pub platforms: BTreeMap<String, PlatformSettings>,
    pub detection: BTreeMap<String, Detection>,
    pub detection_timing: BTreeMap<String, DetectionTiming>,
    pub models: BTreeMap<String, Models>,
    pub degraded: BTreeMap<String, Degradation>,
    pub history: Vec<Job>,
    pub bank: SavedBank,
    pub lm_bank: SavedLmBank,
}
impl Default for Persisted {
    fn default() -> Self {
        Self {
            schema: 3,
            jobs: vec![],
            connection_prompt: super::DEFAULT_CONNECTION_PROMPT.into(),
            platforms: BTreeMap::new(),
            detection: BTreeMap::new(),
            detection_timing: BTreeMap::new(),
            models: BTreeMap::new(),
            degraded: BTreeMap::new(),
            history: vec![],
            bank: SavedBank::default(),
            lm_bank: SavedLmBank::default(),
        }
    }
}
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DetectionStatus {
    pub provider_id: String,
    pub status: String,
    pub model: String,
    pub recent_requests: Option<u64>,
    pub last_started_at: Option<i64>,
    pub interval_minutes: Option<u32>,
    pub next_at: Option<i64>,
    pub cooldown_until: Option<i64>,
    pub error: Option<String>,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct State {
    pub revision: String,
    pub connection_prompt: String,
    pub platform: PlatformSettings,
    pub banks: Value,
    pub jobs: Vec<Job>,
    pub detection: Detection,
    pub degraded: BTreeMap<String, Degradation>,
    pub detection_status: Vec<DetectionStatus>,
    pub history: Vec<Job>,
    pub bank: Value,
    pub error: Option<String>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Model {
    pub id: String,
    pub name: String,
}
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Models {
    pub models: Vec<Model>,
    pub default_model: String,
    pub fetched_at: Option<i64>,
    pub error: Option<String>,
    #[serde(skip_deserializing, skip_serializing_if = "Option::is_none")]
    pub error_detail: Option<TestError>,
}
