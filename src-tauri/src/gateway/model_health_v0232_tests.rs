use super::super::{
    circuit::{CircuitState, Outcome},
    model::Settings,
};
use super::Models;
use crate::{
    events::{Action, Reason, Record},
    gateway::ClientId,
};

const PROVIDER: &str = "00000000-0000-0000-0000-000000000001";

#[test]
fn model_health_retains_half_open_probe_until_success_threshold() {
    let models = Models::default();
    let circuit = models.circuit("fixture-model");
    let settings = Settings {
        failure_threshold: 1,
        success_threshold: 2,
        cooldown_seconds: 0,
        ..Settings::default()
    };

    let mut failure = circuit.acquire(false).expect("initial circuit permit");
    failure.set_failure_event(Record::new(
        Some(ClientId::Codex),
        Some(PROVIDER),
        Some("fixture-model"),
        Reason::Authentication,
        Action::Returned,
        Some(401),
        Some(1),
    ));
    failure.finish(Outcome::Failure(None), &settings);
    assert_eq!(circuit.health().state, CircuitState::Open);

    circuit
        .acquire(false)
        .expect("expired circuit permits a recovery probe")
        .finish(Outcome::Success, &settings);
    let after_first_success = models.states();
    assert_eq!(after_first_success.len(), 1);
    assert_eq!(after_first_success[0].health.state, CircuitState::HalfOpen);
    assert!(after_first_success[0].health.available);
    assert_eq!(after_first_success[0].health.failures, 0);
    assert!(after_first_success[0].health.cause.is_none());

    circuit
        .acquire(false)
        .expect("half-open circuit permits the second recovery probe")
        .finish(Outcome::Success, &settings);
    assert_eq!(circuit.health().state, CircuitState::Closed);
    assert!(models.states().is_empty());
}
