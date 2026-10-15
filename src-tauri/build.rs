fn main() {
    // 声明 app ACL manifest：为自定义命令自动生成 allow-/deny- 权限标识，
    // 供 capabilities 引用（remote 源页面调用自定义命令必须经此显式授权）
    let attributes = tauri_build::Attributes::new().app_manifest(
        tauri_build::AppManifest::new().commands(&["last_error", "restart_server", "show_window", "session_stats", "switch_tab", "report_theme"]),
    );
    tauri_build::try_build(attributes).expect("failed to run tauri-build");
}
