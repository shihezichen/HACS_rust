//! L0 平台层：**节点档案**（对应 Python `node/l0_host/node_profile.py`）。
//!
//! 一句话职责：**让上层对操作系统零感知** —— 路径 / 凭据 / 日志 / 探针 / 宿主真源，
//! 全部在这里落地；上层只拿契约化的返回值，不许自己拼盘符、不许自己开别人的库。
//!
//! 覆盖面：
//! ① 节点身份 ② 运行与配置 ③ 执行器 ④ **目标主机档案** ⑤ 宿主真源
//! ⑥ **只读探针** ⑦ 日志与探针口 ⑧ 协作域地址 ⑨ 服务落盘位置
//!
//! 两条铁律：
//! - **只读**：⑥ 那组探针一律以 SQLite 只读 URI（`mode=ro`）打开，用完即 close；
//!   本模块不建目录、不写文件、不改任何状态。
//! - **不编造**：路径没配 / 文件不在 / 记录不在 → 返回 `None` 或空，**绝不猜值**；
//!   真正的存储/解析故障 → **原样抛**（`Err`），交给调用方按自己的口径降级并记日志。

use std::collections::HashMap;
use std::path::Path;

use crate::node_paths::{self, first_env, join, join_all};

// ======================================================== ① 节点身份

/// 本节点平台语义：`"nt"` / `"posix"`。
pub fn flavour(value: Option<&str>) -> String {
    node_paths::flavour(value)
}

/// 本节点当前用户的家目录。
pub fn home() -> String {
    node_paths::home()
}

/// 本节点名的 env 覆盖（轻量节点守护要回答「我是哪台」）。
pub const ENV_NODE_NAME: &str = "AGENT_TEAM_NODE";

/// **本节点**在档案表 / 部署表里的名字。
///
/// 口径：env `AGENT_TEAM_NODE` 优先 → 否则取主机名并归一（去域名后缀、小写，
/// 与 [`target_host`] 同一套归一规则）。取不到返回空串 —— **不猜**。
pub fn node_name() -> String {
    if let Some(explicit) = first_env(&[ENV_NODE_NAME]) {
        return target_host(&explicit);
    }
    let raw = hostname().unwrap_or_default();
    let head = raw.split('.').next().unwrap_or("").to_string();
    target_host(&head)
}

/// 取主机名：优先 env（Windows `COMPUTERNAME` / posix `HOSTNAME`），否则试 `hostname` 命令。
/// 取不到返回 `None`（不猜）。
fn hostname() -> Option<String> {
    for key in ["COMPUTERNAME", "HOSTNAME"] {
        if let Some(v) = node_paths::env_stripped(key) {
            return Some(v);
        }
    }
    let out = std::process::Command::new("hostname").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

// ======================================================== ② 运行与配置目录

pub fn runtime_dir(value: Option<&str>) -> String {
    node_paths::runtime_dir(value)
}
pub fn etc_dirs(value: Option<&str>) -> Vec<String> {
    node_paths::etc_dirs(value)
}
pub fn etc_dir(value: Option<&str>) -> String {
    node_paths::etc_dir(value)
}
pub fn log_dir(value: Option<&str>) -> String {
    node_paths::log_dir(value)
}
pub fn nats_dir(value: Option<&str>) -> String {
    node_paths::nats_dir(value)
}

// ======================================================== ③ 执行器路径

pub fn node_exe(value: Option<&str>) -> String {
    node_paths::node_exe(value)
}
pub fn claude_exec(value: Option<&str>) -> String {
    node_paths::claude_exec(value)
}
pub fn claude_workdir(value: Option<&str>) -> String {
    node_paths::claude_workdir(value)
}
pub fn codex_exe(value: Option<&str>) -> String {
    node_paths::codex_exe(value)
}
pub fn codex_cmd(value: Option<&str>) -> String {
    node_paths::codex_cmd(value)
}
pub fn pi_cli_js(value: Option<&str>) -> String {
    node_paths::pi_cli_js(value)
}
pub fn pi_workdir(value: Option<&str>) -> String {
    node_paths::pi_workdir(value)
}

// ======================================================== ④ 目标主机档案
// 查不到的主机不编造 —— 按 posix 兜底，同时 target_known() 为 false。

pub const ENV_TARGET_PROFILE_JSON: &str = "AGENT_TEAM_TARGET_PROFILE_JSON";

/// 目标主机的家目录（跨主机真值，字面量只许出现在 L0）。
pub const NT_TARGET_HOME: &str = "<home>";
pub const POSIX_TARGET_HOME: &str = "/root";

/// 家目录 → claude 执行器的相对位置（与本机 `claude_exec()` 同一套惯例）。
pub fn target_claude_sub(flavour: &str) -> &'static str {
    if flavour == "nt" {
        "AppData/Local/hermes/scripts/claude_exec.py"
    } else {
        ".local/share/hermes/scripts/claude_exec.py"
    }
}

/// 未登记主机的兜底 flavour：**不按本机推**，一律 posix。
pub const TARGET_FALLBACK_FLAVOUR: &str = "posix";

/// 一份目标主机档案。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetProfile {
    pub flavour: String,
    pub home: String,
    pub claude_exec: String,
}

/// 内置档案表（键 = 主机名小写）。
pub fn builtin_target_profiles() -> Vec<(&'static str, TargetProfile)> {
    vec![
        (
            "thinkbook",
            TargetProfile {
                flavour: "nt".into(),
                home: NT_TARGET_HOME.into(),
                claude_exec: format!(
                    "{NT_TARGET_HOME}/AppData/Local/hermes/scripts/claude_exec.py"
                ),
            },
        ),
        (
            "lobster",
            TargetProfile {
                flavour: "posix".into(),
                home: POSIX_TARGET_HOME.into(),
                claude_exec: format!(
                    "{POSIX_TARGET_HOME}/.local/share/hermes/scripts/claude_exec.py"
                ),
            },
        ),
        (
            "quantum",
            TargetProfile {
                flavour: "posix".into(),
                home: POSIX_TARGET_HOME.into(),
                claude_exec: format!(
                    "{POSIX_TARGET_HOME}/.local/share/hermes/scripts/claude_exec.py"
                ),
            },
        ),
        (
            "top001",
            TargetProfile {
                flavour: "posix".into(),
                home: POSIX_TARGET_HOME.into(),
                claude_exec: format!(
                    "{POSIX_TARGET_HOME}/.local/share/hermes/scripts/claude_exec.py"
                ),
            },
        ),
    ]
}

/// `user@host` / `host` → 主机名（小写）。只取 host 部分，用户名不参与查表。
pub fn target_host(host_value: &str) -> String {
    let text = host_value.trim();
    let text = match text.rsplit_once('@') {
        Some((_, host)) => host,
        None => text,
    };
    text.trim().to_lowercase()
}

/// 档案表（`AGENT_TEAM_TARGET_PROFILE_JSON` **整表覆盖**；解析不了就用内置表）。
pub fn target_table() -> HashMap<String, TargetProfile> {
    if let Some(env) = node_paths::env_stripped(ENV_TARGET_PROFILE_JSON) {
        if let Ok(data) = serde_json::from_str::<serde_json::Value>(&env) {
            if let Some(obj) = data.as_object() {
                let mut out = HashMap::new();
                for (k, v) in obj {
                    if let Some(row) = v.as_object() {
                        let get = |key: &str| {
                            row.get(key).and_then(|x| x.as_str()).map(|s| s.to_string())
                        };
                        out.insert(
                            target_host(k),
                            TargetProfile {
                                flavour: get("flavour").unwrap_or_default(),
                                home: get("home").unwrap_or_default(),
                                claude_exec: get("claude_exec").unwrap_or_default(),
                            },
                        );
                    }
                }
                return out;
            }
        }
        // 覆盖值坏了就退回内置表，绝不抛给调用方
    }
    builtin_target_profiles()
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect()
}

/// → `(档案, 是否登记在案)`。档案里缺的键按该 flavour 的惯例补齐。
pub fn target_profile(host_value: &str) -> (TargetProfile, bool) {
    let table = target_table();
    let known = table.get(&target_host(host_value)).cloned();
    let is_known = known.is_some();
    let entry = known.unwrap_or(TargetProfile {
        flavour: String::new(),
        home: String::new(),
        claude_exec: String::new(),
    });
    let mut fl = entry.flavour.trim().to_lowercase();
    if fl != "nt" && fl != "posix" {
        fl = TARGET_FALLBACK_FLAVOUR.to_string();
    }
    let mut home_dir = entry.home.trim().to_string();
    if home_dir.is_empty() {
        home_dir = if fl == "nt" {
            NT_TARGET_HOME.to_string()
        } else {
            POSIX_TARGET_HOME.to_string()
        };
    }
    let mut exe = entry.claude_exec.trim().to_string();
    if exe.is_empty() {
        exe = format!(
            "{}/{}",
            home_dir.trim_end_matches(['/', '\\']),
            target_claude_sub(&fl)
        );
    }
    (
        TargetProfile {
            flavour: fl,
            home: home_dir,
            claude_exec: exe,
        },
        is_known,
    )
}

/// 目标主机是否登记在档案表里。false = 下面是**兜底值不是真值**。
pub fn target_known(host_value: &str) -> bool {
    target_profile(host_value).1
}

/// 目标主机的平台语义：`"nt"` / `"posix"`（未登记按 posix 兜底）。
pub fn target_flavour(host_value: &str) -> String {
    target_profile(host_value).0.flavour
}

/// 目标主机上的家目录。
pub fn target_home(host_value: &str) -> String {
    target_profile(host_value).0.home
}

/// **目标主机上**的 Claude Code 封装脚本路径（跨主机调用取这个，别取 `claude_exec()`）。
pub fn target_claude_exec(host_value: &str) -> String {
    target_profile(host_value).0.claude_exec
}

// ======================================================== ⑤ 宿主真源
// 「**宿主**上长什么样」的知识：环境变量名、平台惯例目录、文件名。

pub const ENV_HERMES_STATE_DB: &[&str] = &["A2A_HERMES_STATE_DB", "CTX_HERMES_STATE_DB"];
pub const ENV_OPENCLAW_SESSIONS: &[&str] = &["A2A_SESSIONS_JSON", "CTX_SESSIONS_JSON"];
pub const ENV_HERMES_HOME: &[&str] = &["A2A_HERMES_HOME", "HERMES_HOME"];
pub const ENV_HERMES_AGENT_DIR: &[&str] = &["A2A_HERMES_AGENT_DIR"];

/// Hermes 主目录的平台惯例位置（相对家目录，按优先级）。
pub const HERMES_HOME_SUBS: &[&[&str]] = &[&["AppData", "Local", "hermes"], &[".hermes"]];
/// Hermes 生效配置的文件名。
pub const HERMES_CONFIG_NAME: &str = "config.yaml";

/// OpenClaw 会话档案里 A2A 会话落到的键格式。
pub const OPENCLAW_SESSION_KEY_FMT: &str = "agent:main:openai-user:a2a:%s:%s";

/// Hermes state.db 的读法（宿主文件格式知识，只该在 L0 出现）。
pub const SQL_HERMES_SESSION_USAGE: &str =
    "SELECT input_tokens, cache_read_tokens, api_call_count, model \
     FROM sessions WHERE session_key = ? \
     ORDER BY COALESCE(last_activity_at, started_at) DESC LIMIT 1";
pub const SQL_HERMES_SESSION_ID: &str = "SELECT id, model FROM sessions WHERE session_key = ? \
     ORDER BY COALESCE(last_activity_at, started_at) DESC LIMIT 1";
pub const SQL_HERMES_ACTIVE_MESSAGES: &str =
    "SELECT content, tool_calls, reasoning_content FROM messages WHERE session_id = ? AND active = 1";

/// 只读打开的超时（秒）。宿主库可能正被它自己的进程写着，别死等。
pub const HERMES_STATE_RO_TIMEOUT_SECS: f64 = 3.0;

/// Hermes `state.db` 的**候选路径**。没配置就是空列表 —— 不猜。
pub fn hermes_state_db_paths() -> Vec<String> {
    match first_env(ENV_HERMES_STATE_DB) {
        Some(v) => vec![v],
        None => vec![],
    }
}

/// Hermes `state.db` 路径（候选中的第一个；未配置返回空串）。
pub fn hermes_state_db() -> String {
    hermes_state_db_paths()
        .into_iter()
        .next()
        .unwrap_or_default()
}

/// OpenClaw `sessions.json` 的**候选路径**。没配置就是空列表 —— 不猜。
pub fn openclaw_sessions_paths() -> Vec<String> {
    match first_env(ENV_OPENCLAW_SESSIONS) {
        Some(v) => vec![v],
        None => vec![],
    }
}

/// OpenClaw 会话档案路径（候选中的第一个；未配置返回空串）。
pub fn openclaw_sessions_json() -> String {
    openclaw_sessions_paths()
        .into_iter()
        .next()
        .unwrap_or_default()
}

/// 宿主 Hermes 主目录的**候选**（env 覆盖时只有一个；否则是各平台惯例位置）。
pub fn hermes_home_paths() -> Vec<String> {
    if let Some(explicit) = first_env(ENV_HERMES_HOME) {
        return vec![explicit];
    }
    let base = home();
    HERMES_HOME_SUBS
        .iter()
        .map(|sub| join_all(&[&base, sub[0], if sub.len() > 1 { sub[1] } else { "" }]))
        .collect()
}

/// 宿主 Hermes 主目录：先 env，再按平台惯例探测（都不在返回空串，不猜）。
pub fn hermes_home() -> String {
    if let Some(explicit) = first_env(ENV_HERMES_HOME) {
        return explicit;
    }
    for candidate in hermes_home_paths() {
        if Path::new(&candidate).is_dir() {
            return candidate;
        }
    }
    String::new()
}

/// 宿主 Hermes 生效配置 `config.yaml` 的路径（主目录找不到时返回空串）。
pub fn hermes_config_path() -> String {
    let base = hermes_home();
    if base.is_empty() {
        return String::new();
    }
    join(&base, HERMES_CONFIG_NAME)
}

/// 宿主 Hermes 的 Python 包目录（未配置返回空串，不猜）。
pub fn hermes_agent_dir() -> String {
    first_env(ENV_HERMES_AGENT_DIR).unwrap_or_default()
}

/// 本节点上是否真有一份 OpenClaw 会话档案（存在性探针；不读内容）。
pub fn openclaw_sessions_present() -> bool {
    let path = openclaw_sessions_json();
    !path.is_empty() && Path::new(&path).is_file()
}

/// A2A 调用在 OpenClaw 会话档案里的键。
pub fn openclaw_session_key(caller: &str, session_id: &str) -> String {
    let c = if caller.is_empty() { "unknown" } else { caller };
    OPENCLAW_SESSION_KEY_FMT
        .replacen("%s", c, 1)
        .replacen("%s", session_id, 1)
}

// ======================================================== ⑥ 只读探针
// 契约：**只读**（SQLite `mode=ro` URI）。「没有」与「坏了」严格分开。

/// 只读打开宿主 Hermes `state.db`。
///
/// 未配置 / 文件不存在 → `Ok(None)`（这是「没有」，不是「坏了」）。
/// 打开失败 → `Err`（原样抛，由调用方按自己的口径降级并记日志）。
pub fn open_hermes_state_ro(
    db_path: Option<&str>,
) -> Result<Option<rusqlite::Connection>, rusqlite::Error> {
    use rusqlite::OpenFlags;
    let db = match db_path {
        Some(p) => p.to_string(),
        None => hermes_state_db(),
    };
    if db.is_empty() || !Path::new(&db).is_file() {
        return Ok(None);
    }
    let uri = format!("file:{}?mode=ro", db.replace('\\', "/"));
    let conn = rusqlite::Connection::open_with_flags(
        uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )?;
    conn.busy_timeout(std::time::Duration::from_secs_f64(
        HERMES_STATE_RO_TIMEOUT_SECS,
    ))?;
    Ok(Some(conn))
}

/// 会话的**累计用量行** → `(input_tokens, cache_read_tokens, api_call_count, model)`。
///
/// 库不可用 / 该 session_key 没有记录 → `Ok(None)`。读失败 `Err`。
/// Hermes 会话的累计用量行：`(input_tokens, cache_read_tokens, api_call_count, model)`。
pub type UsageRow = (Option<i64>, Option<i64>, Option<i64>, Option<String>);

/// 会话当前生效上下文的一行：`(content, tool_calls, reasoning_content)`。
pub type ActiveMessageRow = (Option<String>, Option<String>, Option<String>);

/// Hermes 会话的累计用量行 → 上述四元组。库不可用 / 没有记录 → `Ok(None)`。
pub fn hermes_session_usage_row(
    session_key: &str,
    db_path: Option<&str>,
) -> Result<Option<UsageRow>, rusqlite::Error> {
    let conn = match open_hermes_state_ro(db_path)? {
        Some(c) => c,
        None => return Ok(None),
    };
    let mut stmt = conn.prepare(SQL_HERMES_SESSION_USAGE)?;
    let mut rows = stmt.query([session_key])?;
    if let Some(row) = rows.next()? {
        Ok(Some((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)))
    } else {
        Ok(None)
    }
}

/// 会话**当前生效上下文**的消息行 → `(rows, 会话登记的 model)`。
///
/// `rows` 每行 = `(content, tool_calls, reasoning_content)`，只取 `active = 1`
/// （随 Hermes 压缩轮转会变，**切勿缓存**）。
/// 库不可用 / 会话不在 / 一行都没有 → `Ok(None)`。读失败 `Err`。
pub fn hermes_active_messages(
    session_key: &str,
    db_path: Option<&str>,
) -> Result<Option<(Vec<ActiveMessageRow>, String)>, rusqlite::Error> {
    let conn = match open_hermes_state_ro(db_path)? {
        Some(c) => c,
        None => return Ok(None),
    };
    let (sid, row_model) = {
        let mut stmt = conn.prepare(SQL_HERMES_SESSION_ID)?;
        let mut rows = stmt.query([session_key])?;
        match rows.next()? {
            Some(row) => (
                row.get::<_, Option<String>>(0)?,
                row.get::<_, Option<String>>(1)?.unwrap_or_default(),
            ),
            None => return Ok(None),
        }
    };
    let sid = match sid {
        Some(s) => s,
        None => return Ok(None),
    };
    let mut stmt = conn.prepare(SQL_HERMES_ACTIVE_MESSAGES)?;
    let mut rows = stmt.query([sid])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push((row.get(0)?, row.get(1)?, row.get(2)?));
    }
    if out.is_empty() {
        return Ok(None);
    }
    Ok(Some((out, row_model)))
}

/// OpenClaw 会话档案里某个 key 的条目。
///
/// 未配置 / 文件不在 / key 不在 / 条目不是对象 → `Ok(None)`。读或解析失败 `Err`。
pub fn openclaw_session_entry(
    session_key: &str,
    path: Option<&str>,
) -> Result<Option<serde_json::Value>, OpenclawError> {
    let p = match path {
        Some(p) => p.to_string(),
        None => openclaw_sessions_json(),
    };
    if p.is_empty() || !Path::new(&p).is_file() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(&p).map_err(|e| OpenclawError::Io(e.to_string()))?;
    let store: serde_json::Value =
        serde_json::from_str(&raw).map_err(|e| OpenclawError::Parse(e.to_string()))?;
    let entry = store.get(session_key);
    Ok(match entry {
        Some(v) if v.is_object() => Some(v.clone()),
        _ => None,
    })
}

/// OpenClaw 读/解析失败的两种「坏了」（原样抛给调用方）。
#[derive(Debug)]
pub enum OpenclawError {
    Io(String),
    Parse(String),
}

impl std::fmt::Display for OpenclawError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpenclawError::Io(m) => write!(f, "io: {m}"),
            OpenclawError::Parse(m) => write!(f, "parse: {m}"),
        }
    }
}
impl std::error::Error for OpenclawError {}

/// 宿主 Hermes 生效配置里 `model:` 段声明的 `(model.default, model.context_length)`。
///
/// `model.context_length` **只对 `model.default` 那一个模型生效**，不是全局分母。
/// 主目录/配置文件不在 → `("", 0)`（不猜）。读或解析失败 `Err`。
pub fn hermes_config_model_window() -> Result<(String, i64), std::io::Error> {
    let path = hermes_config_path();
    if path.is_empty() || !Path::new(&path).is_file() {
        return Ok((String::new(), 0));
    }
    let text = std::fs::read_to_string(&path)?;
    let mut name = String::new();
    let mut ctx_len: i64 = 0;
    let mut in_model = false;
    for raw in text.lines() {
        let stripped = raw.trim();
        if stripped.is_empty() || stripped.starts_with('#') {
            continue;
        }
        let first_char = raw.chars().next();
        let indented = matches!(first_char, Some(c) if c.is_whitespace());
        if !indented {
            // 顶层键：判断是否仍在 model: 段内
            let head = stripped.split(':').next().unwrap_or("");
            in_model = head == "model";
            continue;
        }
        if !in_model {
            continue;
        }
        let (key, val) = match stripped.split_once(':') {
            Some((k, v)) => (k, v.trim().trim_matches(['"', '\''])),
            None => (stripped, ""),
        };
        if key == "default" {
            name = val.to_string();
        } else if key == "context_length" {
            ctx_len = val.parse::<i64>().unwrap_or(0);
        }
    }
    Ok((name, ctx_len))
}

// ======================================================== ⑦ 日志与探针口

pub const ENV_A2A_PROBE_BASE: &[&str] = &["CTX_A2A_BASE", "A2A_BASE"];
pub const ENV_A2A_PROBE_TOKEN: &[&str] = &["CTX_A2A_TOKEN", "A2A_TOKEN"];
pub const ENV_A2A_PROBE_TOKEN_FILE: &[&str] = &[
    "CTX_A2A_TOKEN_FILE",
    "A2A_TOKEN_FILE",
    "AGENT_TEAM_TOKEN_FILE",
];
/// 令牌文件在配置目录里的文件名（运行期拼装，非字面量）。
pub const A2A_TOKEN_FILE_NAME: &str = "client-token";

/// 本节点 a2a 服务基址（水位探针口；未配置返回空串）。
pub fn a2a_probe_base() -> String {
    first_env(ENV_A2A_PROBE_BASE).unwrap_or_default()
}

/// 令牌文件的**候选路径**（按优先级）：三个 env 指名的，再加配置目录候选下的默认文件名。
pub fn a2a_probe_token_paths() -> Vec<String> {
    let mut candidates: Vec<String> = ENV_A2A_PROBE_TOKEN_FILE
        .iter()
        .filter_map(|n| node_paths::env_stripped(n))
        .collect();
    for d in etc_dirs(None) {
        candidates.push(join(&d, A2A_TOKEN_FILE_NAME));
    }
    candidates
}

/// a2a 服务令牌：先 env，再逐个候选令牌文件。都拿不到返回空串（不编造）。
pub fn a2a_probe_token() -> String {
    if let Some(tok) = first_env(ENV_A2A_PROBE_TOKEN) {
        return tok;
    }
    for path in a2a_probe_token_paths() {
        if path.is_empty() || !Path::new(&path).is_file() {
            continue;
        }
        if let Ok(raw) = std::fs::read_to_string(&path) {
            let value = raw.trim();
            if !value.is_empty() {
                return value.to_string();
            }
        }
    }
    String::new()
}

// ======================================================== ⑧ 协作域地址
// 「**这个协作域**长什么样」的知识 —— 尾网节点地址与各服务端口。
// 本组函数是**纯函数**，不读 env、不看平台，返回值恒定。

/// 协作域（tailnet）节点地址 —— 主 / 备。跨件唯一真源（脱敏占位符）。
pub const NODE_ADDR_PRIMARY: &str = "<host>";
pub const NODE_ADDR_SECONDARY: &str = "<node-ip>";

/// 端口（与 `hacs_layers.json` 的 ports 登记同号）。
pub const PORT_NATS: u16 = 9930;
pub const PORT_BLACKBOARD: u16 = 9940;
pub const PORT_CHAT: u16 = 9984;
pub const PORT_TASK: u16 = 9981;

/// 只给人看的地址**形态**（CLI help 文本里的示例），**不是真地址，不许拿去连**。
pub const NODE_ADDR_SHAPE: &str = "100.x.y.z";

/// NATS 中枢地址 —— **单值形态**：只给主节点，不带端口。
pub fn nats_host_default() -> String {
    NODE_ADDR_PRIMARY.to_string()
}

/// NATS 集群地址串 —— **多值形态**：`主:端口,备:端口`，主先备后。
pub fn nats_hosts_default() -> String {
    format!(
        "{}:{},{}:{}",
        NODE_ADDR_PRIMARY, PORT_NATS, NODE_ADDR_SECONDARY, PORT_NATS
    )
}

/// 黑板服务基址（写入只走它的 POST /entries）。
pub fn blackboard_base_default() -> String {
    format!("http://{}:{}", NODE_ADDR_PRIMARY, PORT_BLACKBOARD)
}

/// 人机对话台（群的家）基址 —— 群消息走 HTTP 落库。
pub fn chat_base_default() -> String {
    format!("http://{}:{}", NODE_ADDR_PRIMARY, PORT_CHAT)
}

/// 任务服务（群任务清单）基址。
pub fn task_base_default() -> String {
    format!("http://{}:{}", NODE_ADDR_PRIMARY, PORT_TASK)
}

/// CLI help 文本里的地址形态占位 —— 展示用，不是可连地址。
pub fn nats_host_shape() -> String {
    NODE_ADDR_SHAPE.to_string()
}

// ======================================================== ⑨ 服务落盘默认位置
// 这几条**刻意不随本机平台自适应** —— 它们描述的是「那套部署惯例长什么样」。

/// posix 节点上 root 的家目录。
pub const POSIX_ROOT_HOME: &str = POSIX_TARGET_HOME;
/// NATS 集群路由凭据的**文件名**。
pub const NATS_ROUTE_CRED_NAME: &str = ".nats-route.pass";
/// task_svc 落盘件。
pub const TASK_SVC_DB_NAME: &str = "task_svc.db";
pub const TASK_SVC_LOG_SUB: &str = "logs/task_svc.log";
/// Codex CLI 的工作目录 —— 本机 Windows 真值（脱敏占位符）。
pub const WIN_CODEX_WORKDIR: &str = r"<work-dir>";

/// NATS 服务端配置目录的 **posix 惯例值**（env `AGENT_TEAM_NATS_DIR` 仍优先）。
pub fn nats_conf_dir_posix() -> String {
    node_paths::nats_dir(Some("posix"))
}

/// NATS 集群路由凭据文件（posix；root 家目录下，运行期拼装）。
pub fn nats_route_pass_file() -> String {
    format!("{POSIX_ROOT_HOME}/{NATS_ROUTE_CRED_NAME}")
}

/// task_svc 的 SQLite 库文件默认位置（posix 部署惯例）。
pub fn task_svc_db_default() -> String {
    format!("{}/{}", node_paths::dev_deploy_dir(), TASK_SVC_DB_NAME)
}

/// task_svc 的日志文件默认位置（posix 部署惯例）。
pub fn task_svc_log_default() -> String {
    format!("{}/{}", node_paths::dev_deploy_dir(), TASK_SVC_LOG_SUB)
}

/// Codex CLI 的工作目录默认值。
pub fn codex_workdir_default() -> String {
    WIN_CODEX_WORKDIR.to_string()
}
