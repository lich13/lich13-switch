//! v0.25.10 downstream delivery classification boundaries.
//!
//! These fixtures stay at the protocol/diagnostics boundary.  They use only
//! loopback-free synthetic payloads and preserve the distinction between an
//! upstream terminal event and bytes handed to the downstream transport.
use super::{
    protocol::{Protocol, Terminal},
    wire_progress::Activity,
    ClientId, Gateway,
};
use crate::events::{
    Action, Category, Change, DeliveryEvidence, Filter, Level, Reason, Record as EventRecord,
    Service as EventService,
};
use crate::usage::{
    model::{EndCause, Filter as UsageFilter, Record as UsageRecord},
    Service as UsageService,
};
use std::{io::ErrorKind, sync::atomic::Ordering, time::Duration};

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
    // This waits for a background filesystem commit, not a transport deadline.
    // Windows ACL/fsync work may be delayed by parallel synthetic DB tests.
    let change = tokio::time::timeout(Duration::from_secs(30), receiver.recv())
        .await
        .expect("diagnostic persistence worker did not publish the record")
        .expect("diagnostic event channel closed");
    assert!(change.error.is_none(), "diagnostic persistence failed");
    assert!(!change.notify, "request interruptions must never notify");
    change.record.unwrap()
}

fn category_for(events: &EventService, id: &str) -> Category {
    events.detail(id).unwrap().category
}

async fn next_usage_record(
    service: &UsageService,
    receiver: &mut tokio::sync::broadcast::Receiver<()>,
) -> UsageRecord {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let page = service
                .query(|store| store.logs(&UsageFilter::default()))
                .expect("usage database query should succeed");
            if let Some(record) = page.rows.into_iter().next() {
                assert_eq!(page.total, 1);
                assert!(service.state().error.is_none());
                return record;
            }
            receiver
                .recv()
                .await
                .expect("usage writer notification channel closed");
        }
    })
    .await
    .expect("synthetic usage record did not settle")
}

#[tokio::test]
async fn explicit_downstream_ends_keep_delivery_bytes_without_new_diagnostics() {
    let fixture = Fixture::new();
    let mut receiver = fixture.events.subscribe();

    let cases = [
        ("DOWNSTREAM_CLOSED", "closed", 5),
        ("DOWNSTREAM_PEER_RESET", "reset", 6),
        ("DOWNSTREAM_WRITE_FAILED", "broken_pipe", 7),
        ("DOWNSTREAM_CANCELLED", "transport_cancel", 4),
    ];
    for (expected_code, kind, expected_error) in cases {
        let mut protocol = fixture.protocol();
        protocol.delivery().handed(11, false);
        let delivery = protocol.delivery();
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
        assert_eq!(delivery.bytes.load(Ordering::Acquire), 11, "{kind}");
        assert_eq!(
            delivery.error.load(Ordering::Acquire),
            expected_error,
            "{kind}"
        );
        assert!(!delivery.terminal.load(Ordering::Acquire), "{kind}");
        let actual_code = match delivery.error.load(Ordering::Acquire) {
            4 => "DOWNSTREAM_CANCELLED",
            5 => "DOWNSTREAM_CLOSED",
            6 => "DOWNSTREAM_PEER_RESET",
            7 => "DOWNSTREAM_WRITE_FAILED",
            other => panic!("unexpected downstream code {other} for {kind}"),
        };
        assert_eq!(actual_code, expected_code, "{kind}");
    }

    super::diagnostic_test_support::assert_no_interruptions_after_barrier(
        &fixture.events,
        &mut receiver,
        &fixture._dir.path().join("events"),
    )
    .await;
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
async fn observed_terminal_without_handoff_keeps_usage_evidence_without_new_diagnostic() {
    let fixture = Fixture::new();
    let mut receiver = fixture.events.subscribe();
    let usage = UsageService::new(&fixture._dir.path().join("usage"));
    let mut usage_receiver = usage.subscribe();
    let terminal =
        b"data: {\"type\":\"response.completed\",\"response\":{\"id\":\"fixture-response\",\"model\":\"fixture-model\",\"status\":\"completed\",\"usage\":{\"input_tokens\":42,\"output_tokens\":17}}}\n\n";

    let trace = usage.begin("codex", Some("fixture-model"));
    let mut protocol = fixture.protocol();
    protocol.attach_usage(trace.attempt(PROVIDER_ID, true, "http"));
    protocol.feed(terminal);
    assert_eq!(protocol.terminal(), Some(Terminal::Success));
    assert!(protocol.succeeded());
    let delivery = protocol.delivery();
    delivery.handed(17, false);
    let activity = Activity::default();
    activity.failed(
        &std::io::Error::new(ErrorKind::BrokenPipe, "fixture broken pipe"),
        true,
    );
    protocol.downstream_closed(&activity, true);
    assert_eq!(delivery.error.load(Ordering::Acquire), 7);
    drop(protocol);
    drop(trace);

    assert_eq!(delivery.bytes.load(Ordering::Acquire), 17);
    assert!(!delivery.terminal.load(Ordering::Acquire));
    let usage_record = next_usage_record(&usage, &mut usage_receiver).await;
    let attempt = usage_record.final_attempt().expect("one fixture attempt");
    assert_eq!(
        attempt.end.as_ref().map(|end| end.cause),
        Some(EndCause::Completed)
    );
    assert!(attempt.end.as_ref().unwrap().terminal_received);
    let transfer = attempt.transfer.as_ref().expect("usage transfer evidence");
    assert_eq!(transfer.upstream_bytes, terminal.len() as u64);
    assert_eq!(transfer.downstream_bytes, 17);
    assert!(!transfer.terminal_handed_off);
    assert_eq!(
        transfer.downstream_error.as_deref(),
        Some("DOWNSTREAM_WRITE_FAILED")
    );
    super::diagnostic_test_support::assert_no_interruptions_after_barrier(
        &fixture.events,
        &mut receiver,
        &fixture._dir.path().join("events"),
    )
    .await;
}

#[tokio::test]
async fn direct_event_service_still_merges_historical_delivery_evidence() {
    let fixture = Fixture::new();
    let mut receiver = fixture.events.subscribe();

    for (code, handed) in [
        ("DOWNSTREAM_CLOSED", 3),
        ("DOWNSTREAM_CLOSED", 7),
        ("DOWNSTREAM_PEER_RESET", 11),
    ] {
        let mut record = EventRecord::new(
            Some(ClientId::Codex),
            Some(PROVIDER_ID),
            Some("fixture-model"),
            Reason::RequestInterrupted,
            Action::NotRetried,
            Some(200),
            None,
        );
        record.level = Level::Info;
        record.details.local_code = Some(code.into());
        record.details.counted_failure = Some(false);
        record.details.delivery = Some(DeliveryEvidence {
            duration_ms: 10,
            upstream_bytes: 20,
            handed_bytes: handed,
            terminal_received: false,
            terminal_handed_off: false,
            request_hash: None,
            response_hash: None,
        });
        fixture.events.emit(record);
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
        .expect("merged historical close record");
    assert_eq!(closed.count, 2);
    assert_eq!(closed.details.delivery.as_ref().unwrap().handed_bytes, 7);
    let reset = page
        .items
        .iter()
        .find(|item| item.details.local_code.as_deref() == Some("DOWNSTREAM_PEER_RESET"))
        .expect("distinct historical reset record");
    assert_eq!(reset.count, 1);
    assert_eq!(reset.details.delivery.as_ref().unwrap().handed_bytes, 11);
}
