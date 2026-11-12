//! cache-keeper 插件状态桥接与顶栏子 WebView 布局
//!
//! 控制器由插件自身管理；壳只读取本机发现文件并调用短超时 HTTP 接口，
//! 不启动插件进程，也不把控制 token 暴露给页面或日志。

use serde::Deserialize;
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, LogicalPosition, LogicalSize, Manager};

use crate::state::AppState;
use crate::usage::kimi_code_home;
use crate::window::{focus_active_content, window_logical_size, TABBAR_HEIGHT};

pub(crate) const CACHE_KEEPER_WIDTH: f64 = 300.0;
const OVERLAY_MARGIN: f64 = 8.0;
const MIN_OVERLAY_WIDTH: f64 = 900.0;
const HTTP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Discovery {
    api_version: u64,
    port: u16,
    control_token: String,
}

fn plugin_installed() -> bool {
    kimi_code_home().is_some_and(|home| {
        home.join("plugins")
            .join("managed")
            .join("cache-keeper")
            .join("kimi.plugin.json")
            .is_file()
    })
}

fn read_discovery() -> Option<Discovery> {
    let path = kimi_code_home()?
        .join("cache-keeper")
        .join("controller.json");
    let discovery: Discovery = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    (discovery.api_version == 1 && discovery.port > 0 && !discovery.control_token.is_empty())
        .then_some(discovery)
}

fn http_client() -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .user_agent(concat!("tauri-kimi/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|_| "无法创建保活控制客户端".to_string())
}

fn current_session_id(app: &AppHandle) -> String {
    app.state::<AppState>()
        .reported_session
        .lock()
        .unwrap()
        .clone()
}

fn unavailable_status(app: &AppHandle, installed: bool) -> Value {
    json!({
        "installed": installed,
        "enabled": false,
        "currentSessionId": current_session_id(app),
        "sessions": [],
    })
}

fn status_impl(app: &AppHandle) -> Value {
    let installed = plugin_installed();
    if !installed {
        return unavailable_status(app, false);
    }
    let Some(discovery) = read_discovery() else {
        return unavailable_status(app, true);
    };
    let Ok(client) = http_client() else {
        return unavailable_status(app, true);
    };
    let response = client
        .get(format!("http://127.0.0.1:{}/v1/status", discovery.port))
        .bearer_auth(&discovery.control_token)
        .send();
    let Ok(response) = response else {
        return unavailable_status(app, true);
    };
    if !response.status().is_success() {
        return unavailable_status(app, true);
    }
    let Ok(mut value) = response.json::<Value>() else {
        return unavailable_status(app, true);
    };
    value["installed"] = Value::Bool(true);
    value["currentSessionId"] = Value::String(current_session_id(app));
    value
}

#[tauri::command]
pub(crate) async fn cache_keeper_status(app: AppHandle) -> Result<Value, ()> {
    let fallback = app.clone();
    Ok(
        tauri::async_runtime::spawn_blocking(move || status_impl(&app))
            .await
            .unwrap_or_else(|_| unavailable_status(&fallback, plugin_installed())),
    )
}

fn valid_session_id(session_id: &str) -> bool {
    !session_id.is_empty()
        && session_id.len() <= 200
        && session_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn post_action(app: &AppHandle, path: String) -> Result<Value, String> {
    if !plugin_installed() {
        return Err("未安装 cache-keeper 插件".into());
    }
    let discovery = read_discovery().ok_or_else(|| "保活控制器未运行".to_string())?;
    let response = http_client()?
        .post(format!("http://127.0.0.1:{}{path}", discovery.port))
        .bearer_auth(&discovery.control_token)
        .send()
        .map_err(|_| "无法连接保活控制器".to_string())?;
    let status = response.status();
    let value = response.json::<Value>().unwrap_or_else(|_| json!({}));
    if !status.is_success() {
        let message = value
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("保活控制请求失败");
        return Err(message.to_string());
    }
    let _ = app.emit_to("cache-keeper", "cache-keeper-changed", json!({}));
    Ok(value)
}

#[tauri::command]
pub(crate) async fn cache_keeper_off(session_id: String, app: AppHandle) -> Result<Value, String> {
    if !valid_session_id(&session_id) {
        return Err("无效的保活会话 ID".into());
    }
    tauri::async_runtime::spawn_blocking(move || {
        post_action(&app, format!("/v1/sessions/{session_id}/off"))
    })
    .await
    .map_err(|_| "保活控制线程异常".to_string())?
}

#[tauri::command]
pub(crate) async fn cache_keeper_stop(app: AppHandle) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || post_action(&app, "/v1/stop".into()))
        .await
        .map_err(|_| "保活控制线程异常".to_string())?
}

fn emit_theme(app: &AppHandle) {
    let (dark, bg) = crate::window::effective_theme(app);
    let _ = app.emit_to(
        "cache-keeper",
        "page-theme",
        json!({ "dark": dark, "bg": bg }),
    );
}

/// 保活条固定在顶栏；统计条吸附且可见时放在它左侧，否则靠右显示。
pub(crate) fn apply_cache_keeper_geom(app: &AppHandle) {
    let Some(win) = app.get_window("main") else {
        return;
    };
    let (win_w, win_h) = window_logical_size(&win);
    if win.is_minimized().unwrap_or(false) || win_w <= 0.0 || win_h <= 0.0 {
        return;
    }
    let state = app.state::<AppState>();
    let (height, stats_width) = {
        let keeper = state.cache_keeper_geom.lock().unwrap();
        let stats = state.stats_geom.lock().unwrap();
        (
            keeper.height.clamp(TABBAR_HEIGHT, win_h.max(TABBAR_HEIGHT)),
            if stats.visible && stats.docked {
                stats.w
            } else {
                0.0
            },
        )
    };
    let x = (win_w - stats_width - CACHE_KEEPER_WIDTH - OVERLAY_MARGIN).max(0.0);
    if let Some(wv) = app.get_webview("cache-keeper") {
        let _ = wv.set_position(LogicalPosition::new(x, 0.0));
        let _ = wv.set_size(LogicalSize::new(CACHE_KEEPER_WIDTH, height));
    }
    let _ = app.emit_to(
        "cache-keeper",
        "cache-keeper-applied",
        json!({ "width": CACHE_KEEPER_WIDTH, "height": height }),
    );
}

pub(crate) fn apply_cache_keeper_visibility(app: &AppHandle) {
    let wide_enough = app
        .get_window("main")
        .map(|win| window_logical_size(&win).0 >= MIN_OVERLAY_WIDTH)
        .unwrap_or(false);
    let visible = wide_enough && {
        let state = app.state::<AppState>();
        state.cache_keeper_geom.lock().unwrap().visible
            && state.active_tab.lock().unwrap().as_str() != "settings"
    };
    if let Some(wv) = app.get_webview("cache-keeper") {
        let _ = if visible { wv.show() } else { wv.hide() };
    }
    if visible {
        apply_cache_keeper_geom(app);
        emit_theme(app);
    }
}

#[tauri::command]
pub(crate) fn cache_keeper_set_visible(visible: bool, app: AppHandle) {
    app.state::<AppState>()
        .cache_keeper_geom
        .lock()
        .unwrap()
        .visible = visible;
    apply_cache_keeper_visibility(&app);
}

#[tauri::command]
pub(crate) fn cache_keeper_set_size(_width: f64, height: f64, app: AppHandle) {
    app.state::<AppState>()
        .cache_keeper_geom
        .lock()
        .unwrap()
        .height = height.max(TABBAR_HEIGHT);
    apply_cache_keeper_geom(&app);
}

#[tauri::command]
pub(crate) fn cache_keeper_focus_content(app: AppHandle) {
    focus_active_content(&app);
}

#[cfg(test)]
mod tests {
    use super::valid_session_id;

    #[test]
    fn session_id_only_allows_a_single_safe_path_segment() {
        assert!(valid_session_id(
            "session_6991cf42-0979-46f1-9fb4-1f34d4c552ba"
        ));
        assert!(valid_session_id("s"));
        assert!(!valid_session_id(""));
        assert!(!valid_session_id("../other"));
        assert!(!valid_session_id("session/off"));
        assert!(!valid_session_id("session%2Foff"));
        assert!(!valid_session_id("session?action=on"));
    }
}
