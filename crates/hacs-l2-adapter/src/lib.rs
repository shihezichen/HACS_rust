//! # hacs-l2-adapter —— HACS 适配层（Adapter）
//!
//! 一句话职责：**Agent 差异的唯一落脚点** —— 调用 / 能力 / 上下文用量 / 存储字面量归一。
//! 上层只看统一口径。
//!
//! 对应 Python：`node/l2_adapter/`（`agent_usage` · `cli_backend` · `ctx_watermark`）
//! 与 `hub/l2_adapter/`（`agent_model` · `pg_literals`）。
//!
//! | 模块 | 对应 Python | 职责 |
//! |---|---|---|
//! | [`pg_literals`] | `pg_literals.py` | pgvector 字面量与时间归一的**单一出处**（纯函数） |
//! | [`agent_usage`] | `agent_usage.py` | **上下文水位真源**（后端分派 / 分母链 / 远端回退护栏） |
//! | [`ctx_watermark`] | `ctx_watermark.py` | 水位薄壳（对外入口，实现全在 `agent_usage`） |
//! | [`agent_model`] | `agent_model.py` | **模型执行器适配**（ssh 命令行 / 子进程 / 输出解析）+ Agent 展示名 |
//!
//! ## 迁移状态
//!
//! ✅ 已迁移：`pg_literals` · `agent_usage` · `ctx_watermark` · `agent_model`
//!
//! ⏳ 待迁移：`cli_backend`（CLI 类 Agent 统一后端，落 `hacs-c` 的 bin 9970）

pub mod agent_model;
pub mod agent_usage;
pub mod ctx_watermark;
pub mod logx;
pub mod pg_literals;

/// 依赖方向：本层可用 `L0`（宿主事实）· `L1`（通信）· `X`（横切）。
pub use hacs_l0_host::{member_roster, node_paths, node_profile};
pub use hacs_l1_transport::http_client;
pub use hacs_x_identity::{safe_err, safe_err_with};
