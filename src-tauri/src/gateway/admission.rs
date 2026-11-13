//! Request slots and bounded FIFO admission. Behavior informed by Sub2API
//! a3eb7ef3 concurrency_service / account scheduler; independently implemented in Rust.
use super::rpm::Ledger;
use super::{
    circuit::Circuit, health_settlement::Permits, model::Provider, routing::Requirement, Route,
};
use parking_lot::Mutex;
use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
    time::Duration,
};
use tokio::{
    sync::{broadcast, Notify},
    time::Instant,
};

#[derive(Clone)]
pub struct Scheduler(Arc<Shared>);
struct Shared {
    state: Mutex<State>,
    wake: Notify,
    events: broadcast::Sender<()>,
}
#[derive(Default)]
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
    rpm: Ledger,
    waiting: VecDeque<(u64, Vec<String>)>,
    capacity_waits: HashMap<u64, (String, Instant, WaitKind)>,
    resets: HashMap<String, u64>,
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
}

struct RpmReservation {
    scheduler: Scheduler,
    id: String,
    token: u64,
    sent: bool,
}

struct Waiting {
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
}
pub struct Budget {
    pub traffic: Option<super::freeze::Ticket>,
    pub handoff_owner: Option<String>,
    pub capacity_owner: Option<String>,
    pub reconnect_from: Option<String>,
    remaining: Duration,
    protected_capacity: bool,
    fixed_provider: bool,
    intake: Option<(super::compaction::Intake, Option<String>)>,
}
impl Admission {
    /// Marks the request as sent; an unsent admission returns its RPM budget on drop.
    pub fn commit_rpm(&mut self) {
        if let Some(rpm) = self.rpm.as_mut() {
            rpm.sent = true;
        }
    }
}
impl Drop for RpmReservation {
    fn drop(&mut self) {
        if self.sent {
            return;
        }
        self.scheduler
            .0
            .state
            .lock()
            .rpm
            .refund(&self.id, self.token);
        self.scheduler.signal();
    }
}
impl Budget {
    /// A thread that follows its owner may only move to a handoff receiver.
    pub fn follow(&mut self, lease: Option<&super::compaction::Lease>) {
        if let Some(lease) = lease.filter(|lease| lease.follows_owner()) {
            self.handoff_owner = lease.handoff.then(|| lease.owner.clone()).flatten();
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
    pub fn new(events: broadcast::Sender<()>) -> Self {
        Self(Arc::new(Shared {
            state: Mutex::new(State::default()),
            wake: Notify::new(),
            events,
        }))
    }
    fn available(s: &State, id: &str) -> bool {
        let limit = s.limits[id];
        limit == 0 || s.active.get(id).copied().unwrap_or(0) < limit as usize
    }

    pub fn configure(&self, providers: &[Provider], running: bool) {
        let mut s = self.0.state.lock();
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
        drop(s);
        self.signal();
    }
    /// Sends counted in the current window and seconds until the next one fits.
    pub fn rpm_status(&self, id: &str, limit: u32) -> (usize, u64, bool) {
        let mut s = self.0.state.lock();
        let used = if limit == 0 { 0 } else { s.rpm.used(id) };
        let gate = s.rpm.gate(id, limit);
        (used, gate.map_or(0, seconds), gate.is_some())
    }
    pub fn counts(&self) -> (HashMap<String, usize>, usize) {
        let s = self.0.state.lock();
        (s.active.clone(), s.waiting.len() + s.capacity_waits.len())
    }
    pub(super) fn capacity_available(&self, id: &str) -> bool {
        Self::available(&self.0.state.lock(), id)
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
        let s = self.0.state.lock();
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
            reset_generation: self.0.state.lock().resets.get(id).copied().unwrap_or(0),
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
        let generation = self.0.state.lock().resets.get(id).copied().unwrap_or(0);
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
            let mut s = self.0.state.lock();
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
                let s = self.0.state.lock();
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
        let s = self.0.state.lock();
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
        let mut state = self.0.state.lock();
        for circuit in circuits {
            circuit.reset();
        }
        *state.resets.entry(id.to_owned()).or_default() += 1;
        drop(state);
        self.signal();
    }
    pub fn signal(&self) {
        self.0.wake.notify_waiters();
        let _ = self.0.events.send(());
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
        let epoch = self.0.state.lock().epoch;
        let mut waiting: Option<Waiting> = None;
        let mut deadline = tokio::time::Instant::now() + budget.remaining;
        let mut cooling;
        let mut rate_retry;
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
                    let mut s = self.0.state.lock();
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
                let mut s = self.0.state.lock();
                if !s.running || s.epoch != epoch {
                    break Err(Rejected::Stopped);
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
                    let rpm_limit = s.rpm_limits.get(owner).copied().unwrap_or_default();
                    let rpm_blocked = s.rpm.gate(owner, rpm_limit).is_some();
                    if !route.health_for(requirement).available
                        || rpm_blocked
                        || Self::available(&s, owner)
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
                for route in &eligible {
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
                    if !Self::available(&s, id) {
                        if first_turn && route.provider.take_new_threads {
                            intake_skip =
                                Some(crate::usage::model::RoutingDecision::NewThreadCapacityFull);
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
                        let limit = s.rpm_limits.get(id).copied().unwrap_or_default();
                        if let Some(delay) = s.rpm.gate(id, limit) {
                            rate_retry = Some(rate_retry.unwrap_or(0).max(seconds(delay)));
                        }
                        if first_turn && route.provider.take_new_threads {
                            intake_skip =
                                Some(crate::usage::model::RoutingDecision::NewThreadQueueWaiting);
                        }
                        continue;
                    }
                    let rpm_limit = s.rpm_limits.get(id).copied().unwrap_or_default();
                    if let Some(delay) = s.rpm.gate(id, rpm_limit) {
                        rate_retry = Some(seconds(delay));
                        break;
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
                    if let Some(permits) = Permits::acquire_for(route, requirement) {
                        if budget.traffic.as_ref().is_some_and(|t| !t.dispatch()) {
                            drop(permits);
                            continue 'admission;
                        }
                        let rpm = (rpm_limit > 0).then(|| RpmReservation {
                            scheduler: self.clone(),
                            id: id.clone(),
                            token: s.rpm.charge(id),
                            sent: false,
                        });
                        *s.active.entry(id.clone()).or_default() += 1;
                        accepted = Some(Admission {
                            handoff_wait: (capacity_blocked
                                && budget.handoff_owner.as_deref() == Some(id))
                            .then_some(crate::usage::model::RoutingDecision::ReceiverCapacityFull)
                            .or(intake_skip),
                            route: (**route).clone(),
                            permits,
                            rpm,
                            reset_generation: s.resets.get(id).copied().unwrap_or(0),
                            slot: Slot {
                                scheduler: self.clone(),
                                id: id.clone(),
                            },
                        });
                        break;
                    }
                }
                cooling = (!any_ready).then_some(if retry_in == u64::MAX { 1 } else { retry_in });
                if let Some(admission) = accepted {
                    break Ok(admission);
                }
                if let Some(retry) = rate_retry {
                    if immediate {
                        break Err(Rejected::RateLimited(retry));
                    }
                }
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
            tokio::select! {
                biased;
                _ = super::freeze::changed(&mut freeze_changes) => {},
                _ = notified => {},
                // RPM waits share the request's waiting budget with every other wait.
                _ = tokio::time::sleep_until(deadline), if !budget.protected_capacity => break Err(match (rate_retry, cooling) {
                    (Some(retry), _) => Rejected::RateLimited(retry),
                    (None, Some(retry)) => Rejected::Cooling(retry),
                    (None, None) => Rejected::Timeout,
                }),
                _ = async {
                    if let Some(at) = next_wake { tokio::time::sleep_until(at).await; }
                    else { std::future::pending::<()>().await; }
                } => {},
            }
        };
        if waiting.is_some() {
            budget.remaining = budget
                .remaining
                .saturating_sub(started.elapsed().saturating_sub(paused));
        }
        drop(waiting);
        self.signal();
        result
    }
}
impl Drop for Slot {
    fn drop(&mut self) {
        let mut s = self.scheduler.0.state.lock();
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
            .capacity_waits
            .remove(&self.ticket);
        self.scheduler.signal();
    }
}

/// Whole seconds a caller should wait, never less than one.
fn seconds(delay: Duration) -> u64 {
    delay.as_millis().div_ceil(1000).max(1) as u64
}

#[cfg(test)]
mod test_support {
    use super::*;
    impl Scheduler {
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
    }
}
