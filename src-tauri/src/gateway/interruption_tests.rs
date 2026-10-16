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
use std::{sync::atomic::Ordering, time::Duration};

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
async fn partial_response_close_keeps_unknown_terminal_and_cancel_evidence() {
    let fixture = Fixture::new();
    let mut receiver = fixture.events.subscribe();
    let mut protocol = fixture.protocol();
    protocol.response(200, true, "identity");
    // The payload announces a terminal, but the event has not ended and is not observed.
    protocol.feed(b"data: {\"type\":\"response.completed\"");
    protocol.delivery().handed(9, false);
    protocol.downstream_closed(&Activity::default(), false);
    drop(protocol);

    let record = next_record(&mut receiver).await;
    assert_eq!(record.reason, Reason::RequestInterrupted);
    assert_eq!(
        record.details.local_code.as_deref(),
        Some("DOWNSTREAM_CLOSED")
    );
    assert_eq!(
        record.details.message.as_deref(),
        Some("下游连接关闭，未确认客户端主动取消")
    );
    assert_eq!(record.details.counted_failure, Some(false));
    assert_eq!(record.level, Level::Warning);
    assert!(matches!(
        category_for(&fixture.events, &record.id),
        Category::OrdinaryError
    ));
    assert!(!record.reason.notifiable(record.action));

    let delivery = record.details.delivery.unwrap();
    assert!(delivery.upstream_bytes > 0);
    assert_eq!(delivery.handed_bytes, 9);
    assert!(!delivery.terminal_received);
    assert!(!delivery.terminal_handed_off);
    assert!(record.details.last_progress_ms.is_some());
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
async fn reset_and_write_failures_are_warning_records_not_client_cancellations() {
    let fixture = Fixture::new();
    let mut receiver = fixture.events.subscribe();

    let mut reset = fixture.protocol();
    reset.response(200, true, "identity");
    let reset_activity = Activity::default();
    reset_activity.failed(
        &std::io::Error::new(std::io::ErrorKind::ConnectionReset, "fixture reset"),
        false,
    );
    reset.downstream_closed(&reset_activity, false);
    drop(reset);

    let reset_record = next_record(&mut receiver).await;
    assert_eq!(
        reset_record.details.local_code.as_deref(),
        Some("DOWNSTREAM_PEER_RESET")
    );
    assert_eq!(
        reset_record.details.message.as_deref(),
        Some("下游连接被重置，未确认客户端主动取消")
    );
    assert_eq!(reset_record.level, Level::Warning);
    assert!(matches!(
        category_for(&fixture.events, &reset_record.id),
        Category::OrdinaryError
    ));

    let mut failed_write = fixture.protocol();
    failed_write.response(200, true, "identity");
    let write_activity = Activity::default();
    write_activity.failed(
        &std::io::Error::new(std::io::ErrorKind::BrokenPipe, "fixture write failure"),
        true,
    );
    failed_write.downstream_closed(&write_activity, true);
    drop(failed_write);

    let write_record = next_record(&mut receiver).await;
    assert_eq!(
        write_record.details.local_code.as_deref(),
        Some("DOWNSTREAM_WRITE_FAILED")
    );
    assert_eq!(
        write_record.details.message.as_deref(),
        Some("向下游写入失败")
    );
    assert_eq!(write_record.level, Level::Warning);
    assert!(matches!(
        category_for(&fixture.events, &write_record.id),
        Category::OrdinaryError
    ));
}

#[tokio::test]
async fn observed_upstream_success_remains_distinct_from_a_failed_delivery() {
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
    drop(protocol);

    let record = next_record(&mut receiver).await;
    assert_eq!(
        record.details.local_code.as_deref(),
        Some("DOWNSTREAM_WRITE_FAILED")
    );
    assert_eq!(record.level, Level::Warning);
    assert!(matches!(
        category_for(&fixture.events, &record.id),
        Category::OrdinaryError
    ));
    let evidence = record.details.delivery.unwrap();
    assert!(evidence.terminal_received);
    assert!(!evidence.terminal_handed_off);
    assert_eq!(evidence.handed_bytes, 17);
    assert_eq!(delivery.bytes.load(Ordering::Relaxed), 17);
}

#[tokio::test]
async fn diagnostics_without_subscribers_do_not_block_delivery_or_interruption_recording() {
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

    tokio::time::timeout(Duration::from_secs(3), async {
        while fixture.events.query(Filter::default()).total == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(delivery.bytes.load(Ordering::Relaxed), 11);
    assert_eq!(delivery.error.load(Ordering::Relaxed), 7);
    let page = fixture.events.query(Filter {
        reason: Some(Reason::RequestInterrupted),
        ..Filter::default()
    });
    assert_eq!(page.total, 1);
    assert_eq!(
        page.items[0].details.local_code.as_deref(),
        Some("DOWNSTREAM_WRITE_FAILED")
    );
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
