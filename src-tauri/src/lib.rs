use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;

use regex::Regex;
use tauri::menu::{MenuBuilder, MenuItemBuilder};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::webview::NewWindowResponse;
use tauri::{AppHandle, Manager, Url, WebviewUrl, WebviewWindowBuilder};

const KIMI_PORT: u16 = 58627;
const READY_TIMEOUT: Duration = Duration::from_secs(30);

/// Windows Job Object：把 kimi.exe 绑定进带 KILL_ON_JOB_CLOSE 的作业，
/// 使壳进程以任何方式退出（正常关闭、被强杀、崩溃）时，系统都会级联回收
/// job 内的 kimi.exe 及其启动的所有 MCP 孙进程，避免孤儿进程残留
#[cfg(windows)]
mod win_job {
    use std::os::windows::io::AsRawHandle;
    use std::process::Child;

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    pub struct Job(HANDLE);

    // HANDLE 仅是内核对象的标识值，跨线程传递与共享不涉及数据竞争
    unsafe impl Send for Job {}
    unsafe impl Sync for Job {}

    impl Job {
        pub fn new() -> Result<Self, String> {
            unsafe {
                let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if handle.is_null() {
                    return Err(format!(
                        "CreateJobObjectW 失败: {}",
                        std::io::Error::last_os_error()
                    ));
                }
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                if SetInformationJobObject(
                    handle,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const _,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                ) == 0
                {
                    let err = std::io::Error::last_os_error();
                    CloseHandle(handle);
                    return Err(format!("SetInformationJobObject 失败: {err}"));
                }
                Ok(Self(handle))
            }
        }

        pub fn assign(&self, child: &Child) -> Result<(), String> {
            unsafe {
                if AssignProcessToJobObject(self.0, child.as_raw_handle() as HANDLE) == 0 {
                    return Err(format!(
                        "AssignProcessToJobObject 失败: {}",
                        std::io::Error::last_os_error()
                    ));
                }
            }
            Ok(())
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.0) };
        }
    }
}

#[derive(Default)]
struct AppState {
    child: Mutex<Option<Child>>,
    /// 持有 job handle 使整个应用生命周期内 job 存活；进程退出时 handle 关闭，
    /// 触发 KILL_ON_JOB_CLOSE 级联回收 job 内所有进程
    #[cfg(windows)]
    job: Mutex<Option<win_job::Job>>,
    ready: Mutex<bool>,
    quitting: Mutex<bool>,
    last_error: Mutex<String>,
    app_origin: Mutex<String>,
}

/// 允许在窗口内导航的来源：Tauri 应用自身页面与本机 kimi web 服务
fn is_local(url: &Url) -> bool {
    match url.host_str() {
        Some("127.0.0.1") | Some("localhost") | Some("tauri.localhost") => true,
        None => url.scheme() == "tauri",
        _ => false,
    }
}

fn open_external(url: &Url) {
    let _ = tauri_plugin_opener::open_url(url.as_str(), None::<&str>);
}

/// 依次尝试 PATH 上的 kimi 与已知的 native 安装路径
fn spawn_kimi() -> Result<Child, String> {
    let mut candidates: Vec<String> = vec!["kimi".into()];
    if let Ok(home) = std::env::var("USERPROFILE") {
        candidates.push(format!(r"{home}\.kimi-code\bin\kimi.exe"));
    }
    let mut last_err = String::new();
    for cand in &candidates {
        let mut cmd = Command::new(cand);
        cmd.args(["web", "--no-open", "--port", &KIMI_PORT.to_string()])
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

fn navigate_main(app: &AppHandle, url: &str) {
    if let Some(w) = app.get_webview_window("main") {
        if let Ok(u) = url.parse() {
            let _ = w.navigate(u);
        }
    }
}

fn app_page(app: &AppHandle, page: &str) -> Option<String> {
    let origin = app.state::<AppState>().app_origin.lock().unwrap().clone();
    if origin.is_empty() {
        None
    } else {
        Some(format!("{origin}{page}"))
    }
}

/// 显示并聚焦主窗口（从托盘恢复时调用）
fn show_main_window(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
    }
}

fn show_error(app: &AppHandle, msg: &str) {
    *app.state::<AppState>().last_error.lock().unwrap() = msg.to_string();
    if let Some(url) = app_page(app, "error.html") {
        navigate_main(app, &url);
    }
    // 托盘模式下窗口可能处于隐藏状态，出错时主动唤出让用户感知
    show_main_window(app);
}

/// 真正退出应用：标记 quitting、杀掉 kimi web 子进程并退出事件循环
fn quit_app(app: &AppHandle) {
    let state = app.state::<AppState>();
    *state.quitting.lock().unwrap() = true;
    kill_child(&state);
    app.exit(0);
}

fn kill_child(state: &AppState) {
    if let Some(mut child) = state.child.lock().unwrap().take() {
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// 启动（或重启）kimi web 子进程，并在就绪后把主窗口导航到 web UI
fn start_server(app: &AppHandle) {
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
            if let Ok(j) = win_job::Job::new() {
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
                    navigate_main(&app_reader, &format!("http://127.0.0.1:{p}/#token={t}"));
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
fn last_error(state: tauri::State<AppState>) -> String {
    state.last_error.lock().unwrap().clone()
}

#[tauri::command]
fn restart_server(app: AppHandle) {
    if let Some(url) = app_page(&app, "index.html") {
        navigate_main(&app, &url);
    }
    start_server(&app);
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![last_error, restart_server])
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
                    "quit" => quit_app(app),
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

            let win = WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()))
                .title("Kimi Code")
                .inner_size(win_w, win_h)
                .min_inner_size(720.0, 480.0)
                .center()
                // 关闭 Tauri 自带拖放拦截，让 web UI 的 HTML5 原生 drop 事件生效
                .disable_drag_drop_handler()
                // WebView2 默认不响应浏览器式刷新快捷键，注入脚本补齐：
                // F5 / Ctrl+R 触发页面重载，用于长时间会话后释放渲染进程内存
                .initialization_script(
                    r#"window.addEventListener("keydown", (e) => {
                        if (e.key === "F5" || (e.ctrlKey && !e.shiftKey && !e.altKey && (e.key === "r" || e.key === "R"))) {
                            e.preventDefault();
                            location.reload();
                        }
                    });"#,
                )
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
                })
                .build()?;

            // 记录应用页面源（如 http://tauri.localhost/），供错误页/加载页导航使用
            if let Ok(mut u) = win.url() {
                u.set_path("/");
                u.set_query(None);
                u.set_fragment(None);
                *app.state::<AppState>().app_origin.lock().unwrap() = u.to_string();
            }

            start_server(app.handle());
            Ok(())
        })
        .on_window_event(|window, event| {
            // 托盘模式下关窗不退出：拦截关闭请求并隐藏窗口，
            // kimi web 子进程继续运行；真正退出走托盘菜单的"退出"
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                if *window.state::<AppState>().quitting.lock().unwrap() {
                    return;
                }
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
