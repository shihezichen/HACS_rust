//! `roundtable` —— 圆桌会议调度器（对应 Python `hub/l3_domain/roundtable.py`）。
//!
//! 口径（=Python，逐条对齐）：
//! * 一次性跑完即退；按议程逐项点名发言，每条发言写黑板（POST /entries，bin 组合根）
//!   + roundtable_turns；每项判收敛或撞上限 → 主持人出结论 → 当场确认；
//!   全部议程走完出总体结论 + actions（有人有期限）。
//! * 会前门禁：goal/agenda 缺一不开（exit 2）；**round_no < 2 不许判收敛**；
//!   确认通过 = 无 object 且 agree >= ⌊在场/2⌋+1（分母=应答者）；出席率分母=应到人数。
//! * 缺席=疑似 bug：自动开 kind=defect 单（24h，owner=本人，同一场一人一条）。
//! * 解析降级：抠不出 JSON 重试 1 次后整段塞 content + refs 自引；refs 空 → 不入库记 error。
//! * 省 token：prompt 不放完整历史发言，只放片段（k<=12、240 字符截断）+ max_chars 夹逼
//!   （先砍最旧片段 → 砍非当前议程行 → 硬截）。
//! * Store/Blackboard/Transport 都是 trait，NullStore/NullBlackboard/DryRun 供演练与单测。

use serde_json::{json, Map, Value};

pub(crate) fn json_of<T: Into<Value>>(v: Option<T>) -> Value {
    v.map(Into::into).unwrap_or(Value::Null)
}
use std::collections::HashSet;

pub const VERSION: &str = "1.2";
pub const DEFAULT_MAX_ROUNDS: i64 = 2;
pub const DEFAULT_MAX_TOTAL_ROUNDS: i64 = 0;
pub const DEFECT_DUE_HOURS: i64 = 24;
pub const DEFAULT_DEADLINE_S: f64 = 900.0;
pub const DEFAULT_MAX_CHARS: usize = 6000;
pub const SNIPPET_MAX: usize = 12;
pub const SNIPPET_CHARS: usize = 240;
pub const CONTENT_MAX: usize = 8000;
pub const SIM_THRESHOLD: f64 = 0.85;
pub const SIM_RATIO: f64 = 2.0 / 3.0;
pub const TOKENS_PER_MEMBER_ROUND: i64 = 4000;
pub const CHARS_PER_TOKEN: i64 = 2;
pub const ABSENT_STREAK_LIMIT: i64 = 2;
pub const PING_TIMEOUT_S: f64 = 6.0;
pub const ASK_TIMEOUT_S: f64 = 180.0;
pub const HTTP_TIMEOUT_S: f64 = 15.0;
pub const AGENDA_TITLE_CHARS: usize = 60;
pub const AGENDA_MAX_ITEMS: usize = 20;
pub const CONCLUSION_CHARS: usize = 600;

pub const STANCES: [&str; 4] = ["support", "oppose", "neutral", "need_info"];
pub const VOTES: [&str; 2] = ["continue", "converge"];
pub const REF_TYPES: [&str; 5] = ["entry", "file", "measurement", "session", "turn"];
pub const VERDICTS: [&str; 4] = ["agree", "object", "abstain", "absent"];

pub const MUTABLE_SESSION_COLUMNS: [&str; 11] = [
    "state",
    "round_no",
    "ended_at",
    "conclusion_entry",
    "note",
    "confirmed_at",
    "confirm_summary",
    "expected_count",
    "present_count",
    "absent_count",
    "attendance_rate",
];
pub const MUTABLE_AGENDA_COLUMNS: [&str; 4] = ["conclusion", "conclusion_entry", "status", "note"];
pub const SESSION_STATES: [&str; 5] = ["running", "converged", "timeout", "aborted", "incomplete"];

pub fn default_moderator() -> String {
    std::env::var("AGENT_TEAM_MODERATOR")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "hermes".into())
}

pub fn moderator_fallback() -> Vec<String> {
    std::env::var("AGENT_TEAM_MODERATOR_FALLBACK")
        .unwrap_or_else(|_| "hermes,lobster,top001".into())
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

// ---------------- 小工具 ----------------

pub fn now_epoch_f() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// UTC `YYYYMMDD-HHMMSS`。
fn utc_stamp(epoch: f64) -> String {
    let (y, mo, d, h, mi) = crate::team_group::beijing_parts_pub(epoch as i64);
    format!(
        "{y:04}{mo:02}{d:02}-{h:02}{mi:02}{:02}",
        (epoch as i64) % 60
    )
}

pub fn new_session_id(now: f64) -> String {
    // rt-YYYYMMDD-HHMMSS-xxxx（uuid4 前 4 hex；无 rand 依赖 → 纳秒+pid 混合熵）
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    let pid = std::process::id() as u64;
    let x = nanos ^ (pid << 17) ^ (now.to_bits() >> 13);
    format!("rt-{}-{:04x}", utc_stamp(now), x & 0xffff)
}

pub fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        text.to_string()
    } else {
        let head: String = text.chars().take(limit).collect();
        format!("{head}…")
    }
}

pub fn one_line(text: &str, limit: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate(&flat, limit)
}

pub fn read_secret(path: &str) -> Result<String, String> {
    std::fs::read_to_string(path)
        .map(|s| s.trim().to_string())
        .map_err(|e| format!("读不到凭据文件 {path}：{e}"))
}

pub fn read_dsn(path: &str) -> Result<String, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("读不到 DSN 文件 {path}：{e}"))?;
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("BB_PG_DSN=") {
            let mut value = v.trim_end_matches(['\r', '\n']).trim().to_string();
            let bytes: Vec<char> = value.chars().collect();
            if bytes.len() >= 2
                && bytes[0] == bytes[bytes.len() - 1]
                && (bytes[0] == '\'' || bytes[0] == '"')
            {
                value = bytes[1..bytes.len() - 1].iter().collect();
            }
            return Ok(value);
        }
    }
    Ok(String::new())
}

// ---------------- 解析：从回包里抠 JSON ----------------

/// 扫出文本里所有"括号平衡"的顶层 {...} 片段（字符串内的括号不计）。
pub fn balanced_objects(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut start: i64 = -1;
    let mut in_str = false;
    let mut escaped = false;
    for (i, ch) in text.char_indices() {
        if in_str {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_str = false;
            }
            continue;
        }
        match ch {
            '"' => in_str = true,
            '{' => {
                if depth == 0 {
                    start = i as i64;
                }
                depth += 1;
            }
            '}' => {
                if depth > 0 {
                    depth -= 1;
                    if depth == 0 && start >= 0 {
                        let s = start as usize;
                        out.push(text[s..i + ch.len_utf8()].to_string());
                        start = -1;
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// **只接受单个 JSON 对象**。① ```json 代码块 ② 整段 ③ 扫括号平衡片段。
pub fn extract_json_object(raw: &str) -> (Option<Value>, Option<String>) {
    let text = raw.trim();
    if text.is_empty() {
        return (None, Some("回包为空".into()));
    }
    let mut candidates: Vec<String> = Vec::new();
    // ```json ... ``` 代码块（regex-free：逐个围栏段扫描）
    let mut rest = text;
    while let Some(p) = rest.find("```") {
        let after = &rest[p + 3..];
        let body_start = after.find('\n').map(|n| n + 1).unwrap_or(0);
        let body = &after[body_start..];
        if let Some(q) = body.find("```") {
            candidates.push(body[..q].trim().to_string());
            rest = &body[q + 3..];
        } else {
            break;
        }
    }
    candidates.push(text.to_string());
    candidates.extend(balanced_objects(text));
    for cand in candidates {
        let cand = cand.trim();
        if !cand.starts_with('{') {
            continue;
        }
        if let Ok(obj) = serde_json::from_str::<Value>(cand) {
            if obj.is_object() {
                return (Some(obj), None);
            }
        }
    }
    (None, Some("回包里找不到单个合法 JSON 对象".into()))
}

/// 把 refs 规整成 [{"type":..., "value":...}]。认不出来的元素直接丢。
pub fn normalize_refs(value: &Value) -> Vec<Value> {
    let arr = match value.as_array() {
        Some(a) => a,
        None => return Vec::new(),
    };
    let mut out = Vec::new();
    for item in arr {
        if let Some(obj) = item.as_object() {
            let rtype = obj
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_lowercase();
            let mut rvalue = obj.get("value").cloned();
            if rvalue.as_ref().map(|v| v.is_null()).unwrap_or(true) {
                rvalue = obj.get("entry_id").cloned();
            }
            let empty = matches!(rvalue, Some(Value::String(ref s)) if s.trim().is_empty())
                || rvalue.is_none();
            if empty {
                continue;
            }
            let rtype = if REF_TYPES.contains(&rtype.as_str()) {
                rtype
            } else if rvalue.as_ref().map(|v| v.is_i64()).unwrap_or(false) {
                "entry".to_string()
            } else {
                "file".to_string()
            };
            out.push(json!({"type": rtype, "value": rvalue.unwrap()}));
        } else if item.is_boolean() {
            continue;
        } else if let Some(n) = item.as_i64() {
            out.push(json!({"type": "entry", "value": n}));
        } else if let Some(s) = item.as_str() {
            let token = s.trim();
            if token.is_empty() {
                continue;
            }
            let digits = token.trim_start_matches('#');
            if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) {
                out.push(json!({"type": "entry", "value": digits.parse::<i64>().unwrap_or(0)}));
            } else {
                out.push(json!({"type": "file", "value": token}));
            }
        }
    }
    out
}

/// 解析一条发言回包。返回 (规整字段, 错误原因)。
/// 错误分两种："回包里找不到..."→整体失败可降级；"refs 为空..."→不入库记 error。
pub fn parse_turn(raw: &str) -> (Option<Map<String, Value>>, Option<String>) {
    let (obj, err) = extract_json_object(raw);
    let obj = match obj {
        Some(o) => o,
        None => return (None, err),
    };
    let content = match obj["content"].as_str() {
        Some(c) if !c.trim().is_empty() => c.trim().to_string(),
        _ => return (None, Some("content 为空或不是字符串".into())),
    };
    let kind = obj["kind"]
        .as_str()
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "opinion".into());
    let mut stance = obj["stance"]
        .as_str()
        .map(|s| s.trim().to_lowercase())
        .unwrap_or_else(|| "neutral".into());
    if !STANCES.contains(&stance.as_str()) {
        stance = "neutral".into();
    }
    let mut vote = obj["vote"]
        .as_str()
        .map(|s| s.trim().to_lowercase())
        .unwrap_or_else(|| "continue".into());
    if !VOTES.contains(&vote.as_str()) {
        vote = "continue".into();
    }
    let mut confidence = obj["confidence"].as_f64().unwrap_or_else(|| {
        obj["confidence"]
            .as_str()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0.5)
    });
    if !confidence.is_finite() {
        confidence = 0.5;
    }
    confidence = confidence.clamp(0.0, 1.0);
    let refs = normalize_refs(&obj["refs"]);
    let mut turn = Map::new();
    turn.insert("kind".into(), json!(kind));
    turn.insert("content".into(), json!(truncate(&content, CONTENT_MAX)));
    turn.insert("stance".into(), json!(stance));
    turn.insert("vote".into(), json!(vote));
    turn.insert("confidence".into(), json!(confidence));
    turn.insert("refs".into(), Value::Array(refs.clone()));
    turn.insert("actions".into(), obj["actions"].clone());
    turn.insert("issues".into(), obj["issues"].clone());
    if refs.is_empty() {
        return (
            Some(turn),
            Some("refs 为空或缺失：没有证据的观点不入库".into()),
        );
    }
    (Some(turn), None)
}

/// 把 --agenda 原始串解析成 [{"seq","title","expected"}]。标题为空即非法。
pub fn parse_agenda(raw_items: &[String]) -> (Vec<Value>, Option<String>) {
    let mut items: Vec<Value> = Vec::new();
    for raw in raw_items {
        for chunk in raw.split([';', '；', '\n']) {
            let chunk = chunk.trim();
            if chunk.is_empty() {
                continue;
            }
            let mut parts = chunk.splitn(2, ['|', '｜']);
            let title = parts.next().unwrap_or("").trim();
            let expected = parts.next().unwrap_or("").trim();
            if title.is_empty() {
                return (Vec::new(), Some(format!("议程项 {chunk:?} 的标题为空")));
            }
            items.push(json!({
                "seq": items.len() + 1,
                "title": title,
                "expected": if expected.is_empty() { "给出该项的明确结论" } else { expected },
            }));
            if items.len() > AGENDA_MAX_ITEMS {
                return (
                    Vec::new(),
                    Some(format!("议程项超过 {AGENDA_MAX_ITEMS} 条，拆成多场会开")),
                );
            }
        }
    }
    if items.is_empty() {
        return (Vec::new(), Some("议程为空：至少要有 1 项".into()));
    }
    (items, None)
}

/// 解析一条确认回包 → (verdict, note, 错误)。抠不出 JSON 一律 abstain，绝不替人点头。
pub fn parse_confirm(raw: &str) -> (String, String, Option<String>) {
    let (obj, err) = extract_json_object(raw);
    let obj = match obj {
        Some(o) => o,
        None => {
            return (
                "abstain".into(),
                String::new(),
                Some(err.unwrap_or_else(|| "确认回包无法解析".into())),
            )
        }
    };
    let verdict = obj["verdict"].as_str().unwrap_or("").trim().to_lowercase();
    let note = obj["note"].as_str().unwrap_or("").trim().to_string();
    if !["agree", "object", "abstain"].contains(&verdict.as_str()) {
        return (
            "abstain".into(),
            note,
            Some(format!("verdict 取值非法（拿到 {:?}）", obj["verdict"])),
        );
    }
    (verdict, note, None)
}

/// 汇总确认 → confirm_summary。通过 = 无 object 且 agree >= ⌊在场/2⌋+1。
pub fn tally_confirmations(rows: &[Value]) -> Value {
    let mut summary = json!({
        "agree": [], "object": [], "abstain": [], "absent": [],
    });
    for row in rows {
        let verdict = row["verdict"].as_str().unwrap_or("");
        if let Some(bucket) = summary[verdict].as_array_mut() {
            bucket.push(row["member"].clone());
        }
    }
    let total = rows.len() as i64;
    let present = total - summary["absent"].as_array().map(|a| a.len()).unwrap_or(0) as i64;
    let required = if present > 0 { present / 2 + 1 } else { 0 };
    summary["required"] = json!(required);
    summary["total"] = json!(total);
    summary["present"] = json!(present);
    let n_obj = summary["object"].as_array().map(|a| a.len()).unwrap_or(0);
    let n_agree = summary["agree"].as_array().map(|a| a.len()).unwrap_or(0);
    summary["passed"] = json!(present > 0 && n_obj == 0 && n_agree as i64 >= required);
    summary
}

/// 解析 ISO8601 → UTC epoch 秒。naive 一律当 UTC。认不出来返回 None。
pub fn parse_iso_datetime(value: &Value) -> Option<i64> {
    let text = match value {
        Value::String(s) => s.trim().to_string(),
        _ if value.is_null() => return None,
        other => other
            .as_str()
            .map(|s| s.trim().to_string())
            .unwrap_or_default(),
    };
    if text.is_empty() {
        return None;
    }
    let text = if text.ends_with('Z') || text.ends_with('z') {
        format!("{}+00:00", &text[..text.len() - 1])
    } else {
        text
    };
    hacs_x_identity::view_auth::parse_ts(&text)
}

/// 校验主持人给的 actions。返回 (合格事项, 不合格原因)。
/// ① what 非空；② owner 在 roster 内；③ due_at 是未来 ISO8601。
pub fn validate_actions(
    value: &Value,
    roster: &[String],
    now_epoch: i64,
    agenda_seqs: &[i64],
) -> (Vec<Map<String, Value>>, Vec<String>) {
    let roster_set: HashSet<&String> = roster.iter().filter(|s| !s.trim().is_empty()).collect();
    let seq_set: HashSet<i64> = agenda_seqs.iter().copied().collect();
    let (mut ok, mut errors) = (Vec::new(), Vec::new());
    let arr = match value.as_array() {
        Some(a) if !a.is_empty() => a,
        _ => return (ok, vec!["actions 缺失或不是非空数组".into()]),
    };
    for (idx, item) in arr.iter().enumerate() {
        let i = idx + 1;
        let obj = match item.as_object() {
            Some(o) => o,
            None => {
                errors.push(format!("第 {i} 条 action 不是对象"));
                continue;
            }
        };
        let what = obj
            .get("what")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        let owner = obj
            .get("owner")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        let due_raw = obj.get("due_at").cloned().unwrap_or(Value::Null);
        let mut reasons: Vec<String> = Vec::new();
        if what.is_empty() {
            reasons.push("what 为空".into());
        }
        if owner.is_empty() {
            reasons.push("owner 为空".into());
        } else if !roster_set.is_empty() && !roster_set.contains(&owner.to_string()) {
            let mut sorted: Vec<&String> = roster_set.iter().copied().collect();
            sorted.sort();
            let names: Vec<String> = sorted.iter().map(|s| s.to_string()).collect();
            reasons.push(format!(
                "owner={owner:?} 不在 roster 内（可选：{}）",
                names.join(",")
            ));
        }
        let due = parse_iso_datetime(&due_raw);
        match due {
            None => reasons.push(format!("due_at={due_raw:?} 不是合法 ISO8601 时间")),
            Some(t) if t <= now_epoch => {
                reasons.push(format!("due_at={} 不晚于当前时间", iso_utc(t)))
            }
            _ => {}
        }
        if !reasons.is_empty() {
            errors.push(format!("第 {i} 条 action：{}", reasons.join("；")));
            continue;
        }
        let mut agenda_seq = obj.get("agenda_seq").and_then(|v| v.as_i64());
        if let Some(sq) = agenda_seq {
            if !seq_set.is_empty() && !seq_set.contains(&sq) {
                agenda_seq = None; // 指了不存在的议程项：不算错，挂空
            }
        }
        let mut m = Map::new();
        m.insert("what".into(), json!(truncate(what, 500)));
        m.insert("owner".into(), json!(owner));
        m.insert("due_at".into(), json!(due.unwrap_or(0)));
        m.insert("agenda_seq".into(), json_of(agenda_seq));
        ok.push(m);
    }
    (ok, errors)
}

/// epoch → ISO8601 UTC（秒级，+00:00 后缀）。
pub fn iso_utc(epoch: i64) -> String {
    let (y, mo, d, h, mi) = crate::team_group::beijing_parts_pub(epoch - 8 * 3600);
    let s = epoch.rem_euclid(60);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}+00:00")
}

/// 解析主持人主动登记的遗留问题（可选字段）。认不出来的直接丢。
pub fn parse_issues(value: &Value) -> Vec<Map<String, Value>> {
    let mut out = Vec::new();
    let arr = match value.as_array() {
        Some(a) => a,
        None => return out,
    };
    for item in arr {
        if let Some(s) = item.as_str() {
            let title = s.trim();
            if !title.is_empty() {
                let mut m = Map::new();
                m.insert("title".into(), json!(truncate(title, 300)));
                m.insert("detail".into(), json!(""));
                m.insert("owner".into(), Value::Null);
                out.push(m);
            }
        } else if let Some(obj) = item.as_object() {
            let title = obj
                .get("title")
                .or_else(|| obj.get("what"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            if title.is_empty() {
                continue;
            }
            let mut m = Map::new();
            m.insert("title".into(), json!(truncate(&title, 300)));
            m.insert(
                "detail".into(),
                json!(truncate(
                    obj.get("detail").and_then(|v| v.as_str()).unwrap_or(""),
                    1000
                )),
            );
            m.insert(
                "owner".into(),
                match obj.get("owner").and_then(|v| v.as_str()) {
                    Some(o) if !o.trim().is_empty() => json!(o.trim()),
                    _ => Value::Null,
                },
            );
            m.insert(
                "due_at".into(),
                obj.get("due_at")
                    .and_then(parse_iso_datetime)
                    .map(|e| Value::from(e))
                    .unwrap_or(Value::Null),
            );
            out.push(m);
        }
    }
    out
}

// ---------------- 相似度（trgm 口径，不依赖 embedder） ----------------

fn is_word_char(c: char) -> bool {
    c.is_ascii_digit() || ('a'..='z').contains(&c) || ('一'..='鿿').contains(&c)
}

fn normalize_text(text: &str) -> String {
    let lower = text.to_lowercase();
    let mut out = String::new();
    let mut prev_space = true;
    for c in lower.chars() {
        if is_word_char(c) {
            out.push(c);
            prev_space = false;
        } else if !prev_space {
            out.push(' ');
            prev_space = true;
        }
    }
    out.trim().to_string()
}

fn trigrams(text: &str) -> HashSet<String> {
    let norm = normalize_text(text);
    if norm.is_empty() {
        return HashSet::new();
    }
    let padded = format!("  {norm} ");
    let chars: Vec<char> = padded.chars().collect();
    if chars.len() < 3 {
        return HashSet::new();
    }
    (0..=chars.len() - 3)
        .map(|i| chars[i..i + 3].iter().collect())
        .collect()
}

/// 两段文本的相似度（0~1）。三元组不够退回词 Jaccard。刻意不依赖 embedder。
pub fn similarity(a: &str, b: &str) -> f64 {
    let (ta, tb) = (trigrams(a), trigrams(b));
    if !ta.is_empty() && !tb.is_empty() {
        let inter = ta.intersection(&tb).count();
        let union = ta.union(&tb).count();
        return inter as f64 / union as f64;
    }
    let norm_a = normalize_text(a);
    let norm_b = normalize_text(b);
    let wa: HashSet<&str> = norm_a.split_whitespace().collect();
    let wb: HashSet<&str> = norm_b.split_whitespace().collect();
    if wa.is_empty() || wb.is_empty() {
        return if norm_a == norm_b { 1.0 } else { 0.0 };
    }
    let inter = wa.intersection(&wb).count();
    let union = wa.union(&wb).count();
    inter as f64 / union as f64
}

pub fn max_similarity(text: &str, corpus: &[String]) -> f64 {
    corpus
        .iter()
        .map(|other| similarity(text, other))
        .fold(0.0f64, f64::max)
}

/// 本轮新增条目里"跟既有条目高度重复"的占比。没有既有条目就是 0。
pub fn repetition_ratio(new_texts: &[String], prior_texts: &[String]) -> f64 {
    if new_texts.is_empty() || prior_texts.is_empty() {
        return 0.0;
    }
    let hits = new_texts
        .iter()
        .filter(|t| max_similarity(t, prior_texts) >= SIM_THRESHOLD)
        .count();
    hits as f64 / new_texts.len() as f64
}

/// 收敛判据①②③。round_no < 2 一律不许收敛。
pub fn check_convergence(
    round_no: i64,
    turns: &[Value],
    new_texts: &[String],
    prior_texts: &[String],
) -> (bool, String, String) {
    if round_no < 2 {
        return (
            false,
            String::new(),
            format!("第 {round_no} 轮：未满 2 轮，不允许判收敛"),
        );
    }
    let spoke: Vec<&Value> = turns.iter().filter(|t| t["status"] == "ok").collect();
    let ratio = repetition_ratio(new_texts, prior_texts);
    if ratio >= SIM_RATIO {
        let hits = (ratio * new_texts.len() as f64).round() as i64;
        return (
            true,
            "similarity".into(),
            format!(
                "本轮 {hits}/{} 条与既有条目相似度 >= {SIM_THRESHOLD:.2}（占比 {:.0}%）",
                new_texts.len(),
                ratio * 100.0
            ),
        );
    }
    let support = spoke.iter().filter(|t| t["stance"] == "support").count() as i64;
    let against = spoke
        .iter()
        .filter(|t| t["stance"] == "oppose" || t["stance"] == "need_info")
        .count() as i64;
    if against == 0 && support >= 2 {
        return (
            true,
            "stance".into(),
            format!("本轮无人反对/要资料，且 {support} 人明确支持"),
        );
    }
    let converge_votes = spoke.iter().filter(|t| t["vote"] == "converge").count() as i64;
    let need = spoke.len() as i64 / 2 + 1;
    if !spoke.is_empty() && converge_votes >= need {
        return (
            true,
            "vote".into(),
            format!(
                "本轮 {converge_votes}/{} 人投 converge（过半需 {need}）",
                spoke.len()
            ),
        );
    }
    (
        false,
        String::new(),
        format!(
            "本轮未达成收敛（重复率 {ratio:.2} / 支持 {support} / 反对+待资料 {against} / converge {converge_votes}）"
        ),
    )
}

/// 判据④：硬上限。
pub fn check_hard_stop(
    round_no: i64,
    max_rounds: i64,
    elapsed_s: f64,
    deadline_s: f64,
    chars_used: i64,
    budget_chars: i64,
) -> (bool, String) {
    if round_no >= max_rounds {
        return (true, format!("达到最大轮次 {max_rounds}"));
    }
    if elapsed_s >= deadline_s {
        return (
            true,
            format!("超过 deadline {deadline_s:.0}s（已用 {elapsed_s:.0}s）"),
        );
    }
    if budget_chars > 0 && chars_used >= budget_chars {
        return (
            true,
            format!(
                "超过字符预算 {budget_chars}（已用 {chars_used}，约 {} token）",
                chars_used / CHARS_PER_TOKEN
            ),
        );
    }
    (false, String::new())
}

// ---------------- 单测 ----------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_json_object_paths() {
        // ① 代码块 ② 整段 ③ 脏文本括号扫描
        let (o, e) = extract_json_object("```json\n{\"a\":1}\n```");
        assert!(o.is_some() && e.is_none());
        let (o, e) = extract_json_object("{\"a\":1,\"b\":\"{内嵌括号}\"}");
        assert!(o.is_some() && e.is_none());
        let (o, e) = extract_json_object("好的，这是我的回复：{\"a\":1} 请查收");
        assert!(o.is_some() && e.is_none());
        let (o, _e) = extract_json_object("不是 JSON");
        assert!(o.is_none());
        // 括号内的字符串里有 { } 不应破坏平衡
        let (o, _e) = extract_json_object("xx {\"s\":\"}\"} yy");
        assert!(o.is_some());
    }

    #[test]
    fn test_parse_turn_contract() {
        let (t, e) = parse_turn(
            "{\"kind\":\"opinion\",\"content\":\"看法\",\"stance\":\"support\",\"vote\":\"continue\",\"confidence\":0.8,\"refs\":[{\"type\":\"entry\",\"value\":12}]}",
        );
        let t = t.unwrap();
        assert!(e.is_none());
        assert_eq!(t["stance"], json!("support"));
        assert_eq!(t["refs"][0]["value"], json!(12));
        // refs 空 → 不入库错误
        let (t, e) = parse_turn("{\"content\":\"x\",\"refs\":[]}");
        assert!(t.is_some() && e.is_some());
        // 非法 stance/vote 归一；#12 → entry
        let (t, e) = parse_turn("{\"content\":\"x\",\"stance\":\"angry\",\"vote\":\"maybe\",\"refs\":[\"#12\",\"file.txt\"]}");
        let t = t.unwrap();
        assert!(e.is_none());
        assert_eq!(t["stance"], json!("neutral"));
        assert_eq!(t["vote"], json!("continue"));
        assert_eq!(t["refs"][0]["type"], json!("entry"));
        assert_eq!(t["refs"][1]["type"], json!("file"));
    }

    #[test]
    fn test_parse_agenda_gate() {
        let (items, e) = parse_agenda(&["频率|数字; 保留期|天数".to_string()]);
        assert!(e.is_none());
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["expected"], json!("数字"));
        let (_items, e) = parse_agenda(&["|没标题".to_string()]);
        assert!(e.is_some());
        let (_items, e) = parse_agenda(&[]);
        assert_eq!(e.unwrap(), "议程为空：至少要有 1 项");
    }

    #[test]
    fn test_tally_confirmations_present_denominator() {
        // absent 不进分母：2 应答（1 agree 1 object）→ present=2, required=2, passed=false（有 object）
        let rows = vec![
            json!({"member": "a", "verdict": "agree"}),
            json!({"member": "b", "verdict": "object"}),
            json!({"member": "c", "verdict": "absent"}),
        ];
        let s = tally_confirmations(&rows);
        assert_eq!(s["present"], json!(2));
        assert_eq!(s["required"], json!(2));
        assert_eq!(s["passed"], json!(false));
        // 3 人在场 2 agree 无 object → passed（required=2）
        let rows = vec![
            json!({"member": "a", "verdict": "agree"}),
            json!({"member": "b", "verdict": "agree"}),
            json!({"member": "c", "verdict": "abstain"}),
        ];
        assert_eq!(tally_confirmations(&rows)["passed"], json!(true));
    }

    #[test]
    fn test_validate_actions_rules() {
        let now = 1_800_000_000i64;
        let roster = vec!["a".to_string(), "b".to_string()];
        let (ok, errs) = validate_actions(
            &json!([
                {"what": "写文档", "owner": "a", "due_at": "2030-01-01T00:00:00+00:00"},
                {"what": "", "owner": "a", "due_at": "2030-01-01T00:00:00+00:00"},
                {"what": "x", "owner": "全体", "due_at": "2030-01-01T00:00:00+00:00"},
                {"what": "y", "owner": "b", "due_at": "1999-01-01T00:00:00+00:00"},
                {"what": "z", "owner": "b", "due_at": "not-a-time"},
            ]),
            &roster,
            now,
            &[1],
        );
        assert_eq!(ok.len(), 1);
        assert_eq!(errs.len(), 4);
    }

    #[test]
    fn test_convergence_rules() {
        // round 1 不许收敛
        let (c, _r, why) = check_convergence(1, &[], &["x".into()], &[]);
        assert!(!c && why.contains("未满 2 轮"));
        // 相似收敛：3 条新发言里 2 条与旧条目几乎相同 → ratio 2/3 >= 2/3
        let old = vec!["采用每周一次全量备份并且保留三十天".to_string()];
        let new = vec![
            "采用每周一次全量备份并且保留三十天".to_string(),
            "采用每周一次全量备份并且保留三十天".to_string(),
            "完全不同的第三条发言内容关于监控告警".to_string(),
        ];
        let turns: Vec<Value> = (0..3).map(|_| json!({"status":"ok"})).collect();
        let (c, r, _w) = check_convergence(2, &turns, &new, &old);
        assert!(c && r == "similarity");
    }

    #[test]
    fn test_similarity_word_fallback() {
        assert!(similarity("abc def", "abc def") > 0.99);
        assert!(similarity("abc def", "xyz qrs") < 0.5);
    }

    #[test]
    fn test_parse_confirm_fallback_abstain() {
        let (v, _n, _e) = parse_confirm("完全不是 JSON");
        assert_eq!(v, "abstain");
        let (v, _n, e) = parse_confirm("{\"verdict\":\"maybe\"}");
        assert_eq!(v, "abstain");
        assert!(e.is_some());
        let (v, _n, e) = parse_confirm("{\"verdict\":\"agree\",\"note\":\"ok\"}");
        assert_eq!(v, "agree");
        assert!(e.is_none());
    }

    #[test]
    fn test_render_prompt_budget_and_snippets() {
        let agenda = vec![
            json!({"seq": 1, "title": "频率", "expected": "数字"}),
            json!({"seq": 2, "title": "保留期", "expected": "天数"}),
        ];
        let snippets: Vec<Value> = (0..20)
            .map(|i| json!({"entry_id": i, "author": "a", "kind": "opinion", "content": "x".repeat(100)}))
            .collect();
        let (p, used) = crate::roundtable_sched::render_prompt(
            "a",
            "t",
            "q",
            1,
            2,
            &snippets,
            6000,
            "",
            "g",
            &agenda,
            Some(1),
        );
        assert!(used <= SNIPPET_MAX);
        assert!(p.chars().count() <= 6000);
        assert!(p.contains("▶ 1.")); // 当前项标记
        assert!(p.contains("【输出契约】"));
        // 极小预算：砍片段 + 只留当前项 + 硬截
        let (p2, used2) = crate::roundtable_sched::render_prompt(
            "a",
            "t",
            "q",
            1,
            2,
            &snippets,
            400,
            "",
            "g",
            &agenda,
            Some(1),
        );
        assert!(p2.chars().count() <= 400);
        assert!(used2 <= used);
    }

    #[test]
    fn test_read_dsn_and_session_id_shape() {
        let dir = std::env::temp_dir().join(format!("rttest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("pg.env");
        std::fs::write(&f, "BB_PG_DSN=host=x port=5 dbname=y\n").unwrap();
        assert_eq!(
            read_dsn(f.to_str().unwrap()).unwrap(),
            "host=x port=5 dbname=y"
        );
        let sid = new_session_id(1_800_000_000.0);
        assert!(sid.starts_with("rt-") && sid.len() >= 15);
    }
}
