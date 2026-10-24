//! 套餐额度：Kimi Code 的 5 小时 / 本周额度（官方 usages 接口），
//! 以及 OpenCode Go 的滚动 / 本周 / 本月额度（本机 seekfetch CLI）。
//!
//! 两条链路都带短 TTL 缓存与失败回退：请求失败时保留上次成功数据，
//! 统计浮窗不会因为一次网络抖动整块变空。Kimi 凭据沿用 CLI 自己的
//! `~/.kimi-code/credentials/kimi-code.json`；只有 token 临近过期或接口
//! 返回 401 时才刷新，并在刷新后原子写回，绝不记录 token 内容。

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Map, Value};
use tauri::{AppHandle, Manager};

use crate::state::AppState;
use crate::usage::kimi_code_home;

const USAGES_API: &str = "https://api.kimi.com/coding/v1/usages";
const TOKEN_API: &str = "https://auth.kimi.com/api/oauth/token";
/// Kimi Code CLI 的公开 OAuth 客户端，与 CLI 自身使用的一致
const CLIENT_ID: &str = "17e5f671-d194-4dfb-9706-5516cb48c098";

const FIVE_HOUR_MS: i64 = 5 * 60 * 60 * 1000;
const WEEK_MS: i64 = 7 * 24 * 60 * 60 * 1000;
const MONTH_MS: i64 = 30 * 24 * 60 * 60 * 1000;

const HTTP_TIMEOUT: Duration = Duration::from_secs(8);
const SEEKFETCH_TIMEOUT: Duration = Duration::from_secs(8);
/// Kimi 额度变化慢，60s 轮询下 45s 缓存仍能保证每次取新值
const KIMI_CACHE_TTL: Duration = Duration::from_secs(45);
/// seekfetch 要起进程 + 走网络，缓存久一点
const OPENCODE_CACHE_TTL: Duration = Duration::from_secs(120);
/// token 距过期不足该值时提前刷新，避免请求撞在过期边界上
const REFRESH_MARGIN_SECS: i64 = 120;

/// 额度缓存（按来源分开，失败时回退到各自的旧值）
#[derive(Default)]
pub(crate) struct QuotaCache {
    pub(crate) kimi: Option<(Instant, Value)>,
    pub(crate) opencode: Option<(Instant, Value)>,
}

/// 统计浮窗轮询入口：网络与进程调用都放到阻塞线程，避免卡住 UI
#[tauri::command]
pub(crate) async fn quota_stats(force: Option<bool>, app: AppHandle) -> Result<Value, ()> {
    let force = force.unwrap_or(false);
    let result = tauri::async_runtime::spawn_blocking(move || fetch_all(&app, force))
        .await
        .unwrap_or_else(|_| {
            json!({
                "kimi": { "ok": false, "error": "额度查询线程异常" },
                "opencode": { "ok": false, "error": "额度查询线程异常" },
            })
        });
    Ok(result)
}

/// 额度查询失败：`configured=false` 表示「本机没装 / 没配」，
/// 页面应静默隐藏该数据源，而不是显示一条错误
#[derive(Debug)]
struct QuotaError {
    message: String,
    configured: bool,
}

impl QuotaError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            configured: true,
        }
    }

    fn unconfigured(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            configured: false,
        }
    }
}

fn fetch_all(app: &AppHandle, force: bool) -> Value {
    let state = app.state::<AppState>();
    // 串行化：两个来源之外只允许一个查询在跑，失败重试不会叠成并发风暴
    let _guard = state.quota_lock.lock().unwrap();
    let (kimi_enabled, opencode_enabled) = {
        let settings = state.settings.lock().unwrap();
        (settings.stats_quota_kimi, settings.stats_quota_opencode)
    };

    let kimi = if !kimi_enabled {
        json!({ "ok": false, "disabled": true })
    } else {
        let cached = state.quota_cache.lock().unwrap().kimi.clone();
        let hit = if force {
            None
        } else {
            cached
                .as_ref()
                .filter(|(at, _)| at.elapsed() < KIMI_CACHE_TTL)
                .map(|(_, v)| v.clone())
        };
        hit.unwrap_or_else(|| match fetch_kimi() {
            Ok(v) => {
                state.quota_cache.lock().unwrap().kimi = Some((Instant::now(), v.clone()));
                v
            }
            Err(e) => stale_or_error(cached, e),
        })
    };

    let opencode = if !opencode_enabled {
        json!({ "ok": false, "disabled": true })
    } else {
        let cached = state.quota_cache.lock().unwrap().opencode.clone();
        let hit = if force {
            None
        } else {
            cached
                .as_ref()
                .filter(|(at, _)| at.elapsed() < OPENCODE_CACHE_TTL)
                .map(|(_, v)| v.clone())
        };
        hit.unwrap_or_else(|| match fetch_opencode() {
            Ok(v) => {
                state.quota_cache.lock().unwrap().opencode = Some((Instant::now(), v.clone()));
                v
            }
            Err(e) => stale_or_error(cached, e),
        })
    };

    json!({ "kimi": kimi, "opencode": opencode })
}

/// 失败时回退上次成功数据，并标记 stale 让页面可以弱化提示
fn stale_or_error(cached: Option<(Instant, Value)>, error: QuotaError) -> Value {
    match cached {
        Some((_, mut v)) => {
            v["stale"] = Value::Bool(true);
            v["warning"] = Value::String(error.message);
            v
        }
        None => json!({
            "ok": false,
            "configured": error.configured,
            "error": error.message,
        }),
    }
}

/* ---------- Kimi Code ---------- */

struct Credentials {
    access: String,
    refresh: String,
    expires_at: i64,
}

fn read_credentials(path: &Path) -> Option<Credentials> {
    let text = std::fs::read_to_string(path).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    let access = v.get("access_token")?.as_str()?.to_string();
    let refresh = v
        .get("refresh_token")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let expires_at = v.get("expires_at").and_then(value_i64).unwrap_or(0);
    Some(Credentials {
        access,
        refresh,
        expires_at,
    })
}

fn fetch_kimi() -> Result<Value, QuotaError> {
    let home = kimi_code_home().ok_or_else(|| QuotaError::new("找不到 Kimi 数据目录"))?;
    let path = home.join("credentials").join("kimi-code.json");
    let mut creds = read_credentials(&path)
        .ok_or_else(|| QuotaError::new("未找到 Kimi 登录凭据，请先运行 kimi login"))?;

    // 提前刷新：CLI 自己的请求也会刷新，这里只处理「文件里的 token 即将过期」
    if !creds.refresh.is_empty()
        && creds.expires_at > 0
        && creds.expires_at - now_secs() < REFRESH_MARGIN_SECS
    {
        if let Ok(access) = refresh_access(&path, &creds.refresh) {
            creds.access = access;
        }
    }

    let client = http_client().map_err(QuotaError::new)?;
    let mut resp = request_usages(&client, &creds.access).map_err(QuotaError::new)?;

    if is_auth_error(&resp) && !creds.refresh.is_empty() {
        // CLI 可能刚在别处刷新过，先重读文件；仍是 401 才自己刷新
        if let Some(fresh) = read_credentials(&path) {
            if fresh.access != creds.access {
                creds = fresh;
                resp = request_usages(&client, &creds.access).map_err(QuotaError::new)?;
            }
        }
        if is_auth_error(&resp) {
            creds.access = refresh_access(&path, &creds.refresh)?;
            resp = request_usages(&client, &creds.access).map_err(QuotaError::new)?;
        }
    }

    let status = resp.status();
    if !status.is_success() {
        return Err(QuotaError::new(format!(
            "Kimi 额度接口返回 HTTP {}",
            status.as_u16()
        )));
    }
    let body: Value = resp
        .json()
        .map_err(|e| QuotaError::new(format!("Kimi 额度响应解析失败：{e}")))?;
    parse_kimi_usage(&body).map_err(QuotaError::new)
}

fn request_usages(
    client: &reqwest::blocking::Client,
    access: &str,
) -> Result<reqwest::blocking::Response, String> {
    client
        .get(USAGES_API)
        .bearer_auth(access)
        .header("Accept", "application/json")
        .send()
        .map_err(|e| format!("Kimi 额度请求失败：{e}"))
}

fn is_auth_error(resp: &reqwest::blocking::Response) -> bool {
    matches!(resp.status().as_u16(), 401 | 403)
}

/// 用 CLI 的 refresh_token 换新 access_token；成功后原子写回凭据文件。
/// 写回前会核对 refresh_token 仍是本次使用的那一个，避免覆盖别的进程的刷新结果。
fn refresh_access(path: &Path, used_refresh: &str) -> Result<String, QuotaError> {
    let client = http_client().map_err(QuotaError::new)?;
    let resp = client
        .post(TOKEN_API)
        .form(&[
            ("client_id", CLIENT_ID),
            ("grant_type", "refresh_token"),
            ("refresh_token", used_refresh),
        ])
        .send()
        .map_err(|e| QuotaError::new(format!("Kimi 登录刷新失败：{e}")))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(QuotaError::new(format!(
            "Kimi 登录已过期（HTTP {}），请在设置中重新登录 CLI",
            status.as_u16()
        )));
    }
    let data: Value = resp
        .json()
        .map_err(|e| QuotaError::new(format!("Kimi 登录刷新响应解析失败：{e}")))?;
    let access = data
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or_else(|| QuotaError::new("Kimi 登录刷新响应缺少 access_token"))?
        .to_string();
    let new_refresh = data
        .get("refresh_token")
        .and_then(Value::as_str)
        .unwrap_or(used_refresh);
    let expires_in = data
        .get("expires_in")
        .and_then(value_i64)
        .unwrap_or(3600)
        .max(60);
    persist_refresh(
        path,
        used_refresh,
        &access,
        new_refresh,
        now_secs() + expires_in,
    );
    Ok(access)
}

fn persist_refresh(path: &Path, used_refresh: &str, access: &str, refresh: &str, expires_at: i64) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    let Ok(mut v) = serde_json::from_str::<Value>(&text) else {
        return;
    };
    if v.get("refresh_token").and_then(Value::as_str) != Some(used_refresh) {
        return;
    }
    v["access_token"] = Value::String(access.to_string());
    v["refresh_token"] = Value::String(refresh.to_string());
    v["expires_at"] = Value::from(expires_at);
    let Ok(text) = serde_json::to_string(&v) else {
        return;
    };
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "credentials.json".into());
    let tmp = path.with_file_name(format!("{file_name}.{}.tmp", std::process::id()));
    if write_private(&tmp, text.as_bytes()).is_err() {
        return;
    }
    // CLI 可能正短暂持有凭据文件；替换失败时重试几次，仍失败则清理临时文件
    for _ in 0..3 {
        if crate::settings::replace_file(&tmp, path).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = std::fs::remove_file(&tmp);
}

/// 凭据文件含密钥：Unix 下新建临时文件即 0600，Windows 继承目录 ACL
fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path)?;
    f.write_all(data)?;
    f.sync_all()
}

fn http_client() -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .user_agent("tauri-kimi-quota/1")
        .build()
        .map_err(|e| format!("HTTP 客户端初始化失败：{e}"))
}

/// 兼容两种响应形态：
/// - 新版 `usages.limit_5h / limit_7d`（snake_case，used_ratio 为 0..1）
/// - 旧版 `limits[]`（5h）+ `usage`（本周，used/limit 为整数字符串）
fn parse_kimi_usage(body: &Value) -> Result<Value, String> {
    let usages = body.get("usages");
    let five = usages
        .and_then(|u| u.get("limit_5h"))
        .or_else(|| usages.and_then(|u| u.get("limit5h")));
    let week = usages
        .and_then(|u| u.get("limit_7d"))
        .or_else(|| usages.and_then(|u| u.get("limit7d")));

    let mut five_pct = five.and_then(ratio_pct);
    let mut five_reset = five.and_then(reset_at);
    let mut week_pct = week.and_then(ratio_pct);
    let mut week_reset = week.and_then(reset_at);

    if five_pct.is_none() {
        if let Some(detail) = body
            .get("limits")
            .and_then(Value::as_array)
            .and_then(|arr| {
                arr.iter()
                    .find(|e| {
                        e.get("window")
                            .and_then(|w| w.get("duration"))
                            .and_then(value_i64)
                            == Some(300)
                    })
                    .and_then(|e| e.get("detail"))
            })
        {
            five_pct = ratio_pct(detail);
            five_reset = reset_at(detail);
        }
    }
    if week_pct.is_none() {
        if let Some(usage) = body.get("usage") {
            week_pct = ratio_pct(usage);
            week_reset = reset_at(usage);
        }
    }

    if five_pct.is_none() && week_pct.is_none() {
        return Err("Kimi 额度响应格式无法识别".into());
    }
    Ok(json!({
        "ok": true,
        "five_hour": {
            "used_pct": five_pct,
            "reset_at": five_reset,
            "window_ms": FIVE_HOUR_MS,
        },
        "weekly": {
            "used_pct": week_pct,
            "reset_at": week_reset,
            "window_ms": WEEK_MS,
        },
        "fetched_at": now_ms(),
    }))
}

/// 比例字段统一成 0..100 的百分数
fn ratio_pct(v: &Value) -> Option<f64> {
    if let Some(n) = v
        .get("used_ratio")
        .or_else(|| v.get("usedRatio"))
        .and_then(value_f64)
    {
        return Some((n * 100.0).clamp(0.0, 100.0));
    }
    if let Some(n) = v.get("percent").and_then(value_f64) {
        return Some(n.clamp(0.0, 100.0));
    }
    let used = v.get("used").and_then(value_f64)?;
    let limit = v.get("limit").and_then(value_f64).filter(|l| *l > 0.0)?;
    Some((used / limit * 100.0).clamp(0.0, 100.0))
}

fn reset_at(v: &Value) -> Option<i64> {
    ["reset_time", "resetTime", "reset_at", "resetAt"]
        .iter()
        .find_map(|k| v.get(*k).and_then(Value::as_str))
        .and_then(parse_rfc3339_ms)
}

/* ---------- OpenCode Go（seekfetch） ---------- */

fn fetch_opencode() -> Result<Value, QuotaError> {
    let bin = find_seekfetch();
    let out = match run_with_timeout(&bin, &["opencode", "usage", "--json"], SEEKFETCH_TIMEOUT) {
        Ok(out) => out,
        Err(e) => {
            let lower = e.to_lowercase();
            let missing_bin = e.contains("无法启动");
            let unconfigured = missing_bin
                || [
                    "credential",
                    "api key",
                    "api_key",
                    "not configured",
                    "no key",
                    "未配置",
                    "未设置",
                ]
                .iter()
                .any(|k| lower.contains(k));
            let message = if missing_bin {
                format!(
                    "未找到 seekfetch（{}），可在设置中关闭 OpenCode 额度",
                    bin.display()
                )
            } else {
                e
            };
            return Err(if unconfigured {
                QuotaError::unconfigured(message)
            } else {
                QuotaError::new(message)
            });
        }
    };
    let body: Value = serde_json::from_slice(&out)
        .map_err(|e| QuotaError::new(format!("seekfetch 输出解析失败：{e}")))?;
    parse_opencode_usage(&body).map_err(QuotaError::new)
}

fn parse_opencode_usage(body: &Value) -> Result<Value, String> {
    let usage = body.get("usage").unwrap_or(body);
    let mut out = Map::new();
    out.insert("ok".into(), Value::Bool(true));
    if let Some(v) = window_json(usage.get("rolling"), FIVE_HOUR_MS) {
        out.insert("rolling".into(), v);
    }
    if let Some(v) = window_json(usage.get("weekly"), WEEK_MS) {
        out.insert("weekly".into(), v);
    }
    if let Some(v) = window_json(usage.get("monthly"), MONTH_MS) {
        out.insert("monthly".into(), v);
    }
    if out.len() == 1 {
        return Err("seekfetch 未返回任何额度窗口".into());
    }
    out.insert("fetched_at".into(), Value::from(now_ms()));
    Ok(Value::Object(out))
}

/// 单个窗口：percent 已是 0..100；月窗口按 reset 回推一个日历月，
/// 让参考线在 28~31 天的月份里都落在正确位置
fn window_json(entry: Option<&Value>, window_ms: i64) -> Option<Value> {
    let entry = entry?;
    let pct = entry
        .get("percent")
        .and_then(value_f64)
        .map(|p| p.clamp(0.0, 100.0))?;
    let reset = ["resetsAt", "resets_at", "resetAt", "reset_at"]
        .iter()
        .find_map(|k| entry.get(*k).and_then(Value::as_str))
        .and_then(parse_rfc3339_ms);
    let actual_window = if window_ms == MONTH_MS {
        reset.map(month_window_ms).unwrap_or(MONTH_MS)
    } else {
        window_ms
    };
    Some(json!({
        "used_pct": pct,
        "reset_at": reset,
        "window_ms": actual_window,
    }))
}

fn find_seekfetch() -> PathBuf {
    if let Ok(p) = std::env::var("SEEKFETCH_BIN") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    let exe = if cfg!(windows) {
        "seekfetch.exe"
    } else {
        "seekfetch"
    };
    for var in ["USERPROFILE", "HOME"] {
        if let Ok(home) = std::env::var(var) {
            if home.is_empty() {
                continue;
            }
            let home = PathBuf::from(home);
            for candidate in [
                home.join(".local").join("bin").join(exe),
                home.join("bin").join(exe),
            ] {
                if candidate.is_file() {
                    return candidate;
                }
            }
        }
    }
    // 交给 PATH 解析；找不到时 spawn 会给出系统错误
    PathBuf::from(exe)
}

fn run_with_timeout(bin: &Path, args: &[&str], timeout: Duration) -> Result<Vec<u8>, String> {
    let mut cmd = Command::new(bin);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW：避免每次轮询闪一个控制台窗口
        cmd.creation_flags(0x0800_0000);
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("无法启动 {}：{e}", bin.display()))?;
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err("seekfetch 查询超时".into());
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                let _ = child.kill();
                return Err(format!("等待 seekfetch 失败：{e}"));
            }
        }
    }
    let out = child
        .wait_with_output()
        .map_err(|e| format!("读取 seekfetch 输出失败：{e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let first = stderr.lines().next().unwrap_or("").trim();
        return Err(if first.is_empty() {
            format!("seekfetch 退出码 {}", out.status.code().unwrap_or(-1))
        } else {
            first.to_string()
        });
    }
    Ok(out.stdout)
}

/* ---------- 时间工具 ---------- */

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn value_f64(v: &Value) -> Option<f64> {
    v.as_f64()
        .or_else(|| v.as_str().and_then(|s| s.trim().parse::<f64>().ok()))
}

fn value_i64(v: &Value) -> Option<i64> {
    v.as_i64()
        .or_else(|| v.as_u64().map(|n| n as i64))
        .or_else(|| v.as_str().and_then(|s| s.trim().parse::<i64>().ok()))
}

fn parse_rfc3339_ms(s: &str) -> Option<i64> {
    use time::{format_description::well_known::Rfc3339, OffsetDateTime};
    let dt = OffsetDateTime::parse(s.trim(), &Rfc3339).ok()?;
    Some((dt.unix_timestamp_nanos() / 1_000_000) as i64)
}

/// 月度窗口时长：从 reset 回推一个日历月，长度随月份变化
fn month_window_ms(reset_ms: i64) -> i64 {
    use time::{Date, OffsetDateTime};
    let Ok(dt) = OffsetDateTime::from_unix_timestamp(reset_ms / 1000) else {
        return MONTH_MS;
    };
    let (year, month) = previous_month(dt.year(), dt.month());
    let day = dt.day().min(days_in_month(year, month));
    let Ok(start_date) = Date::from_calendar_date(year, month, day) else {
        return MONTH_MS;
    };
    let start = start_date.with_time(dt.time()).assume_offset(dt.offset());
    (dt.unix_timestamp() - start.unix_timestamp()) * 1000
}

fn previous_month(year: i32, month: time::Month) -> (i32, time::Month) {
    use time::Month::*;
    match month {
        January => (year - 1, December),
        February => (year, January),
        March => (year, February),
        April => (year, March),
        May => (year, April),
        June => (year, May),
        July => (year, June),
        August => (year, July),
        September => (year, August),
        October => (year, September),
        November => (year, October),
        December => (year, November),
    }
}

fn days_in_month(year: i32, month: time::Month) -> u8 {
    use time::{Date, Month::*};
    let (ny, nm) = match month {
        January => (year, February),
        February => (year, March),
        March => (year, April),
        April => (year, May),
        May => (year, June),
        June => (year, July),
        July => (year, August),
        August => (year, September),
        September => (year, October),
        October => (year, November),
        November => (year, December),
        December => (year + 1, January),
    };
    let first = Date::from_calendar_date(year, month, 1).expect("valid month start");
    let next = Date::from_calendar_date(ny, nm, 1).expect("valid next month start");
    (next - first).whole_days() as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_new_usages_shape() {
        let body = json!({
            "usages": {
                "limit_5h": { "used_ratio": 0.4917, "reset_time": "2026-10-24T08:35:30Z" },
                "limit_7d": { "used_ratio": 0.1597, "reset_time": "2026-10-31T02:35:29Z" }
            }
        });
        let out = parse_kimi_usage(&body).unwrap();
        let five = out["five_hour"]["used_pct"].as_f64().unwrap();
        assert!((five - 49.17).abs() < 0.01);
        assert_eq!(out["five_hour"]["window_ms"], FIVE_HOUR_MS);
        let week = out["weekly"]["used_pct"].as_f64().unwrap();
        assert!((week - 15.97).abs() < 0.01);
        assert!(out["weekly"]["reset_at"].as_i64().unwrap() > 0);
    }

    #[test]
    fn parses_legacy_limits_shape() {
        let body = json!({
            "usage": { "limit": "100", "used": "16", "resetTime": "2026-10-31T02:35:30Z" },
            "limits": [{
                "window": { "duration": 300, "timeUnit": "TIME_UNIT_MINUTE" },
                "detail": { "limit": "100", "used": "49", "resetTime": "2026-10-24T08:35:30Z" }
            }]
        });
        let out = parse_kimi_usage(&body).unwrap();
        assert_eq!(out["five_hour"]["used_pct"], 49.0);
        assert_eq!(out["weekly"]["used_pct"], 16.0);
    }

    #[test]
    fn parses_opencode_payload() {
        let body = json!({
            "usage": {
                "rolling": { "status": "ok", "percent": 5.0, "resetsAt": "2026-10-24T08:41:23.000Z" },
                "weekly": { "status": "ok", "percent": 7.0, "resetsAt": "2026-10-26T00:00:00.000Z" },
                "monthly": { "status": "ok", "percent": 3.0, "resetsAt": "2026-11-23T05:35:12.000Z" }
            }
        });
        let out = parse_opencode_usage(&body).unwrap();
        assert_eq!(out["rolling"]["used_pct"], 5.0);
        assert_eq!(out["weekly"]["used_pct"], 7.0);
        assert_eq!(out["monthly"]["used_pct"], 3.0);
        // 10-23 → 11-23 是 31 天
        assert_eq!(out["monthly"]["window_ms"], 31i64 * 24 * 60 * 60 * 1000);
    }

    #[test]
    fn month_window_uses_calendar_month() {
        let feb = parse_rfc3339_ms("2026-03-01T00:00:00Z").unwrap();
        assert_eq!(month_window_ms(feb), 28 * 24 * 60 * 60 * 1000);
        let march = parse_rfc3339_ms("2026-04-01T00:00:00Z").unwrap();
        assert_eq!(month_window_ms(march), 31 * 24 * 60 * 60 * 1000);
    }

    #[test]
    fn unconfigured_error_is_marked_for_ui() {
        let v = stale_or_error(None, QuotaError::unconfigured("not installed"));
        assert_eq!(v["ok"], false);
        assert_eq!(v["configured"], false);
        let v = stale_or_error(None, QuotaError::new("network down"));
        assert_eq!(v["configured"], true);
    }

    #[test]
    #[ignore = "需要本机 Kimi 登录凭据，手动运行"]
    fn live_fetch_kimi() {
        let v = fetch_kimi().expect("Kimi 额度查询失败");
        assert_eq!(v["ok"], true);
        eprintln!("{}", serde_json::to_string_pretty(&v).unwrap());
    }

    #[test]
    #[ignore = "需要本机 seekfetch 与 OpenCode Go 凭据，手动运行"]
    fn live_fetch_opencode() {
        let v = fetch_opencode().expect("OpenCode 额度查询失败");
        assert_eq!(v["ok"], true);
        eprintln!("{}", serde_json::to_string_pretty(&v).unwrap());
    }
}
