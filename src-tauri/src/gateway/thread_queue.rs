//! Live Codex turns, ordered across transports. Payloads stay with their requests.
use super::admission::{Rejected, Scheduler, Waiting};
use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
};
use tokio::sync::watch;

pub(super) fn reserve(
    g: &super::Gateway,
    headers: &hyper::HeaderMap,
    eligible: bool,
    max_waiting: usize,
) -> Result<Option<Ticket>, Rejected> {
    if !eligible || g.client_id() != super::ClientId::Codex {
        return Ok(None);
    }
    let (thread, identity) = super::compaction::identity(headers);
    if identity != crate::usage::model::ThreadIdentity::Reliable {
        return Ok(None);
    }
    thread
        .map(|thread| {
            g.0.thread_queue
                .enqueue(thread, &g.0.admission, max_waiting)
        })
        .transpose()
}

#[derive(Default)]
struct State {
    next: u64,
    threads: HashMap<String, VecDeque<u64>>,
}
struct Shared {
    state: Mutex<State>,
    changes: watch::Sender<()>,
}
#[derive(Clone)]
pub(super) struct Queue(Arc<Shared>);
impl Default for Queue {
    fn default() -> Self {
        Self(Arc::new(Shared {
            state: Mutex::default(),
            changes: watch::channel(()).0,
        }))
    }
}
struct Entry {
    queue: Queue,
    thread: String,
    id: u64,
    waiting: Mutex<Option<Waiting>>,
}
impl Drop for Entry {
    fn drop(&mut self) {
        let mut s = self.queue.0.state.lock().unwrap();
        if let Some(turns) = s.threads.get_mut(&self.thread) {
            turns.retain(|id| *id != self.id);
            if turns.is_empty() {
                s.threads.remove(&self.thread);
            }
        }
        drop(s);
        self.queue.0.changes.send_replace(());
    }
}
#[derive(Clone)]
pub(super) struct Ticket(Arc<Entry>);
impl Queue {
    pub fn enqueue(
        &self,
        thread: String,
        scheduler: &Scheduler,
        max_waiting: usize,
    ) -> Result<Ticket, Rejected> {
        let mut s = self.0.state.lock().unwrap();
        let waiting = s
            .threads
            .contains_key(&thread)
            .then(|| scheduler.thread_wait(max_waiting))
            .transpose()?;
        s.next += 1;
        let id = s.next;
        s.threads.entry(thread.clone()).or_default().push_back(id);
        Ok(Ticket(Arc::new(Entry {
            queue: self.clone(),
            thread,
            id,
            waiting: Mutex::new(waiting),
        })))
    }
}
impl Ticket {
    pub fn ready(&self) -> bool {
        self.0
            .queue
            .0
            .state
            .lock()
            .unwrap()
            .threads
            .get(&self.0.thread)
            .and_then(|v| v.front())
            == Some(&self.0.id)
    }
    pub async fn wait(&self) {
        let mut changes = self.0.queue.0.changes.subscribe();
        loop {
            if self.ready() {
                self.0.waiting.lock().unwrap().take();
                return;
            }
            let _ = changes.changed().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn scheduler(dir: &std::path::Path) -> Scheduler {
        let scheduler = Scheduler::new(tokio::sync::broadcast::channel(8).0, dir.join("rpm.json"));
        scheduler.configure(&[], true);
        scheduler
    }
    #[tokio::test]
    async fn fifo_shares_wait_limit_without_holding_provider_capacity() {
        let dir = tempfile::tempdir().unwrap();
        let scheduler = scheduler(dir.path());
        let queue = Queue::default();
        let first = queue.enqueue("thread-a".into(), &scheduler, 2).unwrap();
        let second = queue.enqueue("thread-a".into(), &scheduler, 2).unwrap();
        let third = queue.enqueue("thread-a".into(), &scheduler, 2).unwrap();
        assert!(matches!(
            queue.enqueue("thread-a".into(), &scheduler, 2),
            Err(Rejected::Full)
        ));
        let other = queue.enqueue("thread-b".into(), &scheduler, 2).unwrap();
        assert!(other.ready());
        assert_eq!(scheduler.counts(), (HashMap::new(), 2));
        assert!(!second.ready());
        assert!(!third.ready());
        drop(second);
        assert_eq!(scheduler.counts().1, 1);
        drop(first);
        third.wait().await;
        assert_eq!(scheduler.counts().1, 0);
        drop(third);
        drop(other);
        assert!(queue.0.state.lock().unwrap().threads.is_empty());
    }
    #[tokio::test(start_paused = true)]
    async fn delivery_ticket_and_frozen_budget_keep_the_turn_until_finish() {
        let dir = tempfile::tempdir().unwrap();
        let scheduler = scheduler(dir.path());
        let queue = Queue::default();
        let gate = super::super::freeze::Gate::default();
        let first = gate.request();
        first.attach_thread(queue.enqueue("thread".into(), &scheduler, 2).unwrap());
        first.waiting();
        assert!(first.dispatch());
        let queued = gate.request();
        queued.waiting();
        let next = queue.enqueue("thread".into(), &scheduler, 2).unwrap();
        queued.attach_thread(next.clone());
        let budget = super::super::wait_budget::WaitBudget::new(300);
        budget.begin();
        budget.attach_traffic(&queued);
        tokio::time::advance(std::time::Duration::from_secs(10)).await;
        gate.set(true).unwrap();
        tokio::time::advance(std::time::Duration::from_secs(600)).await;
        first.finishing();
        assert!(!next.ready());
        first.finish();
        next.wait().await;
        gate.set(false).unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(289), budget.wait())
                .await
                .is_err()
        );
        tokio::time::timeout(std::time::Duration::from_secs(2), budget.wait())
            .await
            .unwrap();
        queued.finish();
        drop(next);
        assert!(queue.0.state.lock().unwrap().threads.is_empty());
    }
}
