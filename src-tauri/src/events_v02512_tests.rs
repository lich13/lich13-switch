use super::{Action, Filter, Journal, Reason, Record, Service};
use crate::gateway::ClientId;
use std::sync::{mpsc, Arc, Mutex};
use tokio::sync::broadcast;

const PROVIDER: &str = "00000000-0000-0000-0000-000000000001";
const MODEL: &str = "fixture-model";
const QUEUE_FULL: &str = "日志队列已满，部分事件未记录";

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
