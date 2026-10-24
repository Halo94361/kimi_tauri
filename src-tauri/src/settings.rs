//! 壳自身设置：持久化到 app_config_dir/settings.json，与 CLI 的 config.toml 无关。
//! 所有字段都有默认值：文件缺失、字段缺失或文件损坏都回退默认，不阻塞启动。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};

use crate::state::AppState;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ProxyMode {
    /// 仅当 CLI 配置指向本代理时才启动
    #[default]
    Auto,
    Always,
    Off,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ThemePreference {
    /// 跟随内容页采样色（自绘页面的既有行为）
    #[default]
    Follow,
    Dark,
    Light,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Settings {
    /// 本地服务就绪后自动开启远程控制隧道；只影响下次启动，不动当前隧道
    pub remote_auto_connect: bool,
    /// 是否显示「远端」选项卡（code-rc.kimi.com 托管页），与 rc 隧道无关
    pub show_remote_tab: bool,
    pub proxy_mode: ProxyMode,
    pub stats_enabled: bool,
    /// 统计浮窗展示会话/今日 token 与 24h 柱图
    pub stats_show_usage: bool,
    /// 统计浮窗展示 Kimi 5h / 本周额度
    pub stats_quota_kimi: bool,
    /// 统计浮窗展示 OpenCode Go 额度（需要本机 seekfetch）
    pub stats_quota_opencode: bool,
    /// 套餐额度刷新间隔（秒），页面按此周期轮询，后端缓存略短于它
    pub stats_quota_refresh_secs: u64,
    /// true = 关窗隐藏到托盘；false = 关窗直接退出并回收子进程
    pub close_to_tray: bool,
    pub theme: ThemePreference,
    pub notifications: bool,
    /// kimi 可执行文件路径；空 = 自动查找（PATH → 已知安装路径）
    pub kimi_path: String,
    /// 本地服务端口；0 = 首选 58627、被占用时自动回退
    pub kimi_port: u16,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            remote_auto_connect: true,
            show_remote_tab: true,
            proxy_mode: ProxyMode::Auto,
            stats_enabled: true,
            stats_show_usage: true,
            stats_quota_kimi: true,
            stats_quota_opencode: true,
            stats_quota_refresh_secs: 120,
            close_to_tray: true,
            theme: ThemePreference::Follow,
            notifications: true,
            kimi_path: String::new(),
            kimi_port: 0,
        }
    }
}

fn settings_path(app: &AppHandle) -> Option<PathBuf> {
    app.path()
        .app_config_dir()
        .ok()
        .map(|dir| dir.join("settings.json"))
}

pub(crate) fn load(app: &AppHandle) -> Settings {
    let Some(path) = settings_path(app) else {
        return Settings::default();
    };
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// 先写临时文件再替换，避免写入中途崩溃留下半个 JSON
fn save(app: &AppHandle, settings: &Settings) -> Result<(), String> {
    let path = settings_path(app).ok_or("无法定位设置目录")?;
    let dir = path.parent().ok_or("无法定位设置目录")?;
    std::fs::create_dir_all(dir).map_err(|e| format!("创建设置目录失败：{e}"))?;
    let text = serde_json::to_string_pretty(settings).map_err(|e| e.to_string())?;
    let tmp = dir.join("settings.json.tmp");
    std::fs::write(&tmp, text).map_err(|e| format!("写入设置失败：{e}"))?;
    replace_file(&tmp, &path).map_err(|e| format!("保存设置失败：{e}"))
}

/// Windows 的 rename 不能覆盖已存在的目标文件，用 MoveFileEx 替换
#[cfg(windows)]
pub(crate) fn replace_file(tmp: &Path, dest: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };
    let from: Vec<u16> = tmp.as_os_str().encode_wide().chain(Some(0)).collect();
    let to: Vec<u16> = dest.as_os_str().encode_wide().chain(Some(0)).collect();
    let flags = MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH;
    if unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), flags) } == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
pub(crate) fn replace_file(tmp: &Path, dest: &Path) -> std::io::Result<()> {
    std::fs::rename(tmp, dest)
}

#[tauri::command]
pub(crate) fn get_settings(state: tauri::State<AppState>) -> Settings {
    state.settings.lock().unwrap().clone()
}

#[tauri::command]
pub(crate) fn set_settings(settings: Settings, app: AppHandle) -> Result<(), String> {
    apply(&app, settings).map(|_| ())
}

#[tauri::command]
pub(crate) fn reset_settings(app: AppHandle) -> Result<Settings, String> {
    apply(&app, Settings::default())
}

/// 落盘成功后才生效，再广播并应用各领域的即时副作用；
/// 端口 / kimi 路径这类「下次重启服务生效」的字段只保存，不在这里处理
fn apply(app: &AppHandle, mut next: Settings) -> Result<Settings, String> {
    next.kimi_path = next.kimi_path.trim().to_string();
    // 页面按此周期轮询额度；下限 30s，防止误填 0 把接口打成压力测试
    next.stats_quota_refresh_secs = next.stats_quota_refresh_secs.clamp(30, 3600);
    save(app, &next)?;
    let prev = {
        let state = app.state::<AppState>();
        let mut cur = state.settings.lock().unwrap();
        let prev = cur.clone();
        *cur = next.clone();
        prev
    };
    // 只发给壳自绘页面：远端托管页不应读到本机设置
    for label in ["tabbar", "settings", "stats"] {
        let _ = app.emit_to(label, "settings-changed", &next);
    }
    crate::window::apply_settings(app, &prev, &next);
    crate::server::apply_settings(app, &prev, &next);
    Ok(next)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_fill_missing_fields() {
        let s: Settings = serde_json::from_str("{}").unwrap();
        assert!(s.remote_auto_connect);
        assert!(s.show_remote_tab);
        assert!(s.stats_enabled);
        assert!(s.stats_show_usage);
        assert!(s.stats_quota_kimi);
        assert!(s.stats_quota_opencode);
        assert_eq!(s.stats_quota_refresh_secs, 120);
        assert!(s.close_to_tray);
        assert!(s.notifications);
        assert_eq!(s.proxy_mode, ProxyMode::Auto);
        assert_eq!(s.theme, ThemePreference::Follow);
        assert_eq!(s.kimi_port, 0);
        assert!(s.kimi_path.is_empty());
    }

    #[test]
    fn roundtrip_preserves_explicit_values() {
        let s = Settings {
            remote_auto_connect: false,
            show_remote_tab: false,
            proxy_mode: ProxyMode::Off,
            stats_enabled: false,
            stats_show_usage: false,
            stats_quota_kimi: false,
            stats_quota_opencode: false,
            stats_quota_refresh_secs: 300,
            close_to_tray: false,
            theme: ThemePreference::Light,
            notifications: false,
            kimi_path: r"C:\tools\kimi.exe".into(),
            kimi_port: 59000,
        };
        let text = serde_json::to_string(&s).unwrap();
        let back: Settings = serde_json::from_str(&text).unwrap();
        assert!(!back.remote_auto_connect);
        assert!(!back.show_remote_tab);
        assert_eq!(back.proxy_mode, ProxyMode::Off);
        assert_eq!(back.theme, ThemePreference::Light);
        assert_eq!(back.stats_quota_refresh_secs, 300);
        assert!(!back.stats_show_usage);
        assert_eq!(back.kimi_port, 59000);
        assert_eq!(back.kimi_path, r"C:\tools\kimi.exe");
    }

    #[test]
    fn corrupt_file_falls_back_to_default() {
        assert!(serde_json::from_str::<Settings>("not json").is_err());
        let s: Settings = serde_json::from_str(r#"{"theme":"dark","unknown":1}"#).unwrap();
        assert_eq!(s.theme, ThemePreference::Dark);
        assert!(s.stats_enabled);
    }
}
