//! Direct provider diagnostics. Only summaries/configuration are persisted;
//! credentials, prompts, samples and media remain in this process.
mod bank;
pub mod commands;
mod compat;
mod connection;
mod downgrade;
mod fingerprint;
mod fingerprint_stream;
mod modeltrace;
mod scheduler;
pub mod types;

use crate::{
    gateway::{testing::Target, ClientId, Gateway},
    storage::{self, AppError, Result},
};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Instant,
};
use tokio::sync::{broadcast, watch, Semaphore};
use types::*;

pub const DEFAULT_DETECTION_MODEL: &str = "gpt-6-luna";
const STATE_LIMIT: u64 = 32 * 1024 * 1024;
fn failure(message: &str) -> AppError {
    AppError::new(crate::errors::code::PROVIDER_TEST, message)
}
fn now() -> i64 {
    crate::usage::model::now()
}
fn config_revision(saved: &Persisted) -> String {
    storage::digest(
        json!({"detection":saved.detection,"degraded":saved.degraded})
            .to_string()
            .as_bytes(),
    )
}
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}
#[derive(Clone)]
struct Context {
    request: Request,
    target: Target,
    challenges: Vec<(usize, String)>,
    bank: (Value, Value),
}
struct Data {
    saved: Persisted,
    revision: String,
    outputs: HashMap<String, Output>,
    contexts: HashMap<String, Context>,
    cancel: HashMap<String, watch::Sender<bool>>,
    models: HashMap<String, (Instant, Models)>,
    detection_status: HashMap<String, DetectionStatus>,
    anchors: HashMap<String, i64>,
    error: Option<String>,
    bank_updating: bool,
}
struct Shared {
    path: PathBuf,
    data: Mutex<Data>,
    gateways: [Gateway; 4],
    events: broadcast::Sender<()>,
    jobs: Arc<Semaphore>,
}
#[derive(Clone)]
pub struct Service(Arc<Shared>);
impl Service {
    pub fn new(directory: &Path, gateways: [Gateway; 4]) -> Result<Self> {
        let path = directory.join("provider-tests.json");
        let raw = storage::read_bounded(&path, STATE_LIMIT)?;
        let mut value: Option<Value> = raw
            .as_deref()
            .map(serde_json::from_slice)
            .transpose()
            .map_err(|_| failure("测试状态无法读取，已保留原文件"))?;
        let retired_schedules = value
            .as_mut()
            .and_then(Value::as_object_mut)
            .is_some_and(|object| object.remove("schedules").is_some());
        let mut saved: Persisted = value
            .map(serde_json::from_value)
            .transpose()
            .map_err(|_| failure("测试状态无法读取，已保留原文件"))?
            .unwrap_or_default();
        if saved.schema != 1 {
            return Err(failure("测试状态来自更高版本，请升级应用"));
        }
        let mut changed = retired_schedules;
        for job in &mut saved.jobs {
            if !job.terminal() {
                job.status = "interrupted".into();
                job.completed_at = Some(now());
                job.error = Some("应用退出时结果未知，未自动重放，请手动重新测试".into());
                changed = true;
            }
            if job.can_retry {
                job.can_retry = false;
                changed = true;
            }
        }
        let mut revision = storage::revision(raw.as_deref());
        if changed {
            let bytes = serde_json::to_vec_pretty(&saved).map_err(|_| failure("测试状态无效"))?;
            storage::atomic_write_bounded(&path, &bytes, Some(&revision), STATE_LIMIT)?;
            revision = storage::digest(&bytes);
        }
        let (events, _) = broadcast::channel(32);
        Ok(Self(Arc::new(Shared {
            path,
            gateways,
            events,
            jobs: Arc::new(Semaphore::new(2)),
            data: Mutex::new(Data {
                saved,
                revision,
                outputs: HashMap::new(),
                contexts: HashMap::new(),
                cancel: HashMap::new(),
                models: HashMap::new(),
                detection_status: HashMap::new(),
                anchors: HashMap::new(),
                error: None,
                bank_updating: false,
            }),
        })))
    }
    fn gateway(&self, client: ClientId) -> &Gateway {
        self.0
            .gateways
            .iter()
            .find(|gateway| gateway.client_id() == client)
            .expect("four clients")
    }
    pub fn subscribe(&self) -> broadcast::Receiver<()> {
        self.0.events.subscribe()
    }
    fn changed(&self) {
        let _ = self.0.events.send(());
    }
    fn save(&self, data: &mut Data, mut next: Persisted) -> Result<()> {
        let terminal = next.jobs.iter().filter(|j| j.terminal()).count();
        let mut remove = terminal.saturating_sub(1000);
        next.jobs.retain(|job| {
            if remove > 0 && job.terminal() {
                remove -= 1;
                false
            } else {
                true
            }
        });
        if next.history.len() > 500 {
            next.history.drain(..next.history.len() - 500);
        }
        let bytes = serde_json::to_vec_pretty(&next).map_err(|_| failure("测试状态无法保存"))?;
        storage::atomic_write_bounded(&self.0.path, &bytes, Some(&data.revision), STATE_LIMIT)?;
        data.revision = storage::digest(&bytes);
        data.saved = next;
        data.outputs
            .retain(|id, _| data.saved.jobs.iter().any(|j| &j.id == id));
        data.contexts
            .retain(|id, _| data.saved.jobs.iter().any(|j| &j.id == id));
        Ok(())
    }
    pub fn state(&self) -> State {
        let data = lock(&self.0.data);
        State {
            revision: config_revision(&data.saved),
            jobs: data.saved.jobs.iter().rev().cloned().collect(),
            detection: data.saved.detection.clone(),
            degraded: data.saved.degraded.clone(),
            detection_status: data.detection_status.values().cloned().collect(),
            history: data.saved.history.iter().rev().take(50).cloned().collect(),
            bank: bank::status(&data.saved.bank, data.bank_updating),
            error: data.error.clone(),
        }
    }
    pub fn detail(&self, id: &str) -> Result<Detail> {
        let data = lock(&self.0.data);
        let job = data
            .saved
            .jobs
            .iter()
            .find(|j| j.id == id)
            .or_else(|| data.saved.history.iter().find(|j| j.id == id))
            .cloned()
            .ok_or_else(|| failure("测试记录已不存在"))?;
        Ok(Detail {
            job,
            output: data.outputs.get(id).cloned(),
        })
    }
    fn validate(&self, request: &Request, target: &Target) -> Result<()> {
        if request.model.trim().is_empty()
            || request.model.len() > 256
            || request.model.chars().any(char::is_control)
            || !(1..=3).contains(&request.concurrency)
        {
            return Err(failure("请选择有效模型和 1–3 并发"));
        }
        if request.kind == Kind::Fingerprint
            && (request.client_id != ClientId::Codex || target.search_only)
        {
            return Err(failure("模型指纹检测仅用于普通 Codex 供应商"));
        }
        if !connection::text_model(&request.model) {
            return Err(failure("请选择普通对话模型"));
        }
        Ok(())
    }
    pub fn start(&self, request: Request) -> Result<Job> {
        self.start_job(request, false, None)
    }
    fn start_job(
        &self,
        mut request: Request,
        automatic: bool,
        batch: Option<String>,
    ) -> Result<Job> {
        let target = self
            .gateway(request.client_id)
            .test_target(&request.provider_id, Some(&request.expected_revision))?;
        if request.kind == Kind::Connection && request.model.is_empty() {
            request.model = self.default_connection_model(&target);
        }
        self.validate(&request, &target)?;
        let mut data = lock(&self.0.data);
        if data.saved.jobs.iter().any(|j| {
            !j.terminal()
                && j.client_id == request.client_id
                && j.provider_id == request.provider_id
        }) {
            return Err(failure("该供应商已有测试正在运行或排队"));
        }
        let snapshot = bank::snapshot(&data.saved.bank)?;
        let challenges = if request.kind == Kind::Fingerprint {
            modeltrace::challenges()
        } else {
            vec![]
        };
        let job = Job {
            id: uuid::Uuid::new_v4().to_string(),
            client_id: request.client_id,
            provider_id: target.provider_id.clone(),
            provider_name: target.provider_name.clone(),
            provider_version: target.version.clone(),
            kind: request.kind,
            mode: Mode::Text,
            requested_model: request.model.clone(),
            forwarded_model: target.model(&request.model),
            returned_model: None,
            status: "queued".into(),
            created_at: now(),
            started_at: None,
            completed_at: None,
            elapsed_ms: 0,
            first_byte_ms: None,
            error: None,
            report: None,
            groups: challenges
                .iter()
                .enumerate()
                .map(|(index, (expected, _))| Group {
                    index,
                    expected: *expected,
                    status: "pending".into(),
                    ..Default::default()
                })
                .collect(),
            automatic,
            batch_id: batch,
            can_retry: false,
            completion_reason: None,
        };
        let mut next = data.saved.clone();
        next.jobs.push(job.clone());
        self.save(&mut data, next)?;
        let (cancel, receiver) = watch::channel(false);
        data.cancel.insert(job.id.clone(), cancel);
        data.outputs.insert(job.id.clone(), Output::default());
        data.contexts.insert(
            job.id.clone(),
            Context {
                request,
                target,
                challenges,
                bank: snapshot,
            },
        );
        drop(data);
        self.launch(job.id.clone(), receiver);
        self.changed();
        Ok(job)
    }
    fn launch(&self, id: String, mut cancel: watch::Receiver<bool>) {
        let service = self.clone();
        tauri::async_runtime::spawn(async move {
            let started = Instant::now();
            let work = async {
                let _permit = service
                    .0
                    .jobs
                    .clone()
                    .acquire_owned()
                    .await
                    .map_err(|_| failure("测试调度已停止"))?;
                let context = lock(&service.0.data)
                    .contexts
                    .get(&id)
                    .cloned()
                    .ok_or_else(|| failure("测试上下文已失效"))?;
                service
                    .gateway(context.target.client_id)
                    .verify_test_version(&context.target.provider_id, &context.target.version)?;
                if service.detail(&id)?.job.automatic
                    && !service.automatic_eligible(&context.target.provider_id)?
                {
                    return Err(failure("供应商已退出检测队列，本轮已停止"));
                }
                service.update(&id, |job, _| {
                    job.status = "running".into();
                    job.started_at = Some(now());
                })?;
                if context.request.kind == Kind::Fingerprint {
                    service.run_fingerprint(&id, &context).await
                } else {
                    service.run_connection(&id, &context).await
                }
            };
            let outcome = tokio::select! { biased;
                _ = async { if !*cancel.borrow() { let _ = cancel.changed().await; } } => Err(failure("测试已停止，已派发请求的上游处理可能仍在继续")),
                result = work => result,
            };
            let cancelled = *cancel.borrow();
            service.finish(
                &id,
                outcome,
                cancelled,
                started.elapsed().as_millis() as u64,
            );
        });
    }
    fn update(&self, id: &str, change: impl FnOnce(&mut Job, &mut Output)) -> Result<()> {
        let mut data = lock(&self.0.data);
        let mut next = data.saved.clone();
        let job = next
            .jobs
            .iter_mut()
            .find(|j| j.id == id)
            .ok_or_else(|| failure("测试记录不存在"))?;
        let output = data.outputs.entry(id.into()).or_default();
        change(job, output);
        self.save(&mut data, next)?;
        drop(data);
        self.changed();
        Ok(())
    }
    fn progress(&self, id: &str, change: impl FnOnce(&mut Job, &mut Output)) {
        let mut data = lock(&self.0.data);
        let Data { saved, outputs, .. } = &mut *data;
        if let Some(job) = saved.jobs.iter_mut().find(|j| j.id == id) {
            change(job, outputs.entry(id.into()).or_default());
        }
        drop(data);
        self.changed();
    }
    fn finish(&self, id: &str, result: Result<()>, cancelled: bool, elapsed: u64) {
        let automatic = self.detail(id).is_ok_and(|d| d.job.automatic);
        if automatic && !cancelled && result.is_ok() {
            self.apply_degradation(id);
        }
        let result = self.update(id, |job, _| {
            job.status = if cancelled {
                "cancelled"
            } else if result.is_ok() {
                "completed"
            } else {
                "failed"
            }
            .into();
            job.error = result.err().map(|e| e.message);
            job.completed_at = Some(now());
            job.elapsed_ms = elapsed;
            for group in &mut job.groups {
                if matches!(group.status.as_str(), "running" | "pending") {
                    group.status = "cancelled".into();
                    group.retryable = false;
                    group.uncertain = group.attempts > 0;
                }
            }
            job.can_retry = !cancelled
                && !job.automatic
                && job
                    .groups
                    .iter()
                    .any(|g| g.status == "failed" && g.retryable && !g.uncertain);
        });
        let mut data = lock(&self.0.data);
        data.cancel.remove(id);
        if let Err(error) = result {
            data.error = Some(error.message);
        }
        if automatic {
            if let Some(job) = data.saved.jobs.iter().find(|j| j.id == id).cloned() {
                data.anchors.insert(job.provider_id.clone(), now());
                let mut next = data.saved.clone();
                next.history.push(job);
                if let Err(error) = self.save(&mut data, next) {
                    data.error = Some(error.message);
                }
            }
        }
        drop(data);
        self.changed();
    }
    pub fn cancel(&self, id: &str) -> Result<()> {
        let data = lock(&self.0.data);
        if let Some(cancel) = data.cancel.get(id) {
            cancel.send_replace(true);
            return Ok(());
        }
        if data.saved.jobs.iter().any(|j| j.id == id) {
            Ok(())
        } else {
            Err(failure("测试记录不存在"))
        }
    }
    pub fn cancel_batch(&self, id: &str) -> Result<()> {
        let data = lock(&self.0.data);
        for job in data
            .saved
            .jobs
            .iter()
            .filter(|job| job.batch_id.as_deref() == Some(id))
        {
            if let Some(cancel) = data.cancel.get(&job.id) {
                cancel.send_replace(true);
            }
        }
        Ok(())
    }
    pub fn retry(&self, id: &str) -> Result<Job> {
        let mut data = lock(&self.0.data);
        let context = data
            .contexts
            .get(id)
            .cloned()
            .ok_or_else(|| failure("原测试上下文已失效，请重新测试"))?;
        self.gateway(context.target.client_id)
            .verify_test_version(&context.target.provider_id, &context.target.version)?;
        if data.saved.jobs.iter().any(|j| {
            j.id != id
                && !j.terminal()
                && j.client_id == context.target.client_id
                && j.provider_id == context.target.provider_id
        }) {
            return Err(failure("该供应商已有测试正在运行"));
        }
        let mut next = data.saved.clone();
        let job = next
            .jobs
            .iter_mut()
            .find(|j| j.id == id && j.can_retry && j.terminal())
            .ok_or_else(|| failure("没有可安全重试的失败组"))?;
        for group in &mut job.groups {
            if group.retryable && !group.uncertain && group.status == "failed" {
                group.status = "pending".into();
                group.error = None;
            }
        }
        job.status = "queued".into();
        job.completed_at = None;
        job.error = None;
        job.can_retry = false;
        let result = job.clone();
        self.save(&mut data, next)?;
        let (sender, receiver) = watch::channel(false);
        data.cancel.insert(id.into(), sender);
        drop(data);
        self.launch(id.into(), receiver);
        self.changed();
        Ok(result)
    }
    pub async fn update_bank(&self) -> Result<Value> {
        {
            let mut data = lock(&self.0.data);
            if data.bank_updating {
                return Err(failure("指纹库正在更新"));
            }
            if data
                .saved
                .bank
                .checked_at
                .is_some_and(|at| now() - at < 60_000)
            {
                return Err(failure("请稍后再更新指纹库"));
            }
            data.bank_updating = true;
        }
        self.changed();
        let result = tokio::time::timeout(std::time::Duration::from_secs(90), bank::update())
            .await
            .unwrap_or_else(|_| Err(failure("指纹库更新超时，已保留当前版本")));
        let mut data = lock(&self.0.data);
        data.bank_updating = false;
        let mut next = data.saved.clone();
        match result {
            Ok(bank) => next.bank = bank,
            Err(error) => {
                next.bank.error = Some(error.message);
                next.bank.checked_at = Some(now());
            }
        }
        self.save(&mut data, next)?;
        let result = bank::status(&data.saved.bank, false);
        drop(data);
        self.changed();
        Ok(result)
    }
}
#[cfg(test)]
mod tests;
