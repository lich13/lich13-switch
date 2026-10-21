use super::{
    bodies,
    model::*,
    query,
    store::{db, Store},
};
use crate::storage::Result;
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Group {
    pub client: String,
    pub provider: Option<String>,
    pub reported: u64,
    pub partial: u64,
    pub unknown: u64,
}
#[derive(Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Summary {
    pub start: i64,
    pub end: i64,
    pub detail_since: i64,
    pub history_missing: bool,
    pub requests: u64,
    pub reported: u64,
    pub partial: u64,
    pub unknown: u64,
    pub groups: Vec<Group>,
}
impl Store {
    pub fn interruptions(&self, filter: &Filter) -> Result<Summary> {
        let mut filter = filter.clone();
        // Explicitly inspect gateway originals even when the overview includes
        // native sessions or exact-response session supplements.
        filter.source = Some("proxy".into());
        filter.end = Some(filter.end.unwrap_or_else(now));
        let detail_since = self.detail_since()?;
        let mut summary = Summary {
            start: filter.start.unwrap_or(0),
            end: filter.end.unwrap(),
            detail_since,
            history_missing: filter.start.unwrap_or(0) < detail_since,
            ..Summary::default()
        };
        let (condition, args) = query::conditions(&filter, "r", true);
        let eligible = query::eligible(&self.connection, &filter, "r", true);
        let condition = query::native_sql(&self.connection, format!("{eligible} AND {condition}"));
        let mut groups: BTreeMap<(String, Option<String>), Group> = BTreeMap::new();
        // A single SQL cursor stays on one read snapshot. Decode bounded batches
        // and count a logical request only once, independently of retry attempts.
        let mut stmt = db(self.connection.prepare(&format!(
            "SELECT body FROM records r WHERE {condition} ORDER BY time,local_sequence,id"
        )))?;
        let mut rows = db(stmt.query(rusqlite::params_from_iter(args.iter())))?;
        loop {
            let mut batch = Vec::with_capacity(128);
            while batch.len() < 128 {
                match db(rows.next())? {
                    Some(row) => batch.push(db(row.get::<_, String>(0))?),
                    None => break,
                }
            }
            if batch.is_empty() {
                break;
            }
            for body in batch {
                let mut record: Record = bodies::decode(&self.connection, &body)?;
                record.gateway_only();
                summary.requests += 1;
                let last = record.final_attempt();
                let interrupted = record
                    .attempts
                    .iter()
                    .rev()
                    .find(|a| a.interruption().is_some());
                let unknown = last.is_none_or(|a| a.end.is_none());
                if interrupted.is_none() && !unknown {
                    continue;
                }
                let attempt = interrupted.or(last);
                let provider = attempt.and_then(|a| a.provider.clone());
                let key = (record.client.clone(), provider.clone());
                let group = groups.entry(key).or_insert_with(|| Group {
                    client: record.client.clone(),
                    provider,
                    ..Group::default()
                });
                match attempt.and_then(Attempt::interruption) {
                    Some("reported") => {
                        summary.reported += 1;
                        group.reported += 1;
                    }
                    Some(_) => {
                        summary.partial += 1;
                        group.partial += 1;
                    }
                    None => {
                        summary.unknown += 1;
                        group.unknown += 1;
                    }
                }
            }
        }
        summary.groups = groups.into_values().collect();
        Ok(summary)
    }
}
