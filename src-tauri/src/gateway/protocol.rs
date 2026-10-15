//! Bounded observation only: bytes forwarded by the gateway are never altered.
use super::metadata::Projector;
use crate::errors::code;
use serde_json::Value;
use std::io::Write;

#[derive(Default, Clone, Debug)]
pub struct Observation {
    pub meter: crate::usage::model::Meter,
    pub error: crate::events::Details,
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
    pub retry_after: Option<std::time::Duration>,
    pub capacity_error: bool,
    pub compaction_fingerprint: Option<String>,
    pub compaction_incompatible: bool,
    responses_seen: bool,
    pending_error_response: Option<String>,
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
pub fn identifier(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str)
        .filter(|s| !s.is_empty() && s.len() <= 256 && !s.chars().any(char::is_control))
        .map(str::to_owned)
}
impl Observation {
    pub fn value(&mut self, outer: &Value) {
        self.value_projected(outer, false);
    }
    fn value_projected(&mut self, outer: &Value, projected: bool) {
        let value = outer
            .get("response")
            .filter(|v| v.is_object())
            .or_else(|| outer.get("message").filter(|v| v.is_object()))
            .unwrap_or(outer);
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
        let details = super::upstream_error::details_value(outer, false);
        let has_error = details != crate::events::Details::default();
        if has_error {
            self.error = details;
            self.retry_after = self
                .retry_after
                .max(super::upstream_error::retry_after_value(outer));
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
            || outer.get("choices").is_some();
        if self.terminal.is_none() {
            self.capacity_error = super::upstream_error::temporary_capacity_event(outer);
            let terminal = if super::upstream_error::model_error(outer) {
                Some(Terminal::ModelUnavailable)
            } else {
                match event {
                    "response.completed" | "response.done" | "message_stop" => {
                        Some(Terminal::Success)
                    }
                    "response.cancelled" | "response.canceled" => Some(Terminal::Cancelled),
                    "response.failed" => Some(error_terminal(value)),
                    "response.incomplete" => Some(incomplete_terminal(value)),
                    "message" if value.get("stop_reason").is_some_and(|v| !v.is_null()) => {
                        Some(Terminal::Success)
                    }
                    _ => match status {
                        "completed" => Some(Terminal::Success),
                        "failed" => Some(error_terminal(value)),
                        "incomplete" => Some(incomplete_terminal(value)),
                        "cancelled" => Some(Terminal::Cancelled),
                        _ if event == "error" => Some(error_terminal(value)),
                        _ if value.get("error").is_some_and(|e| !e.is_null()) => {
                            Some(error_terminal(value))
                        }
                        _ if has_error => Some(error_terminal(outer)),
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
                && self.error.health_scope != Some(crate::events::HealthScope::Provider)
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

fn error_terminal(value: &Value) -> Terminal {
    if super::upstream_error::model_error(value) {
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
        }
    }
    fn data(&mut self, bytes: &[u8]) {
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
            return;
        }
        let json = std::mem::take(&mut self.json);
        let first = self.observation.first_event_model_error.is_none();
        if first {
            // A complete non-JSON SSE event must also release the response
            // prefix. Observation failure cannot stall transparent forwarding.
            self.observation.first_event_model_error = Some(false);
            self.observation.first_event_capacity_error = Some(false);
            self.observation.first_event_account_error = Some(false);
        }
        if self.data_len <= 16 && self.data_prefix.trim_ascii() == b"[DONE]" {
            self.observation.done();
        } else if let Some(mut v) = json.finish() {
            if v.get("type").is_none() && !self.event.is_empty() {
                if let Some(map) = v.as_object_mut() {
                    map.insert("type".into(), Value::String(self.event.clone()));
                }
            }
            if first {
                self.observation.first_event_model_error =
                    Some(super::upstream_error::model_error(&v));
                self.observation.first_event_capacity_error =
                    Some(super::upstream_error::temporary_capacity_event(&v));
                self.observation.first_event_account_error = Some(
                    super::upstream_error::details_value(&v, false).health_scope
                        == Some(crate::events::HealthScope::Provider),
                );
            }
            self.observation.value_projected(&v, true);
        } else {
            self.observation.incomplete = true;
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
            if self.line_kind != 0 || !self.field.is_empty() {
                self.line_end();
            }
            self.event();
        } else if let Some(v) = std::mem::take(&mut self.json).finish() {
            self.observation.value_projected(&v, true);
        } else {
            self.observation.incomplete = true;
        }
        self.observation.finish_pending();
    }
}
impl Write for Sink {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if !self.stream {
            self.json.feed(bytes);
            return Ok(bytes.len());
        }
        for &b in bytes {
            if self.after_cr {
                self.after_cr = false;
                if b == b'\n' {
                    continue;
                }
            }
            if b == b'\r' {
                self.line_end();
                self.after_cr = true;
                continue;
            }
            if b == b'\n' {
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
                    if self.skip_space && b == b' ' {
                        self.skip_space = false;
                        continue;
                    }
                    self.skip_space = false;
                    self.data(&[b]);
                }
                2 if self.field.len() < 256 => self.field.push(b),
                _ => {}
            }
        }
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
        *self.activity.lock().unwrap() = Some(activity.clone());
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
        if self.activity.lock().unwrap().is_none() {
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
        if let Some(activity) = self.activity.lock().unwrap().as_ref() {
            activity.event(kind);
        }
    }
    pub fn dropped(&self) {
        if self.finished.load(std::sync::atomic::Ordering::Acquire) {
            return;
        }
        let activity = self.activity.lock().unwrap();
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
    stop: Option<tokio::sync::watch::Receiver<bool>>,
    downstream: Option<tokio::sync::watch::Receiver<bool>>,
}
impl Protocol {
    pub fn new(stream: bool) -> Self {
        Self {
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
            stop: None,
            downstream: None,
        }
    }
    pub fn attach_thread(
        &mut self,
        thread: Option<std::sync::Arc<super::admission::ThreadRequest>>,
    ) {
        self.thread = thread;
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
    pub fn request_headers(&mut self, headers: &hyper::HeaderMap) {
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
    pub fn attach_usage(&mut self, usage: crate::usage::AttemptTrace) {
        self.observation.meter.anthropic = usage.anthropic();
        self.usage = Some(usage);
    }
    pub fn context(
        &mut self,
        thread: Option<String>,
        decision: Option<crate::usage::model::RoutingDecision>,
    ) {
        if let Some(usage) = &mut self.usage {
            usage.context(thread, decision);
        }
    }
    pub fn response_headers(&mut self, headers: &hyper::HeaderMap) {
        self.transfer.upstream_client_request_hash = headers
            .get("x-client-request-id")
            .filter(|v| !v.is_empty() && v.len() <= 512)
            .map(|v| crate::storage::digest(v.as_bytes()));
        if let Some(usage) = &mut self.usage {
            usage.upstream_request(headers);
        }
    }
    pub fn routing_evidence(&mut self, hints: Option<&super::replay::RequestHints>) {
        if let Some(usage) = &mut self.usage {
            usage.routing_evidence(
                hints
                    .map(|h| h.routing_evidence.clone())
                    .unwrap_or_default(),
            );
        }
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
    }
    pub fn business_observed(&self) -> bool {
        !self.stream
            || self.observation.first_event_model_error.is_some()
            || self.terminal_received
            || self.observation.incomplete
    }
    pub fn output_handed(&self, business: bool) {
        if business {
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
                    lease.complete(provider, self.observation.compaction_fingerprint.as_deref());
                }
            }
        }
    }
    pub fn delivered_success(&self) -> bool {
        self.terminal() == Some(Terminal::Success)
            && self
                .delivery
                .terminal
                .load(std::sync::atomic::Ordering::Relaxed)
    }
    fn record_usage(&mut self) {
        if self.terminal_received {
            if self.transfer.terminal_at.is_none() {
                self.delivery.event("upstream_terminal_observed");
            }
            self.transfer
                .terminal_at
                .get_or_insert_with(crate::usage::model::now);
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
        observer.sink().observation.meter.anthropic = self.observation.meter.anthropic;
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
    pub fn value(&mut self, value: &Value) {
        self.observation.value(value);
        self.terminal_received |= self.observation.terminal.is_some();
        self.record_usage();
    }
    pub fn terminal(&self) -> Option<Terminal> {
        if self.status.is_some_and(|s| s >= 400) {
            return Some(Terminal::Rejected);
        }
        self.observation.terminal
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
        self.record_usage();
        self.record_end(reason);
    }
    fn record_end(&mut self, reason: &str) {
        use crate::usage::model::{EndCause, EndMetadata};
        let cause = match reason {
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
        self.transfer.ended_at = crate::usage::model::now();
        self.transfer.downstream_bytes = self.delivery.bytes.load(Relaxed);
        self.transfer.terminal_handed_off = self.delivery.terminal.load(Relaxed);
        self.transfer.terminal_event = self.observation.terminal_event.clone();
        self.transfer.terminal_status = self.observation.terminal_status.clone();
        self.transfer.incomplete_reason = self.observation.incomplete_reason.clone();
        self.transfer.connection = self
            .delivery
            .activity
            .lock()
            .unwrap()
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
}
impl Drop for Protocol {
    fn drop(&mut self) {
        // A dropped handler is not evidence of an explicit client cancellation.
        // Flush only the bytes already observed; never continue the generation.
        let reason = if self.stop.as_ref().is_some_and(|s| *s.borrow()) {
            code::GATEWAY_STOPPED
        } else if self.downstream.as_ref().is_some_and(|s| *s.borrow()) {
            code::CLIENT_DISCONNECTED
        } else {
            "UNSETTLED"
        };
        self.finish(None, reason);
        self.snapshot_delivery();
        if let Some(usage) = &mut self.usage {
            usage.transfer(self.transfer.clone());
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
        Self { decoder, failed }
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
        result
    }
}
