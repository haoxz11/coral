# Coral 使用文档

## 1. 项目介绍

Coral 是一个**自托管 Markdown 文档服务**：把一个目录（或一个 Git 仓库里的 Markdown 目录）变成带实时预览、增量缓存、全文搜索和代码高亮的文档网站，并且**内嵌 MCP 端点**——AI 助手可以直接参与文档写作。

传统静态站点生成器的工作方式是全站构建——改一个文件也要重新构建整个站点，内容越多等待越久。Coral 从设计上就拒绝了这条路：启动只扫描文件元数据，页面按需渲染，文件保存后只更新变化的部分。

### 为什么选 Coral

**快，是 Coral 的第一设计目标。**

- **毫秒级启动**：数千个文档文件、几十 MB 的内容库，进程起来即就绪，没有漫长的"构建中"。
- **保存即生效**：文件保存后目录树立即更新，正文只重建变化的那一篇——与站点总规模无关。
- **毫秒级响应**：页面首次访问时渲染并落盘，之后直接命中缓存；内容更新时先返回旧版、后台重建，读者不会撞上"正在编译"的空白页。
- **只做必要的更新**：重启时只重建变化的文件，运行中实时监听文件改动；站点长到多大，更新成本都恒定。

**省心，是 Coral 的第二设计目标。**

- **单文件部署**：单文件即可运行。无运行时依赖、无数据库、无 Node.js、无外部服务，下载即用。
- **零迁移成本**：直接指向现有 Markdown 内容目录即可上线，YAML/TOML front matter 自动识别，存量内容一行不用改。
- **低资源占用**：Rust 实现，无 GC 停顿，内存占用小，小规格容器即可长期稳定运行。
- **缓存可丢弃**：缓存是纯派生数据，随时可整目录删除，重启自动全量重建，不丢任何内容。

**让 AI 参与写作，是 Coral 的第三设计目标。**

- **内嵌 MCP 端点**：AI 客户端直接接上就能用，不需要网关、插件或中间服务；配套的 [coral-doc](https://github.com/haoxz11/coral-doc) 技能已按文档库约定写好，一条命令安装
- **插图这件事 AI 能自己做完**：上传的图片/附件存进你自己的对象存储，返回能直接贴进 Markdown 的永久链接——不用人工再传一次、再拼一次 URL
- **AI 数据留在你手里**：MCP 服务由 Coral 自身提供，附件写进你自己的对象存储——不经过任何第三方 SaaS，也不需要为接入多部署一个组件

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

镜像需本地构建（交叉编译出 Linux 二进制后打包）：

```bash
bash bin/image.sh          # 产出 coral:latest（linux/amd64）
```

运行：容器默认读取 `/etc/coral/coral.toml`，把配置与内容目录挂进去即可（内容目录只读、缓存目录需可写）：

```bash
docker run -d --name coral -p 3000:3000 \
  -v /etc/coral/coral.toml:/etc/coral/coral.toml:ro \
  -v /srv/content:/srv/content:ro \
  -v coral-cache:/var/cache/coral \
  coral:latest
```

> 上面挂载的内容路径要与配置里 `[content].root` 一致。K8s 部署清单见 `deploy/k8s.yaml`（含探针、configMap 挂载与可写缓存卷）。

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

```
coral [选项] [子命令]
```

裸跑（不带 `--config`/`--dir`/子命令）：默认配置 `~/.coral/config.toml` 存在则自动用它启动，不存在则显示帮助。

| 参数 | 说明 |
|---|---|
| `--config <path>` / `-c` | 指定 TOML 配置文件启动（与 `--dir` 互斥） |
| `--dir <path>` | **零配置模式**：直接服务指定 Markdown 目录（与 `--config` 互斥），其余全部取默认值 |
| `--port <n>` | 覆盖配置中的监听端口（默认 3000） |
| `--bind <addr>` | 覆盖配置中的监听地址（默认 0.0.0.0） |
| `--draft true\|false` | 覆盖草稿开关（见 [content] 配置） |
| `--search true\|false` | 覆盖全文搜索开关 |
| `--remote <url>` | 本地预览用：把文档里的附件链接（`/f/...`）交给远程 coral 实例解析，本地没配对象存储也能看到图片。须带 `http(s)://` 且不能带路径。优先级最高：启动参数 > 配置文件 > 首页头信息（见 `[upload]` 一节） |
| `-v` / `-V` / `--version` | 输出版本号并退出（第二行带内置 coral-doc skill 版本） |
| `--lang <zh\|en>` | 强制界面语言：影响帮助/错误输出、skill 命令输出、服务日志。默认按 locale 环境检测（`LC_ALL` > `LC_MESSAGES` > `LANG`，zh* → 中文；未设置/`C` 在 macOS 上看系统语言，其他平台英文）；服务日志语言还可由配置 `[log] language` 控制，此参数优先 |

除启动参数外还有两组子命令（执行后退出，不启动服务）：

| 子命令 | 说明 |
|---|---|
| `coral skill <install\|ensure\|upgrade>` | 管理配套技能 coral-doc：`install` 安装/覆盖内置版；`ensure` 幂等检查（未装/过旧则升级，本地更新保留）；`upgrade` 联网从 GitHub 拉最新版（需 git） |
| `coral doc backfill-date <dir>` | 一次性维护：把文件的可靠时间写入 Markdown 的 front matter `date` 字段（详见下文） |
| `coral doc permalink-check <dir>` | 扫描 permalink 冲突并报告（裁决规则与运行时一致：date 老者胜、相等时先注册者胜）；`--fix` 删除失效方的 permalink 字段（行级手术，保注释保顺序），不加则只报告 |

典型用法：

```bash
coral --config /etc/coral/coral.toml        # 标准方式：配置文件启动
coral --dir ./docs                          # 零配置：直接把 ./docs 变成文档站
coral --dir ./docs --port 8080              # 零配置 + 单项覆盖
coral --dir ./docs --search true            # 零配置 + 临时开启全文搜索
coral --dir ./content --remote https://docs.example.com  # 本地预览：/f/ 附件走远程实例
```

### backfill-date：批量补 front matter date

排序场景（发布版本、需求文档目录想按时间倒序看最新）依赖 front matter 的 `date` 字段，但存量文档常常没写。这个命令把每个文件的可靠时间批量写入：

```bash
coral doc backfill-date ./content                     # git 仓库：只处理工作区有变更的文件
coral doc backfill-date ./content --all               # 处理目录下全部 md
coral doc backfill-date ./content --force             # 已有 date 的文件也替换（默认跳过）
coral doc backfill-date ./content --all --force       # 组合使用
coral doc backfill-date ./content --date-source first # 用最早提交时间（默认 last）
```

> 0.3.0 之前此命令是服务启动参数 `coral --backfill-date <dir>`，已改为上述子命令形态。

时间来源规则：

| 场景 | 默认（不加 `--all`） | 加 `--all` |
|---|---|---|
| git 仓库 | 只处理**工作区有变更**的文件（改过的/待提交的/新文件）：改过的用 git 提交时间（`--date-source` 可选，默认最后提交时间，`first` 为最早提交时间），新文件用文件修改时间 | 全部 md：同左 |
| 非 git 目录 | 全部 md 用文件修改时间（`--all` 无区别） | 同左 |

说明：

- 只处理 `.md` 文件；无 front matter 块的文件会自动创建（只含 `date`）
- 时间来源的选择：文件修改时间会被 git 同步/rsync 刷成同一时刻，不可靠；git 提交时间可靠，但注意**批量提交会把一批文件的最后提交时间刷成一致**（无法区分批内先后），此时用 `--date-source first`（最早提交时间 = 文件首次入库）可反映真实诞生顺序
- 执行后**需要重启 Coral 服务**，排序才会生效
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
watch_dir = "docs"                                 # 仓库内目录 → 镜像到 content.root 根；留空则同步仓库全部文件
private_key_path = "/etc/coral/id_ed25519"         # SSH 私钥（K8s 用 Secret 挂载，权限需 0400）
host_key = ""                                      # 可选；留空 = 首次连接信任并记录
secret_token = ""                                  # webhook 校验；留空 = 仅接受本机来源
shallow = true                                     # 首次 clone 只拉最新快照（默认 true）
```

开启时必填项缺失会在启动时 fail-fast 报错。`shallow = false` 用于需要历史回滚能力的部署。

### `[mcp]` — AI 助手接入（默认开启）

开启后 Coral 提供 MCP 端点 `POST /mcp`，供各类 AI 客户端（Agent / Skill 等）**辅助文档编写**：当前提供附件上传能力，后续会扩展搜索等。

```toml
[mcp]
# enabled = true        # 默认就是开启的，不用写；写 false 则 /mcp 返回 404
# token = ""            # 非空 → 客户端需带 Authorization: Bearer <token>
# allowed_origins = []  # 只放行这些来源的浏览器请求；留空 = 一律拒绝（命令行/桌面客户端不发 Origin，不受影响）
```

MCP 端点**默认开启**（默认各能力都关着，所以默认状态下工具列表为空）；`token` 为空时不鉴权，需要鉴权的部署请自行配置。

在 MCP 客户端里把 URL 填成 `[http|https]://<你的域名或IP[:有端口的话]>/mcp` 即可。接入后 `initialize` 会返回服务名 **`coral-mcp`**（标题「Coral 文档库编写助手」），各工具描述都以 `【coral-mcp】` 开头——如果你同时接了多个 MCP 服务，可以据此确认工具来自 Coral。

> 开端点不等于有工具：工具来自各项能力（如 `[upload]`）。没开能力时客户端会看到空工具列表。

> **能力自检**：首次访问 `/mcp` 时会对已启用的能力做一次**静态配置检查**（每个进程一次，结果缓存）。检查未通过的能力会被停用——工具不再列出、调用它返回具体原因，端点与其他能力不受影响；**站点本身照常服务**（可选能力的问题不拖垮文档站）。
> 注意两点：① 配错时启动日志只有 WARN，不会阻止启动，排查请看日志或直接调一次工具；② 自检**只检查配置本身**（必填项、endpoint/region 是否自洽等），**不联网**——RAM 权限不足、桶不存在、AK 失效这类问题仍会在客户端真正上传时才暴露。

**配套技能：coral-doc（可选）**

Coral 的 MCP 能力可以配合配套技能 [coral-doc](https://github.com/haoxz11/coral-doc) 使用：技能按文档库的约定撰写并落盘 Markdown（也支持导入钉钉文档、优化本地 md 文件），其中插图这一步交给 coral-mcp 上传并换成永久链接——所以要让 AI 自己传图，`[upload]` 得开着。

正式发版的 coral 二进制内置了最新版技能，一条命令安装到 `~/.agents/skills/coral-doc`（之后使用技能时自动检查升级，本地更新则保留）：

```bash
coral skill install
```

`coral -v` 可查看内置技能版本。也可用 git clone 方式安装（见该仓库 README），但该方式不会被 coral 自动升级。

技能的用法与支持范围见该仓库 README。

### `[upload]` — 附件上传到对象存储（默认关闭）

让 AI 客户端把图片/附件**直接传到你的阿里云 OSS**（文件数据不经过 Coral），上传完拿到能长期贴进 Markdown 的链接。上传工具通过 MCP 端点 `POST /mcp` 提供，端点默认开启，不需要额外配置。

```toml
[upload]
enabled = true
provider = "oss"
endpoint = "https://oss-cn-hongkong.aliyuncs.com"  # 客户端能访问到的地址（内网部署填内网 endpoint）
region = "cn-hongkong"                             # 必须与 bucket 所在地域一致，不一致则该能力被停用（启动只 WARN，不阻止启动）
bucket = "my-docs"
access_key_id = "LTAI..."                          # 建议用最小权限 RAM 子账号
access_key_secret = "..."
prefix = "uploads/"                                # 对象都放在这个前缀下；RAM 策略也限定到这里
link_mode = "redirect"                             # redirect：/f/{key}（私有桶）| direct：直链（公共读桶/CDN）
# public_base_url = ""                             # link_mode = "direct" 时必填（CDN 或自定义域名）
max_size_mb = 8                                    # 单文件上限（MB，默认 8），超出会被对象存储拒绝
# remote_base = "https://docs.example.com"         # 远程附件地址（本地预览用，见下）
# put_expires_secs = 600                           # 上传凭证有效期（秒，上限 604800）
# get_expires_secs = 300                           # 下载链接签名有效期（秒，上限 604800）
```

**云上要准备什么**

1. 一个 OSS bucket，以及一个只有该 bucket `{prefix}` 读写的 RAM 子账号（`oss:PutObject` + `oss:GetObject` 即可，**不需要**删除权限）
2. `region` 与 `endpoint` 必须对应同一个地域：endpoint 是 `oss-cn-hongkong.aliyuncs.com` 时 `region` 就得是 `cn-hongkong`（配错不会阻止启动，但该能力会被停用并在 MCP 调用时告诉你正确值）
3. 部署机器与 OSS 的**时钟要同步**（NTP），偏差超过 15 分钟上传会被拒

**上传是怎么走的**

AI 客户端调用 `upload_file` 拿到上传凭证，直接把文件 `POST` 给 OSS（这一步不经过 Coral），随即得到访问链接与一段可粘贴的 Markdown（图片渲染成 `![](...)`）。

> 自己实现客户端时：按 `upload_file` 返回的字段组装 multipart 表单提交，文件字段名用 `file`。

**两种链接模式的取舍**

| 模式 | 返回什么 | 适合 |
|---|---|---|
| `redirect` | Coral 的永久链接 `/f/{key}`，访问时跳转到临时签名地址 | 私有桶（默认）；链接永久有效，但依赖 Coral 在线 |
| `direct` | 对象存储/CDN 的直链 | 公共读桶或已接 CDN；链接不依赖 Coral，但需要桶可公开读 |

**本地预览：让附件链接指向另一个实例**

本地没配对象存储、但文档里有附件（`/f/...`）时，可以指定一台「能访问对象存储的」coral 实例：**渲染出的页面会把这些链接直接写成那台实例的绝对地址**，浏览器直接去取图（不经过当前实例，也少一跳）。三个地方可写，**优先级：启动参数 > 配置文件 > 首页头信息**：

| 来源 | 写法 | 说明 |
|---|---|---|
| 启动参数 | `coral --dir ./content --remote https://docs.example.com` | 优先级最高，适合临时预览 |
| 配置文件 | `[upload] remote_base = "https://docs.example.com"` | 该实例长期如此用 |
| 首页头信息 | 首页文档的 front matter 里写 `remote_url: https://docs.example.com` | 跟着内容走（换分支/换站点自动变），优先级最低 |

- 「首页」指根目录的首页文档（`_index.md`、`index.md` 或 `readme.md`，不区分大小写，按此顺序取第一个存在的）
- 三种写法的值都必须形如 `http(s)://域名`（不带路径）。首页头信息里的值若不合法，启动日志会提示并忽略，不影响站点启动
- 改了远程地址后**页面缓存会自动失效重渲**，不需要手动删缓存
- 当前实例自己收到 `/f/...` 请求时（例如本机也配了对象存储）：配置了就用自己的凭证签名跳转，没配置就 404——**它不会替你去访问远程实例**

**注意事项**

- 单文件上限由对象存储强制：超限的文件会被直接拒绝（不会留在桶里），错误里能看到实际大小与上限
- 链接是**不可枚举**的（路径含随机段），但 `redirect` 模式下的 `/f/...` 对任何拿到链接的人可访问——**别把机密文件放进来**
- 只配了对象存储、但想在本地预览含附件的文档时，用 `--remote <已配置实例地址>`（见上文命令行）让远程实例解析链接

### 登录（LDAP 域账号，默认关闭）

> ⚠️ **先读这条**：LDAP 认证部署应置于 **HTTPS 反代**之后。直接用 HTTP 暴露时，登录提交的域密码会明文经过网络——域密码的价值远高于本站任何内容。

部分目录/页面不希望全员可见时，开启 LDAP 登录。整体由配置开关控制，默认关闭（关闭时行为与原来完全一致）：

```toml
[auth]
enabled = true
cookie_secret = "至少16字节的随机串"   # 换这个值 = 所有人重新登录
session_ttl_days = 30               # 会话有效期（默认 30 天，滑动续期：一直用就不掉线，闲置超过这个天数才需要重新登录）
cookie_secure = false               # HTTPS 部署改 true

[auth.ldap]
url = "ldaps://ldap.corp.com"       # ldaps = 加密；ldap = 明文（你的选择，Coral 不唠叨）
# 模板模式（用户 DN 能直接拼出来时用，二选一）：
# user_dn_template = "uid={user},ou=people,dc=corp,dc=com"  # 全员同一 OU；AD 域写 "{user}@corp.com"
# 搜索模式（用户分散在多个 OU 时用，二选一）：
# search_base = "ou=staff,dc=corp,dc=com"     # 在这个子树里搜用户（含全部子 OU）
# search_filter = "(uid={user})"              # 按哪个属性匹配账号名；默认 uid，可改 (cn={user}) 等
# bind_dn = "cn=coral,ou=services,dc=corp"    # 搜索用的服务账号（建议只读）；留空 = 匿名搜索
# bind_password = "..."                       # 与 bind_dn 成对；明文存配置，注意文件权限（见下）
# display = "{user}"                          # 右上角回显名（默认只显示登录名）：
#                                             # {user} = 账号名，其余 {xxx} = 目录里的 xxx 属性，
#                                             # 如 {cn}({user}) → "张三(abc)"；属性缺失时该段置空
#                                             # （登录日志 WARN 提示哪个占位符没取到）
# ca_cert_file = "/etc/coral/ldap-ca.pem"     # ldaps + 内网自签证书时必配（不配会连接失败）

[auth.whitelist]                    # 登录白名单（默认关闭 = 任何域账号密码正确即可登录）
enabled = true
users_file = "users.txt"            # 相对配置文件目录；一行一个账号名，# 开头是注释；改完即时生效不用重启
```

**两种绑定模式怎么选**： Coral 不存储任何账号密码——验证时它拿用户输入的账号密码去你的
LDAP 服务器「登录一次」（bind），对不对由服务器说了算。区别只在于「用户名怎么换算成
LDAP 里的记录」：

- 模板模式：`user_dn_template` 本地拼接（快，但要求全员 DN 同构——AD 的 `账号@域名`
  天然满足；OpenLDAP 全员同一 OU 也满足）
- 搜索模式：先用服务账号（或匿名）在 `search_base` 子树里按 `search_filter` 搜出用户 DN，
  再用「搜到的 DN + 用户密码」bind（多 OU 目录树必须用它）
- 同名命中多条记录时取第一条并在日志 WARN（账号名本应唯一，提示管理员收紧 filter）

**在内容里标记「需要登录」**：在文档的 front matter 写 `auth: true`——

| 写在哪 | 效果 |
|---|---|
| 目录首页（`_index.md` 等） | 整个目录（含子目录）都要登录才能看；未登录的人在左侧菜单里**看不到这个目录** |
| 普通文档 | 左侧菜单照常显示，点进去要登录（页面内嵌登录表单，登录后原地显示内容） |

- `auth: false` 等于没写；目录锁了子页不能单独开门
- 根目录首页写 `auth: true` = 整站都要登录（内部站点形态）
- 登录后右上角显示账号，点开可退出；会话默认保持 30 天
- 受保护目录里的图片等附件同样被锁（未登录拿不到）；注意：**单篇文档加密锁不住同目录的附件**，要锁附件请用目录加密

**部署提示**

- AD 域通常拒绝明文连接：`ldap://` 对 AD 大概率连不上，推荐 `ldaps://` + `ca_cert_file`
- 搜索模式的服务账号只给**读权限**（能搜索用户条目即可）；密码明文存在配置文件——
  裸机部署 `chmod 600 coral.toml`，K8s 用 Secret 挂载（`defaultMode: 0400`，与 git-sync 私钥同款做法）
- `users.txt` 在 K8s 里用 ConfigMap 挂载到配置文件同级目录
- 登录失败统一提示「用户名或密码错误」，不区分密码错还是不在白名单（不向外泄露判断依据；日志里有区分）

## 6. 别用这些 URL（Coral 自己占用了）

Coral 自身的端点占用了下面这些 URL，**优先级最高**——你的内容里不要用它们（无论是 front matter 里的 `permalink`，还是文件/目录名正好撞上）：

| 类别 | 路径 |
|---|---|
| 精确 | `/healthz`、`/readyz`、`/search`、`/search/reindex`、`/api/search`、`/api/tree/children`、`/git/webhook`、`/favicon.ico`、`/mcp`、`/login`、`/logout` |
| 前缀 | `/assets/`、`/f/` |

- 这些路径**与开关无关**：即使没开启上传，`/mcp`、`/f/` 也照样被占——这样同一份内容在任何配置下表现一致
- 撞上了 Coral 不会静默：启动日志会提示，并按下面的规则回退——
  - `permalink` 撞上 → 该 permalink **不生效**，页面回到默认 URL（目录树里的链接也会跟着回到默认 URL）
  - 文件名正好撞上（如根目录放了个 `mcp.md`）→ 该页改用 `/mcp/index`
  - 文件落在保留前缀里（如 `f/pic.md`），或目录名撞上（如 `search/`）→ 这些页面无法访问（日志会提示）
- 建议：内容目录别用 `assets/`、`f/` 这类名字（`f/` 是上传附件的命名空间）

## 7. 日常运维速查

| 操作 | 方式 |
|---|---|
| 健康检查 | `GET /healthz`（进程存活）、`GET /readyz`（索引就绪） |
| 停机 | 发送 SIGTERM/SIGINT：自动停止接收新请求 → 等待在途请求（上限 10s）→ 写回缓存 → 退出 |
| 清缓存 | 直接删除 `[cache].dir` 目录后重启，自动全量重建 |
| 强制全量重建 | 同上（删缓存是唯一且推荐的手段，Coral 不提供 "rebuild" 命令） |
| 换端口临时调试 | `coral --config ... --port 8080`，不改配置文件 |
| K8s 探针 | liveness → `/healthz`，readiness → `/readyz`（就绪前不接流量） |

## 8. 常见问题

**启动报 `缺少 --config 或 --dir` / 直接显示帮助？**
两者都不指定时（裸跑），Coral 会探测默认配置 `~/.coral/config.toml`：存在则自动用它启动（stderr 提示一行）；不存在则显示帮助并退出。容器镜像的默认 CMD 已指向 `/etc/coral/coral.toml`。

**改了文件页面没更新？**
目录树是立即更新的；正文会在下一次请求时更新。若始终未更新，删除缓存目录重启排查。

**配置里写错键名会怎样？**
启动直接失败并指出错误，不会静默忽略——这是有意设计。

**我有个文档的 permalink 不生效了？**
检查它是否撞上了 Coral 自己占用的 URL（见第 6 节）——启动日志里会有对应 WARN，把 permalink 换成别的路径即可；页面本身仍在默认 URL 可访问。

**启用上传后，客户端报 403 或 400？**
按顺序看三件事：① RAM 子账号是否有 `oss:PutObject`/`oss:GetObject` 且授权范围覆盖 `bucket/prefix/*`；② `region` 与 `endpoint` 是否属于同一地域（配错时启动就会报错，所以这条通常是迁移配置时漏改）；③ 报"超过最大大小"就是触发了 `max_size_mb` 上限——换个文件或调大该值。部署机器时钟与 OSS 偏差超过 15 分钟也会被拒，注意开 NTP。

**为什么浏览器下载的二进制无法打开？**
macOS Gatekeeper 拦截未签名二进制，见第 2 节的放行命令，或改用 brew 安装。

**本地预览时文档里的图片（`/f/xxx`）全是 404？**
这些是上传到 OSS 的附件，链接由**能访问 OSS 的那个实例**解析。本地没配 `[upload]` 时用 `--remote` 指定已配置好的实例即可：

```bash
coral --dir ./content --remote https://docs.example.com
```

此时附件链接会交给 `https://docs.example.com` 解析并签出临时地址。注意两点：远程实例不可达时图片仍然打不开（本地无法代为解决）；`--remote` 只影响 `/f/` 开头的链接，内容目录里的普通静态文件不受影响。
