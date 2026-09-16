# Coral 使用文档

> 适用版本：v0.1.0 及以上。安装、命令行、配置参考与日常运维。

## 1. 项目介绍

Coral 是一个用 Rust 编写的**自托管 Markdown 文档服务**：把一个目录（或一个 Git 仓库里的 Markdown 目录）变成带实时预览、增量缓存、全文搜索和代码高亮的文档网站。

传统静态站点生成器的工作方式是全站构建——改一个文件也要重新构建整个站点，内容越多等待越久。Coral 从设计上就拒绝了这条路：启动只扫描文件元数据，页面按需渲染，文件保存后只更新变化的部分。

### 为什么选 Coral

**快，是 Coral 的第一设计目标。**

- **毫秒级启动**：数千个文档文件、几十 MB 的内容库，进程起来即就绪，没有漫长的"构建中"。
- **保存即生效**：文件保存后目录树同步更新（重建一棵子树是微秒级操作）；正文走增量重建，只处理变化的那个文件，与站点总规模无关。
- **毫秒级响应**：页面首次访问时渲染并落盘缓存，之后的访问直接命中缓存返回；缓存过期时先返回旧版、后台重建（stale-while-revalidate），用户永远不用等一个"正在编译"的空白页。
- **必要性更新**：重启时与缓存 manifest 对比，只重建变化的文件；运行中实时监听文件系统事件。站点长到多大，更新成本都恒定。

**省心，是 Coral 的第二设计目标。**

- **单文件部署**：单文件即可运行。无运行时依赖、无数据库、无 Node.js、无外部服务，下载即用。
- **零迁移成本**：直接指向现有 Markdown 内容目录即可上线，YAML/TOML front matter 自动识别，存量内容一行不用改。
- **低资源占用**：Rust 实现，无 GC 停顿，内存占用极小（30M），小规格容器即可长期稳定运行。
- **缓存可丢弃**：缓存是纯派生数据，随时可整目录删除，重启自动全量重建，不丢任何内容。

**功能按需开启，开了就好用。**

- **全文搜索**：一个配置项开关，开启后自动后台建索引，无需外接搜索服务。
- **Git 同步**：自动镜像仓库中的文档目录，Webhook 触发增量刷新，Push 即发布。
- **现代渲染**：代码高亮、Mermaid 图表、KaTeX 数学公式、notice/tabs/children 等常用 shortcode。
- **为容器而生**：优雅停机、`/healthz`/`/readyz` 探针、JSON 结构化日志，开箱即配 K8s。

## 2. 安装

### macOS（Apple Silicon，推荐 brew）

```bash
brew install haoxz11/coral/coral-cli
```

formula 名为 `coral-cli`，安装后的命令叫 `coral`。验证：

```bash
coral -v
```

### Linux（x86_64）

一键安装脚本（随每个 Release 发布）：

```bash
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/haoxz11/coral/releases/latest/download/coral-cli-installer.sh | sh
```

或从 [Release 页](https://github.com/haoxz11/coral/releases) 手动下载 `coral-cli-x86_64-unknown-linux-gnu.tar.xz` 解压使用。

### macOS 手动下载的注意事项

浏览器直接下载的 .tar.xz 可能被 Gatekeeper 拦截，放行：

```bash
xattr -d com.apple.quarantine ./coral
```

推荐优先走 brew，可完全绕开此问题。

### Docker / Kubernetes

镜像基于 debian:bookworm-slim，从源码构建：

```bash
rustup target add x86_64-unknown-linux-gnu
cargo zigbuild --release --target x86_64-unknown-linux-gnu
podman build -f deploy/Dockerfile -t coral:latest .
```

容器默认读取 `/etc/coral/coral.toml`（用 configMap 挂载覆盖），K8s 清单参考 `deploy/k8s.yaml`。

## 3. 更新版本

```bash
# macOS（brew）
brew update && brew upgrade coral-cli

# Linux（安装脚本幂等，重跑即拉取最新版）
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/haoxz11/coral/releases/latest/download/coral-cli-installer.sh | sh
```

更新后用 `coral -v` 确认版本号。缓存是纯派生数据，跨版本升级无需任何迁移操作；若缓存格式不兼容，Coral 会自动全量重建，仅表现为首次启动稍慢。

## 4. 命令行

Coral 的全部命令行界面就是这些——没有子命令，启动即服务：

```
coral [选项]
```

| 参数 | 说明 |
|---|---|
| `--config <path>` / `-c` | 指定 TOML 配置文件启动（与 `--dir` 互斥） |
| `--dir <path>` | **零配置模式**：直接服务指定 Markdown 目录（与 `--config` 互斥），其余全部取默认值 |
| `--port <n>` | 覆盖配置中的监听端口（默认 3000） |
| `--bind <addr>` | 覆盖配置中的监听地址（默认 0.0.0.0） |
| `--draft true\|false` | 覆盖草稿开关（见 [content] 配置） |
| `--search true\|false` | 覆盖全文搜索开关 |
| `--backfill-date <dir>` | 一次性维护命令：把文件的可靠时间写入 Markdown 的 front matter `date` 字段（执行后退出，不启动服务；详见下文） |
| `--force` | 与 `--backfill-date` 配合：已有 `date` 的文件也替换（默认只补齐无值的） |
| `--all` | 与 `--backfill-date` 配合：处理目录下全部文件（git 模式默认只处理工作区有变更的文件） |
| `-v` / `-V` / `--version` | 输出版本号并退出 |

典型用法：

```bash
coral --config /etc/coral/coral.toml        # 标准方式：配置文件启动
coral --dir ./docs                          # 零配置：直接把 ./docs 变成文档站
coral --dir ./docs --port 8080              # 零配置 + 单项覆盖
coral --dir ./docs --search true            # 零配置 + 临时开启全文搜索
```

### backfill-date：批量补 front matter date

排序场景（发布版本、需求文档目录想按时间倒序看最新）依赖 front matter 的 `date` 字段，但存量文档常常没写。这个命令把每个文件的可靠时间批量写入：

```bash
coral --backfill-date ./content              # git 仓库：只处理工作区有变更的文件
coral --backfill-date ./content --all        # 处理目录下全部 md
coral --backfill-date ./content --force      # 已有 date 的文件也替换（默认跳过）
coral --backfill-date ./content --all --force  # 组合使用
```

时间来源规则：

| 场景 | 默认（不加 `--all`） | 加 `--all` |
|---|---|---|
| git 仓库 | 只处理**工作区有变更**的文件（改过的/待提交的/新文件）：改过的用 git 最后提交时间，新文件用文件修改时间 | 全部 md：已提交的用 git 最后提交时间，未提交的用文件修改时间 |
| 非 git 目录 | 全部 md 用文件修改时间（`--all` 无区别） | 同左 |

说明：

- 只处理 `.md` 文件；无 front matter 块的文件会自动创建（只含 `date`）
- git 最后提交时间比文件修改时间可靠——git 同步/rsync 会把所有文件的修改时间刷成同一时刻，而提交时间是内容真实变更的时刻
- 执行后**需要重启 Coral 服务**才在目录树排序中生效（树缓存按目录变化判断过期，不感知文件内容变化）
- 命令输出统计：补齐多少（其中用文件修改时间回填多少）、跳过多少（已有 date）、替换多少（`--force`）

`--dir` 零配置模式的细节：

- 端口 3000、监听 0.0.0.0、草稿隐藏、搜索关闭，全部默认值
- 缓存写到系统临时目录 `coral-<目录名>`（避免多个不同目录的实例互踩）
- 页脚文案留空，回退到首页（根 `_index.md`）的 title

退出码约定：`2` = 用法错误（缺参数、`--config` 与 `--dir` 同给）；`1` = 启动失败（配置解析错误、content 目录不存在、git 必填项缺失等，stderr 给出可读原因）。

## 5. 配置参考

配置文件为 TOML 格式，完整示例见 `deploy/coral.example.toml`。除 `[content].root` 外所有项可省略取默认值；**未知键直接报错**（不静默忽略，拼写错误在启动时暴露）。同名命令行参数可覆盖对应配置项。

### `[server]` — 服务

```toml
[server]
port = 3000          # 监听端口，默认 3000
bind = "0.0.0.0"     # 监听地址；默认 0.0.0.0（局域网可访问），仅本机使用改为 "127.0.0.1"
footer = "© 2026 团队"  # 页脚文案；不配置回退首页（根 _index.md）title
```

`bind` 的效果：`0.0.0.0` 监听所有网卡，适合容器/局域网部署；`127.0.0.1` 只有本机能访问。

### `[content]` — 内容源

```toml
[content]
root = "/path/to/your/content"   # Markdown 根目录，必填；不存在则启动报错退出
exclude = ["drafts", "archive"]  # 排除目录（相对路径）
draft = false                    # false：draft 文档彻底 404；true：正常渲染并进目录树
```

`root` 直接指向现有 Markdown 目录即可，无需改造内容结构。`draft = true` 适合写作预览阶段，发布态保持 `false`。

### `[tree]` — 目录树加载

```toml
[tree]
initial_depth = 2   # 首屏展示的层级深度，默认 2
expand_depth = 1    # 展开一个节点时预载的层级数，默认 1
```

深度只影响目录树的加载范围，不影响内容可达性——深层文档仍可通过链接和搜索访问，展开时按 `expand_depth` 渐进加载。内容层级深、首屏想更快时可以调低 `initial_depth`。

### `[cache]` — 缓存

```toml
[cache]
dir = "./cache"   # 缓存目录，默认 ./cache
```

缓存是**纯派生数据**：删除整个目录后重启即自动全量重建，不丢任何内容。磁盘空间紧张或怀疑缓存异常时，随时可删。

### `[log]` — 日志

```toml
[log]
format = "pretty"   # pretty | json
```

`pretty` 适合本机看；`json` 为结构化输出，供 K8s/采集系统消费。日志级别通过环境变量 `RUST_LOG` 控制（默认 `info`）。稳态下 INFO 日志每分钟个位数，不会刷屏。

### `[search]` — 全文搜索

```toml
[search]
enabled = false   # 默认关闭
```

开启后首次访问时在后台建立索引（大内容库不阻塞启动），搜索框随之可用。也可不改文件，用 `--search true` 临时开启。

### `[render]` — 渲染增强

```toml
[render]
# mermaid_cdn = "https://cdn.jsdelivr.net/npm/mermaid@11/dist/mermaid.esm.min.mjs"
# katex_cdn   = "https://cdn.jsdelivr.net/npm/katex@0.16/dist/katex.min.js"
# inline_math = false
```

Mermaid 图表与 KaTeX 数学公式的前端库从 CDN 加载，内网环境可把两个 URL 指向自建 mirror。块级 `$$...$$` 公式默认启用；行内 `$...$` 公式识别默认关闭（避免误伤价格、shell 变量等单 `$` 文本），确认站点内容无此风险后再开启。

### `[git]` — Git 内容同步

默认关闭。开启后 Coral 在后台 clone 指定仓库，把 `watch_dir` 目录的完整内容**镜像**到 `content.root`，并提供 `/git/webhook` 端点（push 事件触发增量刷新，push 即发布）。

> **⚠️ 本地调试慎用**：镜像是双向覆盖语义——`content.root` 的最终内容完全由远端仓库的 `watch_dir` 决定，远端没有的文件会被删除，远端的改动会覆盖本地同路径文件。如果你把 `content.root` 指向一个自己正在写作/修改的本地目录，开启 git 同步后本地改动会被冲掉。该功能是为"服务器从唯一内容源拉取"的部署形态设计的；本地开发请保持关闭，直接 `--dir` 指向本地目录即可。

> **⚠️ 首次启动有空白期**：开启 git 同步且 `content.root` 尚无内容时，Coral 先启动 HTTP 服务、再后台 clone 远端仓库，克隆和镜像完成前站点是空的（首页/目录树无内容），耗时取决于仓库大小与网络。这是预期行为——准备好内容源（远端 `watch_dir` 已有文档）再对外提供服务；K8s 部署可让 `/readyz` 通过后再接流量。

```toml
[git]
enabled = true
remote = "git@gitlab.example.com:group/docs.git"  # SSH 远端，必填
branch = "main"                                    # 跟踪分支，必填
watch_dir = "docs"                                 # 仓库内目录 → 镜像到 content.root 根，留空或者/那么就同步仓库全部文件
private_key_path = "/etc/coral/id_ed25519"         # SSH 私钥（K8s 用 Secret 挂载，权限需 0400）
host_key = ""                                      # 可选；留空 = 首次连接信任并记录
secret_token = ""                                  # webhook 校验；留空 = 仅接受本机来源
shallow = true                                     # 首次 clone 只拉最新快照（默认 true）
```

开启时必填项缺失会在启动时 fail-fast 报错。`shallow = false` 用于需要历史回滚能力的部署。

## 6. 日常运维速查

| 操作 | 方式 |
|---|---|
| 健康检查 | `GET /healthz`（进程存活）、`GET /readyz`（索引就绪） |
| 停机 | 发送 SIGTERM/SIGINT：自动停止接收新请求 → 等待在途请求（上限 10s）→ 写回缓存 → 退出 |
| 清缓存 | 直接删除 `[cache].dir` 目录后重启，自动全量重建 |
| 强制全量重建 | 同上（删缓存是唯一且推荐的手段，Coral 不提供 "rebuild" 命令） |
| 换端口临时调试 | `coral --config ... --port 8080`，不改配置文件 |
| K8s 探针 | liveness → `/healthz`，readiness → `/readyz`（就绪前不接流量） |

## 7. 常见问题

**启动报 `缺少 --config 或 --dir 参数`？**
Coral 不读取固定的默认配置路径，必须显式指定其一。容器镜像的默认 CMD 已指向 `/etc/coral/coral.toml`。

**改了文件页面没更新？**
目录树是同步即时更新的；正文是异步重建（stale-while-revalidate），下一次请求即拿到新内容。若始终未更新，删除缓存目录重启排查。

**配置里写错键名会怎样？**
启动直接失败并指出错误，不会静默忽略——这是有意设计。

**为什么浏览器下载的二进制无法打开？**
macOS Gatekeeper 拦截未签名二进制，见第 2 节的放行命令，或改用 brew 安装。
