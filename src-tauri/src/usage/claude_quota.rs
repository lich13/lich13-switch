use crate::storage::{self, Result};
use serde::Serialize;
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Window {
    pub id: String,
    pub label: String,
    pub used_percent: f64,
    pub resets_at: Option<i64>,
}
#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct View {
    pub account: Option<String>,
    pub plan: Option<String>,
    pub windows: Vec<Window>,
    pub error: Option<String>,
}
struct Credentials {
    token: String,
    plan: Option<String>,
}
struct Cached {
    home: PathBuf,
    fingerprint: String,
    at: Instant,
    view: View,
    retry_at: Option<i64>,
}
pub struct Service {
    cache: tokio::sync::Mutex<Option<Cached>>,
    client: reqwest::Client,
}
impl Default for Service {
    fn default() -> Self {
        Self {
            cache: Default::default(),
            client: reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(10))
                .build()
                .expect("Claude 额度连接初始化失败"),
        }
    }
}

fn fail(message: &str) -> crate::storage::AppError {
    super::model::failure(message)
}
fn credentials(value: Value) -> Result<Credentials> {
    let oauth = &value["claudeAiOauth"];
    let token = oauth["accessToken"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| fail("请先在 Claude Code 登录官方账号"))?;
    if oauth["expiresAt"]
        .as_i64()
        .is_some_and(|at| at <= super::model::now())
    {
        return Err(fail("Claude 登录已过期，请打开 Claude Code 刷新登录"));
    }
    if oauth["scopes"]
        .as_array()
        .is_some_and(|scopes| !scopes.iter().any(|s| s == "user:profile"))
    {
        return Err(fail("Claude 登录缺少额度读取权限，请使用官方账号登录"));
    }
    Ok(Credentials {
        token: token.into(),
        plan: oauth["subscriptionType"].as_str().map(str::to_owned),
    })
}

fn parse_credentials(raw: &[u8]) -> Result<Credentials> {
    credentials(serde_json::from_slice(raw).map_err(|_| fail("Claude 登录凭据无法解析"))?)
}
#[cfg(any(test, not(target_os = "macos")))]
fn file_credentials(home: &Path) -> Result<Credentials> {
    let raw = storage::read_optional(&home.join(".credentials.json"))?
        .ok_or_else(|| fail("请先在 Claude Code 登录官方账号"))?;
    parse_credentials(&raw)
}
#[cfg(target_os = "macos")]
fn keychain_services(home: &Path) -> Vec<String> {
    // CLAUDE_CONFIG_DIR also gives the default directory a hashed entry name.
    let mut services = vec![format!(
        "Claude Code-credentials-{}",
        &storage::digest(home.to_string_lossy().as_bytes())[..8]
    )];
    if dirs::home_dir().is_some_and(|dir| home == dir.join(".claude")) {
        services.insert(0, "Claude Code-credentials".into());
    }
    services
}
async fn read_credentials(home: &Path) -> Result<Credentials> {
    #[cfg(target_os = "macos")]
    {
        let mut error = None;
        for service in keychain_services(home) {
            let output = tokio::time::timeout(
                Duration::from_secs(10),
                tokio::process::Command::new("/usr/bin/security")
                    .args(["find-generic-password", "-s", &service, "-w"])
                    .stdin(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .kill_on_drop(true)
                    .output(),
            )
            .await
            .map_err(|_| fail("Claude 登录读取超时，请重试"))?
            .map_err(|_| fail("无法读取 Claude 登录凭据"))?;
            if output.status.success() {
                match parse_credentials(&output.stdout) {
                    Ok(auth) => return Ok(auth),
                    Err(e) => error = Some(e),
                }
            }
        }
        if let Some(raw) = storage::read_optional(&home.join(".credentials.json"))? {
            return parse_credentials(&raw);
        }
        Err(error.unwrap_or_else(|| fail("请先在 Claude Code 登录官方账号")))
    }
    #[cfg(not(target_os = "macos"))]
    file_credentials(home)
}

fn account_label(home: &Path) -> Option<String> {
    let global = match dirs::home_dir() {
        Some(user) if home == user.join(".claude") => user.join(".claude.json"),
        _ => home.join(".claude.json"),
    };
    let raw = storage::read_optional(&global).ok()??;
    let value: Value = serde_json::from_slice(&raw).ok()?;
    let email = value.pointer("/oauthAccount/emailAddress")?.as_str()?;
    let (name, domain) = email.split_once('@')?;
    Some(format!("{}***@{domain}", name.chars().next()?))
}

fn reset(value: &Value) -> Option<i64> {
    value
        .as_str()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.timestamp_millis())
}
fn parse(value: &Value) -> Result<Vec<Window>> {
    let mut windows = Vec::new();
    for (id, label) in [
        ("five_hour", "当前 5h"),
        ("seven_day", "当前 7d"),
        ("seven_day_opus", "Opus · 当前 7d"),
        ("seven_day_sonnet", "Sonnet · 当前 7d"),
        ("seven_day_oauth_apps", "OAuth 应用 · 当前 7d"),
    ] {
        let Some(percent) = value[id]["utilization"].as_f64() else {
            continue;
        };
        windows.push(Window {
            id: id.into(),
            label: label.into(),
            used_percent: percent,
            resets_at: reset(&value[id]["resets_at"]),
        });
    }
    if windows.is_empty() {
        return Err(fail("Claude 未返回可用的周期额度"));
    }
    Ok(windows)
}

impl Service {
    pub async fn get(&self, home: PathBuf, refresh: bool) -> View {
        self.get_at(
            home.clone(),
            refresh,
            "https://api.anthropic.com/api/oauth/usage",
            read_credentials(&home),
        )
        .await
    }
    async fn get_at(
        &self,
        home: PathBuf,
        refresh: bool,
        endpoint: &str,
        auth: impl std::future::Future<Output = Result<Credentials>>,
    ) -> View {
        let mut cache = self.cache.lock().await;
        if let Some(saved) = cache
            .as_ref()
            .filter(|c| !refresh && c.home == home && c.at.elapsed() < Duration::from_secs(300))
        {
            return saved.view.clone();
        }
        let now = super::model::now();
        let auth = match auth.await {
            Ok(auth) => auth,
            Err(error) => {
                *cache = None;
                return View {
                    error: Some(error.message),
                    ..Default::default()
                };
            }
        };
        let fingerprint = storage::digest(auth.token.as_bytes());
        if let Some(saved) = cache
            .as_ref()
            .filter(|c| c.home == home && c.fingerprint == fingerprint)
        {
            if saved.retry_at.is_some_and(|at| at > now) {
                return saved.view.clone();
            }
        }
        let mut view = cache
            .as_ref()
            .filter(|c| c.home == home && c.fingerprint == fingerprint)
            .map(|c| c.view.clone())
            .unwrap_or_default();
        view.account = account_label(&home);
        view.plan = auth.plan;
        view.error = None;
        let mut retry_at = None;
        let result = async {
            let response = self
                .client
                .get(endpoint)
                .bearer_auth(&auth.token)
                .header("anthropic-beta", "oauth-2025-04-20")
                .send()
                .await
                .map_err(|_| fail("Claude 额度查询失败，请重试"))?;
            let status = response.status();
            if status.as_u16() == 429 {
                let delay = response
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(300)
                    .max(1);
                retry_at = Some(
                    now.saturating_add(
                        i64::try_from(delay)
                            .unwrap_or(i64::MAX)
                            .saturating_mul(1000),
                    ),
                );
                return Err(fail("Claude 额度查询受限，请稍后重试"));
            }
            if !status.is_success() {
                return Err(fail(match status.as_u16() {
                    401 => "Claude 登录已失效，请在 Claude Code 重新登录",
                    403 => "Claude 登录没有额度读取权限",
                    _ => "Claude 额度服务暂不可用，请重试",
                }));
            }
            let value: Value = response
                .json()
                .await
                .map_err(|_| fail("Claude 额度响应无法解析"))?;
            parse(&value)
        }
        .await;
        match result {
            Ok(windows) => {
                view.windows = windows;
            }
            Err(error) => view.error = Some(error.message),
        }
        *cache = Some(Cached {
            home,
            fingerprint,
            at: Instant::now(),
            view: view.clone(),
            retry_at,
        });
        view
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn parses_reported_windows_and_keeps_unknown_reset_empty() {
        let windows = parse(&json!({"five_hour":{"utilization":0,"resets_at":"2026-11-06T12:00:00Z"},"seven_day":{"utilization":42.5,"resets_at":null},"seven_day_opus":null})).unwrap();
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].used_percent, 0.0);
        assert_eq!(windows[1].resets_at, None);
        assert!(parse(&json!({})).is_err());
    }
    #[test]
    fn missing_expired_and_inference_only_credentials_report_errors() {
        assert!(credentials(json!({})).is_err());
        assert!(
            credentials(json!({"claudeAiOauth":{"accessToken":"fixture","expiresAt":1}})).is_err()
        );
        assert!(credentials(
            json!({"claudeAiOauth":{"accessToken":"fixture","scopes":["user:inference"]}})
        )
        .is_err());
        assert!(credentials(
            json!({"claudeAiOauth":{"accessToken":"fixture","scopes":["user:profile"]}})
        )
        .is_ok());
    }
    #[cfg(target_os = "macos")]
    #[test]
    fn keychain_name_matches_default_and_custom_directories() {
        let default = dirs::home_dir().unwrap().join(".claude");
        let custom = Path::new("/fixture/claude");
        let hashed = |home: &Path| {
            format!(
                "Claude Code-credentials-{}",
                &storage::digest(home.to_string_lossy().as_bytes())[..8]
            )
        };
        assert_eq!(
            keychain_services(&default),
            vec!["Claude Code-credentials".to_owned(), hashed(&default)]
        );
        assert_eq!(keychain_services(custom), vec![hashed(custom)]);
    }
    #[tokio::test]
    async fn cache_refresh_account_switch_and_http_failures() {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut bytes = vec![0; 8192];
                let count = stream.read(&mut bytes).await.unwrap();
                let headers = String::from_utf8_lossy(&bytes[..count]);
                assert!(headers.contains("anthropic-beta: oauth-2025-04-20"));
                assert!(headers.contains("authorization: Bearer fixture-"));
                let n = counter.fetch_add(1, Ordering::SeqCst);
                let (status, extra, body) = match n {
                    1 => ("429 Too Many Requests", "Retry-After: 60\r\n", "{}".into()),
                    3 => ("401 Unauthorized", "", "{}".into()),
                    4 => ("403 Forbidden", "", "{}".into()),
                    5 => ("429 Too Many Requests", "Retry-After: Tue, 01 Jan 2030 00:00:00 GMT\r\n", "{}".into()),
                    _ => ("200 OK", "", json!({"five_hour":{"utilization":30.0,"resets_at":null},"seven_day":{"utilization":40.0,"resets_at":null}}).to_string()),
                };
                stream.write_all(format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{extra}Connection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            }
        });
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_path_buf();
        let write = |token: &str| {
            std::fs::write(home.join(".credentials.json"), json!({"claudeAiOauth":{"accessToken":token,"subscriptionType":"max","scopes":["user:profile"]}}).to_string()).unwrap()
        };
        std::fs::write(
            home.join(".claude.json"),
            json!({"oauthAccount":{"emailAddress":"fixture@example.invalid"}}).to_string(),
        )
        .unwrap();
        let service = Service::default();
        let absent = service
            .get_at(home.clone(), false, &endpoint, async {
                file_credentials(&home)
            })
            .await;
        assert!(absent.error.unwrap().contains("登录"));
        assert!(service.cache.lock().await.is_none());
        write("fixture-one");
        let first = service
            .get_at(home.clone(), false, &endpoint, async {
                file_credentials(&home)
            })
            .await;
        assert_eq!(first.account.as_deref(), Some("f***@example.invalid"));
        assert_eq!(first.windows.len(), 2);
        assert!(first.error.is_none());
        service
            .get_at(home.clone(), false, &endpoint, async {
                panic!("cached quota must not read credentials")
            })
            .await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let limited = service
            .get_at(home.clone(), true, &endpoint, async {
                file_credentials(&home)
            })
            .await;
        assert!(limited.error.unwrap().contains("受限"));
        assert!(
            service
                .cache
                .lock()
                .await
                .as_ref()
                .unwrap()
                .retry_at
                .unwrap()
                >= super::super::model::now() + 50_000
        );
        service
            .get_at(home.clone(), true, &endpoint, async {
                file_credentials(&home)
            })
            .await;
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        // Automatic refresh can keep the previous account for five minutes.
        write("fixture-two");
        let cached = service
            .get_at(home.clone(), false, &endpoint, async {
                panic!("fresh cache must not read the changed account")
            })
            .await;
        assert!(cached.error.unwrap().contains("受限"));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        // Manual refresh reads the new account without inheriting the old throttle.
        assert!(service
            .get_at(home.clone(), true, &endpoint, async {
                file_credentials(&home)
            })
            .await
            .error
            .is_none());
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        let expired = service
            .get_at(home.clone(), true, &endpoint, async {
                file_credentials(&home)
            })
            .await;
        assert!(expired.error.unwrap().contains("失效"));
        let denied = service
            .get_at(home.clone(), true, &endpoint, async {
                file_credentials(&home)
            })
            .await;
        assert!(denied.error.unwrap().contains("权限"));
        let before = super::super::model::now();
        let date_limited = service
            .get_at(home.clone(), true, &endpoint, async {
                file_credentials(&home)
            })
            .await;
        assert!(date_limited.error.unwrap().contains("受限"));
        let retry_at = service
            .cache
            .lock()
            .await
            .as_ref()
            .unwrap()
            .retry_at
            .unwrap();
        assert!((before + 300_000..=super::super::model::now() + 300_000).contains(&retry_at));
        std::fs::remove_file(home.join(".credentials.json")).unwrap();
        service.cache.lock().await.as_mut().unwrap().at -= Duration::from_secs(301);
        let absent = service
            .get_at(home.clone(), false, &endpoint, async {
                file_credentials(&home)
            })
            .await;
        assert!(absent.error.unwrap().contains("登录"));
        assert!(absent.windows.is_empty());
        task.abort();
    }
}
