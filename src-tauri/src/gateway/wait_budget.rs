//! One deadline and cancellation boundary cover a request up to effective output.
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, Weak},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    sync::{broadcast, watch},
    time::Instant,
};

#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingRequest {
    pub request_id: String,
    pub provider_id: Option<String>,
    pub model: Option<String>,
    pub started_at: u64,
    pub elapsed_seconds: u64,
    pub remaining_seconds: Option<u64>,
    pub phase: String,
}
struct Pending {
    view: PendingRequest,
    started: Instant,
    deadline: Instant,
    compaction_execution: bool,
    traffic: Option<super::freeze::Ticket>,
    paused_before: Duration,
}
impl Pending {
    fn paused_for(&self) -> Duration {
        self.traffic.as_ref().map_or(Duration::ZERO, |t| {
            t.paused_for().saturating_sub(self.paused_before)
        })
    }
    fn paused(&self) -> bool {
        self.traffic.as_ref().is_some_and(|t| t.paused())
    }
    fn expires_at(&self) -> Instant {
        self.deadline + self.paused_for()
    }
}
#[derive(Default)]
struct State {
    active: Option<Pending>,
    cancelled: Option<Instant>,
}
struct Shared {
    state: Mutex<State>,
    changes: watch::Sender<()>,
    registry: Option<Registry>,
}
#[derive(Clone)]
pub(super) struct Registry(Arc<RegistryInner>);
struct RegistryInner {
    requests: Mutex<HashMap<String, Weak<Shared>>>,
    events: broadcast::Sender<()>,
}
impl Registry {
    pub fn new(events: broadcast::Sender<()>) -> Self {
        Self(Arc::new(RegistryInner {
            requests: Mutex::new(HashMap::new()),
            events,
        }))
    }
    fn remove(&self, id: &str) {
        self.0.requests.lock().unwrap().remove(id);
        let _ = self.0.events.send(());
    }
    pub fn view(&self) -> Vec<PendingRequest> {
        let entries: Vec<_> = self
            .0
            .requests
            .lock()
            .unwrap()
            .values()
            .filter_map(Weak::upgrade)
            .collect();
        let mut result: Vec<_> = entries
            .iter()
            .filter_map(|shared| {
                let state = shared.state.lock().unwrap();
                let active = state.active.as_ref()?;
                let mut view = active.view.clone();
                view.elapsed_seconds = active
                    .started
                    .elapsed()
                    .saturating_sub(active.paused_for())
                    .as_secs();
                if active.paused() {
                    view.phase = "frozen".into();
                }
                view.remaining_seconds =
                    (!active.compaction_execution && !active.paused()).then(|| {
                        active
                            .expires_at()
                            .saturating_duration_since(Instant::now())
                            .as_millis()
                            .div_ceil(1000) as u64
                    });
                Some(view)
            })
            .collect();
        result.sort_by(|a, b| {
            a.started_at
                .cmp(&b.started_at)
                .then(a.request_id.cmp(&b.request_id))
        });
        result
    }
    pub fn cancel(&self, id: &str) -> bool {
        let shared = self
            .0
            .requests
            .lock()
            .unwrap()
            .get(id)
            .and_then(Weak::upgrade);
        let Some(shared) = shared else {
            return false;
        };
        let mut state = shared.state.lock().unwrap();
        if !state.active.as_ref().is_some_and(|p| {
            p.view.request_id == id
                && (p.compaction_execution || p.paused() || p.expires_at() > Instant::now())
        }) {
            return false;
        }
        state.cancelled = state.active.take().map(|p| p.deadline);
        drop(state);
        self.remove(id);
        shared.changes.send_replace(());
        true
    }
}
impl Drop for Shared {
    fn drop(&mut self) {
        if let (Some(registry), Some(active)) =
            (&self.registry, &self.state.get_mut().unwrap().active)
        {
            registry.remove(&active.view.request_id);
        }
    }
}
#[derive(Clone)]
pub(super) struct WaitBudget {
    shared: Arc<Shared>,
    pub seconds: u64,
}
impl WaitBudget {
    #[cfg(test)]
    pub fn new(seconds: u64) -> Self {
        Self::with_registry(seconds, None)
    }
    pub fn tracked(seconds: u64, registry: &Registry) -> Self {
        Self::with_registry(seconds, Some(registry.clone()))
    }
    fn with_registry(seconds: u64, registry: Option<Registry>) -> Self {
        Self {
            shared: Arc::new(Shared {
                state: Mutex::new(State::default()),
                changes: watch::channel(()).0,
                registry,
            }),
            seconds,
        }
    }
    pub fn begin(&self) {
        self.begin_at(Instant::now());
    }
    pub fn begin_at(&self, started: Instant) {
        let id = uuid::Uuid::new_v4().to_string();
        let mut state = self.shared.state.lock().unwrap();
        if let (Some(registry), Some(old)) = (&self.shared.registry, state.active.take()) {
            registry.remove(&old.view.request_id);
        }
        state.cancelled = None;
        state.active = Some(Pending {
            view: PendingRequest {
                request_id: id.clone(),
                provider_id: None,
                model: None,
                started_at: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs()
                    .saturating_sub(started.elapsed().as_secs()),
                elapsed_seconds: 0,
                remaining_seconds: Some(self.seconds),
                phase: "admission".into(),
            },
            started,
            deadline: started + Duration::from_secs(self.seconds),
            compaction_execution: false,
            traffic: None,
            paused_before: Duration::ZERO,
        });
        if let Some(registry) = &self.shared.registry {
            registry
                .0
                .requests
                .lock()
                .unwrap()
                .insert(id, Arc::downgrade(&self.shared));
            let _ = registry.0.events.send(());
        }
        drop(state);
        self.shared.changes.send_replace(());
    }
    pub fn attach_traffic(&self, traffic: &super::freeze::Ticket) {
        if let Some(active) = &mut self.shared.state.lock().unwrap().active {
            active.paused_before = traffic.paused_for();
            active.traffic = Some(traffic.clone());
        }
        self.shared.changes.send_replace(());
    }
    pub fn traffic(&self) -> Option<super::freeze::Ticket> {
        self.shared
            .state
            .lock()
            .unwrap()
            .active
            .as_ref()
            .and_then(|p| p.traffic.clone())
    }
    pub fn describe(&self, provider: Option<&str>, model: Option<&str>, phase: &str) {
        let mut state = self.shared.state.lock().unwrap();
        if let Some(active) = &mut state.active {
            if matches!(phase, "admission" | "capacity" | "retry" | "ws_wait") {
                if let Some(traffic) = &active.traffic {
                    traffic.waiting();
                }
            }
            if let Some(provider) = provider {
                active.view.provider_id = Some(provider.into());
            }
            if let Some(model) = model {
                active.view.model = crate::events::safe_message(model);
            }
            active.view.phase = phase.into();
            active.compaction_execution = false;
        }
        drop(state);
        self.shared.changes.send_replace(());
        if let Some(registry) = &self.shared.registry {
            let _ = registry.0.events.send(());
        }
    }
    pub fn deadline(&self) -> Option<Instant> {
        self.shared
            .state
            .lock()
            .unwrap()
            .active
            .as_ref()
            .map(|p| p.deadline)
    }
    pub fn compaction_execution(&self, deadline: Option<Instant>, executing: bool) {
        let mut state = self.shared.state.lock().unwrap();
        if let Some(active) = state
            .active
            .as_mut()
            .filter(|p| Some(p.deadline) == deadline)
        {
            active.compaction_execution = executing;
            if executing {
                active.view.phase = "compaction".into();
            }
        }
        drop(state);
        self.shared.changes.send_replace(());
        if let Some(registry) = &self.shared.registry {
            let _ = registry.0.events.send(());
        }
    }
    pub fn executing_compaction(&self) -> bool {
        self.shared
            .state
            .lock()
            .unwrap()
            .active
            .as_ref()
            .is_some_and(|p| p.compaction_execution)
    }
    pub async fn response<T>(
        &self,
        deadline: Instant,
        compaction: bool,
        future: impl std::future::Future<Output = T>,
    ) -> Result<T, tokio::time::error::Elapsed> {
        if !compaction {
            return tokio::time::timeout_at(deadline, future).await;
        }
        let mut changes = self.shared.changes.subscribe();
        tokio::pin!(future);
        loop {
            changes.borrow_and_update();
            if self.executing_compaction() {
                return Ok(future.await);
            }
            tokio::select! {
                biased;
                result = &mut future => return Ok(result),
                _ = changes.changed() => {},
                elapsed = tokio::time::timeout_at(deadline, std::future::pending()) => return elapsed,
            }
        }
    }
    pub fn complete(&self, deadline: Option<Instant>) {
        let mut state = self.shared.state.lock().unwrap();
        if state
            .active
            .as_ref()
            .is_some_and(|p| Some(p.deadline) == deadline)
        {
            if let (Some(registry), Some(active)) = (&self.shared.registry, state.active.take()) {
                registry.remove(&active.view.request_id);
            }
        }
        drop(state);
        self.shared.changes.send_replace(());
    }
    pub fn is_cancelled(&self, deadline: Option<Instant>) -> bool {
        let state = self.shared.state.lock().unwrap();
        state.cancelled.is_some() && state.cancelled == deadline
    }
    pub fn cancel_requested(&self) -> bool {
        self.shared.state.lock().unwrap().cancelled.is_some()
    }
    pub fn acknowledge_cancel(&self) {
        self.shared.state.lock().unwrap().cancelled = None;
    }
    pub fn expired(&self, deadline: Option<Instant>) -> bool {
        self.shared
            .state
            .lock()
            .unwrap()
            .active
            .as_ref()
            .is_some_and(|p| {
                !p.compaction_execution
                    && Some(p.deadline) == deadline
                    && !p.paused()
                    && Instant::now() >= p.expires_at()
            })
    }
    pub async fn cancelled(&self) {
        let mut changes = self.shared.changes.subscribe();
        loop {
            changes.borrow_and_update();
            if self.shared.state.lock().unwrap().cancelled.is_some() {
                return;
            }
            if changes.changed().await.is_err() {
                return;
            }
        }
    }
    pub async fn wait(&self) {
        let mut changes = self.shared.changes.subscribe();
        loop {
            changes.borrow_and_update();
            let mut traffic_changes = self.traffic().map(|t| t.subscribe());
            let deadline = self
                .shared
                .state
                .lock()
                .unwrap()
                .active
                .as_ref()
                .filter(|p| !p.compaction_execution && !p.paused())
                .map(|p| (p.deadline, p.expires_at()));
            if let Some((identity, deadline)) = deadline {
                tokio::select! {
                    biased;
                    _ = changes.changed() => {},
                    _ = super::freeze::changed(&mut traffic_changes) => {},
                    _ = tokio::time::sleep_until(deadline) => { if self.expired(Some(identity)) { return; } }
                }
            } else {
                tokio::select! {
                    _ = changes.changed() => {},
                    _ = super::freeze::changed(&mut traffic_changes) => {},
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::protocol::Protocol;
    use serde_json::json;

    #[tokio::test(start_paused = true)]
    async fn freeze_preserves_deadline_identity_and_only_live_wait_time_counts() {
        let registry = Registry::new(broadcast::channel(16).0);
        let budget = WaitBudget::tracked(10, &registry);
        let gate = crate::gateway::freeze::Gate::default();
        let traffic = gate.request();
        traffic.waiting();
        budget.begin();
        budget.attach_traffic(&traffic);
        let identity = budget.deadline();
        tokio::time::advance(Duration::from_secs(3)).await;
        gate.set(true).unwrap();
        tokio::time::advance(Duration::from_secs(3600)).await;
        assert!(!budget.expired(identity));
        assert!(tokio::time::timeout(Duration::from_secs(1), budget.wait())
            .await
            .is_err());
        let row = registry.view().pop().unwrap();
        assert_eq!(row.phase, "frozen");
        assert_eq!(row.elapsed_seconds, 3);
        assert_eq!(row.remaining_seconds, None);
        gate.set(false).unwrap();
        assert_eq!(budget.deadline(), identity);
        assert_eq!(registry.view()[0].remaining_seconds, Some(7));
        tokio::time::advance(Duration::from_secs(6)).await;
        assert!(!budget.expired(identity));
        budget.wait().await;
        assert!(budget.expired(identity));
        budget.complete(identity);

        budget.begin();
        budget.attach_traffic(&traffic);
        gate.set(true).unwrap();
        tokio::time::advance(Duration::from_secs(3600)).await;
        assert!(registry.cancel(&registry.view()[0].request_id));
        budget.cancelled().await;
        assert!(registry.view().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn compaction_can_be_silent_beyond_300_seconds_and_cancelled_without_resetting_budget() {
        let registry = Registry::new(broadcast::channel(16).0);
        let budget = WaitBudget::tracked(300, &registry);
        budget.begin();
        let identity = budget.deadline();
        let mut protocol = Protocol::new(true);
        protocol.attach_wait_budget(&budget);
        protocol.set_operation(crate::usage::model::Operation::Compaction);
        protocol.begin_compaction_execution();
        tokio::time::advance(Duration::from_secs(601)).await;
        assert!(!budget.expired(identity));
        assert!(tokio::time::timeout(Duration::from_secs(1), budget.wait())
            .await
            .is_err());
        protocol.value(&json!({"type":"response.output_text.delta","delta":"fixture"}));
        let row = registry.view().pop().unwrap();
        assert_eq!(row.remaining_seconds, None);
        assert_eq!(row.phase, "compaction");
        assert!(registry.cancel(&row.request_id));
        budget.cancelled().await;
        protocol.finish(None, crate::errors::code::CANCELLED);
        assert!(budget.is_cancelled(identity));
        budget.complete(identity);
        assert!(registry.view().is_empty());

        budget.begin();
        let identity = budget.deadline();
        budget.compaction_execution(identity, true);
        tokio::time::advance(Duration::from_secs(301)).await;
        budget.describe(None, None, "retry");
        assert_eq!(budget.deadline(), identity);
        assert!(budget.expired(identity));
        budget.wait().await;
    }

    #[tokio::test(start_paused = true)]
    async fn response_deadline_switches_only_after_dispatch_and_old_tail_cannot_end_new_compaction()
    {
        let budget = WaitBudget::new(300);
        budget.begin();
        let identity = budget.deadline();
        let response = budget
            .response(Instant::now() + Duration::from_secs(1), true, async {
                tokio::time::sleep(Duration::from_millis(100)).await;
                budget.compaction_execution(identity, true);
                tokio::time::sleep(Duration::from_secs(601)).await;
                42
            })
            .await;
        assert_eq!(response.unwrap(), 42);
        budget.complete(identity);
        budget.begin();
        let current = budget.deadline();
        budget.compaction_execution(current, true);
        budget.compaction_execution(identity, false);
        budget.complete(identity);
        assert!(budget.executing_compaction());
        budget.complete(current);
        budget.begin();
        let mut ordinary = Protocol::new(true);
        ordinary.attach_wait_budget(&budget);
        let hints = crate::gateway::replay::RequestHints::from_value(json!({
            "input":[{"type":"compaction","encrypted_content":"fixture"},{"role":"user","content":"fixture"}]
        }), false).unwrap();
        ordinary.routing_evidence(Some(&hints));
        ordinary.begin_compaction_execution();
        assert!(!budget.executing_compaction());
        assert!(budget
            .response(
                Instant::now() + Duration::from_secs(1),
                false,
                std::future::pending::<()>()
            )
            .await
            .is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn heartbeats_do_not_extend_shared_budget_and_effective_output_removes_deadline() {
        let budget = WaitBudget::new(300);
        budget.begin();
        let deadline = budget.deadline();
        let mut first = Protocol::new(true);
        first.attach_wait_budget(&budget);
        first.response(200, true, "identity");
        tokio::time::advance(Duration::from_secs(150)).await;
        first.feed(b": heartbeat\n\n");
        assert_eq!(budget.deadline(), deadline);
        let mut retry = Protocol::new(true);
        retry.attach_wait_budget(&budget);
        tokio::time::advance(Duration::from_secs(149)).await;
        retry.value(
            &json!({"type":"response.created", "response":{"id":"fixture","status":"in_progress"}}),
        );
        assert_eq!(budget.deadline(), deadline);
        retry.value(&json!({"type":"response.output_text.delta", "delta":"fixture"}));
        assert!(budget.deadline().is_none());
        tokio::time::advance(Duration::from_secs(601)).await;
        assert!(!budget.expired(deadline));
        retry.value(
            &json!({"type":"response.completed", "response":{"id":"fixture","status":"completed"}}),
        );
        assert!(retry.succeeded());
    }

    #[tokio::test(start_paused = true)]
    async fn new_websocket_turn_gets_its_own_budget_and_old_tail_cannot_complete_it() {
        let budget = WaitBudget::new(300);
        budget.begin();
        let old = budget.deadline();
        budget.complete(old);
        tokio::time::advance(Duration::from_secs(700)).await;
        budget.begin();
        let current = budget.deadline();
        budget.complete(old);
        assert_eq!(budget.deadline(), current);
        tokio::time::advance(Duration::from_secs(300)).await;
        assert!(budget.expired(current));
        budget.wait().await;
    }
}

#[cfg(test)]
mod cancellation_tests {
    use super::*;
    #[tokio::test(start_paused = true)]
    async fn cancel_is_per_request_and_old_ids_cannot_cancel_a_new_round() {
        let registry = Registry::new(broadcast::channel(16).0);
        let first = WaitBudget::tracked(300, &registry);
        let second = WaitBudget::tracked(300, &registry);
        first.begin();
        second.begin();
        first.describe(Some("fixture-one"), Some("fixture-model"), "headers");
        tokio::time::advance(Duration::from_secs(61)).await;
        let row = registry
            .view()
            .into_iter()
            .find(|p| p.provider_id.as_deref() == Some("fixture-one"))
            .unwrap();
        assert_eq!(
            (row.elapsed_seconds, row.remaining_seconds),
            (61, Some(239))
        );
        let deadline = first.deadline();
        assert!(registry.cancel(&row.request_id));
        first.cancelled().await;
        assert!(first.is_cancelled(deadline));
        assert!(second.deadline().is_some());
        assert_eq!(registry.view().len(), 1);
        first.acknowledge_cancel();
        first.begin();
        assert!(!registry.cancel(&row.request_id));
        assert_eq!(registry.view().len(), 2);
        drop(first);
        drop(second);
        assert!(registry.view().is_empty());
    }
    #[tokio::test(start_paused = true)]
    async fn output_and_expiry_reject_cancel_and_registries_are_isolated() {
        let a = Registry::new(broadcast::channel(16).0);
        let b = Registry::new(broadcast::channel(16).0);
        let budget = WaitBudget::tracked(300, &a);
        budget.begin();
        let id = a.view()[0].request_id.clone();
        assert!(!b.cancel(&id));
        budget.complete(budget.deadline());
        assert!(!a.cancel(&id));
        budget.begin();
        let id = a.view()[0].request_id.clone();
        tokio::time::advance(Duration::from_secs(300)).await;
        assert!(!a.cancel(&id));
    }
    #[test]
    fn output_and_cancel_have_one_atomic_winner() {
        for _ in 0..24 {
            let registry = Registry::new(broadcast::channel(16).0);
            let budget = WaitBudget::tracked(300, &registry);
            budget.begin();
            let id = registry.view()[0].request_id.clone();
            let deadline = budget.deadline();
            std::thread::scope(|scope| {
                let task = scope.spawn(|| registry.cancel(&id));
                budget.complete(deadline);
                assert_eq!(task.join().unwrap(), budget.is_cancelled(deadline));
            });
            assert!(registry.view().is_empty());
        }
    }
    #[tokio::test]
    async fn cancelled_output_race_keeps_protocol_cancelled() {
        let registry = Registry::new(broadcast::channel(16).0);
        let budget = WaitBudget::tracked(300, &registry);
        budget.begin();
        let mut protocol = crate::gateway::protocol::Protocol::new(true);
        protocol.attach_wait_budget(&budget);
        let id = registry.view()[0].request_id.clone();
        assert!(registry.cancel(&id));
        protocol.value(
            &serde_json::json!({"type":"response.completed","response":{"id":"fixture","status":"completed"}}),
        );
        protocol.finish(Some(200), crate::errors::code::OK);
        assert_eq!(
            protocol.terminal(),
            Some(crate::gateway::protocol::Terminal::Cancelled)
        );
        assert!(!protocol.succeeded());
        assert!(!protocol.transport_failure());
        assert!(registry.view().is_empty());
    }
}
