//! coommunity v2.0 Axum HTTP & Admin API module.
//!
//! Security & Visibility Model:
//! - Local bearer token (`~/.coommunity/token`) authenticates BOTH CLI/MCP agents and the Human Admin Web UI.
//! - Admin endpoints (`/api/admin/*`) grant `Boss` full visibility across ALL forums, ALL DMs, and EVERY agent inbox.
//! - Standard agent endpoints (`/api/threads`, `/api/threads/:id/messages`) strictly isolate DMs so agents
//!   only see forums and their own DMs.
//! - Fixes v1.4 privacy leaks:
//!   1. `GET /api/threads?reader=...` now requires bearer auth.
//!   2. `GET /api/events` (SSE) now requires bearer auth (`Authorization: Bearer` or `?token=`) so live DM
//!      messages cannot leak to unauthenticated listeners.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, Request, StatusCode},
    middleware::Next,
    response::Response,
    routing::{get, patch, post, put},
    Json, Router,
};
use rusqlite::Connection;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::broadcast;

use crate::db::Database;
use crate::dispatcher::Dispatcher;
use crate::models::{HubEvent, Message, Thread, ThreadKind};
use crate::v2_db;
use crate::v2_models::{
    PostMessageV2Request, PostMessageV2Response, ThreadBriefRequest, TokenUpdateRequest,
    TokenUpdateResponse, UpdateAgentProfileRequest, UpdateMessageStatusRequest, BOSS_ADDRESS,
    DEFAULT_ARCHIVE_DAYS,
};

/// Shared lightweight handle for v2 routes.
#[derive(Clone)]
pub struct V2State {
    pub db: Database,
    pub conn: Arc<Mutex<Connection>>,
    pub dispatcher: Dispatcher,
    pub events: broadcast::Sender<HubEvent>,
    pub api_token: Arc<RwLock<String>>,
    pub token_path: Option<PathBuf>,
    pub default_max_hops: i64,
}

/// Hardened local auth middleware for v2.
/// Public unauthenticated allowlist is strictly limited to `/` (HTML shell) and `/api/health`.
/// All `/api/admin/*`, `/api/events`, and `reader=` queries require the local bearer token
/// (passed via `Authorization: Bearer <token>` header or `?token=<token>` for browser `EventSource`).
pub async fn require_local_auth_v2(
    State(st): State<V2State>,
    req: Request<axum::body::Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    let path = req.uri().path();
    let method = req.method();

    if method == axum::http::Method::OPTIONS || path == "/" || path == "/api/health" {
        return Ok(next.run(req).await);
    }

    let expected = st
        .api_token
        .read()
        .map(|g| g.clone())
        .unwrap_or_default();

    if extract_bearer_or_query_token(req.headers(), req.uri().query())
        .map(|t| !expected.is_empty() && t == expected)
        .unwrap_or(false)
    {
        return Ok(next.run(req).await);
    }

    Err(StatusCode::UNAUTHORIZED)
}

fn extract_bearer_or_query_token(headers: &HeaderMap, query: Option<&str>) -> Option<String> {
    if let Some(auth) = headers.get("authorization").and_then(|v| v.to_str().ok()) {
        if let Some(tok) = auth.strip_prefix("Bearer ").or_else(|| auth.strip_prefix("bearer ")) {
            return Some(tok.trim().to_string());
        }
    }
    if let Some(q) = query {
        for pair in q.split('&') {
            if let Some((k, v)) = pair.split_once('=') {
                if k == "token" && !v.is_empty() {
                    return Some(v.to_string());
                }
            }
        }
    }
    None
}

#[derive(Deserialize)]
pub struct AgentsQuery {
    #[serde(default)]
    pub include_archived: bool,
}

#[derive(Deserialize)]
pub struct SearchAgentsQuery {
    pub capability: Option<String>,
    pub runtime: Option<String>,
}

#[derive(Deserialize)]
pub struct ThreadsPageQuery {
    pub reader: Option<String>,
    pub kind: Option<String>,
    pub filter: Option<String>,
    pub sort: Option<String>,
    pub q: Option<String>,
    pub before_updated_at: Option<String>,
    pub limit: Option<usize>,
}

#[derive(Deserialize)]
pub struct MessagesPageQuery {
    pub reader: Option<String>,
    pub before_seq: Option<i64>,
    pub since_seq: Option<i64>,
    pub limit: Option<usize>,
}

#[derive(Deserialize)]
pub struct DeliveriesQuery {
    pub limit: Option<usize>,
}

#[derive(Deserialize)]
pub struct ArchiveStaleRequest {
    pub days: Option<i64>,
}

#[derive(Deserialize)]
pub struct ArchiveToggleRequest {
    pub archived: bool,
}

#[derive(Deserialize)]
pub struct BossDmRequest {
    pub to: String,
    pub body: String,
    #[serde(default)]
    pub msg_type: crate::v2_models::MessageType,
    #[serde(default)]
    pub refs: Vec<crate::v2_models::MessageRef>,
}

#[derive(Deserialize)]
pub struct AgentDmV2Request {
    pub from: String,
    pub to: String,
    pub body: String,
    #[serde(default)]
    pub ack_only: bool,
    #[serde(default)]
    pub msg_type: crate::v2_models::MessageType,
    #[serde(default)]
    pub refs: Vec<crate::v2_models::MessageRef>,
}

/// Mount all v2 Admin & Collaboration routes onto your existing Axum `Router`.
pub fn v2_router(state: V2State) -> Router {
    Router::new()
        // Admin God-Mode Visibility (Boss sees all forums, all DMs, and every agent inbox)
        .route("/api/admin/overview", get(admin_overview_handler))
        .route("/api/admin/deliveries", get(admin_deliveries_handler))
        .route("/api/admin/token", post(admin_update_token_handler))
        .route("/api/admin/agents", get(admin_list_agents_handler))
        .route("/api/admin/agents/archive-stale", post(admin_archive_stale_handler))
        .route("/api/admin/agents/:address/archive", post(admin_toggle_archive_handler))
        .route("/api/admin/agents/:address/inbox", get(admin_agent_inbox_handler))
        .route("/api/admin/threads", get(admin_list_threads_handler))
        .route("/api/admin/threads/:id/messages", get(admin_read_messages_handler))
        .route("/api/admin/threads/:id/subscriptions", get(admin_list_subscriptions_handler))
        .route("/api/admin/threads/:id/post", post(admin_post_as_boss_handler))
        .route("/api/admin/dm", post(admin_dm_as_boss_handler))
        // Shared Collaboration Endpoints (Agents + Boss)
        .route("/api/v2/threads", get(agent_list_threads_handler))
        .route("/api/v2/threads/:id/messages", get(agent_read_messages_handler).post(agent_post_message_handler))
        .route("/api/v2/dm", post(agent_dm_v2_handler))
        .route("/api/threads/:id/brief", put(set_thread_brief_handler))
        .route("/api/threads/:id/messages/:seq/status", patch(update_message_status_handler))
        .route("/api/agents/:address/profile", patch(update_agent_profile_handler))
        .route("/api/agents/search", get(search_agents_handler))
        .with_state(state)
}

/// Spawn the background 14-day ephemeral agent auto-archive sweeper.
/// Call once during daemon startup.
pub fn spawn_archive_sweeper(conn: Arc<Mutex<Connection>>) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(6 * 3600));
        loop {
            interval.tick().await;
            let c = conn.clone();
            let _ = tokio::task::spawn_blocking(move || {
                if let Ok(guard) = c.lock() {
                    let _ = v2_db::archive_stale_ephemeral_agents(&guard, Some(DEFAULT_ARCHIVE_DAYS));
                }
            })
            .await;
        }
    });
}

async fn admin_overview_handler(
    State(st): State<V2State>,
) -> Result<Json<Value>, StatusCode> {
    let conn = st.conn.clone();
    let res = tokio::task::spawn_blocking(move || {
        let guard = conn.lock().map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        v2_db::admin_overview(&guard).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
    })
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)??;
    Ok(Json(json!(res)))
}

async fn admin_deliveries_handler(
    State(st): State<V2State>,
    Query(q): Query<DeliveriesQuery>,
) -> Result<Json<Value>, StatusCode> {
    let conn = st.conn.clone();
    let limit = q.limit.unwrap_or(50);
    let res = tokio::task::spawn_blocking(move || {
        let guard = conn.lock().map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        v2_db::list_recent_deliveries_v2(&guard, limit).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
    })
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)??;
    Ok(Json(json!({ "deliveries": res })))
}

async fn admin_update_token_handler(
    State(st): State<V2State>,
    Json(req): Json<TokenUpdateRequest>,
) -> Result<Json<Value>, StatusCode> {
    let next_token = match req.new_token.map(|s| s.trim().to_string()) {
        Some(t) if t.len() >= 16 => t,
        Some(t) if !t.is_empty() => return Err(StatusCode::BAD_REQUEST),
        _ => format!("coom_{}", uuid::Uuid::new_v4().simple()),
    };

    if let Ok(mut guard) = st.api_token.write() {
        *guard = next_token.clone();
    } else {
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    }

    let persisted = if let Some(path) = &st.token_path {
        std::fs::write(path, format!("{}\n", next_token)).is_ok()
    } else {
        false
    };

    Ok(Json(json!(TokenUpdateResponse {
        token: next_token,
        persisted_to_disk: persisted,
    })))
}

async fn admin_list_agents_handler(
    State(st): State<V2State>,
    Query(q): Query<AgentsQuery>,
) -> Result<Json<Value>, StatusCode> {
    let conn = st.conn.clone();
    let res = tokio::task::spawn_blocking(move || {
        let guard = conn.lock().map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        v2_db::list_agents_v2(&guard, q.include_archived).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
    })
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)??;
    Ok(Json(json!({ "agents": res })))
}

async fn admin_agent_inbox_handler(
    State(st): State<V2State>,
    Path(address): Path<String>,
) -> Result<Json<Value>, StatusCode> {
    let conn = st.conn.clone();
    let res = tokio::task::spawn_blocking(move || {
        let guard = conn.lock().map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        v2_db::get_agent_inbox_v2(&guard, &address).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
    })
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)??;

    match res {
        Some(inbox) => Ok(Json(json!(inbox))),
        None => Err(StatusCode::NOT_FOUND),
    }
}

async fn admin_list_threads_handler(
    State(st): State<V2State>,
    Query(q): Query<ThreadsPageQuery>,
) -> Result<Json<Value>, StatusCode> {
    let conn = st.conn.clone();
    let res = tokio::task::spawn_blocking(move || {
        let guard = conn.lock().map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        v2_db::list_threads_paginated(
            &guard,
            Some(BOSS_ADDRESS),
            true, // Admin sees all forums + all DMs
            q.kind.as_deref(),
            q.filter.as_deref(),
            q.sort.as_deref(),
            q.q.as_deref(),
            q.before_updated_at.as_deref(),
            q.limit,
        )
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
    })
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)??;
    Ok(Json(json!(res)))
}

async fn agent_list_threads_handler(
    State(st): State<V2State>,
    Query(q): Query<ThreadsPageQuery>,
) -> Result<Json<Value>, StatusCode> {
    let conn = st.conn.clone();
    let res = tokio::task::spawn_blocking(move || {
        let guard = conn.lock().map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        v2_db::list_threads_paginated(
            &guard,
            q.reader.as_deref(),
            false, // Agents only see forums + their own DMs
            q.kind.as_deref(),
            q.filter.as_deref(),
            q.sort.as_deref(),
            q.q.as_deref(),
            q.before_updated_at.as_deref(),
            q.limit,
        )
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
    })
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)??;
    Ok(Json(json!(res)))
}

async fn admin_read_messages_handler(
    State(st): State<V2State>,
    Path(id): Path<String>,
    Query(q): Query<MessagesPageQuery>,
) -> Result<Json<Value>, StatusCode> {
    let conn = st.conn.clone();
    let res = tokio::task::spawn_blocking(move || {
        let guard = conn.lock().map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        v2_db::read_messages_paginated(
            &guard,
            &id,
            Some(BOSS_ADDRESS),
            true, // Admin can inspect any DM or forum
            q.before_seq,
            q.since_seq,
            q.limit,
        )
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
    })
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)??;

    match res {
        Ok(page) => Ok(Json(json!(page))),
        Err("thread_not_found") => Err(StatusCode::NOT_FOUND),
        Err(_) => Err(StatusCode::FORBIDDEN),
    }
}

async fn agent_read_messages_handler(
    State(st): State<V2State>,
    Path(id): Path<String>,
    Query(q): Query<MessagesPageQuery>,
) -> Result<Json<Value>, StatusCode> {
    let conn = st.conn.clone();
    let res = tokio::task::spawn_blocking(move || {
        let guard = conn.lock().map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        v2_db::read_messages_paginated(
            &guard,
            &id,
            q.reader.as_deref(),
            false, // Agent DM isolation strictly enforced
            q.before_seq,
            q.since_seq,
            q.limit,
        )
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
    })
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)??;

    match res {
        Ok(page) => Ok(Json(json!(page))),
        Err("thread_not_found") => Err(StatusCode::NOT_FOUND),
        Err(_) => Err(StatusCode::FORBIDDEN),
    }
}

async fn admin_list_subscriptions_handler(
    State(st): State<V2State>,
    Path(id): Path<String>,
) -> Result<Json<Value>, StatusCode> {
    let conn = st.conn.clone();
    let res = tokio::task::spawn_blocking(move || {
        let guard = conn.lock().map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        v2_db::list_subscriptions_v2(&guard, &id).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
    })
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)??;
    Ok(Json(json!({ "count": res.len(), "subscriptions": res })))
}

fn emit_v2_post(st: &V2State, resp: &PostMessageV2Response) {
    let msg = Message {
        id: resp.message.id.clone(),
        thread_id: resp.message.thread_id.clone(),
        seq: resp.message.seq,
        sender: resp.message.sender.clone(),
        body: resp.message.body.clone(),
        mentions: resp.message.mentions.clone(),
        ack_only: resp.message.ack_only,
        created_at: resp.message.created_at.clone(),
    };
    let thread = Thread {
        id: resp.thread.id.clone(),
        kind: ThreadKind::from_str(&resp.thread.kind),
        title: resp.thread.title.clone(),
        description: Some(resp.thread.description.clone()).filter(|s| !s.is_empty()),
        created_by: resp.thread.created_by.clone(),
        created_at: resp.thread.created_at.clone(),
        updated_at: resp.thread.updated_at.clone(),
        hop_count: resp.thread.hop_count,
        max_hops: resp.thread.max_hops,
        muted: resp.thread.muted,
        message_count: resp.thread.message_count as i64,
    };
    let _ = st.events.send(HubEvent::MessagePosted {
        message: msg.clone(),
        thread: thread.clone(),
    });
    st.dispatcher.dispatch_message(thread, msg);
}

async fn agent_post_message_handler(
    State(st): State<V2State>,
    Path(id): Path<String>,
    Json(req): Json<PostMessageV2Request>,
) -> Result<Json<Value>, StatusCode> {
    let conn = st.conn.clone();
    let res = tokio::task::spawn_blocking(move || {
        let guard = conn.lock().map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        v2_db::post_message_v2(&guard, &id, &req).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
    })
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)??;

    match res {
        Ok(resp) => {
            emit_v2_post(&st, &resp);
            Ok(Json(json!(resp)))
        }
        Err("thread_not_found") => Err(StatusCode::NOT_FOUND),
        Err(_) => Err(StatusCode::FORBIDDEN),
    }
}

async fn admin_post_as_boss_handler(
    State(st): State<V2State>,
    Path(id): Path<String>,
    Json(mut req): Json<PostMessageV2Request>,
) -> Result<Json<Value>, StatusCode> {
    req.sender = BOSS_ADDRESS.to_string();
    agent_post_message_handler(State(st), Path(id), Json(req)).await
}

async fn create_and_post_dm_v2(
    st: &V2State,
    from: String,
    to: String,
    body: String,
    ack_only: bool,
    msg_type: crate::v2_models::MessageType,
    refs: Vec<crate::v2_models::MessageRef>,
) -> Result<Json<Value>, StatusCode> {
    let db = st.db.clone();
    let conn = st.conn.clone();
    let max_hops = st.default_max_hops;
    let res = tokio::task::spawn_blocking(move || {
        let target = {
            let guard = conn.lock().map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
            v2_db::resolve_agent_alias_v2(&guard, &to)
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
                .unwrap_or(to)
        };
        let thread = db
            .get_or_create_dm_thread(&from, &target, max_hops)
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        let guard = conn.lock().map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        let post_req = PostMessageV2Request {
            sender: from,
            body,
            mentions: vec![target],
            ack_only,
            msg_type,
            status: None,
            reply_to_seq: None,
            refs,
        };
        v2_db::post_message_v2(&guard, &thread.id, &post_req)
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
    })
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)??;

    match res {
        Ok(resp) => {
            emit_v2_post(st, &resp);
            Ok(Json(json!(resp)))
        }
        Err(_) => Err(StatusCode::BAD_REQUEST),
    }
}

async fn agent_dm_v2_handler(
    State(st): State<V2State>,
    Json(req): Json<AgentDmV2Request>,
) -> Result<Json<Value>, StatusCode> {
    create_and_post_dm_v2(
        &st,
        req.from,
        req.to,
        req.body,
        req.ack_only,
        req.msg_type,
        req.refs,
    )
    .await
}

async fn admin_dm_as_boss_handler(
    State(st): State<V2State>,
    Json(req): Json<BossDmRequest>,
) -> Result<Json<Value>, StatusCode> {
    create_and_post_dm_v2(
        &st,
        BOSS_ADDRESS.to_string(),
        req.to,
        req.body,
        false,
        req.msg_type,
        req.refs,
    )
    .await
}

async fn set_thread_brief_handler(
    State(st): State<V2State>,
    Path(id): Path<String>,
    Json(req): Json<ThreadBriefRequest>,
) -> Result<Json<Value>, StatusCode> {
    let conn = st.conn.clone();
    let res = tokio::task::spawn_blocking(move || {
        let guard = conn.lock().map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        v2_db::set_thread_brief(&guard, &id, &req.brief, &req.updated_by)
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
    })
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)??;

    match res {
        Some(thread) => Ok(Json(json!({ "thread": thread }))),
        None => Err(StatusCode::NOT_FOUND),
    }
}

async fn update_message_status_handler(
    State(st): State<V2State>,
    Path((id, seq)): Path<(String, i64)>,
    Json(req): Json<UpdateMessageStatusRequest>,
) -> Result<Json<Value>, StatusCode> {
    let conn = st.conn.clone();
    let tid = id.clone();
    let status = req.status.clone();
    let ok = tokio::task::spawn_blocking(move || {
        let guard = conn.lock().map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        v2_db::update_message_status(&guard, &tid, seq, &status)
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
    })
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)??;

    if ok {
        Ok(Json(json!({ "updated": true, "thread_id": id, "seq": seq, "status": req.status })))
    } else {
        Err(StatusCode::NOT_FOUND)
    }
}

async fn update_agent_profile_handler(
    State(st): State<V2State>,
    Path(address): Path<String>,
    Json(req): Json<UpdateAgentProfileRequest>,
) -> Result<Json<Value>, StatusCode> {
    let conn = st.conn.clone();
    let _ = tokio::task::spawn_blocking(move || {
        let guard = conn.lock().map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        v2_db::update_agent_profile(&guard, &address, &req)
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
    })
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)??;
    Ok(Json(json!({ "updated": true })))
}

async fn search_agents_handler(
    State(st): State<V2State>,
    Query(q): Query<SearchAgentsQuery>,
) -> Result<Json<Value>, StatusCode> {
    let conn = st.conn.clone();
    let res = tokio::task::spawn_blocking(move || {
        let guard = conn.lock().map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        v2_db::find_agents_by_capability(&guard, q.capability.as_deref(), q.runtime.as_deref())
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
    })
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)??;
    Ok(Json(json!({ "agents": res })))
}

async fn admin_archive_stale_handler(
    State(st): State<V2State>,
    Json(req): Json<ArchiveStaleRequest>,
) -> Result<Json<Value>, StatusCode> {
    let conn = st.conn.clone();
    let report = tokio::task::spawn_blocking(move || {
        let guard = conn.lock().map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        v2_db::archive_stale_ephemeral_agents(&guard, req.days)
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
    })
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)??;
    Ok(Json(json!(report)))
}

async fn admin_toggle_archive_handler(
    State(st): State<V2State>,
    Path(address): Path<String>,
    Json(req): Json<ArchiveToggleRequest>,
) -> Result<Json<Value>, StatusCode> {
    let conn = st.conn.clone();
    let ok = tokio::task::spawn_blocking(move || {
        let guard = conn.lock().map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        v2_db::set_agent_archived(&guard, &address, req.archived)
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
    })
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)??;
    Ok(Json(json!({ "updated": ok })))
}
