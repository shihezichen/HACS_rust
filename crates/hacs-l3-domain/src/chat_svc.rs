//! chat_svc 纯逻辑件（Python `hub/l3_domain/chat_svc.py` 的移植）。
//!
//! 契约要点（与 Python 严格对齐）：
//! · 错误一律对象信封 `{"ok": false, "error": {"code", "message"}}`；
//! · 时间一律 ISO8601 带 `+08:00`（CST）；
//! · 会话/消息/待办 id 形状见 `gen_*`（§2：不透明，客户端不得解析）；
//! · `classify_human_intent` / `todo_task_name` / `todo_dedupe_key` / `parse_text_mentions`
//!   等纯函数必须逐字对齐（#478/#534/#539 实踩口径）。
//! 数据库：PostgreSQL（psycopg2 → postgres crate，libpq 同源）。

use std::time::{SystemTime, UNIX_EPOCH};

pub mod classify;
pub mod ids;
pub mod landing;
pub mod mentions;
pub mod store;

pub use classify::classify_human_intent;
pub use ids::*;
pub use landing::{landing_kind_of, normalize_landing, validate_todo_evidence};
pub use mentions::parse_text_mentions;
pub use store::{ChatStore, DbUnavailable};

/// 请求级错误（对齐 Python `RequestError`：HTTP 状态 + 契约错误码 + 文案 + extra）。
#[derive(Debug, Clone)]
pub struct RequestError {
    pub status: u16,
    pub code: String,
    pub message: String,
    pub extra: Vec<(String, serde_json::Value)>,
}

impl RequestError {
    pub fn new(status: u16, code: &str, message: &str) -> Self {
        RequestError { status, code: code.to_string(), message: message.to_string(), extra: vec![] }
    }
    pub fn with_extra(mut self, k: &str, v: serde_json::Value) -> Self {
        self.extra.push((k.to_string(), v));
        self
    }
    /// 对象信封（契约 §8）。
    pub fn body(&self) -> serde_json::Value {
        let mut err = serde_json::Map::new();
        err.insert("code".into(), serde_json::json!(self.code));
        err.insert("message".into(), serde_json::json!(self.message));
        for (k, v) in &self.extra {
            err.insert(k.clone(), v.clone());
        }
        serde_json::json!({"ok": false, "error": err})
    }
}

pub const VERSION: &str = "2.0";
pub const DEFAULT_HOST: &str = "127.0.0.1";
pub const DEFAULT_PORT: u16 = 9980;
pub const TEXT_MAX: usize = 4000;
pub const SESSION_TITLE_MAX: usize = 80;
pub const SESSION_NOTE_MAX: usize = 2000;
pub const PAGE_LIMIT_DEFAULT: i64 = 20;
pub const BODY_MAX: usize = 1 << 20;
pub const TODO_TEXT_MAX: usize = 500;
pub const TODO_LANDING_MAX: usize = 8;
pub const TODO_LANDING_MAXLEN: usize = 500;
pub const TODO_TASK_SUMMARY_LEN: usize = 160;
pub const TODO_TASK_NAME_MAX: usize = 36;
pub const TODO_DEDUPE_PREFIX: usize = 120;
pub const MEMO_MAX_BYTES: usize = 256 * 1024;
pub const TODO_MSG_EVIDENCE_TYPE: &str = "msg";

pub const MEMO_STR_KEYS: [&str; 2] = ["goal", "resume"];
pub const MEMO_LIST_KEYS: [&str; 3] = ["decisions", "open_tasks", "conventions"];
pub const TODO_STATUS: [&str; 3] = ["open", "doing", "done"];
pub const TODO_SORTS: [&str; 2] = ["time", "owner"];
pub const TODO_INTENTS: [&str; 2] = ["task", "followup"];
pub const TODO_EVIDENCE_TYPES: [&str; 4] = ["commit", "curl", "restart", "file"];

pub const UTC_EPOCH: u64 = 0;

/// 北京时间（UTC+8）固定偏移；Python `BJ = timezone(timedelta(hours=8))`。
pub const BJ_OFFSET_S: i64 = 8 * 3600;

/// `iso_bj`：epoch 秒 → ISO8601 带 +08:00，秒精度（契约所有时间的形状）。
pub fn iso_bj(epoch_s: i64) -> String {
    let (y, mo, d, h, mi, s) = civil_from_epoch(epoch_s + BJ_OFFSET_S);
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}+08:00", y, mo, d, h, mi, s)
}

/// epoch → (y, m, d, h, min, s)（UTC civil 历法，Howard Hinnant 算法；task_svc 同款）。
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
    (
        y,
        m,
        d,
        (secs / 3600) as u32,
        ((secs % 3600) / 60) as u32,
        (secs % 60) as u32,
    )
}

pub fn now_epoch() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// 宽松解析时间（落点/截止用）：ISO8601 / 'YYYY-MM-DD HH:MM(:SS)' / 'YYYY-MM-DD' / 'YYYY/MM/DD …'。
/// 解析不了回 None —— 调用方拒收，不猜不默认。无时区按 BJ（Python `replace(tzinfo=BJ)`）。
pub fn parse_time(raw: &str) -> Option<i64> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    // ISO8601：先试带时区
    if let Some(v) = parse_iso8601(raw) {
        return Some(v);
    }
    // 'YYYY-MM-DD HH:MM(:SS)' / 'YYYY-MM-DD'
    for (sep, has_hms) in [("-", true), ("/", true)] {
        let _ = has_hms;
        let parts: Vec<&str> = raw.split(sep).collect();
        if sep == "-" && (parts.len() == 2 || parts.len() == 3) {
            if let Some(v) = parse_ymd_parts(&parts, "-") {
                return Some(v);
            }
        }
        if sep == "/" && (parts.len() == 2 || parts.len() == 3) {
            if let Some(v) = parse_ymd_parts(&parts, "/") {
                return Some(v);
            }
        }
    }
    None
}

fn parse_ymd_parts(parts: &[&str], _sep: &str) -> Option<i64> {
    let y: i64 = parts[0].trim().parse().ok()?;
    if !(1000..=9999).contains(&y) {
        return None;
    }
    let m: i64 = parts.get(1)?.trim().parse().ok()?;
    let d: i64 = if parts.len() > 2 {
        parts[2].split_whitespace().next()?.trim().parse().ok()?
    } else {
        1
    };
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    // 时分（可选）：第三段可能带 "HH:MM:SS"
    let (h, mi, s) = if parts.len() > 2 {
        let tail = parts[2];
        let hms: Vec<&str> = tail.split_whitespace().nth(1)?.split(':').collect();
        (
            hms.first()?.trim().parse().ok()?,
            hms.get(1).and_then(|v| v.trim().parse().ok()).unwrap_or(0),
            hms.get(2).and_then(|v| v.trim().parse().ok()).unwrap_or(0),
        )
    } else {
        (0, 0, 0)
    };
    if h > 23 || mi > 59 || s > 59 {
        return None;
    }
    Some(epoch_from_civil(y, m, d, h, mi, s) - BJ_OFFSET_S)
}

fn parse_iso8601(raw: &str) -> Option<i64> {
    // 结尾 Z（UTC）→ 显式 +00:00；单独的 "Z" 字符替换会把日期/时间里的内容弄乱，只处理结尾。
    let raw = if raw.ends_with('Z') || raw.ends_with('z') {
        format!("{}+00:00", &raw[..raw.len() - 1])
    } else {
        raw.to_string()
    };
    // 日期部分
    let date_part: String = raw.chars().take(10).collect();
    let dp: Vec<&str> = date_part.split('-').collect();
    if dp.len() != 3 {
        return None;
    }
    let y: i64 = dp[0].parse().ok()?;
    let mo: i64 = dp[1].parse().ok()?;
    let d: i64 = dp[2].parse().ok()?;
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
        return None;
    }
    let rest = &raw[10..];
    let h: i64;
    let mut mi = 0i64;
    let mut s = 0i64;
    let offset: i64;
    if !rest.is_empty() {
        let body = rest.trim_start_matches(['T', ' ']);
        let (hms, tz) = split_tz(body);
        let hs: Vec<&str> = hms.split(':').collect();
        h = hs.first()?.parse().ok()?;
        if hs.len() > 1 {
            mi = hs[1].parse().ok()?;
        }
        if hs.len() > 2 {
            // 秒可能带小数
            let sec_tok = hs[2];
            let sec_clean: String = sec_tok.chars().take_while(|c| c.is_ascii_digit()).collect();
            s = if sec_clean.is_empty() { 0 } else { sec_clean.parse().ok()? };
        }
        if h > 23 || mi > 59 || s > 59 {
            return None;
        }
        if let Some(tz) = tz {
            offset = tz;
        } else {
            offset = BJ_OFFSET_S; // 无时区按 BJ
        }
    } else {
        return Some(epoch_from_civil(y, mo, d, 0, 0, 0) - BJ_OFFSET_S);
    }
    Some(epoch_from_civil(y, mo, d, h, mi, s) - offset)
}

fn split_tz(body: &str) -> (&str, Option<i64>) {
    for (i, ch) in body.char_indices() {
        let is_tz_mark = ch == '+' || (ch == '-' && i > 5);
        if is_tz_mark {
            if let Some(v) = parse_tz(&body[i..]) {
                return (&body[..i], Some(v));
            }
        }
    }
    (body, None)
}

fn parse_tz(tz: &str) -> Option<i64> {
    let tz = tz.trim();
    if tz.is_empty() {
        return None;
    }
    let sign = if tz.starts_with('-') { -1 } else { 1 };
    let body = tz.strip_prefix(['+', '-'])?;
    let parts: Vec<&str> = body.split(':').collect();
    let h: i64 = parts.first()?.trim().parse().ok()?;
    let m: i64 = parts.get(1).and_then(|v| v.trim().parse().ok()).unwrap_or(0);
    Some(sign * (h * 3600 + m * 60))
}

/// civil → epoch（UTC）。
pub fn epoch_from_civil(y: i64, m: i64, d: i64, h: i64, mi: i64, s: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    (era * 146097 + doe - 719468) * 86400 + h * 3600 + mi * 60 + s
}

/// `memo_etag`：内容 + 版本号指纹，`W/"<sha256前32hex>"`。
pub fn memo_etag(session_id: &str, version: i64, doc: &serde_json::Value) -> String {
    let raw = serde_json::to_string(doc).unwrap_or_default();
    let input = format!("{}|{}|{}", session_id, version, raw);
    let digest = sha256_hex(input.as_bytes());
    format!("W/\"{}\"", &digest[..32])
}

/// `memo_etag_ok`：缺省/`*` 只在"还没有过记忆"时放行。
pub fn memo_etag_ok(current_version: i64, current_etag: &str, if_match: &str) -> bool {
    let want = if_match.trim();
    if want.is_empty() || want == "*" {
        return current_version == 0;
    }
    want == current_etag
}

pub fn sha256_hex(data: &[u8]) -> String {
    hacs_l1_transport::a2a_edge::sha256_hex(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_iso_bj() {
        assert_eq!(iso_bj(0), "1970-01-01T08:00:00+08:00");
        assert_eq!(iso_bj(1767196800), "2026-01-01T00:00:00+08:00");
        assert_eq!(iso_bj(1767225600), "2026-01-01T08:00:00+08:00");
    }

    #[test]
    fn test_parse_time() {
        // 带时区 ISO
        assert_eq!(parse_time("2026-01-01T00:00:00+08:00"), Some(1767196800));
        assert_eq!(parse_time("2026-01-01T00:00:00Z"), Some(1767225600));
        // 无时区 → BJ
        assert_eq!(parse_time("2026-01-01 12:00"), Some(1767240000));
        assert_eq!(parse_time("2026-01-01"), Some(1767196800));
        assert_eq!(parse_time("2026/01/01 12:00"), Some(1767240000));
        assert_eq!(parse_time("垃圾"), None);
        assert_eq!(parse_time(""), None);
    }

    #[test]
    fn test_memo_etag_shape() {
        let e = memo_etag("s1", 1, &serde_json::json!({"goal":"a"}));
        assert!(e.starts_with("W/\""));
        assert_eq!(e.len(), 36); // W/" + 32 hex + "
        assert_eq!(memo_etag("s1", 1, &serde_json::json!({"goal":"a"})), e);
    }

    #[test]
    fn test_memo_etag_ok() {
        assert!(memo_etag_ok(0, "", ""));
        assert!(memo_etag_ok(0, "", "*"));
        assert!(!memo_etag_ok(3, "", "")); // 已有记忆时空 If-Match 不放行
        assert!(memo_etag_ok(3, "W/\"abc\"", "W/\"abc\""));
        assert!(!memo_etag_ok(3, "W/\"abc\"", "W/\"xyz\""));
    }

    #[test]
    fn test_request_error_body() {
        let e = RequestError::new(422, "bad_kind", "kind 只能是 dm / meet");
        let b = e.body();
        assert_eq!(b["ok"], serde_json::json!(false));
        assert_eq!(b["error"]["code"], serde_json::json!("bad_kind"));
    }
}
