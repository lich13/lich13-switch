//! Native login items, matching lich13studio's installed-bundle/hidden-launch behavior.
use crate::storage::{self, AppError, Result};
use auto_launch::{AutoLaunch, AutoLaunchBuilder};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    sync::Mutex,
};
pub const LOGIN_ARG: &str = "--lich13-switch-login-startup";
pub const LEGACY_LOGIN_ARG: &str = "--gpt-switch-login-startup";
pub fn is_login_argument(arg: &str) -> bool {
    [LOGIN_ARG, LEGACY_LOGIN_ARG].contains(&arg)
}
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct Preferences {
    // Login-item state lives in the OS. Advance its edit revision on every save.
    #[serde(skip_serializing_if = "Option::is_none")]
    generation: Option<String>,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct View {
    pub launch_on_boot: bool,
    pub revision: String,
}
pub struct Service {
    path: PathBuf,
    lock: Mutex<()>,
}
impl Service {
    pub fn new(data: &Path) -> Self {
        Self {
            path: data.join("startup.json"),
            lock: Mutex::new(()),
        }
    }
    fn read(&self) -> Result<(Preferences, String)> {
        let raw = storage::read_optional(&self.path)?;
        let settings = raw
            .as_deref()
            .map(serde_json::from_slice)
            .transpose()
            .map_err(|_| AppError::new("STARTUP", "启动设置无法读取"))?
            .unwrap_or_default();
        Ok((settings, storage::revision(raw.as_deref())))
    }
    pub fn view(&self) -> Result<View> {
        let _guard = self.lock.lock().unwrap();
        let (_, revision) = self.read()?;
        Ok(View {
            launch_on_boot: registered()?,
            revision,
        })
    }
    pub fn migrate(&self) -> Result<()> {
        let _guard = self.lock.lock().unwrap();
        #[cfg(target_os = "macos")]
        {
            crate::startup_macos::migrate(&executable()?)?;
            let old = auto_launch(true)?;
            let legacy = old.is_enabled().map_err(system_error)?;
            if legacy {
                let was = registered()?;
                register(true)?;
                if let Err(e) = old.disable() {
                    let _ = register(was);
                    return Err(system_error(e));
                }
                if old.is_enabled().map_err(system_error)? {
                    return Err(AppError::new("STARTUP", "旧登录项未能移除"));
                }
            }
        }
        #[cfg(windows)]
        if auto_launch(true)?.is_enabled().map_err(system_error)? {
            register(true)?;
        }
        let (prefs, rev) = self.read()?;
        if rev != "missing" {
            storage::atomic_write(
                &self.path,
                &serde_json::to_vec_pretty(&prefs).unwrap(),
                Some(&rev),
            )?;
        }
        Ok(())
    }
    pub fn update(&self, enabled: bool, expected: &str) -> Result<View> {
        let _guard = self.lock.lock().unwrap();
        let (_, revision) = self.read()?;
        if revision != expected {
            return Err(AppError::new("CONFLICT", "启动设置已变化，请重新打开设置"));
        }
        let previous = registered()?;
        register(enabled)?;
        let bytes = serde_json::to_vec_pretty(&Preferences {
            generation: Some(uuid::Uuid::new_v4().to_string()),
        })
        .unwrap();
        if let Err(e) = storage::atomic_write(&self.path, &bytes, Some(expected)) {
            let _ = register(previous);
            return Err(e);
        }
        Ok(View {
            launch_on_boot: enabled,
            revision: storage::digest(&bytes),
        })
    }
}
#[cfg(target_os = "macos")]
fn executable() -> Result<PathBuf> {
    std::env::current_exe().map_err(storage::io_error)
}
fn registered() -> Result<bool> {
    #[cfg(target_os = "macos")]
    {
        crate::startup_macos::actual(&executable()?)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let launch = auto_launch(true)?;
        let enabled = launch.is_enabled().map_err(system_error)?;
        #[cfg(windows)]
        if enabled {
            use winreg::{enums::HKEY_CURRENT_USER, RegKey};
            let actual: String = RegKey::predef(HKEY_CURRENT_USER)
                .open_subkey("SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Run")
                .and_then(|key| key.get_value("gpt-Switch"))
                .map_err(storage::io_error)?;
            if actual != format!("{} {LOGIN_ARG}", launch.get_app_path()) {
                return Err(AppError::new(
                    "STARTUP",
                    "启动路径已变化，请重新启用开机启动",
                ));
            }
        }
        Ok(enabled)
    }
}
fn register(enabled: bool) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        let exe = executable()?;
        if bundle_path(&exe).extension().is_none_or(|e| e != "app") {
            return Err(AppError::new("STARTUP", "请使用正式安装的应用设置开机启动"));
        }
        crate::startup_macos::set(enabled, &exe)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let launch = auto_launch(true)?;
        if enabled {
            launch.enable()
        } else {
            launch.disable()
        }
        .map_err(system_error)?;
        if registered()? != enabled {
            return Err(AppError::new("STARTUP", "启动注册状态不匹配"));
        }
        Ok(())
    }
}

fn system_error(_: auto_launch::Error) -> AppError {
    AppError::new(
        "STARTUP",
        "系统登录项操作失败，请检查系统的登录项与自动化权限",
    )
}
fn bundle_path(executable: &Path) -> PathBuf {
    executable
        .ancestors()
        .find(|p| p.extension().is_some_and(|e| e == "app"))
        .unwrap_or(executable)
        .to_owned()
}
fn auto_launch(silent: bool) -> Result<AutoLaunch> {
    let executable = std::env::current_exe().map_err(storage::io_error)?;
    let path = if cfg!(target_os = "macos") {
        bundle_path(&executable)
    } else {
        executable
    };
    if cfg!(target_os = "macos") && path.extension().is_none_or(|e| e != "app") {
        return Err(AppError::new(
            "STARTUP",
            "请在安装后的 lich13-switch.app 中设置开机启动",
        ));
    }
    let path = path
        .to_str()
        .ok_or_else(|| AppError::new("STARTUP", "安装路径无法读取"))?;
    // auto-launch's macOS AppleScript backend quotes these strings directly.
    if path.contains(['"', '\n', '\r']) || (cfg!(target_os = "macos") && path.contains('\\')) {
        return Err(AppError::new(
            "STARTUP",
            "安装路径含不支持的字符，请移动到标准应用目录",
        ));
    }
    let args = if cfg!(target_os = "macos") {
        if silent {
            vec!["--hidden"]
        } else {
            vec![]
        }
    } else {
        vec![LOGIN_ARG]
    };
    // auto-launch writes its Windows path verbatim into the Run command.
    let path = registration_path(path, cfg!(target_os = "windows"));
    AutoLaunchBuilder::new()
        .set_app_name("gpt-Switch")
        .set_app_path(&path)
        .set_use_launch_agent(false)
        .set_args(&args)
        .build()
        .map_err(system_error)
}
fn registration_path(path: &str, windows: bool) -> String {
    if windows {
        format!("\"{path}\"")
    } else {
        path.to_owned()
    }
}
#[cfg(windows)]
pub fn smoke_registration() -> Result<()> {
    let entry = auto_launch(true)?;
    if entry.is_enabled().map_err(system_error)? {
        return Ok(());
    }
    entry.enable().map_err(system_error)?;
    let checked = (|| -> Result<()> {
        use winreg::{enums::HKEY_CURRENT_USER, RegKey};
        let value: String = RegKey::predef(HKEY_CURRENT_USER)
            .open_subkey("SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Run")
            .and_then(|key| key.get_value("gpt-Switch"))
            .map_err(storage::io_error)?;
        let expected = format!("{} {LOGIN_ARG}", entry.get_app_path());
        if value != expected || !entry.is_enabled().map_err(system_error)? {
            return Err(AppError::new("SMOKE", "Windows 启动项校验失败"));
        }
        Ok(())
    })();
    entry.disable().map_err(system_error)?;
    checked?;
    if entry.is_enabled().map_err(system_error)? {
        return Err(AppError::new("SMOKE", "Windows 启动项未解除"));
    }
    Ok(())
}
pub fn login_source(args: &[String]) -> bool {
    if args.iter().any(|a| is_login_argument(a)) {
        return true;
    }
    #[cfg(target_os = "macos")]
    {
        use objc2::MainThreadMarker;
        use objc2_app_kit::NSApplication;
        macos_login_event()
            || MainThreadMarker::new()
                .is_some_and(|m| NSApplication::sharedApplication(m).isHidden())
    }
    #[cfg(not(target_os = "macos"))]
    false
}
#[cfg(target_os = "macos")]
pub fn macos_login_event() -> bool {
    use objc2_foundation::NSAppleEventManager;
    // Read during Tauri setup / applicationDidFinishLaunching, while the
    // launch Apple event is still current. macOS 13+ can ignore the legacy
    // login item's `hidden` flag; the official login-origin marker remains.
    // https://developer.apple.com/documentation/coreservices/keyaelaunchedasloginitem
    NSAppleEventManager::sharedAppleEventManager()
        .currentAppleEvent()
        .is_some_and(|event| {
            is_login_event(
                event.eventID(),
                event
                    .paramDescriptorForKeyword(u32::from_be_bytes(*b"prdt"))
                    .map(|value| value.enumCodeValue()),
            )
        })
}
#[cfg(any(target_os = "macos", test))]
fn is_login_event(event: u32, origin: Option<u32>) -> bool {
    [u32::from_be_bytes(*b"oapp"), u32::from_be_bytes(*b"rapp")].contains(&event)
        && origin == Some(u32::from_be_bytes(*b"lgit"))
}
pub fn silent(args: &[String]) -> bool {
    login_source(args)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn installed_path_and_legacy_preferences() {
        assert_eq!(
            bundle_path(Path::new(
                "/Applications/lich13-switch.app/Contents/MacOS/lich13-switch"
            )),
            PathBuf::from("/Applications/lich13-switch.app")
        );
        let _: Preferences = serde_json::from_str("{}").unwrap();
        let _: Preferences =
            serde_json::from_str(r#"{"launchToTray":false,"restoreGateway":true}"#).unwrap();
        assert!(silent(&[LOGIN_ARG.into()]));
        assert!(silent(&[LEGACY_LOGIN_ARG.into()]));
        assert!(is_login_event(
            u32::from_be_bytes(*b"oapp"),
            Some(u32::from_be_bytes(*b"lgit"))
        ));
        assert!(is_login_event(
            u32::from_be_bytes(*b"rapp"),
            Some(u32::from_be_bytes(*b"lgit"))
        ));
        assert!(!is_login_event(u32::from_be_bytes(*b"oapp"), None));
        assert_eq!(
            registration_path(r"C:\Program Files\lich13-switch\lich13-switch.exe", true),
            r#""C:\Program Files\lich13-switch\lich13-switch.exe""#
        );
        assert!(silent(&[LOGIN_ARG.into()]));
    }
}

#[cfg(test)]
#[path = "startup_v0233_tests.rs"]
mod v0233_tests;
