//! A terminal invokes this binary's small bootstrap mode; secrets never enter shell text.
use crate::{
    errors::code,
    gateway::{self, ClientId},
    storage::{self, AppError, Result},
    Runtime,
};
use serde::Serialize;
use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
};
use tauri_plugin_dialog::DialogExt;

fn error(message: &str) -> AppError {
    AppError::new(code::CONFIG, message)
}
fn executable() -> Result<PathBuf> {
    let name = if cfg!(windows) { "agy.exe" } else { "agy" };
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|v| std::env::split_paths(&v).collect())
        .unwrap_or_default();
    if let Some(home) = dirs::home_dir() {
        dirs.push(home.join(".local/bin"));
    }
    #[cfg(unix)]
    dirs.extend([
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
    ]);
    dirs.into_iter()
        .map(|d| d.join(name))
        .find(|p| p.is_file())
        .ok_or_else(|| error("未找到 agy，请安装 Antigravity CLI 后重试"))
}
pub fn share_config(home: &Path) -> Result<()> {
    let source = dirs::home_dir()
        .ok_or_else(|| error("无法定位原生配置目录"))?
        .join(".gemini/config");
    share_directory(&source, &home.join("config"))
}
fn share_directory(source: &Path, target: &Path) -> Result<()> {
    if !source.is_dir() {
        return Ok(());
    }
    if std::fs::symlink_metadata(target).is_ok() {
        if std::fs::canonicalize(target).ok() == std::fs::canonicalize(source).ok() {
            return Ok(());
        }
        return Err(error("Antigravity 共享配置目录冲突，请检查运行目录"));
    }
    #[cfg(unix)]
    let result = std::os::unix::fs::symlink(source, target);
    #[cfg(windows)]
    let result = std::os::windows::fs::symlink_dir(source, target).or_else(|e| {
        // Environment expansion preserves spaces, Unicode, percent and shell metacharacters.
        use std::os::windows::process::CommandExt;
        let status = Command::new("cmd.exe")
            .raw_arg(r#"/d /v:off /c mklink /J "%LICH13_AGY_TARGET%" "%LICH13_AGY_SOURCE%""#)
            .env("LICH13_AGY_TARGET", target)
            .env("LICH13_AGY_SOURCE", source)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()?;
        if status.success() {
            Ok(())
        } else {
            Err(e)
        }
    });
    result.map_err(|_| error("无法关联 Antigravity 共享配置，请检查目录权限"))
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Info {
    installed: bool,
    model: Option<String>,
}
#[tauri::command]
pub fn get_agy_launch(r: tauri::State<'_, Arc<Runtime>>) -> Result<Info> {
    let raw = storage::read_optional(&ClientId::Agy.config(&r.home(ClientId::Agy)?))?;
    let model = raw
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .and_then(|v| v["model"].as_str().map(str::to_owned));
    Ok(Info {
        installed: executable().is_ok(),
        model: model.or(r.agy.agy_model()?),
    })
}
#[tauri::command]
pub async fn choose_agy_directory(app: tauri::AppHandle) -> Result<Option<String>> {
    tauri::async_runtime::spawn_blocking(move || {
        app.dialog()
            .file()
            .blocking_pick_folder()
            .map(|p| {
                p.into_path()
                    .map(|p| p.to_string_lossy().into_owned())
                    .map_err(|_| error("工作目录无效"))
            })
            .transpose()
    })
    .await
    .map_err(|_| error("无法选择工作目录"))?
}
#[tauri::command]
pub async fn launch_agy(
    r: tauri::State<'_, Arc<Runtime>>,
    model: String,
    directory: String,
    expected_revision: String,
) -> Result<()> {
    let runtime = r.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        executable()?;
        let cwd = PathBuf::from(directory);
        if !cwd.is_absolute() || !cwd.is_dir() {
            return Err(error("请选择有效的工作目录"));
        }
        let home = runtime.home(ClientId::Agy)?;
        runtime.agy.prepare_agy(&home, &model, &expected_revision)?;
        share_config(&home)?;
        let exe = std::env::current_exe().map_err(|_| error("无法定位应用启动入口"))?;
        open_terminal(&exe, &runtime.data.join("agy"), &cwd, &model)
    })
    .await
    .map_err(|_| error("Antigravity 启动失败"))?
}
#[cfg(any(target_os = "macos", test))]
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}
fn launch_args(data: &Path, cwd: &Path, model: &str) -> Vec<String> {
    vec![
        "--agy-launch".into(),
        data.to_string_lossy().into_owned(),
        cwd.to_string_lossy().into_owned(),
        model.into(),
    ]
}
fn open_terminal(exe: &Path, data: &Path, cwd: &Path, model: &str) -> Result<()> {
    let args = launch_args(data, cwd, model);
    #[cfg(target_os = "macos")]
    {
        let command = std::iter::once(exe.to_string_lossy().into_owned())
            .chain(args)
            .map(|s| shell_quote(&s))
            .collect::<Vec<_>>()
            .join(" ");
        // Pass shell text as argv, never interpolate it into AppleScript source.
        let status = Command::new("/usr/bin/osascript").args(["-e", "on run argv\ntell application \"Terminal\"\nactivate\ndo script (item 1 of argv)\nend tell\nend run", "--", &command]).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status().map_err(|_| error("无法打开终端"))?;
        if status.success() {
            Ok(())
        } else {
            Err(error("终端启动失败，请检查系统自动化权限"))
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        Command::new(exe)
            .args(args)
            .creation_flags(0x00000010)
            .spawn()
            .map(|_| ())
            .map_err(|_| error("无法打开终端"))
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        for (terminal, flag) in [
            ("x-terminal-emulator", "-e"),
            ("gnome-terminal", "--"),
            ("xterm", "-e"),
        ] {
            if Command::new(terminal)
                .arg(flag)
                .arg(exe)
                .args(&args)
                .spawn()
                .is_ok()
            {
                return Ok(());
            }
        }
        Err(error("未找到可用终端"))
    }
}
fn child(data: &Path, cwd: &Path, model: &str, binary: &Path) -> Result<Command> {
    if !data.is_absolute()
        || !cwd.is_absolute()
        || !cwd.is_dir()
        || !gateway::gemini::valid_model(model)
    {
        return Err(error("Antigravity 启动参数无效"));
    }
    let store = gateway::agy::load_launch_store(data)?;
    let home = data.join("runtime");
    if !store.0 {
        return Err(error("Antigravity 网关未启用"));
    }
    let mut cmd = Command::new(binary);
    cmd.current_dir(cwd)
        .arg(format!("--gemini_dir={}", home.to_string_lossy()))
        .arg("--model")
        .arg(format!("lich13-switch/{model}"))
        .env("GEMINI_API_KEY", store.1)
        .env("GOOGLE_GEMINI_BASE_URL", store.2);
    Ok(cmd)
}
/// Called before Tauri/single-instance initialization. The UI process stays running.
pub fn early() -> bool {
    let args: Vec<_> = std::env::args().collect();
    if args.get(1).map(String::as_str) != Some("--agy-launch") {
        return false;
    }
    #[cfg(windows)]
    unsafe {
        if windows_sys::Win32::System::Console::AttachConsole(u32::MAX) == 0 {
            windows_sys::Win32::System::Console::AllocConsole();
        }
    }
    let result = (|| -> Result<()> {
        if args.len() != 5 {
            return Err(error("Antigravity 启动参数无效"));
        }
        let binary = executable()?;
        let mut cmd = child(Path::new(&args[2]), Path::new(&args[3]), &args[4], &binary)?;
        let status = cmd
            .status()
            .map_err(|_| error("无法启动 Antigravity CLI"))?;
        if status.success() {
            Ok(())
        } else {
            Err(error("Antigravity CLI 已退出，请查看终端错误"))
        }
    })();
    if let Err(e) = result {
        eprintln!("{}", e.message);
        std::process::exit(1);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn terminal_arguments_quote_paths_and_never_include_credentials() {
        assert_eq!(shell_quote("a'b $()"), "'a'\\''b $()'");
        let args = launch_args(
            Path::new("/fixture/data space"),
            Path::new("/fixture/project's"),
            "gemini-fixture",
        );
        assert_eq!(args.len(), 4);
        assert!(!args.join(" ").contains("GEMINI_API_KEY"));
    }
    #[test]
    fn child_receives_only_local_credentials_in_environment_with_literal_paths() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data space & 中文");
        let cwd = dir.path().join("project's $() %fixture%");
        std::fs::create_dir(&cwd).unwrap();
        let gateway = gateway::Gateway::new(data.clone()).unwrap();
        gateway
            .edit(
                gateway::Edit::SaveProvider {
                    id: None,
                    name: Some("Fixture".into()),
                    base_url: "https://example.invalid".into(),
                    token: "fixture-upstream".into(),
                    quota_access_token: None,
                    search_only: None,
                },
                &gateway.try_view().unwrap().revision,
                &dir.path().join("unused"),
            )
            .unwrap();
        let file = data.join("gateway.json");
        let mut value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
        value["localToken"] = serde_json::json!("fixture-local");
        value["resume"] =
            serde_json::json!({"home":data.join("runtime"),"desired":true,"pairHash":""});
        std::fs::write(&file, serde_json::to_vec(&value).unwrap()).unwrap();
        let cmd = child(&data, &cwd, "gemini-fixture", Path::new("agy")).unwrap();
        assert_eq!(cmd.get_current_dir(), Some(cwd.as_path()));
        let args: Vec<_> = cmd
            .get_args()
            .map(|v| v.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            [
                format!("--gemini_dir={}", data.join("runtime").display()),
                "--model".into(),
                "lich13-switch/gemini-fixture".into()
            ]
        );
        assert!(!args.join(" ").contains("fixture-local"));
        let vars: std::collections::BTreeMap<_, _> = cmd
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.unwrap().to_string_lossy().into_owned(),
                )
            })
            .collect();
        assert_eq!(vars["GEMINI_API_KEY"], "fixture-local");
        assert!(vars["GOOGLE_GEMINI_BASE_URL"].starts_with("http://127.0.0.1:"));
        assert!(!format!("{vars:?}").contains("fixture-upstream"));
        value["resume"]["desired"] = serde_json::json!(false);
        std::fs::write(&file, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(child(&data, &cwd, "gemini-fixture", Path::new("agy")).is_err());
    }
    #[cfg(unix)]
    #[test]
    fn shared_configuration_preserves_conflicting_directory() {
        let t = tempfile::tempdir().unwrap();
        let source = t.path().join("native");
        let target = t.path().join("managed");
        std::fs::create_dir(&source).unwrap();
        share_directory(&source, &target).unwrap();
        share_directory(&source, &target).unwrap();
        let conflict = t.path().join("other");
        std::fs::create_dir(&conflict).unwrap();
        assert!(share_directory(&source, &conflict).is_err());
        assert!(conflict.is_dir());
    }
}
