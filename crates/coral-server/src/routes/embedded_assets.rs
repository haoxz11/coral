//! 内嵌静态资源路由：rust-embed /assets/。
//!
//! app.css / app.js（图标方案为 frontmatter icon + Iconify 运行时，
//! 见 templates.rs has_icons）。
//!
//! 资产 URL 带内容指纹（`app.<xxh3 前 8 hex>.css`）：内容变则 URL 变，
//! immutable 一年缓存因此语义正确（升级二进制后老缓存自然失效）。
//! 无指纹路径继续服务（等价内容），供手工直链/调试。

use axum::extract::Path;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;
use std::collections::HashMap;
use std::sync::OnceLock;
use xxhash_rust::xxh3::xxh3_64;

#[derive(RustEmbed)]
#[folder = "assets/"]
struct Assets;

/// `GET /assets/{*path}`：同时承接带指纹与无指纹两种路径。
pub async fn asset_handler(Path(path): Path<String>) -> Response {
    // 路径安全：拒绝段穿越（内嵌资源是编译期集合，但防御性检查保留）
    if path
        .split('/')
        .any(|s| s == ".." || s == "." || s.is_empty())
    {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    // 剥离指纹段：`app.<hash>.css` → `app.css`（指纹仅是缓存键，不参与查找）
    let lookup = strip_fingerprint(&path);
    let Some(file) = Assets::get(&lookup) else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    let mime = match lookup.rsplit('.').next() {
        Some("css") => "text/css; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("woff2") => "font/woff2",
        Some("woff") => "font/woff",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        _ => "application/octet-stream",
    };
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(mime).expect("mime 可解析"),
    );
    // 内嵌资源随二进制版本不可变：URL 指纹保证内容变则 URL 变
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=31536000, immutable"),
    );
    (StatusCode::OK, headers, file.data).into_response()
}

/// `app.<hash>.css` → `app.css`；指纹段为恰 8 位十六进制，不匹配则原样返回。
fn strip_fingerprint(path: &str) -> String {
    let (dir, file) = match path.rsplit_once('/') {
        Some((d, f)) => (format!("{d}/"), f),
        None => (String::new(), path),
    };
    let Some((stem, ext)) = file.rsplit_once('.') else {
        return path.to_string();
    };
    let Some((name, hash)) = stem.rsplit_once('.') else {
        return path.to_string();
    };
    if hash.len() == 8 && hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        format!("{dir}{name}.{ext}")
    } else {
        path.to_string()
    }
}

/// 带指纹的资产 URL（如 `/assets/app.a1b2c3d4.css`），模板引用。
///
/// 哈希在进程首次调用时对嵌入内容计算并缓存；文件不存在时退回无指纹 URL。
pub fn fingerprinted_url(file: &str) -> String {
    static HASHES: OnceLock<HashMap<String, u64>> = OnceLock::new();
    let hashes = HASHES.get_or_init(|| {
        let mut m = HashMap::new();
        for name in Assets::iter() {
            let Some(file) = Assets::get(name.as_ref()) else {
                continue;
            };
            m.insert(name.as_ref().to_string(), xxh3_64(file.data.as_ref()));
        }
        m
    });
    let Some(hash) = hashes.get(file) else {
        return format!("/assets/{file}");
    };
    let Some((stem, ext)) = file.rsplit_once('.') else {
        return format!("/assets/{file}");
    };
    // 取 u64 哈希低 32 位（8 hex）：与 strip_fingerprint 的 8 位识别约定一致
    let short = (*hash & 0xffff_ffff) as u32;
    format!("/assets/{stem}.{short:08x}.{ext}", short = short)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fingerprint_url_stable_and_hashed() {
        let a = fingerprinted_url("app.css");
        assert!(a.starts_with("/assets/app."), "{a}");
        assert!(a.ends_with(".css"));
        assert_eq!(a, fingerprinted_url("app.css"));
        assert_eq!(fingerprinted_url("no-such.file"), "/assets/no-such.file");
    }

    #[test]
    fn test_strip_fingerprint_forms() {
        assert_eq!(strip_fingerprint("app.a1b2c3d4.css"), "app.css");
        assert_eq!(strip_fingerprint("app.css"), "app.css");
        assert_eq!(strip_fingerprint("app.a1b2c3d4.js"), "app.js");
        // 8 位非纯 hex 不剥（普通文件名不动）
        assert_eq!(strip_fingerprint("app.abcdefgh.css"), "app.abcdefgh.css");
        assert_eq!(
            strip_fingerprint("fa/solid.900.woff2"),
            "fa/solid.900.woff2"
        );
    }
}
