//! 静态资源路由：content 内非 md 文件按原路径服务，
//! mime 推断 + immutable 长缓存头；路径安全是本模块红线。

use crate::state::AppState;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use std::sync::Arc;
use tracing::info;

/// `GET /assets/{*path}`：主题内嵌资源由 ms7 rust-embed 接入；
/// 本 handler 承接 content 内非 md 文件的 `/{*path}` 静态回退。
pub async fn content_asset_handler(
    State(state): State<Arc<AppState>>,
    Path(path): Path<String>,
) -> Response {
    serve_content_asset(&state, &path).await
}

/// 路径安全管线（顺序不可颠倒）：
/// decode（axum Path 已做）→ 拒绝 `.`/`..`/空段 → canonicalize →
/// 校验落点仍在 content 根内 → 拒绝 symlink 逃逸。
/// 任何一步失败一律 404（不区分原因，不泄露信息）。
pub async fn serve_content_asset(state: &Arc<AppState>, decoded: &str) -> Response {
    let mut rel = std::path::PathBuf::new();
    for seg in decoded.split('/') {
        match seg {
            "" => continue,
            "." | ".." => return not_found(),
            s => {
                if s.contains('\0') {
                    return not_found();
                }
                rel.push(s);
            }
        }
    }
    // md 文件走页面路由，不走静态
    if rel.extension().is_some_and(|e| e == "md") {
        return not_found();
    }

    let abs = state.content_root.join(&rel);
    let meta = match tokio::fs::symlink_metadata(&abs).await {
        Ok(m) => m,
        Err(_) => return not_found(),
    };
    if meta.is_dir() {
        return not_found();
    }
    // canonicalize 校验落点（symlink 逃逸在此拦截；根内 symlink 允许跟随，A6）
    let canonical = match tokio::fs::canonicalize(&abs).await {
        Ok(c) => c,
        Err(_) => return not_found(),
    };
    if !canonical.starts_with(&state.content_root) {
        info!(
            path = %rel.display(),
            "静态资源路径逃逸 content 根，404"
        );
        return not_found();
    }

    let bytes = match tokio::fs::read(&canonical).await {
        Ok(b) => b,
        Err(_) => return not_found(),
    };
    let mime = mime_for(rel.extension().and_then(|e| e.to_str()));
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(mime).expect("mime 可解析"),
    );
    // immutable + 1 年（内容不可变假设 + 文件名未带 hash，
    // 改名/删除场景由 404 兜底）
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=31536000, immutable"),
    );
    (StatusCode::OK, headers, bytes).into_response()
}

/// 自建扩展名 mime 表（避免引入新依赖；表外扩展名统一 octet-stream）。
fn mime_for(ext: Option<&str>) -> &'static str {
    match ext.map(str::to_ascii_lowercase).as_deref() {
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("svg") => "image/svg+xml",
        Some("webp") => "image/webp",
        Some("ico") => "image/x-icon",
        Some("css") => "text/css; charset=utf-8",
        Some("html") | Some("htm") => "text/html; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("json") => "application/json",
        Some("txt") | Some("md5") | Some("sql") => "text/plain; charset=utf-8",
        Some("pdf") => "application/pdf",
        Some("woff") => "font/woff",
        Some("woff2") => "font/woff2",
        Some("ttf") => "font/ttf",
        Some("mp4") => "video/mp4",
        Some("webm") => "video/webm",
        Some("zip") => "application/zip",
        Some("gz") => "application/gzip",
        Some("xml") => "application/xml",
        _ => "application/octet-stream",
    }
}

fn not_found() -> Response {
    (StatusCode::NOT_FOUND, "not found").into_response()
}
