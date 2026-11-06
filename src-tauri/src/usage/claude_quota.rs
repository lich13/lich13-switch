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
    pub fetched_at: Option<i64>,
    pub retry_at: Option<i64>,
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
}
#[derive(Default)]
pub struct Service(tokio::sync::Mutex<Option<Cached>>);

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

async fn read_credentials(home: &Path) -> Result<Credentials> {
    #[cfg(target_os = "macos")]
    {
        // Vela launches Claude with an explicit config directory. The CLI keys
        // that login by the first eight SHA-256 digits of the directory.
        let services = if let Ok(directory) = std::env::var("CLAUDE_SECURESTORAGE_CONFIG_DIR") {
            vec![if directory.is_empty() {
                "Claude Code-credentials".into()
            } else {
                format!(
                    "Claude Code-credentials-{}",
                    &storage::digest(directory.as_bytes())[..8]
                )
            }]
        } else {
            let mut services = vec![format!(
                "Claude Code-credentials-{}",
                &storage::digest(home.to_string_lossy().as_bytes())[..8]
            )];
            if dirs::home_dir().is_some_and(|dir| home == dir.join(".claude")) {
                services.push("Claude Code-credentials".into());
            }
            services
        };
        for service in services {
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
                let value = serde_json::from_slice(&output.stdout)
                    .map_err(|_| fail("Claude 登录凭据无法解析"))?;
                return credentials(value);
            }
        }
    }
    let raw = storage::read_optional(&home.join(".credentials.json"))?
        .ok_or_else(|| fail("请先在 Claude Code 登录官方账号"))?;
    credentials(serde_json::from_slice(&raw).map_err(|_| fail("Claude 登录凭据无法解析"))?)
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
    if let Some(limits) = value["limits"].as_array() {
        for (i, limit) in limits.iter().enumerate() {
            let Some(percent) = limit["percent"].as_f64() else {
                continue;
            };
            let kind = limit["kind"].as_str().unwrap_or("");
            let base = match kind {
                "session" => "当前 5h",
                "weekly_all" | "weekly_scoped" => "当前 7d",
                _ => "额度",
            };
            let scope = limit
                .pointer("/scope/model/display_name")
                .or_else(|| limit.pointer("/scope/surface/display_name"))
                .and_then(Value::as_str);
            windows.push(Window {
                id: format!("{kind}-{i}"),
                label: scope.map_or_else(|| base.into(), |s| format!("{s} · {base}")),
                used_percent: percent,
                resets_at: reset(&limit["resets_at"]),
            });
        }
    } else {
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
    }
    if windows.is_empty() {
        return Err(fail("Claude 未返回可用的周期额度"));
    }
    Ok(windows)
}

impl Service {
    pub async fn get(&self, home: PathBuf, refresh: bool) -> View {
        self.get_at(home, refresh, "https://api.anthropic.com/api/oauth/usage")
            .await
    }
    async fn get_at(&self, home: PathBuf, refresh: bool, endpoint: &str) -> View {
        let mut cache = self.0.lock().await;
        let now = super::model::now();
        if let Some(saved) = cache.as_ref().filter(|s| s.home == home) {
            if !refresh && saved.at.elapsed() < Duration::from_secs(300) {
                return saved.view.clone();
            }
        }
        let auth = match read_credentials(&home).await {
            Ok(auth) => auth,
            Err(error) => {
                let view = View {
                    error: Some(error.message),
                    ..Default::default()
                };
                *cache = Some(Cached {
                    home,
                    fingerprint: String::new(),
                    at: Instant::now(),
                    view: view.clone(),
                });
                return view;
            }
        };
        let fingerprint = storage::digest(auth.token.as_bytes());
        if let Some(saved) = cache
            .as_ref()
            .filter(|c| c.home == home && c.fingerprint == fingerprint)
        {
            if saved.view.retry_at.is_some_and(|at| at > now) {
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
        view.retry_at = None;
        let result = async {
            let client = reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(10))
                .build()
                .map_err(|_| fail("Claude 额度连接初始化失败"))?;
            let response = client
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
                    .and_then(|v| {
                        v.parse::<u64>().ok().or_else(|| {
                            httpdate::parse_http_date(v)
                                .ok()
                                .and_then(|t| t.duration_since(std::time::SystemTime::now()).ok())
                                .map(|d| d.as_secs())
                        })
                    })
                    .unwrap_or(300)
                    .max(1);
                view.retry_at = Some(
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
                view.fetched_at = Some(super::model::now());
            }
            Err(error) => view.error = Some(error.message),
        }
        *cache = Some(Cached {
            home,
            fingerprint,
            at: Instant::now(),
            view: view.clone(),
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
        let windows = parse(&json!({"limits":[{"kind":"weekly_scoped","percent":21.0,"scope":{"model":{"display_name":"Opus"}},"resets_at":null}]})).unwrap();
        assert_eq!(windows[0].label, "Opus · 当前 7d");
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
        write("fixture-one");
        let first = service.get_at(home.clone(), false, &endpoint).await;
        assert_eq!(first.account.as_deref(), Some("f***@example.invalid"));
        assert_eq!(first.windows.len(), 2);
        assert!(first.error.is_none());
        service.get_at(home.clone(), false, &endpoint).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let limited = service.get_at(home.clone(), true, &endpoint).await;
        assert!(limited.retry_at.unwrap() >= super::super::model::now() + 50_000);
        service.get_at(home.clone(), true, &endpoint).await;
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        // A newly logged-in account does not inherit another account's quota or throttle.
        write("fixture-two");
        assert!(service
            .get_at(home.clone(), true, &endpoint)
            .await
            .error
            .is_none());
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        let expired = service.get_at(home.clone(), true, &endpoint).await;
        assert!(expired.error.unwrap().contains("失效"));
        let denied = service.get_at(home.clone(), true, &endpoint).await;
        assert!(denied.error.unwrap().contains("权限"));
        std::fs::remove_file(home.join(".credentials.json")).unwrap();
        let absent = service.get_at(home, true, &endpoint).await;
        assert!(absent.error.unwrap().contains("登录"));
        assert!(absent.windows.is_empty());
        task.abort();
    }
}
