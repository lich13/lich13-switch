use super::Scheduler;
use serde_json::Value;
use std::{
    fs,
    path::Path,
    thread,
    time::{Duration, Instant},
};

fn scheduler(dir: &tempfile::TempDir) -> Scheduler {
    let (events, _receiver) = tokio::sync::broadcast::channel(16);
    Scheduler::new(events, dir.path().join("rpm-window.json"))
}

fn reservation_path(dir: &tempfile::TempDir) -> std::path::PathBuf {
    dir.path().join("thread-reservations.json")
}

fn wait_until(mut ready: impl FnMut() -> bool, description: &str) {
    // Persistence is asynchronous; this test checks recovery, not disk latency.
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if ready() {
            return;
        }
        thread::sleep(Duration::from_millis(5));
    }
    assert!(ready(), "timed out waiting for {description}");
}

fn reservations(path: &Path) -> Option<Vec<Value>> {
    let bytes = fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn reservation<'a>(entries: &'a [Value], thread: &str, provider: &str) -> Option<&'a Value> {
    entries
        .iter()
        .find(|entry| entry["thread"] == thread && entry["provider"] == provider)
}

fn add_active(scheduler: &Scheduler, thread: &str, provider: &str) {
    let mut state = scheduler.0.state.lock().unwrap();
    state.retention.seconds = 300;
    let ticket = state.retention.begin(thread, None);
    assert!(state.retention.admit(ticket, provider, true));
}

fn add_idle(scheduler: &Scheduler, thread: &str, provider: &str) {
    let mut state = scheduler.0.state.lock().unwrap();
    state.retention.seconds = 300;
    let ticket = state.retention.begin(thread, None);
    assert!(state.retention.admit(ticket, provider, true));
    state.retention.release(ticket, provider, true);
    state.retention.finish(ticket);
}

#[test]
fn transient_write_failure_recovers_without_clearing_active_or_recent_retention() {
    let dir = tempfile::tempdir().unwrap();
    let path = reservation_path(&dir);
    let scheduler = scheduler(&dir);

    scheduler.signal();
    wait_until(
        || {
            fs::read(&path)
                .map(|bytes| bytes.as_slice() == b"[]")
                .unwrap_or(false)
        },
        "initial empty reservation snapshot",
    );

    let active_thread = crate::storage::digest(b"fixture-retention-active");
    let recent_thread = crate::storage::digest(b"fixture-retention-recent");
    let changed_thread = crate::storage::digest(b"fixture-retention-changed");
    add_active(&scheduler, &active_thread, "fixture-active-provider");
    add_idle(&scheduler, &recent_thread, "fixture-recent-provider");
    scheduler.signal();

    wait_until(
        || {
            reservations(&path).is_some_and(|entries| {
                entries.len() == 2
                    && reservation(&entries, &active_thread, "fixture-active-provider")
                        .is_some_and(|entry| entry["busy"] == true)
                    && reservation(&entries, &recent_thread, "fixture-recent-provider")
                        .is_some_and(|entry| entry["busy"] == false)
            })
        },
        "active and recent reservations to persist",
    );

    add_idle(&scheduler, &changed_thread, "fixture-changed-provider");
    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    scheduler.signal();
    wait_until(
        || scheduler.retention_error(),
        "transient persistence error",
    );

    fs::remove_dir(&path).unwrap();
    scheduler.signal();
    wait_until(
        || {
            !scheduler.retention_error()
                && reservations(&path).is_some_and(|entries| {
                    entries.len() == 3
                        && reservation(&entries, &active_thread, "fixture-active-provider")
                            .is_some_and(|entry| entry["busy"] == true)
                        && reservation(&entries, &recent_thread, "fixture-recent-provider")
                            .is_some_and(|entry| entry["busy"] == false)
                        && reservation(&entries, &changed_thread, "fixture-changed-provider")
                            .is_some_and(|entry| entry["busy"] == false)
                })
        },
        "recovered active and recent reservations",
    );
    assert_eq!(scheduler.0.state.lock().unwrap().retention.seconds, 300);
}

#[test]
fn final_idle_snapshot_recovers_from_transient_write_failure_without_another_signal() {
    let dir = tempfile::tempdir().unwrap();
    let path = reservation_path(&dir);
    let scheduler = scheduler(&dir);
    let thread = crate::storage::digest(b"fixture-retention-final-idle");
    let latest = crate::storage::digest(b"fixture-retention-during-retry");
    fs::create_dir(&path).unwrap();
    {
        let mut state = scheduler.0.state.lock().unwrap();
        let ticket = state.retention.begin(&thread, None);
        assert!(state.retention.admit(ticket, "fixture-provider", true));
        state.retention.release(ticket, "fixture-provider", true);
        state.retention.finish(ticket);
    }
    scheduler.signal();
    wait_until(|| scheduler.retention_error(), "failed final idle write");
    add_idle(&scheduler, &latest, "fixture-latest-provider");
    fs::remove_dir(&path).unwrap();

    // There is no subsequent request or UI event to trigger another signal.
    wait_until(
        || {
            !scheduler.retention_error()
                && reservations(&path).is_some_and(|entries| {
                    entries.len() == 2
                        && reservation(&entries, &thread, "fixture-provider")
                            .is_some_and(|entry| entry["busy"] == false)
                        && reservation(&entries, &latest, "fixture-latest-provider")
                            .is_some_and(|entry| entry["busy"] == false)
                })
        },
        "idle snapshot recovery without another signal",
    );
}

#[test]
fn recovery_retries_when_current_snapshot_matches_last_good_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let path = reservation_path(&dir);
    let scheduler = scheduler(&dir);

    scheduler.signal();
    wait_until(
        || {
            fs::read(&path)
                .map(|bytes| bytes.as_slice() == b"[]")
                .unwrap_or(false)
        },
        "initial empty last-good snapshot",
    );

    let thread = crate::storage::digest(b"fixture-retention-changed-then-empty");
    add_idle(&scheduler, &thread, "fixture-provider");
    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    scheduler.signal();
    wait_until(
        || scheduler.retention_error(),
        "failed changed snapshot write",
    );

    {
        let mut state = scheduler.0.state.lock().unwrap();
        state.retention.configure(&Default::default(), 300);
    }
    fs::remove_dir(&path).unwrap();
    scheduler.signal();
    wait_until(
        || {
            !scheduler.retention_error()
                && fs::read(&path)
                    .map(|bytes| bytes.as_slice() == b"[]")
                    .unwrap_or(false)
        },
        "retry of snapshot equal to last-good",
    );
}

#[test]
fn corrupt_loaded_reservations_are_not_overwritten() {
    let dir = tempfile::tempdir().unwrap();
    let path = reservation_path(&dir);
    let original = b"{corrupt fixture";
    fs::write(&path, original).unwrap();
    let scheduler = scheduler(&dir);
    assert!(scheduler.retention_error());

    for _ in 0..20 {
        scheduler.signal();
        thread::sleep(Duration::from_millis(5));
    }

    assert!(scheduler.retention_error());
    assert_eq!(fs::read(&path).unwrap(), original);
}
