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

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(super) enum Intake {
    #[default]
    Ordinary,
    NewThread,
    Compacted,
}
fn accepts(provider: &super::model::Provider, intake: Intake) -> bool {
    accepts_flags(
        provider.handoff_after_compaction,
        provider.take_new_threads,
        intake,
    )
}
pub(super) fn accepts_flags(compacted: bool, new_thread: bool, intake: Intake) -> bool {
    (!compacted && !new_thread)
        || (intake == Intake::NewThread && new_thread)
        || (intake == Intake::Compacted && compacted)
}
pub fn unidentified_candidates(routes: Vec<super::Route>) -> Vec<super::Route> {
    routes
        .into_iter()
        .filter(|r| accepts(&r.provider, Intake::Ordinary))
        .collect()
}

fn now() -> u64 {
    crate::usage::model::now().max(0) as u64
}
fn digest_valid(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

pub fn session(headers: &hyper::HeaderMap) -> Option<String> {
    let mut identity = None;
    for key in ["thread-id", "thread_id", "conversation_id"] {
        for value in headers.get_all(key) {
            let value = value.to_str().ok()?;
            if value.trim().is_empty()
                || value.len() > 256
                || value.chars().any(char::is_control)
                || identity.is_some_and(|previous| previous != value)
            {
                return None;
            }
            identity = Some(value);
        }
    }
    if identity.is_none() {
        // Underscore spelling is the legacy conversation identifier. Modern
        // session-id may be shared prompt-cache affinity, never a thread ID.
        for value in headers.get_all("session_id") {
            let value = value.to_str().ok()?;
            if value.trim().is_empty()
                || value.len() > 256
                || value.chars().any(char::is_control)
                || identity.is_some_and(|previous| previous != value)
            {
                return None;
            }
            identity = Some(value);
        }
    }
    identity.map(|v| storage::digest(v.as_bytes()))
}

pub fn decision(
    thread: Option<&str>,
    hints: Option<&super::replay::RequestHints>,
    pinned: bool,
    lease: Option<&Lease>,
    provider: &str,
) -> crate::usage::model::RoutingDecision {
    use crate::usage::model::RoutingDecision as D;
    if pinned {
        D::CursorPinned
    } else if hints.is_some_and(|h| h.auxiliary) {
        D::Auxiliary
    } else if thread.is_none() {
        D::ThreadUnidentified
    } else if let Some(lease) = lease {
        lease.decision(hints, provider)
    } else {
        D::OrdinaryQueue
    }
}

pub fn rejection_reason(
    thread: Option<&str>,
    hints: Option<&super::replay::RequestHints>,
) -> &'static str {
    if thread.is_none() {
        "缺少可靠线程标识，没有符合接管策略的供应商"
    } else if hints.is_some_and(|h| h.previous_response_id.is_some()) {
        "响应游标固定，不能跨供应商接管"
    } else if hints.is_some_and(|h| h.compacted_window.is_some()) {
        "压缩边界未匹配或没有符合接管条件的供应商"
    } else if hints.is_some_and(|h| h.first_turn) {
        "没有可接收新线程的供应商"
    } else {
        "接管策略不接受此进行中的线程"
    }
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
    #[serde(default)]
    consumed_boundary: Option<String>,
    boundary_sequence: u64,
    #[serde(default)]
    boundary_fresh: bool,
    #[serde(default)]
    establishing: bool,
    #[serde(default)]
    first_turn: bool,
    #[serde(default)]
    history_seen: bool,
    #[serde(default)]
    claim: Option<u64>,
}
#[derive(Default)]
struct State {
    entries: HashMap<String, Entry>,
    sequence: u64,
    error: Option<String>,
    last_saved: u64,
    save_version: u64,
    persisted_version: u64,
    notifications: Vec<(u64, String, String, String)>,
}
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HandoffWaiting {
    pub provider_id: String,
    pub state: String,
}
pub struct Registry {
    path: PathBuf,
    state: Mutex<State>,
    persist: std::sync::mpsc::SyncSender<()>,
    notifications: Mutex<Option<(super::ClientId, crate::events::Service)>>,
    names: Mutex<HashMap<String, String>>,
}
pub struct Lease {
    registry: Arc<Registry>,
    session: String,
    epoch: u64,
    sequence: u64,
    pub owner: Option<String>,
    pub handoff: bool,
    completed_epoch: Mutex<Option<u64>>,
    pub fresh: bool,
    preferred: Vec<String>,
    receivers: Vec<String>,
    ordinary_receivers: Vec<String>,
    initial_bound: std::sync::atomic::AtomicBool,
    strict: bool,
    confirmed: bool,
    intake: Intake,
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
                                    && e.consumed_boundary.as_deref().is_none_or(digest_valid)
                            }) =>
                    {
                        state.entries = entries
                            .into_iter()
                            .filter(|(_, e)| now().saturating_sub(e.updated) < RETENTION_MS)
                            .map(|(key, mut e)| {
                                // Older records cannot prove that a window is still fresh.
                                // An interrupted claim cannot be replayed after restart.
                                if !e.boundary_fresh || e.claim.take().is_some() {
                                    e.consumed_boundary = e.boundary.take().or(e.consumed_boundary);
                                    e.boundary_fresh = false;
                                }
                                (key, e)
                            })
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
        let (persist, changes) = std::sync::mpsc::sync_channel(1);
        let registry = Arc::new(Self {
            path,
            state: Mutex::new(state),
            persist,
            notifications: Mutex::new(None),
            names: Mutex::new(HashMap::new()),
        });
        let weak = Arc::downgrade(&registry);
        std::thread::spawn(move || {
            while changes.recv().is_ok() {
                let Some(registry) = weak.upgrade() else {
                    break;
                };
                let (entries, version) = {
                    let state = registry.state.lock().unwrap();
                    (state.entries.clone(), state.save_version)
                };
                let saved = serde_json::to_vec(&entries)
                    .ok()
                    .and_then(|raw| storage::atomic_write(&registry.path, &raw, None).ok())
                    .is_some();
                let notifications = {
                    let mut state = registry.state.lock().unwrap();
                    state.error = (!saved).then(|| "线程归属记录保存失败".into());
                    if saved {
                        state.persisted_version = version;
                        state.last_saved = now();
                        let ready: Vec<_> = state
                            .notifications
                            .iter()
                            .filter(|(n, ..)| *n <= version)
                            .cloned()
                            .collect();
                        state.notifications.retain(|(n, ..)| *n > version);
                        ready
                    } else {
                        vec![]
                    }
                };
                for (_, scope, from, to) in notifications {
                    if let Some((client, events)) = registry.notifications.lock().unwrap().as_ref()
                    {
                        let names = registry.names.lock().unwrap();
                        let label = |id: &String| {
                            names
                                .get(id)
                                .and_then(|n| crate::events::safe_message(n))
                                .unwrap_or_else(|| "供应商".into())
                        };
                        let mut record = crate::events::Record::new(
                            Some(*client),
                            Some(&to),
                            None,
                            crate::events::Reason::ThreadHandoff,
                            crate::events::Action::Routed,
                            None,
                            None,
                        );
                        record.details.handoff_scope = Some(scope);
                        record.notification_message =
                            Some(format!("线程已接管：{} → {}", label(&from), label(&to)));
                        events.emit(record);
                    }
                }
            }
        });
        registry
    }
    pub fn notifications(&self, client: super::ClientId, events: crate::events::Service) {
        *self.notifications.lock().unwrap() = Some((client, events));
    }
    pub fn provider_names(&self, providers: &[super::model::Provider]) {
        *self.names.lock().unwrap() = providers
            .iter()
            .map(|p| (p.id.clone(), p.name.clone()))
            .collect();
    }
    fn save(&self, state: &mut State, force: bool) {
        if !force && now().saturating_sub(state.last_saved) < 60_000 {
            return;
        }
        state.save_version = state.save_version.saturating_add(1);
        let _ = self.persist.try_send(());
    }
    #[cfg(test)]
    pub fn flush(&self) {
        let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            {
                let state = self.state.lock().unwrap();
                if state.persisted_version == state.save_version || state.error.is_some() {
                    return;
                }
            }
            assert!(
                std::time::Instant::now() < until,
                "ownership write did not settle"
            );
            std::thread::yield_now();
        }
    }
    #[cfg(test)]
    pub fn prepare(
        self: &Arc<Self>,
        session: String,
        input: Option<&str>,
        can_handoff: bool,
        eligible: &[String],
    ) -> Option<Arc<Lease>> {
        self.prepare_inner(
            session,
            input,
            can_handoff,
            eligible,
            super::replay::TurnEvidence::Unknown,
        )
    }
    fn prepare_inner(
        self: &Arc<Self>,
        session: String,
        input: Option<&str>,
        can_handoff: bool,
        eligible: &[String],
        turn: super::replay::TurnEvidence,
    ) -> Option<Arc<Lease>> {
        use super::replay::TurnEvidence;
        let first_turn = can_handoff && turn == TurnEvidence::First;
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
            let oldest = state
                .entries
                .iter()
                .filter(|(_, e)| e.claim.is_none())
                .min_by_key(|(_, e)| e.updated)
                .map(|(k, _)| k.clone())?;
            state.entries.remove(&oldest);
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
                establishing: true,
                first_turn,
                ..Default::default()
            });
        let history_changed = !entry.history_seen && turn == TurnEvidence::History;
        entry.history_seen |= turn == TurnEvidence::History;
        if entry.history_seen {
            entry.first_turn = false;
        } else if entry.establishing && first_turn {
            entry.first_turn = true;
            entry.provider = first.clone();
        }
        let owner = eligible
            .contains(&entry.provider)
            .then(|| entry.provider.clone());
        let handoff = can_handoff
            && owner.is_some()
            && entry.claim.is_none()
            && entry.boundary_fresh
            && input.is_some()
            && entry.boundary.as_deref() == input;
        if handoff {
            entry.claim = Some(sequence);
        }
        entry.updated = now();
        let epoch = entry.epoch;
        let entry_establishing = entry.establishing;
        let first_turn = first_turn && entry.first_turn;
        let retry_write = state.error.is_some();
        self.save(
            &mut state,
            fresh || retry_write || handoff || history_changed,
        );
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
            fresh: fresh || entry_establishing,
            preferred: vec![],
            receivers: eligible.to_vec(),
            ordinary_receivers: eligible.to_vec(),
            initial_bound: std::sync::atomic::AtomicBool::new(false),
            strict: false,
            confirmed: true,
            intake: if entry_establishing && first_turn {
                Intake::NewThread
            } else {
                Intake::Ordinary
            },
        }))
    }
    pub fn prepare_policy(
        self: &Arc<Self>,
        session: String,
        hints: Option<&super::replay::RequestHints>,
        can_handoff: bool,
        eligible: &[super::model::Provider],
    ) -> Option<Arc<Lease>> {
        if hints.is_some_and(|h| h.auxiliary) {
            return None;
        }
        let ids: Vec<_> = eligible.iter().map(|p| p.id.clone()).collect();
        let mut ordered = eligible.to_vec();
        if can_handoff && hints.is_some_and(|h| h.first_turn) {
            ordered.sort_by_key(|p| !p.take_new_threads);
        }
        let mut lease = self.prepare_inner(
            session,
            hints.and_then(|h| h.compacted_window.as_deref()),
            can_handoff,
            &ordered.iter().map(|p| p.id.clone()).collect::<Vec<_>>(),
            hints.map_or(super::replay::TurnEvidence::Unknown, |h| h.turn),
        )?;
        let inner = Arc::get_mut(&mut lease)?;
        inner.strict = true;
        inner.confirmed = hints.is_some_and(|h| h.turn != super::replay::TurnEvidence::Unknown);
        inner.intake = if inner.handoff {
            Intake::Compacted
        } else {
            inner.intake
        };
        inner.preferred = ordered.iter().map(|p| p.id.clone()).collect();
        inner.ordinary_receivers = eligible
            .iter()
            .filter(|p| accepts(p, Intake::Ordinary))
            .map(|p| p.id.clone())
            .collect();
        let owner_position = inner
            .owner
            .as_ref()
            .and_then(|id| ids.iter().position(|p| p == id))
            .unwrap_or(ids.len());
        inner.receivers = eligible
            .iter()
            .take(owner_position)
            .filter(|p| p.handoff_after_compaction)
            .map(|p| p.id.clone())
            .collect();
        if let Some(owner) = &inner.owner {
            inner.receivers.push(owner.clone());
        }
        Some(lease)
    }
    pub fn waiting_policy(
        &self,
        queued: &[super::model::Provider],
        capacity: &super::admission::Scheduler,
    ) -> Vec<HandoffWaiting> {
        let state = self.state.lock().unwrap();
        let (active, _) = capacity.counts();
        let mut result = std::collections::BTreeMap::new();
        for entry in state
            .entries
            .values()
            .filter(|e| !e.establishing && now().saturating_sub(e.updated) < RETENTION_MS)
        {
            let Some(position) = queued.iter().position(|p| p.id == entry.provider) else {
                continue;
            };
            for provider in queued[..position]
                .iter()
                .filter(|p| p.handoff_after_compaction)
            {
                let full = provider.max_concurrency != 0
                    && active.get(&provider.id).copied().unwrap_or(0)
                        + capacity.reserved(&provider.id)
                        >= provider.max_concurrency as usize;
                let phase = if full {
                    "capacity_full"
                } else if entry.boundary.is_some() {
                    "awaiting_request"
                } else {
                    "awaiting_compaction"
                };
                result
                    .entry(provider.id.clone())
                    .and_modify(|current| {
                        if phase == "capacity_full"
                            || (*current == "awaiting_compaction" && phase == "awaiting_request")
                        {
                            *current = phase;
                        }
                    })
                    .or_insert(phase);
            }
        }
        result
            .into_iter()
            .map(|(provider_id, state)| HandoffWaiting {
                provider_id,
                state: state.into(),
            })
            .collect()
    }
    pub fn boundary_matches(&self, session: &str, input: Option<&str>) -> bool {
        input.is_some()
            && self
                .state
                .lock()
                .unwrap()
                .entries
                .get(session)
                .is_some_and(|entry| {
                    entry.claim.is_none()
                        && entry.boundary_fresh
                        && entry.boundary.as_deref() == input
                })
    }
    pub fn owner(&self, session: &str) -> Option<String> {
        self.state
            .lock()
            .unwrap()
            .entries
            .get(session)
            .filter(|e| !e.establishing && now().saturating_sub(e.updated) < RETENTION_MS)
            .map(|e| e.provider.clone())
    }
    pub fn error(&self) -> Option<String> {
        self.state.lock().unwrap().error.clone()
    }
}
impl Lease {
    pub fn session_digest(&self) -> &str {
        &self.session
    }
    pub fn establishing(&self) -> bool {
        self.registry
            .state
            .lock()
            .unwrap()
            .entries
            .get(&self.session)
            .is_some_and(|e| e.establishing)
    }
    pub fn decision(
        &self,
        hints: Option<&super::replay::RequestHints>,
        provider: &str,
    ) -> crate::usage::model::RoutingDecision {
        use crate::usage::model::RoutingDecision as D;
        if self.handoff {
            return if self.owner.as_deref() == Some(provider) {
                D::NoEligibleReceiver
            } else {
                D::CompactionHandoff
            };
        }
        if self.admission_policy().0 == Intake::NewThread {
            return D::NewThread;
        }
        if self.fresh {
            return if self.confirmed {
                D::OrdinaryQueue
            } else {
                D::FirstTurnUnconfirmed
            };
        }
        if let Some(window) = hints.and_then(|h| h.compacted_window.as_deref()) {
            let state = self.registry.state.lock().unwrap();
            if state.entries.get(&self.session).is_some_and(|entry| {
                entry.claim.is_some() && entry.boundary.as_deref() == Some(window)
            }) {
                return D::BoundaryInFlight;
            }
            if state.entries.get(&self.session).is_some_and(|entry| {
                entry.boundary.as_deref() != Some(window)
                    && entry.consumed_boundary.as_deref() != Some(window)
            }) {
                return D::CompactionWindowMismatch;
            }
        } else if self
            .registry
            .state
            .lock()
            .unwrap()
            .entries
            .get(&self.session)
            .is_some_and(|entry| entry.boundary.is_some())
        {
            return D::CompactionWindowMissing;
        }
        D::ExistingThread
    }
    pub(super) fn admission_policy(&self) -> (Intake, Option<String>) {
        let state = self.registry.state.lock().unwrap();
        let entry = state.entries.get(&self.session);
        let intake = if self.intake == Intake::NewThread && entry.is_some_and(|e| e.history_seen) {
            Intake::Ordinary
        } else {
            self.intake
        };
        let owner = entry
            .filter(|entry| !entry.establishing)
            .map(|entry| entry.provider.clone());
        (intake, owner)
    }
    pub fn owns(&self, provider: &str) -> bool {
        !self.handoff
            && self
                .registry
                .state
                .lock()
                .unwrap()
                .entries
                .get(&self.session)
                .is_some_and(|entry| !entry.establishing && entry.provider == provider)
    }
    /// The first actual admission, not the first successful response, establishes
    /// ownership. Competing first requests release their slot and recheck it.
    pub fn admitted(&self, provider: &str) -> bool {
        use std::sync::atomic::Ordering;
        if !self.fresh || self.initial_bound.load(Ordering::Acquire) {
            return true;
        }
        let mut state = self.registry.state.lock().unwrap();
        let Some(entry) = state.entries.get_mut(&self.session) else {
            return false;
        };
        if !entry.establishing && entry.provider != provider {
            return false;
        }
        if entry.establishing
            && entry.history_seen
            && self.strict
            && self.intake == Intake::NewThread
            && !self.ordinary_receivers.iter().any(|id| id == provider)
        {
            return false;
        }
        let changed = entry.establishing;
        entry.provider = provider.into();
        entry.establishing = !self.confirmed;
        self.initial_bound.store(true, Ordering::Release);
        if changed {
            self.registry.save(&mut state, true);
        }
        state.error.is_none()
    }
    #[cfg(test)]
    pub fn candidates(&self, routes: Vec<super::Route>) -> Vec<super::Route> {
        self.candidates_for(routes, &super::routing::Requirement::Unknown)
    }
    pub fn candidates_for(
        &self,
        mut routes: Vec<super::Route>,
        requirement: &super::routing::Requirement,
    ) -> Vec<super::Route> {
        if self.handoff {
            routes.retain(|r| {
                self.receivers.contains(&r.provider.id)
                    && (!self.strict
                        || self.owner.as_deref() == Some(&r.provider.id)
                        || r.provider.handoff_after_compaction)
            });
            return routes;
        }
        let (intake, bound) = self.admission_policy();
        if self.fresh && bound.is_none() {
            if self.strict {
                routes.retain(|r| accepts(&r.provider, intake));
            }
            routes.sort_by_key(|r| {
                self.preferred
                    .iter()
                    .position(|id| id == &r.provider.id)
                    .unwrap_or(usize::MAX)
            });
            return routes;
        }
        if bound.is_some() {
            self.initial_bound
                .store(true, std::sync::atomic::Ordering::Release);
        }
        let current = bound;
        if let Some(owner) = current
            .as_deref()
            .and_then(|id| {
                routes.iter().find(|r| {
                    if r.provider.id != id {
                        return false;
                    }
                    let health = r.health_for(requirement);
                    // A short upstream Retry-After is not evidence that this
                    // conversation's owner has failed. New concurrent turns must wait
                    // for that owner too, instead of slipping through to another route.
                    health.available
                        || (health.state == super::circuit::CircuitState::Closed
                            && health.cooldown_reason.as_deref() == Some("retry_after")
                            && health.cause.as_ref().is_some_and(|cause| {
                                cause.reason == crate::events::Reason::UpstreamService
                                    && cause.status.is_some_and(super::circuit::is_server_error)
                            }))
                })
            })
            .cloned()
        {
            return vec![owner];
        }
        if self.strict {
            routes.retain(|r| {
                current.as_deref() == Some(&r.provider.id) || accepts(&r.provider, intake)
            });
        }
        routes
    }
    /// A failed attempt before any output opens the ordinary fallback queue.
    /// The compaction claim still gates the eventual atomic ownership commit;
    /// its priority-limited receivers apply only to proactive handoff.
    pub fn candidates_after_failure(
        &self,
        mut routes: Vec<super::Route>,
        requirement: &super::routing::Requirement,
    ) -> Vec<super::Route> {
        if !self.handoff {
            return self.candidates_for(routes, requirement);
        }
        routes.retain(|route| {
            self.owner.as_deref() == Some(&route.provider.id)
                || !self.strict
                || accepts(&route.provider, Intake::Compacted)
        });
        routes
    }
    /// Close this one opportunity at the first output, including response.created.
    /// The in-flight winning claim remains valid solely to commit that same turn.
    pub fn output_started(&self, provider: &str) {
        let mut state = self.registry.state.lock().unwrap();
        let Some(entry) = state.entries.get_mut(&self.session) else {
            return;
        };
        if entry.epoch != self.epoch {
            return;
        }
        let mut changed = false;
        if entry.establishing {
            entry.establishing = false;
            entry.provider = provider.into();
            changed = true;
        }
        if entry.boundary_fresh
            && self.sequence > entry.boundary_sequence
            && entry.claim.is_none_or(|claim| claim == self.sequence)
        {
            entry.consumed_boundary = entry.boundary.take();
            entry.boundary_fresh = false;
            changed = true;
        }
        if changed {
            self.registry.save(&mut state, true);
        }
    }
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
        if !first && boundary == entry.boundary.as_deref() {
            return;
        }
        if entry.epoch != completed.unwrap_or(self.epoch) {
            return;
        }
        if first && self.handoff && entry.claim != Some(self.sequence) {
            return;
        }
        let moved = first && entry.provider != provider;
        let previous = entry.provider.clone();
        let changed = (first && (moved || entry.establishing)) || boundary.is_some();
        if first {
            entry.establishing = false;
        }
        if moved {
            entry.provider = provider.into();
            entry.epoch = entry.epoch.saturating_add(1);
            entry.consumed_boundary = entry.boundary.take().or(entry.consumed_boundary.take());
            entry.boundary_fresh = false;
        }
        if first && self.handoff {
            entry.claim = None;
        }
        if let Some(boundary) = boundary.filter(|_| self.sequence >= entry.boundary_sequence) {
            if entry.consumed_boundary.as_deref() != Some(boundary) {
                entry.boundary = Some(boundary.into());
                entry.boundary_sequence = self.sequence;
                entry.boundary_fresh = true;
            }
        }
        *completed = Some(entry.epoch);
        entry.updated = now();
        if moved && self.handoff {
            let scope = storage::digest(format!("{}:{}", self.session, self.sequence).as_bytes());
            let version = state.save_version.saturating_add(1);
            state
                .notifications
                .push((version, scope, previous, provider.into()));
        }
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
        registry.flush();
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
