//! coral 入口层：只做组装（AGENTS.md 分层准则）。
//!
//! clap 参数 → 配置加载（fail-fast 校验）→ tracing 初始化 →
//! server + watcher 组装 → 优雅停机。

use anyhow::{Context, Result};
use clap::Parser;
use coral_core::config::{
    CacheConfig, Config, ContentConfig, GitConfig, LogConfig, LogFormat, RenderConfig,
    SearchConfig, ServerConfig, TreeConfig,
};
use coral_server::state::AppState;
use std::path::Path;
use std::sync::Arc;

#[derive(Parser, Debug)]
#[command(
    name = "coral",
    version,
    about = "Coral 是一个用 Rust 编写的自托管 Markdown 文档服务：把一个 Git 仓库里的 Markdown 目录变成带实时预览、增量缓存、全文搜索和代码高亮的文档网站。"
)]
struct Args {
    /// 配置文件路径（与 --dir 互斥）
    #[arg(long, short, conflicts_with = "dir")]
    config: Option<std::path::PathBuf>,

    /// 零配置模式：直接服务指定 markdown 目录（与 --config 互斥）
    #[arg(long, conflicts_with = "config")]
    dir: Option<std::path::PathBuf>,

    /// 覆盖 [server] port
    #[arg(long)]
    port: Option<u16>,

    /// 覆盖 [server] bind
    #[arg(long)]
    bind: Option<String>,

    /// 覆盖 [content] draft（true：draft 文档正常渲染）
    #[arg(long, action = clap::ArgAction::Set, value_parser = ["true", "false"])]
    draft: Option<String>,

    /// 覆盖 [search] enabled（true：开启全文搜索）
    #[arg(long, action = clap::ArgAction::Set, value_parser = ["true", "false"])]
    search: Option<String>,

    /// 输出版本号并退出（与 -V/--version 等效）
    #[arg(short = 'v', action = clap::ArgAction::Version)]
    print_version: (),

    /// 一次性维护：把 git 最后提交时间写入 md 的 frontmatter date
    /// （无值补齐，--force 替换；执行后退出，不启动服务）
    #[arg(long, conflicts_with_all = ["config", "dir"])]
    backfill_date: Option<std::path::PathBuf>,

    /// 与 --backfill-date 配合：已有 date 也替换
    #[arg(long, requires = "backfill_date")]
    force: bool,

    /// 与 --backfill-date 配合：处理全部文件（git 模式默认只处理工作区变更）
    #[arg(long, requires = "backfill_date")]
    all: bool,

    /// 与 --backfill-date 配合：git 时间来源（first=最早提交时间，last=最后提交时间，默认 last）
    #[arg(long, requires = "backfill_date", value_parser = ["first", "last"], default_value = "last")]
    date_source: String,
}

/// args → 配置（R6：纯函数便于参数矩阵单测）。
///
/// `--dir` 零配置模式下全取默认值，仅派生字段（PRD R2）：
/// `content.root` 为 canonicalize 后的绝对路径（Q7）；
/// `cache.dir` 为 `temp_dir()/coral-<basename>`（Q3：加前缀防同名目录互踩）；
/// footer 留空，渲染期走现有回退链（Q4：根 `_index` title → 兜底 "coral"）。
fn resolve_config(args: &Args) -> Result<Config> {
    // 同时给出由 clap conflicts_with 拦截（exit 2）；此处 config 分支优先作兜底
    let mut cfg = match (&args.config, &args.dir) {
        (Some(path), _) => {
            Config::load(path).with_context(|| format!("加载配置 {} 失败", path.display()))?
        }
        (None, Some(dir)) => dir_config(dir)?,
        (None, None) => anyhow::bail!("缺少 --config 或 --dir 参数"),
    };
    if let Some(port) = args.port {
        cfg.server.port = port;
    }
    if let Some(bind) = &args.bind {
        cfg.server.bind = bind.clone();
    }
    if let Some(draft) = &args.draft {
        cfg.content.draft = draft == "true";
    }
    if let Some(search) = &args.search {
        cfg.search.enabled = search == "true";
    }
    Ok(cfg)
}

/// `--dir` 零配置模式的配置构造（PRD R2）。Config 无 Default（root 无默认值），
/// 逐 section 显式组装；cache.dir 派生规则见 `resolve_config` 注释。
fn dir_config(dir: &Path) -> Result<Config> {
    let root = dir
        .canonicalize()
        .with_context(|| format!("--dir 目录不存在或不可访问：{}", dir.display()))?;
    if !root.is_dir() {
        anyhow::bail!("--dir 不是目录：{}", root.display());
    }
    // canonicalize 结果为 "/" 时无文件名，回退 "coral"
    let name = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "coral".to_string());
    Ok(Config {
        server: ServerConfig::default(),
        content: ContentConfig {
            root,
            exclude: Vec::new(),
            draft: false,
        },
        tree: TreeConfig::default(),
        cache: CacheConfig {
            dir: std::env::temp_dir().join(format!("coral-{name}")),
        },
        log: LogConfig::default(),
        search: SearchConfig::default(),
        git: GitConfig::default(),
        render: RenderConfig::default(),
    })
}

/// 缺参错误提示 + 用法示例（PRD R3）。exit code 2 为 clap 用法错误惯例，
/// 区别于启动失败 exit 1。
fn usage_error_and_exit() -> ! {
    eprintln!("coral 启动失败：缺少 --config 或 --dir 参数\n");
    eprintln!("用法示例：");
    eprintln!("  coral --config /path/to/coral.toml   # 使用配置文件启动");
    eprintln!("  coral --dir /path/to/content           # 零配置：直接服务指定目录");
    eprintln!("  coral --dir ./docs --port 8080         # 零配置 + 单项覆盖");
    eprintln!("  coral --dir ./docs --search true       # 零配置 + 开启全文搜索");
    eprintln!("  coral -v                               # 查看版本号");
    std::process::exit(2);
}

fn init_tracing(format: LogFormat) {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    match format {
        LogFormat::Json => {
            tracing_subscriber::registry()
                .with(env_filter)
                .with(tracing_subscriber::fmt::layer().json())
                .init();
        }
        LogFormat::Pretty => {
            tracing_subscriber::registry()
                .with(env_filter)
                .with(tracing_subscriber::fmt::layer())
                .init();
        }
    }
}

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        // 启动失败：可读提示到 stderr，非 panic 堆栈
        eprintln!("coral 启动失败：{e:#}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let args = Args::parse();
    // backfill-date：一次性维护命令，执行后退出（不启动服务）
    if let Some(root) = &args.backfill_date {
        let root = root
            .canonicalize()
            .with_context(|| format!("--backfill-date 目录不存在：{}", root.display()))?;
        let source = match args.date_source.as_str() {
            "first" => coral_core::backfill::DateSource::First,
            _ => coral_core::backfill::DateSource::Last,
        };
        let report = coral_core::backfill::backfill_date(&root, args.force, args.all, source)?;
        println!(
            "backfill-date 完成：补齐 {}（其中 mtime 回填 {}），跳过（已有 date）{}，替换（--force）{}",
            report.filled, report.mtime_filled, report.skipped_has_date, report.replaced
        );
        return Ok(());
    }
    if args.config.is_none() && args.dir.is_none() {
        usage_error_and_exit();
    }
    let cfg = resolve_config(&args)?;

    // content 根 fail-fast 校验：启动报错退出，可读提示
    if !cfg.content.root.is_dir() {
        anyhow::bail!(
            "content 根目录不存在或不是目录：{}（检查 coral.toml [content].root）",
            cfg.content.root.display()
        );
    }

    // git 同步 fail-fast：必填/可写探针/私钥权限
    if cfg.git.enabled {
        let probe = coral_core::git_sync::GitSync::new(
            cfg.git.clone(),
            cfg.cache.dir.join("git-mirror"),
            cfg.content.root.clone(),
        );
        if let Err(e) = probe.validate() {
            anyhow::bail!("{e}");
        }
    }

    init_tracing(cfg.log.format);
    tracing::info!(
        root = %cfg.content.root.display(),
        port = cfg.server.port,
        bind = %cfg.server.bind,
        draft = cfg.content.draft,
        "coral 启动（只建索引与树，页面按需渲染）"
    );

    // 初始化（扫描 → manifest diff → 索引 → ready；返回 Arc<AppState>）
    let state = coral_server::initialize(cfg.clone()).context("初始化失败（扫描 content 目录）")?;
    let app = coral_server::build_app(state.clone());

    // watcher：失败不拒绝启动，内部已降级
    let _watch_handle = coral_server::watcher::spawn_watcher(state.clone());

    // git 同步：开启时后台 clone + 首次同步（失败 WARN 不阻断）
    if state.cfg.git.enabled {
        coral_server::git_sync::spawn_startup_sync(state.clone());
    }

    // 未知 shortcode 定期汇总由渲染计数器承载——
    // 进程内计数聚合在单页渲染闭包中，此处不额外轮询（v1 简化，
    // 渲染时已逐条 WARN）

    let addr = format!("{}:{}", cfg.server.bind, cfg.server.port);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("监听 {addr} 失败"))?;
    tracing::info!(%addr, "coral 就绪（/readyz 通过）");

    // 优雅停机：SIGTERM/SIGINT → 停 accept → 等 in-flight ≤10s
    // → manifest flush → 退出（K8s terminationGracePeriodSeconds 15 留余量）。
    // ConnectInfo：/search/reindex 的 loopback 来源校验依赖对端地址
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal(state.clone()))
    .await
    .context("HTTP 服务异常退出")?;
    Ok(())
}

async fn shutdown_signal(state: Arc<AppState>) {
    let ctrl_c = async {
        tokio::signal::ctrl_c().await.expect("安装 SIGINT 处理失败");
    };
    #[cfg(unix)]
    let terminate = async {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(e) => {
                tracing::warn!(%e, "安装 SIGTERM 处理失败");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    tracing::info!("收到停机信号，等待在途请求（上限 10s）");
    // git 同步停机联动：等 syncing 复位（超时 WARN 退出，镜像 diff 自愈）
    coral_server::git_sync::wait_sync_done(
        &state,
        tokio::time::Instant::now() + tokio::time::Duration::from_secs(9),
    )
    .await;
    // in-flight 等待由 with_graceful_shutdown 内建；此处做 manifest 最后写回
    if let Err(e) = state.cache.flush() {
        tracing::warn!(%e, "停机时 manifest 写回失败（下次启动全量重建）");
    } else {
        tracing::info!("manifest 已写回，coral 退出");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn args(argv: &[&str]) -> Args {
        Args::parse_from(std::iter::once("coral").chain(argv.iter().copied()))
    }

    #[test]
    fn test_dir_mode_derives_root_and_cache() {
        let dir = tempfile::tempdir().unwrap();
        // 带 "." 分量验证 canonicalize 归一化（Q7）
        let messy = dir.path().join(".");
        let cfg = resolve_config(&args(&["--dir", messy.to_str().unwrap()])).unwrap();
        assert_eq!(cfg.content.root, dir.path().canonicalize().unwrap());
        let name = dir.path().file_name().unwrap().to_string_lossy();
        assert_eq!(
            cfg.cache.dir,
            std::env::temp_dir().join(format!("coral-{name}"))
        );
        assert_eq!(cfg.server.port, 3000);
        assert_eq!(cfg.server.bind, "0.0.0.0");
        assert!(cfg.server.footer.is_none(), "footer 留空走渲染期回退（Q4）");
        assert!(!cfg.search.enabled);
        assert!(!cfg.git.enabled);
    }

    #[test]
    fn test_dir_mode_accepts_overrides() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = resolve_config(&args(&[
            "--dir",
            dir.path().to_str().unwrap(),
            "--port",
            "8080",
            "--bind",
            "127.0.0.1",
            "--draft",
            "true",
        ]))
        .unwrap();
        assert_eq!(cfg.server.port, 8080);
        assert_eq!(cfg.server.bind, "127.0.0.1");
        assert!(cfg.content.draft);
    }

    #[test]
    fn test_search_override_in_dir_and_config_mode() {
        // --dir 模式：默认 false，--search true 覆盖为开启
        let dir = tempfile::tempdir().unwrap();
        let cfg = resolve_config(&args(&[
            "--dir",
            dir.path().to_str().unwrap(),
            "--search",
            "true",
        ]))
        .unwrap();
        assert!(cfg.search.enabled);

        // config 模式：配置里 enabled = true，--search false 覆盖为关闭
        let cfgdir = tempfile::tempdir().unwrap();
        let path = cfgdir.path().join("coral.toml");
        std::fs::write(
            &path,
            "[content]\nroot = \"/tmp/content\"\n[search]\nenabled = true\n",
        )
        .unwrap();
        let cfg = resolve_config(&args(&[
            "--config",
            path.to_str().unwrap(),
            "--search",
            "false",
        ]))
        .unwrap();
        assert!(!cfg.search.enabled);

        // 不给参数：保持配置原值不动（此例配置里 enabled = true）
        let cfg = resolve_config(&args(&["--config", path.to_str().unwrap()])).unwrap();
        assert!(cfg.search.enabled);
    }

    #[test]
    fn test_config_mode_keeps_cache_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("coral.toml");
        std::fs::write(&path, "[content]\nroot = \"/tmp/content\"\n").unwrap();
        let cfg = resolve_config(&args(&["--config", path.to_str().unwrap()])).unwrap();
        assert_eq!(cfg.content.root, PathBuf::from("/tmp/content"));
        // config 模式 cache 默认保持 ./cache（Q3：仅 --dir 模式派生）
        assert_eq!(cfg.cache.dir, PathBuf::from("./cache"));
    }

    #[test]
    fn test_dir_not_exist_is_error() {
        let err = resolve_config(&args(&["--dir", "/nonexistent/coral-xyz"])).unwrap_err();
        assert!(err.to_string().contains("--dir"), "{err}");
    }

    #[test]
    fn test_missing_both_is_error_in_resolve() {
        // 裸跑由 run() 的 usage_error_and_exit 拦截（exit 2）；
        // resolve_config 兜底返回错误，保证单独调用不 panic
        let err = resolve_config(&args(&[])).unwrap_err();
        assert!(err.to_string().contains("--config"), "{err}");
    }

    #[test]
    fn test_config_and_dir_conflict_rejected_by_clap() {
        let err = Args::try_parse_from(["coral", "--config", "a.toml", "--dir", "b"]).unwrap_err();
        assert_eq!(err.exit_code(), 2, "互斥走 clap 用法错误（R3 实现口径）");
    }
}
