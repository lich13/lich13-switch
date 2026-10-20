//! Health permits and terminal settlement shared by HTTP, native WebSocket and
//! admission. Keep ownership/drop order independent of either transport.
use super::{
    circuit::{Outcome, Permit},
    model::Settings,
    Gateway, Route,
};
use crate::events::{Action, Reason};
use std::time::Duration;

/// An ephemeral receipt bound to this emitted event. Cached health causes are
/// deliberately unconfirmed; their presence cannot suppress a new failure.
pub(super) struct Reported {
    pub record: crate::events::Record,
    pub queued: bool,
}
impl std::ops::Deref for Reported {
    type Target = crate::events::Record;
    fn deref(&self) -> &Self::Target {
        &self.record
    }
}
impl From<crate::events::Record> for Reported {
    fn from(record: crate::events::Record) -> Self {
        Self {
            record,
            queued: false,
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn observe_protocol(
    protocol: &super::protocol::Protocol,
    permits: &mut Permits,
    cfg: &Settings,
    g: &Gateway,
    route: &Route,
    model: Option<&str>,
    status: Option<u16>,
    attempt: usize,
) {
    if permits.provider.is_some() {
        let reason = match protocol.terminal() {
            Some(super::protocol::Terminal::ModelUnavailable) => Some(Reason::ModelUnavailable),
            Some(super::protocol::Terminal::Failure) if protocol.transport_failure() => {
                Some(Reason::Network)
            }
            Some(super::protocol::Terminal::Failure) if status.is_some_and(|value| value < 400) => {
                Some(Reason::ProtocolError)
            }
            Some(super::protocol::Terminal::Failure) => Some(Reason::UpstreamService),
            Some(super::protocol::Terminal::Rejected)
                if protocol.observation.error != crate::events::Details::default() =>
            {
                Some(Reason::ProtocolError)
            }
            _ => None,
        };
        if let Some(reason) = reason {
            let mut details = protocol.observation.error.clone();
            details.phase = Some(crate::events::Phase::Stream);
            details.counted_failure =
                Some(reason == Reason::Network || reason == Reason::UpstreamService);
            protocol.report_failure(|| {
                permits.report(
                    g,
                    route,
                    model,
                    reason,
                    Action::Returned,
                    status,
                    attempt,
                    details,
                )
            });
        }
    }
    settle_protocol(protocol, permits, cfg, status);
}
pub(super) fn settle_protocol(
    protocol: &super::protocol::Protocol,
    permits: &mut Permits,
    cfg: &Settings,
    status: Option<u16>,
) {
    use super::protocol::Terminal;
    match protocol.terminal() {
        Some(Terminal::Success | Terminal::Limited) => permits.success(cfg),
        Some(Terminal::Failure)
            if protocol.transport_failure() || status.is_none_or(|value| value >= 400) =>
        {
            permits.failure(cfg, None)
        }
        Some(Terminal::Failure) => permits.neutral(cfg),
        Some(_) => permits.neutral(cfg),
        None => (),
    }
}
pub(super) struct Permits {
    provider: Option<Permit>,
    model: Option<Permit>,
    scope: crate::events::HealthScope,
}
impl Permits {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn report(
        &mut self,
        g: &Gateway,
        route: &Route,
        model: Option<&str>,
        reason: Reason,
        action: Action,
        status: Option<u16>,
        attempt: usize,
        mut details: crate::events::Details,
    ) -> Reported {
        use crate::events::{HealthScope, Phase};
        self.scope = details.health_scope.unwrap_or_else(|| {
            if reason == Reason::Network
                && status.is_none()
                && matches!(details.phase, Some(Phase::Connect | Phase::WsHandshake))
            {
                HealthScope::Provider
            } else if self.model.is_some() {
                HealthScope::Model
            } else {
                HealthScope::Request
            }
        });
        if details.error_basis == Some(crate::events::ErrorBasis::Rpm) {
            details.rpm_wait_reason = Some(crate::events::RpmWaitReason::UpstreamBackoff);
            let delay = std::time::Duration::from_secs(details.wait_seconds.unwrap_or_else(|| {
                g.0.inner
                    .lock()
                    .unwrap()
                    .store
                    .settings
                    .capacity_retry_seconds
            }));
            g.0.admission.rpm_backoff(&route.provider.id, delay);
        }
        details.health_scope = Some(self.scope);
        if self.scope == HealthScope::Request {
            details.counted_failure = Some(false);
        }
        if details.upstream_code.is_none() && details.local_code.is_none() {
            details.local_code = serde_json::to_value(reason.code())
                .ok()
                .and_then(|v| v.as_str().map(str::to_owned));
        }
        let mut event = crate::events::Record::new(
            Some(g.0.client),
            Some(&route.provider.id),
            model,
            reason,
            action,
            status,
            Some(attempt.min(u32::MAX as usize) as u32),
        );
        event.details = details.sanitized();
        let target = match self.scope {
            HealthScope::Provider => &mut self.provider,
            _ => &mut self.model,
        };
        if let Some(p) = target {
            p.set_failure_event(event.clone());
        }
        let queued = super::locks::published(&g.0.diagnostics)
            .as_ref()
            .is_some_and(|service| service.emit_checked(event.clone()));
        Reported {
            record: event,
            queued,
        }
    }

    fn settle(&mut self, cfg: &Settings, outcome: Outcome) {
        use crate::events::HealthScope;
        match self.scope {
            HealthScope::Provider => {
                if let Some(p) = self.provider.take() {
                    p.finish(outcome, cfg);
                }
                if let Some(p) = self.model.take() {
                    p.finish(Outcome::Neutral, cfg);
                }
            }
            HealthScope::Model => {
                if let Some(p) = self.model.take() {
                    p.finish(outcome, cfg);
                }
                if let Some(p) = self.provider.take() {
                    p.finish(Outcome::Neutral, cfg);
                }
            }
            HealthScope::Request => self.neutral(cfg),
        }
    }
    pub(super) fn capacity_limited(&mut self, cfg: &Settings, retry: Option<Duration>) {
        self.settle(cfg, Outcome::CapacityLimited(retry));
    }
    pub(super) fn rate_limited(&mut self, cfg: &Settings, retry: Option<Duration>) {
        self.settle(cfg, Outcome::RateLimited(retry));
    }
    pub(super) fn acquire_for(
        route: &Route,
        manual: bool,
        requirement: &super::routing::Requirement,
    ) -> Option<Self> {
        let provider = route.provider_circuit.acquire(manual)?;
        let model = match route.model_circuit(requirement) {
            Some(circuit) => Some(circuit.acquire(manual)?),
            None => None,
        };
        let scope = if model.is_some() {
            crate::events::HealthScope::Model
        } else {
            crate::events::HealthScope::Request
        };
        Some(Self {
            provider: Some(provider),
            model,
            scope,
        })
    }
    pub(super) fn success(&mut self, cfg: &Settings) {
        if let Some(p) = self.provider.take() {
            p.finish(Outcome::Success, cfg);
        }
        if let Some(p) = self.model.take() {
            p.finish(Outcome::Success, cfg);
        }
    }
    pub(super) fn neutral(&mut self, cfg: &Settings) {
        if let Some(p) = self.provider.take() {
            p.finish(Outcome::Neutral, cfg);
        }
        if let Some(p) = self.model.take() {
            p.finish(Outcome::Neutral, cfg);
        }
    }
    pub(super) fn failure(&mut self, cfg: &Settings, retry: Option<Duration>) {
        self.settle(cfg, Outcome::Failure(retry));
    }
}
