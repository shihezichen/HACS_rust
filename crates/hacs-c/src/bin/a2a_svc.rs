//! `a2a_svc` —— A2A 边缘服务进程（对应 Python `a2a_edge.py` 的 HTTP 装配 +
//! 组合根 `a2a_svc.py` 的当前形态：能力未注入，走降级口径）。
//!
//! 路由（与 Python 逐条对齐）：
//!   GET  /healthz                              免鉴权存活探针
//!   GET  /agents                               鉴权后列本机成员
//!   GET  /a2a/<id>/.well-known/agent-card.json 公开 agent card
//!   POST /a2a/<id>/messages                    派活（鉴权 + task_loop 续跑）
//!   其余                                       裸 404（无 path 字段）
//!   PUT/DELETE/HEAD/OPTIONS                    501（web_gateway 未登记方法行为）
//!
//! `Server: a2a_svc/2.2`、HTTP/1.1（keep-alive）、日志一行一请求
//! `<方法> <路径> -> <码> from=<成员> ip=<ip>`。

use std::io::Write;
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use hacs_l1_transport::a2a_edge as edge;
use hacs_l1_transport::web_gateway::{Gateway, Reply, Request};
use serde_json::{json, Value};

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

struct Runtime {
    node_id: String,
    agents: Value,
    peers: Vec<(String, String)>,
    advertise_host: String,
    advertise_port: u16,
    allow_anon: bool,
}

fn main() {
    let mut host = "127.0.0.1".to_string();
    let mut port = 9920u16;
    let mut config = String::new();
    let mut peers_path: Option<String> = None;
    let mut advertise_host: Option<String> = None;
    let mut advertise_port: Option<u16> = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--host" => host = args.next().unwrap_or(host),
            "--port" => port = args.next().and_then(|v| v.parse().ok()).unwrap_or(port),
            "--config" => config = args.next().unwrap_or_default(),
            "--peers" => peers_path = args.next(),
            "--advertise-host" => advertise_host = args.next(),
            "--advertise-port" => advertise_port = args.next().and_then(|v| v.parse().ok()),
            other => {
                eprintln!("未知参数: {other}");
                std::process::exit(2);
            }
        }
    }
    if config.is_empty() {
        eprintln!("FAIL 需要 --config <agents.json>");
        std::process::exit(2);
    }

    let cfg_text = match std::fs::read_to_string(&config) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("加载 agents.json 失败: {e}");
            std::process::exit(2);
        }
    };
    let (node_id, agents, cfg_host, cfg_port) = match edge::load_config(&cfg_text) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("加载 agents.json 失败: {e}");
            std::process::exit(2);
        }
    };
    let peers = match &peers_path {
        None => Vec::new(),
        Some(p) => match std::fs::read_to_string(p).map_err(|e| e.to_string())
            .and_then(|t| edge::load_peers(&t))
        {
            Ok(v) => v,
            Err(e) => {
                eprintln!("加载 peers.json 失败: {e}");
                std::process::exit(2);
            }
        },
    };

    let allow_anon = std::env::var("A2A_ALLOW_ANON").as_deref() == Ok("1");
    if peers.is_empty() && !allow_anon {
        eprintln!("WARNING peers 表为空且未开启 A2A_ALLOW_ANON，所有鉴权请求都会 401");
    }
    if allow_anon {
        eprintln!("WARNING A2A_ALLOW_ANON=1 已开启，鉴权被跳过（仅限本地调试）");
    }

    let rt = Arc::new(Runtime {
        node_id,
        agents,
        peers,
        advertise_host: advertise_host
            .or(if cfg_host.is_empty() { None } else { Some(cfg_host) })
            .unwrap_or_else(|| host.clone()),
        advertise_port: advertise_port
            .or(cfg_port.map(|p| p as u16))
            .unwrap_or(port),
        allow_anon,
    });

    let mut gw = Gateway::new("a2a_svc", Some(edge::VERSION), true);
    gw.protocol_version = "HTTP/1.1".to_string();
    gw.sys_version = String::new();

    // ---- GET /healthz（免鉴权）------------------------------------------
    let rt_h = rt.clone();
    gw.route("/healthz", &["GET"], false, move |_req| {
        let agent_names: Vec<String> = rt_h
            .agents
            .as_object()
            .map(|o| o.keys().cloned().collect())
            .unwrap_or_default();
        Ok(Reply::json(
            200,
            json!({
                "ok": true,
                "node": rt_h.node_id,
                "version": edge::VERSION,
                "protocolVersion": edge::PROTOCOL_VERSION,
                "agents": agent_names,
                "ts": now_secs(),
            }),
        ))
    });

    // ---- GET /agents（鉴权）---------------------------------------------
    let rt_a = rt.clone();
    gw.route("/agents", &["GET"], false, move |req| {
        let peer = edge::auth_peer(req.header("Authorization"), &rt_a.peers, rt_a.allow_anon);
        match peer {
            None => Ok(Reply::json(401, json!({"ok": false, "error": "unauthorized"}))),
            Some(_peer) => {
                let mut listing = serde_json::Map::new();
                if let Some(obj) = rt_a.agents.as_object() {
                    for (aid, cfg) in obj {
                        listing.insert(
                            aid.clone(),
                            json!({
                                "description": cfg.get("description").cloned().unwrap_or(Value::String(String::new())),
                                "skills": cfg.get("skills").cloned().unwrap_or(Value::Array(Vec::new())),
                            }),
                        );
                    }
                }
                Ok(Reply::json(
                    200,
                    json!({"ok": true, "node": rt_a.node_id, "agents": listing}),
                ))
            }
        }
    });

    // ---- GET /a2a 前缀（注入路由 → agent-card → 404；当前无注入路由）------
    let rt_t = rt.clone();
    gw.route("/a2a", &["GET"], true, move |req| {
        let parts = split_parts(&req.raw_path);
        // /a2a/<agent_id>/.well-known/agent-card.json —— 公开
        if parts.len() == 4
            && parts[0] == "a2a"
            && parts[2] == ".well-known"
            && parts[3] == "agent-card.json"
        {
            let agent_id = &parts[1];
            let cfg = rt_t.agents.get(agent_id);
            return match cfg {
                None => Ok(Reply::json(404, json!({"ok": false, "error": "unknown_agent"}))),
                Some(cfg) => Ok(Reply::json(
                    200,
                    edge::build_agent_card(agent_id, cfg, &rt_t.advertise_host, rt_t.advertise_port),
                )),
            };
        }
        Ok(Reply::json(404, json!({"ok": false, "error": "not_found"})))
    });

    // ---- POST /a2a/<id>/messages ----------------------------------------
    let rt_p = rt.clone();
    gw.route("/a2a", &["POST"], true, move |req| {
        handle_post_messages(req, &rt_p)
    });

    // ---- 404：GET 也走 tail（当前无注入路由，等价裸 404）；POST 裸 404 ------
    let rt_nf = rt.clone();
    gw.not_found(move |req| {
        if req.method == "GET" {
            let parts = split_parts(&req.raw_path);
            let cfg = if parts.len() == 4
                && parts[0] == "a2a"
                && parts[2] == ".well-known"
                && parts[3] == "agent-card.json"
            {
                rt_nf.agents.get(&parts[1])
            } else {
                None
            };
            return match cfg {
                Some(cfg) => Ok(Reply::json(
                    200,
                    edge::build_agent_card(
                        &parts[1],
                        cfg,
                        &rt_nf.advertise_host,
                        rt_nf.advertise_port,
                    ),
                )),
                None => Ok(Reply::json(404, json!({"ok": false, "error": "not_found"}))),
            };
        }
        Ok(Reply::json(404, json!({"ok": false, "error": "not_found"})))
    });

    let agent_names: Vec<String> = rt
        .agents
        .as_object()
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default();
    println!(
        "a2a_svc {} node={} listening on {}:{} agents={}",
        edge::VERSION,
        rt.node_id,
        host,
        port,
        if agent_names.is_empty() { "-".to_string() } else { agent_names.join(",") }
    );

    if let Err(e) = gw.serve(&host, port) {
        eprintln!("监听失败: {e}");
        std::process::exit(1);
    }
}

/// Python `_parts()` 同口径：`urlsplit().path.split('/')` 去空段（fragment 已被
/// web_gateway 的 raw_path 保留；`//a` 切出空段全丢）。
fn split_parts(raw_path: &str) -> Vec<String> {
    let path = raw_path.split(['?', '#']).next().unwrap_or("");
    path.split('/').filter(|s| !s.is_empty()).map(String::from).collect()
}

fn handle_post_messages(req: &Request, rt: &Runtime) -> Result<Reply, hacs_l1_transport::web_gateway::HttpError> {
    let parts = split_parts(&req.raw_path);
    if !(parts.len() == 3 && parts[0] == "a2a" && parts[2] == "messages") {
        return Ok(Reply::json(404, json!({"ok": false, "error": "not_found"})));
    }
    let peer = match edge::auth_peer(req.header("Authorization"), &rt.peers, rt.allow_anon) {
        Some(p) => p,
        None => return Ok(Reply::json(401, json!({"ok": false, "error": "unauthorized"}))),
    };
    let agent_id = &parts[1];
    let Some(cfg) = rt.agents.get(agent_id) else {
        return Ok(Reply::json(404, json!({"ok": false, "error": "unknown_agent"})));
    };
    let body: Value = match serde_json::from_slice::<Value>(&req.body) {
        Ok(v) if v.is_object() => v,
        _ => return Ok(Reply::json(400, json!({"ok": false, "error": "bad_request"}))),
    };
    let msg = match edge::parse_message_body(&body) {
        Ok(m) => m,
        Err((status, err)) => {
            return Ok(Reply::json(status, json!({"ok": false, "error": err})))
        }
    };

    let started = Instant::now();
    let (code, result) = call_backend(cfg, &msg.text, msg.timeout_s, msg.session_id.as_deref(), &peer);
    let elapsed_ms = started.elapsed().as_millis() as u64;

    if code != 200 {
        return Ok(Reply::json(code, result));
    }
    let mut resp = json!({
        "ok": true,
        "agent": agent_id,
        "reply": result["reply"],
        "elapsed_ms": elapsed_ms,
        "session_id": msg.session_id,
        "peer": peer,
    });
    if let Some(v) = result.get("loop_rounds") {
        resp["loop_rounds"] = v.clone();
    }
    if let Some(v) = result.get("loop_stop") {
        resp["loop_stop"] = v.clone();
    }
    Ok(Reply::json(200, resp))
}

// ---------------------------------------------------------------- backend 调用（含 B2 续跑循环）

/// 一次同步 HTTP POST（HTTP/1.0 短连接；与 teamd/a2a_call 同构）。
fn http_post_json(
    url: &str,
    headers: &[(&str, String)],
    body: &[u8],
    timeout_s: f64,
) -> Result<(u16, Vec<u8>), String> {
    use std::io::Read;
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
    let dur = std::time::Duration::from_secs_f64(timeout_s.max(0.1));
    let addr = (host.as_str(), port)
        .to_socket_addrs()
        .map_err(|e| format!("解析地址失败: {e}"))?
        .next()
        .ok_or("无可用地址")?;
    let mut sock = TcpStream::connect_timeout(&addr, dur).map_err(|e| format!("连不上: {e}"))?;
    sock.set_read_timeout(Some(dur)).ok();
    sock.set_write_timeout(Some(dur)).ok();
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
    sock.read_to_end(&mut raw).map_err(|e| format!("读取失败: {e}"))?;
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

/// `_single_round`：跑一轮 backend /chat/completions。
fn single_round(
    agent: &Value,
    messages: &[Value],
    timeout_s: f64,
    user_field: Option<&str>,
) -> Result<String, (u16, Value)> {
    let (url, payload) = edge::build_single_round_request(agent, messages, user_field)?;
    let body = payload.to_string();
    let mut headers: Vec<(&str, String)> = vec![
        ("Content-Type", "application/json; charset=utf-8".to_string()),
        ("x-openclaw-message-channel", "a2a".to_string()),
    ];
    if let Some(key) = agent.get("api_key").and_then(Value::as_str) {
        if !key.is_empty() {
            headers.push(("Authorization", format!("Bearer {key}")));
        }
    }
    match http_post_json(&url, &headers, body.as_bytes(), timeout_s) {
        Err(e) => {
            if e.contains("超时") || e.contains("timed out") {
                Err((504, json!({"ok": false, "error": "backend_timeout"})))
            } else {
                Err((
                    502,
                    json!({"ok": false, "error": "backend_error", "detail": edge::truncate_detail(&e)}),
                ))
            }
        }
        Ok((status, resp_body)) if !(200..300).contains(&status) => {
            let detail = format!(
                "HTTP {} {}",
                status,
                String::from_utf8_lossy(&resp_body)
            );
            Err((
                502,
                json!({"ok": false, "error": "backend_error", "detail": edge::truncate_detail(&detail)}),
            ))
        }
        Ok((_, resp_body)) => match edge::extract_openai_reply(&resp_body) {
            Some(reply) => Ok(reply),
            None => Err((
                502,
                json!({
                    "ok": false, "error": "backend_error",
                    "detail": edge::truncate_detail(&String::from_utf8_lossy(&resp_body)),
                }),
            )),
        },
    }
}

/// `call_backend`：B2 续跑循环 + 上下文自愈（当前能力端口全走降级：pressure=None）。
fn call_backend(
    agent: &Value,
    text: &str,
    mut timeout_s: f64,
    session_id: Option<&str>,
    caller: &str,
) -> (u16, Value) {
    let sys_prompt = agent
        .get("system_prompt")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let mut messages: Vec<Value> = Vec::new();
    if !sys_prompt.is_empty() {
        messages.push(json!({"role": "system", "content": sys_prompt}));
    }
    messages.push(json!({"role": "user", "content": text}));
    timeout_s = edge::effective_timeout(agent, timeout_s);

    let loop_on = session_id.is_some() && edge::loop_enabled(agent);
    let Some(session_id) = session_id else {
        return match single_round(agent, &messages, timeout_s, None) {
            Ok(reply) => (200, json!({"reply": reply})),
            Err(e) => e,
        };
    };

    if !loop_on {
        let user_field = edge::session_user_field(caller, session_id);
        return match single_round(agent, &messages, timeout_s, Some(&user_field)) {
            Ok(reply) => (200, json!({"reply": reply})),
            Err(e) => e,
        };
    }

    // ---- 续跑循环 ----
    let total_budget = timeout_s.min(edge::TASK_LOOP_MAX_TIMEOUT_S);
    let started = Instant::now();
    let mut last_reply: Option<String> = None;
    let active_session = session_id.to_string();
    let soft_hits = 0u32;
    let rotations = 0u32;
    for rnd in 1..=edge::TASK_LOOP_MAX_ROUNDS {
        let remaining = total_budget - started.elapsed().as_secs_f64();
        if remaining <= 1.0 {
            break;
        }
        let user_field = edge::session_user_field(caller, &active_session);
        let reply = match single_round(agent, &messages, remaining, Some(&user_field)) {
            Ok(r) => r,
            Err((code, err)) => {
                if let Some(last) = last_reply {
                    return (
                        200,
                        json!({"reply": last, "loop_rounds": rnd - 1, "loop_stop": "round_error"}),
                    );
                }
                return (code, err);
            }
        };
        last_reply = Some(reply.clone());
        if edge::is_blocked(&reply) {
            return (
                200,
                json!({"reply": reply, "loop_rounds": rnd, "loop_stop": "blocked",
                       "ctx_soft": soft_hits, "ctx_rotations": rotations}),
            );
        }
        if edge::is_done(&reply) {
            return (
                200,
                json!({"reply": reply, "loop_rounds": rnd, "loop_stop": "done",
                       "ctx_soft": soft_hits, "ctx_rotations": rotations}),
            );
        }
        if rnd >= edge::TASK_LOOP_MAX_ROUNDS {
            return (
                200,
                json!({"reply": reply, "loop_rounds": rnd, "loop_stop": "max_rounds",
                       "ctx_soft": soft_hits, "ctx_rotations": rotations}),
            );
        }
        // 回灌续跑指令
        messages.push(json!({"role": "assistant", "content": reply}));
        messages.push(json!({"role": "user", "content": edge::TASK_LOOP_CONTINUE_PROMPT}));
        // 上下文自愈：能力端口未注入 → pressure=None（降级：不折腾，照常续跑）
        // 组合根后续注入 pressure 后在此分派软/硬阈值逻辑。
    }
    match last_reply {
        None => (504, json!({"ok": false, "error": "backend_timeout"})),
        Some(reply) => (
            200,
            json!({"reply": reply, "loop_rounds": 0, "loop_stop": "budget",
                   "ctx_soft": soft_hits, "ctx_rotations": rotations}),
        ),
    }
}
