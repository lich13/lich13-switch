// 覆盖故障转移阶段、候选排除与退出证据。
//! Bounded failover evidence and explicit retry-after parsing regressions.
use super::failure_policy;
use crate::usage::model::Record;
use serde_json::{json, Value};
use std::time::Duration;

const MAX_RETRY_BODY: usize = 128 * 1024;

#[test]
fn old_usage_records_without_failover_evidence_remain_compatible() {
    let old: Record = serde_json::from_value(json!({
        "id": "fixture-legacy-record",
        "client": "codex",
        "source": "proxy",
        "startedAt": 1,
        "attempts": [],
        "completed": false
    }))
    .unwrap();

    assert!(old.failover_evidence.is_none());
    let round_trip = serde_json::to_value(old).unwrap();
    assert!(round_trip.get("failoverEvidence").is_none());
}

fn failure_event(field: &str, value: Value) -> Value {
    let mut event = json!({
        "type": "error",
        "error": {"code": "rate_limit_exceeded"}
    });
    event["error"][field] = value;
    event
}

#[test]
fn retry_after_reads_explicit_relative_and_absolute_fields() {
    for field in ["retry_after", "retry_after_seconds", "resets_in_seconds"] {
        assert_eq!(
            failure_policy::retry_after_value(&failure_event(field, json!(9))),
            Some(Duration::from_secs(9)),
            "field {field}"
        );
    }

    let future = chrono::Utc::now().timestamp().max(0) as u64 + 120;
    for field in ["resets_at", "reset_at", "reset_at_unix"] {
        let seconds = failure_policy::retry_after_value(&failure_event(field, json!(future)))
            .expect("explicit reset field was ignored")
            .as_secs();
        assert!((119..=120).contains(&seconds), "field {field}: {seconds}");
    }

    let rfc3339 = chrono::DateTime::<chrono::Utc>::from_timestamp(future as i64, 0)
        .unwrap()
        .to_rfc3339();
    let seconds = failure_policy::retry_after_value(&failure_event("resets_at", json!(rfc3339)))
        .expect("RFC3339 reset timestamp was ignored")
        .as_secs();
    assert!((119..=120).contains(&seconds), "RFC3339: {seconds}");
}

#[test]
fn retry_after_ignores_negative_unstructured_and_invalid_timestamps() {
    let negative = json!({
        "type": "error",
        "error": {
            "code": "rate_limit_exceeded",
            "retry_after": -1,
            "retry_after_seconds": "-3",
            "resets_in_seconds": "not-a-number",
            "message": "fixture retry_after=300"
        }
    });
    assert_eq!(failure_policy::retry_after_value(&negative), None);

    let huge = failure_event("reset_at_unix", json!(u64::MAX));
    assert_eq!(failure_policy::retry_after_value(&huge), None);

    let http_body = br#"{"error":{"code":"rate_limit_exceeded","retry_after_seconds":"17"}}"#;
    assert_eq!(
        failure_policy::retry_after_http(http_body),
        Some(Duration::from_secs(17))
    );

    let mut oversized = br#"{"error":{"code":"rate_limit_exceeded","retry_after":17}}"#.to_vec();
    oversized.resize(MAX_RETRY_BODY + 1, b' ');
    assert_eq!(failure_policy::retry_after_http(&oversized), None);
}

#[test]
fn completed_responses_and_success_payload_text_do_not_supply_retry_timing() {
    let completed = json!({
        "type": "response.completed",
        "response": {
            "status": "completed",
            "output": [{"type": "message", "text": "fixture retry_after=86400"}]
        }
    });
    assert_eq!(failure_policy::retry_after_value(&completed), None);

    let success_body =
        br#"{"id":"fixture-response","status":"completed","output":[{"retry_after":86400}]}"#;
    assert_eq!(failure_policy::retry_after_http(success_body), None);
}
