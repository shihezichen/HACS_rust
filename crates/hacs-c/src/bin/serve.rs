//! `serve` —— 对话台前端本地预览服务（对应 Python hub/l4_experience/serve.py，仅开发用）。
//!
//! 静态文件 + `/api/*`、`/healthz`、`/login`、`/account` 反代到 chat 服务。
//! 用法：serve --port 8080 --bind 127.0.0.1 --backend http://127.0.0.1:9980 --cookie <c>

use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::Duration;

struct Opts {
    port: u16,
    bind: String,
    backend: String,
    cookie: String,
    root: String,
}

fn parse_opts() -> Opts {
    let mut o = Opts {
        port: 8080,
        bind: "127.0.0.1".into(),
        backend: "http://127.0.0.1:9980".into(),
        cookie: String::new(),
        root: std::env::var("SERVE_ROOT").unwrap_or_else(|_| "ui".into()),
    };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--port" => o.port = it.next().and_then(|v| v.parse().ok()).unwrap_or(8080),
            "--bind" => o.bind = it.next().unwrap_or(o.bind),
            "--backend" => o.backend = it.next().unwrap_or(o.backend),
            "--cookie" => o.cookie = it.next().unwrap_or_default(),
            "--root" => o.root = it.next().unwrap_or(o.root),
            _ => {}
        }
    }
    o.backend = o.backend.trim_end_matches('/').to_string();
    o
}

fn content_type(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("") {
        "html" | "htm" => "text/html; charset=utf-8",
        "js" => "application/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        _ => "application/octet-stream",
    }
}

fn is_proxied(path: &str) -> bool {
    path.starts_with("/api/")
        || path.starts_with("/healthz")
        || path.starts_with("/login")
        || path.starts_with("/account")
}

/// 手写 HTTP/1.1 转发：读原始请求头/体 → 用 http_client 转发 → 原样回写。
fn proxy(
    opts: &Opts,
    method: &str,
    path: &str,
    headers: &[(String, String)],
    body: Option<&[u8]>,
) -> (u16, Vec<(String, String)>, Vec<u8>) {
    let url = format!("{}{}", opts.backend, path);
    let mut fwd_headers: Vec<(&str, &str)> = Vec::new();
    let mut content_type = "application/json";
    for (k, v) in headers {
        let kl = k.to_ascii_lowercase();
        if kl == "content-type" {
            content_type = Box::leak(v.clone().into_boxed_str());
            continue;
        }
        if kl == "accept" || kl == "last-event-id" {
            // 静态字符串泄漏仅开发工具可接受？改用值入 args 结构 → 简化：这两个头忽略。
            continue;
        }
    }
    if !opts.cookie.is_empty() {
        fwd_headers.push(("Cookie", &opts.cookie));
    }
    // request_json 只支持 JSON 体；GET/DELETE 走它也行（body=None）
    let body_value = body.and_then(|b| serde_json::from_slice::<serde_json::Value>(b).ok());
    let timeout = Duration::from_secs(30);
    let result = hacs_l1_transport::http_client::request_json(
        &url,
        method,
        body_value.as_ref(),
        &fwd_headers,
        timeout,
    );
    match result {
        Ok((code, value)) => {
            let text = serde_json::to_vec(&value).unwrap_or_default();
            (
                code,
                vec![(
                    "Content-Type".into(),
                    "application/json; charset=utf-8".into(),
                )],
                text,
            )
        }
        Err(e) => {
            let msg = format!(
                "{{\"ok\":false,\"error\":{{\"code\":\"proxy_error\",\"message\":\"{}\"}}}}",
                e.0.replace('"', "'")
            );
            (502, vec![], msg.into_bytes())
        }
    }
}

fn handle(mut stream: std::net::TcpStream, opts: &Opts) {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 8192];
    // 读到头结束
    let header_end = loop {
        match stream.read(&mut tmp) {
            Ok(0) => break buf.len(),
            Ok(n) => {
                buf.extend_from_slice(&tmp[..n]);
                if let Some(p) = find_header_end(&buf) {
                    break p;
                }
                if buf.len() > 1 << 20 {
                    break buf.len();
                }
            }
            Err(_) => break buf.len(),
        }
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut lines = head.lines();
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("GET").to_string();
    let full_path = parts.next().unwrap_or("/").to_string();
    let path = full_path.split('?').next().unwrap_or("/").to_string();
    let mut headers: Vec<(String, String)> = Vec::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    let content_length: usize = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    let mut body = buf[header_end..].to_vec();
    while body.len() < content_length {
        match stream.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => body.extend_from_slice(&tmp[..n]),
            Err(_) => break,
        }
    }

    let (code, resp_headers, data) = if is_proxied(&path) {
        proxy(
            opts,
            &method,
            &full_path,
            &headers,
            if content_length > 0 {
                Some(&body)
            } else {
                None
            },
        )
    } else if method == "GET" || method == "HEAD" {
        serve_static(opts, &path)
    } else {
        (
            404,
            vec![("Content-Type".into(), "text/plain".into())],
            b"not found".to_vec(),
        )
    };

    let mut resp = format!("HTTP/1.1 {code} {}\r\n", reason(code));
    let mut has_ct = false;
    for (k, v) in &resp_headers {
        if k.eq_ignore_ascii_case("content-type") {
            has_ct = true;
        }
        resp.push_str(&format!("{k}: {v}\r\n"));
    }
    if !has_ct {
        resp.push_str("Content-Type: application/octet-stream\r\n");
    }
    resp.push_str(&format!("Content-Length: {}\r\n", data.len()));
    resp.push_str("Connection: close\r\n\r\n");
    let _ = stream.write_all(resp.as_bytes());
    if method != "HEAD" {
        let _ = stream.write_all(&data);
    }
}

fn reason(code: u16) -> &'static str {
    match code {
        200 => "OK",
        404 => "Not Found",
        502 => "Bad Gateway",
        _ => "OK",
    }
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4)
}

fn serve_static(opts: &Opts, path: &str) -> (u16, Vec<(String, String)>, Vec<u8>) {
    let rel = path.trim_start_matches('/');
    let rel = if rel.is_empty() { "index.html" } else { rel };
    let full = std::path::Path::new(&opts.root).join(rel);
    match std::fs::read(&full) {
        Ok(data) => {
            let ct = content_type(rel);
            (200, vec![("Content-Type".into(), ct.into())], data)
        }
        Err(_) => (
            404,
            vec![("Content-Type".into(), "text/plain".into())],
            b"not found".to_vec(),
        ),
    }
}

fn main() {
    let opts = parse_opts();
    let listener = match TcpListener::bind((opts.bind.as_str(), opts.port)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[ui] 绑定 {}:{} 失败：{e}", opts.bind, opts.port);
            std::process::exit(1);
        }
    };
    println!("[ui] 前端预览: http://{}:{}/", opts.bind, opts.port);
    println!("[ui] /api/* -> {}", opts.backend);
    let shared = std::sync::Arc::new(opts);
    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                let shared = shared.clone();
                std::thread::spawn(move || handle(s, &shared));
            }
            Err(_) => continue,
        }
    }
}
