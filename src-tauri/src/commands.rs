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
) -> Result<Option<crate::exchange_template::Prepared>> {
    use tauri_plugin_dialog::DialogExt;
    let gateway = r.gateway(client_id).clone();
    if gateway.exchange_has_template() && !replace_template.unwrap_or(false) {
        return Ok(Some(crate::exchange_template::Prepared::ready()));
    }
    tauri::async_runtime::spawn_blocking(move || {
        let Some(file) = app
            .dialog()
            .file()
            .set_title("选择完整的 Sub2API 模板（旧模板需重新选择）")
            .add_filter("Sub2API JSON", &["json"])
            .blocking_pick_file()
        else {
            return Ok(None);
        };
        let path = file
            .into_path()
            .map_err(|_| AppError::new("PATH", "请选择本地 JSON 文件"))?;
        gateway.exchange_template(&path).map(Some)
    })
    .await
    .map_err(|_| AppError::new("EXPORT", "模板读取失败"))?
}
#[tauri::command]
pub fn select_sub2api_template(
    r: R<'_>,
    client_id: crate::gateway::ClientId,
    token: String,
    id: usize,
) -> Result<()> {
    r.gateway(client_id).exchange_select_template(&token, id)
}

#[tauri::command]
pub async fn preview_account_import(
    app: tauri::AppHandle,
    r: R<'_>,
) -> Result<Option<crate::core::exchange::Preview>> {
    use tauri_plugin_dialog::DialogExt;
    let r = r.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let Some(files) = app
            .dialog()
            .file()
            .set_title("选择 Codex、Sub2API 或 CLIProxyAPI 账号文件")
            .add_filter("JSON", &["json"])
            .blocking_pick_files()
        else {
            return Ok(None);
        };
        let paths = files
            .into_iter()
            .map(|f| {
                f.into_path()
                    .map_err(|_| AppError::new("PATH", "请选择本地 JSON 文件"))
            })
            .collect::<Result<Vec<_>>>()?;
        r.core.lock().unwrap().exchange_preview(&paths).map(Some)
    })
    .await
    .map_err(|_| AppError::new("IMPORT", "账号文件读取失败"))?
}
#[tauri::command]
pub fn cancel_account_exchange(r: R<'_>, token: String) {
    r.core.lock().unwrap().exchange_cancel(&token);
}
#[tauri::command]
pub async fn commit_account_import(
    app: tauri::AppHandle,
    r: R<'_>,
    token: String,
    selected: Vec<crate::core::exchange::Selection>,
    expected_revision: String,
) -> Result<crate::core::ViewState> {
    let _guard = r.official_tx.lock().await;
    r.core
        .lock()
        .unwrap()
        .exchange_commit(&token, &selected, &expected_revision)?;
    crate::refresh(&app, &r)
}
#[tauri::command]
pub fn preview_account_export(r: R<'_>) -> crate::core::exchange::ExportPreview {
    let mut core = r.core.lock().unwrap();
    core.exchange_prune();
    core.exchange_export_preview()
}
#[tauri::command]
pub async fn prepare_account_template(
    app: tauri::AppHandle,
    r: R<'_>,
    ids: Vec<String>,
    replace_template: Option<bool>,
) -> Result<Option<crate::exchange_template::Prepared>> {
    use tauri_plugin_dialog::DialogExt;
    if !replace_template.unwrap_or(false) && r.core.lock().unwrap().exchange_template_ready(&ids) {
        return Ok(Some(crate::exchange_template::Prepared::ready()));
    }
    let r = r.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let Some(file) = app
            .dialog()
            .file()
            .set_title("选择独立的 Sub2API OAuth 模板")
            .add_filter("JSON", &["json"])
            .blocking_pick_file()
        else {
            return Ok(None);
        };
        let path = file
            .into_path()
            .map_err(|_| AppError::new("PATH", "请选择本地 JSON 文件"))?;
        r.core
            .lock()
            .unwrap()
            .exchange_prepare_template(&path)
            .map(Some)
    })
    .await
    .map_err(|_| AppError::new("EXPORT", "OAuth 模板读取失败"))?
}
#[tauri::command]
pub fn select_account_template(r: R<'_>, token: String, id: usize) -> Result<()> {
    r.core.lock().unwrap().exchange_select_template(&token, id)
}
#[tauri::command]
pub async fn export_accounts(
    app: tauri::AppHandle,
    r: R<'_>,
    ids: Vec<String>,
    expected_revision: String,
    format: String,
) -> Result<bool> {
    use tauri_plugin_dialog::DialogExt;
    let r = r.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let file = if format == "cliproxyapi" {
            app.dialog()
                .file()
                .set_title("选择 CLIProxyAPI 账号导出目录")
                .blocking_pick_folder()
        } else if format == "sub2api" {
            app.dialog()
                .file()
                .set_file_name("sub2api-oauth-accounts.json")
                .add_filter("JSON", &["json"])
                .blocking_save_file()
        } else {
            return Err(AppError::new("EXPORT", "导出格式不支持"));
        };
        let Some(file) = file else {
            return Ok(false);
        };
        let path = file
            .into_path()
            .map_err(|_| AppError::new("PATH", "请选择本地路径"))?;
        r.core
            .lock()
            .unwrap()
            .exchange_export(&ids, &expected_revision, &format, &path)?;
        Ok(true)
    })
    .await
    .map_err(|_| AppError::new("EXPORT", "账号导出失败"))?
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
