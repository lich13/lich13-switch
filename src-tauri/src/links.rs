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

#[cfg(target_os = "macos")]
pub const APP_ID: &str = "com.lich13.gpt-switch";
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
    pub error: Option<String>,
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
                error: None,
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
            p.preview.error = error;
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
    for url in urls.into_iter().filter(|url| url.starts_with("ccswitch:")) {
        let outcome = crate::lock(&r.imports).and_then(|mut queue| queue.enqueue(&url));
        match outcome {
            Ok(true) => {
                let _ = app.emit("provider-imports", ());
                let _ = crate::show(app, None);
            }
            Ok(false) => {}
            Err(error) => {
                let _ = app.emit("switch-error", error);
                let _ = crate::show(app, None);
            }
        }
    }
}
/// Only packaged, installed instances may change the system association.
pub async fn ensure_association(app: &tauri::AppHandle) -> Result<()> {
    if cfg!(any(test, debug_assertions)) || !platform::installed() {
        return Ok(());
    }
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.run_on_main_thread(move || platform::ensure(tx))
        .map_err(|_| invalid("无法设置 CC Switch 链接关联"))?;
    rx.await
        .map_err(|_| invalid("CC Switch 链接关联设置中断，请重启应用重试"))?
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
#[tauri::command]
pub fn confirm_provider_import(
    window: tauri::WebviewWindow,
    r: tauri::State<'_, Arc<Runtime>>,
    id: String,
    expected_revision: String,
) -> Result<()> {
    main_only(&window)?;
    let raw = crate::lock(&r.imports)?.claim(&id)?;
    let outcome: Result<()> = (|| {
        let p = parse(&raw)?;
        let home = r.home(p.preview.client_id)?;
        r.gateway(p.preview.client_id).edit(
            gateway::Edit::ImportLink {
                base_url: p.preview.base_url,
                token: p.key,
                display_name: p.preview.name,
            },
            &expected_revision,
            &home,
        )?;
        Ok(())
    })();
    if let Err(error) = outcome {
        crate::lock(&r.imports)?.release(&id, Some(error.message.clone()));
        return Err(error);
    }
    crate::lock(&r.imports)?.remove(&id);
    let _ = window.emit("provider-imports", ());
    Ok(())
}
#[cfg(not(any(target_os = "macos", windows)))]
mod platform {
    use super::*;
    pub fn installed() -> bool {
        false
    }
    pub fn ensure(tx: tokio::sync::oneshot::Sender<Result<()>>) {
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
    fn unsupported_links_show_an_error_without_exposing_secrets() {
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
