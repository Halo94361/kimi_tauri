//! 远程控制隧道：本地界面就绪后经环回接口开启，失败只提示、不影响本地使用。
//!
//! 壳不再用 `kimi web --rc` 启动（CLI 会在打印本地 URL 前同步等待远端握手，
//! 断网时本地界面完全不可用），改为就绪后调
//! `POST /api/v1/remote-control {"enabled":true}` 在运行中开隧道。
//!
//! 关键约束：断网时该 POST 在 CLI 内部永不返回（客户端在后台无限退避重连），
//! 服务端状态机停在 `starting`，且 CLI 的 enable 是队列串行的，后续重试会
//! 一起挂住。唯一可靠的解锁手段是 `{"enabled":false}`：它同步打断 starting
//! 并释放内部锁。因此这里超时后必须主动取消，重试前也要先查状态。

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::Ordering;
use std::time::Duration;

use tauri::{AppHandle, Emitter, Manager};

use crate::state::{AppState, LocalEndpoint, RemotePhase, RemoteStatus};

/// 开启隧道的读超时：CLI 内部注册超时各 10s，这里留出余量
const ENABLE_TIMEOUT: Duration = Duration::from_secs(15);
/// 状态查询 / 取消请求的超时
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
/// 已连接后的巡检周期：CLI 自身会后台重连，但状态掉到 off 时应重新显示提示
const HEALTH_INTERVAL: Duration = Duration::from_secs(90);

#[tauri::command]
pub(crate) fn remote_status(state: tauri::State<AppState>) -> serde_json::Value {
    let status = state.remote.lock().unwrap();
    status_payload(&status)
}

/// 用户点「重试」：只重新调接口，不重启 kimi web、不打断当前会话
#[tauri::command]
pub(crate) fn retry_remote(app: AppHandle) {
    start_enable(&app);
}

/// 设置页手动断开：状态回到 Idle（巡检线程随之退出），后台发 disable 打断
/// CLI 的 starting/on；与「下次启动是否自动开启」的设置无关
#[tauri::command]
pub(crate) fn disconnect_remote(app: AppHandle) {
    let state = app.state::<AppState>();
    let gen = state.server_gen.load(Ordering::SeqCst);
    let endpoint = state.endpoint.lock().unwrap().clone();
    set_status(&app, gen, RemotePhase::Idle, String::new(), None);
    if let Some(ep) = endpoint {
        std::thread::spawn(move || {
            let _ = request(&ep, "POST", Some(r#"{"enabled":false}"#), REQUEST_TIMEOUT);
        });
    }
}

fn phase_name(phase: RemotePhase) -> &'static str {
    match phase {
        RemotePhase::Idle => "idle",
        RemotePhase::Connecting => "connecting",
        RemotePhase::Connected => "connected",
        RemotePhase::Failed => "failed",
    }
}

pub(crate) fn status_payload(status: &RemoteStatus) -> serde_json::Value {
    serde_json::json!({
        "state": phase_name(status.phase),
        "reason": status.reason,
        "code": status.code,
    })
}

/// 写回状态并通知选项卡栏；代次不匹配说明子进程已重启，旧结果直接丢弃
fn set_status(
    app: &AppHandle,
    gen: u64,
    phase: RemotePhase,
    reason: String,
    code: Option<i64>,
) {
    let status = {
        let state = app.state::<AppState>();
        let mut cur = state.remote.lock().unwrap();
        if cur.gen != gen {
            return;
        }
        cur.phase = phase;
        cur.reason = reason;
        cur.code = code;
        cur.clone()
    };
    let _ = app.emit_to("tabbar", "remote-status", status_payload(&status));
}

/// 新子进程启动时重置：旧代次的连接/失败提示全部失效
pub(crate) fn reset(app: &AppHandle, gen: u64) {
    {
        let state = app.state::<AppState>();
        state.endpoint.lock().unwrap().take();
        *state.remote.lock().unwrap() = RemoteStatus {
            gen,
            phase: RemotePhase::Idle,
            ..Default::default()
        };
    }
    let _ = app.emit_to(
        "tabbar",
        "remote-status",
        serde_json::json!({ "state": "idle", "reason": "" }),
    );
}

/// 子进程退出：清空端点，回到 Idle（错误页由 server 模块负责）
pub(crate) fn mark_idle(app: &AppHandle, gen: u64) {
    {
        let state = app.state::<AppState>();
        if state.server_gen.load(Ordering::SeqCst) != gen {
            return;
        }
        state.endpoint.lock().unwrap().take();
    }
    set_status(app, gen, RemotePhase::Idle, String::new(), None);
}

/// 启动（或重试）远程控制开启流程；正在连接或已连接时不重复入队
pub(crate) fn start_enable(app: &AppHandle) {
    let state = app.state::<AppState>();
    let gen = state.server_gen.load(Ordering::SeqCst);
    let endpoint = state.endpoint.lock().unwrap().clone();
    let Some(endpoint) = endpoint else {
        set_status(
            app,
            gen,
            RemotePhase::Failed,
            "本地服务尚未就绪，请稍后重试".into(),
            None,
        );
        return;
    };
    // 检查与置位放在同一把锁内，避免连点「重试」起两个 worker 并发 POST
    let status = {
        let mut cur = state.remote.lock().unwrap();
        if cur.gen != gen
            || matches!(cur.phase, RemotePhase::Connecting | RemotePhase::Connected)
        {
            return;
        }
        cur.phase = RemotePhase::Connecting;
        cur.reason.clear();
        cur.code = None;
        cur.clone()
    };
    let _ = app.emit_to("tabbar", "remote-status", status_payload(&status));
    let app = app.clone();
    std::thread::spawn(move || enable_worker(app, gen, endpoint));
}

fn enable_worker(app: AppHandle, gen: u64, ep: LocalEndpoint) {
    // 先查状态：上次超时可能把 CLI 留在 starting，直接 enable 会排队挂死
    if let Ok((200, body)) = request(&ep, "GET", None, REQUEST_TIMEOUT) {
        match state_of(&body).as_deref() {
            Some("on") => {
                set_status(&app, gen, RemotePhase::Connected, String::new(), None);
                spawn_health(app, gen, ep);
                return;
            }
            Some("starting") => {
                // disable 会同步打断 starting 并释放 CLI 内部锁，队列随之排空
                let _ = request(&ep, "POST", Some(r#"{"enabled":false}"#), REQUEST_TIMEOUT);
            }
            _ => {}
        }
    }

    match request(&ep, "POST", Some(r#"{"enabled":true}"#), ENABLE_TIMEOUT) {
        Ok((status, body)) => match map_response(status, &body) {
            Ok(()) => {
                set_status(&app, gen, RemotePhase::Connected, String::new(), None);
                spawn_health(app, gen, ep);
            }
            Err((reason, code)) => {
                set_status(&app, gen, RemotePhase::Failed, reason, code);
            }
        },
        Err(err) => {
            // 请求还挂在服务端（starting）：主动取消，否则后续重试全部排队
            let _ = request(&ep, "POST", Some(r#"{"enabled":false}"#), REQUEST_TIMEOUT);
            let reason = if err == "timeout" {
                "远程控制连接超时（网络不可用？）".to_string()
            } else {
                format!("远程控制连接失败：{err}")
            };
            set_status(&app, gen, RemotePhase::Failed, reason, None);
        }
    }
}

/// 已连接后的低频巡检：CLI 掉线后把提示条恢复出来
fn spawn_health(app: AppHandle, gen: u64, ep: LocalEndpoint) {
    std::thread::spawn(move || loop {
        std::thread::sleep(HEALTH_INTERVAL);
        {
            let state = app.state::<AppState>();
            let cur = state.remote.lock().unwrap();
            if cur.gen != gen || cur.phase != RemotePhase::Connected {
                return;
            }
        }
        // 只有明确读到 off 才判定断开；环回请求的偶发失败交给子进程生命周期处理
        if let Ok((200, body)) = request(&ep, "GET", None, REQUEST_TIMEOUT) {
            if state_of(&body).as_deref() == Some("off") {
                set_status(
                    &app,
                    gen,
                    RemotePhase::Failed,
                    "远程控制连接已断开".into(),
                    None,
                );
                return;
            }
        }
    });
}

/// 接口响应 → 结果。业务错误一律 HTTP 200，错误码在 body 的 code 字段
fn map_response(status: u16, body: &str) -> Result<(), (String, Option<i64>)> {
    if status == 401 {
        return Err(("本地令牌已失效，请重启应用".into(), None));
    }
    if status == 404 {
        return Err(("当前 kimi CLI 不支持远程控制接口".into(), None));
    }
    if !(200..300).contains(&status) {
        return Err((format!("本地接口异常 (HTTP {status})"), None));
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return Err(("无法解析接口响应".into(), None));
    };
    let Some(code) = value.get("code").and_then(|c| c.as_i64()) else {
        return Err(("无法解析接口响应".into(), None));
    };
    if code == 0 {
        return Ok(());
    }
    if code == 40928 {
        return Err(("另一实例已开启远程控制".into(), Some(code)));
    }
    let msg = value
        .get("msg")
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .to_string();
    let reason = if msg.is_empty() {
        format!("远程控制开启失败 (code {code})")
    } else {
        msg
    };
    Err((reason, Some(code)))
}

fn state_of(body: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    Some(value.get("data")?.get("state")?.as_str()?.to_string())
}

/// 环回明文 HTTP/1.1 客户端：固定 127.0.0.1（`localhost` 可能先解析到 ::1，
/// 而 kimi web 只监听 v4），`Connection: close` 让服务端响应后立即断开
fn request(
    ep: &LocalEndpoint,
    method: &str,
    body: Option<&str>,
    timeout: Duration,
) -> Result<(u16, String), String> {
    let addr = SocketAddr::from(([127, 0, 0, 1], ep.port));
    let mut stream =
        TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT).map_err(|e| e.to_string())?;
    let _ = stream.set_read_timeout(Some(timeout));
    let _ = stream.set_write_timeout(Some(timeout));

    let body = body.unwrap_or("");
    let mut req = format!(
        "{method} /api/v1/remote-control HTTP/1.1\r\n\
         Host: 127.0.0.1:{}\r\n\
         Authorization: Bearer {}\r\n\
         Accept: application/json\r\n\
         Connection: close\r\n",
        ep.port, ep.token
    );
    if method == "POST" {
        req.push_str("Content-Type: application/json\r\n");
        req.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    req.push_str("\r\n");
    req.push_str(body);

    stream.write_all(req.as_bytes()).map_err(|e| e.to_string())?;
    stream.flush().map_err(|e| e.to_string())?;
    read_response(&mut stream)
}

fn read_response(stream: &mut TcpStream) -> Result<(u16, String), String> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let mut head_end: Option<usize> = None;
    let mut content_len: Option<usize> = None;
    let mut chunked = false;

    loop {
        // 头解析完成后，body 按 Content-Length / chunked 终止符判断是否完整，
        // 不依赖 EOF（避免 keep-alive 时读到超时）
        if let Some(end) = head_end {
            let body = &buf[end..];
            let complete = if chunked {
                body.ends_with(b"0\r\n\r\n") || body.ends_with(b"0\r\n")
            } else if let Some(len) = content_len {
                body.len() >= len
            } else {
                false
            };
            if complete {
                break;
            }
        }
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if head_end.is_none() {
                    if let Some(pos) = find(&buf, b"\r\n\r\n") {
                        head_end = Some(pos + 4);
                        let head = String::from_utf8_lossy(&buf[..pos]);
                        for line in head.lines().skip(1) {
                            if let Some((key, value)) = line.split_once(':') {
                                let key = key.trim().to_ascii_lowercase();
                                let value = value.trim();
                                if key == "content-length" {
                                    content_len = value.parse().ok();
                                } else if key == "transfer-encoding"
                                    && value.eq_ignore_ascii_case("chunked")
                                {
                                    chunked = true;
                                }
                            }
                        }
                    }
                }
            }
            // Windows 上读超时表现为 WouldBlock 或 TimedOut，两者都要认
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                return Err("timeout".into())
            }
            Err(e) => return Err(e.to_string()),
        }
    }

    let Some(end) = head_end else {
        return Err("响应头不完整".into());
    };
    let head = String::from_utf8_lossy(&buf[..end - 4]);
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| "无法解析 HTTP 状态行".to_string())?;

    let raw_body = &buf[end..];
    let body = if chunked {
        decode_chunked(raw_body).unwrap_or_default()
    } else {
        match content_len {
            Some(len) if raw_body.len() > len => raw_body[..len].to_vec(),
            _ => raw_body.to_vec(),
        }
    };
    Ok((status, String::from_utf8_lossy(&body).into_owned()))
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn decode_chunked(mut data: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let line_end = find(data, b"\r\n")?;
        let size_field = std::str::from_utf8(&data[..line_end]).ok()?;
        let size = usize::from_str_radix(size_field.split(';').next()?.trim(), 16).ok()?;
        data = &data[line_end + 2..];
        if size == 0 {
            return Some(out);
        }
        if data.len() < size + 2 {
            return None;
        }
        out.extend_from_slice(&data[..size]);
        data = &data[size + 2..];
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    fn serve_once(response: String) -> u16 {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                let _ = stream.write_all(response.as_bytes());
            }
        });
        port
    }

    fn endpoint(port: u16) -> LocalEndpoint {
        LocalEndpoint {
            port,
            token: "test-token".into(),
        }
    }

    fn json_response(status: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
    }

    #[test]
    fn accepts_success_envelope() {
        let body = r#"{"code":0,"msg":"success","data":{"state":"on"},"request_id":"x"}"#;
        let port = serve_once(json_response("200 OK", body));
        assert_eq!(map_response(200, body), Ok(()));
        let (status, got) = request(&endpoint(port), "POST", Some("{}"), REQUEST_TIMEOUT).unwrap();
        assert_eq!(status, 200);
        assert_eq!(got, body);
    }

    #[test]
    fn maps_business_and_http_errors() {
        let busy = r#"{"code":40928,"msg":"another instance","data":null}"#;
        assert_eq!(
            map_response(200, busy),
            Err(("另一实例已开启远程控制".into(), Some(40928)))
        );
        assert_eq!(
            map_response(401, ""),
            Err(("本地令牌已失效，请重启应用".into(), None))
        );
        assert_eq!(
            map_response(404, ""),
            Err(("当前 kimi CLI 不支持远程控制接口".into(), None))
        );
        assert_eq!(
            map_response(200, "not json"),
            Err(("无法解析接口响应".into(), None))
        );
    }

    #[test]
    fn decodes_chunked_body() {
        let chunked = "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n\
                       5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
        let port = serve_once(chunked.into());
        let (status, body) = request(&endpoint(port), "GET", None, REQUEST_TIMEOUT).unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, "hello world");
    }

    #[test]
    fn reports_timeout_without_hanging() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            // 接受连接后不响应，模拟断网时 CLI 内部永不返回的 enable
            if let Ok((_stream, _)) = listener.accept() {
                std::thread::sleep(Duration::from_secs(3));
            }
        });
        let started = std::time::Instant::now();
        let err = request(
            &endpoint(port),
            "POST",
            Some("{}"),
            Duration::from_millis(300),
        )
        .unwrap_err();
        assert_eq!(err, "timeout");
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}
