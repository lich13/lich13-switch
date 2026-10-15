//! Bounded routing metadata. Request and response content never enter this record.
use super::{routing::Requirement, Gateway, Route};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    #[default]
    Initial,
    ProactiveHandoff,
    FailureFallback,
    CursorPinned,
    UnknownOwner,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Failure {
    RateLimit,
    AccountRejected,
    ModelUnavailable,
    UpstreamService,
    Transport,
    Protocol,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Exclusion {
    Model,
    Protocol,
    ConversationPolicy,
    Health,
    Capacity,
    Rpm,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Exit {
    Forwarded,
    NoEligibleProvider,
    AttemptsExhausted,
    PermanentRejection,
    Cancelled,
    Stopped,
    AdmissionRejected,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitKind {
    CapacityRetry,
    RetryAfter,
    TransientRetry,
    Admission,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Wait {
    pub kind: WaitKind,
    pub seconds: u64,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Evidence {
    pub stage: Stage,
    pub failure: Option<Failure>,
    pub eligible_candidates: u32,
    pub exclusions: BTreeMap<Exclusion, u32>,
    pub wait: Option<Wait>,
    pub exit: Option<Exit>,
}
pub(super) struct Tracker {
    trace: Option<crate::usage::Trace>,
    evidence: Evidence,
}
impl Tracker {
    pub(super) fn new(trace: Option<crate::usage::Trace>) -> Self {
        Self {
            trace,
            evidence: Evidence::default(),
        }
    }
    fn publish(&self) {
        if let Some(trace) = &self.trace {
            trace.failover(self.evidence.clone());
        }
    }
    #[allow(clippy::too_many_arguments)]
    pub(super) fn candidates(
        &mut self,
        gateway: &Gateway,
        stage: Stage,
        all: &[Route],
        eligible: &[Route],
        requirement: &Requirement,
        path: &str,
        budget: &super::admission::Budget,
        manual: bool,
    ) {
        self.evidence.stage = stage;
        self.evidence.exit = None;
        self.evidence.exclusions.clear();
        self.evidence.eligible_candidates = 0;
        for route in all {
            let p = &route.provider;
            let excluded = if !route.accepts_protocol(path) {
                Some(Exclusion::Protocol)
            } else if !requirement.allows(p.allowed_models.as_deref()) {
                Some(Exclusion::Model)
            } else if !eligible.iter().any(|r| r.provider.id == p.id) {
                Some(Exclusion::ConversationPolicy)
            } else if !route.health_for(requirement).available {
                Some(Exclusion::Health)
            } else if !gateway
                .0
                .admission
                .capacity_available(&p.id, budget, manual)
            {
                Some(Exclusion::Capacity)
            } else if gateway.0.admission.rpm_status(&p.id, p.max_rpm).2 {
                Some(Exclusion::Rpm)
            } else {
                None
            };
            if let Some(reason) = excluded {
                *self.evidence.exclusions.entry(reason).or_default() += 1;
            } else {
                self.evidence.eligible_candidates += 1;
            }
        }
        self.publish();
    }
    pub fn failure(&mut self, failure: Failure) {
        self.evidence.failure = Some(failure);
        self.publish();
    }
    pub fn wait(&mut self, kind: WaitKind, seconds: u64) {
        self.evidence.wait = Some(Wait { kind, seconds });
        self.publish();
    }
    pub fn finish(&mut self, exit: Exit) {
        if matches!(
            exit,
            Exit::NoEligibleProvider | Exit::AttemptsExhausted | Exit::PermanentRejection
        ) {
            self.evidence.eligible_candidates = 0;
        }
        self.evidence.exit = Some(exit);
        self.publish();
    }
    pub fn detach(&mut self) {
        self.trace = None;
    }
    pub fn rejected(&mut self, reason: &super::admission::Rejected) {
        use super::admission::Rejected;
        let category = match reason {
            Rejected::Full | Rejected::Timeout => Some(Exclusion::Capacity),
            Rejected::Cooling(_) => Some(Exclusion::Health),
            Rejected::RateLimited(_) | Rejected::RateLedger => Some(Exclusion::Rpm),
            Rejected::Model => Some(Exclusion::Model),
            Rejected::Policy => Some(Exclusion::ConversationPolicy),
            _ => None,
        };
        if let Some(category) = category {
            self.evidence.exclusions.entry(category).or_insert(1);
        }
        if let Rejected::Cooling(seconds) | Rejected::RateLimited(seconds) = reason {
            self.evidence.wait = Some(Wait {
                kind: WaitKind::Admission,
                seconds: *seconds,
            });
        }
        self.evidence.exit = Some(if matches!(reason, Rejected::Stopped) {
            Exit::Stopped
        } else {
            Exit::AdmissionRejected
        });
        self.publish();
    }
}
