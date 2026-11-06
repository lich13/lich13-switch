//! A shared dispatch gate. Only live requests are retained; no payload or
//! credential is copied into the state sent to windows.
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{sync::watch, time::Instant};

#[derive(Clone, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct View {
    pub frozen: bool,
    pub draining: usize,
    pub receiving: usize,
    pub waiting: usize,
    pub headers: usize,
    pub streaming: usize,
    pub finishing: usize,
    pub queued: usize,
}
#[derive(Clone, Copy, PartialEq)]
enum Phase {
    Receiving,
    Waiting,
    Headers,
    Streaming,
    Finishing,
    Done,
}
struct Request {
    phase: Phase,
    paused: Duration,
    paused_since: Option<Instant>,
}
impl Request {
    fn paused_for(&self) -> Duration {
        self.paused + self.paused_since.map_or(Duration::ZERO, |at| at.elapsed())
    }
    fn resume_clock(&mut self) {
        self.paused = self.paused_for();
        self.paused_since = None;
    }
}
#[derive(Default)]
struct State {
    frozen: bool,
    next: u64,
    requests: BTreeMap<u64, Request>,
}
struct Shared {
    state: Mutex<State>,
    changes: watch::Sender<()>,
}
#[derive(Clone)]
pub struct Gate(Arc<Shared>);
impl Default for Gate {
    fn default() -> Self {
        Self(Arc::new(Shared {
            state: Mutex::default(),
            changes: watch::channel(()).0,
        }))
    }
}
impl Gate {
    fn view_locked(s: &State) -> View {
        let mut view = View {
            frozen: s.frozen,
            ..Default::default()
        };
        for r in s.requests.values() {
            match r.phase {
                Phase::Receiving => view.receiving += 1,
                Phase::Waiting if s.frozen => view.queued += 1,
                Phase::Waiting => view.waiting += 1,
                Phase::Headers => view.headers += 1,
                Phase::Streaming => view.streaming += 1,
                Phase::Finishing => view.finishing += 1,
                Phase::Done => {}
            }
        }
        view.draining = view.receiving + view.headers + view.streaming + view.finishing;
        view
    }
    pub fn view(&self) -> View {
        Self::view_locked(&self.0.state.lock().unwrap())
    }
    pub fn is_frozen(&self) -> bool {
        self.0.state.lock().unwrap().frozen
    }
    pub fn subscribe(&self) -> watch::Receiver<()> {
        self.0.changes.subscribe()
    }
    pub fn set(&self, frozen: bool) -> crate::storage::Result<View> {
        let mut s = self.0.state.lock().unwrap();
        if !frozen && s.frozen && Self::view_locked(&s).draining != 0 {
            return Err(crate::usage::model::failure("等待收尾完成后才能解冻"));
        }
        if s.frozen != frozen {
            s.frozen = frozen;
            for r in s.requests.values_mut() {
                if frozen && r.phase == Phase::Waiting {
                    r.paused_since = Some(Instant::now());
                } else if !frozen {
                    r.resume_clock();
                }
            }
            self.0.changes.send_replace(());
        }
        Ok(Self::view_locked(&s))
    }
    pub fn request(&self) -> Ticket {
        let mut s = self.0.state.lock().unwrap();
        s.next += 1;
        let id = s.next;
        s.requests.insert(
            id,
            Request {
                phase: Phase::Receiving,
                paused: Duration::ZERO,
                paused_since: None,
            },
        );
        self.0.changes.send_replace(());
        Ticket(Arc::new(Entry {
            gate: self.clone(),
            id,
        }))
    }
}
struct Entry {
    gate: Gate,
    id: u64,
}
impl Drop for Entry {
    fn drop(&mut self) {
        self.gate.0.state.lock().unwrap().requests.remove(&self.id);
        self.gate.0.changes.send_replace(());
    }
}
#[derive(Clone)]
pub struct Ticket(Arc<Entry>);
pub async fn changed(receiver: &mut Option<watch::Receiver<()>>) {
    if let Some(receiver) = receiver {
        let _ = receiver.changed().await;
    } else {
        std::future::pending::<()>().await;
    }
}
impl Ticket {
    fn phase(&self, phase: Phase) {
        let mut s = self.0.gate.0.state.lock().unwrap();
        let frozen = s.frozen;
        if let Some(r) = s.requests.get_mut(&self.0.id) {
            if r.phase == phase || r.phase == Phase::Done {
                return;
            }
            r.resume_clock();
            r.phase = phase;
            if frozen && phase == Phase::Waiting {
                r.paused_since = Some(Instant::now());
            }
            self.0.gate.0.changes.send_replace(());
        }
    }
    pub fn waiting(&self) {
        self.phase(Phase::Waiting);
    }
    pub fn streaming(&self) {
        self.phase(Phase::Streaming);
    }
    pub fn finishing(&self) {
        self.phase(Phase::Finishing);
    }
    pub fn finish(&self) {
        self.phase(Phase::Done);
    }
    pub fn paused(&self) -> bool {
        let s = self.0.gate.0.state.lock().unwrap();
        s.frozen
            && s.requests
                .get(&self.0.id)
                .is_some_and(|r| r.phase == Phase::Waiting)
    }
    pub fn paused_for(&self) -> Duration {
        self.0
            .gate
            .0
            .state
            .lock()
            .unwrap()
            .requests
            .get(&self.0.id)
            .map_or(Duration::ZERO, Request::paused_for)
    }
    pub fn subscribe(&self) -> watch::Receiver<()> {
        self.0.gate.subscribe()
    }
    /// Linearizes admission against the freeze click. A granted request drains;
    /// a losing request remains queued without reserving upstream capacity.
    pub fn dispatch(&self) -> bool {
        let mut s = self.0.gate.0.state.lock().unwrap();
        if s.frozen {
            return false;
        }
        let Some(r) = s.requests.get_mut(&self.0.id) else {
            return false;
        };
        r.resume_clock();
        r.phase = Phase::Headers;
        self.0.gate.0.changes.send_replace(());
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test(start_paused = true)]
    async fn drain_blocks_thaw_and_waiting_time_is_excluded() {
        let gate = Gate::default();
        let current = gate.request();
        current.waiting();
        assert!(current.dispatch());
        current.streaming();
        let queued = gate.request();
        queued.waiting();
        let view = gate.set(true).unwrap();
        assert_eq!((view.draining, view.streaming, view.queued), (1, 1, 1));
        assert!(!queued.dispatch());
        assert!(gate.set(false).is_err());
        tokio::time::advance(Duration::from_secs(3600)).await;
        assert_eq!(queued.paused_for(), Duration::from_secs(3600));
        assert_eq!(current.paused_for(), Duration::ZERO);
        current.finishing();
        assert_eq!(gate.view().draining, 1);
        current.finish();
        assert_eq!(gate.view().draining, 0);
        gate.set(false).unwrap();
        assert!(queued.dispatch());
        drop(queued);
        assert_eq!(gate.view().draining, 0);
    }
    #[test]
    fn cancelled_waiter_is_removed_and_idle_connections_are_not_requests() {
        let gate = Gate::default();
        gate.set(true).unwrap();
        let request = gate.request();
        assert_eq!(gate.view().draining, 1);
        request.waiting();
        assert_eq!(gate.view().queued, 1);
        drop(request);
        assert_eq!(gate.view().queued, 0);
        assert_eq!(gate.view().draining, 0);
    }
}
