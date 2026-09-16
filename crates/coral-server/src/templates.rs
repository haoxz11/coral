//! askama 模板注册与渲染。
//!
//! 模板编译期检查；`|safe` 豁免点收敛在 page.html 的 content_html
//! 与 layout.html 的 initial_tree_json 两处（可信内容源前提）。

use askama::Template;
use coral_core::TocEntry;
use coral_core::scanner::SiteIndex;
use coral_core::tree::build_subtree;
use std::path::Path;

/// 顶层布局数据（layout.html）。
#[derive(Template)]
#[template(path = "layout.html")]
pub struct LayoutTpl {
    pub site_title: String,
    /// 首屏树 JSON（build_subtree(root, initial_depth)）
    pub initial_tree_json: String,
    /// 站点任一节点带 icon/menuPre 时才输出 Iconify script（无图标站点零外链）
    pub has_icons: bool,
    /// 顶栏一级菜单
    pub top_nav: Vec<NavSection>,
    /// 带内容指纹的资产 URL（缓存失效正确性依赖于此）
    pub asset_css: String,
    pub asset_js: String,
    /// 错误页无侧栏
    pub has_sidebar: bool,
    /// 当前页默认形态 URL（sidebar data 属性；permalink 页左树祖先链匹配）
    pub current_default_url: String,
    /// 祖先目录 href 链（仅 PageTpl 有侧栏场景非空；其余模板空集）
    pub current_dir_hrefs: Vec<String>,
    /// 文档库图标（空串 = 无；projectIcon）
    pub project_icon: String,
    /// 页脚文案
    pub footer_text: String,
    /// Mermaid/KaTeX（M2-s3）：页面含对应占位才注入 CDN；URL 可配
    pub has_mermaid: bool,
    pub has_katex: bool,
    pub mermaid_cdn: String,
    /// katex.min.js URL（meta 数据传给前端动态加载）
    pub katex_cdn: String,
    /// katex.min.css URL（Rust 侧从 katex_cdn 派生，模板不做 filter）
    pub katex_cdn_css: String,
    /// 搜索开关（控制搜索框激活/占位形态）
    pub search_enabled: bool,
}

/// 首页三段式数据块：正文 `##` 下 `### 子标题` + 首段文本。
pub struct HomeBlock {
    pub title: String,
    pub desc: String,
}

/// 首页文档库卡片 = 一级目录/一级文档（icon+title+url）。
pub struct HomeCard {
    pub title: String,
    pub url: String,
    /// Iconify 名（空串 = 无图标）
    pub icon: String,
}

/// 门户首页（archetype=home）。
#[derive(Template)]
#[template(path = "home.html")]
pub struct HomeTpl {
    pub site_title: String,
    pub initial_tree_json: String,
    pub has_icons: bool,
    pub top_nav: Vec<NavSection>,
    pub current_default_url: String,
    pub current_dir_hrefs: Vec<String>,
    pub project_icon: String,
    pub footer_text: String,
    /// Mermaid/KaTeX（M2-s3）：页面含对应占位才注入 CDN；URL 可配
    pub has_mermaid: bool,
    pub has_katex: bool,
    pub mermaid_cdn: String,
    /// katex.min.js URL（meta 数据传给前端动态加载）
    pub katex_cdn: String,
    /// katex.min.css URL（Rust 侧从 katex_cdn 派生，模板不做 filter）
    pub katex_cdn_css: String,
    pub search_enabled: bool,
    pub has_sidebar: bool,
    /// 资产 URL
    pub asset_css: String,
    pub asset_js: String,
    /// Hero 大标题（根 _index title）
    pub page_title: String,
    /// 三段式数据块（正文解析；空 = 隐藏）
    pub blocks: Vec<HomeBlock>,
    /// 主按钮（首个人类一级文档；空 url = 隐藏）
    pub primary_action_url: String,
    pub primary_action_text: String,
    /// 文档库卡片（top_sections 自动生成）
    pub cards: Vec<HomeCard>,
}

/// 首页数据块解析：正文每个 `## 标题` 一个块，
/// 首个非空行为描述（剥列表前缀）。个数任意（2/3/5 皆可）；无块返回空。
pub fn parse_home_blocks(body: &str) -> Vec<HomeBlock> {
    let mut blocks: Vec<HomeBlock> = Vec::new();
    let mut current: Option<(String, String)> = None;
    let flush = |cur: &mut Option<(String, String)>, blocks: &mut Vec<HomeBlock>| match cur.take() {
        Some((t, d)) if !t.is_empty() || !d.is_empty() => {
            blocks.push(HomeBlock { title: t, desc: d });
        }
        _ => {}
    };
    for line in body.lines() {
        let t = line.trim();
        let heading = t
            .strip_prefix("## ")
            .map(str::trim)
            .or_else(|| t.strip_prefix("### ").map(str::trim)); // 容错旧格式
        if let Some(h) = heading {
            flush(&mut current, &mut blocks);
            current = Some((h.to_string(), String::new()));
        } else if t.is_empty() {
            // 空行 = 描述段落结束（后续文本不并入，如正文其他内容）
            if let Some((_, d)) = current.as_mut().filter(|(_, d)| !d.is_empty()) {
                d.push('\u{0}'); // 段落分隔哨兵：段落结束，后续文本不并入
            }
        } else {
            match current.as_mut() {
                Some((_, d)) if !d.ends_with('\u{0}') => {
                    // 多行合并为一段：剥列表前缀后拼接
                    let line = t
                        .trim_start_matches("- ")
                        .trim_start_matches("* ")
                        .to_string();
                    if d.is_empty() {
                        *d = line;
                    } else {
                        // 保留原始行结构（white-space: pre-line 渲染换行）
                        *d = format!("{d}\n{line}");
                    }
                }
                _ => {}
            }
        }
    }
    flush(&mut current, &mut blocks);
    blocks
}

/// frontmatter `url` 入参解析：markdown 链接语法
/// `[文案](地址)` → (文案, 地址)；格式不符返回 None。
pub fn parse_home_action(raw: &str) -> Option<(String, String)> {
    let raw = raw.trim();
    let text_start = raw.find('[')?;
    let text_end = raw[text_start..].find("](")? + text_start;
    let href_start = text_end + 2;
    let href_end = raw[href_start..].rfind(')')? + href_start;
    let text = raw[text_start + 1..text_end].trim().to_string();
    let href = raw[href_start..href_end].trim().to_string();
    (!text.is_empty() && !href.is_empty()).then_some((text, href))
}

/// 顶栏一级菜单项。
pub struct NavSection {
    pub title: String,
    pub url: String,
    pub active: bool,
    /// 一级目录 _index 的 icon（空串 = 无；askama 0.16 不支持 Option 字段）
    pub icon: String,
}

/// 面包屑项。
pub struct Crumb {
    pub title: String,
    pub url: String,
    pub current: bool,
}

/// 文档页（page.html）。askama 0.16 不支持 Option 字段：
/// prev/next/date 用空串表示缺失。
#[derive(Template)]
#[template(path = "page.html")]
pub struct PageTpl {
    pub site_title: String,
    pub initial_tree_json: String,
    /// 同 LayoutTpl：条件输出 Iconify script
    pub has_icons: bool,
    /// 带内容指纹的资产 URL
    pub asset_css: String,
    pub asset_js: String,
    /// 错误页无侧栏
    pub has_sidebar: bool,
    /// 顶栏一级菜单
    pub top_nav: Vec<NavSection>,
    /// 当前页默认形态 URL（decode；permalink 页用于左树祖先链匹配）
    pub current_default_url: String,
    /// 当前页祖先目录 href 链（encode 形态，含根）；目录带 permalink 时
    /// 与子页面 URL 无前缀关系，侧栏展开判定不能按前缀推断
    pub current_dir_hrefs: Vec<String>,
    /// 文档库图标（空串 = 无）
    pub project_icon: String,
    /// 页脚文案
    pub footer_text: String,
    /// Mermaid/KaTeX（M2-s3）：页面含对应占位才注入 CDN；URL 可配
    pub has_mermaid: bool,
    pub has_katex: bool,
    pub mermaid_cdn: String,
    /// katex.min.js URL（meta 数据传给前端动态加载）
    pub katex_cdn: String,
    /// katex.min.css URL（Rust 侧从 katex_cdn 派生，模板不做 filter）
    pub katex_cdn_css: String,
    /// 搜索开关
    pub search_enabled: bool,
    /// 正文无 <h1 时注入页面标题为主标题
    pub inject_title: bool,
    pub page_title: String,
    pub content_html: String,
    pub toc: Vec<TocEntry>,
    pub breadcrumbs: Vec<Crumb>,
    pub date_footer: String,
}

/// 错误页（error.html，统一错误页）。
#[derive(Template)]
#[template(path = "error.html")]
pub struct ErrorTpl {
    pub site_title: String,
    pub initial_tree_json: String,
    /// 同 LayoutTpl：条件输出 Iconify script
    pub has_icons: bool,
    /// 带内容指纹的资产 URL
    pub asset_css: String,
    pub asset_js: String,
    /// 错误页无侧栏
    pub has_sidebar: bool,
    /// 顶栏一级菜单（错误页传空，仅站点标题）
    pub top_nav: Vec<NavSection>,
    /// 当前页默认形态 URL（错误页空）
    pub current_default_url: String,
    /// 祖先目录 href 链（错误页空）
    pub current_dir_hrefs: Vec<String>,
    /// 文档库图标（空串 = 无）
    pub project_icon: String,
    /// 页脚文案
    pub footer_text: String,
    /// Mermaid/KaTeX（M2-s3）：页面含对应占位才注入 CDN；URL 可配
    pub has_mermaid: bool,
    pub has_katex: bool,
    pub mermaid_cdn: String,
    /// katex.min.js URL（meta 数据传给前端动态加载）
    pub katex_cdn: String,
    /// katex.min.css URL（Rust 侧从 katex_cdn 派生，模板不做 filter）
    pub katex_cdn_css: String,
    /// 搜索开关（错误页保持关闭形态）
    pub search_enabled: bool,
    pub code: u16,
    pub message: String,
}

/// 结果页单条命中（视图模型）：SearchHit + 预格式化字段
/// （askama 无日期过滤器，epoch 秒 → `YYYY-MM-DD` 在 Rust 侧算好）。
pub struct SearchHitView {
    pub title: String,
    /// 详情页链接（含 ?hl= 分词参数，token 以逗号连接）
    pub url: String,
    pub snippet: String,
    pub dir_path: String,
    /// `YYYY-MM-DD`（epoch 0 → "—"）
    pub date_display: String,
}

/// 搜索结果页（search.html）。enabled/building 用 0/1 表示
/// （askama 0.16 不支持 Option 与独立 bool 分支组合的简洁写法）。
#[derive(Template)]
#[template(path = "search.html")]
pub struct SearchTpl {
    pub site_title: String,
    pub initial_tree_json: String,
    pub has_icons: bool,
    pub asset_css: String,
    pub asset_js: String,
    pub has_sidebar: bool,
    pub top_nav: Vec<NavSection>,
    pub current_default_url: String,
    pub current_dir_hrefs: Vec<String>,
    pub project_icon: String,
    pub footer_text: String,
    /// Mermaid/KaTeX（M2-s3）：页面含对应占位才注入 CDN；URL 可配
    pub has_mermaid: bool,
    pub has_katex: bool,
    pub mermaid_cdn: String,
    /// katex.min.js URL（meta 数据传给前端动态加载）
    pub katex_cdn: String,
    /// katex.min.css URL（Rust 侧从 katex_cdn 派生，模板不做 filter）
    pub katex_cdn_css: String,
    pub search_enabled: bool,
    pub query: String,
    pub enabled: u8,
    pub building: u8,
    pub hits: Vec<SearchHitView>,
    /// 命中总数（分页前）
    pub total: usize,
    /// 当前页（1 起）
    pub page: usize,
    /// 总页数（0 = 无结果）
    pub pages_total: usize,
    /// 页码列表（窗口式，含省略号哨兵 0）
    pub page_numbers: Vec<usize>,
    /// 查询串 percent-encode（分页链接回填）
    pub query_encoded: String,
    /// ?hl= 参数（tokens 逗号连接，已 encode）
    pub hl_param: String,
}

/// epoch 秒 → `YYYY-MM-DD`（本地时区近似即可，展示用途）。
pub fn epoch_to_date(epoch: i64) -> String {
    if epoch <= 0 {
        return "—".to_string();
    }
    let days = epoch / 86400;
    let leap = |y: i64| (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let mut y = 1970i64;
    let mut rem = days;
    loop {
        let dy = if leap(y) { 366 } else { 365 };
        if rem < dy {
            break;
        }
        rem -= dy;
        y += 1;
    }
    let days_in = |m: i64| match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if leap(y) => 29,
        _ => 28,
    };
    let mut m = 1i64;
    while rem >= days_in(m) {
        rem -= days_in(m);
        m += 1;
    }
    format!("{y:04}-{m:02}-{:02}", rem + 1)
}

/// 页码窗口：总数 ≤7 全列；否则首尾页 + 当前页±2（0 = 省略号哨兵）。
pub fn page_window(page: usize, pages_total: usize) -> Vec<usize> {
    if pages_total <= 7 {
        return (1..=pages_total).collect();
    }
    let mut v = vec![1usize];
    let lo = page.saturating_sub(2).max(2);
    let hi = (page + 2).min(pages_total - 1);
    if lo > 2 {
        v.push(0);
    }
    v.extend(lo..=hi);
    if hi < pages_total - 1 {
        v.push(0);
    }
    v.push(pages_total);
    v
}

#[cfg(test)]
mod search_tpl_tests {
    use super::{epoch_to_date, page_window};

    #[test]
    fn test_epoch_to_date() {
        assert_eq!(epoch_to_date(0), "—");
        assert_eq!(epoch_to_date(1740787200), "2025-03-01");
        assert_eq!(epoch_to_date(86400), "1970-01-02");
    }

    #[test]
    fn test_page_window() {
        assert_eq!(page_window(1, 5), vec![1, 2, 3, 4, 5]);
        assert_eq!(page_window(1, 9), vec![1, 2, 3, 0, 9]);
        assert_eq!(page_window(5, 9), vec![1, 0, 3, 4, 5, 6, 7, 0, 9]);
        assert_eq!(page_window(9, 9), vec![1, 0, 7, 8, 9]);
        // 边界：中间页靠近首尾页不产生多余省略号
        assert_eq!(page_window(2, 9), vec![1, 2, 3, 4, 0, 9]);
        assert_eq!(page_window(8, 9), vec![1, 0, 6, 7, 8, 9]);
    }
}

/// 页脚文案：`[server].footer` 配置 > 首页（根 _index）
/// title > "coral" 兜底。
pub fn footer_text(index: &SiteIndex, cfg_footer: Option<&str>) -> String {
    let cfg = cfg_footer.map(str::trim).filter(|s| !s.is_empty());
    cfg.map(str::to_string).unwrap_or_else(|| site_title(index))
}

/// katex CSS URL：js.min.js → js.min.css（同目录同版本）。
pub fn katex_css_url(katex_cdn: &str) -> String {
    katex_cdn.replace(".min.js", ".min.css")
}

pub fn site_title(index: &SiteIndex) -> String {
    index
        .dirs
        .get(Path::new(""))
        .and_then(|d| d.branch_page.as_ref())
        .and_then(|bp| index.pages.get(bp))
        .and_then(|p| p.fm.title.clone())
        .unwrap_or_else(|| "coral".to_string())
}

/// 文档库图标：根 `_index.md` 的 `icon`（根 _index 的
/// title/icon 即库标识，不引入新字段；None = 不显示）。
pub fn project_icon(index: &SiteIndex) -> Option<String> {
    index
        .dirs
        .get(Path::new(""))
        .and_then(|d| d.branch_page.as_ref())
        .and_then(|bp| index.pages.get(bp))
        .and_then(|p| p.fm.icon.clone())
}

/// 站点是否任一页面/目录带 icon（决定 layout 是否输出 Iconify script）。
pub fn has_icons(index: &SiteIndex) -> bool {
    index.pages.values().any(|p| p.fm.icon.is_some())
}

/// 首屏树 JSON（initial_depth 层级）。
pub fn initial_tree_json(index: &SiteIndex, depth: usize, draft_enabled: bool) -> String {
    initial_tree_json_scoped(index, Path::new(""), depth, draft_enabled)
}

/// scoped 首屏树：以指定目录为根（含其自身节点，
/// 树深 +1 展开一层供首屏）；首页/一级文档页由调用方传根（配合
/// has_sidebar=false 不显示侧栏）。
pub fn initial_tree_json_scoped(
    index: &SiteIndex,
    scope: &Path,
    depth: usize,
    draft_enabled: bool,
) -> String {
    // scoped：build_subtree(scope) 返回该目录的子节点，需再包一层
    // 目录自身节点作为树根（左树从一级目录开始）
    let children = build_subtree(index, scope, depth, draft_enabled);
    let Some(dm) = index.dirs.get(scope) else {
        return "[]".to_string();
    };
    let root = coral_core::tree::TreeNode {
        url: index.href_for_dir(scope),
        title: dm.title.clone(),
        weight: dm.weight,
        icon: dm.icon.clone(),
        has_index: dm.branch_page.is_some(),
        node_type: coral_core::tree::NodeType::Branch,
        has_children: !children.is_empty(),
        children,
    };
    serde_json::to_string(&[root]).unwrap_or_else(|_| "[]".to_string())
}

/// 顶栏一级菜单（含 active 判定：当前页 URL 与菜单 URL 同前缀）。
pub fn top_nav(index: &SiteIndex, current_url: &str) -> Vec<NavSection> {
    let cur = current_url.trim_end_matches('/');
    index
        .top_sections()
        .into_iter()
        .map(|ts| {
            let sec = ts.url.trim_end_matches('/');
            let active = cur == sec || cur.starts_with(&format!("{sec}/"));
            NavSection {
                title: ts.title,
                url: ts.url,
                active,
                icon: ts.icon.unwrap_or_default(),
            }
        })
        .collect()
}

/// 面包屑：当前页的祖先目录链。
///
/// - 根分支页（首页）：返回空（单项面包屑无意义）
/// - 一级目录分支页：`首页 / 一级菜单名`——末项当前页标题与
///   一级菜单名重复，去当前页项，一级目录项改为 current
/// - 深层页面：`首页 / … / 当前页` 不变
pub fn breadcrumbs(index: &SiteIndex, page_rel: &Path) -> Vec<Crumb> {
    let mut crumbs = Vec::new();
    // 首页（根分支页，parent 为空路径）：无面包屑
    if page_rel.parent().is_some_and(std::path::Path::is_empty) {
        return crumbs;
    }
    let mut dir = page_rel.parent();
    while let Some(d) = dir {
        // 从内向外收集，最后反转
        let title = if d.as_os_str().is_empty() {
            // 根项固定"首页"（站点标题已在顶栏）
            "首页".to_string()
        } else {
            index
                .dirs
                .get(d)
                .map(|dm| dm.title.clone())
                .unwrap_or_else(|| {
                    d.file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default()
                })
        };
        crumbs.push(Crumb {
            title,
            url: index.href_for_dir(d),
            current: false,
        });
        dir = d.parent();
    }
    crumbs.reverse();
    // 末项：当前页
    let page = index.pages.get(page_rel);
    let (title, url) = page
        .map(|p| {
            (
                p.fm.title.clone().unwrap_or_else(|| {
                    page_rel
                        .file_stem()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default()
                }),
                index.href_for_page(page_rel),
            )
        })
        .unwrap_or_default();
    // 目录分支页（含一级与深层，_index/index/readme 回退链被选中者）：
    // 末项当前页标题与所在目录项重复（同为目录 title）——去掉当前页项，
    // 目录项标 current。按 is_branch 判定而非文件名（回退链语义）
    if page.is_some_and(|p| p.is_branch) {
        if let Some(last) = crumbs.last_mut() {
            last.current = true;
        }
        return crumbs;
    }
    crumbs.push(Crumb {
        title,
        url,
        current: true,
    });
    crumbs
}

#[cfg(test)]
mod tests {
    use super::*;
    use coral_core::config::ContentConfig;
    use coral_core::scanner::{SiteIndex, scan};

    fn fixture_index() -> SiteIndex {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../coral-core/tests/fixtures/site");
        let cfg = ContentConfig {
            root,
            exclude: vec!["drafts".into(), "excluded".into()],
            draft: false,
        };
        SiteIndex::build(scan(&cfg).unwrap(), false)
    }

    #[test]
    fn test_site_title_from_root_index() {
        let index = fixture_index();
        assert_eq!(site_title(&index), "站点首页");
    }

    #[test]
    fn test_breadcrumbs_chain() {
        let index = fixture_index();
        let crumbs = breadcrumbs(&index, Path::new("guide/advanced/topic.md"));
        // 根（首页）→ guide → advanced → 当前页
        assert_eq!(crumbs.len(), 4);
        assert_eq!(crumbs[0].title, "首页"); // 根项固定"首页"
        assert_eq!(crumbs[1].title, "指南");
        assert_eq!(crumbs[2].title, "进阶");
        assert_eq!(crumbs[3].title, "深入主题");
        assert!(crumbs[3].current);
    }

    #[test]
    fn test_parse_home_blocks_three_sections() {
        // 首个 ## 章节下的 ### 子标题 + 首行描述；
        // ## 直接做块，个数任意；此处 5 块验证
        let body = "## 润物无声\n- 顺从标准，完美兼容。\n## 优雅高效\n- 只需简单引用。\n## 坚实底座\n- 海量特性支撑。\n## 四\n- d4\n## 五\n- d5";
        let blocks = parse_home_blocks(body);
        assert_eq!(blocks.len(), 5);
        assert_eq!(blocks[0].title, "润物无声");
        assert_eq!(blocks[4].title, "五");
        // 混合格式：## 与 ### 都成块（## 设计思想 + 2 个 ### = 3）
        let old = "## 设计思想\n### 润物无声\n- x\n### 优雅高效\n- y";
        let mixed = parse_home_blocks(old);
        assert_eq!(mixed.len(), 3);
        assert_eq!(mixed[1].title, "润物无声");
        assert_eq!(blocks[0].title, "润物无声");
        assert_eq!(blocks[0].desc, "顺从标准，完美兼容。");
        assert_eq!(blocks[2].title, "坚实底座");
        // 第二个 ## 后的 ### 不采集
        assert!(!blocks.iter().any(|b| b.title == "不采集"));
    }

    #[test]
    fn test_breadcrumbs_root_page_empty_and_top_dir_short() {
        let index = fixture_index();
        // 首页（根 _index.md）：无面包屑
        assert!(breadcrumbs(&index, Path::new("_index.md")).is_empty());
        // 一级目录 _index：首页 / 一级菜单名（两项，末项 current，无重复标题）
        let crumbs = breadcrumbs(&index, Path::new("guide/_index.md"));
        assert_eq!(crumbs.len(), 2);
        assert_eq!(crumbs[0].title, "首页");
        assert_eq!(crumbs[1].title, "指南");
        assert!(
            crumbs[1].current,
            "一级目录项应为 current（当前页项已去重）"
        );
    }

    #[test]
    fn test_breadcrumbs_fallback_branch_page_dedup() {
        let index = fixture_index();
        // readme/index 充当分支页：与 _index 同语义去重（末项与目录 title 重复，目录项 current）
        let crumbs = breadcrumbs(&index, Path::new("readme-dir/README.md"));
        assert_eq!(crumbs.len(), 2, "首页 / 读我——当前页项应与目录项去重");
        assert_eq!(crumbs[1].title, "读我");
        assert!(crumbs[1].current, "readme 分支页的目录项应为 current");

        let crumbs2 = breadcrumbs(&index, Path::new("index-dir/index.md"));
        assert_eq!(crumbs2.len(), 2);
        assert_eq!(crumbs2[1].title, "索引页");
        assert!(crumbs2[1].current, "index 分支页的目录项应为 current");
    }

    #[test]
    fn test_breadcrumbs_unselected_index_keeps_page_item() {
        let index = fixture_index();
        // 未被选中的 index.md（both-dir 有 _index.md）：普通文档，
        // 末项保留当前页项（不与目录 title 去重）
        let crumbs = breadcrumbs(&index, Path::new("both-dir/index.md"));
        assert_eq!(crumbs.len(), 3, "首页 / 混合 / 普通索引");
        assert_eq!(crumbs[1].title, "混合");
        assert_eq!(crumbs[2].title, "普通索引");
        assert!(crumbs[2].current);
    }

    #[test]
    fn test_initial_tree_json_valid() {
        let index = fixture_index();
        let json = initial_tree_json(&index, 2, false);
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(v.as_array().is_some_and(|a| !a.is_empty()));
    }
}
