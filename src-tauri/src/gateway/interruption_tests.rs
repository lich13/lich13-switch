use super::{
    protocol::{Delivery, Protocol, Terminal},
    wire_progress::Activity,
    Gateway,
};
use crate::{
    events::{
        Action, Category, Change, Filter, Level, Reason, Record as EventRecord,
        Service as EventService,
    },
    gateway::ClientId,
};
use std::{path::PathBuf, sync::atomic::Ordering, time::Duration};

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
        protocol
    }

    fn events_dir(&self) -> PathBuf {
        self._dir.path().join("events")
    }
}

async fn next_change(receiver: &mut tokio::sync::broadcast::Receiver<Change>) -> Change {
    tokio::time::timeout(Duration::from_secs(3), receiver.recv())
        .await
        .unwrap()
        .unwrap()
}

async fn next_record(receiver: &mut tokio::sync::broadcast::Receiver<Change>) -> EventRecord {
    let change = next_change(receiver).await;
    assert!(!change.notify, "request interruptions must never notify");
    change.record.unwrap()
}

fn category_for(events: &EventService, id: &str) -> Category {
    events.detail(id).unwrap().category
}

#[tokio::test]
async fn partial_response_close_keeps_delivery_progress_without_new_diagnostic() {
    let fixture = Fixture::new();
    let mut receiver = fixture.events.subscribe();
    let mut protocol = fixture.protocol();
    protocol.response(200, true, "identity");
    // The payload announces a terminal, but the event has not ended and is not observed.
    protocol.feed(b"data: {\"type\":\"response.completed\"");
    let delivery = protocol.delivery();
    delivery.handed(9, false);
    protocol.downstream_closed(&Activity::default(), false);
    drop(protocol);
    assert_eq!(delivery.bytes.load(Ordering::Acquire), 9);
    assert_eq!(delivery.error.load(Ordering::Acquire), 5);
    super::diagnostic_test_support::assert_no_interruptions_after_barrier(
        &fixture.events,
        &mut receiver,
        &fixture.events_dir(),
    )
    .await;
}

#[tokio::test]
async fn explicit_cancel_and_gateway_stop_are_statuses_without_fake_progress() {
    let fixture = Fixture::new();
    let mut receiver = fixture.events.subscribe();

    let mut cancelled = fixture.protocol();
    cancelled.response(200, true, "identity");
    cancelled.finish(Some(200), "CANCELLED");
    drop(cancelled);

    let cancellation = next_record(&mut receiver).await;
    assert_eq!(
        cancellation.details.local_code.as_deref(),
        Some("CANCELLED")
    );
    assert_eq!(
        cancellation.details.message.as_deref(),
        Some("收到明确取消指令，已结束转发")
    );
    assert_eq!(cancellation.level, Level::Info);
    assert!(matches!(
        category_for(&fixture.events, &cancellation.id),
        Category::Status
    ));
    assert!(cancellation.details.last_progress_ms.is_none());
    let evidence = cancellation.details.delivery.unwrap();
    assert_eq!(evidence.upstream_bytes, 0);
    assert!(!evidence.terminal_received);
    assert!(!evidence.terminal_handed_off);

    let mut stopped = fixture.protocol();
    stopped.response(200, true, "identity");
    let (_stop_sender, stop_receiver) = tokio::sync::watch::channel(true);
    stopped.watch_lifecycle(Some(stop_receiver), None);
    drop(stopped);

    let gateway_stop = next_record(&mut receiver).await;
    assert_eq!(
        gateway_stop.details.local_code.as_deref(),
        Some("GATEWAY_STOPPED")
    );
    assert_eq!(
        gateway_stop.details.message.as_deref(),
        Some("网关停止，已结束转发")
    );
    assert_eq!(gateway_stop.level, Level::Info);
    assert!(matches!(
        category_for(&fixture.events, &gateway_stop.id),
        Category::Status
    ));
    assert!(gateway_stop.details.last_progress_ms.is_none());
}

#[tokio::test]
async fn reset_and_broken_pipes_keep_delivery_without_new_diagnostics() {
    let fixture = Fixture::new();
    let mut receiver = fixture.events.subscribe();

    let mut reset = fixture.protocol();
    reset.response(200, true, "identity");
    let reset_delivery = reset.delivery();
    let reset_activity = Activity::default();
    reset_activity.failed(
        &std::io::Error::new(std::io::ErrorKind::ConnectionReset, "fixture reset"),
        false,
    );
    reset.downstream_closed(&reset_activity, false);
    drop(reset);
    assert_eq!(reset_delivery.error.load(Ordering::Acquire), 6);

    let mut failed_write = fixture.protocol();
    failed_write.response(200, true, "identity");
    let failed_write_delivery = failed_write.delivery();
    let write_activity = Activity::default();
    write_activity.failed(
        &std::io::Error::new(std::io::ErrorKind::BrokenPipe, "fixture write failure"),
        true,
    );
    failed_write.downstream_closed(&write_activity, true);
    drop(failed_write);
    assert_eq!(failed_write_delivery.error.load(Ordering::Acquire), 7);
    super::diagnostic_test_support::assert_no_interruptions_after_barrier(
        &fixture.events,
        &mut receiver,
        &fixture.events_dir(),
    )
    .await;
}

#[tokio::test]
async fn observed_upstream_success_remains_distinct_without_new_diagnostic() {
    let fixture = Fixture::new();
    let mut receiver = fixture.events.subscribe();
    let mut protocol = fixture.protocol();
    protocol.response(200, true, "identity");
    protocol.feed(
        b"data: {\"type\":\"response.completed\",\"response\":{\"id\":\"fixture-response\"}}\n\n",
    );
    assert_eq!(protocol.terminal(), Some(Terminal::Success));
    assert!(protocol.succeeded());

    let delivery: std::sync::Arc<Delivery> = protocol.delivery();
    delivery.handed(17, false);
    let write_activity = Activity::default();
    write_activity.failed(
        &std::io::Error::new(std::io::ErrorKind::BrokenPipe, "fixture write failure"),
        true,
    );
    protocol.downstream_closed(&write_activity, true);
    assert!(protocol.succeeded());
    assert_eq!(delivery.error.load(Ordering::Acquire), 7);
    drop(protocol);

    assert_eq!(delivery.bytes.load(Ordering::Relaxed), 17);
    assert!(!delivery.terminal.load(Ordering::Relaxed));
    super::diagnostic_test_support::assert_no_interruptions_after_barrier(
        &fixture.events,
        &mut receiver,
        &fixture.events_dir(),
    )
    .await;
}

#[tokio::test]
async fn diagnostics_without_subscribers_do_not_block_delivery_or_expected_close_silence() {
    let fixture = Fixture::new();
    // Intentionally do not subscribe to the diagnostics broadcast channel.
    let mut protocol = fixture.protocol();
    protocol.response(200, true, "identity");
    protocol.feed(b"data: {\"type\":\"response.output_text.delta\"");
    let delivery = protocol.delivery();
    delivery.handed(11, false);
    let write_activity = Activity::default();
    write_activity.failed(
        &std::io::Error::new(std::io::ErrorKind::BrokenPipe, "fixture write failure"),
        true,
    );
    protocol.downstream_closed(&write_activity, true);
    drop(protocol);

    let mut receiver = fixture.events.subscribe();
    super::diagnostic_test_support::assert_no_interruptions_after_barrier(
        &fixture.events,
        &mut receiver,
        &fixture.events_dir(),
    )
    .await;
    assert_eq!(delivery.bytes.load(Ordering::Relaxed), 11);
    assert_eq!(delivery.error.load(Ordering::Relaxed), 7);
}

#[tokio::test]
async fn request_interruption_is_never_notifiable_and_only_same_codes_merge_for_60_seconds() {
    for action in [
        Action::TryingNext,
        Action::Returned,
        Action::Waiting,
        Action::Stopped,
        Action::Recovered,
        Action::Routed,
        Action::Reconnecting,
        Action::RetryingSame,
        Action::NotRetried,
    ] {
        assert!(!Reason::RequestInterrupted.notifiable(action));
    }

    let dir = tempfile::tempdir().unwrap();
    let events = EventService::new(dir.path());
    let mut receiver = events.subscribe();
    let first_at = crate::events::Record::new(
        Some(ClientId::Codex),
        Some(PROVIDER_ID),
        Some("fixture-model"),
        Reason::RequestInterrupted,
        Action::NotRetried,
        Some(200),
        None,
    )
    .first_at;

    for (code, at) in [
        ("DOWNSTREAM_PEER_RESET", first_at),
        ("DOWNSTREAM_PEER_RESET", first_at + 59),
        ("DOWNSTREAM_WRITE_FAILED", first_at + 30),
        ("DOWNSTREAM_PEER_RESET", first_at + 60),
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
        record.level = Level::Warning;
        record.first_at = at;
        record.last_at = at;
        record.details.local_code = Some(code.into());
        record.details.counted_failure = Some(false);
        events.emit(record);
        let change = next_change(&mut receiver).await;
        assert!(!change.notify);
        assert!(change.record.is_some());
    }

    let page = events.query(Filter {
        reason: Some(Reason::RequestInterrupted),
        ..Filter::default()
    });
    assert_eq!(page.total, 3);
    let mut counts: Vec<u32> = page.items.iter().map(|item| item.count).collect();
    counts.sort_unstable();
    assert_eq!(counts, [1, 1, 2]);
}
