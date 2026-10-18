//! Codec-v2 binary JSON values. Tags and field IDs are immutable; unknown
//! fields remain literal strings, so diagnostics can evolve without data loss.
use serde_json::{Map, Number, Value};

const FIELDS: &[&str] = &[
    "id",
    "client",
    "source",
    "startedAt",
    "finishedAt",
    "sessionId",
    "attempts",
    "completed",
    "estimatedSpeed",
    "duplicateOf",
    "deduplication",
    "mergedSources",
    "gatewayReported",
    "provider",
    "requestedModel",
    "responseModel",
    "pricingModel",
    "responseId",
    "status",
    "outcome",
    "durationMs",
    "firstTokenMs",
    "stream",
    "transport",
    "tokens",
    "inclusiveInput",
    "input",
    "output",
    "cacheRead",
    "cacheWrite",
    "cacheWrite5m",
    "cacheWrite1h",
    "imageInput",
    "imageOutput",
    "audioInput",
    "audioOutput",
    "serviceTier",
    "price",
    "compactionKind",
    "usageStatus",
    "availability",
    "usageSources",
    "pricingBasis",
    "mappingRevision",
    "repeatCount",
    "compactedUnpriced",
    "operation",
    "costMultiplier",
    "basisRef",
    "cost",
    "model",
    "multiplier",
    "rates",
    "version",
    "cache_read",
    "cache_write",
    "first_token",
    "transfer",
    "end",
    "cause",
    "responseStarted",
    "terminalReceived",
    "threadHash",
    "upstreamRequestHash",
    "routingDecision",
    "routingEvidence",
    "clientRequestHash",
    "downstreamBytes",
    "downstreamError",
    "endedAt",
    "firstByteAt",
    "lastProgressMs",
    "receivedAt",
    "responseHash",
    "sentAt",
    "terminalAt",
    "terminalHandedOff",
    "timeoutPhase",
    "timeoutSeconds",
    "upstreamBytes",
    "upstreamEnd",
    "upstreamEof",
    "wsCloseCode",
    "terminalEvent",
    "terminalStatus",
    "incompleteReason",
    "connection",
    "events",
    "sequence",
];

fn uint(mut n: u64, out: &mut Vec<u8>) {
    while n >= 128 {
        out.push(n as u8 | 128);
        n >>= 7;
    }
    out.push(n as u8);
}
fn string(s: &str, out: &mut Vec<u8>) {
    uint(s.len() as u64, out);
    out.extend_from_slice(s.as_bytes());
}
pub(super) fn encode(value: &Value) -> Vec<u8> {
    fn write(v: &Value, out: &mut Vec<u8>) {
        match v {
            Value::Null => out.push(0),
            Value::Bool(false) => out.push(1),
            Value::Bool(true) => out.push(2),
            Value::Number(n) => {
                if let Some(n) = n.as_u64() {
                    out.push(3);
                    uint(n, out);
                } else if let Some(n) = n.as_i64() {
                    out.push(4);
                    uint(n.unsigned_abs(), out);
                } else {
                    out.push(5);
                    out.extend_from_slice(&n.as_f64().expect("JSON number").to_le_bytes());
                }
            }
            Value::String(s)
                if s.len() == 64
                    && s.bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) =>
            {
                out.push(7);
                for pair in s.as_bytes().chunks_exact(2) {
                    let nibble = |b: u8| if b <= b'9' { b - b'0' } else { b - b'a' + 10 };
                    out.push((nibble(pair[0]) << 4) | nibble(pair[1]));
                }
            }
            Value::String(s) => {
                out.push(6);
                string(s, out);
            }
            Value::Array(values) => {
                out.push(8);
                uint(values.len() as u64, out);
                for v in values {
                    write(v, out);
                }
            }
            Value::Object(fields) => {
                out.push(9);
                uint(fields.len() as u64, out);
                for (key, v) in fields {
                    if let Some(id) = FIELDS.iter().position(|field| *field == key) {
                        uint(id as u64 + 1, out);
                    } else {
                        uint(0, out);
                        string(key, out);
                    }
                    write(v, out);
                }
            }
        }
    }
    let mut out = Vec::new();
    write(value, &mut out);
    out
}

struct Reader<'a> {
    bytes: &'a [u8],
}
impl Reader<'_> {
    fn take(&mut self, n: usize) -> Option<&[u8]> {
        if self.bytes.len() < n {
            return None;
        }
        let (head, tail) = self.bytes.split_at(n);
        self.bytes = tail;
        Some(head)
    }
    fn uint(&mut self) -> Option<u64> {
        let mut n = 0;
        for shift in (0..=63).step_by(7) {
            let byte = self.take(1)?[0];
            if shift == 63 && byte > 1 {
                return None;
            }
            n |= u64::from(byte & 127) << shift;
            if byte < 128 {
                return Some(n);
            }
        }
        None
    }
    fn count(&mut self) -> Option<usize> {
        let n = usize::try_from(self.uint()?).ok()?;
        (n <= self.bytes.len()).then_some(n)
    }
    fn string(&mut self) -> Option<String> {
        let n = self.count()?;
        std::str::from_utf8(self.take(n)?).ok().map(str::to_owned)
    }
    fn value(&mut self, depth: usize) -> Option<Value> {
        if depth > 128 {
            return None;
        }
        Some(match self.take(1)?[0] {
            0 => Value::Null,
            1 => false.into(),
            2 => true.into(),
            3 => self.uint()?.into(),
            4 => {
                let n = self.uint()?;
                if n == 1 << 63 {
                    i64::MIN.into()
                } else {
                    i64::try_from(n).ok()?.checked_neg()?.into()
                }
            }
            5 => Value::Number(Number::from_f64(f64::from_le_bytes(
                self.take(8)?.try_into().ok()?,
            ))?),
            6 => self.string()?.into(),
            7 => {
                const HEX: &[u8] = b"0123456789abcdef";
                let mut s = String::with_capacity(64);
                for b in self.take(32)? {
                    s.push(HEX[(b >> 4) as usize] as char);
                    s.push(HEX[(b & 15) as usize] as char);
                }
                s.into()
            }
            8 => {
                let n = self.count()?;
                let mut values = Vec::with_capacity(n.min(1024));
                for _ in 0..n {
                    values.push(self.value(depth + 1)?);
                }
                Value::Array(values)
            }
            9 => {
                let n = self.count()?;
                let mut fields = Map::new();
                for _ in 0..n {
                    let id = self.uint()?;
                    let key = if id == 0 {
                        self.string()?
                    } else {
                        FIELDS.get(usize::try_from(id - 1).ok()?)?.to_string()
                    };
                    if fields.insert(key, self.value(depth + 1)?).is_some() {
                        return None;
                    }
                }
                Value::Object(fields)
            }
            _ => return None,
        })
    }
}
pub(super) fn decode(bytes: &[u8]) -> Option<Value> {
    let mut reader = Reader { bytes };
    let value = reader.value(0)?;
    reader.bytes.is_empty().then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_preserves_unknown_fields_extreme_numbers_and_literal_strings() {
        let value = serde_json::json!({
            "id": "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
            "future-field": [null, false, true, 0, u64::MAX, i64::MIN, -0.0, 0.125,
                "", "合成数据", "ABCDEF0123456789abcdef0123456789abcdef0123456789abcdef0123456789"],
            "empty": [{}, []], "price": {"cost": "0.000000000000000001"}
        });
        let encoded = encode(&value);
        assert_eq!(decode(&encoded), Some(value));
        for end in 0..encoded.len() {
            assert_eq!(decode(&encoded[..end]), None);
        }
        let mut extra = encoded;
        extra.push(0);
        assert_eq!(decode(&extra), None);
        assert_eq!(decode(&[8, 255, 255, 255, 255, 127]), None);
        assert_eq!(decode(&[5, 255, 255, 255, 255, 255, 255, 255, 255]), None);
    }
}
