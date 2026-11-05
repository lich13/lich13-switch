// 覆盖电源助手修复、取消与状态冲突。
//! Isolated helper transactions; no native power command or authorization UI runs.
use super::{Service, State, System};
use crate::storage::{AppError, Result};
use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc, Arc, Mutex,
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
    calls: Mutex<Vec<(State, bool, u16)>>,
}

impl MockHelper {
    fn reply(&self, state: State, error: Option<&'static str>) {
        self.replies
            .lock()
            .unwrap()
            .push_back(Reply { state, error });
    }
    fn repair_to(&self, state: State) {
        *self.repaired.lock().unwrap() = Some(state);
    }
    fn repair_count(&self) -> usize {
        self.repairs.load(Ordering::SeqCst)
    }
}

impl System for MockHelper {
    fn read(&self) -> Result<State> {
        Ok(self.current.lock().unwrap().clone())
    }
    fn prepare(&self) -> Result<()> {
        self.repairs.fetch_add(1, Ordering::SeqCst);
        if let Some(gate) = self.repair_gate.lock().unwrap().take() {
            gate.entered.send(()).unwrap();
            gate.release
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(5))
                .expect("fixture repair was not released");
        }
        if let Some(code) = *self.repair_error.lock().unwrap() {
            return Err(AppError::new(code, "fixture authorization failed"));
        }
        if let Some(prepared) = self.repaired.lock().unwrap().take() {
            *self.current.lock().unwrap() = prepared;
        }
        Ok(())
    }
    fn apply(&self, before: &State, enabled: bool, minutes: u16) -> Result<()> {
        assert_eq!(&*self.current.lock().unwrap(), before);
        self.calls
            .lock()
            .unwrap()
            .push((before.clone(), enabled, minutes));
        let reply = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("fixture apply reply");
        *self.current.lock().unwrap() = reply.state;
        reply.error.map_or(Ok(()), |code| {
            Err(AppError::new(code, "fixture helper transaction failed"))
        })
    }
}

fn state(helper: &str, enabled: bool, minutes: u16, ownership: &str, revision: &str) -> State {
    State {
        helper: helper.into(),
        supported: true,
        enabled,
        battery_sleep: minutes,
        revision: revision.into(),
        ownership: ownership.into(),
        external_changed: false,
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
        calls: Mutex::new(Vec::new()),
    });
    let service = Service {
        path: dir.path().join("clamshell-restore.json"),
        system: helper.clone(),
        lock: Mutex::new(()),
    };
    (dir, service, helper)
}

fn legacy(service: &Service) {
    std::fs::write(&service.path, r#"{"version":1,"minutes":137}"#).unwrap();
}

fn assert_legacy(service: &Service) {
    assert_eq!(
        std::fs::read_to_string(&service.path).unwrap(),
        r#"{"version":1,"minutes":137}"#
    );
}

#[test]
fn missing_and_needs_repair_authorize_once_then_apply_the_original_intent() {
    for helper_status in ["missing", "needsRepair"] {
        for (enabled, minutes, ownership) in [
            (false, 7, "none"),
            (true, 0, "application"),
            (true, 9, "external"),
        ] {
            let original = state(helper_status, enabled, minutes, ownership, "fixture-before");
            let prepared = state("ready", enabled, minutes, ownership, "fixture-prepared");
            let actual = state(
                "ready",
                !enabled,
                if enabled { minutes } else { 0 },
                if enabled { "none" } else { "application" },
                "fixture-after",
            );
            let (_dir, service, helper) = fixture(original.clone());
            legacy(&service);
            helper.repair_to(prepared.clone());
            helper.reply(actual.clone(), None);

            assert_eq!(service.set(!enabled, &original.revision).unwrap(), actual);
            assert_eq!(helper.repair_count(), 1);
            assert_eq!(
                *helper.calls.lock().unwrap(),
                [(prepared, !enabled, minutes)]
            );
            assert!(!service.path.exists());
        }
    }
}

#[test]
fn cancelled_authorization_and_failed_repair_preserve_power_and_legacy_record() {
    for helper_status in ["missing", "needsRepair"] {
        for code in ["POWER_AUTH", "POWER_HELPER"] {
            let original = state(helper_status, false, 7, "none", "fixture-before");
            let (_dir, service, helper) = fixture(original.clone());
            legacy(&service);
            *helper.repair_error.lock().unwrap() = Some(code);
            assert_eq!(
                service.set(true, &original.revision).unwrap_err().code,
                code
            );
            assert_eq!(service.state().unwrap(), original);
            assert_eq!(helper.repair_count(), 1);
            assert!(helper.calls.lock().unwrap().is_empty());
            assert_legacy(&service);
        }
    }
}

#[test]
fn repair_returning_an_unavailable_helper_never_applies_or_prompts_twice() {
    let original = state("missing", false, 7, "none", "fixture-before");
    let (_dir, service, helper) = fixture(original.clone());
    helper.repair_to(state(
        "needsRepair",
        false,
        7,
        "none",
        "fixture-repair-failed",
    ));
    assert_eq!(
        service.set(true, &original.revision).unwrap_err().code,
        "POWER_HELPER"
    );
    assert_eq!(helper.repair_count(), 1);
    assert!(helper.calls.lock().unwrap().is_empty());
}

#[test]
fn external_power_changes_during_repair_stop_before_apply() {
    for changed in ["enabled", "minutes", "supported", "externalChanged"] {
        let original = state("needsRepair", false, 7, "none", "fixture-before");
        let mut prepared = state("ready", false, 7, "none", "fixture-external");
        match changed {
            "enabled" => prepared.enabled = true,
            "minutes" => prepared.battery_sleep = 19,
            "supported" => prepared.supported = false,
            _ => prepared.external_changed = true,
        }
        let (_dir, service, helper) = fixture(original.clone());
        legacy(&service);
        helper.repair_to(prepared.clone());
        assert_eq!(
            service.set(true, &original.revision).unwrap_err().code,
            "CONFLICT",
            "{changed}"
        );
        assert_eq!(service.state().unwrap(), prepared);
        assert_eq!(helper.repair_count(), 1);
        assert!(helper.calls.lock().unwrap().is_empty());
        assert_legacy(&service);
    }
}

#[test]
fn duplicate_click_during_authorization_is_busy_without_another_repair() {
    let original = state("missing", false, 7, "none", "fixture-before");
    let prepared = state("ready", false, 7, "none", "fixture-prepared");
    let actual = state("ready", true, 0, "application", "fixture-after");
    let (_dir, service, helper) = fixture(original.clone());
    helper.repair_to(prepared);
    helper.reply(actual.clone(), None);
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    *helper.repair_gate.lock().unwrap() = Some(RepairGate {
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
    assert_eq!(helper.calls.lock().unwrap().len(), 1);
}

#[test]
fn ready_helper_is_reused_without_authorization() {
    let original = state("ready", false, 7, "none", "fixture-before");
    let actual = state("ready", true, 0, "application", "fixture-after");
    let (_dir, service, helper) = fixture(original.clone());
    helper.reply(actual.clone(), None);
    assert_eq!(service.set(true, &original.revision).unwrap(), actual);
    assert_eq!(helper.repair_count(), 0);
    assert_eq!(*helper.calls.lock().unwrap(), [(original, true, 7)]);
}

#[test]
fn authorization_expiring_at_apply_repairs_only_once_even_if_retry_fails() {
    for expired_code in ["POWER_AUTH", "POWER_HELPER"] {
        for retry_error in [None, Some(expired_code)] {
            let original = state("ready", false, 7, "none", "fixture-before");
            let unavailable = state("needsRepair", false, 7, "none", "fixture-expired");
            let prepared = state("ready", false, 7, "none", "fixture-prepared");
            let actual = state("ready", true, 0, "application", "fixture-after");
            let (_dir, service, helper) = fixture(original.clone());
            legacy(&service);
            helper.reply(unavailable, Some(expired_code));
            helper.repair_to(prepared.clone());
            helper.reply(
                if retry_error.is_some() {
                    prepared.clone()
                } else {
                    actual.clone()
                },
                retry_error,
            );
            let result = service.set(true, &original.revision);
            if let Some(code) = retry_error {
                assert_eq!(result.unwrap_err().code, code);
                assert_legacy(&service);
            } else {
                assert_eq!(result.unwrap(), actual);
                assert!(!service.path.exists());
            }
            assert_eq!(helper.repair_count(), 1);
            assert_eq!(
                *helper.calls.lock().unwrap(),
                [(original, true, 7), (prepared, true, 7)]
            );
        }
    }
}

#[test]
fn already_repaired_toggle_does_not_repair_again_when_apply_loses_authorization() {
    for code in ["POWER_AUTH", "POWER_HELPER"] {
        let original = state("missing", false, 7, "none", "fixture-before");
        let prepared = state("ready", false, 7, "none", "fixture-prepared");
        let (_dir, service, helper) = fixture(original.clone());
        legacy(&service);
        helper.repair_to(prepared.clone());
        helper.reply(prepared, Some(code));
        assert_eq!(
            service.set(true, &original.revision).unwrap_err().code,
            code
        );
        assert_eq!(helper.repair_count(), 1);
        assert_eq!(helper.calls.lock().unwrap().len(), 1);
        assert_legacy(&service);
    }
}

#[test]
fn ready_ownership_change_during_authorization_repair_stops_the_retry() {
    let original = state("ready", true, 0, "application", "fixture-before");
    let external = state("ready", true, 0, "external", "fixture-external");
    let (_dir, service, helper) = fixture(original.clone());
    legacy(&service);
    helper.reply(original.clone(), Some("POWER_HELPER"));
    helper.repair_to(external.clone());
    assert_eq!(
        service.set(false, &original.revision).unwrap_err().code,
        "CONFLICT"
    );
    assert_eq!(service.state().unwrap(), external);
    assert_eq!(helper.repair_count(), 1);
    assert_eq!(*helper.calls.lock().unwrap(), [(original, false, 0)]);
    assert_legacy(&service);
}

#[test]
fn non_authorization_apply_errors_are_returned_without_retry_or_rollback() {
    for code in ["POWER", "CONFLICT", "FIXTURE_UNKNOWN"] {
        let original = state("ready", false, 7, "none", "fixture-before");
        let mut external = state("ready", false, 19, "external", "fixture-external");
        external.external_changed = true;
        let (_dir, service, helper) = fixture(original.clone());
        legacy(&service);
        helper.reply(external.clone(), Some(code));
        assert_eq!(
            service.set(true, &original.revision).unwrap_err().code,
            code
        );
        assert_eq!(service.state().unwrap(), external);
        assert_eq!(helper.repair_count(), 0);
        assert_eq!(*helper.calls.lock().unwrap(), [(original, true, 7)]);
        assert_legacy(&service);
    }
}

#[test]
fn cancelled_reauthorization_after_expired_apply_does_not_retry_the_toggle() {
    let original = state("ready", false, 7, "none", "fixture-before");
    let (_dir, service, helper) = fixture(original.clone());
    legacy(&service);
    helper.reply(original.clone(), Some("POWER_AUTH"));
    *helper.repair_error.lock().unwrap() = Some("POWER_AUTH");
    assert_eq!(
        service.set(true, &original.revision).unwrap_err().code,
        "POWER_AUTH"
    );
    assert_eq!(service.state().unwrap(), original);
    assert_eq!(helper.repair_count(), 1);
    assert_eq!(helper.calls.lock().unwrap().len(), 1);
    assert_legacy(&service);
}

#[test]
fn mismatched_state_after_apply_never_reports_a_successful_toggle() {
    let original = state("ready", false, 7, "none", "fixture-before");
    let (_dir, service, helper) = fixture(original.clone());
    legacy(&service);
    helper.reply(original.clone(), None);
    assert_eq!(
        service.set(true, &original.revision).unwrap_err().code,
        "POWER"
    );
    assert_eq!(helper.repair_count(), 0);
    assert_eq!(helper.calls.lock().unwrap().len(), 1);
    assert_legacy(&service);
}

#[test]
fn disable_if_enabled_turns_off_active_clamshell_and_repairs_when_needed() {
    let enabled = state("ready", true, 0, "application", "fixture-enabled");
    let disabled = state("ready", false, 5, "none", "fixture-disabled");
    let (_dir, service, helper) = fixture(enabled.clone());
    helper.reply(disabled.clone(), None);

    assert_eq!(service.disable_if_enabled(|| true).unwrap(), Some(disabled));
    assert_eq!(*helper.calls.lock().unwrap(), [(enabled, false, 0)]);
    assert_eq!(helper.repair_count(), 0);

    assert_eq!(service.disable_if_enabled(|| true).unwrap(), None);

    for not_ready in ["missing", "needsRepair"] {
        let unready = state(not_ready, true, 0, "application", "fixture-unready");
        let (_dir, service, helper) = fixture(unready);
        helper.repair_to(state("ready", true, 0, "application", "fixture-repaired"));
        helper.reply(state("ready", false, 5, "none", "fixture-disabled"), None);
        assert!(
            !service
                .disable_if_enabled(|| true)
                .unwrap()
                .unwrap()
                .enabled
        );
        assert_eq!(helper.repair_count(), 1);
        assert_eq!(helper.calls.lock().unwrap().len(), 1);
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
        let initial = state("missing", true, 0, "external", "fixture-initial");
        let (_dir, service, helper) = fixture(initial.clone());
        *helper.repair_error.lock().unwrap() = Some(code);
        assert_eq!(service.disable_if_enabled(|| true).unwrap_err().code, code);
        assert_eq!(service.state().unwrap(), initial);
        assert_eq!(helper.repair_count(), 1);
        assert!(helper.calls.lock().unwrap().is_empty());
        *helper.repair_error.lock().unwrap() = None;
        helper.repair_to(state("ready", true, 0, "external", "fixture-repaired"));
        helper.reply(state("ready", false, 5, "none", "fixture-disabled"), None);
        assert!(!service.set(false, &initial.revision).unwrap().enabled);
        assert_eq!(helper.repair_count(), 2);
    }
}

#[test]
fn thawing_during_authorization_prevents_automatic_close() {
    let initial = state("missing", true, 0, "application", "fixture-initial");
    let (_dir, service, helper) = fixture(initial);
    let prepared = state("ready", true, 0, "application", "fixture-repaired");
    helper.repair_to(prepared.clone());
    assert!(service
        .disable_if_enabled(|| helper.repair_count() == 0)
        .unwrap()
        .is_none());
    assert_eq!(service.state().unwrap(), prepared);
    assert_eq!(helper.repair_count(), 1);
    assert!(helper.calls.lock().unwrap().is_empty());
}

#[test]
fn authenticated_helper_state_preserves_its_physical_and_ownership_fields() {
    let value = serde_json::json!({"supported":true,"enabled":true,"batterySleep":17,"ownership":"external","externalChanged":true});
    let state = super::helper_state(&value).unwrap();
    assert!(state.supported && state.enabled && state.external_changed);
    assert_eq!(state.battery_sleep, 17);
    assert_eq!(state.ownership, "external");
    assert_eq!(state.helper, "ready");
    assert_eq!(
        state.revision,
        crate::storage::digest(value.to_string().as_bytes())
    );
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
