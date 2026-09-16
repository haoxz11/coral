//! URL 规范：rel_path 与 URL 的精确换算，
//! 所有路由/树/链接生成共用。
//!
//! - 生成：每段独立 percent-encode（RFC 3986 unreserved `A-Za-z0-9-._~` 之外全部编码）
//! - 匹配：逐段 percent-decode；路由表 key 存 decode 形态（与 rel_path 同构）
//! - decode 产生的 `..` 等越界内容由请求侧 canonicalize 校验拦截（ms5），本模块只做纯换算

/// percent-encoding / 解码错误。
#[derive(Debug, thiserror::Error, PartialEq)]
pub enum UrlError {
    #[error("非法 percent-escape：{0}")]
    InvalidEscape(String),
    #[error("解码结果不是合法 UTF-8")]
    InvalidUtf8,
}

const HEX: &[u8; 16] = b"0123456789ABCDEF";

/// 单个路径段的 percent-encode（unreserved 字符之外全部 `%XX`，大写十六进制）。
pub fn encode_segment(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for &b in segment.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 0x0f) as usize] as char);
        }
    }
    out
}

/// 单个路径段的 percent-decode；非法 `%` 序列或解码后非 UTF-8 视为错误。
pub fn decode_segment(segment: &str) -> Result<String, UrlError> {
    let bytes = segment.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'%' {
            out.push(bytes[i]);
            i += 1;
            continue;
        }
        if i + 2 >= bytes.len() {
            return Err(UrlError::InvalidEscape(
                String::from_utf8_lossy(&bytes[i..]).into_owned(),
            ));
        }
        let high = hex_val(bytes[i + 1]).ok_or_else(|| {
            UrlError::InvalidEscape(String::from_utf8_lossy(&bytes[i..i + 3]).into_owned())
        })?;
        let low = hex_val(bytes[i + 2]).ok_or_else(|| {
            UrlError::InvalidEscape(String::from_utf8_lossy(&bytes[i..i + 3]).into_owned())
        })?;
        out.push((high << 4) | low);
        i += 3;
    }
    String::from_utf8(out).map_err(|_| UrlError::InvalidUtf8)
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// decode 形态 URL → encode 形态（逐段编码，保留 `/` 分隔与首段空串）。
pub fn encode_url(decoded: &str) -> String {
    let mut out = String::with_capacity(decoded.len());
    for (i, seg) in decoded.split('/').enumerate() {
        if i > 0 {
            out.push('/');
        }
        out.push_str(&encode_segment(seg));
    }
    out
}

/// encode 形态 URL → decode 形态（逐段解码）。
pub fn decode_url(encoded: &str) -> Result<String, UrlError> {
    let mut out = String::with_capacity(encoded.len());
    for (i, seg) in encoded.split('/').enumerate() {
        if i > 0 {
            out.push('/');
        }
        out.push_str(&decode_segment(seg)?);
    }
    Ok(out)
}

/// 普通文档 rel_path → URL（decode 形态）：去 `.md` 扩展名，段以 `/` 连接。
/// 分支页（`_index.md`）用 [`dir_url`]。
pub fn page_url(rel_path: &std::path::Path) -> String {
    let mut segments: Vec<String> = rel_path
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    if let Some(stripped) = segments
        .last_mut()
        .and_then(|last| last.strip_suffix(".md"))
    {
        // `stripped` 借自 segments 的最后一段，直接重写该段
        let stripped = stripped.to_string();
        *segments.last_mut().unwrap() = stripped;
    }
    format!("/{}", segments.join("/"))
}

/// 目录 rel_path → URL（decode 形态）：目录自身的 URL，规范化无尾斜杠；根目录为 `/`。
pub fn dir_url(dir: &std::path::Path) -> String {
    if dir.as_os_str().is_empty() {
        return "/".to_string();
    }
    let segments: Vec<String> = dir
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    format!("/{}", segments.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn test_encode_segment_unreserved_preserved() {
        assert_eq!(encode_segment("a-b.c_d~e"), "a-b.c_d~e");
    }

    #[test]
    fn test_encode_segment_chinese_and_space() {
        // 脚 = E8 84 9A，本 = E6 9C AC
        assert_eq!(encode_segment("脚本"), "%E8%84%9A%E6%9C%AC");
        assert_eq!(encode_segment("a b"), "a%20b");
    }

    #[test]
    fn test_encode_url_keeps_slash_separators() {
        assert_eq!(encode_url("/kun/脚本/foo"), "/kun/%E8%84%9A%E6%9C%AC/foo");
        assert_eq!(encode_url("/"), "/");
    }

    #[test]
    fn test_decode_url_roundtrip() {
        let decoded = decode_url("/kun/%E8%84%9A%E6%9C%AC/foo").unwrap();
        assert_eq!(decoded, "/kun/脚本/foo");
        assert_eq!(encode_url(&decoded), "/kun/%E8%84%9A%E6%9C%AC/foo");
    }

    #[test]
    fn test_decode_dotdot_yields_traversal_for_downstream_check() {
        // decode 本身不设防；越界拦截在请求侧 canonicalize
        assert_eq!(decode_url("/%2e%2e/etc").unwrap(), "/../etc");
    }

    #[test]
    fn test_decode_invalid_escape() {
        assert_eq!(
            decode_url("/a/%zz"),
            Err(UrlError::InvalidEscape("%zz".to_string()))
        );
        assert_eq!(
            decode_url("/a/%2"),
            Err(UrlError::InvalidEscape("%2".to_string()))
        );
    }

    #[test]
    fn test_decode_invalid_utf8() {
        // %FF 单字节不是合法 UTF-8
        assert_eq!(decode_url("/%FF"), Err(UrlError::InvalidUtf8));
    }

    #[test]
    fn test_page_url_strips_md_extension() {
        assert_eq!(page_url(Path::new("intro.md")), "/intro");
        assert_eq!(page_url(Path::new("kun/脚本/foo.md")), "/kun/脚本/foo");
        assert_eq!(page_url(Path::new("index.md")), "/index");
    }

    #[test]
    fn test_dir_url_no_trailing_slash() {
        assert_eq!(dir_url(Path::new("")), "/");
        assert_eq!(dir_url(Path::new("kun/脚本")), "/kun/脚本");
    }
}
