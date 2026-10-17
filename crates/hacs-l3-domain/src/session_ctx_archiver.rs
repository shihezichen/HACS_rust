//! `session_ctx_archiver` —— 会话上下文存档器（对应 Python
//! `hub/l3_domain/session_ctx_archiver.py`）。
//!
//! 上下文涨到阈值（默认 90000 tok）时，把现场未完成任务结构化存档（JSON+MD），
//! 并写回 chat_svc 的群记忆锚 RoomMemo + 发一条群消息，重启后靠群消息/群记忆恢复现场。
//!
//! 口径（=Python，逐条对齐）：
//! * 只认**显式标记**（OPEN/DONE/DECISION 词表），不做语义猜测——猜错比漏掉更坏；
//! * last_in = 该会话最新一次 API 调用的 in=，即"现在上下文多大"；
//! * 幂等：同会话再次存档至少要多涨 `REARCHIVE_DELTA`(20000) tok；
//! * PATCH 群记忆 = 只追加（数组追加），open_tasks/decisions 都按键去重；
//! * resume 会剥掉历史上自己写的归档指针，避免层层套娃。
//!
//! 本模块是**纯逻辑**：agent.log 扫描/state.db 读取/HTTP 读写都由 bin 组合根注入
//! （Python 里混在一起，Rust 侧拆开以便单测）。

use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};

// ---------------- 常量（=Python） ----------------

pub const DEFAULT_THRESHOLD: i64 = 90000;
/// 幂等：同一会话再次存档，至少要再多涨这么多 token
pub const REARCHIVE_DELTA: i64 = 20000;
pub const MSG_SCAN_LIMIT: usize = 400;
/// chat_svc 群消息正文上限
pub const TEXT_MAX: usize = 4000;
/// 群写失败后多久不再重试
pub const SKIP_GROUP_TTL_S: i64 = 3600;
pub const DEFAULT_AGENT_ID: &str = "hermes";

pub const OPEN_MARKERS: &[&str] = &[
    "未完成",
    "未做完",
    "未做成",
    "没做成",
    "待办",
    "todo",
    "TODO",
    "ToDo",
    "待验证",
    "待确认",
    "待补",
    "待处理",
    "遗留",
    "还没",
    "尚未",
    "下一步",
    "阻塞",
    "卡在",
    "受限",
    "剩下",
    "剩余",
    "未落",
    "未验证",
    "未实测",
    "没有验证",
    "待实测",
    "pending",
    "remaining",
    "blocked",
];
pub const DONE_MARKERS: &[&str] = &[
    "已完成",
    "已做完",
    "已完成并",
    "✅",
    "已修复",
    "已落地",
    "已上线",
    "已把",
    "已改",
    "已加",
    "已建",
    "已过",
    "done",
    "DONE",
    "实测通过",
    "验证通过",
    "复现并修复",
];
pub const DECISION_MARKERS: &[&str] = &[
    "决定",
    "定稿",
    "冻结",
    "口径",
    "结论是",
    "拍板",
    "定为",
    "选定",
    "裁决",
    "采纳",
    "确认采用",
];
/// 纯状态词不构成"已完成/未完成"条目，只是协议回执
pub const STATUS_NOISE: &[&str] = &[
    "done",
    "DONE",
    "ok",
    "OK",
    "yes",
    "no",
    "完成",
    "已发送",
    "success",
];

/// 群映射：精确 id 优先，其次 `*` 结尾的前缀匹配；都不中返回 ""（= 只落本机）。
pub fn resolve_group(group_map: &HashMap<String, String>, sid: &str) -> String {
    if let Some(v) = group_map.get(sid) {
        return v.clone();
    }
    for (k, v) in group_map {
        if k.ends_with('*') && sid.starts_with(k.trim_end_matches('*')) {
            return v.clone();
        }
    }
    String::new()
}

// ---------------- agent.log 行解析（API_RE 的手写对应物） ----------------
// Python: r"INFO \[([A-Za-z0-9_\-]+)\] agent\.conversation_loop: API call #(\d+): "
//         r".*?in=(\d+) out=(\d+) total=(\d+)"
// 手写逐段匹配，语义对齐（不引 regex crate）。

#[derive(Debug, Clone, PartialEq)]
pub struct ApiCallRec {
    pub sid: String,
    pub call: u64,
    pub input: u64,
    pub output: u64,
    pub total: u64,
}

fn take_digits(s: &[u8], pos: &mut usize) -> Option<u64> {
    let start = *pos;
    while *pos < s.len() && s[*pos].is_ascii_digit() {
        *pos += 1;
    }
    if *pos == start {
        return None;
    }
    std::str::from_utf8(&s[start..*pos]).ok()?.parse().ok()
}

fn expect(s: &[u8], pos: &mut usize, pat: &str) -> Option<()> {
    let p = pat.as_bytes();
    if *pos + p.len() <= s.len() && &s[*pos..*pos + p.len()] == p {
        *pos += p.len();
        Some(())
    } else {
        None
    }
}

fn take_token(s: &[u8], pos: &mut usize) -> Option<String> {
    // [A-Za-z0-9_-]+
    let start = *pos;
    while *pos < s.len() {
        let c = s[*pos] as char;
        if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
            *pos += 1;
        } else {
            break;
        }
    }
    if *pos == start {
        return None;
    }
    Some(String::from_utf8_lossy(&s[start..*pos]).into_owned())
}

/// 解析一行 agent.log；命中 API_RE 才返回 Some。
pub fn parse_api_line(line: &str) -> Option<ApiCallRec> {
    let b = line.as_bytes();
    let mut i = 0usize;
    // `INFO [` 可以出现在行中任意位置（Python 用 search 不是 match）
    loop {
        if let Some(hit) = find(&b[i..], b"INFO [") {
            let mut p = i + hit + 6;
            if let Some(sid) = take_token(b, &mut p) {
                if expect(b, &mut p, "] agent.conversation_loop: API call #").is_some() {
                    if let Some(call) = take_digits(b, &mut p) {
                        if expect(b, &mut p, ": ").is_some() {
                            // .*?in= —— 就近找 "in="
                            if let Some(j) = find(&b[p..], b"in=") {
                                let mut q = p + j + 3;
                                if let Some(input) = take_digits(b, &mut q) {
                                    if expect(b, &mut q, " out=").is_some() {
                                        if let Some(output) = take_digits(b, &mut q) {
                                            if expect(b, &mut q, " total=").is_some() {
                                                if let Some(total) = take_digits(b, &mut q) {
                                                    return Some(ApiCallRec {
                                                        sid,
                                                        call,
                                                        input,
                                                        output,
                                                        total,
                                                    });
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            i += hit + 6;
        } else {
            return None;
        }
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    (0..=hay.len() - needle.len()).find(|&i| &hay[i..i + needle.len()] == needle)
}

/// 一个会话的上下文水位（=Python scan_conflicts 的 per-sid 归并）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CtxWatermark {
    pub last_in: u64,
    pub max_in: u64,
    pub last_ts: String,
    pub calls: u64,
}

/// 把逐行解析结果归并进水位表（`last_ts` 取行首 19 字符 = `%Y-%m-%d %H:%M:%S`）。
pub fn scan_note(out: &mut HashMap<String, CtxWatermark>, line: &str) {
    let Some(rec) = parse_api_line(line) else {
        return;
    };
    let ts = line.chars().take(19).collect::<String>();
    let e = out.entry(rec.sid).or_default();
    e.last_in = rec.input;
    e.max_in = e.max_in.max(rec.input);
    e.last_ts = ts;
    e.calls += 1;
}

// ---------------- 文本清洗（_flatten/_clean_line/_clip） ----------------

/// 把消息正文压成可扫的行列表（JSON 正文也尽力拆）。超过 160 字的行再按
/// 「句末 + 编号项」切开——只按编号切，不按 `**` 切。
pub fn flatten(text: &str) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut s = text.to_string();
    if s.trim_start().starts_with('{') {
        if let Ok(obj) = serde_json::from_str::<Value>(&s) {
            let mut parts = Vec::new();
            for k in [
                "findings", "summary", "status", "error", "note", "result", "text",
            ] {
                if let Some(Value::String(v)) = obj.get(k) {
                    if !v.trim().is_empty() {
                        parts.push(v.clone());
                    }
                }
            }
            if !parts.is_empty() {
                s = parts.join("\n");
            }
        }
    }
    let raw: Vec<&str> = s
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect();
    let mut out = Vec::new();
    for ln in raw {
        if ln.chars().count() > 160 {
            for piece in split_numbered(ln) {
                let piece = clean_line(&piece);
                if !piece.is_empty() {
                    out.push(piece);
                }
            }
        } else {
            out.push(ln.to_string());
        }
    }
    out
}

/// `re.split(r"(?<=[。；;！])\s*(?=\d+[\)\.、]\s?\S)", ln)` 的手写对应物：
/// 在「句末标点 + 空白 + 编号 + )`.、` + 空白? + 非空白」的边界切开。
fn split_numbered(ln: &str) -> Vec<String> {
    let chars: Vec<char> = ln.chars().collect();
    let mut cuts = vec![0usize];
    let mut i = 0usize;
    while i < chars.len() {
        if matches!(chars[i], '。' | '；' | ';' | '！') {
            // 向后跳空白
            let mut j = i + 1;
            while j < chars.len() && chars[j].is_whitespace() {
                j += 1;
            }
            if j < chars.len() && chars[j].is_ascii_digit() {
                let mut k = j;
                while k < chars.len() && chars[k].is_ascii_digit() {
                    k += 1;
                }
                if k < chars.len() && matches!(chars[k], ')' | '.' | '、') {
                    let mut m = k + 1;
                    if m < chars.len() && chars[m] == ' ' {
                        m += 1; // \s? 只跳一个
                    }
                    if m < chars.len() && !chars[m].is_whitespace() {
                        cuts.push(m);
                        i = m;
                        continue;
                    }
                }
            }
        }
        i += 1;
    }
    cuts.push(chars.len());
    let mut out = Vec::new();
    for w in cuts.windows(2) {
        if w[0] < w[1] {
            let piece: String = chars[w[0]..w[1]].iter().collect();
            let t = piece.trim();
            if !t.is_empty() {
                out.push(t.to_string());
            }
        }
    }
    out
}

fn is_lead_junk(c: char) -> bool {
    matches!(
        c,
        ' ' | '\t' | '\n' | '\r' | '-' | '*' | '#' | '>' | '•' | '·'
    )
}

fn is_lead_punct(c: char) -> bool {
    matches!(
        c,
        '。' | '，' | '、' | '；' | '：' | ',' | '.' | ';' | ':' | '）' | ')' | '」' | '』' | '】'
    ) || c.is_whitespace()
}

fn is_tail_junk(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r' | '*' | '#' | '-')
}

/// 去掉 markdown 记号与编号，留下可读句子；去空后返回 ""。
pub fn clean_line(ln: &str) -> String {
    let mut x = ln.trim().replace("**", "").replace('`', "");
    // _LEAD_JUNK
    loop {
        match x.chars().next() {
            Some(c) if is_lead_junk(c) => x = x[c.len_utf8()..].to_string(),
            _ => break,
        }
    }
    // _LEAD_NUM: ^\d+[\)\.、]\s*
    x = strip_lead_num(&x);
    // _LEAD_PUNCT
    loop {
        match x.chars().next() {
            Some(c) if is_lead_punct(c) => x = x[c.len_utf8()..].to_string(),
            _ => break,
        }
    }
    // _TAIL_JUNK
    loop {
        match x.chars().next_back() {
            Some(c) if is_tail_junk(c) => {
                x.truncate(x.len() - c.len_utf8());
            }
            _ => break,
        }
    }
    // 连续空白压成单个空格
    let mut out = String::with_capacity(x.len());
    let mut prev_space = false;
    for c in x.chars() {
        if c.is_whitespace() {
            if !prev_space {
                out.push(' ');
            }
            prev_space = true;
        } else {
            out.push(c);
            prev_space = false;
        }
    }
    // 只剩标点/极短的碎片没有信息量：len(re.sub(r"[\W_]+","",x)) < 4
    if alnum_len(&out) < 4 {
        return String::new();
    }
    out
}

fn strip_lead_num(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0usize;
    while i < chars.len() && chars[i].is_ascii_digit() {
        i += 1;
    }
    if i == 0 {
        return s.to_string();
    }
    if i < chars.len() && matches!(chars[i], ')' | '.' | '、') {
        let mut j = i + 1;
        while j < chars.len() && chars[j].is_whitespace() {
            j += 1;
        }
        return chars[j..].iter().collect();
    }
    s.to_string()
}

/// `re.sub(r"[\W_]+", "", x)` 的长度：只数字母/数字（Unicode 字母也算，\W 取补含 CJK）。
fn alnum_len(s: &str) -> usize {
    s.chars().filter(|c| c.is_alphanumeric()).count()
}

/// `_clip(s, n=220)`：压空白、截断补 `…`（按**字符**数，Python str 切片语义）。
pub fn clip(s: &str, n: usize) -> String {
    let flat: String = s
        .chars()
        .map(|c| if c.is_whitespace() { ' ' } else { c })
        .collect();
    // re.sub(r"\s+", " ") 语义：连续空白压成单个空格
    let mut s = String::with_capacity(flat.len());
    let mut prev_space = false;
    for c in flat.chars() {
        if c == ' ' {
            if !prev_space {
                s.push(' ');
            }
            prev_space = true;
        } else {
            s.push(c);
            prev_space = false;
        }
    }
    s = s.trim().to_string();
    if s.chars().count() > n {
        let mut out: String = s.chars().take(n).collect();
        out.push('…');
        out
    } else {
        s
    }
}

fn contains_any(hay: &str, needles: &[&str]) -> bool {
    needles.iter().any(|k| hay.contains(k))
}

/// `_key(t)`：去掉分隔符后取前 60 字符（去重键）。
pub fn dedup_key(t: &str) -> String {
    let mut out = String::new();
    for c in t.chars() {
        if !matches!(
            c,
            ' ' | '\t' | '\n' | '\r' | '，' | '。' | '；' | ',' | '.' | ';' | ':' | '：' | '、'
        ) {
            out.push(c);
        }
    }
    out.chars().take(60).collect()
}

fn in_status_noise(t: &str) -> bool {
    STATUS_NOISE.contains(&t)
}

// ---------------- 场景提取（extract_scene） ----------------

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SceneItem {
    pub text: String,
    pub src: String,
    pub blocked_by: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Scene {
    pub open_tasks: Vec<SceneItem>,
    pub done: Vec<SceneItem>,
    pub decisions: Vec<SceneItem>,
    pub last_user_brief: String,
    pub next_step: String,
}

/// 一条已读入内存的会话消息（bin 从 state.db 读出后喂进来）。
#[derive(Debug, Clone)]
pub struct Msg {
    pub role: String,
    pub tool_name: String,
    pub content: String,
}

/// `BLOCKED_BY_RE` 的手写对应物：`卡在\s*([A-Za-z0-9_\u4e00-\u9fff\-]{2,20})` 或
/// `阻塞(?:于|在)\s*(…)`，返回全部捕获（每次 findall 的两个组里取非空那个）。
fn blocked_by_all(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0usize;
    while i < chars.len() {
        let rest_is_kazai = chars[i] == '卡' && chars.get(i + 1) == Some(&'在');
        let rest_is_zuse = chars[i] == '阻' && chars.get(i + 1) == Some(&'塞');
        let (mut j, word2len) = if rest_is_kazai {
            (i + 2, 0usize)
        } else if rest_is_zuse {
            // 阻塞(于|在)
            let c = *chars.get(i + 2).unwrap_or(&'\0');
            if c == '于' || c == '在' {
                (i + 3, 0usize)
            } else {
                (0usize, usize::MAX) // 不匹配
            }
        } else {
            (0usize, usize::MAX)
        };
        if word2len == usize::MAX {
            i += 1;
            continue;
        }
        // \s*
        while j < chars.len() && chars[j].is_whitespace() {
            j += 1;
        }
        // 捕获组：[A-Za-z0-9_\u4e00-\u9fff\-]{2,20}
        let start = j;
        while j < chars.len() && (j - start) < 20 {
            let c = chars[j];
            let ok = c.is_ascii_alphanumeric()
                || c == '_'
                || c == '-'
                || ('\u{4e00}'..='\u{9fff}').contains(&c);
            if !ok {
                break;
            }
            j += 1;
        }
        let span = j - start;
        if span >= 2 {
            let v: String = chars[start..j].iter().collect();
            if !out.contains(&v) {
                out.push(v);
            }
            i = j;
            continue;
        }
        i += 1;
    }
    out
}

/// 从会话消息里机械提取：未完成 / 已完成 / 关键决定 / 下一步。
pub fn extract_scene(msgs: &[Msg]) -> Scene {
    let mut scene = Scene::default();
    let mut seen_o: HashSet<String> = HashSet::new();
    let mut seen_d: HashSet<String> = HashSet::new();
    let mut seen_dec: HashSet<String> = HashSet::new();

    let add = |bucket: &mut Vec<SceneItem>, seen: &mut HashSet<String>, text: &str, src: &str| {
        let text = clean_line(text);
        if text.is_empty() || in_status_noise(&text) {
            return;
        }
        let key = dedup_key(&text);
        if key.is_empty() || seen.contains(&key) {
            return;
        }
        seen.insert(key);
        bucket.push(SceneItem {
            text: clip(&text, 240),
            src: src.to_string(),
            blocked_by: Vec::new(),
        });
    };

    for (idx, m) in msgs.iter().enumerate() {
        if m.role == "tool" {
            continue; // 工具回显里全是别的项目的字眼，噪声大
        }
        let src = if m.tool_name.is_empty() {
            format!("#{} {}", idx, m.role)
        } else {
            format!("#{} {}/{}", idx, m.role, m.tool_name)
        };
        for ln in flatten(&m.content) {
            if ln.chars().count() < 4 {
                continue;
            }
            let has_checkbox = {
                // ^\s*[-*]?\s*\[\s\]
                let t = ln.trim_start();
                let t = t
                    .strip_prefix('-')
                    .or_else(|| t.strip_prefix('*'))
                    .unwrap_or(t);
                let t = t.trim_start();
                t.starts_with("[ ]")
            };
            if contains_any(&ln, OPEN_MARKERS) || has_checkbox {
                add(&mut scene.open_tasks, &mut seen_o, &ln, &src);
            }
            if contains_any(&ln, DONE_MARKERS) {
                add(&mut scene.done, &mut seen_d, &ln, &src);
            }
            if contains_any(&ln, DECISION_MARKERS) {
                add(&mut scene.decisions, &mut seen_dec, &ln, &src);
            }
        }
    }

    // 最近一次用户交代 = 本会话的任务书
    for m in msgs.iter().rev() {
        if m.role == "user" {
            let cleaned = clean_line(&m.content);
            let base = if cleaned.is_empty() {
                m.content.clone()
            } else {
                cleaned
            };
            scene.last_user_brief = clip(&base, 1200);
            break;
        }
    }

    // 最近一次 assistant 的收尾 = 下一步（滤掉纯状态词）
    for m in msgs.iter().rev() {
        if m.role == "assistant" && !m.content.trim().is_empty() {
            let noise: HashSet<&str> = STATUS_NOISE.iter().copied().chain(["下一步"]).collect();
            let lines: Vec<String> = flatten(&m.content)
                .iter()
                .map(|y| clean_line(y))
                .filter(|x| !x.is_empty() && !noise.contains(x.as_str()))
                .collect();
            if !lines.is_empty() {
                let tail: Vec<String> = lines.iter().rev().take(3).rev().cloned().collect();
                scene.next_step = clip(&tail.join(" / "), 600);
            }
            break;
        }
    }

    // blocked_by：从 open_tasks 文本里抽"卡在 X"
    for t in &mut scene.open_tasks {
        t.blocked_by = blocked_by_all(&t.text);
    }

    scene
}

// ---------------- 存档文档 ----------------

/// `write_archive` 的文档构造部分（落盘由 bin 做）。
pub fn build_doc(
    sid: &str,
    archived_at: &str,
    ctx: &CtxWatermark,
    meta: &Value,
    scene: &Scene,
    threshold: i64,
    group: &str,
    group_write: Value,
    compression: Value,
    archive_root: &str,
) -> Value {
    let items = |v: &[SceneItem]| -> Vec<Value> {
        v.iter()
            .map(|t| {
                json!({
                    "text": t.text,
                    "src": t.src,
                    "blocked_by": t.blocked_by,
                })
            })
            .collect()
    };
    let mut doc = json!({
        "schema": "ctx-archive/v1",
        "session_id": sid,
        "archived_at": archived_at,
        "context_tokens": ctx.last_in,
        "context_tokens_max": ctx.max_in,
        "threshold": threshold,
        "api_calls": ctx.calls,
        "last_api_ts": ctx.last_ts,
        "session_meta": meta,
        "last_user_brief": scene.last_user_brief,
        "next_step": scene.next_step,
        "open_tasks": items(&scene.open_tasks),
        "done": items(&scene.done),
        "decisions": items(&scene.decisions),
        "extract_note": "机械提取（显式标记词），未用 LLM 归纳；open_tasks 需人复核后再当口径",
        "group": if group.is_empty() { Value::Null } else { json!(group) },
        "group_write": group_write,
        "compression": compression,
    });
    // render_md 恢复步骤里要用 archive_root；Python 是直接拼路径，这里挂进文档旁路
    doc["_archive_root"] = json!(archive_root);
    doc
}

/// `render_md`：与 Python 逐段对齐（含空桶提示文案）。
pub fn render_md(doc: &Value, chat_base_default: &str, archive_root: &str) -> String {
    let sid = doc["session_id"].as_str().unwrap_or("");
    let mut l: Vec<String> = Vec::new();
    l.push(format!("# 上下文现场存档 · {sid}"));
    l.push(String::new());
    l.push(format!(
        "- 存档时间：{}",
        doc["archived_at"].as_str().unwrap_or("")
    ));
    l.push(format!(
        "- 上下文 token：**{}**（峰值 {}，阈值 {}，API 调用 {} 次，最后一次 {}）",
        doc["context_tokens"],
        doc["context_tokens_max"],
        doc["threshold"],
        doc["api_calls"],
        doc["last_api_ts"]
    ));
    let m = doc.get("session_meta").cloned().unwrap_or(Value::Null);
    let mget = |k: &str| -> String {
        match m.get(k) {
            Some(Value::Null) | None => String::new(),
            Some(Value::String(s)) => s.clone(),
            Some(v) => v.to_string(),
        }
    };
    l.push(format!(
        "- 来源：source={} · title={} · cwd={} · 模型={} · 父子={}",
        mget("source"),
        mget("title"),
        mget("cwd"),
        mget("model"),
        mget("parent_session_id")
    ));
    let group = match doc.get("group") {
        Some(Value::String(g)) => g.clone(),
        _ => String::new(),
    };
    l.push(format!(
        "- 所属群：{}",
        if group.is_empty() {
            "（未指定，本次未写群记忆）".to_string()
        } else {
            group.clone()
        }
    ));
    l.push(String::new());
    l.push("## 本会话任务书（最近一次用户交代）".into());
    l.push(String::new());
    l.push(
        doc["last_user_brief"]
            .as_str()
            .filter(|s| !s.is_empty())
            .unwrap_or("（未取到）")
            .to_string(),
    );
    l.push(String::new());

    let section_items = |key: &str| -> Vec<(String, String, Vec<String>)> {
        doc[key]
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|t| {
                        (
                            t["text"].as_str().unwrap_or("").to_string(),
                            t["src"].as_str().unwrap_or("").to_string(),
                            t["blocked_by"]
                                .as_array()
                                .map(|b| {
                                    b.iter()
                                        .filter_map(|x| x.as_str().map(|s| s.to_string()))
                                        .collect()
                                })
                                .unwrap_or_default(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
    };

    let open = section_items("open_tasks");
    l.push(format!("## 未完成任务（{}）", open.len()));
    l.push(String::new());
    if !open.is_empty() {
        for (i, (text, src, bb)) in open.iter().enumerate() {
            let bb = if bb.is_empty() {
                String::new()
            } else {
                format!("｜blocked_by={}", bb.join(","))
            };
            l.push(format!("{}. {}  `{}`{}", i + 1, text, src, bb));
        }
    } else {
        l.push("（未扫到显式未完成标记 —— 不代表真没有，见下方「下一步」）".into());
    }
    l.push(String::new());

    let done = section_items("done");
    l.push(format!("## 已完成（{}）", done.len()));
    l.push(String::new());
    for (i, (text, src, _)) in done.iter().enumerate() {
        l.push(format!("{}. {}  `{}`", i + 1, text, src));
    }
    if done.is_empty() {
        l.push("（无显式的已完成标记）".into());
    }
    l.push(String::new());

    let dec = section_items("decisions");
    l.push(format!("## 关键决定（{}）", dec.len()));
    l.push(String::new());
    for (i, (text, src, _)) in dec.iter().enumerate() {
        l.push(format!("{}. {}  `{}`", i + 1, text, src));
    }
    if dec.is_empty() {
        l.push("（无显式决定标记）".into());
    }
    l.push(String::new());

    l.push("## 下一步（最近一次 Agent 收尾）".into());
    l.push(String::new());
    l.push(
        doc["next_step"]
            .as_str()
            .filter(|s| !s.is_empty())
            .unwrap_or("（未取到）")
            .to_string(),
    );
    l.push(String::new());
    l.push("## 恢复步骤".into());
    l.push(String::new());
    l.push(format!(
        "1. 本文件即现场：`{}`",
        std::path::Path::new(archive_root)
            .join(sid)
            .join("latest.md")
            .display()
            .to_string()
    ));
    if !group.is_empty() {
        l.push(format!(
            "2. 群记忆：`GET {}/api/sessions/{}/memo`（open_tasks[].blocked_by 先扫外部阻塞）",
            chat_base_default, group
        ));
    }
    l.push("3. 压缩限制：Hermes 无外部压缩 API，见下方 compression 事实。".into());
    l.push(String::new());
    l.push("## 群写入 / 压缩事实".into());
    l.push(String::new());
    l.push("```json".into());
    l.push(
        serde_json::to_string_pretty(&json!({
            "group_write": doc.get("group_write"),
            "compression": doc.get("compression"),
        }))
        .unwrap_or_default(),
    );
    l.push("```".into());
    l.push(String::new());
    l.push(format!("> {}", doc["extract_note"].as_str().unwrap_or("")));
    l.push(String::new());
    l.join("\n")
}

// ---------------- 群写（archive_to_group 的纯逻辑部分） ----------------

/// 由群记忆旧值算出本次 PATCH 的纯逻辑：
/// 返回 (new_tasks_memo, new_dec, resume)。HTTP 由 bin 做。
pub fn build_memo_patch(
    agent_id: &str,
    doc: &Value,
    old_memo: &Map<String, Value>,
    archive_root: &str,
) -> Value {
    let old_tasks = old_memo
        .get("open_tasks")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let old_keys: HashSet<String> = old_tasks
        .iter()
        .map(|t| {
            let text = match t {
                Value::Object(o) => o.get("text").and_then(|x| x.as_str()).unwrap_or(""),
                Value::String(s) => s.as_str(),
                _ => "",
            };
            dedup_key(text)
        })
        .collect();
    let doc_tasks = doc["open_tasks"].as_array().cloned().unwrap_or_default();
    let mut new_tasks: Vec<Value> = Vec::new();
    for t in &doc_tasks {
        let text = t["text"].as_str().unwrap_or("");
        if !old_keys.contains(&dedup_key(text)) {
            new_tasks.push(json!({
                "text": text,
                "owner": agent_id,
                "blocked_by": t.get("blocked_by").cloned().unwrap_or(json!([])),
                "src": format!("ctx-archiver:{}", doc["session_id"].as_str().unwrap_or("")),
            }));
            if new_tasks.len() >= 20 {
                break;
            }
        }
    }

    let old_dec_keys: HashSet<String> = old_memo
        .get("decisions")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|t| t.as_str()).map(dedup_key).collect())
        .unwrap_or_default();
    let mut new_dec: Vec<String> = Vec::new();
    for t in doc["decisions"].as_array().cloned().unwrap_or_default() {
        let x = clip(t["text"].as_str().unwrap_or(""), 240);
        if !old_dec_keys.contains(&dedup_key(&x)) {
            new_dec.push(x);
        }
    }
    // decisions 只取最后 10 条
    if new_dec.len() > 10 {
        new_dec = new_dec.split_off(new_dec.len() - 10);
    }

    // resume：剥掉历史上自己写的归档指针，避免层层套娃
    let prev_resume_raw = old_memo
        .get("resume")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let prev_resume = strip_archive_pointer(&prev_resume_raw);
    let latest = std::path::Path::new(archive_root)
        .join(doc["session_id"].as_str().unwrap_or(""))
        .join("latest.md")
        .display()
        .to_string();
    let mut resume = format!(
        "↩ 上下文归档 {}：会话 {} 已达 {} tok。现场 {}｜未完成 {} 项见 open_tasks。",
        doc["archived_at"].as_str().unwrap_or(""),
        doc["session_id"].as_str().unwrap_or(""),
        doc["context_tokens"],
        latest,
        doc["open_tasks"].as_array().map(|a| a.len()).unwrap_or(0)
    );
    if !prev_resume.is_empty() && !resume.contains(&prev_resume) {
        resume = format!("{}［此前原话］{}", resume, prev_resume);
        let chars: Vec<char> = resume.chars().collect();
        if chars.len() > 4000 {
            resume = chars[..4000].iter().collect();
        }
    }

    json!({
        "open_tasks": new_tasks,
        "decisions": new_dec,
        "resume": resume,
    })
}

/// `re.sub(r"↩\s*上下文归档[^↩]*?未完成\s*\d+\s*项见\s*open_tasks。?\s*(［此前(?:原话)?］)?", "", s)`
/// + `re.sub(r"^(［此前(?:原话)?］\s*)+", "", s).strip()` 的手写对应物。
fn strip_archive_pointer(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out: Vec<char> = Vec::with_capacity(chars.len());
    let mut i = 0usize;
    while i < chars.len() {
        // 找 "↩"，随后 \s* 上下文归档
        if chars[i] == '↩' {
            let mut j = i + 1;
            while j < chars.len() && chars[j].is_whitespace() {
                j += 1;
            }
            let head = ['上', '下', '文', '归', '档'];
            if chars[j..].starts_with(&head) {
                let mut k = j + head.len();
                // [^↩]*? 未完成 \d+ 项见 open_tasks 。? \s* (［此前(?:原话)?］)?
                // 懒惰匹配：从 k 往后找第一个能接上 "未完成" 的位置
                let mut matched = false;
                let mut p = k;
                while p <= chars.len() {
                    let wei = ['未', '完', '成'];
                    if chars[p..].starts_with(&wei) {
                        let mut q = p + 3;
                        // \s*
                        while q < chars.len() && chars[q].is_whitespace() {
                            q += 1;
                        }
                        let digits_start = q;
                        while q < chars.len() && chars[q].is_ascii_digit() {
                            q += 1;
                        }
                        if q > digits_start {
                            // \s* 项见 \s* open_tasks 。?
                            while q < chars.len() && chars[q].is_whitespace() {
                                q += 1;
                            }
                            let xiang = ['项', '见'];
                            if chars[q..].starts_with(&xiang) {
                                let mut q = q + 2;
                                while q < chars.len() && chars[q].is_whitespace() {
                                    q += 1;
                                }
                                let ot = ['o', 'p', 'e', 'n', '_', 't', 'a', 's', 'k', 's'];
                                if chars[q..].starts_with(&ot) {
                                    let mut q = q + ot.len();
                                    if q < chars.len() && chars[q] == '。' {
                                        q += 1;
                                    }
                                    while q < chars.len() && chars[q].is_whitespace() {
                                        q += 1;
                                    }
                                    // (［此前(?:原话)?］)? —— 可选，吃了也删
                                    if chars[q..].starts_with(&['［', '此', '前']) {
                                        let mut q2 = q + 3;
                                        let yuan = ['原', '话'];
                                        if chars[q2..].starts_with(&yuan) {
                                            q2 += 2;
                                        }
                                        if chars.get(q2) == Some(&'］') {
                                            q = q2 + 1;
                                        }
                                    }
                                    // 整段删掉：out 不追加 chars[i..q]
                                    k = q;
                                    matched = true;
                                    break;
                                }
                            }
                        }
                    }
                    if p >= chars.len() || chars[p] == '↩' {
                        break;
                    }
                    p += 1;
                }
                if matched {
                    i = k;
                    continue;
                }
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    // ^(［此前(?:原话)?］\s*)+
    let s2: String = out.into_iter().collect();
    let mut rest: &str = s2.as_str();
    loop {
        let t = rest.trim_start();
        if t.starts_with('［') {
            let after = if let Some(r) = t.strip_prefix("［此前原话］") {
                r
            } else if let Some(r) = t.strip_prefix("［此前］") {
                r
            } else {
                break;
            };
            rest = after;
        } else {
            break;
        }
    }
    rest.trim().to_string()
}

/// 群消息正文（@all，<= TEXT_MAX）。
pub fn build_group_text(doc: &Value, archive_root: &str) -> String {
    let sid = doc["session_id"].as_str().unwrap_or("");
    let latest = std::path::Path::new(archive_root)
        .join(sid)
        .join("latest.md")
        .display()
        .to_string();
    let mut lines: Vec<String> = Vec::new();
    lines.push(format!(
        "【上下文归档 · 自动】会话 {} 上下文已达 {} tok（阈值 {}），现场已存档。",
        sid, doc["context_tokens"], doc["threshold"]
    ));
    lines.push(format!("存档：{latest}"));
    let open: Vec<(String, Vec<String>)> = doc["open_tasks"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|t| {
                    (
                        t["text"].as_str().unwrap_or("").to_string(),
                        t["blocked_by"]
                            .as_array()
                            .map(|b| {
                                b.iter()
                                    .filter_map(|x| x.as_str().map(|s| s.to_string()))
                                    .collect()
                            })
                            .unwrap_or_default(),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    if !open.is_empty() {
        lines.push(format!(
            "未完成 {} 项（已写入本群记忆 open_tasks）：",
            open.len()
        ));
        for (i, (text, bb)) in open.iter().take(8).enumerate() {
            let bb = if bb.is_empty() {
                String::new()
            } else {
                format!("〔卡在：{}〕", bb.join(","))
            };
            lines.push(format!("{}. {}{}", i + 1, text, bb));
        }
    } else {
        lines.push("未扫到显式未完成标记 —— 别当没有，见存档「下一步」。".into());
    }
    if let Some(ns) = doc["next_step"].as_str().filter(|s| !s.is_empty()) {
        lines.push(format!("下一步：{ns}"));
    }
    lines.push(format!(
        "恢复：GET /api/sessions/{}/memo（先扫 open_tasks[].blocked_by）。",
        sid
    ));
    let text = lines.join("\n");
    let chars: Vec<char> = text.chars().collect();
    if chars.len() > TEXT_MAX {
        chars[..TEXT_MAX].iter().collect()
    } else {
        text
    }
}

// ---------------- 幂等判定（handle_one 的纯逻辑部分） ----------------

/// 返回 Err(原因 JSON) = 跳过；Ok(()) = 继续存档。
pub fn check_idempotent(
    sid: &str,
    last_in: u64,
    threshold: i64,
    force: bool,
    state: &Map<String, Value>,
) -> Result<(), Value> {
    if (last_in as i64) < threshold && !force {
        return Err(json!({
            "skipped": "below_threshold",
            "session_id": sid,
            "last_in": last_in,
        }));
    }
    if !force {
        if let Some(prev) = state.get(sid) {
            let prev_in = prev.get("last_in").and_then(|v| v.as_u64()).unwrap_or(0);
            if (last_in as i64) - (prev_in as i64) < REARCHIVE_DELTA {
                return Err(json!({
                    "skipped": "no_delta",
                    "session_id": sid,
                    "last_in": last_in,
                }));
            }
        }
    }
    Ok(())
}

/// 扫描结果里的超阈值会话，按 last_in 降序（Python: sorted(key=lambda x: -x[1]["last_in"])）。
pub fn threshold_hits(
    all: &HashMap<String, CtxWatermark>,
    threshold: i64,
) -> Vec<(String, CtxWatermark)> {
    let mut hits: Vec<(String, CtxWatermark)> = all
        .iter()
        .filter(|(_, c)| (c.last_in as i64) >= threshold)
        .map(|(s, c)| (s.clone(), c.clone()))
        .collect();
    hits.sort_by(|a, b| b.1.last_in.cmp(&a.1.last_in).then_with(|| a.0.cmp(&b.0)));
    hits
}

// ---------------- 压缩事实（compression_facts） ----------------

/// 解析 config.yaml 的 compression: 段（缩进敏感，手写小解析器）。
pub fn parse_compression_config(txt: &str) -> Map<String, Value> {
    let mut comp = Map::new();
    let mut in_block = false;
    for line in txt.lines() {
        if line.starts_with("compression:") {
            in_block = true;
            continue;
        }
        if in_block {
            if line.is_empty() {
                continue;
            }
            if !line.starts_with(' ') && !line.starts_with('\t') {
                break; // 出块
            }
            let t = line.trim();
            if let Some((k, v)) = t.split_once(':') {
                comp.insert(k.trim().to_string(), json!(v.trim()));
            }
        }
    }
    comp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_api_line() {
        let line = "2026-10-17 10:00:00,123 INFO [20261016_145048_d119ef1e] agent.conversation_loop: API call #42: model=gpt in=120500 out=800 total=121300";
        let r = parse_api_line(line).unwrap();
        assert_eq!(r.sid, "20261016_145048_d119ef1e");
        assert_eq!(r.call, 42);
        assert_eq!(r.input, 120500);
        assert_eq!(r.output, 800);
        assert_eq!(r.total, 121300);
        assert!(parse_api_line("INFO [abc] agent.other: in=1").is_none());
        assert!(parse_api_line("普通日志行").is_none());
    }

    #[test]
    fn test_scan_note_merge() {
        let mut m = HashMap::new();
        scan_note(
            &mut m,
            "2026-10-17 10:00:00,1 INFO [s1] agent.conversation_loop: API call #1: in=100 out=1 total=101",
        );
        scan_note(
            &mut m,
            "2026-10-17 10:01:00,2 INFO [s1] agent.conversation_loop: API call #2: in=90 out=1 total=91",
        );
        let w = &m["s1"];
        assert_eq!(w.last_in, 90);
        assert_eq!(w.max_in, 100);
        assert_eq!(w.calls, 2);
        assert_eq!(&w.last_ts, "2026-10-17 10:01:00");
    }

    #[test]
    fn test_resolve_group() {
        let mut gm = HashMap::new();
        gm.insert("20260922_121121_21253d".into(), "meet_a".into());
        gm.insert("20261016_*".into(), "meet_b".into());
        assert_eq!(resolve_group(&gm, "20260922_121121_21253d"), "meet_a");
        assert_eq!(resolve_group(&gm, "20261016_145048_x"), "meet_b");
        assert_eq!(resolve_group(&gm, "20270101_z"), "");
    }

    #[test]
    fn test_clean_line_and_clip() {
        assert_eq!(clean_line("**1) 未完成**：编译报错"), "未完成：编译报错");
        assert_eq!(clean_line("- 待办：写测试。"), "待办：写测试。");
        assert_eq!(clean_line("。、；"), "");
        assert_eq!(clean_line("ab"), "");
        assert_eq!(clip("0123456789", 4), "0123…");
        assert_eq!(clip("  a  b  ", 10), "a b");
    }

    #[test]
    fn test_flatten_json_and_numbered() {
        let v = flatten(r#"{"findings": "A未完成", "summary": "B已落地"}"#);
        assert_eq!(v, vec!["A未完成", "B已落地"]);
        let mut long = String::new();
        for _ in 0..30 {
            long.push_str("背景铺垫内容若干文字凑长度。");
        }
        long.push_str("1) 未完成 a 部分；2) 已完成 b 部分！3) 决定走方案 X");
        assert!(long.chars().count() > 160);
        let v = flatten(&long);
        assert!(v.len() >= 3, "{v:?}");
        assert!(v.iter().any(|x| x.starts_with("未完成")), "{v:?}");
    }

    #[test]
    fn test_extract_scene() {
        let msgs = vec![
            Msg { role: "user".into(), tool_name: "".into(), content: "把导入器写完".into() },
            Msg {
                role: "assistant".into(),
                tool_name: "".into(),
                content: "1) 已完成：解析器落地。2) 未完成：写导入器，卡在 编码问题。\n3) 决定：用 utf-8 硬解。".into(),
            },
            Msg { role: "tool".into(), tool_name: "exec".into(), content: "已完成（工具回显，应忽略）".into() },
        ];
        let s = extract_scene(&msgs);
        assert_eq!(s.open_tasks.len(), 1);
        assert_eq!(s.open_tasks[0].blocked_by, vec!["编码问题"]);
        assert_eq!(s.done.len(), 1);
        assert_eq!(s.decisions.len(), 1);
        assert_eq!(s.last_user_brief, "把导入器写完");
        assert!(s.next_step.contains("决定"));
    }

    #[test]
    fn test_extract_scene_noise_and_dedup() {
        let msgs = vec![Msg {
            role: "assistant".into(),
            tool_name: "".into(),
            content: "done\n未完成 a\n未完成 a".into(),
        }];
        let s = extract_scene(&msgs);
        assert!(s.done.is_empty()); // 纯状态词
        assert_eq!(s.open_tasks.len(), 1); // 去重
    }

    #[test]
    fn test_check_idempotent() {
        let mut state = Map::new();
        state.insert("s1".into(), json!({"last_in": 100000}));
        // 未达阈值
        let e = check_idempotent("s2", 80000, 90000, false, &state).unwrap_err();
        assert_eq!(e["skipped"], "below_threshold");
        // 无显著增长
        let e = check_idempotent("s1", 110000, 90000, false, &state).unwrap_err();
        assert_eq!(e["skipped"], "no_delta");
        // delta 够
        assert!(check_idempotent("s1", 120001, 90000, false, &state).is_ok());
        // force 豁免一切
        assert!(check_idempotent("s2", 10, 90000, true, &state).is_ok());
    }

    #[test]
    fn test_threshold_hits_order() {
        let mut m = HashMap::new();
        m.insert(
            "a".into(),
            CtxWatermark {
                last_in: 100,
                max_in: 0,
                last_ts: "".into(),
                calls: 1,
            },
        );
        m.insert(
            "b".into(),
            CtxWatermark {
                last_in: 300,
                max_in: 0,
                last_ts: "".into(),
                calls: 1,
            },
        );
        m.insert(
            "c".into(),
            CtxWatermark {
                last_in: 200,
                max_in: 0,
                last_ts: "".into(),
                calls: 1,
            },
        );
        let hits = threshold_hits(&m, 100);
        assert_eq!(hits.len(), 3);
        assert_eq!(hits[0].0, "b");
        assert_eq!(hits[1].0, "c");
        assert_eq!(hits[2].0, "a");
    }

    #[test]
    fn test_build_memo_patch_dedup_and_resume() {
        let doc = json!({
            "session_id": "s1",
            "archived_at": "2026-10-17T11:00:00+08:00",
            "context_tokens": 91000,
            "open_tasks": [
                {"text": "未完成 a", "blocked_by": ["x"]},
                {"text": "未完成 b", "blocked_by": []}
            ],
            "decisions": [{"text": "决定 1"}]
        });
        let mut old = Map::new();
        old.insert("open_tasks".into(), json!([{"text": "未完成 a"}]));
        old.insert(
            "resume".into(),
            json!("↩ 上下文归档 2026-10-16：会话 s1 已达 90000 tok。现场 /a/s1/latest.md｜未完成 1 项见 open_tasks。［此前原话］真正的话"),
        );
        let p = build_memo_patch("hermes", &doc, &old, "/arc");
        let tasks = p["open_tasks"].as_array().unwrap();
        assert_eq!(tasks.len(), 1); // 未完成 a 已在旧 memo，只追加 b
        assert_eq!(tasks[0]["text"], "未完成 b");
        assert_eq!(tasks[0]["owner"], "hermes");
        assert_eq!(tasks[0]["src"], "ctx-archiver:s1");
        let resume = p["resume"].as_str().unwrap();
        assert!(resume.starts_with("↩ 上下文归档 2026-10-17"));
        assert!(resume.ends_with("［此前原话］真正的话")); // 旧指针被剥掉，原话保留
        assert_eq!(p["decisions"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn test_build_group_text() {
        let doc = json!({
            "session_id": "s1",
            "context_tokens": 91000,
            "threshold": 90000,
            "open_tasks": [{"text": "未完成 a", "blocked_by": ["x"]}],
            "next_step": "下一步：写导入器"
        });
        let t = build_group_text(&doc, "/arc");
        assert!(t.starts_with("【上下文归档 · 自动】会话 s1"));
        assert!(t.contains("1. 未完成 a〔卡在：x〕"));
        assert!(t.contains("下一步：下一步：写导入器"));
        assert!(t.contains("恢复：GET /api/sessions/s1/memo"));
    }

    #[test]
    fn test_render_md() {
        let doc = json!({
            "schema": "ctx-archive/v1",
            "session_id": "s1",
            "archived_at": "2026-10-17T11:00:00+08:00",
            "context_tokens": 91000,
            "context_tokens_max": 95000,
            "threshold": 90000,
            "api_calls": 42,
            "last_api_ts": "2026-10-17 10:00:00",
            "session_meta": {"source": "feishu", "title": "t", "cwd": "/w", "model": "m", "parent_session_id": null},
            "last_user_brief": "任务书",
            "next_step": "下一步",
            "open_tasks": [{"text": "未完成 a", "src": "#1 assistant", "blocked_by": ["x"]}],
            "done": [],
            "decisions": [],
            "extract_note": "机械提取",
            "group": "meet_a",
            "group_write": null,
            "compression": {}
        });
        let md = render_md(&doc, "http://127.0.0.1:9980", "/arc");
        assert!(md.contains("# 上下文现场存档 · s1"));
        assert!(md.contains("## 未完成任务（1）"));
        assert!(md.contains("1. 未完成 a  `#1 assistant`｜blocked_by=x"));
        assert!(md.contains("## 已完成（0）"));
        assert!(md.contains("（无显式的已完成标记）"));
        assert!(md.contains("（无显式决定标记）"));
        assert!(md.contains("GET http://127.0.0.1:9980/api/sessions/meet_a/memo"));
    }

    #[test]
    fn test_strip_archive_pointer_variants() {
        assert_eq!(
            strip_archive_pointer(
                "↩ 上下文归档 a：会话 s 已达 1 tok。现场 p｜未完成 2 项见 open_tasks。新的话"
            ),
            "新的话"
        );
        assert_eq!(strip_archive_pointer("［此前］更早"), "更早");
        assert_eq!(
            strip_archive_pointer("［此前原话］ ［此前］保留我"),
            "保留我"
        );
        assert_eq!(strip_archive_pointer("没指针的一句"), "没指针的一句");
    }

    #[test]
    fn test_blocked_by_all() {
        assert_eq!(blocked_by_all("卡在 编码问题"), vec!["编码问题"]);
        assert_eq!(blocked_by_all("阻塞在 网络上"), vec!["网络上"]);
        assert_eq!(blocked_by_all("阻塞于 API"), vec!["API"]);
        let v = blocked_by_all("卡在 编码，阻塞于 网络");
        assert_eq!(v, vec!["编码", "网络"]);
        // 1 字捕获不够 {2,20}，整处不命中
        assert!(blocked_by_all("卡在 a，阻塞于 b").is_empty());
        assert!(blocked_by_all("卡在 x").is_empty()); // 1 字不够 {2,20}
    }

    #[test]
    fn test_parse_compression_config() {
        let txt = "other: 1\ncompression:\n  proactive_prune_tokens: 120000\n  mode: auto\nfoo: 2";
        let c = parse_compression_config(txt);
        assert_eq!(c["proactive_prune_tokens"], "120000");
        assert_eq!(c["mode"], "auto");
    }

    #[test]
    fn test_dedup_key() {
        assert_eq!(dedup_key("未完成， a。b"), dedup_key("未完成a b"));
        assert_eq!(dedup_key(&"x".repeat(100)).chars().count(), 60);
    }
}
