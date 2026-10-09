use super::delivery::{key, Gate};
use crate::{
    events::{Action, Reason, Record},
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
fn model_unavailable_remains_notifiable_while_transport_reconnects_stay_quiet() {
    for action in [
        Action::TryingNext,
        Action::Returned,
        Action::Waiting,
        Action::Stopped,
        Action::Recovered,
        Action::Routed,
        Action::Reconnecting,
        Action::NotRetried,
    ] {
        assert!(
            Reason::ModelUnavailable.notifiable(action),
            "model unavailable {action:?}"
        );
    }
    for reason in [
        Reason::Network,
        Reason::UpstreamService,
        Reason::ProtocolError,
        Reason::Capacity,
        Reason::RateLimit,
    ] {
        for action in [
            Action::TryingNext,
            Action::Waiting,
            Action::Reconnecting,
            Action::Routed,
        ] {
            assert!(
                !reason.notifiable(action),
                "intermediate {reason:?} {action:?}"
            );
        }
        for action in [Action::Returned, Action::NotRetried] {
            assert!(reason.notifiable(action), "terminal {reason:?} {action:?}");
        }
    }
    assert!(!Reason::Recovered.notifiable(Action::Recovered));
    assert!(!Reason::Failover.notifiable(Action::Routed));
}

#[test]
fn circuit_and_terminal_failure_notifications_share_one_accepted_delivery_window() {
    let now = Instant::now();
    let mut gate = Gate::default();
    let circuit = event(Reason::CircuitOpen, Action::Stopped);
    let accepted = key(&circuit);
    assert!(gate.begin(&accepted, now));
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
        assert_eq!(key(&terminal), accepted);
        assert!(!gate.begin(&key(&terminal), now));
    }
    gate.finish(&accepted, true, now);
    assert!(!gate.begin(
        &key(&event(Reason::Network, Action::NotRetried)),
        now + Duration::from_secs(299)
    ));
    assert!(gate.begin(
        &key(&event(Reason::FailoverExhausted, Action::Returned)),
        now + Duration::from_secs(300)
    ));
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
