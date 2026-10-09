//! One-to-one cross-source matching, based on CC Switch 5ae6ad38's ten-minute
//! fingerprint window. Ambiguous sessions remain inspectable and never double bill.
use super::{model::*, pricing::Quote, query};
use crate::storage::Result;
use rusqlite::{params, Connection};
const WINDOW: i64 = 600_000;
fn db<T>(v: rusqlite::Result<T>) -> Result<T> {
    v.map_err(|_| failure("用量关联失败"))
}
fn end(r: &Record) -> i64 {
    r.final_attempt()
        .map(|a| a.started_at.saturating_add(a.duration_ms as i64))
        .unwrap_or(r.started_at)
}
fn response(r: &Record) -> Option<&str> {
    r.final_attempt().and_then(|a| a.response_id.as_deref())
}
fn provider(r: &Record) -> Option<&str> {
    r.final_attempt().and_then(|a| a.provider.as_deref())
}
fn compatible(a: &Record, b: &Record) -> bool {
    a.client == b.client
        && a.final_attempt().map(|a| a.operation) == b.final_attempt().map(|a| a.operation)
        && (provider(a).is_none() || provider(b).is_none() || provider(a) == provider(b))
}
fn exact(a: &Record, b: &Record) -> bool {
    response(a).is_some() && response(a) == response(b) && compatible(a, b)
}
fn fingerprint(a: &Record, b: &Record) -> bool {
    a.source != b.source
        && (a.source == "proxy" || b.source == "proxy")
        && compatible(a, b)
        && (response(a).is_none() || response(b).is_none())
        && a.signature().is_some()
        && a.signature() == b.signature()
        && end(a).abs_diff(end(b)) <= WINDOW as u64
        && [a, b].iter().filter(|r| r.source == "proxy").all(|r| {
            r.final_attempt().is_some_and(|a| {
                a.status
                    .is_some_and(|s| (200..300).contains(&s) || s == 101)
            })
        })
}
fn rows(c: &Connection, seed: &Record) -> Result<Vec<Record>> {
    let mut stmt=db(c.prepare("SELECT body FROM records WHERE id=?6 UNION SELECT body FROM records WHERE ?2 IS NOT NULL AND client=?1 AND response=?2 UNION SELECT body FROM records WHERE ?3 IS NOT NULL AND client=?1 AND signature=?3 AND source<>?7 AND (source='proxy' OR ?7='proxy') AND time BETWEEN ?4 AND ?5"))?;
    let rows = db(stmt.query_map(
        params![
            seed.client,
            response(seed),
            seed.signature(),
            end(seed).saturating_sub(WINDOW * 2 + 86_400_000),
            end(seed) + WINDOW * 2,
            seed.id,
            seed.source
        ],
        |r| r.get::<_, String>(0),
    ))?;
    rows.map(|v| serde_json::from_str(&db(v)?).map_err(|_| failure("用量关联记录无效")))
        .collect()
}
fn state(
    c: &Connection,
    r: &Record,
    effective: i64,
    target: Option<&str>,
    rule: &str,
    sources: Vec<String>,
) -> Result<()> {
    let mut r = r.clone();
    r.duplicate_of = target.map(str::to_string);
    r.deduplication = rule.into();
    r.merged_sources = sources;
    db(c.execute(
        "UPDATE records SET effective=?2,duplicate_of=?3,body=?4 WHERE id=?1",
        params![
            r.id,
            effective,
            target,
            serde_json::to_string(&r).map_err(|_| failure("用量关联无效"))?
        ],
    ))?;
    Ok(())
}
pub fn reconcile(c: &Connection, seed: &Record) -> Result<()> {
    let mut all = rows(c, seed)?;
    if seed.source != "proxy" {
        let mut seen: std::collections::BTreeSet<String> =
            all.iter().map(|r| r.id.clone()).collect();
        let mut connected = Vec::new();
        for p in all.iter().filter(|r| r.source == "proxy") {
            for row in rows(c, p)? {
                if seen.insert(row.id.clone()) {
                    connected.push(row);
                }
            }
        }
        all.extend(connected);
    }
    for r in &all {
        let mut archived_stmt=db(c.prepare("SELECT id,response FROM (SELECT id,response FROM receipts WHERE effective=1 AND operation=?8 AND client=?1 AND ?2 IS NOT NULL AND response=?2 AND (provider IS NULL OR ?3 IS NULL OR provider=?3) UNION SELECT id,response FROM receipts WHERE effective=1 AND operation=?8 AND client=?1 AND ?5 IS NOT NULL AND signature=?5 AND ended BETWEEN ?6 AND ?7 AND (provider IS NULL OR ?3 IS NULL OR provider=?3) AND source<>?4 AND (source='proxy' OR ?4='proxy') AND (response IS NULL OR ?2 IS NULL)) ORDER BY CASE WHEN response=?2 THEN 0 ELSE 1 END,id LIMIT 3"))?;
        let archived = db(archived_stmt.query_map(
            params![
                r.client,
                response(r),
                provider(r),
                r.source,
                r.signature(),
                end(r) - WINDOW,
                end(r) + WINDOW,
                r.final_attempt()
                    .map(|a| a.operation)
                    .unwrap_or_default()
                    .as_str()
            ],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)),
        ))?;
        let archived = db(archived.collect::<rusqlite::Result<Vec<_>>>())?;
        if let Some((id, _)) = archived
            .iter()
            .find(|(_, id)| id.as_deref().is_some() && id.as_deref() == response(r))
        {
            state(c, r, 0, Some(id), "archived_match", vec![])?;
            continue;
        }
        if !archived.is_empty() {
            // A compacted fallback candidate cannot be proven one-to-one after
            // the peer window was discarded. Keep it reviewable, never bill twice.
            state(c, r, 2, None, "ambiguous", vec![])?;
            continue;
        }
        // The seed's candidate window may omit a peer's already-linked proxy.
        // Resolve each row against its own IDs before changing its effective state.
        let peers = rows(c, r)?;
        let mut same: Vec<_> = peers.iter().filter(|b| exact(r, b)).collect();
        same.sort_by(|a, b| (a.source != "proxy", &a.id).cmp(&(b.source != "proxy", &b.id)));
        if let Some(canonical) = same.first().filter(|_| same.len() > 1) {
            let mut sources: Vec<_> = same.iter().map(|r| r.source.clone()).collect();
            sources.sort();
            sources.dedup();
            state(
                c,
                r,
                if canonical.id == r.id { 1 } else { 0 },
                if canonical.id == r.id {
                    None
                } else {
                    Some(&canonical.id)
                },
                "response_id",
                sources,
            )?;
            continue;
        }
        if r.source == "proxy" {
            state(c, r, 1, None, "", vec![])?;
            continue;
        }
        let candidates: Vec<_> = peers
            .iter()
            .filter(|b| b.id != r.id && fingerprint(r, b))
            .collect();
        if candidates.len() == 1 {
            let p = candidates[0];
            // Re-evaluate against the complete candidate window, not just live
            // effective rows: a later import can invalidate an earlier pairing.
            let peers = rows(c, p)?;
            let reverse = peers
                .iter()
                .filter(|s| s.source != "proxy" && (fingerprint(p, s) || exact(p, s)))
                .count();
            if reverse == 1 {
                state(
                    c,
                    r,
                    0,
                    Some(&p.id),
                    "strict_match",
                    vec![r.source.clone(), "proxy".into()],
                )?;
                continue;
            }
        }
        state(
            c,
            r,
            if candidates.is_empty() { 1 } else { 2 },
            None,
            if candidates.is_empty() {
                ""
            } else {
                "ambiguous"
            },
            vec![],
        )?;
    }
    // Preserve the gateway's actual status/timing. A reliable response ID may
    // supply usage the gateway did not receive, but never replace a real zero.
    for p in all.iter().filter(|r| r.source == "proxy") {
        supplement(c, &p.id)?;
    }
    Ok(())
}

fn save_projection(c: &Connection, r: &Record) -> Result<()> {
    let body = serde_json::to_string(r).map_err(|_| failure("用量关联无效"))?;
    db(c.execute(
        "UPDATE records SET body=?2,signature=?3,model=?4 WHERE id=?1",
        params![
            r.id,
            body,
            r.signature(),
            r.final_attempt().and_then(|a| a.grouping_model())
        ],
    ))?;
    query::project(c, 0, &r.id, r, 1, None)
}
fn supplement(c: &Connection, id: &str) -> Result<()> {
    let body: String = db(c.query_row("SELECT body FROM records WHERE id=?1", [id], |r| r.get(0)))?;
    let mut current: Record = serde_json::from_str(&body).map_err(|_| failure("用量关联无效"))?;
    let previous_price = current.final_attempt().and_then(|a| a.price.clone());
    if let Some(original) = current.gateway_reported.take() {
        if let Some(last) = current.attempts.last_mut() {
            *last = original;
        }
    }
    let mut stmt = db(c.prepare(
        "SELECT body FROM records WHERE duplicate_of=?1 AND source<>'proxy' ORDER BY id",
    ))?;
    let peers = db(stmt.query_map([id], |r| r.get::<_, String>(0)))?;
    let peers = peers
        .map(|r| serde_json::from_str::<Record>(&db(r)?).map_err(|_| failure("用量关联无效")))
        .collect::<Result<Vec<_>>>()?;
    current.merged_sources = peers.iter().map(|r| r.source.clone()).collect();
    if !peers.is_empty() {
        current.merged_sources.push("proxy".into());
        current.merged_sources.sort();
        current.merged_sources.dedup();
        if current.deduplication.is_empty() {
            current.deduplication = "strict_match".into();
        }
    }
    if current
        .final_attempt()
        .is_some_and(|a| a.tokens.total().is_none())
    {
        let donor = peers
            .iter()
            .filter(|p| exact(&current, p))
            .filter_map(|p| p.final_attempt().map(|a| (p.completed, a)))
            .filter(|(_, a)| a.tokens.total().is_some())
            .max_by_key(|(complete, a)| (*complete, a.tokens.output, a.tokens.total()));
        if let Some((_, donor)) = donor {
            let original = current
                .final_attempt()
                .cloned()
                .expect("checked final attempt");
            let last = current.attempts.last_mut().expect("checked final attempt");
            last.tokens = donor.tokens.clone();
            if last.response_model.is_none() {
                last.response_model = donor.response_model.clone();
            }
            if last.pricing_model.is_none() {
                last.pricing_model = donor.pricing_model.clone();
            }
            // Existing billed snapshots are immutable. A previously unpriced
            // record uses a matching known rate snapshot with its own multiplier.
            if last.price.is_none() && last.operation == super::model::Operation::Model {
                last.price = previous_price
                    .as_ref()
                    .or(donor.price.as_ref())
                    .filter(|p| Some(p.model.as_str()) == last.pricing_model.as_deref())
                    .and_then(|p| {
                        Quote {
                            model: p.model.clone(),
                            data: p.basis.clone(),
                            version: p.version.clone(),
                            source: p.source.clone(),
                            multiplier: last.cost_multiplier.clone(),
                        }
                        .calculate(&last.tokens, last.service_tier.as_deref())
                    });
            }
            current.gateway_reported = Some(original);
        }
    }
    if serde_json::to_string(&current).map_err(|_| failure("用量关联无效"))? != body {
        save_projection(c, &current)?;
    }
    Ok(())
}

/// Source rebuilding must not leave session-derived consumption on gateway rows
/// if the source is removed or no longer matches after re-parsing.
pub fn reset_source_supplements(c: &Connection, source: &str) -> Result<()> {
    let mut stmt=db(c.prepare("SELECT body FROM records WHERE source='proxy' AND client=?1 AND (json_type(body,'$.gatewayReported')='object' OR json_array_length(json_extract(body,'$.mergedSources'))>0)"))?;
    let rows = db(stmt.query_map([source], |r| r.get::<_, String>(0)))?;
    let rows = db(rows.collect::<rusqlite::Result<Vec<_>>>())?;
    for body in rows {
        let mut r: Record = serde_json::from_str(&body).map_err(|_| failure("用量关联无效"))?;
        if let Some(original) = r.gateway_reported.take() {
            if let Some(last) = r.attempts.last_mut() {
                *last = original;
            }
        }
        r.merged_sources.clear();
        r.deduplication.clear();
        save_projection(c, &r)?;
    }
    Ok(())
}
