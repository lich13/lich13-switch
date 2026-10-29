//! Bounded observation only: bytes forwarded by the gateway are never altered.
use super::metadata::Projector;
use crate::errors::code;
use serde_json::Value;
use std::io::Write;

#[derive(Default, Clone, Debug)]
pub struct Observation {
    pub failure_rules: Vec<super::failure_rules::Rule>,
    pub meter: crate::usage::model::Meter,
    pub error: crate::events::Details,
    limit_headers: hyper::HeaderMap,
    pub(super) raw_headers: Vec<crate::event_raw::Header>,
    http_status: Option<u16>,
    pub response_id: Option<String>,
    pub model: Option<String>,
    pub incomplete: bool,
    pub terminal: Option<Terminal>,
    pub terminal_event: Option<String>,
    pub terminal_status: Option<String>,
    pub incomplete_reason: Option<String>,
    pub expects_terminal: bool,
    pub first_event_model_error: Option<bool>,
    pub first_event_capacity_error: Option<bool>,
    pub first_event_account_error: Option<bool>,
    pub first_event_service_error: Option<bool>,
    pub retry_after: Option<std::time::Duration>,
    pub capacity_error: bool,
    pub search_content: bool,
    pub opaque_output: bool,
    pub compaction_fingerprint: Option<String>,
    pub compaction_incompatible: bool,
    responses_seen: bool,
    pending_error_response: Option<String>,
    usage_tail: bool,
    strict_tail_identity: bool,
    fault_sequence: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Terminal {
    Success,
    Limited,
    Rejected,
    ModelUnavailable,
    Failure,
    Cancelled,
    Unknown,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RoutingSettlement {
    Delivered,
    DownstreamClosed,
    ReportedFailure,
    Failed,
}
#[derive(Clone, PartialEq)]
struct RoutingFault {
    protocol_sequence: u64,
    error: crate::events::Details,
    status: Option<u16>,
    incomplete: bool,
    transport: bool,
    unspecified: bool,
    tail_failure: bool,
}
#[derive(Default)]
struct FaultReports {
    observed: Option<RoutingFault>,
    generation: u64,
    reported: u64,
    unreported_before: bool,
}
impl FaultReports {
    fn observe(&mut self, fault: RoutingFault) {
        if self.observed.as_ref() == Some(&fault) {
            return;
        }
        if self.observed.as_ref().is_some_and(|old| {
            self.generation != self.reported
                && old.status.is_some()
                && old.status == fault.status
                && old.error == crate::events::Details::default()
                && old.protocol_sequence == 0
                && !old.incomplete
                && !old.transport
                && !old.tail_failure
                && !fault.incomplete
                && !fault.transport
                && !fault.tail_failure
        }) {
            self.observed = Some(fault);
            return;
        }
        self.unreported_before |= self.generation != self.reported;
        self.generation = self.generation.saturating_add(1);
        self.observed = Some(fault);
    }
    fn covered(&self) -> bool {
        self.generation != 0 && self.generation == self.reported && !self.unreported_before
    }
}
#[derive(Clone, Copy)]
enum DownstreamClose {
    Closed,
    Reset,
    BrokenPipe,
    Cancelled,
}
pub fn identifier(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str)
        .filter(|s| !s.is_empty() && s.len() <= 256 && !s.chars().any(char::is_control))
        .map(str::to_owned)
}
impl Observation {
    pub fn value(&mut self, outer: &Value) {
        let sequence = self.fault_sequence;
        self.value_projected(outer, false);
        if self.fault_sequence != sequence {
            self.error.raw = Some(std::sync::Arc::new(crate::event_raw::Raw::bytes(
                &serde_json::to_vec(outer).unwrap_or_default(),
                self.raw_headers.clone(),
                None,
            )));
        }
    }
    fn value_projected(&mut self, outer: &Value, projected: bool) {
        let value = outer
            .get("response")
            .filter(|v| v.is_object())
            .or_else(|| outer.get("message").filter(|v| v.is_object()))
            .unwrap_or(outer);
        self.search_content |= outer.get("results").is_some_and(Value::is_array)
            || outer.get("web_pages").is_some_and(Value::is_array)
            || matches!(
                outer.get("type").and_then(Value::as_str),
                Some("search.result" | "search.results" | "web_search.result")
            );
        let event = outer.get("type").and_then(Value::as_str).unwrap_or("");
        let status = value.get("status").and_then(Value::as_str).unwrap_or("");
        let response_id = crate::usage::model::response_id(outer);
        let explicit_terminal =
            matches!(
                event,
                "response.completed"
                    | "response.done"
                    | "response.failed"
                    | "response.incomplete"
                    | "response.cancelled"
                    | "response.canceled"
            ) || matches!(status, "completed" | "failed" | "incomplete" | "cancelled");
        if self.usage_tail {
            if let Some(current) = self.response_id.as_deref() {
                if response_id.is_some_and(|id| id != current)
                    || ((self.strict_tail_identity || self.responses_seen)
                        && response_id != Some(current))
                {
                    return;
                }
            }
        }
        if let Some(pending) = &self.pending_error_response {
            // Only the same response can resolve a provisional Responses error.
            // These checks affect observation, never the bytes sent downstream.
            if response_id.is_some_and(|id| id != pending)
                || (explicit_terminal && response_id != Some(pending.as_str()))
            {
                return;
            }
        }
        self.responses_seen |= event.starts_with("response.")
            || outer.get("response_id").is_some()
            || outer.get("response").is_some_and(Value::is_object);
        self.compaction_incompatible |= super::compaction::incompatible(outer);
        if let Some(fingerprint) = super::compaction::output(outer, projected) {
            self.compaction_fingerprint = Some(fingerprint);
        }
        self.meter.observe(outer, 0);
        if self.meter.model.is_some() {
            self.model = self.meter.model.clone();
        }
        let (details, retry) = if let Some(status) = self.http_status.filter(|s| *s >= 400) {
            super::failure_policy::http_value_with_rules(
                status,
                &self.limit_headers,
                outer,
                &self.failure_rules,
            )
        } else {
            super::failure_policy::details_with_headers(
                outer,
                &self.limit_headers,
                &self.failure_rules,
            )
        };
        let has_error = details != crate::events::Details::default();
        if has_error || matches!(event, "error" | "response.failed") || status == "failed" {
            self.fault_sequence = self.fault_sequence.saturating_add(1);
        }
        if has_error {
            self.error = details;
            self.retry_after = self.retry_after.max(retry);
        }
        if let Some(id) = response_id {
            self.response_id = Some(id.into());
        }
        if let Some(model) = identifier(value.get("model")) {
            self.model = Some(model);
        }
        self.expects_terminal |= event.starts_with("response.")
            || matches!(
                event,
                "message_start"
                    | "message_delta"
                    | "content_block_start"
                    | "content_block_delta"
                    | "content_block_stop"
            )
            || outer.get("choices").is_some()
            || outer.get("candidates").is_some();
        if self.terminal.is_none() {
            self.capacity_error = super::failure_policy::temporary(&self.error);
            let terminal =
                if self.error.error_basis == Some(crate::events::ErrorBasis::ModelUnavailable) {
                    Some(Terminal::ModelUnavailable)
                } else {
                    match event {
                        "response.completed" | "response.done" | "message_stop" => {
                            Some(Terminal::Success)
                        }
                        "response.cancelled" | "response.canceled" => Some(Terminal::Cancelled),
                        "response.failed" => Some(rule_terminal(value, &self.error)),
                        "response.incomplete" => Some(incomplete_terminal(value)),
                        "message" if value.get("stop_reason").is_some_and(|v| !v.is_null()) => {
                            Some(Terminal::Success)
                        }
                        _ => match status {
                            "completed" => Some(Terminal::Success),
                            "failed" => Some(rule_terminal(value, &self.error)),
                            "incomplete" => Some(incomplete_terminal(value)),
                            "cancelled" => Some(Terminal::Cancelled),
                            _ if event == "error" => Some(rule_terminal(value, &self.error)),
                            _ if has_error => Some(rule_terminal(outer, &self.error)),
                            _ => None,
                        },
                    }
                };
            // A chat completion finishes only once all reported choices finish.
            let choices = outer.get("choices").and_then(Value::as_array);
            let terminal = terminal.or_else(|| {
                choices
                    .filter(|c| {
                        !c.is_empty()
                            && c.iter()
                                .all(|v| v.get("finish_reason").is_some_and(|v| !v.is_null()))
                    })
                    .map(|c| {
                        if c.iter().any(|v| v["finish_reason"] == "content_filter") {
                            Terminal::Rejected
                        } else if c.iter().any(|v| v["finish_reason"] == "length") {
                            Terminal::Limited
                        } else {
                            Terminal::Success
                        }
                    })
            });
            let terminal = terminal.or_else(|| gemini_terminal(outer));
            if self.pending_error_response.is_some()
                && matches!(terminal, Some(Terminal::Success | Terminal::Limited))
                && !explicit_terminal
            {
                // A stop marker from another protocol is not a Responses
                // terminal, even if it appears in the same transport stream.
                return;
            }
            if event == "error"
                && terminal == Some(Terminal::Failure)
                && !explicit_terminal
                && self.responses_seen
                && self.response_id.is_some()
                && !self.capacity_error
                && !super::failure_policy::hard_rejection(&self.error)
            {
                // Some Responses streams report an error before a final response
                // envelope. Do not settle usage, health or the WS turn yet.
                self.pending_error_response = self.response_id.clone();
            } else if let Some(terminal) = terminal {
                if self.pending_error_response.take().is_some() && terminal == Terminal::Success {
                    self.error = crate::events::Details::default();
                }
                self.terminal = Some(terminal);
                self.terminal_event = safe_terminal_event(event);
                self.terminal_status = safe_terminal_status(status);
                if event == "response.incomplete" || status == "incomplete" {
                    self.incomplete_reason = safe_incomplete_reason(value);
                }
            }
        }
    }
    fn finish_pending(&mut self) {
        if self.pending_error_response.take().is_some() && self.terminal.is_none() {
            self.terminal = Some(Terminal::Failure);
            self.terminal_event = Some("error".into());
        }
    }
    fn done(&mut self) {
        // A Responses tail needs the current response's terminal envelope;
        // an unscoped [DONE] cannot resolve it or attach another turn's usage.
        if self.usage_tail && self.responses_seen {
            return;
        }
        self.finish_pending();
        if self.terminal.is_none() {
            self.terminal = Some(Terminal::Success);
            self.terminal_event = Some("[DONE]".into());
        }
    }
}

fn safe_terminal_event(event: &str) -> Option<String> {
    matches!(
        event,
        "response.completed"
            | "response.done"
            | "response.failed"
            | "response.incomplete"
            | "response.cancelled"
            | "response.canceled"
            | "message_stop"
            | "message"
            | "error"
    )
    .then(|| event.to_owned())
}
fn safe_terminal_status(status: &str) -> Option<String> {
    (!status.is_empty()).then(|| {
        match status {
            "completed" | "failed" | "incomplete" | "cancelled" => status,
            _ => "unrecognized",
        }
        .into()
    })
}
fn safe_incomplete_reason(value: &Value) -> Option<String> {
    value
        .pointer("/incomplete_details/reason")
        .and_then(Value::as_str)
        .filter(|reason| !reason.is_empty())
        .map(|reason| {
            match reason {
                "max_output_tokens"
                | "max_tokens"
                | "content_filter"
                | "server_error"
                | "rate_limit_exceeded" => reason,
                _ => "unrecognized",
            }
            .into()
        })
}

fn rule_terminal(value: &Value, details: &crate::events::Details) -> Terminal {
    if super::failure_policy::temporary(details) {
        return Terminal::Failure;
    }
    if details.custom_rule_id.is_none() {
        return error_terminal(value);
    }
    use crate::events::ErrorBasis::*;
    match details.error_basis {
        Some(ModelUnavailable) => Terminal::ModelUnavailable,
        Some(InvalidRequest | RequestTooLarge | SafetyRejection) => Terminal::Rejected,
        _ => Terminal::Failure,
    }
}
fn error_terminal(value: &Value) -> Terminal {
    if super::failure_policy::model_error(value) {
        return Terminal::ModelUnavailable;
    }
    let e = value.get("error").unwrap_or(value);
    let code = e
        .get("code")
        .or_else(|| e.get("type"))
        .and_then(Value::as_str)
        .unwrap_or("");
    if matches!(
        code,
        "invalid_request_error"
            | "invalid_request"
            | "context_length_exceeded"
            | "response_not_found"
            | "content_filter"
    ) {
        Terminal::Rejected
    } else {
        Terminal::Failure
    }
}
fn incomplete_terminal(value: &Value) -> Terminal {
    match value
        .pointer("/incomplete_details/reason")
        .and_then(Value::as_str)
        .unwrap_or("")
    {
        "max_output_tokens" | "max_tokens" => Terminal::Limited,
        "content_filter" => Terminal::Rejected,
        "server_error" | "rate_limit_exceeded" => Terminal::Failure,
        _ => Terminal::Unknown,
    }
}
struct Sink {
    observation: Observation,
    json: Projector,
    stream: bool,
    field: Vec<u8>,
    line_kind: u8,
    event: String,
    data_prefix: Vec<u8>,
    data_len: usize,
    skip_space: bool,
    after_cr: bool,
    raw: crate::event_raw::Capture,
    raw_wire: crate::event_raw::Capture,
}
impl Sink {
    fn new(stream: bool) -> Self {
        Self {
            observation: Observation::default(),
            json: Projector::default(),
            stream,
            field: vec![],
            line_kind: 0,
            event: String::new(),
            data_prefix: vec![],
            data_len: 0,
            skip_space: false,
            after_cr: false,
            raw: crate::event_raw::Capture::default(),
            raw_wire: crate::event_raw::Capture::default(),
        }
    }
    fn data(&mut self, bytes: &[u8]) {
        self.raw.feed(bytes);
        self.data_prefix.extend(
            bytes
                .iter()
                .take(16usize.saturating_sub(self.data_prefix.len())),
        );
        self.data_len = self.data_len.saturating_add(bytes.len());
        self.json.feed(bytes);
    }
    fn event(&mut self) {
        if self.data_len == 0 {
            self.event.clear();
            self.raw_wire = crate::event_raw::Capture::default();
            return;
        }
        let json = std::mem::take(&mut self.json);
        let raw = std::mem::take(&mut self.raw).finish(
            self.observation.raw_headers.clone(),
            (!self.event.is_empty()).then(|| self.event.clone()),
        );
        let wire = std::mem::take(&mut self.raw_wire).finish(
            self.observation.raw_headers.clone(),
            Some(self.event.clone()),
        );
        let first = self.observation.first_event_model_error.is_none();
        if first {
            // A complete non-JSON SSE event must also release the response
            // prefix. Observation failure cannot stall transparent forwarding.
            self.observation.first_event_model_error = Some(false);
            self.observation.first_event_capacity_error = Some(false);
            self.observation.first_event_account_error = Some(false);
            self.observation.first_event_service_error = Some(false);
        }
        if self.data_len <= 16 && self.data_prefix.trim_ascii() == b"[DONE]" {
            self.observation.done();
        } else if let Some(mut v) = json.finish().or_else(|| {
            raw.text()
                .ok()
                .filter(|text| {
                    matches!(self.event.as_str(), "error" | "response.failed")
                        || super::failure_policy::bare_capacity(text)
                })
                .map(|text| serde_json::json!({"type":"error", "message":text}))
        }) {
            if matches!(self.event.as_str(), "error" | "response.failed")
                || super::failure_policy::has_error(&v)
            {
                if let Some(full) = raw.value() {
                    v = full;
                }
            }
            if v.get("type").is_none() && !self.event.is_empty() {
                if let Some(map) = v.as_object_mut() {
                    map.insert("type".into(), Value::String(self.event.clone()));
                }
            }
            let sequence = self.observation.fault_sequence;
            self.observation.value_projected(&v, true);
            if self.observation.fault_sequence != sequence {
                self.observation.error.raw = Some(std::sync::Arc::new(wire));
            }
            if first {
                self.observation.first_event_model_error = Some(
                    self.observation.error.error_basis
                        == Some(crate::events::ErrorBasis::ModelUnavailable),
                );
                self.observation.first_event_capacity_error =
                    Some(super::failure_policy::temporary(&self.observation.error));
                self.observation.first_event_account_error = Some(
                    super::failure_policy::hard_rejection(&self.observation.error),
                );
            }
            if first {
                self.observation.first_event_service_error = Some(
                    self.observation.terminal == Some(Terminal::Failure)
                        && self.observation.error.error_basis
                            == Some(crate::events::ErrorBasis::ServiceError),
                );
            }
        } else {
            self.observation.incomplete = true;
            // Unknown nonempty data is still opaque application output. Named
            // initialization/heartbeat events remain behind the pre-output gate.
            self.observation.opaque_output |=
                self.data_prefix.iter().any(|b| !b.is_ascii_whitespace())
                    && !matches!(
                        self.event.as_str(),
                        "ping"
                            | "heartbeat"
                            | "keepalive"
                            | "message_start"
                            | "response.created"
                            | "response.in_progress"
                    );
        }
        self.data_len = 0;
        self.data_prefix.clear();
        self.event.clear();
    }
    fn line_end(&mut self) {
        match self.line_kind {
            0 if self.field.is_empty() => self.event(),
            2 => {
                if let Ok(v) = std::str::from_utf8(&self.field) {
                    self.event = v.trim().to_owned();
                }
            }
            1 => self.data(b"\n"),
            _ => {}
        }
        self.field.clear();
        self.line_kind = 0;
        self.skip_space = false;
    }
    fn finish(&mut self) {
        if self.stream {
            let sequence = self.observation.fault_sequence;
            if self.after_cr {
                self.after_cr = false;
                self.line_end();
            }
            if self.line_kind != 0 || !self.field.is_empty() {
                self.line_end();
            }
            self.event();
            if self.observation.fault_sequence != sequence {
                if let Some(raw) = &mut self.observation.error.raw {
                    std::sync::Arc::make_mut(raw).incomplete = Some("流式错误事件未收完整".into());
                }
            }
        } else {
            let raw =
                std::mem::take(&mut self.raw).finish(self.observation.raw_headers.clone(), None);
            let projected = std::mem::take(&mut self.json).finish();
            let value = if self.observation.http_status.is_some_and(|s| s >= 400) {
                raw.value().or_else(|| raw.text().ok().map(Value::String))
            } else {
                projected
                    .and_then(|v| {
                        if super::failure_policy::has_error(&v) {
                            raw.value().or(Some(v))
                        } else {
                            Some(v)
                        }
                    })
                    .or_else(|| {
                        raw.text()
                            .ok()
                            .filter(|s| super::failure_policy::bare_capacity(s))
                            .map(Value::String)
                    })
            };
            if let Some(v) = value {
                self.observation.value_projected(&v, true);
                if self.observation.http_status.is_some_and(|s| s >= 400)
                    || super::failure_policy::has_error(&v)
                {
                    self.observation.error.raw = Some(std::sync::Arc::new(raw));
                }
            } else {
                self.observation.incomplete = true;
                if self.observation.http_status.is_some_and(|s| s >= 400) {
                    self.observation.error.raw = Some(std::sync::Arc::new(raw));
                }
            }
        }
        self.observation.finish_pending();
    }
}
impl Write for Sink {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if !self.stream {
            self.raw.feed(bytes);
            self.json.feed(bytes);
            return Ok(bytes.len());
        }
        let mut offset = 0;
        let mut wire_start = 0;
        while offset < bytes.len() {
            let b = bytes[offset];
            if self.after_cr {
                self.after_cr = false;
                if b == b'\n' {
                    offset += 1;
                    self.raw_wire.feed(&bytes[wire_start..offset]);
                    wire_start = offset;
                    self.line_end();
                    continue;
                }
                self.raw_wire.feed(&bytes[wire_start..offset]);
                wire_start = offset;
                self.line_end();
            }
            if b == b'\r' {
                self.after_cr = true;
                offset += 1;
                continue;
            }
            if b == b'\n' {
                offset += 1;
                self.raw_wire.feed(&bytes[wire_start..offset]);
                wire_start = offset;
                self.line_end();
                continue;
            }
            match self.line_kind {
                0 => {
                    if b == b':' {
                        self.line_kind = match self.field.as_slice() {
                            b"data" => 1,
                            b"event" => 2,
                            _ => 3,
                        };
                        self.field.clear();
                        self.skip_space = true;
                    } else if self.field.len() < 32 {
                        self.field.push(b);
                    } else {
                        self.line_kind = 3;
                    }
                }
                1 => {
                    let skip = self.skip_space && b == b' ';
                    self.skip_space = false;
                    if !skip {
                        let end = bytes[offset..]
                            .iter()
                            .position(|b| matches!(b, b'\r' | b'\n'))
                            .map_or(bytes.len(), |n| offset + n);
                        self.data(&bytes[offset..end]);
                        offset = end;
                        continue;
                    }
                }
                2 if self.field.len() < 256 => self.field.push(b),
                _ => {}
            }
            offset += 1;
        }
        self.raw_wire.feed(&bytes[wire_start..]);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
enum Decoder {
    Plain(Sink),
    Brotli(Box<brotli::DecompressorWriter<Sink>>),
    Gzip(flate2::write::GzDecoder<Sink>),
    Deflate(flate2::write::ZlibDecoder<Sink>),
    Zstd(zstd::stream::write::Decoder<'static, Sink>),
}
pub struct Observer {
    decoder: Decoder,
    failed: bool,
    encoded: Option<crate::event_raw::Capture>,
}

/// Health observation and bounded usage metadata share decoding, never terminal policy.
#[derive(Default)]
pub struct Delivery {
    pub bytes: std::sync::atomic::AtomicU64,
    pub terminal: std::sync::atomic::AtomicBool,
    pub finished: std::sync::atomic::AtomicBool,
    // 1: body dropped; 2: HTTP connection error; 3: downstream stall.
    pub error: std::sync::atomic::AtomicU8,
    activity: std::sync::Mutex<Option<std::sync::Arc<super::wire_progress::Activity>>>,
    request_sequence: std::sync::atomic::AtomicU64,
    settled: std::sync::atomic::AtomicBool,
    settled_notify: tokio::sync::Notify,
}
impl Delivery {
    pub fn track(
        self: &std::sync::Arc<Self>,
        activity: std::sync::Arc<super::wire_progress::Activity>,
    ) {
        self.request_sequence.store(
            activity.begin_request(),
            std::sync::atomic::Ordering::Relaxed,
        );
        *super::locks::published(&self.activity) = Some(activity.clone());
        activity.track(self);
    }
    pub fn is_settled(&self) -> bool {
        self.settled.load(std::sync::atomic::Ordering::Acquire)
    }
    pub fn settle(&self) {
        self.settled
            .store(true, std::sync::atomic::Ordering::Release);
        self.settled_notify.notify_one();
    }
    pub fn failed(&self, code: u8) {
        if !self.is_settled() {
            let _ = self.error.compare_exchange(
                0,
                code,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Relaxed,
            );
            self.settle();
        }
    }
    pub async fn wait(&self) {
        if super::locks::published(&self.activity).is_none() {
            return;
        }
        while !self.is_settled() {
            self.settled_notify.notified().await;
        }
    }
    pub fn body_complete(&self) {
        if !self
            .finished
            .swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            self.event("response_body_complete");
        }
    }
    pub fn event(&self, kind: &str) {
        if let Some(activity) = super::locks::published(&self.activity).as_ref() {
            activity.event(kind);
        }
    }
    pub fn dropped(&self) {
        if self.finished.load(std::sync::atomic::Ordering::Acquire) {
            return;
        }
        let activity = super::locks::published(&self.activity);
        let code = activity.as_ref().map_or(0, |a| a.error_code());
        if let Some(activity) = activity.as_ref() {
            activity.event("response_body_cancelled");
        }
        let _ = self.error.compare_exchange(
            0,
            if code == 0 { 1 } else { code },
            std::sync::atomic::Ordering::AcqRel,
            std::sync::atomic::Ordering::Relaxed,
        );
        self.settle();
    }
    pub fn handed(&self, bytes: usize, terminal: bool) {
        use std::sync::atomic::Ordering::Relaxed;
        self.bytes.fetch_add(bytes as u64, Relaxed);
        if terminal && !self.terminal.swap(true, Relaxed) {
            self.event("terminal_handed_to_transport");
        }
    }
}
pub struct Protocol {
    business: Option<(super::Gateway, super::testing::BusinessIdentity)>,
    wait_budget: Option<(super::wait_budget::WaitBudget, Option<tokio::time::Instant>)>,
    traffic: Option<super::freeze::Ticket>,
    rpm_completion: Option<super::admission::Completion>,
    observer: Option<Observer>,
    thread: Option<std::sync::Arc<super::admission::ThreadRequest>>,
    transfer: crate::usage::model::TransferMetadata,
    delivery: std::sync::Arc<Delivery>,
    last_progress: std::time::Instant,

    pub observation: Observation,
    stream: bool,
    status: Option<u16>,
    finished: bool,
    transport_failure: bool,
    terminal_received: bool,
    ended_early: bool,
    usage: Option<crate::usage::AttemptTrace>,
    started: std::time::Instant,
    first_usage_ms: Option<u64>,
    ownership: Option<(std::sync::Arc<super::compaction::Lease>, String)>,
    compaction_operation: bool,
    routing_log: Option<std::sync::Arc<super::routing_log::RoutingLog>>,
    routing_decision: Option<crate::usage::model::RoutingDecision>,
    identity: Option<crate::usage::model::ThreadIdentity>,
    stop: Option<tokio::sync::watch::Receiver<bool>>,
    downstream: Option<tokio::sync::watch::Receiver<bool>>,
    diagnostics: Option<(
        crate::events::Service,
        super::ClientId,
        String,
        Option<String>,
    )>,
    end_reason: Option<String>,
    routing_fault: bool,
    routing_tail_failure: bool,
    fault_reports: std::sync::Mutex<FaultReports>,
    failover_generation: std::sync::atomic::AtomicU64,
}
impl Protocol {
    pub fn new(stream: bool) -> Self {
        Self {
            business: None,
            wait_budget: None,
            traffic: None,
            rpm_completion: None,
            observer: None,
            thread: None,
            transfer: crate::usage::model::TransferMetadata {
                sent_at: crate::usage::model::now(),
                ..Default::default()
            },
            delivery: Default::default(),
            last_progress: std::time::Instant::now(),
            observation: Observation::default(),
            stream,
            status: None,
            finished: false,
            transport_failure: false,
            terminal_received: false,
            ended_early: false,
            usage: None,
            started: std::time::Instant::now(),
            first_usage_ms: None,
            ownership: None,
            compaction_operation: false,
            routing_log: None,
            routing_decision: None,
            identity: None,
            stop: None,
            downstream: None,
            diagnostics: None,
            end_reason: None,
            routing_fault: false,
            routing_tail_failure: false,
            fault_reports: std::sync::Mutex::default(),
            failover_generation: std::sync::atomic::AtomicU64::new(0),
        }
    }
    pub(super) fn attach_failure_rules(&mut self, rules: &[super::failure_rules::Rule]) {
        self.observation.failure_rules = rules.to_vec();
        if let Some(observer) = &mut self.observer {
            observer.sink().observation.failure_rules = rules.to_vec();
        }
    }
    pub(super) fn attach_wait_budget(&mut self, budget: &super::wait_budget::WaitBudget) {
        self.wait_budget = Some((budget.clone(), budget.deadline()));
        self.traffic = budget.traffic();
    }
    pub(super) fn effective_output(&self) {
        if self.compaction_operation && !self.succeeded() {
            return;
        }
        if let Some((budget, deadline)) = &self.wait_budget {
            budget.complete(*deadline);
        }
    }
    pub(super) fn begin_compaction_execution(&self) {
        if self.compaction_operation {
            if let Some((budget, deadline)) = &self.wait_budget {
                budget.compaction_execution(*deadline, true);
            }
        }
    }
    fn end_compaction_execution(&self) {
        if self.compaction_operation {
            if let Some((budget, deadline)) = &self.wait_budget {
                budget.compaction_execution(*deadline, false);
            }
        }
    }
    pub(super) fn attach_rpm(&mut self, completion: Option<super::admission::Completion>) {
        self.rpm_completion = completion;
    }
    pub fn attach_thread(
        &mut self,
        thread: Option<std::sync::Arc<super::admission::ThreadRequest>>,
    ) {
        self.thread = thread;
    }
    pub fn diagnostics(&mut self, gateway: &super::Gateway, provider: &str, model: Option<&str>) {
        self.diagnostics = super::locks::published(&gateway.0.diagnostics)
            .clone()
            .map(|service| {
                (
                    service,
                    gateway.client_id(),
                    provider.into(),
                    model.map(str::to_owned),
                )
            });
    }
    pub fn downstream_closed(&mut self, activity: &super::wire_progress::Activity, writing: bool) {
        super::attempt::DownstreamEnd::PeerClosed {
            error: activity.error_code(),
            writing,
        }
        .record(&self.delivery(), self);
    }
    pub fn begin_usage_tail(&mut self, strict_identity: bool) {
        // CC Switch settles events already seen on cancellation. Here the
        // transport additionally permits a fixed, bounded usage-only tail.
        self.observation.usage_tail = true;
        self.observation.strict_tail_identity = strict_identity;
        if let Some(observer) = &mut self.observer {
            observer.sink().observation.usage_tail = true;
            observer.sink().observation.strict_tail_identity = strict_identity;
        }
    }
    pub(super) fn stop_signal(&self) -> Option<tokio::sync::watch::Receiver<bool>> {
        self.stop.clone()
    }
    pub fn finish_usage_tail(&mut self, upstream_end: &str) {
        self.routing_tail_failure |=
            matches!(upstream_end, "upstream_read_failed" | "gateway_stopping");
        self.finish(None, code::CLIENT_DISCONNECTED);
        self.delivery.event(upstream_end);
        match upstream_end {
            "upstream_eof" => {
                self.transfer.upstream_eof = true;
                self.transfer.upstream_end = Some("EOF".into());
            }
            "upstream_read_failed" => {
                self.transfer.upstream_end = Some("DISCONNECTED".into());
            }
            _ => (),
        }
        self.record_end(code::CLIENT_DISCONNECTED);
    }
    pub fn request_received(&mut self, at: i64) {
        self.transfer.received_at = Some(at);
    }
    pub fn websocket_close(&mut self, code: Option<u16>) {
        self.transfer.ws_close_code = code;
    }
    pub fn delivery(&self) -> std::sync::Arc<Delivery> {
        self.delivery.clone()
    }
    pub fn set_delivery(&mut self, delivery: std::sync::Arc<Delivery>) {
        self.delivery = delivery;
    }
    pub fn thread_identity(&mut self, identity: crate::usage::model::ThreadIdentity) {
        self.identity = Some(identity);
    }
    pub fn business_request(
        &mut self,
        gateway: &super::Gateway,
        _provider: &str,
        headers: &hyper::HeaderMap,
    ) {
        self.business = gateway
            .business_identity(headers)
            .map(|identity| (gateway.clone(), identity));
    }
    pub fn request_headers(&mut self, headers: &hyper::HeaderMap) {
        self.identity = Some(super::compaction::identity(headers).1);
        self.transfer.client_request_hash = ["x-request-id", "request-id", "x-client-request-id"]
            .iter()
            .find_map(|key| headers.get(*key))
            .filter(|v| !v.is_empty() && v.len() <= 512)
            .map(|v| crate::storage::digest(v.as_bytes()));
    }
    pub fn timeout(&mut self, phase: &str, seconds: u64) {
        self.timeout_after(phase, seconds, self.last_progress.elapsed());
    }
    pub fn timeout_after(&mut self, phase: &str, seconds: u64, silent: std::time::Duration) {
        self.routing_fault = true;
        self.transfer.timeout_phase = Some(phase.into());
        self.transfer.timeout_seconds = Some(seconds);
        self.transfer.last_progress_ms = Some(silent.as_millis().min(u64::MAX as u128) as u64);
        self.observation.error.timeout_phase = self.transfer.timeout_phase.clone();
        self.observation.error.timeout_seconds = self.transfer.timeout_seconds;
        self.observation.error.last_progress_ms = self.transfer.last_progress_ms;
    }
    fn progress(&mut self, bytes: usize) {
        if bytes == 0 {
            return;
        }
        self.transfer
            .first_byte_at
            .get_or_insert_with(crate::usage::model::now);
        self.transfer.upstream_bytes = self.transfer.upstream_bytes.saturating_add(bytes as u64);
        self.last_progress = std::time::Instant::now();
    }
    pub fn websocket_progress(&mut self, bytes: usize) {
        self.progress(bytes);
    }
    pub fn attach_usage(&mut self, mut usage: crate::usage::AttemptTrace) {
        self.observation.meter.anthropic = usage.anthropic();
        if let Some((lease, _)) = &self.ownership {
            lease.attach_usage(usage.routing_completion());
        }
        self.usage = Some(usage);
    }
    pub fn request_speed(&mut self, speed: Option<String>) {
        self.observation.meter.requested_speed = speed;
    }
    pub fn context(
        &mut self,
        thread: Option<String>,
        decision: Option<crate::usage::model::RoutingDecision>,
    ) {
        self.routing_decision = decision;
        if let Some(usage) = &mut self.usage {
            usage.context(thread, decision);
        }
    }
    pub fn response_headers(&mut self, headers: &hyper::HeaderMap) {
        self.observation.limit_headers = super::failure_policy::limit_headers(headers);
        self.observation.raw_headers = crate::event_raw::headers(headers);
        if let Some(observer) = &mut self.observer {
            observer.sink().observation.limit_headers = self.observation.limit_headers.clone();
            observer.sink().observation.raw_headers = self.observation.raw_headers.clone();
        }
        self.transfer.upstream_client_request_hash = headers
            .get("x-client-request-id")
            .filter(|v| !v.is_empty() && v.len() <= 512)
            .map(|v| crate::storage::digest(v.as_bytes()));
        if let Some(usage) = &mut self.usage {
            usage.upstream_request(headers);
        }
    }
    pub fn routing_evidence(&mut self, hints: Option<&super::replay::RequestHints>) {
        self.compaction_operation |= hints.is_some_and(|h| h.compaction_trigger);
        let mut evidence = hints
            .map(|h| h.routing_evidence.clone())
            .unwrap_or_default();
        if self.compaction_operation {
            evidence.input = crate::usage::model::RoutingInput::Compaction;
        }
        evidence.identity = self.identity;
        evidence.boundary = self.ownership.as_ref().map(|(lease, _)| {
            lease.boundary_state(hints.and_then(|h| h.compacted_window.as_deref()))
        });
        use crate::usage::model::{RoutingDecision as D, RoutingRestriction as R};
        evidence.restriction = match self.routing_decision {
            Some(D::NewThreadModelDenied | D::ModelDetourOwnerPreserved) => Some(R::ModelMismatch),
            Some(D::NewThreadModelCooling) => Some(R::ModelCooling),
            Some(D::NewThreadProviderUnavailable) => Some(R::ProviderUnavailable),
            Some(D::NewThreadCapacityFull | D::ReceiverCapacityFull) => Some(R::CapacityFull),
            Some(D::NewThreadQueueWaiting) => Some(R::QueueWaiting),
            _ => None,
        };
        if let (Some(log), Some(decision), Some((_, _, provider, model))) =
            (&self.routing_log, self.routing_decision, &self.diagnostics)
        {
            log.context(
                provider,
                model.as_deref(),
                decision,
                evidence.clone(),
                self.usage.as_ref().map(|usage| usage.id()),
            );
        }
        if let Some(usage) = &mut self.usage {
            usage.routing_evidence(evidence);
        }
    }
    pub fn attach_routing_log(
        &mut self,
        log: Option<std::sync::Arc<super::routing_log::RoutingLog>>,
    ) {
        if let (Some(log), Some((lease, _))) = (&log, &self.ownership) {
            lease.attach_routing_log(log.clone());
        }
        self.routing_log = log;
    }
    pub fn watch_lifecycle(
        &mut self,
        stop: Option<tokio::sync::watch::Receiver<bool>>,
        downstream: Option<tokio::sync::watch::Receiver<bool>>,
    ) {
        self.stop = stop;
        self.downstream = downstream;
    }
    pub fn attach_ownership(
        &mut self,
        lease: Option<std::sync::Arc<super::compaction::Lease>>,
        provider: &str,
    ) {
        self.ownership = lease.map(|lease| (lease, provider.into()));
        if let (Some((lease, _)), Some(usage)) = (&self.ownership, &mut self.usage) {
            lease.attach_usage(usage.routing_completion());
        }
    }
    pub fn search_content_observed(&self) -> bool {
        self.observation.search_content
            || self.observation.opaque_output
            || self.observation.meter.first_token_ms.is_some()
            || self.succeeded()
    }
    pub fn explicit_failure(&self) -> bool {
        self.observation.fault_sequence > 0
            && matches!(
                self.terminal(),
                Some(Terminal::Failure | Terminal::Rejected | Terminal::Unknown)
            )
    }
    pub fn business_observed(&self) -> bool {
        !self.stream
            || self.observation.meter.first_token_ms.is_some()
            || self.observation.search_content
            || self.observation.opaque_output
            || self.terminal_received
            || self.observation.incomplete
    }
    pub fn set_operation(&mut self, operation: crate::usage::model::Operation) {
        self.compaction_operation = operation == crate::usage::model::Operation::Compaction;
    }
    pub fn output_handed(&self, business: bool) {
        if business {
            if let Some(traffic) = &self.traffic {
                traffic.streaming();
            }
        }
        if business && !self.compaction_operation {
            if let Some((lease, provider)) = &self.ownership {
                lease.output_started(provider);
            }
        }
        self.commit_ownership();
    }
    pub fn commit_ownership(&self) {
        if self.terminal() == Some(Terminal::Success) {
            if let Some((lease, provider)) = &self.ownership {
                if self
                    .delivery
                    .terminal
                    .load(std::sync::atomic::Ordering::Relaxed)
                {
                    if let Some(log) = &self.routing_log {
                        log.finish(self.status, true);
                    }
                    lease.complete(provider, self.observation.compaction_fingerprint.as_deref());
                }
            }
        }
    }
    fn observe_routing_fault(&self) {
        let mut error = self.observation.error.clone();
        // Progress is not another cause. All retained fields here describe the
        // bounded protocol error, never the forwarded body or an old attempt.
        error.last_progress_ms = None;
        let has_error = error.upstream_code.is_some()
            || error.upstream_reason.is_some()
            || error.upstream_type.is_some()
            || error.local_code.is_some()
            || error.message.is_some()
            || error.timeout_phase.is_some();
        let status = self.status.filter(|status| *status >= 400);
        let unspecified = ((self.terminal_received
            && matches!(
                self.terminal(),
                Some(
                    Terminal::Failure
                        | Terminal::Rejected
                        | Terminal::ModelUnavailable
                        | Terminal::Unknown
                )
            ))
            || (self.routing_fault
                && !self.succeeded()
                && self.terminal() != Some(Terminal::Cancelled)))
            && !has_error
            && status.is_none()
            && !self.observation.incomplete
            && !self.transport_failure;
        if has_error
            || status.is_some()
            || self.observation.incomplete
            || self.transport_failure
            || unspecified
            || self.routing_tail_failure
        {
            if let Ok(mut reports) = self.fault_reports.lock() {
                reports.observe(RoutingFault {
                    protocol_sequence: self.observation.fault_sequence,
                    error,
                    status,
                    incomplete: self.observation.incomplete,
                    transport: self.transport_failure,
                    unspecified,
                    tail_failure: self.routing_tail_failure,
                });
            }
        }
    }
    pub(super) fn associate_failure(&self, generation: u64) {
        self.failover_generation
            .store(generation, std::sync::atomic::Ordering::Relaxed);
    }
    /// Synchronously bind a newly emitted diagnostic to the fault observed by
    /// this Protocol instance. A subsequent fault gets a new generation.
    pub(super) fn report_failure(
        &self,
        emit: impl FnOnce() -> super::health_settlement::Reported,
    ) -> super::health_settlement::Reported {
        self.observe_routing_fault();
        let generation = self.fault_reports.lock().ok().map(|state| state.generation);
        let report = emit();
        if report.queued {
            if let Ok(mut state) = self.fault_reports.lock() {
                if generation == Some(state.generation) {
                    state.reported = state.generation;
                }
            }
            if let Some(log) = &self.routing_log {
                log.reported(
                    self.failover_generation
                        .load(std::sync::atomic::Ordering::Relaxed),
                );
            }
        }
        report
    }
    pub fn delivered_success(&self) -> bool {
        self.terminal() == Some(Terminal::Success)
            && self
                .delivery
                .terminal
                .load(std::sync::atomic::Ordering::Relaxed)
    }
    fn record_usage(&mut self) {
        // Diagnostic retention is sticky across observations. A later success
        // or downstream close must not hide an earlier upstream/parse fault.
        let error = &self.observation.error;
        self.routing_fault |= self.observation.incomplete
            || error.upstream_code.is_some()
            || error.upstream_reason.is_some()
            || error.upstream_type.is_some()
            || error.local_code.is_some()
            || error.message.is_some()
            || (self.terminal_received
                && !matches!(self.terminal(), Some(Terminal::Success | Terminal::Limited)));
        self.observe_routing_fault();
        // Count upstream completion at observation time, independently of a
        // slow or closed downstream socket. The token can settle only once.
        if self.terminal_received {
            let result = match self.terminal() {
                Some(Terminal::Success | Terminal::Limited) => {
                    Some(super::rpm::Settlement::Success)
                }
                Some(Terminal::Failure | Terminal::Rejected | Terminal::ModelUnavailable) => {
                    Some(super::rpm::Settlement::Failed)
                }
                _ => None,
            };
            if let Some(result) = result {
                if let Some(mut completion) = self.rpm_completion.take() {
                    completion.settle(result);
                }
            }
        }
        if self.terminal_received {
            if self.transfer.terminal_at.is_none() {
                self.delivery.event("upstream_terminal_observed");
            }
            self.transfer
                .terminal_at
                .get_or_insert_with(crate::usage::model::now);
        }
        if self.observation.meter.first_token_ms.is_some() || self.succeeded() {
            self.effective_output();
        }
        if self.observation.meter.first_token_ms.is_some() && self.first_usage_ms.is_none() {
            self.first_usage_ms = Some(self.started.elapsed().as_millis() as u64);
        }
        let mut meter = self.observation.meter.clone();
        meter.parse_incomplete |= self.observation.incomplete;
        meter.ended_early = self.ended_early
            || self.transport_failure
            || self.terminal() == Some(Terminal::Cancelled);
        meter.first_token_ms = self.first_usage_ms;
        let outcome = self.terminal().map(|t| match t {
            Terminal::Success => "success",
            Terminal::Limited => "limited",
            Terminal::Rejected => "rejected",
            Terminal::ModelUnavailable => "model_unavailable",
            Terminal::Cancelled => "cancelled",
            Terminal::Failure => "failure",
            Terminal::Unknown => "unknown",
        });
        if let Some(usage) = &mut self.usage {
            usage.update(&meter, self.status, outcome);
        }
    }
    pub fn websocket_status(&mut self, status: u16) {
        self.status = Some(status);
    }
    pub fn response(&mut self, status: u16, stream: bool, encoding: &str) {
        self.status = Some(status);
        self.stream = stream;
        let mut observer = Observer::new(stream, encoding);
        observer.sink().observation.http_status = Some(status);
        observer.sink().observation.raw_headers = self.observation.raw_headers.clone();
        observer.sink().observation.limit_headers = self.observation.limit_headers.clone();
        observer.sink().observation.failure_rules = self.observation.failure_rules.clone();
        observer.sink().observation.meter.anthropic = self.observation.meter.anthropic;
        observer.sink().observation.meter.requested_speed =
            self.observation.meter.requested_speed.clone();
        self.observer = Some(observer);
    }
    pub fn feed(&mut self, bytes: &[u8]) {
        self.progress(bytes.len());
        if let Some(o) = &mut self.observer {
            o.feed(bytes);
            self.observation = o.snapshot(false);
        }
        self.terminal_received |= self.observation.terminal.is_some();
        self.record_usage();
    }
    pub fn possible_bare_capacity(&mut self) -> bool {
        let Some(observer) = &mut self.observer else {
            return false;
        };
        let prefix = observer.sink().raw.prefix();
        let Ok(text) = std::str::from_utf8(prefix) else {
            return false;
        };
        let text = text
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase();
        let expected = "selected model is at capacity. please try a different model.";
        expected.starts_with(&text) || super::failure_policy::bare_capacity(&text)
    }
    #[cfg(test)]
    pub fn value(&mut self, value: &Value) {
        self.observation.value(value);
        self.terminal_received |= self.observation.terminal.is_some();
        self.record_usage();
    }
    pub fn value_raw(&mut self, value: &Value, bytes: &[u8]) {
        let sequence = self.observation.fault_sequence;
        self.observation.value_projected(value, false);
        if self.observation.fault_sequence != sequence {
            self.observation.error.raw = Some(std::sync::Arc::new(crate::event_raw::Raw::bytes(
                bytes,
                self.observation.raw_headers.clone(),
                None,
            )));
        }
        self.terminal_received |= self.observation.terminal.is_some();
        self.record_usage();
    }
    pub fn terminal(&self) -> Option<Terminal> {
        if self
            .wait_budget
            .as_ref()
            .is_some_and(|(budget, deadline)| budget.is_cancelled(*deadline))
        {
            return Some(Terminal::Cancelled);
        }
        if self.status.is_some_and(|s| s >= 400) {
            return Some(Terminal::Rejected);
        }
        self.observation.terminal
    }
    pub fn terminal_observed(&self) -> bool {
        self.terminal_received
    }
    pub fn succeeded(&self) -> bool {
        matches!(self.terminal(), Some(Terminal::Success | Terminal::Limited))
    }
    /// True only for a transport-level failure. Application/protocol errors
    /// received in an otherwise successful HTTP response must not trip a
    /// provider circuit.
    pub fn transport_failure(&self) -> bool {
        self.transport_failure
    }
    pub fn finish(&mut self, status: Option<u16>, reason: &str) {
        self.end_compaction_execution();
        let reason = if self
            .wait_budget
            .as_ref()
            .is_some_and(|(budget, deadline)| budget.is_cancelled(*deadline))
        {
            code::CANCELLED
        } else {
            reason
        };
        self.routing_fault |= !matches!(reason, code::OK | code::CLIENT_DISCONNECTED | "UNSETTLED");
        if self.finished {
            if matches!(
                reason,
                code::CLIENT_DISCONNECTED | code::GATEWAY_STOPPED | code::CANCELLED
            ) {
                self.record_end(reason);
            }
            return;
        }
        self.finished = true;
        if self.transfer.last_progress_ms.is_none() && self.transfer.first_byte_at.is_some() {
            self.transfer.last_progress_ms = Some(
                self.last_progress
                    .elapsed()
                    .as_millis()
                    .min(u64::MAX as u128) as u64,
            );
        }
        self.delivery.event(match reason {
            code::OK => "upstream_eof",
            code::STREAM_INTERRUPTED => "upstream_read_failed",
            code::STREAM_TIMEOUT | code::FIRST_BYTE_TIMEOUT => "upstream_timeout",
            code::GATEWAY_STOPPED => "gateway_stopping",
            code::CANCELLED => "request_cancelled",
            code::CLIENT_DISCONNECTED => "response_consumer_closed",
            _ => "upstream_observation_ended",
        });
        self.transfer.upstream_eof = reason == code::OK;
        self.transfer.upstream_end = match reason {
            code::OK => Some("EOF".into()),
            code::STREAM_INTERRUPTED => Some("DISCONNECTED".into()),
            code::STREAM_TIMEOUT | code::FIRST_BYTE_TIMEOUT => Some("TIMEOUT".into()),
            code::NETWORK | code::TLS => Some("CONNECT_FAILED".into()),
            _ => None,
        };
        self.status = self.status.or(status);
        if let Some(o) = &mut self.observer {
            self.observation = o.snapshot(true);
        }
        if matches!(
            reason,
            code::STREAM_INTERRUPTED
                | code::STREAM_TIMEOUT
                | code::FIRST_BYTE_TIMEOUT
                | code::CANCELLED
                | code::GATEWAY_STOPPED
        ) {
            if let Some(raw) = &mut self.observation.error.raw {
                std::sync::Arc::make_mut(raw).incomplete =
                    Some("上游响应未收完整，保存已收到的错误内容".into());
            }
        }
        self.observation.finish_pending();
        self.observation.error.timeout_phase = self.transfer.timeout_phase.clone();
        self.observation.error.timeout_seconds = self.transfer.timeout_seconds;
        self.observation.error.last_progress_ms = self.transfer.last_progress_ms;
        self.terminal_received |= self.observation.terminal.is_some();
        self.ended_early = !self.terminal_received
            && matches!(
                reason,
                code::CLIENT_DISCONNECTED | code::GATEWAY_STOPPED | code::CANCELLED | "UNSETTLED"
            );
        self.transport_failure = self.observation.terminal.is_none()
            && matches!(
                reason,
                code::NETWORK
                    | code::TLS
                    | code::FIRST_BYTE_TIMEOUT
                    | code::STREAM_TIMEOUT
                    | code::STREAM_INTERRUPTED
            );
        if self.transport_failure {
            self.observation.error.local_code = Some(
                match reason {
                    code::TLS => code::TLS_HANDSHAKE_FAILED,
                    code::NETWORK => code::CONNECTION_FAILED,
                    other => other,
                }
                .into(),
            );
        }
        if self.observation.terminal.is_none() {
            if reason == code::OK
                && self.stream
                && self.observation.expects_terminal
                && !self.observation.incomplete
            {
                // EOF before a terminal event is a transport/incomplete-stream
                // failure, unlike an explicit response.failed event.
                self.transport_failure = true;
                self.observation.error.local_code = Some(code::STREAM_INTERRUPTED.into());
            }
            self.observation.terminal = Some(match reason {
                code::OK if self.stream && self.observation.expects_terminal => {
                    if self.observation.incomplete {
                        Terminal::Unknown
                    } else {
                        Terminal::Failure
                    }
                }
                code::OK => Terminal::Success,
                code::HTTP | code::CLIENT_ERROR => Terminal::Rejected,
                code::CANCELLED | code::GATEWAY_STOPPED => Terminal::Cancelled,
                code::CLIENT_DISCONNECTED | "UNSETTLED" => Terminal::Unknown,
                _ => Terminal::Failure,
            });
        }
        if let Some(mut completion) = self.rpm_completion.take() {
            use super::rpm::Settlement;
            let result = if self.succeeded() {
                Settlement::Success
            } else if self.status.is_some_and(|s| s >= 400)
                || (self.terminal_received
                    && matches!(
                        self.terminal(),
                        Some(Terminal::Failure | Terminal::Rejected | Terminal::ModelUnavailable)
                    ))
            {
                Settlement::Failed
            } else {
                Settlement::Uncertain
            };
            completion.settle(result);
        }
        self.record_usage();
        self.record_end(reason);
    }
    fn record_end(&mut self, reason: &str) {
        use crate::usage::model::{EndCause, EndMetadata};
        if !matches!(
            self.end_reason.as_deref(),
            Some(code::CANCELLED | code::GATEWAY_STOPPED)
        ) {
            self.end_reason = Some(reason.into());
        }
        let cause = match reason {
            code::CANCELLED
                if self
                    .wait_budget
                    .as_ref()
                    .is_some_and(|(budget, deadline)| budget.is_cancelled(*deadline)) =>
            {
                EndCause::ClientCancelled
            }
            _ if self.terminal_received => match self.observation.terminal {
                Some(Terminal::Success | Terminal::Limited) => EndCause::Completed,
                Some(Terminal::Cancelled) => EndCause::ClientCancelled,
                Some(Terminal::Unknown) => EndCause::UpstreamIncomplete,
                _ => EndCause::ApplicationError,
            },
            code::CLIENT_DISCONNECTED => match self
                .delivery
                .error
                .load(std::sync::atomic::Ordering::Acquire)
            {
                1 => EndCause::Unsettled,
                3 => EndCause::DownstreamWriteTimeout,
                2 | 4 | 7 | 8 => EndCause::DownstreamTransportError,
                _ => EndCause::ClientDisconnected,
            },
            code::GATEWAY_STOPPED => EndCause::GatewayStopped,
            code::CANCELLED => EndCause::ClientCancelled,
            _ if self.status.is_some_and(|s| s >= 400) => EndCause::UpstreamRejected,
            code::FIRST_BYTE_TIMEOUT => EndCause::FirstByteTimeout,
            code::STREAM_TIMEOUT => EndCause::StreamTimeout,
            code::STREAM_INTERRUPTED => EndCause::UpstreamDisconnected,
            code::NETWORK | code::TLS => EndCause::ConnectFailed,
            "UNSETTLED" => EndCause::Unsettled,
            _ if self.observation.incomplete => EndCause::ParseIncomplete,
            code::OK if self.succeeded() => EndCause::Completed,
            code::OK => EndCause::UpstreamDisconnected,
            _ => EndCause::Unsettled,
        };
        if reason == code::CLIENT_DISCONNECTED {
            self.transfer.downstream_error = Some(code::CLIENT_DISCONNECTED.into());
        }
        self.snapshot_delivery();
        if let Some(usage) = &mut self.usage {
            usage.transfer(self.transfer.clone());
            usage.end(EndMetadata {
                cause,
                terminal_received: self.terminal_received,
                response_started: self.status.is_some(),
            });
        }
    }
    fn snapshot_delivery(&mut self) {
        use std::sync::atomic::Ordering::Relaxed;
        if self.transfer.timeout_phase.is_none() && self.transfer.first_byte_at.is_some() {
            self.transfer.last_progress_ms = Some(
                self.last_progress
                    .elapsed()
                    .as_millis()
                    .min(u64::MAX as u128) as u64,
            );
        }
        self.transfer.ended_at = crate::usage::model::now();
        self.transfer.downstream_bytes = self.delivery.bytes.load(Relaxed);
        self.transfer.terminal_handed_off = self.delivery.terminal.load(Relaxed);
        self.transfer.terminal_event = self.observation.terminal_event.clone();
        self.transfer.terminal_status = self.observation.terminal_status.clone();
        self.transfer.incomplete_reason = self.observation.incomplete_reason.clone();
        self.transfer.connection = super::locks::published(&self.delivery.activity)
            .as_ref()
            .map(|activity| activity.snapshot(self.delivery.request_sequence.load(Relaxed)));
        self.transfer.response_hash = self
            .observation
            .response_id
            .as_ref()
            .map(|v| crate::storage::digest(v.as_bytes()));
        self.transfer.downstream_error = match self.delivery.error.load(Relaxed) {
            2 => Some("DOWNSTREAM_CONNECTION_ERROR".into()),
            4 => Some("DOWNSTREAM_CANCELLED".into()),
            5 => Some("DOWNSTREAM_CLOSED".into()),
            6 => Some("DOWNSTREAM_PEER_RESET".into()),
            7 => Some("DOWNSTREAM_WRITE_FAILED".into()),
            8 => Some("DOWNSTREAM_PROTOCOL_ERROR".into()),
            3 => Some("DOWNSTREAM_WRITE_TIMEOUT".into()),
            1 => Some("RESPONSE_BODY_DROPPED".into()),
            _ => self.transfer.downstream_error.clone(),
        };
    }
    fn emit_interruption(&self) -> bool {
        use crate::events::{Action, Details, HealthScope, Level, Phase, Reason, Record};
        if self.downstream_close().is_some() {
            return false;
        }
        let Some((service, client, provider, model)) = &self.diagnostics else {
            return false;
        };
        let (local, message, status_only) = match self.end_reason.as_deref() {
            Some(code::GATEWAY_STOPPED) => (code::GATEWAY_STOPPED, "网关停止，已结束转发", true),
            Some(code::CANCELLED) => (code::CANCELLED, "收到明确取消指令，已结束转发", true),
            _ => match self.transfer.downstream_error.as_deref() {
                Some("DOWNSTREAM_PROTOCOL_ERROR") => {
                    ("DOWNSTREAM_PROTOCOL_ERROR", "下游协议处理失败", false)
                }
                Some("DOWNSTREAM_WRITE_TIMEOUT") => (
                    "DOWNSTREAM_WRITE_TIMEOUT",
                    "下游持续未接收数据，写入超时",
                    false,
                ),
                Some("DOWNSTREAM_CONNECTION_ERROR") => (
                    "DOWNSTREAM_CONNECTION_ERROR",
                    "下游连接异常，关闭原因未确认",
                    false,
                ),
                Some("RESPONSE_BODY_DROPPED") => (
                    "RESPONSE_BODY_DROPPED",
                    "响应体被释放，关闭原因未确认",
                    false,
                ),
                Some(code::CLIENT_DISCONNECTED) => (
                    code::CLIENT_DISCONNECTED,
                    "下游响应消费者结束，关闭原因未确认",
                    false,
                ),
                _ => return false,
            },
        };
        // Closing an idle, successfully delivered WebSocket turn is expected.
        if self.status == Some(101)
            && self.succeeded()
            && self.transfer.terminal_handed_off
            && matches!(local, "DOWNSTREAM_CLOSED" | code::CLIENT_DISCONNECTED)
        {
            return false;
        }
        let mut record = Record::new(
            Some(*client),
            Some(provider),
            model.as_deref(),
            Reason::RequestInterrupted,
            if self.transfer.downstream_bytes > 0 {
                Action::NotRetried
            } else {
                Action::Returned
            },
            self.status,
            None,
        );
        record.level = if status_only {
            Level::Info
        } else {
            Level::Warning
        };
        record.details = Details {
            health_scope: Some(HealthScope::Request),
            local_code: Some(local.into()),
            message: Some(message.into()),
            phase: Some(if self.status == Some(101) {
                Phase::WsSend
            } else {
                Phase::Stream
            }),
            counted_failure: Some(false),
            delivery: Some(crate::events::DeliveryEvidence {
                duration_ms: self.started.elapsed().as_millis().min(u64::MAX as u128) as u64,
                upstream_bytes: self.transfer.upstream_bytes,
                handed_bytes: self.transfer.downstream_bytes,
                terminal_received: self.terminal_received,
                terminal_handed_off: self.transfer.terminal_handed_off,
                request_hash: self.transfer.client_request_hash.clone(),
                response_hash: self.transfer.response_hash.clone(),
            }),
            timeout_phase: self.transfer.timeout_phase.clone(),
            timeout_seconds: self.transfer.timeout_seconds,
            last_progress_ms: self.transfer.last_progress_ms,
            ..Default::default()
        };
        service.emit_checked(record)
    }
    fn downstream_close(&self) -> Option<DownstreamClose> {
        if matches!(
            self.end_reason.as_deref(),
            Some(code::CANCELLED | code::GATEWAY_STOPPED)
        ) {
            return None;
        }
        match self.transfer.downstream_error.as_deref() {
            Some("DOWNSTREAM_CLOSED") => Some(DownstreamClose::Closed),
            Some("DOWNSTREAM_PEER_RESET") => Some(DownstreamClose::Reset),
            Some("DOWNSTREAM_WRITE_FAILED") => Some(DownstreamClose::BrokenPipe),
            Some("DOWNSTREAM_CANCELLED") => Some(DownstreamClose::Cancelled),
            _ => None,
        }
    }
    fn routing_settlement(&self, interruption_reported: bool) -> RoutingSettlement {
        let Ok(reports) = self.fault_reports.lock() else {
            return RoutingSettlement::Failed;
        };
        let upstream_fault = reports.observed.is_some();
        let interruption = (self.transfer.downstream_error.is_some()
            && self.downstream_close().is_none())
            || matches!(
                self.end_reason.as_deref(),
                Some(code::CANCELLED | code::GATEWAY_STOPPED)
            );
        if upstream_fault || interruption {
            if (!upstream_fault || reports.covered()) && (!interruption || interruption_reported) {
                RoutingSettlement::ReportedFailure
            } else {
                RoutingSettlement::Failed
            }
        } else if self.succeeded()
            && self
                .delivery
                .terminal
                .load(std::sync::atomic::Ordering::Relaxed)
        {
            RoutingSettlement::Delivered
        } else if self.downstream_close().is_some() {
            RoutingSettlement::DownstreamClosed
        } else {
            RoutingSettlement::Failed
        }
    }
}
impl Drop for Protocol {
    fn drop(&mut self) {
        if let Some((budget, deadline)) = &self.wait_budget {
            if budget.is_cancelled(*deadline) && !self.finished {
                self.finish(None, code::CANCELLED);
            } else if budget.expired(*deadline) && !self.finished {
                self.timeout("retry_budget", budget.seconds);
                self.finish(None, code::FIRST_BYTE_TIMEOUT);
            }
        }
        // Only the transport's live cancellation branch performs a bounded
        // tail. Drop itself flushes observed bytes without starting any I/O.
        let stopped = self.stop.as_ref().is_some_and(|s| *s.borrow());
        let reason = if self.observation.usage_tail {
            if stopped {
                self.delivery.event("gateway_stopping");
            }
            code::CLIENT_DISCONNECTED
        } else if stopped {
            code::GATEWAY_STOPPED
        } else if self.downstream.as_ref().is_some_and(|s| *s.borrow())
            || matches!(
                self.delivery
                    .error
                    .load(std::sync::atomic::Ordering::Acquire),
                2..=8
            )
        {
            // Hyper may drop the request future before response headers exist.
            // Retain an observed socket failure; body cancellation alone (1)
            // still cannot establish why the client stopped reading.
            code::CLIENT_DISCONNECTED
        } else {
            "UNSETTLED"
        };
        self.finish(None, reason);
        self.snapshot_delivery();
        self.observe_routing_fault();
        let interruption_reported = self.emit_interruption();
        let settlement = self.routing_settlement(interruption_reported);
        if self.delivered_success() && !self.routing_fault {
            if let Some((gateway, identity)) = self.business.take() {
                gateway.remember_business_identity(identity);
            }
        }
        if let Some(log) = &self.routing_log {
            log.settle(self.status, settlement);
        }
        if let Some(usage) = &mut self.usage {
            usage.transfer(self.transfer.clone());
            if self.routing_decision.is_some() {
                usage
                    .routing_completion()
                    .finish(if settlement == RoutingSettlement::Delivered {
                        crate::events::RoutingOutcome::OwnerPreserved
                    } else {
                        crate::events::RoutingOutcome::Failed
                    });
            }
        }
        if let Some((lease, _)) = &self.ownership {
            lease.finish_round(
                self.usage.as_ref().map(|usage| usage.id()),
                self.routing_log.as_ref(),
            );
        }
    }
}
impl Observer {
    pub fn new(stream: bool, encoding: &str) -> Self {
        let sink = Sink::new(stream);
        let mut failed = false;
        let decoder = match encoding.trim().to_ascii_lowercase().as_str() {
            "" | "identity" => Decoder::Plain(sink),
            "br" => Decoder::Brotli(Box::new(brotli::DecompressorWriter::new(sink, 4096))),
            "gzip" => Decoder::Gzip(flate2::write::GzDecoder::new(sink)),
            "deflate" => Decoder::Deflate(flate2::write::ZlibDecoder::new(sink)),
            "zstd" => Decoder::Zstd(zstd::stream::write::Decoder::new(sink).expect("zstd decoder")),
            _ => {
                failed = true;
                Decoder::Plain(sink)
            }
        };
        Self {
            decoder,
            failed,
            encoded: (!matches!(
                encoding.trim().to_ascii_lowercase().as_str(),
                "" | "identity"
            ))
            .then(crate::event_raw::Capture::default),
        }
    }
    fn sink(&mut self) -> &mut Sink {
        match &mut self.decoder {
            Decoder::Plain(s) => s,
            Decoder::Brotli(d) => d.get_mut(),
            Decoder::Gzip(d) => d.get_mut(),
            Decoder::Deflate(d) => d.get_mut(),
            Decoder::Zstd(d) => d.get_mut(),
        }
    }
    pub fn feed(&mut self, bytes: &[u8]) {
        if let Some(encoded) = &mut self.encoded {
            encoded.feed(bytes);
        }
        if self.failed {
            return;
        }
        let result = match &mut self.decoder {
            Decoder::Plain(s) => s.write_all(bytes),
            Decoder::Brotli(d) => d.write_all(bytes),
            Decoder::Gzip(d) => d.write_all(bytes),
            Decoder::Deflate(d) => d.write_all(bytes),
            Decoder::Zstd(d) => d.write_all(bytes),
        };
        if result.is_err() {
            self.failed = true;
        }
    }
    pub fn snapshot(&mut self, finish: bool) -> Observation {
        if finish {
            let result = match &mut self.decoder {
                Decoder::Plain(_) => Ok(()),
                Decoder::Brotli(d) => d.close(),
                Decoder::Gzip(d) => d.try_finish(),
                Decoder::Deflate(d) => d.try_finish(),
                Decoder::Zstd(d) => d.flush(),
            };
            if result.is_err() {
                self.failed = true;
            }
            self.sink().finish();
        }
        let mut result = self.sink().observation.clone();
        result.incomplete |= self.failed;
        if self.failed {
            if finish {
                if let Some(encoded) = self.encoded.take() {
                    let mut raw = encoded.finish(self.sink().observation.raw_headers.clone(), None);
                    raw.incomplete = Some("响应解码失败，保存已收到的原始响应".into());
                    result.error.raw = Some(std::sync::Arc::new(raw));
                }
            }
            if let Some(raw) = &mut result.error.raw {
                std::sync::Arc::make_mut(raw).incomplete =
                    Some("响应解码失败，错误原文可能不完整".into());
            }
        }
        if finish {
            self.encoded = None;
        }
        result
    }
}

#[cfg(test)]
#[path = "protocol_poison_tests.rs"]
mod poison_tests;

fn gemini_terminal(value: &Value) -> Option<Terminal> {
    if value
        .pointer("/promptFeedback/blockReason")
        .and_then(Value::as_str)
        .is_some_and(|r| r != "BLOCK_REASON_UNSPECIFIED")
    {
        return Some(Terminal::Rejected);
    }
    let candidates = value.get("candidates")?.as_array()?;
    if candidates.is_empty()
        || !candidates.iter().all(|c| {
            c.get("finishReason")
                .and_then(Value::as_str)
                .is_some_and(|r| r != "FINISH_REASON_UNSPECIFIED")
        })
    {
        return None;
    }
    if candidates.iter().any(|c| c["finishReason"] == "MAX_TOKENS") {
        Some(Terminal::Limited)
    } else if candidates.iter().all(|c| c["finishReason"] == "STOP") {
        Some(Terminal::Success)
    } else if candidates.iter().any(|c| {
        matches!(
            c["finishReason"].as_str(),
            Some("MALFORMED_FUNCTION_CALL" | "UNEXPECTED_TOOL_CALL" | "OTHER")
        )
    }) {
        Some(Terminal::Failure)
    } else {
        Some(Terminal::Rejected)
    }
}
