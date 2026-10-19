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

/// 主题采样脚本（由 Rust 定时 eval 进当前活动的内容 WebView）：
/// 依次检查 body / html / body 首子元素的计算背景色，取第一个非透明色
/// 按 Rec.601 亮度判断深浅；全部透明时回退到系统主题。
/// 结果经 report_theme 命令上报，Rust 侧去重后转发给 tabbar
const THEME_SAMPLE_JS: &str = r#"(function () {
  try {
    var T = window.__TAURI__; if (!T) return;
    var els = [document.body, document.documentElement];
    if (document.body && document.body.firstElementChild) els.push(document.body.firstElementChild);
    var dark = null;
    for (var i = 0; i < els.length; i++) {
      if (!els[i]) continue;
      var c = getComputedStyle(els[i]).backgroundColor;
      var m = c && c.match(/rgba?\(\s*(\d+)[,\s]+(\d+)[,\s]+(\d+)(?:[,\s/]+([\d.]+))?\s*\)/);
      if (!m) continue;
      var a = m[4] === undefined ? 1 : parseFloat(m[4]);
      if (a < 0.5) continue;
      dark = (0.299 * m[1] + 0.587 * m[2] + 0.114 * m[3]) < 128;
      break;
    }
    if (dark === null) dark = matchMedia("(prefers-color-scheme: dark)").matches;
    T.core.invoke("report_theme", { dark: dark });
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

/// 注入 kimi web 页面的用量统计悬浮窗：
/// 每 2s 轮询 session_stats（本会话）与 daily_stats（今日全部会话）命令，
/// 渲染可拖动、跟随页面深浅主题的半透明毛玻璃浮窗（位置与折叠状态存
/// localStorage），明细面板分「本会话」「今日（全部会话）」两区；
/// 仅在 127.0.0.1/localhost 的 kimi web 页面启用，壳内建 index/error 页不显示
const STATS_OVERLAY_JS: &str = r##"
(function () {
  if (!window.__TAURI__) return;
  var invoke = window.__TAURI__.core.invoke;
  var POS_KEY = "kimiStatsPos", FOLD_KEY = "kimiStatsFold", PIN_KEY = "kimiStatsPin";
  var root = null, bar = null, dot = null, detail = null, detailBody = null, pinBtn = null;
  var folded = localStorage.getItem(FOLD_KEY) === "1";
  var pinned = localStorage.getItem(PIN_KEY) === "1";
  var lastData = null, lastDaily = null;

  function hasData() {
    return (lastData && lastData.found) || (lastDaily && lastDaily.found);
  }

  function onKimiWeb() {
    return location.hostname === "127.0.0.1" || location.hostname === "localhost";
  }
  function isDark() {
    var de = document.documentElement;
    var s = ((de.getAttribute("data-color-scheme") || "") + " " +
      (de.getAttribute("data-theme") || "") + " " + de.className).toLowerCase();
    if (s.indexOf("dark") >= 0) return true;
    if (s.indexOf("light") >= 0) return false;
    return window.matchMedia("(prefers-color-scheme: dark)").matches;
  }
  function applyTheme() {
    if (!root) return;
    var t = isDark() ? {
      bg: "rgba(28,28,30,0.72)", fg: "rgba(255,255,255,0.92)",
      border: "rgba(255,255,255,0.18)", shadow: "0 2px 12px rgba(0,0,0,0.4)"
    } : {
      bg: "rgba(255,255,255,0.72)", fg: "rgba(0,0,0,0.85)",
      border: "rgba(0,0,0,0.12)", shadow: "0 2px 12px rgba(0,0,0,0.12)"
    };
    [bar, detail, dot].forEach(function (el) {
      // 用 backgroundColor/borderColor 而非 shorthand，避免重置 detail 的
      // background-clip 与 6px 透明上边框（鼠标命中桥）
      el.style.backgroundColor = t.bg; el.style.color = t.fg;
      el.style.borderColor = t.border; el.style.boxShadow = t.shadow;
    });
    detail.style.borderTopColor = "transparent";
  }
  function applyFold() {
    bar.style.display = folded ? "none" : "flex";
    dot.style.display = folded ? "flex" : "none";
    // 锁定状态下详情面板常驻；折叠成小圆点时强制收起
    detail.style.display = (!folded && pinned && hasData()) ? "block" : "none";
  }
  function applyPin() {
    pinBtn.style.opacity = pinned ? "1" : "0.35";
    applyFold();
  }
  function enableDrag() {
    var sx = 0, sy = 0, ox = 0, oy = 0, dragging = false, moved = false;
    function down(e) {
      dragging = true; moved = false;
      sx = e.clientX; sy = e.clientY;
      var r = root.getBoundingClientRect();
      ox = r.left; oy = r.top;
      document.addEventListener("pointermove", move);
      document.addEventListener("pointerup", up);
    }
    function move(e) {
      if (!dragging) return;
      var dx = e.clientX - sx, dy = e.clientY - sy;
      if (Math.abs(dx) + Math.abs(dy) > 3) moved = true;
      if (!moved) return;
      var left = Math.min(Math.max(0, ox + dx), window.innerWidth - root.offsetWidth);
      var top = Math.min(Math.max(0, oy + dy), window.innerHeight - root.offsetHeight);
      root.style.left = left + "px"; root.style.top = top + "px";
      root.style.right = "auto";
      localStorage.setItem(POS_KEY, JSON.stringify({ left: left, top: top }));
    }
    function up() {
      dragging = false;
      document.removeEventListener("pointermove", move);
      document.removeEventListener("pointerup", up);
    }
    bar.addEventListener("pointerdown", down);
    dot.addEventListener("pointerdown", down);
  }
  function ensureRoot() {
    if (root || !document.body) return;
    root = document.createElement("div");
    root.id = "kimi-stats-overlay";
    var s = root.style;
    s.position = "fixed"; s.zIndex = "2147483000";
    s.font = "12px/1.5 system-ui, sans-serif";
    s.userSelect = "none"; s.webkitUserSelect = "none";
    s.display = "none";
    var pos = null;
    try { pos = JSON.parse(localStorage.getItem(POS_KEY) || "null"); } catch (e) {}
    if (pos && typeof pos.left === "number" && typeof pos.top === "number") {
      s.left = pos.left + "px"; s.top = pos.top + "px";
    } else {
      s.right = "12px"; s.top = "12px";
    }
    bar = document.createElement("div");
    bar.style.cssText = "display:flex;align-items:center;justify-content:center;min-width:230px;padding:5px 12px;" +
      "border:1px solid transparent;border-radius:10px;" +
      "backdrop-filter:blur(12px);-webkit-backdrop-filter:blur(12px);cursor:move;white-space:nowrap";
    dot = document.createElement("div");
    dot.style.cssText = "display:none;align-items:center;justify-content:center;width:36px;height:36px;" +
      "border:1px solid transparent;border-radius:50%;backdrop-filter:blur(12px);-webkit-backdrop-filter:blur(12px);cursor:move;font-size:16px";
    dot.textContent = "⚡";
    detail = document.createElement("div");
    // top:100% + 透明上边框：保留 6px 视觉间距，同时让命中区域与横条连成一片，
    // 鼠标才能从横条移到面板上（否则会触发 mouseleave 自动收起）；
    // background-clip:padding-box 让毛玻璃背景不渗进透明边框区
    detail.style.cssText = "display:none;position:absolute;top:100%;left:0;right:0;box-sizing:border-box;" +
      "border:1px solid transparent;border-top:6px solid transparent;background-clip:padding-box;" +
      "padding:8px 12px;border-radius:10px;backdrop-filter:blur(12px);-webkit-backdrop-filter:blur(12px);cursor:default";
    // 头部固定（含锁定按钮），内容区由 render 重写，避免每轮重建按钮
    var head = document.createElement("div");
    head.style.cssText = "display:flex;justify-content:space-between;align-items:center;margin-bottom:2px";
    var title = document.createElement("span");
    title.style.opacity = "0.5";
    title.textContent = "本会话";
    pinBtn = document.createElement("span");
    pinBtn.textContent = "📌";
    pinBtn.title = "锁定 / 自动收起";
    pinBtn.style.cssText = "cursor:pointer;opacity:0.35";
    pinBtn.addEventListener("click", function () {
      pinned = !pinned;
      localStorage.setItem(PIN_KEY, pinned ? "1" : "0");
      applyPin();
    });
    head.appendChild(title); head.appendChild(pinBtn);
    detailBody = document.createElement("div");
    detail.appendChild(head); detail.appendChild(detailBody);
    root.appendChild(bar); root.appendChild(dot); root.appendChild(detail);
    document.body.appendChild(root);
    root.addEventListener("mouseenter", function () {
      if (!folded && hasData()) detail.style.display = "block";
    });
    root.addEventListener("mouseleave", function () { if (!pinned) detail.style.display = "none"; });
    root.addEventListener("dblclick", function (e) {
      // 详情面板内的双击（如锁定按钮）不触发折叠切换
      if (detail.contains(e.target)) return;
      folded = !folded;
      localStorage.setItem(FOLD_KEY, folded ? "1" : "0");
      applyFold();
    });
    enableDrag();
    applyTheme(); applyPin();
    var mq = window.matchMedia("(prefers-color-scheme: dark)");
    if (mq.addEventListener) mq.addEventListener("change", applyTheme); else mq.addListener(applyTheme);
    new MutationObserver(applyTheme).observe(document.documentElement,
      { attributes: true, attributeFilter: ["class", "data-color-scheme", "data-theme"] });
  }

  function fmt(n) {
    if (n >= 1000000) return (n / 1000000).toFixed(1) + "M";
    if (n >= 1000) return (n / 1000).toFixed(1) + "K";
    return String(n);
  }
  function row(k, v) {
    return '<div style="display:flex;justify-content:space-between;gap:16px">' +
      '<span style="opacity:0.65">' + k + "</span><span>" + v + "</span></div>";
  }
  function render() {
    var d = lastData, daily = lastDaily;
    var sFound = d && d.found, dFound = daily && daily.found;
    if (!sFound && !dFound) { root.style.display = "none"; return; }
    root.style.display = "block";
    if (sFound) {
      // 空闲后保留最后一次统计值，避免横条退化成 -- 丢失可读信息
      var tps = d.has_tps ? d.tps_avg.toFixed(1) : "--";
      bar.textContent = "⚡ " + tps + " tok/s · ↑" + fmt(d.input_total) +
        " ↓" + fmt(d.output_total) + " · 缓存 " + d.cache_hit_rate.toFixed(0) + "%";
    } else {
      // 会话页尚未产生数据时，横条退化为今日汇总
      bar.textContent = "⚡ 今日 ↑" + fmt(daily.input_total) + " ↓" + fmt(daily.output_total) +
        " · 缓存 " + daily.cache_hit_rate.toFixed(0) + "%";
    }
    var html = "";
    if (sFound) {
      html +=
        row("TPS 均值（近10次）", d.has_tps ? d.tps_avg.toFixed(1) + " tok/s" : "--") +
        row("TPS 瞬时", d.has_tps ? d.tps_latest.toFixed(1) : "--") +
        row("输入合计", fmt(d.input_total)) +
        row("缓存读", fmt(d.cache_read)) +
        row("输出合计", fmt(d.output_total)) +
        row("缓存命中率", d.cache_hit_rate.toFixed(1) + "%") +
        row("会话", String(d.session_id || "").slice(0, 24));
    }
    html += '<div style="opacity:0.5;border-top:1px solid rgba(128,128,128,0.35);margin:6px 0 2px;padding-top:4px">今日（全部会话）</div>';
    if (dFound) {
      html +=
        row("输入合计", fmt(daily.input_total)) +
        row("缓存读", fmt(daily.cache_read)) +
        row("输出合计", fmt(daily.output_total)) +
        row("缓存命中率", daily.cache_hit_rate.toFixed(1) + "%");
    } else {
      html += '<div style="opacity:0.5">暂无数据</div>';
    }
    detailBody.innerHTML = html;
  }
  function currentSessionId() {
    var m = location.href.match(/session_[0-9a-fA-F-]{36}/);
    return m ? m[0] : null;
  }
  // 会话统计 2s 一轮（TPS 需要跟手）；今日汇总 10s 一轮（变化慢，无需高频）
  function tickSession() {
    if (!onKimiWeb()) { if (root) root.style.display = "none"; return; }
    ensureRoot();
    if (!root) return;
    invoke("session_stats", { sessionId: currentSessionId() })
      .then(function (d) { lastData = d; render(); })
      .catch(function () { lastData = null; render(); });
  }
  function tickDaily() {
    if (!onKimiWeb() || !root) return;
    invoke("daily_stats")
      .then(function (d) { lastDaily = d; render(); })
      .catch(function () { lastDaily = null; render(); });
  }
  setInterval(tickSession, 2000);
  setInterval(tickDaily, 10000);
  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", function () { tickSession(); tickDaily(); });
  } else {
    tickSession(); tickDaily();
  }
})();
"##;

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
    /// 最近一次采样得到的页面主题（true=深色），用于去重后只在变化时通知 tabbar
    last_theme: Mutex<Option<bool>>,
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
    if let Some(url) = app_page(app, "error.html") {
        navigate_local(app, &url);
    }
    // 错误发生在本地后端：强制切回"本地"选项卡让错误页可见
    switch_to_tab(app, "local");
    // 托盘模式下窗口可能处于隐藏状态，出错时主动唤出让用户感知
    show_main_window(app);
}

/// 按窗口逻辑尺寸重排子 WebView：tabbar 置顶 TABBAR_HEIGHT，
/// 两个内容 WebView 铺满下方（bounds 是物理像素，需除以缩放因子换算）
fn layout_webviews(window: &tauri::Window) {
    let Ok(size) = window.inner_size() else {
        return;
    };
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

/// 主题采样脚本（THEME_SAMPLE_JS）的上报入口：
/// 去重后只在深浅色变化时通知 tabbar 切换配色
#[tauri::command]
fn report_theme(dark: bool, app: AppHandle) {
    let state = app.state::<AppState>();
    let mut last = state.last_theme.lock().unwrap();
    if *last == Some(dark) {
        return;
    }
    *last = Some(dark);
    let _ = app.emit_to("tabbar", "theme-changed", if dark { "dark" } else { "light" });
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

/// 返回指定（或当前最活跃）会话的累计用量与实时 TPS，
/// 供注入 kimi web 页面的统计悬浮窗轮询
#[tauri::command]
fn session_stats(session_id: Option<String>, state: tauri::State<AppState>) -> serde_json::Value {
    let not_found = serde_json::json!({ "found": false });
    let Some(home) = kimi_code_home() else { return not_found };
    let dir = session_id
        .as_deref()
        .filter(|s| !s.is_empty())
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
            report_theme
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

            // 裸窗口 + 3 个子 WebView：tabbar（顶部选项卡栏）、local（本地 kimi web）、
            // remote（远端托管服务）；切换选项卡只改可见性，两个会话同时存活
            let mut win_builder = WindowBuilder::new(app, "main")
                .title("Kimi Code")
                .inner_size(win_w, win_h)
                .min_inner_size(720.0, 480.0)
                .center()
                // 先隐藏，待三个子 WebView 就位（远端初始隐藏）后再显示，避免闪动
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
                    // 注入用量统计悬浮窗：轮询 session_stats，实时显示 TPS / 会话 token / 缓存命中率
                    .initialization_script(STATS_OVERLAY_JS)
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
