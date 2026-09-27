// TermBridge 桌面前端壳入口。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    termbridge_desktop::run()
}
