//! `task_svc` —— 群任务清单服务（对应 Python `hub/l3_domain/task_svc.py`，871 行）。
//!
//! 设计口径（对齐 Python 源头注释，不消红）：
//! * 任务清单由**程序**维护，不依赖 Agent/LLM 记忆；按群（group_id）隔离；
//! * 队列顺序 = 优先级（special 优先）+ 创建时间；
//! * `/tasks/next` 取队首并置 running（at-least-once，dispatch_count 计数）；
//! * `/tasks/<id>/report` 按 task_id 幂等：重复汇报不改状态，返回 already_reported；
//! * 回帖账 task_replies 与 report 同一事务落库（PK=task_id 幂等闸门），
//!   投递失败进重试队列，到上限标记 dead，绝不静默丢；
//! * ack/lease：同 agent 重复 ack 幂等返回同一 token；他人持有 → leased_by_other；
//!   lease 超时返队列；dispatch_count 到上限 → failed 并冻结 chain_id；
//! * kind=reply 任务：report 成功后按 idem_key=`reply:<origin_task_id>` 派最小任务，
//!   **只派任务、绝不代写回答正文**（content 恒为 None）。

use hacs_l0_host::node_paths;
use rusqlite::Connection;
use serde_json::{json, Map, Value};
use std::time::{SystemTime, UNIX_EPOCH};

pub const REPLY_KIND: &str = "reply";
pub const CONTEXT_DIGEST_MAX_TOKEN: i64 = 800;
pub const CHAT_TEXT_MAX: usize = 3800; // 与 chat_svc TEXT_MAX(4000) 留余量
pub const CHAT_RETRY_MAX_DEFAULT: i64 = 20; // TASK_CHAT_RETRY_MAX 默认
pub const LEASE_S_MAX: i64 = 3600;
pub const VALID_STATUS: [&str; 4] = ["pending", "running", "done", "failed"];
pub const VALID_PRIORITY: [&str; 2] = ["normal", "special"];

/// reply 任务的 fallback 三级（口径：仍禁止代写正文）。
pub fn reply_fallback() -> Value {
    json!({
        "L1": "重派1次",
        "L2": "事实性降级通知（仍禁止代写正文）",
        "L3": "告警",
    })
}

// ------------------------------------------------------------------ 时间口径
/// CST（UTC+8）当前时刻的 ISO 字符串，`timespec="seconds"` 口径：
/// `2026-10-16T18:30:45+08:00`。与 Python `datetime.now(CST).isoformat(timespec="seconds")`
/// 逐字节同形（civil 历法自实现，不引依赖）。
pub fn now_iso() -> String {
    iso_from_secs(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0),
    )
}

/// Unix 秒（UTC）→ CST ISO 串。
pub fn iso_from_secs(secs_utc: i64) -> String {
    let cst = secs_utc + 8 * 3600;
    let days = cst.div_euclid(86_400);
    let rem = cst.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}+08:00",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// 解析 `YYYY-MM-DDTHH:MM:SS+08:00`（本服务自己写出的口径）→ Unix 秒。
/// 其他时区偏移也认（Python `datetime.fromisoformat` 行为）。
pub fn parse_iso(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 19 {
        return None;
    }
    let num = |a: usize, z: usize| -> Option<i64> { s.get(a..z)?.parse::<i64>().ok() };
    let y = num(0, 4)?;
    let mo = num(5, 7)?;
    let d = num(8, 10)?;
    let h = num(11, 13)?;
    let mi = num(14, 16)?;
    let se = num(17, 19)?;
    if !(b[4] == b'-'
        && b[7] == b'-'
        && (b[10] == b'T' || b[10] == b' ')
        && b[13] == b':'
        && b[16] == b':')
    {
        return None;
    }
    let mut secs = days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + se;
    // 尾部：`+08:00` / `-05:00` / `Z`
    let tail = &s[19..];
    let offset = if tail == "Z" || tail == "z" || tail.is_empty() {
        0
    } else if tail.len() >= 6
        && (tail.starts_with('+') || tail.starts_with('-'))
        && tail.as_bytes()[3] == b':'
    {
        let sign: i64 = if tail.starts_with('-') { -1 } else { 1 };
        let oh: i64 = tail[1..3].parse().ok()?;
        let om: i64 = tail[4..6].parse().ok()?;
        sign * (oh * 3600 + om * 60)
    } else {
        return None;
    };
    secs -= offset;
    Some(secs)
}

/// 天数（1970-01-01 = 0）→ (y, m, d)。Howard Hinnant 的 civil_from_days。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// (y, m, d) → 天数。days_from_civil（同上作者）。
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

// ------------------------------------------------------------------ token 估算
/// 粗估 token：CJK 字符 1:1，其余约 4 字符 1 token（保守偏高）。
pub fn est_tokens(text: &str) -> i64 {
    let mut cjk = 0i64;
    for ch in text.chars() {
        let o = ch as u32;
        if (0x2E80..=0x9FFF).contains(&o)
            || (0xF900..=0xFAFF).contains(&o)
            || (0xFF00..=0xFFEF).contains(&o)
        {
            cjk += 1;
        }
    }
    let other = text.chars().count() as i64 - cjk;
    cjk + (other + 3) / 4
}

/// 把上下文摘要裁到 ≤ max_token（保守估算），保证落库不超限。
/// Python 用字符数二分；这里按字符边界等价实现（末尾补 `…`，占 1 token）。
pub fn clip_digest(text: &str, max_token: i64) -> String {
    let s = text.trim();
    if est_tokens(s) <= max_token {
        return s.to_string();
    }
    let budget = std::cmp::max(1, max_token - 1);
    let chars: Vec<char> = s.chars().collect();
    let mut lo: usize = 0;
    let mut hi = chars.len();
    while lo < hi {
        let mid = (lo + hi + 1).div_ceil(2);
        if est_tokens(&chars[..mid].iter().collect::<String>()) <= budget {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    let mut out: String = chars[..lo].iter().collect();
    // Python `.rstrip()` 按 `str.isspace()` 语义（见 PARITY 既有口径）。
    while out.ends_with(|c: char| char_is_py_space(c)) {
        out.pop();
    }
    out.push('…');
    out
}

/// Python `str.isspace()` 单字符语义（与 L2 cli_backend 同一套：`\x07`→false、`\x1c`→true）。
fn char_is_py_space(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\x0b' | '\x0c' | '\r' | '\x1c'..='\x1f' | ' ' | '\u{85}' | '\u{a0}'
    ) || (c.is_whitespace() && c != '\u{0b}')
        || matches!(c, '\u{1c}'..='\u{1f}' | '\u{85}')
}

// ------------------------------------------------------------------ 回帖文案
/// 回帖正文：任务标题 / 结果摘要 / 执行 Agent 名（口径：群 #340）。
pub fn build_reply_text(
    task: &Value,
    agent: &str,
    result: Option<&str>,
    note: Option<&str>,
) -> String {
    let title = task
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let title = if title.is_empty() {
        "(无标题)".to_string()
    } else {
        title
    };
    let seq = task
        .get("msg_seq")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let head = if seq.is_empty() {
        format!("【任务完成】{title}")
    } else {
        format!("【任务完成】{seq} {title}")
    };
    let mut summary = note
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .or_else(|| {
            result
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
        })
        .unwrap_or_else(|| "done".to_string());
    if summary == "done" || summary == "success" || summary == "ok" {
        summary = "已完成".to_string();
    }
    let text = format!("{head}\n结果：{summary}\n执行：{}", agent.trim());
    if text.chars().count() > CHAT_TEXT_MAX {
        // Python `text[:CHAT_TEXT_MAX-1] + "…"`：按**字符**截断（Python str 索引）。
        let mut s: String = text.chars().take(CHAT_TEXT_MAX - 1).collect();
        s.push('…');
        s
    } else {
        text
    }
}

// ------------------------------------------------------------------ Store
pub struct Store {
    pub db_path: String,
    conn: std::sync::Mutex<Connection>,
}

/// 单连接 + 锁；WAL 下读写并发可以接受（本用量极小）。对应 Python `Store`。
impl Store {
    pub fn open(db_path: &str) -> rusqlite::Result<Self> {
        let conn = Connection::open(db_path)?;
        conn.busy_timeout(std::time::Duration::from_secs(15)).ok();
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA foreign_keys=ON;
             CREATE TABLE IF NOT EXISTS tasks (
               id             TEXT PRIMARY KEY,
               group_id       TEXT NOT NULL,
               msg_seq        TEXT,
               proposer       TEXT,
               created_at     TEXT NOT NULL,
               title          TEXT NOT NULL,
               content        TEXT,
               owner          TEXT,
               priority       TEXT NOT NULL DEFAULT 'normal',
               status         TEXT NOT NULL DEFAULT 'pending',
               dispatch_count INTEGER NOT NULL DEFAULT 0,
               updated_at     TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_tasks_group_status ON tasks(group_id, status);
             CREATE TABLE IF NOT EXISTS task_reports (
               task_id    TEXT PRIMARY KEY,
               agent      TEXT,
               result     TEXT,
               note       TEXT,
               created_at TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS task_replies (
               task_id     TEXT PRIMARY KEY,
               group_id    TEXT,
               agent       TEXT,
               text        TEXT,
               status      TEXT NOT NULL DEFAULT 'pending',
               attempts    INTEGER NOT NULL DEFAULT 0,
               message_id  TEXT,
               message_seq INTEGER,
               last_error  TEXT,
               created_at  TEXT NOT NULL,
               updated_at  TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_task_replies_status ON task_replies(status);
             CREATE TABLE IF NOT EXISTS chain_freezes (
               chain_id   TEXT PRIMARY KEY,
               reason     TEXT,
               frozen_at  TEXT NOT NULL,
               frozen_by  TEXT
             );",
        )?;
        // 老库迁移：只新增列，幂等（已存在则忽略）。
        let have: Vec<String> = {
            let mut st = conn.prepare("PRAGMA table_info(tasks)")?;
            let rows = st.query_map([], |r| r.get::<_, String>(1))?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        for (col, decl) in [
            ("chain_id", "TEXT"),
            ("lease_token", "TEXT"),
            ("lease_expire_at", "TEXT"),
            ("lease_agent", "TEXT"),
            ("lease_s", "INTEGER"),
            ("ack_count", "INTEGER DEFAULT 0"),
            ("kind", "TEXT"),
            ("idem_key", "TEXT"),
            ("origin_task_id", "TEXT"),
            ("context_digest", "TEXT"),
            ("groups", "TEXT"),
            ("fallback", "TEXT"),
            ("hold", "INTEGER DEFAULT 0"),
        ] {
            if !have.iter().any(|h| h == col) {
                conn.execute(&format!("ALTER TABLE tasks ADD COLUMN {col} {decl}"), [])?;
            }
        }
        conn.execute_batch(
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_tasks_idem_key ON tasks(idem_key) WHERE idem_key IS NOT NULL;
             CREATE INDEX IF NOT EXISTS idx_tasks_lease ON tasks(status, lease_expire_at);",
        )?;
        Ok(Self {
            db_path: db_path.to_string(),
            conn: std::sync::Mutex::new(conn),
        })
    }

    fn with<T>(&self, f: impl FnOnce(&Connection) -> rusqlite::Result<T>) -> rusqlite::Result<T> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        f(&conn)
    }

    fn rows(&self, sql: &str, args: &[&dyn rusqlite::ToSql]) -> Vec<Value> {
        self.with(|c| {
            let mut st = c.prepare(sql)?;
            let names: Vec<String> = st.column_names().iter().map(|s| s.to_string()).collect();
            let mut rows = st.query(args)?;
            let mut out = Vec::new();
            while let Some(row) = rows.next()? {
                let mut m = Map::new();
                for (i, name) in names.iter().enumerate() {
                    let v: Value = match row.get_ref(i)? {
                        rusqlite::types::ValueRef::Null => Value::Null,
                        rusqlite::types::ValueRef::Integer(n) => json!(n),
                        rusqlite::types::ValueRef::Real(f) => json!(f),
                        rusqlite::types::ValueRef::Text(t) => {
                            json!(String::from_utf8_lossy(t).to_string())
                        }
                        rusqlite::types::ValueRef::Blob(b) => json!(b.to_vec()),
                    };
                    m.insert(name.clone(), v);
                }
                out.push(Value::Object(m));
            }
            Ok(out)
        })
        .unwrap_or_default()
    }

    fn one(&self, sql: &str, args: &[&dyn rusqlite::ToSql]) -> Option<Value> {
        self.rows(sql, args).into_iter().next()
    }

    /// `PRAGMA` 帮手（迁移测试用）。
    pub fn table_columns(&self) -> Vec<String> {
        self.with(|c| {
            let mut st = c.prepare("PRAGMA table_info(tasks)")?;
            let rows = st.query_map([], |r| r.get::<_, String>(1))?;
            rows.collect()
        })
        .unwrap_or_default()
    }

    // --- create -------------------------------------------------------
    /// 幂等：同 idem_key 不重复派发（返回既有任务，already_exists=true）。
    #[allow(clippy::too_many_arguments)]
    pub fn create(
        &self,
        group_id: &str,
        title: &str,
        content: Option<&str>,
        owner: Option<&str>,
        priority: &str,
        msg_seq: Option<&str>,
        proposer: Option<&str>,
        status: &str,
        task_id: Option<&str>,
        chain_id: Option<&str>,
        kind: Option<&str>,
        idem_key: Option<&str>,
        origin_task_id: Option<&str>,
        context_digest: Option<&str>,
        groups: Option<&str>,
        fallback: Option<&str>,
        lease_s: Option<i64>,
        hold: i64,
    ) -> Result<Value, String> {
        if !VALID_PRIORITY.contains(&priority) {
            return Err("bad_priority".to_string());
        }
        if !VALID_STATUS.contains(&status) {
            return Err("bad_status".to_string());
        }
        if let Some(key) = idem_key {
            if let Some(prev) = self.idem_hit(key) {
                return Ok(prev);
            }
        }
        let tid = task_id
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("T-{}", uuid4_hex8()));
        let ts = now_iso();
        let ins = self.with(|c| {
            c.execute(
                "INSERT INTO tasks(id,group_id,msg_seq,proposer,created_at,title,content,\
                 owner,priority,status,dispatch_count,updated_at,chain_id,kind,idem_key,\
                 origin_task_id,context_digest,groups,fallback,lease_s,hold) \
                 VALUES(?,?,?,?,?,?,?,?,?,?,0,?,?,?,?,?,?,?,?,?,?)",
                rusqlite::params![
                    tid,
                    group_id,
                    msg_seq,
                    proposer,
                    ts,
                    title,
                    content,
                    owner,
                    priority,
                    status,
                    ts,
                    chain_id,
                    kind,
                    idem_key,
                    origin_task_id,
                    context_digest,
                    groups,
                    fallback,
                    lease_s,
                    hold,
                ],
            )
        });
        match ins {
            Ok(_) => Ok(self.get(&tid).unwrap_or(Value::Null)),
            Err(rusqlite::Error::SqliteFailure(e, _))
                if e.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                // 并发下唯一索引兜底：别人刚插进去，返回既有那条。
                match idem_key.and_then(|k| self.idem_hit(k)) {
                    Some(prev) => Ok(prev),
                    None => Err("integrity_error".to_string()),
                }
            }
            Err(e) => Err(e.to_string()),
        }
    }

    fn idem_hit(&self, key: &str) -> Option<Value> {
        let mut v = self.one("SELECT * FROM tasks WHERE idem_key=?", &[&key])?;
        if let Some(m) = v.as_object_mut() {
            m.insert("already_exists".into(), json!(true));
            m.insert("idem_key_hit".into(), json!(true));
        }
        Some(v)
    }

    pub fn get(&self, tid: &str) -> Option<Value> {
        self.one("SELECT * FROM tasks WHERE id=?", &[&tid])
    }

    pub fn list(&self, group_id: &str, status: Option<&str>, limit: i64) -> Vec<Value> {
        match status {
            Some(st) => self.rows(
                "SELECT * FROM tasks WHERE group_id=? AND status=? \
                 ORDER BY CASE priority WHEN 'special' THEN 0 ELSE 1 END, \
                 created_at ASC, rowid ASC LIMIT ?",
                &[&group_id, &st, &limit],
            ),
            None => self.rows(
                "SELECT * FROM tasks WHERE group_id=? \
                 ORDER BY CASE priority WHEN 'special' THEN 0 ELSE 1 END, \
                 created_at ASC, rowid ASC LIMIT ?",
                &[&group_id, &limit],
            ),
        }
    }

    /// 取队首（优先级+时间）派发；只派 pending 且未被别人认领的。
    /// 派发前先做一次惰性 lease 巡检（超时任务返队列）。
    pub fn next_for(&self, group_id: &str, agent: &str) -> Option<Value> {
        self.reap_leases(3); // 惰性巡检（max_dispatch 常态默认 3，见 LEASE_MAX_DISPATCH）
        let tid = {
            let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
            let row: Option<String> = conn
                .query_row(
                    "SELECT id FROM tasks WHERE group_id=?1 AND status='pending' \
                     AND (owner IS NULL OR owner='' OR owner=?2) \
                     AND COALESCE(hold,0)=0 \
                     AND (chain_id IS NULL OR chain_id='' OR chain_id NOT IN \
                     (SELECT chain_id FROM chain_freezes)) \
                     ORDER BY CASE priority WHEN 'special' THEN 0 ELSE 1 END, \
                     created_at ASC, rowid ASC LIMIT 1",
                    rusqlite::params![group_id, agent],
                    |r| r.get(0),
                )
                .ok();
            let tid = row?;
            let _ = conn.execute(
                "UPDATE tasks SET status='running', dispatch_count=dispatch_count+1, \
                 owner=CASE WHEN owner IS NULL OR owner='' THEN ?1 ELSE owner END, \
                 updated_at=?2 WHERE id=?3",
                rusqlite::params![agent, now_iso(), tid],
            );
            let _ = conn.execute_batch("COMMIT").ok(); // autocommit 模式下无事务可提交，忽略
            tid
        };
        self.get(&tid)
    }

    // --- ack / lease / chain 冻结 ------------------------------------
    fn freeze_chain(&self, chain_id: &str, reason: &str, by: Option<&str>) -> Option<Value> {
        if chain_id.is_empty() {
            return None;
        }
        let _ = self.with(|c| {
            c.execute(
                "INSERT INTO chain_freezes(chain_id,reason,frozen_at,frozen_by) \
                 VALUES(?1,?2,?3,?4) ON CONFLICT(chain_id) DO UPDATE SET \
                 reason=COALESCE(excluded.reason,reason)",
                rusqlite::params![chain_id, reason, now_iso(), by],
            )
        });
        self.one("SELECT * FROM chain_freezes WHERE chain_id=?", &[&chain_id])
    }

    pub fn chain_frozen(&self, chain_id: &str) -> Option<Value> {
        if chain_id.is_empty() {
            return None;
        }
        self.one("SELECT * FROM chain_freezes WHERE chain_id=?", &[&chain_id])
    }

    pub fn freeze_list(&self) -> Vec<Value> {
        self.rows("SELECT * FROM chain_freezes ORDER BY frozen_at DESC", &[])
    }

    pub fn unfreeze_chain(&self, chain_id: &str) -> bool {
        self.with(|c| c.execute("DELETE FROM chain_freezes WHERE chain_id=?", [&chain_id]))
            .map(|n| n > 0)
            .unwrap_or(false)
    }

    /// ack = 领任务的显式认领 + 租约。
    /// 幂等：同一 agent 在 lease 未过期前重复 ack 返回**同一个 lease_token**。
    pub fn ack(
        &self,
        tid: &str,
        agent: &str,
        lease_s: Option<i64>,
        _lease_s_default: i64,
        whitelist: &[String],
    ) -> Value {
        let Some(t) = self.get(tid) else {
            return json!({"ok": false, "error": "not_found", "task_id": tid});
        };
        if !agent_allowed(agent, whitelist) {
            return json!({
                "ok": false, "error": "agent_not_allowed", "agent": agent,
                "whitelist": whitelist,
            });
        }
        let Some(ls_req) = lease_s else {
            return json!({"ok": false, "error": "bad_lease_s"});
        };
        let ls = ls_req.clamp(1, LEASE_S_MAX);
        let status = t.get("status").and_then(|v| v.as_str()).unwrap_or("");
        if status == "done" || status == "failed" {
            return json!({"ok": false, "error": "not_dispatchable", "status": status, "task_id": tid});
        }
        let cur_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let exp = t
            .get("lease_expire_at")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let mut active = false;
        if let (Some(tok), Some(e)) = (
            t.get("lease_token").and_then(|v| v.as_str()),
            exp.as_deref(),
        ) {
            if !tok.is_empty() {
                active = parse_iso(e).map(|es| es > cur_secs).unwrap_or(false);
            }
        }
        if active {
            let holder = t
                .get("lease_agent")
                .and_then(|v| v.as_str())
                .or_else(|| t.get("owner").and_then(|v| v.as_str()))
                .unwrap_or("");
            if holder == agent {
                return json!({
                    "ok": true, "already_acked": true, "task_id": tid,
                    "lease_token": t.get("lease_token"),
                    "lease_expire_at": exp,
                    "lease_s": t.get("lease_s").and_then(|v| v.as_i64()).filter(|v| *v != 0).unwrap_or(ls),
                    "task": t,
                });
            }
            return json!({
                "ok": false, "error": "leased_by_other",
                "holder": t.get("lease_agent"), "lease_expire_at": exp, "task_id": tid,
            });
        }
        let token = format!("LT-{}", uuid4_hex32());
        let exp_iso = iso_from_secs(cur_secs + ls);
        let _ = self.with(|c| {
            c.execute(
                "UPDATE tasks SET status='running', lease_token=?1, lease_expire_at=?2, \
                 lease_agent=?3, lease_s=?4, ack_count=COALESCE(ack_count,0)+1, \
                 dispatch_count=dispatch_count+1, \
                 owner=CASE WHEN owner IS NULL OR owner='' THEN ?3 ELSE owner END, \
                 updated_at=?5 WHERE id=?6",
                rusqlite::params![token, exp_iso, agent, ls, now_iso(), tid],
            )
        });
        json!({
            "ok": true, "already_acked": false, "task_id": tid,
            "lease_token": token, "lease_expire_at": exp_iso, "lease_s": ls,
            "agent": agent, "task": self.get(tid),
        })
    }

    /// lease 超时巡检：返队列；超过 dispatch 上限 → failed 并冻结同 chain_id。
    /// 同时被后台线程（周期）和 next_for/ack（惰性）调用，幂等安全。
    pub fn reap_leases(&self, max_dispatch: i64) -> Vec<Value> {
        let mut out = Vec::new();
        let ts = now_iso();
        let rows = self.rows(
            "SELECT * FROM tasks WHERE status='running' AND lease_token IS NOT NULL \
             AND lease_expire_at IS NOT NULL AND lease_expire_at < ?",
            &[&ts],
        );
        for row in rows {
            let Some(tid) = row.get("id").and_then(|v| v.as_str()).map(str::to_string) else {
                continue;
            };
            let dc = row
                .get("dispatch_count")
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            let chain_id = row
                .get("chain_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if dc >= max_dispatch {
                let _ = self.with(|c| {
                    c.execute(
                        "UPDATE tasks SET status='failed', lease_token=NULL, \
                         lease_expire_at=NULL, lease_agent=NULL, updated_at=? WHERE id=?",
                        rusqlite::params![now_iso(), tid],
                    )
                });
                if !chain_id.is_empty() {
                    self.freeze_chain(
                        &chain_id,
                        &format!("lease_timeout_exceeded_dispatch_max({dc})"),
                        Some("task_svc.lease_reaper"),
                    );
                }
                out.push(json!({
                    "task_id": tid, "action": "failed_and_frozen",
                    "dispatch_count": dc,
                    "chain_id": if chain_id.is_empty() { Value::Null } else { json!(chain_id) },
                }));
            } else {
                let _ = self.with(|c| {
                    c.execute(
                        "UPDATE tasks SET status='pending', lease_token=NULL, \
                         lease_expire_at=NULL, lease_agent=NULL, updated_at=? WHERE id=?",
                        rusqlite::params![now_iso(), tid],
                    )
                });
                out.push(json!({
                    "task_id": tid, "action": "requeued",
                    "dispatch_count": dc,
                    "chain_id": if chain_id.is_empty() { Value::Null } else { json!(chain_id) },
                }));
            }
        }
        out
    }

    /// 当前持有 lease 的任务（观测用）。
    pub fn lease_snapshot(&self) -> Vec<Value> {
        self.rows(
            "SELECT id,group_id,status,lease_agent,lease_expire_at,dispatch_count,ack_count \
             FROM tasks WHERE lease_token IS NOT NULL ORDER BY lease_expire_at ASC",
            &[],
        )
    }

    // --- reply 任务派发 ----------------------------------------------
    /// report 成功后按 idem_key 派一条**最小 reply 任务**。
    /// 硬口径：idem_key = "reply:" + origin_task_id；**只派任务，不生成任何回答正文**
    /// （content 恒为 None，本函数没有任何产出回答文本的分支）。
    pub fn dispatch_reply(
        &self,
        origin: &Value,
        report_agent: Option<&str>,
        note: Option<&str>,
        result: Option<&str>,
    ) -> Value {
        let Some(oid) = origin.get("id").and_then(|v| v.as_str()) else {
            return json!({"ok": false, "error": "no_origin"});
        };
        let idem = format!("reply:{oid}");
        if origin.get("kind").and_then(|v| v.as_str()) == Some(REPLY_KIND) {
            // reply 任务自身不再派生 reply，防止套娃。
            return json!({"ok": false, "error": "origin_is_reply_task", "idem_key": idem});
        }
        let owner = origin
            .get("owner")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| {
                report_agent
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
            });
        let digest = clip_digest(
            &format!(
                "任务:{}\n标题:{}\n内容:{}\n上报Agent:{}\n上报结果:{}\n备注:{}",
                oid,
                trunc_chars(
                    origin.get("title").and_then(|v| v.as_str()).unwrap_or(""),
                    200
                ),
                trunc_chars(
                    origin.get("content").and_then(|v| v.as_str()).unwrap_or(""),
                    1200
                ),
                report_agent.unwrap_or(""),
                result.unwrap_or(""),
                note.unwrap_or(""),
            ),
            CONTEXT_DIGEST_MAX_TOKEN,
        );
        let title_orig = origin.get("title").and_then(|v| v.as_str()).unwrap_or(oid);
        let title = format!(
            "【reply·待原 Owner 自行作答】{}",
            trunc_chars(title_orig, 80)
        );
        let group_id = origin
            .get("group_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let fb = serde_json::to_string(&reply_fallback()).unwrap_or_default();
        let r = self.create(
            group_id,
            &title,
            None, // 禁止程序代写：正文只能由 Agent 自己产出
            owner.as_deref(),
            "normal",
            origin.get("msg_seq").and_then(|v| v.as_str()),
            Some("task_svc"),
            "pending",
            None,
            origin.get("chain_id").and_then(|v| v.as_str()),
            Some(REPLY_KIND),
            Some(&idem),
            Some(oid),
            Some(&digest),
            Some(group_id),
            Some(&fb),
            Some(120),
            0,
        );
        let Ok(r) = r else {
            return json!({"ok": false, "error": r.unwrap_err()});
        };
        let already = r
            .get("idem_key_hit")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let mut reply_task = Map::new();
        for k in [
            "id",
            "group_id",
            "kind",
            "idem_key",
            "origin_task_id",
            "owner",
            "status",
            "lease_s",
            "fallback",
            "context_digest",
            "groups",
        ] {
            reply_task.insert(k.to_string(), r.get(k).cloned().unwrap_or(Value::Null));
        }
        json!({
            "ok": true, "idem_key": idem, "already_dispatched": already,
            "reply_task": Value::Object(reply_task),
            "autopost_disabled": true,
        })
    }

    // --- patch --------------------------------------------------------
    #[allow(clippy::too_many_arguments)]
    pub fn patch(
        &self,
        tid: &str,
        fields: &Value,
        no_reply: bool,
        _whitelist: &[String],
        chat_base: &str,
        agent_fallback: &str,
        retry_max: i64,
    ) -> Result<Option<Value>, String> {
        let Some(t) = self.get(tid) else {
            return Ok(None);
        };
        let mut sets: Vec<&str> = Vec::new();
        let mut args: Vec<String> = Vec::new();
        for k in [
            "status", "owner", "priority", "title", "content", "msg_seq", "proposer",
        ] {
            let Some(v) = fields.get(k) else { continue };
            if v.is_null() {
                continue;
            }
            let s = match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            if k == "status" && !VALID_STATUS.contains(&s.as_str()) {
                return Err("bad_status".to_string());
            }
            if k == "priority" && !VALID_PRIORITY.contains(&s.as_str()) {
                return Err("bad_priority".to_string());
            }
            sets.push(k);
            args.push(s);
        }
        if !sets.is_empty() {
            let sql = format!(
                "UPDATE tasks SET {}, updated_at=?{} WHERE id=?{}",
                sets.iter()
                    .map(|k| format!("{k}=?"))
                    .collect::<Vec<_>>()
                    .join(","),
                sets.len() + 1,
                sets.len() + 2,
            );
            let ts = now_iso();
            let tid_s = tid.to_string();
            let mut params: Vec<&dyn rusqlite::ToSql> =
                args.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
            params.push(&ts);
            params.push(&tid_s);
            let _ = self.with(|c| c.execute(&sql, params.as_slice()));
            // #418-1 任务结束报告（PATCH 路径补齐）：状态**首次**落到终态时补一条
            // 「【任务完成】…」回帖进群；与 report 共用 task_replies 的 PK 幂等闸门。
            let new_st = fields.get("status").and_then(|v| v.as_str()).unwrap_or("");
            let old_st = t.get("status").and_then(|v| v.as_str()).unwrap_or("");
            let kind = t.get("kind").and_then(|v| v.as_str()).unwrap_or("");
            if (new_st == "done" || new_st == "failed") && new_st != old_st && kind != REPLY_KIND {
                let agent = t
                    .get("owner")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .or_else(|| {
                        fields
                            .get("owner")
                            .and_then(|v| v.as_str())
                            .map(str::to_string)
                    })
                    .unwrap_or_else(|| "hermes".to_string());
                let text = build_reply_text(&t, &agent, Some(new_st), None);
                let _ = self.with(|c| {
                    c.execute(
                        "INSERT OR IGNORE INTO task_replies(task_id,group_id,agent,text,\
                         status,attempts,created_at,updated_at) VALUES(?1,?2,?3,?4,'pending',0,?5,?5)",
                        rusqlite::params![tid, t.get("group_id").and_then(|v| v.as_str()), agent, text, now_iso()],
                    )
                });
            }
        }
        let out = self.get(tid);
        // 事务外投递（不持库锁跑网络 IO）；失败留给 reply_retry_loop 补投。
        let new_st = fields.get("status").and_then(|v| v.as_str()).unwrap_or("");
        let kind = t.get("kind").and_then(|v| v.as_str()).unwrap_or("");
        if (new_st == "done" || new_st == "failed") && kind != REPLY_KIND && !no_reply {
            if let Some(row) = self.reply_get(tid) {
                if row.get("status").and_then(|v| v.as_str()) != Some("posted") {
                    if let Some(mut o) = out {
                        let delivered =
                            self.reply_deliver(tid, chat_base, agent_fallback, retry_max);
                        if let Some(m) = o.as_object_mut() {
                            m.insert("reply".into(), delivered);
                        }
                        return Ok(Some(o));
                    }
                }
            }
        }
        Ok(out)
    }

    pub fn delete(&self, tid: &str) -> bool {
        self.with(|c| c.execute("DELETE FROM tasks WHERE id=?", [&tid]))
            .map(|n| n > 0)
            .unwrap_or(false)
    }

    // --- report -------------------------------------------------------
    /// 幂等：同一 task_id 重复汇报不改状态。
    /// * chain_id：上报时写入；result=failed → 按 chain_id 冻结；
    /// * 释放 lease（汇报即交付，租约作废）；
    /// * kind != reply 且 result 成功 → 派一条最小 reply 任务（幂等由 idem_key 兜底）；
    /// * kind == reply → 不做任何自动回帖（禁止程序代写）。
    pub fn report(
        &self,
        tid: &str,
        agent: Option<&str>,
        result: &str,
        note: Option<&str>,
        chain_id: Option<&str>,
    ) -> Value {
        let Some(t) = self.get(tid) else {
            return json!({"ok": false, "error": "not_found", "task_id": tid});
        };
        let cid = chain_id
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| {
                t.get("chain_id")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
            });
        let is_reply_origin = t.get("kind").and_then(|v| v.as_str()) == Some(REPLY_KIND);
        // 幂等检查 + 落库：整个块持锁（Python 是单事务；这里先查后写在同一把锁内）。
        {
            let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
            let prev: Option<(String, String, String)> = conn
                .query_row(
                    "SELECT agent,result,created_at FROM task_reports WHERE task_id=?",
                    [tid],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .ok();
            if let Some((pagent, presult, pat)) = prev {
                drop(conn);
                return json!({
                    "ok": true, "already_reported": true, "task_id": tid,
                    "task": self.get(tid),
                    "reply": self.reply_get(tid),
                    "reply_dispatch": self.one(
                        "SELECT * FROM tasks WHERE idem_key=?", &[&format!("reply:{tid}")]),
                    "first_report": {"agent": pagent, "result": presult, "at": pat},
                });
            }
            let st =
                if result.is_empty() || result == "done" || result == "success" || result == "ok" {
                    "done"
                } else {
                    "failed"
                };
            let _ = conn.execute(
                "INSERT INTO task_reports(task_id,agent,result,note,created_at) VALUES(?1,?2,?3,?4,?5)",
                rusqlite::params![tid, agent, result, note, now_iso()],
            );
            let _ = conn.execute(
                "UPDATE tasks SET status=?1, chain_id=COALESCE(?2,chain_id), lease_token=NULL, \
                 lease_expire_at=NULL, lease_agent=NULL, updated_at=?3 WHERE id=?4",
                rusqlite::params![st, cid, now_iso(), tid],
            );
            if st == "failed" {
                if let Some(c) = &cid {
                    if !c.is_empty() {
                        // failed 状态按 chain_id 冻结：同 chain 任务禁止再次派发。
                        let _ = conn.execute(
                            "INSERT INTO chain_freezes(chain_id,reason,frozen_at,frozen_by) \
                             VALUES(?1,?2,?3,?4) ON CONFLICT(chain_id) DO UPDATE SET \
                             reason=COALESCE(excluded.reason,reason)",
                            rusqlite::params![
                                c,
                                format!("report_failed task={tid}"),
                                now_iso(),
                                agent
                            ],
                        );
                    }
                }
            }
            // 回帖与 report **同一事务**落库（PK=task_id 即幂等闸门）。
            if !is_reply_origin {
                let reply_agent = agent
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .or_else(|| {
                        t.get("owner")
                            .and_then(|v| v.as_str())
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .map(str::to_string)
                    })
                    .unwrap_or_else(|| "hermes".to_string());
                let text = build_reply_text(&t, &reply_agent, Some(result), note);
                let _ = conn.execute(
                    "INSERT OR IGNORE INTO task_replies(task_id,group_id,agent,text,status,\
                     attempts,created_at,updated_at) VALUES(?1,?2,?3,?4,'pending',0,?5,?5)",
                    rusqlite::params![
                        tid,
                        t.get("group_id").and_then(|v| v.as_str()),
                        reply_agent,
                        text,
                        now_iso()
                    ],
                );
            }
        }
        // --- 事务外：派 reply 任务（只派任务，绝不生成回答正文）
        let mut dispatch = Value::Null;
        let st_done = {
            let got = self.get(tid);
            got.as_ref()
                .and_then(|v| v.get("status"))
                .and_then(|v| v.as_str())
                == Some("done")
        };
        if st_done && !is_reply_origin {
            dispatch = match self.get(tid) {
                Some(t2) => self.dispatch_reply(&t2, agent, note, Some(result)),
                None => json!({"ok": false, "error": "origin_missing"}),
            };
        }
        let chain_frozen = cid
            .as_deref()
            .filter(|c| !c.is_empty())
            .and_then(|c| self.chain_frozen(c))
            .is_some();
        json!({
            "ok": true, "already_reported": false, "task_id": tid,
            "task": self.get(tid),
            "reply_queued": !is_reply_origin,
            "autopost_disabled": is_reply_origin,
            "chain_id": cid.as_deref().map(|c| json!(c)).unwrap_or(Value::Null),
            "chain_frozen": chain_frozen,
            "reply_dispatch": dispatch,
        })
    }

    // --- 回帖台账（task_replies） ------------------------------------
    /// 把这个 task 的回帖投出去（幂等：已 posted 直接返回）。
    /// `chat_post` 由装配层注入（HTTP 依赖不进域层）；`chat_base` 仅留作口径记录。
    pub fn reply_deliver(
        &self,
        tid: &str,
        chat_base: &str,
        agent_fallback: &str,
        retry_max: i64,
    ) -> Value {
        let _ = (chat_base, agent_fallback); // 口径：文案已在入队时定死
        let Some(row) = self.reply_get(tid) else {
            return json!({"ok": false, "status": "missing", "error": "no_reply_row"});
        };
        if row.get("status").and_then(|v| v.as_str()) == Some("posted") {
            return json!({
                "ok": true, "status": "posted", "already": true,
                "message_id": row.get("message_id"),
                "message_seq": row.get("message_seq"),
            });
        }
        let group = row.get("group_id").and_then(|v| v.as_str()).unwrap_or("");
        let agent = row.get("agent").and_then(|v| v.as_str()).unwrap_or("");
        let text = row.get("text").and_then(|v| v.as_str()).unwrap_or("");
        let (ok, info) =
            crate::task_svc_chat::chat_post_message(group, agent, text, chat_base, None);
        if ok {
            let r = self.reply_mark(
                tid,
                "posted",
                info.get("message_id").and_then(|v| v.as_str()),
                info.get("message_seq").and_then(|v| v.as_i64()),
                None,
            );
            return json!({
                "ok": true, "status": "posted",
                "message_id": r.as_ref().and_then(|v| v.get("message_id")).cloned().unwrap_or(Value::Null),
                "message_seq": r.as_ref().and_then(|v| v.get("message_seq")).cloned().unwrap_or(Value::Null),
            });
        }
        let attempts = row.get("attempts").and_then(|v| v.as_i64()).unwrap_or(0);
        let status = if attempts + 1 < retry_max {
            "failed"
        } else {
            "dead"
        };
        let r = self.reply_mark(
            tid,
            status,
            None,
            None,
            info.get("error").and_then(|v| v.as_str()),
        );
        json!({
            "ok": false, "status": status,
            "attempts": r.as_ref().and_then(|v| v.get("attempts")).cloned().unwrap_or(Value::Null),
            "error": info.get("error"),
            "retry_queue": status == "failed",
        })
    }

    pub fn reply_get(&self, tid: &str) -> Option<Value> {
        self.one("SELECT * FROM task_replies WHERE task_id=?", &[&tid])
    }

    /// 待投递（含失败待重试）的回帖；dead 与 posted 不再取。
    pub fn reply_pending(&self, retry_max: i64, limit: i64) -> Vec<Value> {
        self.rows(
            "SELECT * FROM task_replies WHERE status IN ('pending','failed') \
             AND attempts < ? ORDER BY created_at ASC LIMIT ?",
            &[&retry_max, &limit],
        )
    }

    pub fn reply_mark(
        &self,
        tid: &str,
        status: &str,
        message_id: Option<&str>,
        message_seq: Option<i64>,
        error: Option<&str>,
    ) -> Option<Value> {
        let err = error.unwrap_or("").chars().take(500).collect::<String>();
        let err_opt = if err.is_empty() { None } else { Some(err) };
        let _ = self.with(|c| {
            c.execute(
                "UPDATE task_replies SET status=?1, attempts=attempts+1, \
                 message_id=COALESCE(?2,message_id), message_seq=COALESCE(?3,message_seq), \
                 last_error=?4, updated_at=?5 WHERE task_id=?6",
                rusqlite::params![status, message_id, message_seq, err_opt, now_iso(), tid],
            )
        });
        self.reply_get(tid)
    }

    pub fn stats(&self, group_id: Option<&str>) -> Vec<Value> {
        match group_id {
            Some(g) => self.rows(
                "SELECT group_id, status, COUNT(*) n FROM tasks WHERE group_id=? \
                 GROUP BY group_id, status",
                &[&g],
            ),
            None => self.rows(
                "SELECT group_id, status, COUNT(*) n FROM tasks GROUP BY group_id, status",
                &[],
            ),
        }
    }

    pub fn groups(&self) -> Vec<String> {
        let mut gs: Vec<String> = self
            .stats(None)
            .iter()
            .filter_map(|r| {
                r.get("group_id")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
            })
            .collect();
        gs.sort();
        gs.dedup();
        gs
    }
}

/// ack 白名单：空 = 不启用（放行全部）。
pub fn agent_allowed(agent: &str, whitelist: &[String]) -> bool {
    if whitelist.is_empty() {
        return true;
    }
    whitelist.iter().any(|w| w == agent.trim())
}

pub fn default_db_path() -> String {
    format!("{}/task_svc.db", node_paths::runtime_dir(None))
}

// ------------------------------------------------------------------ 随机源
/// Python `uuid.uuid4().hex[:8]`：读 /dev/urandom，避免引 uuid crate。
pub fn uuid4_hex8() -> String {
    uuid4_hex(4)[..8].to_string()
}

pub fn uuid4_hex32() -> String {
    uuid4_hex(16)
}

fn uuid4_hex(n: usize) -> String {
    let mut buf = vec![0u8; n];
    fill_random(&mut buf);
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

fn fill_random(buf: &mut [u8]) {
    use std::io::Read;
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        if f.read_exact(buf).is_ok() {
            return;
        }
    }
    // 兜底：时间熵（仅测试环境可达）
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    for (i, b) in buf.iter_mut().enumerate() {
        *b = ((t >> (i % 16 * 8)) & 0xff) as u8;
    }
}

/// Python `s[:n]`：按**字符**截断。
fn trunc_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    fn store() -> Store {
        let dir = std::env::temp_dir().join(format!(
            "tsvc_test_{}_{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Store::open(&dir.join("t.db").to_string_lossy()).unwrap()
    }

    fn mk(store: &Store, group: &str, title: &str) -> Value {
        store
            .create(
                group, title, None, None, "normal", None, None, "pending", None, None, None, None,
                None, None, None, None, None, 0,
            )
            .unwrap()
    }

    #[test]
    fn iso_roundtrip_cst() {
        // Python: datetime(2026,10,16,18,30,45,tzinfo=CST).isoformat(timespec="seconds")
        let s = iso_from_secs(1_792_148_245); // 对应时刻手工换算
        assert!(s.ends_with("+08:00"), "{s}");
        assert_eq!(s.len(), 25);
        // 回环解析：parse_iso(iso_from_secs(x)) == x
        let now = 1_792_148_245i64;
        assert_eq!(parse_iso(&iso_from_secs(now)), Some(now));
        // 已知锚点：2026-01-01T00:00:00+08:00 = 2025-12-31T16:00:00Z
        assert_eq!(parse_iso("2026-01-01T00:00:00+08:00"), Some(1_767_196_800));
        assert_eq!(iso_from_secs(1_767_196_800), "2026-01-01T00:00:00+08:00");
    }

    #[test]
    fn est_tokens_matches_python() {
        // CJK 1:1
        assert_eq!(est_tokens("你好世界"), 4);
        // 其他 4 字符 1 token
        assert_eq!(est_tokens("abcd"), 1);
        assert_eq!(est_tokens("abcde"), 2); // (5+3)/4
                                            // 混合
        assert_eq!(est_tokens("你好ab"), 3); // 2 + (2+3)/4=1
    }

    #[test]
    fn clip_digest_caps_at_max() {
        let long = "字".repeat(2000);
        let out = clip_digest(&long, 800);
        assert_eq!(out.chars().count(), 800); // 799 + …
        assert!(out.ends_with('…'));
        // 短文本原样
        assert_eq!(clip_digest("短文本", 800), "短文本");
    }

    #[test]
    fn reply_text_format_and_truncate() {
        let t = json!({"title": "写周报", "msg_seq": "#42"});
        let s = build_reply_text(&t, "hermes", Some("ok"), None);
        assert_eq!(s, "【任务完成】#42 写周报\n结果：已完成\n执行：hermes");
        // done/success/ok → 已完成；note 优先
        let s2 = build_reply_text(&t, "hermes", Some("done"), Some("已经交了"));
        assert!(s2.contains("结果：已经交了"));
        // 无标题
        let s3 = build_reply_text(&json!({}), "a", None, None);
        assert!(s3.starts_with("【任务完成】(无标题)"));
        // 超长按字符截到 3800（Python str 索引口径）
        let big = json!({"title": "长".repeat(4000)});
        let s4 = build_reply_text(&big, "a", None, None);
        assert_eq!(s4.chars().count(), 3800);
        assert!(s4.ends_with('…'));
    }

    #[test]
    fn create_list_next_report_flow() {
        let s = store();
        let t = mk(&s, "g1", "任务一");
        assert!(t
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap()
            .starts_with("T-"));
        assert_eq!(t.get("status").and_then(|v| v.as_str()), Some("pending"));
        // 队列：special 优先
        mk(&s, "g1", "普通");
        let _sp = s
            .create(
                "g1", "加急", None, None, "special", None, None, "pending", None, None, None, None,
                None, None, None, None, None, 0,
            )
            .unwrap();
        let nx = s.next_for("g1", "a1").unwrap();
        assert_eq!(nx.get("title").and_then(|v| v.as_str()), Some("加急"));
        assert_eq!(nx.get("status").and_then(|v| v.as_str()), Some("running"));
        // report 幂等
        let tid = nx.get("id").and_then(|v| v.as_str()).unwrap().to_string();
        let r1 = s.report(&tid, Some("a1"), "done", Some("好了"), None);
        assert_eq!(r1.get("already_reported"), Some(&json!(false)));
        let r2 = s.report(&tid, Some("a1"), "failed", None, None);
        assert_eq!(r2.get("already_reported"), Some(&json!(true)));
        assert_eq!(
            r2.get("first_report").and_then(|v| v.get("result")),
            Some(&json!("done"))
        );
        assert_eq!(
            s.get(&tid).unwrap().get("status").and_then(|v| v.as_str()),
            Some("done")
        );
    }

    #[test]
    fn ack_lease_and_freeze() {
        let s = store();
        let t = mk(&s, "g1", "活");
        let tid = t.get("id").and_then(|v| v.as_str()).unwrap().to_string();
        let wl = vec!["a1".to_string()];
        // 白名单外
        let r = s.ack(&tid, "a2", Some(120), 120, &wl);
        assert_eq!(r.get("error"), Some(&json!("agent_not_allowed")));
        // 正常 ack
        let r = s.ack(&tid, "a1", Some(120), 120, &wl);
        assert_eq!(r.get("ok"), Some(&json!(true)));
        let tok = r
            .get("lease_token")
            .and_then(|v| v.as_str())
            .unwrap()
            .to_string();
        // 重复 ack 同 token
        let r2 = s.ack(&tid, "a1", Some(120), 120, &wl);
        assert_eq!(r2.get("already_acked"), Some(&json!(true)));
        assert_eq!(
            r2.get("lease_token").and_then(|v| v.as_str()),
            Some(tok.as_str())
        );
        // 他人 ack → leased_by_other
        let wl3 = vec!["a1".to_string(), "a3".to_string()];
        let r3 = s.ack(&tid, "a3", Some(120), 120, &wl3);
        assert_eq!(r3.get("error"), Some(&json!("leased_by_other")));
        // report failed + chain 冻结
        let r = s.report(&tid, Some("a1"), "failed", None, Some("chain-x"));
        assert_eq!(r.get("chain_frozen"), Some(&json!(true)));
        assert!(!s.freeze_list().is_empty());
        // 同 chain 不再派发
        let _t2 = s
            .create(
                "g1",
                "后续",
                None,
                None,
                "normal",
                None,
                None,
                "pending",
                None,
                Some("chain-x"),
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                0,
            )
            .unwrap();
        assert!(s.next_for("g1", "a1").is_none());
        // 解冻后可派
        assert!(s.unfreeze_chain("chain-x"));
        assert!(s.next_for("g1", "a1").is_some());
    }

    #[test]
    fn reap_leases_requeues_then_fails() {
        let s = store();
        let t = mk(&s, "g1", "活");
        let tid = t.get("id").and_then(|v| v.as_str()).unwrap().to_string();
        let wl = vec!["a1".to_string()];
        for _ in 0..3 {
            s.ack(&tid, "a1", Some(120), 120, &wl);
            // 把 lease_expire_at 改成过去，模拟超时
            s.with(|c| {
                c.execute(
                    "UPDATE tasks SET lease_expire_at='2000-01-01T00:00:00+08:00' WHERE id=?1",
                    rusqlite::params![tid],
                )
            })
            .unwrap();
            let out = s.reap_leases(3);
            // 前 2 次 requeued（dispatch_count 1,2,3 里前两轮 <3）
            let action = out[0]
                .get("action")
                .and_then(|v| v.as_str())
                .unwrap()
                .to_string();
            if out[0]
                .get("dispatch_count")
                .and_then(|v| v.as_i64())
                .unwrap()
                >= 3
            {
                assert_eq!(action, "failed_and_frozen");
            } else {
                assert_eq!(action, "requeued");
            }
        }
        assert_eq!(
            s.get(&tid).unwrap().get("status").and_then(|v| v.as_str()),
            Some("failed")
        );
    }

    #[test]
    fn dispatch_reply_idempotent_and_no_autowrite() {
        let s = store();
        let t = mk(&s, "g1", "原任务");
        let tid = t.get("id").and_then(|v| v.as_str()).unwrap().to_string();
        let r = s.report(&tid, Some("a1"), "done", Some("结果良好"), None);
        let d = r.get("reply_dispatch").unwrap();
        assert_eq!(d.get("ok"), Some(&json!(true)));
        let rt = d.get("reply_task").unwrap();
        assert_eq!(rt.get("kind"), Some(&json!("reply")));
        assert_eq!(
            rt.get("idem_key").and_then(|v| v.as_str()),
            Some(format!("reply:{tid}").as_str())
        );
        assert!(rt.get("content").is_none() || rt.get("content").unwrap().is_null()); // 禁止代写
        assert_eq!(rt.get("lease_s"), Some(&json!(120)));
        // digest ≤800 token
        let dg = rt.get("context_digest").and_then(|v| v.as_str()).unwrap();
        assert!(est_tokens(dg) <= 800);
        // 再 report → already，reply_dispatch 不变
        let r2 = s.report(&tid, Some("a1"), "done", None, None);
        assert_eq!(r2.get("already_reported"), Some(&json!(true)));
        // reply 任务自身 report 不再派生
        let rid = rt.get("id").and_then(|v| v.as_str()).unwrap().to_string();
        let rr = s.report(&rid, Some("a1"), "done", None, None);
        assert_eq!(rr.get("autopost_disabled"), Some(&json!(true)));
    }

    #[test]
    fn patch_terminal_status_queues_reply_once() {
        let s = store();
        let t = mk(&s, "g1", "活");
        let tid = t.get("id").and_then(|v| v.as_str()).unwrap().to_string();
        let out = s
            .patch(&tid, &json!({"status": "done"}), false, &[], "", "", 20)
            .unwrap()
            .unwrap();
        // PATCH 路径补齐回帖（#418-1）
        assert!(out.get("reply").is_some());
        // 再 patch 到 failed → 不再重复（PK 幂等闸门：INSERT OR IGNORE）
        let _ = s
            .patch(&tid, &json!({"status": "failed"}), false, &[], "", "", 20)
            .unwrap();
        let pending = s.reply_pending(20, 100);
        assert_eq!(pending.len(), 1);
    }

    #[test]
    fn reply_deliver_against_stub() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let th = std::thread::spawn(move || {
            if let Ok((mut sock, _)) = listener.accept() {
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf);
                let body = br#"{"ok": true, "message": {"message_id": "m1", "seq": 7}}"#;
                let resp = format!(
                    "HTTP/1.0 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                    body.len(),
                    String::from_utf8_lossy(body)
                );
                let _ = sock.write_all(resp.as_bytes());
            }
        });
        let s = store();
        let t = mk(&s, "sess-1", "活");
        let tid = t.get("id").and_then(|v| v.as_str()).unwrap().to_string();
        s.report(&tid, Some("a1"), "done", None, None);
        // 注入 chat_base
        let d = s.reply_deliver(&tid, &format!("http://127.0.0.1:{port}"), "", 20);
        assert_eq!(d.get("ok"), Some(&json!(true)));
        assert_eq!(d.get("status"), Some(&json!("posted")));
        // 幂等：再投 already
        let d2 = s.reply_deliver(&tid, &format!("http://127.0.0.1:{port}"), "", 20);
        assert_eq!(d2.get("already"), Some(&json!(true)));
        th.join().unwrap();
        let _ = Duration::from_secs(0);
    }

    #[test]
    fn migrate_adds_columns_to_legacy_db() {
        let dir = std::env::temp_dir().join(format!(
            "tsvc_legacy_{}_{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("old.db");
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE tasks (id TEXT PRIMARY KEY, group_id TEXT NOT NULL, msg_seq TEXT,
                 proposer TEXT, created_at TEXT NOT NULL, title TEXT NOT NULL, content TEXT,
                 owner TEXT, priority TEXT NOT NULL DEFAULT 'normal',
                 status TEXT NOT NULL DEFAULT 'pending',
                 dispatch_count INTEGER NOT NULL DEFAULT 0, updated_at TEXT NOT NULL);",
            )
            .unwrap();
        }
        let s = Store::open(&path.to_string_lossy()).unwrap();
        let cols = s.table_columns();
        for c in ["chain_id", "lease_token", "idem_key", "hold"] {
            assert!(cols.iter().any(|h| h == c), "缺列 {c}");
        }
    }

    #[test]
    fn agent_allowed_semantics() {
        let wl: Vec<String> = vec!["hermes".into(), "openclaw-nb".into()];
        assert!(agent_allowed("hermes", &wl));
        assert!(agent_allowed(" hermes ", &wl)); // Python .strip() 口径
        assert!(!agent_allowed("other", &wl));
        assert!(agent_allowed("anyone", &[])); // 空=放行全部
    }
}
