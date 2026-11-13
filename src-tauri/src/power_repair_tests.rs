// 覆盖电源助手修复、取消与状态冲突。
//! Isolated helper transactions; no native power command or authorization UI runs.
use super::{Service, State, System};
use crate::storage::{AppError, Result};
use parking_lot::Mutex;
use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc, Arc,
    },
    time::Duration,
};

struct Reply {
    state: State,
    error: Option<&'static str>,
}

struct RepairGate {
    entered: mpsc::Sender<()>,
    release: Mutex<mpsc::Receiver<()>>,
}

struct MockHelper {
    current: Mutex<State>,
    repaired: Mutex<Option<State>>,
    repair_error: Mutex<Option<&'static str>>,
    repair_gate: Mutex<Option<RepairGate>>,
    replies: Mutex<VecDeque<Reply>>,
    repairs: AtomicUsize,
    reads: AtomicUsize,
    calls: Mutex<Vec<bool>>,
}

impl MockHelper {
    fn reply(&self, state: State, error: Option<&'static str>) {
        self.replies.lock().push_back(Reply { state, error });
    }
    fn repair_to(&self, state: State) {
        *self.repaired.lock() = Some(state);
    }
    fn repair_count(&self) -> usize {
        self.repairs.load(Ordering::SeqCst)
    }
}

impl System for MockHelper {
    fn read(&self) -> Result<State> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        Ok(self.current.lock().clone())
    }
    fn prepare(&self) -> Result<()> {
        self.repairs.fetch_add(1, Ordering::SeqCst);
        if let Some(gate) = self.repair_gate.lock().take() {
            gate.entered.send(()).unwrap();
            gate.release
                .lock()
                .recv_timeout(Duration::from_secs(5))
                .expect("fixture repair was not released");
        }
        if let Some(code) = *self.repair_error.lock() {
            return Err(AppError::new(code, "fixture authorization failed"));
        }
        if let Some(prepared) = self.repaired.lock().take() {
            *self.current.lock() = prepared;
        }
        Ok(())
    }
    fn apply(&self, enabled: bool) -> Result<State> {
        self.calls.lock().push(enabled);
        let reply = self
            .replies
            .lock()
            .pop_front()
            .expect("fixture apply reply");
        *self.current.lock() = reply.state.clone();
        reply.error.map_or(Ok(reply.state), |code| {
            Err(AppError::new(code, "fixture helper transaction failed"))
        })
    }
}

fn state(helper: &str, enabled: bool, minutes: u16, revision: &str) -> State {
    State {
        helper: helper.into(),
        supported: true,
        enabled,
        battery_sleep: minutes,
        revision: revision.into(),
    }
}

fn fixture(initial: State) -> (tempfile::TempDir, Service, Arc<MockHelper>) {
    let dir = tempfile::tempdir().unwrap();
    let helper = Arc::new(MockHelper {
        current: Mutex::new(initial),
        repaired: Mutex::new(None),
        repair_error: Mutex::new(None),
        repair_gate: Mutex::new(None),
        replies: Mutex::new(VecDeque::new()),
        repairs: AtomicUsize::new(0),
        reads: AtomicUsize::new(0),
        calls: Mutex::new(Vec::new()),
    });
    let service = Service {
        system: helper.clone(),
        lock: Mutex::new(()),
    };
    (dir, service, helper)
}

#[test]
fn missing_and_needs_repair_authorize_once_then_apply_the_original_intent() {
    for helper_status in ["missing", "needsRepair"] {
        for (enabled, minutes) in [(false, 7), (true, 0), (true, 9)] {
            let original = state(helper_status, enabled, minutes, "fixture-before");
            let prepared = state("ready", enabled, minutes, "fixture-prepared");
            let actual = state(
                "ready",
                !enabled,
                if enabled { minutes } else { 0 },
                "fixture-after",
            );
            let (_dir, service, helper) = fixture(original.clone());
            helper.repair_to(prepared.clone());
            helper.reply(actual.clone(), None);

            assert_eq!(service.set(!enabled, &original.revision).unwrap(), actual);
            assert_eq!(helper.repair_count(), 1);
            assert_eq!(*helper.calls.lock(), [!enabled]);
        }
    }
}

#[test]
fn cancelled_authorization_and_failed_repair_preserve_power() {
    for helper_status in ["missing", "needsRepair"] {
        for code in ["POWER_AUTH", "POWER_HELPER"] {
            let original = state(helper_status, false, 7, "fixture-before");
            let (_dir, service, helper) = fixture(original.clone());
            *helper.repair_error.lock() = Some(code);
            assert_eq!(
                service.set(true, &original.revision).unwrap_err().code,
                code
            );
            assert_eq!(service.state().unwrap(), original);
            assert_eq!(helper.repair_count(), 1);
            assert!(helper.calls.lock().is_empty());
        }
    }
}

#[test]
fn repair_returning_an_unavailable_helper_never_applies_or_prompts_twice() {
    let original = state("missing", false, 7, "fixture-before");
    let (_dir, service, helper) = fixture(original.clone());
    helper.repair_to(state("needsRepair", false, 7, "fixture-repair-failed"));
    assert_eq!(
        service.set(true, &original.revision).unwrap_err().code,
        "POWER_HELPER"
    );
    assert_eq!(helper.repair_count(), 1);
    assert!(helper.calls.lock().is_empty());
}

#[test]
fn external_power_changes_during_repair_stop_before_apply() {
    for changed in ["enabled", "minutes", "supported"] {
        let original = state("needsRepair", false, 7, "fixture-before");
        let mut prepared = state("ready", false, 7, "fixture-external");
        match changed {
            "enabled" => prepared.enabled = true,
            "minutes" => prepared.battery_sleep = 19,
            _ => prepared.supported = false,
        }
        let (_dir, service, helper) = fixture(original.clone());
        helper.repair_to(prepared.clone());
        assert_eq!(
            service.set(true, &original.revision).unwrap_err().code,
            "CONFLICT",
            "{changed}"
        );
        assert_eq!(service.state().unwrap(), prepared);
        assert_eq!(helper.repair_count(), 1);
        assert!(helper.calls.lock().is_empty());
    }
}

#[test]
fn duplicate_click_during_authorization_is_busy_without_another_repair() {
    let original = state("missing", false, 7, "fixture-before");
    let prepared = state("ready", false, 7, "fixture-prepared");
    let actual = state("ready", true, 0, "fixture-after");
    let (_dir, service, helper) = fixture(original.clone());
    helper.repair_to(prepared);
    helper.reply(actual.clone(), None);
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    *helper.repair_gate.lock() = Some(RepairGate {
        entered: entered_tx,
        release: Mutex::new(release_rx),
    });
    let service = Arc::new(service);
    let worker_service = service.clone();
    let revision = original.revision.clone();
    let worker = std::thread::spawn(move || worker_service.set(true, &revision));
    entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let duplicate = service.set(true, &original.revision);
    release_tx.send(()).unwrap();
    let result = worker.join().unwrap();
    assert_eq!(duplicate.unwrap_err().code, "BUSY");
    assert_eq!(result.unwrap(), actual);
    assert_eq!(helper.repair_count(), 1);
    assert_eq!(helper.calls.lock().len(), 1);
}

#[test]
fn ready_helper_is_reused_without_authorization() {
    let original = state("ready", false, 7, "fixture-before");
    let actual = state("ready", true, 0, "fixture-after");
    let (_dir, service, helper) = fixture(original.clone());
    helper.reply(actual.clone(), None);
    assert_eq!(service.set(true, &original.revision).unwrap(), actual);
    assert_eq!(helper.repair_count(), 0);
    assert_eq!(helper.reads.load(Ordering::SeqCst), 1);
    assert_eq!(*helper.calls.lock(), [true]);
}

#[test]
fn apply_errors_are_returned_without_retry_or_rollback() {
    for code in [
        "POWER",
        "POWER_AUTH",
        "POWER_HELPER",
        "CONFLICT",
        "FIXTURE_UNKNOWN",
    ] {
        let original = state("ready", false, 7, "fixture-before");
        let external = state("ready", false, 19, "fixture-external");
        let (_dir, service, helper) = fixture(original.clone());
        helper.reply(external.clone(), Some(code));
        assert_eq!(
            service.set(true, &original.revision).unwrap_err().code,
            code
        );
        assert_eq!(service.state().unwrap(), external);
        assert_eq!(helper.repair_count(), 0);
        assert_eq!(*helper.calls.lock(), [true]);
    }
}

#[test]
fn disable_if_enabled_turns_off_active_clamshell_and_repairs_when_needed() {
    let enabled = state("ready", true, 0, "fixture-enabled");
    let disabled = state("ready", false, 5, "fixture-disabled");
    let (_dir, service, helper) = fixture(enabled.clone());
    helper.reply(disabled.clone(), None);

    assert_eq!(service.disable_if_enabled(|| true).unwrap(), Some(disabled));
    assert_eq!(*helper.calls.lock(), [false]);
    assert_eq!(helper.repair_count(), 0);

    assert_eq!(service.disable_if_enabled(|| true).unwrap(), None);

    for not_ready in ["missing", "needsRepair"] {
        let unready = state(not_ready, true, 0, "fixture-unready");
        let (_dir, service, helper) = fixture(unready);
        helper.repair_to(state("ready", true, 0, "fixture-repaired"));
        helper.reply(state("ready", false, 5, "fixture-disabled"), None);
        assert!(
            !service
                .disable_if_enabled(|| true)
                .unwrap()
                .unwrap()
                .enabled
        );
        assert_eq!(helper.repair_count(), 1);
        assert_eq!(helper.calls.lock().len(), 1);
    }
}

#[test]
fn automatic_close_waits_for_drain_and_attempts_once_until_thawed() {
    let mut attempted = false;
    for (frozen, draining, expected) in [
        (false, 0, false),
        (true, 1, false),
        (true, 0, true),
        (true, 0, false),
        (false, 0, false),
        (true, 0, true),
    ] {
        assert_eq!(
            super::close_after_freeze(&mut attempted, frozen, draining),
            expected
        );
    }
}

#[test]
fn automatic_close_keeps_cancelled_and_failed_authorization_available_for_manual_retry() {
    for code in ["POWER_AUTH", "POWER_HELPER"] {
        let initial = state("missing", true, 0, "fixture-initial");
        let (_dir, service, helper) = fixture(initial.clone());
        *helper.repair_error.lock() = Some(code);
        assert_eq!(service.disable_if_enabled(|| true).unwrap_err().code, code);
        assert_eq!(service.state().unwrap(), initial);
        assert_eq!(helper.repair_count(), 1);
        assert!(helper.calls.lock().is_empty());
        *helper.repair_error.lock() = None;
        helper.repair_to(state("ready", true, 0, "fixture-repaired"));
        helper.reply(state("ready", false, 5, "fixture-disabled"), None);
        assert!(!service.set(false, &initial.revision).unwrap().enabled);
        assert_eq!(helper.repair_count(), 2);
    }
}

#[test]
fn thawing_during_authorization_prevents_automatic_close() {
    let initial = state("missing", true, 0, "fixture-initial");
    let (_dir, service, helper) = fixture(initial);
    let prepared = state("ready", true, 0, "fixture-repaired");
    helper.repair_to(prepared.clone());
    assert!(service
        .disable_if_enabled(|| helper.repair_count() == 0)
        .unwrap()
        .is_none());
    assert_eq!(service.state().unwrap(), prepared);
    assert_eq!(helper.repair_count(), 1);
    assert!(helper.calls.lock().is_empty());
}

#[test]
fn authenticated_helper_state_is_validated() {
    let value = serde_json::json!({"supported":true,"enabled":true,"batterySleep":17});
    let state = super::helper_state(&value).unwrap();
    assert_eq!(state, State::new(true, true, 17));
    assert_eq!(state.helper, "ready");
    for (key, invalid) in [
        ("supported", serde_json::Value::Null),
        ("enabled", serde_json::json!("false")),
        ("batterySleep", serde_json::json!(1441)),
    ] {
        let mut bad = value.clone();
        bad[key] = invalid;
        assert!(super::helper_state(&bad).is_err());
    }
}
