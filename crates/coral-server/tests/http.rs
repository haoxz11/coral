//! HTTP 层测试：axum oneshot。
//!
//! 覆盖：页面 200/404/draft-404/permalink、ETag 304、路径安全回归
//! （穿越变体全 404）、静态资源 mime/immutable、探针、并发去重、SWR。

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use coral_core::config::{CacheConfig, Config, ContentConfig, LogConfig, ServerConfig, TreeConfig};
use coral_server::{build_app, initialize};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use tower::util::ServiceExt;

fn make_config(root: PathBuf, exclude: &[&str], draft: bool) -> Config {
    // 缓存目录独立 tempdir（测试不写共享目录；cache 与 content 同一 tmp 下）
    let cache_dir = root.parent().expect("root 有父目录").join("cache");
    Config {
        server: ServerConfig::default(),
        content: ContentConfig {
            root,
            exclude: exclude.iter().map(PathBuf::from).collect(),
            draft,
        },
        tree: TreeConfig::default(),
        cache: CacheConfig { dir: cache_dir },
        log: LogConfig::default(),
        search: coral_core::SearchConfig::default(),
        git: coral_core::GitConfig::default(),
        render: coral_core::RenderConfig::default(),
    }
}

/// 构造测试 content 树（tempdir），返回 (tmp, root)。
fn make_content() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("content");
    std::fs::create_dir_all(root.join("guide")).unwrap();
    std::fs::create_dir_all(root.join("docs/deep")).unwrap();
    std::fs::create_dir_all(root.join("参考")).unwrap();
    std::fs::create_dir_all(root.join(".hidden")).unwrap();
    std::fs::write(
        root.join("guide/_index.md"),
        "---\ntitle: 指南\n---\n指南正文 {{% children %}}",
    )
    .unwrap();
    std::fs::write(
        root.join("guide/intro.md"),
        "---\ntitle: 入门\n---\n# 入门\n正文",
    )
    .unwrap();
    std::fs::write(
        root.join("guide/draft-page.md"),
        "---\ntitle: 草稿\ndraft: true\n---\n草稿正文",
    )
    .unwrap();
    std::fs::write(
        root.join("news.md"),
        "---\ntitle: 发布\npermalink: /release-notes/\n---\n发布正文",
    )
    .unwrap();
    std::fs::write(
        root.join("参考/_index.md"),
        "---\ntitle: 参考\n---\n参考正文",
    )
    .unwrap();
    std::fs::write(root.join("docs/img.png"), "png-bytes").unwrap();
    std::fs::write(root.join(".hidden/secret.md"), "隐藏").unwrap();
    (tmp, root)
}

async fn make_app(cfg: Config) -> Router {
    let state = initialize(cfg).expect("初始化失败");
    build_app(state)
}

async fn get(app: &Router, uri: &str) -> (StatusCode, Vec<(String, String)>, Vec<u8>) {
    let resp = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let headers: Vec<(String, String)> = resp
        .headers()
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec();
    (status, headers, body)
}

#[tokio::test]
async fn test_branch_fallback_dual_url_readme_and_index() {
    // 回退链 HTTP 语义：readme-only / index-only 目录的目录 URL 与
    // 小写普通文档 URL 双可达，渲染同一页面
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("content");
    std::fs::create_dir_all(root.join("readme-dir")).unwrap();
    std::fs::create_dir_all(root.join("index-dir")).unwrap();
    std::fs::write(
        root.join("readme-dir/README.md"),
        "---\ntitle: 读我\n---\nreadme 首页正文",
    )
    .unwrap();
    std::fs::write(
        root.join("index-dir/index.md"),
        "---\ntitle: 索引页\n---\nindex 首页正文",
    )
    .unwrap();
    let app = make_app(make_config(root.clone(), &[], false)).await;

    // readme-only：/dir 与 /dir/readme 双 200 同内容
    let (s1, _, b1) = get(&app, "/readme-dir").await;
    let (s2, _, b2) = get(&app, "/readme-dir/readme").await;
    assert_eq!(s1, StatusCode::OK);
    assert_eq!(s2, StatusCode::OK);
    assert_eq!(b1, b2, "目录 URL 与普通文档 URL 渲染同一页面");

    // index-only：/dir 与 /dir/index 双 200 同内容
    let (s3, _, b3) = get(&app, "/index-dir").await;
    let (s4, _, b4) = get(&app, "/index-dir/index").await;
    assert_eq!(s3, StatusCode::OK);
    assert_eq!(s4, StatusCode::OK);
    assert_eq!(b3, b4);

    // 树中不出现被吸收的 readme/index 子节点
    let (_, _, tree_body) = get(&app, "/api/tree?path=%2F").await;
    let tree = String::from_utf8(tree_body).unwrap();
    assert!(
        !tree.contains("/readme-dir/readme"),
        "readme 不应出现在树中"
    );
    assert!(!tree.contains("/index-dir/index"), "index 不应出现在树中");
}

#[tokio::test]
async fn test_branch_fallback_root_readme_dual_url() {
    // 根目录 readme-only：/ 与 /readme 双 200
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("content");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("readme.md"), "---\ntitle: 根读我\n---\n根正文").unwrap();
    let app = make_app(make_config(root.clone(), &[], false)).await;
    let (s1, _, _) = get(&app, "/").await;
    let (s2, _, _) = get(&app, "/readme").await;
    assert_eq!(s1, StatusCode::OK);
    assert_eq!(s2, StatusCode::OK);
}

#[tokio::test]
async fn test_page_200_with_content_and_404_for_unknown() {
    let (tmp, root) = make_content();
    let app = make_app(make_config(root.clone(), &[], false)).await;
    let (status, _, body) = get(&app, "/guide/intro").await;
    assert_eq!(status, StatusCode::OK);
    let html = String::from_utf8(body).unwrap();
    assert!(html.contains("<h1"), "{}", html);
    assert!(html.contains("正文"), "{}", html);

    // 根分支页
    let (status, _, _) = get(&app, "/guide").await;
    assert_eq!(status, StatusCode::OK);

    // 中文目录（percent-encode）
    let (status, _, body) = get(&app, "/%E5%8F%82%E8%80%83").await;
    assert_eq!(status, StatusCode::OK, "中文目录应可访问");
    assert!(String::from_utf8(body).unwrap().contains("参考正文"));

    // 未知路径 404
    let (status, _, _) = get(&app, "/no/such/page").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    drop(tmp);
}

#[tokio::test]
async fn test_draft_page_returns_404_without_leaking() {
    let (tmp, root) = make_content();
    let app = make_app(make_config(root, &[], false)).await;
    let (status, _, _) = get(&app, "/guide/draft-page").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "draft 不暴露存在性");

    // draft = true 时可访问
    let (tmp2, root2) = make_content();
    let app2 = make_app(make_config(root2, &[], true)).await;
    let (status, _, _) = get(&app2, "/guide/draft-page").await;
    assert_eq!(status, StatusCode::OK);
    drop((tmp, tmp2));
}

#[tokio::test]
async fn test_permalink_priority() {
    let (tmp, root) = make_content();
    let app = make_app(make_config(root, &[], false)).await;
    let (status, _, body) = get(&app, "/release-notes/").await;
    assert_eq!(status, StatusCode::OK, "permalink 自定义 URL 可访问");
    assert!(String::from_utf8(body).unwrap().contains("发布正文"));
    // 原 URL 保留
    let (status, _, _) = get(&app, "/news").await;
    assert_eq!(status, StatusCode::OK);
    // 尾斜杠兼容：文档默认 URL 带尾斜杠按无斜杠解析
    let (status, _, _) = get(&app, "/news/").await;
    assert_eq!(status, StatusCode::OK, "文档 URL 尾斜杠应兼容");
    // permalink 不带尾斜杠同样可访问（permalink 声明为 /release-notes/）
    let (status, _, _) = get(&app, "/release-notes").await;
    assert_eq!(status, StatusCode::OK, "permalink 去尾斜杠应兼容");
    drop(tmp);
}

/// 侧栏祖先目录 href 链注入：data-dir-hrefs
/// 中的每个目录 href 必须与首屏树中对应节点的 url 一致——目录带 permalink
/// 时两者都是 permalink 形态，前端才能据此展开祖先链并让 current 高亮可见。
#[tokio::test]
async fn test_sidebar_dir_hrefs_match_tree_nodes() {
    let (tmp, root) = make_content();
    let app = make_app(make_config(root, &[], false)).await;
    let (status, _, body) = get(&app, "/guide/intro").await;
    assert_eq!(status, StatusCode::OK);
    let html = String::from_utf8(body).unwrap();
    // guide 无 permalink：注入的是 encode 形态默认目录 URL（含根；单引号属性）
    assert!(
        html.contains("data-dir-hrefs='/guide|/'"),
        "data-dir-hrefs 应含祖先链（guide + 根）：{html}"
    );
    // 首屏树中 guide 节点的 url 与注入链一致（data-tree 内 JSON 已 HTML 转义）
    assert!(
        html.contains("data-tree='[{&#34;url&#34;:&#34;/guide&#34;"),
        "树根节点（guide）应与注入链一致：{html}"
    );
    drop(tmp);
}

/// 目录带 permalink 时祖先链注入 permalink 形态：
/// 子页面的 data-dir-hrefs 必须是目录的 permalink（与树节点 url 一致），
/// 而不是目录默认 URL——前端按前缀推断祖先对 permalink 目录失效。
#[tokio::test]
async fn test_sidebar_dir_hrefs_use_dir_permalink() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("content");
    std::fs::create_dir_all(root.join("guide/deep")).unwrap();
    std::fs::write(
        root.join("guide/_index.md"),
        "---\ntitle: 指南\npermalink: /handbook/\n---\n指南正文",
    )
    .unwrap();
    std::fs::write(
        root.join("guide/deep/_index.md"),
        "---\ntitle: 深层\n---\n深层正文",
    )
    .unwrap();
    std::fs::write(
        root.join("guide/deep/page.md"),
        "---\ntitle: 页面\n---\n页面正文",
    )
    .unwrap();
    let app = make_app(make_config(root, &[], false)).await;

    let (status, _, body) = get(&app, "/guide/deep/page").await;
    assert_eq!(status, StatusCode::OK);
    let html = String::from_utf8(body).unwrap();
    // guide 的 href 是 permalink /handbook/，deep 是默认 encode 形态；
    // 顺序：最近祖先在前（guide/deep → /handbook/ → 根）
    assert!(
        html.contains("data-dir-hrefs='/guide/deep|/handbook/|/'"),
        "祖先链应为 [guide/deep, /handbook/, /]：{html}"
    );
    // scoped 树中 guide 根节点 url 同为 permalink（一致性即前端可匹配的前提）
    assert!(
        html.contains("data-tree='[{&#34;url&#34;:&#34;/handbook/&#34;"),
        "树根节点（guide）应为 permalink 形态"
    );
    drop(tmp);
}

#[tokio::test]
async fn test_tree_api_etag_304() {
    let (tmp, root) = make_content();
    let state = initialize(make_config(root.clone(), &[], false)).expect("init");
    let app = build_app(state.clone());

    let (status, headers, body) = get(&app, "/api/tree/children?path=/guide").await;
    assert_eq!(status, StatusCode::OK);
    let etag = headers
        .iter()
        .find(|(k, _)| k == "etag")
        .map(|(_, v)| v.clone())
        .expect("应有 ETag");
    let json = String::from_utf8(body).unwrap();
    assert!(json.contains("\"children\""), "{}", json);

    // If-None-Match → 304 无 body
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/tree/children?path=/guide")
                .header(header::IF_NONE_MATCH, &etag)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_MODIFIED);
    let body304 = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    assert!(body304.is_empty(), "304 无 body");

    // 树内容变化（新增文件）→ 索引重建（watcher 的职责，此处手动模拟）→ 新 ETag 200
    std::fs::write(root.join("guide/new.md"), "---\ntitle: 新页\n---\n").unwrap();
    let new_index = coral_core::scanner::SiteIndex::build(
        coral_core::scanner::scan(&coral_core::config::ContentConfig {
            root: root.clone(),
            exclude: vec![],
            draft: false,
        })
        .unwrap(),
        false,
    );
    state.replace_index(new_index);
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/tree/children?path=/guide")
                .header(header::IF_NONE_MATCH, &etag)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "树变化后应返回新内容");
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    assert!(
        String::from_utf8_lossy(&body).contains("新页"),
        "新文件应出现在树中"
    );
    drop(tmp);
}

#[tokio::test]
async fn test_path_traversal_all_404() {
    let (tmp, root) = make_content();
    let app = make_app(make_config(root, &[], false)).await;
    // 静态资源穿越变体（验收第 10 条）
    for uri in [
        "/..%2F..%2Fetc%2Fpasswd",
        "/%2e%2e/%2e%2e/etc/passwd",
        "/../secret.txt",
        "/guide/../../../etc/hostname",
        "/%00secret",
    ] {
        let (status, _, body) = get(&app, uri).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "穿越请求 {uri} 应 404");
        assert!(
            !String::from_utf8_lossy(&body).contains("root:"),
            "{uri} 不得泄露根外文件内容"
        );
    }
    // 树 API 穿越参数
    let (status, _, _) = get(&app, "/api/tree/children?path=../../etc").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _, _) = get(&app, "/api/tree/children?path=/..%2F..%2Fetc").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    drop(tmp);
}

#[tokio::test]
async fn test_static_asset_mime_and_immutable() {
    let (tmp, root) = make_content();
    let app = make_app(make_config(root, &[], false)).await;
    let (status, headers, body) = get(&app, "/docs/img.png").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, b"png-bytes");
    let ct = headers.iter().find(|(k, _)| k == "content-type").unwrap();
    assert_eq!(ct.1, "image/png");
    let cc = headers.iter().find(|(k, _)| k == "cache-control").unwrap();
    assert!(cc.1.contains("immutable"), "{}", cc.1);
    assert!(cc.1.contains("max-age=31536000"), "{}", cc.1);

    // md 不走静态——页面路由正常处理
    let (status, _, _) = get(&app, "/guide/intro.md").await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "md 走页面路由的 .md 形态不服务"
    );
    // 隐藏目录不服务
    let (status, _, _) = get(&app, "/.hidden/secret.md").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    drop(tmp);
}

#[tokio::test]
async fn test_probes_healthz_readyz() {
    let (tmp, root) = make_content();
    let cfg = make_config(root, &[], false);
    let state = initialize(cfg).expect("init");
    state.ready.store(false, Ordering::Release); // 模拟未就绪
    let app = build_app(state);
    let (status, _, _) = get(&app, "/healthz").await;
    assert_eq!(status, StatusCode::OK, "healthz 不依赖就绪");
    let (status, _, _) = get(&app, "/readyz").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    drop(tmp);
}

#[tokio::test]
async fn test_concurrent_miss_renders_once() {
    let (tmp, root) = make_content();
    let cfg = make_config(root, &[], false);
    let state = initialize(cfg).expect("init");
    let app = build_app(state.clone());

    // 50 并发同 miss：只渲染 1 次
    let mut handles = Vec::new();
    for _ in 0..50 {
        let app2 = app.clone();
        handles.push(tokio::spawn(async move {
            let (status, _, _) = get(&app2, "/guide/intro").await;
            assert_eq!(status, StatusCode::OK);
        }));
    }
    for h in handles {
        h.await.unwrap();
    }
    assert_eq!(
        state.render_count.load(Ordering::SeqCst),
        1,
        "singleflight：50 并发只渲染 1 次"
    );
    drop(tmp);
}

#[tokio::test]
async fn test_swr_stale_returns_old_then_revalidates() {
    let (tmp, root) = make_content();
    let cfg = make_config(root.clone(), &[], false);
    let state = initialize(cfg).expect("init");
    let app = build_app(state.clone());

    // 首次渲染落盘
    let (status, _, _) = get(&app, "/guide/intro").await;
    assert_eq!(status, StatusCode::OK);

    // 修改源文件（mtime/size 变）→ 首请求返回旧内容 + 后台重渲染
    std::fs::write(
        root.join("guide/intro.md"),
        "---\ntitle: 入门\n---\n# 入门\n新正文内容更长",
    )
    .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    let (status, _, body) = get(&app, "/guide/intro").await;
    assert_eq!(status, StatusCode::OK);
    let _ = String::from_utf8_lossy(&body); // 旧或新都可接受（SWR 语义）

    // 等后台重渲染完成 → 新内容可见
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let (_, _, body) = get(&app, "/guide/intro").await;
        if String::from_utf8_lossy(&body).contains("新正文") {
            drop(tmp);
            return;
        }
    }
    panic!("SWR：后台重渲染后新内容应可见");
}

#[tokio::test]
async fn test_cache_hit_no_rerender() {
    let (tmp, root) = make_content();
    let cfg = make_config(root, &[], false);
    let state = initialize(cfg).expect("init");
    let app = build_app(state.clone());

    let (status, _, _) = get(&app, "/guide/intro").await;
    assert_eq!(status, StatusCode::OK);
    let count_after_first = state.render_count.load(Ordering::SeqCst);
    assert_eq!(count_after_first, 1);

    // 二次请求：缓存命中（mtime 未变）不渲染
    let (status, _, _) = get(&app, "/guide/intro").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        state.render_count.load(Ordering::SeqCst),
        1,
        "缓存命中路径不重复渲染"
    );
    drop(tmp);
}

#[tokio::test]
async fn test_page_renders_full_layout() {
    let (tmp, root) = make_content();
    let app = make_app(make_config(root, &[], false)).await;
    let (status, _, body) = get(&app, "/guide/intro").await;
    assert_eq!(status, StatusCode::OK);
    let html = String::from_utf8(body).unwrap();
    // layout 骨架（三区 + 顶栏）
    assert!(
        html.contains("class=\"topbar\""),
        "{}",
        &html[..200.min(html.len())]
    );
    assert!(html.contains("class=\"sidebar\""));
    assert!(html.contains("id=\"tree\""));
    // 防 FOUC 内联脚本
    assert!(html.contains("coral-theme"));
    // 面包屑 + 最后更新区域（上下篇 pager 已移除）
    assert!(html.contains("breadcrumb"));
    assert!(!html.contains("pager"));
    // 内容片段注入
    assert!(html.contains("正文"));
    drop(tmp);
}

#[tokio::test]
async fn test_error_page_uses_template() {
    let (tmp, root) = make_content();
    let app = make_app(make_config(root, &[], false)).await;
    let (status, _, body) = get(&app, "/no/such/page").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let html = String::from_utf8(body).unwrap();
    assert!(html.contains("404"), "{}", html);
    assert!(html.contains("返回首页"), "{}", html);
    // 不泄露内部错误字符串
    assert!(!html.contains("panic"), "{}", html);
    drop(tmp);
}

#[tokio::test]
async fn test_embedded_assets_served() {
    let (tmp, root) = make_content();
    let app = make_app(make_config(root, &[], false)).await;
    let (status, headers, body) = get(&app, "/assets/app.css").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.len() > 1000, "app.css 应有实际内容");
    let ct = headers.iter().find(|(k, _)| k == "content-type").unwrap();
    assert_eq!(ct.1, "text/css; charset=utf-8");
    let cc = headers.iter().find(|(k, _)| k == "cache-control").unwrap();
    assert!(cc.1.contains("immutable"));

    let (status, _, _) = get(&app, "/assets/app.js").await;
    assert_eq!(status, StatusCode::OK);

    // 内嵌资源穿越 404
    let (status, _, _) = get(&app, "/assets/../secret").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    drop(tmp);
}

#[tokio::test]
async fn test_search_disabled_returns_empty() {
    let (tmp, root) = make_content();
    // 默认配置 search.enabled = false
    let app = make_app(make_config(root, &[], false)).await;
    let (status, _, body) = get(&app, "/api/search?q=入门").await;
    assert_eq!(status, StatusCode::OK, "未开启不 503");
    let json = String::from_utf8(body).unwrap();
    assert!(json.contains(r#""enabled":false"#), "未开启标记：{json}");
    assert!(json.contains(r#""hits":[]"#), "{json}");
    drop(tmp);
}

#[tokio::test]
async fn test_search_enabled_chinese_and_incremental() {
    let (tmp, root) = make_content();
    let mut cfg = make_config(root.clone(), &[], false);
    cfg.search.enabled = true;
    let state = initialize(cfg).expect("init");
    let app = build_app(state.clone());
    // watcher 增量维护需要监听运行（本测试验证完整失效链）
    let _watch = coral_server::watcher::spawn_watcher(state.clone());

    // 等后台构建完成（最终一致轮询）
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut hits = Vec::new();
    while std::time::Instant::now() < deadline {
        let (_, _, body) = get(&app, "/api/search?q=入门&limit=10").await;
        let json = String::from_utf8(body).unwrap();
        if json.contains("入门") && !json.contains("building") {
            hits.push(json);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    assert!(!hits.is_empty(), "搜索索引应在 10s 内就绪并命中中文词");

    // 结果结构：title/url/snippet/dir_path
    let json = hits[0].clone();
    assert!(json.contains(r#""url":"/guide/intro""#), "{json}");
    assert!(json.contains("snippet"), "{json}");
    assert!(json.contains("dir_path"), "{json}");

    // 新增文档 → watcher 增量 → 可搜到
    std::fs::write(
        root.join("guide/search-new.md"),
        "---\ntitle: 增量页\n---\n增量搜索关键词内容",
    )
    .unwrap();
    let found = {
        let app = app.clone();
        let mut ok = false;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            let (_, _, body) = get(&app, "/api/search?q=增量搜索关键词").await;
            if String::from_utf8(body).unwrap().contains("增量页") {
                ok = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        }
        ok
    };
    assert!(found, "watcher 增量维护后新文档应可被搜到");
    drop((tmp, state));
}

#[tokio::test]
async fn test_search_page_server_rendered() {
    let (tmp, root) = make_content();
    let mut cfg = make_config(root, &[], false);
    cfg.search.enabled = true;
    let state = initialize(cfg).expect("init");
    let app = build_app(state.clone());
    // 等就绪
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        let (_, _, body) = get(&app, "/api/search?q=入门").await;
        if String::from_utf8(body).unwrap().contains("入门") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    let (status, _, body) = get(&app, "/search?q=入门").await;
    assert_eq!(status, StatusCode::OK);
    let html = String::from_utf8(body).unwrap();
    assert!(html.contains("search-results"), "{html}");
    assert!(html.contains("入门"), "{html}");
    drop((tmp, state));
}

/// 真实 TCP 起 app（oneshot 无 ConnectInfo，reindex 依赖对端地址）。
async fn spawn_server(state: std::sync::Arc<coral_server::AppState>) -> String {
    let app = build_app(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await;
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn test_reindex_endpoints() {
    let (tmp, root) = make_content();
    let mut cfg = make_config(root, &[], false);
    cfg.search.enabled = true;
    let state = initialize(cfg).expect("init");

    let base = spawn_server(state.clone()).await;
    // 先等启动后台构建就绪（reindex 语义：复用就绪实例，构建中 503）
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        let resp = reqwest_shim(&base, "/api/search?q=入门").await;
        if resp.1.contains("入门") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    // 同步重建完成
    let resp = reqwest_shim(&base, "/search/reindex").await;
    assert_eq!(resp.0, StatusCode::OK, "reindex 应同步完成: {}", resp.1);
    assert!(resp.1.contains(r#""ok":true"#), "{}", resp.1);
    // 重建后搜索可用
    let resp = reqwest_shim(&base, "/api/search?q=入门").await;
    assert!(resp.1.contains("入门"), "{}", resp.1);
    drop((tmp, state));
}

#[tokio::test]
async fn test_reindex_disabled_404() {
    let (tmp, root) = make_content();
    // 默认 search.enabled = false
    let state = initialize(make_config(root, &[], false)).expect("init");
    let base = spawn_server(state.clone()).await;
    let resp = reqwest_shim(&base, "/search/reindex").await;
    assert_eq!(resp.0, StatusCode::NOT_FOUND, "未开启时 404 不暴露端点");
    drop((tmp, state));
}

/// 极简 HTTP GET（避免引入 reqwest 依赖）。
async fn reqwest_shim(base: &str, path: &str) -> (StatusCode, String) {
    let resp = tokio::fs::read("/dev/null").await; // 占位避免 unused 警告
    let _ = resp;
    let url = format!("{base}{path}");
    let uri: axum::http::Uri = url.parse().unwrap();
    let host = uri.host().unwrap().to_string();
    let port = uri.port_u16().unwrap();
    let stream = tokio::net::TcpStream::connect((host.as_str(), port))
        .await
        .unwrap();
    let (mut rx, mut tx) = tokio::io::split(stream);
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let req = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    tx.write_all(req.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    rx.read_to_end(&mut buf).await.unwrap();
    let text = String::from_utf8_lossy(&buf).into_owned();
    let status: u16 = text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let body = text
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_string())
        .unwrap_or_default();
    let code = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_REQUEST);
    (code, body)
}

#[tokio::test]
async fn test_git_webhook_disabled_404() {
    let (tmp, root) = make_content();
    // 默认 git.enabled = false
    let state = initialize(make_config(root, &[], false)).expect("init");
    let base = spawn_server(state.clone()).await;
    let resp = reqwest_shim_post(
        &base,
        "/git/webhook",
        r#"{"object_kind":"push","ref":"refs/heads/main","checkout_sha":"abc"}"#,
    )
    .await;
    assert_eq!(resp.0, StatusCode::NOT_FOUND, "未开启 404");
    drop((tmp, state));
}

#[tokio::test]
async fn test_git_webhook_filters() {
    let (tmp, root) = make_content();
    let mut cfg = make_config(root, &[], false);
    cfg.git.enabled = true;
    cfg.git.branch = "main".into();
    cfg.git.secret_token = "s3cr3t".into();
    let state = initialize(cfg).expect("init");
    let base = spawn_server(state.clone()).await;

    // token 错误 → 403
    let resp = reqwest_shim_post_auth(
        &base,
        "/git/webhook",
        r#"{"object_kind":"push","ref":"refs/heads/main","checkout_sha":"abc"}"#,
        "wrong",
    )
    .await;
    assert_eq!(resp.0, StatusCode::FORBIDDEN);

    // 非 push 事件 → 200 ignored
    let resp = reqwest_shim_post_auth(
        &base,
        "/git/webhook",
        r#"{"object_kind":"tag_push","ref":"refs/tags/v1"}"#,
        "s3cr3t",
    )
    .await;
    assert_eq!(resp.0, StatusCode::OK);
    assert!(resp.1.contains("ignored"), "{}", resp.1);

    // 分支不匹配 → 200 ignored
    let resp = reqwest_shim_post_auth(
        &base,
        "/git/webhook",
        r#"{"object_kind":"push","ref":"refs/heads/dev","checkout_sha":"abc"}"#,
        "s3cr3t",
    )
    .await;
    assert!(resp.1.contains("ignored"), "{}", resp.1);

    // 分支删除（checkout_sha null）→ 200 ignored
    let resp = reqwest_shim_post_auth(
        &base,
        "/git/webhook",
        r#"{"object_kind":"push","ref":"refs/heads/main","checkout_sha":null}"#,
        "s3cr3t",
    )
    .await;
    assert!(resp.1.contains("branch deleted"), "{}", resp.1);

    // 合法 push（远端不可达，file:// 不存在路径）→ 500（同步失败但端点语义正确）
    let resp = reqwest_shim_post_auth(
        &base,
        "/git/webhook",
        r#"{"object_kind":"push","ref":"refs/heads/main","checkout_sha":"abc"}"#,
        "s3cr3t",
    )
    .await;
    assert_eq!(resp.0, StatusCode::INTERNAL_SERVER_ERROR, "{}", resp.1);
    drop((tmp, state));
}

#[tokio::test]
async fn test_git_webhook_full_sync_flow() {
    // file:// 真实远端：clone + 同步 + 刷新全链
    let tmp = tempfile::tempdir().unwrap();
    let remote = tmp.path().join("repo");
    std::fs::create_dir_all(remote.join("docs/guide")).unwrap();
    let run = |args: &[&str], dir: &std::path::Path| {
        let ok = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?}");
    };
    run(&["init", "-b", "main", "--quiet"], &remote);
    std::fs::write(remote.join("docs/_index.md"), "---\ntitle: 首页\n---\n首页").unwrap();
    std::fs::write(remote.join("docs/guide/intro.md"), "搜索关键词内容").unwrap();
    run(&["add", "."], &remote);
    run(
        &[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "commit",
            "--quiet",
            "-m",
            "init",
        ],
        &remote,
    );

    let content = tmp.path().join("content");
    std::fs::create_dir_all(&content).unwrap();
    let cache = tmp.path().join("cache");
    let cfg = Config {
        server: ServerConfig::default(),
        content: ContentConfig {
            root: content.clone(),
            exclude: vec![],
            draft: false,
        },
        tree: TreeConfig::default(),
        cache: CacheConfig { dir: cache },
        log: LogConfig::default(),
        search: coral_core::SearchConfig::default(),
        git: coral_core::GitConfig {
            enabled: true,
            remote: format!("file://{}", remote.display()),
            branch: "main".into(),
            watch_dir: "docs".into(),
            ..Default::default()
        },
        render: coral_core::RenderConfig::default(),
    };
    let state = initialize(cfg).expect("init");
    let base = spawn_server(state.clone()).await;

    // webhook 触发同步（loopback 无 secret）
    let resp = reqwest_shim_post(
        &base,
        "/git/webhook",
        r#"{"object_kind":"push","ref":"refs/heads/main","checkout_sha":"abc"}"#,
    )
    .await;
    assert_eq!(resp.0, StatusCode::OK, "{}", resp.1);
    assert!(resp.1.contains(r#""changed":2"#), "{}", resp.1);

    // 内容落位 + 页面可访问（刷新链生效）
    assert!(content.join("_index.md").is_file());
    let page = reqwest_shim(&base, "/guide/intro").await;
    assert!(
        page.1.contains("搜索关键词内容"),
        "{}",
        page.1[..page.1.len().min(300)].to_string()
    );
    drop((tmp, state));
}

/// 极简 HTTP POST JSON（同 reqwest_shim）。
async fn http_post(base: &str, path: &str, body: &str, auth: Option<&str>) -> (StatusCode, String) {
    let url = format!("{base}{path}");
    let uri: axum::http::Uri = url.parse().unwrap();
    let host = uri.host().unwrap().to_string();
    let port = uri.port_u16().unwrap();
    let mut stream = tokio::net::TcpStream::connect((host.as_str(), port))
        .await
        .unwrap();
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut req = format!(
        "POST {path} HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    if let Some(token) = auth {
        req.push_str(&format!("X-Gitlab-Token: {token}\r\n"));
    }
    req.push_str("\r\n");
    req.push_str(body);
    stream.write_all(req.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.unwrap();
    let text = String::from_utf8_lossy(&buf).into_owned();
    let status: u16 = text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let body = text
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_string())
        .unwrap_or_default();
    let code = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_REQUEST);
    (code, body)
}

async fn reqwest_shim_post(base: &str, path: &str, body: &str) -> (StatusCode, String) {
    http_post(base, path, body, None).await
}

async fn reqwest_shim_post_auth(
    base: &str,
    path: &str,
    body: &str,
    token: &str,
) -> (StatusCode, String) {
    http_post(base, path, body, Some(token)).await
}
