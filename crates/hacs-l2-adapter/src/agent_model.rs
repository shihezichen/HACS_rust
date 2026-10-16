//! L2 适配层：**模型执行器适配** + **Agent 展示名**。对应 Python `hub/l2_adapter/agent_model.py`。
//!
//! 职责边界（只此两件，别往里加业务）：
//!
//! ① 模型富化执行器 —— 回答「**怎么调**模型」：ssh 二进制 / 反向目标 / 私钥与 known_hosts /
//!    远端命令行 / 编码口径 / 超时与限速默认值 / 子进程调用 / 输出解析。
//!    **不回答「调什么」**（提示词模板、队列 worker、限速重投、写库、台账 —— 那是 L3）。
//!
//! ② Agent 展示名 —— 兜底表 + `AGENT_TEAM_DISPLAY_NAMES` 覆盖 + [`display_name`] 语义。
//!
//! **失败是正常路径**：超时 / ssh 不通 / 执行器不存在 / 输出不是 JSON —— 一律返回
//! `(None, 原因)`，绝不抛给上层、绝不编造判定。
//!
//! 远端执行器路径取**目标主机**的真值：`REMOTE_CMD` 里的 `claude_exec.py` 跑在 **ssh 目标机**
//! 上，所以路径必须按目标主机取 `node_profile::target_claude_exec(目标)`。取本机路径会把
//! Linux 的路径发给 Windows 目标机，远端报 `can't open file`，整条模型通道静默失效。

use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use hacs_l0_host::node_paths as np;
use hacs_l0_host::node_profile as npf;
use hacs_x_identity::safe_err;
use serde_json::{json, Value};

// ------------------------------------------------------------ 宿主默认值（env 优先）

/// ⚠️ **目标主机**上的执行器路径取的是目标主机真值（不是本机的）。
pub const ENV_SSH_TARGET: &str = "AGENT_TEAM_SSH_TARGET";

/// 模型通道 = 目标机上**反向 SSH 回本机**跑 `claude_exec.py`。
pub const ENABLE_ENV: &str = "CHAT_ENRICH_ENABLE";
pub const SSH_BIN_ENV: &str = "CHAT_ENRICH_SSH_BIN";
pub const SSH_ENV: &str = "CHAT_ENRICH_SSH";
pub const KEY_ENV: &str = "CHAT_ENRICH_KEY";
pub const KNOWN_ENV: &str = "CHAT_ENRICH_KNOWN_HOSTS";
pub const CMD_ENV: &str = "CHAT_ENRICH_CMD";
pub const TIMEOUT_ENV: &str = "CHAT_ENRICH_TIMEOUT_S";
pub const MIN_INTERVAL_ENV: &str = "CHAT_ENRICH_MIN_INTERVAL_S";

pub const SSH_BIN: &str = "/usr/bin/ssh";
pub const TIMEOUT_S: f64 = 240.0;
pub const MIN_INTERVAL_S: f64 = 1.0;
pub const NAME_MAX: usize = 18;
pub const SUMMARY_MAX: usize = 80;

/// ssh 目标（env 优先；未配置返回空串 —— 不猜）。
fn ssh_target() -> String {
    np::env_stripped(ENV_SSH_TARGET).unwrap_or_default()
}

/// 私钥/known_hosts 放**配置目录下**而不是 `~/.ssh`：公网实例带 `ProtectHome=yes`，
/// 家目录被隐藏，默认私钥路径在沙箱里读不到，显式 `-i` 才是两个实例都能跑的写法。
fn key_file() -> String {
    format!("{}/ssh/id_ed25519", np::etc_dir(None))
}

fn known_hosts() -> String {
    format!("{}/ssh/known_hosts", np::etc_dir(None))
}

/// 目标机上要跑的那条命令（执行器路径按**目标主机档案**取，不取本机路径）。
fn remote_cmd(target: &str) -> String {
    // ⚠️ 远端执行器必须用 `python -X utf8` 起：本机 Windows 上 python 的 stdin/stdout
    // 被 ssh 接到管道时按 cp936 走，中文提示词与中文回答会在两端各烂一次。
    format!(
        "python -X utf8 {} --timeout 150 -",
        npf::target_claude_exec(target)
    )
}

/// 富化总开关（`CHAT_ENRICH_ENABLE=0/false/no/off` 关）。
pub fn enabled() -> bool {
    let raw = np::env_stripped(ENABLE_ENV).unwrap_or_else(|| "1".to_string());
    let raw = raw.to_lowercase();
    !matches!(raw.as_str(), "0" | "false" | "no" | "off")
}

/// 单次调用超时（夹在 5..1800）。
pub fn timeout_s() -> f64 {
    let value = np::env_stripped(TIMEOUT_ENV)
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(TIMEOUT_S);
    value.clamp(5.0, 1800.0)
}

/// 两次调用的最小间隔（>= 0）。
pub fn min_interval_s() -> f64 {
    let value = np::env_stripped(MIN_INTERVAL_ENV)
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(MIN_INTERVAL_S);
    if value < 0.0 {
        0.0
    } else {
        value
    }
}

/// 本次调用的 ssh 目标（`CHAT_ENRICH_SSH` > `AGENT_TEAM_SSH_TARGET`）。
pub fn target() -> String {
    np::env_stripped(SSH_ENV).unwrap_or_else(ssh_target)
}

/// 模型调用的完整命令行（ssh → 目标机上的 `claude_exec.py`），提示词走 stdin。
///
/// **选项顺序是契约的一部分**（排障时人要照着敲同一条命令），别重排。
/// 远端命令默认值按**当次生效的目标主机**取 —— 换了目标，路径跟着换。
pub fn ssh_argv() -> Vec<String> {
    let ssh_bin = np::env_stripped(SSH_BIN_ENV).unwrap_or_else(|| SSH_BIN.to_string());
    let target = target();
    let remote = np::env_stripped(CMD_ENV).unwrap_or_else(|| remote_cmd(&target));
    let key = np::env_stripped(KEY_ENV).unwrap_or_else(key_file);
    let known = np::env_stripped(KNOWN_ENV).unwrap_or_else(known_hosts);

    let mut argv = vec![
        ssh_bin,
        "-o".to_string(),
        "BatchMode=yes".to_string(),
        "-o".to_string(),
        "ConnectTimeout=10".to_string(),
    ];
    if !key.is_empty() {
        argv.push("-i".to_string());
        argv.push(key);
        argv.push("-o".to_string());
        argv.push("IdentitiesOnly=yes".to_string());
    }
    if !known.is_empty() {
        argv.push("-o".to_string());
        argv.push(format!("UserKnownHostsFile={known}"));
    }
    argv.push(target);
    argv.push(remote);
    argv
}

// ---------------------------------------------------------------- 输出解析

/// 判定里的名字要剥掉的装饰字符（与 Python `str.strip(...)` 同一套字符）。
const NAME_TRIM: &[char] = &[
    '，', ',', '。', '.', '、', ':', '：', ';', '；', '"', '\'', '“', '”', '（', '）', '(', ')',
    '【', '】', '[', ']',
];

/// 模型输出 → 校验过的判定。**不合格一律返回 `None`**（调用方据此重试 / 降级）。
pub fn parse(raw: &str) -> Option<Value> {
    let text = raw.trim();
    if text.is_empty() {
        return None;
    }
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    if end <= start {
        return None;
    }
    let data: Value = serde_json::from_str(&text[start..=end]).ok()?;
    let obj = data.as_object()?;
    if !obj.contains_key("is_task") {
        return None;
    }

    let flag = match obj.get("is_task") {
        Some(Value::String(s)) => {
            matches!(
                s.trim().to_lowercase().as_str(),
                "true" | "1" | "yes" | "y" | "是"
            )
        }
        Some(v) => py_truthy(v),
        None => false,
    };

    let name = collapse_ws(&py_or_str(obj.get("task_name")));
    let name = name.trim_matches(|c| NAME_TRIM.contains(&c));
    let summary = collapse_ws(&py_or_str(obj.get("summary")));
    if flag && name.is_empty() {
        return None; // 判是任务却不给名字：不算合格输出
    }
    let conf = to_float(obj.get("confidence")).clamp(0.0, 1.0);

    Some(json!({
        "is_task": flag,
        "task_name": name.chars().take(NAME_MAX).collect::<String>(),
        "summary": summary.chars().take(SUMMARY_MAX).collect::<String>(),
        "confidence": conf,
        "raw": text.chars().take(600).collect::<String>(),
    }))
}

/// Python `re.sub(r"\s+", " ", s).strip()` 的等价物。
fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Python `str(x or "")`：falsy 一律成空串。
fn py_or_str(v: Option<&Value>) -> String {
    match v {
        None => String::new(),
        Some(v) => {
            if py_truthy(v) {
                py_str(v)
            } else {
                String::new()
            }
        }
    }
}

/// Python `str(x)` 对 JSON 值的对应物。
fn py_str(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Bool(b) => if *b { "True" } else { "False" }.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Python 真值判定的对应物。
fn py_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// Python `float(x)`（失败 → 0.0）。
fn to_float(v: Option<&Value>) -> f64 {
    match v {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0),
        Some(Value::Bool(b)) => {
            if *b {
                1.0
            } else {
                0.0
            }
        }
        Some(Value::String(s)) => s.trim().parse::<f64>().unwrap_or(0.0),
        _ => 0.0,
    }
}

// ---------------------------------------------------------------- 子进程调用

enum RunOutcome {
    Ok {
        code: i32,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
    },
    Timeout,
    SpawnError(std::io::Error),
}

/// 跑一次子进程，提示词走 stdin，stdout/stderr 并发读走（避免管道缓冲堵死），
/// 到点未退则 kill。**对应 Python `subprocess.run(..., timeout=...)`。**
fn run_with_timeout(argv: &[String], input: &[u8], timeout: Duration) -> RunOutcome {
    let mut child = match Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return RunOutcome::SpawnError(e),
    };

    let mut stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let payload = input.to_vec();
    let writer = std::thread::spawn(move || {
        if let Some(si) = stdin.as_mut() {
            use std::io::Write;
            let _ = si.write_all(&payload);
            let _ = si.flush();
        }
        // stdin 在此 drop ⇒ 关闭管道，远端不会一直等输入
    });
    let out_reader = stdout.map(spawn_reader);
    let err_reader = stderr.map(spawn_reader);

    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Some(st),
            Ok(None) => {
                if Instant::now() >= deadline {
                    timed_out = true;
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };

    let _ = writer.join();
    let stdout = out_reader
        .map(|h| h.join().unwrap_or_default())
        .unwrap_or_default();
    let stderr = err_reader
        .map(|h| h.join().unwrap_or_default())
        .unwrap_or_default();

    if timed_out {
        return RunOutcome::Timeout;
    }
    RunOutcome::Ok {
        code: status.and_then(|s| s.code()).unwrap_or(-1),
        stdout,
        stderr,
    }
}

fn spawn_reader<R: std::io::Read + Send + 'static>(
    mut reader: R,
) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = reader.read_to_end(&mut buf);
        buf
    })
}

/// 把提示词交给模型，返回 `(判定, 说明)`。判定 `None` = 这次失败（原因写在说明里）。
///
/// **失败是正常路径**：超时 / ssh 不通 / 执行器不存在 / 输出不是 JSON —— 全部只返回 `None`。
pub fn call(prompt: &str, timeout: Option<f64>) -> (Option<Value>, String) {
    if !enabled() {
        return (None, "disabled".to_string());
    }
    let argv = ssh_argv();
    let timeout = timeout.unwrap_or_else(timeout_s);
    let started = Instant::now();

    let outcome = run_with_timeout(&argv, prompt.as_bytes(), Duration::from_secs_f64(timeout));
    let (code, stdout, stderr) = match outcome {
        RunOutcome::Timeout => return (None, format!("timeout_{}s", timeout as i64)),
        RunOutcome::SpawnError(e) => {
            let head = argv.first().cloned().unwrap_or_default();
            // 对应 Python 的 FileNotFoundError 分支
            if e.kind() == std::io::ErrorKind::NotFound {
                return (None, format!("ssh_missing:{head}"));
            }
            let detail = e.to_string();
            let msg = if detail.trim().is_empty() {
                std::io::Error::from(e.kind()).to_string()
            } else {
                detail
            };
            return (None, format!("spawn_error:{}", safe_err(&msg)));
        }
        RunOutcome::Ok {
            code,
            stdout,
            stderr,
        } => (code, stdout, stderr),
    };

    let out = String::from_utf8_lossy(&stdout).trim().to_string();
    let err = String::from_utf8_lossy(&stderr).trim().to_string();
    if code != 0 {
        let tail: String = if err.is_empty() { out } else { err };
        return (None, format!("exit_{}:{}", code, tail_chars(&tail, 200)));
    }
    match parse(&out) {
        None => (None, format!("unparsable:{}", tail_chars(&out, 200))),
        Some(mut verdict) => {
            if let Some(obj) = verdict.as_object_mut() {
                obj.insert(
                    "elapsed_s".to_string(),
                    json!(round2(started.elapsed().as_secs_f64())),
                );
            }
            (Some(verdict), "ok".to_string())
        }
    }
}

/// 末尾 n **个字符**（对应 Python 的 `s[-n:]` 切片，按字符不按字节）。
fn tail_chars(s: &str, n: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    let start = chars.len().saturating_sub(n);
    chars[start..].iter().collect()
}

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

// -------------------------------------------------------------------- 通道自检

/// 自检用的最小提示词：回答内容固定、不含业务语义，只用来证明「这条通道能通且能回 JSON」。
pub const PROBE_PROMPT: &str = "只回一行 JSON，不要任何解释、不要代码块：\
{\"is_task\": false, \"task_name\": \"\", \"summary\": \"\", \"confidence\": 0.9}";

/// 真打一次最小 prompt，报告这条模型通道通不通。**不写库、不改状态、不抛异常。**
///
/// 返回 `{"ok","target","argv","elapsed_s","why","parsed"}`。
/// 目标主机没登记在档案表里时，`why` 额外挂一句 `target_unknown`，提醒远端路径是兜底值。
pub fn probe(timeout_secs: f64) -> Value {
    let target = target();
    let argv = ssh_argv();
    let started = Instant::now();
    let (verdict, mut why) = call(PROBE_PROMPT, Some(timeout_secs));
    if !npf::target_known(&target) {
        why = format!("{why}|target_unknown:{target}");
    }
    json!({
        "ok": verdict.is_some(),
        "target": target,
        "argv": argv,
        "elapsed_s": round2(started.elapsed().as_secs_f64()),
        "why": why,
        "parsed": verdict,
    })
}

// ---------------------------------------------------------------- Agent 展示名

/// 成员显示名补充表（`agents.json` 里只有 description 时用；键 = 成员 id）。
pub const DISPLAY_NAMES_FALLBACK: &[(&str, &str)] = &[
    ("lobster", "龙虾"),
    ("quantum", "量子"),
    ("top001", "Top001"),
    ("openclaw", "OpenClaw"),
    ("openclaw-nb", "笔记本 OpenClaw"),
    ("claudecode", "Claude Code"),
    ("codex", "Codex"),
    ("piagent", "Pi Agent"),
    ("hermes", "Hermes"),
];

/// 展示名表（兜底表 + `AGENT_TEAM_DISPLAY_NAMES` 覆盖）。
///
/// Python 在 import 期算一次；Rust 侧按 env 值记忆（env 变了自动重建），
/// 既避免每次调用重解析 JSON，又让测试可控。
type NameTable = Vec<(String, String)>;

pub fn display_names() -> NameTable {
    static CACHE: OnceLock<Mutex<Option<(String, NameTable)>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(None));
    let override_raw = np::env_stripped("AGENT_TEAM_DISPLAY_NAMES").unwrap_or_default();
    let mut guard = cache.lock().unwrap();
    if let Some((cached_raw, table)) = guard.as_ref() {
        if *cached_raw == override_raw {
            return table.clone();
        }
    }
    let mut table: Vec<(String, String)> = DISPLAY_NAMES_FALLBACK
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect();
    if !override_raw.is_empty() {
        if let Ok(Value::Object(map)) = serde_json::from_str::<Value>(&override_raw) {
            for (k, v) in map {
                let value = v
                    .as_str()
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| py_str(&v));
                match table.iter_mut().find(|(key, _)| *key == k) {
                    Some(entry) => entry.1 = value,
                    None => table.push((k, value)),
                }
            }
        }
    }
    *guard = Some((override_raw, table.clone()));
    table
}

/// 成员显示名：补充表 > `id` 原样。
pub fn display_name(member_id: &str) -> String {
    if member_id.is_empty() {
        return String::new();
    }
    display_names()
        .into_iter()
        .find(|(k, _)| k == member_id)
        .map(|(_, v)| v)
        .unwrap_or_else(|| member_id.to_string())
}
