//! Process termination is deliberately tested with a fake inventory only.
use crate::errors::code;
use crate::storage::{AppError, Result};
use serde::Serialize;
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};
use sysinfo::{Pid, ProcessesToUpdate, Signal, System};
#[derive(Clone, Debug, PartialEq)]
struct Entry {
    pid: u32,
    parent: Option<u32>,
    started: u64,
    executable: PathBuf,
    user: String,
    official_app: bool,
}
trait Processes {
    fn inventory(&mut self) -> Vec<Entry>;
    fn terminate_if_same(&mut self, entry: &Entry) -> bool;
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Outcome {
    pub terminated: usize,
    pub failed: usize,
}
fn root(entry: &Entry) -> bool {
    let filename = entry
        .executable
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    matches!(filename, "codex" | "codex.exe") || entry.official_app
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
    let mut result = Outcome {
        terminated: 0,
        failed: 0,
    };
    for p in targets {
        if backend.terminate_if_same(p) {
            result.terminated += 1;
        } else {
            result.failed += 1;
        }
    }
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
struct Native(System);
impl Native {
    fn entry(&self, pid: Pid) -> Option<Entry> {
        let p = self.0.process(pid)?;
        let executable = p.exe()?.to_owned();
        Some(Entry {
            pid: pid.as_u32(),
            parent: p.parent().map(|p| p.as_u32()),
            started: p.start_time(),
            official_app: official_app(&executable),
            executable,
            user: p.user_id().map(|u| format!("{u:?}")).unwrap_or_default(),
        })
    }
}
impl Processes for Native {
    fn inventory(&mut self) -> Vec<Entry> {
        self.0.refresh_processes(ProcessesToUpdate::All, true);
        self.0
            .processes()
            .keys()
            .filter_map(|p| self.entry(*p))
            .collect()
    }
    fn terminate_if_same(&mut self, entry: &Entry) -> bool {
        let pid = Pid::from_u32(entry.pid);
        self.0
            .refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
        self.entry(pid).as_ref() == Some(entry)
            && self
                .0
                .process(pid)
                .is_some_and(|p| p.kill_with(Signal::Kill) == Some(true))
    }
}
pub fn force_quit() -> Result<Outcome> {
    run(&mut Native(System::new()), std::process::id())
}
#[cfg(test)]
mod tests {
    use super::*;
    struct Fake {
        items: Vec<Entry>,
        calls: Vec<u32>,
        changed: HashSet<u32>,
    }
    impl Processes for Fake {
        fn inventory(&mut self) -> Vec<Entry> {
            self.items.clone()
        }
        fn terminate_if_same(&mut self, e: &Entry) -> bool {
            self.calls.push(e.pid);
            !self.changed.contains(&e.pid)
        }
    }
    fn e(pid: u32, parent: Option<u32>, name: &str, official: bool) -> Entry {
        Entry {
            pid,
            parent,
            started: 1,
            executable: name.into(),
            user: "user".into(),
            official_app: official,
        }
    }
    #[test]
    fn matches_exact_identity_and_descendants_without_killing_any_real_process() {
        let mut other = e(8, None, "/bin/codex", false);
        other.user = "another".into();
        let mut f = Fake {
            items: vec![
                e(1, None, "/app/lich13-switch", false),
                e(2, None, "/Applications/ChatGPT.app/ChatGPT", true),
                e(3, Some(2), "/node", false),
                e(4, None, "/bin/codex", false),
                e(5, None, "/app/codexhost", false),
                e(6, Some(1), "/bin/codex", false),
                e(7, None, "/extension/ChatGPT for Chrome", false),
                other,
            ],
            calls: vec![],
            changed: HashSet::from([4]),
        };
        let out = run(&mut f, 1).unwrap();
        assert_eq!((out.terminated, out.failed), (2, 1));
        assert_eq!(f.calls[0], 3);
        assert_eq!(
            f.calls.into_iter().collect::<HashSet<_>>(),
            HashSet::from([2, 3, 4])
        );
    }
}
