//! # hacs-l4-experience —— HACS 呈现层（Experience）
//!
//! 一句话职责：**人在浏览器里看到什么** —— 静态壳同源托管 · 反代 · 鉴权跳转 · SSE 聚合。
//!
//! 对应 Python `hub/l4_experience/`：前端三件套（`group.html` / `group.css` /
//! `group_view.js` / `group_transport.js`）+ 开发用静态服务器 `serve.py`。
//!
//! ## 迁移口径
//!
//! 前端**静态件保持原样**（HTML/CSS/JS 不在 Rust 重写范围内）—— 本 crate 承接的是
//! Python 侧「谁在写 socket」那一半：同源托管 / MIME 钉死 / no-cache / SPA fallback /
//! 备份件拦截（对应 `chat_svc` 的 `_serve_static` 与刀34 的 `STATIC_BACKUP_RE`）。

/// 迁移占位：本 crate 尚未实现（见 README 的迁移路线图）。
pub const MIGRATION_STATUS: &str = "pending";
