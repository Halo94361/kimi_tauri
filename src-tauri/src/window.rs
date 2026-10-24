//! 主窗口与子 WebView 的布局、选项卡切换与导航管控

use tauri::webview::{NewWindowResponse, PermissionKind, PermissionResponse, WebviewBuilder};
use tauri::{AppHandle, Emitter, LogicalPosition, LogicalSize, Manager, Url, WebviewUrl};

use crate::settings::{Settings, ThemePreference};
use crate::state::AppState;
use crate::stats_win::{apply_stats_geom, apply_stats_visibility, clamp_stats_geom};

/// 远端托管版 kimi web：登录态由 WebView2 用户数据目录持久化
const REMOTE_URL: &str = "https://code-rc.kimi.com/";

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

/// 切换活动选项卡：只改内容 WebView 可见性，本地与远端会话同时存活互不打扰；
/// 设置页打开时统计浮窗隐藏，避免置顶遮挡
pub(crate) fn switch_to_tab(app: &AppHandle, tab: &str) {
    *app.state::<AppState>().active_tab.lock().unwrap() = tab.to_string();
    for label in ["local", "remote", "settings"] {
        let active = label == tab;
        if let Some(wv) = app.get_webview(label) {
            if active {
                let _ = wv.show();
            } else {
                let _ = wv.hide();
            }
        }
    }
    apply_stats_visibility(app);
    // 同步选项卡栏高亮（覆盖 show_error 等 Rust 侧强制切卡的场景）
    let _ = app.emit_to("tabbar", "tab-changed", tab);
}

/// 把键盘焦点还给当前选项卡的内容 WebView：
/// 点击/拖动统计浮窗会夺走焦点，不回焦会导致聊天输入框无法直接输入
pub(crate) fn focus_active_content(app: &AppHandle) {
    let tab = app.state::<AppState>().active_tab.lock().unwrap().clone();
    let label = match tab.as_str() {
        "remote" | "settings" => tab.as_str(),
        _ => "local",
    };
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
/// 各内容 WebView（local/remote/settings）铺满下方
/// （bounds 是物理像素，需除以缩放因子换算），统计浮窗按当前模式重新钳制位置
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
    for label in ["local", "remote", "settings"] {
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

/// 选项卡栏页调用：切换本地 / 远端 / 设置内容 WebView 的可见性
#[tauri::command]
pub(crate) fn switch_tab(tab: String, app: AppHandle) -> Result<(), String> {
    match tab.as_str() {
        "local" | "settings" => {}
        "remote" if app.state::<AppState>().settings.lock().unwrap().show_remote_tab => {}
        "remote" => return Err("远端选项卡已隐藏".into()),
        _ => return Err(format!("未知选项卡: {tab}")),
    }
    switch_to_tab(&app, &tab);
    Ok(())
}

/// 主题采样脚本（THEME_SAMPLE_JS）的上报入口：采样结果去重后广播有效主题
#[tauri::command]
pub(crate) fn report_theme(dark: bool, bg: Option<String>, app: AppHandle) {
    let state = app.state::<AppState>();
    let mut last = state.last_theme.lock().unwrap();
    let next = (dark, bg);
    if last.as_ref() == Some(&next) {
        return;
    }
    *last = Some(next);
    drop(last);
    broadcast_theme(&app);
}

/// 有效主题：跟随 = 内容页采样结果（未采样前默认深色，与页面 CSS 兜底一致）；
/// 固定深浅色时不再给统计浮窗页面底色，让它用自己的默认底色
pub(crate) fn effective_theme(app: &AppHandle) -> (bool, Option<String>) {
    let state = app.state::<AppState>();
    let pref = state.settings.lock().unwrap().theme;
    match pref {
        ThemePreference::Dark => (true, None),
        ThemePreference::Light => (false, None),
        ThemePreference::Follow => state
            .last_theme
            .lock()
            .unwrap()
            .clone()
            .unwrap_or((true, None)),
    }
}

/// 向自绘页广播主题：tabbar / settings / local（index/error）监听 theme-changed，
/// 统计浮窗监听 page-theme（附页面底色，用于消除圆角色差）。
/// 定向投递，远端托管页收不到本机状态
pub(crate) fn broadcast_theme(app: &AppHandle) {
    let (dark, bg) = effective_theme(app);
    for label in ["tabbar", "settings", "local"] {
        let _ = app.emit_to(label, "theme-changed", if dark { "dark" } else { "light" });
    }
    let _ = app.emit_to(
        "stats",
        "page-theme",
        serde_json::json!({ "dark": dark, "bg": bg }),
    );
}

/// 设置的即时副作用：主题、远端选项卡、统计浮窗总开关。
/// 端口 / kimi 路径等「下次重启服务生效」的字段不在这里处理
pub(crate) fn apply_settings(app: &AppHandle, prev: &Settings, next: &Settings) {
    if prev.theme != next.theme {
        broadcast_theme(app);
    }
    if prev.show_remote_tab != next.show_remote_tab {
        if next.show_remote_tab {
            let _ = ensure_remote_webview(app);
        } else {
            if app.state::<AppState>().active_tab.lock().unwrap().as_str() == "remote" {
                switch_to_tab(app, "local");
            }
            if let Some(wv) = app.get_webview("remote") {
                let _ = wv.hide();
                let _ = wv.close();
            }
        }
    }
    apply_stats_visibility(app);
}

/// 创建远端内容 WebView（启动与设置里重新开启时共用）；创建后保持隐藏，
/// 由选项卡切换控制可见性。不注入统计浮窗（远端没有本机 wire.jsonl）
pub(crate) fn ensure_remote_webview(app: &AppHandle) -> tauri::Result<()> {
    if app.get_webview("remote").is_some() {
        return Ok(());
    }
    let Some(win) = app.get_window("main") else {
        return Ok(());
    };
    let (w, h) = window_logical_size(&win);
    let perm_app = app.clone();
    let wv = win.add_child(
        WebviewBuilder::new("remote", WebviewUrl::External(Url::parse(REMOTE_URL).unwrap()))
            .disable_drag_drop_handler()
            .initialization_script(crate::inject::REFRESH_BRIDGE_JS)
            .on_permission_request(move |_, kind| notification_permission(&perm_app, kind))
            .on_navigation(|url| {
                if is_remote_allowed(url) {
                    true
                } else {
                    open_external(url);
                    false
                }
            })
            .on_new_window(|url, _features| {
                if is_remote_allowed(&url) {
                    NewWindowResponse::Allow
                } else {
                    open_external(&url);
                    NewWindowResponse::Deny
                }
            }),
        LogicalPosition::new(0.0, TABBAR_HEIGHT),
        LogicalSize::new(w, (h - TABBAR_HEIGHT).max(1.0)),
    )?;
    let _ = wv.hide();
    Ok(())
}

/// Web 通知按设置放行，其余权限保持 WebView2 默认行为；
/// WebView2 对未应答的权限请求默认拒绝，会静默吞掉页面通知
pub(crate) fn notification_permission(app: &AppHandle, kind: PermissionKind) -> PermissionResponse {
    match kind {
        PermissionKind::Notifications => {
            if app.state::<AppState>().settings.lock().unwrap().notifications {
                PermissionResponse::Allow
            } else {
                PermissionResponse::Deny
            }
        }
        _ => PermissionResponse::Default,
    }
}
