# Kimi Code Desktop (tauri_kimi)

把 Kimi Code CLI 的本地 Web UI（`kimi web`）封装成 Windows 桌面应用：双击即用，无浏览器地址栏，本地与远端双连接共存。

## 特性

- 原生窗口 + WebView2，不打包前端；`kimi upgrade` 后界面自动更新
- 顶部选项卡：本地 `kimi web` 与远端 `https://code-rc.kimi.com/` 会话同时存活
- 系统托盘驻留，关窗不退出；托盘“退出”统一回收子进程
- Web 通知、F5 / Ctrl+R 刷新、原生文件拖放、外链走系统浏览器
- 实时用量悬浮窗：TPS、会话与今日 token、缓存命中率

## 环境要求

- Windows 10 (1809+) / Windows 11，已安装 Edge WebView2 Runtime
- Kimi Code CLI（`kimi`，且 `kimi web` 可用）
- 构建时需要 Rust 工具链与 `cargo-tauri`

## 构建与运行

打 `v*` 标签推送到 GitHub 后，Actions 会自动编译并在 Releases 页面附上 `tauri_kimi.exe`。本地构建：

```bash
# 开发模式
cd src-tauri && cargo tauri dev

# 生产构建（产物 src-tauri/target/release/tauri_kimi.exe）
cd src-tauri && cargo build --release
```

## 说明

- 启动时自动执行 `kimi web --rc --no-open --port <port>`，端口首选 58627，占用时自动回退；`--rc` 允许手机或另一台电脑接管本机会话。
- 关闭窗口只隐藏到托盘，需从托盘菜单“退出”才会结束 `kimi web` 子进程。
- 架构与开发细节见 [AGENTS.md](AGENTS.md)。

## 许可证

本项目以 [MIT](LICENSE) 发布。第三方组件：内置字体子集更纱黑体（Sarasa Gothic，OFL-1.1，见 `ui/fonts/LICENSE-Sarasa-Gothic.txt`）；`src-tauri/link-libs/libgcc_eh.a` 来自 GCC 运行时库（GPL-3.0 with GCC Runtime Library Exception，链接使用不构成衍生作品）。
