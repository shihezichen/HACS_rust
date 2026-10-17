//! ChatStore：PostgreSQL 持久层（Python `ChatDB` 的移植）。
//!
//! 对齐点：
//! · `query()` 读完 rollback / `execute_tx()` 事务提交、只回最后一条语句的行 ——
//!   Rust 侧 postgres crate 自带事务语义：每条查询独立执行（autocommit），
//!   写路径用显式事务 `COMMIT/ROLLBACK`，粒度等价（PARITY 登记）。
//! · DBUnavailable → 503 `upstream_unavailable`（bin 层转）。
//! · DSN：`CHAT_PG_DSN` / `BB_PG_DSN` env，fallback `~/.chat_pg_dsn`。

use postgres::{Client, NoTls, Row};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use super::classify::{todo_dedupe_key, todo_task_name, TODO_INTENTS};
use super::iso_bj;
use super::{gen_message_id, gen_skip_id, gen_todo_id, memo_etag, memo_etag_ok, RequestError};

pub struct ChatStore {
    client: Mutex<Option<Client>>,
    last_failed: Mutex<Option<Instant>>,
    dsn: String,
}

pub const DB_RETRY_S: u64 = 5;

pub fn resolve_dsn() -> String {
    for name in ["CHAT_PG_DSN", "BB_PG_DSN"] {
        if let Ok(v) = std::env::var(name) {
            let v = v.trim().to_string();
            if !v.is_empty() {
                return v;
            }
        }
    }
    let path =
        std::path::Path::new(&std::env::var("HOME").unwrap_or_default()).join(".chat_pg_dsn");
    if let Ok(v) = std::fs::read_to_string(&path) {
        return v.trim().to_string();
    }
    String::new()
}

impl ChatStore {
    pub fn connect(dsn: &str) -> Result<Self, DbUnavailable> {
        let client = Client::connect(dsn, NoTls)
            .map_err(|e| DbUnavailable(format!("连接数据库失败：{}", scrub(&e.to_string()))))?;
        Ok(ChatStore {
            client: Mutex::new(Some(client)),
            last_failed: Mutex::new(None),
            dsn: dsn.to_string(),
        })
    }

    pub fn dsn(&self) -> &str {
        &self.dsn
    }

    fn with_client<T>(
        &self,
        f: impl FnOnce(&mut Client) -> Result<T, String>,
    ) -> Result<T, DbUnavailable> {
        // Python：失败后 DB_RETRY_S 内不重试
        {
            let lf = self.last_failed.lock().unwrap();
            if let Some(t) = *lf {
                if t.elapsed() < Duration::from_secs(DB_RETRY_S) {
                    return Err(DbUnavailable(format!(
                        "数据库最近连接失败，{}s 内不重试",
                        DB_RETRY_S
                    )));
                }
            }
        }
        let mut guard = self.client.lock().unwrap();
        if guard.is_none() {
            match Client::connect(&self.dsn, NoTls) {
                Ok(c) => *guard = Some(c),
                Err(e) => {
                    *self.last_failed.lock().unwrap() = Some(Instant::now());
                    return Err(DbUnavailable(format!(
                        "连接数据库失败：{}",
                        scrub(&e.to_string())
                    )));
                }
            }
        }
        let client = guard.as_mut().unwrap();
        match f(client) {
            Ok(v) => Ok(v),
            Err(e) => {
                // 连接级故障 → 丢弃重建
                *guard = None;
                *self.last_failed.lock().unwrap() = Some(Instant::now());
                Err(DbUnavailable(format!("数据库操作失败：{}", scrub(&e))))
            }
        }
    }

    /// 只读查询（postgres crate autocommit，无需显式 rollback）。
    pub fn query(
        &self,
        sql: &str,
        params: &[&(dyn postgres::types::ToSql + Sync)],
    ) -> Result<Vec<Row>, DbUnavailable> {
        self.with_client(|c| {
            let rows = c.query(sql, params).map_err(|e| {
                format!(
                    "{:?} :: {} :: {}",
                    e.as_db_error().map(|d| (d.code(), d.message())),
                    e,
                    e
                )
            })?;

            Ok(rows)
        })
    }

    /// 一个事务里按序执行多条，返回**最后一条**有 RETURNING 的行（对齐 `execute_tx`）。
    pub fn execute_tx(
        &self,
        statements: Vec<(&str, Vec<&(dyn postgres::types::ToSql + Sync)>)>,
    ) -> Result<Vec<Row>, DbUnavailable> {
        self.with_client(|c| {
            let mut tx = c.transaction().map_err(|e| e.to_string())?;
            let mut last: Vec<Row> = Vec::new();
            for (sql, params) in &statements {
                let rows = tx.query(*sql, params).map_err(|e| e.to_string())?;
                if tx.query_one("SELECT 1", &[]).is_ok() || true {
                    // placeholders always return rows for SELECT; keep last non-select result
                }
                if !rows.is_empty() || !sql.to_uppercase().contains("SELECT") {
                    last = rows;
                }
            }
            tx.commit().map_err(|e| e.to_string())?;
            Ok(last)
        })
    }

    // -- schema（幂等建表，启动跑一次；失败只告警不阻断）----------------
    pub fn ensure_schema(&self) -> Result<(), String> {
        let stmts: Vec<&str> = vec![
            // 核心表（Python 端由外部部署建；Rust 端幂等兜底建）
            "CREATE TABLE IF NOT EXISTS chat_sessions (session_id text PRIMARY KEY, kind text NOT NULL, title text NOT NULL DEFAULT '', sensitivity text NOT NULL DEFAULT 'internal', members jsonb NOT NULL DEFAULT '[]'::jsonb, created_by text NOT NULL DEFAULT '', created_at timestamptz NOT NULL DEFAULT now(), updated_at timestamptz NOT NULL DEFAULT now(), state text NOT NULL DEFAULT 'open', ref_table text, ref_id text, client_ref text, note text NOT NULL DEFAULT '')",
            "CREATE UNIQUE INDEX IF NOT EXISTS uq_chat_sessions_client_ref ON chat_sessions (created_by, client_ref) WHERE client_ref IS NOT NULL",
            "CREATE TABLE IF NOT EXISTS chat_messages (message_id text PRIMARY KEY, session_id text NOT NULL, seq bigint NOT NULL, author_type text NOT NULL, author_id text NOT NULL, kind text NOT NULL DEFAULT 'text', text text NOT NULL, status text NOT NULL DEFAULT 'done', reply_to text, meta jsonb NOT NULL DEFAULT '{}'::jsonb, created_at timestamptz NOT NULL DEFAULT now(), UNIQUE (session_id, seq))",
            "CREATE INDEX IF NOT EXISTS idx_chat_messages_session ON chat_messages (session_id, seq)",
            "CREATE TABLE IF NOT EXISTS chat_session_memo (session_id text PRIMARY KEY, doc jsonb NOT NULL DEFAULT '{}'::jsonb, version integer NOT NULL DEFAULT 0, etag text NOT NULL DEFAULT '', updated_at timestamptz NOT NULL DEFAULT now(), updated_by text NOT NULL DEFAULT '')",
            "CREATE TABLE IF NOT EXISTS chat_session_memo_events (event_id bigserial PRIMARY KEY, session_id text NOT NULL, version integer NOT NULL, action text NOT NULL, actor text NOT NULL, etag text NOT NULL, summary jsonb NOT NULL, created_at timestamptz NOT NULL DEFAULT now())",
            "CREATE TABLE IF NOT EXISTS chat_session_todos (todo_id text PRIMARY KEY, session_id text NOT NULL, text text NOT NULL, owner text NOT NULL, kind text NOT NULL DEFAULT 'confirm', status text NOT NULL DEFAULT 'open', deadline timestamptz, created_by text NOT NULL DEFAULT '', created_at timestamptz NOT NULL DEFAULT now(), updated_at timestamptz NOT NULL DEFAULT now(), cleared_at timestamptz, cleared_by text NOT NULL DEFAULT '', evidences jsonb NOT NULL DEFAULT '[]'::jsonb, landing jsonb NOT NULL DEFAULT '[]'::jsonb)",
            "ALTER TABLE chat_session_todos ADD COLUMN IF NOT EXISTS landing jsonb NOT NULL DEFAULT '[]'::jsonb",
            "ALTER TABLE chat_session_todos ADD COLUMN IF NOT EXISTS intent text NOT NULL DEFAULT 'task'",
            "ALTER TABLE chat_session_todos ADD COLUMN IF NOT EXISTS followup_count integer NOT NULL DEFAULT 0",
            "ALTER TABLE chat_session_todos ADD COLUMN IF NOT EXISTS task_name text NOT NULL DEFAULT ''",
            "ALTER TABLE chat_session_todos ADD COLUMN IF NOT EXISTS task_summary text NOT NULL DEFAULT ''",
            "ALTER TABLE chat_session_todos ADD COLUMN IF NOT EXISTS enrich_state text NOT NULL DEFAULT ''",
            "CREATE INDEX IF NOT EXISTS idx_chat_todos_session ON chat_session_todos (session_id, created_at)",
            "CREATE TABLE IF NOT EXISTS chat_todo_intent_skips (skip_id text PRIMARY KEY, session_id text NOT NULL, seq integer, message_id text NOT NULL DEFAULT '', author_id text NOT NULL DEFAULT '', owner text NOT NULL DEFAULT '', intent text NOT NULL DEFAULT '', reason text NOT NULL DEFAULT '', hits jsonb NOT NULL DEFAULT '[]'::jsonb, text text NOT NULL DEFAULT '', dedupe_key text NOT NULL, created_at timestamptz NOT NULL DEFAULT now(), UNIQUE (session_id, dedupe_key))",
            "CREATE INDEX IF NOT EXISTS idx_chat_todo_skips_session ON chat_todo_intent_skips (session_id, created_at)",
            "ALTER TABLE chat_sessions ADD COLUMN IF NOT EXISTS note text NOT NULL DEFAULT ''",
            "ALTER TABLE chat_sessions ADD COLUMN IF NOT EXISTS title text NOT NULL DEFAULT ''",
            "CREATE TABLE IF NOT EXISTS chat_session_meta_events (event_id bigserial PRIMARY KEY, session_id text NOT NULL, field text NOT NULL, old_value text NOT NULL, new_value text NOT NULL, actor text NOT NULL, created_at timestamptz NOT NULL DEFAULT now())",
            "CREATE TABLE IF NOT EXISTS chat_observers (session_id text NOT NULL, username text NOT NULL, joined_at timestamptz NOT NULL DEFAULT now(), left_at timestamptz, PRIMARY KEY (session_id, username))",
        ];
        self.with_client(|c| {
            for s in stmts {
                c.batch_execute(s).map_err(|e| e.to_string())?;
            }
            Ok(())
        })
        .map_err(|e: DbUnavailable| e.0)
    }

    // -- sessions -------------------------------------------------------
    #[allow(clippy::too_many_arguments)]
    pub fn create_session(
        &self,
        session_id: &str,
        kind: &str,
        title: &str,
        members: &[String],
        created_by: &str,
        sensitivity: &str,
        client_ref: Option<&str>,
    ) -> Result<Option<Row>, DbUnavailable> {
        let members_json = serde_json::json!(members);
        let rows = self.query(
            "INSERT INTO chat_sessions (session_id, kind, title, sensitivity, members, created_by, state, client_ref) VALUES ($1, $2, $3, $4, $5, $6, 'open', $7) ON CONFLICT DO NOTHING RETURNING session_id",
            &[&session_id, &kind, &title, &sensitivity, &members_json, &created_by, &client_ref],
        )?;
        if !rows.is_empty() {
            return self.get_session_row(session_id);
        }
        // 冲突但未插入：会话已存在（同 sid 或同 client_ref）→ 返回已有行
        {
            let exist = self.query(
                "SELECT session_id FROM chat_sessions WHERE session_id = $1 OR (client_ref IS NOT NULL AND client_ref = $2 AND created_by = $3)",
                &[&session_id, &client_ref.unwrap_or(""), &created_by],
            )?;
            if let Some(r) = exist.first() {
                let sid0: String = r.get(0);
                return self.get_session_row(&sid0);
            }
        }
        {
            let rows = if let Some(cr) = client_ref {
                self.query(
                    "SELECT session_id FROM chat_sessions WHERE created_by = $1 AND client_ref = $2",
                    &[&created_by, &cr],
                )?
            } else {
                self.query(
                    "SELECT session_id FROM chat_sessions WHERE session_id = $1",
                    &[&session_id],
                )?
            };
            let _ = &rows;
            if let Some(r) = rows.first() {
                let sid: String = r.get(0);
                return self.get_session_row(&sid);
            }
        }
        Ok(None)
    }

    pub fn get_session_row(&self, session_id: &str) -> Result<Option<Row>, DbUnavailable> {
        let rows = self.query(
            "SELECT session_id, kind, title, sensitivity, members, created_by, created_at, updated_at, state, ref_table, ref_id, note FROM chat_sessions WHERE session_id = $1",
            &[&session_id],
        )?;
        Ok(rows.into_iter().next())
    }

    /// 会话 → JSON（字段口径与 Python `_row_to_session` 一致 + member_ids 解析）。
    pub fn session_json(&self, row: &Row) -> Result<serde_json::Value, DbUnavailable> {
        let sid: String = row.get(0);
        let kind: String = row.get(1);
        let title: String = row.get::<_, Option<String>>(2).unwrap_or_default();
        let sensitivity: String = row.get(3);
        let members_val: serde_json::Value =
            row.get::<_, postgres::types::Json<serde_json::Value>>(4).0;
        let created_by: String = row.get(5);
        let created_at: Option<std::time::SystemTime> =
            row.get::<_, Option<std::time::SystemTime>>(6);
        let updated_at: Option<std::time::SystemTime> =
            row.get::<_, Option<std::time::SystemTime>>(7);
        let state: String = row.get(8);
        let ref_table: Option<String> = row.get::<_, Option<String>>(9);
        let ref_id: Option<String> = row.get::<_, Option<String>>(10);
        let note: String = row.get::<_, Option<String>>(11).unwrap_or_default();
        let member_ids: Vec<String> = members_val
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        Ok(serde_json::json!({
            "session_id": sid, "kind": kind, "title": title,
            "sensitivity": sensitivity,
            "members": member_ids, "member_ids": member_ids,
            "created_by": created_by,
            "created_at": created_at.map(|t| iso_epoch(&t)),
            "updated_at": updated_at.map(|t| iso_epoch(&t)),
            "state": state,
            "ref": ref_table.map(|t| serde_json::json!({"table": t, "id": ref_id})).unwrap_or(serde_json::json!(null)),
            "note": note,
        }))
    }

    pub fn list_sessions(
        &self,
        kind: Option<&str>,
        state: Option<&str>,
        member: Option<&str>,
        cursor: Option<&str>,
        limit: i64,
    ) -> Result<Vec<serde_json::Value>, DbUnavailable> {
        let rows = self.query(
            "SELECT session_id, kind, title, sensitivity, members, created_by, created_at, updated_at, state, ref_table, ref_id, note FROM chat_sessions WHERE 1=1 ORDER BY updated_at DESC",
            &[],
        )?;
        let mut out = Vec::new();
        for row in &rows {
            let k: String = row.get(1);
            if let Some(kf) = kind {
                if k != kf {
                    continue;
                }
            }
            let st: String = row.get(8);
            if let Some(sf) = state {
                if st != sf {
                    continue;
                }
            }
            let members_val: serde_json::Value =
                row.get::<_, postgres::types::Json<serde_json::Value>>(4).0;
            if let Some(mf) = member {
                let ids: Vec<String> = members_val
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                if !ids.iter().any(|m| m == mf) {
                    continue;
                }
            }
            let updated: Option<std::time::SystemTime> =
                row.get::<_, Option<std::time::SystemTime>>(7);
            let up_iso = updated.map(|t| iso_epoch(&t)).unwrap_or_default();
            if let Some(cf) = cursor {
                if up_iso.as_str() >= cf {
                    continue;
                }
            }
            out.push(self.session_json(row)?);
            if out.len() as i64 >= limit {
                break;
            }
        }
        Ok(out)
    }

    pub fn set_session_state(
        &self,
        session_id: &str,
        state: &str,
    ) -> Result<Option<Row>, DbUnavailable> {
        let rows = self.query(
            "UPDATE chat_sessions SET state = $1, updated_at = now() WHERE session_id = $2 RETURNING session_id, kind, title, sensitivity, members, created_by, created_at, updated_at, state, ref_table, ref_id, note",
            &[&state, &session_id],
        )?;
        Ok(rows.into_iter().next())
    }

    pub fn set_session_meta(
        &self,
        session_id: &str,
        title: Option<&str>,
        note: Option<&str>,
        state: Option<&str>,
        actor: &str,
    ) -> Result<Option<(Row, Vec<String>)>, DbUnavailable> {
        let old = match self.get_session_row(session_id)? {
            Some(r) => r,
            None => return Ok(None),
        };
        let old_title: String = old.get::<_, Option<String>>(2).unwrap_or_default();
        let old_note: String = old.get::<_, Option<String>>(11).unwrap_or_default();
        let mut events: Vec<(String, String, String)> = Vec::new();
        if let Some(t) = title {
            if old_title != t {
                events.push(("title".into(), old_title.clone(), t.to_string()));
            }
        }
        if let Some(n) = note {
            if old_note != n {
                events.push(("note".into(), old_note.clone(), n.to_string()));
            }
        }
        let rows = self.query(
            "UPDATE chat_sessions SET title = COALESCE($1, title), note = COALESCE($2, note), state = COALESCE($3, state), updated_at = now() WHERE session_id = $4 RETURNING session_id, kind, title, sensitivity, members, created_by, created_at, updated_at, state, ref_table, ref_id, note",
            &[&title, &note, &state, &session_id],
        )?;
        for (field, ov, nv) in &events {
            let _ = self.query(
                "INSERT INTO chat_session_meta_events (session_id, field, old_value, new_value, actor) VALUES ($1, $2, $3, $4, $5)",
                &[&session_id, field, ov, nv, &actor],
            );
        }
        Ok(rows
            .into_iter()
            .next()
            .map(|r| (r, events.iter().map(|e| e.0.clone()).collect())))
    }

    pub fn members_write(
        &self,
        session_id: &str,
        add: &[String],
        remove: &[String],
        actor: &str,
    ) -> Result<serde_json::Value, RequestError> {
        let old = match self.get_session_row(session_id).map_err(db_err)? {
            Some(r) => self.session_json(&r).map_err(db_err)?,
            None => return Err(RequestError::new(404, "not_found", "会话不存在")),
        };
        let old_ids: Vec<String> = old["member_ids"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let mut new_ids: Vec<String> = old_ids
            .iter()
            .filter(|m| !remove.contains(m))
            .cloned()
            .collect();
        for a in add {
            if !new_ids.contains(a) {
                new_ids.push(a.clone());
            }
        }
        let changed: Vec<String> = new_ids
            .iter()
            .filter(|m| !old_ids.contains(m))
            .cloned()
            .collect();
        let left: Vec<String> = old_ids
            .iter()
            .filter(|m| !new_ids.contains(m))
            .cloned()
            .collect();
        if changed.is_empty() && left.is_empty() {
            return Ok(
                serde_json::json!({"session": old, "added": [], "removed": [], "voided": 0, "cleared_todos": 0}),
            );
        }
        // 踢人结清其名下未结待办
        let mut cleared = 0i64;
        if !left.is_empty() {
            let _ = &left;
            cleared = self
                .query(
                    "SELECT count(*)::int FROM chat_session_todos WHERE session_id = $1 AND owner = ANY($2::text[]) AND status IN ('open','doing')",
                    &[&session_id, &left.to_vec()],
                )
                .map_err(db_err)?
                .iter()
                .map(|r| r.get::<_, i32>(0) as i64)
                .sum();
            let _ = self.query(
                "UPDATE chat_session_todos SET status = 'done', cleared_at = now(), cleared_by = $1, updated_at = now() WHERE session_id = $2 AND owner = ANY($3::text[]) AND status IN ('open','doing')",
                &[&actor, &session_id, &left.to_vec()],
            );
        }
        let new_val = serde_json::json!(new_ids);
        let old_val = serde_json::json!(old_ids);
        let rows = self
            .query(
                "UPDATE chat_sessions SET members = $1, updated_at = now() WHERE session_id = $2 AND members = $3 RETURNING session_id",
                &[&new_val, &session_id, &old_val],
            )
            .map_err(db_err)?;
        if rows.is_empty() {
            return Err(RequestError::new(
                409,
                "members_conflict",
                "成员表刚被其他人改过，请刷新后重试",
            ));
        }
        // voided 只读统计
        let voided: i64 = self
            .query(
                "SELECT count(*)::int FROM chat_messages m WHERE m.session_id = $1 AND m.meta->'needs_response' ?| $2::text[] AND m.seq > COALESCE((SELECT max(t.seq) FROM chat_messages t WHERE t.session_id = $1 AND t.author_id = ANY($3::text[])), 0)",
                &[&session_id, &left.to_vec(), &left.to_vec()],
            )
            .map_err(db_err)?
            .iter()
            .map(|r| r.get::<_, i32>(0) as i64)
            .sum();
        let updated = self.get_session_row(session_id).map_err(db_err)?.unwrap();
        let updated = self.session_json(&updated).map_err(db_err)?;
        Ok(serde_json::json!({
            "session": updated, "added": changed, "removed": left,
            "voided": voided, "cleared_todos": cleared,
        }))
    }

    // -- messages -------------------------------------------------------
    #[allow(clippy::too_many_arguments)]
    pub fn add_message(
        &self,
        session_id: &str,
        author_type: &str,
        author_id: &str,
        text: &str,
        mentions: &[String],
        audience: &[String],
        needs_response: &[String],
        reply_to: Option<&str>,
        client_msg_id: Option<&str>,
    ) -> Result<Option<Row>, DbUnavailable> {
        let meta = serde_json::json!({
            "mentions": mentions, "audience": audience,
            "needs_response": needs_response, "client_msg_id": client_msg_id,
        });
        let message_id = gen_message_id();
        let rows = self.query(
            "INSERT INTO chat_messages (message_id, session_id, seq, author_type, author_id, kind, text, status, reply_to, meta) SELECT $1, $2, COALESCE(MAX(seq), 0) + 1, $3, $4, 'text', $5, 'done', $6, $7 FROM chat_messages WHERE session_id = $8 RETURNING message_id, session_id, seq, author_type, author_id, kind, text, status, reply_to, meta, created_at",
            &[&message_id, &session_id, &author_type, &author_id, &text, &reply_to, &meta, &session_id],
        )?;
        let _ = self.query(
            "UPDATE chat_sessions SET updated_at = now() WHERE session_id = $1",
            &[&session_id],
        );
        Ok(rows.into_iter().next())
    }

    pub fn message_json(row: &Row) -> serde_json::Value {
        let created: Option<std::time::SystemTime> =
            row.get::<_, Option<std::time::SystemTime>>(10);
        let meta: serde_json::Value = row.get(9);
        serde_json::json!({
            "message_id": row.get::<_, String>(0),
            "session_id": row.get::<_, String>(1),
            "seq": row.get::<_, i64>(2),
            "author": {
                "type": row.get::<_, String>(3),
                "id": row.get::<_, String>(4),
            },
            "kind": row.get::<_, String>(5),
            "text": row.get::<_, String>(6),
            "status": row.get::<_, String>(7),
            "reply_to": row.get::<_, Option<String>>(8),
            "meta": meta,
            "created_at": created.map(|t| iso_epoch(&t)),
        })
    }

    pub fn list_messages(
        &self,
        session_id: &str,
        limit: i64,
        before_seq: Option<i32>,
        after_seq: Option<i32>,
    ) -> Result<(Vec<serde_json::Value>, bool, Option<i64>), DbUnavailable> {
        let rows = if let Some(b) = before_seq {
            self.query(
                "SELECT message_id, session_id, seq, author_type, author_id, kind, text, status, reply_to, meta, created_at FROM (SELECT * FROM chat_messages WHERE session_id = $1 AND seq < $2 ORDER BY seq DESC LIMIT $3) t ORDER BY seq ASC",
                &[&session_id, &b, &(limit + 1)],
            )?
        } else if let Some(a) = after_seq {
            self.query(
                "SELECT message_id, session_id, seq, author_type, author_id, kind, text, status, reply_to, meta, created_at FROM chat_messages WHERE session_id = $1 AND seq > $2 ORDER BY seq ASC LIMIT $3",
                &[&session_id, &a, &(limit + 1)],
            )?
        } else {
            self.query(
                "SELECT message_id, session_id, seq, author_type, author_id, kind, text, status, reply_to, meta, created_at FROM (SELECT * FROM chat_messages WHERE session_id = $1 ORDER BY seq DESC LIMIT $2) t ORDER BY seq ASC",
                &[&session_id, &(limit + 1)],
            )?
        };
        let has_more = rows.len() as i64 > limit;
        let mut rows = rows;
        if has_more {
            if after_seq.is_some() {
                rows.truncate(limit as usize);
            } else {
                rows = rows.split_off(rows.len() - limit as usize);
            }
        }
        let messages: Vec<serde_json::Value> = rows.iter().map(Self::message_json).collect();
        let next_before = if has_more && !messages.is_empty() {
            messages[0]["seq"].as_i64()
        } else {
            None
        };
        Ok((messages, has_more, next_before))
    }

    pub fn last_message(&self, session_id: &str) -> Result<Option<Row>, DbUnavailable> {
        let rows = self.query(
            "SELECT message_id, session_id, seq, author_type, author_id, kind, text, status, reply_to, meta, created_at FROM chat_messages WHERE session_id = $1 ORDER BY seq DESC LIMIT 1",
            &[&session_id],
        )?;
        Ok(rows.into_iter().next())
    }

    // -- memo -----------------------------------------------------------
    pub fn memo_get(&self, session_id: &str) -> Result<serde_json::Value, DbUnavailable> {
        let rows = self.query(
            "SELECT doc, version, etag, updated_at, updated_by FROM chat_session_memo WHERE session_id = $1",
            &[&session_id],
        )?;
        if rows.is_empty() {
            return Ok(
                serde_json::json!({"memo": {}, "version": 0, "etag": "", "updated_at": null, "updated_by": ""}),
            );
        }
        let r = &rows[0];
        let doc: serde_json::Value = r.get(0);
        let updated: Option<std::time::SystemTime> = r.get::<_, Option<std::time::SystemTime>>(3);
        Ok(serde_json::json!({
            "memo": doc,
            "version": r.get::<_, i32>(1),
            "etag": r.get::<_, String>(2),
            "updated_at": updated.map(|t| iso_epoch(&t)),
            "updated_by": r.get::<_, String>(4),
        }))
    }

    /// `memo_write`：乐观锁 UPSERT + 事件同事务。
    pub fn memo_write(
        &self,
        session_id: &str,
        doc: &serde_json::Value,
        actor: &str,
        if_match: &str,
        action: &str,
    ) -> Result<(bool, serde_json::Value), DbUnavailable> {
        let current = self.memo_get(session_id)?;
        let cur_version = current["version"].as_i64().unwrap_or(0) as i32;
        let cur_etag = current["etag"].as_str().unwrap_or("");
        if !memo_etag_ok(cur_version as i64, cur_etag, if_match) {
            return Ok((false, current));
        }
        let new_version = cur_version + 1;
        let etag = memo_etag(session_id, new_version as i64, doc);
        let doc_val = doc.clone();
        let summary_val = serde_json::json!({
            "action": action,
            "counts": {
                "decisions": doc.get("decisions").and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0),
                "open_tasks": doc.get("open_tasks").and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0),
                "conventions": doc.get("conventions").and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0),
            },
            "goal": doc.get("goal").and_then(|v| v.as_str()).map(|s| s.chars().take(80).collect::<String>()).unwrap_or_default(),
        });
        let rows = self.execute_tx(vec![
            (
                "INSERT INTO chat_session_memo (session_id, doc, version, etag, updated_by, updated_at) VALUES ($1, $2, $3, $4, $5, now()) ON CONFLICT (session_id) DO UPDATE SET doc = EXCLUDED.doc, version = EXCLUDED.version, etag = EXCLUDED.etag, updated_by = EXCLUDED.updated_by, updated_at = now() WHERE chat_session_memo.version = $6 RETURNING version",
                vec![&session_id as &(dyn postgres::types::ToSql + Sync), &doc_val, &new_version, &etag, &actor, &cur_version],
            ),
            (
                "INSERT INTO chat_session_memo_events (session_id, version, action, actor, etag, summary) SELECT $1, $2, $3, $4, $5, $6 WHERE EXISTS (SELECT 1 FROM chat_session_memo WHERE session_id = $1 AND version = $2 AND etag = $5) RETURNING version, etag, created_at",
                vec![&session_id as &(dyn postgres::types::ToSql + Sync), &new_version, &action, &actor, &etag, &summary_val],
            ),
        ])?;
        if rows.is_empty() {
            return Ok((false, self.memo_get(session_id)?));
        }
        let created: Option<std::time::SystemTime> =
            rows[0].get::<_, Option<std::time::SystemTime>>(2);
        Ok((
            true,
            serde_json::json!({
                "memo": doc, "version": new_version, "etag": etag,
                "updated_at": created.map(|t| iso_epoch(&t)),
                "updated_by": actor,
            }),
        ))
    }

    // -- todos ----------------------------------------------------------
    const TODO_COLS: &'static str = "todo_id, session_id, text, owner, kind, status, deadline, created_by, created_at, updated_at, cleared_at, cleared_by, evidences, landing, intent, followup_count, task_name, task_summary, enrich_state";

    pub fn todo_row_json(&self, row: &Row) -> serde_json::Value {
        let deadline: Option<std::time::SystemTime> =
            row.get::<_, Option<std::time::SystemTime>>(6);
        let created: Option<std::time::SystemTime> = row.get::<_, Option<std::time::SystemTime>>(8);
        let updated: Option<std::time::SystemTime> = row.get::<_, Option<std::time::SystemTime>>(9);
        let cleared: Option<std::time::SystemTime> =
            row.get::<_, Option<std::time::SystemTime>>(10);
        let evid_val: serde_json::Value = row.get(12);
        let land_val: serde_json::Value = row.get(13);
        let owner: String = row.get(3);
        let text: String = row.get(2);
        let task_name_db: String = row.get::<_, Option<String>>(16).unwrap_or_default();
        serde_json::json!({
            "id": row.get::<_, String>(0),
            "session_id": row.get::<_, String>(1),
            "text": text,
            "owner": owner,
            "owner_display": owner, // display_of → L2 名册由 bin 层覆盖
            "kind": row.get::<_, String>(4),
            "status": row.get::<_, String>(5),
            "deadline": deadline.map(|t| iso_epoch(&t)),
            "created_by": row.get::<_, Option<String>>(7).unwrap_or_default(),
            "created_at": created.map(|t| iso_epoch(&t)),
            "updated_at": updated.map(|t| iso_epoch(&t)),
            "cleared_at": cleared.map(|t| iso_epoch(&t)),
            "cleared_by": row.get::<_, Option<String>>(11).unwrap_or_default(),
            "evidences": evid_val.as_array().cloned().unwrap_or_default(),
            "landing": land_val.as_array().cloned().unwrap_or_default(),
            "intent": row.get::<_, Option<String>>(14).unwrap_or_else(|| "task".into()),
            "followup_count": row.get::<_, i32>(15) as i64,
            "task_name": if task_name_db.is_empty() { todo_task_name(&text) } else { task_name_db },
            "task_summary": row.get::<_, Option<String>>(17).unwrap_or_default(),
            "enrich_state": row.get::<_, Option<String>>(18).unwrap_or_default(),
        })
    }

    pub fn todos_list(
        &self,
        session_id: &str,
        owner: Option<&str>,
        status: Option<&str>,
    ) -> Result<Vec<serde_json::Value>, DbUnavailable> {
        let sql = format!(
            "SELECT {} FROM chat_session_todos WHERE session_id = $1 ORDER BY created_at ASC, todo_id ASC",
            Self::TODO_COLS
        );
        let rows = self.query(&sql, &[&session_id])?;
        Ok(rows
            .iter()
            .map(|r| self.todo_row_json(r))
            .filter(|t| {
                owner
                    .map(|o| t["owner"] == serde_json::json!(o))
                    .unwrap_or(true)
            })
            .filter(|t| {
                status
                    .map(|s| t["status"] == serde_json::json!(s))
                    .unwrap_or(true)
            })
            .collect())
    }

    pub fn todo_get(&self, todo_id: &str) -> Result<Option<serde_json::Value>, DbUnavailable> {
        let sql = format!(
            "SELECT {} FROM chat_session_todos WHERE todo_id = $1",
            Self::TODO_COLS
        );
        let rows = self.query(&sql, &[&todo_id])?;
        Ok(rows.first().map(|r| self.todo_row_json(r)))
    }

    pub fn todos_rows(
        &self,
        session_id: &str,
        ids: &[String],
    ) -> Result<Vec<serde_json::Value>, DbUnavailable> {
        if ids.is_empty() {
            return Ok(vec![]);
        }
        let sql = format!(
            "SELECT {} FROM chat_session_todos WHERE session_id = $1 AND todo_id = ANY($2::text[])",
            Self::TODO_COLS
        );
        let rows = self.query(&sql, &[&session_id, &ids.to_vec()])?;
        Ok(rows.iter().map(|r| self.todo_row_json(r)).collect())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn todo_create(
        &self,
        session_id: &str,
        text: &str,
        owner: &str,
        kind: &str,
        deadline: Option<std::time::SystemTime>,
        actor: &str,
        landing: &[String],
        intent: &str,
        task_name: &str,
        enrich_state: &str,
    ) -> Result<Option<serde_json::Value>, DbUnavailable> {
        let name = if task_name.is_empty() {
            todo_task_name(text)
        } else {
            task_name.to_string()
        };
        let intent = if TODO_INTENTS.contains(&intent) {
            intent
        } else {
            "task"
        };
        let landing_val = serde_json::json!(landing);
        let name_cut: String = name.chars().take(super::TODO_TEXT_MAX).collect();
        let state_cut = enrich_state.chars().take(32).collect::<String>();
        let sql = format!(
            "INSERT INTO chat_session_todos (todo_id, session_id, text, owner, kind, status, deadline, created_by, landing, intent, task_name, enrich_state) VALUES ($1, $2, $3, $4, $5, 'open', $6, $7, $8, $9, $10, $11) RETURNING {}",
            Self::TODO_COLS
        );
        let todo_id = gen_todo_id();
        let rows = self.query(
            &sql,
            &[
                &todo_id,
                &session_id,
                &text,
                &owner,
                &kind,
                &deadline,
                &actor,
                &landing_val,
                &intent,
                &name_cut,
                &state_cut,
            ],
        )?;
        Ok(rows.first().map(|r| self.todo_row_json(r)))
    }

    pub fn todo_mark_done(
        &self,
        todo_id: &str,
        actor: &str,
        evidences: &[serde_json::Value],
        landing: &[String],
    ) -> Result<Option<serde_json::Value>, DbUnavailable> {
        let ev_json = serde_json::json!(evidences);
        let land_json = serde_json::json!(landing);
        let sql = format!(
            "UPDATE chat_session_todos SET status = 'done', cleared_at = now(), cleared_by = $1, updated_at = now(), evidences = COALESCE(evidences, '[]'::jsonb) || $2, landing = COALESCE(landing, '[]'::jsonb) || $3 WHERE todo_id = $4 RETURNING {}",
            Self::TODO_COLS
        );
        let rows = self.query(&sql, &[&actor, &ev_json, &land_json, &todo_id])?;
        Ok(rows.first().map(|r| self.todo_row_json(r)))
    }

    pub fn todo_mark_doing(
        &self,
        todo_id: &str,
    ) -> Result<Option<serde_json::Value>, DbUnavailable> {
        let sql = format!(
            "UPDATE chat_session_todos SET status = 'doing', updated_at = now(), cleared_at = NULL, cleared_by = '' WHERE todo_id = $1 RETURNING {}",
            Self::TODO_COLS
        );
        let rows = self.query(&sql, &[&todo_id])?;
        Ok(rows.first().map(|r| self.todo_row_json(r)))
    }

    pub fn todo_reopen(&self, todo_id: &str) -> Result<Option<serde_json::Value>, DbUnavailable> {
        let sql = format!(
            "UPDATE chat_session_todos SET status = 'open', cleared_at = NULL, cleared_by = '', evidences = '[]'::jsonb, landing = '[]'::jsonb, updated_at = now() WHERE todo_id = $1 RETURNING {}",
            Self::TODO_COLS
        );
        let rows = self.query(&sql, &[&todo_id])?;
        Ok(rows.first().map(|r| self.todo_row_json(r)))
    }

    pub fn todos_purge(&self, session_id: &str) -> Result<Vec<String>, DbUnavailable> {
        let rows = self.query(
            "SELECT todo_id FROM chat_session_todos WHERE session_id = $1 ORDER BY created_at ASC, todo_id ASC",
            &[&session_id],
        )?;
        let ids: Vec<String> = rows.iter().map(|r| r.get(0)).collect();
        if !ids.is_empty() {
            self.query(
                "DELETE FROM chat_session_todos WHERE session_id = $1 AND todo_id = ANY($2::text[])",
                &[&session_id, &ids.to_vec()],
            )?;
        }
        Ok(ids)
    }

    pub fn todos_clear(
        &self,
        session_id: &str,
        ids: &[String],
        actor: &str,
        landing: &[String],
        evidences: &[serde_json::Value],
    ) -> Result<(Vec<String>, Vec<serde_json::Value>), DbUnavailable> {
        let existing = self.todos_rows(session_id, ids)?;
        let mut cleared = Vec::new();
        let mut skipped = Vec::new();
        for t in &existing {
            let id = t["id"].as_str().unwrap_or("");
            if t["owner"] != serde_json::json!(actor) {
                skipped.push(serde_json::json!({"id": id, "why": "not_yours"}));
            } else if t["status"] != serde_json::json!("open") {
                skipped.push(serde_json::json!({"id": id, "why": "already_done"}));
            } else {
                cleared.push(id.to_string());
            }
        }
        if !cleared.is_empty() {
            let ev_json = serde_json::json!(evidences);
            let land_json = serde_json::json!(landing);
            self.query(
                "UPDATE chat_session_todos SET status = 'done', cleared_at = now(), cleared_by = $1, updated_at = now(), evidences = COALESCE(evidences, '[]'::jsonb) || $2, landing = COALESCE(landing, '[]'::jsonb) || $3 WHERE session_id = $4 AND todo_id = ANY($5::text[]) AND owner = $6 AND status = 'open'",
                &[&actor, &ev_json, &land_json, &session_id, &ids.to_vec(), &actor],
            )?;
        }
        let known: Vec<&str> = existing.iter().filter_map(|t| t["id"].as_str()).collect();
        for id in ids {
            if !known.contains(&id.as_str()) {
                skipped.push(serde_json::json!({"id": id, "why": "not_found"}));
            }
        }
        Ok((cleared, skipped))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn todo_skip_create(
        &self,
        session_id: &str,
        seq: Option<i32>,
        message_id: &str,
        author_id: &str,
        owner: &str,
        intent: &str,
        reason: &str,
        hits: &[String],
        text: &str,
    ) -> Result<Option<String>, DbUnavailable> {
        let key = todo_dedupe_key(text);
        if key.is_empty() {
            return Ok(None);
        }
        let skip_id = gen_skip_id();
        let text_cut: String = text.chars().take(super::TODO_TEXT_MAX).collect();
        let hits_val = serde_json::json!(hits);
        let rows = self.query(
            "INSERT INTO chat_todo_intent_skips (skip_id, session_id, seq, message_id, author_id, owner, intent, reason, hits, text, dedupe_key) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11) ON CONFLICT (session_id, dedupe_key) DO NOTHING RETURNING skip_id",
            &[&skip_id, &session_id, &seq, &message_id, &author_id, &owner, &intent, &reason, &hits_val, &text_cut, &key],
        )?;
        Ok(rows.into_iter().next().map(|r| r.get(0)))
    }

    pub fn todo_bump_followup(
        &self,
        todo_id: &str,
    ) -> Result<Option<serde_json::Value>, DbUnavailable> {
        let sql = format!(
            "UPDATE chat_session_todos SET followup_count = followup_count + 1, updated_at = now() WHERE todo_id = $1 RETURNING {}",
            Self::TODO_COLS
        );
        let rows = self.query(&sql, &[&todo_id])?;
        Ok(rows.first().map(|r| self.todo_row_json(r)))
    }

    pub fn todos_skips_list(
        &self,
        session_id: &str,
        limit: i64,
    ) -> Result<Vec<serde_json::Value>, DbUnavailable> {
        let rows = self.query(
            "SELECT skip_id, session_id, seq, message_id, author_id, owner, intent, reason, hits, text, created_at FROM chat_todo_intent_skips WHERE session_id = $1 ORDER BY created_at DESC, skip_id DESC LIMIT $2",
            &[&session_id, &limit],
        )?;
        Ok(rows
            .iter()
            .map(|r| {
                let created: Option<std::time::SystemTime> =
                    r.get::<_, Option<std::time::SystemTime>>(10);
                let hits_val: serde_json::Value =
                    r.get::<_, postgres::types::Json<serde_json::Value>>(8).0;
                let owner: String = r.get(5);
                serde_json::json!({
                    "id": r.get::<_, String>(0),
                    "session_id": r.get::<_, String>(1),
                    "seq": r.get::<_, Option<i32>>(2).map(|v| v as i64),
                    "message_id": r.get::<_, String>(3),
                    "author_id": r.get::<_, String>(4),
                    "owner": owner.clone(),
                    "owner_display": owner,
                    "intent": r.get::<_, String>(6),
                    "reason": r.get::<_, String>(7),
                    "hits": hits_val.as_array().cloned().unwrap_or_default(),
                    "text": r.get::<_, String>(9),
                    "created_at": created.map(|t| iso_epoch(&t)),
                })
            })
            .collect())
    }

    pub fn healthz_probe(&self) -> bool {
        self.query("SELECT 1", &[]).is_ok()
    }
}

/// 数据库不可用（对外统一 503）。
#[derive(Debug, Clone)]
pub struct DbUnavailable(pub String);

impl std::fmt::Display for DbUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

fn db_err(e: DbUnavailable) -> RequestError {
    RequestError::new(503, "upstream_unavailable", "数据库暂不可用")
        .with_extra("_dbg", serde_json::json!(e.0))
}

/// 错误文本脱敏（Python `scrubbed`/`view_auth.safe_err`）：口令类字段抹掉 + 截断。
pub fn scrub(msg: &str) -> String {
    let mut out = String::new();
    for tok in msg.split_whitespace() {
        let low = tok.to_lowercase();
        if low.contains("password") || low.contains("token") || low.contains("secret") {
            out.push_str("[REDACTED]");
        } else {
            out.push_str(tok);
        }
        out.push(' ');
    }
    let out = out.trim().to_string();
    out.chars().take(300).collect()
}

fn iso_epoch(t: &std::time::SystemTime) -> String {
    let secs = t
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    iso_bj(secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_dsn() -> String {
        std::env::var("CHAT_TEST_PG_DSN").unwrap_or_default()
    }

    #[test]
    fn test_scrub() {
        assert!(scrub("password=abc123 failed").contains("[REDACTED]"));
        let long = "x".repeat(400);
        assert!(scrub(&long).chars().count() <= 300);
    }

    #[test]
    fn test_store_e2e() {
        let dsn = test_dsn();
        if dsn.is_empty() {
            eprintln!("skip: CHAT_TEST_PG_DSN 未设");
            return;
        }
        let store = ChatStore::connect(&dsn).unwrap();
        store.ensure_schema().unwrap();
        // 建群
        let sid = format!("dm_test_{}", std::process::id());
        let members = vec!["hermes".to_string(), "boss".to_string()];
        let row = store
            .create_session(&sid, "dm", "测试群", &members, "boss", "internal", None)
            .unwrap()
            .unwrap();
        let session = store.session_json(&row).unwrap();
        assert_eq!(session["state"], serde_json::json!("open"));
        assert_eq!(session["member_ids"].as_array().unwrap().len(), 2);
        // 幂等（client_ref）
        let row2 = store
            .create_session(
                &sid,
                "dm",
                "测试群",
                &members,
                "boss",
                "internal",
                Some("cr1"),
            )
            .unwrap();
        assert!(row2.is_some());
        // 发消息
        let m = store
            .add_message(
                &sid,
                "human",
                "boss",
                "你好",
                &["@all".to_string()],
                &members,
                &["hermes".to_string()],
                None,
                None,
            )
            .unwrap()
            .unwrap();
        assert_eq!(m.get::<_, i64>(2), 1);
        let m2 = store
            .add_message(
                &sid,
                "agent",
                "hermes",
                "收到",
                &["@all".to_string()],
                &members,
                &["boss".to_string()],
                None,
                None,
            )
            .unwrap()
            .unwrap();
        assert_eq!(m2.get::<_, i64>(2), 2);
        let (msgs, has_more, _) = store.list_messages(&sid, 20, None, None).unwrap();
        assert_eq!(msgs.len(), 2);
        assert!(!has_more);
        assert_eq!(msgs[0]["seq"], serde_json::json!(1));
        // memo
        let doc = serde_json::json!({"goal": "修网关", "resume": "", "decisions": [], "open_tasks": [], "conventions": []});
        let (ok, st) = store.memo_write(&sid, &doc, "boss", "", "put").unwrap();
        assert!(ok);
        assert_eq!(st["version"], serde_json::json!(1));
        // 过期 etag 不放行；当前 etag 重写正常 +1
        let (ok2b, _) = store
            .memo_write(&sid, &doc, "boss", st["etag"].as_str().unwrap(), "put")
            .unwrap();
        assert!(ok2b);
        let (ok2, cur) = store
            .memo_write(
                &sid,
                &doc,
                "boss",
                "W/\"00000000000000000000000000000000\"",
                "put",
            )
            .unwrap();
        assert!(!ok2);
        assert_eq!(cur["version"], serde_json::json!(2));
        // todo
        let t = store
            .todo_create(
                &sid,
                "#1 修复登录页",
                "hermes",
                "confirm",
                None,
                "boss",
                &[],
                "task",
                "",
                "",
            )
            .unwrap()
            .unwrap();
        assert_eq!(t["status"], serde_json::json!("open"));
        assert!(!t["task_name"].as_str().unwrap().is_empty());
        // 幂等清账
        let ids = vec![t["id"].as_str().unwrap().to_string()];
        let (cleared, skipped) = store
            .todos_clear(
                &sid,
                &ids,
                "hermes",
                vec!["abc1234".to_string()].as_slice(),
                &[],
            )
            .unwrap();
        assert_eq!(cleared.len(), 1);
        let (_, skipped2) = store.todos_clear(&sid, &ids, "hermes", &[], &[]).unwrap();
        assert_eq!(skipped2[0]["why"], serde_json::json!("already_done"));
        let _ = skipped;
        // skip 台账幂等
        let k1 = store
            .todo_skip_create(
                &sid,
                Some(1),
                "m1",
                "boss",
                "hermes",
                "followup",
                "followup_phrase",
                &["进度".to_string()],
                "进度如何",
            )
            .unwrap();
        let k2 = store
            .todo_skip_create(
                &sid,
                Some(1),
                "m1",
                "boss",
                "hermes",
                "followup",
                "followup_phrase",
                &["进度".to_string()],
                "进度如何",
            )
            .unwrap();
        assert!(k1.is_some() && k2.is_none());
        // purge
        let removed = store.todos_purge(&sid).unwrap();
        assert_eq!(removed.len(), 1);
        // 踢人
        let out = store
            .members_write(&sid, &[], &["hermes".to_string()], "boss")
            .unwrap();
        assert_eq!(out["removed"].as_array().unwrap().len(), 1);
    }
}
