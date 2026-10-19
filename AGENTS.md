# AGENTS.md - tauri_kimi 开发指南

`tauri_kimi` 是 Tauri v2 (Rust) 的 Windows 桌面壳，把 `kimi web` 的本地 Web UI 包装成双击即用的原生窗口。核心逻辑集中在 `src-tauri/src/lib.rs`，前端只有 `ui/` 下三个原生静态页。

## 架构

- **零前端打包**：不打包 kimi web 前端，WebView2 直接加载 `kimi web` 在 `127.0.0.1:<port>` 提供的同源页面；`kimi upgrade` 后界面自动更新。
- **单窗口多 WebView**：裸窗口 + 3 个子 WebView（`tabbar` / `local` / `remote`）。本地与远端 `https://code-rc.kimi.com/` 同时存活，切换只改可见性。
- **子进程直管**：Rust 直接 spawn `kimi web --rc --no-open --port <port>`，Windows 下设置 `CREATE_NO_WINDOW`，避免控制台黑窗；`--rc` 同时把本机会话注册到远端，手机或另一台电脑可接管。
- **端口选择**：`pick_port` 首选 58627，被占用时改用系统空闲端口。注意 localStorage 按 origin 隔离，回退端口会让主题等页面设置重置（登录态与会话数据不受影响）。
- **就绪探测**：读 stdout 正则解析端口与 token，30s 看门狗兜底；随后导航到 `http://127.0.0.1:<port>/#token=<token>`，并持续清空 stdout 防止管道写满。
- **生命周期**：关窗仅隐藏（托盘驻留，子进程继续跑）；托盘“退出”才 kill 子进程并退出；子进程异常退出导航到 `error.html`，可调 `restart_server` 重启。
- **进程树回收**：spawn 后把 `kimi.exe` 挂入带 `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` 的 Job Object（`win_job`），壳进程无论正常退出、被强杀还是崩溃都会级联回收 MCP 孙进程；创建/绑定失败时退回直接 kill。
- **外链管控**：`is_local()` 白名单（127.0.0.1 / localhost / tauri.localhost）内才允许窗口内导航，其余经 `tauri-plugin-opener` 交给系统浏览器。
- **窗口尺寸**：按主显示器工作区 85% 钳制（宽 720~1600，高 480~1200），适配高 DPI。

## 关键契约

- **桌面桥 `window.kimiDesktop`**：kimi web 前端会无存在性检查地调用 `onUpdateStatus` / `getUpdateStatus` / `setTheme`，缺失会让设置面板抛 `TypeError`。注入脚本必须提供最小实现：`getUpdateStatus` 返回 `{state:"idle"}`、`onUpdateStatus` 返回退订函数、`setTheme` / `log` 空实现或转 console。kimi CLI 升级后设置面板报“出错了”，先查这里。
- **通知**：窗口构建时用 `on_permission_request` 放行 `PermissionKind::Notifications`（需 tauri ≥ 2.12）；点击 toast 经 `show_window` 命令唤回窗口。
- **ACL**：`build.rs` 的 `AppManifest.commands` 为自定义命令生成 `allow-*` 权限；本地页声明在 `capabilities/default.json`，remote 源在 `capabilities/kimi-remote.json`（按 `remote.urls` 放行 `allow-show-window` / `allow-session-stats` / `allow-daily-stats`）。新增命令要同步这两处。
- **敏感信息**：Bearer token 只存内存与 URL fragment，禁止写入日志或持久化。
- **用量统计**：数据源是 `~/.kimi-code/sessions/<workDirKey>/<sessionId>/agents/*/wire.jsonl`；会话 token 累加 `usage.record`，缓存命中率 = `ΣinputCacheRead / Σ(inputOther+inputCacheRead+inputCacheCreation)`，TPS 取 `usage.record` 与最近 `llm.request` 配对（间隔 ≤300s）的最近 10 次均值；今日汇总按事件 epoch 毫秒分桶到本地日期（`time` crate 的 `local-offset`，失败回退 UTC）。

## 构建与验证

在 `src-tauri` 下执行：

```bash
cargo check                                   # 语法与类型检查
cargo tauri dev                               # 开发模式
cargo build --release                         # 产物 target/release/tauri_kimi.exe
```

换机后先跑环境检查：

```bash
bash scripts/check_env.sh | diff scripts/check_env_golden.txt -
```

diff 中用户名/安装路径差异可忽略，重点核对版本一致、两个 `libgcc_eh.a` 都在、无失效路径告警。更新 golden：`bash scripts/check_env.sh > scripts/check_env_golden.txt`。

## 开发约定

- 业务逻辑集中在 `lib.rs`，最小修改，不引入多余抽象。
- 注释用中文，写设计意图与平台特殊处理；不写一次性排障记录。
- `ui/` 保持纯原生 HTML/CSS/JS，禁止引入前端框架；Tauri API 走 `window.__TAURI__.core.invoke`。
- 任何启动子进程的路径都必须保证 `WindowEvent::CloseRequested` 与异常分支能 `kill_child`，防止进程泄漏。
- 外部 URL 一律走系统浏览器，不得在内置 WebView 加载。
