//! Shared gateway ports prevent self-routing and cross-client loops.
use super::{listener::Listener, ClientId, Gateway, Shared};
use crate::storage::Result;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock, Weak};

#[derive(Clone)]
pub(super) struct TokenEntry {
    pub client: ClientId,
    pub token: String,
    pub shared: Weak<Shared>,
}

pub struct Registry {
    pub mutation: Mutex<()>,
    tokens: RwLock<Arc<Vec<TokenEntry>>>,
    pub ports: Arc<Mutex<Vec<u16>>>,
    pub lifecycle: tokio::sync::Mutex<()>,
    pub listener: Listener,
}
impl Registry {
    pub fn new(data: PathBuf, port: Option<u16>) -> Result<Self> {
        let listener = Listener::load(data.join("gateway-listener.json"), port)?;
        let port = listener.view().port;
        Ok(Self {
            mutation: Mutex::new(()),
            tokens: RwLock::new(Arc::new(Vec::new())),
            ports: Arc::new(Mutex::new(vec![port])),
            lifecycle: tokio::sync::Mutex::new(()),
            listener,
        })
    }
    pub fn gateway(&self, client: ClientId) -> Option<Gateway> {
        self.tokens_snapshot()
            .iter()
            .filter_map(|entry| entry.shared.upgrade())
            .find(|g| g.client == client)
            .map(Gateway)
    }
    pub(super) fn gateways(&self) -> Vec<Arc<Shared>> {
        self.tokens_snapshot()
            .iter()
            .filter_map(|entry| entry.shared.upgrade())
            .collect()
    }
    pub(super) fn tokens_snapshot(&self) -> Arc<Vec<TokenEntry>> {
        // The guarded value is only ever replaced with a fully built immutable
        // snapshot. A poisoned lock therefore still contains a valid snapshot.
        self.tokens
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
    fn update_tokens(&self, edit: impl FnOnce(&mut Vec<TokenEntry>)) {
        let mut current = self
            .tokens
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut next = current.as_ref().clone();
        // Do not upgrade and drop a final Shared while holding this write lock:
        // Shared::drop unregisters itself. A weak entry never keeps it alive.
        next.retain(|entry| entry.shared.strong_count() > 0);
        edit(&mut next);
        *current = Arc::new(next);
    }
    /// Called under the shared lifecycle lock before an activation transaction.
    /// Never rotate a peer's credential or publish this value outside that transaction.
    pub fn unique_local_token(&self, client: ClientId, current: &str) -> String {
        let snapshot = self.tokens_snapshot();
        let mut candidate = current.to_owned();
        while candidate.is_empty()
            || snapshot.iter().any(|entry| {
                entry.client != client
                    && entry.shared.strong_count() > 0
                    && entry.token == candidate
            })
        {
            candidate = super::model::new_local_token();
        }
        candidate
    }
    pub fn changed(&self) {
        for gateway in self.gateways() {
            let _ = gateway.events.send(());
        }
    }
    pub fn register(&self, shared: &Arc<Shared>, token: &str) {
        self.update_tokens(|entries| {
            entries.retain(|entry| entry.client != shared.client);
            entries.push(TokenEntry {
                client: shared.client,
                token: token.to_owned(),
                shared: Arc::downgrade(shared),
            });
        });
        self.update_ports();
    }
    /// Publish only after the private store transaction has committed or reloaded.
    pub(super) fn publish_token(&self, shared: &Arc<Shared>, token: &str) {
        let identity = Arc::as_ptr(shared);
        self.update_tokens(|entries| {
            if let Some(entry) = entries
                .iter_mut()
                .find(|entry| entry.shared.as_ptr() == identity)
            {
                entry.token = token.to_owned();
            }
        });
    }
    pub(super) fn unregister(&self, shared: &Shared) {
        self.update_tokens(|entries| {
            entries.retain(|entry| !std::ptr::eq(entry.shared.as_ptr(), shared));
        });
    }
    pub fn update_ports(&self) {
        // Published as a whole after a listener transaction; never mutate in place.
        *super::locks::published(&self.ports) = vec![self.listener.view().port];
    }
}

#[cfg(test)]
#[path = "registry_poison_tests.rs"]
mod poison_tests;
