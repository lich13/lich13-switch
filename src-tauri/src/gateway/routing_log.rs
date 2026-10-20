//! One bounded routing result per logical HTTP request or WebSocket round.
#[cfg(test)]
#[path = "routing_log_v02511_tests.rs"]
mod v02511_tests;
#[cfg(test)]
#[path = "routing_log_v0259_tests.rs"]
mod v0259_tests;

use crate::{
    events,
    usage::model::{BoundaryState, RoutingDecision, RoutingEvidence},
};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct State {
    record: Option<events::Record>,
    failure: Option<super::failover::Failure>,
    handled: bool,
    attempts: u32,
    keep: bool,
    downstream_closed_only: bool,
}
pub(super) struct RoutingLog {
    service: events::Service,
    client: super::ClientId,
    state: Mutex<State>,
}
impl RoutingLog {
    pub fn new(gateway: &super::Gateway) -> Option<Arc<Self>> {
        let service = super::locks::published(&gateway.0.diagnostics).clone()?;
        Some(Arc::new(Self {
            service,
            client: gateway.client_id(),
            state: Mutex::default(),
        }))
    }
    pub fn context(
        &self,
        provider: &str,
        model: Option<&str>,
        decision: RoutingDecision,
        evidence: RoutingEvidence,
        attempt_id: Option<&str>,
    ) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        if state.handled {
            return;
        }
        state.attempts = state.attempts.saturating_add(1);
        // Ordinary unchanged continuation is not a journal event. A fresh
        // window, first-turn decision or actual fallback supplies new evidence.
        if decision == RoutingDecision::ExistingThread
            && state.failure.is_none()
            && evidence.input != crate::usage::model::RoutingInput::Compaction
            && !matches!(
                evidence.boundary,
                Some(BoundaryState::Fresh | BoundaryState::Claimed)
            )
        {
            return;
        }
        // Keep earlier routing work even when a later attempt replaces its
        // context with an ordinary continuation on a consumed window.
        state.keep |= decision != RoutingDecision::BoundaryConsumed
            || evidence.restriction.is_some()
            || matches!(
                evidence.boundary,
                Some(BoundaryState::Fresh | BoundaryState::Claimed)
            )
            || state
                .record
                .as_ref()
                .is_some_and(|record| record.provider_id.as_deref() != Some(provider));
        let mut record = events::Record::new(
            Some(self.client),
            Some(provider),
            model,
            events::Reason::ThreadRouting,
            events::Action::Returned,
            None,
            Some(state.attempts),
        );
        record.details.health_scope = Some(events::HealthScope::Request);
        record.details.counted_failure = Some(false);
        record.details.handoff_scope = Some(crate::storage::digest(record.id.as_bytes()));
        record.details.routing = Some(events::RoutingDetails {
            decision,
            evidence,
            outcome: events::RoutingOutcome::Attempted,
            attempt_id: attempt_id.map(str::to_owned),
            failure: state.failure,
        });
        state.record = Some(record);
    }
    pub fn pending(
        self: &Arc<Self>,
        headers: &hyper::HeaderMap,
        hints: Option<&super::replay::RequestHints>,
        lease: Option<&super::compaction::Lease>,
        pinned: bool,
    ) {
        // A cancellation can release a claim before any upstream attempt has
        // created its Protocol. Keep the result with that claim until saved.
        if let Some(lease) = lease {
            lease.attach_routing_log(self.clone());
        }
        let (thread, identity) = super::compaction::identity(headers);
        let provider = lease.and_then(|lease| lease.owner.as_deref()).unwrap_or("");
        // A claimed window is an attempted handoff. Before admission, using the
        // old owner as the provider would falsely report no eligible receiver.
        let decision = if !pinned && lease.is_some_and(|lease| lease.handoff) {
            RoutingDecision::CompactionHandoff
        } else {
            super::compaction::decision(thread.as_deref(), hints, pinned, lease, provider)
        };
        let mut evidence = hints
            .map(|h| h.routing_evidence.clone())
            .unwrap_or_default();
        evidence.identity = Some(identity);
        evidence.boundary = lease
            .map(|lease| lease.boundary_state(hints.and_then(|h| h.compacted_window.as_deref())));
        self.context(
            provider,
            hints.and_then(|h| h.model.as_deref()),
            decision,
            evidence,
            None,
        );
        if let Ok(mut state) = self.state.lock() {
            state.attempts = 0;
            if let Some(record) = &mut state.record {
                record.attempt = None;
            }
        }
    }
    pub fn failure(&self, failure: super::failover::Failure) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        state.failure = Some(failure);
        if let Some(routing) = state
            .record
            .as_mut()
            .and_then(|r| r.details.routing.as_mut())
        {
            routing.failure = Some(failure);
        }
    }
    pub fn outcome(&self, outcome: events::RoutingOutcome) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        // Explicit outcomes describe work or an unsettled/unknown result;
        // they must not be mistaken for the final protocol's quiet close.
        state.keep = true;
        if let Some(routing) = state
            .record
            .as_mut()
            .and_then(|r| r.details.routing.as_mut())
        {
            routing.outcome = outcome;
        }
    }
    pub fn compaction_operation(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.keep = true;
        }
    }
    pub fn finish(&self, status: Option<u16>, success: bool) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        // Non-protocol callers use false for admission, cancellation or an
        // unknown failure. A later downstream close cannot erase that fact.
        state.keep |= !success;
        if let Some(record) = &mut state.record {
            record.status = status;
            if let Some(routing) = &mut record.details.routing {
                if routing.outcome == events::RoutingOutcome::Attempted {
                    routing.outcome = if success {
                        events::RoutingOutcome::OwnerPreserved
                    } else {
                        events::RoutingOutcome::Failed
                    };
                }
            }
        }
    }
    pub fn settle(&self, status: Option<u16>, result: super::protocol::RoutingSettlement) {
        use super::protocol::RoutingSettlement;
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        state.keep |= result == RoutingSettlement::Failed;
        state.downstream_closed_only = result == RoutingSettlement::DownstreamClosed;
        if let Some(record) = &mut state.record {
            record.status = status.or(record.status);
            if let Some(routing) = &mut record.details.routing {
                if routing.outcome == events::RoutingOutcome::Attempted {
                    routing.outcome = if result == RoutingSettlement::Delivered {
                        events::RoutingOutcome::OwnerPreserved
                    } else {
                        events::RoutingOutcome::Failed
                    };
                }
            }
        }
    }
    /// Transfer emission to the ownership saver. Only its successful write may
    /// report committed ownership or a durably released claim.
    pub fn take(&self, outcome: events::RoutingOutcome) -> Option<events::Record> {
        let Ok(mut state) = self.state.lock() else {
            return None;
        };
        if state.handled {
            return None;
        }
        state.handled = true;
        let mut record = state.record.take()?;
        if let Some(routing) = &mut record.details.routing {
            routing.outcome = outcome;
        }
        Some(record)
    }
}
impl Drop for RoutingLog {
    fn drop(&mut self) {
        let Ok(state) = self.state.get_mut() else {
            return;
        };
        if !state.handled {
            if let Some(record) = state.record.take() {
                let quiet = !state.keep
                    && state.failure.is_none()
                    && record.status.is_none_or(|status| status < 400)
                    && record.details.routing.as_ref().is_some_and(|routing| {
                        routing.decision == RoutingDecision::BoundaryConsumed
                            && routing.failure.is_none()
                            && (routing.outcome == events::RoutingOutcome::OwnerPreserved
                                || (routing.outcome == events::RoutingOutcome::Failed
                                    && state.downstream_closed_only))
                    });
                if !quiet {
                    self.service.emit(record);
                }
            }
        }
    }
}
