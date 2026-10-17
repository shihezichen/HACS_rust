# HACS_rust

**HACS（Human-Agent Communication System）的 Rust 实现。**
HACS 是一个多 Agent 协作系统：成员（人 + Agent）通过群聊、任务、会议（圆桌）、共享黑板协作，
长驻服务负责消息投递、协作域逻辑与 Web 呈现。本仓库是它的 Rust 侧实现——与
[Python 原版](https://github.com/shihezichen/HACS) **线级兼容**（同端口 / 同 HTTP 路径与状态码 /
同 JSON 信封 / 同鉴权头 / 同 NATS 主题），可作为原版的逐进程替换品，单进程内存降至原版的
**1/3 ～ 1/18**。

## 为什么是 Rust

HACS 由十几个常驻服务组成，Python 版单进程内存 20–40 MB；服务数量 × 节点数量一乘，常驻内存成为
部署的主要成本。Rust 版以「**线级兼容、逐服务替换、随时回退**」为约束重写全部服务，
实测内存收益（Max RSS，Python vs Rust）：

| 服务 | 收益 | | 服务 | 收益 |
|---|---|---|---|---|
| group_watch | **18.5×** | | roundtable | 8.5× |
| shared_content | 9.7× | | gen_nats_conf | 9.3× |
| serve | 10.0× | | post_group | 8.9× |
| blackboard_view | 9.2× | | team | 9.9× |
| blackboard_svc | 7.3× | | nodelet | 3.4× |

收益的直接来源是下面的技术选型：无框架、无异步运行时、零外部密码学/HTTP 依赖。

## 架构

### 分层模型

系统分六层，外加一个横切层；Rust 侧每层是一个 crate：

```
C 组合根 hacs-c          进程入口装配：每个常驻服务/CLI 是一个 [[bin]]，只做装配与注入
 ├── L4 呈现 hacs-l4-experience      静态壳同源托管 / 反向代理 / 鉴权跳转
 ├── L3 协作域 hacs-l3-domain        群聊 / 任务 / 圆桌会议 / 黑板 / 共享内容（业务语义层）
 ├── L2 适配 hacs-l2-adapter         Agent 差异唯一落脚点（模型执行器 / 上下文水位 / pg 字面量）
 ├── L1 通信 hacs-l1-transport       投信封 / 保投到 / 不重复 / 成员入网控制面（无业务语义）
 └── L0 平台 hacs-l0-host            宿主事实唯一出处（路径 / 节点档案 / 名册 / 只读探针）
 X 横切  hacs-x-identity            身份与授权（scrypt 会话）/ 错误脱敏；可被任意层引用
```

**依赖方向是编译期事实**：各层 crate 的 `Cargo.toml` 只声明自己允许依赖的层，
「L3 反向引用 C」这类违规根本编译不过——Python 版需要运行时门禁脚本做的事，Rust 版交给编译器。
`hacs-c` 是唯一声明了全部层的 crate。

### L0 铁律（贯穿全系统的纪律）

- **只读**：宿主探针一律以 SQLite `mode=ro` 打开、用完即关；不建目录、不写状态。
- **不编造**：没配置 / 文件不在 / 记录不在 → `None` 或空；真出错原样抛，绝不把「打不开」变成「没有数据」。
- **传输不含语义**：L1 的网关不出现业务词，NATS 客户端不认识群聊/会议；语义由调用方注入。
- **不泄密**：凭据只走 stdin / 文件，日志与探针输出中只允许 `{"present":bool,"len":int}` 形态，出口机械复核。

## 技术选型（刻意最小依赖）

核心第三方依赖只有三个：

| crate | 用途 |
|---|---|
| `serde_json`（`preserve_order`） | JSON 编解码；自定义 Formatter 逐字节对齐 Python `json.dumps` 的 `, ` `: ` 分隔符与键序 |
| `rusqlite`（`bundled`） | 静态编译 SQLite：L0 只读探针读宿主 state.db |
| `postgres` 0.19 | 同步 PostgreSQL 客户端（chat/blackboard/task 等服务的存储） |

**明确不引的，以及替代实现**（全部自写、逐条对齐 Python 行为、有等价测试钉死）：

| 常规做法 | 本项目 | 理由 |
|---|---|---|
| axum / hyper / tokio | 手写 HTTP/1.0 socket（线程 + 阻塞 I/O） | 对齐 Python `http.server` 的状态行/头序/信封形状；省掉框架与运行时内存 |
| reqwest | 手写 HTTP 客户端（`http_client.rs`） | 同上 |
| async-nats | 手写 NATS 文本协议客户端（多地址故障转移） | 协议极简，客户端 <500 行 |
| regex | 每条 Python 正则手写对应扫描器 | 见 `tools/scan_secrets.rs` 等 |
| sha2 / hmac / scrypt crate | 自实现 sha256 / sha1 / hmac / scrypt / base64（md5 以 sha256[:8] 替代，仅自观测） | 逐字节与 hashlib 对齐（含 scrypt 双端序修复） |
| uuid crate | 手写 uuid5（SHA1 命名空间版） | 与 Python `uuid.uuid5` 逐字节一致 |
| chrono | 自实现 civil 历法 + 时区归一（支持 `+08:00`） | 只用到日期运算与 ISO8601 格式化 |
| fastembed / onnxruntime | embedder 骨架 + 粘性失败（PARITY 50） | 不为可选功能引入 ONNX 运行时 |

这一整套「零框架、零运行时、自实现密码学」直接服务于内存目标，也把攻击面压到最小。

## 仓库结构

```
crates/
  hacs-x-identity/      X 横切：view_auth（scrypt 会话/密级）、错误脱敏、sha256/hmac/scrypt
  hacs-l0-host/         L0：node_paths · node_profile · member_roster · nodelet（节点守护纯逻辑）
  hacs-l1-transport/    L1：nats_client · web_gateway · http_client · teamd · team_call/broadcast/roster
                        · team_group · a2a_edge/a2a_call · group_watch_pending
  hacs-l2-adapter/      L2：pg_literals · agent_usage · ctx_watermark · agent_model · cli_backend
  hacs-l3-domain/       L3：task_svc · chat_svc · blackboard_svc · blackboard_view · roundtable
                        · rt_remind · shared_content · session_ctx_archiver · embedder
  hacs-l4-experience/   L4：静态托管 / 反代
  hacs-c/               C 组合根：23 个 [[bin]]（每个服务/CLI 一个进程入口）+ 前端资产内嵌
tools/scan_secrets.rs   提交前密钥扫描硬门禁（独立 rustc 单文件）
docs/                   HANDOFF（交接/复跑）· ARCHITECTURE · MIGRATION · PARITY（57 条偏差登记）
```

### 常驻服务端口表

| bin | 端口 | 职责 |
|---|---|---|
| `chat_svc` | 9980 | 群聊服务（PostgreSQL 存储，错误信封 `{"ok":false,"error":{"code","message"}}`） |
| `blackboard_svc` | 9940 | 共享黑板（PostgreSQL，平信封契约，`--peers-json` 认证） |
| `blackboard_view` | 9941 | 黑板 Web 视图（HTML 资产 `include_str!` 内嵌） |
| `task_svc` | 9981 | 任务服务 |
| `a2a_svc` | 9920 | Agent-to-Agent 边缘 |
| `teamd` | 9960 | 成员入网控制面（NATS `a2a.v1.req.<member>`） |
| `nodelet` | 9971 | 节点守护探针（只读，键序冻结输出） |
| `serve` | 自定 | L4 前端静态 + `/api/*` 反代 + `/login` |
| `cli_backend` | 9970 | CLI 后端 |

其余 CLI（`team` / `post_group` / `roundtable` / `rt_remind` / `shared_content` / `session_ctx_archiver` /
`group_watch` / `gen_nats_conf` / `roster_creds_mirror` / `a2a_call` / `team_call` / `team_broadcast` /
`team_roster`）为一次性运维/客户端命令。

## 构建与测试

Linux（推荐）：

```bash
cargo build --release
cargo test          # 262 个等价测试全绿
```

Windows 用 GNU host 工具链（本机无 MSVC）：先 `. .\scripts\dev-env.ps1`，再同上。

等价测试覆盖：网关线上字节形态、NATS 完整握手、uuid5 逐字节一致、Python 风格浮点 `repr`、
四舍六入五成双、scrypt 与 hashlib 逐字节一致、探针只读性（写不进去断言）、脱敏正则口径等。

## 与 Python 版的关系

- **真源**：分层结构 / 端口表 / 契约以 [Python 版](https://github.com/shihezichen/HACS) 为准。
- **PARITY**：所有已知偏差逐条登记在 [`docs/PARITY.md`](docs/PARITY.md)（57 条），不消红、如实记账；
  例如 md5 未实现（nodelet 指纹改 sha256[:8]，仅自观测不跨实现比对）、embedder 骨架化。
- **部署使用**：从 [Releases](https://github.com/shihezichen/HACS_rust/releases) 下载对应平台包，
  中枢机 / 成员机装什么、如何配置组网——见 **[`docs/USER_GUIDE.md`](docs/USER_GUIDE.md)**
  （全部步骤实机验证过）。
- **替换与回退**：逐服务把 systemd 单元 / 计划任务的 `ExecStart` 指向 Rust 二进制即可切换；
  行为对不上改回 Python 件即回退。流程见 [`docs/MIGRATION.md`](docs/MIGRATION.md)。
- **不移植件**：`check_layers` / `check_layers_reconcile` / `gen_arch_doc`（分层已由 Cargo 依赖图
  编译期固化，运行时门禁地位消失）；`backfill_embeddings`（依赖 ONNX，见 PARITY 50）。

## 文档

- [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) —— 分层 ↔ crate 映射、依赖方向、关键实现决定
- [`docs/PARITY.md`](docs/PARITY.md) —— 57 条与 Python 版的等价性记录与偏差登记
- [`docs/USER_GUIDE.md`](docs/USER_GUIDE.md) —— **使用手册**：部署、组网配置、故障速查
- [`docs/MIGRATION.md`](docs/MIGRATION.md) —— 逐服务替换流程与回退
- [`docs/HANDOFF.md`](docs/HANDOFF.md) —— 交接文档：环境坑、关键决定、复跑命令
