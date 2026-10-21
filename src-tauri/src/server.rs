//! kimi web 子进程的生命周期：端口选择、spawn、就绪探测、看门狗与错误页

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use regex::Regex;
use tauri::{AppHandle, Manager};

use crate::state::AppState;
use crate::window::{show_main_window, switch_to_tab};

const KIMI_PORT: u16 = 58627;
const READY_TIMEOUT: Duration = Duration::from_secs(30);

/// 选定 kimi web 监听端口：首选固定端口，被占用时让系统分配空闲端口。
/// 端口在 spawn 前选定并传入 --port，保证传入端口与实际监听端口一致；
/// stdout 正则解析仅作确认。drop 监听到 kimi 绑定之间有极小竞争窗口，
/// 最坏结果退化为启动失败进错误页，可接受
fn pick_port() -> u16 {
    use std::net::TcpListener;
    if TcpListener::bind(("127.0.0.1", KIMI_PORT)).is_ok() {
        return KIMI_PORT;
    }
    TcpListener::bind(("127.0.0.1", 0))
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .unwrap_or(KIMI_PORT)
}

/// 依次尝试 PATH 上的 kimi 与已知的 native 安装路径
fn spawn_kimi() -> Result<Child, String> {
    let mut candidates: Vec<String> = vec!["kimi".into()];
    if let Ok(home) = std::env::var("USERPROFILE") {
        candidates.push(format!(r"{home}\.kimi-code\bin\kimi.exe"));
    }
    let port = pick_port();
    let mut last_err = String::new();
    for cand in &candidates {
        let mut cmd = Command::new(cand);
        // --rc：本地 web 启动的同时注册到 Kimi Remote Control，
        // 远端（手机/另一台电脑）可通过 code-rc.kimi.com 接管本机会话
        cmd.args(["web", "--rc", "--no-open", "--port", &port.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // CREATE_NO_WINDOW：避免拉起子进程时闪现控制台黑窗
            cmd.creation_flags(0x08000000);
        }
        match cmd.spawn() {
            Ok(child) => return Ok(child),
            Err(e) => last_err = format!("{cand}: {e}"),
        }
    }
    Err(format!(
        "无法启动 kimi web（{last_err}）。请确认已安装 Kimi Code CLI。"
    ))
}

/// 导航本地内容 WebView（本地 kimi web / 壳内建页面都加载在这个 WebView 里）
pub(crate) fn navigate_local(app: &AppHandle, url: &str) {
    if let Some(w) = app.get_webview("local") {
        if let Ok(u) = url.parse() {
            let _ = w.navigate(u);
        }
    }
}

pub(crate) fn app_page(app: &AppHandle, page: &str) -> Option<String> {
    let origin = app.state::<AppState>().app_origin.lock().unwrap().clone();
    if origin.is_empty() {
        None
    } else {
        Some(format!("{origin}{page}"))
    }
}

pub(crate) fn show_error(app: &AppHandle, msg: &str) {
    *app.state::<AppState>().last_error.lock().unwrap() = msg.to_string();
    // 服务不可用时统计浮窗没有意义，等下次就绪再显示
    *app.state::<AppState>().ready.lock().unwrap() = false;
    if let Some(url) = app_page(app, "error.html") {
        navigate_local(app, &url);
    }
    // 错误发生在本地后端：强制切回"本地"选项卡让错误页可见
    switch_to_tab(app, "local");
    // 托盘模式下窗口可能处于隐藏状态，出错时主动唤出让用户感知
    show_main_window(app);
}

/// 真正退出应用：标记 quitting、杀掉 kimi web 子进程并退出事件循环
pub(crate) fn quit_app(app: &AppHandle) {
    let state = app.state::<AppState>();
    *state.quitting.lock().unwrap() = true;
    kill_child(&state);
    app.exit(0);
}

pub(crate) fn kill_child(state: &AppState) {
    if let Some(mut child) = state.child.lock().unwrap().take() {
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// 启动（或重启）kimi web 子进程，并在就绪后把主窗口导航到 web UI
pub(crate) fn start_server(app: &AppHandle) {
    {
        let state = app.state::<AppState>();
        *state.quitting.lock().unwrap() = false;
        *state.ready.lock().unwrap() = false;
        *state.last_error.lock().unwrap() = String::new();
        kill_child(&state);
    }

    let mut child = match spawn_kimi() {
        Ok(c) => c,
        Err(e) => {
            show_error(app, &e);
            return;
        }
    };

    // 把 kimi.exe 挂入 Job Object：之后它启动的孙进程（含 MCP server）自动归入
    // 同一作业；job 创建/绑定失败时退回原有的直接 kill 清理，不影响启动
    #[cfg(windows)]
    {
        let state = app.state::<AppState>();
        let mut job = state.job.lock().unwrap();
        if job.is_none() {
            if let Ok(j) = crate::win_job::Job::new() {
                *job = Some(j);
            }
        }
        if let Some(j) = job.as_ref() {
            let _ = j.assign(&child);
        }
    }

    let stdout = child.stdout.take().expect("stdout piped");
    app.state::<AppState>().child.lock().unwrap().replace(child);

    // 读子进程 stdout：解析端口与 token，EOF 意味着服务退出
    let app_reader = app.clone();
    std::thread::spawn(move || {
        let url_re = Regex::new(r#"http://[\w.%-]+:(\d+)[^\s"']*#token=([^\s"']+)"#).unwrap();
        let port_re = Regex::new(r"127\.0\.0\.1:(\d+)").unwrap();
        let token_re = Regex::new(r#"(?i)#token=([^\s"']+)|token\s*[:=]\s*([A-Za-z0-9._\-]+)"#).unwrap();
        let mut port: Option<String> = None;
        let mut token: Option<String> = None;
        let mut navigated = false;

        for line in BufReader::new(stdout).lines() {
            let line = match line {
                Ok(l) => l,
                Err(_) => break,
            };
            if let Some(c) = url_re.captures(&line) {
                port = Some(c[1].to_string());
                token = Some(c[2].to_string());
            } else {
                if port.is_none() {
                    if let Some(c) = port_re.captures(&line) {
                        port = Some(c[1].to_string());
                    }
                }
                if token.is_none() {
                    if let Some(c) = token_re.captures(&line) {
                        token = c
                            .get(1)
                            .or_else(|| c.get(2))
                            .map(|m| m.as_str().to_string());
                    }
                }
            }
            if !navigated {
                if let (Some(p), Some(t)) = (&port, &token) {
                    *app_reader.state::<AppState>().ready.lock().unwrap() = true;
                    navigate_local(&app_reader, &format!("http://127.0.0.1:{p}/#token={t}"));
                    navigated = true;
                }
            }
            // 导航完成后继续排空 stdout，防止管道写满阻塞子进程
        }

        // stdout 关闭：子进程已退出
        let state = app_reader.state::<AppState>();
        if *state.quitting.lock().unwrap() {
            return;
        }
        kill_child(&state);
        if navigated {
            show_error(&app_reader, "kimi web 服务已断开。");
        } else {
            show_error(&app_reader, "kimi web 启动失败或提前退出。");
        }
    });

    // 就绪超时看门狗
    let app_watchdog = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(READY_TIMEOUT);
        let state = app_watchdog.state::<AppState>();
        if *state.quitting.lock().unwrap() || *state.ready.lock().unwrap() {
            return;
        }
        kill_child(&state);
        show_error(&app_watchdog, "kimi web 启动超时。");
    });
}

#[tauri::command]
pub(crate) fn last_error(state: tauri::State<AppState>) -> String {
    state.last_error.lock().unwrap().clone()
}

#[tauri::command]
pub(crate) fn restart_server(app: AppHandle) {
    if let Some(url) = app_page(&app, "index.html") {
        navigate_local(&app, &url);
    }
    start_server(&app);
}
