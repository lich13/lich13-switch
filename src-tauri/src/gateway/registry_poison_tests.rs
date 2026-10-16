use super::*;
use std::{
    panic::{catch_unwind, AssertUnwindSafe},
    sync::Arc,
};

fn route(
    path: &str,
    token: &str,
    registry: &Registry,
) -> std::result::Result<(super::super::ClientId, String), super::super::listener::RouteError> {
    let uri = path.parse::<hyper::Uri>().unwrap();
    let mut headers = hyper::HeaderMap::new();
    headers.insert(
        hyper::header::AUTHORIZATION,
        format!("Bearer {token}").parse().unwrap(),
    );
    super::super::listener::route(&uri, &headers, registry)
        .map(|(client, uri)| (client, uri.to_string()))
}

#[test]
fn poisoned_token_lock_keeps_exact_client_snapshot_and_routes_no_peer_token() {
    use super::super::{ClientId, Gateway};

    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("gateway-data");
    let codex = Gateway::new(data.clone()).unwrap();
    let claude = codex.companion(data.join("claude")).unwrap();
    let registry = &codex.0.registry;
    let codex_token = codex.0.inner.lock().unwrap().store.local_token.clone();
    let claude_token = claude.0.inner.lock().unwrap().store.local_token.clone();
    assert_ne!(codex_token, claude_token);

    let before = registry.tokens_snapshot();
    assert_eq!(before.len(), 2);
    let replacement = Arc::new(before.as_ref().clone());
    let expected = Arc::clone(&replacement);
    let injected = catch_unwind(AssertUnwindSafe(|| {
        let mut guard = registry.tokens.write().unwrap();
        *guard = Arc::clone(&replacement);
        panic!("inject token snapshot lock poison after whole-value publication");
    }));
    assert!(injected.is_err());
    assert!(registry.tokens.is_poisoned());

    let snapshot = registry.tokens_snapshot();
    assert!(Arc::ptr_eq(&snapshot, &expected));
    assert_eq!(
        snapshot
            .iter()
            .find(|entry| entry.token == codex_token)
            .map(|entry| entry.client),
        Some(ClientId::Codex)
    );
    assert_eq!(
        snapshot
            .iter()
            .find(|entry| entry.token == claude_token)
            .map(|entry| entry.client),
        Some(ClientId::Claude)
    );
    assert_eq!(
        registry.gateway(ClientId::Codex).unwrap().client_id(),
        ClientId::Codex
    );
    assert_eq!(
        registry.gateway(ClientId::Claude).unwrap().client_id(),
        ClientId::Claude
    );

    assert_eq!(
        route("/v1/responses?fixture=1", &codex_token, registry).unwrap(),
        (ClientId::Codex, "/v1/responses?fixture=1".to_owned())
    );
    assert_eq!(
        route("/claude/v1/messages?fixture=2", &claude_token, registry).unwrap(),
        (ClientId::Claude, "/v1/messages?fixture=2".to_owned())
    );
    assert_eq!(
        route("/claude/v1/messages", &codex_token, registry),
        Err(super::super::listener::RouteError::Unauthorized)
    );
    assert_eq!(
        route("/codex/v1/responses", &claude_token, registry),
        Err(super::super::listener::RouteError::Unauthorized)
    );
}
