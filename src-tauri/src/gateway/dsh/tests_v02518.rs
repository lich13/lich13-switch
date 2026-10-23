use super::*;
use crate::gateway::model::{Edit, Store};
use std::fs;

fn change_default(home: &Path, profile: &str, model: &str, effort: &str) {
    let path = home.join("profiles").join(profile).join(PATCH);
    let (file, patch) = patch_file(&fs::read_to_string(&path).unwrap()).unwrap();
    let config = find_row(&patch, DEFAULT)
        .unwrap()
        .unwrap()
        .1
        .get_mapping("config")
        .unwrap();
    config.set("model", model);
    config.set("reasoningEffort", effort);
    fs::write(path, file.to_string()).unwrap();
}

#[tokio::test]
async fn sync_restart_and_stop_preserve_each_profiles_model_and_reasoning() {
    let (home, _parent, gateway) = tests_v02516::fixture();
    gateway
        .attach_dsh_profiles(
            home.path(),
            tests_v02516::options(),
            true,
            &gateway.view().revision,
            None,
        )
        .await
        .unwrap();
    change_default(home.path(), "desktop", "deepseek-future", "max");
    change_default(home.path(), "web", models::PRO, "high");
    change_default(home.path(), "headless", models::FLASH, "low");
    let current = gateway.view();
    assert_eq!(
        current.current.state,
        crate::gateway::current::Kind::Gateway
    );
    assert_eq!(current.current.model, None);
    assert_eq!(current.config_error, None);
    gateway
        .restart_dsh(home.path(), &gateway.view().revision)
        .await
        .unwrap();
    let mut options = tests_v02516::options();
    options.model = None;
    gateway
        .attach_dsh_profiles(home.path(), options, true, &gateway.view().revision, None)
        .await
        .unwrap();
    for (profile, model, effort) in [
        ("desktop", "deepseek-future", "max"),
        ("web", models::PRO, "high"),
        ("headless", models::FLASH, "low"),
    ] {
        let (input, _, _) = selection::effective(home.path(), profile).unwrap();
        let current = &input.configs[DEFAULT];
        assert_eq!(string(current.get("model")).as_deref(), Some(model));
        assert_eq!(
            string(current.get("reasoningEffort")).as_deref(),
            Some(effort)
        );
    }
    change_default(home.path(), "desktop", "deepseek-after-sync", "max");
    gateway.stop_for_exit().await.unwrap();
    gateway.resume(home.path()).await.unwrap();
    let resumed = gateway.view();
    assert_eq!(
        resumed.current.state,
        crate::gateway::current::Kind::Gateway
    );
    assert_eq!(resumed.current.model, None);
    assert_eq!(resumed.config_error, None);
    let (input, _, _) = selection::effective(home.path(), "desktop").unwrap();
    assert_eq!(
        string(input.configs[DEFAULT].get("model")).as_deref(),
        Some("deepseek-after-sync")
    );
    assert_eq!(
        string(input.configs[DEFAULT].get("reasoningEffort")).as_deref(),
        Some("max")
    );
    gateway.stop_dsh(false).await.unwrap();
    let (headless, _, _) = selection::effective(home.path(), "headless").unwrap();
    assert_eq!(
        string(headless.configs[DEFAULT].get("reasoningEffort")).as_deref(),
        Some("low")
    );
    let (input, _, _) = selection::effective(home.path(), "desktop").unwrap();
    assert_eq!(
        string(input.configs[DEFAULT].get("reasoningEffort")).as_deref(),
        Some("max")
    );
}

#[tokio::test]
async fn mapping_save_updates_every_active_catalog_and_retains_model_choices() {
    let (home, _parent, gateway) = tests_v02516::fixture();
    gateway
        .attach_dsh_profiles(
            home.path(),
            tests_v02516::options(),
            true,
            &gateway.view().revision,
            None,
        )
        .await
        .unwrap();
    change_default(home.path(), "desktop", "deepseek-future", "max");
    gateway.0.inner.lock().unwrap().store.dsh_catalog =
        vec![serde_json::json!({"id":"deepseek-future","name":"Future"})];
    let id = gateway.view().providers[0].id.clone();
    gateway
        .edit(
            Edit::DshModelsProvider {
                id,
                mapping: ModelMapping(BTreeMap::from([(
                    "deepseek-future".into(),
                    Some("vendor-future".into()),
                )])),
            },
            &gateway.view().revision,
            home.path(),
        )
        .unwrap();
    for profile in ["desktop", "headless", "web"] {
        let input = Input::read(home.path(), profile).unwrap();
        let snapshot = gateway
            .0
            .inner
            .lock()
            .unwrap()
            .store
            .dsh
            .as_ref()
            .unwrap()
            .for_profile(profile)
            .unwrap();
        check_owned(&snapshot, &input).unwrap();
        assert!(snapshot.patch.after.contains("deepseek-future"));
    }
    assert!(gateway
        .model_union(None)
        .unwrap()
        .contains(&"deepseek-future".into()));
    gateway.stop_dsh(false).await.unwrap();
}

#[test]
fn legacy_mapping_preserves_explicit_empty_and_canonical_values() {
    let mut value = serde_json::to_value(Store::default()).unwrap();
    value["dshModels"] = serde_json::json!({"fixture": {"flash":"custom-flash", "pro":null, "deepseek-future":"custom-future"}});
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("gateway.json");
    storage::atomic_write(&path, &serde_json::to_vec(&value).unwrap(), Some("missing")).unwrap();
    let (loaded, revision) = Store::load(&path).unwrap();
    crate::migrations::dsh_model_mappings(&mut value);
    let store: Store = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(loaded.dsh_models, store.dsh_models);
    loaded.persist(&path, &revision).unwrap();
    assert_eq!(
        store.dsh_models["fixture"].resolve(models::FLASH),
        Some("custom-flash")
    );
    assert_eq!(store.dsh_models["fixture"].0.get(models::PRO), Some(&None));
    assert_eq!(
        store.dsh_models["fixture"].resolve("deepseek-future"),
        Some("custom-future")
    );
    crate::migrations::dsh_model_mappings(&mut value);
    assert_eq!(
        serde_json::to_value(&store).unwrap()["dshModels"],
        value["dshModels"]
    );
}

#[tokio::test]
async fn global_default_edits_preserve_reasoning_and_do_not_overwrite_profile_choices_on_sync() {
    let (home, _parent, gateway) = tests_v02516::fixture();
    fs::write(
        home.path().join("settings.yaml"),
        "agent-default-model: {provider: relay, model: fixture-model, reasoningEffort: low}\n",
    )
    .unwrap();
    gateway
        .attach_dsh_profiles(
            home.path(),
            tests_v02516::options(),
            true,
            &gateway.view().revision,
            None,
        )
        .await
        .unwrap();
    change_default(home.path(), "desktop", "profile-choice", "high");
    let view = gateway
        .read_dsh_file(home.path(), None, editor::FileKey::Settings)
        .unwrap();
    let changed = view
        .text
        .replace("fixture-model", "global-choice")
        .replace("low", "max");
    gateway
        .save_dsh_file(
            home.path(),
            None,
            editor::FileKey::Settings,
            Some(&changed),
            &view.revision,
        )
        .await
        .unwrap();
    let mut options = tests_v02516::options();
    options.model = None;
    gateway
        .attach_dsh_profiles(home.path(), options, true, &gateway.view().revision, None)
        .await
        .unwrap();
    let input = Input::read(home.path(), "desktop").unwrap();
    let (_, rows) = patch_file(input.patch.before.as_deref().unwrap()).unwrap();
    let config = find_row(&rows, DEFAULT)
        .unwrap()
        .unwrap()
        .1
        .get_mapping("config")
        .unwrap();
    assert_eq!(
        string(config.get("model")).as_deref(),
        Some("profile-choice")
    );
    let (effective, _, _) = selection::effective(home.path(), "desktop").unwrap();
    assert_eq!(
        string(effective.configs[DEFAULT].get("model")).as_deref(),
        Some("global-choice")
    );
    assert_eq!(
        string(effective.configs[DEFAULT].get("reasoningEffort")).as_deref(),
        Some("max")
    );
    gateway.stop_dsh(false).await.unwrap();
    assert!(fs::read_to_string(home.path().join("settings.yaml"))
        .unwrap()
        .contains("max"));
}

#[tokio::test]
async fn actual_route_conflict_keeps_files_and_mapping_untouched() {
    let (home, _parent, gateway) = tests_v02516::fixture();
    gateway
        .attach_dsh_profiles(
            home.path(),
            tests_v02516::options(),
            true,
            &gateway.view().revision,
            None,
        )
        .await
        .unwrap();
    let path = home.path().join("profiles/desktop").join(PATCH);
    let original = fs::read_to_string(&path).unwrap();
    let (file, rows) = patch_file(&original).unwrap();
    owned_route(&rows)
        .unwrap()
        .unwrap()
        .as_mapping()
        .unwrap()
        .set("baseURL", "https://changed.example.invalid/v1");
    let external = file.to_string();
    fs::write(&path, &external).unwrap();
    let before = fs::read(gateway.0.data.join("gateway.json")).unwrap();
    let error = gateway
        .restart_dsh(home.path(), &gateway.view().revision)
        .await
        .err()
        .unwrap();
    assert!(error.message.contains("网关地址"));
    assert_eq!(fs::read_to_string(&path).unwrap(), external);
    assert_eq!(
        fs::read(gateway.0.data.join("gateway.json")).unwrap(),
        before
    );
    fs::write(&path, original).unwrap();
    gateway.stop_dsh(false).await.unwrap();
}
