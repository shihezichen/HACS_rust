# 迁移路线图（渐进式 strangler）

口径（已与需求方拍板）：**运行时优先 · 渐进式 · 严格线级兼容**。
每次只换一个进程，Python 与 Rust 在同一边界上并存；任何一步都可回退。

## 0 已完成的骨架

| 阶段 | 内容 | 状态 |
|---|---|---|
| `P0` | Rust 工具链 + `Cargo` workspace + 分层 crate（依赖方向编译期固化） | ✅ |
| `P0` | `L0` 全层（`node_paths` · `node_profile` · `member_roster`） | ✅ |
| `P0` | `L1` 核心（`nats_client` · `web_gateway`）+ 34 例等价测试 | ✅ |

## 1 剩余阶段

### `P1` · L2 适配层（先做，因为它是所有 L3 服务的共同依赖）

| Rust 落点 | Python 原件 | 备注 |
|---|---|---|
| `hacs-l2-adapter::pg_literals` | `hub/l2_adapter/pg_literals.py` | 纯函数，最易，先做 |
| `hacs-l2-adapter::agent_usage` | `node/l2_adapter/agent_usage.py` | 依赖 L0 只读探针（已就位） |
| `hacs-l2-adapter::ctx_watermark` | `node/l2_adapter/ctx_watermark.py` | 薄壳 |
| `hacs-l2-adapter::agent_model` | `hub/l2_adapter/agent_model.py` | 子进程 / ssh / 目标主机档案 |
| `hacs-l2-adapter::cli_backend` | `node/l2_adapter/cli_backend.py` | HTTP 面 + 子进程；落 `hacs-c` 的 bin `cli_backend`（9970） |

### `P2` · L1 其余件

| Rust 落点 | Python 原件 | 端口 |
|---|---|---|
| `hacs-l1-transport::teamd` | `node/l1_transport/teamd.py` | 9960 |
| `hacs-l1-transport::team_cli_core` | `node/l1_transport/team_cli_core.py` | — |
| `hacs-l1-transport::team_call` / `team_broadcast` / `team_roster` | 同名 | — |
| `hacs-l1-transport::group_watch_delivery` | 同名 | — |
| `hacs-l1-transport::post_group` | 同名 | 9984（客户端） |
| `hacs-l1-transport::a2a::{a2a_edge,a2a_call}` | `node/l1_transport/a2a/*` | 9920 |
| `hacs-l1-transport::gen_nats_conf` | `hub/l1_relay/gen_nats_conf.py` | 9930 |

### `P3` · L3 重服务（内存大头，收益最大）

| Rust 落点 | Python 原件 | 端口 | 行数（Python） |
|---|---|---|---|
| `hacs-l3-domain::chat_svc` | `hub/l3_domain/chat_svc.py` | 9980 / 9984 | 3954 |
| `hacs-l3-domain::blackboard_svc` | `hub/l3_domain/blackboard_svc.py` | 9940 | 3957 |
| `hacs-l3-domain::blackboard_view` | `hub/l3_domain/blackboard_view.py` | 9941 / 9942 | 3296 |
| `hacs-l3-domain::task_svc` | `hub/l3_domain/task_svc.py` | 9981 | 871 |

> 这 4 件是收益最大的落点：既是内存大头，也是**对外行为面最宽**的四件 —— 替换前必须先把
> `L2`（模型执行器 / 上下文水位）与 `X`（`view_auth` 会话与密级）落地，否则行为对不齐。

### `P4` · L3 其余件与 `X`

`roundtable` · `rt_remind` · `shared_content` · `embedder` · `backfill_embeddings` ·
`session_ctx_archiver` · `group_watch_pending` · `team_group` · `view_auth`（`X`）。

### `P5` · L4 与 `C` 收口

- `hacs-l4-experience`：`chat_svc` / `blackboard_view` 的静态托管那一半（MIME 钉死 / no-cache /
  SPA fallback / **备份件拦截** `STATIC_BACKUP_RE`，刀34 口径）。
- `hacs-c`：为每个进程入口加一个 `[[bin]]`，**单元名 / 端口 / argv 与 Python 件一字不差**
  （便于 systemd 单元与 Windows 计划任务原地切换）。
- 部署件：`deploy/systemd/*.service` 与 Windows 计划任务的 `ExecStart` 改指 Rust 二进制。

## 2 每个服务的替换流程（固定动作）

1. **落地实现**：在对应层 crate 写模块；契约面（路径 / 信封 / 头 / 主题）逐条对齐 `PARITY.md`。
2. **等价验证**：新增/扩充 `crates/*/tests/`（Rust 侧行为断言）+ 必要时加一个
   **对照装置**（同机双实例：Python 一个端口、Rust 另一个端口，喂同一张请求矩阵，逐例比
   状态码 / 响应头 / 正文）。
3. **本机混跑**：Rust 实例起在**非生产端口**，用真前端 / 真 CLI 打一遍。
4. **单台切换**：只在一台节点上把单元指向 Rust 二进制，观察一轮；行为对不上就回退。
5. **回退**：单元 `ExecStart` 改回 Python 件并重启 —— **数据面不动**（PG / SQLite / NATS 主题不变）。

## 3 验证纪律（沿用 Python 版的红线）

- **不写死真值**：仓内零机器地址、零绝对路径；一切宿主真值走 env（`AGENT_TEAM_*`）。
- **口令零入库**：`.env` / `*.pass` / `*.token` 进 `.gitignore`；只提交 `.example`。
- **只读探针不许变成写**：`L0` 的探针拿到的句柄天生写不进去（`mode=ro`），由测试钉死。
- **不消红**：对不齐的行为如实登记进 `PARITY.md`，不靠改判据变绿。

## 4 待办（本仓尚未建）

- `hacs-parity`：跨语言对照装置的可执行壳（同机双实例 + 请求矩阵 + 逐例 diff）。
  在 `P3` 之前建，作为重服务的过关证据。
- 相方向（`hub/` vs `node/`）的静态检查或在 crate 结构上显式表达。
- 与 Python 版 `tools/hacs_layers.json` 的**件数/端口对账**脚本（防漂移）。
