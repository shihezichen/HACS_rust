//! `roundtable` 编排层（对应 Python roundtable.py 的 Roundtable/Store/Blackboard/Transport 抽象）。
//! Store/Blackboard/Transport 全是 trait：NullStore/NullBlackboard 供演练，
//! PgStore/BlackboardClient/NatsTransport 在 bin 组合根。X 横切层铁律：本模块不连库、不碰 socket。

use crate::roundtable_core::*;
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};

fn json_of<T: Into<Value>>(v: Option<T>) -> Value {
    v.map(Into::into).unwrap_or(Value::Null)
}

pub const DEFAULT_TOPIC: &str = "roundtable";
pub const OUTPUT_CONTRACT: &str = "只回一个 JSON 对象，前后不要任何多余文字（可以用 ```json 包起来）：\n{\"kind\":\"opinion\",\"content\":\"你的观点，200 字以内\",\"stance\":\"support|oppose|neutral|need_info\",\"vote\":\"continue|converge\",\"confidence\":0.0,\"refs\":[{\"type\":\"entry\",\"value\":12}]}\nrefs 必须至少一条，type 取 entry（上面片段的 # 号）/ file（文件路径）/ measurement（实测数据）——**refs 为空的发言不会被记录**。\nvote=converge 表示你认为讨论已经够了，continue 表示还需要再谈一轮。";
pub const RETRY_HINT: &str =
    "\n\n【上一次回复不合格】原因：%s。请**只**输出一个 JSON 对象，且 refs 至少一条。";
pub const CONFIRM_CONTRACT: &str = "只回一个 JSON 对象，前后不要任何多余文字：\n{\"verdict\":\"agree|object|abstain\",\"note\":\"一句话理由（反对必须写清反对什么）\"}\nagree=当场确认这条结论；object=不同意，该项会被记为未确认并进遗留问题；abstain=弃权。**解析不出来一律按 abstain 处理，不会替你点头。**";
pub const OVERALL_CONTRACT: &str = "\n【输出契约】只回一个 JSON 对象：\n{\"kind\":\"decision\",\"content\":\"总体结论 + 依据 + 还有什么没定\",\"stance\":\"neutral\",\"vote\":\"converge\",\"confidence\":0.0,\"refs\":[{\"type\":\"entry\",\"value\":12}],\"actions\":[{\"what\":\"要做的事\",\"owner\":\"成员名\",\"due_at\":\"2026-01-31T18:00:00+08:00\",\"agenda_seq\":1}],\"issues\":[{\"title\":\"还没定的事\",\"owner\":\"成员名\",\"detail\":\"为什么没定\"}]}\nactions **至少一条**，且每条都要满足：owner 必须是下面【本场成员】里的名字（不许写\"全体\"\"大家\"\"待定\"），due_at 必须是 ISO8601 且晚于现在。\nrefs 必须至少一条。content 里要写清：结论是什么、基于哪几条证据、还有什么没定。";
pub const ITEM_CONTRACT: &str = "\n【输出契约】只回一个 JSON 对象：\n{\"kind\":\"decision\",\"content\":\"本议程项的结论 + 依据\",\"stance\":\"neutral\",\"vote\":\"converge\",\"confidence\":0.0,\"refs\":[{\"type\":\"entry\",\"value\":12}]}\ncontent 只写**当前这一项**的结论，不要提前下别项的结论；refs 至少一条。";

pub fn now_utc_iso() -> String {
    iso_utc(now_epoch_f() as i64)
}

pub fn dedup_i64(values: &[i64]) -> Vec<i64> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for v in values {
        if seen.insert(*v) {
            out.push(*v);
        }
    }
    out
}

pub fn dedup_names(names: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for n in names {
        let name = n.trim().to_string();
        if !name.is_empty() && seen.insert(name.clone()) {
            out.push(name);
        }
    }
    out
}

// ---------------- traits ----------------

pub trait Store {
    fn create_session(&mut self, session: &Value);
    fn update_session(&mut self, session_id: &str, fields: &Map<String, Value>);
    fn add_agenda_items(&mut self, session_id: &str, items: &[Value]);
    fn update_agenda_item(&mut self, session_id: &str, seq: i64, fields: &Map<String, Value>);
    fn add_confirmation(&mut self, row: &Value) -> Option<i64>;
    fn add_action(&mut self, row: &Value) -> Option<i64>;
    fn add_issue(&mut self, row: &Value) -> Option<i64>;
    fn add_turn(&mut self, turn: &Value) -> Option<i64>;
    fn load_session(&self, session_id: &str) -> Option<Value>;
    fn close(&mut self);
}

pub trait Blackboard {
    fn post_entry(
        &mut self,
        topic: &str,
        kind: &str,
        content: &str,
        refs: &Value,
    ) -> (Option<i64>, Option<String>);
    fn list_entries(&mut self, topic: &str, limit: i64) -> Vec<Value>;
}

/// (status, 正文, 错误说明)。status ∈ ok|absent|error。
pub type AskResult = (String, String, String);

pub trait Transport {
    fn name(&self) -> &'static str {
        "nats"
    }
    fn open(&mut self) {}
    fn ping(&mut self, member: &str) -> bool;
    fn ask(&mut self, member: &str, text: &str, timeout: f64) -> AskResult;
    fn close(&mut self) {}
}

pub struct NullStore {
    pub sessions: HashMap<String, Value>,
    pub turns: Vec<Value>,
    pub updates: Vec<(String, Map<String, Value>)>,
    pub agenda_items: Vec<Value>,
    pub confirmations: Vec<Value>,
    pub actions: Vec<Value>,
    pub issues: Vec<Value>,
    next: i64,
    next_aux: i64,
}

impl Default for NullStore {
    fn default() -> Self {
        Self::new()
    }
}

impl NullStore {
    pub fn new() -> Self {
        Self {
            sessions: HashMap::new(),
            turns: Vec::new(),
            updates: Vec::new(),
            agenda_items: Vec::new(),
            confirmations: Vec::new(),
            actions: Vec::new(),
            issues: Vec::new(),
            next: 1,
            next_aux: 1,
        }
    }

    fn append_aux(bucket: &mut Vec<Value>, next_aux: &mut i64, row: &Value) -> i64 {
        let rid = *next_aux;
        *next_aux += 1;
        let mut item = row.clone();
        if let Some(o) = item.as_object_mut() {
            o.insert("id".into(), json!(rid));
        }
        bucket.push(item);
        rid
    }
}

impl Store for NullStore {
    fn create_session(&mut self, session: &Value) {
        let sid = session["session_id"].as_str().unwrap_or("").to_string();
        self.sessions.insert(sid, session.clone());
    }
    fn update_session(&mut self, session_id: &str, fields: &Map<String, Value>) {
        let bad: Vec<&String> = fields
            .keys()
            .filter(|k| !MUTABLE_SESSION_COLUMNS.contains(&k.as_str()))
            .collect();
        if !bad.is_empty() {
            let mut sorted = bad.iter().map(|s| s.to_string()).collect::<Vec<_>>();
            sorted.sort();
            panic!("试图改不可变列：{}", sorted.join(","));
        }
        self.updates.push((session_id.to_string(), fields.clone()));
        let entry = self.sessions.entry(session_id.to_string()).or_default();
        if let Some(o) = entry.as_object_mut() {
            for (k, v) in fields {
                o.insert(k.clone(), v.clone());
            }
        }
    }
    fn add_agenda_items(&mut self, session_id: &str, items: &[Value]) {
        for item in items {
            self.agenda_items.push(json!({
                "session_id": session_id, "seq": item["seq"], "title": item["title"],
                "expected": item["expected"],
                "status": item.get("status").cloned().unwrap_or(json!("pending")),
                "conclusion": Value::Null, "conclusion_entry": Value::Null, "note": Value::Null,
            }));
        }
    }
    fn update_agenda_item(&mut self, session_id: &str, seq: i64, fields: &Map<String, Value>) {
        let bad: Vec<&String> = fields
            .keys()
            .filter(|k| !MUTABLE_AGENDA_COLUMNS.contains(&k.as_str()))
            .collect();
        if !bad.is_empty() {
            let mut sorted = bad.iter().map(|s| s.to_string()).collect::<Vec<_>>();
            sorted.sort();
            panic!("试图改议程项的不可变列：{}", sorted.join(","));
        }
        for row in self.agenda_items.iter_mut() {
            if row["session_id"] == json!(session_id) && row["seq"] == json!(seq) {
                if let Some(o) = row.as_object_mut() {
                    for (k, v) in fields {
                        o.insert(k.clone(), v.clone());
                    }
                }
            }
        }
    }
    fn add_confirmation(&mut self, row: &Value) -> Option<i64> {
        Some(Self::append_aux(
            &mut self.confirmations,
            &mut self.next_aux,
            row,
        ))
    }
    fn add_action(&mut self, row: &Value) -> Option<i64> {
        Some(Self::append_aux(&mut self.actions, &mut self.next_aux, row))
    }
    fn add_issue(&mut self, row: &Value) -> Option<i64> {
        Some(Self::append_aux(&mut self.issues, &mut self.next_aux, row))
    }
    fn add_turn(&mut self, turn: &Value) -> Option<i64> {
        let tid = self.next;
        self.next += 1;
        let mut row = turn.clone();
        if let Some(o) = row.as_object_mut() {
            o.insert("turn_id".into(), json!(tid));
        }
        self.turns.push(row);
        Some(tid)
    }
    fn load_session(&self, session_id: &str) -> Option<Value> {
        self.sessions.get(session_id).cloned()
    }
    fn close(&mut self) {}
}

/// 演练 / --dry-run 用：只记调用，不发网络请求，entry_id 给负数以示假。
pub struct NullBlackboard {
    pub posted: Vec<Value>,
    pub entries: Vec<Value>,
    next: i64,
}

impl Default for NullBlackboard {
    fn default() -> Self {
        Self::new(Vec::new())
    }
}

impl NullBlackboard {
    pub fn new(seed: Vec<Value>) -> Self {
        Self {
            posted: Vec::new(),
            entries: seed,
            next: -1,
        }
    }
}

impl Blackboard for NullBlackboard {
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
        let entry_id = self.next;
        self.next -= 1;
        let row = json!({"entry_id": entry_id, "topic": topic, "kind": kind,
                         "content": content, "refs": refs, "author": "dry-run"});
        self.posted.push(row.clone());
        self.entries.push(row);
        (Some(entry_id), None)
    }
    fn list_entries(&mut self, topic: &str, limit: i64) -> Vec<Value> {
        let rows: Vec<Value> = self
            .entries
            .iter()
            .filter(|e| e["topic"] == json!(topic))
            .rev()
            .take(limit.max(0) as usize)
            .cloned()
            .collect();
        rows
    }
}

/// --dry-run 用：假应答。按成员名轮流出 support/neutral/oppose 合规回包；
/// 认 prompt 类型（确认 → verdict；总体 → 带 actions/issues 的 decision；单项 → decision）。
pub struct DryRunTransport {
    pub scripted: HashMap<String, Vec<String>>,
    pub offline: HashSet<String>,
    pub verdicts: HashMap<String, Vec<String>>,
    pub overall: Vec<String>,
    pub due_days: i64,
    pub asked: Vec<(String, String)>,
    round: HashMap<String, i64>,
}

impl Default for DryRunTransport {
    fn default() -> Self {
        Self::new(HashMap::new(), &[], HashMap::new(), &[], 7)
    }
}

impl DryRunTransport {
    pub fn new(
        scripted: HashMap<String, Vec<String>>,
        offline: &[&str],
        verdicts: HashMap<String, Vec<String>>,
        overall: &[&str],
        due_days: i64,
    ) -> Self {
        Self {
            scripted,
            offline: offline.iter().map(|s| s.to_string()).collect(),
            verdicts,
            overall: overall.iter().map(|s| s.to_string()).collect(),
            due_days,
            asked: Vec::new(),
            round: HashMap::new(),
        }
    }

    fn roster_from_prompt(text: &str) -> Vec<String> {
        match text.find("【本场成员】") {
            Some(p) => text[p + "【本场成员】".len()..]
                .lines()
                .next()
                .unwrap_or("")
                .split(',')
                .map(|m| m.trim().to_string())
                .filter(|m| !m.is_empty())
                .collect(),
            None => Vec::new(),
        }
    }

    fn canned(&self, member: &str) -> String {
        let n = *self.round.get(member).unwrap_or(&0);
        let stance = ["support", "neutral", "oppose"][n.rem_euclid(3) as usize];
        let vote = if n >= 1 { "converge" } else { "continue" };
        json!({
            "kind": "opinion",
            "content": format!("（dry-run）{} 在第 {} 次发言里的看法：按现有证据，{}。", member, n + 1, stance),
            "stance": stance, "vote": vote, "confidence": 0.6,
            "refs": [{"type": "measurement", "value": format!("dry-run-{}-{}", member, n + 1)}],
        })
        .to_string()
    }

    fn canned_confirm(&mut self, member: &str) -> String {
        let verdict = self
            .verdicts
            .get_mut(member)
            .and_then(|q| {
                if q.is_empty() {
                    None
                } else {
                    Some(q.remove(0))
                }
            })
            .unwrap_or_else(|| "agree".into());
        json!({"verdict": verdict, "note": format!("（dry-run）{} 的当场表态", member)}).to_string()
    }

    fn canned_overall(&mut self, member: &str, text: &str) -> String {
        if !self.overall.is_empty() {
            return self.overall.remove(0);
        }
        let roster = Self::roster_from_prompt(text);
        let roster = if roster.is_empty() {
            vec![member.to_string()]
        } else {
            roster
        };
        let due_epoch = now_epoch_f() as i64 + self.due_days.max(1) * 86400;
        json!({
            "kind": "decision",
            "content": "（dry-run）总体结论：按各议程项的结论执行，责任人与截止时间见 actions。",
            "stance": "neutral", "vote": "converge", "confidence": 0.7,
            "refs": [{"type": "measurement", "value": "dry-run-overall"}],
            "actions": [{"what": "把本场结论落到文档并回帖", "owner": roster[0],
                         "due_at": iso_utc(due_epoch), "agenda_seq": 1}],
            "issues": [{"title": "（dry-run）本场没定的事，留到下一场跟踪",
                        "owner": roster[roster.len() - 1], "detail": "dry-run 自动登记"}],
        })
        .to_string()
    }
}

fn canned_item() -> String {
    json!({
        "kind": "decision",
        "content": "（dry-run）本议程项的结论：按本项发言里的多数意见执行。",
        "stance": "neutral", "vote": "converge", "confidence": 0.7,
        "refs": [{"type": "measurement", "value": "dry-run-item"}],
    })
    .to_string()
}

impl Transport for DryRunTransport {
    fn name(&self) -> &'static str {
        "dry-run"
    }
    fn ping(&mut self, member: &str) -> bool {
        !self.offline.contains(member)
    }
    fn ask(&mut self, member: &str, text: &str, _timeout: f64) -> AskResult {
        self.asked.push((member.to_string(), text.to_string()));
        if self.offline.contains(member) {
            return (
                "absent".into(),
                "".to_string(),
                "dry-run：该成员被标记为离线".into(),
            );
        }
        if text.contains("【确认契约】") {
            return ("ok".into(), self.canned_confirm(member), "".into());
        }
        if text.contains("【本场成员】") {
            return ("ok".into(), self.canned_overall(member, text), "".into());
        }
        if text.contains("【现在只为第") {
            return ("ok".into(), canned_item(), "".into());
        }
        if let Some(queue) = self.scripted.get_mut(member) {
            if !queue.is_empty() {
                let reply = queue.remove(0);
                *self.round.entry(member.to_string()).or_insert(0) += 1;
                return ("ok".into(), reply, "".into());
            }
        }
        let reply = self.canned(member);
        *self.round.entry(member.to_string()).or_insert(0) += 1;
        ("ok".into(), reply, "".into())
    }
}

/// 把 teamd / a2a_svc 的回包剥成 (status, 正文, 错误)。不是 JSON 就当纯文本用。
pub fn unwrap_reply(raw: &str) -> AskResult {
    let text = raw.trim();
    if text.is_empty() {
        return ("error".into(), "".to_string(), "对方回了空包".into());
    }
    let obj = match serde_json::from_str::<Value>(text) {
        Ok(v) => v,
        Err(_) => return ("ok".into(), text.to_string(), "".into()),
    };
    if !obj.is_object() {
        return ("ok".into(), text.to_string(), "".into());
    }
    if obj["ok"].as_bool().unwrap_or(false) {
        if !obj["reply"].is_null() {
            return (
                "ok".into(),
                obj["reply"].as_str().unwrap_or("").to_string(),
                "".into(),
            );
        }
        return ("ok".into(), text.to_string(), "".into());
    }
    let detail = obj["detail"].as_str().unwrap_or("");
    (
        "error".into(),
        "".into(),
        format!("对方回了错误：{}/{}", obj["error"], truncate(detail, 120)),
    )
}

// ---------------- prompt 渲染 ----------------

pub fn render_agenda_block(
    agenda: &[Value],
    current_seq: Option<i64>,
    title_chars: usize,
) -> Vec<String> {
    agenda
        .iter()
        .map(|item| {
            let mark = if current_seq.is_some() && item["seq"] == json!(current_seq.unwrap()) {
                "▶"
            } else {
                " "
            };
            format!(
                "{} {}. {}",
                mark,
                item["seq"],
                one_line(item["title"].as_str().unwrap_or(""), title_chars)
            )
        })
        .collect()
}

fn fit(head: &str, body: &str, tail: &str, max_chars: usize) -> String {
    let mut prompt = format!("{head}{body}{tail}");
    if prompt.chars().count() > max_chars {
        prompt = prompt.chars().take(max_chars).collect();
    }
    prompt
}

/// 渲染一次点名的 prompt。返回 (prompt, 实际放进去的片段数)。
pub fn render_prompt(
    member: &str,
    topic: &str,
    question: &str,
    round_no: i64,
    max_rounds: i64,
    snippets: &[Value],
    max_chars: usize,
    retry_reason: &str,
    goal: &str,
    agenda: &[Value],
    current_seq: Option<i64>,
) -> (String, usize) {
    let current = agenda
        .iter()
        .find(|i| current_seq.is_some() && i["seq"] == json!(current_seq.unwrap()));

    let build_head = |agenda_rows: &[Value]| -> String {
        let mut parts: Vec<String> = vec![
            format!("你是本场圆桌会议的成员「{member}」。"),
            format!("议题：{topic}"),
        ];
        if !goal.is_empty() {
            parts.push(format!("【会议目标】{}", one_line(goal, 300)));
        }
        parts.push(format!("问题：{question}"));
        if !agenda_rows.is_empty() {
            parts.push(format!(
                "【议程】（共 {} 项，▶ 是当前这一项）",
                agenda.len()
            ));
            parts.extend(render_agenda_block(
                agenda_rows,
                current_seq,
                AGENDA_TITLE_CHARS,
            ));
        }
        if let Some(c) = current {
            parts.push(format!(
                "【当前第 {}/{} 项】{}",
                c["seq"],
                agenda.len(),
                one_line(c["title"].as_str().unwrap_or(""), 200)
            ));
            parts.push(format!(
                "【该项预期产出】{}",
                one_line(c["expected"].as_str().unwrap_or(""), 200)
            ));
        }
        parts.push(format!(
            "当前第 {round_no} 轮（本项最多 {max_rounds} 轮）。"
        ));
        format!("{}\n", parts.join("\n"))
    };

    let mut tail = format!("\n【输出契约】\n{OUTPUT_CONTRACT}");
    if !retry_reason.is_empty() {
        tail += &format!(
            "\n\n【上一次回复不合格】原因：{}。请**只**输出一个 JSON 对象，且 refs 至少一条。",
            one_line(retry_reason, 120)
        );
    }

    let mut rows: Vec<Value> = agenda.to_vec();
    let mut picked: Vec<Value> = snippets.iter().take(SNIPPET_MAX).cloned().collect();
    #[allow(unused_assignments)]
    let mut body = String::new();
    loop {
        if !picked.is_empty() {
            let mut lines = vec!["\n【黑板片段】（已截断，仅供引用，不是完整发言）".to_string()];
            for s in &picked {
                lines.push(format!(
                    "[#{} {}/{}] {}",
                    s.get("entry_id")
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "?".into()),
                    s.get("author").and_then(|v| v.as_str()).unwrap_or("?"),
                    s.get("kind").and_then(|v| v.as_str()).unwrap_or("?"),
                    one_line(
                        s.get("content").and_then(|v| v.as_str()).unwrap_or(""),
                        SNIPPET_CHARS
                    )
                ));
            }
            body = format!("{}\n", lines.join("\n"));
        } else {
            body = "\n【黑板片段】暂无可引用的条目，请直接基于问题作答。\n".into();
        }
        let prompt = format!("{}{}{}", build_head(&rows), body, tail);
        if prompt.chars().count() <= max_chars {
            break;
        }
        if !picked.is_empty() {
            picked.remove(0); // 先从最旧的一条片段开始砍
            continue;
        }
        let trimmed: Vec<Value> = rows
            .iter()
            .filter(|r| current_seq.is_some() && r["seq"] == json!(current_seq.unwrap()))
            .cloned()
            .collect();
        if trimmed.len() < rows.len() {
            rows = trimmed;
            continue;
        }
        break;
    }
    let used = picked.len();
    (fit(&build_head(&rows), &body, &tail, max_chars), used)
}

/// 主持人出结论的 prompt。item 给了 = 该议程项结论；None = 总体结论（actions 硬要求）。
#[allow(clippy::too_many_arguments)]
pub fn render_conclusion_prompt(
    topic: &str,
    question: &str,
    rounds: i64,
    tally: &Map<String, Value>,
    snippets: &[Value],
    max_chars: usize,
    goal: &str,
    agenda: &[Value],
    item: Option<&Value>,
    item_results: &[Value],
    roster: &[String],
    now_text: &str,
    retry_reason: &str,
) -> (String, usize) {
    let build_head =
        |agenda_rows: &[Value]| -> String {
            let mut parts: Vec<String> = vec![
                "你是本场圆桌会议的主持人。".into(),
                format!("议题：{topic}"),
            ];
            if !goal.is_empty() {
                parts.push(format!("【会议目标】{}", one_line(goal, 300)));
            }
            parts.push(format!("问题：{question}"));
            if !agenda_rows.is_empty() {
                parts.push(format!("【议程】（共 {} 项）", agenda.len()));
                parts.extend(render_agenda_block(
                    agenda_rows,
                    item.and_then(|i| i["seq"].as_i64()),
                    AGENDA_TITLE_CHARS,
                ));
            }
            if let Some(it) = item {
                parts.push(format!(
                    "【现在只为第 {} 项出结论】{}",
                    it["seq"],
                    one_line(it["title"].as_str().unwrap_or(""), 200)
                ));
                parts.push(format!(
                    "【该项预期产出】{}",
                    one_line(it["expected"].as_str().unwrap_or(""), 200)
                ));
            } else {
                for res in item_results {
                    parts.push(format!(
                        "[第 {} 项 {}] {}",
                        res["seq"],
                        res["status"],
                        one_line(res["conclusion"].as_str().unwrap_or("（无结论）"), 160)
                    ));
                }
            }
            let tally_of = |k: &str| tally.get(k).and_then(|v| v.as_i64()).unwrap_or(0);
            parts.push(format!(
            "本项/本场共进行了 {rounds} 轮，立场统计：支持 {} / 反对 {} / 中立 {} / 待资料 {}。",
            tally_of("support"), tally_of("oppose"), tally_of("neutral"), tally_of("need_info")
        ));
            if item.is_none() {
                parts.push(format!("【本场成员】{}", roster.join(",")));
                if !now_text.is_empty() {
                    parts.push(format!("【当前时间】{now_text}（due_at 必须晚于它）"));
                }
            }
            format!("{}\n", parts.join("\n"))
        };

    let mut tail = if item.is_some() {
        ITEM_CONTRACT.to_string()
    } else {
        OVERALL_CONTRACT.to_string()
    };
    if !retry_reason.is_empty() {
        tail += &format!(
            "\n【上一次结论不合格】原因：{}。请改正后重新输出。",
            one_line(retry_reason, 300)
        );
    }

    let mut rows: Vec<Value> = agenda.to_vec();
    let mut picked: Vec<Value> = snippets.iter().take(SNIPPET_MAX).cloned().collect();
    #[allow(unused_assignments)]
    let mut body = String::new();
    loop {
        if !picked.is_empty() {
            let mut lines = vec!["\n【本场黑板片段】（已截断）".to_string()];
            for s in &picked {
                lines.push(format!(
                    "[#{} {}] {}",
                    s.get("entry_id")
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "?".into()),
                    s.get("author").and_then(|v| v.as_str()).unwrap_or("?"),
                    one_line(
                        s.get("content").and_then(|v| v.as_str()).unwrap_or(""),
                        SNIPPET_CHARS
                    )
                ));
            }
            body = format!("{}\n", lines.join("\n"));
        } else {
            body = "\n【本场黑板片段】无\n".into();
        }
        let prompt = format!("{}{}{}", build_head(&rows), body, tail);
        if prompt.chars().count() <= max_chars {
            break;
        }
        if !picked.is_empty() {
            picked.remove(0);
            continue;
        }
        if !rows.is_empty() {
            rows.clear();
            continue;
        }
        break;
    }
    let used = picked.len();
    (fit(&build_head(&rows), &body, &tail, max_chars), used)
}

/// 点名要一条「当场确认」。
pub fn render_confirm_prompt(
    member: &str,
    goal: &str,
    target_label: &str,
    conclusion: &str,
    max_chars: usize,
    item: Option<&Value>,
) -> String {
    let mut parts = vec![format!(
        "你是本场圆桌会议的成员「{member}」，现在请对下面这条结论**当场表态**。"
    )];
    if !goal.is_empty() {
        parts.push(format!("【会议目标】{}", one_line(goal, 200)));
    }
    if let Some(it) = item {
        parts.push(format!(
            "【议程第 {} 项】{}",
            it["seq"],
            one_line(it["title"].as_str().unwrap_or(""), 160)
        ));
        parts.push(format!(
            "【该项预期产出】{}",
            one_line(it["expected"].as_str().unwrap_or(""), 160)
        ));
    }
    parts.push(format!("【待确认的结论：{target_label}】"));
    let head = format!("{}\n", parts.join("\n"));
    let tail = format!("\n【确认契约】\n{CONFIRM_CONTRACT}");
    let mut body = format!("{}\n", one_line(conclusion, CONCLUSION_CHARS));
    let head_len = head.chars().count();
    let tail_len = tail.chars().count();
    let max_len = max_chars.max(head_len + tail_len);
    let room = max_len - head_len - tail_len;
    let body_len = body.chars().count();
    if room > 0 && body_len > room {
        body = truncate(&body, room.saturating_sub(1).max(1));
    }
    fit(&head, &body, &tail, max_chars)
}

// ---------------- 调度器 ----------------

pub struct RoundtableConfig {
    pub session_id: String,
    pub topic: String,
    pub question: String,
    pub members: Vec<String>,
    pub max_rounds: i64,
    pub max_total_rounds: i64,
    pub deadline_s: f64,
    pub max_chars: usize,
    pub moderator: String,
    pub start_round: i64,
    pub prompt_log: Option<String>,
    pub goal: String,
    pub agenda: Vec<Value>,
}

pub struct Roundtable<'a> {
    pub cfg_topic: String,
    pub question: String,
    pub members: Vec<String>,
    pub active: Vec<String>,
    pub max_rounds: i64,
    pub max_total_rounds: i64,
    pub deadline_s: f64,
    pub max_chars: usize,
    pub moderator: String,
    pub goal: String,
    pub agenda: Vec<Value>,
    pub agenda_seqs: Vec<i64>,
    pub round_no: i64,
    pub state: String,
    pub note: String,
    pub conclusion_entry: Option<i64>,
    pub chars_used: i64,
    pub absent_streak: HashMap<String, i64>,
    pub spoken: HashMap<String, i64>,
    pub session_texts: Vec<String>,
    pub cited_entries: Vec<i64>,
    pub tally: HashMap<String, i64>,
    pub started: f64,
    pub prompt_log: Option<String>,
    pub budget_chars: i64,
    pub item_results: Vec<Value>,
    pub confirm_rows: Vec<Value>,
    pub action_rows: Vec<Value>,
    pub issue_rows: Vec<Value>,
    pub overall_confirm: Value,
    pub absent_members: HashSet<String>,
    pub responded: HashSet<String>,
    pub absent_log: HashMap<String, Value>,
    defects_opened: bool,
    pub session_id: String,
    out: &'a mut dyn FnMut(&str),
}

impl<'a> Roundtable<'a> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(cfg: RoundtableConfig, out: &'a mut dyn FnMut(&str)) -> Self {
        let mut agenda = cfg.agenda;
        if agenda.is_empty() {
            agenda = vec![json!({
                "seq": 1, "title": one_line(&cfg.question, 120),
                "expected": "给出该问题的明确结论"
            })];
        }
        let agenda_seqs: Vec<i64> = agenda.iter().filter_map(|i| i["seq"].as_i64()).collect();
        let budget_chars = (cfg.members.len() as i64)
            * cfg.max_rounds
            * (agenda.len() as i64)
            * TOKENS_PER_MEMBER_ROUND
            * CHARS_PER_TOKEN;
        let session_id = cfg.session_id.clone();
        Self {
            session_id,
            cfg_topic: cfg.topic,
            question: cfg.question,
            members: cfg.members.clone(),
            active: cfg.members,
            max_rounds: cfg.max_rounds,
            max_total_rounds: cfg.max_total_rounds,
            deadline_s: cfg.deadline_s,
            max_chars: cfg.max_chars,
            moderator: if cfg.moderator.is_empty() {
                default_moderator()
            } else {
                cfg.moderator
            },
            goal: cfg.goal.trim().to_string(),
            agenda,
            agenda_seqs,
            round_no: cfg.start_round,
            state: "running".into(),
            note: String::new(),
            conclusion_entry: None,
            chars_used: 0,
            absent_streak: HashMap::new(),
            spoken: HashMap::new(),
            session_texts: Vec::new(),
            cited_entries: Vec::new(),
            tally: HashMap::from([
                ("support".to_string(), 0),
                ("oppose".to_string(), 0),
                ("neutral".to_string(), 0),
                ("need_info".to_string(), 0),
            ]),
            started: now_epoch_f(),
            prompt_log: cfg.prompt_log,
            budget_chars,
            item_results: Vec::new(),
            confirm_rows: Vec::new(),
            action_rows: Vec::new(),
            issue_rows: Vec::new(),
            overall_confirm: Value::Null,
            absent_members: HashSet::new(),
            responded: HashSet::new(),
            absent_log: HashMap::new(),
            defects_opened: false,
            out,
        }
    }

    pub fn say(&mut self, line: &str) {
        (self.out)(line);
    }

    fn log_prompt(&mut self, member: &str, prompt: &str) {
        let Some(path) = self.prompt_log.clone() else {
            return;
        };
        match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            Ok(f) => {
                use std::io::Write;
                let mut f = f;
                let _ = writeln!(
                    f,
                    "\n===== {} | 第 {} 轮 | {} | {} 字符 =====\n{}\n",
                    self.session_id,
                    self.round_no,
                    member,
                    prompt.chars().count(),
                    prompt
                );
            }
            Err(e) => self.say(&format!(
                "[warn] prompt 日志写不动（{}），继续跑",
                truncate(&e.to_string(), 120)
            )),
        }
    }

    /// 统计 support/oppose/neutral/need_info 的 JSON 视图。
    fn tally_json(&self) -> Map<String, Value> {
        let mut m = Map::new();
        for k in ["support", "oppose", "neutral", "need_info"] {
            m.insert(k.into(), json!(self.tally.get(k).copied().unwrap_or(0)));
        }
        m
    }

    // ---------------- 出席计量 ----------------

    fn mark_present(&mut self, member: &str) {
        self.responded.insert(member.to_string());
    }

    fn mark_absent(&mut self, member: &str, reason: &str, where_: &str) {
        self.absent_members.insert(member.to_string());
        let entry = self
            .absent_log
            .entry(member.to_string())
            .or_insert_with(|| {
                json!({"count": 0, "where": [], "reasons": [],
                       "round_no": self.round_no, "ts": now_utc_iso()})
            });
        if let Some(o) = entry.as_object_mut() {
            o["count"] = json!(o["count"].as_i64().unwrap_or(0) + 1);
            o["where"].as_array_mut().unwrap().push(json!(where_));
            let text = one_line(
                if reason.is_empty() {
                    "点名无应答"
                } else {
                    reason
                },
                120,
            );
            let reasons = o["reasons"].as_array_mut().unwrap();
            if !reasons.iter().any(|r| r == &json!(text)) {
                reasons.push(json!(text));
            }
        }
    }

    pub fn attendance(&self) -> Value {
        let expected = self.members.len() as i64;
        let present_members: Vec<&String> = self
            .members
            .iter()
            .filter(|m| self.responded.contains(m.as_str()))
            .collect();
        let absent_members: Vec<&String> = self
            .members
            .iter()
            .filter(|m| !self.responded.contains(m.as_str()))
            .collect();
        let rate = if expected > 0 {
            json!(present_members.len() as f64 / expected as f64)
        } else {
            Value::Null
        };
        json!({
            "expected": expected,
            "present": present_members.len(),
            "absent": absent_members.len(),
            "rate": rate,
            "present_members": present_members.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            "absent_members": absent_members.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        })
    }

    pub fn attendance_line(&self) -> String {
        let att = self.attendance();
        let rate_text = match att["rate"].as_f64() {
            Some(r) => format!("{:.1}%", r * 100.0),
            None => "-".into(),
        };
        let mut text = format!(
            "出席率 {}/{} = {}",
            att["present"].as_i64().unwrap_or(0),
            att["expected"].as_i64().unwrap_or(0),
            rate_text
        );
        let missing: Vec<String> = att["absent_members"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        if !missing.is_empty() {
            text += &format!(" 缺席[{}]", missing.join(","));
        }
        text
    }

    fn open_absence_defects(&mut self, store: &mut dyn Store) {
        if self.defects_opened {
            return;
        }
        self.defects_opened = true;
        let due_epoch = now_epoch_f() as i64 + DEFECT_DUE_HOURS * 3600;
        for member in self.members.clone() {
            let Some(log) = self.absent_log.get(&member).cloned() else {
                continue;
            };
            let count = log["count"].as_i64().unwrap_or(0);
            let reasons: Vec<String> = log["reasons"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            let wheres: Vec<String> = log["where"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            let reason = if reasons.is_empty() {
                "点名无应答".to_string()
            } else {
                reasons.join("；")
            };
            let evidence = json!([{
                "session_id": self.session_id,
                "round_no": log["round_no"],
                "点名次数": count,
                "失败原因": reason,
                "环节": wheres,
                "ts": log["ts"],
            }]);
            self.record_issue(
                store,
                &format!("成员 {member} 点名无应答（疑似程序缺陷）"),
                &format!(
                    "成员是云主机 / 常驻 Agent，设计上不允许不在线。本场共 {count} 次点名拿不到应答（{}）；失败原因：{reason}。请在 24 小时内查清并闭环。",
                    wheres.iter().take(6).cloned().collect::<Vec<_>>().join("、")
                ),
                Some(&member),
                Some(due_epoch),
                "defect",
                Some(&evidence),
                &[json!({"type": "session", "value": self.session_id})],
            );
            self.say(&format!(
                "[缺陷] 自动开单：成员 {member} 点名无应答 {count} 次 → defect（截止 {}）",
                iso_utc(due_epoch)
            ));
        }
    }

    fn ask_moderator(
        &mut self,
        moderator: &str,
        prompt: &str,
        label: &str,
        transport: &mut dyn Transport,
    ) -> String {
        self.chars_used += prompt.chars().count() as i64;
        self.log_prompt(&format!("{moderator}({label})"), prompt);
        let (status, reply, error) = transport.ask(moderator, prompt, ASK_TIMEOUT_S);
        self.chars_used += reply.chars().count() as i64;
        if status == "ok" && !reply.is_empty() {
            return reply;
        }
        self.say(&format!(
            "[{label}] 主持人没给出回复（{}）",
            truncate(&error, 120)
        ));
        String::new()
    }

    // ---------------- 遗留问题 ----------------

    #[allow(clippy::too_many_arguments)]
    fn record_issue(
        &mut self,
        _store: &mut dyn Store,
        title: &str,
        detail: &str,
        owner: Option<&str>,
        due_at: Option<i64>,
        kind: &str,
        evidence: Option<&Value>,
        refs: &[Value],
    ) {
        let refs_v = if refs.is_empty() {
            vec![json!({"type": "session", "value": self.session_id})]
        } else {
            refs.to_vec()
        };
        self.issue_rows.push(json!({
            "session_id": self.session_id,
            "title": truncate(title, 300),
            "detail": if detail.is_empty() { Value::Null } else { json!(truncate(detail, 2000)) },
            "owner": json_of(owner),
            "status": "open",
            "due_at": json_of(due_at),
            "next_check_at": Value::Null,
            "kind": kind,
            "evidence": evidence.cloned().unwrap_or(Value::Array(vec![])),
            "refs": refs_v,
        }));
    }

    fn flush_issues(&mut self, store: &mut dyn Store) {
        self.open_absence_defects(store);
        // 结论条目出号之后，把 refs 指回结论（放结论之后做，= Python）
        if let Some(back_id) = self.conclusion_entry {
            for row in self.issue_rows.iter_mut() {
                if let Some(o) = row.as_object_mut() {
                    let refs = o.entry("refs").or_insert_with(|| Value::Array(vec![]));
                    let arr = refs.as_array_mut().unwrap();
                    let back = json!({"type": "entry", "value": back_id});
                    if !arr.contains(&back) {
                        arr.push(back);
                    }
                }
            }
        }
        for row in self.issue_rows.clone() {
            if store.add_issue(&row).is_none() {
                self.say(&format!(
                    "[遗留] 写 issue 失败（{}）",
                    row["title"].as_str().unwrap_or("")
                ));
            }
        }
    }

    fn abort(&mut self, store: &mut dyn Store, reason: &str) {
        self.state = "aborted".into();
        self.note = reason.to_string();
        self.say(&format!("[中止] {reason}"));
        let att = self.attendance();
        store.update_session(
            &self.session_id,
            &serde_json::from_str::<Map<String, Value>>(
                &json!({
                    "state": "aborted",
                    "round_no": self.round_no,
                    "ended_at": now_utc_iso(),
                    "note": truncate(reason, 500),
                    "expected_count": att["expected"],
                    "present_count": att["present"],
                    "absent_count": att["absent"],
                    "attendance_rate": att["rate"],
                })
                .to_string(),
            )
            .unwrap_or_default(),
        );
        self.flush_issues(store);
        self.summary();
    }

    fn summary(&mut self) {
        let elapsed = now_epoch_f() - self.started;
        let per_member = self
            .members
            .iter()
            .map(|m| format!("{}={}", m, self.spoken.get(m).copied().unwrap_or(0)))
            .collect::<Vec<_>>()
            .join(" ");
        let confirm_rows = self.confirm_rows.clone();
        let count_of = move |verdict: &str| {
            confirm_rows
                .iter()
                .filter(|r| r["verdict"] == json!(verdict))
                .count() as i64
        };
        let item_results = self.item_results.clone();
        for res in &item_results {
            self.say(&format!(
                "[汇总·议程 {}] {} status={} 确认 agree={}/object={} entry={} ｜ {}",
                res["seq"],
                res["title"],
                res["status"],
                res["confirm"]["agree"]
                    .as_array()
                    .map(|a| a.len())
                    .unwrap_or(0),
                res["confirm"]["object"]
                    .as_array()
                    .map(|a| a.len())
                    .unwrap_or(0),
                res["entry_id"]
                    .as_i64()
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "-".into()),
                one_line(res["conclusion"].as_str().unwrap_or(""), 120)
            ));
        }
        if let Some(oc) = self.overall_confirm.as_object() {
            self.say(&format!(
                "[汇总·总体确认] passed={} agree={}/{} required={}",
                oc.get("passed").map(|v| v.to_string()).unwrap_or_default(),
                oc.get("agree")
                    .and_then(|v| v.as_array())
                    .map(|a| a.len())
                    .unwrap_or(0),
                oc.get("total").and_then(|v| v.as_i64()).unwrap_or(0),
                oc.get("required").and_then(|v| v.as_i64()).unwrap_or(0)
            ));
        }
        let action_rows = self.action_rows.clone();
        for row in &action_rows {
            self.say(&format!(
                "[汇总·落实] {} → {} 截止 {}",
                one_line(row["what"].as_str().unwrap_or(""), 80),
                row["owner"],
                row["due_at"]
            ));
        }
        let issue_rows = self.issue_rows.clone();
        for row in &issue_rows {
            let tag = if row["kind"] == json!("defect") {
                "【疑似缺陷】"
            } else {
                ""
            };
            self.say(&format!(
                "[汇总·遗留] {}{}（跟踪人：{}）",
                tag,
                one_line(row["title"].as_str().unwrap_or(""), 100),
                row["owner"].as_str().unwrap_or("待定")
            ));
        }
        let att = self.attendance();
        let missing: Vec<String> = att["absent_members"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        self.say(&format!(
            "[汇总·出席] 应到 {} 人 / 实到 {} 人 / 缺席 {} 人 —— {}；缺席名单：{}",
            att["expected"].as_i64().unwrap_or(0),
            att["present"].as_i64().unwrap_or(0),
            att["absent"].as_i64().unwrap_or(0),
            self.attendance_line(),
            if missing.is_empty() {
                "无（全员到齐）".to_string()
            } else {
                missing.join("、")
            }
        ));
        let defects = self
            .issue_rows
            .iter()
            .filter(|r| r["kind"] == json!("defect"))
            .cloned()
            .collect::<Vec<_>>();
        if !defects.is_empty() {
            let owners: Vec<String> = defects
                .iter()
                .map(|r| {
                    r["owner"]
                        .as_str()
                        .map(String::from)
                        .unwrap_or_else(|| "?".into())
                })
                .collect();
            self.say(&format!(
                "[汇总·缺陷] 缺席开单 {} 条，24 小时内必须闭环：{}",
                defects.len(),
                owners.join("、")
            ));
        }
        let concluded = self
            .item_results
            .iter()
            .filter(|r| r["status"] == json!("concluded"))
            .count() as i64;
        self.say(&format!(
            "[汇总] session={} state={} rounds={} agenda={}/{} concluded {} confirm[agree={} object={} abstain={} absent={}] actions={} issues={} defects={} speakers[{}] elapsed={:.1}s chars={}(~{}tok) conclusion_entry={}",
            self.session_id, self.state, self.round_no,
            concluded, self.agenda.len(),
            self.attendance_line(),
            count_of("agree"), count_of("object"), count_of("abstain"), count_of("absent"),
            self.action_rows.len(), self.issue_rows.len(), defects.len(),
            per_member, elapsed, self.chars_used, self.chars_used / CHARS_PER_TOKEN,
            self.conclusion_entry.map(|v| v.to_string()).unwrap_or_else(|| "-".into())
        ));
    }

    // ---------------- 主流程 ----------------

    /// 主入口（RunCtx 封装 store/bb/transport，保证 &mut 分离）。
    pub fn run(&mut self, ctx: &mut RunCtx) -> i32 {
        let rc = self.run_inner(ctx);
        if rc == ERR_SIGNAL {
            self.abort(ctx.store, "收到中断（Ctrl-C）");
        } else if rc == ERR_EXCEPTION {
            self.abort(ctx.store, "调度器异常：内部错误（Rust 侧无 panic 逃逸）");
        }
        ctx.transport.close();
        ctx.store.close();
        rc
    }

    fn run_inner(&mut self, ctx: &mut RunCtx) -> i32 {
        if self.round_no == 0 {
            ctx.store.create_session(&json!({
                "session_id": self.session_id, "topic": self.cfg_topic,
                "question": self.question, "moderator": self.moderator,
                "members": self.members, "max_rounds": self.max_rounds,
                "round_no": 0, "state": "running",
                "goal": self.goal, "agenda": self.agenda}));
            ctx.store.add_agenda_items(&self.session_id, &self.agenda);
        }
        self.say(&format!(
            "[会议] {} 议题={} 成员={} 主持={} 议程 {} 项 / 每项上限 {} 轮 / {:.0}s / {} 字符预算",
            self.session_id,
            self.cfg_topic,
            self.members.join(","),
            self.moderator,
            self.agenda.len(),
            self.max_rounds,
            self.deadline_s,
            self.budget_chars
        ));
        self.say(&format!(
            "[目标] {}",
            if self.goal.is_empty() {
                "（未给）"
            } else {
                self.goal.as_str()
            }
        ));
        let agenda_print = self.agenda.clone();
        for item in &agenda_print {
            self.say(&format!(
                "[议程] {}. {} —— 预期产出：{}",
                item["seq"], item["title"], item["expected"]
            ));
        }
        if self.max_total_rounds > 0 {
            self.say(&format!(
                "[安全阀] 整场总轮数上限 {}（--max-rounds 仍按每个议程项计）",
                self.max_total_rounds
            ));
        }
        let agenda = self.agenda.clone();
        for item in &agenda {
            if self.total_rounds_hit() {
                self.defer_item(
                    ctx,
                    item,
                    &format!(
                        "整场已跑满 {} 轮（--max-total-rounds）",
                        self.max_total_rounds
                    ),
                );
                continue;
            }
            if let Some(rc) = self.run_item(ctx, item) {
                return rc;
            }
        }
        self.conclude_overall(ctx);
        self.flush_issues(ctx.store);
        self.summary();
        0
    }

    fn total_rounds_hit(&self) -> bool {
        self.max_total_rounds > 0 && self.round_no >= self.max_total_rounds
    }

    fn defer_item(&mut self, ctx: &mut RunCtx, item: &Value, reason: &str) {
        let seq = item["seq"].as_i64().unwrap_or(0);
        let title = item["title"].as_str().unwrap_or("").to_string();
        let expected = item["expected"].as_str().unwrap_or("").to_string();
        let note = format!("未进入讨论：{reason}");
        self.say(&format!("[warn] 议程 {seq}「{title}」{note} → 标 deferred"));
        let mut fields = Map::new();
        fields.insert("status".into(), json!("deferred"));
        fields.insert("note".into(), json!(note));
        ctx.store.update_agenda_item(&self.session_id, seq, &fields);
        self.item_results.push(json!({
            "seq": seq, "title": item["title"], "status": "deferred",
            "conclusion": "", "entry_id": Value::Null,
            "confirm": {"agree": [], "object": []}, "rounds": 0}));
        let sid = self.session_id.clone();
        let moderator = self.moderator.clone();
        self.record_issue(
            ctx.store,
            &format!("议程第 {seq} 项「{}」未进入讨论", one_line(&title, 80)),
            &format!("{note}；预期产出：{}", one_line(&expected, 200)),
            Some(&moderator),
            None,
            "issue",
            None,
            &[json!({"type": "session", "value": sid})],
        );
        if self.state == "running" {
            self.state = "timeout".into();
            self.note = if self.note.is_empty() {
                note
            } else {
                format!("{}；{}", self.note, note)
            };
        }
    }

    /// 跑完一个议程项。None = 继续；Some(码) = 整场中止。
    fn run_item(&mut self, ctx: &mut RunCtx, item: &Value) -> Option<i32> {
        let seq = item["seq"].as_i64().unwrap_or(0);
        self.say(&format!(
            "[议程 {}] 开始：{}（预期产出：{}）",
            seq, item["title"], item["expected"]
        ));
        let mut item_round = 0i64;
        #[allow(unused_assignments)]
        let mut item_state = "running".to_string();
        #[allow(unused_assignments)]
        let mut detail = String::new();
        let mut item_texts: Vec<String> = Vec::new();

        loop {
            if item_round > 0 && self.total_rounds_hit() {
                item_state = "timeout".into();
                detail = format!(
                    "整场已跑满 {} 轮（--max-total-rounds）",
                    self.max_total_rounds
                );
                self.say(&format!("[warn] 议程 {seq} 收尾：{detail}"));
                break;
            }
            let (stop, reason) = check_hard_stop(
                item_round,
                self.max_rounds,
                now_epoch_f() - self.started,
                self.deadline_s,
                self.chars_used,
                self.budget_chars,
            );
            if stop {
                item_state = "timeout".into();
                detail = format!("未自然收敛：{reason}");
                self.say(&format!("[议程 {seq}] 收尾：{detail}"));
                break;
            }

            item_round += 1;
            self.round_no += 1;
            let prior_texts = item_texts.clone();
            let (turns, new_texts) = self.run_round(ctx, Some(item));
            item_texts.extend(new_texts.iter().cloned());
            let mut fields = Map::new();
            fields.insert("round_no".into(), json!(self.round_no));
            ctx.store.update_session(&self.session_id, &fields);

            let spoke = turns.iter().filter(|t| t["status"] == json!("ok")).count();
            self.say(&format!(
                "[议程 {seq} 轮 {item_round}] 发言 {} 人 / 缺席 {} 人 / 异常 {} 人",
                spoke,
                turns
                    .iter()
                    .filter(|t| t["status"] == json!("absent"))
                    .count(),
                turns
                    .iter()
                    .filter(|t| t["status"] == json!("error"))
                    .count()
            ));

            self.drop_chronic_absentees(ctx);

            if spoke < 2 {
                self.abort(
                    ctx.store,
                    &format!("本轮实际发言人数 {spoke} < 2，会议无法继续"),
                );
                return Some(3);
            }
            if self.active.len() < 2 {
                self.abort(
                    ctx.store,
                    &format!("在场成员只剩 {} 人，会议无法继续", self.active.len()),
                );
                return Some(3);
            }

            let (converged, rule, why) =
                check_convergence(item_round, &turns, &new_texts, &prior_texts);
            self.say(&format!("[判据] {why}"));
            if converged {
                item_state = "converged".into();
                detail = format!("收敛判据 {rule}：{why}");
                break;
            }
        }

        if item_state == "timeout" && self.state == "running" {
            self.state = "timeout".into();
            if self.note.is_empty() {
                self.note = detail.clone();
            }
        } else if item_state == "converged" && self.state == "running" && self.note.is_empty() {
            self.note = detail.clone();
        }
        self.conclude_item(ctx, item, item_round, &item_state, &detail);
        None
    }

    /// 一轮：对每个在场成员点名发言。返回 (turns, 新增正文)。
    fn run_round(&mut self, ctx: &mut RunCtx, item: Option<&Value>) -> (Vec<Value>, Vec<String>) {
        let mut turns = Vec::new();
        let mut new_texts = Vec::new();
        let mut seq = 0;
        for member in self.active.clone() {
            seq += 1;
            let turn = self.run_turn(ctx, &member, seq, item);
            if turn["status"] == json!("ok") && !turn["content"].as_str().unwrap_or("").is_empty() {
                new_texts.push(turn["content"].as_str().unwrap_or("").to_string());
            }
            turns.push(turn);
        }
        (turns, new_texts)
    }

    fn run_turn(
        &mut self,
        ctx: &mut RunCtx,
        member: &str,
        seq: i64,
        item: Option<&Value>,
    ) -> Value {
        let started = now_epoch_f();
        let base = json!({
            "session_id": self.session_id, "round_no": self.round_no, "seq": seq,
            "speaker": member, "refs": [], "prompt_chars": 0,
        });
        let current_seq = item.and_then(|i| i["seq"].as_i64());

        if !ctx.transport.ping(member) {
            *self.absent_streak.entry(member.to_string()).or_insert(0) += 1;
            let streak = *self.absent_streak.get(member).unwrap_or(&0);
            self.mark_absent(
                member,
                "点名探针无应答，判定离线",
                &format!("第 {} 轮发言", self.round_no),
            );
            self.say(&format!(
                "[轮 {}] {} 离线（连续 {} 轮），跳过",
                self.round_no, member, streak
            ));
            let turn = merge_json(
                &base,
                &json!({
                    "status": "absent", "error": "点名探针无应答，判定离线",
                    "elapsed_ms": ((now_epoch_f() - started) * 1000.0) as i64,
                }),
            );
            ctx.store.add_turn(&turn);
            return turn;
        }

        let snippets = self.snippets(ctx);
        let mut retry_reason = String::new();
        let mut parsed: Option<Map<String, Value>> = None;
        let mut raw_reply = String::new();
        let mut last_error = String::new();
        let mut prompt_chars = 0usize;

        for attempt in 1..=2 {
            let (prompt, used) = render_prompt(
                member,
                &self.cfg_topic,
                &self.question,
                self.round_no,
                self.max_rounds,
                &snippets,
                self.max_chars,
                &retry_reason,
                &self.goal,
                &self.agenda,
                current_seq,
            );
            prompt_chars = prompt.chars().count();
            self.chars_used += prompt_chars as i64;
            self.log_prompt(member, &prompt);
            self.say(&format!(
                "[轮 {}] {} 第 {} 次点名：prompt {} 字符 / 片段 {} 条",
                self.round_no, member, attempt, prompt_chars, used
            ));

            let (status, reply, error) = ctx.transport.ask(member, &prompt, ASK_TIMEOUT_S);
            self.chars_used += reply.chars().count() as i64;
            if status != "ok" {
                last_error = if error.is_empty() {
                    "对方无有效应答".into()
                } else {
                    error
                };
                if status == "absent" {
                    *self.absent_streak.entry(member.to_string()).or_insert(0) += 1;
                    self.mark_absent(member, &last_error, &format!("第 {} 轮发言", self.round_no));
                    let turn = merge_json(
                        &base,
                        &json!({
                            "status": "absent", "error": last_error,
                            "prompt_chars": prompt_chars,
                            "elapsed_ms": ((now_epoch_f() - started) * 1000.0) as i64,
                        }),
                    );
                    ctx.store.add_turn(&turn);
                    return turn;
                }
                break;
            }

            self.absent_streak.insert(member.to_string(), 0);
            self.mark_present(member); // 应答了就算在场，哪怕这条回包格式不合规
            raw_reply = reply;
            let (p, err) = parse_turn(&raw_reply);
            match (p, err) {
                (Some(m), None) => {
                    parsed = Some(m);
                    break;
                }
                (Some(m), Some(e)) => {
                    // 格式对但 refs 空 → 不再降级解析，重试一次后按 ② 处理
                    parsed = Some(m);
                    last_error = e;
                    retry_reason = last_error.clone();
                }
                (None, Some(e)) => {
                    last_error = e;
                    retry_reason = last_error.clone();
                }
                _ => unreachable!(),
            }
        }

        let elapsed_ms = ((now_epoch_f() - started) * 1000.0) as i64;

        // ① 正常发言：refs 非空 → 写黑板 → 写 turn
        if let Some(m) = parsed.as_ref() {
            let refs_empty = m["refs"].as_array().map(|a| a.is_empty()).unwrap_or(true);
            if !refs_empty {
                let refs_v = m["refs"].clone();
                let (entry_id, post_err) = ctx.bb.post_entry(
                    &self.cfg_topic,
                    m["kind"].as_str().unwrap_or("opinion"),
                    m["content"].as_str().unwrap_or(""),
                    &refs_v,
                );
                let turn = merge_json(
                    &base,
                    &json!({
                        "status": if entry_id.is_some() { "ok" } else { "error" },
                        "kind": m["kind"], "stance": m["stance"], "vote": m["vote"],
                        "confidence": m["confidence"], "content": m["content"],
                        "refs": refs_v, "entry_id": json_of(entry_id),
                        "error": json_of(post_err.clone()),
                        "prompt_chars": prompt_chars, "elapsed_ms": elapsed_ms,
                    }),
                );
                ctx.store.add_turn(&turn);
                if let Some(eid) = entry_id {
                    *self.spoken.entry(member.to_string()).or_insert(0) += 1;
                    *self
                        .tally
                        .entry(m["stance"].as_str().unwrap_or("neutral").to_string())
                        .or_insert(0) += 1;
                    self.cited_entries.push(eid);
                    for r in m["refs"].as_array().unwrap_or(&vec![]) {
                        if r["type"] == json!("entry") {
                            if let Some(v) = r["value"].as_i64() {
                                self.cited_entries.push(v);
                            }
                        }
                    }
                } else {
                    self.say(&format!(
                        "[轮 {}] {} 写黑板失败：{}",
                        self.round_no,
                        member,
                        post_err.unwrap_or_default()
                    ));
                }
                return turn;
            }
            // ② 格式对但 refs 空 → 整条不入库，turn 记 error
            self.say(&format!(
                "[轮 {}] {} refs 为空，按纪律不入库",
                self.round_no, member
            ));
            let turn = merge_json(
                &base,
                &json!({
                    "status": "error", "kind": m["kind"], "stance": m["stance"],
                    "vote": m["vote"], "confidence": m["confidence"], "content": Value::Null,
                    "error": if last_error.is_empty() {
                        "refs 为空或缺失：没有证据的观点不入库".to_string()
                    } else {
                        last_error.clone()
                    },
                    "prompt_chars": prompt_chars, "elapsed_ms": elapsed_ms,
                }),
            );
            ctx.store.add_turn(&turn);
            return turn;
        }

        // ③ 对方压根没给出可用回包
        if raw_reply.is_empty() {
            let turn = merge_json(
                &base,
                &json!({
                    "status": "error",
                    "error": if last_error.is_empty() { "无有效应答".to_string() } else { last_error.clone() },
                    "prompt_chars": prompt_chars, "elapsed_ms": elapsed_ms,
                }),
            );
            ctx.store.add_turn(&turn);
            return turn;
        }

        // ④ 整体解析失败 → 降级：整段塞 content + refs 自引该 turn，照常入库
        let self_ref = json!([{"type": "turn",
            "value": format!("{}#r{}-s{}", self.session_id, self.round_no, seq)}]);
        let content = truncate(raw_reply.trim(), CONTENT_MAX);
        let (entry_id, post_err) =
            ctx.bb
                .post_entry(&self.cfg_topic, "opinion", &content, &self_ref);
        let mut reason = format!(
            "parse_degraded：{}（已整段留痕，refs 自引）",
            if last_error.is_empty() {
                "解析失败"
            } else {
                &last_error
            }
        );
        if let Some(pe) = &post_err {
            reason += &format!("；写黑板失败：{pe}");
        }
        self.say(&format!("[轮 {}] {} {}", self.round_no, member, reason));
        let turn = merge_json(
            &base,
            &json!({
                "status": if entry_id.is_some() { "ok" } else { "error" },
                "kind": "opinion", "stance": "neutral", "vote": "continue", "confidence": 0.3,
                "content": content, "refs": self_ref,
                "entry_id": json_of(entry_id),
                "error": reason, "prompt_chars": prompt_chars, "elapsed_ms": elapsed_ms,
            }),
        );
        ctx.store.add_turn(&turn);
        if entry_id.is_some() {
            *self.spoken.entry(member.to_string()).or_insert(0) += 1;
            *self.tally.entry("neutral".to_string()).or_insert(0) += 1;
            if let Some(eid) = entry_id {
                self.cited_entries.push(eid);
            }
        }
        turn
    }

    fn snippets(&mut self, ctx: &mut RunCtx) -> Vec<Value> {
        ctx.bb
            .list_entries(&self.cfg_topic, (SNIPPET_MAX * 2) as i64)
            .into_iter()
            .take(SNIPPET_MAX)
            .collect()
    }

    fn drop_chronic_absentees(&mut self, ctx: &mut RunCtx) {
        for member in self.active.clone() {
            let streak = *self.absent_streak.get(&member).unwrap_or(&0);
            if streak < ABSENT_STREAK_LIMIT {
                continue;
            }
            self.active.retain(|m| m != &member);
            let note = format!(
                "成员 {member} 连续 {streak} 轮点名无应答，已移出会议 {} 的本场成员集。",
                self.session_id
            );
            self.say(&format!("[成员] {note}"));
            let refs = json!([{"type": "session", "value": self.session_id}]);
            let (entry_id, err) = ctx.bb.post_entry(&self.cfg_topic, "system", &note, &refs);
            if let Some(e) = err {
                self.say(&format!("[warn] system 条目写失败：{e}"));
            } else if let Some(eid) = entry_id {
                self.cited_entries.push(eid);
            }
        }
    }

    fn pick_moderator(&mut self, transport: &mut dyn Transport) -> Option<String> {
        let mut chain = vec![self.moderator.clone()];
        for m in moderator_fallback() {
            if m != self.moderator {
                chain.push(m);
            }
        }
        for candidate in chain {
            if transport.ping(&candidate) {
                return Some(candidate);
            }
        }
        None
    }

    fn speakers(&self) -> Vec<String> {
        self.members
            .iter()
            .filter(|m| self.spoken.get(*m).copied().unwrap_or(0) > 0)
            .cloned()
            .collect()
    }

    // ---------------- 议程项结论 + 当场确认 ----------------

    fn conclude_item(
        &mut self,
        ctx: &mut RunCtx,
        item: &Value,
        rounds: i64,
        item_state: &str,
        detail: &str,
    ) {
        let seq = item["seq"].as_i64().unwrap_or(0);
        let moderator = self.pick_moderator(ctx.transport);
        let snippets = self.snippets(ctx);
        let mut text = String::new();
        if let Some(mod_name) = &moderator {
            let (prompt, used) = render_conclusion_prompt(
                &self.cfg_topic,
                &self.question,
                rounds,
                &self.tally_json(),
                &snippets,
                self.max_chars,
                &self.goal,
                &self.agenda,
                Some(item),
                &[],
                &[],
                "",
                "",
            );
            self.say(&format!(
                "[议程 {seq} 结论] 主持人 {mod_name}：prompt {} 字符 / 片段 {used} 条",
                prompt.chars().count()
            ));
            let reply = self.ask_moderator(
                mod_name,
                &prompt,
                &format!("议程 {seq} 结论"),
                ctx.transport,
            );
            if !reply.is_empty() {
                let (parsed, _err) = parse_turn(&reply);
                text = match parsed {
                    Some(m) => m["content"].as_str().unwrap_or("").to_string(),
                    None => truncate(reply.trim(), CONTENT_MAX),
                };
            }
        } else {
            self.say(&format!(
                "[议程 {seq} 结论] 主持人全员离线，取本项最后一条有效发言"
            ));
        }

        if text.is_empty() {
            let tail = self
                .session_texts
                .last()
                .cloned()
                .unwrap_or_else(|| "本项没有产生任何有效发言。".into());
            text = format!("（主持人缺席，取最后一条有效发言作为本项结论）\n{tail}");
        }
        if item_state == "timeout" {
            text = format!("【本项未自然收敛】{detail}\n{text}");
        }

        let body = format!(
            "【议程 {}：{}】预期产出：{}\n{}",
            seq,
            item["title"].as_str().unwrap_or(""),
            item["expected"].as_str().unwrap_or(""),
            text
        );
        let refs = dedup_i64(&self.cited_entries)
            .into_iter()
            .map(|eid| json!({"type": "entry", "value": eid}))
            .collect::<Vec<_>>();
        let refs = if refs.is_empty() {
            vec![json!({"type": "session", "value": self.session_id})]
        } else {
            refs
        };
        let (entry_id, err) = ctx.bb.post_entry(
            &self.cfg_topic,
            "decision",
            &truncate(&body, CONTENT_MAX),
            &Value::Array(refs),
        );
        if let Some(e) = &err {
            self.say(&format!("[议程 {seq} 结论] 写黑板失败：{e}"));
        } else if let Some(eid) = entry_id {
            self.cited_entries.push(eid);
        }

        // ---- 当场确认（硬）：无 object 且 agree 过半才算通过
        let target = format!("agenda:{seq}");
        let summary = self.collect_confirmations(ctx, &target, &text, Some(item));
        let (status, note) = if summary["passed"].as_bool().unwrap_or(false) {
            (
                "concluded".to_string(),
                format!(
                    "当场确认通过：agree {}/在场 {} 人（需 {}，被点名 {} 人）",
                    summary["agree"].as_array().map(|a| a.len()).unwrap_or(0),
                    summary["present"].as_i64().unwrap_or(0),
                    summary["required"].as_i64().unwrap_or(0),
                    summary["total"].as_i64().unwrap_or(0)
                ),
            )
        } else {
            let note = format!(
                "**未当场确认**：agree {} / object {} / abstain {} / absent {}（在场 {} 人，需 agree {}）",
                summary["agree"].as_array().map(|a| a.len()).unwrap_or(0),
                summary["object"].as_array().map(|a| a.len()).unwrap_or(0),
                summary["abstain"].as_array().map(|a| a.len()).unwrap_or(0),
                summary["absent"].as_array().map(|a| a.len()).unwrap_or(0),
                summary["present"].as_i64().unwrap_or(0),
                summary["required"].as_i64().unwrap_or(0)
            );
            let first_object = summary["object"]
                .as_array()
                .and_then(|a| a.first())
                .and_then(|v| v.as_str())
                .map(String::from);
            let issue_refs = if let Some(eid) = entry_id {
                vec![json!({"type": "entry", "value": eid})]
            } else {
                vec![json!({"type": "session", "value": self.session_id})]
            };
            self.record_issue(
                ctx.store,
                &format!(
                    "议程第 {} 项「{}」未当场确认",
                    seq,
                    one_line(item["title"].as_str().unwrap_or(""), 80)
                ),
                &format!("{note}；本项结论：{}", one_line(&text, 400)),
                first_object.as_deref(),
                None,
                "issue",
                None,
                &issue_refs,
            );
            ("deferred".to_string(), note)
        };
        self.say(&format!("[议程 {seq} 确认] {note}"));

        let mut fields = Map::new();
        fields.insert("conclusion".into(), json!(truncate(&text, CONTENT_MAX)));
        fields.insert("conclusion_entry".into(), json_of(entry_id));
        fields.insert("status".into(), json!(status));
        fields.insert("note".into(), json!(note));
        ctx.store.update_agenda_item(&self.session_id, seq, &fields);
        self.item_results.push(json!({
            "seq": seq, "title": item["title"], "status": status,
            "conclusion": text, "entry_id": json_of(entry_id),
            "confirm": summary, "rounds": rounds,
        }));
    }

    fn collect_confirmations(
        &mut self,
        ctx: &mut RunCtx,
        target: &str,
        conclusion: &str,
        item: Option<&Value>,
    ) -> Value {
        let speakers = self.speakers();
        let label = match item {
            Some(it) => format!("议程第 {} 项", it["seq"]),
            None => "总体结论".to_string(),
        };
        let mut rows: Vec<Value> = Vec::new();
        for member in speakers {
            let prompt = render_confirm_prompt(
                &member,
                &self.goal,
                &label,
                conclusion,
                self.max_chars,
                item,
            );
            self.chars_used += prompt.chars().count() as i64;
            self.log_prompt(&format!("{member}(确认 {target})"), &prompt);
            let (status, reply, error) = ctx.transport.ask(&member, &prompt, ASK_TIMEOUT_S);
            self.chars_used += reply.chars().count() as i64;
            let (verdict, mut note) = if status != "ok" || reply.is_empty() {
                let raw = if error.is_empty() {
                    "确认点名无应答".to_string()
                } else {
                    error
                };
                let n = truncate(&raw, 200);
                self.mark_absent(&member, &n, &format!("确认 {target}"));
                ("absent".to_string(), n)
            } else {
                self.mark_present(&member);
                let (v, n, perr) = parse_confirm(&reply);
                if let Some(pe) = perr {
                    note_combined(n, pe)
                } else {
                    (v, n)
                }
            };
            if note.is_empty() {
                note = String::new();
            }
            let row = json!({
                "session_id": self.session_id, "target": target, "member": member,
                "verdict": verdict, "note": if note.is_empty() { Value::Null } else { json!(note) },
            });
            ctx.store.add_confirmation(&row);
            rows.push(row.clone());
            self.confirm_rows.push(row);
            self.say(&format!("[确认 {target}] {member} → {verdict}"));
        }

        let summary = tally_confirmations(&rows);
        for row in &rows {
            if row["verdict"] == json!("object") {
                self.record_issue(
                    ctx.store,
                    &format!("{} 被 {} 反对", label, row["member"].as_str().unwrap_or("")),
                    &truncate(row["note"].as_str().unwrap_or("（没写理由）"), 1000),
                    row["member"].as_str(),
                    None,
                    "issue",
                    None,
                    &[json!({"type": "session", "value": self.session_id})],
                );
            }
        }
        summary
    }

    // ---------------- 总体结论 + 落实事项 ----------------

    fn conclude_overall(&mut self, ctx: &mut RunCtx) {
        let moderator = self.pick_moderator(ctx.transport);
        let snippets = self.snippets(ctx);
        let roster: Vec<String> = dedup_names(
            &self
                .members
                .iter()
                .cloned()
                .chain(std::iter::once(self.moderator.clone()))
                .collect::<Vec<_>>(),
        );
        let mut text = String::new();
        let mut actions: Vec<Map<String, Value>> = Vec::new();
        let mut issues: Vec<Map<String, Value>> = Vec::new();
        let mut action_errors: Vec<String> = Vec::new();

        if let Some(mod_name) = &moderator {
            let mut retry_reason = String::new();
            for attempt in 1..=2 {
                let (prompt, used) = render_conclusion_prompt(
                    &self.cfg_topic,
                    &self.question,
                    self.round_no,
                    &self.tally_json(),
                    &snippets,
                    self.max_chars,
                    &self.goal,
                    &self.agenda,
                    None,
                    &self.item_results,
                    &roster,
                    &now_utc_iso(),
                    &retry_reason,
                );
                self.say(&format!(
                    "[总体结论] 主持人 {mod_name} 第 {attempt} 次：prompt {} 字符 / 片段 {used} 条",
                    prompt.chars().count()
                ));
                let reply = self.ask_moderator(mod_name, &prompt, "总体结论", ctx.transport);
                if reply.is_empty() {
                    action_errors = vec!["主持人没给出结论".into()];
                    break;
                }
                let (parsed, _err) = parse_turn(&reply);
                match parsed {
                    None => {
                        if text.is_empty() {
                            text = truncate(reply.trim(), CONTENT_MAX);
                        }
                        action_errors = vec!["结论回包解析不出 JSON，拿不到 actions".into()];
                        retry_reason = action_errors[0].clone();
                        continue;
                    }
                    Some(m) => {
                        text = m["content"].as_str().unwrap_or("").to_string();
                        issues = parse_issues(&m["issues"]);
                        let now_epoch = now_epoch_f() as i64;
                        let (ok, errs) =
                            validate_actions(&m["actions"], &roster, now_epoch, &self.agenda_seqs);
                        if !ok.is_empty() && errs.is_empty() {
                            actions = ok;
                            action_errors = Vec::new();
                            break;
                        }
                        action_errors = if errs.is_empty() {
                            vec!["没给 actions".into()]
                        } else {
                            errs
                        };
                        retry_reason = action_errors.join("；").chars().take(400).collect();
                        self.say(&format!(
                            "[落实] 第 {attempt} 次 actions 不合格：{retry_reason}"
                        ));
                    }
                }
            }
        } else {
            action_errors = vec!["主持人全员离线，拿不到落实事项".into()];
            self.say("[总体结论] 主持人全员离线，取最后一条有效发言");
        }

        if text.is_empty() {
            let tail = self
                .session_texts
                .last()
                .cloned()
                .unwrap_or_else(|| "本场没有产生任何有效发言。".into());
            text = format!("（主持人缺席，取最后一条有效发言作为结论）\n{tail}");
        }
        if self.state == "timeout" {
            text = format!("【未自然收敛】{}\n{text}", self.note);
        }

        // ---- 落实事项：合格的落库；不合格 → incomplete，绝不静默通过
        if !actions.is_empty() && action_errors.is_empty() {
            for act in &actions {
                let row = json!({
                    "session_id": self.session_id,
                    "agenda_seq": act.get("agenda_seq").cloned().unwrap_or(Value::Null),
                    "what": act["what"], "owner": act["owner"],
                    "due_at": act["due_at"], "status": "open",
                });
                self.say(&format!(
                    "[落实] {} → {}（截止 {}）",
                    act["what"].as_str().unwrap_or(""),
                    act["owner"].as_str().unwrap_or(""),
                    act["due_at"]
                ));
                self.action_rows.push(row.clone());
                ctx.store.add_action(&row);
            }
        } else {
            let joined = action_errors.join("；");
            let reason = format!(
                "落实事项缺失或不合格：{}",
                if action_errors.is_empty() {
                    "没给 actions"
                } else {
                    joined.as_str()
                }
            );
            self.state = "incomplete".into();
            self.note = if self.note.is_empty() {
                reason.clone()
            } else {
                format!("{}；{}", self.note, reason)
            };
            self.say(&format!("[落实] {reason} → 会议记 incomplete"));
            self.record_issue(
                ctx.store,
                "本场没有产生有效的落实事项",
                &truncate(&reason, 1000),
                moderator.as_deref(),
                None,
                "issue",
                None,
                &[json!({"type": "session", "value": self.session_id})],
            );
        }

        // ---- 主持人主动登记的遗留问题
        for extra in &issues {
            self.record_issue(
                ctx.store,
                extra["title"].as_str().unwrap_or(""),
                extra["detail"].as_str().unwrap_or(""),
                extra["owner"].as_str(),
                extra.get("due_at").and_then(|v| v.as_i64()),
                "issue",
                None,
                &[json!({"type": "session", "value": self.session_id})],
            );
        }

        // ---- 缺席导致证据不全
        let missing: Vec<String> = self.attendance()["absent_members"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        if !missing.is_empty() {
            self.record_issue(
                ctx.store,
                &format!(
                    "成员 {} 全程未发言，本场结论的证据不完整",
                    missing.join(",")
                ),
                &format!(
                    "缺席成员：{}；需在下一场补齐其意见或确认其弃权。",
                    missing.join(",")
                ),
                moderator.as_deref(),
                None,
                "issue",
                None,
                &[json!({"type": "session", "value": self.session_id})],
            );
        }

        // ---- 四要素摘要附到结论正文
        text = self.decorate_conclusion(&text);

        let refs = dedup_i64(&self.cited_entries)
            .into_iter()
            .map(|eid| json!({"type": "entry", "value": eid}))
            .collect::<Vec<_>>();
        let refs = if refs.is_empty() {
            vec![json!({"type": "session", "value": self.session_id})]
        } else {
            refs
        };
        let (entry_id, err) = ctx.bb.post_entry(
            &self.cfg_topic,
            "decision",
            &truncate(&text, CONTENT_MAX),
            &Value::Array(refs),
        );
        if let Some(e) = &err {
            self.say(&format!("[总体结论] 写黑板失败：{e}"));
        }
        self.conclusion_entry = entry_id;

        // ---- 总体结论也要当场确认
        let summary = self.collect_confirmations(ctx, "overall", &text, None);
        let mut summary_obj = summary.as_object().cloned().unwrap_or_default();
        summary_obj.insert("attendance".into(), self.attendance());
        let summary = Value::Object(summary_obj);
        self.overall_confirm = summary.clone();
        let passed = summary["passed"].as_bool().unwrap_or(false);
        let confirmed_at = if passed { Some(now_utc_iso()) } else { None };
        if !passed {
            let note = format!(
                "总体结论**未当场确认**：agree {} / object {}（在场 {} 人，需 {}）",
                summary["agree"].as_array().map(|a| a.len()).unwrap_or(0),
                summary["object"].as_array().map(|a| a.len()).unwrap_or(0),
                summary["present"].as_i64().unwrap_or(0),
                summary["required"].as_i64().unwrap_or(0)
            );
            self.note = if self.note.is_empty() {
                note.clone()
            } else {
                format!("{}；{}", self.note, note)
            };
            self.say(&format!("[总体确认] {note}"));
            let issue_refs = if let Some(eid) = entry_id {
                vec![json!({"type": "entry", "value": eid})]
            } else {
                vec![json!({"type": "session", "value": self.session_id})]
            };
            let first_object = summary["object"]
                .as_array()
                .and_then(|a| a.first())
                .and_then(|v| v.as_str())
                .map(String::from);
            self.record_issue(
                ctx.store,
                "总体结论未当场确认",
                &note,
                first_object.as_deref(),
                None,
                "issue",
                None,
                &issue_refs,
            );
        } else {
            self.say(&format!(
                "[总体确认] 通过：agree {}/在场 {} 人（需 {}，被点名 {} 人）",
                summary["agree"].as_array().map(|a| a.len()).unwrap_or(0),
                summary["present"].as_i64().unwrap_or(0),
                summary["required"].as_i64().unwrap_or(0),
                summary["total"].as_i64().unwrap_or(0)
            ));
        }

        if self.state == "running" {
            self.state = "converged".into();
        }
        let att = summary["attendance"].clone();
        ctx.store.update_session(
            &self.session_id,
            &serde_json::from_str::<Map<String, Value>>(
                &json!({
                    "state": self.state, "round_no": self.round_no,
                    "ended_at": now_utc_iso(),
                    "conclusion_entry": json_of(entry_id),
                    "note": if self.note.is_empty() { Value::Null } else { json!(self.note) },
                    "confirmed_at": json_of(confirmed_at),
                    "confirm_summary": summary,
                    "expected_count": att["expected"],
                    "present_count": att["present"],
                    "absent_count": att["absent"],
                    "attendance_rate": att["rate"],
                })
                .to_string(),
            )
            .unwrap_or_default(),
        );
    }

    fn decorate_conclusion(&self, text: &str) -> String {
        let att = self.attendance();
        let missing: Vec<String> = att["absent_members"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let mut parts = vec![
            text.to_string(),
            String::new(),
            format!(
                "【出席】应到 {} 人，实到 {} 人，{}；缺席：{}",
                att["expected"].as_i64().unwrap_or(0),
                att["present"].as_i64().unwrap_or(0),
                self.attendance_line(),
                if missing.is_empty() {
                    "无".to_string()
                } else {
                    missing.join("、")
                }
            ),
            "【逐项结论】".to_string(),
        ];
        for res in &self.item_results {
            let mark = if res["status"] == json!("concluded") {
                "已确认"
            } else {
                "未当场确认"
            };
            parts.push(format!(
                "· 第 {} 项 {}（{}）：{}",
                res["seq"],
                res["title"].as_str().unwrap_or(""),
                mark,
                one_line(res["conclusion"].as_str().unwrap_or(""), 200)
            ));
        }
        if !self.action_rows.is_empty() {
            parts.push("【落实事项】".into());
            for row in &self.action_rows {
                parts.push(format!(
                    "· {} → {}，截止 {}",
                    row["what"].as_str().unwrap_or(""),
                    row["owner"],
                    row["due_at"]
                ));
            }
        } else {
            parts.push("【落实事项】无有效落实事项，本场记 incomplete。".into());
        }
        if !self.issue_rows.is_empty() {
            parts.push("【遗留问题】".into());
            for row in &self.issue_rows {
                parts.push(format!(
                    "· {}（跟踪人：{}）",
                    row["title"].as_str().unwrap_or(""),
                    row["owner"].as_str().unwrap_or("待定")
                ));
            }
        }
        parts.join("\n")
    }
}

/// (status, reply, error) 组合处 note+perr 的口径：note 在前，err 括号在后。
fn note_combined(note: String, perr: String) -> (String, String) {
    if note.is_empty() {
        (String::new(), perr)
    } else {
        (format!("{note}（{perr}）"), String::new())
    }
}

fn merge_json(base: &Value, extra: &Value) -> Value {
    let mut obj = base.as_object().cloned().unwrap_or_default();
    if let Some(extra_obj) = extra.as_object() {
        for (k, v) in extra_obj {
            obj.insert(k.clone(), v.clone());
        }
    }
    Value::Object(obj)
}

/// 运行上下文：三个互斥的 &mut 引用打包，避免借用冲突。
pub struct RunCtx<'t> {
    pub store: &'t mut dyn Store,
    pub bb: &'t mut dyn Blackboard,
    pub transport: &'t mut dyn Transport,
}

pub const ERR_SIGNAL: i32 = 130;
pub const ERR_EXCEPTION: i32 = 1;

// ---------------- 单测（MemoryStore + NullBlackboard + DryRunTransport 全链路） ----------------

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(members: &[&str], agenda: Vec<Value>) -> RoundtableConfig {
        RoundtableConfig {
            session_id: "rt-test".into(),
            topic: "t".into(),
            question: "q?".into(),
            members: members.iter().map(|s| s.to_string()).collect(),
            max_rounds: 2,
            max_total_rounds: 0,
            deadline_s: 900.0,
            max_chars: 6000,
            moderator: "hermes".into(),
            start_round: 0,
            prompt_log: None,
            goal: "定一个结论".into(),
            agenda,
        }
    }

    #[test]
    fn test_dry_run_full_meeting_converged() {
        let agenda = crate::roundtable_core::parse_agenda(&vec![
            "频率|给出数字".to_string(),
            "保留期|给出天数".to_string(),
        ])
        .0;
        let agenda = Value::Array(agenda);
        let agenda = agenda.as_array().cloned().unwrap();
        let mut out_lines: Vec<String> = Vec::new();
        let mut sink = |line: &str| out_lines.push(line.to_string());
        let mut store = NullStore::new();
        let mut bb = NullBlackboard::new(Vec::new());
        let mut tr = DryRunTransport::new(HashMap::new(), &[], HashMap::new(), &[], 7);
        let mut rt = Roundtable::new(cfg(&["a", "b", "c"], agenda), &mut sink);
        let rc = rt.run(&mut RunCtx {
            store: &mut store,
            bb: &mut bb,
            transport: &mut tr,
        });
        assert_eq!(rc, 0, "dry-run 全链应 rc=0");
        // 两个议程项都有结论；出席 3/3；sessions 状态 converged（actions 合法）
        assert_eq!(rt.item_results.len(), 2);
        assert_eq!(rt.attendance()["expected"], json!(3));
        assert!(rt.state == "converged" || rt.state == "timeout");
        assert!(!store.sessions.is_empty());
        assert!(!store.turns.is_empty());
        assert!(!bb.posted.is_empty());
        // 确认行 + 落实行都产生了
        assert!(!store.confirmations.is_empty());
        assert!(!store.actions.is_empty());
    }

    #[test]
    fn test_dry_run_offline_absent_defect() {
        let agenda = crate::roundtable_core::parse_agenda(&vec!["标题|产出".to_string()]).0;
        let agenda = Value::Array(agenda).as_array().cloned().unwrap();
        let mut out_lines: Vec<String> = Vec::new();
        let mut sink = |line: &str| out_lines.push(line.to_string());
        let mut store = NullStore::new();
        let mut bb = NullBlackboard::new(Vec::new());
        let mut tr = DryRunTransport::new(HashMap::new(), &["c"], HashMap::new(), &[], 7);
        let mut rt = Roundtable::new(cfg(&["a", "b", "c"], agenda), &mut sink);
        let rc = rt.run(&mut RunCtx {
            store: &mut store,
            bb: &mut bb,
            transport: &mut tr,
        });
        assert_eq!(rc, 0);
        // 缺席者自动开 defect 单（owner=c）
        let defects: Vec<&Value> = store
            .issues
            .iter()
            .filter(|r| r["kind"] == json!("defect"))
            .collect();
        assert!(!defects.is_empty(), "缺席必须开 defect 单");
        assert_eq!(defects[0]["owner"], json!("c"));
    }

    #[test]
    fn test_object_blocks_confirmation() {
        let agenda = crate::roundtable_core::parse_agenda(&vec!["标题|产出".to_string()]).0;
        let agenda = Value::Array(agenda).as_array().cloned().unwrap();
        let mut verdicts = HashMap::new();
        verdicts.insert("a".to_string(), vec!["object".to_string()]);
        verdicts.insert("b".to_string(), vec!["agree".to_string()]);
        verdicts.insert("c".to_string(), vec!["agree".to_string()]);
        let mut out_lines: Vec<String> = Vec::new();
        let mut sink = |line: &str| out_lines.push(line.to_string());
        let mut store = NullStore::new();
        let mut bb = NullBlackboard::new(Vec::new());
        let mut tr = DryRunTransport::new(HashMap::new(), &[], verdicts, &[], 7);
        let mut rt = Roundtable::new(cfg(&["a", "b", "c"], agenda), &mut sink);
        let rc = rt.run(&mut RunCtx {
            store: &mut store,
            bb: &mut bb,
            transport: &mut tr,
        });
        assert_eq!(rc, 0);
        // object 了 → 该项 deferred，且留了"被 a 反对"的遗留问题
        assert_eq!(rt.item_results[0]["status"], json!("deferred"));
        assert!(store
            .issues
            .iter()
            .any(|r| r["title"].as_str().unwrap_or("").contains("被 a 反对")));
    }

    #[test]
    fn test_unwrap_reply_shapes() {
        let (s, body, _e) = unwrap_reply("{\"ok\":true,\"reply\":\"hello\"}");
        assert_eq!((s.as_str(), body.as_str()), ("ok", "hello"));
        let (s, body, _e) = unwrap_reply("纯文本");
        assert_eq!((s.as_str(), body.as_str()), ("ok", "纯文本"));
        let (s, _body, e) = unwrap_reply("{\"ok\":false,\"error\":\"bad\",\"detail\":\"x\"}");
        assert_eq!(s, "error");
        assert!(e.contains("bad"));
    }
}
