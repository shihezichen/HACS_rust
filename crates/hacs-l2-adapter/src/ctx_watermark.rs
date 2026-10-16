//! L2 适配层：**上下文水位薄壳**（对外入口）。对应 Python `node/l2_adapter/ctx_watermark.py`。
//!
//! Python 侧本件原先自带一整套 `state.db` / `sessions.json` 读取实现，与 `a2a_svc` 里那套
//! 是邻居 —— 同一件事两份代码，且窗口分母已漂移成两个值。现全部下沉到同层
//! [`crate::agent_usage`]（单一权威口径），本件只保留对外入口。
//!
//! 契约（对外不变）：`read_watermark(caller, session_id, agent_cfg) -> Option<Value>`，
//! 读不到返回 `None` —— 不渲染、**绝不编造数字**。

pub use crate::agent_usage::{
    backend_kind, estimate_tokens_rough, fmt, read_hermes, read_openclaw, read_remote,
    read_watermark, window_for, CTX_WINDOW_FALLBACK,
};

// 旧内部名保留为别名（迁移期兼容，勿新增引用）—— 实现全部在 agent_usage。
pub use crate::agent_usage::backend_kind as _backend_kind;
pub use crate::agent_usage::estimate_tokens_rough as _estimate_tokens_rough;
pub use crate::agent_usage::fmt as _fmt;
pub use crate::agent_usage::read_hermes as _read_hermes;
pub use crate::agent_usage::read_openclaw as _read_openclaw;
pub use crate::agent_usage::read_remote as _read_remote;
pub use crate::agent_usage::window_for as _window_for;
