//! Attempt policy and delivery settlement shared by the transports. HTTP body
//! error frames and native WebSocket close frames keep their own wire encoding.
//! Protocol::finish remains the single usage/terminal finalizer.
use super::{
    protocol::{Delivery, DownstreamError, Protocol},
    replay_policy,
};
use crate::errors::code;
use std::sync::Arc;

pub(super) fn transport_retry(received: bool, eligible: bool, failure: &str) -> bool {
    replay_policy::transport_retry(received, eligible) && failure != code::FIRST_BYTE_TIMEOUT
}

/// The current queue order without hard-denied provider versions, and the
/// routes among it that speak the request path.
pub(super) fn queue(
    gateway: &super::Gateway,
    pinned: Option<&str>,
    hard_denied: &std::collections::HashSet<(String, String)>,
    path: &str,
) -> (Vec<String>, std::collections::HashMap<String, super::Route>) {
    let mut ids = gateway.routing_ids(pinned);
    ids.retain(|id| {
        gateway
            .route(id)
            .is_some_and(|route| !hard_denied.contains(&(id.clone(), route.provider.version)))
    });
    let routes = ids
        .iter()
        .filter_map(|id| {
            gateway
                .route(id)
                .filter(|route| route.accepts_protocol(path))
                .map(|route| (id.clone(), route))
        })
        .collect();
    (ids, routes)
}

pub(super) struct DeliveryGuard {
    pub delivery: Arc<Delivery>,
    pub complete: bool,
}
impl Drop for DeliveryGuard {
    fn drop(&mut self) {
        if !self.complete {
            self.delivery.dropped();
        }
    }
}

#[derive(Clone, Copy)]
pub(super) enum DownstreamEnd {
    Closed,
    Timeout(u64),
    PeerClosed { error: u8, writing: bool },
}
impl DownstreamEnd {
    pub fn record(self, delivery: &Delivery, protocol: &mut Protocol) {
        match self {
            Self::Timeout(seconds) => {
                delivery.event("downstream_write_timeout");
                let _ = delivery.error.compare_exchange(
                    0,
                    DownstreamError::WriteTimeout.into(),
                    std::sync::atomic::Ordering::AcqRel,
                    std::sync::atomic::Ordering::Relaxed,
                );
                protocol.timeout("downstream_write", seconds);
                delivery.settle();
            }
            Self::Closed if !delivery.is_settled() => delivery.dropped(),
            Self::Closed => (),
            Self::PeerClosed { error, writing } => {
                delivery.failed(if error != 0 {
                    error
                } else if writing {
                    DownstreamError::ConnectionError.into()
                } else {
                    DownstreamError::Closed.into()
                });
            }
        }
    }
}
