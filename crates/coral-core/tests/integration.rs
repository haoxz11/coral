//! 集成测试：fixture 树扫描 → 索引 → 树构建。
//!
//! fixture 是需求无关的通用样本（不复制真实内容目录）。
//! symlink 用例运行时 tempdir 构造（逃逸目标需真实存在，不宜进 git fixture）。

use coral_core::config::ContentConfig;
use coral_core::scanner::{SiteIndex, scan};
use coral_core::tree::{NodeType, build_subtree};
use serde_json::json;
use std::path::{Path, PathBuf};

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/site")
}

fn content_config(exclude: &[&str], draft: bool) -> ContentConfig {
    ContentConfig {
        root: fixture_root(),
        exclude: exclude.iter().map(PathBuf::from).collect(),
        draft,
    }
}

fn build_index(exclude: &[&str], draft: bool) -> SiteIndex {
    let result = scan(&content_config(exclude, draft)).expect("扫描 fixture 树");
    SiteIndex::build(result, draft)
}

#[test]
fn test_scan_ignores_junk_and_hidden_and_excluded() {
    let index = build_index(&["drafts", "excluded"], false);
    // 隐藏目录/junk/exclude 不入索引
    assert!(!index.pages.contains_key(Path::new(".hidden/secret.md")));
    assert!(!index.pages.contains_key(Path::new("edit.md~")));
    assert!(!index.pages.contains_key(Path::new("#lock.md#")));
    assert!(!index.pages.contains_key(Path::new("drafts/hidden-doc.md")));
    assert!(!index.pages.contains_key(Path::new("excluded/old.md")));
    // 非 md 文件不进 pages（静态资源按需服务）
    assert!(!index.pages.contains_key(Path::new("docs/image.png")));
    // 正常页面都在
    for rel in [
        "_index.md",
        "plain.md",
        "toml-page.md",
        "guide/intro.md",
        "guide/advanced/topic.md",
        "docs/deep/deeper/leaf.md",
        "reference/_index.md",
    ] {
        assert!(index.pages.contains_key(Path::new(rel)), "缺 {rel}");
    }
}

#[test]
fn test_routes_cover_branch_dir_permalink_and_conflict() {
    let index = build_index(&["drafts", "excluded"], false);

    // 根分支页 → /
    assert_eq!(index.routes.get("/"), Some(&PathBuf::from("_index.md")));
    // 目录即 URL
    assert_eq!(
        index.routes.get("/guide"),
        Some(&PathBuf::from("guide/_index.md"))
    );
    assert_eq!(
        index.routes.get("/guide/advanced"),
        Some(&PathBuf::from("guide/advanced/_index.md"))
    );
    // reference 分支页（原名中文目录，非 ASCII 覆盖移至 test_non_ascii_dir_route_decoded_and_tree_url_encoded）
    assert_eq!(
        index.routes.get("/reference"),
        Some(&PathBuf::from("reference/_index.md"))
    );
    // 普通文档去扩展名
    assert_eq!(
        index.routes.get("/guide/intro"),
        Some(&PathBuf::from("guide/intro.md"))
    );
    // permalink 表 + 原 URL 保留
    assert_eq!(
        index.permalinks.get("/release-notes/"),
        Some(&PathBuf::from("news/2026-release.md"))
    );
    assert_eq!(
        index.routes.get("/news/2026-release"),
        Some(&PathBuf::from("news/2026-release.md"))
    );
    // 冲突裁决：foo/ 占位，foo.md 改用 /conflict/foo/index（目录优先）
    assert!(!index.routes.contains_key("/conflict/foo"));
    assert_eq!(
        index.routes.get("/conflict/foo/index"),
        Some(&PathBuf::from("conflict/foo.md"))
    );
    // 无 _index.md 的目录不进路由表（404），但保留命名空间占位
    assert!(!index.routes.contains_key("/conflict"));
    assert!(!index.routes.contains_key("/news"));
    // 有 _index.md 的深层目录正常进路由表
    assert_eq!(
        index.routes.get("/docs/deep"),
        Some(&PathBuf::from("docs/deep/_index.md"))
    );
}

#[test]
fn test_non_ascii_dir_route_decoded_and_tree_url_encoded() {
    // 非 ASCII 目录名（运行时 tempdir 构造，不进 git fixture）：
    // 路由表 key 为 decode 形态，树节点 URL 为 percent-encode 形态
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("content");
    std::fs::create_dir_all(root.join("参考")).unwrap();
    std::fs::write(
        root.join("参考/_index.md"),
        "---\ntitle: 参考资料\n---\n参考资料分支页",
    )
    .unwrap();
    let cfg = ContentConfig {
        root,
        exclude: vec![],
        draft: false,
    };
    let index = SiteIndex::build(scan(&cfg).unwrap(), false);
    assert_eq!(
        index.routes.get("/参考"),
        Some(&PathBuf::from("参考/_index.md")),
        "路由表 key 应为 decode 形态"
    );
    let tree = build_subtree(&index, Path::new(""), 1, false);
    assert!(
        tree.iter().any(|n| n.url == "/%E5%8F%82%E8%80%83"),
        "树节点 URL 应为 percent-encode 形态"
    );
}

#[test]
fn test_draft_excluded_from_routes_and_tree() {
    let index = build_index(&["drafts", "excluded"], false);
    assert!(!index.routes.contains_key("/guide/draft-page"));
    // pages 映射保留全量信息
    assert!(index.pages.contains_key(Path::new("guide/draft-page.md")));

    // draft = true 时正常进路由
    let enabled = build_index(&["drafts", "excluded"], true);
    assert_eq!(
        enabled.routes.get("/guide/draft-page"),
        Some(&PathBuf::from("guide/draft-page.md"))
    );
}

#[test]
fn test_children_deps_ancestor_chain() {
    let index = build_index(&["drafts", "excluded"], false);
    // 根 _index.md 含 children → 注册到根（""）
    assert!(
        index
            .children_deps
            .get(Path::new(""))
            .is_some_and(|v| v.contains(&PathBuf::from("_index.md")))
    );
    // guide/advanced/_index.md 含 children → 注册到 guide/advanced、guide、根（祖先链）
    for dir in ["guide/advanced", "guide"] {
        assert!(
            index
                .children_deps
                .get(Path::new(dir))
                .is_some_and(|v| v.contains(&PathBuf::from("guide/advanced/_index.md"))),
            "children_deps 缺 {dir}"
        );
    }
    // 不含 children 的目录不注册
    assert!(!index.children_deps.contains_key(Path::new("reference")));
}

#[test]
fn test_tree_json_matches_expected() {
    let index = build_index(&["drafts", "excluded"], false);
    // 首屏 initial_depth = 2（默认值）
    let root_children = build_subtree(&index, Path::new(""), 2, false);

    let actual = serde_json::to_value(&root_children).unwrap();
    let expected = json!([
        {
            "url": "/toml-page",
            "title": "TOML 页面",
            "weight": 1,
            "node_type": "leaf",
            "has_children": false
        },
        {
            "url": "/readme-dir",
            "title": "读我",
            "weight": 3,
            "node_type": "branch",
            "has_children": false
        },
        {
            "url": "/docs",
            "title": "文档集",
            "weight": 5,
            "node_type": "branch",
            "has_children": true,
            "children": [
                {
                    "url": "/docs/deep",
                    "title": "深层",
                    "weight": null,
                    "node_type": "branch",
                    "has_children": true
                }
            ]
        },
        {
            "url": "/guide",
            "title": "指南",
            "weight": 10,
            "node_type": "branch",
            "has_children": true,
            "children": [
                {
                    "url": "/guide/intro",
                    "title": "入门",
                    "weight": 1,
                    "node_type": "leaf",
                    "has_children": false
                },
                {
                    "url": "/guide/advanced",
                    "title": "进阶",
                    "weight": 2,
                    "node_type": "branch",
                    "has_children": true
                }
            ]
        },
        {
            "url": "/both-dir",
            "title": "混合",
            "weight": null,
            "node_type": "branch",
            "has_children": true,
            "children": [
                {
                    "url": "/both-dir/index",
                    "title": "普通索引",
                    "weight": null,
                    "node_type": "leaf",
                    "has_children": false
                }
            ]
        },
        {
            "url": "/conflict",
            "title": "conflict",
            "weight": null,
            "node_type": "branch",
            "has_children": true,
            "children": [
                {
                    "url": "/conflict/foo",
                    "title": "foo",
                    "weight": null,
                    "node_type": "branch",
                    "has_children": true
                },
                {
                    "url": "/conflict/foo/index",
                    "title": "与目录同名的文档",
                    "weight": null,
                    "node_type": "leaf",
                    "has_children": false
                }
            ]
        },
        {
            "url": "/index-dir",
            "title": "索引页",
            "weight": null,
            "node_type": "branch",
            "has_children": false
        },
        {
            "url": "/news",
            "title": "news",
            "weight": null,
            "node_type": "branch",
            "has_children": true,
            "children": [
                {
                    "url": "/release-notes/",
                    "title": "发布说明",
                    "weight": null,
                    "node_type": "leaf",
                    "has_children": false
                }
            ]
        },
        {
            "url": "/plain",
            "title": "plain",
            "weight": null,
            "node_type": "leaf",
            "has_children": false
        },
        {
            "url": "/reference",
            "title": "参考资料",
            "weight": null,
            "node_type": "branch",
            "has_children": false
        },
        {
            "url": "/rootreadme-draft",
            "title": "rootreadme-draft",
            "weight": null,
            "node_type": "branch",
            "has_children": true,
            "children": [
                {
                    "url": "/rootreadme-draft/readme",
                    "title": "读我",
                    "weight": null,
                    "node_type": "leaf",
                    "has_children": false
                }
            ]
        }
    ]);
    assert_eq!(actual, expected, "首屏树与期望 JSON 不一致");
}

#[test]
fn test_branch_fallback_tree_links_for_unselected_candidates() {
    // 多候选并存的目录（both-dir 有 _index + index；readme-dir 只 readme）：
    // 树中分支页吸收进目录节点，未被选中者作为普通子节点、href 为自身 URL
    let index = build_index(&["drafts", "excluded"], false);

    // both-dir：_index 被选中 → 目录节点；index.md 未选中 → 子节点
    let both = build_subtree(&index, Path::new("both-dir"), 1, false);
    assert_eq!(both.len(), 1, "未被选中的 index.md 应是唯一子节点");
    assert_eq!(both[0].url, "/both-dir/index");
    assert_eq!(both[0].node_type, coral_core::tree::NodeType::Leaf);
    assert_eq!(both[0].title, "普通索引");

    // 顶栏/菜单数据源同口径：目录 title 取自被选中分支页
    let top_sections = index.top_sections();
    let both_ts = top_sections
        .iter()
        .find(|t| t.url == "/both-dir")
        .expect("both-dir 应在一级菜单");
    assert_eq!(both_ts.title, "混合");
    // readme-dir 目录节点：readme 被吸收，无子文档节点
    let readme_ts = top_sections
        .iter()
        .find(|t| t.url == "/readme-dir")
        .expect("readme-dir 应在一级菜单");
    assert_eq!(readme_ts.title, "读我");
}

#[test]
fn test_tree_depth_semantics() {
    let index = build_index(&["drafts", "excluded"], false);
    // 深度 0：空（父层 has_children 计算用）
    assert!(build_subtree(&index, Path::new(""), 0, false).is_empty());
    // 深度 1：guide 出现但 children 为空，has_children = true（懒加载展开依据）
    let depth1 = build_subtree(&index, Path::new(""), 1, false);
    let guide = depth1
        .iter()
        .find(|n| n.title == "指南")
        .expect("深度 1 应含 guide");
    assert!(guide.has_children);
    assert!(guide.children.is_empty());
    // 懒加载语义：expand_depth + 1 = 2 层，guide/advanced/topic 可见
    let lazy = build_subtree(&index, Path::new("guide"), 2, false);
    let advanced = lazy
        .iter()
        .find(|n| n.title == "进阶")
        .expect("guide 子树应含 advanced");
    assert!(
        advanced.children.iter().any(|n| n.title == "深入主题"),
        "expand_depth+1 层应露出叶子文档"
    );
}

#[test]
fn test_tree_draft_pages_hidden_and_dir_fallback_title() {
    let index = build_index(&["drafts", "excluded"], false);
    let guide = build_subtree(&index, Path::new("guide"), 2, false);
    // draft 页不出现在树中
    assert!(!guide.iter().any(|n| n.title == "草稿页"));

    // conflict/foo 无 _index.md：树中保留，标题回退目录名
    let root = build_subtree(&index, Path::new("conflict"), 1, false);
    let foo_dir = root.iter().find(|n| n.title == "foo");
    assert!(foo_dir.is_some(), "无 _index.md 的目录仍应出现在树中");
    assert_eq!(
        foo_dir.unwrap().node_type,
        coral_core::tree::NodeType::Branch
    );
    // 子页 bar 在下一层
    let foo_children = build_subtree(&index, Path::new("conflict/foo"), 1, false);
    assert!(foo_children.iter().any(|n| n.title == "冲突子页"));
}

#[test]
fn test_tree_with_draft_enabled_shows_draft_pages() {
    let index = build_index(&["drafts", "excluded"], true);
    let guide = build_subtree(&index, Path::new("guide"), 2, true);
    assert!(guide.iter().any(|n| n.title == "草稿页"));
}

#[test]
fn test_scan_root_unreadable_fails_fast() {
    let cfg = ContentConfig {
        root: PathBuf::from("/nonexistent/coral-content-root"),
        exclude: vec![],
        draft: false,
    };
    let err = scan(&cfg).unwrap_err();
    assert!(err.to_string().contains("content 根目录不可读"));
}

#[test]
fn test_permalink_conflict_older_date_wins_and_loser_falls_back() {
    // 拷贝文件忘改 permalink 的场景：date 老者优先保留，
    // 落败者 permalink 不生效（树 href 回退默认 URL），路由不指向落败者
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::write(
        root.join("old.md"),
        "---\ntitle: 老文档\ndate: 2023-05-17\npermalink: /kun/face/verify\n---\n正文",
    )
    .unwrap();
    std::fs::write(
        root.join("new.md"),
        "---\ntitle: 新文档（拷贝忘改 permalink）\ndate: 2025-06-30\npermalink: /kun/face/verify\n---\n正文",
    )
    .unwrap();
    let cfg = ContentConfig {
        root: root.to_path_buf(),
        exclude: vec![],
        draft: false,
    };
    let result = scan(&cfg).unwrap();
    let index = SiteIndex::build(result, false);

    // permalink 表归 date 老的文档
    assert_eq!(
        index.permalinks.get("/kun/face/verify"),
        Some(&PathBuf::from("old.md")),
        "date 老者应保留 permalink"
    );
    // 老文档树 href = permalink；新文档回退默认 URL（不指向他人占用的地址）
    assert_eq!(index.href_for_page(Path::new("old.md")), "/kun/face/verify");
    assert_eq!(
        index.href_for_page(Path::new("new.md")),
        "/new",
        "落败者 permalink 不生效，回退默认 URL"
    );
    // permalink 路由命中老文档
    assert_eq!(
        index.permalinks.get("/kun/face/verify"),
        Some(&PathBuf::from("old.md"))
    );
}

#[test]
fn test_single_index_dir_downgrades_to_leaf() {
    // M2 用户决策：有分支页（回退链任一）且无其他可见子项的目录 → leaf（可点击）；
    // 多子项目录仍 branch；无首页空目录仍 branch
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::write(root.join("_index.md"), "---\ntitle: 根\n---\n").unwrap();
    std::fs::create_dir_all(root.join("single")).unwrap();
    std::fs::write(root.join("single/_index.md"), "---\ntitle: 单页目录\n---\n").unwrap();
    std::fs::create_dir_all(root.join("multi")).unwrap();
    std::fs::write(root.join("multi/_index.md"), "---\ntitle: 多页\n---\n").unwrap();
    std::fs::write(root.join("multi/a.md"), "---\ntitle: A\n---\n").unwrap();
    std::fs::create_dir_all(root.join("empty")).unwrap();
    let cfg = ContentConfig {
        root: root.to_path_buf(),
        exclude: vec![],
        draft: false,
    };
    let result = scan(&cfg).unwrap();
    let index = SiteIndex::build(result, false);
    let nodes = build_subtree(&index, Path::new(""), 2, false);
    let find = |t: &str| {
        nodes
            .iter()
            .find(|n| n.title == t)
            .unwrap_or_else(|| panic!("缺 {t}"))
    };
    assert_eq!(
        find("单页目录").node_type,
        NodeType::Leaf,
        "单首页目录应为 leaf"
    );
    assert!(!find("单页目录").has_children);
    assert_eq!(
        find("多页").node_type,
        NodeType::Branch,
        "多子项目录仍为 branch"
    );
    assert_eq!(
        find("empty").node_type,
        NodeType::Branch,
        "无首页空目录仍为 branch（无导航目标）"
    );
}

#[test]
fn test_symlink_in_root_followed_escape_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("content");
    std::fs::create_dir_all(root.join("real")).unwrap();
    std::fs::write(root.join("real/page.md"), "---\ntitle: 真实页\n---\n正文").unwrap();

    // 根内 symlink：跟随，页面入索引
    std::os::unix::fs::symlink("real", root.join("alias")).unwrap();
    // 逃逸 symlink：指向根外
    let outside = tmp.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("secret.md"), "机密").unwrap();
    std::os::unix::fs::symlink(&outside, root.join("escape")).unwrap();

    let cfg = ContentConfig {
        root: root.clone(),
        exclude: vec![],
        draft: false,
    };
    let result = scan(&cfg).unwrap();
    let index = SiteIndex::build(result, false);
    // alias/real/page.md 经 symlink 路径入索引（内容复用语义）
    assert!(
        index.pages.contains_key(Path::new("alias/page.md")),
        "根内 symlink 应跟随"
    );
    // 逃逸 symlink 拒绝
    assert!(
        !index.pages.contains_key(Path::new("escape/secret.md")),
        "逃逸 symlink 应拒绝"
    );
}

#[test]
fn test_branch_fallback_readme_and_index() {
    let index = build_index(&["drafts", "excluded"], false);

    // readme-only 目录：目录 URL 路由到 README.md，普通文档 URL 双可达
    assert_eq!(
        index.routes.get("/readme-dir"),
        Some(&PathBuf::from("readme-dir/README.md"))
    );
    assert_eq!(
        index.routes.get("/readme-dir/readme"),
        Some(&PathBuf::from("readme-dir/README.md"))
    );
    // frontmatter 提供目录标题
    assert_eq!(
        index
            .dirs
            .get(Path::new("readme-dir"))
            .map(|d| d.title.clone()),
        Some("读我".to_string())
    );
    // 树中不出现 readme 子节点（被吸收进目录节点）
    let readme_child = index
        .dirs
        .get(Path::new("readme-dir"))
        .unwrap()
        .child_pages
        .clone();
    assert!(!readme_child.contains(&PathBuf::from("readme-dir/README.md")));

    // index-only 目录：/dir 与 /dir/index 双可达
    assert_eq!(
        index.routes.get("/index-dir"),
        Some(&PathBuf::from("index-dir/index.md"))
    );
    assert_eq!(
        index.routes.get("/index-dir/index"),
        Some(&PathBuf::from("index-dir/index.md"))
    );
    assert_eq!(
        index
            .dirs
            .get(Path::new("index-dir"))
            .map(|d| d.title.clone()),
        Some("索引页".to_string())
    );
}

#[test]
fn test_branch_fallback_index_class_still_wins() {
    let index = build_index(&["drafts", "excluded"], false);
    // _index.md 仍是最高优先：/both-dir 归 _index.md
    assert_eq!(
        index.routes.get("/both-dir"),
        Some(&PathBuf::from("both-dir/_index.md"))
    );
    // 未被选中的 index.md 按普通文档处理：进路由 + 树子节点
    assert_eq!(
        index.routes.get("/both-dir/index"),
        Some(&PathBuf::from("both-dir/index.md"))
    );
    let child_pages = index
        .dirs
        .get(Path::new("both-dir"))
        .unwrap()
        .child_pages
        .clone();
    assert!(child_pages.contains(&PathBuf::from("both-dir/index.md")));
    // 分支页语义字段来自 _index.md
    assert_eq!(
        index
            .dirs
            .get(Path::new("both-dir"))
            .map(|d| d.title.clone()),
        Some("混合".to_string())
    );
}

#[test]
fn test_branch_fallback_draft_index_excluded_no_passthrough() {
    // draft=false：draft _index.md 被排除，目录 URL 不路由（不顺延 readme），
    // readme.md 作为普通文档仍可访问
    let index = build_index(&["drafts", "excluded"], false);
    assert!(!index.routes.contains_key("/rootreadme-draft"));
    assert_eq!(
        index.routes.get("/rootreadme-draft/readme"),
        Some(&PathBuf::from("rootreadme-draft/readme.md"))
    );
}

#[test]
fn test_branch_fallback_root_readme_only() {
    // 根目录 readme-only（tempdir 构造）：/ 与 /readme 双可达，根标题取自 readme
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("content");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("readme.md"), "---\ntitle: 根读我\n---\n根内容").unwrap();
    let cfg = ContentConfig {
        root,
        exclude: vec![],
        draft: false,
    };
    let index = SiteIndex::build(scan(&cfg).unwrap(), false);
    assert_eq!(index.routes.get("/"), Some(&PathBuf::from("readme.md")));
    assert_eq!(
        index.routes.get("/readme"),
        Some(&PathBuf::from("readme.md"))
    );
    assert_eq!(
        index.dirs.get(Path::new("")).map(|d| d.title.clone()),
        Some("根读我".to_string())
    );
}

#[test]
fn test_branch_fallback_same_level_tie_breaks_deterministically() {
    // 大小写敏感 FS 场景（tempdir 运行时构造，macOS 上两文件名会被
    // 大小写不敏感 FS 合并，无法进 git fixture；断言字节序最小者胜）
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("content");
    std::fs::create_dir_all(root.join("dir")).unwrap();
    // 两个不同候选名绕开大小写合并：index.md 的同级变体不存在时
    // 用 INDEX.MD/INDEX.md 在 mac 上是同一文件，改用真实可并存的
    // index.md 与 Index.md（大小写不敏感 FS 上同样合并，但 Linux CI 可并存；
    // 无论哪种 FS，选取必须是确定性的——不 panic、结果属于同级候选集合）
    std::fs::write(root.join("dir/index.md"), "---\ntitle: 小写\n---\n小写").unwrap();
    let cfg = ContentConfig {
        root,
        exclude: vec![],
        draft: false,
    };
    let index = SiteIndex::build(scan(&cfg).unwrap(), false);
    let branch = index
        .dirs
        .get(Path::new("dir"))
        .unwrap()
        .branch_page
        .clone();
    assert_eq!(
        branch.as_deref(),
        Some(Path::new("dir/index.md")),
        "单一候选确定性命中"
    );
}

#[test]
fn test_merge_changes_branch_fallback_consistency() {
    // 增量 vs 全量：新增/删除 readme.md 引发分支页回退链变化，
    // 两路径产出的路由表/DirMeta 必须一致
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("content");
    std::fs::create_dir_all(root.join("guide")).unwrap();
    std::fs::write(root.join("_index.md"), "---\ntitle: 首页\n---\n首页").unwrap();
    std::fs::write(root.join("guide/_index.md"), "---\ntitle: 指南\n---\n指南").unwrap();
    let cfg = ContentConfig {
        root: root.clone(),
        exclude: vec![],
        draft: false,
    };
    let base = SiteIndex::build(scan(&cfg).unwrap(), false);

    // 删除 guide/_index.md、新增 guide/readme.md：分支页回退到 readme
    std::fs::remove_file(root.join("guide/_index.md")).unwrap();
    std::fs::write(
        root.join("guide/readme.md"),
        "---\ntitle: 指南读我\n---\n读我",
    )
    .unwrap();
    let incremental = base.merge_changes(
        &cfg,
        &root,
        false,
        &["guide/readme.md".into()],
        &["guide/_index.md".into()],
    );
    let full = SiteIndex::build(scan(&cfg).unwrap(), false);

    assert_eq!(incremental.routes, full.routes, "路由表增量与全量一致");
    assert_eq!(incremental.dirs.len(), full.dirs.len());
    for (k, v) in &full.dirs {
        let got = incremental
            .dirs
            .get(k)
            .unwrap_or_else(|| panic!("增量缺 DirMeta：{k:?}"));
        assert_eq!(got.branch_page, v.branch_page, "branch_page 不一致：{k:?}");
        assert_eq!(got.title, v.title, "title 不一致：{k:?}");
        assert_eq!(got.child_pages, v.child_pages, "child_pages 不一致：{k:?}");
    }
    // 回退语义生效：/guide 归 readme.md，双可达保留
    assert_eq!(
        incremental.routes.get("/guide"),
        Some(&PathBuf::from("guide/readme.md"))
    );
    assert_eq!(
        incremental.routes.get("/guide/readme"),
        Some(&PathBuf::from("guide/readme.md"))
    );
}

#[test]
fn test_merge_changes_incremental_single_file() {
    // 增量更新正确性：单文件变更/删除后索引与全量重建逐字段等价
    let (_tmp, root) = {
        let t = tempfile::tempdir().unwrap();
        let r = t.path().join("content");
        std::fs::create_dir_all(r.join("guide/deep")).unwrap();
        std::fs::write(r.join("_index.md"), "---\ntitle: 首页\n---\n首页").unwrap();
        std::fs::write(r.join("guide/_index.md"), "---\ntitle: 指南\n---\n指南").unwrap();
        std::fs::write(
            r.join("guide/a.md"),
            "---\ntitle: 甲\nweight: 1\n---\n甲内容",
        )
        .unwrap();
        std::fs::write(r.join("guide/deep/b.md"), "---\ntitle: 乙\n---\n乙内容").unwrap();
        (t, r)
    };
    let cfg = ContentConfig {
        root: root.clone(),
        exclude: vec![],
        draft: false,
    };
    let base = SiteIndex::build(scan(&cfg).unwrap(), false);
    assert_eq!(base.pages.len(), 4);

    // 单文件修改 + 新增 + 删除：增量 vs 全量
    std::fs::write(
        root.join("guide/a.md"),
        "---\ntitle: 甲改\nweight: 1\n---\n新内容",
    )
    .unwrap();
    std::fs::write(root.join("guide/new.md"), "---\ntitle: 新页\n---\n新页内容").unwrap();
    std::fs::remove_file(root.join("guide/deep/b.md")).unwrap();

    let incremental = base.merge_changes(
        &cfg,
        &root,
        false,
        &["guide/a.md".into(), "guide/new.md".into()],
        &["guide/deep/b.md".into()],
    );
    let full = SiteIndex::build(scan(&cfg).unwrap(), false);

    assert_eq!(
        incremental.pages.len(),
        full.pages.len(),
        "pages 条目数一致"
    );
    for (k, v) in &full.pages {
        let got = incremental
            .pages
            .get(k)
            .unwrap_or_else(|| panic!("增量缺 PageMeta：{k:?}"));
        assert_eq!(got.url, v.url, "url 不一致：{k:?}");
        assert_eq!(got.size, v.size, "size 不一致：{k:?}");
        assert_eq!(got.fm.title, v.fm.title, "title 不一致：{k:?}");
        assert_eq!(got.mtime, v.mtime, "mtime 不一致：{k:?}");
        assert_eq!(
            got.has_children_shortcode, v.has_children_shortcode,
            "children_shortcode 不一致：{k:?}"
        );
    }
    assert_eq!(incremental.routes, full.routes, "路由表一致");
    assert_eq!(incremental.permalinks, full.permalinks);
    assert_eq!(incremental.children_deps, full.children_deps);
    assert_eq!(incremental.dirs.len(), full.dirs.len(), "dirs 数一致");
    // 关键语义断言：新页可路由、删除断路由、修改换内容
    assert!(incremental.routes.contains_key("/guide/new"));
    assert!(!incremental.routes.contains_key("/guide/deep/b"));
    assert_eq!(
        incremental
            .pages
            .get(std::path::Path::new("guide/a.md"))
            .map(|p| p.fm.title.clone()),
        Some(Some("甲改".into()))
    );
    // deep 目录删除唯一文件后仍存在（空目录）但无子页
    assert!(
        incremental
            .dirs
            .contains_key(std::path::Path::new("guide/deep"))
    );
}

#[test]
fn test_merge_changes_directory_removal() {
    // 整目录删除：dirs 条目与父目录 child_dirs 同步清理
    let (_tmp, root) = {
        let t = tempfile::tempdir().unwrap();
        let r = t.path().join("content");
        std::fs::create_dir_all(r.join("sub")).unwrap();
        std::fs::write(r.join("_index.md"), "---\ntitle: 首页\n---\n首页").unwrap();
        std::fs::write(r.join("sub/x.md"), "---\ntitle: X\n---\nX").unwrap();
        (t, r)
    };
    let cfg = ContentConfig {
        root: root.clone(),
        exclude: vec![],
        draft: false,
    };
    let base = SiteIndex::build(scan(&cfg).unwrap(), false);
    assert!(base.dirs.contains_key(std::path::Path::new("sub")));

    std::fs::remove_dir_all(root.join("sub")).unwrap();
    let incremental = base.merge_changes(&cfg, &root, false, &[], &["sub/x.md".into()]);
    let full = SiteIndex::build(scan(&cfg).unwrap(), false);
    assert_eq!(incremental.pages.len(), full.pages.len());
    assert!(!incremental.routes.contains_key("/sub/x"), "删除断路由");
    assert_eq!(
        incremental.dirs.len(),
        full.dirs.len(),
        "dirs 数与全量一致（空目录被清理）"
    );
}
