# HACS_rust · 交接文档（handoff）

> 本文是**工作交接件**：接手的人（或下一个 Agent）照着它就能把活接着干下去。
> 真值以仓内代码与本机实测为准；本文若与之冲突，以代码为准并回来改本文。

- 交接时点 HEAD：`f54eb59`
- 分支：`main`
- 远端：`https://github.com/shihezichen/HACS_rust`（private）
- 对应 Python 原件：<https://github.com/shihezichen/HACS>（分层结构 / 端口表 / 契约以它为真源）

---

## 1 目标与口径（已与需求方拍板，勿擅自改）

| 项 | 口径 |
|---|---|
| 范围 | **运行时优先**：先换长驻服务（内存大头），工具面 / 部署件 / 前端静态件后续按需 |
| 策略 | **渐进式 strangler**：Rust 与 Python 并存，逐服务替换、逐台节点切换，**随时可回退** |
| 兼容 | **严格线级兼容**：同端口 / 同路径 / 同 JSON / 同鉴权头 / 同 NATS 主题与信封 → 可混跑 |
| 收益目标 | **减少每进程内存占用**（去掉 Python 解释器与 GC 堆；零 HTTP 框架、零异步运行时） |
| 前端 | **保持原样**（HTML/CSS/JS 不在重写范围）；Rust 侧只接「谁在写 socket」那一半 |

---

## 2 当前进度

```
P0  骨架                 ✅ 完成
P1  L2 适配层            ✅ 完成（cli_backend 已落地，含 hacs-c 第一个 [[bin]]）
P2  L1 其余件            ✅ 完成（teamd / a2a_edge(a2a_svc) / team_roster / team_call / team_broadcast / a2a_call 全落地）
P3  L3 重服务（收益最大）  ✅ task_svc + chat_svc + blackboard_svc + blackboard_view 全落地
P4  L3 其余件 + X 其余面  🟡 进行中：team_group ✅ / rt_remind ✅ / session_ctx_archiver ✅ / shared_content ✅ / group_watch_pending ✅ / embedder ✅（lib-only，见 PARITY 50）；余 roundtable、node/c 其余件、l4_experience/serve.py、gen_nats_conf.py、tools
P5  L4 + C 收口 + 部署件   ⏳ 未开始
```

### 已完成（3 个提交）

| 提交 | 内容 |
|---|---|
| `6127614` | 建仓：分层 workspace + **L0 全层** + L1 核心（`nats_client` · `web_gateway`） |
| `534bb3a` | L2 核心：`pg_literals` · `agent_usage` · `ctx_watermark` + L1 `http_client` |
| `f54eb59` | `agent_model` + **X 首次落地**（`redact::safe_err`） |

### 已迁移件清单（模块 ↔ Python 原件）

| Rust 落点 | Python 原件 | 说明 |
|---|---|---|
| `hacs-l0-host::node_paths` | `node/l0_host/node_paths.py` | 本机宿主默认值唯一出处（env 优先 > 平台惯例，`flavour` 可注入） |
| `hacs-l0-host::node_profile` | `node/l0_host/node_profile.py` | 节点档案：目标主机 / 宿主真源 / **只读探针** / 协作域地址 / 落盘默认位置 |
| `hacs-l0-host::member_roster` | `node/l0_host/member_roster.py` | 成员名册档案（纯数据 + 纯查询） |
| `hacs-l1-transport::nats_client` | `node/l1_transport/nats_client.py` | NATS 文本协议客户端（多地址故障转移；3 个已踩坑全部保留） |
| `hacs-l1-transport::web_gateway` | `node/l1_transport/web_gateway.py` | 手写 HTTP 入口件（对齐 `http.server` 响应形态） |
| `hacs-l1-transport::http_client` | （Python 用 `urllib.request`） | 极简 HTTP/1.0 客户端（远端水位回退用） |
| `hacs-l2-adapter::pg_literals` | `hub/l2_adapter/pg_literals.py` | pgvector 字面量 + 时间归一（含 Python 风格 `repr(float)`） |
| `hacs-l2-adapter::agent_usage` | `node/l2_adapter/agent_usage.py` | 上下文水位真源（后端分派 / 分母链 / 远端回退三层护栏） |
| `hacs-l2-adapter::ctx_watermark` | `node/l2_adapter/ctx_watermark.py` | 水位薄壳 |
| `hacs-l2-adapter::agent_model` | `hub/l2_adapter/agent_model.py` | 模型执行器适配（ssh 命令行 / 带超时子进程 / 输出解析）+ 展示名 |
| `hacs-x-identity::redact` | `hub/x/view_auth.py::safe_err` | 错误文本脱敏 |
| `hacs-l2-adapter::cli_backend` | `node/l2_adapter/cli_backend.py` | CLI 成员后端纯逻辑（文本清洗 / prompt 拼接 / 模型解析 / uuid5 会话映射 / codex 会话落盘） |
| `hacs-c::bin/cli_backend` | `node/l2_adapter/cli_backend.py` | **第一个 `[[bin]]`**：HTTP 装配（9970）+ 三成员 runner + 超时杀树 + busy 门闸 |
| `hacs-l1-transport::teamd` | `node/l1_transport/teamd.py` | teamd 纯逻辑（凭据解析 / timeout 夹取 / 错误信封 / 有界队列+每成员闸门 / 计数器 / 单行日志） |
| `hacs-c::bin/teamd` | `node/l1_transport/teamd.py` | **第二个 `[[bin]]`**：每成员独立 NATS 连接（心跳/退避重连）+ worker 池回调本机 A2A + 9960 管理口（/healthz /varz） |
| `hacs-l1-transport::team_tools` | `team_roster/team_call/team_broadcast.py` | 三工具共享纯逻辑（心跳窗口 / 点名载荷 / 汇总行 / a2a 人类可读出口） |
| `hacs-c::bin/team_roster` | `node/l1_transport/team_roster.py` | 订阅 `team.hb.>` 收心跳出成员表（exit 0/3/4） |
| `hacs-c::bin/team_call` | `node/l1_transport/team_call.py` | NATS request/reply 点名（--dry-run/--json/--text-file，exit 0/3/4） |
| `hacs-c::bin/team_broadcast` | `node/l1_transport/team_broadcast.py` | 并行扇出群发（限并发信号量、--all 扫心跳取名单、--no-wait） |
| `hacs-c::bin/a2a_call` | `node/l1_transport/a2a/a2a_call.py` | A2A HTTP 客户端（healthz/agents/card/call；A2A_TOKEN env 优先；公共参数任意位置） |
| `hacs-l1-transport::a2a_edge` | `node/l1_transport/a2a/a2a_edge.py` | A2A 纯逻辑（SHA-256 鉴权反查 / 配置加载 / agent card / 终标记 / task_loop 开关 / OpenAI 形状） |
| `hacs-c::bin/a2a_svc` | `node/l1_transport/a2a/a2a_edge.py`（+组合根 a2a_svc.py 现形态） | **A2A 边缘服务**（9920，HTTP/1.1 + `Server: a2a_svc/2.2`）：healthz/agents/agent-card/messages + B2 续跑循环四出口；能力端口（pressure 等）暂走降级口径 |
| `hacs-l3-domain::task_svc` | `hub/l3_domain/task_svc.py` | Store 全量（create / next_for 队首 / report 按 task_id 幂等 / ack+lease / chain 冻结 / reply 派发**禁止代写** / 回帖台账）；CST ISO 时间口径自实现（civil 历法，逐字节对齐 `isoformat+08:00`）；est_tokens / clip_digest |
| `hacs-l3-domain::task_svc_chat` | `task_svc.py::chat_post_message` | 回帖出站（POST /api/sessions/{sid}/messages，`X-Agent-Id` + `@all`） |
| `hacs-c::bin/task_svc` | `hub/l3_domain/task_svc.py`（Handler+main） | **L3 第一个 `[[bin]]`**（9981）：全路由（health/tasks/next/ack/lease/chains/report/PATCH/DELETE）+ 回帖重试线程(30s) + lease 巡检线程(10s)；`CHAT_BASE` env 口径 |

### 验证口径（当前实测）

| 命令 | 期望 |
|---|---|
| `cargo test` | **119 例全绿**（L0 23 · L1 45 · L2 34 · L3 12 · X 6） |
| `cargo clippy --all-targets` | **0 警告** |
| `cargo build --release` | 产出 `cli_backend`、`teamd`、`team_roster`、`team_call`、`team_broadcast`、`a2a_call`、`a2a_svc` |

> `cli_backend` 已在 Linux 上做过端到端验证（假 CLI 注入，对齐 Python
> `tests/test_cli_backend.py` 的请求矩阵：401/404/502/503/504、ANSI 清洗、
> 多轮上下文、超时杀树无残留、同成员串行跨成员并行、日志不记 key）。
> uuid5 与 Python `uuid.uuid5(NAMESPACE_URL, ...)` 逐字节一致（有交叉验证钉死）。
> `teamd` 做过 e2e 冒烟（凭据缺失 FATAL exit 2 / 多成员裸值拒绝 / 管理口路由）；
> NATS 端到端（真连 nats-server 的收发/心跳）尚待 parity 装置补验。

测试文件：`crates/*/tests/*.rs`（`parity.rs` / `node_profile.rs` / `agent_model.rs`）。
判据是「与 Python 同口径」：env 优先顺序、平台默认值形状、名册规范序、**只读探针写不进去**、
NATS 握手与 request-reply、网关响应**线上字节形态**、Python 风格浮点 `repr`、四舍六入五成双、
ISO 时间归一、ssh 命令行**选项顺序**、脱敏正则口径。

---

## 3 环境与工具链（重要，换机器先读这节）

本机（Windows）**没有 MSVC 构建工具**，因此用 **GNU host 工具链 + MinGW 链接器**：

```
rustc/cargo 1.99.1 (stable-x86_64-pc-windows-gnu)
MinGW-W64 gcc 16.2.0（WinLibs / winget: BrechtSanders.WinLibs.POSIX.UCRT）
rusqlite 0.40.2 (bundled，静态编译 SQLite)  ·  serde_json 1.0.154 (preserve_order)  ·  serde 1.0.229
```

**每次开新 shell 先跑一次环境脚本**（它做三件事：把 cargo 与 MinGW 放上 PATH；本机无 MSVC 时
选 GNU 工具链；**非 ASCII 检出路径时把 target 挪到 ASCII 位置**）：

```powershell
. .\scripts\dev-env.ps1
cargo build ; cargo test ; cargo clippy --all-targets
```

### 三个必须知道的坑（都已在脚本里处理）

1. **`rust-toolchain.toml` 的 `channel = "stable"` 会解析成 rustup 的 default host = msvc** ——
   本机链接不上。脚本用 `RUSTUP_TOOLCHAIN` 覆盖（仓库文件保持跨平台可移植，不写死 gnu）。
2. **MinGW `ld.exe` 解析不了含非 ASCII 字符的路径** —— 本仓在 `D:\Work\HACS架构优化\HACS_rust`，
   直接构建会在链接期报一堆 `cannot find ...\deps\*.o`。脚本检测到非 ASCII 就把
   `CARGO_TARGET_DIR` 指到 `%LOCALAPPDATA%\hacs-rust-target`。
3. **PowerShell 5.1 按 ANSI 读 UTF-8 脚本**，脚本里写中文注释可能吞掉后续行 ——
   `scripts/dev-env.ps1` 保持**纯 ASCII**，别给它加中文注释。

Linux 节点（`lobster` / `top001` / `quantum`）上直接 `cargo build --release`，不需要上面脚本。

---

## 4 关键实现决定（改代码前先读，别推翻）

| 决定 | 理由 |
|---|---|
| **分层做成 crate，依赖方向编译期固化** | Python 版靠 `tools/check_layers.py` 事后抓反向依赖；Rust 里一个 crate 只能引用自己 `Cargo.toml` 里声明过的层，**反向依赖根本编译不过**。`hacs-c` 是唯一声明全部层的 crate |
| **手写 HTTP（不引 axum/hyper）** | 既有各件的响应形态是 `http.server` 的形状（HTTP/1.0 状态行 / `Server:`+`Date:` 头序 / 未注册方法 501 而非 405）；框架会改掉这些。手写同时省掉框架的内存 |
| **`serde_json` + 自定义 Formatter** | `json.dumps(..., ensure_ascii=False)` 用 `, ` `: ` 分隔符；`preserve_order` 保 Python dict 键序 |
| **不用异步运行时** | 既有各件是「线程 + 阻塞 I/O」（`ThreadingHTTPServer`），Rust 侧同构即可 |
| **自写带超时子进程** | `std::process` 没有 `wait` 超时；实现要点：stdin 并发写、stdout/stderr **并发读**（否则管道缓冲写满会死等）、到点 `kill` 再 `wait` |
| **写死真值一律走 env** | 仓内零机器地址、零绝对路径字面量；`WIN_NODE_EXE` 等沿用 Python 源码仓的脱敏占位符，真值由 `AGENT_TEAM_*` env 注入 |

更细的分层↔crate 映射与理由见 [`ARCHITECTURE.md`](ARCHITECTURE.md)。

---

## 5 已知偏差（**不消红**，如实登记在 [`PARITY.md`](PARITY.md) §5）

接手时先接受这些，**不要靠改判据把它们变绿**：

1. `Server:` 头的 `sys_version`（Python 会拼 `Python/3.x`，Rust 默认空 —— 客户端不解析）
2. JSON 浮点格式在极边界值上与 Python `repr` 可能不同
3. 主机名获取方式（env + `hostname` 命令 vs `socket.gethostname()`）
4. `etc_dirs` 的 nt 默认值、脱敏占位符（与 Python 一致，非新问题）
5. **`window_for` 第 ② 级（Hermes 模型目录）Rust 侧跳过** —— 要 `import` Python 包
6. `repr(float)` 的「最短表示撞车」边缘
7. `agent_usage` 的 debug 日志改由 `HACS_DEBUG` 开关
8. **模块级 env 捕获 → 惰性求值**（Python import 期定死；Rust 调用时读）
9. `elapsed_s` 的 `round(x, 2)`（纯展示字段）
10. **`X` 只落了 `safe_err`** —— 会话令牌 / 角色密级 / 时间口径（`ts`/`parse_ts`/`beijing`）未迁

---

## 6 下一步（按顺序做）

### 6.1 ~~立刻可做：`cli_backend`（L2 最后一件，9970）~~ ✅ 已完成

- 纯逻辑落 `hacs-l2-adapter::cli_backend`（uuid5 自实现零依赖、ANSI 清洗手写扫描器、
  codex 会话映射原子落盘）；HTTP/进程装配落 `hacs-c::bin/cli_backend`。
- 偏差登记见 `PARITY.md` §5（新增 13~15 条）。

### 6.2 然后 `P2`：L1 其余件

`teamd`(9960) · `team_cli_core` · `team_call` · `team_broadcast` · `team_roster` ·
`group_watch_delivery` · `post_group` · `a2a/{a2a_edge,a2a_call}`(9920) · `gen_nats_conf`。

### 6.3 然后 `P3`：L3 重服务（**内存收益主战场**）

| 件 | 端口 | Python 行数 |
|---|---|---|
| `chat_svc` | 9980 / 9984 | 3954 | ✅ 已完成（L3 `chat_svc` 模块 + `hacs-c::bin/chat_svc` 9980） |
| `blackboard_svc` | 9940 | 4558（v1.9） |
| `blackboard_view` | 9941 / 9942 | 3296 |
| `task_svc` | 9981 | 871 | ✅ 已完成 |

> ✅ `blackboard_view` 已落地：X 层 `hacs-x-identity::view_auth`（会话令牌 / 角色密级 / scrypt 口令散列 / 时间口径）先行补齐（15 单测），L3 + bin（9941）随后（E2E 全链路通过，内存对比 Python 32,240 kB → Rust 3,504 kB，~9.2×）
> （见已知偏差 10）。`chat_svc` 依赖 `agent_model`（已就绪）与 `view_auth`（未就绪）。

### 6.4 建议先建的公共设施：`hacs-parity` 对照装置

即 [`MIGRATION.md`](MIGRATION.md) §4 的待办：同机双实例（Python 一个端口、Rust 另一个端口）
喂同一张请求矩阵，逐例比状态码 / 响应头 / 正文。**在 P3 之前建**，作为重服务的过关证据。

---

## 7 每个服务的替换流程（固定动作，别自创）

1. **实现**：在对应层 crate 写模块；契约面逐条对齐 `PARITY.md`。
2. **等价验证**：扩 `crates/*/tests/`；必要时加同机双实例对照装置。
3. **本机混跑**：Rust 实例起在**非生产端口**，用真前端 / 真 CLI 打一遍。
4. **单台切换**：只在一台节点把单元 `ExecStart` 指向 Rust 二进制，观察一轮。
5. **回退**：`ExecStart` 改回 Python 件并重启 —— **数据面不动**（PG / SQLite / NATS 主题不变）。

---

## 8 纪律（硬规矩，与 HACS 同一套）

1. **私钥密钥零入库**：`.env` / `*.pass` / `*.token` 一律进 `.gitignore`，只提交 `.example`。
2. **仓内零真值**：不写机器地址 / 绝对路径；一切宿主真值走 `AGENT_TEAM_*` env。
3. **只读探针不许变成写**：`L0` 探针拿到的句柄天生写不进去（`mode=ro`），已有测试钉死。
4. **不消红**：对不齐的行为登记进 `PARITY.md`，不靠改判据变绿。
5. **一次一提交**：每批落地即 `cargo test` + `cargo clippy --all-targets` 全绿再提交。

---

## 9 复跑命令清单

| 命令 | 期望 |
|---|---|
| `. .\scripts\dev-env.ps1` | 打印 `dev-env ready: rustc 1.99.1 ...` |
| `cargo test` | 全绿（当前 68 例） |
| `cargo clippy --all-targets` | 0 警告 |
| `cargo build --release` | 产出 `hacs-c` 二进制（当前仅有 lib，`[[bin]]` 随 P3/P5 加入） |
| `git log --oneline` | 3 个提交（交接时点） |

---

## 10 未决 / 风险（如实记账）

1. ~~**`hacs-c` 还没有 `[[bin]]`**~~ ✅ 已随 `cli_backend` 落地。
2. **相（`hub/` vs `node/`）没有在 Rust 侧表达** —— Python 的「检查 6 相方向」是硬门禁，
   Rust 目前只镜像了**层**；需要时再加一刀（见 `ARCHITECTURE.md` §2 的代价段）。
3. **未与 Python 版做过端到端对照** —— 现有 68 例是**单元/组件级**等价；跨语言 e2e 对照
   装置尚未建（§6.4）。
4. **`hacs_layers.json` 的件数/端口对账**未做 —— 防漂移脚本待建。
5. **推送通道**：本机 **SSH 出网被墙**（22/443 均超时），GitHub 走 **HTTPS + Git Credential
   Manager**（已可用）。`~/.ssh/id_ed25519_github` 是 HACS 的推送专用密钥，网络恢复后可用。
