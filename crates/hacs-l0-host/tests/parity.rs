//! L0 平台层等价测试（对照 Python `node/l0_host/node_paths.py` 与 `member_roster.py`）。
//!
//! 这些测试的判据是「与 Python 版同口径」：env 优先顺序、平台默认值形状、名册顺序与查询。
//! 环境变量是**进程全局**的，故所有动 env 的用例共用一把锁串行跑。

use std::sync::Mutex;

use hacs_l0_host::{member_roster, node_paths};

static ENV_LOCK: Mutex<()> = Mutex::new(());

/// 在受控 env 下跑一段断言；结束后按 `restore` 还原。
fn with_env<F: FnOnce()>(set: &[(&str, &str)], restore: &[&str], f: F) {
    let _guard = ENV_LOCK.lock().unwrap();
    let saved: Vec<(String, Option<String>)> = restore
        .iter()
        .map(|k| ((*k).to_string(), std::env::var(k).ok()))
        .collect();
    for (k, v) in set {
        std::env::set_var(k, v);
    }
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    for (k, v) in &saved {
        match v {
            Some(val) => std::env::set_var(k, val),
            None => std::env::remove_var(k),
        }
    }
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}

#[test]
fn flavour_explicit_beats_default() {
    assert_eq!(node_paths::flavour(Some("nt")), "nt");
    assert_eq!(node_paths::flavour(Some("posix")), "posix");
    let expected = if cfg!(windows) { "nt" } else { "posix" };
    assert_eq!(node_paths::flavour(None), expected);
    assert_eq!(node_paths::flavour(Some("")), expected);
}

#[test]
fn runtime_dir_env_wins_and_is_trimmed() {
    with_env(
        &[(node_paths::ENV_RUNTIME_DIR, "  /srv/hacs  ")],
        &[node_paths::ENV_RUNTIME_DIR],
        || {
            assert_eq!(node_paths::runtime_dir(None), "/srv/hacs");
        },
    );
}

#[test]
fn runtime_dir_defaults_to_existing_dev_dir() {
    with_env(&[], &[node_paths::ENV_RUNTIME_DIR], || {
        let default = node_paths::runtime_dir(None);
        assert_eq!(default, node_paths::dev_deploy_dir());
        // 本件约束「默认值必须是一个已存在的目录，且不建目录」
        assert!(std::path::Path::new(&default).is_dir());
    });
}

#[test]
fn etc_dirs_shape_per_flavour() {
    with_env(
        &[],
        &[node_paths::ENV_ETC_DIR, node_paths::ENV_RUNTIME_DIR],
        || {
            // posix 兜底 = 开发占位；nt 兜底 = 运行目录
            assert_eq!(
                node_paths::etc_dirs(Some("posix")),
                vec![node_paths::posix_etc_dir()]
            );
            assert_eq!(
                node_paths::etc_dirs(Some("nt")),
                vec![node_paths::runtime_dir(Some("nt"))]
            );
        },
    );
}

#[test]
fn etc_dirs_env_is_single_candidate() {
    with_env(
        &[(node_paths::ENV_ETC_DIR, "/etc/hacs")],
        &[node_paths::ENV_ETC_DIR],
        || {
            assert_eq!(node_paths::etc_dirs(None), vec!["/etc/hacs".to_string()]);
            assert_eq!(node_paths::etc_dir(None), "/etc/hacs");
        },
    );
}

#[test]
fn log_dir_joins_runtime_and_logs() {
    with_env(
        &[(node_paths::ENV_RUNTIME_DIR, "/srv/hacs")],
        &[node_paths::ENV_RUNTIME_DIR, node_paths::ENV_LOG_DIR],
        || {
            let sep = std::path::MAIN_SEPARATOR;
            assert_eq!(node_paths::log_dir(None), format!("/srv/hacs{sep}logs"));
        },
    );
    with_env(
        &[(node_paths::ENV_LOG_DIR, "/var/log/hacs")],
        &[node_paths::ENV_LOG_DIR],
        || {
            assert_eq!(node_paths::log_dir(None), "/var/log/hacs");
        },
    );
}

#[test]
fn nats_dir_is_platform_specific() {
    with_env(
        &[],
        &[node_paths::ENV_NATS_DIR, node_paths::ENV_RUNTIME_DIR],
        || {
            assert_eq!(node_paths::nats_dir(Some("posix")), "/etc/nats");
            let sep = std::path::MAIN_SEPARATOR;
            assert_eq!(
                node_paths::nats_dir(Some("nt")),
                format!("{}{sep}nats", node_paths::runtime_dir(Some("nt")))
            );
        },
    );
}

#[test]
fn claude_exec_shapes_per_flavour() {
    with_env(
        &[("USERPROFILE", "C:/Users/dev"), ("HOME", "/home/dev")],
        &[node_paths::ENV_CLAUDE_EXEC, "USERPROFILE", "HOME"],
        || {
            let nt = node_paths::claude_exec(Some("nt"));
            let posix = node_paths::claude_exec(Some("posix"));
            assert!(nt.ends_with("claude_exec.py"));
            assert!(nt.contains("AppData"));
            assert!(posix.ends_with("claude_exec.py"));
            assert!(posix.contains(".local"));
        },
    );
}

#[test]
fn codex_cmd_exe_shapes() {
    with_env(
        &[],
        &[node_paths::ENV_CODEX_CMD, node_paths::ENV_CODEX_EXE],
        || {
            assert!(node_paths::codex_cmd(Some("nt")).ends_with("codex.cmd"));
            assert!(node_paths::codex_cmd(Some("posix")).ends_with("codex"));
            assert!(node_paths::codex_exe(Some("nt")).ends_with("codex.exe"));
            assert!(node_paths::codex_exe(Some("posix")).ends_with("codex"));
        },
    );
}

#[test]
fn roster_keeps_canonical_order() {
    // 顺序 == 中枢 conf 里 users 的规范序（字典序）；不许再排一次。
    let names = member_roster::members();
    assert_eq!(
        names,
        vec![
            "claudecode",
            "codex",
            "hermes",
            "lobster",
            "openclaw-nb",
            "piagent",
            "quantum",
            "top001"
        ]
    );
    let mut sorted = names.clone();
    sorted.sort_unstable();
    assert_eq!(names, sorted, "档案顺序必须已是字典序");
    assert_eq!(member_roster::member_names(), names);
}

#[test]
fn roster_queries_do_not_guess() {
    assert_eq!(member_roster::role_of("lobster"), Some("member"));
    assert_eq!(
        member_roster::cred_spec("lobster"),
        Some(("lobster", "conf"))
    );
    assert_eq!(
        member_roster::cred_spec("piagent"),
        Some(("thinkbook", "runtime"))
    );
    assert_eq!(member_roster::entry("nobody"), None);
    assert_eq!(member_roster::role_of("nobody"), None);
    assert_eq!(member_roster::cred_spec("nobody"), None);
}

#[test]
fn roster_schema_and_vocabulary_frozen() {
    assert_eq!(member_roster::SCHEMA, "l5-roster-archive/1");
    assert_eq!(member_roster::ROLES, &["member"]);
    assert_eq!(member_roster::CRED_KINDS, &["runtime", "conf"]);
    assert_eq!(
        member_roster::CRED_NODES,
        &["thinkbook", "lobster", "quantum", "top001"]
    );
}
