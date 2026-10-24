use super::*;
use objc2_app_kit::NSWorkspace;
use objc2_foundation::{NSBundle, NSError, NSString, NSURL};
use std::{path::Path, sync::Mutex};

pub fn installed() -> bool {
    let bundle = NSBundle::mainBundle();
    if bundle
        .bundleIdentifier()
        .is_none_or(|id| id.to_string() != APP_ID)
    {
        return false;
    }
    let Some(path) = bundle.bundleURL().path() else {
        return false;
    };
    let path = path.to_string();
    Path::new(&path).extension().is_some_and(|e| e == "app")
        && (path.starts_with("/Applications/")
            || dirs::home_dir()
                .is_some_and(|home| Path::new(&path).starts_with(home.join("Applications"))))
}
fn current_is_self() -> bool {
    let url = NSURL::URLWithString(&NSString::from_str("ccswitch://v1/import")).unwrap();
    NSWorkspace::sharedWorkspace()
        .URLForApplicationToOpenURL(&url)
        .is_some_and(|url| url.path() == NSBundle::mainBundle().bundleURL().path())
}
pub fn ensure(tx: tokio::sync::oneshot::Sender<Result<()>>) {
    if current_is_self() {
        let _ = tx.send(Ok(()));
        return;
    }
    let tx = Mutex::new(Some(tx));
    let block = block2::RcBlock::new(move |error: *mut NSError| {
        if let Some(tx) = tx.lock().unwrap().take() {
            let _ = tx.send(if error.is_null() && current_is_self() {
                Ok(())
            } else {
                Err(invalid(
                    "CC Switch 链接未关联到本应用，请在系统提示中确认后重启重试",
                ))
            });
        }
    });
    NSWorkspace::sharedWorkspace()
        .setDefaultApplicationAtURL_toOpenURLsWithScheme_completionHandler(
            &NSBundle::mainBundle().bundleURL(),
            &NSString::from_str("ccswitch"),
            Some(&block),
        );
}
