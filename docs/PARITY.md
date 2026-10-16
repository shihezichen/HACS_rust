# 线级兼容契约与已验证面

「线级兼容」的判据：**同端口、同 HTTP 路径与状态码、同 JSON 字段与格式、同鉴权头、
同 NATS 主题与信封**。目标是 Python 与 Rust 可以**混跑**（同一协作域里一半进程是 Python、
一半是 Rust），从而支持逐服务、逐节点替换。

## 1 接口级承诺（逐服务）

| 服务 | 端口 | 承诺 |
|---|---|---|
| `a2a_svc` / `a2a_edge` | 9920 | 同路径 / 同 JSON / 同令牌鉴权面（`/ctx-usage` 等） |
| `chat_svc` | 9980 / 9984 | 同路径表 / 同群消息信封 / 同静态件托管语义 / 同 SPA fallback |
| `blackboard_svc` | 9940 | 同 `/entries` 写入面 / 同健康口 |
| `blackboard_view` | 9941 / 9942 | 同只读视图 / 同登录门户 / 同会话 cookie 口径 |
| `task_svc` | 9981 | 同 `/tasks/*` 取队/汇报/租约面 / 同 at-least-once 语义 |
| `teamd` | 9960 | 同管理口状态行与响应形态 |
| `cli_backend` | 9970 | 同请求矩阵（含 502/503/504 兜底） |
| `nodelet` | 9971 | 同 `/probe` 的**键名与顺序冻结**的 JSON / 同凭据红线 |
| `team`（CLI） | — | 同 argv 树 / 同 stdout / 同退出码 / 同出站请求序列 |

## 2 响应形态对齐（已落地的细节）

`web_gateway` 刻意复刻 `http.server` 的形态，已由测试钉死：

- 状态行 `HTTP/1.0 <code> <reason>`（与基类一致；HTTP/1.1 由子类覆盖）。
- 头序：`Server:` → `Date:` → 调用方头 → `Content-Length`（若调用方没给）。
- `Date:` 是 RFC 1123 GMT（自实现民用历反算，不引时间库）。
- `Content-Type: application/json; charset=utf-8`（JSON 回复默认）。
- JSON 体按 Python 默认分隔符 `, ` `: ` 序列化，`ensure_ascii=False`（UTF-8 原样）。
- 未注册的 HTTP 方法保持**基类 501 行为**（不自行发明 405 信封），含 `http.server` 的
  HTML 错误体与 `Unsupported method ('X')` 状态行。
- 路径登记与分发**同一套归一化**（`rstrip("/")`，空则 `/`）—— 避免「带尾斜杠的登记变静默死路由」。

## 3 NATS 客户端已保留的两个坑

1. `MSG <subject> <sid> [reply] <size>` —— 带 reply 时字段错位，`size` 永远在**最后一个**字段，
   按字段个数（4 或 5）分别取。
2. `-ERR` 必须落到 `last_err` 并回调使用者（服务端权限违规靠它报，静默吞掉会变成哑故障）。
3. 收件箱 `_INBOX.<pid>.<seq>.<tid>.<rand>` **进程内唯一**（并发多连接若撞车会出现
   「所有成员回复内容一模一样」的幽灵应答）。

## 4 已验证面（当前 49 例）

| 面 | 用例 |
|---|---|
| `web_gateway` 线上字节 | 状态行 / `Content-Type` / `Content-Length` / Python 分隔符 / `Server` 头 |
| `web_gateway` 路由 | 精确路由 / 前缀路由 / 路径归一化（登记 `"/api/"` 命中 `"/api"`） |
| `web_gateway` 兜底 | 默认 404 信封 / 未注册方法 501 / 钩子短路 / `HttpError` 短路 |
| `web_gateway` 请求体 | 非法 JSON → 400 `bad_json` / 超限 → 413 `payload_too_large` |
| `http_client` | `quote` 对齐 `urllib.parse.quote(safe="")` / 对真网关取 JSON / 连不上按超时报错 |
| `nats_client` | 地址解析 / 对假服务端完整握手 / PUB-SUB / request-reply / 连不上给人话错误 |
| `nats_client` 凭据 | 令牌文件去 BOM 去空白 |
| `L0 node_paths` | env 优先与 trim / 平台默认值形状 / 默认目录必须已存在 |
| `L0 node_profile` | 目标主机档案（登记 / 未登记兜底 / JSON 整表覆盖 / 坏 JSON 退回内置） |
| `L0 node_profile` 探针 | Hermes 用量行与活跃消息 / 只读性 / 记录不在 → `None` 不猜 |
| `L0 member_roster` | 档案顺序 == 规范序 / 查询不猜 / 契约词汇冻结 |
| `L2 pg_literals` | 向量字面量逐字节（含 `nan`/`inf`）/ `repr(float)` 指数写法 / 时间归一与非法输入 → `None` |
| `L2 agent_usage` | token 估算公式 / **四舍六入五成双** / 分母链最长匹配 / 生效配置仅 hermes 后端 / 未知后端不猜 |
| `L2 agent_model` | ssh 命令行**选项顺序** / 远端路径跟目标主机走 / 解析校验与截断 / 失败一律降级不抛 / 展示名覆盖 |
| `X redact` | 抹 `password/passfile/sslpassword=值`（保留关键词原大小写）/ `\S+` 空值不命中 / 400 字符截断 |

## 5 已知偏差（不消红，如实登记）

1. **`Server:` 头的 `sys_version`**：Python 会拼上 `Python/3.x`（如 `Server: teamd/1.0 Python/3.13.9`），
   Rust 侧默认 `sys_version = ""`（`Server: teamd/1.0 `，注意尾部空格与 Python 一致）。
   有子类在 Python 侧把 `sys_version` 置空（如 `a2a_edge`）—— 那类服务逐字节一致。
   HTTP 客户端不解析 `Server:`，不影响行为。
2. **JSON 浮点格式**：Python `repr(float)` 与 serde_json 的浮点输出在个别值上不同
   （如 `1e16` / `0.1` 的边界）。整型 / 字符串 / 布尔 / null 逐字节一致。
   响应里出现裸浮点的服务需在替换前对照实测。
3. **主机名获取**：`node_profile::node_name()` 先读 env（`COMPUTERNAME`/`HOSTNAME`），
   再退 `hostname` 命令；Python 用 `socket.gethostname()`。取值语义一致。
4. **`AGENT_TEAM_TARGET_PROFILE_JSON` 覆盖**：只保留 `flavour` / `home` / `claude_exec` 三个键
   （与 Python 消费面一致）；额外键（若有）不影响结果。
5. **`etc_dirs` 的 nt 默认值**：Windows 上当 `AGENT_TEAM_ETC_DIR` 未配置时返回
   `[runtime_dir]`；Windows 部署把配置直接放在运行目录里 —— 与 Python 一致。
6. **脱敏占位符**：`WIN_NODE_EXE` / `WIN_PI_CLI_JS` / `WIN_PI_WORKDIR` / `WIN_CODEX_WORKDIR` /
   `NT_TARGET_HOME` 等沿用 Python 源码仓的占位符口径（真值由 env 注入）。
7. **`window_for` 的第 ② 级（Hermes 自带模型目录 `DEFAULT_CONTEXT_LENGTHS`）Rust 侧跳过**：
   Python 靠 `sys.path` 注入后 `import agent.model_metadata` 取表；Rust 进程里没有 Python 解释器，
   这级无法复刻。**影响面**：只有在「① 生效配置不命中、③ 兜底表不命中」时才可能少一个分母来源
   —— 而 ④ 别名兜底（仅 hermes 后端）会接住绝大多数这类情况；最终仍取不到就返回 0 = 「不知道」，
   调用方降级为不自愈（与 Python 同向，不会静默用错分母）。
8. **`repr(float)` 的「最短表示撞车」边缘**：Rust `{:?}` 与 Python `repr` 都是最短往返十进制，
   同族但**不是同一实现**，在极少数存在两个等长最短表示的浮点上可能各选一个（数值仍然都能往返，
   写进 pgvector 的向量不影响检索结果）。已覆盖常见形态的逐字节断言。
9. **`agent_usage` 的 debug 日志**：Python 走 `logging`；Rust 侧由 `HACS_DEBUG` 环境变量开关
   （非空且非 `0` 时写 stderr）。默认静默。
10. **模块级 env 捕获 → 惰性求值**：Python 的 `agent_model` 在 **import 期**就把
    `AGENT_TEAM_ETC_DIR` / `AGENT_TEAM_SSH_TARGET` / `AGENT_TEAM_DISPLAY_NAMES` 定死
    （要求 env 先于 import）；Rust 侧一律**调用时**读取。行为上更正确（env 在进程启动前就设好，
    两种口径等价），但在「同进程内运行期改 env」这种用法下两者会分叉 —— 那是 Python 的陷阱，
    不是可依赖的行为。
11. **`round(x, 2)`**：`agent_model` 的 `elapsed_s` 用 Rust 的 `(x*100).round()/100`，
    与 Python `round(x, 2)` 的十进制四舍六入五成双在极边界值上可能差 0.01。这是**纯展示字段**
    （耗时），不参与任何判定。
12. **`X` 横切只落了 `safe_err`**：`view_auth` 的会话令牌 / 角色密级 / 时间口径
    （`ts` / `parse_ts` / `beijing`）尚未迁移 —— `blackboard_view`（`P3`）落地前必须补齐。
13. **`cli_backend` 的 stub 模式与默认 key 路径分隔符**：`--stub` 的回声桩在 Rust 侧用
    `sh -c`（Python 用 `sys.executable -c`），回声行为一致；默认 key 文件路径 posix 落
    `<runtime_dir>/cli-backend-key`（Python 字面量是 Windows 分隔符，posix 上由部署 env 注入，
    两者在各自平台上等价）。
14. **codex 会话 id 的 banner 正则**：Python 用 `session id:\s*<uuid27>` 宽松匹配；
    Rust 用「`session id:` 后取 hex/`-` 段并按 8-4-4-4-12 校验」——对合法 UUID 两种口径同结果，
    对畸形输出 Rust 更保守（不认）。会话抓不到只会走「首轮新建」路径，不影响正确性。
15. **`cli_backend` 的日志格式**：Python 走 `logging`（`asctime levelname message`）；
    Rust 侧是「unix 秒 + INFO + 单行」，字段/取值一致（`model= member= task= elapsed_ms= code= rc= pid=`），
    时间戳形态不同。日志是诊断面，不进任何契约。
16. **`Server:` 头 keep-alive**：`cli_backend` 与 Python 同为 HTTP/1.1 + `sys_version=""`
    （`Server: cli_backend/1.0`，逐字节一致）；`Date:` 头用 RFC1123 自算，格式一致。
17. **`teamd` 的 PING 探活**：Python 版每 60s 发 `PING`；Rust 版用 `flush(5s)` +
    `is_connected()` 组合替代（语义等价：探活失败即重连），间隔同为探活循环粒度。
18. **`teamd` worker 回投连接**：Python worker 共用主连接回投；Rust 版每个 worker
    自持一条 NATS 连接回投（避免 worker 长任务阻塞 reader 回调线程）。busy 回复
    用短命连接（低频路径）。
19. **`teamd` 日志时间戳**：Python 用 `time.strftime` 本地时区；Rust 侧按 UTC 简化
    （诊断面，不影响协议 parity）。
20. **`team_broadcast` 并行度**：Python 用 `threading.Semaphore`；Rust std 无信号量，
    用 Mutex+Condvar 自实现（语义同：限并发、入参同序汇总）。
21. **`team_call` elapsed 舍入**：Python `round(t,1)` 银行家舍入；Rust 按 0.1s 网格
    四舍五入（诊断面，差异 <=0.05s）。
22. **`teamd` 心跳载荷**：Python 载含 host/uptime_s/pid 等宿主字段；Rust 版当前只发
    member/ts/v（roster 的 uptime/pid 列会显示空）——待 L0 node_profile 宿主事实
    接入后补齐（偏差登记，不阻塞协议 parity）。
