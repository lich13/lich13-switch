//! One deadline covers admission, retries and first effective output. Completing
//! output removes the deadline; the transports keep their independent idle limit.
use std::time::Duration;
use tokio::{sync::watch, time::Instant};

#[derive(Clone)]
pub(super) struct WaitBudget {
    state: watch::Sender<Option<Instant>>,
    pub seconds: u64,
}

impl WaitBudget {
    pub fn new(seconds: u64) -> Self {
        Self {
            state: watch::channel(None).0,
            seconds,
        }
    }
    pub fn begin(&self) {
        self.begin_at(Instant::now());
    }
    pub fn begin_at(&self, started: Instant) {
        self.state
            .send_replace(Some(started + Duration::from_secs(self.seconds)));
    }
    pub fn deadline(&self) -> Option<Instant> {
        *self.state.borrow()
    }
    pub fn complete(&self, deadline: Option<Instant>) {
        self.state.send_if_modified(|current| {
            if current.is_some() && *current == deadline {
                *current = None;
                true
            } else {
                false
            }
        });
    }
    pub fn expired(&self, deadline: Option<Instant>) -> bool {
        deadline.is_some_and(|d| self.deadline() == Some(d) && Instant::now() >= d)
    }
    pub async fn wait(&self) {
        let mut changes = self.state.subscribe();
        loop {
            let deadline = *changes.borrow_and_update();
            if let Some(deadline) = deadline {
                tokio::select! {
                    biased;
                    _ = changes.changed() => {},
                    _ = tokio::time::sleep_until(deadline) => {
                        if self.expired(Some(deadline)) { return; }
                    }
                }
            } else if changes.changed().await.is_err() {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::protocol::Protocol;
    use serde_json::json;

    #[tokio::test(start_paused = true)]
    async fn heartbeats_do_not_extend_shared_budget_and_effective_output_removes_deadline() {
        let budget = WaitBudget::new(300);
        budget.begin();
        let deadline = budget.deadline();
        let mut first = Protocol::new(true);
        first.attach_wait_budget(&budget);
        first.response(200, true, "identity");
        tokio::time::advance(Duration::from_secs(150)).await;
        first.feed(b": heartbeat\n\n");
        assert_eq!(budget.deadline(), deadline);
        let mut retry = Protocol::new(true);
        retry.attach_wait_budget(&budget);
        tokio::time::advance(Duration::from_secs(149)).await;
        retry.value(
            &json!({"type":"response.created", "response":{"id":"fixture","status":"in_progress"}}),
        );
        assert_eq!(budget.deadline(), deadline);
        retry.value(&json!({"type":"response.output_text.delta", "delta":"fixture"}));
        assert!(budget.deadline().is_none());
        tokio::time::advance(Duration::from_secs(601)).await;
        assert!(!budget.expired(deadline));
        retry.value(
            &json!({"type":"response.completed", "response":{"id":"fixture","status":"completed"}}),
        );
        assert!(retry.succeeded());
    }

    #[tokio::test(start_paused = true)]
    async fn new_websocket_turn_gets_its_own_budget_and_old_tail_cannot_complete_it() {
        let budget = WaitBudget::new(300);
        budget.begin();
        let old = budget.deadline();
        budget.complete(old);
        tokio::time::advance(Duration::from_secs(700)).await;
        budget.begin();
        let current = budget.deadline();
        budget.complete(old);
        assert_eq!(budget.deadline(), current);
        tokio::time::advance(Duration::from_secs(300)).await;
        assert!(budget.expired(current));
        budget.wait().await;
    }
}
