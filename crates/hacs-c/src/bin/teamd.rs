//! `teamd` —— **团队 NATS 客户端守护**（对应 Python `node/l1_transport/teamd.py`）。
//!
//! 用法（与 Python 版 CLI 同形）：
//! ```text
//! teamd --member lobster --member openclaw-nb \
//!       [--user lobster=nats-lobster ...] [--password-file lobster=/path/x.pass ...] \
//!       [--nats-url nats://127.0.0.1:4222] [--a2a-url http://127.0.0.1:9920] \
//!       [--mgmt-port 9960] [--workers 8] [--per-member-limit 2] [--queue-size 64]
//! ```
//!
//! 装配结构：
//!   * 每个成员一条独立 NATS 连接 + 独立凭据（凭据解析失败 → FATAL 退出 2）；
//!   * 订阅 `a2a.v1.req.<member>`，reader 回调只「入队」，worker 池消费并回调本机
//!     A2A（HTTP `POST /message` + `Authorization: Bearer <teamd token>`）；
//!   * 每 30s 心跳 `team.hb.<member>`；退避 1→2→5→10→30s 重连；60s PING 探活；
//!   * 127.0.0.1:9960 只读管理面：`/healthz` `/varz`。
//!
//! 本文件只做装配；纯逻辑在 `hacs_l1_transport::teamd`。

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use hacs_l1_transport::http_client as hcli;
use hacs_l1_transport::nats_client as nats;
use hacs_l1_transport::teamd as td;
use serde_json::{json, Value};

/// busy 回复：用一条短命连接直接回（reader 回调内不能久留，但 publish 一次很快）。
fn busy_reply(_member: &str, reply_to: &str, env: &Value) {
    // 复用进程级惰性连接：这里简单起见每次新建短命连接（busy 是低频路径）
    let url = std::env::var("AGENT_TEAM_NATS_URL")
        .unwrap_or_else(|_| "nats://127.0.0.1:4222".to_string());
    let conn = nats::NatsClient::new(
        nats::NatsConfig::from_hosts_str(&url, 4222)
            .unwrap_or_else(|_| nats::NatsConfig::from_hosts_str("127.0.0.1", 4222).unwrap()),
    );
    if conn.connect().is_ok() {
        let _ = conn.publish(reply_to, env.to_string().as_bytes(), "");
        conn.close();
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opts = parse_args(&args);
    if let Err(msgs) = opts {
        for m in msgs {
            eprintln!("{m}");
        }
        std::process::exit(2);
    }
    let opts = opts.unwrap();

    println!(
        "{}",
        json!({
            "event": "teamd.start",
            "version": td::VERSION,
            "members": opts.members,
            "nats_url": opts.nats_url,
            "a2a_url": opts.a2a_url,
            "mgmt_port": opts.mgmt_port,
            "workers": opts.workers,
            "per_member_limit": opts.per_member_limit,
            "queue_size": opts.queue_size,
        })
    );

    let members = opts.members.clone();
    let creds = match td::resolve_member_creds(
        &members,
        &opts.user_entries,
        &opts.password_file_entries,
    ) {
        Ok(c) => c,
        Err(missing) => {
            eprintln!("[FATAL] 成员凭据缺失：");
            for m in &missing {
                eprintln!("  - {m}");
            }
            eprintln!("提示：nats-passwd.py 需要先跑过；或用 --password-file 显式指定路径。");
            std::process::exit(2);
        }
    };

    let counters = Arc::new(td::Counters::new());
    let gate = Arc::new(td::TaskGate::new(opts.queue_size));
    let stop = Arc::new(AtomicBool::new(false));
    let a2a_url = opts.a2a_url.clone();
    let a2a_token = read_token(&opts.a2a_token_file);

    // ---- worker 池 -----------------------------------------------------
    for _ in 0..opts.workers {
        let gate = gate.clone();
        let counters = counters.clone();
        let a2a_url = a2a_url.clone();
        let a2a_token = a2a_token.clone();
        let stop = stop.clone();
        let nats_url_w = opts.nats_url.clone();
        std::thread::spawn(move || {
        let worker_conn = nats::NatsClient::new(
            nats::NatsConfig::from_hosts_str(&nats_url_w, 4222)
                .unwrap_or_else(|_| nats::NatsConfig::from_hosts_str("127.0.0.1", 4222).unwrap()),
        );
        let _ = worker_conn.connect();
        let conn_for_worker = worker_conn;
        loop {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            let Some((payload, reply, member)) = gate.pop(Duration::from_millis(500)) else {
                continue;
            };
            let started = Instant::now();
            let (text, caller, session_id, timeout_s) =
                match td::parse_request_payload(&payload) {
                    Ok(v) => v,
                    Err(env) => {
                        counters.inc("bad_requests");
                        counters.inc("errors");
                        let _ = conn_for_worker.publish(&reply, env.to_string().as_bytes(), "");
                        gate.release(&member);
                        continue;
                    }
                };
            counters.inc("rx_requests");
            td::log_event(
                "req.start",
                Some(&member),
                &[("from", json!(caller)), ("timeout_s", json!(timeout_s))],
            );
            let outcome = call_a2a(
                &a2a_url,
                a2a_token.as_deref(),
                &text,
                &caller,
                session_id.as_deref(),
                timeout_s,
            );
            let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
            let reply_payload = match outcome {
                Ok(body) => {
                    counters.inc("ok");
                    td::log_event("req.done", Some(&member), &[("elapsed_ms", json!(elapsed_ms))]);
                    body
                }
                Err((kind, detail, status)) => {
                    if kind == "timeout" {
                        counters.inc("timeouts");
                    }
                    counters.inc("errors");
                    td::log_event(
                        "req.error",
                        Some(&member),
                        &[
                            ("error", json!(kind)),
                            ("elapsed_ms", json!(elapsed_ms)),
                        ],
                    );
                    td::err_envelope(&kind, &detail, status)
                }
            };
            if conn_for_worker.publish(&reply, reply_payload.to_string().as_bytes(), "").is_err() {
                counters.inc("hb_failed"); // 回投失败走同一计数（诊断面）
            }
            gate.release(&member);
        }
        });
    }

    // ---- 每成员一条连接 + 心跳 -------------------------------------------
    let mut handles = Vec::new();
    for m in &members {
        let (user, pass_file) = creds.get(m).cloned().expect("creds 已解析");
        let m = m.clone();
        let nats_url = opts.nats_url.clone();
        let gate = gate.clone();
        let counters = counters.clone();
        let stop = stop.clone();
        let per_member_limit = opts.per_member_limit;
        let busy_retry = td::BUSY_RETRY_AFTER_MS;
        handles.push(std::thread::spawn(move || {
            member_loop(MemberLoop {
                member: m,
                nats_url,
                user,
                pass_file,
                gate,
                counters,
                stop,
                per_member_limit,
                busy_retry,
            });
        }));
    }

    // ---- 管理口 ----------------------------------------------------------
    let mgmt = opts.mgmt_port;
    let mgmt_counters = counters.clone();
    let mgmt_gate = gate.clone();
    let mgmt_workers = opts.workers;
    let mgmt_limit = opts.per_member_limit;
    std::thread::spawn(move || {
        serve_mgmt(mgmt, mgmt_counters, mgmt_gate, mgmt_workers, mgmt_limit);
    });

    for h in handles {
        let _ = h.join();
    }
}

struct Opts {
    members: Vec<String>,
    user_entries: Vec<String>,
    password_file_entries: Vec<String>,
    nats_url: String,
    a2a_url: String,
    a2a_token_file: String,
    mgmt_port: u16,
    workers: usize,
    per_member_limit: usize,
    queue_size: usize,
}

fn parse_args(args: &[String]) -> Result<Opts, Vec<String>> {
    let mut members = Vec::new();
    let mut user_entries = Vec::new();
    let mut password_file_entries = Vec::new();
    let mut nats_url = "nats://127.0.0.1:4222".to_string();
    let mut a2a_url = "http://127.0.0.1:9920".to_string();
    let mut a2a_token_file = String::new();
    let mut mgmt_port = 9960u16;
    let mut workers = td::DEFAULT_WORKERS;
    let mut per_member_limit = td::DEFAULT_PER_MEMBER_LIMIT;
    let mut queue_size = td::DEFAULT_QUEUE_SIZE;
    let mut errors = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let flag = args[i].as_str();
        let mut next = || {
            i += 1;
            args.get(i).cloned()
        };
        match flag {
            "--member" | "-m" => match next() {
                Some(v) => members.push(v),
                None => errors.push("[FATAL] --member 需要一个值".into()),
            },
            "--user" => match next() {
                Some(v) => user_entries.push(v),
                None => errors.push("[FATAL] --user 需要一个值".into()),
            },
            "--password-file" => match next() {
                Some(v) => password_file_entries.push(v),
                None => errors.push("[FATAL] --password-file 需要一个值".into()),
            },
            "--nats-url" => match next() {
                Some(v) => nats_url = v,
                None => errors.push("[FATAL] --nats-url 需要一个值".into()),
            },
            "--a2a-url" => match next() {
                Some(v) => a2a_url = v,
                None => errors.push("[FATAL] --a2a-url 需要一个值".into()),
            },
            "--a2a-token-file" => match next() {
                Some(v) => a2a_token_file = v,
                None => errors.push("[FATAL] --a2a-token-file 需要一个值".into()),
            },
            "--mgmt-port" => match next().and_then(|v| v.parse::<u16>().ok()) {
                Some(v) => mgmt_port = v,
                None => errors.push("[FATAL] --mgmt-port 需要一个 0-65535 的整数".into()),
            },
            "--workers" => match next().and_then(|v| v.parse::<usize>().ok()) {
                Some(v) if v >= 1 => workers = v,
                _ => errors.push("[FATAL] --workers 需要 >= 1".into()),
            },
            "--per-member-limit" => match next().and_then(|v| v.parse::<usize>().ok()) {
                Some(v) if v >= 1 => per_member_limit = v,
                _ => errors.push("[FATAL] --per-member-limit 需要 >= 1".into()),
            },
            "--queue-size" => match next().and_then(|v| v.parse::<usize>().ok()) {
                Some(v) if v >= 1 => queue_size = v,
                _ => errors.push("[FATAL] --queue-size 需要 >= 1".into()),
            },
            other => errors.push(format!("[FATAL] 未知参数: {other}")),
        }
        i += 1;
    }
    if members.is_empty() {
        errors.push("[FATAL] 至少需要一个 --member（可重复给多个成员）".into());
    }
    if !errors.is_empty() {
        return Err(errors);
    }
    Ok(Opts {
        members,
        user_entries,
        password_file_entries,
        nats_url,
        a2a_url,
        a2a_token_file,
        mgmt_port,
        workers,
        per_member_limit,
        queue_size,
    })
}

fn read_token(path: &str) -> Option<String> {
    if path.is_empty() {
        return None;
    }
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

struct MemberLoop {
    member: String,
    nats_url: String,
    user: String,
    pass_file: String,
    gate: Arc<td::TaskGate>,
    counters: Arc<td::Counters>,
    stop: Arc<AtomicBool>,
    per_member_limit: usize,
    busy_retry: u64,
}

/// 单成员主循环：连接 → 订阅 → 心跳 → PING 探活，退避重连。
fn member_loop(ctx: MemberLoop) {
    let password = match std::fs::read_to_string(&ctx.pass_file) {
        Ok(p) => p.trim().to_string(),
        Err(e) => {
            td::log_event(
                "fatal",
                Some(&ctx.member),
                &[("error", json!(format!("口令文件读不到: {e}")))],
            );
            std::process::exit(2);
        }
    };
    let subject_req = format!("a2a.v1.req.{}", ctx.member);
    let subject_hb = format!("team.hb.{}", ctx.member);
    let mut backoff_idx: usize = 0;
    let mut last_hb = Instant::now() - Duration::from_secs(3600); // 连上先发一跳

    loop {
        if ctx.stop.load(Ordering::Relaxed) {
            return;
        }
        let conn = nats::NatsClient::new(
            nats::NatsConfig::from_hosts_str(&ctx.nats_url, 4222)
                .unwrap_or_else(|_| nats::NatsConfig::from_hosts_str("127.0.0.1", 4222).unwrap())
                .with_credentials(Some(ctx.user.clone()), Some(password.clone()))
                .with_name(td::REQ_SID),
        );
        let conn_result = conn.connect();
        let conn = match conn_result {
            Ok(()) => {
                backoff_idx = 0;
                conn
            }
            Err(e) => {
                ctx.counters.inc("reconnects");
                let wait = td::BACKOFF_STEPS[backoff_idx.min(td::BACKOFF_STEPS.len() - 1)];
                backoff_idx += 1;
                td::log_event(
                    "conn.retry",
                    Some(&ctx.member),
                    &[("error", json!(e.0)), ("wait_s", json!(wait))],
                );
                std::thread::sleep(Duration::from_secs_f64(wait));
                continue;
            }
        };
        td::log_event("conn.up", Some(&ctx.member), &[]);

        // 订阅；reader 回调只入队
        {
            let sub_gate = ctx.gate.clone();
            let sub_counters = ctx.counters.clone();
            let sub_member = ctx.member.clone();
            let sub_limit = ctx.per_member_limit;
            let sub_busy_retry = ctx.busy_retry;
            let hb_subject = subject_hb.clone();
            let _ = conn.subscribe(
                &subject_req,
                move |payload: &[u8], reply_to: &str| {
                    if reply_to.is_empty() {
                        return;
                    }
                    match sub_gate.admit(&sub_member, sub_limit) {
                        Err(reason) => {
                            sub_counters.inc("busy_rejected");
                            let env = json!({
                                "ok": false,
                                "error": "busy",
                                "detail": reason,
                                "retry_after_ms": sub_busy_retry,
                            });
                            // 回投由 worker 连接完成；此处只记录
                            td::log_event("req.busy", Some(&sub_member), &[("reason", json!(reason)), ("reply_to", json!(reply_to))]);
                            // busy 也必须回：直接在本连接回（publish 带 reply 参数为空）
                            busy_reply(&sub_member, reply_to, &env);
                        }
                        Ok(()) => {
                            if let Err(e) = sub_gate.enqueue(payload.to_vec(), reply_to.to_string(), &sub_member) {
                                sub_counters.inc("busy_rejected");
                                let env = json!({
                                    "ok": false,
                                    "error": "busy",
                                    "detail": e,
                                    "retry_after_ms": sub_busy_retry,
                                });
                                busy_reply(&sub_member, reply_to, &env);
                                sub_gate.release(&sub_member);
                            }
                        }
                    }
                    let _ = &hb_subject;
                },
                td::REQ_SID,
            );
        }

        // 心跳 + PING 探活循环（简单起见与读循环同线程轮询 nats::flush_timeout）
        loop {
            if ctx.stop.load(Ordering::Relaxed) {
                return;
            }
            std::thread::sleep(Duration::from_millis(500));
            if last_hb.elapsed() >= Duration::from_secs(30) {
                let payload = json!({
                    "member": ctx.member,
                    "ts": SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0),
                    "v": td::VERSION,
                });
                match conn.publish(&subject_hb, payload.to_string().as_bytes(), "") {
                    Ok(()) => {
                        ctx.counters.inc("hb_sent");
                        last_hb = Instant::now();
                    }
                    Err(_) => {
                        ctx.counters.inc("hb_failed");
                        break; // 连接已死 → 重连
                    }
                }
            }
            if !conn.flush(Duration::from_secs(5)) || !conn.is_connected() {
                ctx.counters.inc("reconnects");
                td::log_event("conn.lost", Some(&ctx.member), &[]);
                break;
            }
        }
    }
}

/// 调本机 A2A：`POST <a2a-url>/message`。错误归类：
///   * 连不上 / 超时 → `("timeout"|"http_unreachable", detail, None)`
///   * HTTP 状态非 2xx → `("http_error", "HTTP <code>: <尾 200 字>", Some(code))`
///   * 响应非 JSON 对象 → `("bad_response", ...)`
fn call_a2a(
    base_url: &str,
    token: Option<&str>,
    text: &str,
    caller: &str,
    session_id: Option<&str>,
    timeout_s: f64,
) -> Result<Value, (String, String, Option<u16>)> {
    let url = format!("{}/message", base_url.trim_end_matches('/'));
    let mut body = json!({
        "text": text,
        "from": caller,
        "timeout_s": timeout_s,
    });
    if let Some(s) = session_id {
        body["session_id"] = json!(s);
    }
    let payload = body.to_string();
    let mut headers: Vec<(&str, &str)> = vec![("Content-Type", "application/json")];
    let token_str;
    if let Some(t) = token {
        token_str = format!("Bearer {t}");
        headers.push(("Authorization", &token_str));
    }

    let deadline = Instant::now() + Duration::from_secs_f64(timeout_s);
    let resp = http_post(&url, &headers, payload.as_bytes(), deadline).map_err(|e| {
        (
            "timeout".to_string(),
            format!("A2A 调用失败: {}", e.0),
            None,
        )
    })?;
    if !(200..300).contains(&resp.status) {
        let tail: String = String::from_utf8_lossy(&resp.body)
            .chars()
            .rev()
            .take(200)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        return Err((
            "http_error".to_string(),
            format!("HTTP {}: {}", resp.status, tail),
            Some(resp.status),
        ));
    }
    let value: Value = serde_json::from_slice(&resp.body)
        .map_err(|e| ("bad_response".to_string(), format!("A2A 响应不是 JSON: {e}"), None))?;
    if !value.is_object() {
        return Err((
            "bad_response".to_string(),
            "A2A 响应顶层不是对象".to_string(),
            None,
        ));
    }
    Ok(value)
}

/// 简易 POST（复用 L1 http_client 的 URL 解析思路；HTTP/1.0，读满即关）。
fn http_post(
    url: &str,
    headers: &[(&str, &str)],
    body: &[u8],
    deadline: Instant,
) -> Result<hcli::HttpResponse, hcli::HttpError> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| hcli::HttpError(format!("只支持 http:// 前缀: {url}")))?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (
            h.to_string(),
            p.parse::<u16>()
                .map_err(|_| hcli::HttpError(format!("端口不是数字: {p}")))?,
        ),
        None => (authority.to_string(), 80),
    };
    let remain = deadline.saturating_duration_since(Instant::now());
    let addr = (host.as_str(), port)
        .to_socket_addrs()
        .map_err(|e| hcli::HttpError(format!("解析地址失败: {e}")))?
        .next()
        .ok_or_else(|| hcli::HttpError("无可用地址".into()))?;
    let mut sock = TcpStream::connect_timeout(&addr, remain)
        .map_err(|e| hcli::HttpError(format!("连不上 {host}:{port} — {e}")))?;
    sock.set_read_timeout(Some(remain)).ok();
    sock.set_write_timeout(Some(remain)).ok();
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
    sock.write_all(&req)
        .map_err(|e| hcli::HttpError(format!("发送失败: {e}")))?;
    sock.flush().ok();
    let mut raw = Vec::new();
    sock.read_to_end(&mut raw)
        .map_err(|e| hcli::HttpError(format!("读取失败: {e}")))?;
    // 复用 http_client 的响应解析（pub 未导出，这里手写同构解析）
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| hcli::HttpError("响应头不完整".into()))?;
    let head = String::from_utf8_lossy(&raw[..split]).to_string();
    let resp_body = raw[split + 4..].to_vec();
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or("");
    let status = status_line
        .split(' ')
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| hcli::HttpError(format!("状态行看不懂: {status_line:?}")))?;
    let mut resp_headers = Vec::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            resp_headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    Ok(hcli::HttpResponse {
        status,
        headers: resp_headers,
        body: resp_body,
    })
}

use std::net::ToSocketAddrs;

/// 管理面：`GET /healthz`（免鉴权存活）、`GET /varz`（计数器 + 门闸状态）。
fn serve_mgmt(
    port: u16,
    counters: Arc<td::Counters>,
    gate: Arc<td::TaskGate>,
    workers: usize,
    per_member_limit: usize,
) {
    let listener = match TcpListener::bind(("127.0.0.1", port)) {
        Ok(l) => l,
        Err(e) => {
            td::log_event("mgmt.bind_failed", None, &[("error", json!(e.to_string()))]);
            return;
        }
    };
    td::log_event("mgmt.up", None, &[("port", json!(port))]);
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue };
        let counters = counters.clone();
        let gate = gate.clone();
        std::thread::spawn(move || {
            let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            let req = String::from_utf8_lossy(&buf);
            let path = req.split_whitespace().nth(1).unwrap_or("/");
            let (status, body) = match path {
                "/healthz" => (
                    200,
                    json!({"ok": true, "service": "teamd", "version": td::VERSION}),
                ),
                "/varz" => (
                    200,
                    json!({
                        "ok": true,
                        "service": "teamd",
                        "version": td::VERSION,
                        "counters": counters.snapshot(),
                        "gate": gate.stats(workers, per_member_limit),
                        "ts": SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .map(|d| d.as_secs())
                            .unwrap_or(0),
                    }),
                ),
                _ => (404, json!({"ok": false, "error": "not_found"})),
            };
            let body = body.to_string();
            let resp = format!(
                "HTTP/1.0 {status} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                if status == 200 { "OK" } else { "Not Found" },
                body.len()
            );
            let _ = stream.write_all(resp.as_bytes());
            let _ = stream.flush();
        });
    }
}

// 引用计数器原子量（保留给未来升级为 lock-free；当前 Counters 内部是 Mutex）
#[allow(dead_code)]
static _COUNTER_HINT: AtomicU64 = AtomicU64::new(0);
