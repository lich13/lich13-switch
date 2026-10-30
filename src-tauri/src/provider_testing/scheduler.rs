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
fn period(data: &Data, client: ClientId, config: &Detection, provider: &str) -> Option<u32> {
    if config.mode == "smart" {
        (!data.frequency_error.contains_key(client_key(client)))
            .then(|| {
                data.frequencies
                    .get(&scope_key(client, provider))
                    .copied()
                    .unwrap_or(0)
            })
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
        for client in [
            ClientId::Codex,
            ClientId::Claude,
            ClientId::Grok,
            ClientId::Dsh,
        ] {
            let counts = self.gateway(client).detection_request_counts(at);
            let mut data = lock(&self.0.data);
            data.frequency_sampled_at = Some(at);
            data.frequencies
                .retain(|k, _| !k.starts_with(&format!("{}:", client_key(client))));
            match counts {
                Ok(counts) => {
                    data.frequencies.extend(
                        counts
                            .into_iter()
                            .map(|(id, n)| (scope_key(client, &id), n)),
                    );
                    data.frequency_error.remove(client_key(client));
                }
                Err(error) => {
                    data.frequency_error.insert(
                        client_key(client).into(),
                        format!("调用统计暂不可用：{}", error.message),
                    );
                }
            }
        }
    }
    pub fn set_detection(
        &self,
        client: ClientId,
        provider: &str,
        mut config: Detection,
        expected: &str,
    ) -> Result<()> {
        config.model = config.model.trim().into();
        if !["smart", "fixed"].contains(&config.mode.as_str())
            || !(1..=525600).contains(&config.interval_minutes)
            || !(5..=30).contains(&config.min_interval_minutes)
            || (config.enabled && !valid_model(&config.model))
        {
            return Err(failure(
                "检测配置无效：固定间隔为 1–525600 分钟，智能最低间隔为 5–30 分钟",
            ));
        }
        let gateway = self.gateway(client);
        if !gateway
            .test_candidates()?
            .iter()
            .any(|p| p.id == provider && !p.search_only)
        {
            return Err(failure("定时检测仅用于普通供应商"));
        }
        let endpoint = gateway.test_endpoint(provider, config.endpoint.as_deref());
        if config.enabled {
            config.endpoint = Some(endpoint?);
        } else if let Ok(endpoint) = endpoint {
            config.endpoint = Some(endpoint);
        }
        let at = self.now();
        let mut data = lock(&self.0.data);
        if config_revision(&data.saved, client, provider) != expected {
            return Err(failure("该供应商检测设置已变化，请重新读取后重试"));
        }
        if config.enabled {
            Self::cached_model(
                &data,
                client,
                provider,
                config.endpoint.as_deref().unwrap(),
                &config.model,
            )?;
        }
        let previous = data
            .saved
            .detection
            .get(&scope_key(client, provider))
            .cloned()
            .unwrap_or_else(|| Detection::for_client(client));
        if previous == config {
            return Ok(());
        }
        let mut next = data.saved.clone();
        let timing = next
            .detection_timing
            .entry(scope_key(client, provider))
            .or_default();
        if !previous.enabled {
            timing.anchor_at = Some(at);
            timing.paused = false;
        }
        if !config.enabled {
            timing.paused = true;
        }
        next.detection.insert(scope_key(client, provider), config);
        self.save(&mut data, next)?;
        Self::cancel_automatic(&data, client, provider, false);
        data.detection_status.remove(&scope_key(client, provider));
        drop(data);
        self.changed();
        Ok(())
    }
    pub fn clear_mark(&self, client: ClientId, provider: &str, expected: &str) -> Result<()> {
        let mut data = lock(&self.0.data);
        if config_revision(&data.saved, client, provider) != expected {
            return Err(failure("该供应商检测状态已变化，请重新读取后重试"));
        }
        let mut next = data.saved.clone();
        next.degraded.remove(&scope_key(client, provider));
        next.detection_timing
            .entry(scope_key(client, provider))
            .or_default()
            .paused = true;
        self.save(&mut data, next)?;
        Self::cancel_automatic(&data, client, provider, false);
        drop(data);
        self.changed();
        Ok(())
    }
    fn cancel_automatic(data: &Data, client: ClientId, provider: &str, queued_only: bool) {
        for job in data.saved.jobs.iter().filter(|j| {
            j.automatic
                && j.client_id == client
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
        let client = context.target.client_id;
        let provider = &context.target.provider_id;
        if self
            .gateway(client)
            .verify_test_version(provider, &context.target.version)
            .is_err()
            || self
                .gateway(client)
                .test_endpoint(provider, Some(&context.target.base_url))
                .is_err()
            || !self.gateway(client).test_candidates().is_ok_and(|items| {
                items
                    .iter()
                    .any(|p| p.id == *provider && p.queued && !p.search_only)
            })
        {
            return false;
        }
        if !self
            .gateway(client)
            .test_target_at(provider, None, Some(&context.target.base_url))
            .is_ok_and(|current| {
                current.headers == context.target.headers
                    && current.protocol == context.target.protocol
            })
        {
            return false;
        }
        let data = lock(&self.0.data);
        context.detection_revision.as_deref()
            == Some(config_revision(&data.saved, client, provider).as_str())
            && data
                .saved
                .detection
                .get(&scope_key(client, provider))
                .is_some_and(|c| c.enabled)
            && !data.cancel.get(id).is_some_and(|cancel| *cancel.borrow())
            && !data
                .saved
                .degraded
                .contains_key(&scope_key(client, provider))
    }
    pub(super) fn prepare_automatic(&self, id: &str, context: &Context) -> Result<()> {
        let at = self.now();
        self.refresh_frequencies(at, true);
        let client = context.target.client_id;
        let provider = &context.target.provider_id;
        let current = self.automatic_current(id);
        let mut data = lock(&self.0.data);
        let config = data
            .saved
            .detection
            .get(&scope_key(client, provider))
            .cloned()
            .unwrap_or_else(|| Detection::for_client(client));
        let timing = data
            .saved
            .detection_timing
            .get(&scope_key(client, provider))
            .cloned()
            .unwrap_or_default();
        let minutes = period(&data, client, &config, provider);
        let valid = current
            && context.detection_revision.as_deref()
                == Some(config_revision(&data.saved, client, provider).as_str())
            && config.model == context.request.model
            && config.endpoint.as_deref() == Some(context.target.base_url.as_str())
            && !timing.paused
            && minutes.is_some_and(|m| due(&timing, m, at) <= at)
            && Self::cached_model(
                &data,
                client,
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
            .entry(scope_key(client, provider))
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
                if !suspended && service.sync_disabled_if_idle() {
                    continue;
                }
                let tick_service = service.clone();
                let result =
                    tokio::task::spawn_blocking(move || tick_service.tick(wall, suspended))
                        .await
                        .map_err(|_| failure("定时检测调度失败"))
                        .and_then(|r| r);
                if let Err(error) = result {
                    let mut data = lock(&service.0.data);
                    if data.error.as_ref() != Some(&error.message) {
                        data.error = Some(error.message);
                        drop(data);
                        service.changed();
                    }
                }
            }
        });
    }
    pub(super) fn sync_disabled_if_idle(&self) -> bool {
        {
            let data = lock(&self.0.data);
            if data.saved.detection.values().any(|c| c.enabled)
                || data.saved.jobs.iter().any(|j| j.automatic && !j.terminal())
            {
                return false;
            }
        }
        let mut live_candidates = Vec::new();
        for client in [
            ClientId::Codex,
            ClientId::Claude,
            ClientId::Grok,
            ClientId::Dsh,
        ] {
            let Ok(candidates) = self.gateway(client).test_candidates() else {
                return false;
            };
            for provider in candidates.into_iter().filter(|p| !p.search_only) {
                live_candidates.push((client, provider.id));
            }
        }
        let mut data = lock(&self.0.data);
        if data.saved.detection.values().any(|c| c.enabled)
            || data.saved.jobs.iter().any(|j| j.automatic && !j.terminal())
        {
            return false;
        }
        let mut next_statuses = HashMap::with_capacity(live_candidates.len());
        for (client, provider_id) in live_candidates {
            let key = scope_key(client, &provider_id);
            let config = data
                .saved
                .detection
                .get(&key)
                .cloned()
                .unwrap_or_else(|| Detection::for_client(client));
            if config.enabled {
                return false;
            }
            let last_started_at = data
                .saved
                .detection_timing
                .get(&key)
                .and_then(|t| t.last_started_at);
            next_statuses.insert(
                key,
                DetectionStatus {
                    provider_id,
                    status: "disabled".into(),
                    model: config.model,
                    recent_requests: None,
                    last_started_at,
                    interval_minutes: None,
                    next_at: None,
                    cooldown_until: None,
                    error: None,
                },
            );
        }
        let changed = data.detection_status != next_statuses;
        if changed {
            data.detection_status = next_statuses;
        }
        drop(data);
        if changed {
            self.changed();
        }
        true
    }
    pub(super) fn tick(&self, at: i64, suspended: bool) -> Result<()> {
        #[cfg(test)]
        self.0.clock.store(at, std::sync::atomic::Ordering::SeqCst);
        let (statuses, revision, error) = {
            let data = lock(&self.0.data);
            (
                data.detection_status.clone(),
                data.revision.clone(),
                data.error.clone(),
            )
        };
        let result: Result<()> = (|| {
            self.refresh_frequencies(at, suspended);
            for client in [
                ClientId::Codex,
                ClientId::Claude,
                ClientId::Grok,
                ClientId::Dsh,
            ] {
                self.tick_client(client, at, suspended)?;
            }
            Ok(())
        })();
        let mut data = lock(&self.0.data);
        if let Err(error) = &result {
            data.error = Some(error.message.clone());
        }
        let changed =
            statuses != data.detection_status || revision != data.revision || error != data.error;
        drop(data);
        if changed {
            self.changed();
        }
        result
    }
    fn tick_client(&self, client: ClientId, at: i64, suspended: bool) -> Result<()> {
        let gateway = self.gateway(client);
        let candidates = gateway.test_candidates()?;
        let mut launch = vec![];
        {
            let mut data = lock(&self.0.data);
            let mut next_timing = data.saved.detection_timing.clone();
            let live: std::collections::HashSet<_> = candidates
                .iter()
                .filter(|p| !p.search_only)
                .map(|p| p.id.as_str())
                .collect();
            for job in data.saved.jobs.iter().filter(|j| {
                j.automatic
                    && j.client_id == client
                    && !j.terminal()
                    && !live.contains(j.provider_id.as_str())
            }) {
                if let Some(cancel) = data.cancel.get(&job.id) {
                    cancel.send_replace(true);
                }
            }
            data.detection_status.retain(|key, _| {
                !key.starts_with(&format!("{}:", client_key(client)))
                    || live.iter().any(|id| scope_key(client, id) == *key)
            });
            for provider in candidates.iter().filter(|p| !p.search_only) {
                let config = data
                    .saved
                    .detection
                    .get(&scope_key(client, &provider.id))
                    .cloned()
                    .unwrap_or_else(|| Detection::for_client(client));
                let minutes = period(&data, client, &config, &provider.id);
                let recent = (!data.frequency_error.contains_key(client_key(client))).then(|| {
                    data.frequencies
                        .get(&scope_key(client, &provider.id))
                        .copied()
                        .unwrap_or(0)
                });
                let mut cache_error = None;
                let running = data.saved.jobs.iter().find(|j| {
                    j.client_id == client && j.provider_id == provider.id && !j.terminal()
                });
                let started = running.is_some_and(|j| j.started_at.is_some());
                let queued = running.is_some() && !started;
                let timing = next_timing
                    .entry(scope_key(client, &provider.id))
                    .or_default();
                if suspended {
                    timing.anchor_at = Some(at);
                    Self::cancel_automatic(&data, client, &provider.id, true);
                }
                let status = if !config.enabled {
                    "disabled"
                } else if data
                    .saved
                    .degraded
                    .contains_key(&scope_key(client, &provider.id))
                {
                    "degraded"
                } else if !provider.queued {
                    "outOfQueue"
                } else if started {
                    "running"
                } else if config.mode == "smart"
                    && data.frequency_error.contains_key(client_key(client))
                {
                    "statsUnavailable"
                } else if minutes.is_none() {
                    "idle"
                } else {
                    cache_error = gateway
                        .test_endpoint(&provider.id, config.endpoint.as_deref())
                        .and_then(|endpoint| {
                            Self::cached_model(
                                &data,
                                client,
                                &provider.id,
                                &endpoint,
                                &config.model,
                            )
                        })
                        .err()
                        .map(|e| e.message);
                    if cache_error.is_some() {
                        "modelsUnavailable"
                    } else if queued {
                        "queued"
                    } else {
                        "waiting"
                    }
                };
                if matches!(status, "disabled" | "degraded" | "outOfQueue") {
                    timing.paused = true;
                    Self::cancel_automatic(&data, client, &provider.id, false);
                } else if status == "idle" {
                    timing.paused = true;
                    Self::cancel_automatic(&data, client, &provider.id, true);
                } else if matches!(status, "statsUnavailable" | "modelsUnavailable") {
                    Self::cancel_automatic(&data, client, &provider.id, true);
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
                    "statsUnavailable" => data.frequency_error.get(client_key(client)).cloned(),
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
                    .insert(scope_key(client, &provider.id), status_value);
                if status == "waiting" && next_at.is_some_and(|due| due <= at) {
                    launch.push((provider.id.clone(), config));
                }
            }
            if next_timing != data.saved.detection_timing {
                let mut next = data.saved.clone();
                next.detection_timing = next_timing;
                self.save(&mut data, next)?;
            }
        }
        for (provider_id, config) in launch {
            let revision = gateway.try_view()?.revision;
            if let Err(error) = self.start_job(
                Request {
                    client_id: client,
                    provider_id: provider_id.clone(),
                    expected_revision: revision,
                    model: config.model,
                    endpoint: config.endpoint,
                    kind: Kind::Fingerprint,
                    concurrency: 1,
                    prompt: None,
                    tokenizer: false,
                },
                true,
            ) {
                if let Some(status) = lock(&self.0.data)
                    .detection_status
                    .get_mut(&scope_key(client, &provider_id))
                {
                    status.error = Some(error.message);
                }
            }
        }
        Ok(())
    }
}
