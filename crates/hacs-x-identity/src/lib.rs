//! # hacs-x-identity —— HACS 横切层（X）：身份与授权 / 错误脱敏
//!
//! 对应 Python `hub/x/view_auth.py`：账号 / 会话 / 角色 / 密级 + `safe_err` 错误脱敏。
//!
//! **横切层不参与依赖方向判定** —— 可被任意层引用（见 `docs/ARCHITECTURE.md`）。
//!
//! | 模块 | 对应 Python | 职责 |
//! |---|---|---|
//! | [`redact`] | `view_auth.safe_err` | 错误文本脱敏（抹 DSN 口令再截断） |
//! | [`view_auth`] | `view_auth.py`（纯逻辑段） | 会话令牌签发/校验 · 角色与密级 · 口令哈希（scrypt）· 暴力破解防护 · IP 工具 · 时间口径 |
//!
//! ## 迁移状态
//!
//! 🟡 部分迁移：错误脱敏 + 认证纯逻辑已落地（`blackboard_view` 用它们）。
//!
//! ⏳ 待迁移：SQLite/Pg 存储访问与 CLI（归组合根，`blackboard_view` bin 内落 PG 口径）。

pub mod redact;
pub mod view_auth;

pub use redact::{safe_err, safe_err_with};
