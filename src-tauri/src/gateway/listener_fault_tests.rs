use super::super::{ClientId, Gateway};
use super::*;
use std::{
    fs,
    net::{Ipv4Addr, TcpListener as StdTcpListener},
    panic::{catch_unwind, AssertUnwindSafe},
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
};

fn free_loopback_port() -> u16 {
    StdTcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn listener_at(path: &Path, port: u16) -> Listener {
    Listener::load(path.to_owned(), Some(port)).unwrap()
}

fn poison_listener_state(listener: &Listener, update: impl FnOnce(&mut super::State)) {
    let panic = catch_unwind(AssertUnwindSafe(|| {
        let mut state = listener.state.lock().unwrap();
        update(&mut state);
        panic!("inject listener state poison");
    }));
    assert!(panic.is_err());
    assert!(listener.state.is_poisoned());
}

fn poisoned_state(listener: &Listener) -> std::sync::MutexGuard<'_, super::State> {
    match listener.state.lock() {
        Err(poisoned) => poisoned.into_inner(),
        Ok(_) => panic!("listener state should remain poisoned"),
    }
}

#[tokio::test]
async fn poisoned_queries_and_port_changes_fail_before_prepare_binds_or_commits() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("gateway-listener.json");
    let port = free_loopback_port();
    let replacement_port = free_loopback_port();
    let listener = listener_at(&path, port);
    let before = listener.view().unwrap();
    let saved_config = fs::read(&path).unwrap();

    poison_listener_state(&listener, |_| {});

    assert!(listener.view().is_err());
    assert!(listener.request_port().is_err());
    assert!(listener
        .set_port(replacement_port, &before.revision)
        .is_err());
    assert!(listener.prepare_at(port, port).await.is_err());
    assert_eq!(fs::read(&path).unwrap(), saved_config);
    assert!(listener.state.is_poisoned());

    let _available = StdTcpListener::bind((Ipv4Addr::LOCALHOST, port))
        .expect("poisoned prepare must not bind or retain the listener port");
}

#[tokio::test]
async fn poison_after_prepare_skips_managed_config_commit_and_drops_prepared_socket() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("gateway-listener.json");
    let port = free_loopback_port();
    let listener = listener_at(&path, port);
    let original_listener_config = fs::read(&path).unwrap();
    let prepared = listener.prepare_at(port, port).await.unwrap();
    let prepared_port = prepared.port;
    let socket = prepared
        .socket
        .expect("prepare should hold the selected port");
    let gateway = Gateway::new(temp.path().join("registry-data")).unwrap();

    let managed_config = temp.path().join("fixture-managed-config.json");
    fs::write(&managed_config, b"fixture-original-config").unwrap();
    poison_listener_state(&listener, |_| {});
    let commit_called = AtomicBool::new(false);

    let result = listener.activate_with(
        ClientId::Codex,
        Some(socket),
        gateway.0.registry.clone(),
        || {
            fs::write(&managed_config, b"fixture-managed-config")
                .expect("fixture commit write should succeed");
            commit_called.store(true, Ordering::SeqCst);
            Ok(())
        },
    );

    assert!(result.is_err());
    assert!(!commit_called.load(Ordering::SeqCst));
    assert_eq!(
        fs::read(&managed_config).unwrap(),
        b"fixture-original-config"
    );
    assert_eq!(fs::read(&path).unwrap(), original_listener_config);
    {
        let state = poisoned_state(&listener);
        assert!(state.clients.is_empty());
        assert!(state.shutdown.is_none());
        assert!(state.task.is_none());
    }
    assert!(listener.state.is_poisoned());

    let _available = StdTcpListener::bind((Ipv4Addr::LOCALHOST, prepared_port))
        .expect("rejected activation must drop the prepared socket");
}

#[tokio::test]
async fn poisoned_deactivate_reclaims_task_and_socket_without_clearing_poison() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("gateway-listener.json");
    let port = free_loopback_port();
    let listener = listener_at(&path, port);
    let prepared = listener.prepare_at(port, port).await.unwrap();
    let prepared_port = prepared.port;
    let socket = prepared
        .socket
        .expect("prepare should hold the selected port");
    let gateway = Gateway::new(temp.path().join("registry-data")).unwrap();

    listener
        .activate_with(
            ClientId::Codex,
            Some(socket),
            gateway.0.registry.clone(),
            || Ok(()),
        )
        .unwrap();
    {
        let state = listener.state.lock().unwrap();
        assert!(state.clients.contains(&ClientId::Codex));
        assert!(state.shutdown.is_some());
        assert!(state.task.is_some());
    }

    // Deliberately make the client list stale while preserving the live handles.
    poison_listener_state(&listener, |state| state.clients.clear());

    assert!(listener.deactivate(ClientId::Codex).await.is_err());
    {
        let state = poisoned_state(&listener);
        assert!(state.clients.is_empty());
        assert!(state.shutdown.is_none());
        assert!(state.task.is_none());
    }
    assert!(listener.state.is_poisoned());

    let _available = StdTcpListener::bind((Ipv4Addr::LOCALHOST, prepared_port))
        .expect("deactivate should await the real listener task and release its socket");
}

#[tokio::test]
async fn gateway_exit_releases_listener_resources_after_poison() {
    let temp = tempfile::tempdir().unwrap();
    let gateway = Gateway::new(temp.path().join("gateway-data")).unwrap();
    let port = free_loopback_port();
    let listener_before = gateway.try_listener_view().unwrap();
    gateway
        .set_listener_port(port, &listener_before.revision)
        .unwrap();

    let listener = &gateway.0.registry.listener;
    let prepared = listener.prepare_at(port, port).await.unwrap();
    let prepared_port = prepared.port;
    listener
        .activate(ClientId::Codex, prepared.socket, gateway.0.registry.clone())
        .unwrap();
    poison_listener_state(listener, |state| state.clients.clear());

    assert!(gateway.stop_for_exit().await.is_err());
    {
        let state = poisoned_state(listener);
        assert!(state.shutdown.is_none());
        assert!(state.task.is_none());
    }
    assert!(listener.state.is_poisoned());

    let _available = StdTcpListener::bind((Ipv4Addr::LOCALHOST, prepared_port))
        .expect("gateway exit should reap the listener despite poisoned metadata");
}

#[test]
fn gateway_queries_and_port_updates_surface_listener_poison() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("gateway-data");
    let listener_config = data.join("gateway-listener.json");
    let gateway = Gateway::new(data).unwrap();
    let listener_before = gateway.try_listener_view().unwrap();
    let saved_config = fs::read(&listener_config).unwrap();
    let replacement_port = free_loopback_port();

    poison_listener_state(&gateway.0.registry.listener, |_| {});

    assert!(gateway.try_view().is_err());
    assert!(gateway.try_listener_view().is_err());
    assert!(gateway
        .set_listener_port(replacement_port, &listener_before.revision)
        .is_err());
    assert_eq!(fs::read(&listener_config).unwrap(), saved_config);
    assert!(gateway.0.registry.listener.state.is_poisoned());
}
