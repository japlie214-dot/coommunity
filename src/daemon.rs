use anyhow::Result;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{
        sse::{Event, KeepAlive, Sse},
        Html, IntoResponse,
    },
    routing::{delete, get, post},
    Json, Router,
};
use futures_util::stream::Stream;
use serde::Deserialize;
use std::convert::Infallible;
use std::time::Duration;
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt as _;
use tower_http::cors::CorsLayer;
use tracing::info;

use crate::config::Config;
use crate::db::Database;
use crate::dispatcher::Dispatcher;
use crate::models::{
    AckInboxRequest, AgentStatus, CreateThreadRequest, HubEvent, PostMessageRequest,
    RegisterAgentRequest, SendDmRequest, SnapshotResponse, SubscribeRequest, SubscriptionMode,
};
use crate::web_ui;

#[derive(Clone)]
pub struct AppState {
    pub cfg: Config,
    pub db: Database,
    pub dispatcher: Dispatcher,
    pub events: broadcast::Sender<HubEvent>,
}

pub async fn run_daemon(cfg: Config) -> Result<()> {
    let db = Database::open(&cfg.db_path)?;
    let (events, _) = broadcast::channel::<HubEvent>(512);
    let dispatcher = Dispatcher::new(db.clone(), cfg.clone(), events.clone());

    let state = AppState {
        cfg: cfg.clone(),
        db,
        dispatcher,
        events,
    };

    let app = Router::new()
        // Read-only local web viewer
        .route("/", get(index_html))
        .route("/api/health", get(health))
        .route("/api/snapshot", get(snapshot))
        .route("/api/events", get(sse_events))
        // Agents
        .route("/api/agents", get(list_agents).post(register_agent))
        .route("/api/agents/:address/status", post(set_agent_status))
        .route("/api/agents/:address/inbox", get(check_inbox))
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
        .layer(CorsLayer::permissive())
        .with_state(state);

    let addr = format!("{}:{}", cfg.host, cfg.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    info!(
        url = %cfg.base_url(),
        db = %cfg.db_path.display(),
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
    match st.db.hub_status() {
        Ok(s) => (StatusCode::OK, Json(s)).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn snapshot(State(st): State<AppState>) -> impl IntoResponse {
    let res = (|| -> Result<SnapshotResponse> {
        Ok(SnapshotResponse {
            status: st.db.hub_status()?,
            agents: st.db.list_agents()?,
            threads: st.db.list_threads()?,
            subscriptions: st.db.list_subscriptions(None)?,
            recent_deliveries: st.db.recent_deliveries(40)?,
        })
    })();
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
    match st.db.list_agents() {
        Ok(agents) => (StatusCode::OK, Json(agents)).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn register_agent(
    State(st): State<AppState>,
    Json(req): Json<RegisterAgentRequest>,
) -> impl IntoResponse {
    match st.db.upsert_agent(&req) {
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
    match st
        .db
        .set_agent_session_status(
            &address,
            body.session_id.as_deref(),
            AgentStatus::from_str(&body.status),
        )
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
    match st.db.check_inbox(&address) {
        Ok(items) => (StatusCode::OK, Json(items)).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn ack_inbox(
    State(st): State<AppState>,
    Json(req): Json<AckInboxRequest>,
) -> impl IntoResponse {
    match st
        .db
        .ack_inbox(&req.agent_address, &req.thread_id, req.up_to_seq)
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn list_threads(State(st): State<AppState>) -> impl IntoResponse {
    match st.db.list_threads() {
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
    match st.db.create_forum_thread(
        &req.title,
        req.description.as_deref(),
        &req.created_by,
        &req.members,
        mode,
        max_hops,
    ) {
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
}

async fn read_messages(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<ReadQuery>,
) -> impl IntoResponse {
    match st
        .db
        .read_messages(&id, q.since_seq.unwrap_or(0), q.limit.unwrap_or(100))
    {
        Ok(msgs) => (StatusCode::OK, Json(msgs)).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn post_message(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<PostMessageRequest>,
) -> impl IntoResponse {
    match st
        .db
        .post_message(&id, &req.sender, &req.body, &req.mentions, req.ack_only)
    {
        Ok((message, thread)) => {
            let _ = st.events.send(HubEvent::MessagePosted {
                message: message.clone(),
                thread: thread.clone(),
            });
            st.dispatcher
                .dispatch_message(thread, message.clone());
            (StatusCode::CREATED, Json(message)).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn send_dm(State(st): State<AppState>, Json(req): Json<SendDmRequest>) -> impl IntoResponse {
    let resolved_to = st.db.resolve_recipient_alias(&req.to);
    let thread = match st
        .db
        .get_or_create_dm_thread(&req.from, &resolved_to, st.cfg.default_max_hops)
    {
        Ok(t) => t,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };

    match st
        .db
        .post_message(&thread.id, &req.from, &req.body, &[resolved_to.clone()], req.ack_only)
    {
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
    match st.db.list_subscriptions(Some(&id)) {
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
    match st.db.upsert_subscription(&id, &req.agent_address, mode) {
        Ok(sub) => {
            let _ = st.events.send(HubEvent::SubscriptionUpdated {
                subscription: sub.clone(),
            });
            (StatusCode::OK, Json(sub)).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn unsubscribe(
    State(st): State<AppState>,
    Path((id, agent)): Path<(String, String)>,
) -> impl IntoResponse {
    match st.db.remove_subscription(&id, &agent) {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn mute_thread(State(st): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
    match st.db.set_thread_muted(&id, true) {
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
    match st.db.set_thread_muted(&id, false) {
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
    match st.db.reset_thread_hops(&id) {
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
    match st.db.set_paused(true) {
        Ok(()) => {
            let _ = st.events.send(HubEvent::HubPausedChanged { paused: true });
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn resume_hub(State(st): State<AppState>) -> impl IntoResponse {
    match st.db.set_paused(false) {
        Ok(()) => {
            let _ = st.events.send(HubEvent::HubPausedChanged { paused: false });
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}
