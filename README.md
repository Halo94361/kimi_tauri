# Kimi Code Desktop (tauri_kimi)

把 Kimi Code CLI 的本地 Web UI（`kimi web`）封装成 Windows 桌面应用：双击即用，无浏览器地址栏，本地与远端双连接共存。

## 特性

- 原生窗口 + WebView2，不打包前端；`kimi upgrade` 后界面自动更新
- 顶部选项卡：本地 `kimi web` 与远端 `https://code-rc.kimi.com/` 会话同时存活
- 设置选项卡：rc 自动开启与手动连接/断开、远端选项卡显隐、kimi 路径与端口、OpenCode Go 代理策略、关窗行为、壳主题、通知、统计浮窗开关、近 7 天用量图，以及状态诊断与二次确认后的一键升级 CLI
- 系统托盘驻留，关窗不退出；托盘菜单含设置与重启本地服务入口，“退出”统一回收子进程
- Web 通知、F5 / Ctrl+R 刷新、原生文件拖放、外链走系统浏览器
- 实时用量悬浮窗：TPS、会话与今日 token、缓存命中率

## 环境要求

- Windows 10 (1809+) / Windows 11，已安装 Edge WebView2 Runtime
- Kimi Code CLI（`kimi`，且 `kimi web` 可用）
- 构建时需要 Rust 工具链与 `cargo-tauri`

## 构建与运行

打 `v*` 标签推送到 GitHub 后，Actions 会自动编译并在 Releases 页面发布 `tauri_kimi-windows-x64.zip`。**解压后请保持 `WebView2Loader.dll` 与 exe 同目录**（exe 动态依赖它；WebView2 运行时本身 Win10/11 通常已自带）。本地构建：

```bash
# 开发模式
cd src-tauri && cargo tauri dev

# 生产构建（产物 src-tauri/target/release/tauri_kimi.exe）
cd src-tauri && cargo build --release
```

## 说明

- 启动时执行 `kimi web --no-open --port <port>`，端口首选 58627，占用时自动回退（可在设置中指定固定端口与 kimi 路径，重启服务后生效）；本地界面就绪后由壳调用本地 `/api/v1/remote-control` 接口开启远程隧道（手机或另一台电脑可接管本机会话）。网络不佳时开启失败不阻塞本地界面，选项卡栏会显示提示与「重试」按钮；设置中可关闭启动自动开启（只影响下次启动），无订阅用户不会再看到失败提示。
- 关闭窗口默认只隐藏到托盘（可在设置改为直接退出），需从托盘菜单“退出”才会结束 `kimi web` 子进程。
- 使用 OpenCode Go 订阅时，把 `%USERPROFILE%\.kimi-code\config.toml` 里该 provider 的 `base_url` 设为 `http://127.0.0.1:58787/openai/v1`。Release 包已附带 `opencode-session-proxy.exe`（与主程序同目录；源码与工作原理见 [sidecar/opencode-session-proxy/README.md](sidecar/opencode-session-proxy/README.md)），壳默认仅在配置指向代理时才于启动 `kimi` 前拉起并随进程树回收（设置中可改为始终启动或关闭）；缺少该文件时自动跳过，不影响其他 provider。
- 架构与开发细节见 [AGENTS.md](AGENTS.md)。

## 许可证

本项目以 [MIT](LICENSE) 发布。第三方组件：内置字体子集更纱黑体（Sarasa Gothic，OFL-1.1，见 `ui/fonts/LICENSE-Sarasa-Gothic.txt`）；`src-tauri/link-libs/libgcc_eh.a` 来自 GCC 运行时库（GPL-3.0 with GCC Runtime Library Exception，链接使用不构成衍生作品）。
