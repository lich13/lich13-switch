//! Exception-only journal. No upstream text, URLs, credentials or account data cross this boundary.
use crate::{
    gateway::ClientId,
    storage::{self, AppError, Result},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
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
}
impl Reason {
    pub fn text(self) -> &'static str {
        match self {
            Self::ModelUnavailable => "指定模型不存在或无权使用",
            Self::Authentication => "上游认证失败",
            Self::UpstreamService => "上游服务异常",
            Self::Network => "上游连接失败",
            Self::RateLimit => "上游限流",
            Self::Capacity => "模型容量不足",
            Self::FailoverExhausted => "没有可用的替代供应商",
            Self::Failover => "自动换商",
            Self::CircuitOpen => "供应商已熔断",
            Self::Recovered => "供应商已恢复",
            Self::ConfigConflict => "配置发生冲突",
            Self::StartupRecovery => "启动恢复失败",
            Self::AccountSync => "账号同步异常",
        }
    }
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    TryingNext,
    Returned,
    Waiting,
    Stopped,
    Recovered,
    Routed,
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
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Record {
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
    pub notified_at: u64,
}
impl Record {
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
            id: uuid::Uuid::new_v4().to_string(),
            first_at: t,
            last_at: t,
            count: 1,
            client_id,
            provider_id: provider_id.and_then(safe_id),
            model: model.and_then(safe_model),
            reason,
            action,
            level: if matches!(reason, Reason::Recovered | Reason::Failover) {
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
            notified_at: 0,
        }
    }
    pub fn notification(&self) -> String {
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
        self.client_id == other.client_id
            && self.provider_id == other.provider_id
            && self.model == other.model
            && self.reason == other.reason
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
    error: Option<String>,
}
enum Job {
    Append(Record),
    Clear(oneshot::Sender<Result<()>>),
    Prune,
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
    pub reason: Option<Reason>,
    pub page: Option<usize>,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Page {
    pub items: Vec<Record>,
    pub total: usize,
    pub page: usize,
    pub error: Option<String>,
}
fn load(path: &Path) -> Journal {
    let mut j = Journal::default();
    match storage::read_bounded(path, LIMIT) {
        Ok(Some(bytes)) => {
            for line in bytes.split(|b| *b == b'\n').filter(|b| !b.is_empty()) {
                match serde_json::from_slice::<Record>(line) {
                    Ok(r)
                        if uuid::Uuid::parse_str(&r.id).is_ok()
                            && r.provider_id
                                .as_deref()
                                .is_none_or(|s| safe_id(s).is_some())
                            && r.model.as_deref().is_none_or(|s| safe_model(s).is_some())
                            && r.last_at <= now().saturating_add(60) =>
                    {
                        if r.last_at >= now().saturating_sub(RETENTION) {
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
                Job::Append(mut r) => {
                    notify = !matches!(r.reason, Reason::Recovered | Reason::Failover);
                    if let Some(old) =
                        journal.records.iter_mut().rev().find(|old| {
                            old.same(&r) && r.last_at.saturating_sub(old.first_at) < MERGE
                        })
                    {
                        notify &= r.last_at.saturating_sub(old.notified_at) >= MERGE;
                        old.last_at = r.last_at;
                        old.count = old.count.saturating_add(1);
                        old.status = r.status;
                        old.action = r.action;
                        old.attempt = r.attempt;
                        if notify {
                            old.notified_at = r.last_at;
                        }
                        changed = Some(old.clone());
                    } else {
                        if notify {
                            r.notified_at = r.last_at;
                        }
                        changed = Some(r.clone());
                        journal.records.push_back(r);
                    }
                }
                Job::Clear(tx) => {
                    journal.records.clear();
                    journal.error = None;
                    done = Some(tx);
                }
                Job::Prune => (),
            }
            let result = persist(&mut journal, &path);
            if result.is_err() {
                if done.is_some() {
                    journal.records = previous.records;
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
        if self.sender.try_send(Job::Append(r)).is_err() {
            let mut j = self.journal.lock().unwrap();
            j.error = Some("日志队列已满，部分事件未记录".into());
            let _ = self.events.send(Change {
                record: None,
                error: j.error.clone(),
                notify: false,
            });
        }
    }
    pub fn query(&self, f: Filter) -> Page {
        let j = self.journal.lock().unwrap();
        let cutoff = now().saturating_sub(RETENTION);
        let mut rows: Vec<_> = j
            .records
            .iter()
            .filter(|r| {
                r.last_at >= cutoff
                    && f.from.is_none_or(|t| r.last_at >= t)
                    && f.to.is_none_or(|t| r.first_at <= t)
                    && f.client_id.is_none_or(|v| r.client_id == Some(v))
                    && f.provider_id
                        .as_ref()
                        .is_none_or(|v| r.provider_id.as_ref() == Some(v))
                    && f.level.is_none_or(|v| r.level == v)
                    && f.reason.is_none_or(|v| r.reason == v)
            })
            .cloned()
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
    pub fn detail(&self, id: &str) -> Option<Record> {
        self.journal
            .lock()
            .unwrap()
            .records
            .iter()
            .find(|r| r.id == id && r.last_at >= now().saturating_sub(RETENTION))
            .cloned()
    }
    pub async fn clear(&self) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.sender
            .try_send(Job::Clear(tx))
            .map_err(|_| AppError::new("LOG_BUSY", "日志繁忙，请重试"))?;
        rx.await
            .map_err(|_| AppError::new("LOG_WRITE", "日志清空失败"))?
    }
}
