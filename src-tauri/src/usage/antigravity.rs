//! Read-only Antigravity app accounting. No conversation text or API headers persist.
use super::{
    model::*,
    pricing::Pricing,
    sessions::{Report, Repository},
};
use crate::storage::{self, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::{BufRead, BufReader, Seek, SeekFrom},
    path::{Path, PathBuf},
};

const LIMIT: usize = 32 * 1024 * 1024;
#[cfg(test)]
#[path = "antigravity_tests.rs"]
mod regression_tests;
#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct Cursor {
    modified: String,
    count: usize,
    covered: BTreeSet<u64>,
}
#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct FileCursor {
    offset: u64,
    tail: String,
    identity: String,
    modified: String,
    complete: bool,
}
fn file_identity(meta: &std::fs::Metadata) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        format!("{}:{}", meta.dev(), meta.ino())
    }
    #[cfg(not(unix))]
    {
        format!("{:?}", meta.created().ok())
    }
}
fn file_modified(meta: &std::fs::Metadata) -> Option<String> {
    let modified = meta.modified().ok()?;
    let d = modified.duration_since(std::time::UNIX_EPOCH).ok()?;
    Some(format!("{}:{}", d.as_secs(), d.subsec_nanos()))
}
fn file_settled(meta: &std::fs::Metadata) -> bool {
    meta.modified()
        .ok()
        .and_then(|m| std::time::SystemTime::now().duration_since(m).ok())
        .is_some_and(|age| age >= std::time::Duration::from_secs(2))
}
fn local_file_stamp(path: &Path) -> String {
    match std::fs::metadata(path) {
        Ok(meta) => format!(
            "local:{}:{}:{}",
            file_identity(&meta),
            meta.len(),
            file_modified(&meta).unwrap_or_default()
        ),
        Err(_) => "local:missing".into(),
    }
}
fn loopback_client() -> Result<reqwest::Client> {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    if let Some(client) = CLIENT.get() {
        return Ok(client.clone());
    }
    let built = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .danger_accept_invalid_certs(true)
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .map_err(|_| failure("Antigravity 本地连接初始化失败"))?;
    let _ = CLIENT.set(built.clone());
    Ok(built)
}
fn number(v: &Value) -> Option<u64> {
    v.as_u64()
        .or_else(|| v.as_str()?.parse().ok())
        .filter(|n| *n <= 1_000_000_000_000)
}
fn time(v: &Value) -> Option<i64> {
    v.as_str()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.timestamp_millis())
}
fn text<'a>(v: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|k| v[*k].as_str().filter(|s| !s.is_empty()))
}
fn duration(v: &Value) -> Option<u64> {
    let seconds = v.as_f64().or_else(|| {
        v.as_str()?
            .trim()
            .trim_end_matches('s')
            .trim()
            .parse::<f64>()
            .ok()
    })?;
    (seconds.is_finite() && (0.0..=u64::MAX as f64 / 1000.0).contains(&seconds))
        .then_some((seconds * 1000.0).round() as u64)
}
fn actual_model(v: Option<&str>) -> Option<String> {
    model_id(v)
        .filter(|s| !s.starts_with("MODEL_PLACEHOLDER_") && !s.starts_with("MODEL_UNSPECIFIED"))
}
fn label(v: Option<&str>) -> Option<String> {
    v.map(str::trim)
        .filter(|s| {
            !s.is_empty()
                && s.len() <= 120
                && !s.chars().any(char::is_control)
                && model_id(Some(&s.replace([' ', '(', ')'], ""))).is_some()
        })
        .map(str::to_owned)
}
pub(super) fn normalize_attempt(a: &mut Attempt) -> bool {
    let mut changed = false;
    if let Some((name, effort)) = a.model_display_name.as_deref().and_then(|name| {
        let (name, suffix) = name.strip_suffix(')')?.rsplit_once(" (")?;
        let effort = suffix.to_ascii_lowercase();
        (!name.is_empty()
            && matches!(
                effort.as_str(),
                "none" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max" | "ultra"
            ))
        .then(|| (name.to_owned(), effort))
    }) {
        a.model_display_name = Some(name);
        a.reasoning_effort.get_or_insert(effort);
        changed = true;
    }
    if let Some(total) = a
        .first_token_ms
        .zip(a.generation_ms)
        .and_then(|(f, g)| f.checked_add(g))
    {
        if a.duration_ms != total
            || a.availability
                .get("duration")
                .is_none_or(|s| s != "reported")
        {
            a.duration_ms = total;
            a.availability.insert("duration".into(), "reported".into());
            a.parse_reasons.retain(|s| s != "attempt_time_unreported");
            changed = true;
        }
    } else if a.duration_ms == 0
        && a.availability
            .get("duration")
            .is_none_or(|s| s == "reported")
    {
        a.availability
            .insert("duration".into(), "upstream_unreported".into());
        if !a
            .parse_reasons
            .iter()
            .any(|s| s == "attempt_time_unreported")
        {
            a.parse_reasons.push("attempt_time_unreported".into());
        }
        changed = true;
    }

    changed
}

pub(super) fn normalize_record(r: &mut Record) -> bool {
    if r.source != "antigravity" {
        return false;
    }
    let mut changed = false;
    for a in &mut r.attempts {
        changed |= normalize_attempt(a);
    }
    if let Some(a) = r.final_attempt() {
        if r.finished_at.is_some_and(|end| end <= a.started_at) && a.duration_ms > 0 {
            if let Some(end) = i64::try_from(a.duration_ms)
                .ok()
                .and_then(|d| a.started_at.checked_add(d))
            {
                r.finished_at = Some(end);
                changed = true;
            }
        }
    }
    changed
}
#[derive(Default)]
struct StepTime {
    start: Option<i64>,
    generated: Option<i64>,
    end: Option<i64>,
}
#[derive(Default)]
struct Steps {
    times: BTreeMap<u64, StepTime>,
    executions: BTreeMap<String, Vec<u64>>,
}
impl Steps {
    fn extend(&mut self, offset: u64, steps: &[Value]) {
        for (i, step) in steps.iter().enumerate() {
            let index = offset + i as u64;
            if self.times.contains_key(&index) {
                continue;
            }
            let m = &step["metadata"];
            if let Some(id) = m["executionId"].as_str() {
                self.executions.entry(id.into()).or_default().push(index);
            }
            self.times.insert(
                index,
                StepTime {
                    start: time(&m["startedAt"]).or_else(|| time(&m["createdAt"])),
                    generated: time(&m["finishedGeneratingAt"]),
                    end: time(&m["completedAt"]),
                },
            );
        }
    }
    fn related(&self, entry: &Value) -> Vec<&StepTime> {
        let indices: Vec<_> = entry["stepIndices"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(number)
            .collect();
        let indices = if indices.is_empty() {
            entry["executionId"]
                .as_str()
                .and_then(|id| self.executions.get(id))
                .map(Vec::as_slice)
                .unwrap_or_default()
        } else {
            &indices
        };
        indices.iter().filter_map(|i| self.times.get(i)).collect()
    }
}
#[cfg(test)]
impl From<&[Value]> for Steps {
    fn from(values: &[Value]) -> Self {
        let mut result = Self::default();
        result.extend(0, values);
        result
    }
}
fn tokens(u: &Value) -> Tokens {
    Tokens {
        input: number(&u["inputTokens"]),
        output: number(&u["outputTokens"]),
        cache_read: number(&u["cacheReadTokens"]),
        cache_write: number(&u["cacheWriteTokens"]),
        cache_write_5m: number(&u["cacheWrite5mTokens"]),
        cache_write_1h: number(&u["cacheWrite1hTokens"]),
        reasoning: number(&u["thinkingOutputTokens"]),
        ..Tokens::default()
    }
}
fn price(record: &mut Record, pricing: &Pricing, settings: &Settings) {
    for a in &mut record.attempts {
        a.cost_multiplier = settings.multiplier.clone();
        a.pricing_model = a
            .response_model
            .clone()
            .or_else(|| a.requested_model.clone());
        a.price = pricing
            .quote_at(a.pricing_model.as_deref(), &a.cost_multiplier, a.started_at)
            .calculate(&a.tokens, a.price_tier());
        a.pricing_basis = Some("estimated_import_snapshot".into());
        a.annotate_availability("antigravity");
    }
}
fn response_id(u: &Value) -> Option<&str> {
    text(u, &["responseId", "messageId"])
}
fn failed(v: &Value) -> bool {
    v.get("error")
        .is_some_and(|e| !e.is_null() && e != "" && e != &json!({}))
}
fn error_code(v: &Value) -> Option<String> {
    let e = &v["error"];
    [&e["code"], &e["type"], &v["errorCode"]]
        .into_iter()
        .find_map(|code| {
            code.as_i64()
                .map(|n| n.to_string())
                .or_else(|| label(code.as_str()))
        })
}

/// Request IDs separate calls sharing one execution. A final usage envelope
/// may mirror that request's last retry; equal counts alone never merge calls.
fn record(
    cascade: &str,
    entry: &Value,
    steps: &Steps,
    generator_index: usize,
) -> Result<Option<Record>> {
    let Some(chat) = entry.get("chatModel") else {
        return Ok(None);
    };
    let request_key = text(&chat["customMetadata"], &["request_id"]);
    let Some(key) = request_key.or_else(|| text(entry, &["executionId"])) else {
        if chat.get("usage").is_some()
            || chat["retryInfos"].as_array().is_some_and(|a| !a.is_empty())
        {
            return Err(failure("Antigravity 用量缺少可靠请求标识，未计入账单"));
        }
        return Ok(None);
    };
    let related = steps.related(entry);
    let start = related
        .iter()
        .filter_map(|s| s.start)
        .min()
        .or_else(|| time(&chat["startedAt"]));
    let finish = related.iter().filter_map(|s| s.generated).max();
    let end = related.iter().filter_map(|s| s.end).max();
    let id = if request_key.is_some() {
        safe_id(&format!("antigravity:request:{key}"))
    } else {
        safe_id(&format!("antigravity:{cascade}:{key}:{generator_index}"))
    };
    // Keep the retry identity used by earlier imports. A matching final envelope
    // supplements that attempt; it never creates another charge.
    let mut usages: Vec<(&Value, &Value, String)> = Vec::new();
    for (i, retry) in chat["retryInfos"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
    {
        let u = retry.get("usage").unwrap_or(&Value::Null);
        let attempt_key = response_id(u)
            .or_else(|| text(retry, &["traceId"]))
            .map(str::to_owned)
            .unwrap_or_else(|| format!("retry:{i}"));
        if let Some(existing) = usages
            .iter_mut()
            .find(|(other, _, _)| response_id(u).is_some() && response_id(u) == response_id(other))
        {
            existing.0 = u;
            existing.1 = retry;
        } else {
            usages.push((u, retry, attempt_key));
        }
    }
    let mut final_index = None;
    if let Some(u) = chat.get("usage").filter(|u| u.is_object()) {
        final_index = usages.iter().position(|(other, _, _)| {
            response_id(u).is_some() && response_id(u) == response_id(other)
        });
        if final_index.is_none()
            && request_key.is_some()
            && usages.last().is_some_and(|(last, _, _)| *last == u)
        {
            final_index = usages.len().checked_sub(1);
        }
        if let Some(index) = final_index {
            usages[index].0 = u;
        } else {
            if !usages.is_empty() && response_id(u).is_none() {
                return Err(failure(
                    "Antigravity 重试用量缺少响应标识，无法核对完整账单",
                ));
            }
            final_index = Some(usages.len());
            usages.push((u, chat, "final".into()));
        }
    }
    if chat.get("usage").is_none_or(Value::is_null) {
        final_index = usages.len().checked_sub(1);
    }
    if usages.is_empty() {
        usages.push((&Value::Null, chat, "final".into()));
        final_index = Some(0);
    }
    if final_index.is_some_and(|i| i + 1 != usages.len()) {
        return Err(failure("Antigravity 最终响应与重试顺序不一致，未计入账单"));
    }
    let Some(start) = start else {
        return Err(failure("Antigravity 用量缺少请求时间，未计入账单"));
    };
    let single = usages.len() == 1;
    let mut r = Record {
        id,
        client: "antigravity".into(),
        source: "antigravity".into(),
        started_at: start,
        session_id: Some(safe_id(cascade)),
        ..Record::default()
    };
    for (index, (u, info, attempt_key)) in usages.into_iter().enumerate() {
        let final_info = final_index == Some(index);
        let field = |key: &str| {
            info.get(key)
                .filter(|v| !v.is_null())
                .or_else(|| final_info.then(|| chat.get(key)).flatten())
                .unwrap_or(&Value::Null)
        };
        let first = duration(field("timeToFirstToken"));
        let generation = duration(field("streamingDuration"));
        let attempt_start = time(field("startedAt")).or_else(|| single.then_some(start));
        let reported_end =
            time(field("completedAt")).or_else(|| final_info.then_some(finish).flatten());
        let explicit_end =
            reported_end.filter(|end| attempt_start.is_none_or(|start| *end > start));
        let elapsed = first
            .zip(generation)
            .and_then(|(f, g)| f.checked_add(g))
            .or_else(|| {
                attempt_start
                    .zip(explicit_end)
                    .and_then(|(s, e)| u64::try_from(e.checked_sub(s)?).ok())
            });
        let is_failed = failed(info) || (final_info && failed(chat));
        let completed_at = explicit_end
            .or_else(|| {
                attempt_start
                    .zip(elapsed)
                    .and_then(|(s, d)| s.checked_add(i64::try_from(d).ok()?))
            })
            // A completion marker still establishes the outcome when its
            // clock has insufficient precision to measure elapsed time.
            .or_else(|| reported_end.filter(|end| attempt_start.is_none_or(|start| *end >= start)))
            .or_else(|| is_failed.then_some(end).flatten());
        let mut tokens = tokens(u);
        tokens.fill_missing(&self::tokens(&info["usage"]));
        let outcome = if is_failed {
            "failed"
        } else if completed_at.is_some() && tokens.input.is_some() && tokens.output.is_some() {
            "success"
        } else if completed_at.is_some() {
            "unknown"
        } else {
            "in_progress"
        };
        let response_model = actual_model(text(u, &["model"]))
            .or_else(|| actual_model(text(&info["usage"], &["model"])))
            .or_else(|| actual_model(text(info, &["responseModel"])))
            .or_else(|| {
                final_info
                    .then(|| actual_model(text(chat, &["responseModel"])))
                    .flatten()
            });
        let mut a = Attempt {
            id: safe_id(&format!("{}:{attempt_key}", r.id)),
            response_id: response_id(u).map(safe_id),
            upstream_request_hash: text(info, &["traceId"]).map(safe_id),
            thread_hash: r.session_id.clone(),
            requested_model: actual_model(text(chat, &["model"])),
            response_model,
            model_display_name: final_info
                .then(|| label(text(chat, &["modelDisplayName"])))
                .flatten(),
            api_provider: label(text(u, &["apiProvider"]))
                .or_else(|| label(text(&info["usage"], &["apiProvider"]))),
            error_code: error_code(info).or_else(|| final_info.then(|| error_code(chat)).flatten()),
            started_at: attempt_start.unwrap_or(start),
            duration_ms: elapsed.unwrap_or(0),
            first_token_ms: first,
            generation_ms: generation,
            stream: first.is_some() || generation.is_some(),
            transport: "local_api".into(),
            outcome: outcome.into(),
            usage_status: if tokens.input.is_some() && tokens.output.is_some() {
                "reported"
            } else {
                "partial"
            }
            .into(),
            raw_input_tokens: tokens.input,
            input_basis: Some("exclusive".into()),
            output_reported: Some(tokens.output.is_some()),
            tokens,
            service_tier: text(u, &["serviceTier"])
                .or_else(|| text(info, &["serviceTier"]))
                .or_else(|| final_info.then(|| text(chat, &["serviceTier"])).flatten())
                .map(str::to_owned),
            ..Attempt::default()
        };
        a.availability.insert(
            "duration".into(),
            if elapsed.is_some() {
                "reported"
            } else {
                "upstream_unreported"
            }
            .into(),
        );
        a.availability.insert(
            "attempt_start".into(),
            if attempt_start.is_some() {
                "reported"
            } else {
                "upstream_unreported"
            }
            .into(),
        );
        if elapsed.is_none() {
            a.parse_reasons.push("attempt_time_unreported".into());
        }
        if a.tokens
            .reasoning
            .zip(a.tokens.output)
            .is_some_and(|(thinking, output)| thinking > output)
        {
            a.parse_reasons.push("reasoning_exceeds_output".into());
        }
        normalize_attempt(&mut a);
        r.finished_at = completed_at;
        r.attempts.push(a);
    }
    r.completed = r.final_attempt().is_some_and(|a| a.outcome == "success");
    Ok(Some(r))
}

struct Api {
    http: reqwest::Client,
    origin: String,
    token: String,
}
impl Api {
    async fn bytes(request: reqwest::RequestBuilder) -> Result<Vec<u8>> {
        let mut response = request
            .send()
            .await
            .map_err(|_| failure("Antigravity 本地只读接口不可用，请启动 app 后同步"))?;
        if !response.status().is_success() {
            return Err(failure("Antigravity 本地接口拒绝读取，未更新同步位置"));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| failure("Antigravity 本地响应未完整读取"))?
        {
            if bytes.len().saturating_add(chunk.len()) > LIMIT {
                return Err(failure("Antigravity 单页用量超过读取限制"));
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }
    async fn connect() -> Result<Option<Self>> {
        let port_path = dirs::config_dir()
            .ok_or_else(|| failure("无法定位 Antigravity app"))?
            .join("Antigravity/DevToolsActivePort");
        let Some(bytes) = storage::read_bounded(&port_path, 4096)? else {
            return Ok(None);
        };
        let port = std::str::from_utf8(&bytes)
            .ok()
            .and_then(|s| s.lines().next())
            .and_then(|s| s.parse::<u16>().ok())
            .filter(|p| *p > 0)
            .ok_or_else(|| failure("Antigravity 本地端口无效"))?;
        let http = loopback_client()?;
        let pages: Value = serde_json::from_slice(
            &Self::bytes(http.get(format!("http://127.0.0.1:{port}/json/list"))).await?,
        )
        .map_err(|_| failure("Antigravity 窗口信息无效"))?;
        let origin = pages
            .as_array()
            .into_iter()
            .flatten()
            .filter(|p| p["type"] == "page")
            .filter_map(|p| url::Url::parse(p["url"].as_str()?).ok())
            .find(|u| {
                u.scheme() == "https"
                    && u.host_str() == Some("127.0.0.1")
                    && u.port().is_some()
                    && u.username().is_empty()
                    && u.password().is_none()
            })
            .ok_or_else(|| failure("未找到 Antigravity app 窗口"))?
            .origin()
            .ascii_serialization();
        let html = String::from_utf8(Self::bytes(http.get(format!("{origin}/"))).await?)
            .map_err(|_| failure("Antigravity app 配置不可读"))?;
        let raw = html
            .split("window.__APP_CONFIG__")
            .nth(1)
            .and_then(|s| s.split_once('=').map(|(_, s)| s))
            .ok_or_else(|| failure("Antigravity app 接口版本不支持"))?;
        let config = serde_json::Deserializer::from_str(raw.trim())
            .into_iter::<Value>()
            .next()
            .and_then(std::result::Result::ok)
            .ok_or_else(|| failure("Antigravity app 接口配置无效"))?;
        let token = config["csrfToken"]
            .as_str()
            .filter(|t| !t.is_empty() && t.len() < 4096)
            .ok_or_else(|| failure("Antigravity app 只读接口认证不可用"))?
            .to_owned();
        Ok(Some(Self {
            http,
            origin,
            token,
        }))
    }
    async fn call(&self, method: &str, body: Value) -> Result<Value> {
        let bytes = Self::bytes(
            self.http
                .post(format!(
                    "{}/exa.language_server_pb.LanguageServerService/{method}",
                    self.origin
                ))
                .header("Connect-Protocol-Version", "1")
                .header("x-codeium-csrf-token", &self.token)
                .json(&body),
        )
        .await?;
        serde_json::from_slice(&bytes).map_err(|_| failure("Antigravity 用量响应格式无效"))
    }
    #[cfg(test)]
    async fn collect(
        &self,
        store: &mut impl Repository,
        home: &Path,
        pricing: &Pricing,
        settings: &Settings,
        report: &mut Report,
        repair: bool,
    ) -> Result<()> {
        let brain = home.join("antigravity/brain");
        let files = collect_transcript_files(&brain)?;
        self.collect_with_files(store, &brain, &files, pricing, settings, report, repair)
            .await
    }
    #[allow(clippy::too_many_arguments)]
    async fn collect_with_files(
        &self,
        store: &mut impl Repository,
        brain: &Path,
        files: &[PathBuf],
        pricing: &Pricing,
        settings: &Settings,
        report: &mut Report,
        repair: bool,
    ) -> Result<()> {
        let all = self
            .call(
                "GetAllCascadeTrajectories",
                json!({"excludeSubtrajectories":false}),
            )
            .await?;
        let mut summaries = all["trajectorySummaries"]
            .as_object()
            .cloned()
            .ok_or_else(|| failure("Antigravity 会话目录不完整"))?;
        for path in files {
            let id = cascade_id(brain, path)?;
            let stamp = local_file_stamp(path);
            summaries
                .entry(id.to_owned())
                .or_insert_with(|| json!({"localModifiedStamp": stamp}));
        }
        report.total_files = summaries.len() as u64;
        for (id, summary) in &summaries {
            match self
                .collect_session(id, summary, store, pricing, settings, report, repair)
                .await
            {
                Ok(()) => report.files += 1,
                Err(error) => {
                    if store.requires_complete_history() {
                        return Err(error);
                    }
                    report.errors += 1;
                }
            }
            store.progress(report);
        }
        Ok(())
    }
    #[allow(clippy::too_many_arguments)]
    async fn collect_session(
        &self,
        id: &str,
        summary: &Value,
        store: &mut impl Repository,
        pricing: &Pricing,
        settings: &Settings,
        report: &mut Report,
        repair: bool,
    ) -> Result<()> {
        let cursor_id = safe_id(&format!("antigravity:rpc:{id}"));
        let mut cursor: Cursor = if repair {
            Cursor::default()
        } else {
            store
                .cursor(&cursor_id)?
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_default()
        };
        let modified = summary["lastModifiedTime"]
            .as_str()
            .filter(|s| !s.is_empty())
            .or_else(|| summary["localModifiedStamp"].as_str())
            .unwrap_or("");
        if !modified.is_empty() && cursor.modified == modified {
            return Ok(());
        }
        let total = number(&summary["stepCount"]);
        if total.is_some_and(|n| n > 100_000) {
            return Err(failure("Antigravity 会话步骤超过读取限制"));
        }
        let mut steps = Steps::default();
        let mut fallback_loaded = false;
        let mut offset = cursor.count.saturating_sub(1);
        let mut stable = offset;
        loop {
            let page=self.call("GetCascadeTrajectoryGeneratorMetadata",json!({"cascadeId":id,"generatorMetadataOffset":offset,"includeMessages":false})).await?;
            let entries = match page["generatorMetadata"].as_array() {
                Some(entries) => entries,
                None if page.as_object().is_some_and(|o| o.is_empty()) => break,
                None => return Err(failure("Antigravity 历史用量读取不完整")),
            };
            if entries.is_empty() {
                break;
            }
            if offset.saturating_add(entries.len()) > 100_000 {
                return Err(failure("Antigravity 会话用量超过读取限制"));
            }
            let fallback = entries.iter().any(|e| {
                e.get("chatModel").is_some()
                    && e["stepIndices"].as_array().is_none_or(|a| a.is_empty())
            });
            let required: BTreeSet<_> = entries
                .iter()
                .flat_map(|e| {
                    e["stepIndices"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(number)
                })
                .collect();
            if required
                .last()
                .is_some_and(|i| *i >= 100_000 || total.is_some_and(|n| *i >= n))
            {
                return Err(failure("Antigravity 用量关联步骤不完整"));
            }
            let missing = if fallback && !fallback_loaded {
                Some(0)
            } else {
                required
                    .iter()
                    .find(|i| !steps.times.contains_key(i))
                    .copied()
            };
            if let Some(mut step_offset) = missing {
                loop {
                    let page=self.call("GetCascadeTrajectorySteps",json!({"cascadeId":id,"stepOffset":step_offset,"trajectoryVerbosity":2})).await?;
                    let next = page["steps"]
                        .as_array()
                        .map(Vec::as_slice)
                        .or_else(|| page.as_object().filter(|o| o.is_empty()).map(|_| &[][..]))
                        .ok_or_else(|| failure("Antigravity 历史步骤读取不完整"))?;
                    if next.is_empty() {
                        if required.iter().all(|i| steps.times.contains_key(i))
                            && total.is_none_or(|n| step_offset >= n)
                        {
                            break;
                        }
                        return Err(failure("Antigravity 历史步骤读取不完整"));
                    }
                    if step_offset.saturating_add(next.len() as u64) > 100_000 {
                        return Err(failure("Antigravity 会话步骤超过读取限制"));
                    }
                    steps.extend(step_offset, next);
                    step_offset += next.len() as u64;
                    if required.iter().all(|i| steps.times.contains_key(i))
                        && (!fallback || total.is_some_and(|n| step_offset >= n))
                    {
                        break;
                    }
                }
            }
            fallback_loaded |= fallback;
            for batch in entries.chunks(128) {
                let mut rows = Vec::new();
                for entry in batch {
                    let parsed = match record(id, entry, &steps, offset) {
                        Ok(parsed) => parsed,
                        Err(error) => {
                            if store.requires_complete_history() {
                                return Err(error);
                            }
                            report.errors += 1;
                            offset += 1;
                            continue;
                        }
                    };
                    let complete = match parsed {
                        Some(mut r) => {
                            let complete = r.finished_at.is_some()
                                && r.final_attempt()
                                    .is_some_and(|a| a.outcome != "in_progress");
                            price(&mut r, pricing, settings);
                            rows.push(r);
                            complete
                        }
                        None => entry.get("chatModel").is_none(),
                    };
                    if complete {
                        if stable == offset {
                            stable += 1;
                        }
                        let indices: Vec<_> = entry["stepIndices"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(number)
                            .collect();
                        if indices.is_empty() {
                            if let Some(indices) = entry["executionId"]
                                .as_str()
                                .and_then(|id| steps.executions.get(id))
                            {
                                cursor.covered.extend(indices);
                            }
                        } else {
                            cursor.covered.extend(indices);
                        }
                    } else if entry.get("chatModel").is_some() {
                        if store.requires_complete_history() {
                            return Err(failure("Antigravity 历史用量尚未完整，请稍后重试"));
                        }
                        report.errors += 1;
                    }
                    offset += 1;
                }
                store.write_batch(&rows, None)?;
                report.imported += rows.len() as u64;
            }
        }
        // Commit only after the whole session is readable. Failed sessions keep
        // their previous cursor, so already saved batches are safely replayed.
        cursor.count = stable;
        cursor.modified = if stable == offset {
            modified.into()
        } else {
            String::new()
        };
        let cursor =
            serde_json::to_string(&cursor).map_err(|_| failure("Antigravity 同步位置无效"))?;
        store.write_batch(&[], Some((&cursor_id, &cursor)))
    }
}

fn cascade_id<'a>(brain: &Path, path: &'a Path) -> Result<&'a str> {
    path.strip_prefix(brain)
        .ok()
        .and_then(|p| p.components().next())
        .and_then(|c| c.as_os_str().to_str())
        .ok_or_else(|| failure("Antigravity 历史路径无效"))
}

fn collect_transcript_files(brain: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    transcript_files(brain, &mut files, 0)?;
    Ok(files)
}

fn should_skip_transcript_subdir(depth: usize, name: &std::ffi::OsStr) -> bool {
    depth >= 1
        && matches!(
            name.to_str(),
            Some("scratch" | "node_modules" | "target" | ".git")
        )
}

fn transcript_files(root: &Path, out: &mut Vec<PathBuf>, depth: usize) -> Result<()> {
    if !root.exists() {
        return Ok(());
    }
    if depth > 8 || out.len() > 30_000 {
        return Err(failure("Antigravity 历史目录超过读取限制"));
    }
    for item in std::fs::read_dir(root).map_err(storage::io_error)? {
        let item = item.map_err(storage::io_error)?;
        let kind = item.file_type().map_err(storage::io_error)?;
        let name = item.file_name();
        if kind.is_dir() {
            if !should_skip_transcript_subdir(depth, &name) {
                transcript_files(&item.path(), out, depth + 1)?;
            }
        } else if kind.is_file() && name == "transcript.jsonl" {
            out.push(item.path());
        }
    }
    Ok(())
}
#[cfg(test)]
pub(super) fn scan_files(
    store: &mut impl Repository,
    home: &Path,
    pricing: &Pricing,
    settings: &Settings,
    report: &mut Report,
    repair: bool,
) -> Result<()> {
    let brain = home.join("antigravity/brain");
    let files = collect_transcript_files(&brain)?;
    scan_file_list(store, &brain, &files, pricing, settings, report, repair)
}
fn scan_file_list(
    store: &mut impl Repository,
    brain: &Path,
    files: &[PathBuf],
    pricing: &Pricing,
    settings: &Settings,
    report: &mut Report,
    repair: bool,
) -> Result<()> {
    for path in files {
        if let Err(error) = scan_file(store, brain, path, pricing, settings, report, repair) {
            if store.requires_complete_history() {
                return Err(error);
            }
            report.errors += 1;
        }
    }
    Ok(())
}
#[allow(clippy::too_many_arguments)]
fn scan_file(
    store: &mut impl Repository,
    brain: &Path,
    path: &Path,
    pricing: &Pricing,
    settings: &Settings,
    report: &mut Report,
    repair: bool,
) -> Result<()> {
    let key = safe_id(&format!("antigravity:file:{}", path.to_string_lossy()));
    let saved_raw = if repair { None } else { store.cursor(&key)? };
    let mut c: FileCursor = saved_raw
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_default();
    let meta = std::fs::metadata(path).map_err(storage::io_error)?;
    let identity = file_identity(&meta);
    let modified = file_modified(&meta).unwrap_or_default();
    if !repair
        && c.complete
        && !modified.is_empty()
        && c.identity == identity
        && meta.len() == c.offset
        && c.modified == modified
        && file_settled(&meta)
    {
        return Ok(());
    }
    let cascade = cascade_id(brain, path)?;
    let rpc: Cursor = store
        .cursor(&safe_id(&format!("antigravity:rpc:{cascade}")))?
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let mut file = File::open(path).map_err(storage::io_error)?;
    let meta = file.metadata().map_err(storage::io_error)?;
    let identity = file_identity(&meta);
    let modified = file_modified(&meta).unwrap_or_default();
    let tail = |file: &mut File, offset: u64| -> Result<String> {
        use std::io::Read;
        file.seek(SeekFrom::Start(offset.saturating_sub(4096)))
            .map_err(storage::io_error)?;
        let mut b = vec![0; offset.min(4096) as usize];
        file.read_exact(&mut b).map_err(storage::io_error)?;
        Ok(storage::digest(&b))
    };
    let mut verified_tail = None;
    let valid_prefix = c.identity == identity && meta.len() >= c.offset && {
        let digest = tail(&mut file, c.offset)?;
        let ok = digest == c.tail;
        if ok {
            verified_tail = Some(digest);
        }
        ok
    };
    if !valid_prefix {
        c = FileCursor::default();
        verified_tail = None;
    }
    c.identity = identity;
    let start_offset = c.offset;
    file.seek(SeekFrom::Start(c.offset))
        .map_err(storage::io_error)?;
    let mut reader = BufReader::new(file);
    let mut rows = Vec::new();
    let mut bytes = Vec::new();
    let mut gap = false;
    let mut partial_tail = false;
    loop {
        bytes.clear();
        use std::io::Read;
        let size = (&mut reader)
            .take(LIMIT as u64 + 1)
            .read_until(b'\n', &mut bytes)
            .map_err(storage::io_error)?;
        if size == 0 {
            break;
        }
        if size > LIMIT {
            return Err(failure("Antigravity 历史单行超过读取限制"));
        }
        if bytes.last() != Some(&b'\n') {
            if store.requires_complete_history() {
                return Err(failure(
                    "Antigravity 历史仍在写入，未导出、未清理，请稍后重试",
                ));
            }
            partial_tail = true;
            break;
        }
        let parsed = (|| -> Result<Option<Record>> {
            let v: Value = serde_json::from_slice(&bytes)
                .map_err(|_| failure("Antigravity 历史文件存在无效完整行"))?;
            let has_usage = [
                "input_tokens",
                "output_tokens",
                "cache_read_tokens",
                "cache_write_tokens",
            ]
            .iter()
            .any(|k| number(&v[*k]).is_some());
            if has_usage && !number(&v["step_index"]).is_some_and(|i| rpc.covered.contains(&i)) {
                let request = text(&v, &["request_id"]);
                let model = text(&v, &["model", "response_model"]);
                let at = time(&v["completed_at"]).or_else(|| time(&v["created_at"]));
                let (Some(request), Some(model), Some(at)) = (request, model, at) else {
                    return Err(failure(
                        "Antigravity 历史用量缺少请求标识、模型或时间，请启动 app 后重新同步",
                    ));
                };
                let id = safe_id(&format!("antigravity:request:{request}"));
                let mut r = Record {
                    id: id.clone(),
                    client: "antigravity".into(),
                    source: "antigravity".into(),
                    session_id: Some(safe_id(cascade)),
                    started_at: at,
                    finished_at: Some(at),
                    // A transcript usage row reports consumption, not the model's
                    // terminal outcome. Only the API can supplement that evidence.
                    completed: false,
                    attempts: vec![Attempt {
                        id: safe_id(&format!("{id}:final")),
                        response_id: text(&v, &["response_id", "message_id"]).map(safe_id),
                        requested_model: actual_model(Some(model)),
                        response_model: actual_model(Some(model)),
                        started_at: at,
                        outcome: "unknown".into(),
                        transport: "history_file".into(),
                        tokens: Tokens {
                            input: number(&v["input_tokens"]),
                            output: number(&v["output_tokens"]),
                            cache_read: number(&v["cache_read_tokens"]),
                            cache_write: number(&v["cache_write_tokens"]),
                            cache_write_5m: number(&v["cache_write_5m_tokens"]),
                            cache_write_1h: number(&v["cache_write_1h_tokens"]),
                            reasoning: number(&v["thinking_output_tokens"]),
                            ..Tokens::default()
                        },
                        usage_status: "reported".into(),
                        parse_reasons: vec!["attempt_time_unreported".into()],
                        ..Attempt::default()
                    }],
                    ..Record::default()
                };
                price(&mut r, pricing, settings);
                return Ok(Some(r));
            }
            Ok(None)
        })();
        match parsed {
            Ok(Some(row)) => rows.push(row),
            Ok(None) => {}
            Err(error) => {
                if store.requires_complete_history() {
                    return Err(error);
                }
                gap = true;
                report.errors += 1;
            }
        }
        if !gap {
            c.offset = reader.stream_position().map_err(storage::io_error)?;
        }
        if rows.len() >= 128 {
            store.write_batch(&rows, None)?;
            report.imported += rows.len() as u64;
            rows.clear();
        }
    }
    report.imported += rows.len() as u64;
    c.tail = match verified_tail.filter(|_| c.offset == start_offset) {
        Some(digest) => digest,
        None => tail(reader.get_mut(), c.offset)?,
    };
    c.complete = !gap && !partial_tail && c.offset == meta.len();
    c.modified = if c.complete { modified } else { String::new() };
    let cursor = serde_json::to_string(&c).map_err(|_| failure("Antigravity 历史同步位置无效"))?;
    if !rows.is_empty() || saved_raw.as_deref() != Some(cursor.as_str()) {
        store.write_batch(&rows, Some((&key, &cursor)))?;
    }
    Ok(())
}

pub fn sync(
    store: &mut impl Repository,
    pricing: &Pricing,
    settings: &Settings,
    home: &Path,
    repair: bool,
) -> Result<Report> {
    let mut report = Report {
        phase: "syncing".into(),
        ..Report::default()
    };
    let pricing = pricing.frozen();
    let brain = home.join("antigravity/brain");
    let files = collect_transcript_files(&brain)?;
    let runtime = tokio::runtime::Handle::try_current()
        .map_err(|_| failure("Antigravity 同步运行时不可用"))?;
    let connected = runtime.block_on(async {
        match Api::connect().await? {
            Some(api) => {
                api.collect_with_files(
                    store,
                    &brain,
                    &files,
                    &pricing,
                    settings,
                    &mut report,
                    repair,
                )
                .await?;
                Ok(true)
            }
            None if repair => Err(failure("请启动 Antigravity app 后补全历史")),
            None => Ok(false),
        }
    });
    // Already collected details remain available while the app is closed. Any
    // new uncorrelated file usage makes a complete export fail explicitly.
    scan_file_list(
        store,
        &brain,
        &files,
        &pricing,
        settings,
        &mut report,
        repair,
    )?;
    connected?;
    report.phase = if report.errors == 0 {
        "done"
    } else {
        "partial"
    }
    .into();
    report.completed_at = Some(now());
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shared_execution_keeps_requests_separate_and_final_mirror_is_not_a_retry() {
        let steps = json!([{"metadata":{"executionId":"exec","startedAt":"2026-01-01T00:00:00Z","finishedGeneratingAt":"2026-01-01T00:00:03Z"}}]);
        let mut entry = json!({"executionId":"exec","stepIndices":[0],"chatModel":{"customMetadata":{"request_id":"request-one"},"model":"fixture-model","usage":{"inputTokens":"9","outputTokens":"3"},"retryInfos":[{"usage":{"inputTokens":"9","outputTokens":"3"}}]}});
        let one = record(
            "parent",
            &entry,
            &Steps::from(steps.as_array().unwrap().as_slice()),
            0,
        )
        .unwrap()
        .unwrap();
        assert_eq!(one.attempts.len(), 1);
        assert_eq!(one.tokens().total(), Some(12));
        assert_eq!(
            one.id,
            record(
                "child",
                &entry,
                &Steps::from(steps.as_array().unwrap().as_slice()),
                2
            )
            .unwrap()
            .unwrap()
            .id
        );
        entry["chatModel"]["customMetadata"]["request_id"] = json!("request-two");
        let two = record(
            "parent",
            &entry,
            &Steps::from(steps.as_array().unwrap().as_slice()),
            1,
        )
        .unwrap()
        .unwrap();
        assert_ne!(one.id, two.id);
    }
    #[test]
    fn retries_are_unique_and_reasoning_is_subset() {
        let steps = json!([{"metadata":{"executionId":"exec","startedAt":"2026-01-01T00:00:00Z","finishedGeneratingAt":"2026-01-01T00:00:03Z"}}]);
        let m = json!({"executionId":"exec","stepIndices":[0],"chatModel":{"model":"claude-fixture","usage":{"responseId":"last","inputTokens":"10","cacheReadTokens":"20","outputTokens":"5","thinkingOutputTokens":"3"},"retryInfos":[{"traceId":"first","error":"unavailable","usage":{"inputTokens":"2","outputTokens":"0"}},{"usage":{"responseId":"last","inputTokens":"10","cacheReadTokens":"20","outputTokens":"5","thinkingOutputTokens":"3"}}]}});
        let r = record(
            "cascade",
            &m,
            &Steps::from(steps.as_array().unwrap().as_slice()),
            0,
        )
        .unwrap()
        .unwrap();
        assert_eq!(r.attempts.len(), 2);
        assert_eq!(r.tokens().total(), Some(37));
        assert_eq!(r.tokens().reasoning, Some(3));
        assert!(r.completed);
        assert_eq!(r.attempts[0].outcome, "failed");
        assert_eq!(r.attempts[1].tokens.cache_write, None);
        assert_ne!(
            r.id,
            record(
                "child",
                &m,
                &Steps::from(steps.as_array().unwrap().as_slice()),
                0
            )
            .unwrap()
            .unwrap()
            .id
        );
    }
    #[test]
    fn estimates_and_ambiguous_retries_do_not_become_bills() {
        let steps = json!([{"metadata":{"executionId":"exec","createdAt":"2026-01-01T00:00:00Z"}}]);
        let m = json!({"executionId":"exec","chatModel":{"chatStartMetadata":{"contextWindowMetadata":{"estimatedTokensUsed":500}}}});
        let pending = record(
            "cascade",
            &m,
            &Steps::from(steps.as_array().unwrap().as_slice()),
            0,
        )
        .unwrap()
        .unwrap();
        assert_eq!(pending.tokens().total(), None);
        assert_eq!(pending.final_attempt().unwrap().outcome, "in_progress");
        let m = json!({"executionId":"exec","chatModel":{"usage":{"inputTokens":"5"},"retryInfos":[{"usage":{"inputTokens":"5"}}]}});
        assert!(record(
            "cascade",
            &m,
            &Steps::from(steps.as_array().unwrap().as_slice()),
            0
        )
        .is_err());
    }
}

#[cfg(test)]
mod history_tests {
    use super::super::store::Store;
    use super::*;
    #[test]
    fn history_is_incremental_and_correlates_rpc_steps_without_duplicates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir
            .path()
            .join("antigravity/brain/cascade/.system_generated/logs/transcript.jsonl");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut store = Store::open(&dir.path().join("db")).unwrap();
        let pricing = Pricing::new(&dir.path().join("db")).unwrap();
        let mut report = Report::default();
        let cursor = serde_json::to_string(&Cursor {
            covered: BTreeSet::from([3]),
            ..Cursor::default()
        })
        .unwrap();
        store
            .write_batch(&[], Some((&safe_id("antigravity:rpc:cascade"), &cursor)))
            .unwrap();
        let rows=concat!("{\"step_index\":3,\"input_tokens\":10,\"output_tokens\":2}\n", "{\"request_id\":\"request-fixture\",\"model\":\"fixture-model\",\"created_at\":\"2026-01-01T00:00:00Z\",\"input_tokens\":10,\"output_tokens\":3}\n");
        std::fs::write(&path, rows).unwrap();
        scan_files(
            &mut store,
            dir.path(),
            &pricing,
            &Settings::default(),
            &mut report,
            false,
        )
        .unwrap();
        assert_eq!(report.imported, 1);
        let saved = store
            .detail(&safe_id("antigravity:request:request-fixture"))
            .unwrap();
        assert_eq!(saved.known_success(), None);
        assert_eq!(saved.attempts[0].measured_duration_ms(), None);
        scan_files(
            &mut store,
            dir.path(),
            &pricing,
            &Settings::default(),
            &mut report,
            false,
        )
        .unwrap();
        assert_eq!(report.imported, 1);
        assert_eq!(
            store.dashboard(&Filter::default()).unwrap().totals.requests,
            1
        );
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(b"{\"step_index\":99,\"input_tokens\":10}\n")
            .unwrap();
        assert!(scan_files(
            &mut store,
            dir.path(),
            &pricing,
            &Settings::default(),
            &mut report,
            false,
        )
        .is_ok());
        assert_eq!(report.errors, 1);
        assert_eq!(
            store.dashboard(&Filter::default()).unwrap().totals.requests,
            1
        );
    }

    #[test]
    fn transcript_discovery_keeps_system_logs_while_skipping_scratch_and_build_trees() {
        let dir = tempfile::tempdir().unwrap();
        let brain = dir.path().join("antigravity/brain");
        let valid = brain.join("cascade-one/.system_generated/logs/transcript.jsonl");
        let scratch = brain.join("cascade-one/scratch/sub/transcript.jsonl");
        let modules = brain.join("cascade-one/node_modules/pkg/transcript.jsonl");
        let target_dir = brain.join("cascade-one/target/debug/transcript.jsonl");
        for path in [&valid, &scratch, &modules, &target_dir] {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "{}\n").unwrap();
        }
        let discovered = collect_transcript_files(&brain).unwrap();
        assert_eq!(discovered, vec![valid]);
    }
}
