//! Battery clamshell control through the narrowly scoped, authorized power helper.
use crate::storage::{self, AppError, Result};
use serde::{Deserialize, Serialize};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct State {
    pub helper: String,
    pub supported: bool,
    pub enabled: bool,
    pub battery_sleep: u16,
    pub revision: String,
    #[serde(default)]
    pub ownership: String,
    #[serde(default)]
    pub external_changed: bool,
}
impl State {
    fn new(supported: bool, enabled: bool, minutes: u16) -> Self {
        Self {
            helper: if supported { "ready" } else { "unsupported" }.into(),
            ownership: if enabled { "external" } else { "none" }.into(),
            external_changed: false,
            supported,
            enabled,
            battery_sleep: minutes,
            revision: storage::digest(format!("{supported}:{enabled}:{minutes}").as_bytes()),
        }
    }
}
trait System: Send + Sync {
    fn prepare(&self, _force: bool) -> Result<()> {
        Ok(())
    }
    fn remove(&self) -> Result<()> {
        Err(AppError::new("UNSUPPORTED", "此设备不支持电源助手"))
    }
    fn read(&self) -> Result<State>;
    fn apply(&self, before: &State, enabled: bool, minutes: u16) -> Result<()>;
}
struct Native;
pub struct Service {
    path: PathBuf,
    system: Arc<dyn System>,
    lock: Mutex<()>,
}
impl Service {
    pub fn new(data: &std::path::Path) -> Self {
        Self {
            path: data.join("clamshell-restore.json"),
            system: Arc::new(Native),
            lock: Mutex::new(()),
        }
    }
    pub fn state(&self) -> Result<State> {
        self.system.read()
    }
    pub fn install(&self) -> Result<State> {
        let _lock = self.lock.lock().unwrap();
        self.system.prepare(true)?;
        self.state()
    }
    pub fn remove(&self) -> Result<State> {
        let _lock = self.lock.lock().unwrap();
        // The helper restores only the fields it still owns. External settings survive removal.
        self.system.remove()?;
        self.retire_legacy_record()?;
        self.state()
    }

    pub fn set(&self, enabled: bool, expected: &str) -> Result<State> {
        let _lock = self.lock.lock().unwrap();
        self.set_locked(enabled, expected)
    }
    fn set_locked(&self, enabled: bool, expected: &str) -> Result<State> {
        let before = self.state()?;
        if !before.supported {
            return Err(AppError::new("UNSUPPORTED", "此设备不支持电池合盖控制"));
        }
        if before.revision != expected {
            return Err(AppError::new("CONFLICT", "电源状态已变化，请重新操作"));
        }
        self.system.prepare(false)?;
        let prepared = self.state()?;
        let installed_without_power_change = before.helper != "ready"
            && before.supported == prepared.supported
            && before.enabled == prepared.enabled
            && before.battery_sleep == prepared.battery_sleep;
        if prepared.revision != expected && !installed_without_power_change {
            return Err(AppError::new("CONFLICT", "电源状态已变化，请重新操作"));
        }
        let before = prepared;
        // Restoration belongs exclusively to the privileged helper. An unverified
        // legacy user/script record must never supply a battery sleep value.
        self.system.apply(&before, enabled, before.battery_sleep)?;
        let actual = self.state()?;
        if actual.enabled != enabled {
            return Err(AppError::new("POWER", "电源状态再次变化，请重新操作"));
        }
        self.retire_legacy_record()?;
        Ok(actual)
    }
    fn retire_legacy_record(&self) -> Result<()> {
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(_) => Err(AppError::new("POWER", "电源已更新，旧恢复记录清理失败")),
        }
    }
}
#[cfg(target_os = "macos")]
fn output(args: &[&str]) -> Result<String> {
    let result = std::process::Command::new("/usr/bin/pmset")
        .args(args)
        .output()
        .map_err(storage::io_error)?;
    if !result.status.success() {
        return Err(AppError::new("POWER", "无法读取系统电源状态"));
    }
    String::from_utf8(result.stdout).map_err(|_| AppError::new("POWER", "无法读取系统电源状态"))
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
        _ => Err(AppError::new("POWER", "无法读取合盖休眠状态")),
    }
}
impl System for Native {
    fn prepare(&self, force: bool) -> Result<()> {
        #[cfg(target_os = "macos")]
        {
            crate::power_macos::ensure(force)
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = force;
            Err(AppError::new("UNSUPPORTED", "此设备不支持电源助手"))
        }
    }
    fn remove(&self) -> Result<()> {
        #[cfg(target_os = "macos")]
        {
            crate::power_macos::remove()
        }
        #[cfg(not(target_os = "macos"))]
        {
            Err(AppError::new("UNSUPPORTED", "此设备不支持电源助手"))
        }
    }
    fn read(&self) -> Result<State> {
        #[cfg(target_os = "macos")]
        {
            let mut state = parse(&output(&["-g"])?, &output(&["-g", "custom"])?)?;
            state.helper = if state.supported {
                crate::power_macos::status()
            } else {
                "unsupported"
            }
            .into();
            if state.helper == "ready" {
                let response = crate::power_macos::request(serde_json::json!({"op":"state"}))?;
                let actual = &response["state"];
                state.enabled = actual["enabled"]
                    .as_bool()
                    .ok_or_else(|| AppError::new("POWER", "电源状态无效"))?;
                state.battery_sleep = actual["batterySleep"]
                    .as_u64()
                    .filter(|n| *n <= 1440)
                    .ok_or_else(|| AppError::new("POWER", "电源状态无效"))?
                    as u16;
                state.ownership = actual["ownership"].as_str().unwrap_or("external").into();
                state.external_changed = actual["externalChanged"].as_bool().unwrap_or(false);
                state.revision = storage::digest(actual.to_string().as_bytes());
            }
            Ok(state)
        }
        #[cfg(not(target_os = "macos"))]
        {
            Ok(State::new(false, false, 0))
        }
    }
    fn apply(&self, before: &State, enabled: bool, minutes: u16) -> Result<()> {
        #[cfg(target_os = "macos")]
        {
            crate::power_macos::request(
                serde_json::json!({"op":"set", "enabled":enabled, "minutes":minutes, "beforeEnabled":before.enabled, "beforeSleep":before.battery_sleep,"beforeOwnership":before.ownership}),
            )?;
            Ok(())
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (before, enabled, minutes);
            Err(AppError::new("UNSUPPORTED", "此设备不支持电池合盖控制"))
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
        calls: Mutex<Vec<(State, bool, u16)>>,
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
            self.replies
                .lock()
                .unwrap()
                .push_back(Reply { state, error });
        }
    }
    impl System for MockHelper {
        fn read(&self) -> Result<State> {
            Ok(self.current.lock().unwrap().clone())
        }
        fn prepare(&self, force: bool) -> Result<()> {
            self.prepares.fetch_add(1, Ordering::SeqCst);
            if force || !self.ready.load(Ordering::SeqCst) {
                self.approvals.fetch_add(1, Ordering::SeqCst);
                if self.cancelled.load(Ordering::SeqCst) {
                    return Err(AppError::new("POWER_AUTH", "已取消系统授权"));
                }
                self.ready.store(true, Ordering::SeqCst);
            }
            if let Some(state) = self.after_prepare.lock().unwrap().take() {
                *self.current.lock().unwrap() = state;
            }
            Ok(())
        }
        fn apply(&self, before: &State, enabled: bool, minutes: u16) -> Result<()> {
            let mut state = self.current.lock().unwrap();
            if state.revision != before.revision {
                return Err(AppError::new("CONFLICT", "外部状态变化"));
            }
            self.calls
                .lock()
                .unwrap()
                .push((before.clone(), enabled, minutes));
            // The native tests own restoration semantics. These explicit replies
            // verify that Service accepts only the helper's authoritative result.
            let reply = self
                .replies
                .lock()
                .unwrap()
                .pop_front()
                .expect("helper reply");
            *state = reply.state;
            reply
                .error
                .map_or(Ok(()), |code| Err(AppError::new(code, "助手事务失败")))
        }
        fn remove(&self) -> Result<()> {
            self.removals.fetch_add(1, Ordering::SeqCst);
            let reply = self
                .remove_reply
                .lock()
                .unwrap()
                .take()
                .expect("remove reply");
            *self.current.lock().unwrap() = reply.state;
            if let Some(code) = reply.error {
                return Err(AppError::new(code, "助手移除失败"));
            }
            self.ready.store(false, Ordering::SeqCst);
            Ok(())
        }
    }

    fn state(enabled: bool, minutes: u16, ownership: &str, external_changed: bool) -> State {
        let mut state = State::new(true, enabled, minutes);
        state.ownership = ownership.into();
        state.external_changed = external_changed;
        state.revision = storage::digest(
            format!("{enabled}:{minutes}:{ownership}:{external_changed}").as_bytes(),
        );
        state
    }
    fn fixture(initial: State) -> (tempfile::TempDir, Service, Arc<MockHelper>) {
        let dir = tempfile::tempdir().unwrap();
        let mut service = Service::new(dir.path());
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
    fn root_helper_supplies_restored_minutes_including_zero() {
        for minutes in [0, 7] {
            let original = state(false, minutes, "none", false);
            let owned = state(true, 0, "application", false);
            let (_dir, service, helper) = fixture(original.clone());
            helper.reply(owned.clone(), None);
            helper.reply(original.clone(), None);
            assert_eq!(service.set(true, &original.revision).unwrap(), owned);
            assert_eq!(service.set(false, &owned.revision).unwrap(), original);
            let calls = helper.calls.lock().unwrap();
            assert_eq!(
                calls.as_slice(),
                &[(original, true, minutes), (owned, false, 0)]
            );
            assert!(!service.path.exists());
        }
    }

    #[test]
    fn legacy_user_records_never_supply_sleep_or_a_default() {
        for (minutes, legacy) in [(0, r#"{"version":1,"minutes":7}"#), (9, "invalid record")] {
            let original = state(true, minutes, "external", false);
            let closed = state(false, minutes, "none", false);
            let (_dir, service, helper) = fixture(original.clone());
            std::fs::write(&service.path, legacy).unwrap();
            helper.reply(closed.clone(), None);
            assert_eq!(service.set(false, &original.revision).unwrap(), closed);
            assert_eq!(
                helper.calls.lock().unwrap().as_slice(),
                &[(original, false, minutes)]
            );
            assert!(!service.path.exists());
        }
    }

    #[test]
    fn authorization_cancellation_preserves_state_and_legacy_record() {
        let original = state(false, 3, "none", false);
        let (_dir, service, helper) = fixture(original.clone());
        helper.ready.store(false, Ordering::SeqCst);
        helper.cancelled.store(true, Ordering::SeqCst);
        std::fs::write(&service.path, "legacy").unwrap();
        assert_eq!(
            service.set(true, &original.revision).unwrap_err().code,
            "POWER_AUTH"
        );
        assert_eq!(service.state().unwrap(), original);
        assert_eq!(std::fs::read_to_string(&service.path).unwrap(), "legacy");
        assert!(helper.calls.lock().unwrap().is_empty());
        assert_eq!(helper.approvals.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn daily_toggles_reuse_authorization_but_repair_forces_it() {
        let original = state(false, 9, "none", false);
        let owned = state(true, 0, "application", false);
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
        assert_eq!(helper.calls.lock().unwrap().len(), 6);
        assert_eq!(service.install().unwrap(), original);
        assert_eq!(helper.approvals.load(Ordering::SeqCst), 2);
        assert_eq!(helper.calls.lock().unwrap().len(), 6);
        helper.ready.store(false, Ordering::SeqCst);
        helper.reply(owned, None);
        service.set(true, &original.revision).unwrap();
        assert_eq!(helper.approvals.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn first_installation_with_unchanged_power_accepts_helper_revision() {
        for (enabled, minutes) in [(false, 7), (true, 5)] {
            let mut original = State::new(true, enabled, minutes);
            original.helper = "missing".into();
            let prepared = state(
                enabled,
                minutes,
                if enabled { "external" } else { "none" },
                false,
            );
            let actual = state(
                !enabled,
                if enabled { minutes } else { 0 },
                if enabled { "none" } else { "application" },
                false,
            );
            assert_ne!(original.revision, prepared.revision);
            let (_dir, service, helper) = fixture(original.clone());
            helper.ready.store(false, Ordering::SeqCst);
            *helper.after_prepare.lock().unwrap() = Some(prepared.clone());
            helper.reply(actual.clone(), None);

            assert_eq!(service.set(!enabled, &original.revision).unwrap(), actual);
            assert_eq!(
                helper.calls.lock().unwrap().as_slice(),
                &[(prepared, !enabled, minutes)]
            );
            assert_eq!(helper.prepares.load(Ordering::SeqCst), 1);
            assert_eq!(helper.approvals.load(Ordering::SeqCst), 1);
        }
    }

    #[test]
    fn power_changes_during_first_installation_conflict_without_applying() {
        for (supported, enabled, minutes) in [(false, false, 7), (true, true, 7), (true, false, 9)]
        {
            let mut original = State::new(true, false, 7);
            original.helper = "missing".into();
            let prepared = if supported {
                state(
                    enabled,
                    minutes,
                    if enabled { "external" } else { "none" },
                    false,
                )
            } else {
                State::new(false, enabled, minutes)
            };
            assert_ne!(original.revision, prepared.revision);
            let (_dir, service, helper) = fixture(original.clone());
            helper.ready.store(false, Ordering::SeqCst);
            *helper.after_prepare.lock().unwrap() = Some(prepared.clone());

            assert_eq!(
                service.set(true, &original.revision).unwrap_err().code,
                "CONFLICT"
            );
            assert_eq!(service.state().unwrap(), prepared);
            assert!(helper.calls.lock().unwrap().is_empty());
            assert_eq!(helper.prepares.load(Ordering::SeqCst), 1);
            assert_eq!(helper.approvals.load(Ordering::SeqCst), 1);
        }
    }

    #[test]
    fn stale_revision_does_not_prepare_or_call_helper() {
        let original = state(false, 7, "none", false);
        let (_dir, service, helper) = fixture(original);
        assert_eq!(service.set(true, "stale").unwrap_err().code, "CONFLICT");
        assert_eq!(helper.prepares.load(Ordering::SeqCst), 0);
        assert!(helper.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn ownership_changes_during_prepare_require_a_new_revision() {
        let original = state(true, 0, "application", false);
        let external = state(true, 0, "external", true);
        let (_dir, service, helper) = fixture(original.clone());
        *helper.after_prepare.lock().unwrap() = Some(external.clone());
        assert_eq!(
            service.set(false, &original.revision).unwrap_err().code,
            "CONFLICT"
        );
        assert_eq!(service.state().unwrap(), external);
        assert!(helper.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn helper_partial_failure_is_returned_without_user_level_rollback() {
        let original = state(false, 7, "none", false);
        let external = state(false, 4, "none", true);
        let (_dir, service, helper) = fixture(original.clone());
        std::fs::write(&service.path, "legacy").unwrap();
        helper.reply(external.clone(), Some("CONFLICT"));
        assert_eq!(
            service.set(true, &original.revision).unwrap_err().code,
            "CONFLICT"
        );
        assert_eq!(service.state().unwrap(), external);
        assert_eq!(helper.calls.lock().unwrap().len(), 1);
        assert_eq!(std::fs::read_to_string(&service.path).unwrap(), "legacy");
    }

    #[test]
    fn successful_helper_reply_with_wrong_final_flag_is_rejected() {
        let original = state(false, 7, "none", false);
        let (_dir, service, helper) = fixture(original.clone());
        helper.reply(original.clone(), None);
        assert_eq!(
            service.set(true, &original.revision).unwrap_err().code,
            "POWER"
        );
        assert_eq!(service.state().unwrap(), original);
        assert_eq!(helper.calls.lock().unwrap().len(), 1);
    }

    #[test]
    fn removal_delegates_restoration_and_preserves_external_enabled_state() {
        for (original, removed) in [
            (
                state(true, 0, "application", false),
                state(false, 7, "none", false),
            ),
            (
                state(true, 5, "external", false),
                state(true, 5, "external", false),
            ),
        ] {
            let (_dir, service, helper) = fixture(original);
            std::fs::write(&service.path, "legacy").unwrap();
            *helper.remove_reply.lock().unwrap() = Some(Reply {
                state: removed.clone(),
                error: None,
            });
            assert_eq!(service.remove().unwrap(), removed);
            assert_eq!(helper.removals.load(Ordering::SeqCst), 1);
            assert!(helper.calls.lock().unwrap().is_empty());
            assert!(!helper.ready.load(Ordering::SeqCst));
            assert!(!service.path.exists());
        }
    }

    #[test]
    fn failed_removal_keeps_legacy_record_and_returns_helper_error() {
        let original = state(true, 0, "application", false);
        let (_dir, service, helper) = fixture(original.clone());
        std::fs::write(&service.path, "legacy").unwrap();
        *helper.remove_reply.lock().unwrap() = Some(Reply {
            state: original.clone(),
            error: Some("POWER"),
        });
        assert_eq!(service.remove().unwrap_err().code, "POWER");
        assert_eq!(service.state().unwrap(), original);
        assert_eq!(std::fs::read_to_string(&service.path).unwrap(), "legacy");
        assert!(helper.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn concurrent_toggles_reserve_one_state_revision() {
        let original = state(false, 9, "none", false);
        let (_dir, service, helper) = fixture(original.clone());
        helper.reply(state(true, 0, "application", false), None);
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
        assert_eq!(helper.calls.lock().unwrap().len(), 1);
        assert_eq!(helper.approvals.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn state_serialization_preserves_ownership_metadata() {
        let mixed = state(true, 4, "mixed", true);
        let value = serde_json::to_value(&mixed).unwrap();
        assert_eq!(value["ownership"], "mixed");
        assert_eq!(value["externalChanged"], true);
        assert_eq!(serde_json::from_value::<State>(value).unwrap(), mixed);
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
