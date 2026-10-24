# AGENTS.md - tauri_kimi 开发指南

`tauri_kimi` 是 Tauri v2 (Rust) 的 Windows 桌面壳，把 `kimi web` 的本地 Web UI 包装成双击即用的原生窗口。前端只有 `ui/` 下五个原生静态页（`index` / `tabs` / `error` / `stats` / `settings`）。Rust 侧按领域分模块（`src-tauri/src/`）：`lib.rs`（入口：Builder/托盘/窗口与 WebView 创建）、`state.rs`（AppState 与数据struct）、`server.rs`（kimi 子进程生命周期）、`window.rs`（布局/选项卡/导航管控/主题广播）、`stats_win.rs`（统计浮窗几何与拖动）、`usage.rs`（wire.jsonl 用量统计）、`quota.rs`（Kimi / OpenCode Go 套餐额度）、`remote.rs`（远程控制隧道）、`settings.rs`（壳设置与 settings.json 持久化）、`diagnostics.rs`（状态汇总、CLI 版本与一键升级）、`inject.rs`（注入 JS 常量）、`win_job.rs`（Windows 进程树回收）。

## 架构

- **零前端打包**：不打包 kimi web 前端，WebView2 直接加载 `kimi web` 在 `127.0.0.1:<port>` 提供的同源页面；`kimi upgrade` 后界面自动更新。
- **单窗口多 WebView**：裸窗口 + 5 个子 WebView（`tabbar` / `local` / `remote` / `settings` / `stats`）。本地与远端 `https://code-rc.kimi.com/` 同时存活，切换只改可见性；`remote` 可按设置不创建（隐藏「远端」选项卡，重新开启时由 `ensure_remote_webview` 按需创建），`settings` 是壳自绘设置页。
- **统计浮窗 `stats`**：独立子 WebView（`ui/stats.html`），固定逻辑宽 300，几何与拖动全部由 Rust 管理。拖入顶部 36px 选项卡栏带内实时吸附为栏右侧信息条，拖出恢复悬浮；吸附/悬浮位置、折叠状态存页面 localStorage，经 `stats_restore` 恢复。`stats` 在内容 WebView 之后创建，wry 建子窗口时置顶，后续 `set_position`/`set_size` 带 `SWP_NOZORDER`，因此始终浮在最上层。拖动用 Rust 轮询 `GetCursorPos` + `GetAsyncKeyState(VK_LBUTTON)`，不依赖跨 WebView 的 pointer capture；拖动结束把焦点还给活动内容 WebView。实际可见性由 `apply_stats_visibility` 统一收口：页面意愿 && 设置总开关 && 当前不在设置页；`stats_reset_position` 复位几何并经 `stats-reset` 事件清页面 localStorage。明细页含套餐额度、近 24 小时柱图与本会话/今日指标，全部用半透明填充与等宽数字；悬停横条展开，pin 常驻。
- **套餐额度**：`quota.rs` 的 `quota_stats` 拉两路数据。Kimi 走 `https://api.kimi.com/coding/v1/usages`，读 `~/.kimi-code/credentials/kimi-code.json`；token 距过期不足 120s 或接口 401 时，用 CLI 的 refresh_token 刷新（client id 与 CLI 一致）并原子写回，写回前核对 refresh_token 未被别的进程替换。OpenCode Go 直连 `https://opencode.ai/zen/go/v1/usage`（Bearer key），key 取 CLI `config.toml` 的 `[providers."OpenCode Go"].api_key`（其次按 provider 名含 opencode、base_url 指向 127.0.0.1:58787 的顺序回退），找不到 key 或直连网络层失败（证书吊销检查 / 代理）时才回退 `seekfetch opencode usage --json`。缓存 TTL 跟随设置 `stats_quota_refresh_secs`（默认 120s，取间隔减 5s），失败回退上次成功数据并标 `stale`；OpenCode 未安装/未配置时返回 `configured=false`，浮窗静默隐藏。设置 `stats_quota_kimi` / `stats_quota_opencode` 关闭时后端直接跳过对应查询。参考线：Kimi 按窗口时间匀速；OpenCode 的 5h / 本周上限是月额度 20% / 50%，线按 `本月已用 ÷ 20%/50%` 折算到窗口刻度。
- **主题同步**：Rust 每 50ms 轮询当前活动内容 WebView，`THEME_SAMPLE_JS` 同步读取计算样式并立即经 `report_theme` 上报，不依赖 `requestAnimationFrame` 或页面重绘；切卡与 `setTheme` 也直接触发采样，非活动页面的迟到来报会被丢弃，启动时 `active_tab` 必须初始化为 `local`。采样结果变化后 `broadcast_theme` 立即用同一事件广播全部 WebView，并同步原生标题栏、窗口与各 WebView 的首绘背景色。自绘页创建时由 `shell_theme_init_js` 在 CSS 解析前写入有效主题，监听注册后再调用 `sync_theme` 补发。设置可强制深浅色（`effective_theme`），固定时不再沿用页面底色，改为浮窗默认底色。
- **统计浮窗主题**：跨 WebView 无法使用 `backdrop-filter`，浮窗以采样背景色作基底消除圆角色差；深浅色与背景色经 `page-theme` 事件下发。
- **统一等宽字体**：壳内自绘页面（`tabbar` / 启动页 / 错误页 / `stats` / `settings`）统一用更纱黑体等宽子集 `ui/fonts/SarasaMonoSC-UI.woff2`（family `SarasaMonoSC`），随 `ui` 目录打进二进制，不依赖系统是否安装字体；kimi web 内容页不注入此字体。子集由 `scripts/gen_font_subset.py` 生成，新增页面文案后需重新生成。
- **子进程直管**：Rust 直接 spawn `kimi web --no-open --port <port>`，Windows 下设置 `CREATE_NO_WINDOW`，避免控制台黑窗。不带 `--rc` 是有意为之：CLI 的 `--rc` 会在打印本地 URL 前同步等待远端握手，断网时本地界面完全不可用。
- **OpenCode Go 代理（可选）**：CLI 不发 OpenCode Go 要求的 `x-opencode-session` 头（上游 [#3506](https://github.com/MoonshotAI/kimi-code/issues/3506) 明确拒绝适配）。`server.rs` 在启动 kimi 前按策略拉起 `opencode-session-proxy.exe`（依次查 exe 同目录、`%USERPROFILE%\.kimi-code\bin`、PATH），由它从请求体 `prompt_cache_key` 取值注入该头；找不到二进制时静默跳过，不影响其他 provider。启用时把对应 provider 的 `base_url` 指向 `http://127.0.0.1:58787/openai/v1`。启动前用 `/__ocsp_health` 区分“代理正常 / 端口被占用 / 未监听”，拉起后最多等 3 秒确认就绪；失败且策略要求启动时，选项卡栏显示「OpenCode 代理未启动」（原因见 tooltip）并提供重试，事件为 `proxy-status`。策略由设置 `proxy_mode` 决定：自动 = 仅 CLI 配置指向 127.0.0.1:58787（`proxy_required`，含 KIMI_CODE_HOME），始终 = 无条件启动，关闭 = 不启动不提示。
- **远程控制开关**：服务就绪后由 `remote.rs` 调本地 `POST /api/v1/remote-control {"enabled":true}` 开隧道，失败只提示、不阻塞本地界面（选项卡栏显示「远程控制未连接」+「重试」），不自动重试。断网时该 POST 在 CLI 内部永不返回（后台无限退避重连）且 enable 是队列串行的：实现固定 15s 超时，超时后立刻发 `{"enabled":false}` 打断 `starting` 并释放锁；重试前先 GET 状态，`on` 直接复用、`starting` 先取消。状态写回带子进程代次（`server_gen`），旧代次结果丢弃；已连接后每 90s 巡检，状态掉到 `off` 时重新提示。设置 `remote_auto_connect=false` 时启动不再自动开启（只影响下次启动，不动当前隧道）；设置页可手动连接（`retry_remote`）与断开（`disconnect_remote`，发 `{"enabled":false}` 并回到 Idle）。
- **端口选择**：`pick_port` 首选 58627，被占用时改用系统空闲端口；设置 `kimi_port` 非 0 时直接使用该端口（被占用由 CLI 自行 +1，实际端口以 stdout 解析为准）。注意 localStorage 按 origin 隔离，回退端口会让主题等页面设置重置（登录态与会话数据不受影响）。
- **就绪探测**：读 stdout 正则解析端口与 token，30s 看门狗兜底；随后导航到 `http://127.0.0.1:<port>/#token=<token>`，并持续清空 stdout 防止管道写满。
- **生命周期**：关窗行为由设置 `close_to_tray` 决定：默认仅隐藏（托盘驻留，子进程继续跑），选直接退出则走 `quit_app` 回收子进程；托盘菜单含显示窗口 / 设置（唤出并切到设置页）/ 重启本地服务 / 退出，“退出”始终 kill 子进程并退出；子进程异常退出导航到 `error.html`，可调 `restart_server` 重启。
- **进程树回收**：spawn 后把 `kimi.exe` 与可选的 `opencode-session-proxy.exe` 挂入带 `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` 的 Job Object（`win_job`），壳进程无论正常退出、被强杀还是崩溃都会级联回收 MCP 孙进程；创建/绑定失败时退回直接 kill。
- **外链管控**：`is_local()` 白名单（127.0.0.1 / localhost / tauri.localhost）内才允许窗口内导航，其余经 `tauri-plugin-opener` 交给系统浏览器。
- **窗口尺寸**：按主显示器工作区 85% 钳制（宽 720~1600，高 480~1200），适配高 DPI。
- **壳设置**：`settings.rs` 持久化到 `app_config_dir/settings.json`（Windows 为 `%APPDATA%\com.tauri-kimi.app\settings.json`），字段全带默认值、文件缺失或损坏回退默认；统计浮窗展示开关（`stats_show_usage` / `stats_quota_kimi` / `stats_quota_opencode`）与额度刷新间隔（`stats_quota_refresh_secs`，默认 120s，钳制 30~3600）也在其中；写入走 tmp + `MoveFileExW` 原子替换。`set_settings` 先落盘再生效，广播 `settings-changed`（同时发给 `stats`）并应用即时副作用（主题/远端选项卡/浮窗/代理模式/用量与额度开关/额度刷新间隔）；kimi 路径与端口只保存，重启本地服务后生效，`state.launched` 记录本次启动实际配置，与当前设置不一致时 `get_diagnostics.restart_pending=true`，设置页显示「待重启」提示条。设置页 `ui/settings.html` 是第三个内容 WebView，命令：`get_settings` / `set_settings` / `reset_settings` / `get_diagnostics` / `refresh_cli_info` / `upgrade_cli` / `open_config_target` / `disconnect_remote` / `stats_reset_position`。
- **CLI 升级**：设置页二次确认（提示打断对话并重启）后 `upgrade_cli` 后台执行 `kimi upgrade -y`（10 分钟超时、stdout/stderr 捕获、防重入、`CREATE_NO_WINDOW`），成功走 `restart_server` 重启本地服务使新二进制生效，进度经 `cli-upgrade` 事件回发。`refresh_cli_info` 读 `kimi --version` 与 `~/.kimi-code/updates/latest.json`（`latest` / `checkedAt`），只做展示、不主动触发网络检查；`kimi upgrade` 没有只检查不安装的参数。诊断汇总不含 token。

## 关键契约

- **桌面桥 `window.kimiDesktop`**：kimi web 前端会无存在性检查地调用 `onUpdateStatus` / `getUpdateStatus` / `setTheme`，缺失会让设置面板抛 `TypeError`。注入脚本必须提供最小实现：`getUpdateStatus` 返回 `{state:"idle"}`、`onUpdateStatus` 返回退订函数、`setTheme` 触发即时主题采样、`log` 转 console。kimi CLI 升级后设置面板报“出错了”，先查这里。
- **通知**：窗口构建时用 `on_permission_request` 按设置 `notifications` 放行 `PermissionKind::Notifications`（需 tauri ≥ 2.12），关闭时返回 Deny；点击 toast 经 `show_window` 命令唤回窗口。
- **ACL**：`build.rs` 的 `AppManifest.commands` 为自定义命令生成 `allow-*` 权限；应用源页面（`tabbar` / `local` 内建页 / `stats` / `settings`）声明在 `capabilities/default.json`，remote 源在 `capabilities/kimi-remote.json`（按 `remote.urls` 放行 `allow-show-window` / `allow-report-theme` / `allow-report-session`）；远程控制提示条位于 tabbar，两条命令（`allow-remote-status` / `allow-retry-remote`）声明在 `default.json`。`stats.html` 需要 `allow-session-stats` / `allow-daily-stats` / `allow-hourly-stats` / `allow-quota-stats` 与全部 `allow-stats-*`；内容页的会话上报脚本需要 `allow-report-session`。设置页与选项卡栏用到的 `allow-get-settings` 等九条新命令同样只在 `default.json`，remote 源不授权。新增命令要同步这两处。
- **敏感信息**：Bearer token 只存内存与 URL fragment，禁止写入日志或持久化。
- **用量统计**：数据源是 `~/.kimi-code/sessions/<workDirKey>/<sessionId>/agents/*/wire.jsonl`；会话 token 累加 `usage.record`，缓存命中率 = `ΣinputCacheRead / Σ(inputOther+inputCacheRead+inputCacheCreation)`，TPS 取 `usage.record` 与最近 `llm.request` 配对（间隔 ≤300s）的最近 10 次均值；今日汇总按事件 epoch 毫秒分桶到本地日期（`time` crate 的 `local-offset`，失败回退 UTC）。`weekly_stats` 供设置页近 7 天柱状图：逐日 `previous_day` 推日期边界（避免夏令时漂移），事件按 `bucket_index` 分桶；与 `daily_stats` 的常驻游标不同，它按需全量解析，但只读 mtime 不早于窗口起点的 wire 文件，窗口内无记录时 `found=false`。`hourly_stats` 供统计浮窗近 24 小时柱图：按本地整点分桶（`floor_hour` 处理非整小时与负偏移），主/子代理分开累计，同样用增量游标；滚出窗口的桶与游标会被丢弃，页面 30s 轮询。

## 构建与验证

在 `src-tauri` 下执行：

```bash
cargo check                                   # 语法与类型检查
cargo tauri dev                               # 开发模式
cargo build --release                         # 产物 target/release/tauri_kimi.exe
```

统计浮窗改版后先用 `python3 scripts/render_stats_preview.py` 做视觉回测：脚本把 `ui/stats.html` 注入样例数据，经 headless chromium 渲染出 `ui/.preview/stats-{dark,light}.png`（产物已 gitignore）。它不依赖 Tauri 运行时，改 CSS/图表后可直接重跑。

产物 exe 动态依赖 `WebView2Loader.dll`（webview2-com 构建脚本自动复制到 `target/release/`，无静态链接开关），分发时 zip 打包 exe + dll 同目录，CI 工作流已如此处理。

Go sidecar 在 `sidecar/opencode-session-proxy/` 下独立构建，无第三方依赖：

```bash
cd sidecar/opencode-session-proxy
CGO_ENABLED=0 GOOS=windows GOARCH=amd64 go build -trimpath -buildvcs=false -ldflags="-s -w" -o opencode-session-proxy.exe .
```

CI 的 verify 与 build 两个 job 都会先编译它；Release 的 zip 内与 `tauri_kimi.exe` 同目录。仓库保留一份编译产物，未装 Go 的开发者可直接运行。

工具链要求：rustc host 为 `x86_64-pc-windows-gnu`，gcc 用完整 MinGW-Builds（自带 `libgcc_eh.a`）。若用 w64devkit（缺 `libgcc_eh.a`），需自建本机 `src-tauri/.cargo/config.toml`（已 gitignore，可复制 `config.toml.example`），用绝对路径把 `link-libs/` 加入链接参数：

```toml
[target.x86_64-pc-windows-gnu]
rustflags = ["-C", "link-arg=-L<仓库绝对路径>/src-tauri/link-libs"]
```

CI 行为：推 `master` 只跑 `cargo check --all-targets` 与 `cargo test --release`（含链接与单测；debug 版 cdylib 会超过 MinGW 导出序数上限，必须用 release）；推 `v*` 标签或手动 dispatch 才编译并打包。

## 开发约定

- 按领域模块组织（模块地图见首段），最小修改，不引入多余抽象。
- 注释用中文，写设计意图与平台特殊处理；不写一次性排障记录。
- `ui/` 保持纯原生 HTML/CSS/JS，禁止引入前端框架；Tauri API 走 `window.__TAURI__.core.invoke`。
- 任何启动子进程的路径都必须保证 `WindowEvent::CloseRequested` 与异常分支能 `kill_child`，防止进程泄漏。
- 外部 URL 一律走系统浏览器，不得在内置 WebView 加载。
- **UI 来源声明**：`ui/stats.html` 的视觉语言与图表技法参考 `bfjnbvf/kimi-code-monitor`（MIT）；改动该页时保留文件头注释、README 许可证一节与 `THIRD_PARTY_NOTICES.md` 中的声明。
- **推送**：每次提交都要同步推送到两个 remote（GitHub 与内网镜像），只推一个会让两处历史分叉。纯文档/说明类改动在 commit message 末尾加 `[skip ci]`，避免触发 GitHub Actions 构建。
