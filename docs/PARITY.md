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

## 4 已验证面（当前 34 例）

| 面 | 用例 |
|---|---|
| `web_gateway` 线上字节 | 状态行 / `Content-Type` / `Content-Length` / Python 分隔符 / `Server` 头 |
| `web_gateway` 路由 | 精确路由 / 前缀路由 / 路径归一化（登记 `"/api/"` 命中 `"/api"`） |
| `web_gateway` 兜底 | 默认 404 信封 / 未注册方法 501 / 钩子短路 / `HttpError` 短路 |
| `web_gateway` 请求体 | 非法 JSON → 400 `bad_json` / 超限 → 413 `payload_too_large` |
| `nats_client` | 地址解析 / 对假服务端完整握手 / PUB-SUB / request-reply / 连不上给人话错误 |
| `nats_client` 凭据 | 令牌文件去 BOM 去空白 |
| `L0 node_paths` | env 优先与 trim / 平台默认值形状 / 默认目录必须已存在 |
| `L0 node_profile` | 目标主机档案（登记 / 未登记兜底 / JSON 整表覆盖 / 坏 JSON 退回内置） |
| `L0 node_profile` 探针 | Hermes 用量行与活跃消息 / 只读性 / 记录不在 → `None` 不猜 |
| `L0 member_roster` | 档案顺序 == 规范序 / 查询不猜 / 契约词汇冻结 |

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
