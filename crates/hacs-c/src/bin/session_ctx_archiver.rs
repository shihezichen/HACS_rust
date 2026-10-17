//! `session_ctx_archiver` —— 会话上下文存档器 CLI（对应 Python
//! `hub/l3_domain/session_ctx_archiver.py` 的 main/装配）。
//!
//! 用法（argv 风格与 Python 对齐）：
//! ```text
//! session_ctx_archiver [--session-id S | --scan] [--threshold 90000]
//!                      [--group meet_xxx] [--chat-base URL] [--agent-id hermes]
//!                      [--archive-dir DIR] [--force] [--dry-run] [--no-group]
//!                      [--log-file F] [--hermes-home DIR] [--group-map JSON]
//!                      [--facts] [--timeout 20]
//! ```
//!
//! 退出码沿用 Python：0（单会话分支恒 0；扫描分支也是 0）。

use std::collections::HashMap;
use std::io::{BufRead, Write as _};
use std::time::Duration;

use hacs_l3_domain::session_ctx_archiver as sca;
use serde_json::{json, Map, Value};

fn main() {
    let code = run();
    std::process::exit(code);
}

fn usage() {
    eprintln!(
        "用法: session_ctx_archiver [--session-id S | --scan] [--threshold 90000]\n\
         \x20                 [--group meet_xxx] [--chat-base URL] [--agent-id hermes]\n\
         \x20                 [--archive-dir DIR] [--force] [--dry-run] [--no-group]\n\
         \x20                 [--log-file F] [--hermes-home DIR] [--group-map JSON]\n\
         \x20                 [--facts] [--timeout 20]"
    );
}

// ---------------------------------------------------------------- 参数小工具

struct Flags {
    map: HashMap<String, String>,
    flags: std::collections::HashSet<String>,
}

fn parse_flags(args: &[String]) -> Flags {
    let mut map = HashMap::new();
    let mut flags = std::collections::HashSet::new();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if let Some(name) = a.strip_prefix("--") {
            match args.get(i + 1) {
                Some(v) if !v.starts_with("--") => {
                    map.insert(name.to_string(), v.clone());
                    i += 2;
                }
                _ => {
                    flags.insert(name.to_string());
                    i += 1;
                }
            }
        } else {
            i += 1;
        }
    }
    Flags { map, flags }
}

impl Flags {
    fn get(&self, k: &str, d: &str) -> String {
        self.map.get(k).cloned().unwrap_or_else(|| d.to_string())
    }
    fn has(&self, k: &str) -> bool {
        self.flags.contains(k)
    }
}

// ---------------------------------------------------------------- 日志（=Python log()）

struct Logger {
    file: Option<std::fs::File>,
}

impl Logger {
    fn log(&mut self, parts: &[&str]) {
        let now = now_beijing();
        let line = format!("[{}] {}", now, parts.join(" "));
        println!("{line}");
        let _ = std::io::stdout().flush();
        if let Some(f) = &mut self.file {
            let _ = writeln!(f, "{line}");
        }
    }
}

/// `%Y-%m-%d %H:%M:%S` 北京时间（复用 team_group 的 civil 算法）。
fn now_beijing() -> String {
    let epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let (y, mo, d, h, mi) = hacs_l3_domain::team_group::beijing_parts_pub(epoch + 8 * 3600);
    let (y2, mo2, d2, h2, mi2) = if h >= 24 {
        (y, mo, d + (h / 24), h % 24, mi)
    } else {
        (y, mo, d, h, mi)
    };
    format!("{y2:04}-{mo2:02}-{d2:02} {h2:02}:{mi2:02}:00")
}

// ---------------------------------------------------------------- agent.log 扫描

fn scan_agent_log(
    path: &str,
    conflict_sid: Option<&str>,
    logger: &mut Logger,
) -> HashMap<String, sca::CtxWatermark> {
    let mut out = HashMap::new();
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => {
            logger.log(&["找不到 agent.log：", path]);
            return out;
        }
    };
    let reader = std::io::BufReader::new(file);
    for line in reader.lines().map_while(Result::ok) {
        if let Some(sid) = conflict_sid {
            // 快速过滤：行里没有该 sid 就不用解析
            if !line.contains(sid) {
                continue;
            }
        }
        sca::scan_note(&mut out, &line);
    }
    if let Some(sid) = conflict_sid {
        out.retain(|k, _| k == sid);
    }
    out
}

// ---------------------------------------------------------------- state.db（rusqlite，只读）

const META_COLS: &[&str] = &[
    "id",
    "source",
    "title",
    "session_key",
    "chat_id",
    "chat_type",
    "cwd",
    "parent_session_id",
    "message_count",
    "tool_names",
    "started_at",
    "last_activity_at",
    "model",
    "profile_name",
];

fn session_meta(db_path: &str, sid: &str, logger: &mut Logger) -> Value {
    let conn = match rusqlite::Connection::open_with_flags(
        db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    ) {
        Ok(c) => c,
        Err(e) => {
            logger.log(&["读 state.db 会话元信息失败:", &e.to_string()]);
            return json!({});
        }
    };
    let sql = format!("select {} from sessions where id=?1", META_COLS.join(", "));
    let mut stmt = match conn.prepare(&sql) {
        Ok(s) => s,
        Err(e) => {
            logger.log(&["读 state.db 会话元信息失败:", &e.to_string()]);
            return json!({});
        }
    };
    let mut meta = Map::new();
    let res = stmt.query_row([sid], |row| {
        for (i, c) in META_COLS.iter().enumerate() {
            let v: Option<String> = row.get(i).unwrap_or(None);
            meta.insert(
                c.to_string(),
                match v {
                    Some(s) => json!(s),
                    None => Value::Null,
                },
            );
        }
        Ok(())
    });
    if let Err(e) = res {
        if !matches!(e, rusqlite::Error::QueryReturnedNoRows) {
            logger.log(&["读 state.db 会话元信息失败:", &e.to_string()]);
        }
        return json!({});
    }
    Value::Object(meta)
}

fn session_messages(db_path: &str, sid: &str, limit: usize, logger: &mut Logger) -> Vec<sca::Msg> {
    let conn = match rusqlite::Connection::open_with_flags(
        db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    ) {
        Ok(c) => c,
        Err(e) => {
            logger.log(&["读 state.db 消息失败:", &e.to_string()]);
            return Vec::new();
        }
    };
    let Ok(mut stmt) = conn.prepare(
        "select role, tool_name, content, timestamp from messages \
         where session_id=?1 order by timestamp desc limit ?2",
    ) else {
        logger.log(&["读 state.db 消息失败:", "prepare 失败"]);
        return Vec::new();
    };
    let rows = stmt.query_map(rusqlite::params![sid, limit as i64], |row| {
        Ok((
            row.get::<_, Option<String>>(0)?.unwrap_or_default(),
            row.get::<_, Option<String>>(1)?.unwrap_or_default(),
            row.get::<_, Option<String>>(2)?.unwrap_or_default(),
        ))
    });
    let mut rows = match rows {
        Ok(r) => r.filter_map(Result::ok).collect::<Vec<_>>(),
        Err(e) => {
            logger.log(&["读 state.db 消息失败:", &e.to_string()]);
            return Vec::new();
        }
    };
    rows.reverse(); // Python: rows.reverse()
    rows.into_iter()
        .map(|(role, tool_name, content)| sca::Msg {
            role,
            tool_name,
            content,
        })
        .collect()
}

// ---------------------------------------------------------------- HTTP（走 L1 手写客户端）

fn http_call(
    method: &str,
    url: &str,
    body: Option<&Value>,
    agent_id: Option<&str>,
    extra_headers: &[(&str, &str)],
    timeout: Duration,
) -> (u16, Value) {
    let mut headers: Vec<(&str, &str)> = vec![("Accept", "application/json")];
    if let Some(a) = agent_id {
        headers.push(("X-Agent-Id", a));
    }
    for (k, v) in extra_headers {
        headers.push((k, v));
    }
    match hacs_l1_transport::http_client::request_json(url, method, body, &headers, timeout) {
        Ok((st, v)) => (st, v),
        Err(e) => (0, json!({"error": format!("HttpError: {}", e.0)})),
    }
}

// ---------------------------------------------------------------- 幂等状态

fn load_state(path: &str) -> Map<String, Value> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
}

fn save_state(path: &str, st: &Map<String, Value>) {
    if let Some(dir) = std::path::Path::new(path).parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = format!("{path}.tmp");
    if std::fs::write(
        &tmp,
        serde_json::to_string_pretty(&json!(st)).unwrap_or_default(),
    )
    .is_ok()
    {
        let _ = std::fs::rename(&tmp, path);
    }
}

// ---------------------------------------------------------------- 压缩事实（=Python compression_facts 的 Rust 侧）

fn compression_facts(hermes_home: &str) -> Value {
    let cfg = format!("{hermes_home}/config.yaml");
    let comp = match std::fs::read_to_string(&cfg) {
        Ok(txt) => Value::Object(sca::parse_compression_config(&txt)),
        Err(e) => json!({"error": e.to_string()}),
    };
    json!({
        "config_path": cfg,
        "config_compression": comp,
        "in_session_slash": "/compress（别名 /compact；hermes_cli/commands.py:80）—— 只作用于**当前交互会话**",
        "external_trigger_available": false,
        "external_trigger_note": "未找到任何外部可用的压缩/清理入口：CLI 无子命令、gateway 控制套接字只答 identify/status、没有 /api/sessions/{id}/compress 之类 HTTP 端点。本机实测 0/1：不伪造结果。",
        "auto_compression_exists": true,
        "auto_compression_note": "Hermes 自身会在超限时压缩/清理（config.yaml compression.proactive_prune_tokens 即为自动剪枝阈值）。本脚本的存档是**补充**，不是替代。",
    })
}

// ---------------------------------------------------------------- 存档落盘

fn write_archive(
    archive_root: &str,
    sid: &str,
    doc: &Value,
    logger: &mut Logger,
) -> Option<(String, String)> {
    use hacs_l3_domain::team_group::beijing_parts_pub;
    let epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let (y, mo, d, h, mi) = beijing_parts_pub(epoch + 8 * 3600);
    let sec = epoch % 60;
    let ts = format!("{y:04}{mo:02}{d:02}-{h:02}{mi:02}{sec:02}");
    let dir = std::path::Path::new(archive_root).join(sid);
    if std::fs::create_dir_all(&dir).is_err() {
        logger.log(&["建目录失败:", &dir.display().to_string()]);
        return None;
    }
    let jp = dir.join(format!("{ts}.json"));
    let mp = dir.join(format!("{ts}.md"));
    let chat_base_default =
        std::env::var("AGENT_TEAM_CHAT_BASE").unwrap_or_else(|_| "http://127.0.0.1:9980".into());
    let md = sca::render_md(doc, &chat_base_default, archive_root);
    if std::fs::write(&jp, serde_json::to_string_pretty(doc).unwrap_or_default()).is_err() {
        return None;
    }
    if std::fs::write(&mp, md).is_err() {
        return None;
    }
    // latest 便于恢复时固定路径
    let _ = std::fs::copy(&jp, dir.join("latest.json"));
    let _ = std::fs::copy(&mp, dir.join("latest.md"));
    Some((jp.display().to_string(), mp.display().to_string()))
}

// ---------------------------------------------------------------- 群写（archive_to_group）

fn group_members(base: &str, sid: &str, timeout: Duration) -> (Vec<String>, Value) {
    let (st, body) = http_call(
        "GET",
        &format!("{base}/api/sessions/{sid}"),
        None,
        None,
        &[],
        timeout,
    );
    if st == 200 {
        if let Some(s) = body.get("session") {
            let members: Vec<String> = s
                .get("members")
                .and_then(|m| m.as_array())
                .map(|a| {
                    a.iter()
                        .map(|x| {
                            if let Some(id) = x.get("id").and_then(|v| v.as_str()) {
                                id.to_string()
                            } else {
                                x.as_str().unwrap_or("").to_string()
                            }
                        })
                        .collect()
                })
                .unwrap_or_default();
            return (members, s.clone());
        }
    }
    (Vec::new(), json!({}))
}

fn memo_get(base: &str, sid: &str, agent_id: &str, timeout: Duration) -> Value {
    let (st, body) = http_call(
        "GET",
        &format!("{base}/api/sessions/{sid}/memo"),
        None,
        Some(agent_id),
        &[],
        timeout,
    );
    if st == 200 && body.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
        body
    } else {
        json!({"ok": false, "status": st, "body": body})
    }
}

fn memo_patch(base: &str, sid: &str, agent_id: &str, patch: &Value, timeout: Duration) -> Value {
    let mut last = Value::Null;
    for _ in 0..3 {
        let cur = memo_get(base, sid, agent_id, timeout);
        if !cur.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
            return json!({"ok": false, "stage": "memo_get", "detail": cur});
        }
        let etag = cur.get("etag").and_then(|v| v.as_str()).unwrap_or("");
        let extra = [("If-Match", etag)];
        let (st, body) = http_call(
            "PATCH",
            &format!("{base}/api/sessions/{sid}/memo"),
            Some(patch),
            Some(agent_id),
            &extra,
            timeout,
        );
        if st == 200 && body.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
            return json!({"ok": true, "version": body.get("version"), "etag": body.get("etag")});
        }
        last = json!({"status": st, "body": body});
        if st == 409 {
            std::thread::sleep(Duration::from_millis(400));
            continue;
        }
        break;
    }
    json!({"ok": false, "stage": "memo_patch", "detail": last})
}

fn archive_to_group(
    base: &str,
    sid: &str,
    agent_id: &str,
    doc: &Value,
    archive_root: &str,
    timeout: Duration,
) -> Value {
    let (members, sess) = group_members(base, sid, timeout);
    if !members.iter().any(|m| m == agent_id) {
        return json!({
            "ok": false,
            "stage": "member_check",
            "detail": format!(
                "X-Agent-Id={} 不是群 {} 成员（members={:?}）",
                agent_id, sid, members
            )
        });
    }
    let _ = sess;
    let cur = memo_get(base, sid, agent_id, timeout);
    let old: Map<String, Value> = if cur.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
        cur.get("memo")
            .and_then(|v| v.as_object())
            .cloned()
            .unwrap_or_default()
    } else {
        Map::new()
    };
    let patch = sca::build_memo_patch(agent_id, doc, &old, archive_root);
    let mw = memo_patch(base, sid, agent_id, &patch, timeout);
    let text = sca::build_group_text(doc, archive_root);
    let (st, body) = http_call(
        "POST",
        &format!("{base}/api/sessions/{sid}/messages"),
        Some(&json!({"text": text, "mentions": ["@all"]})),
        Some(agent_id),
        &[],
        timeout,
    );
    let posted =
        matches!(st, 200 | 201) && body.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
    let seq = body
        .get("message")
        .and_then(|m| m.get("seq"))
        .cloned()
        .unwrap_or(Value::Null);
    json!({
        "ok": mw.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) && posted,
        "memo_write": mw,
        "message_post": {
            "ok": posted,
            "status": st,
            "seq": seq,
            "detail": if posted { Value::Null } else { body },
        },
        "text": text,
    })
}

// ---------------------------------------------------------------- 单会话处理

fn handle_one(
    fl: &Flags,
    sid: &str,
    ctx: &sca::CtxWatermark,
    state: &mut Map<String, Value>,
    hermes_home: &str,
    archive_root: &str,
    group_map: &HashMap<String, String>,
    logger: &mut Logger,
) -> Value {
    let threshold: i64 = fl.get("threshold", "90000").parse().unwrap_or(90000);
    let force = fl.has("force");
    let dry_run = fl.has("dry-run");
    let no_group = fl.has("no-group");
    let chat_base = fl.get("chat-base", &default_chat_base());
    let agent_id = fl.get("agent-id", sca::DEFAULT_AGENT_ID);
    let timeout = Duration::from_secs_f64(fl.get("timeout", "20").parse::<f64>().unwrap_or(20.0));

    if let Err(skip) = sca::check_idempotent(sid, ctx.last_in, threshold, force, state) {
        let reason = skip["skipped"].as_str().unwrap_or("");
        if reason == "below_threshold" {
            logger.log(&[
                "跳过（未达阈值）",
                sid,
                &format!("last_in={} < {}", ctx.last_in, threshold),
            ]);
        } else {
            logger.log(&[
                "跳过（幂等：未显著增长）",
                sid,
                &format!(
                    "cur={} prev={}",
                    ctx.last_in,
                    state
                        .get(sid)
                        .and_then(|p| p.get("last_in").and_then(|v| v.as_u64()))
                        .unwrap_or(0)
                ),
            ]);
        }
        return skip;
    }

    let db_path = format!("{hermes_home}/state.db");
    let meta = session_meta(&db_path, sid, logger);
    let msgs = session_messages(&db_path, sid, sca::MSG_SCAN_LIMIT, logger);
    let scene = sca::extract_scene(&msgs);
    logger.log(&[
        "存档中",
        sid,
        &format!("tok={}", ctx.last_in),
        &format!("消息={}", msgs.len()),
        &format!("未完成={}", scene.open_tasks.len()),
        &format!("已完成={}", scene.done.len()),
        &format!("决定={}", scene.decisions.len()),
    ]);

    let gid = if fl.map.contains_key("group") {
        fl.get("group", "")
    } else {
        sca::resolve_group(group_map, sid)
    };
    let archived_at = now_beijing();
    let doc = sca::build_doc(
        sid,
        &archived_at,
        ctx,
        &meta,
        &scene,
        threshold,
        &gid,
        Value::Null,
        compression_facts(hermes_home),
        archive_root,
    );
    let Some((jp, mp)) = write_archive(archive_root, sid, &doc, logger) else {
        return json!({"error": "write_failed", "session_id": sid});
    };
    logger.log(&["已落盘:", &jp]);
    logger.log(&["已落盘:", &mp]);

    if dry_run {
        logger.log(&["--dry-run：跳过群写入"]);
        return json!({"archived": [jp, mp], "last_in": ctx.last_in, "group_write": null});
    }

    let mut doc = doc;
    if !gid.is_empty() && !no_group {
        let gw = archive_to_group(&chat_base, &gid, &agent_id, &doc, archive_root, timeout);
        doc["group_write"] = gw.clone();
        // Python 会把 group_write 回填进 json/md 重写一遍
        if let Some(obj) = doc.as_object_mut() {
            obj.remove("_archive_root");
        }
        if let Some((jp2, mp2)) = write_archive(archive_root, sid, &doc, logger) {
            let _ = (jp2, mp2); // latest 已刷新
        }
        let ok = gw.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
        logger.log(&[
            "群写入:",
            if ok { "OK" } else { "失败" },
            &format!("memo={}", gw.get("memo_write").unwrap_or(&Value::Null)),
            &format!("msg={}", gw.get("message_post").unwrap_or(&Value::Null)),
        ]);
    } else {
        logger.log(&["未指定 --group：只落本机存档，不写群"]);
    }

    state.insert(
        sid.to_string(),
        json!({
            "last_in": ctx.last_in,
            "at": archived_at,
            "archive": jp,
            "open_tasks": scene.open_tasks.len(),
        }),
    );
    json!({
        "archived": [jp, mp],
        "last_in": ctx.last_in,
        "open_tasks": scene.open_tasks.len(),
        "group_write": doc.get("group_write").cloned().unwrap_or(Value::Null),
    })
}

fn default_chat_base() -> String {
    std::env::var("AGENT_TEAM_CHAT_BASE").unwrap_or_else(|_| "http://127.0.0.1:9980".into())
}

// ---------------------------------------------------------------- main 装配

fn run() -> i32 {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        usage();
        return 0;
    }
    let fl = parse_flags(&args);
    let hermes_home = fl
        .map
        .get("hermes-home")
        .cloned()
        .or_else(|| std::env::var("HERMES_HOME").ok())
        .unwrap_or_else(|| dirs_home().unwrap_or_else(|| "/root".into()));
    let archive_root = fl.get(
        "archive-dir",
        // Python 默认 HERE/ctx_archive（hub/l3_domain/ 下）
        "~/HACS_py/hub/l3_domain/ctx_archive",
    );
    let archive_root = archive_root.replace("~", &dirs_home().unwrap_or_default());
    let agent_log = format!("{hermes_home}/logs/agent.log");
    let state_file = format!(
        "{}/state/ctx_archiver_state.json",
        archive_root.trim_end_matches('/')
    );
    let mut logger = Logger {
        file: if fl.map.contains_key("log-file") {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(fl.get("log-file", ""))
                .ok()
        } else {
            None
        },
    };

    // group_map：--group-map JSON 或 HERE/ctx_group_map.json（Rust 侧默认同目录，找不到就空）
    let mut group_map: HashMap<String, String> = HashMap::new();
    let gm_path =
        fl.map.get("group-map").cloned().unwrap_or_else(|| {
            format!("{}/ctx_group_map.json", archive_root.trim_end_matches('/'))
        });
    if std::path::Path::new(&gm_path).exists() {
        if let Ok(txt) = std::fs::read_to_string(&gm_path) {
            if let Ok(Value::Object(m)) = serde_json::from_str::<Value>(&txt) {
                for (k, v) in m {
                    if let Some(s) = v.as_str() {
                        group_map.insert(k, s.to_string());
                    }
                }
                logger.log(&[
                    "已加载 group_map:",
                    &gm_path,
                    &format!("条目={}", group_map.len()),
                ]);
            }
        }
    }

    if fl.has("facts") {
        let out = json!({
            "compression": compression_facts(&hermes_home),
            "chat_base": fl.get("chat-base", &default_chat_base()),
            "archive_root": archive_root,
            "agent_log": agent_log,
            "state_db": format!("{hermes_home}/state.db"),
        });
        println!("{}", serde_json::to_string_pretty(&out).unwrap_or_default());
        return 0;
    }

    let mut state = load_state(&state_file);
    let session_id = fl.map.get("session-id").cloned().unwrap_or_default();
    let all_ctx = scan_agent_log(
        &agent_log,
        if session_id.is_empty() {
            None
        } else {
            Some(&session_id)
        },
        &mut logger,
    );

    if !session_id.is_empty() {
        let ctx = all_ctx.get(&session_id).cloned().unwrap_or_default();
        if ctx.last_in == 0 && ctx.calls == 0 {
            logger.log(&["agent.log 里没有该会话的 API 记录：", &session_id]);
        }
        let r = handle_one(
            &fl,
            &session_id,
            &ctx,
            &mut state,
            &hermes_home,
            &archive_root,
            &group_map,
            &mut logger,
        );
        save_state(&state_file, &state);
        let _ = r;
        return 0; // Python: return 0 if not handle_one(...) else 0 —— 恒 0
    }

    // 扫描分支
    let threshold: i64 = fl.get("threshold", "90000").parse().unwrap_or(90000);
    let hits = sca::threshold_hits(&all_ctx, threshold);
    logger.log(&[&format!(
        "扫描完成：{} 个会话有 API 记录，其中 {} 个 >= {} tok",
        all_ctx.len(),
        hits.len(),
        threshold
    )]);
    for (sid, c) in &hits {
        logger.log(&[&format!(
            "· {sid} last_in={} max={} {}",
            c.last_in, c.max_in, c.last_ts
        )]);
    }
    let mut n = 0;
    for (sid, c) in &hits {
        let r = handle_one(
            &fl,
            sid,
            c,
            &mut state,
            &hermes_home,
            &archive_root,
            &group_map,
            &mut logger,
        );
        if r.get("archived").is_some() {
            n += 1;
        }
    }
    save_state(&state_file, &state);
    logger.log(&[&format!("本轮存档 {n} 个会话")]);
    0
}

fn dirs_home() -> Option<String> {
    std::env::var("HOME").ok()
}
