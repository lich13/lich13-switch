//! RPM work drained by the existing admission persistence thread.
use super::Shared;
use crate::gateway::rpm::{Charge, Ledger, Settlement, Stamp};
use tokio::sync::oneshot;

#[derive(Default)]
pub(super) struct PendingRpm {
    pub dirty: bool,
    pub receipts: Vec<Receipt>,
    pub settlements: Vec<Settled>,
}
pub(super) struct Receipt {
    pub id: String,
    pub charge: Option<Charge>,
    pub limited: bool,
    pub sender: oneshot::Sender<bool>,
}
pub(super) struct Settled {
    id: String,
    token: String,
    result: Settlement,
    at: Stamp,
}
impl Settled {
    pub fn new(id: String, token: String, result: Settlement) -> Self {
        Self {
            id,
            token,
            result,
            at: Stamp::now(),
        }
    }
    fn apply(&self, ledger: &mut Ledger) {
        ledger.settle_at(&self.id, &self.token, self.result, self.at);
    }
}

#[cfg(test)]
pub(super) struct WriteGate {
    pub entered: oneshot::Sender<()>,
    pub resume: std::sync::mpsc::Receiver<()>,
}
pub(super) fn flush(shared: &Shared) {
    let (mut snapshot, path, pending, failed) = {
        let mut state = shared.state.lock().unwrap();
        if !state.rpm_writes.dirty {
            return;
        }
        let pending = std::mem::take(&mut state.rpm_writes);
        (
            state.rpm.clone(),
            state.rpm_path.clone(),
            pending,
            state.rpm_error,
        )
    };
    for settled in &pending.settlements {
        settled.apply(&mut snapshot);
    }
    // A corrupt or previously failed ledger is never replaced with an empty
    // recovery. New requests with a finite limit remain closed until restart.
    #[cfg(test)]
    {
        let gate = shared.rpm_write_gate.lock().unwrap().take();
        if let Some(gate) = gate {
            let _ = gate.entered.send(());
            let _ = gate.resume.recv();
        }
        if !failed {
            shared
                .rpm_writes
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }
    let saved = !failed && snapshot.save(&path).is_ok();
    {
        let mut state = shared.state.lock().unwrap();
        if saved {
            // Merge only the acknowledged releases. Starts, backoff and config
            // changes arriving during this write already live in state.rpm.
            for settled in &pending.settlements {
                settled.apply(&mut state.rpm);
            }
        } else {
            state.rpm_error = true;
            for receipt in &pending.receipts {
                if receipt.limited {
                    if let Some(charge) = &receipt.charge {
                        state.rpm.retract(&receipt.id, charge);
                    }
                }
            }
        }
    }
    for receipt in pending.receipts {
        let _ = receipt.sender.send(saved);
    }
    shared.wake.notify_waiters();
    let _ = shared.events.send(());
}
