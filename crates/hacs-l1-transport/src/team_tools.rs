//! L1 通信层：**团队小工具共享逻辑**（对应 Python `team_roster.py` /
//! `team_call.py` / `team_broadcast.py` 的纯函数部分）。
//!
//! 三个工具都是「连 NATS 中枢干一件事」的短命 CLI，共享：
//!
//! * 心跳窗口收集（`team.hb.>` → 按成员去重、ts 新者胜）；
//!   * 点名载荷构造（`{text, timeout_s, session_id?}`，timeout 夹取 [0.1, 300]）；
//!   * 群发结果汇总行（✅/❌ + 160 字符截断）。
//!
//! 进程装配在 `hacs-c::bin/team_roster|team_call|team_broadcast`。

use std::collections::HashMap;

use serde_json::{json, Value};

pub const MAX_TIMEOUT_S: f64 = 300.0;
pub const MIN_TIMEOUT_S: f64 = 0.1;
pub const HB_PREFIX: &str = "team.hb.";
pub const REQ_FMT: &str = "a2a.v1.req.";
pub const REPLY_PREVIEW_MAX: usize = 160;

/// 心跳窗口收集：`member → 最新心跳对象`（ts 新者胜；无 ts 视为 0）。
#[derive(Default)]
pub struct HbWindow {
    seen: HashMap<String, Value>,
}

impl HbWindow {
    pub fn new() -> Self {
        Self::default()
    }

    /// 喂一条心跳 payload（非 JSON / 无 member 的静默忽略，与 Python on_hb 同口径）。
    pub fn feed(&mut self, payload: &[u8]) {
        let Ok(text) = std::str::from_utf8(payload) else {
            return;
        };
        let Ok(obj) = serde_json::from_str::<Value>(text) else {
            return;
        };
        let Some(member) = obj.get("member").and_then(Value::as_str) else {
            return;
        };
        if member.is_empty() {
            return;
        }
        let ts = obj.get("ts").and_then(Value::as_i64).unwrap_or(0);
        let replace = match self.seen.get(member) {
            None => true,
            Some(prev) => ts >= prev.get("ts").and_then(Value::as_i64).unwrap_or(0),
        };
        if replace {
            self.seen.insert(member.to_string(), obj);
        }
    }

    /// Python `discover_online` 口径：从心跳里抠成员名（优先 `members` 列表，
    /// 退回单个 `member`），去重返回排序名单。
    pub fn discover_members(&self) -> Vec<String> {
        let mut names = std::collections::BTreeSet::new();
        for obj in self.seen.values() {
            match obj.get("members").and_then(Value::as_array) {
                Some(list) => {
                    for n in list {
                        if let Some(s) = n.as_str() {
                            if !s.is_empty() {
                                names.insert(s.to_string());
                            }
                        }
                    }
                }
                // Python 口径：有 members 列表就不再回退到单个 member
                None => {
                    if let Some(s) = obj.get("member").and_then(Value::as_str) {
                        if !s.is_empty() {
                            names.insert(s.to_string());
                        }
                    }
                }
            }
        }
        names.into_iter().collect()
    }

    /// roster 表：按成员名排序输出行（心跳龄按 `now` 折算）。
    pub fn roster_rows(&self, now: i64) -> Vec<Value> {
        let mut out = Vec::new();
        for (m, o) in self.seen.iter() {
            let ts = o.get("ts").and_then(Value::as_i64).unwrap_or(now);
            out.push(json!({
                "member": m,
                "host": o.get("host"),
                "age_s": now - ts,
                "uptime_s": o.get("uptime_s"),
                "version": o.get("version"),
                "pid": o.get("pid"),
                "members": o.get("members"),
            }));
        }
        out.sort_by_key(|r| r["member"].as_str().unwrap_or("").to_string());
        out
    }

    pub fn len(&self) -> usize {
        self.seen.len()
    }

    pub fn is_empty(&self) -> bool {
        self.seen.is_empty()
    }
}

/// 点名 timeout 夹取：`max(0.1, min(timeout, 300))`。
pub fn clamp_call_timeout(timeout: f64) -> f64 {
    timeout.clamp(MIN_TIMEOUT_S, MAX_TIMEOUT_S)
}

/// 点名载荷：`{text, timeout_s}` + 可选 `session_id`（与 A2A 平面同形状）。
pub fn build_call_payload(text: &str, session_id: Option<&str>, timeout_s: f64) -> Value {
    let mut payload = json!({
        "text": text,
        "timeout_s": timeout_s,
    });
    if let Some(s) = session_id {
        if !s.is_empty() {
            payload["session_id"] = json!(s);
        }
    }
    payload
}

/// 单成员群发结果（`call_one` / `call_one_nowait` 的输出形状）。
#[derive(Debug, Clone, Default)]
pub struct MemberOutcome {
    pub member: String,
    pub ok: bool,
    pub reply: Option<String>,
    pub error: Option<String>,
    pub elapsed: f64,
}

impl MemberOutcome {
    /// 汇总一行：`✅ 成员  6.1s  回复预览`（160 字符截断 + 换行折叠）。
    pub fn summary_line(&self) -> String {
        let mark = if self.ok { "✅" } else { "❌" };
        let rep = self
            .reply
            .clone()
            .or_else(|| self.error.clone())
            .unwrap_or_default()
            .replace('\n', " ");
        let rep = if rep.chars().count() > REPLY_PREVIEW_MAX {
            let head: String = rep.chars().take(REPLY_PREVIEW_MAX).collect();
            format!("{head}…")
        } else {
            rep
        };
        format!("  {mark} {:<12} {:>6.1}s  {}", self.member, self.elapsed, rep)
    }
}

/// 群发汇总 JSON（`--json` / `--out` 的输出形状）。
pub fn broadcast_summary(subject: &str, results: &[MemberOutcome], total: usize, no_wait: bool, elapsed: f64) -> Value {
    let ok = results.iter().filter(|r| r.ok).count();
    json!({
        "subject": subject,
        "total": total,
        "ok": ok,
        "no_wait": no_wait,
        "elapsed": elapsed,
        "results": results.iter().map(|r| json!({
            "member": r.member,
            "ok": r.ok,
            "reply": r.reply,
            "error": r.error,
            "elapsed": r.elapsed,
        })).collect::<Vec<_>>(),
    })
}

/// A2A 客户端应答的人类可读出口（`a2a_call.py` 非 JSON 模式）：
/// dict 有 `reply` → 打 reply；否则 pretty JSON；解析失败 → 原文。
pub fn humanize_a2a_body(text: &str) -> String {
    let Ok(data) = serde_json::from_str::<Value>(text) else {
        return text.to_string();
    };
    if !data.is_object() {
        return text.to_string();
    }
    if let Some(reply) = data.get("reply").and_then(Value::as_str) {
        return format!("{}\n", reply.trim_end());
    }
    serde_json::to_string_pretty(&data).unwrap_or_else(|_| text.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hb(member: &str, ts: i64, extra: Value) -> Vec<u8> {
        let mut obj = json!({"member": member, "ts": ts});
        if let (Some(a), Some(b)) = (obj.as_object_mut(), extra.as_object()) {
            for (k, v) in b {
                a.insert(k.clone(), v.clone());
            }
        }
        obj.to_string().into_bytes()
    }

    #[test]
    fn hb_window_newest_ts_wins() {
        let mut w = HbWindow::new();
        w.feed(&hb("a", 100, json!({})));
        w.feed(&hb("a", 90, json!({"version": "old"}))); // 旧的不覆盖
        w.feed(&hb("b", 200, json!({"host": "h1"})));
        w.feed(b"not json");
        w.feed(b"{\"no_member\":1}");
        assert_eq!(w.len(), 2);
        let rows = w.roster_rows(300);
        assert_eq!(rows[0]["member"], "a");
        assert_eq!(rows[0]["age_s"], 200);
        assert_eq!(rows[1]["host"], "h1");
    }

    #[test]
    fn discover_members_prefers_members_list() {
        let mut w = HbWindow::new();
        w.feed(&hb("hub", 1, json!({"members": ["lobster", "top001", ""]})));
        w.feed(&hb("solo", 2, json!({})));
        // 有 members 列表的心跳不回退到 member 本身（Python elif 口径）
        assert_eq!(w.discover_members(), vec!["lobster", "solo", "top001"]);
    }

    #[test]
    fn call_timeout_clamp() {
        assert_eq!(clamp_call_timeout(0.0), MIN_TIMEOUT_S);
        assert_eq!(clamp_call_timeout(9999.0), MAX_TIMEOUT_S);
        assert_eq!(clamp_call_timeout(120.0), 120.0);
    }

    #[test]
    fn call_payload_shape() {
        let p = build_call_payload("你好", Some("meet-1"), 30.0);
        assert_eq!(p["text"], "你好");
        assert_eq!(p["timeout_s"], 30.0);
        assert_eq!(p["session_id"], "meet-1");
        let p = build_call_payload("x", None, 30.0);
        assert!(p.get("session_id").is_none());
        let p = build_call_payload("x", Some(""), 30.0);
        assert!(p.get("session_id").is_none()); // 空 session 不带
    }

    #[test]
    fn outcome_summary_line() {
        let o = MemberOutcome {
            member: "lobster".into(),
            ok: true,
            reply: Some("好\n的".into()),
            error: None,
            elapsed: 6.13,
        };
        let line = o.summary_line();
        assert!(line.contains("✅"));
        assert!(line.contains("lobster"));
        assert!(line.contains("好 的")); // 换行折叠
        let bad = MemberOutcome {
            member: "x".into(),
            ok: false,
            reply: None,
            error: Some("超时未回应".into()),
            elapsed: 120.0,
        };
        assert!(bad.summary_line().contains("❌"));
        assert!(bad.summary_line().contains("超时未回应"));
        // 长回复截断
        let long = MemberOutcome {
            member: "y".into(),
            ok: true,
            reply: Some("字".repeat(200)),
            error: None,
            elapsed: 1.0,
        };
        assert!(long.summary_line().ends_with('…'));
    }

    #[test]
    fn broadcast_summary_counts() {
        let results = vec![
            MemberOutcome { member: "a".into(), ok: true, reply: Some("r".into()), error: None, elapsed: 1.0 },
            MemberOutcome { member: "b".into(), ok: false, reply: None, error: Some("e".into()), elapsed: 2.0 },
        ];
        let s = broadcast_summary("群发", &results, 2, false, 3.0);
        assert_eq!(s["ok"], 1);
        assert_eq!(s["total"], 2);
        assert_eq!(s["results"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn a2a_body_humanize() {
        assert_eq!(humanize_a2a_body("plain"), "plain");
        assert_eq!(humanize_a2a_body(r#"{"reply":"第一行\n"}"#), "第一行\n");
        let pretty = humanize_a2a_body(r#"{"ok":true}"#);
        assert!(pretty.contains("\"ok\": true"));
    }
}
