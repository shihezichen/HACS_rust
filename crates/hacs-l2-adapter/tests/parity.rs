//! L2 适配层等价测试（对照 Python `pg_literals.py` / `agent_usage.py`）。
//!
//! 判据：字面量逐字节对齐 Python（含 `repr(float)` 的指数写法与四舍六入五成双）、
//! 时间归一「裸时间按 UTC」、分母链顺序、读不到一律 `None`（不编造）。

use std::sync::Mutex;

use hacs_l2_adapter::{agent_usage, pg_literals};

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
    let dir = std::env::temp_dir().join(format!("hacs_l2_test_{}_{tag}_{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

// ------------------------------------------------------------------ pg_literals

#[test]
fn vector_literal_matches_python_bytes() {
    assert_eq!(pg_literals::vector_literal(None), None);
    assert_eq!(
        pg_literals::vector_literal(Some(&[])),
        Some("[]".to_string())
    );
    assert_eq!(
        pg_literals::vector_literal(Some(&[0.1, 1.0, -2.5])),
        Some("[0.1,1.0,-2.5]".to_string())
    );
    // 非数拼写与 Python 一致（小写）
    assert_eq!(
        pg_literals::vector_literal(Some(&[f64::NAN, f64::INFINITY, f64::NEG_INFINITY])),
        Some("[nan,inf,-inf]".to_string())
    );
}

#[test]
fn py_repr_float_index_notation_matches_python() {
    // Python: repr(1.0)='1.0' / repr(1e16)='1e+16' / repr(1e-5)='1e-05' / repr(1e300)='1e+300'
    assert_eq!(pg_literals::py_repr_float(1.0), "1.0");
    assert_eq!(pg_literals::py_repr_float(0.1), "0.1");
    assert_eq!(pg_literals::py_repr_float(-0.0), "-0.0");
    assert_eq!(pg_literals::py_repr_float(1e16), "1e+16");
    assert_eq!(pg_literals::py_repr_float(1e-5), "1e-05");
    assert_eq!(pg_literals::py_repr_float(1e300), "1e+300");
    assert_eq!(pg_literals::py_repr_float(1e15), "1000000000000000.0");
}

#[test]
fn to_utc_normalizes_to_utc_and_rejects_junk() {
    let z = pg_literals::to_utc("2026-09-27T12:34:56Z").unwrap();
    // 同义写法归一
    assert_eq!(z, pg_literals::to_utc("2026-09-27T12:34:56+00:00").unwrap());
    assert_eq!(z, pg_literals::to_utc("2026-09-27 12:34:56").unwrap());
    assert_eq!(z, pg_literals::to_utc("20260927T123456Z").unwrap());
    // 带偏移：东八区的 20:34 就是 UTC 的 12:34
    assert_eq!(z, pg_literals::to_utc("2026-09-27T20:34:56+08:00").unwrap());
    assert_eq!(z.to_isoformat(), "2026-09-27T12:34:56+00:00");

    assert_eq!(
        pg_literals::to_utc("2026-09-27").unwrap().to_isoformat(),
        "2026-09-27T00:00:00+00:00"
    );
    assert_eq!(
        pg_literals::to_utc("2026-09-27T12:34:56.789Z")
            .unwrap()
            .to_isoformat(),
        "2026-09-27T12:34:56.789000+00:00"
    );

    // 认不出来 → None，不猜
    assert!(pg_literals::to_utc("").is_none());
    assert!(pg_literals::to_utc("not-a-date").is_none());
    assert!(pg_literals::to_utc("2026-13-01T00:00:00Z").is_none());
    assert!(pg_literals::to_utc("2026-09-27T25:00:00Z").is_none());
}

// ------------------------------------------------------------------ agent_usage

#[test]
fn token_estimate_matches_hermes_formula() {
    assert_eq!(agent_usage::estimate_tokens_rough(""), 0);
    assert_eq!(agent_usage::estimate_tokens_rough("abcd"), 1);
    assert_eq!(agent_usage::estimate_tokens_rough("abcdefgh"), 2);
    assert_eq!(agent_usage::estimate_tokens_rough("a"), 1); // ceil(1/4)=1
                                                            // CJK 密集 ≈1 token/字，其余 ceil(bytes/4)
    assert_eq!(agent_usage::estimate_tokens_rough("你好"), 2);
    assert_eq!(agent_usage::estimate_tokens_rough("你好abcd"), 3);
}

#[test]
fn fmt_uses_python_bankers_rounding() {
    // 100 * 125000 / 200000 = 62.5 → Python round 取偶 → 62（不是 63）
    let v = agent_usage::fmt(125_000, 200_000);
    assert_eq!(v["used"], 125_000);
    assert_eq!(v["window"], 200_000);
    assert_eq!(v["pct"], 62);
    assert_eq!(v["text"], "上下文：约 125k/200k (62%)");

    // 100 * 1 / 8 = 12.5 → 12
    assert_eq!(agent_usage::fmt(1, 8)["pct"], 12);
    // 100 * 3 / 8 = 37.5 → 38（取偶方向为偶）
    assert_eq!(agent_usage::fmt(3, 8)["pct"], 38);
}

#[test]
fn window_for_walks_the_fallback_chain_by_longest_match() {
    let home = unique_temp_dir("no_cfg"); // 无 config.yaml ⇒ 生效窗口取不到
    with_env(
        &[
            ("A2A_HERMES_HOME", home.to_str().unwrap()),
            ("HERMES_HOME", home.to_str().unwrap()),
        ],
        &["A2A_HERMES_HOME", "HERMES_HOME"],
        || {
            assert_eq!(
                agent_usage::window_for("deepseek-v4-flash", "other"),
                200_000
            );
            assert_eq!(agent_usage::window_for("deepseek-v4-pro", "other"), 200_000);
            // 最长匹配优先：只有 "deepseek" 命中过
            assert_eq!(
                agent_usage::window_for("my-deepseek-thing", "other"),
                128_000
            );
            assert_eq!(agent_usage::window_for("", "other"), 0);
            assert_eq!(agent_usage::window_for("some-unknown", "other"), 0);
        },
    );
}

#[test]
fn window_for_prefers_deployment_config_only_for_hermes_backend() {
    let home = unique_temp_dir("cfg");
    std::fs::write(
        home.join("config.yaml"),
        "model:\n  default: hermes-agent\n  context_length: 777000\n",
    )
    .unwrap();
    with_env(
        &[
            ("A2A_HERMES_HOME", home.to_str().unwrap()),
            ("HERMES_HOME", home.to_str().unwrap()),
        ],
        &["A2A_HERMES_HOME", "HERMES_HOME"],
        || {
            // ① 生效配置命中模型名 → 用 config 的分母
            assert_eq!(agent_usage::window_for("hermes-agent", "hermes"), 777_000);
            // ④ 别名兜底（仅 hermes 后端）：名字查不到时用该实例生效窗口
            assert_eq!(
                agent_usage::window_for("total-alias-name", "hermes"),
                777_000
            );
            // 非 hermes 后端不做别名兜底，避免把本机窗口套给别的后端
            assert_eq!(agent_usage::window_for("total-alias-name", "openclaw"), 0);
        },
    );
}

#[test]
fn backend_kind_dispatches_by_port_then_model_then_archive() {
    let missing = unique_temp_dir("no_sessions").join("sessions.json");
    with_env(
        &[("A2A_SESSIONS_JSON", missing.to_str().unwrap())],
        &["A2A_SESSIONS_JSON"],
        || {
            let cfg = |s: &str| serde_json::from_str::<serde_json::Value>(s).unwrap();
            assert_eq!(
                agent_usage::backend_kind(Some(&cfg(r#"{"backend":"http://127.0.0.1:8642"}"#))),
                "hermes"
            );
            assert_eq!(
                agent_usage::backend_kind(Some(&cfg(r#"{"backend":"http://127.0.0.1:18789"}"#))),
                "openclaw"
            );
            // 各机 OpenClaw 端口不统一，故模型名关键词也认
            assert_eq!(
                agent_usage::backend_kind(Some(&cfg(r#"{"model":"openclaw-x"}"#))),
                "openclaw"
            );
            // 会话档案不在 ⇒ 不能兜底成 openclaw
            assert_eq!(
                agent_usage::backend_kind(Some(&cfg(r#"{"backend":"http://other:1"}"#))),
                "other"
            );
        },
    );
}

#[test]
fn session_key_and_headers_are_frozen() {
    assert_eq!(
        agent_usage::session_key("claudecode", Some("s-1")),
        "a2a:claudecode:s-1"
    );
    assert_eq!(agent_usage::session_key("", Some("s-1")), "a2a:unknown:s-1");

    // 空 session_id ⇒ 不注入任何头（一次性会话）
    assert!(agent_usage::hermes_session_headers("claudecode", None).is_empty());
    assert!(agent_usage::hermes_session_headers("claudecode", Some("")).is_empty());

    let headers = agent_usage::hermes_session_headers("claudecode", Some("s-1"));
    assert_eq!(headers.len(), 2);
    assert_eq!(headers[0].0, "X-Hermes-Session-Key");
    assert_eq!(headers[1].0, "X-Hermes-Session-Id");
    assert_eq!(headers[0].1, "a2a:claudecode:s-1");
}

#[test]
fn unknown_backend_yields_no_watermark_instead_of_guessing() {
    let missing = unique_temp_dir("no_sessions2").join("sessions.json");
    with_env(
        &[("A2A_SESSIONS_JSON", missing.to_str().unwrap())],
        &["A2A_SESSIONS_JSON"],
        || {
            let cfg: serde_json::Value =
                serde_json::from_str(r#"{"backend":"http://other:1"}"#).unwrap();
            // 未知后端：无水位数据源 ⇒ None（降级不自愈），绝不编造
            assert_eq!(
                agent_usage::read_usage("claudecode", "s-1", Some(&cfg), ""),
                None
            );
            assert_eq!(
                agent_usage::pressure("claudecode", "s-1", Some(&cfg), 8000),
                None
            );
        },
    );
}

#[test]
fn read_watermark_requires_both_ids() {
    assert!(agent_usage::read_watermark("", "s-1", None).is_none());
    assert!(agent_usage::read_watermark("claudecode", "", None).is_none());
}

#[test]
fn remote_timeout_clamps_bad_config_to_default() {
    with_env(
        &[("CTX_A2A_TIMEOUT_S", "999")],
        &["CTX_A2A_TIMEOUT_S"],
        || {
            assert_eq!(agent_usage::remote_timeout().as_secs_f64(), 1.5);
        },
    );
    with_env(
        &[("CTX_A2A_TIMEOUT_S", "3.5")],
        &["CTX_A2A_TIMEOUT_S"],
        || {
            assert_eq!(agent_usage::remote_timeout().as_secs_f64(), 3.5);
        },
    );
    with_env(
        &[("CTX_A2A_TIMEOUT_S", "abc")],
        &["CTX_A2A_TIMEOUT_S"],
        || {
            assert_eq!(agent_usage::remote_timeout().as_secs_f64(), 1.5);
        },
    );
}
