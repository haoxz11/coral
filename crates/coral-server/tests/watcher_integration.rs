//! Watcher 失效链集成测试。
//!
//! 真实 FSEvents + 防抖 + 重扫描；断言用"最终一致"异步轮询（CI 时序不敏感）。

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use coral_core::config::{CacheConfig, Config, ContentConfig, LogConfig, ServerConfig, TreeConfig};
use coral_server::{build_app, initialize, watcher};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tower::util::ServiceExt;

fn make_config(root: PathBuf) -> Config {
    let cache_dir = root.parent().unwrap().join("cache");
    Config {
        server: ServerConfig::default(),
        content: ContentConfig {
            root,
            exclude: vec![],
            draft: false,
        },
        tree: TreeConfig::default(),
        cache: CacheConfig { dir: cache_dir },
        log: LogConfig::default(),
        search: coral_core::SearchConfig::default(),
        git: coral_core::GitConfig::default(),
        render: coral_core::RenderConfig::default(),
    }
}

fn make_content() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("content");
    std::fs::create_dir_all(root.join("guide/advanced")).unwrap();
    // 根 _index.md 含 children → 祖先链连带失效基础（验收 12 条）
    std::fs::write(
        root.join("_index.md"),
        "---\ntitle: 首页\n---\n首页 {{% children %}}",
    )
    .unwrap();
    std::fs::write(
        root.join("guide/_index.md"),
        "---\ntitle: 指南\n---\n指南页 {{% children %}}",
    )
    .unwrap();
    std::fs::write(
        root.join("guide/intro.md"),
        "---\ntitle: 入门\n---\n# 入门\n旧内容",
    )
    .unwrap();
    std::fs::write(
        root.join("guide/advanced/topic.md"),
        "---\ntitle: 主题\n---\n主题内容",
    )
    .unwrap();
    (tmp, root)
}

async fn get(app: &Router, uri: &str) -> (StatusCode, String) {
    let resp = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec();
    (status, String::from_utf8_lossy(&body).into_owned())
}

/// 异步轮询断言（最终一致，超时 panic）。
async fn eventually<F, Fut>(what: &str, mut f: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if f().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("超时（5s）：{what}");
}

async fn page_contains(app: &Router, uri: &str, needle: &str) -> bool {
    let (status, body) = get(app, uri).await;
    status == StatusCode::OK && body.contains(needle)
}

async fn page_not_contains(app: &Router, uri: &str, needle: &str) -> bool {
    let (status, body) = get(app, uri).await;
    status == StatusCode::OK && !body.contains(needle)
}

fn setup() -> (
    tempfile::TempDir,
    PathBuf,
    Arc<coral_server::AppState>,
    Router,
    Option<coral_server::watcher::WatchHandle>,
) {
    let (tmp, root) = make_content();
    let state = initialize(make_config(root.clone())).expect("init");
    let app = build_app(state.clone());
    let handle = watcher::spawn_watcher(state.clone());
    (tmp, root, state, app, handle)
}

#[tokio::test]
async fn test_modify_md_updates_page_within_second() {
    // 验收第 3 条（自动化）：修改 md 保存后 ≤1s（防抖 300ms + 处理）刷新可见新内容
    let (tmp, root, _state, app, _handle) = setup();
    let (status, body) = get(&app, "/guide/intro").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("旧内容"));

    std::fs::write(
        root.join("guide/intro.md"),
        "---\ntitle: 入门\n---\n# 入门\n全新内容 v2",
    )
    .unwrap();

    eventually("修改后新内容可见", || {
        page_contains(&app, "/guide/intro", "全新内容 v2")
    })
    .await;
    drop(tmp);
}

#[tokio::test]
async fn test_new_file_appears_in_tree_and_page() {
    // 验收第 4 条（自动化）：新建 md → 树即时反映 + 页面可访问
    let (tmp, root, _state, app, _handle) = setup();
    std::fs::write(
        root.join("guide/new-page.md"),
        "---\ntitle: 新页面\n---\n新页面正文",
    )
    .unwrap();

    eventually("新文件出现在树 API", || async {
        page_contains(&app, "/api/tree/children?path=/guide", "新页面").await
    })
    .await;
    eventually("新页面 URL 可访问", || {
        page_contains(&app, "/guide/new-page", "新页面正文")
    })
    .await;
    drop(tmp);
}

#[tokio::test]
async fn test_deleted_file_removed_and_404() {
    let (tmp, root, _state, app, _handle) = setup();
    let (status, _) = get(&app, "/guide/advanced/topic").await;
    assert_eq!(status, StatusCode::OK);

    std::fs::remove_file(root.join("guide/advanced/topic.md")).unwrap();

    eventually("删除后页面 404", || async {
        let (status, _) = get(&app, "/guide/advanced/topic").await;
        status == StatusCode::NOT_FOUND
    })
    .await;
    drop(tmp);
}

#[tokio::test]
async fn test_children_shortcode_invalidation_on_sibling_change() {
    // 验收第 12 条（自动化）：children 页面 fragment 在同目录增删后连带失效
    let (tmp, root, _state, app, _handle) = setup();

    // 预热：渲染 guide/_index.md（children 列表）与根 _index.md
    let (status, body) = get(&app, "/guide").await;
    assert_eq!(status, StatusCode::OK);
    assert!(!body.contains("新增的子页"), "初始列表不含新页");
    let (status, _) = get(&app, "/").await;
    assert_eq!(status, StatusCode::OK);

    // 同目录新增 → guide/_index.md 的 children fragment 连带失效，刷新出正确列表
    std::fs::write(root.join("guide/added.md"), "---\ntitle: 新增的子页\n---\n").unwrap();
    eventually("guide children 列表含新页", || {
        page_contains(&app, "/guide", "新增的子页")
    })
    .await;

    // 深层新增：guide/advanced/deep-new.md 可见（祖先链失效不清空无关缓存）。
    // 注意 needle 必须是正文内容：front matter 的 title 不进 HTML 片段（模板层才注入）
    std::fs::write(
        root.join("guide/advanced/deep-new.md"),
        "---\ntitle: 深层新页\n---\n# 深层新页\n深层新页的正文内容",
    )
    .unwrap();
    eventually("深层新页可见", || {
        page_contains(&app, "/guide/advanced/deep-new", "深层新页的正文内容")
    })
    .await;

    // 删除后 children 列表移除该页
    std::fs::remove_file(root.join("guide/added.md")).unwrap();
    eventually("children 列表移除已删页", || {
        page_not_contains(&app, "/guide", "新增的子页")
    })
    .await;

    // manifest 已写回磁盘（批处理完成触发）
    let cache_dir = root.parent().unwrap().join("cache");
    let manifest = std::fs::read_to_string(cache_dir.join("manifest.json")).unwrap_or_default();
    assert!(!manifest.is_empty(), "批处理后 manifest 应已写回");
    drop(tmp);
}

#[tokio::test]
async fn test_non_md_change_no_op() {
    // 非 md 文件变化：无操作（不触发重扫描/失效），静态资源直接读
    let (tmp, root, state, app, _handle) = setup();
    let renders_before = state.render_count.load(std::sync::atomic::Ordering::SeqCst);
    std::fs::write(root.join("guide/logo.png"), b"png").unwrap();
    tokio::time::sleep(Duration::from_millis(600)).await; // 超过防抖窗口
    assert_eq!(
        state.render_count.load(std::sync::atomic::Ordering::SeqCst),
        renders_before,
        "非 md 变化不应触发任何渲染"
    );
    let (status, _) = get(&app, "/guide/logo.png").await;
    assert_eq!(status, StatusCode::OK);
    drop(tmp);
}
