//! #478：人发言的意图分类（新任务 vs 进度追问）—— 纯确定性规则，逐字对齐 Python。

pub const TODO_INTENTS: [&str; 2] = ["task", "followup"];

/// 任务引导词（任务书抬头）。
pub const TODO_TASK_INTRODUCERS: [&str; 15] = [
    "如下任务", "以下任务", "任务如下", "任务：", "任务:",
    "需求：", "需求:", "要求：", "要求:", "待办：", "待办:",
    "你负责", "负责如下", "安排如下", "分配如下",
];

pub const TODO_FOLLOWUP_PHRASES: [&str; 61] = [
    "完成了吗", "完成了没", "完成了没有", "做完了吗", "做完了没", "改完了吗", "改完了没",
    "好了吗", "好了没", "好了没有", "搞定了吗", "搞定了没", "写好了吗", "写完了吗",
    "弄好了吗", "弄好了没", "上线了吗", "上线了没", "修好了吗", "修好了没", "完了吗",
    "有没有完成", "是否完成", "有没有做好", "有没有做", "有没有改",
    "进度", "进展", "到哪一步", "到哪了", "到哪儿了", "做到哪", "进行到",
    "怎么样了", "怎么样啦", "多久能", "还要多久", "要多久", "什么时候能", "什么时候好",
    "啥时候能", "何时能", "催一下", "催你", "没动静", "有结果了吗", "有结果没",
    "通报", "汇报", "报进度", "报一下", "同步一下", "同步下", "反馈一下",
    "在做了吗", "开始了吗", "开始没", "开工了吗", "动工了吗", "可以了吗", "干完了吗",
];

pub const TODO_FOLLOWUP_STRONG: [&str; 13] = [
    "最后一次机会", "最后机会", "最后通牒", "把你删", "把你踢", "把你移出", "不需要你",
    "糊弄", "别糊弄", "在糊弄", "我等你的", "等你通报", "警告你",
];

pub const TODO_TASK_ACTION_WORDS: [&str; 85] = [
    "改", "修改", "修", "修复", "加", "增加", "新增", "补", "补齐", "补充",
    "做", "实现", "开发", "写", "编写", "重构", "优化", "完善", "调整", "替换",
    "删除", "去掉", "迁移", "接入", "支持", "升级", "部署", "上线", "发布",
    "创建", "搭", "生成", "导出", "导入", "排查", "定位", "检查", "核对",
    "测试", "验证", "复现", "整理", "梳理", "盘点", "清理", "归档",
    "重启", "启动", "停止", "停掉", "关掉", "删掉", "清掉", "收尾", "收口",
    "回滚", "回退", "改回", "补上", "接上", "跑通", "执行", "安装", "装上",
    "落盘", "出图", "出件", "打包", "对齐", "拉取", "推送", "提交",
    "重跑", "复跑", "复查", "跑一遍", "跑一下",
    "add", "fix", "implement", "refactor", "deploy", "create", "build", "update",
];

pub const TODO_TASK_DIRECTIVE_MARKERS: [&str; 14] = [
    "请", "帮我", "帮忙", "麻烦", "给我", "把", "你去", "需要你", "要你",
    "安排", "记得", "别忘", "顺手", "新任务",
];

pub const TODO_ACTION_NEGATIONS: [&str; 6] = ["不", "没", "未", "别", "无", "非"];
pub const TODO_ACTION_NEG_WINDOW: usize = 3;

const TODO_TASK_NAME_PREFIXES: [&str; 11] = [
    "如下任务", "以下任务", "任务如下", "任务", "需求", "要求", "待办",
    "你负责", "负责如下", "安排如下", "分配如下",
];

/// 入队时剥掉正文开头的「点名」（`@<id>`/`@<id>：` 这类；id 允许汉字后缀）。
/// 对齐 Python `TODO_TASK_MENTION_STRIP_RE = ^\s*(?:@[...]+[\s:：,，]*)+`。
pub fn strip_leading_mentions(raw: &str) -> String {
    let s = raw.trim_start();
    let bytes = s.as_bytes();
    let mut i = 0usize;
    let mut changed = true;
    while changed {
        changed = false;
        if i < bytes.len() && bytes[i] == b'@' {
            i += 1;
            // 吃到空白/标点分隔符之前
            while i < bytes.len() {
                let ch = s[i..].chars().next().unwrap();
                if matches!(ch, ' ' | '\t' | ':' | '：' | ',' | '，') {
                    break;
                }
                i += ch.len_utf8();
            }
            // 吃分隔符（含多个）
            while i < bytes.len() {
                let ch = s[i..].chars().next().unwrap();
                if matches!(ch, ' ' | '\t' | ':' | '：' | ',' | '，') {
                    i += ch.len_utf8();
                } else {
                    break;
                }
            }
            changed = true;
        }
    }
    s[i..].trim_start().to_string()
}

/// 压平所有空白为单个空格（Python `re.sub(r"\s+", " ")` + strip）。
pub fn flatten_ws(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut in_ws = false;
    for ch in raw.chars() {
        if ch.is_whitespace() {
            in_ws = true;
        } else {
            if in_ws && !out.is_empty() {
                out.push(' ');
            }
            in_ws = false;
            out.push(ch);
        }
    }
    out
}

/// `todo_dedupe_key`：剥 `#seq` 前缀、去掉**所有**空白、取前 120 字。
pub fn todo_dedupe_key(text: &str) -> String {
    let raw = strip_seq_prefix(text);
    let no_ws: String = raw.chars().filter(|c| !c.is_whitespace()).collect();
    take_chars(&no_ws, super::TODO_DEDUPE_PREFIX)
}

fn strip_seq_prefix(text: &str) -> String {
    // Python `re.sub(r"^#\d+\s+", "", ...)` —— 单层（sub 非 subn）
    let s = text.trim_start();
    if !s.starts_with('#') {
        return s.to_string();
    }
    let rest = &s[1..];
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return s.to_string();
    }
    let after = &rest[digits.len()..];
    // `\s+` 至少一个空白才剥
    let ws: usize = after.chars().take_while(|c| c.is_whitespace()).count();
    if ws == 0 {
        return s.to_string();
    }
    after[after
        .char_indices()
        .take(ws)
        .last()
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0)..]
        .to_string()
}

/// `todo_message_key`：先剥开头 @点名 再算指纹。
pub fn todo_message_key(text: &str) -> String {
    let body = flatten_ws(text);
    let body = strip_leading_mentions(&body).trim().to_string();
    todo_dedupe_key(&body)
}

fn take_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// `todo_task_name`：正文 → 抽象短名（#534）。抽不出 → ""。
pub fn todo_task_name(text: &str) -> String {
    todo_task_name_limit(text, super::TODO_TASK_NAME_MAX)
}

pub fn todo_task_name_limit(text: &str, limit: usize) -> String {
    let mut body = flatten_ws(text);
    body = strip_leading_mentions(&body).trim().to_string();
    body = strip_seq_prefix_multi(&body);
    // 循环剥抬头/序号到底
    let mut changed = true;
    while changed && !body.is_empty() {
        changed = false;
        for pre in TODO_TASK_NAME_PREFIXES.iter() {
            if let Some(rest) = body.strip_prefix(pre) {
                body = rest.trim_start_matches([' ', ':', '：', ',', '，', '、', '\t'])
                    .trim()
                    .to_string();
                changed = true;
                break;
            }
        }
        if let Some(rest) = strip_serial(&body) {
            body = rest;
            changed = true;
        }
    }
    // 取第一个硬停顿之前
    let stop_at = body
        .char_indices()
        .find(|&(_, c)| matches!(c, '。' | '！' | '？' | '；' | ';' | '!' | '?' | '\n' | '\r'))
        .map(|(i, _)| i)
        .unwrap_or(body.len());
    body = body[..stop_at].trim().to_string();
    body = body.trim_matches(['，', ',', '、', ':', '：', ';', '；', ' ']).to_string();
    if body.is_empty() {
        return String::new();
    }
    let n = body.chars().count();
    if n > limit {
        format!("{}…", take_chars(&body, limit))
    } else {
        body
    }
}

fn strip_seq_prefix_multi(s: &str) -> String {
    // Python `^(?:#\d+\s*)+` —— 多层
    let mut s = s.trim_start();
    loop {
        if !s.starts_with('#') {
            return s.to_string();
        }
        let rest = &s[1..];
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if digits.is_empty() {
            return s.to_string();
        }
        s = rest[digits.len()..].trim_start();
    }
}

/// `^[\(\[（【]?\d+[\.、\)）】]\s*` 序号剥除。
fn strip_serial(s: &str) -> Option<String> {
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0usize;
    if i < chars.len() && matches!(chars[i], '(' | '[' | '（' | '【') {
        i += 1;
    }
    let dstart = i;
    while i < chars.len() && chars[i].is_ascii_digit() {
        i += 1;
    }
    if i == dstart {
        return None;
    }
    if i < chars.len() && matches!(chars[i], '.' | '、' | ')' | '）' | '】') {
        i += 1;
    } else {
        return None;
    }
    while i < chars.len() && chars[i] == ' ' {
        i += 1;
    }
    Some(chars[i..].iter().collect())
}

/// `todo_text_from_message`：消息 → 队列条目正文 `#<seq> <摘要>`。
pub fn todo_text_from_message(text: &str, seq: Option<i64>, limit: usize) -> String {
    let raw = flatten_ws(text);
    let raw = strip_leading_mentions(&raw).trim().to_string();
    let head = seq.map(|s| format!("#{} ", s)).unwrap_or_default();
    let full = format!("{}{}", head, raw);
    let cap = limit.min(super::TODO_TEXT_MAX);
    take_chars(&full, cap)
}

/// `todo_action_hits`：真·动作指令动词（被否定的命中不算）。
pub fn todo_action_hits(body: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let low = body.to_lowercase();
    for word in TODO_TASK_ACTION_WORDS.iter() {
        let mut idx = match low.find(word) {
            Some(i) => i,
            None => continue,
        };
        loop {
            let start = idx.saturating_sub(TODO_ACTION_NEG_WINDOW);
            // 字节窗口可能切开多字节字符——用 char 边界修正
            let start = floor_char_boundary(body, start);
            let near = &body[start..idx];
            let negated = TODO_ACTION_NEGATIONS.iter().any(|neg| near.contains(neg));
            if !negated {
                if !out.iter().any(|w| w == word) {
                    out.push(word.to_string());
                }
                break;
            }
            match low[idx + word.len()..].find(word) {
                Some(off) => idx = idx + word.len() + off,
                None => break,
            }
        }
    }
    out
}

fn floor_char_boundary(s: &str, mut i: usize) -> usize {
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn intent_out(intent: &str, reason: &str, hits: Vec<String>) -> serde_json::Value {
    serde_json::json!({
        "intent": intent,
        "reason": reason,
        "hits": hits.into_iter().take(12).collect::<Vec<_>>(),
    })
}

/// `classify_human_intent`：一句话 → intent/reason/hits。判定顺序即口径：
/// 抬头冒号空体 > 任务书抬头 > 强催问 > 动作指令 > 追问短语 > 疑问句 > 把字兜底 > 默认不入队。
pub fn classify_human_intent(text: &str) -> serde_json::Value {
    let raw = flatten_ws(text);
    let body = strip_leading_mentions(&raw).trim().to_string();
    if body.is_empty() {
        return intent_out("followup", "empty_text", vec![]);
    }
    let low = body.to_lowercase();
    let mut hits: Vec<String> = Vec::new();
    for phrase in TODO_FOLLOWUP_PHRASES.iter() {
        if low.contains(phrase) && !hits.iter().any(|h| h == phrase) {
            hits.push(phrase.to_string());
        }
    }
    let actions = todo_action_hits(&body);
    let directives: Vec<String> = TODO_TASK_DIRECTIVE_MARKERS
        .iter()
        .filter(|w| body.contains(**w))
        .map(|w| w.to_string())
        .collect();
    // 抬头词紧跟「的」= 定语，不是派活抬头
    let introducers: Vec<String> = TODO_TASK_INTRODUCERS
        .iter()
        .filter(|w| {
            if let Some(pos) = body.find(**w) {
                let after = &body[pos + w.len()..];
                !after.starts_with('的')
            } else {
                false
            }
        })
        .map(|w| w.to_string())
        .collect();
    let is_q = body.contains(['?', '？'])
        || TODO_QUESTION_TAIL_WORDS.iter().any(|w| body.ends_with(w))
        || body.contains("有没有")
        || body.contains("是不是")
        || body.contains("能不能")
        || body.contains("行不行")
        || body.contains("可不可以")
        || body.contains("会不会")
        || body.contains("算不算")
        || body.contains("多久")
        || body.contains("什么时候")
        || body.contains("啥时候")
        || body.contains("何时")
        || body.contains("哪一步")
        || body.contains("到哪")
        || body.contains("怎么样")
        || body.contains("如何");
    // 抬头收冒号 = 只发了抬头没发正文 → 不入队
    if body.ends_with(':') || body.ends_with('：') {
        let mut h = introducers.clone();
        h.extend(directives.clone());
        return intent_out("followup", "task_preamble_no_payload", h);
    }
    if !introducers.is_empty() && !(is_q && directives.is_empty()) {
        let mut h = introducers.clone();
        h.extend(directives.clone());
        return intent_out("task", "task_introducer", h);
    }
    let strong: Vec<String> = TODO_FOLLOWUP_STRONG
        .iter()
        .filter(|w| body.contains(**w))
        .map(|w| w.to_string())
        .collect();
    if !strong.is_empty() {
        let mut h = strong;
        h.extend(hits);
        return intent_out("followup", "followup_strong", h);
    }
    if !actions.is_empty() && (!is_q || !directives.is_empty()) {
        let mut h = actions.clone();
        for d in &directives {
            if !h.contains(d) {
                h.push(d.clone());
            }
        }
        return intent_out("task", "action_directive", h);
    }
    if !hits.is_empty() {
        return intent_out("followup", "followup_phrase", hits);
    }
    if is_q {
        let h = if directives.is_empty() { actions } else { directives };
        return intent_out("followup", "question_no_directive", h);
    }
    // 处置式祈使句兜底：非否定语境的「把 …」
    if directives.iter().any(|d| d == "把") {
        if let Some(at) = body.find('把') {
            let start = floor_char_boundary(&body, at.saturating_sub(TODO_ACTION_NEG_WINDOW));
            let near = &body[start..at];
            if !TODO_ACTION_NEGATIONS.iter().any(|neg| near.contains(neg)) {
                return intent_out("task", "imperative_ba", directives);
            }
        }
    }
    intent_out("followup", "no_action_directive", vec![])
}

pub const TODO_QUESTION_TAIL_WORDS: [&str; 7] = ["吗", "呢", "了没", "没有", "了吧", "了么", "没"];

/// #448-2 完成词表（先查否定词）。
pub const TODO_REPLY_DONE_WORDS: [&str; 22] = [
    "已完成", "已经完成", "均已改完", "完成了", "已做完", "修好了", "修好", "已修复", "修复完成",
    "搞定了", "搞定", "已上线", "上线了", "改好了", "已改好", "已落库", "已生效",
    "done", "finished", "fixed", "resolved", "completed",
];
pub const TODO_REPLY_DONE_NEG: [&str; 23] = [
    "未完成", "没完成", "还没完成", "没有完成", "完不成", "未做完", "没做完",
    "未修好", "没修好", "还没修好", "未修复", "没修复", "未搞定", "没搞定",
    "未上线", "还没上线", "没上线", "未生效", "还没生效",
    "not done", "not fixed", "unfinished", "incomplete",
];
pub const TODO_REPLY_DOING_WORDS: [&str; 11] = [
    "进行中", "已开工", "开工", "正在做", "开始做", "已开始", "在做了", "处理中",
    "doing", "in progress", "wip",
];

/// agent 回复里是否报完成（否定词优先）。
pub fn reply_is_done(text: &str) -> bool {
    let low = text.to_lowercase();
    TODO_REPLY_DONE_WORDS.iter().any(|w| low.contains(w))
        && !TODO_REPLY_DONE_NEG.iter().any(|w| low.contains(w))
}

/// agent 回复里是否报开工。
pub fn reply_is_doing(text: &str) -> bool {
    let low = text.to_lowercase();
    TODO_REPLY_DOING_WORDS.iter().any(|w| low.contains(w))
}

/// 正文里对条目的引用：`td_xxx` / `#<seq>`。
pub fn todo_ref_ids(text: &str) -> Vec<String> {
    // Python `td_[A-Za-z0-9_\-]{1,64}` 贪婪 findall
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b't' && text[i..].starts_with("td_") {
            let start = i + 3;
            let mut j = start;
            while j < bytes.len() && j - start < 64 {
                let c = bytes[j];
                if c.is_ascii_alphanumeric() || c == b'_' || c == b'-' {
                    j += 1;
                } else {
                    break;
                }
            }
            if j > start {
                out.push(text[start..j].to_string());
                i = j;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// 正文里 `#<seq>` 引用（1~9 位数字）。
pub fn todo_ref_seqs(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'#' {
            let mut j = i + 1;
            if j < bytes.len() && bytes[j] == b' ' {
                j += 1; // `#\s?` 可选一个空格
            }
            let start = j;
            while j < bytes.len() && bytes[j].is_ascii_digit() && j - start < 9 {
                j += 1;
            }
            if j > start {
                out.push(text[start..j].to_string());
                i = j;
                continue;
            }
        }
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dedupe_key() {
        assert_eq!(todo_dedupe_key("#476 如下任务： a b c"), "如下任务：abc");
        assert_eq!(todo_dedupe_key("同一句话  重发"), todo_dedupe_key("同一句话重发"));
        let long: String = "甲".repeat(200);
        assert_eq!(todo_dedupe_key(&long).chars().count(), 120);
    }

    #[test]
    fn test_message_key_strips_mention() {
        // 队列正文剥过点名，消息原文带点名 —— 指纹必须相等（#448 回填实踩）
        let queued = todo_text_from_message("@hermes 如下任务：修网关", Some(448), 160);
        assert_eq!(todo_dedupe_key(&queued), todo_message_key("@hermes 如下任务：修网关"));
    }

    #[test]
    fn test_task_name() {
        assert_eq!(todo_task_name("@hermes 如下任务：1. 修复登录页。要求今天完成"),
                   "修复登录页");
        assert_eq!(todo_task_name("#509 你负责的任务队列"), "的任务队列");
        assert_eq!(todo_task_name("我给你一个任务："), "我给你一个任务"); // 硬停顿不含冒号，抬头剥不到开头
        let long: String = format!("任务：{}", "字".repeat(40));
        let name = todo_task_name(&long);
        assert!(name.ends_with('…'));
        assert_eq!(name.chars().count(), 37);
    }

    #[test]
    fn test_classify_task() {
        let v = classify_human_intent("@hermes 帮我修复登录页的超时问题");
        assert_eq!(v["intent"], serde_json::json!("task"));
        let v = classify_human_intent("你负责如下任务：1. 重新部署网关");
        assert_eq!(v["intent"], serde_json::json!("task"));
        assert_eq!(v["reason"], serde_json::json!("task_introducer"));
        // 疑问句里带祈使标志也算任务
        let v = classify_human_intent("帮我加个导出功能好吗？");
        assert_eq!(v["intent"], serde_json::json!("task"));
        // 处置式祈使兜底（动词不在白名单时才落到这条）
        let v = classify_human_intent("@hermes 把队列里的陈年旧账收拾利索");
        assert_eq!(v["reason"], serde_json::json!("imperative_ba"));
    }

    #[test]
    fn test_classify_followup() {
        let v = classify_human_intent("完成了吗？");
        assert_eq!(v["intent"], serde_json::json!("followup"));
        assert_eq!(v["reason"], serde_json::json!("followup_phrase"));
        // 强催问压过动作词（#494）
        let v = classify_human_intent("我等你的完成结果通报，这是最后一次机会，否则把你从机器上删除");
        assert_eq!(v["reason"], serde_json::json!("followup_strong"));
        // 否定动作（「什么都不做」）
        let v = classify_human_intent("如果这次你还是什么都不做在这里糊弄我");
        assert_eq!(v["intent"], serde_json::json!("followup"));
        // 抬头没正文
        let v = classify_human_intent("我给你一个任务：");
        assert_eq!(v["reason"], serde_json::json!("task_preamble_no_payload"));
        // 定语「你负责的任务队列」——但句中「测试」是动作词 → 仍判 task（与 Python 一致，
        // #509 实踩后 Python 保留这个行为，靠台账可回查）
        let v = classify_human_intent("我们继续测试你负责的任务队列");
        assert_eq!(v["intent"], serde_json::json!("task"));
        // 空文
        assert_eq!(classify_human_intent("   ")["reason"], serde_json::json!("empty_text"));
    }

    #[test]
    fn test_reply_done_words() {
        assert!(reply_is_done("已经完成，已上线"));
        assert!(!reply_is_done("还没完成")); // 否定词优先
        assert!(!reply_is_done("未修复，还在排查"));
        assert!(reply_is_doing("正在做，已开工"));
        assert!(!reply_is_doing("还没完成")); // done 优先于 doing
    }

    #[test]
    fn test_refs() {
        assert_eq!(todo_ref_ids("看 td_abc_123 和别的"), vec!["abc_123".to_string()]);
        assert_eq!(todo_ref_seqs("#448 与 #12"), vec!["448".to_string(), "12".to_string()]);
        assert!(todo_ref_seqs("没有引用").is_empty());
    }

    #[test]
    fn test_strip_leading_mentions() {
        assert_eq!(strip_leading_mentions("@hermes，你好"), "你好");
        assert_eq!(strip_leading_mentions("@hermes 如下任务"), "如下任务");
        assert_eq!(strip_leading_mentions("@a @b 修东西"), "修东西");
        assert_eq!(strip_leading_mentions("没有点名"), "没有点名");
    }
}
