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
23. **`a2a_edge` 能力端口降级**：Python 由组合根 a2a_svc.py 注入 backend_of /
    session_headers / read_usage / pressure / get_routes；Rust 版 a2a_svc 目前
    全部走降级口径（"other" / {} / None / None / 无额外路由），行为语义一致
    （绝不编造数字），真实能力随层 C 组合根批次注入。
24. **`a2a_edge` 续跑循环的上下文自愈**：软/硬阈值逻辑已实现常量与出口
    （loop_stop 带回 ctx_soft/ctx_rotations），但 pressure 端口未注入前不触发
    （Python 未注入时同样不触发）。
25. **`a2a_edge` 日志**：Python 一行一请求（`<方法> <路径> -> <码> from=<成员>`）；
    Rust 版暂以启动横幅 + stdout 打印为主，逐请求日志行待 web_gateway 日志钩子
    对齐后补。
26. **`task_svc` 时间口径**：Python `datetime.now(CST).isoformat(timespec="seconds")`
    → Rust 自实现 civil 历法（Hinnant 算法），输出逐字节同形 `YYYY-MM-DDTHH:MM:SS+08:00`；
    `parse_iso` 接受任意 `±HH:MM` 偏移与 `Z`（对齐 `datetime.fromisoformat`）。
27. **`task_svc` uuid4**：Python `uuid.uuid4().hex` → Rust 读 `/dev/urandom` 自实现
    （不引 uuid crate）；`T-` + hex[:8]、`LT-` + hex[32] 形状一致。
28. **`task_svc` 事务粒度**：Python `with self.conn:` 单事务；Rust rusqlite 默认
    autocommit，report/ack 的多语句写以 Mutex 持锁粒度等价（单连接单写者，无并发
    撕裂面）；`create` 的 IntegrityError 兜底路径逐条保留。
29. **`task_svc` PATCH 响应**：Python `out["reply"] = reply_deliver(...)`；
    Rust bin 侧 PATCH 处理器把 `task.reply` 提升到响应顶层（`{"ok","task","reply"}`），
    形状一致。`--no-reply` / `reply_queued` / `autopost_disabled` 语义逐条对齐。
30. **`task_svc` 白名单**：env `TASK_AGENT_WHITELIST` 未设时默认 `hermes,openclaw-nb`
    （Python 模块级默认同值）；`--agent-whitelist` 覆盖。

31. **`chat_svc` 访客解析**：Python 走 `view_auth.check_cookie`（会话 cookie）+ tailnet
    免登录；Rust 版只认 tailnet 免登录（`HACS_TAILNET_CIDR` 覆盖，语义同 view_auth），
    cookie 会话通道未接（无外部看板签发依赖时 401），其余鉴权/密级口径（owner 全看、
    internal 起步、404 不泄露存在性）逐条对齐。
32. **`chat_svc` PresenceProbe / 上下文水位 / 模型富化 worker**：三者都是增强能力——
    Rust 版 `/healthz` 回 `presence={"nodes":{}}`、`enrich={"running":false}`；
    `/todos/enrich` 恒 503 `enricher_not_running`；`_agents` 的 `online/last_hb_at`
    恒 null（「未知」，不谎报在线）；agent 消息不挂 `meta.context` 水位。
33. **`chat_svc` 静态托管 / 登录跳转 / 计算器反代**：`/login`、`/logout`、`/calc*`、
    ui/ 静态与 SPA 兜底未移植（前端产物归 <MEMBER_ID>，Rust 侧无 ui/）——非 `/api`
    路径一律 404 对象信封。
34. **`chat_svc` chat_messages append-only 触发器**：Python 由 `sql/chat_schema.sql`
    外部部署（触发器拒 UPDATE/DELETE）；Rust `ensure_schema` 兜底建表不含触发器
    （E2E 测试库需要清库），生产行为依赖外部部署同一份 schema.sql。
35. **`chat_svc` SSE `text/event-stream`**：Python 契约 ② 阶段的推流端点本版未在
    Python 侧实现（501 兜底），Rust 同样落 501 `not_implemented`，行为一致。
36. **`blackboard_svc` 移植完成（v1.9 对齐）**：4558 行 Python → L3 纯逻辑+数据访问
    `blackboard_svc.rs`（约 2600 行，含 14 个单测）+ HTTP bin `bin/blackboard_svc.rs`。
    路由面全量对齐：/healthz、/entries、/search、/sessions、/actions、/issues、
    /tracking、/tmp、/sess、/msg、/api/shared|/shared（GET 只读 + 写 405）、admin 健康口。
    错误契约：平信封 `{"ok": false, "error": "<code>", "detail": "..."}`（Python v1.9 对齐）。
    E2E：11 个业务路由真 PG（15432，8 个 schema + pgvector）全链路 ok:true——
    entries 写读、tmp 读满销毁（reads_max=1 → read_quota）、sess 建会/turn/关闭归档
    （归档落 shared_content + shared_tasks 容器）、msg 投递→收件箱→ack→done、
    search（关键词降级）、tracking/counts（缺表降级 None）、shared list/search/detail/tasks。
37. **`blackboard_svc` postgres crate 参数类型坑**：psycopg2 的 `%s::jsonb`/`%s::vector`/
    `::timestamptz` 传 str 都行，postgres crate 严格按目标类型 OID 序列化——String 会
    `error serializing parameter N`。修法：jsonb 传 `serde_json::Value`；vector 用
    `$N::text::vector`（先声明 text 再转）；timestamptz 用 `$N::text::timestamptz`；
    `make_interval(secs => $N)` 传 f64（不能 i64）、`days => $N::int` 传 i32。
38. **`blackboard_svc` 嵌入模型未接入**：Python /healthz 的 embed 段报 BAAI/bge-small-zh-v1.5
    （loaded:false），Rust 报 `{"ok": false, "reason": "rust 移植暂未接入嵌入模型"}`；
    /search 与 /shared/search 语义检索降级为关键词（mode 如实报 keyword）。语义排序
    （pgvector <=>）的 SQL 路径已就绪（$N::text::vector），接模型后即生效。
39. **`blackboard_svc` 内存对比（P3 收益实测）**：同一 PG 实例各 100+ 请求预热后：
    Python 3.11 VmRSS 39,292 kB（Pss 26,609）；Rust release VmRSS 5,300 kB（Pss 1,870）；
    Rust debug VmRSS 6,536 kB。**RSS 降至 Python 的 ~13.5%（省 ~86%）**。
40. **`blackboard_svc` msg_log sweep 的 stalled 常量**：Python 把 MSG_STALL_AFTER_S 直接
    拼进 last_error 文案（f-string）；Rust 同样编译期常量拼 SQL 文本（make_interval 的
    秒数用字面量 `{N}.0`），行为一致。

41. **`blackboard_view` X 层 scrypt 双端自实现（本轮修出两处 RFC 7914 偏差）**：
    ① Integerify(X) 是最后 64 字节子块前 8 字节按**小端** u64（原用大端 word 取模）；
    ② B 块按 32 位**小端** words 解释（原用大端装载）——真实参数 N=16384, r=8 下
    与 Python `hashlib.scrypt` 校验值一致后全链路（登录/改密/锁定）E2E 通过。
    测试参数（n 小、r=1）发现不了这两个 bug——教训：交叉验证必须用生产参数。
42. **`blackboard_view` 移植完成（1.5 对齐）**：X 层 `hacs-x-identity::view_auth`
    （sha256/hmac/scrypt/token/CSRF/LockTracker，15 单测）+ L3 `blackboard_view.rs`
    （ViewStore PG view_* 表 + Auth 门面 + 遮罩 apply_role，6 单测）+ bin（~1700 行，
    端口 9941）：portal/board/healthz/favicon/api/*（透传 502 收敛 + 404 例外）/
    POST /login 唯一写口（login/passwd/logout 三动作 + CSRF）/GET /account。
    E2E：登录/错口令/登录锁（5 败锁 15 分钟，含 view_login_failures append-only
    触发器）、会话 cookie、userbar、改密→全会话作废→新口令重登、退出、405 只读。
43. **`blackboard_view` 内存对比（P3 收益实测）**：同一 PG 实例各 120 轮预热后：
    Python 3.11 VmRSS 32,240 kB（Pss 18,089）；Rust release VmRSS 3,504 kB（Pss 1,606）。
    **RSS 降至 Python 的 ~10.9%（省 ~89%）**。
44. **`blackboard_view` 路由闭包与 auth_gate 的 visitor 双解析**：Python 在 do_GET
    解析一次 visitor；Rust 网关 auth 钩子与路由闭包是两个 Ctx 对象，各解析一次
    （多一次会话查库），语义等价——auth 门禁仍以 gate 结果为准。

45. session_ctx_archiver：Python `str.lines()`/`re` 全部手写对应物（API_RE/编号切分/BLOCKED_BY_RE/resume 指针剥离）；`_clip`/`_clean_line` 按 Python 字符语义；`[\W_]` 长度统计用 Unicode alphanumeric。agent.log last_ts 取行首 19 字符。one-shot CLI，内存对比 Py 22,780 kB vs Rust release 4,736 kB（4.8×）。
46. session_ctx_archiver：群消息/memo PATCH 走 L1 手写 http_client（Connection: close，不跟重定向）；memo PATCH 409 重试 3 次 × 400ms 间隔 = Python；state.db 用 rusqlite 只读模式（=sqlite3 uri mode=ro）。

47. shared_content：Rust 侧 embedder 未移植，`compute_embedding` 恒 None（写库向量 NULL、
    `--semantic` 降级关键词，mode 如实报 keyword）= Python 模型不可用路径；接 embedder 后
    (`$N::text::vector` 路径已就绪) 即对齐。
48. shared_content bin：`--sqlite` 替身后端未实现（Python SqliteStore 为测试替身；Rust 单测
    用 MemoryStore 覆盖同一 Store trait），给明确报错；`--no-embed` 旗标接受但为 no-op（同上）。
    PgStore 全文本绑定 + `NULLIF($N::text,'')` 处理空串→NULL（timestamptz/jsonb/vector 三类
    cast 前 ::text 双跳，沿 rt_remind 教训）；数值参数按 i64/f64 二进制绑定。
    内存对比（一次性 CLI，`/usr/bin/time -v` Max RSS，tasks --limit 50）：Python 28,584 kB
    vs Rust release 2,944 kB —— **降至 Python 的 ~10.3%（省 ~89.7%）**。

49. group_watch_pending（+bin group_watch 组合根）：State 落盘格式/键名与 Python 一致
    （done/last/chain，"sid|member|seq"）；人的 @ 优先、Agent @ 带 max-chain、人说话链条
    归零、(sid,member) gap 限流、mark 先于回复检查（NATS 失败也记账不重发）=Python；
    mentions 结构化解析（群内过滤、"所有人"→@all、frm 首位补齐）。真 chat_svc E2E：
    pending 识别/点名/NATS 失败不写回/幂等跳过与 Python 逐行对齐（同群同 seq 同输出）。
    内存对比（--once，`/usr/bin/time -v` Max RSS）：Python 26,112 kB vs Rust release
    1,408 kB —— **降至 Python 的 ~5.4%（省 ~94.6%）**。回复写回链路（deliver→reply→
    post_reply）由 L3 单测 FakeDlv 覆盖；真 NATS 集成路径复用 team_group/broadcast
    已验证的 call_one 实现（本机无 nats-server，登记限制同 PARITY 前件）。

50. embedder：Rust 遵守零外部依赖铁律不引 fastembed/onnxruntime —— load 恒定粘性失败
    （last_error="fastembed 导入失败：Rust 侧未内嵌 ONNX 运行时"），embed_texts 恒 None、
    空列表返回 [] 不触发加载、status() 不触发加载、reset() 丢弃失败标记：行为=Python
    "没装 fastembed" 路径的逐条对齐；配置解析（BB_EMBED_MODEL/BB_EMBED_DIM 非法退默认）
    与 ERROR_MAX=200 截断一致。接模型后只需实现 load_inner()（粘性/单例语义已就位）。
    Python 版无独立入口，Rust 同为 lib-only，无内存对比项。

50. roundtable（L3：roundtable_core + roundtable_sched，bin roundtable）
- 全量移植 Python roundtable.py（2430 行）：会前门禁（goal/agenda 缺一 exit 2）、JSON 提取三路降级（```json 块→整段→括号平衡扫描）、parse_turn/parse_confirm（解析失败一律 abstain 不替人点头）、refs 空不入库、parse_degraded 降级（整段留痕 + refs 自引）、收敛判据①相似度(trgm 三元组自实现)②立场③投票④硬上限、round_no<2 不许收敛、当场确认（分母=在场应答者，无 object 且 agree>=⌊在场/2⌋+1）、落实事项三硬口径（what/owner∈roster/due_at 未来）、缺席=defect 单（24h、同场一人一条、evidence 记次数）、出席率分母=应到人数、prompt 省 token（片段≤12 条×240 字符，超限先砍最旧→砍非当前议程行→硬截）。
- Rust 侧差异（登记）：①相似度用自实现 trgm 三元组（Python re 去除非 [0-9a-z一-鿿] 后 padded 3-gram），未用 pg_trgm 语义完全一致；②new_session_id 尾巴 4 hex 用纳秒^pid 混合熵代替 uuid4（形状 rt-YYYYMMDD-HHMMSS-xxxx 一致）；③due_at 落库为 UTC ISO 字符串（`$n::text::timestamptz`），Python 直接传 datetime；④bin 增加 `RT_FAKE_REPLY=1` 环境口（DryRunTransport 可与 --commit 组合，供真库 E2E；Python 无此口，dry-run 与 commit 互斥）；⑤确认 verdict 解析失败时 note 拼接格式与 Python 一致（`note（err）`）。
- 单测 18 个（core 10 + sched 4 + bin 场景 4）：dry-run 全链 converged、缺席 defect 开单、object 阻断 deferred、确认分母口径、actions 三硬口径、prompt 预算夹逼等。
- 真库 E2E（PG@15432 + blackboard_svc@9940 + RT_FAKE_REPLY）：aborted 路径（真 NATS down：探针无应答→缺席→0<2 中止→defect×2 落库 ✓）；converged 全链（create_session→turns×4→黑板条目→结论条目→确认→actions（due_at=+7d timestamptz）→issues→sessions state=converged/confirmed ✓）。
- 内存：Python 30,496 kB vs Rust 3,584 kB = **8.5×**（release + /usr/bin/time -v Max RSS）。

52. gen_nats_conf / serve / roster_creds_mirror（C 层 + tools 面三件）：gen_nats_conf 口令只从 creds/<成员>.pass 读、缺件必拒 rc=2、--drop-user 唯一退场、--dry-run 不写盘（与 Python 对齐）；serve 仅开发用静态+反代，/api 错误信封对齐 chat_svc 契约；creds_mirror 纯逻辑（probe/parse_probe/analyze/exit_code）逐条对齐，出口只有 sha256[:12]、口令只走 ssh stdin 绝不进 argv，采集失败绝不退化为 0/1。Rust 侧 ssh 目标表用 env AGENT_TEAM_SSH_TARGETS（Python 为内置真值表）——部署表来源不同，口径一致。RT_FAKE_REPLY 已在 51 登记。

53. nodelet（L0 轻量节点守护 + bin，9971）：绑定面只放行环回/尾网（拒启不回退）；单元状态两布尔（查不出来=不认识，不编造）；凭据红线出口机械复核（形状违规即 panic）；探针键序冻结断言；--once/--node/--bind/--services 薄壳口径对齐。偏差：① version 指纹 Python=磁盘 nodelet.py 的 md5[:8]，Rust=二进制自身的 sha256[:8]（自观测"该不该重启"用途不变，算法不参与跨实现比对）；② 档案面 profile.file 记 "node_profile.rs"（Python 记模块文件名）；③ 时间戳 UTC Z 形态一致；④ services 查询 stderr 丢弃口径一致，GBK 中文 Windows 场景 Rust 侧 utf-8 lossy（Linux 部署无差）。

54. team（成员侧统一 CLI bin，Python node/c/team.py + L1 team_cli_core.py）：who/say/broadcast/meet/history/group 全子命令；身份推断（nats_dir/*.pass 恰好一个）、30 分钟去重（sha1+窗口，失败回滚）、限并发扇出（自实现信号量）、按小时分桶的 say 默认会话、互斥检查最前（SESS-07）、退出码 0/1/2/3 一致。偏差：① say 默认会话按北京时间分桶（Python time.localtime 本地时区；服务器同 CST 无差）；② --new-session 随机 4 位用时间哈希（Python random）；③ group say/pull/show 的 HTTP 走内联 curl 子进程（Python urllib；错误形状一致）。
55. post_group（L1 出口件 bin）：选群（显式>env>默认群>最近 open，群标题唯一命中否则拒发）、@ 提及过滤（名册校验+@所有人→@all）、回读校验（前 40 字比对）、--todo-id/--todo-status 同给同不给、翻 done 须 --landing。HTTP 用手写 http_client（X-Agent-Id 头），错误形状 network/http_N 一致。E2E @19980：发送+回读校验 seq=3 通过、todo 缺件 404 透传、参数一半报错 rc=2。

56. scan_secrets（提交前密钥扫描门禁，Python tools/scan_secrets.py → Rust tools/scan_secrets.rs 独立 rustc -O 单文件，不入 workspace）：两路口径（凭证指纹 + 赋值即口令）手写对应物 regex-free；指纹逐条对齐（github/aws/private-key/feishu/slack/openai/anthropic/jwt/pg-dsn/url-basic-auth/bearer）；private-key 用「BEGIN 与 PRIVATE KEY 之间仅大写+空格」复刻正则语义（substring contains 会误伤自家字面量）；PLACEHOLDER/NUMERICISH/CONSTNAME/LITERALISH/KEY_HINT 词边界口径一致；FIXTURE_MARK 路2豁免路1照扫；掩码 head3…tail2+len 不回显原文；rc 0/1/2。偏差：Rust 结构体字段初始化 `token: String,`（无引号裸标识符值）按变量引用放过 —— Python 树无此形态（字段带类型注解），属 Rust 侧必需豁免。E2E：Rust 树 clean、Python 树 clean、伪造 ghp_ token 命中 rc=1、夹具豁免 ✓。
