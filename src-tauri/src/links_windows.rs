use super::*;
use std::path::{Path, PathBuf};
use winreg::{
    enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, KEY_WOW64_32KEY, KEY_WOW64_64KEY},
    RegKey,
};

fn executable(command: &str) -> Option<PathBuf> {
    let text = command.trim();
    let path = if let Some(text) = text.strip_prefix('"') {
        text.split('"').next()?
    } else {
        text.split(",0").next()?.trim()
    };
    let path = PathBuf::from(path);
    (path.is_file()
        && path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("exe")))
    .then_some(path)
}
fn app_id(path: &Path) -> Option<&'static str> {
    match path.file_name()?.to_str()?.to_ascii_lowercase().as_str() {
        "lich13-switch.exe" | "gpt-switch.exe" => Some(APPS[0].0),
        "cc-switch.exe" => Some(APPS[1].0),
        "lich13studio.exe" => Some(APPS[2].0),
        _ => None,
    }
}
fn command_for(prog: &str) -> Option<PathBuf> {
    for hive in [HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE] {
        if let Ok(key) = RegKey::predef(hive)
            .open_subkey(format!("Software\\Classes\\{prog}\\shell\\open\\command"))
        {
            if let Ok(value) = key.get_value::<String, _>("") {
                let quoted = value.trim();
                if let Some(path) = quoted
                    .strip_prefix('"')
                    .and_then(|s| s.split('"').next())
                    .and_then(executable)
                {
                    return Some(path);
                }
                if let Some(end) = value.to_lowercase().find(".exe") {
                    if let Some(p) = executable(&value[..end + 4]) {
                        return Some(p);
                    }
                }
            }
        }
    }
    None
}
fn installed() -> Vec<Handler> {
    let mut found = BTreeMap::new();
    if let Ok(exe) = std::env::current_exe() {
        if app_id(&exe) == Some(APPS[0].0) {
            found.insert(APPS[0].0, exe);
        }
    }
    if let Some(path) = command_for("ccswitch") {
        if let Some(id) = app_id(&path) {
            found.insert(id, path);
        }
    }
    for hive in [HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE] {
        for view in [KEY_WOW64_64KEY, KEY_WOW64_32KEY] {
            if let Ok(uninstall) = RegKey::predef(hive).open_subkey_with_flags(
                "Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall",
                KEY_READ | view,
            ) {
                for key in uninstall.enum_keys().flatten() {
                    let Ok(k) = uninstall.open_subkey(key) else {
                        continue;
                    };
                    let display = k.get_value::<String, _>("DisplayName").unwrap_or_default();
                    if !APPS.iter().any(|(_, name)| {
                        name.eq_ignore_ascii_case(&display)
                            || display.eq_ignore_ascii_case("gpt-Switch")
                    }) {
                        continue;
                    }
                    let mut candidates = Vec::new();
                    if let Ok(icon) = k.get_value::<String, _>("DisplayIcon") {
                        if let Some(p) = executable(&icon) {
                            candidates.push(p);
                        }
                    }
                    if let Ok(root) = k.get_value::<String, _>("InstallLocation") {
                        for exe in [
                            "lich13-switch.exe",
                            "gpt-switch.exe",
                            "cc-switch.exe",
                            "lich13studio.exe",
                        ] {
                            candidates.push(Path::new(&root).join(exe));
                        }
                    }
                    for path in candidates {
                        if path.is_file() {
                            if let Some(id) = app_id(&path) {
                                found.insert(id, path);
                            }
                        }
                    }
                }
            }
        }
    }
    APPS.iter()
        .filter_map(|(id, name)| {
            found.get(id).map(|p| Handler {
                id: (*id).into(),
                name: (*name).into(),
                path: p.to_string_lossy().into_owned(),
            })
        })
        .collect()
}
pub fn state() -> Result<HandlerState> {
    let choice=RegKey::predef(HKEY_CURRENT_USER).open_subkey("Software\\Microsoft\\Windows\\Shell\\Associations\\UrlAssociations\\ccswitch\\UserChoice").ok().and_then(|k|k.get_value::<String,_>("ProgId").ok());
    let current = command_for(choice.as_deref().unwrap_or("ccswitch"))
        .as_deref()
        .and_then(app_id)
        .map(str::to_owned);
    Ok(HandlerState {
        current,
        apps: installed(),
        remembered: None,
        system_picker: true,
    })
}
// Managed capability aliases let all installed receivers coexist without writing
// the shared protocol command or another application's registry keys.
fn advertise(app: &Handler) -> Result<String> {
    let root = RegKey::predef(HKEY_CURRENT_USER);
    let suffix = APPS
        .iter()
        .position(|(id, _)| *id == app.id)
        .ok_or_else(|| invalid("无效的接收应用"))?;
    let prog = format!("gptSwitch.CCSwitch.{suffix}");
    let capability = format!("Software\\gpt-Switch\\LinkHandlers\\{suffix}\\Capabilities");
    let registered = format!("lich13-switch Link {}", app.name);
    let write = || -> std::io::Result<()> {
        let (key, _) = root.create_subkey(format!("Software\\Classes\\{prog}"))?;
        key.set_value("", &format!("{} CC Switch Link", app.name))?;
        key.set_value("URL Protocol", &"")?;
        let (command, _) = key.create_subkey("shell\\open\\command")?;
        command.set_value("", &format!("\"{}\" \"%1\"", app.path))?;
        let (caps, _) = root.create_subkey(&capability)?;
        caps.set_value("ApplicationName", &app.name)?;
        caps.set_value("ApplicationDescription", &"CC Switch 链接")?;
        let (urls, _) = caps.create_subkey("URLAssociations")?;
        urls.set_value("ccswitch", &prog)?;
        root.create_subkey("Software\\RegisteredApplications")?
            .0
            .set_value(&registered, &capability)?;
        Ok(())
    };
    write().map_err(|_| invalid("无法注册链接接收应用"))?;
    Ok(registered)
}
pub fn set(id: &str, tx: tokio::sync::oneshot::Sender<Result<()>>) {
    let result = (|| {
        let app = installed()
            .into_iter()
            .find(|a| a.id == id)
            .ok_or_else(|| invalid("应用未安装或不支持 CC Switch 链接"))?;
        let registered = advertise(&app)?;
        let query = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("registeredAppUser", &registered)
            .finish();
        open::that(format!("ms-settings:defaultapps?{query}"))
            .map_err(|_| invalid("无法打开系统默认应用设置"))?;
        Ok(())
    })();
    let _ = tx.send(result);
}

pub fn open(id: &str, raw: &str, tx: tokio::sync::oneshot::Sender<Result<()>>) {
    use std::{
        os::windows::process::CommandExt,
        process::{Command, Stdio},
    };
    let result = (|| {
        let app = installed()
            .into_iter()
            .find(|a| a.id == id)
            .ok_or_else(|| invalid("接收应用不可用，请重新选择"))?;
        Command::new(app.path)
            .arg(raw)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(0x08000000)
            .spawn()
            .map_err(|_| invalid("无法打开接收应用，请重新选择"))?;
        Ok(())
    })();
    let _ = tx.send(result);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn identities_are_exact() {
        assert_eq!(
            app_id(Path::new("C:\\Apps\\cc-switch.exe")),
            Some("com.ccswitch.desktop")
        );
        assert!(app_id(Path::new("C:\\Apps\\my-cc-switch.exe")).is_none());
    }
}
