//! `team` —— 成员侧统一通信 CLI 组合根（对应 Python node/c/team.py）。
//!
//! 子命令：who / say / broadcast / meet / history / group {say|pull|show}。
//! who/say/broadcast 的通信层实现在本 bin 内联（Python 下沉到 team_cli_core.py 的那套，
//! Rust 侧 L1 `team_tools` 已有部分原语，缺的在组装箱内补齐——契约面逐条对齐）；
//! meet/history/group 复用 L3 `team_group` 纯逻辑（与 `team_group` bin 同源）。
//!
//! 退出码约定：0 成功 / 1 无应答或缺席 / 2 参数或连接错误 / 3 对方回错误结构或名单为空。
#![allow(clippy::too_many_arguments)]

use hacs_l1_transport::nats_client::{NatsClient, NatsConfig};
use serde_json::{json, Value};
use std::time::Duration;

const MAX_TIMEOUT: f64 = 300.0;
const DEFAULT_TIMEOUT: f64 = 120.0;
const DEFAULT_PORT: u16 = 9930;
const HB_PREFIX: &str = "team.hb.";
const DEDUP_WINDOW: f64 = 30.0 * 60.0;

fn nats_hosts_default() -> String {
    hacs_l0_host::node_profile::nats_hosts_default()
}
fn default_dedup_file() -> String {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    format!("{home}/.team_broadcast_dedup.json")
}
fn default_meetings_file() -> String {
    hacs_l3_domain::team_group::default_meetings_file()
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = args.first().cloned() else {
        usage();
        std::process::exit(2);
    };
    let rest: Vec<String> = args[1..].to_vec();
    let code = match cmd.as_str() {
        "who" => cmd_who(&rest),
        "say" => cmd_say(&rest),
        "broadcast" => cmd_broadcast(&rest),
        "meet" => tg_cmd_meet(&rest),
        "history" => tg_cmd_history(&rest),
        "group" => {
            let Some(sub) = rest.first().cloned() else {
                eprintln!("group 需要 say|pull|show 子命令");
                usage();
                std::process::exit(2);
            };
            match sub.as_str() {
                "say" => tg_cmd_group_say(&rest[1..]),
                "pull" => tg_cmd_group_pull(&rest[1..]),
                "show" => tg_cmd_group_show(&rest[1..]),
                other => {
                    eprintln!("未知 group 子命令：{other}");
                    2
                }
            }
        }
        "--help" | "-h" => {
            usage();
            0
        }
        other => {
            eprintln!("未知子命令：{other}");
            usage();
            2
        }
    };
    std::process::exit(code);
}

fn usage() {
    eprintln!(
        "用法: team <who|say|broadcast|meet|history|group> [参数]\n\
         \x20 who        [--seconds 35] [--json] 共通参数\n\
         \x20 say        <target> [text|--text-file F] [--session-id S|--new-session] 共通参数\n\
         \x20 broadcast  [text|--text-file F] --all|--to a,b [--session-id S] [--concurrency 3] 共通参数\n\
         \x20 meet       --goal G --agenda A --members a,b [--concurrency 3] [--out F] 共通参数\n\
         \x20 history    --session-id S [--file F]\n\
         \x20 group say  <group> --text T|--text-file F [--to @all] [--chat-base URL] 共通参数\n\
         \x20 group pull <group> [--since N] [--limit 50] [--chat-base URL] 共通参数\n\
         \x20 group show <group> [--chat-base URL] 共通参数\n\
         共通参数：[--as M] [--password-file F] [--nats-host H] [--nats-port 9930] [--timeout 120] [--json]"
    );
}

// ---------------------------------------------------------------- 参数解析

struct Opts {
    as_member: Option<String>,
    password_file: Option<String>,
    nats_host: String,
    nats_port: u16,
    timeout: f64,
    json: bool,
    seconds: f64,
    text: Option<String>,
    text_file: Option<String>,
    session_id: Option<String>,
    new_session: bool,
    all: bool,
    to: Option<String>,
    concurrency: usize,
    roster_seconds: f64,
    dedup_file: String,
    force: bool,
    out: String,
    goal: Option<String>,
    agenda: Vec<String>,
    members: Option<String>,
    file: Option<String>,
    group: Option<String>,
    reply_to: Option<i64>,
    client_msg_id: Option<String>,
    chat_base: String,
    limit: i64,
    since: Option<i64>,
}

fn parse_flags(args: &[String]) -> Opts {
    let mut o = Opts {
        as_member: None,
        password_file: None,
        nats_host: nats_hosts_default(),
        nats_port: DEFAULT_PORT,
        timeout: DEFAULT_TIMEOUT,
        json: false,
        seconds: 35.0,
        text: None,
        text_file: None,
        session_id: None,
        new_session: false,
        all: false,
        to: None,
        concurrency: 3,
        roster_seconds: 40.0,
        dedup_file: default_dedup_file(),
        force: false,
        out: default_meetings_file(),
        goal: None,
        agenda: Vec::new(),
        members: None,
        file: None,
        group: None,
        reply_to: None,
        client_msg_id: None,
        chat_base: "http://127.0.0.1:9980".into(),
        limit: 50,
        since: None,
    };
    let mut positional: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        let take = |i: &mut usize| -> Option<String> {
            *i += 1;
            args.get(*i).cloned()
        };
        match a.as_str() {
            "--as" => o.as_member = take(&mut i),
            "--password-file" => o.password_file = take(&mut i),
            "--nats-host" => o.nats_host = take(&mut i).unwrap_or(o.nats_host),
            "--nats-port" => {
                o.nats_port = take(&mut i)
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(o.nats_port)
            }
            "--timeout" => {
                o.timeout = take(&mut i)
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(o.timeout)
            }
            "--seconds" => {
                o.seconds = take(&mut i)
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(o.seconds)
            }
            "--text" => o.text = take(&mut i),
            "--text-file" => o.text_file = take(&mut i),
            "--session-id" => o.session_id = take(&mut i),
            "--new-session" => o.new_session = true,
            "--all" => o.all = true,
            "--to" => o.to = take(&mut i),
            "--concurrency" => {
                o.concurrency = take(&mut i)
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(o.concurrency)
            }
            "--roster-seconds" => {
                o.roster_seconds = take(&mut i)
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(o.roster_seconds)
            }
            "--dedup-file" => o.dedup_file = take(&mut i).unwrap_or(o.dedup_file),
            "--force" => o.force = true,
            "--out" => o.out = take(&mut i).unwrap_or(o.out),
            "--goal" => o.goal = take(&mut i),
            "--agenda" => {
                if let Some(v) = take(&mut i) {
                    o.agenda.push(v);
                }
            }
            "--members" => o.members = take(&mut i),
            "--file" => o.file = take(&mut i),
            "--group" => o.group = take(&mut i),
            "--reply-to" => o.reply_to = take(&mut i).and_then(|v| v.parse().ok()),
            "--client-msg-id" => o.client_msg_id = take(&mut i),
            "--chat-base" => o.chat_base = take(&mut i).unwrap_or(o.chat_base),
            "--limit" => o.limit = take(&mut i).and_then(|v| v.parse().ok()).unwrap_or(o.limit),
            "--since" => o.since = take(&mut i).and_then(|v| v.parse().ok()),
            "--json" => o.json = true,
            other if other.starts_with("--") => {}
            other => positional.push(other.to_string()),
        }
        i += 1;
    }
    if o.text.is_none() && !positional.is_empty() {
        o.text = Some(positional.join(" "));
    }
    if o.group.is_none() && !positional.is_empty() {
        o.group = Some(positional[0].clone());
    }
    o
}

fn clamp_timeout(raw: f64) -> f64 {
    raw.clamp(1.0, MAX_TIMEOUT)
}

fn read_text(o: &Opts) -> Option<String> {
    if let Some(f) = &o.text_file {
        return std::fs::read_to_string(f).ok();
    }
    o.text.clone()
}

// ---------------------------------------------------------------- 身份与口令

fn infer_member() -> Result<String, String> {
    // Linux 口径：扫 nats_dir/*.pass，要求恰好一个
    let d = hacs_l0_host::node_paths::nats_dir(None);
    let names: Vec<String> = std::fs::read_dir(&d)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|n| n.ends_with(".pass"))
                .map(|n| n[..n.len() - 5].to_string())
                .collect()
        })
        .unwrap_or_default();
    match names.len() {
        1 => Ok(names[0].clone()),
        0 => Err(format!(
            "自动推断身份失败：{d} 下没有任何 *.pass 文件，请显式加 --as <成员名>"
        )),
        _ => Err(format!(
            "自动推断身份失败：{d} 下有多个口令文件（{}），无法确定你是谁，请显式加 --as <成员名>",
            {
                let mut s = names.clone();
                s.sort();
                s.join(", ")
            }
        )),
    }
}

fn resolve_password_file(o: &Opts, member: &str) -> String {
    if let Some(f) = &o.password_file {
        return f.clone();
    }
    let d = hacs_l0_host::node_paths::nats_dir(None);
    format!("{d}/{member}.pass")
}

fn resolve_member_and_password(o: &Opts) -> Option<(String, String)> {
    let member = match o.as_member.clone() {
        Some(m) => m,
        None => match infer_member() {
            Ok(m) => m,
            Err(e) => {
                eprintln!("{e}");
                return None;
            }
        },
    };
    let pf = resolve_password_file(o, &member);
    match hacs_l1_transport::nats_client::read_secret_file(&pf) {
        Ok(p) => Some((member, p)),
        Err(e) => {
            eprintln!("读口令文件失败：{e}");
            None
        }
    }
}

fn mk_client(o: &Opts, member: &str, password: &str, name: &str) -> NatsClient {
    let cfg = NatsConfig::from_hosts_str(&o.nats_host, o.nats_port)
        .unwrap_or_else(|_| NatsConfig::from_hosts_str("127.0.0.1", o.nats_port).unwrap())
        .with_credentials(Some(member.to_string()), Some(password.to_string()))
        .with_name(name);
    NatsClient::new(cfg)
}

// ---------------------------------------------------------------- 去重节流

fn dedup_key(subject_norm: &str, body: &str) -> String {
    hacs_l3_domain::team_group::dedup_key(subject_norm, body)
}

fn load_dedup(path: &str) -> serde_json::Map<String, Value> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
}

fn save_dedup(path: &str, data: &serde_json::Map<String, Value>) {
    let tmp = format!("{path}.tmp-{}", std::process::id());
    if std::fs::write(&tmp, Value::Object(data.clone()).to_string()).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

/// 命中窗口内已发过 → (false, key)；否则先标记再 (true, key)。失败回滚用 rollback_dedup。
fn check_and_mark_dedup(
    dedup_file: &str,
    subject_norm: &str,
    body: &str,
    force: bool,
) -> (bool, String) {
    let key = dedup_key(subject_norm, body);
    let mut data = load_dedup(dedup_file);
    let now = now_epoch();
    data.retain(|_, v| now - v.as_f64().unwrap_or(0.0) < DEDUP_WINDOW);
    if !force {
        if let Some(ts) = data.get(&key).and_then(Value::as_f64) {
            if now - ts < DEDUP_WINDOW {
                return (false, key);
            }
        }
    }
    data.insert(key.clone(), json!(now));
    save_dedup(dedup_file, &data);
    (true, key)
}

fn rollback_dedup(dedup_file: &str, key: &str) {
    let mut data = load_dedup(dedup_file);
    data.remove(key);
    save_dedup(dedup_file, &data);
}

fn now_epoch() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

// ---------------------------------------------------------------- who

fn collect_heartbeats(client: &NatsClient, seconds: f64) -> serde_json::Map<String, Value> {
    let seen = std::sync::Arc::new(std::sync::Mutex::new(serde_json::Map::new()));
    let seen2 = seen.clone();
    let _ = client.subscribe(
        &format!("{HB_PREFIX}>"),
        move |payload: &[u8], _reply: &str| {
            if let Ok(obj) = serde_json::from_slice::<Value>(payload) {
                let member = obj
                    .get("member")
                    .and_then(Value::as_str)
                    .unwrap_or("?")
                    .to_string();
                seen2.lock().unwrap().insert(member, obj);
            }
        },
        "_team_who",
    );
    let deadline = now_epoch() + seconds.max(1.0);
    while now_epoch() < deadline {
        std::thread::sleep(Duration::from_millis(200));
    }
    let _ = client.unsubscribe("_team_who");
    let m = seen.lock().unwrap().clone();
    m
}

fn cmd_who(args: &[String]) -> i32 {
    let o = parse_flags(args);
    let Some((member, password)) = resolve_member_and_password(&o) else {
        return 2;
    };
    let client = mk_client(&o, &member, &password, "team-who");
    if client.connect().is_err() {
        eprintln!("连接 NATS 中枢失败：{}", o.nats_host);
        return 2;
    }
    let seen = collect_heartbeats(&client, o.seconds);
    client.close();
    if o.json {
        for m in seen.keys() {
            println!("{}", seen[m]);
        }
        return if seen.is_empty() { 1 } else { 0 };
    }
    if seen.is_empty() {
        println!(
            "本窗口 {:.0}s 内没收到任何心跳（检查中枢是否在跑、口令是否正确）",
            o.seconds
        );
        return 1;
    }
    println!("在线成员 {} 个（窗口 {:.0}s）：", seen.len(), o.seconds);
    println!("成员|主机名|心跳距今|版本");
    let now = now_epoch() as i64;
    let mut keys: Vec<&String> = seen.keys().collect();
    keys.sort();
    for m in keys {
        let obj = &seen[m];
        let age = now - obj.get("ts").and_then(Value::as_i64).unwrap_or(0);
        println!(
            "{}|{}|{}s|{}",
            m,
            obj.get("host").and_then(Value::as_str).unwrap_or(""),
            age,
            obj.get("version").and_then(Value::as_str).unwrap_or("")
        );
    }
    0
}

// ---------------------------------------------------------------- say

fn derive_default_session(from_member: &str, target: &str, now: f64) -> String {
    // 按小时分桶的稳定派生（本地时间，与 Python time.localtime 口径一致）
    let (y, mo, d, h, _mi) = hacs_l3_domain::team_group::beijing_parts_pub(now as i64);
    format!("say-{from_member}-{target}-{y:04}{mo:02}{d:02}{h:02}")
}

fn make_say_session_id(from_member: &str, target: &str, now: f64) -> String {
    let (y, mo, d, h, mi) = hacs_l3_domain::team_group::beijing_parts_pub(now as i64);
    let rand: String = {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        now.to_bits().hash(&mut hasher);
        format!("{:08x}", hasher.finish() as u32)
            .chars()
            .take(4)
            .collect()
    };
    format!("say-{from_member}-{target}-{y:04}{mo:02}{d:02}{h:02}{mi:02}-{rand}")
}

fn build_say_envelope(text: &str, member: &str, timeout: f64, session_id: &str) -> Value {
    json!({"text": text, "timeout_s": timeout, "from": member, "session_id": session_id})
}

fn cmd_say(args: &[String]) -> i32 {
    let o = parse_flags(args);
    // target 取优先级：显式 --to > 位置参数；注意 --as <v> 的值 v 不以 -- 开头，
    // 不能被误当位置参数（此前 --as lobster --to hermes 会把 target 取成 lobster 的 bug）。
    let target = match o.to.clone().or_else(|| {
        // 位置参数 = 不以 -- 开头、且前一个参数不是取值型旗标的参数。
        let value_flags = [
            "--as",
            "--password-file",
            "--nats-host",
            "--nats-port",
            "--timeout",
            "--seconds",
            "--text",
            "--text-file",
            "--session-id",
            "--to",
            "--concurrency",
            "--roster-seconds",
            "--dedup-file",
            "--out",
            "--goal",
            "--agenda",
            "--members",
            "--file",
            "--group",
            "--reply-to",
            "--client-msg-id",
            "--chat-base",
            "--limit",
            "--since",
        ];
        args.windows(2)
            .find(|w| !w[1].starts_with("--") && !value_flags.contains(&w[0].as_str()))
            .map(|w| w[1].clone())
            .or_else(|| args.first().filter(|a| !a.starts_with("--")).cloned())
    }) {
        Some(t) => t,
        None => {
            eprintln!("必须给被叫成员 id（位置参数或 --to）");
            return 2;
        }
    };
    // 互斥检查放最前（SESS-07：不被读口令失败遮掉）
    if o.session_id.is_some() && o.new_session {
        eprintln!("--session-id 与 --new-session 互斥，只能给一个");
        return 2;
    }
    let Some((member, password)) = resolve_member_and_password(&o) else {
        return 2;
    };
    let Some(text) = read_text(&o).filter(|t| !t.trim().is_empty()) else {
        eprintln!("必须提供正文（位置参数或 --text-file）");
        return 2;
    };
    let timeout = clamp_timeout(o.timeout);
    let session_id = if let Some(s) = &o.session_id {
        s.clone()
    } else if o.new_session {
        make_say_session_id(&member, &target, now_epoch())
    } else {
        derive_default_session(&member, &target, now_epoch())
    };
    let envelope = build_say_envelope(&text, &member, timeout, &session_id);
    let subject = format!("a2a.v1.req.{target}");
    let client = mk_client(&o, &member, &password, "team-say");
    if client.connect().is_err() {
        eprintln!("连不上 NATS 中枢：{}", o.nats_host);
        return 2;
    }
    let reply = client.request(
        &subject,
        envelope.to_string().as_bytes(),
        Duration::from_secs_f64(timeout + 15.0),
    );
    client.close();
    let Some(raw) = reply else {
        eprintln!(
            "点名超时（{timeout}s 内无应答）—— 确认 <{target}> 的 teamd 在跑、subject 正确：{subject}"
        );
        return 1;
    };
    let text_reply = String::from_utf8_lossy(&raw).into_owned();
    let Ok(obj) = serde_json::from_str::<Value>(&text_reply) else {
        println!("{text_reply}");
        return 0;
    };
    if o.json {
        println!("{}", serde_json::to_string_pretty(&obj).unwrap_or_default());
    } else if obj.get("ok").and_then(Value::as_bool).unwrap_or(false)
        && obj.get("reply").map(|v| !v.is_null()).unwrap_or(false)
    {
        println!("{}", obj["reply"]);
    } else {
        eprintln!("对方回了错误：{obj}");
    }
    if obj.get("ok").and_then(Value::as_bool).unwrap_or(false) {
        0
    } else {
        3
    }
}

// ---------------------------------------------------------------- broadcast

fn call_one(
    host: &str,
    port: u16,
    user: &str,
    password: &str,
    member: &str,
    payload: &[u8],
    timeout: f64,
) -> Value {
    let t0 = std::time::Instant::now();
    let mut out =
        json!({"member": member, "ok": false, "reply": null, "error": null, "elapsed": 0.0});
    let client = {
        let cfg = NatsConfig::from_hosts_str(host, port)
            .unwrap_or_else(|_| NatsConfig::from_hosts_str("127.0.0.1", port).unwrap())
            .with_credentials(Some(user.into()), Some(password.into()))
            .with_name("team-bc-one");
        NatsClient::new(cfg)
    };
    if client.connect().is_err() {
        out["error"] = json!("连不上 NATS");
        out["elapsed"] = json!((t0.elapsed().as_secs_f64() * 10.0).round() / 10.0);
        return out;
    }
    let raw = client.request(
        &format!("a2a.v1.req.{member}"),
        payload,
        Duration::from_secs_f64(timeout),
    );
    out["elapsed"] = json!((t0.elapsed().as_secs_f64() * 10.0).round() / 10.0);
    client.close();
    match raw {
        None => out["error"] = json!("超时未回应"),
        Some(bytes) => {
            let text = String::from_utf8_lossy(&bytes).into_owned();
            match serde_json::from_str::<Value>(&text) {
                Ok(data) => {
                    out["reply"] = data.get("reply").cloned().unwrap_or(json!(text));
                    if data.get("ok").and_then(Value::as_bool) == Some(false) {
                        out["error"] = json!("对方返回 ok=false");
                    } else {
                        out["ok"] = json!(true);
                    }
                }
                Err(_) => {
                    out["reply"] = json!(text);
                    out["ok"] = json!(true);
                }
            }
        }
    }
    out
}

fn cmd_broadcast(args: &[String]) -> i32 {
    let o = parse_flags(args);
    let Some((member, password)) = resolve_member_and_password(&o) else {
        return 2;
    };
    let Some(text) = read_text(&o).filter(|t| !t.trim().is_empty()) else {
        eprintln!("必须提供正文（位置参数或 --text-file）");
        return 2;
    };
    if o.all == o.to.is_some() {
        eprintln!("必须**且只能**给 --all 或 --to 其中一个");
        return 2;
    }
    let timeout = clamp_timeout(o.timeout);
    let (target_desc, target_list) = if o.all {
        ("all".to_string(), None)
    } else {
        let mut list: Vec<String> =
            o.to.as_ref()
                .map(|s| {
                    s.split(',')
                        .map(str::trim)
                        .filter(|m| !m.is_empty())
                        .map(String::from)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
        list.sort();
        list.dedup();
        if list.is_empty() {
            eprintln!("--to 不能为空");
            return 2;
        }
        (list.join(","), Some(list))
    };
    let subject_norm = format!("broadcast:{target_desc}");
    let (should_send, key) = check_and_mark_dedup(&o.dedup_file, &subject_norm, &text, o.force);
    if !should_send {
        println!("本窗口内已发过同一内容，跳过（用 --force 强制重发）");
        return 0;
    }
    let members: Vec<String> = if o.all {
        let client = mk_client(&o, &member, &password, "team-bc");
        if client.connect().is_err() {
            rollback_dedup(&o.dedup_file, &key);
            eprintln!("连接中枢失败");
            return 2;
        }
        let seen = collect_heartbeats(&client, o.roster_seconds);
        client.close();
        let mut ms: Vec<String> = seen.keys().filter(|m| *m != &member).cloned().collect();
        ms.sort();
        ms
    } else {
        target_list
            .unwrap()
            .into_iter()
            .filter(|m| *m != member)
            .collect()
    };
    if members.is_empty() {
        rollback_dedup(&o.dedup_file, &key);
        let why = if o.all {
            format!("窗口 {:.0}s 内没扫到在线成员", o.roster_seconds)
        } else {
            "排除自己后 --to 没有其他成员".to_string()
        };
        eprintln!("目标名单为空（{why}）");
        return 3;
    }
    let mut envelope =
        json!({"text": text, "timeout_s": timeout, "subject": "群发", "from": member});
    if let Some(s) = &o.session_id {
        envelope["session_id"] = json!(s);
    }
    let payload = envelope.to_string().into_bytes();
    println!(
        "群发目标 {} 个：{}　（并行度 {}，单成员超时 {:.0}s）",
        members.len(),
        members.join(","),
        o.concurrency,
        timeout
    );
    // 限并发扇出（线程 + 信号量）
    let sem = std::sync::Arc::new(Semaphore::new(o.concurrency.max(1)));
    let mut handles = Vec::new();
    for m in &members {
        let sem = sem.clone();
        let m = m.clone();
        let (h, p, u, pw) = (
            o.nats_host.clone(),
            o.nats_port,
            member.clone(),
            password.clone(),
        );
        let payload = payload.clone();
        handles.push(std::thread::spawn(move || {
            let _g = sem.acquire();
            call_one(&h, p, &u, &pw, &m, &payload, timeout)
        }));
    }
    let mut results: Vec<Value> = Vec::new();
    for h in handles {
        if let Ok(r) = h.join() {
            results.push(r);
        }
    }
    let ok_n = results
        .iter()
        .filter(|r| r["ok"].as_bool().unwrap_or(false))
        .count();
    for r in &results {
        let mark = if r["ok"].as_bool().unwrap_or(false) {
            "OK"
        } else {
            "X "
        };
        let rep = r["reply"]
            .as_str()
            .or(r["error"].as_str())
            .unwrap_or("")
            .replace('\n', " ");
        let rep = if rep.chars().count() > 160 {
            format!("{}…", rep.chars().take(160).collect::<String>())
        } else {
            rep
        };
        println!(
            "  [{mark}] {:<14} {:>6.1}s  {}",
            r["member"].as_str().unwrap_or("?"),
            r["elapsed"].as_f64().unwrap_or(0.0),
            rep
        );
    }
    println!("汇总：{}/{} 回应", ok_n, members.len());
    if o.json {
        println!(
            "{}",
            serde_json::to_string_pretty(
                &json!({"ok": ok_n, "total": members.len(), "results": results})
            )
            .unwrap_or_default()
        );
    }
    if ok_n == members.len() {
        0
    } else {
        1
    }
}

// ---------------------------------------------------------------- meet / history / group（复用 L3 team_group）

fn tg_cmd_meet(args: &[String]) -> i32 {
    let o = parse_flags(args);
    let Some(goal) = o.goal.clone() else {
        eprintln!("必须给 --goal");
        return 2;
    };
    let Some(members) = o.members.clone() else {
        eprintln!("必须给 --members");
        return 2;
    };
    let Some((member, password)) = resolve_member_and_password(&o) else {
        return 2;
    };
    let text = format!("【会议】{}\n{}", goal, o.agenda.join("\n"));
    let timeout = clamp_timeout(o.timeout);
    let subject_norm = format!("meet:{}", members);
    let (should_send, key) = check_and_mark_dedup(&o.dedup_file, &subject_norm, &text, o.force);
    if !should_send {
        println!("本窗口内已召集过同议题会议，跳过（用 --force 强制重发）");
        return 0;
    }
    let mut list: Vec<String> = members
        .split(',')
        .map(str::trim)
        .filter(|m| !m.is_empty() && *m != member)
        .map(String::from)
        .collect();
    list.sort();
    list.dedup();
    if list.is_empty() {
        rollback_dedup(&o.dedup_file, &key);
        eprintln!("参会名单为空（排除自己后）");
        return 3;
    }
    let session_id = o.session_id.clone().unwrap_or_else(|| {
        let now = now_epoch() as i64;
        hacs_l3_domain::team_group::make_session_id(now, "e2e0")
    });
    let mut envelope = json!({
        "text": text, "timeout_s": timeout, "subject": "会议", "from": member,
        "session_id": session_id, "goal": goal,
    });
    if !o.agenda.is_empty() {
        envelope["agenda"] = json!(o.agenda);
    }
    let payload = envelope.to_string().into_bytes();
    let sem = std::sync::Arc::new(Semaphore::new(o.concurrency.max(1)));
    let mut handles = Vec::new();
    for m in &list {
        let sem = sem.clone();
        let m = m.clone();
        let (h, p, u, pw) = (
            o.nats_host.clone(),
            o.nats_port,
            member.clone(),
            password.clone(),
        );
        let payload = payload.clone();
        handles.push(std::thread::spawn(move || {
            let _g = sem.acquire();
            call_one(&h, p, &u, &pw, &m, &payload, timeout)
        }));
    }
    let mut results: Vec<Value> = Vec::new();
    for h in handles {
        if let Ok(r) = h.join() {
            results.push(r);
        }
    }
    let report = hacs_l3_domain::team_group::meet_report(&results, list.len());
    println!("{report}");
    let record = json!({
        "session_id": session_id, "goal": goal, "agenda": o.agenda,
        "members": list, "results": results, "ts": now_epoch() as i64,
    });
    let _ = hacs_l3_domain::team_group::append_jsonl(&o.out, &record);
    println!("留痕：{}", o.out);
    let bad = results
        .iter()
        .filter(|r| !r["ok"].as_bool().unwrap_or(false))
        .count();
    if bad == 0 {
        0
    } else {
        1
    }
}

fn tg_cmd_history(args: &[String]) -> i32 {
    let o = parse_flags(args);
    let Some(file) = o.file.clone() else {
        eprintln!("必须给 --file");
        return 2;
    };
    let Some(session_id) = o.session_id.clone() else {
        eprintln!("必须给 --session-id");
        return 2;
    };
    let Some(found) = hacs_l3_domain::team_group::find_last_by_session(&file, &session_id) else {
        eprintln!("没找到 session_id={session_id} 的会议记录（文件：{file}）");
        return 1;
    };
    println!("会议 {}：{}", found["session_id"], found["goal"]);
    if let Some(results) = found["results"].as_array() {
        for r in results {
            let mark = if r["ok"].as_bool().unwrap_or(false) {
                "OK"
            } else {
                "X "
            };
            println!(
                "  [{mark}] {} {}",
                r["member"].as_str().unwrap_or("?"),
                r["reply"]
                    .as_str()
                    .unwrap_or(r["error"].as_str().unwrap_or(""))
            );
        }
    }
    0
}

fn tg_cmd_group_say(args: &[String]) -> i32 {
    let o = parse_flags(args);
    let Some(group) = o.group.clone() else {
        eprintln!("必须给 --group");
        return 2;
    };
    let Some((member, _pw)) = resolve_member_and_password(&o) else {
        return 2;
    };
    let Some(text) = read_text(&o).filter(|t| !t.trim().is_empty()) else {
        eprintln!("必须提供正文");
        return 2;
    };
    let to = o.to.clone().unwrap_or_else(|| "@all".into());
    let body = json!({
        "text": text, "to": to,
        "reply_to": o.reply_to,
        "client_msg_id": o.client_msg_id,
    });
    let url = format!("{}/api/sessions/{}/messages", o.chat_base, group);
    let (status, resp) = tg_chat_post(&url, &member, &body);
    if (200..300).contains(&status) {
        if o.json {
            println!("{resp}");
        } else {
            println!(
                "已发送（seq={}）",
                resp["message"]["seq"].as_i64().unwrap_or(0)
            );
        }
        0
    } else {
        eprintln!("发送失败：{} {}", status, resp);
        2
    }
}

fn tg_cmd_group_pull(args: &[String]) -> i32 {
    let o = parse_flags(args);
    let Some(group) = o.group.clone() else {
        eprintln!("必须给 --group");
        return 2;
    };
    let Some((member, _pw)) = resolve_member_and_password(&o) else {
        return 2;
    };
    let since = o.since.map(|s| format!("since={s}&")).unwrap_or_default();
    let url = format!(
        "{}/api/sessions/{}/messages?{}limit={}",
        o.chat_base, group, since, o.limit
    );
    let (status, resp) = tg_chat_get(&url, &member);
    if !(200..300).contains(&status) {
        eprintln!("拉取失败：{} {}", status, resp);
        return 2;
    }
    let msgs = resp["messages"].as_array().cloned().unwrap_or_default();
    for m in &msgs {
        println!(
            "{}",
            hacs_l3_domain::team_group::pull_line(Some(member.as_str()), m)
        );
    }
    println!("共 {} 条", msgs.len());
    0
}

fn tg_cmd_group_show(args: &[String]) -> i32 {
    let o = parse_flags(args);
    let Some(group) = o.group.clone() else {
        eprintln!("必须给 --group");
        return 2;
    };
    let Some((member, _pw)) = resolve_member_and_password(&o) else {
        return 2;
    };
    let url = format!("{}/api/sessions/{}", o.chat_base, group);
    let (status, resp) = tg_chat_get(&url, &member);
    if !(200..300).contains(&status) {
        eprintln!("查看失败：{} {}", status, resp);
        return 2;
    }
    println!(
        "群 {}：{}（状态 {}，发起人 {}，成员 {}）",
        group,
        resp["title"].as_str().unwrap_or(""),
        resp["status"].as_str().unwrap_or(""),
        resp["creator"].as_str().unwrap_or(""),
        resp["members"].as_array().map(|a| a.len()).unwrap_or(0)
    );
    0
}

// ---------------------------------------------------------------- chat HTTP 客户端（本 bin 内联；Python 用 urllib）

fn tg_chat_post(url: &str, member: &str, body: &Value) -> (u16, Value) {
    let _ = member;
    let out = std::process::Command::new("curl")
        .args([
            "-s",
            "-m",
            "10",
            "-X",
            "POST",
            url,
            "-H",
            "Content-Type: application/json",
            "-d",
            &body.to_string(),
        ])
        .output();
    match out {
        Ok(o) => {
            let status = o.status.code().unwrap_or(-1);
            let v = serde_json::from_slice(&o.stdout).unwrap_or(Value::Null);
            // curl 不给 HTTP 状态码（-s 无 -w）；统一 200/0 处理
            (if status == 0 { 200 } else { status as u16 }, v)
        }
        Err(e) => (
            0,
            json!({"ok": false, "error": {"code": "curl_error", "message": e.to_string()}}),
        ),
    }
}

fn tg_chat_get(url: &str, member: &str) -> (u16, Value) {
    let _ = member;
    let out = std::process::Command::new("curl")
        .args(["-s", "-m", "10", url])
        .output();
    match out {
        Ok(o) => {
            let v = serde_json::from_slice(&o.stdout).unwrap_or(Value::Null);
            (200, v)
        }
        Err(e) => (
            0,
            json!({"ok": false, "error": {"code": "curl_error", "message": e.to_string()}}),
        ),
    }
}

/// std 无 Semaphore——Mutex+Condvar 自实现计数信号量（同 team_broadcast.rs）。
struct Semaphore {
    state: std::sync::Mutex<usize>,
    cv: std::sync::Condvar,
}
struct SemaphoreGuard<'a> {
    sem: &'a Semaphore,
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
