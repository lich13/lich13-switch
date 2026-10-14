use crate::events::{HealthScope, Reason, Record};
use serde::Serialize;
use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Status {
    Idle,
    Pending,
    Accepted,
    Failed,
}

#[derive(Default)]
pub struct Gate {
    pending: HashSet<String>,
    accepted: HashMap<String, Instant>,
    failed: HashMap<String, Instant>,
}
pub fn key(r: &Record) -> String {
    if r.reason == crate::events::Reason::ThreadHandoff {
        return format!(
            "handoff:{}",
            r.details.handoff_scope.as_deref().unwrap_or(&r.id)
        );
    }
    // Model failures have their own health scope; provider-wide failures must
    // coalesce even when different models happened to trigger them.
    if r.reason == Reason::CircuitOpen {
        return format!(
            "circuit:{:?}:{}:{:?}:{}",
            r.client_id,
            r.provider_id.as_deref().unwrap_or(""),
            r.details.health_scope,
            if r.details.health_scope == Some(HealthScope::Provider) {
                ""
            } else {
                r.model.as_deref().unwrap_or("")
            }
        );
    }
    format!(
        "{:?}:{}:{}:{:?}",
        r.client_id,
        r.provider_id.as_deref().unwrap_or(""),
        r.model.as_deref().unwrap_or(""),
        r.reason
    )
}
impl Gate {
    pub fn begin(&mut self, key: &str, now: Instant) -> bool {
        self.accepted
            .retain(|_, at| now.saturating_duration_since(*at) < Duration::from_secs(300));
        self.failed
            .retain(|_, at| now.saturating_duration_since(*at) < Duration::from_secs(5));
        if self.pending.len() >= 64
            || self.accepted.contains_key(key)
            || self.failed.contains_key(key)
        {
            return false;
        }
        self.pending.insert(key.into())
    }
    pub fn finish(&mut self, key: &str, accepted: bool, now: Instant) {
        self.pending.remove(key);
        if accepted {
            self.accepted.insert(key.into(), now);
        } else {
            self.failed.insert(key.into(), now);
        }
    }
}
