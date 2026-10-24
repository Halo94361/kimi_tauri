//! 设置页的诊断与 CLI 升级：状态汇总、版本与更新信息、确认后的一键升级。
//! 升级执行 `kimi upgrade -y`（CLI 自身检查最新版并安装），成功后重启本地服务
//! 让新二进制生效；「会打断对话并重启」的二次确认由设置页弹层完成，
//! Rust 侧只做防重入与进度事件回发。所有输出不落日志、不含 token。

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use tauri::{AppHandle, Emitter, Manager};

use crate::state::AppState;

const VERSION_TIMEOUT: Duration = Duration::from_secs(15);
/// 更新检查只读 CDN 的 /latest 文本，超时不影响设置页其他信息
const UPDATE_CHECK_TIMEOUT: Duration = Duration::from_secs(8);
const MAINLAND_LATEST_URL: &str = "https://code.kimi.com/kimi-code/latest";
const GLOBAL_LATEST_URL: &str = "https://code.kimi.ai/kimi-code/latest";
/// 下载与安装可能较慢，给足余量；超时杀进程并回报，不无限等待
const UPGRADE_TIMEOUT: Duration = Duration::from_secs(600);

/// 状态汇总（快速路径，不 spawn 任何进程）；token 不出现在任何字段里
#[tauri::command]
pub(crate) fn get_diagnostics(app: AppHandle) -> serde_json::Value {
    let state = app.state::<AppState>();
    let port = state.endpoint.lock().unwrap().as_ref().map(|ep| ep.port);
    let ready = *state.ready.lock().unwrap();
    let remote = state.remote.lock().unwrap().clone();
    let proxy_error = state.proxy_error.lock().unwrap().clone();
    let proxy_running = state.proxy.lock().unwrap().is_some();
    let upgrading = *state.upgrading.lock().unwrap();
    let restart_pending = {
        let settings = state.settings.lock().unwrap();
        let launched = state.launched.lock().unwrap();
        settings.kimi_path != launched.0 || settings.kimi_port != launched.1
    };
    serde_json::json!({
        "app_version": env!("CARGO_PKG_VERSION"),
        "server_ready": ready,
        "port": port,
        "remote": crate::remote::status_payload(&remote),
        "proxy": {
            "failed": proxy_error.is_some(),
            "reason": proxy_error.unwrap_or_default(),
            "running": proxy_running,
        },
        "upgrading": upgrading,
        "restart_pending": restart_pending,
    })
}

/// 后台收集 CLI 版本与更新信息，经 cli-info 事件回发。
/// 刷新按钮会向 CLI 更新源 CDN 的 `/latest` 做一次只读检查（跟随 302）；
/// 失败时回退 `updates/latest.json` 的 `latest` / `checkedAt`，并带
/// `update_error` 让设置页标注「显示缓存」。这里不执行 `kimi upgrade`，
/// 安装仍只由升级按钮触发。
#[tauri::command]
pub(crate) fn refresh_cli_info(check: Option<bool>, app: AppHandle) {
    let check = check.unwrap_or(false);
    std::thread::spawn(move || {
        let settings = app.state::<AppState>().settings.lock().unwrap().clone();
        let mut version = None;
        let mut error = None;
        for cand in crate::server::kimi_candidates(&settings) {
            let mut cmd = Command::new(&cand);
            cmd.arg("--version");
            match run_capture(&mut cmd, VERSION_TIMEOUT) {
                Ok(out) => {
                    version = out
                        .lines()
                        .next()
                        .and_then(|line| line.split_whitespace().last())
                        .map(str::to_string);
                    break;
                }
                Err(e) => error = Some(format!("{cand}: {e}")),
            }
        }

        let mut latest = None;
        let mut checked_at = None;
        let mut update_error = None;
        match crate::usage::kimi_code_home() {
            Some(home) => {
                // 先读 CLI 自己维护的缓存，网络检查失败时保留展示
                if let Ok(text) = std::fs::read_to_string(home.join("updates").join("latest.json")) {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                        latest = v
                            .get("latest")
                            .and_then(|x| x.as_str())
                            .or_else(|| v.get("manifest")?.get("version")?.as_str())
                            .map(str::to_string);
                        checked_at = v
                            .get("checkedAt")
                            .and_then(|x| x.as_str())
                            .map(str::to_string);
                    }
                }
                if check {
                    match check_latest_version(&home) {
                        Ok(v) => {
                            latest = Some(v);
                            checked_at = Some(now_rfc3339());
                        }
                        Err(e) => update_error = Some(e),
                    }
                }
            }
            None => {
                if check {
                    update_error = Some("找不到 Kimi 数据目录".into());
                }
            }
        }

        let _ = app.emit_to(
            "settings",
            "cli-info",
            serde_json::json!({
                "version": version,
                "latest": latest,
                "checked_at": checked_at,
                "error": error,
                "update_error": update_error,
            }),
        );
    });
}

/// 只读检查 CLI 更新源：`/latest` 返回纯文本 semver，CDN 可能先 302。
/// 区域与 CLI 一致：优先读 `~/.kimi-code/region`，再回退 managed provider 的 base_url。
fn check_latest_version(home: &Path) -> Result<String, String> {
    let url = if update_region_is_global(home) {
        GLOBAL_LATEST_URL
    } else {
        MAINLAND_LATEST_URL
    };
    let client = reqwest::blocking::Client::builder()
        .timeout(UPDATE_CHECK_TIMEOUT)
        .user_agent(concat!("tauri-kimi/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| format!("HTTP 客户端初始化失败：{e}"))?;
    let response = client
        .get(url)
        .header("Accept", "text/plain")
        .send()
        .map_err(|e| format!("更新检查请求失败：{e}"))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("更新检查返回 HTTP {}", status.as_u16()));
    }
    let text = response
        .text()
        .map_err(|e| format!("更新检查响应读取失败：{e}"))?;
    let latest = text.trim().to_string();
    if !is_version_like(&latest) {
        return Err("更新检查返回了意外的内容".into());
    }
    Ok(latest)
}

fn update_region_is_global(home: &Path) -> bool {
    let marker = std::fs::read_to_string(home.join("region")).ok();
    let managed_base = std::fs::read_to_string(home.join("config.toml"))
        .ok()
        .and_then(|text| text.parse::<toml::Value>().ok())
        .and_then(|value| {
            value
                .get("providers")?
                .get("managed:kimi-code")?
                .get("base_url")?
                .as_str()
                .map(str::to_string)
        });
    region_is_global(marker.as_deref(), managed_base.as_deref())
}

fn region_is_global(marker: Option<&str>, managed_base: Option<&str>) -> bool {
    match marker.map(str::trim) {
        Some("global") => return true,
        Some("cn") | Some("mainland-cn") => return false,
        _ => {}
    }
    managed_base.is_some_and(|base| base.contains("kimi.ai"))
}

/// `/latest` 应是 `2.3.1` 这类 semver；HTML 错误页或空串一律拒绝
fn is_version_like(value: &str) -> bool {
    let core = value.split(['-', '+']).next().unwrap_or("");
    let parts: Vec<&str> = core.split('.').collect();
    parts.len() >= 3
        && parts
            .iter()
            .all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()))
}

fn now_rfc3339() -> String {
    use time::{format_description::well_known::Rfc3339, OffsetDateTime};
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_default()
}

/// 一键升级：二次确认由设置页完成。成功后自动重启本地服务，
/// rc 是否随重启自动重连由「启动时自动开启远程控制」设置决定
#[tauri::command]
pub(crate) fn upgrade_cli(app: AppHandle) -> Result<(), String> {
    {
        let state = app.state::<AppState>();
        let mut upgrading = state.upgrading.lock().unwrap();
        if *upgrading {
            return Err("正在升级，请稍候".into());
        }
        *upgrading = true;
    }
    emit_stage(&app, "running", "正在下载并安装最新版本…");
    std::thread::spawn(move || {
        match run_upgrade(&app) {
            Ok(()) => {
                emit_stage(&app, "restarting", "安装完成，正在重启本地服务…");
                // 与错误页「重新启动服务」同一路径：代次先自增再杀旧进程，
                // 旧 reader 静默退出，不会把这次重启当成服务异常弹错误页
                crate::server::restart_server(app.clone());
                *app.state::<AppState>().upgrading.lock().unwrap() = false;
                emit_stage(&app, "done", "升级完成，本地服务已重启");
                refresh_cli_info(Some(true), app.clone());
            }
            Err(e) => {
                *app.state::<AppState>().upgrading.lock().unwrap() = false;
                emit_stage(&app, "failed", &e);
            }
        }
    });
    Ok(())
}

fn emit_stage(app: &AppHandle, stage: &str, message: &str) {
    let _ = app.emit_to(
        "settings",
        "cli-upgrade",
        serde_json::json!({ "stage": stage, "message": message }),
    );
}

fn run_upgrade(app: &AppHandle) -> Result<(), String> {
    let settings = app.state::<AppState>().settings.lock().unwrap().clone();
    let mut last_err = String::new();
    for cand in crate::server::kimi_candidates(&settings) {
        let mut cmd = Command::new(&cand);
        cmd.args(["upgrade", "-y"]);
        match run_capture(&mut cmd, UPGRADE_TIMEOUT) {
            Ok(_) => return Ok(()),
            Err(e) => last_err = e,
        }
    }
    if last_err.is_empty() {
        Err("未找到 kimi 可执行文件".into())
    } else {
        Err(last_err)
    }
}

/// 打开 CLI 配置目录 / config.toml / 壳设置目录（系统默认程序）
#[tauri::command]
pub(crate) fn open_config_target(target: String, app: AppHandle) -> Result<(), String> {
    let home = crate::usage::kimi_code_home();
    let path = match target.as_str() {
        "cli_dir" => home,
        "cli_config" => home.map(|p| p.join("config.toml")),
        "shell_dir" => app.path().app_config_dir().ok(),
        _ => return Err(format!("未知目标: {target}")),
    }
    .ok_or("无法定位目录")?;
    if target == "shell_dir" {
        std::fs::create_dir_all(&path).map_err(|e| e.to_string())?;
    }
    tauri_plugin_opener::open_path(&path, None::<&str>).map_err(|e| e.to_string())
}

/// 运行子命令并收集 stdout+stderr，成功才返回 Ok；超时杀进程。
/// 管道读取放在独立线程，避免输出超过缓冲后父子进程互相等待
fn run_capture(cmd: &mut Command, timeout: Duration) -> Result<String, String> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW：避免闪现控制台黑窗
        cmd.creation_flags(0x08000000);
    }
    let mut child = cmd.spawn().map_err(|e| e.to_string())?;
    let mut readers = Vec::new();
    if let Some(stdout) = child.stdout.take() {
        readers.push(read_pipe(stdout));
    }
    if let Some(stderr) = child.stderr.take() {
        readers.push(read_pipe(stderr));
    }
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Ok(s),
            Ok(None) => {}
            Err(e) => break Err(e.to_string()),
        }
        if started.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            break Err("执行超时".to_string());
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let mut output = String::new();
    for reader in readers {
        if let Ok(text) = reader.join() {
            output.push_str(&text);
        }
    }
    let output = tail(&output, 2000);
    match status {
        Ok(s) if s.success() => Ok(output),
        Ok(s) => Err(format!("命令未成功（{s}）：\n{output}")),
        Err(e) => Err(format!("{e}\n{output}")),
    }
}

fn read_pipe<S: Read + Send + 'static>(mut stream: S) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        let mut buf = String::new();
        let _ = stream.read_to_string(&mut buf);
        buf
    })
}

/// 失败输出只保留尾部，避免把整段下载日志塞进提示条
fn tail(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.trim().to_string();
    }
    let mut start = text.len() - max;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    format!("…\n{}", text[start..].trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_keeps_end_and_utf8_boundaries() {
        assert_eq!(tail("abc", 10), "abc");
        let long = "字".repeat(1000);
        let out = tail(&long, 30);
        assert!(out.ends_with(&"字".repeat(10)));
        assert!(out.starts_with('…'));
    }

    #[test]
    fn version_like_accepts_semver_and_rejects_html() {
        assert!(is_version_like("2.3.1"));
        assert!(is_version_like("2.3.1-beta.1"));
        assert!(is_version_like("2.3.1+build.5"));
        assert!(!is_version_like("<html>302</html>"));
        assert!(!is_version_like(""));
        assert!(!is_version_like("2.3"));
    }

    #[test]
    fn region_marker_wins_over_managed_base() {
        assert!(region_is_global(
            Some("global"),
            Some("https://api.kimi.com/coding/v1")
        ));
        assert!(!region_is_global(
            Some("cn"),
            Some("https://api.kimi.ai/coding/v1")
        ));
        assert!(region_is_global(
            None,
            Some("https://api.kimi.ai/coding/v1")
        ));
        assert!(!region_is_global(
            None,
            Some("https://api.kimi.com/coding/v1")
        ));
        assert!(!region_is_global(None, None));
    }

    #[test]
    #[ignore = "需要网络，手动运行"]
    fn live_check_latest_version() {
        let home = crate::usage::kimi_code_home().expect("Kimi 数据目录");
        let latest = check_latest_version(&home).expect("更新检查失败");
        eprintln!("latest={latest}");
    }
}
