//! The only elevation is installation of a verified, narrow helper. Normal toggles use XPC.
use crate::storage::{self, AppError, Result};
use serde_json::{json, Value};
use std::{
    ffi::{CStr, CString},
    os::raw::c_char,
    path::Path,
    process::Command,
};
const APP: &str = "/Applications/lich13-switch.app/Contents/MacOS/lich13-switch";
const HELPER: &str = "/Library/PrivilegedHelperTools/com.lich13.gpt-switch.power-helper";
const IMAGE: &[u8] = include_bytes!(env!("GPT_POWER_HELPER"));
unsafe extern "C" {
    fn gs_power_request(
        json: *const c_char,
        requirement: *const c_char,
        timeout_ms: u64,
    ) -> *mut c_char;
    fn gs_power_identity() -> *mut c_char;
    fn gs_power_free(value: *mut c_char);
}
fn take(value: *mut c_char) -> Option<String> {
    if value.is_null() {
        return None;
    }
    let string = unsafe { CStr::from_ptr(value) }
        .to_string_lossy()
        .into_owned();
    unsafe {
        gs_power_free(value);
    }
    Some(string)
}
fn installed_app() -> bool {
    std::env::current_exe()
        .ok()
        .is_some_and(|p| p == Path::new(APP))
}
pub fn request(value: Value) -> Result<Value> {
    request_with_timeout(value, 15_000)
}
fn request_with_timeout(value: Value, timeout_ms: u64) -> Result<Value> {
    if !installed_app() {
        return Err(AppError::new(
            "POWER_ISOLATED",
            "请从 /Applications 中的正式应用使用电源助手",
        ));
    }
    let input = CString::new(value.to_string()).unwrap();
    let requirement = CString::new(format!(
        "identifier \"com.lich13.gpt-switch.power-helper\" and cdhash H\"{}\"",
        env!("GPT_POWER_HASH")
    ))
    .unwrap();
    let response =
        take(unsafe { gs_power_request(input.as_ptr(), requirement.as_ptr(), timeout_ms) })
            .ok_or_else(|| {
                AppError::new("POWER_HELPER", "电源助手不可用或应用版本未授权，请修复助手")
            })?;
    let response: Value = serde_json::from_str(&response)
        .map_err(|_| AppError::new("POWER_HELPER", "电源助手响应无效"))?;
    if let Some(e) = response.get("error") {
        let code = e
            .get("code")
            .and_then(Value::as_str)
            .unwrap_or("POWER_HELPER");
        let message = e
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("电源操作失败");
        return Err(AppError::new(code, message));
    }
    if response.get("version") != Some(&json!(1)) {
        return Err(AppError::new(
            "POWER_HELPER",
            "电源助手版本不兼容，请修复助手",
        ));
    }
    Ok(response)
}
pub fn status() -> &'static str {
    probe().0
}
/// Reuse the authenticated state reply; a second XPC probe can fail after the first succeeds.
pub fn probe() -> (&'static str, Option<Value>) {
    if !installed_app() {
        return ("isolated", None);
    }
    if !Path::new(HELPER).exists() {
        return ("notInstalled", None);
    }
    match request(json!({"op":"state"})) {
        Ok(response)
            if response["state"]["enabled"].is_boolean()
                && response["state"]["batterySleep"]
                    .as_u64()
                    .is_some_and(|v| v <= 1440) =>
        {
            ("ready", Some(response))
        }
        _ => ("needsRepair", None),
    }
}
fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}
fn installation_script(
    source: &Path,
    stage: &str,
    digest: &str,
    uid: u32,
    hash: &str,
    transaction: &str,
) -> String {
    format!(
        r#"fail() {{
/usr/bin/printf '{{"error":{{"code":"POWER_INSTALL_%s","message":"%s；未更改注册状态"}},"rollback":"unchanged"}}' "$1" "$2"
exit 0
}}
safe_directory() {{
/bin/test ! -L "$1" && /bin/test -d "$1" || return 1
/bin/test "$(/usr/bin/stat -f '%u' "$1")" = 0 || return 1
mode=$(/usr/bin/stat -f '%Lp' "$1") || return 1
case "$mode" in ''|*[!0-7]*) return 1 ;; esac
/bin/test "$((0$mode & 022))" -eq 0
}}
if /bin/test ! -e /Library/PrivilegedHelperTools && /bin/test ! -L /Library/PrivilegedHelperTools; then
/bin/mkdir -m 755 /Library/PrivilegedHelperTools || fail DIRECTORY '无法创建系统助手目录'
fi
safe_directory /Library/PrivilegedHelperTools || fail DIRECTORY '系统助手目录权限不安全'
trap '/bin/rm -f -- {stage}' EXIT
/usr/bin/install -o root -g wheel -m 700 {source} {stage} 2>/dev/null || fail WRITE '无法暂存电源助手'
/bin/test "$(/usr/bin/shasum -a 256 {stage} | /usr/bin/cut -d ' ' -f 1)" = {digest} || fail SIGNATURE '电源助手载荷校验失败'
{stage} --install {uid} {app} {hash} {transaction} 2>/dev/null || fail EXECUTION '无法运行电源助手安装程序'
"#,
        source = quote(&source.to_string_lossy()),
        digest = quote(digest),
        app = quote(APP),
        hash = quote(hash),
        transaction = quote(transaction)
    )
}
fn installation_result(output: &std::process::Output) -> Result<()> {
    if !output.status.success() {
        return Err(AppError::new(
            "POWER_AUTH",
            if String::from_utf8_lossy(&output.stderr).contains("-128") {
                "已取消系统授权"
            } else {
                "系统授权或安装程序中断，无法确认注册状态，请重新修复助手"
            },
        ));
    }
    let result: Value = serde_json::from_slice(&output.stdout).map_err(|_| {
        AppError::new(
            "POWER_INSTALL_RESULT",
            "安装结果无效，无法确认注册状态，请重新修复助手",
        )
    })?;
    if let Some(error) = result.get("error") {
        return Err(AppError::new(
            error["code"].as_str().unwrap_or("POWER_INSTALL_RESULT"),
            error["message"].as_str().unwrap_or("电源助手安装未完成"),
        ));
    }
    if result.get("version") != Some(&json!(1)) {
        return Err(AppError::new(
            "POWER_INSTALL_RESULT",
            "安装结果版本无效，请重新修复助手",
        ));
    }
    Ok(())
}
pub fn ensure(force: bool) -> Result<()> {
    if !installed_app() {
        return Err(AppError::new(
            "POWER_ISOLATED",
            "隔离程序不注册系统电源助手",
        ));
    }
    if !force && status() == "ready" {
        return Ok(());
    }
    let hash = take(unsafe { gs_power_identity() })
        .filter(|s| s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit()))
        .ok_or_else(|| AppError::new("POWER_AUTH", "无法校验当前应用签名"))?;
    let folder = tempfile::tempdir().map_err(storage::io_error)?;
    let payload = folder.path().join("power-helper");
    storage::atomic_write(&payload, IMAGE, None)?;
    let stage = format!(
        "/Library/PrivilegedHelperTools/com.lich13.gpt-switch.power-install-{}",
        uuid::Uuid::new_v4()
    );
    let uid = unsafe { libc::geteuid() };
    // Copy into a root-owned directory, then hash the destination before ever
    // executing it. A modified user-writable staging file cannot become root code.
    let transaction = uuid::Uuid::new_v4().to_string();
    let shell = installation_script(
        &payload,
        &stage,
        &storage::digest(IMAGE),
        uid,
        &hash,
        &transaction,
    );
    let script = format!(
        "do shell script \"{}\" with administrator privileges",
        shell
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
    );
    let mut child = Command::new("/usr/bin/osascript")
        .args(["-e", &script])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(storage::io_error)?;
    let mut verified = false;
    while child.try_wait().map_err(storage::io_error)?.is_none() {
        if !verified {
            verified = request_with_timeout(
                json!({"op":"verifyInstall", "transaction":transaction}),
                2_000,
            )
            .is_ok_and(|reply| {
                reply["state"]["supported"].is_boolean()
                    && reply["state"]["enabled"].is_boolean()
                    && reply["state"]["batterySleep"]
                        .as_u64()
                        .is_some_and(|n| n <= 1440)
            });
        }
        std::thread::sleep(std::time::Duration::from_millis(150));
    }
    installation_result(&child.wait_with_output().map_err(storage::io_error)?)?;
    if !verified {
        return Err(AppError::new(
            "POWER_INSTALL_CONNECT",
            "助手未通过应用连接校验，请重新修复",
        ));
    }
    request(json!({"op":"state"}))?;
    Ok(())
}
pub fn remove() -> Result<()> {
    request(json!({"op":"remove"})).map(|_| ())
}
#[cfg(test)]
mod tests {
    use std::os::unix::process::ExitStatusExt;
    #[test]
    fn install_errors_preserve_stage_and_never_reveal_raw_stderr() {
        let output = |ok: bool, stdout: &[u8], stderr: &[u8]| std::process::Output {
            status: std::process::ExitStatus::from_raw(if ok { 0 } else { 256 }),
            stdout: stdout.to_vec(),
            stderr: stderr.to_vec(),
        };
        let error = super::installation_result(&output(true,
            br#"{"error":{"code":"POWER_INSTALL_REGISTER","message":"service failed; restored"},"rollback":"restored"}"#, b"private diagnostic")).unwrap_err();
        assert_eq!(error.code, "POWER_INSTALL_REGISTER");
        assert_eq!(error.message, "service failed; restored");
        let cancelled =
            super::installation_result(&output(false, b"", b"User canceled (-128)")).unwrap_err();
        assert_eq!(cancelled.message, "已取消系统授权");
        let interrupted =
            super::installation_result(&output(false, b"", b"private diagnostic")).unwrap_err();
        assert!(!interrupted.message.contains("private"));
        assert!(!interrupted.message.contains("已恢复"));
        assert!(super::installation_result(&output(true, b"{}", b"")).is_err());
        assert!(super::installation_result(&output(true, br#"{"version":1}"#, b"")).is_ok());
    }
    #[test]
    fn staging_guard_accepts_safe_system_directory_and_rejects_symlinks_or_user_owned_paths() {
        let script = super::installation_script(
            std::path::Path::new("/unused"),
            "/unused",
            "unused",
            501,
            "unused",
            "unused",
        );
        // Execute only the read-only preflight functions, never installation commands.
        let functions = script.split("\nif /bin/test").next().unwrap();
        let check = |path: &std::path::Path| {
            std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(format!(
                    "{}\nsafe_directory {}",
                    functions,
                    super::quote(&path.to_string_lossy())
                ))
                .status()
                .unwrap()
                .success()
        };
        assert!(check(std::path::Path::new("/Library")));
        let system = std::path::Path::new("/Library/PrivilegedHelperTools");
        if system.exists() {
            assert!(check(system));
        }
        let temp = tempfile::tempdir().unwrap();
        assert!(!check(temp.path()));
        let link = temp.path().join("system-link");
        std::os::unix::fs::symlink("/Library", &link).unwrap();
        assert!(!check(&link));
    }
    #[test]
    fn native_helper_transactions_are_simulated() {
        let output = std::process::Command::new(env!("GPT_POWER_TEST"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    #[test]
    fn isolated_binary_never_installs_or_calls_production_service() {
        assert!(!super::installed_app());
        assert_eq!(super::status(), "isolated");
        assert_eq!(super::ensure(false).unwrap_err().code, "POWER_ISOLATED");
        assert!(super::request(serde_json::json!({"op":"set"})).is_err());
    }
    #[test]
    fn quotes_cannot_expand_shell_syntax() {
        assert_eq!(super::quote("a'b$(id)`x`"), "'a'\\''b$(id)`x`'");
    }
}
