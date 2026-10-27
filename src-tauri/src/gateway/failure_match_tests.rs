use super::{
    failure_match::{lookup, Input, Parameter as P, Source},
    failure_policy,
    failure_rules::{MatchKind, Rule},
    Edit, Gateway,
};
use crate::events::ErrorBasis as B;
use serde_json::json;

const CAPACITY: &str = "Selected model is at capacity. Please try a different model.";
#[test]
fn unified_status_search_lists_branches_without_claiming_success_is_failure() {
    for status in ["429", "200"] {
        let found = lookup(
            &Input {
                query: Some(status.into()),
                ..Default::default()
            },
            &[],
        )
        .unwrap();
        assert_eq!(found.parsed.status, status.parse().ok());
        assert!(found
            .candidates
            .iter()
            .any(|c| c.category == B::Capacity && c.parameters == [P::CapacityRetrySeconds]));
        assert!(found.candidates.iter().any(|c| c.category == B::Billing));
        assert!(found.candidates.iter().any(|c| c.category == B::Rpm));
        assert_eq!(
            found.category,
            (status == "429").then_some(B::UnknownRateLimit)
        );
    }
    let found = lookup(
        &Input {
            query: Some(CAPACITY.to_lowercase()),
            status: Some(200),
            ..Default::default()
        },
        &[],
    )
    .unwrap();
    assert_eq!(found.category, Some(B::Capacity));
    assert_eq!(found.parameters, [P::CapacityRetrySeconds]);
    let found = lookup(
        &Input {
            query: Some(r#"{"error":{"code":429,"message":"fixture failure"}}"#.into()),
            ..Default::default()
        },
        &[],
    )
    .unwrap();
    assert_eq!(found.parsed.status, None);
    assert_eq!(found.parsed.code.as_deref(), Some("429"));
    let found = lookup(
        &Input {
            query: Some(r#"{"code":429}"#.into()),
            ..Default::default()
        },
        &[],
    )
    .unwrap();
    assert_eq!(found.parsed.status, None);
    assert_eq!(found.parsed.code.as_deref(), Some("429"));
    assert_eq!(found.category, None);
    let found = lookup(
        &Input {
            query: Some(r#"{"success":false,"message":"unknown fixture rejection"}"#.into()),
            status: Some(200),
            ..Default::default()
        },
        &[],
    )
    .unwrap();
    assert_eq!(found.category, Some(B::ServiceError));
}
fn input(code: Option<&str>, message: Option<&str>, status: Option<u16>) -> Input {
    Input {
        code: code.map(str::to_owned),
        message: message.map(str::to_owned),
        status,
        ..Default::default()
    }
}

#[test]
fn structured_corrections_apply_to_the_nested_error_without_changing_transport_status() {
    let found = lookup(&Input {
        query: Some(json!({"success":false,"data":{"error":{"code":"model_capacity_exceeded","message":"fixture"}}}).to_string()),
        code: Some("insufficient_quota".into()), status: Some(200), ..Default::default()
    }, &[]).unwrap();
    assert_eq!(found.parsed.status, Some(200));
    assert_eq!(found.parsed.code.as_deref(), Some("insufficient_quota"));
    assert_eq!(found.category, Some(B::Billing));
}
fn rule(kind: MatchKind, value: &str, category: B) -> Rule {
    Rule {
        id: uuid::Uuid::new_v4().to_string(),
        match_kind: kind,
        value: value.into(),
        category,
    }
}

#[test]
fn code_message_and_status_match_live_classification() {
    for (code, message, status, expected, source) in [
        (
            Some("model_capacity_exceeded"),
            None,
            None,
            B::Capacity,
            Source::BuiltinCode,
        ),
        (
            None,
            Some(CAPACITY),
            None,
            B::Capacity,
            Source::BuiltinMessage,
        ),
        (
            None,
            Some(CAPACITY),
            Some(429),
            B::Capacity,
            Source::BuiltinMessage,
        ),
        (
            None,
            Some(CAPACITY),
            Some(503),
            B::Capacity,
            Source::BuiltinMessage,
        ),
        (
            None,
            Some(CAPACITY),
            Some(400),
            B::InvalidRequest,
            Source::Status,
        ),
        (
            Some("insufficient_quota"),
            Some(CAPACITY),
            Some(429),
            B::Billing,
            Source::BuiltinCode,
        ),
        (Some("429"), None, None, B::UnknownRateLimit, Source::Status),
        (None, None, Some(401), B::Authentication, Source::Status),
        (None, None, Some(429), B::UnknownRateLimit, Source::Status),
    ] {
        let found = lookup(&input(code, message, status), &[]).unwrap();
        assert_eq!(found.category, Some(expected));
        assert_eq!(found.source, Some(source));
        let value = json!({"error":{"code":code,"message":message}});
        let live = if let Some(status) = status {
            failure_policy::http(
                status,
                &hyper::HeaderMap::new(),
                &serde_json::to_vec(&value).unwrap(),
            )
            .0
        } else {
            failure_policy::details_with_rules(&value, false, &[])
        };
        assert_eq!(found.category, live.error_basis);
    }
    let capacity = lookup(&input(None, Some(CAPACITY), None), &[]).unwrap();
    assert_eq!(capacity.evidence, "at capacity");
    assert_eq!(capacity.parameters, [P::CapacityRetrySeconds]);
    assert_eq!(capacity.common_parameters, [P::RetryBudgetSeconds]);
}

#[test]
fn custom_priority_conflicts_and_unmatched_keep_runtime_behavior() {
    let code = rule(MatchKind::Code, "fixture_busy", B::Concurrency);
    let keyword = rule(MatchKind::Keyword, "fixture window", B::Billing);
    let rules = [code.clone(), keyword.clone()];
    let found = lookup(
        &input(Some("FIXTURE_BUSY"), Some("fixture window"), None),
        &rules,
    )
    .unwrap();
    assert_eq!(found.category, Some(B::Concurrency));
    assert_eq!(found.source, Some(Source::CustomCode));
    assert_eq!(found.custom_rule_id.as_deref(), Some(code.id.as_str()));
    assert_eq!(found.parameters, [P::RateLimitSeconds]);
    let found = lookup(&input(Some("fixture_busy"), Some(CAPACITY), None), &rules).unwrap();
    assert_eq!(found.category, Some(B::Capacity));
    assert!(found.custom_rule_id.is_none());
    let conflict = [keyword, rule(MatchKind::Keyword, "window", B::Concurrency)];
    let found = lookup(&input(None, Some("fixture window"), Some(429)), &conflict).unwrap();
    assert!(found.conflict);
    assert_eq!(found.category, Some(B::UnknownRateLimit));
    assert!(found.custom_rule_id.is_none());
    let unknown = lookup(&input(None, Some("unrecognized fixture"), None), &[]).unwrap();
    assert!(unknown.category.is_none());
    assert!(unknown.parameters.is_empty());
    assert!(unknown.common_parameters.is_empty());
    assert!(lookup(&Input::default(), &[]).unwrap().category.is_none());
}

#[test]
fn log_fields_and_fixed_policy_do_not_invent_cooldown_controls() {
    let found = lookup(
        &Input {
            upstream_type: Some("not_found_error".into()),
            message: Some("model: fixture".into()),
            ..Default::default()
        },
        &[],
    )
    .unwrap();
    assert_eq!(found.category, Some(B::ModelUnavailable));
    assert!(found.parameters.is_empty());
    assert_eq!(
        found.common_parameters,
        [P::MaxRetries, P::RetryBudgetSeconds]
    );
    let found = lookup(
        &input(None, Some("content policy violation"), Some(429)),
        &[],
    )
    .unwrap();
    assert_eq!(found.category, Some(B::SafetyRejection));
    assert!(found.parameters.is_empty());
    assert!(found.common_parameters.is_empty());
    let service = lookup(&input(None, None, Some(503)), &[]).unwrap();
    assert_eq!(service.category, Some(B::ServiceError));
    assert_eq!(service.source, Some(Source::Status));
    assert!(service.parameters.contains(&P::TransientFailureThreshold));
    assert!(lookup(&input(None, None, Some(600)), &[]).is_err());
    assert!(lookup(&input(None, Some(&"x".repeat(128 * 1024)), None), &[]).is_ok());
}

#[test]
fn lookup_uses_only_selected_clients_rules_and_never_writes() {
    let dir = tempfile::tempdir().unwrap();
    let codex = Gateway::new(dir.path().join("codex")).unwrap();
    let claude = codex.companion(dir.path().join("claude")).unwrap();
    let rule = rule(MatchKind::Code, "fixture_private_code", B::Capacity);
    codex
        .edit(
            Edit::UpsertFailureRule { rule },
            &codex.view().revision,
            dir.path(),
        )
        .unwrap();
    let revision = codex.view().revision;
    let before = std::fs::read(dir.path().join("codex/gateway.json")).unwrap();
    assert_eq!(
        codex
            .match_failure_policy(input(Some("fixture_private_code"), None, None))
            .unwrap()
            .category,
        Some(B::Capacity)
    );
    assert_eq!(
        claude
            .match_failure_policy(input(Some("fixture_private_code"), None, None))
            .unwrap()
            .category,
        None
    );
    assert_eq!(codex.view().revision, revision);
    assert_eq!(
        std::fs::read(dir.path().join("codex/gateway.json")).unwrap(),
        before
    );
}

#[test]
fn one_box_recognizes_combined_errors_and_preserves_json_business_codes() {
    for query in [
        "model_capacity_exceeded",
        CAPACITY,
        "model_capacity_exceeded: fixture busy",
        "503 model_capacity_exceeded: fixture busy",
        "HTTP/1.1 503 model_capacity_exceeded: fixture busy",
        r#"503 {"error":{"code":"model_capacity_exceeded","message":"fixture busy"}}"#,
        r#"{"success":false,"data":{"error":{"code":"model_capacity_exceeded","message":"fixture busy"}}}"#,
    ] {
        let found = lookup(
            &Input {
                query: Some(query.into()),
                ..Default::default()
            },
            &[],
        )
        .unwrap();
        assert_eq!(found.category, Some(B::Capacity), "{query}");
        assert_eq!(found.origin, Some(crate::events::Origin::Upstream));
    }
    for query in [
        "fixture_unknown: fixture failure",
        "unrecognized fixture message",
        "fixtureunknown",
    ] {
        let found = lookup(
            &Input {
                query: Some(query.into()),
                ..Default::default()
            },
            &[],
        )
        .unwrap();
        assert_eq!(found.category, None, "{query}");
    }
    let rules = [
        rule(MatchKind::Keyword, "fixture window", B::Billing),
        rule(MatchKind::Keyword, "window", B::Concurrency),
    ];
    let found = lookup(
        &Input {
            query: Some("429 fixture window".into()),
            ..Default::default()
        },
        &rules,
    )
    .unwrap();
    assert!(found.conflict);
    assert_eq!(found.category, Some(B::UnknownRateLimit));
}

#[test]
fn single_word_search_checks_keywords_without_losing_code_priority() {
    let rules = [
        rule(MatchKind::Keyword, "forbidden", B::Permission),
        rule(MatchKind::Code, "fixture", B::Billing),
        rule(MatchKind::Keyword, "fixture", B::Concurrency),
    ];
    for (query, expected, source) in [
        ("forbidden", B::Permission, Source::CustomKeyword),
        ("FORBIDDEN", B::Permission, Source::CustomKeyword),
        ("fixture", B::Billing, Source::CustomCode),
        ("fixture: forbidden", B::Billing, Source::CustomCode),
        ("fixture forbidden", B::Billing, Source::CustomCode),
        ("fixture：forbidden", B::Billing, Source::CustomCode),
        ("overloaded", B::Capacity, Source::BuiltinMessage),
        (
            r#"{"error":{"message":"forbidden"}}"#,
            B::Permission,
            Source::CustomKeyword,
        ),
    ] {
        let found = lookup(
            &Input {
                query: Some(query.into()),
                ..Default::default()
            },
            &rules,
        )
        .unwrap();
        assert_eq!(found.category, Some(expected), "{query}");
        assert_eq!(found.source, Some(source), "{query}");
    }
    // Explicit JSON codes must not be reinterpreted as messages.
    let found = lookup(
        &Input {
            query: Some(r#"{"error":{"code":"forbidden"}}"#.into()),
            ..Default::default()
        },
        &rules,
    )
    .unwrap();
    assert!(found.custom_rule_id.is_none());
}

#[test]
fn local_policies_use_a_separate_identity_and_do_not_reclassify_upstream_json() {
    for (query, expected) in [
        ("CONNECT_TIMEOUT", "connection"),
        (
            r#"{"error":{"type":"gpt_switch_gateway","code":"NETWORK","message":"fixture connection failed"}}"#,
            "connection",
        ),
        (
            r#"{"error":{"type":"gpt_switch_gateway","code":"CAPACITY","message":"等待队列已满"}}"#,
            "admission",
        ),
        ("TLS_HANDSHAKE_FAILED: fixture failure", "tls"),
        ("上游连接失败", "connection"),
        ("STREAM_INTERRUPTED fixture closed", "stream"),
        (
            r#"{"error":{"type":"gpt_switch_gateway","code":"CONCURRENCY","message":"fixture busy"}}"#,
            "admission",
        ),
        (
            r#"503 {"error":{"type":"gpt_switch_gateway","code":"NO_PROVIDER","message":"fixture missing"}}"#,
            "routing",
        ),
    ] {
        let found = lookup(
            &Input {
                query: Some(query.into()),
                ..Default::default()
            },
            &[],
        )
        .unwrap();
        assert_eq!(
            found.origin,
            Some(crate::events::Origin::Gateway),
            "{query}"
        );
        assert_eq!(found.policy_id, Some(expected), "{query}");
        assert_eq!(found.category, None);
    }
    let found = lookup(
        &Input {
            query: Some(
                r#"{"error":{"code":"CONNECT_TIMEOUT","message":"fixture upstream failure"}}"#
                    .into(),
            ),
            ..Default::default()
        },
        &[],
    )
    .unwrap();
    assert_ne!(found.origin, Some(crate::events::Origin::Gateway));
    assert_eq!(found.policy_id, None);
    let found = lookup(&Input { query: Some(r#"{"error":{"type":"gpt_switch_gateway","code":"FIXTURE_UNKNOWN","message":"fixture unknown"}}"#.into()), ..Default::default() }, &[]).unwrap();
    assert_eq!(found.origin, Some(crate::events::Origin::Gateway));
    assert_eq!(found.policy_id, None);
}
