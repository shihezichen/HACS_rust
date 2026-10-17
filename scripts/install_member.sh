#!/usr/bin/env bash
# HACS_rust 成员机一键安装（傻瓜化）。
# 用法: sudo ./install_member.sh <成员名> <中枢IP> [成员凭据文件]
#   例: sudo ./install_member.sh lobster 10.4.46.228 ./lobster.pass
#   凭据文件不给时，安装后运行 team 会提示放置路径。
set -euo pipefail
MEMBER="${1:?用法: sudo ./install_member.sh <成员名> <中枢IP> [凭据.pass]}"
HUB="${2:?缺少中枢IP}"
PASS_SRC="${3:-}"
PFX=/usr/local/bin
DIR="$(cd "$(dirname "$0")" && pwd)"
say(){ echo -e "\033[32m[hacs-member]\033[0m $*"; }
die(){ echo -e "\033[31m[hacs-member] 失败:\033[0m $*" >&2; exit 1; }

[ "$(id -u)" = 0 ] || die "请用 sudo 运行"
[ -x "$DIR/bin/team" ] || die "找不到 $DIR/bin/team——请在解包目录内运行"

say "安装 team CLI 到 $PFX"
install -m 755 "$DIR/bin/team" "$PFX/"

say "写环境配置 /etc/hacs/member.env"
mkdir -p /etc/hacs
cat > /etc/hacs/member.env <<EOF
AGENT_MEMBER_NAME=$MEMBER
AGENT_TEAM_NATS_HOST=$HUB
AGENT_TEAM_NATS_PORT=9930
EOF
chmod 600 /etc/hacs/member.env

if [ -n "$PASS_SRC" ]; then
  install -m 600 "$PASS_SRC" "/etc/hacs/$MEMBER.pass"
  say "凭据已放置 /etc/hacs/$MEMBER.pass"
else
  say "请把中枢发来的 $MEMBER.pass 放到 /etc/hacs/$MEMBER.pass（chmod 600）"
fi

say "验证入网（心跳窗口 35s，耐心等一下）"
if [ -f "/etc/hacs/$MEMBER.pass" ]; then
  if team who --as "$MEMBER" --password-file "/etc/hacs/$MEMBER.pass" \
       --nats-host "$HUB" --nats-port 9930 --seconds 35; then
    say "✓ $MEMBER 已入网。试一句: team say --as $MEMBER --to <对方> --text '上线了'"
  else
    die "who 未收到心跳——查中枢 teamd 是否在跑 / 凭据是否与中枢一致"
  fi
else
  say "凭据未就位，跳过验证。放入凭据后运行:"
  echo "  team who --as $MEMBER --password-file /etc/hacs/$MEMBER.pass --nats-host $HUB --nats-port 9930 --seconds 35"
fi
