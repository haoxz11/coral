//! Watcher 与失效链。
//!
//! statfs 自适应 event/poll 双模式；300ms 固定防抖窗口（A4）；
//! 批处理：同步重建 SiteIndex + 目录树 + children 连带失效，
//! 异步删 fragment；批次完成 manifest 写回。

use crate::state::AppState;
use coral_core::config::ContentConfig;
use coral_core::scanner::{self, SiteIndex};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc as std_mpsc;
use std::time::Duration;
use tokio::sync::mpsc as tokio_mpsc;
use tracing::{debug, info, warn};

/// 监听模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchMode {
    /// 本地盘：notify RecommendedWatcher（FSEvents/inotify）
    Event,
    /// 网络挂载：PollWatcher 轮询 mtime+size
    Poll,
}

/// statfs 判定网络/同步文件系统集合（不依赖真实挂载，供单测）。
/// 仅 darwin 的 f_fstypename 分支使用；linux 走 f_type magic。
#[cfg(any(target_os = "macos", test))]
fn is_network_fs(fstype: &str) -> bool {
    matches!(
        fstype,
        // darwin fstypename
        "nfs" | "smbfs" | "afpfs" | "exfat" | "osxfuse" | "webdav"
    )
}

/// linux f_type magic 集。macOS 走 fstypename 分支，此函数仅 linux 使用。
#[cfg(any(target_os = "linux", test))]
fn is_network_fs_magic(magic: u64) -> bool {
    matches!(
        magic,
        0x6969        // NFS
        | 0xff534d42  // CIFS/SMB (SMB2 magic)
        | 0x517B      // SMB legacy
        | 0x65735546  // FUSE
        | 0x7461636f // OCFS2
    )
}

/// 检测监听模式：
/// 1. 环境变量 CORAL_WATCHER=event|poll 强制覆盖
/// 2. statfs 判文件系统类型：网络/同步挂载 → Poll；本地盘 → Event
pub fn detect_mode(root: &Path) -> WatchMode {
    if let Ok(v) = std::env::var("CORAL_WATCHER") {
        match v.as_str() {
            "event" => return WatchMode::Event,
            "poll" => return WatchMode::Poll,
            _ => warn!(value = %v, "CORAL_WATCHER 取值非法（应为 event|poll），忽略"),
        }
    }
    match nix::sys::statfs::statfs(root) {
        Ok(fs) => {
            // darwin: f_fstypename 字符串；linux: f_type magic（FsType(pub u64)）
            #[cfg(target_os = "macos")]
            let (is_network, name) = {
                let name = fs.filesystem_type_name();
                (is_network_fs(name), name.to_string())
            };
            #[cfg(not(target_os = "macos"))]
            let (is_network, name) = {
                let magic = fs.filesystem_type().0 as u64;
                (is_network_fs_magic(magic), format!("{magic:x}"))
            };
            if is_network {
                info!(fstype = %name, "网络/同步文件系统，降级 PollWatcher");
                WatchMode::Poll
            } else {
                info!(fstype = %name, "本地文件系统，使用事件监听");
                WatchMode::Event
            }
        }
        Err(e) => {
            warn!(%e, "statfs 失败，默认事件监听");
            WatchMode::Event
        }
    }
}

/// 防抖窗口（A4：首个事件起算固定窗口，非滑动）。
pub const DEBOUNCE: Duration = Duration::from_millis(300);

/// PollWatcher 轮询间隔。
const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// 降级兜底轮询间隔（故障矩阵：watcher init 失败后每 10s 全量 stat）。
pub const FALLBACK_POLL_INTERVAL: Duration = Duration::from_secs(10);

/// watcher 事件（防抖合并后的批处理输入）。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WatchEvent {
    /// 相对 content 根的路径
    pub rel_path: PathBuf,
    pub kind: EventKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EventKind {
    /// 文件内容修改/保存
    Modify,
    /// 新文件/目录
    Create,
    /// 删除
    Remove,
}

/// watcher 生命周期句柄：drop 时停止监听并结束桥接线程
/// （生产中进程退出自然回收；测试需要显式结束 spawn_blocking 线程，
/// 否则 runtime drop 等待 blocking 池死锁）。
pub struct WatchHandle {
    /// 发送端 drop → 桥接线程 recv Err 退出
    _stop_tx: std_mpsc::Sender<()>,
    watcher: Option<Box<dyn notify::Watcher + Send>>,
}

impl Drop for WatchHandle {
    fn drop(&mut self) {
        self.watcher = None; // unwatch + 停事件源
        // _stop_tx drop 由结构体自然完成
    }
}

/// 启动 watcher。初始化失败不拒绝启动（故障矩阵）：
/// WARN + 降级为 mtime 兜底轮询。返回句柄（调用方持有至进程生命周期）。
pub fn spawn_watcher(state: Arc<AppState>) -> Option<WatchHandle> {
    let mode = detect_mode(&state.content_root);
    info!(?mode, "watcher 启动");

    let (tx, mut rx) = tokio_mpsc::unbounded_channel::<WatchEvent>();
    let root = state.content_root.clone();
    let content_cfg = ContentConfig {
        root: root.clone(),
        exclude: state.cfg.content.exclude.clone(),
        draft: state.cfg.content.draft,
    };

    let watcher_result = match mode {
        WatchMode::Event => spawn_event_watcher(root, tx.clone()),
        WatchMode::Poll => spawn_poll_watcher(root, tx.clone()),
    };
    match watcher_result {
        Ok(handle) => {
            // 防抖 + 批处理循环（tokio 任务随通道关闭自然结束）
            tokio::spawn(async move {
                while let Some(first) = rx.recv().await {
                    // 固定 300ms 窗口（A4）：窗口内事件合并，窗口结束批量处理
                    let deadline = tokio::time::Instant::now() + DEBOUNCE;
                    let mut batch: Vec<WatchEvent> = vec![first];
                    loop {
                        let now = tokio::time::Instant::now();
                        if now >= deadline {
                            break;
                        }
                        match tokio::time::timeout(deadline - now, rx.recv()).await {
                            Ok(Some(ev)) => batch.push(ev),
                            Ok(None) => break,
                            Err(_) => break, // 窗口到期
                        }
                    }
                    let start = std::time::Instant::now();
                    let files = batch
                        .iter()
                        .filter(|e| e.rel_path.extension().is_some_and(|x| x == "md"))
                        .count();
                    let dirs = batch.len() - files;
                    process_batch(&state, &content_cfg, batch).await;
                    info!(
                        files,
                        dirs,
                        elapsed_ms = start.elapsed().as_millis() as u64,
                        "watch 事件批处理完成"
                    );
                }
            });
            Some(handle)
        }
        Err(e) => {
            warn!(
                %e,
                "watcher 初始化失败，降级为 mtime 兜底轮询（每 10s 全量 stat，故障矩阵）"
            );
            drop(tx); // 无 watcher：关闭事件通道，防抖循环不启动
            spawn_fallback_poll(state, content_cfg);
            None
        }
    }
}

/// notify 事件种类 → 内部事件种类。
///
/// `Access`（文件被打开/读取）与未知种类一律丢弃：扫描/渲染/搜索都会读全部
/// md 文件，把读取当修改会形成"扫描 → Access 事件 → 再扫描"的自激励死循环
/// （2026-09-15 线上事故）。漏掉的真变更由 PollWatcher mtime 比对与 git
/// 同步主动刷新兜底。
fn map_event_kind(kind: notify::EventKind) -> Option<EventKind> {
    match kind {
        notify::EventKind::Create(_) => Some(EventKind::Create),
        notify::EventKind::Modify(_) => Some(EventKind::Modify),
        notify::EventKind::Remove(_) => Some(EventKind::Remove),
        _ => None,
    }
}

/// 桥接线程公共体：std mpsc → tokio mpsc。
/// 退出条件：发送端（watcher/stop_tx）全部 drop → recv Err。
/// 用 recv() 阻塞（线程常驻），由 WatchHandle drop 保证停止。
fn bridge_loop(
    std_rx: std_mpsc::Receiver<notify::Result<notify::Event>>,
    root: PathBuf,
    tx: tokio_mpsc::UnboundedSender<WatchEvent>,
) {
    loop {
        match std_rx.recv() {
            Ok(Ok(event)) => {
                let kind = match map_event_kind(event.kind) {
                    Some(kind) => kind,
                    None => continue, // Access（读取）/未知种类：不是变更
                };
                for path in event.paths {
                    // 绝对路径 → rel_path（根外事件丢弃）
                    if let Ok(rel) = path.strip_prefix(&root) {
                        let _ = tx.send(WatchEvent {
                            rel_path: rel.to_path_buf(),
                            kind,
                        });
                    }
                }
            }
            Ok(Err(e)) => warn!(%e, "watcher 事件错误"),
            Err(_) => {
                // 发送端 dropped：退出桥接线程
                return;
            }
        }
    }
}

fn spawn_event_watcher(
    root: PathBuf,
    tx: tokio_mpsc::UnboundedSender<WatchEvent>,
) -> notify::Result<WatchHandle> {
    use notify::Watcher;
    let (std_tx, std_rx) = std_mpsc::channel::<notify::Result<notify::Event>>();
    let mut watcher = notify::recommended_watcher(std_tx)?;
    watcher.watch(&root, notify::RecursiveMode::Recursive)?;
    info!("RecommendedWatcher 已挂载（递归）");

    let root2 = root.clone();
    tokio::task::spawn_blocking(move || bridge_loop(std_rx, root2, tx));
    let (_stop_tx, _stop_rx) = std_mpsc::channel::<()>();
    // watcher 装箱 trait 对象（handle drop 时 drop → unwatch + 发送端关闭）
    let boxed: Box<dyn notify::Watcher + Send> = Box::new(watcher);
    Ok(WatchHandle {
        _stop_tx,
        watcher: Some(boxed),
    })
}

fn spawn_poll_watcher(
    root: PathBuf,
    tx: tokio_mpsc::UnboundedSender<WatchEvent>,
) -> notify::Result<WatchHandle> {
    use notify::Watcher;
    let (std_tx, std_rx) = std_mpsc::channel::<notify::Result<notify::Event>>();
    let config = notify::Config::default()
        .with_poll_interval(POLL_INTERVAL)
        .with_compare_contents(false); // 只 stat mtime+size（风险表）
    let mut watcher = notify::PollWatcher::new(std_tx, config)?;
    watcher.watch(&root, notify::RecursiveMode::Recursive)?;
    info!("PollWatcher 已挂载（interval 2s）");

    let root2 = root.clone();
    tokio::task::spawn_blocking(move || bridge_loop(std_rx, root2, tx));
    let (_stop_tx, _stop_rx) = std_mpsc::channel::<()>();
    let boxed: Box<dyn notify::Watcher + Send> = Box::new(watcher);
    Ok(WatchHandle {
        _stop_tx,
        watcher: Some(boxed),
    })
}

/// 降级兜底轮询（故障矩阵）：每 10s 全量重扫对比 mtime+size，
/// 变化项当作一批 Modify/Create/Remove 事件处理。
fn spawn_fallback_poll(state: Arc<AppState>, content_cfg: ContentConfig) {
    tokio::spawn(async move {
        let mut last_snapshot: std::collections::HashMap<PathBuf, (std::time::SystemTime, u64)> =
            match scanner::scan(&content_cfg) {
                Ok(r) => r
                    .pages
                    .into_iter()
                    .map(|(rel, p)| (rel, (p.mtime, p.size)))
                    .collect(),
                Err(e) => {
                    warn!(%e, "兜底轮询首次扫描失败，10s 后重试");
                    std::collections::HashMap::new()
                }
            };
        loop {
            tokio::time::sleep(FALLBACK_POLL_INTERVAL).await;
            let Ok(result) = scanner::scan(&content_cfg) else {
                warn!("兜底轮询扫描失败（content 根不可读？），继续重试");
                continue;
            };
            let mut batch = Vec::new();
            let current: std::collections::HashMap<PathBuf, (std::time::SystemTime, u64)> = result
                .pages
                .iter()
                .map(|(rel, p)| (rel.clone(), (p.mtime, p.size)))
                .collect();
            for (rel, (mtime, size)) in &current {
                match last_snapshot.get(rel) {
                    Some((m, s)) if m == mtime && s == size => {}
                    Some(_) => batch.push(WatchEvent {
                        rel_path: rel.clone(),
                        kind: EventKind::Modify,
                    }),
                    None => batch.push(WatchEvent {
                        rel_path: rel.clone(),
                        kind: EventKind::Create,
                    }),
                }
            }
            for rel in last_snapshot.keys() {
                if !current.contains_key(rel) {
                    batch.push(WatchEvent {
                        rel_path: rel.clone(),
                        kind: EventKind::Remove,
                    });
                }
            }
            last_snapshot = current;
            if !batch.is_empty() {
                let start = std::time::Instant::now();
                process_batch(&state, &content_cfg, batch).await;
                info!(
                    elapsed_ms = start.elapsed().as_millis() as u64,
                    "兜底轮询批处理完成"
                );
            }
        }
    });
}

/// 批处理（失效链）：
/// - .md 变更：同步重扫（整树替换）+ children 连带失效；异步删 fragment
/// - 目录事件：同 .md 同步部分（作用于父目录树）
/// - 非 .md：无操作
/// - 完成：manifest 写回
async fn process_batch(state: &Arc<AppState>, content_cfg: &ContentConfig, batch: Vec<WatchEvent>) {
    let has_md_or_dir = batch.iter().any(|e| {
        e.rel_path.extension().is_some_and(|x| x == "md") || e.rel_path.extension().is_none()
    });
    if !has_md_or_dir {
        return;
    }

    // git 同步进行中：不逐批重扫（大同步会撕裂防抖窗口），
    // 记 dirty；同步完成后 refresh_after_sync 统一处理
    if state.git.is_syncing() {
        state.git.mark_dirty();
        return;
    }

    // 同步：重扫描 → 整树替换（写时复制）
    let cfg = content_cfg.clone();
    let new_index = tokio::task::spawn_blocking(move || {
        scanner::scan(&cfg).map(|r| SiteIndex::build(r, cfg.draft))
    })
    .await;
    match new_index {
        Ok(Ok(index)) => {
            let old_deps = state.snapshot();
            // children 连带失效：事件目录的 children_deps 页面 fragment 删除
            invalidate_children_deps(state, &old_deps, &batch);
            state.replace_index(index);
        }
        Ok(Err(e)) => warn!(%e, "事件批重扫描失败，保留旧索引（mtime 兜底兜住）"),
        Err(e) => warn!(%e, "事件批重扫描 join 失败"),
    }

    // 异步：删除变化文件的 fragment（下次访问按需渲染）+ 搜索索引增量维护
    let state2 = state.clone();
    tokio::spawn(async move {
        let mut changed: Vec<PathBuf> = Vec::new();
        let mut removed: Vec<PathBuf> = Vec::new();
        for ev in &batch {
            if ev.rel_path.extension().is_some_and(|x| x == "md") {
                let rel = ev.rel_path.to_string_lossy().into_owned();
                state2.cache.invalidate_page(&rel);
                match ev.kind {
                    EventKind::Remove => removed.push(ev.rel_path.clone()),
                    _ => changed.push(ev.rel_path.clone()),
                }
            }
        }
        // 搜索增量（仅开启且就绪时）；children 连带失效的页面一并重插
        // （其正文含 children 列表，索引内容需刷新）
        if let Some(si) = state2.search.ready_index() {
            let mut all_changed = changed.clone();
            {
                let snapshot = state2.snapshot();
                let mut dirs: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
                for ev in &batch {
                    let mut dir = ev.rel_path.parent();
                    while let Some(d) = dir {
                        dirs.insert(d.to_path_buf());
                        dir = d.parent();
                    }
                }
                for dir in dirs {
                    if let Some(pages) = snapshot.children_deps.get(&dir) {
                        for p in pages {
                            if !all_changed.contains(p) && !removed.contains(p) {
                                all_changed.push(p.clone());
                            }
                        }
                    }
                }
            }
            let root = state2.content_root.clone();
            let snapshot = state2.snapshot();
            let draft_enabled = state2.cfg.content.draft;
            let result = tokio::task::spawn_blocking(move || {
                si.apply_changes(&snapshot, &root, draft_enabled, &all_changed, &removed)
            })
            .await;
            if let Ok(Err(e)) = result {
                warn!(%e, "搜索索引增量更新失败（下次全量重建兜底）");
            }
        }
        // 批次完成：manifest 写回
        if let Err(e) = state2.cache.flush() {
            warn!(%e, "批处理后 manifest 写回失败");
        }
    });
}

/// children 祖先链连带失效：
/// 事件路径所在目录及其祖先的 children_deps 页面 fragment 删除。
fn invalidate_children_deps(state: &Arc<AppState>, index: &SiteIndex, batch: &[WatchEvent]) {
    let mut affected_dirs: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    for ev in batch {
        let mut dir = ev.rel_path.parent();
        while let Some(d) = dir {
            affected_dirs.insert(d.to_path_buf());
            dir = d.parent();
        }
    }
    for dir in affected_dirs {
        if let Some(pages) = index.children_deps.get(&dir) {
            for page in pages {
                let rel = page.to_string_lossy().into_owned();
                state.cache.invalidate_page(&rel);
                debug!(
                    dir = %dir.display(),
                    page = %page.display(),
                    "children 连带失效"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_network_fs_names() {
        for name in ["nfs", "smbfs", "afpfs", "exfat", "osxfuse", "webdav"] {
            assert!(is_network_fs(name), "{name} 应判定为网络文件系统");
        }
        for name in ["apfs", "ext4", "hfs", "xfs", "btrfs", "tmpfs"] {
            assert!(!is_network_fs(name), "{name} 不应判定为网络文件系统");
        }
    }

    #[test]
    fn test_network_fs_magic() {
        assert!(is_network_fs_magic(0x6969), "NFS");
        assert!(is_network_fs_magic(0xff534d42), "CIFS");
        assert!(is_network_fs_magic(0x517B), "SMB legacy");
        assert!(is_network_fs_magic(0x65735546), "FUSE");
        assert!(!is_network_fs_magic(0x2000), "ext4 magic 不是网络盘");
        assert!(!is_network_fs_magic(0), "0 不是网络盘");
    }

    #[test]
    fn test_map_event_kind_access_and_unknown_dropped() {
        use notify::event::{
            AccessKind, AccessMode, CreateKind, DataChange, ModifyKind, RemoveKind,
        };
        // Access（读取）必须丢弃：读取不是变更（自激励死循环根因，回归库）
        assert_eq!(
            map_event_kind(notify::EventKind::Access(AccessKind::Open(AccessMode::Any))),
            None
        );
        assert_eq!(
            map_event_kind(notify::EventKind::Access(AccessKind::Close(
                AccessMode::Read
            ))),
            None
        );
        assert_eq!(
            map_event_kind(notify::EventKind::Access(AccessKind::Close(
                AccessMode::Write
            ))),
            None
        );
        // 真实变更正常映射
        assert_eq!(
            map_event_kind(notify::EventKind::Create(CreateKind::File)),
            Some(EventKind::Create)
        );
        assert_eq!(
            map_event_kind(notify::EventKind::Modify(ModifyKind::Data(DataChange::Any))),
            Some(EventKind::Modify)
        );
        assert_eq!(
            map_event_kind(notify::EventKind::Remove(RemoveKind::File)),
            Some(EventKind::Remove)
        );
        // 未知种类丢弃（不再兜底成 Modify）
        assert_eq!(map_event_kind(notify::EventKind::Other), None);
    }

    #[test]
    fn test_detect_mode_env_override() {
        // 不依赖真实挂载：环境变量覆盖路径（tempdir 本地盘）。
        // env::set_var 在 edition 2024 是 unsafe（多线程风险）——测试
        // 进程内无并发改 env，且本测试独占该变量
        let tmp = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("CORAL_WATCHER", "poll");
        }
        assert_eq!(detect_mode(tmp.path()), WatchMode::Poll);
        unsafe {
            std::env::set_var("CORAL_WATCHER", "event");
        }
        assert_eq!(detect_mode(tmp.path()), WatchMode::Event);
        unsafe {
            std::env::set_var("CORAL_WATCHER", "garbage");
        }
        // 非法值忽略，回退 statfs（tempdir 在 mac 上是 apfs → Event）
        assert_eq!(detect_mode(tmp.path()), WatchMode::Event);
        unsafe {
            std::env::remove_var("CORAL_WATCHER");
        }
    }
}
