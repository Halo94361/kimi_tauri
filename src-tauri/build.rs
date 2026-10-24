fn main() {
    // w64devkit 的 gcc 缺少 libgcc_eh.a，把项目内补齐的库目录加入链接搜索路径
    // （内容来自 rust 工具链 self-contained；用 CARGO_MANIFEST_DIR 避免硬编码绝对路径）
    println!(
        "cargo:rustc-link-search=native={}/link-libs",
        std::env::var("CARGO_MANIFEST_DIR").unwrap()
    );

    // 声明 app ACL manifest：为自定义命令自动生成 allow-/deny- 权限标识，
    // 供 capabilities 引用（remote 源页面调用自定义命令必须经此显式授权）
    let attributes = tauri_build::Attributes::new().app_manifest(
        tauri_build::AppManifest::new().commands(&[
            "last_error",
            "restart_server",
            "remote_status",
            "retry_remote",
            "proxy_status",
            "retry_proxy",
            "show_window",
            "session_stats",
            "daily_stats",
            "weekly_stats",
            "switch_tab",
            "report_theme",
            "sync_theme",
            "report_session",
            "stats_restore",
            "stats_set_size",
            "stats_set_visible",
            "stats_focus_content",
            "stats_drag_start",
            "stats_reset_position",
            "disconnect_remote",
            "get_settings",
            "set_settings",
            "reset_settings",
            "get_diagnostics",
            "refresh_cli_info",
            "upgrade_cli",
            "open_config_target",
        ]),
    );
    tauri_build::try_build(attributes).expect("failed to run tauri-build");
}
