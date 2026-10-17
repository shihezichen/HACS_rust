//! `roundtable` 组合根（对应 Python roundtable.py 的 PgStore/BlackboardClient/
//! NatsTransport/A2ATransport + main CLI）。
//!
//! 用法与 Python 对齐：
//!   roundtable --goal ... --agenda "标题|产出; ..." --members a,b,c \
//!     [--commit] [--dry-run] [--resume <sid>] [--transport nats|a2a] \
//!     [--nats-host H --nats-port P --nats-user U --nats-password-file F] \
//!     [--dsn-file F] [--blackboard URL] [--token-file F] [--prompt-log F]

#![allow(clippy::too_many_arguments)]

use hacs_l3_domain::roundtable_core::*;
use hacs_l3_domain::roundtable_sched::{
    dedup_names, unwrap_reply, Blackboard, NullBlackboard, NullStore, Roundtable,
    RoundtableConfig, RunCtx, Store, Transport, DEFAULT_TOPIC, ERR_SIGNAL,
};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::time::Duration;

// ---------------- PgStore ----------------

struct PgStore {
    conn: std::cell::RefCell<postgres::Client>,
}

fn mk_client(dsn: &str) -> Result<postgres::Client, String> {
    postgres::Client::connect(dsn, postgres::NoTls)
        .map_err(|e| format!("数据库操作失败：{}", e))
}

impl PgStore {
    fn new(dsn: &str) -> Result<Self, String> {
        if dsn.is_empty() {
            return Err("DSN 为空：检查 --dsn-file 指向的文件里有没有 BB_PG_DSN=".into());
        }
        Ok(Self { conn: std::cell::RefCell::new(mk_client(dsn)?) })
    }

    fn query_rows(&mut self, sql: &str, params: &[&(dyn postgres::types::ToSql + Sync)]) -> Result<Vec<postgres::Row>, String> {
        self.conn.borrow_mut().query(sql, params).map_err(|e| format!("数据库操作失败：{e}"))
    }
}

impl Store for PgStore {
    fn create_session(&mut self, session: &Value) {
        let members = serde_json::to_string(&session["members"]).unwrap_or_default();
        let agenda = serde_json::to_string(&session["agenda"]).unwrap_or_default();
        let res = self.conn.borrow_mut().execute(
            "INSERT INTO roundtable_sessions \
             (session_id, topic, question, moderator, members, max_rounds, round_no, state, goal, agenda) \
             VALUES ($1,$2,$3,$4,$5::text::jsonb,$6,$7,$8,$9,$10::text::jsonb)",
            &[
                &session["session_id"].as_str().unwrap_or(""),
                &session["topic"].as_str().unwrap_or(""),
                &session["question"].as_str().unwrap_or(""),
                &session["moderator"].as_str().unwrap_or(""),
                &members,
                &(session["max_rounds"].as_i64().unwrap_or(0) as i32),
                &(session["round_no"].as_i64().unwrap_or(0) as i32),
                &session["state"].as_str().unwrap_or("running"),
                &session["goal"].as_str().unwrap_or(""),
                &agenda,
            ],
        );
        if let Err(e) = res {
            eprintln!("[存储] create_session 失败：{e}");
        }
    }

    fn update_session(&mut self, session_id: &str, fields: &Map<String, Value>) {
        // 列白名单次序（=MUTABLE_SESSION_COLUMNS）动态拼 SET；jsonb 列单独 cast。
        const JSONB: [&str; 1] = ["confirm_summary"];
        let mut sets: Vec<String> = Vec::new();
        let mut nulls: Vec<bool> = Vec::new();
        let mut params: Vec<Box<dyn postgres::types::ToSql + Sync>> = Vec::new();
        let mut n = 0usize;
        for col in MUTABLE_SESSION_COLUMNS {
            let Some(v) = fields.get(col) else { continue };
            n += 1;
            let null = v.is_null();
            if JSONB.contains(&col) {
                sets.push(format!("{col} = ${n}::text::jsonb"));
                let text = if null { "null".to_string() } else { serde_json::to_string(v).unwrap_or_default() };
                params.push(Box::new(text));
            } else if col == "attendance_rate" {
                sets.push(format!("{col} = ${n}"));
                params.push(Box::new(v.as_f64().map(|f| f as f32)));
            } else if col == "ended_at" || col == "confirmed_at" {
                // timestamptz：文本双跳；空值 NULL
                sets.push(format!("{col} = NULLIF(${n}::text,'')::timestamptz"));
                params.push(Box::new(v.as_str().map(String::from).unwrap_or_default()));
            } else {
                if ["round_no", "expected_count", "present_count", "absent_count"].contains(&col) {
                    sets.push(format!("{col} = ${n}"));
                    params.push(Box::new(v.as_i64().unwrap_or(0) as i32));
                } else {
                    sets.push(format!("{col} = ${n}"));
                    params.push(Box::new(v.as_i64().unwrap_or(0)));
                }
                // 占位修正：字符串列统一走 text
                if v.as_i64().is_none() {
                    let len = params.len();
                    params[len - 1] = Box::new(v.as_str().map(String::from).unwrap_or_default());
                    sets[n - 1] = format!("{col} = ${n}::text");
                }
            }
            nulls.push(null);
        }
        let _ = &nulls;
        if sets.is_empty() {
            return;
        }
        n += 1;
        params.push(Box::new(session_id.to_string()));
        let sql = format!(
            "UPDATE roundtable_sessions SET {} WHERE session_id = ${n}",
            sets.join(", ")
        );
        let refs: Vec<&(dyn postgres::types::ToSql + Sync)> =
            params.iter().map(|b| b.as_ref() as &(dyn postgres::types::ToSql + Sync)).collect();
        if let Err(e) = self.conn.borrow_mut().execute(&sql, &refs) {
            eprintln!("[存储] update_session 失败：{e}");
        }
    }

    fn add_agenda_items(&mut self, session_id: &str, items: &[Value]) {
        for item in items {
            let r = self.conn.borrow_mut().execute(
                "INSERT INTO roundtable_agenda_items (session_id, seq, title, expected, status) \
                 VALUES ($1,$2,$3,$4,$5) ON CONFLICT (session_id, seq) DO NOTHING",
                &[
                    &session_id,
                    &(item["seq"].as_i64().unwrap_or(0) as i32),
                    &item["title"].as_str().unwrap_or(""),
                    &item["expected"].as_str().unwrap_or(""),
                    &item.get("status").and_then(|v| v.as_str()).unwrap_or("pending"),
                ],
            );
            if let Err(e) = r {
                eprintln!("[存储] add_agenda_items 失败：{e}");
            }
        }
    }

    fn update_agenda_item(&mut self, session_id: &str, seq: i64, fields: &Map<String, Value>) {
        let mut sets: Vec<String> = Vec::new();
        let mut params: Vec<Box<dyn postgres::types::ToSql + Sync>> = Vec::new();
        let mut n = 0usize;
        for col in MUTABLE_AGENDA_COLUMNS {
            let Some(v) = fields.get(col) else { continue };
            n += 1;
            if col == "conclusion_entry" {
                sets.push(format!("{col} = NULLIF(${n}::text,'')::bigint"));
                params.push(Box::new(v.as_i64().map(|x| x.to_string()).unwrap_or_default()));
            } else {
                sets.push(format!("{col} = ${n}::text"));
                params.push(Box::new(v.as_str().map(String::from).unwrap_or_default()));
            }
        }
        if sets.is_empty() {
            return;
        }
        n += 1;
        params.push(Box::new(session_id.to_string()));
        params.push(Box::new(seq as i32));
        let sql = format!(
            "UPDATE roundtable_agenda_items SET {} WHERE session_id = ${} AND seq = ${}",
            sets.join(", "),
            n,
            n + 1
        );
        let refs: Vec<&(dyn postgres::types::ToSql + Sync)> =
            params.iter().map(|b| b.as_ref() as &(dyn postgres::types::ToSql + Sync)).collect();
        if let Err(e) = self.conn.borrow_mut().execute(&sql, &refs) {
            eprintln!("[存储] update_agenda_item 失败：{e}");
        }
    }

    fn add_confirmation(&mut self, row: &Value) -> Option<i64> {
        self.conn
            .borrow_mut()
            .query_one(
                "INSERT INTO roundtable_confirmations (session_id, target, member, verdict, note) \
                 VALUES ($1,$2,$3,$4,NULLIF($5::text,'')) RETURNING confirm_id",
                &[
                    &row["session_id"].as_str().unwrap_or(""),
                    &row["target"].as_str().unwrap_or(""),
                    &row["member"].as_str().unwrap_or(""),
                    &row["verdict"].as_str().unwrap_or(""),
                    &row["note"].as_str().unwrap_or(""),
                ],
            )
            .ok()
            .and_then(|r| r.try_get::<_, i64>(0).ok())
    }

    fn add_action(&mut self, row: &Value) -> Option<i64> {
        self.conn
            .borrow_mut()
            .query_one(
                "INSERT INTO roundtable_actions \
                 (session_id, agenda_seq, what, owner, due_at, status, note) \
                 VALUES ($1,NULLIF($2::text,'')::bigint,$3,$4,NULLIF($5::text,'')::timestamptz,$6,NULLIF($7::text,'')) \
                 RETURNING action_id",
                &[
                    &row["session_id"].as_str().unwrap_or(""),
                    &row["agenda_seq"].as_i64().map(|x| x.to_string()).unwrap_or_default(),
                    &row["what"].as_str().unwrap_or(""),
                    &row["owner"].as_str().unwrap_or(""),
                    // due_at 落库用 ISO UTC
                    &row["due_at"].as_i64().map(iso_utc).unwrap_or_default(),
                    &row["status"].as_str().unwrap_or("open"),
                    &row["note"].as_str().unwrap_or(""),
                ],
            )
            .ok()
            .and_then(|r| r.try_get::<_, i64>(0).ok())
    }

    fn add_issue(&mut self, row: &Value) -> Option<i64> {
        let refs = serde_json::to_string(&row["refs"]).unwrap_or_else(|_| "[]".into());
        let evidence = serde_json::to_string(&row["evidence"]).unwrap_or_else(|_| "[]".into());
        let r = self.conn
            .borrow_mut()
            .query_one(
                "INSERT INTO roundtable_issues \
                 (session_id, title, detail, owner, status, due_at, next_check_at, refs, note, kind, evidence) \
                 VALUES ($1,$2,NULLIF($3::text,''),NULLIF($4::text,''),$5,NULLIF($6::text,'')::timestamptz,NULL,$7::text::jsonb,NULL,$8,$9::text::jsonb) \
                 RETURNING issue_id",
                &[
                    &row["session_id"].as_str().unwrap_or(""),
                    &row["title"].as_str().unwrap_or(""),
                    &row["detail"].as_str().unwrap_or(""),
                    &row["owner"].as_str().unwrap_or(""),
                    &row["status"].as_str().unwrap_or("open"),
                    &row["due_at"].as_i64().map(iso_utc).unwrap_or_default(),
                    &refs,
                    &row["kind"].as_str().unwrap_or("issue"),
                    &evidence,
                ],
            );
        match r {
            Ok(row) => row.try_get::<_, i64>(0).ok(),
            Err(e) => {
                eprintln!("[存储] add_issue 失败：{e}");
                None
            }
        }
    }

    fn add_turn(&mut self, turn: &Value) -> Option<i64> {
        let refs = serde_json::to_string(&turn["refs"]).unwrap_or_else(|_| "[]".into());
        self.conn
            .borrow_mut()
            .query_one(
                "INSERT INTO roundtable_turns \
                 (session_id, round_no, seq, speaker, status, kind, stance, vote, confidence, \
                  content, refs, entry_id, error, elapsed_ms, prompt_chars) \
                 VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,NULLIF($10::text,''),$11::text::jsonb,NULLIF($12::text,'')::bigint,$13,$14,$15) \
                 RETURNING turn_id",
                &[
                    &turn["session_id"].as_str().unwrap_or(""),
                    &(turn["round_no"].as_i64().unwrap_or(0) as i32),
                    &(turn["seq"].as_i64().unwrap_or(0) as i32),
                    &turn["speaker"].as_str().unwrap_or(""),
                    &turn["status"].as_str().unwrap_or(""),
                    &turn["kind"].as_str().unwrap_or(""),
                    &turn["stance"].as_str().unwrap_or(""),
                    &turn["vote"].as_str().unwrap_or(""),
                    &(turn["confidence"].as_f64().unwrap_or(0.0) as f32),
                    &turn["content"].as_str().unwrap_or(""),
                    &refs,
                    &turn["entry_id"].as_i64().map(|x| x.to_string()).unwrap_or_default(),
                    &turn["error"].as_str().unwrap_or(""),
                    &(turn["elapsed_ms"].as_i64().unwrap_or(0) as i32),
                    &(turn["prompt_chars"].as_i64().unwrap_or(0) as i32),
                ],
            )
            .ok()
            .and_then(|r| r.try_get::<_, i64>(0).ok())
    }

    fn load_session(&self, session_id: &str) -> Option<Value> {
        let rows = self
            .conn
            .borrow_mut()
            .query(
                "SELECT session_id, topic, question, moderator, members, max_rounds, round_no, \
                 state, note, goal, agenda FROM roundtable_sessions WHERE session_id = $1",
                &[&session_id],
            )
            .ok()?;
        let row = rows.into_iter().next()?;
        let members: Value = row.try_get::<_, serde_json::Value>(4).unwrap_or(json!([]));
        let agenda: Value = row.try_get::<_, serde_json::Value>(10).unwrap_or(json!([]));
        let note: Option<String> = row.try_get(8).ok();
        Some(json!({
            "session_id": row.try_get::<_, String>(0).unwrap_or_default(),
            "topic": row.try_get::<_, String>(1).unwrap_or_default(),
            "question": row.try_get::<_, String>(2).unwrap_or_default(),
            "moderator": row.try_get::<_, String>(3).unwrap_or_default(),
            "members": members,
            "max_rounds": row.try_get::<_, i64>(5).unwrap_or(0),
            "round_no": row.try_get::<_, i64>(6).unwrap_or(0),
            "state": row.try_get::<_, String>(7).unwrap_or_default(),
            "note": note,
            "goal": row.try_get::<_, String>(9).unwrap_or_default(),
            "agenda": agenda,
        }))
    }

    fn close(&mut self) {}
}

// ---------------- BlackboardClient（只走公开 HTTP 接口） ----------------

struct BlackboardClient {
    base: String,
    token: String,
}

impl BlackboardClient {
    fn new(base_url: &str, token_file: &str) -> Result<Self, String> {
        let token = if token_file.is_empty() { String::new() } else { read_secret(token_file)? };
        Ok(Self {
            base: base_url.trim_end_matches('/').to_string(),
            token,
        })
    }

    fn request(&self, method: &str, path: &str, body: Option<&Value>) -> (u16, Value) {
        let url = format!("{}{}", self.base, path);
        let mut headers: Vec<(&str, &str)> = vec![("Accept", "application/json")];
        let auth;
        if !self.token.is_empty() {
            auth = format!("Bearer {}", self.token);
            headers.push(("Authorization", auth.as_str()));
        }
        match hacs_l1_transport::http_client::request_json(
            &url,
            method,
            body,
            &headers,
            Duration::from_secs(HTTP_TIMEOUT_S as u64),
        ) {
            Ok((code, v)) => (code, v),
            Err(e) => (0, json!({"ok": false, "error": "http_error", "detail": e.0})),
        }
    }
}

impl Blackboard for BlackboardClient {
    fn post_entry(
        &mut self,
        topic: &str,
        kind: &str,
        content: &str,
        refs: &Value,
    ) -> (Option<i64>, Option<String>) {
        if refs.as_array().map(|a| a.is_empty()).unwrap_or(true) {
            return (None, Some("refs 为空，按纪律不入库".into()));
        }
        let (code, payload) = self.request(
            "POST",
            "/entries",
            Some(&json!({"topic": topic, "kind": kind, "content": content, "refs": refs})),
        );
        if code == 200 && payload["ok"].as_bool().unwrap_or(false) {
            return (payload["entry_id"].as_i64(), None);
        }
        let detail = payload["detail"]
            .as_str()
            .or_else(|| payload["error"].as_str())
            .unwrap_or("");
        (
            None,
            Some(format!("黑板返回 {}：{}", code, truncate(detail, 160))),
        )
    }

    fn list_entries(&mut self, topic: &str, limit: i64) -> Vec<Value> {
        let path = format!("/entries?topic={}&limit={}", urlencode(topic), limit);
        let (code, payload) = self.request("GET", &path, None);
        if code != 200 || !payload["ok"].as_bool().unwrap_or(false) {
            return Vec::new();
        }
        payload["entries"].as_array().cloned().unwrap_or_default()
    }
}

/// x-www-form-urlencoded 的最小实现（topic 一般是纯词，够用；特殊字符逐字节转义）。
fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.as_bytes() {
        let c = *b as char;
        if c.is_ascii_alphanumeric() || "-_.~".contains(c) {
            out.push(c);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

// ---------------- Transports ----------------

struct NatsTransport {
    client: hacs_l1_transport::nats_client::NatsClient,
    ping_timeout: Duration,
    connected: bool,
}

impl NatsTransport {
    fn new(
        host: &str,
        port: u16,
        user: &str,
        password_file: &str,
    ) -> Result<Self, String> {
        let password = if password_file.is_empty() { String::new() } else { read_secret(password_file)? };
        let cfg = hacs_l1_transport::nats_client::NatsConfig::from_hosts_str(
            &format!("{host}:{port}"),
            port,
        )
        .map_err(|e| format!("NATS 地址解析失败：{e}"))?
        .with_credentials(
            if user.is_empty() { None } else { Some(user.to_string()) },
            if password.is_empty() { None } else { Some(password) },
        )
        .with_name("roundtable");
        Ok(Self {
            client: hacs_l1_transport::nats_client::NatsClient::new(cfg),
            ping_timeout: Duration::from_secs_f64(PING_TIMEOUT_S),
            connected: false,
        })
    }
}

impl Transport for NatsTransport {
    fn name(&self) -> &'static str {
        "nats"
    }

    fn ping(&mut self, member: &str) -> bool {
        if !self.connected {
            if self.client.connect().is_err() {
                return false;
            }
            self.connected = true;
        }
        // 探针包故意不带 text：teamd 会立刻回 bad_request，拿到任何回包即在线。
        let payload = json!({"from": "roundtable", "probe": "alive"}).to_string();
        self.client
            .request(
                &format!("a2a.v1.req.{member}"),
                payload.as_bytes(),
                self.ping_timeout,
            )
            .is_some()
    }

    fn ask(&mut self, member: &str, text: &str, timeout: f64) -> (String, String, String) {
        let envelope = json!({"from": "roundtable", "text": text, "timeout_s": timeout}).to_string();
        if !self.connected && self.client.connect().is_err() {
            return ("error".into(), "".into(), "NATS 连接失败".into());
        }
        self.connected = true;
        match self.client.request(
            &format!("a2a.v1.req.{member}"),
            envelope.as_bytes(),
            Duration::from_secs_f64(timeout + 15.0),
        ) {
            None => (
                "absent".into(),
                "".into(),
                format!("点名超时（{timeout:.0}s 内无应答）"),
            ),
            Some(reply) => {
                let text = String::from_utf8_lossy(&reply).to_string();
                unwrap_reply(&text)
            }
        }
    }

    fn close(&mut self) {
        self.client.close();
    }
}

struct A2ATransport {
    endpoints: HashMap<String, String>,
    token: String,
}

impl A2ATransport {
    fn new(endpoints: &[String], token_file: &str) -> Result<Self, String> {
        let token = if token_file.is_empty() { String::new() } else { read_secret(token_file)? };
        let mut map = HashMap::new();
        for item in endpoints {
            let (member, url) = item.split_once('=').unwrap_or(("", ""));
            if !member.trim().is_empty() && !url.trim().is_empty() {
                map.insert(member.trim().to_string(), url.trim().to_string());
            }
        }
        Ok(Self { endpoints: map, token })
    }
}

impl Transport for A2ATransport {
    fn name(&self) -> &'static str {
        "a2a"
    }

    fn ping(&mut self, member: &str) -> bool {
        let Some(base) = self.endpoints.get(member) else { return false };
        let url = format!("{}/healthz", base.trim_end_matches('/'));
        matches!(
            hacs_l1_transport::http_client::get_json(&url, &[], Duration::from_secs_f64(PING_TIMEOUT_S)),
            Ok((200, _))
        )
    }

    fn ask(&mut self, member: &str, text: &str, timeout: f64) -> (String, String, String) {
        let Some(base) = self.endpoints.get(member) else {
            return (
                "absent".into(),
                "".into(),
                format!("没有 {member} 的 A2A 端点（--a2a-endpoint 没给）"),
            );
        };
        let url = format!("{}/a2a/{member}/messages", base.trim_end_matches('/'));
        let mut headers: Vec<(&str, &str)> = vec![("Content-Type", "application/json")];
        let auth;
        if !self.token.is_empty() {
            auth = format!("Bearer {}", self.token);
            headers.push(("Authorization", auth.as_str()));
        }
        match hacs_l1_transport::http_client::request_json(
            &url,
            "POST",
            Some(&json!({"text": text, "timeout_s": timeout})),
            &headers,
            Duration::from_secs_f64(timeout + 15.0),
        ) {
            Err(e) => ("error".into(), "".into(), format!("A2A 请求失败：{}", e.0)),
            Ok((code, value)) => {
                if !(200..300).contains(&code) {
                    return (
                        "error".into(),
                        "".into(),
                        format!("A2A 返回 {}：{}", code, truncate(&value.to_string(), 160)),
                    );
                }
                unwrap_reply(&value.to_string())
            }
        }
    }
}

// ---------------- CLI ----------------

#[derive(Default)]
struct Opts {
    topic: Option<String>,
    question: Option<String>,
    goal: Option<String>,
    agenda: Vec<String>,
    members: String,
    max_rounds: i64,
    max_total_rounds: i64,
    deadline: f64,
    session_id: Option<String>,
    moderator: Option<String>,
    transport: String,
    max_chars: usize,
    prompt_log: Option<String>,
    commit: bool,
    dry_run: bool,
    resume: Option<String>,
    dsn_file: String,
    blackboard: String,
    token_file: String,
    nats_host: String,
    nats_port: u16,
    nats_user: Option<String>,
    nats_password_file: Option<String>,
    a2a_endpoint: Vec<String>,
}

fn parse_args() -> Opts {
    let mut o = Opts {
        max_rounds: DEFAULT_MAX_ROUNDS,
        max_total_rounds: DEFAULT_MAX_TOTAL_ROUNDS,
        deadline: DEFAULT_DEADLINE_S,
        transport: "nats".into(),
        max_chars: DEFAULT_MAX_CHARS,
        dsn_file: std::env::var("AGENT_TEAM_PG_ENV_FILE").unwrap_or_default(),
        blackboard: std::env::var("AGENT_TEAM_BB_BASE").unwrap_or_default(),
        token_file: std::env::var("AGENT_TEAM_TOKEN_FILE").unwrap_or_default(),
        nats_host: std::env::var("AGENT_TEAM_NATS_HOST").unwrap_or_default(),
        nats_port: 9930,
        ..Default::default()
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut val = |name: &str| -> String {
            let _ = name;
            it.next().unwrap_or_default()
        };
        match arg.as_str() {
            "--topic" => o.topic = Some(val("topic")),
            "--question" => o.question = Some(val("question")),
            "--goal" => o.goal = Some(val("goal")),
            "--agenda" => o.agenda.push(val("agenda")),
            "--members" => o.members = val("members"),
            "--max-rounds" => o.max_rounds = val("x").parse().unwrap_or(DEFAULT_MAX_ROUNDS),
            "--max-total-rounds" => {
                o.max_total_rounds = val("x").parse().unwrap_or(DEFAULT_MAX_TOTAL_ROUNDS)
            }
            "--deadline" => o.deadline = val("x").parse().unwrap_or(DEFAULT_DEADLINE_S),
            "--session-id" => o.session_id = Some(val("sid")),
            "--moderator" => o.moderator = Some(val("mod")),
            "--transport" => o.transport = val("transport"),
            "--max-chars" => o.max_chars = val("x").parse().unwrap_or(DEFAULT_MAX_CHARS),
            "--prompt-log" => o.prompt_log = Some(val("log")),
            "--commit" => o.commit = true,
            "--dry-run" => o.dry_run = true,
            "--resume" => o.resume = Some(val("resume")),
            "--dsn-file" => o.dsn_file = val("dsn"),
            "--blackboard" => o.blackboard = val("bb"),
            "--token-file" => o.token_file = val("tok"),
            "--nats-host" => o.nats_host = val("h"),
            "--nats-port" => o.nats_port = val("p").parse().unwrap_or(9930),
            "--nats-user" => o.nats_user = Some(val("u")),
            "--nats-password-file" => o.nats_password_file = Some(val("pf")),
            "--a2a-endpoint" => o.a2a_endpoint.push(val("ep")),
            _ => {}
        }
    }
    o
}

/// 会前门禁：goal/agenda 缺一不开。返回 (goal, agenda, 缺失清单)。
fn gate_check(goal: &str, agenda_raw: &[String], resumed: Option<&Value>) -> (String, Vec<Value>, Vec<String>) {
    let mut goal = resumed
        .and_then(|r| r["goal"].as_str())
        .filter(|s| !s.trim().is_empty())
        .map(String::from)
        .unwrap_or_else(|| goal.trim().to_string());
    if goal.is_empty() {
        goal = String::new();
    }
    let mut missing = Vec::new();
    if goal.trim().is_empty() {
        missing.push("--goal（会议目标）：会议必须有明确目标，不给就不开会".to_string());
    }
    let mut agenda: Vec<Value> = resumed
        .and_then(|r| r["agenda"].as_array().cloned())
        .unwrap_or_default();
    if agenda.is_empty() {
        let (parsed, err) = parse_agenda(agenda_raw);
        if let Some(e) = err {
            missing.push(format!(
                "--agenda（会前议程）：{e}；格式 \"标题|预期产出; 标题|预期产出\""
            ));
        }
        agenda = parsed;
    }
    (goal, agenda, missing)
}

fn main() {
    let args = parse_args();
    let commit = args.commit && !args.dry_run;
    let mut stderr_err = |msg: &str| {
        eprintln!("{msg}");
    };

    // --- 存储与黑板：--commit 才接真家伙，否则一律 Null（演练不留任何痕迹）
    let mut store: Box<dyn Store> = if commit {
        let dsn = match read_dsn(&args.dsn_file) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("初始化失败：{e}");
                std::process::exit(2);
            }
        };
        match PgStore::new(&dsn) {
            Ok(s) => Box::new(s),
            Err(e) => {
                eprintln!("初始化失败：{e}");
                std::process::exit(2);
            }
        }
    } else {
        Box::new(NullStore::new())
    };
    let mut blackboard: Box<dyn Blackboard> = if commit {
        match BlackboardClient::new(&args.blackboard, &args.token_file) {
            Ok(b) => Box::new(b),
            Err(e) => {
                eprintln!("初始化失败：{e}");
                std::process::exit(2);
            }
        }
    } else {
        eprintln!(
            "[演练] 未给 --commit：不写黑板、不写 PG（{}）",
            if args.dry_run { "dry-run 假应答" } else { "真点名" }
        );
        Box::new(NullBlackboard::new(Vec::new()))
    };

    let mut resumed: Option<Value> = None;
    if let Some(sid) = &args.resume {
        match store.load_session(sid) {
            Some(v) => resumed = Some(v),
            None => {
                eprintln!("--resume 找不到 session：{sid}");
                std::process::exit(2);
            }
        }
    }

    // 题目与问题：目标是硬门禁，题目/问题不是（兜底要明说）
    let mut topic = resumed
        .as_ref()
        .and_then(|r| r["topic"].as_str())
        .filter(|s| !s.is_empty())
        .map(String::from)
        .or_else(|| args.topic.clone())
        .unwrap_or_else(|| {
            eprintln!("[warn] 未给 --topic，本场黑板条目落在缺省话题 {DEFAULT_TOPIC:?} 下");
            DEFAULT_TOPIC.to_string()
        });
    let _ = &mut topic;
    let goal_cli = args.goal.clone().unwrap_or_default();
    let question = resumed
        .as_ref()
        .and_then(|r| r["question"].as_str())
        .filter(|s| !s.is_empty())
        .map(String::from)
        .or_else(|| args.question.clone())
        .unwrap_or_else(|| goal_cli.clone());
    let members_raw: Vec<String> = resumed
        .as_ref()
        .and_then(|r| r["members"].as_array().cloned())
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_else(|| {
            args.members
                .split(',')
                .map(|m| m.trim().to_string())
                .filter(|m| !m.is_empty())
                .collect()
        });
    if question.trim().is_empty() {
        eprintln!("--question 必填（或给 --goal 兜底，或用 --resume 续跑已有会话）");
        std::process::exit(2);
    }
    let members = dedup_names(&members_raw);
    if members.len() < 2 {
        eprintln!("至少要两名成员才开得起圆桌（拿到的是 {members:?}）");
        std::process::exit(2);
    }

    // ---- 会前门禁（硬）
    let (goal, agenda, missing) = gate_check(&goal_cli, &args.agenda, resumed.as_ref());
    if !missing.is_empty() {
        eprintln!("【拒绝开会】会议有目标、会前有议程，这两条不满足就不开：");
        for m in &missing {
            eprintln!("  缺 {m}");
        }
        std::process::exit(2);
    }

    // ---- transport
    let fake = std::env::var("RT_FAKE_REPLY").map(|v| v == "1").unwrap_or(false);
    let mut transport: Box<dyn Transport> = if args.dry_run || fake {
        Box::new(hacs_l3_domain::roundtable_sched::DryRunTransport::new(
            HashMap::new(),
            &[],
            HashMap::new(),
            &[],
            7,
        ))
    } else if args.transport == "a2a" {
        match A2ATransport::new(&args.a2a_endpoint, &args.token_file) {
            Ok(t) => Box::new(t),
            Err(e) => {
                eprintln!("点名通道不可用：{e}");
                std::process::exit(2);
            }
        }
    } else {
        let (u, pf) = match (args.nats_user.clone(), args.nats_password_file.clone()) {
            (Some(u), Some(pf)) => (u, pf),
            _ => {
                eprintln!("--transport nats 需要 --nats-user 与 --nats-password-file");
                std::process::exit(2);
            }
        };
        match NatsTransport::new(&args.nats_host, args.nats_port, &u, &pf) {
            Ok(t) => Box::new(t),
            Err(e) => {
                eprintln!("点名通道不可用：{e}");
                std::process::exit(2);
            }
        }
    };

    let session_id = resumed
        .as_ref()
        .and_then(|r| r["session_id"].as_str())
        .map(String::from)
        .or_else(|| args.session_id.clone())
        .unwrap_or_else(|| new_session_id(now_epoch_f()));
    let max_rounds = resumed
        .as_ref()
        .and_then(|r| r["max_rounds"].as_i64())
        .filter(|v| *v > 0)
        .unwrap_or(args.max_rounds);
    let moderator = resumed
        .as_ref()
        .and_then(|r| r["moderator"].as_str())
        .filter(|s| !s.is_empty())
        .map(String::from)
        .or_else(|| args.moderator.clone())
        .unwrap_or_default();
    let start_round = resumed.as_ref().and_then(|r| r["round_no"].as_i64()).unwrap_or(0);

    let cfg = RoundtableConfig {
        session_id,
        topic,
        question,
        members,
        max_rounds,
        max_total_rounds: args.max_total_rounds.max(0),
        deadline_s: args.deadline,
        max_chars: args.max_chars,
        moderator,
        start_round,
        prompt_log: args.prompt_log,
        goal: goal.clone(),
        agenda,
    };
    if resumed.is_some() {
        println!("[续跑] {} 从第 {} 轮之后继续", cfg.session_id, cfg.start_round);
    }

    let mut out_line = |line: &str| println!("{line}");
    let mut rt = Roundtable::new(cfg, &mut out_line);
    let mut ctx = RunCtx {
        store: store.as_mut(),
        bb: blackboard.as_mut(),
        transport: transport.as_mut(),
    };
    let code = rt.run(&mut ctx);
    let _ = &mut stderr_err;
    let _ = ERR_SIGNAL;
    std::process::exit(code);
}
