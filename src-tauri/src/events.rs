//! Exception journal; complete upstream errors live in separately loaded attachments.
use crate::errors::code;
#[path = "event_details.rs"]
mod details;
use crate::{
    gateway::ClientId,
    storage::{self, AppError, Result},
};
pub(crate) use details::safe_upstream_reason;
pub use details::{
    safe_message, CircuitEvidence, DeliveryEvidence, Details, ErrorBasis, HealthScope, Phase,
    QuotaBasis, QuotaCheck, QuotaSource, QuotaVerdict, RoutingDetails, RoutingOutcome,
    RpmWaitReason,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashSet, VecDeque},
    io::{BufRead, BufReader},
    path::Path,
    sync::{mpsc, Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::sync::{broadcast, oneshot};
const RETENTION: u64 = 7 * 86400;
const MERGE: u64 = 300;
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    ModelUnavailable,
    Authentication,
    UpstreamService,
    Network,
    RateLimit,
    Capacity,
    FailoverExhausted,
    Failover,
    CircuitOpen,
    Recovered,
    ConfigConflict,
    StartupRecovery,
    AccountSync,
    ProtocolError,
    ThreadHandoff,
    ThreadRouting,
    RequestInterrupted,
}
impl Reason {
    pub fn notifiable(self, _action: Action) -> bool {
        matches!(self, Self::CircuitOpen | Self::ThreadHandoff)
    }
    pub fn text(self) -> &'static str {
        match self {
            Self::ModelUnavailable => "指定模型不存在或无权使用",
            Self::Authentication => "上游认证失败",
            Self::UpstreamService => "上游服务异常",
            Self::Network => "上游连接失败",
            Self::RateLimit => "上游限流",
            Self::Capacity => "模型容量不足",
            Self::FailoverExhausted => "请求未完成",
            Self::Failover => "自动换商",
            Self::CircuitOpen => "供应商已熔断",
            Self::Recovered => "供应商已恢复",
            Self::ConfigConflict => "配置发生冲突",
            Self::StartupRecovery => "启动恢复失败",
            Self::AccountSync => "账号同步异常",
            Self::ProtocolError => "上游协议返回错误",
            Self::ThreadHandoff => "线程已接管",
            Self::ThreadRouting => "线程路由结果",
            Self::RequestInterrupted => "请求交付提前结束",
        }
    }
    pub fn code(self) -> ErrorCode {
        match self {
            Self::ModelUnavailable => ErrorCode::ModelUnavailable,
            Self::Authentication => ErrorCode::AuthenticationFailed,
            Self::UpstreamService => ErrorCode::UpstreamServiceError,
            Self::Network => ErrorCode::NetworkError,
            Self::RateLimit => ErrorCode::RateLimited,
            Self::Capacity => ErrorCode::CapacityLimited,
            Self::FailoverExhausted => ErrorCode::FailoverExhausted,
            Self::Failover => ErrorCode::Failover,
            Self::CircuitOpen => ErrorCode::CircuitOpen,
            Self::Recovered => ErrorCode::Recovered,
            Self::ConfigConflict => ErrorCode::ConfigConflict,
            Self::StartupRecovery => ErrorCode::StartupRecoveryFailed,
            Self::AccountSync => ErrorCode::AccountSyncError,
            Self::ProtocolError => ErrorCode::ProtocolError,
            Self::ThreadHandoff => ErrorCode::ThreadHandoff,
            Self::ThreadRouting => ErrorCode::ThreadRouting,
            Self::RequestInterrupted => ErrorCode::RequestInterrupted,
        }
    }
}
pub use crate::errors::ErrorCode;
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    TryingNext,
    Returned,
    Waiting,
    Stopped,
    Recovered,
    Routed,
    Reconnecting,
    RetryingSame,
    NotRetried,
}
impl Action {
    pub fn text(self) -> &'static str {
        match self {
            Self::TryingNext => "尝试下一供应商",
            Self::Returned => "已反馈客户端",
            Self::Waiting => "等待后重试",
            Self::Stopped => "需要处理",
            Self::Routed => "已切换供应商",
            Self::Recovered => "已恢复可用",
            Self::Reconnecting => "等待后重新连接",
            Self::RetryingSame => "原供应商等待重试",
            Self::NotRetried => "响应已开始，未重试",
        }
    }
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Warning,
    Error,
    Info,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    OrdinaryError,
    CircuitError,
    Status,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    Upstream,
    Gateway,
    Antigravity,
}
// Historical identities must not become executable gateway clients.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum JournalClient {
    Active(ClientId),
    Retired(RetiredClient),
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RetiredClient {
    Agy,
    Antigravity,
}
impl From<ClientId> for JournalClient {
    fn from(value: ClientId) -> Self {
        Self::Active(value)
    }
}
impl JournalClient {
    pub fn active(self) -> Option<ClientId> {
        match self {
            Self::Active(c) => Some(c),
            Self::Retired(_) => None,
        }
    }
    fn name(self) -> &'static str {
        match self {
            Self::Active(client) => client.name(),
            Self::Retired(RetiredClient::Agy) => "Antigravity CLI（历史）",
            Self::Retired(RetiredClient::Antigravity) => "Antigravity app",
        }
    }
}
/// Query-only classification. Persisted records keep their original schema.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClassifiedRecord {
    #[serde(flatten)]
    pub record: Record,
    pub category: Category,
    pub origin: Origin,
}
impl std::ops::Deref for ClassifiedRecord {
    type Target = Record;
    fn deref(&self) -> &Record {
        &self.record
    }
}
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StatusGroup {
    Success,
    ClientError,
    ServerError,
    NoStatus,
}
impl StatusGroup {
    pub fn matches(self, status: Option<u16>) -> bool {
        match self {
            Self::Success => status.is_some_and(|v| (200..300).contains(&v)),
            Self::ClientError => status.is_some_and(|v| (400..500).contains(&v)),
            Self::ServerError => status.is_some_and(|v| (500..600).contains(&v)),
            Self::NoStatus => status.is_none(),
        }
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Record {
    #[serde(skip)]
    pub notification_message: Option<String>,
    #[serde(skip)]
    pub notification_provider: Option<String>,
    pub id: String,
    pub first_at: u64,
    pub last_at: u64,
    pub count: u32,
    pub client_id: Option<JournalClient>,
    pub provider_id: Option<String>,
    pub model: Option<String>,
    pub reason: Reason,
    pub action: Action,
    pub level: Level,
    pub status: Option<u16>,
    pub attempt: Option<u32>,
    /// Added in v0.13.1. Old JSONL records omit this field and are mapped
    /// from reason at read time by the frontend.
    pub error_code: Option<ErrorCode>,
    pub notified_at: u64,
    #[serde(default)]
    pub details: Details,
}
impl Record {
    pub fn configuration_failure(
        client: ClientId,
        error: &AppError,
        startup: bool,
    ) -> Option<Self> {
        let reason = if matches!(
            error.code.as_str(),
            code::CONFLICT
                | code::RESUME_CONFLICT
                | code::CLAUDE_PROFILE_CONFLICT
                | "CONFIG_CHANGED"
                | "OFFICIAL_CONFLICT"
        ) {
            Reason::ConfigConflict
        } else if startup {
            Reason::StartupRecovery
        } else {
            return None;
        };
        let mut record = Self::new(
            Some(client),
            None,
            None,
            reason,
            Action::Stopped,
            None,
            None,
        );
        record.details.local_code = safe_message(&error.code);
        record.details.message = safe_message(&error.message);
        Some(record)
    }
    fn visible(&self) -> bool {
        if self.reason == Reason::Recovered {
            return false;
        }
        if self.reason != Reason::ThreadRouting {
            return true;
        }
        self.details.routing.as_ref().is_none_or(|routing| {
            routing.failure.is_some()
                || self.level != Level::Info
                || self.status.is_some_and(|status| status >= 400)
                || self.action == Action::Stopped
                || matches!(
                    routing.evidence.identity,
                    Some(crate::usage::model::ThreadIdentity::Conflict)
                )
                || routing.outcome == RoutingOutcome::SaveFailed
        })
    }
    pub fn origin(&self) -> Origin {
        if self.client_id == Some(JournalClient::Retired(RetiredClient::Antigravity)) {
            return Origin::Antigravity;
        }
        let response_error = matches!(
            self.reason,
            Reason::ModelUnavailable
                | Reason::Authentication
                | Reason::UpstreamService
                | Reason::RateLimit
                | Reason::Capacity
                | Reason::ProtocolError
        );
        if response_error
            && !(self.level == Level::Info && self.details.quota_check.is_some())
            && (self.details.raw_error.is_some()
                || self.details.raw.is_some()
                || self.details.upstream_code.is_some()
                || self.details.upstream_type.is_some()
                || self.status.is_some_and(|s| s >= 400)
                || (self.status.is_some() && self.details.error_basis.is_some()))
        {
            Origin::Upstream
        } else {
            Origin::Gateway
        }
    }
    pub fn antigravity_error(
        key: &str,
        timestamp: u64,
        model: Option<&str>,
        raw_error: &str,
    ) -> Self {
        let lower = raw_error.to_ascii_lowercase();
        let (reason, status, code_label) = if lower.contains("stream was interrupted")
            || lower.contains("5m0s")
            || lower.contains("deadline_exceeded")
            || lower.contains("context deadline exceeded")
            || lower.contains("timed out")
        {
            (Reason::RequestInterrupted, None, "STREAM_TIMEOUT")
        } else if lower.contains("resource_exhausted")
            || lower.contains("quota")
            || lower.contains("rate limit")
            || lower.contains("too many requests")
            || has_status_token(&lower, "429")
        {
            (Reason::RateLimit, Some(429), "RESOURCE_EXHAUSTED")
        } else if lower.contains("unauthenticated")
            || lower.contains("permission_denied")
            || lower.contains("invalid_grant")
            || has_status_token(&lower, "401")
            || has_status_token(&lower, "403")
        {
            (Reason::Authentication, Some(401), "UNAUTHENTICATED")
        } else if lower.contains("model")
            && (lower.contains("not found")
                || lower.contains("unavailable")
                || lower.contains("not supported"))
        {
            (Reason::ModelUnavailable, Some(404), "MODEL_UNAVAILABLE")
        } else {
            (Reason::UpstreamService, Some(502), "ANTIGRAVITY_ERROR")
        };
        let digest = storage::digest(format!("antigravity-event:{key}").as_bytes());
        let mut bytes = [0u8; 16];
        for (i, slot) in bytes.iter_mut().enumerate() {
            *slot = u8::from_str_radix(&digest[i * 2..i * 2 + 2], 16).unwrap_or(0);
        }
        bytes[6] = (bytes[6] & 0x0f) | 0x40;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        let details = Details {
            upstream_type: None,
            upstream_code: Some(code_label.into()),
            message: raw_log_message(raw_error),
            ..Default::default()
        };
        let t = timestamp.min(now());
        Self {
            notification_message: None,
            notification_provider: None,
            id: uuid::Uuid::from_bytes(bytes).to_string(),
            first_at: t,
            last_at: t,
            count: 1,
            client_id: Some(JournalClient::Retired(RetiredClient::Antigravity)),
            provider_id: None,
            model: model.and_then(safe_model),
            reason,
            action: Action::Returned,
            level: if matches!(
                reason,
                Reason::ModelUnavailable | Reason::RateLimit | Reason::Capacity
            ) {
                Level::Warning
            } else {
                Level::Error
            },
            status,
            attempt: Some(1),
            error_code: Some(reason.code()),
            notified_at: 0,
            details: details.sanitized(),
        }
    }
    fn merge_seconds(&self) -> u64 {
        if self.reason == Reason::RequestInterrupted {
            60
        } else {
            MERGE
        }
    }
    pub fn new(
        client_id: Option<ClientId>,
        provider_id: Option<&str>,
        model: Option<&str>,
        reason: Reason,
        action: Action,
        status: Option<u16>,
        attempt: Option<u32>,
    ) -> Self {
        let t = now();
        Self {
            notification_message: None,
            notification_provider: None,
            id: uuid::Uuid::new_v4().to_string(),
            first_at: t,
            last_at: t,
            count: 1,
            client_id: client_id.map(Into::into),
            provider_id: provider_id.and_then(safe_id),
            model: model.and_then(safe_model),
            reason,
            action,
            level: if matches!(
                reason,
                Reason::Recovered
                    | Reason::Failover
                    | Reason::ThreadHandoff
                    | Reason::ThreadRouting
            ) {
                Level::Info
            } else if matches!(
                reason,
                Reason::ModelUnavailable | Reason::RateLimit | Reason::Capacity
            ) {
                Level::Warning
            } else {
                Level::Error
            },
            status,
            attempt,
            error_code: Some(reason.code()),
            notified_at: 0,
            details: Details::default(),
        }
    }
    pub fn notification(&self) -> String {
        self.notification_at(now())
    }
    fn notification_at(&self, timestamp: u64) -> String {
        if self.reason == Reason::CircuitOpen {
            let provider = self
                .notification_provider
                .as_deref()
                .filter(|s| s.chars().count() <= 120)
                .and_then(safe_message)
                .unwrap_or_else(|| {
                    format!(
                        "供应商 {}",
                        self.provider_id
                            .as_deref()
                            .map(|s| &s[..s.len().min(8)])
                            .unwrap_or("未记录")
                    )
                });
            let scope = match self.details.health_scope {
                Some(HealthScope::Model) => format!(
                    "模型熔断 · {}",
                    self.model.as_deref().unwrap_or("模型未记录")
                ),
                Some(HealthScope::Provider) => "供应商熔断 · 全部模型".into(),
                _ => "熔断 · 影响范围未记录".into(),
            };
            let evidence = self.details.circuit.as_ref();
            let cause = self
                .details
                .upstream_code
                .as_deref()
                .or(self.details.local_code.as_deref());
            let mut direct = Vec::new();
            if let Some(status) = self.status {
                direct.push(format!("HTTP {status}"));
            }
            if let Some(code) = cause {
                direct.push(code.to_owned());
            }
            if let Some(message) = self.details.message.as_deref().and_then(safe_message) {
                direct.push(message.chars().take(120).collect());
            } else if let Some(reason) = evidence.and_then(|c| c.cause_reason) {
                direct.push(reason.text().into());
            }
            if direct.is_empty() {
                direct.push("原因未记录".into());
            }
            let trigger = evidence
                .map(|c| match c.trigger.as_str() {
                    "consecutive_failures" | "transient_failures" => {
                        format!("连续失败 {}/{}", c.failures, c.failure_threshold)
                    }
                    "probe_failed" => "恢复探测失败".into(),
                    "error_rate" => format!(
                        "错误率 {:.0}% ≥ {:.0}%（{}/{}，至少 {} 次）",
                        f64::from(c.failed_requests) / f64::from(c.requests.max(1)) * 100.,
                        c.error_rate * 100.,
                        c.failed_requests,
                        c.requests,
                        c.min_requests
                    ),
                    _ => "触发条件未记录".into(),
                })
                .unwrap_or_else(|| "触发条件未记录".into());
            let wait = match evidence.and_then(|c| c.retry_at) {
                Some(at) if at > timestamp => {
                    format!("等待 {} 秒后可探测", at - timestamp)
                }
                Some(_) => "待请求探测".into(),
                None => "等待时间未记录".into(),
            };
            return format!(
                "{} · {provider}：{scope}；{}；{trigger}；{wait}",
                self.client_id.map(JournalClient::name).unwrap_or("应用"),
                direct.join(" · ")
            );
        }
        if let Some(message) = &self.notification_message {
            return message.clone();
        }
        format!(
            "{} · {}{}：{}；{}",
            self.client_id.map(JournalClient::name).unwrap_or("应用"),
            self.provider_id
                .as_deref()
                .map(|s| &s[..s.len().min(8)])
                .unwrap_or("系统"),
            self.model
                .as_ref()
                .map(|s| format!(" · {s}"))
                .unwrap_or_default(),
            self.reason.text(),
            self.action.text()
        )
    }
    fn same(&self, other: &Self) -> bool {
        if self.client_id != other.client_id
            || self.provider_id != other.provider_id
            || self.model != other.model
            || self.reason != other.reason
            || self.status != other.status
            || self.action != other.action
        {
            return false;
        }
        let mut details = self.details.clone();
        let mut other_details = other.details.clone();
        details.last_progress_ms = None;
        other_details.last_progress_ms = None;
        details.delivery = None;
        other_details.delivery = None;
        if let Some(raw) = &mut details.raw_error {
            raw.id.clear();
        }
        if let Some(raw) = &mut other_details.raw_error {
            raw.id.clear();
        }
        details == other_details
    }
}
fn safe_id(s: &str) -> Option<String> {
    uuid::Uuid::parse_str(s).ok().map(|v| v.to_string())
}
fn raw_log_message(value: &str) -> Option<String> {
    let text = value.split_whitespace().collect::<Vec<_>>().join(" ");
    (!text.is_empty()).then(|| text.chars().take(4096).collect())
}
fn is_antigravity_log_record(r: &Record) -> bool {
    r.client_id == Some(JournalClient::Retired(RetiredClient::Antigravity))
        && r.provider_id.is_none()
        && matches!(
            r.details.upstream_code.as_deref(),
            Some(
                "STREAM_TIMEOUT"
                    | "STREAM_INTERRUPTED"
                    | "INTERNAL"
                    | "RESOURCE_EXHAUSTED"
                    | "UNAUTHENTICATED"
                    | "MODEL_UNAVAILABLE"
                    | "ANTIGRAVITY_ERROR"
            )
        )
}
fn scan_antigravity_file_lines(
    path: &Path,
    file_len: u64,
    file_secs: u64,
    cursors: &mut std::collections::HashMap<std::path::PathBuf, (u64, u64, u64)>,
    mut on_line: impl FnMut(u64, &[u8]),
) {
    use std::io::{Seek, SeekFrom};
    let start_offset = match cursors.get(path) {
        Some(&(prev_len, prev_secs, prev_off))
            if prev_len == file_len && prev_secs == file_secs && prev_off == file_len =>
        {
            return;
        }
        Some(&(_, _, prev_off)) if file_len >= prev_off => prev_off,
        _ => 0,
    };
    let Ok(mut file) = std::fs::File::open(path) else {
        return;
    };
    let mut offset = start_offset;
    let mut discard_first = false;
    if start_offset == 0 && file_len > 4 * 1024 * 1024 {
        offset = file_len - 4 * 1024 * 1024;
        discard_first = true;
    }
    if offset > 0 && file.seek(SeekFrom::Start(offset)).is_err() {
        return;
    }
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    if discard_first {
        let Ok(discarded) = reader.read_until(b'\n', &mut line) else {
            return;
        };
        if discarded == 0 || !line.ends_with(b"\n") {
            return;
        }
        offset = offset.saturating_add(discarded as u64);
        line.clear();
    }
    while let Ok(read_bytes) = reader.read_until(b'\n', &mut line) {
        if read_bytes == 0 || !line.ends_with(b"\n") {
            break;
        }
        let line_offset = offset;
        offset = offset.saturating_add(read_bytes as u64);
        on_line(line_offset, &line);
        line.clear();
    }
    cursors.insert(path.to_path_buf(), (file_len, file_secs, offset));
}
fn has_status_token(lower: &str, code: &str) -> bool {
    let bytes = lower.as_bytes();
    let pat = code.as_bytes();
    for (idx, _) in lower.match_indices(code) {
        let prev = idx.checked_sub(1).map(|i| bytes[i]);
        let prev2 = idx.checked_sub(2).map(|i| bytes[i]);
        let next = bytes.get(idx + pat.len()).copied();
        let bad_prev = prev.is_some_and(|b| {
            b.is_ascii_alphanumeric()
                || b == b'.'
                || b == b'-'
                || (b == b':' && prev2.is_some_and(|p| p.is_ascii_digit()))
        });
        let bad_next =
            next.is_some_and(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b':' || b == b'-');
        if !bad_prev && !bad_next {
            return true;
        }
    }
    false
}
fn is_actionable_antigravity_log_error(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    lower.contains("[error]")
        || lower.contains("level=error")
        || lower.contains("unauthenticated")
        || lower.contains("resource_exhausted")
        || lower.contains("permission_denied")
        || lower.contains("deadline_exceeded")
        || lower.contains("context deadline exceeded")
        || lower.contains("stream was interrupted")
        || lower.contains("5m0s")
        || lower.contains("quota")
        || lower.contains("rate limit")
        || lower.contains("too many requests")
        || lower.contains("invalid_grant")
        || has_status_token(&lower, "401")
        || has_status_token(&lower, "403")
        || has_status_token(&lower, "429")
        || has_status_token(&lower, "500")
        || has_status_token(&lower, "502")
        || has_status_token(&lower, "503")
        || has_status_token(&lower, "504")
}
fn parse_antigravity_log_line(line: &str, fallback_secs: u64) -> Option<(u64, &str)> {
    use chrono::{Datelike, TimeZone};
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(after_bracket) = trimmed.strip_prefix('[') {
        if let Some((ts_part, rest)) = after_bracket.split_once(']') {
            if let Some(ndt) = ts_part
                .get(..19)
                .and_then(|s| chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").ok())
            {
                let ts = chrono::Local
                    .from_local_datetime(&ndt)
                    .single()
                    .and_then(|dt| u64::try_from(dt.timestamp()).ok())
                    .unwrap_or(fallback_secs);
                let rest_trimmed = rest.trim();
                let lower = rest_trimmed.to_ascii_lowercase();
                if lower.contains("[error]") || lower.contains("level=error") {
                    let body = rest_trimmed
                        .strip_prefix("[error]")
                        .or_else(|| rest_trimmed.strip_prefix("[ERROR]"))
                        .unwrap_or(rest_trimmed)
                        .trim();
                    return (!body.is_empty()).then_some((ts, body));
                }
                return None;
            }
        }
    }
    let glog = trimmed
        .strip_prefix("ERROR: logging before google.Init: ")
        .unwrap_or(trimmed);
    if let Some(epoch_part) = glog.strip_prefix("E0000 00:00:") {
        let (sec_str, _) = epoch_part.split_once('.')?;
        let ts = sec_str.parse::<u64>().unwrap_or(fallback_secs);
        let body = glog.split_once(']').map(|(_, b)| b.trim()).unwrap_or(glog);
        return is_actionable_antigravity_log_error(body).then_some((ts, body));
    }
    if glog.starts_with('E')
        && glog.len() > 14
        && glog.as_bytes()[1..5].iter().all(u8::is_ascii_digit)
        && glog.as_bytes()[5] == b' '
    {
        let mm = glog[1..3].parse::<u32>().ok()?;
        let dd = glog[3..5].parse::<u32>().ok()?;
        let hms = glog.get(6..14)?;
        let year = chrono::DateTime::from_timestamp(fallback_secs as i64, 0)
            .map(|dt| dt.with_timezone(&chrono::Local).year())
            .unwrap_or(2026);
        let ts = chrono::NaiveDate::from_ymd_opt(year, mm, dd)
            .and_then(|d| {
                let t = chrono::NaiveTime::parse_from_str(hms, "%H:%M:%S").ok()?;
                chrono::Local.from_local_datetime(&d.and_time(t)).single()
            })
            .and_then(|dt| u64::try_from(dt.timestamp()).ok())
            .unwrap_or(fallback_secs);
        let body = glog.split_once(']').map(|(_, b)| b.trim()).unwrap_or(glog);
        return is_actionable_antigravity_log_error(body).then_some((ts, body));
    }
    let lower = trimmed.to_ascii_lowercase();
    if lower.starts_with("[error]") || lower.contains("level=error") {
        let body = trimmed
            .strip_prefix("[error]")
            .or_else(|| trimmed.strip_prefix("[ERROR]"))
            .unwrap_or(trimmed)
            .trim();
        return (!body.is_empty()).then_some((fallback_secs, body));
    }
    None
}
pub fn safe_model(s: &str) -> Option<String> {
    let lower = s.to_ascii_lowercase();
    (!s.is_empty()
        && s.len() <= 120
        && ![
            "sk-", "bearer", "eyj", "http", "token", "secret", "password", "cookie", "users/",
            "home/", ".com", ".net", ".org", ".xyz",
        ]
        .iter()
        .any(|v| lower.contains(v))
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-:/".contains(c))
        && !s.contains("://")
        && !s.starts_with('/')
        && s.parse::<std::net::IpAddr>().is_err()
        && !s
            .as_bytes()
            .windows(32)
            .any(|w| w.iter().all(u8::is_ascii_hexdigit)))
    .then(|| s.to_owned())
}
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Change {
    pub record: Option<Record>,
    pub error: Option<String>,
    #[serde(skip)]
    pub notify: bool,
}
#[derive(Default)]
struct Journal {
    records: VecDeque<Arc<Record>>,
    // Runtime health can refer to a just-merged event. Keep bounded aliases for
    // those links; persisted terminal events use the canonical row ID.
    aliases: VecDeque<(String, String)>,
    error: Option<String>,
    dirty: bool,
}
impl Journal {
    fn needs_write(&self, path: &Path) -> bool {
        self.dirty
            || !path.exists()
            || self
                .records
                .iter()
                .any(|r| r.last_at < now().saturating_sub(RETENTION))
    }
    fn retain_since(&mut self, cutoff: u64) -> bool {
        let before = self.records.len();
        self.records.retain(|r| r.last_at >= cutoff);
        let pruned = self.records.len() != before;
        let retained: HashSet<_> = self.records.iter().map(|r| r.id.as_str()).collect();
        self.aliases
            .retain(|(_, target)| retained.contains(target.as_str()));
        pruned
    }
    fn prepare_rewrite(&mut self, path: &Path, force: bool) -> Option<Vec<Arc<Record>>> {
        if !force && !self.needs_write(path) {
            return None;
        }
        self.retain_since(now().saturating_sub(RETENTION));
        self.dirty = true;
        Some(self.records.iter().cloned().collect())
    }
    fn retained_raw_ids(&self) -> HashSet<String> {
        self.records
            .iter()
            .filter_map(|r| r.details.raw_error.as_ref().map(|r| r.id.clone()))
            .collect()
    }
}
enum Job {
    Append(Box<Record>),
    Clear(oneshot::Sender<Result<()>>),
    Prune,
    NotificationAccepted(String),
}
type AntigravityCursors = std::collections::HashMap<std::path::PathBuf, (u64, u64, u64)>;

#[derive(Clone)]
pub struct Service {
    sender: mpsc::SyncSender<Job>,
    journal: Arc<Mutex<Journal>>,
    events: broadcast::Sender<Change>,
    raw_directory: Arc<std::path::PathBuf>,
    antigravity_cursors: Arc<Mutex<AntigravityCursors>>,
}
#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Filter {
    pub origin: Option<Origin>,
    pub from: Option<u64>,
    pub to: Option<u64>,
    pub client_id: Option<JournalClient>,
    pub provider_id: Option<String>,
    pub level: Option<Level>,
    pub category: Option<Category>,
    pub reason: Option<Reason>,
    pub status_group: Option<StatusGroup>,
    pub page: Option<usize>,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Page {
    pub items: Vec<ClassifiedRecord>,
    pub total: usize,
    pub page: usize,
    pub error: Option<String>,
}
impl Journal {
    fn circuit_causes(&self, cutoff: u64) -> HashSet<&str> {
        self.records
            .iter()
            .filter(|r| r.reason == Reason::CircuitOpen && r.last_at >= cutoff)
            .filter_map(|r| r.details.cause_id.as_deref())
            .collect()
    }
}
fn classify(record: &Record, causes: &HashSet<&str>) -> Category {
    if record.reason == Reason::CircuitOpen || causes.contains(record.id.as_str()) {
        Category::CircuitError
    } else if (record.details.quota_check.is_some() && record.level == Level::Info)
        || matches!(
            record.reason,
            Reason::Failover | Reason::Recovered | Reason::ThreadHandoff
        )
        || (matches!(
            record.reason,
            Reason::RequestInterrupted | Reason::ThreadRouting
        ) && record.level == Level::Info)
    {
        Category::Status
    } else {
        Category::OrdinaryError
    }
}
fn load(path: &Path) -> Journal {
    let mut j = Journal::default();
    let file = (|| -> std::io::Result<Option<std::fs::File>> {
        let metadata = match std::fs::symlink_metadata(path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(std::io::Error::other("invalid journal file"));
        }
        std::fs::File::open(path).map(Some)
    })();
    match file {
        Ok(Some(file)) => {
            let mut reader = BufReader::new(file);
            let mut line = Vec::new();
            loop {
                line.clear();
                match reader.read_until(b'\n', &mut line) {
                    Ok(0) => break,
                    Ok(_) => (),
                    Err(_) => {
                        j.error = Some("日志读取失败，请检查文件权限与空间".into());
                        break;
                    }
                }
                if line == b"\n" {
                    continue;
                }
                match serde_json::from_slice::<Record>(&line) {
                    Ok(mut r)
                        if uuid::Uuid::parse_str(&r.id).is_ok()
                            && r.provider_id
                                .as_deref()
                                .is_none_or(|s| safe_id(s).is_some())
                            && r.model.as_deref().is_none_or(|s| safe_model(s).is_some())
                            && r.last_at <= now().saturating_add(60) =>
                    {
                        let sanitized = r.details.clone().sanitized();
                        j.dirty |= sanitized != r.details;
                        r.details = sanitized;
                        if r.details.upstream_type.as_deref() == Some("antigravity_log") {
                            r.details.upstream_type = None;
                            j.dirty = true;
                        }
                        let redacted_agy = r.client_id
                            == Some(JournalClient::Retired(RetiredClient::Antigravity))
                            && r.details
                                .message
                                .as_deref()
                                .is_some_and(|m| m.contains("[已隐去]"));
                        if (r.client_id == Some(JournalClient::Retired(RetiredClient::Antigravity))
                            && !is_antigravity_log_record(&r))
                            || redacted_agy
                        {
                            j.dirty = true;
                        } else if r.visible() && r.last_at >= now().saturating_sub(RETENTION) {
                            j.records.push_back(Arc::new(r))
                        } else {
                            j.dirty = true;
                        }
                    }
                    _ => {
                        j.dirty = true;
                        j.error = Some("日志包含损坏记录，已跳过；其余记录仍可查看".into());
                    }
                }
            }
        }
        Ok(None) => (),
        Err(_) => j.error = Some("日志读取失败，请检查文件权限与空间".into()),
    }
    j
}
fn append_lines<'a>(path: &Path, rows: impl Iterator<Item = &'a Record>) -> Result<()> {
    use std::io::Write;
    let metadata = std::fs::symlink_metadata(path).map_err(storage::io_error)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(storage::io_error(std::io::Error::other(
            "invalid journal file",
        )));
    }
    let mut buf = Vec::new();
    for record in rows {
        serde_json::to_writer(&mut buf, record)
            .map_err(|_| AppError::new(code::LOG_WRITE, "日志写入失败，请检查文件权限与空间"))?;
        buf.push(b'\n');
    }
    if buf.is_empty() {
        return Ok(());
    }
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(path)
        .map_err(storage::io_error)?;
    file.write_all(&buf).map_err(storage::io_error)?;
    Ok(())
}
fn resolve_cause_id<'a, I>(r: &mut Record, aliases: &VecDeque<(String, String)>, records: I)
where
    I: DoubleEndedIterator<Item = &'a Record> + Clone,
{
    if let Some((_, target)) = aliases
        .iter()
        .rev()
        .find(|(id, _)| r.details.cause_id.as_ref() == Some(id))
    {
        r.details.cause_id = Some(target.clone());
    }
    if matches!(r.reason, Reason::CircuitOpen | Reason::FailoverExhausted)
        && r.details
            .cause_id
            .as_ref()
            .is_none_or(|id| !records.clone().any(|old| &old.id == id))
    {
        // A repeated cause may already have been merged into an earlier row.
        r.details.cause_id = records
            .rev()
            .find(|old| {
                !matches!(old.reason, Reason::CircuitOpen | Reason::FailoverExhausted)
                    && (r.reason != Reason::CircuitOpen
                        || old.details.counted_failure == Some(true))
                    && old.client_id == r.client_id
                    && old.provider_id == r.provider_id
                    && old.model == r.model
                    && old.status == r.status
                    && old.details.upstream_code == r.details.upstream_code
                    && old.details.upstream_reason == r.details.upstream_reason
                    && old.details.error_basis == r.details.error_basis
                    && old.details.quota_check == r.details.quota_check
                    && old.details.rpm_wait_reason == r.details.rpm_wait_reason
                    && old.details.local_code == r.details.local_code
                    && old.details.message == r.details.message
                    && old.details.phase == r.details.phase
            })
            .map(|cause| cause.id.clone());
    }
}
fn write_records_stream(path: &Path, records: &[Arc<Record>]) -> Result<()> {
    storage::atomic_write_stream(path, |writer| {
        for record in records {
            serde_json::to_writer(&mut *writer, record.as_ref())?;
            writer.write_all(b"\n")?;
        }
        Ok(())
    })
}
impl Service {
    pub fn new(data: &Path) -> Self {
        let path = data.join("diagnostics.jsonl");
        let raw_directory = Arc::new(data.join("diagnostic-errors"));
        let raw_path = raw_directory.clone();
        let journal = Arc::new(Mutex::new(load(&path)));
        let (events, _) = broadcast::channel(128);
        let (sender, receiver) = mpsc::sync_channel(256);
        let j = journal.clone();
        let events_copy = events.clone();
        std::thread::spawn(move || {
            // Retention also runs after restart, before processing queued writes.
            {
                let startup = {
                    let mut initial = j.lock().unwrap();
                    (initial.error.is_none() && path.exists()).then(|| {
                        let snapshot = initial.prepare_rewrite(&path, false);
                        let retained = initial.retained_raw_ids();
                        (snapshot, retained)
                    })
                };
                if let Some((snapshot, retained)) = startup {
                    let written = match &snapshot {
                        Some(records) => write_records_stream(&path, records),
                        None => Ok(()),
                    };
                    let pruned = if written.is_ok() {
                        prune_raw_ids(&raw_path, &retained)
                    } else {
                        Ok(())
                    };
                    let mut initial = j.lock().unwrap();
                    if written.is_err() {
                        initial.error = Some("日志写入失败，请检查文件权限与空间".into());
                    } else {
                        if snapshot.is_some() {
                            initial.dirty = false;
                        }
                        if pruned.is_err() {
                            initial.error = Some("日志原文清理失败，请重试".into());
                        }
                    }
                }
            }
            let mut pending = None;
            loop {
                let job = match pending.take() {
                    Some(job) => job,
                    None => match receiver.recv_timeout(std::time::Duration::from_secs(3600)) {
                        Ok(v) => v,
                        Err(mpsc::RecvTimeoutError::Timeout) => Job::Prune,
                        Err(_) => break,
                    },
                };
                if matches!(&job, Job::Prune) {
                    let mut current = j.lock().unwrap();
                    if !current.needs_write(&path) {
                        if prune_raw(&raw_path, &current).is_err() {
                            current.error = Some("日志原文清理失败，请重试".into());
                        }
                        let _ = events_copy.send(Change {
                            record: None,
                            error: current.error.clone(),
                            notify: false,
                        });
                        continue;
                    }
                }
                if let Job::Clear(tx) = job {
                    let result = storage::atomic_write_stream(&path, |_| Ok(()));
                    let raw_pruned = if result.is_ok() {
                        prune_raw_ids(&raw_path, &HashSet::new())
                    } else {
                        Ok(())
                    };
                    let error = {
                        let mut current = j.lock().unwrap();
                        if result.is_ok() {
                            current.records.clear();
                            current.aliases.clear();
                            current.dirty = false;
                            current.error =
                                raw_pruned.err().map(|_| "日志原文清理失败，请重试".into());
                        } else {
                            current.dirty = true;
                            current.error = Some("日志写入失败，请检查文件权限与空间".into());
                        }
                        current.error.clone()
                    };
                    let _ = events_copy.send(Change {
                        record: None,
                        error,
                        notify: false,
                    });
                    let _ = tx.send(result);
                    continue;
                }
                let cleanup = matches!(&job, Job::Prune);
                let prune_only = matches!(&job, Job::Prune);
                let append = matches!(&job, Job::Append(_));
                let mut jobs = vec![job];
                if append {
                    while jobs.len() < 64 {
                        match receiver.try_recv() {
                            Ok(job @ Job::Append(_)) => jobs.push(job),
                            Ok(barrier) => {
                                pending = Some(barrier);
                                break;
                            }
                            Err(_) => break,
                        }
                    }
                }
                let mut append_only = append;
                let mut changes = Vec::with_capacity(jobs.len());
                for job in jobs {
                    let mut changed = None;
                    let mut notify = false;
                    match job {
                        Job::Append(r) => {
                            let mut r = *r;
                            let raw = r.details.raw.take();
                            if let Some(raw) = &raw {
                                r.details.raw_error = Some(raw.reference(&r.id));
                                // Only the list preview is shortened. Matching and the drawer read the attachment.
                                for value in [
                                    &mut r.details.message,
                                    &mut r.details.upstream_code,
                                    &mut r.details.upstream_reason,
                                    &mut r.details.upstream_type,
                                    &mut r.details.parameter,
                                ]
                                .into_iter()
                                .flatten()
                                {
                                    *value = value.chars().take(512).collect();
                                }
                            }
                            notify = r.reason.notifiable(r.action);
                            let outcome = {
                                let mut journal = j.lock().unwrap();
                                resolve_cause_id(
                                    &mut r,
                                    &journal.aliases,
                                    journal.records.iter().map(Arc::as_ref),
                                );
                                if let Some(old_arc) =
                                    journal.records.iter_mut().rev().find(|old| {
                                        old.same(&r)
                                            && r.last_at.saturating_sub(old.first_at)
                                                < r.merge_seconds()
                                    })
                                {
                                    append_only = false;
                                    let old = Arc::make_mut(old_arc);
                                    old.last_at = r.last_at;
                                    old.count = old.count.saturating_add(1);
                                    old.status = r.status;
                                    old.action = r.action;
                                    old.attempt = r.attempt;
                                    if r.reason == Reason::RequestInterrupted {
                                        old.details.delivery = r.details.delivery;
                                        old.details.last_progress_ms = r.details.last_progress_ms;
                                    }
                                    let snapshot = old.clone();
                                    let target = snapshot.id.clone();
                                    journal.aliases.push_back((r.id, target));
                                    while journal.aliases.len() > 4096 {
                                        journal.aliases.pop_front();
                                    }
                                    Ok(snapshot)
                                } else if raw.is_none() {
                                    let snapshot = r.clone();
                                    journal.records.push_back(Arc::new(r));
                                    Ok(snapshot)
                                } else {
                                    Err(r)
                                }
                            };
                            match outcome {
                                Ok(snapshot) => {
                                    changed = Some(snapshot);
                                }
                                Err(mut r) => {
                                    let raw_failed = raw
                                        .as_ref()
                                        .is_some_and(|raw| raw.save(&raw_path, &r.id).is_err());
                                    if raw_failed {
                                        if let Some(reference) = &mut r.details.raw_error {
                                            reference.incomplete = Some("错误原文保存失败".into());
                                        }
                                    }
                                    let mut journal = j.lock().unwrap();
                                    if raw_failed {
                                        journal.error =
                                            Some("错误原文保存失败，请检查文件权限与空间".into());
                                    }
                                    changed = Some(r.clone());
                                    journal.records.push_back(Arc::new(r));
                                }
                            }
                        }
                        Job::NotificationAccepted(id) => {
                            let mut journal = j.lock().unwrap();
                            if let Some(r_arc) = journal.records.iter_mut().find(|r| r.id == id) {
                                let r = Arc::make_mut(r_arc);
                                r.notified_at = now();
                                changed = Some(r.clone());
                            }
                        }
                        Job::Clear(_) | Job::Prune => (),
                    }
                    changes.push((changed, notify));
                }
                let (snapshot, retained_raw_ids) = {
                    let mut journal = j.lock().unwrap();
                    let before_retention = journal.records.len();
                    append_only &= !journal.needs_write(&path);
                    let snapshot = if append_only {
                        None
                    } else {
                        journal.prepare_rewrite(&path, !prune_only)
                    };
                    let retained_raw = (cleanup || journal.records.len() != before_retention)
                        .then(|| journal.retained_raw_ids());
                    (snapshot, retained_raw)
                };
                let result = if append_only {
                    append_lines(
                        &path,
                        changes.iter().filter_map(|(record, _)| record.as_ref()),
                    )
                } else {
                    match &snapshot {
                        Some(records) => write_records_stream(&path, records),
                        None => Ok(()),
                    }
                };
                let raw_pruned = if result.is_ok() {
                    if let Some(retained) = &retained_raw_ids {
                        prune_raw_ids(&raw_path, retained)
                    } else {
                        Ok(())
                    }
                } else {
                    Ok(())
                };
                let error = {
                    let mut journal = j.lock().unwrap();
                    if result.is_ok() {
                        if snapshot.is_some() {
                            journal.dirty = false;
                        }
                        if raw_pruned.is_err() {
                            journal.error = Some("日志原文清理失败，请重试".into());
                        }
                    } else {
                        journal.dirty = true;
                        journal.error = Some("日志写入失败，请检查文件权限与空间".into());
                    }
                    journal.error.clone()
                };
                for (record, notify) in changes {
                    let _ = events_copy.send(Change {
                        record,
                        error: error.clone(),
                        notify,
                    });
                }
            }
        });
        Self {
            sender,
            journal,
            events,
            raw_directory,
            antigravity_cursors: Arc::new(Mutex::new(std::collections::HashMap::new())),
        }
    }
    pub fn subscribe(&self) -> broadcast::Receiver<Change> {
        self.events.subscribe()
    }
    pub fn emit(&self, r: Record) {
        self.emit_checked(r);
    }
    /// Queue acceptance, not a durability acknowledgment. Gateway diagnostics
    /// use this receipt only to avoid a second description of the same fault.
    pub(crate) fn emit_checked(&self, mut r: Record) -> bool {
        if !r.visible() {
            return false;
        }
        r.details = r.details.sanitized();
        if self.sender.try_send(Job::Append(Box::new(r))).is_err() {
            let mut j = self.journal.lock().unwrap();
            j.error = Some("日志队列已满，部分事件未记录".into());
            let _ = self.events.send(Change {
                record: None,
                error: j.error.clone(),
                notify: false,
            });
            return false;
        }
        true
    }
    pub fn notification_accepted(&self, id: String) {
        // Failure to persist acknowledgment never blocks gateway delivery.
        let _ = self.sender.try_send(Job::NotificationAccepted(id));
    }
    pub fn query(&self, f: Filter) -> Page {
        let j = self.journal.lock().unwrap();
        let cutoff = now().saturating_sub(RETENTION);
        let causes = j.circuit_causes(cutoff);
        let mut rows: Vec<_> = j
            .records
            .iter()
            .filter(|r| {
                r.visible()
                    && f.origin.is_none_or(|v| r.origin() == v)
                    && r.last_at >= cutoff
                    && f.from.is_none_or(|t| r.last_at >= t)
                    && f.to.is_none_or(|t| r.first_at <= t)
                    && f.client_id.is_none_or(|v| r.client_id == Some(v))
                    && f.provider_id
                        .as_ref()
                        .is_none_or(|v| r.provider_id.as_ref() == Some(v))
                    && f.level.is_none_or(|v| r.level == v)
                    && f.category.is_none_or(|v| classify(r, &causes) == v)
                    && f.reason.is_none_or(|v| r.reason == v)
                    && f.status_group.is_none_or(|v| v.matches(r.status))
            })
            .collect();
        rows.sort_by_key(|r| std::cmp::Reverse(r.last_at));
        let total = rows.len();
        let page = f.page.unwrap_or(1).max(1).min(total.div_ceil(50).max(1));
        Page {
            items: rows
                .into_iter()
                .skip((page - 1) * 50)
                .take(50)
                .map(|r| ClassifiedRecord {
                    origin: r.origin(),
                    record: (**r).clone(),
                    category: classify(r, &causes),
                })
                .collect(),
            total,
            page,
            error: j.error.clone(),
        }
    }
    pub fn sync_antigravity_logs(&self, home: &Path) {
        let cutoff = now().saturating_sub(RETENTION);
        let mut cursors = self
            .antigravity_cursors
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut discovered = Vec::new();
        let brain = home.join(".gemini/antigravity/brain");
        if let Ok(entries) = std::fs::read_dir(&brain) {
            let mut transcripts = Vec::new();
            for entry in entries.flatten() {
                let candidate = entry.path().join(".system_generated/logs/transcript.jsonl");
                if let Ok(meta) = std::fs::metadata(&candidate) {
                    if let Ok(modified) = meta.modified() {
                        let secs = modified
                            .duration_since(UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs();
                        if secs >= cutoff && meta.is_file() {
                            transcripts.push((secs, meta.len(), entry.file_name(), candidate));
                        }
                    }
                }
            }
            transcripts.sort_by_key(|(secs, _, _, _)| std::cmp::Reverse(*secs));
            for (file_secs, file_len, conv_name, path) in transcripts.into_iter().take(48) {
                let conv = conv_name.to_string_lossy();
                scan_antigravity_file_lines(
                    &path,
                    file_len,
                    file_secs,
                    &mut cursors,
                    |line_offset, line| {
                        if line.len() <= 512 * 1024 {
                            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(line) {
                                let step_type = v["type"].as_str().unwrap_or("");
                                let raw_err = v["errorMessage"]
                                    .as_str()
                                    .or_else(|| v["error"]["message"].as_str())
                                    .or_else(|| v["error"].as_str())
                                    .map(str::trim)
                                    .filter(|s| !s.is_empty());
                                let is_err = !matches!(
                                    step_type,
                                    "GENERIC" | "TOOL_RESPONSE" | "USER_INPUT"
                                ) && (step_type.eq_ignore_ascii_case("ERROR_MESSAGE")
                                    || raw_err.is_some());
                                if is_err {
                                    let step_key = v["step_index"]
                                        .as_u64()
                                        .map(|n| n.to_string())
                                        .unwrap_or_else(|| {
                                            format!(
                                                "off:{line_offset}:{}",
                                                &storage::digest(line)[..12]
                                            )
                                        });
                                    let ts = v["created_at"]
                                        .as_str()
                                        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                                        .and_then(|dt| u64::try_from(dt.timestamp()).ok())
                                        .unwrap_or(file_secs);
                                    if ts >= cutoff {
                                        let raw = raw_err.unwrap_or("Antigravity step error");
                                        let model = v["model"]
                                            .as_str()
                                            .or_else(|| v["response_model"].as_str());
                                        let r = Record::antigravity_error(
                                            &format!("transcript:{conv}:{step_key}"),
                                            ts,
                                            model,
                                            raw,
                                        );
                                        discovered.push(r);
                                    }
                                }
                            }
                        }
                    },
                );
            }
        }
        for log_dir in [
            home.join("Library/Logs/Antigravity"),
            home.join(".gemini/antigravity/logs"),
        ] {
            let Ok(entries) = std::fs::read_dir(&log_dir) else {
                continue;
            };
            for entry in entries.flatten().take(24) {
                let path = entry.path();
                let Ok(meta) = std::fs::metadata(&path) else {
                    continue;
                };
                if !meta.is_file() {
                    continue;
                }
                let file_secs = meta
                    .modified()
                    .ok()
                    .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                if file_secs < cutoff {
                    continue;
                }
                let file_len = meta.len();
                let fname = entry.file_name();
                let fname = fname.to_string_lossy();
                scan_antigravity_file_lines(
                    &path,
                    file_len,
                    file_secs,
                    &mut cursors,
                    |line_offset, line| {
                        if line.len() <= 64 * 1024 {
                            let line_str = String::from_utf8_lossy(line);
                            if let Some((line_ts, body)) =
                                parse_antigravity_log_line(&line_str, file_secs)
                            {
                                if line_ts >= cutoff {
                                    let key = format!(
                                        "logfile:{fname}:{line_offset}:{}",
                                        &storage::digest(body.as_bytes())[..16]
                                    );
                                    let r = Record::antigravity_error(&key, line_ts, None, body);
                                    discovered.push(r);
                                }
                            }
                        }
                    },
                );
            }
        }
        drop(cursors);
        if !discovered.is_empty() {
            let mut guard = self.journal.lock().unwrap();
            let mut added = false;
            for r in discovered {
                if !guard.records.iter().any(|existing| existing.id == r.id) {
                    guard.records.push_back(Arc::new(r));
                    added = true;
                }
            }
            if added {
                guard.dirty = true;
                let _ = self.sender.try_send(Job::Prune);
            }
        }
    }
    pub fn detail(&self, id: &str) -> Option<ClassifiedRecord> {
        let journal = self.journal.lock().unwrap();
        let cutoff = now().saturating_sub(RETENTION);
        let causes = journal.circuit_causes(cutoff);
        let target = journal
            .aliases
            .iter()
            .rev()
            .find(|(alias, _)| alias == id)
            .map(|(_, target)| target.as_str())
            .unwrap_or(id);
        journal
            .records
            .iter()
            .find(|r| r.visible() && r.id == target && r.last_at >= cutoff)
            .map(|r| ClassifiedRecord {
                origin: r.origin(),
                record: (**r).clone(),
                category: classify(r, &causes),
            })
    }
    pub fn raw(&self, id: &str) -> Result<Option<crate::event_raw::View>> {
        let reference = self
            .detail(id)
            .and_then(|record| record.record.details.raw_error);
        reference
            .map(|reference| crate::event_raw::read(&self.raw_directory, &reference.id))
            .transpose()
    }
    pub async fn clear(&self) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.sender
            .try_send(Job::Clear(tx))
            .map_err(|_| AppError::new(code::LOG_BUSY, "日志繁忙，请重试"))?;
        rx.await
            .map_err(|_| AppError::new(code::LOG_WRITE, "日志清空失败"))?
    }
}

fn prune_raw(directory: &Path, journal: &Journal) -> std::io::Result<()> {
    prune_raw_ids(directory, &journal.retained_raw_ids())
}

fn prune_raw_ids(directory: &Path, retained: &HashSet<String>) -> std::io::Result<()> {
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let Some(id) = name.to_str().and_then(|name| name.strip_suffix(".error")) else {
            continue;
        };
        if uuid::Uuid::parse_str(id).is_ok()
            && !retained.contains(id)
            && entry.file_type()?.is_file()
        {
            std::fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "events_categories_tests.rs"]
mod v0232_tests;

#[cfg(test)]
mod v016_tests {
    use super::*;
    #[test]
    fn retired_cli_journal_identity_is_readable_but_not_executable() {
        let client: JournalClient = serde_json::from_str(r#""agy""#).unwrap();
        assert_eq!(client.active(), None);
        assert_eq!(serde_json::to_string(&client).unwrap(), r#""agy""#);
        assert!(serde_json::from_str::<ClientId>(r#""agy""#).is_err());
    }
    #[test]
    fn configuration_diagnostics_preserve_actual_codes_and_safe_causes() {
        for code in [
            code::CONFLICT,
            code::RESUME_CONFLICT,
            code::CLAUDE_PROFILE_CONFLICT,
            "CONFIG_CHANGED",
            "OFFICIAL_CONFLICT",
        ] {
            let error = AppError::new(
                code,
                "受管字段被外部修改 settings.json https://example.invalid/v1 \"model\"",
            );
            let record = Record::configuration_failure(ClientId::Dsh, &error, true).unwrap();
            assert_eq!(record.reason, Reason::ConfigConflict);
            assert_eq!(record.details.local_code.as_deref(), Some(code));
            let message = record.details.message.unwrap();
            assert_eq!(
                message,
                "受管字段被外部修改 settings.json https://example.invalid/v1 \"model\""
            );
        }
        for code in [code::RECOVERY, "GUARDED", "GATEWAY_ACTIVE", "IO"] {
            let error = AppError::new(code, "恢复未完成");
            assert!(Record::configuration_failure(ClientId::Dsh, &error, false).is_none());
            let record = Record::configuration_failure(ClientId::Dsh, &error, true).unwrap();
            assert_eq!(record.reason, Reason::StartupRecovery);
            assert_eq!(record.details.local_code.as_deref(), Some(code));
            assert_eq!(record.details.message.as_deref(), Some("恢复未完成"));
        }
    }
    #[test]
    fn distinct_error_codes_statuses_and_actions_do_not_merge() {
        let mut a = Record::new(
            None,
            None,
            Some("fixture-model"),
            Reason::ModelUnavailable,
            Action::TryingNext,
            Some(404),
            Some(1),
        );
        a.details.upstream_code = Some("model_not_found".into());
        a.details.counted_failure = Some(false);
        let mut b = a.clone();
        assert!(a.same(&b));
        b.status = Some(403);
        assert!(!a.same(&b));
        b = a.clone();
        b.details.upstream_code = Some("unsupported_model".into());
        assert!(!a.same(&b));
        b = a.clone();
        b.action = Action::Returned;
        assert!(!a.same(&b));
        b = a.clone();
        b.details.ws_close_code = Some(1013);
        assert!(!a.same(&b));
    }
    #[test]
    fn legacy_records_read_without_details_and_recoveries_are_hidden() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("diagnostics.jsonl");
        let a = Record::new(
            None,
            None,
            None,
            Reason::ModelUnavailable,
            Action::Returned,
            Some(200),
            None,
        );
        let mut old = serde_json::to_value(&a).unwrap();
        old.as_object_mut().unwrap().remove("details");
        let recovered = Record::new(
            None,
            None,
            None,
            Reason::Recovered,
            Action::Recovered,
            None,
            None,
        );
        std::fs::write(
            &path,
            format!("{}\n{}\n", old, serde_json::to_string(&recovered).unwrap()),
        )
        .unwrap();
        let loaded = load(&path);
        assert!(loaded.error.is_none());
        assert_eq!(loaded.records.len(), 1);
        assert_eq!(loaded.records[0].status, Some(200));
        assert_eq!(loaded.records[0].details, Details::default());
    }
}
#[cfg(test)]
#[path = "events_v02512_tests.rs"]
mod v02512_tests;

#[cfg(test)]
mod routing_visibility_tests {
    use super::*;
    use crate::usage::model::{RoutingDecision, RoutingEvidence};

    fn persist(j: &mut Journal, path: &Path, changed: bool) -> Result<()> {
        if let Some(snapshot) = j.prepare_rewrite(path, changed) {
            write_records_stream(path, &snapshot)?;
            j.dirty = false;
        }
        Ok(())
    }

    #[test]
    fn loaded_retention_and_sanitization_remain_dirty_until_persisted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("diagnostics.jsonl");
        let mut keep = Record::new(
            None,
            None,
            None,
            Reason::AccountSync,
            Action::Stopped,
            None,
            None,
        );
        keep.details.custom_rule_id = Some("invalid-fixture-id".into());
        let mut expired = keep.clone();
        expired.id = uuid::Uuid::new_v4().to_string();
        expired.last_at = now() - RETENTION - 1;
        let lines = format!(
            "{}\n{}\n",
            serde_json::to_string(&keep).unwrap(),
            serde_json::to_string(&expired).unwrap()
        );
        std::fs::write(&path, lines).unwrap();
        let mut journal = load(&path);
        assert!(journal.error.is_none());
        assert_eq!(journal.records.len(), 1);
        assert!(journal.needs_write(&path));
        persist(&mut journal, &path, false).unwrap();
        assert!(!journal.needs_write(&path));
        assert!(!load(&path).dirty);
        assert!(!std::fs::read_to_string(&path)
            .unwrap()
            .contains("invalid-fixture-id"));
        std::fs::write(&path, "invalid fixture\n").unwrap();
        let mut damaged = load(&path);
        assert!(damaged.error.is_some());
        assert!(damaged.needs_write(&path));
        persist(&mut damaged, &path, false).unwrap();
        assert!(damaged.error.is_some());
        assert!(!damaged.needs_write(&path));
        assert!(std::fs::read(&path).unwrap().is_empty());
    }

    #[test]
    fn unchanged_retention_keeps_file_and_expired_records_are_removed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("diagnostics.jsonl");
        let mut journal = Journal::default();
        journal.records.push_back(Arc::new(Record::new(
            None,
            None,
            None,
            Reason::AccountSync,
            Action::Stopped,
            None,
            None,
        )));
        persist(&mut journal, &path, true).unwrap();
        let old_time = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1);
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(old_time)
            .unwrap();
        persist(&mut journal, &path, false).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            old_time
        );
        Arc::make_mut(&mut journal.records[0]).last_at = now() - RETENTION - 1;
        persist(&mut journal, &path, false).unwrap();
        assert!(journal.records.is_empty());
        assert_eq!(std::fs::read(&path).unwrap(), b"");
    }

    #[test]
    fn routine_routing_is_hidden_on_emit_and_legacy_load_but_faults_remain() {
        let dir = tempfile::tempdir().unwrap();
        let service = Service::new(dir.path());
        let mut lines = Vec::new();
        for outcome in [
            RoutingOutcome::OwnerPreserved,
            RoutingOutcome::WindowConsumed,
            RoutingOutcome::BoundaryRegistered,
            RoutingOutcome::Failed,
            RoutingOutcome::OwnershipSaved,
        ] {
            let mut record = Record::new(
                None,
                None,
                None,
                Reason::ThreadRouting,
                Action::Returned,
                Some(200),
                None,
            );
            record.details.routing = Some(RoutingDetails {
                decision: RoutingDecision::BoundaryConsumed,
                evidence: RoutingEvidence::default(),
                outcome,
                attempt_id: None,
                failure: None,
            });
            assert!(!service.emit_checked(record.clone()));
            lines.push(serde_json::to_string(&record).unwrap());
            record.details.routing.as_mut().unwrap().failure =
                Some(crate::gateway::failover::Failure::Transport);
            assert!(record.visible());
            lines.push(serde_json::to_string(&record).unwrap());
        }
        assert_eq!(service.query(Filter::default()).total, 0);
        let path = dir.path().join("legacy.jsonl");
        std::fs::write(&path, lines.join("\n")).unwrap();
        let loaded = load(&path);
        assert_eq!(loaded.records.len(), 5);
        assert!(loaded.records.iter().all(|r| r
            .details
            .routing
            .as_ref()
            .unwrap()
            .failure
            .is_some()));
        let service = Service::new(dir.path());
        *service.journal.lock().unwrap() = loaded;
        assert_eq!(service.query(Filter::default()).total, 5);
        // Query/count also defend against routine records still held in memory.
        let mut routine = (*service.journal.lock().unwrap().records[0]).clone();
        routine.id = uuid::Uuid::new_v4().to_string();
        routine.details.routing.as_mut().unwrap().failure = None;
        let id = routine.id.clone();
        service
            .journal
            .lock()
            .unwrap()
            .records
            .push_back(Arc::new(routine));
        assert_eq!(
            service
                .query(Filter {
                    reason: Some(Reason::ThreadRouting),
                    ..Default::default()
                })
                .total,
            5
        );
        assert!(service.detail(&id).is_none());
    }

    #[tokio::test]
    async fn incremental_append_adds_new_lines_and_merge_rewrites_without_duplicates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("diagnostics.jsonl");
        let service = Service::new(dir.path());
        let mut rx = service.subscribe();

        let first = Record::new(
            Some(ClientId::Codex),
            None,
            Some("fixture-model-a"),
            Reason::RateLimit,
            Action::TryingNext,
            Some(429),
            Some(1),
        );
        service.emit(first);
        rx.recv().await.unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), 1);

        service.journal.lock().unwrap().error = Some("fixture attachment warning".into());
        // Keep a handle to the old file: append updates it, atomic replacement does not.
        let mut original = std::fs::File::open(&path).unwrap();

        let second = Record::new(
            Some(ClientId::Codex),
            None,
            Some("fixture-model-b"),
            Reason::UpstreamService,
            Action::Returned,
            Some(502),
            Some(1),
        );
        service.emit(second.clone());
        rx.recv().await.unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), 2);
        let mut appended = String::new();
        std::io::Read::read_to_string(&mut original, &mut appended).unwrap();
        assert_eq!(appended.lines().count(), 2);
        assert!(service.query(Filter::default()).error.is_some());

        let mut repeated = second;
        repeated.id = uuid::Uuid::new_v4().to_string();
        service.emit(repeated);
        let merged = rx.recv().await.unwrap().record.unwrap();
        assert_eq!(merged.count, 2);
        let reloaded = load(&path);
        assert!(reloaded.error.is_none());
        assert_eq!(reloaded.records.len(), 2);
        assert_eq!(reloaded.records[1].count, 2);
    }

    #[tokio::test]
    async fn clear_write_failure_preserves_in_memory_records_and_merged_aliases() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("diagnostics.jsonl");
        let service = Service::new(dir.path());
        let mut rx = service.subscribe();

        let base = Record::new(
            Some(ClientId::Codex),
            None,
            Some("fixture-model"),
            Reason::UpstreamService,
            Action::Returned,
            Some(502),
            Some(1),
        );
        let canonical_id = base.id.clone();
        service.emit(base.clone());
        rx.recv().await.unwrap();

        let mut dup = base;
        let alias_id = uuid::Uuid::new_v4().to_string();
        dup.id = alias_id.clone();
        service.emit(dup);
        rx.recv().await.unwrap();
        assert_eq!(service.detail(&alias_id).unwrap().record.id, canonical_id);

        // Replace diagnostics.jsonl with a directory so atomic rename fails during Clear.
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("blocker"), b"x").unwrap();

        assert!(service.clear().await.is_err());
        assert_eq!(service.query(Filter::default()).total, 1);
        assert_eq!(service.detail(&alias_id).unwrap().record.id, canonical_id);

        // Restore path and confirm normal clear empties records and aliases.
        std::fs::remove_dir_all(&path).unwrap();
        service.clear().await.unwrap();
        assert_eq!(service.query(Filter::default()).total, 0);
        assert!(service.detail(&alias_id).is_none());
    }

    #[test]
    fn antigravity_log_scan_preserves_unfinished_half_line_and_seeks_tail_over_4mb() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let service = Service::new(dir.path());
        let log_dir = dir
            .path()
            .join(".gemini/antigravity/brain/conv-1/.system_generated/logs");
        std::fs::create_dir_all(&log_dir).unwrap();
        let transcript = log_dir.join("transcript.jsonl");

        // 1) Half-written line should not advance offset or lose the event once completed.
        let part1 = br#"{"step_index":1,"type":"ERROR_MESSAGE","errorMessage":"429 Too Many "#;
        let part2 = b"Requests from upstream\"}\n";
        std::fs::write(&transcript, part1).unwrap();
        service.sync_antigravity_logs(dir.path());
        assert_eq!(service.query(Filter::default()).total, 0);

        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&transcript)
            .unwrap();
        f.write_all(part2).unwrap();
        drop(f);
        service.sync_antigravity_logs(dir.path());
        assert_eq!(service.query(Filter::default()).total, 1);

        // 2) Initial scan of >4MB file seeks to tail (file_len - 4MB) and skips early errors.
        let log_dir2 = dir.path().join(".gemini/antigravity/logs");
        std::fs::create_dir_all(&log_dir2).unwrap();
        let big_log = log_dir2.join("language_server.log");
        let mut big = std::fs::File::create(&big_log).unwrap();
        writeln!(big, "[ERROR] 401 Unauthorized early error").unwrap();
        let filler = vec![b'x'; 1024];
        for _ in 0..(4 * 1024 + 64) {
            big.write_all(&filler).unwrap();
            big.write_all(b"\n").unwrap();
        }
        writeln!(big, "[ERROR] 503 Service Unavailable tail error").unwrap();
        drop(big);

        service.sync_antigravity_logs(dir.path());
        let page = service.query(Filter::default());
        assert_eq!(page.total, 2);
        assert!(page
            .items
            .iter()
            .any(|r| r.reason == Reason::UpstreamService));
        assert!(!page
            .items
            .iter()
            .any(|r| r.reason == Reason::Authentication));
    }

    #[test]
    fn load_journal_retains_antigravity_log_events_and_purges_legacy_gateway_antigravity_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("diagnostics.jsonl");
        let mut legacy = Record::new(
            None,
            None,
            Some("gemini-2.5-pro"),
            Reason::RateLimit,
            Action::Returned,
            Some(429),
            Some(1),
        );
        legacy.client_id = Some(JournalClient::Retired(RetiredClient::Antigravity));
        let log_record = Record::antigravity_error(
            "transcript:conv-1:1",
            now(),
            Some("gemini-2.5-pro"),
            "429 Too Many Requests for \"gemini-2.5-pro\" at https://example.invalid/v1/models",
        );
        assert!(log_record.details.upstream_type.is_none());
        assert_eq!(
            log_record.details.message.as_deref(),
            Some(
                "429 Too Many Requests for \"gemini-2.5-pro\" at https://example.invalid/v1/models"
            )
        );
        let timeout_record = Record::antigravity_error(
            "transcript:conv-1:3",
            now(),
            Some("gemini-2.5-pro"),
            "Error: stream was interrupted after 5m0s",
        );
        assert_eq!(
            timeout_record.details.upstream_code.as_deref(),
            Some("STREAM_TIMEOUT")
        );
        let mut old_redacted = Record::antigravity_error(
            "transcript:conv-1:2",
            now(),
            Some("gemini-2.5-pro"),
            "429 Too Many Requests",
        );
        old_redacted.details.upstream_type = Some("antigravity_log".into());
        old_redacted.details.message = Some("429 Too Many Requests [已隐去]".into());
        let content = format!(
            "{}\n{}\n{}\n{}\n",
            serde_json::to_string(&legacy).unwrap(),
            serde_json::to_string(&old_redacted).unwrap(),
            serde_json::to_string(&log_record).unwrap(),
            serde_json::to_string(&timeout_record).unwrap()
        );
        std::fs::write(&path, content).unwrap();

        let loaded = load(&path);
        assert!(loaded.dirty);
        assert_eq!(loaded.records.len(), 2);
        assert_eq!(loaded.records[0].id, log_record.id);
        assert_eq!(loaded.records[1].id, timeout_record.id);
        assert!(loaded.records[0].details.upstream_type.is_none());
    }
}
