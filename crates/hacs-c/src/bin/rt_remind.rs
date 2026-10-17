//! `rt_remind` —— 每日逾期提醒 CLI（对应 Python `hub/l3_domain/rt_remind.py`
//! 的 main/argparse/PgStore/NatsSender 装配）。
//!
//! 用法（与 Python 对齐）：
//! * `rt_remind --dry-run --dsn-file <ETC>/pg.env`：演练，只读只打印；
//! * `rt_remind --commit --dsn-file F --nats-host H --nats-user U
//!    --nats-password-file P`：真发 + 写 reminder_log（**默认被停用**，
//!   需 `RT_REMIND_ENABLE=1`，按 2026-09-20 指令）；
//! * `rt_remind --dry-run --fixture F.json`：离线自测，不连库不联网。
//!
//! DSN 取法等价 `sed -n 's/^BB_PG_DSN=//p' <file> | head -1` —— **绝不能 source**。
//! 退出码：0 跑完（没有逾期也算）；1 有条目发送失败；2 参数或初始化错误。

use std::time::Duration;

use hacs_l3_domain::rt_remind as rr;
use hacs_l3_domain::rt_remind::{Sender, Stats};
use serde_json::Value;

fn main() {
    std::process::exit(run());
}

struct Opts {
    dry_run: bool,
    commit: bool,
    dsn_file: String,
    fixture: Option<String>,
    owner: Option<String>,
    limit: i64,
    timeout: f64,
    nats_host: Option<String>,
    nats_port: u16,
    nats_user: Option<String>,
    nats_password_file: Option<String>,
}

fn run() -> i32 {
    let mut o = Opts {
        dry_run: false,
        commit: false,
        dsn_file: default_dsn_file(),
        fixture: None,
        owner: None,
        limit: rr::DEFAULT_LIMIT,
        timeout: rr::DEFAULT_TIMEOUT_S,
        nats_host: std::env::var("AGENT_TEAM_NATS_HOST").ok(),
        nats_port: 9930,
        nats_user: None,
        nats_password_file: None,
    };
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--dry-run" => o.dry_run = true,
            "--commit" => o.commit = true,
            "--dsn-file" => o.dsn_file = args.next().unwrap_or_default(),
            "--fixture" => o.fixture = Some(args.next().unwrap_or_default()),
            "--owner" => o.owner = Some(args.next().unwrap_or_default()),
            "--limit" => {
                o.limit = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(rr::DEFAULT_LIMIT)
            }
            "--timeout" => {
                o.timeout = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(rr::DEFAULT_TIMEOUT_S)
            }
            "--nats-host" => o.nats_host = Some(args.next().unwrap_or_default()),
            "--nats-port" => o.nats_port = args.next().and_then(|v| v.parse().ok()).unwrap_or(9930),
            "--nats-user" => o.nats_user = Some(args.next().unwrap_or_default()),
            "--nats-password-file" => o.nats_password_file = Some(args.next().unwrap_or_default()),
            "--help" | "-h" => {
                println!(
                    "rt_remind —— 每日逾期提醒（--dry-run / --commit / --fixture，详见文件头注释）"
                );
                return 0;
            }
            other => {
                eprintln!("未知参数: {other}");
                return 2;
            }
        }
    }

    let commit = o.commit && !o.dry_run;
    // ── 停用开关（2026-09-20 指令）：发送路径现已被拒，--commit 直接返回不连库不发消息；
    //    确需恢复：先经主人同意，再以 RT_REMIND_ENABLE=1 运行。dry-run / fixture 永远可用。
    if commit && std::env::var("RT_REMIND_ENABLE").as_deref() != Ok("1") {
        println!(
            "[已停用] rt_remind —— 按 2026-09-20 指令，旧会议的遗留/逾期提醒功能已停止：\
             不再向任何成员点名发消息，--commit 路径被拒（不连库、不发消息）。\
             只读演练仍可用；确需恢复请先经主人同意，再以 RT_REMIND_ENABLE=1 运行。"
        );
        return 0;
    }
    if o.fixture.is_some() && commit {
        eprintln!("--fixture 是离线自测用的假数据，不许配 --commit");
        return 2;
    }
    if !commit {
        println!("[演练] 未给 --commit：只读清单、只打印点名文本，不写 reminder_log、不发消息");
    }

    // --- 存储
    enum Store {
        Pg(PgStore),
        Mem(rr::MemoryStore),
    }
    let store = if let Some(path) = &o.fixture {
        match std::fs::read_to_string(path)
            .map_err(|e| e.to_string())
            .and_then(|t| serde_json::from_str::<Value>(&t).map_err(|e| e.to_string()))
        {
            Ok(data) => Store::Mem(rr::load_fixture(&data)),
            Err(e) => {
                eprintln!("初始化存储失败：{e}");
                return 2;
            }
        }
    } else {
        let dsn = read_dsn(&o.dsn_file);
        match PgStore::connect(&dsn) {
            Ok(s) => Store::Pg(s),
            Err(e) => {
                eprintln!("初始化存储失败：{e}");
                return 2;
            }
        }
    };

    // --- 发送通道：只有 commit 才接真 NATS
    if commit && (o.nats_user.is_none() || o.nats_password_file.is_none()) {
        eprintln!("--commit 需要 --nats-user 与 --nats-password-file");
        return 2;
    }
    let timeout = o.timeout.clamp(1.0, rr::MAX_TIMEOUT_S);

    let stats = match store {
        Store::Mem(mut m) => {
            let mut ps = rr::PrintSender::default();
            let (stats, lines) = rr::run_remind(
                &mut m,
                &mut ps,
                now_utc(),
                commit,
                o.limit.max(1),
                o.owner.as_deref(),
            );
            print_lines(&lines, &ps.sent, commit);
            stats
        }
        Store::Pg(mut pg) => {
            if commit {
                let password = o
                    .nats_password_file
                    .as_deref()
                    .map(read_secret)
                    .unwrap_or_default();
                let mut sender = match NatsSender::new(
                    o.nats_host.as_deref().unwrap_or("127.0.0.1"),
                    o.nats_port,
                    o.nats_user.as_deref().unwrap_or(""),
                    &password,
                    timeout,
                ) {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("点名通道不可用：{e}");
                        return 2;
                    }
                };
                let now = now_utc();
                let overdue = pg.overdue(now, o.limit.max(1));
                let (stats, lines) = run_pg(
                    &mut pg,
                    &mut sender,
                    &overdue,
                    now,
                    true,
                    o.owner.as_deref(),
                );
                for l in &lines {
                    println!("{l}");
                }
                sender.close();
                stats
            } else {
                let mut ps = rr::PrintSender::default();
                let now = now_utc();
                let overdue = pg.overdue(now, o.limit.max(1));
                let (stats, lines) =
                    run_pg(&mut pg, &mut ps, &overdue, now, false, o.owner.as_deref());
                print_lines(&lines, &ps.sent, false);
                stats
            }
        }
    };
    if stats.failed > 0 {
        1
    } else {
        0
    }
}

fn print_lines(lines: &[String], sent: &[(String, String)], _commit: bool) {
    let mut si = 0usize;
    for l in lines {
        println!("{l}");
    }
    for (owner, text) in sent {
        println!("[dry-run] 本该点名 a2a.v1.req.{owner}，内容如下（没发）：");
        println!("          {text}");
        si += 1;
    }
    let _ = si;
}

/// PgStore 路径的主流程（与 run_remind 同构，store 换成 PgStore）。
fn run_pg<S: Sender>(
    pg: &mut PgStore,
    sender: &mut S,
    items_all: &[rr::Item],
    now: i64,
    commit: bool,
    owner_filter: Option<&str>,
) -> (Stats, Vec<String>) {
    let mut say: Vec<String> = Vec::new();
    let items = rr::select_overdue(items_all, now);
    let items: Vec<rr::Item> = items
        .into_iter()
        .filter(|i| {
            owner_filter
                .map(|f| (i.owner.as_deref().unwrap_or("")) == f)
                .unwrap_or(true)
        })
        .collect();

    let already = pg.sent_today(now);
    let pending: Vec<&rr::Item> = items
        .iter()
        .filter(|i| !already.contains(&rr::item_key(i)))
        .collect();
    let skipped = items.len() - pending.len();

    let owned: Vec<rr::Item> = pending.iter().map(|i| (*i).clone()).collect();
    let groups = rr::group_by_owner(&owned);
    say.push(rr::render_summary(&groups, now, skipped));
    say.push(String::new());

    let mut sent_ok = 0usize;
    let mut failed = 0usize;
    let mut logged = 0usize;
    for (owner, rows) in &groups {
        let text = rr::render_call(owner, rows, now);
        if owner == rr::NO_OWNER {
            say.push(format!(
                "[跳过] {} 项没有责任人，点名发不出去，等人工认领",
                rows.len()
            ));
            continue;
        }
        if !commit {
            let _ = sender.send(owner, &text);
            continue;
        }
        let (ok, detail) = sender.send(owner, &text);
        if ok {
            sent_ok += 1;
            say.push(format!(
                "[已点名] {owner}（{} 项）→ {}",
                rows.len(),
                rr::truncate(&detail, 120)
            ));
            for item in rows {
                pg.log_reminder(&item.target_kind, &item.target_id, owner);
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

// ================================================================ PgStore

pub struct PgStore {
    conn: postgres::Client,
}

impl PgStore {
    pub fn connect(dsn: &str) -> Result<Self, String> {
        if dsn.is_empty() {
            return Err("DSN 为空：检查 --dsn-file 指向的文件里有没有 BB_PG_DSN=".into());
        }
        let conn = postgres::Client::connect(dsn, postgres::NoTls).map_err(|e| {
            format!(
                "连不上库：{}",
                hacs_x_identity::redact::safe_err(&e.to_string())
            )
        })?;
        Ok(Self { conn })
    }

    /// 两张表各捞一次逾期清单（过滤条件全部下推 SQL，now 由调用方给）。
    pub fn overdue(&mut self, now: i64, limit: i64) -> Vec<rr::Item> {
        let now_iso = epoch_to_iso_utc(now);
        let mut out = Vec::new();
        let sql_a = format!(
            "SELECT a.action_id, a.what, a.owner, a.due_at::text, a.status, a.session_id, s.topic \
             FROM roundtable_actions a \
             LEFT JOIN roundtable_sessions s ON s.session_id = a.session_id \
             WHERE a.status IN ('open','doing') AND a.due_at IS NOT NULL \
               AND a.due_at < $1::text::timestamptz \
             ORDER BY a.due_at ASC, a.action_id ASC LIMIT {limit}"
        );
        let rows = match self.conn.query(&sql_a, &[&now_iso]) {
            Ok(r) => r,
            Err(_) => Vec::new(),
        };
        for r in &rows {
            out.push(rr::make_item(
                "action",
                r.get::<_, i64>(0).into(),
                &r.get::<_, Option<String>>(1).unwrap_or_default().into(),
                &r.get::<_, Option<String>>(2).unwrap_or_default().into(),
                &r.get::<_, Option<String>>(3).unwrap_or_default().into(),
                &r.get::<_, Option<String>>(4).unwrap_or_default(),
                r.get::<_, Option<String>>(5).as_deref(),
                None,
                r.get::<_, Option<String>>(6).as_deref(),
            ));
        }
        let sql_i = format!(
            "SELECT i.issue_id, i.title, i.owner, i.due_at::text, i.status, i.session_id, i.kind, s.topic \
             FROM roundtable_issues i \
             LEFT JOIN roundtable_sessions s ON s.session_id = i.session_id \
             WHERE i.status IN ('open','watching') AND i.due_at IS NOT NULL \
               AND i.due_at < $1::text::timestamptz \
             ORDER BY i.due_at ASC, i.issue_id ASC LIMIT {limit}"
        );
        if let Ok(rows) = self.conn.query(&sql_i, &[&now_iso]) {
            for r in &rows {
                out.push(rr::make_item(
                    "issue",
                    r.get::<_, i64>(0).into(),
                    &r.get::<_, Option<String>>(1).unwrap_or_default().into(),
                    &r.get::<_, Option<String>>(2).unwrap_or_default().into(),
                    &r.get::<_, Option<String>>(3).unwrap_or_default().into(),
                    &r.get::<_, Option<String>>(4).unwrap_or_default(),
                    r.get::<_, Option<String>>(5).as_deref(),
                    r.get::<_, Option<String>>(6).as_deref(),
                    r.get::<_, Option<String>>(7).as_deref(),
                ));
            }
        }
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

    /// 今天（UTC 日历日）已经提醒过的 (kind, id) 集合。
    pub fn sent_today(&mut self, now: i64) -> Vec<(String, Value)> {
        let start_iso = epoch_to_iso_utc(rr::day_start(now));
        let sql =
            "SELECT target_kind, target_id::text FROM reminder_log WHERE sent_at >= $1::text::timestamptz";
        let mut out = Vec::new();
        if let Ok(rows) = self.conn.query(sql, &[&start_iso]) {
            for r in &rows {
                let kind = r.get::<_, Option<String>>(0).unwrap_or_default();
                let id: Value = r
                    .get::<_, Option<String>>(1)
                    .and_then(|s| serde_json::from_str(&s).ok())
                    .unwrap_or(Value::Null);
                out.push((kind, id));
            }
        }
        out
    }

    /// 写的只有 reminder_log 的 INSERT —— 不改任何业务表的状态。
    pub fn log_reminder(&mut self, target_kind: &str, target_id: &Value, owner: &str) {
        let sql =
            "INSERT INTO reminder_log (target_kind, target_id, owner) VALUES ($1,$2::text,$3)";
        let id_text = target_id.to_string();
        let _ = self
            .conn
            .execute(sql, &[&target_kind.to_string(), &id_text, &owner]);
    }
}

/// epoch → `YYYY-MM-DDTHH:MM:SSZ`（UTC，裸口径给 `::timestamptz`）。
fn epoch_to_iso_utc(epoch: i64) -> String {
    let (y, mo, d, h, mi) = hacs_l3_domain::team_group::beijing_parts_pub(epoch - 8 * 3600);
    let s = (epoch - 8 * 3600).rem_euclid(86400) % 60;
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

// ================================================================ NatsSender

pub struct NatsSender {
    timeout: f64,
    client: Option<hacs_l1_transport::nats_client::NatsClient>,
}

impl NatsSender {
    pub fn new(
        host: &str,
        port: u16,
        user: &str,
        password: &str,
        timeout: f64,
    ) -> Result<Self, String> {
        let client = hacs_l1_transport::nats_client::NatsClient::new(
            hacs_l1_transport::nats_client::NatsConfig::from_hosts_str(
                &format!("{host}:{port}"),
                port,
            )
            .unwrap_or_else(|_| {
                hacs_l1_transport::nats_client::NatsConfig::from_hosts_str("127.0.0.1", port)
                    .unwrap()
            })
            .with_credentials(Some(user.to_string()), Some(password.to_string()))
            .with_name("rt-remind"),
        );
        Ok(Self {
            timeout,
            client: Some(client),
        })
    }
}

impl Sender for NatsSender {
    fn name(&self) -> &'static str {
        "nats"
    }
    fn send(&mut self, owner: &str, text: &str) -> (bool, String) {
        let Some(client) = &mut self.client else {
            return (false, "NATS 客户端未初始化".into());
        };
        let envelope = serde_json::json!({
            "from": "rt_remind", "text": text, "timeout_s": self.timeout,
        });
        let raw = envelope.to_string();
        if client.connect().is_err() {
            return (false, "NATS 请求异常：连不上".into());
        }
        let reply = client.request(
            &format!("a2a.v1.req.{owner}"),
            raw.as_bytes(),
            Duration::from_secs_f64(self.timeout + 15.0),
        );
        match reply {
            None => (false, format!("点名超时（{:.0}s 内无应答）", self.timeout)),
            Some(raw) => (true, rr::truncate(&String::from_utf8_lossy(&raw), 200)),
        }
    }
}

impl NatsSender {
    pub fn close(&mut self) {
        if let Some(c) = &mut self.client {
            c.close();
        }
    }
}

// ================================================================ 小工具

/// 从 systemd 环境文件取 BB_PG_DSN（等价 sed -n 's/^BB_PG_DSN=//p' | head -1）。
/// **绝不能 source**：值里带空格，shell 会截断成错值。
pub fn read_dsn(path: &str) -> String {
    let Ok(text) = std::fs::read_to_string(path) else {
        return String::new();
    };
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("BB_PG_DSN=") {
            let mut value = rest.trim().to_string();
            // 去一层引号（与 Python 同口径）
            if value.len() >= 2 {
                let b = value.as_bytes();
                if (b[0] == b'\'' && b[value.len() - 1] == b'\'')
                    || (b[0] == b'"' && b[value.len() - 1] == b'"')
                {
                    value = value[1..value.len() - 1].to_string();
                }
            }
            return value;
        }
    }
    String::new()
}

/// 读凭据文件（只取内容去空白）。内容绝不打印、绝不进日志。
pub fn read_secret(path: &str) -> String {
    std::fs::read_to_string(path)
        .map(|t| t.trim().to_string())
        .unwrap_or_default()
}

fn now_utc() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn default_dsn_file() -> String {
    if let Ok(p) = std::env::var("AGENT_TEAM_PG_ENV_FILE") {
        return p;
    }
    format!("{}/pg.env", hacs_l0_host::node_paths::etc_dir(None))
}
