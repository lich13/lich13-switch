//! Transient NSMenu attachment for macOS 27, as used by Sub2Ops 4d0eafc1.
use crate::errors::code;
use crate::storage::{AppError, Result};
use objc2::{rc::Retained, MainThreadMarker};
use objc2_app_kit::NSMenu;
use std::cell::RefCell;
thread_local! { static CONTEXT_MENU: RefCell<Option<Retained<NSMenu>>> = const { RefCell::new(None) }; }
pub fn install(tray: &tauri::tray::TrayIcon) -> Result<()> {
    tray.with_inner_tray_icon(|inner| -> Result<()> {
        let main =
            MainThreadMarker::new().ok_or_else(|| AppError::new(code::TRAY, "托盘需要主线程"))?;
        inner.set_show_menu_on_right_click(false);
        let status = inner
            .ns_status_item()
            .ok_or_else(|| AppError::new(code::TRAY, "托盘图标不可用"))?;
        CONTEXT_MENU.with(|saved| saved.replace(status.menu(main)));
        status.setMenu(None);
        Ok(())
    })
    .map_err(|_| AppError::new(code::TRAY, "无法初始化托盘菜单"))?
}
pub fn context_menu(tray: &tauri::tray::TrayIcon) -> Result<()> {
    tray.with_inner_tray_icon(|inner| -> Result<()> {
        let main =
            MainThreadMarker::new().ok_or_else(|| AppError::new(code::TRAY, "托盘需要主线程"))?;
        let status = inner
            .ns_status_item()
            .ok_or_else(|| AppError::new(code::TRAY, "托盘图标不可用"))?;
        let button = status
            .button(main)
            .ok_or_else(|| AppError::new(code::TRAY, "托盘按钮不可用"))?;
        let menu = CONTEXT_MENU
            .with(|saved| saved.borrow().clone())
            .ok_or_else(|| AppError::new(code::TRAY, "托盘菜单不可用"))?;
        status.setMenu(Some(&menu));
        unsafe {
            button.performClick(None);
        }
        status.setMenu(None);
        Ok(())
    })
    .map_err(|_| AppError::new(code::TRAY, "无法打开托盘菜单"))?
}
