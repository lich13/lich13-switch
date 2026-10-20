use super::*;
use crate::{
    events::{Action, Reason, RoutingOutcome},
    gateway::{diagnostic_test_support::assert_no_interruptions_after_barrier, protocol::Protocol},
};

#[tokio::test]
async fn settled_consumed_window_continuations_do_not_append_routing_events() {
    let dir = tempfile::tempdir().unwrap();
    let gateway = super::super::Gateway::new(dir.path().join("gateway")).unwrap();
    let service = events::Service::new(dir.path());
    gateway.set_diagnostics(service.clone());
    let mut receiver = service.subscribe();
    for _ in 0..5 {
        let log = RoutingLog::new(&gateway).unwrap();
        log.context(
            "fixture-provider",
            Some("fixture-model"),
            RoutingDecision::BoundaryConsumed,
            RoutingEvidence {
                boundary: Some(BoundaryState::Consumed),
                ..Default::default()
            },
            None,
        );
        log.finish(Some(200), true);
        drop(log);
    }
    gateway.0.compaction.flush();
    assert_no_interruptions_after_barrier(&service, &mut receiver, dir.path()).await;
    let page = service.query(events::Filter {
        reason: Some(Reason::ThreadRouting),
        ..Default::default()
    });
    assert_eq!(page.error, None);
    assert_eq!(
        page.total, 0,
        "steady continuations must not append routing events"
    );
    let restored = events::Service::new(dir.path());
    assert_eq!(
        restored
            .query(events::Filter {
                reason: Some(Reason::ThreadRouting),
                ..Default::default()
            })
            .total,
        0
    );
}

#[tokio::test]
async fn known_downstream_closes_do_not_append_interruption_events() {
    let dir = tempfile::tempdir().unwrap();
    let gateway = super::super::Gateway::new(dir.path().join("gateway")).unwrap();
    let service = events::Service::new(dir.path());
    gateway.set_diagnostics(service.clone());
    let mut receiver = service.subscribe();
    for error in [4, 5, 6, 7] {
        let mut protocol = Protocol::new(true);
        protocol.diagnostics(&gateway, "fixture-provider", Some("fixture-model"));
        protocol.response(200, true, "identity");
        protocol.delivery().failed(error);
        drop(protocol);
    }
    gateway.0.compaction.flush();
    assert_no_interruptions_after_barrier(&service, &mut receiver, dir.path()).await;
}

#[tokio::test]
async fn unknown_failed_and_unsettled_routing_results_remain_observable() {
    for outcome in [
        RoutingOutcome::Failed,
        RoutingOutcome::Attempted,
        RoutingOutcome::Unknown,
    ] {
        let dir = tempfile::tempdir().unwrap();
        let gateway = super::super::Gateway::new(dir.path().join("gateway")).unwrap();
        let service = events::Service::new(dir.path());
        gateway.set_diagnostics(service.clone());
        let mut receiver = service.subscribe();
        let log = RoutingLog::new(&gateway).unwrap();
        log.context(
            "fixture-provider",
            None,
            RoutingDecision::BoundaryConsumed,
            RoutingEvidence::default(),
            None,
        );
        log.outcome(outcome);
        drop(log);
        assert_no_interruptions_after_barrier(&service, &mut receiver, dir.path()).await;
        let page = service.query(events::Filter {
            reason: Some(Reason::ThreadRouting),
            ..Default::default()
        });
        assert_eq!(page.total, 1);
        assert_eq!(page.items[0].record.action, Action::Returned);
        assert_eq!(
            page.items[0]
                .record
                .details
                .routing
                .as_ref()
                .unwrap()
                .outcome,
            outcome
        );
    }
}

#[tokio::test]
async fn delivered_terminal_does_not_hide_unknown_downstream_faults() {
    for error in [1, 2, 3, 8] {
        let dir = tempfile::tempdir().unwrap();
        let gateway = super::super::Gateway::new(dir.path().join("gateway")).unwrap();
        let service = events::Service::new(dir.path());
        gateway.set_diagnostics(service.clone());
        let mut receiver = service.subscribe();
        let log = RoutingLog::new(&gateway).unwrap();
        let mut protocol = Protocol::new(true);
        protocol.diagnostics(&gateway, "fixture-provider", Some("fixture-model"));
        protocol.attach_routing_log(Some(log.clone()));
        protocol.context(None, Some(RoutingDecision::BoundaryConsumed));
        protocol.routing_evidence(None);
        protocol.response(200, true, "identity");
        let payload = b"data: {\"type\":\"response.completed\",\"response\":{\"id\":\"fixture-response\",\"status\":\"completed\"}}\n\n";
        protocol.feed(payload);
        protocol.delivery().handed(payload.len(), true);
        protocol.delivery().failed(error);
        protocol.finish(Some(200), crate::errors::code::OK);
        drop(protocol);
        drop(log);

        let marker = events::Record::new(
            None,
            None,
            None,
            Reason::Network,
            Action::Returned,
            None,
            None,
        );
        let marker_id = marker.id.clone();
        service.emit(marker);
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let change = receiver.recv().await.unwrap();
                assert!(change.error.is_none());
                assert!(!change.notify);
                if change.record.is_some_and(|record| record.id == marker_id) {
                    break;
                }
            }
        })
        .await
        .expect("diagnostic queue did not settle");
        let page = service.query(events::Filter {
            reason: Some(Reason::ThreadRouting),
            ..Default::default()
        });
        assert_eq!(page.error, None);
        assert_eq!(page.total, 0, "独立 Warning 已保留，省去重复路由");
        let warnings = service.query(events::Filter {
            reason: Some(Reason::RequestInterrupted),
            ..Default::default()
        });
        assert_eq!(warnings.total, 1);
        assert_eq!(warnings.items[0].level, events::Level::Warning);
    }
}
