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
    pub fn schedule(&self, requests: Vec<Request>, at: i64) -> Result<Schedule> {
        if requests.is_empty()
            || requests.len() > 1000
            || at <= now()
            || at > now() + 366 * 86_400_000
        {
            return Err(failure("请选择 1–1000 个供应商和未来一年内的预约时间"));
        }
        let mut providers = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for request in requests {
            if request.kind != Kind::Connection
                || request.mode != Mode::Text
                || request.media.is_some()
                || !request.input.is_empty()
            {
                return Err(failure("预约仅支持普通连接测试，不保存素材和输入"));
            }
            let target = self
                .gateway(request.client_id)
                .test_target(&request.provider_id, Some(&request.expected_revision))?;
            self.validate(&request, &target)?;
            if seen.insert((request.client_id, request.provider_id.clone())) {
                providers.push(ScheduledProvider {
                    client_id: request.client_id,
                    provider_id: request.provider_id,
                    provider_version: target.version,
                    model: request.model,
                });
            }
        }
        let schedule = Schedule {
            id: uuid::Uuid::new_v4().to_string(),
            at,
            status: "pending".into(),
            providers,
            batch_id: None,
            error: None,
        };
        let mut data = lock(&self.0.data);
        let mut next = data.saved.clone();
        if next
            .schedules
            .iter()
            .filter(|s| s.status == "pending")
            .count()
            >= 100
        {
            return Err(failure("预约数量已达上限，请先取消已有预约"));
        }
        next.schedules.push(schedule.clone());
        if next.schedules.len() > 200 {
            if let Some(index) = next.schedules.iter().position(|s| s.status != "pending") {
                next.schedules.remove(index);
            }
        }
        self.save(&mut data, next)?;
        drop(data);
        self.changed();
        Ok(schedule)
    }
    pub fn cancel_schedule(&self, id: &str) -> Result<()> {
        let mut data = lock(&self.0.data);
        let mut next = data.saved.clone();
        let schedule = next
            .schedules
            .iter_mut()
            .find(|s| s.id == id && s.status == "pending")
            .ok_or_else(|| failure("预约已派发或已取消，请在批次中停止运行中的测试"))?;
        schedule.status = "cancelled".into();
        self.save(&mut data, next)?;
        drop(data);
        self.changed();
        Ok(())
    }
    pub fn next_cycle(&self, client: ClientId, providers: &[String]) -> Result<i64> {
        self.gateway(client).next_test_cycle(providers)
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
        self.dispatch_schedules(at)?;
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
                    mode: Mode::Text,
                    input: String::new(),
                    media: None,
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
    fn dispatch_schedules(&self, at: i64) -> Result<()> {
        let due: Vec<_> = lock(&self.0.data)
            .saved
            .schedules
            .iter()
            .filter(|s| s.status == "pending" && s.at <= at)
            .cloned()
            .collect();
        for schedule in due {
            let batch = uuid::Uuid::new_v4().to_string();
            {
                let mut data = lock(&self.0.data);
                let mut next = data.saved.clone();
                let Some(current) = next
                    .schedules
                    .iter_mut()
                    .find(|s| s.id == schedule.id && s.status == "pending")
                else {
                    continue;
                };
                current.status = "dispatching".into();
                current.batch_id = Some(batch.clone());
                self.save(&mut data, next)?;
            }
            let mut requests = vec![];
            let mut errors = vec![];
            for provider in &schedule.providers {
                let gateway = self.gateway(provider.client_id);
                if let Err(error) =
                    gateway.verify_test_version(&provider.provider_id, &provider.provider_version)
                {
                    errors.push(error.message);
                    continue;
                }
                requests.push(Request {
                    client_id: provider.client_id,
                    provider_id: provider.provider_id.clone(),
                    expected_revision: gateway.try_view()?.revision,
                    model: provider.model.clone(),
                    kind: Kind::Connection,
                    mode: Mode::Text,
                    input: String::new(),
                    media: None,
                    concurrency: 1,
                });
            }
            let result = self.batch_with_id(&batch, requests)?;
            if let Some(failed) = result["errors"].as_array() {
                for error in failed {
                    errors.push(compat::string(&error["error"]).to_owned());
                }
            }
            let mut data = lock(&self.0.data);
            let mut next = data.saved.clone();
            if let Some(current) = next.schedules.iter_mut().find(|s| s.id == schedule.id) {
                current.status = "dispatched".into();
                current.error = (!errors.is_empty()).then(|| errors.join("；"));
            }
            self.save(&mut data, next)?;
        }
        Ok(())
    }
}
