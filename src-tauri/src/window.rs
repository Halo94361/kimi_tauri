//! 主窗口与子 WebView 的布局、选项卡切换与导航管控

use tauri::{AppHandle, Emitter, LogicalPosition, LogicalSize, Manager, Url};

use crate::state::AppState;
use crate::stats_win::{apply_stats_geom, clamp_stats_geom};

/// 顶部选项卡栏（tabbar WebView）的逻辑高度
pub(crate) const TABBAR_HEIGHT: f64 = 36.0;

/// 允许在窗口内导航的来源：Tauri 应用自身页面与本机 kimi web 服务
pub(crate) fn is_local(url: &Url) -> bool {
    match url.host_str() {
        Some("127.0.0.1") | Some("localhost") | Some("tauri.localhost") => true,
        None => url.scheme() == "tauri",
        _ => false,
    }
}

/// 远端内容 WebView 允许在窗口内导航的域名：
/// code-rc.kimi.com 本体与 kimi.com 登录链路子域；其余外链一律系统浏览器打开
pub(crate) fn is_remote_allowed(url: &Url) -> bool {
    match url.host_str() {
        Some(h) => url.scheme() == "https" && (h == "kimi.com" || h.ends_with(".kimi.com")),
        None => false,
    }
}

pub(crate) fn open_external(url: &Url) {
    let _ = tauri_plugin_opener::open_url(url.as_str(), None::<&str>);
}

/// 显示并聚焦主窗口（从托盘恢复时调用）
pub(crate) fn show_main_window(app: &AppHandle) {
    if let Some(w) = app.get_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
    }
}

/// 切换活动选项卡：只改内容 WebView 可见性，两个会话同时存活互不打扰
pub(crate) fn switch_to_tab(app: &AppHandle, tab: &str) {
    *app.state::<AppState>().active_tab.lock().unwrap() = tab.to_string();
    for (label, active) in [("local", tab == "local"), ("remote", tab == "remote")] {
        if let Some(wv) = app.get_webview(label) {
            if active {
                let _ = wv.show();
            } else {
                let _ = wv.hide();
            }
        }
    }
    // 同步选项卡栏高亮（覆盖 show_error 等 Rust 侧强制切卡的场景）
    let _ = app.emit_to("tabbar", "tab-changed", tab);
}

/// 把键盘焦点还给当前选项卡的内容 WebView：
/// 点击/拖动统计浮窗会夺走焦点，不回焦会导致聊天输入框无法直接输入
pub(crate) fn focus_active_content(app: &AppHandle) {
    let tab = app.state::<AppState>().active_tab.lock().unwrap().clone();
    let label = if tab == "remote" { "remote" } else { "local" };
    if let Some(wv) = app.get_webview(label) {
        let _ = wv.set_focus();
    }
}

/// 主窗口客户区的逻辑尺寸
pub(crate) fn window_logical_size(window: &tauri::Window) -> (f64, f64) {
    let scale = window.scale_factor().unwrap_or(1.0);
    window
        .inner_size()
        .map(|s| (s.width as f64 / scale, s.height as f64 / scale))
        .unwrap_or((0.0, 0.0))
}

/// 按窗口逻辑尺寸重排子 WebView：tabbar 置顶 TABBAR_HEIGHT，
/// 两个内容 WebView 铺满下方（bounds 是物理像素，需除以缩放因子换算），
/// 统计浮窗按当前模式重新钳制位置
pub(crate) fn layout_webviews(window: &tauri::Window) {
    let Ok(size) = window.inner_size() else {
        return;
    };
    // 最小化会带出 0x0 的 Resized 事件；此时不能按 0 尺寸钳制浮窗位置
    if window.is_minimized().unwrap_or(false) || size.width == 0 || size.height == 0 {
        return;
    }
    let scale = window.scale_factor().unwrap_or(1.0);
    let w = size.width as f64 / scale;
    let h = size.height as f64 / scale;
    let content_h = (h - TABBAR_HEIGHT).max(0.0);
    if let Some(tabbar) = window.get_webview("tabbar") {
        let _ = tabbar.set_position(LogicalPosition::new(0.0, 0.0));
        let _ = tabbar.set_size(LogicalSize::new(w, TABBAR_HEIGHT));
    }
    for label in ["local", "remote"] {
        if let Some(wv) = window.get_webview(label) {
            let _ = wv.set_position(LogicalPosition::new(0.0, TABBAR_HEIGHT));
            let _ = wv.set_size(LogicalSize::new(w, content_h));
        }
    }

    let app = window.app_handle();
    let state = app.state::<AppState>();
    let g = {
        let mut g = state.stats_geom.lock().unwrap();
        clamp_stats_geom(&mut g, w, h);
        g.clone()
    };
    apply_stats_geom(app, &g);
    let _ = app.emit_to(
        "stats",
        "stats-geom",
        serde_json::json!({ "width": g.w, "height": g.h }),
    );
    // 高度可能被窗口尺寸钳制，通知页面按最新可用高度调整明细滚动区
    let _ = app.emit_to(
        "stats",
        "stats-applied",
        serde_json::json!({ "width": g.w, "height": g.h }),
    );
}

// 供注入脚本暴露的 window.kimiDesktop.showWindow 调用：显示并聚焦主窗口
#[tauri::command]
pub(crate) fn show_window(app: AppHandle) {
    show_main_window(&app);
}

/// 选项卡栏页调用：切换本地 / 远端内容 WebView 的可见性
#[tauri::command]
pub(crate) fn switch_tab(tab: String, app: AppHandle) -> Result<(), String> {
    match tab.as_str() {
        "local" | "remote" => {
            switch_to_tab(&app, &tab);
            Ok(())
        }
        _ => Err(format!("未知选项卡: {tab}")),
    }
}

/// 主题采样脚本（THEME_SAMPLE_JS）的上报入口：
/// 深浅色变化时通知 tabbar；深浅色或背景色变化时通知统计浮窗
#[tauri::command]
pub(crate) fn report_theme(dark: bool, bg: Option<String>, app: AppHandle) {
    let state = app.state::<AppState>();
    let mut last = state.last_theme.lock().unwrap();
    let dark_changed = last.as_ref().map(|(d, _)| *d) != Some(dark);
    let next = (dark, bg.clone());
    if last.as_ref() == Some(&next) {
        return;
    }
    *last = Some(next);
    drop(last);
    if dark_changed {
        let _ = app.emit_to("tabbar", "theme-changed", if dark { "dark" } else { "light" });
    }
    let _ = app.emit_to(
        "stats",
        "page-theme",
        serde_json::json!({ "dark": dark, "bg": bg }),
    );
}
