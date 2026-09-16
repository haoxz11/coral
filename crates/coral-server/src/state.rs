//! AppState 与启动编排。
//!
//! SiteIndex 写时复制：任何变更构造新索引后单次写锁替换，
//! 读路径 clone Arc 零锁等待拿一致性快照。

use coral_core::cache::CacheStore;
use coral_core::config::Config;
use coral_core::scanner::{self, SiteIndex};
use coral_core::search::SearchIndex;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;
use tracing::{info, warn};

use crate::singleflight::Singleflight;

/// 搜索状态：未开启恒 None；构建中 Ready 标志 false。
#[derive(Default)]
pub struct SearchState {
    /// None = 未开启；Some(None) = 开启但构建中；Some(Some) = 就绪
    inner: std::sync::RwLock<Option<Arc<SearchIndex>>>,
    enabled: bool,
    /// 重建进行中标记（/search/reindex 并发去重：重建中再请求立刻失败）
    reindexing: AtomicBool,
    /// 最近一次重建完成通知（detached 重建任务 set；请求端 await 拿响应；
    /// 请求方断开只丢响应，任务不受影响）
    reindex_done: tokio::sync::Notify,
}

impl SearchState {
    /// 就绪索引（未开启或构建中返回 None）。
    pub fn ready_index(&self) -> Option<Arc<SearchIndex>> {
        self.inner
            .read()
            .expect("search 锁中毒")
            .clone()
            .filter(|_| self.enabled)
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn set_ready(&self, idx: Arc<SearchIndex>) {
        *self.inner.write().expect("search 锁中毒") = Some(idx);
    }

    /// 尝试进入重建态：CAS 抢占，已被占用返回 false（调用方立刻拒绝）。
    pub fn try_begin_reindex(&self) -> bool {
        self.reindexing
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    pub fn end_reindex(&self) {
        self.reindexing.store(false, Ordering::Release);
    }

    /// 通知一次重建完成（请求端 notified().await 唤醒）。
    pub fn notify_reindex_done(&self) {
        self.reindex_done.notify_waiters();
    }

    /// 等待重建完成通知。带超时防极端卡死（重建任务完成时一定 notify）。
    pub async fn wait_reindex_done(&self) {
        let notified = self.reindex_done.notified();
        tokio::pin!(notified);
        notified.as_mut().await;
    }
}

/// 服务全局状态。
pub struct AppState {
    pub cfg: Arc<Config>,
    /// canonicalize 后的 content 根（路径安全校验基准）
    pub content_root: PathBuf,
    pub index: std::sync::RwLock<Arc<SiteIndex>>,
    pub cache: Arc<CacheStore>,
    /// /readyz 依据（索引就绪标记）
    pub ready: AtomicBool,
    pub flights: Singleflight,
    /// 渲染次数计数（诊断 + singleflight 测试断言）
    pub render_count: std::sync::atomic::AtomicUsize,
    /// 全文搜索（默认关闭）
    pub search: SearchState,
    /// git 内容同步运行态（默认关闭时标记闲置）
    pub git: crate::git_sync::GitState,
}

impl AppState {
    pub fn new(cfg: Config, index: SiteIndex, cache: CacheStore, content_root: PathBuf) -> Self {
        let search_enabled = cfg.search.enabled;
        Self {
            cfg: Arc::new(cfg),
            content_root,
            index: std::sync::RwLock::new(Arc::new(index)),
            cache: Arc::new(cache),
            ready: AtomicBool::new(false),
            flights: Singleflight::default(),
            render_count: std::sync::atomic::AtomicUsize::new(0),
            search: SearchState {
                inner: std::sync::RwLock::new(None),
                enabled: search_enabled,
                reindexing: AtomicBool::new(false),
                reindex_done: tokio::sync::Notify::new(),
            },
            git: crate::git_sync::GitState::default(),
        }
    }

    /// 读一致性快照（写时复制，读路径零锁等待）。
    pub fn snapshot(&self) -> Arc<SiteIndex> {
        self.index.read().expect("index 锁中毒").clone()
    }

    /// 整树替换（watcher/重建变更用）。
    /// 索引换了，树 JSON 的数据源就换了：连带清空全部树缓存，
    /// 防止"目录 mtime 未变但索引已变"窗口内重建写入旧数据缓存。
    pub fn replace_index(&self, new_index: SiteIndex) {
        self.cache.invalidate_all_trees();
        *self.index.write().expect("index 锁中毒") = Arc::new(new_index);
    }

    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }

    pub fn set_ready(&self) {
        self.ready.store(true, Ordering::Release);
    }
}

/// 启动编排：扫描 → manifest diff → 索引 → ready
/// →（开启搜索时）后台构建搜索索引。
/// content 根不存在/不可读在扫描层报错，由调用方（cli）fail-fast。
pub fn initialize(cfg: Config) -> Result<Arc<AppState>, scanner::ScanError> {
    let start = Instant::now();
    let result = scanner::scan(&cfg.content)?;
    let content_root = result.root.clone();

    // 启动增量：diff 只清缓存不渲染
    let cache = CacheStore::open(&cfg.cache.dir);
    let loaded = cache.load_manifest();
    let pages: std::collections::HashMap<String, _> = result
        .pages
        .iter()
        .map(|(rel, p)| {
            (
                rel.to_string_lossy().into_owned(),
                (
                    p.mtime
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as u64,
                    p.size,
                    p.url.clone(),
                ),
            )
        })
        .collect();
    let dirs: std::collections::HashMap<String, u64> = result
        .dirs
        .iter()
        .map(|(rel, d)| {
            (
                rel.to_string_lossy().into_owned(),
                d.mtime
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64,
            )
        })
        .collect();
    let diff = cache.diff_against_scan(&pages, &dirs);
    info!(
        manifest_loaded = loaded,
        total = pages.len(),
        changed = diff.changed.len(),
        added = diff.added.len(),
        removed = diff.removed.len(),
        elapsed_ms = start.elapsed().as_millis() as u64,
        "启动扫描完成（增量 diff）"
    );

    let index = SiteIndex::build(result, cfg.content.draft);
    let search_dir = cfg.cache.dir.join("search-index");
    let state = Arc::new(AppState::new(cfg, index, cache, content_root));
    state.set_ready();

    // 搜索索引后台全量构建（不阻塞 readyz）。
    // 构建中 API 返回空结果 building 标记
    if state.search.is_enabled() {
        let state2 = state.clone();
        let snapshot = state.snapshot();
        let root = state.content_root.clone();
        let draft_enabled = state.cfg.content.draft;
        std::thread::spawn(move || {
            let start = Instant::now();
            match coral_core::search::SearchIndex::open(&search_dir).and_then(
                |(si, need_rebuild)| {
                    if need_rebuild {
                        let _ = si.build_full(&snapshot, &root, draft_enabled)?;
                    }
                    Ok(si)
                },
            ) {
                Ok(si) => {
                    state2.search.set_ready(Arc::new(si));
                    info!(
                        elapsed_ms = start.elapsed().as_millis() as u64,
                        "搜索索引就绪"
                    );
                }
                Err(e) => warn!(%e, "搜索索引构建失败，搜索不可用（服务继续）"),
            }
        });
    }
    Ok(state)
}
