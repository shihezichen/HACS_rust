//! `chat_svc` —— 人机对话台（team_chat）后端 HTTP 装配（对应 Python
//! `hub/l3_domain/chat_svc.py` 的 `ChatHandler` + `main`，端口 9980）。
//!
//! 路由面（与 Python 逐条对齐）：
//! * GET/HEAD/POST `/healthz` → 健康（db 探测 + members + uptime）
//! * GET  `/api/whoami`、`/api/agents`
//! * GET  `/api/sessions`（kind/state/member/cursor/limit 过滤 + last_message）
//! * POST `/api/sessions`（建群；发起人自动进成员表）
//! * GET  `/api/sessions/{sid}`、`/api/sessions/{sid}/messages`（分页 + my_pending）
//! * GET  `/api/sessions/{sid}/todos`（分组 + sort）、`/todos/intents`
//! * GET  `/api/sessions/{sid}/members`（prefix 过滤）、`/memo`
//! * POST `/api/sessions/{sid}/messages`（门禁 + mentions 合成 + 意图闸门入队/回执翻状态）
//! * PATCH `/api/sessions/{sid}`（title/note/state=archived）
//! * POST/DELETE `/api/sessions/{sid}/members`（加人/踢人 + 系统消息留痕）
//! * PUT/PATCH `/api/sessions/{sid}/memo`（乐观锁 If-Match）
//! * POST `/api/sessions/{sid}/todos`、`/todos/clear`、`/todos/purge`、
//!   `/todos/backfill`、`/todos/enrich`（503 enricher_not_running）
//! * PATCH `/api/todos/{id}`（status 变更 / 落点）
//! * 其余 → 501 not_implemented / 404（与 Python 兜底一致）
//!
//! 鉴权：tailnet（100.64.0.0/10，HACS_TAILNET_CIDR 覆盖）免登录 = owner；
//! 其余带有效会话 cookie 才能进（本移植版只认 tailnet + `X-Agent-Id`，
//! cookie 会话走外部看板签发，Rust 版未接 check_cookie → 一律 401，PARITY 登记）。
#![allow(clippy::too_many_arguments)]

use hacs_l1_transport::web_gateway::{Gateway, HttpError, Reply};
use hacs_l3_domain::chat_svc::{
    ids, landing::normalize_landing, mentions::parse_text_mentions, store::ChatStore,
    store::DbUnavailable, classify::*, iso_bj, now_epoch, memo_etag, memo_etag_ok, RequestError,
    BODY_MAX, DEFAULT_PORT, MEMO_MAX_BYTES, MEMO_LIST_KEYS, MEMO_STR_KEYS,
    PAGE_LIMIT_DEFAULT, SESSION_NOTE_MAX,
    SESSION_TITLE_MAX, TEXT_MAX, TODO_MSG_EVIDENCE_TYPE, TODO_SORTS, TODO_STATUS,
    TODO_TEXT_MAX,
};
use serde_json::{json, Value};

const EXCERPT_LEN: usize = 80;
const MSG_LIMIT_DEFAULT: i64 = 50;
const MSG_LIMIT_MAX: i64 = 200;
const PAGE_LIMIT_MAX: i64 = 200;

// ---------------------------------------------------------------- 名册

const DISPLAY_NAMES_FALLBACK: &[(&str, &str)] = &[
    ("lobster", "龙虾"),
    ("quantum", "量子"),
    ("top001", "Top001"),
    ("openclaw", "OpenClaw"),
    ("openclaw-nb", "笔记本 OpenClaw"),
    ("claudecode", "Claude Code"),
    ("codex", "Codex"),
    ("piagent", "Pi Agent"),
    ("hermes", "Hermes"),
];

fn display_of(member_id: &str) -> String {
    for (k, v) in DISPLAY_NAMES_FALLBACK {
        if *k == member_id {
            return v.to_string();
        }
    }
    member_id.to_string()
}

struct Roster {
    members: Vec<Value>,
}

impl Roster {
    fn load() -> Roster {
        let mut paths: Vec<String> = Vec::new();
        if let Ok(v) = std::env::var("CHAT_AGENTS_JSON") {
            for p in v.split([':', ';']) {
                if !p.trim().is_empty() {
                    paths.push(p.trim().to_string());
                }
            }
        }
        if let Ok(v) = std::env::var("AGENT_TEAM_AGENTS_JSON") {
            if !v.trim().is_empty() {
                paths.push(v.trim().to_string());
            }
        }
        let mut members: Vec<Value> = Vec::new();
        let mut seen: Vec<String> = Vec::new();
        for path in paths {
            let Ok(raw) = std::fs::read_to_string(&path) else { continue };
            let Ok(data) = serde_json::from_str::<Value>(&raw) else { continue };
            let Some(agents) = data.get("agents").and_then(Value::as_object) else { continue };
            let node_id = data.get("node_id").and_then(Value::as_str).unwrap_or("");
            let host = data.get("host").and_then(Value::as_str).unwrap_or("");
            let node_port = data.get("port").and_then(Value::as_i64).unwrap_or(0);
            for (mid, spec) in agents {
                if seen.iter().any(|s| s == mid) {
                    continue;
                }
                seen.push(mid.clone());
                members.push(json!({
                    "id": mid,
                    "display": spec.get("display").and_then(Value::as_str)
                        .map(str::to_string).unwrap_or_else(|| display_of(mid)),
                    "node_id": node_id, "host": host, "node_port": node_port,
                    "model": spec.get("model").and_then(Value::as_str).unwrap_or(""),
                    "description": spec.get("description").and_then(Value::as_str).unwrap_or(""),
                }));
            }
        }
        Roster { members }
    }
    fn ids(&self) -> Vec<String> {
        self.members.iter().filter_map(|m| m["id"].as_str().map(String::from)).collect()
    }
    fn agent_ids(&self) -> Vec<String> {
        self.ids()
    }
    fn get(&self, mid: &str) -> Option<&Value> {
        self.members.iter().find(|m| m["id"].as_str() == Some(mid))
    }
}

// ---------------------------------------------------------------- 访客

#[derive(Clone, Debug)]
struct Visitor {
    username: Option<String>,
    role: String,
    source: String,
}

impl Visitor {
    fn caps(&self) -> Value {
        json!({
            "can_admin": self.role == "owner",
            "can_write": self.role == "owner" || self.role == "member",
        })
    }
}

fn in_tailnet(ip: &str) -> bool {
    // HACS_TAILNET_CIDR 覆盖（与 view_auth 同名同语义；支持 100.64.0.0/10 或 /32 等前缀）
    let cidr = std::env::var("HACS_TAILNET_CIDR").unwrap_or_default();
    // peer 形如 "127.0.0.1:54321"：剥掉端口（IPv6 [::1]:p 也剥）
    let mut text = ip.trim().to_string();
    if text.starts_with('[') {
        if let Some(end) = text.find(']') {
            text = text[1..end].to_string();
        }
    } else if let Some((h, _)) = text.rsplit_once(':') {
        if h.parse::<std::net::Ipv4Addr>().is_ok() {
            text = h.to_string();
        }
    }
    if text.starts_with('[') && text.ends_with(']') {
        text = text[1..text.len() - 1].to_string();
    }
    if let Some(mapped) = text.strip_prefix("::ffff:") {
        text = mapped.to_string();
    }
    let Some(addr) = text.parse::<std::net::Ipv4Addr>().ok() else { return false };
    if !cidr.is_empty() {
        if let Some((net, bits)) = cidr.split_once('/') {
            if let (Ok(net_addr), Ok(bits)) = (net.parse::<std::net::Ipv4Addr>(), bits.parse::<u32>()) {
                let mask = if bits == 0 { 0 } else { u32::MAX << (32 - bits) };
                let a = u32::from(addr) & mask;
                let n = u32::from(net_addr) & mask;
                return a == n;
            }
        } else if let Ok(net_addr) = cidr.parse::<std::net::Ipv4Addr>() {
            return addr == net_addr;
        }
    }
    let o = addr.octets();
    o[0] == 100 && (64..=127).contains(&o[1]) // 100.64.0.0/10
}

fn speaker(v: &Visitor, agent_hdr: Option<&str>, default_human: &str) -> (Option<String>, Option<String>) {
    if let Some(a) = agent_hdr {
        if !a.trim().is_empty() && v.source == "tailnet" {
            return (Some("agent".into()), Some(a.trim().to_string()));
        }
    }
    if v.source == "session" {
        if let Some(u) = &v.username {
            return (Some("human".into()), Some(u.clone()));
        }
    }
    if v.source == "tailnet" {
        return (Some("human".into()), Some(default_human.to_string()));
    }
    (None, None)
}

fn resolve_visitor(client_ip: &str) -> Option<Visitor> {
    if in_tailnet(client_ip) {
        return Some(Visitor { username: None, role: "owner".into(), source: "tailnet".into() });
    }
    None // cookie 会话：Rust 版未接 check_cookie（PARITY 登记）
}

// ---------------------------------------------------------------- 小工具

fn excerpt(text: &str, limit: usize) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= limit {
        flat
    } else {
        let cut: String = flat.chars().take(limit).collect();
        format!("{cut}…")
    }
}

fn clamp_i64(raw: Option<i64>, low: i64, high: i64, default: i64) -> i64 {
    match raw {
        Some(n) => n.clamp(low, high),
        None => default,
    }
}

fn one(q: &str, key: &str) -> Option<String> {
    for pair in q.split('&') {
        let (k, v) = pair.split_once('=')?;
        if k == key {
            return Some(v.to_string());
        }
    }
    None
}

fn server_time() -> Value {
    json!(iso_bj(now_epoch()))
}

fn db_err(e: DbUnavailable) -> HttpError {
    eprintln!("chat_svc DB 不可用：{}", e.0);
    HttpError::json(503, json!({"ok": false, "error": {"code": "upstream_unavailable", "message": "数据库暂不可用"}}))
}

fn req_err(e: RequestError) -> HttpError {
    let mut body = json!({"ok": false, "error": {
        "code": e.code, "message": e.message}});
    for (k, v) in &e.extra {
        if let Some(obj) = body["error"].as_object_mut() {
            obj.insert(k.clone(), v.clone());
        }
    }
    HttpError::json(e.status, body)
}

fn not_impl(method: &str, path: &str) -> HttpError {
    HttpError::json(501, json!({"ok": false, "error": {
        "code": "not_implemented",
        "message": format!("本版尚未实现（契约 §12.4 实现顺序中）：{method} {path}")}}))
}

// ---------------------------------------------------------------- 运行时

struct Rt {
    db: ChatStore,
    roster: Roster,
    default_human: String,
    started_at: i64,
}

impl Rt {
    fn get_session(&self, sid: &str, role: &str) -> Result<Option<Value>, HttpError> {
        let row = self.db.get_session_row(sid).map_err(db_err)?;
        let Some(row) = row else { return Ok(None) };
        let s = self.db.session_json(&row).map_err(db_err)?;
        // 密级：owner 全看；认不出来的按 internal（role_can_see 口径）
        let sens = s["sensitivity"].as_str().unwrap_or("internal");
        let rank_role = match role { "owner" => 3, "member" => 2, _ => 1 };
        let rank_sens = match sens { "public" => 1, _ => 2 };
        if rank_role >= rank_sens { Ok(Some(s)) } else { Ok(None) }
    }

    fn can_read(&self, v: &Visitor, session: &Value, agent_hdr: Option<&str>, default_human: &str) -> bool {
        if v.role == "owner" {
            return true;
        }
        let (_, me) = speaker(v, agent_hdr, default_human);
        match me {
            None => true,
            Some(me) => session["member_ids"].as_array().map(|a| {
                a.iter().filter_map(|x| x.as_str()).any(|m| m == me)
            }).unwrap_or(false),
        }
    }

    fn require_visitor(req: &hacs_l1_transport::web_gateway::Request) -> Result<Visitor, HttpError> {
        resolve_visitor(&req.client).ok_or_else(|| {
            req_err(RequestError::new(401, "unauthorized", "需要登录"))
        })
    }

    fn json_body(req: &hacs_l1_transport::web_gateway::Request) -> Result<Value, HttpError> {
        if req.body.is_empty() {
            return Ok(json!({}));
        }
        if req.body.len() > BODY_MAX {
            return Err(HttpError::json(413, json!({"ok": false, "error": {"code": "payload_too_large", "message": "请求体超限"}})));
        }
        let s = std::str::from_utf8(&req.body)
            .map_err(|_| req_err(RequestError::new(400, "bad_json", "请求体不是合法 JSON")))?;
        let v: Value = serde_json::from_str(s)
            .map_err(|_| req_err(RequestError::new(400, "bad_json", "请求体不是合法 JSON")))?;
        if !v.is_object() {
            return Err(req_err(RequestError::new(400, "bad_json", "请求体必须是 JSON 对象")));
        }
        Ok(v)
    }

    fn memo_actor(&self, v: &Visitor, session: &Value, agent_hdr: Option<&str>, default_human: &str) -> Result<String, HttpError> {
        let (at, aid) = speaker(v, agent_hdr, default_human);
        let Some(aid) = aid else {
            return Err(req_err(RequestError::new(400, "agent_id_required",
                "tailnet 免登录不能以某个成员名义写入：请带 X-Agent-Id 头")));
        };
        let Some(at) = at else {
            return Err(req_err(RequestError::new(400, "agent_id_required",
                "tailnet 免登录不能以某个成员名义写入：请带 X-Agent-Id 头")));
        };
        let in_group = session["member_ids"].as_array().map(|a| {
            a.iter().filter_map(|x| x.as_str()).any(|m| m == aid)
        }).unwrap_or(false);
        if at != "system" && !in_group {
            return Err(req_err(RequestError::new(403, "not_a_member",
                "只有本群成员能改本群成员表 / 群记忆")));
        }
        Ok(aid)
    }

    fn todo_write_guard(&self, session: &Value) -> Result<(), HttpError> {
        let state = session["state"].as_str().unwrap_or("open");
        if state != "open" {
            return Err(req_err(RequestError::new(409, "read_only_archived",
                &format!("群已归档（state={state}）：待办只读，写口全关"))));
        }
        Ok(())
    }

    fn todo_out(&self, item: &Value) -> Value {
        let text = item["text"].as_str().unwrap_or("");
        let task_name = item["task_name"].as_str().unwrap_or("");
        json!({
            "id": item["id"], "text": text,
            "owner": item["owner"],
            "owner_display": display_of(item["owner"].as_str().unwrap_or("")),
            "kind": item["kind"], "status": item["status"],
            "deadline": item["deadline"], "created_at": item["created_at"],
            "created_by": item["created_by"], "cleared_at": item["cleared_at"],
            "cleared_by": item["cleared_by"], "evidences": item["evidences"],
            "landing": item["landing"],
            "intent": item["intent"].as_str().filter(|s| !s.is_empty()).unwrap_or("task"),
            "followup_count": item["followup_count"].as_i64().unwrap_or(0),
            "task_name": if task_name.is_empty() { todo_task_name(text) } else { task_name.to_string() },
            "task_summary": item["task_summary"].as_str().unwrap_or(""),
            "enrich_state": item["enrich_state"].as_str().unwrap_or(""),
        })
    }
}

// ---- 会话拆 sid：/api/sessions/{sid}/xxx -------------------------------
fn split_session_path(path: &str) -> Option<(String, String)> {
    let rest = path.strip_prefix("/api/sessions/")?;
    let (sid, tail) = match rest.split_once('/') {
        Some((a, b)) => (a, b),
        None => (rest, ""),
    };
    if sid.is_empty() || sid.len() > 64
        || !sid.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return None;
    }
    Some((sid.to_string(), tail.to_string()))
}

fn split_todo_path(path: &str) -> Option<String> {
    let tid = path.strip_prefix("/api/todos/")?;
    if tid.is_empty() || tid.len() > 64
        || !tid.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return None;
    }
    Some(tid.to_string())
}

// ==================================================================== main

fn main() {
    let mut args = std::env::args().skip(1);
    let (mut host, mut port) = ("127.0.0.1".to_string(), DEFAULT_PORT);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--host" => host = args.next().unwrap_or(host),
            "--port" => port = args.next().and_then(|v| v.parse().ok()).unwrap_or(port),
            _ => {}
        }
    }
    let dsn = hacs_l3_domain::chat_svc::store::resolve_dsn();
    let db = match ChatStore::connect(&dsn) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("chat_svc 连不上数据库：{}", e.0);
            std::process::exit(1);
        }
    };
    if let Err(e) = db.ensure_schema() {
        eprintln!("chat_svc 建表告警（不阻断）：{e}");
    }
    let rt = std::sync::Arc::new(Rt {
        db,
        roster: Roster::load(),
        default_human: std::env::var("AGENT_TEAM_HUMAN_ID")
            .or_else(|_| std::env::var("CHAT_HUMAN_ID"))
            .unwrap_or_else(|_| "<human-id>".into()),
        started_at: now_epoch(),
    });

    let mut gw = Gateway::new("chat_svc", Some("2.0"), true);
    gw.protocol_version = "HTTP/1.1".to_string();

    // ---- /healthz（GET/HEAD 公开口 + POST/PUT/PATCH/DELETE 同样 200，Python 原样）----
    for methods in [&["GET", "HEAD"][..], &["POST", "PUT", "PATCH", "DELETE"][..]] {
        let rt_h = rt.clone();
        gw.route("/healthz", methods, false, move |req| {
            let db_ok = rt_h.db.healthz_probe();
            let body = json!({
                "ok": true, "service": "agent-team-chat", "version": "2.0",
                "phase": "group", "db": db_ok,
                "members": rt_h.roster.ids().len(),
                "presence": {"nodes": {}},
                "enrich": {"running": false},
                "uptime_s": now_epoch() - rt_h.started_at,
                "server_time": server_time(),
            });
            let _ = req;
            Ok(Reply::json(200, body))
        });
    }

    // ---- GET /api/whoami、/api/agents -----------------------------------
    gw.route("/api/whoami", &["GET", "HEAD"], false, move |req| {
        let v = Rt::require_visitor(req)?;
        let (name, disp) = match &v.username {
            Some(u) => (u.clone(), display_of(u)),
            None => (v.source.clone(), v.source.clone()),
        };
        Ok(Reply::json(200, json!({
            "ok": true,
            "user": {"name": name, "display": disp, "role": v.role,
                     "role_cn": if v.role == "owner" { "管理员" } else { "只读访客" },
                     "source": v.source},
            "caps": v.caps(), "server_time": server_time(),
        })))
    });
    let rt_h = rt.clone();
    gw.route("/api/agents", &["GET", "HEAD"], false, move |req| {
        let v = Rt::require_visitor(req)?;
        let _ = v;
        let mut items: Vec<Value> = rt_h.roster.members.iter().map(|m| json!({
            "id": m["id"], "display": m["display"],
            "online": Value::Null, "busy": Value::Null, "last_hb_at": Value::Null,
            "meta": {"node_id": m["node_id"], "host": m["host"],
                     "model": m["model"], "description": m["description"]},
        })).collect();
        items.sort_by(|a, b| a["id"].as_str().unwrap_or("").cmp(b["id"].as_str().unwrap_or("")));
        Ok(Reply::json(200, json!({"ok": true, "agents": items, "server_time": server_time()})))
    });

    // ---- GET /api/sessions（列表）---------------------------------------
    let rt_h = rt.clone();
    gw.route("/api/sessions", &["GET", "HEAD"], false, move |req| {
        let v = Rt::require_visitor(req)?;
        let limit = clamp_i64(one(&req.query, "limit").and_then(|s| s.parse().ok()),
            1, PAGE_LIMIT_MAX, PAGE_LIMIT_DEFAULT);
        let kind = one(&req.query, "kind").filter(|s| !s.is_empty());
        if let Some(k) = &kind {
            if k != "dm" && k != "meet" {
                return Err(req_err(RequestError::new(422, "bad_kind", "kind 只能是 dm / meet")));
            }
        }
        let state = one(&req.query, "state").filter(|s| !s.is_empty());
        let member = one(&req.query, "member").filter(|s| !s.is_empty());
        let cursor = one(&req.query, "cursor").filter(|s| !s.is_empty());
        let mut sessions = rt_h.db.list_sessions(
            kind.as_deref(), state.as_deref(), member.as_deref(), cursor.as_deref(), limit,
        ).map_err(db_err)?;
        // 密级过滤
        sessions.retain(|s| rt_h.get_session(s["session_id"].as_str().unwrap_or(""), &v.role)
            .ok().flatten().is_some());
        let mut out: Vec<Value> = Vec::new();
        for s in sessions {
            let sid = s["session_id"].as_str().unwrap_or("").to_string();
            let mut row = s.clone();
            row.as_object_mut().map(|o| o.remove("member_ids"));
            let last = rt_h.db.last_message(&sid).ok().flatten();
            row["last_message"] = match last {
                Some(l) => {
                    let m = ChatStore::message_json(&l);
                    json!({
                        "seq": m["seq"], "author": m["author"],
                        "excerpt": excerpt(m["text"].as_str().unwrap_or(""), EXCERPT_LEN),
                        "created_at": m["created_at"],
                    })
                }
                None => Value::Null,
            };
            row["unread"] = Value::Null;
            row["observing"] = json!(false);
            row["my_role"] = json!(v.role);
            out.push(row);
        }
        let next_cursor = if out.len() as i64 == limit && !out.is_empty() {
            out.last().and_then(|o| o["updated_at"].as_str().map(String::from))
        } else {
            None
        };
        Ok(Reply::json(200, json!({"ok": true, "count": out.len(),
            "next_cursor": next_cursor, "sessions": out, "server_time": server_time()})))
    });

    // ---- POST /api/sessions（建群）--------------------------------------
    let rt_h = rt.clone();
    gw.route("/api/sessions", &["POST"], false, move |req| {
        let v = Rt::require_visitor(req)?;
        let body = Rt::json_body(req)?;
        let agent_hdr = req.header("X-Agent-Id");
        let (at, aid) = speaker(&v, agent_hdr, &rt_h.default_human);
        if at.as_deref() != Some("human") {
            return Err(req_err(RequestError::new(403, "creator_required", "建群必须由人发起（请先登录）")));
        }
        let author_id = aid.unwrap_or_default();
        let kind = body.get("kind").and_then(Value::as_str).unwrap_or("meet").trim().to_string();
        if kind != "dm" && kind != "meet" {
            return Err(req_err(RequestError::new(422, "bad_kind", "kind 只能是 dm / meet")));
        }
        let roster_ids = rt_h.roster.ids();
        let wanted = body.get("members");
        let wanted: Vec<Value> = match wanted {
            None => vec![],
            Some(Value::Array(a)) => a.clone(),
            Some(_) => return Err(req_err(RequestError::new(422, "bad_members", "members 必须是数组"))),
        };
        let mut members = vec![author_id.clone()];
        for raw in wanted {
            let mid = raw.as_str().unwrap_or("").trim().to_string();
            if mid.is_empty() || members.contains(&mid) {
                continue;
            }
            if !roster_ids.contains(&mid) {
                return Err(req_err(RequestError::new(422, "unknown_member",
                &format!("成员不存在：{mid}"))));
            }
            members.push(mid);
        }
        let now = iso_bj(now_epoch());
        let mm = &now[5..7];
        let dd = &now[8..10];
        let hh = &now[11..13];
        let mi = &now[14..16];
        let title = body.get("title").and_then(Value::as_str).map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.chars().take(120).collect::<String>())
            .unwrap_or_else(|| format!("临时群 · {mm}月{dd}日 {hh}:{mi}"));
        let session_id = ids::gen_session_id("meet");
        let created = rt_h.db.create_session(&session_id, &kind, &title, &members,
            &author_id, "internal",
            body.get("client_ref").and_then(Value::as_str));
        let row = created.map_err(db_err)?;
        let Some(row) = row else {
            return Err(req_err(RequestError::new(500, "create_failed", "建群失败")));
        };
        let session = rt_h.db.session_json(&row).map_err(db_err)?;
        Ok(Reply::json(201, json!({"ok": true, "session": session, "server_time": server_time()})))
    });

    // ---- /api/sessions/{sid}/... 写端点（前缀登记，二次判形）--------------
    let rt_h = rt.clone();
    gw.route("/api/sessions", &["POST", "PUT", "PATCH", "DELETE"], true, move |req| {
        let v = Rt::require_visitor(req)?;
        let body = Rt::json_body(req)?;
        let agent_hdr = req.header("X-Agent-Id");
        let path = req.path.clone();
        let Some((sid, tail)) = split_session_path(&path) else {
            return Err(not_impl(&req.method, &req.path));
        };
        let method = req.method.as_str();

        // POST messages —— 群内发言
        if tail == "messages" && method == "POST" {
            let session = rt_h.get_session(&sid, &v.role)?
                .ok_or_else(|| req_err(RequestError::new(404, "not_found", "会话不存在")))?;
            let (at, aid) = speaker(&v, agent_hdr, &rt_h.default_human);
            let (Some(at), Some(author_id)) = (at, aid) else {
                return Err(req_err(RequestError::new(400, "agent_id_required",
                    "tailnet 免登录不能以某个成员名义发言：请带 X-Agent-Id 头")));
            };
            let author_id = author_id.clone();
            let members: Vec<String> = session["member_ids"].as_array().map(|a| {
                a.iter().filter_map(|x| x.as_str().map(String::from)).collect()
            }).unwrap_or_default();
            if !members.contains(&author_id) {
                return Err(req_err(RequestError::new(404, "not_found", "会话不存在")));
            }
            if session["state"].as_str() != Some("open") {
                return Err(req_err(RequestError::new(409, "session_closed",
                    "群已归档，不能再发言（规则 4：由发起人宣布结束）")));
            }
            let text = body.get("text").and_then(Value::as_str).unwrap_or("").trim().to_string();
            if text.is_empty() {
                return Err(req_err(RequestError::new(422, "empty_text", "text 不能为空")));
            }
            if text.chars().count() > TEXT_MAX {
                return Err(req_err(RequestError::new(422, "text_too_long",
                &format!("text 超过 {TEXT_MAX} 字"))));
            }
            let mut mentions: Vec<String> = match body.get("mentions") {
                None => if at == "agent" { vec![] } else { vec!["@all".into()] },
                Some(Value::Array(a)) => a.iter().filter_map(|m| m.as_str())
                    .map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect(),
                Some(_) => return Err(req_err(RequestError::new(422, "bad_mentions", "mentions 必须是数组"))),
            };
            for target in parse_text_mentions(&text, &members) {
                if !mentions.contains(&target) {
                    mentions.push(target);
                }
            }
            if mentions.is_empty() {
                if at == "agent" {
                    return Err(req_err(RequestError::new(422, "mention_required",
                        "Agent 发言必须 @ 至少一个目标（@所有人 写 \"@all\"）")));
                }
                mentions = vec!["@all".into()];
            }
            for target in &mentions {
                if target == "@all" { continue; }
                if !members.contains(target) {
                    return Err(req_err(RequestError::new(422, "unknown_mention",
                &format!("不在群里的目标：{target}"))));
                }
            }
            let audience: Vec<String> = if mentions.iter().any(|m| m == "@all") {
                members.clone()
            } else {
                members.iter().filter(|m| mentions.contains(m)).cloned().collect()
            };
            let needs_response: Vec<String> =
                audience.iter().filter(|m| *m != &author_id).cloned().collect();
            // todo 声明：先校验再落消息
            let decl = body.get("todo").filter(|d| !d.is_null());
            let mut decl_plan: Option<Value> = None;
            if let Some(d) = decl {
                if at != "agent" {
                    return Err(req_err(RequestError::new(403, "todo_agent_only",
                        "todo 声明只能由 agent 带（X-Agent-Id）一起发")));
                }
                decl_plan = Some(todo_declare_plan(&rt_h, &sid, &author_id, d)?);
            }
            let row = rt_h.db.add_message(&sid, &at, &author_id, &text, &mentions,
                &audience, &needs_response,
                body.get("reply_to").and_then(Value::as_str),
                body.get("client_msg_id").and_then(Value::as_str)).map_err(db_err)?;
            let Some(row) = row else {
                return Err(req_err(RequestError::new(500, "write_failed", "发言落库失败")));
            };
            let message = ChatStore::message_json(&row);
            // 1) 人 @agent 派活 → 自动入队（意图闸门）
            let queued = todo_autotrack(&rt_h, &session, &sid, &author_id, &mentions, &message);
            // 2) agent 声明 → 翻状态
            let decl_item = match &decl_plan {
                Some(p) => todo_declare_apply(&rt_h, p).ok(),
                None => None,
            };
            // 3) agent 回群报进度 → 翻自己的在办条目
            let tracked = todo_reply_autotrack(&rt_h, &sid, &author_id, &text, &message,
                decl_plan.as_ref());
            return Ok(Reply::json(201, json!({
                "ok": true, "message": message,
                "queued": queued.map(|q| rt_h.todo_out(&q)),
                "todo": decl_item.map(|d| rt_h.todo_out(&d)),
                "tracked": tracked.iter().map(|t| rt_h.todo_out(t)).collect::<Vec<_>>(),
                "intent": Value::Null,
                "server_time": server_time(),
            })));
        }

        // PATCH /api/sessions/{sid} —— 改群名/备注/归档
        if tail.is_empty() && method == "PATCH" {
            let session = rt_h.get_session(&sid, &v.role)?
                .ok_or_else(|| req_err(RequestError::new(404, "not_found", "会话不存在")))?;
            if !rt_h.can_read(&v, &session, agent_hdr, &rt_h.default_human) {
                return Err(req_err(RequestError::new(404, "not_found", "会话不存在")));
            }
            let mut fields_title: Option<String> = None;
            let mut fields_note: Option<String> = None;
            if let Some(t) = body.get("title") {
                if !t.is_string() {
                    return Err(req_err(RequestError::new(422, "bad_title", "title 必须是字符串")));
                }
                let text = t.as_str().unwrap_or("").trim().to_string();
                if text.is_empty() {
                    return Err(req_err(RequestError::new(422, "bad_title", "title 不能为空（群名至少 1 个字）")));
                }
                let n = text.chars().count();
                if n > SESSION_TITLE_MAX {
                    return Err(req_err(RequestError::new(422, "title_too_long",
                &format!("title 超过上限 {SESSION_TITLE_MAX} 字（收到 {n} 字），未写入"))
                        .with_extra("field", json!("title")).with_extra("max", json!(SESSION_TITLE_MAX)).with_extra("len", json!(n))));
                }
                fields_title = Some(text);
            }
            if let Some(t) = body.get("note") {
                if !t.is_string() {
                    return Err(req_err(RequestError::new(422, "bad_note", "note 必须是字符串")));
                }
                let text = t.as_str().unwrap_or("").trim().to_string();
                let n = text.chars().count();
                if n > SESSION_NOTE_MAX {
                    return Err(req_err(RequestError::new(422, "note_too_long",
                &format!("note 超过上限 {SESSION_NOTE_MAX} 字（收到 {n} 字），未写入"))
                        .with_extra("field", json!("note")).with_extra("max", json!(SESSION_NOTE_MAX)).with_extra("len", json!(n))));
                }
                fields_note = Some(text);
            }
            let has_state = body.get("state").is_some();
            if !has_state && fields_title.is_none() && fields_note.is_none() {
                return Err(req_err(RequestError::new(422, "empty_patch",
                    "PATCH 至少要带一个要改的字段（title / note / state）")));
            }
            let mut new_state: Option<String> = None;
            if has_state {
                let s = body["state"].as_str().unwrap_or("").trim().to_string();
                if s != "archived" {
                    return Err(req_err(RequestError::new(422, "bad_state", "本版只支持 state=archived（关群 = 归档）")));
                }
                let is_owner = v.username.as_deref()
                    .map(|u| u == session["created_by"].as_str().unwrap_or("")).unwrap_or(false);
                if !is_owner {
                    return Err(req_err(RequestError::new(403, "only_owner_can_close", "只有群的发起人能宣布结束")));
                }
                new_state = Some(s);
            }
            let actor = if fields_title.is_some() || fields_note.is_some() {
                rt_h.memo_actor(&v, &session, agent_hdr, &rt_h.default_human)?
            } else {
                String::new()
            };
            let updated = rt_h.db.set_session_meta(&sid,
                fields_title.as_deref(), fields_note.as_deref(), new_state.as_deref(), &actor)
                .map_err(db_err)?;
            let Some((row, changed)) = updated else {
                return Err(req_err(RequestError::new(500, "write_failed", "改群名 / 备注失败")));
            };
            let session = rt_h.db.session_json(&row).map_err(db_err)?;
            return Ok(Reply::json(200, json!({"ok": true, "session": session,
                "changed": changed, "server_time": server_time()})));
        }

        // POST/DELETE members —— 加人 / 踢人
        if tail == "members" && (method == "POST" || method == "DELETE") {
            let session = rt_h.get_session(&sid, &v.role)?
                .ok_or_else(|| req_err(RequestError::new(404, "not_found", "会话不存在")))?;
            if !rt_h.can_read(&v, &session, agent_hdr, &rt_h.default_human) {
                return Err(req_err(RequestError::new(404, "not_found", "会话不存在")));
            }
            let actor = rt_h.memo_actor(&v, &session, agent_hdr, &rt_h.default_human)?;
            let raw = body.get("members").filter(|x| !x.is_null())
                .or_else(|| body.get("member").filter(|x| !x.is_null()));
            let Some(raw) = raw else {
                return Err(req_err(RequestError::new(422, "bad_members", "members 必须是数组（或给 member 单个 id）")));
            };
            let ids_in: Vec<String> = match raw {
                Value::String(s) => vec![s.clone()],
                Value::Array(a) => a.iter().map(|x| x.as_str().unwrap_or("").trim().to_string())
                    .filter(|s| !s.is_empty()).collect(),
                _ => return Err(req_err(RequestError::new(422, "bad_members", "members 必须是数组"))),
            };
            if ids_in.is_empty() {
                return Err(req_err(RequestError::new(422, "bad_members", "members 不能为空")));
            }
            let outcome;
            let verb;
            let moved: Vec<String>;
            if method == "POST" {
                let known = rt_h.roster.ids();
                for mid in &ids_in {
                    if !known.contains(mid) {
                        return Err(req_err(RequestError::new(422, "unknown_member",
                &format!("成员不存在：{mid}"))));
                    }
                }
                outcome = rt_h.db.members_write(&sid, &ids_in, &[], &actor).map_err(req_err)?;
                verb = "加入";
                moved = outcome["added"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
            } else {
                let is_creator = v.username.as_deref()
                    .map(|u| u == session["created_by"].as_str().unwrap_or("")).unwrap_or(false);
                if !is_creator && v.role != "owner" {
                    return Err(req_err(RequestError::new(403, "only_owner_can_kick", "只有群发起人（或有管理权的人）能踢人")));
                }
                outcome = rt_h.db.members_write(&sid, &[], &ids_in, &actor).map_err(req_err)?;
                verb = "移出";
                moved = outcome["removed"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
            }
            let updated = outcome["session"].clone();
            if !moved.is_empty() {
                let detail = moved.join("、");
                let extra = if method == "DELETE" {
                    let mut bits: Vec<String> = Vec::new();
                    let voided = outcome["voided"].as_i64().unwrap_or(0);
                    if voided > 0 { bits.push(format!("{voided} 条待回应已随其离群失效（无需再回）")); }
                    let cleared = outcome["cleared_todos"].as_i64().unwrap_or(0);
                    if cleared > 0 { bits.push(format!("{cleared} 条未结待办已结清")); }
                    if bits.is_empty() { String::new() } else { format!("；{}", bits.join("；")) }
                } else {
                    String::new()
                };
                let member_ids: Vec<String> = updated["member_ids"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
                let _ = rt_h.db.add_message(&sid, "system", "system",
                    &format!("【成员变更】{detail} 被 {actor} {verb}{extra}"),
                    &[], &member_ids, &[], None, None);
            }
            return Ok(Reply::json(200, json!({
                "ok": true, "session_id": sid,
                "members": updated["members"],
                "added": outcome["added"], "removed": outcome["removed"],
                "voided": outcome["voided"], "cleared_todos": outcome["cleared_todos"],
                "server_time": server_time(),
            })));
        }

        // PUT/PATCH memo —— 群记忆锚
        if tail == "memo" && (method == "PUT" || method == "PATCH") {
            let session = rt_h.get_session(&sid, &v.role)?
                .ok_or_else(|| req_err(RequestError::new(404, "not_found", "会话不存在")))?;
            if !rt_h.can_read(&v, &session, agent_hdr, &rt_h.default_human) {
                return Err(req_err(RequestError::new(404, "not_found", "会话不存在")));
            }
            let actor = rt_h.memo_actor(&v, &session, agent_hdr, &rt_h.default_human)?;
            let current = rt_h.db.memo_get(&sid).map_err(db_err)?;
            let (doc, action) = if method == "PUT" {
                (memo_doc(&body)?, "put")
            } else {
                let patch = body.get("patch").filter(|p| p.is_object()).unwrap_or(&body);
                (memo_merge(current["memo"].clone(), patch)?, "patch")
            };
            let if_match = req.header("If-Match").unwrap_or("");
            let cur_version = current["version"].as_i64().unwrap_or(0);
            let cur_etag = current["etag"].as_str().unwrap_or("");
            if !memo_etag_ok(cur_version, cur_etag, if_match) {
                return Err(req_err(RequestError::new(409, "memo_conflict",
                    "本群记忆已被他人更新，已为你刷新到最新版本，请确认后重试")
                    .with_extra("etag", current["etag"].clone())
                    .with_extra("version", current["version"].clone())
                    .with_extra("updated_by", current["updated_by"].clone())
                    .with_extra("memo", current["memo"].clone())));
            }
            let new_version = cur_version + 1;
            let etag = memo_etag(&sid, new_version, &doc);
            let (ok, state) = rt_h.db.memo_write(&sid, &doc, &actor, if_match, action).map_err(db_err)?;
            if !ok {
                return Err(req_err(RequestError::new(409, "memo_conflict",
                    "本群记忆已被他人更新，已为你刷新到最新版本，请确认后重试")
                    .with_extra("etag", state["etag"].clone())
                    .with_extra("version", state["version"].clone())
                    .with_extra("updated_by", state["updated_by"].clone())
                    .with_extra("memo", state["memo"].clone())));
            }
            let _ = (new_version, etag);
            return Ok(Reply::json(200, json!({
                "ok": true, "session_id": sid,
                "memo": state["memo"], "version": state["version"],
                "etag": state["etag"], "updated_at": state["updated_at"],
                "updated_by": state["updated_by"], "server_time": server_time(),
            })));
        }

        // POST todos —— 新增条目
        if tail == "todos" && method == "POST" {
            let session = rt_h.get_session(&sid, &v.role)?
                .ok_or_else(|| req_err(RequestError::new(404, "not_found", "会话不存在")))?;
            if !rt_h.can_read(&v, &session, agent_hdr, &rt_h.default_human) {
                return Err(req_err(RequestError::new(404, "not_found", "会话不存在")));
            }
            let actor = rt_h.memo_actor(&v, &session, agent_hdr, &rt_h.default_human)?;
            rt_h.todo_write_guard(&session)?;
            let allowed = ["text", "kind", "owner", "deadline", "landing"];
            for k in body.as_object().map(|o| o.keys().cloned().collect::<Vec<_>>()).unwrap_or_default() {
                if !allowed.contains(&k.as_str()) {
                    return Err(req_err(RequestError::new(422, "bad_todo",
                &format!("未知字段：{k}"))));
                }
            }
            let text = body.get("text").and_then(Value::as_str).unwrap_or("").trim().to_string();
            if text.is_empty() {
                return Err(req_err(RequestError::new(422, "bad_todo", "text 不能为空")));
            }
            if text.chars().count() > TODO_TEXT_MAX {
                return Err(req_err(RequestError::new(422, "bad_todo",
                &format!("text 超过 {TODO_TEXT_MAX} 字"))));
            }
            let kind = body.get("kind").and_then(Value::as_str).unwrap_or("confirm").trim().to_string();
            let kind = if kind.is_empty() { "confirm".to_string() } else { kind };
            if !["confirm", "review", "decide"].contains(&kind.as_str()) {
                return Err(req_err(RequestError::new(422, "bad_todo", "kind 只能是 confirm、review、decide")));
            }
            let owner = body.get("owner").and_then(Value::as_str).unwrap_or("").trim().to_string();
            let owner = if owner.is_empty() { actor.clone() } else { owner };
            if owner != actor {
                return Err(req_err(RequestError::new(403, "not_your_field",
                &format!("只能给自己加条目（owner={actor}），不能替别人加"))));
            }
            let deadline = match body.get("deadline").filter(|d| !d.is_null()) {
                None => None,
                Some(d) => {
                    let raw = d.as_str().unwrap_or("");
                    let ep = hacs_l3_domain::chat_svc::parse_time(raw);
                    if ep.is_none() {
                        return Err(req_err(RequestError::new(422, "bad_deadline",
                            "deadline 需 ISO8601（如 2026-09-23T18:00:00+08:00）")));
                    }
                    ep.map(|e| std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(e as u64))
                }
            };
            let (landing, _ev) = normalize_landing(body.get("landing")).map_err(req_err)?;
            let item = rt_h.db.todo_create(&sid, &text, &owner, &kind, deadline, &actor,
                &landing, "task", "", "").map_err(db_err)?;
            let Some(item) = item else {
                return Err(req_err(RequestError::new(500, "write_failed", "入队失败")));
            };
            let count = rt_h.db.todos_list(&sid, None, None).map_err(db_err)?.len();
            return Ok(Reply::json(201, json!({"ok": true, "session_id": sid,
                "item": rt_h.todo_out(&item), "count": count, "server_time": server_time()})));
        }

        // POST todos/clear —— 批量勾选递交
        if tail == "todos/clear" && method == "POST" {
            let session = rt_h.get_session(&sid, &v.role)?
                .ok_or_else(|| req_err(RequestError::new(404, "not_found", "会话不存在")))?;
            if !rt_h.can_read(&v, &session, agent_hdr, &rt_h.default_human) {
                return Err(req_err(RequestError::new(404, "not_found", "会话不存在")));
            }
            let actor = rt_h.memo_actor(&v, &session, agent_hdr, &rt_h.default_human)?;
            rt_h.todo_write_guard(&session)?;
            let allowed = ["ids", "landing", "evidence", "evidences"];
            for k in body.as_object().map(|o| o.keys().cloned().collect::<Vec<_>>()).unwrap_or_default() {
                if !allowed.contains(&k.as_str()) {
                    return Err(req_err(RequestError::new(422, "bad_todos",
                &format!("未知字段：{k}"))));
                }
            }
            let raw_ids = body.get("ids");
            let ids_in: Vec<String> = match raw_ids {
                Some(Value::String(s)) => vec![s.clone()],
                Some(Value::Array(a)) if !a.is_empty() => a.iter().map(|x| x.as_str().unwrap_or("").trim().to_string()).filter(|s| !s.is_empty()).collect(),
                _ => return Err(req_err(RequestError::new(422, "bad_todos", "ids 必须是非空数组"))),
            };
            if ids_in.is_empty() {
                return Err(req_err(RequestError::new(422, "bad_todos", "ids 不能为空")));
            }
            let raw = body.get("landing").filter(|x| !x.is_null())
                .or_else(|| body.get("evidences").filter(|x| !x.is_null()))
                .or_else(|| body.get("evidence").filter(|x| !x.is_null()));
            let (landing, evidences) = normalize_landing(raw).map_err(req_err)?;
            // 落点校验：本次真会被清的条目缺落点 → 整批拒
            let rows = rt_h.db.todos_rows(&sid, &ids_in).map_err(db_err)?;
            let target: Vec<&Value> = rows.iter()
                .filter(|r| r["owner"] == json!(actor) && r["status"] == json!("open")).collect();
            if landing.is_empty() && evidences.is_empty() {
                let missing: Vec<String> = target.iter()
                    .filter(|r| r["landing"].as_array().map(|a| a.is_empty()).unwrap_or(true)
                        && r["evidences"].as_array().map(|a| a.is_empty()).unwrap_or(true))
                    .filter_map(|r| r["id"].as_str().map(String::from)).collect();
                if !missing.is_empty() {
                    let mut m = missing.clone();
                    m.sort();
                    m.truncate(8);
                    return Err(req_err(RequestError::new(422, "no_landing",
                &format!("不能自盖完成章：批量勾选递交需先交落点（landing）——commit 哈希 / 字面 curl+HTTP 码 / 文件路径+时间戳 / 重启 PID+时间戳；以下条目既无落点、本次也没带：{}（一条都没清）", m.join("、")))));
                }
            }
            let now = iso_bj(now_epoch());
            let stamped: Vec<Value> = evidences.iter().map(|e| {
                let mut c = e.clone();
                c["by"] = json!(actor);
                c["validated_at"] = json!(now);
                c
            }).collect();
            let (cleared, skipped) = rt_h.db.todos_clear(&sid, &ids_in, &actor, &landing, &stamped).map_err(db_err)?;
            return Ok(Reply::json(200, json!({
                "ok": true, "session_id": sid,
                "cleared": cleared, "skipped": skipped,
                "changed": cleared.len(), "validated": !landing.is_empty() || !evidences.is_empty(),
                "landing_count": landing.len(), "server_time": now,
            })));
        }

        // POST todos/purge —— 队列级清空
        if tail == "todos/purge" && method == "POST" {
            let session = rt_h.get_session(&sid, &v.role)?
                .ok_or_else(|| req_err(RequestError::new(404, "not_found", "会话不存在")))?;
            if !rt_h.can_read(&v, &session, agent_hdr, &rt_h.default_human) {
                return Err(req_err(RequestError::new(404, "not_found", "会话不存在")));
            }
            let actor = rt_h.memo_actor(&v, &session, agent_hdr, &rt_h.default_human)?;
            rt_h.todo_write_guard(&session)?;
            let scope = body.get("scope").and_then(Value::as_str).unwrap_or("all").trim().to_string();
            let scope = if scope.is_empty() { "all".to_string() } else { scope };
            if scope != "all" {
                return Err(req_err(RequestError::new(422, "bad_scope", "scope 只支持 all（逐条完成走 /todos/clear）")));
            }
            for k in body.as_object().map(|o| o.keys().cloned().collect::<Vec<_>>()).unwrap_or_default() {
                if k != "scope" {
                    return Err(req_err(RequestError::new(422, "bad_todos",
                &format!("未知字段：{k}"))));
                }
            }
            let before = rt_h.db.todos_list(&sid, None, None).map_err(db_err)?.len();
            let removed = rt_h.db.todos_purge(&sid).map_err(db_err)?;
            let after = rt_h.db.todos_list(&sid, None, None).map_err(db_err)?.len();
            return Ok(Reply::json(200, json!({
                "ok": true, "session_id": sid, "scope": scope, "cleared_by": actor,
                "removed": removed, "changed": removed.len(),
                "before": before, "after": after, "count": after,
                "server_time": server_time(),
            })));
        }

        // POST todos/backfill —— 历史任务回填
        if tail == "todos/backfill" && method == "POST" {
            let session = rt_h.get_session(&sid, &v.role)?
                .ok_or_else(|| req_err(RequestError::new(404, "not_found", "会话不存在")))?;
            if !rt_h.can_read(&v, &session, agent_hdr, &rt_h.default_human) {
                return Err(req_err(RequestError::new(404, "not_found", "会话不存在")));
            }
            rt_h.memo_actor(&v, &session, agent_hdr, &rt_h.default_human)?;
            for k in body.as_object().map(|o| o.keys().cloned().collect::<Vec<_>>()).unwrap_or_default() {
                if k != "limit" {
                    return Err(req_err(RequestError::new(422, "bad_todos",
                &format!("未知字段：{k}"))));
                }
            }
            let limit = clamp_i64(body.get("limit").and_then(Value::as_i64), 1, 5000, 500);
            let agents = rt_h.roster.agent_ids();
            if agents.is_empty() {
                return Ok(Reply::json(200, json!({"ok": true, "session_id": sid,
                    "scanned": 0, "created": [], "skipped": 0, "items": [],
                    "server_time": server_time()})));
            }
            let members: Vec<String> = session["member_ids"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).filter(|s| !s.is_empty()).collect()).unwrap_or_default();
            let (messages, _more, _nb) = rt_h.db.list_messages(&sid, limit, None, None).map_err(db_err)?;
            let mut scanned = 0i64;
            let mut skipped = 0i64;
            let mut followups = 0i64;
            let mut created: Vec<Value> = Vec::new();
            for msg in &messages {
                let author = &msg["author"];
                let author_id = author["id"].as_str().unwrap_or("").to_string();
                if author["type"].as_str() != Some("human") || agents.contains(&author_id) {
                    continue;
                }
                let declared: Vec<String> = msg["meta"]["mentions"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
                let text = msg["text"].as_str().unwrap_or("");
                let mut targets: Vec<String> = declared.iter().filter(|m| agents.contains(m)).cloned().collect();
                for t in parse_text_mentions(text, &members) {
                    if agents.contains(&t) && !targets.contains(&t) {
                        targets.push(t);
                    }
                }
                if targets.is_empty() {
                    continue;
                }
                scanned += 1;
                let owner = targets[0].clone();
                let verdict = classify_human_intent(text);
                if verdict["intent"].as_str() != Some("task") {
                    // 回填只留台账、不动 followup_count
                    let seq = msg["seq"].as_i64().map(|s| s as i32);
                    let _ = rt_h.db.todo_skip_create(&sid, seq,
                        msg["message_id"].as_str().unwrap_or(""), &author_id, &owner,
                        verdict["intent"].as_str().unwrap_or("followup"),
                        verdict["reason"].as_str().unwrap_or(""),
                        &verdict["hits"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect::<Vec<String>>()).unwrap_or_default(),
                        text);
                    followups += 1;
                    continue;
                }
                let seq = msg["seq"].as_i64();
                let entry = todo_text_from_message(text, seq, TODO_TEXT_MAX);
                if entry.is_empty() {
                    continue;
                }
                let key = todo_dedupe_key(&entry);
                let rows = rt_h.db.todos_list(&sid, Some(&owner), None).map_err(db_err)?;
                if rows.iter().any(|r| todo_dedupe_key(r["text"].as_str().unwrap_or("")) == key) {
                    skipped += 1;
                    continue;
                }
                let item = rt_h.db.todo_create(&sid, &entry, &owner, "confirm", None,
                    &author_id, &[], "task", "", "").map_err(db_err)?;
                if let Some(item) = item {
                    created.push(rt_h.todo_out(&item));
                }
            }
            let items = rt_h.db.todos_list(&sid, None, None).map_err(db_err)?;
            let count = items.len();
            return Ok(Reply::json(200, json!({
                "ok": true, "session_id": sid, "scanned": scanned,
                "created": created, "created_count": created.len(),
                "skipped": skipped, "followup": followups, "count": count,
                "items": items.iter().map(|i| rt_h.todo_out(i)).collect::<Vec<_>>(),
                "server_time": server_time(),
            })));
        }

        // POST todos/enrich —— 模型富化回填（Rust 版无 enricher → 503）
        if tail == "todos/enrich" && method == "POST" {
            let session = rt_h.get_session(&sid, &v.role)?
                .ok_or_else(|| req_err(RequestError::new(404, "not_found", "会话不存在")))?;
            if !rt_h.can_read(&v, &session, agent_hdr, &rt_h.default_human) {
                return Err(req_err(RequestError::new(404, "not_found", "会话不存在")));
            }
            rt_h.memo_actor(&v, &session, agent_hdr, &rt_h.default_human)?;
            return Err(req_err(RequestError::new(503, "enricher_not_running",
                "模型富化 worker 没在跑：本次不排队，条目保持现状")));
        }

        Err(not_impl(&req.method, &req.path))
    });

    // ---- GET /api/sessions/{sid}/...（前缀登记）--------------------------
    let rt_h = rt.clone();
    gw.route("/api/sessions", &["GET", "HEAD"], true, move |req| {
        let v = Rt::require_visitor(req)?;
        let agent_hdr = req.header("X-Agent-Id");
        let Some((sid, tail)) = split_session_path(&req.path) else {
            return Err(not_impl(&req.method, &req.path));
        };

        // 会话详情
        if tail.is_empty() {
            let session = rt_h.get_session(&sid, &v.role)?
                .ok_or_else(|| req_err(RequestError::new(404, "not_found", "会话不存在")))?;
            if !rt_h.can_read(&v, &session, agent_hdr, &rt_h.default_human) {
                return Err(req_err(RequestError::new(404, "not_found", "会话不存在")));
            }
            let mut s = session.clone();
            s.as_object_mut().map(|o| o.remove("member_ids"));
            return Ok(Reply::json(200, json!({"ok": true, "session": s})));
        }

        // 消息列表
        if tail == "messages" {
            let session = rt_h.get_session(&sid, &v.role)?
                .ok_or_else(|| req_err(RequestError::new(404, "not_found", "会话不存在")))?;
            if !rt_h.can_read(&v, &session, agent_hdr, &rt_h.default_human) {
                return Err(req_err(RequestError::new(404, "not_found", "会话不存在")));
            }
            let limit = clamp_i64(one(&req.query, "limit").and_then(|s| s.parse().ok()),
                1, MSG_LIMIT_MAX, MSG_LIMIT_DEFAULT);
            let as_seq = |name: &str| -> Result<Option<i32>, HttpError> {
                match one(&req.query, name).filter(|s| !s.is_empty()) {
                    None => Ok(None),
                    Some(raw) => raw.parse::<i32>().map(Some).map_err(|_| {
                        req_err(RequestError::new(422, "bad_seq",
                &format!("{name} 必须是整数")))
                    }),
                }
            };
            let mut before_seq = as_seq("before_seq")?;
            let mut after_seq = as_seq("after_seq")?;
            if after_seq.is_none() {
                after_seq = as_seq("since")?;
            }
            if before_seq.is_some() && after_seq.is_some() {
                return Err(req_err(RequestError::new(422, "bad_seq", "before_seq 与 after_seq 不能同时给")));
            }
            let (messages, has_more, next_before) =
                rt_h.db.list_messages(&sid, limit, before_seq, after_seq).map_err(db_err)?;
            let _ = &mut before_seq;
            let _ = &mut after_seq;
            let (_, me) = speaker(&v, agent_hdr, &rt_h.default_human);
            let mut pending: Vec<Value> = Vec::new();
            if let Some(me) = &me {
                let spoke_through = messages.iter()
                    .filter(|m| m["author"]["id"].as_str() == Some(me.as_str()))
                    .filter_map(|m| m["seq"].as_i64())
                    .max().unwrap_or(0);
                for m in &messages {
                    let needs: Vec<String> = m["meta"]["needs_response"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
                    if needs.contains(me) && m["seq"].as_i64().unwrap_or(0) > spoke_through {
                        pending.push(json!({
                            "seq": m["seq"], "message_id": m["message_id"],
                            "from": m["author"]["id"],
                            "text": m["text"].as_str().unwrap_or("").chars().take(EXCERPT_LEN).collect::<String>(),
                        }));
                    }
                }
            }
            return Ok(Reply::json(200, json!({
                "ok": true, "session_id": sid, "me": me,
                "messages": messages, "my_pending": pending,
                "has_more": has_more, "next_before_seq": next_before,
                "server_time": server_time(),
            })));
        }

        // 待办清单
        if tail == "todos" {
            let session = rt_h.get_session(&sid, &v.role)?
                .ok_or_else(|| req_err(RequestError::new(404, "not_found", "会话不存在")))?;
            if !rt_h.can_read(&v, &session, agent_hdr, &rt_h.default_human) {
                return Err(req_err(RequestError::new(404, "not_found", "会话不存在")));
            }
            let owner = one(&req.query, "owner").unwrap_or_default();
            let owner = owner.trim().to_string();
            let status = one(&req.query, "status").unwrap_or_default();
            let status = status.trim().to_string();
            if !status.is_empty() && !TODO_STATUS.contains(&status.as_str()) {
                return Err(req_err(RequestError::new(422, "bad_status",
                &format!("status 只能是 {}", TODO_STATUS.join("、")))));
            }
            let sort = one(&req.query, "sort").unwrap_or_default();
            let sort = if sort.is_empty() { "time".to_string() } else { sort };
            if !TODO_SORTS.contains(&sort.as_str()) {
                return Err(req_err(RequestError::new(422, "bad_sort",
                &format!("sort 只能是 {}", TODO_SORTS.join("、")))));
            }
            let owner_f = if owner.is_empty() { None } else { Some(owner.as_str()) };
            let status_f = if status.is_empty() { None } else { Some(status.as_str()) };
            let items: Vec<Value> = rt_h.db.todos_list(&sid, owner_f, status_f).map_err(db_err)?
                .iter().map(|i| rt_h.todo_out(i)).collect();
            let members: Vec<String> = session["member_ids"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).filter(|s| !s.is_empty()).collect()).unwrap_or_default();
            let mut groups: Vec<Value> = Vec::new();
            for mid in &members {
                let own: Vec<&Value> = items.iter().filter(|i| i["owner"].as_str() == Some(mid.as_str())).collect();
                groups.push(json!({"owner": mid, "display": display_of(mid),
                    "count": own.len(),
                    "items": own.into_iter().cloned().collect::<Vec<_>>()}));
            }
            if !owner.is_empty() {
                groups.retain(|g| g["owner"].as_str() == Some(owner.as_str()));
                if groups.is_empty() {
                    groups = vec![json!({"owner": owner, "display": display_of(&owner),
                        "count": 0, "items": []})];
                }
            }
            if sort == "owner" {
                groups.sort_by(|a, b| a["owner"].as_str().unwrap_or("").cmp(b["owner"].as_str().unwrap_or("")));
            } else {
                let key = |g: &Value| g["items"].as_array().and_then(|a| a.last())
                    .and_then(|i| i["created_at"].as_str()).unwrap_or("").to_string();
                let filled: Vec<Value> = groups.iter().filter(|g| !g["items"].as_array().map(|a| a.is_empty()).unwrap_or(true)).cloned().collect();
                let empty: Vec<Value> = groups.iter().filter(|g| g["items"].as_array().map(|a| a.is_empty()).unwrap_or(true)).cloned().collect();
                let mut filled = filled;
                filled.sort_by_key(|g| std::cmp::Reverse(key(g)));
                groups = filled;
                groups.extend(empty);
            }
            return Ok(Reply::json(200, json!({
                "ok": true, "session_id": sid,
                "state": session["state"].as_str().unwrap_or("open"),
                "owner": owner, "status": status, "sort": sort,
                "count": items.len(), "groups": groups, "items": items,
                "server_time": server_time(),
            })));
        }

        // 意图分类台账
        if tail == "todos/intents" {
            let session = rt_h.get_session(&sid, &v.role)?
                .ok_or_else(|| req_err(RequestError::new(404, "not_found", "会话不存在")))?;
            if !rt_h.can_read(&v, &session, agent_hdr, &rt_h.default_human) {
                return Err(req_err(RequestError::new(404, "not_found", "会话不存在")));
            }
            let limit = clamp_i64(one(&req.query, "limit").and_then(|s| s.parse().ok()), 1, 1000, 200);
            let skips = rt_h.db.todos_skips_list(&sid, limit).map_err(db_err)?;
            let items = rt_h.db.todos_list(&sid, None, None).map_err(db_err)?;
            let followup_total: i64 = items.iter().map(|i| i["followup_count"].as_i64().unwrap_or(0)).sum();
            return Ok(Reply::json(200, json!({
                "ok": true, "session_id": sid, "count": skips.len(),
                "skips": skips, "todos_count": items.len(),
                "followup_total": followup_total, "server_time": server_time(),
            })));
        }

        // 成员清单
        if tail == "members" {
            let session = rt_h.get_session(&sid, &v.role)?
                .ok_or_else(|| req_err(RequestError::new(404, "not_found", "会话不存在")))?;
            if !rt_h.can_read(&v, &session, agent_hdr, &rt_h.default_human) {
                return Err(req_err(RequestError::new(404, "not_found", "会话不存在")));
            }
            let prefix = one(&req.query, "prefix").unwrap_or_default();
            let limit = clamp_i64(one(&req.query, "limit").and_then(|s| s.parse().ok()),
                1, PAGE_LIMIT_MAX, PAGE_LIMIT_DEFAULT);
            let mut items: Vec<Value> = Vec::new();
            for mid in session["member_ids"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect::<Vec<String>>()).unwrap_or_default() {
                let known = rt_h.roster.get(&mid).is_some();
                let entry = json!({"name": mid, "agent_id": if known { mid.clone() } else { String::new() },
                    "kind": if known { "agent" } else { "human" }});
                if !prefix.is_empty()
                    && !entry["name"].as_str().unwrap_or("").starts_with(&prefix)
                    && !entry["agent_id"].as_str().unwrap_or("").starts_with(&prefix)
                {
                    continue;
                }
                items.push(entry);
            }
            let truncated = items.len() as i64 > limit;
            if truncated {
                items.truncate(limit as usize);
            }
            return Ok(Reply::json(200, json!({"ok": true, "items": items,
                "truncated": truncated, "server_time": server_time()})));
        }

        // memo 读取
        if tail == "memo" {
            let session = rt_h.get_session(&sid, &v.role)?
                .ok_or_else(|| req_err(RequestError::new(404, "not_found", "会话不存在")))?;
            if !rt_h.can_read(&v, &session, agent_hdr, &rt_h.default_human) {
                return Err(req_err(RequestError::new(404, "not_found", "会话不存在")));
            }
            let state = rt_h.db.memo_get(&sid).map_err(db_err)?;
            return Ok(Reply::json(200, json!({
                "ok": true, "session_id": sid,
                "memo": state["memo"], "version": state["version"],
                "etag": state["etag"], "updated_at": state["updated_at"],
                "updated_by": state["updated_by"], "server_time": server_time(),
            })));
        }

        Err(not_impl(&req.method, &req.path))
    });

    // ---- PATCH /api/todos/{id} ------------------------------------------
    let rt_h = rt.clone();
    gw.route("/api/todos", &["PATCH"], true, move |req| {
        let v = Rt::require_visitor(req)?;
        let body = Rt::json_body(req)?;
        let agent_hdr = req.header("X-Agent-Id");
        let Some(tid) = split_todo_path(&req.path) else {
            return Err(not_impl(&req.method, &req.path));
        };
        let item = rt_h.db.todo_get(&tid).map_err(db_err)?
            .ok_or_else(|| req_err(RequestError::new(404, "not_found", "待办不存在")))?;
        let session = rt_h.get_session(item["session_id"].as_str().unwrap_or(""), &v.role)?
            .ok_or_else(|| req_err(RequestError::new(404, "not_found", "待办不存在")))?;
        if !rt_h.can_read(&v, &session, agent_hdr, &rt_h.default_human) {
            return Err(req_err(RequestError::new(404, "not_found", "待办不存在")));
        }
        let actor = rt_h.memo_actor(&v, &session, agent_hdr, &rt_h.default_human)?;
        rt_h.todo_write_guard(&session)?;
        if actor != item["owner"].as_str().unwrap_or("") {
            return Err(req_err(RequestError::new(403, "not_your_field",
                &format!("只能改 owner=自己的条目（本条 owner={}）", item["owner"].as_str().unwrap_or("")))));
        }
        for k in body.as_object().map(|o| o.keys().cloned().collect::<Vec<_>>()).unwrap_or_default() {
            if !["status", "evidence", "evidences", "landing"].contains(&k.as_str()) {
                return Err(req_err(RequestError::new(422, "bad_todo",
                &format!("未知字段：{k}"))));
            }
        }
        let raw = body.get("landing").filter(|x| !x.is_null())
            .or_else(|| body.get("evidences").filter(|x| !x.is_null()))
            .or_else(|| body.get("evidence").filter(|x| !x.is_null()));
        let (landing, evidences) = normalize_landing(raw).map_err(req_err)?;
        if let Some(st) = body.get("status") {
            let status = st.as_str().unwrap_or("").trim().to_string();
            if !TODO_STATUS.contains(&status.as_str()) {
                return Err(req_err(RequestError::new(422, "bad_status",
                &format!("status 只能是 {}", TODO_STATUS.join("、")))));
            }
            if status == "open" {
                let updated = rt_h.db.todo_reopen(&tid).map_err(db_err)?
                    .ok_or_else(|| req_err(RequestError::new(404, "not_found", "待办不存在")))?;
                return Ok(Reply::json(200, json!({"ok": true,
                    "session_id": item["session_id"], "item": rt_h.todo_out(&updated),
                    "validated": false, "server_time": server_time()})));
            }
            if status == "doing" {
                let updated = rt_h.db.todo_mark_doing(&tid).map_err(db_err)?
                    .ok_or_else(|| req_err(RequestError::new(404, "not_found", "待办不存在")))?;
                return Ok(Reply::json(200, json!({"ok": true,
                    "session_id": item["session_id"], "item": rt_h.todo_out(&updated),
                    "validated": false, "server_time": server_time()})));
            }
            // status=done：落点必须已在本次提交里，或此前已经落过
            let have_landing = !landing.is_empty() || !evidences.is_empty()
                || item["landing"].as_array().map(|a| !a.is_empty()).unwrap_or(false)
                || item["evidences"].as_array().map(|a| !a.is_empty()).unwrap_or(false);
            if !have_landing {
                return Err(req_err(RequestError::new(422, "no_landing",
                    "不能自盖完成章：status=done 需先交落点（landing）——commit 哈希 / 字面 curl+HTTP 码 / 文件路径+时间戳 / 重启 PID+时间戳；落点为空，条目继续挂 open")));
            }
            let now = iso_bj(now_epoch());
            let stamped: Vec<Value> = evidences.iter().map(|e| {
                let mut c = e.clone();
                c["by"] = json!(actor);
                c["validated_at"] = json!(now);
                c
            }).collect();
            let updated = rt_h.db.todo_mark_done(&tid, &actor, &stamped, &landing).map_err(db_err)?
                .ok_or_else(|| req_err(RequestError::new(404, "not_found", "待办不存在")))?;
            return Ok(Reply::json(200, json!({"ok": true,
                "session_id": item["session_id"], "item": rt_h.todo_out(&updated),
                "validated": true, "landing_count": landing.len(), "server_time": now})));
        }
        if landing.is_empty() && evidences.is_empty() {
            return Err(req_err(RequestError::new(422, "bad_todo",
                "PATCH 需带 status（撤回）或 landing/evidence(s)（落点）")));
        }
        let now = iso_bj(now_epoch());
        let stamped: Vec<Value> = evidences.iter().map(|e| {
            let mut c = e.clone();
            c["by"] = json!(actor);
            c["validated_at"] = json!(now);
            c
        }).collect();
        let updated = rt_h.db.todo_mark_done(&tid, &actor, &stamped, &landing).map_err(db_err)?
            .ok_or_else(|| req_err(RequestError::new(404, "not_found", "待办不存在")))?;
        Ok(Reply::json(200, json!({"ok": true,
            "session_id": item["session_id"], "item": rt_h.todo_out(&updated),
            "validated": true, "landing_count": landing.len(), "server_time": now})))
    });

    // ---- 兜底：未知路径 ---------------------------------------------------
    gw.not_found(move |req| {
        if req.path.starts_with("/api/") {
            return Err(not_impl(&req.method, &req.path));
        }
        // 非 /api：静态/SPA 兜底，Rust 版未带 ui/ → 404（PARITY 登记）
        Ok(Reply::json(404, json!({"ok": false, "error": {
            "code": "not_found", "message": "静态资源不存在"}})))
    });

    eprintln!("chat_svc starting host={host} port={port}");
    if let Err(e) = gw.serve(&host, port) {
        eprintln!("chat_svc 启动失败: {e}");
        std::process::exit(1);
    }
}

// ==================================================================== 辅助

/// memo 文档收敛（与 Python `_memo_doc` 同口径：5 字段、blocked_by 数组、大小上限）。
fn memo_doc(body: &Value) -> Result<Value, HttpError> {
    let mut doc = serde_json::Map::new();
    for key in MEMO_STR_KEYS {
        let v = body.get(key).cloned().unwrap_or(Value::Null);
        let s = v.as_str().unwrap_or("").trim().chars().take(4000).collect::<String>();
        doc.insert(key.to_string(), json!(s));
    }
    for key in MEMO_LIST_KEYS {
        match body.get(key) {
            None => { doc.insert(key.to_string(), json!([])); }
            Some(Value::Array(a)) => { doc.insert(key.to_string(), json!(a)); }
            Some(_) => {
                return Err(req_err(RequestError::new(422, "bad_memo",
                &format!("{key} 必须是数组"))));
            }
        }
    }
    let mut unknown: Vec<String> = body.as_object().map(|o| {
        o.keys().filter(|k| !MEMO_STR_KEYS.contains(&k.as_str()) && !MEMO_LIST_KEYS.contains(&k.as_str())).cloned().collect()
    }).unwrap_or_default();
    unknown.sort();
    if !unknown.is_empty() {
        return Err(req_err(RequestError::new(422, "bad_memo",
                &format!("未知字段：{}", unknown.join("、")))));
    }
    if let Some(tasks) = doc.get_mut("open_tasks").and_then(Value::as_array_mut).cloned() {
        for (idx, item) in tasks.iter().enumerate() {
            if !item.is_object() {
                return Err(req_err(RequestError::new(422, "bad_memo",
                &format!("open_tasks[{idx}] 必须是对象"))));
            }
            let blocked = item.get("blocked_by");
            let fixed: Vec<String> = match blocked {
                None => vec![],
                Some(Value::Array(a)) => a.iter().map(|x| x.as_str().unwrap_or("").trim().to_string()).filter(|s| !s.is_empty()).collect(),
                Some(_) => return Err(req_err(RequestError::new(422, "bad_memo",
                &format!("open_tasks[{idx}].blocked_by 必须是数组")))),
            };
            // fixed 写回
            if let Some(arr) = doc.get_mut("open_tasks").and_then(Value::as_array_mut) {
                arr[idx]["blocked_by"] = json!(fixed);
            }
        }
    }
    let raw = serde_json::to_vec(&Value::Object(doc.clone())).unwrap_or_default();
    if raw.len() > MEMO_MAX_BYTES {
        return Err(req_err(RequestError::new(413, "memo_too_large",
                &format!("记忆超过 {} KB，请只留要点、明细留原消息", MEMO_MAX_BYTES / 1024))));
    }
    Ok(Value::Object(doc))
}

/// memo PATCH 只追加（数组追加、标量覆盖）。
fn memo_merge(current: Value, patch: &Value) -> Result<Value, HttpError> {
    if !patch.is_object() || patch.as_object().map(|o| o.is_empty()).unwrap_or(true) {
        return Err(req_err(RequestError::new(422, "empty_patch", "PATCH 至少要带一个要改的字段")));
    }
    let mut doc = serde_json::Map::new();
    for key in MEMO_STR_KEYS {
        doc.insert(key.to_string(), json!(current.get(key).and_then(Value::as_str).unwrap_or("")));
    }
    for key in MEMO_LIST_KEYS {
        doc.insert(key.to_string(), json!(current.get(key).and_then(Value::as_array).cloned().unwrap_or_default()));
    }
    for (key, value) in patch.as_object().unwrap() {
        if MEMO_LIST_KEYS.contains(&key.as_str()) {
            if !value.is_array() {
                return Err(req_err(RequestError::new(422, "bad_memo",
                &format!("{key} 必须是数组"))));
            }
            let mut merged = doc.get(key).and_then(Value::as_array).cloned().unwrap_or_default();
            merged.extend(value.as_array().cloned().unwrap_or_default());
            doc.insert(key.clone(), json!(merged));
        } else if MEMO_STR_KEYS.contains(&key.as_str()) {
            let s = value.as_str().unwrap_or("").trim().chars().take(4000).collect::<String>();
            doc.insert(key.clone(), json!(s));
        } else {
            return Err(req_err(RequestError::new(422, "bad_memo",
                &format!("PATCH 只追加 {} / 覆盖 {}，收到：{}", MEMO_LIST_KEYS.join("、"), MEMO_STR_KEYS.join("、"), key))));
        }
    }
    memo_doc(&Value::Object(doc))
}

/// #448：agent 回复里的队列声明（只校验不落库）。
fn todo_declare_plan(rt: &Rt, sid: &str, actor: &str, decl: &Value) -> Result<Value, HttpError> {
    if !decl.is_object() {
        return Err(req_err(RequestError::new(422, "bad_todo", "todo 需是对象 {id, status, landing}")));
    }
    for k in decl.as_object().map(|o| o.keys().cloned().collect::<Vec<_>>()).unwrap_or_default() {
        if !["id", "status", "landing"].contains(&k.as_str()) {
            return Err(req_err(RequestError::new(422, "bad_todo",
                &format!("todo 未知字段：{k}"))));
        }
    }
    let todo_id = decl.get("id").and_then(Value::as_str).unwrap_or("").trim().to_string();
    let status = decl.get("status").and_then(Value::as_str).unwrap_or("").trim().to_string();
    if todo_id.is_empty() {
        return Err(req_err(RequestError::new(422, "bad_todo", "todo 需给 id")));
    }
    if !TODO_STATUS.contains(&status.as_str()) {
        return Err(req_err(RequestError::new(422, "bad_status",
                &format!("status 只能是 {}", TODO_STATUS.join("、")))));
    }
    let item = rt.db.todo_get(&todo_id).map_err(db_err)?
        .ok_or_else(|| req_err(RequestError::new(404, "not_found", "待办不存在")))?;
    if item["session_id"].as_str() != Some(sid) {
        return Err(req_err(RequestError::new(404, "not_found", "待办不存在")));
    }
    if item["owner"].as_str() != Some(actor) {
        return Err(req_err(RequestError::new(403, "not_your_field",
                &format!("只能改 owner=自己的条目（本条 owner={}）", item["owner"].as_str().unwrap_or("")))));
    }
    let (landing, evidences) = normalize_landing(decl.get("landing")).map_err(req_err)?;
    if status == "done" {
        let have = !landing.is_empty() || !evidences.is_empty()
            || item["landing"].as_array().map(|a| !a.is_empty()).unwrap_or(false)
            || item["evidences"].as_array().map(|a| !a.is_empty()).unwrap_or(false);
        if !have {
            return Err(req_err(RequestError::new(422, "no_landing",
                "不能自盖完成章：status=done 需先交落点（landing），落点为空，条目继续挂原状态")));
        }
    }
    Ok(json!({"id": todo_id, "status": status, "actor": actor,
        "landing": landing, "evidences": evidences}))
}

/// 按已校验的声明落库。
fn todo_declare_apply(rt: &Rt, plan: &Value) -> Result<Value, HttpError> {
    let todo_id = plan["id"].as_str().unwrap_or("");
    let status = plan["status"].as_str().unwrap_or("");
    let actor = plan["actor"].as_str().unwrap_or("");
    if status == "open" {
        return rt.db.todo_reopen(todo_id).map_err(db_err)?
            .ok_or_else(|| req_err(RequestError::new(404, "not_found", "待办不存在")));
    }
    if status == "doing" {
        return rt.db.todo_mark_doing(todo_id).map_err(db_err)?
            .ok_or_else(|| req_err(RequestError::new(404, "not_found", "待办不存在")));
    }
    let now = iso_bj(now_epoch());
    let stamped: Vec<Value> = plan["evidences"].as_array().map(|a| {
        a.iter().map(|e| {
            let mut c = e.clone();
            c["by"] = json!(actor);
            c["validated_at"] = json!(now);
            c
        }).collect()
    }).unwrap_or_default();
    let landing: Vec<String> = plan["landing"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect::<Vec<String>>()).unwrap_or_default();
    rt.db.todo_mark_done(todo_id, actor, &stamped, &landing).map_err(db_err)?
        .ok_or_else(|| req_err(RequestError::new(404, "not_found", "待办不存在")))
}

/// 人 @agent 派活 → 自动入队（意图闸门 + 幂等 + 回执翻状态）。任何异常只告警不阻断。
fn todo_autotrack(rt: &Rt, _session: &Value, sid: &str, author_id: &str,
                  mentions: &[String], message: &Value) -> Option<Value> {
    let result: Result<Option<Value>, String> = (|| {
        let agents = rt.roster.agent_ids();
        if agents.is_empty() || agents.iter().any(|a| a == author_id) {
            return Ok(None); // agent 之间互相 @：不入队
        }
        let targets: Vec<String> = mentions.iter().filter(|m| agents.contains(m)).cloned().collect();
        if targets.is_empty() {
            return Ok(None);
        }
        let owner = &targets[0];
        let raw = message["text"].as_str().unwrap_or("");
        let verdict = classify_human_intent(raw);
        if verdict["intent"].as_str() != Some("task") {
            // 记跳过台账 + 给已有条目累加追问次数（幂等）
            let owner_todos = rt.db.todos_list(sid, Some(owner), None).map_err(|e| e.0)?;
            let dkey = todo_message_key(raw);
            if !dkey.is_empty() {
                for row in &owner_todos {
                    if todo_dedupe_key(row["text"].as_str().unwrap_or("")) == dkey {
                        return Ok(None); // 队列里已有同正文条目：不记台账
                    }
                }
            }
            let seq = message["seq"].as_i64().map(|s| s as i32);
            let key = rt.db.todo_skip_create(sid, seq,
                message["message_id"].as_str().unwrap_or(""), author_id, owner,
                verdict["intent"].as_str().unwrap_or("followup"),
                verdict["reason"].as_str().unwrap_or(""),
                &verdict["hits"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect::<Vec<String>>()).unwrap_or_default(),
                raw).map_err(|e| e.0)?;
            if key.is_some() {
                // 只有本次真新增了台账行才 +1
                if let Some(target) = todo_followup_target(rt, sid, owner) {
                    let _ = rt.db.todo_bump_followup(target["id"].as_str().unwrap_or(""));
                }
            }
            return Ok(None);
        }
        let seq = message["seq"].as_i64();
        let text = todo_text_from_message(raw, seq, TODO_TEXT_MAX);
        if text.is_empty() {
            return Ok(None);
        }
        let name = todo_task_name(&text);
        if name.is_empty() {
            return Ok(None);
        }
        let key = todo_dedupe_key(&text);
        for row in rt.db.todos_list(sid, Some(owner), None).map_err(|e| e.0)? {
            if row["status"].as_str() == Some("done") {
                continue;
            }
            if todo_dedupe_key(row["text"].as_str().unwrap_or("")) == key {
                return Ok(None); // 幂等：已在队列里且未完成
            }
        }
        let item = rt.db.todo_create(sid, &text, owner, "confirm", None,
            author_id, &[], "task", &name, "").map_err(|e| e.0)?;
        Ok(item)
    })();
    match result {
        Ok(item) => item,
        Err(e) => {
            eprintln!("chat_svc 队列自动入队失败（不影响发言）：{e}");
            None
        }
    }
}

/// 追问该记到哪个已有条目上：同 owner 最近一条未完成；都没有就最近一条。
fn todo_followup_target(rt: &Rt, sid: &str, owner: &str) -> Option<Value> {
    let rows = rt.db.todos_list(sid, Some(owner), None).ok()?;
    if rows.is_empty() {
        return None;
    }
    for status in ["open", "doing"] {
        let cand: Vec<&Value> = rows.iter().filter(|r| r["status"].as_str() == Some(status)).collect();
        if let Some(last) = cand.last() {
            return Some((*last).clone());
        }
    }
    rows.into_iter().last()
}

/// agent 回群报进度 → 翻自己的在办条目（否定词优先；引用优先；单条兜底）。
fn todo_reply_autotrack(rt: &Rt, sid: &str, author_id: &str, text: &str,
                        message: &Value, decl: Option<&Value>) -> Vec<Value> {
    let result: Result<Vec<Value>, String> = (|| {
        let agents = rt.roster.agent_ids();
        if agents.is_empty() || !agents.iter().any(|a| a == author_id) {
            return Ok(vec![]);
        }
        let low = text.to_lowercase();
        let done = reply_is_done(&low);
        let doing = !done && reply_is_doing(&low);
        if !done && !doing {
            return Ok(vec![]);
        }
        let pending: Vec<Value> = rt.db.todos_list(sid, Some(author_id), None).map_err(|e| e.0)?
            .into_iter().filter(|r| matches!(r["status"].as_str(), Some("open") | Some("doing"))).collect();
        if pending.is_empty() {
            return Ok(vec![]);
        }
        let ids: Vec<String> = todo_ref_ids(text);
        let seqs: Vec<String> = todo_ref_seqs(text);
        let mut hits: Vec<Value> = Vec::new();
        for row in &pending {
            if ids.iter().any(|i| i == row["id"].as_str().unwrap_or("")) {
                hits.push(row.clone());
                continue;
            }
            // 正文写了 #<seq> 就翻那几条
            let row_text = row["text"].as_str().unwrap_or("");
            if let Some(rest) = row_text.strip_prefix('#') {
                let num: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
                if !num.is_empty() && seqs.iter().any(|s| s == &num) {
                    hits.push(row.clone());
                }
            }
        }
        if hits.is_empty() && pending.len() == 1 && seqs.is_empty() && ids.is_empty() {
            hits = pending.clone();
        }
        if let Some(d) = decl {
            hits.retain(|r| r["id"].as_str() != Some(d["id"].as_str().unwrap_or("")));
        }
        if hits.is_empty() {
            return Ok(vec![]);
        }
        let now = iso_bj(now_epoch());
        // 落点：正文抠得到机器可验落点就用它，抠不到用这条群消息本身
        let mut landing: Vec<String> = vec![];
        let mut evidences: Vec<Value> = vec![];
        for cand in todo_landing_candidates(text) {
            if let Ok((l, e)) = normalize_landing(Some(&json!(vec![cand]))) {
                if !l.is_empty() || !e.is_empty() {
                    landing = l;
                    evidences = e;
                    break;
                }
            }
        }
        if landing.is_empty() && evidences.is_empty() {
            let seq = message["seq"].as_i64().unwrap_or(0);
            let value = format!("msg: #{seq} {author_id} 回群（{now}）");
            landing = vec![value.clone()];
            evidences = vec![json!({"type": TODO_MSG_EVIDENCE_TYPE, "value": value,
                "by": author_id, "validated_at": now})];
        }
        let mut out = Vec::new();
        for row in &hits {
            let tid = row["id"].as_str().unwrap_or("");
            let item = if done {
                rt.db.todo_mark_done(tid, author_id, &evidences, &landing).map_err(|e| e.0)?
            } else if row["status"].as_str() == Some("open") {
                rt.db.todo_mark_doing(tid).map_err(|e| e.0)?
            } else {
                None
            };
            if let Some(item) = item {
                out.push(item);
            }
        }
        Ok(out)
    })();
    match result {
        Ok(v) => v,
        Err(e) => {
            eprintln!("chat_svc 队列回执翻状态失败（不影响发言）：{e}");
            vec![]
        }
    }
}

/// 正文里抠机器可验落点候选（commit / curl / file / restart；见 Python `_todo_reply_landing`）。
fn todo_landing_candidates(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for token in text.split_whitespace() {
        if is_commit(token) || has_http_code(token) {
            out.push(token.to_string());
        }
    }
    out.truncate(8);
    out
}

use hacs_l3_domain::chat_svc::landing::{has_http_code, is_commit};

