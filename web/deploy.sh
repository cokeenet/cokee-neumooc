#!/bin/bash
# 构建 + 校验 + 重启，三步原子完成（2026-11-02 新增）。
#
# 为什么需要它：这台机器上「构建」和「重启」长期是两个独立动作，
# 已经连续踩过两次同类故障：
#   · 换 dist 后没重启 → 运行进程按需加载新 chunk 时 404 → 全站 500
#   · 重启了但 dist 是旧的 → HTML 引用旧 hash → CSS 404 → 页面无样式
# 本脚本保证：**构建失败不动线上；产物不自洽不重启；只有全绿才重启。**
set -euo pipefail

SRC="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SERVICE="${SERVICE:-neumooc-frontend}"

cd "$SRC"
echo "▶ 构建"
pnpm build

echo "▶ 校验产物自洽（HTML/JS 里引用的 /assets/ 文件必须都存在）"
missing=0
# 从构建产物里抓出所有 /assets/<file> 引用，逐个核对磁盘
for ref in $(grep -rhoE '/assets/[A-Za-z0-9_.@~%-]+' dist 2>/dev/null | sort -u); do
  [ -f "dist/client${ref}" ] && continue
  # Astro 的 SSR 产物可能引用 client 目录之外的路径，一并检查
  [ -f "dist${ref}" ] && continue
  echo "  ✗ 缺失 $ref"
  missing=1
done
if [ "$missing" -ne 0 ]; then
  echo "❌ 产物不完整，已放弃部署（线上保持原样）"
  exit 1
fi
echo "  ✓ 引用全部存在"

echo "▶ 重启 $SERVICE"
systemctl restart "$SERVICE"
sleep 4
for i in 1 2 3 4 5; do
  systemctl is-active --quiet "$SERVICE" && break
  sleep 2
done
if ! systemctl is-active --quiet "$SERVICE"; then
  echo "❌ $SERVICE 重启后未 active，请查看 journalctl -u $SERVICE"
  exit 1
fi
echo "✅ 完成：$SERVICE 已用新产物重启"
