//! 数学公式定界预处理（M2-s3 KaTeX）。
//!
//! 在 comrak 渲染**前**的文本层工作（ shortcode 之后）：
//! - 块级 `$$...$$` → 占位符（默认启用）
//! - 行内 `$...$` → 占位符（`inline_math` 开启才识别；`$` 对紧邻非空白）
//! - 跳过 fenced 代码块与行内代码内的 `$`；`\$` 转义不触发
//!
//! 占位符与 shortcode 共用私有区字符（comrak 原样透传），回填时替换为
//! katex 语义占位（原文转义——安全红线：公式原文可含任意文本）。

use crate::render::escape_html;

/// 数学公式占位符形态（回填后进 HTML）。
/// `data-original` 保留转义原文供前端 KaTeX render；元素本身的内容
/// 也是转义原文——CDN 不可达/无 JS 时天然降级为可读文本。
pub fn katex_block_html(formula: &str) -> String {
    format!(
        "<span class=\"katex-block\" data-formula=\"{}\">$$ {} $$</span>",
        escape_attr(formula),
        escape_html(formula)
    )
}

pub fn katex_inline_html(formula: &str) -> String {
    format!(
        "<span class=\"katex-inline\" data-formula=\"{}\">${}$</span>",
        escape_attr(formula),
        escape_html(formula)
    )
}

fn escape_attr(s: &str) -> String {
    escape_html(s).replace('"', "&quot;")
}

/// 预处理结果：占位符替换后的文本 + 公式清单（按出现顺序）。
pub struct MathExtract {
    pub text: String,
    pub formulas: Vec<(String, bool /*is_block*/)>,
}

/// 提取数学公式为占位符。
///
/// `placeholder_fn(i)` 由调用方提供（与 shortcode 占位符区分命名空间，
/// 避免冲突——这里用独立计数器，占位符形如 `\u{E001}<i>\u{E001}`）。
pub fn extract_math(input: &str, inline_enabled: bool) -> MathExtract {
    const PH: char = '\u{E001}';
    let mut out = String::with_capacity(input.len());
    let mut formulas: Vec<(String, bool)> = Vec::new();
    let bytes = input.as_bytes();
    let mut i = 0;
    let mut in_fence = false;
    let mut fence_marker_len = 0usize;

    while i < bytes.len() {
        let b = bytes[i];
        // fenced 代码块状态跟踪（``` 或 ~~~ 开头的行）：
        // 整个 run 一次性消费——状态翻转后不再让 run 内剩余字符重复判定
        //（修复：闭合 ``` 行的后续 backtick 被误判为新 fence 开启，
        // 导致双 fence 后 $$ 永远丢失）
        if (b == b'`' || b == b'~') && is_line_start(&out).is_some() {
            let run = count_run(bytes, i, b);
            if run >= 3 && run == bytes.len().saturating_sub(i).min(run) {
                // run 延伸到行尾才是 fence（行中 backtick 是行内代码）
                let after = bytes.get(i + run);
                if after.is_none() || after.is_some_and(|c| *c == b'\n' || *c == b'\r') {
                    if in_fence && run == fence_marker_len {
                        in_fence = false;
                    } else if !in_fence {
                        in_fence = true;
                        fence_marker_len = run;
                    }
                    out.push_str(&input[i..i + run]);
                    i += run;
                    continue;
                }
            }
        }
        // 行内代码 `...`：原样复制到闭合反引号
        if b == b'`' && !in_fence {
            let (rel_end, content_len) = inline_code_span(bytes, i);
            if content_len > 0 {
                out.push_str(&input[i..i + rel_end]);
                i += rel_end;
                continue;
            }
        }
        // \$ 转义：输出 $ 本身，不触发定界
        if b == b'\\' && bytes.get(i + 1) == Some(&b'$') {
            out.push('$');
            i += 2;
            continue;
        }
        if b == b'$' && !in_fence {
            // 块级 $$...$$
            if bytes.get(i + 1) == Some(&b'$')
                && let Some(end) = find_delim(input, i + 2, "$$")
            {
                let formula = &input[i + 2..end];
                if !formula.trim().is_empty() {
                    formulas.push((formula.to_string(), true));
                    out.push_str(&format!("{PH}{}{PH}", formulas.len() - 1));
                    i = end + 2;
                    continue;
                }
            }
            // 行内 $...$（可配）：$ 对紧邻非空白
            if inline_enabled
                && bytes.get(i + 1).is_some_and(|c| !c.is_ascii_whitespace())
                && !starts_fence(bytes, i + 1)
                && let Some(end) = find_inline_end(input, i + 1)
            {
                let formula = &input[i + 1..end];
                formulas.push((formula.to_string(), false));
                out.push_str(&format!("{PH}{}{PH}", formulas.len() - 1));
                i = end + 1;
                continue;
            }
        }
        let ch_len = utf8_len(b);
        let end = (i + ch_len).min(input.len());
        out.push_str(&input[i..end]);
        i += ch_len;
    }

    MathExtract {
        text: out,
        formulas,
    }
}

/// 回填：占位符 → katex 语义占位 HTML。
pub fn replace_math_placeholders(html: &str, math: &MathExtract) -> String {
    const PH: char = '\u{E001}';
    let mut out = html.to_string();
    for (i, (formula, is_block)) in math.formulas.iter().enumerate() {
        let ph = format!("{PH}{i}{PH}");
        let replacement = if *is_block {
            katex_block_html(formula)
        } else {
            katex_inline_html(formula)
        };
        out = out.replace(&ph, &replacement);
    }
    out
}

// ---- 工具函数 ----

fn utf8_len(b: u8) -> usize {
    match b {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        _ => 4,
    }
}

/// 是否处于行首（out 的末尾是 \n 或 out 为空）。
fn is_line_start(out: &str) -> Option<()> {
    if out.is_empty() || out.ends_with('\n') {
        Some(())
    } else {
        None
    }
}

fn count_run(bytes: &[u8], mut i: usize, ch: u8) -> usize {
    let mut n = 0;
    while i < bytes.len() && bytes[i] == ch {
        n += 1;
        i += 1;
    }
    n
}

/// 行内代码 span：返回 (整段长度含反引号, 是否有效)。无效返回 (0, 0)。
fn inline_code_span(bytes: &[u8], start: usize) -> (usize, usize) {
    let open = count_run(bytes, start, b'`');
    let mut i = start + open;
    while i < bytes.len() {
        if bytes[i] == b'`' {
            let close = count_run(bytes, i, b'`');
            if close == open {
                return (i + close - start, open);
            }
            i += close;
        } else {
            i += utf8_len(bytes[i]);
        }
    }
    (0, 0)
}

/// 找定界符（跨行），返回其起始位置。
fn find_delim(input: &str, from: usize, delim: &str) -> Option<usize> {
    input[from..].find(delim).map(|p| from + p)
}

/// 行内公式闭合 `$`：与开 `$` 同行（跨行 $ 是块级语义），
/// 且闭合前一个字符非空白、非标点开头歧义。
fn find_inline_end(input: &str, from: usize) -> Option<usize> {
    let bytes = input.as_bytes();
    let mut i = from;
    while i < bytes.len() {
        match bytes[i] {
            b'\n' => return None, // 行内公式不跨行
            b'\\' => i += 2,      // 转义字符跳过
            b'$' => {
                if i > from && !bytes[i - 1].is_ascii_whitespace() {
                    return Some(i);
                }
                return None;
            }
            b'`' => return None, // 公式内不该有代码——视为误识别
            b => i += utf8_len(b),
        }
    }
    None
}

fn starts_fence(bytes: &[u8], i: usize) -> bool {
    bytes.get(i) == Some(&b'`') && bytes.get(i + 1) == Some(&b'`')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_two_fences_then_math_regression() {
        // 回归：闭合 ``` 行的 run 内字符曾被误判为新 fence 开启，
        // 双 fence 后 $$ 永远丢失（e2e 16b 抓出）
        let m = extract_math(
            "```rust\nx()\n```\n\n```mermaid\ng\n```\n\n$$E=mc$$\n",
            false,
        );
        assert_eq!(m.formulas.len(), 1, "text={:?}", m.text);
    }

    #[test]
    fn test_block_math_extracted() {
        let m = extract_math("前文\n\n$$ E = mc^2 $$\n\n后文", false);
        assert_eq!(m.formulas.len(), 1);
        assert_eq!(m.formulas[0], (" E = mc^2 ".to_string(), true));
        assert!(m.text.starts_with("前文"));
        assert!(!m.text.contains("mc^2"));
    }

    #[test]
    fn test_inline_math_respects_flag() {
        // 默认关闭
        let m = extract_math("价值 $100 与 $200", false);
        assert!(m.formulas.is_empty(), "默认不识别行内");
        assert!(m.text.contains("$100"));
        // 开启后识别（紧邻非空白）
        let m = extract_math("公式 $x^2$ 结束", true);
        assert_eq!(m.formulas.len(), 1);
        assert_eq!(m.formulas[0].0, "x^2");
        // 开启后相邻双 $（"价格 $100 和 $200"）不误识别：闭合 $ 前是空白，
        // 不满足"紧邻非空白"规则——保守失败优于误吞
        let m = extract_math("价格 $100 和 $200 总计", true);
        assert!(
            m.formulas.is_empty(),
            "闭合 $ 前空白不识别：{:?}",
            m.formulas
        );
        // "价格$a$ 总计"形态：两侧紧邻，正确识别
        let m = extract_math("价格$a$总计", true);
        assert_eq!(m.formulas.len(), 1);
        assert_eq!(m.formulas[0].0, "a");
    }

    #[test]
    fn test_inline_math_not_cross_line_or_code() {
        // 不跨行
        let m = extract_math("$x\ny$", true);
        assert!(m.formulas.is_empty());
        // 行内代码内不触发
        let m = extract_math("代码 `$HOME` 变量", true);
        assert!(m.formulas.is_empty(), "行内代码内 $ 不触发");
        assert!(m.text.contains("`$HOME`"));
        // fenced 内不触发
        let m = extract_math("```bash\necho $HOME\n```\n正文", true);
        assert!(m.formulas.is_empty(), "fenced 代码块内 $ 不触发");
        // \$ 转义
        let m = extract_math("价格 \\$100", true);
        assert!(m.formulas.is_empty());
        assert!(m.text.contains("$100"), "转义后 $ 原样输出：{}", m.text);
    }

    #[test]
    fn test_html_escaping_in_formula() {
        // 安全红线：公式原文含 HTML 标签必须转义
        let html = katex_block_html("<script>alert(1)</script>");
        assert!(html.contains("&lt;script&gt;"), "{html}");
        assert!(!html.contains("<script>"), "{html}");
        let inline = katex_inline_html("a<b");
        assert!(inline.contains("&lt;"), "{inline}");
    }

    #[test]
    fn test_placeholder_roundtrip() {
        let m = extract_math(r"前 $x_1$ 后 $$\int_0^1 f$$ 尾", true);
        assert_eq!(m.formulas.len(), 2);
        const PH: char = '\u{E001}';
        // 占位符形态验证（E001 私有区包裹索引）
        assert!(m.text.contains(&format!("{PH}0{PH}")), "text={:?}", m.text);
        assert!(m.text.contains(&format!("{PH}1{PH}")), "text={:?}", m.text);
        // 回填 HTML 结构
        let html = replace_math_placeholders(&m.text, &m);
        assert!(html.contains("katex-inline"), "{html}");
        assert!(html.contains("katex-block"), "{html}");
        assert!(html.contains("data-formula=\"x_1\""), "{html}");
        assert!(
            !html.contains(&format!("{PH}0{PH}")),
            "占位符应全部回填：{html}"
        );
    }
}
