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
    pub path: String,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HandlerState {
    pub current: Option<String>,
    pub apps: Vec<Handler>,
    pub system_picker: bool,
}
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Preview {
    pub client_id: gateway::ClientId,
    pub id: String,
    pub name: String,
    pub base_url: String,
}
struct Pending {
    preview: Preview,
    key: String,
    fingerprint: String,
}
#[derive(Default)]
pub struct Imports {
    pending: VecDeque<Pending>,
    handled: VecDeque<(String, Instant)>,
}
fn invalid(message: &str) -> AppError {
    AppError::new(code::IMPORT_LINK, message)
}
fn parse(raw: &str) -> Result<Pending> {
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
    Ok(Pending {
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
        let p = parse(raw)?;
        self.handled
            .retain(|(_, when)| when.elapsed() < Duration::from_secs(3));
        if self.pending.iter().any(|v| v.fingerprint == p.fingerprint)
            || self.handled.iter().any(|(f, _)| f == &p.fingerprint)
        {
            return Ok(false);
        }
        if self.pending.len() >= 16 {
            return Err(invalid("待导入供应商过多，请先处理已有请求"));
        }
        self.pending.push_back(p);
        Ok(true)
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
    for url in urls {
        if !url.starts_with("ccswitch:") {
            continue;
        }
        let result = r.imports.lock().unwrap().enqueue(&url);
        if let Err(e) = result {
            let _ = app.emit("switch-error", e);
        } else {
            let _ = app.emit("provider-imports", ());
        }
        let _ = crate::show(app, None);
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
) -> Result<Vec<Preview>> {
    main_only(&window)?;
    Ok(r.imports
        .lock()
        .unwrap()
        .pending
        .iter()
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
    r.imports.lock().unwrap().remove(&id);
    let _ = window.emit("provider-imports", ());
    Ok(())
}
#[tauri::command]
pub fn confirm_provider_import(
    window: tauri::WebviewWindow,
    r: tauri::State<'_, Arc<Runtime>>,
    id: String,
    expected_revision: String,
) -> Result<gateway::View> {
    main_only(&window)?;
    let mut imports = r.imports.lock().unwrap();
    let p = imports
        .pending
        .iter()
        .find(|p| p.preview.id == id)
        .ok_or_else(|| invalid("导入请求已处理"))?;
    let home = r.home(p.preview.client_id)?;
    let result = r.gateway(p.preview.client_id).edit(
        gateway::Edit::ImportLink {
            base_url: p.preview.base_url.clone(),
            token: p.key.clone(),
            display_name: p.preview.name.clone(),
        },
        &expected_revision,
        &home,
    )?;
    imports.remove(&id);
    let _ = window.emit("provider-imports", ());
    Ok(result)
}
#[tauri::command]
pub async fn get_link_handler_state(app: tauri::AppHandle) -> Result<HandlerState> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.run_on_main_thread(move || {
        let _ = tx.send(platform::state());
    })
    .map_err(|_| invalid("无法查询默认应用"))?;
    rx.await.map_err(|_| invalid("无法查询默认应用"))?
}
#[tauri::command]
pub async fn set_link_handler(app: tauri::AppHandle, app_id: String) -> Result<HandlerState> {
    if !APPS.iter().any(|(id, _)| *id == app_id) {
        return Err(invalid("无效的接收应用"));
    }
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.run_on_main_thread(move || platform::set(&app_id, tx))
        .map_err(|_| invalid("无法设置默认应用"))?;
    rx.await.map_err(|_| invalid("默认应用设置中断"))??;
    get_link_handler_state(app).await
}
#[cfg(not(any(target_os = "macos", windows)))]
mod platform {
    use super::*;
    pub fn state() -> Result<HandlerState> {
        Ok(HandlerState {
            current: None,
            apps: vec![],
            system_picker: true,
        })
    }
    pub fn set(_: &str, tx: tokio::sync::oneshot::Sender<Result<()>>) {
        let _ = tx.send(Err(invalid("当前平台不支持协议关联")));
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
