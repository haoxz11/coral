//! 渲染管线编排。
//!
//! 顺序：shortcode 扫描（提取/占位）→ comrak 渲染（GFM + 高亮 + 锚点）
//! → shortcode 回填（内部内容递归走同一管线）→ TOC → date 页脚。
//! 输出 HTML 片段（不含 layout）。

use crate::highlight;
use crate::scanner::SiteIndex;
use crate::shortcode::{self, Chunk, RenderCtx, UnknownShortcodeCounter};
use comrak::adapters::{HeadingAdapter, HeadingMeta};
use comrak::nodes::{Ast, AstNode, NodeValue, Sourcepos};
use comrak::options::{Options, Plugins, RenderPlugins};
use comrak::{Arena, format_html_with_plugins, parse_document};
use std::cell::RefCell;
use std::path::Path;

/// TOC 条目（页内目录，右侧或正文上方由布局决定）。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TocEntry {
    pub level: u8,
    pub title: String,
    pub anchor: String,
}

/// 渲染产物。
#[derive(Debug, Clone, Default)]
pub struct RenderedPage {
    /// HTML 片段（不含 layout）
    pub html: String,
    /// disableToc = true 或无标题时为空
    pub toc: Vec<TocEntry>,
    /// date 页脚（原始字符串直接展示；缺失为 None）
    pub date_footer: Option<String>,
}

/// 渲染错误（core 层 thiserror；调用方映射 500 + render_failed 日志）。
#[derive(Debug, thiserror::Error)]
pub enum RenderError {
    #[error("comrak 渲染失败：{0}")]
    Comrak(String),
}

/// GitHub 风格 slug：小写、空格转 `-`、去掉标点；中文保留（页内锚点用）。
pub fn github_slug(title: &str) -> String {
    let mut out = String::with_capacity(title.len());
    let mut prev_dash = false;
    for ch in title.chars() {
        if ch.is_whitespace() {
            if !prev_dash && !out.is_empty() {
                out.push('-');
                prev_dash = true;
            }
        } else if ch.is_alphanumeric() {
            for lc in ch.to_lowercase() {
                out.push(lc);
            }
            prev_dash = false;
        }
        // 标点丢弃（GitHub slug 行为）
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

thread_local! {
    /// heading adapter 与管线主体之间的 TOC 收集通道（comrak API 约束：
    /// adapter 是 &dyn 回调，只能借线程局部传数据）
    static TOC_SINK: RefCell<Vec<TocEntry>> = const { RefCell::new(Vec::new()) };
}

/// 锚点 + TOC 收集 adapter：输出 `<hN id="slug">`，同时记录条目。
#[derive(Default)]
struct AnchorHeadingAdapter;

impl HeadingAdapter for AnchorHeadingAdapter {
    fn enter(
        &self,
        output: &mut dyn std::fmt::Write,
        heading: &HeadingMeta,
        _sourcepos: Option<Sourcepos>,
    ) -> std::fmt::Result {
        let anchor = github_slug(&heading.content);
        TOC_SINK.with(|sink| {
            sink.borrow_mut().push(TocEntry {
                level: heading.level,
                title: heading.content.clone(),
                anchor: anchor.clone(),
            });
        });
        write!(output, "<h{} id=\"{anchor}\">", heading.level)
    }

    fn exit(&self, output: &mut dyn std::fmt::Write, heading: &HeadingMeta) -> std::fmt::Result {
        write!(output, "</h{}>", heading.level)
    }
}

/// comrak 公共选项：GFM 全开。
fn comrak_options() -> Options<'static> {
    let mut opts = Options::default();
    opts.extension.table = true;
    opts.extension.tasklist = true;
    opts.extension.strikethrough = true;
    opts.extension.autolink = true;
    // HTML 直通由可信内容源前提保障
    opts.render.r#unsafe = true;
    opts
}

fn render_plugins() -> Plugins<'static> {
    Plugins {
        render: RenderPlugins {
            heading_adapter: Some(&AnchorHeadingAdapter),
            ..Default::default()
        },
    }
}

/// markdown → HTML（含代码高亮与锚点）。TOC 经线程局部收集后取走。
/// `base_dir`：源文件所在目录（相对 content 根），相对链接重写的基准；
/// None 时（测试等场景）不做重写。
fn render_markdown_collect(text: &str, base_dir: Option<&Path>) -> (String, Vec<TocEntry>) {
    let opts = comrak_options();
    let arena = Arena::new();
    let root = parse_document(&arena, text, &opts);
    rewrite_code_blocks(&arena, root);
    let mut out = String::new();
    // format_html 失败仅在底层 io 错误（String 写入不会失败），降级为部分输出
    let _ = format_html_with_plugins(root, &opts, &mut out, &render_plugins());
    let out = rewrite_relative_links(&out, base_dir);
    let out = mark_external_links(&out);
    let toc = TOC_SINK.with(|sink| std::mem::take(&mut *sink.borrow_mut()));
    (out, toc)
}

/// 内层 markdown 渲染（shortcode body 递归用；TOC 不收集）。
/// base_dir 与外层同源（内层相对链接以整页源文件位置为基准）。
fn render_markdown_text(text: &str, base_dir: Option<&Path>) -> String {
    let (html, _) = render_markdown_collect(text, base_dir);
    html
}

/// 外链与站内文件链接新开页，在 HTML 输出后追加 `target="_blank" rel="noopener"`：
/// - http(s):// 绝对地址（外链）
/// - 站内 `/` 开头且最后一段含 `.` 的路径（非 md 静态文件，如 /docs/x.html——
///   在当前页打开会替换掉正在阅读的文档页）
///
/// comrak 无链接属性 adapter，后处理基于其固定输出形态
/// `<a href="URL">` / `<a href="URL" title="T">`；href 已被 comrak 转义，
/// URL 内不会出现裸 `>`，切分安全。站内页面路由（无扩展名）、页内锚点
/// （`#`）、mailto 等不动。
fn mark_external_links(html: &str) -> String {
    if !html.contains("http://") && !html.contains("https://") && !html.contains("<a href=\"/") {
        return html.to_string();
    }
    // 逐个定位以 http 或站内 / 开头的 <a href，替换其开标签
    let mut result = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(pos) = find_link_open(rest) {
        let (before, after) = rest.split_at(pos);
        result.push_str(before);
        // after 以 <a href=" 开头；找开标签的结束 '>'
        let Some(gt_rel) = after.find('>') else {
            result.push_str(after);
            return result;
        };
        let open_tag = &after[..gt_rel]; // 如 <a href="https://x.com" title="t"
        let url = &open_tag[9..]; // 去掉 <a href="
        let new_tab =
            url.starts_with("https://") || url.starts_with("http://") || internal_file_link(url);
        if new_tab && !open_tag.contains("target=") {
            result.push_str(open_tag);
            result.push_str(" target=\"_blank\" rel=\"noopener\""); // '>' 在下面补
            result.push('>');
        } else {
            result.push_str(open_tag);
            result.push('>');
        }
        rest = &after[gt_rel + 1..];
    }
    result.push_str(rest);
    result
}

/// 定位下一个待处理链接开标签：`<a href="http` 或 `<a href="/`。
fn find_link_open(html: &str) -> Option<usize> {
    let a = html.find("<a href=\"http");
    let b = html.find("<a href=\"/");
    match (a, b) {
        (Some(x), Some(y)) => Some(x.min(y)),
        (x, y) => x.or(y),
    }
}

/// 相对链接重写为站内绝对路径（a href 与 img src）。
///
/// comrak 原样输出相对链接，浏览器按**当前页 URL** 解析——permalink 页面的
/// URL 空间与文件目录脱钩，`./b.md` 解析到 permalink 路径下 404。此处基于
/// 源文件所在目录（相对 content 根）在服务端完成解析：
/// - `./b.md` → `/{dir}/b`（md 剥扩展名，对齐页面路由形态）
/// - `../guide.md` → 逐级上溯；`./img.png` → `/{dir}/img.png`（静态资产保扩展名）
/// - 指向目录 `./sub/` → `/{dir}/sub`
///
/// 不动：绝对路径、锚点、带 scheme（http/mailto）、`data:`。`..` 上溯超出
/// content 根（base_dir 耗尽）原样保留（浏览器侧自然 404，不做映射）。
/// 路径段 percent-encode 对齐路由形态（中文/空格），comrak 输出的 href 已是
/// 转义文本，此处基于 UTF-8 字符串操作，不引入切分风险。
fn rewrite_relative_links(html: &str, base_dir: Option<&Path>) -> String {
    let Some(base) = base_dir else {
        return html.to_string();
    };
    if !html.contains("<a href=\"./")
        && !html.contains("<a href=\"../")
        && !html.contains("<img src=\"./")
        && !html.contains("<img src=\"../")
    {
        return html.to_string();
    }
    let mut result = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(pos) = find_relative_attr(rest) {
        let (before, after) = rest.split_at(pos);
        result.push_str(before);
        // after 以 `<a href=".` 或 `<img src=".` 开头；attr 值到闭引号结束
        let quote_end = match after.find('"') {
            Some(q) if q > 0 => after[q + 1..].find('"').map(|e| q + 1 + e),
            _ => None,
        };
        let Some(end) = quote_end else {
            result.push_str(after);
            return result;
        };
        // 开标签属性名长度：<a href=" 或 <img src="
        let attr_len = if after.starts_with("<img") { 10 } else { 9 };
        let value = &after[attr_len..end];
        let rewritten = match resolve_relative(value, base) {
            Some(abs) => abs,
            None => value.to_string(),
        };
        result.push_str(&after[..attr_len]);
        result.push_str(&rewritten);
        result.push('"');
        rest = &after[end + 1..];
    }
    result.push_str(rest);
    result
}

/// 定位下一个相对形态的 `<a href=".` / `<img src=".`（含 `./` 与 `../`）。
fn find_relative_attr(html: &str) -> Option<usize> {
    let mut best: Option<usize> = None;
    for needle in [
        "<a href=\"./",
        "<a href=\"../",
        "<img src=\"./",
        "<img src=\"../",
    ] {
        if let Some(p) = html.find(needle) {
            best = Some(best.map_or(p, |b: usize| b.min(p)));
        }
    }
    best
}

/// 相对路径 → 站内绝对 URL。返回 None 表示不重写（越根/空段非法）。
fn resolve_relative(value: &str, base: &Path) -> Option<String> {
    // 剥锚点后缀（./x.md#sec → ./x.md + #sec）
    let (path, anchor) = match value.split_once('#') {
        Some((p, a)) => (p, Some(a)),
        None => (value, None),
    };
    if !path.starts_with("./") && !path.starts_with("../") {
        return None;
    }
    let mut segs: Vec<&str> = base
        .components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect();
    for seg in path.split('/') {
        match seg {
            "." | "" => continue,
            ".." => {
                segs.pop()?;
            }
            s => segs.push(s),
        }
    }
    // md 剥扩展名对齐页面路由（.md 直访已 404）
    let is_md = segs.last().is_some_and(|s| s.ends_with(".md"));
    if is_md {
        let last = segs.pop()?;
        segs.push(last.trim_end_matches(".md"));
    }
    let mut url = String::from("/");
    url.push_str(&segs.join("/"));
    // 指向目录的相对链接（./sub/）保留尾斜杠：浏览器对 /sub 与 /sub/ 解析
    // 下一级相对路径的行为不同，且目录路由形态以尾斜杠为准
    if path.ends_with('/') && !url.ends_with('/') {
        url.push('/');
    }
    if let Some(a) = anchor {
        url.push('#');
        url.push_str(a);
    }
    Some(url)
}

/// 站内文件链接：`/` 开头，最后一段（文件名）含 `.`——即带扩展名的静态资产；
/// 页面路由无扩展名（/guide/intro）返回 false。`#` 锚点后缀先剥掉。
fn internal_file_link(url: &str) -> bool {
    let path = url.split('#').next().unwrap_or(url);
    if !path.starts_with('/') {
        return false;
    }
    path.rsplit('/')
        .next()
        .is_some_and(|last| last.contains('.'))
}

/// HTML 转义（未知语言代码块纯转义路径；math 模块复用）。
pub(crate) fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// 围栏 info 首词（语言名）；`math {align="center"}` → `math`。
fn lang_first_word(info_trimmed: &str) -> Option<&str> {
    info_trimmed.split_whitespace().next()
}

/// 遍历 AST，把 fenced 代码块替换为高亮后的 HTML 块节点（服务端高亮）。
///
/// 用 HtmlBlock 而非行内节点，避免块级结构错位。
fn rewrite_code_blocks<'a>(arena: &'a Arena<'a>, root: &'a AstNode<'a>) {
    let children: Vec<&'a AstNode<'a>> = root.children().collect();
    for node in children {
        let replace_html = match node.data.borrow().value {
            NodeValue::CodeBlock(ref cb) => {
                let lang = cb.info.to_string();
                let code = cb.literal.to_string();
                let lang_trim = lang.trim();
                // mermaid 图表（M2-s3）：服务端只转语义占位，前端 CDN JS 渲染；
                // 原文转义（安全红线：图表源码可含任意文本）
                if lang_trim == "mermaid" {
                    format!(
                        "<pre class=\"mermaid\">{}</pre>",
                        escape_html(code.trim_end_matches('\n'))
                    )
                } else if lang_first_word(lang_trim) == Some("math") {
                    // ```math 围栏（GitHub/Hugo 迁移文档形态）：走 katex 块级占位，
                    // 前端 CDN 渲染；info 附加属性（如 {align="center"}）忽略
                    let mut formula = code.trim();
                    if formula.len() >= 4 && formula.starts_with("$$") && formula.ends_with("$$") {
                        formula = formula[2..formula.len() - 2].trim();
                    }
                    crate::math::katex_block_html(formula)
                } else {
                    match highlight::highlight_code(lang_trim, &code) {
                        Some(highlighted) => {
                            // highlight_code 只输出内层 span（无 pre/code），必须补包裹，
                            // 否则代码内容裸躺在正文中丢失代码块样式
                            format!(
                                "<pre><code class=\"language-{}\">{}</code></pre>",
                                escape_html(lang_trim),
                                highlighted
                            )
                        }
                        None if lang_trim.is_empty() => {
                            // 无语言信息：comrak 默认 <pre><code> 即可，保留原节点
                            rewrite_code_blocks(arena, node);
                            continue;
                        }
                        None => format!(
                            "<pre><code class=\"language-{}\">{}</code></pre>",
                            escape_html(lang_trim),
                            escape_html(&code)
                        ),
                    }
                }
            }
            _ => {
                rewrite_code_blocks(arena, node);
                continue;
            }
        };
        let html_block = comrak::nodes::NodeHtmlBlock {
            block_type: 6,
            literal: format!("{replace_html}\n"),
        };
        let new = arena.alloc(AstNode::from(Ast::new(
            NodeValue::HtmlBlock(html_block),
            (1, 1).into(),
        )));
        node.insert_before(new);
        node.detach();
    }
}

/// 占位符：私有区控制字符包裹索引，comrak 原样透传，回填时字符串替换。
const PLACEHOLDER: char = '\u{E000}';

fn make_placeholder(i: usize) -> String {
    format!("{PLACEHOLDER}{i}{PLACEHOLDER}")
}

/// 渲染入口。
///
/// `body` 为去 front matter 后的正文；`rel_path` 用于 children shortcode
/// 与未知 shortcode 日志定位。
#[allow(clippy::too_many_arguments)]
pub fn render_page(
    body: &str,
    rel_path: &Path,
    index: &SiteIndex,
    draft_enabled: bool,
    disable_toc: bool,
    date: Option<&str>,
    render_cfg: &crate::config::RenderConfig,
    unknown_counter: &mut UnknownShortcodeCounter,
) -> Result<RenderedPage, RenderError> {
    // 1) shortcode 解析 → 占位符替换（独立成段，避免被包进段落文本）
    let chunks = shortcode::parse(body);
    let mut md_input = String::with_capacity(body.len());
    let mut tokens = Vec::new();
    for chunk in &chunks {
        match chunk {
            Chunk::Text(t) => md_input.push_str(t),
            Chunk::Shortcode(t) => {
                md_input.push_str(&format!("\n\n{}\n\n", make_placeholder(tokens.len())));
                tokens.push(t.clone());
            }
        }
    }

    // 1.5) 数学定界提取（M2-s3：shortcode 后、comrak 前——块级 $$ 默认，
    // 行内 $ 需 inline_math；占位符 E001 与 shortcode E000 命名空间隔离）
    let math = crate::math::extract_math(&md_input, render_cfg.inline_math);

    // 2) comrak 渲染（锚点 + TOC 收集 + 代码高亮）；
    // 相对链接重写以源文件目录为基准——与页面最终 URL（permalink）解耦
    let base_dir = rel_path.parent();
    let (mut html, toc) = render_markdown_collect(&math.text, base_dir);

    // 3) 回填：shortcode 内部内容递归走 markdown 管线（% 语义）
    let render_inner = |text: &str| render_markdown_text(text, base_dir);
    let ctx = RenderCtx {
        index,
        rel_path,
        draft_enabled,
        render_markdown: &render_inner,
    };
    for (i, token) in tokens.iter().enumerate() {
        let rendered = shortcode::render_token(token, &ctx, unknown_counter);
        html = html.replace(&make_placeholder(i), &rendered);
    }

    // 3.5) 数学占位回填（katex 语义占位，原文转义）
    html = crate::math::replace_math_placeholders(&html, &math);

    Ok(RenderedPage {
        html,
        toc: if disable_toc { Vec::new() } else { toc },
        date_footer: date.map(str::to_string),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ContentConfig;
    use crate::scanner::{SiteIndex, scan};

    fn test_index() -> SiteIndex {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/site");
        let cfg = ContentConfig {
            root,
            exclude: vec!["drafts".into(), "excluded".into()],
            draft: false,
        };
        SiteIndex::build(scan(&cfg).unwrap(), false)
    }

    fn render(body: &str, rel: &str) -> RenderedPage {
        let mut counter = UnknownShortcodeCounter::default();
        render_page(
            body,
            Path::new(rel),
            &test_index(),
            false,
            false,
            None,
            &crate::config::RenderConfig::default(),
            &mut counter,
        )
        .unwrap()
    }

    #[test]
    fn test_mermaid_code_block_placeholder() {
        let page = render("```mermaid\ngraph TD; A-->B;\n```\n", "x.md");
        assert!(
            page.html.contains("<pre class=\"mermaid\">"),
            "mermaid 占位：{}",
            page.html
        );
        assert!(page.html.contains("graph TD;"), "原文保留：{}", page.html);
        // 安全红线：图表源码含 HTML 标签必须转义
        let evil = render("```mermaid\ngraph <script>alert(1)</script>\n```\n", "x.md");
        assert!(evil.html.contains("&lt;script&gt;"), "{}", evil.html);
        assert!(!evil.html.contains("<script>alert"), "{}", evil.html);
    }

    #[test]
    fn test_math_fence_placeholder() {
        // GitHub/Hugo 迁移形态：$$ 括裹剥除后进 data-formula
        let page = render("```math\n$$E = mc^2$$\n```\n", "x.md");
        assert!(
            page.html
                .contains("katex-block\" data-formula=\"E = mc^2\""),
            "math 围栏占位：{}",
            page.html
        );
        // 裸公式（无 $$ 括裹）同样支持
        let bare = render("```math\n\\sigma = \\sqrt{N}\n```\n", "x.md");
        assert!(
            bare.html.contains("data-formula=\"\\sigma = \\sqrt{N}\""),
            "裸公式：{}",
            bare.html
        );
    }

    #[test]
    fn test_math_fence_info_attributes_ignored() {
        // info 首词识别，附加属性不参与
        let page = render("```math {align=\"center\"}\n$$x^2$$\n```\n", "x.md");
        assert!(
            page.html.contains("data-formula=\"x^2\""),
            "带属性 info：{}",
            page.html
        );
        assert!(!page.html.contains("language-math"), "{}", page.html);
    }

    #[test]
    fn test_math_fence_escapes_formula() {
        // 安全红线：公式原文含 HTML/引号，属性侧必须转义（< → &lt;、" → &quot;）
        let evil = render(
            "```math\n$$a < b \" onmouseover=\"alert(1)$$\n```\n",
            "x.md",
        );
        assert!(
            evil.html
                .contains("data-formula=\"a &lt; b &quot; onmouseover=&quot;alert(1)\""),
            "属性转义：{}",
            evil.html
        );
        assert!(!evil.html.contains("<script"), "{}", evil.html);
    }

    #[test]
    fn test_block_math_in_pipeline() {
        let page = render(r"前文 $$E = mc^2$$ 后文", "x.md");
        assert!(page.html.contains("katex-block"), "{}", page.html);
        assert!(
            page.html.contains("data-formula=\"E = mc^2\""),
            "{}",
            page.html
        );
        // 公式内 markdown 不被二次渲染（_ 转义保留给 KaTeX）
        let page = render(r"$$x_i^2$$", "x.md");
        assert!(
            page.html.contains("data-formula=\"x_i^2\"") && !page.html.contains("<em>"),
            "公式原文不被 comrak 渲染：{}",
            page.html
        );
    }

    #[test]
    fn test_inline_math_gated_by_config() {
        // 默认关闭：$..$ 原样
        let mut counter = UnknownShortcodeCounter::default();
        let cfg_off = crate::config::RenderConfig::default();
        let page = render_page(
            "公式 $x^2$ 结束",
            Path::new("x.md"),
            &test_index(),
            false,
            false,
            None,
            &cfg_off,
            &mut counter,
        )
        .unwrap();
        assert!(!page.html.contains("katex-inline"), "{}", page.html);
        assert!(page.html.contains("$x^2$"), "{}", page.html);
        // 开启：识别占位
        let cfg_on = crate::config::RenderConfig {
            inline_math: true,
            ..Default::default()
        };
        let page = render_page(
            "公式 $x^2$ 结束",
            Path::new("x.md"),
            &test_index(),
            false,
            false,
            None,
            &cfg_on,
            &mut counter,
        )
        .unwrap();
        assert!(page.html.contains("katex-inline"), "{}", page.html);
        assert!(page.html.contains("data-formula=\"x^2\""), "{}", page.html);
    }

    #[test]
    fn test_gfm_table_tasklist_strikethrough() {
        let page = render(
            "| a | b |\n|---|---|\n| 1 | 2 |\n\n- [x] 完成\n\n~~删除~~\n\nhttps://example.com\n",
            "x.md",
        );
        assert!(page.html.contains("<table>"), "{}", page.html);
        assert!(
            page.html.contains("checkbox") || page.html.contains("task-list"),
            "{}",
            page.html
        );
        assert!(page.html.contains("<del>"), "{}", page.html);
        assert!(
            page.html.contains("href=\"https://example.com\""),
            "{}",
            page.html
        );
    }

    #[test]
    fn test_notice_with_title_details() {
        // title 形态为标题行（内联 SVG 图标+粗体），不用 details 折叠
        let page = render(
            "{{% notice style=\"warning\" title=\"点击查看\" %}}**加粗**内容{{% /notice %}}",
            "x.md",
        );
        assert!(
            page.html
                .contains("<div class=\"notice notice-warning\"><p class=\"notice-title\"><svg class=\"notice-icon\""),
            "{}",
            page.html
        );
        // 内联 SVG 不依赖 iconify 运行时（内网/断网图标仍显示）
        assert!(!page.html.contains("<iconify-icon"), "{}", page.html);
        assert!(
            page.html.contains("<strong>加粗</strong>"),
            "内层 markdown 渲染：{}",
            page.html
        );
    }

    #[test]
    fn test_notice_without_title_div() {
        let page = render("{{% notice %}}正文{{% /notice %}}", "x.md");
        assert!(
            page.html.contains("<div class=\"notice notice-info\">"),
            "{}",
            page.html
        );
    }

    #[test]
    fn test_tabs_structure() {
        let input = "{{< tabs >}}{{% tab title=\"说明\" %}}A{{% /tab %}}{{% tab title=\"示例\" %}}B{{% /tab %}}{{< /tabs >}}";
        let page = render(input, "x.md");
        assert!(page.html.contains("<div class=\"tabs\">"), "{}", page.html);
        assert!(page.html.contains("data-tab=\"0\""), "{}", page.html);
        assert!(page.html.contains("data-panel=\"1\""), "{}", page.html);
        assert!(page.html.contains("tab-panel active"), "{}", page.html);
    }

    #[test]
    fn test_children_renders_subtree_links() {
        let page = render("列表：{{% children %}}", "guide/_index.md");
        assert!(
            page.html.contains("<ul class=\"children\">"),
            "{}",
            page.html
        );
        assert!(page.html.contains("href=\"/guide/intro\""), "{}", page.html);
    }

    #[test]
    fn test_unknown_shortcode_passthrough_with_warn() {
        let mut counter = UnknownShortcodeCounter::default();
        let index = test_index();
        let page = render_page(
            "前 {{% mermaid %}}graph A{{% /mermaid %}} 后",
            Path::new("some/page.md"),
            &index,
            false,
            false,
            None,
            &crate::config::RenderConfig::default(),
            &mut counter,
        )
        .unwrap();
        assert!(
            page.html.contains("{{% mermaid"),
            "未知 shortcode 原样输出：{}",
            page.html
        );
        assert!(page.html.contains("{{% /mermaid %}}"), "{}", page.html);
        assert!(page.html.contains("graph A"), "{}", page.html);
        assert_eq!(counter.total(), 1);
        let ((name, file), _) = counter.iter().next().unwrap();
        assert_eq!(name, "mermaid");
        assert_eq!(file, "some/page.md");
    }

    #[test]
    fn test_toc_generated_with_anchors() {
        let page = render("# 一级\n\n## 二级 A\n\n### 三级\n\n## 二级 B\n", "x.md");
        assert_eq!(page.toc.len(), 4);
        assert_eq!(
            page.toc[0],
            TocEntry {
                level: 1,
                title: "一级".to_string(),
                anchor: "一级".to_string()
            }
        );
        assert_eq!(page.toc[1].level, 2);
        assert_eq!(page.toc[3].title, "二级 B");
        // 锚点与正文 id 一致
        assert!(page.html.contains("id=\"一级\""), "{}", page.html);
    }

    #[test]
    fn test_disable_toc_skips_toc() {
        let index = test_index();
        let mut counter = UnknownShortcodeCounter::default();
        let page = render_page(
            "# 标题\n正文",
            Path::new("x.md"),
            &index,
            false,
            true, // disableToc
            None,
            &crate::config::RenderConfig::default(),
            &mut counter,
        )
        .unwrap();
        assert!(page.toc.is_empty());
        // 正文标题本身仍渲染
        assert!(page.html.contains("<h1"), "{}", page.html);
    }

    #[test]
    fn test_date_footer_passthrough() {
        let index = test_index();
        let mut counter = UnknownShortcodeCounter::default();
        let page = render_page(
            "正文",
            Path::new("x.md"),
            &index,
            false,
            false,
            Some("2024-03-05"),
            &crate::config::RenderConfig::default(),
            &mut counter,
        )
        .unwrap();
        assert_eq!(page.date_footer.as_deref(), Some("2024-03-05"));
    }

    #[test]
    fn test_code_highlight_rust() {
        let page = render("```rust\nfn main() {}\n```\n", "x.md");
        assert!(
            page.html.contains("source rust") || page.html.contains("class=\""),
            "{}",
            page.html
        );
    }

    #[test]
    fn test_notice_title_escaped_and_style_whitelisted() {
        // title 转义防注入；未识别 style 归一 info
        let page = render(
            "{{% notice style=\"<script>x</script>\" title=\"<b>注</b>入\" %}}内容{{% /notice %}}",
            "n.md",
        );
        assert!(
            !page.html.contains("<script>"),
            "style 不应透传：{}",
            page.html
        );
        assert!(
            page.html.contains("notice-info"),
            "未识别 style 归一 info：{}",
            page.html
        );
        assert!(
            page.html.contains("&lt;b&gt;注&lt;/b&gt;入"),
            "title 应转义：{}",
            page.html
        );
    }

    #[test]
    fn test_tabs_leading_text_first_tab_active() {
        // 回归：tabs body 前有文本（{{< tabs >}} 后换行），
        // 修复前 enumerate 索引使首个 tab 的 i 非 0 → 无任何 active（面板全隐藏）
        let page = render(
            "{{< tabs >}}\n{{% tab title=\"入参\" %}}A{{% /tab %}}\n{{% tab title=\"出参\" %}}B{{% /tab %}}\n{{< /tabs >}}",
            "tabs.md",
        );
        assert!(
            page.html.contains("tab-header active"),
            "首 tab 标题应 active：{}",
            page.html
        );
        assert!(
            page.html.contains("tab-panel active"),
            "首 tab 面板应 active：{}",
            page.html
        );
        assert_eq!(page.html.matches("tab-header active").count(), 1);
    }

    #[test]
    fn test_relative_links_rewritten_from_source_dir() {
        // 相对链接以源文件目录为基准重写（permalink 页面不再按页面 URL 解析）
        let html = r##"<p><a href="./b.md">md</a> <a href="./img.png">图链</a> <img src="./pic.png" alt="p" /> <a href="../guide.md">上级</a> <a href="./sub/">目录</a> <a href="./b.md#sec">带锚点</a></p>"##;
        let out = rewrite_relative_links(html, Some(Path::new("tools")));
        assert!(out.contains(r##"<a href="/tools/b">md</a>"##), "{out}");
        assert!(
            out.contains(r##"<a href="/tools/img.png">图链</a>"##),
            "{out}"
        );
        assert!(
            out.contains(r##"<img src="/tools/pic.png" alt="p" />"##),
            "{out}"
        );
        assert!(out.contains(r##"<a href="/guide">上级</a>"##), "{out}");
        assert!(out.contains(r##"<a href="/tools/sub/">目录</a>"##), "{out}");
        assert!(
            out.contains(r##"<a href="/tools/b#sec">带锚点</a>"##),
            "{out}"
        );
    }

    #[test]
    fn test_relative_links_non_relative_untouched() {
        // 绝对路径/锚点/带 scheme 不动
        let html = r##"<p><a href="/guide">绝对</a> <a href="#s">锚</a> <a href="https://x.com">外链</a> <a href="mailto:a@b.c">邮件</a> <img src="data:image/png;base64,xx" alt="d" /></p>"##;
        let out = rewrite_relative_links(html, Some(Path::new("tools")));
        assert_eq!(out, html);
    }

    #[test]
    fn test_relative_links_escape_root_kept() {
        // ../ 上溯超出 content 根：原样保留（不做映射，浏览器侧自然 404）
        let html = r##"<p><a href="../../../etc/passwd">逃逸</a></p>"##;
        let out = rewrite_relative_links(html, Some(Path::new("a/b")));
        assert_eq!(out, html);
    }

    #[test]
    fn test_relative_links_in_full_render_with_permalink() {
        // 端到端：permalink 页面的相对链接仍以源文件位置解析
        let page = render(
            "---\ntitle: t\npermalink: /fixed/\n---\n\n[同目录](./b.md) [图](./i.png)",
            "tools/zz.md",
        );
        assert!(page.html.contains("href=\"/tools/b\""), "{}", page.html);
        assert!(page.html.contains("href=\"/tools/i.png\""), "{}", page.html);
        // 带扩展名的静态文件链接同时获得新开页标记
        assert!(
            page.html
                .contains("href=\"/tools/i.png\" target=\"_blank\""),
            "{}",
            page.html
        );
    }

    #[test]
    fn test_external_links_get_target_blank() {
        let out = mark_external_links(
            r##"<p><a href="https://example.com/x">外链</a> <a href="/guide">内链</a> <a href="#sec">锚点</a> <a href="https://e.com/y" title="t">带title外链</a> <a href="mailto:a@b.c">邮件</a></p>"##,
        );
        assert!(
            out.contains(
                r##"<a href="https://example.com/x" target="_blank" rel="noopener">外链</a>"##
            ),
            "{out}"
        );
        assert!(out.contains(r##"<a href="https://e.com/y" title="t" target="_blank" rel="noopener">带title外链</a>"##), "{out}");
        assert!(out.contains(r##"<a href="/guide">内链</a>"##), "{out}");
        assert!(out.contains(r##"<a href="#sec">锚点</a>"##), "{out}");
        assert!(
            out.contains(r##"<a href="mailto:a@b.c">邮件</a>"##),
            "{out}"
        );
    }

    #[test]
    fn test_internal_file_links_get_target_blank() {
        // 站内静态文件（带扩展名）新开页：当前页打开会替换掉正在阅读的文档页
        let out = mark_external_links(
            r##"<p><a href="/docs/x.html">html</a> <a href="/a/b.sql" title="t">sql</a> <a href="/img/p.png">图</a> <a href="/docs/x.html#sec">带锚点文件</a></p>"##,
        );
        assert!(
            out.contains(r##"<a href="/docs/x.html" target="_blank" rel="noopener">html</a>"##),
            "{out}"
        );
        assert!(
            out.contains(
                r##"<a href="/a/b.sql" title="t" target="_blank" rel="noopener">sql</a>"##
            ),
            "{out}"
        );
        assert!(
            out.contains(r##"<a href="/img/p.png" target="_blank" rel="noopener">图</a>"##),
            "{out}"
        );
        assert!(
            out.contains(
                r##"<a href="/docs/x.html#sec" target="_blank" rel="noopener">带锚点文件</a>"##
            ),
            "{out}"
        );
    }

    #[test]
    fn test_internal_page_links_stay_same_tab() {
        // 站内页面路由（无扩展名）与纯锚点不动
        let out = mark_external_links(
            r##"<p><a href="/guide/intro">页面</a> <a href="/guide/intro#sec">页面锚点</a> <a href="/">首页</a></p>"##,
        );
        assert!(
            out.contains(r##"<a href="/guide/intro">页面</a>"##),
            "{out}"
        );
        assert!(
            out.contains(r##"<a href="/guide/intro#sec">页面锚点</a>"##),
            "{out}"
        );
        assert!(out.contains(r##"<a href="/">首页</a>"##), "{out}");
    }

    #[test]
    fn test_code_unknown_lang_escaped() {
        let page = render("```someslang\na < b\n```\n", "x.md");
        assert!(page.html.contains("&lt;"), "未知语言纯转义：{}", page.html);
        assert!(page.html.contains("language-someslang"), "{}", page.html);
    }

    #[test]
    fn test_code_no_lang_default_block() {
        let page = render("```\nplain\n```\n", "x.md");
        assert!(page.html.contains("<pre><code"), "{}", page.html);
    }

    #[test]
    fn test_github_slug() {
        assert_eq!(github_slug("Hello World"), "hello-world");
        assert_eq!(github_slug("中文 标题"), "中文-标题");
        assert_eq!(github_slug("What's New?"), "whats-new");
    }

    #[test]
    fn test_shortcode_in_markdown_paragraph_flow() {
        // shortcode 前后有文本：占位符独立成段，前后文本各自成段
        let page = render("前面文字 {{% notice %}}中{{% /notice %}} 后面文字", "x.md");
        assert!(page.html.contains("前面文字"), "{}", page.html);
        assert!(
            page.html.contains("<div class=\"notice notice-info\">"),
            "{}",
            page.html
        );
        assert!(page.html.contains("后面文字"), "{}", page.html);
    }
}
