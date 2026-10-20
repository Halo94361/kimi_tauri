//! 用量统计：增量解析 ~/.kimi-code 下各会话 agents/*/wire.jsonl，
//! 聚合 usage.record 计算 token 用量、缓存命中率与 TPS

use std::path::{Path, PathBuf};

use crate::state::{AppState, DailyAgg, DailyStats, SessionAgg, StatsState, WireCursor};

/// 内容页会话切换上报（SESSION_REPORTER_JS）
#[tauri::command]
pub(crate) fn report_session(session_id: String, state: tauri::State<AppState>) {
    let mut cur = state.reported_session.lock().unwrap();
    if *cur != session_id {
        *cur = session_id;
    }
}

/// Kimi Code 数据根目录：优先 KIMI_CODE_HOME，默认 ~/.kimi-code
fn kimi_code_home() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("KIMI_CODE_HOME") {
        if !p.is_empty() {
            return Some(PathBuf::from(p));
        }
    }
    for var in ["USERPROFILE", "HOME"] {
        if let Ok(h) = std::env::var(var) {
            if !h.is_empty() {
                return Some(PathBuf::from(h).join(".kimi-code"));
            }
        }
    }
    None
}

/// 按 sessionId 定位会话目录：sessions/<workDirKey>/<sessionId>
fn find_session_dir(home: &Path, session_id: &str) -> Option<PathBuf> {
    for wd in std::fs::read_dir(home.join("sessions")).ok()?.flatten() {
        let cand = wd.path().join(session_id);
        if cand.is_dir() {
            return Some(cand);
        }
    }
    None
}

/// 未提供 sessionId 时的回退：取 main wire.jsonl mtime 最新的会话，
/// 即当前最活跃会话
fn find_latest_session_dir(home: &Path) -> Option<PathBuf> {
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for wd in std::fs::read_dir(home.join("sessions")).ok()?.flatten() {
        for sess in std::fs::read_dir(wd.path()).ok()?.flatten() {
            let wire = sess.path().join("agents").join("main").join("wire.jsonl");
            let Ok(mtime) = wire.metadata().and_then(|m| m.modified()) else {
                continue;
            };
            if best.as_ref().is_none_or(|(t, _)| mtime > *t) {
                best = Some((mtime, sess.path()));
            }
        }
    }
    best.map(|(_, p)| p)
}

/// 从游标位置增量读取 wire.jsonl，只处理完整行（末尾半行留下轮），
/// 聚合 usage.record 并把每条记录与前方的 llm.request 配对估算 TPS
fn consume_wire(cur: &mut WireCursor, agg: &mut SessionAgg, path: &Path) {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return,
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    if len < cur.offset {
        // 文件被截断重建，从头解析
        cur.offset = 0;
        cur.last_req_time = None;
    }
    if f.seek(SeekFrom::Start(cur.offset)).is_err() {
        return;
    }
    let mut buf = Vec::new();
    if f.read_to_end(&mut buf).is_err() {
        return;
    }
    let complete = buf.iter().rposition(|&b| b == b'\n').map(|i| i + 1).unwrap_or(0);
    cur.offset += complete as u64;
    for line in String::from_utf8_lossy(&buf[..complete]).lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        match v.get("type").and_then(|t| t.as_str()) {
            Some("llm.request") => {
                cur.last_req_time = v.get("time").and_then(|t| t.as_i64());
            }
            Some("usage.record") => {
                let Some(u) = v.get("usage") else { continue };
                let get = |k: &str| u.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
                let output = get("output");
                agg.input_other += get("inputOther");
                agg.output += output;
                agg.cache_read += get("inputCacheRead");
                agg.cache_creation += get("inputCacheCreation");
                let Some(t) = v.get("time").and_then(|t| t.as_i64()) else {
                    continue;
                };
                agg.last_record_time = Some(agg.last_record_time.map_or(t, |x| x.max(t)));
                // 配对前方的 llm.request：间隔即本次请求耗时（含排队/重试，
                // 与 kimi web 内置组件口径一致的近似值）；异常间隔直接丢弃
                if let Some(req_t) = cur.last_req_time.take() {
                    let dur_ms = t - req_t;
                    if (1..=300_000).contains(&dur_ms) {
                        agg.recent_tps
                            .push_back(output as f64 / (dur_ms as f64 / 1000.0));
                        if agg.recent_tps.len() > 10 {
                            agg.recent_tps.pop_front();
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

/// 返回指定会话的累计用量与实时 TPS，供统计浮窗轮询；
/// 未指定时优先使用内容页上报的当前会话，再回退到最活跃会话
#[tauri::command]
pub(crate) fn session_stats(
    session_id: Option<String>,
    state: tauri::State<AppState>,
) -> serde_json::Value {
    let not_found = serde_json::json!({ "found": false });
    // 服务未就绪（启动加载页/错误页）时不展示历史会话的快照
    if !*state.ready.lock().unwrap() {
        return not_found;
    }
    let Some(home) = kimi_code_home() else { return not_found };
    let reported = state.reported_session.lock().unwrap().clone();
    let dir = session_id
        .as_deref()
        .filter(|s| !s.is_empty())
        .or_else(|| Some(reported.as_str()).filter(|s| !s.is_empty()))
        .and_then(|s| find_session_dir(&home, s))
        .or_else(|| find_latest_session_dir(&home));
    let Some(dir) = dir else { return not_found };
    let real_id = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();

    let mut st = state.stats.lock().unwrap();
    if st.session_id != real_id {
        *st = StatsState {
            session_id: real_id.clone(),
            ..Default::default()
        };
    }
    if let Ok(entries) = std::fs::read_dir(dir.join("agents")) {
        // 拆分字段借用，避免 cursors 与 agg 同时可变借用 st
        let StatsState { cursors, agg, .. } = &mut *st;
        for e in entries.flatten() {
            let wire = e.path().join("wire.jsonl");
            if wire.is_file() {
                let cur = cursors.entry(wire.clone()).or_default();
                consume_wire(cur, agg, &wire);
            }
        }
    }

    let agg = &st.agg;
    let input_total = agg.input_other + agg.cache_read + agg.cache_creation;
    let hit_rate = if input_total > 0 {
        agg.cache_read as f64 * 100.0 / input_total as f64
    } else {
        0.0
    };
    let tps_avg = if agg.recent_tps.is_empty() {
        0.0
    } else {
        agg.recent_tps.iter().sum::<f64>() / agg.recent_tps.len() as f64
    };
    serde_json::json!({
        "found": true,
        "session_id": real_id,
        "has_tps": !agg.recent_tps.is_empty(),
        "tps_avg": tps_avg,
        "tps_latest": agg.recent_tps.back().copied().unwrap_or(0.0),
        "input_total": input_total,
        "output_total": agg.output,
        "cache_read": agg.cache_read,
        "cache_creation": agg.cache_creation,
        "cache_hit_rate": hit_rate,
        "last_record_time": agg.last_record_time,
    })
}

/// 本地今日日期字符串（yyyy-mm-dd）与今日 00:00 的 epoch 毫秒；
/// 获取本地时区失败时回退 UTC
fn local_today() -> (String, i64) {
    use time::{OffsetDateTime, UtcOffset};
    let offset = UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC);
    let today = OffsetDateTime::now_utc().to_offset(offset).date();
    let midnight = today.midnight().assume_offset(offset).unix_timestamp() * 1000;
    (today.to_string(), midnight)
}

/// 从游标位置增量读取 wire.jsonl，只聚合 time >= day_start 的 usage.record
/// （按事件时间戳直接分桶到今日，避免跨天会话重复计算）；
/// 末尾半行留下轮，文件被截断重建时从头解析
fn consume_wire_today(offset: &mut u64, agg: &mut DailyAgg, path: &Path, day_start: i64) {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return,
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    if len < *offset {
        *offset = 0;
    }
    if f.seek(SeekFrom::Start(*offset)).is_err() {
        return;
    }
    let mut buf = Vec::new();
    if f.read_to_end(&mut buf).is_err() {
        return;
    }
    let complete = buf.iter().rposition(|&b| b == b'\n').map(|i| i + 1).unwrap_or(0);
    *offset += complete as u64;
    for line in String::from_utf8_lossy(&buf[..complete]).lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if v.get("type").and_then(|t| t.as_str()) != Some("usage.record") {
            continue;
        }
        let Some(t) = v.get("time").and_then(|t| t.as_i64()) else {
            continue;
        };
        if t < day_start {
            continue;
        }
        let Some(u) = v.get("usage") else { continue };
        let get = |k: &str| u.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
        agg.input_other += get("inputOther");
        agg.output += get("output");
        agg.cache_read += get("inputCacheRead");
        agg.cache_creation += get("inputCacheCreation");
    }
}

/// 返回本地今日全部会话（含子 agent）的累计用量，供统计悬浮窗轮询。
/// 首次调用对今日活跃文件全量回填，之后按 offset 增量解析；跨天整体重置。
/// async：同步命令跑在主线程，首次回填的目录遍历可能阻塞 UI
/// （含引用参数的 async 命令按 Tauri 约定必须返回 Result；本命令不失败，恒为 Ok）
#[tauri::command]
pub(crate) async fn daily_stats(
    state: tauri::State<'_, AppState>,
) -> Result<serde_json::Value, ()> {
    let not_found = serde_json::json!({ "found": false });
    if !*state.ready.lock().unwrap() {
        return Ok(not_found);
    }
    let Some(home) = kimi_code_home() else {
        return Ok(not_found);
    };
    let (today, day_start) = local_today();

    let mut st = state.daily.lock().unwrap();
    if st.date != today {
        *st = DailyStats {
            date: today,
            ..Default::default()
        };
    }

    let mut active_today = false;
    if let Ok(workdirs) = std::fs::read_dir(home.join("sessions")) {
        // 拆分字段借用，避免 cursors 与 agg 同时可变借用 st
        let DailyStats { cursors, agg, .. } = &mut *st;
        for wd in workdirs.flatten() {
            let Ok(sessions) = std::fs::read_dir(wd.path()) else {
                continue;
            };
            for sess in sessions.flatten() {
                let Ok(agents) = std::fs::read_dir(sess.path().join("agents")) else {
                    continue;
                };
                for agent in agents.flatten() {
                    let wire = agent.path().join("wire.jsonl");
                    if !wire.is_file() {
                        continue;
                    }
                    // append-only：mtime 早于今日 0 点的文件不可能含今日事件
                    let mtime_ms = wire
                        .metadata()
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_millis() as i64)
                        .unwrap_or(0);
                    if mtime_ms < day_start {
                        continue;
                    }
                    active_today = true;
                    let offset = cursors.entry(wire.clone()).or_insert(0);
                    consume_wire_today(offset, agg, &wire, day_start);
                }
            }
        }
    }

    let agg = &st.agg;
    let input_total = agg.input_other + agg.cache_read + agg.cache_creation;
    let hit_rate = if input_total > 0 {
        agg.cache_read as f64 * 100.0 / input_total as f64
    } else {
        0.0
    };
    Ok(serde_json::json!({
        "found": active_today,
        "input_total": input_total,
        "output_total": agg.output,
        "cache_read": agg.cache_read,
        "cache_hit_rate": hit_rate,
    }))
}
