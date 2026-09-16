//! 缓存集成测试。
//!
//! 覆盖：写读 roundtrip、启动增量 diff（改 1 文件仅 1 项变化）、
//! 删 cache 全量重建、manifest 损坏降级、只读目录不 panic。

use coral_core::cache::CacheStore;
use coral_core::config::ContentConfig;
use coral_core::scanner::{PageMeta, scan};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// 扫描结果 → (rel -> PageMeta)，用真实 mtime/size 驱动缓存条目。
fn scan_all(root: &Path) -> HashMap<String, PageMeta> {
    let cfg = ContentConfig {
        root: root.to_path_buf(),
        exclude: vec![],
        draft: false,
    };
    scan(&cfg)
        .unwrap()
        .pages
        .into_iter()
        .map(|(rel, p)| (rel.to_string_lossy().into_owned(), p))
        .collect()
}

fn scan_pages(root: &Path) -> HashMap<String, (u64, u64, String)> {
    scan_all(root)
        .iter()
        .map(|(rel, p)| {
            (
                rel.clone(),
                (
                    p.mtime
                        .duration_since(SystemTime::UNIX_EPOCH)
                        .unwrap()
                        .as_millis() as u64,
                    p.size,
                    p.url.clone(),
                ),
            )
        })
        .collect()
}

fn scan_dirs(root: &Path) -> HashMap<String, u64> {
    let cfg = ContentConfig {
        root: root.to_path_buf(),
        exclude: vec![],
        draft: false,
    };
    let result = scan(&cfg).unwrap();
    result
        .dirs
        .iter()
        .map(|(rel, d)| {
            (
                rel.to_string_lossy().into_owned(),
                d.mtime
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as u64,
            )
        })
        .collect()
}

/// 构造小 content 树并返回 (tmp, root)。
fn make_content(files: &[(&str, &str)]) -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("content");
    for (rel, content) in files {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, content).unwrap();
    }
    (tmp, root)
}

#[test]
fn test_store_and_lookup_roundtrip() {
    let tmp = tempfile::tempdir().unwrap();
    let store = CacheStore::open(&tmp.path().join("cache"));
    let mtime = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);

    store
        .store_page("a.md", "/a", mtime, 42, "<p>hello</p>", false)
        .unwrap();

    let lookup = store.lookup_page("a.md").unwrap();
    assert!(lookup.fragment_exists);
    assert_eq!(lookup.entry.url, "/a");
    assert_eq!(lookup.entry.size, 42);

    let html = store.read_page_fragment("a.md").unwrap().unwrap();
    assert_eq!(html, "<p>hello</p>");

    // 失效后：条目与文件均消失
    store.invalidate_page("a.md");
    assert!(store.lookup_page("a.md").is_none());
    assert!(store.read_page_fragment("a.md").unwrap().is_none());
}

#[test]
fn test_startup_incremental_diff_single_change() {
    let (tmp, root) = make_content(&[("a.md", "内容 A"), ("b.md", "内容 B"), ("c.md", "内容 C")]);

    // 第一次启动：全量（空 manifest diff 全部为 added）
    let cache_dir = tmp.path().join("cache");
    let store = CacheStore::open(&cache_dir);
    store.load_manifest();
    let diff = store.diff_against_scan(&scan_pages(&root), &scan_dirs(&root));
    assert_eq!(diff.added.len(), 3);
    assert!(diff.changed.is_empty());

    // 模拟渲染落盘：用扫描到的真实 mtime/size 写入 manifest
    for (rel, page) in &scan_all(&root) {
        store
            .store_page(rel, &page.url, page.mtime, page.size, "<p>x</p>", false)
            .unwrap();
    }
    store.flush().unwrap();

    // 重启（新 CacheStore 实例）：改 1 个文件 → diff 仅 1 项变化
    // （fs::write 自然更新 mtime；内容也变，size 可能相同，mtime 必变）
    std::fs::write(root.join("b.md"), "内容 B 已修改，长度不同").unwrap();
    let store2 = CacheStore::open(&cache_dir);
    assert!(store2.load_manifest(), "manifest 应可加载");
    let diff2 = store2.diff_against_scan(&scan_pages(&root), &scan_dirs(&root));
    assert_eq!(diff2.changed.len(), 1, "仅 1 项变化：{diff2:?}");
    assert_eq!(diff2.changed[0], "b.md");
    assert!(diff2.added.is_empty());
    assert!(diff2.removed.is_empty());
}

#[test]
fn test_add_and_remove_files_reflected() {
    let (tmp, root) = make_content(&[("a.md", "A"), ("b.md", "B")]);
    let cache_dir = tmp.path().join("cache");
    let store = CacheStore::open(&cache_dir);
    store.load_manifest();
    for (rel, page) in &scan_all(&root) {
        store
            .store_page(rel, &page.url, page.mtime, page.size, "<p>x</p>", false)
            .unwrap();
    }

    // 新增 c.md、删除 b.md
    std::fs::write(root.join("c.md"), "C").unwrap();
    std::fs::remove_file(root.join("b.md")).unwrap();

    let diff = store.diff_against_scan(&scan_pages(&root), &scan_dirs(&root));
    assert_eq!(diff.added, vec!["c.md"]);
    assert_eq!(diff.removed, vec!["b.md"]);
    assert!(diff.changed.is_empty());
    // removed 的 fragment 被清理
    assert!(store.lookup_page("b.md").is_none());
}

#[test]
fn test_deleted_cache_dir_full_rebuild() {
    let tmp = tempfile::tempdir().unwrap();
    let cache_dir = tmp.path().join("cache");
    {
        let store = CacheStore::open(&cache_dir);
        store
            .store_page("a.md", "/a", SystemTime::UNIX_EPOCH, 1, "<p>a</p>", false)
            .unwrap();
        store.flush().unwrap();
    }
    // 删 cache 重启：load_manifest 失败 → 空 manifest → 全量重建路径
    std::fs::remove_dir_all(&cache_dir).unwrap();
    let store2 = CacheStore::open(&cache_dir);
    assert!(!store2.load_manifest());
    assert_eq!(store2.page_count(), 0);
    // 重新写入可恢复
    store2
        .store_page("a.md", "/a", SystemTime::UNIX_EPOCH, 1, "<p>a</p>", false)
        .unwrap();
    assert_eq!(store2.page_count(), 1);
}

#[test]
fn test_corrupted_manifest_degrades_to_rebuild() {
    let tmp = tempfile::tempdir().unwrap();
    let cache_dir = tmp.path().join("cache");
    std::fs::create_dir_all(&cache_dir).unwrap();
    // 损坏 JSON
    std::fs::write(cache_dir.join("manifest.json"), "{ broken json").unwrap();
    let store = CacheStore::open(&cache_dir);
    assert!(!store.load_manifest());
    assert_eq!(store.page_count(), 0);
    // 版本不符
    std::fs::write(
        cache_dir.join("manifest.json"),
        r#"{"version": 999, "pages": {}, "trees": {}}"#,
    )
    .unwrap();
    let store2 = CacheStore::open(&cache_dir);
    assert!(!store2.load_manifest());
    assert_eq!(store2.page_count(), 0);
}

#[test]
fn test_flush_persists_and_survives_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let cache_dir = tmp.path().join("cache");
    let mtime = SystemTime::UNIX_EPOCH + Duration::from_secs(1234);
    {
        let store = CacheStore::open(&cache_dir);
        store
            .store_page("x/y.md", "/x/y", mtime, 77, "<p>y</p>", true)
            .unwrap();
        store
            .store_tree("x", mtime, 0, "[{\"title\":\"t\"}]")
            .unwrap();
        store.flush().unwrap();
    }
    let store2 = CacheStore::open(&cache_dir);
    assert!(store2.load_manifest());
    let lookup = store2.lookup_page("x/y.md").unwrap();
    assert!(lookup.fragment_exists);
    assert_eq!(lookup.entry.size, 77);
    assert!(lookup.entry.has_children_shortcode);
    assert_eq!(store2.lookup_tree("x").unwrap(), "[{\"title\":\"t\"}]");
}

#[test]
fn test_readonly_cache_dir_write_fails_but_no_panic() {
    // 故障矩阵：缓存目录不可写 → WARN + 服务继续（内存是真相源）
    let tmp = tempfile::tempdir().unwrap();
    let cache_dir = tmp.path().join("cache");
    std::fs::create_dir_all(&cache_dir).unwrap();
    // 同目录建只读子目录作为 cache 根（在根上 chmod 在 macOS 上不可靠）
    let readonly = tmp.path().join("ro");
    std::fs::create_dir_all(&readonly).unwrap();
    let mut perms = std::fs::metadata(&readonly).unwrap().permissions();
    #[allow(clippy::permissions_set_readonly_false)] // 测试后恢复
    perms.set_readonly(true);
    std::fs::set_permissions(&readonly, perms).unwrap();

    let store = CacheStore::open(&readonly);
    let result = store.store_page("a.md", "/a", SystemTime::UNIX_EPOCH, 1, "<p>a</p>", false);
    assert!(result.is_err(), "只读目录写入应报错");
    // 不 panic，后续 lookup 仍正常（内存态）
    assert!(store.lookup_page("a.md").is_none());

    // 恢复权限（TempDir 清理需要）
    #[allow(clippy::permissions_set_readonly_false)] // 测试收尾恢复可写
    {
        let mut perms = std::fs::metadata(&readonly).unwrap().permissions();
        perms.set_readonly(false);
        std::fs::set_permissions(&readonly, perms).unwrap();
    }
}

#[test]
fn test_tree_invalidation_and_etag_semantics() {
    let tmp = tempfile::tempdir().unwrap();
    let store = CacheStore::open(&tmp.path().join("cache"));
    let mtime = SystemTime::UNIX_EPOCH + Duration::from_secs(10);
    store
        .store_tree("guide", mtime, 0, "{\"children\":[]}")
        .unwrap();
    assert!(store.lookup_tree("guide").is_some());
    store.invalidate_tree("guide");
    assert!(store.lookup_tree("guide").is_none());

    // 目录 mtime 变化 → diff 后树条目被清理
    // （目录 mtime 由直接子项增删自然改变，不人为设置）
    let (tmp2, root) = make_content(&[("guide/a.md", "A")]);
    let store2 = CacheStore::open(&tmp.path().join("cache2"));
    store2.load_manifest();
    let _ = store2.diff_against_scan(&scan_pages(&root), &scan_dirs(&root));
    let dir_mtime = scan_dirs(&root)["guide"];
    store2
        .store_tree(
            "guide",
            scan(&ContentConfig {
                root: root.clone(),
                exclude: vec![],
                draft: false,
            })
            .unwrap()
            .dirs[Path::new("guide")]
            .mtime,
            0,
            "{}",
        )
        .unwrap();
    assert!(store2.lookup_tree("guide").is_some());
    // 新增子文件 → 目录 mtime 自然变化 → diff 清理树条目
    std::fs::write(root.join("guide/b.md"), "B").unwrap();
    let new_dir_mtime = scan_dirs(&root)["guide"];
    assert_ne!(dir_mtime, new_dir_mtime, "目录 mtime 应随子项新增而变化");
    let _ = store2.diff_against_scan(&scan_pages(&root), &scan_dirs(&root));
    assert!(
        store2.lookup_tree("guide").is_none(),
        "目录 mtime 变化后树缓存应清理"
    );
    drop(tmp2);
}
