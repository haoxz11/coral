#!/usr/bin/env bash
# coral 发版脚本（手动模式：GitHub 仓库不放源码，流程详见 docs/release.md）
#
# 用法：
#   scripts/dist.sh                            # patch 位自动 +1，notes 取 docs/release-notes/v<X.Y.Z>.md
#   scripts/dist.sh 0.2.0                      # 指定版本号（X.Y.Z 或 vX.Y.Z）
#   scripts/dist.sh --notes "多行
#   文本" 0.2.0                                # notes 直接传入（支持多行）
#   scripts/dist.sh --notes-file notes.md      # notes 从文件读取
#
# notes 来源优先级：--notes-file > --notes > docs/release-notes/<版本>.md；
# 三者皆无时报错（不再交互输入）
# 环境变量：PROXY（默认 http://127.0.0.1:7897，置空则不走代理）
#
# 流程：bump 版本 → dist 双平台构建 → 本地验证 → 确认 → 远端打 tag →
#       建 Release（不上传含源码的 source.tar.gz）→ 更新 tap formula →
#       本地提交版本号。发布未完成时自动还原 Cargo.toml/Cargo.lock。
set -euo pipefail

REPO="haoxz11/coral"
TAP_REPO="haoxz11/homebrew-coral"
FORMULA_PATH="Formula/coral-cli.rb"
ZIG_DIR="$HOME/.local/zig"
PROXY="${PROXY:-http://127.0.0.1:7897}"

die() { echo "错误：$*" >&2; exit 1; }

usage() {
  sed -n '2,13p' "$0" | sed 's/^# \{0,1\}//'
  exit 0
}

# ---------- 参数解析 ----------

VERSION=""
NOTES=""
NOTES_FILE=""

while [ $# -gt 0 ]; do
  case "$1" in
    --notes)      [ $# -ge 2 ] || die "--notes 需要参数"; NOTES="$2"; shift 2 ;;
    --notes-file) [ $# -ge 2 ] || die "--notes-file 需要参数"; NOTES_FILE="$2"; shift 2 ;;
    -h|--help)    usage ;;
    -*)           die "未知参数：$1（-h 看用法）" ;;
    *)            [ -z "$VERSION" ] || die "版本号只能有一个：$VERSION 与 $1"
                  VERSION="$1"; shift ;;
  esac
done

cd "$(cd "$(dirname "$0")/.." && pwd)"

# ---------- 前置检查 ----------

command -v dist >/dev/null 2>&1 || die "未安装 cargo-dist（cargo install cargo-dist --locked）"
command -v gh   >/dev/null 2>&1 || die "未安装 gh"
[ -x "$ZIG_DIR/zig" ] || die "未找到 $ZIG_DIR/zig（dist 交叉编译 Linux 依赖）"
gh auth status >/dev/null 2>&1 || die "gh 未登录（记得走代理 gh auth login）"

export PATH="$ZIG_DIR:$PATH"
if [ -n "$PROXY" ]; then
  export https_proxy="$PROXY" http_proxy="$PROXY"
fi

# ---------- 版本号 ----------

V=$(grep -m1 -E '^version = "[0-9]+\.[0-9]+\.[0-9]+"$' Cargo.toml | sed -E 's/^version = "([^"]+)".*/\1/')
[ -n "$V" ] || die "无法从 Cargo.toml 读取当前版本"

if [ -z "$VERSION" ]; then
  major="${V%%.*}"; rest="${V#*.}"; minor="${rest%%.*}"; patch="${rest##*.}"
  VERSION="$major.$minor.$((patch + 1))"
  echo "未指定版本号：$V → ${VERSION}（patch 自动 +1）"
else
  VERSION="${VERSION#v}"   # 容忍 v 前缀
fi
[[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "版本号格式应为 X.Y.Z：$VERSION"
[ "$VERSION" != "$V" ] || die "目标版本 $VERSION 与当前版本相同"

gh api "repos/$REPO/git/ref/tags/v$VERSION" >/dev/null 2>&1 && die "远端已存在 tag v$VERSION"

# ---------- bump 版本号（失败自动还原） ----------

TMP="$(mktemp -d)"
BACKUP_TOML="$TMP/Cargo.toml.bak"
BACKUP_LOCK="$TMP/Cargo.lock.bak"
cp Cargo.toml "$BACKUP_TOML"
[ -f Cargo.lock ] && cp Cargo.lock "$BACKUP_LOCK"

RELEASE_CREATED=0
on_exit() {
  if [ "$RELEASE_CREATED" -eq 0 ] && [ -f "$BACKUP_TOML" ]; then
    cp "$BACKUP_TOML" Cargo.toml
    [ -f "$BACKUP_LOCK" ] && cp "$BACKUP_LOCK" Cargo.lock
    echo "（发布未完成，已还原 Cargo.toml/Cargo.lock 的版本改动）" >&2
  fi
  rm -rf "$TMP"
}
trap on_exit EXIT

perl -pi -e "if (!\$done && /^version = \"[^\"]+\"/) { s/version = \"[^\"]+\"/version = \"$VERSION\"/; \$done = 1 }" Cargo.toml
grep -q "^version = \"$VERSION\"$" Cargo.toml || die "Cargo.toml 版本号写入失败"

# ---------- notes ----------
# 优先级：--notes-file > --notes > docs/release-notes/<版本>.md；皆无则报错

if [ -n "$NOTES_FILE" ]; then
  [ -f "$NOTES_FILE" ] || die "notes 文件不存在：$NOTES_FILE"
  NOTES="$(cat "$NOTES_FILE")"
elif [ -z "$NOTES" ]; then
  # 版本文件命名与 Release tag 一致：vX.Y.Z.md
  DEFAULT_NOTES="docs/release-notes/v$VERSION.md"
  if [ -f "$DEFAULT_NOTES" ]; then
    echo "未传 --notes/--notes-file，使用 $DEFAULT_NOTES 作为 notes"
    NOTES="$(cat "$DEFAULT_NOTES")"
  fi
fi
[ -n "$NOTES" ] || die "缺少 release notes：--notes / --notes-file 二选一必填，或提供 docs/release-notes/v$VERSION.md"

# ---------- 构建 + 本地验证 ----------

echo "==> dist build（双 target，约 1 分钟）"
dist build --target aarch64-apple-darwin --target x86_64-unknown-linux-gnu >/dev/null

DIST_DIR="target/distrib"
for f in coral-cli.rb coral-cli-installer.sh \
         coral-cli-aarch64-apple-darwin.tar.xz \
         coral-cli-x86_64-unknown-linux-gnu.tar.xz; do
  [ -f "$DIST_DIR/$f" ] || die "构建产物缺失：$DIST_DIR/$f"
done

tar -xJf "$DIST_DIR/coral-cli-aarch64-apple-darwin.tar.xz" -C "$TMP"
got="$("$TMP/coral-cli-aarch64-apple-darwin/coral" -v)"
[ "$got" = "coral $VERSION" ] || die "二进制版本校验失败：期望 coral $VERSION，实际 $got"

# ---------- 确认 ----------

echo
echo "================ 发布摘要 ================"
echo "版本：  $V → $VERSION"
echo "标题：  coral $VERSION"
echo "仓库：  $REPO / tap: $TAP_REPO"
echo "notes："
echo "$NOTES" | sed 's/^/  /'
echo "========================================="
read -r -p "确认发布？[y/N] " ans
case "$ans" in
  y|Y|yes|YES) ;;
  *) echo "已取消"; exit 0 ;;
esac

# ---------- 远端发布 ----------

echo "==> 打 tag v$VERSION"
head_sha="$(gh api "repos/$REPO/branches/main" --jq '.commit.sha')"
gh api -X POST "repos/$REPO/git/refs" -f "ref=refs/tags/v$VERSION" -f "sha=$head_sha" >/dev/null

echo "==> 创建 Release 并上传产物（不含 source.tar.gz）"
gh release create "v$VERSION" \
  "$DIST_DIR/coral-cli-aarch64-apple-darwin.tar.xz" \
  "$DIST_DIR/coral-cli-x86_64-unknown-linux-gnu.tar.xz" \
  "$DIST_DIR/coral-cli-installer.sh" \
  --repo "$REPO" --title "coral $VERSION" --notes "$NOTES"
RELEASE_CREATED=1

echo "==> 更新 tap formula"
content="$(base64 -i "$DIST_DIR/coral-cli.rb" | tr -d '\n')"
put=(-X PUT "repos/$TAP_REPO/contents/$FORMULA_PATH" -f "message=coral-cli $VERSION" -f "content=$content")
sha="$(gh api "repos/$TAP_REPO/contents/$FORMULA_PATH" --jq '.sha' 2>/dev/null || true)"
[ -n "$sha" ] && put+=(-f "sha=$sha")
gh api "${put[@]}" >/dev/null

# ---------- 本地提交版本号 ----------

git add Cargo.toml
[ -f Cargo.lock ] && git add Cargo.lock
git commit -m "chore: bump version to $VERSION" -- Cargo.toml $( [ -f Cargo.lock ] && echo Cargo.lock )

echo
echo "✓ 发布完成：https://github.com/$REPO/releases/tag/v$VERSION"
echo "  验证：brew update && brew upgrade coral-cli && coral -v"
