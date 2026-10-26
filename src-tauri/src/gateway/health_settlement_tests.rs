use super::{
    circuit::Outcome,
    health_settlement as settlement,
    model::Settings,
    protocol::{Protocol, Terminal},
    routing::Requirement,
    Edit, Gateway, Route,
};
use crate::{errors::code, events::HealthScope};
use serde_json::json;
use std::time::Duration;

const MODEL: &str = "fixture-model";

fn fixture() -> (tempfile::TempDir, Gateway, Route) {
    let data = tempfile::tempdir().unwrap();
    let gateway = Gateway::new(data.path().to_path_buf()).unwrap();
    let revision = gateway.view().revision;
    let view = gateway
        .edit(
            Edit::SaveProvider {
                search_only: None,
                quota_access_token: None,
                id: None,
                base_url: "https://example.invalid/v1".into(),
                token: "fixture-token".into(),
                name: Some("settlement fixture".into()),
            },
            &revision,
            data.path(),
        )
        .unwrap();
    let route = gateway.route(&view.providers[0].id).unwrap();
    (data, gateway, route)
}

fn permits(route: &Route) -> settlement::Permits {
    settlement::Permits::acquire_for(route, false, &Requirement::Model(MODEL.into()))
        .expect("fixture provider and model permits")
}

fn settings() -> Settings {
    Settings {
        failure_threshold: 100,
        transient_failure_threshold: 100,
        min_requests: 100,
        ..Settings::default()
    }
}

#[test]
fn default_settings_match_current_serialization() {
    assert_eq!(
        serde_json::to_value(Settings::default()).unwrap(),
        json!({
            "port": 43721,
            "maxRetries": 3,
            "failureThreshold": 4,
            "transientFailureThreshold": 6,
            "successThreshold": 2,
            "cooldownSeconds": 60,
            "rateLimitSeconds": 5,
            "capacityRetrySeconds": 60,
            "customFailureRules": [],
            "hardFailureCooldownSeconds": 300,
            "websocketRetrySeconds": 300,
            "threadIdleReleaseSeconds": 300,
            "endpointProbeSeconds": 300,
            "errorRate": 0.6,
            "minRequests": 10,
            "firstByteSeconds": 300,
            "idleSeconds": 300,
            "retryBudgetSeconds": 300,
            "connectSeconds": 300,
            "queueSeconds": 300,
            "maxWaiting": 100
        })
    );
}

#[test]
fn http_json_sse_and_websocket_share_classification_scope_and_settlement() {
    use crate::events::{Action, Reason};
    for (code, scope, cooldown) in [
        (
            "insufficient_quota",
            HealthScope::Provider,
            "hard_rejection",
        ),
        ("invalid_api_key", HealthScope::Provider, "hard_rejection"),
        ("tpm_limit_exceeded", HealthScope::Model, "rate_limit"),
        (
            "concurrency_limit_exceeded",
            HealthScope::Model,
            "rate_limit",
        ),
        (
            "model_capacity_exceeded",
            HealthScope::Model,
            "capacity_retry",
        ),
    ] {
        let event = json!({"type":"error","error":{"code":code}});
        for transport in ["http", "json", "sse", "websocket"] {
            let (_data, gateway, route) = fixture();
            let mut permits = permits(&route);
            let mut protocol = Protocol::new(transport != "json");
            if transport == "http" {
                let (d, wait) = super::failure_policy::http(
                    429,
                    &hyper::HeaderMap::new(),
                    event.to_string().as_bytes(),
                );
                permits.report(
                    &gateway,
                    &route,
                    Some(MODEL),
                    Reason::UpstreamService,
                    Action::Returned,
                    Some(429),
                    1,
                    d,
                );
                permits.failure(&settings(), wait);
            } else {
                if transport == "sse" {
                    protocol.response(200, true, "identity");
                    protocol.feed(format!("event: error\ndata: {event}\n\n").as_bytes());
                } else {
                    protocol.value(&event);
                }
                settlement::observe_protocol(
                    &protocol,
                    &mut permits,
                    &settings(),
                    &gateway,
                    &route,
                    Some(MODEL),
                    Some(200),
                    1,
                );
            }
            let provider = route.provider_circuit.health();
            let model = route.model_circuits.circuit(MODEL).health();
            let (affected, other) = if scope == HealthScope::Provider {
                (&provider, &model)
            } else {
                (&model, &provider)
            };
            assert_eq!(
                affected.cooldown_reason.as_deref(),
                Some(cooldown),
                "{code}/{transport}"
            );
            assert_eq!(affected.failures, 0, "{code}/{transport}");
            assert!(other.available, "{code}/{transport}");
            assert!(
                route
                    .model_circuits
                    .circuit("other-model")
                    .health()
                    .available
            );
            assert_eq!(
                gateway.0.admission.rpm_status(&route.provider.id, 100).1,
                0,
                "token/concurrency/capacity/billing cannot become RPM backoff"
            );
        }
    }
}

#[test]
fn successful_and_limited_terminals_settle_each_permit_once() {
    let cases = [
        (
            "completed",
            json!({
                "type": "response.completed",
                "response": {
                    "id": "fixture-completed-response",
                    "model": MODEL,
                    "status": "completed"
                }
            }),
            Terminal::Success,
        ),
        (
            "limited",
            json!({
                "type": "response.incomplete",
                "response": {
                    "id": "fixture-limited-response",
                    "model": MODEL,
                    "status": "incomplete",
                    "incomplete_details": {"reason": "max_output_tokens"}
                }
            }),
            Terminal::Limited,
        ),
    ];

    for (label, event, expected) in cases {
        let (_data, _gateway, route) = fixture();
        let provider = route.provider_circuit.clone();
        let model = route.model_circuits.circuit(MODEL);
        let mut permits = permits(&route);
        let mut protocol = Protocol::new(true);
        protocol.value(&event);
        assert_eq!(protocol.terminal(), Some(expected), "{label} fixture");

        settlement::settle_protocol(&protocol, &mut permits, &settings(), Some(200));
        settlement::settle_protocol(&protocol, &mut permits, &settings(), Some(200));

        for (scope, circuit) in [("provider", provider), ("model", model)] {
            let health = circuit.health();
            assert_eq!(health.requests, 1, "{label} must count {scope} once");
            assert_eq!(health.failures, 0, "{label} must not fail {scope}");
        }
    }
}

#[test]
fn transport_failures_only_count_the_selected_health_scope() {
    let cases = [
        ("model", Some(200), None, 0, 1),
        ("provider", None, Some(HealthScope::Provider), 1, 0),
    ];

    for (label, status, scope, provider_failures, model_failures) in cases {
        let (_data, gateway, route) = fixture();
        let provider = route.provider_circuit.clone();
        let model = route.model_circuits.circuit(MODEL);
        let mut permits = permits(&route);
        let mut protocol = Protocol::new(true);
        protocol.observation.error.health_scope = scope;
        protocol.finish(status, code::STREAM_INTERRUPTED);
        assert!(protocol.transport_failure(), "{label} fixture");

        settlement::observe_protocol(
            &protocol,
            &mut permits,
            &settings(),
            &gateway,
            &route,
            Some(MODEL),
            status,
            1,
        );

        assert_eq!(
            provider.health().failures,
            provider_failures,
            "{label} provider"
        );
        assert_eq!(model.health().failures, model_failures, "{label} model");
        assert_eq!(
            provider.health().requests,
            provider_failures,
            "{label} provider requests"
        );
        assert_eq!(
            model.health().requests,
            model_failures,
            "{label} model requests"
        );
    }
}

#[test]
fn explicit_service_failure_on_http_200_counts_only_model_health() {
    let (_data, gateway, route) = fixture();
    let provider = route.provider_circuit.clone();
    let model = route.model_circuits.circuit(MODEL);
    let mut permits = permits(&route);
    let mut protocol = Protocol::new(true);
    protocol.value(&json!({
        "type": "response.failed",
        "response": {
            "id": "fixture-application-error",
            "model": MODEL,
            "status": "failed",
            "error": {"code": "server_error"}
        }
    }));
    assert_eq!(protocol.terminal(), Some(Terminal::Failure));
    assert!(!protocol.transport_failure());

    settlement::observe_protocol(
        &protocol,
        &mut permits,
        &settings(),
        &gateway,
        &route,
        Some(MODEL),
        Some(200),
        1,
    );

    assert_eq!(provider.health().requests, 0);
    assert_eq!(provider.health().failures, 0);
    assert_eq!(model.health().requests, 1);
    assert_eq!(model.health().failures, 1);
}

#[test]
fn unterminated_protocol_keeps_permits_until_a_terminal_arrives() {
    let (_data, gateway, route) = fixture();
    let provider = route.provider_circuit.clone();
    let model = route.model_circuits.circuit(MODEL);
    let mut permits = permits(&route);
    let mut protocol = Protocol::new(true);
    assert_eq!(protocol.terminal(), None);

    settlement::observe_protocol(
        &protocol,
        &mut permits,
        &settings(),
        &gateway,
        &route,
        Some(MODEL),
        Some(200),
        1,
    );
    assert_eq!(provider.health().requests, 0);
    assert_eq!(model.health().requests, 0);

    protocol.value(&json!({
        "type": "response.completed",
        "response": {
            "id": "fixture-late-terminal",
            "model": MODEL,
            "status": "completed"
        }
    }));
    settlement::observe_protocol(
        &protocol,
        &mut permits,
        &settings(),
        &gateway,
        &route,
        Some(MODEL),
        Some(200),
        1,
    );

    assert_eq!(provider.health().requests, 1);
    assert_eq!(model.health().requests, 1);
}

#[test]
fn dropping_settlement_permits_releases_retry_probes() {
    let (_data, _gateway, route) = fixture();
    let provider = route.provider_circuit.clone();
    let model = route.model_circuits.circuit(MODEL);
    let cfg = Settings {
        rate_limit_seconds: 0,
        capacity_retry_seconds: 0,
        ..Settings::default()
    };
    for circuit in [&provider, &model] {
        circuit
            .acquire(false)
            .expect("initial permit")
            .finish(Outcome::RateLimited(Some(Duration::ZERO)), &cfg);
    }

    let permits = permits(&route);
    assert!(provider.health().probe_in_flight);
    assert!(model.health().probe_in_flight);
    drop(permits);

    assert!(!provider.health().probe_in_flight);
    assert!(!model.health().probe_in_flight);
    assert!(provider.health().available);
    assert!(model.health().available);
}
