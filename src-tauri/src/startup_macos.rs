//! A per-user LaunchAgent. Disabling never unloads/kills the running application.
use crate::errors::code;
use crate::storage::{self, AppError, Result};
use std::{
    path::{Path, PathBuf},
    process::Command,
};
const LABEL: &str = "com.lich13.gpt-switch";
fn failure() -> AppError {
    AppError::new(code::STARTUP, "LaunchAgent 操作失败，请检查系统登录项设置")
}
fn location() -> Result<PathBuf> {
    Ok(dirs::home_dir()
        .ok_or_else(failure)?
        .join("Library/LaunchAgents/com.lich13.gpt-switch.plist"))
}
fn domain() -> Result<String> {
    let output = Command::new("/usr/bin/id")
        .arg("-u")
        .output()
        .map_err(storage::io_error)?;
    let uid = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<u32>()
        .map_err(|_| failure())?;
    Ok(format!("gui/{uid}"))
}
fn disabled(output: &str) -> bool {
    output
        .lines()
        .any(|line| line.trim().starts_with(&format!("\"{LABEL}\"")) && line.contains("=> true"))
}
fn system_enabled() -> Result<bool> {
    let output = Command::new("/bin/launchctl")
        .args(["print-disabled", &domain()?])
        .output()
        .map_err(storage::io_error)?;
    if !output.status.success() {
        return Err(failure());
    }
    Ok(!disabled(&String::from_utf8_lossy(&output.stdout)))
}
fn enabled(value: bool) -> Result<()> {
    let target = format!("{}/{LABEL}", domain()?);
    if Command::new("/bin/launchctl")
        .args([if value { "enable" } else { "disable" }, &target])
        .status()
        .map_err(storage::io_error)?
        .success()
    {
        Ok(())
    } else {
        Err(failure())
    }
}
fn document(executable: &Path) -> Result<Vec<u8>> {
    use plist::{Dictionary, Value};
    let exe = executable.to_str().ok_or_else(failure)?;
    let mut d = Dictionary::new();
    d.insert("Label".into(), Value::String(LABEL.into()));
    d.insert(
        "ProgramArguments".into(),
        Value::Array(vec![
            Value::String(exe.into()),
            Value::String(super::startup::LOGIN_ARG.into()),
        ]),
    );
    d.insert("RunAtLoad".into(), Value::Boolean(true));
    d.insert(
        "LimitLoadToSessionType".into(),
        Value::String("Aqua".into()),
    );
    d.insert("ProcessType".into(), Value::String("Interactive".into()));
    let mut bytes = vec![];
    Value::Dictionary(d)
        .to_writer_xml(&mut bytes)
        .map_err(|_| failure())?;
    Ok(bytes)
}
pub fn actual(executable: &Path) -> Result<bool> {
    let Some(bytes) = storage::read_optional(&location()?)? else {
        return Ok(false);
    };
    let expected =
        plist::Value::from_reader_xml(document(executable)?.as_slice()).map_err(|_| failure())?;
    let actual = plist::Value::from_reader_xml(bytes.as_slice()).map_err(|_| failure())?;
    if actual != expected {
        return Err(AppError::new(
            code::STARTUP,
            "本应用 LaunchAgent 内容不匹配，请重新启用开机启动",
        ));
    }
    system_enabled()
}
fn legacy_document(bytes: &[u8], executable: &Path) -> bool {
    let Ok(value) = plist::Value::from_reader_xml(bytes) else {
        return false;
    };
    let Some(args) = value
        .as_dictionary()
        .and_then(|d| d.get("ProgramArguments"))
        .and_then(plist::Value::as_array)
    else {
        return false;
    };
    let Some(old_path) = args.first().and_then(plist::Value::as_string) else {
        return false;
    };
    let expected_old = executable.to_string_lossy().replace(
        "lich13-switch.app/Contents/MacOS/lich13-switch",
        "gpt-Switch.app/Contents/MacOS/gpt-switch",
    );
    if old_path != expected_old
        || args.len() != 2
        || args[1].as_string() != Some(super::startup::LEGACY_LOGIN_ARG)
    {
        return false;
    }
    let Ok(bytes) = document(Path::new(old_path)) else {
        return false;
    };
    let Ok(mut expected) = plist::Value::from_reader_xml(bytes.as_slice()) else {
        return false;
    };
    expected
        .as_dictionary_mut()
        .unwrap()
        .get_mut("ProgramArguments")
        .unwrap()
        .as_array_mut()
        .unwrap()[1] = plist::Value::String(super::startup::LEGACY_LOGIN_ARG.into());
    expected == value
}
pub fn migrate(executable: &Path) -> Result<()> {
    let path = location()?;
    let Some(old) = storage::read_optional(&path)? else {
        return Ok(());
    };
    if !legacy_document(&old, executable) {
        return Ok(());
    }
    let target = format!("{}/{LABEL}", domain()?);
    let was_enabled = system_enabled()?;
    let job = Command::new("/bin/launchctl")
        .args(["print", &target])
        .output()
        .map_err(storage::io_error)?;
    let own_job = String::from_utf8_lossy(&job.stdout)
        .lines()
        .any(|line| line.trim() == format!("pid = {}", std::process::id()));
    let result = (|| {
        storage::atomic_write(&path, &document(executable)?, Some(&storage::digest(&old)))?;
        // Never unload the process hosting this migration. Its next login reads
        // the updated plist. A manual launch can safely refresh an inactive job.
        if !own_job {
            if job.status.success()
                && !Command::new("/bin/launchctl")
                    .args(["bootout", &target])
                    .output()
                    .map_err(storage::io_error)?
                    .status
                    .success()
            {
                return Err(failure());
            }
            if was_enabled
                && !Command::new("/bin/launchctl")
                    .arg("bootstrap")
                    .arg(domain()?)
                    .arg(&path)
                    .output()
                    .map_err(storage::io_error)?
                    .status
                    .success()
            {
                return Err(failure());
            }
        }
        if actual(executable)? != was_enabled {
            return Err(failure());
        }
        Ok(())
    })();
    if result.is_err() {
        storage::atomic_write(&path, &old, None)?;
        if !own_job && job.status.success() {
            let _ = Command::new("/bin/launchctl")
                .args(["bootout", &target])
                .output();
            let _ = Command::new("/bin/launchctl")
                .arg("bootstrap")
                .arg(domain()?)
                .arg(&path)
                .output();
        }
        return Err(failure());
    }
    Ok(())
}
pub fn set(value: bool, executable: &Path) -> Result<()> {
    let path = location()?;
    let old = storage::read_optional(&path)?;
    let was_enabled = system_enabled()?;
    let result = (|| {
        if value {
            // LaunchAgents is a shared system directory: do not change its permissions.
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(storage::io_error)?;
            }
            storage::atomic_write(
                &path,
                &document(executable)?,
                Some(&storage::revision(old.as_deref())),
            )?;
            enabled(true)?;
            let target = format!("{}/{LABEL}", domain()?);
            let registered = Command::new("/bin/launchctl")
                .args(["print", &target])
                .output()
                .map_err(storage::io_error)?
                .status
                .success();
            if !registered {
                let output = Command::new("/bin/launchctl")
                    .arg("bootstrap")
                    .arg(domain()?)
                    .arg(&path)
                    .output()
                    .map_err(storage::io_error)?;
                if !output.status.success() {
                    return Err(failure());
                }
            }
        } else {
            enabled(false)?;
            if old.is_some() {
                std::fs::remove_file(&path).map_err(storage::io_error)?;
            }
        }
        if actual(executable)? != value {
            return Err(failure());
        }
        Ok(())
    })();
    if result.is_err() {
        match old {
            Some(bytes) => {
                let _ = storage::atomic_write(&path, &bytes, None);
            }
            None => {
                let _ = std::fs::remove_file(&path);
            }
        }
        let _ = enabled(was_enabled);
    }
    result
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn launch_arguments_are_separate_and_disable_parser_is_exact() {
        let bytes = document(Path::new(
            "/Applications/Some Folder/lich13-switch.app/Contents/MacOS/lich13-switch",
        ))
        .unwrap();
        let value = plist::Value::from_reader_xml(bytes.as_slice()).unwrap();
        let args = value.as_dictionary().unwrap()["ProgramArguments"]
            .as_array()
            .unwrap();
        assert_eq!(args.len(), 2);
        assert_eq!(args[1].as_string(), Some(crate::startup::LOGIN_ARG));
        assert!(!disabled("\"com.lich13.gpt-switch-other\" => true"));
        assert!(disabled("\"com.lich13.gpt-switch\" => true"));
        let mut legacy = plist::Value::from_reader_xml(
            document(Path::new(
                "/Applications/gpt-Switch.app/Contents/MacOS/gpt-switch",
            ))
            .unwrap()
            .as_slice(),
        )
        .unwrap();
        legacy
            .as_dictionary_mut()
            .unwrap()
            .get_mut("ProgramArguments")
            .unwrap()
            .as_array_mut()
            .unwrap()[1] = plist::Value::String(crate::startup::LEGACY_LOGIN_ARG.into());
        let mut bytes = Vec::new();
        legacy.to_writer_xml(&mut bytes).unwrap();
        assert!(legacy_document(
            &bytes,
            Path::new("/Applications/lich13-switch.app/Contents/MacOS/lich13-switch")
        ));
        assert!(!legacy_document(
            &bytes,
            Path::new("/tmp/lich13-switch.app/Contents/MacOS/lich13-switch")
        ));
    }
}
