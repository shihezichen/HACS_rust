//! `team_broadcast` —— 新平面(9930)群发（对应 Python `node/l1_transport/team_broadcast.py`）。
//!
//! 成员 teamd 只订自己的点名主题 → 群发 = 对名单**并行扇出**同一条 payload，
//! 各自 request/reply，再汇总成表（天然带回应）。
//! 退出码：0 全员回应；1 有成员未回应；2 参数/连接错误；3 名单为空。

use std::sync::Arc;
use std::time::{Duration, Instant};

use hacs_l1_transport::nats_client as nats;
use hacs_l1_transport::team_tools::{
    broadcast_summary, build_call_payload, clamp_call_timeout, MemberOutcome,
};
use serde_json::json;

const DEFAULT_PORT: u16 = 9930;
const HB_WINDOW_POLL_MS: u64 = 200;

fn main() {
    let mut members_arg: Option<String> = None;
    let mut all = false;
    let mut text: Option<String> = None;
    let mut text_file: Option<String> = None;
    let mut subject = "群发".to_string();
    let mut session_id: Option<String> = None;
    let mut host: Option<String> = None;
    let mut port = DEFAULT_PORT;
    let mut user = String::new();
    let mut password_file = String::new();
    let mut timeout = 120.0f64;
    let mut concurrency = 3usize;
    let mut roster_seconds = 40.0f64;
    let mut no_wait = false;
    let mut dry_run = false;
    let mut exclude = String::new();
    let mut json_out = false;
    let mut out_file: Option<String> = None;

    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--members" => members_arg = args.next(),
            "--all" => all = true,
            "--text" => text = args.next(),
            "--text-file" => text_file = args.next(),
            "--subject" => subject = args.next().unwrap_or_else(|| subject.clone()),
            "--session-id" => session_id = args.next(),
            "--nats-host" => host = args.next(),
            "--nats-port" => {
                port = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(DEFAULT_PORT)
            }
            "--user" => user = args.next().unwrap_or_default(),
            "--password-file" => password_file = args.next().unwrap_or_default(),
            "--timeout" => timeout = args.next().and_then(|v| v.parse().ok()).unwrap_or(120.0),
            "--concurrency" => concurrency = args.next().and_then(|v| v.parse().ok()).unwrap_or(3),
            "--roster-seconds" => {
                roster_seconds = args.next().and_then(|v| v.parse().ok()).unwrap_or(40.0)
            }
            "--no-wait" => no_wait = true,
            "--dry-run" => dry_run = true,
            "--exclude" => exclude = args.next().unwrap_or_default(),
            "--json" => json_out = true,
            "--out" => out_file = args.next(),
            other => {
                eprintln!("未知参数: {other}");
                std::process::exit(2);
            }
        }
    }

    let body = if let Some(f) = &text_file {
        match std::fs::read_to_string(f) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("FAIL 读正文文件失败: {e}");
                std::process::exit(2);
            }
        }
    } else {
        text.clone().unwrap_or_default()
    };
    if body.is_empty() {
        eprintln!("必须给 --text 或 --text-file");
        std::process::exit(2);
    }
    let t = clamp_call_timeout(timeout.max(1.0));
    let password = match nats::read_secret_file(&password_file) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("读口令文件失败：{e}");
            std::process::exit(2);
        }
    };
    let Some(host) = host.or_else(|| std::env::var("AGENT_TEAM_NATS_HOST").ok()) else {
        eprintln!("FAIL 缺 --nats-host（或 AGENT_TEAM_NATS_HOST）");
        std::process::exit(2);
    };

    let mut excluded: Vec<String> = exclude
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    excluded.push(user.clone()); // 不给自己发

    // ---- 名单 -------------------------------------------------------------
    let members: Vec<String> = if all {
        let client = nats::NatsClient::new(
            nats::NatsConfig::from_hosts_str(&format!("{host}:{port}"), port)
                .unwrap_or_else(|_| nats::NatsConfig::from_hosts_str("127.0.0.1", port).unwrap())
                .with_credentials(Some(user.clone()), Some(password.clone()))
                .with_name("team-broadcast-roster"),
        );
        if client.connect().is_err() {
            eprintln!("连接中枢/取名单失败");
            std::process::exit(2);
        }
        use hacs_l1_transport::team_tools::HbWindow;
        let window = Arc::new(std::sync::Mutex::new(HbWindow::new()));
        let win_cb = window.clone();
        let _ = client.subscribe(
            "team.hb.>",
            move |payload: &[u8], _reply: &str| {
                if let Ok(mut w) = win_cb.lock() {
                    w.feed(payload);
                }
            },
            "_bc_roster",
        );
        let deadline = Instant::now() + Duration::from_secs_f64(roster_seconds.max(1.0));
        while Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(HB_WINDOW_POLL_MS));
        }
        let _ = client.unsubscribe("_bc_roster");
        client.close();
        let discovered = window
            .lock()
            .map(|w| w.discover_members())
            .unwrap_or_default();
        let found: Vec<String> = discovered
            .into_iter()
            .filter(|m| !excluded.contains(m))
            .collect();
        if found.is_empty() {
            eprintln!("窗口 {:.0}s 内没有扫到任何在线成员", roster_seconds);
            std::process::exit(3);
        }
        found
    } else {
        let listed: Vec<String> = members_arg
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .filter(|m| !excluded.contains(m))
            .collect();
        if listed.is_empty() {
            eprintln!("必须给 --members 或 --all");
            std::process::exit(2);
        }
        listed
    };

    let payload = build_call_payload(&body, session_id.as_deref(), t);
    let raw = payload.to_string();

    if dry_run {
        println!(
            "【dry-run】不发任何消息。主题：{subject}　目标 {} 个：{}",
            members.len(),
            members.join(",")
        );
        let preview: String = body.chars().take(200).collect();
        println!("正文预览（前 200 字）：{}", preview.replace('\n', " "));
        return;
    }

    println!(
        "群发主题：{subject}　目标 {} 个：{}{}",
        members.len(),
        members.join(","),
        if no_wait {
            "　（只投递不等回应）".to_string()
        } else {
            format!("　（并行度 {concurrency}，单成员超时 {t:.0}s）")
        }
    );

    // ---- 并行扇出（限并发） --------------------------------------------------
    let t0 = Instant::now();
    let host_a = host.clone();
    let results: Vec<MemberOutcome> = std::thread::scope(|s| {
        let mut handles = Vec::new();
        let sem = Arc::new(Semaphore::new(concurrency.max(1)));
        for m in &members {
            let sem = sem.clone();
            let host = host_a.clone();
            let user = user.clone();
            let password = password.clone();
            let raw = raw.clone();
            let m = m.clone();
            handles.push(s.spawn(move || {
                let _permit = sem.acquire();
                if no_wait {
                    call_one_nowait(&host, port, &user, &password, &m, &raw)
                } else {
                    call_one(&host, port, &user, &password, &m, &raw, t)
                }
            }));
        }
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let total_elapsed = (t0.elapsed().as_secs_f64() * 10.0).round() / 10.0;

    for r in &results {
        println!("{}", r.summary_line());
    }
    let ok_count = results.iter().filter(|r| r.ok).count();
    println!(
        "汇总：{}/{} 回应，用时 {:.1}s",
        ok_count,
        members.len(),
        total_elapsed
    );

    let summary = broadcast_summary(&subject, &results, members.len(), no_wait, total_elapsed);
    if json_out {
        println!("{summary}");
    }
    if let Some(path) = out_file {
        if let Ok(mut f) = std::fs::File::create(&path) {
            use std::io::Write;
            let _ = writeln!(f, "{summary}");
            println!("结果已落：{path}");
        }
    }
    std::process::exit(if ok_count == members.len() { 0 } else { 1 });
}

/// 单成员：独立连接 + 一条 request（避免共享连接相互阻塞）。
fn call_one(
    host: &str,
    port: u16,
    user: &str,
    password: &str,
    member: &str,
    raw: &str,
    timeout: f64,
) -> MemberOutcome {
    let t0 = Instant::now();
    let mut out = MemberOutcome {
        member: member.to_string(),
        ..Default::default()
    };
    let client = nats::NatsClient::new(
        nats::NatsConfig::from_hosts_str(&format!("{host}:{port}"), port)
            .unwrap_or_else(|_| nats::NatsConfig::from_hosts_str("127.0.0.1", port).unwrap())
            .with_credentials(Some(user.to_string()), Some(password.to_string())),
    );
    if client.connect().is_err() {
        out.error = Some("连接失败".to_string());
        out.elapsed = secs(t0);
        return out;
    }
    let reply = client.request(
        &format!("a2a.v1.req.{member}"),
        raw.as_bytes(),
        Duration::from_secs_f64(timeout),
    );
    out.elapsed = secs(t0);
    client.close();
    match reply {
        None => out.error = Some("超时未回应".to_string()),
        Some(bytes) => {
            let text = String::from_utf8_lossy(&bytes).to_string();
            match serde_json::from_str::<serde_json::Value>(&text) {
                Ok(v) if v.is_object() => {
                    if v.get("ok") == Some(&json!(false)) {
                        out.error = Some("对方返回 ok=false".to_string());
                    } else {
                        out.ok = true;
                        out.reply = v
                            .get("reply")
                            .map(|r| r.to_string())
                            .or_else(|| Some(text.clone()));
                    }
                }
                _ => {
                    out.ok = true;
                    out.reply = Some(text);
                }
            }
        }
    }
    out
}

/// 只投递不等回应（--no-wait）。
fn call_one_nowait(
    host: &str,
    port: u16,
    user: &str,
    password: &str,
    member: &str,
    raw: &str,
) -> MemberOutcome {
    let t0 = Instant::now();
    let mut out = MemberOutcome {
        member: member.to_string(),
        ..Default::default()
    };
    let client = nats::NatsClient::new(
        nats::NatsConfig::from_hosts_str(&format!("{host}:{port}"), port)
            .unwrap_or_else(|_| nats::NatsConfig::from_hosts_str("127.0.0.1", port).unwrap())
            .with_credentials(Some(user.to_string()), Some(password.to_string())),
    );
    let sent = client.connect().is_ok()
        && client
            .publish(&format!("a2a.v1.req.{member}"), raw.as_bytes(), "")
            .is_ok()
        && client.flush(Duration::from_secs(5));
    client.close();
    out.elapsed = secs(t0);
    if sent {
        out.ok = true;
        out.reply = Some("(已投递，未等回应)".to_string());
    } else {
        out.error = Some("投递失败".to_string());
    }
    out
}

/// std 无 Semaphore——Mutex+Condvar 自实现计数信号量。
struct Semaphore {
    state: std::sync::Mutex<usize>,
    cv: std::sync::Condvar,
}

impl Semaphore {
    fn new(permits: usize) -> Self {
        Self {
            state: std::sync::Mutex::new(permits),
            cv: std::sync::Condvar::new(),
        }
    }

    fn acquire(&self) -> SemaphoreGuard<'_> {
        let mut avail = self.state.lock().unwrap();
        while *avail == 0 {
            avail = self.cv.wait(avail).unwrap();
        }
        *avail -= 1;
        SemaphoreGuard { sem: self }
    }
}

impl Drop for SemaphoreGuard<'_> {
    fn drop(&mut self) {
        let mut avail = self.sem.state.lock().unwrap();
        *avail += 1;
        self.sem.cv.notify_one();
    }
}

struct SemaphoreGuard<'a> {
    sem: &'a Semaphore,
}

fn secs(t0: Instant) -> f64 {
    (t0.elapsed().as_secs_f64() * 10.0).round() / 10.0
}
