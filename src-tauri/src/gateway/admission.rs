//! Request slots and bounded FIFO admission. Behavior informed by Sub2API
//! a3eb7ef3 concurrency_service / account scheduler; independently implemented in Rust.
use super::rpm::{Ledger, Settlement};
use super::{
    circuit::Circuit, health_settlement::Permits, model::Provider, routing::Requirement, Route,
};
use crate::storage;
use std::{
    collections::{HashMap, VecDeque},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    sync::{broadcast, Notify},
    time::Instant,
};

#[cfg(test)]
#[path = "admission_persistence_tests.rs"]
mod persistence_tests;

#[derive(Clone)]
pub struct Scheduler(Arc<Shared>);
struct Shared {
    state: Mutex<State>,
    wake: Notify,
    events: broadcast::Sender<()>,
    persist: std::sync::mpsc::SyncSender<()>,
}
struct State {
    running: bool,
    epoch: u64,
    next: u64,
    limits: HashMap<String, u32>,
    order: HashMap<String, usize>,
    queued: std::collections::HashSet<String>,
    versions: HashMap<String, String>,
    rpm_limits: HashMap<String, u32>,
    models: HashMap<String, Option<Vec<String>>>,
    intake: HashMap<String, (bool, bool)>,
    active: HashMap<String, usize>,
    retention: super::thread_capacity::Retention,
    // A failed write can be retried from the still-authoritative in-memory state.
    // Retention::error instead means the initial ledger could not be trusted.
    retention_write_error: bool,
    rpm: Ledger,
    rpm_pending: HashMap<String, usize>,
    rpm_path: PathBuf,
    rpm_error: bool,
    waiting: VecDeque<(u64, Vec<String>)>,
    capacity_waits: HashMap<u64, (String, Instant, WaitKind)>,
    resets: HashMap<String, u64>,
}
impl State {
    fn new(rpm_path: PathBuf, rpm: Ledger, rpm_error: bool) -> Self {
        Self {
            retention: super::thread_capacity::Retention::load(
                &rpm_path.with_file_name("thread-reservations.json"),
            ),
            retention_write_error: false,
            running: false,
            epoch: 0,
            next: 0,
            limits: HashMap::new(),
            order: HashMap::new(),
            queued: Default::default(),
            versions: HashMap::new(),
            rpm_limits: HashMap::new(),
            models: HashMap::new(),
            intake: HashMap::new(),
            active: HashMap::new(),
            rpm,
            rpm_pending: HashMap::new(),
            rpm_path,
            rpm_error,
            waiting: VecDeque::new(),
            capacity_waits: HashMap::new(),
            resets: HashMap::new(),
        }
    }
}
pub(super) fn now_wall_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}
#[derive(Clone)]
pub struct CapacitySource {
    pub provider_id: String,
    pub reset_generation: u64,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum WaitKind {
    Capacity,
    ExistingCapacity,
    Websocket,
    Transient,
}
#[derive(serde::Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct CapacityRetry {
    pub provider_id: String,
    pub retry_in: u64,
    pub existing_owner: bool,
}
pub struct Slot {
    scheduler: Scheduler,
    id: String,
    thread: Option<Arc<ThreadRequest>>,
    success: bool,
}
pub struct ThreadRequest {
    scheduler: Scheduler,
    ticket: u64,
    retain_owner: Option<String>,
}
impl ThreadRequest {
    fn retains(&self, provider: &str) -> bool {
        self.retain_owner
            .as_deref()
            .is_none_or(|owner| owner == provider)
    }
}
impl Drop for ThreadRequest {
    fn drop(&mut self) {
        self.scheduler
            .0
            .state
            .lock()
            .unwrap()
            .retention
            .finish(self.ticket);
        self.scheduler.signal();
    }
}
impl Slot {
    pub fn complete(&mut self) {
        self.success = true;
    }
}

struct RpmReservation {
    scheduler: Scheduler,
    id: String,
    committed: bool,
    version: String,
    require_queued: bool,
    epoch: u64,
    completion: Option<Completion>,
}
/// Moved from admission to the protocol observer after actual send commitment.
/// Drop retains an uncertainty guard, never inventing an upstream success.
pub(super) struct Completion {
    scheduler: Scheduler,
    id: String,
    token: Option<String>,
}
impl Completion {
    pub(super) fn settle(&mut self, result: Settlement) {
        let Some(token) = self.token.take() else {
            return;
        };
        let mut s = self.scheduler.0.state.lock().unwrap();
        s.rpm.prune();
        let previous = s.rpm.checkpoint(&self.id);
        s.rpm.settle(&self.id, &token, result);
        if s.rpm_error || s.rpm.save(&s.rpm_path).is_err() {
            s.rpm.restore(&self.id, previous);
            s.rpm_error = true;
        }
        drop(s);
        self.scheduler.signal();
    }
}
impl Drop for Completion {
    fn drop(&mut self) {
        self.settle(Settlement::Uncertain);
    }
}

pub(super) struct Waiting {
    scheduler: Scheduler,
    ticket: u64,
}
struct CapacityWait {
    scheduler: Scheduler,
    ticket: u64,
}
pub struct Admission {
    pub handoff_wait: Option<crate::usage::model::RoutingDecision>,
    pub route: Route,
    pub permits: Permits,
    pub slot: Slot,
    rpm: Option<RpmReservation>,
    pub reset_generation: u64,
}
#[derive(Debug, PartialEq)]
pub enum Rejected {
    Reconnect,
    Stopped,
    Unavailable,
    Full,
    Timeout,
    Cooling(u64),
    Model,
    Policy,
    RateLimited(u64),
    RateLedger,
}
pub struct Budget {
    pub traffic: Option<super::freeze::Ticket>,
    pub thread: Option<Arc<ThreadRequest>>,
    pub handoff_owner: Option<String>,
    pub capacity_owner: Option<String>,
    pub reconnect_from: Option<String>,
    remaining: Duration,
    protected_capacity: bool,
    fixed_provider: bool,
    intake: Option<(super::compaction::Intake, Option<String>)>,
}
impl Admission {
    pub(super) fn take_completion(&mut self) -> Option<Completion> {
        self.rpm.as_mut().and_then(|r| r.completion.take())
    }
    pub fn commit_rpm(&mut self) -> Result<(), Rejected> {
        if let Some(rpm) = self.rpm.as_mut() {
            rpm.commit()?;
        }
        Ok(())
    }
}
impl RpmReservation {
    fn commit(&mut self) -> Result<(), Rejected> {
        if self.committed {
            return Ok(());
        }
        let mut s = self.scheduler.0.state.lock().unwrap();
        let limit = s.rpm_limits.get(&self.id).copied().unwrap_or(0);
        s.rpm.prune();
        let rejected = if !s.running || s.epoch != self.epoch {
            Some(Rejected::Stopped)
        } else if s.versions.get(&self.id) != Some(&self.version)
            || (self.require_queued && !s.queued.contains(&self.id))
        {
            Some(Rejected::Unavailable)
        } else if s.rpm_error && limit > 0 {
            Some(Rejected::RateLedger)
        } else {
            s.rpm
                .gate(&self.id, limit, 0)
                .map(|(delay, _)| Rejected::RateLimited(delay.as_millis().div_ceil(1000) as u64))
        };
        if let Some(reason) = rejected {
            Self::release_pending(&mut s, &self.id);
            self.committed = true;
            drop(s);
            self.scheduler.signal();
            return Err(reason);
        }
        let previous = s.rpm.checkpoint(&self.id);
        let token = s.rpm.commit(&self.id, limit);
        if token.is_some() && (s.rpm_error || s.rpm.save(&s.rpm_path).is_err()) && limit > 0 {
            s.rpm.restore(&self.id, previous);
            s.rpm_error = true;
            Self::release_pending(&mut s, &self.id);
            self.committed = true;
            drop(s);
            self.scheduler.signal();
            return Err(Rejected::RateLedger);
        }
        Self::release_pending(&mut s, &self.id);
        self.committed = true;
        self.completion = token.map(|token| Completion {
            scheduler: self.scheduler.clone(),
            id: self.id.clone(),
            token: Some(token),
        });
        drop(s);
        self.scheduler.signal();
        Ok(())
    }
    fn release_pending(state: &mut State, id: &str) {
        if let Some(value) = state.rpm_pending.get_mut(id) {
            *value = value.saturating_sub(1);
            if *value == 0 {
                state.rpm_pending.remove(id);
            }
        }
    }
}
impl Drop for RpmReservation {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        let mut s = self.scheduler.0.state.lock().unwrap();
        Self::release_pending(&mut s, &self.id);
        drop(s);
        self.scheduler.signal();
    }
}
impl Budget {
    pub fn track(
        &mut self,
        scheduler: &Scheduler,
        lease: Option<&super::compaction::Lease>,
        seconds: u64,
    ) {
        if self.thread.is_some() {
            return;
        }
        if let Some(lease) = lease.filter(|lease| lease.follows_owner()) {
            self.handoff_owner = lease.handoff.then(|| lease.owner.clone()).flatten();
            let mut state = scheduler.0.state.lock().unwrap();
            state.retention.seconds = seconds;
            let ticket = state
                .retention
                .begin(lease.session_digest(), lease.owner.as_deref());
            drop(state);
            self.thread = Some(Arc::new(ThreadRequest {
                scheduler: scheduler.clone(),
                ticket,
                retain_owner: lease.request_only().then(|| lease.owner.clone()).flatten(),
            }));
        }
    }

    pub(super) fn set_intake(&mut self, intake: super::compaction::Intake, owner: Option<String>) {
        self.intake = Some((intake, owner));
    }
    pub(super) fn search_fallback(&mut self) {
        self.fixed_provider = false;
        self.intake = None;
        self.handoff_owner = None;
        self.capacity_owner = None;
        self.reconnect_from = None;
    }
    pub fn protect_capacity(&mut self) {
        self.protected_capacity = true;
    }
    pub fn pin_provider(&mut self) {
        self.fixed_provider = true;
        self.capacity_owner = None;
        self.reconnect_from = None;
        self.intake = None;
    }
    pub fn new(seconds: u64) -> Self {
        Self {
            traffic: None,
            thread: None,
            handoff_owner: None,
            capacity_owner: None,
            reconnect_from: None,
            remaining: Duration::from_secs(seconds),
            protected_capacity: false,
            fixed_provider: false,
            intake: None,
        }
    }
}
impl Scheduler {
    /// Thread waiters share the global queue limit without reserving a provider.
    pub(super) fn thread_wait(&self, max_waiting: usize) -> Result<Waiting, Rejected> {
        let mut s = self.0.state.lock().unwrap();
        if !s.running {
            return Err(Rejected::Stopped);
        }
        if s.waiting.len() + s.capacity_waits.len() >= max_waiting {
            return Err(Rejected::Full);
        }
        s.next += 1;
        let ticket = s.next;
        s.waiting.push_back((ticket, Vec::new()));
        drop(s);
        self.signal();
        Ok(Waiting {
            scheduler: self.clone(),
            ticket,
        })
    }

    pub fn new(events: broadcast::Sender<()>, rpm_path: PathBuf) -> Self {
        let (rpm, rpm_error) = Ledger::load(&rpm_path);
        let (persist, updates) = std::sync::mpsc::sync_channel(1);
        let path = rpm_path.with_file_name("thread-reservations.json");
        let shared = Arc::new(Shared {
            state: Mutex::new(State::new(rpm_path, rpm, rpm_error)),
            wake: Notify::new(),
            events,
            persist,
        });
        let weak = Arc::downgrade(&shared);
        std::thread::spawn(move || {
            let mut previous = Vec::new();
            while updates.recv().is_ok() {
                // A transient failure of the final update otherwise waits forever
                // for another request. Retry here, without adding a polling task.
                let delays = [100, 250, 500];
                for retry in 0..=delays.len() {
                    let Some(shared) = weak.upgrade() else {
                        return;
                    };
                    let (snapshot, retry_write) = {
                        let mut state = shared.state.lock().unwrap();
                        if state.retention.error {
                            break;
                        }
                        state.retention.prune();
                        (state.retention.snapshot(), state.retention_write_error)
                    };
                    if snapshot == previous && !retry_write {
                        break;
                    }
                    if storage::atomic_write(&path, &snapshot, None).is_err() {
                        shared.state.lock().unwrap().retention_write_error = true;
                        let _ = shared.events.send(());
                        let Some(delay) = delays.get(retry) else {
                            break;
                        };
                        drop(shared);
                        // New state can wake the same retry early. Each attempt
                        // snapshots memory again, never republishes a stale copy.
                        if matches!(
                            updates.recv_timeout(Duration::from_millis(*delay)),
                            Err(std::sync::mpsc::RecvTimeoutError::Disconnected)
                        ) {
                            return;
                        }
                    } else {
                        previous = snapshot;
                        let recovered = {
                            let mut state = shared.state.lock().unwrap();
                            std::mem::take(&mut state.retention_write_error)
                        };
                        if recovered {
                            shared.wake.notify_waiters();
                            let _ = shared.events.send(());
                        }
                        break;
                    }
                }
            }
        });
        Self(shared)
    }
    pub fn configure_retention(&self, seconds: u64) {
        let mut s = self.0.state.lock().unwrap();
        let providers = s.limits.keys().cloned().collect();
        s.retention.configure(&providers, seconds);
    }
    pub fn reserved(&self, id: &str) -> usize {
        self.0.state.lock().unwrap().retention.reserved(id)
    }
    pub fn retention_error(&self) -> bool {
        let state = self.0.state.lock().unwrap();
        state.retention.error || state.retention_write_error
    }
    #[cfg(test)]
    pub(super) fn retention_snapshot(&self) -> Vec<u8> {
        self.0.state.lock().unwrap().retention.snapshot()
    }
    fn available(s: &State, id: &str, request: Option<u64>, manual: bool) -> bool {
        let limit = s.limits[id];
        let occupied = s.active.get(id).copied().unwrap_or(0)
            + if manual { 0 } else { s.retention.reserved(id) };
        limit == 0 || (!manual && s.retention.reusable(request, id)) || occupied < limit as usize
    }

    pub fn configure(&self, providers: &[Provider], running: bool) {
        let mut s = self.0.state.lock().unwrap();
        let changed: Vec<_> =
            s.versions
                .iter()
                .filter(|(id, version)| {
                    providers.iter().find(|p| &p.id == *id).is_none_or(|p| {
                        p.version != **version || p.queued != s.queued.contains(*id)
                    })
                })
                .map(|(id, _)| id.clone())
                .collect();
        for id in changed {
            *s.resets.entry(id).or_default() += 1;
        }
        s.versions = providers
            .iter()
            .map(|p| (p.id.clone(), p.version.clone()))
            .collect();
        if s.running != running {
            s.epoch += 1;
        }
        s.running = running;
        s.limits = providers
            .iter()
            .map(|p| (p.id.clone(), p.max_concurrency))
            .collect();
        let mut newly_limited = false;
        for provider in providers {
            if provider.max_rpm > 0 && s.rpm_limits.get(&provider.id) == Some(&0) {
                s.rpm.enable(&provider.id);
                newly_limited = true;
            }
        }
        if newly_limited && !s.rpm_error && s.rpm.save(&s.rpm_path).is_err() {
            s.rpm_error = true;
        }
        s.rpm_limits = providers
            .iter()
            .map(|p| (p.id.clone(), p.max_rpm))
            .collect();
        s.models = providers
            .iter()
            .map(|p| (p.id.clone(), p.allowed_models.clone()))
            .collect();
        s.intake = providers
            .iter()
            .map(|p| {
                (
                    p.id.clone(),
                    (p.handoff_after_compaction, p.take_new_threads),
                )
            })
            .collect();
        s.order = providers
            .iter()
            .enumerate()
            .map(|(i, p)| (p.id.clone(), i))
            .collect();
        s.queued = providers
            .iter()
            .filter(|p| p.queued)
            .map(|p| p.id.clone())
            .collect();
        s.rpm.prune();
        drop(s);
        self.signal();
    }
    pub fn rpm_status(&self, id: &str, limit: u32) -> (usize, u64, bool, bool) {
        let mut s = self.0.state.lock().unwrap();
        s.rpm.prune();
        let used = if limit == 0 { 0 } else { s.rpm.used(id) };
        let gate = s.rpm.gate(
            id,
            limit,
            s.rpm_pending.get(id).copied().unwrap_or_default(),
        );
        (
            used,
            gate.map_or(0, |(d, _)| d.as_millis().div_ceil(1000) as u64),
            gate.is_some(),
            s.rpm_error && limit > 0,
        )
    }
    pub(super) fn rpm_wait_reason(
        &self,
        id: &str,
        limit: u32,
    ) -> Option<crate::events::RpmWaitReason> {
        let mut s = self.0.state.lock().unwrap();
        s.rpm.prune();
        s.rpm
            .gate(
                id,
                limit,
                s.rpm_pending.get(id).copied().unwrap_or_default(),
            )
            .map(|(_, reason)| reason)
    }
    pub(super) fn rpm_backoff(&self, id: &str, delay: Duration) {
        let mut s = self.0.state.lock().unwrap();
        s.rpm.prune();
        s.rpm.backoff(id, delay);
        if !s.rpm_error && s.rpm.save(&s.rpm_path).is_err() {
            s.rpm_error = true;
        }
        drop(s);
        self.signal();
    }
    fn reserve_rpm(
        &self,
        state: &mut State,
        id: &str,
        limit: u32,
        require_queued: bool,
    ) -> Result<RpmReservation, Rejected> {
        state.rpm.prune();
        if state.rpm_error && limit > 0 {
            return Err(Rejected::RateLedger);
        }
        if let Some((delay, _)) = state.rpm.gate(
            id,
            limit,
            state.rpm_pending.get(id).copied().unwrap_or_default(),
        ) {
            return Err(Rejected::RateLimited(
                delay.as_millis().div_ceil(1000) as u64
            ));
        }
        // Unlimited reservations still revalidate on commit, including 0 -> N edits.
        *state.rpm_pending.entry(id.into()).or_default() += 1;
        Ok(RpmReservation {
            scheduler: self.clone(),
            id: id.into(),
            committed: false,
            version: state.versions.get(id).cloned().unwrap_or_default(),
            require_queued,
            epoch: state.epoch,
            completion: None,
        })
    }
    pub fn counts(&self) -> (HashMap<String, usize>, usize) {
        let mut s = self.0.state.lock().unwrap();
        let changed = s.retention.prune();
        let result = (s.active.clone(), s.waiting.len() + s.capacity_waits.len());
        drop(s);
        if changed {
            self.signal();
        }
        result
    }
    pub(super) fn capacity_available(&self, id: &str, budget: &Budget, manual: bool) -> bool {
        let mut state = self.0.state.lock().unwrap();
        state.retention.prune();
        Self::available(&state, id, budget.thread.as_ref().map(|t| t.ticket), manual)
    }
    pub fn capacity_retries(&self) -> Vec<CapacityRetry> {
        let mut retries = self.retries(WaitKind::Capacity);
        retries.extend(self.retries(WaitKind::ExistingCapacity));
        retries
    }
    pub fn websocket_retries(&self) -> Vec<CapacityRetry> {
        self.retries(WaitKind::Websocket)
    }
    pub fn transient_retries(&self) -> Vec<CapacityRetry> {
        self.retries(WaitKind::Transient)
    }
    fn retries(&self, kind: WaitKind) -> Vec<CapacityRetry> {
        let s = self.0.state.lock().unwrap();
        let mut result: Vec<_> = s
            .capacity_waits
            .values()
            .filter(|(_, _, wait)| *wait == kind)
            .map(|(id, until, _)| CapacityRetry {
                provider_id: id.clone(),
                existing_owner: kind == WaitKind::ExistingCapacity,
                retry_in: until
                    .saturating_duration_since(Instant::now())
                    .as_millis()
                    .div_ceil(1000) as u64,
            })
            .collect();
        result.sort_by(|a, b| {
            a.provider_id
                .cmp(&b.provider_id)
                .then(a.retry_in.cmp(&b.retry_in))
        });
        result
    }
    pub async fn wait_capacity(
        &self,
        sources: &[CapacitySource],
        delay: Duration,
        max_waiting: usize,
    ) -> Result<(), Rejected> {
        self.wait_kind(sources, delay, max_waiting, WaitKind::Capacity)
            .await
    }
    pub fn capacity_source(&self, id: &str) -> CapacitySource {
        CapacitySource {
            provider_id: id.into(),
            reset_generation: self
                .0
                .state
                .lock()
                .unwrap()
                .resets
                .get(id)
                .copied()
                .unwrap_or(0),
        }
    }
    pub async fn wait_owner_capacity(
        &self,
        sources: &[CapacitySource],
        delay: Duration,
        max_waiting: usize,
    ) -> Result<(), Rejected> {
        self.wait_kind(sources, delay, max_waiting, WaitKind::ExistingCapacity)
            .await
    }
    pub async fn wait_transient(
        &self,
        source: CapacitySource,
        delay: Duration,
        max_waiting: usize,
    ) -> Result<(), Rejected> {
        self.wait_kind(&[source], delay, max_waiting, WaitKind::Transient)
            .await
    }
    pub async fn wait_websocket(
        &self,
        id: &str,
        delay: Duration,
        max_waiting: usize,
    ) -> Result<(), Rejected> {
        let generation = self
            .0
            .state
            .lock()
            .unwrap()
            .resets
            .get(id)
            .copied()
            .unwrap_or(0);
        self.wait_kind(
            &[CapacitySource {
                provider_id: id.into(),
                reset_generation: generation,
            }],
            delay,
            max_waiting,
            WaitKind::Websocket,
        )
        .await
    }
    async fn wait_kind(
        &self,
        sources: &[CapacitySource],
        delay: Duration,
        max_waiting: usize,
        kind: WaitKind,
    ) -> Result<(), Rejected> {
        let until = Instant::now() + delay;
        let (ticket, epoch) = {
            let mut s = self.0.state.lock().unwrap();
            if !s.running {
                return Err(Rejected::Stopped);
            }
            if Self::was_reset(&s, sources) {
                return Ok(());
            }
            if s.waiting.len() + s.capacity_waits.len() >= max_waiting {
                return Err(Rejected::Full);
            }
            s.next += 1;
            let ticket = s.next;
            s.capacity_waits.insert(
                ticket,
                (
                    sources
                        .last()
                        .map(|s| s.provider_id.clone())
                        .unwrap_or_default(),
                    until,
                    kind,
                ),
            );
            (ticket, s.epoch)
        };
        let _wait = CapacityWait {
            scheduler: self.clone(),
            ticket,
        };
        self.signal();
        loop {
            let notified = self.0.wake.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let s = self.0.state.lock().unwrap();
                if !s.running || s.epoch != epoch {
                    return Err(Rejected::Stopped);
                }
                if Self::was_reset(&s, sources) {
                    return Ok(());
                }
            }
            tokio::select! {
                _ = notified => {},
                _ = tokio::time::sleep_until(until) => return Ok(()),
            }
        }
    }
    pub(super) fn capacity_current(&self, sources: &[CapacitySource]) -> bool {
        let s = self.0.state.lock().unwrap();
        s.running && !Self::was_reset(&s, sources)
    }
    fn was_reset(state: &State, sources: &[CapacitySource]) -> bool {
        sources.iter().any(|source| {
            state.resets.get(&source.provider_id).copied().unwrap_or(0) != source.reset_generation
        })
    }
    pub fn reset_provider(&self, id: &str, circuits: &[Circuit]) {
        // Same lock order as admission: slot state, then circuit. Capture the
        // reset generation with each reservation so a reset during an upstream
        // attempt cannot be lost before that attempt registers its wait.
        let mut state = self.0.state.lock().unwrap();
        for circuit in circuits {
            circuit.reset();
        }
        *state.resets.entry(id.to_owned()).or_default() += 1;
        drop(state);
        self.signal();
    }
    pub fn signal(&self) {
        let _ = self.0.persist.try_send(());
        self.0.wake.notify_waiters();
        let _ = self.0.events.send(());
    }
    #[cfg(test)]
    pub async fn acquire(
        &self,
        routes: &[Route],
        manual: bool,
        max_waiting: usize,
        budget: &mut Budget,
    ) -> Result<Admission, Rejected> {
        self.acquire_for(routes, manual, max_waiting, budget, &Requirement::Resource)
            .await
    }
    #[cfg(test)]
    pub async fn acquire_for(
        &self,
        routes: &[Route],
        manual: bool,
        max_waiting: usize,
        budget: &mut Budget,
        requirement: &Requirement,
    ) -> Result<Admission, Rejected> {
        self.acquire_for_immediate(routes, manual, max_waiting, budget, requirement, false)
            .await
    }
    pub async fn acquire_for_immediate(
        &self,
        routes: &[Route],
        manual: bool,
        max_waiting: usize,
        budget: &mut Budget,
        requirement: &Requirement,
        immediate: bool,
    ) -> Result<Admission, Rejected> {
        let started = Instant::now();
        if let Some(traffic) = &budget.traffic {
            traffic.waiting();
        }
        let paused_before = budget
            .traffic
            .as_ref()
            .map_or(Duration::ZERO, |t| t.paused_for());
        let mut paused = Duration::ZERO;
        let mut freeze_changes = budget.traffic.as_ref().map(|t| t.subscribe());
        let epoch = {
            let mut state = self.0.state.lock().unwrap();
            state.retention.prune();
            if budget.fixed_provider && routes.len() == 1 {
                if let Some(thread) = &budget.thread {
                    // A previous_response_id may still belong to the old provider
                    // after a handoff. Its queued turn protects that provider too.
                    state.retention.hold(thread.ticket, &routes[0].provider.id);
                }
            }
            state.epoch
        };
        let mut waiting: Option<Waiting> = None;
        let mut deadline = tokio::time::Instant::now() + budget.remaining;
        let mut rpm_waited = Duration::ZERO;
        let mut cooling;
        let mut rate_retry;
        let mut ledger_error;
        let result = 'admission: loop {
            if let Some(traffic) = &budget.traffic {
                let next = traffic.paused_for().saturating_sub(paused_before);
                deadline += next.saturating_sub(paused);
                paused = next;
            }
            let next_wake;
            let notified = self.0.wake.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if budget.traffic.as_ref().is_some_and(|t| t.paused()) {
                {
                    let mut s = self.0.state.lock().unwrap();
                    if !s.running || s.epoch != epoch {
                        break Err(Rejected::Stopped);
                    }
                    if waiting.is_none() {
                        if s.waiting.len() + s.capacity_waits.len() >= max_waiting {
                            break Err(Rejected::Full);
                        }
                        s.next += 1;
                        let ticket = s.next;
                        s.waiting.push_back((
                            ticket,
                            routes.iter().map(|r| r.provider.id.clone()).collect(),
                        ));
                        waiting = Some(Waiting {
                            scheduler: self.clone(),
                            ticket,
                        });
                        let _ = self.0.events.send(());
                    }
                }
                tokio::select! {
                    _ = notified => {},
                    _ = super::freeze::changed(&mut freeze_changes) => {},
                }
                continue;
            }
            {
                let mut s = self.0.state.lock().unwrap();
                if !s.running || s.epoch != epoch {
                    break Err(Rejected::Stopped);
                }
                s.retention.prune();
                let request = budget.thread.as_ref().map(|t| t.ticket);
                if request.is_some() && (s.retention.error || s.retention_write_error) {
                    break Err(Rejected::Unavailable);
                }
                let matching: Vec<_> = routes
                    .iter()
                    .filter(|r| {
                        s.limits.contains_key(&r.provider.id)
                            && s.versions.get(&r.provider.id) == Some(&r.provider.version)
                    })
                    .filter(|r| {
                        budget.fixed_provider || manual || s.queued.contains(&r.provider.id)
                    })
                    .collect();
                if matching.is_empty() {
                    break Err(Rejected::Unavailable);
                }
                let matching: Vec<_> = matching
                    .into_iter()
                    .filter(|r| {
                        requirement.allows(s.models.get(&r.provider.id).and_then(|m| m.as_deref()))
                    })
                    .collect();
                if matching.is_empty() {
                    break Err(Rejected::Model);
                }
                let mut eligible = matching;
                if let Some((intake, owner)) = &budget.intake {
                    eligible.retain(|r| {
                        owner.as_deref() == Some(&r.provider.id)
                            || s.intake.get(&r.provider.id).is_some_and(|(compact, new)| {
                                super::compaction::accepts_flags(*compact, *new, *intake)
                            })
                    });
                    if eligible.is_empty() {
                        break Err(Rejected::Policy);
                    }
                }
                if let Some(owner) = budget
                    .handoff_owner
                    .as_ref()
                    .filter(|_| !budget.fixed_provider)
                {
                    eligible.retain(|r| {
                        r.provider.id == *owner
                            || (s.order.get(&r.provider.id) < s.order.get(owner)
                                && s.intake
                                    .get(&r.provider.id)
                                    .is_some_and(|(compact, _)| *compact))
                    });
                    if eligible.is_empty() {
                        break Err(Rejected::Policy);
                    }
                }
                if (budget.protected_capacity || budget.handoff_owner.is_some())
                    && !budget.fixed_provider
                    && !manual
                {
                    eligible.sort_by_key(|r| {
                        let preferred =
                            budget.intake.as_ref().is_some_and(|(intake, _)| {
                                *intake == super::compaction::Intake::NewThread
                            }) && s.intake.get(&r.provider.id).is_some_and(|(_, new)| *new);
                        (
                            !preferred,
                            s.order.get(&r.provider.id).copied().unwrap_or(usize::MAX),
                        )
                    });
                }
                if let Some(owner) = &budget.capacity_owner {
                    eligible.retain(|r| {
                        r.provider.id == *owner
                            || s.order
                                .get(&r.provider.id)
                                .zip(s.order.get(owner))
                                .is_some_and(|(candidate, owner)| candidate > owner)
                    });
                    let Some(route) = eligible.iter().find(|r| r.provider.id == *owner) else {
                        break Err(Rejected::Unavailable);
                    };
                    s.rpm.prune();
                    let rpm_limit = s.rpm_limits.get(owner).copied().unwrap_or_default();
                    let rpm_blocked = (s.rpm_error && rpm_limit > 0)
                        || s.rpm
                            .gate(
                                owner,
                                rpm_limit,
                                s.rpm_pending.get(owner).copied().unwrap_or_default(),
                            )
                            .is_some();
                    if !route.health_for(requirement).available
                        || rpm_blocked
                        || Self::available(&s, owner, request, manual)
                    {
                        eligible.retain(|r| r.provider.id == *owner);
                    }
                }
                let mut any_ready = false;
                let now = Instant::now();
                next_wake = eligible
                    .iter()
                    .filter_map(|route| {
                        let health = route.health_for(requirement);
                        (health.retry_in > 0).then(|| now + Duration::from_secs(health.retry_in))
                    })
                    .chain(
                        eligible
                            .iter()
                            .filter_map(|r| s.rpm.next_change(&r.provider.id)),
                    )
                    .chain(s.retention.next_expiry().map(|delay| now + delay))
                    .min();
                let mut retry_in = u64::MAX;
                rate_retry = None;
                let ticket = waiting.as_ref().map(|w| w.ticket);
                if let Some(ticket) = ticket {
                    if let Some(entry) = s.waiting.iter_mut().find(|(id, _)| *id == ticket) {
                        entry.1 = eligible
                            .iter()
                            .filter(|r| r.health_for(requirement).available)
                            .map(|r| r.provider.id.clone())
                            .collect();
                    }
                }
                let mut accepted = None;
                let mut capacity_blocked = false;
                let mut intake_skip = None;
                let first_turn = budget
                    .intake
                    .as_ref()
                    .is_some_and(|(intake, _)| *intake == super::compaction::Intake::NewThread);
                let mut retry_selection = true;
                'selection: loop {
                    for (position, route) in eligible.iter().enumerate() {
                        let id = &route.provider.id;

                        let health = route.health_for(requirement);
                        if !health.available {
                            if first_turn && route.provider.take_new_threads {
                                intake_skip = Some(if route.provider_circuit.health().available {
                                    crate::usage::model::RoutingDecision::NewThreadModelCooling
                                } else {
                                    crate::usage::model::RoutingDecision::NewThreadProviderUnavailable
                                });
                            }
                            retry_in = retry_in.min(health.retry_in.max(1));
                            continue;
                        }
                        any_ready = true;
                        if !Self::available(&s, id, request, manual) {
                            if first_turn && route.provider.take_new_threads {
                                intake_skip = Some(
                                    crate::usage::model::RoutingDecision::NewThreadCapacityFull,
                                );
                            }
                            capacity_blocked |= budget
                                .handoff_owner
                                .as_ref()
                                .is_some_and(|owner| owner != id);
                            continue;
                        }
                        // A waiter pinned to another provider never blocks this provider.
                        if s.waiting
                            .iter()
                            .take_while(|(id, _)| Some(*id) != ticket)
                            .any(|(_, ids)| ids.contains(id))
                        {
                            // A FIFO follower also waits for this provider's RPM
                            // budget. It must not time out before the head waiter
                            // gets a chance to send when the minute rolls forward.
                            s.rpm.prune();
                            if !s.rpm_error {
                                let limit = s.rpm_limits.get(id).copied().unwrap_or_default();
                                let pending = s.rpm_pending.get(id).copied().unwrap_or_default();
                                if let Some((delay, _)) = s.rpm.gate(id, limit, pending) {
                                    let retry = delay.as_millis().div_ceil(1000).max(1) as u64;
                                    rate_retry = Some(rate_retry.unwrap_or(0).max(retry));
                                }
                            }
                            if first_turn && route.provider.take_new_threads {
                                intake_skip = Some(
                                    crate::usage::model::RoutingDecision::NewThreadQueueWaiting,
                                );
                            }
                            continue;
                        }
                        // Capacity cannot change under this lock, but a higher-priority
                        // circuit may recover between its health read and this reservation.
                        if retry_selection
                            && eligible[..position].iter().any(|earlier| {
                                let id = &earlier.provider.id;
                                Self::available(&s, id, request, manual)
                                    && earlier.health_for(requirement).available
                                    && !s
                                        .waiting
                                        .iter()
                                        .take_while(|(n, _)| Some(*n) != ticket)
                                        .any(|(_, ids)| ids.contains(id))
                            })
                        {
                            retry_selection = false;
                            continue 'selection;
                        }
                        let rpm_limit = s.rpm_limits.get(id).copied().unwrap_or_default();
                        s.rpm.prune();
                        if s.rpm_error && rpm_limit > 0 {
                            break 'selection;
                        }
                        if let Some((delay, _)) = s.rpm.gate(
                            id,
                            rpm_limit,
                            s.rpm_pending.get(id).copied().unwrap_or_default(),
                        ) {
                            rate_retry = Some(delay.as_millis().div_ceil(1000).max(1) as u64);
                            break 'selection;
                        }
                        if budget
                            .reconnect_from
                            .as_ref()
                            .is_some_and(|current| current != id)
                        {
                            // Re-enter connection selection before reserving a
                            // slot or RPM; the next admission rechecks all gates.
                            break 'admission Err(Rejected::Reconnect);
                        }
                        if let Some(permits) = Permits::acquire_for(route, manual, requirement) {
                            if budget.traffic.as_ref().is_some_and(|t| !t.dispatch()) {
                                drop(permits);
                                continue 'admission;
                            }
                            match self.reserve_rpm(
                                &mut s,
                                id,
                                rpm_limit,
                                !manual && !budget.fixed_provider,
                            ) {
                                Ok(rpm) => {
                                    let retain = budget
                                        .handoff_owner
                                        .as_ref()
                                        .is_none_or(|owner| owner == id);
                                    if let Some(request) = request {
                                        let retain = retain
                                            && budget.thread.as_ref().is_none_or(|t| t.retains(id));
                                        if !s.retention.admit(request, id, retain) {
                                            // RpmReservation locks on drop; release it outside the lock.
                                            drop(s);
                                            drop(rpm);
                                            drop(permits);
                                            return Err(Rejected::Full);
                                        }
                                    }
                                    *s.active.entry(id.clone()).or_default() += 1;
                                    accepted = Some(Admission {
                                        handoff_wait: (capacity_blocked && budget.handoff_owner.as_deref() == Some(id)).then_some(crate::usage::model::RoutingDecision::ReceiverCapacityFull).or(intake_skip),
                                        route: (**route).clone(),
                                        permits,
                                        rpm: Some(rpm),
                                        reset_generation: s.resets.get(id).copied().unwrap_or(0),
                                        slot: Slot {
                                            scheduler: self.clone(),
                                            id: id.clone(),
                                            thread: budget.thread.clone(),
                                            success: false,
                                        },
                                    });
                                    break;
                                }
                                Err(Rejected::RateLimited(retry)) => {
                                    if let Some(t) = &budget.traffic {
                                        t.waiting();
                                    }
                                    rate_retry = Some(retry);
                                    drop(permits);
                                    break 'selection;
                                }
                                Err(Rejected::RateLedger) => {
                                    if let Some(t) = &budget.traffic {
                                        t.waiting();
                                    }
                                    drop(permits);
                                    break 'selection;
                                }
                                Err(_) => {
                                    if let Some(t) = &budget.traffic {
                                        t.waiting();
                                    }
                                    drop(permits);
                                }
                            }
                        }

                        if retry_selection {
                            retry_selection = false;
                            any_ready = false;
                            retry_in = u64::MAX;
                            continue 'selection;
                        }
                    }
                    break;
                }
                cooling = (!any_ready).then_some(if retry_in == u64::MAX { 1 } else { retry_in });
                if let Some(admission) = accepted {
                    break Ok(admission);
                }
                if s.rpm_error
                    && eligible
                        .iter()
                        .any(|r| s.rpm_limits.get(&r.provider.id).copied().unwrap_or(0) > 0)
                {
                    if immediate || budget.protected_capacity {
                        break Err(Rejected::RateLedger);
                    }
                    if waiting.is_none() {
                        if s.waiting.len() + s.capacity_waits.len() >= max_waiting {
                            break Err(Rejected::Full);
                        }
                        s.next += 1;
                        let ticket = s.next;
                        s.waiting.push_back((
                            ticket,
                            eligible
                                .iter()
                                .filter(|r| r.health_for(requirement).available)
                                .map(|r| r.provider.id.clone())
                                .collect(),
                        ));
                        waiting = Some(Waiting {
                            scheduler: self.clone(),
                            ticket,
                        });
                        let _ = self.0.events.send(());
                    }
                } else if let Some(retry) = rate_retry {
                    if immediate {
                        break Err(Rejected::RateLimited(retry));
                    }
                    if waiting.is_none() {
                        if s.waiting.len() + s.capacity_waits.len() >= max_waiting {
                            break Err(Rejected::Full);
                        }
                        s.next += 1;
                        let ticket = s.next;
                        s.waiting.push_back((
                            ticket,
                            eligible
                                .iter()
                                .filter(|r| r.health_for(requirement).available)
                                .map(|r| r.provider.id.clone())
                                .collect(),
                        ));
                        waiting = Some(Waiting {
                            scheduler: self.clone(),
                            ticket,
                        });
                        let _ = self.0.events.send(());
                    }
                }
                ledger_error = s.rpm_error
                    && eligible
                        .iter()
                        .any(|r| s.rpm_limits.get(&r.provider.id).copied().unwrap_or(0) > 0);
                if immediate {
                    break Err(cooling.map_or(Rejected::Timeout, Rejected::Cooling));
                }
                if waiting.is_none() {
                    if s.waiting.len() + s.capacity_waits.len() >= max_waiting {
                        break Err(Rejected::Full);
                    }
                    s.next += 1;
                    let ticket = s.next;
                    s.waiting.push_back((
                        ticket,
                        eligible
                            .iter()
                            .filter(|r| r.health_for(requirement).available)
                            .map(|r| r.provider.id.clone())
                            .collect(),
                    ));
                    waiting = Some(Waiting {
                        scheduler: self.clone(),
                        ticket,
                    });
                    let _ = self.0.events.send(());
                }
            }
            let wait_started = Instant::now();
            let pause_started = budget
                .traffic
                .as_ref()
                .map_or(Duration::ZERO, |t| t.paused_for());
            let waiting_for_rpm = rate_retry.is_some() && !ledger_error;
            tokio::select! {
                biased;
                _ = super::freeze::changed(&mut freeze_changes) => {},
                _ = notified => {},
                _ = tokio::time::sleep_until(deadline), if !budget.protected_capacity && !waiting_for_rpm => break Err(if let Some(retry) = rate_retry { Rejected::RateLimited(retry) } else if ledger_error { Rejected::RateLedger } else { cooling.map_or(Rejected::Timeout, Rejected::Cooling) }),
                _ = async {
                    if let Some(at) = next_wake { tokio::time::sleep_until(at).await; }
                    else { std::future::pending::<()>().await; }
                } => {},
            }
            if waiting_for_rpm {
                let paused = budget
                    .traffic
                    .as_ref()
                    .map_or(Duration::ZERO, |t| t.paused_for())
                    .saturating_sub(pause_started);
                let elapsed = wait_started.elapsed().saturating_sub(paused);
                deadline += elapsed;
                rpm_waited += elapsed;
            }
        };
        if waiting.is_some() {
            budget.remaining = budget.remaining.saturating_sub(
                started
                    .elapsed()
                    .saturating_sub(rpm_waited)
                    .saturating_sub(paused),
            );
        }
        drop(waiting);
        self.signal();
        result
    }
}
impl Drop for Slot {
    fn drop(&mut self) {
        let mut s = self.scheduler.0.state.lock().unwrap();
        if let Some(thread) = &self.thread {
            s.retention.release(
                thread.ticket,
                &self.id,
                self.success && thread.retains(&self.id),
            );
        }
        if let Some(n) = s.active.get_mut(&self.id) {
            *n = n.saturating_sub(1);
        }
        drop(s);
        self.scheduler.signal();
    }
}
impl Drop for Waiting {
    fn drop(&mut self) {
        self.scheduler
            .0
            .state
            .lock()
            .unwrap()
            .waiting
            .retain(|(id, _)| *id != self.ticket);
        self.scheduler.signal();
    }
}
impl Drop for CapacityWait {
    fn drop(&mut self) {
        self.scheduler
            .0
            .state
            .lock()
            .unwrap()
            .capacity_waits
            .remove(&self.ticket);
        self.scheduler.signal();
    }
}
