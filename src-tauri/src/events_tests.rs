use crate::{
    events::{Action, Change, Filter, Reason, Record, Service},
    gateway::ClientId,
};
use std::{
    fs,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const PROVIDER: &str = "00000000-0000-0000-0000-000000000001";
const RETENTION: u64 = 7 * 86400;

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn record(model: &str, reason: Reason) -> Record {
    Record::new(
        Some(ClientId::Codex),
        Some(PROVIDER),
        Some(model),
        reason,
        Action::TryingNext,
        Some(503),
        Some(1),
    )
}

async fn next(rx: &mut tokio::sync::broadcast::Receiver<Change>) -> Change {
    tokio::time::timeout(Duration::from_secs(3), rx.recv())
        .await
        .unwrap()
        .unwrap()
}

async fn emit(service: &Service, rx: &mut tokio::sync::broadcast::Receiver<Change>, value: Record) {
    service.emit(value);
    let _ = next(rx).await;
}

#[tokio::test]
async fn records_merge_within_300_seconds_and_expose_subscribe_detail_and_clear() {
    let temp = tempfile::tempdir().unwrap();
    let service = Service::new(temp.path());
    let mut rx = service.subscribe();
    let first = record("fixture-model", Reason::UpstreamService);
    let id = first.id.clone();
    emit(&service, &mut rx, first.clone()).await;
    let mut merged = record("fixture-model", Reason::UpstreamService);
    merged.last_at = first.last_at + 299;
    emit(&service, &mut rx, merged).await;
    let page = service.query(Filter::default());
    assert_eq!(page.total, 1);
    assert_eq!(page.items[0].count, 2);
    assert_eq!(service.detail(&id).unwrap().count, 2);

    let mut separate = record("fixture-model", Reason::UpstreamService);
    separate.last_at = first.last_at + 600;
    emit(&service, &mut rx, separate).await;
    assert_eq!(service.query(Filter::default()).total, 2);

    service.clear().await.unwrap();
    assert_eq!(service.query(Filter::default()).total, 0);
    assert!(service.detail(&id).is_none());
    assert!(fs::read(temp.path().join("diagnostics.jsonl"))
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn sensitive_models_are_omitted_and_pages_are_filtered_and_bounded() {
    assert!(crate::events::safe_model("fixture/model-v1").is_some());
    for value in [
        "sk-secret-fixture",
        "Bearer secret-fixture",
        "eyJ.fixture.token",
        "https://example.invalid/secret",
        "/fixture/private",
        "users/fixture/private",
    ] {
        assert!(crate::events::safe_model(value).is_none(), "{value}");
    }
    let secret = Record::new(
        Some(ClientId::Codex),
        Some(PROVIDER),
        Some("sk-secret-fixture"),
        Reason::Authentication,
        Action::Stopped,
        Some(401),
        None,
    );
    assert!(secret.model.is_none());
    assert!(!secret.notification().contains("sk-secret"));

    let temp = tempfile::tempdir().unwrap();
    let service = Service::new(temp.path());
    let mut rx = service.subscribe();
    for i in 0..55 {
        emit(
            &service,
            &mut rx,
            record(
                &format!("fixture-model-{i}"),
                if i % 2 == 0 {
                    Reason::Network
                } else {
                    Reason::UpstreamService
                },
            ),
        )
        .await;
    }
    let page_one = service.query(Filter {
        client_id: Some(ClientId::Codex),
        page: Some(1),
        ..Filter::default()
    });
    assert_eq!(page_one.total, 55);
    assert_eq!(page_one.page, 1);
    assert_eq!(page_one.items.len(), 50);
    let page_two = service.query(Filter {
        reason: Some(Reason::Network),
        page: Some(99),
        ..Filter::default()
    });
    assert_eq!(page_two.total, 28);
    assert_eq!(page_two.page, 1);
    assert_eq!(page_two.items.len(), 28);

    let server_errors = service.query(Filter {
        status_group: Some(crate::events::StatusGroup::ServerError),
        ..Filter::default()
    });
    assert_eq!(server_errors.total, 55);
}

#[test]
fn seven_day_retention_and_damaged_tail_keep_valid_records() {
    let temp = tempfile::tempdir().unwrap();
    let mut old = record("old-model", Reason::Network);
    old.first_at = now().saturating_sub(RETENTION + 1);
    old.last_at = old.first_at;
    let fresh = record("fresh-model", Reason::Network);
    let mut bytes = serde_json::to_vec(&old).unwrap();
    bytes.push(b'\n');
    bytes.extend(serde_json::to_vec(&fresh).unwrap());
    bytes.extend_from_slice(b"\n{damaged-tail\n");
    fs::write(temp.path().join("diagnostics.jsonl"), bytes).unwrap();
    let service = Service::new(temp.path());
    let page = service.query(Filter::default());
    assert_eq!(page.total, 1);
    assert_eq!(page.items[0].model.as_deref(), Some("fresh-model"));
    assert!(page
        .error
        .as_deref()
        .is_some_and(|message| message.contains("损坏记录")));
}

#[test]
fn files_over_5_mib_are_not_loaded_and_report_a_read_error() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("diagnostics.jsonl");
    fs::write(path, vec![b'x'; 5 * 1024 * 1024 + 1]).unwrap();
    let service = Service::new(temp.path());
    let page = service.query(Filter::default());
    assert_eq!(page.total, 0);
    assert!(page
        .error
        .as_deref()
        .is_some_and(|message| message.contains("读取失败")));
}

#[tokio::test]
async fn write_failures_are_visible_through_subscription_and_query() {
    let temp = tempfile::tempdir().unwrap();
    let data_file = temp.path().join("data-file");
    fs::write(&data_file, b"fixture").unwrap();
    let service = Service::new(&data_file);
    let mut rx = service.subscribe();
    service.emit(record("fixture-model", Reason::Network));
    let change = next(&mut rx).await;
    assert!(change
        .error
        .as_deref()
        .is_some_and(|message| message.contains("写入失败")));
    assert!(service
        .query(Filter::default())
        .error
        .as_deref()
        .is_some_and(|message| message.contains("写入失败")));
}

#[test]
fn serialization_uses_the_public_camel_case_contract() {
    let value = serde_json::to_value(record("fixture-model", Reason::Network)).unwrap();
    assert!(value.get("firstAt").is_some());
    assert!(value.get("lastAt").is_some());
    assert!(value.get("clientId").is_some());
    assert!(value.get("providerId").is_some());
    assert!(value.get("notifiedAt").is_some());
    assert_eq!(
        value.get("errorCode").and_then(|v| v.as_str()),
        Some("NETWORK_ERROR")
    );
    assert!(value.get("first_at").is_none());
}

#[test]
fn record_ids_are_unique_for_distinct_events() {
    static SEED: AtomicU64 = AtomicU64::new(0);
    let a = record(
        &format!("fixture-{}", SEED.fetch_add(1, Ordering::Relaxed)),
        Reason::Network,
    );
    let b = record(
        &format!("fixture-{}", SEED.fetch_add(1, Ordering::Relaxed)),
        Reason::Network,
    );
    assert_ne!(a.id, b.id);
}
