//! Rolling one-minute send budget per provider.
//! All transitions are made under the admission lock using a monotonic clock.
use std::{
    collections::{HashMap, VecDeque},
    time::Duration,
};
use tokio::time::Instant;

#[cfg(test)]
#[path = "v02510_rpm_tests.rs"]
mod v02510_rpm_tests;

// Keep a one-second margin for upstream second-granularity windows.
pub(super) const WINDOW: Duration = Duration::from_secs(61);

#[derive(Default)]
pub(super) struct Ledger {
    next: u64,
    starts: HashMap<String, VecDeque<(u64, Instant)>>,
}
impl Ledger {
    fn prune(&mut self) {
        let now = Instant::now();
        self.starts.retain(|_, starts| {
            while starts
                .front()
                .is_some_and(|(_, at)| now.duration_since(*at) >= WINDOW)
            {
                starts.pop_front();
            }
            !starts.is_empty()
        });
    }
    /// When the oldest counted send leaves the window.
    pub(super) fn next_change(&self, id: &str) -> Option<Instant> {
        self.starts
            .get(id)
            .and_then(|starts| starts.front())
            .map(|(_, at)| *at + WINDOW)
    }
    pub(super) fn used(&mut self, id: &str) -> usize {
        self.prune();
        self.starts.get(id).map_or(0, VecDeque::len)
    }
    /// How long until one more send fits under `limit`; `None` when it fits now.
    pub(super) fn gate(&mut self, id: &str, limit: u32) -> Option<Duration> {
        if limit == 0 {
            return None;
        }
        self.prune();
        let starts = self.starts.get(id)?;
        let blocking = starts.len().checked_sub(limit as usize)?;
        let (_, at) = starts[blocking];
        Some((at + WINDOW).saturating_duration_since(Instant::now()))
    }
    pub(super) fn charge(&mut self, id: &str) -> u64 {
        self.next += 1;
        self.starts
            .entry(id.into())
            .or_default()
            .push_back((self.next, Instant::now()));
        self.next
    }
    /// Returns the budget of a request that was admitted but never sent.
    pub(super) fn refund(&mut self, id: &str, token: u64) {
        if let Some(starts) = self.starts.get_mut(id) {
            starts.retain(|(candidate, _)| *candidate != token);
        }
    }
}
