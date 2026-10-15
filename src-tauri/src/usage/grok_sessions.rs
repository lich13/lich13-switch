//! Native ACP prompt summaries. A prompt is not an individual inference request.
use super::{base, Cursor, SessionTiming};
use crate::usage::model::{model_id, safe_id, Attempt, Record, Tokens};
use serde_json::Value;

fn tokens(value: &Value) -> Tokens {
    let inclusive = value["inputTokens"].as_u64();
    let read = value["cachedReadTokens"]
        .as_u64()
        .or(value["cacheReadInputTokens"].as_u64());
    let write = value["cacheCreationTokens"]
        .as_u64()
        .or(value["cacheCreationInputTokens"].as_u64());
    let fresh = inclusive.and_then(|n| {
        n.checked_sub(read.unwrap_or(0))?
            .checked_sub(write.unwrap_or(0))
    });
    Tokens {
        inclusive_input: inclusive,
        input: fresh,
        output: value["outputTokens"].as_u64(),
        cache_read: read,
        cache_write: write,
        ..Tokens::default()
    }
}

pub(super) fn parse(value: &Value, cursor: &mut Cursor, at: i64) -> Option<Record> {
    let params = value.get("params")?;
    let update = params.get("update")?;
    let session = params["sessionId"].as_str().map(safe_id)?;
    cursor.session = Some(session.clone());
    if let Some(model) = update
        .pointer("/_meta/modelId")
        .and_then(Value::as_str)
        .and_then(|m| model_id(Some(m)))
    {
        cursor.model = Some(model);
    }
    if update["sessionUpdate"] != "turn_completed" {
        return None;
    }
    let prompt = update["prompt_id"].as_str()?;
    let usage = update.get("usage").unwrap_or(&Value::Null);
    let id = safe_id(&format!("grok:{session}:{prompt}"));
    let elapsed = update["elapsed_ms"].as_u64().filter(|n| *n <= 86_400_000);
    let start = elapsed.and_then(|n| at.checked_sub(n as i64));
    let complete = matches!(
        update["stop_reason"].as_str(),
        Some("end_turn" | "stop" | "completed" | "max_tokens")
    );
    let mut record = base(
        "grok",
        id.clone(),
        Some(session),
        cursor.model.clone(),
        tokens(usage),
        SessionTiming {
            at,
            start,
            complete,
        },
    );
    // This source has its own query view. Never fingerprint-match a multi-call
    // prompt to a gateway request, even when its aggregate counters happen to agree.
    record.attempts[0].transport = "session_prompt".into();
    record.attempts[0].usage_status = if record.attempts[0].tokens.total().is_some() {
        "reported"
    } else {
        "missing"
    }
    .into();
    record.attempts[0].repeat_count = usage["modelCalls"].as_u64().filter(|n| *n > 0).unwrap_or(1);
    if let Some(models) = usage["modelUsage"]
        .as_object()
        .filter(|v| !v.is_empty() && v.len() <= 64)
    {
        let parts: Vec<_> = models
            .iter()
            .filter_map(|(model, usage)| {
                let model = model_id(Some(model))?;
                Some(Attempt {
                    id: safe_id(&format!("{id}:{model}")),
                    requested_model: Some(model.clone()),
                    response_model: Some(model),
                    tokens: tokens(usage),
                    repeat_count: usage["modelCalls"].as_u64().filter(|n| *n > 0).unwrap_or(1),
                    started_at: start.unwrap_or(at),
                    duration_ms: usage["apiDurationMs"].as_u64().unwrap_or(0),
                    usage_status: "reported".into(),
                    transport: "session_prompt".into(),
                    outcome: if complete { "completed" } else { "reported" }.into(),
                    ..Attempt::default()
                })
            })
            .collect();
        // Use the breakdown only when it covers every reported aggregate field;
        // otherwise retain the complete prompt total instead of losing calls.
        let mut sum = Tokens::default();
        for a in &parts {
            sum.add(&a.tokens);
        }
        let total = tokens(usage);
        if !parts.is_empty()
            && sum.inclusive_input == total.inclusive_input
            && sum.output == total.output
            && sum.cache_read == total.cache_read
            && sum.cache_write == total.cache_write
        {
            record.attempts = parts;
        }
    }
    Some(record)
}
