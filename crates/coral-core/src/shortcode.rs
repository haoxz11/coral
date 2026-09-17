//! Shortcode 解析与渲染。
//!
//! 解析与渲染分离：状态机先把 body 切成 文本段/shortcode token 交替序列，
//! 渲染期消费 token 流。三族：notice / tabs·tab / children；
//! 未知 shortcode 原样输出 + WARN。

use crate::scanner::SiteIndex;
use crate::tree::build_subtree;
use std::fmt::Write as _;
use std::path::Path;
use tracing::warn;

/// shortcode 定界族：`{{% ... %}}`（内层按 markdown 渲染）与 `{{< ... >}}`（原样）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delim {
    Percent,
    Angle,
}

/// 解析出的单个 shortcode token。
#[derive(Debug, Clone, PartialEq)]
pub struct ScToken {
    pub delim: Delim,
    pub name: String,
    /// 原始参数文本（如 `style="info" title="点击查看"`）
    pub params: String,
    /// 闭合标签之间的原始内容（自闭合/无 body 时为 None）
    pub body: Option<String>,
}

/// body 切分结果：文本段与 shortcode 交替。
#[derive(Debug, Clone, PartialEq)]
pub enum Chunk {
    Text(String),
    Shortcode(ScToken),
}

/// 解析参数中的 `key="value"` 对；不支持裸参数。
pub fn parse_params(params: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let bytes = params.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // 跳过空白
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let key_start = i;
        while i < bytes.len() && bytes[i] != b'=' && !bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if key_start == i {
            break;
        }
        let key = params[key_start..i].to_string();
        if i < bytes.len() && bytes[i] == b'=' && i + 1 < bytes.len() && bytes[i + 1] == b'"' {
            i += 2;
            let val_start = i;
            while i < bytes.len() && bytes[i] != b'"' {
                i += 1;
            }
            let value = params[val_start..i.min(bytes.len())].to_string();
            if i < bytes.len() {
                i += 1; // 跳过闭引号
            }
            out.push((key, value));
        } else {
            // 无值参数：记为空值
            out.push((key, String::new()));
        }
    }
    out
}

/// 一次完整扫描出的 shortcode span（`{{% ... %}}` 或 `{{< ... >}}`）。
#[derive(Debug)]
struct Span {
    /// span 结束位置（闭定界之后）
    end: usize,
    delim: Delim,
    /// 开闭定界之间的原文（未 trim）
    inner: String,
}

/// 扫描全部完整 span；`{{%` 无 `%}}` 配对等内容按普通文本跳过。
/// fenced 代码块与行内代码内的定界符不识别（展示源码场景），原样透传。
fn scan_spans(body: &str) -> Vec<(usize, Span)> {
    let bytes = body.as_bytes();
    let mut spans = Vec::new();
    let mut i = 0;
    // 围栏状态：与 math.rs extract_math 同款判定（行首 ```/~~~ run ≥3 到行尾），
    // 开闭 run 长度必须一致才视为闭合
    let mut in_fence = false;
    let mut fence_marker_len = 0usize;
    while i < body.len() {
        let b = bytes[i];
        if (b == b'`' || b == b'~') && crate::math::is_line_start_at(bytes, i) {
            let run = crate::math::count_run(bytes, i, b);
            if run >= 3 {
                let after = bytes.get(i + run);
                if after.is_none() || after.is_some_and(|c| *c == b'\n' || *c == b'\r') {
                    if in_fence && run == fence_marker_len {
                        in_fence = false;
                    } else if !in_fence {
                        in_fence = true;
                        fence_marker_len = run;
                    }
                    i += run;
                    continue;
                }
            }
        }
        if b == b'`' && !in_fence {
            let len = crate::math::inline_code_span_len(bytes, i);
            if len > 0 {
                i += len;
                continue;
            }
        }
        if in_fence || b != b'{' || bytes.get(i + 1) != Some(&b'{') {
            i += crate::math::utf8_len(b);
            continue;
        }
        let (delim, close) = match bytes.get(i + 2) {
            Some(b'%') => (Delim::Percent, "%}}"),
            Some(b'<') => (Delim::Angle, ">}}"),
            _ => {
                i += 2;
                continue;
            }
        };
        let Some(rel) = body[i + 3..].find(close) else {
            // 开定界无闭定界：跳过 "{{" 继续扫
            i += 2;
            continue;
        };
        let inner = body[i + 3..i + 3 + rel].to_string();
        spans.push((
            i,
            Span {
                end: i + 3 + rel + close.len(),
                delim,
                inner,
            },
        ));
        i = i + 3 + rel + close.len();
    }
    spans
}

/// span 内文拆为 (name, params)；闭标签（`/name`）返回 (name, true)。
fn split_inner(inner: &str) -> (String, String, bool) {
    let trimmed = inner.trim();
    let (is_close, rest) = match trimmed.strip_prefix('/') {
        Some(r) => (true, r),
        None => (false, trimmed),
    };
    let (name, params) = match rest.find(char::is_whitespace) {
        Some(p) => (rest[..p].to_string(), rest[p + 1..].trim().to_string()),
        None => (rest.to_string(), String::new()),
    };
    (name, params, is_close)
}

/// 这些是包裹型 shortcode：必须有配对闭标签，否则整段降级为文本
/// （内容作者失误不该让页面 500）。
fn is_container(name: &str) -> bool {
    matches!(name, "notice" | "tab" | "tabs")
}

/// 状态机解析 body 为 chunk 序列。
///
/// 两阶段：先扫出全部 span，再按名字深度配对（嵌套同名 Hugo 语义）。
/// 未配对的包裹型标签降级为文本；非包裹型（children、未知）按自闭合处理，
/// 若文档后部存在配对闭标签则吞入 body。
pub fn parse(body: &str) -> Vec<Chunk> {
    let spans = scan_spans(body);
    let mut chunks: Vec<Chunk> = Vec::new();
    let mut text_start = 0;
    let mut i = 0;
    while i < spans.len() {
        let (start, span) = &spans[i];
        let (name, params, is_close) = split_inner(&span.inner);
        if is_close || name.is_empty() {
            // 顶层游离闭标签：当普通文本跳过
            i += 1;
            continue;
        }
        // 向后找配对闭标签（同名深度配对）
        let mut depth = 1;
        let mut close_idx = None;
        let mut j = i + 1;
        while j < spans.len() {
            let (_, ref s2) = spans[j];
            let (n2, _, c2) = split_inner(&s2.inner);
            if c2 {
                if n2 == name {
                    depth -= 1;
                    if depth == 0 {
                        close_idx = Some(j);
                        break;
                    }
                }
            } else if n2 == name {
                depth += 1;
            }
            j += 1;
        }
        match close_idx {
            Some(cj) => {
                let body_end = spans[cj].0;
                if *start > text_start {
                    chunks.push(Chunk::Text(body[text_start..*start].to_string()));
                }
                chunks.push(Chunk::Shortcode(ScToken {
                    delim: span.delim,
                    name,
                    params,
                    body: Some(body[span.end..body_end].to_string()),
                }));
                text_start = spans[cj].1.end;
                i = cj + 1;
            }
            None => {
                if is_container(&name) {
                    // 包裹型未闭合：降级为文本，跳过该 span
                    i += 1;
                } else {
                    // 非包裹型：自闭合（children / 未知自闭合标签）
                    if *start > text_start {
                        chunks.push(Chunk::Text(body[text_start..*start].to_string()));
                    }
                    chunks.push(Chunk::Shortcode(ScToken {
                        delim: span.delim,
                        name,
                        params,
                        body: None,
                    }));
                    text_start = span.end;
                    i += 1;
                }
            }
        }
    }
    if text_start < body.len() {
        chunks.push(Chunk::Text(body[text_start..].to_string()));
    }
    chunks
}

/// 渲染上下文：children shortcode 需要当前索引与页面位置。
pub struct RenderCtx<'a> {
    pub index: &'a SiteIndex,
    pub rel_path: &'a Path,
    pub draft_enabled: bool,
    /// 内层 markdown 渲染回调（render.rs 注入，避免模块循环依赖）
    pub render_markdown: &'a dyn Fn(&str) -> String,
}

/// 未知 shortcode 计数（滚动计数器载体，由调用方持有并汇总）。
#[derive(Debug, Default)]
pub struct UnknownShortcodeCounter {
    /// (name, file) -> 次数
    counts: std::collections::BTreeMap<(String, String), u64>,
}

impl UnknownShortcodeCounter {
    pub fn record(&mut self, name: &str, rel_path: &Path) {
        *self
            .counts
            .entry((name.to_string(), rel_path.display().to_string()))
            .or_insert(0) += 1;
    }

    /// 累计记录迭代（调用方按批输出汇总 + 停机最终汇总）。
    pub fn iter(&self) -> impl Iterator<Item = (&(String, String), &u64)> {
        self.counts.iter()
    }

    pub fn total(&self) -> u64 {
        self.counts.values().sum()
    }
}

/// 渲染单个 token 为 HTML；未知 shortcode 原样输出 + WARN。
pub fn render_token(
    token: &ScToken,
    ctx: &RenderCtx<'_>,
    unknown: &mut UnknownShortcodeCounter,
) -> String {
    let md = ctx.render_markdown;
    match token.name.as_str() {
        "notice" => {
            let params = parse_params(&token.params);
            // style 白名单归一：未识别回退 info，
            // 且 class 值不透传原始输入（防任意 class 注入）
            const KNOWN_STYLES: [&str; 8] = [
                "tip",
                "info",
                "note",
                "caution",
                "warning",
                "important",
                "danger",
                "error",
            ];
            let style = params
                .iter()
                .find(|(k, _)| k == "style")
                .map(|(_, v)| v.clone())
                .unwrap_or_else(|| "info".to_string());
            let style = if KNOWN_STYLES.contains(&style.as_str()) {
                style
            } else {
                "info".to_string()
            };
            // 空串 title（title=""）视为未提供；HTML 转义防注入
            let title = params
                .iter()
                .find(|(k, _)| k == "title")
                .map(|(_, v)| v.as_str())
                .filter(|v| !v.is_empty())
                .map(|v| {
                    v.replace('&', "&amp;")
                        .replace('<', "&lt;")
                        .replace('>', "&gt;")
                });
            let body_html = token.body.as_deref().map(md).unwrap_or_default();
            // theme-hope hint 形态（始终展开，不用 details 折叠）：
            // 内容始终展开；title 为标题行（内联 SVG 图标 + 粗体同色标题，服务端渲染）。
            // 图标为构建期内联 SVG（path 取自 fa6-solid），不依赖 iconify 运行时
            // CDN——内网/受限网络下组件拉取图标数据失败会导致 0×0 空白
            const ICONS: [(&str, &str, &str); 5] = [
                // (style 图标键, viewBox, path d)
                (
                    "lightbulb",
                    "0 0 384 512",
                    "M272 384c9.6-31.9 29.5-59.1 49.2-86.2c5.2-7.1 10.4-14.2 15.4-21.4c19.8-28.5 31.4-63 31.4-100.3C368 78.8 289.2 0 192 0S16 78.8 16 176c0 37.3 11.6 71.9 31.4 100.3c5 7.2 10.2 14.3 15.4 21.4c19.8 27.1 39.7 54.4 49.2 86.2h160zm-80 128c44.2 0 80-35.8 80-80v-16H112v16c0 44.2 35.8 80 80 80m-80-336c0 8.8-7.2 16-16 16s-16-7.2-16-16c0-61.9 50.1-112 112-112c8.8 0 16 7.2 16 16s-7.2 16-16 16c-44.2 0-80 35.8-80 80",
                ),
                (
                    "circle-info",
                    "0 0 512 512",
                    "M256 512a256 256 0 1 0 0-512a256 256 0 1 0 0 512m-40-176h24v-64h-24c-13.3 0-24-10.7-24-24s10.7-24 24-24h48c13.3 0 24 10.7 24 24v88h8c13.3 0 24 10.7 24 24s-10.7 24-24 24h-80c-13.3 0-24-10.7-24-24s10.7-24 24-24m40-208a32 32 0 1 1 0 64a32 32 0 1 1 0-64",
                ),
                (
                    "circle-exclamation",
                    "0 0 512 512",
                    "M256 512a256 256 0 1 0 0-512a256 256 0 1 0 0 512m0-384c13.3 0 24 10.7 24 24v112c0 13.3-10.7 24-24 24s-24-10.7-24-24V152c0-13.3 10.7-24 24-24m-32 224a32 32 0 1 1 64 0a32 32 0 1 1-64 0",
                ),
                (
                    "triangle-exclamation",
                    "0 0 512 512",
                    "M256 32c14.2 0 27.3 7.5 34.5 19.8l216 368c7.3 12.4 7.3 27.7.2 40.1S486.3 480 472 480H40c-14.3 0-27.6-7.7-34.7-20.1s-7-27.8.2-40.1l216-368C228.7 39.5 241.8 32 256 32m0 128c-13.3 0-24 10.7-24 24v112c0 13.3 10.7 24 24 24s24-10.7 24-24V184c0-13.3-10.7-24-24-24m32 224a32 32 0 1 0-64 0a32 32 0 1 0 64 0",
                ),
                (
                    "circle-xmark",
                    "0 0 512 512",
                    "M256 512a256 256 0 1 0 0-512a256 256 0 1 0 0 512m-81-337c9.4-9.4 24.6-9.4 33.9 0l47 47l47-47c9.4-9.4 24.6-9.4 33.9 0s9.4 24.6 0 33.9l-47 47l47 47c9.4 9.4 9.4 24.6 0 33.9s-24.6 9.4-33.9 0l-47-47l-47 47c-9.4 9.4-24.6 9.4-33.9 0s-9.4-24.6 0-33.9l47-47l-47-47c-9.4-9.4-9.4-24.6 0-33.9",
                ),
            ];
            // style → 图标键（info/note 同款圆点 i；caution/important/danger 同款圆点叹号）
            let icon_key = match style.as_str() {
                "tip" => "lightbulb",
                "caution" | "important" | "danger" => "circle-exclamation",
                "warning" => "triangle-exclamation",
                "error" => "circle-xmark",
                _ => "circle-info", // info / note / 未识别回退
            };
            let (_, vb, d) = ICONS
                .iter()
                .find(|(k, _, _)| *k == icon_key)
                .expect("icon_key 由 match 产生，必在 ICONS 表内");
            let icon_svg = format!(
                "<svg class=\"notice-icon\" viewBox=\"{vb}\" aria-hidden=\"true\"><path fill=\"currentColor\" d=\"{d}\"/></svg>"
            );
            match title {
                Some(title) => format!(
                    "<div class=\"notice notice-{style}\"><p class=\"notice-title\">{icon_svg} {title}</p>{body_html}</div>"
                ),
                None => format!("<div class=\"notice notice-{style}\">{body_html}</div>"),
            }
        }
        "tabs" => {
            // 成员 {{% tab title="X" %}}...{{% /tab %}} 从 body 解析。
            // tab 序号用独立计数器而非 chunks enumerate 索引：body 里的
            // 前导文本 chunk（{{< tabs >}} 后的换行等）会使首个 tab 的
            // enumerate 索引非 0，导致"默认第一个 active"失效
            let inner = parse(token.body.as_deref().unwrap_or(""));
            let mut headers = String::new();
            let mut panels = String::new();
            let mut tab_index = 0usize;
            for chunk in inner.iter() {
                let Chunk::Shortcode(t) = chunk else { continue };
                if t.name != "tab" {
                    continue;
                }
                let i = tab_index;
                tab_index += 1;
                let params = parse_params(&t.params);
                let title = params
                    .iter()
                    .find(|(k, _)| k == "title")
                    .map(|(_, v)| v.clone())
                    .unwrap_or_else(|| format!("Tab {}", i + 1));
                let body_html = t.body.as_deref().map(md).unwrap_or_default();
                let active = if i == 0 { " active" } else { "" };
                // 表头与面板同步 active：默认选中第一个 tab——
                // 此前仅面板 active，首个 tab 标题无高亮，视觉上像未选中）
                let _ = writeln!(
                    headers,
                    "<button class=\"tab-header{active}\" data-tab=\"{i}\">{title}</button>"
                );
                let _ = writeln!(
                    panels,
                    "<div class=\"tab-panel{active}\" data-panel=\"{i}\">{body_html}</div>"
                );
            }
            format!(
                "<div class=\"tabs\"><div class=\"tab-headers\">{headers}</div><div class=\"tab-panels\">{panels}</div></div>"
            )
        }
        "children" => {
            // sort="weight" 即默认排序，参数仅记录不分支（排序规则唯一）
            let dir = ctx.rel_path.parent().unwrap_or(Path::new(""));
            let nodes = build_subtree(ctx.index, dir, 1, ctx.draft_enabled);
            let mut lis = String::new();
            for node in &nodes {
                let _ = writeln!(lis, "<li><a href=\"{}\">{}</a></li>", node.url, node.title);
            }
            format!("<ul class=\"children\">{lis}</ul>")
        }
        _ => {
            warn!(
                shortcode = %token.name,
                rel_path = %ctx.rel_path.display(),
                "未知 shortcode，原样输出"
            );
            unknown.record(&token.name, ctx.rel_path);
            let open = if token.delim == Delim::Percent {
                "{{%"
            } else {
                "{{<"
            };
            let close = if token.delim == Delim::Percent {
                "%}}"
            } else {
                ">}}"
            };
            let body = token.body.as_deref().unwrap_or("");
            format!(
                "{open} {} {} {close}{}{open} /{} {close}",
                token.name, token.params, body, token.name
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_shortcode_in_fence_not_scanned() {
        // fenced 代码块内的 shortcode 写法是展示源码，不识别（透传给 comrak 高亮）
        let md = "```markdown\n{{% notice %}}示例{{% /notice %}}\n```\n";
        let chunks = parse(md);
        assert_eq!(chunks, vec![Chunk::Text(md.to_string())]);
    }

    #[test]
    fn test_shortcode_in_tilde_fence_not_scanned() {
        let md = "~~~markdown\n{{< tabs >}}\n~~~\n";
        assert_eq!(parse(md), vec![Chunk::Text(md.to_string())]);
    }

    #[test]
    fn test_shortcode_in_inline_code_not_scanned() {
        // 行内代码内的写法同样透传
        let md = "写法 `{{% notice %}}` 即可";
        assert_eq!(parse(md), vec![Chunk::Text(md.to_string())]);
    }

    #[test]
    fn test_shortcode_after_fence_still_scanned() {
        // 围栏开闭状态不泄漏：块外 shortcode 照常识别（围栏文本保留为 Text）
        let chunks = parse("```rust\nx()\n```\n\n{{% notice %}}内容{{% /notice %}}");
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0], Chunk::Text("```rust\nx()\n```\n\n".to_string()));
        let Chunk::Shortcode(t) = &chunks[1] else {
            panic!()
        };
        assert_eq!(t.name, "notice");
    }

    #[test]
    fn test_shortcode_in_quadruple_backtick_fence() {
        // 四反引号外层围栏包三反引号示例：外层开启后，内层 ``` 行不是 fence 开关
        let md = "````markdown\n```bash\ncmd\n```\n{{% notice %}}\n````\n";
        assert_eq!(parse(md), vec![Chunk::Text(md.to_string())]);
    }

    #[test]
    fn test_unclosed_shortcode_in_fence_stays_text() {
        // 围栏内未闭合写法不触发降级逻辑（整块都是文本）
        let md = "```markdown\n{{% tabs %}}\n```";
        assert_eq!(parse(md), vec![Chunk::Text(md.to_string())]);
    }

    #[test]
    fn test_parse_simple_notice() {
        let chunks = parse("前文 {{% notice style=\"info\" %}}内容**加粗**{{% /notice %}} 后文");
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0], Chunk::Text("前文 ".to_string()));
        let Chunk::Shortcode(t) = &chunks[1] else {
            panic!()
        };
        assert_eq!(t.name, "notice");
        assert_eq!(t.params, "style=\"info\"");
        assert_eq!(t.body.as_deref(), Some("内容**加粗**"));
        assert_eq!(chunks[2], Chunk::Text(" 后文".to_string()));
    }

    #[test]
    fn test_parse_multiline_body() {
        let chunks = parse("{{% notice %}}\n第一行\n\n第二行\n{{% /notice %}}");
        let Chunk::Shortcode(t) = &chunks[0] else {
            panic!()
        };
        assert_eq!(t.body.as_deref(), Some("\n第一行\n\n第二行\n"));
    }

    #[test]
    fn test_parse_nested_same_name() {
        // notice 嵌套 notice：深度配对
        let input = "{{% notice %}}外层 {{% notice %}}内层{{% /notice %}} 回外层{{% /notice %}}";
        let chunks = parse(input);
        let Chunk::Shortcode(t) = &chunks[0] else {
            panic!()
        };
        assert_eq!(
            t.body.as_deref(),
            Some("外层 {{% notice %}}内层{{% /notice %}} 回外层")
        );
    }

    #[test]
    fn test_parse_nested_tab_in_notice() {
        let input = "{{% notice %}}{{< tabs >}}{{% tab title=\"A\" %}}x{{% /tab %}}{{< /tabs >}}{{% /notice %}}";
        let chunks = parse(input);
        assert_eq!(chunks.len(), 1, "外层 notice 吞掉内部全部");
        let Chunk::Shortcode(t) = &chunks[0] else {
            panic!()
        };
        assert_eq!(
            t.body.as_deref(),
            Some("{{< tabs >}}{{% tab title=\"A\" %}}x{{% /tab %}}{{< /tabs >}}")
        );
    }

    #[test]
    fn test_parse_unclosed_degrades_to_text() {
        let input = "正文 {{% notice %}}没闭合";
        let chunks = parse(input);
        // 未闭合：notice 开标签降级，正文保留
        assert_eq!(
            chunks,
            vec![Chunk::Text("正文 {{% notice %}}没闭合".to_string())]
        );
    }

    #[test]
    fn test_parse_self_closing_children() {
        let chunks = parse("{{% children sort=\"weight\" %}}");
        assert_eq!(chunks.len(), 1);
        let Chunk::Shortcode(t) = &chunks[0] else {
            panic!()
        };
        assert_eq!(t.name, "children");
        assert_eq!(t.params, "sort=\"weight\"");
        assert_eq!(t.body, None);
    }

    #[test]
    fn test_parse_angle_delim_with_body() {
        let chunks = parse("{{< notice >}}内容{{< /notice >}}");
        let Chunk::Shortcode(t) = &chunks[0] else {
            panic!()
        };
        assert_eq!(t.delim, Delim::Angle);
        assert_eq!(t.name, "notice");
        assert_eq!(t.body.as_deref(), Some("内容"));
    }

    #[test]
    fn test_parse_lone_tabs_degrades_to_text() {
        // {{< tabs >}} 是包裹型：无 {{< /tabs >}} 配对时降级为文本
        let chunks = parse("{{< tabs >}}");
        assert_eq!(chunks, vec![Chunk::Text("{{< tabs >}}".to_string())]);
    }

    #[test]
    fn test_parse_tabs_with_close() {
        let input = "{{< tabs >}}{{% tab title=\"说明\" %}}内容{{% /tab %}}{{< /tabs >}}";
        let chunks = parse(input);
        let Chunk::Shortcode(t) = &chunks[0] else {
            panic!()
        };
        assert_eq!(t.name, "tabs");
        assert_eq!(
            t.body.as_deref(),
            Some("{{% tab title=\"说明\" %}}内容{{% /tab %}}")
        );
    }

    #[test]
    fn test_parse_unknown_shortcode() {
        let chunks = parse("a {{% mermaid %}}图{{% /mermaid %}} b");
        assert_eq!(chunks.len(), 3);
        let Chunk::Shortcode(t) = &chunks[1] else {
            panic!()
        };
        assert_eq!(t.name, "mermaid");
        assert_eq!(t.body.as_deref(), Some("图"));
    }

    #[test]
    fn test_parse_params() {
        let params = parse_params("style=\"warning\" title=\"点击 查看代码\"");
        assert_eq!(
            params,
            vec![
                ("style".to_string(), "warning".to_string()),
                ("title".to_string(), "点击 查看代码".to_string()),
            ]
        );
        assert!(parse_params("").is_empty());
    }

    #[test]
    fn test_unclosed_tabs_degrades() {
        let input = "前 {{< tabs >}} 后";
        // tabs 需要配对 {{< /tabs >}}，未找到 → 降级
        let chunks = parse(input);
        assert_eq!(chunks, vec![Chunk::Text("前 {{< tabs >}} 后".to_string())]);
    }
}
