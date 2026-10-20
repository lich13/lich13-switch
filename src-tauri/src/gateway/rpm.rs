//! Rolling minute budgets for starts and independently reserved completions.
//! All transitions are made under the admission lock. Wall times are only for
//! restart recovery; live deadlines use a monotonic clock.
use crate::{events::RpmWaitReason, storage};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, VecDeque},
    path::Path,
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
struct Stamp {
    at: Instant,
    wall: i64,
}
impl Stamp {
    fn now() -> Self {
        Self {
            at: Instant::now(),
            wall: super::admission::now_wall_ms(),
        }
    }
    fn after(delay: Duration) -> Self {
        let now = Self::now();
        Self {
            at: now.at + delay,
            wall: now
                .wall
                .saturating_add(delay.as_millis().min(i64::MAX as u128) as i64),
        }
    }
    fn history(wall: i64) -> Option<Self> {
        let now = Self::now();
        let age = Duration::from_millis(now.wall.saturating_sub(wall).max(0) as u64);
        (age < WINDOW).then(|| Self {
            at: now.at.checked_sub(age).unwrap_or(now.at),
            wall,
        })
    }
    fn deadline(wall: i64, cap: Duration) -> Option<Self> {
        let delay =
            Duration::from_millis(wall.saturating_sub(Self::now().wall).max(0) as u64).min(cap);
        (!delay.is_zero()).then(|| Self::after(delay))
    }
}
#[derive(Clone, Default)]
struct Provider {
    starts: VecDeque<Stamp>,
    completed: VecDeque<Stamp>,
    // None means still observing. Some means an uncertain end, protected to a deadline.
    inflight: HashMap<String, Option<Stamp>>,
    // Retained for v2 ledger compatibility, not used to pace sends.
    last: Option<Stamp>,
    backoff: Option<Stamp>,
    initialize: Option<Stamp>,
}
#[derive(Clone, Default)]
pub(super) struct Ledger {
    providers: HashMap<String, Provider>,
    legacy_until: Option<Stamp>,
}
#[derive(Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct Saved {
    #[serde(default)]
    version: u32,
    providers: HashMap<String, Vec<i64>>,
    #[serde(default)]
    protection: HashMap<String, SavedProtection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    legacy_until: Option<i64>,
}
#[derive(Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct SavedProtection {
    #[serde(default)]
    completed: Vec<i64>,
    #[serde(default)]
    inflight: HashMap<String, Option<i64>>,
    #[serde(default)]
    last: Option<i64>,
    #[serde(default)]
    backoff: Option<i64>,
    #[serde(default)]
    initialize: Option<i64>,
}
pub(super) enum Settlement {
    Success,
    Failed,
    Uncertain,
}
impl Ledger {
    pub(super) fn load(path: &Path) -> (Self, bool) {
        let raw = match storage::read_optional(path) {
            Ok(None) => return (Self::default(), false),
            Ok(Some(raw)) if raw.len() <= 8 * 1024 * 1024 => raw,
            _ => return (Self::default(), true),
        };
        let saved = match serde_json::from_slice::<Saved>(&raw) {
            Ok(saved) if saved.version <= 2 => saved,
            _ => return (Self::default(), true),
        };
        let mut ledger = Self {
            legacy_until: if saved.version < 2 {
                Some(Stamp::after(WINDOW))
            } else {
                saved
                    .legacy_until
                    .and_then(|at| Stamp::deadline(at, WINDOW))
            },
            ..Default::default()
        };
        for (id, values) in saved.providers {
            let p = ledger.providers.entry(id).or_default();
            p.starts = values.into_iter().filter_map(Stamp::history).collect();
            p.starts.make_contiguous().sort_by_key(|s| s.at);
            p.last = p.starts.back().copied();
        }
        for (id, protection) in saved.protection {
            let p = ledger.providers.entry(id).or_default();
            p.completed = protection
                .completed
                .into_iter()
                .filter_map(Stamp::history)
                .collect();
            p.completed.make_contiguous().sort_by_key(|s| s.at);
            p.last = protection.last.and_then(Stamp::history).or(p.last);
            p.backoff = protection
                .backoff
                .and_then(|at| Stamp::deadline(at, Duration::from_secs(86400)));
            p.initialize = protection
                .initialize
                .and_then(|at| Stamp::deadline(at, WINDOW));
            p.inflight = protection
                .inflight
                .into_iter()
                .filter_map(|(id, deadline)| {
                    let until = match deadline {
                        None => Some(Stamp::after(WINDOW)),
                        Some(at) => Stamp::deadline(at, WINDOW),
                    };
                    until.map(|until| (id, Some(until)))
                })
                .collect();
        }
        // Persist recovery exactly once. A failed conversion must fail closed.
        let failed = ledger.save(path).is_err();
        (ledger, failed)
    }
    pub(super) fn save(&self, path: &Path) -> storage::Result<()> {
        let saved = Saved {
            version: 2,
            legacy_until: self.legacy_until.map(|s| s.wall),
            providers: self
                .providers
                .iter()
                .map(|(id, p)| (id.clone(), p.starts.iter().map(|s| s.wall).collect()))
                .collect(),
            protection: self
                .providers
                .iter()
                .map(|(id, p)| {
                    (
                        id.clone(),
                        SavedProtection {
                            completed: p.completed.iter().map(|s| s.wall).collect(),
                            inflight: p
                                .inflight
                                .iter()
                                .map(|(id, deadline)| (id.clone(), deadline.map(|s| s.wall)))
                                .collect(),
                            last: p.last.map(|s| s.wall),
                            backoff: p.backoff.map(|s| s.wall),
                            initialize: p.initialize.map(|s| s.wall),
                        },
                    )
                })
                .collect(),
        };
        let bytes = serde_json::to_vec(&saved).map_err(|_| {
            storage::AppError::new(crate::errors::code::RPM_LEDGER, "无法保存 RPM 状态")
        })?;
        storage::atomic_write(path, &bytes, None)
    }
    pub(super) fn prune(&mut self) {
        let now = Instant::now();
        if self.legacy_until.is_some_and(|s| s.at <= now) {
            self.legacy_until = None;
        }
        self.providers.retain(|_, p| {
            p.starts.retain(|s| now.duration_since(s.at) < WINDOW);
            p.completed.retain(|s| now.duration_since(s.at) < WINDOW);
            p.inflight
                .retain(|_, until| until.is_none_or(|s| s.at > now));
            if p.last.is_some_and(|s| now.duration_since(s.at) >= WINDOW) {
                p.last = None;
            }
            if p.backoff.is_some_and(|s| s.at <= now) {
                p.backoff = None;
            }
            if p.initialize.is_some_and(|s| s.at <= now) {
                p.initialize = None;
            }
            !p.starts.is_empty()
                || !p.completed.is_empty()
                || !p.inflight.is_empty()
                || p.last.is_some()
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
            if let Some(init) = self.legacy_until {
                add(init.at, RpmWaitReason::CompletionWindow);
            }
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
    pub(super) fn commit(&mut self, id: &str, _limit: u32) -> Option<String> {
        let now = Stamp::now();
        let token = uuid::Uuid::new_v4().to_string();
        let p = self.providers.entry(id.into()).or_default();
        p.starts.push_back(now);
        p.last = Some(now);
        p.inflight.insert(token.clone(), None);
        Some(token)
    }
    pub(super) fn settle(&mut self, id: &str, token: &str, result: Settlement) {
        let Some(p) = self.providers.get_mut(id) else {
            return;
        };
        if p.inflight.remove(token).is_none() {
            return;
        }
        match result {
            Settlement::Success => p.completed.push_back(Stamp::now()),
            Settlement::Failed => (),
            Settlement::Uncertain => {
                p.inflight.insert(token.into(), Some(Stamp::after(WINDOW)));
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
