use anyhow::{anyhow, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::{broadcast, Mutex};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tracing::{info, warn};

use crate::config::{resolve_cli_program, Config};
use crate::db::Database;
use crate::models::{
    Agent, HubEvent, Message, Subscription, SubscriptionMode, Thread, ThreadKind, TransportKind,
};

#[derive(Clone)]
pub struct Dispatcher {
    db: Database,
    cfg: Config,
    events: broadcast::Sender<HubEvent>,
    /// Per-agent lock so Tier 2 headless resumes never collide for the same session
    agent_locks: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
    /// Buffered digest messages keyed by (agent_address, thread_id)
    digest_buffers: Arc<Mutex<HashMap<(String, String), Vec<Message>>>>,
}

impl Dispatcher {
    pub fn new(db: Database, cfg: Config, events: broadcast::Sender<HubEvent>) -> Self {
        Self {
            db,
            cfg,
            events,
            agent_locks: Arc::new(Mutex::new(HashMap::new())),
            digest_buffers: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn dispatch_message(&self, thread: Thread, message: Message) {
        let this = self.clone();
        tokio::spawn(async move {
            if let Err(err) = this.dispatch_inner(thread, message).await {
                warn!(error = %err, "dispatch_message failed");
            }
        });
    }

    async fn dispatch_inner(&self, thread: Thread, message: Message) -> Result<()> {
        // Global kill-switch
        if self.db.is_paused()? {
            self.record_and_emit(
                &message.id,
                &thread.id,
                "*",
                "guardrail",
                "skipped_paused",
                Some("hub is globally paused"),
            );
            return Ok(());
        }

        // Thread mute kill-switch
        if thread.muted {
            self.record_and_emit(
                &message.id,
                &thread.id,
                "*",
                "guardrail",
                "skipped_muted",
                Some("thread is muted"),
            );
            return Ok(());
        }

        // Anti-loop hop budget check
        if thread.hop_count > thread.max_hops {
            self.record_and_emit(
                &message.id,
                &thread.id,
                "*",
                "guardrail",
                "skipped_hop_cap",
                Some(&format!(
                    "hop cap reached ({}/{}); run `coommunity reset-hops {}` to resume",
                    thread.hop_count, thread.max_hops, thread.id
                )),
            );
            return Ok(());
        }

        let subs = self.db.list_subscriptions(Some(&thread.id))?;
        for sub in subs {
            // Echo suppression: never notify the sender of their own post
            if sub.agent_address == message.sender {
                continue;
            }
            if !should_notify(&thread, &sub, &message) {
                continue;
            }

            if sub.mode == SubscriptionMode::Digest && thread.kind == ThreadKind::Forum {
                self.enqueue_digest(sub.agent_address.clone(), thread.clone(), message.clone())
                    .await;
                continue;
            }

            let prompt = format_single_notification(&thread, &message);
            self.deliver_to_agent(&sub.agent_address, &thread, &message.id, &prompt)
                .await;
        }

        Ok(())
    }

    async fn enqueue_digest(&self, agent_address: String, thread: Thread, message: Message) {
        let key = (agent_address.clone(), thread.id.clone());
        let should_spawn = {
            let mut map = self.digest_buffers.lock().await;
            let entry = map.entry(key.clone()).or_default();
            let was_empty = entry.is_empty();
            entry.push(message.clone());
            was_empty
        };

        self.record_and_emit(
            &message.id,
            &thread.id,
            &agent_address,
            "digest",
            "buffered",
            Some(&format!("debouncing {}s", self.cfg.digest_window_secs)),
        );

        if should_spawn {
            let this = self.clone();
            let wait_secs = self.cfg.digest_window_secs;
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(wait_secs)).await;
                let batch = {
                    let mut map = this.digest_buffers.lock().await;
                    map.remove(&key).unwrap_or_default()
                };
                if batch.is_empty() {
                    return;
                }
                let last_id = batch
                    .last()
                    .map(|m| m.id.clone())
                    .unwrap_or_else(|| "digest".to_string());
                let prompt = format_digest_notification(&thread, &batch);
                this.deliver_to_agent(&agent_address, &thread, &last_id, &prompt)
                    .await;
            });
        }
    }

    async fn deliver_to_agent(
        &self,
        agent_address: &str,
        thread: &Thread,
        message_id: &str,
        prompt: &str,
    ) {
        let agent = match self.db.get_agent(agent_address) {
            Ok(Some(a)) => a,
            Ok(None) => {
                self.record_and_emit(
                    message_id,
                    &thread.id,
                    agent_address,
                    "tier3_inbox",
                    "queued_unregistered",
                    Some("agent not yet registered; waiting in inbox"),
                );
                return;
            }
            Err(e) => {
                warn!(error = %e, agent = agent_address, "db lookup failed");
                return;
            }
        };

        match agent.transport {
            TransportKind::ClaudePipe => {
                let active_sessions = self.db.get_active_sessions(agent_address).unwrap_or_default();
                let mut delivered_count = 0;
                let mut last_err = String::new();

                if !active_sessions.is_empty() {
                    for (endpoint_opt, auth_token_opt) in active_sessions {
                        let mut target_agent = agent.clone();
                        target_agent.endpoint = endpoint_opt;
                        target_agent.auth_token = auth_token_opt;
                        match push_claude_pipe(&target_agent, prompt).await {
                            Ok(()) => {
                                delivered_count += 1;
                            }
                            Err(err) => {
                                last_err = err.to_string();
                            }
                        }
                    }
                } else {
                    match push_claude_pipe(&agent, prompt).await {
                        Ok(()) => {
                            delivered_count += 1;
                        }
                        Err(err) => {
                            last_err = err.to_string();
                        }
                    }
                }

                if delivered_count > 0 {
                    self.record_and_emit(
                        message_id,
                        &thread.id,
                        agent_address,
                        "tier1_claude_pipe",
                        "delivered",
                        Some(&format!("pushed to {delivered_count} active session(s)")),
                    );
                } else {
                    self.record_and_emit(
                        message_id,
                        &thread.id,
                        agent_address,
                        "tier1_claude_pipe",
                        "fallback_inbox",
                        Some(&format!("pipes unavailable ({last_err}); queued in inbox")),
                    );
                }
            }
            TransportKind::CodexAppServer => match push_codex_app_server(&agent, prompt).await {
                Ok(()) => {
                    self.record_and_emit(
                        message_id,
                        &thread.id,
                        agent_address,
                        "tier1_codex_rpc",
                        "delivered",
                        Some("pushed turn/start via codex app-server"),
                    );
                }
                Err(err) => {
                    self.record_and_emit(
                        message_id,
                        &thread.id,
                        agent_address,
                        "tier1_codex_rpc",
                        "fallback_inbox",
                        Some(&format!("app-server rpc failed ({err}); queued in inbox")),
                    );
                }
            },
            TransportKind::CodexResume | TransportKind::AgyResume => {
                if agent.allow_wake_resume && agent.session_id.is_some() {
                    let lock = self.get_agent_lock(agent_address).await;
                    let _guard = lock.lock().await;
                    let res = match agent.transport {
                        TransportKind::CodexResume => wake_codex_resume(&agent, prompt).await,
                        TransportKind::AgyResume => wake_agy_resume(&agent, prompt).await,
                        _ => unreachable!(),
                    };
                    match res {
                        Ok(()) => {
                            self.record_and_emit(
                                message_id,
                                &thread.id,
                                agent_address,
                                "tier2_wake_resume",
                                "delivered",
                                Some("woke session via headless resume"),
                            );
                        }
                        Err(err) => {
                            self.record_and_emit(
                                message_id,
                                &thread.id,
                                agent_address,
                                "tier2_wake_resume",
                                "fallback_inbox",
                                Some(&format!("resume failed ({err}); queued in inbox")),
                            );
                        }
                    }
                } else {
                    self.record_and_emit(
                        message_id,
                        &thread.id,
                        agent_address,
                        "tier3_inbox",
                        "queued_inbox",
                        Some("awaiting hook banner or check_inbox"),
                    );
                }
            }
            TransportKind::PollOnly => {
                self.record_and_emit(
                    message_id,
                    &thread.id,
                    agent_address,
                    "tier3_inbox",
                    "queued_inbox",
                    Some("awaiting hook banner or check_inbox"),
                );
            }
        }
    }

    async fn get_agent_lock(&self, agent_address: &str) -> Arc<Mutex<()>> {
        let mut map = self.agent_locks.lock().await;
        map.entry(agent_address.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    fn record_and_emit(
        &self,
        message_id: &str,
        thread_id: &str,
        target_agent: &str,
        tier: &str,
        status: &str,
        detail: Option<&str>,
    ) {
        if let Ok(rec) = self
            .db
            .log_delivery(message_id, thread_id, target_agent, tier, status, detail)
        {
            let _ = self.events.send(HubEvent::DeliveryLogged { delivery: rec });
        }
    }
}

pub fn should_notify(thread: &Thread, sub: &Subscription, msg: &Message) -> bool {
    if sub.mode == SubscriptionMode::Muted {
        return false;
    }
    if thread.kind == ThreadKind::Dm {
        return true;
    }
    match sub.mode {
        SubscriptionMode::All | SubscriptionMode::Digest => true,
        SubscriptionMode::Mentions => msg.mentions.iter().any(|m| {
            m.eq_ignore_ascii_case(&sub.agent_address)
                || m.eq_ignore_ascii_case("all")
                || m.eq_ignore_ascii_case("everyone")
        }),
        SubscriptionMode::Muted => false,
    }
}

fn format_single_notification(thread: &Thread, msg: &Message) -> String {
    let kind_label = match thread.kind {
        ThreadKind::Dm => "DM",
        ThreadKind::Forum => "Forum",
    };
    let reply_hint = if msg.ack_only {
        "This is an informational update (ack_only=true). Do not reply unless you have a critical correction."
    } else {
        "Use coom MCP `post` (or `dm`) to respond if needed, or ignore if no reply is required."
    };

    format!(
        "[coom {kind_label}] Thread: {title} ({tid}) #{seq}\nFrom: {sender}\nMessage:\n{body}\n\n{hint}",
        title = thread.title,
        tid = thread.id,
        seq = msg.seq,
        sender = msg.sender,
        body = msg.body,
        hint = reply_hint,
    )
}

fn format_digest_notification(thread: &Thread, batch: &[Message]) -> String {
    let mut lines = Vec::new();
    lines.push(format!(
        "[coom Forum Digest] Thread: {} ({}) — {} new messages:",
        thread.title,
        thread.id,
        batch.len()
    ));
    for m in batch {
        lines.push(format!("- #{} {}: {}", m.seq, m.sender, m.body));
    }
    lines.push(
        "\nUse coom MCP `read_thread` for full context or `post` if you need to reply."
            .to_string(),
    );
    lines.join("\n")
}

/// Tier 1a: Push directly to Claude Code's per-session Windows Named Pipe or Unix Domain Socket.
async fn push_claude_pipe(agent: &Agent, text: &str) -> Result<()> {
    let raw_endpoint = agent
        .endpoint
        .as_deref()
        .ok_or_else(|| anyhow!("missing CLAUDE_CODE_MESSAGING_SOCKET endpoint"))?;
    let path = raw_endpoint.strip_prefix("uds:").unwrap_or(raw_endpoint);

    let mut payload = String::new();
    if let Some(token) = agent.auth_token.as_deref().filter(|t| !t.is_empty()) {
        payload.push_str(&json!({ "type": "auth", "token": token }).to_string());
        payload.push('\n');
    }
    payload.push_str(
        &json!({
            "type": "message",
            "from": "coommunity",
            "text": text
        })
        .to_string(),
    );
    payload.push('\n');

    #[cfg(windows)]
    {
        use tokio::net::windows::named_pipe::ClientOptions;
        let mut client = ClientOptions::new().open(path)?;
        client.write_all(payload.as_bytes()).await?;
        client.flush().await?;
        info!(agent = %agent.address, pipe = path, "pushed message to Claude named pipe");
        Ok(())
    }

    #[cfg(unix)]
    {
        use tokio::net::UnixStream;
        let mut stream = UnixStream::connect(path).await?;
        stream.write_all(payload.as_bytes()).await?;
        stream.flush().await?;
        info!(agent = %agent.address, socket = path, "pushed message to Claude UDS");
        Ok(())
    }
}

/// Tier 1b: Push a live `turn/start` JSON-RPC 2.0 request to `codex app-server` over WebSocket.
async fn push_codex_app_server(agent: &Agent, text: &str) -> Result<()> {
    let ws_url = agent
        .endpoint
        .as_deref()
        .ok_or_else(|| anyhow!("missing codex app-server ws:// endpoint"))?;
    let thread_id = agent
        .session_id
        .as_deref()
        .ok_or_else(|| anyhow!("missing Codex threadId/session_id"))?;

    let (mut ws, _) = tokio_tungstenite::connect_async(ws_url).await?;

    // 1. JSON-RPC initialize
    let init_req = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "clientInfo": { "name": "coommunity", "version": env!("CARGO_PKG_VERSION") }
        }
    });
    ws.send(WsMessage::Text(init_req.to_string())).await?;

    // Wait briefly for initialize response
    let _ = tokio::time::timeout(Duration::from_secs(3), ws.next()).await;

    // 2. initialized notification
    let initialized = json!({
        "jsonrpc": "2.0",
        "method": "initialized",
        "params": {}
    });
    ws.send(WsMessage::Text(initialized.to_string())).await?;

    // 3. turn/start into the active Codex thread
    let turn_start = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "turn/start",
        "params": {
            "threadId": thread_id,
            "input": [{ "type": "text", "text": text }]
        }
    });
    ws.send(WsMessage::Text(turn_start.to_string())).await?;

    let _ = tokio::time::timeout(Duration::from_secs(3), ws.next()).await;
    let _ = ws.close(None).await;
    info!(agent = %agent.address, ws = ws_url, thread_id = thread_id, "pushed turn/start to codex app-server");
    Ok(())
}

/// Tier 2a: Wake an idle Codex session via `codex exec resume <session_id> -`
async fn wake_codex_resume(agent: &Agent, text: &str) -> Result<()> {
    let session_id = agent
        .session_id
        .as_deref()
        .ok_or_else(|| anyhow!("missing Codex session_id"))?;

    let (prog, prefix) = resolve_cli_program("codex");
    let mut cmd = Command::new(prog);
    cmd.args(prefix).args(["exec", "resume", session_id, "-"])
        .env("COOMMUNITY_INTERNAL", "1")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    if let Some(cwd) = agent.cwd.as_deref() {
        cmd.current_dir(cwd);
    }

    let mut child = cmd.spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(text.as_bytes()).await?;
        drop(stdin);
    }
    let status = child.wait().await?;
    if !status.success() {
        return Err(anyhow!("codex exec resume exited with {status}"));
    }
    Ok(())
}

/// Tier 2b: Wake an idle Antigravity session via `agy --conversation=<uuid> -p <prompt>`
async fn wake_agy_resume(agent: &Agent, text: &str) -> Result<()> {
    let session_id = agent
        .session_id
        .as_deref()
        .ok_or_else(|| anyhow!("missing Antigravity conversation id"))?;

    let (prog, prefix) = resolve_cli_program("agy");
    let mut cmd = Command::new(prog);
    cmd.args(prefix).args([&format!("--conversation={session_id}"), "-p", text])
        .env("COOMMUNITY_INTERNAL", "1")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    if let Some(cwd) = agent.cwd.as_deref() {
        cmd.current_dir(cwd);
    }

    let status = cmd.status().await?;
    if !status.success() {
        return Err(anyhow!("agy headless resume exited with {status}"));
    }
    Ok(())
}
