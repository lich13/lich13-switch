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
const POISONED: &str = "线程归属状态不可用，已停止自动接管";
const RETENTION_MS: u64 = 7 * 24 * 60 * 60 * 1000;

#[cfg(test)]
#[path = "compaction_lifecycle_v0259_tests.rs"]
mod lifecycle_v0259_tests;
#[cfg(test)]
#[path = "model_detour_policy_tests.rs"]
mod model_detour_policy_tests;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(super) enum Intake {
    #[default]
    Ordinary,
    NewThread,
    Compacted,
    Unavailable,
}
fn accepts(provider: &super::model::Provider, intake: Intake) -> bool {
    accepts_flags(
        provider.handoff_after_compaction,
        provider.take_new_threads,
        intake,
    )
}
pub(super) fn accepts_flags(compacted: bool, new_thread: bool, intake: Intake) -> bool {
    intake != Intake::Unavailable
        && ((!compacted && !new_thread)
            || (intake == Intake::NewThread && new_thread)
            || (intake == Intake::Compacted && compacted))
}
pub fn unidentified_candidates(routes: Vec<super::Route>) -> Vec<super::Route> {
    routes
        .into_iter()
        .filter(|r| accepts(&r.provider, Intake::Ordinary))
        .collect()
}

pub fn queued(gateway: &super::Gateway) -> storage::Result<Vec<super::model::Provider>> {
    Ok(gateway
        .request_state()?
        .store
        .providers
        .iter()
        .filter(|provider| provider.queued)
        .cloned()
        .collect())
}

fn now() -> u64 {
    crate::usage::model::now().max(0) as u64
}
fn digest_valid(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

pub fn session(headers: &hyper::HeaderMap) -> Option<String> {
    identity(headers).0
}

pub fn identity(
    headers: &hyper::HeaderMap,
) -> (Option<String>, crate::usage::model::ThreadIdentity) {
    use crate::usage::model::ThreadIdentity as I;
    let mut value = None;
    let mut kind = I::Reliable;
    for keys in [
        &["thread-id", "thread_id", "conversation_id"][..],
        &["session_id"][..],
    ] {
        for key in keys {
            for header in headers.get_all(*key) {
                let Ok(candidate) = header.to_str() else {
                    return (None, I::Conflict);
                };
                if candidate.trim().is_empty()
                    || candidate.len() > 256
                    || candidate.chars().any(char::is_control)
                    || value.is_some_and(|previous| previous != candidate)
                {
                    return (None, I::Conflict);
                }
                value = Some(candidate);
            }
        }
        if let Some(value) = value {
            return (Some(storage::digest(value.as_bytes())), kind);
        }
        // Modern session-id can be prompt-cache affinity; only the legacy
        // underscore spelling is accepted after reliable thread headers.
        kind = I::LegacySession;
    }
    (None, I::Missing)
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
fn model_detour(
    entry: &Entry,
    queued: &[super::model::Provider],
    requirement: &super::routing::Requirement,
) -> bool {
    !entry.establishing
        && now().saturating_sub(entry.updated) < RETENTION_MS
        && matches!(requirement, super::routing::Requirement::Model(_))
        && queued.iter().any(|provider| {
            provider.id == entry.provider
                && provider.queued
                && !requirement.allows(provider.allowed_models.as_deref())
        })
}
struct RequestPolicy<'a> {
    queued: &'a [super::model::Provider],
    requirement: &'a super::routing::Requirement,
    pinned: Option<&'a str>,
}
impl RequestPolicy<'_> {
    fn request_only(&self, entry: &Entry) -> bool {
        model_detour(entry, self.queued, self.requirement)
            // A response cursor may outlive a model detour or a legal handoff.
            // Its fixed provider can serve that continuation, but the cursor
            // itself does not authorize moving the conversation back there.
            || (!entry.establishing
                && now().saturating_sub(entry.updated) < RETENTION_MS
                && self.pinned.is_some_and(|id| id != entry.provider))
    }
}
#[derive(Clone)]
struct PendingReport {
    version: u64,
    record: crate::events::Record,
    handoff: Option<(String, String)>,
}
#[derive(Default)]
struct State {
    entries: HashMap<String, Entry>,
    sequence: u64,
    error: Option<String>,
    last_saved: u64,
    save_version: u64,
    persisted_version: u64,
    notifications: Vec<PendingReport>,
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
    boundary_sequence: u64,
    pub owner: Option<String>,
    pub handoff: bool,
    completed_epoch: Mutex<Option<u64>>,
    pub fresh: bool,
    preferred: Vec<String>,
    receivers: Vec<String>,
    ordinary_receivers: Vec<String>,
    initial_bound: std::sync::atomic::AtomicBool,
    failure_fallback: std::sync::atomic::AtomicBool,
    strict: bool,
    confirmed: bool,
    intake: Intake,
    // Request-only authority is immutable, including after failures or a
    // concurrent handoff. It must never be upgraded by successful delivery.
    request_only: bool,
    routing_log: Mutex<Option<Arc<super::routing_log::RoutingLog>>>,
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
                            .map(|e| e.boundary_sequence.max(e.epoch))
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
                    let Ok(state) = registry.state.lock() else {
                        break;
                    };
                    (state.entries.clone(), state.save_version)
                };
                let saved = serde_json::to_vec(&entries)
                    .ok()
                    .and_then(|raw| storage::atomic_write(&registry.path, &raw, None).ok())
                    .is_some();
                let notifications = {
                    let Ok(mut state) = registry.state.lock() else {
                        break;
                    };
                    state.error = (!saved).then(|| "线程归属记录保存失败".into());
                    if saved {
                        state.persisted_version = version;
                        state.last_saved = now();
                    }
                    let ready: Vec<_> = state
                        .notifications
                        .iter()
                        .filter(|report| report.version <= version)
                        .cloned()
                        .collect();
                    state
                        .notifications
                        .retain(|report| report.version > version);
                    ready
                };
                for mut report in notifications {
                    if let Some((_, events)) =
                        super::locks::published(&registry.notifications).as_ref()
                    {
                        if !saved {
                            report.record.reason = crate::events::Reason::ThreadRouting;
                            report.record.error_code =
                                Some(crate::errors::ErrorCode::ConversationOwnership);
                            report.record.level = crate::events::Level::Error;
                            report.record.action = crate::events::Action::Stopped;
                            report.record.details.message = Some("线程归属记录保存失败".into());
                            if let Some(routing) = &mut report.record.details.routing {
                                routing.outcome = crate::events::RoutingOutcome::SaveFailed;
                            }
                        } else if let Some((from, to)) = report.handoff {
                            let names = super::locks::published(&registry.names);
                            let label = |id: &String| {
                                names
                                    .get(id)
                                    .and_then(|n| crate::events::safe_message(n))
                                    .unwrap_or_else(|| "供应商".into())
                            };
                            report.record.notification_message =
                                Some(format!("线程已接管：{} → {}", label(&from), label(&to)));
                        }
                        events.emit(report.record);
                    }
                }
            }
        });
        registry
    }
    pub fn notifications(&self, client: super::ClientId, events: crate::events::Service) {
        *super::locks::published(&self.notifications) = Some((client, events));
    }
    pub fn provider_names(&self, providers: &[super::model::Provider]) {
        *super::locks::published(&self.names) = providers
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
                let Ok(state) = self.state.lock() else { return };
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
            None,
        )
    }
    fn prepare_inner(
        self: &Arc<Self>,
        session: String,
        input: Option<&str>,
        can_handoff: bool,
        eligible: &[String],
        turn: super::replay::TurnEvidence,
        policy: Option<RequestPolicy<'_>>,
    ) -> Option<Arc<Lease>> {
        use super::replay::TurnEvidence;
        let first_turn = can_handoff && turn == TurnEvidence::First;
        let first = eligible.first()?;
        let mut state = self.state.lock().ok()?;
        // A corrupt ledger must not silently guess a persisted conversation owner.
        if state.error.is_some() && state.entries.is_empty() {
            return None;
        }
        // Check the unfiltered queue under the ownership lock, before touching
        // history, sequence, timestamps or the one-use compaction claim.
        if let Some(entry) = state.entries.get(&session).filter(|entry| {
            policy
                .as_ref()
                .is_some_and(|policy| policy.request_only(entry))
        }) {
            return Some(Arc::new(Lease {
                registry: self.clone(),
                session,
                epoch: entry.epoch,
                sequence: state.sequence,
                boundary_sequence: entry.boundary_sequence,
                owner: Some(entry.provider.clone()),
                handoff: false,
                completed_epoch: Mutex::new(None),
                fresh: false,
                preferred: vec![],
                receivers: eligible.to_vec(),
                ordinary_receivers: eligible.to_vec(),
                initial_bound: std::sync::atomic::AtomicBool::new(true),
                failure_fallback: std::sync::atomic::AtomicBool::new(false),
                strict: true,
                confirmed: true,
                intake: Intake::Ordinary,
                request_only: true,
                routing_log: Mutex::new(None),
            }));
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
                epoch: sequence,
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
        // Queue/model eligibility controls sending, not the trusted identity.
        // A removed owner may still authorize a matching compaction handoff.
        let owner = (!entry.establishing || eligible.contains(&entry.provider))
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
        let boundary_sequence = entry.boundary_sequence;
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
            boundary_sequence,
            owner,
            handoff,
            completed_epoch: Mutex::new(None),
            fresh: fresh || entry_establishing,
            preferred: vec![],
            receivers: eligible.to_vec(),
            ordinary_receivers: eligible.to_vec(),
            initial_bound: std::sync::atomic::AtomicBool::new(false),
            failure_fallback: std::sync::atomic::AtomicBool::new(false),
            strict: false,
            confirmed: true,
            intake: if entry_establishing && first_turn {
                Intake::NewThread
            } else {
                Intake::Ordinary
            },
            request_only: false,
            routing_log: Mutex::new(None),
        }))
    }
    #[cfg(test)]
    pub fn prepare_policy(
        self: &Arc<Self>,
        session: String,
        hints: Option<&super::replay::RequestHints>,
        can_handoff: bool,
        eligible: &[super::model::Provider],
    ) -> Option<Arc<Lease>> {
        self.prepare_request(
            session,
            hints,
            can_handoff,
            eligible,
            &super::routing::Requirement::Resource,
            None,
        )
    }
    pub fn prepare_request(
        self: &Arc<Self>,
        session: String,
        hints: Option<&super::replay::RequestHints>,
        can_handoff: bool,
        queued: &[super::model::Provider],
        requirement: &super::routing::Requirement,
        pinned: Option<&str>,
    ) -> Option<Arc<Lease>> {
        if hints.is_some_and(|h| h.auxiliary) {
            return None;
        }
        let eligible: Vec<_> = queued
            .iter()
            .filter(|p| requirement.allows(p.allowed_models.as_deref()))
            .cloned()
            .collect();
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
            Some(RequestPolicy {
                queued,
                requirement,
                pinned,
            }),
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
        let Ok(state) = self.state.lock() else {
            return vec![];
        };
        let (active, _) = capacity.counts();
        let mut result = std::collections::BTreeMap::new();
        for entry in state
            .entries
            .values()
            .filter(|e| !e.establishing && now().saturating_sub(e.updated) < RETENTION_MS)
        {
            let position = queued
                .iter()
                .position(|p| p.id == entry.provider)
                .unwrap_or(queued.len());
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
                } else if entry.claim.is_some() {
                    "claim_in_flight"
                } else if entry.boundary_fresh {
                    "awaiting_request"
                } else {
                    "awaiting_compaction"
                };
                result
                    .entry(provider.id.clone())
                    .and_modify(|current| {
                        let rank = |phase| match phase {
                            "capacity_full" => 3,
                            "claim_in_flight" => 2,
                            "awaiting_request" => 1,
                            _ => 0,
                        };
                        if rank(phase) > rank(*current) {
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
            && self.state.lock().ok().is_some_and(|state| {
                state.entries.get(session).is_some_and(|entry| {
                    entry.claim.is_none()
                        && entry.boundary_fresh
                        && entry.boundary.as_deref() == input
                })
            })
    }
    pub fn model_detour(
        &self,
        session: &str,
        queued: &[super::model::Provider],
        requirement: &super::routing::Requirement,
    ) -> bool {
        self.state.lock().ok().is_some_and(|state| {
            state
                .entries
                .get(session)
                .is_some_and(|entry| model_detour(entry, queued, requirement))
        })
    }
    pub fn owner(&self, session: &str) -> Option<String> {
        self.state
            .lock()
            .ok()?
            .entries
            .get(session)
            .filter(|e| !e.establishing && now().saturating_sub(e.updated) < RETENTION_MS)
            .map(|e| e.provider.clone())
    }
    pub fn error(&self) -> Option<String> {
        match self.state.lock() {
            Ok(state) => state.error.clone(),
            Err(_) => Some(POISONED.into()),
        }
    }
}

impl Lease {
    pub fn request_only(&self) -> bool {
        self.request_only
    }
    pub fn session_digest(&self) -> &str {
        &self.session
    }
    pub fn establishing(&self) -> bool {
        !self.request_only
            && self.registry.state.lock().ok().is_some_and(|state| {
                state
                    .entries
                    .get(&self.session)
                    .is_some_and(|e| e.establishing)
            })
    }
    pub fn decision(
        &self,
        hints: Option<&super::replay::RequestHints>,
        provider: &str,
    ) -> crate::usage::model::RoutingDecision {
        use crate::usage::model::RoutingDecision as D;
        if self.request_only {
            return D::ModelDetourOwnerPreserved;
        }
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
            let Ok(state) = self.registry.state.lock() else {
                return D::ExistingThread;
            };
            if state.entries.get(&self.session).is_some_and(|entry| {
                entry.claim.is_some() && entry.boundary.as_deref() == Some(window)
            }) {
                return D::BoundaryInFlight;
            }
            if let Some(entry) = state.entries.get(&self.session) {
                if !entry.boundary_fresh && entry.consumed_boundary.as_deref() == Some(window) {
                    return D::BoundaryConsumed;
                }
                if entry.boundary.is_none() && entry.consumed_boundary.is_none() {
                    return D::BoundaryNotObserved;
                }
            }
            if state.entries.get(&self.session).is_some_and(|entry| {
                entry.boundary.as_deref() != Some(window)
                    && entry.consumed_boundary.as_deref() != Some(window)
            }) {
                return D::CompactionWindowMismatch;
            }
        } else if self.registry.state.lock().ok().is_some_and(|state| {
            state
                .entries
                .get(&self.session)
                .is_some_and(|entry| entry.boundary.is_some())
        }) {
            return D::CompactionWindowMissing;
        }
        D::ExistingThread
    }
    pub fn attach_routing_log(&self, log: Arc<super::routing_log::RoutingLog>) {
        if let Ok(mut current) = self.routing_log.lock() {
            *current = Some(log);
        }
    }
    pub fn boundary_state(&self, input: Option<&str>) -> crate::usage::model::BoundaryState {
        use crate::usage::model::BoundaryState as B;
        let Ok(state) = self.registry.state.lock() else {
            return B::Unknown;
        };
        let Some(entry) = state.entries.get(&self.session) else {
            return B::NotObserved;
        };
        if entry.boundary.is_none() && entry.consumed_boundary.is_none() {
            B::NotObserved
        } else if input.is_some_and(|value| {
            entry.boundary.as_deref() != Some(value)
                && entry.consumed_boundary.as_deref() != Some(value)
        }) {
            B::Mismatch
        } else if entry.claim.is_some() {
            B::Claimed
        } else if entry.boundary_fresh {
            B::Fresh
        } else {
            B::Consumed
        }
    }
    pub(super) fn admission_policy(&self) -> (Intake, Option<String>) {
        let Ok(state) = self.registry.state.lock() else {
            return (Intake::Unavailable, None);
        };
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
        !self.request_only
            && !self.handoff
            && self.registry.state.lock().ok().is_some_and(|state| {
                state
                    .entries
                    .get(&self.session)
                    .is_some_and(|entry| !entry.establishing && entry.provider == provider)
            })
    }
    /// The first actual admission, not the first successful response, establishes
    /// ownership. Competing first requests release their slot and recheck it.
    #[cfg(test)]
    pub fn admitted(&self, provider: &str) -> bool {
        let Ok(mut state) = self.registry.state.lock() else {
            return false;
        };
        self.admit_locked(&mut state, provider)
    }
    /// Recheck the live queue and identity while both snapshots are locked.
    /// A changed model policy cannot turn a pending writable lease into an
    /// implicit transfer. Reject that stale admission before sending any bytes;
    /// a new request will prepare against the current owner and model policy.
    pub fn admit_request(
        &self,
        gateway: &super::Gateway,
        provider: &super::model::Provider,
        requirement: &super::routing::Requirement,
        pinned: bool,
    ) -> storage::Result<bool> {
        use crate::{errors::code, storage::AppError};
        let config = gateway.request_state()?;
        let current = config
            .store
            .providers
            .iter()
            .find(|p| p.id == provider.id && (p.queued || pinned))
            .ok_or_else(|| AppError::new(code::NO_ELIGIBLE_PROVIDER, "供应商队列已变化，请重试"))?;
        if current.version != provider.version {
            return Err(AppError::new(code::PROVIDER, "供应商配置已变化，请重试"));
        }
        if !requirement.allows(current.allowed_models.as_deref()) {
            return Err(AppError::new(requirement.code(), requirement.message()));
        }
        let mut state = self
            .registry
            .state
            .lock()
            .map_err(|_| AppError::new(code::CONVERSATION_OWNERSHIP, POISONED))?;
        let entry = state.entries.get(&self.session);
        if !self.request_only && !entry.is_some_and(|entry| self.current(entry)) {
            return Err(AppError::new(
                code::CONVERSATION_OWNERSHIP,
                "线程归属或压缩边界已变化，请重试本次请求",
            ));
        }
        if !self.request_only
            && entry.is_some_and(|e| model_detour(e, &config.store.providers, requirement))
        {
            return Err(AppError::new(
                code::NO_ELIGIBLE_PROVIDER,
                "线程归属或模型白名单已变化，请重试本次请求",
            ));
        }
        if self.handoff && self.owner.as_deref() != Some(provider.id.as_str()) {
            let before_owner = config
                .store
                .providers
                .iter()
                .filter(|p| p.queued)
                .take_while(|p| self.owner.as_deref() != Some(p.id.as_str()))
                .any(|p| p.id == provider.id);
            let fallback = self
                .failure_fallback
                .load(std::sync::atomic::Ordering::Acquire);
            if (fallback && !accepts(current, Intake::Compacted))
                || (!fallback && (!current.handoff_after_compaction || !before_owner))
            {
                return Err(AppError::new(
                    code::NO_ELIGIBLE_PROVIDER,
                    "压缩接收策略或队列顺序已变化，请重试本次请求",
                ));
            }
        }
        if self.request_only
            && !pinned
            && !entry.is_some_and(|e| !e.establishing && e.provider == provider.id)
            && !accepts(current, Intake::Ordinary)
        {
            return Err(AppError::new(
                code::NO_ELIGIBLE_PROVIDER,
                "供应商不再接收普通请求",
            ));
        }
        Ok(self.admit_locked(&mut state, &provider.id))
    }
    fn admit_locked(&self, state: &mut State, provider: &str) -> bool {
        use std::sync::atomic::Ordering;
        if self.request_only {
            // Expiration/removal must not upgrade the lease or cause an
            // endless first-admission retry. Live receiver policy was checked
            // above; this request can never recreate the missing owner.
            return true;
        }
        if !state
            .entries
            .get(&self.session)
            .is_some_and(|entry| self.current(entry))
        {
            return false;
        }
        if !self.fresh || self.initial_bound.load(Ordering::Acquire) {
            return true;
        }
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
            self.registry.save(state, true);
        }
        state.error.is_none()
    }
    #[cfg(test)]
    pub fn candidates(&self, routes: Vec<super::Route>) -> Vec<super::Route> {
        self.candidates_for(routes, &super::routing::Requirement::Unknown)
    }
    pub fn candidates_for(
        &self,
        routes: Vec<super::Route>,
        requirement: &super::routing::Requirement,
    ) -> Vec<super::Route> {
        self.candidates_for_observed(routes, requirement, true)
    }
    fn candidates_for_observed(
        &self,
        mut routes: Vec<super::Route>,
        requirement: &super::routing::Requirement,
        observe: bool,
    ) -> Vec<super::Route> {
        if self.registry.state.is_poisoned() {
            return vec![];
        }
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
        if bound.is_some() && observe {
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
                        || (!self.fresh && super::routing::capacity_cooling(&health))
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
        routes: Vec<super::Route>,
        requirement: &super::routing::Requirement,
    ) -> Vec<super::Route> {
        self.failure_fallback
            .store(true, std::sync::atomic::Ordering::Release);
        self.failure_candidates(routes, requirement, true)
    }
    /// The log's preview must never claim a boundary, bind an owner, or grant
    /// failure-fallback authority before the scheduler actually takes that path.
    pub(super) fn preview_after_failure(
        &self,
        routes: Vec<super::Route>,
        requirement: &super::routing::Requirement,
    ) -> Vec<super::Route> {
        self.failure_candidates(routes, requirement, false)
    }
    fn failure_candidates(
        &self,
        mut routes: Vec<super::Route>,
        requirement: &super::routing::Requirement,
        observe: bool,
    ) -> Vec<super::Route> {
        if self.registry.state.is_poisoned() {
            return vec![];
        }
        if !self.handoff {
            return self.candidates_for_observed(routes, requirement, observe);
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
    fn current(&self, entry: &Entry) -> bool {
        entry.epoch == self.epoch
            && entry.boundary_sequence == self.boundary_sequence
            && (!self.handoff || entry.claim == Some(self.sequence))
    }
    pub fn output_started(&self, provider: &str) {
        if self.request_only {
            return;
        }
        let Ok(mut state) = self.registry.state.lock() else {
            return;
        };
        let Some(entry) = state.entries.get_mut(&self.session) else {
            return;
        };
        if !self.current(entry) {
            return;
        }
        let mut changed = false;
        if entry.establishing {
            entry.establishing = false;
            entry.provider = provider.into();
            changed = true;
        }
        if entry.boundary_fresh && self.sequence > entry.boundary_sequence {
            entry.consumed_boundary = entry.boundary.take();
            entry.boundary_fresh = false;
            if let Ok(log) = self.routing_log.lock() {
                if let Some(log) = log.as_ref() {
                    log.outcome(crate::events::RoutingOutcome::WindowConsumed);
                }
            }
            // Another turn delivered real output on this generation. Its old
            // pending handoff must not later commit against that spent window.
            if entry.claim != Some(self.sequence) {
                entry.claim = None;
            }
            changed = true;
        }
        if changed {
            self.registry.save(&mut state, true);
        }
    }
    pub fn complete(&self, provider: &str, boundary: Option<&str>) {
        if self.request_only {
            return;
        }
        let Ok(mut completed) = self.completed_epoch.lock() else {
            return;
        };
        let first = completed.is_none();
        if !first && boundary.is_none() {
            return;
        }
        let Ok(mut state) = self.registry.state.lock() else {
            return;
        };
        let Some(entry) = state.entries.get_mut(&self.session) else {
            return;
        };
        if !first && boundary == entry.boundary.as_deref() {
            return;
        }
        if entry.epoch != completed.unwrap_or(self.epoch) {
            return;
        }
        if entry.boundary_sequence != self.boundary_sequence
            && (first || entry.boundary_sequence != self.sequence)
        {
            return;
        }
        if first && self.handoff && entry.claim != Some(self.sequence) {
            return;
        }
        let moved = first && entry.provider != provider;
        let previous = entry.provider.clone();
        let changed =
            (first && (moved || entry.establishing || self.handoff)) || boundary.is_some();
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
        let mut registered = false;
        if let Some(boundary) = boundary.filter(|_| self.sequence >= entry.boundary_sequence) {
            if entry.consumed_boundary.as_deref() != Some(boundary) {
                entry.boundary = Some(boundary.into());
                entry.boundary_sequence = self.sequence;
                entry.boundary_fresh = true;
                entry.claim = None;
                registered = true;
            }
        }
        *completed = Some(entry.epoch);
        entry.updated = now();
        if moved || registered {
            let mut record = self.routing_log.lock().ok().and_then(|log| {
                log.as_ref().and_then(|log| {
                    log.take(if moved {
                        crate::events::RoutingOutcome::OwnershipSaved
                    } else {
                        crate::events::RoutingOutcome::BoundaryRegistered
                    })
                })
            });
            if moved && self.handoff && record.is_none() {
                let client = super::locks::published(&self.registry.notifications)
                    .as_ref()
                    .map(|(client, _)| *client);
                record = Some(crate::events::Record::new(
                    client,
                    Some(provider),
                    None,
                    crate::events::Reason::ThreadHandoff,
                    crate::events::Action::Routed,
                    None,
                    None,
                ));
            }
            if let Some(mut record) = record {
                record.details.handoff_scope = Some(storage::digest(
                    format!("{}:{}", self.session, self.sequence).as_bytes(),
                ));
                if moved && self.handoff {
                    record.reason = crate::events::Reason::ThreadHandoff;
                    record.error_code = Some(crate::errors::ErrorCode::ThreadHandoff);
                }
                if moved {
                    record.action = crate::events::Action::Routed;
                }
                let version = state.save_version.saturating_add(1);
                state.notifications.push(PendingReport {
                    version,
                    record,
                    handoff: (moved && self.handoff).then(|| (previous, provider.into())),
                });
            }
        }
        self.registry.save(&mut state, changed);
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        if !self.handoff {
            return;
        }
        // A destructor must not panic while unwinding another failed request.
        // Never repair or persist ownership from a partially edited ledger.
        let Ok(mut state) = self.registry.state.lock() else {
            return;
        };
        if let Some(entry) = state.entries.get_mut(&self.session) {
            if self.current(entry) {
                let released = entry.boundary_fresh;
                entry.claim = None;
                if released {
                    if let Some(record) = self.routing_log.lock().ok().and_then(|log| {
                        log.as_ref()
                            .and_then(|log| log.take(crate::events::RoutingOutcome::WindowReleased))
                    }) {
                        let version = state.save_version.saturating_add(1);
                        state.notifications.push(PendingReport {
                            version,
                            record,
                            handoff: None,
                        });
                    }
                }
                self.registry.save(&mut state, true);
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

#[cfg(test)]
#[path = "compaction_poison_tests.rs"]
mod poison_tests;
