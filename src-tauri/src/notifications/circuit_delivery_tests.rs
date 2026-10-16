// 覆盖熔断通知内容、等待期限和作用域合并。
use super::delivery::{key, Gate};
use crate::{
    events::{Action, HealthScope, Reason, Record},
    gateway::ClientId,
};
use std::time::{Duration, Instant};

const PROVIDER: &str = "00000000-0000-0000-0000-000000000001";

fn circuit(model: &str, scope: HealthScope, client: ClientId, provider: &str) -> Record {
    let mut record = Record::new(
        Some(client),
        Some(provider),
        Some(model),
        Reason::CircuitOpen,
        Action::Stopped,
        Some(503),
        Some(1),
    );
    record.details.health_scope = Some(scope);
    record
}

fn handoff(scope: &str) -> Record {
    let mut record = Record::new(
        Some(ClientId::Codex),
        Some(PROVIDER),
        Some("fixture-model"),
        Reason::ThreadHandoff,
        Action::Routed,
        None,
        None,
    );
    record.details.handoff_scope = Some(scope.into());
    record
}

#[test]
fn circuit_keys_follow_health_scope_and_suppress_provider_duplicates_for_300_seconds() {
    let provider_model_a = key(&circuit(
        "fixture-model-a",
        HealthScope::Provider,
        ClientId::Codex,
        PROVIDER,
    ));
    let provider_model_b = key(&circuit(
        "fixture-model-b",
        HealthScope::Provider,
        ClientId::Codex,
        PROVIDER,
    ));
    assert_eq!(provider_model_a, provider_model_b);

    let model_a = key(&circuit(
        "fixture-model-a",
        HealthScope::Model,
        ClientId::Codex,
        PROVIDER,
    ));
    let model_b = key(&circuit(
        "fixture-model-b",
        HealthScope::Model,
        ClientId::Codex,
        PROVIDER,
    ));
    assert_ne!(model_a, model_b);
    assert_ne!(provider_model_a, model_a);
    assert_ne!(
        provider_model_a,
        key(&circuit(
            "fixture-model-a",
            HealthScope::Provider,
            ClientId::Claude,
            PROVIDER,
        ))
    );
    assert_ne!(
        provider_model_a,
        key(&circuit(
            "fixture-model-a",
            HealthScope::Provider,
            ClientId::Codex,
            "00000000-0000-0000-0000-000000000002",
        ))
    );

    let now = Instant::now();
    let mut gate = Gate::default();
    assert!(gate.begin(&provider_model_a, now));
    gate.finish(&provider_model_a, true, now);
    assert!(!gate.begin(&provider_model_b, now + Duration::from_secs(299)));
    assert!(gate.begin(&provider_model_b, now + Duration::from_secs(300)));
    assert!(gate.begin(&model_a, now + Duration::from_secs(1)));
    assert!(gate.begin(&model_b, now + Duration::from_secs(1)));
}

#[test]
fn handoff_keys_remain_independent_from_circuit_and_other_handoffs() {
    let circuit = key(&circuit(
        "fixture-model",
        HealthScope::Provider,
        ClientId::Codex,
        PROVIDER,
    ));
    let first_handoff = key(&handoff(&"a".repeat(64)));
    let second_handoff = key(&handoff(&"b".repeat(64)));

    assert_ne!(first_handoff, circuit);
    assert_ne!(second_handoff, circuit);
    assert_ne!(first_handoff, second_handoff);
}
