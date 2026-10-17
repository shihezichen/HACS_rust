# HACS_rust

**HACS（Human-Agent Communication System）的 Rust 重写。** 目标：把 Python 版的长驻服务逐件换成
Rust，**显著降低每进程内存占用**，同时保持**线级兼容**（同端口 / 同路径 / 同 JSON / 同鉴权头 /
同 NATS 主题），从而可以**逐台、逐服务替换并可随时回退**。

- 对应 Python 原件：<https://github.com/shihezichen/HACS>（分层结构 / 端口表 / 契约以它为真源）。
- 分支：`main`。
- **接手先读**：**[`docs/HANDOFF.md`](docs/HANDOFF.md)** —— 交接文档（进度 / 环境与工具链坑 /
  关键决定 / 已知偏差 / 下一步 / 替换流程 / 复跑命令）。

## 三条工程口径

1. **分层结构镜像**，且**编译期固化**：Python 版的「依赖方向」是运行时门禁（`tools/check_layers.py`）；
   在 Rust 里它就是 `Cargo.toml` 的依赖图 —— **一个 crate 只能引用自己在 `Cargo.toml` 里声明过的层**，
   反向依赖根本编译不过。见 [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md)。
2. **线级兼容**：同端口、同 HTTP 路径与状态码、同 JSON 字段与格式、同鉴权头、同 NATS 主题与信封。
   连响应形态也尽量对齐 Python 的 `http.server`（HTTP/1.0 状态行 / `Server:`+`Date:` 头序 /
   `json.dumps` 的 `, ` `: ` 分隔符）—— 见 [`docs/PARITY.md`](docs/PARITY.md)。
3. **渐进式 strangler**：Rust 与 Python 并存，逐服务替换、逐台节点切换，每个服务可对照 Python 版
   做等价验证。见 [`docs/MIGRATION.md`](docs/MIGRATION.md) 与 README 的「当前状态」。

## 目录

| 路径 | 层 | 对应 Python | 状态 |
|---|---|---|---|
| `crates/hacs-x-identity/` | `X` 横切 | `hub/x/view_auth.py` | ✅ 已迁移（含 scrypt/sha256 自实现） |
| `crates/hacs-l0-host/` | `L0` 平台 | `node/l0_host/*` | ✅ 已迁移 |
| `crates/hacs-l1-transport/` | `L1` 通信 | `node/l1_transport/*` · `hub/l1_relay/*` | ✅ 已迁移 |
| `crates/hacs-l2-adapter/` | `L2` 适配 | `node/l2_adapter/*` · `hub/l2_adapter/*` | ✅ 已迁移 |
| `crates/hacs-l3-domain/` | `L3` 协作域 | `hub/l3_domain/*` | ✅ 已迁移 |
| `crates/hacs-l4-experience/` | `L4` 呈现 | `hub/l4_experience/*` | ✅ 已迁移（前端资产内嵌 serve） |
| `crates/hacs-c/` | `C` 组合根 | `node/c/*` · `hub/l4_experience/serve.py` | ✅ 已迁移（全部 [[bin]]） |
| `scripts/dev-env.ps1` | — | — | Windows 开发环境准备 |

依赖方向（编译期事实）：`hacs-c → L4 → L3 → L2 → L1 → L0`，`hacs-x-identity` 可被任意层引用。

### 已迁移的件（L0 / L1）

| Rust 模块 | Python 原件 | 说明 |
|---|---|---|
| `hacs-l0-host::node_paths` | `node_paths.py` | **本机**宿主默认值唯一出处（env 优先 > 平台惯例） |
| `hacs-l0-host::node_profile` | `node_profile.py` | 节点档案：目标主机 / 宿主真源 / **只读探针** / 协作域地址 |
| `hacs-l0-host::member_roster` | `member_roster.py` | 成员名册档案（纯数据 + 纯查询） |
| `hacs-l1-transport::nats_client` | `nats_client.py` | 极简 NATS 文本协议客户端（多地址故障转移） |
| `hacs-l1-transport::web_gateway` | `web_gateway.py` | HTTP 入口件：路由 / JSON 信封 / 前置与鉴权钩子 |
| `hacs-l1-transport::http_client` | （Python 侧是 `urllib.request`） | 极简 HTTP/1.0 客户端（远端水位回退用） |
| `hacs-l2-adapter::pg_literals` | `hub/l2_adapter/pg_literals.py` | pgvector 字面量与时间归一（含 Python 风格 `repr(float)`） |
| `hacs-l2-adapter::agent_usage` | `node/l2_adapter/agent_usage.py` | 上下文水位真源（后端分派 / 分母链 / 远端回退三层护栏） |
| `hacs-l2-adapter::ctx_watermark` | `node/l2_adapter/ctx_watermark.py` | 水位薄壳（对外入口） |
| `hacs-l2-adapter::agent_model` | `hub/l2_adapter/agent_model.py` | 模型执行器适配（ssh 命令行是契约 / 子进程带超时 / 输出解析）+ Agent 展示名 |
| `hacs-x-identity::redact` | `hub/x/view_auth.py::safe_err` | 错误文本脱敏（抹 DSN 口令再截断 400 字符） |

## 跑法

> 本机（Windows）没有 MSVC 构建工具，用 **GNU host 工具链**（MinGW 链接器）。先过一次环境脚本：

```powershell
. .\scripts\dev-env.ps1          # 把 cargo + MinGW 放上 PATH，必要时把 target 挪到 ASCII 路径
cargo build
cargo test
cargo clippy --all-targets
```

Linux 节点上（`lobster` / `top001` / `quantum`）直接 `cargo build --release`，无需上面的脚本。

### 依赖（刻意最小）

| crate | 用途 | 备注 |
|---|---|---|
| `rusqlite`（`bundled`） | L0 只读探针读宿主 Hermes `state.db` | 静态编译 SQLite，不依赖系统库 |
| `serde_json`（`preserve_order`） | JSON 编解码 | `preserve_order` 保 Python dict 的键序 |
| `serde` | 自定义 Serializer 对齐 Python 的 `, ` `: ` 分隔符 | |

**不引 HTTP 框架 / 异步运行时**：L1 的 HTTP 面是手写 socket（对齐 `http.server` 的行为与响应形态），
NATS 客户端是自己实现的文本协议。零框架依赖直接服务于「减少内存占用」这个目标。

## 当前状态

**P0–P5 移植收官（2026-10）**：Python 版全部可移植件已迁到 Rust，262 个等价测试全绿，
偏差逐条登记 [`docs/PARITY.md`](docs/PARITY.md)（57 条）。

- ✅ **L0**（`node_paths` · `node_profile` · `member_roster` · `nodelet`）。
- ✅ **L1**（`nats_client` · `web_gateway` · `http_client` · `teamd` · `team_call/broadcast/roster` ·
  `a2a_edge/a2a_call` 等）。
- ✅ **L2**（`pg_literals` · `agent_usage` · `ctx_watermark` · `agent_model` · `cli_backend`）。
- ✅ **X 横切**（`view_auth` 含自实现 scrypt/sha1/base64）。
- ✅ **L3 全部**：`task_svc` · `chat_svc` · `blackboard_svc` · `blackboard_view` · `roundtable` ·
  `rt_remind` · `shared_content` · `session_ctx_archiver` · `group_watch_pending` · `team_group` ·
  `embedder`（lib-only，骨架+粘性失败）。
- ✅ **L4 + C + tools**：`serve`（前端资产内嵌）· `nodelet` bin · `team` · `post_group` ·
  `gen_nats_conf` · `roster_creds_mirror` · `scan_secrets` 门禁。
- 内存收益（Max RSS，Python vs Rust）：3.4×（nodelet）– 18.5×（group_watch），
  重服务普遍 7×–10×（blackboard_svc 7.3×、chat 链路同量级、roundtable 8.5×）。
- 已知不移植：`check_layers` / `check_layers_reconcile` / `gen_arch_doc`（分层已由 Cargo 依赖图
  编译期固化，运行时门禁地位消失）；`backfill_embeddings`（依赖 fastembed/onnxruntime，按
  PARITY 50 口径不引，embedder 以骨架对齐）。
- 替换流程与逐服务回退方式见 [`docs/MIGRATION.md`](docs/MIGRATION.md) 与
  [`docs/HANDOFF.md`](docs/HANDOFF.md)。
