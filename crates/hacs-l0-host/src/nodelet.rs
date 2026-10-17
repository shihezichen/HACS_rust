//! `nodelet` —— L0 轻量节点守护纯逻辑（对应 Python node/l0_host/nodelet.py）。
//!
//! 三条铁律：只读（不写文件/不试写）/ 不越层（只依赖本层 node_profile）/ 不泄密
//! （凭据面唯一合法形态 `{"present":bool,"len":int}`，出口机械复核，违反即 panic）。
//! 探针 JSON 顶层键序与 facts 键序**冻结**（`preserve_order` + 断言）。

use super::node_profile as np;
use serde_json::{json, Map, Value};

pub const ENV_BIND: &str = "AGENT_TEAM_NODELET_BIND";
pub const DEFAULT_HOST: &str = "127.0.0.1";
pub const DEFAULT_PORT: u16 = 9971;
pub const PROBE_PATH: &str = "/probe";

pub const ENV_SERVICES: &str = "AGENT_TEAM_NODELET_SERVICES";
pub const UNIT_NAME_POSIX: &str = "agent-team-nodelet";
pub const UNIT_NAME_NT: &str = "AgentTeamNodelet";

pub const SYSTEMD_STATES: &[&str] = &[
    "active",
    "activating",
    "deactivating",
    "inactive",
    "failed",
    "reloading",
];
pub const NT_STATES_RUNNING: &[&str] = &["running", "正在运行"];
pub const NT_STATES_KNOWN: &[&str] = &[
    "running",
    "正在运行",
    "ready",
    "disabled",
    "queued",
    "就绪",
    "已禁用",
    "已排队",
];
pub const UNIT_QUERY_TIMEOUT: f64 = 5.0;

pub const CRED_KEYS: [&str; 2] = ["present", "len"];
pub const REQUIRED_FACES: &[&str] = &[
    "node_name",
    "flavour",
    "home",
    "runtime_dir",
    "etc_dirs",
    "log_dir",
    "target_known",
    "target_flavour",
    "target_home",
    "target_claude_exec",
    "hermes_state_db",
    "openclaw_sessions_json",
    "a2a_probe_token",
];
pub const FACE_TARGET_PROFILE: &str = "target_profile";
pub const PROBE_KEYS: [&str; 6] = ["node", "profile", "facts", "version", "uptime_s", "ts"];
pub const FACT_KEYS: [&str; 6] = [
    "runtime_dir",
    "log_dir",
    "hermes_state_db",
    "openclaw_sessions",
    "a2a_probe_token",
    "services",
];

/// 单元查询执行器（None = 查询跑不起来）。
pub type UnitRun = dyn Fn(&[String]) -> Option<(i32, String)>;
/// 档案面注入（None = 面取不到）。
pub type FaceFn = dyn Fn(&str) -> Option<Value>;

// ======================================================== ① 绑定面

fn octets(host: &str) -> Option<[u8; 4]> {
    let parts: Vec<&str> = host.split('.').collect();
    if parts.len() != 4 {
        return None;
    }
    let mut out = [0u8; 4];
    for (i, p) in parts.iter().enumerate() {
        if p.is_empty() || !p.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        // Python int() 容忍前导零与超长数字；u32 解析防溢出
        let v: u64 = p.parse().ok()?;
        if v > 255 {
            return None;
        }
        out[i] = v as u8;
    }
    Some(out)
}

fn is_loopback(host: &str) -> bool {
    let low = host.trim().trim_matches(['[', ']']).to_lowercase();
    if matches!(low.as_str(), "localhost" | "::1" | "0:0:0:0:0:0:0:1") {
        return true;
    }
    octets(&low).map(|o| o[0] == 127).unwrap_or(false)
}

fn is_tailnet(host: &str) -> bool {
    octets(host.trim().trim_matches(['[', ']']))
        .map(|o| o[0] == 100 && (64..=127).contains(&o[1]))
        .unwrap_or(false)
}

/// `"host:port"` → `(host, port)`。空值 → 默认（环回）。只放行环回与尾网，其余报错拒启。
pub fn parse_bind(text: Option<&str>) -> Result<(String, u16), String> {
    let raw = text
        .map(|s| s.to_string())
        .or_else(|| std::env::var(ENV_BIND).ok())
        .unwrap_or_default()
        .trim()
        .to_string();
    if raw.is_empty() {
        return Ok((DEFAULT_HOST.into(), DEFAULT_PORT));
    }
    let (host, port_s) = if raw.starts_with('[') {
        match raw.split_once(']') {
            Some((h, rest)) => (h[1..].to_string(), rest.trim_start_matches(':').to_string()),
            None => return Err(format!("{} 要写成 host:port，实际是 {:?}", ENV_BIND, raw)),
        }
    } else if raw.matches(':').count() == 1 {
        let (h, p) = raw.split_once(':').unwrap();
        (h.to_string(), p.to_string())
    } else {
        return Err(format!("{} 要写成 host:port，实际是 {:?}", ENV_BIND, raw));
    };
    let host = host.trim().to_string();
    let port_s = port_s.trim().to_string();
    if port_s.is_empty() || !port_s.chars().all(|c| c.is_ascii_digit()) {
        return Err(format!("{} 的端口不是数字：{:?}", ENV_BIND, raw));
    }
    let port: u64 = port_s
        .parse()
        .map_err(|_| format!("{} 的端口不是数字：{:?}", ENV_BIND, raw))?;
    if !(1..=65535).contains(&port) {
        return Err(format!("{} 的端口越界：{:?}", ENV_BIND, raw));
    }
    if !(is_loopback(&host) || is_tailnet(&host)) {
        return Err(format!(
            "{} 只许绑环回或尾网地址，拒绝 {:?}（探针口不许暴露到协作域以外）",
            ENV_BIND, host
        ));
    }
    Ok((host, port as u16))
}

// ======================================================== ② 监管单元面

/// 本守护自己在这台机上的单元名。
pub fn unit_name(flavour_value: Option<&str>) -> String {
    if np::flavour(flavour_value) == "nt" {
        UNIT_NAME_NT.into()
    } else {
        UNIT_NAME_POSIX.into()
    }
}

/// 本机声明的单元名列表（去重保序）；没声明就只认自己那一个。
pub fn declared_services(flavour_value: Option<&str>, env_value: Option<&str>) -> Vec<String> {
    let raw = env_value
        .map(|s| s.to_string())
        .or_else(|| std::env::var(ENV_SERVICES).ok())
        .unwrap_or_default();
    let mut names: Vec<String> = Vec::new();
    for piece in raw.split(',') {
        let name = piece.trim();
        if !name.is_empty() && !names.iter().any(|n| n == name) {
            names.push(name.to_string());
        }
    }
    if names.is_empty() {
        vec![unit_name(flavour_value)]
    } else {
        names
    }
}

/// 查单元状态的命令行（只查不动）。
pub fn unit_probe_argv(name: &str, flavour_value: Option<&str>) -> Vec<String> {
    if np::flavour(flavour_value) == "nt" {
        vec![
            "schtasks".into(),
            "/Query".into(),
            "/TN".into(),
            name.into(),
            "/FO".into(),
            "CSV".into(),
            "/NH".into(),
        ]
    } else {
        vec!["systemctl".into(), "is-active".into(), name.into()]
    }
}

/// `schtasks /FO CSV /NH` 的最后一行最后一列（= 状态）。
pub fn nt_status_field(text: &str) -> String {
    let rows: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    match rows.last() {
        Some(last) => last
            .rsplit(',')
            .next()
            .unwrap_or("")
            .trim_matches('"')
            .trim()
            .to_string(),
        None => String::new(),
    }
}

/// 一个单元在本机的状态 → `(known, active)`，两个布尔，没有第三种话。
/// `run` 返回 `None` = 查询跑不起来/超时。
pub fn unit_state(name: &str, flavour_value: Option<&str>, run: &UnitRun) -> (bool, bool) {
    let Some((rc, out)) = run(&unit_probe_argv(name, flavour_value)) else {
        return (false, false);
    };
    if np::flavour(flavour_value) == "nt" {
        if rc != 0 {
            return (false, false);
        }
        let status = nt_status_field(&out).to_lowercase();
        let known = NT_STATES_KNOWN.iter().any(|s| s.to_lowercase() == status);
        let active = NT_STATES_RUNNING.iter().any(|s| s.to_lowercase() == status);
        return (known, active);
    }
    let word = out.trim().to_lowercase();
    (SYSTEMD_STATES.contains(&word.as_str()), word == "active")
}

// ======================================================== ③ 凭据红线

fn is_credential_face(name: &str) -> bool {
    let low = name.to_lowercase();
    const CRED_WORDS: [&str; 5] = ["token", "secret", "password", "credential", "key"];
    CRED_WORDS.iter().any(|w| low.contains(w))
}

/// 凭据面的唯一合法输出：有没有 + 多长。值本身一个字符都不出门。
pub fn credential_fact(value: &str) -> Value {
    json!({"present": !value.is_empty(), "len": value.chars().count()})
}

/// 出口机械复核：凡名字像口令的面，形状必须恰好是 `{"present","len"}`。违反即 panic。
pub fn enforce_credential_redline(facts: &Map<String, Value>) {
    for (name, value) in facts {
        if !is_credential_face(name) {
            continue;
        }
        let ok = value
            .as_object()
            .map(|o| {
                let keys: Vec<&String> = o.keys().collect();
                keys.len() == 2 && keys[0] == "present" && keys[1] == "len"
            })
            .unwrap_or(false);
        assert!(
            ok,
            "凭据红线：面「{}」的形状必须是 {{\"present\",\"len\"}}",
            name
        );
    }
}

// ======================================================== ④ 事实面与探针

/// 一个目录的事实：在不在、写不写得进去（`access(W_OK)` 判，**不试写**）。
pub fn dir_fact(path: &str) -> Value {
    let exists = !path.is_empty() && std::path::Path::new(path).is_dir();
    let writable = exists && !path.is_empty() && {
        #[cfg(unix)]
        {
            // os.access(W_OK) ≙ 以真实 uid 检查写权限
            let Ok(meta) = std::fs::metadata(path) else {
                return json!({"exists": exists, "writable": false});
            };
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            let mode = meta.permissions().mode();
            let (uid, gid) = (unsafe { libc_getuid() }, meta.gid());
            let (f_uid, f_gid) = (meta.uid(), meta.gid());
            let (u, g, o) = ((mode >> 6) & 7, (mode >> 3) & 7, mode & 7);
            let bit = if f_uid == uid {
                u
            } else if f_gid == gid {
                g
            } else {
                o
            };
            bit & 2 != 0
        }
        #[cfg(not(unix))]
        {
            true
        }
    };
    json!({"exists": exists, "writable": writable})
}

#[cfg(unix)]
unsafe fn libc_getuid() -> u32 {
    // 零依赖：直接走 syscall 封装（std 不暴露 getuid）
    extern "C" {
        fn getuid() -> u32;
    }
    getuid()
}
/// 一个档案声明的文件：配没配、在不在。只给布尔，不给路径。
pub fn file_fact(path: &str) -> Value {
    let exists = !path.is_empty() && std::path::Path::new(path).is_file();
    json!({"configured": !path.is_empty(), "exists": exists})
}

/// 磁盘上本件的 md5 前 8 位（每次探针现算）。Rust 版用二进制自身路径；读不到 → 空串。
pub fn self_fingerprint(self_path: Option<&str>) -> String {
    let path = self_path.map(|s| s.to_string()).or_else(|| {
        std::env::current_exe()
            .ok()
            .map(|p| p.to_string_lossy().into_owned())
    });
    let Some(path) = path else {
        return String::new();
    };
    std::fs::read(&path)
        .map(|bytes| {
            let d = hacs_x_identity::view_auth::sha256_hex(&bytes);
            d[..8].to_string()
        })
        .unwrap_or_default()
}

fn fmt_ts_gmt(epoch: f64) -> String {
    // %Y-%m-%dT%H:%M:%SZ（UTC）—— civil 历法内联（L0 不越层，不依赖 L2/L3）
    let secs = epoch.floor() as i64;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// 天数（1970-01-01 = 0）→ (y, m, d)。Howard Hinnant 的 civil_from_days。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// 探针载荷（键名与顺序冻结）。`archive` 允许装置注入假档案做负向场景（残档 → profile.ok=false）。
pub fn build_probe(
    started_at: Option<f64>,
    now: f64,
    run: &UnitRun,
    archive: Option<&FaceFn>,
    flavour_value: Option<&str>,
    services_env: Option<&str>,
) -> Value {
    // faces：取不到 → missing 记账，事实退成 False（不编造）
    let mut missing: Vec<String> = Vec::new();
    let face = |name: &str, missing: &mut Vec<String>| -> Option<Value> {
        match archive {
            Some(a) => match a(name) {
                Some(v) => Some(v),
                None => {
                    if !missing.iter().any(|m| m == name) {
                        missing.push(name.into());
                    }
                    None
                }
            },
            None => default_face(name),
        }
    };
    macro_rules! face {
        ($n:expr) => {{
            face($n, &mut missing)
        }};
    }

    let node = face!("node_name")
        .and_then(|v| v.as_str().map(|s| s.to_string()))
        .unwrap_or_default();
    let flav = face!("flavour")
        .and_then(|v| v.as_str().map(|s| s.to_string()))
        .unwrap_or_else(|| np::flavour(flavour_value));
    if !node.is_empty() && !missing.iter().any(|m| m == FACE_TARGET_PROFILE) {
        let known = face!("target_known")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if !known {
            missing.push(FACE_TARGET_PROFILE.into());
        }
    }

    let mut dir_of = |name: &str| -> String {
        face!(name)
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .unwrap_or_default()
    };
    let mut facts = Map::new();
    facts.insert("runtime_dir".into(), dir_fact(&dir_of("runtime_dir")));
    facts.insert("log_dir".into(), dir_fact(&dir_of("log_dir")));
    facts.insert(
        "hermes_state_db".into(),
        file_fact(&dir_of("hermes_state_db")),
    );
    facts.insert(
        "openclaw_sessions".into(),
        file_fact(&dir_of("openclaw_sessions_json")),
    );
    facts.insert(
        "a2a_probe_token".into(),
        credential_fact(&dir_of("a2a_probe_token")),
    );
    let mut services: Vec<Value> = Vec::new();
    for name in declared_services(Some(&flav), services_env) {
        let (known, active) = unit_state(&name, Some(&flav), run);
        services.push(json!({"name": name, "known": known, "active": active}));
    }
    facts.insert("services".into(), Value::Array(services));
    enforce_credential_redline(&facts);

    let uptime_s = match started_at {
        None => 0.0,
        Some(s) => ((now - s) * 1000.0).round() / 1000.0,
    };
    let payload = json!({
        "node": node,
        "profile": {"file": "node_profile.rs", "ok": missing.is_empty(),
                    "missing": missing},
        "facts": Value::Object(facts),
        "version": self_fingerprint(None),
        "uptime_s": uptime_s,
        "ts": fmt_ts_gmt(now),
    });
    let top: Vec<&String> = payload.as_object().unwrap().keys().collect();
    let want: Vec<&str> = PROBE_KEYS.to_vec();
    assert!(
        top.iter().map(|s| s.as_str()).collect::<Vec<_>>() == want,
        "探针顶层键序被改了（契约冻结）"
    );
    let f: Vec<&String> = payload["facts"].as_object().unwrap().keys().collect();
    let want_f: Vec<&str> = FACT_KEYS.to_vec();
    assert!(
        f.iter().map(|s| s.as_str()).collect::<Vec<_>>() == want_f,
        "facts 键序被改了（契约冻结）"
    );
    payload
}

/// 默认档案面（真 node_profile）。
fn default_face(name: &str) -> Option<Value> {
    Some(match name {
        "node_name" => json!(np::node_name()),
        "flavour" => json!(np::flavour(None)),
        "home" => json!(np::home()),
        "runtime_dir" => json!(np::runtime_dir(None)),
        "etc_dirs" => json!(np::etc_dirs(None)),
        "log_dir" => json!(np::log_dir(None)),
        "target_known" => json!(np::target_known(&np::node_name())),
        "target_flavour" => json!(np::target_flavour(&np::node_name())),
        "target_home" => json!(np::target_home(&np::node_name())),
        "target_claude_exec" => json!(np::target_claude_exec(&np::node_name())),
        "hermes_state_db" => json!(np::hermes_state_db()),
        "openclaw_sessions_json" => json!(np::openclaw_sessions_json()),
        "a2a_probe_token" => json!(np::a2a_probe_token()),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_ok(_: &[String]) -> Option<(i32, String)> {
        Some((0, "active\n".into()))
    }

    #[test]
    fn test_parse_bind() {
        assert_eq!(
            parse_bind(None).unwrap(),
            ("127.0.0.1".to_string(), 9971u16)
        );
        assert_eq!(
            parse_bind(Some("127.0.0.1:8080")).unwrap(),
            ("127.0.0.1".into(), 8080)
        );
        assert_eq!(
            parse_bind(Some("localhost:1")).unwrap(),
            ("localhost".into(), 1)
        );
        assert_eq!(
            parse_bind(Some("[::1]:9")).unwrap(),
            ("::1".to_string(), 9u16)
        );
        assert_eq!(
            parse_bind(Some("100.64.5.5:9971")).unwrap(),
            ("100.64.5.5".into(), 9971)
        );
        assert!(parse_bind(Some("8.8.8.8:80")).is_err(), "公网地址必须拒启");
        assert!(parse_bind(Some("1.2.3.4:0")).is_err());
        assert!(parse_bind(Some("1.2.3.4:abc")).is_err());
        assert!(parse_bind(Some("1.2.3.4")).is_err());
    }

    #[test]
    fn test_unit_state_systemd() {
        let (k, a) = unit_state("x", None, &|_| Some((0, "active\n".into())));
        assert!(k && a);
        let (k, a) = unit_state("x", None, &|_| Some((3, "inactive\n".into())));
        assert!(k && !a);
        let (k, a) = unit_state("x", None, &|_| None);
        assert!(!k && !a, "查不出来 = 不认识，不编造");
    }

    #[test]
    fn test_unit_state_nt() {
        let (k, a) = unit_state("x", Some("nt"), &|_| {
            Some((0, "\"Task\",\"正在运行\"\r\n".into()))
        });
        assert!(k && a);
        let (k, a) = unit_state("x", Some("nt"), &|_| Some((1, "".into())));
        assert!(!k && !a);
    }

    #[test]
    fn test_declared_services_dedup_and_default() {
        assert_eq!(
            declared_services(None, Some("a,b,a ,c")),
            vec!["a", "b", "c"]
        );
        assert_eq!(
            declared_services(None, Some("")),
            vec![UNIT_NAME_POSIX],
            "没声明就只认自己"
        );
    }

    #[test]
    fn test_credential_redline() {
        let mut m = Map::new();
        m.insert("a2a_probe_token".into(), json!({"present": true, "len": 8}));
        enforce_credential_redline(&m); // 合规，不 panic
        m.insert("a2a_probe_token".into(), json!("secret-value"));
        let r = std::panic::catch_unwind(|| enforce_credential_redline(&m));
        assert!(r.is_err(), "凭据面形状违规必须炸");
    }

    #[test]
    fn test_dir_file_facts() {
        let f = dir_fact("/tmp");
        assert_eq!(f["exists"], json!(true));
        let f = dir_fact("");
        assert_eq!(f["exists"], json!(false));
        let f = file_fact("/nonexistent-xyz");
        assert_eq!(f, json!({"configured": true, "exists": false}));
        let f = file_fact("");
        assert_eq!(f, json!({"configured": false, "exists": false}));
    }

    #[test]
    fn test_build_probe_key_order_and_missing_profile() {
        // 残档注入：所有面都取不到 → profile.ok=false，事实退 False
        let p = build_probe(
            Some(0.0),
            10.0,
            &run_ok,
            Some(&|_| None),
            None,
            Some("svc_a"),
        );
        assert_eq!(p["profile"]["ok"], json!(false));
        assert_eq!(
            p["facts"]["services"][0]["known"],
            json!(true),
            "服务状态与档案无关（run_ok 返回 active）"
        );
        assert_eq!(p["facts"]["services"][0]["name"], "svc_a");
        assert_eq!(p["uptime_s"], json!(10.0));
        let top: Vec<&String> = p.as_object().unwrap().keys().collect();
        let want: Vec<&str> = PROBE_KEYS.to_vec();
        assert_eq!(top.iter().map(|s| s.as_str()).collect::<Vec<_>>(), want);
        // 默认档案（本机真档案）能跑出完整探针
        let p = build_probe(None, 0.0, &run_ok, None, None, None);
        assert!(p["facts"]["a2a_probe_token"]["present"].is_boolean());
    }
}
