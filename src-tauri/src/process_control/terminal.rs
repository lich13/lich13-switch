//! Output-only terminal recovery, guarded by the original process and shell identities.
use super::Entry;
use std::{
    collections::{HashMap, HashSet},
    fs::{File, OpenOptions},
    io::Write,
    os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt},
};

// Disable mouse/focus reporting left behind by a killed TUI. Do not reset termios,
// paste mode or inject shell input: the shell may already be editing a command.
const CLEAR_REPORTING: &[u8] = b"\x1b[?9l\x1b[?1000l\x1b[?1001l\x1b[?1002l\x1b[?1003l\x1b[?1004l\x1b[?1005l\x1b[?1006l\x1b[?1015l\x1b[?1016l";

fn info(pid: u32) -> Option<libc::proc_bsdinfo> {
    let mut result = std::mem::MaybeUninit::<libc::proc_bsdinfo>::uninit();
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
    // proc_pidinfo initializes the complete structure only on an exact-size result.
    let read = unsafe {
        libc::proc_pidinfo(
            pid as i32,
            libc::PROC_PIDTBSDINFO,
            0,
            result.as_mut_ptr().cast(),
            size,
        )
    };
    (read == size).then(|| unsafe { result.assume_init() })
}

pub(super) fn started(pid: u32) -> Option<u64> {
    info(pid).map(|p| start_stamp(&p))
}
fn start_stamp(p: &libc::proc_bsdinfo) -> u64 {
    p.pbi_start_tvsec
        .saturating_mul(1_000_000)
        .saturating_add(p.pbi_start_tvusec)
}

#[derive(Clone, Copy)]
struct Identity {
    pid: u32,
    start: (u64, u64),
    uid: u32,
}
impl Identity {
    fn new(p: &libc::proc_bsdinfo) -> Self {
        Self {
            pid: p.pbi_pid,
            start: (p.pbi_start_tvsec, p.pbi_start_tvusec),
            uid: p.pbi_uid,
        }
    }
    fn matches(self, p: &libc::proc_bsdinfo) -> bool {
        self.pid == p.pbi_pid
            && self.uid == p.pbi_uid
            && self.start == (p.pbi_start_tvsec, p.pbi_start_tvusec)
    }
    fn gone(self) -> bool {
        match info(self.pid) {
            Some(p) => self.matches(&p) && p.pbi_status == libc::SZOMB,
            None => {
                // Failure to inspect a live process is not proof of exit.
                (unsafe { libc::kill(self.pid as i32, 0) }) == -1
                    && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
            }
        }
    }
}
struct Terminal {
    file: File,
    device: u32,
    session: i32,
    shell: Identity,
    shell_group: u32,
    target_group: u32,
    targets: Vec<Identity>,
}
pub(super) struct Captured(Option<Terminal>);

fn shell(entry: &Entry) -> bool {
    matches!(
        entry.executable.file_name().and_then(|s| s.to_str()),
        Some("sh" | "bash" | "zsh" | "fish" | "dash" | "ksh" | "csh" | "tcsh" | "nu")
    )
}

fn open_terminal(device: u32, uid: u32) -> Option<File> {
    // A device number from the kernel selects the file. Never trust a command line
    // or a process-provided path, and never follow device-directory symlinks.
    for item in std::fs::read_dir("/dev").ok()?.flatten() {
        let name = item.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.starts_with("ttys") || !name.bytes().all(|b| b.is_ascii_alphanumeric()) {
            continue;
        }
        let path = item.path();
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if !meta.file_type().is_char_device() || meta.uid() != uid || meta.rdev() as u32 != device {
            continue;
        }
        let file = OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NOCTTY | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(path)
            .ok()?;
        let meta = file.metadata().ok()?;
        if meta.file_type().is_char_device() && meta.uid() == uid && meta.rdev() as u32 == device {
            return Some(file);
        }
    }
    None
}

pub(super) fn capture(entries: &[&Entry], inventory: &[Entry]) -> Vec<Captured> {
    let selected: HashSet<_> = entries.iter().map(|e| e.pid).collect();
    let all: HashMap<_, _> = inventory.iter().map(|e| (e.pid, e)).collect();
    let uid = unsafe { libc::geteuid() };
    let mut seen = HashSet::new();
    let mut result = Vec::new();
    for entry in entries {
        let Some(p) = info(entry.pid) else {
            continue;
        };
        if start_stamp(&p) != entry.started
            || p.pbi_uid != uid
            || p.e_tdev == u32::MAX
            || p.e_tpgid != p.pbi_pgid
            || !seen.insert(p.e_tdev)
        {
            continue;
        }
        let ready = (|| {
            let session = unsafe { libc::getsid(entry.pid as i32) };
            if session < 0 {
                return None;
            }
            let mut parent = entry.parent;
            let mut visited = HashSet::new();
            let shell = loop {
                let ancestor = all.get(&parent?)?;
                if !visited.insert(ancestor.pid) {
                    return None;
                }
                if !selected.contains(&ancestor.pid) && shell(ancestor) {
                    break ancestor;
                }
                parent = ancestor.parent;
            };
            let shell_info = info(shell.pid)?;
            if start_stamp(&shell_info) != shell.started
                || shell_info.pbi_uid != uid
                || shell_info.e_tdev != p.e_tdev
                || unsafe { libc::getsid(shell.pid as i32) } != session
            {
                return None;
            }
            let file = open_terminal(p.e_tdev, uid)?;
            // tcgetpgrp rejects a terminal outside this app's controlling session.
            // proc_pidinfo reports its foreground group without acquiring it.
            if shell_info.e_tpgid != p.pbi_pgid {
                return None;
            }
            let targets = entries
                .iter()
                .filter_map(|e| {
                    info(e.pid).filter(|i| start_stamp(i) == e.started && i.pbi_uid == uid)
                })
                .filter(|i| i.e_tdev == p.e_tdev)
                .map(|i| Identity::new(&i))
                .collect();
            Some(Terminal {
                file,
                device: p.e_tdev,
                session,
                shell: Identity::new(&shell_info),
                shell_group: shell_info.pbi_pgid,
                target_group: p.pbi_pgid,
                targets,
            })
        })();
        result.push(Captured(ready));
    }
    result
}

impl Captured {
    #[cfg(test)]
    pub(super) fn cleanup(self) -> bool {
        self.cleanup_until(std::time::Instant::now() + super::EXIT_CONFIRMATION)
    }
    pub(super) fn cleanup_until(self, deadline: std::time::Instant) -> bool {
        let Some(mut tty) = self.0 else {
            return false;
        };
        if !tty.targets.iter().all(|p| p.gone()) {
            return false;
        }
        loop {
            let Some(shell) = info(tty.shell.pid) else {
                return false;
            };
            if !tty.shell.matches(&shell)
                || shell.e_tdev != tty.device
                || shell.pbi_pgid != tty.shell_group
                || unsafe { libc::getsid(tty.shell.pid as i32) } != tty.session
            {
                return false;
            }
            if shell.e_tpgid == tty.shell_group {
                break;
            }
            // The npm launcher can outlive its native child briefly. Only wait
            // while the original group owns the terminal; never touch a new job.
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if shell.e_tpgid != tty.target_group || remaining.is_zero() {
                return false;
            }
            std::thread::sleep(remaining.min(std::time::Duration::from_millis(10)));
        }
        let Ok(meta) = tty.file.metadata() else {
            return false;
        };
        if meta.uid() != tty.shell.uid
            || meta.rdev() as u32 != tty.device
            || !meta.file_type().is_char_device()
        {
            return false;
        }
        if !info(tty.shell.pid).is_some_and(|p| {
            tty.shell.matches(&p) && p.e_tdev == tty.device && p.e_tpgid == tty.shell_group
        }) {
            return false;
        }
        tty.file.write_all(CLEAR_REPORTING).is_ok()
    }
}

#[cfg(test)]
#[path = "terminal_tests.rs"]
mod tests;
