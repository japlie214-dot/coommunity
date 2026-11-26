use anyhow::{anyhow, Result};
use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderValue, Method, StatusCode},
    middleware,
    response::{
        sse::{Event, KeepAlive, Sse},
        Html, IntoResponse,
    },
    routing::{delete, get, post},
    Json, Router,
};
use futures_util::stream::Stream;
use serde::Deserialize;
use serde_json::json;
use std::convert::Infallible;
use std::time::Duration;
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt as _;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tracing::info;

use crate::config::Config;
use crate::db::Database;
use crate::dispatcher::Dispatcher;
use crate::models::{
    AckInboxRequest, AgentStatus, CreateThreadRequest, HubEvent, PostMessageRequest,
    RegisterAgentRequest, SendDmRequest, SnapshotResponse, SubscribeRequest, SubscriptionMode,
    ThreadKind,
};
use crate::web_ui;

#[derive(Clone)]
pub struct AppState {
    pub cfg: Config,
    pub db: Database,
    pub dispatcher: Dispatcher,
    pub events: broadcast::Sender<HubEvent>,
    pub api_token: String,
}

async fn run_db<F, T>(db: Database, f: F) -> Result<T>
where
    F: FnOnce(&Database) -> Result<T> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(move || f(&db))
        .await
        .map_err(|e| anyhow!("database worker task failed: {e}"))?
}

pub async fn run_daemon(cfg: Config) -> Result<()> {
    let api_token = cfg.load_or_create_token()?;
    let db = Database::open(&cfg.db_path)?;
    let (events, _) = broadcast::channel::<HubEvent>(512);
    let dispatcher = Dispatcher::new(db.clone(), cfg.clone(), events.clone());

    let state = AppState {
        cfg: cfg.clone(),
        db,
        dispatcher,
        events,
        api_token,
    };

    let allowed_origins = [
        HeaderValue::from_str(&format!("http://127.0.0.1:{}", cfg.port))?,
        HeaderValue::from_str(&format!("http://localhost:{}", cfg.port))?,
    ];
    let cors = CorsLayer::new()
        .allow_origin(AllowOrigin::list(allowed_origins))
        .allow_methods([
            Method::GET,
            Method::POST,
            Method::PUT,
            Method::PATCH,
            Method::DELETE,
            Method::OPTIONS,
        ])
        .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE]);

    let v2_state = crate::v2_daemon::V2State {
        db: state.db.clone(),
        conn: state.db.conn_arc(),
        dispatcher: state.dispatcher.clone(),
        events: state.events.clone(),
        api_token: std::sync::Arc::new(std::sync::RwLock::new(state.api_token.clone())),
        token_path: Some(state.cfg.token_path.clone()),
        default_max_hops: state.cfg.default_max_hops,
    };
    crate::v2_daemon::spawn_archive_sweeper(v2_state.conn.clone());

    let app = Router::new()
        // Read-only local web viewer
        .route("/", get(index_html))
        .route("/api/health", get(health))
        .route("/api/snapshot", get(snapshot))
        .route("/api/events", get(sse_events))
        // Agents & Inbox
        .route("/api/agents", get(list_agents).post(register_agent))
        .route("/api/agents/:address/status", post(set_agent_status))
        .route("/api/agents/:address/inbox", get(check_inbox))
        .route("/api/agents/:address/inbox/ack-all", post(ack_all_inbox))
        .route("/api/inbox/ack", post(ack_inbox))
        // Threads & Messages
        .route("/api/threads", get(list_threads).post(create_thread))
        .route("/api/threads/:id/messages", get(read_messages).post(post_message))
        .route("/api/threads/:id/subscriptions", get(list_thread_subs).post(subscribe))
        .route(
            "/api/threads/:id/subscriptions/:agent",
            delete(unsubscribe),
        )
        .route("/api/threads/:id/mute", post(mute_thread))
        .route("/api/threads/:id/unmute", post(unmute_thread))
        .route("/api/threads/:id/reset-hops", post(reset_hops))
        // Direct Messages
        .route("/api/dm", post(send_dm))
        // Global Controls
        .route("/api/hub/pause", post(pause_hub))
        .route("/api/hub/resume", post(resume_hub))
        .with_state(state)
        .merge(crate::v2_daemon::v2_router(v2_state.clone()))
        .layer(middleware::from_fn_with_state(
            v2_state,
            crate::v2_daemon::require_local_auth_v2,
        ))
        .layer(cors);

    let addr = format!("{}:{}", cfg.host, cfg.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    info!(
        url = %cfg.base_url(),
        db = %cfg.db_path.display(),
        token = %cfg.token_path.display(),
        "coommunityd listening (read-only viewer at /)"
    );
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    info!("coom daemon shut down cleanly");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}

async fn index_html() -> Html<&'static str> {
    Html(web_ui::INDEX_HTML)
}

async fn health(State(st): State<AppState>) -> impl IntoResponse {
    match run_db(st.db.clone(), |db| db.hub_status()).await {
        Ok(s) => (StatusCode::OK, Json(s)).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn snapshot(State(st): State<AppState>) -> impl IntoResponse {
    let res = run_db(st.db.clone(), |db| {
        Ok(SnapshotResponse {
            status: db.hub_status()?,
            agents: db.list_agents()?,
            threads: db.list_threads_for_agent(None)?,
            subscriptions: db.list_subscriptions(None)?,
            recent_deliveries: db.recent_deliveries(40)?,
        })
    })
    .await;
    match res {
        Ok(snap) => (StatusCode::OK, Json(snap)).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn sse_events(
    State(st): State<AppState>,
) -> Sse<impl Stream<Item = std::result::Result<Event, Infallible>>> {
    let rx = st.events.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(|msg| match msg {
        Ok(ev) => {
            let data = serde_json::to_string(&ev).unwrap_or_else(|_| "{}".to_string());
            Some(Ok(Event::default().data(data)))
        }
        Err(_) => None,
    });
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

async fn list_agents(State(st): State<AppState>) -> impl IntoResponse {
    match run_db(st.db.clone(), |db| db.list_agents()).await {
        Ok(agents) => (StatusCode::OK, Json(agents)).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn register_agent(
    State(st): State<AppState>,
    Json(req): Json<RegisterAgentRequest>,
) -> impl IntoResponse {
    match run_db(st.db.clone(), move |db| db.upsert_agent(&req)).await {
        Ok(agent) => {
            let _ = st.events.send(HubEvent::AgentUpdated {
                agent: agent.clone(),
            });
            (StatusCode::OK, Json(agent)).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
struct StatusBody {
    status: String,
    session_id: Option<String>,
}

async fn set_agent_status(
    State(st): State<AppState>,
    Path(address): Path<String>,
    Json(body): Json<StatusBody>,
) -> impl IntoResponse {
    let status = AgentStatus::from_str(&body.status);
    let session_id = body.session_id;
    match run_db(st.db.clone(), move |db| {
        db.set_agent_session_status(&address, session_id.as_deref(), status)
    })
    .await
    {
        Ok(Some(agent)) => {
            let _ = st.events.send(HubEvent::AgentUpdated {
                agent: agent.clone(),
            });
            (StatusCode::OK, Json(agent)).into_response()
        }
        Ok(None) => (StatusCode::NOT_FOUND, "agent not found").into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn check_inbox(
    State(st): State<AppState>,
    Path(address): Path<String>,
) -> impl IntoResponse {
    match run_db(st.db.clone(), move |db| db.check_inbox(&address)).await {
        Ok(items) => (StatusCode::OK, Json(items)).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn ack_inbox(
    State(st): State<AppState>,
    Json(req): Json<AckInboxRequest>,
) -> impl IntoResponse {
    match run_db(st.db.clone(), move |db| {
        db.ack_inbox(&req.agent_address, &req.thread_id, req.up_to_seq)
    })
    .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn ack_all_inbox(
    State(st): State<AppState>,
    Path(address): Path<String>,
) -> impl IntoResponse {
    match run_db(st.db.clone(), move |db| db.ack_all_inbox(&address)).await {
        Ok(updated) => (StatusCode::OK, Json(json!({ "acked_threads": updated }))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
struct ListThreadsQuery {
    reader: Option<String>,
}

async fn list_threads(
    State(st): State<AppState>,
    Query(q): Query<ListThreadsQuery>,
) -> impl IntoResponse {
    match run_db(st.db.clone(), move |db| {
        db.list_threads_for_agent(q.reader.as_deref())
    })
    .await
    {
        Ok(threads) => (StatusCode::OK, Json(threads)).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn create_thread(
    State(st): State<AppState>,
    Json(req): Json<CreateThreadRequest>,
) -> impl IntoResponse {
    let mode = req
        .default_mode
        .as_deref()
        .map(SubscriptionMode::from_str)
        .unwrap_or(SubscriptionMode::Mentions);
    let max_hops = req.max_hops.unwrap_or(st.cfg.default_max_hops);
    match run_db(st.db.clone(), move |db| {
        db.create_forum_thread(
            &req.title,
            req.description.as_deref(),
            &req.created_by,
            &req.members,
            mode,
            max_hops,
        )
    })
    .await
    {
        Ok(thread) => {
            let _ = st.events.send(HubEvent::ThreadUpdated {
                thread: thread.clone(),
            });
            (StatusCode::CREATED, Json(thread)).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
struct ReadQuery {
    since_seq: Option<i64>,
    limit: Option<i64>,
    reader: Option<String>,
    auto_ack: Option<bool>,
}

async fn read_messages(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<ReadQuery>,
) -> impl IntoResponse {
    let since_seq = q.since_seq.unwrap_or(0);
    let limit = q.limit.unwrap_or(100);
    let reader = q.reader;
    let auto_ack = q.auto_ack.unwrap_or(true);

    let auth_check = run_db(st.db.clone(), {
        let thread_id = id.clone();
        let reader = reader.clone();
        move |db| {
            let thread = db.get_thread(&thread_id)?;
            match thread {
                None => Ok(None),
                Some(t) => {
                    if t.kind == ThreadKind::Dm {
                        match reader.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                            Some(r) => Ok(Some(db.is_dm_participant(&thread_id, r)?)),
                            None => Ok(Some(false)),
                        }
                    } else {
                        Ok(Some(true))
                    }
                }
            }
        }
    })
    .await;

    match auth_check {
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": format!("Thread '{}' not found", id)})),
        )
            .into_response(),
        Ok(Some(false)) => (
            StatusCode::FORBIDDEN,
            Json(json!({"error": format!("Access denied: agent '{:?}' is not authorized to read private DM thread '{}'", reader, id)})),
        )
            .into_response(),
        Ok(Some(true)) => {
            match run_db(st.db.clone(), move |db| {
                db.read_messages_for_agent(&id, since_seq, limit, reader.as_deref(), auto_ack)
            })
            .await
            {
                Ok(msgs) => (StatusCode::OK, Json(msgs)).into_response(),
                Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
            }
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn post_message(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<PostMessageRequest>,
) -> impl IntoResponse {
    let auth_check = run_db(st.db.clone(), {
        let thread_id = id.clone();
        let sender = req.sender.clone();
        move |db| {
            let thread = db.get_thread(&thread_id)?;
            match thread {
                None => Ok(None),
                Some(t) => {
                    if t.kind == ThreadKind::Dm {
                        Ok(Some(db.is_dm_participant(&thread_id, &sender)?))
                    } else {
                        Ok(Some(true))
                    }
                }
            }
        }
    })
    .await;

    match auth_check {
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": format!("Thread '{}' not found", id)})),
        )
            .into_response(),
        Ok(Some(false)) => (
            StatusCode::FORBIDDEN,
            Json(json!({"error": format!("Access denied: sender '{}' is not a participant in DM thread '{}'", req.sender, id)})),
        )
            .into_response(),
        Ok(Some(true)) => {
            match run_db(st.db.clone(), move |db| {
                db.post_message(&id, &req.sender, &req.body, &req.mentions, req.ack_only)
            })
            .await
            {
                Ok((message, thread)) => {
                    let _ = st.events.send(HubEvent::MessagePosted {
                        message: message.clone(),
                        thread: thread.clone(),
                    });
                    st.dispatcher.dispatch_message(thread, message.clone());
                    (StatusCode::CREATED, Json(message)).into_response()
                }
                Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
            }
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn send_dm(State(st): State<AppState>, Json(req): Json<SendDmRequest>) -> impl IntoResponse {
    let max_hops = st.cfg.default_max_hops;
    let res = run_db(st.db.clone(), move |db| {
        let resolved_to = db.resolve_recipient_alias(&req.to);
        let thread = db.get_or_create_dm_thread(&req.from, &resolved_to, max_hops)?;
        db.post_message(
            &thread.id,
            &req.from,
            &req.body,
            &[resolved_to],
            req.ack_only,
        )
    })
    .await;

    match res {
        Ok((message, updated_thread)) => {
            let _ = st.events.send(HubEvent::MessagePosted {
                message: message.clone(),
                thread: updated_thread.clone(),
            });
            st.dispatcher
                .dispatch_message(updated_thread, message.clone());
            (StatusCode::CREATED, Json(message)).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn list_thread_subs(
    State(st): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match run_db(st.db.clone(), move |db| db.list_subscriptions(Some(&id))).await {
        Ok(subs) => (StatusCode::OK, Json(subs)).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn subscribe(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<SubscribeRequest>,
) -> impl IntoResponse {
    let mode = SubscriptionMode::from_str(&req.mode);
    let auth_check = run_db(st.db.clone(), {
        let thread_id = id.clone();
        let agent = req.agent_address.clone();
        move |db| {
            let thread = db.get_thread(&thread_id)?;
            match thread {
                None => Ok(None),
                Some(t) => {
                    if t.kind == ThreadKind::Dm {
                        Ok(Some(db.is_dm_participant(&thread_id, &agent)?))
                    } else {
                        Ok(Some(true))
                    }
                }
            }
        }
    })
    .await;

    match auth_check {
        Ok(None) => (StatusCode::NOT_FOUND, "Thread not found").into_response(),
        Ok(Some(false)) => (
            StatusCode::FORBIDDEN,
            "Access denied: cannot subscribe to private DM thread",
        )
            .into_response(),
        Ok(Some(true)) => {
            match run_db(st.db.clone(), move |db| {
                db.upsert_subscription(&id, &req.agent_address, mode)
            })
            .await
            {
                Ok(sub) => {
                    let _ = st.events.send(HubEvent::SubscriptionUpdated {
                        subscription: sub.clone(),
                    });
                    (StatusCode::OK, Json(sub)).into_response()
                }
                Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
            }
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn unsubscribe(
    State(st): State<AppState>,
    Path((id, agent)): Path<(String, String)>,
) -> impl IntoResponse {
    match run_db(st.db.clone(), move |db| db.remove_subscription(&id, &agent)).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn mute_thread(State(st): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
    match run_db(st.db.clone(), move |db| db.set_thread_muted(&id, true)).await {
        Ok(Some(thread)) => {
            let _ = st.events.send(HubEvent::ThreadUpdated {
                thread: thread.clone(),
            });
            (StatusCode::OK, Json(thread)).into_response()
        }
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn unmute_thread(State(st): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
    match run_db(st.db.clone(), move |db| db.set_thread_muted(&id, false)).await {
        Ok(Some(thread)) => {
            let _ = st.events.send(HubEvent::ThreadUpdated {
                thread: thread.clone(),
            });
            (StatusCode::OK, Json(thread)).into_response()
        }
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn reset_hops(State(st): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
    match run_db(st.db.clone(), move |db| db.reset_thread_hops(&id)).await {
        Ok(Some(thread)) => {
            let _ = st.events.send(HubEvent::ThreadUpdated {
                thread: thread.clone(),
            });
            (StatusCode::OK, Json(thread)).into_response()
        }
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn pause_hub(State(st): State<AppState>) -> impl IntoResponse {
    match run_db(st.db.clone(), |db| db.set_paused(true)).await {
        Ok(()) => {
            let _ = st.events.send(HubEvent::HubPausedChanged { paused: true });
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn resume_hub(State(st): State<AppState>) -> impl IntoResponse {
    match run_db(st.db.clone(), |db| db.set_paused(false)).await {
        Ok(()) => {
            let _ = st.events.send(HubEvent::HubPausedChanged { paused: false });
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}
