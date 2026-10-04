//! Read-only provider quota queries. Never participate in business routing or circuit health.
use super::{circuit, connector, replay, HttpClient};
use crate::storage::{AppError, Result};
use http_body_util::BodyExt;
use hyper::{header, Request};
use serde::Serialize;
use serde_json::Value;
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{broadcast, watch, Semaphore};
const LIMIT: usize = 2_000_000;
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
#[derive(Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Plan {
    pub name: String,
    pub remaining: Option<f64>,
    pub used: Option<f64>,
    pub total: Option<f64>,
    pub unit: String,
    pub unlimited: bool,
    pub reset_at: Option<String>,
}
#[derive(Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    pub requests: Option<f64>,
    pub tokens: Option<f64>,
    pub cost: Option<f64>,
}
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QuotaView {
    pub provider_id: String,
    pub version: String,
    pub state: String,
    pub source: Option<String>,
    pub checked_at: Option<u64>,
    pub success_at: Option<u64>,
    pub retry_at: Option<u64>,
    pub next_refresh_at: Option<u64>,
    pub stale: bool,
    pub error: Option<String>,
    pub key_status: Option<String>,
    pub plans: Vec<Plan>,
    pub expires_at: Option<String>,
    pub expires_at_unix: Option<i64>,
    pub today: Option<Usage>,
    pub total_usage: Option<Usage>,
}
impl QuotaView {
    fn empty(id: &str, version: &str) -> Self {
        Self {
            provider_id: id.into(),
            version: version.into(),
            state: "idle".into(),
            source: None,
            checked_at: None,
            success_at: None,
            retry_at: None,
            next_refresh_at: None,
            stale: false,
            error: None,
            key_status: None,
            plans: vec![],
            expires_at: None,
            expires_at_unix: None,
            today: None,
            total_usage: None,
        }
    }
}
struct Entry {
    view: QuotaView,
    flight: Option<watch::Receiver<Option<QuotaView>>>,
}
struct Shared {
    entries: Mutex<HashMap<String, Entry>>,
    interval: AtomicU64,
    permits: Semaphore,
    events: broadcast::Sender<QuotaView>,
}
#[derive(Clone)]
pub struct Service(Arc<Shared>);
pub struct Query {
    pub client_id: super::ClientId,
    pub id: String,
    pub version: String,
    pub base: String,
    pub token: String,
    pub client: HttpClient,
}
impl Service {
    pub fn new() -> Self {
        let (events, _) = broadcast::channel(64);
        Self(Arc::new(Shared {
            entries: Mutex::new(HashMap::new()),
            interval: AtomicU64::new(60),
            permits: Semaphore::new(3),
            events,
        }))
    }
    pub fn subscribe(&self) -> broadcast::Receiver<QuotaView> {
        self.0.events.subscribe()
    }
    pub fn set_interval(&self, seconds: u64) {
        self.0.interval.store(seconds, Ordering::Relaxed);
        for e in self.0.entries.lock().unwrap().values_mut() {
            self.schedule(&mut e.view);
            let _ = self.0.events.send(e.view.clone());
        }
    }
    fn schedule(&self, v: &mut QuotaView) {
        let seconds = self.0.interval.load(Ordering::Relaxed);
        v.next_refresh_at = (seconds > 0).then(|| {
            v.checked_at
                .unwrap_or(0)
                .saturating_add(seconds)
                .max(v.retry_at.unwrap_or(0))
        });
        v.stale = v.success_at.is_some()
            && (v.state == "error"
                || v.state == "unsupported"
                || (seconds > 0
                    && v.success_at
                        .is_some_and(|t| now().saturating_sub(t) >= seconds)));
    }
    pub fn cached(&self, id: &str, version: &str) -> Option<QuotaView> {
        self.0
            .entries
            .lock()
            .unwrap()
            .get(id)
            .filter(|e| e.view.version == version)
            .map(|e| {
                let mut view = e.view.clone();
                self.schedule(&mut view);
                view
            })
    }
    pub fn retain(&self, versions: &HashMap<String, String>) {
        self.0
            .entries
            .lock()
            .unwrap()
            .retain(|id, e| versions.get(id) == Some(&e.view.version));
    }
    pub async fn query(&self, input: Query, force: bool) -> Result<QuotaView> {
        let mut rx = {
            let mut entries = self.0.entries.lock().unwrap();
            let e = entries.entry(input.id.clone()).or_insert_with(|| Entry {
                view: QuotaView::empty(&input.id, &input.version),
                flight: None,
            });
            if e.view.version != input.version {
                *e = Entry {
                    view: QuotaView::empty(&input.id, &input.version),
                    flight: None,
                };
            }
            if let Some(rx) = &e.flight {
                rx.clone()
            } else {
                self.schedule(&mut e.view);
                if e.view.retry_at.is_some_and(|t| t > now())
                    || (!force
                        && (self.0.interval.load(Ordering::Relaxed) == 0
                            || e.view.next_refresh_at.is_some_and(|t| t > now())))
                {
                    return Ok(e.view.clone());
                }
                let previous = e.view.clone();
                e.view.state = "loading".into();
                let (tx, rx) = watch::channel(None);
                e.flight = Some(rx.clone());
                let _ = self.0.events.send(e.view.clone());
                let service = self.clone();
                tokio::spawn(async move {
                    let _permit = service
                        .0
                        .permits
                        .acquire()
                        .await
                        .expect("quota semaphore open");
                    let result = fetch(&input).await;
                    let mut view = previous;
                    view.checked_at = Some(now());
                    view.retry_at = None;
                    match result {
                        Ok(Some(data)) => {
                            view.source = Some(data.source.into());
                            view.plans = data.plans;
                            view.key_status = data.key_status;
                            view.expires_at = data.expires_at;
                            view.expires_at_unix = data.expires_at_unix;
                            view.today = data.today;
                            view.total_usage = data.total_usage;
                            view.state = "ok".into();
                            view.stale = false;
                            view.error = None;
                            view.success_at = view.checked_at;
                        }
                        Ok(None) => {
                            view.state = "unsupported".into();
                            view.error =
                                Some("不可查询：未识别 Sub2API 或 New API 额度接口".into());
                            view.stale = view.success_at.is_some();
                        }
                        Err(e) => {
                            view.state = "error".into();
                            view.error = Some(e.message);
                            view.retry_at = e.retry_at;
                            view.stale = view.success_at.is_some();
                        }
                    }
                    service.schedule(&mut view);
                    let mut entries = service.0.entries.lock().unwrap();
                    if let Some(e) = entries
                        .get_mut(&input.id)
                        .filter(|e| e.view.version == input.version)
                    {
                        e.view = view.clone();
                        e.flight = None;
                        let _ = service.0.events.send(view.clone());
                    }
                    tx.send_replace(Some(view));
                });
                rx
            }
        };
        loop {
            if let Some(v) = rx.borrow().clone() {
                return Ok(v);
            }
            rx.changed()
                .await
                .map_err(|_| AppError::new("QUOTA", "额度查询已取消"))?;
        }
    }
}
struct Failure {
    message: String,
    retry_at: Option<u64>,
}
impl Failure {
    fn new(s: &str) -> Self {
        Self {
            message: s.into(),
            retry_at: None,
        }
    }
}
struct Reply {
    status: u16,
    body: Option<Value>,
}
async fn get(input: &Query, url: String, auth: bool) -> std::result::Result<Reply, Failure> {
    let request = async {
        let mut req = Request::builder()
            .method("GET")
            .uri(url)
            .header(header::ACCEPT, "application/json")
            .header(header::ACCEPT_ENCODING, "identity")
            .header(
                header::USER_AGENT,
                concat!("lich13-switch/", env!("CARGO_PKG_VERSION")),
            );
        if auth {
            req = req.header(header::AUTHORIZATION, format!("Bearer {}", input.token));
        }
        let req = req
            .body(replay::empty())
            .map_err(|_| Failure::new("供应商地址或 Key 格式无效"))?;
        let mut response = input.client.request(req).await.map_err(|e| {
            Failure::new(
                &connector::classify(&e)
                    .map(|e| e.to_string())
                    .unwrap_or_else(|| "额度接口连接失败".into()),
            )
        })?;
        let status = response.status().as_u16();
        if status == 429 {
            let retry = response
                .headers()
                .get(header::RETRY_AFTER)
                .and_then(|h| h.to_str().ok())
                .and_then(circuit::retry_after)
                .map(|d| now() + d.as_secs().max(1));
            return Err(Failure {
                message: "额度查询被限流".into(),
                retry_at: Some(retry.unwrap_or_else(|| now() + 60)),
            });
        }
        if (300..400).contains(&status) {
            return Err(Failure::new("额度接口要求跳转，已停止发送凭据"));
        }
        let mut bytes = Vec::new();
        while let Some(frame) = response.body_mut().frame().await {
            let frame = frame.map_err(|_| Failure::new("额度响应中断"))?;
            if let Some(data) = frame.data_ref() {
                if bytes.len() + data.len() > LIMIT {
                    return Err(Failure::new("额度响应超过 2 MB"));
                }
                bytes.extend_from_slice(data);
            }
        }
        Ok(Reply {
            status,
            body: serde_json::from_slice(&bytes).ok().map(|mut value| {
                redact_echoed_key(&mut value, &input.token);
                value
            }),
        })
    };
    tokio::time::timeout(Duration::from_secs(10), request)
        .await
        .map_err(|_| Failure::new("额度查询超时"))?
}
fn redact_echoed_key(value: &mut Value, key: &str) {
    if key.is_empty() {
        return;
    }
    match value {
        Value::String(s) => *s = s.replace(key, "[已隐藏]"),
        Value::Array(values) => values.iter_mut().for_each(|v| redact_echoed_key(v, key)),
        Value::Object(values) => values.values_mut().for_each(|v| redact_echoed_key(v, key)),
        _ => {}
    }
}
fn urls(base: &str) -> std::result::Result<(String, String, String), Failure> {
    let mut u = url::Url::parse(base).map_err(|_| Failure::new("供应商地址无效"))?;
    let path = u.path().trim_end_matches('/').to_owned();
    let prefix = path.strip_suffix("/v1").unwrap_or(&path);
    let sub = if path.ends_with("/v1") {
        format!("{path}/usage")
    } else {
        format!("{path}/v1/usage")
    };
    u.set_path(&sub);
    let a = u.to_string();
    u.set_path(&format!("{prefix}/api/usage/token/"));
    let b = u.to_string();
    u.set_path(&format!("{prefix}/api/status"));
    Ok((a, b, u.to_string()))
}
#[derive(Default)]
struct Data {
    source: &'static str,
    plans: Vec<Plan>,
    key_status: Option<String>,
    expires_at: Option<String>,
    expires_at_unix: Option<i64>,
    today: Option<Usage>,
    total_usage: Option<Usage>,
}
fn number(v: &Value) -> Option<f64> {
    v.as_f64()
        .or_else(|| v.as_str()?.parse().ok())
        .filter(|x| x.is_finite())
}
fn n(v: &Value, k: &str) -> Option<f64> {
    number(&v[k])
}
fn string(v: &Value, k: &str) -> Option<String> {
    v[k].as_str()
        .filter(|s| !s.is_empty())
        .map(|s| s.chars().filter(|c| !c.is_control()).take(120).collect())
}
fn plan(
    name: &str,
    remaining: Option<f64>,
    used: Option<f64>,
    total: Option<f64>,
    unit: &str,
    unlimited: bool,
) -> Plan {
    Plan {
        name: name.into(),
        remaining: if unlimited { None } else { remaining },
        used,
        total,
        unit: unit.into(),
        unlimited,
        reset_at: None,
    }
}
fn usage(v: &Value) -> Option<Usage> {
    if !v.is_object() {
        return None;
    }
    let u = Usage {
        requests: n(v, "requests"),
        tokens: n(v, "total_tokens"),
        cost: n(v, "actual_cost").or_else(|| n(v, "cost")),
    };
    (u.requests.is_some() || u.tokens.is_some() || u.cost.is_some()).then_some(u)
}
fn sub2(v: &Value) -> Option<Data> {
    if !v.is_object() || v.get("error").is_some() {
        return None;
    }
    let status = v["status"].as_str().unwrap_or("");
    let known_status = matches!(
        status,
        "active" | "disabled" | "expired" | "quota_exhausted"
    );
    if !(v["subscription"].is_object()
        || v["quota"].is_object()
        || v["rate_limits"].is_array()
        || n(v, "remaining").is_some()
        || n(v, "balance").is_some()
        || (known_status && v["isValid"].is_boolean()))
    {
        return None;
    }
    let unit = string(v, "unit").unwrap_or_else(|| "USD".into());
    let mut d = Data {
        source: "sub2api",
        expires_at: string(v, "expires_at"),
        today: usage(&v["usage"]["today"]),
        total_usage: usage(&v["usage"]["total"]),
        ..Default::default()
    };
    d.key_status = match status {
        "disabled" => Some("已禁用"),
        "expired" => Some("已过期"),
        "quota_exhausted" => Some("额度用尽"),
        _ if v["isValid"] == false => Some("Key 无效"),
        _ => None,
    }
    .map(str::to_owned);
    if v["subscription"].is_object() {
        let sub = &v["subscription"];
        let mut windows = vec![];
        for (label, u, l) in [
            ("每日", "daily_usage_usd", "daily_limit_usd"),
            ("每周", "weekly_usage_usd", "weekly_limit_usd"),
            ("每月", "monthly_usage_usd", "monthly_limit_usd"),
        ] {
            if let Some(limit) = n(sub, l).filter(|x| *x > 0.) {
                let used = n(sub, u).unwrap_or(0.);
                windows.push(plan(
                    label,
                    Some((limit - used).max(0.)),
                    Some(used),
                    Some(limit),
                    &unit,
                    false,
                ));
            }
        }
        let unlimited = n(v, "remaining").is_some_and(|n| n < 0.);
        let worst = windows.iter().max_by(|a, b| {
            (a.used.unwrap_or(0.) / a.total.unwrap())
                .total_cmp(&(b.used.unwrap_or(0.) / b.total.unwrap()))
        });
        let remaining = n(v, "remaining")
            .or_else(|| windows.iter().filter_map(|p| p.remaining).reduce(f64::min));
        d.plans.push(plan(
            &string(v, "planName").unwrap_or_else(|| "订阅".into()),
            remaining,
            worst.and_then(|p| p.used),
            worst.and_then(|p| p.total),
            &unit,
            unlimited,
        ));
        d.plans.extend(windows);
    } else if v["quota"].is_object() || v["rate_limits"].is_array() {
        if v["quota"].is_object() {
            let q = &v["quota"];
            let total = n(q, "limit");
            let used = n(q, "used");
            let remaining = n(q, "remaining").or_else(|| Some((total? - used?).max(0.)));
            d.plans.push(plan(
                "API Key 配额",
                remaining,
                used,
                total,
                &string(q, "unit").unwrap_or_else(|| unit.clone()),
                total.is_some_and(|n| n <= 0.),
            ));
        }
        if let Some(rates) = v["rate_limits"].as_array() {
            let mut rates: Vec<_> = rates.iter().filter(|r| r.is_object()).collect();
            rates.sort_by_key(|r| match r["window"].as_str() {
                Some("5h") => 0,
                Some("1d") => 1,
                Some("7d") => 2,
                _ => 3,
            });
            for r in rates {
                let label = match r["window"].as_str() {
                    Some("5h") => "5 小时",
                    Some("1d") => "1 天",
                    Some("7d") => "7 天",
                    _ => "用量窗口",
                };
                let used = n(r, "used").unwrap_or(0.);
                let total = n(r, "limit");
                let remaining = n(r, "remaining").or_else(|| total.map(|t| (t - used).max(0.)));
                let mut p = plan(
                    label,
                    remaining,
                    Some(used),
                    total,
                    &string(r, "unit").unwrap_or_else(|| unit.clone()),
                    false,
                );
                p.reset_at = string(r, "reset_at");
                d.plans.push(p);
            }
        }
    } else if let Some(amount) = n(v, "remaining").or_else(|| n(v, "balance")) {
        d.plans.push(plan(
            "钱包余额",
            Some(amount),
            None,
            None,
            &unit,
            amount < 0.,
        ));
    }
    Some(d)
}
fn newapi(v: &Value) -> Option<Data> {
    let body = &v["data"];
    if !(v["code"] == true || v["success"] == true) || body["object"] != "token_usage" {
        return None;
    }
    let unlimited = body["unlimited_quota"] == true;
    let remaining = n(body, "total_available");
    let used = n(body, "total_used");
    let total = n(body, "total_granted");
    if !unlimited && remaining.is_none() {
        return None;
    }
    let expires = body["expires_at"].as_i64().filter(|t| *t > 0);
    Some(Data {
        source: "newapi",
        plans: vec![plan(
            "Key 额度",
            remaining,
            used,
            total,
            "额度单位",
            unlimited,
        )],
        expires_at_unix: expires,
        key_status: expires
            .filter(|t| *t < now() as i64)
            .map(|_| "已过期".into()),
        ..Default::default()
    })
}
fn convert(d: &mut Data, status: &Value) {
    let s = &status["data"];
    let Some(per) = n(s, "quota_per_unit").filter(|n| *n > 0.) else {
        return;
    };
    let kind = s["quota_display_type"]
        .as_str()
        .or_else(|| (s["display_in_currency"] == true).then_some("USD"));
    let (unit, rate) = match kind {
        Some("USD") => ("USD".to_string(), Some(1.)),
        Some("CNY") => ("CNY".to_string(), n(s, "usd_exchange_rate")),
        Some("CUSTOM") => (
            string(s, "custom_currency_symbol").unwrap_or_else(|| "自定义单位".into()),
            n(s, "custom_currency_exchange_rate"),
        ),
        _ => return,
    };
    let Some(rate) = rate.filter(|r| *r > 0.) else {
        return;
    };
    for p in &mut d.plans {
        p.unit = unit.clone();
        p.remaining = p.remaining.map(|n| n / per * rate);
        p.used = p.used.map(|n| n / per * rate);
        p.total = p.total.map(|n| n / per * rate);
    }
}
async fn fetch(input: &Query) -> std::result::Result<Option<Data>, Failure> {
    let (sub, new, status) = urls(&input.base)?;
    let first = get(input, sub, true).await?;
    if (200..300).contains(&first.status) || [401, 403].contains(&first.status) {
        if let Some(data) = first.body.as_ref().and_then(sub2) {
            return Ok(Some(data));
        }
    }
    let second = get(input, new, true).await?;
    if (200..300).contains(&second.status) {
        if let Some(mut data) = second.body.as_ref().and_then(newapi) {
            if let Ok(response) = get(input, status, false).await {
                if response.status == 200 {
                    if let Some(body) = response.body {
                        convert(&mut data, &body);
                    }
                }
            }
            return Ok(Some(data));
        }
    }
    if [first.status, second.status]
        .iter()
        .any(|s| [401, 403].contains(s))
    {
        return Err(Failure::new("额度接口认证失败，请检查 Key 或访问权限"));
    }
    if [first.status, second.status]
        .iter()
        .any(|s| *s >= 500 && *s != 501)
    {
        return Err(Failure::new("额度服务暂时不可用"));
    }
    if second
        .body
        .as_ref()
        .is_some_and(|v| v["success"] == false || v["code"] == false)
    {
        return Err(Failure::new("额度接口拒绝查询，请检查 Key 状态"));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn protocol_urls_preserve_deployment_prefix() {
        for base in [
            "https://x.test/site",
            "https://x.test/site/v1",
            "https://x.test/site/v1/",
        ] {
            assert_eq!(
                urls(base).unwrap_or_else(|_| panic!("url")),
                (
                    "https://x.test/site/v1/usage".into(),
                    "https://x.test/site/api/usage/token/".into(),
                    "https://x.test/site/api/status".into()
                )
            );
        }
    }
    #[test]
    fn sub2_wallet_subscription_key_and_rate_windows() {
        let wallet=sub2(&json!({"isValid":true,"mode":"balance","balance":3.25,"usage":{"today":{"requests":3,"actual_cost":0.15,"total_tokens":12000}}})).unwrap();
        assert_eq!(wallet.plans[0].remaining, Some(3.25));
        assert_eq!(wallet.today.unwrap().cost, Some(0.15));
        let subscription=sub2(&json!({"remaining":5,"subscription":{"daily_limit_usd":10,"daily_usage_usd":5,"weekly_limit_usd":100,"weekly_usage_usd":30,"monthly_limit_usd":300,"monthly_usage_usd":60}})).unwrap();
        assert_eq!(subscription.plans.len(), 4);
        assert_eq!(subscription.plans[0].used, Some(5.));
        let key=sub2(&json!({"quota":{"limit":20,"used":7.5},"rate_limits":[{"window":"7d","limit":100,"used":12},{"window":"5h","limit":10,"used":8,"reset_at":"2026-09-28T12:00:00Z"}]})).unwrap();
        assert_eq!(key.plans[0].remaining, Some(12.5));
        assert_eq!(key.plans[1].name, "5 小时");
        assert!(key.plans[1].reset_at.is_some());
        for v in [
            json!({"remaining":-1}),
            json!({"subscription":{},"remaining":-1}),
            json!({"quota":{"limit":0}}),
        ] {
            assert!(sub2(&v).unwrap().plans[0].unlimited);
        }
        let empty = sub2(&json!({"quota":{"limit":10,"used":10,"remaining":0}})).unwrap();
        assert_eq!(empty.plans[0].remaining, Some(0.));
    }
    #[test]
    fn invalid_key_state_is_recognized_without_fabricating_balance() {
        let data = sub2(&json!({"isValid":false,"status":"disabled"})).unwrap();
        assert_eq!(data.key_status.as_deref(), Some("已禁用"));
        assert!(data.plans.is_empty());
        for v in [
            json!({}),
            json!({"data":{"balance":50}}),
            json!({"error":{"message":"secret"}}),
            json!("html"),
        ] {
            assert!(sub2(&v).is_none());
        }
    }
    #[test]
    fn newapi_units_expiry_and_unlimited_are_not_assumed() {
        let body = json!({"code":true,"data":{"object":"token_usage","total_available":1000,"total_used":500,"total_granted":1500,"unlimited_quota":false,"expires_at":1}});
        let d = newapi(&body).unwrap();
        assert_eq!(d.key_status.as_deref(), Some("已过期"));
        assert_eq!(d.plans[0].unit, "额度单位");
        for (kind, unit, remaining) in [
            ("USD", "USD", 10.),
            ("CNY", "CNY", 70.),
            ("CUSTOM", "点", 20.),
            ("TOKENS", "额度单位", 1000.),
        ] {
            let mut d = newapi(&body).unwrap();
            convert(
                &mut d,
                &json!({"data":{"quota_per_unit":100,"quota_display_type":kind,"usd_exchange_rate":7,"custom_currency_symbol":"点","custom_currency_exchange_rate":2}}),
            );
            assert_eq!(d.plans[0].unit, unit);
            assert_eq!(d.plans[0].remaining, Some(remaining));
        }
        let mut d = newapi(&body).unwrap();
        convert(
            &mut d,
            &json!({"data":{"quota_per_unit":0,"quota_display_type":"USD"}}),
        );
        assert_eq!(d.plans[0].unit, "额度单位");
        assert!(
            newapi(&json!({"success":true,"data":{"object":"token_usage","unlimited_quota":true}}))
                .unwrap()
                .plans[0]
                .unlimited
        );
        assert!(newapi(
            &json!({"success":false,"data":{"object":"token_usage","total_available":10}})
        )
        .is_none());
    }
}
