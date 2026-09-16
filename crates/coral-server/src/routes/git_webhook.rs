//! `POST /git/webhook`：接收 GitLab push 事件触发同步。
//!
//! 安全：X-Gitlab-Token 比对（未配 secret_token 时仅 loopback）；
//! 只处理 push + ref 匹配 refs/heads/<branch>；checkout_sha null（分支删除）
//! 忽略；未开启 404。响应自适应：预计 <10s 阻塞返回总结，≥10s 返回 202。

use crate::git_sync::run_sync;
use crate::state::AppState;
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use coral_core::git_sync::GitSync;
use serde::Deserialize;
use std::sync::Arc;
use tracing::info;

#[derive(Deserialize)]
pub struct GitlabPush {
    #[serde(default)]
    object_kind: String,
    #[serde(default)]
    r#ref: String,
    /// 分支删除时为 null
    #[serde(default)]
    checkout_sha: Option<String>,
}

/// 自适应阈值（GitLab webhook 超时约 10s；超过则转 202 后台）。
const BLOCKING_BUDGET_MS: u64 = 10_000;

pub async fn webhook(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    ConnectInfo(addr): ConnectInfo<std::net::SocketAddr>,
    axum::Json(payload): axum::Json<GitlabPush>,
) -> Response {
    if !state.cfg.git.enabled {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    // 安全：secret 比对（未配置时仅 loopback）
    let secret = state.cfg.git.secret_token.trim();
    if secret.is_empty() {
        if !addr.ip().is_loopback() {
            return (StatusCode::FORBIDDEN, "forbidden").into_response();
        }
    } else {
        let ok = headers
            .get("x-gitlab-token")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|t| t == secret);
        if !ok {
            return (StatusCode::FORBIDDEN, "forbidden").into_response();
        }
    }

    // 事件过滤：非 push 忽略；分支不匹配忽略；分支删除忽略
    if payload.object_kind != "push" {
        return (StatusCode::OK, "ignored: not a push event").into_response();
    }
    let expect_ref = format!("refs/heads/{}", state.cfg.git.branch);
    if payload.r#ref != expect_ref {
        return (StatusCode::OK, "ignored: branch mismatch").into_response();
    }
    if payload.checkout_sha.is_none() {
        return (StatusCode::OK, "ignored: branch deleted").into_response();
    }

    // 收到即记录：同步可能耗时较长（大仓库 fetch），无此日志时
    // webhook 处理期间静默，观测上像"请求丢了"
    info!(
        r#ref = %payload.r#ref,
        checkout_sha = payload.checkout_sha.as_deref().unwrap_or_default(),
        from = %addr,
        "收到 git webhook，开始同步"
    );

    // 并发去重（启动 clone 共用；GitLab 自带重试）
    if !state.git.try_begin() {
        return with_retry_after(StatusCode::SERVICE_UNAVAILABLE, "sync in progress");
    }

    // 自适应：按上次耗时估计（estimate==0 首次 < 阈值，阻塞给出结果）
    let estimate = state.git.last_sync_ms();
    let sync = Arc::new(GitSync::new(
        state.cfg.git.clone(),
        state.cfg.cache.dir.join("git-mirror"),
        state.content_root.clone(),
    ));
    let state2 = state.clone();
    let start = std::time::Instant::now();
    let handle = tokio::spawn(async move {
        let result = run_sync(&state2, &sync).await;
        let elapsed = start.elapsed().as_millis() as u64;
        state2.git.end(elapsed);
        (result, elapsed)
    });

    if estimate >= BLOCKING_BUDGET_MS {
        // 预计慢：立刻 202，后台继续（detached——断开不影响）
        return with_retry_after(
            StatusCode::ACCEPTED,
            "sync running in background (slow repository); see logs",
        );
    }
    // 阻塞等结果（请求方断开 → future drop → 任务照跑完，标记照常复位）
    let (result, elapsed) = handle.await.unwrap_or_else(|e| {
        let msg = e.to_string();
        (Err(msg), 0)
    });
    match result {
        Ok(stats) => {
            let _ = elapsed; // run_sync 已打日志
            (
                StatusCode::OK,
                axum::Json(serde_json::json!({
                    "ok": true,
                    "changed": stats.changed.len(),
                    "removed": stats.removed.len(),
                    "elapsed_ms": elapsed,
                })),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("sync failed: {e}"),
        )
            .into_response(),
    }
}

fn with_retry_after(code: StatusCode, msg: &'static str) -> Response {
    let mut resp = (code, msg).into_response();
    resp.headers_mut()
        .insert(header::RETRY_AFTER, header::HeaderValue::from_static("30"));
    resp
}
