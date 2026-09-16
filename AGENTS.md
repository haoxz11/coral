# coral 开发准则

> 适用范围：本 workspace 全部 crate（coral-core / coral-server / coral-cli）。
> 上游文档：[需求文档](./docs/需求文档.md) v1.2、[技术设计](./docs/技术设计.md) v1.2。文档与实现冲突时，先停下澄清，不要静默偏向任一方。

## 一、总体原则

- 正确性优先于速度，清晰性优先于炫技
- 优先复用现有模式、公共组件、工具类和基础设施
- 改动要小而集中，避免无关重构
- 对外行为变化必须明确说明
- 对高风险修改优先补测试，再改实现
- 涉及安全、权限、资金、用户身份、数据删除、后台操作审计时，必须提高审慎级别

## 二、代码风格与编码规范

- 遵循标准 Rust API 指南（rust-lang API Guidelines）命名与惯用法：类型 `UpperCamelCase`、函数/变量 `snake_case`、常量 `SCREAMING_SNAKE_CASE`
- 格式化与 lint 是硬性门槛：提交前 `cargo fmt --all` 零 diff、`cargo clippy --workspace --all-targets -- -D warnings` 零警告；`#![allow(...)]` 需在代码注释中写明理由
- 依赖版本按技术设计 §4 精确锁定；新增依赖需说明必要性（是否有标准库/现有依赖可覆盖），升级依赖是显式变更，不在功能改动中夹带
- 公共 API（跨 crate 边界）必须有文档注释（`///`），说明用途、边界条件、错误情形；crate 内部私有项不强制
- 错误处理：库代码（core）用 `thiserror` 定义可匹配的错误类型，禁止 `Box<dyn Error>` 跨边界传递；二进制/应用层（cli/server）用 `anyhow`；禁止静默吞错——`let _ =`、空 `catch`、`unwrap_or_default()` 掩盖 IO 错误一律不允许
- `unwrap()/expect()` 只允许出现在三种位置：测试代码、启动期配置校验（fail-fast）、附注释证明不可达的分支；运行时路径一律 `?` 或显式匹配
- 注释只写代码无法表达的"为什么"（约束、取舍、坑），不写"这行在干什么"；删除注释掉的死代码，git 里有历史
- 模块文件超过约 500 行或职责出现两个以上名词时考虑拆分

## 三、推荐分层

依赖方向严格单向：`coral-cli → coral-server → coral-core`，不允许反向引用，不允许 feature 引入环。

- **coral-core（纯领域层）**：无 tokio/axum/异步运行时依赖，全部同步代码。frontmatter/扫描/树/渲染/缓存逻辑都在这里；统一走 `tracing` 事件；不知道运行时，重 IO 由调用方决定是否包 `spawn_blocking`
- **coral-server（应用层）**：axum Router、路由处理、模板、watcher、singleflight。HTTP 语义（状态码、header、缓存控制）只在这层出现；core 不感知 HTTP
- **coral-cli（入口层）**：clap 参数、配置加载、tracing 初始化、生命周期与优雅停机。业务逻辑为零，只做组装

判断标准：一段逻辑换掉 axum 换成别的框架还要能用，它就该在 core；一段逻辑只在回答"这个 HTTP 请求怎么回"，它就在 server。

## 四、接口规范

- crate 内模块对外只暴露必要项（`pub use` 收敛在 lib.rs，模块内非必要不 pub）；公共结构体字段优先封装，暴露构造/访问方法
- HTTP 端点遵循技术设计 §7.1 的契约：路径、方法、响应结构不得随意增改；行为变化（字段增删、状态码变化）视为破坏性变更，需在提交说明中标注
- JSON 字段命名固定 `snake_case`（serde `rename_all = "snake_case"`）；`#[serde(skip_serializing_if)]` 保持输出稳定，空集合不省略语义的除外
- 缓存文件格式（manifest schema）变更必须递增 `version` 并兼容"旧缓存全量重建"路径
- 配置项增改：提供合理默认值 + 在 `deploy/coral.example.toml` 同步示例；删除配置项需保留一个版本的 WARN 提示
- 错误响应统一走 `error.html` 模板路径，不裸吐内部错误字符串（日志里有细节即可）

## 五、安全规范

- **路径安全是本项目的核心红线**（对应 FR-4 / 技术设计 §5.7）：所有来自请求的路径参数，必须"percent-decode → 规范化 → canonicalize → 校验仍在 content 根内"，顺序不可颠倒；任何改动涉及路径处理时，必须附带穿越用例测试（`..`、`%2e%2e`、绝对路径、symlink 逃逸）
- 拒绝 symlink 逃逸；content 根内 symlink 允许跟随，但落点校验不得跨请求缓存 canonicalize 结果
- 不做 HTML 净化是**已记录的决策**（可信内容源前提，见技术设计 §4）；任何引入外部内容来源的改动，必须先推翻该决策补 ammonia，不允许静默扩大豁免范围
- `menuPre` 等透传 HTML 的字段，转义豁免点（askama `|safe`）必须收敛在模板中可数的位置，禁止在 Rust 侧拼接 HTML 字符串
- 日志与错误页不得泄露堆栈、环境变量、内部 IP 等系统信息
- 依赖供应链：只添加 crates.io 上的主流维护 crate；新依赖先看下载量与最近发布时间

## 六、日志、监控与可观测性

- 统一 `tracing`，禁止 `println!/eprintln!`（CLI 的用户提示输出除外，且用 `stderr`）；span 结构对齐技术设计 §10：`request`（method/path/status/耗时）、`scan`、`watch`、`render`、`unknown_shortcode`
- 日志级别纪律：`ERROR` = 需要人介入；`WARN` = 自动恢复但值得观察（缓存损坏重建、路由冲突、未知 shortcode）；`INFO` = 关键生命周期事件（启动、就绪、监听模式、停机）；`DEBUG` = 其余一切。生产日志量按"稳态下 INFO 每分钟个位数"约束
- 消息用结构化字段（`tracing::info!(files = n, elapsed_ms = ms, ...)`）而不是把变量拼进消息文本，便于 json 格式采集
- 每个错误路径都要有日志且带上下文（rel_path、url），但同一错误只记一次——不层层包装重复打印
- `/healthz` 只反映进程存活，`/readyz` 只反映索引就绪（NFR-7），两者不得混用
- 性能敏感路径（缓存命中判断）不打每请求日志，靠 criterion 基准与采样

## 七、测试规范

- 分层对齐技术设计 §12：单元（模块内）→ 集成（core fixture 树）→ HTTP（server oneshot）→ 基准（criterion）→ e2e（真实目录，scripts/e2e.sh）
- 集成测试的 fixture 树保持**需求无关**：不复制真实内容目录，不新增"只照抄实例特征"的 fixture；fixture 变更是测试设计的显式变更
- 测试命名表达意图：`test_url_decode_before_normalize_blocks_traversal` 而不是 `test_1`
- 失效链（文件变化 → 树重建 → children 祖先链片段失效）必须有端到端集成测试覆盖，这条链是本系统最复杂的正确性约束（FR-8/8a）
- 路径安全用例是回归测试库的一部分，每次改路径处理都必须跑并考虑补充新变体
- 渲染输出做 golden 快照测试（comrak/syntect 升级时 diff 可见）
- 测试不依赖执行顺序、不依赖真实时钟（mtime 显式设置）、不写共享临时目录（每测试 `tempfile` 独立目录）
- `cargo test --workspace` 全绿是合并前提；未覆盖项须在提交说明中显式说明，不许静默跳过

## 八、代码审查视角
当你执行 code review、重构建议或方案评审时，优先关注：

- 是否存在业务 bug
- 是否会引入兼容性或行为回归
- 是否存在权限绕过、越权、敏感信息泄漏、安全配置缺失
- 是否破坏分层边界
- 是否缺少必要测试
- 是否存在明显性能隐患或并发问题

## 九、编码前先思考

**不要假设。不要掩饰困惑。明确呈现权衡。**

在实现之前：
- 明确写出你的假设。如果不确定，就提问。
- 如果存在多种解释，先把它们列出来，不要默默自行选择。
- 如果有更简单的方法，就直接指出来。在有必要时提出异议。
- 如果有不清楚的地方，就停下来。说清楚困惑点，并提问。

## 十、简单优先

**只写解决问题所需的最少代码。不做任何预设性扩展。**

- 不要加入超出需求范围的功能。
- 不要为一次性代码做抽象。
- 不要加入未被要求的"灵活性"或"可配置性"。
- 不要为不可能发生的场景写错误处理。
- 如果你写了 200 行，但 50 行就够，就重写。

问问自己："一个资深工程师会认为这太复杂了吗？" 如果答案是会，那就继续简化。

## 十一、外科手术式修改

**只改必须改的内容。只清理你自己造成的问题。**

编辑现有代码时：
- 不要"顺手优化"相邻代码、注释或格式。
- 不要重构没有坏掉的部分。
- 保持现有风格，即使你个人会写成别的样子。
- 如果发现无关的死代码，可以指出，但不要删除。

当你的改动产生遗留项时：
- 删除那些因你的修改而变成未使用的 import、变量或函数。
- 不要删除原本就存在的死代码，除非被明确要求。

检验标准：每一行改动都应当能直接追溯到用户请求。

## 十二、目标驱动执行

**先定义成功标准，再循环推进，直到验证通过。**

把任务转换成可验证的目标：
- "添加校验" → "先为非法输入写测试，再让测试通过"
- "修复这个 bug" → "先写能复现它的测试，再让测试通过"
- "重构 X" → "确保改动前后测试都通过"

对于多步骤任务，先给出简短计划：
```
1. [步骤] → 验证：[检查项]
2. [步骤] → 验证：[检查项]
3. [步骤] → 验证：[检查项]
```

强有力的成功标准能让你独立闭环推进。弱成功标准（"把它弄好"）则会不断需要额外澄清。

## 十三、业务文档优先

**业务规则文档是业务逻辑开发的前置输入。先对齐文档，再修改代码。**

- 添加或修改任何业务逻辑前，必须先阅读 `docs/README.md`，并按索引加载对应领域的业务规则文档、状态表和代码地图。
- 如果现有代码、需求描述与 `docs/` 中的业务规则不一致，不得静默选择实现口径；必须先明确差异并确认以哪一方为准。
- 业务规则发生变化时，先更新对应领域文档、状态机或状态表，再修改代码和测试。
- 新增业务功能时，先在 `docs/` 中补充业务目标、流程、状态、规则、异常边界和配置口径，再开始代码编写。
- 代码实现、自动化测试、配置说明等必须与业务文档保持一致。
- 任务完成前，应再次核对相关业务文档；如果实现产生了新的状态、事件、配置或约束，必须同步更新文档索引和领域参考。


<!-- TRELLIS:START -->
# Trellis Instructions

These instructions are for AI assistants working in this project.

This project is managed by Trellis. The working knowledge you need lives under `.trellis/`:

- `.trellis/workflow.md` — development phases, when to create tasks, skill routing
- `.trellis/spec/` — package- and layer-scoped coding guidelines (read before writing code in a given layer)
- `.trellis/workspace/` — per-developer journals and session traces
- `.trellis/tasks/` — active and archived tasks (PRDs, research, jsonl context)

If a Trellis command is available on your platform (e.g. `/trellis:finish-work`, `/trellis:continue`), prefer it over manual steps. Not every platform exposes every command.

If you're using Codex or another agent-capable tool, additional project-scoped helpers may live in:
- `.agents/skills/` — reusable Trellis skills
- `.codex/agents/` — optional custom subagents

Managed by Trellis. Edits outside this block are preserved; edits inside may be overwritten by a future `trellis update`.

<!-- TRELLIS:END -->