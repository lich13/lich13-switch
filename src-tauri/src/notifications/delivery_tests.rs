use super::delivery::Gate;
use std::time::{Duration, Instant};

#[test]
fn first_send_is_allowed_and_duplicate_inflight_send_is_blocked() {
    let mut gate = Gate::default();
    let now = Instant::now();

    assert!(gate.begin("same-notice", now));
    assert!(!gate.begin("same-notice", now + Duration::from_secs(1)));
}

#[test]
fn accepted_notice_is_suppressed_for_300_seconds() {
    let mut gate = Gate::default();
    let now = Instant::now();

    assert!(gate.begin("accepted-notice", now));
    gate.finish("accepted-notice", true, now);

    assert!(!gate.begin("accepted-notice", now + Duration::from_secs(299)));
    assert!(gate.begin("accepted-notice", now + Duration::from_secs(300)));
}

#[test]
fn failed_notice_cools_down_for_five_seconds_without_becoming_accepted() {
    let mut gate = Gate::default();
    let now = Instant::now();

    assert!(gate.begin("failed-notice", now));
    gate.finish("failed-notice", false, now);

    assert!(!gate.begin("failed-notice", now + Duration::from_secs(4)));
    assert!(gate.begin("failed-notice", now + Duration::from_secs(5)));
}

#[test]
fn pending_queue_is_limited_to_64_and_finish_releases_capacity() {
    let mut gate = Gate::default();
    let now = Instant::now();

    for index in 0..64 {
        assert!(gate.begin(&format!("notice-{index}"), now));
    }
    assert!(!gate.begin("overflow", now));

    gate.finish("notice-0", false, now);
    assert!(gate.begin("replacement", now));
}
