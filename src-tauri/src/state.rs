//! 全局共享状态：AppState 与各领域的数据结构

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::process::Child;
use std::sync::atomic::AtomicU64;
use std::sync::Mutex;

use crate::stats_win::{STATS_BAR_HEIGHT, STATS_WIDTH};

#[derive(Default)]
pub(crate) struct AppState {
    pub(crate) child: Mutex<Option<Child>>,
    /// 可选的 OpenCode Go 会话头代理子进程（二进制缺失时为 None）
    pub(crate) proxy: Mutex<Option<Child>>,
    /// 代理启动失败的原因；None 表示正常或未启用
    pub(crate) proxy_error: Mutex<Option<String>>,
    /// 持有 job handle 使整个应用生命周期内 job 存活；进程退出时 handle 关闭，
    /// 触发 KILL_ON_JOB_CLOSE 级联回收 job 内所有进程
    #[cfg(windows)]
    pub(crate) job: Mutex<Option<crate::win_job::Job>>,
    pub(crate) ready: Mutex<bool>,
    pub(crate) quitting: Mutex<bool>,
    pub(crate) last_error: Mutex<String>,
    pub(crate) app_origin: Mutex<String>,
    pub(crate) stats: Mutex<StatsState>,
    /// 今日用量（跨会话聚合）：按本地日期分桶，跨天时整体重置
    pub(crate) daily: Mutex<DailyStats>,
    /// 当前活动选项卡："local" | "remote"
    pub(crate) active_tab: Mutex<String>,
    /// 最近一次采样得到的页面主题与背景色，用于去重后通知 tabbar / 统计浮窗
    pub(crate) last_theme: Mutex<Option<(bool, Option<String>)>>,
    /// 统计浮窗的几何与状态（由 Rust 统一管理，页面只上报内容尺寸）
    pub(crate) stats_geom: Mutex<StatsGeom>,
    /// 拖动循环占用标记，避免重复启动
    pub(crate) stats_dragging: Mutex<bool>,
    /// 内容页当前打开的会话 id（由 SESSION_REPORTER_JS 上报）
    pub(crate) reported_session: Mutex<String>,
    /// 当前 kimi web 子进程代次：重启后旧线程的解析/握手结果据此丢弃
    pub(crate) server_gen: AtomicU64,
    /// 本地服务端点快照：就绪时写入，子进程退出或重启时清空
    pub(crate) endpoint: Mutex<Option<LocalEndpoint>>,
    /// 远程控制隧道状态（选项卡栏提示与重试按钮的数据源）
    pub(crate) remote: Mutex<RemoteStatus>,
}

/// 远程控制隧道的阶段，与 kimi web 的 `data.state`（off/starting/on）对应
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum RemotePhase {
    #[default]
    Idle,
    Connecting,
    Connected,
    Failed,
}

/// 远程控制状态：gen 标识所属子进程代次，避免旧代次线程覆盖新状态
#[derive(Clone, Default)]
pub(crate) struct RemoteStatus {
    pub(crate) gen: u64,
    pub(crate) phase: RemotePhase,
    /// 失败原因（接口 msg 首行或本地错误描述），直接展示在提示条 title 上
    pub(crate) reason: String,
    /// 接口业务错误码，仅用于诊断
    pub(crate) code: Option<i64>,
}

/// 本地服务端点快照：端口与 token 一次性取出，避免加锁间隙被重启替换
#[derive(Clone)]
pub(crate) struct LocalEndpoint {
    pub(crate) port: u16,
    pub(crate) token: String,
}

/// 统计浮窗几何：悬浮/吸附两种模式共用；坐标为相对主窗口客户区的逻辑像素
#[derive(Clone)]
pub(crate) struct StatsGeom {
    pub(crate) docked: bool,
    pub(crate) folded: bool,
    pub(crate) visible: bool,
    pub(crate) x: f64,
    pub(crate) y: f64,
    pub(crate) w: f64,
    pub(crate) h: f64,
}

impl Default for StatsGeom {
    fn default() -> Self {
        Self {
            docked: false,
            folded: false,
            visible: false,
            x: 0.0,
            y: 0.0,
            w: STATS_WIDTH,
            h: STATS_BAR_HEIGHT,
        }
    }
}

/// 会话用量统计的增量解析状态：按会话维度聚合该会话所有 agent 的
/// wire.jsonl（含子 agent）；切换会话时整体重置
#[derive(Default)]
pub(crate) struct StatsState {
    pub(crate) session_id: String,
    pub(crate) cursors: HashMap<PathBuf, WireCursor>,
    pub(crate) agg: SessionAgg,
}

/// 单个 wire.jsonl 的增量读取游标
#[derive(Default)]
pub(crate) struct WireCursor {
    pub(crate) offset: u64,
    /// 最近一条 llm.request 的时间戳，用于与紧随的 usage.record 配对估算 TPS
    pub(crate) last_req_time: Option<i64>,
}

#[derive(Default)]
pub(crate) struct SessionAgg {
    pub(crate) input_other: u64,
    pub(crate) output: u64,
    pub(crate) cache_read: u64,
    pub(crate) cache_creation: u64,
    /// 最近若干次请求的 TPS，用于计算平滑均值
    pub(crate) recent_tps: VecDeque<f64>,
    pub(crate) last_record_time: Option<i64>,
}

/// 今日用量聚合状态：cursors 记录各 wire.jsonl 的增量读取位置，
/// date 为本地日期（yyyy-mm-dd），跨天时整体重置并重扫
#[derive(Default)]
pub(crate) struct DailyStats {
    pub(crate) date: String,
    pub(crate) cursors: HashMap<PathBuf, u64>,
    pub(crate) agg: DailyAgg,
}

#[derive(Default)]
pub(crate) struct DailyAgg {
    pub(crate) input_other: u64,
    pub(crate) output: u64,
    pub(crate) cache_read: u64,
    pub(crate) cache_creation: u64,
}
