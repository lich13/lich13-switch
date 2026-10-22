//! Exception-only journal. Only bounded, sanitized error details cross this boundary.
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
    path::Path,
    sync::{mpsc, Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::sync::{broadcast, oneshot};
const LIMIT: u64 = 5 * 1024 * 1024;
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
/// Query-only classification. Persisted records keep their original schema.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClassifiedRecord {
    #[serde(flatten)]
    pub record: Record,
    pub category: Category,
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
    pub client_id: Option<ClientId>,
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
            client_id,
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
                .filter(|s| !s.contains("[已隐去]"))
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
                self.client_id.map(ClientId::name).unwrap_or("应用"),
                direct.join(" · ")
            );
        }
        if let Some(message) = &self.notification_message {
            return message.clone();
        }
        format!(
            "{} · {}{}：{}；{}",
            self.client_id.map(ClientId::name).unwrap_or("应用"),
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
        let mut details = self.details.clone();
        let mut other_details = other.details.clone();
        details.last_progress_ms = None;
        other_details.last_progress_ms = None;
        details.delivery = None;
        other_details.delivery = None;
        self.client_id == other.client_id
            && self.provider_id == other.provider_id
            && self.model == other.model
            && self.reason == other.reason
            && self.status == other.status
            && self.action == other.action
            && details == other_details
    }
}
fn safe_id(s: &str) -> Option<String> {
    uuid::Uuid::parse_str(s).ok().map(|v| v.to_string())
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
#[derive(Default, Clone)]
struct Journal {
    records: VecDeque<Record>,
    // Runtime health can refer to a just-merged event. Keep bounded aliases for
    // those links; persisted terminal events use the canonical row ID.
    aliases: VecDeque<(String, String)>,
    error: Option<String>,
}
enum Job {
    Append(Box<Record>),
    Clear(oneshot::Sender<Result<()>>),
    Prune,
    NotificationAccepted(String),
}
#[derive(Clone)]
pub struct Service {
    sender: mpsc::SyncSender<Job>,
    journal: Arc<Mutex<Journal>>,
    events: broadcast::Sender<Change>,
}
#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Filter {
    pub from: Option<u64>,
    pub to: Option<u64>,
    pub client_id: Option<ClientId>,
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
    match storage::read_bounded(path, LIMIT) {
        Ok(Some(bytes)) => {
            for line in bytes.split(|b| *b == b'\n').filter(|b| !b.is_empty()) {
                match serde_json::from_slice::<Record>(line) {
                    Ok(mut r)
                        if uuid::Uuid::parse_str(&r.id).is_ok()
                            && r.provider_id
                                .as_deref()
                                .is_none_or(|s| safe_id(s).is_some())
                            && r.model.as_deref().is_none_or(|s| safe_model(s).is_some())
                            && r.last_at <= now().saturating_add(60) =>
                    {
                        r.details = r.details.sanitized();
                        if r.visible() && r.last_at >= now().saturating_sub(RETENTION) {
                            j.records.push_back(r)
                        }
                    }
                    _ => j.error = Some("日志包含损坏记录，已跳过；其余记录仍可查看".into()),
                }
            }
        }
        Ok(None) => (),
        Err(_) => j.error = Some("日志读取失败，请检查文件权限与空间".into()),
    }
    j
}
fn persist(j: &mut Journal, path: &Path) -> Result<()> {
    let cutoff = now().saturating_sub(RETENTION);
    j.records.retain(|r| r.last_at >= cutoff);
    let mut lines: VecDeque<Vec<u8>> = j
        .records
        .iter()
        .filter_map(|r| serde_json::to_vec(r).ok())
        .collect();
    let mut size: usize = lines.iter().map(|l| l.len() + 1).sum();
    while size > LIMIT as usize {
        if let Some(l) = lines.pop_front() {
            size -= l.len() + 1;
            j.records.pop_front();
        } else {
            break;
        }
    }
    let retained: std::collections::HashSet<_> = j.records.iter().map(|r| r.id.as_str()).collect();
    j.aliases
        .retain(|(_, target)| retained.contains(target.as_str()));
    let mut bytes = Vec::with_capacity(size);
    for line in lines {
        bytes.extend(line);
        bytes.push(b'\n');
    }
    storage::atomic_write_bounded(path, &bytes, None, LIMIT)
}
impl Service {
    pub fn new(data: &Path) -> Self {
        let path = data.join("diagnostics.jsonl");
        let journal = Arc::new(Mutex::new(load(&path)));
        let (events, _) = broadcast::channel(128);
        let (sender, receiver) = mpsc::sync_channel(256);
        let j = journal.clone();
        let events_copy = events.clone();
        std::thread::spawn(move || loop {
            let job = match receiver.recv_timeout(std::time::Duration::from_secs(3600)) {
                Ok(v) => v,
                Err(mpsc::RecvTimeoutError::Timeout) => Job::Prune,
                Err(_) => break,
            };
            let previous = j.lock().unwrap().clone();
            let mut journal = previous.clone();
            let mut changed = None;
            let mut notify = false;
            let mut done = None;
            match job {
                Job::Append(r) => {
                    let mut r = *r;
                    if let Some((_, target)) = journal
                        .aliases
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
                            .is_none_or(|id| !journal.records.iter().any(|old| &old.id == id))
                    {
                        // A repeated cause may already have been merged into an earlier row.
                        r.details.cause_id = journal
                            .records
                            .iter()
                            .rev()
                            .find(|old| {
                                !matches!(
                                    old.reason,
                                    Reason::CircuitOpen | Reason::FailoverExhausted
                                ) && (r.reason != Reason::CircuitOpen
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
                    notify = r.reason.notifiable(r.action);
                    if let Some(old) = journal.records.iter_mut().rev().find(|old| {
                        old.same(&r) && r.last_at.saturating_sub(old.first_at) < r.merge_seconds()
                    }) {
                        old.last_at = r.last_at;
                        old.count = old.count.saturating_add(1);
                        old.status = r.status;
                        old.action = r.action;
                        old.attempt = r.attempt;
                        if r.reason == Reason::RequestInterrupted {
                            old.details.delivery = r.details.delivery;
                            old.details.last_progress_ms = r.details.last_progress_ms;
                        }
                        changed = Some(old.clone());
                        let target = old.id.clone();
                        journal.aliases.push_back((r.id, target));
                        while journal.aliases.len() > 4096 {
                            journal.aliases.pop_front();
                        }
                    } else {
                        changed = Some(r.clone());
                        journal.records.push_back(r);
                    }
                }
                Job::Clear(tx) => {
                    journal.records.clear();
                    journal.aliases.clear();
                    journal.error = None;
                    done = Some(tx);
                }
                Job::NotificationAccepted(id) => {
                    if let Some(r) = journal.records.iter_mut().find(|r| r.id == id) {
                        r.notified_at = now();
                        changed = Some(r.clone());
                    }
                }
                Job::Prune => (),
            }
            let result = persist(&mut journal, &path);
            if result.is_err() {
                if done.is_some() {
                    journal.records = previous.records;
                    journal.aliases = previous.aliases;
                }
                journal.error = Some("日志写入失败，请检查文件权限与空间".into());
            }
            *j.lock().unwrap() = journal.clone();
            let _ = events_copy.send(Change {
                record: changed,
                error: journal.error.clone(),
                notify,
            });
            if let Some(tx) = done {
                let _ = tx.send(result);
            }
        });
        Self {
            sender,
            journal,
            events,
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
            .map(|r| ClassifiedRecord {
                record: r.clone(),
                category: classify(r, &causes),
            })
            .collect();
        rows.sort_by_key(|r| std::cmp::Reverse(r.last_at));
        let total = rows.len();
        let page = f.page.unwrap_or(1).max(1).min(total.div_ceil(50).max(1));
        Page {
            items: rows.into_iter().skip((page - 1) * 50).take(50).collect(),
            total,
            page,
            error: j.error.clone(),
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
                record: r.clone(),
                category: classify(r, &causes),
            })
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

#[cfg(test)]
#[path = "events_categories_tests.rs"]
mod v0232_tests;

#[cfg(test)]
mod v016_tests {
    use super::*;
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
        let mut routine = service.journal.lock().unwrap().records[0].clone();
        routine.id = uuid::Uuid::new_v4().to_string();
        routine.details.routing.as_mut().unwrap().failure = None;
        let id = routine.id.clone();
        service.journal.lock().unwrap().records.push_back(routine);
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
}
