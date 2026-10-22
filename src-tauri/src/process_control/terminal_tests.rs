//! Isolated PTY coverage for terminal recovery. Every signal targets a PID
//! created by this module; these tests never invoke the process-wide command.
use super::*;
use crate::process_control::{Entry, Native, Processes, State, Termination};
use std::{
    fs,
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::process::CommandExt,
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};
use sysinfo::{Pid, ProcessesToUpdate, System};
use tempfile::TempDir;

const ENABLE_REPORTING: &[u8] =
    b"\x1b[?9h\x1b[?1000h\x1b[?1001h\x1b[?1002h\x1b[?1003h\x1b[?1004h\x1b[?1005h\x1b[?1006h\x1b[?1015h\x1b[?1016h";
const READY_MARKER: &[u8] = b"__PTY_FIXTURE_READY__";
const PROMPT_MARKER: &[u8] = b"__PTY_SHELL_READY__";
const IO_TIMEOUT: Duration = Duration::from_secs(5);

struct PtyHarness {
    shell: Entry,
    child: Option<Child>,
    master: fs::File,
    transcript: Vec<u8>,
    native: Native,
    temp: TempDir,
    fixture_script: PathBuf,
    fixtures: Vec<Entry>,
}

impl PtyHarness {
    fn spawn() -> io::Result<Self> {
        let temp = tempfile::Builder::new()
            .prefix("terminal-recovery-pty-")
            .tempdir()?;
        let fixture_script = temp.path().join("fixture.sh");
        fs::write(
            &fixture_script,
            r##"#!/bin/sh
pid_file=$1
printf '%s\n' "$$" > "$pid_file"
printf '\033[?9h\033[?1000h\033[?1001h\033[?1002h\033[?1003h\033[?1004h\033[?1005h\033[?1006h\033[?1015h\033[?1016h'
printf '__PTY_FIXTURE_READY__\n'
IFS= read -r _
"##,
        )?;

        let mut master_fd = -1;
        let mut slave_fd = -1;
        let mut window = libc::winsize {
            ws_row: 24,
            ws_col: 80,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let opened = unsafe {
            libc::openpty(
                &mut master_fd,
                &mut slave_fd,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut window,
            )
        };
        if opened == -1 {
            return Err(io::Error::last_os_error());
        }
        let master = unsafe { fs::File::from_raw_fd(master_fd) };
        set_nonblocking(master.as_raw_fd())?;
        let slave = unsafe { fs::File::from_raw_fd(slave_fd) };

        let mut command = Command::new("/bin/bash");
        command
            .args(["--noprofile", "--norc", "-i"])
            .env_clear()
            .env("HOME", temp.path())
            .env("PATH", "/usr/bin:/bin")
            .env("TERM", "xterm-256color")
            .env("PS1", "__PTY_SHELL_READY__ ")
            .stdin(Stdio::from(slave.try_clone()?))
            .stdout(Stdio::from(slave.try_clone()?))
            .stderr(Stdio::from(slave));
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                if libc::ioctl(libc::STDIN_FILENO, libc::TIOCSCTTY as libc::c_ulong, 0) == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn()?;
        let shell_pid = child.id();
        let mut harness = Self {
            shell: Entry {
                claude_code: false,
                pid: shell_pid,
                parent: None,
                started: 0,
                executable: PathBuf::new(),
                user: String::new(),
                official_app: false,
            },
            child: Some(child),
            master,
            transcript: Vec::new(),
            native: Native {
                system: System::new(),
                terminals: Vec::new(),
            },
            temp,
            fixture_script,
            fixtures: Vec::new(),
        };
        harness.wait_for_output(PROMPT_MARKER, 0, IO_TIMEOUT)?;
        harness.shell = wait_for_entry(&mut harness.native, shell_pid, IO_TIMEOUT)?;
        Ok(harness)
    }

    fn start_fixture(&mut self) -> io::Result<Entry> {
        let pid_file = self
            .temp
            .path()
            .join(format!("fixture-{}.pid", self.fixtures.len()));
        let command = format!(
            "/bin/sh {} {}\n",
            shell_quote(&self.fixture_script)?,
            shell_quote(&pid_file)?
        );
        let checkpoint = self.transcript.len();
        self.master.write_all(command.as_bytes())?;

        let pid = wait_for_pid_file(&pid_file, IO_TIMEOUT)?;
        let entry = wait_for_entry(&mut self.native, pid, IO_TIMEOUT)?;
        if entry.parent != Some(self.shell.pid) {
            return Err(io::Error::other(
                "fixture is not a direct child of the isolated shell",
            ));
        }
        self.fixtures.push(entry.clone());
        self.wait_for_output(READY_MARKER, checkpoint, IO_TIMEOUT)?;
        if !contains_from(&self.transcript, ENABLE_REPORTING, checkpoint) {
            return Err(io::Error::other(
                "fixture did not enable terminal reporting modes",
            ));
        }
        Ok(entry)
    }

    fn capture(&mut self, target: &Entry) -> io::Result<Captured> {
        let mut inventory = vec![target.clone()];
        let mut parent = target.parent;
        let mut found_shell = false;
        for _ in 0..64 {
            let Some(pid) = parent else {
                break;
            };
            if pid == target.pid {
                break;
            }
            let ancestor = wait_for_entry(&mut self.native, pid, IO_TIMEOUT)?;
            parent = ancestor.parent;
            found_shell |= shell(&ancestor);
            inventory.push(ancestor);
            if found_shell {
                break;
            }
        }
        if !found_shell {
            return Err(io::Error::other(
                "isolated process has no shell ancestor in its parent chain",
            ));
        }
        let captured = capture(&[&inventory[0]], &inventory);
        let Some(captured) = captured.into_iter().next() else {
            return Err(io::Error::other("terminal capture returned no candidate"));
        };
        if captured.0.is_none() {
            return Err(io::Error::other(format!(
                "isolated PTY was not captured: {}",
                capture_diagnostic(&inventory[1], &inventory)
            )));
        }
        Ok(captured)
    }

    fn terminate_force(&mut self, target: &Entry) {
        self.native.terminate_if_same(target, Termination::Force);
    }

    fn wait_for_shell_prompt(
        &mut self,
        captured: &Captured,
        target: &Entry,
        output_checkpoint: usize,
        timeout: Duration,
    ) -> io::Result<()> {
        let tty = captured
            .0
            .as_ref()
            .ok_or_else(|| io::Error::other("terminal capture is absent"))?;
        let shell_group = tty.shell_group as i32;
        let deadline = Instant::now() + timeout;
        loop {
            self.drain_master()?;
            let target_state = self.native.state(target);
            let shell_state = self.native.state(&self.shell);
            let foreground = info(tty.shell.pid)
                .filter(|shell| {
                    tty.shell.matches(shell)
                        && shell.e_tdev == tty.device
                        && unsafe { libc::getsid(tty.shell.pid as i32) } == tty.session
                })
                .map(|shell| shell.e_tpgid as i32);
            if target_state == State::Gone
                && shell_state == State::Running
                && foreground == Some(shell_group)
                && contains_from(&self.transcript, PROMPT_MARKER, output_checkpoint)
            {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "isolated shell did not resume after the fixture exited",
                ));
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn wait_for_output(
        &mut self,
        token: &[u8],
        output_checkpoint: usize,
        timeout: Duration,
    ) -> io::Result<()> {
        let deadline = Instant::now() + timeout;
        loop {
            self.drain_master()?;
            if contains_from(&self.transcript, token, output_checkpoint) {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "isolated PTY output marker did not arrive",
                ));
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn drain_master(&mut self) -> io::Result<()> {
        let mut bytes = [0u8; 4096];
        loop {
            match self.master.read(&mut bytes) {
                Ok(0) => return Ok(()),
                Ok(size) => self.transcript.extend_from_slice(&bytes[..size]),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) if error.raw_os_error() == Some(libc::EIO) => return Ok(()),
                Err(error) => return Err(error),
            }
        }
    }
}

impl Drop for PtyHarness {
    fn drop(&mut self) {
        for fixture in self.fixtures.iter().rev() {
            self.native.terminate_if_same(fixture, Termination::Force);
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        while self
            .fixtures
            .iter()
            .any(|fixture| self.native.state(fixture) == State::Running)
            && Instant::now() < deadline
        {
            thread::sleep(Duration::from_millis(10));
        }
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn set_nonblocking(fd: libc::c_int) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn shell_quote(path: &Path) -> io::Result<String> {
    let value = path
        .to_str()
        .ok_or_else(|| io::Error::other("temporary path is not valid UTF-8"))?;
    Ok(format!("'{}'", value.replace('\'', "'\\''")))
}

fn wait_for_pid_file(path: &Path, timeout: Duration) -> io::Result<u32> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(value) = fs::read_to_string(path) {
            if let Ok(pid) = value.trim().parse() {
                return Ok(pid);
            }
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "isolated fixture PID file did not arrive",
            ));
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn wait_for_entry(native: &mut Native, pid: u32, timeout: Duration) -> io::Result<Entry> {
    let deadline = Instant::now() + timeout;
    loop {
        let process = Pid::from_u32(pid);
        native
            .system
            .refresh_processes(ProcessesToUpdate::Some(&[process]), true);
        if let Some(entry) = native.entry(process) {
            return Ok(entry);
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "created process identity was not available",
            ));
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn contains_from(haystack: &[u8], needle: &[u8], start: usize) -> bool {
    haystack
        .get(start..)
        .is_some_and(|tail| tail.windows(needle.len()).any(|window| window == needle))
}

fn capture_diagnostic(entry: &Entry, inventory: &[Entry]) -> &'static str {
    let Some(target) = info(entry.pid) else {
        return "target-info-unavailable";
    };
    if start_stamp(&target) != entry.started {
        return "target-start-identity-mismatch";
    }
    if target.pbi_uid != unsafe { libc::geteuid() } {
        return "target-user-mismatch";
    }
    if target.e_tdev == u32::MAX {
        return "target-has-no-controlling-tty";
    }
    if target.e_tpgid != target.pbi_pgid {
        return "target-is-not-foreground";
    }
    let session = unsafe { libc::getsid(entry.pid as i32) };
    if session < 0 {
        return "target-session-unavailable";
    }
    let mut parent = entry.parent;
    let mut visited = std::collections::HashSet::new();
    let shell_entry = loop {
        let Some(ancestor) = inventory
            .iter()
            .find(|candidate| Some(candidate.pid) == parent)
        else {
            return "shell-ancestor-not-in-inventory";
        };
        if !visited.insert(ancestor.pid) {
            return "shell-ancestor-cycle";
        }
        if ancestor.pid != entry.pid && shell(ancestor) {
            break ancestor;
        }
        parent = ancestor.parent;
    };
    let Some(shell_info) = info(shell_entry.pid) else {
        return "shell-info-unavailable";
    };
    if start_stamp(&shell_info) != shell_entry.started {
        return "shell-start-identity-mismatch";
    }
    if shell_info.pbi_uid != unsafe { libc::geteuid() } {
        return "shell-user-mismatch";
    }
    if shell_info.e_tdev != target.e_tdev {
        return "shell-tty-mismatch";
    }
    if unsafe { libc::getsid(shell_entry.pid as i32) } != session {
        return "shell-session-mismatch";
    }
    let Some(_tty) = open_terminal(target.e_tdev, target.pbi_uid) else {
        return "tty-device-node-not-openable";
    };
    if shell_info.e_tpgid != target.pbi_pgid {
        return "foreground-group-mismatch";
    }
    "capture-ready"
}

fn reset_bytes_after_kill(harness: &PtyHarness, checkpoint: usize) {
    assert!(
        !contains_from(&harness.transcript, CLEAR_REPORTING, checkpoint),
        "SIGKILL must leave terminal reporting modes for guarded recovery"
    );
}

#[test]
fn force_kill_leaves_reporting_modes_and_cleanup_resets_them_after_shell_resumes() {
    let mut harness = PtyHarness::spawn().expect("create isolated shell PTY");
    let target = harness.start_fixture().expect("start PTY fixture");
    let captured = harness.capture(&target).expect("capture own PTY");
    let checkpoint = harness.transcript.len();

    harness.terminate_force(&target);
    harness
        .wait_for_shell_prompt(&captured, &target, checkpoint, IO_TIMEOUT)
        .expect("shell to resume on its PTY");
    reset_bytes_after_kill(&harness, checkpoint);

    harness.drain_master().expect("drain before cleanup");
    let cleanup_checkpoint = harness.transcript.len();
    assert!(
        captured.cleanup(),
        "same-user resumed shell should be cleaned"
    );
    harness
        .wait_for_output(CLEAR_REPORTING, cleanup_checkpoint, IO_TIMEOUT)
        .expect("cleanup sequence on the isolated PTY");
    assert!(contains_from(
        &harness.transcript,
        CLEAR_REPORTING,
        cleanup_checkpoint
    ));
}

#[test]
fn cleanup_skips_when_a_different_foreground_program_owns_the_tty() {
    let mut harness = PtyHarness::spawn().expect("create isolated shell PTY");
    let first = harness.start_fixture().expect("start first fixture");
    let captured = harness.capture(&first).expect("capture first fixture PTY");
    let killed_checkpoint = harness.transcript.len();
    harness.terminate_force(&first);
    harness
        .wait_for_shell_prompt(&captured, &first, killed_checkpoint, IO_TIMEOUT)
        .expect("shell to resume after first fixture");
    reset_bytes_after_kill(&harness, killed_checkpoint);

    let second = harness.start_fixture().expect("start foreground fixture");
    let second_capture = harness
        .capture(&second)
        .expect("capture second fixture PTY");
    let tty = captured.0.as_ref().expect("first capture has a TTY");
    let shell_info = info(tty.shell.pid).expect("read the captured shell identity");
    let second_info = info(second.pid).expect("read the second fixture identity");
    assert_eq!(
        shell_info.e_tpgid, second_info.pbi_pgid,
        "the second fixture must own the foreground process group"
    );
    assert_ne!(shell_info.e_tpgid, tty.shell_group);

    let cleanup_checkpoint = harness.transcript.len();
    assert!(
        !captured.cleanup(),
        "cleanup must skip while another program owns the foreground"
    );
    harness.drain_master().expect("drain after guarded skip");
    assert!(!contains_from(
        &harness.transcript,
        CLEAR_REPORTING,
        cleanup_checkpoint
    ));

    let second_checkpoint = harness.transcript.len();
    harness.terminate_force(&second);
    harness
        .wait_for_shell_prompt(&second_capture, &second, second_checkpoint, IO_TIMEOUT)
        .expect("shell to resume after second fixture");
}

#[test]
fn cleanup_skips_when_captured_shell_pid_identity_changes() {
    let mut harness = PtyHarness::spawn().expect("create isolated shell PTY");
    let target = harness.start_fixture().expect("start PTY fixture");
    let mut captured = harness.capture(&target).expect("capture own PTY");
    let killed_checkpoint = harness.transcript.len();
    harness.terminate_force(&target);
    harness
        .wait_for_shell_prompt(&captured, &target, killed_checkpoint, IO_TIMEOUT)
        .expect("shell to resume on its PTY");
    reset_bytes_after_kill(&harness, killed_checkpoint);

    let cleanup_checkpoint = harness.transcript.len();
    let terminal = captured.0.as_mut().expect("capture has a TTY");
    terminal.shell.start.0 = terminal.shell.start.0.saturating_add(1);
    assert!(
        !captured.cleanup(),
        "a changed process identity must invalidate the captured shell"
    );
    harness.drain_master().expect("drain after identity guard");
    assert!(!contains_from(
        &harness.transcript,
        CLEAR_REPORTING,
        cleanup_checkpoint
    ));
}

#[test]
fn capture_skips_a_process_created_without_a_controlling_tty() {
    let mut command = Command::new("/bin/sleep");
    command
        .arg("30")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().expect("start isolated no-TTY process");
    let pid = child.id();
    let mut native = Native {
        system: System::new(),
        terminals: Vec::new(),
    };
    let entry = wait_for_entry(&mut native, pid, IO_TIMEOUT).expect("read own PID identity");
    let captured = capture(&[&entry], std::slice::from_ref(&entry));
    assert!(
        captured.into_iter().all(|candidate| !candidate.cleanup()),
        "a process with no controlling TTY must not receive terminal cleanup"
    );
    child.kill().expect("stop only the created no-TTY child");
    child.wait().expect("reap no-TTY child");
}
