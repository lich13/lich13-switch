use crate::{
    power, process_control,
    storage::{AppError, Result},
    Runtime,
};
use std::sync::{atomic::Ordering, Arc};
use tauri::Emitter;
type R<'a> = tauri::State<'a, Arc<Runtime>>;
#[tauri::command]
pub async fn get_clamshell_state(r: R<'_>) -> Result<power::State> {
    let r = r.inner().clone();
    tauri::async_runtime::spawn_blocking(move || r.power.state())
        .await
        .map_err(|_| AppError::new("POWER", "电源状态读取失败"))?
}
#[tauri::command]
pub async fn set_clamshell_awake(
    app: tauri::AppHandle,
    r: R<'_>,
    enabled: bool,
    expected_revision: String,
) -> Result<power::State> {
    let r = r.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let result = r.power.set(enabled, &expected_revision);
        if let Ok(state) = r.power.state() {
            let _ = app.emit("clamshell-state", state);
        }
        result
    })
    .await
    .map_err(|_| AppError::new("POWER", "电源设置失败"))?
}
#[tauri::command]
pub async fn install_power_helper(app: tauri::AppHandle, r: R<'_>) -> Result<power::State> {
    let r = r.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let result = r.power.install();
        if let Ok(state) = r.power.state() {
            let _ = app.emit("clamshell-state", state);
        }
        result
    })
    .await
    .map_err(|_| AppError::new("POWER", "助手安装失败"))?
}
#[tauri::command]
pub async fn remove_power_helper(app: tauri::AppHandle, r: R<'_>) -> Result<power::State> {
    let r = r.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let result = r.power.remove();
        if let Ok(state) = r.power.state() {
            let _ = app.emit("clamshell-state", state);
        }
        result
    })
    .await
    .map_err(|_| AppError::new("POWER", "助手移除失败"))?
}
#[tauri::command]
pub async fn force_quit_codex_clients(r: R<'_>) -> Result<process_control::Outcome> {
    if r.force_quitting.swap(true, Ordering::AcqRel) {
        return Err(AppError::new("BUSY", "正在退出客户端"));
    }
    let runtime = r.inner().clone();
    let result = tauri::async_runtime::spawn_blocking(process_control::force_quit)
        .await
        .map_err(|_| AppError::new("PROCESS", "退出客户端失败"));
    runtime.force_quitting.store(false, Ordering::Release);
    result?
}
#[tauri::command]
pub fn get_startup_error(r: R<'_>) -> Option<AppError> {
    r.cleanup_error
        .lock()
        .unwrap()
        .clone()
        .or_else(|| r.startup_error.lock().unwrap().clone())
}

#[tauri::command]
pub fn cleanup_retired_data(r: R<'_>) -> Result<()> {
    crate::cleanup::retired_statistics(&r.data)?;
    *r.cleanup_error.lock().unwrap() = None;
    Ok(())
}

#[tauri::command]
pub async fn get_app_events(
    r: R<'_>,
    filter: crate::events::Filter,
) -> Result<crate::events::Page> {
    let service = r.diagnostics.clone();
    tauri::async_runtime::spawn_blocking(move || service.query(filter))
        .await
        .map_err(|_| AppError::new("LOG_READ", "日志读取失败"))
}
#[tauri::command]
pub fn get_app_event(r: R<'_>, id: String) -> Option<crate::events::Record> {
    r.diagnostics.detail(&id)
}
#[tauri::command]
pub async fn clear_app_events(r: R<'_>) -> Result<()> {
    r.diagnostics.clear().await
}
