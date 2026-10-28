// Adapted from cc-switch circuit_breaker.rs and forwarder.rs, commit 1ee2fdc3.
// Copyright (c) 2025 JasonYoung. MIT; see THIRD_PARTY_NOTICES.md.
// Independent 429 cooldown follows Sub2API 9a62841f (default: 5 seconds).
use super::model::Settings;
use serde::Serialize;
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

const FAILURE_WINDOW: Duration = Duration::from_secs(60);
const MAX_HEALTH_SAMPLES: usize = 1024;

#[derive(Clone, Copy, PartialEq)]
enum SampleKind {
    Success,
    EarlierSuccess,
    Failure,
    Transient,
}

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
    pub cause: Option<Cause>,
}
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Cause {
    pub event_id: String,
    pub code: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream_reason: Option<String>,
    pub error_basis: Option<crate::events::ErrorBasis>,
    pub reason: crate::events::Reason,
    pub status: Option<u16>,
    pub ws_close_code: Option<u16>,
}
impl Cause {
    fn from_event(event: &crate::events::Record) -> Self {
        Self {
            event_id: event.id.clone(),
            upstream_reason: event.details.upstream_reason.clone(),
            error_basis: event.details.error_basis,
            code: event
                .details
                .upstream_code
                .clone()
                .or(event.details.local_code.clone())
                .unwrap_or_else(|| {
                    serde_json::to_value(event.reason.code())
                        .ok()
                        .and_then(|v| v.as_str().map(str::to_owned))
                        .unwrap_or_default()
                }),
            reason: event.reason,
            status: event.status,
            ws_close_code: event.details.ws_close_code,
        }
    }
}
struct State {
    phase: CircuitState,
    failures: u32,
    successes: u32,
    total: u32,
    failed: u32,
    samples: VecDeque<(Instant, SampleKind)>,
    until: Option<Instant>,
    probe: bool,
    generation: u64,
    retry_until: Option<Instant>,
    retry_reason: Option<&'static str>,
    retry_probe: bool,
    retry_generation: u64,
    rate_streak: u32,
    protected_single_provider: bool,
    protection: Option<Duration>,
    revision: u64,
    open_event: Option<crate::events::Record>,
    cause: Option<Cause>,
}
impl Default for State {
    fn default() -> Self {
        Self {
            phase: CircuitState::Closed,
            failures: 0,
            successes: 0,
            total: 0,
            failed: 0,
            samples: VecDeque::new(),
            until: None,
            probe: false,
            generation: 0,
            retry_until: None,
            retry_reason: None,
            retry_probe: false,
            retry_generation: 0,
            rate_streak: 0,
            protected_single_provider: false,
            protection: None,
            revision: 0,
            open_event: None,
            cause: None,
        }
    }
}
impl State {
    fn refresh_samples(&mut self, now: Instant) {
        while self
            .samples
            .front()
            .is_some_and(|(at, _)| now.saturating_duration_since(*at) >= FAILURE_WINDOW)
        {
            self.samples.pop_front();
        }
        self.total = self.samples.len() as u32;
        self.failed = self
            .samples
            .iter()
            .filter(|(_, kind)| matches!(kind, SampleKind::Failure | SampleKind::Transient))
            .count() as u32;
        self.failures = self
            .samples
            .iter()
            .rev()
            .find(|(_, kind)| *kind != SampleKind::EarlierSuccess)
            .map_or(0, |(_, last)| {
                if *last == SampleKind::Success {
                    return 0;
                }
                self.samples
                    .iter()
                    .rev()
                    .filter(|(_, kind)| *kind != SampleKind::EarlierSuccess)
                    .take_while(|(_, kind)| kind == last)
                    .count() as u32
            });
    }
    fn record(&mut self, kind: SampleKind) {
        let now = Instant::now();
        self.samples.push_back((now, kind));
        while self.samples.len() > MAX_HEALTH_SAMPLES {
            self.samples.pop_front();
        }
        self.refresh_samples(now);
    }
    fn fault_rate(&self) -> (u32, u32) {
        // Intermittent 5xx responses have their own consecutive threshold.
        // They must not later trip the generic error-rate path indirectly.
        self.samples
            .iter()
            .fold((0, 0), |(total, failed), (_, kind)| match kind {
                SampleKind::Success | SampleKind::EarlierSuccess => (total + 1, failed),
                SampleKind::Failure => (total + 1, failed + 1),
                SampleKind::Transient => (total, failed),
            })
    }
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
    HardRejected(Option<Duration>),
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
    prepared_delay: Option<Duration>,
}
impl Circuit {
    pub(super) fn protection(&self) -> Option<Duration> {
        self.0.lock().unwrap().protection
    }
    pub(super) fn discardable(&self) -> bool {
        if Arc::strong_count(&self.0) != 1 {
            return false;
        }
        let h = self.health();
        h.available && h.failures == 0 && h.cause.is_none()
    }
    pub fn take_open_event(&self) -> Option<crate::events::Record> {
        self.0.lock().unwrap().open_event.take()
    }
    pub fn health(&self) -> Health {
        let mut s = self.0.lock().unwrap();
        let now = Instant::now();
        s.refresh_samples(now);
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
            cause: s.cause.clone(),
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
            // A recovery probe starts a new failure streak; old failures must
            // not turn a single transient response into an immediate reopen.
            s.samples.clear();
            s.failures = 0;
            s.failed = 0;
            s.total = 0;
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
            prepared_delay: None,
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
    pub(super) fn prepare_delay(
        &mut self,
        cfg: &Settings,
        details: &crate::events::Details,
    ) -> Option<Duration> {
        let server = details.wait_seconds.map(Duration::from_secs);
        self.prepared_delay = if super::failure_policy::hard_rejection(details) {
            Some(
                server
                    .filter(|d| !d.is_zero())
                    .unwrap_or(Duration::from_secs(cfg.hard_failure_cooldown_seconds)),
            )
        } else if details.error_basis == Some(crate::events::ErrorBasis::Capacity) {
            Some(
                server
                    .unwrap_or_default()
                    .max(Duration::from_secs(cfg.capacity_retry_seconds)),
            )
        } else if super::failure_policy::temporary(details) {
            let streak = self.circuit.0.lock().unwrap().rate_streak.saturating_add(1);
            let bytes = uuid::Uuid::new_v4().into_bytes();
            Some(super::failure_policy::rate_delay(
                cfg.rate_limit_seconds,
                streak,
                server,
                u16::from_le_bytes([bytes[0], bytes[1]]),
            ))
        } else {
            None
        };
        self.prepared_delay
    }
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
        if !matches!(outcome, Outcome::Neutral | Outcome::Success) {
            s.cause = self.failure_event.as_ref().map(Cause::from_event);
        }
        match outcome {
            Outcome::Neutral => (),
            Outcome::RateLimited(retry) => {
                s.rate_streak = s.rate_streak.saturating_add(1);
                let bytes = uuid::Uuid::new_v4().into_bytes();
                let delay = self.prepared_delay.unwrap_or_else(|| {
                    super::failure_policy::rate_delay(
                        cfg.rate_limit_seconds,
                        s.rate_streak,
                        retry,
                        u16::from_le_bytes([bytes[0], bytes[1]]),
                    )
                });
                s.cooldown(delay, "rate_limit");
                s.protected_single_provider |= protected_single_provider;
            }
            Outcome::HardRejected(retry) => {
                s.cooldown(
                    self.prepared_delay
                        .or(retry)
                        .filter(|d| !d.is_zero())
                        .unwrap_or(Duration::from_secs(cfg.hard_failure_cooldown_seconds)),
                    "hard_rejection",
                );
                s.phase = CircuitState::Closed;
                s.until = None;
                s.samples.clear();
                s.failures = 0;
                s.successes = 0;
                s.total = 0;
                s.failed = 0;
            }
            Outcome::CapacityLimited(retry) => {
                // Capacity cooldown is separate from the normal 429/fault
                // policy. Even when the upstream sends a shorter hint, keep
                // the Codex capacity retry floor so another request does not
                // immediately stampede the same unavailable model.
                let delay = self
                    .prepared_delay
                    .or(retry)
                    .unwrap_or_default()
                    .max(Duration::from_secs(cfg.capacity_retry_seconds));
                s.cooldown(delay, "capacity_retry");
                s.protected_single_provider |= protected_single_provider;
            }
            Outcome::Success => {
                if self.retry_probe && self.retry_generation == s.retry_generation {
                    s.retry_until = None;
                    s.retry_reason = None;
                    s.rate_streak = 0;
                }
                // An earlier request completing successfully cannot erase a
                // newer cooldown or release its recovery probe.
                if self.retry_generation != s.retry_generation && s.retry_until.is_some() {
                    s.record(SampleKind::EarlierSuccess);
                    return;
                }
                s.record(SampleKind::Success);
                s.cause = None;
                s.failures = 0;
                s.protected_single_provider = false;
                if s.phase == CircuitState::HalfOpen {
                    s.successes += 1;
                    if s.successes >= cfg.success_threshold {
                        s.phase = CircuitState::Closed;
                        s.until = None;
                        s.total = 0;
                        s.failed = 0;
                        s.samples.clear();
                        s.generation += 1;
                    }
                }
            }
            Outcome::Failure(retry) => {
                let transient = self.failure_event.as_ref().is_some_and(|event| {
                    use crate::errors::code;
                    use crate::events::{Phase, Reason};
                    (event.reason == Reason::UpstreamService
                        && (event.status.is_some_and(is_server_error)
                            || event.details.error_basis
                                == Some(crate::events::ErrorBasis::ServiceError)))
                        || (event.reason == Reason::Network
                            && event.status.is_none()
                            && matches!(
                                event.details.phase,
                                Some(Phase::Connect | Phase::WsHandshake)
                            )
                            && matches!(
                                event.details.local_code.as_deref(),
                                Some(
                                    code::CONNECTION_FAILED
                                        | code::CONNECT_TIMEOUT
                                        | code::WS_HANDSHAKE_TIMEOUT
                                )
                            ))
                });
                s.record(if transient {
                    SampleKind::Transient
                } else {
                    SampleKind::Failure
                });
                let threshold = if transient {
                    cfg.transient_failure_threshold
                } else {
                    cfg.failure_threshold
                };
                let consecutive = s.failures >= threshold;
                let (rate_total, rate_failed) = s.fault_rate();
                let rate_exceeded = rate_total >= cfg.min_requests
                    && f64::from(rate_failed) / f64::from(rate_total) >= cfg.error_rate;
                let should_open = if transient {
                    consecutive
                } else {
                    s.phase != CircuitState::Closed || consecutive || rate_exceeded
                };
                s.successes = 0;
                if protected_single_provider && (!transient || consecutive) {
                    let delay = retry.unwrap_or_default().max(s.protection.unwrap());
                    s.cooldown(delay, "single_provider_protected");
                    s.protected_single_provider = true;
                } else if let Some(retry) = retry {
                    s.cooldown(retry, "retry_after");
                }
                if !protected_single_provider && should_open {
                    let opened_at = Instant::now();
                    let until = opened_at + Duration::from_secs(cfg.cooldown_seconds);
                    let wait = s
                        .retry_until
                        .unwrap_or(until)
                        .max(until)
                        .saturating_duration_since(opened_at);
                    if let Some(cause) = self.failure_event.take() {
                        use crate::events::{Action, CircuitEvidence, Reason, Record};
                        let mut event = Record::new(
                            cause
                                .client_id
                                .and_then(crate::events::JournalClient::active),
                            cause.provider_id.as_deref(),
                            cause.model.as_deref(),
                            Reason::CircuitOpen,
                            Action::Stopped,
                            cause.status,
                            cause.attempt,
                        );
                        event.details = cause.details;
                        event.details.cause_id = Some(cause.id);
                        // The source event may describe a short retry on this
                        // request. Opening the circuit imposes a new deadline.
                        event.details.wait_seconds =
                            Some(wait.as_nanos().div_ceil(1_000_000_000) as u64);
                        event.details.circuit = Some(CircuitEvidence {
                            cause_reason: Some(cause.reason),
                            retry_at: Some(
                                (std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .unwrap_or_default()
                                    + wait)
                                    .as_nanos()
                                    .div_ceil(1_000_000_000) as u64,
                            ),
                            failures: s.failures,
                            failure_threshold: threshold,
                            failed_requests: if !transient && !consecutive {
                                rate_failed
                            } else {
                                s.failed
                            },
                            requests: if !transient && !consecutive {
                                rate_total
                            } else {
                                s.total
                            },
                            error_rate: cfg.error_rate,
                            min_requests: cfg.min_requests,
                            trigger: if transient {
                                "transient_failures"
                            } else if s.phase != CircuitState::Closed {
                                "probe_failed"
                            } else if consecutive {
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
                    s.until = Some(until);
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "circuit_deadline_tests.rs"]
mod v0232_tests;
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
#[cfg(test)]
pub fn retryable(status: u16) -> bool {
    super::failure_policy::retryable_status(status)
}
pub fn is_server_error(status: u16) -> bool {
    (500..=599).contains(&status)
}
/// Positive decimal seconds with nanosecond precision; never truncate a
/// fractional server deadline to an earlier whole second.
pub(super) fn retry_seconds(value: &str) -> Option<Duration> {
    if value.is_empty() || value.len() > 64 {
        return None;
    }
    let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
    if whole.is_empty()
        || !whole.bytes().all(|b| b.is_ascii_digit())
        || !fraction.bytes().all(|b| b.is_ascii_digit())
        || (value.contains('.') && fraction.is_empty())
    {
        return None;
    }
    let seconds = whole.parse::<u64>().ok()?;
    // Keep server deadlines beyond one day. Bound only values that cannot be
    // represented by the monotonic clock; never replace them with an earlier retry.
    std::time::Instant::now().checked_add(Duration::from_secs(seconds))?;
    let mut nanos = fraction
        .bytes()
        .take(9)
        .fold(0u32, |n, b| n * 10 + u32::from(b - b'0'));
    nanos *= 10u32.pow(9 - fraction.len().min(9) as u32);
    if fraction.bytes().skip(9).any(|b| b != b'0') {
        nanos += 1;
    }
    Some(Duration::from_secs(seconds) + Duration::from_nanos(nanos as u64))
}
pub fn retry_after(value: &str) -> Option<Duration> {
    retry_seconds(value.trim()).or_else(|| {
        Some(
            httpdate::parse_http_date(value)
                .ok()?
                .duration_since(std::time::SystemTime::now())
                .unwrap_or_default(),
        )
    })
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
        for s in [400, 404, 405, 406, 409, 413, 414, 415, 422, 501] {
            assert!(!retryable(s));
        }
        for s in [401, 403, 408, 429, 500, 502, 503, 504, 507, 599] {
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
    fn hard_rejection_allows_only_one_on_demand_probe_and_recools_on_failure() {
        let c = Circuit::default();
        let cfg = Settings::default();
        let earlier = c.acquire(false).unwrap();
        c.acquire(false)
            .unwrap()
            .finish(Outcome::HardRejected(None), &cfg);
        assert_eq!(c.health().retry_in, 300);
        assert_eq!(c.health().failures, 0);
        assert!(c.acquire(true).is_none());
        earlier.finish(Outcome::Success, &cfg);
        assert!(!c.health().available);
        c.0.lock().unwrap().retry_until = Some(Instant::now());
        let probe = c.acquire(false).unwrap();
        assert!(c.acquire(false).is_none());
        assert!(c.acquire(true).is_none());
        probe.finish(Outcome::HardRejected(None), &cfg);
        assert_eq!(c.health().retry_in, 300);
        c.0.lock().unwrap().retry_until = Some(Instant::now());
        c.acquire(false).unwrap().finish(Outcome::Success, &cfg);
        assert!(c.health().available);
        assert!(c.health().cooldown_reason.is_none());
    }
    #[test]
    fn rate_limit_respects_local_floor_and_does_not_poison_fault_statistics() {
        let c = Circuit::default();
        let cfg = Settings::default();
        let stale = c.acquire(false).unwrap();
        c.acquire(false)
            .unwrap()
            .finish(Outcome::RateLimited(Some(Duration::from_secs(2))), &cfg);
        assert!((5..=6).contains(&c.health().retry_in));
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
        assert!((5..=6).contains(&c.health().retry_in));
        c.reset();
        c.acquire(false)
            .unwrap()
            .finish(Outcome::RateLimited(retry_after("0")), &cfg);
        assert!(!c.health().available);
        assert!((5..=6).contains(&c.health().retry_in));
        c.reset();
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

#[cfg(test)]
mod v020_tolerance_tests {
    use super::*;
    use crate::events::{Action, Reason, Record};

    fn event(reason: Reason, status: Option<u16>) -> Record {
        Record::new(
            None,
            Some("fixture-provider"),
            Some("fixture-model"),
            reason,
            Action::Returned,
            status,
            Some(1),
        )
    }

    fn finish_failure(
        circuit: &Circuit,
        cfg: &Settings,
        reason: Reason,
        status: Option<u16>,
        retry: Option<Duration>,
    ) {
        let mut permit = circuit.acquire(false).expect("fixture circuit permit");
        permit.set_failure_event(event(reason, status));
        permit.finish(Outcome::Failure(retry), cfg);
    }

    fn finish_transient(circuit: &Circuit, cfg: &Settings, status: u16) {
        finish_failure(circuit, cfg, Reason::UpstreamService, Some(status), None);
    }

    fn expire_retry(circuit: &Circuit) {
        circuit.0.lock().unwrap().retry_until = Some(Instant::now());
    }

    #[test]
    fn connection_failures_share_the_5xx_threshold_and_keep_their_network_cause() {
        use crate::{errors::code, events::Phase};
        let cfg = Settings {
            failure_threshold: 1,
            transient_failure_threshold: 6,
            min_requests: 1,
            ..Settings::default()
        };
        let circuit = Circuit::default();
        for (index, local_code) in [
            code::CONNECTION_FAILED,
            code::CONNECT_TIMEOUT,
            code::WS_HANDSHAKE_TIMEOUT,
        ]
        .into_iter()
        .enumerate()
        {
            let mut record = event(Reason::Network, None);
            record.details.phase = Some(if index == 2 {
                Phase::WsHandshake
            } else {
                Phase::Connect
            });
            record.details.local_code = Some(local_code.into());
            let mut permit = circuit.acquire(false).unwrap();
            permit.set_failure_event(record);
            permit.finish(Outcome::Failure(None), &cfg);
            assert_eq!(circuit.health().state, CircuitState::Closed);
            assert_eq!(circuit.health().cooldown_reason, None);
        }
        for status in [500, 502] {
            finish_transient(&circuit, &cfg, status);
            assert_eq!(circuit.health().state, CircuitState::Closed);
        }
        let mut record = event(Reason::Network, None);
        record.details.phase = Some(Phase::Connect);
        record.details.local_code = Some(code::CONNECTION_FAILED.into());
        let mut permit = circuit.acquire(false).unwrap();
        permit.set_failure_event(record);
        permit.finish(Outcome::Failure(None), &cfg);
        assert_eq!(circuit.health().state, CircuitState::Open);
        let opened = circuit.take_open_event().unwrap();
        assert_eq!(opened.status, None);
        assert_eq!(
            opened.details.local_code.as_deref(),
            Some(code::CONNECTION_FAILED)
        );
        assert_eq!(
            opened.details.circuit.unwrap().trigger,
            "transient_failures"
        );

        for (phase, local_code) in [
            (Phase::Connect, code::TLS_HANDSHAKE_FAILED),
            (Phase::Connect, code::GATEWAY_LOOP),
            (Phase::Headers, code::FIRST_BYTE_TIMEOUT),
            (Phase::Stream, code::CONNECTION_FAILED),
        ] {
            let circuit = Circuit::default();
            let mut record = event(Reason::Network, None);
            record.details.phase = Some(phase);
            record.details.local_code = Some(local_code.into());
            let mut permit = circuit.acquire(false).unwrap();
            permit.set_failure_event(record);
            permit.finish(Outcome::Failure(None), &cfg);
            assert_eq!(circuit.health().state, CircuitState::Open);
        }
    }

    #[test]
    fn transient_errors_need_six_consecutive_samples_and_record_the_threshold() {
        let circuit = Circuit::default();
        let cfg = Settings {
            failure_threshold: 50,
            transient_failure_threshold: 6,
            ..Settings::default()
        };

        for status in [502, 503, 502, 503, 502] {
            finish_transient(&circuit, &cfg, status);
            assert_eq!(circuit.health().state, CircuitState::Closed);
            assert_eq!(circuit.health().cooldown_reason, None);
        }
        finish_transient(&circuit, &cfg, 503);
        let health = circuit.health();
        assert_eq!(health.state, CircuitState::Open);
        assert_eq!(health.failures, 6);
        assert_eq!(health.requests, 6);
        let opened = circuit.take_open_event().expect("transient open event");
        assert_eq!(opened.reason, Reason::CircuitOpen);
        let evidence = opened.details.circuit.expect("circuit evidence");
        assert_eq!(evidence.failure_threshold, 6);
        assert_eq!(evidence.failed_requests, 6);
        assert_eq!(evidence.requests, 6);
        assert_eq!(evidence.trigger, "transient_failures");

        let spaced = Circuit::default();
        for status in [502, 503, 502, 503, 502, 503, 502] {
            finish_transient(&spaced, &cfg, status);
            spaced
                .acquire(false)
                .unwrap()
                .finish(Outcome::Success, &cfg);
        }
        let health = spaced.health();
        assert_eq!(health.state, CircuitState::Closed);
        assert!(health.available);
        assert_eq!(health.cooldown_reason, None);
        assert!(spaced.take_open_event().is_none());
    }

    #[test]
    fn single_provider_stays_closed_while_sixth_transient_failure_applies_cooldown() {
        let circuit = Circuit::default();
        let cfg = Settings {
            transient_failure_threshold: 6,
            ..Settings::default()
        };
        circuit.set_single_provider_protection(Some(Duration::from_secs(30)));
        for status in [502, 503, 502, 503, 502] {
            finish_transient(&circuit, &cfg, status);
            assert_eq!(circuit.health().state, CircuitState::Closed);
            expire_retry(&circuit);
        }
        finish_transient(&circuit, &cfg, 503);
        let health = circuit.health();
        assert_eq!(health.state, CircuitState::Closed);
        assert!(health.protected_single_provider);
        assert_eq!(
            health.cooldown_reason.as_deref(),
            Some("single_provider_protected")
        );
        assert!(health.retry_in >= 29);
        assert!(circuit.take_open_event().is_none());

        let retry = Circuit::default();
        finish_transient(&retry, &cfg, 502);
        expire_retry(&retry);
        let mut permit = retry.acquire(false).unwrap();
        permit.set_failure_event(event(Reason::UpstreamService, Some(503)));
        permit.finish(Outcome::Failure(retry_after("30")), &cfg);
        assert_eq!(retry.health().state, CircuitState::Closed);
        assert_eq!(
            retry.health().cooldown_reason.as_deref(),
            Some("retry_after")
        );
        assert!(retry.health().retry_in >= 29);

        retry
            .0
            .lock()
            .unwrap()
            .samples
            .iter_mut()
            .for_each(|(at, _)| {
                *at = Instant::now() - FAILURE_WINDOW - Duration::from_secs(1);
            });
        expire_retry(&retry);
        let health = retry.health();
        assert_eq!(health.requests, 0);
        assert_eq!(health.failures, 0);
        assert!(health.available);
    }

    #[test]
    fn half_open_transient_failure_and_other_error_classes_keep_separate_rules() {
        let cfg = Settings {
            failure_threshold: 1,
            transient_failure_threshold: 6,
            ..Settings::default()
        };
        let half_open = Circuit::default();
        finish_failure(&half_open, &cfg, Reason::Authentication, Some(401), None);
        assert_eq!(half_open.health().state, CircuitState::Open);
        half_open.take_open_event();
        half_open.0.lock().unwrap().until = Some(Instant::now());
        finish_transient(&half_open, &cfg, 503);
        assert_eq!(half_open.health().state, CircuitState::HalfOpen);
        assert!(half_open.health().available);

        let network = Circuit::default();
        finish_failure(&network, &cfg, Reason::Network, None, None);
        assert_eq!(network.health().state, CircuitState::Open);

        let capacity = Circuit::default();
        let permit = capacity.acquire(false).unwrap();
        permit.finish(
            Outcome::CapacityLimited(Some(Duration::from_secs(30))),
            &cfg,
        );
        let health = capacity.health();
        assert_eq!(health.state, CircuitState::Closed);
        assert_eq!(health.requests, 0);
        assert_eq!(health.failures, 0);
        assert_eq!(health.cooldown_reason.as_deref(), Some("capacity_retry"));
        expire_retry(&capacity);
        capacity
            .acquire(false)
            .unwrap()
            .finish(Outcome::Neutral, &cfg);
        assert_eq!(capacity.health().requests, 0);
    }

    #[test]
    fn half_open_probe_tolerates_one_transient_failure_after_six_closed_failures() {
        let cfg = Settings {
            failure_threshold: 50,
            transient_failure_threshold: 6,
            ..Settings::default()
        };
        let circuit = Circuit::default();
        for status in [502, 503, 502, 503, 502, 503] {
            finish_transient(&circuit, &cfg, status);
        }
        assert_eq!(circuit.health().state, CircuitState::Open);
        circuit
            .take_open_event()
            .expect("initial circuit-open event");

        circuit.0.lock().unwrap().until = Some(Instant::now());
        finish_transient(&circuit, &cfg, 503);
        let health = circuit.health();
        assert_eq!(health.state, CircuitState::HalfOpen);
        assert_eq!(health.failures, 1);
        assert!(health.available);
        assert!(circuit.take_open_event().is_none());
    }
}
