//! L1 通信层：**团队 NATS 客户端守护**（对应 Python `node/l1_transport/teamd.py`）。
//!
//! 它是「NATS 中枢」与「本机 A2A 服务（a2a_svc.py:9920）」之间的翻译官：
//! 订阅 `a2a.v1.req.<member>`，把点名转成本机 A2A HTTP 调用，响应原样回投。
//! 同时每 30s 发心跳 `team.hb.<member>`、按 1→2→5→10→30s 退避重连、
//! 每 60s PING 探活，并在 127.0.0.1:9960 起只读管理口（/healthz /varz）。
//!
//! 纪律（与 Python 版逐条对齐）：
//!   * **多成员多连接**：同机多个成员共用一个进程，但每个成员各自一条独立
//!     NATS 连接、各自一套凭据；一个成员断线不牵连其他成员；
//!   * **有界队列 + 固定 worker 池**：reader 回调只入队（必须立刻返回），
//!     队列满或成员在途超限 → 立刻回 `busy` 拒收（带 retry_after_ms），不排队不静默丢；
//!   * **回合硬墙**：worker 最多等 `timeout_s + HARD_WALL_GRACE_S`，到点立刻回
//!     `error=timeout` 放掉 worker，不占坑干等；
//!   * 纯逻辑（凭据解析 / 超时夹取 / 错误信封 / 计数器）在本模块，供单测；
//!     进程装配在 `hacs-c::bin/teamd`。
//!
//! 单元级可测面：[`parse_member_kv`] / [`resolve_member_creds`] / [`default_password_path`]
//! / [`clamp_timeout`] / [`err_envelope`] / [`Counters`] / [`truncate_text`]。

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hacs_l0_host::node_paths as np;
use serde_json::{json, Value};

pub const VERSION: &str = "1.2";
pub const REQ_SUBJECT_FMT: &str = "a2a.v1.req.%s";
pub const HB_SUBJECT_FMT: &str = "team.hb.%s";
pub const REQ_SID: &str = "teamd-req";

pub const DEFAULT_TIMEOUT_S: f64 = 120.0;
pub const MAX_TIMEOUT_S: f64 = 600.0;
pub const MIN_TIMEOUT_S: f64 = 0.1;
pub const HARD_ROUND_CAP_S: f64 = 240.0;
pub const HARD_WALL_GRACE_S: f64 = 8.0;
pub const DETAIL_MAX: usize = 200;
pub const MAX_PAYLOAD_BYTES: usize = 262144;
pub const BACKOFF_STEPS: &[f64] = &[1.0, 2.0, 5.0, 10.0, 30.0];
pub const BUSY_RETRY_AFTER_MS: u64 = 2000;
pub const DEFAULT_QUEUE_SIZE: usize = 64;
pub const DEFAULT_INBOX_REPLAY_LIMIT: usize = 20;
pub const INBOX_HTTP_TIMEOUT_S: f64 = 10.0;
pub const DEFAULT_WORKERS: usize = 8;
pub const DEFAULT_PER_MEMBER_LIMIT: usize = 2;

pub const COUNTER_KEYS: &[&str] = &[
    "rx_requests", "ok", "errors", "timeouts", "hb_sent", "reconnects",
    "bad_requests", "hb_failed", "busy_rejected", "inbox_replayed", "inbox_errors",
];

/// 成员名规则：字母数字 `._-`（与 Python `_MEMBER_RE` 同口径）。
pub fn valid_member(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

// ---------------------------------------------------------------- 日志 / 文本

/// stdout 一行一事件，带时间戳与成员前缀。不走日志框架。
pub fn log_event(event: &str, member: Option<&str>, fields: &[(&str, Value)]) {
    let ts = unix_now();
    let mut parts = Vec::new();
    if let Some(m) = member {
        parts.push(format!("[{m}]"));
    }
    parts.push(event.to_string());
    let mut sorted: Vec<&(&str, Value)> = fields.iter().collect();
    sorted.sort_by_key(|(k, _)| *k);
    for (k, v) in sorted {
        let value = match v {
            Value::Number(n) if n.as_f64().map(|f| f.fract() != 0.0).unwrap_or(false) => {
                json!(round3(n.as_f64().unwrap_or(0.0))).to_string()
            }
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        parts.push(format!("{k}={value}"));
    }
    println!("{} {}", fmt_time(ts), parts.join(" "));
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `time.strftime("%Y-%m-%d %H:%M:%S")` 的对应物（本地时区按 UTC 简化——日志诊断面）。
fn fmt_time(secs: u64) -> String {
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (y, mo, d) = civil_from_days(days as i64);
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}")
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn round3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}

/// 错误 detail 截断（超出补 `…`）。
pub fn truncate_text(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        text.to_string()
    } else {
        let head: String = text.chars().take(limit).collect();
        format!("{head}…")
    }
}

// ---------------------------------------------------------------- 计数器

/// 线程安全计数器。errors 含 timeouts / bad_requests（它们是 errors 的子类）。
#[derive(Default)]
pub struct Counters {
    data: Mutex<HashMap<String, u64>>,
}

impl Counters {
    pub fn new() -> Self {
        let mut data = HashMap::new();
        for key in COUNTER_KEYS {
            data.insert((*key).to_string(), 0);
        }
        Self {
            data: Mutex::new(data),
        }
    }

    pub fn inc(&self, key: &str) {
        self.inc_by(key, 1);
    }

    pub fn inc_by(&self, key: &str, delta: u64) {
        let mut data = self.data.lock().unwrap();
        *data.entry(key.to_string()).or_insert(0) += delta;
    }

    pub fn snapshot(&self) -> HashMap<String, u64> {
        self.data.lock().unwrap().clone()
    }
}

// ---------------------------------------------------------------- 凭据解析（不连网，可单元测试）

/// 把 `--user`/`--password-file` 的原始值列表拆成「成员=值」与「裸值」两类。
pub fn parse_member_kv(entries: &[String]) -> (HashMap<String, String>, Vec<String>) {
    let mut keyed = HashMap::new();
    let mut unkeyed = Vec::new();
    for entry in entries {
        match entry.split_once('=') {
            Some((key, value)) if valid_member(key) => {
                keyed.insert(key.to_string(), value.to_string());
            }
            _ => unkeyed.push(entry.clone()),
        }
    }
    (keyed, unkeyed)
}

/// 两者都缺省时的平台默认口令文件路径。
pub fn default_password_path(member: &str) -> String {
    if cfg!(windows) {
        let base = np::env_stripped("AGENT_TEAM_NATS_DIR")
            .unwrap_or_else(|| np::runtime_dir(None));
        return format!("{base}\\nats-{member}.pass");
    }
    format!("{}/{}.pass", np::nats_dir(None), member)
}

/// 解析每个成员的 NATS 用户名 + 口令文件路径（只定位路径，不读文件内容）。
///
/// 规则与 Python 一致：
///   * `成员=值` 形式可重复给出，各归各；
///   * 裸值只允许在单成员时给出一个（兼容旧写法）；多成员给裸值报错；
///   * 缺省按平台推断默认路径；
///   * user 缺省取成员名本身；
///   * 口令文件不存在 → `Err(missing)`，由调用方汇总成 FATAL。
pub fn resolve_member_creds(
    members: &[String],
    user_entries: &[String],
    password_file_entries: &[String],
) -> Result<HashMap<String, (String, String)>, Vec<String>> {
    let (user_keyed, user_unkeyed) = parse_member_kv(user_entries);
    let (pass_keyed, pass_unkeyed) = parse_member_kv(password_file_entries);

    if let Some(first) = user_unkeyed.first() {
        if user_unkeyed.len() > 1 {
            return Err(vec![format!(
                "[FATAL] --user 不带「成员=」前缀时只能给一个值，收到多个: {user_unkeyed:?}"
            )]);
        }
        if members.len() > 1 {
            return Err(vec![format!(
                "[FATAL] --member 有多个成员时，--user 必须用「成员=用户名」形式给出；收到未加成员前缀的单值: {first:?}"
            )]);
        }
    }
    if let Some(first) = pass_unkeyed.first() {
        if pass_unkeyed.len() > 1 {
            return Err(vec![format!(
                "[FATAL] --password-file 不带「成员=」前缀时只能给一个值，收到多个: {pass_unkeyed:?}"
            )]);
        }
        if members.len() > 1 {
            return Err(vec![format!(
                "[FATAL] --member 有多个成员时，--password-file 必须用「成员=路径」形式给出；收到未加成员前缀的单值: {first:?}"
            )]);
        }
    }

    let mut result = HashMap::new();
    let mut missing = Vec::new();
    for (i, m) in members.iter().enumerate() {
        let user = user_keyed
            .get(m)
            .cloned()
            .or_else(|| {
                if user_unkeyed.len() == 1 && members.len() == 1 {
                    user_unkeyed.first().cloned()
                } else {
                    None
                }
            })
            .unwrap_or_else(|| m.clone());
        let pf = pass_keyed
            .get(m)
            .cloned()
            .or_else(|| {
                if pass_unkeyed.len() == 1 && members.len() == 1 {
                    pass_unkeyed.first().cloned()
                } else {
                    None
                }
            })
            .unwrap_or_else(|| default_password_path(m));
        let expanded = expand_user(&pf);
        if !std::path::Path::new(&expanded).is_file() {
            missing.push(format!("{m}（口令文件不存在: {pf}）"));
            continue;
        }
        result.insert(m.clone(), (user, pf));
        let _ = i;
    }
    if !missing.is_empty() {
        return Err(missing);
    }
    Ok(result)
}

fn expand_user(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
        let home = np::home();
        if !home.is_empty() {
            return format!("{home}{}{rest}", std::path::MAIN_SEPARATOR);
        }
    }
    path.to_string()
}

// ---------------------------------------------------------------- 超时 / 信封

/// 超时夹取：非法/<=0 → 默认；>MAX → MAX；<MIN → MIN。
pub fn clamp_timeout(raw: Option<&Value>) -> f64 {
    let value = match raw {
        Some(Value::Number(n)) => n.as_f64(),
        Some(Value::String(s)) => s.trim().parse::<f64>().ok(),
        _ => None,
    };
    match value {
        None => DEFAULT_TIMEOUT_S,
        Some(v) if v <= 0.0 => DEFAULT_TIMEOUT_S,
        Some(v) if v > MAX_TIMEOUT_S => MAX_TIMEOUT_S,
        Some(v) if v < MIN_TIMEOUT_S => MIN_TIMEOUT_S,
        Some(v) => v,
    }
}

/// 错误信封（`status` 可选字段按需挂）。
pub fn err_envelope(kind: &str, detail: &str, status: Option<u16>) -> Value {
    let mut out = json!({
        "ok": false,
        "error": kind,
        "detail": truncate_text(detail, DETAIL_MAX),
    });
    if let Some(s) = status {
        out["status"] = json!(s);
    }
    out
}

// ---------------------------------------------------------------- A2A 请求载荷（纯函数部分）

/// 请求载荷校验 + 解析（对应 `handle_request` 的前半段，纯函数供单测）。
/// 返回 `(text, caller, session_id, timeout_s)` 或错误信封。
pub fn parse_request_payload(payload: &[u8]) -> Result<(String, String, Option<String>, f64), Value> {
    if payload.len() > MAX_PAYLOAD_BYTES {
        return Err(err_envelope(
            "bad_request",
            &format!("payload 超过 {MAX_PAYLOAD_BYTES} 字节"),
            None,
        ));
    }
    let text = std::str::from_utf8(payload)
        .map_err(|e| err_envelope("bad_request", &format!("payload 不是合法 UTF-8: {e}"), None))?;
    let obj: Value = serde_json::from_str(text)
        .map_err(|e| err_envelope("bad_request", &format!("payload 不是合法 JSON: {e}"), None))?;
    if !obj.is_object() {
        return Err(err_envelope("bad_request", "payload 顶层必须是对象", None));
    }
    let body = obj.get("text").and_then(Value::as_str).unwrap_or("");
    if body.trim().is_empty() {
        return Err(err_envelope("bad_request", "缺少必填字段 text", None));
    }
    let caller = obj
        .get("from")
        .map(|v| match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        })
        .unwrap_or_default();
    let session_id = obj
        .get("session_id")
        .map(|v| match v {
            Value::Null => None,
            Value::String(s) => Some(s.clone()),
            other => Some(other.to_string()),
        })
        .unwrap_or(None);
    let declared = clamp_timeout(obj.get("timeout_s"));
    // 回合硬上限：调用方声明只是"最多"，真正可用时长还要过 240s 这道墙
    let timeout_s = declared.min(HARD_ROUND_CAP_S);
    Ok((body.to_string(), caller, session_id, timeout_s))
}

// ---------------------------------------------------------------- 并发闸门

/// 有界队列 + 每成员在途上限。Python 用 `queue.Queue(maxsize)` + `_admit`；
/// 这里用 `Mutex<Vec>` + `Condvar`（消费端逐条 pop）。
pub struct TaskGate {
    max_depth: usize,
    inner: Mutex<GateInner>,
    cv: Condvar,
}

#[derive(Default)]
struct GateInner {
    queue: Vec<(Vec<u8>, String, String)>,
    inflight: HashMap<String, usize>,
}

impl TaskGate {
    pub fn new(queue_size: usize) -> Self {
        Self {
            max_depth: queue_size.max(1),
            inner: Mutex::new(GateInner::default()),
            cv: Condvar::new(),
        }
    }

    /// 尝试占用额度：放行 `true`；拒绝返回原因（人类可读）。
    pub fn admit(&self, member: &str, per_member_limit: usize) -> Result<(), String> {
        let mut inner = self.inner.lock().unwrap();
        if inner.queue.len() >= self.max_depth {
            return Err(format!(
                "队列已满({}/{})",
                inner.queue.len(),
                self.max_depth
            ));
        }
        let cur = inner.inflight.get(member).copied().unwrap_or(0);
        if cur >= per_member_limit.max(1) {
            return Err(format!(
                "成员 {member} 在途已达上限({})",
                per_member_limit.max(1)
            ));
        }
        inner.inflight.insert(member.to_string(), cur + 1);
        Ok(())
    }

    pub fn enqueue(&self, payload: Vec<u8>, reply: String, member: &str) -> Result<(), String> {
        let mut inner = self.inner.lock().unwrap();
        if inner.queue.len() >= self.max_depth {
            return Err("队列已满".to_string());
        }
        inner.queue.push((payload, reply, member.to_string()));
        self.cv.notify_one();
        Ok(())
    }

    /// worker 取一条（阻塞，带 0.5s 超时循环由调用方控制）。
    pub fn pop(&self, timeout: Duration) -> Option<(Vec<u8>, String, String)> {
        let deadline = std::time::Instant::now() + timeout;
        let mut inner = self.inner.lock().unwrap();
        loop {
            if let Some(item) = inner.queue.first().cloned() {
                inner.queue.remove(0);
                return Some(item);
            }
            let now = std::time::Instant::now();
            if now >= deadline {
                return None;
            }
            let (guard, _) = self.cv.wait_timeout(inner, deadline - now).unwrap();
            inner = guard;
        }
    }

    pub fn release(&self, member: &str) {
        let mut inner = self.inner.lock().unwrap();
        let cur = inner.inflight.get(member).copied().unwrap_or(0);
        if cur > 1 {
            inner.inflight.insert(member.to_string(), cur - 1);
        } else {
            inner.inflight.remove(member);
        }
    }

    pub fn stats(&self, workers: usize, per_member_limit: usize) -> Value {
        let inner = self.inner.lock().unwrap();
        json!({
            "depth": inner.queue.len(),
            "capacity": self.max_depth,
            "workers": workers,
            "per_member_limit": per_member_limit,
            "inflight": inner.inflight,
        })
    }
}

use std::sync::Condvar;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn member_name_validation() {
        assert!(valid_member("lobster"));
        assert!(valid_member("openclaw-nb.2_x"));
        assert!(!valid_member(""));
        assert!(!valid_member("a b"));
        assert!(!valid_member("成员"));
        assert!(!valid_member("a=b"));
    }

    #[test]
    fn parse_member_kv_shapes() {
        let (keyed, unkeyed) = parse_member_kv(&[
            "lobster=nats-lobster".to_string(),
            "plain-pass".to_string(),
            "bad member=x".to_string(),
        ]);
        assert_eq!(keyed.get("lobster").unwrap(), "nats-lobster");
        assert_eq!(unkeyed, vec!["plain-pass".to_string(), "bad member=x".to_string()]);
    }

    #[test]
    fn resolve_creds_multi_member_rejects_bare_values() {
        let members = vec!["a".to_string(), "b".to_string()];
        // 多成员 + 裸值 → FATAL
        assert!(resolve_member_creds(&members, &["user1".to_string()], &[]).is_err());
        assert!(resolve_member_creds(&members, &[], &["p1".to_string()]).is_err());
        // 多成员 + keyed 但文件不存在 → missing
        let err = resolve_member_creds(
            &members,
            &["a=ua".to_string(), "b=ub".to_string()],
            &["a=/no/such/a.pass".to_string(), "b=/no/such/b.pass".to_string()],
        )
        .unwrap_err();
        assert_eq!(err.len(), 2);
        assert!(err[0].contains("口令文件不存在"));
    }

    #[test]
    fn clamp_timeout_bounds_match_python() {
        assert_eq!(clamp_timeout(None), DEFAULT_TIMEOUT_S);
        assert_eq!(clamp_timeout(Some(&json!("abc"))), DEFAULT_TIMEOUT_S);
        assert_eq!(clamp_timeout(Some(&json!(0))), DEFAULT_TIMEOUT_S);
        assert_eq!(clamp_timeout(Some(&json!(-5))), DEFAULT_TIMEOUT_S);
        assert_eq!(clamp_timeout(Some(&json!(9999))), MAX_TIMEOUT_S);
        assert_eq!(clamp_timeout(Some(&json!(0.01))), MIN_TIMEOUT_S);
        assert_eq!(clamp_timeout(Some(&json!(30))), 30.0);
    }

    #[test]
    fn request_payload_validation() {
        // 缺 text
        let err = parse_request_payload(br#"{"from":"bob"}"#).unwrap_err();
        assert_eq!(err["error"], "bad_request");
        // 空 text
        let err = parse_request_payload(br#"{"text":"  "}"#).unwrap_err();
        assert_eq!(err["error"], "bad_request");
        // 非对象
        let err = parse_request_payload(br#"[1,2]"#).unwrap_err();
        assert_eq!(err["error"], "bad_request");
        // 正常：timeout_s 被硬墙夹住
        let payload = r#"{"text":"你好","from":"bob","session_id":"s1","timeout_s":500}"#.to_string();
        let (text, caller, session, timeout) =
            parse_request_payload(payload.as_bytes()).unwrap();
        assert_eq!(text, "你好");
        assert_eq!(caller, "bob");
        assert_eq!(session.as_deref(), Some("s1"));
        assert_eq!(timeout, HARD_ROUND_CAP_S);
        // 超长 payload
        let big = vec![b'x'; MAX_PAYLOAD_BYTES + 1];
        let err = parse_request_payload(&big).unwrap_err();
        assert_eq!(err["error"], "bad_request");
    }

    #[test]
    fn err_envelope_status_optional() {
        let e = err_envelope("timeout", "慢了", None);
        assert_eq!(e["ok"], false);
        assert!(e.get("status").is_none());
        let e = err_envelope("http_error", "HTTP 500: boom", Some(500));
        assert_eq!(e["status"], 500);
        // detail 截断
        let long = "字".repeat(300);
        let e = err_envelope("x", &long, None);
        assert!(e["detail"].as_str().unwrap().ends_with('…'));
    }

    #[test]
    fn counters_thread_safe_and_snapshot() {
        let c = Counters::new();
        let c2 = Arc::new(c);
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let c = c2.clone();
                std::thread::spawn(move || {
                    for _ in 0..100 {
                        c.inc("ok");
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(c2.snapshot().get("ok"), Some(&400));
        assert_eq!(c2.snapshot().get("rx_requests"), Some(&0)); // 初始键都在
    }

    #[test]
    fn task_gate_admit_release_and_busy() {
        let gate = TaskGate::new(4);
        assert!(gate.admit("a", 2).is_ok());
        assert!(gate.admit("a", 2).is_ok());
        // 第 3 个 → 拒
        let reason = gate.admit("a", 2).unwrap_err();
        assert!(reason.contains("在途已达上限"));
        gate.release("a");
        assert!(gate.admit("a", 2).is_ok());
    }

    #[test]
    fn task_gate_queue_full_rejects() {
        let gate = TaskGate::new(1);
        assert!(gate.enqueue(b"1".to_vec(), "r".into(), "a").is_ok());
        assert!(gate.enqueue(b"2".to_vec(), "r".into(), "a").is_err());
        let item = gate.pop(Duration::from_millis(10)).unwrap();
        assert_eq!(item.0, b"1".to_vec());
    }

    #[test]
    fn truncate_text_char_based() {
        assert_eq!(truncate_text("abc", 5), "abc");
        assert_eq!(truncate_text("中文截断测试", 3), "中文截…");
    }

    #[test]
    fn log_event_smoke() {
        // 只验证不 panic；输出人类可读
        log_event("req.done", Some("lobster"), &[("elapsed_ms", json!(12.3456))]);
    }
}
