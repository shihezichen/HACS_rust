//! # hacs-x-identity —— HACS 横切层（X）：身份与授权
//!
//! 对应 Python `hub/x/view_auth.py`：账号 / 会话 / 角色 / 密级。
//!
//! **横切层不参与依赖方向判定** —— 可被任意层引用（见 `docs/ARCHITECTURE.md`）。
//!
//! ## 迁移状态
//!
//! ⏳ 待迁移。Python 原件的可移植面（会话令牌签发/校验、角色与密级判定、
//! SQLite 存储访问）将在本 crate 落地；落点遵循「存储句柄归 L0」的口径。

/// 迁移占位：本 crate 尚未实现（见 README 的迁移路线图）。
pub const MIGRATION_STATUS: &str = "pending";
