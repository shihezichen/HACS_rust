//! # hacs-l2-adapter —— HACS 适配层（Adapter）
//!
//! 一句话职责：**Agent 差异的唯一落脚点** —— 调用 / 能力 / 上下文用量 / 存储字面量归一。
//! 上层只看统一口径。
//!
//! 对应 Python：`node/l2_adapter/`（`agent_usage` · `cli_backend` · `ctx_watermark`）
//! 与 `hub/l2_adapter/`（`agent_model` · `pg_literals`）。
//!
//! ## 迁移状态
//!
//! ⏳ 待迁移。计划顺序：`pg_literals`（纯函数，最易）→ `agent_usage`（依赖 L0 只读探针，
//! 本 crate 已可承接）→ `agent_model`（子进程/ssh）→ `cli_backend`（HTTP 面 + 子进程）。

/// 迁移占位：本 crate 尚未实现（见 README 的迁移路线图）。
pub const MIGRATION_STATUS: &str = "pending";
