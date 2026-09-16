//! 错误页：统一 askama 模板，含返回首页链接；
//! 不泄露内部错误细节（日志里有）。

use crate::routes::embedded_assets::fingerprinted_url;
use crate::state::AppState;
use crate::templates::{ErrorTpl, site_title};
use askama::Template as _;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use std::sync::Arc;

/// 渲染 404/500 统一错误页。
pub fn error_page(state: &Arc<AppState>, code: u16, message: &str) -> Response {
    let index = state.snapshot();
    let tpl = ErrorTpl {
        site_title: site_title(&index),
        initial_tree_json: "[]".to_string(), // 错误页最小树（避免错误页也触发树构建）
        has_icons: false,                    // 无树数据可判断，保守不输出外链 script
        asset_css: fingerprinted_url("app.css"),
        asset_js: fingerprinted_url("app.js"),
        has_sidebar: false,
        top_nav: Vec::new(), // 错误页顶栏仅站点标题（上下文不明，不猜一级菜单）
        current_default_url: String::new(),
        current_dir_hrefs: Vec::new(),
        project_icon: crate::templates::project_icon(&index).unwrap_or_default(),
        footer_text: crate::templates::footer_text(&index, state.cfg.server.footer.as_deref()),
        has_mermaid: false,
        has_katex: false,
        mermaid_cdn: state.cfg.render.mermaid_cdn.clone(),
        katex_cdn: state.cfg.render.katex_cdn.clone(),
        katex_cdn_css: crate::templates::katex_css_url(&state.cfg.render.katex_cdn),
        // 错误页搜索框保持占位（页面上下文已失败，不再触发查询）
        search_enabled: false,
        code,
        message: message.to_string(),
    };
    let status = StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let html = tpl.render().unwrap_or_else(|e| {
        // 模板自身失败退化为最简 HTML（不能因模板错误丢错误语义）
        tracing::error!(%e, code, "错误页模板渲染失败");
        format!("<html><body><h1>{code}</h1><p><a href=\"/\">返回首页</a></p></body></html>")
    });
    let mut resp = (status, axum::response::Html(html)).into_response();
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    resp
}
