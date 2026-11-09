//! Observed account quota boundaries. Never extrapolate a new window.
use super::{time, AntigravityQuotaTrack, AntigravityTrackWindow, Api, WindowAcc};
use crate::{
    antigravity::auth::{self, AccountKind},
    storage::Result,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::BTreeMap, path::Path};

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Period {
    pub start: i64,
    pub end: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remaining_percent: Option<f64>,
}
impl Period {
    fn current(&self, now: i64) -> bool {
        self.start <= now && now < self.end
    }
    fn contains(&self, at: i64) -> bool {
        self.start <= at && at < self.end
    }
}
pub type Periods = BTreeMap<String, Period>;
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Account {
    pub id: String,
    pub name: String,
    pub kind: AccountKind,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AccountUsage {
    pub id: String,
    pub name: String,
    pub kind: AccountKind,
    pub tracks: Vec<AntigravityQuotaTrack>,
    pub total: AntigravityTrackWindow,
}
fn parse_remaining_percent(bucket: &Value) -> Option<f64> {
    if let Some(fraction) = bucket["remainingFraction"]
        .as_f64()
        .or_else(|| bucket["remaining_fraction"].as_f64())
        .filter(|v| v.is_finite())
    {
        return Some(((fraction.clamp(0.0, 1.0) * 100.0) * 10.0).round() / 10.0);
    }
    bucket["remainingPercent"]
        .as_f64()
        .or_else(|| bucket["remaining_percent"].as_f64())
        .filter(|v| v.is_finite())
        .map(|pct| (pct.clamp(0.0, 100.0) * 10.0).round() / 10.0)
}
pub fn parse(value: &Value) -> Periods {
    let response = &value["response"];
    let mut periods = Periods::new();
    for b in response["groups"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|g| g["buckets"].as_array().into_iter().flatten())
    {
        if b["disabled"] == true {
            continue;
        }
        let Some(id) = b["bucketId"].as_str() else {
            continue;
        };
        let duration = match id {
            "gemini-5h" | "3p-5h" => 5 * 3_600_000,
            "gemini-weekly" | "3p-weekly" => 7 * 86_400_000,
            _ => continue,
        };
        let Some(end) = time(&b["resetTime"]) else {
            continue;
        };
        let remaining_percent = parse_remaining_percent(b);
        let next = Period {
            start: end.saturating_sub(duration),
            end,
            remaining_percent,
        };
        match periods.get_mut(id) {
            Some(existing) => {
                existing.start = next.start;
                existing.end = next.end;
                existing.remaining_percent = match (existing.remaining_percent, remaining_percent) {
                    (Some(a), Some(b)) => Some(a.min(b)),
                    (Some(a), None) => Some(a),
                    (None, b) => b,
                };
            }
            None => {
                periods.insert(id.to_owned(), next);
            }
        }
    }
    periods
}
impl Api {
    pub(super) async fn quota(
        &self,
        data: &Path,
        home: &Path,
    ) -> Result<Option<(String, Periods)>> {
        let Some((id, email)) = auth::current_oauth_identity(data, home)? else {
            return Ok(None);
        };
        Ok(self.quota_for(&email).await?.map(|periods| (id, periods)))
    }
    async fn quota_for(&self, email: &str) -> Result<Option<Periods>> {
        for endpoint in &self.endpoints {
            let user = self.call_on(endpoint, "GetUserStatus", &json!({})).await?;
            if !user["userStatus"]["email"]
                .as_str()
                .is_some_and(|e| e.eq_ignore_ascii_case(email))
            {
                continue;
            }
            let value = self
                .call_on(endpoint, "RetrieveUserQuotaSummary", &json!({}))
                .await?;
            return Ok(Some(parse(&value)));
        }
        Ok(None)
    }
}
pub async fn observe(data: &Path) -> Result<Option<(String, Periods)>> {
    let Some((id, email)) = auth::current_oauth_identity(data, &crate::antigravity::home()?)?
    else {
        return Ok(None);
    };
    match Api::connect().await? {
        Some(api) => Ok(api.quota_for(&email).await?.map(|periods| (id, periods))),
        None => Ok(None),
    }
}
pub(in crate::usage) struct AccountAccumulator {
    account: Account,
    periods: Periods,
    windows: BTreeMap<&'static str, WindowAcc>,
    total: WindowAcc,
}
impl AccountAccumulator {
    pub(in crate::usage) fn new(account: Account, mut periods: Periods, now: i64) -> Self {
        periods.retain(|_, p| p.current(now));
        Self {
            account,
            periods,
            windows: BTreeMap::new(),
            total: WindowAcc::default(),
        }
    }
    pub(in crate::usage) fn earliest(&self) -> Option<i64> {
        self.periods.values().map(|p| p.start).min()
    }
    pub(super) fn ingest(
        &mut self,
        at: i64,
        track: Option<&str>,
        amounts: &WindowAcc,
        historical: bool,
    ) {
        let add = |w: &mut WindowAcc| {
            w.requests += amounts.requests;
            w.unpriced_requests += amounts.unpriced_requests;
            w.tokens = w.tokens.saturating_add(amounts.tokens);
            w.reasoning_tokens = w.reasoning_tokens.saturating_add(amounts.reasoning_tokens);
            w.cost += amounts.cost;
        };
        if self.account.kind != AccountKind::Oauth {
            if historical {
                add(&mut self.total);
            }
            return;
        }
        if self.periods.is_empty() {
            return;
        }
        let keys = match track {
            Some("gemini") => ["gemini-5h", "gemini-weekly"],
            Some("claude_gpt") => ["3p-5h", "3p-weekly"],
            _ => return,
        };
        for key in keys {
            if self.periods.get(key).is_some_and(|p| p.contains(at)) {
                add(self.windows.entry(key).or_default());
            }
        }
    }
    pub(super) fn finish(mut self) -> AccountUsage {
        let mut tracks = Vec::new();
        if self.account.kind == AccountKind::Oauth {
            for (id, label, burst, weekly) in [
                ("gemini", "Gemini", "gemini-5h", "gemini-weekly"),
                ("claude_gpt", "Claude / GPT", "3p-5h", "3p-weekly"),
            ] {
                let mut window = |key: &str, hours: f64| {
                    let mut w = self
                        .windows
                        .remove(key)
                        .unwrap_or_default()
                        .into_view(hours);
                    let period = self.periods.remove(key);
                    w.remaining_percent = period.as_ref().and_then(|p| p.remaining_percent);
                    w.period = period;
                    w
                };
                tracks.push(AntigravityQuotaTrack {
                    id: id.into(),
                    label: label.into(),
                    burst_5h: window(burst, 5.0),
                    weekly_7d: window(weekly, 168.0),
                });
            }
        }
        AccountUsage {
            id: self.account.id,
            name: self.account.name,
            kind: self.account.kind,
            tracks,
            total: self.total.into_view(0.0),
        }
    }
}

#[cfg(test)]
#[path = "antigravity_quota_tests.rs"]
mod tests;
