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
use tauri_plugin_notification::NotificationExt;
static ASKING: AtomicBool = AtomicBool::new(false);
static LAST_ERROR: Mutex<Option<String>> = Mutex::new(None);
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct State {
    pub permission: String,
    pub error: Option<String>,
}
#[cfg(target_os = "macos")]
fn permission(request: bool, _id: &str) -> Result<String> {
    use objc2::{
        class, msg_send,
        rc::Retained,
        runtime::{AnyObject, Bool},
    };
    #[link(name = "UserNotifications", kind = "framework")]
    unsafe extern "C" {}
    let (tx, rx) = std::sync::mpsc::channel();
    unsafe {
        let center: Retained<AnyObject> =
            msg_send![class!(UNUserNotificationCenter), currentNotificationCenter];
        let callback = block2::RcBlock::new(move |settings: *mut AnyObject| {
            let status: isize = msg_send![settings, authorizationStatus];
            let _ = tx.send(status);
        });
        let _: () = msg_send![&*center, getNotificationSettingsWithCompletionHandler:&*callback];
        let status = rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .map_err(|_| AppError::new("NOTIFICATION", "通知权限读取超时"))?;
        if status == 0 && request {
            let (tx, rx) = std::sync::mpsc::channel();
            let callback = block2::RcBlock::new(move |granted: Bool, _error: *mut AnyObject| {
                let _ = tx.send(granted.as_bool());
            });
            let _: () = msg_send![&*center, requestAuthorizationWithOptions:7usize, completionHandler:&*callback];
            return rx
                .recv_timeout(std::time::Duration::from_secs(120))
                .map(|ok| if ok { "granted" } else { "denied" }.into())
                .map_err(|_| AppError::new("NOTIFICATION", "通知授权尚未完成"));
        }
        Ok(match status {
            0 => "prompt",
            1 => "denied",
            2..=4 => "granted",
            _ => "unknown",
        }
        .into())
    }
}
#[cfg(windows)]
fn permission(_request: bool, id: &str) -> Result<String> {
    use windows::{
        core::HSTRING,
        UI::Notifications::{NotificationSetting, ToastNotificationManager},
    };
    let setting = ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(id))
        .and_then(|n| n.Setting())
        .map_err(|_| AppError::new("NOTIFICATION", "无法读取系统通知设置"))?;
    Ok(if setting == NotificationSetting::Enabled {
        "granted"
    } else {
        "denied"
    }
    .into())
}
#[cfg(not(any(target_os = "macos", windows)))]
fn permission(_request: bool, _id: &str) -> Result<String> {
    Ok("unknown".into())
}
fn state(app: &tauri::AppHandle, request: bool) -> State {
    match permission(request, &app.config().identifier) {
        Ok(permission) => State {
            permission,
            error: LAST_ERROR.lock().unwrap().clone(),
        },
        Err(e) => State {
            permission: "unknown".into(),
            error: Some(e.message),
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
pub fn connect(app: &tauri::AppHandle, service: &events::Service) {
    let mut receiver = service.subscribe();
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        loop {
            match receiver.recv().await {
                Ok(event) => {
                    let _ = app.emit("app-event", &event);
                    if event.notify
                        && app
                            .state::<Arc<Runtime>>()
                            .core
                            .lock()
                            .unwrap()
                            .preferences()
                            .system_notifications
                        && app.state::<Arc<Runtime>>().smoke.is_none()
                    {
                        if let Some(record) = event.record {
                            let handle = app.clone();
                            tauri::async_runtime::spawn_blocking(move || {
                                if permission(false, &handle.config().identifier)
                                    .is_ok_and(|v| v == "granted")
                                    && handle
                                        .notification()
                                        .builder()
                                        .title("lich13-switch")
                                        .body(record.notification())
                                        .show()
                                        .is_err()
                                {
                                    *LAST_ERROR.lock().unwrap() = Some("系统通知发送失败".into());
                                    let _ =
                                        handle.emit("notification-state", state(&handle, false));
                                }
                            })
                            .await
                            .ok();
                        }
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => break,
            }
        }
    });
}
