#!/usr/bin/env bash
# coral target 目录瘦身脚本（不影响增量编译，详见 docs 目标目录梳理）
#
# 原理：只删"按时间已过期"的产物——
#   1. cargo sweep -t 删 deps/build 里超过 N 天的旧 hash 产物（当前有效的依赖 rlib 全保留）
#   2. 手动删 debug/incremental 里超过 N 天的旧增量会话（cargo sweep 不覆盖这里）
# 当天/近期的活跃缓存不动，下次 cargo check/test 仍走增量。
#
# 用法：
#   scripts/clear.sh              # 清理 7 天前的产物（默认）
#   scripts/clear.sh 3            # 清理 3 天前的产物
#   scripts/clear.sh --dry-run 3  # 只预览将释放多少空间，不删除
#
# 依赖 cargo-sweep（没有会提示安装命令）。
set -euo pipefail

die() { echo "错误：$*" >&2; exit 1; }

usage() {
  sed -n '2,13p' "$0" | sed 's/^# \{0,1\}//'
  exit 0
}

DAYS=7
DRY_RUN=0
while [ $# -gt 0 ]; do
  case "$1" in
    --dry-run|-n) DRY_RUN=1; shift ;;
    -h|--help)    usage ;;
    -*)           die "未知参数：$1（-h 看用法）" ;;
    *)            [[ "$1" =~ ^[0-9]+$ ]] || die "天数应为正整数：$1"
                  DAYS="$1"; shift ;;
  esac
done

cd "$(cd "$(dirname "$0")/.." && pwd)"
TARGET_DIR="target"
[ -d "$TARGET_DIR" ] || die "target 目录不存在，无需清理"

command -v cargo-sweep >/dev/null 2>&1 \
  || die "未安装 cargo-sweep（cargo install cargo-sweep --locked）"

size_before="$(du -sk "$TARGET_DIR" | cut -f1)"

# ---------- 1. deps/build 旧产物（cargo-sweep 按指纹+时间清理） ----------

if [ "$DRY_RUN" -eq 1 ]; then
  echo "==> [dry-run] cargo sweep -t $DAYS"
  cargo sweep -t "$DAYS" --dry-run
else
  echo "==> cargo sweep -t $DAYS"
  cargo sweep -t "$DAYS"
fi

# ---------- 2. debug/incremental 旧增量会话 ----------
# 会话目录名是 <crate>-<随机后缀>，只按目录 mtime 判断新旧；
# cargo 对缺失的旧会话会自动降级为该 crate 重编一次，不影响其他 crate。

INC_DIR="$TARGET_DIR/debug/incremental"
if [ -d "$INC_DIR" ]; then
  if [ "$DRY_RUN" -eq 1 ]; then
    n=$(find "$INC_DIR" -maxdepth 1 -mindepth 1 -type d ! -mtime -"$DAYS" | wc -l | tr -d ' ')
    s=$(find "$INC_DIR" -maxdepth 1 -mindepth 1 -type d ! -mtime -"$DAYS" -print0 \
        | xargs -0 du -sk 2>/dev/null | awk '{t+=$1} END {printf "%.1fG", t/1024/1024}')
    echo "==> [dry-run] 将删除 incremental 旧会话：$n 个，$s"
  else
    echo "==> 清理 incremental 中 ${DAYS} 天前的会话"
    find "$INC_DIR" -maxdepth 1 -mindepth 1 -type d ! -mtime -"$DAYS" -exec rm -rf {} +
  fi
fi

# ---------- 汇总 ----------

size_after="$(du -sk "$TARGET_DIR" | cut -f1)"
freed=$(( (size_before - size_after) / 1024 ))
if [ "$DRY_RUN" -eq 1 ]; then
  echo "（dry-run，未删除任何文件）"
else
  echo "✓ 清理完成：释放 ${freed}M，target 现占 $((size_after / 1024))M"
fi
