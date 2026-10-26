//! v0.25.7 upstream reason classification and redaction boundaries.
//!
//! All payloads below are synthetic fixtures.  These tests exercise only the
//! bounded error envelope and never use production responses or credentials.
use super::failure_policy;
use crate::events::{Details, HealthScope};
use serde_json::json;

#[test]
fn daily_limit_reason_wins_over_429_capacity_and_stays_provider_scoped() {
    let details = failure_policy::details_value(
        &json!({
            "error": {
                "type": "error",
                "code": 429,
                "reason": "DAILY_LIMIT_EXCEEDED",
                "message": "temporary rate limit while the daily account limit is exhausted"
            }
        }),
        true,
    );

    assert_eq!(details.upstream_code.as_deref(), Some("429"));
    assert_eq!(
        details.upstream_reason.as_deref(),
        Some("DAILY_LIMIT_EXCEEDED")
    );
    assert_eq!(details.health_scope, Some(HealthScope::Provider));
    assert!(!failure_policy::temporary_capacity_event(&json!({
        "error": {
            "type": "error",
            "code": 429,
            "reason": "DAILY_LIMIT_EXCEEDED",
            "message": "model is at capacity"
        }
    })));
    assert!(failure_policy::permanent_rejection(
        br#"{"error":{"code":429,"reason":"DAILY_LIMIT_EXCEEDED"}}"#
    ));
}

#[test]
fn usage_limit_reason_remains_a_temporary_capacity_event() {
    let value = json!({
        "response": {
            "error": {
                "type": "error",
                "code": "429",
                "reason": "USAGE_LIMIT_EXCEEDED",
                "message": "try again after the short window"
            }
        }
    });
    let details = failure_policy::details_value(&value, false);

    assert_eq!(details.upstream_code.as_deref(), Some("429"));
    assert_eq!(
        details.upstream_reason.as_deref(),
        Some("USAGE_LIMIT_EXCEEDED")
    );
    assert_eq!(details.health_scope, None);
    assert!(failure_policy::temporary_capacity_event(&value));
    assert!(!failure_policy::permanent_rejection(
        value.to_string().as_bytes()
    ));
}

#[test]
fn wrapped_error_key_and_numeric_code_are_retained_without_opening_arbitrary_content() {
    let details = failure_policy::details_value(
        &json!({
            "data": {
                "error": {
                    "type": "provider_error",
                    "errorKey": 429,
                    "errorMessage": "rate limited by the upstream",
                    "reason": "rate_limit_exceeded"
                }
            }
        }),
        true,
    );

    assert_eq!(details.upstream_code.as_deref(), Some("429"));
    assert_eq!(
        details.upstream_reason.as_deref(),
        Some("rate_limit_exceeded")
    );
    assert_eq!(
        details.message.as_deref(),
        Some("rate limited by the upstream")
    );
}

#[test]
fn text_error_extracts_code_and_quoted_reason_but_redacts_reason_from_message() {
    let body = br#"error: code=429 reason="rate_limit_exceeded" message="fixture secret""#;
    let details = failure_policy::details_http(body);

    assert_eq!(details.upstream_code.as_deref(), Some("429"));
    assert_eq!(
        details.upstream_reason.as_deref(),
        Some("rate_limit_exceeded")
    );
    let message = details.message.expect("bounded text error message");
    assert!(message.contains("[已隐去]"));
    assert!(!message.contains("rate_limit_exceeded"));
    assert!(!message.contains("fixture secret"));
}

#[test]
fn text_error_explicit_message_field_can_carry_the_daily_limit_reason() {
    let details = failure_policy::details_http(
        br#"error: code=429 message="DAILY_LIMIT_EXCEEDED" detail="fixture""#,
    );
    assert_eq!(
        details.upstream_reason.as_deref(),
        Some("DAILY_LIMIT_EXCEEDED")
    );
    assert!(failure_policy::permanent_rejection(
        br#"error: code=429 message="DAILY_LIMIT_EXCEEDED""#
    ));
}

#[test]
fn successful_content_unknown_reason_and_user_text_never_become_upstream_reason() {
    let success = json!({
        "type": "response.completed",
        "response": {
            "code": 429,
            "reason": "DAILY_LIMIT_EXCEEDED",
            "message": "successful fixture content"
        }
    });
    assert_eq!(
        failure_policy::details_value(&success, false),
        Details::default()
    );
    assert!(!failure_policy::temporary_capacity_event(&success));

    let unknown = failure_policy::details_value(
        &json!({
            "error": {
                "type": "error",
                "code": 429,
                "reason": "PRIVATE_REASON_FIXTURE",
                "message": "safe error summary"
            }
        }),
        true,
    );
    assert_eq!(unknown.upstream_code.as_deref(), Some("429"));
    assert_eq!(unknown.upstream_reason, None);

    let user_text = failure_policy::details_value(
        &json!({
            "error": {
                "type": "error",
                "code": 429,
                "message": "user content reason=DAILY_LIMIT_EXCEEDED content=fixture-secret"
            }
        }),
        true,
    );
    assert_eq!(user_text.upstream_reason, None);
    assert_eq!(user_text.health_scope, None);
    assert_eq!(user_text.message, None);
}

#[test]
fn oversized_error_body_is_ignored_before_classification_or_logging() {
    let mut body = b"error: code=429 reason=DAILY_LIMIT_EXCEEDED message=".to_vec();
    body.extend(std::iter::repeat_n(b'x', 128 * 1024));
    body.extend_from_slice(b"\"");

    assert_eq!(failure_policy::details_http(&body), Details::default());
    assert!(!failure_policy::permanent_rejection(&body));
}
