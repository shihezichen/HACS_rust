//! `cli_backend` —— 本机 CLI 成员后端（端口 9970）。对应 Python `node/l2_adapter/cli_backend.py`。
//!
//! 第一个走「组合根 + `[[bin]]`」装配路径的件：
//!   * HTTP 面用 L1 `web_gateway`（`sys_version=""` + HTTP/1.1，保 keep-alive 语义）；
//!   * 子进程复用 L2 `agent_model` 那套「并发读 + 到点 kill」跑法（本文件自带 `run_cli`，
//!     因 Python 原件同样内联实现 —— 不为复用破坏既有判据）；
//!   * 纯逻辑（clean_output / build_prompt / resolve_model / 会话映射）在
//!     `hacs-l2-adapter::cli_backend`，本文件只做装配与注入。

use std::collections::HashMap;
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use hacs_l0_host::node_paths as np;
use hacs_l0_host::node_profile as npf;
use hacs_l1_transport::web_gateway::{self, Body, Gateway, Reply};
use hacs_l2_adapter::cli_backend as cb;
use serde_json::{json, Value};

// ---------------------------------------------------------------- Runtime

/// 每个成员一把锁 + busy 标志；`lock.acquire(timeout=...)` 用 Condvar 实现。
pub struct MemberGate {
    inner: Mutex<Inner>,
    cv: Condvar,
}

#[derive(Default)]
struct Inner {
    locked: bool,
    busy: bool,
}

impl MemberGate {
    fn new() -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            cv: Condvar::new(),
        }
    }

    /// 对应 Python `lock.acquire(timeout=busy_wait_s)`；取不到返回 None → 503。
    fn acquire(&self, wait: Duration) -> Option<MemberGuard<'_>> {
        let deadline = Instant::now() + wait;
        let mut inner = self.inner.lock().unwrap();
        while inner.locked {
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            let (guard, _timeout) = self
                .cv
                .wait_timeout(inner, deadline - now)
                .unwrap();
            inner = guard;
        }
        inner.locked = true;
        inner.busy = true;
        Some(MemberGuard { gate: self })
    }

    fn is_busy(&self) -> bool {
        self.inner.lock().unwrap().busy
    }

    fn release(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.locked = false;
        inner.busy = false;
        self.cv.notify_one();
    }
}

struct MemberGuard<'a> {
    gate: &'a MemberGate,
}

impl Drop for MemberGuard<'_> {
    fn drop(&mut self) {
        self.gate.release();
    }
}

/// 进程级运行时配置（由 main 填充，handler 只读；测试可直接构造）。
pub struct Runtime {
    pub api_key: String,
    /// 假 CLI 注入（argv 前缀），None = 走真实 CLI。
    pub pi_cmd: Option<Vec<String>>,
    pub claude_cmd: Option<Vec<String>>,
    pub codex_cmd: Option<Vec<String>>,
    pub pi_workdir: String,
    pub claude_workdir: String,
    pub codex_workdir: String,
    pub claude_python: String,
    pub claude_exec: String,
    pub busy_wait: Duration,
    pub gates: HashMap<&'static str, MemberGate>,
    pub sessions: cb::SessionRegistry,
}

impl Default for Runtime {
    fn default() -> Self {
        Self::new()
    }
}

impl Runtime {
    pub fn new() -> Self {
        let gates = cb::MEMBERS
            .iter()
            .map(|m| (*m, MemberGate::new()))
            .collect();
        Self {
            api_key: String::new(),
            pi_cmd: None,
            claude_cmd: None,
            codex_cmd: None,
            pi_workdir: np::pi_workdir(None),
            claude_workdir: np::claude_workdir(None),
            codex_workdir: np::env_stripped("AGENT_TEAM_CODEX_WORKDIR")
                .or_else(|| np::env_stripped("CODEX_WORKDIR"))
                .unwrap_or_else(npf::codex_workdir_default),
            claude_python: "python3".to_string(),
            claude_exec: np::claude_exec(None),
            busy_wait: Duration::from_secs_f64(cb::BUSY_WAIT_S),
            gates,
            sessions: cb::SessionRegistry::new(),
        }
    }
}

// ---------------------------------------------------------------- 子进程

struct CliResult {
    rc: i32,
    stdout: String,
    stderr: String,
    pid: u32,
    timed_out: bool,
    elapsed_ms: u64,
}

/// 杀整棵进程树：posix 用进程组 SIGKILL（spawn 时要求 setpgid）。
#[cfg(unix)]
fn kill_tree(pid: u32) {
    // kill(-pgid, SIGKILL)：子进程在自己独立的进程组里（见 spawn 时的 pre_exec）
    let _ = libc_kill_group(pid as i32);
}

#[cfg(unix)]
fn libc_kill_group(pgid: i32) -> i32 {
    // 裸系统调用，避免引 nix/libc crate：kill(-pgid, 9)
    extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    unsafe { kill(-pgid, 9) }
}

#[cfg(not(unix))]
fn kill_tree(_pid: u32) {}

/// 跑一次 CLI。对应 Python `run_cli()`：Popen + communicate(timeout) + kill_tree。
fn run_cli(argv: &[String], cwd: Option<&str>, timeout_s: f64) -> CliResult {
    let started = Instant::now();
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("NO_COLOR", "1")
        .env("FORCE_COLOR", "0")
        .env("PYTHONIOENCODING", "utf-8")
        .env("PYTHONUTF8", "1");
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    // posix：让子进程自成进程组，超时才能整树杀
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            cmd.pre_exec(|| {
                let _ = setsid_call();
                Ok(())
            });
        }
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return CliResult {
                rc: -1,
                stdout: String::new(),
                stderr: format!("spawn failed: {e}"),
                pid: 0,
                timed_out: false,
                elapsed_ms: 0,
            }
        }
    };

    let pid = child.id();
    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();

    // stdout/stderr 并发读走（否则管道缓冲写满会死等）—— 同 agent_model 的跑法
    let out_handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut p) = stdout_pipe {
            let _ = p.read_to_end(&mut buf);
        }
        buf
    });
    let err_handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut p) = stderr_pipe {
            let _ = p.read_to_end(&mut buf);
        }
        buf
    });

    let deadline = Instant::now() + Duration::from_secs_f64(timeout_s);
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Some(st),
            Ok(None) => {
                if Instant::now() >= deadline {
                    timed_out = true;
                    kill_tree(pid);
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
    let status_code = status.and_then(|s| s.code()).unwrap_or(-1);
    // 杀树后给读线程一点时间收尾；拿不到就当空
    let out = out_handle.join().unwrap_or_default();
    let err = err_handle.join().unwrap_or_default();

    CliResult {
        rc: status_code,
        stdout: cb::clean_output(&String::from_utf8_lossy(&out)),
        stderr: cb::clean_output(&String::from_utf8_lossy(&err)),
        pid,
        timed_out,
        elapsed_ms: started.elapsed().as_millis() as u64,
    }
}

#[cfg(unix)]
unsafe fn setsid_call() -> i32 {
    extern "C" {
        fn setsid() -> i32;
    }
    setsid()
}

// ---------------------------------------------------------------- 成员执行

fn run_piagent(rt: &Runtime, prompt: &str, cli_model: &str, session_key: Option<&str>, timeout_s: f64) -> CliResult {
    if let Some(prefix) = &rt.pi_cmd {
        // 测试注入的假 CLI：前缀 + 任务
        let mut argv = prefix.clone();
        argv.push(prompt.to_string());
        return run_cli(&argv, Some(&rt.pi_workdir), timeout_s);
    }
    let mut argv = vec![
        np::node_exe(None),
        np::pi_cli_js(None),
        "--print".to_string(),
    ];
    if !cli_model.is_empty() {
        argv.push("--model".to_string());
        argv.push(cli_model.to_string());
    }
    if let Some(key) = session_key {
        argv.push("--session-id".to_string());
        argv.push(cb::safe_session_id(key));
    } else {
        argv.push("--no-session".to_string());
    }
    argv.push("--".to_string());
    argv.push(prompt.to_string());
    run_cli(&argv, Some(&rt.pi_workdir), timeout_s)
}

fn run_claudecode(rt: &Runtime, prompt: &str, cli_model: &str, session_key: Option<&str>, timeout_s: f64) -> CliResult {
    if let Some(prefix) = &rt.claude_cmd {
        let mut argv = prefix.clone();
        argv.push(prompt.to_string());
        return run_cli(&argv, Some(&rt.claude_workdir), timeout_s);
    }
    // 任务用 --file 传临时文件，避免超长命令行与引号转义
    let tmp = std::env::temp_dir().join(format!("cli_backend_{}.txt", std::process::id()));
    if std::fs::write(&tmp, prompt.as_bytes()).is_err() {
        return CliResult {
            rc: -1,
            stdout: String::new(),
            stderr: "spawn failed: cannot write task file".into(),
            pid: 0,
            timed_out: false,
            elapsed_ms: 0,
        };
    }

    let build = |resume: bool, sid: Option<&str>| -> Vec<String> {
        let mut argv = vec![
            rt.claude_python.clone(),
            rt.claude_exec.clone(),
            "--file".to_string(),
            tmp.to_string_lossy().to_string(),
            "--workdir".to_string(),
            rt.claude_workdir.clone(),
            // claude_exec 自己的超时放宽，让外层 kill_tree 先生效 → 统一 504 语义
            "--timeout".to_string(),
            format!("{}", timeout_s as i64 + 30),
        ];
        if !cli_model.is_empty() {
            argv.push("--model".to_string());
            argv.push(cli_model.to_string());
        }
        if let Some(s) = sid {
            // 必须写成 --opt=value 的**单 token**（分两个 token 会被 claude_exec
            // 的 argparse 把值吃掉 —— 实测报 "argument missing"）。
            argv.push(format!("--{}={}", if resume { "resume" } else { "session-id" }, s));
        }
        argv
    };

    let sid = session_key.map(cb::claude_session_uuid);
    let mut resume = false;
    if let Some(s) = &sid {
        resume = rt.sessions.claude_seen_or_add(s);
    }
    let mut res = run_cli(&build(resume, sid.as_deref()), Some(&rt.claude_workdir), timeout_s);
    // --session-id 撞上「已存在」→ 自动改 --resume 重试一次
    if let Some(s) = &sid {
        if !resume && res.rc != 0 && res.stdout.is_empty() && is_already_exists(&res.stderr) {
            res = run_cli(&build(true, Some(s)), Some(&rt.claude_workdir), timeout_s);
        }
    }
    let _ = std::fs::remove_file(&tmp);
    res
}

fn is_already_exists(stderr: &str) -> bool {
    let low = stderr.to_lowercase();
    low.contains("already") || low.contains("exist") || low.contains("in use")
}

/// codex 可执行文件解析：① CODEX_CLI ② 原生 exe ③ codex.cmd（经 cmd /c）。
fn codex_executable() -> Vec<String> {
    if let Some(override_cmd) = np::env_stripped("CODEX_CLI") {
        let o = override_cmd.trim_matches('"').to_string();
        let low = o.to_lowercase();
        if low.ends_with(".cmd") || low.ends_with(".bat") {
            return ["cmd".to_string(), "/c".to_string(), o].into();
        }
        return vec![o];
    }
    let exe = np::codex_exe(None);
    if std::path::Path::new(&exe).is_file() {
        return vec![exe];
    }
    ["cmd".to_string(), "/c".to_string(), np::codex_cmd(None)].into()
}

/// codex 的工作目录；不存在退回 HOME（codex 在不存在的 cwd 里根本起不来）。
fn codex_workdir(rt: &Runtime) -> String {
    if !rt.codex_workdir.is_empty() && std::path::Path::new(&rt.codex_workdir).is_dir() {
        return rt.codex_workdir.clone();
    }
    np::home()
}

/// 从 codex 的 stderr banner 里抓会话 id 并登记（含落盘）。
fn remember_codex_session(rt: &Runtime, session_key: Option<&str>, stderr_text: &str) -> Option<String> {
    let key = session_key?;
    let found = codex_session_id(stderr_text)?;
    Some(rt.sessions.codex_setdefault(key, &found))
}

/// banner 里的 `session id: <uuid>`。
fn codex_session_id(stderr_text: &str) -> Option<String> {
    // 与 Python CODEX_SESSION_RE 同口径：`session id:` 后跟一个 UUID 形态
    let low_idx = stderr_text.to_lowercase().find("session id:")?;
    let rest = &stderr_text[low_idx + "session id:".len()..];
    let trimmed = rest.trim_start();
    let hex: String = trimmed
        .chars()
        .take_while(|c| c.is_ascii_hexdigit() || *c == '-')
        .collect();
    // 校验 UUID 形态（8-4-4-4-12）
    let parts: Vec<&str> = hex.split('-').collect();
    if parts.len() == 5
        && [8, 4, 4, 4, 12]
            .iter()
            .zip(&parts)
            .all(|(n, p)| p.len() == *n && p.chars().all(|c| c.is_ascii_hexdigit()))
    {
        Some(hex.to_lowercase())
    } else {
        None
    }
}

fn run_codex(rt: &Runtime, prompt: &str, cli_model: &str, session_key: Option<&str>, timeout_s: f64) -> CliResult {
    if let Some(prefix) = &rt.codex_cmd {
        let mut argv = prefix.clone();
        argv.push(prompt.to_string());
        return run_cli(&argv, Some(&codex_workdir(rt)), timeout_s);
    }
    let workdir = codex_workdir(rt);
    let last_msg = std::env::temp_dir().join(format!("cli_backend_codex_{}.txt", std::process::id()));

    // 首次使用时把磁盘映射读进内存缓存（重启后 codex 会话仍能续上）
    if !rt.sessions.codex_is_loaded() {
        let path = codex_sessions_path(rt);
        rt.sessions.codex_merge(cb::load_codex_sessions(&path));
        rt.sessions.codex_mark_loaded();
    }
    let sid = session_key.and_then(|k| rt.sessions.codex_get(k));

    let build = |sid: &Option<String>, skip_git: bool| -> Vec<String> {
        let mut argv = codex_executable();
        argv.push("exec".to_string());
        if sid.is_some() {
            argv.push("resume".to_string());
        }
        argv.push("-o".to_string());
        argv.push(last_msg.to_string_lossy().to_string());
        if !cli_model.is_empty() {
            argv.push("--model".to_string());
            argv.push(cli_model.to_string());
        }
        if skip_git {
            argv.push("--skip-git-repo-check".to_string());
        }
        if session_key.is_none() {
            argv.push("--ephemeral".to_string());
        }
        argv.push("--".to_string());
        if let Some(s) = sid {
            argv.push(s.clone());
        }
        argv.push(prompt.to_string());
        argv
    };

    let mut res = run_cli(&build(&sid, false), Some(&workdir), timeout_s);
    // 非 git 仓库/未受信任目录 → 加 --skip-git-repo-check 重试一次
    if !res.timed_out
        && res.rc != 0
        && res.stdout.is_empty()
        && codex_untrusted(&res.stderr)
    {
        res = run_cli(&build(&sid, true), Some(&workdir), timeout_s);
    }

    if !res.timed_out && res.rc == 0 {
        if session_key.is_some() && sid.is_none() {
            let _ = remember_codex_session(rt, session_key, &res.stderr);
        }
        // 正文优先取 -o 文件（最稳），取不到才退回 stdout
        let final_out = std::fs::read_to_string(&last_msg)
            .map(|s| cb::clean_output(&s))
            .unwrap_or_default();
        if !final_out.is_empty() {
            res.stdout = final_out;
        }
    }
    let _ = std::fs::remove_file(&last_msg);
    res
}

/// codex 会话映射的持久化路径（env 覆盖 > Runtime 注入 > workdir 下默认名）。
fn codex_sessions_path(rt: &Runtime) -> String {
    if let Some(v) = np::env_stripped("CODEX_SESSIONS_FILE") {
        return v;
    }
    if let Some(v) = rt.sessions.codex_sessions_file.lock().unwrap().clone() {
        return v;
    }
    hacs_l0_host::node_paths::join(&codex_workdir(rt), ".codex_sessions.json")
}

/// codex 拒绝在「非 git 仓库/未受信任目录」里跑时的报错特征。
fn codex_untrusted(stderr: &str) -> bool {
    let low = stderr.to_lowercase();
    low.contains("skip-git-repo-check")
        || low.contains("skip_git_repo_check")
        || low.contains("trusted director")
        || low.contains("git repo")
}

// ---------------------------------------------------------------- HTTP 分派

fn need_auth(method: &str, path: &str) -> bool {
    matches!(
        (method, path),
        ("GET", "/v1/models")
            | ("GET", "/models")
            | ("POST", "/v1/chat/completions")
            | ("POST", "/chat/completions")
    )
}

fn authed(req: &web_gateway::Request, api_key: &str) -> bool {
    let Some(header) = req.header("Authorization") else {
        return false;
    };
    let Some(token) = header.strip_prefix("Bearer ") else {
        return false;
    };
    let token = token.trim();
    if token.is_empty() || api_key.is_empty() {
        return false;
    }
    // 常量时间比较（对应 hmac.compare_digest）
    constant_time_eq(token.as_bytes(), api_key.as_bytes())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

fn build_gateway(rt: Arc<Runtime>) -> Gateway {
    let mut gw = Gateway::new("cli_backend", Some(cb::VERSION), true);
    gw.protocol_version = "HTTP/1.1".to_string(); // keep-alive；sys_version 已是 ""

    let rt_auth = rt.clone();
    gw.auth(move |req| {
        if need_auth(&req.method, &req.path) && !authed(req, &rt_auth.api_key) {
            return Ok(Some(Reply::json(
                401,
                json!({"ok": false, "error": "unauthorized"}),
            )));
        }
        Ok(None)
    });

    // 本件的 404 不带 path 字段（照迁移前的字面量回）
    gw.not_found(|_req| {
        Ok(Reply::json(404, json!({"ok": false, "error": "not_found"})))
    });

    gw.route("/healthz", &["GET"], false, {
        let rt = rt.clone();
        move |_req| {
            Ok(Reply::json(
                200,
                json!({
                    "ok": true,
                    "service": "cli_backend",
                    "version": cb::VERSION,
                    "members": cb::MEMBERS,
                    "busy": {
                        "piagent": rt.gates["piagent"].is_busy(),
                        "claudecode": rt.gates["claudecode"].is_busy(),
                        "codex": rt.gates["codex"].is_busy(),
                    },
                    "ts": unix_now(),
                }),
            ))
        }
    });

    gw.route("/v1/models", &["GET"], false, |_req| {
        let now = unix_now();
        let data: Vec<Value> = cb::MODEL_IDS
            .iter()
            .map(|mid| {
                json!({"id": mid, "object": "model", "created": now, "owned_by": "cli_backend"})
            })
            .collect();
        Ok(Reply::json(200, json!({"object": "list", "data": data})))
    });
    gw.route("/models", &["GET"], false, |_req| {
        let now = unix_now();
        let data: Vec<Value> = cb::MODEL_IDS
            .iter()
            .map(|mid| {
                json!({"id": mid, "object": "model", "created": now, "owned_by": "cli_backend"})
            })
            .collect();
        Ok(Reply::json(200, json!({"object": "list", "data": data})))
    });

    gw.route("/v1/chat/completions", &["POST"], false, {
        let rt = rt.clone();
        move |req| Ok(reply_from(chat_completions(&rt, req)))
    });
    gw.route("/chat/completions", &["POST"], false, {
        let rt = rt.clone();
        move |req| Ok(reply_from(chat_completions(&rt, req)))
    });

    gw
}

type ApiReply = (u16, Value);

fn reply_from((status, body): ApiReply) -> Reply {
    Reply::json(status, body)
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 鉴权之后的核心处理（对应 Python `_chat_completions` → `_dispatch`）。
fn chat_completions(rt: &Runtime, req: &web_gateway::Request) -> ApiReply {
    let body: Value = match req.json_body(cb::MAX_BODY_BYTES) {
        Ok(v) => v,
        Err(_) => {
            return (400, json!({"ok": false, "error": "bad_request", "detail": "invalid json"}))
        }
    };
    if !body.is_object() {
        return (400, json!({"ok": false, "error": "bad_request"}));
    }

    let model = body.get("model").and_then(Value::as_str).unwrap_or("");
    if model.trim().is_empty() {
        return (400, json!({"ok": false, "error": "bad_request", "detail": "missing model"}));
    }
    let Some((member, cli_model)) = cb::resolve_model(model) else {
        return (
            404,
            json!({"ok": false, "error": "unknown_model",
                   "detail": format!("supported: {}", cb::MODEL_IDS.join(","))}),
        );
    };

    let Some(messages) = body.get("messages").filter(|m| m.is_array()) else {
        return (400, json!({"ok": false, "error": "bad_request", "detail": "missing messages"}));
    };
    if messages.as_array().unwrap().is_empty() {
        return (400, json!({"ok": false, "error": "bad_request", "detail": "missing messages"}));
    }
    let Some(prompt) = cb::build_prompt(messages) else {
        return (400, json!({"ok": false, "error": "bad_request", "detail": "no user content"}));
    };

    let timeout_s = match cb::clamp_timeout(body.get("timeout_s")) {
        Some(t) => t,
        None => return (400, json!({"ok": false, "error": "bad_request", "detail": "bad timeout_s"})),
    };

    let session_key = body
        .get("user")
        .and_then(Value::as_str)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    dispatch(rt, &member, &cli_model, model, &prompt, session_key.as_deref(), timeout_s)
}

/// 同一成员串行（各自一把锁，成员之间可并行）；排队等不到 → 503 busy。
fn dispatch(
    rt: &Runtime,
    member: &str,
    cli_model: &str,
    model: &str,
    prompt: &str,
    session_key: Option<&str>,
    timeout_s: f64,
) -> ApiReply {
    let gate = &rt.gates[match member {
        "piagent" => "piagent",
        "claudecode" => "claudecode",
        "codex" => "codex",
        _ => return (502, json!({"ok": false, "error": "backend_error", "detail": "unknown member"})),
    }];
    let Some(_guard) = gate.acquire(rt.busy_wait) else {
        log_line(&format!(
            "model={model} member={member} task={} code=503 rc=- pid=- elapsed_ms=0 busy",
            cb::task_head(prompt)
        ));
        return (
            503,
            json!({"ok": false, "error": "busy", "detail": format!("{member} 正在执行其它任务")}),
        );
    };
    let res = match member {
        "piagent" => run_piagent(rt, prompt, cli_model, session_key, timeout_s),
        "claudecode" => run_claudecode(rt, prompt, cli_model, session_key, timeout_s),
        _ => run_codex(rt, prompt, cli_model, session_key, timeout_s),
    };

    let code: u16 = if res.timed_out {
        504
    } else if res.rc != 0 || res.stdout.is_empty() {
        502
    } else {
        200
    };
    log_line(&format!(
        "model={model} member={member} task={} elapsed_ms={} code={} rc={} pid={}",
        cb::task_head(prompt),
        res.elapsed_ms,
        code,
        res.rc,
        res.pid
    ));

    if code == 504 {
        return (
            504,
            json!({"ok": false, "error": "backend_timeout",
                   "detail": format!("{member} 超时 {:.0}s，已杀进程树 pid={}", timeout_s, res.pid)}),
        );
    }
    if code == 502 {
        // 绝不编造回复：非零退出或空输出一律如实报错，detail 带 stderr 尾部
        let tail = tail_chars(&res.stderr, cb::DETAIL_MAX);
        let detail = if tail.is_empty() {
            format!("rc={} 且输出为空", res.rc)
        } else {
            tail
        };
        return (502, json!({"ok": false, "error": "backend_error", "detail": detail}));
    }

    (
        200,
        json!({
            "id": format!("chatcmpl-cli-{}", rand_hex16()),
            "object": "chat.completion",
            "created": unix_now(),
            "model": model,
            "choices": [{"index": 0, "finish_reason": "stop",
                         "message": {"role": "assistant", "content": res.stdout}}],
            "usage": {"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0},
        }),
    )
}

/// 末尾 n 个字符（对应 Python `s[-n:]`，按字符不按字节）。
fn tail_chars(s: &str, n: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    let start = chars.len().saturating_sub(n);
    chars[start..].iter().collect()
}

fn rand_hex16() -> String {
    // 16 hex chars：时间 + 地址 + pid 混合（urandom 取不到时也能给出唯一形态）
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let mut buf = [0u8; 8];
        if f.read_exact(&mut buf).is_ok() {
            return buf.iter().map(|b| format!("{b:02x}")).collect();
        }
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!("{:016x}", nanos as u64 ^ (std::process::id() as u64) << 32)
}

// ---------------------------------------------------------------- 日志 / 入口

fn log_line(message: &str) {
    let ts = unix_now();
    println!("{ts} INFO {message}");
}

fn load_key(path: &str) -> Result<String, String> {
    let raw = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let key = raw.trim();
    if key.is_empty() {
        return Err(format!("key 文件为空: {path}"));
    }
    Ok(key.to_string())
}

/// CLI 参数（对应 Python build_parser）。
pub struct Args {
    pub host: String,
    pub port: u16,
    pub key_file: String,
    pub pi_workdir: Option<String>,
    pub claude_workdir: Option<String>,
    pub codex_workdir: Option<String>,
    pub busy_wait: f64,
    pub stub: bool,
    pub pi_cmd: Option<Vec<String>>,
    pub claude_cmd: Option<Vec<String>>,
    pub codex_cmd: Option<Vec<String>>,
}

fn parse_args() -> Result<Args, String> {
    let mut host = "127.0.0.1".to_string();
    let mut port: u16 = 9970;
    let mut key_file = {
        let runtime_dir = np::runtime_dir(None);
        // Python: AGENT_TEAM_PASS_FILE or runtime_dir + "\cli-backend-key"
        // posix 惯例落到 runtime_dir 下
        np::env_stripped("AGENT_TEAM_PASS_FILE")
            .unwrap_or_else(|| np::join(&runtime_dir, "cli-backend-key"))
    };
    let mut pi_workdir = None;
    let mut claude_workdir = None;
    let mut codex_workdir = None;
    let mut busy_wait = cb::BUSY_WAIT_S;
    let mut stub = false;
    let mut pi_cmd = None;
    let mut claude_cmd = None;
    let mut codex_cmd = None;

    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    let need = |args: &[String], i: &mut usize, name: &str| -> Result<String, String> {
        *i += 1;
        args.get(*i)
            .cloned()
            .ok_or_else(|| format!("missing value for {name}"))
    };
    while i < argv.len() {
        let arg = argv[i].as_str();
        match arg {
            "--host" => host = need(&argv, &mut i, "--host")?,
            "--port" => port = need(&argv, &mut i, "--port")?.parse().map_err(|_| "bad --port")?,
            "--key-file" => key_file = need(&argv, &mut i, "--key-file")?,
            "--pi-workdir" => pi_workdir = Some(need(&argv, &mut i, "--pi-workdir")?),
            "--claude-workdir" => claude_workdir = Some(need(&argv, &mut i, "--claude-workdir")?),
            "--codex-workdir" => codex_workdir = Some(need(&argv, &mut i, "--codex-workdir")?),
            "--busy-wait" => busy_wait = need(&argv, &mut i, "--busy-wait")?.parse().map_err(|_| "bad --busy-wait")?,
            "--stub" => stub = true,
            "--pi-cmd" => pi_cmd = Some(parse_json_argv(&need(&argv, &mut i, "--pi-cmd")?)?),
            "--claude-cmd" => claude_cmd = Some(parse_json_argv(&need(&argv, &mut i, "--claude-cmd")?)?),
            "--codex-cmd" => codex_cmd = Some(parse_json_argv(&need(&argv, &mut i, "--codex-cmd")?)?),
            other => return Err(format!("unknown argument: {other}")),
        }
        i += 1;
    }
    Ok(Args {
        host,
        port,
        key_file,
        pi_workdir,
        claude_workdir,
        codex_workdir,
        busy_wait,
        stub,
        pi_cmd,
        claude_cmd,
        codex_cmd,
    })
}

fn parse_json_argv(raw: &str) -> Result<Vec<String>, String> {
    let v: Value = serde_json::from_str(raw).map_err(|e| format!("bad argv json: {e}"))?;
    v.as_array()
        .ok_or("argv json must be array")?
        .iter()
        .map(|x| {
            x.as_str()
                .map(|s| s.to_string())
                .ok_or_else(|| "argv items must be strings".to_string())
        })
        .collect()
}

/// stub 模式：三个成员都换成内置回声桩（不碰真实 CLI）。
fn stub_cmd() -> Vec<String> {
    // /bin/sh -c 'echo "[stub] $*"' 不行（$* 会带上 -c 之类）……直接用一个小脚本语义：
    // argv 末位是任务，原样回一句。用 sh 的 eval 组合。
    [
        "sh".to_string(),
        "-c".to_string(),
        "printf '%s\\n' \"[stub] $(eval echo \\\"\\${@: -1}\\\")\"".to_string(),
        "stub".to_string(),
    ]
    .into()
}

fn main() -> std::process::ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("cli_backend: {e}");
            return std::process::ExitCode::from(2);
        }
    };

    let mut rt = Runtime::new();
    rt.busy_wait = Duration::from_secs_f64(args.busy_wait);
    if let Some(d) = args.pi_workdir {
        rt.pi_workdir = d;
    }
    if let Some(d) = args.claude_workdir {
        rt.claude_workdir = d;
    }
    if let Some(d) = args.codex_workdir {
        rt.codex_workdir = d;
    }

    match load_key(&args.key_file) {
        Ok(key) => rt.api_key = key,
        Err(e) => {
            log_line(&format!("读取 key 文件失败，拒绝启动（fail closed）: {e}"));
            return std::process::ExitCode::from(2);
        }
    }

    if args.stub {
        let cmd = stub_cmd();
        rt.pi_cmd = Some(cmd.clone());
        rt.claude_cmd = Some(cmd.clone());
        rt.codex_cmd = Some(cmd);
    }
    rt.pi_cmd = args.pi_cmd.or(rt.pi_cmd);
    rt.claude_cmd = args.claude_cmd.or(rt.claude_cmd);
    rt.codex_cmd = args.codex_cmd.or(rt.codex_cmd);

    for directory in [&rt.pi_workdir, &rt.claude_workdir, &rt.codex_workdir] {
        if let Err(e) = std::fs::create_dir_all(directory) {
            log_line(&format!("工作目录不可用 {directory}: {e}"));
        }
    }

    if !matches!(args.host.as_str(), "127.0.0.1" | "localhost" | "::1") {
        log_line(&format!("只允许绑 loopback，拒绝监听 {}", args.host));
        return std::process::ExitCode::from(2);
    }

    let rt = Arc::new(rt);
    let gw = build_gateway(rt);
    log_line(&format!(
        "cli_backend {} listening on {}:{} members={} stub={}",
        cb::VERSION,
        args.host,
        args.port,
        cb::MEMBERS.join(","),
        "false"
    ));
    match gw.serve(&args.host, args.port) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            log_line(&format!("serve 失败: {e}"));
            std::process::ExitCode::from(2)
        }
    }
}

// keep Body imported for parity with gateway types
#[allow(dead_code)]
fn _body_witness(_b: Body) {}
