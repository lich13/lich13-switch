use crate::{
    events,
    storage::{AppError, Result},
    Runtime,
};
use serde::Serialize;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use tauri::{Emitter, Manager};
mod delivery;
mod native;
use native::permission;
static ASKING: AtomicBool = AtomicBool::new(false);
static DELIVERY: Mutex<delivery::Status> = Mutex::new(delivery::Status::Idle);
static LAST_PERMISSION: Mutex<Option<String>> = Mutex::new(None);
static LAST_ERROR: Mutex<Option<String>> = Mutex::new(None);
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct State {
    pub permission: String,
    pub error: Option<String>,
    pub delivery: delivery::Status,
}
fn state(app: &tauri::AppHandle, request: bool) -> State {
    match permission(request, &app.config().identifier) {
        Ok(permission) => {
            *LAST_PERMISSION.lock().unwrap() = Some(permission.clone());
            State {
                permission,
                error: LAST_ERROR.lock().unwrap().clone(),
                delivery: *DELIVERY.lock().unwrap(),
            }
        }
        Err(e) => State {
            permission: "unknown".into(),
            error: Some(e.message),
            delivery: *DELIVERY.lock().unwrap(),
        },
    }
}
#[tauri::command]
pub async fn notification_permission(
    app: tauri::AppHandle,
    request: Option<bool>,
    r: tauri::State<'_, Arc<Runtime>>,
) -> Result<State> {
    let request =
        request.unwrap_or(false) && r.core.lock().unwrap().preferences().system_notifications;
    tauri::async_runtime::spawn_blocking(move || state(&app, request))
        .await
        .map_err(|_| AppError::new("NOTIFICATION", "通知状态读取失败"))
}
pub fn on_manual_open(app: &tauri::AppHandle) {
    let Some(r) = app.try_state::<Arc<Runtime>>() else {
        return;
    };
    if r.smoke.is_some()
        || !r.core.lock().unwrap().preferences().system_notifications
        || ASKING.swap(true, Ordering::AcqRel)
    {
        return;
    }
    let app = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let value = state(&app, true);
        let _ = app.emit("notification-state", value);
        ASKING.store(false, Ordering::Release);
    });
}
fn publish_delivery(app: &tauri::AppHandle, status: delivery::Status, error: Option<String>) {
    *DELIVERY.lock().unwrap() = status;
    *LAST_ERROR.lock().unwrap() = error;
    let _ = app.emit(
        "notification-state",
        State {
            permission: LAST_PERMISSION
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_else(|| "unknown".into()),
            error: LAST_ERROR.lock().unwrap().clone(),
            delivery: status,
        },
    );
}
async fn submit(app: tauri::AppHandle, body: String) -> Result<()> {
    tauri::async_runtime::spawn_blocking(move || {
        let current = permission(false, &app.config().identifier)?;
        *LAST_PERMISSION.lock().unwrap() = Some(current.clone());
        if current != "granted" {
            return Err(AppError::new("NOTIFICATION_DENIED", "系统未允许通知"));
        }
        native::send(&app.config().identifier, "lich13-switch", &body)
    })
    .await
    .map_err(|_| AppError::new("NOTIFICATION", "通知发送任务中断"))?
}
#[tauri::command]
pub async fn test_notification(app: tauri::AppHandle) -> Result<State> {
    publish_delivery(&app, delivery::Status::Pending, None);
    let result = submit(app.clone(), "测试通知".into()).await;
    match &result {
        Ok(()) => publish_delivery(&app, delivery::Status::Accepted, None),
        Err(e) => publish_delivery(&app, delivery::Status::Failed, Some(e.message.clone())),
    }
    tauri::async_runtime::spawn_blocking(move || state(&app, false))
        .await
        .map_err(|_| AppError::new("NOTIFICATION", "通知状态读取失败"))
}
#[tauri::command]
pub fn open_notification_settings() -> Result<()> {
    #[cfg(target_os = "macos")]
    let target = "x-apple.systempreferences:com.apple.Notifications-Settings.extension";
    #[cfg(windows)]
    let target = "ms-settings:notifications";
    #[cfg(not(any(target_os = "macos", windows)))]
    return Err(AppError::new(
        "NOTIFICATION",
        "此平台不支持系统通知设置入口",
    ));
    #[cfg(any(target_os = "macos", windows))]
    open::that(target).map_err(|_| AppError::new("NOTIFICATION", "无法打开系统通知设置"))
}
pub fn connect(app: &tauri::AppHandle, service: &events::Service) {
    native::initialize();
    let mut receiver = service.subscribe();
    let app = app.clone();
    let journal = service.clone();
    let gate = Arc::new(Mutex::new(delivery::Gate::default()));
    tauri::async_runtime::spawn(async move {
        loop {
            match receiver.recv().await {
                Ok(event) => {
                    let _ = app.emit("app-event", &event);
                    let r = app.state::<Arc<Runtime>>();
                    if !event.notify
                        || !r.core.lock().unwrap().preferences().system_notifications
                        || r.smoke.is_some()
                    {
                        continue;
                    }
                    let Some(record) = event.record else { continue };
                    let current = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs();
                    if record.notified_at > 0 && current.saturating_sub(record.notified_at) < 300 {
                        continue;
                    }
                    let key = delivery::key(&record);
                    if !gate.lock().unwrap().begin(&key, std::time::Instant::now()) {
                        continue;
                    }
                    let app = app.clone();
                    let gate = gate.clone();
                    let journal = journal.clone();
                    tauri::async_runtime::spawn(async move {
                        publish_delivery(&app, delivery::Status::Pending, None);
                        let mut result = Err(AppError::new("NOTIFICATION", "通知尚未发送"));
                        for attempt in 0..3 {
                            if !app
                                .state::<Arc<Runtime>>()
                                .core
                                .lock()
                                .unwrap()
                                .preferences()
                                .system_notifications
                            {
                                break;
                            }
                            result = submit(app.clone(), record.notification()).await;
                            if result.is_ok()
                                || result
                                    .as_ref()
                                    .err()
                                    .is_some_and(|e| e.code == "NOTIFICATION_DENIED")
                            {
                                break;
                            }
                            if attempt < 2 {
                                tokio::time::sleep(std::time::Duration::from_secs(
                                    if attempt == 0 { 2 } else { 10 },
                                ))
                                .await;
                            }
                        }
                        gate.lock().unwrap().finish(
                            &key,
                            result.is_ok(),
                            std::time::Instant::now(),
                        );
                        match result {
                            Ok(()) => {
                                journal.notification_accepted(record.id);
                                publish_delivery(&app, delivery::Status::Accepted, None);
                            }
                            Err(e) => {
                                publish_delivery(&app, delivery::Status::Failed, Some(e.message))
                            }
                        }
                    });
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => break,
            }
        }
    });
}

#[cfg(test)]
#[path = "notifications/delivery_tests.rs"]
mod delivery_tests;
