use super::*;

pub(super) async fn claude_fixture(
    base: &str,
    token: &str,
) -> (tempfile::TempDir, Gateway, Gateway, PathBuf) {
    let t = tempfile::tempdir().unwrap();
    let codex = Gateway::new(t.path().join("data")).unwrap();
    let claude = codex.companion(t.path().join("data/claude")).unwrap();
    let home = t.path().join("claude-home");
    let text = serde_json::to_vec_pretty(&serde_json::json!({"env":{"ANTHROPIC_BASE_URL":base,"ANTHROPIC_AUTH_TOKEN":token},"permissions":{"deny":["Bash"]}})).unwrap();
    storage::atomic_write(&home.join("settings.json"), &text, None).unwrap();
    claude.import_initial(&home).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    claude
        .set_listener_port(
            listener.local_addr().unwrap().port(),
            &claude.listener_view().revision,
        )
        .unwrap();
    drop(listener);
    (t, codex, claude, home)
}

#[tokio::test]
async fn native_models_paginate_and_config_conflicts_recover_independently() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let copy = seen.clone();
    let port = server(move |req| {
        let copy = copy.clone();
        async move {
            assert_eq!(req.headers()["anthropic-version"], "2023-06-01");
            copy.lock().unwrap().push(req.uri().to_string());
            let page = if req.uri().query().is_some() {
                r#"{"data":[{"id":"Claude-B"}],"has_more":false}"#
            } else {
                r#"{"data":[{"id":"Claude-A"}],"has_more":true,"last_id":"Claude-A"}"#
            };
            Response::new(full(page))
        }
    })
    .await;
    let (_temp, codex, claude, home) =
        claude_fixture(&format!("http://127.0.0.1:{port}/deployment"), "test-key").await;
    let models = claude
        .list_models(&claude.view().providers[0].id, true)
        .await
        .unwrap();
    assert_eq!(models.models, ["Claude-A", "Claude-B"]);
    assert_eq!(
        *seen.lock().unwrap(),
        [
            "/deployment/v1/models",
            "/deployment/v1/models?after_id=Claude-A"
        ]
    );
    assert_eq!(claude.view().providers[0].health.requests, 0);
    claude.start(&claude.view().revision, &home).await.unwrap();
    let current = std::fs::read_to_string(home.join("settings.json")).unwrap();
    let foreign = current.replace("\"permissions\"", "\"externalPermissions\"");
    std::fs::write(home.join("settings.json"), &foreign).unwrap();
    claude.stop_for_exit().await.unwrap();
    assert!(std::fs::read_to_string(home.join("settings.json"))
        .unwrap()
        .contains("externalPermissions"));
    claude.resume(&home).await.unwrap();
    let managed = std::fs::read_to_string(home.join("settings.json")).unwrap();
    let changed = super::super::claude_config::patch(
        &managed,
        &takeover::Pair::new("https://external.invalid", "external-key"),
    )
    .unwrap();
    std::fs::write(home.join("settings.json"), &changed).unwrap();
    assert!(claude.stop().await.is_err());
    assert_eq!(
        std::fs::read_to_string(home.join("settings.json")).unwrap(),
        changed
    );
    assert!(!codex.view().running);
    std::fs::write(home.join("settings.json"), managed).unwrap();
    claude.stop().await.unwrap();
}

#[tokio::test]
async fn claude_missing_settings_are_created_without_other_fields() {
    let temp = tempfile::tempdir().unwrap();
    let codex = Gateway::new(temp.path().join("data")).unwrap();
    let claude = codex.companion(temp.path().join("data/claude")).unwrap();
    let home = temp.path().join("missing-home");
    claude
        .edit(
            Edit::SaveProvider {
                search_only: None,
                quota_access_token: None,
                id: None,
                base_url: "https://fixture.invalid/root".into(),
                token: "fixture-only".into(),
                name: None,
            },
            &claude.view().revision,
            &home,
        )
        .unwrap();
    let id = claude.view().providers[0].id.clone();
    claude
        .edit(Edit::Select { id }, &claude.view().revision, &home)
        .unwrap();
    let value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(home.join("settings.json")).unwrap()).unwrap();
    assert_eq!(value.as_object().unwrap().len(), 1);
    assert_eq!(value["env"].as_object().unwrap().len(), 2);
    assert!(!home.join("auth.json").exists());
}

#[tokio::test]
async fn visual_config_save_keeps_gateway_fields_and_conflicts_do_not_lose_drafts() {
    let (_temp, codex, claude, home) =
        claude_fixture("https://fixture.invalid/sub", "fixture-token").await;
    let original = claude.read_config(&home).unwrap();
    claude.start(&claude.view().revision, &home).await.unwrap();
    let managed = claude.read_config(&home).unwrap();
    assert!(managed.guarded);
    let text = managed
        .text
        .replacen('{', "{\n  \"language\": \"中文\",", 1);
    let saved = claude.save_config(&home, &text, &managed.revision).unwrap();
    assert_eq!(saved.text, text);
    assert!(
        takeover::pair_for(ClientId::Claude, &managed.text).unwrap()
            == takeover::pair_for(ClientId::Claude, &saved.text).unwrap()
    );
    let bad = text.replace("127.0.0.1", "other.invalid");
    assert_eq!(
        claude
            .save_config(&home, &bad, &saved.revision)
            .err()
            .unwrap()
            .code,
        "GATEWAY_ACTIVE"
    );
    let outside = text.replacen("中文", "English", 1);
    std::fs::write(home.join("settings.json"), &outside).unwrap();
    assert_eq!(
        claude
            .save_config(&home, &text, &saved.revision)
            .err()
            .unwrap()
            .code,
        "CONFLICT"
    );
    assert_eq!(
        std::fs::read_to_string(home.join("settings.json")).unwrap(),
        outside
    );
    claude.stop().await.unwrap();
    let stopped = claude.read_config(&home).unwrap();
    assert!(!stopped.guarded);
    assert!(stopped.text.contains("English"));
    let original_pair = takeover::pair_for(ClientId::Claude, &original.text).unwrap();
    let stopped_pair = takeover::pair_for(ClientId::Claude, &stopped.text).unwrap();
    let expected_base = super::model::base_url(
        original_pair.base_url.as_deref().unwrap(),
        claude.listener_view().port,
    )
    .unwrap()
    .to_string();
    assert_eq!(
        stopped_pair.base_url.as_deref(),
        Some(expected_base.as_str())
    );
    assert_eq!(stopped_pair.token, original_pair.token);
    assert!(!codex.view().running);
    assert!(!home.join("auth.json").exists());
}

#[tokio::test]
async fn editor_and_gateway_start_are_serial_and_use_revision_checks() {
    let (_temp, _codex, claude, home) =
        claude_fixture("https://fixture.invalid/sub", "fixture-token").await;
    let before = claude.read_config(&home).unwrap();
    let gateway = claude.clone();
    let target = home.clone();
    let revision = claude.view().revision;
    let start = tokio::spawn(async move { gateway.start(&revision, &target).await });
    let text = before
        .text
        .replacen('{', "{\n  \"showTurnDuration\": false,", 1);
    let result = claude.save_config(&home, &text, &before.revision);
    start.await.unwrap().unwrap();
    if let Err(e) = result {
        assert!(matches!(e.code.as_str(), "GATEWAY_ACTIVE" | "CONFLICT"));
    }
    let current = claude.read_config(&home).unwrap();
    assert!(current.guarded);
    assert!(takeover::pair_for(ClientId::Claude, &current.text)
        .unwrap()
        .base_url
        .unwrap()
        .contains("127.0.0.1"));
    claude.stop().await.unwrap();
}

#[tokio::test]
async fn retired_credentials_migrate_once_without_changing_provider_or_other_files() {
    let (temp, codex, claude, home) =
        claude_fixture("https://fixture.invalid/sub", "fixture-token").await;
    let before_config = std::fs::read(home.join("settings.json")).unwrap();
    let data = temp.path().join("data/claude");
    let path = data.join("gateway.json");
    let original = std::fs::read(&path).unwrap();
    let mut old: serde_json::Value = serde_json::from_slice(&original).unwrap();
    old["schema"] = 1.into();
    old["proxies"] = serde_json::json!([{"id":"retired","password":"obsolete-fixture"}]);
    old["providers"][0]["proxyId"] = "retired".into();
    std::fs::write(&path, old.to_string()).unwrap();
    std::fs::write(data.join("proxy-profiles.json"), "obsolete-fixture").unwrap();
    let upgraded = codex.companion(data.clone()).unwrap();
    assert!(!upgraded.view().recovery_pending);
    assert_eq!(
        upgraded.view().providers[0].id,
        claude.view().providers[0].id
    );
    assert_eq!(upgraded.view().selected, claude.view().selected);
    let bytes = std::fs::read(&path).unwrap();
    assert!(!String::from_utf8_lossy(&bytes).contains("obsolete-fixture"));
    assert!(!String::from_utf8_lossy(&bytes).contains("proxyId"));
    assert!(!data.join("proxy-profiles.json").exists());
    drop(upgraded);
    let reopened = codex.companion(data.clone()).unwrap();
    assert_eq!(bytes, std::fs::read(&path).unwrap());
    assert_eq!(
        before_config,
        std::fs::read(home.join("settings.json")).unwrap()
    );
    // Cleanup failures stop import and automatic start until retry succeeds.
    drop(reopened);
    std::fs::create_dir(data.join("proxy-profiles.json")).unwrap();
    let blocked = codex.companion(data.clone()).unwrap();
    assert!(blocked.view().recovery_pending);
    assert!(blocked.import_initial(&home).is_err());
    assert!(blocked
        .start(&blocked.view().revision, &home)
        .await
        .is_err());
    std::fs::remove_dir(data.join("proxy-profiles.json")).unwrap();
    blocked.stop().await.unwrap();
    assert!(!blocked.view().recovery_pending);
}

#[tokio::test]
#[ignore = "Explicit real Claude CLI and provider acceptance; reads a supplied private settings file and uses an isolated home"]
async fn real_claude_http_and_cli_stream_isolated() {
    let settings =
        std::env::var_os("LICH13_SWITCH_CLAUDE_SETTINGS").expect("private settings path required");
    let cli = std::env::var_os("LICH13_SWITCH_CLAUDE_CLI").expect("official CLI path required");
    let original = std::fs::read(&settings).unwrap();
    let value: serde_json::Value = serde_json::from_slice(&original).unwrap();
    let base = value["env"]["ANTHROPIC_BASE_URL"].as_str().unwrap();
    let token = value["env"]["ANTHROPIC_AUTH_TOKEN"].as_str().unwrap();
    let (_temp, _codex, claude, home) = claude_fixture(base, token).await;
    let configured = value["env"]["ANTHROPIC_MODEL"]
        .as_str()
        .or_else(|| value["model"].as_str())
        .map(str::to_owned);
    let model = if let Some(model) = configured {
        model
    } else {
        let models = claude
            .list_models(&claude.view().providers[0].id, true)
            .await
            .unwrap();
        models
            .models
            .iter()
            .find(|m| m.contains("claude") && m.contains("haiku"))
            .or_else(|| {
                models
                    .models
                    .iter()
                    .find(|m| m.contains("claude") && m.contains("sonnet"))
            })
            .or_else(|| models.models.iter().find(|m| m.contains("claude")))
            .expect("provider must expose a Claude model")
            .clone()
    };
    let doc = claude.read_config(&home).unwrap();
    let mut edited: serde_json::Value = serde_json::from_str(&doc.text).unwrap();
    edited["model"] = model.clone().into();
    edited["language"] = "English".into();
    claude
        .save_config(
            &home,
            &serde_json::to_string_pretty(&edited).unwrap(),
            &doc.revision,
        )
        .unwrap();
    let before = std::fs::read(home.join("settings.json")).unwrap();
    claude.start(&claude.view().revision, &home).await.unwrap();
    let body=serde_json::to_vec(&serde_json::json!({"model":model,"max_tokens":64,"messages":[{"role":"user","content":"Reply exactly gateway-claude-ok"}]})).unwrap();
    let response = request(
        &claude,
        "/v1/messages",
        body,
        vec![
            ("anthropic-version", "2023-06-01"),
            ("content-type", "application/json"),
        ],
    )
    .await;
    let status = response.status();
    let response = response.into_body().collect().await.unwrap().to_bytes();
    let http_ok = serde_json::from_slice::<serde_json::Value>(&response)
        .ok()
        .is_some_and(|v| v["type"] == "message" && v["stop_reason"].is_string());
    claude.0.inner.lock().unwrap().last_successful = None;
    let mut command = tokio::process::Command::new(cli);
    command.args([
        "-p",
        "Reply exactly gateway-claude-ok. Do not use tools.",
        "--output-format",
        "stream-json",
        "--include-partial-messages",
        "--verbose",
        "--no-session-persistence",
        "--tools",
        "",
        "--strict-mcp-config",
        "--mcp-config",
        r#"{"mcpServers":{}}"#,
        "--setting-sources",
        "user",
    ]);
    for key in [
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_MODEL",
        "ANTHROPIC_AUTH_TOKEN",
        "ANTHROPIC_BASE_URL",
        "CLAUDE_CODE_OAUTH_TOKEN",
        "CLAUDE_CODE_USE_BEDROCK",
        "CLAUDE_CODE_USE_VERTEX",
        "CLAUDE_CODE_USE_FOUNDRY",
    ] {
        command.env_remove(key);
    }
    let result = tokio::time::timeout(
        Duration::from_secs(120),
        command
            .env("CLAUDE_CONFIG_DIR", &home)
            .env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1")
            .env("DISABLE_AUTOUPDATER", "1")
            .current_dir(&home)
            .kill_on_drop(true)
            .output(),
    )
    .await;
    let completed = claude.view().last_successful.is_some();
    claude.stop().await.unwrap();
    assert_eq!(
        storage::digest(&std::fs::read(&settings).unwrap()),
        storage::digest(&original)
    );
    assert_eq!(
        storage::digest(&std::fs::read(home.join("settings.json")).unwrap()),
        storage::digest(&before)
    );
    assert!(
        status.is_success() && http_ok,
        "Claude HTTP returned status {status}; body withheld"
    );
    let output = result.expect("isolated Claude CLI timed out").unwrap();
    let stream = String::from_utf8_lossy(&output.stdout);
    let result_ok = stream
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .any(|v| {
            v["type"] == "result"
                && v["is_error"] == false
                && v["result"]
                    .as_str()
                    .is_some_and(|s| s.contains("gateway-claude-ok"))
        });
    if !(output.status.success() && result_ok) {
        let local = claude.0.inner.lock().unwrap().store.local_token.clone();
        let safe = String::from_utf8_lossy(&output.stderr)
            .replace(token, "[redacted]")
            .replace(&local, "[redacted]");
        let summaries: Vec<_> = stream.lines().filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok()).map(|v| serde_json::json!({"type":v["type"],"subtype":v["subtype"],"is_error":v["is_error"]})).collect();
        eprintln!(
            "CLI exit {}; events {}; stderr: {}",
            output.status,
            serde_json::to_string(&summaries).unwrap(),
            safe.chars().take(2000).collect::<String>()
        );
    }
    assert!(
        output.status.success() && result_ok,
        "Claude CLI failed; private output withheld"
    );
    assert!(
        stream.contains("stream_event") && completed,
        "Claude streaming completion missing"
    );
    println!("Claude HTTP and official CLI SSE passed; isolated settings restored, real settings unchanged");
}
