//! Bounded observation only: bytes forwarded by the gateway are never altered.
use serde_json::Value;
use std::io::Write;
const MAX_EVENT: usize = 2 * 1024 * 1024;

#[derive(Default, Clone, Debug)]
pub struct Observation {
    pub response_id: Option<String>,
    pub model: Option<String>,
    pub incomplete: bool,
    pub terminal: Option<Terminal>,
    pub expects_terminal: bool,
    pub first_event_model_error: Option<bool>,
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
        let value = outer
            .get("response")
            .filter(|v| v.is_object())
            .or_else(|| outer.get("message").filter(|v| v.is_object()))
            .unwrap_or(outer);
        // Only protocol envelopes may contribute an ID, never nested tool/output IDs.
        if (value.get("id").is_some() && outer.get("type").is_none())
            || value
                .get("object")
                .and_then(Value::as_str)
                .is_some_and(|v| {
                    matches!(v, "response" | "chat.completion" | "chat.completion.chunk")
                })
            || outer.get("response").is_some()
            || outer.get("message").is_some()
        {
            if let Some(id) = identifier(value.get("id")) {
                self.response_id = Some(id);
            }
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
            let status = value.get("status").and_then(Value::as_str).unwrap_or("");
            let terminal = match event {
                "response.completed" | "response.done" | "message_stop" => Some(Terminal::Success),
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
                    _ => None,
                },
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
    buffer: Vec<u8>,
    data: Vec<u8>,
    stream: bool,
    overflow: bool,
    event_overflow: bool,
}
impl Sink {
    fn new(stream: bool) -> Self {
        Self {
            observation: Observation::default(),
            buffer: vec![],
            data: vec![],
            stream,
            overflow: false,
            event_overflow: false,
        }
    }
    fn parse(&mut self, data: &[u8]) {
        if !data.trim_ascii().is_empty() && self.observation.first_event_model_error.is_none() {
            self.observation.first_event_model_error = Some(
                serde_json::from_slice(data).is_ok_and(|v| super::upstream_error::model_error(&v)),
            );
        }
        if data.trim_ascii() == b"[DONE]" {
            self.observation.terminal.get_or_insert(Terminal::Success);
            return;
        }
        if let Ok(v) = serde_json::from_slice(data) {
            self.observation.value(&v);
        }
    }
    fn line(&mut self) {
        if self.overflow {
            self.event_overflow = true;
            self.observation.incomplete = true;
        } else {
            let line = self.buffer.strip_suffix(b"\n").unwrap_or(&self.buffer);
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            if line.is_empty() {
                if !self.event_overflow && !self.data.is_empty() {
                    let data = std::mem::take(&mut self.data);
                    self.parse(&data);
                }
                self.data.clear();
                self.event_overflow = false;
            } else if let Some(data) = line.strip_prefix(b"data:") {
                if self.data.len() + data.len() < MAX_EVENT {
                    self.data.extend_from_slice(data);
                    self.data.push(b'\n');
                } else {
                    self.event_overflow = true;
                    self.observation.incomplete = true;
                }
            }
        }
        self.buffer.clear();
        self.overflow = false;
    }
    fn finish(&mut self) {
        if self.stream {
            if !self.buffer.is_empty() {
                self.line();
            }
            if !self.event_overflow {
                let data = std::mem::take(&mut self.data);
                self.parse(&data);
            }
        } else if !self.overflow {
            let data = std::mem::take(&mut self.buffer);
            self.parse(&data);
        }
    }
}
impl Write for Sink {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.stream {
            for segment in bytes.split_inclusive(|b| *b == b'\n') {
                if !self.overflow && self.buffer.len() + segment.len() <= MAX_EVENT {
                    self.buffer.extend_from_slice(segment);
                } else {
                    self.overflow = true;
                    self.observation.incomplete = true;
                    self.buffer.clear();
                }
                if segment.ends_with(b"\n") {
                    self.line();
                }
            }
        } else if !self.overflow && self.buffer.len() + bytes.len() <= MAX_EVENT {
            self.buffer.extend_from_slice(bytes);
        } else {
            self.overflow = true;
            self.observation.incomplete = true;
            self.buffer.clear();
            return Err(std::io::Error::other("protocol observation limit"));
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
enum Decoder {
    Plain(Sink),
    Gzip(flate2::write::GzDecoder<Sink>),
    Deflate(flate2::write::ZlibDecoder<Sink>),
    Zstd(zstd::stream::write::Decoder<'static, Sink>),
}
pub struct Observer {
    decoder: Decoder,
    failed: bool,
}

/// Only completion and response affinity are observed; no usage is collected.
pub struct Protocol {
    observer: Option<Observer>,
    pub observation: Observation,
    stream: bool,
    status: Option<u16>,
    finished: bool,
}
impl Protocol {
    pub fn new(stream: bool) -> Self {
        Self {
            observer: None,
            observation: Observation::default(),
            stream,
            status: None,
            finished: false,
        }
    }
    pub fn response(&mut self, status: u16, stream: bool, encoding: &str) {
        self.status = Some(status);
        self.stream = stream;
        self.observer = Some(Observer::new(stream, encoding));
    }
    pub fn feed(&mut self, bytes: &[u8]) {
        if let Some(o) = &mut self.observer {
            o.feed(bytes);
            self.observation = o.snapshot(false);
        }
    }
    pub fn value(&mut self, value: &Value) {
        self.observation.value(value);
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
    pub fn finish(&mut self, status: Option<u16>, reason: &str) {
        if self.finished {
            return;
        }
        self.finished = true;
        self.status = status.or(self.status);
        if let Some(o) = &mut self.observer {
            self.observation = o.snapshot(matches!(reason, "OK" | "HTTP"));
        }
        if self.observation.terminal.is_none() {
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
    }
}
impl Observer {
    pub fn new(stream: bool, encoding: &str) -> Self {
        let sink = Sink::new(stream);
        let mut failed = false;
        let decoder = match encoding {
            "" | "identity" => Decoder::Plain(sink),
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
