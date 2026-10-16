//! 极简诊断日志（对应 Python 各件的 `logging.getLogger(...).debug`）。
//!
//! 降级路径必须留痕（「读不到」与「读坏了」要能分开看），但默认不该噪声化 stdout。
//! 口径：`HACS_DEBUG` 非空且不是 `0` 时，把 debug 行写到 **stderr**。

use std::sync::OnceLock;

fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| match std::env::var("HACS_DEBUG") {
        Ok(v) => !v.is_empty() && v != "0",
        Err(_) => false,
    })
}

/// 写一行 debug（默认关闭）。
pub fn debug(target: &str, message: &str) {
    if enabled() {
        eprintln!("[{target}] {message}");
    }
}
