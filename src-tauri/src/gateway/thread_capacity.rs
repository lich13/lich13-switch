//! Capacity reservations are independent of conversation ownership and request slots.
//! Only digests, provider IDs and activity timestamps cross the persistence boundary.
use super::admission::now_wall_ms;
use crate::storage;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    path::Path,
    time::{Duration, Instant},
};
const MAX_RESERVATIONS: usize = 8192;
#[derive(Default)]
pub(super) struct Retention {
    entries: HashMap<(String, String), Entry>,
    requests: HashMap<u64, (String, HashSet<String>)>,
    next: u64,
    pub seconds: u64,
    pub error: bool,
}
struct Entry {
    active: usize,
    pending: usize,
    retained: bool,
    idle: Instant,
    wall: i64,
}
#[derive(Deserialize, Serialize)]
struct Saved {
    thread: String,
    provider: String,
    idle_since: i64,
    busy: bool,
}
impl Retention {
    pub fn load(path: &Path) -> Self {
        let mut state = Self {
            seconds: 300,
            ..Self::default()
        };
        let saved: Vec<Saved> = match storage::read_optional(path) {
            Ok(None) => return state,
            Ok(Some(raw)) if raw.len() <= 4 * 1024 * 1024 => match serde_json::from_slice(&raw) {
                Ok(v) => v,
                Err(_) => {
                    state.error = true;
                    return state;
                }
            },
            _ => {
                state.error = true;
                return state;
            }
        };
        if saved.len() > MAX_RESERVATIONS {
            state.error = true;
            return state;
        }
        let now = now_wall_ms();
        for item in saved {
            if item.thread.len() != 64
                || !item.thread.bytes().all(|b| b.is_ascii_hexdigit())
                || item.provider.is_empty()
                || item.provider.len() > 256
                || item.provider.chars().any(char::is_control)
            {
                state.error = true;
                state.entries.clear();
                return state;
            }
            // A crash never resurrects an in-flight request. Reserve at most one
            // idle slot; future clocks and interrupted requests get one bounded grace.
            let age = if item.busy {
                0
            } else {
                now.saturating_sub(item.idle_since).max(0) as u64
            };
            let instant = Instant::now();
            state.entries.insert(
                (item.thread, item.provider),
                Entry {
                    active: 0,
                    pending: 0,
                    retained: true,
                    idle: instant
                        .checked_sub(Duration::from_millis(age))
                        .unwrap_or(instant),
                    wall: now.saturating_sub(age.min(i64::MAX as u64) as i64),
                },
            );
        }
        state
    }
    pub fn snapshot(&self) -> Vec<u8> {
        let saved: Vec<_> = self
            .entries
            .iter()
            .filter(|(_, e)| e.retained)
            .map(|((thread, provider), e)| Saved {
                thread: thread.clone(),
                provider: provider.clone(),
                idle_since: e.wall,
                busy: e.active != 0 || e.pending != 0,
            })
            .collect();
        serde_json::to_vec(&saved).expect("capacity metadata")
    }
    pub fn configure(&mut self, providers: &HashSet<String>, seconds: u64) {
        self.seconds = seconds;
        self.entries
            .retain(|(_, id), e| providers.contains(id) || e.active != 0 || e.pending != 0);
        self.prune();
    }
    pub fn prune(&mut self) -> bool {
        let len = self.entries.len();
        let limit = Duration::from_secs(self.seconds);
        self.entries.retain(|_, e| {
            e.active != 0 || e.pending != 0 || (e.retained && e.idle.elapsed() < limit)
        });
        len != self.entries.len()
    }
    pub fn begin(&mut self, thread: &str, owner: Option<&str>) -> u64 {
        self.prune();
        self.next += 1;
        let mut providers = HashSet::new();
        if let Some(id) = owner {
            if let Some(entry) = self.entries.get_mut(&(thread.into(), id.into())) {
                entry.pending += 1;
                providers.insert(id.to_owned());
            }
        }
        self.requests.insert(self.next, (thread.into(), providers));
        self.next
    }
    pub fn finish(&mut self, request: u64) {
        if let Some((thread, providers)) = self.requests.remove(&request) {
            for provider in providers {
                if let Some(entry) = self.entries.get_mut(&(thread.clone(), provider)) {
                    entry.pending = entry.pending.saturating_sub(1);
                    if entry.pending == 0 && entry.active == 0 {
                        entry.idle = Instant::now();
                        entry.wall = now_wall_ms();
                    }
                }
            }
        }
        self.prune();
    }
    pub fn hold(&mut self, request: u64, provider: &str) {
        let Some((thread, providers)) = self.requests.get_mut(&request) else {
            return;
        };
        if let Some(entry) = self.entries.get_mut(&(thread.clone(), provider.to_owned())) {
            if providers.insert(provider.to_owned()) {
                entry.pending += 1;
            }
        }
    }
    pub fn reserved(&self, provider: &str) -> usize {
        self.entries
            .iter()
            .filter(|((_, id), e)| id == provider && e.retained && e.active == 0)
            .count()
    }
    pub fn reusable(&self, request: Option<u64>, provider: &str) -> bool {
        request
            .and_then(|id| self.requests.get(&id))
            .and_then(|(thread, _)| self.entries.get(&(thread.clone(), provider.into())))
            .is_some_and(|e| e.retained && e.active == 0)
    }
    pub fn admit(&mut self, request: u64, provider: &str, retain: bool) -> bool {
        let Some((thread, providers)) = self.requests.get_mut(&request) else {
            return false;
        };
        let key = (thread.clone(), provider.to_owned());
        if !self.entries.contains_key(&key) && self.entries.len() >= MAX_RESERVATIONS {
            return false;
        }
        let entry = self.entries.entry(key).or_insert_with(|| Entry {
            active: 0,
            pending: 0,
            retained: false,
            idle: Instant::now(),
            wall: now_wall_ms(),
        });
        if providers.insert(provider.into()) {
            entry.pending += 1;
        }
        entry.active += 1;
        entry.retained |= retain;
        entry.wall = now_wall_ms();
        true
    }
    pub fn release(&mut self, request: u64, provider: &str, success: bool) {
        if let Some((thread, _)) = self.requests.get(&request) {
            if let Some(entry) = self.entries.get_mut(&(thread.clone(), provider.into())) {
                entry.active = entry.active.saturating_sub(1);
                entry.retained |= success;
                if entry.active == 0 && entry.pending == 0 {
                    entry.idle = Instant::now();
                    entry.wall = now_wall_ms();
                }
            }
        }
    }
}
