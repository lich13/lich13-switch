// 覆盖熔断冷却与 Retry-After 探测期限。
use super::super::model::Settings;
use super::{Circuit, CircuitState, Outcome};
use crate::{
    events::{Action, Reason, Record},
    gateway::ClientId,
};
use std::{time::Duration, time::SystemTime, time::UNIX_EPOCH};

const PROVIDER: &str = "00000000-0000-0000-0000-000000000001";

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn settings(cooldown_seconds: u64) -> Settings {
    Settings {
        failure_threshold: 50,
        transient_failure_threshold: 2,
        min_requests: 50,
        cooldown_seconds,
        ..Settings::default()
    }
}

fn fail(
    circuit: &Circuit,
    settings: &Settings,
    retry_after: Option<Duration>,
    cause_wait_seconds: Option<u64>,
) -> Option<Record> {
    let mut permit = circuit.acquire(false).expect("closed fixture circuit");
    let mut cause = Record::new(
        Some(ClientId::Codex),
        Some(PROVIDER),
        Some("fixture-circuit-model"),
        Reason::UpstreamService,
        Action::Returned,
        Some(503),
        Some(1),
    );
    cause.details.wait_seconds = cause_wait_seconds;
    permit.set_failure_event(cause);
    permit.finish(Outcome::Failure(retry_after), settings);
    circuit.take_open_event()
}

fn assert_deadline_matches_event_time(event: &Record) {
    let wait = event.details.wait_seconds.expect("open wait in seconds");
    let deadline = event
        .details
        .circuit
        .as_ref()
        .and_then(|evidence| evidence.retry_at)
        .expect("absolute probe deadline");
    let expected = event.first_at + wait;
    assert!(
        deadline >= expected,
        "deadline must not precede wait: {deadline} < {expected}"
    );
    assert!(
        deadline <= expected + 1,
        "Unix-second rounding may add at most one second: {deadline} > {}",
        expected + 1
    );
}

#[test]
fn real_transient_threshold_opens_after_the_last_below_threshold_failure() {
    let circuit = Circuit::default();
    let cfg = settings(3);

    assert!(fail(&circuit, &cfg, None, None).is_none());
    assert_eq!(circuit.health().state, CircuitState::Closed);
    assert_eq!(circuit.health().failures, 1);

    let opened = fail(&circuit, &cfg, Some(Duration::from_secs(1)), Some(1))
        .expect("second consecutive transient failure opens the circuit");
    assert_eq!(circuit.health().state, CircuitState::Open);
    assert_eq!(opened.reason, Reason::CircuitOpen);
    assert!(opened.details.cause_id.is_some());
    assert_eq!(opened.details.wait_seconds, Some(3));
    let evidence = opened.details.circuit.as_ref().expect("circuit evidence");
    assert_eq!(evidence.cause_reason, Some(Reason::UpstreamService));
    assert_eq!(evidence.failure_threshold, 2);
    assert_eq!(evidence.trigger, "transient_failures");
    assert_deadline_matches_event_time(&opened);
}

#[test]
fn concurrent_failures_preserve_a_long_retry_after_over_a_short_cause_wait() {
    let circuit = Circuit::default();
    let cfg = settings(2);

    // Both requests are already in flight before the first Retry-After arrives.
    let mut first = circuit.acquire(false).expect("first concurrent permit");
    let mut second = circuit.acquire(false).expect("second concurrent permit");
    first.set_failure_event(Record::new(
        Some(ClientId::Codex),
        Some(PROVIDER),
        Some("fixture-circuit-model"),
        Reason::UpstreamService,
        Action::Returned,
        Some(503),
        Some(1),
    ));
    first.finish(Outcome::Failure(Some(Duration::from_secs(8))), &cfg);
    assert_eq!(circuit.health().state, CircuitState::Closed);
    assert!(circuit.health().retry_in >= 7);

    let mut short_cause = Record::new(
        Some(ClientId::Codex),
        Some(PROVIDER),
        Some("fixture-circuit-model"),
        Reason::UpstreamService,
        Action::Returned,
        Some(503),
        Some(1),
    );
    short_cause.details.wait_seconds = Some(1);
    second.set_failure_event(short_cause);
    second.finish(Outcome::Failure(None), &cfg);

    let opened = circuit
        .take_open_event()
        .expect("concurrent threshold failure opens the circuit");
    assert_eq!(opened.details.wait_seconds, Some(8));
    let retry_at = opened
        .details
        .circuit
        .as_ref()
        .and_then(|evidence| evidence.retry_at)
        .expect("absolute probe deadline");
    assert!(retry_at > unix_seconds());
    assert_deadline_matches_event_time(&opened);
}

#[test]
fn single_provider_protection_still_suppresses_circuit_open_events() {
    let circuit = Circuit::default();
    let cfg = settings(3);
    circuit.set_single_provider_protection(Some(Duration::from_secs(30)));

    assert!(fail(&circuit, &cfg, None, None).is_none());
    assert!(fail(&circuit, &cfg, None, None).is_none());
    let health = circuit.health();
    assert_eq!(health.state, CircuitState::Closed);
    assert!(health.protected_single_provider);
    assert!(health.retry_in >= 29);
    assert!(circuit.take_open_event().is_none());
}
