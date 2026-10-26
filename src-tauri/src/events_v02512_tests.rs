use super::{Action, Filter, Journal, Reason, Record, Service};
use crate::gateway::ClientId;
use std::sync::{mpsc, Arc, Mutex};
use tokio::sync::broadcast;

const PROVIDER: &str = "00000000-0000-0000-0000-000000000001";
const MODEL: &str = "fixture-model";
const QUEUE_FULL: &str = "日志队列已满，部分事件未记录";

#[tokio::test]
async fn clear_write_failure_restores_rows_aliases_and_stable_page_order() {
    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(dir.path());
    let mut events = service.subscribe();
    for n in 0..55 {
        let mut record = Record::new(
            Some(ClientId::Codex),
            Some(PROVIDER),
            Some(&format!("fixture-{n}")),
            Reason::ProtocolError,
            Action::Returned,
            Some(502),
            Some(1),
        );
        record.first_at = super::now();
        record.last_at = record.first_at;
        service.emit(record);
        events.recv().await.unwrap();
    }
    let before = service.query(Filter::default());
    assert_eq!(before.total, 55);
    let ids: Vec<_> = before.items.iter().map(|r| r.id.clone()).collect();
    let path = dir.path().join("diagnostics.jsonl");
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    assert!(service.clear().await.is_err());
    let restored = service.query(Filter::default());
    assert_eq!(restored.total, 55);
    assert_eq!(
        restored
            .items
            .iter()
            .map(|r| r.id.clone())
            .collect::<Vec<_>>(),
        ids
    );
    assert!(restored.error.is_some());
    std::fs::remove_dir(&path).unwrap();
    service.clear().await.unwrap();
    assert_eq!(service.query(Filter::default()).total, 0);
}

#[test]
fn emit_checked_returns_false_and_reports_a_full_bounded_queue() {
    let (sender, _receiver) = mpsc::sync_channel(1);
    let (events, mut changes) = broadcast::channel(8);
    let service = Service {
        sender,
        journal: Arc::new(Mutex::new(Journal::default())),
        events,
    };

    let accepted = Record::new(
        Some(ClientId::Codex),
        Some(PROVIDER),
        Some(MODEL),
        Reason::ProtocolError,
        Action::Returned,
        Some(502),
        Some(1),
    );
    assert!(service.emit_checked(accepted));

    let rejected = Record::new(
        Some(ClientId::Codex),
        Some(PROVIDER),
        Some(MODEL),
        Reason::ProtocolError,
        Action::Returned,
        Some(502),
        Some(2),
    );
    assert!(!service.emit_checked(rejected));

    let change = changes
        .try_recv()
        .expect("queue rejection should immediately publish a diagnostic error");
    assert!(change.record.is_none());
    assert_eq!(change.error.as_deref(), Some(QUEUE_FULL));
    assert!(!change.notify);

    let page = service.query(Filter::default());
    assert_eq!(page.error.as_deref(), Some(QUEUE_FULL));
    assert_eq!(
        page.total, 0,
        "the worker was intentionally held off the queue"
    );
}
