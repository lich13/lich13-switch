#[cfg(not(any(target_os = "macos", windows)))]
compile_error!("Vela supports only macOS and Windows");

mod antigravity;
use crate::errors::code;
mod auth_cache;
mod claude_profile;
mod cleanup_spool;
mod commands;
mod configuration;
mod core;
mod errors;
mod event_raw;
mod events;
mod exchange_template;
#[cfg(test)]
mod exchange_template_tests;
mod gateway;
mod links;
mod login;
mod migrations;
mod models_dev;
mod notifications;
mod official;
mod power;
#[cfg(target_os = "macos")]
mod power_macos;
mod process_control;
mod provider_testing;
mod quick;
mod smoke;
mod startup;
#[cfg(target_os = "macos")]
mod startup_macos;
mod storage;
#[cfg(target_os = "macos")]
mod tray_macos;
mod update;
mod usage;
use core::{Core, Preferences, ViewState};
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};
use storage::{AppError, Result};
use tauri::{
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    Emitter, Manager,
};

struct Runtime {
    diagnostics: events::Service,
    usage: usage::Service,
    core: Mutex<Core>,
    data: PathBuf,
    imports: Mutex<links::Imports>,
    provider_tests: provider_testing::Service,
    gateway: gateway::Gateway,
    claude: gateway::Gateway,
    grok: gateway::Gateway,
    dsh: gateway::Gateway,
    login: Mutex<login::Session>,
    claude_login: Mutex<claude_profile::login::Session>,
    quitting: AtomicBool,
    quit_pending: AtomicBool,
    smoke: Option<PathBuf>,
    smoke_result: Mutex<Option<Result<smoke::SmokeSnapshot>>>,
    fixture: Mutex<Option<tempfile::TempDir>>,
    startup: startup::Service,
    start_silently: bool,
    frontend_started: AtomicBool,
    force_quitting: AtomicBool,
    power: power::Service,
    startup_error: Mutex<Option<AppError>>,
    official_tx: tokio::sync::Mutex<()>,
    window_wake: tokio::sync::Notify,
    window_visible: [AtomicBool; 2],
    pending_main: PendingMainEvents,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MainSignal {
    ProviderTests,
    PricingState,
    UsageState,
}
impl MainSignal {
    pub(crate) const fn event_name(self) -> &'static str {
        match self {
            Self::ProviderTests => "provider-tests",
            Self::PricingState => "pricing-state",
            Self::UsageState => "usage-state",
        }
    }
}

pub(crate) struct DrainedMainEvents {
    app_event: Option<events::Change>,
    notification_state: Option<notifications::State>,
    provider_tests: bool,
    pricing_state: bool,
    usage_state: bool,
    config_states: Vec<serde_json::Value>,
}

#[derive(Default)]
pub(crate) struct PendingMainEvents {
    app_event: Mutex<Option<events::Change>>,
    notification_state: Mutex<Option<notifications::State>>,
    config_states: Mutex<std::collections::BTreeMap<String, serde_json::Value>>,
    provider_tests: AtomicBool,
    pricing_state: AtomicBool,
    usage_state: AtomicBool,
}
impl PendingMainEvents {
    fn record_signal(&self, signal: MainSignal) {
        match signal {
            MainSignal::ProviderTests => self.provider_tests.store(true, Ordering::Relaxed),
            MainSignal::PricingState => self.pricing_state.store(true, Ordering::Relaxed),
            MainSignal::UsageState => self.usage_state.store(true, Ordering::Relaxed),
        }
    }
    fn record_app_event(&self, change: &events::Change) {
        if let Ok(mut slot) = self.app_event.lock() {
            let previous_record = slot.as_ref().and_then(|old| old.record.clone());
            *slot = Some(events::Change {
                record: change.record.clone().or(previous_record),
                error: change.error.clone(),
                notify: false,
            });
        }
    }
    fn record_notification_state(&self, state: notifications::State) {
        if let Ok(mut slot) = self.notification_state.lock() {
            *slot = Some(state);
        }
    }
    fn record_config_state(&self, key: &str, payload: serde_json::Value) {
        if let Ok(mut map) = self.config_states.lock() {
            map.insert(key.to_owned(), payload);
        }
    }
    fn drain(&self) -> DrainedMainEvents {
        let app_event = self.app_event.lock().ok().and_then(|mut s| s.take());
        let notification_state = self
            .notification_state
            .lock()
            .ok()
            .and_then(|mut s| s.take());
        let provider_tests = self.provider_tests.swap(false, Ordering::Relaxed);
        let pricing_state = self.pricing_state.swap(false, Ordering::Relaxed);
        let usage_state = self.usage_state.swap(false, Ordering::Relaxed);
        let config_states = self
            .config_states
            .lock()
            .ok()
            .map(|mut map| std::mem::take(&mut *map).into_values().collect())
            .unwrap_or_default();
        DrainedMainEvents {
            app_event,
            notification_state,
            provider_tests,
            pricing_state,
            usage_state,
            config_states,
        }
    }
    fn drain_for_window(&self, label: &str) -> Option<DrainedMainEvents> {
        (label == "main").then(|| self.drain())
    }
}
impl Runtime {
    pub(crate) fn mark_window_visible(&self, label: &str, visible: bool) -> bool {
        let slot = match label {
            "main" => &self.window_visible[0],
            "quick" => &self.window_visible[1],
            _ => return visible,
        };
        let was_visible = slot.swap(visible, Ordering::Relaxed);
        if visible && !was_visible {
            self.window_wake.notify_one();
        }
        was_visible
    }

    fn background_poll_active(&self) -> bool {
        [&self.gateway, &self.claude, &self.grok, &self.dsh]
            .into_iter()
            .any(gateway::Gateway::is_running)
            || self
                .login
                .lock()
                .map(|s| login_active(&s.state))
                .unwrap_or(false)
            || self
                .claude_login
                .lock()
                .map(|s| matches!(s.state.phase.as_str(), "waiting" | "cancelling"))
                .unwrap_or(false)
    }

    fn config_failure(&self, client: gateway::ClientId, error: AppError) -> AppError {
        if let Some(record) = events::Record::configuration_failure(client, &error, false) {
            self.diagnostics.emit(record);
        }
        error
    }

    fn gateway(&self, client: gateway::ClientId) -> &gateway::Gateway {
        match client {
            gateway::ClientId::Codex => &self.gateway,
            gateway::ClientId::Claude => &self.claude,
            gateway::ClientId::Grok => &self.grok,
            gateway::ClientId::Dsh => &self.dsh,
        }
    }
    fn home(&self, client: gateway::ClientId) -> Result<PathBuf> {
        let core = self.core.lock().unwrap();
        Ok(match client {
            gateway::ClientId::Codex => core.home(),
            gateway::ClientId::Claude => PathBuf::from(core.preferences().claude_home),
            gateway::ClientId::Grok => PathBuf::from(core.preferences().grok_home),
            gateway::ClientId::Dsh => PathBuf::from(core.preferences().dsh_home),
        })
    }
    async fn stop_gateways(&self) -> Result<()> {
        let _official_guard = self.official_tx.lock().await;
        let a = self.gateway.stop_for_exit().await;
        let b = self.claude.stop_for_exit().await;
        let c = self.grok.stop_for_exit().await;
        let d = self.dsh.stop_for_exit().await;
        let registry = self.gateway.registry();
        let _listener = registry.lifecycle.lock().await;
        let e = gateway::antigravity::stop(&registry).await;
        a.and(b).and(c).and(d).and(e).map(|_| ())
    }
}

#[cfg(target_os = "macos")]
fn application_menu(app: &tauri::AppHandle) -> tauri::Result<Menu<tauri::Wry>> {
    // The native predefined Quit calls NSApplication.terminate directly.
    // Use our command so configuration restoration can finish (or report a conflict).
    let menu = Menu::default(app)?;
    menu.remove_at(0)?;
    let application = tauri::menu::Submenu::with_items(
        app,
        "Vela",
        true,
        &[
            &PredefinedMenuItem::about(app, Some("关于 Vela"), None)?,
            &PredefinedMenuItem::separator(app)?,
            &PredefinedMenuItem::services(app, None)?,
            &PredefinedMenuItem::separator(app)?,
            &PredefinedMenuItem::hide(app, None)?,
            &PredefinedMenuItem::hide_others(app, None)?,
            &PredefinedMenuItem::separator(app)?,
            &MenuItem::with_id(app, "app-quit", "退出 Vela", true, Some("CmdOrCtrl+Q"))?,
        ],
    )?;
    menu.insert(&application, 0)?;
    Ok(menu)
}
fn login_active(s: &login::LoginState) -> bool {
    ["starting", "waiting", "cancelling"].contains(&s.phase.as_str())
}
pub(crate) fn visible_windows(app: &tauri::AppHandle) -> Vec<tauri::WebviewWindow> {
    ["main", "quick"]
        .into_iter()
        .filter_map(|name| app.get_webview_window(name))
        .filter(|w| w.is_visible().unwrap_or(false) && !w.is_minimized().unwrap_or(false))
        .collect()
}
pub(crate) fn main_visible_window(app: &tauri::AppHandle) -> Option<tauri::WebviewWindow> {
    app.get_webview_window("main")
        .filter(|w| w.is_visible().unwrap_or(false) && !w.is_minimized().unwrap_or(false))
}
pub(crate) fn emit_if_main_visible<S: serde::Serialize + Clone>(
    app: &tauri::AppHandle,
    signal: MainSignal,
    payload: S,
) {
    if let Some(window) = main_visible_window(app) {
        let _ = window.emit(signal.event_name(), payload);
    } else if let Some(r) = app.try_state::<Arc<Runtime>>() {
        r.pending_main.record_signal(signal);
    }
}
pub(crate) fn emit_main_app_event(app: &tauri::AppHandle, change: &events::Change) {
    if let Some(window) = main_visible_window(app) {
        let _ = window.emit("app-event", change);
    } else if let Some(r) = app.try_state::<Arc<Runtime>>() {
        r.pending_main.record_app_event(change);
    }
}
pub(crate) fn emit_main_notification_state(app: &tauri::AppHandle, state: notifications::State) {
    if let Some(window) = main_visible_window(app) {
        let _ = window.emit("notification-state", state);
    } else if let Some(r) = app.try_state::<Arc<Runtime>>() {
        r.pending_main.record_notification_state(state);
    }
}
pub(crate) fn emit_main_config_state(
    app: &tauri::AppHandle,
    client_key: &str,
    payload: serde_json::Value,
) {
    if let Some(window) = main_visible_window(app) {
        let _ = window.emit("config-state", payload);
    } else if let Some(r) = app.try_state::<Arc<Runtime>>() {
        r.pending_main.record_config_state(client_key, payload);
    }
}
pub(crate) fn sync_window_on_show(app: &tauri::AppHandle, label: &str) {
    let Some(window) = app.get_webview_window(label) else {
        return;
    };
    let Some(r) = app.try_state::<Arc<Runtime>>() else {
        return;
    };
    let was_visible = r.mark_window_visible(label, true);
    if let Some(drained) = r.pending_main.drain_for_window(label) {
        if let Some(change) = drained.app_event {
            let _ = window.emit("app-event", &change);
        }
        if let Some(state) = drained.notification_state {
            let _ = window.emit("notification-state", state);
        }
        if drained.provider_tests {
            let _ = window.emit(MainSignal::ProviderTests.event_name(), ());
        }
        if drained.pricing_state && was_visible {
            let _ = window.emit(MainSignal::PricingState.event_name(), ());
        }
        if drained.usage_state && was_visible {
            let _ = window.emit(MainSignal::UsageState.event_name(), ());
        }
        for payload in drained.config_states {
            let _ = window.emit("config-state", payload);
        }
    }
    if was_visible {
        return;
    }
    if let Ok(state) = state_for(&r) {
        let _ = window.emit("switch-state", &state);
    }
    for g in [&r.gateway, &r.claude, &r.grok, &r.dsh] {
        if let Ok(view) = g.try_view() {
            let _ = window.emit("gateway-state", &view);
            let client_id = g.client_id();
            for quota in view.providers.iter().filter_map(|p| p.quota.as_ref()) {
                let _ = window.emit(
                    "provider-quota",
                    serde_json::json!({"clientId": client_id, "quota": quota}),
                );
            }
        }
    }
    if label == "main" {
        let _ = window.emit("usage-state", ());
        let _ = window.emit("pricing-state", ());
    }
}
fn show(app: &tauri::AppHandle, page: Option<&str>) -> Result<()> {
    notifications::on_manual_open(app);
    #[cfg(target_os = "macos")]
    app.set_activation_policy(tauri::ActivationPolicy::Regular)
        .map_err(|_| AppError::new(code::WINDOW, "无法显示主窗口"))?;
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        w.show()
            .map_err(|_| AppError::new(code::WINDOW, "无法显示主窗口"))?;
        let _ = w.set_focus();
        let _ = w.emit("app-visibility", true);
        sync_window_on_show(app, "main");
        if let Some(page) = page {
            let _ = w.emit("navigate", page);
        }
    }
    Ok(())
}
fn tray_menu(app: &tauri::AppHandle) -> tauri::Result<Menu<tauri::Wry>> {
    let menu = Menu::new(app)?;
    for (id, label) in [
        ("open", "打开 Vela"),
        ("config", "编辑配置"),
        ("gateway", "打开网关"),
        ("settings", "设置"),
        ("check_update", "检查更新"),
        ("github", "GitHub"),
        ("quit", "退出 Vela"),
    ] {
        if id == "quit" {
            menu.append(&PredefinedMenuItem::separator(app)?)?;
        }
        menu.append(&MenuItem::with_id(app, id, label, true, None::<&str>)?)?;
    }
    Ok(menu)
}
#[tauri::command]
async fn check_for_updates() -> Result<update::Info> {
    update::latest().await
}
#[tauri::command]
fn open_github() -> Result<()> {
    update::open_repository()
}
#[tauri::command]
fn open_update_release() -> Result<()> {
    update::open_releases()
}
#[derive(Default, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
enum ProviderAction {
    #[default]
    Settings,
    Testing,
}
fn valid_main_page(page: &str) -> bool {
    ["accounts", "config", "gateway", "logs", "usage", "settings"].contains(&page)
}
#[tauri::command]
fn open_main(
    app: tauri::AppHandle,
    page: Option<String>,
    provider_id: Option<String>,
    client_id: Option<gateway::ClientId>,
    event_id: Option<String>,
    provider_action: Option<ProviderAction>,
) -> Result<()> {
    if page.as_deref().is_some_and(|p| !valid_main_page(p)) {
        return Err(AppError::new(code::WINDOW, "无效页面"));
    }
    quick::hide_quick(app.clone())?;
    if let Some(id) = event_id {
        uuid::Uuid::parse_str(&id).map_err(|_| AppError::new(code::EVENT, "日志标识无效"))?;
        show(&app, None)?;
        if let Some(w) = app.get_webview_window("main") {
            let _ = w.emit("event-log", serde_json::json!({"id":id,"clientId":client_id.unwrap_or_default(),"providerId":provider_id}));
        }
        Ok(())
    } else if let Some(id) = provider_id {
        show(&app, None)?;
        if let Some(w) = app.get_webview_window("main") {
            let _ = w.emit(
                "provider-settings",
                serde_json::json!({"id":id,"clientId":client_id.unwrap_or_default(),"action":provider_action.unwrap_or_default()}),
            );
        }
        Ok(())
    } else {
        show(&app, page.as_deref())
    }
}
const VISIBLE_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);
const HIDDEN_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);

async fn wait_next_background_poll(has_visible_windows: bool, wake: &tokio::sync::Notify) {
    if has_visible_windows {
        tokio::time::sleep(VISIBLE_POLL_INTERVAL).await;
    } else {
        tokio::select! {
            _ = tokio::time::sleep(HIDDEN_POLL_INTERVAL) => {}
            _ = wake.notified() => {}
        }
    }
}

const fn should_poll_editor_configs(has_visible_windows: bool, background_active: bool) -> bool {
    has_visible_windows || background_active
}

fn emit_account_sync_diagnostic(diagnostics: &events::Service, state: &ViewState) -> bool {
    let should_emit = state.error.is_some()
        || state
            .auth_sync
            .as_ref()
            .is_some_and(|s| matches!(s.state.as_str(), "invalid" | "missing" | "error"));
    if should_emit {
        diagnostics.emit(events::Record::new(
            Some(gateway::ClientId::Codex),
            None,
            None,
            events::Reason::AccountSync,
            events::Action::Stopped,
            None,
            None,
        ));
    }
    should_emit
}

fn tray_tooltip_text(state: &ViewState) -> String {
    let current = state
        .accounts
        .iter()
        .find(|a| a.current)
        .map(|a| a.name.as_str())
        .unwrap_or("未保存账号");
    format!("Vela · {current}")
}

enum PollStateUpdate {
    Unchanged,
    StateChanged(ViewState),
    ErrorChanged(ViewState),
}

fn poll_account_state_step(
    core: &Mutex<Core>,
    gateway: &gateway::Gateway,
    last: &mut Option<ViewState>,
) -> PollStateUpdate {
    let result = {
        let mut c = core.lock().unwrap();
        c.state().map(|s| (s, c.home()))
    };
    match result {
        Ok((s, home)) => {
            if last.as_ref() != Some(&s) {
                gateway.observe_home(&home);
                *last = Some(s.clone());
                return PollStateUpdate::StateChanged(s);
            }
        }
        Err(e) => {
            if let Some(mut s) = last.clone() {
                if s.error.as_ref() != Some(&e.message) {
                    s.error = Some(e.message);
                    *last = Some(s.clone());
                    return PollStateUpdate::ErrorChanged(s);
                }
            }
        }
    }
    PollStateUpdate::Unchanged
}

fn publish(app: &tauri::AppHandle, state: ViewState) {
    if let Some(r) = app.try_state::<Arc<Runtime>>() {
        emit_account_sync_diagnostic(&r.diagnostics, &state);
    }
    if let Some(tray) = app.tray_by_id("switch") {
        let _ = tray.set_tooltip(Some(tray_tooltip_text(&state)));
    }
    for window in visible_windows(app) {
        let _ = window.emit("switch-state", &state);
    }
}
fn refresh(app: &tauri::AppHandle, r: &Runtime) -> Result<ViewState> {
    let state = state_for(r)?;
    let home = r.core.lock().unwrap().home();
    r.gateway.observe_home(&home);
    publish(app, state.clone());
    Ok(state)
}
pub(crate) fn coalesce_gateway_events(events: &mut tokio::sync::broadcast::Receiver<()>) {
    // Drain only the backlog observed now. A busy producer cannot postpone
    // publishing the latest view or the periodic final zero indefinitely.
    let pending = events.len();
    for _ in 0..pending {
        match events.try_recv() {
            Ok(()) | Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => (),
            Err(_) => break,
        }
    }
}
#[cfg(test)]
mod gateway_broadcast_tests {
    #[tokio::test]
    async fn coalesced_burst_publishes_latest_and_leaves_future_notification_live() {
        let (tx, mut rx) = tokio::sync::broadcast::channel(8);
        let mut final_state = 0;
        for n in 1..=32 {
            final_state = n;
            tx.send(()).unwrap();
        }
        let _ = rx.recv().await;
        super::coalesce_gateway_events(&mut rx);
        assert_eq!(final_state, 32);
        assert!(matches!(
            rx.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));
        tx.send(()).unwrap();
        rx.recv().await.unwrap();
        drop(tx);
        super::coalesce_gateway_events(&mut rx);
        assert!(matches!(
            rx.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Closed)
        ));
    }
}
fn publish_gateway(app: &tauri::AppHandle, gateway: &gateway::Gateway) {
    let windows = visible_windows(app);
    if windows.is_empty() {
        return;
    }
    match gateway.try_view() {
        Ok(view) => {
            for window in windows {
                let _ = window.emit("gateway-state", &view);
            }
        }
        Err(error) => {
            let _ = app.emit("switch-error", error);
        }
    }
}
fn state_for(r: &Runtime) -> Result<ViewState> {
    r.core.lock().unwrap().state()
}
#[tauri::command]
async fn list_provider_models(
    client_id: gateway::ClientId,
    r: tauri::State<'_, Arc<Runtime>>,
    provider_id: String,
    force: bool,
) -> Result<gateway::catalog::View> {
    r.gateway(client_id).list_models(&provider_id, force).await
}
#[tauri::command]
fn get_state(r: tauri::State<'_, Arc<Runtime>>) -> Result<ViewState> {
    state_for(r.inner())
}
#[tauri::command]
async fn switch_account(
    app: tauri::AppHandle,
    r: tauri::State<'_, Arc<Runtime>>,
    id: String,
    expected_revision: String,
) -> Result<ViewState> {
    let _connection_guard = r.official_tx.lock().await;
    let mut core = r.core.lock().unwrap();
    let official_view = core.official_view();
    if official::blocks_gateway(&r.data) && official_view.account_id.as_deref() != Some(id.as_str())
    {
        return Err(AppError::new(
            code::OFFICIAL_MODE,
            "请先关闭官方账号连接，再切换其他账号",
        ));
    }
    let result = core.switch_account(&id, &expected_revision);
    drop(core);
    match result {
        Ok(s) => {
            publish(&app, s.clone());
            Ok(s)
        }
        Err(e) => {
            let _ = refresh(&app, &r);
            Err(e)
        }
    }
}

#[tauri::command]
async fn use_official_account(
    app: tauri::AppHandle,
    r: tauri::State<'_, Arc<Runtime>>,
    account_id: String,
    expected_auth_revision: String,
    expected_config_revision: String,
) -> Result<ViewState> {
    let _official_guard = r.official_tx.lock().await;
    let state = get_state(r.clone())?;
    let account = state
        .accounts
        .iter()
        .find(|a| a.id == account_id)
        .ok_or_else(|| AppError::new(code::ACCOUNT, "账号不存在"))?;
    if account.kind != "chatgpt" {
        return Err(AppError::new(
            code::OFFICIAL_MODE,
            "只有 ChatGPT 账号可以使用官方连接",
        ));
    }
    if state.official_mode.enabled {
        if state.official_mode.account_id.as_deref() == Some(account_id.as_str()) {
            return Ok(state);
        }
        return Err(AppError::new(
            code::OFFICIAL_MODE,
            "已有官方账号连接，请先关闭",
        ));
    }
    if r.gateway.try_view()?.recovery_pending && !r.gateway.try_view()?.running {
        return Err(AppError::new(
            code::RECOVERY,
            "请先处理 Codex 网关的恢复事务",
        ));
    }
    let gateway_was_running = r.gateway.try_view()?.running;
    if gateway_was_running {
        r.gateway.stop_checked(None).await?;
    }
    let home = r.home(gateway::ClientId::Codex)?;
    let config_revision = r.gateway.read_config(&home)?.revision;
    if !gateway_was_running
        && config_revision != expected_config_revision
        && !expected_config_revision.is_empty()
    {
        return Err(AppError::new(
            code::CONFLICT,
            "Codex 配置已变化，请刷新后重试",
        ));
    }
    let auth_revision = expected_auth_revision.clone();
    official::enable_with_switch(&r.data, &home, &account_id, &config_revision, || {
        r.core
            .lock()
            .unwrap()
            .switch_account(&account_id, &auth_revision)
            .map(|_| ())
    })?;
    let next = refresh(&app, &r)?;
    let _ = app.emit("switch-notice", "官方账号已启用，请重新打开 Codex");
    Ok(next)
}

#[tauri::command]
async fn disable_official_account(
    app: tauri::AppHandle,
    r: tauri::State<'_, Arc<Runtime>>,
) -> Result<ViewState> {
    let _official_guard = r.official_tx.lock().await;
    let home = r.home(gateway::ClientId::Codex)?;
    official::disable(&r.data, &home)?;
    let next = refresh(&app, &r)?;
    let _ = app.emit("switch-notice", "官方连接已关闭，配置已恢复");
    Ok(next)
}
#[tauri::command]
fn import_current(app: tauri::AppHandle, r: tauri::State<'_, Arc<Runtime>>) -> Result<ViewState> {
    let mut c = r.core.lock().unwrap();
    let path = c.home().join("auth.json");
    c.import_file(&path, None)?;
    drop(c);
    refresh(&app, &r)
}
#[tauri::command]
fn rename_account(
    app: tauri::AppHandle,
    r: tauri::State<'_, Arc<Runtime>>,
    id: String,
    name: String,
) -> Result<ViewState> {
    r.core.lock().unwrap().rename(&id, &name)?;
    refresh(&app, &r)
}
#[tauri::command]
fn delete_account(
    app: tauri::AppHandle,
    r: tauri::State<'_, Arc<Runtime>>,
    id: String,
) -> Result<ViewState> {
    if let Ok(session) = r.login.lock() {
        if login_active(&session.state)
            && session.state.target_account_id.as_deref() == Some(id.as_str())
        {
            return Err(AppError::new(
                code::LOGIN_BUSY,
                "该账号正在刷新登录，请先取消或等待完成",
            ));
        }
    }
    r.core.lock().unwrap().delete(&id)?;
    refresh(&app, &r)
}
#[tauri::command]
fn read_config(
    client_id: Option<gateway::ClientId>,
    r: tauri::State<'_, Arc<Runtime>>,
) -> Result<configuration::Document> {
    let client = client_id.unwrap_or_default();
    r.gateway(client).read_config(&r.home(client)?)
}
#[tauri::command]
fn read_previous_config(
    client_id: gateway::ClientId,
    r: tauri::State<'_, Arc<Runtime>>,
) -> Result<String> {
    r.gateway(client_id).previous_config()
}
#[tauri::command]
fn validate_config(client_id: Option<gateway::ClientId>, text: String) -> Result<()> {
    configuration::validate(client_id.unwrap_or_default(), &text)
}
#[tauri::command]
async fn save_config(
    app: tauri::AppHandle,
    r: tauri::State<'_, Arc<Runtime>>,
    text: String,
    expected_revision: String,
    client_id: Option<gateway::ClientId>,
) -> Result<configuration::Document> {
    let _official_guard = r.official_tx.lock().await;
    let client = client_id.unwrap_or_default();
    let home = r.home(client)?;
    if client == gateway::ClientId::Codex {
        official::guard_save(&r.data, &home, &text)?;
    } else if client == gateway::ClientId::Claude {
        claude_profile::guard_save(&r.data, &home, &claude_profile::user_home()?, &text)?;
    }
    let doc = r
        .gateway(client)
        .save_config(&home, &text, &expected_revision)
        .map_err(|e| r.config_failure(client, e))?;
    let _ = app.emit(
        "config-state",
        serde_json::json!({"clientId":client,"revision":doc.revision,"guarded":doc.guarded}),
    );
    refresh(&app, &r)?;
    Ok(doc)
}
#[tauri::command]
fn set_quota_refresh(
    app: tauri::AppHandle,
    r: tauri::State<'_, Arc<Runtime>>,
    seconds: u64,
    expected_seconds: u64,
) -> Result<ViewState> {
    let state = r
        .core
        .lock()
        .unwrap()
        .set_quota_refresh(seconds, expected_seconds)?;
    r.gateway.set_quota_interval(seconds);
    r.claude.set_quota_interval(seconds);
    r.grok.set_quota_interval(seconds);
    r.dsh.set_quota_interval(seconds);
    publish(&app, state.clone());
    Ok(state)
}
#[tauri::command]
async fn set_preferences(
    app: tauri::AppHandle,
    r: tauri::State<'_, Arc<Runtime>>,
    mut preferences: Preferences,
) -> Result<ViewState> {
    let _official_guard = r.official_tx.lock().await;
    if matches!(
        r.claude_login.lock().unwrap().state.phase.as_str(),
        "waiting" | "cancelling"
    ) && r.core.lock().unwrap().preferences().claude_home != preferences.claude_home
    {
        return Err(AppError::new(
            code::LOGIN_BUSY,
            "请先完成或取消 Claude 登录",
        ));
    }
    if login_active(&r.login.lock().unwrap().state) {
        return Err(AppError::new(code::LOGIN_BUSY, "请先完成或取消登录"));
    }
    if (r.gateway.guarded_home() || official::blocks_gateway(&r.data))
        && r.core.lock().unwrap().preferences().codex_home != preferences.codex_home
    {
        return Err(AppError::new(
            code::GATEWAY_ACTIVE,
            "更换 Codex 目录前请先停用网关并恢复配置",
        ));
    }
    if (r.claude.guarded_home() || claude_profile::blocks(&r.data))
        && r.core.lock().unwrap().preferences().claude_home != preferences.claude_home
    {
        return Err(AppError::new(
            code::GATEWAY,
            "请先停止 Claude Code 网关并处理恢复事务",
        ));
    }
    if r.dsh.guarded_home() && r.core.lock().unwrap().preferences().dsh_home != preferences.dsh_home
    {
        return Err(AppError::new(
            code::GATEWAY,
            "请先停止 DSH 网关并处理恢复事务",
        ));
    }
    if r.grok.guarded_home()
        && r.core.lock().unwrap().preferences().grok_home != preferences.grok_home
    {
        return Err(AppError::new(
            code::GATEWAY,
            "请先停止 Grok Build 网关并处理恢复事务",
        ));
    }
    let s = {
        let mut core = r.core.lock().unwrap();
        preferences.quota_refresh_seconds = core.preferences().quota_refresh_seconds;
        core.set_preferences(preferences)?
    };
    r.gateway
        .set_quota_interval(s.preferences.quota_refresh_seconds);
    r.claude
        .set_quota_interval(s.preferences.quota_refresh_seconds);
    r.grok
        .set_quota_interval(s.preferences.quota_refresh_seconds);
    r.dsh
        .set_quota_interval(s.preferences.quota_refresh_seconds);
    if s.preferences.system_notifications {
        notifications::on_manual_open(&app);
    }
    r.gateway.observe_home(&r.home(gateway::ClientId::Codex)?);
    r.claude.observe_home(&r.home(gateway::ClientId::Claude)?);
    r.grok.observe_home(&r.home(gateway::ClientId::Grok)?);
    r.dsh.observe_home(&r.home(gateway::ClientId::Dsh)?);
    publish(&app, s.clone());
    Ok(s)
}
#[tauri::command]
async fn get_startup(r: tauri::State<'_, Arc<Runtime>>) -> Result<startup::View> {
    let r = r.inner().clone();
    tauri::async_runtime::spawn_blocking(move || r.startup.view())
        .await
        .map_err(|_| AppError::new(code::STARTUP, "读取启动设置失败"))?
}
#[tauri::command]
async fn set_startup(r: tauri::State<'_, Arc<Runtime>>, enabled: bool) -> Result<startup::View> {
    let r = r.inner().clone();
    tauri::async_runtime::spawn_blocking(move || r.startup.update(enabled))
        .await
        .map_err(|_| AppError::new(code::STARTUP, "更新启动设置失败"))?
}
#[tauri::command]
fn get_login(r: tauri::State<'_, Arc<Runtime>>) -> Result<login::LoginState> {
    Ok(r.login.lock().unwrap().state.clone())
}
#[tauri::command]
fn start_login(
    app: tauri::AppHandle,
    r: tauri::State<'_, Arc<Runtime>>,
    mode: String,
    target_account_id: Option<String>,
    expected_credential_revision: Option<String>,
) -> Result<login::LoginState> {
    if !["browser", "device"].contains(&mode.as_str()) {
        return Err(AppError::new(code::LOGIN_MODE, "登录方式无效"));
    }
    let prefs = r.core.lock().unwrap().preferences();
    let cli = login::resolve_cli(&prefs.cli_path)?;
    let target = match target_account_id.as_deref() {
        Some(id) if mode == "device" => Some(r.core.lock().unwrap().login_target(
            id,
            expected_credential_revision.as_deref().unwrap_or_default(),
        )?),
        Some(_) => {
            return Err(AppError::new(
                code::LOGIN_MODE,
                "指定账号刷新请使用设备码登录",
            ))
        }
        None => None,
    };
    let mut session = r.login.lock().unwrap();
    if login_active(&session.state) {
        return Err(AppError::new(code::LOGIN_BUSY, "已有登录正在进行"));
    }
    let (tx, rx) = tokio::sync::oneshot::channel();
    let (callback_tx, callback_rx) = tokio::sync::mpsc::channel(2);
    session.cancel = Some(tx);
    session.callback = Some(callback_tx);
    session.state = login::LoginState {
        phase: "starting".into(),
        mode: mode.clone(),
        message: "正在启动官方登录…".into(),
        callback_ready: false,
        callback_port: None,
        target_account_id: target_account_id.clone(),
        ..Default::default()
    };
    let initial = session.state.clone();
    drop(session);
    r.window_wake.notify_one();
    let runtime = r.inner().clone();
    tauri::async_runtime::spawn(async move {
        let (sender, mut events) = tokio::sync::mpsc::channel(8);
        let task = login::run_with_callbacks(&cli, &mode, rx, sender, callback_rx);
        tokio::pin!(task);
        let result = loop {
            tokio::select! {
                Some(mut state) = events.recv() => {
                    if let Ok(mut s) = runtime.login.lock() {
                        // Buffered CLI prompts must not revive a cancelled session.
                        if s.cancel.is_some() {
                            state.target_account_id = target_account_id.clone();
                            s.state = state.clone();
                            let _ = app.emit("login-state", state);
                        }
                    }
                },
                result = &mut task => break result,
            }
        };
        let _connection_guard = runtime.official_tx.lock().await;
        let mut final_state = match result {
            Ok(Some(raw)) => match (|| -> Result<bool> {
                let s = runtime.login.lock().unwrap();
                if s.cancel.is_none() {
                    return Err(AppError::new(code::LOGIN_CANCELLED, "登录已取消"));
                }
                let mut c = runtime.core.lock().unwrap();
                match target.as_ref() {
                    Some(target) => c.complete_target_login(&raw, target),
                    None => c.complete_login(&raw),
                }
            })() {
                Ok(updated) => login::LoginState {
                    phase: "success".into(),
                    mode: mode.clone(),
                    message: if target.is_some() {
                        "指定账号凭据已更新"
                    } else if updated {
                        "当前账号凭据已更新"
                    } else {
                        "账号已添加，选择后即可切换"
                    }
                    .into(),
                    ..Default::default()
                },
                Err(e) => login::LoginState {
                    phase: "error".into(),
                    mode: mode.clone(),
                    message: e.message,
                    ..Default::default()
                },
            },
            Ok(None) => login::LoginState {
                phase: "cancelled".into(),
                mode: mode.clone(),
                message: "登录已取消".into(),
                ..Default::default()
            },
            Err(e) => login::LoginState {
                phase: "error".into(),
                mode: mode.clone(),
                message: e.message,
                ..Default::default()
            },
        };
        final_state.target_account_id = target_account_id;
        if let Ok(mut s) = runtime.login.lock() {
            s.state = final_state.clone();
            s.cancel = None;
            s.callback = None;
        }
        let _ = app.emit("login-state", final_state);
        let _ = refresh(&app, &runtime);
        if runtime.quitting.load(Ordering::Relaxed) {
            app.exit(0);
        }
    });
    Ok(initial)
}
#[tauri::command]
fn cancel_login(r: tauri::State<'_, Arc<Runtime>>) -> Result<()> {
    let mut s = r.login.lock().unwrap();
    if let Some(tx) = s.cancel.take() {
        let _ = tx.send(());
        s.state.phase = "cancelling".into();
        s.callback = None;
    }
    Ok(())
}
#[tauri::command]
fn open_login_url(r: tauri::State<'_, Arc<Runtime>>) -> Result<()> {
    let s = r.login.lock().unwrap();
    let url = s
        .state
        .auth_url
        .as_ref()
        .or(s.state.url.as_ref())
        .ok_or_else(|| AppError::new(code::LOGIN_URL, "登录链接尚未生成"))?;
    open::that(url).map_err(|_| AppError::new(code::OPEN, "无法打开浏览器"))
}
#[tauri::command]
fn copy_login_value(
    app: tauri::AppHandle,
    r: tauri::State<'_, Arc<Runtime>>,
    kind: login::CopyKind,
) -> Result<()> {
    use tauri_plugin_clipboard_manager::ClipboardExt;
    r.login
        .lock()
        .unwrap()
        .copy_value(kind, |value| app.clipboard().write_text(value))
}
#[tauri::command]
async fn complete_login_callback(
    app: tauri::AppHandle,
    r: tauri::State<'_, Arc<Runtime>>,
    callback_url: String,
) -> Result<login::LoginState> {
    let callback = {
        let session = r.login.lock().unwrap();
        if session.cancel.is_none()
            || session.state.mode != "browser"
            || !["starting", "waiting"].contains(&session.state.phase.as_str())
            || !session.state.callback_ready
        {
            return Err(AppError::new(code::LOGIN_STATE, "当前浏览器登录会话不可用"));
        }
        session
            .callback
            .clone()
            .ok_or_else(|| AppError::new(code::LOGIN_STATE, "当前登录会话尚未准备回调"))?
    };
    let (reply, result) = tokio::sync::oneshot::channel();
    callback
        .send(login::CallbackRequest {
            url: callback_url,
            reply,
        })
        .await
        .map_err(|_| AppError::new(code::LOGIN_STATE, "登录会话已结束，请重新开始"))?;
    result
        .await
        .map_err(|_| AppError::new(code::LOGIN_STATE, "登录会话已结束，请重新开始"))??;
    let mut session = r.login.lock().unwrap();
    if session.cancel.is_none() || !login_active(&session.state) {
        return Err(AppError::new(
            code::LOGIN_STATE,
            "登录会话已结束，请重新开始",
        ));
    }
    session.state.message = "已提交，等待登录完成".into();
    let state = session.state.clone();
    drop(session);
    let _ = app.emit("login-state", state.clone());
    Ok(state)
}
fn quit(app: &tauri::AppHandle, r: &Arc<Runtime>) {
    if r.quit_pending.swap(true, Ordering::Relaxed) {
        return;
    }
    let runtime = r.clone();
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        if let Err(e) = runtime.stop_gateways().await {
            if runtime.smoke.is_some() {
                if let Ok(mut result) = runtime.smoke_result.lock() {
                    *result = Some(Err(e));
                }
                runtime.quitting.store(true, Ordering::Relaxed);
                runtime.window_wake.notify_one();
                app.exit(1);
                return;
            }
            runtime.quit_pending.store(false, Ordering::Relaxed);
            let _ = app.emit("switch-error", e);
            let _ = show(&app, Some("gateway"));
            return;
        }
        runtime.quitting.store(true, Ordering::Relaxed);
        runtime.window_wake.notify_one();
        if let Ok(mut s) = runtime.login.lock() {
            if let Some(tx) = s.cancel.take() {
                let _ = tx.send(());
                return;
            }
            if login_active(&s.state) {
                return;
            }
        }
        app.exit(0);
    });
}
#[tauri::command]
fn get_gateway_freeze(r: tauri::State<'_, Arc<Runtime>>) -> gateway::freeze::View {
    r.gateway.freeze_gate().view()
}
#[tauri::command]
fn set_gateway_frozen(
    frozen: bool,
    r: tauri::State<'_, Arc<Runtime>>,
) -> Result<gateway::freeze::View> {
    r.gateway.freeze_gate().set(frozen)
}
#[tauri::command]
fn get_gateway(
    client_id: gateway::ClientId,
    r: tauri::State<'_, Arc<Runtime>>,
) -> Result<gateway::View> {
    if let Ok(home) = r.home(client_id) {
        r.gateway(client_id).observe_home(&home);
    }
    r.gateway(client_id).try_view()
}
#[tauri::command]
fn get_failure_policy_catalog() -> Vec<gateway::failure_match::CatalogEntry> {
    gateway::failure_match::catalog()
}
#[tauri::command]
async fn match_failure_policy(
    client_id: gateway::ClientId,
    mut input: gateway::failure_match::Input,
    r: tauri::State<'_, Arc<Runtime>>,
) -> Result<gateway::failure_match::Match> {
    let diagnostics = r.diagnostics.clone();
    let gateway = r.gateway(client_id).clone();
    tauri::async_runtime::spawn_blocking(move || {
        if let Some(id) = input.event_id.as_deref() {
            let event = diagnostics
                .detail(id)
                .ok_or_else(|| AppError::new(code::LOG_READ, "日志已过期或不存在"))?;
            if event.record.client_id != Some(client_id.into()) {
                return Err(AppError::new(code::LOG_READ, "日志客户端不匹配"));
            }
            input.status = event.record.status;
            if let Some(raw) = diagnostics.raw(id)? {
                if raw.base64 {
                    return Err(AppError::new(
                        code::LOG_READ,
                        "错误响应不是文本，请在日志中复制原始数据",
                    ));
                }
                input.raw_response = Some(raw.body);
                input.headers = Some(raw.headers);
                input.event = raw.event;
            }
        }
        gateway.match_failure_policy(input)
    })
    .await
    .map_err(|_| AppError::new(code::LOG_READ, "错误策略查询失败"))?
}
#[tauri::command]
fn cancel_gateway_wait(
    client_id: gateway::ClientId,
    request_id: String,
    r: tauri::State<'_, Arc<Runtime>>,
) -> bool {
    r.gateway(client_id).cancel_wait(&request_id)
}
#[tauri::command]
fn get_gateway_models(
    client_id: Option<gateway::ClientId>,
    r: tauri::State<'_, Arc<Runtime>>,
) -> Result<gateway::model_list::View> {
    r.gateway(client_id.unwrap_or_default()).gateway_models()
}
#[tauri::command]
async fn refresh_gateway_models(
    client_id: Option<gateway::ClientId>,
    provider_ids: Option<Vec<String>>,
    r: tauri::State<'_, Arc<Runtime>>,
) -> Result<gateway::model_list::View> {
    r.gateway(client_id.unwrap_or_default())
        .refresh_gateway_models(provider_ids)
        .await
}
#[tauri::command]
fn search_models_dev(
    query: String,
    limit: Option<usize>,
    r: tauri::State<'_, Arc<Runtime>>,
) -> Result<models_dev::Search> {
    Ok(r.usage
        .prices()?
        .models_dev
        .search(&query, limit.unwrap_or(50)))
}
#[tauri::command]
fn edit_model_list(
    client_id: Option<gateway::ClientId>,
    op: gateway::model_list::Operation,
    id: String,
    source: Option<String>,
    r: tauri::State<'_, Arc<Runtime>>,
) -> Result<gateway::model_list::View> {
    r.gateway(client_id.unwrap_or_default())
        .edit_model_list(op, &id, source.as_deref())
}
#[tauri::command]
async fn dsh_official_models(
    refresh: bool,
    r: tauri::State<'_, Arc<Runtime>>,
) -> Result<gateway::dsh::official::CatalogView> {
    r.dsh
        .dsh_official_models(&r.home(gateway::ClientId::Dsh)?, refresh)
        .await
}
#[tauri::command]
fn preview_dsh_profile(
    profile: Option<String>,
    r: tauri::State<'_, Arc<Runtime>>,
) -> Result<gateway::dsh::Preview> {
    r.dsh
        .preview_dsh(&r.home(gateway::ClientId::Dsh)?, profile.as_deref())
}
#[tauri::command]
async fn attach_dsh_profiles(
    options: gateway::dsh::Options,
    all: bool,
    expected_revision: String,
    preview_revision: String,
    r: tauri::State<'_, Arc<Runtime>>,
) -> Result<gateway::dsh::Batch> {
    r.dsh
        .attach_dsh_profiles(
            &r.home(gateway::ClientId::Dsh)?,
            options,
            all,
            &expected_revision,
            Some(&preview_revision),
        )
        .await
        .map_err(|e| r.config_failure(gateway::ClientId::Dsh, e))
}
#[tauri::command]
fn read_dsh_file(
    profile: Option<String>,
    key: gateway::dsh::editor::FileKey,
    r: tauri::State<'_, Arc<Runtime>>,
) -> Result<gateway::dsh::editor::FileView> {
    r.dsh
        .read_dsh_file(&r.home(gateway::ClientId::Dsh)?, profile.as_deref(), key)
}
#[tauri::command]
fn read_previous_dsh_file(
    profile: Option<String>,
    key: gateway::dsh::editor::FileKey,
    r: tauri::State<'_, Arc<Runtime>>,
) -> Result<String> {
    r.dsh
        .previous_dsh_file(&r.home(gateway::ClientId::Dsh)?, profile.as_deref(), key)
}
#[tauri::command]
async fn save_dsh_file(
    profile: Option<String>,
    key: gateway::dsh::editor::FileKey,
    text: String,
    expected_revision: String,
    r: tauri::State<'_, Arc<Runtime>>,
) -> Result<gateway::dsh::editor::FileView> {
    r.dsh
        .save_dsh_file(
            &r.home(gateway::ClientId::Dsh)?,
            profile.as_deref(),
            key,
            Some(&text),
            &expected_revision,
        )
        .await
        .map_err(|e| r.config_failure(gateway::ClientId::Dsh, e))
}
#[tauri::command]
async fn restore_dsh_file(
    profile: Option<String>,
    key: gateway::dsh::editor::FileKey,
    expected_revision: String,
    r: tauri::State<'_, Arc<Runtime>>,
) -> Result<gateway::dsh::editor::FileView> {
    r.dsh
        .save_dsh_file(
            &r.home(gateway::ClientId::Dsh)?,
            profile.as_deref(),
            key,
            None,
            &expected_revision,
        )
        .await
        .map_err(|e| r.config_failure(gateway::ClientId::Dsh, e))
}
#[tauri::command]
fn preview_grok_import(
    selected: Option<String>,
    r: tauri::State<'_, Arc<Runtime>>,
) -> Result<gateway::GrokImportPreview> {
    r.grok
        .preview_grok_import(&r.home(gateway::ClientId::Grok)?, selected.as_deref())
}
#[tauri::command]
fn cancel_grok_import(token: Option<String>, r: tauri::State<'_, Arc<Runtime>>) {
    r.grok.cancel_grok_import(token.as_deref());
}
#[tauri::command]
async fn commit_grok_import(
    token: String,
    expected_revision: String,
    app: tauri::AppHandle,
    r: tauri::State<'_, Arc<Runtime>>,
) -> Result<gateway::View> {
    let _guard = r.official_tx.lock().await;
    let view = r.grok.commit_grok_import(
        &r.home(gateway::ClientId::Grok)?,
        &token,
        &expected_revision,
    )?;
    let result = r
        .grok
        .start(&view.revision, &r.home(gateway::ClientId::Grok)?)
        .await;
    let _ = refresh(&app, &r);
    result
}
#[tauri::command]
fn reveal_provider_token(
    window: tauri::WebviewWindow,
    client_id: gateway::ClientId,
    provider_id: String,
    expected_revision: String,
    r: tauri::State<'_, Arc<Runtime>>,
) -> Result<String> {
    r.gateway(client_id)
        .reveal_provider_token(window.label(), &provider_id, &expected_revision)
}
#[tauri::command]
async fn probe_provider_endpoints(
    client_id: gateway::ClientId,
    provider_id: String,
    r: tauri::State<'_, Arc<Runtime>>,
) -> Result<gateway::View> {
    r.gateway(client_id)
        .probe_endpoints(&provider_id, true)
        .await
}
#[tauri::command]
async fn update_gateway(
    client_id: gateway::ClientId,
    app: tauri::AppHandle,
    r: tauri::State<'_, Arc<Runtime>>,
    edit: gateway::Edit,
    expected_revision: String,
    expected_config_revision: Option<String>,
) -> Result<gateway::View> {
    let _official_guard = r.official_tx.lock().await;
    let home = r.home(client_id)?;
    let selected = matches!(&edit, gateway::Edit::Select { .. });
    let result = r
        .gateway(client_id)
        .edit_checked(
            edit,
            &expected_revision,
            &home,
            expected_config_revision.as_deref(),
        )
        .map_err(|e| r.config_failure(client_id, e))?;
    let _ = refresh(&app, &r);
    if selected {
        let _ = app.emit(
            "switch-notice",
            if result.running {
                "已切换供应商，新请求立即生效".to_string()
            } else {
                format!("文件已切换，请重新打开 {}", client_id.name())
            },
        );
    }
    Ok(result)
}
#[tauri::command]
async fn start_gateway(
    client_id: gateway::ClientId,
    app: tauri::AppHandle,
    r: tauri::State<'_, Arc<Runtime>>,
    expected_revision: String,
    expected_config_revision: Option<String>,
) -> Result<gateway::View> {
    let _official_guard = r.official_tx.lock().await;
    if client_id == gateway::ClientId::Codex && official::blocks_gateway(&r.data) {
        return Err(AppError::new(
            code::OFFICIAL_MODE,
            "请先关闭官方账号连接，再启动 Codex 网关",
        ));
    }
    let home = r.home(client_id)?;
    let result = r
        .gateway(client_id)
        .start_checked(
            &expected_revision,
            &home,
            expected_config_revision.as_deref(),
        )
        .await
        .map_err(|e| r.config_failure(client_id, e))?;
    r.window_wake.notify_one();
    let _ = refresh(&app, &r);
    let _ = app.emit(
        "switch-notice",
        format!("网关已启用，请重新打开 {}", client_id.name()),
    );
    Ok(result)
}
#[tauri::command]
async fn stop_gateway(
    client_id: gateway::ClientId,
    app: tauri::AppHandle,
    r: tauri::State<'_, Arc<Runtime>>,
    expected_config_revision: Option<String>,
) -> Result<gateway::View> {
    let _official_guard = r.official_tx.lock().await;
    let result = r
        .gateway(client_id)
        .stop_checked(expected_config_revision.as_deref())
        .await
        .map_err(|e| r.config_failure(client_id, e))?;
    r.window_wake.notify_one();
    let _ = refresh(&app, &r);
    let _ = app.emit(
        "switch-notice",
        format!("已写入当前供应商，请重新打开 {}", client_id.name()),
    );
    Ok(result)
}
#[tauri::command]
async fn query_provider_quota(
    client_id: gateway::ClientId,
    r: tauri::State<'_, Arc<Runtime>>,
    provider_id: String,
    force: bool,
) -> Result<gateway::QuotaView> {
    r.gateway(client_id).query_quota(&provider_id, force).await
}
#[tauri::command]
async fn frontend_ready(
    app: tauri::AppHandle,
    window: tauri::WebviewWindow,
    r: tauri::State<'_, Arc<Runtime>>,
) -> Result<()> {
    if window.label() != "main" {
        return Ok(());
    }
    if r.frontend_started.swap(true, Ordering::AcqRel) {
        return Ok(());
    }
    if !r.start_silently {
        notifications::on_manual_open(&app);
    }
    if r.smoke.is_some() {
        return smoke::run(&app, &r).await;
    } else {
        for client in [
            gateway::ClientId::Codex,
            gateway::ClientId::Claude,
            gateway::ClientId::Grok,
            gateway::ClientId::Dsh,
        ] {
            if (client == gateway::ClientId::Codex && official::blocks_gateway(&r.data))
                || (client == gateway::ClientId::Claude && claude_profile::blocks(&r.data))
            {
                continue;
            }
            if let Err(e) = r.gateway(client).resume(&r.home(client)?).await {
                let e = AppError::new(&e.code, &format!("{}：{}", client.name(), e.message));
                if let Some(record) = events::Record::configuration_failure(client, &e, true) {
                    r.diagnostics.emit(record);
                }
                *r.startup_error.lock().unwrap() = Some(e.clone());
                let _ = app.emit("switch-error", e);
            }
            publish_gateway(&app, r.gateway(client));
        }
        if !r.start_silently {
            show(&app, None)?;
        }
    }
    Ok(())
}
#[cfg(windows)]
fn tray_is_dark() -> bool {
    use winreg::{enums::HKEY_CURRENT_USER, RegKey};
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize")
        .and_then(|key| key.get_value::<u32, _>("SystemUsesLightTheme"))
        .unwrap_or(0)
        == 0
}
pub fn run() {
    let args: Vec<_> = std::env::args().collect();
    let smoke = args
        .iter()
        .position(|s| s == "--smoke-test")
        .and_then(|i| args.get(i + 1))
        .map(PathBuf::from);
    let mut context = tauri::generate_context!();
    if smoke.is_some() {
        // Installer checks must not open the live application's WebView store.
        for window in &mut context.config_mut().app.windows {
            window.incognito = true;
        }
    }
    let builder = tauri::Builder::default();
    let builder = if smoke.is_none() {
        builder.plugin(tauri_plugin_single_instance::init(|app, args, _| {
            let has_link = args.iter().any(|arg| arg.starts_with("ccswitch:"));
            links::receive(app, args.iter().cloned());
            if !has_link && !args.iter().any(|a| startup::is_login_argument(a)) {
                let _ = show(app, None);
            }
        }))
    } else {
        // Smoke runs own a temporary data directory and must not notify the live app.
        builder
    };
    builder
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .setup(move |app| {
            let fixture = if smoke.is_some() {
                Some(
                    tempfile::Builder::new()
                        .prefix("vela-smoke-")
                        .tempdir()?,
                )
            } else {
                None
            };
            let user_home = dirs::home_dir()
                .ok_or_else(|| AppError::new(code::PATH, "无法定位用户主目录"))?;
            let data = fixture
                .as_ref()
                .map(|t| t.path().join("data"))
                .unwrap_or_else(|| user_home.join(".vela"));
            storage::private_dir(&data)?;
            let home = fixture
                .as_ref()
                .map(|t| t.path().join("codex"))
                .unwrap_or_else(|| {
                    std::env::var_os("CODEX_HOME")
                        .map(PathBuf::from)
                        .unwrap_or_else(|| user_home.join(".codex"))
                });
            // Single-instance registration has completed; no current spool exists yet.
            cleanup_spool::clean(&data);
            official::recover_interrupted(&data);
            let mut core = Core::new(data.clone(), home)?;
            let gateway = gateway::Gateway::new(data.clone())?;
            let claude = gateway.companion(data.join("claude"))?;
            let grok = gateway.grok_companion(data.join("grok"))?;
            let dsh = gateway.dsh_companion(data.join("dsh"))?;
            let startup = startup::Service::new();
            let start_silently = smoke.is_none() && startup::login_source(&args);
            app.manage(quick::Panel::new(&data)?);
            let power = power::Service::new();
            if let Some(fixture) = &fixture {
                storage::private_dir(&core.home())?;
                storage::private_dir(&fixture.path().join("claude"))?;
                storage::private_dir(&fixture.path().join("grok"))?;
                let mut preferences = core.preferences();
                preferences.claude_home =
                    fixture.path().join("claude").to_string_lossy().into_owned();
                preferences.grok_home = fixture.path().join("grok").to_string_lossy().into_owned();
                preferences.dsh_home = fixture.path().join("dsh").to_string_lossy().into_owned();
                core.set_preferences(preferences)?;
            }
            if smoke.is_none() {
                let _ = antigravity::auth::poll_state(&data);
            }
            let usage = usage::Service::new(&data);
            gateway.set_usage(usage.clone());
            claude.set_usage(usage.clone());
            grok.set_usage(usage.clone());
            dsh.set_usage(usage.clone());
            let codex_import_error = if smoke.is_none() { gateway.import_initial(&core.home()).err() } else { None };
            let claude_import_error = if smoke.is_none() {
                let home=std::path::PathBuf::from(core.preferences().claude_home);
                let recovery=if claude_profile::recovery_pending(&data) && claude_profile::login::running_sessions()? {
                    Err(AppError::new(code::CLAUDE_RUNNING,"请先退出 Claude Code，再恢复配置切换事务"))
                } else {claude_profile::recover(&data,&home,&claude_profile::user_home()?)};
                match recovery {
                    Err(e)=>Some(e),
                    Ok(()) if !claude_profile::blocks(&data)=>claude.import_initial(&home).err(),
                    Ok(())=>None,
                }
            } else {
                None
            };
            let grok_import_error = if smoke.is_none() { grok.import_initial(&PathBuf::from(core.preferences().grok_home)).err() } else { None };
            gateway.set_quota_interval(core.preferences().quota_refresh_seconds);
            grok.set_quota_interval(core.preferences().quota_refresh_seconds);
            dsh.set_quota_interval(core.preferences().quota_refresh_seconds);
            claude.set_quota_interval(core.preferences().quota_refresh_seconds);
            let diagnostics = events::Service::new(&data);
            gateway.set_diagnostics(diagnostics.clone());
            claude.set_diagnostics(diagnostics.clone());
            grok.set_diagnostics(diagnostics.clone());
            dsh.set_diagnostics(diagnostics.clone());
            let provider_tests = provider_testing::Service::new(&data, [gateway.clone(), claude.clone(), grok.clone(), dsh.clone()])?;
            let vertex_error = if smoke.is_none() {
                tauri::async_runtime::block_on(antigravity::auth::restore_vertex_listener(&data, &gateway)).err()
            } else { None };
            let runtime = Arc::new(Runtime {
                usage,
                diagnostics,
                core: Mutex::new(core),
                data,
                imports: Mutex::new(links::Imports::default()),
                provider_tests,
                gateway,
                claude,
                grok,
                dsh,
                login: Mutex::new(Default::default()),
                claude_login: Mutex::new(Default::default()),
                quitting: AtomicBool::new(false),
                quit_pending: AtomicBool::new(false),
                smoke,
                smoke_result: Mutex::new(None),
                fixture: Mutex::new(fixture),
                startup,
                start_silently,
                power,
                frontend_started: AtomicBool::new(false),
                force_quitting: AtomicBool::new(false),
                startup_error: Mutex::new(codex_import_error.or(claude_import_error).or(grok_import_error).or(vertex_error)),
                official_tx: tokio::sync::Mutex::new(()),
                window_wake: tokio::sync::Notify::new(),
                window_visible: [AtomicBool::new(!start_silently), AtomicBool::new(false)],
                pending_main: PendingMainEvents::default(),
            });
            runtime
                .gateway
                .observe_home(&runtime.core.lock().unwrap().home());
            runtime
                .claude
                .observe_home(&runtime.home(gateway::ClientId::Claude)?);
            let state = state_for(&runtime)?;
            app.manage(runtime.clone());
            if runtime.smoke.is_none() { provider_testing::commands::connect(app.handle()); }
            notifications::connect(app.handle(), &runtime.diagnostics);
            usage::commands::connect(app.handle(), &runtime);
            runtime.grok.observe_home(&runtime.home(gateway::ClientId::Grok)?);
            runtime.dsh.observe_home(&runtime.home(gateway::ClientId::Dsh)?);
            if runtime.smoke.is_none() {
                for g in [runtime.gateway.clone(), runtime.claude.clone(), runtime.grok.clone(), runtime.dsh.clone()] {
                    tauri::async_runtime::spawn(async move {
                        if let Err(e) = g.backfill_models().await {
                            g.report_model_error(e);
                        }
                    });
                }
            }
            runtime.gateway.report_diagnostics();
            runtime.grok.report_diagnostics();
            runtime.dsh.report_diagnostics();
            runtime.claude.report_diagnostics();
            if runtime.smoke.is_none() {
                let handle = app.handle().clone();
                let r = runtime.clone();
                tauri::async_runtime::spawn(async move {
                    if let Err(error) = links::ensure_association(&handle).await {
                        r.startup_error.lock().unwrap().get_or_insert(error.clone());
                        let _ = handle.emit("switch-error", error);
                    }
                });
            }
            links::receive(app.handle(), args.iter().cloned());
            quick::Panel::create(app.handle(), runtime.smoke.is_some())?;
            let freeze = runtime.gateway.freeze_gate();
            let mut freeze_events = freeze.subscribe();
            let freeze_app = app.handle().clone();
            let freeze_runtime = runtime.clone();
            tauri::async_runtime::spawn(async move {
                let mut power_attempted = false;
                while freeze_events.changed().await.is_ok() {
                    let view = freeze.view();
                    for window in visible_windows(&freeze_app) {
                        let _ = window.emit("gateway-freeze", &view);
                    }
                    if power::close_after_freeze(&mut power_attempted, view.frozen, view.draining) {
                        let gate = freeze.clone();
                        let r = freeze_runtime.clone();
                        let app_handle = freeze_app.clone();
                        tauri::async_runtime::spawn_blocking(move || {
                            match r.power.disable_if_enabled(|| { let v = gate.view(); v.frozen && v.draining == 0 }) {
                                Ok(Some(state)) => {
                                    let _ = app_handle.emit("clamshell-state", state);
                                    let _ = app_handle.emit("clamshell-error", Option::<AppError>::None);
                                }
                                Ok(None) => {}
                                Err(err) => {
                                    let _ = app_handle.emit("clamshell-error", Some(err));
                                }
                            }
                        });
                    }
                }
            });
            #[cfg(target_os = "macos")]
            if start_silently {
                app.set_activation_policy(tauri::ActivationPolicy::Accessory);
            }
            #[cfg(target_os = "macos")]
            app.set_menu(application_menu(app.handle())?)?;
            for probe_gateway in [
                runtime.gateway.clone(),
                runtime.claude.clone(),
                runtime.grok.clone(),
                runtime.dsh.clone(),
            ] {
                let mut probe_freeze = probe_gateway.freeze_gate().subscribe();
                tauri::async_runtime::spawn(async move {
                    loop {
                        if probe_gateway
                            .next_endpoint_probe_wait()
                            .is_some_and(|w| w.is_zero())
                        {
                            probe_gateway.probe_due_endpoints().await;
                        }
                        let sleep_for = probe_gateway
                            .next_endpoint_probe_wait()
                            .map(|w| w.max(std::time::Duration::from_secs(1)));
                        tokio::select! {
                            _ = probe_gateway.wait_endpoint_probe_wake() => {}
                            res = probe_freeze.changed() => if res.is_err() { break; },
                            _ = async {
                                if let Some(d) = sleep_for {
                                    tokio::time::sleep(d).await;
                                } else {
                                    std::future::pending::<()>().await;
                                }
                            } => {}
                        }
                    }
                });
            }
            for g in [runtime.gateway.clone(), runtime.claude.clone(), runtime.grok.clone(), runtime.dsh.clone()] {
                let mut quota_events = g.quota_events();
                let quota_app = app.handle().clone();
                let client_id = g.client_id();
                tauri::async_runtime::spawn(async move {
                    loop {
                        match quota_events.recv().await {
                            Ok(view) => {
                                let windows = visible_windows(&quota_app);
                                if !windows.is_empty() {
                                    let payload = serde_json::json!({"clientId":client_id,"quota":view});
                                    for window in windows {
                                        let _ = window.emit("provider-quota", &payload);
                                    }
                                }
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                            Err(_) => break,
                        }
                    }
                });
                let mut events = g.subscribe();
                let handle = app.handle().clone();
                tauri::async_runtime::spawn(async move {
                    let mut waiting = g.has_health_wait();
                    loop {
                        tokio::select! {
                            event = events.recv() => {
                                if matches!(event, Err(tokio::sync::broadcast::error::RecvError::Closed)) {
                                    break;
                                }
                                coalesce_gateway_events(&mut events);
                                waiting = g.has_health_wait();
                            }
                            _ = tokio::time::sleep(std::time::Duration::from_secs(1)), if waiting => {
                                let current = g.has_health_wait();
                                let refresh = waiting || current;
                                waiting = current;
                                // Include the final zero so an idle open circuit
                                // becomes visibly ready for a request probe.
                                if !refresh { continue; }
                            }
                        }
                        g.report_diagnostics();
                        publish_gateway(&handle, &g);
                    }
                });
            }
            let mut tray = TrayIconBuilder::with_id("switch")
                .menu(&tray_menu(app.handle())?)
                .show_menu_on_left_click(false)
                .tooltip("Vela");
            #[cfg(target_os = "macos")]
            {
                tray = tray
                    .icon(tauri::image::Image::from_bytes(include_bytes!(
                        "../icons/tray.png"
                    ))?)
                    .icon_as_template(true);
            }
            #[cfg(not(target_os = "macos"))]
            {
                let bytes = if cfg!(windows) {
                    #[cfg(windows)]
                    {
                        if tray_is_dark() {
                            include_bytes!("../icons/tray-white.png").as_slice()
                        } else {
                            include_bytes!("../icons/tray-black.png").as_slice()
                        }
                    }
                    #[cfg(not(windows))]
                    {
                        include_bytes!("../icons/tray-white.png").as_slice()
                    }
                } else {
                    include_bytes!("../icons/tray-white.png").as_slice()
                };
                tray = tray.icon(tauri::image::Image::from_bytes(bytes)?);
            }
            let tray = tray
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button,
                        button_state,
                        rect,
                        ..
                    } = event
                    {
                        if button == MouseButton::Left && button_state == MouseButtonState::Up {
                            let app = tray.app_handle().clone();
                            let handle = app.clone();
                            let _ = app.run_on_main_thread(move || {
                                if let Err(e) = quick::Panel::show(&handle, Some(rect)) {
                                    let _ = handle.emit("switch-error", e);
                                }
                            });
                        }
                        #[cfg(target_os = "macos")]
                        if button == MouseButton::Right && button_state == MouseButtonState::Down {
                            if let Err(e) = tray_macos::context_menu(tray) {
                                let _ = tray.app_handle().emit("switch-error", e);
                            }
                        }
                    }
                })
                .on_menu_event(|app, e| match e.id().as_ref() {
                    "quit" => quit(app, &app.state::<Arc<Runtime>>()),
                    "open" => {
                        let _ = open_main(app.clone(), None, None, None, None, None);
                    }
                    "config" | "gateway" | "settings" => {
                        let _ = open_main(app.clone(), Some(e.id().as_ref().into()), None, None, None, None);
                    }
                    "github" => {
                        let _ = update::open_repository();
                    }
                    "check_update" => {
                        let handle = app.clone();
                        tauri::async_runtime::spawn(async move {
                            match update::latest().await {
                                Ok(info) if info.has_update => {
                                    let version = info.latest_version.as_deref().unwrap_or("latest");
                                    let _ = handle.emit("switch-notice", format!("发现新版本 v{version}"));
                                    let _ = update::open_releases();
                                }
                                Ok(_) => {
                                    let _ = handle.emit("switch-notice", "已是最新版本".to_string());
                                }
                                Err(error) => {
                                    let _ = handle.emit("switch-error", error);
                                }
                            }
                        });
                    }
                    _ => (),
                })
                .build(app)?;
            #[cfg(target_os = "macos")]
            tray_macos::install(&tray)?;
            #[cfg(not(target_os = "macos"))]
            let _ = tray;
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let mut last = Some(state);
                let mut config_versions = std::collections::HashMap::new();
                let mut config_cache = std::collections::HashMap::new();
                let mut agy_config_version: Option<String> = None;
                let mut agy_config_cache = auth_cache::Cache::default();
                let mut agy_auth_stamp: Option<String> = None;
                let mut agy_auth_revision: Option<String> = None;
                #[cfg(windows)]
                let mut last_tray_dark: Option<bool> = None;
                loop {
                    let has_visible = !visible_windows(&handle).is_empty();
                    wait_next_background_poll(has_visible, &runtime.window_wake).await;
                    if runtime.quitting.load(Ordering::Relaxed) {
                        break;
                    }
                    match poll_account_state_step(
                        &runtime.core,
                        &runtime.gateway,
                        &mut last,
                    ) {
                        PollStateUpdate::StateChanged(s) => {
                            publish_gateway(&handle, &runtime.gateway);
                            publish(&handle, s);
                        }
                        PollStateUpdate::ErrorChanged(s) => {
                            publish(&handle, s);
                        }
                        PollStateUpdate::Unchanged => {}
                    }
                    if should_poll_editor_configs(
                        !visible_windows(&handle).is_empty(),
                        runtime.background_poll_active(),
                    ) {
                        for client in [
                            gateway::ClientId::Codex,
                            gateway::ClientId::Claude,
                            gateway::ClientId::Grok,
                            gateway::ClientId::Dsh,
                        ] {
                            if let Ok(stamp) = runtime.home(client).and_then(|home| {
                                runtime
                                    .gateway(client)
                                    .config_stamp(&home, config_cache.entry(client).or_default())
                            }) {
                                if config_versions.get(&client) != Some(&stamp) {
                                    let key = serde_json::to_string(&client).unwrap_or_default();
                                    emit_main_config_state(
                                        &handle,
                                        &key,
                                        serde_json::json!({"clientId":client,"revision":stamp.1,"guarded":stamp.2}),
                                    );
                                    config_versions.insert(client, stamp);
                                    if client != gateway::ClientId::Codex {
                                        publish_gateway(&handle, runtime.gateway(client));
                                    }
                                }
                            }
                        }
                        if let Ok(rev) = antigravity::home()
                            .and_then(|home| antigravity::config_stamp(&home, &mut agy_config_cache))
                        {
                            if agy_config_version.as_ref() != Some(&rev) {
                                emit_main_config_state(
                                    &handle,
                                    "antigravity",
                                    serde_json::json!({"clientId":"antigravity","revision":rev,"guarded":false}),
                                );
                                agy_config_version = Some(rev);
                            }
                        }
                        if let Some(stamp) = antigravity::auth::disk_stamp() {
                            if agy_auth_stamp.as_ref() != Some(&stamp) {
                                if let Ok((auth_state, clean)) =
                                    antigravity::auth::poll_state(&runtime.data)
                                {
                                    if agy_auth_revision.as_ref() != Some(&auth_state.revision) {
                                        agy_auth_revision = Some(auth_state.revision.clone());
                                        let _ = handle.emit("antigravity-auth-state", auth_state);
                                    }
                                    if clean {
                                        agy_auth_stamp = Some(stamp);
                                    }
                                }
                            }
                        }
                    }
                    #[cfg(windows)]
                    {
                        let dark = tray_is_dark();
                        if last_tray_dark != Some(dark) {
                            if let Some(tray) = handle.tray_by_id("switch") {
                                let bytes = if dark {
                                    include_bytes!("../icons/tray-white.png").as_slice()
                                } else {
                                    include_bytes!("../icons/tray-black.png").as_slice()
                                };
                                if let Ok(image) = tauri::image::Image::from_bytes(bytes) {
                                    if tray.set_icon(Some(image)).is_ok() {
                                        last_tray_dark = Some(dark);
                                    }
                                }
                            }
                        }
                    }
                }
            });
            Ok(())
        })
        .on_menu_event(|app, event| {
            if event.id().as_ref() == "app-quit" {
                let r = app.state::<Arc<Runtime>>();
                quit(app, &r);
            }
        })
        .on_window_event(|w, event| {
            let visible = w.is_visible().unwrap_or(false) && !w.is_minimized().unwrap_or(false);
            if matches!(event, tauri::WindowEvent::Focused(true))
                || (matches!(event, tauri::WindowEvent::Focused(false)) && !visible)
                || (matches!(event, tauri::WindowEvent::Resized(_)) && w.is_minimized().unwrap_or(false)) {
                let _ = w.emit(if w.label() == "quick" { "panel-visibility" } else { "app-visibility" }, visible);
                if visible {
                    sync_window_on_show(w.app_handle(), w.label());
                } else if let Some(r) = w.app_handle().try_state::<Arc<Runtime>>() {
                    r.mark_window_visible(w.label(), false);
                }
            }
            if w.label() == "quick" {
                match event {
                    tauri::WindowEvent::Focused(focused) => {
                        quick::Panel::focus(w.app_handle(), *focused)
                    }
                    tauri::WindowEvent::CloseRequested { api, .. } => {
                        api.prevent_close();
                        let _ = quick::hide_quick(w.app_handle().clone());
                    }
                    _ => (),
                }
                return;
            }
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                let r = w.state::<Arc<Runtime>>();
                if !r.quitting.load(Ordering::Relaxed) {
                    api.prevent_close();
                    let _ = w.hide();
                    let _ = w.emit("app-visibility", false);
                    r.mark_window_visible("main", false);
                    #[cfg(target_os = "macos")]
                    let _ = w
                        .app_handle()
                        .set_activation_policy(tauri::ActivationPolicy::Accessory);
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            antigravity::read_antigravity_config,
            antigravity::save_antigravity_config,
            antigravity::read_previous_antigravity_config,
            antigravity::validate_antigravity_config,
            antigravity::auth::get_antigravity_auth_state,
            antigravity::auth::add_antigravity_adc,
            antigravity::auth::save_antigravity_vertex_key,
            antigravity::auth::pick_antigravity_credential_file,
            antigravity::auth::switch_antigravity_account,
            antigravity::auth::set_antigravity_auth_mode,
            antigravity::auth::rename_antigravity_account,
            antigravity::auth::delete_antigravity_account,
            usage::commands::get_usage_state,
            usage::commands::export_usage_bill,
            usage::commands::get_usage_dashboard,
            usage::commands::get_antigravity_usage_stats,
            usage::commands::get_claude_usage_stats,
            usage::commands::get_claude_usage_quota,
            usage::commands::get_usage_logs,
            usage::commands::get_usage_detail,
            usage::commands::set_usage_settings,
            usage::commands::sync_usage,
            usage::commands::get_pricing,
            usage::commands::search_pricing,
            usage::commands::get_models_dev,
            usage::commands::get_antigravity_vertex_models,
            usage::commands::update_models_dev,
            usage::commands::compare_model_prices,
            usage::commands::configure_pricing,
            usage::commands::update_pricing,
            usage::commands::reload_pricing,
            usage::commands::open_pricing_directory,
            provider_testing::commands::provider_test_state,
            provider_testing::commands::provider_test_start,
            provider_testing::commands::provider_test_save_prompt,
            provider_testing::commands::provider_test_set_platform,
            provider_testing::commands::provider_test_query,
            provider_testing::commands::provider_test_cancel,
            provider_testing::commands::provider_test_retry,
            provider_testing::commands::provider_test_set_detection,
            provider_testing::commands::provider_test_clear_mark,
            provider_testing::commands::provider_test_models,
            provider_testing::commands::provider_test_update_bank,
            links::get_provider_imports,
            links::confirm_provider_import,
            links::cancel_provider_import,
            commands::get_clamshell_state,
            commands::set_clamshell_awake,
            commands::install_power_helper,
            commands::remove_power_helper,
            commands::force_quit_agent_clients,
            commands::get_startup_error,
            commands::get_app_events,
            commands::get_app_event,
            commands::get_app_event_raw,
            commands::clear_app_events,
            claude_profile::commands::get_claude_profile,
            claude_profile::commands::switch_claude_profile,
            claude_profile::commands::recover_claude_profile,
            claude_profile::commands::claude_login_status,
            claude_profile::commands::start_claude_login,
            claude_profile::commands::cancel_claude_login,
            notifications::notification_permission,
            notifications::test_notification,
            notifications::open_notification_settings,
            open_main,
            quick::get_quick,
            quick::set_quick,
            quick::hide_quick,
            get_gateway,
            get_gateway_freeze,
            set_gateway_frozen,
            cancel_gateway_wait,
            get_failure_policy_catalog,
            match_failure_policy,
            preview_grok_import,
            get_gateway_models,
            refresh_gateway_models,
            search_models_dev,
            edit_model_list,
            dsh_official_models,
            preview_dsh_profile,
            attach_dsh_profiles,
            read_dsh_file,
            read_previous_dsh_file,
            save_dsh_file,
            restore_dsh_file,
            cancel_grok_import,
            commit_grok_import,
            reveal_provider_token,
            probe_provider_endpoints,
            update_gateway,
            commands::preview_account_import,
            commands::commit_account_import,
            commands::cancel_account_exchange,
            commands::preview_account_export,
            commands::prepare_account_template,
            commands::select_account_template,
            commands::export_accounts,
            start_gateway,
            stop_gateway,
            query_provider_quota,
            list_provider_models,
            get_state,
            switch_account,
            use_official_account,
            disable_official_account,
            import_current,
            rename_account,
            delete_account,
            read_config,
            read_previous_config,
            validate_config,
            save_config,
            set_preferences,
            set_quota_refresh,
            get_startup,
            set_startup,
            get_login,
            start_login,
            cancel_login,
            open_login_url,
            copy_login_value,
            complete_login_callback,
            check_for_updates,
            open_github,
            open_update_release,
            frontend_ready
        ])
        .build(context)
        .expect("Vela initialization failed")
        .run(|app, event| {
            #[cfg(target_os = "macos")]
            if let tauri::RunEvent::Opened { urls } = &event {
                links::receive(app, urls.iter().map(|url| url.as_str().to_owned()));
            }
            #[cfg(target_os = "macos")]
            if let tauri::RunEvent::Reopen { .. } = &event {
                if !startup::macos_login_event() {
                    let _ = show(app, None);
                }
            }
            if let tauri::RunEvent::ExitRequested { api, .. } = &event {
                let r = app.state::<Arc<Runtime>>();
                if !r.quitting.load(Ordering::Relaxed) {
                    api.prevent_exit();
                    quit(app, &r);
                }
            }
            if let tauri::RunEvent::Exit = event {
                let r = app.state::<Arc<Runtime>>();
                // macOS Dock/system termination can bypass ExitRequested.
                // Serialize restoration with any in-flight takeover before returning to the OS.
                let restored = tauri::async_runtime::block_on(r.stop_gateways());
                if r.smoke.is_some() {
                    let _ = smoke::report_exit(app, &r, restored);
                }
            }
        });
}

#[cfg(test)]
mod page_whitelist_tests {
    use super::{
        core::Core, emit_account_sync_diagnostic, events, gateway, poll_account_state_step,
        should_poll_editor_configs, tray_tooltip_text, valid_main_page, wait_next_background_poll,
        MainSignal, PendingMainEvents, PollStateUpdate, HIDDEN_POLL_INTERVAL,
        VISIBLE_POLL_INTERVAL,
    };
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    #[test]
    fn allows_usage_and_all_primary_pages_while_rejecting_unknown_pages() {
        for page in ["accounts", "config", "gateway", "logs", "usage", "settings"] {
            assert!(valid_main_page(page), "{page} should be allowed");
        }
        assert!(!valid_main_page("invalid"));
    }

    #[test]
    fn pending_main_events_coalesce_while_hidden_and_drain_once_on_show() {
        let pending = PendingMainEvents::default();
        pending.record_signal(MainSignal::ProviderTests);
        pending.record_signal(MainSignal::ProviderTests);
        pending.record_signal(MainSignal::PricingState);
        pending.record_signal(MainSignal::UsageState);
        pending.record_signal(MainSignal::UsageState);
        pending.record_config_state(
            "claude",
            serde_json::json!({"clientId":"claude","revision":"r1","guarded":false}),
        );
        pending.record_config_state(
            "claude",
            serde_json::json!({"clientId":"claude","revision":"r2","guarded":true}),
        );
        pending.record_config_state(
            "antigravity",
            serde_json::json!({"clientId":"antigravity","revision":"a1","guarded":false}),
        );
        pending.record_app_event(&events::Change {
            record: None,
            error: Some("first".into()),
            notify: true,
        });
        pending.record_app_event(&events::Change {
            record: None,
            error: Some("second".into()),
            notify: true,
        });
        pending.record_notification_state(crate::notifications::State {
            permission: "granted".into(),
            error: None,
            delivery: crate::notifications::delivery::Status::Pending,
        });
        pending.record_notification_state(crate::notifications::State {
            permission: "granted".into(),
            error: None,
            delivery: crate::notifications::delivery::Status::Accepted,
        });

        let drained = pending.drain();
        assert!(drained.provider_tests);
        assert!(drained.pricing_state);
        assert!(drained.usage_state);
        assert_eq!(drained.app_event.unwrap().error.as_deref(), Some("second"));
        assert_eq!(
            drained.notification_state,
            Some(crate::notifications::State {
                permission: "granted".into(),
                error: None,
                delivery: crate::notifications::delivery::Status::Accepted,
            })
        );
        assert_eq!(drained.config_states.len(), 2);
        assert!(drained
            .config_states
            .iter()
            .any(|v| v["clientId"] == "claude" && v["revision"] == "r2" && v["guarded"] == true));
        assert!(drained
            .config_states
            .iter()
            .any(|v| v["clientId"] == "antigravity" && v["revision"] == "a1"));

        let drained_again = pending.drain();
        assert!(drained_again.app_event.is_none());
        assert!(drained_again.notification_state.is_none());
        assert!(!drained_again.provider_tests);
        assert!(!drained_again.pricing_state);
        assert!(!drained_again.usage_state);
        assert!(drained_again.config_states.is_empty());
    }

    #[test]
    fn hidden_main_window_buffers_notification_state_without_leaking_to_quick_window() {
        let pending = PendingMainEvents::default();
        pending.record_notification_state(crate::notifications::State {
            permission: "granted".into(),
            error: None,
            delivery: crate::notifications::delivery::Status::Pending,
        });
        pending.record_notification_state(crate::notifications::State {
            permission: "denied".into(),
            error: Some("请在系统设置开启通知".into()),
            delivery: crate::notifications::delivery::Status::Failed,
        });
        pending.record_app_event(&events::Change {
            record: None,
            error: Some("background-notice".into()),
            notify: true,
        });

        // Showing the quick window must not consume notification_state or app_event buffered for main.
        assert!(pending.drain_for_window("quick").is_none());

        // Showing the main window replays the latest notification_state and strips notify=true on app_event.
        let main_drained = pending
            .drain_for_window("main")
            .expect("main window should drain pending notifications");
        assert_eq!(
            main_drained.notification_state,
            Some(crate::notifications::State {
                permission: "denied".into(),
                error: Some("请在系统设置开启通知".into()),
                delivery: crate::notifications::delivery::Status::Failed,
            })
        );
        let replayed_event = main_drained
            .app_event
            .expect("app_event should be replayed");
        assert_eq!(replayed_event.error.as_deref(), Some("background-notice"));
        assert!(
            !replayed_event.notify,
            "replayed hidden app_event must not re-trigger system notifications"
        );
        assert!(pending
            .drain_for_window("main")
            .unwrap()
            .notification_state
            .is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn background_poll_uses_two_speed_interval_and_wakes_immediately_on_window_show() {
        let wake = Arc::new(tokio::sync::Notify::new());

        // Visible mode polls every 2s.
        let wake_visible = wake.clone();
        let visible_wait =
            tokio::spawn(async move { wait_next_background_poll(true, &wake_visible).await });
        tokio::time::advance(Duration::from_millis(1900)).await;
        tokio::task::yield_now().await;
        assert!(!visible_wait.is_finished());
        tokio::time::advance(Duration::from_millis(150)).await;
        assert_eq!(VISIBLE_POLL_INTERVAL, Duration::from_secs(2));
        visible_wait.await.unwrap();

        // Hidden tray-resident mode never hangs forever: it completes after 10s even when idle.
        let wake_hidden = wake.clone();
        let hidden_wait =
            tokio::spawn(async move { wait_next_background_poll(false, &wake_hidden).await });
        tokio::time::advance(Duration::from_secs(9)).await;
        tokio::task::yield_now().await;
        assert!(!hidden_wait.is_finished());
        tokio::time::advance(Duration::from_secs(1)).await;
        assert_eq!(HIDDEN_POLL_INTERVAL, Duration::from_secs(10));
        hidden_wait.await.unwrap();

        // Showing a window notifies wake and interrupts the 10s hidden sleep immediately.
        let wake_early = wake.clone();
        let early_wait =
            tokio::spawn(async move { wait_next_background_poll(false, &wake_early).await });
        tokio::task::yield_now().await;
        wake.notify_one();
        tokio::time::timeout(Duration::from_millis(50), early_wait)
            .await
            .expect("window wake should interrupt hidden sleep immediately")
            .unwrap();

        // UI-only editor config stamps skip only when hidden AND idle.
        assert!(!should_poll_editor_configs(false, false));
        assert!(should_poll_editor_configs(true, false));
        assert!(should_poll_editor_configs(false, true));
    }

    #[test]
    fn hidden_poll_step_still_syncs_external_auth_updates_tooltip_and_emits_account_diagnostics() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let home = dir.path().join("codex-home");
        std::fs::create_dir_all(&home).unwrap();

        let core = Mutex::new(Core::new(data.clone(), home.clone()).unwrap());
        let gateway = gateway::Gateway::new(data.join("gateway")).unwrap();
        let diagnostics = events::Service::new(&data);
        let mut last = None;

        // Initial missing auth.json poll updates state without importing an account.
        match poll_account_state_step(&core, &gateway, &mut last) {
            PollStateUpdate::StateChanged(initial) => {
                assert_eq!(tray_tooltip_text(&initial), "Vela · 未保存账号");
            }
            _ => panic!("expected initial state update"),
        }

        // External terminal login writes a valid API key auth.json while windows remain hidden.
        crate::storage::atomic_write(
            &home.join("auth.json"),
            br#"{"auth_mode":"apikey","OPENAI_API_KEY":"sk-fixture-external-login"}"#,
            None,
        )
        .unwrap();
        let synced = loop {
            match poll_account_state_step(&core, &gateway, &mut last) {
                PollStateUpdate::StateChanged(state) => break state,
                PollStateUpdate::ErrorChanged(_) => continue,
                PollStateUpdate::Unchanged => panic!("expected external auth.json sync"),
            }
        };
        assert_eq!(synced.accounts.len(), 1);
        assert!(synced.accounts[0].current);
        assert_eq!(tray_tooltip_text(&synced), "Vela · API Key");
        assert!(!emit_account_sync_diagnostic(&diagnostics, &synced));

        // Corrupting auth.json while hidden triggers AccountSync diagnostic emission.
        crate::storage::atomic_write(&home.join("auth.json"), b"{invalid-json", None).unwrap();
        let broken = loop {
            match poll_account_state_step(&core, &gateway, &mut last) {
                PollStateUpdate::StateChanged(state) => break state,
                PollStateUpdate::ErrorChanged(_) => continue,
                PollStateUpdate::Unchanged => panic!("expected invalid auth.json state update"),
            }
        };
        assert!(emit_account_sync_diagnostic(&diagnostics, &broken));
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            let page = diagnostics.query(events::Filter::default());
            if page
                .items
                .iter()
                .any(|item| item.record.reason == events::Reason::AccountSync)
            {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "AccountSync diagnostic was not persisted"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
