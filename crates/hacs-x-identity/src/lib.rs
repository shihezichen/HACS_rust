//! # hacs-x-identity —— HACS 横切层（X）：身份与授权 / 错误脱敏
//!
//! 对应 Python `hub/x/view_auth.py`：账号 / 会话 / 角色 / 密级 + `safe_err` 错误脱敏。
//!
//! **横切层不参与依赖方向判定** —— 可被任意层引用（见 `docs/ARCHITECTURE.md`）。
//!
//! | 模块 | 对应 Python | 职责 |
//! |---|---|---|
//! | [`redact`] | `view_auth.safe_err` | 错误文本脱敏（抹 DSN 口令再截断） |
//!
//! ## 迁移状态
//!
//! 🟡 部分迁移：错误脱敏已落地（`agent_model` 需要它）。
//!
//! ⏳ 待迁移：会话令牌签发/校验 · 角色与密级判定 · 时间口径
//! （`ts` / `parse_ts` / `beijing`，被 `blackboard_view` 使用）· SQLite 存储访问
//! （落点遵循「存储句柄归 L0」的口径）。

pub mod redact;

pub use redact::{safe_err, safe_err_with};
