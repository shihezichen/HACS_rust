//! `group_watch` 组合根 C（对应 Python `node/c/group_watch.py` + L1 投递原语
//! `group_watch_delivery` 的 deliver_ask/post_reply/http）。
//!
//! 循环做四件事：扫 open 临时群 → 以每个 Agent 身份拉 my_pending →
//! 人的 @ 优先（Agent @ 带 max-chain 防死循环）→ NATS 点名 → 写回群。
//! 幂等 (sid,member,seq) 落盘 + (sid,member) gap 限流，State 格式 = Python。
use hacs_l1_transport::http_client as hc;
use hacs_l1_transport::nats_client as nats;
use hacs_l3_domain::group_watch_pending as gw;
use serde_json::{json, Value};
use std::time::Duration;

const CHAT_BASE_DEFAULT: &str = "http://127.0.0.1:9980";
const NATS_HOST_DEFAULT: &str = "127.0.0.1:9960";
const STATE_DEFAULT: &str = "/var/lib/hacs/group_watch_state.json";
const PASS_DEFAULT: &str = "/var/lib/hacs/nats-hermes.pass";

fn flag(argv: &[String], name: &str) -> Option<String> {
    argv.iter()
        .position(|a| a == name)
        .and_then(|i| argv.get(i + 1).cloned())
}

fn has(argv: &[String], name: &str) -> bool {
    argv.iter().any(|a| a == name)
}

fn fnum(argv: &[String], name: &str, dft: f64) -> f64 {
    flag(argv, name).and_then(|v| v.parse().ok()).unwrap_or(dft)
}

fn log_line(line: &str, log_file: &Option<String>) {
    let epoch = gw::now_epoch();
    // [YYYY-MM-DD HH:MM:SS]（本地钟 = UTC 简化，日志展示用）
    let text = format!("[ts {epoch}] {line}");
    println!("{text}");
    if let Some(f) = log_file {
        use std::io::Write;
        if let Ok(mut fh) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(f)
        {
            let _ = writeln!(fh, "{text}");
        }
    }
}

/// L1 投递原语实现：HTTP（chat_svc）+ NATS（a2a.v1.req.<member>）。
struct HttpDlv {
    chat_base: String,
    nats_host: String,
    nats_user: String,
    nats_pass: String,
}

impl HttpDlv {
    fn http(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
        agent_id: Option<&str>,
    ) -> Option<Value> {
        let url = format!("{}{}", self.chat_base, path);
        let mut extra: Vec<(&str, String)> = Vec::new();
        if let Some(a) = agent_id {
            extra.push(("X-Agent-Id", a.to_string()));
        }
        let hdrs: Vec<(&str, &str)> = extra.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let t = Duration::from_secs_f64(20.0);
        let resp = if method == "GET" {
            hc::get_json(&url, &hdrs, t)
        } else {
            hc::request_json(&url, method, body, &hdrs, t)
        };
        match resp {
            Ok((status, v)) if (200..300).contains(&status) => Some(v),
            Ok((status, v)) => {
                eprintln!("HTTP {method} {url} -> {status}: {v}");
                None
            }
            Err(e) => {
                eprintln!("HTTP {method} {url} 失败：{e}");
                None
            }
        }
    }
}

impl gw::Delivery for HttpDlv {
    fn list_sessions(&mut self) -> Option<Value> {
        self.http("GET", "/api/sessions?limit=100", None, None)
    }

    fn fetch_pending(&mut self, sid: &str, member: &str) -> Option<Value> {
        self.http(
            "GET",
            &format!("/api/sessions/{sid}/messages?limit=60"),
            None,
            Some(member),
        )
    }

    fn deliver_ask(&mut self, member: &str, payload: &[u8], timeout: f64) -> Value {
        // nats_host 形如 "host1:port1,host2:port2"；取第一台（=Python deliver_ask）
        let first = self
            .nats_host
            .split(',')
            .next()
            .unwrap_or("")
            .trim()
            .to_string();
        let (host, port) = match first.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), p.parse::<u16>().unwrap_or(4222)),
            None => (first.clone(), 4222),
        };
        let client = nats::NatsClient::new(
            nats::NatsConfig::from_hosts_str(&format!("{host}:{port}"), port)
                .unwrap_or_else(|_| nats::NatsConfig::from_hosts_str("127.0.0.1", port).unwrap())
                .with_credentials(Some(self.nats_user.clone()), Some(self.nats_pass.clone())),
        );
        if client.connect().is_err() {
            return json!({"ok": false, "error": "连接失败"});
        }
        let reply = client.request(
            &format!("a2a.v1.req.{member}"),
            payload,
            Duration::from_secs_f64(timeout),
        );
        client.close();
        match reply {
            None => json!({"ok": false, "error": "超时未回应"}),
            Some(bytes) => {
                let text = String::from_utf8_lossy(&bytes).to_string();
                match serde_json::from_str::<Value>(&text) {
                    Ok(v) if v.is_object() => v,
                    _ => json!({"ok": true, "reply": text}),
                }
            }
        }
    }

    fn post_reply(&mut self, sid: &str, member: &str, reply: &str, mentions: &[String]) -> Value {
        let body = json!({"text": reply, "mentions": mentions});
        match self.http(
            "POST",
            &format!("/api/sessions/{sid}/messages"),
            Some(&body),
            Some(member),
        ) {
            Some(v) => v,
            None => json!({"ok": false, "error": "HTTP 失败"}),
        }
    }
}

fn main() -> std::process::ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if has(&argv, "--help") || has(&argv, "-h") {
        println!("用法：group_watch [--chat-base URL] [--nats-host H:P] [--nats-user U] [--nats-pass-file F] [--state F] [--interval S] [--gap S] [--timeout S] [--include-hermes] [--no-allow-agent] [--max-chain N] [--log-file F] [--once]");
        return std::process::ExitCode::SUCCESS;
    }
    let chat_base = flag(&argv, "--chat-base")
        .or_else(|| std::env::var("AGENT_TEAM_CHAT_BASE").ok())
        .unwrap_or_else(|| CHAT_BASE_DEFAULT.to_string());
    let nats_host = flag(&argv, "--nats-host")
        .or_else(|| std::env::var("AGENT_TEAM_NATS_HOST").ok())
        .or_else(|| std::env::var("AGENT_TEAM_NATS").ok())
        .unwrap_or_else(|| NATS_HOST_DEFAULT.to_string());
    let nats_user = flag(&argv, "--nats-user").unwrap_or_else(|| "hermes".into());
    let pass_file = flag(&argv, "--nats-pass-file").unwrap_or_else(|| PASS_DEFAULT.into());
    let state_path = flag(&argv, "--state").unwrap_or_else(|| STATE_DEFAULT.into());
    let interval = fnum(&argv, "--interval", 8.0);
    let gap = fnum(&argv, "--gap", 30.0);
    let timeout = fnum(&argv, "--timeout", 600.0);
    let include_hermes = has(&argv, "--include-hermes");
    let allow_agent = !has(&argv, "--no-allow-agent");
    let max_chain: i64 = flag(&argv, "--max-chain")
        .and_then(|v| v.parse().ok())
        .unwrap_or(2);
    let log_file = flag(&argv, "--log-file");
    let once = has(&argv, "--once");

    let nats_pass = match std::fs::read_to_string(&pass_file) {
        Ok(s) => s.trim().to_string(),
        Err(e) => {
            log_line(&format!("读不到 NATS 口令: {e}"), &log_file);
            return std::process::ExitCode::from(2);
        }
    };

    let mut state = gw::WatchState::load(&state_path);
    let mut dlv = HttpDlv {
        chat_base,
        nats_host,
        nats_user,
        nats_pass,
    };
    let args = gw::RoundArgs {
        chat_base: String::new(), // one_round 用 dlv 的基址
        gap,
        timeout,
        include_hermes,
        allow_agent,
        max_chain,
    };
    log_line(
        &format!("群守望器启动 · 每 {:.0}s 一轮 · 间隔 {:.0}s", interval, gap),
        &log_file,
    );
    loop {
        let n = gw::one_round(&args, &mut state, &mut dlv, &mut |m: String| {
            log_line(&m, &log_file);
        });
        if n > 0 {
            log_line(&format!("本轮回话 {n} 条"), &log_file);
        }
        if once {
            return std::process::ExitCode::SUCCESS;
        }
        std::thread::sleep(Duration::from_secs_f64(interval));
    }
}
