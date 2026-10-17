//! `blackboard_view` —— 团队黑板·只读看板（对应 Python
//! `hub/l3_domain/blackboard_view.py` 的非 HTTP 部分 + `hub/x/view_auth.py`
//! 的 `PgStore` / `Auth` 门面）。
//!
//! 分层（对应 HANDOFF 偏差 12 的规划）：
//! * **X 层**（`hacs-x-identity::view_auth`）：纯逻辑——sha256/HMAC/scrypt 自实现、
//!   token 签名、CSRF、角色密级、LockTracker。本文件只调用它，不重复实现。
//! * **L3 本文件**：`ViewStore`（PG 四张 view_* 表）+ `Auth` 门面 + 遮罩
//!   （`mask_shared` / `apply_role`）。HTTP 装配在 bin（`hacs-c/src/bin/blackboard_view.rs`）。
//!
//! 口径与 Python 严格对齐：
//! * 口令只存 scrypt 哈希；会话只存 sha256 指纹；审计只增不改；
//! * 登录失败提示**一律同一句**（不给用户名枚举器）；
//! * 锁定 = 同 IP 5 分钟内 5 败 → 锁 15 分钟（内存 + 表双记，取较晚）；
//! * 角色三级 viewer/member/owner；密级 public/internal/sensitive；
//!   viewer 只看 public，member 到 internal，owner 全量。

use hacs_x_identity::view_auth as va;
use postgres::types::ToSql;
use serde_json::{json, Map, Value};
use std::sync::{Arc, Mutex};

pub const VERSION: &str = "1.5";

/// 角色密级表（Python `SENSITIVITY_RANK` 的键）。
pub const SENSITIVITY_LEVELS: [&str; 3] = ["public", "internal", "sensitive"];

// ================================================================ 存储层

pub struct ViewStore {
    dsn: String,
    lock: Mutex<()>,
    conn: Mutex<Option<postgres::Client>>,
}

/// 与 `blackboard_svc::DbUnavailable` 同形：数据库层面的失败（已脱敏）。
pub struct DbUnavailable(pub String);

impl std::fmt::Display for DbUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

fn scrub(text: &str) -> String {
    hacs_x_identity::redact::safe_err(text)
}

impl ViewStore {
    pub fn connect(dsn: &str) -> Result<Self, DbUnavailable> {
        if dsn.is_empty() {
            return Err(DbUnavailable(
                "DSN 为空：检查 --dsn-file 指向的文件里有没有 BB_PG_DSN=".into(),
            ));
        }
        let conn = postgres::Client::connect(dsn, postgres::NoTls)
            .map_err(|e| DbUnavailable(scrub(&format!("{e}"))))?;
        Ok(ViewStore {
            dsn: dsn.to_string(),
            lock: Mutex::new(()),
            conn: Mutex::new(Some(conn)),
        })
    }

    fn is_conn_closed(err: &postgres::Error) -> bool {
        err.code().is_none() // 非服务端错误 = 连接坏了（与 blackboard_svc 同口径）
    }

    /// 统一查询入口：autocommit、断线重连一次（Python `PgStore._exec`）。
    fn exec(
        &self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
        fetch_one: bool,
    ) -> Result<Vec<postgres::Row>, DbUnavailable> {
        let _g = self.lock.lock().unwrap();
        let mut guard = self.conn.lock().unwrap();
        let mut last: Option<String> = None;
        for attempt in 0..2 {
            let fresh = guard.is_none();
            if fresh {
                match postgres::Client::connect(&self.dsn, postgres::NoTls) {
                    Ok(c) => *guard = Some(c),
                    Err(e) => {
                        last = Some(format!("{e}"));
                        break;
                    }
                }
            }
            let conn = guard.as_mut().unwrap();
            match conn.query(sql, params) {
                Ok(rows) if fetch_one => return Ok(rows.into_iter().take(1).collect()),
                Ok(rows) => return Ok(rows),
                Err(e) => {
                    last = Some(format!("{e}"));
                    *guard = None;
                    if fresh || attempt == 1 || !Self::is_conn_closed(&e) {
                        break;
                    }
                }
            }
        }
        Err(DbUnavailable(scrub(&last.unwrap_or_default())))
    }

    // --- 账号 ---------------------------------------------------------

    pub fn get_user(&self, username: &str) -> Result<Option<UserRow>, DbUnavailable> {
        let rows = self.exec(
            "SELECT username, role, pwd_hash, disabled, note, created_at, created_by, \
             updated_at, pwd_changed_at, last_login_at \
             FROM view_users WHERE username = $1",
            &[&username],
            true,
        )?;
        Ok(rows.first().map(user_from_row))
    }

    pub fn list_users(&self) -> Result<Vec<UserRow>, DbUnavailable> {
        let rows = self.exec(
            "SELECT username, role, pwd_hash, disabled, note, created_at, created_by, \
             updated_at, pwd_changed_at, last_login_at \
             FROM view_users ORDER BY username",
            &[],
            false,
        )?;
        Ok(rows.iter().map(user_from_row).collect())
    }

    pub fn add_user(
        &self,
        username: &str,
        role: &str,
        pwd_hash: &str,
        created_by: Option<&str>,
        note: Option<&str>,
    ) -> Result<u64, DbUnavailable> {
        Ok(self
            .exec(
                "INSERT INTO view_users (username, role, pwd_hash, note, created_by) \
                 VALUES ($1, $2, $3, $4, $5)",
                &[&username, &role, &pwd_hash, &note, &created_by],
                false,
            )?
            .len() as u64)
    }

    pub fn set_password(&self, username: &str, pwd_hash: &str) -> Result<(), DbUnavailable> {
        self.exec(
            "UPDATE view_users SET pwd_hash = $1, pwd_changed_at = now() WHERE username = $2",
            &[&pwd_hash, &username],
            false,
        )?;
        Ok(())
    }

    pub fn set_disabled(&self, username: &str, disabled: bool) -> Result<(), DbUnavailable> {
        self.exec(
            "UPDATE view_users SET disabled = $1 WHERE username = $2",
            &[&disabled, &username],
            false,
        )?;
        Ok(())
    }

    pub fn touch_login(&self, username: &str, ts_iso: &str) -> Result<(), DbUnavailable> {
        self.exec(
            "UPDATE view_users SET last_login_at = $1::text::timestamptz WHERE username = $2",
            &[&ts_iso, &username],
            false,
        )?;
        Ok(())
    }

    // --- 会话 ---------------------------------------------------------

    pub fn create_session(
        &self,
        token_hash: &str,
        username: &str,
        role: &str,
        created_iso: &str,
        expires_iso: &str,
        ip: &str,
        ua: &str,
    ) -> Result<(), DbUnavailable> {
        self.exec(
            "INSERT INTO view_sessions (token_hash, username, role, created_at, \
             expires_at, last_seen, ip, ua) VALUES ($1,$2,$3,$4::text::timestamptz,\
             $5::text::timestamptz,$4::text::timestamptz,$6,$7)",
            &[
                &token_hash,
                &username,
                &role,
                &created_iso,
                &expires_iso,
                &ip,
                &ua,
            ],
            false,
        )?;
        Ok(())
    }

    pub fn get_session(&self, token_hash: &str) -> Result<Option<SessionRow>, DbUnavailable> {
        let rows = self.exec(
            "SELECT token_hash, username, role, created_at, expires_at, last_seen, ip, ua, \
             revoked, revoked_at FROM view_sessions WHERE token_hash = $1",
            &[&token_hash],
            true,
        )?;
        Ok(rows.first().map(session_from_row))
    }

    pub fn touch_session(
        &self,
        token_hash: &str,
        last_seen_iso: &str,
        expires_iso: Option<&str>,
    ) -> Result<(), DbUnavailable> {
        match expires_iso {
            None => {
                self.exec(
                    "UPDATE view_sessions SET last_seen = $1::text::timestamptz \
                     WHERE token_hash = $2",
                    &[&last_seen_iso, &token_hash],
                    false,
                )?;
            }
            Some(exp) => {
                self.exec(
                    "UPDATE view_sessions SET last_seen = $1::text::timestamptz, \
                     expires_at = $2::text::timestamptz WHERE token_hash = $3",
                    &[&last_seen_iso, &exp, &token_hash],
                    false,
                )?;
            }
        }
        Ok(())
    }

    pub fn revoke_session(&self, token_hash: &str) -> Result<i64, DbUnavailable> {
        let rows = self.exec(
            "UPDATE view_sessions SET revoked = true, revoked_at = now() \
             WHERE token_hash = $1 AND revoked = false RETURNING 1",
            &[&token_hash],
            false,
        )?;
        Ok(rows.len() as i64)
    }

    pub fn revoke_user_sessions(&self, username: &str) -> Result<i64, DbUnavailable> {
        let rows = self.exec(
            "UPDATE view_sessions SET revoked = true, revoked_at = now() \
             WHERE username = $1 AND revoked = false RETURNING 1",
            &[&username],
            false,
        )?;
        Ok(rows.len() as i64)
    }

    // --- 审计 ---------------------------------------------------------

    pub fn audit(
        &self,
        event: &str,
        username: Option<&str>,
        ip: Option<&str>,
        ua: Option<&str>,
        detail: Option<&str>,
    ) -> Result<(), DbUnavailable> {
        let ts = iso_utc(va::utcnow());
        self.exec(
            "INSERT INTO view_audit (ts, username, ip, ua, event, detail) \
             VALUES ($1::text::timestamptz,$2,$3,$4,$5,$6)",
            &[&ts, &username, &ip, &ua, &event, &detail],
            false,
        )?;
        Ok(())
    }

    pub fn list_audit(
        &self,
        limit: i64,
        event: Option<&str>,
        username: Option<&str>,
    ) -> Result<Vec<Value>, DbUnavailable> {
        let mut sql =
            String::from("SELECT audit_id, ts, username, ip, ua, event, detail FROM view_audit");
        let mut where_parts: Vec<String> = Vec::new();
        if event.is_some() {
            where_parts.push(format!("event = ${}", where_parts.len() + 2));
        }
        if username.is_some() {
            where_parts.push(format!("username = ${}", where_parts.len() + 2));
        }
        if !where_parts.is_empty() {
            sql += " WHERE ";
            sql += &where_parts.join(" AND ");
        }
        sql += &format!(" ORDER BY audit_id DESC LIMIT ${}", where_parts.len() + 2);
        let mut params: Vec<&(dyn ToSql + Sync)> = Vec::new();
        params.push(&event);
        params.push(&username);
        params.push(&limit);
        let rows = self.exec(&sql, &params, false)?;
        Ok(rows.iter().map(audit_from_row).collect())
    }

    // --- 登录失败流水 ---------------------------------------------------

    pub fn record_login_outcome(
        &self,
        ip: &str,
        username: Option<&str>,
        outcome: &str,
    ) -> Result<(), DbUnavailable> {
        let ts = iso_utc(va::utcnow());
        self.exec(
            "INSERT INTO view_login_failures (ts, ip, username, outcome) \
             VALUES ($1::text::timestamptz,$2,$3,$4)",
            &[&ts, &ip, &username, &outcome],
            false,
        )?;
        Ok(())
    }

    /// 同 IP `since` 之后的 (epoch 秒, outcome) 流水（喂 `LockTracker`）。
    pub fn recent_login_rows(
        &self,
        ip: &str,
        since: i64,
    ) -> Result<Vec<(i64, String)>, DbUnavailable> {
        let rows = self.exec(
            "SELECT ts, outcome FROM view_login_failures \
             WHERE ip = $1 AND ts >= $2::text::timestamptz ORDER BY ts",
            &[&ip, &iso_utc(since)],
            false,
        )?;
        let mut out = Vec::new();
        for row in &rows {
            let us: Option<i64> = row
                .try_get::<_, Option<ts_from_row::Ts>>(0)
                .ok()
                .map(|t| t.map(|v| v.0).unwrap_or(0));
            let outcome: String = row.try_get(1).unwrap_or_default();
            if let Some(us) = us {
                if us > 0 {
                    out.push((us / 1_000_000, outcome));
                }
            }
        }
        Ok(out)
    }
}

mod ts_from_row {
    //! timestamptz → epoch 微秒（与 blackboard_svc 同款读法）。
    #[derive(Debug)]
    pub struct Ts(pub i64);
    impl<'a> postgres::types::FromSql<'a> for Ts {
        fn from_sql(
            _ty: &postgres::types::Type,
            raw: &'a [u8],
        ) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
            let us = i64::from_be_bytes(raw.try_into()?);
            const PG_EPOCH_US: i64 = 946_684_800 * 1_000_000;
            Ok(Ts(us + PG_EPOCH_US))
        }
        fn accepts(ty: &postgres::types::Type) -> bool {
            matches!(
                ty,
                &postgres::types::Type::TIMESTAMPTZ | &postgres::types::Type::TIMESTAMP
            )
        }
    }
}

/// epoch 秒 → psycopg2 `datetime.isoformat()` 形状（`+00:00`）。
pub fn iso_utc(secs: i64) -> String {
    va::iso_utc(secs)
}

// ================================================================ 行结构

pub struct UserRow {
    pub username: String,
    pub role: String,
    pub pwd_hash: String,
    pub disabled: bool,
    pub note: Option<String>,
}

pub struct SessionRow {
    pub token_hash: String,
    pub username: String,
    pub role: String,
    pub expires_iso: Option<String>,
    pub revoked: bool,
}

fn user_from_row(row: &postgres::Row) -> UserRow {
    UserRow {
        username: row.get::<_, String>(0),
        role: va::normalize_role(&row.get::<_, String>(1)).to_string(),
        pwd_hash: row.get::<_, String>(2),
        disabled: row.get::<_, bool>(3),
        note: row.get::<_, Option<String>>(4),
    }
}

fn session_from_row(row: &postgres::Row) -> SessionRow {
    let us: Option<i64> = row
        .try_get::<_, Option<ts_from_row::Ts>>(4)
        .ok()
        .map(|t| t.map(|v| v.0).unwrap_or(0));
    let expires_iso = us.filter(|v| *v > 0).map(|v| va::iso_utc(v / 1_000_000));
    SessionRow {
        token_hash: row.get::<_, String>(0),
        username: row.get::<_, String>(1),
        role: va::normalize_role(&row.get::<_, String>(2)).to_string(),
        expires_iso,
        revoked: row.get::<_, bool>(8),
    }
}

fn audit_from_row(row: &postgres::Row) -> Value {
    let us: Option<i64> = row
        .try_get::<_, Option<ts_from_row::Ts>>(1)
        .ok()
        .map(|t| t.map(|v| v.0).unwrap_or(0));
    json!({
        "audit_id": row.get::<_, i64>(0),
        "ts": us.filter(|v| *v > 0).map(|v| Value::String(iso_utc(v / 1_000_000))).unwrap_or(Value::Null),
        "username": row.get::<_, Option<String>>(2),
        "ip": row.get::<_, Option<String>>(3),
        "ua": row.get::<_, Option<String>>(4),
        "event": row.get::<_, String>(5),
        "detail": row.get::<_, Option<String>>(6),
    })
}

// ================================================================ Auth 门面

/// 一次请求背后的「人」（Python `view_auth.Principal`）。
#[derive(Debug, Clone)]
pub struct Principal {
    pub username: String,
    pub role: String,
    pub source: String, // 固定 "session"
    pub token: String,
    pub token_hash: String,
    pub expires_at: i64,
}

/// 看板侧认证门面（Python `view_auth.Auth`）：store + 会话密钥 + 锁定计数 + 审计。
pub struct Auth {
    pub store: Arc<ViewStore>,
    pub key: Vec<u8>,
    pub ttl: i64,
    pub locks: va::LockTracker,
}

impl Auth {
    pub fn new(store: Arc<ViewStore>, session_key: Vec<u8>, ttl_seconds: i64) -> Self {
        Auth {
            store,
            key: session_key,
            ttl: if ttl_seconds > 0 {
                ttl_seconds
            } else {
                va::SESSION_TTL_S
            },
            locks: va::LockTracker::default(),
        }
    }

    /// 落审计表。库挂了不抛（调用方无需处理）——审计尽力而为，主流程不能被它带崩。
    pub fn audit(&self, event: &str, username: Option<&str>, ip: &str, ua: &str, detail: &str) {
        let detail = truncate(detail, 500);
        let ua_cut = truncate(ua, 300);
        let _ = self
            .store
            .audit(event, username, Some(ip), Some(&ua_cut), Some(&detail));
    }

    /// 登录：返回 (成功与否, 签名后的 cookie 值, 给人看的原因)。
    ///
    /// 失败提示**一律是同一句** —— 不给用户名枚举器；真实原因只进审计。
    pub fn login(
        &self,
        username: &str,
        password: &str,
        ip: &str,
        ua: &str,
    ) -> (bool, Option<String>, String) {
        let generic = "账号或口令不正确";
        let now = va::utcnow();

        // 锁定判定：内存 + 表双记，取较晚（表那份由调用方喂，库抖了喂 None）。
        let table_rows = self
            .store
            .recent_login_rows(ip, now - (va::FAIL_WINDOW_S + va::LOCK_S))
            .ok();
        if let Some(locked) = self.locks.locked_until(ip, table_rows.as_deref(), now) {
            let wait = (locked - now).div_euclid(60) + 1;
            self.audit(
                "login_lock",
                Some(username),
                ip,
                ua,
                &format!("锁定中，拒绝登录；解锁还需约 {wait} 分钟"),
            );
            return (false, None, format!("尝试过于频繁，请约 {wait} 分钟后再试"));
        }

        let user = match self.store.get_user(username) {
            Ok(u) => u,
            Err(_) => {
                self.audit("login_fail", Some(username), ip, ua, "后端不可用");
                return (false, None, "认证后端暂时不可用，请稍后再试".into());
            }
        };

        let ok = user
            .as_ref()
            .map(|u| !u.disabled && va::verify_password(password, &u.pwd_hash))
            .unwrap_or(false);
        if !ok {
            self.locks.record(ip, "fail", now);
            let _ = self.store.record_login_outcome(ip, Some(username), "fail");
            let reason = match &user {
                None => "账号不存在",
                Some(u) if u.disabled => "账号已停用",
                Some(_) => "口令不匹配",
            };
            self.audit("login_fail", Some(username), ip, ua, reason);
            return (false, None, generic.into());
        }

        let user = user.unwrap();
        let token = va::new_token();
        let fingerprint = va::token_fingerprint(&token);
        let expires = now + self.ttl;
        let created_iso = iso_utc(now);
        let expires_iso = iso_utc(expires);
        if self
            .store
            .create_session(
                &fingerprint,
                &user.username,
                &user.role,
                &created_iso,
                &expires_iso,
                ip,
                ua,
            )
            .and_then(|_| self.store.touch_login(&user.username, &created_iso))
            .is_err()
        {
            return (false, None, "认证后端暂时不可用，请稍后再试".into());
        }

        self.locks.record(ip, "ok", now);
        let _ = self
            .store
            .record_login_outcome(ip, Some(&user.username), "ok");
        self.audit(
            "login_ok",
            Some(&user.username),
            ip,
            ua,
            &format!(
                "角色 {}，会话有效至 {}（北京时间）",
                user.role,
                va::beijing(&expires_iso)
            ),
        );
        (
            true,
            Some(va::sign_token(&token, &self.key, b"session")),
            String::new(),
        )
    }

    /// cookie → Principal（Python `Auth.check_cookie`）。
    ///
    /// 依次卡：签名 → 库里有 → 没撤销 → 没过期 → 账号还在；任何一关不过都只回
    /// 一句"请重新登录"，不告诉浏览器卡在哪一关。
    pub fn check_cookie(
        &self,
        cookie_value: &str,
        _ip: &str,
        _ua: &str,
    ) -> (Option<Principal>, String) {
        if cookie_value.is_empty() {
            return (None, "没有会话 cookie".into());
        }
        let Some(token) = va::unsign_token(cookie_value, &self.key, b"session") else {
            return (None, "cookie 签名不匹配（被篡改或换过密钥）".into());
        };
        let fingerprint = va::token_fingerprint(&token);
        let Some(row) = (match self.store.get_session(&fingerprint) {
            Ok(r) => r,
            Err(_) => return (None, "认证后端不可用".into()),
        }) else {
            return (None, "会话不存在".into());
        };
        if row.revoked {
            return (None, "会话已撤销".into());
        }
        let now = va::utcnow();
        let Some(expires) = row.expires_iso.as_deref().and_then(va::parse_ts) else {
            return (None, "会话已过期".into());
        };
        if expires <= now {
            return (None, "会话已过期".into());
        }
        let Ok(user_opt) = self.store.get_user(&row.username) else {
            return (None, "账号已停用".into());
        };
        let Some(user) = user_opt.filter(|u| !u.disabled) else {
            return (None, "账号已停用".into());
        };

        // 续期：剩余不足一半就顺延到满 TTL（活跃的人不该在看着看着被踢出去）。
        let mut renewed: Option<i64> = None;
        if expires - now < va::SESSION_RENEW_S {
            renewed = Some(now + self.ttl);
        }
        let _ = self.store.touch_session(
            &fingerprint,
            &iso_utc(now),
            renewed.map(|r| iso_utc(r)).as_deref(),
        );
        // 角色以**账号表当前值**为准：改了角色立刻生效。
        (
            Some(Principal {
                username: user.username,
                role: user.role,
                source: "session".into(),
                token,
                token_hash: fingerprint,
                expires_at: renewed.unwrap_or(expires),
            }),
            String::new(),
        )
    }

    /// 退出登录：撤销会话（Python `Auth.logout`）。
    pub fn logout(&self, cookie_value: &str, ip: &str, ua: &str) -> bool {
        let Some(token) = va::unsign_token(cookie_value, &self.key, b"session") else {
            return false;
        };
        let fingerprint = va::token_fingerprint(&token);
        let username = self
            .store
            .get_session(&fingerprint)
            .ok()
            .flatten()
            .map(|r| r.username);
        let Ok(changed) = self.store.revoke_session(&fingerprint) else {
            return false;
        };
        if changed > 0 {
            self.audit("logout", username.as_deref(), ip, ua, "用户主动退出登录");
        }
        changed > 0
    }

    /// 改自己的口令：必须先验旧口令；改完把这个人的会话全部撤掉。
    pub fn change_password(
        &self,
        username: &str,
        old: &str,
        new: &str,
        ip: &str,
        ua: &str,
    ) -> (bool, String) {
        if let Err(msg) = va::check_password_policy(new) {
            return (false, msg);
        }
        if old == new {
            return (false, "新口令不能和旧口令一样".into());
        }
        let user = match self.store.get_user(username) {
            Ok(u) => u,
            Err(_) => return (false, "认证后端暂时不可用，请稍后再试".into()),
        };
        let Some(user) = user.filter(|u| !u.disabled) else {
            return (false, "旧口令不正确".into());
        };
        if !va::verify_password(old, &user.pwd_hash) {
            self.audit("passwd", Some(username), ip, ua, "改密失败：旧口令不匹配");
            return (false, "旧口令不正确".into());
        }
        let new_hash = va::hash_password(new, None);
        let killed = match self
            .store
            .set_password(username, &new_hash)
            .and_then(|_| self.store.revoke_user_sessions(username))
        {
            Ok(k) => k,
            Err(_) => return (false, "认证后端暂时不可用，请稍后再试".into()),
        };
        self.audit(
            "passwd",
            Some(username),
            ip,
            ua,
            &format!("改密成功，同时撤销了 {killed} 个会话"),
        );
        (
            true,
            "口令已修改，所有旧会话已失效，请用新口令重新登录".into(),
        )
    }
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        text.chars().take(max).collect()
    }
}

// ================================================================ 遮罩（v1.3 sensitive 口径 + v1.4 角色口径）

/// 角色不够时能留下的字段（Python `SHARED_PUBLIC_KEYS`）。
const SHARED_PUBLIC_KEYS: [&str; 13] = [
    "shared_id",
    "scope",
    "kind",
    "task_id",
    "sensitivity",
    "updated_at",
    "expires_at",
    "review_at",
    "version",
    "revoked",
    "expired",
    "review_due",
    "expiring_soon",
];

/// sensitive 且未授权时能留下的字段（Python `SHARED_SAFE_KEYS`）。
const SHARED_SAFE_KEYS: [&str; 16] = [
    "shared_id",
    "scope",
    "kind",
    "owner",
    "task_id",
    "task_title",
    "updated_at",
    "expires_at",
    "review_at",
    "version",
    "revoked",
    "sensitive",
    "authorized",
    "expired",
    "review_due",
    "expiring_soon",
];

/// 响应里可能装着条目的列表键（Python `LIST_KEYS`）。
const LIST_KEYS: [&str; 8] = [
    "items",
    "permanent",
    "results",
    "entries",
    "actions",
    "issues",
    "turns",
    "agenda_items",
];

/// 一条内容的密级（Python `item_sensitivity`）。
///
/// 优先 `sensitivity` 字段；没有就看老的 `sensitive` 布尔；都没有时 `default` 为
/// None 表示"这条东西压根没有密级概念"，`internal` 表示"没标按内部算"（默认收紧）。
pub fn item_sensitivity(item: &Map<String, Value>, default: Option<&str>) -> Option<String> {
    let raw = item
        .get("sensitivity")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if SENSITIVITY_LEVELS.contains(&raw.as_str()) {
        return Some(raw);
    }
    if item.get("sensitive").and_then(Value::as_bool) == Some(true) {
        return Some("sensitive".into());
    }
    default.map(str::to_string)
}

fn keep_keys(item: &Map<String, Value>, keys: &[&str]) -> Map<String, Value> {
    let mut out = Map::new();
    for (k, v) in item {
        if keys.contains(&k.as_str()) {
            out.insert(k.clone(), v.clone());
        }
    }
    out
}

fn mask_item(item: &mut Value, role: Option<&str>, default_level: Option<&str>) {
    let Some(obj) = item.as_object_mut() else {
        return;
    };
    let level = item_sensitivity(obj, default_level);
    if let Some(level) = &level {
        if let Some(role) = role {
            if !va::role_can_see(role, level) {
                let mut masked = keep_keys(obj, &SHARED_PUBLIC_KEYS);
                masked.insert("masked".into(), json!(true));
                masked.insert("mask_reason".into(), json!("role"));
                masked.insert("sensitivity".into(), json!(level));
                masked.insert("title".into(), json!(""));
                masked.insert("body".into(), json!(""));
                masked.insert("tags".into(), json!([]));
                *item = Value::Object(masked);
                return;
            }
        }
    }
    let sensitive = obj.get("sensitive").and_then(Value::as_bool) == Some(true);
    let authorized = obj.get("authorized").and_then(Value::as_bool) == Some(true);
    if !sensitive || authorized {
        return;
    }
    let mut masked = keep_keys(obj, &SHARED_SAFE_KEYS);
    masked.insert("masked".into(), json!(true));
    masked.insert("mask_reason".into(), json!("sensitive"));
    masked.insert("title".into(), json!(""));
    masked.insert("body".into(), json!(""));
    masked.insert("tags".into(), json!([]));
    *item = Value::Object(masked);
}

fn walk(payload: &mut Value, role: Option<&str>, default_level: Option<&str>) {
    let Some(obj) = payload.as_object_mut() else {
        return;
    };
    for key in LIST_KEYS {
        if let Some(rows) = obj.get_mut(key).and_then(Value::as_array_mut) {
            for row in rows.iter_mut() {
                mask_item(row, role, default_level);
            }
        }
    }
    if let Some(tasks) = obj.get_mut("tasks").and_then(Value::as_array_mut) {
        for task in tasks.iter_mut() {
            if let Some(items) = task.get_mut("items").and_then(Value::as_array_mut) {
                for row in items.iter_mut() {
                    mask_item(row, role, default_level);
                }
            }
        }
    }
    if let Some(session) = obj.get_mut("session") {
        if session.is_object() {
            walk(session, role, default_level);
        }
    }
}

/// 共享内容的遮罩：没标密级的按 internal 算（默认收紧）。
pub fn mask_shared(payload: &mut Value, role: Option<&str>) {
    walk(payload, role, Some(va::DEFAULT_SENSITIVITY));
}

/// 所有 API 响应的统一收口：按角色再过一遍。
///
/// 与 `mask_shared` 的差别只有一个 —— default_level=None：**没带密级字段的条目
/// 原样放行**（会议、落实事项、遗留问题的公开字段本来就是给所有登录用户看的）。
pub fn apply_role(payload: &mut Value, role: Option<&str>) {
    if role.is_none() || role == Some("owner") {
        return; // owner 看全量（sensitive 仍受上游 authorized 约束）
    }
    walk(payload, role, None);
}

// ================================================================ 单测

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn shared_item(fields: Value) -> Map<String, Value> {
        fields.as_object().unwrap().clone()
    }

    #[test]
    fn test_item_sensitivity_priority() {
        // sensitivity 字段优先；老 sensitive 布尔次之；默认收紧 internal
        let a = shared_item(json!({"sensitivity": "Public", "sensitive": false}));
        assert_eq!(item_sensitivity(&a, Some("internal")).unwrap(), "public");
        let b = shared_item(json!({"sensitive": true}));
        assert_eq!(item_sensitivity(&b, None).unwrap(), "sensitive");
        let c = shared_item(json!({"kind": "note"}));
        assert_eq!(item_sensitivity(&c, None), None);
        assert_eq!(item_sensitivity(&c, Some("internal")).unwrap(), "internal");
    }

    #[test]
    fn test_mask_role() {
        // viewer 看 internal 的共享内容：只剩骨架，标题正文标签全抹
        let mut item = json!({
            "shared_id": "s1", "scope": "task", "kind": "note", "owner": "刚哥",
            "task_id": "t1", "task_title": "秘密任务", "title": "内网地址",
            "body": "10.0.0.1", "tags": ["secret"], "sensitivity": "internal",
            "updated_at": "2026-10-01T00:00:00+00:00", "version": 3
        });
        mask_item(&mut item, Some("viewer"), None);
        assert_eq!(item["masked"], json!(true));
        assert_eq!(item["mask_reason"], json!("role"));
        assert_eq!(item["title"], json!(""));
        assert_eq!(item["body"], json!(""));
        assert_eq!(item["owner"], Value::Null); // 连 owner 都不给
        assert_eq!(item["shared_id"], json!("s1"));
        assert_eq!(item["version"], json!(3));
    }

    #[test]
    fn test_mask_sensitive_unauthorized() {
        // owner 也受 v1.3 口径约束：sensitive 且未 authorized → 骨架 + safe keys
        let mut item = json!({
            "shared_id": "s2", "kind": "profile", "owner": "刚哥",
            "title": "主人画像", "body": "关键信息", "tags": ["p"],
            "sensitive": true, "authorized": false
        });
        mask_item(&mut item, Some("owner"), None);
        assert_eq!(item["masked"], json!(true));
        assert_eq!(item["mask_reason"], json!("sensitive"));
        assert_eq!(item["body"], json!(""));
        assert_eq!(item["owner"], json!("刚哥")); // safe keys 里有 owner
        assert_eq!(item["sensitivity"], Value::Null); // 不是 role 口径，不带密级
    }

    #[test]
    fn test_mask_authorized_pass() {
        // owner + authorized：v1.3 口径放行
        let mut item = json!({"shared_id": "s3", "title": "ok", "body": "x",
                              "sensitive": true, "authorized": true});
        mask_item(&mut item, Some("owner"), None);
        assert_eq!(item["body"], json!("x"));
        assert!(item.get("masked").is_none());
        // viewer + authorized：角色口径**先**判，authorized 救不了（Python 顺序一致）
        let mut item2 = json!({"shared_id": "s4", "title": "ok", "body": "x",
                               "sensitive": true, "authorized": true});
        mask_item(&mut item2, Some("viewer"), None);
        assert_eq!(item2["mask_reason"], json!("role"));
        assert_eq!(item2["body"], json!(""));
    }

    #[test]
    fn test_apply_role_walks_lists_and_session() {
        let mut payload = json!({
            "items": [{"shared_id": "a", "sensitivity": "internal", "body": "b1"}],
            "tasks": [{"items": [{"shared_id": "b", "sensitivity": "sensitive", "body": "b2"}]}],
            "session": {"agenda_items": [{"shared_id": "c", "sensitive": true, "body": "b3"}]}
        });
        apply_role(&mut payload, Some("member"));
        assert_eq!(payload["items"][0]["body"], json!("b1")); // member 看 internal
        assert_eq!(payload["tasks"][0]["items"][0]["masked"], json!(true)); // sensitive 遮
        assert_eq!(payload["session"]["agenda_items"][0]["masked"], json!(true));
        // owner 放行全量
        let mut p2 =
            json!({"items": [{"shared_id": "a", "sensitivity": "internal", "body": "b1"}]});
        apply_role(&mut p2, Some("owner"));
        assert_eq!(p2["items"][0]["body"], json!("b1"));
        // role=None 放行
        let mut p3 =
            json!({"items": [{"shared_id": "a", "sensitivity": "internal", "body": "b1"}]});
        apply_role(&mut p3, None);
        assert_eq!(p3["items"][0]["body"], json!("b1"));
    }

    #[test]
    fn test_mask_shared_defaults_internal() {
        // 没标密级的共享内容按 internal：viewer 遮，member 放
        let mut p = json!({"items": [{"shared_id": "a", "body": "x"}]});
        mask_shared(&mut p, Some("viewer"));
        assert_eq!(p["items"][0]["masked"], json!(true));
        let mut p2 = json!({"items": [{"shared_id": "a", "body": "x"}]});
        mask_shared(&mut p2, Some("member"));
        assert_eq!(p2["items"][0]["body"], json!("x"));
    }
}
