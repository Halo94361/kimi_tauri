# AGENTS.md - tauri_kimi 开发指南

`tauri_kimi` 是 Tauri v2 (Rust) 的 Windows 桌面壳，把 `kimi web` 的本地 Web UI 包装成双击即用的原生窗口。前端只有 `ui/` 下四个原生静态页。Rust 侧按领域分模块（`src-tauri/src/`）：`lib.rs`（入口：Builder/托盘/窗口与 WebView 创建）、`state.rs`（AppState 与数据struct）、`server.rs`（kimi 子进程生命周期）、`window.rs`（布局/选项卡/导航管控）、`stats_win.rs`（统计浮窗几何与拖动）、`usage.rs`（wire.jsonl 用量统计）、`remote.rs`（远程控制隧道）、`inject.rs`（注入 JS 常量）、`win_job.rs`（Windows 进程树回收）。

## 架构

- **零前端打包**：不打包 kimi web 前端，WebView2 直接加载 `kimi web` 在 `127.0.0.1:<port>` 提供的同源页面；`kimi upgrade` 后界面自动更新。
- **单窗口多 WebView**：裸窗口 + 4 个子 WebView（`tabbar` / `local` / `remote` / `stats`）。本地与远端 `https://code-rc.kimi.com/` 同时存活，切换只改可见性。
- **统计浮窗 `stats`**：独立子 WebView（`ui/stats.html`），固定逻辑宽 300，几何与拖动全部由 Rust 管理。拖入顶部 36px 选项卡栏带内实时吸附为栏右侧信息条，拖出恢复悬浮；吸附/悬浮位置、折叠状态存页面 localStorage，经 `stats_restore` 恢复。`stats` 在内容 WebView 之后创建，wry 建子窗口时置顶，后续 `set_position`/`set_size` 带 `SWP_NOZORDER`，因此始终浮在最上层。拖动用 Rust 轮询 `GetCursorPos` + `GetAsyncKeyState(VK_LBUTTON)`，不依赖跨 WebView 的 pointer capture；拖动结束把焦点还给活动内容 WebView。
- **统计浮窗主题**：跨 WebView 无法使用 `backdrop-filter`，`THEME_SAMPLE_JS` 额外上报页面背景色（`report_theme` 的 `bg`），浮窗以同色作基底消除圆角色差；深浅色与背景色经 `page-theme` 事件下发。
- **统一等宽字体**：壳内自绘页面（`tabbar` / 启动页 / 错误页 / `stats`）统一用更纱黑体等宽子集 `ui/fonts/SarasaMonoSC-UI.woff2`（family `SarasaMonoSC`），随 `ui` 目录打进二进制，不依赖系统是否安装字体；kimi web 内容页不注入此字体。子集由 `scripts/gen_font_subset.py` 生成，新增页面文案后需重新生成。
- **子进程直管**：Rust 直接 spawn `kimi web --no-open --port <port>`，Windows 下设置 `CREATE_NO_WINDOW`，避免控制台黑窗。不带 `--rc` 是有意为之：CLI 的 `--rc` 会在打印本地 URL 前同步等待远端握手，断网时本地界面完全不可用。
- **远程控制开关**：服务就绪后由 `remote.rs` 调本地 `POST /api/v1/remote-control {"enabled":true}` 开隧道，失败只提示、不阻塞本地界面（选项卡栏显示「远程控制未连接」+「重试」）。断网时该 POST 在 CLI 内部永不返回（后台无限退避重连）且 enable 是队列串行的：实现固定 15s 超时，超时后立刻发 `{"enabled":false}` 打断 `starting` 并释放锁；重试前先 GET 状态，`on` 直接复用、`starting` 先取消。状态写回带子进程代次（`server_gen`），旧代次结果丢弃；已连接后每 90s 巡检，状态掉到 `off` 时重新提示。
- **端口选择**：`pick_port` 首选 58627，被占用时改用系统空闲端口。注意 localStorage 按 origin 隔离，回退端口会让主题等页面设置重置（登录态与会话数据不受影响）。
- **就绪探测**：读 stdout 正则解析端口与 token，30s 看门狗兜底；随后导航到 `http://127.0.0.1:<port>/#token=<token>`，并持续清空 stdout 防止管道写满。
- **生命周期**：关窗仅隐藏（托盘驻留，子进程继续跑）；托盘“退出”才 kill 子进程并退出；子进程异常退出导航到 `error.html`，可调 `restart_server` 重启。
- **进程树回收**：spawn 后把 `kimi.exe` 挂入带 `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` 的 Job Object（`win_job`），壳进程无论正常退出、被强杀还是崩溃都会级联回收 MCP 孙进程；创建/绑定失败时退回直接 kill。
- **外链管控**：`is_local()` 白名单（127.0.0.1 / localhost / tauri.localhost）内才允许窗口内导航，其余经 `tauri-plugin-opener` 交给系统浏览器。
- **窗口尺寸**：按主显示器工作区 85% 钳制（宽 720~1600，高 480~1200），适配高 DPI。

## 关键契约

- **桌面桥 `window.kimiDesktop`**：kimi web 前端会无存在性检查地调用 `onUpdateStatus` / `getUpdateStatus` / `setTheme`，缺失会让设置面板抛 `TypeError`。注入脚本必须提供最小实现：`getUpdateStatus` 返回 `{state:"idle"}`、`onUpdateStatus` 返回退订函数、`setTheme` / `log` 空实现或转 console。kimi CLI 升级后设置面板报“出错了”，先查这里。
- **通知**：窗口构建时用 `on_permission_request` 放行 `PermissionKind::Notifications`（需 tauri ≥ 2.12）；点击 toast 经 `show_window` 命令唤回窗口。
- **ACL**：`build.rs` 的 `AppManifest.commands` 为自定义命令生成 `allow-*` 权限；应用源页面（`tabbar` / `local` 内建页 / `stats`）声明在 `capabilities/default.json`，remote 源在 `capabilities/kimi-remote.json`（按 `remote.urls` 放行 `allow-show-window` / `allow-report-theme` / `allow-report-session`）；远程控制提示条位于 tabbar，两条命令（`allow-remote-status` / `allow-retry-remote`）声明在 `default.json`。`stats.html` 需要 `allow-session-stats` / `allow-daily-stats` 与全部 `allow-stats-*`；内容页的会话上报脚本需要 `allow-report-session`。新增命令要同步这两处。
- **敏感信息**：Bearer token 只存内存与 URL fragment，禁止写入日志或持久化。
- **用量统计**：数据源是 `~/.kimi-code/sessions/<workDirKey>/<sessionId>/agents/*/wire.jsonl`；会话 token 累加 `usage.record`，缓存命中率 = `ΣinputCacheRead / Σ(inputOther+inputCacheRead+inputCacheCreation)`，TPS 取 `usage.record` 与最近 `llm.request` 配对（间隔 ≤300s）的最近 10 次均值；今日汇总按事件 epoch 毫秒分桶到本地日期（`time` crate 的 `local-offset`，失败回退 UTC）。

## 构建与验证

在 `src-tauri` 下执行：

```bash
cargo check                                   # 语法与类型检查
cargo tauri dev                               # 开发模式
cargo build --release                         # 产物 target/release/tauri_kimi.exe
```

产物 exe 动态依赖 `WebView2Loader.dll`（webview2-com 构建脚本自动复制到 `target/release/`，无静态链接开关），分发时 zip 打包 exe + dll 同目录，CI 工作流已如此处理。

工具链要求：rustc host 为 `x86_64-pc-windows-gnu`，gcc 用完整 MinGW-Builds（自带 `libgcc_eh.a`）。若用 w64devkit（缺 `libgcc_eh.a`），需自建本机 `src-tauri/.cargo/config.toml`（已 gitignore，可复制 `config.toml.example`），用绝对路径把 `link-libs/` 加入链接参数：

```toml
[target.x86_64-pc-windows-gnu]
rustflags = ["-C", "link-arg=-L<仓库绝对路径>/src-tauri/link-libs"]
```

换机后先跑环境检查（golden 已脱敏，用户名/安装路径差异可忽略）：

```bash
bash scripts/check_env.sh | diff scripts/check_env_golden.txt -
```

重点核对版本号一致、两处 `libgcc_eh.a` 都在、无失效路径告警。更新 golden：`bash scripts/check_env.sh > scripts/check_env_golden.txt`。

CI 行为：推 `master` 只跑 `cargo check --all-targets` 与 `cargo test --release`（含链接与单测；debug 版 cdylib 会超过 MinGW 导出序数上限，必须用 release）；推 `v*` 标签或手动 dispatch 才编译并打包。

## 开发约定

- 按领域模块组织（模块地图见首段），最小修改，不引入多余抽象。
- 注释用中文，写设计意图与平台特殊处理；不写一次性排障记录。
- `ui/` 保持纯原生 HTML/CSS/JS，禁止引入前端框架；Tauri API 走 `window.__TAURI__.core.invoke`。
- 任何启动子进程的路径都必须保证 `WindowEvent::CloseRequested` 与异常分支能 `kill_child`，防止进程泄漏。
- 外部 URL 一律走系统浏览器，不得在内置 WebView 加载。
