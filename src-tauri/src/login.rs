use crate::errors::code;
use crate::storage::{self, AppError, Result};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    sync::{mpsc, oneshot},
    time::{Duration, Instant},
};

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LoginState {
    pub phase: String,
    pub mode: String,
    pub url: Option<String>,
    pub code: Option<String>,
    pub message: String,
    #[serde(default)]
    pub target_account_id: Option<String>,
    /// The complete authorization URL stays in the Rust session so it is not
    /// exposed through login-state events.
    #[serde(skip)]
    pub auth_url: Option<String>,
}
pub struct Session {
    pub state: LoginState,
    pub cancel: Option<oneshot::Sender<()>>,
}
#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CopyKind {
    Url,
    Code,
}
impl Session {
    pub fn copy_value<E>(
        &self,
        kind: CopyKind,
        write: impl FnOnce(&str) -> std::result::Result<(), E>,
    ) -> Result<()> {
        if self.cancel.is_none()
            || self.state.mode != "device"
            || !["starting", "waiting"].contains(&self.state.phase.as_str())
        {
            return Err(AppError::new(
                code::LOGIN_STATE,
                "设备码登录已结束或正在取消",
            ));
        }
        let value = match kind {
            CopyKind::Url => self.state.url.as_deref(),
            CopyKind::Code => self.state.code.as_deref(),
        }
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::new(code::LOGIN_PENDING, "登录链接或设备码尚未生成"))?;
        write(value).map_err(|_| AppError::new(code::CLIPBOARD, "无法写入剪贴板，请重试"))
    }
}
impl Default for Session {
    fn default() -> Self {
        Self {
            state: LoginState {
                phase: "idle".into(),
                ..Default::default()
            },
            cancel: None,
        }
    }
}
pub fn resolve_cli(configured: &str) -> Result<PathBuf> {
    if !configured.is_empty() {
        let p = PathBuf::from(configured);
        if p.is_absolute() && p.is_file() {
            return Ok(p);
        }
        return Err(AppError::new(
            code::CLI,
            "配置的 Codex CLI 不存在，请在设置中重新选择",
        ));
    }
    let names = if cfg!(windows) {
        vec!["codex.exe", "codex.cmd"]
    } else {
        vec!["codex"]
    };
    let mut dirs: Vec<PathBuf> =
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).collect();
    if let Some(home) = dirs::home_dir() {
        dirs.extend([
            home.join(".local/bin"),
            home.join(".npm-global/bin"),
            PathBuf::from("/opt/homebrew/bin"),
            PathBuf::from("/usr/local/bin"),
        ]);
        if let Ok(versions) = std::fs::read_dir(home.join(".nvm/versions/node")) {
            let mut v: Vec<_> = versions.flatten().map(|v| v.path().join("bin")).collect();
            v.sort();
            v.reverse();
            dirs.extend(v);
        }
    }
    #[cfg(windows)]
    if let Some(appdata) = std::env::var_os("APPDATA") {
        dirs.push(PathBuf::from(appdata).join("npm"));
    }
    for dir in dirs {
        for name in &names {
            let p = dir.join(name);
            if p.is_file() {
                return Ok(p);
            }
        }
    }
    Err(AppError::new(
        code::CLI_MISSING,
        "未找到官方 Codex CLI，请安装后在设置中选择其路径",
    ))
}
fn command(path: &Path) -> Result<tokio::process::Command> {
    #[cfg(windows)]
    if path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("cmd"))
    {
        let entry = path
            .parent()
            .unwrap_or(Path::new("."))
            .join("node_modules/@openai/codex/bin/codex.js");
        if !entry.is_file() {
            return Err(AppError::new(
                code::CLI,
                "此 CMD 不是标准 Codex npm 安装，请选择 codex.exe",
            ));
        }
        let adjacent = path.parent().unwrap().join("node.exe");
        let mut c = tokio::process::Command::new(if adjacent.is_file() {
            adjacent
        } else {
            PathBuf::from("node.exe")
        });
        c.arg(entry);
        return Ok(c);
    }
    Ok(tokio::process::Command::new(path))
}
fn extract_url(line: &str) -> Option<String> {
    line.split_whitespace()
        .map(|s| s.trim_matches(|c: char| ['"', '\'', '(', ')', '\u{1b}'].contains(&c)))
        .find(|s| {
            s.starts_with("https://auth.openai.com/")
                || s.starts_with("https://auth.openai.com?")
                || s.starts_with("https://chatgpt.com/")
        })
        .map(str::to_owned)
}
fn public_login_url(url: &str, browser: bool) -> String {
    if !browser {
        return url.to_owned();
    }
    let Ok(mut parsed) = url::Url::parse(url) else {
        return url.to_owned();
    };
    // Keep the complete authorization URL in the Rust session only. The
    // webview needs to know that a URL exists, but must not receive the
    // loopback callback address or its ephemeral port through an event.
    parsed.set_query(None);
    parsed.set_fragment(None);
    parsed.into()
}

pub fn update_prompt(state: &mut LoginState, line: &str) {
    let line = strip_ansi(line);
    if let Some(url) = extract_url(&line) {
        state.auth_url = Some(url.clone());
        state.url = Some(public_login_url(&url, state.mode == "browser"));
    }
    if state.mode == "device" {
        for word in line.split_whitespace() {
            let word = word.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '-');
            let stripped = word.replace('-', "");
            let parts: Vec<_> = word.split('-').collect();
            if parts.len() == 2
                && parts.iter().all(|part| {
                    (3..=8).contains(&part.len())
                        && part
                            .chars()
                            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
                })
                && (8..=16).contains(&stripped.len())
            {
                state.code = Some(word.to_string());
            }
        }
    }
}
fn strip_ansi(line: &str) -> String {
    let mut clean = String::new();
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            for c in chars.by_ref() {
                if ('@'..='~').contains(&c) {
                    break;
                }
            }
        } else {
            clean.push(c);
        }
    }
    clean
}
async fn terminate(child: &mut tokio::process::Child) {
    if let Some(pid) = child.id() {
        #[cfg(unix)]
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
        #[cfg(windows)]
        {
            let _ = tokio::process::Command::new("taskkill.exe")
                .args(["/PID", &pid.to_string(), "/T", "/F"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .creation_flags(0x08000000)
                .status()
                .await;
        }
    }
    let _ = child.kill().await;
    let _ = child.wait().await;
}
pub async fn run(
    cli: &Path,
    mode: &str,
    mut cancel: oneshot::Receiver<()>,
    events: mpsc::Sender<LoginState>,
) -> Result<Option<String>> {
    let temp = storage::private_tempdir(&std::env::temp_dir(), "vela-login-")?;
    let mut cmd = command(cli)?;
    cmd.arg("login")
        .args(["-c", "cli_auth_credentials_store=\"file\""])
        .env("CODEX_HOME", temp.path())
        .current_dir(temp.path())
        .env_remove("OPENAI_API_KEY")
        .env_remove("CODEX_ACCESS_TOKEN")
        .env_remove("CODEX_API_KEY");
    if mode == "device" {
        cmd.arg("--device-auth");
    }
    // Finder does not inherit the terminal's nvm PATH. Add the chosen CLI's own bin directory.
    let mut paths = vec![cli.parent().unwrap_or(Path::new(".")).to_path_buf()];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    if let Ok(path) = std::env::join_paths(paths) {
        cmd.env("PATH", path);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(0x08000000);
    #[cfg(unix)]
    cmd.process_group(0);
    let mut child = cmd.spawn().map_err(|_| {
        AppError::new(
            code::CLI_START,
            "无法启动 Codex CLI，请检查可执行文件及 Node.js 环境",
        )
    })?;
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let (tx, mut rx) = mpsc::channel::<String>(16);
    let tx2 = tx.clone();
    let out = tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if line.len() < 16384 && tx.send(line).await.is_err() {
                break;
            }
        }
    });
    let err = tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if line.len() < 16384 && tx2.send(line).await.is_err() {
                break;
            }
        }
    });
    let mut state = LoginState {
        phase: "waiting".into(),
        mode: mode.into(),
        message: "请在浏览器中完成 ChatGPT 登录".into(),
        ..Default::default()
    };
    let _ = events.send(state.clone()).await;
    let deadline = Instant::now() + Duration::from_secs(600);
    let result = loop {
        tokio::select! {
            _=&mut cancel=>{terminate(&mut child).await;break Ok(None);},
            _=tokio::time::sleep_until(deadline)=>{terminate(&mut child).await;break Err(AppError::new(code::LOGIN_TIMEOUT,"登录已超时，请重试"));},
            Some(line)=rx.recv()=>{let before=state.clone();update_prompt(&mut state,&line);if state!=before{let _=events.send(state.clone()).await;}},
            status=child.wait()=>{
                break (|| {
                    if !status.map_err(storage::io_error)?.success(){return Err(AppError::new(code::LOGIN_FAILED,"官方登录未完成；请检查浏览器授权、网络或改用设备码登录"));}
                    let raw=storage::read_optional(&temp.path().join("auth.json"))?.ok_or_else(||AppError::new(code::LOGIN_EMPTY,"登录进程未生成 auth.json，请检查本机认证策略"))?;
                    String::from_utf8(raw).map(Some).map_err(|_|AppError::new(code::LOGIN_FORMAT,"登录生成的凭据格式无效"))
                })();
            }
        }
    };
    out.abort();
    err.abort();
    let _ = out.await;
    let _ = err.await;
    temp.close().map_err(|_| {
        AppError::new(
            code::LOGIN_CLEANUP,
            "登录临时目录无法清理，请检查临时目录权限",
        )
    })?;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn device_copy_uses_only_the_live_session_and_redacts_clipboard_failures() {
        let (cancel, _rx) = oneshot::channel();
        let mut s = Session {
            state: LoginState {
                phase: "waiting".into(),
                mode: "device".into(),
                ..Default::default()
            },
            cancel: Some(cancel),
        };
        assert!(s
            .copy_value(CopyKind::Url, |_| -> std::result::Result<(), ()> {
                panic!("no prompt yet")
            })
            .is_err());
        update_prompt(
            &mut s.state,
            "https://auth.openai.com/codex/device ABCD-EFGH",
        );
        for (kind, expected) in [
            (CopyKind::Url, "https://auth.openai.com/codex/device"),
            (CopyKind::Code, "ABCD-EFGH"),
        ] {
            let mut copied = String::new();
            s.copy_value(kind, |text| {
                copied = text.into();
                Ok::<_, ()>(())
            })
            .unwrap();
            assert_eq!(copied, expected);
        }
        let err = s
            .copy_value(CopyKind::Code, |_| Err("ABCD-EFGH must not leak"))
            .unwrap_err();
        assert_eq!(err.code, code::CLIPBOARD);
        assert!(!err.message.contains("ABCD"));
        for phase in ["cancelling", "cancelled", "success", "error", "idle"] {
            s.state.phase = phase.into();
            assert!(s
                .copy_value(CopyKind::Code, |_| -> std::result::Result<(), ()> {
                    panic!("inactive session")
                })
                .is_err());
        }
        s.state.phase = "waiting".into();
        s.cancel.take(); // Reject a delayed prompt even if its phase says waiting.
        assert!(s
            .copy_value(CopyKind::Url, |_| -> std::result::Result<(), ()> {
                panic!("cancelled session")
            })
            .is_err());
        assert!(serde_json::from_str::<CopyKind>(r#""text""#).is_err());
    }
    #[test]
    fn output_is_filtered() {
        let mut s = LoginState {
            mode: "device".into(),
            ..Default::default()
        };
        update_prompt(
            &mut s,
            "Open https://auth.openai.com/codex/device and enter ABCD-EFGH",
        );
        assert_eq!(s.code.as_deref(), Some("ABCD-EFGH"));
        assert!(s.url.unwrap().starts_with("https://auth.openai.com/"));
        let mut s = LoginState::default();
        update_prompt(&mut s, "access_token=private https://evil.invalid/");
        assert!(s.url.is_none());
        assert!(s.message.is_empty());
    }
    #[test]
    fn accepts_current_cli_device_code_format() {
        let mut s = LoginState {
            mode: "device".into(),
            ..Default::default()
        };
        update_prompt(
            &mut s,
            "2. Enter this one-time code 91CX-VA5M3 (expires in 15 minutes)",
        );
        assert_eq!(s.code.as_deref(), Some("91CX-VA5M3"));
    }

    #[test]
    fn browser_prompt_keeps_the_redirect_out_of_the_public_state() {
        let mut s = LoginState {
            mode: "browser".into(),
            ..Default::default()
        };
        update_prompt(
            &mut s,
            "https://auth.openai.com/authorize?redirect_uri=http%3A%2F%2Flocalhost%3A1888%2Fauth%2Fcallback",
        );
        assert!(s
            .auth_url
            .as_deref()
            .is_some_and(|url| url.contains("1888")));
        let json = serde_json::to_string(&s).unwrap();
        assert!(!json.contains("1888"));
    }
    #[test]
    fn missing_cli_is_clear() {
        assert!(resolve_cli("/missing/vela-codex").is_err());
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn cli_login_is_isolated_and_cleans_its_directory() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = tempfile::tempdir().unwrap();
        let script = fixture.path().join("codex");
        let marker = fixture.path().join("home-path");
        let body = format!("#!/bin/sh\nprintf '%s' \"$CODEX_HOME\" > '{}'\nprintf '%s' '{{\"auth_mode\":\"apikey\",\"OPENAI_API_KEY\":\"fixture-only\"}}' > \"$CODEX_HOME/auth.json\"\n", marker.display());
        std::fs::write(&script, body).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let (_cancel, rx) = oneshot::channel();
        let (events, _receiver) = mpsc::channel(8);
        let raw = run(&script, "browser", rx, events).await.unwrap().unwrap();
        assert!(raw.contains("fixture-only"));
        let path = std::fs::read_to_string(marker).unwrap();
        assert!(path.contains("vela-login-"));
        assert!(!Path::new(&path).exists());
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn cancellation_kills_cli_tree_and_cleans_directory() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = tempfile::tempdir().unwrap();
        let script = fixture.path().join("codex");
        let marker = fixture.path().join("home-path");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf '%s' \"$CODEX_HOME\" > '{}'\nsleep 60 &\nwait\n",
                marker.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let (cancel, rx) = oneshot::channel();
        let (events, mut receiver) = mpsc::channel(8);
        let execution = run(&script, "browser", rx, events);
        tokio::pin!(execution);
        tokio::select! {_=&mut execution=>panic!("login ended before cancel"),_ = receiver.recv()=>{}}
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if std::fs::metadata(&marker).is_ok_and(|m| m.len() > 0) {
                    break;
                }
                tokio::select! {
                    _ = &mut execution => panic!("login ended before marker"),
                    _ = tokio::time::sleep(Duration::from_millis(10)) => (),
                }
            }
        })
        .await
        .expect("fixture CLI did not start");
        let path = std::fs::read_to_string(marker).unwrap();
        cancel.send(()).unwrap();
        assert!(tokio::time::timeout(Duration::from_secs(3), execution)
            .await
            .unwrap()
            .unwrap()
            .is_none());
        assert!(!Path::new(&path).exists());
    }
}
