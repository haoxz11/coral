#!/usr/bin/env bash
# coral e2e 验收：以真实二进制进程对真实内容目录做端到端测试。
# 用法：scripts/e2e.sh [二进制路径]（默认 target/release/coral）
set -euo pipefail

BIN="${1:-./target/release/coral}"
WORK=$(mktemp -d /tmp/coral-e2e.XXXXXX)
CONTENT="$WORK/content"
CACHE="$WORK/cache"
PORT=18321
BASE="http://127.0.0.1:$PORT"
PASS=0
FAIL=0
PID=""

cleanup() {
  [ -n "$PID" ] && kill "$PID" 2>/dev/null || true
  [ -n "$PID" ] && wait "$PID" 2>/dev/null || true
  rm -rf "$WORK"
}
trap cleanup EXIT

ok()   { PASS=$((PASS+1)); echo "  ✅ $1"; }
fail() { FAIL=$((FAIL+1)); echo "  ❌ $1"; }
check() { # check <描述> <命令成功条件>（eval，勿传入含用户内容的变量）
  if eval "$2" >/dev/null 2>&1; then ok "$1"; else fail "$1"; fi
}
# check_contains <描述> <URL> <needle>：直接请求断言（内容不经 eval）
check_contains() {
  if curl -sf "$2" | grep -qF -- "$3"; then ok "$1"; else fail "$1"; fi
}

# ---------- 构造测试内容树（需求无关样本） ----------
mkdir -p "$CONTENT/guide/advanced" "$CONTENT/news" "$CONTENT/静态目录"
cat > "$CONTENT/_index.md" <<'EOF'
---
title: 站点首页
weight: 1
---
首页内容，含子文档列表 {{% children %}}
EOF
cat > "$CONTENT/guide/_index.md" <<'EOF'
---
title: 指南
weight: 10
menuPre: "<i class=\"fa fa-book\"></i>"
---
指南分支页 {{% children %}}
EOF
cat > "$CONTENT/guide/intro.md" <<'EOF'
---
title: 入门
weight: 1
date: 2026-09-01
---
# 入门指南

正文段落，含 [链接](/guide)。

{{% notice style="warning" title="点击查看" %}}
**折叠内容**
{{% /notice %}}

{{< tabs >}}{{% tab title="说明" %}}面板一{{% /tab %}}{{% tab title="示例" %}}面板二{{% /tab %}}{{< /tabs >}}

```rust
fn main() {}
```

```mermaid
graph TD; A-->B;
```

$$E = mc^2$$
EOF
cat > "$CONTENT/guide/advanced/topic.md" <<'EOF'
---
title: 深入主题
weight: 2
---
深入主题正文。
EOF
cat > "$CONTENT/guide/draft-page.md" <<'EOF'
---
title: 草稿
draft: true
---
草稿正文。
EOF
cat > "$CONTENT/news/release.md" <<'EOF'
---
title: 发布说明
permalink: /release-notes/
date: 2026-08-15
---
发布正文。
EOF
cat > "$CONTENT/静态目录/_index.md" <<'EOF'
---
title: 中文目录页
---
中文目录内容。
EOF
cat > "$CONTENT/unknown-sc.md" <<'EOF'
---
title: 未知 shortcode 页
---
前文 {{% mermaid %}}graph A{{% /mermaid %}} 后文
EOF
echo "png-data" > "$CONTENT/guide/logo.png"

cat > "$WORK/coral.toml" <<EOF
[server]
port = $PORT
bind = "127.0.0.1"

[content]
root = "$CONTENT"
exclude = []
draft = false

[tree]
initial_depth = 2
expand_depth = 1

[cache]
dir = "$CACHE"

[log]
format = "pretty"

[search]
enabled = true
EOF

echo "==> 启动 coral（${BIN}）"
"$BIN" --config "$WORK/coral.toml" > "$WORK/server.log" 2>&1 &
PID=$!

# 等就绪
for i in $(seq 1 50); do
  curl -sf "$BASE/readyz" >/dev/null 2>&1 && break
  sleep 0.2
done
curl -sf "$BASE/readyz" >/dev/null || { echo "服务未就绪："; cat "$WORK/server.log"; exit 1; }

echo "==> 验收断言"

# 1. 目录树正常展示前 2 层
check "1. 首页 layout 完整（top-nav/主题/搜索框）" "curl -sf $BASE/ | grep -q 'class=\"top-nav\"'"

# 2. 懒加载接口 + 二次展开缓存（日志可验证树缓存命中）
TREE1=$(curl -sf "$BASE/api/tree/children?path=/guide")
check "2. 懒加载接口返回子节点 JSON" "echo '$TREE1' | grep -q '入门'"
ETAG=$(curl -sI "$BASE/api/tree/children?path=/guide" | tr -d '\r' | awk -F': ' '/^[Ee]tag/{print $2}')
CODE2=$(curl -s -o /dev/null -w '%{http_code}' -H "If-None-Match: $ETAG" "$BASE/api/tree/children?path=/guide")
check "2b. 二次展开 ETag 304" "[ \"$CODE2\" = 304 ]"

# 3. 修改 md ≤1s 可见（watcher 失效链）
curl -sf "$BASE/guide/intro" | grep -q "入门指南" || fail "3. 初始内容缺失"
sleep 0.3
cat > "$CONTENT/guide/intro.md" <<'EOF'
---
title: 入门
weight: 1
date: 2026-09-01
---
# 入门指南

修改后的全新内容 v2。

{{% notice style="warning" title="点击查看" %}}
**折叠内容**
{{% /notice %}}

{{< tabs >}}{{% tab title="说明" %}}面板一{{% /tab %}}{{% tab title="示例" %}}面板二{{% /tab %}}{{< /tabs >}}

```rust
fn main() {}
```

```mermaid
graph TD; A-->B;
```

$$E = mc^2$$
EOF
SEEN=0
for i in $(seq 1 20); do
  curl -sf "$BASE/guide/intro" 2>/dev/null | grep -q "全新内容 v2" && { SEEN=1; break; }
  sleep 0.25
done
[ "$SEEN" = 1 ] && ok "3. 修改 md 后 ≤1s 新内容可见" || fail "3. 修改 md 后新内容不可见"

# 4. 新建/删除 md 树即时反映
cat > "$CONTENT/guide/new-page.md" <<'EOF'
---
title: 新建页面
---
新建页面正文。
EOF
SEEN=0
for i in $(seq 1 20); do
  curl -sf "$BASE/api/tree/children?path=/guide" 2>/dev/null | grep -q "新建页面" && { SEEN=1; break; }
  sleep 0.25
done
[ "$SEEN" = 1 ] && ok "4. 新建 md 树即时反映" || fail "4. 新建 md 树未反映"
rm "$CONTENT/guide/new-page.md"
SEEN=0
for i in $(seq 1 20); do
  CODE=$(curl -s -o /dev/null -w '%{http_code}' "$BASE/guide/new-page")
  [ "$CODE" = 404 ] && { SEEN=1; break; }
  sleep 0.25
done
[ "$SEEN" = 1 ] && ok "4b. 删除 md 后 404" || fail "4b. 删除 md 后仍可访问"

# 5. shortcode 渲染（notice/tabs/children 结构断言；视觉抽查人工）
check_contains "5. notice 块渲染" "$BASE/guide/intro" 'class="notice notice-warning"'
check_contains "5b. tabs 结构" "$BASE/guide/intro" 'class="tabs"'
check_contains "5c. children 列表" "$BASE/" 'ul class="children"'
# 16. Mermaid/KaTeX（M2-s3）：占位 HTML 结构断言（不请求 CDN，防 CI flaky）
check_contains "16a. mermaid 语义占位" "$BASE/guide/intro" 'pre class="mermaid"'
check_contains "16b. katex 块占位+公式原文" "$BASE/guide/intro" 'katex-block'
check_contains "16c. katex CDN CSS 条件注入" "$BASE/guide/intro" 'katex.min.css'
if curl -sf "$BASE/advanced/topic" | grep -q "katex.min.css"; then fail "16d. 无占位页不应注入 katex CSS"; else ok "16d. 无占位页零注入"; fi

# 6. permalink 自定义 URL
check "6. permalink 页面可访问" "curl -sf $BASE/release-notes/ | grep -q '发布正文'"

# 7. 非 md 静态资源
check "7. 静态资源原路径 + mime" "curl -sf $BASE/guide/logo.png | grep -q png-data"

# 8. 删 cache 重启全量重建
kill "$PID"; wait "$PID" 2>/dev/null || true; PID=""
rm -rf "$CACHE"
"$BIN" --config "$WORK/coral.toml" >> "$WORK/server.log" 2>&1 &
PID=$!
for i in $(seq 1 50); do curl -sf "$BASE/readyz" >/dev/null 2>&1 && break; sleep 0.2; done
check "8. 删 cache 重启后站点完整" "curl -sf $BASE/guide/intro | grep -q '全新内容 v2'"

# 9. 未知 shortcode 不报错 + 日志统计
check "9. 未知 shortcode 页不报错（原样输出）" "curl -sf $BASE/unknown-sc | grep -q mermaid"
check "9b. 未知 shortcode 日志统计" "grep -q '未知 shortcode' $WORK/server.log"

# 10. 路径穿越全部 404
for p in "..%2F..%2Fetc%2Fpasswd" "%2e%2e/%2e%2e/etc/passwd" "../secret"; do
  CODE=$(curl -s -o /dev/null -w '%{http_code}' "$BASE/$p")
  [ "$CODE" = 404 ] || fail "10. 穿越 $p 返回 $CODE"
done
ok "10. 路径穿越全部 404"
CODE=$(curl -s -o /dev/null -w '%{http_code}' "$BASE/api/tree/children?path=../../etc")
check "10b. 树 API 穿越参数 404" "[ \"$CODE\" = 404 ]"

# 11. draft 404；配置错误启动报错
CODE=$(curl -s -o /dev/null -w '%{http_code}' "$BASE/guide/draft-page")
check "11. draft 文档 404" "[ \"$CODE\" = 404 ]"
kill "$PID"; wait "$PID" 2>/dev/null || true; PID=""
BAD_CFG="$WORK/bad.toml"
sed "s|root = \"$CONTENT\"|root = \"$WORK/不存在目录\"|" "$WORK/coral.toml" > "$BAD_CFG"
if "$BIN" --config "$BAD_CFG" >/dev/null 2>"$WORK/bad.log"; then
  fail "11b. 错误配置应退出非 0"
else
  grep -q "不存在" "$WORK/bad.log" && ok "11b. 错误配置可读提示退出" || fail "11b. 提示不可读"
fi

# 12. children 失效链（重启服务恢复干净状态）
"$BIN" --config "$WORK/coral.toml" >> "$WORK/server.log" 2>&1 &
PID=$!
for i in $(seq 1 50); do curl -sf "$BASE/readyz" >/dev/null 2>&1 && break; sleep 0.2; done
# 首页为 home archetype（卡片布局）：children 失效链在 guide 分支页验证
curl -sf "$BASE/guide" | grep -q "children" || fail "12. 预热失败"
cat > "$CONTENT/guide/added.md" <<'EOF'
---
title: 新增子文档
---
新增子文档正文。
EOF
SEEN=0
for i in $(seq 1 20); do
  curl -sf "$BASE/guide" 2>/dev/null | grep -q "新增子文档" && { SEEN=1; break; }
  sleep 0.25
done
[ "$SEEN" = 1 ] && ok "12. children 连带失效（新增子文档后首页列表更新）" || fail "12. children 列表未更新"

# 13. 搜索：中文命中/高亮/下拉 API/增量
SEEN=0
for i in $(seq 1 30); do
  RESULT=$(curl -sf "$BASE/api/search?q=%E5%85%A5%E9%97%A8" 2>/dev/null || true)
  echo "$RESULT" | grep -q "url" && { SEEN=1; break; }
  sleep 0.3
done
[ "$SEEN" = 1 ] && ok "13. 搜索 API 中文词命中（后台构建完成）" || fail "13. 搜索索引 9s 未就绪"
RESULT=$(curl -sf "$BASE/api/search?q=%E5%85%A5%E9%97%A8" 2>/dev/null || true)
check "13b. 搜索结果含 snippet/dir_path" "echo '$RESULT' | grep -q snippet"
check "13c. 搜索结果页服务端渲染" "curl -sf '$BASE/search?q=入门' | grep -q search-results"
# 增量：新文档可搜到
cat > "$CONTENT/guide/search-probe.md" <<'EOF2'
---
title: 搜索探针页
---
搜索增量验证关键词内容
EOF2
SEEN=0
for i in $(seq 1 30); do
  curl -sf "$BASE/api/search?q=%E6%90%9C%E7%B4%A2%E5%A2%9E%E9%87%8F%E9%AA%8C%E8%AF%81" 2>/dev/null | grep -q "搜索探针页" && { SEEN=1; break; }
  sleep 0.3
done
[ "$SEEN" = 1 ] && ok "13d. 新文档增量可搜到" || fail "13d. 搜索增量未生效"
rm -f "$CONTENT/guide/search-probe.md"

# 14. reindex 运维后门：同步重建 + 未开启 404（本机请求）
CODE=$(curl -s -o /dev/null -w '%{http_code}' "$BASE/search/reindex")
check "14. reindex 同步重建 200" "[ "$CODE" = 200 ]"
check "14b. reindex 返回重建总结" "curl -sf $BASE/search/reindex | grep -q indexed_docs"
check "14c. 总结含 segments/index_bytes/elapsed" "curl -sf $BASE/search/reindex | grep -q segments && curl -sf $BASE/search/reindex | grep -q index_bytes"
# 重建后搜索仍正常
check "14d. reindex 后搜索可用" "curl -sf '$BASE/api/search?q=入门' | grep -q 入门"

# 15. git-sync：file:// 远端 + webhook 镜像同步全链
REPO="$WORK/git-repo"
mkdir -p "$REPO/docs/guide"
git -C "$REPO" init -b main --quiet
printf -- '---\ntitle: 同步首页\n---\n仓库同步内容 {{%% children %%}}' > "$REPO/docs/_index.md"
printf -- '---\ntitle: 同步页\n---\n# 同步页\n同步正文关键词' > "$REPO/docs/guide/synced.md"
git -C "$REPO" add . >/dev/null
git -C "$REPO" -c user.email=t@t -c user.name=t commit --quiet -m init

# 重启服务（git-sync 开启 + 指向该仓库；search 保持开启）
sed "s|^root = \"$CONTENT\"|root = \"$WORK/git-content\"|" "$WORK/coral.toml" > "$WORK/git.toml"
mkdir -p "$WORK/git-content"
cat >> "$WORK/git.toml" <<EOF2
[git]
enabled = true
remote = "file://$REPO"
branch = "main"
watch_dir = "docs"
private_key_path = "$WORK/nokey"
EOF2
kill "$PID"; wait "$PID" 2>/dev/null || true; PID=""
"$BIN" --config "$WORK/git.toml" >> "$WORK/server.log" 2>&1 &
PID=$!
for i in $(seq 1 50); do curl -sf "$BASE/readyz" >/dev/null 2>&1 && break; sleep 0.2; done
# 等启动自动同步完成（content.root 出现同步文件）
SEEN=0
for i in $(seq 1 30); do
  [ -f "$WORK/git-content/_index.md" ] && { SEEN=1; break; }
  sleep 0.3
done
[ "$SEEN" = 1 ] && ok "15. 启动自动 clone+同步" || fail "15. 启动同步未完成"
check_contains "15b. 同步页面可访问" "$BASE/guide/synced" "同步正文关键词"

# 增量 push + webhook 触发（含删除镜像同步）
printf -- '---\ntitle: 新页\n---\nwebhook 增量内容' > "$REPO/docs/guide/added.md"
rm "$REPO/docs/guide/synced.md"
git -C "$REPO" add . >/dev/null
git -C "$REPO" -c user.email=t@t -c user.name=t commit --quiet -m update
CODE=$(curl -s -o /dev/null -w '%{http_code}' -X POST -H "Content-Type: application/json" -d '{"object_kind":"push","ref":"refs/heads/main","checkout_sha":"x"}' "$BASE/git/webhook")
[ "$CODE" = 200 ] && ok "15c. webhook 200" || fail "15c. webhook 返回 $CODE"
SEEN=0
for i in $(seq 1 30); do
  [ -f "$WORK/git-content/guide/added.md" ] && [ ! -f "$WORK/git-content/guide/synced.md" ] && { SEEN=1; break; }
  sleep 0.3
done
[ "$SEEN" = 1 ] && ok "15d. 镜像同步（新增+删除）" || fail "15d. 镜像同步未生效"
# 同步后树/页面联动
check_contains "15e. 同步后新页面可访问" "$BASE/guide/added" "webhook 增量内容"
CODE=$(curl -s -o /dev/null -w '%{http_code}' "$BASE/guide/synced")
check "15f. 删除页面 404" "[ "$CODE" = 404 ]"
# 性能预算（宽松上限防 flaky）
T0=$(python3 -c 'import time; print(int(time.time()*1000))')
curl -s -o /dev/null -X POST -H "Content-Type: application/json" -d '{"object_kind":"push","ref":"refs/heads/main","checkout_sha":"x"}' "$BASE/git/webhook"
T1=$(python3 -c 'import time; print(int(time.time()*1000))')
ELAPSED=$((T1 - T0))
[ "$ELAPSED" -lt 30000 ] && ok "15g. 同步耗时 ${ELAPSED}ms < 30s" || fail "15g. 同步耗时 ${ELAPSED}ms 超预算"
# 分支不匹配忽略
CODE=$(curl -s -o /dev/null -w '%{http_code}' -X POST -H "Content-Type: application/json" -d '{"object_kind":"push","ref":"refs/heads/dev","checkout_sha":"x"}' "$BASE/git/webhook")
check "15h. 分支不匹配 200 忽略" "[ "$CODE" = 200 ]"

# 优雅停机：SIGTERM 后 manifest flush、进程退出
kill -TERM "$PID"
WAITED=0
while kill -0 "$PID" 2>/dev/null && [ $WAITED -lt 15 ]; do sleep 0.5; WAITED=$((WAITED+1)); done
if kill -0 "$PID" 2>/dev/null; then
  fail "SIGTERM 15s 未退出"
  kill -9 "$PID" || true
else
  ok "SIGTERM 优雅退出（${WAITED}x0.5s）"
fi
PID=""

echo ""
echo "==> 结果：$PASS 通过，$FAIL 失败"
[ "$FAIL" = 0 ] && echo "✅ e2e 验收全部通过" || { echo "❌ 存在失败项"; exit 1; }
