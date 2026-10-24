use crate::gateway::ClientId;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

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
    pub kind: Kind,
    #[serde(default = "one")]
    pub concurrency: usize,
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
}
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Detail {
    pub job: Job,
    pub output: Option<Output>,
}
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ProviderDetection {
    pub enabled: Option<bool>,
    pub model: Option<String>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Detection {
    pub enabled: bool,
    pub mode: String,
    pub interval_minutes: u32,
    pub min_interval_minutes: u32,
    pub model: String,
    pub providers: BTreeMap<String, ProviderDetection>,
}
impl Default for Detection {
    fn default() -> Self {
        Self {
            enabled: false,
            mode: "smart".into(),
            interval_minutes: 15,
            min_interval_minutes: 5,
            model: super::DEFAULT_DETECTION_MODEL.into(),
            providers: BTreeMap::new(),
        }
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Degradation {
    pub provider_id: String,
    pub job_id: String,
    pub detected_at: i64,
    pub requested_model: String,
    pub predicted_model: String,
    pub probability: f64,
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
#[derive(Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Persisted {
    pub schema: u32,
    pub jobs: Vec<Job>,
    pub detection: Detection,
    pub degraded: BTreeMap<String, Degradation>,
    pub history: Vec<Job>,
    pub bank: SavedBank,
}
impl Default for Persisted {
    fn default() -> Self {
        Self {
            schema: 1,
            jobs: vec![],
            detection: Detection::default(),
            degraded: BTreeMap::new(),
            history: vec![],
            bank: SavedBank::default(),
        }
    }
}
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DetectionStatus {
    pub provider_id: String,
    pub status: String,
    pub model: String,
    pub recent_requests: usize,
    pub interval_minutes: Option<u32>,
    pub next_at: Option<i64>,
    pub cooldown_until: Option<i64>,
    pub error: Option<String>,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct State {
    pub revision: String,
    pub jobs: Vec<Job>,
    pub detection: Detection,
    pub degraded: BTreeMap<String, Degradation>,
    pub detection_status: Vec<DetectionStatus>,
    pub history: Vec<Job>,
    pub bank: Value,
    pub error: Option<String>,
}
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Model {
    pub id: String,
    pub name: String,
}
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Models {
    pub models: Vec<Model>,
    pub default_model: String,
    pub stale: bool,
    pub error: Option<String>,
}
