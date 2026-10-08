use crate::storage::{AppError, Result};
#[cfg(target_os = "macos")]
pub fn permission(request: bool, _id: &str) -> Result<String> {
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
            let callback = block2::RcBlock::new(move |granted: Bool, error: *mut AnyObject| {
                let result = if error.is_null() {
                    Ok(granted.as_bool())
                } else {
                    let code: isize = msg_send![error, code];
                    Err(code)
                };
                let _ = tx.send(result);
            });
            let _: () = msg_send![&*center, requestAuthorizationWithOptions:7usize, completionHandler:&*callback];
            let result = rx
                .recv_timeout(std::time::Duration::from_secs(120))
                .map_err(|_| AppError::new("NOTIFICATION", "通知授权尚未完成"))?;
            return result
                .map(|ok| if ok { "granted" } else { "denied" }.into())
                .map_err(|code| {
                    AppError::new("NOTIFICATION", &format!("系统通知授权失败（{code}）"))
                });
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
pub fn permission(_request: bool, id: &str) -> Result<String> {
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
pub fn permission(_request: bool, _id: &str) -> Result<String> {
    Ok("unknown".into())
}

#[cfg(target_os = "macos")]
mod mac {
    use super::*;
    use objc2::{class, define_class, msg_send, rc::Retained, runtime::AnyObject, ClassType};
    use objc2_foundation::{NSObject, NSObjectProtocol, NSString};
    use std::sync::Once;
    #[link(name = "UserNotifications", kind = "framework")]
    unsafe extern "C" {}
    define_class!(
        #[unsafe(super(NSObject))]
        struct ForegroundNotifications;
        impl ForegroundNotifications {
            #[unsafe(method(userNotificationCenter:willPresentNotification:withCompletionHandler:))]
            fn present(&self,_center:*mut AnyObject,_notification:*mut AnyObject,completion:&block2::Block<dyn Fn(usize)>) {
                // Sound + list + banner. Runs on the notification center queue.
                completion.call((1usize | 8usize | 16usize,));
            }
        }
        unsafe impl NSObjectProtocol for ForegroundNotifications {}
    );
    pub fn initialize() {
        static ONCE: Once = Once::new();
        ONCE.call_once(|| unsafe {
            let delegate: Retained<ForegroundNotifications> =
                msg_send![ForegroundNotifications::class(), new];
            let center: Retained<AnyObject> =
                msg_send![class!(UNUserNotificationCenter), currentNotificationCenter];
            let _: () = msg_send![&*center,setDelegate:&*delegate];
            // UNUserNotificationCenter keeps a weak delegate. One process-wide
            // delegate intentionally lives until application termination.
            std::mem::forget(delegate);
        });
    }
    pub fn send(title: &str, body: &str) -> Result<()> {
        initialize();
        let (tx, rx) = std::sync::mpsc::channel();
        unsafe {
            let content: Retained<AnyObject> = msg_send![class!(UNMutableNotificationContent), new];
            let _: () = msg_send![&*content,setTitle:&*NSString::from_str(title)];
            let _: () = msg_send![&*content,setBody:&*NSString::from_str(body)];
            let sound: Retained<AnyObject> = msg_send![class!(UNNotificationSound), defaultSound];
            let _: () = msg_send![&*content,setSound:&*sound];
            let identifier = NSString::from_str(&uuid::Uuid::new_v4().to_string());
            let request: Retained<AnyObject> = msg_send![class!(UNNotificationRequest),requestWithIdentifier:&*identifier,content:&*content,trigger:std::ptr::null::<AnyObject>()];
            let callback = block2::RcBlock::new(move |error: *mut AnyObject| {
                // Never surface NSError descriptions (may embed user paths).
                let code: Option<isize> = if error.is_null() {
                    None
                } else {
                    Some(msg_send![error, code])
                };
                let _ = tx.send(code);
            });
            let center: Retained<AnyObject> =
                msg_send![class!(UNUserNotificationCenter), currentNotificationCenter];
            let _: () = msg_send![&*center,addNotificationRequest:&*request,withCompletionHandler:&*callback];
        }
        match rx.recv_timeout(std::time::Duration::from_secs(10)) {
            Ok(None) => Ok(()),
            Ok(Some(code)) => Err(AppError::new(
                "NOTIFICATION",
                &format!("系统拒绝通知（{code}）"),
            )),
            Err(_) => Err(AppError::new("NOTIFICATION", "系统未确认接收通知")),
        }
    }
}
#[cfg(target_os = "macos")]
pub fn initialize() {
    mac::initialize();
}
#[cfg(not(target_os = "macos"))]
pub fn initialize() {}
#[cfg(target_os = "macos")]
pub fn send(_id: &str, title: &str, body: &str) -> Result<()> {
    mac::send(title, body)
}
#[cfg(windows)]
pub fn send(id: &str, title: &str, body: &str) -> Result<()> {
    use windows::{
        core::HSTRING,
        Data::Xml::Dom::XmlDocument,
        UI::Notifications::{ToastNotification, ToastNotificationManager},
    };
    let escape = |value: &str| {
        value
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
            .replace('\'', "&apos;")
    };
    let xml=format!("<toast><visual><binding template=\"ToastGeneric\"><text>{}</text><text>{}</text></binding></visual></toast>",escape(title),escape(body));
    let run = || -> windows::core::Result<()> {
        let doc = XmlDocument::new()?;
        doc.LoadXml(&HSTRING::from(xml))?;
        let toast = ToastNotification::CreateToastNotification(&doc)?;
        ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(id))?.Show(&toast)
    };
    run().map_err(|e| {
        AppError::new(
            "NOTIFICATION",
            &format!("系统拒绝通知（{:08X}）", e.code().0 as u32),
        )
    })
}
#[cfg(not(any(target_os = "macos", windows)))]
pub fn send(_id: &str, _title: &str, _body: &str) -> Result<()> {
    Err(AppError::new("NOTIFICATION", "此平台未提供原生通知通道"))
}
