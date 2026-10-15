//! Tray panel lifecycle, following Sub2Ops 4d0eafc1 interaction behavior.
use crate::errors::code;
use crate::storage::{self, AppError, Result};
use serde::{Deserialize, Serialize};
use std::{path::PathBuf, sync::Mutex, time::Duration};
use tauri::{Emitter, Manager, WebviewUrl, WebviewWindowBuilder};

pub const QUICK_DEFAULT_HEIGHT: f64 = 720.;

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Preferences {
    pub pinned: bool,
    pub tab: String,
}
impl Default for Preferences {
    fn default() -> Self {
        Self {
            pinned: false,
            tab: "providers".into(),
        }
    }
}
#[derive(Default)]
struct Lifecycle {
    generation: u64,
    focused: bool,
    showing: bool,
    anchor: Option<tauri::Rect>,
    height: f64,
}
pub struct Panel {
    path: PathBuf,
    preferences: Mutex<Preferences>,
    lifecycle: Mutex<Lifecycle>,
}
impl Panel {
    pub fn new(data: &std::path::Path) -> Result<Self> {
        let path = data.join("quick.json");
        let preferences = storage::read_optional(&path)?
            .map(|raw| {
                serde_json::from_slice(&raw)
                    .map_err(|_| AppError::new(code::PANEL, "快捷面板设置无法读取"))
            })
            .transpose()?
            .unwrap_or_default();
        Ok(Self {
            path,
            preferences: Mutex::new(preferences),
            lifecycle: Mutex::new(Lifecycle {
                height: QUICK_DEFAULT_HEIGHT,
                ..Default::default()
            }),
        })
    }
    pub fn create(app: &tauri::AppHandle) -> tauri::Result<()> {
        WebviewWindowBuilder::new(
            app,
            "quick",
            WebviewUrl::App("index.html?panel=quick".into()),
        )
        .title("lich13-switch 快捷面板")
        .inner_size(420., QUICK_DEFAULT_HEIGHT)
        .decorations(false)
        .resizable(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .visible(false)
        .focused(false)
        .build()?;
        Ok(())
    }
    pub fn show(app: &tauri::AppHandle, anchor: Option<tauri::Rect>) -> Result<()> {
        let window = app
            .get_webview_window("quick")
            .ok_or_else(|| AppError::new(code::PANEL, "快捷面板未创建"))?;
        let anchor = anchor
            .or_else(|| {
                app.tray_by_id("switch")
                    .and_then(|t| t.rect().ok().flatten())
            })
            .ok_or_else(|| AppError::new(code::PANEL, "无法定位托盘图标"))?;
        let panel = app.state::<Panel>();
        let height = {
            let mut life = panel.lifecycle.lock().unwrap();
            life.generation += 1;
            life.showing = true;
            life.anchor = Some(anchor);
            life.height
        };
        let result = (|| -> Result<()> {
            position(&window, anchor, height)?;
            #[cfg(target_os = "macos")]
            app.show()
                .map_err(|_| AppError::new(code::PANEL, "无法显示快捷面板"))?;
            window
                .show()
                .map_err(|_| AppError::new(code::PANEL, "无法显示快捷面板"))?;
            window
                .set_focus()
                .map_err(|_| AppError::new(code::PANEL, "无法聚焦快捷面板"))?;
            let _ = window.emit("panel-visibility", true);
            Ok(())
        })();
        let mut life = panel.lifecycle.lock().unwrap();
        life.showing = false;
        life.focused = window.is_focused().unwrap_or(false);
        result
    }
    pub fn focus(app: &tauri::AppHandle, focused: bool) {
        let panel = app.state::<Panel>();
        let generation = {
            let mut life = panel.lifecycle.lock().unwrap();
            if !focused && (!life.focused || life.showing) {
                return;
            }
            life.generation += 1;
            life.focused = focused;
            life.generation
        };
        if focused {
            return;
        }
        let app = app.clone();
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            let handle = app.clone();
            let _ = app.run_on_main_thread(move || {
                let panel = handle.state::<Panel>();
                if panel.preferences.lock().unwrap().pinned {
                    return;
                }
                let life = panel.lifecycle.lock().unwrap();
                if life.generation != generation || life.focused || life.showing {
                    return;
                }
                drop(life);
                if handle
                    .get_webview_window("quick")
                    .is_some_and(|w| !w.is_focused().unwrap_or(true))
                {
                    let _ = hide_quick(handle);
                }
            });
        });
    }
}
fn bounds(
    anchor: (f64, f64, f64, f64),
    area: (f64, f64, f64, f64),
    scale: f64,
    height: f64,
) -> (f64, f64, f64, f64) {
    let (ax, ay, aw, ah) = anchor;
    let (x, y, w, h) = area;
    let width = (420. * scale).min(w);
    let height = (height.clamp(128., 720.) * scale).min(h);
    let left = (ax + aw / 2. - width / 2.).clamp(x, x + w - width);
    let top = if ay >= y + h / 2. {
        ay - height - 4. * scale
    } else {
        ay + ah + 4. * scale
    };
    (left, top.clamp(y, y + h - height), width, height)
}
fn position(window: &tauri::WebviewWindow, anchor: tauri::Rect, height: f64) -> Result<()> {
    let p = anchor.position.to_physical::<f64>(1.);
    let size = anchor.size.to_physical::<f64>(1.);
    let monitors = window
        .available_monitors()
        .map_err(|_| AppError::new(code::PANEL, "无法读取屏幕区域"))?;
    let monitor = monitors
        .iter()
        .find(|m| {
            let mp = m.position();
            let ms = m.size();
            p.x >= mp.x as f64
                && p.x < mp.x as f64 + ms.width as f64
                && p.y >= mp.y as f64
                && p.y < mp.y as f64 + ms.height as f64
        })
        .or_else(|| monitors.first())
        .ok_or_else(|| AppError::new(code::PANEL, "没有可用屏幕"))?;
    let area = monitor.work_area();
    let (x, y, width, height) = bounds(
        (p.x, p.y, size.width, size.height),
        (
            area.position.x as f64,
            area.position.y as f64,
            area.size.width as f64,
            area.size.height as f64,
        ),
        monitor.scale_factor(),
        height,
    );
    window
        .set_size(tauri::PhysicalSize::new(width, height))
        .and_then(|_| window.set_position(tauri::PhysicalPosition::new(x, y)))
        .map_err(|_| AppError::new(code::PANEL, "无法定位快捷面板"))
}
#[tauri::command]
pub fn get_quick(app: tauri::AppHandle, panel: tauri::State<'_, Panel>) -> serde_json::Value {
    let prefs = panel.preferences.lock().unwrap().clone();
    serde_json::json!({"pinned": prefs.pinned, "tab": prefs.tab, "visible": app.get_webview_window("quick").is_some_and(|w| w.is_visible().unwrap_or(false))})
}
#[tauri::command]
pub fn set_quick(
    app: tauri::AppHandle,
    panel: tauri::State<'_, Panel>,
    pinned: Option<bool>,
    tab: Option<String>,
) -> Result<Preferences> {
    let mut prefs = panel.preferences.lock().unwrap();
    let mut next = prefs.clone();
    if let Some(pinned) = pinned {
        next.pinned = pinned;
    }
    if let Some(tab) = tab {
        if !["providers", "accounts"].contains(&tab.as_str()) {
            return Err(AppError::new(code::PANEL, "无效快捷页面"));
        }
        next.tab = tab;
    }
    storage::atomic_write(
        &panel.path,
        &serde_json::to_vec(&next).map_err(|_| AppError::new(code::PANEL, "无法保存快捷设置"))?,
        None,
    )?;
    *prefs = next.clone();
    let _ = app.emit("quick-state", &next);
    Ok(next)
}
#[tauri::command]
pub fn hide_quick(app: tauri::AppHandle) -> Result<()> {
    {
        let panel = app.state::<Panel>();
        let mut life = panel.lifecycle.lock().unwrap();
        life.generation += 1;
        life.focused = false;
        life.showing = false;
    }
    if let Some(w) = app.get_webview_window("quick") {
        w.hide()
            .map_err(|_| AppError::new(code::PANEL, "无法关闭快捷面板"))?;
        let _ = w.emit("panel-visibility", false);
    }
    Ok(())
}
#[tauri::command]
pub fn resize_quick(
    app: tauri::AppHandle,
    window: tauri::WebviewWindow,
    height: f64,
) -> Result<()> {
    if window.label() != "quick" || !height.is_finite() {
        return Err(AppError::new(code::PANEL, "窗口尺寸无效"));
    }
    let panel = app.state::<Panel>();
    let anchor = {
        let mut life = panel.lifecycle.lock().unwrap();
        life.height = height.clamp(128., QUICK_DEFAULT_HEIGHT);
        life.anchor
    };
    if let Some(anchor) = anchor {
        position(&window, anchor, height)?;
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn panel_fits_secondary_and_scaled_taskbars() {
        for anchor in [
            (2500., 0., 22., 22.),
            (3800., 2100., 44., 44.),
            (-1200., 10., 22., 22.),
        ] {
            let area = if anchor.0 < 0. {
                (-1440., 24., 1440., 876.)
            } else {
                (1920., 48., 1920., 2072.)
            };
            let (x, y, w, h) = bounds(anchor, area, 2., 720.);
            assert!(
                x >= area.0 && y >= area.1 && x + w <= area.0 + area.2 && y + h <= area.1 + area.3
            );
        }
    }
    #[test]
    fn taller_panel_clamps_to_work_area_without_padding_short_content() {
        let anchor = (500., 0., 22., 22.);
        let large = (0., 24., 1440., 900.);
        assert_eq!(bounds(anchor, large, 1., 1000.).3, 720.);
        assert_eq!(bounds(anchor, large, 1., 250.).3, 250.);
        let (x, y, w, h) = bounds(anchor, (0., 24., 760., 480.), 1., 720.);
        assert_eq!(h, 480.);
        assert!(x >= 0. && x + w <= 760. && y >= 24. && y + h <= 504.);
    }
}
