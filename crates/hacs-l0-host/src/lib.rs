//! # hacs-l0-host —— HACS 平台层（Host）
//!
//! 一句话职责：**宿主事实唯一出处** —— 这台机器长什么样、东西放在哪、凭据落在谁手里。
//! 上层一律不自己猜宿主事实。
//!
//! 对应 Python `node/l0_host/`。
//!
//! | 模块 | 对应 Python | 职责 |
//! |---|---|---|
//! | [`node_paths`] | `node_paths.py` | **本机**宿主默认值唯一出处（env 优先 > 平台惯例） |
//! | [`node_profile`] | `node_profile.py` | 节点档案：目标主机 / 宿主真源 / **只读探针** / 协作域地址 |
//! | [`member_roster`] | `member_roster.py` | 成员名册档案（纯数据 + 纯查询） |
//!
//! 两条铁律（贯穿全层）：**只读**（不建目录、不写状态）与**不编造**（拿不到就 `None`，
//! 真出错原样抛，绝不把「打不开」偷偷变成「没有数据」）。

pub mod member_roster;
pub mod node_paths;
pub mod node_profile;
pub mod nodelet;
