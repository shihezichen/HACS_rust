//! L0 平台层：**成员名册档案**（对应 Python `node/l0_host/member_roster.py`）。
//!
//! 一句话职责：回答「这个协作域有哪些成员、各自是什么角色、它的口令真值落在哪台机的哪个文件」
//! —— 仅此三样。它是名册的**唯一真源**，不是配置的副本。
//!
//! 条目形态（**契约，冻结**）：每条 = `(member, role, cred_node, cred_kind)`。
//!
//! 三条铁律：
//! - **不存口令**：档案只说「口令真值在哪台机的哪个文件」，一个字节的口令都不进本模块。
//! - **不写路径、不写地址**：`cred_kind` 是**位置的种类**不是路径。
//! - **不存权限串**：角色模板的唯一出处是 L1 `gen_nats_conf.perm_list`。
//!
//! 本模块是**纯数据 + 纯查询**：零 IO、零环境依赖。

/// 档案契约版本（**键名/条目形态冻结**）。
pub const SCHEMA: &str = "l5-roster-archive/1";

/// 本刀唯一的角色名。多角色是将来的事。
pub const ROLE_MEMBER: &str = "member";
pub const ROLES: &[&str] = &[ROLE_MEMBER];

/// 凭据落地的两类位置（**种类不是路径**）。
pub const CRED_RUNTIME: &str = "runtime";
pub const CRED_CONF: &str = "conf";
pub const CRED_KINDS: &[&str] = &[CRED_RUNTIME, CRED_CONF];

/// 凭据可能落在哪些节点（节点名，不是地址）。
pub const CRED_NODES: &[&str] = &["thinkbook", "lobster", "quantum", "top001"];

/// 名册本体 —— **有序**表，顺序 == 中枢 conf 里 users 的规范序（字典序）。
pub type RosterRow = (&'static str, &'static str, &'static str, &'static str);

/// 每条 `(member, role, cred_node, cred_kind)`。
pub const ROSTER: &[RosterRow] = &[
    ("claudecode", ROLE_MEMBER, "thinkbook", CRED_RUNTIME),
    ("codex", ROLE_MEMBER, "thinkbook", CRED_RUNTIME),
    ("hermes", ROLE_MEMBER, "thinkbook", CRED_RUNTIME),
    ("lobster", ROLE_MEMBER, "lobster", CRED_CONF),
    ("openclaw-nb", ROLE_MEMBER, "thinkbook", CRED_RUNTIME),
    ("piagent", ROLE_MEMBER, "thinkbook", CRED_RUNTIME),
    ("quantum", ROLE_MEMBER, "quantum", CRED_CONF),
    ("top001", ROLE_MEMBER, "top001", CRED_CONF),
];

/// 全体成员名，**按档案顺序**（= 规范序）。调用方拿它去比 conf 时不许再排一次。
pub fn members() -> Vec<&'static str> {
    ROSTER.iter().map(|r| r.0).collect()
}

/// [`members`] 的别名（与对账器里 `roster` 那个词对齐）。
pub fn member_names() -> Vec<&'static str> {
    members()
}

/// 成员的完整条目；不在名册返回 `None`（**不猜**）。
pub fn entry(name: &str) -> Option<RosterRow> {
    ROSTER.iter().copied().find(|r| r.0 == name)
}

/// 成员的角色名；不在名册返回 `None`。
pub fn role_of(name: &str) -> Option<&'static str> {
    entry(name).map(|r| r.1)
}

/// 成员的凭据**位置说明** `(cred_node, cred_kind)`；不在名册返回 `None`。
///
/// 返回的是「哪台机 + 哪一类位置」，**不是路径** —— 拼绝对路径要拿 `cred_kind` 去问
/// L0 [`crate::node_profile`] 与部署表。
pub fn cred_spec(name: &str) -> Option<(&'static str, &'static str)> {
    entry(name).map(|r| (r.2, r.3))
}
