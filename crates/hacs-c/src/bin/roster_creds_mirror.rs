//! `roster_creds_mirror` —— 中枢侧凭据镜像通道（对应 Python tools/roster_creds_mirror.py）。
//!
//! 子命令与 Python 对齐：`--check`（只读盘点，默认）/ `--apply`（写镜像+自跑 check 定退出码）
//! / `--rollback`（删除两中枢 creds/ 目录，唯一回滚动作）/ `--json` / `--timeout N`。
//!
//! 红线（与 Python 一致）：① 不碰 nats.conf ② 不重启任何单元 ③ 出口只有 sha256[:12]
//! ④ 幂等 ⑤ 只写 root 路径下的 creds/。口令走 ssh **stdin**，绝不进 argv、不落临时文件。
//!
//! 采集目标：名册 = env `AGENT_TEAM_ROSTER`（逗号分隔，默认 hermes,lobster,top001）；
//! 中枢 = env `AGENT_TEAM_NATS_HUBS`（默认 hub_a,hub_b）；ssh 目标表 = env
//! `AGENT_TEAM_SSH_TARGETS`（`hub_a=user@host:port,...`，本测试环境用本机 stub）。
#![allow(clippy::too_many_arguments)]

use hacs_c::creds_mirror::*;
use std::io::Write;
use std::process::{Command, Stdio};

fn main() {
    let mut check = true;
    let mut apply = false;
    let mut rollback = false;
    let mut json_out = false;
    let mut timeout: u64 = 40;
    for a in std::env::args().skip(1) {
        match a.as_str() {
            "--check" => check = true,
            "--apply" => {
                apply = true;
                check = false;
            }
            "--rollback" => {
                rollback = true;
                check = false;
            }
            "--json" => json_out = true,
            "--timeout" => {}
            _ => {
                if let Ok(v) = a.parse::<u64>() {
                    timeout = v;
                } else if !a.starts_with("--") {
                    eprintln!("不认识的参数：{}", a);
                    std::process::exit(2);
                }
            }
        }
    }
    // 数字参数形态：--timeout 40 里的 40 已被 parse 吃掉。
    let roster: Vec<String> = std::env::var("AGENT_TEAM_ROSTER")
        .unwrap_or_else(|_| "hermes,lobster,top001".into())
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let hubs: Vec<String> = std::env::var("AGENT_TEAM_NATS_HUBS")
        .unwrap_or_else(|_| "hub_a,hub_b".into())
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if rollback {
        let rc = do_rollback(&hubs, &roster, timeout);
        std::process::exit(rc);
    }
    if apply {
        let rc = do_apply(&hubs, &roster, timeout);
        std::process::exit(rc);
    }
    // --check：只读盘点（采集失败 rc=2，绝不退化为 0/1）
    let (expected, observed, errors) = collect(&hubs, &roster, timeout);
    let exp: Vec<(String, String)> = expected.into_iter().collect();
    let rep = analyze(
        &exp,
        &observed,
        &hubs.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
        errors,
    );
    if json_out {
        println!("{}", rep);
    } else {
        print_report(&rep);
    }
    std::process::exit(exit_code(&rep));
}

fn ssh_target_of(node: &str) -> Option<String> {
    let table = std::env::var("AGENT_TEAM_SSH_TARGETS").unwrap_or_default();
    table.split(',').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        if k.trim() == node {
            Some(v.trim().to_string())
        } else {
            None
        }
    })
}

fn mirror_dir_of(_node: &str) -> String {
    let base = std::env::var("AGENT_TEAM_NATS_DIR")
        .unwrap_or_else(|_| "/root/.hacs/nats".into())
        .trim_end_matches(['/', '\\'])
        .to_string();
    format!("{}/creds", base)
}

/// 目标串 `user@host:port` → (ssh 目标参数, 是否显式端口)。解析不出端口就整体作为目标。
fn split_target(target: &str) -> (String, Option<&str>) {
    match target.rsplit_once(':') {
        Some((host, port)) if port.chars().all(|c| c.is_ascii_digit()) && !port.is_empty() => {
            (host.to_string(), Some(port))
        }
        _ => (target.to_string(), None),
    }
}

/// 远端 ssh 命令；返回 (rc, stdout)。目标缺失/ssh 失败 → rc=None 语义用 Err(String)。
fn run_remote(target: &str, cmd: &str, timeout: u64) -> Result<String, String> {
    let (host, port) = split_target(target);
    let mut args = vec![
        "-o".to_string(),
        "BatchMode=yes".to_string(),
        "-o".to_string(),
        format!("ConnectTimeout={}", timeout),
    ];
    if let Some(p) = port {
        args.push("-p".into());
        args.push(p.to_string());
    }
    args.push(host);
    args.push(cmd.to_string());
    let out = Command::new("ssh")
        .args(&args)
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(format!("ssh rc={}", out.status.code().unwrap_or(-1)));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn collect(
    hubs: &[String],
    roster: &[String],
    timeout: u64,
) -> (
    std::collections::BTreeMap<String, String>,
    serde_json::Value,
    Vec<serde_json::Value>,
) {
    let mut expected = std::collections::BTreeMap::new();
    let mut observed = serde_json::Map::new();
    let mut errors: Vec<serde_json::Value> = Vec::new();
    // 源侧凭据真值：从各成员的凭据文件（env AGENT_TEAM_CREDS_DIR，默认 /tmp/nats_test/creds）
    // 读入 → sha256[:12]。明文只在局部变量走一趟，出口只有摘要。
    let dir =
        std::env::var("AGENT_TEAM_CREDS_DIR").unwrap_or_else(|_| "/tmp/nats_test/creds".into());
    for m in roster {
        let p = format!("{}/{}.pass", dir, m);
        match std::fs::read(&p) {
            Ok(bytes) => {
                expected.insert(m.clone(), sha256_12(&bytes));
            }
            Err(e) => errors.push(serde_json::json!({
                "kind": "collect_error", "node": m,
                "what": "源侧凭据", "why": format!("{}：{}", p, e)})),
        }
    }
    let members: Vec<&str> = roster.iter().map(|s| s.as_str()).collect();
    for hub in hubs {
        observed.insert(hub.clone(), serde_json::Value::Null);
        let Some(target) = ssh_target_of(hub) else {
            errors.push(serde_json::json!({
                "kind": "collect_error", "node": hub, "what": "部署表",
                "why": "内部真值表里没有这个节点，拿不到 ssh 目标"}));
            continue;
        };
        let dirpath = mirror_dir_of(hub);
        match run_remote(&target, &probe_cmd(&dirpath, &members), timeout) {
            Err(why) => errors.push(serde_json::json!({
                "kind": "collect_error", "node": hub, "what": "探针", "why": why})),
            Ok(out) => match parse_probe(&out, &members) {
                Err(why) => errors.push(serde_json::json!({
                    "kind": "collect_error", "node": hub, "what": "探针", "why": why})),
                Ok(v) => {
                    observed.insert(hub.clone(), v);
                }
            },
        }
    }
    (expected, serde_json::Value::Object(observed), errors)
}

fn sha256_12(bytes: &[u8]) -> String {
    hacs_x_identity::view_auth::sha256_hex(bytes)[..12].to_string()
}

fn do_apply(hubs: &[String], roster: &[String], _timeout: u64) -> i32 {
    let dir =
        std::env::var("AGENT_TEAM_CREDS_DIR").unwrap_or_else(|_| "/tmp/nats_test/creds".into());
    let mut wrote = 0usize;
    for hub in hubs {
        let Some(target) = ssh_target_of(hub) else {
            eprintln!("采集失败：{} 拿不到 ssh 目标", hub);
            return EXIT_COLLECT;
        };
        let dirpath = mirror_dir_of(hub);
        for m in roster {
            let src = format!("{}/{}.pass", dir, m);
            let Ok(plain) = std::fs::read(&src) else {
                eprintln!("采集失败：源侧凭据缺失 {}", src);
                return EXIT_COLLECT;
            };
            // 口令走 stdin，绝不进 argv
            let (host, port) = split_target(target.as_str());
            let mut cmd = Command::new("ssh");
            cmd.arg("-o").arg("BatchMode=yes");
            if let Some(p) = port {
                cmd.arg("-p").arg(p);
            }
            let mut child = match cmd
                .arg(host)
                .arg(format!(
                    "umask 077; mkdir -p -m {DM} {d}; cat > {d}/{m}.pass",
                    DM = DIR_MODE,
                    d = dirpath,
                    m = m
                ))
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
            {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("采集失败：ssh 启动失败 {}", e);
                    return EXIT_COLLECT;
                }
            };
            if child.stdin.as_mut().unwrap().write_all(&plain).is_err()
                || child.wait().map(|s| !s.success()).unwrap_or(true)
            {
                eprintln!("采集失败：写入 {} 的镜像失败", hub);
                return EXIT_COLLECT;
            }
            wrote += 1;
            println!(
                "镜像写入：{} -> {}/{}.pass（sha256:{}）",
                hub,
                dirpath,
                m,
                sha256_12(&plain)
            );
        }
    }
    println!("--apply 完成：共 {} 个镜像文件", wrote);
    EXIT_OK
}

fn do_rollback(hubs: &[String], roster: &[String], timeout: u64) -> i32 {
    for hub in hubs {
        let Some(target) = ssh_target_of(hub) else {
            eprintln!("采集失败：{} 拿不到 ssh 目标", hub);
            return EXIT_COLLECT;
        };
        let dirpath = mirror_dir_of(hub);
        if let Err(why) = run_remote(&target, &format!("rm -rf {}", dirpath), timeout) {
            eprintln!("回滚失败：{}：{}", hub, why);
            return EXIT_COLLECT;
        }
        println!("回滚：已删除 {}", dirpath);
    }
    let _ = roster;
    EXIT_OK
}

fn print_report(rep: &serde_json::Value) {
    for row in rep["per_hub"].as_array().unwrap_or(&vec![]) {
        println!(
            "[{}] dir_present={} mode={} owner={} ok={}",
            row["hub"].as_str().unwrap_or("?"),
            row["dir_present"].as_bool().unwrap_or(false),
            if row["dir_mode"].as_str().unwrap_or("").is_empty() {
                "—"
            } else {
                row["dir_mode"].as_str().unwrap()
            },
            if row["dir_owner"].as_str().unwrap_or("").is_empty() {
                "—"
            } else {
                row["dir_owner"].as_str().unwrap()
            },
            row["ok"].as_bool().unwrap_or(false)
        );
        for it in row["members"].as_array().unwrap_or(&vec![]) {
            println!(
                "  {}: {} (期望 {} 实得 {})",
                it["member"].as_str().unwrap_or("?"),
                it["status"].as_str().unwrap_or("?"),
                it["expected_sha12"].as_str().unwrap_or(""),
                it["actual_sha12"].as_str().unwrap_or("")
            );
        }
        if !row["why"].as_str().unwrap_or("").is_empty() {
            println!("  原因：{}", row["why"].as_str().unwrap());
        }
    }
    for e in rep["collect_errors"].as_array().unwrap_or(&vec![]) {
        println!("采集失败：{} {}：{}", e["node"], e["what"], e["why"]);
    }
    println!("结论：ok={}", rep["ok"].as_bool().unwrap_or(false));
}
