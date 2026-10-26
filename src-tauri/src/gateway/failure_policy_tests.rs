use super::{failure_policy as policy, model::Settings};
use crate::events::{ErrorBasis as B, HealthScope as S};
use hyper::HeaderMap;
use serde_json::{json, Value};
use std::time::Duration;

fn classify(status: u16, value: Value) -> crate::events::Details {
    policy::http(
        status,
        &HeaderMap::new(),
        &serde_json::to_vec(&value).unwrap(),
    )
    .0
}

#[test]
fn http_200_capacity_and_unknown_failures_use_complete_error_envelopes() {
    const MESSAGE: &str = "selected model is at capacity. please try a different model.";
    let values = [
        json!({"message": MESSAGE}),
        json!(MESSAGE),
        json!({"error":{"code":429,"message":MESSAGE}}),
        json!({"error":{"code":"rate_limit_exceeded","message":MESSAGE.to_uppercase()}}),
        json!({"error":{"type":"invalid_request_error","message":MESSAGE}}),
        json!({"success":false,"message":MESSAGE}),
        json!({"data":{"result":{"error":{"message":MESSAGE}}}}),
    ];
    for value in values {
        let details = classify(200, value.clone());
        assert_eq!(details.error_basis, Some(B::Capacity), "{value}");
        assert!(!policy::decision(Some(200), &details).counted_failure);
        for stream in [false, true] {
            let mut protocol = super::protocol::Protocol::new(stream);
            protocol.response(200, stream, "identity");
            let text = if stream {
                format!("data: {value}\n\n")
            } else {
                value.to_string()
            };
            for part in text.as_bytes().chunks(3) {
                protocol.feed(part);
            }
            protocol.finish(Some(200), crate::errors::code::OK);
            assert_eq!(
                protocol.observation.error.error_basis,
                Some(B::Capacity),
                "{value}"
            );
            assert!(!protocol.succeeded());
            assert!(protocol.observation.error.raw.is_some());
        }
    }
    for value in [
        json!({"success":false}),
        json!({"error":{"message":"fixture unknown"}}),
    ] {
        assert_eq!(classify(200, value).error_basis, Some(B::ServiceError));
    }
    for value in [
        json!({"success":true,"message":MESSAGE}),
        json!({"choices":[{"message":{"content":MESSAGE},"finish_reason":"stop"}]}),
        json!({"type":"response.completed","response":{"status":"completed","output":[{"text":MESSAGE}]}}),
    ] {
        assert_eq!(classify(200, value).error_basis, None);
    }
    let message = format!("{} prompt: 'quoted fixture' {}", "x".repeat(4096), MESSAGE);
    let mut protocol = super::protocol::Protocol::new(true);
    protocol.response(200, true, "identity");
    protocol.feed(format!("event: error\ndata: {}\n\n", json!({"message":message})).as_bytes());
    assert_eq!(
        protocol.observation.error.message.as_deref(),
        Some(message.as_str())
    );
    assert_eq!(protocol.observation.error.error_basis, Some(B::Capacity));
}

#[test]
fn complete_sse_and_websocket_errors_keep_wire_text_and_headers() {
    const MESSAGE: &str = "selected model is at capacity. please try a different model.";
    let mut headers = HeaderMap::new();
    headers.append("x-fixture", "first".parse().unwrap());
    headers.append("x-fixture", "second".parse().unwrap());
    headers.insert("retry-after", "11".parse().unwrap());
    for data in [json!({"message": MESSAGE}).to_string(), MESSAGE.into()] {
        let wire =
            format!("id: fixture-id\r\n: fixture comment\r\nevent: error\r\ndata: {data}\r\n\r\n");
        let mut protocol = super::protocol::Protocol::new(true);
        protocol.response_headers(&headers);
        protocol.response(200, true, "identity");
        for chunk in wire.as_bytes().chunks(1) {
            protocol.feed(chunk);
        }
        protocol.finish(Some(200), crate::errors::code::OK);
        let raw = protocol.observation.error.raw.as_ref().unwrap();
        assert_eq!(raw.text().unwrap(), wire);
        assert_eq!(raw.headers, crate::event_raw::headers(&headers));
        assert_eq!(
            protocol.observation.retry_after,
            Some(Duration::from_secs(11))
        );
        assert_eq!(protocol.observation.error.error_basis, Some(B::Capacity));
        let matched = super::failure_match::lookup(
            &super::failure_match::Input {
                status: Some(200),
                raw_response: Some(wire),
                event: raw.event.clone(),
                headers: Some(raw.headers.clone()),
                ..Default::default()
            },
            &[],
        )
        .unwrap();
        assert_eq!(matched.category, Some(B::Capacity));
    }
    let wire = format!("{{ \"type\": \"error\", \"error\": {{\"message\":\"{MESSAGE}\"}} }}");
    let mut protocol = super::protocol::Protocol::new(true);
    protocol.response_headers(&headers);
    protocol.value_raw(&serde_json::from_str(&wire).unwrap(), wire.as_bytes());
    assert_eq!(
        protocol
            .observation
            .error
            .raw
            .as_ref()
            .unwrap()
            .text()
            .unwrap(),
        wire
    );
    assert_eq!(protocol.observation.error.error_basis, Some(B::Capacity));
    assert_eq!(
        protocol.observation.retry_after,
        Some(Duration::from_secs(11))
    );
}

#[test]
fn explicit_reasons_override_generic_status_messages_and_retry_hints() {
    for (code, basis, hard) in [
        ("credit_balance_exhausted", B::Billing, true),
        ("organization_spend_limit_exceeded", B::Billing, true),
        ("project_spend_limit_exceeded", B::Billing, true),
        ("billing_hard_limit_reached", B::Billing, true),
        ("enforced_spend_limit_reached", B::QuotaExhausted, true),
        ("rpm_limit_exceeded", B::Rpm, false),
        ("rps_limit_exceeded", B::Rps, false),
        ("tpm_limit_exceeded", B::Tpm, false),
        ("itpm_limit_exceeded", B::InputTokens, false),
        ("otpm_limit_exceeded", B::OutputTokens, false),
        ("concurrency_limit_exceeded", B::Concurrency, false),
        ("acceleration_limit_exceeded", B::Acceleration, false),
        ("spend_rate_limit_exceeded", B::SpendingRate, false),
        ("model_capacity_exceeded", B::Capacity, false),
    ] {
        let d = classify(
            429,
            json!({"error":{"code":code,"type":"rate_limit_error","message":"too many requests","retry_after":1}}),
        );
        assert_eq!(d.error_basis, Some(basis), "{code}");
        let decision = policy::decision(Some(429), &d);
        assert_eq!(decision.hard, hard, "{code}");
        assert_eq!(decision.temporary, !hard, "{code}");
        assert!(!decision.counted_failure, "{code}");
    }
    let d = classify(
        429,
        json!({"error":{"type":"rate_limit_error","message":"too many requests","details":{"error_code":"enforced_spend_limit_reached"}}}),
    );
    assert_eq!(d.error_basis, Some(B::QuotaExhausted));
    assert_eq!(d.health_scope, Some(S::Provider));
}

fn google(violations: Value) -> Value {
    json!({"error":{"code":429,"status":"RESOURCE_EXHAUSTED","details":[
        {"@type":"type.googleapis.com/google.rpc.QuotaFailure","violations":violations},
        {"@type":"type.googleapis.com/google.rpc.RetryInfo","retryDelay":"172800.25s"}
    ]}})
}

#[test]
fn gemini_mixed_quota_dimensions_keep_hard_scope_and_long_retry_info() {
    let v = google(json!([
        {"quotaId":"GenerateRequestsPerMinutePerProjectPerModel","quotaDimensions":{"model":"fixture"}},
        {"quotaId":"GenerateRequestsPerDayPerProject"},
        {"quotaId":"InputTokensPerMinutePerProjectPerModel","quotaDimensions":{"model":"fixture"}}
    ]));
    let d = classify(429, v.clone());
    assert_eq!(d.error_basis, Some(B::QuotaExhausted));
    assert_eq!(d.health_scope, Some(S::Provider));
    assert_eq!(
        policy::retry_after_value(&v),
        Some(Duration::from_millis(172_800_250))
    );
    for (quota, value, expected) in [
        (
            "GenerateRequestsPerMinutePerProjectPerModel",
            0,
            B::QuotaExhausted,
        ),
        (
            "GenerateRequestsPerWeekPerProjectPerModel",
            10,
            B::QuotaExhausted,
        ),
        ("InputTokensPerMinutePerProjectPerModel", 10, B::InputTokens),
        (
            "OutputTokensPerMinutePerProjectPerModel",
            10,
            B::OutputTokens,
        ),
        ("SpendPerMinutePerProjectPerModel", 10, B::SpendingRate),
    ] {
        let d = classify(
            429,
            google(
                json!([{"quotaId":quota,"quotaValue":value,"quotaDimensions":{"model":"fixture"}}]),
            ),
        );
        assert_eq!(d.error_basis, Some(expected), "{quota}");
        assert_eq!(d.health_scope, Some(S::Model), "{quota}");
    }
}

#[test]
fn headers_refine_messages_but_cannot_override_structured_hard_reasons() {
    let mut headers = HeaderMap::new();
    for (k, v) in [
        ("x-ratelimit-remaining-tokens", "0"),
        ("x-ratelimit-reset-tokens", "1d2h3m4s"),
        ("x-ratelimit-remaining-requests", "0"),
        ("x-ratelimit-reset-requests", "1d3h"),
        ("retry-after", "2"),
    ] {
        headers.insert(k, v.parse().unwrap());
    }
    let (d, wait) = policy::http(
        429,
        &headers,
        br#"{"error":{"type":"rate_limit_error","message":"requests per minute exceeded"}}"#,
    );
    assert_eq!(d.error_basis, Some(B::Tpm));
    assert_eq!(wait, Some(Duration::from_secs(97_200)));
    let (d, _) = policy::http(
        429,
        &headers,
        br#"{"error":{"code":"insufficient_quota","message":"too many requests"}}"#,
    );
    assert_eq!(d.error_basis, Some(B::Billing));
    headers.remove("x-ratelimit-remaining-requests");
    headers.remove("x-ratelimit-remaining-tokens");
    let (_, wait) = policy::http(429, &headers, br#"{"error":{"code":"tpm_limit_exceeded"}}"#);
    assert_eq!(wait, Some(Duration::from_secs(93_784)));
}

#[test]
fn unknown_reasons_and_successful_output_never_become_billing_evidence() {
    for value in [
        json!({"error":{"type":"rate_limit_error","message":"daily quota is not exhausted"}}),
        json!({"error":{"status":"RESOURCE_EXHAUSTED"}}),
        json!({"error":{"message":"documentation: insufficient balance, token limit"}}),
    ] {
        let d = classify(429, value);
        assert_eq!(d.error_basis, Some(B::UnknownRateLimit));
        assert!(!policy::hard_rejection(&d));
    }
    let success = json!({"type":"response.completed","response":{"status":"completed","output":[{"text":"daily quota exhausted; insufficient balance"}]}});
    assert_eq!(policy::details_value(&success, false).error_basis, None);
    assert_eq!(classify(200, success).error_basis, None);
}

#[test]
fn request_denials_do_not_retry_and_model_permissions_stay_isolated() {
    for code in [
        "context_length_exceeded",
        "max_tokens_exceeded",
        "safety_error",
        "content_filter",
    ] {
        let d = classify(429, json!({"error":{"code":code}}));
        assert!(!policy::decision(Some(429), &d).retryable, "{code}");
        assert_eq!(d.health_scope, Some(S::Request));
    }
    for status in [400, 404, 409, 413, 422, 501] {
        assert!(
            !policy::decision(
                Some(status),
                &classify(status, json!({"error":{"message":"fixture"}}))
            )
            .retryable
        );
    }
    let d = classify(
        403,
        json!({"error":{"code":"permission_denied","param":"model"}}),
    );
    assert_eq!(d.health_scope, Some(S::Model));
    let d = classify(403, json!({"error":{"code":"ip_not_authorized"}}));
    assert_eq!(d.health_scope, Some(S::Provider));
}

#[test]
fn exponential_delays_respect_server_floor_and_existing_user_values() {
    assert_eq!(policy::rate_delay(5, 1, None, 0), Duration::from_secs(5));
    assert_eq!(policy::rate_delay(5, 2, None, 0), Duration::from_secs(10));
    assert_eq!(policy::rate_delay(5, 20, None, 0), Duration::from_secs(60));
    assert_eq!(
        policy::rate_delay(120, 20, None, 0),
        Duration::from_secs(120)
    );
    assert!(
        policy::rate_delay(5, 4, Some(Duration::from_secs(172800)), 99)
            >= Duration::from_secs(172800)
    );
    let mut old = serde_json::to_value(Settings::default()).unwrap();
    old.as_object_mut()
        .unwrap()
        .remove("hardFailureCooldownSeconds");
    old["rateLimitSeconds"] = json!(123);
    let settings: Settings = serde_json::from_value(old).unwrap();
    assert_eq!(settings.hard_failure_cooldown_seconds, 300);
    assert_eq!(settings.rate_limit_seconds, 123);
}

#[test]
fn streamed_projection_preserves_claude_and_gemini_error_details_and_deadlines() {
    for event in [
        google(
            json!([{"quotaId":"GenerateRequestsPerDayPerProjectPerModel","quotaDimensions":{"model":"fixture"}}]),
        ),
        json!({"error":{"type":"rate_limit_error","details":{"error_code":"enforced_spend_limit_reached"}}}),
    ] {
        let expected = policy::details_value(&event, false);
        for stream in [false, true] {
            let mut protocol = super::protocol::Protocol::new(stream);
            protocol.response(200, stream, "identity");
            let body = if stream {
                format!("data: {event}\n\n")
            } else {
                event.to_string()
            };
            for chunk in body.as_bytes().chunks(7) {
                protocol.feed(chunk);
            }
            protocol.finish(Some(200), crate::errors::code::HTTP);
            assert_eq!(
                protocol.observation.error.error_basis, expected.error_basis,
                "{stream}"
            );
            assert_eq!(
                protocol.observation.error.health_scope, expected.health_scope,
                "{stream}"
            );
            assert_eq!(
                protocol.observation.retry_after,
                policy::retry_after_value(&event),
                "{stream}"
            );
        }
    }
}

#[test]
fn sse_and_json_apply_limit_headers_only_after_an_error_envelope() {
    let mut headers = HeaderMap::new();
    headers.insert("x-ratelimit-remaining-tokens", "0".parse().unwrap());
    headers.insert("x-ratelimit-reset-tokens", "2d".parse().unwrap());
    headers.insert("retry-after", "3".parse().unwrap());
    let event = json!({"type":"error","error":{"type":"rate_limit_error"}});
    for stream in [false, true] {
        let mut protocol = super::protocol::Protocol::new(stream);
        protocol.response(200, stream, "identity");
        protocol.response_headers(&headers);
        let body = if stream {
            format!("data: {event}\n\n")
        } else {
            event.to_string()
        };
        protocol.feed(body.as_bytes());
        protocol.finish(Some(200), crate::errors::code::HTTP);
        assert_eq!(protocol.observation.error.error_basis, Some(B::Tpm));
        assert_eq!(
            protocol.observation.retry_after,
            Some(Duration::from_secs(172800))
        );
    }
    let success = json!({"type":"response.completed","response":{"status":"completed"}});
    let (details, wait) = policy::details_with_headers(&success, &headers, &[]);
    assert_eq!(details.error_basis, None);
    assert_eq!(wait, None);
}

fn custom(
    kind: super::failure_rules::MatchKind,
    value: &str,
    category: B,
) -> super::failure_rules::Rule {
    super::failure_rules::Rule {
        id: uuid::Uuid::new_v4().to_string(),
        match_kind: kind,
        value: value.into(),
        category,
    }
}
#[test]
fn custom_rules_fill_only_ambiguous_evidence_and_code_precedes_keyword() {
    use super::failure_rules::MatchKind::*;
    let rules = [
        custom(Code, "fixture_throttled", B::Concurrency),
        custom(Keyword, "fixture window", B::Billing),
    ];
    let parse = |body: Value, headers: &HeaderMap| {
        policy::http_with_rules(429, headers, &serde_json::to_vec(&body).unwrap(), &rules).0
    };
    let body = json!({"error":{"code":"fixture_throttled","message":"fixture window"}});
    assert_eq!(
        parse(body.clone(), &HeaderMap::new()).error_basis,
        Some(B::Concurrency)
    );
    assert_eq!(
        parse(
            json!({"error":{"code":"insufficient_quota","message":"fixture window"}}),
            &HeaderMap::new()
        )
        .error_basis,
        Some(B::Billing)
    );
    let mut headers = HeaderMap::new();
    headers.insert("x-ratelimit-remaining-tokens", "0".parse().unwrap());
    let limited = json!({"error":{"code":"fixture_throttled","type":"rate_limit_error","message":"too many requests; fixture window"}});
    assert_eq!(parse(limited, &headers).error_basis, Some(B::Tpm));
    let success = json!({"type":"response.completed","response":{"output":"fixture window","status":"completed"}});
    assert_eq!(
        policy::details_with_rules(&success, false, &rules),
        crate::events::Details::default()
    );
    let concrete =
        json!({"error":{"code":"fixture_throttled","message":"content policy violation"}});
    assert_eq!(
        parse(concrete, &HeaderMap::new()).error_basis,
        Some(B::SafetyRejection)
    );
    let rules = [custom(Code, "usage_limit_exceeded", B::Concurrency)];
    let value =
        json!({"error":{"code":"usage_limit_exceeded","message":"content policy violation"}});
    assert_eq!(
        policy::details_with_rules(&value, false, &rules).custom_rule_id,
        None
    );
    let value = json!({"error":{"code":"fixture_throttled","status":"PERMISSION_DENIED","message":"fixture"}});
    assert_eq!(
        policy::details_with_rules(
            &value,
            false,
            &[custom(Code, "fixture_throttled", B::Concurrency)]
        )
        .custom_rule_id,
        None
    );
}
#[test]
fn custom_conflicts_keep_original_and_streams_share_the_snapshot() {
    use super::failure_rules::MatchKind::*;
    let conflict = [
        custom(Keyword, "fixture", B::Concurrency),
        custom(Keyword, "window", B::Billing),
    ];
    let value = json!({"type":"error","error":{"code":"fixture_throttled","message":"fixture window","type":"rate_limit_error"}});
    let result = policy::details_with_rules(&value, false, &conflict);
    assert_eq!(result.rule_conflict, Some(true));
    assert_eq!(result.custom_rule_id, None);
    let rules = [custom(Code, "fixture_throttled", B::Concurrency)];
    let mut stream = super::protocol::Protocol::new(true);
    stream.attach_failure_rules(&rules);
    stream.response(200, true, "identity");
    stream.feed(format!("data: {value}\n\n").as_bytes());
    assert_eq!(stream.observation.error.error_basis, Some(B::Concurrency));
    assert_eq!(stream.observation.first_event_capacity_error, Some(true));
    let mut ws = super::protocol::Protocol::new(true);
    ws.attach_failure_rules(&rules);
    ws.value(&value);
    assert_eq!(
        ws.observation.error.error_basis,
        stream.observation.error.error_basis
    );
    assert_eq!(
        ws.observation.error.custom_rule_id,
        Some(rules[0].id.clone())
    );
}
#[test]
fn custom_rule_edits_are_isolated_persisted_and_preserve_settings() {
    use super::failure_rules::MatchKind::*;
    let dir = tempfile::tempdir().unwrap();
    let codex = super::Gateway::new(dir.path().join("codex")).unwrap();
    let claude = super::Gateway::new(dir.path().join("claude")).unwrap();
    let before = codex.view();
    let rule = custom(Code, "fixture_custom", B::Capacity);
    codex
        .edit(
            super::Edit::UpsertFailureRule { rule: rule.clone() },
            &before.revision,
            dir.path(),
        )
        .unwrap();
    assert_eq!(
        codex.view().settings.custom_failure_rules.as_slice(),
        std::slice::from_ref(&rule)
    );
    assert!(claude.view().settings.custom_failure_rules.is_empty());
    assert_eq!(codex.view().settings.port, before.settings.port);
    assert!(codex
        .edit(
            super::Edit::DeleteFailureRule {
                id: rule.id.clone()
            },
            &before.revision,
            dir.path()
        )
        .is_err());
    let loaded = super::Gateway::new(dir.path().join("codex")).unwrap();
    assert_eq!(loaded.view().settings.custom_failure_rules, [rule]);
    let mut legacy = serde_json::to_value(Settings::default()).unwrap();
    legacy.as_object_mut().unwrap().remove("customFailureRules");
    assert!(serde_json::from_value::<Settings>(legacy)
        .unwrap()
        .custom_failure_rules
        .is_empty());
}
#[test]
fn every_catalog_code_uses_the_same_classifier_and_categories_are_complete() {
    let catalog = super::failure_rules::catalog();
    assert_eq!(catalog.len(), 20);
    for entry in super::failure_rules::CODES {
        let value = json!({"error":{"code":entry.value}});
        assert_eq!(
            classify(429, value).error_basis,
            Some(entry.category),
            "{}",
            entry.value
        );
        assert!(catalog.iter().any(|c| c.category == entry.category
            && c.codes.iter().any(|code| code.value == entry.value)));
    }
}
