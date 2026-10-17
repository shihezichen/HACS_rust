//! `a2a_call` —— A2A 客户端（对应 Python `node/l1_transport/a2a/a2a_call.py`）。
//!
//! 子命令：`healthz` / `agents` / `card <agent>` / `call <agent> --text ...`。
//! 身份靠 token（环境变量 A2A_TOKEN 优先，其次 --token-file）。
//! 退出码：0 成功；1 服务端非 200；2 参数/网络错误。

use std::time::Duration;

use hacs_l1_transport::team_tools::humanize_a2a_body;
use serde_json::json;

const DEFAULT_TIMEOUT: f64 = 300.0;

fn main() {
    // 公共参数可在任意位置（与 Python split_common 同口径，支持 --opt=value）
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut base: Option<String> = None;
    let mut token_file: Option<String> = None;
    let mut timeout = DEFAULT_TIMEOUT;
    let mut json_out = false;
    let mut rest: Vec<String> = Vec::new();
    let common = ["--base", "--token-file", "--timeout", "--json"];
    let mut i = 0;
    while i < argv.len() {
        let arg = &argv[i];
        let (name, inline) = match arg.split_once('=') {
            Some((n, v)) => (n.to_string(), Some(v.to_string())),
            None => (arg.clone(), None),
        };
        if common.contains(&name.as_str()) {
            match name.as_str() {
                "--json" => json_out = true,
                _ => {
                    let value = match inline {
                        Some(v) => v,
                        None => {
                            i += 1;
                            match argv.get(i) {
                                Some(v) => v.clone(),
                                None => {
                                    eprintln!("缺少参数值: {name}");
                                    std::process::exit(2);
                                }
                            }
                        }
                    };
                    match name.as_str() {
                        "--base" => base = Some(value),
                        "--token-file" => token_file = Some(value),
                        "--timeout" => match value.parse() {
                            Ok(v) => timeout = v,
                            Err(_) => {
                                eprintln!("--timeout 不是数字: {value}");
                                std::process::exit(2);
                            }
                        },
                        _ => {}
                    }
                }
            }
            i += 1;
            continue;
        }
        rest.push(arg.clone());
        i += 1;
    }

    if rest.is_empty() {
        eprintln!(
            "用法: a2a_call <healthz|agents|card <agent>|call <agent> --text ...> --base URL"
        );
        std::process::exit(2);
    }
    let cmd = rest[0].clone();
    let mut agent: Option<String> = None;
    let mut text: Option<String> = None;
    let mut session: Option<String> = None;
    let mut j = 1;
    while j < rest.len() {
        match rest[j].as_str() {
            "--text" => {
                j += 1;
                text = rest.get(j).cloned();
            }
            "--session" => {
                j += 1;
                session = rest.get(j).cloned();
            }
            other if agent.is_none() => agent = Some(other.to_string()),
            other => {
                eprintln!("未知参数: {other}");
                std::process::exit(2);
            }
        }
        j += 1;
    }

    let Some(base) = base else {
        eprintln!("必须提供 --base（例如 --base http://127.0.0.1:9920）");
        std::process::exit(2);
    };
    let base = base.trim_end_matches('/').to_string();

    let (method, url, needs_token, body): (&str, String, bool, Option<serde_json::Value>) =
        match cmd.as_str() {
            "healthz" => ("GET", format!("{base}/healthz"), false, None),
            "agents" => ("GET", format!("{base}/agents"), true, None),
            "card" => (
                "GET",
                format!(
                    "{base}/a2a/{}/.well-known/agent-card.json",
                    agent.unwrap_or_default()
                ),
                false,
                None,
            ),
            "call" => {
                let Some(t) = text else {
                    eprintln!("call 需要 --text");
                    std::process::exit(2);
                };
                let mut b = json!({"text": t, "session_id": session});
                b["timeout_s"] = json!(timeout);
                (
                    "POST",
                    format!("{base}/a2a/{}/messages", agent.unwrap_or_default()),
                    true,
                    Some(b),
                )
            }
            other => {
                eprintln!("未知子命令: {other}");
                std::process::exit(2);
            }
        };

    let token = token_from(&token_file);
    if needs_token && token.is_empty() {
        eprintln!("需要 token：--token-file 或环境变量 A2A_TOKEN");
        std::process::exit(2);
    }

    let result = match method {
        "GET" => http_get(&url, &token, timeout),
        _ => http_post(&url, &token, &body.unwrap().to_string(), timeout),
    };
    let (code, text_out) = match result {
        Ok(v) => v,
        Err(e) => {
            eprintln!("请求失败: {e}");
            std::process::exit(2);
        }
    };

    if json_out {
        println!("{text_out}");
    } else {
        println!("{}", humanize_a2a_body(&text_out));
    }
    if code != 200 {
        eprintln!("HTTP {code}");
        std::process::exit(1);
    }
}

fn token_from(file: &Option<String>) -> String {
    if let Ok(t) = std::env::var("A2A_TOKEN") {
        let t = t.trim().to_string();
        if !t.is_empty() {
            return t;
        }
    }
    file.as_deref()
        .and_then(|f| std::fs::read_to_string(f).ok())
        .map(|t| t.trim().to_string())
        .unwrap_or_default()
}

fn http_get(url: &str, token: &str, timeout: f64) -> Result<(u16, String), String> {
    let mut headers: Vec<(&str, &str)> = Vec::new();
    let auth = format!("Bearer {token}");
    if !token.is_empty() {
        headers.push(("Authorization", &auth));
    }
    let resp = raw_request("GET", url, &headers, None, timeout)?;
    Ok((resp.0, String::from_utf8_lossy(&resp.1).to_string()))
}

fn http_post(url: &str, token: &str, body: &str, timeout: f64) -> Result<(u16, String), String> {
    let mut headers: Vec<(&str, &str)> = vec![("Content-Type", "application/json")];
    let auth = format!("Bearer {token}");
    if !token.is_empty() {
        headers.push(("Authorization", &auth));
    }
    let resp = raw_request("POST", url, &headers, Some(body.as_bytes()), timeout)?;
    Ok((resp.0, String::from_utf8_lossy(&resp.1).to_string()))
}

/// HTTP/1.0 短连接请求（与 teamd bin 的 http_post 同构；urllib 的对应物）。
fn raw_request(
    method: &str,
    url: &str,
    headers: &[(&str, &str)],
    body: Option<&[u8]>,
    timeout: f64,
) -> Result<(u16, Vec<u8>), String> {
    use std::io::{Read, Write};
    use std::net::TcpStream;

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
    let dur = Duration::from_secs_f64(timeout.max(0.1));
    let addr = (host.as_str(), port)
        .to_socket_addrs()
        .map_err(|e| format!("解析地址失败: {e}"))?
        .next()
        .ok_or("无可用地址")?;
    let mut sock = TcpStream::connect_timeout(&addr, dur).map_err(|e| format!("连不上: {e}"))?;
    sock.set_read_timeout(Some(dur)).ok();
    sock.set_write_timeout(Some(dur)).ok();
    let mut req = Vec::new();
    req.extend_from_slice(format!("{method} {path} HTTP/1.0\r\n").as_bytes());
    req.extend_from_slice(format!("Host: {host}:{port}\r\n").as_bytes());
    req.extend_from_slice(b"Connection: close\r\n");
    if let Some(b) = body {
        req.extend_from_slice(format!("Content-Length: {}\r\n", b.len()).as_bytes());
    }
    for (k, v) in headers {
        req.extend_from_slice(format!("{k}: {v}\r\n").as_bytes());
    }
    req.extend_from_slice(b"\r\n");
    if let Some(b) = body {
        req.extend_from_slice(b);
    }
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

use std::net::ToSocketAddrs;
