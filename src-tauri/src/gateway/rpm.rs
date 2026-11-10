//! Rolling minute budgets for starts and independently reserved completions.
//! All transitions are made under the admission lock using a monotonic clock.
use crate::events::RpmWaitReason;
use std::{
    collections::{HashMap, VecDeque},
    time::Duration,
};
use tokio::time::Instant;

#[cfg(test)]
#[path = "v02510_rpm_tests.rs"]
mod v02510_rpm_tests;
#[cfg(test)]
#[path = "v02510_rpm_window_tests.rs"]
mod v02510_rpm_window_tests;

// Keep the existing one-second margin for upstream second-granularity windows.
pub(super) const WINDOW: Duration = Duration::from_secs(61);
#[derive(Clone, Copy)]
pub(super) struct Stamp {
    at: Instant,
}
impl Stamp {
    pub(super) fn now() -> Self {
        Self { at: Instant::now() }
    }
    fn after(delay: Duration) -> Self {
        Self {
            at: Instant::now() + delay,
        }
    }
}
#[derive(Clone, Default)]
pub(super) struct Provider {
    starts: VecDeque<Stamp>,
    completed: VecDeque<Stamp>,
    // None means still observing. Some means an uncertain end, protected to a deadline.
    inflight: HashMap<String, Option<Stamp>>,
    backoff: Option<Stamp>,
    initialize: Option<Stamp>,
}
#[derive(Clone, Default)]
pub(super) struct Ledger {
    providers: HashMap<String, Provider>,
}
#[derive(Clone, Copy)]
pub(super) enum Settlement {
    Success,
    Failed,
    Uncertain,
}
impl Ledger {
    /// Pending sends and observing responses have no time-based release.
    /// Their reservation/settlement signals wake the scheduler instead.
    pub(super) fn next_change(&self, id: &str) -> Option<Instant> {
        let now = Instant::now();
        self.providers
            .get(id)
            .into_iter()
            .flat_map(|p| {
                p.starts
                    .iter()
                    .chain(p.completed.iter())
                    .map(|s| s.at + WINDOW)
                    .chain(p.inflight.values().filter_map(|s| s.map(|s| s.at)))
                    .chain(p.backoff.map(|s| s.at))
                    .chain(p.initialize.map(|s| s.at))
            })
            .filter(|at| *at > now)
            .min()
    }
    pub(super) fn prune(&mut self) {
        let now = Instant::now();
        self.providers.retain(|_, p| {
            p.starts.retain(|s| now.duration_since(s.at) < WINDOW);
            p.completed.retain(|s| now.duration_since(s.at) < WINDOW);
            p.inflight
                .retain(|_, until| until.is_none_or(|s| s.at > now));
            if p.backoff.is_some_and(|s| s.at <= now) {
                p.backoff = None;
            }
            if p.initialize.is_some_and(|s| s.at <= now) {
                p.initialize = None;
            }
            !p.starts.is_empty()
                || !p.completed.is_empty()
                || !p.inflight.is_empty()
                || p.backoff.is_some()
                || p.initialize.is_some()
        });
    }
    pub(super) fn used(&self, id: &str) -> usize {
        self.providers.get(id).map_or(0, |p| p.starts.len())
    }
    pub(super) fn gate(
        &self,
        id: &str,
        limit: u32,
        pending: usize,
    ) -> Option<(Duration, RpmWaitReason)> {
        let now = Instant::now();
        let mut blocked: Option<(Instant, RpmWaitReason)> = None;
        let mut add = |until: Instant, reason| {
            if until > now && blocked.is_none_or(|(at, _)| at < until) {
                blocked = Some((until, reason));
            }
        };
        if let Some(p) = self.providers.get(id) {
            if let Some(backoff) = p.backoff {
                add(backoff.at, RpmWaitReason::UpstreamBackoff);
            }
        }
        if limit > 0 {
            let limit = limit as usize;
            let provider = self.providers.get(id);
            let starts = provider.map_or(0, |p| p.starts.len());
            if starts.saturating_add(pending) >= limit {
                // Pending reservations occupy budget without imposing spacing.
                // They may be released before any send; scheduler notifications
                // recheck promptly, including when no ledger entry exists yet.
                let needed = starts.saturating_add(pending).saturating_sub(limit);
                let until = provider
                    .and_then(|p| p.starts.get(needed))
                    .map_or(now + Duration::from_millis(200), |s| s.at + WINDOW);
                add(until, RpmWaitReason::SendWindow);
            }
            if let Some(p) = provider {
                if let Some(init) = p.initialize {
                    add(init.at, RpmWaitReason::CompletionWindow);
                }
                if p.completed.len() + p.inflight.len() + pending >= limit {
                    let mut releases: Vec<_> = p
                        .completed
                        .iter()
                        .map(|s| s.at + WINDOW)
                        .chain(
                            p.inflight
                                .values()
                                .map(|s| s.map_or(now + Duration::from_millis(200), |s| s.at)),
                        )
                        .collect();
                    releases.sort();
                    let needed = releases.len().saturating_add(pending).saturating_sub(limit);
                    add(
                        releases
                            .get(needed)
                            .copied()
                            .unwrap_or(now + Duration::from_millis(200)),
                        RpmWaitReason::CompletionWindow,
                    );
                }
            }
        }
        blocked.map(|(at, reason)| (at.saturating_duration_since(now), reason))
    }
    pub(super) fn charge(&mut self, id: &str) -> String {
        let at = Stamp::now();
        let token = uuid::Uuid::new_v4().to_string();
        let p = self.providers.entry(id.into()).or_default();
        p.starts.push_back(at);
        p.inflight.insert(token.clone(), None);
        token
    }
    #[cfg(test)]
    pub(super) fn commit(&mut self, id: &str, _limit: u32) -> Option<String> {
        Some(self.charge(id))
    }
    pub(super) fn settle(&mut self, id: &str, token: &str, result: Settlement) {
        self.settle_at(id, token, result, Stamp::now());
    }
    pub(super) fn settle_at(&mut self, id: &str, token: &str, result: Settlement, at: Stamp) {
        let Some(p) = self.providers.get_mut(id) else {
            return;
        };
        if p.inflight.remove(token).is_none() {
            return;
        }
        match result {
            Settlement::Success => p.completed.push_back(at),
            Settlement::Failed => (),
            Settlement::Uncertain => {
                p.inflight
                    .insert(token.into(), Some(Stamp { at: at.at + WINDOW }));
            }
        }
    }
    pub(super) fn enable(&mut self, id: &str) {
        if let Some(p) = self.providers.get_mut(id) {
            if !p.starts.is_empty() || !p.inflight.is_empty() {
                p.initialize = Some(Stamp::after(WINDOW));
            }
        }
    }
    pub(super) fn backoff(&mut self, id: &str, delay: Duration) {
        let until = Stamp::after(delay.min(Duration::from_secs(86400)));
        let p = self.providers.entry(id.into()).or_default();
        if p.backoff.is_none_or(|s| s.at < until.at) {
            p.backoff = Some(until);
        }
    }
}
