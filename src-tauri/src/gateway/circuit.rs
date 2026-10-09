// Adapted from cc-switch circuit_breaker.rs and forwarder.rs, commit 1ee2fdc3.
// Copyright (c) 2025 JasonYoung. MIT; see THIRD_PARTY_NOTICES.md.
// Independent 429 cooldown follows Sub2API 9a62841f (default: 5 seconds).
use super::model::Settings;
use serde::Serialize;
use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

#[derive(Clone, Copy, PartialEq, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CircuitState {
    Closed,
    Open,
    HalfOpen,
}
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Health {
    pub state: CircuitState,
    pub failures: u32,
    pub requests: u32,
    pub retry_in: u64,
    pub cooldown_reason: Option<String>,
    pub protected_single_provider: bool,
    pub probe_in_flight: bool,
    pub available: bool,
    pub revision: u64,
}
struct State {
    phase: CircuitState,
    failures: u32,
    successes: u32,
    total: u32,
    failed: u32,
    until: Option<Instant>,
    probe: bool,
    generation: u64,
    retry_until: Option<Instant>,
    retry_reason: Option<&'static str>,
    retry_probe: bool,
    retry_generation: u64,
    protected_single_provider: bool,
    protection: Option<Duration>,
    revision: u64,
    open_event: Option<crate::events::Record>,
}
impl Default for State {
    fn default() -> Self {
        Self {
            phase: CircuitState::Closed,
            failures: 0,
            successes: 0,
            total: 0,
            failed: 0,
            until: None,
            probe: false,
            generation: 0,
            retry_until: None,
            retry_reason: None,
            retry_probe: false,
            retry_generation: 0,
            protected_single_provider: false,
            protection: None,
            revision: 0,
            open_event: None,
        }
    }
}
impl State {
    fn available(&self) -> bool {
        let now = Instant::now();
        !self.probe
            && !self.retry_probe
            && self.retry_until.is_none_or(|t| t <= now)
            && (self.phase != CircuitState::Open || self.until.is_none_or(|t| t <= now))
    }
    fn cooldown(&mut self, duration: Duration, reason: &'static str) {
        let until = Instant::now() + duration;
        self.retry_until = Some(self.retry_until.map_or(until, |old| old.max(until)));
        self.retry_reason = Some(reason);
        self.retry_probe = false;
        self.retry_generation += 1;
    }
}
#[derive(Clone, Default)]
pub struct Circuit(Arc<Mutex<State>>);
pub enum Outcome {
    Success,
    Failure(Option<Duration>),
    RateLimited(Option<Duration>),
    CapacityLimited(Option<Duration>),
    Neutral,
}
pub struct Permit {
    circuit: Circuit,
    generation: u64,
    half_open: bool,
    retry_generation: u64,
    retry_probe: bool,
    complete: bool,
    failure_event: Option<crate::events::Record>,
}
impl Circuit {
    pub fn take_open_event(&self) -> Option<crate::events::Record> {
        self.0.lock().unwrap().open_event.take()
    }
    pub fn health(&self) -> Health {
        let s = self.0.lock().unwrap();
        let now = Instant::now();
        let retry = [
            s.until.filter(|_| s.phase == CircuitState::Open),
            s.retry_until,
        ]
        .into_iter()
        .flatten()
        .map(|t| t.saturating_duration_since(now))
        .max()
        .unwrap_or_default();
        Health {
            state: s.phase,
            failures: s.failures,
            requests: s.total,
            retry_in: retry.as_millis().div_ceil(1000) as u64,
            cooldown_reason: if s.phase == CircuitState::Open && s.until.is_some_and(|t| t > now) {
                Some("circuit_open".into())
            } else {
                s.retry_reason.map(str::to_owned)
            },
            protected_single_provider: s.protected_single_provider,
            probe_in_flight: s.probe || s.retry_probe,
            available: s.available(),
            revision: s.revision,
        }
    }
    pub fn acquire(&self, _manual: bool) -> Option<Permit> {
        let mut s = self.0.lock().unwrap();
        if !s.available() {
            return None;
        }
        if s.phase == CircuitState::Open {
            s.phase = CircuitState::HalfOpen;
            s.successes = 0;
            s.generation += 1;
        }
        let half_open = s.phase == CircuitState::HalfOpen;
        let retry_probe = s.retry_until.is_some();
        s.probe = half_open;
        s.retry_probe = retry_probe;
        s.revision += 1;
        Some(Permit {
            circuit: self.clone(),
            generation: s.generation,
            half_open,
            retry_generation: s.retry_generation,
            retry_probe,
            complete: false,
            failure_event: None,
        })
    }
    pub fn set_single_provider_protection(&self, protection: Option<Duration>) {
        let mut s = self.0.lock().unwrap();
        if s.protection == protection {
            return;
        }
        s.protection = protection;
        if let Some(cooldown) = protection {
            if s.phase != CircuitState::Closed {
                let remaining = s
                    .until
                    .take()
                    .map(|until| until.saturating_duration_since(Instant::now()))
                    .unwrap_or_default();
                s.phase = CircuitState::Closed;
                s.probe = false;
                s.generation += 1;
                s.cooldown(remaining.max(cooldown), "single_provider_protected");
                s.protected_single_provider = true;
            }
        } else {
            s.protected_single_provider = false;
        }
        s.revision += 1;
    }
    pub fn reset(&self) {
        let mut s = self.0.lock().unwrap();
        *s = State {
            generation: s.generation + 1,
            retry_generation: s.retry_generation + 1,
            revision: s.revision + 1,
            protection: s.protection,
            ..Default::default()
        };
    }
}
impl Permit {
    pub fn set_failure_event(&mut self, event: crate::events::Record) {
        self.failure_event = Some(event);
    }
    pub fn finish(mut self, outcome: Outcome, cfg: &Settings) {
        let mut s = self.circuit.0.lock().unwrap();
        let protected_single_provider = s.protection.is_some();
        self.complete = true;
        if self.retry_probe && self.retry_generation == s.retry_generation {
            s.retry_probe = false;
        }
        if s.generation != self.generation {
            return;
        }
        if self.half_open {
            s.probe = false;
        }
        s.revision += 1;
        match outcome {
            Outcome::Neutral => (),
            Outcome::RateLimited(retry) => {
                let base = retry.unwrap_or(Duration::from_secs(cfg.rate_limit_seconds));
                let delay = if protected_single_provider {
                    base.max(Duration::from_secs(cfg.capacity_retry_seconds))
                } else {
                    base
                };
                s.cooldown(
                    delay,
                    if protected_single_provider {
                        "capacity_retry"
                    } else {
                        "rate_limit"
                    },
                );
                s.protected_single_provider |= protected_single_provider;
            }
            Outcome::CapacityLimited(retry) => {
                // Capacity cooldown is separate from the normal 429/fault
                // policy. Even when the upstream sends a shorter hint, keep
                // the Codex capacity retry floor so another request does not
                // immediately stampede the same unavailable model.
                let delay = retry
                    .unwrap_or_default()
                    .max(Duration::from_secs(cfg.capacity_retry_seconds));
                s.cooldown(delay, "capacity_retry");
                s.protected_single_provider |= protected_single_provider;
            }
            Outcome::Success => {
                if self.retry_probe && self.retry_generation == s.retry_generation {
                    s.retry_until = None;
                    s.retry_reason = None;
                }
                // An earlier request completing successfully cannot erase a
                // newer cooldown or release its recovery probe.
                if self.retry_generation != s.retry_generation
                    && matches!(
                        s.retry_reason,
                        Some("capacity_retry" | "single_provider_protected")
                    )
                {
                    return;
                }
                s.failures = 0;
                s.protected_single_provider = false;
                s.total = s.total.saturating_add(1);
                if s.phase == CircuitState::HalfOpen {
                    s.successes += 1;
                    if s.successes >= cfg.success_threshold {
                        s.phase = CircuitState::Closed;
                        s.until = None;
                        s.total = 0;
                        s.failed = 0;
                        s.generation += 1;
                    }
                }
            }
            Outcome::Failure(retry) => {
                if protected_single_provider {
                    let delay = retry.unwrap_or_default().max(s.protection.unwrap());
                    s.cooldown(delay, "single_provider_protected");
                    s.protected_single_provider = true;
                } else if let Some(retry) = retry {
                    s.cooldown(retry, "retry_after");
                }
                s.failures = s.failures.saturating_add(1);
                s.failed = s.failed.saturating_add(1);
                s.total = s.total.saturating_add(1);
                if !protected_single_provider
                    && (s.phase != CircuitState::Closed
                        || s.failures >= cfg.failure_threshold
                        || (s.total >= cfg.min_requests
                            && f64::from(s.failed) / f64::from(s.total) >= cfg.error_rate))
                {
                    if let Some(cause) = self.failure_event.take() {
                        use crate::events::{Action, CircuitEvidence, Reason, Record};
                        let mut event = Record::new(
                            cause.client_id,
                            cause.provider_id.as_deref(),
                            cause.model.as_deref(),
                            Reason::CircuitOpen,
                            Action::Stopped,
                            cause.status,
                            cause.attempt,
                        );
                        event.details = cause.details;
                        event.details.cause_id = Some(cause.id);
                        event.details.circuit = Some(CircuitEvidence {
                            failures: s.failures,
                            failure_threshold: cfg.failure_threshold,
                            failed_requests: s.failed,
                            requests: s.total,
                            error_rate: cfg.error_rate,
                            min_requests: cfg.min_requests,
                            trigger: if s.phase != CircuitState::Closed {
                                "probe_failed"
                            } else if s.failures >= cfg.failure_threshold {
                                "consecutive_failures"
                            } else {
                                "error_rate"
                            }
                            .into(),
                        });
                        s.open_event = Some(event);
                    }
                    s.phase = CircuitState::Open;
                    s.probe = false;
                    s.generation += 1;
                    s.until = Some(Instant::now() + Duration::from_secs(cfg.cooldown_seconds));
                }
            }
        }
    }
}
impl Drop for Permit {
    fn drop(&mut self) {
        if !self.complete {
            let mut s = self.circuit.0.lock().unwrap();
            if self.half_open && s.generation == self.generation {
                s.probe = false;
            }
            if self.retry_probe && s.retry_generation == self.retry_generation {
                s.retry_probe = false;
            }
            s.revision += 1;
        }
    }
}
pub fn retryable(status: u16) -> bool {
    status >= 400 && ![400, 405, 406, 413, 414, 415, 422, 501].contains(&status)
}
pub fn retry_after(value: &str) -> Option<Duration> {
    value
        .trim()
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
        .or_else(|| {
            Some(
                httpdate::parse_http_date(value)
                    .ok()?
                    .duration_since(std::time::SystemTime::now())
                    .unwrap_or_default(),
            )
        })
        .map(|d| d.min(Duration::from_secs(86400)))
}
#[cfg(test)]
mod tests {
    use super::*;
    // State-transition cases adapted from the pinned upstream circuit breaker tests.
    #[test]
    fn consecutive_failure_and_single_probe_with_cancel() {
        let c = Circuit::default();
        let cfg = Settings::default();
        for _ in 0..4 {
            c.acquire(false)
                .unwrap()
                .finish(Outcome::Failure(None), &cfg);
        }
        assert!(c.acquire(false).is_none());
        c.0.lock().unwrap().until = Some(Instant::now());
        let p = c.acquire(false).unwrap();
        assert!(c.acquire(false).is_none());
        drop(p);
        c.acquire(false).unwrap().finish(Outcome::Success, &cfg);
        assert_eq!(c.health().state, CircuitState::HalfOpen);
        c.acquire(false).unwrap().finish(Outcome::Success, &cfg);
        assert_eq!(c.health().state, CircuitState::Closed);
    }
    #[test]
    fn stale_success_cannot_close_new_circuit_and_rate_threshold() {
        let c = Circuit::default();
        let cfg = Settings {
            failure_threshold: 20,
            ..Default::default()
        };
        let stale = c.acquire(false).unwrap();
        for i in 0..10 {
            c.acquire(false).unwrap().finish(
                if i < 4 {
                    Outcome::Success
                } else {
                    Outcome::Failure(None)
                },
                &cfg,
            );
        }
        assert_eq!(c.health().state, CircuitState::Open);
        stale.finish(Outcome::Success, &cfg);
        assert_eq!(c.health().state, CircuitState::Open);
    }
    #[test]
    fn classification_and_retry_after() {
        for s in [400, 405, 406, 413, 414, 415, 422, 501] {
            assert!(!retryable(s));
        }
        for s in [401, 403, 408, 429, 500, 502, 503, 504] {
            assert!(retryable(s));
        }
        let c = Circuit::default();
        c.acquire(false)
            .unwrap()
            .finish(Outcome::Failure(retry_after("120")), &Settings::default());
        assert!(c.health().retry_in >= 119);
        assert!(retry_after("bad").is_none());
    }
}

#[cfg(test)]
mod cooldown_tests {
    use super::*;
    #[test]
    fn rate_limit_uses_exact_hint_and_does_not_poison_fault_statistics() {
        let c = Circuit::default();
        let cfg = Settings::default();
        let stale = c.acquire(false).unwrap();
        c.acquire(false)
            .unwrap()
            .finish(Outcome::RateLimited(Some(Duration::from_secs(2))), &cfg);
        assert_eq!(c.health().retry_in, 2);
        assert_eq!(c.health().failures, 0);
        assert_eq!(c.health().requests, 0);
        assert_eq!(c.health().state, CircuitState::Closed);
        stale.finish(Outcome::Success, &cfg);
        assert!(!c.health().available);
        assert_eq!(c.health().requests, 1);
        assert!(c.acquire(true).is_none());
        c.0.lock().unwrap().retry_until = Some(Instant::now());
        let probe = c.acquire(false).unwrap();
        assert!(c.acquire(false).is_none());
        drop(probe);
        c.acquire(false).unwrap().finish(Outcome::Success, &cfg);
        assert!(c.health().available);
        assert!(c.health().cooldown_reason.is_none());
    }
    #[test]
    fn default_cooldown_zero_hint_and_fault_retry_after_are_independent() {
        let c = Circuit::default();
        let cfg = Settings::default();
        c.acquire(false)
            .unwrap()
            .finish(Outcome::RateLimited(None), &cfg);
        assert_eq!(c.health().retry_in, 5);
        c.reset();
        c.acquire(false)
            .unwrap()
            .finish(Outcome::RateLimited(retry_after("0")), &cfg);
        assert!(c.health().available);
        c.acquire(false)
            .unwrap()
            .finish(Outcome::Failure(Some(Duration::from_secs(120))), &cfg);
        assert_eq!(c.health().failures, 1);
        assert_eq!(c.health().retry_in, 120);
        assert_eq!(c.health().state, CircuitState::Closed);
        c.0.lock().unwrap().retry_until = Some(Instant::now());
        c.acquire(false)
            .unwrap()
            .finish(Outcome::Failure(None), &cfg);
        assert_eq!(c.health().state, CircuitState::Closed);
        c.acquire(false)
            .unwrap()
            .finish(Outcome::Failure(None), &cfg);
        c.acquire(false)
            .unwrap()
            .finish(Outcome::Failure(None), &cfg);
        assert_eq!(c.health().state, CircuitState::Open);
        assert_eq!(c.health().retry_in, 60);
    }

    #[test]
    fn protected_single_provider_never_opens_after_repeated_failures() {
        let c = Circuit::default();
        let cfg = Settings {
            capacity_retry_seconds: 1,
            ..Settings::default()
        };
        for _ in 0..8 {
            c.set_single_provider_protection(Some(Duration::from_secs(1)));
            c.0.lock().unwrap().retry_until = Some(Instant::now());
            c.acquire(false)
                .unwrap()
                .finish(Outcome::Failure(None), &cfg);
            assert_eq!(c.health().state, CircuitState::Closed);
            c.0.lock().unwrap().retry_until = Some(Instant::now());
        }
        assert!(c.health().protected_single_provider);
        assert_eq!(
            c.health().cooldown_reason.as_deref(),
            Some("single_provider_protected")
        );
    }

    #[test]
    fn protected_recovery_is_single_probe_and_stale_success_cannot_clear_it() {
        let c = Circuit::default();
        let cfg = Settings::default();
        c.set_single_provider_protection(Some(Duration::from_secs(60)));
        let stale = c.acquire(false).unwrap();
        c.acquire(false)
            .unwrap()
            .finish(Outcome::Failure(None), &cfg);
        stale.finish(Outcome::Success, &cfg);
        assert!(c.health().protected_single_provider);
        assert_eq!(c.health().failures, 1);
        c.0.lock().unwrap().retry_until = Some(Instant::now());
        let probe = c.acquire(false).unwrap();
        assert!(c.acquire(false).is_none());
        drop(probe);
        c.acquire(false).unwrap().finish(Outcome::Success, &cfg);
        assert!(c.health().available);
        assert!(!c.health().protected_single_provider);
        assert!(c.health().cooldown_reason.is_none());
        assert_eq!(c.health().state, CircuitState::Closed);
    }
}
