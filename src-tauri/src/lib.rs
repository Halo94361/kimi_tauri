use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;

use regex::Regex;
use tauri::menu::{MenuBuilder, MenuItemBuilder};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::webview::{NewWindowResponse, PermissionKind, PermissionResponse, WebviewBuilder};
use tauri::window::WindowBuilder;
use tauri::{AppHandle, Emitter, LogicalPosition, LogicalSize, Manager, Url, WebviewUrl};

const KIMI_PORT: u16 = 58627;
const READY_TIMEOUT: Duration = Duration::from_secs(30);
/// 远端托管版 kimi web：登录态由 WebView2 用户数据目录持久化
const REMOTE_URL: &str = "https://code-rc.kimi.com/";
/// 顶部选项卡栏（tabbar WebView）的逻辑高度
const TABBAR_HEIGHT: f64 = 36.0;
/// 活动页面主题采样周期：eval 一段只读脚本的开销可忽略，200ms 足够跟手
const THEME_POLL_INTERVAL: Duration = Duration::from_millis(200);
/// 统计浮窗（独立子 WebView）的固定逻辑宽度：内容变化不再改变宽度与位置
const STATS_WIDTH: f64 = 300.0;
/// 悬浮态横条高度
const STATS_BAR_HEIGHT: f64 = 30.0;
/// 折叠态圆点尺寸
const STATS_FOLDED_SIZE: f64 = 36.0;
/// 吸附态与窗口右边缘的间距（与选项卡栏的 8px padding 对齐）
const STATS_DOCK_MARGIN: f64 = 8.0;
/// 拖动循环的轮询间隔：约 120Hz，保证浮窗跟手
const STATS_DRAG_INTERVAL: Duration = Duration::from_millis(8);

/// 主题采样脚本（由 Rust 定时 eval 进当前活动的内容 WebView）：
/// 依次检查 body / html / body 首子元素的计算背景色，取第一个非透明色
/// 按 Rec.601 亮度判断深浅，同时把该颜色上报给统计浮窗做底色
/// （跨 WebView 无法毛玻璃，只能靠同色基底消除圆角处的色差）；
/// 全部透明时回退到系统主题。结果经 report_theme 上报，Rust 侧去重后转发
const THEME_SAMPLE_JS: &str = r#"(function () {
  try {
    var T = window.__TAURI__; if (!T) return;
    var els = [document.body, document.documentElement];
    if (document.body && document.body.firstElementChild) els.push(document.body.firstElementChild);
    var dark = null, bg = null;
    for (var i = 0; i < els.length; i++) {
      if (!els[i]) continue;
      var c = getComputedStyle(els[i]).backgroundColor;
      var m = c && c.match(/rgba?\(\s*(\d+)[,\s]+(\d+)[,\s]+(\d+)(?:[,\s/]+([\d.]+))?\s*\)/);
      if (!m) continue;
      var a = m[4] === undefined ? 1 : parseFloat(m[4]);
      if (a < 0.5) continue;
      dark = (0.299 * m[1] + 0.587 * m[2] + 0.114 * m[3]) < 128;
      bg = "rgb(" + m[1] + "," + m[2] + "," + m[3] + ")";
      break;
    }
    if (dark === null) dark = matchMedia("(prefers-color-scheme: dark)").matches;
    T.core.invoke("report_theme", { dark: dark, bg: bg });
  } catch (e) {}
})();"#;

/// 注入本地与远端内容 WebView 的共享脚本，补齐两块 WebView2 缺失行为：
/// 1. F5 / Ctrl+R 触发页面重载（WebView2 默认不响应浏览器式刷新快捷键），
///    用于长时间会话后释放渲染进程内存
/// 2. 暴露 kimi web 约定的桌面桥 window.kimiDesktop：showWindow 供通知
///    toast 点击唤回主窗口；其余字段是宿主契约所需的最小空实现
const REFRESH_BRIDGE_JS: &str = r#"window.addEventListener("keydown", (e) => {
    if (e.key === "F5" || (e.ctrlKey && !e.shiftKey && !e.altKey && (e.key === "r" || e.key === "R"))) {
        e.preventDefault();
        location.reload();
    }
});
(function () {
    if (!window.__TAURI__) return;
    window.kimiDesktop = {
        showWindow: () => { window.__TAURI__.core.invoke("show_window").catch(() => {}) },
        // 桌面宿主契约：kimi web 前端会探测若干宿主方法，其中更新相关方法
        // （onUpdateStatus / getUpdateStatus）不做存在性检查、直接调用，
        // 缺失会让设置面板在初始化时抛 TypeError（前端错误边界显示"出错了"）；
        // 本壳不提供自动更新，状态恒为 idle，前端据此隐藏更新入口
        log: (level, message, args) => { console[level === "error" ? "error" : "info"](message, ...(args || [])) },
        setTheme: () => {},
        onUpdateStatus: () => () => {},
        getUpdateStatus: () => Promise.resolve({ state: "idle" })
    };
})();"#;

/// 注入本地内容 WebView 的会话监听脚本：kimi web 是 SPA，会话切换只改
/// location（hash / pushState），这里每秒比对一次 URL 中的 session_<uuid>，
/// 变化时上报 Rust，供统计浮窗（独立子 WebView）查询对应会话的用量
const SESSION_REPORTER_JS: &str = r#"(function () {
  if (!window.__TAURI__) return;
  // 只在本地 kimi web 页面启用；壳内建 index/error 页没有会话概念
  if (location.hostname !== "127.0.0.1" && location.hostname !== "localhost") return;
  var last = null;
  function tick() {
    var m = location.href.match(/session_[0-9a-fA-F-]{36}/);
    var id = m ? m[0] : "";
    if (id !== last) {
      last = id;
      window.__TAURI__.core.invoke("report_session", { sessionId: id }).catch(function () {});
    }
  }
  tick();
  setInterval(tick, 1000);
})();"#;

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
    stats: Mutex<StatsState>,
    /// 今日用量（跨会话聚合）：按本地日期分桶，跨天时整体重置
    daily: Mutex<DailyStats>,
    /// 当前活动选项卡："local" | "remote"
    active_tab: Mutex<String>,
    /// 最近一次采样得到的页面主题与背景色，用于去重后通知 tabbar / 统计浮窗
    last_theme: Mutex<Option<(bool, Option<String>)>>,
    /// 统计浮窗的几何与状态（由 Rust 统一管理，页面只上报内容尺寸）
    stats_geom: Mutex<StatsGeom>,
    /// 拖动循环占用标记，避免重复启动
    stats_dragging: Mutex<bool>,
    /// 内容页当前打开的会话 id（由 SESSION_REPORTER_JS 上报）
    reported_session: Mutex<String>,
}

/// 统计浮窗几何：悬浮/吸附两种模式共用；坐标为相对主窗口客户区的逻辑像素
#[derive(Clone)]
struct StatsGeom {
    docked: bool,
    folded: bool,
    visible: bool,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

impl Default for StatsGeom {
    fn default() -> Self {
        Self {
            docked: false,
            folded: false,
            visible: false,
            x: 0.0,
            y: 0.0,
            w: STATS_WIDTH,
            h: STATS_BAR_HEIGHT,
        }
    }
}

/// 会话用量统计的增量解析状态：按会话维度聚合该会话所有 agent 的
/// wire.jsonl（含子 agent）；切换会话时整体重置
#[derive(Default)]
struct StatsState {
    session_id: String,
    cursors: HashMap<PathBuf, WireCursor>,
    agg: SessionAgg,
}

/// 单个 wire.jsonl 的增量读取游标
#[derive(Default)]
struct WireCursor {
    offset: u64,
    /// 最近一条 llm.request 的时间戳，用于与紧随的 usage.record 配对估算 TPS
    last_req_time: Option<i64>,
}

#[derive(Default)]
struct SessionAgg {
    input_other: u64,
    output: u64,
    cache_read: u64,
    cache_creation: u64,
    /// 最近若干次请求的 TPS，用于计算平滑均值
    recent_tps: VecDeque<f64>,
    last_record_time: Option<i64>,
}

/// 今日用量聚合状态：cursors 记录各 wire.jsonl 的增量读取位置，
/// date 为本地日期（yyyy-mm-dd），跨天时整体重置并重扫
#[derive(Default)]
struct DailyStats {
    date: String,
    cursors: HashMap<PathBuf, u64>,
    agg: DailyAgg,
}

#[derive(Default)]
struct DailyAgg {
    input_other: u64,
    output: u64,
    cache_read: u64,
    cache_creation: u64,
}

/// 允许在窗口内导航的来源：Tauri 应用自身页面与本机 kimi web 服务
fn is_local(url: &Url) -> bool {
    match url.host_str() {
        Some("127.0.0.1") | Some("localhost") | Some("tauri.localhost") => true,
        None => url.scheme() == "tauri",
        _ => false,
    }
}

/// 远端内容 WebView 允许在窗口内导航的域名：
/// code-rc.kimi.com 本体与 kimi.com 登录链路子域；其余外链一律系统浏览器打开
fn is_remote_allowed(url: &Url) -> bool {
    match url.host_str() {
        Some(h) => url.scheme() == "https" && (h == "kimi.com" || h.ends_with(".kimi.com")),
        None => false,
    }
}

fn open_external(url: &Url) {
    let _ = tauri_plugin_opener::open_url(url.as_str(), None::<&str>);
}

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
fn navigate_local(app: &AppHandle, url: &str) {
    if let Some(w) = app.get_webview("local") {
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
    if let Some(w) = app.get_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
    }
}

/// 切换活动选项卡：只改内容 WebView 可见性，两个会话同时存活互不打扰
fn switch_to_tab(app: &AppHandle, tab: &str) {
    *app.state::<AppState>().active_tab.lock().unwrap() = tab.to_string();
    for (label, active) in [("local", tab == "local"), ("remote", tab == "remote")] {
        if let Some(wv) = app.get_webview(label) {
            if active {
                let _ = wv.show();
            } else {
                let _ = wv.hide();
            }
        }
    }
    // 同步选项卡栏高亮（覆盖 show_error 等 Rust 侧强制切卡的场景）
    let _ = app.emit_to("tabbar", "tab-changed", tab);
}

fn show_error(app: &AppHandle, msg: &str) {
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

/// 主窗口客户区的逻辑尺寸
fn window_logical_size(window: &tauri::Window) -> (f64, f64) {
    let scale = window.scale_factor().unwrap_or(1.0);
    window
        .inner_size()
        .map(|s| (s.width as f64 / scale, s.height as f64 / scale))
        .unwrap_or((0.0, 0.0))
}

/// 把统计浮窗几何钳制在窗口内：吸附态右对齐贴顶（展开的明细向下延伸），
/// 悬浮态保持完整可见且不压住选项卡栏
fn clamp_stats_geom(g: &mut StatsGeom, win_w: f64, win_h: f64) {
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
fn apply_stats_geom(app: &AppHandle, g: &StatsGeom) {
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

/// 把键盘焦点还给当前选项卡的内容 WebView：
/// 点击/拖动统计浮窗会夺走焦点，不回焦会导致聊天输入框无法直接输入
fn focus_active_content(app: &AppHandle) {
    let tab = app.state::<AppState>().active_tab.lock().unwrap().clone();
    let label = if tab == "remote" { "remote" } else { "local" };
    if let Some(wv) = app.get_webview(label) {
        let _ = wv.set_focus();
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

/// 按窗口逻辑尺寸重排子 WebView：tabbar 置顶 TABBAR_HEIGHT，
/// 两个内容 WebView 铺满下方（bounds 是物理像素，需除以缩放因子换算），
/// 统计浮窗按当前模式重新钳制位置
fn layout_webviews(window: &tauri::Window) {
    let Ok(size) = window.inner_size() else {
        return;
    };
    // 最小化会带出 0x0 的 Resized 事件；此时不能按 0 尺寸钳制浮窗位置
    if window.is_minimized().unwrap_or(false) || size.width == 0 || size.height == 0 {
        return;
    }
    let scale = window.scale_factor().unwrap_or(1.0);
    let w = size.width as f64 / scale;
    let h = size.height as f64 / scale;
    let content_h = (h - TABBAR_HEIGHT).max(0.0);
    if let Some(tabbar) = window.get_webview("tabbar") {
        let _ = tabbar.set_position(LogicalPosition::new(0.0, 0.0));
        let _ = tabbar.set_size(LogicalSize::new(w, TABBAR_HEIGHT));
    }
    for label in ["local", "remote"] {
        if let Some(wv) = window.get_webview(label) {
            let _ = wv.set_position(LogicalPosition::new(0.0, TABBAR_HEIGHT));
            let _ = wv.set_size(LogicalSize::new(w, content_h));
        }
    }

    let app = window.app_handle();
    let state = app.state::<AppState>();
    let g = {
        let mut g = state.stats_geom.lock().unwrap();
        clamp_stats_geom(&mut g, w, h);
        g.clone()
    };
    apply_stats_geom(app, &g);
    let _ = app.emit_to(
        "stats",
        "stats-geom",
        serde_json::json!({ "width": g.w, "height": g.h }),
    );
    // 高度可能被窗口尺寸钳制，通知页面按最新可用高度调整明细滚动区
    let _ = app.emit_to(
        "stats",
        "stats-applied",
        serde_json::json!({ "width": g.w, "height": g.h }),
    );
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
fn last_error(state: tauri::State<AppState>) -> String {
    state.last_error.lock().unwrap().clone()
}

#[tauri::command]
fn restart_server(app: AppHandle) {
    if let Some(url) = app_page(&app, "index.html") {
        navigate_local(&app, &url);
    }
    start_server(&app);
}

// 供注入脚本暴露的 window.kimiDesktop.showWindow 调用：显示并聚焦主窗口
#[tauri::command]
fn show_window(app: AppHandle) {
    show_main_window(&app);
}

/// 选项卡栏页调用：切换本地 / 远端内容 WebView 的可见性
#[tauri::command]
fn switch_tab(tab: String, app: AppHandle) -> Result<(), String> {
    match tab.as_str() {
        "local" | "remote" => {
            switch_to_tab(&app, &tab);
            Ok(())
        }
        _ => Err(format!("未知选项卡: {tab}")),
    }
}

/// 统计浮窗加载完成：应用页面保存的模式与位置（此时仍保持隐藏，
/// 待首次拿到用量数据后由 stats_set_visible 显示，避免空白横条闪现）
#[tauri::command]
fn stats_restore(docked: bool, folded: bool, x: Option<f64>, y: Option<f64>, app: AppHandle) {
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
fn stats_set_size(width: f64, height: f64, app: AppHandle) {
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
fn stats_set_visible(visible: bool, app: AppHandle) {
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
fn stats_focus_content(app: AppHandle) {
    focus_active_content(&app);
}

/// 内容页会话切换上报（SESSION_REPORTER_JS）
#[tauri::command]
fn report_session(session_id: String, state: tauri::State<AppState>) {
    let mut cur = state.reported_session.lock().unwrap();
    if *cur != session_id {
        *cur = session_id;
    }
}

/// 统计浮窗拖动：由 Rust 轮询光标移动子 WebView，避免依赖跨 WebView 的
/// pointer capture；光标进入顶部选项卡栏带内实时吸附，移出即恢复悬浮
#[tauri::command]
fn stats_drag_start(app: AppHandle) {
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

/// 主题采样脚本（THEME_SAMPLE_JS）的上报入口：
/// 深浅色变化时通知 tabbar；深浅色或背景色变化时通知统计浮窗
#[tauri::command]
fn report_theme(dark: bool, bg: Option<String>, app: AppHandle) {
    let state = app.state::<AppState>();
    let mut last = state.last_theme.lock().unwrap();
    let dark_changed = last.as_ref().map(|(d, _)| *d) != Some(dark);
    let next = (dark, bg.clone());
    if last.as_ref() == Some(&next) {
        return;
    }
    *last = Some(next);
    drop(last);
    if dark_changed {
        let _ = app.emit_to("tabbar", "theme-changed", if dark { "dark" } else { "light" });
    }
    let _ = app.emit_to(
        "stats",
        "page-theme",
        serde_json::json!({ "dark": dark, "bg": bg }),
    );
}

/// Kimi Code 数据根目录：优先 KIMI_CODE_HOME，默认 ~/.kimi-code
fn kimi_code_home() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("KIMI_CODE_HOME") {
        if !p.is_empty() {
            return Some(PathBuf::from(p));
        }
    }
    for var in ["USERPROFILE", "HOME"] {
        if let Ok(h) = std::env::var(var) {
            if !h.is_empty() {
                return Some(PathBuf::from(h).join(".kimi-code"));
            }
        }
    }
    None
}

/// 按 sessionId 定位会话目录：sessions/<workDirKey>/<sessionId>
fn find_session_dir(home: &Path, session_id: &str) -> Option<PathBuf> {
    for wd in std::fs::read_dir(home.join("sessions")).ok()?.flatten() {
        let cand = wd.path().join(session_id);
        if cand.is_dir() {
            return Some(cand);
        }
    }
    None
}

/// 未提供 sessionId 时的回退：取 main wire.jsonl mtime 最新的会话，
/// 即当前最活跃会话
fn find_latest_session_dir(home: &Path) -> Option<PathBuf> {
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for wd in std::fs::read_dir(home.join("sessions")).ok()?.flatten() {
        for sess in std::fs::read_dir(wd.path()).ok()?.flatten() {
            let wire = sess.path().join("agents").join("main").join("wire.jsonl");
            let Ok(mtime) = wire.metadata().and_then(|m| m.modified()) else {
                continue;
            };
            if best.as_ref().is_none_or(|(t, _)| mtime > *t) {
                best = Some((mtime, sess.path()));
            }
        }
    }
    best.map(|(_, p)| p)
}

/// 从游标位置增量读取 wire.jsonl，只处理完整行（末尾半行留下轮），
/// 聚合 usage.record 并把每条记录与前方的 llm.request 配对估算 TPS
fn consume_wire(cur: &mut WireCursor, agg: &mut SessionAgg, path: &Path) {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return,
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    if len < cur.offset {
        // 文件被截断重建，从头解析
        cur.offset = 0;
        cur.last_req_time = None;
    }
    if f.seek(SeekFrom::Start(cur.offset)).is_err() {
        return;
    }
    let mut buf = Vec::new();
    if f.read_to_end(&mut buf).is_err() {
        return;
    }
    let complete = buf.iter().rposition(|&b| b == b'\n').map(|i| i + 1).unwrap_or(0);
    cur.offset += complete as u64;
    for line in String::from_utf8_lossy(&buf[..complete]).lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        match v.get("type").and_then(|t| t.as_str()) {
            Some("llm.request") => {
                cur.last_req_time = v.get("time").and_then(|t| t.as_i64());
            }
            Some("usage.record") => {
                let Some(u) = v.get("usage") else { continue };
                let get = |k: &str| u.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
                let output = get("output");
                agg.input_other += get("inputOther");
                agg.output += output;
                agg.cache_read += get("inputCacheRead");
                agg.cache_creation += get("inputCacheCreation");
                let Some(t) = v.get("time").and_then(|t| t.as_i64()) else {
                    continue;
                };
                agg.last_record_time = Some(agg.last_record_time.map_or(t, |x| x.max(t)));
                // 配对前方的 llm.request：间隔即本次请求耗时（含排队/重试，
                // 与 kimi web 内置组件口径一致的近似值）；异常间隔直接丢弃
                if let Some(req_t) = cur.last_req_time.take() {
                    let dur_ms = t - req_t;
                    if (1..=300_000).contains(&dur_ms) {
                        agg.recent_tps
                            .push_back(output as f64 / (dur_ms as f64 / 1000.0));
                        if agg.recent_tps.len() > 10 {
                            agg.recent_tps.pop_front();
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

/// 返回指定会话的累计用量与实时 TPS，供统计浮窗轮询；
/// 未指定时优先使用内容页上报的当前会话，再回退到最活跃会话
#[tauri::command]
fn session_stats(session_id: Option<String>, state: tauri::State<AppState>) -> serde_json::Value {
    let not_found = serde_json::json!({ "found": false });
    // 服务未就绪（启动加载页/错误页）时不展示历史会话的快照
    if !*state.ready.lock().unwrap() {
        return not_found;
    }
    let Some(home) = kimi_code_home() else { return not_found };
    let reported = state.reported_session.lock().unwrap().clone();
    let dir = session_id
        .as_deref()
        .filter(|s| !s.is_empty())
        .or_else(|| Some(reported.as_str()).filter(|s| !s.is_empty()))
        .and_then(|s| find_session_dir(&home, s))
        .or_else(|| find_latest_session_dir(&home));
    let Some(dir) = dir else { return not_found };
    let real_id = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();

    let mut st = state.stats.lock().unwrap();
    if st.session_id != real_id {
        *st = StatsState {
            session_id: real_id.clone(),
            ..Default::default()
        };
    }
    if let Ok(entries) = std::fs::read_dir(dir.join("agents")) {
        // 拆分字段借用，避免 cursors 与 agg 同时可变借用 st
        let StatsState { cursors, agg, .. } = &mut *st;
        for e in entries.flatten() {
            let wire = e.path().join("wire.jsonl");
            if wire.is_file() {
                let cur = cursors.entry(wire.clone()).or_default();
                consume_wire(cur, agg, &wire);
            }
        }
    }

    let agg = &st.agg;
    let input_total = agg.input_other + agg.cache_read + agg.cache_creation;
    let hit_rate = if input_total > 0 {
        agg.cache_read as f64 * 100.0 / input_total as f64
    } else {
        0.0
    };
    let tps_avg = if agg.recent_tps.is_empty() {
        0.0
    } else {
        agg.recent_tps.iter().sum::<f64>() / agg.recent_tps.len() as f64
    };
    serde_json::json!({
        "found": true,
        "session_id": real_id,
        "has_tps": !agg.recent_tps.is_empty(),
        "tps_avg": tps_avg,
        "tps_latest": agg.recent_tps.back().copied().unwrap_or(0.0),
        "input_total": input_total,
        "output_total": agg.output,
        "cache_read": agg.cache_read,
        "cache_creation": agg.cache_creation,
        "cache_hit_rate": hit_rate,
        "last_record_time": agg.last_record_time,
    })
}

/// 本地今日日期字符串（yyyy-mm-dd）与今日 00:00 的 epoch 毫秒；
/// 获取本地时区失败时回退 UTC
fn local_today() -> (String, i64) {
    use time::{OffsetDateTime, UtcOffset};
    let offset = UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC);
    let today = OffsetDateTime::now_utc().to_offset(offset).date();
    let midnight = today.midnight().assume_offset(offset).unix_timestamp() * 1000;
    (today.to_string(), midnight)
}

/// 从游标位置增量读取 wire.jsonl，只聚合 time >= day_start 的 usage.record
/// （按事件时间戳直接分桶到今日，避免跨天会话重复计算）；
/// 末尾半行留下轮，文件被截断重建时从头解析
fn consume_wire_today(offset: &mut u64, agg: &mut DailyAgg, path: &Path, day_start: i64) {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return,
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    if len < *offset {
        *offset = 0;
    }
    if f.seek(SeekFrom::Start(*offset)).is_err() {
        return;
    }
    let mut buf = Vec::new();
    if f.read_to_end(&mut buf).is_err() {
        return;
    }
    let complete = buf.iter().rposition(|&b| b == b'\n').map(|i| i + 1).unwrap_or(0);
    *offset += complete as u64;
    for line in String::from_utf8_lossy(&buf[..complete]).lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if v.get("type").and_then(|t| t.as_str()) != Some("usage.record") {
            continue;
        }
        let Some(t) = v.get("time").and_then(|t| t.as_i64()) else {
            continue;
        };
        if t < day_start {
            continue;
        }
        let Some(u) = v.get("usage") else { continue };
        let get = |k: &str| u.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
        agg.input_other += get("inputOther");
        agg.output += get("output");
        agg.cache_read += get("inputCacheRead");
        agg.cache_creation += get("inputCacheCreation");
    }
}

/// 返回本地今日全部会话（含子 agent）的累计用量，供统计悬浮窗轮询。
/// 首次调用对今日活跃文件全量回填，之后按 offset 增量解析；跨天整体重置。
/// async：同步命令跑在主线程，首次回填的目录遍历可能阻塞 UI
/// （含引用参数的 async 命令按 Tauri 约定必须返回 Result；本命令不失败，恒为 Ok）
#[tauri::command]
async fn daily_stats(state: tauri::State<'_, AppState>) -> Result<serde_json::Value, ()> {
    let not_found = serde_json::json!({ "found": false });
    if !*state.ready.lock().unwrap() {
        return Ok(not_found);
    }
    let Some(home) = kimi_code_home() else {
        return Ok(not_found);
    };
    let (today, day_start) = local_today();

    let mut st = state.daily.lock().unwrap();
    if st.date != today {
        *st = DailyStats {
            date: today,
            ..Default::default()
        };
    }

    let mut active_today = false;
    if let Ok(workdirs) = std::fs::read_dir(home.join("sessions")) {
        // 拆分字段借用，避免 cursors 与 agg 同时可变借用 st
        let DailyStats { cursors, agg, .. } = &mut *st;
        for wd in workdirs.flatten() {
            let Ok(sessions) = std::fs::read_dir(wd.path()) else {
                continue;
            };
            for sess in sessions.flatten() {
                let Ok(agents) = std::fs::read_dir(sess.path().join("agents")) else {
                    continue;
                };
                for agent in agents.flatten() {
                    let wire = agent.path().join("wire.jsonl");
                    if !wire.is_file() {
                        continue;
                    }
                    // append-only：mtime 早于今日 0 点的文件不可能含今日事件
                    let mtime_ms = wire
                        .metadata()
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_millis() as i64)
                        .unwrap_or(0);
                    if mtime_ms < day_start {
                        continue;
                    }
                    active_today = true;
                    let offset = cursors.entry(wire.clone()).or_insert(0);
                    consume_wire_today(offset, agg, &wire, day_start);
                }
            }
        }
    }

    let agg = &st.agg;
    let input_total = agg.input_other + agg.cache_read + agg.cache_creation;
    let hit_rate = if input_total > 0 {
        agg.cache_read as f64 * 100.0 / input_total as f64
    } else {
        0.0
    };
    Ok(serde_json::json!({
        "found": active_today,
        "input_total": input_total,
        "output_total": agg.output,
        "cache_read": agg.cache_read,
        "cache_hit_rate": hit_rate,
    }))
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![
            last_error,
            restart_server,
            show_window,
            session_stats,
            daily_stats,
            switch_tab,
            report_theme,
            report_session,
            stats_restore,
            stats_set_size,
            stats_set_visible,
            stats_focus_content,
            stats_drag_start
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

            start_server(app.handle());
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
