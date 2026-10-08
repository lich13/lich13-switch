//! Codex high-water/replay and Claude message-ID merging, adapted from CC Switch.
//! Source files are read-only. Only counters, hashed IDs and cursors survive parsing.
use super::{model::*, pricing::Pricing, store::Store};
use crate::storage::{self, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs::File,
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};
const MAX_LINE: u64 = 2 * 1024 * 1024;
#[derive(Default, Clone, Serialize, Deserialize)]
#[serde(default)]
struct Cursor {
    offset: u64,
    identity: String,
    tail: String,
    session: Option<String>,
    parent: Option<String>,
    fork_at: Option<i64>,
    model: Option<String>,
    total: Tokens,
    signature: Option<String>,
    lanes: BTreeMap<String, String>,
    replay: bool,
    boundary: Option<i64>,
    last_output: Option<i64>,
    usage_end: Option<i64>,
    last_usage_at: Option<i64>,
    chain: BTreeMap<String, (Option<String>, i64)>,
    messages: BTreeMap<String, (i64, Option<u64>, bool)>,
    message_usage: BTreeMap<String, String>,
    replay_index: usize,
    meta_loaded: bool,
}
#[derive(Default, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    pub files: u64,
    pub total_files: u64,
    pub merged: u64,
    pub pending: u64,
    pub historical_before: i64,
    pub phase: String,
    pub imported: u64,
    pub skipped: u64,
    pub errors: u64,
    pub completed_at: Option<i64>,
}
fn timestamp(v: &Value) -> Option<i64> {
    v.as_str()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.timestamp_millis())
        .or_else(|| v.as_i64())
}
fn identity(file: &File) -> Result<String> {
    let m = file.metadata().map_err(storage::io_error)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(safe_id(&format!("{}:{}", m.dev(), m.ino())))
    }
    #[cfg(not(unix))]
    {
        Ok(safe_id(&format!("{:?}", m.created().ok())))
    }
}
fn tail(file: &mut File, offset: u64) -> Result<String> {
    let start = offset.saturating_sub(4096);
    file.seek(SeekFrom::Start(start))
        .map_err(storage::io_error)?;
    let mut bytes = vec![0; (offset - start) as usize];
    file.read_exact(&mut bytes).map_err(storage::io_error)?;
    Ok(storage::digest(&bytes))
}
fn files(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    if depth > 8 || out.len() > 30000 {
        return Err(failure("会话文件数量超过单次同步限制"));
    }
    if std::fs::symlink_metadata(dir)
        .map_err(storage::io_error)?
        .file_type()
        .is_symlink()
    {
        return Ok(());
    }
    for e in std::fs::read_dir(dir).map_err(storage::io_error)? {
        let e = e.map_err(storage::io_error)?;
        let kind = e.file_type().map_err(storage::io_error)?;
        if kind.is_symlink() {
            continue;
        }
        if kind.is_dir() {
            files(&e.path(), depth + 1, out)?;
        } else if kind.is_file() && e.path().extension().is_some_and(|s| s == "jsonl") {
            out.push(e.path());
        }
    }
    Ok(())
}
fn line(reader: &mut BufReader<File>) -> Result<Option<Vec<u8>>> {
    let mut bytes = Vec::new();
    let n = (&mut *reader)
        .take(MAX_LINE + 1)
        .read_until(b'\n', &mut bytes)
        .map_err(storage::io_error)?;
    if n == 0 {
        return Ok(None);
    }
    if bytes.len() as u64 > MAX_LINE {
        // Large media/tool lines carry no required counters. Discard them without
        // retaining their contents, then continue reading subsequent metadata.
        if bytes.last() != Some(&b'\n') {
            loop {
                let buffer = reader.fill_buf().map_err(storage::io_error)?;
                if buffer.is_empty() {
                    return Ok(None);
                }
                let end = buffer.iter().position(|b| *b == b'\n');
                let length = end.map(|n| n + 1).unwrap_or(buffer.len());
                reader.consume(length);
                if end.is_some() {
                    break;
                }
            }
        }
        return Ok(Some(Vec::new()));
    }
    if bytes.last() != Some(&b'\n') && serde_json::from_slice::<Value>(&bytes).is_err() {
        return Ok(None);
    }
    Ok(Some(bytes))
}
fn meta(path: &Path) -> Result<(Option<String>, Option<String>, Option<i64>)> {
    let mut reader = BufReader::new(File::open(path).map_err(storage::io_error)?);
    for _ in 0..16 {
        let Some(bytes) = line(&mut reader)? else {
            break;
        };
        let Ok(v) = serde_json::from_slice::<Value>(&bytes) else {
            continue;
        };
        if v["type"] == "session_meta" {
            let p = &v["payload"];
            return Ok((
                p["id"].as_str().map(safe_id),
                p["forked_from_id"]
                    .as_str()
                    .or_else(|| {
                        p["source"]["subagent"]["thread_spawn"]["parent_thread_id"].as_str()
                    })
                    .map(safe_id),
                timestamp(&v["timestamp"]).or_else(|| timestamp(&p["timestamp"])),
            ));
        }
    }
    Ok((None, None, None))
}
fn signature(info: &Value) -> String {
    safe_id(&format!(
        "{}:{}",
        counter_value(&info["total_token_usage"]),
        counter_value(&info["last_token_usage"])
    ))
}
fn counter_value(v: &Value) -> Value {
    serde_json::json!({"input":v["input_tokens"].as_u64(),"cache":v["cached_input_tokens"].as_u64().or(v["cache_read_input_tokens"].as_u64()),"output":v["output_tokens"].as_u64()})
}
fn raw(v: &Value) -> Tokens {
    Tokens {
        input: v["input_tokens"].as_u64(),
        output: v["output_tokens"].as_u64(),
        cache_read: v["cached_input_tokens"]
            .as_u64()
            .or(v["cache_read_input_tokens"].as_u64()),
        ..Tokens::default()
    }
}
fn has_counters(v: &Value) -> bool {
    v.is_object()
        && [
            "input_tokens",
            "cached_input_tokens",
            "cache_read_input_tokens",
            "output_tokens",
        ]
        .iter()
        .any(|key| v[*key].as_u64().is_some())
}
fn parent_prefix(path: &Path, cutoff: i64) -> Result<Vec<String>> {
    let mut reader = BufReader::new(File::open(path).map_err(storage::io_error)?);
    let mut signatures = Vec::new();
    while let Some(bytes) = line(&mut reader)? {
        let Ok(v) = serde_json::from_slice::<Value>(&bytes) else {
            continue;
        };
        if v["type"] == "event_msg"
            && v["payload"]["type"] == "token_count"
            && timestamp(&v["timestamp"]).is_some_and(|t| t <= cutoff)
        {
            let next = signature(&v["payload"]["info"]);
            if signatures.last() != Some(&next) {
                signatures.push(next);
            }
        }
    }
    Ok(signatures)
}
pub trait Repository {
    fn cursor(&self, id: &str) -> Result<Option<String>>;
    fn write_batch(&mut self, rows: &[Record], cursor: Option<(&str, &str)>) -> Result<()>;
    fn rebuild(
        &mut self,
        source: &str,
        rows: &[Record],
        cursors: &[(String, String)],
    ) -> Result<()>;
    fn progress(&mut self, _report: &Report) {}
}
impl Repository for Store {
    fn cursor(&self, id: &str) -> Result<Option<String>> {
        Store::cursor(self, id)
    }
    fn write_batch(&mut self, rows: &[Record], cursor: Option<(&str, &str)>) -> Result<()> {
        Store::write_batch(self, rows, cursor)
    }
    fn rebuild(
        &mut self,
        source: &str,
        rows: &[Record],
        cursors: &[(String, String)],
    ) -> Result<()> {
        Store::rebuild(self, source, rows, cursors)
    }
}
pub fn sync(
    store: &mut impl Repository,
    pricing: &Pricing,
    settings: &Settings,
    client: &str,
    home: &Path,
    rebuild: bool,
) -> Result<Report> {
    let mut paths = Vec::new();
    if client == "codex" {
        files(&home.join("sessions"), 0, &mut paths)?;
        files(&home.join("archived_sessions"), 0, &mut paths)?;
    } else {
        files(&home.join("projects"), 0, &mut paths)?;
    }
    paths.sort();
    let mut parents = BTreeMap::new();
    let mut metadata = BTreeMap::new();
    let mut report = Report {
        total_files: paths.len() as u64,
        phase: "scanning".into(),
        ..Report::default()
    };
    store.progress(&report);
    if client == "codex" {
        for p in &paths {
            let key = safe_id(&format!("{client}:{}", p.to_string_lossy()));
            let saved = store
                .cursor(&key)?
                .and_then(|s| serde_json::from_str::<Cursor>(&s).ok());
            let mut file = File::open(p).map_err(storage::io_error)?;
            let id = identity(&file)?;
            let size = file.metadata().map_err(storage::io_error)?.len();
            let stable = saved
                .as_ref()
                .filter(|c| c.meta_loaded && c.identity == id && size >= c.offset)
                .map(|c| tail(&mut file, c.offset).map(|v| v == c.tail))
                .transpose()?
                .unwrap_or(false);
            let value = if let Some(c) = saved.filter(|_| stable) {
                (c.session, c.parent, c.fork_at)
            } else {
                meta(p)?
            };
            if let Some(id) = &value.0 {
                parents.insert(id.clone(), p.clone());
            }
            metadata.insert(key, value);
        }
    }
    let mut prefixes = BTreeMap::new();
    let mut rebuilt = Vec::new();
    let mut cursors = Vec::new();
    for path in paths {
        let key = safe_id(&format!("{client}:{}", path.to_string_lossy()));
        let result = (|| -> Result<(Vec<Record>, String)> {
            let mut file = File::open(&path).map_err(storage::io_error)?;
            let id = identity(&file)?;
            let mut c = if rebuild {
                Cursor::default()
            } else {
                store
                    .cursor(&key)?
                    .and_then(|s| serde_json::from_str::<Cursor>(&s).ok())
                    .unwrap_or_default()
            };
            if c.identity != id
                || file.metadata().map_err(storage::io_error)?.len() < c.offset
                || tail(&mut file, c.offset)? != c.tail
            {
                c = Cursor::default();
            }
            c.identity = id;
            let mut prefix = Vec::new();
            if client == "codex" && c.offset == 0 {
                let (session, parent, at) = metadata.get(&key).cloned().unwrap_or_default();
                c.meta_loaded = true;
                c.session = session;
                c.parent = parent;
                c.fork_at = at;
                c.replay = c.parent.is_some();
            }
            if c.replay {
                if let (Some(parent), Some(at)) = (&c.parent, c.fork_at) {
                    let parent_key = (parent.clone(), at);
                    if !prefixes.contains_key(&parent_key) {
                        let value = parent_prefix(
                            parents
                                .get(parent)
                                .ok_or_else(|| failure("分叉父会话暂不可用"))?,
                            at,
                        )?;
                        prefixes.insert(parent_key.clone(), value);
                    }
                    prefix = prefixes[&parent_key].clone();
                } else {
                    return Err(failure("分叉会话缺少可靠时间"));
                }
            }
            file.seek(SeekFrom::Start(c.offset))
                .map_err(storage::io_error)?;
            let mut reader = BufReader::new(file);
            let mut rows = BTreeMap::<String, Record>::new();
            let batch_start = c.offset;
            while rebuild || c.offset - batch_start < 64 * 1024 * 1024 {
                let Some(bytes) = line(&mut reader)? else {
                    break;
                };
                let complete_tail = bytes.last() != Some(&b'\n');
                if !complete_tail {
                    c.offset = reader.stream_position().map_err(storage::io_error)?;
                }
                let Ok(v) = serde_json::from_slice::<Value>(&bytes) else {
                    report.skipped += 1;
                    continue;
                };
                let Some(at) = timestamp(&v["timestamp"]) else {
                    continue;
                };
                let record = if client == "codex" {
                    codex(&v, &mut c, at, &prefix)
                } else {
                    claude(&v, &mut c, at)
                };
                if let Some(mut r) = record {
                    if let Some(a) = r.attempts.last_mut() {
                        a.cost_multiplier = settings.multiplier.clone();
                        a.pricing_model = a.response_model.clone().or(a.requested_model.clone());
                        a.price = pricing
                            .quote(a.pricing_model.as_deref(), &settings.multiplier)
                            .calculate(&a.tokens, a.service_tier.as_deref());
                    }
                    if rows.len() >= 100_000 {
                        return Err(failure("单文件会话用量超过安全处理限制"));
                    }
                    rows.entry(r.id.clone())
                        .and_modify(|old| {
                            let a = old.final_attempt().unwrap();
                            let b = r.final_attempt().unwrap();
                            if r.completed && !old.completed
                                || r.completed == old.completed
                                    && b.tokens.output >= a.tokens.output
                            {
                                *old = r.clone();
                            }
                        })
                        .or_insert(r);
                }
                // A complete final JSON can be imported before the writer adds
                // its newline. Hold the byte cursor; stable IDs make replay safe.
                if complete_tail {
                    break;
                }
            }
            c.tail = tail(reader.get_mut(), c.offset)?;
            Ok((
                rows.into_values().collect(),
                serde_json::to_string(&c).map_err(|_| failure("同步游标保存失败"))?,
            ))
        })();
        match result {
            Ok((rows, cursor)) => {
                report.files += 1;
                report.imported += rows.len() as u64;
                if rebuild {
                    if rebuilt.len() + rows.len() > 100_000 {
                        return Err(failure("会话重建超过单次安全限制，已保留原数据"));
                    }
                    rebuilt.extend(rows);
                    cursors.push((key, cursor));
                } else {
                    store.write_batch(&rows, Some((&key, &cursor)))?;
                }
            }
            Err(_) => {
                report.errors += 1;
                if rebuild {
                    return Err(failure("会话重建未完成，已保留原数据"));
                }
            }
        }
        if report.files % 16 == 0 {
            store.progress(&report);
        }
    }
    if rebuild {
        report.phase = "rebuilding".into();
        store.progress(&report);
        store.rebuild(client, &rebuilt, &cursors)?;
    }
    report.phase = "complete".into();
    report.completed_at = Some(now());
    Ok(report)
}
struct SessionTiming {
    at: i64,
    start: Option<i64>,
    complete: bool,
}
fn base(
    client: &str,
    id: String,
    session: Option<String>,
    model: Option<String>,
    tokens: Tokens,
    timing: SessionTiming,
) -> Record {
    let SessionTiming {
        at,
        start,
        complete,
    } = timing;
    let duration = start
        .filter(|s| *s <= at && at - *s < 86_400_000)
        .map(|s| (at - s) as u64)
        .unwrap_or(0);
    Record {
        id: id.clone(),
        client: client.into(),
        source: client.into(),
        started_at: start.unwrap_or(at),
        session_id: session,
        completed: complete,
        estimated_speed: duration > 0,
        attempts: vec![Attempt {
            id,
            response_model: model.clone(),
            requested_model: model,
            tokens,
            started_at: start.unwrap_or(at),
            duration_ms: duration,
            outcome: if complete { "completed" } else { "reported" }.into(),
            transport: "session".into(),
            ..Attempt::default()
        }],
        ..Record::default()
    }
}
fn codex(v: &Value, c: &mut Cursor, at: i64, prefix: &[String]) -> Option<Record> {
    let p = &v["payload"];
    let typ = v["type"].as_str()?;
    if typ == "turn_context" {
        c.model = model_id(p["model"].as_str().or(p["info"]["model"].as_str())).or(c.model.take());
        c.boundary = Some(at);
        return None;
    }
    if typ == "response_item" {
        match p["role"].as_str() {
            Some("user") => c.boundary = Some(at),
            Some("assistant") => c.last_output = Some(at),
            _ => {}
        }
        return None;
    }
    if typ == "token_usage_record" {
        c.usage_end = Some(at);
        return None;
    }
    if typ != "event_msg" {
        return None;
    }
    if matches!(p["type"].as_str(), Some("task_started" | "turn_aborted")) {
        c.boundary = Some(at);
        c.last_output = None;
        c.usage_end = None;
        c.signature = None;
        c.lanes.clear();
        return None;
    }
    if p["type"] != "token_count" {
        return None;
    }
    let info = p.get("info")?.as_object()?;
    let total = info.get("total_token_usage").filter(|v| has_counters(v));
    let last = info.get("last_token_usage").filter(|v| has_counters(v));
    if total.is_none() && last.is_none() {
        return None;
    }
    c.model = model_id(
        info.get("model")
            .or(info.get("model_name"))
            .and_then(Value::as_str)
            .or(p["model"].as_str()),
    )
    .or(c.model.take());
    let sig = signature(&p["info"]);
    let lane = safe_id(
        p["rate_limits"]["limit_id"]
            .as_str()
            .filter(|s| !s.is_empty())
            .unwrap_or(""),
    );
    let duplicate =
        total.is_some() && (c.signature.as_ref() == Some(&sig) || c.lanes.get(&lane) == Some(&sig));
    c.signature = Some(sig.clone());
    if total.is_some() {
        c.lanes.insert(lane, sig.clone());
        if c.lanes.len() > 32 {
            c.lanes.clear();
        }
    }
    if duplicate {
        return None;
    }
    let next_total = total.map(raw);
    // Cumulative counters restart after compaction/session resets. A decrease
    // starts a fresh epoch instead of keeping a high-water mark forever.
    let reset = next_total.as_ref().is_some_and(|n| {
        [(n.input, c.total.input), (n.output, c.total.output)]
            .iter()
            .any(|(a, b)| matches!((a,b),(Some(a),Some(b)) if a < b))
    });
    let new_boundary = c
        .boundary
        .zip(c.last_usage_at)
        .is_some_and(|(boundary, used)| boundary > used);
    if reset && last.is_none() && !new_boundary {
        // A regressing snapshot without a new turn/compaction boundary is late
        // telemetry, not evidence that another request consumed the full total.
        return None;
    }
    c.last_usage_at = Some(at);
    let mut tokens = last.map(raw).unwrap_or_else(|| {
        let n = next_total.as_ref().unwrap();
        if reset {
            n.clone()
        } else {
            n.delta(&c.total)
        }
    });
    if let Some(t) = next_total {
        c.total = t;
    }
    if c.replay {
        if let Some(relative) = prefix
            .get(c.replay_index..)
            .and_then(|tail| tail.iter().position(|s| s == &sig))
        {
            c.replay_index += relative + 1;
            return None;
        }
        c.replay = false;
    }
    tokens.cache_read = tokens.cache_read.map(|n| n.min(tokens.input.unwrap_or(n)));
    tokens.input = tokens
        .input
        .map(|n| n.saturating_sub(tokens.cache_read.unwrap_or(0)));
    if tokens.total().unwrap_or(0) == 0 {
        return None;
    }
    let session = c.session.clone()?;
    let id = safe_id(&format!("codex:{session}:{at}:{sig}"));
    let end = c.usage_end.take().filter(|t| *t <= at).unwrap_or(at);
    let start = c.boundary.replace(at);
    c.last_output = None;
    let mut record = base(
        "codex",
        id,
        Some(session),
        c.model.clone(),
        tokens,
        SessionTiming {
            at: end,
            start,
            complete: true,
        },
    );
    record.attempts[0].response_id = p["response_id"]
        .as_str()
        .or(p["info"]["response_id"].as_str())
        .map(safe_id);
    Some(record)
}
fn claude(v: &Value, c: &mut Cursor, at: i64) -> Option<Record> {
    if c.session.is_none() {
        c.session = v["sessionId"].as_str().map(safe_id);
    }
    let parent = v["parentUuid"].as_str().map(safe_id);
    if let Some(id) = v["uuid"].as_str() {
        c.chain.insert(safe_id(id), (parent.clone(), at));
        while c.chain.len() > 128 {
            let key = c
                .chain
                .iter()
                .min_by_key(|(_, (_, time))| time)
                .map(|(k, _)| k.clone())?;
            c.chain.remove(&key);
        }
    }
    if v["type"] != "assistant" {
        return None;
    }
    let m = &v["message"];
    let mid = safe_id(m["id"].as_str()?);
    let u = m.get("usage")?;
    let tokens = parse_tokens(u, true);
    if tokens.total().unwrap_or(0) == 0 {
        return None;
    }
    let complete = m.get("stop_reason").is_some_and(|v| !v.is_null());
    let prior = c.messages.get(&mid).cloned();
    let usage_hash = storage::digest(serde_json::to_string(&tokens).ok()?.as_bytes());
    if prior.as_ref().is_some_and(|(_, output, done)| {
        *done && !complete
            || *output > tokens.output
            || *done == complete && c.message_usage.get(&mid) == Some(&usage_hash)
    }) {
        return None;
    }
    let start = prior
        .map(|(at, _, _)| at)
        .or_else(|| parent.and_then(|id| c.chain.get(&id).map(|(_, at)| *at)));
    c.message_usage.insert(mid.clone(), usage_hash);
    c.messages
        .insert(mid.clone(), (start.unwrap_or(at), tokens.output, complete));
    if c.messages.len() > 128 {
        if let Some(key) = c
            .messages
            .iter()
            .min_by_key(|(_, v)| v.0)
            .map(|(k, _)| k.clone())
        {
            c.messages.remove(&key);
            c.message_usage.remove(&key);
        }
    }
    let mut r = base(
        "claude",
        safe_id(&format!("claude:{mid}")),
        c.session.clone(),
        model_id(m["model"].as_str()),
        tokens,
        SessionTiming {
            at,
            start,
            complete,
        },
    );
    r.attempts[0].response_id = Some(mid);
    Some(r)
}
