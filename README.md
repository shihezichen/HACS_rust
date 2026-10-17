# HACS_rust

**HACS（Human-Agent Communication System）—— 多 Agent 协作系统的 Rust 实现。**

HACS 让一组成员（人 + AI Agent）通过**群聊、任务、圆桌会议、共享黑板**协作。
本仓库是完整可独立部署的 Rust 实现：十几个常驻服务 + 成员 CLI，单进程内存 **3–15 MB**。

## 系统是什么

```
        ┌──────────── 中枢机（hub）────────────┐
        │  nats-server      消息路由（9930）     │
        │  teamd            成员入网控制面(9960)  │
        │  a2a_svc          Agent 派活边缘(9920) │
        │  cli_backend      派活到 CLI(9970)    │
        │  chat_svc         群聊(9980)          │
        │  blackboard_svc   共享黑板(9940)       │
        └──────────────┬───────────────────────┘
                       │ NATS（跨机走 IP/tailnet）
     ┌─────────────────┼──────────────────┐
  成员机 A           成员机 B            成员机 C
  team CLI           team CLI            team CLI
  （每台一份自己的凭据）
```

- **成员**（人或 Agent）在成员机上用 `team` CLI 说话：`team say`（点名）、`broadcast`（群发）、
  `meet`（圆桌会议）、`group`（群聊）。
- **中枢**负责投递消息、执行协作域逻辑、把活派给 Agent（`a2a_svc → cli_backend → 真实 CLI`）。
- `nats-server` 是第三方开源消息路由器（基础设施，如同 PostgreSQL），不是本项目代码。

## 架构

### 分层模型

六个业务层 + 一个横切层，Rust 侧每层是一个 crate：

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
「L3 反向引用 C」这类违规根本编译不过。`hacs-c` 是唯一声明了全部层的 crate。

### 全系统纪律

- **只读**：宿主探针以 SQLite `mode=ro` 打开、用完即关；不建目录、不写状态。
- **不编造**：没配置 / 文件不在 → `None` 或空；真出错原样抛，绝不把「打不开」变成「没有数据」。
- **传输不含语义**：L1 网关不出现业务词，NATS 客户端不认识群聊/会议；语义由调用方注入。
- **不泄密**：凭据只走 stdin / 文件；日志与探针输出只允许 `{"present":bool,"len":int}` 形态，出口机械复核。

## 技术实现（刻意最小依赖）

核心第三方依赖只有三个：`serde_json`（自定义 Formatter 控制键序与分隔符）、
`rusqlite`（静态编译 SQLite）、`postgres`（同步 PG 客户端）。

其余全部自写，每项有等价测试钉死：

| 常规做法 | 本项目 | 理由 |
|---|---|---|
| axum / hyper / tokio | 手写 HTTP/1.0 socket（线程 + 阻塞 I/O） | 完全控制状态行/头序/信封形状；省掉框架与运行时内存 |
| reqwest | 手写 HTTP 客户端（`http_client.rs`） | 同上 |
| async-nats | 手写 NATS 文本协议客户端（多地址故障转移，<500 行） | 协议极简 |
| regex | 手写扫描器（`tools/scan_secrets.rs` 等） | 规则少且固定，无需正则引擎 |
| sha2 / hmac / scrypt crate | 自实现 sha256 / sha1 / hmac / scrypt / base64（md5 场景用 sha256[:8] 替代） | 减少外部密码学依赖，缩小攻击面 |
| uuid crate | 手写 uuid5（SHA1 命名空间版） | 逐字节确定性 |
| chrono | 自实现 civil 历法 + 时区归一 | 只用到日期运算与 ISO8601 |

这套「零框架、零运行时、自实现密码学」直接服务内存目标：常驻服务 **3–15 MB/进程**
（对照的 Python 实现普遍 20–40 MB），一次性 CLI 仅 2–7 MB。

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
docs/                   ARCHITECTURE · USER_GUIDE · PARITY · MIGRATION · HANDOFF
```

### 常驻服务端口表

| bin | 端口 | 职责 |
|---|---|---|
| `chat_svc` | 9980 | 群聊服务（PostgreSQL，错误信封 `{"ok":false,"error":{"code","message"}}`） |
| `blackboard_svc` | 9940 | 共享黑板（PostgreSQL，平信封契约，`--peers-json` 认证） |
| `blackboard_view` | 9941 | 黑板 Web 视图（HTML 资产 `include_str!` 内嵌） |
| `task_svc` | 9981 | 任务服务 |
| `a2a_svc` | 9920 | Agent-to-Agent 派活边缘 |
| `teamd` | 9960 | 成员入网控制面（NATS `a2a.v1.req.<member>`） |
| `cli_backend` | 9970 | 把派活落到真实 CLI agent（pi/claude/codex 或 `--stub` 回声桩） |
| `nodelet` | 9971 | 节点守护探针（只读，键序冻结输出） |
| `serve` | 自定 | L4 前端静态 + `/api/*` 反代 + `/login` |

CLI / 运维件：`team`（成员统一入口：who/say/broadcast/meet/group/history）、`post_group`、
`roundtable`、`rt_remind`、`shared_content`、`session_ctx_archiver`、`group_watch`、
`gen_nats_conf`（生成中枢 NATS 配置）、`roster_creds_mirror`（凭据镜像）、`a2a_call`、
`team_call`、`team_broadcast`、`team_roster`。

## 开发者：从源码构建与测试

Linux：

```bash
cargo build --release
cargo test          # 262 个测试全绿
```

Windows 用 GNU host 工具链：先 `. .\scripts\dev-env.ps1`，再同上。

交叉编译发布包（ musl 静态 + MinGW）：

```bash
sudo apt-get install -y musl-tools mingw-w64
rustup target add x86_64-unknown-linux-musl x86_64-pc-windows-gnu
CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=x86_64-linux-musl-gcc \
  cargo build --release --target x86_64-unknown-linux-musl -p hacs-c
CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER=x86_64-w64-mingw32-gcc \
  cargo build --release --target x86_64-pc-windows-gnu -p hacs-c
```

测试覆盖：网关线上字节形态、NATS 完整握手、uuid5 逐字节一致、浮点 `repr` 与银行家舍入、
scrypt 逐字节、探针只读性（写不进去断言）、脱敏口径等。
提交前跑 `rustc -O tools/scan_secrets.rs -o /tmp/scan_secrets && /tmp/scan_secrets .`——
命中即禁止提交（含密钥的文件不能进仓）。

加一个新服务：在对应层 crate 写模块（纯逻辑，不碰 socket）→ 在 `hacs-c/src/bin/` 加薄壳
`[[bin]]`（只装配）→ 测试放 `crates/*/tests/`。

## 用户：从 Release 部署

从 [Releases](https://github.com/shihezichen/HACS_rust/releases) 下载对应平台包：

| 包 | 平台 |
|---|---|
| `*-linux-musl.tar.gz` | Ubuntu / Debian 等（**全静态**，解压即用，零系统依赖） |
| `*-centos7.tar.gz` | CentOS 7 / RHEL 等老 glibc（同为 musl 静态） |
| `*-windows.tar.gz` | Windows x64 |

每个包 `bin/` 下 23 个可执行文件 + 本手册副本。部署分中枢机与成员机两种角色：

- **中枢机**：装 `nats-server`（系统包，第三方）与 PostgreSQL → 用 `gen_nats_conf` 生成配置 →
  起 `teamd` / `a2a_svc` / `cli_backend` / `chat_svc` / `blackboard_svc`。
- **成员机**：只需 `team` CLI + 自己的凭据文件。

完整的逐步命令、配置文件形态、验证方法与故障速查见
**[`docs/USER_GUIDE.md`](docs/USER_GUIDE.md)**（所有步骤实机验证过）。

## 文档

- [`docs/USER_GUIDE.md`](docs/USER_GUIDE.md) —— **使用手册**：部署、组网配置、故障速查
- [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) —— 分层 ↔ crate 映射、依赖方向、关键实现决定
- [`docs/PARITY.md`](docs/PARITY.md) —— 行为等价性记录（58 条，含已知偏差）
- [`docs/MIGRATION.md`](docs/MIGRATION.md) —— 逐服务替换流程与回退
- [`docs/HANDOFF.md`](docs/HANDOFF.md) —— 交接文档：环境坑、关键决定、复跑命令
