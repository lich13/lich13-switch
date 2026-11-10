use crate::{
    errors::code,
    gateway,
    storage::{self, AppError, Result},
    Runtime,
};
use std::{path::PathBuf, sync::atomic::Ordering};

pub(crate) struct SmokeSnapshot {
    claude_home: PathBuf,
    claude_config: Option<Vec<u8>>,
    home: PathBuf,
    auth: Option<Vec<u8>>,
    config: Option<Vec<u8>>,
}

pub(crate) async fn run(app: &tauri::AppHandle, r: &Runtime) -> Result<()> {
    let result = (|| -> Result<()> {
        #[cfg(windows)]
        crate::startup::smoke_registration()?;
        let mut c = r.core.lock().unwrap();
        let a = c.add_api_key("Smoke A", "fixture-only-a")?;
        let b = c.add_api_key("Smoke B", "fixture-only-b")?;
        let text = "# smoke\nmodel = 'fixture'\nmodel_provider = 'custom'\n[model_providers.custom]\nbase_url = \"https://example.invalid/v1\"\nexperimental_bearer_token = \"fixture-only\"\nwire_api = 'responses'\n[future]\nkeep = true\n";
        c.save_config(text, "missing")?;
        let cfg = c.state()?.config_revision;
        for id in [&a, &b, &a] {
            let rev = c.state()?.auth_revision;
            c.switch_account(id, &rev)?;
            if c.state()?.config_revision != cfg {
                return Err(AppError::new(code::SMOKE, "配置发生意外变化"));
            }
        }
        Ok(())
    })();
    let result = match result {
        Ok(()) => gateway_smoke(r).await,
        Err(e) => Err(e),
    };
    let result = match result {
        Ok(()) => prepare_exit_smoke(r).await,
        Err(e) => Err(e),
    };
    *r.smoke_result.lock().unwrap() = Some(result);
    // Exercise a coded exit request while the gateway is still active.
    // The report is written only after the exit handler verifies restoration.
    app.exit(0);
    Ok(())
}

async fn prepare_exit_smoke(r: &Runtime) -> Result<SmokeSnapshot> {
    let home = r.core.lock().unwrap().home();
    let claude_home = r.home(gateway::ClientId::Claude)?;
    let snapshot = SmokeSnapshot {
        claude_config: storage::read_optional(&claude_home.join("settings.json"))?,
        claude_home,
        auth: storage::read_optional(&home.join("auth.json"))?,
        config: storage::read_optional(&home.join("config.toml"))?.map(|raw| {
            String::from_utf8(raw)
                .unwrap()
                .replace("https://backup.invalid/v1", "https://example.invalid/v1")
                .replace("fixture-backup", "fixture-only")
                .into_bytes()
        }),
        home,
    };
    r.gateway
        .start(&r.gateway.try_view()?.revision, &snapshot.home)
        .await?;
    r.gateway.edit(
        gateway::Edit::Select {
            id: r.gateway.try_view()?.providers[0].id.clone(),
        },
        &r.gateway.try_view()?.revision,
        &snapshot.home,
    )?;
    r.claude
        .start(&r.claude.try_view()?.revision, &snapshot.claude_home)
        .await?;
    Ok(snapshot)
}

pub(crate) fn report_exit(app: &tauri::AppHandle, r: &Runtime, restored: Result<()>) -> Result<()> {
    let result = (|| -> Result<()> {
        let snapshot = r
            .smoke_result
            .lock()
            .unwrap()
            .take()
            .ok_or_else(|| AppError::new(code::SMOKE, "退出验收未初始化"))??;
        restored?;
        if !r.quit_pending.load(Ordering::Relaxed)
            || r.gateway.guarded_home()
            || r.claude.guarded_home()
            || snapshot.claude_config
                != storage::read_optional(&snapshot.claude_home.join("settings.json"))?
            || snapshot.auth != storage::read_optional(&snapshot.home.join("auth.json"))?
            || snapshot.config != storage::read_optional(&snapshot.home.join("config.toml"))?
        {
            return Err(AppError::new(code::SMOKE, "正常退出未恢复配置"));
        }
        Ok(())
    })();
    let data = serde_json::json!({"safeExit":result.is_ok(),"gateway":result.is_ok(),"ok":result.is_ok(),"version":env!("CARGO_PKG_VERSION"),"revision":env!("VELA_REVISION"),"webview":true,"tray":app.tray_by_id("switch").is_some(),"platform":std::env::consts::OS,"error":result.err()});
    storage::atomic_write(
        r.smoke.as_ref().unwrap(),
        serde_json::to_string_pretty(&data).unwrap().as_bytes(),
        None,
    )?;
    if let Some(fixture) = r.fixture.lock().unwrap().take() {
        fixture.close().map_err(storage::io_error)?;
    }
    Ok(())
}

async fn gateway_smoke(r: &Runtime) -> Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let home = r.core.lock().unwrap().home();
    let auth = storage::read_optional(&home.join("auth.json"))?;
    let config = storage::read_optional(&home.join("config.toml"))?;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").map_err(storage::io_error)?;
    let port = listener.local_addr().map_err(storage::io_error)?.port();
    drop(listener);
    r.gateway.edit(
        gateway::Edit::Settings {
            settings: gateway::Settings {
                port,
                ..Default::default()
            },
        },
        &r.gateway.try_view()?.revision,
        &home,
    )?;
    r.gateway.edit(
        gateway::Edit::SaveProvider {
            search_only: None,
            quota_access_token: None,
            id: None,
            base_url: "https://example.invalid/v1".into(),
            token: "fixture-only".into(),
            name: None,
        },
        &r.gateway.try_view()?.revision,
        &home,
    )?;
    r.gateway.edit(
        gateway::Edit::SaveProvider {
            search_only: None,
            quota_access_token: None,
            id: None,
            base_url: "https://backup.invalid/v1".into(),
            token: "fixture-backup".into(),
            name: None,
        },
        &r.gateway.try_view()?.revision,
        &home,
    )?;
    let claude_home = r.home(gateway::ClientId::Claude)?;
    storage::atomic_write(&claude_home.join("settings.json"), br#"{"env":{"ANTHROPIC_BASE_URL":"https://claude.example.invalid","ANTHROPIC_AUTH_TOKEN":"fixture-claude"},"model":"keep"}"#, Some("missing"))?;
    r.claude.import_initial(&claude_home)?;
    r.gateway
        .start(&r.gateway.try_view()?.revision, &home)
        .await?;
    r.claude
        .start(&r.claude.try_view()?.revision, &claude_home)
        .await?;
    if r.gateway.try_view()?.settings.port != port || r.claude.try_view()?.settings.port != port {
        return Err(AppError::new(code::SMOKE, "共享网关端口不一致"));
    }
    let live = storage::read_optional(&home.join("config.toml"))?;
    r.gateway.edit(
        gateway::Edit::Select {
            id: r.gateway.try_view()?.providers[1].id.clone(),
        },
        &r.gateway.try_view()?.revision,
        &home,
    )?;
    if live != storage::read_optional(&home.join("config.toml"))? {
        return Err(AppError::new(code::SMOKE, "运行中换商修改了配置"));
    }
    let config = config.map(|raw| {
        String::from_utf8(raw)
            .unwrap()
            .replace("https://example.invalid/v1", "https://backup.invalid/v1")
            .replace("fixture-only", "fixture-backup")
            .into_bytes()
    });
    let mut connection = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .map_err(storage::io_error)?;
    connection
        .write_all(b"GET /v1/models HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .map_err(storage::io_error)?;
    let mut response = Vec::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        connection.read_to_end(&mut response),
    )
    .await
    .map_err(|_| AppError::new(code::SMOKE, "网关响应超时"))?
    .map_err(storage::io_error)?;
    r.gateway.stop().await?;
    if !r.claude.try_view()?.running {
        return Err(AppError::new(code::SMOKE, "双网关状态未隔离"));
    }
    r.claude.stop().await?;
    if !response.starts_with(b"HTTP/1.1 401")
        || auth != storage::read_optional(&home.join("auth.json"))?
        || config != storage::read_optional(&home.join("config.toml"))?
    {
        return Err(AppError::new(code::SMOKE, "网关或恢复校验失败"));
    }
    Ok(())
}
