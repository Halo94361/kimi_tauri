//! 注入各 WebView 的 JS 脚本常量

/// 内容页主题采样：初始化、切卡与 Rust 轮询时同步读取计算样式并立即上报；
/// 不依赖 requestAnimationFrame，后台或无重绘的 WebView 也能按轮询刷新
pub(crate) const THEME_SAMPLE_JS: &str = r#"(function () {
  try {
    var T = window.__TAURI__; if (!T) return;
    function sample() {
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
      T.core.invoke("report_theme", { dark: dark, bg: bg }).catch(function () {});
    }
    window.__kimiThemeSample = sample;
    sample();
  } catch (e) {}
})();"#;

/// 自绘页在 CSS 解析前写入有效主题；local 中的 kimi web 不使用此脚本
pub(crate) fn shell_theme_init_js(dark: bool) -> String {
    r#"(function () {
  if (location.hostname !== "tauri.localhost" && location.protocol !== "tauri:") return;
  var theme = "__THEME__";
  function apply() {
    if (!document.documentElement) return false;
    document.documentElement.dataset.theme = theme;
    return true;
  }
  if (apply()) return;
  var observer = new MutationObserver(function () { if (apply()) observer.disconnect(); });
  observer.observe(document, { childList: true, subtree: true });
})();"#
        .replace("__THEME__", if dark { "dark" } else { "light" })
}

/// 注入本地与远端内容 WebView 的共享脚本，补齐两块 WebView2 缺失行为：
/// 1. F5 / Ctrl+R 触发页面重载（WebView2 默认不响应浏览器式刷新快捷键），
///    用于长时间会话后释放渲染进程内存
/// 2. 暴露 kimi web 约定的桌面桥 window.kimiDesktop：showWindow 供通知
///    toast 点击唤回主窗口；其余字段是宿主契约所需的最小空实现
pub(crate) const REFRESH_BRIDGE_JS: &str = r#"window.addEventListener("keydown", (e) => {
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
        setTheme: () => { if (window.__kimiThemeSample) window.__kimiThemeSample() },
        onUpdateStatus: () => () => {},
        getUpdateStatus: () => Promise.resolve({ state: "idle" })
    };
})();"#;

/// 注入本地内容 WebView 的会话监听脚本：kimi web 是 SPA，会话切换只改
/// location（hash / pushState），这里每秒比对一次 URL 中的 session_<uuid>，
/// 变化时上报 Rust，供统计浮窗（独立子 WebView）查询对应会话的用量
pub(crate) const SESSION_REPORTER_JS: &str = r#"(function () {
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
