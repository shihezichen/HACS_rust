//! `team_group` —— 成员侧协作域：临时会议 + 临时群（对应 Python
//! `hub/l3_domain/team_group.py`，345 行，逐函数对齐）。
//!
//! 域拆分（刀15）：team.py 原把群聊/会议压在通信层里，本件只换位置不换行为——
//! 会议走 NATS 点名（`a2a.v1.req.<member>`，L1 nats_client），群消息走对话台 HTTP
//! （X-Agent-Id 署名）。纯逻辑落这里；argv/装配在 bin。
//!
//! 对齐口径：
//! * 会议四要素之"召集"信封 `{"text","session_id","timeout_s","from"}` 逐字段；
//! * `build_meeting_body` 文案逐行（含回报格式行）；
//! * 群铁律：Agent 发言必须 @ 至少一个目标（服务端硬拒），`--to` 缺省 `@all`；
//! * pull 拉全群增量（谁说的都看得见），`my_pending` 单独列出。

use serde_json::{json, Value};

/// `~/.team_meetings.jsonl` 兜底默认。
pub fn default_meetings_file() -> String {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
    format!("{home}/.team_meetings.jsonl")
}

// ---------------------------------------------------------------- meet：临时会议

/// `meet-%Y%m%d-%H%M-<4位随机小写>`（与 Python `make_session_id` 同形）。
pub fn make_session_id(now: i64, rand4: &str) -> String {
    let (y, mo, d, h, mi) = beijing::beijing_parts(now);
    format!("meet-{y:04}{mo:02}{d:02}-{h:02}{mi:02}-{rand4}")
}

/// 召集信正文：文案逐行对齐 Python `build_meeting_body`。
pub fn build_meeting_body(
    goal: &str,
    agenda: &[String],
    members: &[String],
    session_id: &str,
) -> String {
    let mut lines = vec![
        format!("【临时会议召集】session_id={session_id}"),
        format!("目标：{goal}"),
        "议程：".to_string(),
    ];
    for (i, item) in agenda.iter().enumerate() {
        lines.push(format!("  {}. {}", i + 1, item));
    }
    lines.push(format!("参会成员：{}", members.join(", ")));
    lines.push("请每位参会成员按以下格式回报（一行，不要额外寒暄）：".to_string());
    lines.push("<你的成员名>|<真实主机名>|<一句话观点/结论>".to_string());
    lines.join("\n")
}

pub mod beijing {
    //! 北京时间的年月日时分（civil 历法，UTC+8 固定偏移）。
    pub fn beijing_parts(now: i64) -> (i64, i64, i64, i64, i64) {
        let bjt = now + 8 * 3600;
        let days = bjt.div_euclid(86400);
        let secs = bjt.rem_euclid(86400);
        // days_from_civil 反演（Howard Hinnant civil_from_days）
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z.rem_euclid(146_097);
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        let y = if m <= 2 { y + 1 } else { y };
        (y, m, d, (secs / 3600) as i64, ((secs % 3600) / 60) as i64)
    }
}

// ---------------------------------------------------------------- 群（契约 §I）

/// 对话台 HTTP 出参：(HTTP 码, 解析好的 JSON / `{"raw": ...}`)。
/// Agent 身份用 X-Agent-Id（只在 tailnet 来源被服务端接受）。
pub fn chat_call(
    base: &str,
    path: &str,
    method: &str,
    body: Option<&Value>,
    member: Option<&str>,
    timeout: std::time::Duration,
) -> (u16, Value) {
    let url = format!("{}{}", base.trim_end_matches('/'), path);
    let mut headers: Vec<(&str, &str)> = Vec::new();
    let member_buf;
    if let Some(m) = member {
        member_buf = m.to_string();
        headers.push(("X-Agent-Id", member_buf.as_str()));
    }
    match hacs_l1_transport::http_client::request_json(&url, method, body, &headers, timeout) {
        Ok((status, v)) => (status, v),
        Err(e) => (0, json!({"raw": e.0})),
    }
}

/// `_chat_error`：`HTTP <code> <code>：<message>`（对象信封）。
pub fn chat_error(status: u16, body: &Value) -> String {
    let err = body.get("error").cloned().unwrap_or(Value::Null);
    let code = err.get("code").and_then(Value::as_str).unwrap_or("?");
    let msg = err.get("message").and_then(Value::as_str).unwrap_or("");
    format!("HTTP {status} {code}：{msg}")
}

/// `mentions` 归一：`--to` 缺省 `@all`；`@all` 单发；否则逗号拆。
pub fn normalize_mentions(raw_to: Option<&str>) -> Result<Vec<String>, String> {
    let raw = match raw_to {
        None => "@all".to_string(),
        Some(s) => s.trim().to_string(),
    };
    if raw.is_empty() {
        return Err("--to 不能为空：写 @all 或成员 id 逗号分隔（Agent 发言必须 @，服务端会硬拒无 @ 的发言）".into());
    }
    Ok(if raw == "@all" {
        vec!["@all".to_string()]
    } else {
        raw.split(',')
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .collect()
    })
}

// ---------------------------------------------------------------- 留痕 jsonl

/// 会议留痕一行（`append_jsonl`：目录缺则建，UTF-8 追加一行）。
pub fn append_jsonl(path: &str, record: &Value) -> std::io::Result<()> {
    use std::io::Write;
    let full = shellexpand_home(path);
    if let Some(dir) = std::path::Path::new(&full).parent() {
        if !dir.as_os_str().is_empty() && !dir.exists() {
            std::fs::create_dir_all(dir)?;
        }
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(full)?;
    writeln!(f, "{}", serde_json::to_string(record).unwrap_or_default())
}

/// `history`：扫 jsonl，同一 session_id 取**最后一条**。
pub fn find_last_by_session(path: &str, session_id: &str) -> Option<Value> {
    let full = shellexpand_home(path);
    let text = std::fs::read_to_string(full).ok()?;
    let mut found = None;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(obj) = serde_json::from_str::<Value>(line) {
            if obj.get("session_id").and_then(Value::as_str) == Some(session_id) {
                found = Some(obj); // 同一 session_id 可能重发过，取文件里最后一条
            }
        }
    }
    found
}

fn shellexpand_home(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
        format!("{home}/{rest}")
    } else {
        path.to_string()
    }
}

// ---------------------------------------------------------------- 纯打印面（对齐 cmd_* 的输出文案）

/// `cmd_meet` 的结果汇总行（到会/缺席 + 汇总）。
pub fn meet_report(results: &[Value], members_total: usize) -> String {
    let mut out = String::new();
    for r in results {
        let rep = r
            .get("reply")
            .or_else(|| r.get("error"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .replace('\n', " ");
        let elapsed = r.get("elapsed").and_then(Value::as_f64).unwrap_or(0.0);
        if r.get("ok").and_then(Value::as_bool).unwrap_or(false) {
            out.push_str(&format!(
                "  [到会] {:<14} {:>6.1}  {}\n",
                r.get("member").and_then(Value::as_str).unwrap_or(""),
                elapsed,
                rep
            ));
        } else {
            out.push_str(&format!(
                "  [缺席] {:<14} {:>6.1}  {}（不折算进通过率）\n",
                r.get("member").and_then(Value::as_str).unwrap_or(""),
                elapsed,
                rep
            ));
        }
    }
    let present = results
        .iter()
        .filter(|r| r.get("ok").and_then(Value::as_bool).unwrap_or(false))
        .count();
    out.push_str(&format!("汇总：到会 {present} / 应到 {members_total}"));
    out
}

/// `cmd_group_pull` 的逐条消息行。
pub fn pull_line(member: Option<&str>, msg: &Value) -> String {
    let meta = msg.get("meta").cloned().unwrap_or(Value::Null);
    let mentions: Vec<String> = meta
        .get("mentions")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let needs: Vec<String> = meta
        .get("needs_response")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let who = msg
        .pointer("/author/display")
        .or_else(|| msg.pointer("/author/id"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let to = if mentions.is_empty() {
        "-".to_string()
    } else {
        mentions.join("、")
    };
    let mark = if member
        .map(|m| needs.iter().any(|n| n == m))
        .unwrap_or(false)
    {
        " *"
    } else {
        ""
    };
    let text = msg
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or("")
        .replace('\n', " ");
    format!(
        "  [{}]{mark} {who} → {to}：{text}",
        msg.get("seq").and_then(Value::as_i64).unwrap_or(0)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_meeting_body_exact_lines() {
        let body = build_meeting_body(
            "定发布日",
            &["盘点进度".into(), "拍日子".into()],
            &["alpha".into(), "beta".into()],
            "meet-20261017-1015-ab12",
        );
        let want = "【临时会议召集】session_id=meet-20261017-1015-ab12\n\
                    目标：定发布日\n\
                    议程：\n\
                    \x20 1. 盘点进度\n\
                    \x20 2. 拍日子\n\
                    参会成员：alpha, beta\n\
                    请每位参会成员按以下格式回报（一行，不要额外寒暄）：\n\
                    <你的成员名>|<真实主机名>|<一句话观点/结论>";
        assert_eq!(body, want);
    }

    #[test]
    fn test_session_id_shape() {
        // 2026-10-17 10:15 北京 = 2026-10-17 02:15 UTC
        let sid = make_session_id(1_792_203_300, "ab12");
        assert!(sid.starts_with("meet-20261017-1015-"));
        assert!(sid.ends_with("-ab12"));
    }

    #[test]
    fn test_mentions_rules() {
        assert_eq!(normalize_mentions(None).unwrap(), vec!["@all"]);
        assert!(normalize_mentions(Some("  ")).is_err());
        assert_eq!(
            normalize_mentions(Some("a, b,,c")).unwrap(),
            vec!["a", "b", "c"]
        );
        assert_eq!(normalize_mentions(Some("@all")).unwrap(), vec!["@all"]);
    }

    #[test]
    fn test_chat_error_shape() {
        let body = json!({"error": {"code": "forbidden", "message": "没有 @ 任何成员"}});
        assert_eq!(
            chat_error(403, &body),
            "HTTP 403 forbidden：没有 @ 任何成员"
        );
        assert_eq!(chat_error(500, &json!({})), "HTTP 500 ?：");
    }

    #[test]
    fn test_jsonl_append_and_find_last() {
        let dir = std::env::temp_dir().join(format!("tg_test_{}", std::process::id()));
        let p = dir.join("~/sub/x.jsonl").to_string_lossy().to_string();
        // append_jsonl 会展开 ~ —— 用不带 ~ 的路径直接测
        let real = dir.join("x.jsonl").to_string_lossy().to_string();
        append_jsonl(&real, &json!({"session_id": "s1", "n": 1})).unwrap();
        append_jsonl(&real, &json!({"session_id": "s2", "n": 2})).unwrap();
        append_jsonl(&real, &json!({"session_id": "s1", "n": 3})).unwrap();
        let found = find_last_by_session(&real, "s1").unwrap();
        assert_eq!(found["n"], 3);
        assert!(find_last_by_session(&real, "nope").is_none());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn test_meet_report_and_pull_line() {
        let results = vec![
            json!({"ok": true, "member": "alpha", "elapsed": 1.23, "reply": "好"}),
            json!({"ok": false, "member": "beta", "elapsed": 9.9, "error": "timeout"}),
        ];
        let rep = meet_report(&results, 2);
        assert!(rep.contains("[到会] alpha"));
        assert!(rep.contains("[缺席] beta"));
        assert!(rep.ends_with("汇总：到会 1 / 应到 2"));

        let msg = json!({
            "seq": 7, "text": "第一行\n第二行",
            "author": {"id": "alpha", "display": "甲"},
            "meta": {"mentions": ["@all"], "needs_response": ["beta"]}
        });
        let line = pull_line(Some("beta"), &msg);
        assert!(line.contains("[7] * 甲 → @all：第一行 第二行"));
        assert!(!pull_line(Some("alpha"), &msg).contains(" *"));
        assert!(pull_line(None, &msg).contains("→ @all："));
    }
}

// ---------------------------------------------------------------- 去重（窗口内同内容跳过）

pub const DEDUP_WINDOW_SECONDS: i64 = 30 * 60;

/// `_dedup_key`：sha1(subject_norm + b"\\0" + body) 的 hex。
pub fn dedup_key(subject_norm: &str, body: &str) -> String {
    let mut data = subject_norm.as_bytes().to_vec();
    data.push(0);
    data.extend_from_slice(body.as_bytes());
    hacs_l2_adapter::cli_backend::sha1(&data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// `check_and_mark_dedup`：命中窗口内已发过 → (false, key)；否则先标记再 (true, key)。
/// 标记先于发送，失败由调用方 `rollback_dedup` 撤销。文件损坏按空处理。
pub fn check_and_mark_dedup(
    dedup_file: &str,
    subject_norm: &str,
    body: &str,
    force: bool,
    now: i64,
) -> (bool, String) {
    let key = dedup_key(subject_norm, body);
    let full = shellexpand_home(dedup_file);
    let mut data: std::collections::HashMap<String, i64> = std::fs::read_to_string(&full)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    data.retain(|_, v| now - *v < DEDUP_WINDOW_SECONDS);
    if !force {
        if let Some(ts) = data.get(&key) {
            if now - *ts < DEDUP_WINDOW_SECONDS {
                return (false, key);
            }
        }
    }
    data.insert(key.clone(), now);
    let tmp = format!("{full}.tmp.{}", std::process::id());
    if std::fs::write(&tmp, serde_json::to_string(&data).unwrap_or_default()).is_ok() {
        let _ = std::fs::rename(&tmp, &full);
    }
    (true, key)
}

/// `rollback_dedup`：发送失败时撤销标记。
pub fn rollback_dedup(dedup_file: &str, key: &str) {
    let full = shellexpand_home(dedup_file);
    let Ok(text) = std::fs::read_to_string(&full) else {
        return;
    };
    let Ok(mut data) = serde_json::from_str::<std::collections::HashMap<String, i64>>(&text) else {
        return;
    };
    data.remove(key);
    let tmp = format!("{full}.tmp.{}", std::process::id());
    if std::fs::write(&tmp, serde_json::to_string(&data).unwrap_or_default()).is_ok() {
        let _ = std::fs::rename(&tmp, &full);
    }
}

#[cfg(test)]
mod dedup_tests {
    use super::*;

    #[test]
    fn test_dedup_window_flow() {
        let dir = std::env::temp_dir().join(format!("tg_dedup_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("d.json").to_string_lossy().to_string();
        // 第一次：标记并放行
        let (ok1, k) = check_and_mark_dedup(&f, "meet:a,b", "正文", false, 1000);
        assert!(ok1);
        // 窗口内同内容：拦
        let (ok2, _) = check_and_mark_dedup(&f, "meet:a,b", "正文", false, 1100);
        assert!(!ok2);
        // force：放行
        let (ok3, _) = check_and_mark_dedup(&f, "meet:a,b", "正文", true, 1200);
        assert!(ok3);
        // 回滚后：再发又放行
        rollback_dedup(&f, &k);
        let (ok4, _) = check_and_mark_dedup(&f, "meet:a,b", "正文", false, 1300);
        assert!(ok4);
        // 窗口外（最后一次标记 1300 的 30 分钟后）：旧条目被清，放行
        let (ok5, _) = check_and_mark_dedup(
            &f,
            "meet:a,b",
            "正文",
            false,
            1300 + DEDUP_WINDOW_SECONDS + 1,
        );
        assert!(ok5);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn test_dedup_key_differs() {
        assert_ne!(dedup_key("a", "b"), dedup_key("ab", ""));
        assert_eq!(dedup_key("a", "b"), dedup_key("a", "b"));
    }
}

/// bin 用：直接暴露北京时间的年月日时分。
pub fn beijing_parts_pub(now: i64) -> (i64, i64, i64, i64, i64) {
    beijing::beijing_parts(now)
}
