//! L0 节点档案等价测试（对照 Python `node/l0_host/node_profile.py`）。
//!
//! 只读与不编造两条铁律是判据的核心：拿不到就 `None`/空串，真出错原样抛。

use std::sync::Mutex;

use hacs_l0_host::node_profile as np;

static ENV_LOCK: Mutex<()> = Mutex::new(());

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

fn unique_temp_dir(tag: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::SeqCst);
    let dir =
        std::env::temp_dir().join(format!("hacs_rust_test_{}_{}_{n}", std::process::id(), tag));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn target_host_strips_user_and_lowercases() {
    assert_eq!(np::target_host("user@LOBSTER"), "lobster");
    assert_eq!(np::target_host("  Top001 "), "top001");
    assert_eq!(np::target_host("thinkbook"), "thinkbook");
}

#[test]
fn known_target_gets_archive_values() {
    with_env(&[], &[np::ENV_TARGET_PROFILE_JSON], || {
        let (profile, known) = np::target_profile("thinkbook");
        assert!(known);
        assert_eq!(profile.flavour, "nt");
        assert_eq!(profile.home, np::NT_TARGET_HOME);
        assert!(profile.claude_exec.contains("AppData"));

        let (p2, k2) = np::target_profile("lobster");
        assert!(k2);
        assert_eq!(p2.flavour, "posix");
        assert_eq!(p2.home, np::POSIX_TARGET_HOME);
        assert_eq!(np::target_flavour("lobster"), "posix");
        assert_eq!(np::target_home("lobster"), "/root");
        assert_eq!(np::target_claude_exec("lobster"), p2.claude_exec);
    });
}

#[test]
fn unknown_target_falls_back_to_posix_and_reports_unknown() {
    with_env(&[], &[np::ENV_TARGET_PROFILE_JSON], || {
        // 不编造：未登记主机按 posix 兜底，但 target_known() 必须为 false。
        assert!(!np::target_known("some-random-host"));
        assert_eq!(
            np::target_flavour("some-random-host"),
            np::TARGET_FALLBACK_FLAVOUR
        );
        assert_eq!(np::target_home("some-random-host"), np::POSIX_TARGET_HOME);
        assert!(np::target_claude_exec("some-random-host").ends_with("claude_exec.py"));
    });
}

#[test]
fn target_table_env_override_replaces_whole_table() {
    let json = r#"{"mybox":{"flavour":"nt","home":"C:/Users/x","claude_exec":"C:/Users/x/ce.py"}}"#;
    with_env(
        &[(np::ENV_TARGET_PROFILE_JSON, json)],
        &[np::ENV_TARGET_PROFILE_JSON],
        || {
            assert!(np::target_known("mybox"));
            assert_eq!(np::target_home("mybox"), "C:/Users/x");
            assert_eq!(np::target_claude_exec("mybox"), "C:/Users/x/ce.py");
            // 整表覆盖：内置表已被替换
            assert!(!np::target_known("lobster"));
        },
    );
}

#[test]
fn broken_target_table_json_falls_back_to_builtin() {
    with_env(
        &[(np::ENV_TARGET_PROFILE_JSON, "{not json")],
        &[np::ENV_TARGET_PROFILE_JSON],
        || {
            // 覆盖值坏了就退回内置表，绝不抛给调用方
            assert!(np::target_known("lobster"));
        },
    );
}

#[test]
fn target_profile_fills_missing_keys_by_flavour() {
    let json = r#"{"partial":{"flavour":"nt"}}"#;
    with_env(
        &[(np::ENV_TARGET_PROFILE_JSON, json)],
        &[np::ENV_TARGET_PROFILE_JSON],
        || {
            let (p, known) = np::target_profile("partial");
            assert!(known);
            assert_eq!(p.home, np::NT_TARGET_HOME);
            assert!(p
                .claude_exec
                .ends_with("AppData/Local/hermes/scripts/claude_exec.py"));
        },
    );
}

#[test]
fn openclaw_session_key_format_is_frozen() {
    assert_eq!(
        np::openclaw_session_key("claudecode", "s-1"),
        "agent:main:openai-user:a2a:claudecode:s-1"
    );
    assert_eq!(
        np::openclaw_session_key("", "s-1"),
        "agent:main:openai-user:a2a:unknown:s-1"
    );
}

#[test]
fn hermes_config_model_window_parses_only_model_section() {
    let dir = unique_temp_dir("hermes_cfg");
    let cfg = dir.join("config.yaml");
    std::fs::write(
        &cfg,
        "# comment\nother: 1\nmodel:\n  default: \"gpt-x\"\n  context_length: 128000\n  extra: 9\nnotmodel:\n  context_length: 5\n",
    )
    .unwrap();

    with_env(
        &[("A2A_HERMES_HOME", dir.to_str().unwrap())],
        &["A2A_HERMES_HOME"],
        || {
            let (name, len) = np::hermes_config_model_window().unwrap();
            assert_eq!(name, "gpt-x");
            assert_eq!(len, 128000); // `notmodel` 段里的 5 不该被采用
        },
    );
}

#[test]
fn hermes_config_absent_returns_empty_not_error() {
    let dir = unique_temp_dir("hermes_no_cfg");
    with_env(
        &[("A2A_HERMES_HOME", dir.to_str().unwrap())],
        &["A2A_HERMES_HOME"],
        || {
            assert_eq!(
                np::hermes_config_model_window().unwrap(),
                (String::new(), 0)
            );
        },
    );
}

#[test]
fn openclaw_session_entry_reads_only_object_entries() {
    let dir = unique_temp_dir("openclaw");
    let file = dir.join("sessions.json");
    std::fs::write(&file, r#"{"k1":{"a":1},"k2":"nope"}"#).unwrap();

    let entry = np::openclaw_session_entry("k1", Some(file.to_str().unwrap())).unwrap();
    assert!(entry.is_some());
    assert_eq!(entry.unwrap()["a"], 1);

    // 非对象条目 → None（不猜）
    assert!(
        np::openclaw_session_entry("k2", Some(file.to_str().unwrap()))
            .unwrap()
            .is_none()
    );
    // key 不在 → None
    assert!(
        np::openclaw_session_entry("missing", Some(file.to_str().unwrap()))
            .unwrap()
            .is_none()
    );
    // 文件不在 → None（不是错）
    let missing = dir.join("nope.json");
    assert!(
        np::openclaw_session_entry("k1", Some(missing.to_str().unwrap()))
            .unwrap()
            .is_none()
    );
}

#[test]
fn hermes_probes_are_read_only_and_do_not_guess() {
    let dir = unique_temp_dir("hermes_db");
    let db = dir.join("state.db");
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (session_key TEXT, id TEXT, model TEXT, input_tokens INTEGER,
                 cache_read_tokens INTEGER, api_call_count INTEGER, last_activity_at TEXT, started_at TEXT);
             CREATE TABLE messages (session_id TEXT, content TEXT, tool_calls TEXT,
                 reasoning_content TEXT, active INTEGER);
             INSERT INTO sessions VALUES ('key1','sid1','m-1',111,22,3,'2026-01-02','2026-01-01');
             INSERT INTO sessions VALUES ('key1','sid0','m-0',1,2,3,'2025-01-01','2025-01-01');
             INSERT INTO messages VALUES ('sid1','hello','[]',NULL,1);
             INSERT INTO messages VALUES ('sid1','stale',NULL,NULL,0);",
        )
        .unwrap();
    }

    let path = db.to_str().unwrap().to_string();

    // 用量行：取 last_activity_at 最大的那条
    let row = np::hermes_session_usage_row("key1", Some(&path))
        .unwrap()
        .unwrap();
    assert_eq!(row.0, Some(111));
    assert_eq!(row.1, Some(22));
    assert_eq!(row.2, Some(3));
    assert_eq!(row.3.as_deref(), Some("m-1"));

    // 活跃消息：只取 active = 1
    let (rows, model) = np::hermes_active_messages("key1", Some(&path))
        .unwrap()
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0.as_deref(), Some("hello"));
    assert_eq!(model, "m-1");

    // 记录不在 → None（「没有」不是「坏了」）
    assert!(np::hermes_session_usage_row("nope", Some(&path))
        .unwrap()
        .is_none());

    // 文件不在 → None
    assert!(
        np::hermes_session_usage_row("key1", Some("Z:/nope/state.db"))
            .unwrap()
            .is_none()
    );

    // 只读铁律：拿到的句柄写不进去
    let conn = np::open_hermes_state_ro(Some(&path)).unwrap().unwrap();
    assert!(conn
        .execute("INSERT INTO sessions (session_key) VALUES ('x')", [])
        .is_err());
    drop(conn);
}
