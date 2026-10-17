# HACS_rust 使用手册（部署与组网）

> 本手册所有命令与参数均在本机（Ubuntu 24.04，单机扮演中枢 + 成员两种角色）**实机验证通过**
> （2026-10，E2E 记录见 docs/PARITY.md 第 58 条）。示例中的口令均为测试值，生产请换强口令。

## 快速上手（一键脚本，推荐）

v0.1.2 起 Release 按**角色拆包**，附一键安装脚本（全部实机验证过）：

| 包 | 装在哪 | 内容 |
|---|---|---|
| `hacs-rust-*-linux-hub.tar.gz` | 中枢机 | 22 个服务程序 + 安装脚本 |
| `hacs-rust-*-linux-member.tar.gz` | 成员机 | `team` CLI + 安装脚本（<1 MB） |
| `hacs-rust-*-windows-hub.tar.gz` | Windows 中枢 | 同 hub（.exe） |
| `hacs-rust-*-windows-member.tar.gz` | Windows 成员 | team.exe |

**中枢机**（一条命令完成 安装依赖→装程序→建库→生成配置→写 systemd→启动→验证）：

```bash
tar xzf hacs-rust-v*-linux-hub.tar.gz && cd hacs-rust-v*-linux-hub
sudo ./install_hub.sh hermes,lobster            # 参数=成员列表（可再加 中枢IP 对端IP）
# 成功输出：8 步全 ✓，成员凭据在 /etc/nats/creds/<成员>.pass
```

**成员机**（单台手工装）：

```bash
tar xzf hacs-rust-v*-linux-member.tar.gz && cd hacs-rust-v*-linux-member
sudo ./install_member.sh lobster <中枢IP> ./lobster.pass   # 凭据从中枢拷来
# 自动装 team CLI + 写 /etc/hacs/member.env + 放凭据 + who 入网验证
```

**批量部署多台成员机**（在任一已配 SSH 免密的跳板机上，一行循环完成）：

```bash
cat members.txt     # 每行: <成员名> <user>@<host> [中枢IP]
#   lobster  ubuntu@10.4.46.230  10.4.46.228
#   top001   root@10.4.46.231    10.4.46.228
./deploy_members.sh hacs-rust-v*-linux-member.tar.gz members.txt
# 自动 scp 包+凭据 → 远端执行 install_member.sh → 汇总成功/失败
```

> 幂等：脚本可安全重跑（凭据/配置已存在则不覆盖）。服务由 systemd 托管（Restart=always），
> 开机自启。以下 1–3 节是手工部署的逐步说明（调试/无 systemd 环境用）。

---

## 0 角色划分：中枢机 vs 成员机

HACS 是「中枢 + 多成员」的星型拓扑：

```
        ┌──────────── 中枢机（hub）────────────┐
        │  nats-server      消息路由（9930）      │
        │  teamd            成员入网控制面(9960)   │
        │  a2a_svc          Agent 派活边缘(9920)  │
        │  cli_backend      派活到 CLI(9970)     │
        │  chat_svc         群聊(9980, 需 PG)    │
        │  blackboard_svc   黑板(9940, 需 PG)    │
        └──────────────┬───────────────────────┘
                       │ NATS（跨机走 tailnet/IP）
     ┌─────────────────┼──────────────────┐
 成员机 A            成员机 B            成员机 C
 team（CLI）         team（CLI）         team（CLI）
 每台一份自己的凭据    .pass 文件           …
```

| 文件/组件 | 中枢机 | 成员机 |
|---|---|---|
| `nats-server`（系统包） | ✅ 装 | ❌ |
| `nats.conf` + `creds/`（成员口令镜像） | ✅ | ❌ |
| `teamd` / `a2a_svc` / `cli_backend` / `chat_svc` / `blackboard_svc` | ✅ | ❌ |
| `team`（成员 CLI）、**自己的** `<member>.pass` | （中枢上的成员也要） | ✅ 每台一份 |
| PostgreSQL 16 | ✅ | ❌ |
| `serve`（Web 前端） | ✅（可选） | ❌ |

下载：从 GitHub Release 拿对应平台的包（Linux 用 musl 静态包，CentOS/Ubuntu 通用；Windows 用 windows 包）。

```bash
tar xzf hacs-rust-v0.1.0-linux-musl.tar.gz
sudo cp hacs-rust-v0.1.0-linux-musl/bin/* /usr/local/bin/   # 或放到专用目录
```

---

## 1 中枢机部署（按顺序执行）

### 1.1 安装 NATS 并生成配置

```bash
# Ubuntu/Debian
sudo apt-get install -y nats-server
# CentOS：从 nats.io 下载官方二进制放到 /usr/local/bin

# 建工作目录（路径可用 env AGENT_TEAM_NATS_DIR 覆盖，默认 /etc/nats）
sudo mkdir -p /etc/nats/creds && sudo chown -R $(whoami) /etc/nats

# 放一个最小初始模板（gen_nats_conf 是"就地重写"工具，首次需要模板）
cat > /etc/nats/nats.conf <<'EOF'
port: 4222
http_port: 8222
server_name: hub_a
EOF
```

### 1.2 写入成员口令镜像并生成正式配置

`gen_nats_conf` 从名册 + `creds/<成员>.pass` 生成正式 conf（含每成员权限白名单 + 集群路由）。
**缺任何成员的 .pass 文件都会拒绝生成（rc=2）**——名册里的每个成员都必须有凭据文件。

```bash
# 名册可用 env AGENT_TEAM_ROSTER 覆盖（逗号分隔），默认 hermes,lobster,top001
for m in hermes lobster; do
  openssl rand -hex 16 > /etc/nats/creds/$m.pass    # 每成员一个强随机口令
done

AGENT_TEAM_ROSTER=hermes,lobster gen_nats_conf hub_a <本机IP> <对端中枢IP>
# 单中枢时对端 IP 仍需给出（集群路由段留本机自身即可）
```

生成后 conf 形如：客户端口 9930 / 集群路由 9931 / 监控 9932，每成员一条带
`publish/subscribe` 白名单的 user 记录。**改成员集合请重跑 gen_nats_conf，不要手改 conf。**

### 1.3 启动 NATS

```bash
nats-server -c /etc/nats/nats.conf -P /etc/nats/nats.pid &
# 验证
curl -s http://127.0.0.1:9932/varz | grep '"connections"'
```

### 1.4 起 chat_svc / blackboard_svc（需要 PostgreSQL）

两个服务的 DSN 读取顺序：`CHAT_PG_DSN` → `BB_PG_DSN` → `~/.chat_pg_dsn` 文件。

```bash
# 装好 PostgreSQL、建库后：
export BB_PG_DSN='postgresql://hacs:口令@127.0.0.1:5432/chat'
chat_svc --port 9980 &
blackboard_svc --port 9940 --peers-json /etc/hacs/peers.json &
```

`peers.json` 形如 `{"<成员id>": "<该成员token的sha256hex>"}`——成员调黑板/聊天 API 用的
Bearer token 反查表。生成方式：

```bash
echo -n '成员token明文' | sha256sum | cut -d' ' -f1
```

### 1.5 起 a2a_svc（Agent 派活边缘）

需要 `agents.json`（本机有哪些 agent、各自的 backend）与 `peers.json`（同上，鉴权用）：

```json
{
  "node_id": "hub_a",
  "host": "127.0.0.1",
  "port": 9920,
  "agents": {
    "hermes": {
      "backend": "http://127.0.0.1:9970",
      "api_key": "<cli_backend 的 key>",
      "model": "pi/default"
    }
  }
}
```

```bash
a2a_svc --config agents.json --peers peers.json --port 9920 &
# 验证
curl -s http://127.0.0.1:9920/healthz
```

### 1.6 起 cli_backend（把 a2a 派活落到真实 CLI agent）

`--stub` 为内置回声桩（联调用）；生产去掉 `--stub`，用 `--pi-cmd` / `--claude-cmd` /
`--codex-cmd` 指向真实 CLI。key 文件即 API key（fail-closed：读不到拒启动）。

```bash
printf '给一个强随机key' > /etc/hacs/cli-backend-key
cli_backend --port 9970 --key-file /etc/hacs/cli-backend-key \
  --pi-workdir /home/agent/pi-workspace &
```

> 注意：默认工作目录是 `/opt/pi-workspace`（通常无权限），必须用 `--pi-workdir`
> （或 env `AGENT_TEAM_PI_WORKDIR`）指到可写目录，否则 spawn 失败（ENOENT）。

### 1.7 起 teamd（成员入网控制面）

每个成员一条凭据（一成员一凭据是现网拓扑）；单机多成员用逗号列表 + 成员=值形式：

```bash
teamd --member hermes \
  --user hermes \
  --password-file /etc/nats/creds/hermes.pass \
  --nats-url nats://127.0.0.1:9930 \
  --a2a-url http://127.0.0.1:9920 \
  --a2a-token-file /etc/hacs/a2a-token \
  --mgmt-port 9960 &

# 验证（日志出现 conn.up 即连上）
tail -f /var/log/teamd.log
curl -s http://127.0.0.1:9960/healthz
curl -s http://127.0.0.1:9960/varz        # 计数器：hb_sent / rx_requests / ok ...
```

`--a2a-token-file` 内容 = 该成员在 peers.json 里登记的 token 明文（teamd 代成员调 a2a 时用）。

---

## 2 成员机部署

每台成员机只需要 **release 包里的 `team` CLI** + **自己的凭据文件**：

```bash
# 1) 把该成员的 .pass 从中枢凭据镜像安全拷来（scp/人工），放到约定位置：
#    默认推断路径 = nats 目录惯例位置；也可显式 --password-file
scp hub:/etc/nats/creds/lobster.pass ~/.nats-lobster.pass
chmod 600 ~/.nats-lobster.pass

# 2) 验证入网（who：订阅心跳窗口）
team who --as lobster --password-file ~/.nats-lobster.pass \
  --nats-host <中枢IP> --nats-port 9930 --seconds 35
#    输出「在线成员 N 个」及成员表 = 组网成功
#    注意：心跳 30s 一跳，窗口至少给 32s 才能保证收到

# 3) 点名说话
team say --as lobster --to hermes "上线了" \
  --password-file ~/.nats-lobster.pass --nats-host <中枢IP> --nats-port 9930 --timeout 30
#    收到回复 = say→NATS→teamd→a2a→agent 全链通
#    对方离线 = 「点名超时」退出码 1（正确行为，不是故障）

# 4) 群发 / 开会 / 群聊
team broadcast --as lobster --text "通知…" --members hermes,top001 ... 
team meet --goal "定备份频率" --agenda "频率|给数字" --members hermes ... 
team group say --group <群id> --as lobster --text "..." ...
```

`--as` 缺省时按凭据目录里 `.pass` 文件推断身份（目录里恰好一份凭据才可推断）。

---

## 3 中枢可选件

| 件 | 用途 | 启动 |
|---|---|---|
| `serve` | L4 Web 前端（资产已内嵌）+ `/api/*` 反代 | `serve --port 9943 --root /var/www/hacs --backend http://127.0.0.1:9980` |
| `blackboard_view` | 黑板 Web 视图 | `blackboard_view --port 9941`（HTML 内嵌） |
| `nodelet` | 节点守护探针（只读） | `nodelet --bind 127.0.0.1:9971` 或 `--once` |
| `gen_nats_conf` | 成员变动时重写 NATS conf | 见 1.2 |
| `roster_creds_mirror` | 把成员口令镜像到各中枢（ssh） | `roster_creds_mirror --check`（只读盘点）→ `--apply` |
| `scan_secrets` | 提交前密钥扫描 | `scan_secrets <目录>`，rc=1 禁止提交 |
| `post_group` | 把正文以某 Agent 身份发回人机对话群 | `echo 正文 \| post_group --chat-base http://127.0.0.1:9980 --as hermes --text -`（正文须 @某人 或写 @所有人） |
| `rt_remind` / `shared_content` / `session_ctx_archiver` / `group_watch` / `roundtable` | 协作域运维 CLI | `--help` 查看各自参数 |

---

## 4 故障速查

| 症状 | 原因 | 处置 |
|---|---|---|
| teamd 反复 `conn.retry ... Authorization Violation` | 口令文件内容与 nats.conf 里的不一致 | 重跑 gen_nats_conf 重写 conf |
| teamd 反复 `conn.retry ... failed to lookup address` | `--nats-url` 主机名解析不了 | 换 IP 或修 DNS |
| say 全员「点名超时」但 who 正常 | teamd 没起 / a2a-url 指错 / a2a-token 没配 | 查 teamd 日志 req.error 行与 `/varz` 计数 |
| cli_backend 报 `spawn failed: No such file` | 工作目录不可写（默认 /opt/pi-workspace） | `--pi-workdir` 指可写目录 |
| cli_backend 报 `unknown_model` | agents.json 的 model 不在 `pi/default,claude/default,codex/default` | 改 agents.json 重启 a2a_svc |
| a2a 报 `backend not configured` | agents.json 的 agent 缺 `backend` 字段 | 补 backend/api_key/model |
| chat 报 `mention_required` | Agent 发言没有 @ 任何人 | 正文带 `@所有人` 或真实成员名 |
| blackboard 全部 503 | `BB_PG_DSN` 未设置（降级模式） | 设 env 后重启 |
| who 窗口内收不到心跳 | 心跳 30s 一跳，窗口太短 | `--seconds 32` 以上 |

---

## 5 安全红线（部署时必须遵守）

1. 口令文件 `chmod 600`，只放约定目录；**凭据不进 git、不进日志**（scan_secrets 门禁会拦）。
2. 所有服务默认只绑 loopback；跨机暴露用防火墙/tailnet 收口。
3. peers.json 只存 token 的 sha256，明文 token 只在成员机侧。
4. roster_creds_mirror 走 ssh 传口令镜像；入参出参只有 sha256[:12] 指纹，无明文。
5. 生产替换 Python 版时逐服务切 systemd `ExecStart`，行为异常改回即回退（见 docs/MIGRATION.md）。
