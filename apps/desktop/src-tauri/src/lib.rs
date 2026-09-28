//! TermBridge 桌面前端壳：真实命令层 + 托盘 + 关闭窗口行为。
//!
//! 会话 / 主机逻辑由 crates/app（后端所有者）提供；
//! 本 crate 只做 Tauri IPC 装配与桌面生命周期管理。

mod commands;
mod state;

use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, TrayIconBuilder, TrayIconEvent};
use tauri::Manager;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .manage(state::AppState::default())
        .invoke_handler(tauri::generate_handler![
            commands::list_profiles,
            commands::save_profile,
            commands::remove_profile,
            commands::store_profile_password,
            commands::key_passphrase_required,
            commands::host_status,
            commands::init_host,
            commands::switch_host_to_keys,
            commands::set_host_enabled,
            commands::probe_host,
            commands::confirm_host_fingerprint,
            commands::list_sessions,
            commands::list_remote_sessions,
            commands::connect_existing_session,
            commands::create_session,
            commands::attach_session,
            commands::detach_session,
            commands::end_session,
            commands::send_input,
            commands::resize_session,
            commands::take_control,
            hide_to_tray
        ])
        .setup(|app| {
            let show = MenuItem::with_id(app, "show", "显示 TermBridge", true, None::<&str>)?;
            let stop = MenuItem::with_id(app, "stop_host", "退出 TermBridge（停止本 GUI 接收端）", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show, &stop])?;
            TrayIconBuilder::with_id("termbridge-tray")
                .icon(app.default_window_icon().unwrap().clone())
                .tooltip("TermBridge")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_tray_icon_event(|tray, event| {
                    if matches!(event, TrayIconEvent::Click { button: MouseButton::Left, .. }) {
                        if let Some(win) = tray.app_handle().get_webview_window("main") {
                            let _ = win.unminimize();
                            let _ = win.show();
                            let _ = win.set_focus();
                        }
                    }
                })
                .on_menu_event(|app, event| match event.id().as_ref() {
                    "show" => {
                        if let Some(win) = app.get_webview_window("main") {
                            let _ = win.unminimize();
                            let _ = win.show();
                            let _ = win.set_focus();
                        }
                    }
                    "stop_host" => {
                        commands::stop_host_persist(app);
                        app.exit(0);
                    }
                    _ => {}
                })
                .build(app)?;
            // F5 / Ctrl+R 等 WebView2 快捷键会刷新整个 GUI，必须屏蔽。
            #[cfg(windows)]
            disable_browser_accelerators(app.handle());
            Ok(())
        })
        .on_window_event(|window, event| {
            // 关闭窗口时无条件隐藏到托盘；
            // 托盘图标可找回窗口；顶栏按钮也能主动隐藏，菜单可显式退出。
            match event {
                tauri::WindowEvent::CloseRequested { api, .. } => {
                    if window.hide().is_ok() { api.prevent_close(); }
                }
                tauri::WindowEvent::Resized(_) if window.is_minimized().unwrap_or(false) => {
                    let _ = window.hide();
                }
                _ => {}
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

/// 隐藏主窗口到托盘（顶栏按钮调用）。
#[tauri::command]
fn hide_to_tray(app: tauri::AppHandle) -> Result<(), String> {
    let win = app.get_webview_window("main").ok_or("主窗口不存在")?;
    win.hide().map_err(|e| e.to_string())
}

/// Windows：关闭 WebView2 的浏览器加速键（F5/Ctrl+R/Ctrl+W 等），
/// 文本编辑键（输入框里的复制粘贴）不受影响。
#[cfg(windows)]
fn disable_browser_accelerators(app: &tauri::AppHandle) {
    use webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2Settings3;
    use windows_core::Interface;
    let Some(window) = app.get_webview_window("main") else {
        return;
    };
    let _ = window.with_webview(|webview| unsafe {
        let controller = webview.controller();
        let Ok(core) = controller.CoreWebView2() else {
            return;
        };
        let Ok(settings) = core.Settings() else {
            return;
        };
        let Ok(settings3) = settings.cast::<ICoreWebView2Settings3>() else {
            return;
        };
        let _ = settings3.SetAreBrowserAcceleratorKeysEnabled(false);
    });
}
