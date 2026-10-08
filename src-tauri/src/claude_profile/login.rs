use super::{catalog, fail};
use crate::storage::Result;
use serde::Serialize;
use std::{
    path::{Path, PathBuf},
    process::Stdio,
};
use tokio::{process::Command, sync::oneshot};
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct State {
    pub phase: String,
    pub authenticated: bool,
    pub error: Option<String>,
}
impl Default for State {
    fn default() -> Self {
        Self {
            phase: "idle".into(),
            authenticated: false,
            error: None,
        }
    }
}
#[derive(Default)]
pub struct Session {
    pub state: State,
    pub cancel: Option<oneshot::Sender<()>>,
}
fn executable() -> Result<PathBuf> {
    let mut paths: Vec<PathBuf> =
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).collect();
    if let Some(home) = dirs::home_dir() {
        paths.extend([
            home.join(".local/bin"),
            home.join(".npm-global/bin"),
            PathBuf::from("/opt/homebrew/bin"),
            PathBuf::from("/usr/local/bin"),
        ]);
        if let Ok(entries) = std::fs::read_dir(home.join(".nvm/versions/node")) {
            let mut versions: Vec<_> = entries.flatten().map(|e| e.path().join("bin")).collect();
            versions.sort();
            versions.reverse();
            paths.extend(versions);
        }
    }
    #[cfg(windows)]
    if let Some(appdata) = std::env::var_os("APPDATA") {
        paths.push(PathBuf::from(appdata).join("npm"));
    }
    let names = if cfg!(windows) {
        vec!["claude.exe", "claude.cmd"]
    } else {
        vec!["claude"]
    };
    paths
        .iter()
        .flat_map(|p| names.iter().map(move |n| p.join(n)))
        .find(|p| p.is_file())
        .ok_or_else(|| fail("未找到 Claude Code CLI"))
}
fn command(home: &Path) -> Result<Command> {
    let path = executable()?;
    #[cfg(windows)]
    let mut cmd = if path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("cmd"))
    {
        let script = path
            .parent()
            .ok_or_else(|| fail("CLI 路径无效"))?
            .join("node_modules/@anthropic-ai/claude-code/cli.js");
        if !script.is_file() {
            return Err(fail("不支持的 Claude 启动脚本"));
        }
        let mut c = Command::new("node.exe");
        c.arg(script);
        c
    } else {
        Command::new(path)
    };
    #[cfg(not(windows))]
    let mut cmd = Command::new(path);
    cmd.env("CLAUDE_CONFIG_DIR", home)
        .current_dir(home)
        .stdin(Stdio::null())
        .kill_on_drop(true);
    // Only this explicitly launched login process gets a clean connection env.
    // The caller's shell/system environment is never changed.
    for key in catalog::ENV {
        cmd.env_remove(key);
    }
    Ok(cmd)
}
pub async fn status(home: &Path) -> Result<bool> {
    // The official CLI emits JSON by default, including versions without an
    // explicit output-format flag in their documented command contract.
    let mut cmd = command(home)?;
    cmd.args(["auth", "status"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    use tokio::io::AsyncReadExt;
    let mut child = cmd.spawn().map_err(|_| fail("Claude 登录状态读取失败"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| fail("Claude 登录状态读取失败"))?;
    let mut limited = stdout.take(64 * 1024 + 1);
    let mut bytes = Vec::new();
    let exit = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        limited
            .read_to_end(&mut bytes)
            .await
            .map_err(|_| fail("Claude 登录状态读取失败"))?;
        if bytes.len() > 64 * 1024 {
            return Err(fail("Claude 登录状态响应过大"));
        }
        child
            .wait()
            .await
            .map_err(|_| fail("Claude 登录状态读取失败"))
    })
    .await
    .map_err(|_| fail("Claude 登录状态读取超时"))??;
    let authenticated = parse_status(&bytes)?;
    if authenticated && !exit.success() {
        return Err(fail("CLI 尚未确认官方账号登录"));
    }
    Ok(authenticated)
}
fn parse_status(bytes: &[u8]) -> Result<bool> {
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|_| fail("Claude 登录状态无效"))?;
    let logged_in = value
        .as_object()
        .and_then(|v| v.get("loggedIn"))
        .and_then(|v| v.as_bool())
        .ok_or_else(|| fail("Claude 登录状态无效"))?;
    if !logged_in {
        return Ok(false);
    }
    let method = value
        .get("authMethod")
        .and_then(|v| v.as_str())
        .ok_or_else(|| fail("Claude 登录状态无效"))?;
    Ok(matches!(method, "claude.ai" | "oauth_token"))
}
pub async fn run(home: &Path, mut cancel: oneshot::Receiver<()>) -> Result<bool> {
    let mut cmd = command(home)?;
    cmd.args(["auth", "login", "--claudeai"])
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = cmd.spawn().map_err(|_| fail("Claude 官方登录无法启动"))?;
    tokio::select! {
        _ = &mut cancel => {
            let _ = child.kill().await;
            Ok(false)
        },
        _ = tokio::time::sleep(std::time::Duration::from_secs(600)) => {
            let _ = child.kill().await;
            Err(fail("Claude 官方登录超时"))
        },
        exit = child.wait() => {
            if !exit.map_err(|_| fail("Claude 登录进程中断"))?.success() {
                return Err(fail("Claude 官方登录未完成"));
            }
            if !status(home).await? {
                return Err(fail("CLI 尚未确认官方账号登录"));
            }
            Ok(true)
        },
    }
}
pub fn running_sessions() -> Result<bool> {
    use sysinfo::{ProcessesToUpdate, System};
    let mut system = System::new();
    system.refresh_processes(ProcessesToUpdate::All, true);
    let own = system
        .process(sysinfo::Pid::from_u32(std::process::id()))
        .and_then(|p| p.user_id())
        .ok_or_else(|| fail("无法确认当前用户进程"))?;
    Ok(system.processes().values().any(|p| {
        if p.user_id() != Some(own) {
            return false;
        }
        let name = p
            .exe()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or("");
        if matches!(name, "claude" | "claude.exe") {
            return true;
        }
        matches!(name, "node" | "node.exe" | "bun" | "bun.exe")
            && p.cmd().iter().any(|a| {
                let text = a.to_string_lossy().replace('\\', "/");
                text.ends_with("/@anthropic-ai/claude-code/cli.js")
            })
    }))
}

#[cfg(test)]
#[path = "login_tests.rs"]
mod tests;
