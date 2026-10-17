#!/usr/bin/env bash
# HACS_rust 中枢机一键安装 + 配置 + 启动（幂等：重复执行安全）。
# 前提：本脚本与 bin/ 在同一解包目录内；有 sudo 权限。
# 用法: sudo ./install_hub.sh <成员列表逗号分隔> [中枢IP] [对端中枢IP]
#   例: sudo ./install_hub.sh hermes,lobster 10.4.46.228 10.4.46.228
set -euo pipefail
MEMBERS="${1:?用法: sudo ./install_hub.sh <成员列表> [中枢IP] [对端中枢IP]}"
SELF_IP="${2:-$(hostname -I | awk '{print $1}')}"
PEER_IP="${3:-$SELF_IP}"
DIR="$(cd "$(dirname "$0")" && pwd)"
BIN="$DIR/bin"
PFX=/usr/local/bin
HACS_ETC=/etc/hacs
NATS_DIR=/etc/nats
RUN=/var/lib/hacs/run
LOG=/var/log/hacs
say(){ echo -e "\033[32m[hacs-hub]\033[0m $*"; }
die(){ echo -e "\033[31m[hacs-hub] 失败:\033[0m $*" >&2; exit 1; }

[ "$(id -u)" = 0 ] || die "请用 sudo 运行"
[ -x "$BIN/teamd" ] || die "找不到 $BIN/teamd——请在解包目录内运行"

echo "────────────────────────────────────────────"
echo " HACS_rust 中枢机安装  成员: $MEMBERS"
echo " 本机IP: $SELF_IP   对端: $PEER_IP"
echo "────────────────────────────────────────────"

say "1/8 安装依赖（nats-server, postgresql）"
if command -v apt-get >/dev/null; then
  apt-get install -y nats-server postgresql >/dev/null
elif command -v dnf >/dev/null; then
  dnf install -y nats-server postgresql-server >/dev/null || say "  dnf 无 nats-server，请手动从 nats.io 装二进制"
elif command -v yum >/dev/null; then
  yum install -y nats-server postgresql-server >/dev/null || say "  yum 无 nats-server，请手动从 nats.io 装二进制"
else
  say "  未识别的包管理器——请自行安装 nats-server 与 PostgreSQL"
fi
command -v nats-server >/dev/null || die "nats-server 未装上，请手动安装后重跑"

say "2/8 安装程序到 $PFX"
install -m 755 "$BIN/"* "$PFX/"

say "3/8 初始化 PostgreSQL 库"
if ! id hacs &>/dev/null; then useradd -r -s /usr/sbin/nologin hacs 2>/dev/null || true; fi
if command -v pg_isready >/dev/null; then
  PGV=$(ls /etc/postgresql 2>/dev/null | head -1)
  [ -n "${PGV:-}" ] && { systemctl enable --now postgresql >/dev/null 2>&1 || true
    su postgres -c "psql -tc \"SELECT 1 FROM pg_roles WHERE rolname='hacs'\"" | grep -q 1 || \
      su postgres -c "psql -c \"CREATE ROLE hacs LOGIN PASSWORD 'hacs'\""
    su postgres -c "psql -tc \"SELECT 1 FROM pg_database WHERE datname='chat'\"" | grep -q 1 || \
      su postgres -c "createdb -O hacs chat"
  }
  export CHAT_PG_DSN='postgresql://hacs:hacs@127.0.0.1:5432/chat'
else
  say "  未找到 PostgreSQL——chat/blackboard 将以降级模式运行（可后续补装）"
fi

say "4/8 生成 NATS 配置（成员: $MEMBERS）"
mkdir -p "$NATS_DIR/creds" "$HACS_ETC"
chmod 700 "$NATS_DIR/creds"
[ -f "$NATS_DIR/nats.conf" ] || printf 'port: 4222\nhttp_port: 8222\nserver_name: hub_a\n' > "$NATS_DIR/nats.conf"
for m in ${MEMBERS//,/ }; do
  [ -f "$NATS_DIR/creds/$m.pass" ] || openssl rand -hex 16 > "$NATS_DIR/creds/$m.pass"
  chmod 600 "$NATS_DIR/creds/$m.pass"
done
AGENT_TEAM_ROSTER="$MEMBERS" AGENT_TEAM_NATS_DIR="$NATS_DIR" \
  gen_nats_conf hub_a "$SELF_IP" "$PEER_IP"

say "5/8 生成 a2a 鉴权与 agents.json"
openssl rand -hex 24 > "$HACS_ETC/a2a-token"; chmod 600 "$HACS_ETC/a2a-token"
PEERS="{}"
for m in ${MEMBERS//,/ }; do
  h=$(sha256sum "$HACS_ETC/a2a-token" | cut -d' ' -f1)
  PEERS="{\"$m\": \"$h\"}"
done
echo "$PEERS" > "$HACS_ETC/peers.json"
openssl rand -hex 24 > "$HACS_ETC/cli-backend-key"; chmod 600 "$HACS_ETC/cli-backend-key"
KEY=$(cat "$HACS_ETC/cli-backend-key")
cat > "$HACS_ETC/agents.json" <<EOF
{"node_id":"hub_a","host":"127.0.0.1","port":9920,
 "agents":{"$(echo $MEMBERS | cut -d, -f1)":{"backend":"http://127.0.0.1:9970",
 "api_key":"$KEY","model":"pi/default"}}}
EOF
chmod 600 "$HACS_ETC/agents.json"

say "6/8 写 systemd 服务"
mkdir -p "$RUN" "$LOG"; chown -R hacs:hacs "$RUN" "$LOG" 2>/dev/null || true
if [ -n "${CHAT_PG_DSN:-}" ]; then echo "CHAT_PG_DSN=$CHAT_PG_DSN" > "$HACS_ETC/env"; fi
unit(){ cat > "/etc/systemd/system/$1"; }
wunit(){ local n=$1 d=$2 exec=$3 extra=${4:-}; {
  printf '[Unit]\nDescription=%s\n\n[Service]\n' "$d"
  [ -n "$extra" ] && printf '%s\n' "$extra"
  printf 'ExecStart=%s\nRestart=always\n\n[Install]\nWantedBy=multi-user.target\n' "$exec"
} > "/etc/systemd/system/$n"; }
wunit hacs-nats.service "HACS NATS router" \
  "$(command -v nats-server) -c $NATS_DIR/nats.conf -P $NATS_DIR/nats.pid"
mkdir -p "$RUN/work"; chown -R hacs:hacs "$RUN" 2>/dev/null || true
FIRST=$(echo "$MEMBERS" | cut -d, -f1)
ENVLINE=""; [ -f "$HACS_ETC/env" ] && ENVLINE="EnvironmentFile=$HACS_ETC/env"
wunit hacs-chat.service "HACS chat_svc" "$PFX/chat_svc --port 9980" "$ENVLINE"
wunit hacs-blackboard.service "HACS blackboard_svc" \
  "$PFX/blackboard_svc --port 9940 --peers-json $HACS_ETC/peers.json" "$ENVLINE"
wunit hacs-a2a.service "HACS a2a_svc" \
  "$PFX/a2a_svc --config $HACS_ETC/agents.json --peers $HACS_ETC/peers.json --port 9920"
wunit hacs-clibackend.service "HACS cli_backend" \
  "$PFX/cli_backend --port 9970 --key-file $HACS_ETC/cli-backend-key --stub --pi-workdir $RUN/work"
wunit hacs-teamd.service "HACS teamd" \
  "$PFX/teamd --member $FIRST --user $FIRST --password-file $NATS_DIR/creds/$FIRST.pass --nats-url nats://127.0.0.1:9930 --a2a-url http://127.0.0.1:9920 --a2a-token-file $HACS_ETC/a2a-token --mgmt-port 9960"
systemctl daemon-reload

say "7/8 启动全部服务"
for s in hacs-nats hacs-chat hacs-blackboard hacs-a2a hacs-clibackend hacs-teamd; do
  systemctl enable --now "$s" >/dev/null 2>&1 && say "  $s ✓" || say "  $s ✗ （journalctl -u $s 查看）"
done

say "8/8 验证"
sleep 1; FAIL=0
curl -sf http://127.0.0.1:9932/varz >/dev/null && echo "  NATS        ✓" || { echo "  NATS        ✗"; FAIL=1; }
curl -sf http://127.0.0.1:9980/healthz >/dev/null && echo "  chat_svc    ✓" || { echo "  chat_svc    ✗"; FAIL=1; }
curl -sf http://127.0.0.1:9940/healthz >/dev/null && echo "  blackboard  ✓" || { echo "  blackboard  ✗"; FAIL=1; }
curl -sf http://127.0.0.1:9920/healthz >/dev/null && echo "  a2a_svc     ✓" || { echo "  a2a_svc     ✗"; FAIL=1; }
curl -sf http://127.0.0.1:9960/healthz >/dev/null && echo "  teamd       ✓" || { echo "  teamd       ✗"; FAIL=1; }
echo
[ $FAIL = 0 ] && say "中枢安装完成 ✓  成员凭据在 $NATS_DIR/creds/<成员>.pass（发给对应成员机）" \
             || die "有服务未就绪——按上面提示排查后可重跑本脚本（幂等）"
