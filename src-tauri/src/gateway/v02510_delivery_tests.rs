//! v0.25.10 downstream delivery classification boundaries.
//!
//! These fixtures stay at the protocol/diagnostics boundary.  They use only
//! loopback-free synthetic payloads and preserve the distinction between an
//! upstream terminal event and bytes handed to the downstream transport.
use super::{
    protocol::{Protocol, Terminal},
    wire_progress::Activity,
    Gateway,
};
use crate::events::{
    Category, Change, Filter, Level, Reason, Record as EventRecord, Service as EventService,
};
use std::{io::ErrorKind, time::Duration};

const PROVIDER_ID: &str = "00000000-0000-0000-0000-000000000001";

struct Fixture {
    events: EventService,
    gateway: Gateway,
    _dir: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let events = EventService::new(&dir.path().join("events"));
        let gateway = Gateway::new(dir.path().join("gateway")).unwrap();
        gateway.set_diagnostics(events.clone());
        Self {
            events,
            gateway,
            _dir: dir,
        }
    }

    fn protocol(&self) -> Protocol {
        let mut protocol = Protocol::new(true);
        protocol.diagnostics(&self.gateway, PROVIDER_ID, Some("fixture-model"));
        protocol.response(200, true, "identity");
        protocol
    }
}

async fn next_record(receiver: &mut tokio::sync::broadcast::Receiver<Change>) -> EventRecord {
    let change = tokio::time::timeout(Duration::from_secs(3), receiver.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(!change.notify, "request interruptions must never notify");
    change.record.unwrap()
}

fn category_for(events: &EventService, id: &str) -> Category {
    events.detail(id).unwrap().category
}

fn assert_status_record(fixture: &Fixture, record: &EventRecord, local_code: &str) {
    assert_eq!(record.reason, Reason::RequestInterrupted);
    assert_eq!(record.level, Level::Info, "{local_code}");
    assert_eq!(category_for(&fixture.events, &record.id), Category::Status);
    assert_eq!(record.status, Some(200));
    assert_eq!(record.details.local_code.as_deref(), Some(local_code));
    assert_eq!(record.details.counted_failure, Some(false));
    let message = record.details.message.as_deref().unwrap_or_default();
    assert!(!message.contains("客户端已取消"), "{message}");
    assert!(
        !message.is_empty(),
        "{local_code} should keep a safe message"
    );
}

#[tokio::test]
async fn expected_downstream_ends_are_info_statuses_without_claiming_active_cancel() {
    let fixture = Fixture::new();
    let mut receiver = fixture.events.subscribe();

    let cases = [
        ("DOWNSTREAM_CLOSED", "closed"),
        ("DOWNSTREAM_PEER_RESET", "reset"),
        ("DOWNSTREAM_WRITE_FAILED", "broken_pipe"),
        ("DOWNSTREAM_CANCELLED", "transport_cancel"),
    ];
    for (local_code, kind) in cases {
        let mut protocol = fixture.protocol();
        protocol.delivery().handed(11, false);
        match kind {
            "closed" => protocol.downstream_closed(&Activity::default(), false),
            "reset" => {
                let activity = Activity::default();
                activity.failed(
                    &std::io::Error::new(ErrorKind::ConnectionReset, "fixture reset"),
                    false,
                );
                protocol.downstream_closed(&activity, false);
            }
            "broken_pipe" => {
                let activity = Activity::default();
                activity.failed(
                    &std::io::Error::new(ErrorKind::BrokenPipe, "fixture broken pipe"),
                    true,
                );
                protocol.downstream_closed(&activity, true);
            }
            "transport_cancel" => protocol.delivery().failed(4),
            _ => unreachable!(),
        }
        drop(protocol);

        let record = next_record(&mut receiver).await;
        assert_status_record(&fixture, &record, local_code);
        let evidence = record.details.delivery.expect("delivery evidence");
        assert_eq!(evidence.handed_bytes, 11, "{local_code}");
        assert!(!evidence.terminal_received, "{local_code}");
        assert!(!evidence.terminal_handed_off, "{local_code}");
    }
}

#[tokio::test]
async fn timeout_protocol_and_unknown_io_remain_warning_records() {
    let fixture = Fixture::new();
    let mut receiver = fixture.events.subscribe();

    let cases = [
        ("DOWNSTREAM_WRITE_TIMEOUT", "timeout"),
        ("DOWNSTREAM_PROTOCOL_ERROR", "protocol"),
        ("DOWNSTREAM_CONNECTION_ERROR", "unknown_io"),
    ];
    for (local_code, kind) in cases {
        let mut protocol = fixture.protocol();
        match kind {
            "timeout" => {
                protocol.delivery().failed(3);
                protocol.timeout("downstream_write", 2);
            }
            "protocol" => protocol.delivery().failed(8),
            // `Activity::default()` has no concrete socket error.  The
            // writing branch must remain an unknown I/O warning (code 2),
            // never infer BrokenPipe from the fact that a write was pending.
            "unknown_io" => protocol.downstream_closed(&Activity::default(), true),
            _ => unreachable!(),
        }
        drop(protocol);

        let record = next_record(&mut receiver).await;
        assert_eq!(record.reason, Reason::RequestInterrupted);
        assert_eq!(record.level, Level::Warning, "{local_code}");
        assert_eq!(
            category_for(&fixture.events, &record.id),
            Category::OrdinaryError
        );
        assert_eq!(record.status, Some(200));
        assert_eq!(record.details.local_code.as_deref(), Some(local_code));
        assert_eq!(record.details.counted_failure, Some(false));
    }
}

#[tokio::test]
async fn observed_terminal_without_handoff_keeps_distinct_delivery_reason_and_evidence() {
    let fixture = Fixture::new();
    let mut receiver = fixture.events.subscribe();
    let terminal =
        b"data: {\"type\":\"response.completed\",\"response\":{\"id\":\"fixture-response\"}}\n\n";

    let mut protocol = fixture.protocol();
    protocol.feed(terminal);
    assert_eq!(protocol.terminal(), Some(Terminal::Success));
    assert!(protocol.succeeded());
    protocol.delivery().handed(17, false);
    let activity = Activity::default();
    activity.failed(
        &std::io::Error::new(ErrorKind::BrokenPipe, "fixture broken pipe"),
        true,
    );
    protocol.downstream_closed(&activity, true);
    drop(protocol);

    let record = next_record(&mut receiver).await;
    assert_status_record(&fixture, &record, "DOWNSTREAM_WRITE_FAILED");
    assert_eq!(
        record.details.message.as_deref(),
        Some("下游管道已关闭，未确认客户端主动取消")
    );
    let evidence = record.details.delivery.expect("delivery evidence");
    assert_eq!(evidence.upstream_bytes, terminal.len() as u64);
    assert_eq!(evidence.handed_bytes, 17);
    assert!(evidence.terminal_received);
    assert!(!evidence.terminal_handed_off);
}

#[tokio::test]
async fn interruption_records_merge_only_same_delivery_code_and_keep_latest_evidence() {
    let fixture = Fixture::new();
    let mut receiver = fixture.events.subscribe();

    for (code, handed) in [
        ("DOWNSTREAM_CLOSED", 3),
        ("DOWNSTREAM_CLOSED", 7),
        ("DOWNSTREAM_PEER_RESET", 11),
    ] {
        let mut protocol = fixture.protocol();
        protocol.delivery().handed(handed, false);
        if code == "DOWNSTREAM_CLOSED" {
            protocol.downstream_closed(&Activity::default(), false);
        } else {
            let activity = Activity::default();
            activity.failed(
                &std::io::Error::new(ErrorKind::ConnectionReset, "fixture reset"),
                false,
            );
            protocol.downstream_closed(&activity, false);
        }
        drop(protocol);
        let _ = next_record(&mut receiver).await;
    }

    let page = fixture.events.query(Filter {
        reason: Some(Reason::RequestInterrupted),
        ..Filter::default()
    });
    assert_eq!(page.total, 2);
    let closed = page
        .items
        .iter()
        .find(|item| item.details.local_code.as_deref() == Some("DOWNSTREAM_CLOSED"))
        .expect("merged closed record");
    assert_eq!(closed.count, 2);
    assert_eq!(closed.details.delivery.as_ref().unwrap().handed_bytes, 7);
}
