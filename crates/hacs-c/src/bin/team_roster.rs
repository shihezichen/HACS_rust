//! `team_roster` —— 在线成员表（对应 Python `node/l1_transport/team_roster.py`）。
//!
//! 订阅 `team.hb.>` 窗口 N 秒收心跳，窗口结束按成员去重输出。
//! 退出码：0 = 有人在线；3 = 一个心跳都没收到；4 = 读口令失败。

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hacs_l1_transport::nats_client as nats;
use hacs_l1_transport::team_tools::HbWindow;
use serde_json::json;

const DEFAULT_PORT: u16 = 9930;

fn main() {
    let mut host: Option<String> = None;
    let mut port = DEFAULT_PORT;
    let mut user = String::new();
    let mut password_file = String::new();
    let mut wait = 35.0f64;
    let mut json_out = false;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--nats-host" => host = args.next(),
            "--nats-port" => {
                port = args.next().and_then(|v| v.parse().ok()).unwrap_or(DEFAULT_PORT)
            }
            "--user" => user = args.next().unwrap_or_default(),
            "--password-file" => password_file = args.next().unwrap_or_default(),
            "--wait" => wait = args.next().and_then(|v| v.parse().ok()).unwrap_or(35.0),
            "--json" => json_out = true,
            other => {
                eprintln!("未知参数: {other}");
                std::process::exit(2);
            }
        }
    }
    if host.is_none() {
        host = std::env::var("AGENT_TEAM_NATS_HOST").ok();
    }
    let Some(host) = host else {
        eprintln!("FAIL 缺 --nats-host（或 AGENT_TEAM_NATS_HOST）");
        std::process::exit(2);
    };
    if user.is_empty() || password_file.is_empty() {
        eprintln!("FAIL 需要同时给 --user 与 --password-file");
        std::process::exit(2);
    }

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
            .with_credentials(Some(user.clone()), Some(password))
            .with_name("team-roster"),
    );
    if client.connect().is_err() {
        eprintln!("FAIL 连不上 NATS {host}:{port}");
        std::process::exit(3);
    }

    let window = std::sync::Arc::new(std::sync::Mutex::new(HbWindow::new()));
    let win_cb = window.clone();
    let _ = client.subscribe("team.hb.>", move |payload: &[u8], _reply: &str| {
        if let Ok(mut w) = win_cb.lock() {
            w.feed(payload);
        }
    }, "hb");
    let _ = client.flush(Duration::from_secs(5));

    let deadline = Duration::from_secs_f64(wait.max(1.0));
    let start = std::time::Instant::now();
    while start.elapsed() < deadline {
        std::thread::sleep(Duration::from_millis(250));
    }
    client.close();

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let rows = window.lock().map(|w| w.roster_rows(now)).unwrap_or_default();

    if json_out {
        let errs = client.last_err().unwrap_or_default();
        println!(
            "{}",
            json!({"ok": !rows.is_empty(), "count": rows.len(), "members": rows, "nats_errors": [errs]})
        );
    } else if rows.is_empty() {
        println!("在线成员: 0（窗口 {:.0}s 内未收到 team.hb.> 心跳）", wait);
    } else {
        println!(
            "在线成员 {} 个（窗口 {:.0}s, 中枢 {host}:{port}）:",
            rows.len(),
            wait
        );
        println!(
            "{:<14} {:<18} {:>8} {:>10} {:>8}",
            "成员", "主机", "心跳龄s", "在线时长s", "版本"
        );
        for r in &rows {
            println!(
                "{:<14} {:<18} {:>8} {:>10} {:>8}",
                r["member"].as_str().unwrap_or(""),
                r["host"].as_str().unwrap_or(""),
                r["age_s"],
                r["uptime_s"].as_i64().map(|v| v.to_string()).unwrap_or_default(),
                r["version"].as_str().unwrap_or(""),
            );
        }
    }
    std::process::exit(if rows.is_empty() { 3 } else { 0 });
}
