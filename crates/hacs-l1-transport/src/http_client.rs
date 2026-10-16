//! L1 通信层：极简 HTTP/1.0 **客户端**（传输不含语义）。
//!
//! 存在的理由：L2 的上下文水位在「本机读不到」时要回退打本节点 a2a 的 `/ctx-usage`，
//! 那是一次同机同源的简单 GET。为此引一个 HTTP 客户端 crate 不值当 —— 手写 60 行，
//! 零依赖，且超时可控（远端回退的护栏要求「单次代价可控」）。
//!
//! 只支持 `http://`（协作域内网面）；不跟重定向；读满即关（`Connection: close`）。

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

/// 一次 HTTP 响应。
#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HttpResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// HTTP 客户端错误（连不上 / 超时 / 协议不认）。
#[derive(Debug, Clone)]
pub struct HttpError(pub String);

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for HttpError {}

/// 发一次 `GET`，返回响应。`headers` 是追加的请求头（如 `Authorization`）。
///
/// `timeout` 同时用作「连接超时」与「整次读写超时」——上层的远端回退护栏就靠它把
/// 单次代价钉住（一次网络失败后由熔断器接管，不对同一个死对端反复砸）。
pub fn get(
    url: &str,
    headers: &[(&str, &str)],
    timeout: Duration,
) -> Result<HttpResponse, HttpError> {
    let parts = parse_url(url)?;
    let addr = (parts.host.as_str(), parts.port)
        .to_socket_addrs()
        .map_err(|e| HttpError(format!("解析地址失败: {e}")))?
        .next()
        .ok_or_else(|| HttpError("无可用地址".to_string()))?;
    let mut sock = TcpStream::connect_timeout(&addr, timeout)
        .map_err(|e| HttpError(format!("连不上 {}:{} —— {e}", parts.host, parts.port)))?;
    sock.set_read_timeout(Some(timeout)).ok();
    sock.set_write_timeout(Some(timeout)).ok();

    let mut req = Vec::new();
    req.extend_from_slice(format!("GET {} HTTP/1.0\r\n", parts.path).as_bytes());
    req.extend_from_slice(format!("Host: {}:{}\r\n", parts.host, parts.port).as_bytes());
    req.extend_from_slice(b"Connection: close\r\n");
    for (k, v) in headers {
        req.extend_from_slice(format!("{k}: {v}\r\n").as_bytes());
    }
    req.extend_from_slice(b"\r\n");
    sock.write_all(&req)
        .map_err(|e| HttpError(format!("发送失败: {e}")))?;
    sock.flush()
        .map_err(|e| HttpError(format!("发送失败: {e}")))?;

    let mut raw = Vec::new();
    sock.read_to_end(&mut raw)
        .map_err(|e| HttpError(format!("读取失败: {e}")))?;
    parse_response(&raw)
}

/// 发一次 `GET` 并把响应体当 JSON 解析。
pub fn get_json(
    url: &str,
    headers: &[(&str, &str)],
    timeout: Duration,
) -> Result<(u16, serde_json::Value), HttpError> {
    let resp = get(url, headers, timeout)?;
    let value: serde_json::Value =
        serde_json::from_slice(&resp.body).map_err(|e| HttpError(format!("响应不是 JSON: {e}")))?;
    Ok((resp.status, value))
}

struct UrlParts {
    host: String,
    port: u16,
    path: String,
}

fn parse_url(url: &str) -> Result<UrlParts, HttpError> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| HttpError(format!("只支持 http:// 前缀: {url}")))?;
    let (authority, path) = match rest.find('/') {
        Some(idx) => (&rest[..idx], &rest[idx..]),
        None => (rest, "/"),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (
            h.to_string(),
            p.parse::<u16>()
                .map_err(|_| HttpError(format!("端口不是数字: {p}")))?,
        ),
        None => (authority.to_string(), 80),
    };
    if host.is_empty() {
        return Err(HttpError(format!("URL 缺主机名: {url}")));
    }
    Ok(UrlParts {
        host,
        port,
        path: path.to_string(),
    })
}

fn parse_response(raw: &[u8]) -> Result<HttpResponse, HttpError> {
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| HttpError("响应头不完整".to_string()))?;
    let head = String::from_utf8_lossy(&raw[..split]).to_string();
    let body = raw[split + 4..].to_vec();
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or("");
    let status = status_line
        .split(' ')
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| HttpError(format!("状态行看不懂: {status_line:?}")))?;
    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    Ok(HttpResponse {
        status,
        headers,
        body,
    })
}

/// `urllib.parse.quote(s, safe="")` 的对应物：除 `A-Za-z0-9_.-~` 外一律百分号编码（UTF-8 逐字节）。
pub fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        let c = *b as char;
        if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-' | '~') {
            out.push(c);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}
