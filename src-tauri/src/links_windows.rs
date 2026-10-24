use super::*;
use std::path::{Path, PathBuf};
use winreg::{
    enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, KEY_WRITE},
    RegKey,
};

const PROGRAM: &str = "gptSwitch.CCSwitch.0";
const CAPABILITIES: &str = "Software\\gpt-Switch\\LinkHandlers\\0\\Capabilities";
const REGISTERED: &str = "lich13-switch Link lich13-switch";
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
                .open_subkey("Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\gpt-Switch")
            else {
                return false;
            };
            key.get_value::<String, _>("InstallLocation")
                .ok()
                .is_some_and(|path| same_path(&Path::new(&path).join("lich13-switch.exe"), &exe))
                || key
                    .get_value::<String, _>("DisplayIcon")
                    .ok()
                    .and_then(|s| executable(&s))
                    .is_some_and(|path| same_path(&path, &exe))
        })
}
fn owned_external_capability(value: &str) -> bool {
    [1, 2].iter().any(|n| {
        value.eq_ignore_ascii_case(&format!(
            "Software\\gpt-Switch\\LinkHandlers\\{n}\\Capabilities"
        ))
    })
}
fn cleanup_owned_aliases(root: &RegKey) -> std::io::Result<()> {
    if let Ok(registered) =
        root.open_subkey_with_flags("Software\\RegisteredApplications", KEY_READ | KEY_WRITE)
    {
        let names: Vec<_> = registered
            .enum_values()
            .filter_map(|r| r.ok())
            .map(|(name, _)| name)
            .filter(|name| {
                registered
                    .get_value::<String, _>(name)
                    .is_ok_and(|value| owned_external_capability(&value))
            })
            .collect();
        for name in names {
            registered.delete_value(name)?;
        }
        // The old self registration shared the same capabilities path.
        if registered
            .get_value::<String, _>("gpt-Switch Link gpt-Switch")
            .is_ok_and(|value| value.eq_ignore_ascii_case(CAPABILITIES))
        {
            registered.delete_value("gpt-Switch Link gpt-Switch")?;
        }
    }
    for n in [1, 2] {
        for path in [
            format!("Software\\Classes\\gptSwitch.CCSwitch.{n}"),
            format!("Software\\gpt-Switch\\LinkHandlers\\{n}"),
        ] {
            if let Err(error) = root.delete_subkey_all(path) {
                if error.kind() != std::io::ErrorKind::NotFound {
                    return Err(error);
                }
            }
        }
    }
    Ok(())
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
            cleanup_owned_aliases(&root)?;
            for program in [PROGRAM, "ccswitch"] {
                let (key, _) = root.create_subkey(format!("Software\\Classes\\{program}"))?;
                key.set_value("", &"lich13-switch CC Switch Link")?;
                key.set_value("URL Protocol", &"")?;
                key.create_subkey("shell\\open\\command")?
                    .0
                    .set_value("", &format!("\"{}\" \"%1\"", exe.display()))?;
            }
            let (caps, _) = root.create_subkey(CAPABILITIES)?;
            caps.set_value("ApplicationName", &"lich13-switch")?;
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
            "系统需要确认：请在已打开的默认应用设置中，将 CC Switch 链接关联到 lich13-switch",
        ))
    })();
    let _ = tx.send(result);
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cleanup_is_limited_to_our_retired_capabilities() {
        assert!(owned_external_capability(
            "Software\\gpt-Switch\\LinkHandlers\\1\\Capabilities"
        ));
        assert!(!owned_external_capability(CAPABILITIES));
        assert!(!owned_external_capability(
            "Software\\AnotherApp\\Capabilities"
        ));
        assert!(!owned_external_capability(
            "Software\\gpt-Switch\\LinkHandlers\\10\\Capabilities"
        ));
    }
}
