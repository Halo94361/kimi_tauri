//! kimi web 子进程的生命周期：端口选择、spawn、就绪探测、看门狗与错误页

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::Ordering;
use std::time::Duration;

use regex::Regex;
use tauri::{AppHandle, Emitter, Manager};

use crate::settings::{ProxyMode, Settings};
use crate::state::{AppState, LocalEndpoint};
use crate::window::{show_main_window, switch_to_tab};

const KIMI_PORT: u16 = 58627;
const PROXY_PORT: u16 = 58787;
const READY_TIMEOUT: Duration = Duration::from_secs(30);

/// 选定 kimi web 监听端口：设置指定固定端口时直接交给 CLI（被占用会由 CLI 自行 +1，
/// 实际端口以 stdout 解析为准）；否则首选固定端口，被占用时让系统分配空闲端口。
/// 端口在 spawn 前选定并传入 --port，保证传入端口与实际监听端口一致；
/// stdout 正则解析仅作确认。drop 监听到 kimi 绑定之间有极小竞争窗口，
/// 最坏结果退化为启动失败进错误页，可接受
fn pick_port(configured: u16) -> u16 {
    use std::net::TcpListener;
    if configured != 0 {
        return configured;
    }
    if TcpListener::bind(("127.0.0.1", KIMI_PORT)).is_ok() {
        return KIMI_PORT;
    }
    TcpListener::bind(("127.0.0.1", 0))
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .unwrap_or(KIMI_PORT)
}

/// kimi 可执行文件候选：设置里手动指定时只用该路径（失败直接报错，不静默换路径），
/// 否则依次尝试 PATH 上的 kimi 与已知的 native 安装路径
pub(crate) fn kimi_candidates(settings: &Settings) -> Vec<String> {
    if !settings.kimi_path.is_empty() {
        return vec![settings.kimi_path.clone()];
    }
    let mut candidates: Vec<String> = vec!["kimi".into()];
    if let Ok(home) = std::env::var("USERPROFILE") {
        candidates.push(format!(r"{home}\.kimi-code\bin\kimi.exe"));
    }
    candidates
}

fn spawn_kimi(settings: &Settings) -> Result<Child, String> {
    let candidates = kimi_candidates(settings);
    let port = pick_port(settings.kimi_port);
    let mut last_err = String::new();
    for cand in &candidates {
        let mut cmd = Command::new(cand);
        // 不带 --rc：CLI 的 --rc 会在打印本地 URL 前同步等待远端握手，断网时
        // 本地界面完全不可用。隧道改为服务就绪后由壳经本地接口开启（remote 模块）
        cmd.args(["web", "--no-open", "--port", &port.to_string()])
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

enum ProxyHealth {
    /// 我们的代理在答健康检查
    Healthy,
    /// 端口被其他程序占用
    Occupied,
    /// 端口无人监听
    Down,
}

/// 通过健康检查区分“我们的代理”“别人的服务”“没人监听”
fn proxy_health() -> ProxyHealth {
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpStream};

    let addr = SocketAddr::from(([127, 0, 0, 1], PROXY_PORT));
    let Ok(mut stream) = TcpStream::connect_timeout(&addr, Duration::from_millis(300)) else {
        return ProxyHealth::Down;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
    let req = format!(
        "GET /__ocsp_health HTTP/1.1\r\nHost: 127.0.0.1:{PROXY_PORT}\r\nConnection: close\r\n\r\n"
    );
    if stream.write_all(req.as_bytes()).is_err() {
        return ProxyHealth::Occupied;
    }
    let mut buf = [0u8; 512];
    let n = stream.read(&mut buf).unwrap_or(0);
    let resp = String::from_utf8_lossy(&buf[..n]);
    let status_ok = resp
        .lines()
        .next()
        .map(|line| line.contains(" 200"))
        .unwrap_or(false);
    if status_ok && resp.contains("opencode-session-proxy") {
        ProxyHealth::Healthy
    } else {
        ProxyHealth::Occupied
    }
}

/// 配置里是否真的把某个 provider 指向了本代理
fn proxy_required() -> bool {
    let Some(home) = crate::usage::kimi_code_home() else {
        return false;
    };
    std::fs::read_to_string(home.join("config.toml"))
        .map(|text| text.contains("127.0.0.1:58787") || text.contains("localhost:58787"))
        .unwrap_or(false)
}

/// 代理启动策略：自动 = 只有 CLI 配置指向本代理才启动（不用 OpenCode Go 时
/// 不多起一个进程）；始终 = 无条件启动；关闭 = 不启动也不提示
fn proxy_enabled(settings: &Settings) -> bool {
    match settings.proxy_mode {
        ProxyMode::Off => false,
        ProxyMode::Always => true,
        ProxyMode::Auto => proxy_required(),
    }
}

fn set_proxy_error(app: &AppHandle, reason: Option<String>) {
    let state = app.state::<AppState>();
    *state.proxy_error.lock().unwrap() = reason.clone();
    let _ = app.emit_to(
        "tabbar",
        "proxy-status",
        serde_json::json!({ "failed": reason.is_some(), "reason": reason.unwrap_or_default() }),
    );
}

/// 可选组件：把请求体里的 prompt_cache_key 映射为 OpenCode Go 要求的
/// x-opencode-session 头（CLI 自身不发，见 MoonshotAI/kimi-code#3506）。
/// 依次从 exe 同目录、~/.kimi-code/bin、PATH 查找；找不到时静默跳过，
/// 不影响未使用 OpenCode Go 的用户
fn spawn_proxy() -> Option<Child> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("opencode-session-proxy.exe"));
        }
    }
    if let Ok(home) = std::env::var("USERPROFILE") {
        candidates.push(
            PathBuf::from(home)
                .join(".kimi-code")
                .join("bin")
                .join("opencode-session-proxy.exe"),
        );
    }
    candidates.push(PathBuf::from("opencode-session-proxy.exe"));

    for cand in &candidates {
        let mut cmd = Command::new(cand);
        cmd.env("OCSP_LISTEN", format!("127.0.0.1:{PROXY_PORT}"))
            .env("OCSP_UPSTREAM", "https://opencode.ai/inference/go")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x08000000);
        }
        if let Ok(child) = cmd.spawn() {
            return Some(child);
        }
    }
    None
}

/// 确保代理可用：健康则复用；否则拉起新进程并等待就绪。
/// 失败原因写入 AppState，并通过 proxy-status 事件推给选项卡栏
fn start_proxy(app: &AppHandle) -> Result<(), String> {
    match proxy_health() {
        ProxyHealth::Healthy => {
            set_proxy_error(app, None);
            return Ok(());
        }
        ProxyHealth::Occupied => {
            return Err(report_proxy_failure(
                app,
                format!("端口 {PROXY_PORT} 已被其他程序占用"),
            ));
        }
        ProxyHealth::Down => {}
    }

    let Some(child) = spawn_proxy() else {
        return Err(report_proxy_failure(
            app,
            "未找到 opencode-session-proxy.exe".to_string(),
        ));
    };
    #[cfg(windows)]
    assign_to_job(app, &child);

    {
        let state = app.state::<AppState>();
        let mut slot = state.proxy.lock().unwrap();
        if let Some(mut old) = slot.take() {
            let _ = old.kill();
            let _ = old.wait();
        }
        *slot = Some(child);
    }

    // 最多等 3 秒，确认健康检查真的由我们的代理应答
    for _ in 0..15 {
        std::thread::sleep(Duration::from_millis(200));
        if matches!(proxy_health(), ProxyHealth::Healthy) {
            set_proxy_error(app, None);
            return Ok(());
        }
    }

    Err(report_proxy_failure(
        app,
        format!("代理未能在 127.0.0.1:{PROXY_PORT} 就绪"),
    ))
}

/// 只有代理模式确实要求启动时才提示，否则清掉旧提示
fn report_proxy_failure(app: &AppHandle, reason: String) -> String {
    let enabled = proxy_enabled(&app.state::<AppState>().settings.lock().unwrap());
    if enabled {
        set_proxy_error(app, Some(reason.clone()));
    } else {
        set_proxy_error(app, None);
    }
    reason
}

fn kill_proxy(state: &AppState) {
    if let Some(mut child) = state.proxy.lock().unwrap().take() {
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// 把子进程挂入 Job Object：壳退出时由系统级联回收
#[cfg(windows)]
fn assign_to_job(app: &AppHandle, child: &Child) {
    let state = app.state::<AppState>();
    let mut job = state.job.lock().unwrap();
    if job.is_none() {
        if let Ok(j) = crate::win_job::Job::new() {
            *job = Some(j);
        }
    }
    if let Some(j) = job.as_ref() {
        let _ = j.assign(child);
    }
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
    kill_proxy(&state);
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
    // 代次先自增再杀旧进程：旧 reader 线程 EOF 后看到代次不匹配会直接退出，
    // 不会误杀刚拉起的新子进程（原实现只看 quitting，存在竞态）
    let gen = {
        let state = app.state::<AppState>();
        let gen = state.server_gen.fetch_add(1, Ordering::SeqCst) + 1;
        *state.quitting.lock().unwrap() = false;
        *state.ready.lock().unwrap() = false;
        *state.last_error.lock().unwrap() = String::new();
        kill_child(&state);
        gen
    };
    crate::remote::reset(app, gen);

    let settings = app.state::<AppState>().settings.lock().unwrap().clone();
    // 记录本次启动实际使用的配置，供设置页判断「改了但还没重启生效」
    *app.state::<AppState>().launched.lock().unwrap() =
        (settings.kimi_path.clone(), settings.kimi_port);

    // 按代理模式决定是否拉起：kimi 从启动到首次模型请求还有 UI 初始化时间。
    // 失败不阻塞 kimi，原因经 proxy-status 事件显示在选项卡栏
    if proxy_enabled(&settings) {
        let _ = start_proxy(app);
    } else {
        set_proxy_error(app, None);
    }

    let mut child = match spawn_kimi(&settings) {
        Ok(c) => c,
        Err(e) => {
            show_error(app, &e);
            return;
        }
    };

    // 把 kimi.exe 挂入 Job Object：之后它启动的孙进程（含 MCP server）自动归入
    // 同一作业；job 创建/绑定失败时退回原有的直接 kill 清理，不影响启动
    #[cfg(windows)]
    assign_to_job(app, &child);

    let stdout = child.stdout.take().expect("stdout piped");
    app.state::<AppState>().child.lock().unwrap().replace(child);

    // 读子进程 stdout：解析端口与 token，EOF 意味着服务退出。
    // 用 read_until + 有损解码：原 .lines() 遇到非 UTF-8 会直接结束循环，
    // 被误判成“子进程退出”；同时剥离 ANSI，避免 token 尾部带上 \x1b[0m
    let app_reader = app.clone();
    std::thread::spawn(move || {
        let url_re = Regex::new(r#"http://[\w.%-]+:(\d+)[^\s"']*#token=([^\s"']+)"#).unwrap();
        let port_re = Regex::new(r"127\.0\.0\.1:(\d+)").unwrap();
        let token_re =
            Regex::new(r#"(?i)#token=([^\s"']+)|token\s*[:=]\s*([A-Za-z0-9._\-]+)"#).unwrap();
        let ansi_re = Regex::new(r"\x1b\[[0-9;?]*[ -/]*[@-~]").unwrap();
        let mut port: Option<String> = None;
        let mut token: Option<String> = None;
        let mut navigated = false;

        let mut reader = BufReader::new(stdout);
        let mut raw = Vec::new();
        loop {
            raw.clear();
            match reader.read_until(b'\n', &mut raw) {
                Ok(0) => break,
                Ok(_) => {}
                Err(_) => break,
            }
            let line = String::from_utf8_lossy(&raw);
            let line = ansi_re.replace_all(&line, "");
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
                    let state = app_reader.state::<AppState>();
                    if state.server_gen.load(Ordering::SeqCst) != gen {
                        return; // 旧进程的迟到输出：忽略，不覆盖新端点
                    }
                    if let Ok(port_num) = p.parse::<u16>() {
                        *state.endpoint.lock().unwrap() = Some(LocalEndpoint {
                            port: port_num,
                            token: t.clone(),
                        });
                    }
                    *state.ready.lock().unwrap() = true;
                    navigate_local(&app_reader, &format!("http://127.0.0.1:{p}/#token={t}"));
                    // 本地界面已可用，后台开远程隧道（失败只提示，不阻塞本地）；
                    // 设置关闭自动开启时保持 Idle，选项卡栏不出现提示条，
                    // 已建立的旧隧道已被本次重启随子进程一并回收
                    if app_reader
                        .state::<AppState>()
                        .settings
                        .lock()
                        .unwrap()
                        .remote_auto_connect
                    {
                        crate::remote::start_enable(&app_reader);
                    }
                    navigated = true;
                }
            }
            // 导航完成后继续排空 stdout，防止管道写满阻塞子进程
        }

        // stdout 关闭：子进程已退出（或已被重启替换）
        let state = app_reader.state::<AppState>();
        if state.server_gen.load(Ordering::SeqCst) != gen {
            return;
        }
        if *state.quitting.lock().unwrap() {
            return;
        }
        kill_child(&state);
        crate::remote::mark_idle(&app_reader, gen);
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
        if state.server_gen.load(Ordering::SeqCst) != gen {
            return; // 已重启过，旧看门狗不再干预
        }
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

#[tauri::command]
pub(crate) fn proxy_status(state: tauri::State<AppState>) -> serde_json::Value {
    let reason = state.proxy_error.lock().unwrap().clone();
    serde_json::json!({ "failed": reason.is_some(), "reason": reason.unwrap_or_default() })
}

#[tauri::command]
pub(crate) fn retry_proxy(app: AppHandle) {
    let _ = start_proxy(&app);
}

/// 代理模式变更的即时副作用：需要启动则后台拉起（健康检查最多等 3s，不能
/// 阻塞命令），不再需要则停掉自己拉起的子进程并清掉提示
pub(crate) fn apply_settings(app: &AppHandle, prev: &Settings, next: &Settings) {
    if prev.proxy_mode == next.proxy_mode {
        return;
    }
    if proxy_enabled(next) {
        let app2 = app.clone();
        std::thread::spawn(move || {
            let _ = start_proxy(&app2);
        });
    } else {
        let state = app.state::<AppState>();
        kill_proxy(&state);
        set_proxy_error(app, None);
    }
}
