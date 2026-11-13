//! Battery clamshell control through the narrowly scoped, authorized power helper.
use crate::errors::code;
use crate::storage::{self, AppError, Result};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct State {
    pub helper: String,
    pub supported: bool,
    pub enabled: bool,
    pub battery_sleep: u16,
    pub revision: String,
}
impl State {
    fn new(supported: bool, enabled: bool, minutes: u16) -> Self {
        Self {
            helper: if supported { "ready" } else { "unsupported" }.into(),
            supported,
            enabled,
            battery_sleep: minutes,
            revision: storage::digest(format!("{supported}:{enabled}:{minutes}").as_bytes()),
        }
    }
}
trait System: Send + Sync {
    fn prepare(&self) -> Result<()> {
        Ok(())
    }
    fn remove(&self) -> Result<()> {
        Err(AppError::new(code::UNSUPPORTED, "此设备不支持电源助手"))
    }
    fn read(&self) -> Result<State>;
    fn apply(&self, enabled: bool) -> Result<State>;
}
struct Native;
#[cfg(test)]
#[path = "power_repair_tests.rs"]
mod repair_tests;
pub struct Service {
    system: Arc<dyn System>,
    lock: Mutex<()>,
}
pub fn close_after_freeze(attempted: &mut bool, frozen: bool, draining: usize) -> bool {
    if !frozen {
        *attempted = false;
    }
    if frozen && draining == 0 && !*attempted {
        *attempted = true;
        true
    } else {
        false
    }
}
impl Service {
    pub fn new() -> Self {
        Self {
            system: Arc::new(Native),
            lock: Mutex::new(()),
        }
    }
    pub fn state(&self) -> Result<State> {
        self.system.read()
    }
    pub fn install(&self) -> Result<State> {
        let _lock = self.lock.lock();
        self.system.prepare()?;
        self.state()
    }
    pub fn remove(&self) -> Result<State> {
        let _lock = self.lock.lock();
        // The helper puts back the state it remembered before removing itself.
        self.system.remove()?;
        self.state()
    }

    pub fn set(&self, enabled: bool, expected: &str) -> Result<State> {
        let _lock = self
            .lock
            .try_lock()
            .ok_or_else(|| AppError::new(code::BUSY, "电源操作正在进行"))?;
        self.set_locked(enabled, expected)
    }
    pub fn disable_if_enabled(&self, still_frozen: impl Fn() -> bool) -> Result<Option<State>> {
        let _lock = self
            .lock
            .try_lock()
            .ok_or_else(|| AppError::new(code::BUSY, "电源操作正在进行"))?;
        let before = self.state()?;
        if !before.supported || !before.enabled {
            return Ok(None);
        }
        let actual = self.apply_locked(before, false, &still_frozen)?;
        Ok((!actual.enabled).then_some(actual))
    }
    fn set_locked(&self, enabled: bool, expected: &str) -> Result<State> {
        let before = self.state()?;
        if !before.supported {
            return Err(AppError::new(code::UNSUPPORTED, "此设备不支持电池合盖控制"));
        }
        if before.revision != expected {
            return Err(AppError::new(code::CONFLICT, "电源状态已变化，请重新操作"));
        }
        self.apply_locked(before, enabled, &|| true)
    }
    fn apply_locked(
        &self,
        before: State,
        enabled: bool,
        allowed: &dyn Fn() -> bool,
    ) -> Result<State> {
        if !allowed() {
            return Ok(before);
        }
        if before.helper != "ready" {
            let prepared = self.repair_for_toggle(&before)?;
            if !allowed() {
                return Ok(prepared);
            }
        }
        // Restoration belongs exclusively to the privileged helper.
        self.system.apply(enabled)
    }
    fn repair_for_toggle(&self, before: &State) -> Result<State> {
        self.system.prepare()?;
        let prepared = self.state()?;
        if before.supported != prepared.supported
            || before.enabled != prepared.enabled
            || before.battery_sleep != prepared.battery_sleep
        {
            return Err(AppError::new(
                code::CONFLICT,
                "修复期间电源状态已变化，请重新操作",
            ));
        }
        if prepared.helper != "ready" {
            return Err(AppError::new(code::POWER_HELPER, "修复后助手仍不可用"));
        }
        Ok(prepared)
    }
}
#[cfg(target_os = "macos")]
fn output(args: &[&str]) -> Result<String> {
    let result = std::process::Command::new("/usr/bin/pmset")
        .args(args)
        .output()
        .map_err(storage::io_error)?;
    if !result.status.success() {
        return Err(AppError::new(code::POWER, "无法读取系统电源状态"));
    }
    String::from_utf8(result.stdout).map_err(|_| AppError::new(code::POWER, "无法读取系统电源状态"))
}
#[cfg(any(target_os = "macos", test))]
fn parse(general: &str, custom: &str) -> Result<State> {
    let flag = general.lines().find_map(|l| {
        let mut words = l.split_whitespace();
        (words.next() == Some("SleepDisabled"))
            .then(|| words.next())
            .flatten()
    });
    if !custom.lines().any(|l| l.trim() == "Battery Power:") {
        return Ok(State::new(false, false, 0));
    }
    let mut battery = false;
    let mut minutes = None;
    for line in custom.lines() {
        if line.trim() == "Battery Power:" {
            battery = true;
            continue;
        }
        if !line.starts_with(char::is_whitespace) {
            battery = false;
        }
        if battery {
            let mut words = line.split_whitespace();
            if words.next() == Some("sleep") {
                minutes = words
                    .next()
                    .and_then(|s| s.parse::<u16>().ok())
                    .filter(|n| *n <= 1440);
            }
        }
    }
    match (flag, minutes) {
        (Some("0" | "1"), Some(n)) => Ok(State::new(true, flag == Some("1"), n)),
        _ => Err(AppError::new(code::POWER, "无法读取合盖休眠状态")),
    }
}
#[cfg(any(target_os = "macos", test))]
pub(crate) fn helper_state(actual: &serde_json::Value) -> Result<State> {
    let supported = actual["supported"]
        .as_bool()
        .ok_or_else(|| AppError::new(code::POWER, "电源状态无效"))?;
    let enabled = actual["enabled"]
        .as_bool()
        .ok_or_else(|| AppError::new(code::POWER, "电源状态无效"))?;
    let minutes = actual["batterySleep"]
        .as_u64()
        .filter(|n| *n <= 1440)
        .ok_or_else(|| AppError::new(code::POWER, "电源状态无效"))? as u16;
    Ok(State::new(supported, enabled, minutes))
}
impl System for Native {
    fn prepare(&self) -> Result<()> {
        #[cfg(target_os = "macos")]
        {
            crate::power_macos::ensure()
        }
        #[cfg(not(target_os = "macos"))]
        {
            Err(AppError::new(code::UNSUPPORTED, "此设备不支持电源助手"))
        }
    }
    fn remove(&self) -> Result<()> {
        #[cfg(target_os = "macos")]
        {
            crate::power_macos::remove()
        }
        #[cfg(not(target_os = "macos"))]
        {
            Err(AppError::new(code::UNSUPPORTED, "此设备不支持电源助手"))
        }
    }
    fn read(&self) -> Result<State> {
        #[cfg(target_os = "macos")]
        {
            let (helper, state) = crate::power_macos::probe();
            if let Some(state) = state {
                return Ok(state);
            }
            let mut state = parse(&output(&["-g"])?, &output(&["-g", "custom"])?)?;
            state.helper = if state.supported {
                helper
            } else {
                "unsupported"
            }
            .into();
            Ok(state)
        }
        #[cfg(not(target_os = "macos"))]
        {
            Ok(State::new(false, false, 0))
        }
    }
    fn apply(&self, enabled: bool) -> Result<State> {
        #[cfg(target_os = "macos")]
        {
            let response =
                crate::power_macos::request(serde_json::json!({"op":"set", "enabled":enabled}))?;
            helper_state(&response["state"])
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = enabled;
            Err(AppError::new(code::UNSUPPORTED, "此设备不支持电池合盖控制"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        collections::VecDeque,
        sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    };

    struct Reply {
        state: State,
        error: Option<&'static str>,
    }
    struct MockHelper {
        current: Mutex<State>,
        replies: Mutex<VecDeque<Reply>>,
        calls: Mutex<Vec<bool>>,
        ready: AtomicBool,
        cancelled: AtomicBool,
        approvals: AtomicUsize,
        prepares: AtomicUsize,
        after_prepare: Mutex<Option<State>>,
        remove_reply: Mutex<Option<Reply>>,
        removals: AtomicUsize,
    }
    impl MockHelper {
        fn reply(&self, state: State, error: Option<&'static str>) {
            self.replies.lock().push_back(Reply { state, error });
        }
    }
    impl System for MockHelper {
        fn read(&self) -> Result<State> {
            let mut state = self.current.lock().clone();
            if !self.ready.load(Ordering::SeqCst) && state.supported {
                state.helper = "missing".into();
            }
            Ok(state)
        }
        fn prepare(&self) -> Result<()> {
            self.prepares.fetch_add(1, Ordering::SeqCst);
            self.approvals.fetch_add(1, Ordering::SeqCst);
            if self.cancelled.load(Ordering::SeqCst) {
                return Err(AppError::new(code::POWER_AUTH, "已取消系统授权"));
            }
            self.ready.store(true, Ordering::SeqCst);
            if let Some(state) = self.after_prepare.lock().take() {
                *self.current.lock() = state;
            }
            Ok(())
        }
        fn apply(&self, enabled: bool) -> Result<State> {
            let mut state = self.current.lock();
            self.calls.lock().push(enabled);
            // The native tests own restoration semantics. These explicit replies
            // verify that Service accepts only the helper's authoritative result.
            let reply = self.replies.lock().pop_front().expect("helper reply");
            *state = reply.state;
            reply.error.map_or_else(
                || Ok(state.clone()),
                |code| Err(AppError::new(code, "助手事务失败")),
            )
        }
        fn remove(&self) -> Result<()> {
            self.removals.fetch_add(1, Ordering::SeqCst);
            let reply = self.remove_reply.lock().take().expect("remove reply");
            *self.current.lock() = reply.state;
            if let Some(code) = reply.error {
                return Err(AppError::new(code, "助手移除失败"));
            }
            self.ready.store(false, Ordering::SeqCst);
            Ok(())
        }
    }

    fn state(enabled: bool, minutes: u16) -> State {
        State::new(true, enabled, minutes)
    }
    fn fixture(initial: State) -> (tempfile::TempDir, Service, Arc<MockHelper>) {
        let dir = tempfile::tempdir().unwrap();
        let mut service = Service::new();
        let helper = Arc::new(MockHelper {
            current: Mutex::new(initial),
            replies: Mutex::new(VecDeque::new()),
            calls: Mutex::new(Vec::new()),
            ready: AtomicBool::new(true),
            cancelled: AtomicBool::new(false),
            approvals: AtomicUsize::new(0),
            prepares: AtomicUsize::new(0),
            after_prepare: Mutex::new(None),
            remove_reply: Mutex::new(None),
            removals: AtomicUsize::new(0),
        });
        service.system = helper.clone();
        (dir, service, helper)
    }

    #[test]
    fn helper_result_is_authoritative_for_restored_minutes_including_zero() {
        for minutes in [0, 7] {
            let original = state(false, minutes);
            let owned = state(true, 0);
            let (_dir, service, helper) = fixture(original.clone());
            helper.reply(owned.clone(), None);
            helper.reply(original.clone(), None);
            assert_eq!(service.set(true, &original.revision).unwrap(), owned);
            assert_eq!(service.set(false, &owned.revision).unwrap(), original);
            assert_eq!(helper.calls.lock().as_slice(), &[true, false]);
        }
    }

    #[test]
    fn authorization_cancellation_preserves_state() {
        let mut original = state(false, 3);
        original.helper = "missing".into();
        let (_dir, service, helper) = fixture(original.clone());
        helper.ready.store(false, Ordering::SeqCst);
        helper.cancelled.store(true, Ordering::SeqCst);
        assert_eq!(
            service.set(true, &original.revision).unwrap_err().code,
            code::POWER_AUTH
        );
        assert_eq!(service.state().unwrap(), original);
        assert!(helper.calls.lock().is_empty());
        assert_eq!(helper.approvals.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn daily_toggles_reuse_authorization_but_repair_forces_it() {
        let original = state(false, 9);
        let owned = state(true, 0);
        let (_dir, service, helper) = fixture(original.clone());
        helper.ready.store(false, Ordering::SeqCst);
        for _ in 0..3 {
            helper.reply(owned.clone(), None);
            helper.reply(original.clone(), None);
            service
                .set(true, &service.state().unwrap().revision)
                .unwrap();
            service
                .set(false, &service.state().unwrap().revision)
                .unwrap();
        }
        assert_eq!(helper.approvals.load(Ordering::SeqCst), 1);
        assert_eq!(helper.calls.lock().len(), 6);
        assert_eq!(service.install().unwrap(), original);
        assert_eq!(helper.approvals.load(Ordering::SeqCst), 2);
        assert_eq!(helper.calls.lock().len(), 6);
        helper.ready.store(false, Ordering::SeqCst);
        helper.reply(owned, None);
        service.set(true, &original.revision).unwrap();
        assert_eq!(helper.approvals.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn power_changes_during_first_installation_conflict_without_applying() {
        for (supported, enabled, minutes) in [(false, false, 7), (true, true, 7), (true, false, 9)]
        {
            let mut original = State::new(true, false, 7);
            original.helper = "missing".into();
            let prepared = State::new(supported, enabled, minutes);
            let (_dir, service, helper) = fixture(original.clone());
            helper.ready.store(false, Ordering::SeqCst);
            *helper.after_prepare.lock() = Some(prepared.clone());

            assert_eq!(
                service.set(true, &original.revision).unwrap_err().code,
                code::CONFLICT
            );
            assert_eq!(service.state().unwrap(), prepared);
            assert!(helper.calls.lock().is_empty());
            assert_eq!(helper.prepares.load(Ordering::SeqCst), 1);
        }
    }

    #[test]
    fn stale_revision_does_not_prepare_or_call_helper() {
        let original = state(false, 7);
        let (_dir, service, helper) = fixture(original);
        assert_eq!(service.set(true, "stale").unwrap_err().code, code::CONFLICT);
        assert_eq!(helper.prepares.load(Ordering::SeqCst), 0);
        assert!(helper.calls.lock().is_empty());
    }

    #[test]
    fn helper_failure_is_returned_without_user_level_rollback() {
        let original = state(false, 7);
        let actual = state(false, 4);
        let (_dir, service, helper) = fixture(original.clone());
        helper.reply(actual.clone(), Some(code::POWER));
        assert_eq!(
            service.set(true, &original.revision).unwrap_err().code,
            code::POWER
        );
        assert_eq!(service.state().unwrap(), actual);
        assert_eq!(helper.calls.lock().len(), 1);
    }

    #[test]
    fn removal_delegates_restoration_to_the_helper() {
        let (_dir, service, helper) = fixture(state(true, 0));
        let mut removed = state(false, 7);
        *helper.remove_reply.lock() = Some(Reply {
            state: removed.clone(),
            error: None,
        });
        removed.helper = "missing".into();
        assert_eq!(service.remove().unwrap(), removed);
        assert_eq!(helper.removals.load(Ordering::SeqCst), 1);
        assert!(helper.calls.lock().is_empty());
        assert!(!helper.ready.load(Ordering::SeqCst));
    }

    #[test]
    fn failed_removal_returns_helper_error() {
        let original = state(true, 0);
        let (_dir, service, helper) = fixture(original.clone());
        *helper.remove_reply.lock() = Some(Reply {
            state: original.clone(),
            error: Some(code::POWER),
        });
        assert_eq!(service.remove().unwrap_err().code, code::POWER);
        assert_eq!(service.state().unwrap(), original);
        assert!(helper.calls.lock().is_empty());
    }

    #[test]
    fn concurrent_toggles_reserve_one_state_revision() {
        let original = state(false, 9);
        let (_dir, service, helper) = fixture(original.clone());
        helper.reply(state(true, 0), None);
        let service = Arc::new(service);
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let joins: Vec<_> = (0..2)
            .map(|_| {
                let service = service.clone();
                let revision = original.revision.clone();
                let b = barrier.clone();
                std::thread::spawn(move || {
                    b.wait();
                    service.set(true, &revision).is_ok()
                })
            })
            .collect();
        barrier.wait();
        let count = joins
            .into_iter()
            .filter_map(|j| j.join().ok())
            .filter(|v| *v)
            .count();
        assert_eq!(count, 1);
        assert_eq!(helper.calls.lock().len(), 1);
        assert_eq!(helper.approvals.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn native_status_parsing_does_not_mix_power_sources() {
        assert_eq!(
            parse(
                " SleepDisabled 1\n",
                "Battery Power:\n sleep 0\nAC Power:\n sleep 9\n"
            )
            .unwrap(),
            State::new(true, true, 0)
        );
        assert!(
            !parse("SleepDisabled 0", "AC Power:\n sleep 9")
                .unwrap()
                .supported
        );
        assert!(parse("", "Battery Power:\n sleep 0").is_err());
    }
}
