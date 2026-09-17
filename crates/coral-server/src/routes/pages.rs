//! 页面路由。
//!
//! 请求流：decode URL → permalink 表优先 → routes 表 →
//! stat 源文件 mtime+size 兜底 → SWR 三分支。

use crate::routes::embedded_assets::fingerprinted_url;
use crate::state::AppState;
use crate::templates::{PageTpl, breadcrumbs, has_icons, site_title};
use askama::Template as _;
use axum::extract::{Path, State};
use axum::response::{Html, IntoResponse, Response};
use coral_core::cache::CacheStore;
use coral_core::frontmatter;
use coral_core::render::{RenderError, render_page};
use coral_core::scanner::{PageMeta, is_draft_excluded};
use coral_core::shortcode::UnknownShortcodeCounter;
use coral_core::url as core_url;
use std::path::{Path as FsPath, PathBuf};
use std::sync::Arc;
use tracing::{error, info, warn};

pub async fn page_handler(
    State(state): State<Arc<AppState>>,
    Path(path): Path<String>,
) -> Response {
    serve_page(&state, &path).await
}

/// 根页面 `/`。
pub async fn root_handler(State(state): State<Arc<AppState>>) -> Response {
    serve_page(&state, "/").await
}

async fn serve_page(state: &Arc<AppState>, raw_path: &str) -> Response {
    // decode：axum Path 已 percent-decode；显式再走一次逐段 decode
    // 以覆盖自定义编码形态，失败即 404（不泄露原因）。
    // axum 的 {*path} 提取不含前导斜杠，统一补齐与路由 key（/ 开头）同构
    let normalized = if raw_path.starts_with('/') {
        raw_path.to_string()
    } else {
        format!("/{raw_path}")
    };
    let decoded = match core_url::decode_url(&normalized) {
        Ok(d) => d,
        Err(e) => {
            info!(%e, raw_path, "URL 解码失败，404");
            return not_found(state);
        }
    };

    let index = state.snapshot();
    let draft_enabled = state.cfg.content.draft;

    // permalink 优先；尾斜杠双向兼容：
    // 声明与访问任意一方带尾斜杠均可命中（文档 URL 尾斜杠多来自手写/
    // 外部引用；目录 URL 本就无尾斜杠形态，目录优先裁决保证
    // 补/去斜杠不会错配到别的资源）
    let with_slash = if decoded.ends_with('/') {
        decoded.clone()
    } else {
        format!("{decoded}/")
    };
    let without_slash = decoded.trim_end_matches('/').to_string();
    let lookup = |u: &str| -> Option<PathBuf> {
        index
            .permalinks
            .get(u)
            .or_else(|| index.routes.get(u))
            .cloned()
    };
    let rel_path: PathBuf = match lookup(&decoded)
        .or_else(|| lookup(&without_slash))
        .or_else(|| lookup(&with_slash))
    {
        Some(rp) => rp,
        None => return not_found(state),
    };

    let Some(page) = index.pages.get(&rel_path).cloned() else {
        return not_found(state);
    };

    // 左树 scoped 到当前一级目录；首页/一级文档无侧栏
    let scope = index.top_section_of(&rel_path);
    let has_sidebar = scope.is_some();
    let top_nav = crate::templates::top_nav(&index, &decoded, Some(&rel_path));

    // 门户首页（archetype=home）
    if page.fm.archetype.as_deref() == Some("home") {
        let (blocks, cards, primary) = {
            // 正文从磁盘读（PageMeta 不驻留正文），front matter 之后的 body 才是三段块素材
            let body = std::fs::read_to_string(state.content_root.join(&page.rel_path))
                .ok()
                .and_then(|raw| raw.split_once("\n---\n").map(|(_, b)| b.to_string()))
                .unwrap_or_default();
            let blocks = crate::templates::parse_home_blocks(&body);
            let cards: Vec<crate::templates::HomeCard> = index
                .top_sections()
                .iter()
                .map(|ts| crate::templates::HomeCard {
                    title: ts.title.clone(),
                    url: ts.url.clone(),
                    icon: ts.icon.clone().unwrap_or_default(),
                })
                .collect();
            let primary = page
                .fm
                .url
                .as_deref()
                .and_then(crate::templates::parse_home_action);
            (blocks, cards, primary)
        };
        let (primary_text, primary_url) = primary.unwrap_or_default();
        let tpl = crate::templates::HomeTpl {
            site_title: site_title(&index),
            initial_tree_json: "[]".to_string(),
            has_icons: has_icons(&index),
            top_nav,
            current_default_url: page.url.clone(),
            current_dir_hrefs: Vec::new(), // 首页无侧栏
            project_icon: crate::templates::project_icon(&index).unwrap_or_default(),
            footer_text: crate::templates::footer_text(&index, state.cfg.server.footer.as_deref()),
            has_mermaid: false,
            has_katex: false,
            mermaid_cdn: state.cfg.render.mermaid_cdn.clone(),
            katex_cdn: state.cfg.render.katex_cdn.clone(),
            katex_cdn_css: crate::templates::katex_css_url(&state.cfg.render.katex_cdn),
            search_enabled: state.cfg.search.enabled,
            has_sidebar: false,
            asset_css: fingerprinted_url("app.css"),
            asset_js: fingerprinted_url("app.js"),
            page_title: page.fm.title.clone().unwrap_or_default(),
            blocks,
            primary_action_url: primary_url,
            primary_action_text: primary_text,
            cards,
        };
        return match tpl.render() {
            Ok(html) => Html(html).into_response(),
            Err(e) => {
                error!(%e, "home 模板渲染失败");
                internal_error(state)
            }
        };
    }

    // draft 不暴露存在性：与 404 同响应
    if is_draft_excluded(&page.fm, draft_enabled) {
        return not_found(state);
    }
    // front matter 解析失败的页面：500，区分于 404
    if let Some(fm_err) = &page.fm_parse_error {
        error!(
            rel_path = %rel_path.display(),
            error = %fm_err,
            "render_failed：front matter 解析失败"
        );
        return internal_error(state);
    }

    let root = state.content_root.clone();
    match serve_cached_or_render(state, &page, &root).await {
        Ok(cached) => {
            // 渲染产物只是片段——套 layout（面包屑/TOC/上下篇/date）
            let initial_tree_json = match &scope {
                Some(dir) => crate::templates::initial_tree_json_scoped(
                    &index,
                    dir,
                    state.cfg.tree.initial_depth,
                    draft_enabled,
                ),
                None => "[]".to_string(),
            };
            let tpl = PageTpl {
                site_title: site_title(&index),
                initial_tree_json,
                has_icons: has_icons(&index),
                asset_css: fingerprinted_url("app.css"),
                asset_js: fingerprinted_url("app.js"),
                has_sidebar,
                top_nav,
                current_default_url: page.url.clone(),
                current_dir_hrefs: index.ancestor_dir_hrefs(&page.rel_path),
                project_icon: crate::templates::project_icon(&index).unwrap_or_default(),
                footer_text: crate::templates::footer_text(
                    &index,
                    state.cfg.server.footer.as_deref(),
                ),
                search_enabled: state.cfg.search.enabled,
                has_mermaid: cached.html.contains("class=\"mermaid\""),
                has_katex: cached.html.contains("katex-block")
                    || cached.html.contains("katex-inline"),
                mermaid_cdn: state.cfg.render.mermaid_cdn.clone(),
                katex_cdn: state.cfg.render.katex_cdn.clone(),
                katex_cdn_css: crate::templates::katex_css_url(&state.cfg.render.katex_cdn),
                // 正文无 <h1 时模板注入页面标题为主标题；
                // 代码块内容已转义（&lt;h1）不会误判
                inject_title: !cached.html.contains("<h1"),
                // title fallback 与菜单名读取同源：file_stem + 剥数字前缀
                page_title: page.fm.title.clone().unwrap_or_else(|| {
                    page.rel_path
                        .file_stem()
                        .map(|n| {
                            coral_core::scanner::strip_numeric_prefix(&n.to_string_lossy())
                                .to_string()
                        })
                        .unwrap_or_default()
                }),
                content_html: cached.html,
                toc: cached.toc,
                breadcrumbs: breadcrumbs(&index, &page.rel_path),
                date_footer: cached.date_footer.unwrap_or_default(),
            };
            match tpl.render() {
                Ok(html) => Html(html).into_response(),
                Err(e) => {
                    error!(%e, "模板渲染失败");
                    internal_error(state)
                }
            }
        }
        Err(e) => {
            error!(
                rel_path = %page.rel_path.display(),
                error = %e,
                "render_failed"
            );
            internal_error(state)
        }
    }
}

/// 渲染产物缓存包装：html + toc + date 一起进片段文件（JSON），
/// 命中时一次读出全部布局数据（避免缓存命中路径重复渲染提 TOC）。
#[derive(serde::Serialize, serde::Deserialize)]
struct CachedPage {
    html: String,
    toc: Vec<coral_core::TocEntry>,
    date_footer: Option<String>,
}

fn serialize_cached(page: &coral_core::RenderedPage) -> String {
    serde_json::to_string(&CachedPage {
        html: page.html.clone(),
        toc: page.toc.clone(),
        date_footer: page.date_footer.clone(),
    })
    .unwrap_or_else(|_| page.html.clone())
}

fn deserialize_cached(raw: &str) -> CachedPage {
    serde_json::from_str(raw).unwrap_or(CachedPage {
        html: raw.to_string(),
        toc: Vec::new(),
        date_footer: None,
    })
}

/// SWR 三分支：
/// 1. 缓存命中且源文件未变 → 直接返回
/// 2. 缓存命中但源文件已变 → 返回旧内容 + 后台重渲染（singleflight 去重）
/// 3. 无缓存 → singleflight 下同步渲染
async fn serve_cached_or_render(
    state: &Arc<AppState>,
    page: &PageMeta,
    root: &FsPath,
) -> Result<CachedPage, String> {
    let rel = page.rel_path.to_string_lossy().into_owned();

    if let Some(lookup) = state.cache.lookup_page(&rel).filter(|l| l.fragment_exists) {
        // stat 源文件（mtime 兜底；页面查文件自身）
        let stat = tokio::task::spawn_blocking({
            let path = root.join(&page.rel_path);
            move || std::fs::metadata(path).map(|m| (m.modified().ok(), m.len()))
        })
        .await
        .map_err(|e| e.to_string())?;
        let stale = match stat {
            Ok((Some(mtime), size)) => CacheStore::is_stale(&lookup.entry, mtime, size),
            _ => true, // 源文件消失：视同 miss 重新渲染（由 render 报错兜底）
        };
        if let Some(raw) = state
            .cache
            .read_page_fragment(&rel)
            .map_err(|e| e.to_string())?
        {
            if stale {
                // SWR：旧内容先回，后台重渲染
                let state = state.clone();
                let page = page.clone();
                tokio::spawn(async move {
                    if let Err(e) = render_and_store(&state, &page).await {
                        warn!(rel_path = %page.rel_path.display(), %e, "后台重渲染失败（旧内容仍服务）");
                    }
                });
            }
            return Ok(deserialize_cached(&raw));
        }
    }

    render_and_store(state, page).await
}

/// singleflight 下的渲染 + 落盘 + 更新内存 manifest。
/// 被去重的等待者被唤醒后重查缓存——首渲染者已落盘则直接命中；
/// 仍未命中（如缓存不可写）则再执行一轮（此时在途已清，无放大）。
async fn render_and_store(state: &Arc<AppState>, page: &PageMeta) -> Result<CachedPage, String> {
    let rel = page.rel_path.to_string_lossy().into_owned();
    let outcome = state
        .flights
        .do_once(rel.clone(), || {
            let state = state.clone();
            let page = page.clone();
            let rel = rel.clone();
            async move {
                let root = state.content_root.clone();
                state
                    .render_count
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let rendered = tokio::task::spawn_blocking({
                    let page = page.clone();
                    let index = state.snapshot();
                    let draft_enabled = state.cfg.content.draft;
                    let render_cfg = state.cfg.render.clone();
                    move || render_one(&index, &root, &page, draft_enabled, &render_cfg)
                })
                .await
                .map_err(|e| e.to_string())??;

                let _ = state
                    .cache
                    .store_page(
                        &rel,
                        &page.url,
                        page.mtime,
                        page.size,
                        &serialize_cached(&rendered),
                        page.has_children_shortcode,
                    )
                    .map_err(|e| {
                        // 缓存不可写：服务继续（故障矩阵），内容照常返回
                        warn!(%e, "缓存写入失败，服务继续");
                        e
                    });
                Ok(CachedPage {
                    html: rendered.html,
                    toc: rendered.toc,
                    date_footer: rendered.date_footer,
                })
            }
        })
        .await;

    match outcome {
        crate::singleflight::Outcome::Executed(result) => result,
        crate::singleflight::Outcome::Deduplicated => {
            // 首渲染者已完成：重查缓存
            state
                .cache
                .read_page_fragment(&rel)
                .map_err(|e| e.to_string())?
                .map(|raw| deserialize_cached(&raw))
                .ok_or_else(|| "去重后缓存仍缺失".to_string())
        }
    }
}

fn render_one(
    index: &coral_core::scanner::SiteIndex,
    root: &FsPath,
    page: &PageMeta,
    draft_enabled: bool,
    render_cfg: &coral_core::RenderConfig,
) -> Result<coral_core::RenderedPage, String> {
    let raw = std::fs::read_to_string(root.join(&page.rel_path)).map_err(|e| e.to_string())?;
    let (fm, body) = match frontmatter::parse(&raw) {
        Ok(v) => v,
        Err(e) => return Err(format!("front matter: {e}")),
    };
    let mut counter = UnknownShortcodeCounter::default();
    render_page(
        body,
        &page.rel_path,
        index,
        draft_enabled,
        fm.disable_toc,
        fm.date.as_deref(),
        render_cfg,
        &mut counter,
    )
    .map_err(|e: RenderError| e.to_string())
}

fn not_found(state: &Arc<AppState>) -> Response {
    crate::errorpage::error_page(state, 404, "页面不存在")
}

fn internal_error(state: &Arc<AppState>) -> Response {
    crate::errorpage::error_page(state, 500, "服务内部错误")
}

/// `GET /search?q=&page=`：服务端渲染搜索结果页。
/// 未开启/构建中/空查询各有提示态；正常态渲染命中列表（分页，每页 20）。
pub async fn search_page(
    State(state): State<Arc<AppState>>,
    axum::extract::RawQuery(raw): axum::extract::RawQuery,
) -> Response {
    use askama::Template as _;
    let (q, page_raw) = match raw.as_deref() {
        Some(s) if s.contains('&') => {
            // q=...&page=N / q=...&page=N&...：按 & 分段取值
            let mut q_val = String::new();
            let mut page_val = 1usize;
            for seg in s.split('&') {
                if let Some(v) = seg.strip_prefix("q=") {
                    q_val = v.replace('+', " ");
                    if let Some(d) = urldecode(&q_val) {
                        q_val = d;
                    }
                } else if let Some(v) = seg.strip_prefix("page=") {
                    page_val = v.parse().unwrap_or(1).max(1);
                }
            }
            (q_val, page_val)
        }
        Some(s) => (
            s.strip_prefix("q=")
                .map(|v| v.replace('+', " "))
                .and_then(|v| urldecode(&v))
                .unwrap_or_default(),
            1usize,
        ),
        None => (String::new(), 1usize),
    };
    let page = page_raw;

    let index = state.snapshot();
    let (enabled, building, hits, total, tokens) = if !state.search.is_enabled() {
        (0u8, 0u8, Vec::new(), 0usize, Vec::new())
    } else if q.is_empty() {
        (1u8, 0u8, Vec::new(), 0usize, Vec::new())
    } else {
        match state.search.ready_index() {
            None => (1u8, 1u8, Vec::new(), 0usize, Vec::new()),
            Some(si) => {
                let q2 = q.clone();
                // 全量取回（内部 limit 上限 50 → 分页切片在服务端）
                let result = tokio::task::spawn_blocking(move || si.search(&q2, 50)).await;
                let (hits, tokens) = match result {
                    Ok(Ok(v)) => v,
                    Ok(Err(coral_core::search::SearchError::Query(e))) => {
                        // 输入中间态，预期情况不告警（与 /api/search 同口径）
                        tracing::debug!(%e, "搜索查询未完成解析（结果页）");
                        (Vec::new(), Vec::new())
                    }
                    Ok(Err(e)) => {
                        tracing::warn!(%e, "搜索查询失败（结果页）");
                        (Vec::new(), Vec::new())
                    }
                    Err(e) => {
                        tracing::warn!(%e, "搜索查询 join 失败（结果页）");
                        (Vec::new(), Vec::new())
                    }
                };
                let total = hits.len();
                (1u8, 0u8, hits, total, tokens)
            }
        }
    };

    // 分页：每页 20
    const PER_PAGE: usize = 20;
    let pages_total = total.div_ceil(PER_PAGE);
    let page = page.clamp(1, pages_total.max(1));
    let views: Vec<crate::templates::SearchHitView> = hits
        .into_iter()
        .skip((page - 1) * PER_PAGE)
        .take(PER_PAGE)
        .map(|h| crate::templates::SearchHitView {
            title: h.title,
            // ?hl= 由模板统一拼接（Rust 侧只存纯 URL，避免重复参数）
            url: h.url,
            snippet: h.snippet,
            dir_path: h.dir_path,
            date_display: crate::templates::epoch_to_date(h.date),
        })
        .collect();

    let tpl = crate::templates::SearchTpl {
        site_title: site_title(&index),
        initial_tree_json: "[]".to_string(),
        has_icons: has_icons(&index),
        asset_css: crate::routes::embedded_assets::fingerprinted_url("app.css"),
        asset_js: crate::routes::embedded_assets::fingerprinted_url("app.js"),
        has_sidebar: false,
        top_nav: crate::templates::top_nav(&index, "", None),
        current_default_url: String::new(),
        current_dir_hrefs: Vec::new(), // 搜索结果页无侧栏
        project_icon: crate::templates::project_icon(&index).unwrap_or_default(),
        footer_text: crate::templates::footer_text(&index, state.cfg.server.footer.as_deref()),
        has_mermaid: false,
        has_katex: false,
        mermaid_cdn: state.cfg.render.mermaid_cdn.clone(),
        katex_cdn: state.cfg.render.katex_cdn.clone(),
        katex_cdn_css: crate::templates::katex_css_url(&state.cfg.render.katex_cdn),
        search_enabled: state.cfg.search.enabled,
        query: q.clone(),
        enabled,
        building,
        hits: views,
        total,
        page,
        pages_total,
        page_numbers: crate::templates::page_window(page, pages_total),
        query_encoded: url_encode_component(&q),
        hl_param: tokens
            .iter()
            .map(|t| url_encode_component(t))
            .collect::<Vec<_>>()
            .join(","),
    };
    match tpl.render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            error!(%e, "搜索结果页模板渲染失败");
            internal_error(&state)
        }
    }
}

/// 查询串 percent-encode（q/hl 值回填分页与详情链接；无路径拼接面）。
fn url_encode_component(s: &str) -> String {
    // RFC 3986 unreserved 之外全编码（与 core url::encode_segment 同口径，
    // 逗号也编码——它是 hl 多 token 分隔符）
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 0xf) as usize] as char);
        }
    }
    out
}

/// 查询串 percent-decode（仅 q 值，无路径拼接面）。
fn urldecode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let hex = |b: u8| -> Option<u8> {
        match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(b - b'a' + 10),
            b'A'..=b'F' => Some(b - b'A' + 10),
            _ => None,
        }
    };
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                let h = hex(bytes.get(i + 1).copied()?)?;
                let l = hex(bytes.get(i + 2).copied()?)?;
                out.push((h << 4) | l);
                i += 3;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}
