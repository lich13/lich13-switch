use super::*;
use crate::gateway::{
    current::{Kind, Match},
    model::Store,
    Edit,
};
use editor::FileKey;
use std::fs;

fn options(profile: &str) -> Options {
    Options {
        profile: profile.into(),
        protocol: Protocol::ChatCompletions,
        model: Some("fixture-model".into()),
        make_default: true,
    }
}
fn extra_profile(home: &Path, name: &str) {
    let dir = home.join("profiles").join(name);
    fs::create_dir_all(&dir).unwrap();
    fs::copy(
        home.join("profiles/default/package.json"),
        dir.join("package.json"),
    )
    .unwrap();
    fs::write(dir.join(PATCH), "[]\n").unwrap();
    let shared = home.join("runtime/node_modules/fixture-bundle");
    fs::create_dir_all(&shared).unwrap();
    for file in ["package.json", PATCH] {
        fs::copy(
            home.join("profiles/default/node_modules/fixture-bundle")
                .join(file),
            shared.join(file),
        )
        .unwrap();
    }
}
fn gateway(home: &Path) -> Gateway {
    let parent = Gateway::new(home.join("data")).unwrap();
    let gateway = parent.dsh_companion(home.join("data/dsh")).unwrap();
    let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = socket.local_addr().unwrap().port();
    drop(socket);
    gateway
        .set_listener_port(port, &gateway.try_listener_view().unwrap().revision)
        .unwrap();
    let mut state = gateway.0.inner.lock().unwrap();
    state
        .store
        .edit(
            Edit::SaveDshProvider {
                quota_access_token: None,
                id: None,
                base_url: "https://relay.example.invalid/v1".into(),
                token: "fixture-provider".into(),
                name: Some("Fixture".into()),
                protocol: Protocol::ChatCompletions,
            },
            false,
        )
        .unwrap();
    state.revision = state
        .store
        .persist(&gateway.0.data.join("gateway.json"), &state.revision)
        .unwrap();
    drop(state);
    gateway
}

#[test]
fn generated_patch_round_trips_empty_arrays_and_trailing_comments() {
    for empty in [true, false] {
        let home = super::tests::fixture();
        let raw = if empty {
            "[]\n".to_string()
        } else {
            format!(
                "{}# saved comment\n",
                fs::read_to_string(home.path().join("profiles/default").join(PATCH)).unwrap()
            )
        };
        fs::write(home.path().join("profiles/default").join(PATCH), raw).unwrap();
        let mut input = Input::read(home.path(), "default").unwrap();
        let snapshot = prepare(
            &mut input,
            options("default"),
            "fixture-local",
            43721,
            &[],
            None,
        )
        .unwrap();
        let parsed = sequence(&input.patch.after).unwrap();
        assert!(
            owned_route(&parsed).unwrap().is_some(),
            "{}",
            input.patch.after
        );
        assert!(
            find_row(&parsed, DEFAULT).unwrap().is_some(),
            "{}",
            input.patch.after
        );
        input.patch.before = Some(input.patch.after.clone());
        input.environment.before = Some(input.environment.after.clone());
        input.credentials.before = Some(input.credentials.after.clone());
        restore(&snapshot, &mut input).unwrap();
        assert!(
            sequence(&input.patch.after).is_ok(),
            "{}",
            input.patch.after
        );
        // A comment/unknown setting added to a newly managed default row must
        // survive stop, while the native inherited model becomes effective again.
        input.patch.before = Some(snapshot.patch.after.replace(
            "id: agent-default-model",
            "id: agent-default-model # keep new note\n  unknown: keep",
        ));
        input.environment.before = Some(snapshot.environment.after.clone());
        input.credentials.before = Some(snapshot.credentials.after.clone());
        restore(&snapshot, &mut input).unwrap();
        assert!(input.patch.after.contains("# keep new note"));
        assert!(input.patch.after.contains("unknown: keep"));
        input
            .read_layer(&sequence(&input.patch.after).unwrap(), 0)
            .unwrap();
        assert_eq!(
            string(input.configs.get(DEFAULT).unwrap().get("provider")).as_deref(),
            Some("deepseek-official")
        );
    }
}

#[test]
fn effective_profile_home_settings_and_credentials_are_reported_without_secrets() {
    let home = super::tests::fixture();
    let mut store = Store::default();
    store
        .edit(
            Edit::SaveProvider {
                search_only: None,
                quota_access_token: None,
                id: None,
                name: None,
                base_url: "https://relay.example.invalid/v1".into(),
                token: "fixture-provider".into(),
            },
            false,
        )
        .unwrap();
    fs::write(home.path().join("profiles/default").join(PATCH), "- id: llm-pi-ai\n  config:\n    providers:\n      relay: {baseURL: 'https://Relay.Example.invalid:443/', apiKeyEnv: FIXTURE_KEY}\n- id: agent-default-model\n  config: {provider: relay, model: fixture-model}\n").unwrap();
    fs::write(
        home.path().join(".credentials.yaml"),
        "version: 1\nrefs: {FIXTURE_KEY: fixture-provider}\n",
    )
    .unwrap();
    let current = current(home.path(), &store, &[43721], &|_| {
        Some("wrong-environment".into())
    });
    // Stored references outrank unrelated environment values. The launcher does
    // not select a profile through a guessed DSH_PROFILE environment variable.
    assert!(matches!(
        current.state,
        Kind::Provider {
            by: Match::UrlToken,
            ..
        }
    ));
    let selected = super::current(home.path(), &store, &[43721], &|name| {
        (name == "FIXTURE_KEY").then(|| "wrong-environment".into())
    });
    assert!(matches!(
        selected.state,
        Kind::Provider {
            by: Match::UrlToken,
            ..
        }
    ));
    let serialized = serde_json::to_string(&selected).unwrap();
    assert!(
        !serialized.contains("fixture-provider")
            && !serialized.contains(home.path().to_str().unwrap())
    );
    fs::write(
        home.path().join(PATCH),
        "- id: agent-default-model\n  config: {provider: deepseek-official, model: native}\n",
    )
    .unwrap();
    let official = super::current(home.path(), &store, &[43721], &|_| None);
    assert_eq!(official.state, Kind::Official);
    assert!(official.warning.unwrap().contains("home patch"));
    fs::write(home.path().join(PATCH), "- id: llm-pi-ai\n  disabled: false\n- id: agent-default-model\n  name: different-plugin\n  config: {provider: deepseek-official}\n").unwrap();
    assert!(matches!(
        super::current(home.path(), &store, &[43721], &|_| None).state,
        Kind::Provider { .. }
    ));
    fs::write(
        home.path().join("settings.yaml"),
        "agent-default-model: {provider: deepseek-official, model: legacy}\n",
    )
    .unwrap();
    assert_eq!(
        super::current(home.path(), &store, &[43721], &|_| None).state,
        Kind::Official
    );
    fs::remove_file(home.path().join("settings.yaml")).unwrap();
    fs::write(
        home.path().join(".credentials.yaml"),
        "version: 1\nrefs: {FIXTURE_KEY: wrong-provider}\n",
    )
    .unwrap();
    assert!(matches!(
        super::current(home.path(), &store, &[43721], &|_| None).state,
        Kind::Unmatched { .. }
    ));
    extra_profile(home.path(), "desktop");
    assert!(matches!(
        super::current(home.path(), &store, &[43721], &|_| None).state,
        Kind::Mixed
    ));
}

#[tokio::test]
async fn editor_preserves_drafts_backups_comments_and_blocks_candidate_overrides() {
    let home = super::tests::fixture();
    let gateway = gateway(home.path());
    let original = gateway
        .read_dsh_file(home.path(), Some("default"), FileKey::ProfilePatch)
        .unwrap();
    let modified = format!("{}# saved comment\n", original.text);
    let saved = gateway
        .save_dsh_file(
            home.path(),
            Some("default"),
            FileKey::ProfilePatch,
            Some(&modified),
            &original.revision,
        )
        .await
        .unwrap();
    assert!(saved.can_restore);
    assert_eq!(
        gateway
            .previous_dsh_file(home.path(), Some("default"), FileKey::ProfilePatch)
            .unwrap(),
        original.text
    );
    assert!(gateway
        .save_dsh_file(
            home.path(),
            Some("default"),
            FileKey::ProfilePatch,
            Some("[]"),
            &original.revision
        )
        .await
        .is_err());
    assert_eq!(
        gateway
            .read_dsh_file(home.path(), Some("default"), FileKey::ProfilePatch)
            .unwrap()
            .text,
        modified
    );
    let restored = gateway
        .save_dsh_file(
            home.path(),
            Some("default"),
            FileKey::ProfilePatch,
            None,
            &saved.revision,
        )
        .await
        .unwrap();
    assert_eq!(restored.text, original.text);
    // File enumeration/editing does not require a working bundle parser.
    fs::write(home.path().join("profiles/default/package.json"), "{}").unwrap();
    assert!(gateway
        .read_dsh_file(home.path(), Some("default"), FileKey::ProfilePatch)
        .is_ok());
    gateway
        .save_dsh_file(
            home.path(),
            Some("default"),
            FileKey::ProfilePatch,
            Some(&modified),
            &restored.revision,
        )
        .await
        .unwrap();
    fs::write(
        home.path().join("profiles/default/package.json"),
        r#"{"dsh":{"profile":{"bundles":["fixture-bundle"]}}}"#,
    )
    .unwrap();
    gateway
        .attach_dsh(
            home.path(),
            options("default"),
            &gateway.try_view().unwrap().revision,
            None,
        )
        .await
        .unwrap();
    let active = gateway
        .read_dsh_file(home.path(), Some("default"), FileKey::ProfilePatch)
        .unwrap();
    assert!(gateway
        .save_dsh_file(
            home.path(),
            Some("default"),
            FileKey::ProfilePatch,
            Some("[]"),
            &active.revision
        )
        .await
        .is_err());
    let allowed = format!("{}# unrelated\n", active.text);
    gateway
        .save_dsh_file(
            home.path(),
            Some("default"),
            FileKey::ProfilePatch,
            Some(&allowed),
            &active.revision,
        )
        .await
        .unwrap();
    for (key, raw) in [
        (
            FileKey::HomePatch,
            "- id: llm-pi-ai\n  config: {providers: {other: {}}}\n",
        ),
        (
            FileKey::Settings,
            "agent-default-model: {provider: other, model: fixture}\n",
        ),
    ] {
        if key == FileKey::Settings {
            fs::write(home.path().join("settings.yaml"), "{}\n").unwrap();
        }
        let file = gateway.read_dsh_file(home.path(), None, key).unwrap();
        assert!(gateway
            .save_dsh_file(home.path(), None, key, Some(raw), &file.revision)
            .await
            .is_err());
    }
    let readonly = gateway
        .read_dsh_file(home.path(), None, FileKey::Credentials)
        .unwrap();
    assert!(
        readonly.read_only && readonly.text.contains("••••") && !readonly.text.contains("fixture")
    );
    gateway.stop_dsh(false).await.unwrap();
}

#[tokio::test]
async fn batch_preflights_all_profiles_and_restores_shared_credentials_last() {
    let home = super::tests::fixture();
    extra_profile(home.path(), "b-profile");
    extra_profile(home.path(), "c-profile");
    fs::write(
        home.path().join("profiles/b-profile").join(PATCH),
        "- id: llm-pi-ai\n  config: {providers: {lich13-switch: {foreign: true}}}\n",
    )
    .unwrap();
    let gateway = gateway(home.path());
    let before_env = fs::read_to_string(home.path().join(".env")).unwrap();
    let failure = gateway
        .attach_dsh_profiles(
            home.path(),
            options("default"),
            true,
            &gateway.try_view().unwrap().revision,
            None,
        )
        .await
        .err()
        .expect("all targets must be preflighted");
    assert!(failure.message.contains("同名"));
    for profile in ["default", "c-profile"] {
        assert!(
            !fs::read_to_string(home.path().join("profiles").join(profile).join(PATCH))
                .unwrap()
                .contains(ROUTE)
        );
    }
    assert_eq!(
        fs::read_to_string(home.path().join(".env")).unwrap(),
        before_env
    );
    fs::write(home.path().join("profiles/b-profile").join(PATCH), "[]\n").unwrap();
    let result = gateway
        .attach_dsh_profiles(
            home.path(),
            options("default"),
            true,
            &gateway.try_view().unwrap().revision,
            None,
        )
        .await
        .unwrap();
    assert!(result.results.iter().all(|r| r.status == "success"));
    let snapshot = gateway.0.inner.lock().unwrap().store.dsh.clone().unwrap();
    assert_eq!(snapshot.active_profiles().len(), 3);
    assert_eq!(
        snapshot.environment.before.as_deref(),
        Some(before_env.as_str())
    );
    assert!(snapshot.profiles.values().all(|p| !p
        .patch
        .before
        .as_deref()
        .unwrap_or("")
        .contains(ROUTE)));
    let mut legacy = serde_json::to_value(&snapshot).unwrap();
    legacy.as_object_mut().unwrap().remove("profiles");
    let legacy = serde_json::from_value::<Snapshot>(legacy).unwrap();
    assert_eq!(legacy.active_profiles().len(), 1);
    let mut legacy_input = Input::read(home.path(), &legacy.options.profile).unwrap();
    restore(&legacy, &mut legacy_input).unwrap();
    assert!(owned_route(&sequence(&legacy_input.patch.after).unwrap())
        .unwrap()
        .is_none());
    let listener_port = gateway.try_listener_view().unwrap().port;
    let store = gateway.0.inner.lock().unwrap().store.clone();
    assert_eq!(
        super::current(home.path(), &store, &[listener_port], &|_| None).state,
        Kind::Gateway
    );
    gateway.stop_dsh(false).await.unwrap();
    for profile in ["default", "b-profile", "c-profile"] {
        assert!(
            !fs::read_to_string(home.path().join("profiles").join(profile).join(PATCH))
                .unwrap()
                .contains(ROUTE)
        );
    }
    assert!(!fs::read_to_string(home.path().join(".env"))
        .unwrap()
        .contains(KEY));
    assert!(!fs::read_to_string(home.path().join(".credentials.yaml"))
        .unwrap()
        .contains(KEY));
    let revision = gateway.try_view().unwrap().revision;
    gateway.restart_dsh(home.path(), &revision).await.unwrap();
    assert_eq!(
        gateway
            .0
            .inner
            .lock()
            .unwrap()
            .store
            .dsh
            .as_ref()
            .unwrap()
            .active_profiles()
            .len(),
        3
    );
    gateway.stop_dsh(false).await.unwrap();
}

#[tokio::test]
async fn interrupted_new_transaction_rolls_back_and_conflicts_leave_recovery_journal() {
    let home = super::tests::fixture();
    let data = home.path().join("data");
    storage::private_dir(&data).unwrap();
    let mut input = Input::read(home.path(), "default").unwrap();
    prepare(
        &mut input,
        options("default"),
        "fixture-local",
        43721,
        &[],
        None,
    )
    .unwrap();
    let tx = Transaction {
        patches: BTreeMap::new(),
        version: 1,
        rollback: true,
        home: home.path().into(),
        profile: "default".into(),
        patch: input.patch.clone(),
        environment: input.environment.clone(),
        credentials: input.credentials.clone(),
        home_patch: input.home_patch.clone(),
        settings: input.settings.clone(),
        store: Change {
            before: None,
            after: "{}".into(),
        },
    };
    storage::atomic_write(&data.join(JOURNAL), &serde_json::to_vec(&tx).unwrap(), None).unwrap();
    fs::write(input.dir.join(PATCH), &input.patch.after).unwrap();
    fs::write(home.path().join(".env"), "EXTERNAL=kept\n").unwrap();
    let failure = recover(&data).unwrap_err();
    assert_eq!(failure.code, code::CONFLICT);
    let parent = Gateway::new(home.path().join("parent-data")).unwrap();
    let gateway = parent.dsh_companion(data.clone()).unwrap();
    let resumed = gateway.resume(home.path()).await.unwrap_err();
    assert_eq!(resumed.code, failure.code);
    assert_eq!(resumed.message, failure.message);
    assert!(data.join(JOURNAL).exists());
    assert_eq!(
        fs::read_to_string(home.path().join(".env")).unwrap(),
        "EXTERNAL=kept\n"
    );
    fs::write(
        home.path().join(".env"),
        input.environment.before.as_ref().unwrap(),
    )
    .unwrap();
    recover(&data).unwrap();
    assert!(!data.join(JOURNAL).exists());
    assert_eq!(
        fs::read_to_string(input.dir.join(PATCH)).unwrap(),
        input.patch.before.unwrap()
    );
    assert!(!data.join("gateway.json").exists());
}

#[tokio::test]
async fn editor_rejects_duplicate_managed_rows_invalid_root_symlinks_and_pending_recovery() {
    let home = super::tests::fixture();
    let gateway = gateway(home.path());
    let file = gateway
        .read_dsh_file(home.path(), Some("default"), FileKey::ProfilePatch)
        .unwrap();
    for raw in [
        "{}",
        "- id: llm-pi-ai\n- id: llm-pi-ai\n",
        "- id: llm-pi-ai\n  config: {providers: {}, providers: {}}\n",
    ] {
        assert!(gateway
            .save_dsh_file(
                home.path(),
                Some("default"),
                FileKey::ProfilePatch,
                Some(raw),
                &file.revision
            )
            .await
            .is_err());
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(home.path().join(".env"), home.path().join(PATCH)).unwrap();
        assert!(gateway
            .read_dsh_file(home.path(), None, FileKey::HomePatch)
            .is_err());
    }
    fs::write(gateway.0.data.join(JOURNAL), "{}").unwrap();
    assert!(gateway
        .save_dsh_file(
            home.path(),
            Some("default"),
            FileKey::ProfilePatch,
            Some("[]"),
            &file.revision
        )
        .await
        .is_err());
}
