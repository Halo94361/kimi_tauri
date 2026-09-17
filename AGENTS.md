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
 └─ 6. 异常与生命周期管理：
       ├─ 窗口关闭时：kill 子进程并 wait 清理，避免后台僵尸进程
       ├─ 子进程异常退出时：导航至 ui/error.html，提供错误信息与 restart_server 命令
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

---

## 3. 技术栈与环境依赖 (Technology Stack)

- **语言与框架**：
  - Rust (Edition 2021)
  - Tauri v2 (`tauri = "2"`, `tauri-build = "2"`)
  - Tauri 插件：`tauri-plugin-opener = "2"`（用于系统默认浏览器打开外链）
  - 正则引擎：`regex = "1"`
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
    ├── build.rs               # Tauri 构建脚本
    ├── capabilities/          # Tauri v2 权限配置
    │   └── default.json       # 主窗口权限声明（core:default, opener:default）
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
