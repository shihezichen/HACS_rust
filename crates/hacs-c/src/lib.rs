//! # hacs-c —— HACS 组合根（Composition Root）
//!
//! 一句话职责：**进程入口装配** —— 只做装配与注入，不含业务与传输实现。
//!
//! 规则：`C` 可依赖任意层，**任何层不得依赖 `C`**。在 Rust 里这条是编译期事实：
//! 只有本 crate 的 `Cargo.toml` 列了全部层件，各层 crate 都列不到本 crate。
//!
//! 每个 Python 进程入口 ↔ 本 crate 的一个 `[[bin]]`（端口与单元名保持一字不差，
//! 便于逐台替换）：
//!
//! | bin | Python 原件 | 端口 |
//! |---|---|---|
//! | `a2a_svc` | `node/c/a2a_svc.py` | 9920 |
//! | `teamd` | `node/l1_transport/teamd.py` | 9960 |
//! | `nodelet` | `node/l0_host/nodelet.py` | 9971 |
//! | `chat_svc` | `hub/l3_domain/chat_svc.py` | 9980 / 9984 |
//! | `blackboard_svc` | `hub/l3_domain/blackboard_svc.py` | 9940 |
//! | `blackboard_view` | `hub/l3_domain/blackboard_view.py` | 9941 / 9942 |
//! | `task_svc` | `hub/l3_domain/task_svc.py` | 9981 |
//! | `cli_backend` | `node/l2_adapter/cli_backend.py` | 9970 |
//! | `group_watch` | `node/c/group_watch.py` | — |
//! | `roundtable` | `hub/l3_domain/roundtable.py` | 9930 |
//! | `rt_remind` | `hub/l3_domain/rt_remind.py` | 9930 |
//! | `team` | `node/c/team.py` | — |
//! | `session_ctx_archiver` | `hub/l3_domain/session_ctx_archiver.py` | — |
//!
//! ## 迁移状态
//!
//! ⏳ 待迁移：`[[bin]]` 目标随对应层件落地时逐个加入。

/// 迁移占位：本 crate 尚未实现（见 README 的迁移路线图）。
pub const MIGRATION_STATUS: &str = "pending";

/// 中枢侧凭据镜像通道纯逻辑（Python tools/roster_creds_mirror.py 对齐）。
pub mod creds_mirror;
