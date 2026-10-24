use super::*;
use std::time::Duration;

pub(super) fn smart_interval(requests: u64, minimum: u32) -> Option<u32> {
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
    !model.is_empty() && model.len() <= 256 && !model.chars().any(char::is_control)
}
fn period(data: &Data, config: &Detection, provider: &str) -> Option<u32> {
    if config.mode == "smart" {
        data.frequency_error
            .is_none()
            .then(|| data.frequencies.get(provider).copied().unwrap_or(0))
            .and_then(|count| smart_interval(count, config.min_interval_minutes))
    } else {
        Some(config.interval_minutes)
    }
}
fn due(timing: &DetectionTiming, minutes: u32, at: i64) -> i64 {
    (timing.anchor_at.unwrap_or(at) + i64::from(minutes) * 60_000)
        .max(timing.last_started_at.map_or(0, |start| start + 300_000))
}
impl Service {
    fn refresh_frequencies(&self, at: i64, force: bool) {
        {
            let data = lock(&self.0.data);
            if !data
                .saved
                .detection
                .values()
                .any(|c| c.enabled && c.mode == "smart")
                || (!force
                    && data
                        .frequency_sampled_at
                        .is_some_and(|sample| at >= sample && at - sample < 60_000))
            {
                return;
            }
        }
        let counts = self.gateway(ClientId::Codex).detection_request_counts(at);
        let mut data = lock(&self.0.data);
        data.frequency_sampled_at = Some(at);
        match counts {
            Ok(counts) => {
                data.frequencies = counts;
                data.frequency_error = None;
            }
            Err(error) => {
                data.frequencies.clear();
                data.frequency_error = Some(format!("调用统计暂不可用：{}", error.message));
            }
        }
    }
    pub fn set_detection(
        &self,
        provider: &str,
        mut config: Detection,
        expected: &str,
    ) -> Result<()> {
        config.model = config.model.trim().into();
        if !["smart", "fixed"].contains(&config.mode.as_str())
            || !(1..=525600).contains(&config.interval_minutes)
            || !(5..=30).contains(&config.min_interval_minutes)
            || !valid_model(&config.model)
        {
            return Err(failure(
                "检测配置无效：固定间隔为 1–525600 分钟，智能最低间隔为 5–30 分钟",
            ));
        }
        let gateway = self.gateway(ClientId::Codex);
        if !gateway
            .test_candidates()?
            .iter()
            .any(|p| p.id == provider && !p.search_only)
        {
            return Err(failure("定时检测仅用于普通 Codex 供应商"));
        }
        let endpoint = gateway.test_endpoint(provider, config.endpoint.as_deref());
        if config.enabled {
            config.endpoint = Some(endpoint?);
        } else if let Ok(endpoint) = endpoint {
            config.endpoint = Some(endpoint);
        }
        let at = self.now();
        let mut data = lock(&self.0.data);
        if config_revision(&data.saved, provider) != expected {
            return Err(failure("该供应商检测设置已变化，请重新读取后重试"));
        }
        if config.enabled {
            Self::cached_model(
                &data,
                ClientId::Codex,
                provider,
                config.endpoint.as_deref().unwrap(),
                &config.model,
            )?;
        }
        let previous = data
            .saved
            .detection
            .get(provider)
            .cloned()
            .unwrap_or_default();
        if previous == config {
            return Ok(());
        }
        let mut next = data.saved.clone();
        let timing = next.detection_timing.entry(provider.into()).or_default();
        if !previous.enabled {
            timing.anchor_at = Some(at);
            timing.paused = false;
        }
        if !config.enabled {
            timing.paused = true;
        }
        next.detection.insert(provider.into(), config);
        self.save(&mut data, next)?;
        Self::cancel_automatic(&data, provider, false);
        data.detection_status.remove(provider);
        drop(data);
        self.changed();
        Ok(())
    }
    pub fn clear_mark(&self, provider: &str, expected: &str) -> Result<()> {
        let mut data = lock(&self.0.data);
        if config_revision(&data.saved, provider) != expected {
            return Err(failure("该供应商检测状态已变化，请重新读取后重试"));
        }
        let mut next = data.saved.clone();
        next.degraded.remove(provider);
        next.detection_timing
            .entry(provider.into())
            .or_default()
            .paused = true;
        self.save(&mut data, next)?;
        Self::cancel_automatic(&data, provider, false);
        drop(data);
        self.changed();
        Ok(())
    }
    fn cancel_automatic(data: &Data, provider: &str, queued_only: bool) {
        for job in data.saved.jobs.iter().filter(|j| {
            j.automatic
                && j.provider_id == provider
                && !j.terminal()
                && (!queued_only || j.started_at.is_none())
        }) {
            if let Some(cancel) = data.cancel.get(&job.id) {
                cancel.send_replace(true);
            }
        }
    }
    pub(super) fn automatic_current(&self, id: &str) -> bool {
        let context = lock(&self.0.data).contexts.get(id).cloned();
        let Some(context) = context else {
            return false;
        };
        let provider = &context.target.provider_id;
        if self
            .gateway(ClientId::Codex)
            .verify_test_version(provider, &context.target.version)
            .is_err()
            || self
                .gateway(ClientId::Codex)
                .test_endpoint(provider, Some(&context.target.base_url))
                .is_err()
            || !self
                .gateway(ClientId::Codex)
                .test_candidates()
                .is_ok_and(|items| {
                    items
                        .iter()
                        .any(|p| p.id == *provider && p.queued && !p.search_only)
                })
        {
            return false;
        }
        let data = lock(&self.0.data);
        context.detection_revision.as_deref()
            == Some(config_revision(&data.saved, provider).as_str())
            && data
                .saved
                .detection
                .get(provider)
                .is_some_and(|c| c.enabled)
            && !data.cancel.get(id).is_some_and(|cancel| *cancel.borrow())
            && !data.saved.degraded.contains_key(provider)
    }
    pub(super) fn prepare_automatic(&self, id: &str, context: &Context) -> Result<()> {
        let at = self.now();
        self.refresh_frequencies(at, true);
        let provider = &context.target.provider_id;
        let current = self.automatic_current(id);
        let mut data = lock(&self.0.data);
        let config = data
            .saved
            .detection
            .get(provider)
            .cloned()
            .unwrap_or_default();
        let timing = data
            .saved
            .detection_timing
            .get(provider)
            .cloned()
            .unwrap_or_default();
        let minutes = period(&data, &config, provider);
        let valid = current
            && context.detection_revision.as_deref()
                == Some(config_revision(&data.saved, provider).as_str())
            && config.model == context.request.model
            && config.endpoint.as_deref() == Some(context.target.base_url.as_str())
            && !timing.paused
            && minutes.is_some_and(|m| due(&timing, m, at) <= at)
            && Self::cached_model(
                &data,
                ClientId::Codex,
                provider,
                &context.target.base_url,
                &config.model,
            )
            .is_ok()
            && !data.cancel.get(id).is_some_and(|cancel| *cancel.borrow());
        if !valid {
            if let Some(cancel) = data.cancel.get(id) {
                cancel.send_replace(true);
            }
            return Err(failure("检测条件已变化，本轮未派发，等待下次检测"));
        }
        let mut next = data.saved.clone();
        next.detection_timing
            .entry(provider.clone())
            .or_default()
            .last_started_at = Some(at);
        let job = next
            .jobs
            .iter_mut()
            .find(|j| j.id == id)
            .ok_or_else(|| failure("测试记录不存在"))?;
        job.status = "running".into();
        job.started_at = Some(at);
        self.save(&mut data, next)?;
        drop(data);
        self.changed();
        Ok(())
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
        #[cfg(test)]
        self.0.clock.store(at, std::sync::atomic::Ordering::SeqCst);
        let gateway = self.gateway(ClientId::Codex);
        let candidates = gateway.test_candidates()?;
        self.refresh_frequencies(at, suspended);
        let configs = lock(&self.0.data).saved.detection.clone();
        let endpoints: HashMap<_, _> = candidates
            .iter()
            .map(|p| {
                (
                    p.id.clone(),
                    gateway.test_endpoint(
                        &p.id,
                        configs.get(&p.id).and_then(|c| c.endpoint.as_deref()),
                    ),
                )
            })
            .collect();
        let mut launch = vec![];
        {
            let mut data = lock(&self.0.data);
            let mut next = data.saved.clone();
            let live: std::collections::HashSet<_> = candidates
                .iter()
                .filter(|p| !p.search_only)
                .map(|p| p.id.as_str())
                .collect();
            for job in
                data.saved.jobs.iter().filter(|j| {
                    j.automatic && !j.terminal() && !live.contains(j.provider_id.as_str())
                })
            {
                if let Some(cancel) = data.cancel.get(&job.id) {
                    cancel.send_replace(true);
                }
            }
            data.detection_status
                .retain(|id, _| live.contains(id.as_str()));
            for provider in candidates.iter().filter(|p| !p.search_only) {
                let config = next
                    .detection
                    .get(&provider.id)
                    .cloned()
                    .unwrap_or_default();
                let minutes = period(&data, &config, &provider.id);
                let recent = data
                    .frequency_error
                    .is_none()
                    .then(|| data.frequencies.get(&provider.id).copied().unwrap_or(0));
                let cache_error = endpoints[&provider.id]
                    .as_ref()
                    .map_err(|e| e.message.clone())
                    .and_then(|endpoint| {
                        Self::cached_model(
                            &data,
                            ClientId::Codex,
                            &provider.id,
                            endpoint,
                            &config.model,
                        )
                        .map_err(|e| e.message)
                    })
                    .err();
                let running = data.saved.jobs.iter().find(|j| {
                    j.client_id == ClientId::Codex && j.provider_id == provider.id && !j.terminal()
                });
                let started = running.is_some_and(|j| j.started_at.is_some());
                let queued = running.is_some() && !started;
                let timing = next
                    .detection_timing
                    .entry(provider.id.clone())
                    .or_default();
                if suspended {
                    timing.anchor_at = Some(at);
                    Self::cancel_automatic(&data, &provider.id, true);
                }
                let status = if !config.enabled {
                    "disabled"
                } else if data.saved.degraded.contains_key(&provider.id) {
                    "degraded"
                } else if !provider.queued {
                    "outOfQueue"
                } else if started {
                    "running"
                } else if config.mode == "smart" && data.frequency_error.is_some() {
                    "statsUnavailable"
                } else if minutes.is_none() {
                    "idle"
                } else if cache_error.is_some() {
                    "modelsUnavailable"
                } else if queued {
                    "queued"
                } else {
                    "waiting"
                };
                if matches!(status, "disabled" | "degraded" | "outOfQueue") {
                    timing.paused = true;
                    Self::cancel_automatic(&data, &provider.id, false);
                } else if status == "idle" {
                    timing.paused = true;
                    Self::cancel_automatic(&data, &provider.id, true);
                } else if matches!(status, "statsUnavailable" | "modelsUnavailable") {
                    Self::cancel_automatic(&data, &provider.id, true);
                }
                let next_at = if matches!(status, "waiting" | "queued") {
                    if timing.paused || timing.anchor_at.is_none() {
                        timing.anchor_at = Some(at);
                        timing.paused = false;
                    }
                    Some(due(timing, minutes.unwrap(), at))
                } else {
                    None
                };
                let cooldown = timing
                    .last_started_at
                    .map(|start| start + 300_000)
                    .filter(|until| *until > at);
                let error = match status {
                    "statsUnavailable" => data.frequency_error.clone(),
                    "modelsUnavailable" => cache_error,
                    _ => None,
                };
                let status_value = DetectionStatus {
                    provider_id: provider.id.clone(),
                    status: status.into(),
                    model: config.model.clone(),
                    recent_requests: recent,
                    last_started_at: timing.last_started_at,
                    interval_minutes: minutes,
                    next_at,
                    cooldown_until: cooldown,
                    error,
                };
                data.detection_status
                    .insert(provider.id.clone(), status_value);
                if status == "waiting" && next_at.is_some_and(|due| due <= at) {
                    launch.push((provider.id.clone(), config));
                }
            }
            if next.detection_timing != data.saved.detection_timing {
                self.save(&mut data, next)?;
            }
        }
        for (provider_id, config) in launch {
            let revision = gateway.try_view()?.revision;
            if let Err(error) = self.start_job(
                Request {
                    client_id: ClientId::Codex,
                    provider_id: provider_id.clone(),
                    expected_revision: revision,
                    model: config.model,
                    endpoint: config.endpoint,
                    kind: Kind::Fingerprint,
                    concurrency: 1,
                    prompt: None,
                },
                true,
            ) {
                if let Some(status) = lock(&self.0.data).detection_status.get_mut(&provider_id) {
                    status.error = Some(error.message);
                }
            }
        }
        self.changed();
        Ok(())
    }
}
