//! Bounded process shutdown. Native tests may target only their own child PIDs.
use crate::errors::code;
use crate::storage::{AppError, Result};
use serde::Serialize;
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, Signal, System, UpdateKind};
#[derive(Clone, Debug, PartialEq)]
struct Entry {
    pid: u32,
    parent: Option<u32>,
    started: u64,
    executable: PathBuf,
    user: String,
    official_app: bool,
    claude_code: bool,
}
const GRACE: Duration = Duration::from_secs(2);
const EXIT_CONFIRMATION: Duration = Duration::from_secs(1);
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Running,
    Gone,
    Changed,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Termination {
    Graceful,
    Force,
}
impl Entry {
    fn same_process(&self, other: &Self) -> bool {
        self.pid == other.pid
            && self.started == other.started
            && self.executable == other.executable
            && self.user == other.user
            && self.claude_code == other.claude_code
    }
}
trait Processes {
    fn inventory(&mut self) -> Vec<Entry>;
    fn state(&mut self, entry: &Entry) -> State;
    fn terminate_if_same(&mut self, entry: &Entry, how: Termination);
    fn wait_for_exit(&mut self, entries: &[&Entry], timeout: Duration);
    fn prepare_terminals(&mut self, _entries: &[&Entry], _inventory: &[Entry]) {}
    fn cleanup_terminals(&mut self) -> usize {
        0
    }
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Outcome {
    pub terminated: usize,
    pub failed: usize,
    #[serde(skip_serializing_if = "is_zero")]
    pub terminal_cleanup_failed: usize,
}
fn is_zero(value: &usize) -> bool {
    *value == 0
}
fn root(entry: &Entry) -> bool {
    let filename = entry
        .executable
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    matches!(filename, "codex" | "codex.exe") || entry.official_app || entry.claude_code
}

fn claude_code(executable: &Path, command: &[std::ffi::OsString]) -> bool {
    let normalized = |path: &Path| {
        path.to_string_lossy()
            .replace('\\', "/")
            .to_ascii_lowercase()
    };
    let path = normalized(executable);
    // Desktop app executables and helpers are not Claude Code CLI entries.
    if path.contains(".app/") {
        return false;
    }
    let filename = path.rsplit('/').next().unwrap_or("");
    let package = |s: &str| s.contains("/node_modules/@anthropic-ai/claude-code/");
    if (matches!(filename, "claude" | "claude.exe") && package(&path))
        || path.contains("/.local/share/claude/versions/")
        || (filename == "claude" && path.contains("/caskroom/claude-code/"))
        || (matches!(filename, "claude" | "claude.exe")
            && (path.contains("/.local/bin/") || path.contains("/.claude/local/")))
    {
        return true;
    }
    if !matches!(filename, "node" | "node.exe" | "nodejs") {
        return false;
    }
    if command
        .iter()
        .skip(1)
        .take_while(|arg| arg.to_string_lossy().starts_with('-'))
        .any(|arg| matches!(arg.to_str(), Some("-e" | "--eval" | "-p" | "--print")))
    {
        return false;
    }
    // Inspect the script argument, never arbitrary prompt text later in argv.
    let Some(script) = command
        .iter()
        .skip(1)
        .find(|arg| !arg.to_string_lossy().starts_with('-'))
    else {
        return false;
    };
    let script = normalized(Path::new(script));
    package(&script) && script.ends_with("/cli.js")
}
fn run(backend: &mut dyn Processes, own_pid: u32) -> Result<Outcome> {
    let inventory = backend.inventory();
    let own = inventory
        .iter()
        .find(|p| p.pid == own_pid)
        .ok_or_else(|| AppError::new(code::PROCESS, "无法确认当前用户"))?;
    let eligible: HashMap<_, _> = inventory
        .iter()
        .filter(|p| p.user == own.user && !p.user.is_empty())
        .map(|p| (p.pid, p))
        .collect();
    // Protect ourselves and our child processes even if a filename happens to match.
    let mut protected = HashSet::from([own_pid]);
    let mut chosen: HashSet<u32> = eligible
        .values()
        .filter(|p| root(p))
        .map(|p| p.pid)
        .collect();
    for _ in 0..inventory.len() {
        let mut changed = false;
        for p in eligible.values() {
            if p.parent.is_some_and(|id| protected.contains(&id)) {
                changed |= protected.insert(p.pid);
            }
            if p.parent.is_some_and(|id| chosen.contains(&id)) {
                changed |= chosen.insert(p.pid);
            }
        }
        if !changed {
            break;
        }
    }
    let mut targets: Vec<_> = chosen
        .difference(&protected)
        .filter_map(|id| eligible.get(id).copied())
        .collect();
    let depth = |p: &Entry| {
        let mut n = 0;
        let mut parent = p.parent;
        while let Some(next) = parent.and_then(|id| eligible.get(&id)) {
            n += 1;
            if n > inventory.len() {
                break;
            }
            parent = next.parent;
        }
        n
    };
    targets.sort_by_key(|p| std::cmp::Reverse(depth(p)));
    backend.prepare_terminals(&targets, &inventory);
    let selected: HashSet<_> = targets.iter().map(|p| p.pid).collect();
    // Let each root clean up its TUI and children before touching descendants.
    for p in &targets {
        if !p.parent.is_some_and(|id| selected.contains(&id)) {
            backend.terminate_if_same(p, Termination::Graceful);
        }
    }
    backend.wait_for_exit(&targets, GRACE);
    for p in &targets {
        if backend.state(p) == State::Running {
            backend.terminate_if_same(p, Termination::Force);
        }
    }
    backend.wait_for_exit(&targets, EXIT_CONFIRMATION);
    let mut result = Outcome {
        terminated: 0,
        failed: 0,
        terminal_cleanup_failed: 0,
    };
    for p in targets {
        match backend.state(p) {
            State::Gone => result.terminated += 1,
            State::Running | State::Changed => result.failed += 1,
        }
    }
    result.terminal_cleanup_failed = backend.cleanup_terminals();
    Ok(result)
}

fn official_app(path: &Path) -> bool {
    #[cfg(target_os = "macos")]
    {
        let Some(bundle) = path
            .ancestors()
            .find(|p| p.extension().is_some_and(|x| x == "app"))
        else {
            return false;
        };
        let info = plist::Value::from_file(bundle.join("Contents/Info.plist")).ok();
        let id = info
            .as_ref()
            .and_then(|p| p.as_dictionary())
            .and_then(|p| p.get("CFBundleIdentifier"))
            .and_then(|p| p.as_string());
        matches!(
            id,
            Some("com.openai.codex" | "com.openai.chat" | "com.openai.ChatGPT")
        )
    }
    #[cfg(target_os = "windows")]
    {
        let leaf = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        matches!(leaf.as_str(), "chatgpt.exe" | "codex.exe")
            && path.components().any(|p| {
                let s = p.as_os_str().to_string_lossy().to_ascii_lowercase();
                s == "chatgpt"
                    || s == "codex"
                    || s.starts_with("openai.chatgpt_")
                    || s.starts_with("openai.codex_")
            })
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = path;
        false
    }
}
#[cfg(target_os = "macos")]
mod terminal;
struct Native {
    system: System,
    #[cfg(target_os = "macos")]
    terminals: Vec<terminal::Captured>,
}
impl Native {
    fn refresh(&mut self, processes: ProcessesToUpdate<'_>) {
        self.system.refresh_processes_specifics(
            processes,
            true,
            ProcessRefreshKind::nothing()
                .with_exe(UpdateKind::Always)
                .with_cmd(UpdateKind::Always)
                .with_user(UpdateKind::Always),
        );
    }
    fn entry(&self, pid: Pid) -> Option<Entry> {
        let p = self.system.process(pid)?;
        let executable = p.exe()?.to_owned();
        Some(Entry {
            pid: pid.as_u32(),
            parent: p.parent().map(|p| p.as_u32()),
            started: {
                #[cfg(target_os = "macos")]
                {
                    terminal::started(pid.as_u32())?
                }
                #[cfg(not(target_os = "macos"))]
                {
                    p.start_time()
                }
            },
            official_app: official_app(&executable),
            claude_code: claude_code(&executable, p.cmd()),
            executable,
            user: p.user_id().map(|u| format!("{u:?}")).unwrap_or_default(),
        })
    }
}
impl Processes for Native {
    fn inventory(&mut self) -> Vec<Entry> {
        self.refresh(ProcessesToUpdate::All);
        self.system
            .processes()
            .keys()
            .filter_map(|p| self.entry(*p))
            .collect()
    }
    fn state(&mut self, entry: &Entry) -> State {
        let pid = Pid::from_u32(entry.pid);
        self.refresh(ProcessesToUpdate::Some(&[pid]));
        let Some(process) = self.system.process(pid) else {
            return State::Gone;
        };
        if process.status() == sysinfo::ProcessStatus::Zombie {
            #[cfg(target_os = "macos")]
            let same = terminal::started(entry.pid) == Some(entry.started);
            #[cfg(not(target_os = "macos"))]
            let same = process.start_time() == entry.started;
            return if same { State::Gone } else { State::Changed };
        }
        match self.entry(pid) {
            Some(current) if entry.same_process(&current) => State::Running,
            _ => State::Changed,
        }
    }
    fn terminate_if_same(&mut self, entry: &Entry, how: Termination) {
        if self.state(entry) != State::Running {
            return;
        }
        if let Some(process) = self.system.process(Pid::from_u32(entry.pid)) {
            let _ = process.kill_with(match how {
                Termination::Graceful => Signal::Term,
                Termination::Force => Signal::Kill,
            });
        }
    }
    fn wait_for_exit(&mut self, entries: &[&Entry], timeout: Duration) {
        let deadline = Instant::now() + timeout;
        loop {
            if !entries.iter().any(|e| self.state(e) == State::Running) {
                return;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return;
            }
            std::thread::sleep(remaining.min(Duration::from_millis(25)));
        }
    }
    #[cfg(target_os = "macos")]
    fn prepare_terminals(&mut self, entries: &[&Entry], inventory: &[Entry]) {
        self.terminals = terminal::capture(entries, inventory);
    }
    #[cfg(target_os = "macos")]
    fn cleanup_terminals(&mut self) -> usize {
        let mut failures = 0;
        let deadline = Instant::now() + EXIT_CONFIRMATION;
        for captured in std::mem::take(&mut self.terminals) {
            if !captured.cleanup_until(deadline) {
                failures += 1;
            }
        }
        failures
    }
}
pub fn force_quit() -> Result<Outcome> {
    run(
        &mut Native {
            system: System::new(),
            #[cfg(target_os = "macos")]
            terminals: Vec::new(),
        },
        std::process::id(),
    )
}
#[cfg(test)]
#[path = "process_control_tests.rs"]
mod tests;
