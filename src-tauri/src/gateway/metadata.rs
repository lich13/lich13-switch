//! Incremental JSON projection. Large content strings are consumed, never retained.
//! This is an observer, not a replacement for the bytes sent to either peer.
use serde_json::{Map, Number, Value};
use sha2::{Digest, Sha256};

const DEPTH: usize = 32;
const FIELD: usize = 2048;
const ITEMS: usize = 64;
const METADATA_BUDGET: usize = 1024 * 1024;

enum Container {
    Object(Map<String, Value>),
    Array(Vec<Value>),
}
struct Frame {
    value: Container,
    key: Option<String>,
    state: u8,
    role: String,
    keep: bool,
}
enum Lex {
    None,
    String {
        bytes: Vec<u8>,
        escaped: bool,
        large: bool,
        digest: Option<Sha256>,
        nonempty: bool,
    },
    Atom(Vec<u8>),
}
pub struct Projector {
    stack: Vec<Frame>,
    lex: Lex,
    root: Option<Value>,
    failed: bool,
    retained_bytes: usize,
}
impl Default for Projector {
    fn default() -> Self {
        Self {
            stack: vec![],
            lex: Lex::None,
            root: None,
            failed: false,
            retained_bytes: 0,
        }
    }
}
fn retained(key: &str) -> bool {
    matches!(
        key,
        "response"
            | "message"
            | "data"
            | "result"
            | "usage"
            | "id"
            | "response_id"
            | "object"
            | "model"
            | "type"
            | "status"
            | "service_tier"
            | "stop_reason"
            | "finish_reason"
            | "error"
            | "code"
            | "param"
            | "incomplete_details"
            | "reason"
            | "input_tokens"
            | "prompt_tokens"
            | "output_tokens"
            | "completion_tokens"
            | "cache_read_input_tokens"
            | "cached_input_tokens"
            | "cache_creation_input_tokens"
            | "cache_creation_tokens"
            | "cache_creation"
            | "ephemeral_5m_input_tokens"
            | "ephemeral_1h_input_tokens"
            | "input_tokens_details"
            | "prompt_tokens_details"
            | "output_tokens_details"
            | "completion_tokens_details"
            | "cached_tokens"
            | "image_tokens"
            | "audio_tokens"
            | "choices"
            | "delta"
            | "content"
            | "text"
            | "thinking"
            | "partial_json"
            | "reasoning_content"
            | "reasoning"
            | "function_call"
            | "function"
            | "tool_calls"
            | "arguments"
            | "input"
            | "output"
            | "item"
            | "encrypted_content"
            | "previous_response_id"
            | "stream"
            | "stream_id"
            | "payload"
            | "info"
            | "timestamp"
            | "sessionId"
            | "parentUuid"
            | "uuid"
            | "forked_from_id"
            | "source"
            | "subagent"
            | "thread_spawn"
            | "parent_thread_id"
            | "role"
            | "model_name"
            | "total_token_usage"
            | "last_token_usage"
            | "rate_limits"
            | "limit_id"
    )
}
fn content(key: &str) -> bool {
    matches!(
        key,
        "content"
            | "text"
            | "thinking"
            | "partial_json"
            | "reasoning_content"
            | "reasoning"
            | "arguments"
            | "delta"
    )
}
impl Projector {
    fn target(&self) -> (String, bool) {
        self.stack
            .last()
            .map(|f| {
                let key = f.key.as_deref().unwrap_or(&f.role);
                (
                    key.to_owned(),
                    f.keep
                        && match &f.value {
                            Container::Array(items) => items.len() < ITEMS,
                            Container::Object(map) => map.len() < ITEMS && retained(key),
                        },
                )
            })
            .unwrap_or_default()
    }
    fn value_allowed(&self) -> bool {
        self.stack
            .last()
            .map_or(self.root.is_none(), |f| match f.value {
                Container::Object(_) => f.state == 2,
                Container::Array(_) => f.state == 0 || f.state == 4,
            })
    }
    fn accept(&mut self, value: Value) {
        let Some(frame) = self.stack.last_mut() else {
            if self.root.is_some() {
                self.failed = true;
            } else {
                self.root = Some(value);
            }
            return;
        };
        match &mut frame.value {
            Container::Object(map) => {
                if frame.state != 2 {
                    self.failed = true;
                    return;
                }
                if let Some(key) = frame.key.take() {
                    if frame.keep && retained(&key) && map.len() < ITEMS {
                        if map.contains_key(&key) {
                            self.failed = true;
                            return;
                        }
                        self.retained_bytes = self
                            .retained_bytes
                            .saturating_add(64 + key.len() + value.as_str().map_or(0, str::len));
                        if self.retained_bytes > METADATA_BUDGET {
                            self.failed = true;
                            return;
                        }
                        map.insert(key, value);
                    }
                    frame.state = 3;
                } else {
                    self.failed = true;
                }
            }
            Container::Array(items) => {
                if frame.state != 0 && frame.state != 4 {
                    self.failed = true;
                    return;
                }
                frame.state = 3;
                // Request/response content is unnecessary except for compaction markers.
                let compact_array = frame.role == "output";
                let compact = value.get("type").and_then(Value::as_str).is_some_and(|t| {
                    matches!(
                        t,
                        "compaction" | "context_compaction" | "compaction_trigger"
                    )
                });
                if frame.keep && frame.role == "input" && items.len() >= ITEMS {
                    // Overflow is not a valid first-turn proof.
                    if let Some(last) = items.last_mut() {
                        *last = serde_json::json!({"type":"unknown"});
                    }
                }
                let value = if frame.role == "input" && !compact {
                    serde_json::json!({"type":value.get("type").cloned().unwrap_or(Value::String("message".into())),"role":value.get("role")})
                } else {
                    value
                };
                if frame.keep && items.len() < ITEMS && (!compact_array || compact) {
                    self.retained_bytes = self
                        .retained_bytes
                        .saturating_add(64 + value.as_str().map_or(0, str::len));
                    if self.retained_bytes > METADATA_BUDGET {
                        self.failed = true;
                        return;
                    }
                    items.push(value);
                }
            }
        }
    }
    fn string(&mut self, bytes: Vec<u8>, large: bool, digest: Option<Sha256>, nonempty: bool) {
        if self.stack.last().is_some_and(|f| {
            (f.state == 0 || f.state == 4) && matches!(f.value, Container::Object(_))
        }) {
            let key = if large {
                String::new()
            } else {
                serde_json::from_slice::<String>(&bytes).unwrap_or_default()
            };
            let frame = self.stack.last_mut().unwrap();
            frame.key = Some(key);
            frame.state = 1;
            return;
        }
        if !self.value_allowed() {
            self.failed = true;
            return;
        }
        let (role, _) = self.target();
        let v = if role == "encrypted_content" {
            // The opaque blob is only compared within the current routing boundary.
            if !large {
                serde_json::from_slice::<String>(&bytes)
                    .ok()
                    .map(|text| {
                        Value::String(format!(
                            "sha256:{}",
                            crate::storage::digest(text.as_bytes())
                        ))
                    })
                    .unwrap_or(Value::Null)
            } else if let Some(digest) = digest {
                Value::String(format!("sha256:{:x}", digest.finalize()))
            } else {
                Value::Null
            }
        } else if content(&role) || role == "input" {
            Value::String(if nonempty { "x" } else { "" }.into())
        } else if large {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };
        self.accept(v);
    }
    pub fn feed(&mut self, bytes: &[u8]) {
        for &b in bytes {
            if self.failed {
                return;
            }
            let lex = std::mem::replace(&mut self.lex, Lex::None);
            match lex {
                Lex::String {
                    mut bytes,
                    escaped,
                    mut large,
                    mut digest,
                    mut nonempty,
                } => {
                    if b < 0x20 {
                        self.failed = true;
                        return;
                    }
                    if b == b'"' && !escaped {
                        if !large {
                            bytes.push(b);
                        }
                        self.string(bytes, large, digest, nonempty);
                        continue;
                    }
                    if b == b'\\' {
                        digest = None;
                    }
                    if let Some(digest) = &mut digest {
                        digest.update([b]);
                    }
                    nonempty = true;
                    if bytes.len() < FIELD {
                        bytes.push(b);
                    } else {
                        large = true;
                    }
                    self.lex = Lex::String {
                        bytes,
                        escaped: b == b'\\' && !escaped,
                        large,
                        digest,
                        nonempty,
                    };
                    continue;
                }
                Lex::Atom(mut atom) => {
                    if !b.is_ascii_whitespace() && !matches!(b, b',' | b']' | b'}') {
                        if atom.len() >= 64 {
                            self.failed = true;
                            return;
                        }
                        atom.push(b);
                        self.lex = Lex::Atom(atom);
                        continue;
                    }
                    self.atom(&atom);
                }
                Lex::None => {}
            }
            match b {
                b'{' | b'[' => {
                    if !self.value_allowed() {
                        self.failed = true;
                        return;
                    }
                    if self.stack.len() >= DEPTH {
                        self.failed = true;
                        return;
                    }
                    let (role, keep) = self.target();
                    self.stack.push(Frame {
                        value: if b == b'{' {
                            Container::Object(Map::new())
                        } else {
                            Container::Array(vec![])
                        },
                        key: None,
                        state: 0,
                        role,
                        keep: self.stack.is_empty() || keep,
                    });
                }
                b'}' | b']' => {
                    if let Some(f) = self.stack.pop() {
                        if f.state != 0 && f.state != 3 {
                            self.failed = true;
                            return;
                        }
                        let value = match f.value {
                            Container::Object(v) if b == b'}' => Value::Object(v),
                            Container::Array(v) if b == b']' => Value::Array(v),
                            _ => {
                                self.failed = true;
                                return;
                            }
                        };
                        self.accept(value);
                    } else {
                        self.failed = true;
                    }
                }
                b'"' => {
                    self.lex = Lex::String {
                        bytes: vec![b'"'],
                        escaped: false,
                        large: false,
                        digest: (self.target().0 == "encrypted_content").then(Sha256::new),
                        nonempty: false,
                    }
                }
                b':' => match self.stack.last_mut() {
                    Some(f) if matches!(f.value, Container::Object(_)) && f.state == 1 => {
                        f.state = 2
                    }
                    _ => self.failed = true,
                },
                b',' => match self.stack.last_mut() {
                    Some(f) if f.state == 3 => f.state = 4,
                    _ => self.failed = true,
                },
                b if b.is_ascii_whitespace() => {}
                b => self.lex = Lex::Atom(vec![b]),
            }
        }
    }
    fn atom(&mut self, bytes: &[u8]) {
        if !self.value_allowed() {
            self.failed = true;
            return;
        }
        let value = match bytes {
            b"true" => Value::Bool(true),
            b"false" => Value::Bool(false),
            b"null" => Value::Null,
            _ => match std::str::from_utf8(bytes)
                .ok()
                .and_then(|s| s.parse::<Number>().ok())
            {
                Some(v) => Value::Number(v),
                None => {
                    self.failed = true;
                    return;
                }
            },
        };
        self.accept(value);
    }
    pub fn finish(mut self) -> Option<Value> {
        match std::mem::replace(&mut self.lex, Lex::None) {
            Lex::Atom(bytes) => self.atom(&bytes),
            Lex::String { .. } => self.failed = true,
            Lex::None => {}
        }
        if self.failed || !self.stack.is_empty() {
            None
        } else {
            self.root
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trailing_unterminated_string_is_not_a_complete_metadata_record() {
        let mut parser = Projector::default();
        parser.feed(br#"{"usage":{"input_tokens":3}} "unfinished"#);
        assert!(parser.finish().is_none());
    }

    #[test]
    fn nested_metadata_has_a_total_retention_budget() {
        let leaf = serde_json::json!({"code": "x".repeat(1800)});
        let wide = serde_json::json!({"data": vec![leaf; ITEMS]});
        let value = serde_json::json!({"data": vec![wide; ITEMS]});
        let bytes = serde_json::to_vec(&value).unwrap();
        let mut parser = Projector::default();
        for part in bytes.chunks(4096) {
            parser.feed(part);
        }
        assert!(parser.finish().is_none());
    }
}
