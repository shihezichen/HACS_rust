//! # hacs-l1-transport —— HACS 通信层（Transport）
//!
//! 一句话职责：**消息怎么送到对方手里** —— 只管传输，不含任何群聊 / 会议语义。
//!
//! 对应 Python `node/l1_transport/` 与 `hub/l1_relay/`。
//!
//! | 模块 | 对应 Python | 职责 |
//! |---|---|---|
//! | [`nats_client`] | `nats_client.py` | 极简 NATS 文本协议客户端（多地址故障转移） |
//! | [`web_gateway`] | `web_gateway.py` | HTTP 入口件：路由 / JSON 信封 / 前置与鉴权钩子 |
//!
//! ## 迁移状态
//!
//! ✅ 已迁移：`nats_client` · `web_gateway`
//!
//! ⏳ 待迁移：`teamd` · `team_call` · `team_broadcast` · `team_roster` · `team_cli_core` ·
//! `group_watch_delivery` · `post_group` · `a2a/{a2a_edge,a2a_call}` · `gen_nats_conf`

pub mod nats_client;
pub mod web_gateway;

/// L0 平台层（宿主事实的唯一出处）—— 通信层按依赖方向引用它。
pub use hacs_l0_host::{member_roster, node_paths, node_profile};

/// L1 公共面契约（9 条 + 兼容规则）见 Python 版 `docs/l1-contract.md`；
/// Rust 侧对等契约在 `docs/PARITY.md` 里逐条登记。
pub const L1_CONTRACT_DOC: &str = "docs/PARITY.md";
