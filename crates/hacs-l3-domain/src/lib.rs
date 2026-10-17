//! # hacs-l3-domain —— HACS 协作域（Domain）
//!
//! 一句话职责：**人和 Agent 在一起干活的业务语义** —— 群聊 / 待回应账 / 任务 / 会议 /
//! 黑板 / 共享内容。这层才有「群」「任务」「会议」这些词。
//!
//! 对应 Python：`hub/l3_domain/`（`chat_svc` · `blackboard_svc` · `blackboard_view` ·
//! `task_svc` · `team_group` · `roundtable` · `rt_remind` · `shared_content` · `embedder` ·
//! `backfill_embeddings` · `group_watch_pending` · `session_ctx_archiver`）。
//!
//! ## 迁移状态
//!
//! ⏳ 待迁移。这是内存占用的大头（Python 侧 `chat_svc` 3954 行 / `blackboard_svc` 3957 行 /
//! `blackboard_view` 3296 行），也是本工程收益最大的落点。

/// 迁移占位：本 crate 尚未实现（见 README 的迁移路线图）。
pub const MIGRATION_STATUS: &str = "pending";

pub mod blackboard_svc;
pub mod blackboard_view;
pub mod chat_svc;
pub mod task_svc;
pub mod task_svc_chat;
pub mod team_group;
