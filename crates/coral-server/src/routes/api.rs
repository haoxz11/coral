//! API 路由：/api/tree/children 与探针。

use crate::state::AppState;
use axum::extract::{ConnectInfo, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use coral_core::tree::build_subtree;
use coral_core::url as core_url;
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;
use std::path::{Path as FsPath, PathBuf};
use std::sync::Arc;
use tracing::info;

#[derive(Deserialize)]
pub struct ChildrenQuery {
    path: Option<String>,
}

/// `GET /favicon.ico`：重定向到根 `_index.md` icon 的
/// Iconify SVG（防浏览器自动请求 404）；icon 是图片 URL 时直接重定向该 URL；
/// 无 icon 返回 204（浏览器用默认）。
pub async fn favicon(State(state): State<Arc<AppState>>) -> Response {
    let index = state.snapshot();
    let icon = index
        .dirs
        .get(std::path::Path::new(""))
        .and_then(|d| d.branch_page.as_ref())
        .and_then(|bp| index.pages.get(bp))
        .and_then(|p| p.fm.icon.as_deref());
    let location = match icon {
        Some(ic) if ic.contains(':') => format!("https://api.iconify.design/{ic}.svg"),
        Some(ic) if ic.starts_with('/') => ic.to_string(),
        _ => return StatusCode::NO_CONTENT.into_response(),
    };
    (
        StatusCode::FOUND,
        [(
            header::LOCATION,
            HeaderValue::from_str(&location).expect("favicon url 可解析"),
        )],
    )
        .into_response()
}

/// `GET /api/tree/children?path=<url-encoded 路径>`。
///
/// 返回 `{"path": ..., "children": [TreeNode...]}`（expand_depth+1 层）；
/// ETag = 树 JSON 内容 xxh3（A3）；目录 mtime 变则重建（树查目录自身）。
pub async fn tree_children(
    State(state): State<Arc<AppState>>,
    Query(q): Query<ChildrenQuery>,
    headers: HeaderMap,
) -> Response {
    // decode → 规范化路径段（path 参数是目录的 decode 形态 URL，如 /guide）
    let raw = q.path.unwrap_or_else(|| "/".to_string());
    let decoded = match core_url::decode_url(&raw) {
        Ok(d) => d,
        Err(_) => return (StatusCode::NOT_FOUND, "bad path").into_response(),
    };

    // URL 反查目录 rel_path（含 permalink 反查——树节点 URL 可能是
    // branch 页 permalink，不能按路径段直映磁盘；路径安全：decode →
    // 反查 → 结果必在 dirs 表内）
    let index = state.snapshot();
    let dir_rel: PathBuf = match index.dir_for_url(&decoded) {
        Some(d) => d,
        None => return (StatusCode::NOT_FOUND, "no such dir").into_response(),
    };

    // 树缓存：stat 目录 mtime（树查目录自身），未变用缓存
    let dir_mtime = {
        let abs = state.content_root.join(&dir_rel);
        match tokio::fs::metadata(&abs).await {
            Ok(m) => m.modified().ok(),
            Err(_) => return (StatusCode::NOT_FOUND, "dir unreadable").into_response(),
        }
    };

    let dir_key = dir_rel.to_string_lossy().into_owned();
    let cached = state.cache.lookup_tree(&dir_key);
    let json = match (cached, dir_mtime) {
        (Some(c), Some(mtime)) if !tree_stale(&state, &dir_key, mtime) => c,
        _ => {
            // 重建：build_subtree(expand_depth + 1)
            let nodes = build_subtree(
                &index,
                &dir_rel,
                state.cfg.tree.expand_depth + 1,
                state.cfg.content.draft,
            );
            let json = json!({"path": decoded, "children": nodes}).to_string();
            if let Some(mtime) = dir_mtime {
                let _ = state.cache.store_tree(&dir_key, mtime, &json).map_err(|e| {
                    info!(%e, "树缓存写入失败，服务继续");
                    e
                });
            }
            json
        }
    };
    // ETag/304（A3：内容寻址，树变才变）
    let etag = format!("\"{}\"", xxhash_rust::xxh3::xxh3_64(json.as_bytes()));
    if let Some(inm) = headers
        .get(header::IF_NONE_MATCH)
        .filter(|inm| inm.as_bytes() == etag.as_bytes())
    {
        let _ = inm;
        return (
            StatusCode::NOT_MODIFIED,
            [(
                header::ETAG,
                HeaderValue::from_str(&etag).expect("etag 可解析"),
            )],
        )
            .into_response();
    }
    (
        StatusCode::OK,
        [
            (
                header::ETAG,
                HeaderValue::from_str(&etag).expect("etag 可解析"),
            ),
            // 无 Cache-Control 时浏览器启发式缓存会绕过 ETag 验证直接用旧
            // JSON（树排序变化后用户看不到新序，M2 实际踩坑）；no-cache 强制
            // 每次协商——内容未变仍 304 省流量
            (header::CACHE_CONTROL, HeaderValue::from_static("no-cache")),
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            ),
        ],
        json,
    )
        .into_response()
}

/// 树条目是否过期（树查目录自身 mtime）。
/// 内存条目记录的 mtime 与请求时 stat 到的目录 mtime 不一致 → stale 重建。
fn tree_stale(state: &Arc<AppState>, dir_key: &str, current_mtime: std::time::SystemTime) -> bool {
    match state.cache.lookup_tree_mtime_ms(dir_key) {
        None => true,
        Some(recorded_ms) => {
            let current_ms = current_mtime
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            recorded_ms != current_ms
        }
    }
}

/// `GET /healthz`：只反映进程存活。
pub async fn healthz() -> Response {
    (StatusCode::OK, "ok").into_response()
}

/// `GET /readyz`：索引就绪。
pub async fn readyz(State(state): State<Arc<AppState>>) -> Response {
    if state.is_ready() {
        (StatusCode::OK, "ready").into_response()
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "not ready").into_response()
    }
}

#[allow(dead_code)]
fn unused(_: &HashMap<String, String>, _: &FsPath) {}

#[derive(Deserialize)]
pub struct SearchQuery {
    q: String,
    #[serde(default = "default_search_limit")]
    limit: usize,
}

fn default_search_limit() -> usize {
    20
}

/// `GET /api/search?q=&limit=`。
///
/// - 未开启：`{"enabled": false, "hits": []}`（200，不 503）
/// - 构建中：`{"enabled": true, "building": true, "hits": []}`
/// - 就绪：`{"enabled": true, "hits": [{title,url,snippet,dir_path,date}], "tokens": [...]}`
///
/// q 只进 tantivy QueryParser（无路径拼接面）。解析失败（输入中间态，如
/// 悬空 AND/括号）是预期情况：DEBUG + 空结果，不告警；IO 错误仍 WARN。
pub async fn search(State(state): State<Arc<AppState>>, Query(q): Query<SearchQuery>) -> Response {
    if !state.search.is_enabled() {
        return (
            StatusCode::OK,
            axum::Json(json!({"enabled": false, "hits": []})),
        )
            .into_response();
    }
    let Some(si) = state.search.ready_index() else {
        return (
            StatusCode::OK,
            axum::Json(json!({"enabled": true, "building": true, "hits": []})),
        )
            .into_response();
    };
    let q_raw = q.q.clone();
    let limit = q.limit.clamp(1, 50);
    let result = tokio::task::spawn_blocking(move || si.search(&q_raw, limit)).await;
    let (hits, tokens) = match result {
        Ok(Ok(v)) => v,
        Ok(Err(coral_core::search::SearchError::Query(e))) => {
            // 输入中间态（debounce 期间的半截查询），预期情况不告警
            tracing::debug!(%e, "搜索查询未完成解析（输入中间态），返回空结果");
            (Vec::new(), Vec::new())
        }
        Ok(Err(e)) => {
            tracing::warn!(%e, "搜索查询失败，返回空结果");
            (Vec::new(), Vec::new())
        }
        Err(e) => {
            tracing::warn!(%e, "搜索查询 join 失败");
            (Vec::new(), Vec::new())
        }
    };
    (
        StatusCode::OK,
        axum::Json(json!({"enabled": true, "hits": hits, "tokens": tokens})),
    )
        .into_response()
}

/// reindex detached 任务与请求端之间的结果传递容器。
type ReindexResultCell = std::sync::Arc<
    std::sync::Mutex<
        Option<Result<(coral_core::search::ReindexStats, std::time::Duration), String>>,
    >,
>;

/// `GET /search/reindex`（运维后门）：全量重建搜索索引，阻塞至完成
/// 返回重建总结（indexed_docs/segments/index_bytes/elapsed_ms）。
///
/// 安全：限本机来源（loopback）；设置了 CORAL_ADMIN_TOKEN
/// 环境变量时额外校验 `Authorization: Bearer <token>`（未设则仅靠本机限制）。
/// 语义：重建任务 detached 执行——**请求方断开不影响重建**，只丢响应；
/// 重建中再请求立刻 503（不排队）；重建期间旧索引继续服务查询（tantivy
/// reader 快照天然隔离）。未开启搜索时 404（不暴露端点存在性）。
pub async fn reindex(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    ConnectInfo(addr): ConnectInfo<std::net::SocketAddr>,
) -> Response {
    if !state.search.is_enabled() {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    // 本机限制：仅 loopback 来源
    if !addr.ip().is_loopback() {
        return (StatusCode::FORBIDDEN, "forbidden").into_response();
    }
    // 可选令牌：设了 CORAL_ADMIN_TOKEN 则必须匹配 Bearer
    if let Ok(expect) = std::env::var("CORAL_ADMIN_TOKEN") {
        let ok = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .is_some_and(|t| t == expect);
        if !ok {
            return (StatusCode::FORBIDDEN, "forbidden").into_response();
        }
    }
    // 并发去重：重建中立刻失败
    if !state.search.try_begin_reindex() {
        return (StatusCode::SERVICE_UNAVAILABLE, "reindex in progress").into_response();
    }

    // 复用当前就绪实例执行 build_full：tantivy 目录锁单写者——新开实例
    // 必然 LockBusy；同一 writer 的 delete_all+重插对旧 reader 快照不可见
    // （写新读旧），commit 后 reader reload 即新数据。
    // 无就绪实例（启动构建中/构建失败）时给出明确提示。
    let Some(si) = state.search.ready_index() else {
        state.search.end_reindex();
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "search index not ready (building or failed); retry after startup build completes",
        )
            .into_response();
    };

    // detached 重建任务：不随请求 future 生命周期（断开只丢响应）。
    // 结果经共享 cell 传递给等待中的请求端
    let result_cell: ReindexResultCell = Arc::new(std::sync::Mutex::new(None));
    {
        let state = state.clone();
        let cell = result_cell.clone();
        let snapshot = state.snapshot();
        let root = state.content_root.clone();
        let draft_enabled = state.cfg.content.draft;
        tokio::task::spawn(async move {
            let start = std::time::Instant::now();
            let outcome =
                tokio::task::spawn_blocking(move || si.build_full(&snapshot, &root, draft_enabled))
                    .await
                    .map_err(|e| e.to_string())
                    .and_then(|r| r.map_err(|e| e.to_string()));
            // 记录结果（含耗时）→ 释放重建标记 → 唤醒等待者
            let payload = match outcome {
                Ok(stats) => Ok(coral_core::search::ReindexStats {
                    indexed_docs: stats.indexed_docs,
                    segments: stats.segments,
                    index_bytes: stats.index_bytes,
                }),
                Err(e) => Err(e),
            };
            let elapsed = start.elapsed();
            *cell.lock().expect("reindex 结果锁中毒") = Some(payload.map(|s| (s, elapsed)));
            state.search.end_reindex();
            state.search.notify_reindex_done();
        });
    }

    // 请求端等待完成（断开 → future drop → 任务照跑）
    state.search.wait_reindex_done().await;
    let payload = result_cell
        .lock()
        .expect("reindex 结果锁中毒")
        .take()
        .unwrap_or_else(|| Err("reindex 结果缺失".to_string()));

    match payload {
        Ok((stats, elapsed)) => {
            info!(
                docs = stats.indexed_docs,
                segments = stats.segments,
                index_bytes = stats.index_bytes,
                elapsed_ms = elapsed.as_millis() as u64,
                "手动 reindex 完成"
            );
            (
                StatusCode::OK,
                axum::Json(json!({
                    "ok": true,
                    "indexed_docs": stats.indexed_docs,
                    "segments": stats.segments,
                    "index_bytes": stats.index_bytes,
                    "elapsed_ms": elapsed.as_millis() as u64,
                })),
            )
                .into_response()
        }
        Err(e) => {
            tracing::error!(%e, "手动 reindex 失败（旧索引继续服务）");
            (StatusCode::INTERNAL_SERVER_ERROR, "reindex failed").into_response()
        }
    }
}
