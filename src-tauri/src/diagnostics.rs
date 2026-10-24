//! 设置页的诊断与 CLI 升级：状态汇总、版本与更新信息、确认后的一键升级。
//! 升级执行 `kimi upgrade -y`（CLI 自身检查最新版并安装），成功后重启本地服务
//! 让新二进制生效；「会打断对话并重启」的二次确认由设置页弹层完成，
//! Rust 侧只做防重入与进度事件回发。所有输出不落日志、不含 token。

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use tauri::{AppHandle, Emitter, Manager};

use crate::state::AppState;

const VERSION_TIMEOUT: Duration = Duration::from_secs(15);
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
    })
}

/// 后台收集 CLI 版本与更新信息，经 cli-info 事件回发。
/// 更新信息读 CLI 自动更新机制维护的 updates/latest.json（含 checkedAt 检查时间）：
/// `kimi upgrade` 没有只检查不安装的参数，这里不主动触发网络检查，
/// 真正的「检查并安装最新版」由升级按钮完成
#[tauri::command]
pub(crate) fn refresh_cli_info(app: AppHandle) {
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
        if let Some(home) = crate::usage::kimi_code_home() {
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
        }
        let _ = app.emit_to(
            "settings",
            "cli-info",
            serde_json::json!({
                "version": version,
                "latest": latest,
                "checked_at": checked_at,
                "error": error,
            }),
        );
    });
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
                refresh_cli_info(app.clone());
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
}
