//! Bounded observation only: bytes forwarded by the gateway are never altered.
use super::metadata::Projector;
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
    pub expects_terminal: bool,
    pub first_event_model_error: Option<bool>,
    pub first_event_capacity_error: Option<bool>,
    pub capacity_error: bool,
    pub compaction_fingerprint: Option<String>,
    pub compaction_incompatible: bool,
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
        self.compaction_incompatible |= super::compaction::incompatible(outer);
        if let Some(fingerprint) = super::compaction::output(outer, projected) {
            self.compaction_fingerprint = Some(fingerprint);
        }
        self.meter.observe(outer, 0);
        if self.meter.model.is_some() {
            self.model = self.meter.model.clone();
        }
        let details = super::upstream_error::details_value(outer, false);
        if details != crate::events::Details::default() {
            self.error = details;
        }
        let value = outer
            .get("response")
            .filter(|v| v.is_object())
            .or_else(|| outer.get("message").filter(|v| v.is_object()))
            .unwrap_or(outer);
        if let Some(id) = crate::usage::model::response_id(outer) {
            self.response_id = Some(id.into());
        }
        if let Some(model) = identifier(value.get("model")) {
            self.model = Some(model);
        }
        let event = outer.get("type").and_then(Value::as_str).unwrap_or("");
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
            let status = value.get("status").and_then(Value::as_str).unwrap_or("");
            let terminal = if super::upstream_error::model_error(outer) {
                Some(Terminal::ModelUnavailable)
            } else {
                match event {
                    "response.completed" | "response.done" | "message_stop" => {
                        Some(Terminal::Success)
                    }
                    "response.cancelled" | "response.canceled" => Some(Terminal::Cancelled),
                    "response.failed" | "error" => Some(error_terminal(value)),
                    "response.incomplete" => Some(incomplete_terminal(value)),
                    "message" if value.get("stop_reason").is_some_and(|v| !v.is_null()) => {
                        Some(Terminal::Success)
                    }
                    _ => match status {
                        "completed" => Some(Terminal::Success),
                        "failed" => Some(error_terminal(value)),
                        "incomplete" => Some(incomplete_terminal(value)),
                        "cancelled" => Some(Terminal::Cancelled),
                        _ if value.get("error").is_some_and(|e| !e.is_null()) => {
                            Some(error_terminal(value))
                        }
                        _ if self.error != crate::events::Details::default() => {
                            Some(error_terminal(outer))
                        }
                        _ => None,
                    },
                }
            };
            // A chat completion finishes only once all reported choices finish.
            let choices = outer.get("choices").and_then(Value::as_array);
            self.terminal = terminal.or_else(|| {
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
        }
    }
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
        }
        if self.data_len <= 16 && self.data_prefix.trim_ascii() == b"[DONE]" {
            self.observation.terminal.get_or_insert(Terminal::Success);
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
    }
}
impl Write for Sink {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if !self.stream {
            self.json.feed(bytes);
            return Ok(bytes.len());
        }
        for &b in bytes {
            if b == b'\r' {
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
pub struct Protocol {
    observer: Option<Observer>,
    pub observation: Observation,
    stream: bool,
    status: Option<u16>,
    finished: bool,
    transport_failure: bool,
    usage: Option<crate::usage::AttemptTrace>,
    started: std::time::Instant,
    first_usage_ms: Option<u64>,
    ownership: Option<(std::sync::Arc<super::compaction::Lease>, String)>,
}
impl Protocol {
    pub fn new(stream: bool) -> Self {
        Self {
            observer: None,
            observation: Observation::default(),
            stream,
            status: None,
            finished: false,
            transport_failure: false,
            usage: None,
            started: std::time::Instant::now(),
            first_usage_ms: None,
            ownership: None,
        }
    }
    pub fn attach_usage(&mut self, usage: crate::usage::AttemptTrace) {
        self.observation.meter.anthropic = Some(usage.is_claude());
        self.usage = Some(usage);
    }
    pub fn attach_ownership(
        &mut self,
        lease: Option<std::sync::Arc<super::compaction::Lease>>,
        provider: &str,
    ) {
        self.ownership = lease.map(|lease| (lease, provider.into()));
    }
    fn record_usage(&mut self) {
        if self.terminal() == Some(Terminal::Success) {
            if let Some((lease, provider)) = &self.ownership {
                lease.complete(provider, self.observation.compaction_fingerprint.as_deref());
            }
        }
        if self.observation.meter.first_token_ms.is_some() && self.first_usage_ms.is_none() {
            self.first_usage_ms = Some(self.started.elapsed().as_millis() as u64);
        }
        let mut meter = self.observation.meter.clone();
        meter.parse_incomplete |= self.observation.incomplete;
        meter.ended_early = self.transport_failure || self.terminal() == Some(Terminal::Cancelled);
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
        if let Some(o) = &mut self.observer {
            o.feed(bytes);
            self.observation = o.snapshot(false);
        }
        self.record_usage();
    }
    pub fn value(&mut self, value: &Value) {
        self.observation.value(value);
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
            return;
        }
        self.finished = true;
        self.status = self.status.or(status);
        if let Some(o) = &mut self.observer {
            self.observation = o.snapshot(true);
        }
        self.transport_failure = self.observation.terminal.is_none()
            && matches!(
                reason,
                "NETWORK" | "TLS" | "FIRST_BYTE_TIMEOUT" | "STREAM_TIMEOUT" | "STREAM_INTERRUPTED"
            );
        if self.transport_failure {
            self.observation.error.local_code = Some(
                match reason {
                    "TLS" => "TLS_HANDSHAKE_FAILED",
                    "NETWORK" => "CONNECTION_FAILED",
                    other => other,
                }
                .into(),
            );
        }
        if self.observation.terminal.is_none() {
            if reason == "OK"
                && self.stream
                && self.observation.expects_terminal
                && !self.observation.incomplete
            {
                // EOF before a terminal event is a transport/incomplete-stream
                // failure, unlike an explicit response.failed event.
                self.transport_failure = true;
                self.observation.error.local_code = Some("STREAM_INTERRUPTED".into());
            }
            self.observation.terminal = Some(match reason {
                "OK" if self.stream && self.observation.expects_terminal => {
                    if self.observation.incomplete {
                        Terminal::Unknown
                    } else {
                        Terminal::Failure
                    }
                }
                "OK" => Terminal::Success,
                "HTTP" | "CLIENT_ERROR" => Terminal::Rejected,
                "CANCELLED" => Terminal::Cancelled,
                _ => Terminal::Failure,
            });
        }
        self.record_usage();
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
