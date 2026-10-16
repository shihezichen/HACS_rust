//! L1 通信层等价测试（对照 Python `node/l1_transport/web_gateway.py` 与 `nats_client.py`）。
//!
//! 判据是**线上形态**：网关的响应按原始字节断言（状态行 / 头 / Python 风格 JSON 分隔符），
//! NATS 客户端对着一个手写的假 NATS 服务端走完整握手 + PUB/SUB + request/reply。

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hacs_l1_transport::http_client;
use hacs_l1_transport::nats_client::{parse_addrs, read_secret_file, NatsClient, NatsConfig};
use hacs_l1_transport::web_gateway::{Gateway, HttpError, Reply};

// ------------------------------------------------------------------ 网关

fn raw_request(port: u16, request: &str) -> String {
    let mut sock = TcpStream::connect(("127.0.0.1", port)).unwrap();
    sock.write_all(request.as_bytes()).unwrap();
    sock.flush().unwrap();
    let mut out = String::new();
    // 默认 HTTP/1.0：服务端答完即关，读到 EOF 为止。
    let _ = sock.read_to_string(&mut out);
    out
}

fn split_head_body(resp: &str) -> (String, String) {
    match resp.split_once("\r\n\r\n") {
        Some((h, b)) => (h.to_string(), b.to_string()),
        None => (resp.to_string(), String::new()),
    }
}

fn build_gateway() -> Gateway {
    let mut gw = Gateway::new("teamd", Some("1.0"), true);
    gw.route("/healthz", &["GET"], false, |_req| {
        Ok(Reply::json(200, serde_json::json!({"ok": true})))
    });
    gw.route("/api/", &["GET", "POST"], true, |req| {
        Ok(Reply::json(
            200,
            serde_json::json!({"path": req.path, "method": req.method}),
        ))
    });
    gw.route("/echo", &["POST"], false, |req| {
        let body = req.json_body(1024 * 1024)?;
        Ok(Reply::json(200, serde_json::json!({"got": body})))
    });
    gw.route("/boom", &["GET"], false, |_req| {
        Err(HttpError::error(403, "forbidden"))
    });
    gw
}

#[test]
fn json_reply_uses_python_style_separators_and_content_type() {
    let gw = build_gateway();
    let port = gw.serve_in_background("127.0.0.1", 0).unwrap();
    let resp = raw_request(port, "GET /healthz HTTP/1.0\r\nHost: x\r\n\r\n");
    let (head, body) = split_head_body(&resp);
    assert!(head.starts_with("HTTP/1.0 200 OK\r\n"), "状态行: {head}");
    assert!(
        head.contains("Content-Type: application/json; charset=utf-8"),
        "内容类型: {head}"
    );
    assert!(head.contains("Content-Length: 12"), "长度: {head}");
    // Python json.dumps({"ok": True}) == {"ok": true}（冒号后有空格）
    assert_eq!(body, "{\"ok\": true}");
    assert!(head.contains("Server: teamd/1.0"));
}

#[test]
fn default_not_found_envelope_names_the_path() {
    let gw = build_gateway();
    let port = gw.serve_in_background("127.0.0.1", 0).unwrap();
    let resp = raw_request(port, "GET /nope HTTP/1.0\r\n\r\n");
    let (head, body) = split_head_body(&resp);
    assert!(head.starts_with("HTTP/1.0 404 Not Found"), "{head}");
    assert_eq!(
        body,
        "{\"ok\": false, \"error\": \"not_found\", \"path\": \"/nope\"}"
    );
}

#[test]
fn registration_and_dispatch_share_path_normalization() {
    // 登记 "/api/" 必须能被 "/api" 命中（否则是静默死路由）。
    let gw = build_gateway();
    let port = gw.serve_in_background("127.0.0.1", 0).unwrap();
    let resp = raw_request(port, "GET /api HTTP/1.0\r\n\r\n");
    let (_, body) = split_head_body(&resp);
    assert_eq!(body, "{\"path\": \"/api\", \"method\": \"GET\"}");
    // 前缀下钻也命中
    let resp2 = raw_request(port, "GET /api/items HTTP/1.0\r\n\r\n");
    let (_, body2) = split_head_body(&resp2);
    assert_eq!(body2, "{\"path\": \"/api/items\", \"method\": \"GET\"}");
}

#[test]
fn unregistered_method_stays_501_not_405() {
    let gw = build_gateway();
    let port = gw.serve_in_background("127.0.0.1", 0).unwrap();
    // DELETE 没有任何登记 ⇒ 保持基类 501 行为，不自行发明 405 信封。
    let resp = raw_request(port, "DELETE /healthz HTTP/1.0\r\n\r\n");
    let (head, body) = split_head_body(&resp);
    assert!(
        head.starts_with("HTTP/1.0 501 Unsupported method ('DELETE')"),
        "{head}"
    );
    assert!(head.contains("Content-Type: text/html;charset=utf-8"));
    assert!(body.contains("Error code: 501"));
}

#[test]
fn json_body_rejects_bad_and_oversized_payloads() {
    let gw = build_gateway();
    let port = gw.serve_in_background("127.0.0.1", 0).unwrap();

    let bad = "POST /echo HTTP/1.0\r\nContent-Length: 4\r\n\r\nnope";
    let resp = raw_request(port, bad);
    let (head, body) = split_head_body(&resp);
    assert!(head.starts_with("HTTP/1.0 400 Bad Request"), "{head}");
    assert_eq!(body, "{\"ok\": false, \"error\": \"bad_json\"}");

    let payload = "{\"a\": 1}";
    let ok = format!(
        "POST /echo HTTP/1.0\r\nContent-Length: {}\r\n\r\n{}",
        payload.len(),
        payload
    );
    let resp2 = raw_request(port, &ok);
    let (_, body2) = split_head_body(&resp2);
    assert_eq!(body2, "{\"got\": {\"a\": 1}}");
}

#[test]
fn hooks_can_short_circuit_before_routing() {
    let mut gw = Gateway::new("gated", None, true);
    gw.route("/priv", &["GET"], false, |_req| {
        Ok(Reply::json(200, serde_json::json!({"ok": true})))
    });
    gw.pre(|req| {
        if req.path == "/priv" && req.header("X-Key").is_none() {
            return Ok(Some(Reply::json(
                401,
                serde_json::json!({"ok": false, "error": "no_key"}),
            )));
        }
        Ok(None)
    });
    let port = gw.serve_in_background("127.0.0.1", 0).unwrap();

    let denied = raw_request(port, "GET /priv HTTP/1.0\r\n\r\n");
    let (head, body) = split_head_body(&denied);
    assert!(head.starts_with("HTTP/1.0 401"), "{head}");
    assert_eq!(body, "{\"ok\": false, \"error\": \"no_key\"}");

    let allowed = raw_request(port, "GET /priv HTTP/1.0\r\nX-Key: s\r\n\r\n");
    let (_, body2) = split_head_body(&allowed);
    assert_eq!(body2, "{\"ok\": true}");
}

#[test]
fn http_error_from_handler_becomes_the_reply() {
    let gw = build_gateway();
    let port = gw.serve_in_background("127.0.0.1", 0).unwrap();
    let resp = raw_request(port, "GET /boom HTTP/1.0\r\n\r\n");
    let (head, body) = split_head_body(&resp);
    assert!(head.starts_with("HTTP/1.0 403 Forbidden"), "{head}");
    assert_eq!(body, "{\"ok\": false, \"error\": \"forbidden\"}");
}

// ------------------------------------------------------------------ NATS

/// 假 NATS 服务端：INFO → CONNECT/PING → SUB/PUB，并把 PUB 回灌给订阅者。
fn spawn_fake_nats() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let (sock, _) = match listener.accept() {
            Ok(v) => v,
            Err(_) => return,
        };
        let mut reader = BufReader::new(sock.try_clone().unwrap());
        let mut writer = sock;
        let _ = writer.write_all(b"INFO {\"server_id\":\"fake\",\"max_payload\":1048576}\r\n");
        let _ = writer.flush();
        let mut subs: HashMap<String, String> = HashMap::new();
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
            let line = line.trim_end().to_string();
            if line == "PING" {
                let _ = writer.write_all(b"PONG\r\n");
                let _ = writer.flush();
            } else if line.starts_with("CONNECT ") || line.starts_with("UNSUB ") {
                continue;
            } else if let Some(rest) = line.strip_prefix("SUB ") {
                let mut it = rest.split_whitespace();
                if let (Some(subject), Some(sid)) = (it.next(), it.next()) {
                    subs.insert(subject.to_string(), sid.to_string());
                }
            } else if let Some(rest) = line.strip_prefix("PUB ") {
                let parts: Vec<&str> = rest.split_whitespace().collect();
                let (subject, reply, size) = if parts.len() >= 3 {
                    (
                        parts[0],
                        Some(parts[1]),
                        parts[2].parse::<usize>().unwrap_or(0),
                    )
                } else if parts.len() == 2 {
                    (parts[0], None, parts[1].parse::<usize>().unwrap_or(0))
                } else {
                    continue;
                };
                let mut payload = vec![0u8; size];
                if reader.read_exact(&mut payload).is_err() {
                    return;
                }
                let mut crlf = [0u8; 2];
                let _ = reader.read_exact(&mut crlf);
                for target in [reply, Some(subject)].into_iter().flatten() {
                    if let Some(sid) = subs.get(target) {
                        let head = format!("MSG {target} {sid} {size}\r\n");
                        let _ = writer.write_all(head.as_bytes());
                        let _ = writer.write_all(&payload);
                        let _ = writer.write_all(b"\r\n");
                        let _ = writer.flush();
                    }
                }
            }
        }
    });
    port
}

#[test]
fn parse_addrs_handles_forms_and_order() {
    let a = parse_addrs("h1:9930,h2", 9930).unwrap();
    assert_eq!(a, vec![("h1".to_string(), 9930), ("h2".to_string(), 9930)]);
    let b = parse_addrs("only", 1234).unwrap();
    assert_eq!(b, vec![("only".to_string(), 1234)]);
    assert!(parse_addrs("", 9930).is_err());
    assert!(parse_addrs(" , ", 9930).is_err());
}

#[test]
fn nats_handshake_publish_subscribe_and_request() {
    let port = spawn_fake_nats();
    let cfg = NatsConfig::from_hosts_str(&format!("127.0.0.1:{port}"), 9930)
        .unwrap()
        .with_name("test-client")
        .with_timeout(Duration::from_secs(3));
    let client = NatsClient::new(cfg);
    client.connect().unwrap();
    assert!(client.is_connected());
    assert_eq!(client.host(), "127.0.0.1");
    assert_eq!(client.port(), port);
    assert!(client.flush(Duration::from_secs(2)));

    let got: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = got.clone();
    client
        .subscribe(
            "team.hb.x",
            move |p: &[u8], _r: &str| sink.lock().unwrap().push(p.to_vec()),
            "1",
        )
        .unwrap();
    client.publish("team.hb.x", b"hello", "").unwrap();

    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline && got.lock().unwrap().is_empty() {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        got.lock().unwrap().first().map(|v| v.as_slice()),
        Some(&b"hello"[..])
    );

    // request/reply：收件箱回灌
    let reply = client.request("a2a.v1.req.x", b"ping", Duration::from_secs(3));
    assert_eq!(reply, Some(b"ping".to_vec()));
}

#[test]
fn nats_connect_fails_with_readable_reason_for_bad_host() {
    // 端口没人听：必须报错而不是静默
    let cfg = NatsConfig::from_hosts_str("127.0.0.1:1", 9930)
        .unwrap()
        .with_timeout(Duration::from_millis(300));
    let client = NatsClient::new(cfg);
    let err = client.connect().unwrap_err();
    assert!(err.to_string().contains("连不上任何 NATS 地址"), "{err}");
}

#[test]
fn read_secret_file_strips_bom_and_whitespace() {
    let dir = std::env::temp_dir().join(format!("hacs_rust_secret_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("client-token");
    std::fs::write(&f, "\u{feff}  s3cret\n").unwrap();
    assert_eq!(read_secret_file(f.to_str().unwrap()).unwrap(), "s3cret");
    assert!(read_secret_file(dir.join("missing").to_str().unwrap()).is_err());
}

// ------------------------------------------------------------------ HTTP 客户端

#[test]
fn quote_matches_python_urllib_quote() {
    // 除 A-Za-z0-9_.-~ 外一律百分号编码
    assert_eq!(http_client::quote("aA0_.-~"), "aA0_.-~");
    assert_eq!(http_client::quote("a b/c?d=e&f"), "a%20b%2Fc%3Fd%3De%26f");
    assert_eq!(
        http_client::quote("a2a:claudecode:s-1"),
        "a2a%3Aclaudecode%3As-1"
    );
    // UTF-8 逐字节
    assert_eq!(http_client::quote("好"), "%E5%A5%BD");
}

#[test]
fn http_client_reads_json_from_the_gateway() {
    let gw = build_gateway();
    let port = gw.serve_in_background("127.0.0.1", 0).unwrap();
    let url = format!("http://127.0.0.1:{port}/healthz");
    let (status, value) = http_client::get_json(&url, &[], Duration::from_secs(3)).unwrap();
    assert_eq!(status, 200);
    assert_eq!(value, serde_json::json!({"ok": true}));
}

#[test]
fn http_client_reports_unreachable_instead_of_hanging() {
    // 端口没人听：必须报错，且受超时约束
    let started = Instant::now();
    let err = http_client::get("http://127.0.0.1:1/x", &[], Duration::from_millis(500));
    assert!(err.is_err(), "连不上必须报错");
    assert!(started.elapsed() < Duration::from_secs(5));
}
