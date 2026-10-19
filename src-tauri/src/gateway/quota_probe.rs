//! Fresh, cancellable billing evidence. Never route from a cached or rounded amount.
use super::*;
use crate::events::{QuotaBasis, QuotaCheck, QuotaSource, QuotaVerdict};
use std::{cmp::Ordering as Cmp, sync::Weak};
use tokio::{sync::Notify, task::AbortHandle, time::Instant};

const DEADLINE: Duration = Duration::from_secs(10);
#[cfg(test)]
#[path = "v02510_quota_probe_tests.rs"]
mod v02510_quota_probe_tests;
#[derive(Default)]
pub(super) struct ProbeStore {
    entries: HashMap<(String, String), ProbeEntry>,
}
struct ProbeEntry {
    next: Instant,
    retry_at: u64,
    flight: Weak<Flight>,
    last: Option<QuotaCheck>,
}
impl ProbeStore {
    pub(super) fn retain(&mut self, versions: &HashMap<String, String>) {
        self.entries.retain(|(id, version), entry| {
            let keep = versions.get(id) == Some(version);
            if !keep {
                if let Some(flight) = entry.flight.upgrade() {
                    if let Some(task) = flight.abort.lock().unwrap().take() {
                        task.abort();
                    }
                    *flight.result.lock().unwrap() = Some(unknown());
                    flight.ready.notify_waiters();
                }
            }
            keep
        });
    }
}
struct Flight {
    result: Mutex<Option<QuotaCheck>>,
    ready: Notify,
    abort: Mutex<Option<AbortHandle>>,
}
impl Drop for Flight {
    fn drop(&mut self) {
        if let Some(task) = self.abort.get_mut().unwrap().take() {
            task.abort();
        }
    }
}
fn evidence(verdict: QuotaVerdict, source: QuotaSource, basis: QuotaBasis) -> QuotaCheck {
    QuotaCheck {
        verdict,
        source,
        basis,
        checked_at: now(),
    }
}
fn unknown() -> QuotaCheck {
    evidence(
        QuotaVerdict::Unknown,
        QuotaSource::Unknown,
        QuotaBasis::QueryFailed,
    )
}
impl Service {
    /// Shared with display refreshes: an error probe cannot bypass a refresh's
    /// Retry-After, and a refresh cannot bypass a probe's Retry-After.
    pub(super) fn query_retry_at(&self, input: &Query) -> Option<u64> {
        let probe = self
            .0
            .probes
            .lock()
            .unwrap()
            .entries
            .get(&(input.id.clone(), input.version.clone()))
            .map(|entry| entry.retry_at);
        probe
            .max(
                self.cached(&input.id, &input.version)
                    .and_then(|v| v.retry_at),
            )
            .filter(|at| *at > now())
    }
    pub(in crate::gateway) fn probe_view(
        &self,
        id: &str,
        version: &str,
    ) -> (bool, Option<QuotaCheck>) {
        let probes = self.0.probes.lock().unwrap();
        probes
            .entries
            .get(&(id.into(), version.into()))
            .map_or((false, None), |entry| {
                (
                    entry
                        .flight
                        .upgrade()
                        .is_some_and(|f| f.result.lock().unwrap().is_none()),
                    entry.last.clone(),
                )
            })
    }
    pub(in crate::gateway) async fn probe(&self, input: Query) -> QuotaCheck {
        let deadline = Instant::now() + DEADLINE;
        let flight = {
            let cached_retry = self
                .cached(&input.id, &input.version)
                .and_then(|v| v.retry_at)
                .unwrap_or(0);
            let mut probes = self.0.probes.lock().unwrap();
            let key = (input.id.clone(), input.version.clone());
            let entry = probes
                .entries
                .entry(key.clone())
                .or_insert_with(|| ProbeEntry {
                    next: Instant::now(),
                    retry_at: 0,
                    flight: Weak::new(),
                    last: None,
                });
            if let Some(flight) = entry
                .flight
                .upgrade()
                .filter(|f| f.result.lock().unwrap().is_none())
            {
                flight
            } else {
                let start_at = entry.next.max(
                    Instant::now()
                        + Duration::from_secs(
                            entry.retry_at.max(cached_retry).saturating_sub(now()),
                        ),
                );
                if start_at >= deadline {
                    let check = unknown();
                    entry.last = Some(check.clone());
                    return check;
                }
                let flight = Arc::new(Flight {
                    result: Mutex::new(None),
                    ready: Notify::new(),
                    abort: Mutex::new(None),
                });
                entry.flight = Arc::downgrade(&flight);
                let weak = Arc::downgrade(&flight);
                let service = self.clone();
                let task = tokio::spawn(async move {
                    let result = tokio::time::timeout_at(deadline, async {
                        tokio::time::sleep_until(start_at).await;
                        let _permit = loop {
                            let permit = service
                                .0
                                .permits
                                .acquire()
                                .await
                                .map_err(|_| Failure::new("额度查询已取消"))?;
                            if let Some(at) = service.query_retry_at(&input) {
                                drop(permit);
                                tokio::time::sleep(Duration::from_secs(at.saturating_sub(now())))
                                    .await;
                                continue;
                            }
                            break permit;
                        };
                        {
                            let mut probes = service.0.probes.lock().unwrap();
                            let entry = probes
                                .entries
                                .get_mut(&key)
                                .filter(|entry| Weak::ptr_eq(&entry.flight, &weak))
                                .ok_or_else(|| Failure::new("额度查询已取消"))?;
                            // Throttle real probe starts, including time spent
                            // waiting for a shared HTTP permit. A cancelled queue
                            // reservation does not consume another ten seconds.
                            entry.next = Instant::now() + DEADLINE;
                        }
                        fetch_probe(&input).await
                    })
                    .await;
                    let check = match result {
                        Ok(Ok(check)) => check,
                        Ok(Err(failure)) => {
                            if let Some(at) = failure.retry_at {
                                if let Some(entry) =
                                    service.0.probes.lock().unwrap().entries.get_mut(&key)
                                {
                                    entry.retry_at = entry.retry_at.max(at);
                                }
                            }
                            unknown()
                        }
                        Err(_) => unknown(),
                    };
                    if let Some(entry) = service
                        .0
                        .probes
                        .lock()
                        .unwrap()
                        .entries
                        .get_mut(&key)
                        .filter(|entry| Weak::ptr_eq(&entry.flight, &weak))
                    {
                        entry.last = Some(check.clone());
                    }
                    if let Some(flight) = weak.upgrade() {
                        *flight.result.lock().unwrap() = Some(check);
                        flight.ready.notify_waiters();
                    }
                });
                *flight.abort.lock().unwrap() = Some(task.abort_handle());
                flight
            }
        };
        loop {
            let notified = flight.ready.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(result) = flight.result.lock().unwrap().clone() {
                return result;
            }
            notified.await;
        }
    }
}
async fn fetch_probe(input: &Query) -> std::result::Result<QuotaCheck, Failure> {
    let (sub, new, _) = urls(&input.base)?;
    let first = get(input, sub, true).await?;
    if (200..300).contains(&first.status) || matches!(first.status, 401 | 403) {
        if let Some(value) = first.body.as_ref().filter(|v| {
            v.is_object()
                && v.get("error").is_none()
                && (v.get("mode").is_some()
                    || [
                        "balance",
                        "remaining",
                        "quota",
                        "subscription",
                        "subscriptions",
                        "rate_limits",
                    ]
                    .iter()
                    .any(|key| v.get(key).is_some()))
        }) {
            return Ok(sub2_evidence(value));
        }
    }
    let second = get(input, new, true).await?;
    if (200..300).contains(&second.status) {
        if let Some(value) = second.body.as_ref().filter(|v| {
            (v["code"] == true || v["success"] == true) && v["data"]["object"] == "token_usage"
        }) {
            return Ok(new_api_evidence(value));
        }
    }
    Ok(unknown())
}

// Bounded decimal comparison, without binary floats or scale truncation. Values
// outside these generous limits remain unknown, rather than becoming zero.
#[derive(Debug)]
struct Number {
    negative: bool,
    digits: Vec<u8>,
    exponent: i32,
}
impl Number {
    fn parse(value: &Value) -> Option<Self> {
        let text = match value {
            Value::Number(n) => n.to_string(),
            Value::String(s) => s.clone(),
            _ => return None,
        };
        if text.len() > 256 || text.trim() != text {
            return None;
        }
        serde_json::from_str::<serde_json::Number>(&text).ok()?;
        let negative = text.starts_with('-');
        let text = text.strip_prefix('-').unwrap_or(&text);
        let (mantissa, exp) = text.split_once(['e', 'E']).unwrap_or((text, "0"));
        let exp: i32 = exp.parse().ok()?;
        if exp.unsigned_abs() > 4096 {
            return None;
        }
        let scale = mantissa.split_once('.').map_or(0, |(_, s)| s.len() as i32);
        let digits: Vec<_> = mantissa
            .bytes()
            .filter(|b| *b != b'.')
            .skip_while(|b| *b == b'0')
            .collect();
        Some(Self {
            negative: negative && !digits.is_empty(),
            digits,
            exponent: exp - scale,
        })
    }
    fn sign(&self) -> Cmp {
        if self.digits.is_empty() {
            Cmp::Equal
        } else if self.negative {
            Cmp::Less
        } else {
            Cmp::Greater
        }
    }
    fn cmp(&self, other: &Self) -> Cmp {
        let sign = self.sign().cmp(&other.sign());
        if sign != Cmp::Equal {
            return sign;
        }
        if self.digits.is_empty() {
            return Cmp::Equal;
        }
        let order = (self.digits.len() as i32 + self.exponent)
            .cmp(&(other.digits.len() as i32 + other.exponent));
        let order = if order == Cmp::Equal {
            (0..self.digits.len().max(other.digits.len()))
                .map(|i| {
                    self.digits
                        .get(i)
                        .unwrap_or(&b'0')
                        .cmp(other.digits.get(i).unwrap_or(&b'0'))
                })
                .find(|v| *v != Cmp::Equal)
                .unwrap_or(Cmp::Equal)
        } else {
            order
        };
        if self.negative {
            order.reverse()
        } else {
            order
        }
    }
}
fn remaining(value: &Value) -> Option<QuotaVerdict> {
    Some(if Number::parse(value)?.sign() == Cmp::Greater {
        QuotaVerdict::Available
    } else {
        QuotaVerdict::Exhausted
    })
}
fn finite_window(
    value: &Value,
    limit: &str,
    used: &str,
    remaining_key: &str,
) -> Option<QuotaVerdict> {
    let limit = Number::parse(value.get(limit)?)?;
    if limit.sign() != Cmp::Greater {
        return None;
    } // These source limits use zero for unlimited/disabled.
    let by_used = match value.get(used) {
        Some(v) => {
            let used = Number::parse(v)?;
            if used.sign() == Cmp::Less {
                return None;
            }
            Some(if used.cmp(&limit) == Cmp::Less {
                QuotaVerdict::Available
            } else {
                QuotaVerdict::Exhausted
            })
        }
        None => None,
    };
    let by_remaining = match value.get(remaining_key) {
        Some(v) => Some(remaining(v)?),
        None => None,
    };
    if by_used.is_some() && by_remaining.is_some() && by_used != by_remaining {
        return None;
    }
    by_used.or(by_remaining)
}
fn sub2_evidence(value: &Value) -> QuotaCheck {
    use QuotaBasis as B;
    use QuotaVerdict as V;
    let result = |verdict, basis| evidence(verdict, QuotaSource::Sub2Api, basis);
    let mode = value["mode"].as_str().unwrap_or("");
    if value["status"]
        .as_str()
        .is_some_and(|s| !matches!(s, "active" | "quota_exhausted"))
    {
        return result(V::Unknown, B::Incomplete);
    }
    // /v1/usage quota_limited is scoped to the bearer Key. Monetary windows
    // are deliberately restricted to the source's 5h/1d/7d vocabulary.
    if mode == "quota_limited" {
        let mut available = false;
        let mut incomplete = false;
        if let Some(q) = value.get("quota") {
            match finite_window(q, "limit", "used", "remaining") {
                Some(V::Exhausted) => return result(V::Exhausted, B::KeyQuota),
                Some(V::Available) => available = true,
                _ => incomplete = true,
            }
        }
        if let Some(windows) = value["rate_limits"].as_array() {
            for window in windows {
                if !matches!(window["window"].as_str(), Some("5h" | "1d" | "7d")) {
                    continue;
                }
                match finite_window(window, "limit", "used", "remaining") {
                    Some(V::Exhausted) => return result(V::Exhausted, B::SpendingWindow),
                    Some(V::Available) => available = true,
                    _ => incomplete = true,
                }
            }
        }
        return result(
            if available && !incomplete {
                V::Available
            } else {
                V::Unknown
            },
            B::KeyQuota,
        );
    }
    if value.get("subscriptions").is_some() {
        return result(V::Unknown, B::Incomplete);
    }
    if value
        .get("subscription")
        .is_some_and(|s| !s.is_null() && !s.is_object())
    {
        return result(V::Unknown, B::Incomplete);
    }
    if let Some(sub) = value.get("subscription").filter(|v| v.is_object()) {
        if !matches!(mode, "unrestricted" | "subscription")
            || sub["active"] == false
            || sub["status"].as_str().is_some_and(|s| s != "active")
        {
            return result(V::Unknown, B::Incomplete);
        }
        let mut available = false;
        let mut incomplete = false;
        for (limit, used) in [
            ("daily_limit_usd", "daily_usage_usd"),
            ("weekly_limit_usd", "weekly_usage_usd"),
            ("monthly_limit_usd", "monthly_usage_usd"),
        ] {
            if let Some(v) = sub.get(limit) {
                if Number::parse(v).is_some_and(|n| n.sign() == Cmp::Equal) {
                    continue;
                }
                match finite_window(sub, limit, used, "unused_remaining_field") {
                    Some(V::Exhausted) => {
                        if value
                            .get("remaining")
                            .is_some_and(|v| remaining(v) != Some(V::Exhausted))
                        {
                            return result(V::Unknown, B::Incomplete);
                        }
                        return result(V::Exhausted, B::Subscription);
                    }
                    Some(V::Available) => available = true,
                    _ => incomplete = true,
                }
            }
        }
        if incomplete {
            return result(V::Unknown, B::Incomplete);
        }
        if let Some(n) = value.get("remaining").and_then(Number::parse) {
            if n.sign() == Cmp::Equal {
                return result(
                    if available { V::Unknown } else { V::Exhausted },
                    B::Subscription,
                );
            }
            if n.sign() == Cmp::Greater {
                return result(V::Available, B::Subscription);
            }
        }
        return result(
            if available { V::Available } else { V::Unknown },
            B::Subscription,
        );
    }
    if matches!(mode, "balance" | "wallet" | "unrestricted") && value.get("balance").is_some() {
        let verdict = remaining(&value["balance"]).unwrap_or(V::Unknown);
        if value
            .get("remaining")
            .is_some_and(|v| remaining(v) != Some(verdict))
        {
            return result(V::Unknown, B::Incomplete);
        }
        return result(verdict, B::Wallet);
    }
    result(V::Unknown, B::Incomplete)
}
fn new_api_evidence(value: &Value) -> QuotaCheck {
    let data = &value["data"];
    let (verdict, basis) = match data["unlimited_quota"].as_bool() {
        Some(true) => (QuotaVerdict::Available, QuotaBasis::Unlimited),
        Some(false) => (
            remaining(&data["total_available"]).unwrap_or(QuotaVerdict::Unknown),
            QuotaBasis::KeyQuota,
        ),
        None => (QuotaVerdict::Unknown, QuotaBasis::Incomplete),
    };
    evidence(verdict, QuotaSource::NewApi, basis)
}
