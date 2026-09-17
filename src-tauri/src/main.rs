// 防止 release 构建出现控制台窗口
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    tauri_kimi_lib::run()
}
