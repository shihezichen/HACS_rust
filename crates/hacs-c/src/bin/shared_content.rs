//! `shared_content` CLI 组合根（对应 Python `hub/l3_domain/shared_content.py` main）。
//! 用法：shared_content <subcommand> [args] [--json] [--dsn-file F | --sqlite F]
use hacs_l3_domain::shared_content as sc;
use hacs_l3_domain::shared_content::{CmdResult, Fetch, Store};
use hacs_x_identity::redact::safe_err;
use serde_json::{json, Value};
use std::process::ExitCode;

const VERSION: &str = "1.0";
const DEFAULT_DSN_FILE: &str = "/etc/hacs/shared_content_pg.env";

fn err(e: String) -> sc::CliError {
    sc::CliError(e)
}

// ---------------- PG Store ----------------

pub struct PgStore {
    conn: postgres::Client,
    note: Option<String>,
}

impl PgStore {
    fn connect(dsn: &str) -> Result<Self, String> {
        if dsn.is_empty() {
            return Err("DSN 为空：检查 --dsn-file 文件里有没有 BB_PG_DSN=".into());
        }
        let conn = postgres::Client::connect(dsn, postgres::NoTls)
            .map_err(|e| format!("连不上库：{}", safe_err(&e.to_string())))?;
        Ok(Self { conn, note: None })
    }
}

fn bind(v: &Value) -> String {
    // 全部按文本绑定，靠 SQL 里的 ::timestamptz / 数值上下文转换；数值直接给字面量。
    match v {
        Value::Null => "NULL".to_string(),
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        other => other.to_string(),
    }
}

/// 把 `%s` 占位符按序替换成 `$n`；`%%s::timestamptz` 之类不动（我们只换 %s 本身）。
fn to_pg_sql(sql: &str, params: &[Value]) -> String {
    let mut out = String::with_capacity(sql.len() + params.len() * 4);
    let bytes = sql.as_bytes();
    let mut i = 0;
    let mut count = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 1 < bytes.len() && bytes[i + 1] == b's' {
            let rest = &sql[i + 2..];
            let cast_tz = rest.starts_with("::timestamptz");
            let cast_vec = rest.starts_with("::vector");
            let cast_json = rest.starts_with("::jsonb");
            if cast_vec {
                count += 1;
                out.push_str(&format!("NULLIF(${count}::text,'')::vector"));
                i += 2 + "::vector".len();
                continue;
            }
            count += 1;
            if cast_tz || cast_json {
                let suffix = if cast_tz { "::timestamptz" } else { "::jsonb" };
                if cast_tz {
                    out.push_str(&format!("NULLIF(${count}::text,'')::timestamptz"));
                } else {
                    out.push_str(&format!("${count}::text::jsonb"));
                }
                i += 2 + suffix.len();
                continue;
            }
            out.push_str(&format!("${count}"));
            i += 2;
        } else {
            out.push(bytes[i] as char);
            i += 1;
        }
    }
    out
}

impl Store for PgStore {
    fn exec(
        &mut self,
        sql: &str,
        params: &[Value],
        fetch: Fetch,
    ) -> Result<Vec<Vec<Value>>, sc::CliError> {
        let pg_sql = to_pg_sql(sql, params);
        // 参数绑定：数字绑定成 i64/f64（PG 数值列要二进制格式），其余按文本。
        let mut texts: Vec<String> = Vec::new();
        let mut ints: Vec<i64> = Vec::new();
        let mut floats: Vec<f64> = Vec::new();
        let mut kind: Vec<u8> = Vec::new(); // 0=text 1=int 2=float 3=null
        let mut nulls: Vec<Option<String>> = Vec::new();
        for p in params {
            match p {
                Value::Number(n) => {
                    nulls.push(None);
                    if let Some(i) = n.as_i64() {
                        ints.push(i);
                        kind.push(1);
                        texts.push(String::new());
                        floats.push(0.0);
                    } else {
                        let f = n.as_f64().unwrap_or(0.0);
                        floats.push(f);
                        kind.push(2);
                        texts.push(String::new());
                        ints.push(0);
                    }
                }
                Value::Null => {
                    texts.push(String::new());
                    kind.push(3); // NULL
                    ints.push(0);
                    floats.push(0.0);
                    nulls.push(None::<String>);
                }
                other => {
                    texts.push(bind(other));
                    kind.push(0);
                    ints.push(0);
                    floats.push(0.0);
                    nulls.push(None);
                }
            }
        }
        // postgres crate 的参数是异构切片：这里统一用一个封装枚举不现实（类型擦除受限），
        // 采用折衷：全部转成 String 绑定，SQL 里的数值上下文由 PG 自动 cast；
        // timestamptz 由 SQL 里的 ::text::timestamptz 双跳完成（沿 rt_remind 教训）。
        let mut refs: Vec<&(dyn postgres::types::ToSql + Sync)> = Vec::new();
        for (i, k) in kind.iter().enumerate() {
            match k {
                1 => refs.push(&ints[i]),
                2 => refs.push(&floats[i]),
                3 => refs.push(&nulls[i]),
                _ => refs.push(&texts[i]),
            }
        }

        // 空参数（DDL / 无占位符）与有参数分开走
        let rows = if params.is_empty() {
            self.conn.query(&pg_sql, &[])
        } else {
            self.conn.query(&pg_sql, &refs)
        }
        .map_err(|e| {
            let mut msg = e.to_string();
            let mut src = std::error::Error::source(&e);
            while let Some(s) = src {
                msg.push_str(&format!(" | cause: {s}"));
                src = s.source();
            }
            err(format!("数据库操作失败：{}", safe_err(&msg)))
        })?;

        match fetch {
            Fetch::None => Ok(Vec::new()),
            _ => Ok(rows.iter().map(row_to_vec).collect()),
        }
    }

    fn set_note(&mut self, note: Option<&str>) -> Result<(), sc::CliError> {
        self.note = note.filter(|s| !s.is_empty()).map(|s| s.to_string());
        Ok(())
    }
}

fn cell(r: &postgres::Row, idx: usize) -> Value {
    // 列类型未知：先试常见类型
    macro_rules! try_get {
        ($t:ty) => {
            if let Ok(v) = r.try_get::<_, Option<$t>>(idx) {
                return match v {
                    Some(x) => json!(x),
                    None => Value::Null,
                };
            }
        };
    }
    try_get!(String);
    try_get!(i64);
    try_get!(i32);
    try_get!(f64);
    try_get!(bool);
    try_get!(serde_json::Value);
    try_get!(Vec<f32>);
    Value::Null
}

fn row_to_vec(r: &postgres::Row) -> Vec<Value> {
    (0..r.len()).map(|i| cell(r, i)).collect()
}

fn read_dsn(path: &str) -> Result<String, sc::CliError> {
    std::fs::read_to_string(path)
        .map_err(|e| {
            err(format!(
                "读不到 DSN 文件 {path}：{}",
                safe_err(&e.to_string())
            ))
        })?
        .lines()
        .find_map(|l| l.strip_prefix("BB_PG_DSN=").map(|s| s.trim().to_string()))
        .ok_or_else(|| err(format!("DSN 文件 {path} 里没有 BB_PG_DSN=")))
}

// ---------------- 参数解析 ----------------

struct Args {
    json: bool,
    dsn_file: Option<String>,
    sqlite: Option<String>,
    rest: Vec<String>,
}

fn parse_global(argv: &[String]) -> (Args, Vec<String>) {
    let mut json = false;
    let mut dsn_file = None;
    let mut sqlite = None;
    let mut rest = Vec::new();
    let mut i = 0;
    // 子命令先吃掉，其余全局 flag 散落其间
    while i < argv.len() {
        let a = &argv[i];
        if rest.is_empty() && !a.starts_with('-') {
            rest.push(a.clone());
            i += 1;
            continue;
        }
        match a.as_str() {
            "--json" => json = true,
            "--dsn-file" => {
                i += 1;
                dsn_file = argv.get(i).cloned();
            }
            "--sqlite" => {
                i += 1;
                sqlite = argv.get(i).cloned();
            }
            _ => rest.push(a.clone()),
        }
        i += 1;
    }
    (
        Args {
            json,
            dsn_file,
            sqlite,
            rest,
        },
        Vec::new(),
    )
}

fn flag(rest: &[String], name: &str) -> Option<String> {
    rest.iter()
        .position(|a| a == name)
        .and_then(|i| rest.get(i + 1).cloned())
}

fn has(rest: &[String], name: &str) -> bool {
    rest.iter().any(|a| a == name)
}

fn fnum(rest: &[String], name: &str) -> Result<Option<f64>, sc::CliError> {
    match flag(rest, name) {
        None => Ok(None),
        Some(v) => v
            .parse::<f64>()
            .map(Some)
            .map_err(|_| err(format!("{name} 要是数字，得到 {v:?}"))),
    }
}

fn inum(rest: &[String], name: &str, dft: i64) -> Result<i64, sc::CliError> {
    match flag(rest, name) {
        None => Ok(dft),
        Some(v) => v
            .parse::<i64>()
            .map_err(|_| err(format!("{name} 要是整数"))),
    }
}

fn source_refs(rest: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut it = rest.iter().peekable();
    while let Some(a) = it.next() {
        if a == "--source-ref" {
            if let Some(v) = it.next() {
                out.push(v.clone());
            }
        }
    }
    out
}

fn emit(res: &CmdResult, json_mode: bool) {
    if json_mode {
        let mut v = res.value.clone();
        if let Some(obj) = v.as_object_mut() {
            obj.remove("_text");
        }
        println!("{v}");
    } else {
        println!("{}", res.text);
    }
}

fn usage() -> &'static str {
    "用法：shared_content <task-add|task-end|tasks|put|update|revoke|destroy|get|list|search|sweep> [args]\n全局：--json --dsn-file F --sqlite F"
}

fn now_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let (args, _) = parse_global(&argv);
    if args.rest.is_empty() {
        eprintln!("{}", usage());
        return ExitCode::from(2);
    }
    let cmd = args.rest[0].clone();
    let rest = args.rest[1..].to_vec();

    let mut store_box: Box<dyn Store> = {
        if args.sqlite.is_some() {
            eprintln!("失败：--sqlite 替身后端未在 Rust 侧实现（单测用 MemoryStore 覆盖）");
            return ExitCode::from(2);
        }
        let path = args
            .dsn_file
            .clone()
            .unwrap_or_else(|| DEFAULT_DSN_FILE.to_string());
        match read_dsn(&path).and_then(|d| PgStore::connect(&d).map_err(err)) {
            Ok(s) => Box::new(s),
            Err(e) => {
                eprintln!("失败：{}", e.0);
                return ExitCode::from(1);
            }
        }
    };
    let store = store_box.as_mut();
    let now = now_epoch();

    let res = dispatch(&cmd, &rest, store, now);
    match res {
        Ok(r) => {
            emit(&r, args.json);
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("失败：{}", e.0);
            ExitCode::from(2)
        }
    }
}

fn dispatch(
    cmd: &str,
    rest: &[String],
    store: &mut dyn Store,
    now: i64,
) -> Result<CmdResult, sc::CliError> {
    match cmd {
        "task-add" => sc::run_task_add(
            store,
            &flag(rest, "--task-id").unwrap_or_default(),
            &flag(rest, "--title").unwrap_or_default(),
            &flag(rest, "--owner").unwrap_or_default(),
            fnum(rest, "--ttl-hours")?,
            flag(rest, "--ttl-at").as_deref(),
            flag(rest, "--note").as_deref(),
            now,
        ),
        "task-end" => sc::run_task_end(
            store,
            &flag(rest, "--task-id").unwrap_or_default(),
            &flag(rest, "--status").unwrap_or_else(|| "done".into()),
            flag(rest, "--note").as_deref(),
            now,
        ),
        "tasks" => sc::run_tasks(
            store,
            flag(rest, "--status").as_deref(),
            inum(rest, "--limit", 50)? as usize,
            now,
        ),
        "put" => sc::run_put(
            store,
            &flag(rest, "--scope").unwrap_or_default(),
            flag(rest, "--task-id").as_deref(),
            &flag(rest, "--kind").unwrap_or_else(|| "note".into()),
            &flag(rest, "--title").unwrap_or_default(),
            flag(rest, "--body-file").as_deref(),
            flag(rest, "--body").as_deref(),
            &flag(rest, "--owner").unwrap_or_default(),
            flag(rest, "--by").as_deref(),
            flag(rest, "--tags").as_deref(),
            flag(rest, "--audience").as_deref(),
            &flag(rest, "--sensitivity").unwrap_or_else(|| "internal".into()),
            fnum(rest, "--expires-in-days")?,
            fnum(rest, "--expires-in-hours")?,
            flag(rest, "--expires-at").as_deref(),
            fnum(rest, "--review-in-days")?,
            flag(rest, "--review-at").as_deref(),
            &source_refs(rest),
            flag(rest, "--note").as_deref(),
            now,
        ),
        "update" => sc::run_update(
            store,
            flag(rest, "--id").and_then(|v| v.parse().ok()).unwrap_or(0),
            &flag(rest, "--by").unwrap_or_default(),
            flag(rest, "--note").as_deref(),
            flag(rest, "--title").as_deref(),
            flag(rest, "--body-file").as_deref(),
            flag(rest, "--body").as_deref(),
            flag(rest, "--tags").as_deref(),
            flag(rest, "--audience").as_deref(),
            flag(rest, "--owner").as_deref(),
            flag(rest, "--kind").as_deref(),
            flag(rest, "--sensitivity").as_deref(),
            flag(rest, "--status").as_deref(),
            flag(rest, "--expires-at").as_deref(),
            fnum(rest, "--expires-in-days")?,
            flag(rest, "--review-at").as_deref(),
            fnum(rest, "--review-in-days")?,
            &source_refs(rest),
            now,
        ),
        "revoke" => sc::run_revoke(
            store,
            flag(rest, "--id").and_then(|v| v.parse().ok()).unwrap_or(0),
            &flag(rest, "--by").unwrap_or_default(),
            flag(rest, "--note").as_deref(),
            now,
        ),
        "destroy" => sc::run_destroy(
            store,
            flag(rest, "--id").and_then(|v| v.parse().ok()).unwrap_or(0),
            &flag(rest, "--by").unwrap_or_default(),
            flag(rest, "--note").as_deref(),
            now,
        ),
        "get" => sc::run_get(
            store,
            flag(rest, "--id").and_then(|v| v.parse().ok()).unwrap_or(0),
            now,
        ),
        "list" => sc::run_list(
            store,
            &sc::ContentFilter {
                scope: flag(rest, "--scope"),
                kind: flag(rest, "--kind"),
                owner: flag(rest, "--owner"),
                tag: flag(rest, "--tag"),
                task_id: flag(rest, "--task-id"),
                status: flag(rest, "--status"),
                expired: has(rest, "--expired"),
            },
            inum(rest, "--limit", 50)? as usize,
            now,
        ),
        "search" => sc::run_search(
            store,
            &flag(rest, "--q").unwrap_or_default(),
            has(rest, "--semantic"),
            &sc::ContentFilter {
                scope: flag(rest, "--scope"),
                kind: flag(rest, "--kind"),
                owner: flag(rest, "--owner"),
                tag: flag(rest, "--tag"),
                task_id: flag(rest, "--task-id"),
                status: flag(rest, "--status"),
                expired: has(rest, "--expired"),
            },
            inum(rest, "--limit", 50)? as usize,
            now,
        ),
        "sweep" => sc::run_sweep(
            store,
            has(rest, "--commit"),
            flag(rest, "--by").as_deref(),
            inum(rest, "--limit", 500)? as usize,
            now,
        ),
        _ => Err(sc::CliError(format!(
            "不认识的子命令 {cmd:?}（{}）",
            usage()
        ))),
    }
}
