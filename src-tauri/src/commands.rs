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

#[tauri::command]
pub async fn preview_sub2api_import(
    app: tauri::AppHandle,
    r: R<'_>,
    client_id: crate::gateway::ClientId,
) -> Result<Option<crate::gateway::exchange::Preview>> {
    use tauri_plugin_dialog::DialogExt;
    let gateway = r.gateway(client_id).clone();
    tauri::async_runtime::spawn_blocking(move || {
        let Some(file) = app
            .dialog()
            .file()
            .add_filter("Sub2API JSON", &["json"])
            .blocking_pick_file()
        else {
            return Ok(None);
        };
        let path = file
            .into_path()
            .map_err(|_| AppError::new("PATH", "请选择本地 JSON 文件"))?;
        gateway.exchange_preview(&path).map(Some)
    })
    .await
    .map_err(|_| AppError::new("IMPORT", "导入文件读取失败"))?
}
#[tauri::command]
pub async fn commit_sub2api_import(
    app: tauri::AppHandle,
    r: R<'_>,
    client_id: crate::gateway::ClientId,
    token: String,
    ids: Vec<usize>,
    expected_revision: String,
) -> Result<crate::gateway::View> {
    let _guard = r.official_tx.lock().await;
    let view = r.gateway(client_id).exchange_commit(
        &token,
        &ids,
        &expected_revision,
        &r.home(client_id)?,
    )?;
    let _ = crate::refresh(&app, &r);
    Ok(view)
}
#[tauri::command]
pub fn cancel_sub2api_import(r: R<'_>, client_id: crate::gateway::ClientId, token: String) {
    r.gateway(client_id).exchange_cancel(&token);
}
#[tauri::command]
pub async fn prepare_sub2api_export(
    app: tauri::AppHandle,
    r: R<'_>,
    client_id: crate::gateway::ClientId,
    replace_template: Option<bool>,
) -> Result<bool> {
    use tauri_plugin_dialog::DialogExt;
    let gateway = r.gateway(client_id).clone();
    if gateway.exchange_has_template() && !replace_template.unwrap_or(false) {
        return Ok(true);
    }
    tauri::async_runtime::spawn_blocking(move || {
        let Some(file) = app
            .dialog()
            .file()
            .set_title("选择 Sub2API 导出文件作为模板")
            .add_filter("Sub2API JSON", &["json"])
            .blocking_pick_file()
        else {
            return Ok(false);
        };
        let path = file
            .into_path()
            .map_err(|_| AppError::new("PATH", "请选择本地 JSON 文件"))?;
        gateway.exchange_template(&path)?;
        Ok(true)
    })
    .await
    .map_err(|_| AppError::new("EXPORT", "模板读取失败"))?
}
#[tauri::command]
pub async fn export_sub2api_providers(
    app: tauri::AppHandle,
    r: R<'_>,
    client_id: crate::gateway::ClientId,
    ids: Vec<String>,
    expected_revision: String,
) -> Result<bool> {
    use tauri_plugin_dialog::DialogExt;
    let gateway = r.gateway(client_id).clone();
    tauri::async_runtime::spawn_blocking(move || {
        let Some(file) = app
            .dialog()
            .file()
            .set_file_name("sub2api-accounts.json")
            .add_filter("JSON", &["json"])
            .blocking_save_file()
        else {
            return Ok(false);
        };
        let path = file
            .into_path()
            .map_err(|_| AppError::new("PATH", "请选择本地文件"))?;
        gateway.exchange_export(&ids, &expected_revision, &path)?;
        Ok(true)
    })
    .await
    .map_err(|_| AppError::new("EXPORT", "导出保存失败"))?
}
