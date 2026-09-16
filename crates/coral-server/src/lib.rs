//! coral 应用层：axum 路由、模板、watcher、singleflight。
//!
//! HTTP 语义（状态码、header、缓存控制）只出现在本层（AGENTS.md 分层准则）。

pub mod errorpage;
pub mod git_sync;
pub mod routes;
pub mod singleflight;
pub mod state;
pub mod templates;
pub mod watcher;

use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::get;
use std::sync::Arc;

/// 组装 Router。
///
/// 具体路径优先，`/{*path}` 通配兜底：页面命中（含 permalink）返回 HTML；
/// 未命中且文件存在 → 静态资源回退；都未命中 → 404。
pub fn build_app(state: Arc<state::AppState>) -> Router {
    Router::new()
        .route("/healthz", get(routes::api::healthz))
        .route("/readyz", get(routes::api::readyz))
        .route("/api/tree/children", get(routes::api::tree_children))
        .route("/api/search", get(routes::api::search))
        .route("/search", get(routes::pages::search_page))
        .route("/search/reindex", get(routes::api::reindex))
        .route(
            "/git/webhook",
            axum::routing::post(routes::git_webhook::webhook),
        )
        .route(
            "/assets/{*path}",
            get(routes::embedded_assets::asset_handler),
        )
        .route("/favicon.ico", get(routes::api::favicon))
        .route("/", get(routes::pages::root_handler))
        .route("/{*path}", get(fallback_path))
        .with_state(state)
}

/// 页面 → 静态资源回退分发。两级都未命中时返回统一 404 错误页。
async fn fallback_path(state: State<Arc<state::AppState>>, path: Path<String>) -> Response {
    let resp = routes::pages::page_handler(State(state.0.clone()), Path(path.0.clone())).await;
    if resp.status() != StatusCode::NOT_FOUND {
        return resp;
    }
    let asset_resp =
        routes::assets::content_asset_handler(State(state.0.clone()), Path(path.0)).await;
    if asset_resp.status() != StatusCode::NOT_FOUND {
        return asset_resp;
    }
    errorpage::error_page(&state.0, 404, "页面不存在")
}

pub use state::{AppState, initialize};
