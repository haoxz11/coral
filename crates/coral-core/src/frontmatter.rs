//! front matter 解析：YAML（`---`）与 TOML（`+++`）双格式，
//! 按文件开头定界符自动识别；无 front matter 返回默认值 + 全文 body。

use serde_yaml_ng::Value as YamlValue;
use toml::Value as TomlValue;

/// 文档 front matter 中 coral 消费的已知字段。
///
/// `date` 保留原始标量文本，不解析为时间类型（页脚需原样展示）。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct FrontMatter {
    pub title: Option<String>,
    pub weight: Option<i64>,
    /// 默认 false；`draft: true` 的文档按配置 `content.draft` 决定是否服务
    pub draft: bool,
    pub permalink: Option<String>,
    /// 菜单图标：Iconify 图标名（`home` / `mdi:home`）或图片 URL
    pub icon: Option<String>,
    /// 页面原型：`home` = 门户首页布局；
    /// 其他值未识别，unconsumed 降级
    pub archetype: Option<String>,
    /// home 首页主按钮：markdown 链接语法 `[文案](地址)`
    pub url: Option<String>,
    pub disable_toc: bool,
    pub date: Option<String>,
    /// 未识别字段名（如 `icon`），供日志；排序保证确定性
    pub unconsumed: Vec<String>,
}

/// front matter 定界格式，由文件首行定界符决定。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FmFormat {
    Yaml,
    Toml,
}

impl FmFormat {
    fn delimiter(self) -> &'static str {
        match self {
            FmFormat::Yaml => "---",
            FmFormat::Toml => "+++",
        }
    }
}

impl std::fmt::Display for FmFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            FmFormat::Yaml => "yaml",
            FmFormat::Toml => "toml",
        })
    }
}

/// front matter 解析错误。调用方（scanner/请求路径）将此错误映射为
/// 该页 500 + render_failed 日志，不影响其他页面。
#[derive(Debug, thiserror::Error, PartialEq)]
pub enum FmError {
    /// 有开始定界符但找不到独占一行的结束定界符
    #[error("front matter 未闭合：缺少结束定界符 {0}")]
    Unterminated(&'static str),
    /// 字段存在但类型不可接受
    #[error("front matter 字段 {field} 类型不匹配：{message}")]
    InvalidField { field: String, message: String },
    /// 定界块本身语法非法
    #[error("front matter 解析失败（{format}）：{message}")]
    Parse { format: FmFormat, message: String },
}

/// 解析 `raw`，返回 front matter 与正文的剩余部分。
///
/// - 首行是 `---` → YAML；`+++` → TOML；否则视为无 front matter，
///   返回 `(默认值, 原文)`
/// - 有开始定界符但未闭合 → `FmError::Unterminated`（垃圾定界不 panic）
/// - body 返回切片，零拷贝
pub fn parse(raw: &str) -> Result<(FrontMatter, &str), FmError> {
    let Some(format) = detect_format(raw) else {
        return Ok((FrontMatter::default(), raw));
    };
    let (block, body) = split_block(raw, format)?;
    let fm = match format {
        FmFormat::Yaml => parse_yaml(block)?,
        FmFormat::Toml => parse_toml(block)?,
    };
    Ok((fm, body))
}

/// 首行（允许 `\r` 结尾）恰为定界符才算 front matter，行中出现的 `---` 不算。
fn detect_format(raw: &str) -> Option<FmFormat> {
    let first_line = raw.lines().next()?;
    let trimmed = first_line.trim_end_matches('\r');
    match trimmed {
        "---" => Some(FmFormat::Yaml),
        "+++" => Some(FmFormat::Toml),
        _ => None,
    }
}

/// 切出定界块与 body。开始定界符后逐行找独占一行的结束定界符。
fn split_block(raw: &str, format: FmFormat) -> Result<(&str, &str), FmError> {
    let delim = format.delimiter();
    // 首行定界符后兼容 \n 与 \r\n 两种行尾
    let after_open = raw
        .strip_prefix(delim)
        .and_then(|s| s.strip_prefix("\r\n").or_else(|| s.strip_prefix('\n')))
        .ok_or(FmError::Unterminated(delim))?;
    let mut offset = 0;
    for line in after_open.split_inclusive('\n') {
        if line.trim_end_matches(['\n', '\r']) == delim {
            return Ok((&after_open[..offset], &after_open[offset + line.len()..]));
        }
        offset += line.len();
    }
    Err(FmError::Unterminated(delim))
}

fn parse_yaml(block: &str) -> Result<FrontMatter, FmError> {
    let value: YamlValue = if block.trim().is_empty() {
        YamlValue::Null
    } else {
        serde_yaml_ng::from_str(block).map_err(|e| FmError::Parse {
            format: FmFormat::Yaml,
            message: e.to_string(),
        })?
    };
    let YamlValue::Mapping(map) = value else {
        return Ok(FrontMatter::default());
    };
    let mut fm = FrontMatter::default();
    for (k, v) in &map {
        let Some(key) = k.as_str() else {
            continue; // 非字符串 key 不属于任何已知字段
        };
        let field = key.to_string();
        match key {
            "title" => fm.title = yaml_scalar(&field, v)?,
            "weight" => fm.weight = yaml_i64(&field, v)?,
            "draft" => fm.draft = yaml_bool(&field, v)?,
            "permalink" => fm.permalink = yaml_scalar(&field, v)?,
            "icon" => fm.icon = yaml_scalar(&field, v)?,
            "archetype" => fm.archetype = yaml_scalar(&field, v)?,
            "url" => fm.url = yaml_scalar(&field, v)?,
            "disableToc" => fm.disable_toc = yaml_bool(&field, v)?,
            "date" => fm.date = yaml_scalar(&field, v)?,
            _ => fm.unconsumed.push(key.to_string()),
        }
    }
    fm.unconsumed.sort();
    Ok(fm)
}

/// 标量字段：字符串原样取值；数字/布尔按 YAML 展示形式转文本；
/// 映射/序列视为类型不匹配（InvalidField）。
fn yaml_scalar(field: &str, v: &YamlValue) -> Result<Option<String>, FmError> {
    match v {
        YamlValue::Null => Ok(None),
        YamlValue::String(s) => Ok(Some(s.clone())),
        YamlValue::Bool(b) => Ok(Some(b.to_string())),
        YamlValue::Number(n) => Ok(Some(n.to_string())),
        YamlValue::Tagged(t) => yaml_scalar(field, &t.value),
        YamlValue::Mapping(_) | YamlValue::Sequence(_) => Err(FmError::InvalidField {
            field: field.to_string(),
            message: "期望标量，得到映射或序列".to_string(),
        }),
    }
}

fn yaml_i64(field: &str, v: &YamlValue) -> Result<Option<i64>, FmError> {
    match v {
        YamlValue::Null => Ok(None),
        YamlValue::Number(n) => n.as_i64().map(Some).ok_or(FmError::InvalidField {
            field: field.to_string(),
            message: format!("超出 i64 范围：{n}"),
        }),
        _ => Err(FmError::InvalidField {
            field: field.to_string(),
            message: "期望整数".to_string(),
        }),
    }
}

fn yaml_bool(field: &str, v: &YamlValue) -> Result<bool, FmError> {
    match v {
        YamlValue::Null => Ok(false),
        YamlValue::Bool(b) => Ok(*b),
        _ => Err(FmError::InvalidField {
            field: field.to_string(),
            message: "期望布尔值".to_string(),
        }),
    }
}

fn parse_toml(block: &str) -> Result<FrontMatter, FmError> {
    let table: toml::Table = if block.trim().is_empty() {
        toml::Table::new()
    } else {
        toml::from_str(block).map_err(|e| FmError::Parse {
            format: FmFormat::Toml,
            message: e.to_string(),
        })?
    };
    let mut fm = FrontMatter::default();
    for (key, v) in &table {
        let field = key.to_string();
        match key.as_str() {
            "title" => fm.title = toml_scalar(&field, v)?,
            "weight" => fm.weight = toml_i64(&field, v)?,
            "draft" => fm.draft = toml_bool(&field, v)?,
            "permalink" => fm.permalink = toml_scalar(&field, v)?,
            "icon" => fm.icon = toml_scalar(&field, v)?,
            "archetype" => fm.archetype = toml_scalar(&field, v)?,
            "url" => fm.url = toml_scalar(&field, v)?,
            "disableToc" => fm.disable_toc = toml_bool(&field, v)?,
            "date" => fm.date = toml_scalar(&field, v)?,
            _ => fm.unconsumed.push(key.clone()),
        }
    }
    fm.unconsumed.sort();
    Ok(fm)
}

fn toml_scalar(field: &str, v: &TomlValue) -> Result<Option<String>, FmError> {
    match v {
        TomlValue::String(s) => Ok(Some(s.clone())),
        TomlValue::Integer(_)
        | TomlValue::Float(_)
        | TomlValue::Boolean(_)
        | TomlValue::Datetime(_) => Ok(Some(v.to_string())),
        TomlValue::Array(_) | TomlValue::Table(_) => Err(FmError::InvalidField {
            field: field.to_string(),
            message: "期望标量，得到数组或表".to_string(),
        }),
    }
}

fn toml_i64(field: &str, v: &TomlValue) -> Result<Option<i64>, FmError> {
    match v {
        TomlValue::Integer(i) => Ok(Some(*i)),
        _ => Err(FmError::InvalidField {
            field: field.to_string(),
            message: "期望整数".to_string(),
        }),
    }
}

fn toml_bool(field: &str, v: &TomlValue) -> Result<bool, FmError> {
    match v {
        TomlValue::Boolean(b) => Ok(*b),
        _ => Err(FmError::InvalidField {
            field: field.to_string(),
            message: "期望布尔值".to_string(),
        }),
    }
}

/// frontmatter `date` 规范化：任意写法 → 统一 `YYYY-MM-DD` 形态。
///
/// 兼容真实内容源的写法差异（ocean-book 实测三种：`2025-03-01`、
/// `2025-03-01 10:30:09`、`2025-03-01 9:15`——小时不补零；TOML Datetime
/// 会带 `T`/时区后缀）。只取日期部分并校验月/日范围，保证：
/// - 同一天带时间与不带时间的文档排序/仲裁结果一致（原始字符串比较
///   会因前缀关系反直觉）
/// - 非法值（如正文格式 `Tue, 07 Jun 2014 ...`）返回 None，消费方
///   按"无 date"沉底/兜底，不乱插
///
/// `FrontMatter.date` 保留原始文本（页脚原样展示），
/// 本函数是排序/仲裁/搜索等**比较用途**的唯一口径。
pub fn normalize_date(raw: &str) -> Option<String> {
    let s = raw.trim();
    let bytes = s.as_bytes();
    if bytes.len() < 10 {
        return None;
    }
    let num = |r: std::ops::Range<usize>| -> Option<i64> { s.get(r)?.parse::<i64>().ok() };
    let (y, m, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    if !(bytes[4] == b'-' && bytes[7] == b'-') {
        return None;
    }
    // 月/日范围 + 每月天数校验（闰年含）
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let dim = match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if leap => 29,
        _ => 28,
    };
    if d > dim {
        return None;
    }
    Some(format!("{y:04}-{m:02}-{d:02}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_yaml_full_fields() {
        // menuPre 已废弃：保留在输入中验证降级为未识别字段
        let raw = "---\ntitle: \"标题\"\nweight: 10\ndraft: true\npermalink: /custom/\ndisableToc: true\ndate: 2024-01-01\nicon: mdi:home\nmenuPre: \"<i class=\\\"fa fa-book\\\"></i>\"\n---\n正文";
        let (fm, body) = parse(raw).unwrap();
        assert_eq!(fm.title.as_deref(), Some("标题"));
        assert_eq!(fm.weight, Some(10));
        assert!(fm.draft);
        assert_eq!(fm.permalink.as_deref(), Some("/custom/"));
        assert_eq!(fm.icon.as_deref(), Some("mdi:home"));
        // menuPre 已废弃：未识别字段降级
        assert!(fm.unconsumed.contains(&"menuPre".to_string()));
        assert!(fm.disable_toc);
        assert_eq!(fm.date.as_deref(), Some("2024-01-01"));
        assert_eq!(fm.unconsumed, vec!["menuPre"]);
        assert_eq!(body, "正文");
    }

    #[test]
    fn test_toml_full_fields() {
        let raw = "+++\ntitle = \"标题\"\nweight = -3\ndraft = false\npermalink = \"/custom/\"\ndisableToc = false\ndate = 2024-01-01\n+++\n正文";
        let (fm, body) = parse(raw).unwrap();
        assert_eq!(fm.title.as_deref(), Some("标题"));
        assert_eq!(fm.weight, Some(-3));
        assert!(!fm.draft);
        assert_eq!(fm.permalink.as_deref(), Some("/custom/"));
        assert!(!fm.disable_toc);
        // TOML 原生 datetime 类型按展示形式保留（不解析为时间类型）
        assert!(
            fm.date
                .as_deref()
                .is_some_and(|d| d.starts_with("2024-01-01"))
        );
        assert_eq!(body, "正文");
    }

    #[test]
    fn test_missing_fields_get_defaults() {
        let raw = "---\ntitle: 仅标题\n---\n正文";
        let (fm, body) = parse(raw).unwrap();
        assert_eq!(fm.title.as_deref(), Some("仅标题"));
        assert_eq!(fm.weight, None);
        assert!(!fm.draft);
        assert_eq!(fm.permalink, None);
        assert!(!fm.disable_toc);
        assert!(fm.unconsumed.is_empty());
        assert_eq!(body, "正文");
    }

    #[test]
    fn test_no_frontmatter_whole_body() {
        let raw = "# 直接正文\n\n没有 front matter";
        let (fm, body) = parse(raw).unwrap();
        assert_eq!(fm, FrontMatter::default());
        assert_eq!(body, raw);
    }

    #[test]
    fn test_garbage_unterminated_delimiter() {
        let raw = "---\ntitle: 没有闭合\n正文继续";
        assert_eq!(parse(raw), Err(FmError::Unterminated("---")));
    }

    #[test]
    fn test_mismatched_delimiters() {
        // YAML 开始、TOML 结束：结束定界符不匹配，视为未闭合
        let raw = "---\ntitle: x\n+++\n正文";
        assert_eq!(parse(raw), Err(FmError::Unterminated("---")));
    }

    #[test]
    fn test_empty_frontmatter_block() {
        let raw = "---\n---\n正文";
        let (fm, body) = parse(raw).unwrap();
        assert_eq!(fm, FrontMatter::default());
        assert_eq!(body, "正文");
    }

    #[test]
    fn test_null_values_treated_as_absent() {
        let raw = "---\ntitle:\nweight:\n---\n正文";
        let (fm, _) = parse(raw).unwrap();
        assert_eq!(fm.title, None);
        assert_eq!(fm.weight, None);
    }

    #[test]
    fn test_yaml_syntax_error() {
        let raw = "---\ntitle: [unclosed\n---\n正文";
        assert!(matches!(
            parse(raw),
            Err(FmError::Parse {
                format: FmFormat::Yaml,
                ..
            })
        ));
    }

    #[test]
    fn test_field_type_mismatch() {
        let raw = "---\nweight: 不是数字\n---\n正文";
        assert_eq!(
            parse(raw),
            Err(FmError::InvalidField {
                field: "weight".to_string(),
                message: "期望整数".to_string()
            })
        );
    }

    #[test]
    fn test_crlf_line_endings() {
        let raw = "---\r\ntitle: 标题\r\n---\r\n正文";
        let (fm, body) = parse(raw).unwrap();
        assert_eq!(fm.title.as_deref(), Some("标题"));
        assert_eq!(body, "正文");
    }

    #[test]
    fn test_inline_delimiter_in_body_not_frontmatter() {
        // 首行不是定界符，正文中出现的 --- 不触发解析
        let raw = "正文第一行\n---\n第二行";
        let (fm, body) = parse(raw).unwrap();
        assert_eq!(fm, FrontMatter::default());
        assert_eq!(body, raw);
    }

    #[test]
    fn test_unconsumed_sorted_deterministic() {
        let raw = "---\nzz: 1\nunknownicon: book\naa: 2\ntitle: t\n---\n正文";
        let (fm, _) = parse(raw).unwrap();
        assert_eq!(fm.unconsumed, vec!["aa", "unknownicon", "zz"]);
    }

    #[test]
    fn test_normalize_date_variants() {
        // ocean-book 实测三种合法形态 + TOML Datetime 后缀
        assert_eq!(normalize_date("2025-03-01"), Some("2025-03-01".to_string()));
        assert_eq!(
            normalize_date("2025-03-01 10:30:09"),
            Some("2025-03-01".to_string())
        );
        assert_eq!(
            normalize_date("2025-03-01 9:15"),
            Some("2025-03-01".to_string())
        );
        assert_eq!(
            normalize_date("2025-03-01T10:30:00Z"),
            Some("2025-03-01".to_string())
        );
        // 尾部空白（实测样本存在）
        assert_eq!(
            normalize_date("2025-03-01 "),
            Some("2025-03-01".to_string())
        );
    }

    #[test]
    fn test_normalize_date_invalid() {
        assert_eq!(normalize_date(""), None);
        assert_eq!(normalize_date("2025-3-1"), None); // 不补零
        assert_eq!(normalize_date("2025-13-01"), None); // 月越界
        assert_eq!(normalize_date("2025-02-30"), None); // 日越界（非闰年 2 月）
        assert_eq!(normalize_date("2024-02-29"), Some("2024-02-29".to_string())); // 闰年
        assert_eq!(normalize_date("2023-02-29"), None); // 平年
        assert_eq!(
            normalize_date("Tue, 07 Jun 2014 20:51:35 GMT"),
            None,
            "正文形态的时间串应为 None（消费方按无 date 沉底/兜底）"
        );
    }
}
