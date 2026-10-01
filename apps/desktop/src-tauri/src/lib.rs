//! TermBridge 桌面前端壳：真实命令层 + 托盘 + 关闭窗口行为。
//!
//! 会话 / 主机逻辑由 crates/app（后端所有者）提供；
//! 本 crate 只做 Tauri IPC 装配与桌面生命周期管理。

mod commands;
mod state;

use termbridge::i18n::{self, Lang};
use termbridge::tr;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, TrayIconBuilder, TrayIconEvent};
use tauri::Manager;

/// 托盘菜单项；切换界面语言时更新文字。
struct TrayItems {
    show: MenuItem<tauri::Wry>,
    stop: MenuItem<tauri::Wry>,
}

fn tray_labels() -> (String, String) {
    (
        tr!("Show TermBridge", "显示 TermBridge"),
        tr!(
            "Quit TermBridge (stops this GUI's host)",
            "退出 TermBridge（停止本 GUI 接收端）"
        ),
    )
}

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
            hide_to_tray,
            set_language
        ])
        .setup(|app| {
            let (show_text, stop_text) = tray_labels();
            let show = MenuItem::with_id(app, "show", show_text, true, None::<&str>)?;
            let stop = MenuItem::with_id(app, "stop_host", stop_text, true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show, &stop])?;
            app.manage(TrayItems { show: show.clone(), stop: stop.clone() });
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
#[tauri::command(rename_all = "snake_case")]
fn hide_to_tray(app: tauri::AppHandle) -> Result<(), String> {
    let win = app
        .get_webview_window("main")
        .ok_or_else(|| tr!("Main window not found", "主窗口不存在"))?;
    win.hide().map_err(|e| e.to_string())
}

/// 前端切换界面语言：后端消息与托盘菜单随之切换。
#[tauri::command(rename_all = "snake_case")]
fn set_language(app: tauri::AppHandle, lang: String) -> Result<(), String> {
    let lang = Lang::parse(&lang).ok_or_else(|| format!("unsupported language: {lang}"))?;
    i18n::set_lang(lang);
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.set_title(&tr!("TermBridge Desktop Terminal", "TermBridge 桌面终端"));
    }
    let (show_text, stop_text) = tray_labels();
    if let Some(items) = app.try_state::<TrayItems>() {
        items.show.set_text(show_text).map_err(|e| e.to_string())?;
        items.stop.set_text(stop_text).map_err(|e| e.to_string())?;
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use regex::Regex;

    const COMMANDS_RS: &str = include_str!("commands.rs");
    const LIB_RS: &str = include_str!("lib.rs");
    const IPC_TS: &str = include_str!("../../src/ipc.ts");

    /// 后端命令名 -> (属性文本, 参数名集合)，不含 `app` / `state` 这类由 Tauri 注入的参数。
    fn backend_commands() -> BTreeMap<String, (String, BTreeSet<String>)> {
        let re = Regex::new(
            r"(?s)#\[tauri::command([^\]]*)\]\s*(?:pub\s+)?(?:async\s+)?fn\s+(\w+)\((.*?)\)\s*->",
        )
        .unwrap();
        let mut out = BTreeMap::new();
        for src in [COMMANDS_RS, LIB_RS] {
            for caps in re.captures_iter(src) {
                let args = caps[3]
                    .split(',')
                    .filter_map(|arg| arg.split(':').next())
                    .map(str::trim)
                    .filter(|name| !name.is_empty() && !matches!(*name, "app" | "state"))
                    .map(str::to_string)
                    .collect();
                out.insert(caps[2].to_string(), (caps[1].to_string(), args));
            }
        }
        out
    }

    /// 前端（`src/ipc.ts`）调用的命令名 -> 它传的参数名集合。
    fn frontend_calls() -> BTreeMap<String, BTreeSet<String>> {
        let re = Regex::new(r#"invoke<[^>]*>\(\s*"(\w+)"\s*(?:,\s*\{([^}]*)\})?"#).unwrap();
        re.captures_iter(IPC_TS)
            .map(|caps| {
                let keys = caps
                    .get(2)
                    .map(|m| m.as_str())
                    .unwrap_or("")
                    .split(',')
                    .filter_map(|item| item.split(':').next())
                    .map(str::trim)
                    .filter(|key| !key.is_empty())
                    .map(str::to_string)
                    .collect();
                (caps[1].to_string(), keys)
            })
            .collect()
    }

    /// Tauri 2 默认按 camelCase 解析命令参数，而前端用 snake_case（`bind_addr`、`session_id` ……）。
    /// 命令不声明 `rename_all = "snake_case"` 时，前端的每次调用都会报
    /// "command … missing required key bindAddr"。
    #[test]
    fn commands_accept_snake_case_argument_names() {
        let commands = backend_commands();
        assert!(commands.len() >= 20, "found only {} commands", commands.len());
        let missing: Vec<_> = commands
            .iter()
            .filter(|(_, (attr, args))| {
                args.iter().any(|a| a.contains('_')) && !attr.contains(r#"rename_all = "snake_case""#)
            })
            .map(|(name, _)| name.as_str())
            .collect();
        assert!(
            missing.is_empty(),
            "commands with multi-word arguments must use #[tauri::command(rename_all = \"snake_case\")]: {missing:?}"
        );
    }

    /// 前端调用的每个命令都存在，传的参数名与后端声明完全一致。
    #[test]
    fn frontend_calls_match_backend_arguments() {
        let backend = backend_commands();
        let calls = frontend_calls();
        assert!(calls.len() >= 15, "found only {} invoke calls in ipc.ts", calls.len());
        let mut problems = Vec::new();
        for (cmd, keys) in &calls {
            match backend.get(cmd) {
                None => problems.push(format!("ipc.ts calls `{cmd}` but no such command exists")),
                Some((_, args)) if args != keys => {
                    problems.push(format!("`{cmd}`: frontend sends {keys:?}, backend takes {args:?}"))
                }
                Some(_) => {}
            }
        }
        assert!(problems.is_empty(), "{}", problems.join("\n"));
    }
}
