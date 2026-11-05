//! A shared dispatch gate. Only live requests are retained; no payload or
//! credential is copied into the state sent to windows.
use super::ClientId;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{sync::watch, time::Instant};

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActiveRequest {
    pub request_id: String,
    pub provider_id: Option<String>,
    pub model: Option<String>,
    pub started_at: u64,
    pub elapsed_seconds: u64,
    pub phase: String,
    pub tokens_per_second: Option<f64>,
    pub wait_id: Option<String>,
}
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Activity {
    pub client_id: ClientId,
    pub active_requests: Vec<ActiveRequest>,
}
#[derive(Default)]
struct Rate {
    second: u64,
    units: u64,
    previous: u64,
    seen: bool,
}
impl Rate {
    fn add(&mut self, second: u64, units: u64) {
        if second != self.second {
            self.previous = if second == self.second + 1 {
                self.units
            } else {
                0
            };
            self.units = 0;
            self.second = second;
        }
        self.seen = true;
        self.units += units;
    }
    fn value(&self, second: u64) -> Option<f64> {
        self.seen.then(|| {
            let units = if second == self.second {
                self.previous
            } else if second == self.second + 1 {
                self.units
            } else {
                0
            };
            units as f64 / 4.0
        })
    }
}

#[derive(Clone, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct View {
    pub frozen: bool,
    pub draining: usize,
    pub receiving: usize,
    pub waiting: usize,
    pub headers: usize,
    pub streaming: usize,
    pub finishing: usize,
    pub queued: usize,
}
#[derive(Clone, Copy, PartialEq)]
enum Phase {
    Receiving,
    Waiting,
    Headers,
    Streaming,
    Finishing,
    Done,
}
struct Request {
    client: Option<ClientId>,
    provider: Option<String>,
    model: Option<String>,
    wait_id: Option<String>,
    wait_phase: String,
    started: Instant,
    started_at: u64,
    rate: Rate,
    phase: Phase,
    paused: Duration,
    paused_since: Option<Instant>,
}
impl Request {
    fn paused_for(&self) -> Duration {
        self.paused + self.paused_since.map_or(Duration::ZERO, |at| at.elapsed())
    }
    fn resume_clock(&mut self) {
        self.paused = self.paused_for();
        self.paused_since = None;
    }
}
#[derive(Default)]
struct State {
    frozen: bool,
    next: u64,
    requests: BTreeMap<u64, Request>,
}
struct Shared {
    clock: Instant,
    state: Mutex<State>,
    changes: watch::Sender<()>,
}
#[derive(Clone)]
pub struct Gate(Arc<Shared>);
impl Default for Gate {
    fn default() -> Self {
        Self(Arc::new(Shared {
            clock: Instant::now(),
            state: Mutex::default(),
            changes: watch::channel(()).0,
        }))
    }
}
impl Gate {
    pub fn active_requests(&self, client: ClientId) -> Vec<ActiveRequest> {
        let s = self.0.state.lock().unwrap();
        let second = self.0.clock.elapsed().as_secs();
        s.requests
            .iter()
            .filter_map(|(id, r)| {
                if r.client != Some(client) || r.phase == Phase::Done {
                    return None;
                }
                let phase = match r.phase {
                    Phase::Receiving => "receiving",
                    Phase::Waiting if s.frozen => "frozen",
                    Phase::Waiting => &r.wait_phase,
                    Phase::Headers | Phase::Streaming if r.wait_phase == "compaction" => {
                        "compaction"
                    }
                    Phase::Headers => "headers",
                    Phase::Streaming => "streaming",
                    Phase::Finishing => "finishing",
                    Phase::Done => unreachable!(),
                };
                Some(ActiveRequest {
                    request_id: id.to_string(),
                    provider_id: r.provider.clone(),
                    model: r.model.clone(),
                    started_at: r.started_at,
                    elapsed_seconds: r.started.elapsed().saturating_sub(r.paused_for()).as_secs(),
                    phase: phase.into(),
                    tokens_per_second: r.rate.value(second),
                    wait_id: r.wait_id.clone(),
                })
            })
            .collect()
    }
    pub fn activities(&self) -> Vec<Activity> {
        [
            ClientId::Codex,
            ClientId::Claude,
            ClientId::Grok,
            ClientId::Dsh,
        ]
        .into_iter()
        .map(|client_id| Activity {
            client_id,
            active_requests: self.active_requests(client_id),
        })
        .collect()
    }
    pub fn has_activity(&self) -> bool {
        self.0
            .state
            .lock()
            .unwrap()
            .requests
            .values()
            .any(|r| r.client.is_some() && r.phase != Phase::Done)
    }
    pub fn refresh(&self) {
        self.0.changes.send_replace(());
    }
    fn view_locked(s: &State) -> View {
        let mut view = View {
            frozen: s.frozen,
            ..Default::default()
        };
        for r in s.requests.values() {
            match r.phase {
                Phase::Receiving => view.receiving += 1,
                Phase::Waiting if s.frozen => view.queued += 1,
                Phase::Waiting => view.waiting += 1,
                Phase::Headers => view.headers += 1,
                Phase::Streaming => view.streaming += 1,
                Phase::Finishing => view.finishing += 1,
                Phase::Done => {}
            }
        }
        view.draining = view.receiving + view.headers + view.streaming + view.finishing;
        view
    }
    pub fn view(&self) -> View {
        Self::view_locked(&self.0.state.lock().unwrap())
    }
    pub fn is_frozen(&self) -> bool {
        self.0.state.lock().unwrap().frozen
    }
    pub fn subscribe(&self) -> watch::Receiver<()> {
        self.0.changes.subscribe()
    }
    pub fn set(&self, frozen: bool) -> crate::storage::Result<View> {
        let mut s = self.0.state.lock().unwrap();
        if !frozen && s.frozen && Self::view_locked(&s).draining != 0 {
            return Err(crate::usage::model::failure("等待收尾完成后才能解冻"));
        }
        if s.frozen != frozen {
            s.frozen = frozen;
            for r in s.requests.values_mut() {
                if frozen && r.phase == Phase::Waiting {
                    r.paused_since = Some(Instant::now());
                } else if !frozen {
                    r.resume_clock();
                }
            }
            self.0.changes.send_replace(());
        }
        Ok(Self::view_locked(&s))
    }
    pub fn request(&self) -> Ticket {
        self.new_request(None)
    }
    pub fn request_for(&self, client: ClientId) -> Ticket {
        self.new_request(Some(client))
    }
    fn new_request(&self, client: Option<ClientId>) -> Ticket {
        let mut s = self.0.state.lock().unwrap();
        s.next += 1;
        let id = s.next;
        s.requests.insert(
            id,
            Request {
                client,
                provider: None,
                model: None,
                wait_id: None,
                wait_phase: "admission".into(),
                started: Instant::now(),
                started_at: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
                rate: Rate::default(),
                phase: Phase::Receiving,
                paused: Duration::ZERO,
                paused_since: None,
            },
        );
        self.0.changes.send_replace(());
        Ticket(Arc::new(Entry {
            gate: self.clone(),
            id,
        }))
    }
}
struct Entry {
    gate: Gate,
    id: u64,
}
impl Drop for Entry {
    fn drop(&mut self) {
        self.gate.0.state.lock().unwrap().requests.remove(&self.id);
        self.gate.0.changes.send_replace(());
    }
}
#[derive(Clone)]
pub struct Ticket(Arc<Entry>);
pub async fn changed(receiver: &mut Option<watch::Receiver<()>>) {
    if let Some(receiver) = receiver {
        let _ = receiver.changed().await;
    } else {
        std::future::pending::<()>().await;
    }
}
impl Ticket {
    pub fn describe(&self, wait: &super::wait_budget::PendingRequest) {
        let mut s = self.0.gate.0.state.lock().unwrap();
        if let Some(r) = s.requests.get_mut(&self.0.id) {
            r.wait_id = Some(wait.request_id.clone());
            r.provider.clone_from(&wait.provider_id);
            r.model.clone_from(&wait.model);
            r.wait_phase.clone_from(&wait.phase);
            self.0.gate.refresh();
        }
    }
    pub fn clear_wait(&self, id: &str) {
        let mut s = self.0.gate.0.state.lock().unwrap();
        if let Some(r) = s
            .requests
            .get_mut(&self.0.id)
            .filter(|r| r.wait_id.as_deref() == Some(id))
        {
            r.wait_id = None;
            self.0.gate.refresh();
        }
    }
    pub fn output(&self, units: u64) {
        let second = self.0.gate.0.clock.elapsed().as_secs();
        if let Some(r) = self
            .0
            .gate
            .0
            .state
            .lock()
            .unwrap()
            .requests
            .get_mut(&self.0.id)
        {
            if r.phase != Phase::Done {
                r.rate.add(second, units);
            }
        }
    }
    fn phase(&self, phase: Phase) {
        let mut s = self.0.gate.0.state.lock().unwrap();
        let frozen = s.frozen;
        if let Some(r) = s.requests.get_mut(&self.0.id) {
            if r.phase == phase || r.phase == Phase::Done {
                return;
            }
            r.resume_clock();
            r.phase = phase;
            if frozen && phase == Phase::Waiting {
                r.paused_since = Some(Instant::now());
            }
            self.0.gate.0.changes.send_replace(());
        }
    }
    pub fn waiting(&self) {
        self.phase(Phase::Waiting);
    }
    pub fn streaming(&self) {
        self.phase(Phase::Streaming);
    }
    pub fn finishing(&self) {
        self.phase(Phase::Finishing);
    }
    pub fn finish(&self) {
        self.phase(Phase::Done);
    }
    pub fn paused(&self) -> bool {
        let s = self.0.gate.0.state.lock().unwrap();
        s.frozen
            && s.requests
                .get(&self.0.id)
                .is_some_and(|r| r.phase == Phase::Waiting)
    }
    pub fn paused_for(&self) -> Duration {
        self.0
            .gate
            .0
            .state
            .lock()
            .unwrap()
            .requests
            .get(&self.0.id)
            .map_or(Duration::ZERO, Request::paused_for)
    }
    pub fn subscribe(&self) -> watch::Receiver<()> {
        self.0.gate.subscribe()
    }
    /// Linearizes admission against the freeze click. A granted request drains;
    /// a losing request remains queued without reserving upstream capacity.
    pub fn dispatch(&self) -> bool {
        let mut s = self.0.gate.0.state.lock().unwrap();
        if s.frozen {
            return false;
        }
        let Some(r) = s.requests.get_mut(&self.0.id) else {
            return false;
        };
        r.resume_clock();
        r.phase = Phase::Headers;
        r.rate = Rate::default();
        self.0.gate.0.changes.send_replace(());
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test(start_paused = true)]
    async fn activity_keeps_platforms_and_second_buckets_separate_until_delivery_finishes() {
        let gate = Gate::default();
        let request = gate.request_for(ClientId::Codex);
        let other = gate.request_for(ClientId::Claude);
        assert_eq!(gate.active_requests(ClientId::Codex)[0].phase, "receiving");
        assert_eq!(gate.active_requests(ClientId::Claude).len(), 1);
        assert!(gate.active_requests(ClientId::Grok).is_empty());
        request.waiting();
        assert!(request.dispatch());
        request.streaming();
        request.output(1);
        request.output(12);
        assert_eq!(
            gate.active_requests(ClientId::Codex)[0].tokens_per_second,
            Some(0.0)
        );
        tokio::time::advance(Duration::from_secs(1)).await;
        assert_eq!(
            gate.active_requests(ClientId::Codex)[0].tokens_per_second,
            Some(3.25)
        );
        request.output(4);
        assert_eq!(
            gate.active_requests(ClientId::Codex)[0].tokens_per_second,
            Some(3.25)
        );
        tokio::time::advance(Duration::from_secs(1)).await;
        assert_eq!(
            gate.active_requests(ClientId::Codex)[0].tokens_per_second,
            Some(1.0)
        );
        tokio::time::advance(Duration::from_secs(1)).await;
        assert_eq!(
            gate.active_requests(ClientId::Codex)[0].tokens_per_second,
            Some(0.0)
        );
        request.finishing();
        assert_eq!(gate.active_requests(ClientId::Codex)[0].phase, "finishing");
        request.finish();
        assert!(gate.active_requests(ClientId::Codex).is_empty());
        drop(other);
        assert!(!gate.has_activity());
    }
    #[tokio::test(start_paused = true)]
    async fn activity_tracks_compaction_wait_cancel_and_retry_provider_without_extending_budget() {
        let gate = Gate::default();
        let ticket = gate.request_for(ClientId::Codex);
        let events = tokio::sync::broadcast::channel(16).0;
        let registry = super::super::wait_budget::Registry::new(events);
        let budget = super::super::wait_budget::WaitBudget::tracked(60, &registry);
        budget.begin();
        budget.attach_traffic(&ticket);
        budget.describe(Some("first"), Some("fixture-model"), "headers");
        assert!(ticket.dispatch());
        budget.compaction_execution(budget.deadline(), true);
        let view = gate.active_requests(ClientId::Codex).remove(0);
        assert_eq!(view.phase, "compaction");
        assert!(view.wait_id.is_some());
        budget.describe(Some("second"), None, "retry");
        let view = gate.active_requests(ClientId::Codex).remove(0);
        assert_eq!(view.provider_id.as_deref(), Some("second"));
        assert_eq!(view.phase, "retry");
        gate.set(true).unwrap();
        tokio::time::advance(Duration::from_secs(120)).await;
        let view = gate.active_requests(ClientId::Codex).remove(0);
        assert_eq!(view.phase, "frozen");
        assert_eq!(view.elapsed_seconds, 0);
        assert!(registry.cancel(view.wait_id.as_deref().unwrap()));
        assert!(gate.active_requests(ClientId::Codex)[0].wait_id.is_none());
        drop(budget);
        drop(ticket);
        assert!(gate.active_requests(ClientId::Codex).is_empty());
    }
    #[tokio::test(start_paused = true)]
    async fn drain_blocks_thaw_and_waiting_time_is_excluded() {
        let gate = Gate::default();
        let current = gate.request();
        current.waiting();
        assert!(current.dispatch());
        current.streaming();
        let queued = gate.request();
        queued.waiting();
        let view = gate.set(true).unwrap();
        assert_eq!((view.draining, view.streaming, view.queued), (1, 1, 1));
        assert!(!queued.dispatch());
        assert!(gate.set(false).is_err());
        tokio::time::advance(Duration::from_secs(3600)).await;
        assert_eq!(queued.paused_for(), Duration::from_secs(3600));
        assert_eq!(current.paused_for(), Duration::ZERO);
        current.finishing();
        assert_eq!(gate.view().draining, 1);
        current.finish();
        assert_eq!(gate.view().draining, 0);
        gate.set(false).unwrap();
        assert!(queued.dispatch());
        drop(queued);
        assert_eq!(gate.view().draining, 0);
    }
    #[test]
    fn cancelled_waiter_is_removed_and_idle_connections_are_not_requests() {
        let gate = Gate::default();
        gate.set(true).unwrap();
        let request = gate.request();
        assert_eq!(gate.view().draining, 1);
        request.waiting();
        assert_eq!(gate.view().queued, 1);
        drop(request);
        assert_eq!(gate.view().queued, 0);
        assert_eq!(gate.view().draining, 0);
    }
}
