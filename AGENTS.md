# AGENTS.md - tauri_kimi 开发指南

本文档面向在此代码库中工作的 AI 编程代理与开发者，详细记录了项目的架构设计、技术选型、目录结构、开发构建指令以及核心业务逻辑。

---

## 1. 项目概述 (Project Overview)

`tauri_kimi` 是一个基于 **Tauri v2 (Rust)** 开发的轻量级 Windows 桌面包装应用，旨在将 **Kimi Code CLI** 提供的本地 Web UI (`kimi web`) 封装为双击即用、无浏览器地址栏的原生桌面客户端。

### 核心特性与设计原则
- **零前端打包 (Zero-bundle Webview)**：不将 Kimi Web 前端打包进 exe 产物中，主界面直接通过 WebView2 加载由 `kimi web` 进程在本地 (`127.0.0.1:<port>`) 提供的同源页面。当用户通过 `kimi upgrade` 升级 CLI 后，桌面应用会自动呈现最新版 Web UI，无需重新编译桌面壳。
- **本地极简内置页**：壳工程自身仅嵌入两个本地静态页面（位于 `ui/`）：
  - `ui/index.html`：启动加载动画页（Loading Spinner），等待后端服务就绪。
  - `ui/error.html`：服务断开或启动失败时的错误提示页，支持读取错误详情并一键重启服务。
- **子进程原生管控**：Rust 侧直接 spawn 管理 `kimi.exe` 子进程，避免 PowerShell/CMD 中转带来的控制台黑窗与多层进程树问题。
- **原生文件拖放**：关闭 Tauri 默认的拖放捕获，使 Web 页面能够原生接收 HTML5 文件拖放（如拖拽文件/图片至对话框）。
- **页面刷新快捷键**：通过 `initialization_script` 注入按键监听，支持 F5 / Ctrl+R 重载页面（WebView2 默认不响应浏览器式刷新快捷键），用于长时间会话后手动释放渲染进程内存。
- **系统托盘驻留**：应用带系统托盘图标，关闭窗口仅隐藏（kimi web 子进程继续运行，会话不丢），托盘菜单"显示窗口"或左键单击托盘图标唤回窗口，只有托盘菜单"退出"才真正结束应用与子进程。
- **Web 通知唤起**：放行 WebView2 通知权限（`on_permission_request`），页面的 Web Notification API 以原生 toast 正常弹出；注入脚本暴露 kimi web 约定的 `window.kimiDesktop.showWindow` 桥，点击 toast 唤出托盘隐藏的主窗口。
- **实时用量悬浮窗**：向 kimi web 页面注入统计浮窗，轮询 Rust 侧 `session_stats` 命令，实时显示 TPS（近 5 次均值）、当前会话 token 消耗与缓存命中率；浮窗为跟随页面深浅主题的半透明毛玻璃样式，可拖动（localStorage 记忆位置）、双击折叠、悬停展开明细。

---

## 2. 架构与运行机制 (Architecture & Runtime Workflow)

### 2.1 整体架构流程

```
tauri_kimi.exe (Tauri 壳 / Rust 宿主)
 ├─ 1. 初始化窗口：按主显示器工作区比例自适应计算尺寸，加载本地 ui/index.html
 ├─ 2. 启动子进程：无黑窗执行 kimi web --no-open --port 58627
 ├─ 3. 动态探测与鉴权：
 │     ├─ 读取子进程 stdout，通过正则实时解析实际监听端口与 Bearer Token
 │     └─ 启动 30s 看门狗线程监控启动超时
 ├─ 4. 服务就绪：WebView2 自动导航至 http://127.0.0.1:<port>/#token=<token>
 ├─ 5. 导航与外链安全管控：
 │     ├─ 本地来源 (127.0.0.1, localhost, tauri.localhost) 允许在窗口内导航
 │     └─ 外部链接拦截并通过 tauri-plugin-opener 调用系统默认浏览器打开
 ├─ 6. Web 通知：on_permission_request 放行通知权限，toast 由 WebView2 原生 UI 弹出，点击经注入脚本回调唤起主窗口
 └─ 7. 异常与生命周期管理：
       ├─ 窗口关闭时：拦截关闭请求并隐藏窗口（托盘驻留，子进程继续运行）
       ├─ 托盘菜单"退出"时：置 quitting 标志、kill 子进程并 wait 清理，退出事件循环
       ├─ 子进程异常退出时：导航至 ui/error.html 并唤出窗口，提供错误信息与 restart_server 命令
       └─ 重启服务：由前端调用 Tauri Command 重新进入启动检测流程
```

### 2.2 核心模块与实现细节 (`src-tauri/src/lib.rs`)

1. **子进程探测与执行 (`spawn_kimi`)**：
   - 优先查找系统 `PATH` 中的 `kimi` 命令。
   - 备选查找固定安装路径：`%USERPROFILE%\.kimi-code\bin\kimi.exe`。
   - 在 Windows 下设置进程创建标志 `CREATE_NO_WINDOW = 0x08000000`，彻底防止控制台黑窗弹出。
2. **端口与 Token 正则解析**：
   - 匹配完整带 Token URL：`http://[\w.%-]+:(\d+)[^\s"']*#token=([^\s"']+)`
   - 独立匹配端口：`127.0.0.1:(\d+)`
   - 独立匹配 Token：`(?i)#token=([^\s"']+)|token\s*[:=]\s*([A-Za-z0-9._\-]+)`
   - 成功捕获端口与 Token 后即触发主窗口导航，并持续异步清空 stdout 防止管道缓冲区写满阻塞子进程。
3. **高分屏窗口适配**：
   - 获取主显示器工作区（Work Area）与缩放因子（Scale Factor）。
   - 将窗口尺寸钳制在工作区 85% 范围内（宽度 720~1600，高度 480~1200），避免在高 DPI 屏幕下窗口超出可见区域。
4. **IPC 命令 (Tauri Commands)**：
   - `last_error`：返回 Rust 全局状态中记录的最后一次错误信息字符串。
   - `restart_server`：重置状态、导航回 `index.html` 并重新拉起 `kimi web`。
   - `show_window`：显示并聚焦主窗口，由注入脚本暴露的 `window.kimiDesktop.showWindow`（kimi web 通知点击回调）调用。
5. **系统托盘（Tray Icon）**：
   - `TrayIconBuilder` 创建托盘图标（复用应用窗口图标），菜单含"显示窗口"/"退出"两项；左键单击托盘图标同样唤回窗口。
   - `CloseRequested` 拦截：非退出路径一律 `prevent_close()` + 隐藏窗口，kimi web 子进程保持运行；仅托盘"退出"走 `quit_app`（置 `quitting` 标志 → `kill_child` → `app.exit(0)`）。关闭拦截前检查 `quitting` 标志，保证退出流程中窗口能正常销毁。
   - `show_error` 在窗口隐藏时会顺带唤出窗口，确保托盘驻留期间服务异常可被用户感知。
6. **进程树级联回收（Windows Job Object）**：
   - `kimi.exe` 被杀（`TerminateProcess`）时其自身没有机会清理 MCP 孙进程，直接 kill 会导致孤儿进程残留。
   - 因此 spawn 成功后立即把 `kimi.exe` 挂入带 `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` 的 Job Object（`lib.rs` 中 `win_job` 模块），此后 `kimi.exe` 启动的所有孙进程自动归入同一作业；壳进程以任何方式退出（正常关闭、被强杀、崩溃）导致 job handle 关闭时，系统级联回收整棵进程树。
   - job handle 存入 `AppState` 随应用生命周期持有；job 创建/绑定失败时退回原有的直接 kill 清理，不影响启动。
7. **Web 通知权限放行（`on_permission_request`）**：
   - WebView2 对未应答的权限请求默认拒绝，页面的 Web Notification API 会被静默吞掉；窗口构建时放行 `PermissionKind::Notifications`，其余权限类型保持默认。
   - 通知 toast 由 WebView2 原生 UI 显示（未接管 `NotificationReceived`）；点击 toast 时 WebView2 向页面派发通知的 `click` 事件，kimi web 的 onclick 回调随即调用注入脚本暴露的 `window.kimiDesktop.showWindow` 桥，经 `show_window` 命令唤起（托盘隐藏的）主窗口。
   - 桌面桥契约：kimi web 前端会探测 `window.kimiDesktop` 上的宿主方法，部分调用（`onUpdateStatus` / `getUpdateStatus` / `setTheme`）不做存在性检查，缺失会让设置面板初始化时抛 `TypeError` 并显示错误边界"出错了"。因此桥需提供最小空实现：`getUpdateStatus` 返回 `{state:"idle"}`（前端据此隐藏更新入口）、`onUpdateStatus` 返回退订函数、`setTheme` / `log` 为空实现/转 console。kimi CLI 升级后可能出现新方法，报错时应先排查此处。
   - remote 源（`http://127.0.0.1:<port>`）调用自定义命令受 tauri ACL 管控：`build.rs` 通过 `AppManifest.commands` 为命令自动生成 `allow-*` 权限标识，`capabilities/kimi-remote.json` 按 `remote.urls` 放行 `allow-show-window` 与 `allow-session-stats`；本地源页面的命令权限声明在 `capabilities/default.json`。
   - 依赖 tauri ≥ 2.12（该版本引入 `on_permission_request`）。
8. **会话用量统计悬浮窗（`session_stats` + `STATS_OVERLAY_JS`）**：
   - 数据源：`~/.kimi-code/sessions/<workDirKey>/<sessionId>/agents/*/wire.jsonl`（含子 agent）中每行一个 JSON 事件；`usage.record` 携带 `inputOther / output / inputCacheRead / inputCacheCreation`，`llm.request` 提供请求发起时间。
   - Rust 侧 `session_stats` 命令：按 sessionId 定位会话目录（注入脚本从页面 URL 正则提取 `session_<uuid>`；提取不到时回退 main wire.jsonl mtime 最新的会话），对每个 wire 文件按 offset 增量解析（末尾半行留下轮），换会话时重置聚合状态。
   - 统计口径：会话 token = 全部 agent 的 `usage.record` 累加；缓存命中率 = `ΣinputCacheRead / Σ(inputOther+inputCacheRead+inputCacheCreation)`；TPS = 每条 `usage.record` 与前方最近 `llm.request` 配对（间隔 ≤300s 有效），取最近 5 次均值。
   - 注入脚本 `STATS_OVERLAY_JS`：仅在 127.0.0.1/localhost 页面启用，每 2s 轮询渲染可拖动的半透明毛玻璃浮窗（深浅色主题通过 documentElement 属性 + `prefers-color-scheme` 双通道跟随），位置/折叠/锁定状态存 localStorage；双击折叠为小圆点，悬停展开与横条同宽的明细面板（面板内 📌 按钮锁定常驻，未锁定时鼠标离开自动收起），空闲 30s TPS 显示 `--`。
---

## 3. 技术栈与环境依赖 (Technology Stack)

- **语言与框架**：
  - Rust (Edition 2021)
  - Tauri v2 (`tauri = "2"`（需 ≥2.12，提供 `on_permission_request`），含 `tray-icon` feature 用于系统托盘，`tauri-build = "2"`)
  - Tauri 插件：`tauri-plugin-opener = "2"`（用于系统默认浏览器打开外链）
  - 正则引擎：`regex = "1"`
  - JSON 解析：`serde_json = "1"`（增量解析 wire.jsonl 事件行，用于会话用量统计）
  - Windows API：`windows-sys = "0.59"`（`Win32_System_JobObjects`，仅 Windows 下编译，用于 Job Object 进程树级联回收）
- **工具链要求**：
  - Rust 编译器：`rustc` / `cargo` (支持 `x86_64-pc-windows-gnu` 或 `x86_64-pc-windows-msvc`)
  - Tauri CLI：`cargo-tauri` (`tauri-cli v2`)
- **系统运行时**：
  - Windows 10 (1809+) / Windows 11 预装的 Microsoft Edge WebView2 Runtime。
  - 本机已安装并配置好 Kimi Code CLI (`kimi`)。

---

## 4. 目录组织 (Directory Structure)

```
tauri_kimi/
├── .gitignore                 # Git 忽略配置（忽略 target、node_modules、日志及编辑器配置）
├── app-icon.png               # 应用原始高分辨率图标
├── 方案.md                    # 初始架构方案与验收清单
├── AGENTS.md                  # 本文档（面向 AI 代理与开发者的完整项目指南）
├── ui/                        # 本地嵌入静态网页
│   ├── index.html             # 加载中等待页（纯 CSS 动效）
│   └── error.html             # 异常/断线提示页（含 Tauri IPC 错误查询与重启逻辑）
└── src-tauri/                 # Tauri Rust 工程根目录
    ├── Cargo.toml             # Rust 依赖声明与 Release 构建优化配置
    ├── Cargo.lock             # Rust 依赖版本锁定
    ├── tauri.conf.json        # Tauri v2 应用配置文件
    ├── build.rs               # Tauri 构建脚本（声明 app ACL manifest，为自定义命令生成 allow-* 权限）
    ├── capabilities/          # Tauri v2 权限配置
    │   ├── default.json       # 本地源（壳内建页面）权限声明（core/opener 与本地自定义命令）
    │   └── kimi-remote.json   # remote 源（kimi web 页面）权限声明（allow-show-window）
    ├── icons/                 # 各平台打包图标资源（ico, icns, png, Android/iOS）
    └── src/                   # Rust 源码目录
        ├── main.rs            # 程序入口点（配置 release 下无控制台窗口）
        └── lib.rs             # 核心业务逻辑（进程管理、stdout 监控、窗口创建、IPC、事件处理）
```

---

## 5. 构建、测试与运行指令 (Build & Development Commands)

所有 Tauri / Cargo 相关指令均在 `src-tauri` 目录下执行，或在根目录下指定清单路径。

### 5.1 编译语法与类型检查
```bash
# 进入 src-tauri 目录
cd src-tauri

# 语法与编译检查
cargo check

# 检查测试代码编译
cargo check --tests
```

### 5.2 调试运行
```bash
# 在 src-tauri 目录下启动开发模式（需安装 cargo-tauri）
cd src-tauri && cargo tauri dev

# 或在项目根目录下通过 cargo 运行
cargo run --manifest-path src-tauri/Cargo.toml
```

### 5.3 生产构建
```bash
# 仅编译 Release 二进制文件（产物输出至 src-tauri/target/release/tauri_kimi.exe），不构建分发安装包
cd src-tauri && cargo build --release
```

### 5.4 环境同步检查（新机器/换机后执行）

`scripts/check_env.sh` 逐项输出工具链关键信息（rustc 版本与 host、gcc 发行版、两处 `libgcc_eh.a` 兜底、cargo-tauri、kimi CLI、WebView2 Runtime、`.cargo/config.toml` 失效绝对路径）。

```bash
# 新机器运行并与 golden 参考比对
bash scripts/check_env.sh | diff scripts/check_env_golden.txt -
```

diff 中用户名/安装路径差异可忽略；重点核对版本号一致、两个 `libgcc_eh.a` 都在、无"失效"路径告警。全部通过后执行 `cargo build --release` 冒烟。参考机器的 golden 更新方式：`bash scripts/check_env.sh > scripts/check_env_golden.txt`。

---

## 6. 代码规范与开发约定 (Code Conventions)

1. **最小修改与聚焦**：
   - 保持 Rust 代码结构紧凑清晰，业务逻辑集中在 `src-tauri/src/lib.rs`，不引入非必要的抽象层。
2. **注释规范**：
   - 使用**中文**编写代码注释。
   - 注释重点阐述设计意图、特殊平台处理逻辑（例如 Windows `CREATE_NO_WINDOW` 标志的作用、DPI 窗口钳制算法原理、禁用 Tauri 默认拖放的原因）。
   - 禁止在注释中记录一次性排障日志或临时试验数据。
3. **前端代码约定 (`ui/`)**：
   - 保持纯原生 HTML/CSS/JS 实现，禁止引入庞大的前端打包框架（如 webpack/vite/react/vue 等）。
   - 调用 Tauri API 时遵循 Tauri v2 标准用法（通过全局 `window.__TAURI__.core.invoke` 调用命令）。
4. **安全与进程生命周期规范**：
   - 任何涉及启动子进程的操作，必须确保在窗口关闭事件 (`WindowEvent::CloseRequested`) 和异常处理中正确调用 `kill_child`，防止进程泄漏。
   - URL 校验必须严格通过 `is_local()` 白名单过滤，坚决拦截外部 URL 在内置 WebView 内加载，必须通过系统默认浏览器打开。
   - 敏感信息（如 Bearer Token）仅保存在内存中与 URL Fragment（`#token=...`）传递，禁止持久化写入任何日志文件。
