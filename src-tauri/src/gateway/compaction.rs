//! Hash-only, bounded conversation ownership. Request content is never persisted.
use crate::storage;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
};

const MAX_THREADS: usize = 4096;
const RETENTION_MS: u64 = 7 * 24 * 60 * 60 * 1000;

fn now() -> u64 {
    crate::usage::model::now().max(0) as u64
}
fn digest_valid(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

pub fn session(headers: &hyper::HeaderMap) -> Option<String> {
    let values: Vec<_> = ["session_id", "conversation_id"]
        .into_iter()
        .filter_map(|key| headers.get(key).and_then(|v| v.to_str().ok()))
        .filter(|v| !v.is_empty() && v.len() <= 256 && !v.chars().any(char::is_control))
        .collect();
    // Codex's session_id is stable across HTTP and WebSocket transports.
    values.first().map(|v| storage::digest(v.as_bytes()))
}

pub fn fingerprint(value: &Value, projected: bool) -> Option<String> {
    let kind = value.get("type").and_then(Value::as_str)?;
    if !matches!(kind, "compaction" | "context_compaction") {
        return None;
    }
    let opaque = value.get("encrypted_content").and_then(Value::as_str)?;
    if opaque.is_empty() {
        return None;
    }
    if projected {
        opaque
            .strip_prefix("sha256:")
            .filter(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()))
            .map(str::to_owned)
    } else {
        Some(storage::digest(opaque.as_bytes()))
    }
}

pub fn output(value: &Value, projected: bool) -> Option<String> {
    let mut nodes = vec![value];
    for _ in 0..5 {
        let mut next = Vec::new();
        for node in nodes {
            if let Some(items) = node.get("output").and_then(Value::as_array) {
                if let Some(found) = items.iter().rev().find_map(|v| fingerprint(v, projected)) {
                    return Some(found);
                }
            }
            if let Some(found) = node.get("item").and_then(|v| fingerprint(v, projected)) {
                return Some(found);
            }
            for key in ["response", "data", "result"] {
                if let Some(v) = node.get(key).filter(|v| v.is_object()) {
                    next.push(v);
                }
            }
        }
        nodes = next;
    }
    None
}

pub fn incompatible(value: &Value) -> bool {
    let mut nodes = vec![value];
    for _ in 0..5 {
        let mut next = vec![];
        for node in nodes {
            for key in ["code", "type", "message"] {
                if let Some(text) = node
                    .get(key)
                    .and_then(Value::as_str)
                    .filter(|s| s.len() <= 2048)
                {
                    let text = text.to_ascii_lowercase();
                    if [
                        "invalid_encrypted_content",
                        "invalid_compaction",
                        "compaction_context_invalid",
                        "compaction_context_unsupported",
                        "encrypted content could not be verified",
                    ]
                    .iter()
                    .any(|code| text.contains(code))
                    {
                        return true;
                    }
                    if (text.contains("compaction") || text.contains("encrypted_content"))
                        && ["unsupported", "invalid", "not supported", "decrypt"]
                            .iter()
                            .any(|word| text.contains(word))
                    {
                        return true;
                    }
                }
            }
            for key in ["error", "response", "data", "result"] {
                if let Some(v) = node.get(key).filter(|v| v.is_object()) {
                    next.push(v);
                }
            }
        }
        nodes = next;
    }
    false
}

#[derive(Clone, Default, Deserialize, Serialize)]
struct Entry {
    provider: String,
    updated: u64,
    epoch: u64,
    boundary: Option<String>,
    boundary_sequence: u64,
    #[serde(skip)]
    claim: Option<u64>,
}
#[derive(Default)]
struct State {
    entries: HashMap<String, Entry>,
    sequence: u64,
    error: Option<String>,
    last_saved: u64,
}
pub struct Registry {
    path: PathBuf,
    state: Mutex<State>,
}
pub struct Lease {
    registry: Arc<Registry>,
    session: String,
    epoch: u64,
    sequence: u64,
    pub owner: Option<String>,
    pub handoff: bool,
    completed_epoch: Mutex<Option<u64>>,
}
impl Registry {
    pub fn new(path: PathBuf) -> Arc<Self> {
        let mut state = State::default();
        match storage::read_optional(&path) {
            Ok(Some(raw)) if raw.len() <= 2 * 1024 * 1024 => {
                match serde_json::from_slice::<HashMap<String, Entry>>(&raw) {
                    Ok(entries)
                        if entries.len() <= MAX_THREADS
                            && entries.iter().all(|(key, e)| {
                                digest_valid(key)
                                    && !e.provider.is_empty()
                                    && e.provider.len() <= 256
                                    && !e.provider.chars().any(char::is_control)
                                    && e.boundary.as_deref().is_none_or(digest_valid)
                            }) =>
                    {
                        state.entries = entries
                            .into_iter()
                            .filter(|(_, e)| now().saturating_sub(e.updated) < RETENTION_MS)
                            .collect();
                        state.sequence = state
                            .entries
                            .values()
                            .map(|e| e.boundary_sequence)
                            .max()
                            .unwrap_or(0);
                    }
                    _ => state.error = Some("线程归属记录无法读取".into()),
                }
            }
            Ok(None) => {}
            _ => state.error = Some("线程归属记录无法读取".into()),
        }
        Arc::new(Self {
            path,
            state: Mutex::new(state),
        })
    }
    fn save(&self, state: &mut State, force: bool) {
        if !force && now().saturating_sub(state.last_saved) < 60_000 {
            return;
        }
        let result = serde_json::to_vec(&state.entries)
            .ok()
            .and_then(|raw| storage::atomic_write(&self.path, &raw, None).ok());
        state.error = result.is_none().then(|| "线程归属记录保存失败".into());
        if result.is_some() {
            state.last_saved = now();
        }
    }
    pub fn prepare(
        self: &Arc<Self>,
        session: String,
        input: Option<&str>,
        can_handoff: bool,
        eligible: &[String],
    ) -> Option<Arc<Lease>> {
        let first = eligible.first()?;
        let mut state = self.state.lock().unwrap();
        // A corrupt ledger must not silently guess a persisted conversation owner.
        if state.error.is_some() && state.entries.is_empty() {
            return None;
        }
        state
            .entries
            .retain(|_, e| now().saturating_sub(e.updated) < RETENTION_MS || e.claim.is_some());
        if !state.entries.contains_key(&session) && state.entries.len() >= MAX_THREADS {
            if let Some(oldest) = state
                .entries
                .iter()
                .filter(|(_, e)| e.claim.is_none())
                .min_by_key(|(_, e)| e.updated)
                .map(|(k, _)| k.clone())
            {
                state.entries.remove(&oldest);
            } else {
                return None;
            }
        }
        state.sequence = state.sequence.saturating_add(1);
        let sequence = state.sequence;
        let fresh = !state.entries.contains_key(&session);
        let entry = state
            .entries
            .entry(session.clone())
            .or_insert_with(|| Entry {
                provider: first.clone(),
                updated: now(),
                ..Default::default()
            });
        let owner = eligible
            .contains(&entry.provider)
            .then(|| entry.provider.clone());
        let handoff = can_handoff
            && owner.is_some()
            && entry.claim.is_none()
            && input.is_some()
            && entry.boundary.as_deref() == input;
        if handoff {
            entry.claim = Some(sequence);
        }
        entry.updated = now();
        let epoch = entry.epoch;
        let retry_write = state.error.is_some();
        self.save(&mut state, fresh || retry_write);
        if state.error.is_some() {
            if let Some(entry) = state.entries.get_mut(&session) {
                if entry.claim == Some(sequence) {
                    entry.claim = None;
                }
            }
            return None;
        }
        Some(Arc::new(Lease {
            registry: self.clone(),
            session,
            epoch,
            sequence,
            owner,
            handoff,
            completed_epoch: Mutex::new(None),
        }))
    }
    pub fn pending(&self, queued: &[String]) -> Vec<String> {
        let state = self.state.lock().unwrap();
        let mut ids: Vec<_> = state
            .entries
            .values()
            .filter(|e| {
                queued.first().is_some_and(|first| first != &e.provider)
                    && queued.contains(&e.provider)
            })
            .map(|e| e.provider.clone())
            .collect();
        ids.sort();
        ids.dedup();
        ids
    }
    pub fn boundary_matches(&self, session: &str, input: Option<&str>) -> bool {
        input.is_some()
            && self
                .state
                .lock()
                .unwrap()
                .entries
                .get(session)
                .is_some_and(|entry| entry.claim.is_none() && entry.boundary.as_deref() == input)
    }
    pub fn error(&self) -> Option<String> {
        self.state.lock().unwrap().error.clone()
    }
}
impl Lease {
    pub fn complete(&self, provider: &str, boundary: Option<&str>) {
        let mut completed = self.completed_epoch.lock().unwrap();
        let first = completed.is_none();
        if !first && boundary.is_none() {
            return;
        }
        let mut state = self.registry.state.lock().unwrap();
        let Some(entry) = state.entries.get_mut(&self.session) else {
            return;
        };
        if entry.epoch != completed.unwrap_or(self.epoch) {
            return;
        }
        if first && self.handoff && entry.claim != Some(self.sequence) {
            return;
        }
        let changed = (first && (entry.provider != provider || self.handoff)) || boundary.is_some();
        if first && (entry.provider != provider || self.handoff) {
            entry.provider = provider.into();
            entry.epoch = entry.epoch.saturating_add(1);
            entry.boundary = None;
        }
        if first && self.handoff {
            entry.boundary = None;
            entry.claim = None;
        }
        if let Some(boundary) = boundary.filter(|_| self.sequence >= entry.boundary_sequence) {
            entry.boundary = Some(boundary.into());
            entry.boundary_sequence = self.sequence;
        }
        *completed = Some(entry.epoch);
        entry.updated = now();
        self.registry.save(&mut state, changed);
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        if !self.handoff {
            return;
        }
        if let Some(entry) = self
            .registry
            .state
            .lock()
            .unwrap()
            .entries
            .get_mut(&self.session)
        {
            if entry.claim == Some(self.sequence) {
                entry.claim = None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn ids(values: &[&str]) -> Vec<String> {
        values.iter().map(|v| (*v).into()).collect()
    }
    fn key() -> String {
        storage::digest(b"fixture-session")
    }

    #[test]
    fn new_threads_follow_priority_existing_threads_wait_for_matching_window() {
        let dir = tempfile::tempdir().unwrap();
        let registry = Registry::new(dir.path().join("owners.json"));
        let first = registry.prepare(key(), None, true, &ids(&["old"])).unwrap();
        first.complete("old", None);
        let sticky = registry
            .prepare(key(), None, true, &ids(&["new", "old"]))
            .unwrap();
        assert_eq!(sticky.owner.as_deref(), Some("old"));
        assert!(!sticky.handoff);
        sticky.complete("old", Some("fixture-boundary"));
        let wrong = registry
            .prepare(key(), Some("other-window"), true, &ids(&["new", "old"]))
            .unwrap();
        assert!(!wrong.handoff);
        drop(wrong);
        let cursor = registry
            .prepare(
                key(),
                Some("fixture-boundary"),
                false,
                &ids(&["new", "old"]),
            )
            .unwrap();
        assert!(!cursor.handoff);
        drop(cursor);
        let next = registry
            .prepare(key(), Some("fixture-boundary"), true, &ids(&["new", "old"]))
            .unwrap();
        assert!(next.handoff);
        next.complete("new", None);
        let after = registry
            .prepare(key(), None, true, &ids(&["new", "old"]))
            .unwrap();
        assert_eq!(after.owner.as_deref(), Some("new"));
        assert!(!after.handoff);
        let new_thread = registry
            .prepare(
                storage::digest(b"new-fixture-session"),
                None,
                true,
                &ids(&["new", "old"]),
            )
            .unwrap();
        assert_eq!(new_thread.owner.as_deref(), Some("new"));
    }

    #[test]
    fn boundary_claim_is_single_use_cancel_releases_and_late_old_result_cannot_rebind() {
        let dir = tempfile::tempdir().unwrap();
        let registry = Registry::new(dir.path().join("owners.json"));
        registry
            .prepare(key(), None, false, &ids(&["old"]))
            .unwrap()
            .complete("old", Some("window"));
        let first = registry
            .prepare(key(), Some("window"), true, &ids(&["new", "old"]))
            .unwrap();
        let parallel = registry
            .prepare(key(), Some("window"), true, &ids(&["new", "old"]))
            .unwrap();
        assert!(first.handoff);
        assert!(!parallel.handoff);
        drop(first);
        let retry = registry
            .prepare(key(), Some("window"), true, &ids(&["new", "old"]))
            .unwrap();
        assert!(retry.handoff);
        retry.complete("new", None);
        parallel.complete("old", Some("old-window"));
        assert_eq!(
            registry
                .prepare(key(), None, true, &ids(&["new", "old"]))
                .unwrap()
                .owner
                .as_deref(),
            Some("new")
        );
        assert!(!registry.boundary_matches(&key(), Some("old-window")));
    }

    #[test]
    fn restart_restores_owner_and_boundary_without_plain_identifiers_or_payload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("owners.json");
        let registry = Registry::new(path.clone());
        let boundary = storage::digest(b"fixture-compaction-payload");
        registry
            .prepare(key(), None, true, &ids(&["old"]))
            .unwrap()
            .complete("old", Some(&boundary));
        drop(registry);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("fixture-session"));
        assert!(!text.contains("fixture-compaction-payload"));
        let restored = Registry::new(path);
        assert!(
            restored
                .prepare(key(), Some(&boundary), true, &ids(&["new", "old"]))
                .unwrap()
                .handoff
        );
    }

    #[test]
    fn invalid_persisted_owner_never_silently_rebinds_a_thread() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("owners.json");
        let entries = HashMap::from([(
            key(),
            Entry {
                provider: "old".into(),
                updated: now(),
                boundary: Some("invalid-boundary".into()),
                ..Default::default()
            },
        )]);
        let raw = serde_json::to_vec(&entries).unwrap();
        std::fs::write(&path, &raw).unwrap();
        let registry = Registry::new(path.clone());
        assert!(registry
            .prepare(key(), None, true, &ids(&["new", "old"]))
            .is_none());
        assert!(registry.error().is_some());
        assert_eq!(std::fs::read(path).unwrap(), raw);
    }

    #[test]
    fn request_recognition_ignores_capabilities_thresholds_and_plain_text() {
        use super::super::replay::RequestHints;
        use serde_json::json;
        let normal = RequestHints::from_value(json!({"model":"fixture-model","input":[{"type":"message","content":"compaction_trigger"}],"context_management":[{"type":"compaction","compact_threshold":1000}]}), false).unwrap();
        assert!(!normal.compaction_trigger);
        assert!(normal.compacted_window.is_none());
        let trigger =
            RequestHints::from_value(json!({"input":[{"type":"compaction_trigger"}]}), false)
                .unwrap();
        assert!(trigger.compaction_trigger);
        let value = json!({"type":"compaction","encrypted_content":"fixture-opaque"});
        let raw = serde_json::to_vec(&json!({"input":[value.clone()]})).unwrap();
        let mut projection = super::super::metadata::Projector::default();
        for chunk in raw.chunks(3) {
            projection.feed(chunk);
        }
        let hints = RequestHints::from_value(projection.finish().unwrap(), true).unwrap();
        assert_eq!(hints.compacted_window, fingerprint(&value, false));
    }
}
