#!/usr/bin/env bash
# HACS_rust 批量部署成员机：从这台中枢（或任一跳板机）把成员包+凭据推到多台机器并执行安装。
# 前提：已配置 SSH 免密（ssh-copy-id <user>@<host>）。
# 用法: ./deploy_members.sh <成员包tar.gz> <成员列表文件>
#   成员列表文件每行一条:  <成员名> <user>@<host> [中枢IP]
#   例:  lobster  ubuntu@10.4.46.230  10.4.46.228
#        top001   root@10.4.46.231
set -euo pipefail
PKG="${1:?用法: deploy_members.sh <hacs-rust-*member*.tar.gz> <成员列表文件>}"
LIST="${2:?缺少成员列表文件}"
[ -f "$PKG" ] || { echo "包不存在: $PKG"; exit 2; }
[ -f "$LIST" ] || { echo "列表不存在: $LIST"; exit 2; }
say(){ echo -e "\033[32m[deploy]\033[0m $*"; }
FAIL=0
while read -r MEMBER TARGET HUB; do
  [ -z "${MEMBER// /}" ] && continue
  say "→ $MEMBER @ $TARGET"
  if ! ssh -o ConnectTimeout=8 -o BatchMode=yes "$TARGET" true 2>/dev/null; then
    say "  ✗ SSH 连不上 $TARGET（配免密: ssh-copy-id $TARGET）"; FAIL=1; continue
  fi
  scp -q "$PKG" "$TARGET:/tmp/hacs-member.tar.gz" || { say "  ✗ 传包失败"; FAIL=1; continue; }
  ssh "$TARGET" "cd /tmp && rm -rf hacs-member && mkdir hacs-member && \
    tar xzf hacs-member.tar.gz -C hacs-member --strip-components=1 && \
    sudo ./hacs-member/install_member.sh $MEMBER ${HUB:-} /tmp/hacs-member/$MEMBER.pass; \
    rm -rf /tmp/hacs-member /tmp/hacs-member.tar.gz" \
    && say "  ✓ $MEMBER 部署完成" || { say "  ✗ $MEMBER 安装失败"; FAIL=1; }
done < "$LIST"
[ $FAIL = 0 ] && say "全部成员机部署完成 ✓" || { say "部分失败，见上"; exit 1; }
