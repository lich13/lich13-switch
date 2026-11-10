//! Capacity reservations are independent of conversation ownership and request slots.
#[cfg(test)]
use super::admission::now_wall_ms;
#[cfg(test)]
use serde::Serialize;
use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};
const MAX_RESERVATIONS: usize = 8192;
pub(super) struct Retention {
    entries: HashMap<(String, String), Entry>,
    requests: HashMap<u64, (String, HashSet<String>)>,
    next: u64,
    pub seconds: u64,
}
impl Default for Retention {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
            requests: HashMap::new(),
            next: 0,
            seconds: 300,
        }
    }
}
struct Entry {
    active: usize,
    pending: usize,
    retained: bool,
    idle: Instant,
    #[cfg(test)]
    wall: i64,
}
#[cfg(test)]
#[derive(Serialize)]
struct Saved {
    thread: String,
    provider: String,
    idle_since: i64,
    busy: bool,
}
impl Retention {
    #[cfg(test)]
    pub fn snapshot(&self) -> Vec<u8> {
        let items: Vec<Saved> = self
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
        serde_json::to_vec(&items).expect("capacity metadata")
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
    pub fn next_expiry(&self) -> Option<Duration> {
        self.entries
            .values()
            .filter(|e| e.retained && e.active == 0 && e.pending == 0)
            .map(|e| Duration::from_secs(self.seconds).saturating_sub(e.idle.elapsed()))
            .min()
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
                        #[cfg(test)]
                        {
                            entry.wall = now_wall_ms();
                        }
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
            #[cfg(test)]
            wall: now_wall_ms(),
        });
        if providers.insert(provider.into()) {
            entry.pending += 1;
        }
        entry.active += 1;
        entry.retained |= retain;
        #[cfg(test)]
        {
            entry.wall = now_wall_ms();
        }
        true
    }
    pub fn release(&mut self, request: u64, provider: &str, success: bool) {
        if let Some((thread, _)) = self.requests.get(&request) {
            if let Some(entry) = self.entries.get_mut(&(thread.clone(), provider.into())) {
                entry.active = entry.active.saturating_sub(1);
                entry.retained |= success;
                if entry.active == 0 && entry.pending == 0 {
                    entry.idle = Instant::now();
                    #[cfg(test)]
                    {
                        entry.wall = now_wall_ms();
                    }
                }
            }
        }
    }
}
