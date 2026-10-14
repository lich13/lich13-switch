use super::delivery::{key, Gate};
use crate::{
    events::{Action, HealthScope, Reason, Record},
    gateway::ClientId,
};
use std::time::{Duration, Instant};

fn event(reason: Reason, action: Action) -> Record {
    Record::new(
        Some(ClientId::Codex),
        Some("fixture-provider"),
        Some("fixture-model"),
        reason,
        action,
        Some(502),
        Some(1),
    )
}

#[test]
fn only_circuit_open_and_thread_handoff_reasons_are_notifiable() {
    let reasons = [
        Reason::ModelUnavailable,
        Reason::Authentication,
        Reason::UpstreamService,
        Reason::Network,
        Reason::RateLimit,
        Reason::Capacity,
        Reason::FailoverExhausted,
        Reason::Failover,
        Reason::CircuitOpen,
        Reason::Recovered,
        Reason::ConfigConflict,
        Reason::StartupRecovery,
        Reason::AccountSync,
        Reason::ProtocolError,
        Reason::ThreadHandoff,
    ];
    let actions = [
        Action::TryingNext,
        Action::Returned,
        Action::Waiting,
        Action::Stopped,
        Action::Recovered,
        Action::Routed,
        Action::Reconnecting,
        Action::RetryingSame,
        Action::NotRetried,
    ];
    for reason in reasons {
        let expected = matches!(reason, Reason::CircuitOpen | Reason::ThreadHandoff);
        for action in actions {
            assert_eq!(reason.notifiable(action), expected, "{reason:?} {action:?}");
        }
    }
}

#[test]
fn circuit_notifications_follow_health_scope_and_accepted_delivery_window() {
    let now = Instant::now();
    let mut gate = Gate::default();
    let mut circuit = event(Reason::CircuitOpen, Action::Stopped);
    circuit.details.health_scope = Some(HealthScope::Provider);
    let accepted = key(&circuit);
    assert!(gate.begin(&accepted, now));
    gate.finish(&accepted, true, now);

    let mut same_provider = circuit.clone();
    same_provider.model = Some("fixture-other-model".into());
    assert_eq!(key(&same_provider), accepted);

    let mut model_scoped = circuit.clone();
    model_scoped.details.health_scope = Some(HealthScope::Model);
    assert_ne!(key(&model_scoped), accepted);

    for reason in [
        Reason::FailoverExhausted,
        Reason::Network,
        Reason::UpstreamService,
        Reason::ProtocolError,
        Reason::Capacity,
        Reason::RateLimit,
    ] {
        let mut terminal = event(reason, Action::Returned);
        terminal.status = Some(429);
        terminal.attempt = Some(8);
        terminal.details.upstream_code = Some("fixture-terminal".into());
        let terminal_key = key(&terminal);
        assert_ne!(terminal_key, accepted);
        assert!(gate.begin(&terminal_key, now + Duration::from_secs(299)));
        gate.finish(&terminal_key, true, now + Duration::from_secs(299));
    }
    assert!(!gate.begin(&key(&same_provider), now + Duration::from_secs(299)));
    assert!(gate.begin(&key(&same_provider), now + Duration::from_secs(300)));
}

#[test]
fn notification_coalescing_keeps_client_provider_model_and_model_errors_separate() {
    let original = event(Reason::CircuitOpen, Action::Stopped);
    let original_key = key(&original);
    let mut other = original.clone();
    other.client_id = Some(ClientId::Claude);
    assert_ne!(key(&other), original_key);
    other = original.clone();
    other.provider_id = Some("fixture-other-provider".into());
    assert_ne!(key(&other), original_key);
    other = original.clone();
    other.model = Some("fixture-other-model".into());
    assert_ne!(key(&other), original_key);
    assert_ne!(
        key(&event(Reason::ModelUnavailable, Action::TryingNext)),
        original_key
    );
    assert_ne!(
        key(&event(Reason::Authentication, Action::Returned)),
        original_key
    );
}
