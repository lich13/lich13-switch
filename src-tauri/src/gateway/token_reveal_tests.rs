// 覆盖按需查看供应商凭据与版本校验。
use super::*;
use std::path::Path;

fn save_provider(gateway: &Gateway, home: &Path, base_url: &str, token: &str) -> String {
    gateway
        .edit(
            Edit::SaveProvider {
                quota_access_token: None,
                id: None,
                base_url: base_url.into(),
                token: token.into(),
                name: Some("fixture provider".into()),
            },
            &gateway.view().revision,
            home,
        )
        .unwrap();
    gateway.view().providers[0].id.clone()
}

fn provider_version(gateway: &Gateway, id: &str) -> String {
    gateway
        .0
        .inner
        .lock()
        .unwrap()
        .store
        .providers
        .iter()
        .find(|provider| provider.id == id)
        .unwrap()
        .version
        .clone()
}

#[test]
fn reveal_is_main_window_only_and_scoped_to_the_selected_client() {
    let temp = tempfile::tempdir().unwrap();
    let codex = Gateway::new(temp.path().join("codex-data")).unwrap();
    let claude = codex.companion(temp.path().join("claude-data")).unwrap();
    let codex_id = save_provider(
        &codex,
        temp.path(),
        "https://codex.example.invalid/v1",
        "fixture-codex-token",
    );
    let claude_id = save_provider(
        &claude,
        temp.path(),
        "https://claude.example.invalid/v1",
        "fixture-claude-token",
    );

    assert_eq!(
        codex
            .reveal_provider_token("main", &codex_id, &codex.view().revision)
            .unwrap(),
        "fixture-codex-token"
    );
    assert_eq!(
        claude
            .reveal_provider_token("main", &claude_id, &claude.view().revision)
            .unwrap(),
        "fixture-claude-token"
    );

    for result in [
        codex.reveal_provider_token("settings", &codex_id, &codex.view().revision),
        codex.reveal_provider_token("main", &claude_id, &codex.view().revision),
        claude.reveal_provider_token("main", &codex_id, &claude.view().revision),
        codex.reveal_provider_token("main", "../outside-provider", &codex.view().revision),
    ] {
        let error = result.unwrap_err();
        assert!(!error.message.contains("fixture-"));
    }

    let codex_view = serde_json::to_string(&codex.view()).unwrap();
    let claude_view = serde_json::to_string(&claude.view()).unwrap();
    assert!(!codex_view.contains("fixture-codex-token"));
    assert!(!claude_view.contains("fixture-claude-token"));
}

#[test]
fn reveal_checks_memory_and_disk_revisions_without_writes_events_or_logs() {
    let temp = tempfile::tempdir().unwrap();
    let gateway = Gateway::new(temp.path().join("gateway-data")).unwrap();
    let id = save_provider(
        &gateway,
        temp.path(),
        "https://provider.example.invalid/v1",
        "fixture-reveal-token",
    );
    let diagnostics = crate::events::Service::new(temp.path());
    gateway.set_diagnostics(diagnostics.clone());
    let mut changes = gateway.subscribe();
    let path = temp.path().join("gateway-data/gateway.json");
    let disk_before = std::fs::read(&path).unwrap();
    let revision = gateway.view().revision;

    assert_eq!(
        gateway
            .reveal_provider_token("main", &id, &revision)
            .unwrap(),
        "fixture-reveal-token"
    );
    assert_eq!(gateway.view().revision, revision);
    assert_eq!(std::fs::read(&path).unwrap(), disk_before);
    assert!(matches!(
        changes.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Empty)
    ));
    assert_eq!(diagnostics.query(crate::events::Filter::default()).total, 0);
    assert!(!serde_json::to_string(&gateway.view())
        .unwrap()
        .contains("fixture-reveal-token"));

    gateway
        .edit(
            Edit::RenameProvider {
                id: id.clone(),
                name: "renamed fixture".into(),
            },
            &gateway.view().revision,
            temp.path(),
        )
        .unwrap();
    while changes.try_recv().is_ok() {}
    let stale_view = revision;
    let error = gateway
        .reveal_provider_token("main", &id, &stale_view)
        .unwrap_err();
    assert_eq!(error.code, "CONFLICT");
    assert!(!error.message.contains("fixture-reveal-token"));
    assert!(matches!(
        changes.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Empty)
    ));
    assert_eq!(diagnostics.query(crate::events::Filter::default()).total, 0);

    let current_view = gateway.view().revision;
    let current_disk = std::fs::read(&path).unwrap();
    let mut externally_changed = current_disk.clone();
    externally_changed.push(b' ');
    std::fs::write(&path, &externally_changed).unwrap();
    let error = gateway
        .reveal_provider_token("main", &id, &current_view)
        .unwrap_err();
    assert_eq!(error.code, "CONFLICT");
    assert!(!error.message.contains("fixture-reveal-token"));
    assert_eq!(std::fs::read(&path).unwrap(), externally_changed);
    assert_eq!(diagnostics.query(crate::events::Filter::default()).total, 0);
}

#[test]
fn saving_an_empty_token_without_changing_the_url_preserves_provider_identity() {
    let temp = tempfile::tempdir().unwrap();
    let gateway = Gateway::new(temp.path().join("gateway-data")).unwrap();
    let base_url = "https://provider.example.invalid/v1";
    let initial_token = "fixture-initial-token";
    let id = save_provider(&gateway, temp.path(), base_url, initial_token);
    let initial_view = gateway.view();
    let initial_version = provider_version(&gateway, &id);

    gateway
        .edit(
            Edit::SaveProvider {
                quota_access_token: None,
                id: Some(id.clone()),
                base_url: base_url.into(),
                token: String::new(),
                name: None,
            },
            &initial_view.revision,
            temp.path(),
        )
        .unwrap();

    let unchanged_view = gateway.view();
    assert_eq!(unchanged_view.revision, initial_view.revision);
    assert_eq!(provider_version(&gateway, &id), initial_version);
    assert_eq!(
        gateway
            .reveal_provider_token("main", &id, &initial_view.revision)
            .unwrap(),
        initial_token
    );

    let updated_token = "fixture-updated-token";
    gateway
        .edit(
            Edit::SaveProvider {
                quota_access_token: None,
                id: Some(id.clone()),
                base_url: base_url.into(),
                token: updated_token.into(),
                name: None,
            },
            &unchanged_view.revision,
            temp.path(),
        )
        .unwrap();

    let token_changed_view = gateway.view();
    let token_changed_version = provider_version(&gateway, &id);
    assert_ne!(token_changed_view.revision, unchanged_view.revision);
    assert_ne!(token_changed_version, initial_version);
    assert_eq!(
        gateway
            .reveal_provider_token("main", &id, &token_changed_view.revision)
            .unwrap(),
        updated_token
    );

    let updated_base_url = "https://provider-renewed.example.invalid/v2";
    gateway
        .edit(
            Edit::SaveProvider {
                quota_access_token: None,
                id: Some(id.clone()),
                base_url: updated_base_url.into(),
                token: String::new(),
                name: None,
            },
            &token_changed_view.revision,
            temp.path(),
        )
        .unwrap();

    let url_changed_view = gateway.view();
    let url_changed_version = provider_version(&gateway, &id);
    assert_ne!(url_changed_view.revision, token_changed_view.revision);
    assert_ne!(url_changed_version, token_changed_version);
    assert_eq!(url_changed_view.providers[0].base_url, updated_base_url);
    assert_eq!(
        gateway
            .reveal_provider_token("main", &id, &url_changed_view.revision)
            .unwrap(),
        updated_token
    );
}
