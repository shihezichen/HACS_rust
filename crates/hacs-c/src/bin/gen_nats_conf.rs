//! `gen_nats_conf` —— 就地重写 NATS 中枢 conf（对应 Python hub/l1_relay/gen_nats_conf.py）。
//!
//! 成员集合与顺序 = 名册（env `AGENT_TEAM_ROSTER` 或内置 hermes,lobster,top001 的
//! Rust 侧口径），口令只从 `<NATS_DIR>/creds/<member>.pass` 读，缺件必拒（rc=2），
//! 绝不回落旧 conf 继承。`--drop-user` 是唯一退场路径；`--dry-run` 只打印。

use std::io::Write;

const CLUSTER_NAME: &str = "agent-team";
const ROUTE_USER: &str = "route";

fn nats_dir() -> String {
    std::env::var("AGENT_TEAM_NATS_DIR").unwrap_or_else(|_| "/etc/hacs/nats".into())
}

/// env 覆盖（`AGENT_TEAM_ROSTER`，逗号分隔）或默认名册；顺序即规范序。
fn roster() -> Vec<String> {
    std::env::var("AGENT_TEAM_ROSTER")
        .unwrap_or_else(|_| "hermes,lobster,top001".into())
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn hb_subjects(user: &str) -> Vec<String> {
    vec![user.to_string()]
}

fn perm_list(user: &str) -> String {
    let hb: Vec<String> = hb_subjects(user)
        .iter()
        .map(|n| format!("\"team.hb.{n}\""))
        .collect();
    format!(
        "publish: [\"a2a.>\", {}, \"team.probe.{user}\", \"bb.>\", \"$JS.API.>\", \"_INBOX.>\"], subscribe: [\"a2a.>\", \"team.>\", \"bb.>\", \"_INBOX.>\"]",
        hb.join(", ")
    )
}

/// (成员, 口令) 列表 + 缺件成员名。口令只从镜像文件读，strip() 归一。
fn read_roster_users(dir: &str) -> (Vec<(String, String)>, Vec<String>) {
    let mut users = Vec::new();
    let mut missing = Vec::new();
    for member in roster() {
        let path = format!("{dir}/creds/{member}.pass");
        match std::fs::read_to_string(&path) {
            Ok(t) => {
                let password = t.trim().to_string();
                if password.is_empty() {
                    missing.push(member);
                } else {
                    users.push((member, password));
                }
            }
            Err(_) => missing.push(member),
        }
    }
    (users, missing)
}

pub struct Parsed {
    pub pos: Vec<String>,
    pub drop: Vec<String>,
    pub dry: bool,
}

/// 三个位置参数老契约 + `--drop-user`（可重复）+ `--dry-run`；未知开关拒绝。
pub fn parse_argv(argv: &[String]) -> Option<Parsed> {
    let mut pos = Vec::new();
    let mut drop = Vec::new();
    let mut dry = false;
    let mut i = 0;
    while i < len_guard(argv.len()) {
        let a = &argv[i];
        if a == "-h" || a == "--help" {
            return None; // help 走单独分支（main 里处理）
        }
        if a == "--drop-user" {
            if i + 1 >= argv.len() {
                return None;
            }
            drop.push(argv[i + 1].clone());
            i += 2;
        } else if a == "--dry-run" {
            dry = true;
            i += 1;
        } else if a.starts_with("--") {
            return None;
        } else {
            pos.push(a.clone());
            i += 1;
        }
    }
    if pos.len() != 3 {
        return None;
    }
    Some(Parsed { pos, drop, dry })
}

fn len_guard(n: usize) -> usize {
    n
}

/// 生成 conf 正文（与 Python 逐行同形状）。
pub fn render_conf(
    server_name: &str,
    self_ip: &str,
    peer_ip: &str,
    route_pass: &str,
    users: &[(String, String)],
    now_stamp: &str,
) -> String {
    let mut lines: Vec<String> = Vec::new();
    lines.push("# agent-team NATS 中枢（v2 统一端口体系 · 双活集群）".into());
    lines.push("# 端口：9930 客户端 / 9931 集群路由（本机与对端组成 agent-team 集群） / 9932 监控（仅本机）".into());
    lines.push("# 不上 JetStream：真相源是黑板(PG)；集群只做消息路由，不做持久化".into());
    lines.push(format!(
        "# 由 gen_nats_conf.py 生成于 {now_stamp}（改配置请改脚本再重跑，别手改本文件）"
    ));
    lines.push(format!("server_name: \"{server_name}\""));
    lines.push(format!("listen: {self_ip}:9930"));
    lines.push("http: 127.0.0.1:9932".into());
    lines.push(String::new());
    lines.push("max_payload: 1MB".into());
    lines.push("max_connections: 64".into());
    lines.push("write_deadline: \"2s\"".into());
    lines.push("ping_interval: \"2m\"".into());
    lines.push("ping_max: 3".into());
    lines.push(String::new());
    lines.push("debug: false".into());
    lines.push("trace: false".into());
    lines.push("logtime: true".into());
    lines.push(String::new());
    lines.push("# ---- 集群路由：两台中枢互为一个集群，任一在线即可全通畅 ----".into());
    lines.push("cluster {".into());
    lines.push(format!("  name: \"{CLUSTER_NAME}\""));
    lines.push(format!("  listen: {self_ip}:9931"));
    lines.push(format!(
        "  authorization {{ user: \"{ROUTE_USER}\", password: \"{route_pass}\", timeout: 3 }}"
    ));
    if !peer_ip.is_empty() && !peer_ip.eq_ignore_ascii_case("none") {
        lines.push(format!(
            "  routes = [ \"nats-route://{ROUTE_USER}:{route_pass}@{peer_ip}:9931\" ]"
        ));
    }
    lines.push("}".into());
    lines.push(String::new());
    lines.push("authorization {".into());
    lines.push("  timeout: 3".into());
    lines.push("  users = [".into());
    let n = users.len();
    for (idx, (user, password)) in users.iter().enumerate() {
        let comma = if idx < n - 1 { "," } else { "" };
        lines.push(format!(
            "    {{ user: \"{user}\", password: \"{password}\", permissions: {{ {} }} }}{comma}",
            perm_list(user)
        ));
    }
    lines.push("  ]".into());
    lines.push("}".into());
    lines.push(String::new());
    lines.join("\n")
}

pub fn usage() -> &'static str {
    "用法: gen_nats_conf <server_name> <self_ip> <peer_ip> [--drop-user <成员名>]... [--dry-run]"
}

fn now_stamp() -> String {
    let e = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (y, mo, d, h, mi) = hacs_l3_domain::team_group::beijing_parts_pub(e as i64);
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{:02}", e % 60)
}

fn now_stamp_compact() -> String {
    let e = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (y, mo, d, h, mi) = hacs_l3_domain::team_group::beijing_parts_pub(e as i64);
    format!("{y:04}{mo:02}{d:02}-{h:02}{mi:02}{:02}", e % 60)
}

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.is_empty() {
        eprintln!("{}", usage());
        std::process::exit(2);
    }
    if argv[0] == "-h" || argv[0] == "--help" {
        println!("{}", usage());
        return;
    }
    let Some(parsed) = parse_argv(&argv) else {
        eprintln!("{}", usage());
        std::process::exit(2);
    };
    let (server_name, self_ip, peer_ip) = (
        parsed.pos[0].clone(),
        parsed.pos[1].clone(),
        parsed.pos[2].clone(),
    );
    let dir = nats_dir();
    let conf = format!("{dir}/nats.conf");
    if !std::path::Path::new(&conf).exists() {
        eprintln!("找不到 {conf}");
        std::process::exit(2);
    }
    let (mut users, missing) = read_roster_users(&dir);
    if !missing.is_empty() {
        eprintln!("凭据镜像缺件，拒绝改写：{}", missing.join(","));
        eprintln!("（应在 {dir}/creds/<成员>.pass；先跑 roster_creds_mirror --apply）");
        std::process::exit(2);
    }
    if users.is_empty() {
        eprintln!("名册为空，拒绝改写");
        std::process::exit(2);
    }
    let drop_set: std::collections::HashSet<&String> =
        parsed.drop.iter().filter(|d| !d.is_empty()).collect();
    let mut dropped: Vec<String> = users
        .iter()
        .filter(|(u, _)| drop_set.contains(u))
        .map(|(u, _)| u.clone())
        .collect();
    dropped.sort();
    dropped.dedup();
    if !drop_set.is_empty() {
        users.retain(|(u, _)| !drop_set.contains(u));
        if users.is_empty() {
            eprintln!("拒绝改写：--drop-user 会把用户清空");
            std::process::exit(2);
        }
    }
    // 路由口令：文件里没有就生成（48 字符 url-safe）
    let route_pass_file =
        std::env::var("AGENT_TEAM_NATS_ROUTE_PASS").unwrap_or_else(|_| format!("{dir}/route.pass"));
    let route_pass = match std::fs::read_to_string(&route_pass_file) {
        Ok(t) if !t.trim().is_empty() => t.trim().to_string(),
        _ => {
            let p = random_urlsafe(24);
            let _ = std::fs::write(&route_pass_file, &p);
            p
        }
    };
    if route_pass.is_empty() {
        eprintln!("路由口令为空，拒绝改写");
        std::process::exit(2);
    }

    let new = render_conf(
        &server_name,
        &self_ip,
        &peer_ip,
        &route_pass,
        &users,
        &now_stamp(),
    );
    if parsed.dry {
        print!("{new}");
        eprintln!(
            "dry-run：未写盘、未备份（用户数={}；退场={}）",
            users.len(),
            if dropped.is_empty() {
                "无".to_string()
            } else {
                dropped.join(",")
            }
        );
        return;
    }
    let stamp = now_stamp_compact();
    let _ = std::fs::copy(&conf, format!("{conf}.bak-{stamp}"));
    if let Ok(mut f) = std::fs::File::create(&conf) {
        let _ = f.write_all(new.as_bytes());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&conf, std::fs::Permissions::from_mode(0o640));
    }
    println!("已重写 {conf}（备份 .bak-{stamp}）");
    if !dropped.is_empty() {
        println!("已退场用户: {}", dropped.join(","));
    }
    println!(
        "server_name={server_name} listen=9930/9931 peer={peer_ip} 用户数={}",
        users.len()
    );
}

/// 无 rand 依赖的 url-safe 随机串（熵：纳秒 ^ pid ^ 地址噪声，逐字节 hash 链）。
fn random_urlsafe(nbytes: usize) -> String {
    let mut seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
        ^ ((std::process::id() as u64) << 32)
        ^ (&seed_anchor as *const _ as u64);
    const B64: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for _ in 0..nbytes * 2 {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let x = hacs_l2_adapter::cli_backend::sha1(&seed.to_le_bytes());
        for b in x.iter().take(2) {
            out.push(B64[(*b as usize) % B64.len()] as char);
        }
    }
    out.chars().take(nbytes * 4 / 3 + 4).collect()
}

static seed_anchor: u8 = 0;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_argv_contract() {
        let ok = parse_argv(&["sn".into(), "1.1.1.1".into(), "2.2.2.2".into()]).unwrap();
        assert_eq!(ok.pos.len(), 3);
        assert!(ok.drop.is_empty() && !ok.dry);
        let with = parse_argv(&[
            "a".into(),
            "b".into(),
            "c".into(),
            "--drop-user".into(),
            "x".into(),
            "--dry-run".into(),
        ])
        .unwrap();
        assert_eq!(with.drop, vec!["x".to_string()]);
        assert!(with.dry);
        // 未知开关拒绝；位置参数数不对拒绝
        assert!(parse_argv(&["a".into(), "--bogus".into()]).is_none());
        assert!(parse_argv(&["a".into(), "b".into()]).is_none());
    }

    #[test]
    fn test_perm_list_shape() {
        let p = perm_list("hermes");
        assert!(p.starts_with("publish: [\"a2a.>\", \"team.hb.hermes\", \"team.probe.hermes\""));
        assert!(p.contains("subscribe: [\"a2a.>\", \"team.>\", \"bb.>\", \"_INBOX.>\"]"));
    }

    #[test]
    fn test_render_conf_shape() {
        let users = vec![
            ("alpha".to_string(), "p1".to_string()),
            ("beta".to_string(), "p2".to_string()),
        ];
        let c = render_conf(
            "n1",
            "10.0.0.1",
            "10.0.0.2",
            "rp",
            &users,
            "2026-01-01 00:00:00",
        );
        assert!(c.contains("server_name: \"n1\""));
        assert!(c.contains("listen: 10.0.0.1:9930"));
        assert!(c.contains("routes = [ \"nats-route://route:rp@10.0.0.2:9931\" ]"));
        assert!(c.contains("user: \"alpha\", password: \"p1\""));
        assert!(c.contains("user: \"beta\", password: \"p2\", permissions: { publish"));
        assert!(c.trim_end().ends_with("}"));
        // peer=none 不拨号
        let c2 = render_conf("n1", "ip", "none", "rp", &users, "t");
        assert!(!c2.contains("routes = ["));
    }
}
