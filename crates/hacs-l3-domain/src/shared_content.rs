//! `shared_content` —— 共享内容（任务级/永久级）管理 CLI 的纯逻辑（对应 Python
//! `hub/l3_domain/shared_content.py`）。
//!
//! 口径（=Python，逐条对齐）：
//! * 任务级（--scope task）必须挂任务、必须有过期时间（没给就继承任务兜底 TTL）；
//!   永久级不许挂 task_id。
//! * 默认过滤：不返回 revoked/destroyed/expired；`--status` 显式给时一切听它的。
//! * 过期 = 有过期时间且**严格早于**现在（等于不算）。
//! * update 禁止改到 destroyed；destroy = status='destroyed' + 正文置空 + 向量清空，
//!   **不是 DELETE**（留痕由数据库触发器写）。
//! * sweep 三规则：任务结束/TTL 到点 → 该任务全部内容销毁；任务级内容到期 → 销毁；
//!   永久级到期 → 只标 expired（正文保留）。缺省 dry-run。
//! * 嵌入：模型不可用就留 NULL / 退回关键词检索，绝不让写入或检索因为模型而失败。
//!   （Rust 侧 embedder 尚未移植，compute_embedding 现恒 None —— 偏差登记 PARITY。）
//!
//! 存储抽象为 `Store` trait：bin 组合根接 PG（postgres crate）；单测用 MemoryStore。

use serde_json::{json, Map, Value};
use std::collections::HashSet;

pub const VERSION: &str = "1.0";
pub const DEFAULT_LIMIT: usize = 50;
pub const MAX_LIMIT: usize = 500;

pub const SCOPES: &[&str] = &["task", "permanent"];
pub const SENSITIVITIES: &[&str] = &["public", "internal", "sensitive"];
pub const CONTENT_STATUSES: &[&str] = &["active", "expired", "revoked", "destroyed"];
pub const TASK_STATUSES: &[&str] = &["active", "done", "aborted"];
pub const TASK_END_STATUSES: &[&str] = &["done", "aborted"];
/// 默认不端给 Agent 的状态：撤下的和销毁的
pub const HIDDEN_STATUSES: &[&str] = &["revoked", "destroyed"];
/// update 能改到的状态（destroyed 不在里面：销毁要连正文一起清，走 destroy 子命令）
pub const UPDATABLE_STATUSES: &[&str] = &["active", "expired", "revoked"];

pub const SCOPE_CN: &[(&str, &str)] = &[("task", "任务级"), ("permanent", "永久级")];
pub const STATUS_CN: &[(&str, &str)] = &[
    ("active", "生效"),
    ("expired", "已过期"),
    ("revoked", "已撤下"),
    ("destroyed", "已销毁"),
];
pub const TASK_STATUS_CN: &[(&str, &str)] = &[
    ("active", "进行中"),
    ("done", "已完成"),
    ("aborted", "已中止"),
];
pub const SENSITIVITY_CN: &[(&str, &str)] = &[
    ("public", "公开"),
    ("internal", "内部"),
    ("sensitive", "敏感"),
];
pub const ACTION_CN: &[(&str, &str)] = &[
    ("create", "创建"),
    ("update", "修改"),
    ("revoke", "撤下"),
    ("expire", "过期"),
    ("destroy", "销毁"),
    ("restore", "恢复"),
];

pub fn cn(table: &[(&str, &str)], key: &str) -> String {
    table
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v.to_string())
        .unwrap_or_else(|| key.to_string())
}

// ---------------- 业务错误（CliError） ----------------

#[derive(Debug, Clone)]
pub struct CliError(pub String);

impl CliError {
    pub fn new(msg: impl Into<String>) -> Self {
        CliError(msg.into())
    }
}

/// `safe_err`：抹掉 DSN 口令再截断 400 字（password/passfile/sslpassword = 值 → =***）。
pub fn safe_err(text: &str) -> String {
    let flat = text.replace('\n', " ");
    let mut out: Vec<char> = Vec::new();
    let lower = flat.to_lowercase();
    let chars: Vec<char> = flat.chars().collect();
    // 在小写副本上找关键词（ASCII，char 索引一致）
    #[allow(unused_variables)]
    let low: Vec<char> = lower.chars().collect();
    let mut i = 0usize;
    while i < chars.len() {
        let mut hit: Option<(usize, &'static str)> = None;
        for k in ["password", "passfile", "sslpassword"] {
            if let Some(p) = lower[i..].find(k) {
                let at = i + p;
                hit = Some(match hit {
                    Some((a, _)) if a <= at => hit.unwrap(),
                    _ => (at, k),
                });
                break;
            }
        }
        match hit {
            Some((at, k)) => {
                out.extend(chars[i..at].iter());
                out.extend(k.chars());
                // 吃掉 = 与 \S+
                let mut j = at + k.len();
                while j < chars.len() && chars[j] == '=' {
                    out.push('=');
                    j += 1;
                }
                let start = j;
                while j < chars.len() && !chars[j].is_whitespace() {
                    j += 1;
                }
                if j > start {
                    out.extend("***".chars());
                }
                i = j;
            }
            None => {
                out.extend(chars[i..].iter());
                break;
            }
        }
    }
    let s2: String = out.into_iter().collect();
    let s2 = s2.trim().to_string();
    if s2.is_empty() {
        return "Err".to_string();
    }
    s2.chars().take(400).collect()
}

/// 读 DSN 文件：只认 `BB_PG_DSN=` 行（绝不能 source——值里带空格会截断）。
pub fn read_dsn(path: &str) -> Result<String, CliError> {
    let txt = std::fs::read_to_string(path).map_err(|e| {
        CliError(format!(
            "读不到 DSN 文件 {path}：{}",
            safe_err(&e.to_string())
        ))
    })?;
    for line in txt.lines() {
        if let Some(v) = line.strip_prefix("BB_PG_DSN=") {
            let v = v.trim_end_matches(['\r', '\n']).trim();
            let v = if v.len() >= 2
                && (v.starts_with('\'') || v.starts_with('"'))
                && v.ends_with(&v[..1])
            {
                &v[1..v.len() - 1]
            } else {
                v
            };
            return Ok(v.to_string());
        }
    }
    Ok(String::new())
}

// ---------------- 时间小工具 ----------------

/// `fmt_time`：人类那栏统一北京时间（UTC+8）`%m-%d %H:%M`。
pub fn fmt_time(value: Option<&str>) -> String {
    let Some(s) = value else { return "-".into() };
    let Some(epoch) = parse_epoch(s) else {
        return "-".into();
    };
    let (_y, mo, d, h, mi) = crate::team_group::beijing_parts_pub(epoch);
    format!("{mo:02}-{d:02} {h:02}:{mi:02}")
}

/// 把 PG/ISO 时间解析成 epoch 秒（亚秒丢弃）。支持 `2026-10-17 11:00:00+08`、
/// `2026-10-17T11:00:00+08:00`、`2026-10-17T03:00:00Z`、无时区按 UTC。
pub fn parse_epoch(s: &str) -> Option<i64> {
    hacs_x_identity_parse_epoch(s)
}

// 上面这个函数是个转发占位——真实实现接 X 层 parse_ts；见文件底部 fn。

/// `is_expired`：有过期时间且严格早于 now。
pub fn is_expired(expires_at: Option<&str>, now_epoch: i64) -> bool {
    match expires_at.and_then(parse_epoch) {
        Some(t) => t < now_epoch,
        None => false,
    }
}

/// `_when`：--xx-in-days / --xx-in-hours / --xx-at 三种写法归一成 ISO（UTC）串。
pub fn when(
    now_epoch: i64,
    days: Option<f64>,
    hours: Option<f64>,
    explicit: Option<&str>,
    field: &str,
) -> Result<Option<String>, CliError> {
    if let Some(x) = explicit {
        let t = parse_epoch(x).ok_or_else(|| {
            CliError(format!(
                "{field} 不是合法时间（要 ISO8601，例如 2026-10-01T12:00:00+08:00）"
            ))
        })?;
        return Ok(Some(epoch_to_iso(t)));
    }
    if let Some(d) = days {
        return Ok(Some(epoch_to_iso(now_epoch + (d * 86400.0) as i64)));
    }
    if let Some(h) = hours {
        return Ok(Some(epoch_to_iso(now_epoch + (h * 3600.0) as i64)));
    }
    Ok(None)
}

/// epoch -> `YYYY-MM-DDTHH:MM:SS+00:00`（=python datetime.isoformat() 带 UTC 时区）。
pub fn epoch_to_iso(epoch: i64) -> String {
    // beijing_parts_pub 返回的是北京时间钟面；UTC = 北京 - 8h
    let (y, mo, d, h, mi) = crate::team_group::beijing_parts_pub(epoch - 8 * 3600);
    let sec = epoch.rem_euclid(60) as u32;
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{sec:02}+00:00")
}

// ---------------- 文本小工具 ----------------

/// `one_line`：压成一行并截断（40 字，字符语义）。
pub fn one_line(text: Option<&str>, limit: usize) -> String {
    let flat: String = text
        .unwrap_or("")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if flat.chars().count() <= limit {
        flat
    } else {
        let mut out: String = flat.chars().take(limit).collect();
        out.push('…');
        out
    }
}

/// 中文按两格算的显示宽度。
pub fn display_width(text: &str) -> usize {
    text.chars()
        .map(|c| {
            let c = c as u32;
            if (0x1100..=0x115F).contains(&c)
                || (0x2E80..=0xA4CF).contains(&c)
                || (0xAC00..=0xD7A3).contains(&c)
                || (0xF900..=0xFAFF).contains(&c)
                || (0xFE30..=0xFE4F).contains(&c)
                || (0xFF00..=0xFF60).contains(&c)
                || (0xFFE0..=0xFFE6).contains(&c)
                || (0x3000..=0x303E).contains(&c)
            {
                2
            } else {
                1
            }
        })
        .sum()
}

fn pad(text: &str, width: usize) -> String {
    let w = display_width(text);
    let mut out = text.to_string();
    for _ in 0..width.saturating_sub(w) {
        out.push(' ');
    }
    out
}

/// 中文对齐的纯文本表格；空表给人话。
pub fn render_table(headers: &[&str], rows: &[Vec<String>]) -> String {
    if rows.is_empty() {
        return "（没有匹配的记录）".to_string();
    }
    let mut widths: Vec<usize> = headers.iter().map(|h| display_width(h)).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            if i < widths.len() {
                widths[i] = widths[i].max(display_width(cell));
            }
        }
    }
    let mut lines: Vec<String> = Vec::new();
    lines.push(
        headers
            .iter()
            .enumerate()
            .map(|(i, h)| pad(h, widths[i]))
            .collect::<Vec<_>>()
            .join("  ")
            .trim_end()
            .to_string(),
    );
    lines.push(
        widths
            .iter()
            .map(|w| "-".repeat(*w))
            .collect::<Vec<_>>()
            .join("  "),
    );
    for row in rows {
        lines.push(
            row.iter()
                .enumerate()
                .map(|(i, c)| pad(c, widths[i]))
                .collect::<Vec<_>>()
                .join("  ")
                .trim_end()
                .to_string(),
        );
    }
    lines.join("\n")
}

/// `a,b , c` -> ["a","b","c"]；None -> None，空串 -> []。
pub fn split_list(raw: Option<&str>) -> Option<Vec<String>> {
    raw.map(|s| {
        s.split(',')
            .map(|x| x.trim())
            .filter(|x| !x.is_empty())
            .map(|x| x.to_string())
            .collect()
    })
}

/// `entry:61` -> {"type":"entry","value":61}；没有冒号整串当 value。
pub fn parse_source_refs(raw: &[String]) -> Vec<Value> {
    let mut out = Vec::new();
    for item in raw {
        let text = item.trim();
        if text.is_empty() {
            continue;
        }
        match text.split_once(':') {
            None => out.push(json!({"type": "ref", "value": text})),
            Some((kind, value)) => {
                let value = value.trim();
                let v = if value.chars().all(|c| c.is_ascii_digit()) && !value.is_empty() {
                    json!(value.parse::<i64>().unwrap_or(0))
                } else {
                    json!(value)
                };
                out.push(json!({
                    "type": if kind.trim().is_empty() { "ref" } else { kind.trim() },
                    "value": v,
                }));
            }
        }
    }
    out
}

/// 把用户输入包成 ILIKE 模式串：先转义 `\ % _`。
pub fn like_pattern(q: &str) -> String {
    let mut out = String::from("%");
    for c in q.chars() {
        if matches!(c, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('%');
    out
}

pub fn as_json_list(value: &Value) -> Vec<Value> {
    match value {
        Value::Array(a) => a.clone(),
        Value::String(s) => serde_json::from_str(s).unwrap_or_default(),
        _ => Vec::new(),
    }
}

pub fn as_json_obj(value: &Value) -> Map<String, Value> {
    match value {
        Value::Object(o) => o.clone(),
        Value::String(s) => serde_json::from_str(s).unwrap_or_default(),
        _ => Map::new(),
    }
}

// ---------------- 查询过滤（content_filters） ----------------

#[derive(Debug, Clone, Default)]
pub struct ContentFilter {
    pub scope: Option<String>,
    pub kind: Option<String>,
    pub owner: Option<String>,
    pub tag: Option<String>,
    pub task_id: Option<String>,
    pub status: Option<String>,
    pub expired: bool,
}

/// (where[], params[])。默认口径：不返回撤下与销毁的，也不返回已过期的；
/// status 显式指定时一切听它的。
pub fn content_filters(f: &ContentFilter, now_iso: &str) -> (Vec<String>, Vec<String>) {
    let mut where_ = Vec::new();
    let mut params = Vec::new();
    if let Some(v) = &f.scope {
        where_.push("scope = %s".into());
        params.push(v.clone());
    }
    if let Some(v) = &f.kind {
        where_.push("kind = %s".into());
        params.push(v.clone());
    }
    if let Some(v) = &f.owner {
        where_.push("owner = %s".into());
        params.push(v.clone());
    }
    if let Some(v) = &f.task_id {
        where_.push("task_id = %s".into());
        params.push(v.clone());
    }
    if let Some(v) = &f.tag {
        where_.push("tags @> %s::jsonb".into());
        params.push(json!([v]).to_string());
    }
    if let Some(v) = &f.status {
        where_.push("status = %s".into());
        params.push(v.clone());
    } else {
        where_.push("status NOT IN (%s, %s)".into());
        params.push(HIDDEN_STATUSES[0].into());
        params.push(HIDDEN_STATUSES[1].into());
        if !f.expired {
            where_.push("status <> %s".into());
            params.push("expired".into());
            where_.push("(expires_at IS NULL OR expires_at >= %s::timestamptz)".into());
            params.push(now_iso.to_string());
        }
    }
    (where_, params)
}

// ---------------- 行 -> 契约 ----------------

/// CONTENT_COLUMNS 顺序：content_id, scope, task_id, kind, title, body, tags, owner,
/// audience, sensitivity, status, expires_at, review_at, version, created_at,
/// updated_at, updated_by, destroyed_at, source_refs
pub fn row_to_content(row: &[Value], now_epoch: i64) -> Value {
    let status = row[10].as_str().unwrap_or("");
    json!({
        "content_id": row[0],
        "scope": row[1],
        "task_id": row[2],
        "kind": row[3],
        "title": row[4],
        "body": row[5],
        "tags": as_json_list(&row[6]),
        "owner": row[7],
        "audience": as_json_list(&row[8]),
        "sensitivity": row[9],
        "status": status,
        "expires_at": opt_iso(&row[11]),
        "review_at": opt_iso(&row[12]),
        "version": row[13],
        "created_at": opt_iso(&row[14]),
        "updated_at": opt_iso(&row[15]),
        "updated_by": row[16],
        "destroyed_at": opt_iso(&row[17]),
        "source_refs": as_json_list(&row[18]),
        "expired": status == "expired" || is_expired(row[11].as_str(), now_epoch),
    })
}

fn opt_iso(v: &Value) -> Value {
    match v.as_str() {
        Some(s) if !s.is_empty() => json!(epoch_to_iso(parse_epoch(s).unwrap_or(0))),
        _ => Value::Null,
    }
}

/// TASK_COLUMNS：task_id, title, owner, status, note, created_at, ended_at, ttl_at, destroyed_at
pub fn row_to_task(row: &[Value], now_epoch: i64) -> Value {
    json!({
        "task_id": row[0],
        "title": row[1],
        "owner": row[2],
        "status": row[3],
        "note": row[4],
        "created_at": opt_iso(&row[5]),
        "ended_at": opt_iso(&row[6]),
        "ttl_at": opt_iso(&row[7]),
        "destroyed_at": opt_iso(&row[8]),
        "ttl_expired": is_expired(row[7].as_str(), now_epoch),
    })
}

/// REVISION_COLUMNS：rev_id, content_id, action, actor, changed, note, ts
pub fn row_to_revision(row: &[Value]) -> Value {
    json!({
        "rev_id": row[0],
        "content_id": row[1],
        "action": row[2],
        "actor": row[3],
        "changed": as_json_obj(&row[4]),
        "note": row[5],
        "ts": opt_iso(&row[6]),
    })
}

// ---------------- Store 抽象 ----------------

/// 一次 exec 的取回要求。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Fetch {
    None,
    One,
    All,
}

/// 存储抽象：PG（bin）与 MemoryStore（单测）。占位符统一 `%s`。
pub trait Store {
    fn exec(
        &mut self,
        sql: &str,
        params: &[Value],
        #[allow(unused_variables)] _fetch: Fetch,
    ) -> Result<Vec<Vec<Value>>, CliError>;
    fn set_note(&mut self, note: Option<&str>) -> Result<(), CliError>;
}

/// `compute_embedding`：Rust 侧 embedder 未移植前恒 None（降级关键词检索）。
pub fn compute_embedding(_title: &str, _body: &str) -> Option<Vec<f32>> {
    None
}

// ---------------- 结果包装 ----------------

#[derive(Debug, Clone)]
pub struct CmdResult {
    pub value: Value,
    pub text: String,
}

fn require(value: Option<&str>, field: &str) -> Result<String, CliError> {
    match value {
        Some(v) if !v.is_empty() => Ok(v.to_string()),
        _ => Err(CliError(format!("{field} 必填"))),
    }
}

fn row0(rows: Vec<Vec<Value>>) -> Option<Vec<Value>> {
    rows.into_iter().next()
}

// ---------------- 子命令实现 ----------------

pub fn run_task_add(
    store: &mut dyn Store,
    task_id: &str,
    title: &str,
    owner: &str,
    ttl_hours: Option<f64>,
    ttl_at: Option<&str>,
    note: Option<&str>,
    now_epoch: i64,
) -> Result<CmdResult, CliError> {
    let task_id = require(Some(task_id), "--task-id")?;
    let ttl_at = when(now_epoch, None, ttl_hours, ttl_at, "--ttl-at")?;
    store.set_note(note)?;
    store.exec(
        "INSERT INTO shared_tasks (task_id, title, owner, note, created_at, ttl_at) \
         VALUES (%s, %s, %s, %s, %s::timestamptz, %s::timestamptz)",
        &[
            json!(task_id),
            json!(require(Some(title), "--title")?),
            json!(require(Some(owner), "--owner")?),
            json!(note.unwrap_or("")),
            json!(epoch_to_iso(now_epoch)),
            json!(ttl_at.clone().unwrap_or_default()),
        ],
        Fetch::None,
    )?;
    let task =
        get_task(store, &task_id, now_epoch)?.ok_or_else(|| CliError("任务写入后读不回".into()))?;
    let lines = vec![
        format!(
            "已建任务容器 {task_id}（{}）",
            task["title"].as_str().unwrap_or("")
        ),
        format!(
            "  负责人 {}；兜底 TTL {}",
            task["owner"].as_str().unwrap_or(""),
            if task["ttl_at"].is_null() {
                "未设（只能靠 task-end 收尾）".to_string()
            } else {
                fmt_time(task["ttl_at"].as_str())
            }
        ),
    ];
    Ok(CmdResult {
        value: json!({"ok": true, "task": task}),
        text: lines.join("\n"),
    })
}

pub fn run_task_end(
    store: &mut dyn Store,
    task_id: &str,
    status: &str,
    note: Option<&str>,
    now_epoch: i64,
) -> Result<CmdResult, CliError> {
    let task_id = require(Some(task_id), "--task-id")?;
    if !TASK_END_STATUSES.contains(&status) {
        return Err(CliError(format!(
            "--status 只能是 {} 之一",
            TASK_END_STATUSES.join("/")
        )));
    }
    store.set_note(note)?;
    let row = store.exec(
        "UPDATE shared_tasks SET status = %s, ended_at = %s::timestamptz, \
         note = COALESCE(%s, note) WHERE task_id = %s AND status = %s \
         RETURNING task_id",
        &[
            json!(status),
            json!(epoch_to_iso(now_epoch)),
            json!(note.unwrap_or("")),
            json!(task_id),
            json!("active"),
        ],
        Fetch::One,
    )?;
    if row0(row).is_none() {
        return Err(CliError(format!(
            "任务 {task_id} 不存在，或它已经结束了（结束过的任务不重复收尾）"
        )));
    }
    let task =
        get_task(store, &task_id, now_epoch)?.ok_or_else(|| CliError("任务读不回".into()))?;
    let pending = store.exec(
        "SELECT count(*) FROM shared_content WHERE task_id = %s AND status <> %s",
        &[json!(task_id), json!("destroyed")],
        Fetch::One,
    )?;
    let left = row0(pending)
        .and_then(|r| r.first().cloned())
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let lines = vec![
        format!("任务 {task_id} 已标记为 {}", cn(TASK_STATUS_CN, status)),
        format!("  这个任务下还有 {left} 条任务级共享内容没销毁。"),
        "  **任务结束不等于内容已销毁** —— 跑 `sweep --commit` 才真清：先 `sweep` 演练看清楚。"
            .into(),
    ];
    Ok(CmdResult {
        value: json!({"ok": true, "task": task, "pending": left}),
        text: lines.join("\n"),
    })
}

pub fn get_task(
    store: &mut dyn Store,
    task_id: &str,
    now_epoch: i64,
) -> Result<Option<Value>, CliError> {
    let rows = store.exec(
        "SELECT task_id, title, owner, status, note, created_at::text, ended_at::text, ttl_at::text, destroyed_at::text \
         FROM shared_tasks WHERE task_id = %s",
        &[json!(task_id)],
        Fetch::One,
    )?;
    Ok(row0(rows).map(|r| row_to_task(&r, now_epoch)))
}

pub fn get_content(
    store: &mut dyn Store,
    content_id: i64,
    now_epoch: i64,
) -> Result<Option<Value>, CliError> {
    let rows = store.exec(
        "SELECT content_id, scope, task_id, kind, title, body, tags, owner, audience, \
         sensitivity, status, expires_at::text, review_at::text, version, created_at::text, updated_at::text, \
         updated_by, destroyed_at::text, source_refs \
         FROM shared_content WHERE content_id = %s",
        &[json!(content_id)],
        Fetch::One,
    )?;
    Ok(row0(rows).map(|r| row_to_content(&r, now_epoch)))
}

pub fn get_revisions(
    store: &mut dyn Store,
    content_id: i64,
    limit: usize,
) -> Result<Vec<Value>, CliError> {
    let rows = store.exec(
        "SELECT rev_id, content_id, action, actor, changed, note, ts::text \
         FROM shared_content_revisions WHERE content_id = %s \
         ORDER BY ts ASC, rev_id ASC LIMIT %s",
        &[json!(content_id), json!(limit as i64)],
        Fetch::All,
    )?;
    Ok(rows.iter().map(|r| row_to_revision(r)).collect())
}

pub fn select_contents(
    store: &mut dyn Store,
    where_: &[String],
    params: &[String],
    limit: usize,
    now_epoch: i64,
) -> Result<Vec<Value>, CliError> {
    let mut sql = String::from(
        "SELECT content_id, scope, task_id, kind, title, body, tags, owner, audience, \
         sensitivity, status, expires_at::text, review_at::text, version, created_at::text, updated_at::text, \
         updated_by, destroyed_at::text, source_refs FROM shared_content",
    );
    if !where_.is_empty() {
        sql += " WHERE ";
        sql += &where_.join(" AND ");
    }
    sql += &format!(" ORDER BY updated_at DESC, content_id DESC LIMIT %s");
    let mut p: Vec<Value> = params.iter().map(|s| json!(s)).collect();
    p.push(json!(limit as i64));
    let rows = store.exec(&sql, &p, Fetch::All)?;
    Ok(rows.iter().map(|r| row_to_content(r, now_epoch)).collect())
}

fn body_text(
    body_file: Option<&str>,
    body: Option<&str>,
    required: bool,
) -> Result<Option<String>, CliError> {
    if let Some(path) = body_file {
        let txt = std::fs::read_to_string(path).map_err(|e| {
            CliError(format!(
                "读不到正文文件 {path}：{}",
                safe_err(&e.to_string())
            ))
        })?;
        return Ok(Some(txt));
    }
    if let Some(b) = body {
        return Ok(Some(b.to_string()));
    }
    if required {
        return Err(CliError(
            "正文必填：用 --body-file 指一个文件，或 --body 直接给一段".into(),
        ));
    }
    Ok(None)
}

pub fn run_put(
    store: &mut dyn Store,
    scope: &str,
    task_id: Option<&str>,
    kind: &str,
    title: &str,
    body_file: Option<&str>,
    body: Option<&str>,
    owner: &str,
    by: Option<&str>,
    tags: Option<&str>,
    audience: Option<&str>,
    sensitivity: &str,
    expires_in_days: Option<f64>,
    expires_in_hours: Option<f64>,
    expires_at: Option<&str>,
    review_in_days: Option<f64>,
    review_at: Option<&str>,
    source_refs: &[String],
    note: Option<&str>,
    now_epoch: i64,
) -> Result<CmdResult, CliError> {
    if !SCOPES.contains(&scope) {
        return Err(CliError(format!(
            "--scope 只能是 {} 之一",
            SCOPES.join("/")
        )));
    }
    if !SENSITIVITIES.contains(&sensitivity) {
        return Err(CliError(format!(
            "--sensitivity 只能是 {} 之一",
            SENSITIVITIES.join("/")
        )));
    }
    let title = require(Some(title), "--title")?;
    let owner = require(Some(owner), "--owner")?;
    let body = body_text(body_file, body, true)?;
    let mut expires_at = when(
        now_epoch,
        expires_in_days,
        expires_in_hours,
        expires_at,
        "--expires-at",
    )?;
    let review_at = when(now_epoch, review_in_days, None, review_at, "--review-at")?;

    let task_id = task_id.filter(|s| !s.is_empty());
    if scope == "task" {
        let task_id = task_id.ok_or_else(|| {
            CliError("任务级共享内容必须 --task-id：不挂任务就没有销毁条件".into())
        })?;
        let task = get_task(store, task_id, now_epoch)?
            .ok_or_else(|| CliError(format!("任务 {task_id} 不存在，先 task-add")))?;
        if task["status"] != "active" {
            return Err(CliError(format!(
                "任务 {task_id} 已经是 {}，不该再往里发共享内容",
                cn(TASK_STATUS_CN, task["status"].as_str().unwrap_or(""))
            )));
        }
        if expires_at.is_none() {
            expires_at = task["ttl_at"].as_str().map(|s| s.to_string());
        }
        if expires_at.is_none() {
            return Err(CliError(
                "任务级共享内容必须有过期时间：给 --expires-in-days，或者给任务设 --ttl-hours 让它继承".into(),
            ));
        }
    } else if task_id.is_some() {
        return Err(CliError(
            "永久级共享内容不许挂 --task-id：挂了就会被任务结束连坐销毁".into(),
        ));
    }

    let tags_list = split_list(tags).unwrap_or_default();
    let audience_list = split_list(audience).unwrap_or_else(|| vec!["all".to_string()]);
    let refs = parse_source_refs(source_refs);
    let actor = by.filter(|s| !s.is_empty()).unwrap_or(&owner).to_string();
    let embedding = compute_embedding(&title, body.as_deref().unwrap_or(""));

    store.set_note(note)?;
    let tags_json = json!(tags_list).to_string();
    let audience_json = json!(audience_list).to_string();
    let refs_json = json!(refs).to_string();
    let row = store.exec(
        "INSERT INTO shared_content (scope, task_id, kind, title, body, tags, owner, \
         audience, sensitivity, expires_at, review_at, created_at, updated_at, updated_by, \
         source_refs, embedding) VALUES (%s, %s, %s, %s, %s, %s::jsonb, %s, %s::jsonb, %s, \
         %s::timestamptz, %s::timestamptz, %s::timestamptz, %s::timestamptz, %s, %s::jsonb, \
         %s::vector) RETURNING content_id",
        &[
            json!(scope),
            task_id.map(|t| json!(t)).unwrap_or(Value::Null),
            json!(kind),
            json!(title),
            json!(body.clone().unwrap_or_default()),
            json!(tags_json),
            json!(owner),
            json!(audience_json),
            json!(sensitivity),
            json!(expires_at.clone().unwrap_or_default()),
            json!(review_at.clone().unwrap_or_default()),
            json!(epoch_to_iso(now_epoch)),
            json!(epoch_to_iso(now_epoch)),
            json!(actor),
            json!(refs_json),
            json!(vector_literal_opt(&embedding)),
        ],
        Fetch::One,
    )?;
    let new_id = row0(row)
        .and_then(|r| r.first().cloned())
        .and_then(|v| v.as_i64())
        .ok_or_else(|| CliError("写入没有返回 content_id".into()))?;
    let item =
        get_content(store, new_id, now_epoch)?.ok_or_else(|| CliError("写入后读不回".into()))?;
    let audience_str: Vec<String> = item["audience"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    let lines = vec![
        format!(
            "已发布 #{} [{}] {}",
            new_id,
            cn(SCOPE_CN, scope),
            item["title"].as_str().unwrap_or("")
        ),
        format!(
            "  维护人 {}；受众 {}；敏感度 {}；向量 {}",
            item["owner"].as_str().unwrap_or(""),
            audience_str.join("/"),
            cn(SENSITIVITY_CN, item["sensitivity"].as_str().unwrap_or("")),
            if embedding.is_some() {
                "已生成".to_string()
            } else {
                "未生成（检索退回关键词，可事后补）".to_string()
            }
        ),
        format!(
            "  过期 {}；复审 {}",
            if item["expires_at"].is_null() {
                "不过期".to_string()
            } else {
                fmt_time(item["expires_at"].as_str())
            },
            if item["review_at"].is_null() {
                "不复审".to_string()
            } else {
                fmt_time(item["review_at"].as_str())
            }
        ),
    ];
    Ok(CmdResult {
        value: json!({"ok": true, "content": item}),
        text: lines.join("\n"),
    })
}

/// pgvector 文本字面量；None -> SQL NULL 占位（用空串 + IS NULL 判断不了，传 "NULL" 字符串
/// 会被 bin 翻译成 NULL 绑定 —— 这里 None 返回空串，bin 用 `if v.is_empty() → Option::None`）。
pub fn vector_literal_opt(v: &Option<Vec<f32>>) -> String {
    match v {
        None => String::new(),
        Some(vec) => {
            let parts: Vec<String> = vec.iter().map(|x| format!("{x}")).collect();
            format!("[{}]", parts.join(","))
        }
    }
}

pub fn run_update(
    store: &mut dyn Store,
    content_id: i64,
    by: &str,
    note: Option<&str>,
    title: Option<&str>,
    body_file: Option<&str>,
    body: Option<&str>,
    tags: Option<&str>,
    audience: Option<&str>,
    owner: Option<&str>,
    kind: Option<&str>,
    sensitivity: Option<&str>,
    status: Option<&str>,
    expires_at: Option<&str>,
    expires_in_days: Option<f64>,
    review_at: Option<&str>,
    review_in_days: Option<f64>,
    source_refs: &[String],
    now_epoch: i64,
) -> Result<CmdResult, CliError> {
    let actor = require(Some(by), "--by")?;
    let before = get_content(store, content_id, now_epoch)?
        .ok_or_else(|| CliError(format!("没有 #{content_id} 这条共享内容")))?;
    if before["status"] == "destroyed" {
        return Err(CliError(format!(
            "#{content_id} 已经销毁了，正文已清空，不能再改（要重发请新开一条）"
        )));
    }
    let mut sets: Vec<String> = Vec::new();
    let mut params: Vec<Value> = Vec::new();
    let mut changed: Vec<String> = Vec::new();

    let body = body_text(body_file, body, false)?;
    if let Some(t) = title {
        sets.push("title = %s".into());
        params.push(json!(t));
        changed.push("title".into());
    }
    if let Some(b) = &body {
        sets.push("body = %s".into());
        params.push(json!(b));
        changed.push("body".into());
    }
    if let Some(t) = tags {
        sets.push("tags = %s::jsonb".into());
        params.push(json!(
            json!(split_list(Some(t)).unwrap_or_default()).to_string()
        ));
        changed.push("tags".into());
    }
    if let Some(a) = audience {
        sets.push("audience = %s::jsonb".into());
        params.push(json!(json!(
            split_list(Some(a)).unwrap_or_else(|| vec!["all".into()])
        )
        .to_string()));
        changed.push("audience".into());
    }
    if let Some(o) = owner {
        sets.push("owner = %s".into());
        params.push(json!(o));
        changed.push("owner".into());
    }
    if let Some(k) = kind {
        sets.push("kind = %s".into());
        params.push(json!(k));
        changed.push("kind".into());
    }
    if let Some(s) = sensitivity {
        if !SENSITIVITIES.contains(&s) {
            return Err(CliError(format!(
                "--sensitivity 只能是 {} 之一",
                SENSITIVITIES.join("/")
            )));
        }
        sets.push("sensitivity = %s".into());
        params.push(json!(s));
        changed.push("sensitivity".into());
    }
    if let Some(s) = status {
        if !UPDATABLE_STATUSES.contains(&s) {
            return Err(CliError(format!(
                "--status 只能是 {} 之一（销毁请用 destroy 子命令，它会一并清正文）",
                UPDATABLE_STATUSES.join("/")
            )));
        }
        sets.push("status = %s".into());
        params.push(json!(s));
        changed.push("status".into());
    }
    if !source_refs.is_empty() {
        sets.push("source_refs = %s::jsonb".into());
        params.push(json!(json!(parse_source_refs(source_refs)).to_string()));
        changed.push("source_refs".into());
    }
    let exp = when(now_epoch, expires_in_days, None, expires_at, "--expires-at")?;
    if let Some(e) = exp {
        sets.push("expires_at = %s::timestamptz".into());
        params.push(json!(e));
        changed.push("expires_at".into());
    }
    let rev = when(now_epoch, review_in_days, None, review_at, "--review-at")?;
    if let Some(r) = rev {
        sets.push("review_at = %s::timestamptz".into());
        params.push(json!(r));
        changed.push("review_at".into());
    }
    if sets.is_empty() {
        return Err(CliError(
            "没给任何要改的字段：--title/--body-file/--tags/--status/... 至少给一个".into(),
        ));
    }
    // 正文或标题动了就重算向量（embedder 未移植 → 置 NULL，检索退回关键词）
    if title.is_some() || body.is_some() {
        sets.push("embedding = %s::vector".into());
        params.push(json!(""));
    }
    sets.push("version = version + 1".into());
    sets.push("updated_at = %s::timestamptz".into());
    params.push(json!(epoch_to_iso(now_epoch)));
    sets.push("updated_by = %s".into());
    params.push(json!(actor));

    store.set_note(note)?;
    params.push(json!(content_id));
    params.push(json!("destroyed"));
    let row = store.exec(
        &format!(
            "UPDATE shared_content SET {} WHERE content_id = %s AND status <> %s RETURNING content_id",
            sets.join(", ")
        ),
        &params,
        Fetch::One,
    )?;
    if row0(row).is_none() {
        return Err(CliError(format!(
            "改不动 #{content_id}（可能刚刚被销毁了）"
        )));
    }
    let after =
        get_content(store, content_id, now_epoch)?.ok_or_else(|| CliError("改后读不回".into()))?;
    let revisions = get_revisions(store, content_id, 100)?;
    let before_v = before["version"].as_i64().unwrap_or(0);
    let after_v = after["version"].as_i64().unwrap_or(0);
    let lines = vec![
        format!(
            "已更新 #{content_id}：{}（v{before_v} -> v{after_v}）",
            after["title"].as_str().unwrap_or("")
        ),
        format!(
            "  改了：{}；操作人 {actor}；理由 {}",
            changed.join("、"),
            note.unwrap_or("（没写）")
        ),
        format!(
            "  留痕共 {} 条（get --id {content_id} 看全）",
            revisions.len()
        ),
    ];
    Ok(CmdResult {
        value: json!({"ok": true, "content": after, "changed": changed, "revisions": revisions}),
        text: lines.join("\n"),
    })
}

/// revoke / destroy 共用的一次状态推进。**销毁连正文一起清，行留着。**
fn status_change(
    store: &mut dyn Store,
    content_id: i64,
    status: &str,
    actor: &str,
    note: Option<&str>,
    now_epoch: i64,
    clear_body: bool,
) -> Result<Value, CliError> {
    let before = get_content(store, content_id, now_epoch)?
        .ok_or_else(|| CliError(format!("没有 #{content_id} 这条共享内容")))?;
    if before["status"] == status {
        return Err(CliError(format!(
            "#{content_id} 已经是 {} 了，不重复操作",
            cn(STATUS_CN, status)
        )));
    }
    if before["status"] == "destroyed" {
        return Err(CliError(format!("#{content_id} 已经销毁，没什么可再撤的")));
    }
    let mut sets: Vec<String> = vec![
        "status = %s".into(),
        "updated_at = %s::timestamptz".into(),
        "updated_by = %s".into(),
        "version = version + 1".into(),
    ];
    let mut params: Vec<Value> = vec![json!(status), json!(epoch_to_iso(now_epoch)), json!(actor)];
    if clear_body {
        sets.insert(1, "body = %s".into());
        params.insert(1, json!(""));
        sets.push("destroyed_at = %s::timestamptz".into());
        params.push(json!(epoch_to_iso(now_epoch)));
        sets.push("embedding = NULL".into());
    }
    store.set_note(note)?;
    params.push(json!(content_id));
    params.push(json!("destroyed"));
    let row = store.exec(
        &format!(
            "UPDATE shared_content SET {} WHERE content_id = %s AND status <> %s RETURNING content_id",
            sets.join(", ")
        ),
        &params,
        Fetch::One,
    )?;
    if row0(row).is_none() {
        return Err(CliError(format!(
            "动不了 #{content_id}（可能刚刚被别人销毁了）"
        )));
    }
    get_content(store, content_id, now_epoch)?.ok_or_else(|| CliError("状态推进后读不回".into()))
}

pub fn run_revoke(
    store: &mut dyn Store,
    content_id: i64,
    by: &str,
    note: Option<&str>,
    now_epoch: i64,
) -> Result<CmdResult, CliError> {
    let actor = require(Some(by), "--by")?;
    let item = status_change(store, content_id, "revoked", &actor, note, now_epoch, false)?;
    let lines = vec![
        format!("已撤下 #{content_id}：{}", item["title"].as_str().unwrap_or("")),
        format!(
            "  默认列表与只读 API 不再返回它（正文还在，`get --id {content_id}` 查得到）；操作人 {actor}；理由 {}",
            note.unwrap_or("（没写）")
        ),
    ];
    Ok(CmdResult {
        value: json!({"ok": true, "content": item}),
        text: lines.join("\n"),
    })
}

pub fn run_destroy(
    store: &mut dyn Store,
    content_id: i64,
    by: &str,
    note: Option<&str>,
    now_epoch: i64,
) -> Result<CmdResult, CliError> {
    let actor = require(Some(by), "--by")?;
    let item = status_change(
        store,
        content_id,
        "destroyed",
        &actor,
        note,
        now_epoch,
        true,
    )?;
    let lines = vec![
        format!("已销毁 #{content_id}：{}", item["title"].as_str().unwrap_or("")),
        format!(
            "  正文已清空、向量已清、destroyed_at={}；行与留痕保留（谁销毁的查得到）。操作人 {actor}；理由 {}",
            fmt_time(item["destroyed_at"].as_str()),
            note.unwrap_or("（没写）")
        ),
    ];
    Ok(CmdResult {
        value: json!({"ok": true, "content": item}),
        text: lines.join("\n"),
    })
}

pub fn run_get(
    store: &mut dyn Store,
    content_id: i64,
    now_epoch: i64,
) -> Result<CmdResult, CliError> {
    let item = get_content(store, content_id, now_epoch)?
        .ok_or_else(|| CliError(format!("没有 #{content_id} 这条共享内容")))?;
    let revisions = get_revisions(store, content_id, 100)?;
    let head = vec![
        format!(
            "#{content_id} [{}] {}",
            cn(SCOPE_CN, item["scope"].as_str().unwrap_or("")),
            item["title"].as_str().unwrap_or("")
        ),
        format!(
            "  状态 {}{}；v{}；维护人 {}；受众 {}；敏感度 {}",
            cn(STATUS_CN, item["status"].as_str().unwrap_or("")),
            if item["expired"].as_bool().unwrap_or(false) && item["status"] == "active" {
                "（已过期）"
            } else {
                ""
            },
            item["version"],
            item["owner"].as_str().unwrap_or(""),
            item["audience"]
                .as_array()
                .map(|a| a
                    .iter()
                    .filter_map(|x| x.as_str())
                    .collect::<Vec<_>>()
                    .join("/"))
                .unwrap_or_default(),
            cn(SENSITIVITY_CN, item["sensitivity"].as_str().unwrap_or("")),
        ),
        format!(
            "  任务 {}；标签 {}；来源 {}",
            item["task_id"].as_str().unwrap_or("-"),
            item["tags"]
                .as_array()
                .map(|a| a
                    .iter()
                    .filter_map(|x| x.as_str())
                    .collect::<Vec<_>>()
                    .join("、"))
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "-".into()),
            json!(item["source_refs"]).to_string(),
        ),
        format!(
            "  过期 {}；复审 {}；更新 {} by {}",
            if item["expires_at"].is_null() {
                "不过期".into()
            } else {
                fmt_time(item["expires_at"].as_str())
            },
            if item["review_at"].is_null() {
                "不复审".into()
            } else {
                fmt_time(item["review_at"].as_str())
            },
            fmt_time(item["updated_at"].as_str()),
            item["updated_by"].as_str().unwrap_or("-"),
        ),
        String::new(),
        "正文：".into(),
        item["body"]
            .as_str()
            .filter(|s| !s.is_empty())
            .unwrap_or("（已销毁，正文不留残）")
            .to_string(),
        String::new(),
        "改动历史（谁、何时、改了哪些字段）：".into(),
    ];
    let rows: Vec<Vec<String>> = revisions
        .iter()
        .map(|r| {
            let changed: Vec<String> = {
                let mut keys: Vec<String> = r["changed"]
                    .as_object()
                    .map(|o| o.keys().cloned().collect())
                    .unwrap_or_default();
                keys.sort();
                keys
            };
            vec![
                r["rev_id"].to_string(),
                cn(ACTION_CN, r["action"].as_str().unwrap_or("")),
                r["actor"].as_str().unwrap_or("").to_string(),
                fmt_time(r["ts"].as_str()),
                if changed.is_empty() {
                    "-".into()
                } else {
                    changed.join("、")
                },
                one_line(r["note"].as_str(), 24),
            ]
        })
        .collect();
    let text = format!(
        "{}\n{}",
        head.join("\n"),
        render_table(&["#", "动作", "操作人", "时间", "字段", "理由"], &rows)
    );
    Ok(CmdResult {
        value: json!({"ok": true, "content": item, "revisions": revisions}),
        text,
    })
}

pub fn content_table(items: &[Value]) -> String {
    let rows: Vec<Vec<String>> = items
        .iter()
        .map(|it| {
            vec![
                it["content_id"].to_string(),
                cn(SCOPE_CN, it["scope"].as_str().unwrap_or("")),
                it["kind"].as_str().unwrap_or("").to_string(),
                one_line(it["title"].as_str(), 28),
                it["owner"].as_str().unwrap_or("").to_string(),
                format!(
                    "{}{}",
                    cn(STATUS_CN, it["status"].as_str().unwrap_or("")),
                    if it["expired"].as_bool().unwrap_or(false) && it["status"] == "active" {
                        "·过期"
                    } else {
                        ""
                    }
                ),
                if it["expires_at"].is_null() {
                    "不过期".into()
                } else {
                    fmt_time(it["expires_at"].as_str())
                },
                {
                    let t: Vec<String> = it["tags"]
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                                .collect()
                        })
                        .unwrap_or_default();
                    if t.is_empty() {
                        "-".into()
                    } else {
                        t.join("、")
                    }
                },
            ]
        })
        .collect();
    render_table(
        &[
            "ID",
            "范围",
            "类型",
            "标题",
            "维护人",
            "状态",
            "过期",
            "标签",
        ],
        &rows,
    )
}

pub fn run_list(
    store: &mut dyn Store,
    f: &ContentFilter,
    limit: usize,
    now_epoch: i64,
) -> Result<CmdResult, CliError> {
    if let Some(s) = &f.status {
        if !CONTENT_STATUSES.contains(&s.as_str()) {
            return Err(CliError(format!(
                "--status 只能是 {} 之一",
                CONTENT_STATUSES.join("/")
            )));
        }
    }
    if let Some(s) = &f.scope {
        if !SCOPES.contains(&s.as_str()) {
            return Err(CliError(format!(
                "--scope 只能是 {} 之一",
                SCOPES.join("/")
            )));
        }
    }
    let now_iso = epoch_to_iso(now_epoch);
    let (where_, params) = content_filters(f, &now_iso);
    let items = select_contents(store, &where_, &params, limit.min(MAX_LIMIT), now_epoch)?;
    let text = format!(
        "共享内容 {} 条（截至 {}）\n{}",
        items.len(),
        fmt_time(Some(&now_iso)),
        content_table(&items)
    );
    Ok(CmdResult {
        value: json!({"ok": true, "count": items.len(), "contents": items}),
        text,
    })
}

pub fn run_search(
    store: &mut dyn Store,
    q: &str,
    semantic: bool,
    f: &ContentFilter,
    limit: usize,
    now_epoch: i64,
) -> Result<CmdResult, CliError> {
    let q = require(Some(q), "--q")?;
    let limit = limit.min(MAX_LIMIT);
    let now_iso = epoch_to_iso(now_epoch);
    let (where_, params) = content_filters(f, &now_iso);
    let vector = if semantic {
        compute_embedding(&q, "")
    } else {
        None
    };
    let (mode, items, note) = if let Some(_v) = vector {
        // 语义路径（Rust 侧 embedder 未移植 → 到不了这里；留接口）
        unreachable!("embedder 未移植，semantic 路径恒走关键词降级")
    } else {
        let pattern = like_pattern(&q);
        let mut where2 = where_.clone();
        where2.push("(title ILIKE %s OR body ILIKE %s OR tags @> %s::jsonb)".into());
        let mut p: Vec<Value> = params.iter().map(|s| json!(s)).collect();
        p.push(json!(pattern.clone()));
        p.push(json!(pattern));
        p.push(json!(json!([q]).to_string()));
        p.push(json!(limit as i64));
        let sql = format!(
            "SELECT content_id, scope, task_id, kind, title, body, tags, owner, audience, \
             sensitivity, status, expires_at::text, review_at::text, version, created_at::text, updated_at::text, \
             updated_by, destroyed_at::text, source_refs FROM shared_content WHERE {} \
             ORDER BY updated_at DESC, content_id DESC LIMIT %s",
            where2.join(" AND ")
        );
        let rows = store.exec(&sql, &p, Fetch::All)?;
        let mut items: Vec<Value> = rows.iter().map(|r| row_to_content(r, now_epoch)).collect();
        for it in &mut items {
            if let Some(o) = it.as_object_mut() {
                o.insert("score".into(), Value::Null);
            }
        }
        (
            "keyword",
            items,
            "关键词检索（要语义请加 --semantic；加了还是这行，就是嵌入模型不可用，已降级）",
        )
    };
    let text = format!(
        "{}：q={q:?} 命中 {} 条\n{}",
        note,
        items.len(),
        content_table(&items)
    );
    Ok(CmdResult {
        value: json!({"ok": true, "mode": mode, "query": q, "count": items.len(), "contents": items}),
        text,
    })
}

pub fn run_tasks(
    store: &mut dyn Store,
    status: Option<&str>,
    limit: usize,
    now_epoch: i64,
) -> Result<CmdResult, CliError> {
    let mut where_: Vec<String> = Vec::new();
    let mut params: Vec<Value> = Vec::new();
    if let Some(s) = status.filter(|s| !s.is_empty()) {
        if !TASK_STATUSES.contains(&s) {
            return Err(CliError(format!(
                "--status 只能是 {} 之一",
                TASK_STATUSES.join("/")
            )));
        }
        where_.push("status = %s".into());
        params.push(json!(s));
    }
    let mut sql = String::from(
        "SELECT task_id, title, owner, status, note, created_at::text, ended_at::text, ttl_at::text, destroyed_at::text \
         FROM shared_tasks",
    );
    if !where_.is_empty() {
        sql += " WHERE ";
        sql += &where_.join(" AND ");
    }
    sql += " ORDER BY created_at DESC, task_id DESC LIMIT %s";
    params.push(json!(limit.min(MAX_LIMIT) as i64));
    let rows = store.exec(&sql, &params, Fetch::All)?;
    let tasks: Vec<Value> = rows.iter().map(|r| row_to_task(r, now_epoch)).collect();
    let trows: Vec<Vec<String>> = tasks
        .iter()
        .map(|t| {
            vec![
                t["task_id"].as_str().unwrap_or("").to_string(),
                one_line(t["title"].as_str(), 24),
                t["owner"].as_str().unwrap_or("").to_string(),
                format!(
                    "{}{}",
                    cn(TASK_STATUS_CN, t["status"].as_str().unwrap_or("")),
                    if t["ttl_expired"].as_bool().unwrap_or(false) && t["status"] == "active" {
                        "·TTL已到"
                    } else {
                        ""
                    }
                ),
                if t["ttl_at"].is_null() {
                    "-".into()
                } else {
                    fmt_time(t["ttl_at"].as_str())
                },
                if t["ended_at"].is_null() {
                    "-".into()
                } else {
                    fmt_time(t["ended_at"].as_str())
                },
                if t["destroyed_at"].is_null() {
                    "-".into()
                } else {
                    fmt_time(t["destroyed_at"].as_str())
                },
            ]
        })
        .collect();
    let table = render_table(
        &["任务", "标题", "负责人", "状态", "TTL", "结束", "已清空"],
        &trows,
    );
    let n = tasks.len();
    Ok(CmdResult {
        value: json!({"ok": true, "count": n, "tasks": tasks}),
        text: format!("任务容器 {n} 个\n{table}"),
    })
}

// ---------------- sweep ----------------

/// 只读地把"该销毁/该过期/该复审"三份清单查出来。**一个字都不写。**
pub fn sweep_plan(store: &mut dyn Store, now_epoch: i64, limit: usize) -> Result<Value, CliError> {
    let now_iso = epoch_to_iso(now_epoch);
    let rows = store.exec(
        "SELECT task_id, title, owner, status, note, created_at::text, ended_at::text, ttl_at::text, destroyed_at::text \
         FROM shared_tasks WHERE destroyed_at IS NULL \
         AND (status IN (%s, %s) OR (ttl_at IS NOT NULL AND ttl_at < %s::timestamptz)) \
         ORDER BY created_at ASC LIMIT %s",
        &[
            json!("done"),
            json!("aborted"),
            json!(now_iso),
            json!(limit as i64),
        ],
        Fetch::All,
    )?;
    let dead_tasks: Vec<Value> = rows.iter().map(|r| row_to_task(r, now_epoch)).collect();

    let mut destroy: Vec<Value> = Vec::new();
    for task in &dead_tasks {
        let rows = store.exec(
            "SELECT content_id, scope, task_id, kind, title, body, tags, owner, audience, \
             sensitivity, status, expires_at::text, review_at::text, version, created_at::text, updated_at::text, \
             updated_by, destroyed_at::text, source_refs FROM shared_content \
             WHERE task_id = %s AND status <> %s ORDER BY content_id ASC LIMIT %s",
            &[
                json!(task["task_id"].clone()),
                json!("destroyed"),
                json!(limit as i64),
            ],
            Fetch::All,
        )?;
        for r in rows {
            let mut item = row_to_content(&r, now_epoch);
            let st = task["status"].as_str().unwrap_or("");
            item["reason"] = json!(if TASK_END_STATUSES.contains(&st) {
                format!("任务{}", cn(TASK_STATUS_CN, st))
            } else {
                "任务 TTL 到点".to_string()
            });
            destroy.push(item);
        }
    }

    let picked: HashSet<i64> = destroy
        .iter()
        .filter_map(|it| it["content_id"].as_i64())
        .collect();

    let rows = store.exec(
        "SELECT content_id, scope, task_id, kind, title, body, tags, owner, audience, \
         sensitivity, status, expires_at::text, review_at::text, version, created_at::text, updated_at::text, \
         updated_by, destroyed_at::text, source_refs FROM shared_content \
         WHERE scope = %s AND status = %s AND expires_at IS NOT NULL \
         AND expires_at < %s::timestamptz ORDER BY content_id ASC LIMIT %s",
        &[
            json!("task"),
            json!("active"),
            json!(now_iso),
            json!(limit as i64),
        ],
        Fetch::All,
    )?;
    let mut expire: Vec<Value> = Vec::new();
    for r in rows {
        let item = row_to_content(&r, now_epoch);
        let id = item["content_id"].as_i64().unwrap_or(0);
        if !picked.contains(&id) {
            let mut item = item;
            item["reason"] = json!("任务级内容已过期");
            destroy.push(item);
        }
    }

    let rows = store.exec(
        "SELECT content_id, scope, task_id, kind, title, body, tags, owner, audience, \
         sensitivity, status, expires_at::text, review_at::text, version, created_at::text, updated_at::text, \
         updated_by, destroyed_at::text, source_refs FROM shared_content \
         WHERE scope = %s AND status = %s AND expires_at IS NOT NULL \
         AND expires_at < %s::timestamptz ORDER BY content_id ASC LIMIT %s",
        &[
            json!("permanent"),
            json!("active"),
            json!(now_iso),
            json!(limit as i64),
        ],
        Fetch::All,
    )?;
    for r in rows {
        expire.push(row_to_content(&r, now_epoch));
    }

    let rows = store.exec(
        "SELECT content_id, scope, task_id, kind, title, body, tags, owner, audience, \
         sensitivity, status, expires_at::text, review_at::text, version, created_at::text, updated_at::text, \
         updated_by, destroyed_at::text, source_refs FROM shared_content \
         WHERE status = %s AND review_at IS NOT NULL AND review_at < %s::timestamptz \
         ORDER BY review_at ASC LIMIT %s",
        &[json!("active"), json!(now_iso), json!(limit as i64)],
        Fetch::All,
    )?;
    let reviews: Vec<Value> = rows.iter().map(|r| row_to_content(r, now_epoch)).collect();

    Ok(json!({
        "tasks": dead_tasks,
        "destroy": destroy,
        "expire": expire,
        "review": reviews,
    }))
}

pub fn run_sweep(
    store: &mut dyn Store,
    commit: bool,
    by: Option<&str>,
    limit: usize,
    now_epoch: i64,
) -> Result<CmdResult, CliError> {
    let plan = sweep_plan(store, now_epoch, limit.min(MAX_LIMIT))?;
    let actor = by.filter(|s| !s.is_empty()).unwrap_or("sweep");
    let n_destroy = plan["destroy"].as_array().map(|a| a.len()).unwrap_or(0);
    let n_expire = plan["expire"].as_array().map(|a| a.len()).unwrap_or(0);
    let n_review = plan["review"].as_array().map(|a| a.len()).unwrap_or(0);

    if commit {
        for item in plan["destroy"].as_array().cloned().unwrap_or_default() {
            let reason = item["reason"].as_str().unwrap_or("到销毁条件");
            store.set_note(Some(&format!("sweep：{reason}，自动销毁")))?;
            store.exec(
                "UPDATE shared_content SET status = %s, body = %s, destroyed_at = %s::timestamptz, \
                 embedding = NULL, updated_at = %s::timestamptz, updated_by = %s, \
                 version = version + 1 WHERE content_id = %s AND status <> %s",
                &[
                    json!("destroyed"),
                    json!(""),
                    json!(epoch_to_iso(now_epoch)),
                    json!(epoch_to_iso(now_epoch)),
                    json!(actor),
                    json!(item["content_id"].clone()),
                    json!("destroyed"),
                ],
                Fetch::None,
            )?;
        }
        for item in plan["expire"].as_array().cloned().unwrap_or_default() {
            store.set_note(Some("sweep：永久级内容已过期，标记 expired（正文保留）"))?;
            store.exec(
                "UPDATE shared_content SET status = %s, updated_at = %s::timestamptz, \
                 updated_by = %s, version = version + 1 WHERE content_id = %s AND status = %s",
                &[
                    json!("expired"),
                    json!(epoch_to_iso(now_epoch)),
                    json!(actor),
                    json!(item["content_id"].clone()),
                    json!("active"),
                ],
                Fetch::None,
            )?;
        }
        for task in plan["tasks"].as_array().cloned().unwrap_or_default() {
            store.set_note(Some("sweep：这个任务下的共享内容已清空"))?;
            store.exec(
                "UPDATE shared_tasks SET destroyed_at = %s::timestamptz \
                 WHERE task_id = %s AND destroyed_at IS NULL",
                &[
                    json!(epoch_to_iso(now_epoch)),
                    json!(task["task_id"].clone()),
                ],
                Fetch::None,
            )?;
        }
        store.set_note(None)?;
    }

    let head = format!(
        "【过期扫描】截至 {} —— {}",
        fmt_time(Some(&epoch_to_iso(now_epoch))),
        if commit {
            "**已落库**"
        } else {
            "演练（dry-run，一个字节都没写）"
        }
    );
    let mut blocks: Vec<String> = vec![
        head,
        String::new(),
        format!("① 该销毁 {n_destroy} 条（任务结束/TTL 到点/任务级内容过期）："),
        render_table(
            &["ID", "范围", "标题", "任务", "原因"],
            &plan["destroy"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .iter()
                .map(|it| {
                    vec![
                        it["content_id"].to_string(),
                        cn(SCOPE_CN, it["scope"].as_str().unwrap_or("")),
                        one_line(it["title"].as_str(), 24),
                        it["task_id"].as_str().unwrap_or("-").to_string(),
                        it["reason"].as_str().unwrap_or("").to_string(),
                    ]
                })
                .collect::<Vec<_>>(),
        ),
        String::new(),
        format!("② 该标过期 {n_expire} 条（永久级，正文保留）："),
        render_table(
            &["ID", "标题", "维护人", "过期时间"],
            &plan["expire"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .iter()
                .map(|it| {
                    vec![
                        it["content_id"].to_string(),
                        one_line(it["title"].as_str(), 24),
                        it["owner"].as_str().unwrap_or("").to_string(),
                        fmt_time(it["expires_at"].as_str()),
                    ]
                })
                .collect::<Vec<_>>(),
        ),
        String::new(),
        format!("③ 该复审 {n_review} 条（只提醒，不动数据）："),
        render_table(
            &["ID", "标题", "维护人", "复审时间"],
            &plan["review"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .iter()
                .map(|it| {
                    vec![
                        it["content_id"].to_string(),
                        one_line(it["title"].as_str(), 24),
                        it["owner"].as_str().unwrap_or("").to_string(),
                        fmt_time(it["review_at"].as_str()),
                    ]
                })
                .collect::<Vec<_>>(),
        ),
    ];
    if !commit && (n_destroy > 0 || n_expire > 0) {
        blocks.push(String::new());
        blocks.push("以上都没有执行。确认无误后加 --commit 才会真的落库。".into());
    }
    Ok(CmdResult {
        value: json!({"ok": true, "committed": commit, "destroy": plan["destroy"],
                      "expire": plan["expire"], "review": plan["review"], "tasks": plan["tasks"]}),
        text: blocks.join("\n"),
    })
}

// ---------------- X 层时间解析转发 ----------------

pub fn hacs_x_identity_parse_epoch(s: &str) -> Option<i64> {
    hacs_x_identity::view_auth::parse_ts(&normalize_pg_tz(s))
}

/// PG `timestamptz::text` 的时区偏移可能带成 `+08`（无分钟），补成 `+08:00`。
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

// ---------------- MemoryStore（单测替身，模拟 PG 触发器留痕） ----------------

#[derive(Default)]
pub struct MemoryStore {
    pub rows: Vec<Map<String, Value>>,  // shared_content
    pub tasks: Vec<Map<String, Value>>, // shared_tasks
    pub revisions: Vec<Map<String, Value>>,
    pub note: Option<String>,
    next_id: i64,
}

impl MemoryStore {
    fn get_row(&self, id: i64) -> Option<&Map<String, Value>> {
        self.rows
            .iter()
            .find(|r| r["content_id"].as_i64() == Some(id))
    }
}

impl Store for MemoryStore {
    fn exec(
        &mut self,
        sql: &str,
        params: &[Value],
        _fetch: Fetch,
    ) -> Result<Vec<Vec<Value>>, CliError> {
        let sql = sql.replace("%s", "{}");
        let mut it = params.iter();
        let filled: String = {
            let mut out = String::new();
            let mut rest = sql.as_str();
            loop {
                match rest.find("{}") {
                    Some(p) => {
                        out.push_str(&rest[..p]);
                        let v = it.next().cloned().unwrap_or(json!(""));
                        // ::cast 后缀已在原 SQL 中，这里替换 {} 后跟 cast 无意义，仅替值
                        match &v {
                            Value::String(s) => out.push_str(s),
                            other => out.push_str(&other.to_string()),
                        }
                        rest = &rest[p + 2..];
                    }
                    None => {
                        out.push_str(rest);
                        break;
                    }
                }
            }
            out
        };
        let _ = filled;
        // MemoryStore 不做真 SQL：只处理本文件用到的几种模式（按前缀分派）
        let q = sql.trim().to_string();
        if q.starts_with("INSERT INTO shared_tasks") {
            let t = params;
            self.tasks.push(json_obj_from(
                &[
                    ("task_id", &t[0]),
                    ("title", &t[1]),
                    ("owner", &t[2]),
                    ("note", &t[3]),
                    ("created_at", &t[4]),
                    ("ttl_at", &t[5]),
                ],
                &[
                    ("status", json!("active")),
                    ("ended_at", Value::Null),
                    ("destroyed_at", Value::Null),
                ],
            ));
            return Ok(Vec::new());
        }
        if q.starts_with("INSERT INTO shared_content") {
            self.next_id += 1;
            let t = params;
            let mut o = json_obj_from(
                &[
                    ("scope", &t[0]),
                    ("task_id", &t[1]),
                    ("kind", &t[2]),
                    ("title", &t[3]),
                    ("body", &t[4]),
                    ("owner", &t[6]),
                    ("sensitivity", &t[8]),
                    ("expires_at", &t[9]),
                    ("review_at", &t[10]),
                    ("created_at", &t[11]),
                    ("updated_at", &t[12]),
                    ("updated_by", &t[13]),
                ],
                &[],
            );
            o.insert("content_id".into(), json!(self.next_id));
            o.insert("tags".into(), parse_json_or(&t[5], json!([])));
            o.insert("audience".into(), parse_json_or(&t[7], json!(["all"])));
            o.insert("status".into(), json!("active"));
            o.insert("version".into(), json!(1));
            o.insert("destroyed_at".into(), Value::Null);
            o.insert("source_refs".into(), parse_json_or(&t[14], json!([])));
            self.rows.push(o);
            // create 留痕
            self.revisions.push(json_obj_from(
                &[
                    ("rev_id", &json!(self.revisions.len() + 1)),
                    ("content_id", &json!(self.next_id)),
                    ("action", &json!("create")),
                    (
                        "actor",
                        &json!(if t[13].as_str().unwrap_or("").is_empty() {
                            t[6].as_str().unwrap_or("")
                        } else {
                            t[13].as_str().unwrap_or("")
                        }),
                    ),
                    ("changed", &json!({})),
                    ("note", &json!(self.note.clone().unwrap_or_default())),
                    ("ts", &t[11]),
                ],
                &[],
            ));
            return Ok(vec![vec![json!(self.next_id)]]);
        }
        if q.starts_with("UPDATE shared_tasks SET destroyed_at") {
            let tid = params[1].as_str().unwrap_or("");
            for t in &mut self.tasks {
                if t["task_id"].as_str() == Some(tid) && t["destroyed_at"].is_null() {
                    t["destroyed_at"] = params[0].clone();
                }
            }
            return Ok(Vec::new());
        }
        if q.starts_with("UPDATE shared_tasks SET status") {
            let tid = params[3].as_str().unwrap_or("");
            for t in &mut self.tasks {
                if t["task_id"].as_str() == Some(tid) && t["status"] == "active" {
                    t["status"] = params[0].clone();
                    t["ended_at"] = params[1].clone();
                    if !params[2].as_str().unwrap_or("").is_empty() {
                        t["note"] = params[2].clone();
                    }
                    return Ok(vec![vec![json!(tid)]]);
                }
            }
            return Ok(Vec::new());
        }
        if q.starts_with("SELECT count(*) FROM shared_content") {
            let tid = params[0].as_str().unwrap_or("");
            let n = self
                .rows
                .iter()
                .filter(|r| r["task_id"].as_str() == Some(tid) && r["status"] != "destroyed")
                .count();
            return Ok(vec![vec![json!(n as i64)]]);
        }
        if q.contains("FROM shared_tasks WHERE task_id = {}") {
            let tid = params[0].as_str().unwrap_or("");
            let row: Vec<Value> = self
                .tasks
                .iter()
                .find(|t| t["task_id"].as_str() == Some(tid))
                .map(|t| {
                    vec![
                        t["task_id"].clone(),
                        t["title"].clone(),
                        t["owner"].clone(),
                        t["status"].clone(),
                        t["note"].clone(),
                        t["created_at"].clone(),
                        t["ended_at"].clone(),
                        t["ttl_at"].clone(),
                        t["destroyed_at"].clone(),
                    ]
                })
                .unwrap_or_default();
            return Ok(if row.is_empty() {
                Vec::new()
            } else {
                vec![row]
            });
        }
        if q.starts_with("SELECT task_id, title")
            && q.contains("shared_tasks WHERE destroyed_at IS NULL")
        {
            let rows: Vec<Vec<Value>> = self
                .tasks
                .iter()
                .filter(|t| {
                    t["destroyed_at"].is_null()
                        && ((t["status"] == "done" || t["status"] == "aborted")
                            || (!t["ttl_at"].is_null()
                                && is_expired(t["ttl_at"].as_str(), now_of(params))))
                })
                .map(|t| {
                    vec![
                        t["task_id"].clone(),
                        t["title"].clone(),
                        t["owner"].clone(),
                        t["status"].clone(),
                        t["note"].clone(),
                        t["created_at"].clone(),
                        t["ended_at"].clone(),
                        t["ttl_at"].clone(),
                        t["destroyed_at"].clone(),
                    ]
                })
                .collect();
            return Ok(rows);
        }
        if q.starts_with("UPDATE shared_content SET") {
            // 定位 content_id（倒数第 2 个参数）与 status 守卫（倒数第 1 个）
            let cid = params[params.len() - 2].as_i64().unwrap_or(0);
            let guard = params[params.len() - 1].as_str().unwrap_or("");
            let Some(pos) = self
                .rows
                .iter()
                .position(|r| r["content_id"].as_i64() == Some(cid))
            else {
                return Ok(Vec::new());
            };
            let ne_guard = q.contains("status <> {}");
            let eq_guard = !ne_guard && q.contains("AND status = {}");
            if ne_guard && self.rows[pos]["status"].as_str() == Some(guard) {
                return Ok(Vec::new());
            }
            if eq_guard && self.rows[pos]["status"].as_str() != Some(guard) {
                return Ok(Vec::new());
            }
            // 应用 SET 子句（按出现顺序把字段从 SET 里解析出来）
            let set_part = q.split(" SET ").nth(1).unwrap_or("");
            let set_clause = set_part.split(" WHERE ").next().unwrap_or("");
            let mut pi = 0usize;
            let old = self.rows[pos].clone();
            for assign in set_clause.split(',') {
                let assign = assign.trim();
                let Some((col, val)) = assign.split_once('=') else {
                    continue;
                };
                let col = col.trim();
                let val = val.trim();
                if val == "version + 1" {
                    let v = self.rows[pos]["version"].as_i64().unwrap_or(1);
                    self.rows[pos]["version"] = json!(v + 1);
                    continue;
                }
                if val.starts_with('{') || val.starts_with('%') {
                    let v = params.get(pi).cloned().unwrap_or(json!(""));
                    pi += 1;
                    match col {
                        "body" if v.as_str() == Some("") && v.as_str() == Some("") => {
                            // body = "" 只在 destroy/明确赋值时出现
                            self.rows[pos]["body"] = v.clone();
                        }
                        "embedding" => {
                            self.rows[pos].insert("embedding".into(), Value::Null);
                            continue;
                        }
                        _ => {
                            self.rows[pos][col] = v.clone();
                        }
                    }
                } else if val == "NULL" {
                    self.rows[pos].insert(col.to_string(), Value::Null);
                }
            }
            // update 留痕（字段级 diff，body 只记 changed）
            if self.rows[pos] != old {
                let mut changed = Map::new();
                for k in [
                    "scope",
                    "task_id",
                    "kind",
                    "title",
                    "body",
                    "tags",
                    "owner",
                    "audience",
                    "sensitivity",
                    "status",
                    "expires_at",
                    "review_at",
                    "updated_by",
                    "destroyed_at",
                    "source_refs",
                ] {
                    if self.rows[pos].get(k) != old.get(k) {
                        if k == "body"
                            || self.rows[pos]["sensitivity"] == "sensitive"
                            || old["sensitivity"] == "sensitive"
                        {
                            changed.insert(k.into(), json!({"changed": true}));
                        } else {
                            changed.insert(k.into(), json!({"from": old.get(k).cloned().unwrap_or(Value::Null),
                                                            "to": self.rows[pos].get(k).cloned().unwrap_or(Value::Null)}));
                        }
                    }
                }
                let new_st = self.rows[pos]["status"].as_str().unwrap_or("");
                let old_st = old["status"].as_str().unwrap_or("");
                let action = if new_st != old_st {
                    match new_st {
                        "destroyed" => "destroy",
                        "revoked" => "revoke",
                        "expired" => "expire",
                        "active" => "restore",
                        _ => "update",
                    }
                } else {
                    "update"
                };
                if !changed.is_empty() {
                    self.revisions.push(json_obj_from(
                        &[
                            ("rev_id", &json!(self.revisions.len() + 1)),
                            ("content_id", &json!(cid)),
                            ("action", &json!(action)),
                            (
                                "actor",
                                &json!(self.rows[pos]["updated_by"]
                                    .as_str()
                                    .filter(|s| !s.is_empty())
                                    .unwrap_or("unknown")),
                            ),
                            ("changed", &Value::Object(changed)),
                            ("note", &json!(self.note.clone().unwrap_or_default())),
                            ("ts", &self.rows[pos]["updated_at"].clone()),
                        ],
                        &[],
                    ));
                }
            }
            return Ok(vec![vec![json!(cid)]]);
        }
        if q.starts_with("SELECT count(*)") {
            return Ok(vec![vec![json!(0)]]);
        }
        if q.contains("FROM shared_content WHERE content_id") {
            let cid = params[0].as_i64().unwrap_or(0);
            if let Some(r) = self.get_row(cid) {
                return Ok(vec![vec![
                    r["content_id"].clone(),
                    r["scope"].clone(),
                    r["task_id"].clone(),
                    r["kind"].clone(),
                    r["title"].clone(),
                    r["body"].clone(),
                    r["tags"].clone(),
                    r["owner"].clone(),
                    r["audience"].clone(),
                    r["sensitivity"].clone(),
                    r["status"].clone(),
                    r["expires_at"].clone(),
                    r["review_at"].clone(),
                    r["version"].clone(),
                    r["created_at"].clone(),
                    r["updated_at"].clone(),
                    r["updated_by"].clone(),
                    r["destroyed_at"].clone(),
                    r["source_refs"].clone(),
                ]]);
            }
            return Ok(Vec::new());
        }
        if q.contains("FROM shared_content_revisions") {
            let cid = params[0].as_i64().unwrap_or(0);
            let rows: Vec<Vec<Value>> = self
                .revisions
                .iter()
                .filter(|r| r["content_id"].as_i64() == Some(cid))
                .map(|r| {
                    vec![
                        r["rev_id"].clone(),
                        r["content_id"].clone(),
                        r["action"].clone(),
                        r["actor"].clone(),
                        r["changed"].clone(),
                        r["note"].clone(),
                        r["ts"].clone(),
                    ]
                })
                .collect();
            return Ok(rows);
        }
        if q.contains("FROM shared_content WHERE") {
            // 列表/扫描：按 MemoryStore 全量线性过滤（单测够用）
            let now = now_of(params);
            let mut rows: Vec<Vec<Value>> = Vec::new();
            for r in &self.rows {
                if content_row_matches(&q, r, &params, now) {
                    rows.push(vec![
                        r["content_id"].clone(),
                        r["scope"].clone(),
                        r["task_id"].clone(),
                        r["kind"].clone(),
                        r["title"].clone(),
                        r["body"].clone(),
                        r["tags"].clone(),
                        r["owner"].clone(),
                        r["audience"].clone(),
                        r["sensitivity"].clone(),
                        r["status"].clone(),
                        r["expires_at"].clone(),
                        r["review_at"].clone(),
                        r["version"].clone(),
                        r["created_at"].clone(),
                        r["updated_at"].clone(),
                        r["updated_by"].clone(),
                        r["destroyed_at"].clone(),
                        r["source_refs"].clone(),
                    ]);
                }
            }
            return Ok(rows);
        }
        Err(CliError(format!("MemoryStore 不认识这条 SQL：{q}")))
    }

    fn set_note(&mut self, note: Option<&str>) -> Result<(), CliError> {
        self.note = note.filter(|s| !s.is_empty()).map(|s| s.to_string());
        Ok(())
    }
}

fn now_of(params: &[Value]) -> i64 {
    params
        .iter()
        .filter_map(|v| v.as_str())
        .find_map(|s| parse_epoch(s))
        .unwrap_or(0)
}

fn content_row_matches(q: &str, r: &Map<String, Value>, params: &[Value], _now: i64) -> bool {
    // 只覆盖单测用到的简单条件：scope = / status = / status NOT IN / status <> /
    // expires_at < / review_at < / task_id =
    let mut pi = 0usize;
    let cond = q.split(" WHERE ").nth(1).unwrap_or("");
    for clause in cond.split(" AND ") {
        let c = clause
            .trim()
            .trim_end_matches(" ORDER BY content_id ASC LIMIT {}")
            .trim_end_matches(" ORDER BY review_at ASC LIMIT {}")
            .trim_end_matches(" ORDER BY updated_at DESC, content_id DESC LIMIT {}")
            .trim();
        let c = c.trim_start_matches('(').trim_end_matches(')');
        let c = c.trim();
        if c.contains("scope =") {
            let want = param_str(params, pi);
            pi += 1;
            if r["scope"].as_str() != Some(want) {
                return false;
            }
        } else if c.contains("status =") && !c.contains("NOT IN") && !c.contains("<>") {
            let want = param_str(params, pi);
            pi += 1;
            if r["status"].as_str() != Some(want) {
                return false;
            }
        } else if c.contains("status NOT IN") {
            let a = param_str(params, pi);
            pi += 1;
            let b = param_str(params, pi);
            pi += 1;
            if Some(a) == r["status"].as_str() || Some(b) == r["status"].as_str() {
                return false;
            }
        } else if c.contains("status <>") {
            let want = param_str(params, pi);
            pi += 1;
            if r["status"].as_str() == Some(want) {
                return false;
            }
        } else if c.contains("ILIKE") {
            let needle = params
                .get(pi)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim_matches('%')
                .trim_start_matches('\\')
                .to_lowercase();
            let hay_title = r["title"].as_str().unwrap_or("").to_lowercase();
            let hay_body = r["body"].as_str().unwrap_or("").to_lowercase();
            let hay_tags = r["tags"].to_string().to_lowercase();
            if !(hay_title.contains(&needle)
                || hay_body.contains(&needle)
                || hay_tags.contains(&needle))
            {
                return false;
            }
        } else if c.contains("expires_at <") {
            let bound = params
                .get(pi)
                .and_then(|v| v.as_str())
                .and_then(parse_epoch);
            pi += 1;
            let t = r["expires_at"].as_str().and_then(parse_epoch);
            match (t, bound) {
                (Some(t), Some(b)) if t < b => {}
                _ => return false,
            }
        } else if c.contains("review_at <") {
            let bound = params
                .get(pi)
                .and_then(|v| v.as_str())
                .and_then(parse_epoch);
            pi += 1;
            let t = r["review_at"].as_str().and_then(parse_epoch);
            match (t, bound) {
                (Some(t), Some(b)) if t < b => {}
                _ => return false,
            }
        } else if c.contains("expires_at IS NOT NULL") {
            if r["expires_at"].is_null() {
                return false;
            }
        } else if c.contains("expires_at IS NULL OR expires_at >= ") {
            let bound = params
                .get(pi)
                .and_then(|v| v.as_str())
                .and_then(parse_epoch);
            pi += 1;
            let t = r["expires_at"].as_str().and_then(parse_epoch);
            match (t, bound) {
                (Some(t), Some(b)) if t >= b => {}
                (None, _) => {}
                _ => return false,
            }
        } else if c.contains("expires_at >= ") {
            let bound = params
                .get(pi)
                .and_then(|v| v.as_str())
                .and_then(parse_epoch);
            pi += 1;
            let t = r["expires_at"].as_str().and_then(parse_epoch);
            match (t, bound) {
                (Some(t), Some(b)) if t >= b => {}
                _ => return false,
            }
        } else if c.contains("expires_at IS NULL") {
            if !r["expires_at"].is_null() {
                return false;
            }
        } else if c.contains("task_id =") {
            let want = param_str(params, pi);
            pi += 1;
            if r["task_id"].as_str() != Some(want) {
                return false;
            }
        }
    }
    true
}

fn param_str(params: &[Value], i: usize) -> &'static str {
    // 返回生命周期有歧义 —— 用泄漏换简洁（单测专用，条目有限）
    match params.get(i).and_then(|v| v.as_str()) {
        Some(s) => Box::leak(s.to_string().into_boxed_str()),
        None => Box::leak(String::new().into_boxed_str()),
    }
}

fn parse_json_or(v: &Value, d: Value) -> Value {
    match v {
        Value::String(s) => serde_json::from_str(s).unwrap_or(d),
        Value::Null => d,
        other => other.clone(),
    }
}

fn json_obj_from(pairs: &[(&str, &Value)], extra: &[(&str, Value)]) -> Map<String, Value> {
    let mut o = Map::new();
    for (k, v) in pairs {
        o.insert(k.to_string(), (*v).clone());
    }
    for (k, v) in extra {
        o.insert(k.to_string(), v.clone());
    }
    o
}

// ---------------- 单测 ----------------

#[cfg(test)]
mod tests {
    use super::*;

    fn store_with_task(ttl_hours: Option<f64>, now: i64) -> MemoryStore {
        let mut s = MemoryStore::default();
        run_task_add(
            &mut s,
            "T1",
            "备份策略",
            "hermes",
            ttl_hours,
            None,
            None,
            now,
        )
        .unwrap();
        s
    }

    #[test]
    fn test_safe_err_and_read_dsn() {
        assert_eq!(safe_err("password=abc123 x"), "password=*** x");
        assert_eq!(safe_err("SSLpassword=zz\nline2"), "SSLpassword=*** line2");
        assert_eq!(safe_err("普通错误"), "普通错误");
        let f = std::env::temp_dir().join("sc_dsn_test.env");
        std::fs::write(&f, "A=1\nBB_PG_DSN=\"host=h port=5 dbname=d\"\n").unwrap();
        assert_eq!(
            read_dsn(f.to_str().unwrap()).unwrap(),
            "host=h port=5 dbname=d"
        );
    }

    #[test]
    fn test_time_helpers() {
        // 严格早于：等于不算过期
        assert!(!is_expired(Some("2026-01-01T00:00:00+00:00"), 1767225600));
        assert!(is_expired(Some("2026-01-01T00:00:00+00:00"), 1767225601));
        assert!(!is_expired(None, 1));
        // iso 往返
        assert_eq!(epoch_to_iso(1767225600), "2026-01-01T00:00:00+00:00");
        // when：days/hours/explicit
        assert_eq!(
            when(1000, Some(1.0), None, None, "--expires-at")
                .unwrap()
                .unwrap(),
            epoch_to_iso(1000 + 86400)
        );
        assert_eq!(
            when(1000, None, Some(2.0), None, "--expires-at")
                .unwrap()
                .unwrap(),
            epoch_to_iso(1000 + 7200)
        );
        assert_eq!(
            when(
                0,
                None,
                None,
                Some("2026-01-01T00:00:00+00:00"),
                "--expires-at"
            )
            .unwrap()
            .unwrap(),
            "2026-01-01T00:00:00+00:00"
        );
        assert!(when(0, None, None, Some("bad"), "--expires-at").is_err());
    }

    #[test]
    fn test_text_helpers() {
        assert_eq!(one_line(Some("a  b\nc"), 10), "a b c");
        assert_eq!(one_line(Some("0123456789"), 4), "0123…");
        assert_eq!(display_width("中文a"), 5);
        assert_eq!(
            split_list(Some("a,b , c")),
            Some(vec!["a".to_string(), "b".to_string(), "c".to_string()])
        );
        assert_eq!(split_list(None), None);
        assert_eq!(split_list(Some("")), Some(Vec::<String>::new()));
        let refs = parse_source_refs(&["entry:61".into(), "没冒号".into(), ":62".into()]);
        assert_eq!(refs[0]["type"], "entry");
        assert_eq!(refs[0]["value"], 61);
        assert_eq!(refs[1], json!({"type": "ref", "value": "没冒号"}));
        assert_eq!(refs[2]["type"], "ref");
        assert_eq!(like_pattern("50%_x"), "%50\\%\\_x%");
    }

    #[test]
    fn test_render_table() {
        assert_eq!(render_table(&["a"], &[]), "（没有匹配的记录）");
        let t = render_table(&["ID", "名"], &[vec!["1".into(), "中文".into()]]);
        let lines: Vec<&str> = t.lines().collect();
        assert_eq!(lines.len(), 3);
        // 中文对齐：表头行 "ID" 列宽 2
        assert!(lines[0].contains("ID"));
        assert!(lines[2].contains("中文"));
    }

    #[test]
    fn test_content_filters_default_hides() {
        let (w, p) = content_filters(&ContentFilter::default(), "2026-01-01T00:00:00+00:00");
        // status NOT IN (revoked, destroyed) + status <> expired + (expires_at IS NULL OR >= now)
        assert!(w.iter().any(|x| x.contains("NOT IN")));
        assert!(w.iter().any(|x| x.contains("status <>")));
        assert_eq!(p[0], "revoked");
        assert_eq!(p[1], "destroyed");
        assert_eq!(p[2], "expired");
        // 显式 status 时听它的
        let f = ContentFilter {
            status: Some("revoked".into()),
            ..Default::default()
        };
        let (w, p) = content_filters(&f, "x");
        assert_eq!(w.len(), 1);
        assert_eq!(p[0], "revoked");
    }

    #[test]
    fn test_task_lifecycle() {
        let now = 1_767_225_600i64;
        let mut s = store_with_task(Some(48.0), now);
        // put 任务级：没给过期 → 继承 TTL
        run_put(
            &mut s,
            "task",
            Some("T1"),
            "note",
            "背景",
            None,
            Some("正文A"),
            "hermes",
            None,
            Some("x,y"),
            None,
            "internal",
            None,
            None,
            None,
            None,
            None,
            &[],
            None,
            now,
        )
        .unwrap();
        let item = run_list(&mut s, &ContentFilter::default(), 50, now).unwrap();
        assert_eq!(item.value["count"], 1);
        let c = &item.value["contents"][0];
        assert_eq!(c["scope"], "task");
        assert_eq!(c["expires_at"], epoch_to_iso(now + 48 * 3600));
        assert_eq!(c["tags"].as_array().unwrap().len(), 2);
        // 永久级挂 task_id 报错
        let e = run_put(
            &mut s,
            "permanent",
            Some("T1"),
            "note",
            "t",
            None,
            Some("b"),
            "hermes",
            None,
            None,
            None,
            "internal",
            Some(1.0),
            None,
            None,
            None,
            None,
            &[],
            None,
            now,
        );
        assert!(e.is_err());
        // task-end：done
        let r = run_task_end(&mut s, "T1", "done", None, now + 10).unwrap();
        assert_eq!(r.value["pending"], 1);
        // sweep：任务结束 → 该任务内容进销毁清单
        let plan = sweep_plan(&mut s, now + 20, 500).unwrap();
        assert_eq!(plan["destroy"].as_array().unwrap().len(), 1);
        assert_eq!(plan["destroy"][0]["reason"], "任务已完成");
        // dry-run 不落库
        let r = run_sweep(&mut s, false, None, 500, now + 20).unwrap();
        assert!(!r.value["committed"].as_bool().unwrap());
        assert!(r.text.contains("演练"));
        assert_eq!(s.rows[0]["status"].as_str().unwrap(), "active");
        // commit：真销毁
        let r = run_sweep(&mut s, true, None, 500, now + 20).unwrap();
        assert!(r.value["committed"].as_bool().unwrap());
        assert_eq!(s.rows[0]["status"].as_str().unwrap(), "destroyed");
        assert_eq!(s.rows[0]["body"].as_str().unwrap(), "");
        assert!(s.rows[0]["destroyed_at"].is_string());
        // 留痕：create + destroy
        assert_eq!(s.revisions.len(), 2);
        assert_eq!(s.revisions[1]["action"], "destroy");
        assert_eq!(s.revisions[1]["changed"]["body"]["changed"], json!(true));
        // 任务本身打 destroyed_at
        assert!(s.tasks[0]["destroyed_at"].is_string());
    }

    #[test]
    fn test_put_validation_and_expired_semantics() {
        let now = 1_767_225_600i64;
        let mut s = store_with_task(None, now);
        // 任务级没过期时间（任务也没 TTL）→ 报错
        let e = run_put(
            &mut s,
            "task",
            Some("T1"),
            "note",
            "t",
            None,
            Some("b"),
            "hermes",
            None,
            None,
            None,
            "internal",
            None,
            None,
            None,
            None,
            None,
            &[],
            None,
            now,
        );
        assert!(e.is_err());
        // 任务不存在
        let e = run_put(
            &mut s,
            "task",
            Some("T9"),
            "note",
            "t",
            None,
            Some("b"),
            "hermes",
            None,
            None,
            None,
            "internal",
            Some(1.0),
            None,
            None,
            None,
            None,
            &[],
            None,
            now,
        );
        assert!(e.is_err());
        // 正常永久级 + 短过期
        run_put(
            &mut s,
            "permanent",
            None,
            "node",
            "IP",
            None,
            Some("1.2.3.4"),
            "hermes",
            None,
            None,
            None,
            "internal",
            Some(0.5 / 24.0),
            None,
            None, // 半小时
            None,
            None,
            &[],
            None,
            now,
        )
        .unwrap();
        // 30 分钟 +1s 后：默认列表过滤掉（expires_at < now，严格早于）
        let later = now + 1801;
        let r = run_list(&mut s, &ContentFilter::default(), 50, later).unwrap();
        assert_eq!(r.value["count"], 0);
        // expired=True 带出来，且 expired 字段由服务端算好
        let r = run_list(
            &mut s,
            &ContentFilter {
                expired: true,
                ..Default::default()
            },
            50,
            later,
        )
        .unwrap();
        assert_eq!(r.value["count"], 1);
        assert_eq!(r.value["contents"][0]["expired"], json!(true));
        // sweep：永久级到期 → 只标 expired，正文保留
        run_sweep(&mut s, true, None, 500, later).unwrap();
        assert_eq!(s.rows[0]["status"].as_str().unwrap(), "expired");
        assert_eq!(s.rows[0]["body"].as_str().unwrap(), "1.2.3.4");
    }

    #[test]
    fn test_update_revoke_destroy_guards() {
        let now = 1_767_225_600i64;
        let mut s = store_with_task(None, now);
        run_put(
            &mut s,
            "permanent",
            None,
            "node",
            "IP",
            None,
            Some("1.2.3.4"),
            "hermes",
            None,
            None,
            None,
            "internal",
            None,
            None,
            None,
            None,
            None,
            &[],
            None,
            now,
        )
        .unwrap();
        // update 留痕 + version 自增
        run_update(
            &mut s,
            1,
            "刚哥",
            Some("换了宽带"),
            None,
            None,
            Some("5.6.7.8"),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            &[],
            now + 60,
        )
        .unwrap();
        assert_eq!(s.rows[0]["version"], json!(2));
        assert_eq!(s.rows[0]["body"], json!("5.6.7.8"));
        let revs = get_revisions(&mut s, 1, 100).unwrap();
        assert_eq!(revs.len(), 2);
        assert_eq!(revs[1]["action"], "update");
        assert_eq!(revs[1]["actor"], "刚哥");
        assert!(revs[1]["changed"]["body"].is_object());
        assert_eq!(revs[1]["changed"]["body"]["changed"], json!(true)); // body 只记 changed
        assert_eq!(revs[1]["changed"]["from"], json!(null)); // body 无 from/to
                                                             // 空 update 报错
        let e = run_update(
            &mut s,
            1,
            "x",
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            &[],
            now,
        );
        assert!(e.is_err());
        // revoke
        run_revoke(&mut s, 1, "刚哥", Some("不共享了"), now + 120).unwrap();
        assert_eq!(s.rows[0]["status"], json!("revoked"));
        // 重复 revoke 报错
        let e = run_revoke(&mut s, 1, "刚哥", None, now + 120);
        assert!(e.is_err());
        // update 到 destroyed 禁止
        let e = run_update(
            &mut s,
            1,
            "x",
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some("destroyed"),
            None,
            None,
            None,
            None,
            None,
            &[],
            now,
        );
        assert!(e.is_err());
        // destroy：正文清空
        run_destroy(&mut s, 1, "刚哥", None, now + 180).unwrap();
        assert_eq!(s.rows[0]["status"], json!("destroyed"));
        assert_eq!(s.rows[0]["body"], json!(""));
        assert!(s.rows[0]["destroyed_at"].is_string());
        // 已销毁再 revoke 报错
        let e = run_revoke(&mut s, 1, "刚哥", None, now + 180);
        assert!(e.is_err());
        // 已销毁再 update 报错
        let e = run_update(
            &mut s,
            1,
            "x",
            None,
            None,
            None,
            Some("b"),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            &[],
            now,
        );
        assert!(e.is_err());
        // get 的留痕动作序列
        let g = run_get(&mut s, 1, now + 200).unwrap();
        let actions: Vec<&str> = g.value["revisions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["action"].as_str().unwrap())
            .collect();
        assert_eq!(actions, vec!["create", "update", "revoke", "destroy"]);
        // get 文本含人话
        assert!(g.text.contains("已销毁，正文不留残"));
        assert!(g.text.contains("改动历史"));
    }

    #[test]
    fn test_search_keyword() {
        let now = 1_767_225_600i64;
        let mut s = store_with_task(None, now);
        run_put(
            &mut s,
            "permanent",
            None,
            "node",
            "龙虾公网IP",
            None,
            Some("1.2.3.4"),
            "hermes",
            None,
            Some("node,ip"),
            None,
            "internal",
            None,
            None,
            None,
            None,
            None,
            &[],
            None,
            now,
        )
        .unwrap();
        run_put(
            &mut s,
            "permanent",
            None,
            "agent",
            "各机Agent清单",
            None,
            Some("claude"),
            "hermes",
            None,
            None,
            None,
            "internal",
            None,
            None,
            None,
            None,
            None,
            &[],
            None,
            now,
        )
        .unwrap();
        // 关键词命中标题
        let r = run_search(&mut s, "公网", false, &ContentFilter::default(), 50, now).unwrap();
        assert_eq!(r.value["mode"], "keyword");
        assert_eq!(r.value["count"], 1);
        assert!(r.value["contents"][0]["score"].is_null());
        // semantic 标志也降级为关键词
        let r = run_search(&mut s, "Agent", true, &ContentFilter::default(), 50, now).unwrap();
        assert_eq!(r.value["mode"], "keyword");
        assert_eq!(r.value["count"], 1);
    }

    #[test]
    fn test_task_level_expiry_sweep_destroys() {
        let now = 1_767_225_600i64;
        let mut s = store_with_task(Some(100.0), now);
        run_put(
            &mut s,
            "task",
            Some("T1"),
            "note",
            "t",
            None,
            Some("b"),
            "hermes",
            None,
            None,
            None,
            "internal",
            None,
            Some(0.1),
            None,
            None,
            None,
            &[],
            None,
            now,
        )
        .unwrap();
        // 任务本身还 active，但内容自己到期 → 销毁清单（原因=任务级内容已过期）
        let plan = sweep_plan(&mut s, now + 3600, 500).unwrap();
        assert_eq!(plan["destroy"].as_array().unwrap().len(), 1);
        assert_eq!(plan["destroy"][0]["reason"], "任务级内容已过期");
        run_sweep(&mut s, true, Some("sweep"), 500, now + 3600).unwrap();
        assert_eq!(s.rows[0]["status"], json!("destroyed"));
        // 任务本身不受影响（还 active，没 destroyed_at）
        assert!(s.tasks[0]["destroyed_at"].is_null());
    }
}
