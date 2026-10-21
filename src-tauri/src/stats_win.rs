//! 统计浮窗（stats 子 WebView）的几何、吸附/悬浮模式切换与拖动

use tauri::{AppHandle, Emitter, LogicalPosition, LogicalSize, Manager};

use crate::state::{AppState, StatsGeom};
use crate::window::{focus_active_content, window_logical_size, TABBAR_HEIGHT};

/// 统计浮窗（独立子 WebView）的固定逻辑宽度：内容变化不再改变宽度与位置
pub(crate) const STATS_WIDTH: f64 = 300.0;
/// 悬浮态横条高度
pub(crate) const STATS_BAR_HEIGHT: f64 = 30.0;
/// 折叠态圆点尺寸
const STATS_FOLDED_SIZE: f64 = 36.0;
/// 吸附态与窗口右边缘的间距（与选项卡栏的 8px padding 对齐）
const STATS_DOCK_MARGIN: f64 = 8.0;
/// 拖动循环的轮询间隔：约 120Hz，保证浮窗跟手
const STATS_DRAG_INTERVAL: std::time::Duration = std::time::Duration::from_millis(8);

/// 把统计浮窗几何钳制在窗口内：吸附态右对齐贴顶（展开的明细向下延伸），
/// 悬浮态保持完整可见且不压住选项卡栏
pub(crate) fn clamp_stats_geom(g: &mut StatsGeom, win_w: f64, win_h: f64) {
    g.h = g.h.clamp(1.0, win_h.max(1.0));
    if g.docked {
        g.x = (win_w - g.w - STATS_DOCK_MARGIN).max(0.0);
        g.y = 0.0;
    } else {
        g.x = g.x.clamp(0.0, (win_w - g.w).max(0.0));
        let max_y = (win_h - g.h).max(0.0);
        let min_y = TABBAR_HEIGHT.min(max_y);
        g.y = g.y.clamp(min_y, max_y);
    }
}

/// 应用统计浮窗几何；位置与尺寸都是相对主窗口客户区的逻辑像素
pub(crate) fn apply_stats_geom(app: &AppHandle, g: &StatsGeom) {
    if let Some(wv) = app.get_webview("stats") {
        let _ = wv.set_position(LogicalPosition::new(g.x, g.y));
        let _ = wv.set_size(LogicalSize::new(g.w, g.h));
    }
}

/// 只移动统计浮窗：拖动过程中尺寸不变，避免每帧重复触发 WebView 重排
fn move_stats_webview(app: &AppHandle, x: f64, y: f64) {
    if let Some(wv) = app.get_webview("stats") {
        let _ = wv.set_position(LogicalPosition::new(x, y));
    }
}

/// 通知统计浮窗当前处于吸附还是悬浮
fn emit_stats_mode(app: &AppHandle, docked: bool) {
    let _ = app.emit_to("stats", "stats-mode", serde_json::json!({ "docked": docked }));
}

/// 重发最近一次采样的主题：浮窗 WebView 可能晚于采样就绪，
/// 轮询去重后不会再发，因此在恢复/显示时补一次
fn emit_stats_theme(app: &AppHandle) {
    let state = app.state::<AppState>();
    let last = state.last_theme.lock().unwrap().clone();
    if let Some((dark, bg)) = last {
        let _ = app.emit_to(
            "stats",
            "page-theme",
            serde_json::json!({ "dark": dark, "bg": bg }),
        );
    }
}

/// 鼠标位置（物理屏幕像素）
#[cfg(windows)]
fn cursor_pos() -> Option<(f64, f64)> {
    use windows_sys::Win32::Foundation::POINT;
    use windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos;
    let mut p = POINT { x: 0, y: 0 };
    if unsafe { GetCursorPos(&mut p) } != 0 {
        Some((p.x as f64, p.y as f64))
    } else {
        None
    }
}

#[cfg(not(windows))]
fn cursor_pos() -> Option<(f64, f64)> {
    None
}

/// 鼠标左键是否仍按下（Rust 侧拖动循环据此判断结束）
#[cfg(windows)]
fn left_button_down() -> bool {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON};
    unsafe { (GetAsyncKeyState(VK_LBUTTON as i32) as u16 & 0x8000) != 0 }
}

#[cfg(not(windows))]
fn left_button_down() -> bool {
    false
}

/// 统计浮窗加载完成：应用页面保存的模式与位置（此时仍保持隐藏，
/// 待首次拿到用量数据后由 stats_set_visible 显示，避免空白横条闪现）
#[tauri::command]
pub(crate) fn stats_restore(
    docked: bool,
    folded: bool,
    x: Option<f64>,
    y: Option<f64>,
    app: AppHandle,
) {
    let Some(win) = app.get_window("main") else {
        return;
    };
    let (win_w, win_h) = window_logical_size(&win);
    if win.is_minimized().unwrap_or(false) || win_w <= 0.0 || win_h <= 0.0 {
        return;
    }
    let state = app.state::<AppState>();
    let g = {
        let mut g = state.stats_geom.lock().unwrap();
        g.docked = docked;
        g.folded = folded;
        g.w = if folded { STATS_FOLDED_SIZE } else { STATS_WIDTH };
        g.h = if folded {
            STATS_FOLDED_SIZE
        } else if docked {
            TABBAR_HEIGHT
        } else {
            STATS_BAR_HEIGHT
        };
        // 保存的 x 是展开横条的左边缘；折叠态以右边缘对齐，避免圆点跳位
        g.x = x.unwrap_or(win_w - STATS_WIDTH - 12.0);
        g.y = y.unwrap_or(TABBAR_HEIGHT + 12.0);
        if folded && !docked {
            g.x += STATS_WIDTH - STATS_FOLDED_SIZE;
        }
        clamp_stats_geom(&mut g, win_w, win_h);
        g.clone()
    };
    apply_stats_geom(&app, &g);
    emit_stats_mode(&app, g.docked);
    emit_stats_theme(&app);
}

/// 统计浮窗上报内容尺寸（固定宽度 + 按内容变化的高度）；
/// Rust 负责钳制到窗口内并保持右边缘不因折叠/展开而跳动
#[tauri::command]
pub(crate) fn stats_set_size(width: f64, height: f64, app: AppHandle) {
    let Some(win) = app.get_window("main") else {
        return;
    };
    let (win_w, win_h) = window_logical_size(&win);
    // 最小化期间不按 0 尺寸钳制，否则恢复后浮窗位置会丢
    if win.is_minimized().unwrap_or(false) || win_w <= 0.0 || win_h <= 0.0 {
        return;
    }
    let state = app.state::<AppState>();
    let g = {
        let mut g = state.stats_geom.lock().unwrap();
        let folded = width <= STATS_FOLDED_SIZE + 0.5;
        let old_w = g.w;
        g.folded = folded;
        g.w = if folded { STATS_FOLDED_SIZE } else { STATS_WIDTH };
        g.h = if folded {
            STATS_FOLDED_SIZE
        } else if g.docked {
            height.clamp(TABBAR_HEIGHT, win_h.max(TABBAR_HEIGHT))
        } else {
            height.clamp(STATS_BAR_HEIGHT, win_h.max(STATS_BAR_HEIGHT))
        };
        if !g.docked && (old_w - g.w).abs() > 0.5 {
            g.x += old_w - g.w;
        }
        clamp_stats_geom(&mut g, win_w, win_h);
        g.clone()
    };
    apply_stats_geom(&app, &g);
    let _ = app.emit_to(
        "stats",
        "stats-applied",
        serde_json::json!({ "width": g.w, "height": g.h }),
    );
}

/// 有数据才显示统计浮窗；隐藏时只 Hide，不改几何
#[tauri::command]
pub(crate) fn stats_set_visible(visible: bool, app: AppHandle) {
    let state = app.state::<AppState>();
    {
        let mut g = state.stats_geom.lock().unwrap();
        if g.visible == visible {
            return;
        }
        g.visible = visible;
    }
    if let Some(wv) = app.get_webview("stats") {
        let _ = if visible { wv.show() } else { wv.hide() };
    }
    if visible {
        let g = state.stats_geom.lock().unwrap().clone();
        apply_stats_geom(&app, &g);
        emit_stats_theme(&app);
    }
}

/// 点击/拖动统计浮窗后把键盘焦点还给内容页
#[tauri::command]
pub(crate) fn stats_focus_content(app: AppHandle) {
    focus_active_content(&app);
}

/// 统计浮窗拖动：由 Rust 轮询光标移动子 WebView，避免依赖跨 WebView 的
/// pointer capture；光标进入顶部选项卡栏带内实时吸附，移出即恢复悬浮
#[tauri::command]
pub(crate) fn stats_drag_start(app: AppHandle) {
    let Some(win) = app.get_window("main") else {
        return;
    };
    let state = app.state::<AppState>();
    {
        let mut dragging = state.stats_dragging.lock().unwrap();
        if *dragging {
            return;
        }
        *dragging = true;
    }
    let mut g = state.stats_geom.lock().unwrap().clone();
    let Some((cx, cy)) = cursor_pos() else {
        *state.stats_dragging.lock().unwrap() = false;
        return;
    };
    if !g.visible {
        *state.stats_dragging.lock().unwrap() = false;
        return;
    }
    // 拖动期间固定按横条高度，收起明细，避免高度反复变化
    g.h = if g.folded {
        STATS_FOLDED_SIZE
    } else if g.docked {
        TABBAR_HEIGHT
    } else {
        STATS_BAR_HEIGHT
    };
    {
        let mut st = state.stats_geom.lock().unwrap();
        st.h = g.h;
    }
    apply_stats_geom(&app, &g);

    let scale = win.scale_factor().unwrap_or(1.0);
    let Ok(inner) = win.inner_position() else {
        *state.stats_dragging.lock().unwrap() = false;
        return;
    };
    let rel_x = (cx - inner.x as f64) / scale;
    let rel_y = (cy - inner.y as f64) / scale;
    let grab_x = rel_x - g.x;
    let grab_y = rel_y - g.y;

    let app2 = app.clone();
    std::thread::spawn(move || {
        let state = app2.state::<AppState>();
        let mut cur = g;
        let mut docked = cur.docked;
        let mut moved = false;

        loop {
            std::thread::sleep(STATS_DRAG_INTERVAL);
            let Some((px, py)) = cursor_pos() else { break };
            let down = left_button_down();
            let Ok(inner) = win.inner_position() else { break };
            let (win_w, win_h) = window_logical_size(&win);
            let scale = win.scale_factor().unwrap_or(1.0);
            let rel_x = (px - inner.x as f64) / scale;
            let rel_y = (py - inner.y as f64) / scale;

            if !moved && ((rel_x - grab_x) - cur.x).abs() + ((rel_y - grab_y) - cur.y).abs() > 3.0
            {
                moved = true;
            }
            if moved {
                let in_band = (0.0..=TABBAR_HEIGHT).contains(&rel_y);
                let mut mode_changed = false;
                if in_band && !docked {
                    docked = true;
                    cur.docked = true;
                    mode_changed = true;
                    emit_stats_mode(&app2, true);
                } else if !in_band && docked {
                    docked = false;
                    cur.docked = false;
                    mode_changed = true;
                    emit_stats_mode(&app2, false);
                }
                // 吸附条与选项卡栏同高；悬浮横条为 30，折叠态为 36 圆点
                let bar_h = if cur.docked {
                    TABBAR_HEIGHT
                } else {
                    STATS_BAR_HEIGHT
                };
                if cur.docked {
                    cur.x = (win_w - cur.w - STATS_DOCK_MARGIN).max(0.0);
                    cur.y = 0.0;
                } else {
                    cur.x = rel_x - grab_x;
                    cur.y = (rel_y - grab_y).max(TABBAR_HEIGHT);
                }
                cur.h = if cur.folded { STATS_FOLDED_SIZE } else { bar_h };
                clamp_stats_geom(&mut cur, win_w, win_h);
                {
                    // 只同步拖动相关字段，避免覆盖页面在拖动期间上报的宽度/折叠状态
                    let mut st = state.stats_geom.lock().unwrap();
                    st.docked = cur.docked;
                    st.x = cur.x;
                    st.y = cur.y;
                    st.h = cur.h;
                }
                if mode_changed {
                    apply_stats_geom(&app2, &cur);
                } else {
                    move_stats_webview(&app2, cur.x, cur.y);
                }
            }
            if !down {
                break;
            }
        }

        let final_geom = {
            // 只落拖动结果；高度/折叠/可见性以页面最后一次上报为准，
            // 避免拖动结束恰好发生在折叠切换时覆盖页面的尺寸
            let mut st = state.stats_geom.lock().unwrap();
            st.docked = cur.docked;
            st.x = cur.x;
            st.y = cur.y;
            st.clone()
        };
        apply_stats_geom(&app2, &final_geom);
        // 持久化用「展开横条左边缘」，折叠态换算回去，避免下次恢复时重复偏移
        let float_x = if final_geom.folded {
            final_geom.x - (STATS_WIDTH - STATS_FOLDED_SIZE)
        } else {
            final_geom.x
        };
        let _ = app2.emit_to(
            "stats",
            "stats-drop",
            serde_json::json!({ "docked": final_geom.docked, "x": float_x, "y": final_geom.y }),
        );
        let _ = app2.emit_to("stats", "stats-drag-end", serde_json::json!({}));
        *state.stats_dragging.lock().unwrap() = false;
        focus_active_content(&app2);
    });
}
