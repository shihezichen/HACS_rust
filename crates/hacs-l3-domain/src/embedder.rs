//! `embedder` —— 本地向量嵌入接口层（对应 Python `hub/l3_domain/embedder.py`）。
//!
//! 口径（=Python）：
//! * 惰性单例：导入只解析环境变量，第一次 embed_texts() 才尝试加载。
//! * 永不抛异常：任何失败统一返回 None，调用方自行降级（写入留 NULL、检索退关键词）。
//! * 加载失败是**粘性的**（记住 failed 不再重试），reset() 显式丢弃。
//! * 空列表原样返回 []，不触发加载。
//!
//! Rust 侧偏差（PARITY 50）：Python 走 fastembed/onnxruntime；Rust 遵守"零外部依赖"
//! 铁律不引 ONNX 运行时 —— 加载步恒定失败（last_error="fastembed 不可用（Rust 侧
//! 未内嵌 ONNX 运行时）"），embed_texts 恒 None，行为等价于 Python 的"没装 fastembed"
//! 路径。配置/状态/维度校验逻辑逐条对齐，接模型后只需替换 load()。

use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

pub const DEFAULT_MODEL: &str = "BAAI/bge-small-zh-v1.5";
pub const DEFAULT_DIM: i64 = 512;
pub const ERROR_MAX: usize = 200; // last_error 截断长度（=DETAIL_MAX 口径）

static FAILED: AtomicBool = AtomicBool::new(false);
static LOADED: AtomicBool = AtomicBool::new(false);
static LAST_ERROR: Mutex<Option<String>> = Mutex::new(None);

/// 当前模型名。每次读环境变量，便于测试里临时改。
pub fn model_name() -> String {
    std::env::var("BB_EMBED_MODEL")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_MODEL.to_string())
}

/// 期望维度。非法值（空/非数字/<=0）一律退回默认值，不抛。
pub fn embed_dim() -> i64 {
    let raw = std::env::var("BB_EMBED_DIM")
        .ok()
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    if raw.is_empty() {
        return DEFAULT_DIM;
    }
    match raw.parse::<i64>() {
        Ok(v) if v > 0 => v,
        _ => DEFAULT_DIM,
    }
}

fn set_err(msg: &str) {
    let mut e = LAST_ERROR.lock().unwrap();
    let text: String = msg.chars().take(ERROR_MAX).collect();
    *e = Some(text);
}

fn clear_err() {
    *LAST_ERROR.lock().unwrap() = None;
}

/// last_error 读出（status 用）。
pub fn last_error() -> Option<String> {
    LAST_ERROR.lock().unwrap().clone()
}

/// 惰性加载。Rust 侧：无内嵌 ONNX 运行时 → 粘性失败（=Python 没装 fastembed 路径）。
/// 接模型后在 `load_inner` 里实现真实加载，粘性/单例语义不变。
fn load() -> Option<()> {
    if LOADED.load(Ordering::SeqCst) {
        return Some(()); // 快路径
    }
    if FAILED.load(Ordering::SeqCst) {
        return None; // 粘性失败：不重试
    }
    match load_inner() {
        Some(()) => {
            LOADED.store(true, Ordering::SeqCst);
            clear_err();
            Some(())
        }
        None => {
            FAILED.store(true, Ordering::SeqCst);
            None
        }
    }
}

fn load_inner() -> Option<()> {
    set_err(&format!(
        "fastembed 导入失败：Rust 侧未内嵌 ONNX 运行时（模型 {}）",
        model_name()
    ));
    None
}

/// 丢掉已加载的模型与失败标记（测试 / 改完环境变量想重试）。
pub fn reset() {
    LOADED.store(false, Ordering::SeqCst);
    FAILED.store(false, Ordering::SeqCst);
    clear_err();
}

/// 把一批文本编码成向量。任何失败返回 None；空列表原样返回 []（不触发加载）。
pub fn embed_texts(texts: &[String]) -> Option<Vec<Vec<f32>>> {
    if texts.is_empty() {
        return Some(Vec::new());
    }
    if load().is_none() {
        return None;
    }
    let want = embed_dim() as usize;
    // 接模型后此处调用推理；现在到不了（load 恒 None）。
    let _ = want;
    None
}

/// 健康信息。**绝不触发加载**。available=尚未确证不可用；loaded=模型已在内存。
pub fn status() -> Value {
    json!({
        "available": !FAILED.load(Ordering::SeqCst),
        "model": model_name(),
        "dim": embed_dim(),
        "loaded": LOADED.load(Ordering::SeqCst),
        "last_error": last_error(),
    })
}

// ---------------- 单测 ----------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_defaults() {
        std::env::remove_var("BB_EMBED_MODEL");
        std::env::remove_var("BB_EMBED_DIM");
        assert_eq!(model_name(), DEFAULT_MODEL);
        assert_eq!(embed_dim(), DEFAULT_DIM);
        // 非法值退回默认
        std::env::set_var("BB_EMBED_DIM", "abc");
        assert_eq!(embed_dim(), DEFAULT_DIM);
        std::env::set_var("BB_EMBED_DIM", "0");
        assert_eq!(embed_dim(), DEFAULT_DIM);
        std::env::set_var("BB_EMBED_DIM", "-5");
        assert_eq!(embed_dim(), DEFAULT_DIM);
        std::env::set_var("BB_EMBED_DIM", "384");
        assert_eq!(embed_dim(), 384);
        std::env::set_var("BB_EMBED_MODEL", "  custom/model  ");
        assert_eq!(model_name(), "custom/model");
        std::env::remove_var("BB_EMBED_MODEL");
        std::env::set_var("BB_EMBED_DIM", "512");
    }

    #[test]
    fn test_embed_empty_no_load() {
        reset();
        assert_eq!(embed_texts(&[]), Some(Vec::new()));
        // 空列表不触发失败
        assert!(!last_error().is_some());
        reset();
    }

    #[test]
    fn test_sticky_failure_and_status() {
        reset();
        assert!(status()["available"].as_bool().unwrap()); // 还没失败过
        assert!(!status()["loaded"].as_bool().unwrap());
        let r = embed_texts(&["你好".to_string()]);
        assert!(r.is_none()); // 永不抛，返回 None
                              // 粘性：再试仍然 None 且不重置 last_error
        let e1 = last_error().unwrap();
        assert!(e1.contains("fastembed 导入失败"));
        let e1 = Some(e1);
        let _ = embed_texts(&["再来".to_string()]);
        assert_eq!(last_error(), e1);
        // status：available=false（已确证不可用）、loaded=false
        let st = status();
        assert!(!st["available"].as_bool().unwrap());
        assert!(!st["loaded"].as_bool().unwrap());
        assert_eq!(st["model"], json!(model_name()));
        assert_eq!(st["dim"], json!(embed_dim()));
        // reset 后恢复
        reset();
        assert!(status()["available"].as_bool().unwrap());
        assert!(last_error().is_none());
    }

    #[test]
    fn test_error_truncated() {
        reset();
        let _ = embed_texts(&["x".to_string()]);
        let e = last_error().unwrap();
        assert!(e.chars().count() <= ERROR_MAX);
        reset();
    }
}
