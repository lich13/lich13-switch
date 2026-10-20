//! Model policy is independent of transport versions, health and capacity.
use crate::errors::code;
/// Capture before admission: admitting a first request must not retroactively
/// turn its ordinary capacity fallback into an established-thread wait.
pub(super) fn capacity_owner(
    client: super::ClientId,
    manual: bool,
    lease: Option<&super::compaction::Lease>,
) -> Option<String> {
    if client != super::ClientId::Codex || manual {
        return None;
    }
    lease
        // A legal compaction handoff retains its own receiver/fallback policy;
        // the existing-thread capacity wait applies only to an ordinary bound
        // owner, never to a handoff turn that is already authorized to move.
        .filter(|l| !l.request_only() && !l.handoff)
        .and_then(|l| l.admission_policy().1)
}
pub(super) fn capacity_cooling(health: &super::circuit::Health) -> bool {
    !health.available
        && health.state == super::circuit::CircuitState::Closed
        && matches!(
            health.cooldown_reason.as_deref(),
            Some("capacity_retry" | "rate_limit")
        )
        && health.cause.as_ref().is_some_and(|cause| {
            matches!(
                cause.reason,
                crate::events::Reason::RateLimit | crate::events::Reason::Capacity
            )
        })
}
/// Give an established conversation two short, cancellable retries on its
/// owner before ordinary failover. These still consume the normal retry budget.
pub fn transient_delay(
    retries: &mut u8,
    retry_after: Option<std::time::Duration>,
) -> Option<std::time::Duration> {
    if *retries >= 2 {
        return None;
    }
    *retries += 1;
    Some(std::time::Duration::from_secs(u64::from(*retries)).max(retry_after.unwrap_or_default()))
}

#[derive(Clone, Debug, PartialEq)]
pub enum Requirement {
    Resource,
    Model(String),
    Unknown,
}
impl Requirement {
    pub fn model(model: Option<&str>) -> Self {
        model
            .filter(|m| !m.is_empty())
            .map(|m| Self::Model(m.into()))
            .unwrap_or(Self::Unknown)
    }
    pub fn allows(&self, allowed: Option<&[String]>) -> bool {
        match (self, allowed) {
            (Self::Resource, _) | (_, None) => true,
            (Self::Model(model), Some(allowed)) => allowed.contains(model),
            (Self::Unknown, Some(_)) => false,
        }
    }
    pub fn code(&self) -> &'static str {
        if matches!(self, Self::Unknown) {
            code::MODEL_UNDETERMINED
        } else {
            code::MODEL_NOT_ALLOWED
        }
    }
    pub fn message(&self) -> &'static str {
        if matches!(self, Self::Unknown) {
            "无法识别请求模型，且没有不限模型的供应商"
        } else {
            "当前候选供应商不允许此模型，请检查模型白名单和上下文归属"
        }
    }
}
pub fn resource(method: &hyper::Method, path: &str) -> bool {
    let path = path
        .strip_prefix("/v1/")
        .map(|p| format!("/{p}"))
        .unwrap_or_else(|| path.to_owned());
    ["/models", "/files", "/uploads"]
        .iter()
        .any(|prefix| path == *prefix || path.starts_with(&format!("{prefix}/")))
        || (*method == hyper::Method::GET && path.starts_with("/responses/"))
}

/// A diagnostic preview of the existing fallback policy; it reserves nothing.
/// The real admission still performs its atomic version/queue/capacity check.
#[allow(clippy::too_many_arguments)]
pub(super) fn fallback_action(
    gateway: &super::Gateway,
    current: &str,
    candidates: Vec<super::Route>,
    requirement: &Requirement,
    path: &str,
    lease: Option<&super::compaction::Lease>,
    unidentified: bool,
    budget: &super::admission::Budget,
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
    let candidates = eligible(
        candidates
            .into_iter()
            .filter(|route| {
                route.provider.id != current
                    && route.accepts_protocol(path)
                    && requirement.allows(route.provider.allowed_models.as_deref())
            })
            .collect(),
    );
    let candidates: Vec<_> = candidates
        .into_iter()
        .filter_map(|route| {
            gateway
                .route(&route.provider.id)
                .filter(|live| live.provider.version == route.provider.version)
        })
        .filter(|route| {
            route.provider.id != current
                && queued.contains(&route.provider.id)
                && route.accepts_protocol(path)
                && requirement.allows(route.provider.allowed_models.as_deref())
        })
        .collect();
    let candidates = eligible(candidates);
    if candidates.iter().any(|route| {
        let p = &route.provider;
        route.health_for(requirement).available
            && gateway.0.admission.capacity_available(&p.id, budget, false)
            && !gateway.0.admission.rpm_status(&p.id, p.max_rpm).2
    }) {
        Action::TryingNext
    } else {
        Action::Returned
    }
}
