#!/usr/bin/env bash
# HACS_rust 发布打包：按角色拆分包（hub / member / windows-member），附 SHA256SUMS。
# 用法: ./scripts/package_release.sh <版本号> [二进制根目录]
#   二进制根目录需含 target/x86_64-unknown-linux-musl/release/ 与 target/x86_64-pc-windows-gnu/release/
set -euo pipefail
V="${1:?用法: package_release.sh <版本号> [根目录]}"
ROOT="${2:-$HOME/HACS_rust}"
MUSL="$ROOT/target/x86_64-unknown-linux-musl/release"
GNU="$ROOT/target/x86_64-pc-windows-gnu/release"
OUT="/tmp/rel_$V"
rm -rf "$OUT"; mkdir -p "$OUT"

HUB_BINS="a2a_svc teamd cli_backend chat_svc blackboard_svc blackboard_view task_svc serve gen_nats_conf nodelet roster_creds_mirror post_group roundtable rt_remind shared_content session_ctx_archiver group_watch a2a_call team_call team_broadcast team_group team_roster team"
MEMBER_BINS="team"

mk() { # $1=包名  $2=bin列表  $3=来源目录  $4=后缀
  local name=$1 list=$2 src=$3 suffix=$4
  mkdir -p "$OUT/hacs-rust-$V-$name/bin"
  for b in $list; do cp "$src/$b$suffix" "$OUT/hacs-rust-$V-$name/bin/"; done
  cp "$ROOT/docs/USER_GUIDE.md" "$OUT/hacs-rust-$V-$name/"
  cp "$ROOT/scripts/install_hub.sh" "$ROOT/scripts/install_member.sh" \
     "$ROOT/scripts/deploy_members.sh" "$OUT/hacs-rust-$V-$name/" 2>/dev/null || true
  tar czf "$OUT/hacs-rust-$V-$name.tar.gz" -C "$OUT" "hacs-rust-$V-$name"
  echo "✓ hacs-rust-$V-$name.tar.gz"
}

mk "linux-hub"     "$HUB_BINS"    "$MUSL" ""
mk "linux-member"  "$MEMBER_BINS" "$MUSL" ""
mk "windows-hub"   "$HUB_BINS"    "$GNU"  ".exe"
mk "windows-member" "$MEMBER_BINS" "$GNU"  ".exe"

( cd "$OUT" && sha256sum hacs-rust-$V-*.tar.gz > SHA256SUMS.txt )
echo "── 完成: $OUT"; ls -lh "$OUT"/*.tar.gz
