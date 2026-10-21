//! tauri_kimi：把 kimi web 包装成双击即用的 Windows 桌面壳。
//! 模块划分：state（全局状态）/ server（kimi 子进程）/ window（窗口与选项卡）/
//! stats_win（统计浮窗）/ usage（用量统计）/ inject（注入脚本）/ win_job（进程树回收）

mod inject;
mod remote;
mod server;
mod state;
mod stats_win;
mod usage;
#[cfg(windows)]
mod win_job;
mod window;

use std::time::Duration;

use tauri::menu::{MenuBuilder, MenuItemBuilder};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::webview::{NewWindowResponse, PermissionKind, PermissionResponse, WebviewBuilder};
use tauri::window::WindowBuilder;
use tauri::{LogicalPosition, LogicalSize, Manager, Url, WebviewUrl};

use crate::inject::{REFRESH_BRIDGE_JS, SESSION_REPORTER_JS, THEME_SAMPLE_JS};
use crate::state::AppState;
use crate::stats_win::{STATS_BAR_HEIGHT, STATS_WIDTH};
use crate::window::{
    is_local, is_remote_allowed, layout_webviews, open_external, show_main_window, TABBAR_HEIGHT,
};

/// 远端托管版 kimi web：登录态由 WebView2 用户数据目录持久化
const REMOTE_URL: &str = "https://code-rc.kimi.com/";
/// 活动页面主题采样周期：eval 一段只读脚本的开销可忽略，200ms 足够跟手
const THEME_POLL_INTERVAL: Duration = Duration::from_millis(200);

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![
            server::last_error,
            server::restart_server,
            remote::remote_status,
            remote::retry_remote,
            window::show_window,
            usage::session_stats,
            usage::daily_stats,
            window::switch_tab,
            window::report_theme,
            usage::report_session,
            stats_win::stats_restore,
            stats_win::stats_set_size,
            stats_win::stats_set_visible,
            stats_win::stats_focus_content,
            stats_win::stats_drag_start
        ])
        .setup(|app| {
            // 系统托盘：关窗仅隐藏，右键菜单"退出"才真正结束应用与子进程
            let tray_menu = MenuBuilder::new(app)
                .item(&MenuItemBuilder::with_id("show", "显示窗口").build(app)?)
                .separator()
                .item(&MenuItemBuilder::with_id("quit", "退出").build(app)?)
                .build()?;
            let mut tray = TrayIconBuilder::with_id("main-tray")
                .tooltip("Kimi Code")
                .menu(&tray_menu)
                .on_menu_event(|app, event| match event.id().as_ref() {
                    "show" => show_main_window(app),
                    "quit" => server::quit_app(app),
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    // 左键单击托盘图标：显示主窗口
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        show_main_window(tray.app_handle());
                    }
                });
            if let Some(icon) = app.default_window_icon() {
                tray = tray.icon(icon.clone());
            }
            tray.build(app)?;

            // 高分屏下 inner_size 是逻辑尺寸，会被 DPI 放大到超出屏幕；
            // 创建窗口前就先按主显示器工作区的 85% 钳制（上限 1600x1200），
            // 避免先按大尺寸显示再缩小的闪烁
            let (win_w, win_h) = match app.primary_monitor() {
                Ok(Some(monitor)) => {
                    let scale = monitor.scale_factor();
                    let work = monitor.work_area();
                    let max_w = work.size.width as f64 / scale;
                    let max_h = work.size.height as f64 / scale;
                    (
                        1600.0f64.min(max_w * 0.85).max(720.0),
                        1200.0f64.min(max_h * 0.85).max(480.0),
                    )
                }
                _ => (1280.0, 800.0),
            };

            // 裸窗口 + 4 个子 WebView：tabbar（顶部选项卡栏）、local（本地 kimi web）、
            // remote（远端托管服务）、stats（用量统计浮窗）；切换选项卡只改可见性，
            // 两个会话同时存活，stats 始终置顶
            let mut win_builder = WindowBuilder::new(app, "main")
                .title("Kimi Code")
                .inner_size(win_w, win_h)
                .min_inner_size(720.0, 480.0)
                .center()
                // 先隐藏，待四个子 WebView 就位（远端与统计浮窗初始隐藏）后再显示，避免闪动
                .visible(false);
            if let Some(icon) = app.default_window_icon() {
                win_builder = win_builder.icon(icon.clone())?;
            }
            let win = win_builder.build()?;

            let content_pos = LogicalPosition::new(0.0, TABBAR_HEIGHT);
            let content_size = LogicalSize::new(win_w, (win_h - TABBAR_HEIGHT).max(1.0));

            let tabbar = win.add_child(
                WebviewBuilder::new("tabbar", WebviewUrl::App("tabs.html".into())),
                LogicalPosition::new(0.0, 0.0),
                LogicalSize::new(win_w, TABBAR_HEIGHT),
            )?;

            // 本地内容 WebView：承载壳内建页（index/error）与本地 kimi web
            win.add_child(
                WebviewBuilder::new("local", WebviewUrl::App("index.html".into()))
                    // 关闭 Tauri 自带拖放拦截，让 web UI 的 HTML5 原生 drop 事件生效
                    .disable_drag_drop_handler()
                    // F5/Ctrl+R 刷新快捷键 + window.kimiDesktop 桌面桥
                    .initialization_script(REFRESH_BRIDGE_JS)
                    // 上报当前会话 id，供独立的统计浮窗查询对应会话
                    .initialization_script(SESSION_REPORTER_JS)
                    // 放行 Web 通知权限：WebView2 对未应答的权限请求默认拒绝，
                    // 页面的 Web Notification API 会因此被静默吞掉
                    .on_permission_request(|_, kind| match kind {
                        PermissionKind::Notifications => PermissionResponse::Allow,
                        _ => PermissionResponse::Default,
                    })
                    .on_navigation(|url| {
                        if is_local(url) {
                            true
                        } else {
                            open_external(url);
                            false
                        }
                    })
                    .on_new_window(|url, _features| {
                        if is_local(&url) {
                            NewWindowResponse::Allow
                        } else {
                            open_external(&url);
                            NewWindowResponse::Deny
                        }
                    }),
                content_pos,
                content_size,
            )?;

            // 远端内容 WebView：托管服务自带登录与错误提示，壳只做域名白名单管控；
            // 不注入统计浮窗（统计依赖本机 wire.jsonl，远端无数据）
            let remote = win.add_child(
                WebviewBuilder::new("remote", WebviewUrl::External(Url::parse(REMOTE_URL).unwrap()))
                    .disable_drag_drop_handler()
                    .initialization_script(REFRESH_BRIDGE_JS)
                    .on_permission_request(|_, kind| match kind {
                        PermissionKind::Notifications => PermissionResponse::Allow,
                        _ => PermissionResponse::Default,
                    })
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
                content_pos,
                content_size,
            )?;
            // 初始落在"本地"选项卡，远端 WebView 后台预加载但不可见
            let _ = remote.hide();

            // 统计浮窗：独立子 WebView，创建顺序在内容 WebView 之后，
            // wry 创建子窗口时置顶（HWND_TOP），因此始终浮在页面与选项卡栏之上；
            // 初始隐藏，待页面恢复几何并拿到用量数据后再显示
            let stats_pos = LogicalPosition::new(win_w - STATS_WIDTH - 12.0, TABBAR_HEIGHT + 12.0);
            let stats = win.add_child(
                WebviewBuilder::new("stats", WebviewUrl::App("stats.html".into()))
                    // 不在创建时抢焦点，键盘输入应留在内容页
                    .focused(false),
                stats_pos,
                LogicalSize::new(STATS_WIDTH, STATS_BAR_HEIGHT),
            )?;
            let _ = stats.hide();
            {
                let state = app.state::<AppState>();
                let mut g = state.stats_geom.lock().unwrap();
                g.x = win_w - STATS_WIDTH - 12.0;
                g.y = TABBAR_HEIGHT + 12.0;
            }

            // 记录应用页面源（如 http://tauri.localhost/），供错误页/加载页导航使用
            if let Ok(mut u) = tabbar.url() {
                u.set_path("/");
                u.set_query(None);
                u.set_fragment(None);
                *app.state::<AppState>().app_origin.lock().unwrap() = u.to_string();
            }

            let _ = win.show();

            // 主题采样轮询：定时在当前活动的内容 WebView 里 eval 只读采样脚本，
            // 由脚本读取页面计算背景色并经 report_theme 上报深浅色
            let app_theme = app.handle().clone();
            std::thread::spawn(move || loop {
                std::thread::sleep(THEME_POLL_INTERVAL);
                let tab = app_theme.state::<AppState>().active_tab.lock().unwrap().clone();
                let label = if tab == "remote" { "remote" } else { "local" };
                if let Some(wv) = app_theme.get_webview(label) {
                    let _ = wv.eval(THEME_SAMPLE_JS);
                }
            });

            server::start_server(app.handle());
            Ok(())
        })
        .on_window_event(|window, event| match event {
            // 托盘模式下关窗不退出：拦截关闭请求并隐藏窗口，
            // kimi web 子进程继续运行；真正退出走托盘菜单的"退出"
            tauri::WindowEvent::CloseRequested { api, .. } => {
                if *window.state::<AppState>().quitting.lock().unwrap() {
                    return;
                }
                api.prevent_close();
                let _ = window.hide();
            }
            // 子 WebView 不随窗口自动缩放，需手动按逻辑尺寸重排 bounds
            tauri::WindowEvent::Resized(_) => layout_webviews(window),
            _ => {}
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
