use super::*;
use crate::gateway::{current::Kind, model::Store};
use std::fs;

const DESKTOP: &str = "# desktop comment\n- id: llm-pi-ai\n  config:\n    providers:\n      relay:\n        displayName: Fixture Relay\n        baseURL: https://relay.example.invalid/v1\n        api: openai-completions\n        apiKeyEnv: FIXTURE_KEY\n        models: [{id: fixture-model}]\n      another: {baseURL: 'https://other.example.invalid/v1'}\n- id: agent-default-model\n  config: {provider: relay, model: fixture-model}\n- id: unrelated\n  config: {keep: true}\n";

pub(super) fn fixture() -> (tempfile::TempDir, Gateway, Gateway) {
    let home = tempfile::tempdir().unwrap();
    for profile in ["desktop", "headless", "web"] {
        let path = home.path().join("profiles").join(profile);
        fs::create_dir_all(&path).unwrap();
        fs::write(
            path.join("package.json"),
            r#"{"dsh":{"profile":{"bundles":["fixture-missing-bundle"]}}}"#,
        )
        .unwrap();
        fs::write(
            path.join(PATCH),
            if profile == "desktop" {
                DESKTOP
            } else {
                "# empty profile\n[]\n"
            },
        )
        .unwrap();
    }
    fs::write(
        home.path().join(".credentials.yaml"),
        "version: 1\nrefs: {FIXTURE_KEY: fixture-provider-secret}\nrecords: {}\n",
    )
    .unwrap();
    let parent = Gateway::new(home.path().join("data")).unwrap();
    let gateway = parent.dsh_companion(home.path().join("data/dsh")).unwrap();
    let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = socket.local_addr().unwrap().port();
    drop(socket);
    gateway
        .set_listener_port(port, &gateway.try_listener_view().unwrap().revision)
        .unwrap();
    (home, parent, gateway)
}
pub(super) fn options() -> Options {
    Options {
        profile: "desktop".into(),
        protocol: Protocol::ChatCompletions,
        model: Some("fixture-model".into()),
        make_default: true,
    }
}

#[tokio::test]
async fn desktop_without_bundle_is_recognized_imported_and_shared_with_empty_profiles() {
    let (home, _parent, gateway) = fixture();
    let preview = gateway
        .preview_dsh(home.path(), None, Protocol::Messages)
        .unwrap();
    assert_eq!(preview.profile.as_deref(), Some("desktop"));
    assert_eq!(preview.protocol, Protocol::ChatCompletions);
    assert_eq!(preview.model.as_deref(), Some(models::FLASH));
    assert!(matches!(
        preview.current.as_ref().unwrap().state,
        Kind::Unmatched { .. }
    ));
    let serialized = serde_json::to_string(&preview).unwrap();
    assert!(!serialized.contains("fixture-provider-secret"));
    assert_eq!(preview.states.len(), 3);
    let file = gateway
        .read_dsh_file(home.path(), None, editor::FileKey::ProfilePatch)
        .unwrap();
    assert_eq!(file.profile.as_deref(), Some("desktop"));
    assert_eq!(file.text, DESKTOP);
    let result = gateway
        .attach_dsh_profiles(
            home.path(),
            options(),
            true,
            &gateway.try_view().unwrap().revision,
            Some(&preview.revision),
        )
        .await
        .unwrap();
    assert!(result.results.iter().all(|r| r.status == "success"));
    assert_eq!(result.state.providers.len(), 1);
    assert_eq!(result.state.providers[0].name, "Fixture Relay");
    assert!(result
        .state
        .dsh_profiles
        .iter()
        .all(|p| p.current.state == Kind::Gateway));
    gateway.stop_dsh(false).await.unwrap();
    let restored = fs::read_to_string(home.path().join("profiles/desktop").join(PATCH)).unwrap();
    assert!(
        restored.contains("desktop comment")
            && restored.contains("another:")
            && restored.contains("keep: true")
    );
    assert!(!restored.contains(ROUTE));
    let credentials = fs::read_to_string(home.path().join(".credentials.yaml")).unwrap();
    assert!(credentials.contains("fixture-provider-secret") && !credentials.contains(KEY));
}

#[tokio::test]
async fn shared_overrides_restore_only_owned_fields_and_refuse_external_changes() {
    let (home, _parent, gateway) = fixture();
    fs::write(home.path().join(PATCH), DESKTOP).unwrap();
    fs::write(
        home.path().join("settings.yaml"),
        "# legacy\nagent-default-model: {provider: relay, model: fixture-model}\nunrelated: keep\n",
    )
    .unwrap();
    gateway
        .attach_dsh_profiles(
            home.path(),
            options(),
            true,
            &gateway.try_view().unwrap().revision,
            None,
        )
        .await
        .unwrap();
    let current = gateway.try_view().unwrap();
    assert!(current
        .dsh_profiles
        .iter()
        .all(|p| p.current.state == Kind::Gateway));
    let path = home.path().join(PATCH);
    let managed = fs::read_to_string(&path).unwrap();
    fs::write(&path, managed.replace("keep: true", "keep: false")).unwrap();
    let credential_path = home.path().join(".credentials.yaml");
    let credential = fs::read_to_string(&credential_path).unwrap();
    let changed = credential.replace(
        &gateway.0.inner.lock().unwrap().store.local_token,
        "external-fixture-key",
    );
    fs::write(&credential_path, &changed).unwrap();
    assert_eq!(
        gateway.stop_dsh(false).await.err().unwrap().code,
        "CONFLICT"
    );
    assert_eq!(fs::read_to_string(&credential_path).unwrap(), changed);
    fs::write(&credential_path, credential).unwrap();
    gateway.stop_dsh(false).await.unwrap();
    let restored = fs::read_to_string(&path).unwrap();
    assert!(
        restored.contains("keep: false")
            && restored.contains("another:")
            && !restored.contains(ROUTE)
    );
    let restored = fs::read_to_string(home.path().join("settings.yaml")).unwrap();
    assert!(restored.contains("provider: relay") && restored.contains("unrelated: keep"));
}

#[test]
fn same_unimported_address_with_different_credentials_is_inconsistent() {
    let (home, _parent, gateway) = fixture();
    for profile in ["headless", "web"] {
        fs::write(
            home.path().join("profiles").join(profile).join(PATCH),
            DESKTOP.replace("apiKeyEnv: FIXTURE_KEY", "apiKey: different-fixture-key"),
        )
        .unwrap();
    }
    let current = current(home.path(), &Store::default(), &[43721], &|_| None);
    assert!(matches!(current.state, Kind::Unknown { reason } if reason.contains("配置不一致")));
    assert!(gateway
        .preview_dsh(home.path(), Some("desktop"), Protocol::ChatCompletions)
        .unwrap()
        .current
        .is_some());
}
