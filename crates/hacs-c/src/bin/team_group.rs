//! `team_group` —— 成员侧协作域 CLI（对应 Python `hub/l3_domain/team_group.py`
//! 的 cmd_* + argparse 装配）。
//!
//! 子命令（argv 风格与 Python 对齐）：
//! * `meet   --goal --agenda(可多条) --members --timeout --session-id --out
//!           --dedup-file --force --concurrency --json`：NATS 并发点名召集会议；
//! * `history --file --session-id --json`：回放本地留痕；
//! * `say    <group> --as --to --text/--text-file --reply-to --client-msg-id`：
//!           群发言（必须 @，走对话台 POST /messages）；
//! * `pull   <group> --as --limit --since`：拉全群增量 + 我欠谁回应；
//! * `show   <group> --as`：查群元数据。
//!
//! 退出码沿用 Python：0 成功；1 有缺席/失败；2 参数错。

use std::time::Duration;

use hacs_l3_domain::team_group as tg;
use serde_json::{json, Value};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = args.first().cloned() else {
        usage();
        std::process::exit(2);
    };
    let code = match cmd.as_str() {
        "meet" => cmd_meet(&args[1..]),
        "history" => cmd_history(&args[1..]),
        "say" => cmd_group_say(&args[1..]),
        "pull" => cmd_group_pull(&args[1..]),
        "show" => cmd_group_show(&args[1..]),
        "--help" | "-h" => {
            usage();
            0
        }
        other => {
            eprintln!("未知子命令: {other}");
            usage();
            2
        }
    };
    std::process::exit(code);
}

fn usage() {
    eprintln!(
        "用法: team_group <meet|history|say|pull|show> [参数]\n\
         \x20 meet    --goal G --agenda A [--agenda B...] --members a,b [--timeout 120]\n\
         \x20                 [--session-id S] [--out F] [--dedup-file F] [--force]\n\
         \x20                 [--concurrency 4] [--nats-host H] [--nats-port 9930]\n\
         \x20                 [--user U] [--password-file F] [--json]\n\
         \x20 history --file F --session-id S [--json]\n\
         \x20 say     <group> [--as M] [--to @all|a,b] --text T|--text-file F\n\
         \x20                 [--reply-to N] [--client-msg-id X] [--chat-base URL] [--json]\n\
         \x20 pull    <group> [--as M] [--limit 50] [--since N] [--chat-base URL] [--json]\n\
         \x20 show    <group> [--as M] [--chat-base URL] [--json]"
    );
}

// ---------------------------------------------------------------- 参数小工具

struct Flags {
    map: std::collections::HashMap<String, String>,
    multi: std::collections::HashMap<String, Vec<String>>,
    pos: Vec<String>,
    flags: std::collections::HashSet<String>,
}

fn parse_flags(args: &[String]) -> Flags {
    let mut map = std::collections::HashMap::new();
    let mut multi: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    let mut pos = Vec::new();
    let mut flags = std::collections::HashSet::new();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if let Some(name) = a.strip_prefix("--") {
            match args.get(i + 1) {
                Some(v) if !v.starts_with("--") => {
                    if name == "agenda" {
                        multi.entry(name.to_string()).or_default().push(v.clone());
                    }
                    map.insert(name.to_string(), v.clone());
                    i += 2;
                    continue;
                }
                _ => {
                    flags.insert(name.to_string());
                    i += 1;
                    continue;
                }
            }
        }
        pos.push(a.clone());
        i += 1;
    }
    Flags {
        map,
        multi,
        pos,
        flags,
    }
}

impl Flags {
    fn get(&self, k: &str) -> Option<&str> {
        self.map.get(k).map(String::as_str)
    }
    fn has(&self, k: &str) -> bool {
        self.flags.contains(k)
    }
}

fn default_chat_base() -> String {
    std::env::var("AGENT_TEAM_CHAT_BASE")
        .or_else(|_| std::env::var("TEAM_CHAT_BASE"))
        .unwrap_or_else(|_| hacs_l0_host::node_profile::chat_base_default())
}

fn read_text_arg(f: &Flags) -> Result<String, String> {
    if let Some(t) = f.get("text") {
        return Ok(t.to_string());
    }
    if let Some(file) = f.get("text-file") {
        return std::fs::read_to_string(file).map_err(|e| format!("读正文文件失败: {e}"));
    }
    if !f.pos.is_empty() {
        return Ok(f.pos.join(" "));
    }
    Err("发言不能为空：给位置参数 text 或 --text-file".into())
}

// ---------------------------------------------------------------- meet

fn cmd_meet(args: &[String]) -> i32 {
    let f = parse_flags(args);
    let member = f.get("member").unwrap_or("").to_string();
    let user = f.get("user").unwrap_or("").to_string();
    let password_file = f.get("password-file").unwrap_or("").to_string();
    if member.is_empty() || user.is_empty() || password_file.is_empty() {
        eprintln!("FAIL 需要 --member / --user / --password-file");
        return 2;
    }
    let Some(goal) = f.get("goal").map(str::trim).filter(|g| !g.is_empty()) else {
        eprintln!("必须给 --goal");
        return 2;
    };
    let agenda: Vec<String> = f
        .multi
        .get("agenda")
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|a| !a.trim().is_empty())
        .collect();
    if agenda.is_empty() {
        eprintln!("必须至少给一条 --agenda");
        return 2;
    }
    let mut members: Vec<String> = f
        .get("members")
        .unwrap_or("")
        .split(',')
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .map(String::from)
        .collect();
    members.sort();
    members.dedup();
    if members.is_empty() {
        eprintln!("必须给 --members");
        return 2;
    }

    let now = now_secs();
    let session_id = f
        .get("session-id")
        .map(String::from)
        .unwrap_or_else(|| tg::make_session_id(now, &rand4()));
    let timeout = f
        .get("timeout")
        .and_then(|v| v.parse::<f64>().ok())
        .map(hacs_l1_transport::team_tools::clamp_call_timeout)
        .unwrap_or(120.0);
    let body = tg::build_meeting_body(goal, &agenda, &members, &session_id);

    let dedup_file = f
        .get("dedup-file")
        .map(String::from)
        .unwrap_or_else(tg::default_meetings_file);
    let subject_norm = format!("meet:{}", members.join(","));
    let (should_send, key) =
        tg::check_and_mark_dedup(&dedup_file, &subject_norm, &body, f.has("force"), now);
    if !should_send {
        println!("本窗口内已发过同一内容，跳过（用 --force 强制重发）");
        return 0;
    }

    let envelope = json!({
        "text": body, "session_id": session_id,
        "timeout_s": timeout, "from": member,
    });
    let payload = envelope.to_string();

    println!(
        "召集会议 session_id={}\u{3000}参会 {} 人：{}",
        session_id,
        members.len(),
        members.join(",")
    );

    let Some(host) = f
        .get("nats-host")
        .map(String::from)
        .or_else(|| std::env::var("AGENT_TEAM_NATS_HOST").ok())
    else {
        eprintln!("FAIL 缺 --nats-host（或 AGENT_TEAM_NATS_HOST）");
        tg::rollback_dedup(&dedup_file, &key);
        return 2;
    };
    let port = f
        .get("nats-port")
        .and_then(|v| v.parse().ok())
        .unwrap_or(9930u16);
    let Ok(password) = hacs_l1_transport::nats_client::read_secret_file(&password_file) else {
        eprintln!("FAIL 读口令失败: {password_file}");
        tg::rollback_dedup(&dedup_file, &key);
        return 2;
    };
    let concurrency = f
        .get("concurrency")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(4)
        .max(1);

    let results = fan_out_calls(
        &host,
        port,
        &user,
        &password,
        &member,
        &members,
        &payload,
        timeout,
        concurrency,
    );
    if results.is_empty() {
        tg::rollback_dedup(&dedup_file, &key);
        eprintln!("召集失败，已回滚去重标记");
        return 2;
    }

    println!("目标：{goal}");
    for (i, item) in agenda.iter().enumerate() {
        println!("  议程 {}：{}", i + 1, item);
    }
    println!("{}", "-".repeat(60));
    print!("{}", tg::meet_report(&results, members.len()));

    let record = json!({
        "session_id": session_id,
        "ts": beijing_ts(now),
        "convener": member, "goal": goal,
        "agenda": agenda, "members": members, "body": body,
        "results": results_present(&results),
        "present": results.iter()
            .filter(|r| r.get("ok") == Some(&json!(true)))
            .filter_map(|r| r.get("member").cloned())
            .collect::<Vec<Value>>(),
        "absent": results.iter()
            .filter(|r| r.get("ok") != Some(&json!(true)))
            .filter_map(|r| r.get("member").cloned())
            .collect::<Vec<Value>>(),
    });
    let out = f
        .get("out")
        .map(String::from)
        .unwrap_or_else(tg::default_meetings_file);
    if let Err(e) = tg::append_jsonl(&out, &record) {
        eprintln!("警告：本地留痕写入失败（不影响本次会议结果）：{e}");
    }
    if f.has("json") {
        println!(
            "{}",
            serde_json::to_string_pretty(&record).unwrap_or_default()
        );
    }
    let absent = record["absent"].as_array().map(Vec::len).unwrap_or(0);
    if absent > 0 {
        println!();
        1
    } else {
        0
    }
}

/// 并发点名（Python `run_pool(call_one, ...)`）：每成员一条线程，subject =
/// `a2a.v1.req.<member>`。线程数 = min(concurrency, 成员数)。
fn fan_out_calls(
    host: &str,
    port: u16,
    user: &str,
    password: &str,
    _from: &str,
    members: &[String],
    payload: &str,
    timeout: f64,
    concurrency: usize,
) -> Vec<Value> {
    use std::sync::{Arc, Mutex};
    let results: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let queue: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(members.to_vec()));
    let nthreads = concurrency.min(members.len().max(1));
    let mut handles = Vec::new();
    for _ in 0..nthreads {
        let queue = Arc::clone(&queue);
        let results = Arc::clone(&results);
        let host = host.to_string();
        let user = user.to_string();
        let password = password.to_string();
        let payload = payload.to_string();
        handles.push(std::thread::spawn(move || loop {
            let Some(m) = queue.lock().unwrap().pop() else {
                break;
            };
            let out = call_one(&host, port, &user, &password, &m, &payload, timeout);
            results.lock().unwrap().push(out);
        }));
    }
    for h in handles {
        let _ = h.join();
    }
    let mut out = results.lock().unwrap().clone();
    // 与 Python 一致：results 顺序 = members 顺序（run_pool 保序）
    out.sort_by_key(|r| {
        members
            .iter()
            .position(|m| Some(m.as_str()) == r.get("member").and_then(Value::as_str))
            .unwrap_or(usize::MAX)
    });
    out
}

/// 单次点名：连接 → request → 解析 reply。失败也产出一条 `{"ok":false}` 行。
fn call_one(
    host: &str,
    port: u16,
    user: &str,
    password: &str,
    member: &str,
    payload: &str,
    timeout: f64,
) -> Value {
    let client = hacs_l1_transport::nats_client::NatsClient::new(
        hacs_l1_transport::nats_client::NatsConfig::from_hosts_str(&format!("{host}:{port}"), port)
            .unwrap_or_else(|_| {
                hacs_l1_transport::nats_client::NatsConfig::from_hosts_str("127.0.0.1", port)
                    .unwrap()
            })
            .with_credentials(Some(user.to_string()), Some(password.to_string()))
            .with_name("team-group"),
    );
    if client.connect().is_err() {
        return json!({"ok": false, "member": member, "elapsed": 0.0, "error": "连不上 NATS"});
    }
    let t0 = std::time::Instant::now();
    let reply = client.request(
        &format!("a2a.v1.req.{member}"),
        payload.as_bytes(),
        Duration::from_secs_f64(timeout + 10.0),
    );
    let elapsed = (t0.elapsed().as_secs_f64() * 10.0).round() / 10.0;
    client.close();
    match reply {
        Some(raw) => {
            let parsed: Value = serde_json::from_slice(&raw).unwrap_or(Value::Null);
            let ok = parsed.get("ok").and_then(Value::as_bool).unwrap_or(false);
            let rep = parsed
                .get("text")
                .or_else(|| parsed.get("reply"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if ok {
                json!({"ok": true, "member": member, "elapsed": elapsed, "reply": rep})
            } else {
                json!({
                    "ok": false, "member": member, "elapsed": elapsed,
                    "error": rep.is_empty().then(|| "对端拒绝".to_string()).unwrap_or(rep),
                })
            }
        }
        None => json!({
            "ok": false, "member": member, "elapsed": elapsed,
            "error": "timeout 或对端无应答",
        }),
    }
}

fn results_present(results: &[Value]) -> Vec<Value> {
    results.to_vec()
}

// ---------------------------------------------------------------- history

fn cmd_history(args: &[String]) -> i32 {
    let f = parse_flags(args);
    let Some(file) = f.get("file") else {
        eprintln!("必须给 --file");
        return 2;
    };
    let Some(session_id) = f.get("session-id") else {
        eprintln!("必须给 --session-id");
        return 2;
    };
    let Some(found) = tg::find_last_by_session(file, session_id) else {
        eprintln!("没找到 session_id={session_id} 的会议记录（文件：{file}）");
        return 1;
    };
    if f.has("json") {
        println!(
            "{}",
            serde_json::to_string_pretty(&found).unwrap_or_default()
        );
        return 0;
    }
    println!(
        "会议 session_id={}\u{3000}召集人={}\u{3000}时间={}",
        found
            .get("session_id")
            .and_then(Value::as_str)
            .unwrap_or(""),
        found.get("convener").and_then(Value::as_str).unwrap_or(""),
        found.get("ts").and_then(Value::as_str).unwrap_or(""),
    );
    println!(
        "目标：{}",
        found.get("goal").and_then(Value::as_str).unwrap_or("")
    );
    if let Some(agenda) = found.get("agenda").and_then(Value::as_array) {
        for (i, item) in agenda.iter().enumerate() {
            println!("  议程 {}：{}", i + 1, item.as_str().unwrap_or(""));
        }
    }
    println!("{}", "-".repeat(60));
    if let Some(results) = found.get("results").and_then(Value::as_array) {
        for r in results {
            let tag = if r.get("ok") == Some(&json!(true)) {
                "到会"
            } else {
                "缺席"
            };
            let rep = r
                .get("reply")
                .or_else(|| r.get("error"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .replace('\n', " ");
            println!(
                "  [{tag}] {:<14}  {}",
                r.get("member").and_then(Value::as_str).unwrap_or(""),
                rep
            );
        }
    }
    let present = found
        .get("present")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    let total = found
        .get("members")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    println!("汇总：到会 {present} / 应到 {total}");
    0
}

// ---------------------------------------------------------------- 群

fn chat_base_of(f: &Flags) -> String {
    f.get("chat-base")
        .map(String::from)
        .unwrap_or_else(default_chat_base)
}

/// 群里的"我是谁"：`--as` 优先；读操作推断失败给 None（服务端按匿名 tailnet 处理）。
fn as_member(f: &Flags, required: bool) -> Result<Option<String>, String> {
    if let Some(w) = f.get("as") {
        return Ok(Some(w.to_string()));
    }
    match infer_member() {
        Ok(m) => Ok(Some(m)),
        Err(e) => {
            if required {
                Err(e)
            } else {
                Ok(None)
            }
        }
    }
}

fn cmd_group_say(args: &[String]) -> i32 {
    let f = parse_flags(args);
    let Some(group) = f.pos.first() else {
        eprintln!("必须给群 session_id（位置参数）");
        return 2;
    };
    let member = match as_member(&f, true) {
        Ok(Some(m)) => m,
        Ok(None) => {
            eprintln!("认不出我是谁：加 --as <成员名>");
            return 2;
        }
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    let text = match read_text_arg(&f) {
        Ok(t) if !t.trim().is_empty() => t,
        Ok(_) => {
            eprintln!("发言不能为空：给位置参数 text 或 --text-file");
            return 2;
        }
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    let mentions = match tg::normalize_mentions(f.get("to")) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    let body = json!({
        "text": text,
        "mentions": mentions,
        "reply_to": f.get("reply-to").and_then(|v| v.parse::<i64>().ok()),
        "client_msg_id": f.get("client-msg-id"),
    });
    let (status, resp) = tg::chat_call(
        &chat_base_of(&f),
        &format!("/api/sessions/{group}/messages"),
        "POST",
        Some(&body),
        Some(&member),
        Duration::from_secs(15),
    );
    if status != 201 {
        println!("发言失败：{}", tg::chat_error(status, &resp));
        if f.has("json") {
            println!(
                "{}",
                serde_json::to_string_pretty(&resp).unwrap_or_default()
            );
        }
        return 1;
    }
    let msg = resp.get("message").cloned().unwrap_or(Value::Null);
    if f.has("json") {
        println!("{}", serde_json::to_string_pretty(&msg).unwrap_or_default());
        return 0;
    }
    let mentions_str = msg
        .pointer("/meta/mentions")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join("、")
        })
        .unwrap_or_default();
    println!(
        "[{}] {} → {}",
        msg.get("seq").and_then(Value::as_i64).unwrap_or(0),
        member,
        mentions_str
    );
    println!(
        "已落群：seq={} id={}",
        msg.get("seq").and_then(Value::as_i64).unwrap_or(0),
        msg.get("message_id").and_then(Value::as_str).unwrap_or("")
    );
    0
}

fn cmd_group_pull(args: &[String]) -> i32 {
    let f = parse_flags(args);
    let Some(group) = f.pos.first() else {
        eprintln!("必须给群 session_id（位置参数）");
        return 2;
    };
    let member = as_member(&f, false).unwrap_or(None);
    if member.is_none() {
        println!("（没认出我是谁：加 --as <成员名> 才能看到“我欠谁回应”）");
    }
    let limit = f
        .get("limit")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(50)
        .max(1);
    let mut query = format!("?limit={limit}");
    if let Some(since) = f.get("since").and_then(|v| v.parse::<i64>().ok()) {
        query.push_str(&format!("&after_seq={since}"));
    }
    let (status, resp) = tg::chat_call(
        &chat_base_of(&f),
        &format!("/api/sessions/{group}/messages{query}"),
        "GET",
        None,
        member.as_deref(),
        Duration::from_secs(15),
    );
    if status != 200 {
        println!("拉取失败：{}", tg::chat_error(status, &resp));
        return 1;
    }
    if f.has("json") {
        println!(
            "{}",
            serde_json::to_string_pretty(&resp).unwrap_or_default()
        );
        return 0;
    }
    let messages = resp
        .get("messages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    println!(
        "群 {group} 共 {} 条（我是 {}）",
        messages.len(),
        member.as_deref().unwrap_or("?")
    );
    for msg in &messages {
        println!("{}", tg::pull_line(member.as_deref(), msg));
    }
    let pending = resp
        .get("my_pending")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if pending.is_empty() {
        println!("\n没有欠回应。");
    } else {
        println!("\n我还欠 {} 条回应（@我 且我没回过）：", pending.len());
        for item in &pending {
            println!(
                "  seq {} 来自 {}：{}",
                item.get("seq").and_then(Value::as_i64).unwrap_or(0),
                item.get("from").and_then(Value::as_str).unwrap_or(""),
                item.get("text").and_then(Value::as_str).unwrap_or("")
            );
        }
        println!("（回的时候记得 @ 对方，规则 6）");
    }
    0
}

fn cmd_group_show(args: &[String]) -> i32 {
    let f = parse_flags(args);
    let Some(group) = f.pos.first() else {
        eprintln!("必须给群 session_id（位置参数）");
        return 2;
    };
    let member = as_member(&f, false).unwrap_or(None);
    let (status, resp) = tg::chat_call(
        &chat_base_of(&f),
        &format!("/api/sessions/{group}"),
        "GET",
        None,
        member.as_deref(),
        Duration::from_secs(15),
    );
    if status != 200 {
        println!("查群失败：{}", tg::chat_error(status, &resp));
        return 1;
    }
    if f.has("json") {
        println!(
            "{}",
            serde_json::to_string_pretty(&resp).unwrap_or_default()
        );
        return 0;
    }
    let sess = resp.get("session").cloned().unwrap_or(Value::Null);
    println!(
        "群：{}",
        sess.get("title").and_then(Value::as_str).unwrap_or("")
    );
    println!(
        "  id     ：{}",
        sess.get("session_id").and_then(Value::as_str).unwrap_or("")
    );
    let state = sess.get("state").and_then(Value::as_str).unwrap_or("");
    println!(
        "  状态   ：{}{}",
        state,
        if state != "open" {
            "（已结束，只读）"
        } else {
            ""
        }
    );
    println!(
        "  发起人 ：{}",
        sess.get("created_by").and_then(Value::as_str).unwrap_or("")
    );
    let members: Vec<String> = sess
        .get("members")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|m| m.get("id").and_then(Value::as_str))
                .map(String::from)
                .collect()
        })
        .unwrap_or_default();
    println!("  成员   ：{}", members.join("、"));
    0
}

// ---------------------------------------------------------------- 小工具

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn rand4() -> String {
    let mut b = [0u8; 4];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        use std::io::Read;
        let _ = f.read_exact(&mut b);
    }
    const CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    b.iter()
        .map(|&x| CHARS[(x as usize) % CHARS.len()] as char)
        .collect()
}

/// `YYYY-MM-DD HH:MM:SS` 北京时间（对齐 Python `time.strftime` 本地默认——
/// 部署机时区为 CST，这里固定 +08:00 口径与 workspace 其余件一致）。
fn beijing_ts(now: i64) -> String {
    let (y, mo, d, h, mi) = tg::beijing_parts_pub(now);
    let s = (now + 8 * 3600).rem_euclid(86400) % 60;
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}")
}

/// Linux 身份推断（对齐 `_infer_member_linux`）：nats_dir 下恰好一个 `*.pass`
/// → 成员名；0 个 / 多个都报错让显式 `--as`。
fn infer_member() -> Result<String, String> {
    let d = hacs_l0_host::node_paths::nats_dir(None);
    let mut names: Vec<String> = std::fs::read_dir(&d)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|n| n.ends_with(".pass"))
                .map(|n| n[..n.len() - 5].to_string())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    match names.len() {
        1 => Ok(names.remove(0)),
        0 => Err(format!(
            "自动推断身份失败：{d} 下没有任何 *.pass 文件，请显式加 --as <成员名>"
        )),
        _ => Err(format!(
            "自动推断身份失败：{d} 下有多个口令文件（{}），无法确定你是谁，请显式加 --as <成员名>",
            names.join(", ")
        )),
    }
}
