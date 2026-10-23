use super::*;
use objc2_app_kit::{NSRunningApplication, NSWorkspace, NSWorkspaceOpenConfiguration};
use objc2_foundation::{NSArray, NSBundle, NSError, NSString, NSURL};
use std::sync::Mutex;

pub fn state() -> Result<HandlerState> {
    let ws = NSWorkspace::sharedWorkspace();
    let url = NSURL::URLWithString(&NSString::from_str("ccswitch://v1/import")).unwrap();
    let current = ws
        .URLForApplicationToOpenURL(&url)
        .and_then(|u| NSBundle::bundleWithURL(&u))
        .and_then(|b| b.bundleIdentifier())
        .map(|id| id.to_string());
    let handlers = ws.URLsForApplicationsToOpenURL(&url);
    let mut apps = Vec::new();
    for (id, name) in APPS {
        let mut paths: Vec<String> = handlers
            .iter()
            .filter(|u| {
                NSBundle::bundleWithURL(u)
                    .and_then(|b| b.bundleIdentifier())
                    .is_some_and(|s| s.to_string() == *id)
            })
            .filter_map(|u| u.path().map(|p| p.to_string()))
            .collect();
        paths.sort_by_key(|p| (!p.starts_with("/Applications/"), p.clone()));
        if let Some(path) = paths.first() {
            apps.push(Handler {
                id: (*id).into(),
                name: (*name).into(),
                path: path.clone(),
            });
        }
    }
    Ok(HandlerState {
        current,
        apps,
        remembered: None,
        system_picker: false,
    })
}
pub fn set(id: &str, tx: tokio::sync::oneshot::Sender<Result<()>>) {
    let Some(app) = state()
        .ok()
        .and_then(|s| s.apps.into_iter().find(|a| a.id == id))
    else {
        let _ = tx.send(Err(invalid("应用未安装或不支持 CC Switch 链接")));
        return;
    };
    let url = NSURL::fileURLWithPath(&NSString::from_str(&app.path));
    let tx = Mutex::new(Some(tx));
    let block = block2::RcBlock::new(move |error: *mut NSError| {
        if let Some(tx) = tx.lock().unwrap().take() {
            let _ = tx.send(if error.is_null() {
                Ok(())
            } else {
                Err(invalid("默认应用未更改，请重试"))
            });
        }
    });
    NSWorkspace::sharedWorkspace()
        .setDefaultApplicationAtURL_toOpenURLsWithScheme_completionHandler(
            &url,
            &NSString::from_str("ccswitch"),
            Some(&block),
        );
}

pub fn open(id: &str, raw: &str, tx: tokio::sync::oneshot::Sender<Result<()>>) {
    let Some(app) = state()
        .ok()
        .and_then(|s| s.apps.into_iter().find(|a| a.id == id))
    else {
        let _ = tx.send(Err(invalid("接收应用不可用，请重新选择")));
        return;
    };
    let Some(url) = NSURL::URLWithString(&NSString::from_str(raw)) else {
        let _ = tx.send(Err(invalid("无效的导入链接")));
        return;
    };
    let application = NSURL::fileURLWithPath(&NSString::from_str(&app.path));
    let configuration = NSWorkspaceOpenConfiguration::configuration();
    configuration.setAddsToRecentItems(false);
    let tx = Mutex::new(Some(tx));
    let block = block2::RcBlock::new(move |_: *mut NSRunningApplication, error: *mut NSError| {
        if let Some(tx) = tx.lock().unwrap().take() {
            let _ = tx.send(if error.is_null() {
                Ok(())
            } else {
                Err(invalid("无法打开接收应用，请重新选择"))
            });
        }
    });
    NSWorkspace::sharedWorkspace().openURLs_withApplicationAtURL_configuration_completionHandler(
        &NSArray::from_retained_slice(&[url]),
        &application,
        &configuration,
        Some(&block),
    );
}
