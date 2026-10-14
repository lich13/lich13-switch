use super::*;
use crate::gateway::{Edit, Gateway, Settings};
use std::path::PathBuf;

const CODEX_CONFIG: &str = "model = 'fixture'\nmodel_provider = 'custom'\n[model_providers.custom]\nbase_url = 'https://resume.example.invalid/v1'\nexperimental_bearer_token = 'fixture-resume-token'\nwire_api = 'responses'\n";
const CLAUDE_BASE: &str = "https://claude-resume.example.invalid/v1";
const CLAUDE_TOKEN: &str = "fixture-claude-resume-token";

struct Resumable {
    _temp: tempfile::TempDir,
    data: PathBuf,
    home: PathBuf,
    gateway: Gateway,
}

async fn test_port() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
}

async fn resumable_codex() -> Resumable {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    let home = temp.path().join("codex-home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join("config.toml"), CODEX_CONFIG).unwrap();

    let gateway = Gateway::new(data.clone()).unwrap();
    gateway
        .edit(
            Edit::SaveProvider {
                id: None,
                base_url: "https://resume.example.invalid/v1".into(),
                token: "fixture-resume-token".into(),
                name: Some("resume fixture".into()),
            },
            &gateway.view().revision,
            &home,
        )
        .unwrap();
    gateway
        .edit(
            Edit::Settings {
                settings: Settings {
                    port: test_port().await,
                    ..Default::default()
                },
            },
            &gateway.view().revision,
            &home,
        )
        .unwrap();
    gateway
        .start(&gateway.view().revision, &home)
        .await
        .unwrap();
    gateway.stop_for_exit().await.unwrap();
    drop(gateway);

    Resumable {
        _temp: temp,
        data: data.clone(),
        home,
        gateway: Gateway::new(data).unwrap(),
    }
}

#[tokio::test]
async fn old_restore_flags_are_ignored_and_do_not_block_desired_resume() {
    for legacy in [
        Some(r#"{"restoreGateway":false}"#),
        Some(r#"{"restoreGateway":true}"#),
        Some("{}"),
        None,
    ] {
        let fixture = resumable_codex().await;
        if let Some(legacy) = legacy {
            std::fs::write(fixture.data.join("startup.json"), legacy).unwrap();
        }
        let legacy_description = legacy.unwrap_or("<missing startup.json>");
        let preferences = Service::new(&fixture.data).read().unwrap().0;
        assert_eq!(
            serde_json::to_value(preferences).unwrap(),
            serde_json::json!({})
        );

        fixture.gateway.resume(&fixture.home).await.unwrap();
        let running_view = fixture.gateway.view();
        assert!(running_view.running, "legacy value: {legacy_description}");
        let managed_config = std::fs::read(fixture.home.join("config.toml")).unwrap();

        fixture.gateway.resume(&fixture.home).await.unwrap();
        let repeated_resume = fixture.gateway.view();
        assert!(
            repeated_resume.running,
            "legacy value: {legacy_description}"
        );
        assert_eq!(
            repeated_resume.revision, running_view.revision,
            "legacy value: {legacy_description}"
        );
        assert_eq!(
            std::fs::read(fixture.home.join("config.toml")).unwrap(),
            managed_config,
            "legacy value: {legacy_description}"
        );
        fixture.gateway.stop().await.unwrap();
    }
}

#[test]
fn startup_view_and_silent_mode_have_no_restore_gateway_setting() {
    for legacy in [
        r#"{"restoreGateway":false}"#,
        r#"{"restoreGateway":true}"#,
        "{}",
    ] {
        let preferences: Preferences = serde_json::from_str(legacy).unwrap();
        assert_eq!(
            serde_json::to_value(preferences).unwrap(),
            serde_json::json!({})
        );
    }

    let view = View {
        launch_on_boot: false,
        revision: "fixture-revision".into(),
    };
    assert_eq!(
        serde_json::to_value(view).unwrap(),
        serde_json::json!({
            "launchOnBoot": false,
            "revision": "fixture-revision"
        })
    );
    assert!(silent(&[LOGIN_ARG.into()]));
    assert!(silent(&[LEGACY_LOGIN_ARG.into()]));
}

#[tokio::test]
async fn manual_stop_clears_desired_resume() {
    let fixture = resumable_codex().await;
    fixture.gateway.resume(&fixture.home).await.unwrap();
    assert!(fixture.gateway.view().running);
    fixture.gateway.stop().await.unwrap();
    drop(fixture.gateway);

    let gateway = Gateway::new(fixture.data.clone()).unwrap();
    gateway.resume(&fixture.home).await.unwrap();
    assert!(!gateway.view().running);
}

#[tokio::test]
async fn resume_rejects_a_changed_home_or_managed_pair() {
    let wrong_home = resumable_codex().await;
    let changed_home = wrong_home._temp.path().join("different-home");
    let error = wrong_home.gateway.resume(&changed_home).await.unwrap_err();
    assert_eq!(error.code, "RESUME_CONFLICT");
    assert!(!wrong_home.gateway.view().running);

    let changed_pair = resumable_codex().await;
    let config_path = changed_pair.home.join("config.toml");
    let before = std::fs::read_to_string(&config_path).unwrap();
    let external = before.replace("fixture-resume-token", "fixture-external-token");
    assert_ne!(before, external);
    std::fs::write(&config_path, &external).unwrap();
    let error = changed_pair
        .gateway
        .resume(&changed_pair.home)
        .await
        .unwrap_err();
    assert_eq!(error.code, "RESUME_CONFLICT");
    assert!(!changed_pair.gateway.view().running);
    assert_eq!(std::fs::read_to_string(config_path).unwrap(), external);
}

async fn resumable_claude() -> (tempfile::TempDir, PathBuf, PathBuf, Gateway, Gateway) {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    let home = temp.path().join("claude-home");
    std::fs::create_dir_all(&home).unwrap();
    let settings = serde_json::json!({
        "env": {
            "ANTHROPIC_BASE_URL": CLAUDE_BASE,
            "ANTHROPIC_AUTH_TOKEN": CLAUDE_TOKEN
        },
        "model": "fixture-model"
    });
    std::fs::write(
        home.join("settings.json"),
        serde_json::to_vec_pretty(&settings).unwrap(),
    )
    .unwrap();

    let codex = Gateway::new(data.clone()).unwrap();
    let claude = codex.companion(data.join("claude")).unwrap();
    claude
        .edit(
            Edit::SaveProvider {
                id: None,
                base_url: CLAUDE_BASE.into(),
                token: CLAUDE_TOKEN.into(),
                name: Some("Claude resume fixture".into()),
            },
            &claude.view().revision,
            &home,
        )
        .unwrap();
    claude
        .edit(
            Edit::Settings {
                settings: Settings {
                    port: test_port().await,
                    ..Default::default()
                },
            },
            &claude.view().revision,
            &home,
        )
        .unwrap();
    claude.start(&claude.view().revision, &home).await.unwrap();
    claude.stop_for_exit().await.unwrap();
    drop(claude);
    drop(codex);

    let codex = Gateway::new(data.clone()).unwrap();
    let claude = codex.companion(data.join("claude")).unwrap();
    (temp, data, home, codex, claude)
}

#[tokio::test]
async fn official_claude_profile_blocks_startup_resume() {
    let (_temp, data, home, _codex, claude) = resumable_claude().await;
    let profile = serde_json::json!({
        "version": 1,
        "home": home.to_string_lossy(),
        "mode": "official",
        "apiSettings": null,
        "apiLegacy": null,
        "officialSettings": "{\"env\":{}}",
        "primaryKey": null,
        "apiApprovals": null
    });
    let profile_path = data.join("claude/official-profile.json");
    std::fs::write(&profile_path, serde_json::to_vec_pretty(&profile).unwrap()).unwrap();

    claude.resume(&home).await.unwrap();
    assert!(!claude.view().running);
    assert!(crate::claude_profile::blocks(&data));
}
