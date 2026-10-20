use crate::events::{Action, Change, Filter, Reason, Record, Service as EventService};
use crate::gateway::ClientId;
use std::{path::Path, time::Duration};
use tokio::sync::broadcast;

const SENTINEL_PROVIDER: &str = "00000000-0000-0000-0000-000000000001";
const SENTINEL_MODEL: &str = "fixture-diagnostic-barrier";

/// Drain the diagnostics worker through an event emitted after the request has
/// settled, then prove no request interruption reached memory or disk first.
///
/// A broadcast receive is the barrier: the worker emits each change only after
/// persisting its preceding job, so receiving the sentinel orders both the
/// in-memory journal and its JSONL file after the request's asynchronous saver.
pub(super) async fn assert_no_interruptions_after_barrier(
    service: &EventService,
    receiver: &mut broadcast::Receiver<Change>,
    events_dir: &Path,
) {
    let sentinel = Record::new(
        Some(ClientId::Codex),
        Some(SENTINEL_PROVIDER),
        Some(SENTINEL_MODEL),
        Reason::Network,
        Action::Returned,
        None,
        None,
    );
    let sentinel_id = sentinel.id.clone();
    let mut sentinel = sentinel;
    // Keep repeated barriers distinct from the previous sentinel so the
    // journal worker cannot merge them into an older row and rebroadcast its
    // old ID.
    sentinel.details.message = Some(format!("diagnostic-test-sentinel-{sentinel_id}"));
    service.emit(sentinel);

    loop {
        let change = tokio::time::timeout(Duration::from_secs(30), receiver.recv())
            .await
            .expect("diagnostic sentinel barrier timed out")
            .expect("diagnostic event channel closed before sentinel barrier");
        assert!(change.error.is_none(), "diagnostic persistence failed");
        assert!(!change.notify, "synthetic diagnostics must never notify");
        if let Some(record) = change.record {
            assert_ne!(
                record.reason,
                Reason::RequestInterrupted,
                "unexpected request interruption before diagnostic sentinel"
            );
            if record.id == sentinel_id {
                break;
            }
        }
    }

    let page = service.query(Filter {
        reason: Some(Reason::RequestInterrupted),
        ..Filter::default()
    });
    assert_eq!(page.error, None, "diagnostic query failed at barrier");
    assert_eq!(page.total, 0, "request interruption reached memory");

    let path = events_dir.join("diagnostics.jsonl");
    let persisted = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("diagnostic sentinel file could not be read: {error}"));
    for line in persisted.lines().filter(|line| !line.is_empty()) {
        let value: serde_json::Value = serde_json::from_str(line)
            .unwrap_or_else(|error| panic!("diagnostic JSONL is invalid: {error}"));
        assert_ne!(
            value.get("reason").and_then(serde_json::Value::as_str),
            Some("request_interrupted"),
            "request interruption reached temporary persistence"
        );
    }
}
