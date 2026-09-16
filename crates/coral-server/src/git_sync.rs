//! git 同步 server 集成：
//! GitState（syncing/dirty 标记）、同步后全链刷新、/git/webhook、停机联动。

use crate::state::AppState;
use coral_core::git_sync::{GitSync, SyncStats};
use coral_core::scanner::SiteIndex;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tracing::{info, warn};

/// git 同步运行态：syncing 去重标记（启动 clone 与 webhook 共用）+ dirty（同步期间 watcher 记账）。
#[derive(Default)]
pub struct GitState {
    syncing: AtomicBool,
    dirty: AtomicBool,
    /// 上次同步耗时 ms（自适应响应的估计依据）
    last_sync_ms: std::sync::atomic::AtomicU64,
}

impl GitState {
    pub fn try_begin(&self) -> bool {
        self.syncing
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    pub fn end(&self, elapsed_ms: u64) {
        self.last_sync_ms.store(elapsed_ms, Ordering::Release);
        self.syncing.store(false, Ordering::Release);
    }

    pub fn is_syncing(&self) -> bool {
        self.syncing.load(Ordering::Acquire)
    }

    pub fn last_sync_ms(&self) -> u64 {
        self.last_sync_ms.load(Ordering::Acquire)
    }

    /// watcher 侧：同步期间的批标记 dirty（同步后统一刷新，不逐批重扫）。
    pub fn mark_dirty(&self) {
        self.dirty.store(true, Ordering::Release);
    }

    pub fn take_dirty(&self) -> bool {
        self.dirty.swap(false, Ordering::AcqRel)
    }
}

/// 执行一次完整同步（spawn_blocking 包裹；调用方保证已持有 syncing 标记）：
/// fetch+镜像应用 → 主动全链刷新（同步驱动，不依赖文件系统事件语义）。
pub async fn run_sync(state: &Arc<AppState>, sync: &Arc<GitSync>) -> Result<SyncStats, String> {
    let state = state.clone();
    let sync = sync.clone();
    let start = std::time::Instant::now();
    // ensure_cloned 幂等（已 clone 零开销）；webhook 可能先于启动同步到达
    let stats = tokio::task::spawn_blocking(move || {
        sync.ensure_cloned().map_err(|e| e.to_string())?;
        sync.sync().map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())??;

    // 主动刷新（不依赖文件系统事件语义；对 FSEvents/inotify/NFS-Poll 免疫）
    refresh_after_sync(&state, &stats).await;
    let _ = state.git.take_dirty(); // 同步期间 watcher 记的 dirty 已被本次刷新覆盖
    info!(
        changed = stats.changed.len(),
        removed = stats.removed.len(),
        elapsed_ms = start.elapsed().as_millis() as u64,
        "git 同步+刷新完成"
    );
    Ok(stats)
}

/// 同步后全链刷新。变更比 ≤25% 走增量（局部重扫受影响
/// 目录，路由/children_deps 纯内存重算——单文件变更不触发全量扫描）；
/// >25%（含首次 clone 全量）走全量重扫；**空清单零操作**。
async fn refresh_after_sync(state: &Arc<AppState>, stats: &SyncStats) {
    let change_count = stats.changed.len() + stats.removed.len();
    if change_count == 0 {
        return; // 无变更：零操作（幂等同步常见）
    }
    const FULL_RESCAN_RATIO: f64 = 0.25;

    let old = state.snapshot();
    let total = old.pages.len().max(1) as f64;
    let incremental = (change_count as f64 / total) <= FULL_RESCAN_RATIO;

    // 1) 索引更新：增量（局部重扫受影响目录）或全量重扫 → 整树替换
    if incremental {
        let cfg = state.cfg.content.clone();
        // clone_shallow（毫秒级）后锁外 merge：局部 IO 不阻塞读者
        let old_index = old.clone_shallow();
        let changed = stats.changed.clone();
        let removed = stats.removed.clone();
        let root = state.content_root.clone();
        let draft_enabled = state.cfg.content.draft;
        let result = tokio::task::spawn_blocking(move || {
            old_index.merge_changes(&cfg, &root, draft_enabled, &changed, &removed)
        })
        .await;
        match result {
            Ok(index) => {
                state.replace_index(index);
                info!(change_count, "同步增量更新索引（无全量重扫）");
            }
            Err(e) => {
                warn!(%e, "增量更新 join 失败，回退全量重扫");
                if let Some(index) = full_rescan(state).await {
                    state.replace_index(index);
                }
            }
        }
    } else if let Some(index) = full_rescan(state).await {
        state.replace_index(index);
    }

    // 2) children 连带失效（按变更文件所在目录的祖先链）
    let mut affected: std::collections::HashSet<PathBuf> = Default::default();
    for rel in stats.changed.iter().chain(stats.removed.iter()) {
        let p = PathBuf::from(rel);
        let mut dir = p.parent();
        while let Some(d) = dir {
            affected.insert(d.to_path_buf());
            dir = d.parent();
        }
    }
    for dir in affected {
        if let Some(pages) = old.children_deps.get(&dir) {
            for page in pages {
                state.cache.invalidate_page(&page.to_string_lossy());
            }
        }
    }

    // 3) 变更文件 fragment 失效（下次访问按需渲染）
    for rel in stats.changed.iter().chain(stats.removed.iter()) {
        state.cache.invalidate_page(rel);
    }

    // 4) 搜索索引按精确清单增量（changed 重插 / removed 删除；children 连带页重插）
    if let Some(si) = state.search.ready_index() {
        let snapshot = state.snapshot();
        let mut changed: Vec<PathBuf> = stats.changed.iter().map(PathBuf::from).collect();
        for pages in old.children_deps.values() {
            for p in pages {
                if !stats.removed.contains(&p.to_string_lossy().into_owned())
                    && !changed.contains(p)
                {
                    changed.push(p.clone());
                }
            }
        }
        let removed: Vec<PathBuf> = stats.removed.iter().map(PathBuf::from).collect();
        let root = state.content_root.clone();
        let draft_enabled = state.cfg.content.draft;
        let result = tokio::task::spawn_blocking(move || {
            si.apply_changes(&snapshot, &root, draft_enabled, &changed, &removed)
        })
        .await;
        if let Ok(Err(e)) = result {
            warn!(%e, "同步后搜索增量失败（下次事件/reindex 兜底）");
        }
    }
}

/// 全量重扫描 → 新 SiteIndex（失败返回 None：保留旧索引，mtime 兜底兜住）。
async fn full_rescan(state: &Arc<AppState>) -> Option<SiteIndex> {
    let cfg = state.cfg.content.clone();
    let result = tokio::task::spawn_blocking(move || {
        coral_core::scanner::scan(&cfg).map(|r| SiteIndex::build(r, cfg.draft))
    })
    .await;
    match result {
        Ok(Ok(index)) => Some(index),
        Ok(Err(e)) => {
            warn!(%e, "同步后重扫描失败，保留旧索引（mtime 兜底兜住）");
            None
        }
        Err(e) => {
            warn!(%e, "同步后重扫描 join 失败");
            None
        }
    }
}

/// 启动同步（后台线程）：clone + 首次同步 + 刷新；失败 WARN 不阻断（故障矩阵）。
/// 与 webhook 共用 syncing 标记（并发去重）。
pub fn spawn_startup_sync(state: Arc<AppState>) {
    let sync = Arc::new(GitSync::new(
        state.cfg.git.clone(),
        state.cfg.cache.dir.join("git-mirror"),
        state.content_root.clone(),
    ));
    // 启动即触发（幂等：content.root 非空时只同步增量差异，"为空则全量拉取"
    // 是同一同步的特例，无需条件分支）。观测：root 为空 + mirror 状态可在此
    // 一眼判断（此前 clone 期间 90s 静默，看似"慢半拍"）。
    info!(
        root_empty = dir_is_empty(&state.content_root),
        mirror_cloned = sync.mirror_is_cloned(),
        "git 启动同步开始"
    );
    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                warn!(%e, "git 启动同步线程 runtime 创建失败");
                return;
            }
        };
        rt.block_on(async {
            if !state.git.try_begin() {
                info!("git 同步已在进行（webhook 先到），跳过启动同步");
                return;
            }
            let start = std::time::Instant::now();
            let result = match tokio::task::spawn_blocking({
                let sync = sync.clone();
                move || sync.ensure_cloned()
            })
            .await
            {
                Ok(Ok(_)) => run_sync(&state, &sync).await,
                Ok(Err(e)) => Err(e.to_string()),
                Err(e) => Err(e.to_string()),
            };
            state.git.end(start.elapsed().as_millis() as u64);
            if let Err(e) = result {
                warn!(%e, "git 启动同步失败（服务继续；webhook 可重试）");
            }
        });
    });
}

/// 停机联动：等待 syncing 复位（上限 deadline；超时 WARN 退出，
/// 半途状态由下次启动同步的镜像 diff 自愈）。
pub async fn wait_sync_done(state: &Arc<AppState>, deadline: tokio::time::Instant) {
    while state.git.is_syncing() {
        if tokio::time::Instant::now() >= deadline {
            warn!("停机等待 git 同步超时，退出（半途状态由下次启动自愈）");
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

/// 目录是否无可见内容（隐藏条目不算——git sync 的 remove_stale 同口径）。
fn dir_is_empty(dir: &Path) -> bool {
    let Ok(mut entries) = std::fs::read_dir(dir) else {
        return true; // 目录不可读（含不存在）按空处理：同步会重建内容
    };
    !entries.any(|e| {
        e.map(|e| !e.file_name().to_string_lossy().starts_with('.'))
            .unwrap_or(true) // 读取出错的条目按"非空"处理（保守）
    })
}
