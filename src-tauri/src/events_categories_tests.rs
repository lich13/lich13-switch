// 覆盖普通错误、熔断与状态日志分类。
use super::{
    Action, Category, Change, CircuitEvidence, Filter, HealthScope, Reason, Record, Service,
};
use crate::gateway::ClientId;
use std::{
    fs,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const PROVIDER: &str = "00000000-0000-0000-0000-000000000001";

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn event(model: &str, reason: Reason, action: Action) -> Record {
    Record::new(
        Some(ClientId::Codex),
        Some(PROVIDER),
        Some(model),
        reason,
        action,
        Some(503),
        Some(1),
    )
}

fn at(mut record: Record, timestamp: u64) -> Record {
    record.first_at = timestamp;
    record.last_at = timestamp;
    record
}

async fn append(
    service: &Service,
    receiver: &mut tokio::sync::broadcast::Receiver<Change>,
    record: Record,
) -> Change {
    service.emit(record);
    let change = tokio::time::timeout(Duration::from_secs(3), receiver.recv())
        .await
        .expect("journal append timed out")
        .expect("journal subscription closed");
    assert!(change.record.is_some());
    change
}

#[tokio::test]
async fn subscription_notifies_for_circuit_open_and_handoff_but_not_ordinary_errors() {
    let temp = tempfile::tempdir().unwrap();
    let service = Service::new(temp.path());
    let mut receiver = service.subscribe();

    let ordinary = append(
        &service,
        &mut receiver,
        event("fixture-quiet-model", Reason::Network, Action::Returned),
    )
    .await;
    assert!(!ordinary.notify);

    let circuit = append(
        &service,
        &mut receiver,
        event(
            "fixture-circuit-model",
            Reason::CircuitOpen,
            Action::Stopped,
        ),
    )
    .await;
    assert!(circuit.notify);

    let handoff = append(
        &service,
        &mut receiver,
        event(
            "fixture-handoff-model",
            Reason::ThreadHandoff,
            Action::Routed,
        ),
    )
    .await;
    assert!(handoff.notify);
}

#[tokio::test]
async fn category_filter_classifies_causes_before_pagination_and_flattens_detail() {
    let temp = tempfile::tempdir().unwrap();
    let service = Service::new(temp.path());
    let mut receiver = service.subscribe();
    let base = now().saturating_sub(100);

    let mut cause = at(
        event(
            "fixture-circuit-model",
            Reason::UpstreamService,
            Action::Returned,
        ),
        base,
    );
    cause.details.counted_failure = Some(true);
    let cause_id = cause.id.clone();
    append(&service, &mut receiver, cause).await;

    let mut opened = at(
        event(
            "fixture-circuit-model",
            Reason::CircuitOpen,
            Action::Stopped,
        ),
        base + 1,
    );
    opened.details.health_scope = Some(HealthScope::Model);
    opened.details.cause_id = Some(cause_id.clone());
    opened.details.wait_seconds = Some(45);
    opened.details.circuit = Some(CircuitEvidence {
        cause_reason: Some(Reason::UpstreamService),
        retry_at: Some(base + 46),
        failures: 6,
        failure_threshold: 6,
        failed_requests: 6,
        requests: 6,
        error_rate: 0.5,
        min_requests: 10,
        trigger: "transient_failures".into(),
    });
    let opened_id = opened.id.clone();
    append(&service, &mut receiver, opened).await;

    append(
        &service,
        &mut receiver,
        at(
            event("fixture-failover-model", Reason::Failover, Action::Routed),
            base + 2,
        ),
    )
    .await;
    append(
        &service,
        &mut receiver,
        at(
            event(
                "fixture-handoff-model",
                Reason::ThreadHandoff,
                Action::Routed,
            ),
            base + 3,
        ),
    )
    .await;

    for index in 0..60 {
        append(
            &service,
            &mut receiver,
            at(
                event(
                    &format!("fixture-ordinary-{index}"),
                    Reason::Network,
                    Action::TryingNext,
                ),
                base + 10 + index,
            ),
        )
        .await;
    }

    let default_page = service.query(Filter {
        page: Some(1),
        ..Filter::default()
    });
    assert_eq!(default_page.total, 64);
    assert_eq!(default_page.items.len(), 50);
    assert!(default_page
        .items
        .iter()
        .all(|row| row.category != Category::CircuitError));

    let circuit_page = service.query(Filter {
        category: Some(Category::CircuitError),
        page: Some(1),
        ..Filter::default()
    });
    assert_eq!(circuit_page.total, 2);
    assert_eq!(circuit_page.items.len(), 2);
    assert!(circuit_page
        .items
        .iter()
        .any(|row| row.id == opened_id && row.category == Category::CircuitError));
    let classified_cause = service.detail(&cause_id).expect("cause detail");
    assert_eq!(classified_cause.reason, Reason::UpstreamService);
    assert_eq!(classified_cause.category, Category::CircuitError);

    let flattened = serde_json::to_value(&classified_cause).unwrap();
    assert_eq!(flattened["category"].as_str(), Some("circuit_error"));
    assert!(flattened.get("id").is_some());
    assert!(flattened.get("record").is_none());

    let statuses = service.query(Filter {
        category: Some(Category::Status),
        ..Filter::default()
    });
    assert_eq!(statuses.total, 2);
    assert!(statuses
        .items
        .iter()
        .all(|row| matches!(row.reason, Reason::Failover | Reason::ThreadHandoff)));

    let ordinary_page_two = service.query(Filter {
        category: Some(Category::OrdinaryError),
        page: Some(2),
        ..Filter::default()
    });
    assert_eq!(ordinary_page_two.total, 60);
    assert_eq!(ordinary_page_two.page, 2);
    assert_eq!(ordinary_page_two.items.len(), 10);
    assert!(ordinary_page_two
        .items
        .iter()
        .all(|row| row.category == Category::OrdinaryError));
}

fn legacy_line(record: &Record) -> Vec<u8> {
    let mut value = serde_json::to_value(record).unwrap();
    if let Some(circuit) = value
        .get_mut("details")
        .and_then(|details| details.get_mut("circuit"))
        .and_then(serde_json::Value::as_object_mut)
    {
        circuit.remove("causeReason");
        circuit.remove("retryAt");
    }
    let mut bytes = serde_json::to_vec(&value).unwrap();
    bytes.push(b'\n');
    bytes
}

#[tokio::test]
async fn legacy_jsonl_queries_classify_without_persisting_categories() {
    let temp = tempfile::tempdir().unwrap();
    let mut cause = event(
        "fixture-legacy-model",
        Reason::UpstreamService,
        Action::Returned,
    );
    cause.details.counted_failure = Some(true);
    let mut opened = event("fixture-legacy-model", Reason::CircuitOpen, Action::Stopped);
    opened.details.cause_id = Some(cause.id.clone());
    opened.details.circuit = Some(CircuitEvidence {
        cause_reason: None,
        retry_at: None,
        failures: 4,
        failure_threshold: 4,
        failed_requests: 4,
        requests: 4,
        error_rate: 0.5,
        min_requests: 10,
        trigger: "consecutive_failures".into(),
    });

    let path = temp.path().join("diagnostics.jsonl");
    let mut legacy = legacy_line(&cause);
    legacy.extend(legacy_line(&opened));
    fs::write(&path, &legacy).unwrap();
    let service = Service::new(temp.path());
    // Startup retention may normalize the index independently of a read query.
    // Drain that work before checking that classification itself never writes.
    let mut changes = service.subscribe();
    service.sender.send(super::Job::Prune).unwrap();
    tokio::time::timeout(Duration::from_secs(10), changes.recv())
        .await
        .unwrap()
        .unwrap();
    let before = fs::read(&path).unwrap();
    let saved: Vec<serde_json::Value> = before
        .split(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).unwrap())
        .collect();
    assert_eq!(saved.len(), 2);
    assert!(saved.iter().all(|record| record.get("category").is_none()));
    assert_eq!(saved[0]["id"], cause.id);
    assert_eq!(saved[1]["details"]["causeId"], cause.id);
    let circuit_rows = service.query(Filter {
        category: Some(Category::CircuitError),
        ..Filter::default()
    });
    assert_eq!(circuit_rows.total, 2);
    assert_eq!(
        service.detail(&cause.id).unwrap().category,
        Category::CircuitError
    );
    assert_eq!(
        service.detail(&opened.id).unwrap().category,
        Category::CircuitError
    );
    assert_eq!(fs::read(path).unwrap(), before);
}

#[test]
fn only_circuit_open_and_thread_handoff_are_notifiable() {
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
fn circuit_notification_uses_remaining_deadline_and_hides_untrusted_provider_text() {
    let mut record = event(
        "fixture-notification-model",
        Reason::CircuitOpen,
        Action::Stopped,
    );
    record.notification_provider =
        Some("Bearer fixture-secret /Users/fixture/private user@example.invalid".into());
    record.details.health_scope = Some(HealthScope::Provider);
    record.details.circuit = Some(CircuitEvidence {
        cause_reason: Some(Reason::UpstreamService),
        retry_at: Some(2_000),
        failures: 6,
        failure_threshold: 6,
        failed_requests: 6,
        requests: 6,
        error_rate: 0.5,
        min_requests: 10,
        trigger: "transient_failures".into(),
    });

    let thirty_seconds = record.notification_at(1_970);
    assert!(thirty_seconds.contains("等待 30 秒后可探测"));
    let twenty_seconds = record.notification_at(1_980);
    assert!(twenty_seconds.contains("等待 20 秒后可探测"));
    let due = record.notification_at(2_000);
    assert!(due.contains("待请求探测"));
    for sensitive in [
        "Bearer",
        "fixture-secret",
        "/Users/fixture",
        "private",
        "example.invalid",
    ] {
        assert!(!thirty_seconds.contains(sensitive));
    }

    let serialized = serde_json::to_value(&record).unwrap();
    assert!(serialized.get("notificationProvider").is_none());
}

#[tokio::test]
async fn origins_filter_before_counting_and_paging_without_rewriting_history() {
    use super::{ErrorBasis, Origin};
    let temp = tempfile::tempdir().unwrap();
    let service = Service::new(temp.path());
    let mut changes = service.subscribe();
    for index in 0..56 {
        append(
            &service,
            &mut changes,
            event(
                &format!("upstream-{index}"),
                Reason::UpstreamService,
                Action::Returned,
            ),
        )
        .await;
    }
    for reason in [Reason::Network, Reason::CircuitOpen] {
        let mut record = event("internal-cause", reason, Action::Returned);
        record.details.upstream_code = Some("overloaded_error".into());
        record.details.error_basis = Some(ErrorBasis::ServiceError);
        append(&service, &mut changes, record).await;
    }
    let mut unknown = event("legacy-unknown", Reason::ProtocolError, Action::Returned);
    unknown.status = None;
    append(&service, &mut changes, unknown).await;
    let mut business = event("business-failure", Reason::Capacity, Action::Waiting);
    business.status = Some(200);
    business.details.error_basis = Some(ErrorBasis::Capacity);
    let business_id = business.id.clone();
    append(&service, &mut changes, business).await;
    for (origin, total, page, count) in [
        (Origin::Upstream, 57, 1, 50),
        (Origin::Upstream, 57, 2, 7),
        (Origin::Gateway, 3, 1, 3),
    ] {
        let rows = service.query(Filter {
            origin: Some(origin),
            page: Some(page),
            ..Default::default()
        });
        assert_eq!(rows.total, total);
        assert_eq!(rows.items.len(), count);
        assert!(rows.items.iter().all(|r| r.origin == origin));
    }
    let detail = service.detail(&business_id).unwrap();
    assert_eq!(serde_json::to_value(detail).unwrap()["origin"], "upstream");
    let persisted = fs::read_to_string(temp.path().join("diagnostics.jsonl")).unwrap();
    assert!(persisted
        .lines()
        .all(|line| serde_json::from_str::<serde_json::Value>(line)
            .unwrap()
            .get("origin")
            .is_none()));
    let restarted = Service::new(temp.path());
    assert_eq!(
        restarted
            .query(Filter {
                origin: Some(Origin::Gateway),
                ..Default::default()
            })
            .total,
        3
    );
}
