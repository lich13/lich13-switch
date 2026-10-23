use crate::errors::code;
use crate::{
    gateway,
    storage::{AppError, Result},
    Runtime,
};
use serde::Serialize;
use std::{
    collections::{BTreeMap, VecDeque},
    sync::Arc,
    time::{Duration, Instant},
};
use tauri::{Emitter, Manager};

#[cfg(target_os = "macos")]
#[path = "links_macos.rs"]
mod platform;
#[cfg(windows)]
#[path = "links_windows.rs"]
mod platform;

pub const APPS: &[(&str, &str)] = &[
    ("com.lich13.gpt-switch", "lich13-switch"),
    ("com.ccswitch.desktop", "CC Switch"),
    ("com.lich13.studio", "lich13studio"),
];
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Handler {
    pub id: String,
    pub name: String,
    #[serde(skip)]
    pub path: String,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HandlerState {
    pub current: Option<String>,
    pub apps: Vec<Handler>,
    pub system_picker: bool,
    pub remembered: Option<String>,
}
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Preview {
    pub client_id: gateway::ClientId,
    pub id: String,
    pub name: String,
    pub base_url: String,
}
struct Parsed {
    preview: Preview,
    key: String,
    fingerprint: String,
}
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkPreview {
    pub id: String,
    pub provider: Option<Preview>,
    pub import_error: Option<String>,
    pub dispatch_error: Option<String>,
}
struct Pending {
    preview: LinkPreview,
    raw: String,
    fingerprint: String,
    processing: bool,
}
#[derive(Default)]
pub struct Imports {
    pending: VecDeque<Pending>,
    handled: VecDeque<(String, Instant)>,
}
#[derive(Serialize)]
pub struct Receipt {
    warning: Option<String>,
}
fn invalid(message: &str) -> AppError {
    AppError::new(code::IMPORT_LINK, message)
}
fn parse(raw: &str) -> Result<Parsed> {
    if raw.len() > 64 * 1024 {
        return Err(invalid("导入链接过长"));
    }
    let url = url::Url::parse(raw).map_err(|_| invalid("无效的导入链接"))?;
    if url.scheme() != "ccswitch"
        || url.host_str() != Some("v1")
        || url.path() != "/import"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
        || url.fragment().is_some()
    {
        return Err(invalid("仅支持 CCS v1 供应商导入链接"));
    }
    let mut fields = BTreeMap::new();
    for (k, v) in url.query_pairs() {
        if fields.insert(k.to_string(), v.to_string()).is_some() {
            return Err(invalid("导入链接含重复字段"));
        }
    }
    if fields.get("resource").map(String::as_str) != Some("provider")
        || !matches!(
            fields.get("app").map(String::as_str),
            Some("codex" | "claude")
        )
    {
        return Err(invalid("仅支持 Codex 或 Claude Code 供应商导入"));
    }
    let client_id = if fields.get("app").map(String::as_str) == Some("claude") {
        gateway::ClientId::Claude
    } else {
        gateway::ClientId::Codex
    };
    let base = fields
        .get("endpoint")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| invalid("缺少供应商地址"))?;
    let endpoint = url::Url::parse(base).map_err(|_| invalid("无效的供应商地址"))?;
    if !matches!(endpoint.scheme(), "https" | "http")
        || endpoint.host_str().is_none()
        || !endpoint.username().is_empty()
        || endpoint.password().is_some()
        || endpoint.query().is_some()
        || endpoint.fragment().is_some()
    {
        return Err(invalid("无效的供应商地址"));
    }
    let key = fields
        .get("apiKey")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty() && !s.chars().any(char::is_control))
        .ok_or_else(|| invalid("缺少或无效的 API Key"))?
        .to_owned();
    let name = fields
        .get("name")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .unwrap_or(endpoint.host_str().unwrap());
    if name.chars().count() > 120 || name.chars().any(char::is_control) {
        return Err(invalid("无效的供应商名称"));
    }
    let fingerprint = crate::storage::digest(
        format!(
            "{client_id:?}\0{}\0{key}",
            endpoint.as_str().trim_end_matches('/')
        )
        .as_bytes(),
    );
    Ok(Parsed {
        preview: Preview {
            client_id,
            id: uuid::Uuid::new_v4().to_string(),
            name: name.to_owned(),
            base_url: base.to_owned(),
        },
        key,
        fingerprint,
    })
}
impl Imports {
    fn enqueue(&mut self, raw: &str) -> Result<bool> {
        if raw.len() > 64 * 1024 || raw.chars().any(char::is_control) {
            return Err(invalid("导入链接过长或无效"));
        }
        let url = url::Url::parse(raw).map_err(|_| invalid("无效的导入链接"))?;
        if url.scheme() != "ccswitch" {
            return Err(invalid("仅支持 CC Switch 链接"));
        }
        let parsed = parse(raw);
        let fingerprint = parsed
            .as_ref()
            .map(|p| p.fingerprint.clone())
            .unwrap_or_else(|_| crate::storage::digest(raw.as_bytes()));
        self.handled
            .retain(|(_, when)| when.elapsed() < Duration::from_secs(3));
        if self.pending.iter().any(|v| v.fingerprint == fingerprint)
            || self.handled.iter().any(|(f, _)| f == &fingerprint)
        {
            return Ok(false);
        }
        if self.pending.len() >= 16 {
            return Err(invalid("待处理链接过多，请先处理已有请求"));
        }
        let id = uuid::Uuid::new_v4().to_string();
        self.pending.push_back(Pending {
            preview: LinkPreview {
                id,
                provider: parsed.as_ref().ok().map(|p| p.preview.clone()),
                import_error: parsed.err().map(|e| e.message),
                dispatch_error: None,
            },
            raw: raw.to_owned(),
            fingerprint,
            processing: false,
        });
        Ok(true)
    }
    fn claim(&mut self, id: &str) -> Result<String> {
        let p = self
            .pending
            .iter_mut()
            .find(|p| p.preview.id == id)
            .ok_or_else(|| invalid("链接已处理"))?;
        if p.processing {
            return Err(invalid("链接正在处理"));
        }
        p.processing = true;
        Ok(p.raw.clone())
    }
    fn release(&mut self, id: &str, error: Option<String>) {
        if let Some(p) = self.pending.iter_mut().find(|p| p.preview.id == id) {
            p.processing = false;
            p.preview.dispatch_error = error;
        }
    }
    fn remove(&mut self, id: &str) {
        if let Some(i) = self.pending.iter().position(|p| p.preview.id == id) {
            let p = self.pending.remove(i).unwrap();
            self.handled.push_back((p.fingerprint, Instant::now()));
            if self.handled.len() > 64 {
                self.handled.pop_front();
            }
        }
    }
}
pub fn receive(app: &tauri::AppHandle, urls: impl IntoIterator<Item = String>) {
    let Some(r) = app.try_state::<Arc<Runtime>>() else {
        return;
    };
    type Forward = (String, Option<String>, Option<String>);
    for url in urls {
        if !url.starts_with("ccswitch:") {
            continue;
        }
        let pending: Result<Option<Forward>> = (|| {
            let remembered = crate::lock(&r.core)?.link_receiver();
            let mut queue = crate::lock(&r.imports)?;
            if !queue.enqueue(&url)? {
                return Ok(None);
            }
            if remembered.as_deref().is_some_and(|id| id != APPS[0].0) {
                let id = queue.pending.back().unwrap().preview.id.clone();
                let raw = queue.claim(&id)?;
                return Ok(Some((id, Some(raw), remembered)));
            }
            Ok(Some((String::new(), None, remembered)))
        })();
        match pending {
            Ok(Some((id, Some(raw), Some(target)))) => {
                let app = app.clone();
                let runtime = r.inner().clone();
                tauri::async_runtime::spawn(async move {
                    let result: Result<bool> = async {
                        open_external(&app, &target, raw).await?;
                        Ok(true)
                    }
                    .await;
                    if let Ok(mut queue) = crate::lock(&runtime.imports) {
                        match &result {
                            Ok(true) => queue.remove(&id),
                            _ => {
                                queue.release(&id, result.as_ref().err().map(|e| e.message.clone()))
                            }
                        }
                    }
                    let _ = app.emit("provider-imports", ());
                    if !matches!(result, Ok(true)) {
                        let _ = crate::show(&app, None);
                    }
                });
            }
            Ok(None) => {}
            Ok(Some((_, None, _))) => {
                let _ = app.emit("provider-imports", ());
                let _ = crate::show(app, None);
            }
            Ok(Some(_)) => {}
            Err(error) => {
                let _ = app.emit("switch-error", error);
                let _ = crate::show(app, None);
            }
        }
    }
}
fn main_only(window: &tauri::WebviewWindow) -> Result<()> {
    if window.label() == "main" {
        Ok(())
    } else {
        Err(invalid("请在主窗口处理导入"))
    }
}
#[tauri::command]
pub fn get_provider_imports(
    window: tauri::WebviewWindow,
    r: tauri::State<'_, Arc<Runtime>>,
) -> Result<Vec<LinkPreview>> {
    main_only(&window)?;
    Ok(crate::lock(&r.imports)?
        .pending
        .iter()
        .filter(|p| !p.processing)
        .map(|p| p.preview.clone())
        .collect())
}
#[tauri::command]
pub fn cancel_provider_import(
    window: tauri::WebviewWindow,
    r: tauri::State<'_, Arc<Runtime>>,
    id: String,
) -> Result<()> {
    main_only(&window)?;
    let mut queue = crate::lock(&r.imports)?;
    queue.claim(&id)?;
    queue.remove(&id);
    let _ = window.emit("provider-imports", ());
    Ok(())
}
async fn open_external(app: &tauri::AppHandle, id: &str, raw: String) -> Result<()> {
    if id == APPS[0].0 || !APPS.iter().any(|(known, _)| *known == id) {
        return Err(invalid("无效的接收应用"));
    }
    let id = id.to_owned();
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.run_on_main_thread(move || platform::open(&id, &raw, tx))
        .map_err(|_| invalid("无法打开接收应用"))?;
    rx.await.map_err(|_| invalid("打开接收应用中断"))?
}
#[tauri::command]
pub async fn confirm_provider_import(
    window: tauri::WebviewWindow,
    r: tauri::State<'_, Arc<Runtime>>,
    id: String,
    app_id: String,
    remember: bool,
    expected_revision: Option<String>,
) -> Result<Receipt> {
    main_only(&window)?;
    let raw = crate::lock(&r.imports)?.claim(&id)?;
    let outcome = if app_id == APPS[0].0 {
        (|| {
            let p = parse(&raw)?;
            let home = r.home(p.preview.client_id)?;
            r.gateway(p.preview.client_id).edit(
                gateway::Edit::ImportLink {
                    base_url: p.preview.base_url,
                    token: p.key,
                    display_name: p.preview.name,
                },
                expected_revision
                    .as_deref()
                    .ok_or_else(|| invalid("请重新确认供应商信息"))?,
                &home,
            )?;
            Ok(())
        })()
    } else {
        open_external(window.app_handle(), &app_id, raw).await
    };
    if let Err(error) = outcome {
        crate::lock(&r.imports)?.release(&id, Some(error.message.clone()));
        return Err(error);
    }
    // The link is already delivered. A preference failure must never replay it.
    crate::lock(&r.imports)?.remove(&id);
    let warning = remember
        .then(|| crate::lock(&r.core).and_then(|mut c| c.set_link_receiver(Some(app_id))))
        .and_then(|result| result.err())
        .map(|_| "本次处理已完成，但未能记住接收应用，请在设置中重试".to_owned());
    let _ = window.emit("provider-imports", ());
    Ok(Receipt { warning })
}
#[tauri::command]
pub async fn get_link_handler_state(app: tauri::AppHandle) -> Result<HandlerState> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.run_on_main_thread(move || {
        let _ = tx.send(platform::state());
    })
    .map_err(|_| invalid("无法查询默认应用"))?;
    let mut state = rx.await.map_err(|_| invalid("无法查询默认应用"))??;
    if !state.apps.iter().any(|a| a.id == APPS[0].0) {
        state.apps.insert(
            0,
            Handler {
                id: APPS[0].0.into(),
                name: APPS[0].1.into(),
                path: String::new(),
            },
        );
    }
    state.remembered = crate::lock(&app.state::<Arc<Runtime>>().core)?.link_receiver();
    Ok(state)
}
#[tauri::command]
pub async fn set_link_receiver(
    window: tauri::WebviewWindow,
    app_id: Option<String>,
) -> Result<HandlerState> {
    main_only(&window)?;
    let app = window.app_handle();
    let mut state = get_link_handler_state(app.clone()).await?;
    if app_id
        .as_ref()
        .is_some_and(|id| !state.apps.iter().any(|a| a.id == *id))
    {
        return Err(invalid("应用未安装或不支持 CC Switch 链接"));
    }
    crate::lock(&app.state::<Arc<Runtime>>().core)?.set_link_receiver(app_id.clone())?;
    state.remembered = app_id;
    Ok(state)
}
#[tauri::command]
pub async fn enable_link_picker(window: tauri::WebviewWindow) -> Result<HandlerState> {
    main_only(&window)?;
    let app = window.app_handle();
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.run_on_main_thread(move || platform::set(APPS[0].0, tx))
        .map_err(|_| invalid("无法启用链接选择"))?;
    rx.await.map_err(|_| invalid("默认应用设置中断"))??;
    get_link_handler_state(app.clone()).await
}
#[cfg(not(any(target_os = "macos", windows)))]
mod platform {
    use super::*;
    pub fn state() -> Result<HandlerState> {
        Ok(HandlerState {
            current: None,
            apps: vec![],
            system_picker: true,
            remembered: None,
        })
    }
    pub fn set(_: &str, tx: tokio::sync::oneshot::Sender<Result<()>>) {
        let _ = tx.send(Err(invalid("当前平台不支持协议关联")));
    }
    pub fn open(_: &str, _: &str, tx: tokio::sync::oneshot::Sender<Result<()>>) {
        let _ = tx.send(Err(invalid("当前平台不支持打开接收应用")));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const URL:&str="ccswitch://v1/import?resource=provider&app=codex&name=Fixture&endpoint=https%3A%2F%2Fexample.invalid%2Fsite%2Fv1&apiKey=fixture-only&model=ignored";
    #[test]
    fn client_identity_is_part_of_import_deduplication() {
        let codex = parse("ccswitch://v1/import?resource=provider&app=codex&endpoint=https%3A%2F%2Fexample.invalid%2Fsite&apiKey=fixture-only").unwrap();
        let claude = parse("ccswitch://v1/import?resource=provider&app=claude&endpoint=https%3A%2F%2Fexample.invalid%2Fsite&apiKey=fixture-only").unwrap();
        assert_eq!(claude.preview.client_id, gateway::ClientId::Claude);
        assert_eq!(claude.preview.base_url, "https://example.invalid/site");
        assert_ne!(codex.fingerprint, claude.fingerprint);
        assert!(!serde_json::to_string(&claude.preview)
            .unwrap()
            .contains("fixture-only"));
    }

    #[test]
    fn decode_redact_and_preserve_path() {
        let p = parse(URL).unwrap();
        assert_eq!(p.preview.name, "Fixture");
        assert_eq!(p.preview.base_url, "https://example.invalid/site/v1");
        assert_eq!(p.key, "fixture-only");
        assert!(!serde_json::to_string(&p.preview)
            .unwrap()
            .contains("fixture-only"));
    }

    #[test]
    fn missing_name_falls_back_to_endpoint_host() {
        let p = parse("ccswitch://v1/import?resource=provider&app=codex&endpoint=https%3A%2F%2Fapi.example.invalid%2Fv1&apiKey=fixture-only").unwrap();
        assert_eq!(p.preview.name, "api.example.invalid");
    }

    #[test]
    fn rejects_control_and_overlong_names() {
        let control = "ccswitch://v1/import?resource=provider&app=codex&name=bad%0Aname&endpoint=https%3A%2F%2Fapi.example.invalid&apiKey=fixture-only";
        assert!(parse(control).is_err());
        let long = "x".repeat(121);
        let url = format!("ccswitch://v1/import?resource=provider&app=codex&name={long}&endpoint=https%3A%2F%2Fapi.example.invalid&apiKey=fixture-only");
        assert!(parse(&url).is_err());
    }
    #[test]
    fn duplicate_delivery_cancel_and_bounded_queue() {
        let mut q = Imports::default();
        assert!(q.enqueue(URL).unwrap());
        assert!(!q.enqueue(URL).unwrap());
        let id = q.pending[0].preview.id.clone();
        q.remove(&id);
        assert!(!q.enqueue(URL).unwrap());
        assert!(q.pending.is_empty());
        q.handled[0].1 = Instant::now() - Duration::from_secs(4);
        assert!(q.enqueue(URL).unwrap());
        for n in 1..16 {
            assert!(q
                .enqueue(&URL.replace("fixture-only", &format!("fixture-{n}")))
                .unwrap());
        }
        assert!(q
            .enqueue(&URL.replace("fixture-only", "over-limit"))
            .is_err());
        assert_eq!(q.pending.len(), 16);
    }
    #[test]
    fn external_links_keep_original_bytes_without_exposing_them_in_previews() {
        let raw = URL.replace("resource=provider", "resource=skill");
        let mut queue = Imports::default();
        assert!(queue.enqueue(&raw).unwrap());
        assert!(queue.pending[0].preview.provider.is_none());
        assert!(queue.pending[0].preview.import_error.is_some());
        let id = queue.pending[0].preview.id.clone();
        assert!(!serde_json::to_string(&queue.pending[0].preview)
            .unwrap()
            .contains("fixture-only"));
        assert_eq!(queue.claim(&id).unwrap(), raw);
        assert!(queue.claim(&id).is_err());
        assert!(!queue.enqueue(&raw).unwrap());
        queue.release(&id, Some("打开失败".into()));
        assert_eq!(queue.claim(&id).unwrap(), raw);
        queue.remove(&id);
        assert!(queue.claim(&id).is_err());
        assert!(!queue.enqueue(&raw).unwrap());
    }
    #[test]
    fn handlers_do_not_publish_installation_paths() {
        let handler = Handler {
            id: APPS[1].0.into(),
            name: APPS[1].1.into(),
            path: "fixture-private-path".into(),
        };
        let value = serde_json::to_value(handler).unwrap();
        assert!(value.get("path").is_none());
    }
    #[test]
    fn invalid_resources_and_secrets_never_enter_errors() {
        for u in [
            URL.replace("app=codex", "app=unsupported"),
            URL.replace("resource=provider", "resource=skill"),
            format!("{URL}&apiKey=again"),
            URL.replace(
                "https%3A%2F%2Fexample.invalid",
                "file%3A%2F%2Fexample.invalid",
            ),
        ] {
            let e = parse(&u).err().unwrap();
            assert!(!e.message.contains("fixture-only"));
        }
    }
}
