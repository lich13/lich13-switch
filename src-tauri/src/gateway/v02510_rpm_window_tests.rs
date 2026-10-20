//! Independent upstream window models driven by the real local ledger.
use super::*;

fn assert_upstream_limits(times: &[Duration], limit: u32) {
    let window = Duration::from_secs(60);
    for (index, at) in times.iter().enumerate() {
        let in_window = times[..=index]
            .iter()
            .rev()
            .take_while(|earlier| at.saturating_sub(**earlier) < window)
            .count();
        assert!(
            in_window <= limit as usize,
            "rolling window exceeded {limit}"
        );
    }
    for offset in [0, 13_000, 59_000] {
        let mut minutes = HashMap::<u128, usize>::new();
        for time in times {
            *minutes
                .entry((time.as_millis() + offset) / 60_000)
                .or_default() += 1;
        }
        assert!(
            minutes.values().all(|count| *count <= limit as usize),
            "fixed minute window exceeded {limit}, offset={offset}"
        );
    }
    // Integer credit units avoid floating-point tolerance in the upstream
    // total-request token bucket model: one request costs sixty seconds.
    let cost = window.as_nanos();
    let capacity = cost * u128::from(limit);
    let mut credit = capacity;
    let mut previous = Duration::ZERO;
    for time in times {
        credit =
            (credit + time.saturating_sub(previous).as_nanos() * u128::from(limit)).min(capacity);
        assert!(
            credit >= cost,
            "upstream token bucket exhausted for {limit}"
        );
        credit -= cost;
        previous = *time;
    }
}

#[tokio::test(start_paused = true)]
async fn local_scheduler_obeys_fixed_minutes_token_buckets_and_out_of_order_success_windows() {
    for limit in [1, 3, 10, 60] {
        let base = Instant::now();
        let mut ledger = Ledger::default();
        let mut active = Vec::<(Instant, String, bool)>::new();
        let mut starts = Vec::new();
        let mut successes = Vec::new();
        let mut sent = 0;
        while sent < 80 || !active.is_empty() {
            let now = Instant::now();
            ledger.prune();
            let mut index = 0;
            while index < active.len() {
                if active[index].0 > now {
                    index += 1;
                    continue;
                }
                let (_, token, succeeded) = active.swap_remove(index);
                ledger.settle(
                    "fixture-provider",
                    &token,
                    if succeeded {
                        successes.push(now.duration_since(base));
                        Settlement::Success
                    } else {
                        Settlement::Failed
                    },
                );
            }
            if sent < 80 && ledger.gate("fixture-provider", limit, 0).is_none() {
                let token = ledger.commit("fixture-provider", limit).unwrap();
                starts.push(now.duration_since(base));
                let duration = Duration::from_millis([400, 69_000, 17_000, 100, 95_000][sent % 5]);
                active.push((now + duration, token, sent % 11 != 0));
                sent += 1;
                continue;
            }
            let next_event = active.iter().map(|(at, _, _)| *at).min();
            let next_send = (sent < 80)
                .then(|| ledger.gate("fixture-provider", limit, 0))
                .flatten()
                .map(|(delay, _)| now + delay);
            let next = next_event.into_iter().chain(next_send).min();
            if let Some(next) = next {
                tokio::time::advance(next.saturating_duration_since(now)).await;
            }
        }
        assert_eq!(starts.len(), 80);
        assert_eq!(successes.len(), 72);
        assert_upstream_limits(&starts, limit);
        assert_upstream_limits(&successes, limit);
    }
}
