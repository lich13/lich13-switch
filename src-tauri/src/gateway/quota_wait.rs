use super::{
    admission::{CapacitySource, Rejected},
    Gateway, Route,
};
use crate::events::{Action, ErrorBasis, HealthScope, Level, Phase, QuotaVerdict, Record};
use std::time::Duration;

impl Gateway {
    pub(super) fn capacity_cause(
        &self,
        provider: &str,
        model: Option<&str>,
        health: &super::circuit::Health,
    ) -> Option<Record> {
        let cause = health
            .cause
            .as_ref()
            .filter(|cause| cause.error_basis == Some(ErrorBasis::UsageLimit))?;
        let mut event = Record::new(
            Some(self.0.client),
            Some(provider),
            model,
            cause.reason,
            Action::Waiting,
            cause.status,
            None,
        );
        event.id = cause.event_id.clone();
        event.details.error_basis = cause.error_basis;
        event.details.upstream_code = Some(cause.code.clone());
        event.details.upstream_reason = cause.upstream_reason.clone();
        event.details.wait_seconds = Some(health.retry_in);
        Some(event)
    }
    /// The capacity timer is started once, before querying. Dropping this future
    /// releases both the waiter and the last subscription to an error probe.
    pub(super) async fn wait_capacity_checked(
        &self,
        sources: &[CapacitySource],
        delay: Duration,
        max_waiting: usize,
        owner: bool,
        route: Option<&Route>,
        event: Option<&Record>,
    ) -> Result<bool, Rejected> {
        let query = route
            .filter(|_| {
                event.is_some_and(|e| e.details.error_basis == Some(ErrorBasis::UsageLimit))
            })
            .and_then(|route| {
                self.query_input(&route.provider.id).ok().filter(|q| {
                    q.version
                        == Self::quota_version(&self.0.inner.lock().unwrap().store, &route.provider)
                })
            });
        // Keep one cancellable waiter while the query is pending, even when
        // the configured retry is shorter than its ten-second deadline. The
        // original retry clock continues running and is never started again.
        let hold = if query.is_some() {
            delay.max(Duration::from_secs(10))
        } else {
            delay
        };
        let retry_clock = tokio::time::sleep(delay);
        tokio::pin!(retry_clock);
        let waiting = async {
            if owner {
                self.0
                    .admission
                    .wait_owner_capacity(sources, hold, max_waiting)
                    .await
            } else {
                self.0
                    .admission
                    .wait_capacity(sources, hold, max_waiting)
                    .await
            }
        };
        tokio::pin!(waiting);
        let Some(query) = query else {
            return waiting.await.map(|()| false);
        };
        let id = query.id.clone();
        let version = query.version.clone();
        let probe = self.0.quota.probe(query);
        tokio::pin!(probe);
        let check = tokio::select! {
            biased;
            result = &mut waiting => return result.map(|()| false),
            check = &mut probe => check,
        };
        // A reset/configuration notification may race the HTTP response. Never
        // authorize another provider with stale credentials or a reset waiter.
        if !self.0.admission.capacity_current(sources)
            || !self.query_input(&id).is_ok_and(|q| q.version == version)
            || self.route(&id).is_none_or(|r| owner && !r.provider.queued)
        {
            return waiting.await.map(|()| false);
        }
        self.changed();
        let exhausted = check.verdict == QuotaVerdict::Exhausted;
        if let Some(previous) = event {
            let mut record = Record::new(
                Some(self.0.client),
                Some(&id),
                previous.model.as_deref(),
                previous.reason,
                if exhausted {
                    Action::TryingNext
                } else {
                    Action::Waiting
                },
                previous.status,
                previous.attempt,
            );
            record.level = Level::Info;
            record.details = previous.details.clone();
            record.details.phase = Some(Phase::Admission);
            record.details.health_scope = Some(HealthScope::Request);
            record.details.counted_failure = Some(false);
            record.details.cause_id = Some(previous.id.clone());
            record.details.quota_check = Some(check);
            if exhausted {
                record.details.error_basis = Some(ErrorBasis::QuotaExhausted);
                record.details.wait_seconds = None;
            }
            if let Some(service) = super::locks::published(&self.0.diagnostics).as_ref() {
                service.emit(record);
            }
        }
        if exhausted {
            return Ok(true);
        }
        tokio::select! {
            biased;
            result = &mut waiting => result.map(|()| false),
            _ = &mut retry_clock => Ok(false),
        }
    }
}
