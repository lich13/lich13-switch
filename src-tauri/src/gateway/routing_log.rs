//! One bounded routing result per logical HTTP request or WebSocket round.
#[cfg(test)]
#[path = "routing_log_v02511_tests.rs"]
mod v02511_tests;
#[cfg(test)]
#[path = "routing_log_v02512_tests.rs"]
mod v02512_tests;
#[cfg(test)]
#[path = "routing_log_v02513_tests.rs"]
mod v02513_tests;
#[cfg(test)]
#[path = "routing_log_v0259_tests.rs"]
mod v0259_tests;

#[cfg(test)]
use crate::usage::model::BoundaryState;
use crate::{
    events,
    usage::model::{RoutingDecision, RoutingEvidence},
};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct State {
    record: Option<events::Record>,
    failure: Option<super::failover::Failure>,
    handled: bool,
    attempts: u32,
    keep: bool,
    settled: Option<super::protocol::RoutingSettlement>,
    failure_generation: u64,
    reported_generation: u64,
    unreported: bool,
    save_generation: u64,
    save_failed: bool,
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
        if state.handled || state.save_generation != 0 {
            return;
        }
        state.attempts = state.attempts.saturating_add(1);
        // Keep real policy restrictions and actions, not the ordinary decision
        // itself. A successful model detour intentionally preserves ownership.
        state.keep |= !ordinary(decision)
            || matches!(
                evidence.identity,
                Some(
                    crate::usage::model::ThreadIdentity::Conflict
                        | crate::usage::model::ThreadIdentity::Unknown
                )
            )
            || (evidence.restriction.is_some()
                && !(decision == RoutingDecision::ModelDetourOwnerPreserved
                    && evidence.restriction
                        == Some(crate::usage::model::RoutingRestriction::ModelMismatch)));
        state.settled = None;
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
    pub fn failure(&self, failure: super::failover::Failure) -> u64 {
        let Ok(mut state) = self.state.lock() else {
            return 0;
        };
        state.unreported |= state.failure_generation != state.reported_generation;
        state.failure_generation = state.failure_generation.saturating_add(1);
        state.failure = Some(failure);
        if let Some(routing) = state
            .record
            .as_mut()
            .and_then(|r| r.details.routing.as_mut())
        {
            routing.failure = Some(failure);
        }
        state.failure_generation
    }
    pub fn reported(&self, generation: u64) {
        if let Ok(mut state) = self.state.lock() {
            if generation != 0 && generation == state.failure_generation {
                state.reported_generation = generation;
            }
        }
    }
    pub fn failure_reported(&self) -> bool {
        self.state.lock().is_ok_and(|state| {
            state.failure_generation != 0
                && state.failure_generation == state.reported_generation
                && !state.unreported
        })
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
    pub fn window_consumed(&self, no_receiver: bool) {
        if !no_receiver {
            self.outcome(events::RoutingOutcome::WindowConsumed);
        } else if let Ok(mut state) = self.state.lock() {
            if let Some(routing) = state
                .record
                .as_mut()
                .and_then(|r| r.details.routing.as_mut())
            {
                routing.outcome = events::RoutingOutcome::WindowConsumed;
            }
        }
    }
    pub fn begin_save(&self) -> u64 {
        let Ok(mut state) = self.state.lock() else {
            return 0;
        };
        state.save_generation = state.save_generation.saturating_add(1);
        state.save_generation
    }
    pub fn saved(
        &self,
        generation: u64,
        outcome: events::RoutingOutcome,
        scope: &str,
        handoff: bool,
        notification: Option<String>,
    ) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        let failed = outcome == events::RoutingOutcome::SaveFailed;
        if state.save_failed || (generation != state.save_generation && !failed) {
            return;
        }
        state.save_failed |= failed;
        state.keep |= matches!(
            outcome,
            events::RoutingOutcome::OwnershipSaved
                | events::RoutingOutcome::WindowReleased
                | events::RoutingOutcome::SaveFailed
        );
        if let Some(record) = &mut state.record {
            record.details.handoff_scope = Some(scope.into());
            if let Some(routing) = &mut record.details.routing {
                routing.outcome = outcome;
            }
            if failed {
                record.reason = events::Reason::ThreadRouting;
                record.error_code = Some(crate::errors::ErrorCode::ConversationOwnership);
                record.level = events::Level::Error;
                record.action = events::Action::Stopped;
                record.details.message = Some("线程归属记录保存失败".into());
                record.notification_message = None;
            } else if outcome == events::RoutingOutcome::OwnershipSaved {
                record.action = events::Action::Routed;
                if handoff {
                    record.reason = events::Reason::ThreadHandoff;
                    record.error_code = Some(crate::errors::ErrorCode::ThreadHandoff);
                    record.notification_message = notification;
                }
            }
        }
    }
    pub fn finish(&self, status: Option<u16>, success: bool) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        // Non-protocol callers use false for admission, cancellation or an
        // unknown failure. A later downstream close cannot erase that fact.
        state.unreported |= !success;
        state.settled = Some(if success {
            super::protocol::RoutingSettlement::Delivered
        } else {
            super::protocol::RoutingSettlement::Failed
        });
        if let Some(record) = &mut state.record {
            record.status = status;
            if !success {
                record.level = events::Level::Warning;
                record
                    .details
                    .message
                    .get_or_insert_with(|| "请求未正常完成".into());
            }
            if let Some(routing) = &mut record.details.routing {
                if routing.outcome == events::RoutingOutcome::Attempted
                    || (success
                        && routing.outcome == events::RoutingOutcome::Failed
                        && routing.failure.is_none())
                {
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
        state.unreported |= result == RoutingSettlement::Failed;
        state.settled = Some(result);
        if let Some(record) = &mut state.record {
            record.status = status.or(record.status);
            if result == RoutingSettlement::Failed {
                record.level = events::Level::Warning;
                record
                    .details
                    .message
                    .get_or_insert_with(|| "请求未正常完成".into());
            }
            if let Some(routing) = &mut record.details.routing {
                if routing.outcome == events::RoutingOutcome::Attempted
                    || (result == RoutingSettlement::Delivered
                        && routing.outcome == events::RoutingOutcome::Failed
                        && routing.failure.is_none())
                {
                    routing.outcome = if result == RoutingSettlement::Delivered {
                        events::RoutingOutcome::OwnerPreserved
                    } else {
                        events::RoutingOutcome::Failed
                    };
                }
            }
        }
    }
    /// Build historical records for compatibility tests. Live save receipts
    /// keep the log alive and pass through the final settlement filter instead.
    #[cfg(test)]
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
                    && !state.unreported
                    && state.failure_generation == state.reported_generation
                    && record.details.routing.as_ref().is_some_and(|routing| {
                        ordinary(routing.decision)
                            && (routing.decision != RoutingDecision::ThreadUnidentified
                                || state.settled
                                    == Some(super::protocol::RoutingSettlement::Delivered))
                            && matches!(
                                routing.outcome,
                                events::RoutingOutcome::OwnerPreserved
                                    | events::RoutingOutcome::Failed
                                    | events::RoutingOutcome::WindowConsumed
                                    | events::RoutingOutcome::BoundaryRegistered
                            )
                            && matches!(
                                state.settled,
                                Some(
                                    super::protocol::RoutingSettlement::Delivered
                                        | super::protocol::RoutingSettlement::DownstreamClosed
                                        | super::protocol::RoutingSettlement::ReportedFailure
                                )
                            )
                    });
                if !quiet {
                    self.service.emit(record);
                }
            }
        }
    }
}

fn ordinary(decision: RoutingDecision) -> bool {
    matches!(
        decision,
        RoutingDecision::ExistingThread
            | RoutingDecision::BoundaryConsumed
            | RoutingDecision::BoundaryNotObserved
            | RoutingDecision::NewThread
            | RoutingDecision::FirstTurnUnconfirmed
            | RoutingDecision::OrdinaryQueue
            | RoutingDecision::Auxiliary
            | RoutingDecision::ModelDetourOwnerPreserved
            | RoutingDecision::ThreadUnidentified
    )
}
