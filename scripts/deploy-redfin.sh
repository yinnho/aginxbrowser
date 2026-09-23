#!/usr/bin/env bash
# deploy-redfin.sh — 构建 musl 面板二进制 + 模板，推到 redfin 样机。
#
# 官方 musl 资产在本机不能用（V8 snapshot 架构错配，v0.2.10 教训），
# 设备件一律本树 zigbuild。落位守三律（换在跑 binary）：
#   ① staging 与真身同 fs（/var/bin/.new，rename 原子换，不跨 fs mv）
#   ② 落位后 md5 核对
#   ③ 杀进程用 pidof（pkill -f 会匹配 adb/ssh 整条命令行）
# svcd 的 aginxbrowser 单元会自动把杀掉的进程拉回来。
set -euo pipefail

cd "$(dirname "$0")/.."

DEST="${1:-root@192.168.3.93}"
BIN=/var/bin/aginxbrowser
TPL=/var/lib/aginxbrowser/templates

echo "==> zigbuild aarch64-unknown-linux-musl (features screenshot)"
# /health 的 commit 字段是编译期盖章（option_env!），不设就恒 "unknown"——
# 设备上没法分辨烧的是哪个版本（2026-09-23 aginxos 线验证门踩过）。
export AGINXBROWSER_BUILD_COMMIT="$(git rev-parse --short HEAD)"
# option_env! 只在 bin crate 重编时才重读——cargo 不跟踪 env 变化，同树
# 重复部署会带着旧 stamp（86quan runbook 2026-09-11 同款坑），touch 强制重烘。
touch src/main.rs
CARGO_INCREMENTAL=0 cargo zigbuild --release --features screenshot \
  --target aarch64-unknown-linux-musl
OUT="target/aarch64-unknown-linux-musl/release/aginxbrowser"
ls -lh "${OUT}"
SUM_LOCAL="$(md5 -q "${OUT}" 2>/dev/null || md5sum "${OUT}" | cut -d' ' -f1)"
echo "local  md5 ${SUM_LOCAL}"

echo "==> push binary"
scp -q "${OUT}" "${DEST}:${BIN}.new"
ssh "${DEST}" "set -e
  test \"\$(md5sum ${BIN}.new | cut -d' ' -f1)\" = '${SUM_LOCAL}' || { echo 'md5 mismatch on device'; exit 1; }
  mv ${BIN}.new ${BIN}
  echo device md5 \$(md5sum ${BIN} | cut -d' ' -f1)
"

echo "==> push templates (browser-owned dir, registry included)"
ssh "${DEST}" "mkdir -p ${TPL}"
scp -q templates/registry.json templates/*.html "${DEST}:${TPL}/"

echo "==> restart the browser (svcd picks it back up)"
ssh "${DEST}" "pid=\$(pidof aginxbrowser) && kill \${pid} && echo \"killed \${pid}\" || echo 'not running'"

echo "done. panel waits for /run/aginxbrowser/show.html to take the screen."
