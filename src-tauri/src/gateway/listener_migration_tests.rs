use super::Listener;
use serde_json::{json, Value};
use std::{
    fs,
    net::{Ipv4Addr, TcpListener as StdTcpListener},
    path::Path,
};

const NEW_DEFAULT_PORT: u16 = 43721;

fn held_port() -> StdTcpListener {
    StdTcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap()
}

fn free_port_excluding(excluded: &[u16]) -> u16 {
    loop {
        let listener = held_port();
        let port = listener.local_addr().unwrap().port();
        if !excluded.contains(&port) {
            return port;
        }
    }
}

fn distinct_free_ports(count: usize) -> Vec<u16> {
    let mut ports = Vec::with_capacity(count);
    while ports.len() < count {
        let port = free_port_excluding(&ports);
        ports.push(port);
    }
    ports
}

fn config_bytes(version: u32, port: u16) -> Vec<u8> {
    serde_json::to_vec_pretty(&json!({ "version": version, "port": port })).unwrap()
}

fn write_config(path: &Path, version: u32, port: u16) {
    fs::write(path, config_bytes(version, port)).unwrap();
}

fn read_config(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

fn assert_config(path: &Path, version: u32, port: u16) {
    let config = read_config(path);
    assert_eq!(config["version"], version);
    assert_eq!(config["port"], port);
}

#[test]
fn missing_config_creates_v2_with_new_default_or_inherited_legacy_port() {
    let temp = tempfile::tempdir().unwrap();
    let no_store_path = temp.path().join("no-store.json");
    let no_store = Listener::load(no_store_path.clone(), None).unwrap();

    assert_eq!(no_store.view().port, NEW_DEFAULT_PORT);
    assert_config(&no_store_path, 2, NEW_DEFAULT_PORT);

    let legacy_port = free_port_excluding(&[NEW_DEFAULT_PORT]);
    let legacy_path = temp.path().join("legacy-store.json");
    let inherited = Listener::load(legacy_path.clone(), Some(legacy_port)).unwrap();

    assert_eq!(inherited.view().port, legacy_port);
    assert_config(&legacy_path, 2, legacy_port);
}

#[tokio::test]
async fn default_migration_keeps_v1_until_the_new_port_is_bound_then_persists_v2() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("gateway-listener.json");
    let [new_default, legacy_default] = distinct_free_ports(2).try_into().unwrap();
    write_config(&path, 1, legacy_default);
    let listener = Listener::load(path.clone(), Some(legacy_default)).unwrap();

    assert_config(&path, 1, legacy_default);
    let prepared = listener
        .prepare_at(new_default, legacy_default)
        .await
        .unwrap();

    assert_eq!(prepared.port, new_default);
    let notice = prepared
        .notice
        .as_deref()
        .expect("successful default migration should explain the port change");
    assert!(
        notice.contains(&format!("共享端口已迁移到 {new_default}")),
        "migration notice should include the selected port: {notice}"
    );
    assert!(
        notice.contains("请重新打开已启动的 Codex / Claude Code / Grok"),
        "migration notice should tell users to reopen clients: {notice}"
    );
    assert_eq!(
        prepared
            .socket
            .as_ref()
            .unwrap()
            .local_addr()
            .unwrap()
            .port(),
        new_default
    );
    assert_config(&path, 2, new_default);
}

#[tokio::test]
async fn custom_v1_port_is_preserved_during_migration() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("gateway-listener.json");
    let [new_default, legacy_default, custom_port] = distinct_free_ports(3).try_into().unwrap();
    write_config(&path, 1, custom_port);
    let listener = Listener::load(path.clone(), None).unwrap();

    let prepared = listener
        .prepare_at(new_default, legacy_default)
        .await
        .unwrap();

    assert_eq!(prepared.port, custom_port);
    assert!(
        prepared.notice.is_none(),
        "a custom v1 port should be preserved without a migration notice"
    );
    assert_eq!(
        prepared
            .socket
            .as_ref()
            .unwrap()
            .local_addr()
            .unwrap()
            .port(),
        custom_port
    );
    assert_config(&path, 2, custom_port);
}

#[tokio::test]
async fn occupied_new_default_falls_back_to_legacy_default_with_notice() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("gateway-listener.json");
    let occupied_new_default = held_port();
    let new_default = occupied_new_default.local_addr().unwrap().port();
    let legacy_default = free_port_excluding(&[new_default]);
    write_config(&path, 1, legacy_default);
    let listener = Listener::load(path.clone(), Some(legacy_default)).unwrap();

    let prepared = listener
        .prepare_at(new_default, legacy_default)
        .await
        .unwrap();

    assert_eq!(prepared.port, legacy_default);
    assert_eq!(
        prepared
            .socket
            .as_ref()
            .unwrap()
            .local_addr()
            .unwrap()
            .port(),
        legacy_default
    );
    assert_eq!(
        prepared.notice.as_deref(),
        Some("新默认端口不可用，已保留原共享端口")
    );
    assert_config(&path, 2, legacy_default);
}

#[tokio::test]
async fn both_default_ports_occupied_leave_the_v1_file_and_listener_state_unchanged() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("gateway-listener.json");
    let occupied_new_default = held_port();
    let occupied_legacy_default = held_port();
    let new_default = occupied_new_default.local_addr().unwrap().port();
    let legacy_default = occupied_legacy_default.local_addr().unwrap().port();
    write_config(&path, 1, legacy_default);
    let listener = Listener::load(path.clone(), Some(legacy_default)).unwrap();
    let saved_bytes = fs::read(&path).unwrap();
    let view_before = listener.view();

    let error = listener
        .prepare_at(new_default, legacy_default)
        .await
        .err()
        .expect("preparation should fail when both candidate ports are occupied");

    assert_eq!(error.code, "PORT");
    assert_eq!(fs::read(&path).unwrap(), saved_bytes);
    let view_after = listener.view();
    assert_eq!(view_after.port, view_before.port);
    assert_eq!(view_after.revision, view_before.revision);
}

#[tokio::test]
async fn external_config_change_conflicts_before_migration_and_preserves_external_bytes() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("gateway-listener.json");
    let [new_default, legacy_default, external_port] = distinct_free_ports(3).try_into().unwrap();
    write_config(&path, 1, legacy_default);
    let listener = Listener::load(path.clone(), Some(legacy_default)).unwrap();
    let view_before = listener.view();

    write_config(&path, 2, external_port);
    let external_bytes = fs::read(&path).unwrap();
    let error = listener
        .prepare_at(new_default, legacy_default)
        .await
        .err()
        .expect("stale listener state should reject migration");

    assert_eq!(error.code, "CONFLICT");
    assert_eq!(fs::read(&path).unwrap(), external_bytes);
    let view_after = listener.view();
    assert_eq!(view_after.port, view_before.port);
    assert_eq!(view_after.revision, view_before.revision);
}

#[test]
fn unsupported_config_version_is_rejected_without_rewriting_the_file() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("gateway-listener.json");
    let unsupported = config_bytes(99, free_port_excluding(&[]));
    fs::write(&path, &unsupported).unwrap();

    let error = Listener::load(path.clone(), None)
        .err()
        .expect("unknown listener config version should be rejected");

    assert_eq!(error.code, "SETTINGS");
    assert_eq!(fs::read(path).unwrap(), unsupported);
}

#[tokio::test]
async fn persist_failure_keeps_listener_config_and_state_unchanged() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("gateway-listener.json");
    let [new_default, legacy_default] = distinct_free_ports(2).try_into().unwrap();
    write_config(&path, 1, legacy_default);
    let listener = Listener::load(path.clone(), Some(legacy_default)).unwrap();
    let saved_bytes = fs::read(&path).unwrap();
    let view_before = listener.view();
    let _fault = crate::storage::faults::inject(crate::storage::faults::Point::Replace);

    let error = listener
        .prepare_at(new_default, legacy_default)
        .await
        .err()
        .expect("injected config replacement failure should abort preparation");

    assert_eq!(error.code, "IO");
    assert_eq!(fs::read(&path).unwrap(), saved_bytes);
    let view_after = listener.view();
    assert_eq!(view_after.port, view_before.port);
    assert_eq!(view_after.revision, view_before.revision);
}

#[tokio::test]
async fn directory_sync_failure_keeps_listener_inactive_even_if_v2_was_published() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("gateway-listener.json");
    let [new_default, legacy_default] = distinct_free_ports(2).try_into().unwrap();
    write_config(&path, 1, legacy_default);
    let listener = Listener::load(path.clone(), Some(legacy_default)).unwrap();
    let view_before = listener.view();
    let _fault = crate::storage::faults::inject(crate::storage::faults::Point::DirectorySync);

    let error = listener
        .prepare_at(new_default, legacy_default)
        .await
        .err()
        .expect("directory sync failure should abort listener preparation");

    assert_eq!(error.code, "IO");
    assert_config(&path, 2, new_default);
    let view_after = listener.view();
    assert_eq!(view_after.port, view_before.port);
    assert_eq!(view_after.revision, view_before.revision);
    assert!(view_after.running_clients.is_empty());
    let _probe = StdTcpListener::bind((Ipv4Addr::LOCALHOST, new_default)).unwrap();
}

#[tokio::test]
async fn v2_config_does_not_retry_default_migration() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("gateway-listener.json");
    let occupied_new_default = held_port();
    let new_default = occupied_new_default.local_addr().unwrap().port();
    let persisted_port = free_port_excluding(&[new_default]);
    write_config(&path, 2, persisted_port);
    let listener = Listener::load(path.clone(), None).unwrap();
    let saved_bytes = fs::read(&path).unwrap();

    let prepared = listener
        .prepare_at(new_default, persisted_port)
        .await
        .unwrap();

    assert_eq!(prepared.port, persisted_port);
    assert!(prepared.notice.is_none(), "v2 ports are not being migrated");
    assert_eq!(
        prepared
            .socket
            .as_ref()
            .unwrap()
            .local_addr()
            .unwrap()
            .port(),
        persisted_port
    );
    assert_eq!(fs::read(&path).unwrap(), saved_bytes);
}

#[tokio::test]
async fn running_client_reuses_listener_without_binding_again() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("gateway-listener.json");
    let already_bound = held_port();
    let port = already_bound.local_addr().unwrap().port();
    let [new_default, legacy_default] = distinct_free_ports(2).try_into().unwrap();
    write_config(&path, 2, port);
    let listener = Listener::load(path.clone(), None).unwrap();
    {
        let mut state = listener.state.lock().unwrap();
        state.clients.push(super::ClientId::Codex);
    }
    let saved_bytes = fs::read(&path).unwrap();
    let view_before = listener.view();

    let prepared = listener
        .prepare_at(new_default, legacy_default)
        .await
        .unwrap();

    assert!(prepared.socket.is_none());
    assert_eq!(prepared.port, port);
    assert_eq!(prepared.revision, view_before.revision);
    assert_eq!(fs::read(path).unwrap(), saved_bytes);
}

#[tokio::test]
async fn repeated_prepare_and_reload_keep_the_first_migrated_port() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("gateway-listener.json");
    let [first_default, second_default, legacy_default] =
        distinct_free_ports(3).try_into().unwrap();
    write_config(&path, 1, legacy_default);
    let listener = Listener::load(path.clone(), Some(legacy_default)).unwrap();

    let first = listener
        .prepare_at(first_default, legacy_default)
        .await
        .unwrap();
    assert_eq!(first.port, first_default);
    assert_config(&path, 2, first_default);
    drop(first);

    let repeated = listener
        .prepare_at(second_default, legacy_default)
        .await
        .unwrap();
    assert_eq!(repeated.port, first_default);
    assert_config(&path, 2, first_default);
    drop(repeated);

    let reloaded = Listener::load(path.clone(), None).unwrap();
    let after_reload = reloaded
        .prepare_at(second_default, legacy_default)
        .await
        .unwrap();
    assert_eq!(after_reload.port, first_default);
    assert_config(&path, 2, first_default);
}
