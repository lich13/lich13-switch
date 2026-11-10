use super::*;
use std::path::{Path, PathBuf};
use winreg::{
    enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE},
    RegKey,
};

const PROGRAM: &str = "gptSwitch.CCSwitch.0";
const CAPABILITIES: &str = "Software\\gpt-Switch\\LinkHandlers\\0\\Capabilities";
const REGISTERED: &str = "Vela Link Vela";
const USER_CHOICE: &str =
    "Software\\Microsoft\\Windows\\Shell\\Associations\\UrlAssociations\\ccswitch\\UserChoice";
fn executable(command: &str) -> Option<PathBuf> {
    let command = command.trim();
    let path = if let Some(quoted) = command.strip_prefix('"') {
        quoted.split('"').next()?
    } else {
        let end = command.to_ascii_lowercase().find(".exe")? + 4;
        &command[..end]
    };
    Some(PathBuf::from(path))
}
fn same_path(a: &Path, b: &Path) -> bool {
    a.to_string_lossy()
        .eq_ignore_ascii_case(&b.to_string_lossy())
}
pub fn installed() -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    [HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE]
        .into_iter()
        .any(|hive| {
            let Ok(key) = RegKey::predef(hive)
                .open_subkey("Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\Vela")
            else {
                return false;
            };
            key.get_value::<String, _>("InstallLocation")
                .ok()
                .is_some_and(|path| same_path(&Path::new(&path).join("vela.exe"), &exe))
                || key
                    .get_value::<String, _>("DisplayIcon")
                    .ok()
                    .and_then(|s| executable(&s))
                    .is_some_and(|path| same_path(&path, &exe))
        })
}
fn command_is_self(program: &str, exe: &Path) -> bool {
    [HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE]
        .into_iter()
        .any(|hive| {
            RegKey::predef(hive)
                .open_subkey(format!(
                    "Software\\Classes\\{program}\\shell\\open\\command"
                ))
                .and_then(|key| key.get_value::<String, _>(""))
                .ok()
                .and_then(|s| executable(&s))
                .is_some_and(|path| same_path(&path, exe))
        })
}
pub fn ensure(tx: tokio::sync::oneshot::Sender<Result<()>>) {
    let result = (|| {
        let exe = std::env::current_exe().map_err(|_| invalid("无法读取已安装应用"))?;
        let root = RegKey::predef(HKEY_CURRENT_USER);
        let write = || -> std::io::Result<()> {
            for program in [PROGRAM, "ccswitch"] {
                let (key, _) = root.create_subkey(format!("Software\\Classes\\{program}"))?;
                key.set_value("", &"Vela CC Switch Link")?;
                key.set_value("URL Protocol", &"")?;
                key.create_subkey("shell\\open\\command")?
                    .0
                    .set_value("", &format!("\"{}\" \"%1\"", exe.display()))?;
            }
            let (caps, _) = root.create_subkey(CAPABILITIES)?;
            caps.set_value("ApplicationName", &"Vela")?;
            caps.set_value("ApplicationDescription", &"CC Switch links")?;
            caps.create_subkey("URLAssociations")?
                .0
                .set_value("ccswitch", &PROGRAM)?;
            root.create_subkey("Software\\RegisteredApplications")?
                .0
                .set_value(REGISTERED, &CAPABILITIES)?;
            Ok(())
        };
        write().map_err(|_| invalid("无法注册链接关联，请重启应用重试"))?;
        let choice = root
            .open_subkey(USER_CHOICE)
            .and_then(|key| key.get_value::<String, _>("ProgId"))
            .ok();
        if choice
            .as_deref()
            .is_none_or(|program| command_is_self(program, &exe))
        {
            return Ok(());
        }
        let query = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("registeredAppUser", REGISTERED)
            .finish();
        open::that(format!("ms-settings:defaultapps?{query}"))
            .map_err(|_| invalid("无法打开系统默认应用设置，请手动关联 CC Switch 链接"))?;
        Err(invalid(
            "系统需要确认：请在已打开的默认应用设置中，将 CC Switch 链接关联到 Vela",
        ))
    })();
    let _ = tx.send(result);
}
