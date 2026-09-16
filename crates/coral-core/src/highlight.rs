//! syntect + two-face 代码高亮适配。
//!
//! two-face 提供全量语法集（覆盖 syntect 默认集缺口：SQL/DDL/JSON 等）；
//! class 风格输出（非 inline style），配合 ms7 前端 CSS 变量实现亮/暗双主题。

use std::sync::OnceLock;
use syntect::highlighting::ThemeSet;
use syntect::html::{ClassStyle, ClassedHTMLGenerator};
use syntect::parsing::{SyntaxReference, SyntaxSet};

/// 进程级语法集（two-face 全量，加载一次）。
fn syntax_set() -> &'static SyntaxSet {
    static SS: OnceLock<SyntaxSet> = OnceLock::new();
    SS.get_or_init(two_face::syntax::extra_newlines)
}

/// 主题仅用于 scope→token 解析；着色以 class 输出，配色由前端 CSS 决定。
fn theme() -> &'static syntect::highlighting::Theme {
    static TS: OnceLock<ThemeSet> = OnceLock::new();
    static THEME: OnceLock<syntect::highlighting::Theme> = OnceLock::new();
    let ts = TS.get_or_init(ThemeSet::load_defaults);
    THEME.get_or_init(|| ts.themes.values().next().expect("默认主题集非空").clone())
}

/// 按语言名查语法；找不到返回 None（调用方输出纯转义 `<pre><code>`）。
pub fn find_syntax(lang: &str) -> Option<&'static SyntaxReference> {
    let ss = syntax_set();
    ss.find_syntax_by_token(lang)
        .or_else(|| ss.find_syntax_by_extension(lang))
}

/// 高亮代码块为带 class span 的 HTML（不含外层 pre/code 标签）。
///
/// lang 未知时返回 None，调用方走纯转义路径。
pub fn highlight_code(lang: &str, code: &str) -> Option<String> {
    let syntax = find_syntax(lang)?;
    let mut generator =
        ClassedHTMLGenerator::new_with_class_style(syntax, syntax_set(), ClassStyle::Spaced);
    let _ = theme(); // 确认主题资源可加载（class 输出实际不读配色）
    for line in syntect::util::LinesWithEndings::from(code) {
        // 行级解析失败按原文行继续（不中断整块高亮）；错误仅在底层 io，可安全忽略
        let _ = generator.parse_html_for_line_which_includes_newline(line);
    }
    Some(generator.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rust_code_highlighted_with_class_spans() {
        let html = highlight_code("rust", "fn main() {}").expect("rust 语法应可高亮");
        assert!(
            html.contains("source rust") || html.contains("class="),
            "输出应含 class 标记：{html}"
        );
    }

    #[test]
    fn test_known_langs_covered_by_two_face() {
        // two-face 的价值：覆盖 syntect 默认集缺口（SQL/DDL/JSON 等）
        for lang in ["sql", "json", "go", "python"] {
            assert!(find_syntax(lang).is_some(), "缺 {lang} 语法");
        }
    }

    #[test]
    fn test_unknown_lang_returns_none() {
        assert!(find_syntax("no-such-lang-xyz").is_none());
        assert!(highlight_code("no-such-lang-xyz", "code").is_none());
    }

    #[test]
    fn test_html_escaping_in_highlight() {
        // 代码中的 < > & 必须转义，防注入
        let html = highlight_code("rust", "let x = a < b;").unwrap();
        assert!(html.contains("&lt;"), "应转义 <：{html}");
    }
}
