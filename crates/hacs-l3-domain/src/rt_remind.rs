//! `rt_remind` —— 每日逾期提醒（对应 Python `hub/l3_domain/rt_remind.py`，
//! 633 行；一次性跑完即退，不常驻、不监听任何端口）。
//!
//! 口径（与 Python 逐条对齐）：
//! * **逾期** = status 还没闭环（action: open/doing；issue: open/watching）
//!   且 due_at 非空且**严格早于** now（正好等于 now 不算逾期）；
//! * **同一事项当天只提醒一次**：以 UTC 日历日零点为界查 reminder_log；
//! * 点名短消息 ≤900 字、最多列 5 项（超了写"…等共 N 项"）；
//! * 无主的归最后一组"（未指定责任人）"，不许悄悄丢掉，也发不出去（跳过）；
//! * **commit 发送路径默认被拒**（2026-09-20 指令），须 `RT_REMIND_ENABLE=1`；
//!   dry-run / fixture 演练永远可用。
//!
//! 时间口径：裸时间一律按 UTC；显示统一北京时间 `YYYY-MM-DD HH:MM CST`。
//! 纯逻辑 + MemoryStore（fixture 离线自测）落这里；PgStore / NATS sender / argv
//! 装配在 bin。

use serde_json::Value;

pub const VERSION: &str = "1.0";

pub const ACTION_OPEN_STATUSES: [&str; 2] = ["open", "doing"];
pub const ISSUE_OPEN_STATUSES: [&str; 2] = ["open", "watching"];

pub const DEFAULT_LIMIT: i64 = 200;
pub const DEFAULT_TIMEOUT_S: f64 = 60.0;
pub const MAX_TIMEOUT_S: f64 = 300.0;
pub const TITLE_MAX: usize = 120;
pub const CALL_ITEMS_MAX: usize = 5;
pub const CALL_TEXT_MAX: usize = 900;

pub const NO_OWNER: &str = "（未指定责任人）";

/// 人话标签：英文枚举进来、中文出去（与看板口径一致）。
pub fn kind_cn(kind: &str) -> &'static str {
    match kind {
        "action" => "落实事项",
        "defect" => "疑似缺陷",
        _ => "遗留问题",
    }
}

pub fn status_cn(status: &str) -> &str {
    match status {
        "open" => "待办",
        "doing" => "进行中",
        "watching" => "观察中",
        other => other,
    }
}

// ================================================================ 小工具

/// `truncate`：`str` 语义按**字符**截（Python len 是字符数），超长补 `…`。
pub fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        text.to_string()
    } else {
        let cut: String = text.chars().take(limit).collect();
        format!("{cut}…")
    }
}

/// `one_line`：空白压成单空格再截断。
pub fn one_line(text: &str, limit: usize) -> String {
    let squeezed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate(&squeezed, limit)
}

/// ISO timestamptz / 裸时间统一解析为 UTC epoch 秒；认不出来 None。
/// 裸时间（不带时区）一律按 UTC 解释 —— 口径单一。
pub fn to_utc(value: &Value) -> Option<i64> {
    let s = value.as_str()?;
    // PG `timestamptz::text` 的时区偏移可能带成 `+08`（无分钟），补成 `+08:00`
    let normalized = normalize_pg_tz(s);
    hacs_x_identity::view_auth::parse_ts(&normalized)
}

/// `2026-10-16 11:10:58.075411+08` → `+08` 补 `:00`（psycopg2 到手的是 datetime，
/// 我们走 text 通道，得在这儿把 PG 的缩写偏移归一）。
fn normalize_pg_tz(s: &str) -> String {
    let bytes = s.as_bytes();
    if bytes.len() >= 3 {
        let tail = &s[s.len() - 3..];
        if (tail.starts_with('+') || tail.starts_with('-'))
            && tail[1..].chars().all(|c| c.is_ascii_digit())
        {
            return format!("{}:00", s);
        }
    }
    s.to_string()
}

/// 当天（UTC 日历日）零点的 epoch 秒。
pub fn day_start(now: i64) -> i64 {
    now.div_euclid(86400) * 86400
}

/// 逾期 = 还没闭环 且 有截止时间 且 截止时间严格早于 now。
pub fn is_overdue(due_at: Option<i64>, status: &str, open_states: &[&str], now: i64) -> bool {
    open_states.contains(&status) && due_at.map(|d| d < now).unwrap_or(false)
}

/// 逾期多久，人话版：`3 天 5 小时` / `2 小时 10 分钟` / `7 分钟`。
pub fn human_late(due_at: Option<i64>, now: i64) -> String {
    let Some(m) = due_at else {
        return "未定截止".into();
    };
    let seconds = now - m;
    if seconds < 0 {
        return "还没到期".into();
    }
    let days = seconds / 86400;
    let rest = seconds % 86400;
    let hours = rest / 3600;
    let minutes = (rest % 3600) / 60;
    if days > 0 {
        format!("{days} 天 {hours} 小时")
    } else if hours > 0 {
        format!("{hours} 小时 {minutes} 分钟")
    } else {
        format!("{minutes} 分钟")
    }
}

/// 统一显示成北京时间（UTC+8）：`YYYY-MM-DD HH:MM CST`。
pub fn fmt_time(value: Option<i64>) -> String {
    let Some(m) = value else {
        return "未定截止".into();
    };
    let (y, mo, d, h, mi) = crate::team_group::beijing_parts_pub(m);
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02} CST")
}

// ================================================================ 事项的统一形状

/// action / issue 两种行规整成同一个形状，后面的分组与渲染只认它。
#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    pub target_kind: String, // "action" | "issue"
    pub target_id: Value,
    pub title: String,
    pub owner: Option<String>,
    pub due_at: Option<i64>,
    pub status: String,
    pub session_id: Option<String>,
    pub issue_kind: Option<String>, // issue 的 kind：issue | defect
    pub topic: Option<String>,
}

/// `make_item`：issue 的 kind 规整——认不出来的归 "issue"。
pub fn make_item(
    target_kind: &str,
    target_id: Value,
    title: &Value,
    owner: &Value,
    due_at: &Value,
    status: &str,
    session_id: Option<&str>,
    issue_kind: Option<&str>,
    topic: Option<&str>,
) -> Item {
    let mut kind = issue_kind.unwrap_or("").trim().to_lowercase();
    if target_kind != "issue" || (kind != "issue" && kind != "defect") {
        kind = if target_kind == "issue" {
            "issue".to_string()
        } else {
            String::new()
        };
    }
    Item {
        target_kind: target_kind.to_string(),
        target_id,
        title: one_line(title.as_str().unwrap_or(""), 500),
        owner: owner
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from),
        due_at: to_utc(due_at),
        status: status.to_string(),
        session_id: session_id.map(String::from),
        issue_kind: if kind.is_empty() { None } else { Some(kind) },
        topic: topic.map(String::from),
    }
}

/// 这条事项在人话里叫什么：落实事项 / 遗留问题 / 疑似缺陷。
pub fn label_of(item: &Item) -> &'static str {
    if item.target_kind == "action" {
        return kind_cn("action");
    }
    kind_cn(item.issue_kind.as_deref().unwrap_or("issue"))
}

pub fn item_key(item: &Item) -> (String, Value) {
    (item.target_kind.clone(), item.target_id.clone())
}

/// 内存版筛逾期（PG 那边同样的条件下推到 SQL，两边共用 is_overdue 一份判据），
/// 按"逾期最久的排最前"（due_at, kind, id 字典序）。
pub fn select_overdue(items: &[Item], now: i64) -> Vec<Item> {
    let mut out: Vec<Item> = items
        .iter()
        .filter(|item| {
            let states: &[&str] = if item.target_kind == "action" {
                &ACTION_OPEN_STATUSES
            } else {
                &ISSUE_OPEN_STATUSES
            };
            is_overdue(item.due_at, &item.status, states, now)
        })
        .cloned()
        .collect();
    out.sort_by(|a, b| {
        (
            a.due_at.unwrap_or(0),
            a.target_kind.clone(),
            a.target_id.to_string(),
        )
            .cmp(&(
                b.due_at.unwrap_or(0),
                b.target_kind.clone(),
                b.target_id.to_string(),
            ))
    });
    out
}

/// 按责任人分组。无主的归到最后一组，**不许被悄悄丢掉**。
pub fn group_by_owner(items: &[Item]) -> Vec<(String, Vec<Item>)> {
    let mut buckets: std::collections::HashMap<String, Vec<Item>> =
        std::collections::HashMap::new();
    let mut order: Vec<String> = Vec::new();
    for item in items {
        let owner = item.owner.clone().unwrap_or_else(|| NO_OWNER.to_string());
        let entry = buckets.entry(owner.clone()).or_default();
        if entry.is_empty() {
            order.push(owner);
        }
        entry.push(item.clone());
    }
    let mut named: Vec<String> = order
        .iter()
        .filter(|o| o.as_str() != NO_OWNER)
        .cloned()
        .collect();
    named.sort();
    if buckets.contains_key(NO_OWNER) {
        named.push(NO_OWNER.to_string());
    }
    named
        .into_iter()
        .map(|o| {
            let rows = buckets.get(&o).cloned().unwrap_or_default();
            (o, rows)
        })
        .collect()
}

// ================================================================ 文本渲染

/// ① 按责任人分组的汇总文本（给人看的那一份）。
pub fn render_summary(groups: &[(String, Vec<Item>)], now: i64, skipped: usize) -> String {
    let total: usize = groups.iter().map(|(_, rows)| rows.len()).sum();
    let mut lines = vec![format!(
        "【逾期提醒】截至 {}，待提醒 {} 项，涉及 {} 人",
        fmt_time(Some(now)),
        total,
        groups.len()
    )];
    if skipped > 0 {
        lines.push(format!("（另有 {skipped} 项今天已经提醒过，本次不再重复）"));
    }
    if total == 0 {
        lines.push("没有逾期事项 —— 今天不用喊人。".to_string());
        return lines.join("\n");
    }
    for (owner, rows) in groups {
        lines.push(String::new());
        lines.push(format!("== {owner}（{} 项）==", rows.len()));
        if owner == NO_OWNER {
            lines.push("   ！这几项没有责任人，点名发不出去，需要人工认领。".to_string());
        }
        for (idx, item) in rows.iter().enumerate() {
            lines.push(format!(
                "   {}. [{} #{}] {}",
                idx + 1,
                label_of(item),
                item.target_id,
                one_line(&item.title, 200)
            ));
            lines.push(format!(
                "      截止 {}，已逾期 {}；状态 {}；来源会议 {}",
                fmt_time(item.due_at),
                human_late(item.due_at, now),
                status_cn(&item.status),
                item.topic
                    .as_deref()
                    .or(item.session_id.as_deref())
                    .unwrap_or("-")
            ));
        }
    }
    lines.join("\n")
}

/// ② 给一个责任人的**点名短消息**（发到 a2a.v1.req.<owner>）。
/// 刻意写短：点名是要人干活，不是给人读报告。
pub fn render_call(owner: &str, rows: &[Item], now: i64) -> String {
    let head = format!("[逾期提醒] {owner}：你有 {} 项已逾期未闭环。", rows.len());
    let mut parts: Vec<String> = Vec::new();
    for (idx, item) in rows.iter().take(CALL_ITEMS_MAX).enumerate() {
        parts.push(format!(
            "{}) 【{}】{}（截止 {}，逾期 {}）",
            idx + 1,
            label_of(item),
            one_line(&item.title, TITLE_MAX),
            fmt_time(item.due_at),
            human_late(item.due_at, now)
        ));
    }
    if rows.len() > CALL_ITEMS_MAX {
        parts.push(format!("…等共 {} 项", rows.len()));
    }
    let tail = "请今天内做到二选一：把事办完并回帖闭环，或者说明为什么改期、改到什么时候。\
                缺陷单（疑似程序缺陷）必须查清原因，不许直接关掉。";
    truncate(&format!("{head} {} {tail}", parts.join(" ")), CALL_TEXT_MAX)
}

// ================================================================ 内存库（fixture / 单测）

#[derive(Default)]
pub struct MemoryStore {
    pub items: Vec<Item>,
    pub sent: Vec<(String, Value, i64)>,
    pub logged: Vec<(String, Value, Option<String>)>,
}

impl MemoryStore {
    /// 过滤口径与 PgStore 共用 select_overdue / is_overdue，不重写判据。
    /// limit 在 PgStore 那边是**每张表**的上限，两张表合起来 2×limit，这里对齐。
    pub fn overdue(&self, now: i64, limit: i64) -> Vec<Item> {
        select_overdue(&self.items, now)
            .into_iter()
            .take((limit * 2) as usize)
            .collect()
    }

    /// 今天（UTC 日历日）已经提醒过的 (kind, id) 集合。
    pub fn sent_today(&self, now: i64) -> Vec<(String, Value)> {
        let start = day_start(now);
        self.sent
            .iter()
            .filter(|(_, _, ts)| *ts >= start)
            .map(|(k, i, _)| (k.clone(), i.clone()))
            .collect()
    }

    pub fn log_reminder(&mut self, target_kind: &str, target_id: &Value, owner: Option<&str>) {
        self.logged.push((
            target_kind.to_string(),
            target_id.clone(),
            owner.map(String::from),
        ));
        self.sent
            .push((target_kind.to_string(), target_id.clone(), now_epoch()));
    }
}

fn now_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 从 JSON 造一个内存库（--fixture 离线自测用）。
pub fn load_fixture(data: &Value) -> MemoryStore {
    let mut store = MemoryStore::default();
    for row in data
        .get("actions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        store.items.push(make_item(
            "action",
            row.get("action_id").cloned().unwrap_or(Value::Null),
            row.get("what").unwrap_or(&Value::Null),
            row.get("owner").unwrap_or(&Value::Null),
            row.get("due_at").unwrap_or(&Value::Null),
            row.get("status").and_then(Value::as_str).unwrap_or("open"),
            row.get("session_id").and_then(Value::as_str),
            None,
            row.get("topic").and_then(Value::as_str),
        ));
    }
    for row in data
        .get("issues")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        store.items.push(make_item(
            "issue",
            row.get("issue_id").cloned().unwrap_or(Value::Null),
            row.get("title").unwrap_or(&Value::Null),
            row.get("owner").unwrap_or(&Value::Null),
            row.get("due_at").unwrap_or(&Value::Null),
            row.get("status").and_then(Value::as_str).unwrap_or("open"),
            row.get("session_id").and_then(Value::as_str),
            Some(row.get("kind").and_then(Value::as_str).unwrap_or("issue")),
            row.get("topic").and_then(Value::as_str),
        ));
    }
    for row in data
        .get("reminder_log")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let ts = to_utc(row.get("sent_at").unwrap_or(&Value::Null)).unwrap_or_else(now_epoch);
        store.sent.push((
            row.get("target_kind")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            row.get("target_id").cloned().unwrap_or(Value::Null),
            ts,
        ));
    }
    store
}

// ================================================================ 主流程

/// 发送通道抽象：NatsSender（bin）/ PrintSender（dry-run）都长这样。
pub trait Sender {
    fn name(&self) -> &'static str;
    fn send(&mut self, owner: &str, text: &str) -> (bool, String);
}

/// `--dry-run` 用的发送通道：只收集要发什么，一条消息都不发。
#[derive(Default)]
pub struct PrintSender {
    pub sent: Vec<(String, String)>,
}

impl Sender for PrintSender {
    fn name(&self) -> &'static str {
        "dry-run"
    }
    fn send(&mut self, owner: &str, text: &str) -> (bool, String) {
        self.sent.push((owner.to_string(), text.to_string()));
        (true, "dry-run 未发送".into())
    }
}

/// 跑一次提醒的统计（单测直接断言它）。
#[derive(Debug, PartialEq)]
pub struct Stats {
    pub overdue: usize,
    pub pending: usize,
    pub skipped: usize,
    pub owners: usize,
    pub sent: usize,
    pub failed: usize,
    pub logged: usize,
    pub commit: bool,
}

/// `run_remind`：commit=false（--dry-run）不写 reminder_log、不发消息，只打印。
/// 返回 (统计, 全部打印行)。
pub fn run_remind(
    store: &mut MemoryStore,
    sender: &mut dyn Sender,
    now: i64,
    commit: bool,
    limit: i64,
    owner_filter: Option<&str>,
) -> (Stats, Vec<String>) {
    let mut say: Vec<String> = Vec::new();
    let items_all = store.overdue(now, limit);
    let items = select_overdue(&items_all, now); // PG 已筛过一遍，这里兜底保证口径一致
    let items: Vec<Item> = items
        .into_iter()
        .filter(|i| {
            owner_filter
                .map(|f| (i.owner.as_deref().unwrap_or("")) == f)
                .unwrap_or(true)
        })
        .collect();

    let already = store.sent_today(now);
    let pending: Vec<&Item> = items
        .iter()
        .filter(|i| !already.contains(&item_key(i)))
        .collect();
    let skipped = items.len() - pending.len();

    let owned: Vec<Item> = pending.iter().map(|i| (*i).clone()).collect();
    let groups = group_by_owner(&owned);
    say.push(render_summary(&groups, now, skipped));
    say.push(String::new());

    let mut sent_ok = 0usize;
    let mut failed = 0usize;
    let mut logged = 0usize;
    for (owner, rows) in &groups {
        let text = render_call(owner, rows, now);
        if owner == NO_OWNER {
            say.push(format!(
                "[跳过] {} 项没有责任人，点名发不出去，等人工认领",
                rows.len()
            ));
            continue;
        }
        if !commit {
            let _ = sender.send(owner, &text); // PrintSender：只打印
            continue;
        }
        let (ok, detail) = sender.send(owner, &text);
        if ok {
            sent_ok += 1;
            say.push(format!(
                "[已点名] {owner}（{} 项）→ {}",
                rows.len(),
                truncate(&detail, 120)
            ));
            for item in rows {
                // 发成功才记账，失败留到明天再喊
                store.log_reminder(&item.target_kind, &item.target_id, Some(owner));
                logged += 1;
            }
        } else {
            failed += 1;
            say.push(format!(
                "[点名失败] {owner}（{} 项）：{detail} —— 本次不记 reminder_log，下次还会提醒",
                rows.len()
            ));
        }
    }

    let stats = Stats {
        overdue: items.len(),
        pending: pending.len(),
        skipped,
        owners: groups.len(),
        sent: sent_ok,
        failed,
        logged,
        commit,
    };
    say.push(format!(
        "[汇总] 逾期 {} 项（今天已提醒过 {} 项）／待提醒 {} 项／责任人 {} 人／已点名 {} 人／失败 {} 人／记账 {} 条／模式 {}",
        stats.overdue, stats.skipped, stats.pending, stats.owners,
        stats.sent, stats.failed, stats.logged,
        if commit { "commit" } else { "dry-run" },
    ));
    (stats, say)
}

// ================================================================ 单测

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // 相对锚：2026-10-17 04:00 UTC = 12:00 北京
    const NOW: i64 = 1_792_209_600;

    fn iso(epoch: i64) -> String {
        // UTC ISO（带 Z），parse_ts 认识
        let days = epoch.div_euclid(86400);
        let secs = epoch.rem_euclid(86400);
        let (y, mo, d, h, mi) = crate::team_group::beijing_parts_pub(epoch); // +8 口径，回退 8h 得 UTC
        let _ = (days, secs);
        // beijing_parts 是 +8；要 UTC 就先减 8h
        let (y, mo, d, h, mi) = crate::team_group::beijing_parts_pub(epoch - 8 * 3600);
        format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:00Z")
    }

    fn item(
        kind: &str,
        id: i64,
        title: &str,
        owner: Option<&str>,
        due_at: Option<i64>,
        status: &str,
        issue_kind: Option<&str>,
    ) -> Item {
        make_item(
            kind,
            json!(id),
            &json!(title),
            &owner.map(|o| serde_json::json!(o)).unwrap_or(Value::Null),
            &due_at.map(|d| json!(iso(d))).unwrap_or(Value::Null),
            status,
            Some("sess-1"),
            issue_kind,
            Some("话题A"),
        )
    }

    #[test]
    fn test_is_overdue_strict_boundary() {
        // 正好等于 now 不算逾期（严格早于）
        assert!(!is_overdue(Some(NOW), "open", &ACTION_OPEN_STATUSES, NOW));
        assert!(is_overdue(
            Some(NOW - 1),
            "open",
            &ACTION_OPEN_STATUSES,
            NOW
        ));
        // 闭环了不算
        assert!(!is_overdue(
            Some(NOW - 1),
            "done",
            &ACTION_OPEN_STATUSES,
            NOW
        ));
        // 没截止不算
        assert!(!is_overdue(None, "open", &ACTION_OPEN_STATUSES, NOW));
    }

    #[test]
    fn test_human_late_and_fmt_time() {
        // 3 天 5 小时前
        assert_eq!(
            human_late(Some(NOW - 3 * 86400 - 5 * 3600), NOW),
            "3 天 5 小时"
        );
        assert_eq!(
            human_late(Some(NOW - 2 * 3600 - 10 * 60), NOW),
            "2 小时 10 分钟"
        );
        assert_eq!(human_late(Some(NOW - 7 * 60), NOW), "7 分钟");
        assert_eq!(human_late(Some(NOW + 60), NOW), "还没到期");
        assert_eq!(human_late(None, NOW), "未定截止");
        assert_eq!(fmt_time(Some(NOW)), "2026-10-17 12:00 CST");
        assert_eq!(fmt_time(None), "未定截止");
    }

    #[test]
    fn test_issue_kind_normalization() {
        // defect 保留；乱写的归 issue；action 一律无 kind
        let d = item("issue", 1, "t", Some("o"), None, "open", Some("defect"));
        assert_eq!(d.issue_kind.as_deref(), Some("defect"));
        let w = item("issue", 2, "t", Some("o"), None, "open", Some("weird"));
        assert_eq!(w.issue_kind.as_deref(), Some("issue"));
        let a = item("action", 3, "t", Some("o"), None, "open", Some("defect"));
        assert_eq!(a.issue_kind, None);
        assert_eq!(label_of(&d), "疑似缺陷");
        assert_eq!(label_of(&w), "遗留问题");
        assert_eq!(label_of(&a), "落实事项");
    }

    #[test]
    fn test_select_overdue_sort_and_states() {
        let items = vec![
            item(
                "action",
                1,
                "新逾期",
                Some("o"),
                Some(NOW - 10),
                "open",
                None,
            ),
            item(
                "action",
                2,
                "闭环了",
                Some("o"),
                Some(NOW - 20),
                "done",
                None,
            ),
            item(
                "issue",
                3,
                "更早",
                Some("o"),
                Some(NOW - 99999),
                "watching",
                None,
            ),
            item(
                "issue",
                4,
                "defect",
                Some("o"),
                Some(NOW - 500),
                "open",
                Some("defect"),
            ),
        ];
        let out = select_overdue(&items, NOW);
        let ids: Vec<i64> = out.iter().map(|i| i.target_id.as_i64().unwrap()).collect();
        assert_eq!(ids, vec![3, 4, 1]); // 逾期最久在前；done 不算
    }

    #[test]
    fn test_group_by_owner_no_owner_last() {
        let items = vec![
            item("action", 1, "t", Some("zeta"), None, "open", None),
            item("action", 2, "t", None, None, "open", None),
            item("action", 3, "t", Some("alpha"), None, "open", None),
            item("action", 4, "t", None, None, "open", None),
        ];
        let groups = group_by_owner(&items);
        let owners: Vec<&str> = groups.iter().map(|(o, _)| o.as_str()).collect();
        assert_eq!(owners, vec!["alpha", "zeta", NO_OWNER]);
        assert_eq!(groups[2].1.len(), 2);
    }

    #[test]
    fn test_render_call_limits() {
        let rows: Vec<Item> = (0..8)
            .map(|i| {
                item(
                    "action",
                    i,
                    &format!("事情{i}"),
                    Some("o"),
                    Some(NOW - 3600),
                    "open",
                    None,
                )
            })
            .collect();
        let call = render_call("张三", &rows, NOW);
        assert!(call.starts_with("[逾期提醒] 张三：你有 8 项已逾期未闭环。"));
        assert!(call.contains("5) 【落实事项】"));
        assert!(!call.contains("6) "));
        assert!(call.contains("…等共 8 项"));
        assert!(call.chars().count() <= CALL_TEXT_MAX);
        assert!(call.ends_with("不许直接关掉。"));
    }

    #[test]
    fn test_truncate_one_line() {
        assert_eq!(truncate("abcdef", 3), "abc…");
        assert_eq!(truncate("中文截断测", 2), "中文…");
        assert_eq!(truncate("短", 3), "短");
        assert_eq!(one_line("  a \n\t b  ", 10), "a b");
    }

    #[test]
    fn test_fixture_and_run_remind_dry() {
        let data = json!({
            "actions": [
                {"action_id": 11, "what": "交周报", "owner": "alpha",
                 "due_at": "2026-10-16T00:00:00Z", "status": "open", "topic": "周会"},
                {"action_id": 12, "what": "已办完", "owner": "alpha",
                 "due_at": "2026-10-16T00:00:00Z", "status": "done", "topic": "周会"}
            ],
            "issues": [
                {"issue_id": 21, "title": "登录失败", "owner": "beta",
                 "due_at": "2026-10-15T00:00:00Z", "status": "open", "kind": "defect",
                 "topic": "周会"},
                {"issue_id": 22, "title": "无主事项", "owner": null,
                 "due_at": "2026-10-15T00:00:00Z", "status": "open", "kind": "issue"}
            ],
            "reminder_log": []
        });
        let mut store = load_fixture(&data);
        let mut sender = PrintSender::default();
        let (stats, lines) = run_remind(&mut store, &mut sender, NOW, false, 200, None);
        // 逾期 = action#11 + issue#21 + issue#22（done 不算）
        assert_eq!(stats.overdue, 3);
        assert_eq!(stats.pending, 3);
        assert_eq!(stats.skipped, 0);
        assert_eq!(stats.owners, 3); // alpha / beta / 无主
        assert_eq!(stats.sent, 0); // dry-run 不记账
        assert_eq!(stats.commit, false);
        // dry-run 打了两条点名 + 1 条无主跳过
        assert_eq!(sender.sent.len(), 2);
        assert!(lines.iter().any(|l| l.starts_with("[跳过] 1 项没有责任人")));
        assert!(lines.iter().any(|l| l.contains("[汇总] 逾期 3 项")));
        // 点名文本含 defect 标签
        let beta_call = sender.sent.iter().find(|(o, _)| o == "beta").unwrap();
        assert!(beta_call.1.contains("【疑似缺陷】"));
    }

    #[test]
    fn test_run_remind_commit_and_dedup() {
        let data = json!({
            "actions": [
                {"action_id": 11, "what": "交周报", "owner": "alpha",
                 "due_at": "2026-10-16T00:00:00Z", "status": "open", "topic": "周会"}
            ],
            "issues": [],
            "reminder_log": []
        });
        let mut store = load_fixture(&data);
        let mut sender = PrintSender::default();
        // 第一天 commit：发送 + 记账
        let (stats, _) = run_remind(&mut store, &mut sender, NOW, true, 200, None);
        assert_eq!((stats.sent, stats.logged), (1, 1));
        // 同一天再跑：今天已提醒过 → pending 0
        let (stats2, lines2) = run_remind(&mut store, &mut sender, NOW + 3600, true, 200, None);
        assert_eq!(stats2.pending, 0);
        assert_eq!(stats2.skipped, 1);
        assert!(lines2.iter().any(|l| l.contains("没有逾期事项")));
        // 次日（UTC 日历日翻转）：又会提醒
        let (stats3, _) = run_remind(&mut store, &mut sender, NOW + 86400 + 10, true, 200, None);
        assert_eq!(stats3.pending, 1);
    }

    #[test]
    fn test_run_remind_owner_filter() {
        let data = json!({
            "actions": [
                {"action_id": 1, "what": "a的事", "owner": "alpha",
                 "due_at": "2026-10-16T00:00:00Z", "status": "open"},
                {"action_id": 2, "what": "b的事", "owner": "beta",
                 "due_at": "2026-10-16T00:00:00Z", "status": "open"}
            ],
            "issues": []
        });
        let mut store = load_fixture(&data);
        let mut sender = PrintSender::default();
        let (stats, _) = run_remind(&mut store, &mut sender, NOW, false, 200, Some("alpha"));
        assert_eq!(stats.overdue, 1);
        assert_eq!(stats.owners, 1);
    }
}
