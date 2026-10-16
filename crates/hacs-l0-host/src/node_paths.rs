//! L0 平台层：宿主默认值的唯一出处（对应 Python `node/l0_host/node_paths.py`）。
//!
//! 规则（唯一一条）：**env 优先 > 平台自适应默认值**。除本模块外，任何模块都不许再出现
//! 宿主路径字面量。
//!
//! 平台默认值 = **该平台的惯例位置**，不引另一平台的惯例。所有函数接受可选 `flavour`
//! （`"nt"` / `"posix"`），便于单测在两种平台语义下断言，不必真换机器。
//!
//! 「本机真值」与「目标主机真值」是两回事：本模块只回答**本机**的路径长什么样；
//! 目标主机档案见同层 [`crate::node_profile`]。

use std::path::PathBuf;

pub const ENV_RUNTIME_DIR: &str = "AGENT_TEAM_RUNTIME_DIR";
pub const ENV_ETC_DIR: &str = "AGENT_TEAM_ETC_DIR";
pub const ENV_NODE_EXE: &str = "AGENT_TEAM_NODE_EXE";
pub const ENV_LOG_DIR: &str = "AGENT_TEAM_LOG_DIR";
pub const ENV_CLAUDE_EXEC: &str = "AGENT_TEAM_CLAUDE_EXEC";
pub const ENV_CLAUDE_WORKDIR: &str = "AGENT_TEAM_CLAUDE_WORKDIR";
pub const ENV_CODEX_EXE: &str = "AGENT_TEAM_CODEX_EXE";
pub const ENV_CODEX_CMD: &str = "AGENT_TEAM_CODEX_CMD";
pub const ENV_NATS_DIR: &str = "AGENT_TEAM_NATS_DIR";
pub const ENV_PI_CLI_JS: &str = "AGENT_TEAM_PI_CLI_JS";
pub const ENV_PI_WORKDIR: &str = "AGENT_TEAM_PI_WORKDIR";

/// 读取 env 并 strip；空/全空白 → `None`（语义同 Python `os.environ.get(x) or ""` + `.strip()` 判真）。
pub fn env_stripped(name: &str) -> Option<String> {
    match std::env::var(name) {
        Ok(v) => {
            let t = v.trim();
            if t.is_empty() {
                None
            } else {
                Some(t.to_string())
            }
        }
        Err(_) => None,
    }
}

/// 按序取第一个非空环境变量（取不到返回 `None`）。
pub fn first_env(names: &[&str]) -> Option<String> {
    names.iter().find_map(|n| env_stripped(n))
}

/// 宿主路径拼接：**用宿主平台的分隔符**（与 Python `os.path.join` 同口径 ——
/// `flavour` 参数只决定「选哪套默认值」，不参与分隔符选择）。
pub fn join(a: &str, b: &str) -> String {
    if a.is_empty() {
        return b.to_string();
    }
    let sep = std::path::MAIN_SEPARATOR;
    if a.ends_with(sep) {
        format!("{a}{b}")
    } else {
        format!("{a}{sep}{b}")
    }
}

/// 多段宿主路径拼接。
pub fn join_all(parts: &[&str]) -> String {
    let mut out = String::new();
    for p in parts {
        out = if out.is_empty() {
            (*p).to_string()
        } else {
            join(&out, p)
        };
    }
    out
}

/// 当前用户家目录（对应 Python `os.path.expanduser("~")`）。取不到返回空串 —— **不猜**。
pub fn home() -> String {
    if cfg!(windows) {
        if let Some(p) = env_stripped("USERPROFILE") {
            return p;
        }
        let drive = env_stripped("HOMEDRIVE").unwrap_or_default();
        let path = env_stripped("HOMEPATH").unwrap_or_default();
        if !drive.is_empty() || !path.is_empty() {
            return format!("{drive}{path}");
        }
    } else if let Some(h) = env_stripped("HOME") {
        return h;
    }
    String::new()
}

/// 开发用「部署目录」占位：与 Python `tempfile.gettempdir()` 同口径。
///
/// 本模块约束「纯函数、只读、**不建目录**」，故默认值必须是一个**已存在**的目录，
/// 系统临时目录正合。真值一律由 env 注入。
pub fn dev_deploy_dir() -> String {
    let p: PathBuf = std::env::temp_dir();
    let s = p.to_string_lossy().to_string();
    s.trim_end_matches(['/', '\\']).to_string()
}

/// 平台语义：`"nt"` / `"posix"`。显式传入优先，便于单测。
pub fn flavour(value: Option<&str>) -> String {
    match value {
        Some(v) if !v.is_empty() => v.to_string(),
        _ => if cfg!(windows) { "nt" } else { "posix" }.to_string(),
    }
}

pub const WIN_RUNTIME_DIR_SRC: &str = "tempdir";
pub const POSIX_NODE_EXE: &str = "node";
/// 刻意钉住 node22 那份（自动更新已关 + 哨兵），不走 PATH 里的 node。
/// 字面量沿用脱敏占位符口径：真值由 env `AGENT_TEAM_NODE_EXE` 注入。
pub const WIN_NODE_EXE: &str = r"<home>\node22\node-v22.23.2-win-x64\node.exe";
pub const WIN_PI_CLI_JS: &str = r"<home>\node22\node-v22.23.2-win-x64\node_modules\@earendil-works\pi-coding-agent\dist\bundle\cli.js";
pub const POSIX_PI_CLI_JS: &str = "/opt/pi-coding-agent/dist/bundle/cli.js";
pub const WIN_PI_WORKDIR: &str = r"<home>\pi-workspace";
pub const POSIX_PI_WORKDIR: &str = "/opt/pi-workspace";

/// posix 侧部署目录默认值（= 开发占位，真值由 env 注入）。
pub fn posix_runtime_dir() -> String {
    dev_deploy_dir()
}

/// posix 配置目录默认值（= 开发占位）。
pub fn posix_etc_dir() -> String {
    dev_deploy_dir()
}

/// 本节点运行目录（部署件落地的那个目录）。
pub fn runtime_dir(value: Option<&str>) -> String {
    if let Some(v) = env_stripped(ENV_RUNTIME_DIR) {
        return v;
    }
    // nt 与 posix 的兜底占位同源（见 dev_deploy_dir 说明）
    let _ = flavour(value);
    dev_deploy_dir()
}

/// 配置目录**候选列表**（按优先级；调用方逐个探测存在性）。
pub fn etc_dirs(value: Option<&str>) -> Vec<String> {
    if let Some(v) = env_stripped(ENV_ETC_DIR) {
        return vec![v];
    }
    if flavour(value) == "nt" {
        // Windows 部署把配置直接放在运行目录里（如 <运行目录>\client-token）
        vec![runtime_dir(value)]
    } else {
        vec![posix_etc_dir()]
    }
}

/// 配置目录（候选中的第一个）。需要探测多个位置时请用 [`etc_dirs`]。
pub fn etc_dir(value: Option<&str>) -> String {
    etc_dirs(value).into_iter().next().unwrap_or_default()
}

/// 日志目录（默认 = 运行目录下的 `logs/`）。
pub fn log_dir(value: Option<&str>) -> String {
    if let Some(v) = env_stripped(ENV_LOG_DIR) {
        return v;
    }
    join(&runtime_dir(value), "logs")
}

/// PI/Codex 子进程用的 node 可执行文件。
pub fn node_exe(value: Option<&str>) -> String {
    if let Some(v) = env_stripped(ENV_NODE_EXE) {
        return v;
    }
    if flavour(value) == "nt" {
        WIN_NODE_EXE.to_string()
    } else {
        POSIX_NODE_EXE.to_string()
    }
}

/// PI CLI 的 bundle 入口（`cli.js`）。
pub fn pi_cli_js(value: Option<&str>) -> String {
    if let Some(v) = env_stripped(ENV_PI_CLI_JS) {
        return v;
    }
    if flavour(value) == "nt" {
        WIN_PI_CLI_JS.to_string()
    } else {
        POSIX_PI_CLI_JS.to_string()
    }
}

/// PI 的工作目录。
pub fn pi_workdir(value: Option<&str>) -> String {
    if let Some(v) = env_stripped(ENV_PI_WORKDIR) {
        return v;
    }
    if flavour(value) == "nt" {
        WIN_PI_WORKDIR.to_string()
    } else {
        POSIX_PI_WORKDIR.to_string()
    }
}

/// 把「每用户工具」默认值写成 `~` 相对形式：同一份代码在两个平台各自落到正确位置。
pub fn home_sub(parts: &[&str]) -> String {
    let mut all: Vec<&str> = Vec::with_capacity(parts.len() + 1);
    let h = home();
    all.push(h.as_str());
    all.extend_from_slice(parts);
    join_all(&all)
}

/// Claude Code 封装脚本（本机调用 Claude 的唯一入口）。
pub fn claude_exec(value: Option<&str>) -> String {
    if let Some(v) = env_stripped(ENV_CLAUDE_EXEC) {
        return v;
    }
    if flavour(value) == "nt" {
        home_sub(&["AppData", "Local", "hermes", "scripts", "claude_exec.py"])
    } else {
        home_sub(&[".local", "share", "hermes", "scripts", "claude_exec.py"])
    }
}

/// Claude Code 的工作目录。
pub fn claude_workdir(value: Option<&str>) -> String {
    let _ = value; // 与 Python 一致：本函数不随 flavour 分流
    if let Some(v) = env_stripped(ENV_CLAUDE_WORKDIR) {
        return v;
    }
    home_sub(&["claude-workspace"])
}

/// Codex CLI 可执行文件。
pub fn codex_exe(value: Option<&str>) -> String {
    if let Some(v) = env_stripped(ENV_CODEX_EXE) {
        return v;
    }
    let exe = if flavour(value) == "nt" {
        "codex.exe"
    } else {
        "codex"
    };
    home_sub(&[".codex", "packages", "standalone", "current", "bin", exe])
}

/// Codex CLI 的启动壳（Windows 为 `.cmd`，posix 为同名脚本）。
pub fn codex_cmd(value: Option<&str>) -> String {
    if let Some(v) = env_stripped(ENV_CODEX_CMD) {
        return v;
    }
    home_sub(&[
        ".local",
        "bin",
        if flavour(value) == "nt" {
            "codex.cmd"
        } else {
            "codex"
        },
    ])
}

/// NATS 服务端配置目录（posix 惯例 `/etc/nats`；Windows 侧落在运行目录下，env 可覆盖）。
pub fn nats_dir(value: Option<&str>) -> String {
    if let Some(v) = env_stripped(ENV_NATS_DIR) {
        return v;
    }
    if flavour(value) == "nt" {
        join(&runtime_dir(value), "nats")
    } else {
        "/etc/nats".to_string()
    }
}
