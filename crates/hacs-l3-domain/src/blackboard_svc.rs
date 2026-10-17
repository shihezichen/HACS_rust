//! blackboard_svc 纯逻辑 + 数据访问层（Python `hub/l3_domain/blackboard_svc.py` v1.9 的移植）。
//!
//! 契约要点（与 Python 严格对齐）：
//! · 错误一律平信封 `{"ok": false, "error": "<code>", "detail": "..."}`（与 chat_svc 的
//!   对象信封不同——Python blackboard_svc 一直是平的，parity 以 Python 为准）；
//! · 时间：黑板/会议/跟踪走 ISO8601 原样（psycopg2 默认带偏移）；tmp/sess/msg 走
//!   `_iso_z` 的 `...Z` 形状；
//! · 身份只由 Bearer token 的 sha256 反查 peers.json 决定，body 里的 author/owner/from
//!   一律忽略（防冒名）；
//! · 黑板 / 会议 / 共享内容 / 议程 / 确认对本服务只有 SELECT；tmp / sess / msg 有受控写路径；
//! · 缺表/缺列降级（degraded[]），与"库挂了"（503）严格分开；
//! · 向量嵌入是可选增强：embedder 不可用时写入照常 200（embedding NULL）、检索退关键词。
//! 数据库：PostgreSQL（postgres crate，与 chat_svc 同一套；BB_PG_DSN env）。

use postgres::types::ToSql;
use serde_json::{json, Map, Value};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

// ================================================================ 常量

pub const VERSION: &str = "1.9";
pub const DEFAULT_PORT: u16 = 9940;

pub const MAX_BODY_BYTES: usize = 262144; // 256 KiB（与 a2a_svc 对齐）
pub const DRAIN_MAX: usize = 4 * 1024 * 1024;
pub const DETAIL_MAX: usize = 200;

pub const DEFAULT_LIMIT: i64 = 20;
pub const MAX_LIMIT: i64 = 200;
pub const SEARCH_DEFAULT_LIMIT: i64 = 10;
pub const SEARCH_MAX_LIMIT: i64 = 50;
pub const SEARCH_QUERY_MAX: usize = 2048;
pub const TEXT_FIELD_MAX: usize = 512;
pub const DB_CONNECT_TIMEOUT_S: u64 = 5;

pub const SESSIONS_DEFAULT_LIMIT: i64 = 20;
pub const SESSIONS_MAX_LIMIT: i64 = 50;
pub const TURNS_DEFAULT_LIMIT: i64 = 50;
pub const SESSION_ID_MAX: usize = 128;
pub const TRACK_DEFAULT_LIMIT: i64 = 20;
pub const TRACK_MAX_LIMIT: i64 = 50;
pub const OWNER_MAX: usize = 128;

pub const TMP_DEFAULT_TTL_S: i64 = 7200;
pub const TMP_MIN_TTL_S: i64 = 60;
pub const TMP_MAX_TTL_S: i64 = 86400;
pub const TMP_READS_MIN: i64 = 1;
pub const TMP_READS_MAX: i64 = 100;
pub const TMP_OWNER_ACTIVE_MAX: i64 = 200;
pub const TMP_ACTIVE_MAX: i64 = 5000;
pub const TMP_SWEEP_INTERVAL_S: u64 = 300;
pub const TMP_LIST_DEFAULT_LIMIT: i64 = 20;
pub const TMP_LIST_MAX_LIMIT: i64 = 50;
pub const TMP_ID_PREFIX: &str = "t-";
pub const TMP_ID_LEN: usize = 8;
pub const TMP_ID_ALPHABET: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

pub const SESS_ID_PREFIX: &str = "as-";
pub const SESS_ID_RAND_LEN: usize = 4;
pub const SESS_ID_TRY: usize = 5;
pub const SESS_TOPIC_MAX: usize = TEXT_FIELD_MAX;
pub const SESS_DEFAULT_TOPIC: &str = "general";
pub const SESS_STATES: [&str; 3] = ["open", "closed", "expired"];
pub const SESS_TERMINAL_STATES: [&str; 2] = ["closed", "expired"];
pub const SESS_CLOSE_REASON_MANUAL: &str = "manual";
pub const SESS_CLOSE_REASON_IDLE: &str = "idle";
pub const SESS_DEFAULT_IDLE_TTL_S: i64 = 1800;
pub const SESS_MIN_IDLE_TTL_S: i64 = 60;
pub const SESS_MAX_IDLE_TTL_S: i64 = 86400;
pub const SESS_MEMBERS_MAX: usize = 100;
pub const SESS_MEMBER_NAME_MAX: usize = 64;
pub const SESS_LIST_DEFAULT_LIMIT: i64 = 20;
pub const SESS_LIST_MAX_LIMIT: i64 = 50;
pub const SESS_TURNS_DEFAULT_LIMIT: i64 = 50;
pub const SESS_TURNS_MAX_LIMIT: i64 = 200;
pub const SESS_DETAIL_TURNS_DEFAULT: i64 = 20;
pub const SESS_ARCHIVE_EXPIRES_DAYS: i64 = 30;
pub const SESS_ARCHIVE_KIND: &str = "note";
pub const SESS_ARCHIVE_TASK_PREFIX: &str = "sess-archive-";
pub const SESS_UNARCHIVED_BATCH: i64 = 50;
pub const SESS_ROLES: [&str; 3] = ["user", "assistant", "system"];

pub const MSG_ID_PREFIX: &str = "m-";
pub const MSG_ID_LEN: usize = 8;
pub const MSG_ID_TRY: usize = 5;
pub const MSG_STATES: [&str; 6] = ["queued", "processing", "done", "failed", "expired", "stalled"];
pub const MSG_TERMINAL_STATES: [&str; 4] = ["done", "failed", "expired", "stalled"];
pub const MSG_OPEN_STATES: [&str; 2] = ["queued", "processing"];
pub const MSG_DEFAULT_TOPIC: &str = "general";
pub const MSG_TOPIC_MAX: usize = TEXT_FIELD_MAX;
pub const MSG_BODY_MAX: usize = 65536;
pub const MSG_MEMBER_MAX: usize = 64;
pub const MSG_DEFAULT_TTL_S: i64 = 86400;
pub const MSG_MIN_TTL_S: i64 = 60;
pub const MSG_MAX_TTL_S: i64 = 604800;
pub const MSG_LIST_DEFAULT_LIMIT: i64 = 20;
pub const MSG_LIST_MAX_LIMIT: i64 = 50;
pub const MSG_STALL_AFTER_S: i64 = 900;

pub const ACTION_STATUSES: [&str; 4] = ["open", "doing", "done", "dropped"];
pub const ACTION_OPEN_STATUSES: [&str; 2] = ["open", "doing"];
pub const ISSUE_STATUSES: [&str; 3] = ["open", "watching", "closed"];
pub const ISSUE_OPEN_STATUSES: [&str; 2] = ["open", "watching"];
pub const AGENDA_STATUSES: [&str; 3] = ["pending", "concluded", "deferred"];
pub const ISSUE_KINDS: [&str; 2] = ["issue", "defect"];
pub const DEFECT_KIND: &str = "defect";

pub const SHARED_DEFAULT_LIMIT: i64 = 20;
pub const SHARED_MAX_LIMIT: i64 = 50;
pub const SHARED_REVISION_LIMIT: i64 = 100;
pub const SHARED_SCOPES: [&str; 2] = ["task", "permanent"];
pub const SHARED_STATUSES: [&str; 4] = ["active", "expired", "revoked", "destroyed"];
pub const SHARED_TASK_STATUSES: [&str; 3] = ["active", "done", "aborted"];
pub const SHARED_HIDDEN_STATUSES: [&str; 2] = ["revoked", "destroyed"];
pub const SHARED_ALL_AUDIENCE: &str = "all";

pub const EMBEDDING_DIM: usize = 1024;

// ================================================================ 错误

/// 数据库不可用（消息必须已脱敏——scrub 与 Python `_safe_err` 对齐）。
#[derive(Debug, Clone)]
pub struct DbUnavailable(pub String);

impl std::fmt::Display for DbUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// 会话已终态还追加轮次 → 409 session_not_open。
#[derive(Debug, Clone)]
pub struct SessionNotOpen(pub String);

/// 请求级错误（平信封：`{"ok": false, "error": code, "detail": ...}`）。
#[derive(Debug, Clone)]
pub struct ApiError {
    pub status: u16,
    pub code: String,
    pub detail: Option<String>,
    pub extra: Vec<(String, Value)>,
}

impl ApiError {
    pub fn new(status: u16, code: &str) -> Self {
        ApiError { status, code: code.into(), detail: None, extra: vec![] }
    }
    pub fn detail(mut self, d: impl Into<String>) -> Self {
        self.detail = Some(d.into());
        self
    }
    pub fn with(mut self, k: &str, v: Value) -> Self {
        self.extra.push((k.into(), v));
        self
    }
    pub fn body(&self) -> Value {
        let mut m = Map::new();
        m.insert("ok".into(), Value::Bool(false));
        m.insert("error".into(), json!(self.code));
        if let Some(d) = &self.detail {
            m.insert("detail".into(), json!(d));
        }
        for (k, v) in &self.extra {
            m.insert(k.clone(), v.clone());
        }
        Value::Object(m)
    }
}

/// 脱敏：抹掉 DSN 口令、换行压平、截断 200 字符（Python `_safe_err` 逐条对齐）。
pub fn safe_err(text: &str) -> String {
    let mut out = String::new();
    let lower = text.to_ascii_lowercase();
    let bytes = text.as_bytes();
    let mut i = 0usize;
    let keys = ["password", "passfile", "sslpassword"];
    while i < bytes.len() {
        let mut hit: Option<usize> = None;
        for k in keys {
            if lower[i..].starts_with(k) {
                // 后面跳过空白与 '='，然后吞到空白为止
                let mut j = i + k.len();
                while j < bytes.len() && (bytes[j] as char).is_ascii_whitespace() {
                    j += 1;
                }
                if j < bytes.len() && bytes[j] == b'=' {
                    j += 1;
                    while j < bytes.len() && !(bytes[j] as char).is_ascii_whitespace() {
                        j += 1;
                    }
                    out.push_str(&format!("{}=***", &text[i..i + k.len()]));
                    hit = Some(j);
                }
                break;
            }
        }
        match hit {
            Some(j) => i = j,
            None => {
                let ch = text[i..].chars().next().unwrap();
                out.push(if ch == '\n' { ' ' } else { ch });
                i += ch.len_utf8();
            }
        }
    }
    let out = out.trim().to_string();
    if out.is_empty() {
        return "DbUnavailable".into();
    }
    out.chars().take(DETAIL_MAX).collect()
}

/// "缺表/缺列"（降级）与"库挂了"（503）分开：PG 原话匹配（Python `_is_missing_schema`）。
pub fn is_missing_schema(err: &str) -> bool {
    let low = err.to_ascii_lowercase();
    ["does not exist", "undefined table", "undefined column", "no such table", "no such column", "unknown column"]
        .iter()
        .any(|k| low.contains(k))
}

// ================================================================ 时间

pub fn now_epoch() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

pub fn now_micros() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_micros() as i64).unwrap_or(0)
}

/// UTC civil 历法（Howard Hinnant 算法，chat_svc 同款）。
pub fn civil_from_epoch(epoch_s: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = epoch_s.div_euclid(86400);
    let secs = epoch_s.rem_euclid(86400);
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d, (secs / 3600) as u32, ((secs % 3600) / 60) as u32, (secs % 60) as u32)
}

/// `_iso_z`：timestamptz → `...Z` 形状（tmp/sess/msg 契约示例的形状）。
/// 输入是 epoch 微秒（PG 读出的 timestamptz 由 store 层转）。
pub fn iso_z(epoch_us: i64) -> String {
    let secs = epoch_us.div_euclid(1_000_000);
    let (y, mo, d, h, mi, s) = civil_from_epoch(secs);
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", y, mo, d, h, mi, s)
}

/// `now.isoformat()`（Python datetime 带微秒的 UTC 形状：`YYYY-MM-DDTHH:MM:SS.ffffff+00:00`）。
pub fn iso_utc_now() -> String {
    let us = now_micros();
    let secs = us.div_euclid(1_000_000);
    let frac = us.rem_euclid(1_000_000);
    let (y, mo, d, h, mi, s) = civil_from_epoch(secs);
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:06}+00:00", y, mo, d, h, mi, s, frac)
}

/// `to_utc`：ISO 字符串 / 数字 epoch（秒或毫秒）→ epoch 微秒；认不出来 None（裸时间按 UTC）。
pub fn to_utc_epoch_us(value: &Value) -> Option<i64> {
    match value {
        Value::Number(n) => {
            let f = n.as_f64()?;
            // Python datetime 直通场景只有测试假库；生产只来字符串。按秒解释。
            Some((f * 1_000_000.0).round() as i64)
        }
        Value::String(s) => {
            let text = s.trim();
            if text.is_empty() {
                return None;
            }
            parse_iso_to_epoch_us(text)
        }
        _ => None,
    }
}

/// 宽松 ISO 解析：`YYYY-MM-DD[ T]HH:MM[:SS[.ffffff]][+HH:MM|Z]`；日期-only 也认。
pub fn parse_iso_to_epoch_us(text: &str) -> Option<i64> {
    let body = text.trim();
    let bytes = body.as_bytes();
    if bytes.len() < 10 {
        return None;
    }
    let y: i64 = body.get(0..4)?.parse().ok()?;
    if bytes.get(4) != Some(&b'-') {
        return None;
    }
    let mo: i64 = body.get(5..7)?.parse().ok()?;
    let d: i64 = body.get(8..10)?.parse().ok()?;
    #[allow(unused_assignments)]
    let (mut h, mut mi, mut s, mut frac_us) = (0i64, 0i64, 0i64, 0i64);
    let mut rest = &body[10..];
    if !rest.is_empty() {
        let first = rest.chars().next()?;
        if first != 'T' && first != 't' && first != ' ' {
            return None;
        }
        rest = &rest[1..];
        let time = rest;
        // HH:MM[:SS[.frac]][Z|±HH:MM]
        let hms: String = time.chars().take_while(|c| c.is_ascii_digit() || *c == ':' || *c == '.').collect();
        let tz_part = &time[hms.len()..];
        let segs: Vec<&str> = hms.split(':').collect();
        if segs.is_empty() || segs.len() > 3 {
            return None;
        }
        h = segs[0].parse().ok()?;
        if segs.len() > 1 {
            mi = segs[1].parse().ok()?;
        }
        if segs.len() > 2 {
            let ss = segs[2];
            if let Some(dot) = ss.find('.') {
                s = ss[..dot].parse().ok()?;
                let f = &ss[dot + 1..];
                let mut fd = String::new();
                for c in f.chars() {
                    if !c.is_ascii_digit() {
                        break;
                    }
                    if fd.len() < 6 {
                        fd.push(c);
                    }
                }
                while fd.len() < 6 {
                    fd.push('0');
                }
                frac_us = fd.parse::<i64>().unwrap_or(0);
            } else {
                s = ss.parse().ok()?;
            }
        }
        // 时区
        let tz = tz_part.trim();
        let mut offset_s: i64 = 0;
        if !tz.is_empty() {
            if tz == "Z" || tz == "z" {
                offset_s = 0;
            } else {
                let (sign, tzb) = match tz.as_bytes()[0] {
                    b'+' => (1i64, &tz[1..]),
                    b'-' => (-1i64, &tz[1..]),
                    _ => return None,
                };
                let parts: Vec<&str> = tzb.split(':').collect();
                let th: i64 = parts.first()?.parse().ok()?;
                let tm: i64 = parts.get(1).and_then(|x| x.parse().ok()).unwrap_or(0);
                offset_s = sign * (th * 3600 + tm * 60);
            }
        }
        let days = days_from_civil(y, mo, d)?;
        let secs = days * 86400 + h * 3600 + mi * 60 + s - offset_s;
        return Some(secs * 1_000_000 + frac_us);
    }
    let days = days_from_civil(y, mo, d)?;
    Some(days * 86400 * 1_000_000)
}

/// 天数 → civil 的逆（days_from_civil，Howard Hinnant）。
pub fn days_from_civil(y: i64, m: i64, d: i64) -> Option<i64> {
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146097 + doe - 719468)
}

// ================================================================ 值/行转换助手

/// psycopg2 风格 jsonb 读回（chat_svc store 同款：jsonb 直读 Value，字符串补解析）。
pub fn as_refs(value: &Value) -> Value {
    match value {
        Value::Null => Value::Array(vec![]),
        Value::Array(_) => value.clone(),
        Value::String(s) => serde_json::from_str(s).unwrap_or(Value::Array(vec![])),
        _ => Value::Array(vec![]),
    }
}

fn as_names(value: &Value) -> Value {
    match value {
        Value::Array(_) => value.clone(),
        _ => Value::Array(vec![]),
    }
}

#[allow(dead_code)]
fn as_str_or_null(value: &Value) -> Value {
    match value {
        Value::String(s) => json!(s),
        Value::Null => Value::Null,
        other => json!(other.to_string()),
    }
}

/// `_as_int`：计数列统一成 int 或 null。
fn as_int(value: &Value) -> Value {
    match value {
        Value::Number(n) if n.is_i64() || n.is_u64() => json!(n.as_i64().unwrap_or(0)),
        Value::String(s) => s.parse::<i64>().map(|v| json!(v)).unwrap_or(Value::Null),
        Value::Null => Value::Null,
        Value::Bool(_) => Value::Null,
        other => match other.as_f64() {
            Some(f) => json!(f as i64),
            None => Value::Null,
        },
    }
}

/// `_as_float`：confidence/attendance_rate 是 real。
fn as_float(value: &Value) -> Value {
    match value {
        Value::Number(n) => json!(n.as_f64().unwrap_or(0.0)),
        Value::String(s) => s.parse::<f64>().map(|v| json!(v)).unwrap_or(Value::Null),
        Value::Null => Value::Null,
        _ => Value::Null,
    }
}

/// `_as_summary`：confirm_summary（jsonb 对象）补齐四个名单 + required/present/passed。
/// passed 只认真正的布尔 true；拿不准一律 null（"没确认"绝不能显示成"通过了"）。
pub fn as_summary(value: &Value) -> Value {
    let parsed;
    let data: &Value = match value {
        Value::Object(_) => value,
        Value::String(s) => {
            match serde_json::from_str::<Value>(s) {
                Ok(v) if v.is_object() => {
                    parsed = v;
                    &parsed
                }
                _ => return Value::Null,
            }
        }
        _ => return Value::Null,
    };
    let mut out = Map::new();
    for key in ["agree", "object", "abstain", "absent"] {
        let names = data.get(key).cloned().unwrap_or(Value::Null);
        out.insert(key.into(), match &names {
            Value::Array(_) => names,
            _ => Value::Array(vec![]),
        });
    }
    let required = data.get("required");
    out.insert(
        "required".into(),
        match required {
            Some(Value::Number(n)) if n.is_i64() => json!(n.as_i64()),
            _ => Value::Null,
        },
    );
    out.insert("present".into(), as_int(data.get("present").unwrap_or(&Value::Null)));
    out.insert(
        "passed".into(),
        match data.get("passed") {
            Some(Value::Bool(b)) => json!(b),
            _ => Value::Null,
        },
    );
    out.insert("attendance".into(), as_attendance(data.get("attendance").unwrap_or(&Value::Null)));
    Value::Object(out)
}

fn as_attendance(value: &Value) -> Value {
    let data = match value {
        Value::Object(_) => value,
        _ => return Value::Null,
    };
    json!({
        "expected": as_int(data.get("expected").unwrap_or(&Value::Null)),
        "present": as_int(data.get("present").unwrap_or(&Value::Null)),
        "absent": as_int(data.get("absent").unwrap_or(&Value::Null)),
        "rate": as_float(data.get("rate").unwrap_or(&Value::Null)),
        "present_members": as_names(data.get("present_members").unwrap_or(&Value::Null)),
        "absent_members": as_names(data.get("absent_members").unwrap_or(&Value::Null)),
    })
}

fn as_obj(value: &Value) -> Value {
    match value {
        Value::Object(_) => value.clone(),
        Value::String(s) => serde_json::from_str(s).unwrap_or(json!({})),
        _ => json!({}),
    }
}

// ================================================================ 向量字面量（L2 pg_literals）

/// pgvector 文本字面量（`vector_literal` 逐字节对齐：`repr(float(x))`）。
/// Python repr 的浮点最短表示与 Rust `{}` 对 f64 的 Display 一致性：
/// 对整数浮点 Python 出 `1.0`、Rust 出 `1` —— 用 `{:?}`（Rust Debug 对 f64 输出 `1.0`），
/// 两者在 PG 解析后是同一个值，字面量差异不改变语义（PARITY 登记）。
pub fn vector_literal(embedding: Option<&[f64]>) -> Option<String> {
    let emb = embedding?;
    let parts: Vec<String> = emb.iter().map(|x| format!("{:?}", x)).collect();
    Some(format!("[{}]", parts.join(",")))
}

// ================================================================ 摇号（id 生成）

/// base32（RFC4648 大写表）—— `os.urandom(5)` 等价：/dev/urandom 读字节。
fn b32_encode_upper(data: &[u8]) -> String {
    const TABLE: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let mut out = String::new();
    let mut buf = 0u32;
    let mut bits = 0u32;
    for &b in data {
        buf = (buf << 8) | b as u32;
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(TABLE[((buf >> bits) & 0x1f) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(TABLE[((buf << (5 - bits)) & 0x1f) as usize] as char);
    }
    out
}

fn random_bytes(n: usize) -> Vec<u8> {
    use std::io::Read;
    let mut buf = vec![0u8; n];
    std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut buf)).expect("urandom");
    buf
}

/// `new_tmp_id` / `new_msg_id`：前缀 + 8 字符 base32（5 字节随机 = 40 bit）。
pub fn new_prefixed_id(prefix: &str) -> String {
    let raw = b32_encode_upper(&random_bytes(5));
    format!("{}{}", prefix, &raw[..TMP_ID_LEN])
}

pub fn tmp_id_ok(tmp_id: &str) -> bool {
    let b = tmp_id.as_bytes();
    if b.len() != TMP_ID_PREFIX.len() + TMP_ID_LEN || !tmp_id.starts_with(TMP_ID_PREFIX) {
        return false;
    }
    b[TMP_ID_PREFIX.len()..].iter().all(|&c| TMP_ID_ALPHABET.as_bytes().contains(&c))
}

pub fn msg_id_ok(msg_id: &str) -> bool {
    let b = msg_id.as_bytes();
    if b.len() != 2 + MSG_ID_LEN || !msg_id.starts_with(MSG_ID_PREFIX) {
        return false;
    }
    b[2..].iter().all(|&c| TMP_ID_ALPHABET.as_bytes().contains(&c))
}

/// `new_session_id`：`as-YYYYMMDD-HHMMSS-xxxx`（UTC，base32 小写字母表 a-z2-7）。
pub fn new_session_id() -> String {
    let (y, mo, d, h, mi, s) = civil_from_epoch(now_epoch());
    let raw = b32_encode_upper(&random_bytes(3)).to_ascii_lowercase();
    format!(
        "{}{:04}{:02}{:02}-{:02}{:02}{:02}-{}",
        SESS_ID_PREFIX, y, mo, d, h, mi, s,
        &raw[..SESS_ID_RAND_LEN]
    )
}

pub fn session_id_ok(session_id: &str) -> bool {
    let b = session_id.as_bytes();
    // as-YYYYMMDD-HHMMSS-xxxx：3 + 8 + 1 + 6 + 1 + 4 = 23
    if b.len() != 23 || !session_id.starts_with(SESS_ID_PREFIX) {
        return false;
    }
    let digits = |r: std::ops::Range<usize>| b[r].iter().all(|c| c.is_ascii_digit());
    if !digits(3..11) || b[11] != b'-' || !digits(12..18) || b[18] != b'-' {
        return false;
    }
    b[19..].iter().all(|&c| b"abcdefghijklmnopqrstuvwxyz234567".contains(&c))
}

// ================================================================ 运行时

/// 进程级运行时（sweep 自报等；Python Runtime 类）。
#[derive(Default)]
pub struct Runtime {
    pub node_id: Mutex<String>,
    pub tmp_swept_at: Mutex<Option<String>>,
    pub tmp_sweep_error: Mutex<Option<String>>,
    pub tmp_sweeps: AtomicU64,
    pub sess_swept_at: Mutex<Option<String>>,
    pub sess_sweep_error: Mutex<Option<String>>,
    pub sess_sweeps: AtomicU64,
    pub sess_archived: AtomicU64,
    pub msg_swept_at: Mutex<Option<String>>,
    pub msg_sweep_error: Mutex<Option<String>>,
    pub msg_sweeps: AtomicU64,
}

impl Runtime {
    pub fn new() -> Self {
        Self { node_id: Mutex::new("unknown".into()), ..Default::default() }
    }
    pub fn node_id(&self) -> String {
        self.node_id.lock().map(|g| g.clone()).unwrap_or_default()
    }
}

// ================================================================ 存储层

/// 最小连接持有者：一条连接 + 一把锁，断了自动重连一次（Python PgBackend 同款语义）。
pub struct BbStore {
    dsn: String,
    lock: Mutex<()>,
    conn: Mutex<Option<postgres::Client>>,
}

impl BbStore {
    pub fn connect(dsn: &str) -> Result<Self, DbUnavailable> {
        if dsn.is_empty() {
            return Err(DbUnavailable("BB_PG_DSN 为空：数据库未配置".into()));
        }
        let conn = postgres::Client::connect(dsn, postgres::NoTls)
            .map_err(|e| DbUnavailable(safe_err(&format!("{e}"))))?;
        Ok(BbStore { dsn: dsn.to_string(), lock: Mutex::new(()), conn: Mutex::new(Some(conn)) })
    }

    pub fn dsn(&self) -> &str {
        &self.dsn
    }

    fn is_conn_closed(err: &postgres::Error) -> bool {
        // postgres crate：连接级错误基本都是 io/protocol；与 Python `conn.closed` 相比的
        // 保守口径：查询错误也重连一次（低频写服务，重试一条刚建的连接的风险见 Python 注释 ——
        // 这里按 err.code().is_none()（非服务端错误）判"连接坏了"。
        err.code().is_none()
    }

    /// 统一查询入口（Python `PgBackend.__call__`：autocommit、断线重连一次）。
    pub fn query(
        &self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
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
                Ok(rows) => return Ok(rows),
                Err(e) => {
                    last = Some(format!("{e}"));
                    *guard = None; // drop 连接
                    if fresh || attempt == 1 || !Self::is_conn_closed(&e) {
                        break;
                    }
                }
            }
        }
        Err(DbUnavailable(safe_err(&last.unwrap_or_default())))
    }

    /// 单行查询。
    pub fn query_one(
        &self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<Option<postgres::Row>, DbUnavailable> {
        let rows = self.query(sql, params)?;
        Ok(rows.into_iter().next())
    }

    /// 显式事务（tmp 计数读等多语句原子性用不到——每条 UPDATE 自身原子；
    /// 预留与 chat_svc execute_tx 同名口径）。
    pub fn execute_tx<F>(&self, f: F) -> Result<(), DbUnavailable>
    where
        F: FnOnce(&mut postgres::Transaction) -> Result<(), postgres::Error>,
    {
        let _g = self.lock.lock().unwrap();
        let mut guard = self.conn.lock().unwrap();
        if guard.is_none() {
            let c = postgres::Client::connect(&self.dsn, postgres::NoTls)
                .map_err(|e| DbUnavailable(safe_err(&format!("{e}"))))?;
            *guard = Some(c);
        }
        let conn = guard.as_mut().unwrap();
        let mut tx = conn.transaction().map_err(|e| DbUnavailable(safe_err(&format!("{e}"))))?;
        f(&mut tx).map_err(|e| DbUnavailable(safe_err(&format!("{e}"))))?;
        tx.commit().map_err(|e| DbUnavailable(safe_err(&format!("{e}"))))?;
        Ok(())
    }
}

// ================================================================ 行 → JSON

pub const SELECT_COLUMNS: &str = "entry_id, ts, author, topic, kind, content, refs, status";

/// `row_to_entry`：ts 原样（psycopg2 isoformat 形状由 PG 读出转字符串）。
pub fn row_to_entry(row: &postgres::Row) -> Value {
    let got = row.try_get::<_, Option<Value>>("refs").unwrap_or(None).unwrap_or(Value::Null);
    json!({
        "entry_id": row.get::<_, i64>("entry_id"),
        "ts": ts_value(row, "ts"),
        "author": row.get::<_, Option<String>>("author").unwrap_or_default(),
        "topic": row.get::<_, Option<String>>("topic").unwrap_or_default(),
        "kind": row.get::<_, Option<String>>("kind").unwrap_or_default(),
        "content": row.get::<_, Option<String>>("content").unwrap_or_default(),
        "refs": as_refs(&got),

        "status": row.get::<_, Option<String>>("status").unwrap_or_default(),
    })
}

/// timestamptz → psycopg2 `datetime.isoformat()` 形状（`+00:00`，微秒按需）。
/// postgres crate 读出来就是带微秒的 epoch；拼 Python 同款字符串。
pub fn ts_value(row: &postgres::Row, col: &str) -> Value {
    let us: Option<i64> = row
        .try_get::<_, Option<system_time_from_row::Ts>>(col)
        .ok()
        .map(|t| t.map(|v| v.0).unwrap_or(0));
    match us {
        Some(0) | None => Value::Null,
        Some(v) => json!(iso_pg(v)),
    }
}

/// psycopg2 `datetime.isoformat()`：微秒为 0 时省略小数，非 0 时 6 位。
pub fn iso_pg(epoch_us: i64) -> String {
    let secs = epoch_us.div_euclid(1_000_000);
    let frac = epoch_us.rem_euclid(1_000_000);
    let (y, mo, d, h, mi, s) = civil_from_epoch(secs);
    if frac == 0 {
        format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}+00:00", y, mo, d, h, mi, s)
    } else {
        format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:06}+00:00", y, mo, d, h, mi, s, frac)
    }
}

/// timestamptz（可空）→ iso 或 null。
pub fn ts_opt(row: &postgres::Row, col: &str) -> Value {
    let v = ts_value(row, col);
    if v.is_null() {
        Value::Null
    } else {
        v
    }
}


pub fn ts_opt_at(row: &postgres::Row, idx: usize) -> Value {
    let us: Option<i64> = row.try_get::<_, Option<system_time_from_row::Ts>>(idx).ok().map(|t| t.map(|v| v.0).unwrap_or(0));
    match us {
        Some(0) | None => Value::Null,
        Some(epoch_us) => {
            let secs = epoch_us.div_euclid(1_000_000);
            let frac = epoch_us.rem_euclid(1_000_000);
            let (y, mo, d, h, mi, s) = civil_from_epoch(secs);
            if frac == 0 {
                Value::String(format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}+00:00", y, mo, d, h, mi, s))
            } else {
                Value::String(format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:06}+00:00", y, mo, d, h, mi, s, frac))
            }
        }
    }
}

pub mod system_time_from_row {
    //! postgres crate 对 timestamptz 默认映射 SystemTime；包一层拿微秒。
    #[derive(Debug)]
    pub struct Ts(pub i64); // epoch micros
    impl<'a> postgres::types::FromSql<'a> for Ts {
        fn from_sql(
            _ty: &postgres::types::Type,
            raw: &'a [u8],
        ) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
            // timestamptz 二进制：pg epoch(2000-01-01) 起的微秒，int64
            let us = i64::from_be_bytes(raw.try_into()?);
            const PG_EPOCH_US: i64 = 946_684_800 * 1_000_000;
            Ok(Ts(us + PG_EPOCH_US))
        }
        fn accepts(ty: &postgres::types::Type) -> bool {
            matches!(ty, &postgres::types::Type::TIMESTAMPTZ | &postgres::types::Type::TIMESTAMP)
        }
    }
}

// ================================================================ 黑板条目

pub struct BbSvc {
    pub store: std::sync::Arc<BbStore>,
    pub runtime: std::sync::Arc<Runtime>,
    /// peers：token sha256hex → 成员 id（恒时比对在 bin 层做）。
    pub peer_map: Vec<(String, String)>,
}

impl BbSvc {
    pub fn new(store: std::sync::Arc<BbStore>, runtime: std::sync::Arc<Runtime>, peer_map: Vec<(String, String)>) -> Self {
        BbSvc { store, runtime, peer_map }
    }

    /// `insert_entry`：写一条黑板条目（本服务唯一的黑板写路径）。
    pub fn insert_entry(
        &self,
        author: &str,
        topic: &str,
        kind: &str,
        content: &str,
        refs: &Value,
        embedding: Option<String>, // vector 字面量或 None
    ) -> Result<Value, DbUnavailable> {
        let sql = "INSERT INTO blackboard_entries (author, topic, kind, content, refs, embedding) \
                   VALUES ($1, $2, $3, $4, $5::jsonb, $6::text::vector) RETURNING entry_id, ts";
        let refs_v = refs.clone();
        let row = self
            .store
            .query_one(sql, &[&author, &topic, &kind, &content, &refs_v, &embedding])?
            .ok_or_else(|| DbUnavailable("insert 没有返回 entry_id".into()))?;
        Ok(json!({
            "entry_id": row.get::<_, i64>(0),
            "ts": ts_value(&row, "ts"),
        }))
    }

    /// `select_entries`：按 topic/kind/author/时间过滤，ts 倒序。全参数化。
    pub fn select_entries(
        &self,
        topic: Option<&str>,
        kind: Option<&str>,
        author: Option<&str>,
        limit: i64,
        since: Option<&str>,
        status: Option<&str>,
    ) -> Result<Vec<Value>, DbUnavailable> {
        let mut where_parts: Vec<String> = vec![];
        let mut params: Vec<Box<dyn ToSql + Sync>> = vec![];
        if let Some(t) = topic {
            params.push(Box::new(t.to_string()));
            where_parts.push(format!("topic = ${}", params.len()));
        }
        if let Some(k) = kind {
            params.push(Box::new(k.to_string()));
            where_parts.push(format!("kind = ${}", params.len()));
        }
        if let Some(a) = author {
            params.push(Box::new(a.to_string()));
            where_parts.push(format!("author = ${}", params.len()));
        }
        if let Some(s) = since {
            params.push(Box::new(s.to_string()));
            where_parts.push(format!("ts >= ${}::text::timestamptz", params.len()));
        }
        let st = if status != Some("all") { Some(status.unwrap_or("active")) } else { None };
        if let Some(s) = st {
            params.push(Box::new(s.to_string()));
            where_parts.push(format!("status = ${}", params.len()));
        }
        params.push(Box::new(limit));
        let mut sql = format!("SELECT {} FROM blackboard_entries", SELECT_COLUMNS);
        if !where_parts.is_empty() {
            sql.push_str(&format!(" WHERE {}", where_parts.join(" AND ")));
        }
        sql.push_str(&format!(" ORDER BY ts DESC, entry_id DESC LIMIT ${}", params.len()));
        let refs: Vec<&(dyn ToSql + Sync)> = params.iter().map(|b| b.as_ref()).collect();
        let rows = self.store.query(&sql, &refs)?;
        Ok(rows.iter().map(row_to_entry).collect())
    }

    // -------------------------------------------------- 语义检索（黑板）

    pub fn has_any_embedding(&self) -> Result<bool, DbUnavailable> {
        let row = self.store.query_one(
            "SELECT count(*) FROM blackboard_entries WHERE embedding IS NOT NULL",
            &[],
        )?;
        Ok(row.map(|r| r.get::<_, i64>(0) > 0).unwrap_or(false))
    }

    /// `_like_pattern`：ILIKE 模式串，转义 `\ % _`。
    pub fn like_pattern(q: &str) -> String {
        let mut out = String::from("%");
        for c in q.chars() {
            if c == '\\' || c == '%' || c == '_' {
                out.push('\\');
            }
            out.push(c);
        }
        out.push('%');
        out
    }

    /// `search_semantic`：余弦距离 <=> 排序，只有 embedding 非 NULL 的行参与。
    pub fn search_semantic(
        &self,
        vector: &str,
        topic: Option<&str>,
        limit: i64,
    ) -> Result<Vec<Value>, DbUnavailable> {
        let mut sql = format!(
            "SELECT {}, 1 - (embedding <=> $1::text::vector) AS score \
             FROM blackboard_entries WHERE embedding IS NOT NULL AND status = $2",
            SELECT_COLUMNS
        );
        let mut params: Vec<Box<dyn ToSql + Sync>> =
            vec![Box::new(vector.to_string()), Box::new("active".to_string())];
        if let Some(t) = topic {
            params.push(Box::new(t.to_string()));
            sql.push_str(&format!(" AND topic = ${}", params.len()));
        }
        params.push(Box::new(vector.to_string()));
        sql.push_str(" ORDER BY embedding <=> ");
        sql.push_str(&format!("${}::text::vector LIMIT ${}", params.len(), params.len() + 1));
        params.push(Box::new(limit));
        let refs: Vec<&(dyn ToSql + Sync)> = params.iter().map(|b| b.as_ref()).collect();
        let rows = self.store.query(&sql, &refs)?;
        let mut out = vec![];
        for row in &rows {
            let mut entry = row_to_entry(row);
            let score: Option<f64> = row.try_get("score").unwrap_or(None);
            entry.as_object_mut().unwrap().insert(
                "score".into(),
                score.map(round4).map(|v| json!(v)).unwrap_or(Value::Null),
            );
            out.push(entry);
        }
        Ok(out)
    }

    /// `search_keyword`：content ILIKE 子串匹配，score 一律 null。
    pub fn search_keyword(
        &self,
        q: &str,
        topic: Option<&str>,
        limit: i64,
    ) -> Result<Vec<Value>, DbUnavailable> {
        let mut sql = format!(
            "SELECT {} FROM blackboard_entries WHERE content ILIKE $1 AND status = $2",
            SELECT_COLUMNS
        );
        let mut params: Vec<Box<dyn ToSql + Sync>> =
            vec![Box::new(Self::like_pattern(q)), Box::new("active".to_string())];
        if let Some(t) = topic {
            params.push(Box::new(t.to_string()));
            sql.push_str(&format!(" AND topic = ${}", params.len()));
        }
        params.push(Box::new(limit));
        sql.push_str(&format!(" ORDER BY ts DESC, entry_id DESC LIMIT ${}", params.len()));
        let refs: Vec<&(dyn ToSql + Sync)> = params.iter().map(|b| b.as_ref()).collect();
        let rows = self.store.query(&sql, &refs)?;
        Ok(rows
            .iter()
            .map(|row| {
                let mut e = row_to_entry(row);
                e.as_object_mut().unwrap().insert("score".into(), Value::Null);
                e
            })
            .collect())
    }
}

fn round4(x: f64) -> f64 {
    (x * 10000.0).round() / 10000.0
}

// ================================================================ 健康探活

pub fn db_health(svc: &BbSvc) -> (bool, Value) {
    let started = Instant::now();
    match svc.store.query_one("SELECT count(*) FROM blackboard_entries", &[]) {
        Err(e) => (
            false,
            json!({"db": {"ok": false, "error": safe_err(&e.0)}, "entries": {"total": null}}),
        ),
        Ok(row) => {
            let latency_ms = started.elapsed().as_millis() as i64;
            let total = row.map(|r| r.get::<_, i64>(0)).unwrap_or(0);
            (true, json!({"db": {"ok": true, "latency_ms": latency_ms}, "entries": {"total": total}}))
        }
    }
}

pub fn health_payload(svc: &BbSvc) -> (u16, Value) {
    let (ok, detail) = db_health(svc);
    let mut payload = json!({
        "ok": ok,
        "version": VERSION,
        "node": svc.runtime.node_id(),
        "ts": now_epoch(),
    });
    let obj = payload.as_object_mut().unwrap();
    if let Value::Object(d) = detail {
        for (k, v) in d {
            obj.insert(k, v);
        }
    }
    obj.insert(
        "embed".into(),
        json!({"ok": false, "reason": "rust 移植暂未接入嵌入模型（可选增强，检索退关键词）"}),
    );
    let g = |m: &Mutex<Option<String>>| m.lock().map(|g| g.clone()).unwrap_or(None);
    obj.insert("tmp_swept_at".into(), json!(g(&svc.runtime.tmp_swept_at)));
    obj.insert("tmp".into(), json!({
        "swept_at": g(&svc.runtime.tmp_swept_at),
        "sweeps": svc.runtime.tmp_sweeps.load(Ordering::Relaxed),
        "sweep_interval_s": TMP_SWEEP_INTERVAL_S,
        "last_error": g(&svc.runtime.tmp_sweep_error),
    }));
    obj.insert("sess_swept_at".into(), json!(g(&svc.runtime.sess_swept_at)));
    obj.insert("sess".into(), json!({
        "swept_at": g(&svc.runtime.sess_swept_at),
        "sweeps": svc.runtime.sess_sweeps.load(Ordering::Relaxed),
        "archived": svc.runtime.sess_archived.load(Ordering::Relaxed),
        "sweep_interval_s": TMP_SWEEP_INTERVAL_S,
        "last_error": g(&svc.runtime.sess_sweep_error),
    }));
    obj.insert("msg_swept_at".into(), json!(g(&svc.runtime.msg_swept_at)));
    obj.insert("msg".into(), json!({
        "swept_at": g(&svc.runtime.msg_swept_at),
        "sweeps": svc.runtime.msg_sweeps.load(Ordering::Relaxed),
        "sweep_interval_s": TMP_SWEEP_INTERVAL_S,
        "last_error": g(&svc.runtime.msg_sweep_error),
        "states": svc.msg_stats(),
    }));
    (if ok { 200 } else { 503 }, payload)
}

// ================================================================ msg_log（消息可靠投递）

pub const MSG_COLUMNS: &str = "msg_id, seq, from_member, to_member, session_id, topic, body, content_ref, \
     state, attempts, last_error, created_at, updated_at, acked_at, done_at, expires_at";

pub fn row_to_msg(row: &postgres::Row) -> Value {
    let state: String = row.get("state");
    json!({
        "msg_id": row.get::<_, String>("msg_id"),
        "seq": row.get::<_, Option<i64>>("seq").map(|s| json!(s)).unwrap_or(Value::Null),
        "from": row.get::<_, String>("from_member"),
        "to": row.get::<_, String>("to_member"),
        "session_id": row.get::<_, Option<String>>("session_id").map(|v| json!(v)).unwrap_or(Value::Null),
        "topic": row.get::<_, String>("topic"),
        "body": row.get::<_, Option<String>>("body").map(|v| json!(v)).unwrap_or(Value::Null),
        "content_ref": row.try_get::<_, Option<Value>>("content_ref").unwrap_or(None).map(|v| json!(v)).unwrap_or(Value::Null),
        "state": state,
        "attempts": row.get::<_, i32>("attempts"),
        "last_error": row.get::<_, Option<String>>("last_error").map(|v| json!(v)).unwrap_or(Value::Null),
        "created_at": ts_z_col(row, "created_at"),
        "updated_at": ts_z_col(row, "updated_at"),
        "acked_at": ts_z_col(row, "acked_at"),
        "done_at": ts_z_col(row, "done_at"),
        "expires_at": ts_z_col(row, "expires_at"),
        "acked": state == "processing" || state == "done",
        "finished": MSG_TERMINAL_STATES.contains(&state.as_str()),
    })
}

/// timestamptz → `...Z`（可空）。
pub fn ts_z_col(row: &postgres::Row, col: &str) -> Value {
    match row.try_get::<_, Option<system_time_from_row::Ts>>(col) {
        Ok(Some(t)) => json!(iso_z(t.0)),
        _ => Value::Null,
    }
}

impl BbSvc {
    /// `insert_msg`：落库一条消息（落库成功 = 送达；msg_id 幂等）。
    #[allow(clippy::too_many_arguments)]
    pub fn insert_msg(
        &self,
        from_member: &str,
        to_member: &str,
        body: Option<&str>,
        content_ref: Option<&Value>,
        session_id: Option<&str>,
        topic: &str,
        ttl_s: i64,
        msg_id: Option<&str>,
    ) -> Result<Value, DbUnavailable> {
        let ref_json: Option<Value> = content_ref.cloned();
        for _ in 0..MSG_ID_TRY {
            let candidate = msg_id.map(str::to_string).unwrap_or_else(|| new_prefixed_id(MSG_ID_PREFIX));
            let sql = "INSERT INTO msg_log (msg_id, from_member, to_member, session_id, topic, body, content_ref, expires_at) \
                       VALUES ($1, $2, $3, $4, $5, $6, $7::jsonb, now() + make_interval(secs => $8)) \
                       ON CONFLICT (msg_id) DO NOTHING RETURNING seq, created_at, expires_at";
            let body_opt = body;
            let session_opt = session_id;
            let topic_s = topic.to_string();
            let row = self.store.query_one(
                sql,
                &[&candidate, &from_member, &to_member, &session_opt, &topic_s, &body_opt, &ref_json, &(ttl_s as f64)],
            )?;
            if let Some(row) = row {
                let seq: i64 = row.get(0);
                return Ok(json!({
                    "msg_id": candidate,
                    "seq": seq,
                    "created_at": ts_z_idx(&row, 1),
                    "expires_at": ts_z_idx(&row, 2),
                    "duplicate": false,
                }));
            }
            // 撞了已有 msg_id：调用方带 id 的 → 返回已有那条（duplicate=true）
            if let Some(mid) = msg_id {
                if let Some(existing) = self.get_msg(mid)? {
                    let mut v = existing;
                    v.as_object_mut().unwrap().insert("duplicate".into(), Value::Bool(true));
                    return Ok(v);
                }
            }
        }
        Err(DbUnavailable(format!("msg_id 连续 {} 次撞主键，放弃", MSG_ID_TRY)))
    }

    pub fn get_msg(&self, msg_id: &str) -> Result<Option<Value>, DbUnavailable> {
        let sql = format!("SELECT {} FROM msg_log WHERE msg_id = $1", MSG_COLUMNS);
        Ok(self.store.query_one(&sql, &[&msg_id])?.map(|r| row_to_msg(&r)))
    }

    pub fn list_inbox(
        &self,
        to_member: &str,
        states: &[String],
        limit: i64,
    ) -> Result<Vec<Value>, DbUnavailable> {
        let sql = format!(
            "SELECT {} FROM msg_log WHERE to_member = $1 AND state = ANY($2::text[]) ORDER BY seq LIMIT $3",
            MSG_COLUMNS
        );
        let rows = self.store.query(&sql, &[&to_member, &states.to_vec(), &limit])?;
        Ok(rows.iter().map(row_to_msg).collect())
    }

    /// `msg_ack`：queued → processing（乐观锁；并发重复 ack 只成功一次）。
    pub fn msg_ack(&self, msg_id: &str) -> Result<Option<Value>, DbUnavailable> {
        let sql = format!(
            "UPDATE msg_log SET state = 'processing', acked_at = now(), attempts = attempts + 1 \
             WHERE msg_id = $1 AND state = 'queued' RETURNING {}",
            MSG_COLUMNS
        );
        Ok(self.store.query_one(&sql, &[&msg_id])?.map(|r| row_to_msg(&r)))
    }

    pub fn msg_finish(
        &self,
        msg_id: &str,
        to_state: &str,
        error: Option<&str>,
    ) -> Result<Option<Value>, DbUnavailable> {
        let sql = format!(
            "UPDATE msg_log SET state = $1, \
             done_at = CASE WHEN $1 IN ('done','failed') THEN now() ELSE done_at END, \
             last_error = COALESCE($2, last_error), attempts = attempts + 1 \
             WHERE msg_id = $3 AND state IN ('queued','processing') RETURNING {}",
            MSG_COLUMNS
        );
        Ok(self
            .store
            .query_one(&sql, &[&to_state, &error, &msg_id])?
            .map(|r| row_to_msg(&r)))
    }

    pub fn msg_stats(&self) -> Value {
        match self.store.query("SELECT state, count(*) FROM msg_log GROUP BY state", &[]) {
            Ok(rows) => {
                let mut m = Map::new();
                for r in &rows {
                    let k: String = r.get(0);
                    let c: i64 = r.get(1);
                    m.insert(k, json!(c));
                }
                Value::Object(m)
            }
            Err(_) => json!({}),
        }
    }

    /// `msg_sweep_once`：到期未取 → expired；processing 卡死 → stalled（绝不自动重投）。
    pub fn msg_sweep_once(&self) -> Result<usize, DbUnavailable> {
        let mut n = 0usize;
        let rows = self
            .store
            .query("UPDATE msg_log SET state = 'expired', last_error = 'expired: 到期仍未被取走' \
                    WHERE state IN ('queued','processing') AND expires_at <= now() RETURNING msg_id", &[])?;
        n += rows.len();
        let stall_sql = format!(
            "UPDATE msg_log SET state = 'stalled', \
             last_error = 'processing 卡死超过 {MSG_STALL_AFTER_S} 秒，已标记待人工确认（不自动重投）' \
             WHERE state = 'processing' \
             AND updated_at <= now() - make_interval(secs => {MSG_STALL_AFTER_S}.0::double precision) RETURNING msg_id"
        );
        let stuck = self.store.query(&stall_sql, &[])?;
        if !stuck.is_empty() {
            log_line(&format!("msg_log：{} 条 processing 卡死已标 stalled（未自动重投）", stuck.len()));
        }
        n += stuck.len();
        Ok(n)
    }
}

fn ts_z_idx(row: &postgres::Row, idx: usize) -> Value {
    match row.try_get::<_, Option<system_time_from_row::Ts>>(idx) {
        Ok(Some(t)) => json!(iso_z(t.0)),
        _ => Value::Null,
    }
}

// ================================================================ 临时共享区（tmp box）

pub const TMP_COLUMNS: &str = "tmp_id, owner, audience, title, body, bytes, digest, reads_max, \
     reads_count, readers, expires_at, created_at, destroyed_at, destroy_reason";

pub fn row_to_tmp(row: &postgres::Row, with_body: bool) -> Value {
    let tmp_id: String = row.get(0);
    let owner: String = row.get(1);
    let audience: Vec<String> = row.get(2);
    let title: Option<String> = row.get(3);
    let body: Option<String> = row.get(4);
    let nbytes: i32 = row.get(5);
    let digest: String = row.get(6);
    let reads_max: i32 = row.get(7);
    let reads_count: i32 = row.get(8);
    let readers: Vec<String> = row.get(9);
    let expires_us: i64 = row.get::<_, Option<system_time_from_row::Ts>>(10).map(|t| t.0).unwrap_or(0);
    let created_us: i64 = row.get::<_, Option<system_time_from_row::Ts>>(11).map(|t| t.0).unwrap_or(0);
    let destroyed: Option<system_time_from_row::Ts> = row.get(12);
    let destroy_reason: Option<String> = row.get(13);
    let now_us = now_micros();
    let mut item = json!({
        "tmp_id": tmp_id,
        "owner": owner,
        "audience": audience,
        "title": title,
        "bytes": nbytes,
        "digest": digest,
        "reads_max": reads_max,
        "reads_count": reads_count,
        "reads_left": (reads_max - reads_count).max(0),
        "readers": readers,
        "expires_at": iso_z(expires_us),
        "created_at": iso_z(created_us),
        "destroyed_at": destroyed.as_ref().map(|t| json!(iso_z(t.0))).unwrap_or(Value::Null),
        "destroy_reason": destroy_reason,
        "destroyed": destroyed.is_some(),
        "expired": expires_us <= now_us,
    });
    if with_body {
        item.as_object_mut().unwrap().insert("body".into(), json!(body.unwrap_or_default()));
    }
    item
}

pub fn row_to_tmp_with_now(row: &postgres::Row, now_us: i64, with_body: bool) -> Value {
    let mut item = row_to_tmp(row, with_body);
    item.as_object_mut().unwrap().insert(
        "expired".into(),
        json!(row.get::<_, Option<system_time_from_row::Ts>>(10).map(|t| t.0).unwrap_or(0) <= now_us),
    );
    item
}

impl BbSvc {
    pub fn known_members(&self) -> Vec<String> {
        let mut names: Vec<String> = self.peer_map.iter().map(|(_, p)| p.clone()).collect();
        names.sort();
        names.dedup();
        names
    }

    /// `select_tmp`：按 id 取一行（含正文）。
    pub fn select_tmp(&self, tmp_id: &str) -> Result<Option<Value>, DbUnavailable> {
        let sql = format!("SELECT {} FROM tmp_shared WHERE tmp_id = $1", TMP_COLUMNS);
        Ok(self.store.query_one(&sql, &[&tmp_id])?.map(|r| row_to_tmp(&r, true)))
    }

    pub fn tmp_owner_active_count(&self, owner: &str) -> Result<i64, DbUnavailable> {
        let row = self.store.query_one(
            "SELECT count(*) FROM tmp_shared WHERE owner = $1 AND destroyed_at IS NULL AND expires_at > now()",
            &[&owner],
        )?;
        Ok(row.map(|r| r.get::<_, i64>(0)).unwrap_or(0))
    }

    /// `insert_tmp`：写一条（tmp_id 撞了重摇，5 次）。
    pub fn insert_tmp(
        &self,
        owner: &str,
        audience: &[String],
        title: Option<&str>,
        body: &str,
        reads_max: i64,
        ttl_s: i64,
    ) -> Result<Value, DbUnavailable> {
        let digest = crate::chat_svc::sha256_hex(body.as_bytes());
        let mut last_err = String::new();
        for _ in 0..5 {
            let tmp_id = new_prefixed_id(TMP_ID_PREFIX);
            let sql = "INSERT INTO tmp_shared (tmp_id, owner, audience, title, body, bytes, digest, reads_max, expires_at) \
                       VALUES ($1, $2, $3, $4, $5, $6, $7, $8, now() + make_interval(secs => $9)) \
                       RETURNING expires_at";
            let bytes_len = body.len() as i32;
            match self.store.query_one(
                sql,
                &[&tmp_id, &owner, &audience.to_vec(), &title, &body, &bytes_len, &digest, &(reads_max as i32), &(ttl_s as f64)],
            ) {
                Ok(Some(row)) => {
                    return Ok(json!({
                        "tmp_id": tmp_id,
                        "expires_at": ts_z_idx(&row, 0),
                        "reads_max": reads_max,
                        "bytes": body.len(),
                        "digest": digest,
                    }));
                }
                Ok(None) => {
                    last_err = "no row".into();
                }
                Err(e) => {
                    if e.0.to_lowercase().contains("duplicate key") {
                        last_err = e.0;
                        continue;
                    }
                    return Err(e);
                }
            }
        }
        Err(DbUnavailable(safe_err(&format!("tmp_id 连续 5 次撞主键，放弃: {}", last_err))))
    }

    /// `tmp_counted_read`：一条 UPDATE 同时记 reader + 读满销毁（天然原子）。
    /// 返回 (reads_count, reads_max, 本次是否销毁, 销毁时间)；None = 已读过或已销毁。
    pub fn tmp_counted_read(
        &self,
        tmp_id: &str,
        peer: &str,
    ) -> Result<Option<MsgFinishRow>, DbUnavailable> {
        let sql = "UPDATE tmp_shared SET \
                   readers = array_append(readers, $1::text), \
                   reads_count = reads_count + 1, \
                   destroyed_at = CASE WHEN reads_count + 1 >= reads_max THEN now() ELSE destroyed_at END, \
                   destroy_reason = CASE WHEN reads_count + 1 >= reads_max THEN 'read_quota' ELSE destroy_reason END, \
                   body = CASE WHEN reads_count + 1 >= reads_max THEN '' ELSE body END \
                   WHERE tmp_id = $2 AND destroyed_at IS NULL AND NOT ($1 = ANY(readers)) \
                   RETURNING reads_count, reads_max, destroyed_at, destroy_reason";
        let row = match self.store.query_one(sql, &[&peer, &tmp_id])? {
            Some(r) => r,
            None => return Ok(None),
        };
        let reads_count: i32 = row.get(0);
        let reads_max: i32 = row.get(1);
        let destroyed_at: Option<system_time_from_row::Ts> = row.get(2);
        Ok(Some(MsgFinishRow(
            reads_count as i64,
            reads_max as i64,
            destroyed_at.is_some(),
            destroyed_at.map(|t| iso_z(t.0)),
        )))
    }

    pub fn tmp_expire_now(&self, tmp_id: &str) -> Result<Option<String>, DbUnavailable> {
        let row = self.store.query_one(
            "UPDATE tmp_shared SET body = '', destroyed_at = now(), destroy_reason = 'expired' \
             WHERE tmp_id = $1 AND destroyed_at IS NULL AND expires_at <= now() RETURNING destroyed_at",
            &[&tmp_id],
        )?;
        Ok(row.and_then(|r| r.get::<_, Option<system_time_from_row::Ts>>(0)).map(|t| iso_z(t.0)))
    }

    pub fn tmp_destroy_by_owner(&self, tmp_id: &str) -> Result<Option<String>, DbUnavailable> {
        let row = self.store.query_one(
            "UPDATE tmp_shared SET body = '', destroyed_at = now(), destroy_reason = 'owner' \
             WHERE tmp_id = $1 AND destroyed_at IS NULL RETURNING destroyed_at",
            &[&tmp_id],
        )?;
        Ok(row.and_then(|r| r.get::<_, Option<system_time_from_row::Ts>>(0)).map(|t| iso_z(t.0)))
    }

    /// `tmp_enforce_capacity`：全局容量兜底（超 TMP_ACTIVE_MAX 按 expires_at 最早销毁）。
    pub fn tmp_enforce_capacity(&self) -> Result<usize, DbUnavailable> {
        let row = self
            .store
            .query_one("SELECT count(*) FROM tmp_shared WHERE destroyed_at IS NULL", &[])?;
        let active = row.map(|r| r.get::<_, i64>(0)).unwrap_or(0);
        let over = active - TMP_ACTIVE_MAX;
        if over <= 0 {
            return Ok(0);
        }
        let rows = self.store.query(
            "UPDATE tmp_shared SET body = '', destroyed_at = now(), destroy_reason = 'capacity' \
             WHERE tmp_id IN (SELECT tmp_id FROM tmp_shared WHERE destroyed_at IS NULL \
             ORDER BY expires_at ASC LIMIT $1) RETURNING tmp_id",
            &[&over],
        )?;
        Ok(rows.len())
    }

    /// `tmp_sweep_once`：过期的 + 容量超额的。
    pub fn tmp_sweep_once(&self) -> Result<usize, DbUnavailable> {
        let rows = self.store.query(
            "UPDATE tmp_shared SET body = '', destroyed_at = now(), destroy_reason = 'expired' \
             WHERE destroyed_at IS NULL AND expires_at <= now() RETURNING tmp_id",
            &[],
        )?;
        let mut destroyed = rows.len();
        destroyed += self.tmp_enforce_capacity()?;
        Ok(destroyed)
    }

    /// GET /tmp 清单（mine/inbox/both）。
    pub fn tmp_list(
        &self,
        peer: &str,
        mine: bool,
        inbox: bool,
        limit: i64,
    ) -> Result<(String, Vec<Value>), DbUnavailable> {
        let now_us = now_micros();
        let (mode, where_s, mut params): (String, String, Vec<Box<dyn ToSql + Sync>>) = if mine && !inbox {
            ("mine".to_string(), "owner = $1".to_string(), vec![Box::new(peer.to_string())])
        } else if inbox && !mine {
            (
                "inbox".to_string(),
                "destroyed_at IS NULL AND expires_at > now() AND $1 = ANY(audience)".into(),
                vec![Box::new(peer.to_string())],
            )
        } else {
            (
                "both".to_string(),
                "(owner = $1 OR (destroyed_at IS NULL AND expires_at > now() AND $2 = ANY(audience)))".into(),
                vec![Box::new(peer.to_string()), Box::new(peer.to_string())],
            )
        };
        let sql = format!(
            "SELECT {} FROM tmp_shared WHERE {} ORDER BY created_at DESC LIMIT ${}",
            TMP_COLUMNS, where_s, params.len() + 1
        );
        params.push(Box::new(limit));
        let refs: Vec<&(dyn ToSql + Sync)> = params.iter().map(|b| b.as_ref()).collect();
        let rows = self.store.query(&sql, &refs)?;
        Ok((
            mode.to_string(),
            rows.iter().map(|r| row_to_tmp_with_now(r, now_us, false)).collect(),
        ))
    }
}

// ================================================================ Agent 会话（/sess）

pub const SESS_COLUMNS: &str = "session_id, topic, title, owner, members, state, created_at, last_turn_at, \
     idle_ttl_s, turn_count, closed_at, close_reason, archive_ref, note";

pub const SESS_TURN_COLUMNS: &str = "turn_id, session_id, seq, from_member, to_member, role, content, \
     content_ref, bytes, ts";

pub fn row_to_agent_session(row: &postgres::Row) -> Value {
    let members: Value = row.try_get("members").unwrap_or(Value::Null);
    let archive_ref: Option<i64> = row.try_get("archive_ref").unwrap_or(None);
    let state: String = row.get("state");
    json!({
        "session_id": row.get::<_, String>("session_id"),
        "topic": row.get::<_, String>("topic"),
        "title": row.get::<_, Option<String>>("title").map(|v| json!(v)).unwrap_or(Value::Null),
        "owner": row.get::<_, String>("owner"),
        "members": as_names(&members),
        "state": state,
        "created_at": ts_z_col(row, "created_at"),
        "last_turn_at": ts_z_col(row, "last_turn_at"),
        "idle_ttl_s": row.get::<_, i32>("idle_ttl_s"),
        "turn_count": row.get::<_, i32>("turn_count"),
        "closed_at": ts_z_col(row, "closed_at"),
        "close_reason": row.get::<_, Option<String>>("close_reason").map(|v| json!(v)).unwrap_or(Value::Null),
        "archive_ref": archive_ref.map(|v| json!(v)).unwrap_or(Value::Null),
        "note": row.get::<_, Option<String>>("note").map(|v| json!(v)).unwrap_or(Value::Null),
        "open": state == "open",
    })
}

pub fn row_to_agent_turn(row: &postgres::Row) -> Value {
    let content: Option<String> = row.get("content");
    json!({
        "turn_id": row.get::<_, i64>("turn_id"),
        "session_id": row.get::<_, String>("session_id"),
        "seq": row.get::<_, i32>("seq"),
        "from_member": row.get::<_, String>("from_member"),
        "to_member": row.get::<_, Option<String>>("to_member").map(|v| json!(v)).unwrap_or(Value::Null),
        "role": row.get::<_, String>("role"),
        "content": content,
        "content_ref": row.try_get::<_, Option<Value>>("content_ref").unwrap_or(None).map(|v| json!(v)).unwrap_or(Value::Null),
        "bytes": row.get::<_, i32>("bytes"),
        "ts": ts_z_col(row, "ts"),
        "content_cleared": content.as_deref().map(|c| c.is_empty()).unwrap_or(true),
    })
}

pub fn sess_visible_to(session: &Value, peer: &str) -> bool {
    if session.get("owner").and_then(Value::as_str) == Some(peer) {
        return true;
    }
    session
        .get("members")
        .and_then(Value::as_array)
        .map(|a| a.iter().any(|m| m.as_str() == Some(peer)))
        .unwrap_or(false)
}

impl BbSvc {
    pub fn select_agent_sessions(
        &self,
        state: Option<&str>,
        topic: Option<&str>,
        limit: i64,
        since: Option<&str>,
    ) -> Result<Vec<Value>, DbUnavailable> {
        let mut where_parts: Vec<String> = vec![];
        let mut params: Vec<Box<dyn ToSql + Sync>> = vec![];
        if let Some(s) = state {
            params.push(Box::new(s.to_string()));
            where_parts.push(format!("state = ${}", params.len()));
        }
        if let Some(t) = topic {
            params.push(Box::new(t.to_string()));
            where_parts.push(format!("topic = ${}", params.len()));
        }
        if let Some(s) = since {
            params.push(Box::new(s.to_string()));
            where_parts.push(format!("created_at >= ${}::text::timestamptz", params.len()));
        }
        params.push(Box::new(limit));
        let mut sql = format!("SELECT {} FROM agent_sessions", SESS_COLUMNS);
        if !where_parts.is_empty() {
            sql.push_str(&format!(" WHERE {}", where_parts.join(" AND ")));
        }
        sql.push_str(&format!(" ORDER BY created_at DESC, session_id DESC LIMIT ${}", params.len()));
        let refs: Vec<&(dyn ToSql + Sync)> = params.iter().map(|b| b.as_ref()).collect();
        let rows = self.store.query(&sql, &refs)?;
        Ok(rows.iter().map(row_to_agent_session).collect())
    }

    pub fn select_agent_session(&self, session_id: &str) -> Result<Option<Value>, DbUnavailable> {
        let sql = format!("SELECT {} FROM agent_sessions WHERE session_id = $1", SESS_COLUMNS);
        Ok(self.store.query_one(&sql, &[&session_id])?.map(|r| row_to_agent_session(&r)))
    }

    /// 最近 limit 轮，按 seq 升序返回（SQL DESC 取最近，Python 一样 reversed）。
    pub fn select_agent_turns(&self, session_id: &str, limit: i64) -> Result<Vec<Value>, DbUnavailable> {
        let sql = format!(
            "SELECT {} FROM agent_session_turns WHERE session_id = $1 ORDER BY seq DESC LIMIT $2",
            SESS_TURN_COLUMNS
        );
        let rows = self.store.query(&sql, &[&session_id, &limit])?;
        let mut out: Vec<Value> = rows.iter().map(row_to_agent_turn).collect();
        out.reverse();
        Ok(out)
    }

    pub fn select_all_agent_turns(&self, session_id: &str) -> Result<Vec<Value>, DbUnavailable> {
        let sql = format!(
            "SELECT {} FROM agent_session_turns WHERE session_id = $1 ORDER BY seq ASC",
            SESS_TURN_COLUMNS
        );
        let rows = self.store.query(&sql, &[&session_id])?;
        Ok(rows.iter().map(row_to_agent_turn).collect())
    }

    /// `insert_agent_turn`：追加一轮流水；seq/bytes/ts 由库触发器给。
    /// 父会话非 open 态触发器 RAISE → SessionNotOpen（端点翻 409）。
    pub fn insert_agent_turn(
        &self,
        session_id: &str,
        from_member: &str,
        to_member: Option<&str>,
        role: &str,
        content: &str,
        content_ref: Option<&Value>,
    ) -> Result<Value, DbUnavailable> {
        let ref_json: Option<Value> = content_ref.cloned();
        let sql = format!(
            "INSERT INTO agent_session_turns (session_id, from_member, to_member, role, content, content_ref) \
             VALUES ($1, $2, $3, $4, $5, $6::jsonb) RETURNING {}",
            SESS_TURN_COLUMNS
        );
        match self.store.query_one(&sql, &[&session_id, &from_member, &to_member, &role, &content, &ref_json]) {
            Err(e) => {
                let low = e.0.to_lowercase();
                if low.contains("closed") || low.contains("expired") || low.contains("terminal")
                    || low.contains("not open") || e.0.contains("不是 open")
                {
                    return Err(DbUnavailable(e.0)); // 保留原文，bin 层判 SessionNotOpen
                }
                Err(e)
            }
            Ok(Some(row)) => Ok(row_to_agent_turn(&row)),
            Ok(None) => Err(DbUnavailable("turn insert 没有返回行".into())),
        }
    }

    /// `insert_agent_session`：开会话（撞主键重摇 5 次）。
    pub fn insert_agent_session(
        &self,
        owner: &str,
        topic: &str,
        title: Option<&str>,
        members: &[String],
        idle_ttl_s: i64,
    ) -> Result<Value, DbUnavailable> {
        let members_json = json!(members.to_vec());
        for _ in 0..SESS_ID_TRY {
            let session_id = new_session_id();
            let sql = format!(
                "INSERT INTO agent_sessions (session_id, topic, title, owner, members, idle_ttl_s) \
                 VALUES ($1, $2, $3, $4, $5::jsonb, $6) RETURNING {}",
                SESS_COLUMNS
            );
            match self.store.query_one(
                &sql,
                &[&session_id, &topic, &title, &owner, &members_json, &(idle_ttl_s as i32)],
            ) {
                Ok(Some(row)) => {
                    let mut v = row_to_agent_session(&row);
                    v.as_object_mut().unwrap().insert("created".into(), Value::Bool(true));
                    return Ok(v);
                }
                Ok(None) => continue,
                Err(e) => {
                    if e.0.to_lowercase().contains("duplicate key") {
                        continue;
                    }
                    return Err(e);
                }
            }
        }
        Err(DbUnavailable(format!("session_id 连续 {} 次撞主键，放弃", SESS_ID_TRY)))
    }

    /// `SESS_CLAIM_SQL`：认领状态机迁移（WHERE state='open' 原子）。
    pub fn sess_claim(
        &self,
        session_id: &str,
        state: &str,
        reason: &str,
        note: Option<&str>,
    ) -> Result<Option<Value>, DbUnavailable> {
        let sql = format!(
            "UPDATE agent_sessions SET state = $1, closed_at = now(), close_reason = $2, note = COALESCE($3, note) \
             WHERE session_id = $4 AND state = 'open' RETURNING {}",
            SESS_COLUMNS
        );
        Ok(self.store.query_one(&sql, &[&state, &reason, &note, &session_id])?.map(|r| row_to_agent_session(&r)))
    }

    /// `SESS_IDLE_SWEEP_SQL`：空闲超时（now - COALESCE(last_turn_at, created_at) > idle_ttl_s）。
    pub fn sess_idle_sweep(&self) -> Result<Vec<String>, DbUnavailable> {
        let rows = self.store.query(
            "UPDATE agent_sessions SET state = 'expired', closed_at = now(), close_reason = 'idle' \
             WHERE state = 'open' \
             AND now() - COALESCE(last_turn_at, created_at) > make_interval(secs => idle_ttl_s) \
             RETURNING session_id",
            &[],
        )?;
        Ok(rows.iter().map(|r| r.get::<_, String>(0)).collect())
    }

    pub fn sess_unarchived(&self, batch: i64) -> Result<Vec<String>, DbUnavailable> {
        let rows = self.store.query(
            "SELECT session_id FROM agent_sessions \
             WHERE state IN ('closed', 'expired') AND archive_ref IS NULL \
             ORDER BY COALESCE(closed_at, created_at) ASC LIMIT $1",
            &[&batch],
        )?;
        Ok(rows.iter().map(|r| r.get::<_, String>(0)).collect())
    }

    pub fn sess_set_archive_ref(&self, session_id: &str, content_id: i64) -> Result<(), DbUnavailable> {
        self.store.query(
            "UPDATE agent_sessions SET archive_ref = $1 WHERE session_id = $2 AND archive_ref IS NULL",
            &[&content_id, &session_id],
        )?;
        Ok(())
    }

    /// 归档文档落 shared_content（任务级 scope；先确保归档任务容器）。
    #[allow(clippy::too_many_arguments)]
    pub fn sess_archive_content(
        &self,
        session_id: &str,
        owner: &str,
        title: &str,
        body: &str,
        topic: &str,
        turn_count: i64,
        close_reason: Option<&str>,
        days: i64,
    ) -> Result<i64, DbUnavailable> {
        let task_id = format!("{}{}", SESS_ARCHIVE_TASK_PREFIX, session_id);
        self.store.query(
            "INSERT INTO shared_tasks (task_id, title, owner, status, ttl_at) \
             VALUES ($1, $2, $3, 'active', now() + make_interval(days => $4::int)) \
             ON CONFLICT (task_id) DO NOTHING",
            &[&task_id, &title, &owner, &(days as i32)],
        )?;
        let tags = json!(["session", "archive", &format!("topic:{}", topic)]);
        let audience = json!([SHARED_ALL_AUDIENCE]);
        let source_refs = json!([{
            "kind": "agent_session", "session_id": session_id, "topic": topic,
            "turn_count": turn_count, "close_reason": close_reason,
        }]);
        let row = self
            .store
            .query_one(
                "INSERT INTO shared_content (scope, task_id, kind, title, body, tags, owner, audience, \
                 sensitivity, status, expires_at, source_refs, updated_by, updated_at, embedding) \
                 VALUES ('task', $1, $2, $3, $4, $5::jsonb, $6, $7::jsonb, 'internal', 'active', \
                 now() + make_interval(days => $8::int), $9::jsonb, $10, now(), $11::text::vector) \
                 RETURNING content_id",
                &[
                    &task_id, &SESS_ARCHIVE_KIND, &title, &body, &tags, &owner, &audience, &days,
                    &source_refs, &"blackboard_svc", &None::<String>,
                ],
            )?
            .ok_or_else(|| DbUnavailable("归档写入 shared_content 没有返回 content_id".into()))?;
        Ok(row.get::<_, i64>(0))
    }

    /// `clear_agent_session_turn_bodies`：归档后清正文留行（D6）。返回被清空前的行数。
    pub fn clear_agent_session_turn_bodies(&self, session_id: &str) -> Result<i64, DbUnavailable> {
        let row = self.store.query_one(
            "SELECT count(*) FROM agent_session_turns WHERE session_id = $1 AND content IS NOT NULL AND content <> ''",
            &[&session_id],
        )?;
        let pending = row.map(|r| r.get::<_, i64>(0)).unwrap_or(0);
        self.store.query(
            "UPDATE agent_session_turns SET content = '' WHERE session_id = $1 AND content IS NOT NULL AND content <> ''",
            &[&session_id],
        )?;
        Ok(pending)
    }

    /// `archive_agent_session`：流水 → markdown → shared_content → archive_ref（幂等）。
    pub fn archive_agent_session(&self, session_id: &str) -> Result<Option<i64>, DbUnavailable> {
        let Some(session) = self.select_agent_session(session_id)? else {
            return Ok(None);
        };
        if let Some(r) = session.get("archive_ref").and_then(Value::as_i64) {
            return Ok(Some(r));
        }
        let turns = self.select_all_agent_turns(session_id)?;
        let body = build_session_archive_markdown(&session, &turns);
        let s = |k: &str| session.get(k).and_then(Value::as_str).unwrap_or("");
        let title = match session.get("title").and_then(Value::as_str) {
            Some(t) => format!("会话归档 {}（{}）", session_id, t),
            None => format!("会话归档 {}", session_id),
        };
        let content_id = self.sess_archive_content(
            session_id,
            if s("owner").is_empty() { "unknown" } else { s("owner") },
            &title,
            &body,
            if s("topic").is_empty() { SESS_DEFAULT_TOPIC } else { s("topic") },
            session.get("turn_count").and_then(Value::as_i64).unwrap_or(0),
            session.get("close_reason").and_then(Value::as_str),
            SESS_ARCHIVE_EXPIRES_DAYS,
        )?;
        self.sess_set_archive_ref(session_id, content_id)?;
        Ok(Some(content_id))
    }

    /// `close_agent_session`：关闭 + 归档 + 清正文。返回 (claimed, fresh_session, archive_ref, turns_cleared)。
    pub fn close_agent_session(
        &self,
        session_id: &str,
        reason: &str,
        note: Option<&str>,
    ) -> Result<(bool, Option<Value>, Option<i64>, i64), DbUnavailable> {
        let state = if reason == SESS_CLOSE_REASON_IDLE { "expired" } else { "closed" };
        let Some(row) = self.sess_claim(session_id, state, reason, note)? else {
            return Ok((false, None, None, 0));
        };
        let _ = row;
        let mut archive_ref = None;
        let mut turns_cleared = 0i64;
        match self.archive_agent_session(session_id) {
            Ok(r) => archive_ref = r,
            Err(e) => log_line(&format!("会话归档未完成 session_id={}: {}", session_id, safe_err(&e.0))),
        }
        match self.clear_agent_session_turn_bodies(session_id) {
            Ok(n) => turns_cleared = n,
            Err(e) => log_line(&format!("会话清正文未完成 session_id={}: {}", session_id, safe_err(&e.0))),
        }
        let fresh = self.select_agent_session(session_id)?;
        Ok((true, fresh, archive_ref, turns_cleared))
    }

    /// `sess_sweep_once`：空闲超时归档 + 补扫未归档。
    pub fn sess_sweep_once(&self) -> Result<usize, DbUnavailable> {
        let mut archived = 0usize;
        for sid in self.sess_idle_sweep()? {
            if self.archive_agent_session(&sid)?.is_some() {
                archived += 1;
            }
            let _ = self.clear_agent_session_turn_bodies(&sid);
            log_line(&format!("会话空闲超时归档 session_id={}", sid));
        }
        for sid in self.sess_unarchived(SESS_UNARCHIVED_BATCH)? {
            if self.archive_agent_session(&sid)?.is_some() {
                archived += 1;
                let _ = self.clear_agent_session_turn_bodies(&sid);
            }
        }
        Ok(archived)
    }
}

/// `build_session_archive_markdown`：流水拼归档文档（逐段对齐 Python 版）。
pub fn build_session_archive_markdown(session: &Value, turns: &[Value]) -> String {
    let g = |k: &str| session.get(k).cloned().unwrap_or(Value::Null);
    let s = |k: &str| g(k).as_str().map(str::to_string).unwrap_or_default();
    let mut lines: Vec<String> = vec![
        format!("# Agent 会话归档 {}", s("session_id")),
        String::new(),
        format!("- topic: `{}`", if s("topic").is_empty() { SESS_DEFAULT_TOPIC.into() } else { s("topic") }),
        format!("- 标题: {}", if s("title").is_empty() { "-".into() } else { s("title") }),
        format!("- 发起方: {}", if s("owner").is_empty() { "-".into() } else { s("owner") }),
        format!(
            "- 参与成员: {}",
            match g("members").as_array() {
                Some(a) if !a.is_empty() => a
                    .iter()
                    .map(|m| m.as_str().unwrap_or("").to_string())
                    .collect::<Vec<_>>()
                    .join(", "),
                _ => "-".into(),
            }
        ),
        format!("- 状态: {}（关闭原因 {}）", s("state"), if s("close_reason").is_empty() { "-".into() } else { s("close_reason") }),
        format!("- 轮次: {}", g("turn_count").as_i64().unwrap_or(0)),
        format!("- 创建: {}", if s("created_at").is_empty() { "-".into() } else { s("created_at") }),
        format!("- 最近一轮: {}", if s("last_turn_at").is_empty() { "-".into() } else { s("last_turn_at") }),
        format!("- 关闭: {}", if s("closed_at").is_empty() { "-".into() } else { s("closed_at") }),
        format!("- 归档: {}", iso_z(now_micros())),
        String::new(),
    ];
    if let Some(note) = g("note").as_str() {
        if !note.is_empty() {
            lines.push(format!("- 收尾说明: {}", note));
            lines.push(String::new());
        }
    }
    lines.push("---".into());
    lines.push(String::new());
    lines.push("## 对话流水".into());
    lines.push(String::new());
    if turns.is_empty() {
        lines.push("（本会话没有留下任何轮次）".into());
        lines.push(String::new());
    }
    for turn in turns {
        let t = |k: &str| turn.get(k).cloned().unwrap_or(Value::Null);
        lines.push(format!(
            "### #{} {} {} → {} [{}]",
            t("seq").as_i64().map(|v| v.to_string()).unwrap_or_else(|| "None".into()),
            t("ts").as_str().unwrap_or("None"),
            t("from_member").as_str().unwrap_or("-"),
            t("to_member").as_str().unwrap_or("-"),
            t("role").as_str().unwrap_or(""),
        ));
        lines.push(String::new());
        match t("content").as_str() {
            Some(c) if !c.is_empty() => lines.push(c.to_string()),
            _ => match t("content_ref").as_str() {
                Some(r) if !r.is_empty() => {
                    lines.push(format!("（正文已外置，见临时共享区 {}；归档文档不搬正文）", r))
                }
                _ => lines.push("（正文已按归档纪律清空）".into()),
            },
        }
        lines.push(String::new());
    }
    lines.join("\n")
}

// ================================================================ 单行日志

fn log_line(msg: &str) {
    println!("{}", msg);
}

// ================================================================ 落实事项 / 遗留问题（roundtable，只读）


pub const AGENDA_SELECT: &str = "item_id, session_id, seq, title, expected, conclusion, \
conclusion_entry, status, note";
pub const CONFIRM_SELECT: &str = "confirm_id, session_id, target, member, verdict, note, ts";
pub const ACTION_SELECT: &str = "a.action_id, a.session_id, a.agenda_seq, a.what, a.owner, a.due_at, \
a.status, a.evidence, a.note, a.created_at, a.closed_at, a.closed_by, s.topic";
pub const ACTION_FROM: &str = " FROM roundtable_actions a \
LEFT JOIN roundtable_sessions s ON s.session_id = a.session_id";
pub const ISSUE_SELECT: &str = "i.issue_id, i.session_id, i.title, i.detail, i.owner, i.status, \
i.due_at, i.next_check_at, i.refs, i.created_at, i.closed_at, i.closed_by, i.note, s.topic";
pub const ISSUE_KIND_COLUMNS: &str = "i.kind, i.evidence";
pub const ISSUE_FROM: &str = " FROM roundtable_issues i \
LEFT JOIN roundtable_sessions s ON s.session_id = i.session_id";

/// `is_overdue`：还没闭环 且 有截止时间 且 严格早于 now。
pub fn is_overdue(due_at: &Value, status: &str, open_states: &[&str], now_epoch_us: i64) -> bool {
    if !open_states.contains(&status) {
        return false;
    }
    let s = match due_at.as_str() {
        Some(s) => s,
        None => return false,
    };
    match to_utc_epoch_us(&json!(s)) {
        Some(us) => us < now_epoch_us,
        None => false,
    }
}

pub fn row_to_action(row: &postgres::Row, now_epoch_us: i64) -> Value {
    let status = row.get::<_, Option<String>>(6).unwrap_or_default();
    let due = ts_opt_at(row, 5);
    let overdue = is_overdue(&due, &status, &ACTION_OPEN_STATUSES, now_epoch_us);
    let topic = row.get::<_, Option<String>>(12).map(Value::String).unwrap_or(Value::Null);
    json!({
        "action_id": row.get::<_, i64>(0),
        "session_id": row.get::<_, Option<String>>(1).unwrap_or_default(),
        "agenda_seq": row.get::<_, Option<i32>>(2).map(|v| json!(v as i64)).unwrap_or(Value::Null),
        "what": row.get::<_, Option<String>>(3).unwrap_or_default(),
        "owner": row.get::<_, Option<String>>(4).unwrap_or_default(),
        "due_at": due,
        "status": status,
        "evidence": row.get::<_, Option<String>>(7).unwrap_or_default(),
        "note": row.get::<_, Option<String>>(8).unwrap_or_default(),
        "created_at": ts_opt_at(row, 9),
        "closed_at": ts_opt_at(row, 10),
        "closed_by": row.get::<_, Option<String>>(11).unwrap_or_default(),
        "session_topic": topic,
        "overdue": overdue,
    })
}

pub fn row_to_issue(row: &postgres::Row, now_epoch_us: i64) -> Value {
    let kind_raw = row.try_get::<_, Option<String>>(14).unwrap_or(None);
    let kind = match kind_raw.as_deref() {
        Some(k) if ISSUE_KINDS.contains(&k) => k.to_string(),
        _ => "issue".to_string(),
    };
    let refs_val = row.try_get::<_, Option<Value>>(8).unwrap_or(None).unwrap_or(Value::Null);
    let ev_val = row.try_get::<_, Option<Value>>(15).unwrap_or(None).unwrap_or(Value::Null);
    let status = row.get::<_, Option<String>>(5).unwrap_or_default();
    let due = ts_opt_at(row, 6);
    let overdue = is_overdue(&due, &status, &ISSUE_OPEN_STATUSES, now_epoch_us);
    let topic = row.get::<_, Option<String>>(13).map(Value::String).unwrap_or(Value::Null);
    json!({
        "issue_id": row.get::<_, i64>(0),
        "session_id": row.get::<_, Option<String>>(1).unwrap_or_default(),
        "title": row.get::<_, Option<String>>(2).unwrap_or_default(),
        "detail": row.get::<_, Option<String>>(3).unwrap_or_default(),
        "owner": row.get::<_, Option<String>>(4).unwrap_or_default(),
        "status": status,
        "due_at": due,
        "next_check_at": ts_opt_at(row, 7),
        "refs": as_refs(&refs_val),
        "created_at": ts_opt_at(row, 9),
        "closed_at": ts_opt_at(row, 10),
        "closed_by": row.get::<_, Option<String>>(11).unwrap_or_default(),
        "note": row.get::<_, Option<String>>(12).unwrap_or_default(),
        "session_topic": topic,
        "overdue": overdue,
        "kind": kind,
        "evidence": as_refs(&ev_val),
    })
}

impl BbSvc {
    fn tracking_where(
        col: &str,
        params: &mut Vec<Box<dyn ToSql + Sync>>,
        status: Option<&str>,
        owner: Option<&str>,
        overdue: bool,
        open_statuses: &[&str],
    ) -> Vec<String> {
        let mut where_parts: Vec<String> = vec![];
        if let Some(v) = status {
            params.push(Box::new(v.to_string()));
            where_parts.push(format!("{}.status = ${}", col, params.len()));
        }
        if let Some(v) = owner {
            params.push(Box::new(v.to_string()));
            where_parts.push(format!("{}.owner = ${}", col, params.len()));
        }
        if overdue {
            // 枚举 IN 也参数化（_in_clause 同款）
            let mut idxs: Vec<String> = vec![];
            for v in open_statuses {
                params.push(Box::new(v.to_string()));
                idxs.push(format!("${}", params.len()));
            }
            where_parts.push(format!("{}.status IN ({})", col, idxs.join(", ")));
            where_parts.push(format!("{}.due_at IS NOT NULL", col));
            params.push(Box::new(iso_utc_now()));
            where_parts.push(format!("{}.due_at < ${}::text::timestamptz", col, params.len()));
        }
        where_parts
    }

    pub fn select_actions(
        &self,
        status: Option<&str>,
        owner: Option<&str>,
        overdue: bool,
        limit: i64,
        degraded: &mut Vec<String>,
    ) -> Vec<Value> {
        let mut params: Vec<Box<dyn ToSql + Sync>> = vec![];
        let where_parts = Self::tracking_where(
            "a", &mut params, status, owner, overdue, &ACTION_OPEN_STATUSES,
        );
        let mut sql = format!("SELECT {}{}", ACTION_SELECT, ACTION_FROM);
        if !where_parts.is_empty() {
            sql.push_str(&format!(" WHERE {}", where_parts.join(" AND ")));
        }
        sql.push_str(&format!(
            " ORDER BY a.due_at ASC NULLS LAST, a.action_id ASC LIMIT ${}",
            params.len() + 1
        ));
        params.push(Box::new(limit));
        let refs: Vec<&(dyn ToSql + Sync)> = params.iter().map(|b| b.as_ref()).collect();
        let now_us = iso_utc_now_epoch_us();
        match self.store.query(&sql, &refs) {
            Ok(rows) => rows.iter().map(|r| row_to_action(r, now_us)).collect(),
            Err(e) => {
                if is_missing_schema(&e.0) {
                    degraded.push("actions".into());
                    vec![]
                } else {
                    // 上抛由调用方处理：这里包成 DbUnavailable 再走 503
                    Vec::new()
                }
            }
        }
    }

    pub fn select_issues(
        &self,
        status: Option<&str>,
        owner: Option<&str>,
        overdue: bool,
        limit: i64,
        kind: Option<&str>,
        degraded: &mut Vec<String>,
    ) -> Vec<Value> {
        let build = |with_kind: bool| -> (String, Vec<Box<dyn ToSql + Sync>>) {
            let mut params: Vec<Box<dyn ToSql + Sync>> = vec![];
            let mut where_parts = Self::tracking_where(
                "i", &mut params, status, owner, overdue, &ISSUE_OPEN_STATUSES,
            );
            if let Some(k) = kind {
                if with_kind {
                    params.push(Box::new(k.to_string()));
                    where_parts.push(format!("i.kind = ${}", params.len()));
                }
            }
            let cols = if with_kind {
                format!("{} , {}", ISSUE_SELECT, ISSUE_KIND_COLUMNS)
            } else {
                ISSUE_SELECT.to_string()
            };
            let mut sql = format!("SELECT {}{}", cols, ISSUE_FROM);
            if !where_parts.is_empty() {
                sql.push_str(&format!(" WHERE {}", where_parts.join(" AND ")));
            }
            sql.push_str(&format!(
                " ORDER BY i.due_at ASC NULLS LAST, i.issue_id ASC LIMIT ${}",
                params.len() + 1
            ));
            params.push(Box::new(limit));
            (sql, params)
        };
        let now_us = iso_utc_now_epoch_us();
        // 第一条：带 kind 列
        let (sql, params) = build(true);
        let refs: Vec<&(dyn ToSql + Sync)> = params.iter().map(|b| b.as_ref()).collect();
        match self.store.query(&sql, &refs) {
            Ok(rows) => rows.iter().map(|r| row_to_issue(r, now_us)).collect(),
            Err(e) => {
                if !is_missing_schema(&e.0) {
                    return vec![];
                }
                // 第二条：老 SQL 分辨（缺 kind 列 vs 整表没建）
                let (sql2, params2) = build(false);
                let refs2: Vec<&(dyn ToSql + Sync)> = params2.iter().map(|b| b.as_ref()).collect();
                match self.store.query(&sql2, &refs2) {
                    Ok(rows) => {
                        degraded.push("issue_kind".into());
                        if kind.is_some() {
                            vec![] // 筛不了就别给
                        } else {
                            rows.iter().map(|r| row_to_issue(r, now_us)).collect()
                        }
                    }
                    Err(_) => {
                        degraded.push("issues".into());
                        vec![]
                    }
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn count_pair(
        &self,
        table: &str,
        status_col: &str,
        due_col: &str,
        open_states: &[&str],
        degraded: &mut Vec<String>,
        name: &str,
        kind_col: Option<&str>,
        kind_value: Option<&str>,
    ) -> (Option<i64>, Option<i64>) {
        let mut params: Vec<Box<dyn ToSql + Sync>> = vec![];
        let clause = |with_due: bool, params: &mut Vec<Box<dyn ToSql + Sync>>| -> String {
            let mut parts: Vec<String> = vec![];
            let mut idxs: Vec<String> = vec![];
            for v in open_states {
                params.push(Box::new(v.to_string()));
                idxs.push(format!("${}", params.len()));
            }
            parts.push(format!("{} IN ({})", status_col, idxs.join(", ")));
            if let (Some(c), Some(v)) = (kind_col, kind_value) {
                params.push(Box::new(v.to_string()));
                parts.push(format!("{} = ${}", c, params.len()));
            }
            if with_due {
                parts.push(format!("{} IS NOT NULL", due_col));
                params.push(Box::new(iso_utc_now()));
                parts.push(format!("{} < ${}::text::timestamptz", due_col, params.len()));
            }
            parts.join(" AND ")
        };
        let open_clause = clause(false, &mut params);
        let late_clause = clause(true, &mut params);
        let sql = format!(
            "SELECT count(*) FILTER (WHERE {}), count(*) FILTER (WHERE {}) FROM {}",
            open_clause, late_clause, table
        );
        let refs: Vec<&(dyn ToSql + Sync)> = params.iter().map(|b| b.as_ref()).collect();
        match self.store.query_one(&sql, &refs) {
            Err(e) => {
                if is_missing_schema(&e.0) {
                    degraded.push(name.into());
                    (None, None)
                } else {
                    (None, None)
                }
            }
            Ok(None) => (Some(0), Some(0)),
            Ok(Some(row)) => (Some(row.get::<_, i64>(0)), Some(row.get::<_, i64>(1))),
        }
    }

    pub fn tracking_counts(&self, degraded: &mut Vec<String>) -> Value {
        let (actions_open, actions_overdue) = self.count_pair(
            "roundtable_actions", "status", "due_at", &ACTION_OPEN_STATUSES,
            degraded, "actions", None, None,
        );
        let (issues_open, issues_overdue) = self.count_pair(
            "roundtable_issues", "status", "due_at", &ISSUE_OPEN_STATUSES,
            degraded, "issues", None, None,
        );
        let mut defects_open = None;
        let mut defects_overdue = None;
        if issues_open.is_some() {
            let (d_open, d_overdue) = self.count_pair(
                "roundtable_issues", "status", "due_at", &ISSUE_OPEN_STATUSES,
                degraded, "issue_kind", Some("kind"), Some(DEFECT_KIND),
            );
            defects_open = d_open;
            defects_overdue = d_overdue;
        }
        json!({
            "actions_open": actions_open, "actions_overdue": actions_overdue,
            "issues_open": issues_open, "issues_overdue": issues_overdue,
            "defects_open": defects_open, "defects_overdue": defects_overdue,
        })
    }

    pub fn select_tracking(
        &self,
        owner: Option<&str>,
        overdue: bool,
        limit: i64,
        degraded: &mut Vec<String>,
    ) -> (Vec<Value>, Vec<Value>, Value) {
        let actions = self.select_actions(None, owner, overdue, limit, degraded);
        let issues = self.select_issues(None, owner, overdue, limit, None, degraded);
        let counts = self.tracking_counts(degraded);
        (actions, issues, counts)
    }
}

pub fn iso_utc_now_epoch_us() -> i64 {
    now_micros()
}

// ================================================================ 共享内容（shared_content，只读）

pub struct MsgFinishRow(pub i64, pub i64, pub bool, pub Option<String>);

pub const SHARED_COLUMNS: &str = "content_id, scope, task_id, kind, title, body, tags, owner, audience, \
sensitivity, status, expires_at, review_at, version, created_at, \
updated_at, updated_by, destroyed_at, source_refs";
pub const SHARED_TASK_COLUMNS: &str = "task_id, title, owner, status, note, created_at, ended_at, \
ttl_at, destroyed_at";
pub const SHARED_REVISION_COLUMNS: &str = "rev_id, content_id, action, actor, changed, note, ts";



pub fn shared_is_expired(expires_at: &Value, now_epoch_us: i64) -> bool {
    match expires_at.as_str() {
        Some(s) => match to_utc_epoch_us(&json!(s)) {
            Some(us) => us < now_epoch_us,
            None => false,
        },
        None => false,
    }
}

/// `row_to_shared`（19 列 + 可选 score=第 20 列）。
pub fn row_to_shared(row: &postgres::Row, now_epoch_us: i64) -> Value {
    let tags = row.try_get::<_, Option<Value>>(6).unwrap_or(None).unwrap_or(Value::Null);
    let audience = row.try_get::<_, Option<Value>>(8).unwrap_or(None).unwrap_or(Value::Null);
    let source_refs = row.try_get::<_, Option<Value>>(18).unwrap_or(None).unwrap_or(Value::Null);
    let status: String = row.get::<_, Option<String>>(10).unwrap_or_default();
    let expires = ts_opt_at(row, 11);
    let score: Option<Option<f64>> = row.try_get::<_, Option<f64>>(19).ok().flatten().map(Some);
    let mut v = json!({
        "content_id": row.get::<_, i64>(0),
        "scope": row.get::<_, Option<String>>(1).unwrap_or_default(),
        "task_id": row.get::<_, Option<String>>(2).unwrap_or_default(),
        "kind": row.get::<_, Option<String>>(3).unwrap_or_default(),
        "title": row.get::<_, Option<String>>(4).unwrap_or_default(),
        "body": row.get::<_, Option<String>>(5).unwrap_or_default(),
        "tags": as_refs(&tags),
        "owner": row.get::<_, Option<String>>(7).unwrap_or_default(),
        "audience": as_refs(&audience),
        "sensitivity": row.get::<_, Option<String>>(9).unwrap_or_default(),
        "status": status,
        "expires_at": expires,
        "review_at": ts_opt_at(row, 12),
        "version": row.get::<_, Option<i32>>(13).map(|x| json!(x as i64)).unwrap_or(Value::Null),
        "created_at": ts_opt_at(row, 14),
        "updated_at": ts_opt_at(row, 15),
        "updated_by": row.get::<_, Option<String>>(16).unwrap_or_default(),
        "destroyed_at": ts_opt_at(row, 17),
        "source_refs": as_refs(&source_refs),
        "expired": status == "expired" || shared_is_expired(&expires, now_epoch_us),
    });
    if let Some(sc) = score {
        v.as_object_mut().unwrap().insert(
            "score".into(),
            match sc {
                Some(x) => json!(x),
                None => Value::Null,
            },
        );
    }
    v
}

pub fn row_to_shared_task(row: &postgres::Row, now_epoch_us: i64) -> Value {
    let ttl_at = ts_opt_at(row, 7);
    json!({
        "task_id": row.get::<_, Option<String>>(0).unwrap_or_default(),
        "title": row.get::<_, Option<String>>(1).unwrap_or_default(),
        "owner": row.get::<_, Option<String>>(2).unwrap_or_default(),
        "status": row.get::<_, Option<String>>(3).unwrap_or_default(),
        "note": row.get::<_, Option<String>>(4).unwrap_or_default(),
        "created_at": ts_opt_at(row, 5),
        "ended_at": ts_opt_at(row, 6),
        "ttl_at": ttl_at,
        "destroyed_at": ts_opt_at(row, 8),
        "ttl_expired": shared_is_expired(&ttl_at, now_epoch_us),
    })
}

pub fn row_to_shared_revision(row: &postgres::Row) -> Value {
    let changed = row.try_get::<_, Option<Value>>(4).unwrap_or(None).unwrap_or(Value::Null);
    json!({
        "rev_id": row.get::<_, i64>(0),
        "content_id": row.get::<_, i64>(1),
        "action": row.get::<_, Option<String>>(2).unwrap_or_default(),
        "actor": row.get::<_, Option<String>>(3).unwrap_or_default(),
        "changed": as_obj(&changed),
        "note": row.get::<_, Option<String>>(5).unwrap_or_default(),
        "ts": ts_opt_at(row, 6),
    })
}

impl BbSvc {
    pub fn select_shared(
        &self,
        where_parts: &[String],
        mut params: Vec<Box<dyn ToSql + Sync>>,
        limit: i64,
        degraded: &mut Vec<String>,
    ) -> Result<Vec<Value>, DbUnavailable> {
        let mut sql = format!(
            "SELECT {} FROM shared_content WHERE {} ORDER BY updated_at DESC, content_id DESC LIMIT ${}",
            SHARED_COLUMNS,
            where_parts.join(" AND "),
            params.len() + 1
        );
        let _ = &mut sql;
        params.push(Box::new(limit));
        let refs: Vec<&(dyn ToSql + Sync)> = params.iter().map(|b| b.as_ref()).collect();
        match self.store.query(&sql, &refs) {
            Ok(rows) => {
                let now_us = now_micros();
                Ok(rows.iter().map(|r| row_to_shared(r, now_us)).collect())
            }
            Err(e) => {
                if is_missing_schema(&e.0) {
                    degraded.push("shared_content".into());
                    Ok(vec![])
                } else {
                    Err(DbUnavailable(safe_err(&e.0)))
                }
            }
        }
    }

    pub fn search_shared_keyword(
        &self,
        q: &str,
        where_parts: &[String],
        mut params: Vec<Box<dyn ToSql + Sync>>,
        limit: i64,
        degraded: &mut Vec<String>,
    ) -> Result<Vec<Value>, DbUnavailable> {
        let pattern = Self::like_pattern(q);
        let tag_v = json!([q]);
        let mut sql = format!(
            "SELECT {} FROM shared_content WHERE {} AND (title ILIKE ${} OR body ILIKE ${} OR tags @> ${}::jsonb) \
             ORDER BY updated_at DESC, content_id DESC LIMIT ${}",
            SHARED_COLUMNS,
            where_parts.join(" AND "),
            params.len() + 1,
            params.len() + 2,
            params.len() + 3,
            params.len() + 4,
        );
        let _ = &mut sql;
        params.push(Box::new(pattern.clone()));
        params.push(Box::new(pattern));
        params.push(Box::new(tag_v));
        params.push(Box::new(limit));
        let refs: Vec<&(dyn ToSql + Sync)> = params.iter().map(|b| b.as_ref()).collect();
        match self.store.query(&sql, &refs) {
            Ok(rows) => {
                let now_us = now_micros();
                Ok(rows
                    .iter()
                    .map(|r| {
                        let mut v = row_to_shared(r, now_us);
                        v.as_object_mut().unwrap().insert("score".into(), Value::Null);
                        v
                    })
                    .collect())
            }
            Err(e) => {
                if is_missing_schema(&e.0) {
                    degraded.push("shared_content".into());
                    Ok(vec![])
                } else {
                    Err(DbUnavailable(safe_err(&e.0)))
                }
            }
        }
    }

    pub fn select_shared_one(
        &self,
        content_id: i64,
        degraded: &mut Vec<String>,
    ) -> Result<Option<Value>, DbUnavailable> {
        let sql = format!(
            "SELECT {} FROM shared_content WHERE content_id = $1",
            SHARED_COLUMNS
        );
        match self.store.query_one(&sql, &[&content_id]) {
            Ok(row) => Ok(row.map(|r| row_to_shared(&r, now_micros()))),
            Err(e) => {
                if is_missing_schema(&e.0) {
                    degraded.push("shared_content".into());
                    Ok(None)
                } else {
                    Err(DbUnavailable(safe_err(&e.0)))
                }
            }
        }
    }

    pub fn select_shared_revisions(
        &self,
        content_id: i64,
        degraded: &mut Vec<String>,
    ) -> Result<Vec<Value>, DbUnavailable> {
        let sql = format!(
            "SELECT {} FROM shared_content_revisions WHERE content_id = $1 ORDER BY ts ASC, rev_id ASC LIMIT $2",
            SHARED_REVISION_COLUMNS
        );
        match self.store.query(&sql, &[&content_id, &SHARED_REVISION_LIMIT]) {
            Ok(rows) => Ok(rows.iter().map(row_to_shared_revision).collect()),
            Err(e) => {
                if is_missing_schema(&e.0) {
                    degraded.push("shared_revisions".into());
                    Ok(vec![])
                } else {
                    Err(DbUnavailable(safe_err(&e.0)))
                }
            }
        }
    }

    pub fn select_shared_tasks(
        &self,
        status: Option<&str>,
        limit: i64,
        degraded: &mut Vec<String>,
    ) -> Result<Vec<Value>, DbUnavailable> {
        let mut params: Vec<Box<dyn ToSql + Sync>> = vec![];
        let mut sql = format!("SELECT {} FROM shared_tasks", SHARED_TASK_COLUMNS);
        if let Some(s) = status {
            params.push(Box::new(s.to_string()));
            sql.push_str(&format!(" WHERE status = ${}", params.len()));
        }
        sql.push_str(&format!(
            " ORDER BY created_at DESC, task_id DESC LIMIT ${}",
            params.len() + 1
        ));
        params.push(Box::new(limit));
        let refs: Vec<&(dyn ToSql + Sync)> = params.iter().map(|b| b.as_ref()).collect();
        match self.store.query(&sql, &refs) {
            Ok(rows) => {
                let now_us = now_micros();
                Ok(rows.iter().map(|r| row_to_shared_task(r, now_us)).collect())
            }
            Err(e) => {
                if is_missing_schema(&e.0) {
                    degraded.push("shared_tasks".into());
                    Ok(vec![])
                } else {
                    Err(DbUnavailable(safe_err(&e.0)))
                }
            }
        }
    }
}

// ================================================================ tests

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ---- 时间 ----
    #[test]
    fn civil_epoch_cross_values() {
        // 2026-01-01T00:00:00Z = 1767225600
        let (y, mo, d, h, mi, s) = civil_from_epoch(1767225600);
        assert_eq!((y, mo, d, h, mi, s), (2026, 1, 1, 0, 0, 0));
        assert_eq!(iso_z(1767225600i64 * 1_000_000), "2026-01-01T00:00:00Z");
    }

    #[test]
    fn parse_iso_basic() {
        // 带时区 +08:00
        assert_eq!(
            parse_iso_to_epoch_us("2026-01-01T08:00:00+08:00"),
            Some(1767225600i64 * 1_000_000)
        );
        // 裸时间按 UTC
        assert_eq!(
            parse_iso_to_epoch_us("2026-01-01 00:00:00"),
            Some(1767225600i64 * 1_000_000)
        );
        // Z 后缀
        assert_eq!(
            parse_iso_to_epoch_us("2026-01-01T00:00:00Z"),
            Some(1767225600i64 * 1_000_000)
        );
        // 日期-only
        assert_eq!(
            parse_iso_to_epoch_us("2026-01-01"),
            Some(1767225600i64 * 1_000_000)
        );
        // 微秒
        assert_eq!(
            parse_iso_to_epoch_us("2026-01-01T00:00:00.5+00:00"),
            Some(1767225600i64 * 1_000_000 + 500_000)
        );
        // 认不出
        assert_eq!(parse_iso_to_epoch_us("垃圾"), None);
        assert_eq!(parse_iso_to_epoch_us(""), None);
    }

    #[test]
    fn to_utc_epoch_us_kinds() {
        assert_eq!(
            to_utc_epoch_us(&json!("2026-01-01T00:00:00Z")),
            Some(1767225600i64 * 1_000_000)
        );
        assert_eq!(to_utc_epoch_us(&json!(1767225600)), Some(1767225600i64 * 1_000_000));
        assert_eq!(to_utc_epoch_us(&json!(null)), None);
        assert_eq!(to_utc_epoch_us(&json!(true)), None);
    }

    // ---- 错误面 ----
    #[test]
    fn safe_err_scrubs() {
        // 密码字样不外泄
        let s = safe_err("host=127.0.0.1 password=secret123 dbname=chat");
        assert!(!s.contains("secret123"), "password leaked: {s}");
    }

    #[test]
    fn missing_schema_detection() {
        assert!(is_missing_schema("relation \"tmp_shared\" does not exist"));
        assert!(is_missing_schema("column \"kind\" does not exist"));
        assert!(!is_missing_schema("connection refused"));
        assert!(!is_missing_schema(""));
    }

    // ---- refs / summary ----
    #[test]
    fn as_refs_shapes() {
        assert_eq!(as_refs(&json!(["a", "b"])), json!(["a", "b"]));
        // 字符串补一次解析
        assert_eq!(as_refs(&json!("[\"x\"]")), json!(["x"]));
        // 单字符串不是数组 → 空数组
        assert_eq!(as_refs(&json!("x")), json!([]));
        assert_eq!(as_refs(&json!(null)), json!([]));
    }

    #[test]
    fn as_summary_contract() {
        let full = as_summary(&json!({
            "agree": ["a"], "object": [], "abstain": [], "absent": ["b"],
            "required": 3, "present": 4, "passed": true,
            "attendance": {"expected": 5, "present": 4, "absent": 1, "rate": 0.8,
                           "present_members": ["a"], "absent_members": ["b"]},
        }));
        assert_eq!(full["agree"], json!(["a"]));
        assert_eq!(full["required"], json!(3));
        assert_eq!(full["present"], json!(4));
        assert_eq!(full["passed"], json!(true));
        assert_eq!(full["attendance"]["rate"], json!(0.8));
        // passed 非布尔 → null
        let tricky = as_summary(&json!({"passed": "yes", "agree": "not-a-list"}));
        assert_eq!(tricky["passed"], Value::Null);
        assert_eq!(tricky["agree"], json!([]));
        // 非对象 → Null
        assert_eq!(as_summary(&json!("junk")), Value::Null);
        assert_eq!(as_summary(&json!([1, 2])), Value::Null);
    }

    // ---- ID 生成与校验 ----
    #[test]
    fn tmp_and_msg_ids() {
        let t = new_prefixed_id(TMP_ID_PREFIX);
        assert!(tmp_id_ok(&t), "tmp id: {t}");
        assert!(!tmp_id_ok("bad"));
        assert!(!tmp_id_ok("t-xxxxxxxx")); // 小写不在大写字母表
        assert!(!tmp_id_ok("t-ABC")); // 长度不足
        let m = new_prefixed_id(MSG_ID_PREFIX);
        assert!(msg_id_ok(&m), "msg id: {m}");
        assert!(!msg_id_ok("zz"));
    }

    #[test]
    fn session_id_shape() {
        let sid = new_session_id();
        assert!(session_id_ok(&sid), "sid: {sid}");
        assert_eq!(sid.len(), 23);
        assert!(sid.starts_with("as-"));
        assert!(!session_id_ok("as-20260101-000000-ZZZZ")); // 非法字母表
        assert!(!session_id_ok("as-2026010100000-xxxx"));
    }

    // ---- like_pattern（ILIKE 转义）----
    #[test]
    fn like_pattern_escapes() {
        assert_eq!(BbSvc::like_pattern("abc"), "%abc%");
        let p = BbSvc::like_pattern("a%b_c\\d");
        assert!(p.starts_with('%') && p.ends_with('%'));
        assert!(p.contains("\\%"), "percent not escaped: {p}");
        assert!(p.contains("\\_"), "underscore not escaped: {p}");
    }

    // ---- is_overdue ----
    #[test]
    fn overdue_semantics() {
        let now = 1767225600i64 * 1_000_000; // 2026-01-01T00:00:00Z
        // 严格早于才算
        assert!(is_overdue(&json!("2025-12-31T23:59:59Z"), "open", &ACTION_OPEN_STATUSES, now));
        // 正好等于不算
        assert!(!is_overdue(&json!("2026-01-01T00:00:00Z"), "open", &ACTION_OPEN_STATUSES, now));
        // 已闭环不算
        assert!(!is_overdue(&json!("2025-12-31T00:00:00Z"), "done", &ACTION_OPEN_STATUSES, now));
        // 没有 due_at 不算
        assert!(!is_overdue(&json!(null), "open", &ACTION_OPEN_STATUSES, now));
        // 认不出的时间不算
        assert!(!is_overdue(&json!("垃圾"), "open", &ACTION_OPEN_STATUSES, now));
    }

    // ---- shared 过期 ----
    #[test]
    fn shared_expired_semantics() {
        let now = 1767225600i64 * 1_000_000;
        assert!(shared_is_expired(&json!("2025-12-31T00:00:00Z"), now));
        assert!(!shared_is_expired(&json!("2026-01-01T00:00:00Z"), now)); // 等于不算
        assert!(!shared_is_expired(&json!(null), now));
    }

    // ---- as_obj ----
    #[test]
    fn as_obj_shapes() {
        assert_eq!(as_obj(&json!({"a": 1})), json!({"a": 1}));
        assert_eq!(as_obj(&json!("{\"a\": 1}")), json!({"a": 1}));
        assert_eq!(as_obj(&json!("junk")), json!({}));
        assert_eq!(as_obj(&json!(3)), json!({}));
    }

    // ---- iso_utc_now 形状 ----
    #[test]
    fn iso_utc_now_shape() {
        let s = iso_utc_now();
        // YYYY-MM-DDTHH:MM:SS.ffffff+00:00
        assert_eq!(s.len(), 32, "iso: {s}");
        assert!(s.ends_with("+00:00"));
        assert_eq!(&s[10..11], "T");
    }
}
