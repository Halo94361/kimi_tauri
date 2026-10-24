//! tauri_kimi：把 kimi web 包装成双击即用的 Windows 桌面壳。
//! 模块划分：state（全局状态）/ server（kimi 子进程）/ window（窗口、选项卡与主题）/
//! stats_win（统计浮窗）/ usage（用量统计）/ inject（注入脚本）/
//! settings（壳设置持久化）/ diagnostics（诊断与 CLI 升级）/ win_job（进程树回收）

mod diagnostics;
mod inject;
mod remote;
mod server;
mod settings;
mod state;
mod stats_win;
mod usage;
#[cfg(windows)]
mod win_job;
mod window;

use std::time::Duration;

use tauri::menu::{MenuBuilder, MenuItemBuilder};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::webview::{NewWindowResponse, WebviewBuilder};
use tauri::window::WindowBuilder;
use tauri::{LogicalPosition, LogicalSize, Manager, Theme, WebviewUrl};

use crate::inject::{
    shell_theme_init_js, REFRESH_BRIDGE_JS, SESSION_REPORTER_JS, THEME_SAMPLE_JS,
};
use crate::state::AppState;
use crate::stats_win::{STATS_BAR_HEIGHT, STATS_WIDTH};
use crate::window::{
    broadcast_theme, effective_theme, ensure_remote_webview, is_local, layout_webviews,
    notification_permission, open_external, show_main_window, theme_background, TABBAR_HEIGHT,
};

/// 活动内容页主题采样周期；每次 eval 同步读取并上报，结果变化时立即广播
const THEME_POLL_INTERVAL: Duration = Duration::from_millis(50);

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
            server::proxy_status,
            server::retry_proxy,
            window::show_window,
            usage::session_stats,
            usage::daily_stats,
            usage::weekly_stats,
            window::switch_tab,
            window::report_theme,
            window::sync_theme,
            usage::report_session,
            stats_win::stats_restore,
            stats_win::stats_set_size,
            stats_win::stats_set_visible,
            stats_win::stats_focus_content,
            stats_win::stats_drag_start,
            stats_win::stats_reset_position,
            remote::disconnect_remote,
            settings::get_settings,
            settings::set_settings,
            settings::reset_settings,
            diagnostics::get_diagnostics,
            diagnostics::refresh_cli_info,
            diagnostics::upgrade_cli,
            diagnostics::open_config_target
        ])
        .setup(|app| {
            // 壳自身设置先于窗口/子进程加载：远端选项卡、代理模式、端口等都读它
            let loaded = settings::load(&app.handle());
            *app.state::<AppState>().settings.lock().unwrap() = loaded;
            let initial_dark = effective_theme(app.handle()).0;
            let initial_bg = theme_background(initial_dark);

            // 系统托盘：关窗行为见 CloseRequested；菜单提供设置与重启入口，
            // 「退出」才真正结束应用与子进程
            let tray_menu = MenuBuilder::new(app)
                .item(&MenuItemBuilder::with_id("show", "显示窗口").build(app)?)
                .item(&MenuItemBuilder::with_id("settings", "设置").build(app)?)
                .item(&MenuItemBuilder::with_id("restart", "重启本地服务").build(app)?)
                .separator()
                .item(&MenuItemBuilder::with_id("quit", "退出").build(app)?)
                .build()?;
            let mut tray = TrayIconBuilder::with_id("main-tray")
                .tooltip("Kimi Code")
                .menu(&tray_menu)
                .on_menu_event(|app, event| match event.id().as_ref() {
                    "show" => show_main_window(app),
                    "settings" => {
                        show_main_window(app);
                        crate::window::switch_to_tab(app, "settings");
                    }
                    "restart" => {
                        show_main_window(app);
                        crate::window::switch_to_tab(app, "local");
                        server::restart_server(app.clone());
                    }
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

            // 裸窗口 + 5 个子 WebView：tabbar（顶部选项卡栏）、local（本地 kimi web）、
            // remote（远端托管服务，可按设置不创建）、settings（设置页）、stats（用量统计浮窗）；
            // 切换选项卡只改可见性，两个会话同时存活，stats 始终置顶（设置页打开时隐藏）
            let mut win_builder = WindowBuilder::new(app, "main")
                .title("Kimi Code")
                .inner_size(win_w, win_h)
                .min_inner_size(720.0, 480.0)
                .center()
                // 先隐藏，待子 WebView 就位（远端/设置/统计浮窗初始隐藏）后再显示，避免闪动
                .visible(false);
            if let Some(icon) = app.default_window_icon() {
                win_builder = win_builder.icon(icon.clone())?;
            }
            let win = win_builder.build()?;
            let _ = win.set_theme(Some(if initial_dark { Theme::Dark } else { Theme::Light }));
            let _ = win.set_background_color(Some(initial_bg));

            let content_pos = LogicalPosition::new(0.0, TABBAR_HEIGHT);
            let content_size = LogicalSize::new(win_w, (win_h - TABBAR_HEIGHT).max(1.0));

            let tabbar = win.add_child(
                WebviewBuilder::new("tabbar", WebviewUrl::App("tabs.html".into()))
                    .background_color(initial_bg)
                    .initialization_script(shell_theme_init_js(initial_dark)),
                LogicalPosition::new(0.0, 0.0),
                LogicalSize::new(win_w, TABBAR_HEIGHT),
            )?;

            // 本地内容 WebView：承载壳内建页（index/error）与本地 kimi web
            let perm_app = app.handle().clone();
            win.add_child(
                WebviewBuilder::new("local", WebviewUrl::App("index.html".into()))
                    .background_color(initial_bg)
                    // 关闭 Tauri 自带拖放拦截，让 web UI 的 HTML5 原生 drop 事件生效
                    .disable_drag_drop_handler()
                    .initialization_script(shell_theme_init_js(initial_dark))
                    // F5/Ctrl+R 刷新快捷键 + window.kimiDesktop 桌面桥
                    .initialization_script(REFRESH_BRIDGE_JS)
                    // 页面主题变化时立即采样上报
                    .initialization_script(THEME_SAMPLE_JS)
                    // 上报当前会话 id，供独立的统计浮窗查询对应会话
                    .initialization_script(SESSION_REPORTER_JS)
                    // Web 通知按设置放行：WebView2 对未应答的权限请求默认拒绝，
                    // 页面的 Web Notification API 会因此被静默吞掉
                    .on_permission_request(move |_, kind| notification_permission(&perm_app, kind))
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

            // 远端内容 WebView 按设置创建（可隐藏「远端」选项卡）：
            // 初始落在"本地"选项卡，远端后台预加载但不可见
            if app.state::<AppState>().settings.lock().unwrap().show_remote_tab {
                ensure_remote_webview(app.handle())?;
            }

            // 设置页：第三个内容 WebView，纯原生自绘；初始隐藏，切到「设置」时显示
            win.add_child(
                WebviewBuilder::new("settings", WebviewUrl::App("settings.html".into()))
                    .background_color(initial_bg)
                    .initialization_script(shell_theme_init_js(initial_dark)),
                content_pos,
                content_size,
            )?
            .hide()?;

            // 统计浮窗：独立子 WebView，创建顺序在内容 WebView 之后，
            // wry 创建子窗口时置顶（HWND_TOP），因此始终浮在页面与选项卡栏之上；
            // 初始隐藏，待页面恢复几何并拿到用量数据后再显示
            let stats_pos = LogicalPosition::new(win_w - STATS_WIDTH - 12.0, TABBAR_HEIGHT + 12.0);
            let stats = win.add_child(
                WebviewBuilder::new("stats", WebviewUrl::App("stats.html".into()))
                    .background_color(initial_bg)
                    .initialization_script(shell_theme_init_js(initial_dark))
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

            *app.state::<AppState>().active_tab.lock().unwrap() = "local".to_string();
            broadcast_theme(app.handle());
            let _ = win.show();

            // 只轮询当前活动的内容 WebView；设置页使用壳自身主题，无需采样
            let app_theme = app.handle().clone();
            std::thread::spawn(move || loop {
                std::thread::sleep(THEME_POLL_INTERVAL);
                let tab = app_theme.state::<AppState>().active_tab.lock().unwrap().clone();
                let label = match tab.as_str() {
                    "local" => "local",
                    "remote" => "remote",
                    _ => continue,
                };
                if let Some(wv) = app_theme.get_webview(label) {
                    let _ = wv.eval(THEME_SAMPLE_JS);
                }
            });

            server::start_server(app.handle());
            Ok(())
        })
        .on_window_event(|window, event| match event {
            // 关窗行为由设置决定：托盘模式仅隐藏（子进程继续跑，真正退出走托盘
            // 菜单）；直接退出模式走 quit_app，同样保证子进程被回收
            tauri::WindowEvent::CloseRequested { api, .. } => {
                if *window.state::<AppState>().quitting.lock().unwrap() {
                    return;
                }
                api.prevent_close();
                if window.state::<AppState>().settings.lock().unwrap().close_to_tray {
                    let _ = window.hide();
                } else {
                    server::quit_app(window.app_handle());
                }
            }
            // 子 WebView 不随窗口自动缩放，需手动按逻辑尺寸重排 bounds
            tauri::WindowEvent::Resized(_) => layout_webviews(window),
            _ => {}
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
