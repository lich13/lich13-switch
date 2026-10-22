use super::*;

#[derive(Default)]
struct Fake {
    items: Vec<Entry>,
    calls: Vec<(u32, Termination)>,
    changed: HashSet<u32>,
    gone: HashSet<u32>,
    graceful_exits: HashSet<u32>,
    stuck: HashSet<u32>,
    waits: Vec<Duration>,
    terminal_events: Vec<&'static str>,
    prepared_terminal_targets: Vec<Vec<u32>>,
    terminal_cleanup_failures: usize,
}
impl Processes for Fake {
    fn inventory(&mut self) -> Vec<Entry> {
        self.items.clone()
    }
    fn state(&mut self, e: &Entry) -> State {
        if self.changed.contains(&e.pid) {
            State::Changed
        } else if self.gone.contains(&e.pid) {
            State::Gone
        } else {
            State::Running
        }
    }
    fn terminate_if_same(&mut self, e: &Entry, how: Termination) {
        if self.state(e) != State::Running {
            return;
        }
        self.terminal_events.push(match how {
            Termination::Graceful => "graceful",
            Termination::Force => "force",
        });
        self.calls.push((e.pid, how));
        if !self.stuck.contains(&e.pid)
            && (how == Termination::Force || self.graceful_exits.contains(&e.pid))
        {
            self.gone.insert(e.pid);
        }
    }
    fn wait_for_exit(&mut self, _entries: &[&Entry], timeout: Duration) {
        self.terminal_events.push("wait");
        self.waits.push(timeout);
    }
    fn prepare_terminals(&mut self, entries: &[&Entry], _inventory: &[Entry]) {
        self.terminal_events.push("prepare");
        self.prepared_terminal_targets
            .push(entries.iter().map(|entry| entry.pid).collect());
    }
    fn cleanup_terminals(&mut self) -> usize {
        self.terminal_events.push("cleanup");
        self.terminal_cleanup_failures
    }
}
fn entry(pid: u32, parent: Option<u32>, name: &str, official: bool) -> Entry {
    Entry {
        pid,
        parent,
        started: 1,
        executable: name.into(),
        user: "fixture-user".into(),
        official_app: official,
        claude_code: false,
    }
}

#[test]
fn claude_code_matches_cli_installations_and_node_entry_not_desktop_or_prompt_text() {
    let args = |v: &[&str]| v.iter().map(std::ffi::OsString::from).collect::<Vec<_>>();
    for path in [
        "/fixture/.local/bin/claude",
        "/fixture/.local/share/claude/versions/2.0.0",
        "/fixture/Caskroom/claude-code/2.0.0/claude",
        "/fixture/node_modules/@anthropic-ai/claude-code/claude.exe",
        "C:\\fixture\\.local\\bin\\claude.exe",
    ] {
        assert!(claude_code(Path::new(path), &[]), "{path}");
    }
    let node = Path::new("/bin/node");
    assert!(claude_code(
        node,
        &args(&[
            "node",
            "/fixture/node_modules/@anthropic-ai/claude-code/cli.js"
        ])
    ));
    assert!(!claude_code(
        Path::new("/fixture/node"),
        &[
            "node".into(),
            "--eval".into(),
            "/fixture/node_modules/@anthropic-ai/claude-code/cli.js".into()
        ]
    ));
    assert!(!claude_code(
        node,
        &args(&[
            "node",
            "/fixture/server.js",
            "/fixture/node_modules/@anthropic-ai/claude-code/cli.js"
        ])
    ));
    assert!(!claude_code(
        Path::new("/Applications/Claude.app/Contents/MacOS/Claude"),
        &[]
    ));
    assert!(!claude_code(
        Path::new("C:\\fixture\\Claude\\claude.exe"),
        &[]
    ));
    assert!(!claude_code(
        Path::new("/fixture/node_modules/@anthropic-ai/claude-code/vendor/rg"),
        &[]
    ));
}

#[cfg(unix)]
#[test]
fn native_inventory_and_identity_recheck_read_child_arguments_and_user() {
    struct OwnedChild(std::process::Child);
    impl Drop for OwnedChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let child = OwnedChild(
        std::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .unwrap(),
    );
    let pid = Pid::from_u32(child.0.id());
    let mut native = Native {
        system: System::new(),
        #[cfg(target_os = "macos")]
        terminals: Vec::new(),
    };
    let entry = native
        .inventory()
        .into_iter()
        .find(|p| p.pid == pid.as_u32())
        .unwrap();
    assert_eq!(entry.parent, Some(std::process::id()));
    assert!(!entry.user.is_empty());
    assert_eq!(
        native
            .system
            .process(pid)
            .unwrap()
            .cmd()
            .get(1)
            .and_then(|arg| arg.to_str()),
        Some("30")
    );
    native.system = System::new();
    assert_eq!(native.state(&entry), State::Running);
    assert_eq!(
        native
            .system
            .process(pid)
            .unwrap()
            .cmd()
            .get(1)
            .and_then(|arg| arg.to_str()),
        Some("30")
    );
}

#[test]
fn quitting_claude_code_preserves_desktop_and_other_users() {
    let mut cli = entry(2, None, "/fixture/.local/bin/claude", false);
    cli.claude_code = true;
    let mut other = cli.clone();
    other.pid = 6;
    other.user = "other-fixture".into();
    let mut fake = Fake {
        items: vec![
            entry(1, None, "/app/switch", false),
            cli,
            entry(3, Some(2), "/bin/worker", false),
            entry(
                4,
                None,
                "/Applications/Claude.app/Contents/MacOS/Claude",
                false,
            ),
            entry(5, Some(4), "/bin/helper", false),
            other,
        ],
        ..Fake::default()
    };
    let result = run(&mut fake, 1).unwrap();
    assert_eq!(result.terminated, 2);
    assert!(fake.calls.iter().all(|(pid, _)| [2, 3].contains(pid)));
}
#[test]
fn exact_targets_preserve_other_users_own_children_and_changed_identities() {
    let mut other = entry(8, None, "/bin/codex", false);
    other.user = "another-fixture".into();
    let mut f = Fake {
        items: vec![
            entry(1, None, "/app/switch", false),
            entry(2, None, "/app/ChatGPT", true),
            entry(3, Some(2), "/node", false),
            entry(4, None, "/bin/codex", false),
            entry(5, None, "/app/codexhost", false),
            entry(6, Some(1), "/bin/codex", false),
            entry(7, None, "/extension/ChatGPT for Chrome", false),
            other,
        ],
        changed: HashSet::from([4]),
        ..Fake::default()
    };
    let out = run(&mut f, 1).unwrap();
    assert_eq!((out.terminated, out.failed), (2, 1));
    assert_eq!(
        f.calls,
        [
            (2, Termination::Graceful),
            (3, Termination::Force),
            (2, Termination::Force)
        ]
    );
    assert_eq!(f.waits, [GRACE, EXIT_CONFIRMATION]);
}
#[test]
fn graceful_exit_avoids_kill_and_signal_acceptance_does_not_mean_exit() {
    let mut f = Fake {
        items: vec![
            entry(1, None, "/app/switch", false),
            entry(2, None, "/bin/codex", false),
            entry(3, None, "/bin/codex", false),
        ],
        graceful_exits: HashSet::from([2]),
        stuck: HashSet::from([3]),
        ..Fake::default()
    };
    let out = run(&mut f, 1).unwrap();
    assert_eq!((out.terminated, out.failed), (1, 1));
    assert!(f.calls.contains(&(2, Termination::Graceful)));
    assert!(!f.calls.contains(&(2, Termination::Force)));
    assert!(f.calls.contains(&(3, Termination::Force)));
    assert_eq!(
        f.waits.len(),
        2,
        "one common deadline per phase, not per process"
    );
}
#[test]
fn reparented_descendant_is_the_same_process_but_reused_pid_is_not() {
    let original = entry(2, Some(3), "/node", false);
    let mut changed = original.clone();
    changed.parent = Some(1);
    assert!(original.same_process(&changed));
    changed.started += 1;
    assert!(!original.same_process(&changed));
}

#[test]
fn terminal_recovery_brackets_shutdown_and_reports_cleanup_failures() {
    let mut f = Fake {
        items: vec![
            entry(1, None, "/app/switch", false),
            entry(2, None, "/bin/codex", false),
        ],
        graceful_exits: HashSet::from([2]),
        terminal_cleanup_failures: 2,
        ..Fake::default()
    };

    let out = run(&mut f, 1).unwrap();

    assert_eq!(
        (out.terminated, out.failed, out.terminal_cleanup_failed),
        (1, 0, 2)
    );
    assert_eq!(f.prepared_terminal_targets, [vec![2]]);
    assert_eq!(f.calls, [(2, Termination::Graceful)]);
    assert_eq!(
        f.terminal_events,
        ["prepare", "graceful", "wait", "wait", "cleanup"]
    );
}
