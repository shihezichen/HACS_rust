//! L2 模型执行器适配等价测试（对照 Python `hub/l2_adapter/agent_model.py`）。
//!
//! 判据：命令行**选项顺序**是契约 · 远端执行器路径按**目标主机**取 · 输出解析的校验与
//! 截断口径 · 失败一律降级成 `(None, 原因)` 而绝不抛。

use std::sync::Mutex;

use hacs_l2_adapter::agent_model as am;

static ENV_LOCK: Mutex<()> = Mutex::new(());

const ENV_KEYS: &[&str] = &[
    "CHAT_ENRICH_ENABLE",
    "CHAT_ENRICH_SSH_BIN",
    "CHAT_ENRICH_SSH",
    "CHAT_ENRICH_KEY",
    "CHAT_ENRICH_KNOWN_HOSTS",
    "CHAT_ENRICH_CMD",
    "CHAT_ENRICH_TIMEOUT_S",
    "CHAT_ENRICH_MIN_INTERVAL_S",
    "AGENT_TEAM_SSH_TARGET",
    "AGENT_TEAM_ETC_DIR",
    "AGENT_TEAM_DISPLAY_NAMES",
];

/// 清掉模型通道相关 env，再按 `set` 覆盖，跑断言后还原。
fn with_env<F: FnOnce()>(set: &[(&str, &str)], f: F) {
    let _guard = ENV_LOCK.lock().unwrap();
    let saved: Vec<(String, Option<String>)> = ENV_KEYS
        .iter()
        .map(|k| ((*k).to_string(), std::env::var(k).ok()))
        .collect();
    for k in ENV_KEYS {
        std::env::remove_var(k);
    }
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
    let dir = std::env::temp_dir().join(format!("hacs_l2am_{}_{tag}_{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn enabled_switch_recognizes_the_off_words() {
    with_env(&[], || assert!(am::enabled()));
    for off in ["0", "false", "no", "off", "FALSE", "Off"] {
        with_env(&[(am::ENABLE_ENV, off)], || {
            assert!(!am::enabled(), "{off} 应关")
        });
    }
    with_env(&[(am::ENABLE_ENV, "1")], || assert!(am::enabled()));
    with_env(&[(am::ENABLE_ENV, "")], || assert!(am::enabled()));
}

#[test]
fn timeout_and_interval_are_clamped() {
    with_env(&[], || assert_eq!(am::timeout_s(), 240.0));
    with_env(&[(am::TIMEOUT_ENV, "1")], || {
        assert_eq!(am::timeout_s(), 5.0)
    });
    with_env(&[(am::TIMEOUT_ENV, "5000")], || {
        assert_eq!(am::timeout_s(), 1800.0)
    });
    with_env(&[(am::TIMEOUT_ENV, "abc")], || {
        assert_eq!(am::timeout_s(), 240.0)
    });

    with_env(&[], || assert_eq!(am::min_interval_s(), 1.0));
    with_env(&[(am::MIN_INTERVAL_ENV, "-1")], || {
        assert_eq!(am::min_interval_s(), 0.0)
    });
    with_env(&[(am::MIN_INTERVAL_ENV, "2.5")], || {
        assert_eq!(am::min_interval_s(), 2.5)
    });
}

#[test]
fn ssh_argv_order_is_a_contract() {
    let etc = unique_temp_dir("etc");
    let etc_str = etc.to_str().unwrap();
    with_env(
        &[
            ("AGENT_TEAM_ETC_DIR", etc_str),
            ("AGENT_TEAM_SSH_TARGET", "lobster"),
        ],
        || {
            let expected = vec![
                "/usr/bin/ssh".to_string(),
                "-o".to_string(),
                "BatchMode=yes".to_string(),
                "-o".to_string(),
                "ConnectTimeout=10".to_string(),
                "-i".to_string(),
                format!("{etc_str}/ssh/id_ed25519"),
                "-o".to_string(),
                "IdentitiesOnly=yes".to_string(),
                "-o".to_string(),
                format!("UserKnownHostsFile={etc_str}/ssh/known_hosts"),
                "lobster".to_string(),
                "python -X utf8 /root/.local/share/hermes/scripts/claude_exec.py --timeout 150 -"
                    .to_string(),
            ];
            assert_eq!(am::ssh_argv(), expected);
        },
    );
}

#[test]
fn remote_command_follows_the_effective_target_host() {
    let etc = unique_temp_dir("etc2");
    let etc_str = etc.to_str().unwrap();
    // 目标 = thinkbook（Windows）⇒ 远端路径必须是 thinkbook 的真值，不是本机的
    with_env(
        &[
            ("AGENT_TEAM_ETC_DIR", etc_str),
            ("CHAT_ENRICH_SSH", "thinkbook"),
        ],
        || {
            let argv = am::ssh_argv();
            let remote = argv.last().unwrap();
            assert!(
                remote.contains("AppData/Local/hermes/scripts/claude_exec.py"),
                "{remote}"
            );
            assert!(
                !remote.contains("/root/"),
                "不该用 posix 目标机路径: {remote}"
            );
        },
    );
    // CHAT_ENRICH_CMD 仍是最外层覆盖
    with_env(
        &[
            ("AGENT_TEAM_ETC_DIR", etc_str),
            ("AGENT_TEAM_SSH_TARGET", "lobster"),
            ("CHAT_ENRICH_CMD", "echo hello"),
        ],
        || {
            assert_eq!(am::ssh_argv().last().unwrap(), "echo hello");
        },
    );
}

#[test]
fn parse_accepts_noisy_output_and_normalizes_fields() {
    let raw = r#"好的，结论如下：
    {"is_task": true, "task_name": "  ，部署新版本。  ", "summary": "  把 服务   重启  ",
     "confidence": 0.83}
    以上。"#;
    let v = am::parse(raw).expect("应能解析");
    assert_eq!(v["is_task"], true);
    assert_eq!(v["task_name"], "部署新版本"); // 折叠空白 + 剥掉两端装饰字符
    assert_eq!(v["summary"], "把 服务 重启");
    assert_eq!(v["confidence"], 0.83);
    assert!(v["elapsed_s"].is_null()); // elapsed_s 由 call() 补，parse 不产
}

#[test]
fn parse_reads_string_booleans_like_python() {
    let t = |raw: &str| am::parse(raw).unwrap()["is_task"].clone();
    assert_eq!(t(r#"{"is_task":"true","task_name":"x"}"#), true);
    assert_eq!(t(r#"{"is_task":"YES","task_name":"x"}"#), true);
    assert_eq!(t(r#"{"is_task":"是","task_name":"x"}"#), true);
    assert_eq!(t(r#"{"is_task":"1","task_name":"x"}"#), true);
    assert_eq!(t(r#"{"is_task":"0"}"#), false);
    assert_eq!(t(r#"{"is_task":"maybe"}"#), false);
}

#[test]
fn parse_rejects_unqualified_output() {
    // 没有 is_task 键
    assert!(am::parse(r#"{"task_name":"x"}"#).is_none());
    assert!(am::parse("{}").is_none());
    // 没有花括号 / 大括号顺序反了
    assert!(am::parse("no json here").is_none());
    assert!(am::parse("} {").is_none());
    assert!(am::parse("   ").is_none());
    // 判是任务却不给名字：不算合格
    assert!(am::parse(r#"{"is_task": true, "task_name": "  "}"#).is_none());
}

#[test]
fn parse_clamps_confidence_and_truncates() {
    let v = am::parse(r#"{"is_task": false, "confidence": 2.7}"#).unwrap();
    assert_eq!(v["confidence"], 1.0);
    let v = am::parse(r#"{"is_task": false, "confidence": -3}"#).unwrap();
    assert_eq!(v["confidence"], 0.0);
    // float("abc") 失败 ⇒ 0.0（不抛）
    let v = am::parse(r#"{"is_task": false, "confidence": "abc"}"#).unwrap();
    assert_eq!(v["confidence"], 0.0);

    let long_name = "名".repeat(50);
    let v = am::parse(&format!(
        r#"{{"is_task": true, "task_name": "{long_name}"}}"#
    ))
    .unwrap();
    assert_eq!(
        v["task_name"].as_str().unwrap().chars().count(),
        am::NAME_MAX
    );

    let huge = format!(
        "{{\"is_task\": false, \"summary\": \"{}\"}}",
        "s".repeat(2000)
    );
    let v = am::parse(&huge).unwrap();
    assert_eq!(v["raw"].as_str().unwrap().chars().count(), 600);
}

// ------------------------------------------------------------------ 失败即降级

#[test]
fn call_returns_disabled_without_touching_the_network() {
    with_env(&[(am::ENABLE_ENV, "0")], || {
        let (verdict, why) = am::call("hi", Some(1.0));
        assert!(verdict.is_none());
        assert_eq!(why, "disabled");
    });
}

#[test]
fn call_reports_missing_ssh_binary_instead_of_panicking() {
    let etc = unique_temp_dir("etc3");
    let bin = unique_temp_dir("nobin").join("no-such-ssh");
    with_env(
        &[
            ("AGENT_TEAM_ETC_DIR", etc.to_str().unwrap()),
            (am::SSH_BIN_ENV, bin.to_str().unwrap()),
        ],
        || {
            let (verdict, why) = am::call("hi", Some(2.0));
            assert!(verdict.is_none());
            assert!(why.starts_with("ssh_missing:"), "{why}");
        },
    );
}

#[test]
fn probe_flags_unknown_target_and_never_throws() {
    let etc = unique_temp_dir("etc4");
    let bin = unique_temp_dir("nobin2").join("no-such-ssh");
    with_env(
        &[
            ("AGENT_TEAM_ETC_DIR", etc.to_str().unwrap()),
            (am::SSH_BIN_ENV, bin.to_str().unwrap()),
            (am::SSH_ENV, "nosuchhost"),
        ],
        || {
            let report = am::probe(2.0);
            assert_eq!(report["ok"], false);
            assert_eq!(report["target"], "nosuchhost");
            assert!(report["argv"].is_array());
            let why = report["why"].as_str().unwrap();
            assert!(why.contains("target_unknown:nosuchhost"), "{why}");
            assert!(report["parsed"].is_null());
        },
    );
}

// ------------------------------------------------------------------ 展示名

#[test]
fn display_name_falls_back_to_the_id() {
    with_env(&[], || {
        assert_eq!(am::display_name("lobster"), "龙虾");
        assert_eq!(am::display_name("codex"), "Codex");
        assert_eq!(am::display_name("whoever"), "whoever");
        assert_eq!(am::display_name(""), "");
    });
}

#[test]
fn display_names_env_override_wins() {
    with_env(
        &[(
            "AGENT_TEAM_DISPLAY_NAMES",
            r#"{"lobster":"大龙虾","newbie":"新人"}"#,
        )],
        || {
            assert_eq!(am::display_name("lobster"), "大龙虾");
            assert_eq!(am::display_name("newbie"), "新人");
            // 兜底表里其余的仍在
            assert_eq!(am::display_name("hermes"), "Hermes");
        },
    );
    // 环境还原后必须回到兜底表（缓存不能串味）
    with_env(&[], || assert_eq!(am::display_name("lobster"), "龙虾"));
}
