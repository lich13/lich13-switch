//! Request slots and bounded FIFO admission. Behavior informed by Sub2API
//! a3eb7ef3 concurrency_service / account scheduler; independently implemented in Rust.
use super::{circuit::Circuit, forward::Permits, model::Provider, routing::Requirement, Route};
use crate::storage;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, VecDeque},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{broadcast, Notify};

#[derive(Clone)]
pub struct Scheduler(Arc<Shared>);
struct Shared {
    state: Mutex<State>,
    wake: Notify,
    events: broadcast::Sender<()>,
}
struct State {
    running: bool,
    epoch: u64,
    next: u64,
    limits: HashMap<String, u32>,
    rpm_limits: HashMap<String, u32>,
    models: HashMap<String, Option<Vec<String>>>,
    active: HashMap<String, usize>,
    rpm: HashMap<String, VecDeque<RateStamp>>,
    rpm_pending: HashMap<String, usize>,
    rpm_path: PathBuf,
    rpm_error: bool,
    waiting: VecDeque<(u64, Vec<String>)>,
    capacity_waits: HashMap<u64, (String, Instant, bool)>,
    resets: HashMap<String, u64>,
}
impl State {
    fn new(rpm_path: PathBuf, rpm: HashMap<String, VecDeque<RateStamp>>, rpm_error: bool) -> Self {
        Self {
            running: false,
            epoch: 0,
            next: 0,
            limits: HashMap::new(),
            rpm_limits: HashMap::new(),
            models: HashMap::new(),
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
#[derive(Clone, Copy)]
struct RateStamp {
    at: Instant,
    wall_ms: i64,
}
#[derive(Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct PersistedRpm {
    providers: HashMap<String, Vec<i64>>,
}
const RPM_WINDOW: Duration = Duration::from_secs(60);
fn now_wall_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}
fn load_rpm(path: &Path) -> (HashMap<String, VecDeque<RateStamp>>, bool) {
    let raw = match storage::read_optional(path) {
        Ok(Some(raw)) => raw,
        Ok(None) => return (HashMap::new(), false),
        Err(_) => return (HashMap::new(), true),
    };
    let Ok(saved) = serde_json::from_slice::<PersistedRpm>(&raw) else {
        return (HashMap::new(), true);
    };
    let now_ms = now_wall_ms();
    let now = Instant::now();
    let providers = saved
        .providers
        .into_iter()
        .map(|(id, timestamps)| {
            let values = timestamps
                .into_iter()
                .filter_map(|wall_ms| {
                    let age = now_ms.saturating_sub(wall_ms).max(0) as u64;
                    if age >= RPM_WINDOW.as_millis() as u64 {
                        None
                    } else {
                        Some(RateStamp {
                            at: now.checked_sub(Duration::from_millis(age)).unwrap_or(now),
                            wall_ms,
                        })
                    }
                })
                .collect::<VecDeque<_>>();
            (id, values)
        })
        .filter(|(_, values)| !values.is_empty())
        .collect();
    (providers, false)
}
#[derive(Clone)]
pub struct CapacitySource {
    pub provider_id: String,
    pub reset_generation: u64,
}
#[derive(serde::Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct CapacityRetry {
    pub provider_id: String,
    pub retry_in: u64,
}
pub struct Slot {
    scheduler: Scheduler,
    id: String,
}
struct RpmReservation {
    scheduler: Scheduler,
    id: String,
    committed: bool,
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
    pub route: Route,
    pub permits: Permits,
    pub slot: Slot,
    rpm: Option<RpmReservation>,
    pub reset_generation: u64,
}
#[derive(Debug, PartialEq)]
pub enum Rejected {
    Stopped,
    Unavailable,
    Full,
    Timeout,
    Cooling(u64),
    Model,
    RateLimited(u64),
    RateLedger,
}
pub struct Budget {
    remaining: Duration,
}
impl Admission {
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
        if s.rpm_error {
            Self::release_pending(&mut s, &self.id);
            self.committed = true;
            drop(s);
            self.scheduler.signal();
            return Err(Rejected::RateLedger);
        }
        Scheduler::prune_rpm(&mut s, &self.id);
        for values in s.rpm.values_mut() {
            values.retain(|stamp| stamp.at.elapsed() < RPM_WINDOW);
        }
        let now = Instant::now();
        let wall_ms = now_wall_ms();
        let mut next = s.rpm.clone();
        next.entry(self.id.clone())
            .or_default()
            .push_back(RateStamp { at: now, wall_ms });
        if persist_rpm(&s.rpm_path, &next).is_err() {
            s.rpm_error = true;
            Self::release_pending(&mut s, &self.id);
            self.committed = true;
            drop(s);
            self.scheduler.signal();
            return Err(Rejected::RateLedger);
        }
        s.rpm = next;
        Self::release_pending(&mut s, &self.id);
        self.committed = true;
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
fn persist_rpm(path: &Path, rpm: &HashMap<String, VecDeque<RateStamp>>) -> storage::Result<()> {
    let providers = rpm
        .iter()
        .filter_map(|(id, values)| {
            let timestamps: Vec<_> = values.iter().map(|stamp| stamp.wall_ms).collect();
            (!timestamps.is_empty()).then_some((id.clone(), timestamps))
        })
        .collect();
    let bytes = serde_json::to_vec(&PersistedRpm { providers })
        .map_err(|_| crate::storage::AppError::new("RPM_LEDGER", "无法保存 RPM 状态"))?;
    storage::atomic_write(path, &bytes, None)
}
impl Budget {
    pub fn new(seconds: u64) -> Self {
        Self {
            remaining: Duration::from_secs(seconds),
        }
    }
}
impl Scheduler {
    pub fn new(events: broadcast::Sender<()>, rpm_path: PathBuf) -> Self {
        let (rpm, rpm_error) = load_rpm(&rpm_path);
        Self(Arc::new(Shared {
            state: Mutex::new(State::new(rpm_path, rpm, rpm_error)),
            wake: Notify::new(),
            events,
        }))
    }
    pub fn configure(&self, providers: &[Provider], running: bool) {
        let mut s = self.0.state.lock().unwrap();
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
        let ids: std::collections::HashSet<_> = providers.iter().map(|p| p.id.as_str()).collect();
        let before = s.rpm.len();
        s.rpm.retain(|id, _| ids.contains(id.as_str()));
        s.rpm_pending.retain(|id, _| ids.contains(id.as_str()));
        if before != s.rpm.len() && !s.rpm_error {
            let snapshot = s.rpm.clone();
            if persist_rpm(&s.rpm_path, &snapshot).is_err() {
                s.rpm_error = true;
            }
        }
        drop(s);
        self.signal();
    }
    pub fn rpm_status(&self, id: &str, limit: u32) -> (usize, u64, bool, bool) {
        let mut s = self.0.state.lock().unwrap();
        let before = s.rpm.get(id).map_or(0, VecDeque::len);
        Self::prune_rpm(&mut s, id);
        let expired = before != s.rpm.get(id).map_or(0, VecDeque::len);
        let used = s.rpm.get(id).map_or(0, VecDeque::len)
            + s.rpm_pending.get(id).copied().unwrap_or_default();
        let retry_in = Self::rpm_retry_in(&s, id).unwrap_or({
            if limit != 0 && used >= limit as usize {
                1
            } else {
                0
            }
        });
        let result = (
            used,
            retry_in,
            limit != 0 && used >= limit as usize,
            s.rpm_error && limit != 0,
        );
        drop(s);
        if expired {
            self.signal();
        }
        result
    }
    fn prune_rpm(state: &mut State, id: &str) {
        let Some(values) = state.rpm.get_mut(id) else {
            return;
        };
        values.retain(|stamp| stamp.at.elapsed() < RPM_WINDOW);
        if values.is_empty() {
            state.rpm.remove(id);
        }
    }
    fn rpm_retry_in(state: &State, id: &str) -> Option<u64> {
        state
            .rpm
            .get(id)
            .and_then(|values| values.front())
            .map(|stamp| {
                stamp
                    .at
                    .checked_add(RPM_WINDOW)
                    .unwrap_or_else(Instant::now)
                    .saturating_duration_since(Instant::now())
                    .as_millis()
                    .div_ceil(1000) as u64
            })
    }
    fn reserve_rpm(
        &self,
        state: &mut State,
        id: &str,
        limit: u32,
    ) -> Result<RpmReservation, Rejected> {
        if limit == 0 {
            return Ok(RpmReservation {
                scheduler: self.clone(),
                id: id.to_owned(),
                committed: true,
            });
        }
        Self::prune_rpm(state, id);
        let used = state.rpm.get(id).map_or(0, VecDeque::len)
            + state.rpm_pending.get(id).copied().unwrap_or_default();
        if state.rpm_error {
            return Err(Rejected::RateLedger);
        }
        if used >= limit as usize {
            return Err(Rejected::RateLimited(
                Self::rpm_retry_in(state, id).unwrap_or(1).max(1),
            ));
        }
        *state.rpm_pending.entry(id.to_owned()).or_default() += 1;
        Ok(RpmReservation {
            scheduler: self.clone(),
            id: id.to_owned(),
            committed: false,
        })
    }
    pub fn counts(&self) -> (HashMap<String, usize>, usize) {
        let s = self.0.state.lock().unwrap();
        (s.active.clone(), s.waiting.len() + s.capacity_waits.len())
    }
    pub fn capacity_retries(&self) -> Vec<CapacityRetry> {
        self.retries(false)
    }
    pub fn websocket_retries(&self) -> Vec<CapacityRetry> {
        self.retries(true)
    }
    fn retries(&self, websocket: bool) -> Vec<CapacityRetry> {
        let s = self.0.state.lock().unwrap();
        let mut result: Vec<_> = s
            .capacity_waits
            .values()
            .filter(|(_, _, ws)| *ws == websocket)
            .map(|(id, until, _)| CapacityRetry {
                provider_id: id.clone(),
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
        self.wait_kind(sources, delay, max_waiting, false).await
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
            true,
        )
        .await
    }
    async fn wait_kind(
        &self,
        sources: &[CapacitySource],
        delay: Duration,
        max_waiting: usize,
        websocket: bool,
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
                    websocket,
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
                _ = tokio::time::sleep_until(until.into()) => return Ok(()),
            }
        }
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
        let epoch = self.0.state.lock().unwrap().epoch;
        let mut waiting: Option<Waiting> = None;
        let deadline = tokio::time::Instant::now() + budget.remaining;
        let mut cooling;
        let mut rate_retry;
        let mut ledger_error;
        let result = loop {
            let notified = self.0.wake.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let mut s = self.0.state.lock().unwrap();
                if !s.running || s.epoch != epoch {
                    break Err(Rejected::Stopped);
                }
                let matching: Vec<_> = routes
                    .iter()
                    .filter(|r| s.limits.contains_key(&r.provider.id))
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
                let eligible = matching;
                let mut any_ready = false;
                let mut retry_in = u64::MAX;
                rate_retry = None;
                let ticket = waiting.as_ref().map(|w| w.ticket);
                if let Some(ticket) = ticket {
                    if let Some(entry) = s.waiting.iter_mut().find(|(id, _)| *id == ticket) {
                        entry.1 = eligible.iter().map(|r| r.provider.id.clone()).collect();
                    }
                }
                let mut accepted = None;
                let mut retry_selection = true;
                'selection: loop {
                    for (position, route) in eligible.iter().enumerate() {
                        let id = &route.provider.id;
                        let limit = s.limits[id];
                        let active = s.active.get(id).copied().unwrap_or(0);
                        let health = route.provider_circuit.health();
                        if !health.available {
                            retry_in = retry_in.min(health.retry_in.max(1));
                            continue;
                        }
                        any_ready = true;
                        if limit != 0 && active >= limit as usize {
                            continue;
                        }
                        // A waiter pinned to another provider never blocks this provider.
                        if s.waiting
                            .iter()
                            .take_while(|(id, _)| Some(*id) != ticket)
                            .any(|(_, ids)| ids.contains(id))
                        {
                            continue;
                        }
                        // Capacity cannot change under this lock, but a higher-priority
                        // circuit may recover between its health read and this reservation.
                        if retry_selection
                            && eligible[..position].iter().any(|earlier| {
                                let id = &earlier.provider.id;
                                let limit = s.limits[id];
                                (limit == 0
                                    || s.active.get(id).copied().unwrap_or(0) < limit as usize)
                                    && earlier.provider_circuit.health().available
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
                        if rpm_limit != 0 {
                            Self::prune_rpm(&mut s, id);
                            let used = s.rpm.get(id).map_or(0, VecDeque::len)
                                + s.rpm_pending.get(id).copied().unwrap_or_default();
                            if s.rpm_error {
                                break 'selection;
                            }
                            if used >= rpm_limit as usize {
                                rate_retry = Some(Self::rpm_retry_in(&s, id).unwrap_or(1).max(1));
                                break 'selection;
                            }
                        }
                        if let Some(permits) = Permits::acquire(route, manual) {
                            match self.reserve_rpm(&mut s, id, rpm_limit) {
                                Ok(rpm) => {
                                    *s.active.entry(id.clone()).or_default() += 1;
                                    accepted = Some(Admission {
                                        route: (**route).clone(),
                                        permits,
                                        rpm: Some(rpm),
                                        reset_generation: s.resets.get(id).copied().unwrap_or(0),
                                        slot: Slot {
                                            scheduler: self.clone(),
                                            id: id.clone(),
                                        },
                                    });
                                    break;
                                }
                                Err(Rejected::RateLimited(retry)) => {
                                    rate_retry = Some(retry);
                                    drop(permits);
                                    break 'selection;
                                }
                                Err(Rejected::RateLedger) => {
                                    drop(permits);
                                    break 'selection;
                                }
                                Err(_) => drop(permits),
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
                if s.rpm_error {
                    if immediate {
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
                            eligible.iter().map(|r| r.provider.id.clone()).collect(),
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
                            eligible.iter().map(|r| r.provider.id.clone()).collect(),
                        ));
                        waiting = Some(Waiting {
                            scheduler: self.clone(),
                            ticket,
                        });
                        let _ = self.0.events.send(());
                    }
                }
                ledger_error = s.rpm_error;
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
                        eligible.iter().map(|r| r.provider.id.clone()).collect(),
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
                _ = tokio::time::sleep_until(deadline) => break Err(if let Some(retry) = rate_retry { Rejected::RateLimited(retry) } else if ledger_error { Rejected::RateLedger } else { cooling.map_or(Rejected::Timeout, Rejected::Cooling) }),
                // Circuit cooldowns may expire without a separate request or UI event.
                _ = tokio::time::sleep(Duration::from_millis(200)) => {},
            }
        };
        if waiting.is_some() {
            budget.remaining = budget.remaining.saturating_sub(started.elapsed());
        }
        drop(waiting);
        self.signal();
        result
    }
}
impl Drop for Slot {
    fn drop(&mut self) {
        let mut s = self.scheduler.0.state.lock().unwrap();
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
