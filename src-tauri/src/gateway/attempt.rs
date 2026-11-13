//! Attempt policy and delivery settlement shared by the transports. HTTP body
//! error frames and native WebSocket close frames keep their own wire encoding.
//! Protocol::finish remains the single usage/terminal finalizer.
use super::{
    protocol::{Delivery, DownstreamError, Protocol},
    replay_policy,
};
use crate::errors::code;
use std::sync::Arc;

pub(super) fn transport_retry(received: bool, eligible: bool, failure: &str) -> bool {
    replay_policy::transport_retry(received, eligible) && failure != code::FIRST_BYTE_TIMEOUT
}

/// The current queue order without hard-denied provider versions, and the
/// routes among it that speak the request path.
pub(super) fn queue(
    gateway: &super::Gateway,
    pinned: Option<&str>,
    hard_denied: &std::collections::HashSet<(String, String)>,
    path: &str,
) -> (Vec<String>, std::collections::HashMap<String, super::Route>) {
    let mut ids = gateway.routing_ids(pinned);
    ids.retain(|id| {
        gateway
            .route(id)
            .is_some_and(|route| !hard_denied.contains(&(id.clone(), route.provider.version)))
    });
    let routes = ids
        .iter()
        .filter_map(|id| {
            gateway
                .route(id)
                .filter(|route| route.accepts_protocol(path))
                .map(|route| (id.clone(), route))
        })
        .collect();
    (ids, routes)
}

pub(super) struct DeliveryGuard {
    pub delivery: Arc<Delivery>,
    pub complete: bool,
}
impl Drop for DeliveryGuard {
    fn drop(&mut self) {
        if !self.complete {
            self.delivery.dropped();
        }
    }
}

#[derive(Clone, Copy)]
pub(super) enum DownstreamEnd {
    Closed,
    Timeout(u64),
    PeerClosed { error: u8, writing: bool },
}
impl DownstreamEnd {
    pub fn record(self, delivery: &Delivery, protocol: &mut Protocol) {
        match self {
            Self::Timeout(seconds) => {
                delivery.event("downstream_write_timeout");
                let _ = delivery.error.compare_exchange(
                    0,
                    DownstreamError::WriteTimeout.into(),
                    std::sync::atomic::Ordering::AcqRel,
                    std::sync::atomic::Ordering::Relaxed,
                );
                protocol.timeout("downstream_write", seconds);
                delivery.settle();
            }
            Self::Closed if !delivery.is_settled() => delivery.dropped(),
            Self::Closed => (),
            Self::PeerClosed { error, writing } => {
                delivery.failed(if error != 0 {
                    error
                } else if writing {
                    DownstreamError::ConnectionError.into()
                } else {
                    DownstreamError::Closed.into()
                });
            }
        }
    }
}

/// A diagnostic preview of the existing fallback policy; it reserves nothing.
/// The real admission still performs its atomic version/queue/capacity check.
#[allow(clippy::too_many_arguments)]
pub(super) fn fallback_action(
    gateway: &super::Gateway,
    current: &str,
    candidates: Vec<super::Route>,
    requirement: &super::routing::Requirement,
    path: &str,
    lease: Option<&super::compaction::Lease>,
    unidentified: bool,
    allowed: bool,
) -> crate::events::Action {
    use crate::events::Action;
    if !allowed {
        return Action::Returned;
    }
    let queued = gateway.routing_ids(None);
    let eligible = |routes| {
        if let Some(lease) = lease {
            lease.preview_after_failure(routes, requirement)
        } else if unidentified {
            super::compaction::unidentified_candidates(routes)
        } else {
            routes
        }
    };
    // The current request still filters its retained route snapshots before
    // admission revalidates live configuration. A newly relaxed receiver
    // policy cannot make a previously ineligible snapshot a real fallback.
    let matches_request = |route: &super::Route| {
        route.provider.id != current
            && route.accepts_protocol(path)
            && requirement.allows(route.provider.allowed_models.as_deref())
    };
    let candidates = eligible(candidates.into_iter().filter(matches_request).collect());
    let candidates = eligible(
        candidates
            .into_iter()
            .filter_map(|route| {
                gateway
                    .route(&route.provider.id)
                    .filter(|live| live.provider.version == route.provider.version)
            })
            .filter(|route| queued.contains(&route.provider.id) && matches_request(route))
            .collect(),
    );
    if candidates.iter().any(|route| {
        let p = &route.provider;
        route.health_for(requirement).available
            && gateway.0.admission.capacity_available(&p.id)
            && !gateway.0.admission.rpm_status(&p.id, p.max_rpm).2
    }) {
        Action::TryingNext
    } else {
        Action::Returned
    }
}

/// Whether a failed attempt may move on to another queued provider at all.
pub(super) fn may_fail_over(
    manual: bool,
    pinned: Option<&str>,
    unknown_affinity: bool,
    ordinary_attempts: usize,
    max_retries: usize,
) -> bool {
    !manual && pinned.is_none() && !unknown_affinity && ordinary_attempts <= max_retries
}

/// `fallback_action` over the route snapshots this request retained.
#[allow(clippy::too_many_arguments)]
pub(super) fn retained_fallback(
    gateway: &super::Gateway,
    current: &str,
    ids: &[String],
    routes: &std::collections::HashMap<String, super::Route>,
    requirement: &super::routing::Requirement,
    path: &str,
    lease: Option<&super::compaction::Lease>,
    unidentified: bool,
    allowed: bool,
) -> crate::events::Action {
    fallback_action(
        gateway,
        current,
        ids.iter()
            .filter_map(|id| routes.get(id).cloned())
            .collect(),
        requirement,
        path,
        lease,
        unidentified,
        allowed,
    )
}

/// A provider that failed transiently is retried in place while it stays
/// available, or while its closed circuit only honours an upstream Retry-After.
pub(super) fn same_provider_delay(
    retry_same: bool,
    health: &super::Health,
    retries: &mut u8,
    retry_after: Option<std::time::Duration>,
) -> Option<std::time::Duration> {
    (retry_same
        && (health.available
            || (health.state == super::circuit::CircuitState::Closed
                && health.cooldown_reason.as_deref() == Some("retry_after"))))
    .then(|| super::routing::transient_delay(retries, retry_after))
    .flatten()
}
