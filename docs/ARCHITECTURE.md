# HACS_rust 架构

本文回答「Rust 版怎么落地 Python 版的分层」，并登记与 Python 版的**结构性对应**与**刻意的偏差**。
分层模型本身以 Python 版 [`docs/architecture.md`](https://github.com/shihezichen/HACS/blob/main/docs/architecture.md)
为真源；本文只讲重写侧。

## 1 分层 ↔ crate 映射

| 层 | Python | Rust crate | 一句话职责 |
|---|---|---|---|
| `C` 组合根 | `node/c/*` | `hacs-c` | 进程入口装配（只做装配与注入） |
| `L0` 平台 | `node/l0_host/*` | `hacs-l0-host` | 宿主事实唯一出处（路径 / 档案 / 名册 / 探针） |
| `L1` 通信 | `node/l1_transport/*` `hub/l1_relay/*` | `hacs-l1-transport` | 投信封 / 保投到 / 不重复 / 成员入网控制面 |
| `L2` 适配 | `node/l2_adapter/*` `hub/l2_adapter/*` | `hacs-l2-adapter` | Agent 差异唯一落脚点 |
| `L3` 协作域 | `hub/l3_domain/*` | `hacs-l3-domain` | 群聊 / 任务 / 会议 / 黑板 / 共享内容 |
| `L4` 呈现 | `hub/l4_experience/*` | `hacs-l4-experience` | 静态壳同源托管 / 反代 / 鉴权跳转 |
| `X` 横切 | `hub/x/view_auth.py` | `hacs-x-identity` | 身份与授权（可被任意层引用） |

## 2 依赖方向 = 编译期事实

Python 版的「依赖方向」是**运行时门禁**（`tools/check_layers.py` 读 `tools/hacs_layers.json` 判 import 图）。
Rust 版把它变成**编译器**的事：

- 各层 crate 的 `Cargo.toml` 只声明自己**允许**依赖的层。
- 于是「`L3` 反向引用 `C`」「`L0` 引用 `L1`」这类违规**根本编译不过**，不需要一个门禁脚本去事后抓。
- `hacs-c` 是唯一声明了全部层件的 crate（`C` 可依赖任意层）；其余层都声明不到 `hacs-c`。

```
hacs-c ──► hacs-l4-experience ──► hacs-l3-domain ──► hacs-l2-adapter ──► hacs-l1-transport ──► hacs-l0-host
   └──────────────┴─────────────────────┴───────────────────┴────────────────────┴───────────────► hacs-x-identity
```

**代价（如实记账）**：Python 的 `hub/`（中枢相）与 `node/`（节点相）是**相**的划分（检查 6 的硬门禁）。
Rust 侧目前只镜像了**层**，没有镜像「相」。若后续需要，可在 crate 名或 feature 上再切一刀；
在那之前，相方向靠 code review 与（将来纳入的）静态检查保证。

## 3 三条贯穿全层的口径

### ① 只读与不编造（`L0` 铁律）

- **只读**：`L0` 的宿主探针一律以 SQLite `mode=ro` URI 打开、用完即 close；不建目录、不写状态。
  Rust 侧用 `rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | SQLITE_OPEN_URI`。
- **不编造**：没配置 / 文件不在 / 记录不在 → `None` 或空；**真出错原样抛**（`Err`）。
  绝不把「打不开」偷偷变成「没有数据」。
- 已由测试钉死：`hermes_probes_are_read_only_and_do_not_guess`（含「拿到的句柄写不进去」断言）。

### ② env 优先 > 平台惯例（`L0`）

`node_paths` 的所有默认值都遵循「env 覆盖优先，否则该平台惯例位置」；
函数接受可选 `flavour`（`"nt"`/`"posix"`），便于单测在两种平台语义下断言而不必真换机器。

### ③ 传输不含语义（`L1`）

`web_gateway` 不出现任何业务词；`nats_client` 不认识群聊 / 会议 / 待办。
所有语义由调用方在闭包里带入。

## 4 关键实现决定

| 决定 | 理由 |
|---|---|
| **手写 HTTP**（不引 axum/hyper） | 既有各件的响应形态是 `http.server` 的形状；框架会改掉状态行 / 头序 / 信封。手写还直接省掉框架的内存开销。 |
| **`serde_json` + 自定义 Formatter** | `json.dumps(..., ensure_ascii=False)` 用 `, ` `: ` 分隔符，serde_json 默认是紧凑的；自定义 Formatter 逐字节对齐。`preserve_order` 保 dict 键序。 |
| **`rusqlite` 静态编译 SQLite** | 不依赖系统 sqlite，Linux/Windows 一致。 |
| **不用异步运行时** | 既有各件是「线程 + 阻塞 I/O」（`ThreadingHTTPServer`），Rust 侧同构即可；省掉运行时内存与复杂度。 |
| **`WIN_NODE_EXE` 等沿用脱敏占位符** | Python 源码仓里那些值就是占位符（真值由 `AGENT_TEAM_NODE_EXE` 等 env 注入）。Rust 侧同口径，不在仓里写机器真值。 |

## 5 已知偏差（不消红，如实登记）

见 [`PARITY.md`](PARITY.md) 的「已知偏差」一节。
