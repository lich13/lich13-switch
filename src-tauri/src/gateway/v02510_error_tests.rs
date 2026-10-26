//! v0.25.10 upstream error classification boundaries.
//!
//! Fixtures are synthetic and contain no credentials or user content.  These
//! tests stay at the bounded `failure_policy` API so routing tests can assert
//! the same evidence without opening arbitrary response text.
use super::failure_policy;
use crate::events::{Details, ErrorBasis, HealthScope};
use serde_json::{json, Value};

fn error_body(code: Value, reason: Option<&str>, message: &str) -> Value {
    let mut error = json!({
        "code": code,
        "message": message,
    });
    if let Some(reason) = reason {
        error["reason"] = Value::String(reason.into());
    }
    json!({"error": error})
}

#[test]
fn daily_usage_limit_is_permanent_for_403_and_429_and_retains_reason_code() {
    for status in [403_u16, 429_u16] {
        let value = error_body(
            json!(status),
            Some("DAILY_LIMIT_EXCEEDED"),
            "daily usage limit exceeded",
        );
        let details = failure_policy::details_value(&value, true);
        let expected_code = status.to_string();

        assert_eq!(
            details.upstream_code.as_deref(),
            Some(expected_code.as_str())
        );
        assert_eq!(
            details.upstream_reason.as_deref(),
            Some("DAILY_LIMIT_EXCEEDED")
        );
        assert_eq!(details.error_basis, Some(ErrorBasis::QuotaExhausted));
        assert_eq!(details.health_scope, Some(HealthScope::Provider));
        assert!(failure_policy::permanent_rejection(
            value.to_string().as_bytes()
        ));
    }
}

#[test]
fn daily_quota_exhausted_message_is_permanent_without_promoting_free_form_text() {
    let value = error_body(json!(429), None, "daily quota exhausted");
    let details = failure_policy::details_value(&value, true);

    assert_eq!(details.upstream_code.as_deref(), Some("429"));
    assert_eq!(details.upstream_reason, None);
    assert_eq!(details.error_basis, Some(ErrorBasis::QuotaExhausted));
    assert_eq!(details.health_scope, Some(HealthScope::Provider));
    assert!(failure_policy::permanent_rejection(
        value.to_string().as_bytes()
    ));
}

#[test]
fn daily_weekly_and_monthly_quota_reasons_messages_and_chinese_are_permanent() {
    let fixtures = [
        (
            Some("USER_PLATFORM_DAILY_QUOTA_EXHAUSTED"),
            "quota window reached",
        ),
        (Some("WEEKLY_LIMIT_EXCEEDED"), "quota window reached"),
        (
            Some("USER_PLATFORM_WEEKLY_QUOTA_EXHAUSTED"),
            "quota window reached",
        ),
        (Some("MONTHLY_LIMIT_EXCEEDED"), "quota window reached"),
        (
            Some("USER_PLATFORM_MONTHLY_QUOTA_EXHAUSTED"),
            "quota window reached",
        ),
        (None, "weekly quota exhausted"),
        (None, "monthly quota exhausted"),
        (None, "每周额度已耗尽"),
        (None, "每月额度已耗尽"),
    ];

    for (reason, message) in fixtures {
        let value = error_body(json!(429), reason, message);
        let details = failure_policy::details_value(&value, true);

        assert_eq!(
            details.error_basis,
            Some(ErrorBasis::QuotaExhausted),
            "{message}"
        );
        assert_eq!(
            details.health_scope,
            Some(HealthScope::Provider),
            "{message}"
        );
        assert!(
            failure_policy::permanent_rejection(value.to_string().as_bytes()),
            "{message}"
        );
        if let Some(reason) = reason {
            assert_eq!(details.upstream_reason.as_deref(), Some(reason));
        } else {
            assert_eq!(details.upstream_reason, None);
        }
    }
}

#[test]
fn usage_limit_exceeded_stays_temporary_and_preserves_its_reason_code() {
    let value = error_body(
        json!(429),
        Some("USAGE_LIMIT_EXCEEDED"),
        "try again after the short window",
    );
    let details = failure_policy::details_value(&value, true);

    assert_eq!(details.upstream_code.as_deref(), Some("429"));
    assert_eq!(
        details.upstream_reason.as_deref(),
        Some("USAGE_LIMIT_EXCEEDED")
    );
    assert_eq!(details.error_basis, Some(ErrorBasis::UsageLimit));
    assert_eq!(details.health_scope, None);
    assert!(failure_policy::temporary_capacity_event(&value));
    assert!(!failure_policy::permanent_rejection(
        value.to_string().as_bytes()
    ));
}

#[test]
fn negative_and_descriptive_daily_quota_text_does_not_become_permanent() {
    for message in [
        "daily quota is not exhausted",
        "daily quota is not yet exhausted",
        "documentation: daily quota exhausted describes the provider policy",
    ] {
        let value = error_body(json!(429), None, message);
        let details = failure_policy::details_value(&value, true);

        assert_eq!(
            details.error_basis,
            Some(ErrorBasis::UnknownRateLimit),
            "{message}"
        );
        assert_eq!(details.health_scope, None, "{message}");
        assert!(
            !failure_policy::permanent_rejection(value.to_string().as_bytes()),
            "{message}"
        );
    }
}

#[test]
fn successful_content_never_classifies_embedded_quota_text() {
    let value = json!({
        "type": "response.completed",
        "response": {
            "status": "completed",
            "output": [{"type": "output_text", "text": "daily quota exhausted"}],
            "error": {"code": 429, "reason": "DAILY_LIMIT_EXCEEDED"}
        }
    });

    assert_eq!(
        failure_policy::details_value(&value, false),
        Details::default()
    );
    assert!(!failure_policy::temporary_capacity_event(&value));
    assert!(!failure_policy::permanent_rejection(
        value.to_string().as_bytes()
    ));
}

#[test]
fn oversized_and_sensitive_error_text_never_classify_or_echo_content() {
    let sensitive = json!({
        "error": {
            "code": 429,
            "message": "request body contains daily quota exhausted and fixture-secret"
        }
    });
    let details = failure_policy::details_value(&sensitive, true);
    assert_eq!(details.error_basis, Some(ErrorBasis::UnknownRateLimit));
    assert_eq!(details.health_scope, None);
    assert_eq!(details.message, None);
    assert!(!failure_policy::permanent_rejection(
        sensitive.to_string().as_bytes()
    ));

    let mut oversized =
        br#"{"error":{"code":429,"message":"daily quota exhausted"},"padding":""}"#.to_vec();
    oversized.extend(std::iter::repeat_n(b'x', 128 * 1024));
    assert_eq!(failure_policy::details_http(&oversized), Details::default());
    assert!(!failure_policy::permanent_rejection(&oversized));
}

#[test]
fn rpm_descriptions_and_negations_do_not_override_ambiguous_usage_evidence() {
    for message in [
        "requests per minute limit is 10",
        "not an RPM limit exceeded condition",
        "requests per minute limit has not been reached",
    ] {
        let value = error_body(json!(429), Some("USAGE_LIMIT_EXCEEDED"), message);
        assert_eq!(
            failure_policy::details_value(&value, true).error_basis,
            Some(ErrorBasis::UsageLimit),
            "{message}"
        );
    }
    for message in ["requests per minute limit exceeded", "RPM limit exceeded"] {
        let value = error_body(json!(429), None, message);
        assert_eq!(
            failure_policy::details_value(&value, true).error_basis,
            Some(ErrorBasis::Rpm),
            "{message}"
        );
    }
}

#[test]
fn retry_after_accepts_decimal_and_http_date_without_rounding_ahead_of_the_deadline() {
    use super::circuit;
    use std::time::{Duration, SystemTime};
    assert_eq!(
        circuit::retry_after("1.25"),
        Some(Duration::from_millis(1250))
    );
    assert_eq!(
        circuit::retry_after("0.0000000001"),
        Some(Duration::from_nanos(1))
    );
    assert_eq!(
        circuit::retry_after("86401.1"),
        Some(Duration::from_millis(86_401_100))
    );
    for invalid in ["-1", "NaN", "Infinity", "1.2.3", "1.", "+2"] {
        assert_eq!(circuit::retry_after(invalid), None, "{invalid}");
    }
    let future = httpdate::fmt_http_date(SystemTime::now() + Duration::from_secs(30));
    let duration = circuit::retry_after(&future).unwrap();
    assert!(duration > Duration::from_secs(28) && duration <= Duration::from_secs(30));
    let event = json!({"type":"error","error":{
        "code":"rate_limit_exceeded","retry_after_seconds":1.25,
        "resets_in_seconds":"2.5"}});
    assert_eq!(
        failure_policy::retry_after_value(&event),
        Some(Duration::from_millis(2500))
    );
}
