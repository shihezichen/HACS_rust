//! `task_svc` 的回帖出站通道：把消息以 agent 身份发进群（对应 Python
//! `task_svc.py: chat_post_message`）。单独成模块：域层 `Store` 经它触网。

use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

/// 把一条消息以 agent_id 的身份发进 session_id 群。
///
/// 通道（读源码得到，非猜测）：POST {CHAT_BASE}/api/sessions/{sid}/messages
///   * 头 X-Agent-Id:<成员id>（只在 tailnet 来源被接受，回环 127.0.0.1 不算 tailnet）
///   * 体 {"text": ..., "mentions": [...]}；Agent 发言必须 @ 至少一个目标
///
/// 返回 (ok, info)；失败一律带回可读的 error，调用方负责落库。
pub fn chat_post_message(
    session_id: &str,
    agent_id: &str,
    text: &str,
    base: &str,
    timeout: Option<Duration>,
) -> (bool, Value) {
    let base = base.trim_end_matches('/');
    let url = format!("{base}/api/sessions/{session_id}/messages");
    let payload = json!({"text": text, "mentions": ["@all"]}).to_string();
    let headers = [
        ("Content-Type", "application/json; charset=utf-8"),
        ("Accept", "application/json"),
        ("X-Agent-Id", agent_id),
    ];
    // 显式禁代理：本机/容器里的 http_proxy 会让 tailnet 直连莫名 502
    // （Rust 直连 TCP，天然无代理；口径注释保留对齐）。
    match http_post_json(
        &url,
        &headers,
        payload.as_bytes(),
        timeout.unwrap_or(Duration::from_secs(8)),
    ) {
        Ok((status, raw)) => {
            let data: Value = serde_json::from_slice(&raw).unwrap_or(Value::Null);
            if data.get("ok").and_then(|v| v.as_bool()) == Some(false) {
                return (
                    false,
                    json!({"error": trunc300(&serde_json::to_string(&data).unwrap_or_default())}),
                );
            }
            let msg = data.get("message").cloned().unwrap_or(Value::Null);
            (
                true,
                json!({
                    "message_id": msg.get("message_id"),
                    "message_seq": msg.get("seq"),
                    "http_status": status,
                }),
            )
        }
        Err(e) => (false, json!({"error": trunc300(&e)})),
    }
}

fn trunc300(s: &str) -> String {
    s.chars().take(300).collect()
}

/// 一次同步 HTTP POST（HTTP/1.0 短连接；与 a2a_svc/teamd 同构）。
fn http_post_json(
    url: &str,
    headers: &[(&str, &str)],
    body: &[u8],
    timeout: Duration,
) -> Result<(u16, Vec<u8>), String> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| format!("只支持 http:// 前缀: {url}"))?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (
            h.to_string(),
            p.parse::<u16>().map_err(|_| format!("端口不是数字: {p}"))?,
        ),
        None => (authority.to_string(), 80),
    };
    let addr = (host.as_str(), port)
        .to_socket_addrs()
        .map_err(|e| format!("解析地址失败: {e}"))?
        .next()
        .ok_or("无可用地址")?;
    let mut sock =
        TcpStream::connect_timeout(&addr, timeout).map_err(|e| format!("连不上: {e}"))?;
    sock.set_read_timeout(Some(timeout)).ok();
    sock.set_write_timeout(Some(timeout)).ok();
    let mut req = Vec::new();
    req.extend_from_slice(format!("POST {path} HTTP/1.0\r\n").as_bytes());
    req.extend_from_slice(format!("Host: {host}:{port}\r\n").as_bytes());
    req.extend_from_slice(format!("Content-Length: {}\r\n", body.len()).as_bytes());
    req.extend_from_slice(b"Connection: close\r\n");
    for (k, v) in headers {
        req.extend_from_slice(format!("{k}: {v}\r\n").as_bytes());
    }
    req.extend_from_slice(b"\r\n");
    req.extend_from_slice(body);
    sock.write_all(&req).map_err(|e| format!("发送失败: {e}"))?;
    sock.flush().ok();
    let mut raw = Vec::new();
    sock.read_to_end(&mut raw)
        .map_err(|e| format!("读取失败: {e}"))?;
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or("响应头不完整")?;
    let head = String::from_utf8_lossy(&raw[..split]);
    let status = head
        .lines()
        .next()
        .and_then(|l| l.split(' ').nth(1))
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or("状态行看不懂")?;
    Ok((status, raw[split + 4..].to_vec()))
}
