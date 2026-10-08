use super::*;
use crate::{gateway::ClientId, Runtime};
use std::sync::Arc;
use tauri::Emitter;
type R<'a> = tauri::State<'a, Arc<Runtime>>;
#[tauri::command]
pub fn get_claude_profile(r: R<'_>) -> Result<View> {
    view(&r.data, &r.home(ClientId::Claude)?, &user_home()?)
}
#[tauri::command]
pub async fn switch_claude_profile(
    r: R<'_>,
    app: tauri::AppHandle,
    mode: Mode,
    expected_revision: String,
) -> Result<View> {
    let _guard = r.official_tx.lock().await;
    let home = r.home(ClientId::Claude)?;
    let user = user_home()?;
    if matches!(
        r.claude_login.lock().unwrap().state.phase.as_str(),
        "waiting" | "cancelling"
    ) {
        return Err(fail("请先完成或取消 Claude 登录"));
    }
    if login::running_sessions()? {
        return Err(AppError::new(
            "CLAUDE_RUNNING",
            "请先退出正在运行的 Claude Code 会话",
        ));
    }
    if view(&r.data, &home, &user)?.revision != expected_revision {
        return Err(conflict());
    }
    // Obtain official auth status before touching any configuration. Failure is
    // conservative: preserve onboarding instead of assuming no active account.
    let logged_in = login::status(&home).await.unwrap_or(true);
    // Capture auxiliary fields before the gateway's deliberate settings write.
    // Only that settings delta can refresh the optimistic revision.
    if view(&r.data, &home, &user)?.revision != expected_revision {
        return Err(conflict());
    }
    let before = snapshot(&home, &user)?;
    let mut edit_revision = expected_revision;
    if mode == Mode::Official {
        let was_running = r.claude.guarded_home();
        let expected_config = revision(before[&Role::Settings].as_deref());
        r.claude
            .stop_checked(if was_running {
                Some(&expected_config)
            } else {
                None
            })
            .await?; // clears dormant resume intent
        let after = snapshot(&home, &user)?;
        if before
            .iter()
            .any(|(role, text)| (*role != Role::Settings || !was_running) && after[role] != *text)
        {
            return Err(conflict());
        }
        edit_revision = view(&r.data, &home, &user)?.revision;
    }
    let result = switch(&r.data, &home, &user, mode, &edit_revision, logged_in)?;
    r.claude.observe_home(&home);
    let _ = app.emit("claude-profile-state", &result);
    if let Ok(doc) = r.claude.read_config(&home) {
        let _ = app.emit(
            "config-state",
            serde_json::json!({"clientId":"claude","revision":doc.revision,"guarded":doc.guarded}),
        );
    }
    let _ = app.emit("gateway-state", r.claude.view());
    Ok(result)
}
#[tauri::command]
pub async fn recover_claude_profile(r: R<'_>, app: tauri::AppHandle) -> Result<View> {
    let _guard = r.official_tx.lock().await;
    if login::running_sessions()? {
        return Err(AppError::new(
            "CLAUDE_RUNNING",
            "请先退出正在运行的 Claude Code 会话",
        ));
    }
    let home = r.home(ClientId::Claude)?;
    recover(&r.data, &home, &user_home()?)?;
    let result = view(&r.data, &home, &user_home()?)?;
    let _ = app.emit("claude-profile-state", &result);
    Ok(result)
}
#[tauri::command]
pub async fn claude_login_status(r: R<'_>) -> Result<login::State> {
    if matches!(
        r.claude_login.lock().unwrap().state.phase.as_str(),
        "waiting" | "cancelling"
    ) {
        return Ok(r.claude_login.lock().unwrap().state.clone());
    }
    let home = r.home(ClientId::Claude)?;
    let result = login::status(&home).await;
    let state = match result {
        Ok(authenticated) => login::State {
            phase: if authenticated { "complete" } else { "idle" }.into(),
            authenticated,
            error: None,
        },
        Err(e) => login::State {
            error: Some(e.message),
            ..Default::default()
        },
    };
    r.claude_login.lock().unwrap().state = state.clone();
    Ok(state)
}
#[tauri::command]
pub async fn start_claude_login(r: R<'_>, app: tauri::AppHandle) -> Result<login::State> {
    let _guard = r.official_tx.lock().await;
    let home = r.home(ClientId::Claude)?;
    let profile = view(&r.data, &home, &user_home()?)?;
    if profile.mode != Mode::Official || profile.conflict.is_some() {
        return Err(fail("请先切换到有效的 Claude 官方配置"));
    }
    if matches!(
        r.claude_login.lock().unwrap().state.phase.as_str(),
        "waiting" | "cancelling"
    ) {
        return Err(fail("Claude 登录正在进行"));
    }
    if login::running_sessions()? {
        return Err(AppError::new(
            "CLAUDE_RUNNING",
            "请先退出正在运行的 Claude Code 会话",
        ));
    }
    if login::status(&home).await.unwrap_or(false) {
        let state = login::State {
            phase: "complete".into(),
            authenticated: true,
            error: None,
        };
        r.claude_login.lock().unwrap().state = state.clone();
        return Ok(state);
    }
    let (cancel, receiver) = tokio::sync::oneshot::channel();
    let state = login::State {
        phase: "waiting".into(),
        ..Default::default()
    };
    {
        let mut session = r.claude_login.lock().unwrap();
        session.state = state.clone();
        session.cancel = Some(cancel);
    }
    let runtime = r.inner().clone();
    tauri::async_runtime::spawn(async move {
        let result = login::run(&home, receiver).await;
        let state = match result {
            Ok(true) => login::State {
                phase: "complete".into(),
                authenticated: true,
                error: None,
            },
            Ok(false) => login::State {
                phase: "cancelled".into(),
                ..Default::default()
            },
            Err(e) => login::State {
                phase: "failed".into(),
                error: Some(e.message),
                ..Default::default()
            },
        };
        {
            let mut session = runtime.claude_login.lock().unwrap();
            session.cancel = None;
            session.state = state.clone();
        }
        let _ = app.emit("claude-login-state", state);
    });
    Ok(state)
}
#[tauri::command]
pub fn cancel_claude_login(r: R<'_>) -> Result<()> {
    let mut session = r.claude_login.lock().unwrap();
    if let Some(cancel) = session.cancel.take() {
        session.state.phase = "cancelling".into();
        let _ = cancel.send(());
    }
    Ok(())
}
