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
        record.details.raw = Some(Arc::new(crate::event_raw::Raw::bytes(
            b"fixture complete error",
            vec![],
            None,
        )));
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
    for id in &ids {
        assert_eq!(
            service.raw(id).unwrap().unwrap().body,
            "fixture complete error"
        );
    }
    std::fs::remove_dir(&path).unwrap();
    service.clear().await.unwrap();
    assert_eq!(service.query(Filter::default()).total, 0);
    assert_eq!(
        std::fs::read_dir(dir.path().join("diagnostic-errors"))
            .unwrap()
            .count(),
        0
    );
}

#[tokio::test]
async fn raw_write_failure_is_visible_and_restart_prunes_expired_attachments() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("diagnostic-errors");
    std::fs::write(&path, b"fixture blocker").unwrap();
    let service = Service::new(dir.path());
    let mut changes = service.subscribe();
    let mut record = Record::new(
        Some(ClientId::Codex),
        Some(PROVIDER),
        Some(MODEL),
        Reason::Capacity,
        Action::Waiting,
        Some(200),
        Some(1),
    );
    record.details.raw = Some(Arc::new(crate::event_raw::Raw::bytes(
        b"fixture error",
        vec![],
        None,
    )));
    let id = record.id.clone();
    service.emit(record.clone());
    let change = changes.recv().await.unwrap();
    assert!(change.error.unwrap().contains("原文保存失败"));
    assert!(service
        .detail(&id)
        .unwrap()
        .details
        .raw_error
        .as_ref()
        .unwrap()
        .incomplete
        .is_some());
    assert!(service.raw(&id).is_err());
    drop(service);

    std::fs::remove_file(&path).unwrap();
    let raw = record.details.raw.take().unwrap();
    raw.save(&path, &id).unwrap();
    record.details.raw_error = Some(raw.reference(&id));
    record.last_at = super::now() - super::RETENTION - 1;
    record.first_at = record.last_at;
    std::fs::write(
        dir.path().join("diagnostics.jsonl"),
        serde_json::to_vec(&record).unwrap(),
    )
    .unwrap();
    std::fs::write(path.join("unrelated.txt"), b"fixture").unwrap();
    let service = Service::new(dir.path());
    let mut changes = service.subscribe();
    // A queued write observes completion of startup retention without sleeps.
    service.emit(Record::new(
        None,
        None,
        None,
        Reason::Network,
        Action::Returned,
        None,
        None,
    ));
    changes.recv().await.unwrap();
    assert!(!path.join(format!("{id}.error")).exists());
    assert!(path.join("unrelated.txt").exists());
    assert!(service.detail(&id).is_none());
}

#[test]
fn emit_checked_returns_false_and_reports_a_full_bounded_queue() {
    let (sender, _receiver) = mpsc::sync_channel(1);
    let (events, mut changes) = broadcast::channel(8);
    let service = Service {
        sender,
        journal: Arc::new(Mutex::new(Journal::default())),
        events,
        raw_directory: Arc::new(std::env::temp_dir().join("unused-event-fixture")),
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
