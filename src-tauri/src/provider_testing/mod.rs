//! Direct provider diagnostics. Only summaries/configuration are persisted;
//! connection prompts are shared globally; credentials and responses stay in this process.
mod bank;
pub mod commands;
mod compat;
mod connection;
mod downgrade;
mod errors;
mod fingerprint;
mod fingerprint_stream;
mod lm_detector;
mod migration;
mod modeltrace;
mod scheduler;
mod tokenizer;
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
pub const DEFAULT_CONNECTION_PROMPT: &str = "Reply with a short greeting.";
const STATE_LIMIT: u64 = 32 * 1024 * 1024;
fn failure(message: &str) -> AppError {
    AppError::new(crate::errors::code::PROVIDER_TEST, message)
}
fn now() -> i64 {
    crate::usage::model::now()
}
fn client_key(client: ClientId) -> &'static str {
    match client {
        ClientId::Codex => "codex",
        ClientId::Claude => "claude",
        ClientId::Grok => "grok",
        ClientId::Dsh => "dsh",
        ClientId::Agy => "agy",
    }
}
fn scope_key(client: ClientId, provider: &str) -> String {
    format!("{}:{provider}", client_key(client))
}
fn config_revision(saved: &Persisted, client: ClientId, provider: &str) -> String {
    let key = scope_key(client, provider);
    storage::digest(
        json!({"detection":saved.detection.get(&key).cloned().unwrap_or_else(|| Detection::for_client(client)),"degraded":saved.degraded.get(&key)})
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
    bank: Option<Arc<(Value, Value)>>,
    lm: Option<Arc<lm_detector::Resource>>,
    engine: Engine,
    sample_count: usize,
    detection_revision: Option<String>,
}
struct Data {
    bank: bank::Resource,
    saved: Persisted,
    revision: String,
    outputs: HashMap<String, Output>,
    contexts: HashMap<String, Context>,
    cancel: HashMap<String, watch::Sender<bool>>,
    detection_status: HashMap<String, DetectionStatus>,
    frequencies: std::collections::BTreeMap<String, u64>,
    frequency_sampled_at: Option<i64>,
    frequency_error: HashMap<String, String>,
    error: Option<String>,
    bank_updating: bool,
    lm_updating: bool,
    lm_error: Option<String>,
    lm_status: Value,
}
struct Shared {
    path: PathBuf,
    data: Mutex<Data>,
    gateways: Vec<Gateway>,
    events: broadcast::Sender<()>,
    jobs: Arc<Semaphore>,
    lm: Mutex<Option<Arc<lm_detector::Resource>>>,
    #[cfg(test)]
    clock: std::sync::atomic::AtomicI64,
}
#[derive(Clone)]
pub struct Service(Arc<Shared>);
impl Service {
    fn now(&self) -> i64 {
        #[cfg(test)]
        {
            let at = self.0.clock.load(std::sync::atomic::Ordering::SeqCst);
            if at != 0 {
                return at;
            }
        }
        now()
    }
    pub fn new(directory: &Path, gateways: impl Into<Vec<Gateway>>) -> Result<Self> {
        let gateways = gateways.into();
        let path = directory.join("provider-tests.json");
        let raw = storage::read_bounded(&path, STATE_LIMIT)?;
        let mut value: Option<Value> = raw
            .as_deref()
            .map(serde_json::from_slice)
            .transpose()
            .map_err(|_| failure("测试状态无法读取，已保留原文件"))?;
        let migrated = value
            .as_mut()
            .map(|value| migration::migrate(value, &gateways))
            .transpose()?
            .unwrap_or(false);
        let mut saved: Persisted = value
            .map(serde_json::from_value)
            .transpose()
            .map_err(|_| failure("测试状态无法读取，已保留原文件"))?
            .unwrap_or_default();
        if saved.schema != 3 {
            return Err(failure("测试状态来自更高版本，请升级应用"));
        }
        let mut changed = migrated;
        for job in &mut saved.jobs {
            if !job.terminal() {
                if job.automatic && job.started_at.is_some() {
                    let timing = saved
                        .detection_timing
                        .entry(scope_key(job.client_id, &job.provider_id))
                        .or_default();
                    timing.anchor_at = Some(now());
                    timing.last_started_at = timing.last_started_at.max(job.started_at);
                }
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
            lm: Mutex::new(None),
            #[cfg(test)]
            clock: std::sync::atomic::AtomicI64::new(0),
            data: Mutex::new(Data {
                bank: bank::Resource::load(&saved.bank),
                saved,
                revision,
                outputs: HashMap::new(),
                contexts: HashMap::new(),
                cancel: HashMap::new(),
                detection_status: HashMap::new(),
                frequencies: Default::default(),
                frequency_sampled_at: None,
                frequency_error: Default::default(),
                error: None,
                bank_updating: false,
                lm_updating: false,
                lm_error: None,
                lm_status: lm_detector::initial_status(directory),
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
    pub fn state(&self, client: ClientId, provider: &str) -> Result<State> {
        self.gateway(client).test_endpoint(provider, None)?;
        let data = lock(&self.0.data);
        let scope = |job: &&Job| job.client_id == client && job.provider_id == provider;
        Ok(State {
            revision: config_revision(&data.saved, client, provider),
            connection_prompt: data.saved.connection_prompt.clone(),
            platform: data
                .saved
                .platforms
                .get(client_key(client))
                .cloned()
                .unwrap_or_else(|| PlatformSettings::for_client(client)),
            banks: json!({"modelTrace":data.bank.status(&data.saved.bank, data.bank_updating),"lmDetector":lm_detector::status(&data)}),
            jobs: data
                .saved
                .jobs
                .iter()
                .rev()
                .filter(scope)
                .cloned()
                .collect(),
            detection: data
                .saved
                .detection
                .get(&scope_key(client, provider))
                .cloned()
                .unwrap_or_else(|| Detection::for_client(client)),
            degraded: data
                .saved
                .degraded
                .iter()
                .filter(|(id, _)| id.as_str() == scope_key(client, provider))
                .map(|(_, value)| (provider.to_owned(), value.clone()))
                .collect(),
            detection_status: data
                .detection_status
                .iter()
                .filter(|(key, _)| **key == scope_key(client, provider))
                .map(|(_, s)| s.clone())
                .collect(),
            error: data.error.clone(),
        })
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
        if request.kind == Kind::Fingerprint && target.search_only {
            return Err(failure("模型指纹检测仅用于普通供应商"));
        }
        if !connection::text_model(&request.model) {
            return Err(failure("请选择普通对话模型"));
        }
        Ok(())
    }
    pub fn start(&self, request: Request) -> Result<Job> {
        if request.client_id == ClientId::Agy && request.kind == Kind::Fingerprint {
            return Err(failure("Antigravity 支持连接测试"));
        }
        self.start_job(request, false)
    }
    pub fn set_prompt(&self, prompt: String) -> Result<()> {
        if prompt.trim().is_empty() {
            return Err(failure("请输入测试文案"));
        }
        if prompt.len() > 256 * 1024 {
            return Err(failure("测试文案过长"));
        }
        let mut data = lock(&self.0.data);
        if data.saved.connection_prompt == prompt {
            return Ok(());
        }
        let mut next = data.saved.clone();
        next.connection_prompt = prompt;
        self.save(&mut data, next)?;
        drop(data);
        self.changed();
        Ok(())
    }
    pub fn set_platform(&self, client: ClientId, settings: PlatformSettings) -> Result<()> {
        if ![1, 3].contains(&settings.model_trace_samples)
            || ![1, 3].contains(&settings.lm_detector_samples)
        {
            return Err(failure("采样组数只能是 1 或 3"));
        }
        let mut data = lock(&self.0.data);
        let mut next = data.saved.clone();
        next.platforms.insert(client_key(client).into(), settings);
        self.save(&mut data, next)?;
        drop(data);
        self.changed();
        Ok(())
    }
    fn start_job(&self, mut request: Request, automatic: bool) -> Result<Job> {
        let target = self.gateway(request.client_id).test_target_at(
            &request.provider_id,
            Some(&request.expected_revision),
            request.endpoint.as_deref(),
        )?;
        request.endpoint = Some(target.base_url.clone());
        if request.kind == Kind::Connection && request.model.is_empty() {
            request.model = self.default_connection_model(&target);
        }
        self.validate(&request, &target)?;
        if request.kind == Kind::Fingerprint {
            self.check_cached_model(&target, &request.model)?;
        }
        let platform = lock(&self.0.data)
            .saved
            .platforms
            .get(client_key(request.client_id))
            .cloned()
            .unwrap_or_else(|| PlatformSettings::for_client(request.client_id));
        let lm = if request.kind == Kind::Fingerprint && platform.engine == Engine::LmDetector {
            Some(self.lm_resource()?)
        } else {
            None
        };
        request.concurrency = request.concurrency.min(platform.samples());
        if request.tokenizer
            && (automatic
                || request.kind != Kind::Fingerprint
                || platform.engine != Engine::LmDetector)
        {
            return Err(failure("分词器仅用于 LM Detector 手动检测"));
        }
        let mut data = lock(&self.0.data);
        if data.saved.jobs.iter().any(|j| {
            !j.terminal()
                && j.client_id == request.client_id
                && j.provider_id == request.provider_id
        }) {
            return Err(failure("该供应商已有测试正在运行或排队"));
        }
        if request.kind == Kind::Connection {
            let prompt = request
                .prompt
                .get_or_insert_with(|| data.saved.connection_prompt.clone());
            if prompt.trim().is_empty() {
                return Err(failure("请输入测试文案"));
            }
        } else {
            request.prompt = None;
        }
        let snapshot = if request.kind == Kind::Fingerprint && platform.engine == Engine::ModelTrace
        {
            Some(data.bank.snapshot.clone()?)
        } else {
            None
        };
        let challenges = if request.kind == Kind::Fingerprint {
            if platform.engine == Engine::LmDetector {
                lmfpd::generate_challenges(platform.samples())
                    .map_err(|_| failure("指纹挑战生成失败"))?
                    .into_iter()
                    .map(|c| (c.expected_count as usize, c.prompt))
                    .collect()
            } else {
                modeltrace::challenges()
                    .into_iter()
                    .take(platform.samples())
                    .collect()
            }
        } else {
            vec![]
        };
        let job = Job {
            id: uuid::Uuid::new_v4().to_string(),
            client_id: request.client_id,
            provider_id: target.provider_id.clone(),
            provider_name: target.provider_name.clone(),
            provider_version: target.version.clone(),
            endpoint: Some(target.base_url.clone()),
            kind: request.kind,
            mode: Mode::Text,
            requested_model: request.model.clone(),
            forwarded_model: target.model(&request.model),
            returned_model: None,
            status: "queued".into(),
            created_at: self.now(),
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
            batch_id: None,
            can_retry: false,
            completion_reason: None,
            engine: platform.engine,
            sample_count: if request.kind == Kind::Fingerprint {
                platform.samples()
            } else {
                1
            },
            resource_version: lm
                .as_ref()
                .map(|r| r.version())
                .or_else(|| snapshot.as_ref().map(|r| r.1.clone()))
                .unwrap_or(Value::Null),
            tokenizer: request.tokenizer,
            tokenizer_result: None,
        };
        let mut next = data.saved.clone();
        next.jobs.push(job.clone());
        self.save(&mut data, next)?;
        let (cancel, receiver) = watch::channel(false);
        data.cancel.insert(job.id.clone(), cancel);
        data.outputs.insert(job.id.clone(), Output::default());
        let detection_revision =
            automatic.then(|| config_revision(&data.saved, job.client_id, &job.provider_id));
        data.contexts.insert(
            job.id.clone(),
            Context {
                request,
                target,
                challenges,
                bank: snapshot,
                lm,
                engine: platform.engine,
                sample_count: platform.samples(),
                detection_revision,
            },
        );
        drop(data);
        self.launch(job.id.clone(), receiver);
        if !automatic {
            self.changed();
        }
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
                service
                    .gateway(context.target.client_id)
                    .test_endpoint(&context.target.provider_id, Some(&context.target.base_url))?;
                if service.detail(&id)?.job.automatic {
                    service.prepare_automatic(&id, &context)?;
                } else {
                    service.update(&id, |job, _| {
                        job.status = "running".into();
                        job.started_at = Some(service.now());
                    })?;
                }
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
            job.completed_at = Some(self.now());
            job.elapsed_ms = elapsed;
            for group in &mut job.groups {
                if matches!(group.status.as_str(), "running" | "pending") {
                    group.status = "cancelled".into();
                    group.retryable = false;
                    group.uncertain = group.attempts > 0;
                }
            }
            if cancelled {
                if let Some(tokenizer) = job
                    .tokenizer_result
                    .as_mut()
                    .filter(|r| r["status"] == "running")
                {
                    tokenizer["status"] = "cancelled".into();
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
                let mut next = data.saved.clone();
                if job.started_at.is_some()
                    && data
                        .contexts
                        .get(id)
                        .and_then(|c| c.detection_revision.as_deref())
                        == Some(
                            config_revision(&data.saved, job.client_id, &job.provider_id).as_str(),
                        )
                {
                    next.detection_timing
                        .entry(scope_key(job.client_id, &job.provider_id))
                        .or_default()
                        .anchor_at = job.completed_at;
                }
                next.history.push(job);
                if let Err(error) = self.save(&mut data, next) {
                    data.error = Some(error.message);
                }
            }
        }
        if data
            .saved
            .jobs
            .iter()
            .any(|job| job.id == id && job.terminal() && !job.can_retry)
        {
            data.contexts.remove(id);
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
        let resource = result.as_ref().ok().map(bank::Resource::load);
        match result {
            Ok(bank) => next.bank = bank,
            Err(error) => {
                next.bank.error = Some(error.message);
                next.bank.checked_at = Some(now());
            }
        }
        self.save(&mut data, next)?;
        if let Some(resource) = resource {
            data.bank = resource;
        }
        let result = data.bank.status(&data.saved.bank, false);
        drop(data);
        self.changed();
        Ok(result)
    }
}
#[cfg(test)]
mod tests;
