//! `team_call` —— 点名要答复（对应 Python `node/l1_transport/team_call.py`）。
//!
//! 走 `a2a.v1.req.<member>`（NATS request/reply），由对方 teamd 转本机 A2A 调用。
//! 退出码：0 成功；3 超时/无应答；4 参数或口令问题。

use std::time::{Duration, Instant};

use hacs_l1_transport::nats_client as nats;
use hacs_l1_transport::team_tools::{build_call_payload, clamp_call_timeout};
use serde_json::json;

const DEFAULT_PORT: u16 = 9930;

fn main() {
    let mut member = String::new();
    let mut text: Option<String> = None;
    let mut text_file: Option<String> = None;
    let mut session_id: Option<String> = None;
    let mut timeout = 120.0f64;
    let mut host: Option<String> = None;
    let mut port = DEFAULT_PORT;
    let mut user = String::new();
    let mut password_file = String::new();
    let mut dry_run = false;
    let mut json_out = false;

    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--member" => member = args.next().unwrap_or_default(),
            "--text" => text = args.next(),
            "--text-file" => text_file = args.next(),
            "--session-id" => session_id = args.next(),
            "--timeout" => timeout = args.next().and_then(|v| v.parse().ok()).unwrap_or(120.0),
            "--nats-host" => host = args.next(),
            "--nats-port" => {
                port = args.next().and_then(|v| v.parse().ok()).unwrap_or(DEFAULT_PORT)
            }
            "--user" => user = args.next().unwrap_or_default(),
            "--password-file" => password_file = args.next().unwrap_or_default(),
            "--dry-run" => dry_run = true,
            "--json" => json_out = true,
            other => {
                eprintln!("未知参数: {other}");
                std::process::exit(2);
            }
        }
    }

    let body = if let Some(f) = &text_file {
        match std::fs::read_to_string(f) {
            Ok(t) => t.trim().to_string(),
            Err(e) => {
                eprintln!("FAIL 读正文文件失败: {e}");
                std::process::exit(4);
            }
        }
    } else {
        text.unwrap_or_default().trim().to_string()
    };
    if body.is_empty() && !dry_run {
        eprintln!("FAIL 正文为空: 给 --text 或 --text-file");
        std::process::exit(4);
    }

    let subject = format!("a2a.v1.req.{member}");
    let t = clamp_call_timeout(timeout);
    let payload = build_call_payload(&body, session_id.as_deref(), t);
    let raw = payload.to_string();

    if dry_run {
        println!(
            "{}",
            json!({"subject": subject, "bytes": raw.len(), "payload": payload})
        );
        return;
    }

    if member.is_empty() || user.is_empty() || password_file.is_empty() {
        eprintln!("FAIL 需要 --member / --user / --password-file");
        std::process::exit(4);
    }
    let Some(host) = host.or_else(|| std::env::var("AGENT_TEAM_NATS_HOST").ok()) else {
        eprintln!("FAIL 缺 --nats-host（或 AGENT_TEAM_NATS_HOST）");
        std::process::exit(2);
    };
    let password = match nats::read_secret_file(&password_file) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("FAIL 读口令失败: {e}");
            std::process::exit(4);
        }
    };

    let client = nats::NatsClient::new(
        nats::NatsConfig::from_hosts_str(&format!("{host}:{port}"), port)
            .unwrap_or_else(|_| nats::NatsConfig::from_hosts_str("127.0.0.1", port).unwrap())
            .with_credentials(Some(user), Some(password))
            .with_name("team-call"),
    );
    if client.connect().is_err() {
        eprintln!("FAIL 连不上 NATS {host}:{port}");
        std::process::exit(3);
    }

    let t0 = Instant::now();
    let reply = client.request(
        &subject,
        raw.as_bytes(),
        Duration::from_secs_f64(t + 10.0),
    );
    let elapsed = (t0.elapsed().as_secs_f64() * 10.0).round() / 10.0;
    client.close();

    match reply {
        None => {
            let out = json!({
                "ok": false, "member": member, "elapsed_s": elapsed,
                "error": "timeout 或对端无应答",
            });
            if json_out {
                println!("{out}");
            } else {
                println!("TIMEOUT {elapsed}s 无应答: {member}");
            }
            std::process::exit(3);
        }
        Some(bytes) => {
            let text_out = String::from_utf8_lossy(&bytes).to_string();
            if json_out {
                match serde_json::from_str::<serde_json::Value>(&text_out) {
                    Ok(v) => println!(
                        "{}",
                        json!({"ok": true, "member": member, "elapsed_s": elapsed, "reply": v})
                    ),
                    Err(_) => println!(
                        "{}",
                        json!({"ok": true, "member": member, "elapsed_s": elapsed, "reply_raw": text_out})
                    ),
                }
            } else {
                println!("OK {elapsed}s 来自 {member}:");
                println!("{text_out}");
            }
        }
    }
}
