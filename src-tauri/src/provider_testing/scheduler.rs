use super::*;
use std::time::Duration;

pub(super) fn smart_interval(requests: usize, minimum: u32) -> Option<u32> {
    let minutes = match requests {
        0 => return None,
        1..=2 => 30,
        3..=9 => 25,
        10..=29 => 20,
        30..=59 => 15,
        60..=119 => 10,
        _ => 5,
    };
    Some(minutes.max(minimum.clamp(5, 30)))
}
fn valid_model(model: &str) -> bool {
    !model.trim().is_empty() && model.len() <= 256 && !model.chars().any(char::is_control)
}
impl Service {
    pub(super) fn automatic_eligible(&self, id: &str) -> Result<bool> {
        let eligible = self
            .gateway(ClientId::Codex)
            .test_candidates()?
            .iter()
            .any(|p| p.id == id && p.queued && !p.search_only);
        let data = lock(&self.0.data);
        Ok(eligible
            && data.saved.detection.enabled
            && data
                .saved
                .detection
                .providers
                .get(id)
                .and_then(|p| p.enabled)
                .unwrap_or(true)
            && !data.saved.degraded.contains_key(id))
    }
    pub fn set_detection(&self, mut config: Detection, expected: &str) -> Result<()> {
        if !["smart", "fixed"].contains(&config.mode.as_str())
            || config.interval_minutes < 5
            || config.interval_minutes > 1440
            || !(5..=30).contains(&config.min_interval_minutes)
            || !valid_model(&config.model)
            || config.providers.len() > 1000
            || config
                .providers
                .values()
                .any(|p| p.model.as_ref().is_some_and(|m| !valid_model(m)))
        {
            return Err(failure(
                "检测配置无效：固定间隔最低 5 分钟，智能最低间隔为 5–30 分钟",
            ));
        }
        let candidates = self.gateway(ClientId::Codex).test_candidates()?;
        config
            .providers
            .retain(|id, _| candidates.iter().any(|p| &p.id == id && !p.search_only));
        let mut data = lock(&self.0.data);
        if config_revision(&data.saved) != expected {
            return Err(failure("检测设置已变化，请刷新后重试"));
        }
        let mut next = data.saved.clone();
        next.detection = config;
        self.save(&mut data, next)?;
        data.anchors.clear();
        data.detection_status.clear();
        drop(data);
        self.changed();
        Ok(())
    }
    pub fn clear_mark(&self, provider: &str, expected: &str) -> Result<()> {
        let mut data = lock(&self.0.data);
        if config_revision(&data.saved) != expected {
            return Err(failure("检测状态已变化，请刷新后重试"));
        }
        let mut next = data.saved.clone();
        next.degraded.remove(provider);
        self.save(&mut data, next)?;
        data.anchors.remove(provider);
        drop(data);
        self.changed();
        Ok(())
    }
    pub fn batch(&self, requests: Vec<Request>) -> Result<Value> {
        if requests.is_empty() || requests.len() > 1000 {
            return Err(failure("批量测试应选择 1–1000 个供应商"));
        }
        let id = uuid::Uuid::new_v4().to_string();
        self.batch_with_id(&id, requests)
    }
    fn batch_with_id(&self, id: &str, requests: Vec<Request>) -> Result<Value> {
        let mut seen = std::collections::HashSet::new();
        let mut jobs = vec![];
        let mut errors = vec![];
        for request in requests {
            if !seen.insert((request.client_id, request.provider_id.clone())) {
                continue;
            }
            let provider = request.provider_id.clone();
            match self.start_job(request, false, Some(id.into())) {
                Ok(job) => jobs.push(job),
                Err(error) => errors.push(json!({"providerId":provider,"error":error.message})),
            }
        }
        Ok(json!({"id":id,"jobs":jobs,"errors":errors}))
    }
    pub fn run_scheduler(&self) {
        let weak = Arc::downgrade(&self.0);
        tauri::async_runtime::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(5));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut last_wall = now();
            loop {
                interval.tick().await;
                let Some(shared) = weak.upgrade() else {
                    break;
                };
                let service = Self(shared);
                let wall = now();
                let suspended = wall - last_wall > 90_000 || wall < last_wall;
                last_wall = wall;
                if let Err(error) = service.tick(wall, suspended) {
                    lock(&service.0.data).error = Some(error.message);
                    service.changed();
                }
            }
        });
    }
    pub(super) fn tick(&self, at: i64, suspended: bool) -> Result<()> {
        let candidates = self.gateway(ClientId::Codex).test_candidates()?;
        let mut launch = vec![];
        {
            let mut data = lock(&self.0.data);
            if suspended {
                data.anchors.clear();
            }
            let config = data.saved.detection.clone();
            let live: std::collections::HashSet<_> = candidates
                .iter()
                .filter(|c| !c.search_only)
                .map(|c| c.id.clone())
                .collect();
            for job in data
                .saved
                .jobs
                .iter()
                .filter(|j| j.automatic && !j.terminal() && !live.contains(&j.provider_id))
            {
                if let Some(cancel) = data.cancel.get(&job.id) {
                    cancel.send_replace(true);
                }
            }
            data.anchors.retain(|id, _| live.contains(id));
            data.detection_status.retain(|id, _| live.contains(id));
            for provider in candidates.iter().filter(|p| !p.search_only) {
                let override_ = config.providers.get(&provider.id);
                let enabled = config.enabled && override_.and_then(|p| p.enabled).unwrap_or(true);
                let model = override_
                    .and_then(|p| p.model.clone())
                    .unwrap_or_else(|| config.model.clone());
                let recent = self
                    .gateway(ClientId::Codex)
                    .recent_business_requests(&provider.id);
                let minutes = if config.mode == "smart" {
                    smart_interval(recent, config.min_interval_minutes)
                } else {
                    Some(config.interval_minutes)
                };
                let cooldown = data
                    .saved
                    .history
                    .iter()
                    .rev()
                    .filter(|j| j.provider_id == provider.id)
                    .find_map(|j| j.completed_at)
                    .map(|at| at + 300_000);
                let active = data.saved.jobs.iter().any(|j| {
                    j.provider_id == provider.id && j.client_id == ClientId::Codex && !j.terminal()
                });
                if !enabled || !provider.queued {
                    for job in
                        data.saved.jobs.iter().filter(|j| {
                            j.provider_id == provider.id && j.automatic && !j.terminal()
                        })
                    {
                        if let Some(cancel) = data.cancel.get(&job.id) {
                            cancel.send_replace(true);
                        }
                    }
                }
                let status = if !enabled {
                    "disabled"
                } else if data.saved.degraded.contains_key(&provider.id) {
                    "degraded"
                } else if !provider.queued {
                    "outOfQueue"
                } else if active {
                    "running"
                } else if minutes.is_none() {
                    "idle"
                } else {
                    "waiting"
                };
                let next_at = if status == "waiting" {
                    let anchor = *data.anchors.entry(provider.id.clone()).or_insert(at);
                    Some((anchor + i64::from(minutes.unwrap()) * 60_000).max(cooldown.unwrap_or(0)))
                } else {
                    if status != "running" {
                        data.anchors.remove(&provider.id);
                    }
                    None
                };
                data.detection_status.insert(
                    provider.id.clone(),
                    DetectionStatus {
                        provider_id: provider.id.clone(),
                        status: status.into(),
                        model: model.clone(),
                        recent_requests: recent,
                        interval_minutes: minutes,
                        next_at,
                        cooldown_until: cooldown.filter(|until| *until > at),
                        error: None,
                    },
                );
                if next_at.is_some_and(|due| due <= at) {
                    launch.push((provider.id.clone(), model));
                    data.anchors.insert(provider.id.clone(), at);
                }
            }
        }
        for (provider_id, model) in launch {
            let revision = self.gateway(ClientId::Codex).try_view()?.revision;
            let result = self.start_job(
                Request {
                    client_id: ClientId::Codex,
                    provider_id: provider_id.clone(),
                    expected_revision: revision,
                    model,
                    kind: Kind::Fingerprint,
                    concurrency: 1,
                },
                true,
                None,
            );
            if let Err(error) = result {
                if let Some(status) = lock(&self.0.data).detection_status.get_mut(&provider_id) {
                    status.error = Some(error.message);
                }
            }
        }
        self.changed();
        Ok(())
    }
}
