//! Shared gateway ports prevent self-routing and cross-client loops.
use super::{listener::Listener, ClientId, Gateway, Shared};
use crate::storage::Result;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, Weak};
pub struct Registry {
    pub mutation: Mutex<()>,
    pub gateways: Mutex<Vec<Weak<Shared>>>,
    pub ports: Arc<Mutex<Vec<u16>>>,
    pub lifecycle: tokio::sync::Mutex<()>,
    pub listener: Listener,
}
impl Registry {
    pub fn new(data: PathBuf, port: u16) -> Result<Self> {
        let listener = Listener::load(data.join("gateway-listener.json"), port)?;
        let port = listener.view().port;
        Ok(Self {
            mutation: Mutex::new(()),
            gateways: Mutex::new(Vec::new()),
            ports: Arc::new(Mutex::new(vec![port])),
            lifecycle: tokio::sync::Mutex::new(()),
            listener,
        })
    }
    pub fn gateway(&self, client: ClientId) -> Option<Gateway> {
        self.gateways
            .lock()
            .unwrap()
            .iter()
            .filter_map(Weak::upgrade)
            .find(|g| g.client == client)
            .map(Gateway)
    }
    pub fn changed(&self) {
        for gateway in self
            .gateways
            .lock()
            .unwrap()
            .iter()
            .filter_map(Weak::upgrade)
        {
            let _ = gateway.events.send(());
        }
    }
    pub fn register(&self, shared: &Arc<Shared>) {
        self.gateways.lock().unwrap().push(Arc::downgrade(shared));
        self.update_ports();
    }
    pub fn update_ports(&self) {
        *self.ports.lock().unwrap() = vec![self.listener.view().port];
    }
}
