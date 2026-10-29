use super::*;
use std::sync::atomic::Ordering;
use tokio::sync::oneshot;

fn scheduler(dir: &tempfile::TempDir, limit: u32) -> Scheduler {
    let scheduler = Scheduler::new(broadcast::channel(16).0, dir.path().join("rpm-window.json"));
    {
        let mut state = scheduler.0.state.lock().unwrap();
        state.running = true;
        state.versions.insert("fixture".into(), "v1".into());
        state.rpm_limits.insert("fixture".into(), limit);
        state.queued.insert("fixture".into());
    }
    scheduler
}
fn reserve(scheduler: &Scheduler) -> RpmReservation {
    let mut state = scheduler.0.state.lock().unwrap();
    let limit = state.rpm_limits["fixture"];
    scheduler
        .reserve_rpm(&mut state, "fixture", limit, true)
        .unwrap()
}
struct Release(std::sync::mpsc::Sender<()>);
impl Drop for Release {
    fn drop(&mut self) {
        let _ = self.0.send(());
    }
}
fn block_write(scheduler: &Scheduler) -> (oneshot::Receiver<()>, Release) {
    let (ready, resume) = scheduler.hold_rpm_write();
    (ready, Release(resume))
}
async fn pending(scheduler: &Scheduler, count: usize) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while scheduler.0.state.lock().unwrap().rpm_writes.receipts.len() != count {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
fn read_ledger(dir: &tempfile::TempDir) -> serde_json::Value {
    serde_json::from_slice(&std::fs::read(dir.path().join("rpm-window.json")).unwrap()).unwrap()
}

#[tokio::test]
async fn already_pending_starts_share_one_write_and_never_send_before_ack() {
    let dir = tempfile::tempdir().unwrap();
    let scheduler = scheduler(&dir, 4);
    let (ready, release) = block_write(&scheduler);
    let worker = scheduler.clone();
    let first = tokio::spawn(async move { worker.flush_rpm().await });
    ready.await.unwrap();
    let mut jobs = Vec::new();
    for _ in 0..4 {
        let mut reservation = reserve(&scheduler);
        jobs.push(tokio::spawn(async move {
            reservation.commit().await.unwrap();
            reservation
        }));
    }
    pending(&scheduler, 4).await;
    assert!(jobs.iter().all(|job| !job.is_finished()));
    assert!(matches!(
        reserve_attempt(&scheduler),
        Err(Rejected::RateLimited(_))
    ));
    // The scheduler remains readable while the only writer is blocked.
    assert_eq!(scheduler.rpm_status("fixture", 4).0, 4);
    drop(release);
    assert!(first.await.unwrap());
    let mut reservations = Vec::new();
    for job in jobs {
        reservations.push(job.await.unwrap());
    }
    assert_eq!(scheduler.0.rpm_writes.load(Ordering::SeqCst), 2);
    assert_eq!(
        read_ledger(&dir)["providers"]["fixture"]
            .as_array()
            .unwrap()
            .len(),
        4
    );
    drop(reservations);
    assert!(scheduler.flush_rpm().await);
}
fn reserve_attempt(scheduler: &Scheduler) -> Result<RpmReservation, Rejected> {
    let mut state = scheduler.0.state.lock().unwrap();
    let limit = state.rpm_limits["fixture"];
    scheduler.reserve_rpm(&mut state, "fixture", limit, true)
}

#[tokio::test]
async fn completion_release_waits_for_disk_and_keeps_its_observed_clock() {
    let dir = tempfile::tempdir().unwrap();
    let scheduler = scheduler(&dir, 1);
    let mut reservation = reserve(&scheduler);
    reservation.commit().await.unwrap();
    tokio::time::pause();
    let (ready, release) = block_write(&scheduler);
    reservation
        .completion
        .as_mut()
        .unwrap()
        .settle(Settlement::Failed);
    ready.await.unwrap();
    tokio::time::advance(super::super::rpm::WINDOW).await;
    assert!(scheduler.rpm_status("fixture", 1).2);
    drop(release);
    assert!(scheduler.flush_rpm().await);
    assert!(!scheduler.rpm_status("fixture", 1).2);

    let mut next = reserve(&scheduler);
    next.commit().await.unwrap();
    let (ready, release) = block_write(&scheduler);
    next.completion
        .as_mut()
        .unwrap()
        .settle(Settlement::Success);
    ready.await.unwrap();
    tokio::time::advance(super::super::rpm::WINDOW).await;
    assert!(scheduler.rpm_status("fixture", 1).2);
    drop(release);
    assert!(scheduler.flush_rpm().await);
    // Completion time is the observed event, not when a delayed disk finishes.
    assert!(!scheduler.rpm_status("fixture", 1).2);
}

#[tokio::test]
async fn failed_release_keeps_the_inflight_guard_and_reports_failure() {
    let dir = tempfile::tempdir().unwrap();
    let scheduler = scheduler(&dir, 1);
    let mut reservation = reserve(&scheduler);
    reservation.commit().await.unwrap();
    let path = dir.path().join("rpm-window.json");
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    reservation
        .completion
        .as_mut()
        .unwrap()
        .settle(Settlement::Failed);
    assert!(!scheduler.flush_rpm().await);
    assert!(scheduler.rpm_status("fixture", 1).3);
    assert!(matches!(
        reserve_attempt(&scheduler),
        Err(Rejected::RateLedger)
    ));
}

#[tokio::test]
async fn cancellation_during_write_retains_recoverable_uncertainty() {
    let dir = tempfile::tempdir().unwrap();
    let scheduler = scheduler(&dir, 1);
    let (ready, release) = block_write(&scheduler);
    let mut reservation = reserve(&scheduler);
    let task = tokio::spawn(async move { reservation.commit().await });
    ready.await.unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    drop(release);
    assert!(scheduler.flush_rpm().await);
    let saved = read_ledger(&dir);
    let inflight = saved["protection"]["fixture"]["inflight"]
        .as_object()
        .unwrap();
    assert_eq!(inflight.len(), 1);
    assert!(inflight.values().all(serde_json::Value::is_number));
    let (recovered, error) = Ledger::load(&dir.path().join("rpm-window.json"));
    assert!(!error);
    assert!(recovered.gate("fixture", 1, 0).is_some());
}

#[tokio::test]
async fn commit_rechecks_stop_credentials_queue_and_limits_after_write() {
    for change in [
        "stop",
        "credentials",
        "dequeue",
        "enable_limit",
        "remove_limit",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let scheduler = scheduler(&dir, if change == "enable_limit" { 0 } else { 2 });
        let (ready, release) = block_write(&scheduler);
        let mut reservation = reserve(&scheduler);
        let task = tokio::spawn(async move { reservation.commit().await });
        ready.await.unwrap();
        {
            let mut state = scheduler.0.state.lock().unwrap();
            match change {
                "stop" => {
                    state.running = false;
                    state.epoch += 1;
                }
                "credentials" => {
                    state.versions.insert("fixture".into(), "v2".into());
                }
                "dequeue" => {
                    state.queued.clear();
                }
                "enable_limit" => {
                    state.rpm_limits.insert("fixture".into(), 1);
                    state.rpm.enable("fixture");
                    state.rpm_writes.dirty = true;
                }
                "remove_limit" => {
                    state.rpm_limits.insert("fixture".into(), 0);
                }
                _ => unreachable!(),
            }
        }
        scheduler.signal();
        drop(release);
        assert_eq!(
            task.await.unwrap(),
            Err(if change == "stop" {
                Rejected::Stopped
            } else {
                Rejected::Unavailable
            })
        );
        assert!(scheduler.flush_rpm().await);
    }
}

#[tokio::test]
async fn corrupt_ledger_stays_untouched_even_for_unlimited_requests() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rpm-window.json");
    std::fs::write(&path, b"fixture invalid ledger").unwrap();
    let scheduler = scheduler(&dir, 0);
    let mut reservation = reserve(&scheduler);
    reservation.commit().await.unwrap();
    drop(reservation);
    assert!(!scheduler.flush_rpm().await);
    assert_eq!(std::fs::read(&path).unwrap(), b"fixture invalid ledger");
    scheduler
        .0
        .state
        .lock()
        .unwrap()
        .rpm_limits
        .insert("fixture".into(), 1);
    assert!(matches!(
        reserve_attempt(&scheduler),
        Err(Rejected::RateLedger)
    ));
}
