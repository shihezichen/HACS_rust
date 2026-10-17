//! `nodelet` —— L0 轻量节点守护组合根（对应 Python nodelet.py 的 HTTP 面 + main）。
//!
//! 只读探针口 `GET /probe`（9971）；`--once` 打一份探针 JSON 就退；
//! `--node/--bind/--services` 只把值写回本进程 env，输入通道仍只有 env。
//!
//! L0 不越层：HTTP 面用 std::TcpListener 自搭（不用 L1 web_gateway），与 Python 用
//! 裸 http.server 同一纪律。响应形态对齐 Python：200/404/405/500 + no-store。
#![allow(clippy::too_many_arguments)]

use hacs_l0_host::node_profile as np;
use hacs_l0_host::nodelet::*;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    let mut node_opt = String::new();
    let mut bind_opt = String::new();
    let mut services_opt = String::new();
    let mut once = false;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--node" => node_opt = args.next().unwrap_or_default(),
            "--bind" => bind_opt = args.next().unwrap_or_default(),
            "--services" => services_opt = args.next().unwrap_or_default(),
            "--once" => once = true,
            other => {
                eprintln!("不认识的参数：{}", other);
                std::process::exit(2);
            }
        }
    }
    if !node_opt.is_empty() {
        std::env::set_var(np::ENV_NODE_NAME, &node_opt);
    }
    if !bind_opt.is_empty() {
        std::env::set_var(ENV_BIND, &bind_opt);
    }
    if !services_opt.is_empty() {
        std::env::set_var(ENV_SERVICES, &services_opt);
    }
    if once {
        let now = now_epoch();
        println!(
            "{}",
            build_probe(Some(now), now, &real_run, None, None, None)
        );
        std::process::exit(0);
    }
    let (host, port) = match parse_bind(None) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[nodelet] 绑定面不合法：{}", e);
            std::process::exit(2);
        }
    };
    let listener = match TcpListener::bind((host.as_str(), port)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[nodelet] 起不来（端口被占或地址不可绑）：{}", e);
            std::process::exit(3);
        }
    };
    let started = now_epoch();
    eprintln!(
        "[nodelet] 探针口 http://{}:{}{}（只读）",
        host, port, PROBE_PATH
    );
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue };
        std::thread::spawn(move || {
            let _ = handle_conn(&mut stream, started);
        });
    }
}

fn now_epoch() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// 真单元查询执行器（只读：systemctl is-active / schtasks /Query）。
fn real_run(argv: &[String]) -> Option<(i32, String)> {
    let out = std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .output()
        .ok()?;
    // 先 utf-8，再 latin-1（不抛；中文 Windows GBK 场景 Rust 侧退 lossy）
    let text = String::from_utf8(out.stdout)
        .unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned());
    Some((out.status.code().unwrap_or(-1), text))
}

fn handle_conn(stream: &mut std::net::TcpStream, started_at: f64) -> std::io::Result<()> {
    let mut buf = [0u8; 8192];
    let n = stream.read(&mut buf)?;
    let req = String::from_utf8_lossy(&buf[..n]);
    let first = req.lines().next().unwrap_or("");
    let mut parts = first.split_whitespace();
    let method = parts.next().unwrap_or("");
    let path_q = parts.next().unwrap_or("");
    let path = path_q.split('?').next().unwrap_or("");
    let mut send = |code: u16, body: &str| -> std::io::Result<()> {
        write_resp(stream, code, body.as_bytes())
    };
    match method {
        "GET" => {
            if path != PROBE_PATH {
                return send(404, "{\"ok\":false,\"why\":\"no such path\"}");
            }
            match std::panic::catch_unwind(|| {
                let now = now_epoch();
                build_probe(Some(started_at), now, &real_run, None, None, None).to_string()
            }) {
                Ok(body) => send(200, &body),
                Err(_) => send(500, "{\"ok\":false,\"why\":\"String\"}"),
            }
        }
        "POST" | "PUT" | "DELETE" | "PATCH" => send(405, "{\"ok\":false,\"why\":\"read only\"}"),
        _ => send(400, "{\"ok\":false,\"why\":\"bad request\"}"),
    }
}

fn write_resp(stream: &mut std::net::TcpStream, code: u16, body: &[u8]) -> std::io::Result<()> {
    let reason = match code {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        500 => "Internal Server Error",
        _ => "OK",
    };
    let head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: application/json; charset=utf-8\r\n\
Content-Length: {}\r\nCache-Control: no-store\r\nServer: nodelet\r\nConnection: close\r\n\r\n",
        code,
        reason,
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}
