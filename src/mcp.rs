use anyhow::{anyhow, Result};
use reqwest::Client;
use serde_json::{json, Value};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::config::Config;
use crate::models::{
    mention_matches_agent, AckInboxRequest, CreateThreadRequest, RegisterAgentRequest,
    SubscribeRequest,
};

pub async fn run_mcp_stdio(
    cfg: Config,
    runtime: String,
    address_override: Option<String>,
) -> Result<()> {
    let client = cfg.http_client(Duration::from_secs(10))?;
    ensure_daemon_running(&cfg, &client).await?;

    let (detected_addr, detected_sess, detected_cwd) =
        if runtime == "claude_desktop" && address_override.is_none() {
            if let Some(id) = crate::claude_desktop::detect_active_session() {
                (Some(id.address), id.session_id, id.cwd)
            } else {
                (None, None, None)
            }
        } else {
            (None, None, None)
        };

    let cwd = detected_cwd.or_else(|| {
        std::env::current_dir()
            .ok()
            .map(|p| p.to_string_lossy().to_string())
    });
    let session_id = detected_sess
        .or_else(|| std::env::var("COOMMUNITY_SESSION_ID").ok())
        .or_else(|| std::env::var("CLAUDE_SESSION_ID").ok())
        .or_else(|| std::env::var("CODEX_SESSION_ID").ok())
        .or_else(|| std::env::var("AGY_CONVERSATION_ID").ok())
        .or_else(|| std::env::var("CONVERSATION_ID").ok());
    let session_key = session_id.clone();

    // Register this MCP session automatically (hook-first PollOnly transport)
    let reg = RegisterAgentRequest {
        address: address_override
            .clone()
            .or_else(|| std::env::var("COOM_AGENT").ok())
            .or_else(|| std::env::var("COOMMUNITY_AGENT").ok())
            .or(detected_addr),
        runtime: runtime.clone(),
        session_id,
        cwd,
        status: Some("online".to_string()),
        transport: Some("poll_only".to_string()),
        endpoint: None,
        auth_token: None,
        allow_wake_resume: None,
    };

    let me: Value = client
        .post(format!("{}/api/agents", cfg.base_url()))
        .json(&reg)
        .send()
        .await?
        .json()
        .await?;
    let my_address = me
        .get("address")
        .and_then(|v| v.as_str())
        .unwrap_or("agent:unknown")
        .to_string();

    let (tx_out, mut rx_out) = tokio::sync::mpsc::channel::<String>(128);
    let mut stdout = tokio::io::stdout();
    let stdout_handle = tokio::spawn(async move {
        while let Some(msg) = rx_out.recv().await {
            let _ = stdout.write_all(msg.as_bytes()).await;
            let _ = stdout.flush().await;
        }
    });

    // Background SSE listener that invalidates `coom://inbox` and logs incoming DMs/@mentions
    let sse_client = client.clone();
    let sse_url = format!("{}/api/events", cfg.base_url());
    let sse_my_address = my_address.clone();
    let sse_tx = tx_out.clone();

    let sse_handle = tokio::spawn(async move {
        if let Ok(mut resp) = sse_client
            .get(&sse_url)
            .timeout(Duration::from_secs(86400))
            .send()
            .await
        {
            let mut buffer = String::new();
            while let Ok(Some(chunk)) = resp.chunk().await {
                if let Ok(text) = std::str::from_utf8(&chunk) {
                    buffer.push_str(text);
                    while let Some(pos) = buffer.find('\n') {
                        let line = buffer[..pos].trim().to_string();
                        buffer = buffer[pos + 1..].to_string();
                        if let Some(json_str) = line.strip_prefix("data:") {
                            let json_str = json_str.trim();
                            if let Ok(event) = serde_json::from_str::<Value>(json_str) {
                                if event.get("type").and_then(|v| v.as_str())
                                    == Some("message_posted")
                                {
                                    if let Some(msg) = event.get("message") {
                                        let sender = msg
                                            .get("sender")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("");
                                        if sender != sse_my_address {
                                            let body = msg
                                                .get("body")
                                                .and_then(|v| v.as_str())
                                                .unwrap_or("");
                                            let mentions =
                                                msg.get("mentions").and_then(|v| v.as_array());
                                            let is_mention = mentions.map_or(false, |arr| {
                                                arr.iter().any(|m| {
                                                    m.as_str().map_or(false, |s| {
                                                        mention_matches_agent(s, &sse_my_address)
                                                    })
                                                })
                                            });
                                            let thread_kind = event
                                                .get("thread")
                                                .and_then(|t| t.get("kind"))
                                                .and_then(|k| k.as_str())
                                                .unwrap_or("");
                                            let is_dm = thread_kind == "dm";

                                            if is_dm || is_mention {
                                                let notify = json!({
                                                    "jsonrpc": "2.0",
                                                    "method": "notifications/message",
                                                    "params": {
                                                        "level": "info",
                                                        "logger": "coom",
                                                        "data": format!("[coom unread] From {sender}: {body}")
                                                    }
                                                });
                                                if let Ok(out) = serde_json::to_string(&notify) {
                                                    let _ = sse_tx.send(format!("{out}\n")).await;
                                                }
                                                let res_update = json!({
                                                    "jsonrpc": "2.0",
                                                    "method": "notifications/resources/updated",
                                                    "params": {
                                                        "uri": "coom://inbox"
                                                    }
                                                });
                                                if let Ok(out) = serde_json::to_string(&res_update) {
                                                    let _ = sse_tx.send(format!("{out}\n")).await;
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    });

    let stdin = tokio::io::stdin();
    let mut lines = BufReader::new(stdin).lines();

    while let Some(line) = lines.next_line().await? {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let id = req.get("id").cloned();
        let method = req.get("method").and_then(|v| v.as_str()).unwrap_or("");
        let params = req.get("params").cloned().unwrap_or_else(|| json!({}));

        let resp = match method {
            "initialize" => Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {
                        "tools": {},
                        "logging": {},
                        "resources": { "subscribe": true, "listChanged": true }
                    },
                    "serverInfo": {
                        "name": "coom",
                        "version": env!("CARGO_PKG_VERSION")
                    },
                    "instructions": format!(
                        "You are connected to the local coom multi-agent hub as `{my_address}`. \
                         Call `check_inbox` when reminded by hooks (or at session start in Claude Desktop) \
                         to see unread DMs and forum @mentions. Calling `read_thread` returns the latest page \
                         and automatically marks that thread read; use `ack_inbox` or `ack_all_inbox` to clear reminders. \
                         Use `find_agents`/`list_agents` to discover peers, `update_profile` to publish capabilities, \
                         `dm` (`to: \"Boss\"` for the human operator) for 1-to-1 messages, and \
                         `create_thread`/`subscribe`/`post`/`set_thread_brief`/`resolve_message` for forums and typed handoffs."
                    )
                }
            })),
            "notifications/initialized" => None,
            "ping" => Some(json!({ "jsonrpc": "2.0", "id": id, "result": {} })),
            "resources/list" => Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": { "resources": resource_definitions() }
            })),
            "resources/read" => {
                let uri = params.get("uri").and_then(|v| v.as_str()).unwrap_or("");
                let effective_address =
                    if runtime == "claude_desktop" && address_override.is_none() {
                        crate::claude_desktop::detect_active_session()
                            .map(|id| id.address)
                            .unwrap_or_else(|| my_address.clone())
                    } else {
                        my_address.clone()
                    };
                match handle_resource_read(&cfg, &client, &effective_address, uri).await {
                    Ok(val) => Some(json!({ "jsonrpc": "2.0", "id": id, "result": val })),
                    Err(e) => Some(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": { "code": -32002, "message": format!("Resource read failed: {e}") }
                    })),
                }
            }
            "resources/subscribe" => Some(json!({ "jsonrpc": "2.0", "id": id, "result": {} })),
            "resources/unsubscribe" => Some(json!({ "jsonrpc": "2.0", "id": id, "result": {} })),
            "tools/list" => Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": { "tools": tool_definitions() }
            })),
            "tools/call" => {
                let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
                let effective_address =
                    if runtime == "claude_desktop" && address_override.is_none() {
                        if let Some(identity) = crate::claude_desktop::detect_active_session() {
                            let _ = client
                                .post(format!("{}/api/agents", cfg.base_url()))
                                .json(&RegisterAgentRequest {
                                    address: Some(identity.address.clone()),
                                    runtime: runtime.clone(),
                                    session_id: identity.session_id.clone(),
                                    cwd: identity.cwd.clone(),
                                    status: Some("online".to_string()),
                                    transport: Some("poll_only".to_string()),
                                    endpoint: None,
                                    auth_token: None,
                                    allow_wake_resume: None,
                                })
                                .send()
                                .await;
                            identity.address
                        } else {
                            my_address.clone()
                        }
                    } else {
                        my_address.clone()
                    };
                let call_result =
                    handle_tool_call(&cfg, &client, &effective_address, name, &args).await;
                let payload = match call_result {
                    Ok(v) => json!({
                        "content": [{ "type": "text", "text": serde_json::to_string_pretty(&v).unwrap_or_default() }],
                        "isError": false
                    }),
                    Err(e) => json!({
                        "content": [{ "type": "text", "text": format!("Error: {e}") }],
                        "isError": true
                    }),
                };
                Some(json!({ "jsonrpc": "2.0", "id": id, "result": payload }))
            }
            _ => id.map(|req_id| {
                json!({
                    "jsonrpc": "2.0",
                    "id": req_id,
                    "error": { "code": -32601, "message": format!("Method not found: {method}") }
                })
            }),
        };

        if let Some(r) = resp {
            let mut out = serde_json::to_string(&r)?;
            out.push('\n');
            let _ = tx_out.send(out).await;
        }
    }

    sse_handle.abort();
    drop(tx_out);
    let _ = stdout_handle.await;

    // Best-effort session offline update when MCP stdio closes
    let _ = client
        .post(format!("{}/api/agents/{}/status", cfg.base_url(), my_address))
        .json(&json!({ "status": "offline", "session_id": session_key }))
        .send()
        .await;

    Ok(())
}

pub async fn ensure_daemon_running(cfg: &Config, client: &Client) -> Result<()> {
    let health_url = format!("{}/api/health", cfg.base_url());
    if client.get(&health_url).send().await.is_ok() {
        return Ok(());
    }

    // Spawn `coommunity daemon` detached in background
    let exe = std::env::current_exe()?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("daemon")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x00000008;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NO_WINDOW);
    }

    let _ = cmd.spawn()?;

    for _ in 0..25 {
        tokio::time::sleep(Duration::from_millis(120)).await;
        if client.get(&health_url).send().await.is_ok() {
            return Ok(());
        }
    }
    Err(anyhow!(
        "coommunity daemon failed to start at {}",
        cfg.base_url()
    ))
}

async fn handle_tool_call(
    cfg: &Config,
    client: &Client,
    my_address: &str,
    name: &str,
    args: &Value,
) -> Result<Value> {
    let base = cfg.base_url();
    match name {
        "whoami" => Ok(json!({
            "address": my_address,
            "hub_url": base,
            "viewer_url": base
        })),
        "list_agents" => {
            let v: Value = client.get(format!("{base}/api/agents")).send().await?.json().await?;
            Ok(v)
        }
        "set_status" => {
            let status = req_str(args, "status")?;
            let v: Value = client
                .post(format!("{base}/api/agents/{my_address}/status"))
                .json(&json!({ "status": status }))
                .send()
                .await?
                .json()
                .await?;
            Ok(v)
        }
        "dm" => {
            let mut payload = json!({
                "from": my_address,
                "to": req_str(args, "to")?,
                "body": req_str(args, "body")?,
                "ack_only": args.get("ack_only").and_then(|v| v.as_bool()).unwrap_or(false),
                "msg_type": args.get("msg_type").and_then(|v| v.as_str()).unwrap_or("chat"),
                "refs": args.get("refs").cloned().unwrap_or_else(|| json!([]))
            });
            if payload["msg_type"].is_null() {
                payload["msg_type"] = json!("chat");
            }
            let resp = client.post(format!("{base}/api/v2/dm")).json(&payload).send().await?;
            if !resp.status().is_success() {
                let status = resp.status();
                let msg = resp.text().await.unwrap_or_default();
                return Err(anyhow!("DM failed ({status}): {msg}"));
            }
            let v: Value = resp.json().await?;
            Ok(v)
        }
        "create_thread" => {
            let members: Vec<String> = args
                .get("members")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|x| x.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            let req = CreateThreadRequest {
                title: req_str(args, "title")?,
                description: args
                    .get("description")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                created_by: my_address.to_string(),
                members,
                default_mode: args
                    .get("default_mode")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                max_hops: args.get("max_hops").and_then(|v| v.as_i64()),
            };
            let v: Value = client
                .post(format!("{base}/api/threads"))
                .json(&req)
                .send()
                .await?
                .json()
                .await?;
            Ok(v)
        }
        "list_threads" => {
            let mut query_params: Vec<(&str, String)> = vec![("reader", my_address.to_string())];
            if let Some(kind) = args.get("kind").and_then(|v| v.as_str()) {
                query_params.push(("kind", kind.to_string()));
            }
            if let Some(q) = args.get("q").and_then(|v| v.as_str()) {
                query_params.push(("q", q.to_string()));
            }
            if let Some(cursor) = args.get("before_updated_at").and_then(|v| v.as_str()) {
                query_params.push(("before_updated_at", cursor.to_string()));
            }
            if let Some(limit) = args.get("limit").and_then(|v| v.as_i64()) {
                query_params.push(("limit", limit.to_string()));
            }
            let v: Value = client
                .get(format!("{base}/api/v2/threads"))
                .query(&query_params)
                .send()
                .await?
                .json()
                .await?;
            Ok(v)
        }
        "post" => {
            let thread_id = req_str(args, "thread_id")?;
            let mentions: Vec<String> = args
                .get("mentions")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|x| x.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            let mut payload = json!({
                "sender": my_address,
                "body": req_str(args, "body")?,
                "mentions": mentions,
                "ack_only": args.get("ack_only").and_then(|v| v.as_bool()).unwrap_or(false),
                "msg_type": args.get("msg_type").and_then(|v| v.as_str()).unwrap_or("chat"),
                "refs": args.get("refs").cloned().unwrap_or_else(|| json!([]))
            });
            if let Some(status) = args.get("status").and_then(|v| v.as_str()) {
                payload["status"] = json!(status);
            }
            if let Some(reply_to) = args.get("reply_to_seq").and_then(|v| v.as_i64()) {
                payload["reply_to_seq"] = json!(reply_to);
            }
            let resp = client
                .post(format!("{base}/api/v2/threads/{thread_id}/messages"))
                .json(&payload)
                .send()
                .await?;
            if resp.status() == reqwest::StatusCode::FORBIDDEN {
                return Err(anyhow!("FORBIDDEN: Access denied: not authorized to post in private DM"));
            }
            if !resp.status().is_success() {
                let status = resp.status();
                let msg = resp.text().await.unwrap_or_default();
                return Err(anyhow!("Post failed ({status}): {msg}"));
            }
            let v: Value = resp.json().await?;
            Ok(v)
        }
        "read_thread" => {
            let thread_id = req_str(args, "thread_id")?;
            let limit = args.get("limit").and_then(|v| v.as_i64()).unwrap_or(50);
            let mut query_params: Vec<(&str, String)> = vec![
                ("limit", limit.to_string()),
                ("reader", my_address.to_string()),
            ];
            if let Some(before) = args.get("before_seq").and_then(|v| v.as_i64()) {
                query_params.push(("before_seq", before.to_string()));
            }
            if let Some(since) = args.get("since_seq").and_then(|v| v.as_i64()) {
                query_params.push(("since_seq", since.to_string()));
            }
            let resp = client
                .get(format!("{base}/api/v2/threads/{thread_id}/messages"))
                .query(&query_params)
                .send()
                .await?;
            if resp.status() == reqwest::StatusCode::FORBIDDEN {
                return Err(anyhow!("FORBIDDEN: Access denied: not authorized to read private DM"));
            }
            if !resp.status().is_success() {
                let status = resp.status();
                let msg = resp.text().await.unwrap_or_default();
                return Err(anyhow!("Read thread failed ({status}): {msg}"));
            }
            let v: Value = resp.json().await?;
            Ok(v)
        }
        "resolve_message" => {
            let thread_id = req_str(args, "thread_id")?;
            let seq = args
                .get("seq")
                .and_then(|v| v.as_i64())
                .ok_or_else(|| anyhow!("missing required integer parameter `seq`"))?;
            let status = req_str(args, "status")?;
            let resp = client
                .patch(format!("{base}/api/threads/{thread_id}/messages/{seq}/status"))
                .json(&json!({ "status": status, "updated_by": my_address }))
                .send()
                .await?;
            if !resp.status().is_success() {
                let code = resp.status();
                let msg = resp.text().await.unwrap_or_default();
                return Err(anyhow!("Resolve message failed ({code}): {msg}"));
            }
            let v: Value = resp.json().await?;
            Ok(v)
        }
        "set_thread_brief" => {
            let thread_id = req_str(args, "thread_id")?;
            let brief = req_str(args, "brief")?;
            let resp = client
                .put(format!("{base}/api/threads/{thread_id}/brief"))
                .json(&json!({ "brief": brief, "updated_by": my_address }))
                .send()
                .await?;
            if !resp.status().is_success() {
                let code = resp.status();
                let msg = resp.text().await.unwrap_or_default();
                return Err(anyhow!("Set brief failed ({code}): {msg}"));
            }
            let v: Value = resp.json().await?;
            Ok(v)
        }
        "update_profile" => {
            let mut body = json!({});
            if let Some(bio) = args.get("bio") {
                body["bio"] = bio.clone();
            }
            if let Some(caps) = args.get("capabilities") {
                body["capabilities"] = caps.clone();
            }
            if let Some(eph) = args.get("ephemeral") {
                body["ephemeral"] = eph.clone();
            }
            let v: Value = client
                .patch(format!("{base}/api/agents/{my_address}/profile"))
                .json(&body)
                .send()
                .await?
                .json()
                .await?;
            Ok(v)
        }
        "find_agents" => {
            let mut query_params: Vec<(&str, String)> = Vec::new();
            if let Some(cap) = args.get("capability").and_then(|v| v.as_str()) {
                query_params.push(("capability", cap.to_string()));
            }
            if let Some(rt) = args.get("runtime").and_then(|v| v.as_str()) {
                query_params.push(("runtime", rt.to_string()));
            }
            let v: Value = client
                .get(format!("{base}/api/agents/search"))
                .query(&query_params)
                .send()
                .await?
                .json()
                .await?;
            Ok(v)
        }
        "subscribe" => {
            let thread_id = req_str(args, "thread_id")?;
            let mode = args
                .get("mode")
                .and_then(|v| v.as_str())
                .unwrap_or("mentions")
                .to_string();
            let req = SubscribeRequest {
                agent_address: my_address.to_string(),
                mode,
            };
            let v: Value = client
                .post(format!("{base}/api/threads/{thread_id}/subscriptions"))
                .json(&req)
                .send()
                .await?
                .json()
                .await?;
            Ok(v)
        }
        "unsubscribe" => {
            let thread_id = req_str(args, "thread_id")?;
            let resp = client
                .delete(format!(
                    "{base}/api/threads/{thread_id}/subscriptions/{my_address}"
                ))
                .send()
                .await?;
            Ok(json!({ "unsubscribed": resp.status().is_success(), "thread_id": thread_id }))
        }
        "check_inbox" => {
            let v: Value = client
                .get(format!("{base}/api/agents/{my_address}/inbox"))
                .send()
                .await?
                .json()
                .await?;
            Ok(v)
        }
        "ack_inbox" => {
            let req = AckInboxRequest {
                agent_address: my_address.to_string(),
                thread_id: req_str(args, "thread_id")?,
                up_to_seq: args
                    .get("up_to_seq")
                    .and_then(|v| v.as_i64())
                    .ok_or_else(|| anyhow!("missing required integer parameter `up_to_seq`"))?,
            };
            let resp = client
                .post(format!("{base}/api/inbox/ack"))
                .json(&req)
                .send()
                .await?;
            Ok(json!({ "acked": resp.status().is_success() }))
        }
        "ack_all_inbox" => {
            let v: Value = client
                .post(format!("{base}/api/agents/{my_address}/inbox/ack-all"))
                .send()
                .await?
                .json()
                .await?;
            Ok(v)
        }
        "wait_for_message" => {
            let timeout_secs = args
                .get("timeout_secs")
                .and_then(|v| v.as_u64())
                .unwrap_or(30)
                .min(300);

            // 1. Check if unread messages are already in inbox
            let inbox_items: Vec<Value> = client
                .get(format!("{base}/api/agents/{my_address}/inbox"))
                .send()
                .await?
                .json()
                .await
                .unwrap_or_default();

            if !inbox_items.is_empty() {
                return Ok(json!({
                    "status": "received",
                    "source": "inbox",
                    "messages": inbox_items
                }));
            }

            // 2. Stream SSE events until a matching message arrives or timeout
            let sse_url = format!("{base}/api/events");
            let timeout_dur = Duration::from_secs(timeout_secs);
            let sse_call = async {
                if let Ok(mut resp) = client.get(&sse_url).timeout(timeout_dur).send().await {
                    let mut buffer = String::new();
                    while let Ok(Some(chunk)) = resp.chunk().await {
                        if let Ok(text) = std::str::from_utf8(&chunk) {
                            buffer.push_str(text);
                            while let Some(pos) = buffer.find('\n') {
                                let line = buffer[..pos].trim().to_string();
                                buffer = buffer[pos + 1..].to_string();
                                if let Some(json_str) = line.strip_prefix("data:") {
                                    if let Ok(ev) = serde_json::from_str::<Value>(json_str.trim()) {
                                        if ev.get("type").and_then(|v| v.as_str()) == Some("message_posted") {
                                            if let Some(msg) = ev.get("message") {
                                                let sender = msg.get("sender").and_then(|v| v.as_str()).unwrap_or("");
                                                if sender != my_address {
                                                    let mentions = msg.get("mentions").and_then(|v| v.as_array());
                                                    let is_mention = mentions.map_or(false, |arr| {
                                                        arr.iter().any(|m| {
                                                            m.as_str().map_or(false, |s| {
                                                                mention_matches_agent(s, my_address)
                                                            })
                                                        })
                                                    });
                                                    let thread_kind = ev.get("thread").and_then(|t| t.get("kind")).and_then(|k| k.as_str()).unwrap_or("");
                                                    if thread_kind == "dm" || is_mention {
                                                        return Ok::<Value, anyhow::Error>(json!({
                                                            "status": "received",
                                                            "source": "live_stream",
                                                            "message": msg,
                                                            "thread": ev.get("thread")
                                                        }));
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                Ok(json!({
                    "status": "timeout",
                    "message": format!("No incoming messages received within {timeout_secs}s.")
                }))
            };

            match tokio::time::timeout(timeout_dur, sse_call).await {
                Ok(res) => res,
                Err(_) => Ok(json!({
                    "status": "timeout",
                    "message": format!("Timed out waiting for message after {timeout_secs}s.")
                })),
            }
        }
        _ => Err(anyhow!("unknown tool: {name}")),
    }
}

fn req_str(args: &Value, key: &str) -> Result<String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| anyhow!("missing required string parameter `{key}`"))
}

pub fn tool_definitions() -> Value {
    let mut tools = vec![
        json!({
            "name": "whoami",
            "description": "Return this agent's registered coommunity address and local viewer URL.",
            "inputSchema": { "type": "object", "properties": {} }
        }),
        json!({
            "name": "list_agents",
            "description": "List all registered agents (Claude Code, Claude Desktop, Codex, Antigravity, Boss), their addresses, working directories, statuses, and delivery transports.",
            "inputSchema": { "type": "object", "properties": {} }
        }),
        json!({
            "name": "set_status",
            "description": "Update this agent's presence status (online, busy, idle, offline).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "status": { "type": "string", "enum": ["online", "busy", "idle", "offline"] }
                },
                "required": ["status"]
            }
        }),
        json!({
            "name": "create_thread",
            "description": "Create a multi-agent forum thread and subscribe initial members.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "title": { "type": "string", "description": "Short forum topic title" },
                    "description": { "type": "string", "description": "Optional context or goal for the thread" },
                    "members": { "type": "array", "items": { "type": "string" }, "description": "Agent addresses to subscribe" },
                    "default_mode": { "type": "string", "enum": ["mentions", "digest", "all", "muted"], "description": "Initial subscription mode (default: mentions)" },
                    "max_hops": { "type": "integer", "description": "Max automatic agent replies before pausing notifications (default: 8)" }
                },
                "required": ["title"]
            }
        }),
        json!({
            "name": "subscribe",
            "description": "Subscribe this agent to a forum thread with a notification mode (mentions, digest, all, muted).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "thread_id": { "type": "string" },
                    "mode": { "type": "string", "enum": ["mentions", "digest", "all", "muted"], "default": "mentions" }
                },
                "required": ["thread_id"]
            }
        }),
        json!({
            "name": "unsubscribe",
            "description": "Unsubscribe this agent from a forum thread.",
            "inputSchema": {
                "type": "object",
                "properties": { "thread_id": { "type": "string" } },
                "required": ["thread_id"]
            }
        }),
        json!({
            "name": "check_inbox",
            "description": "Check unread DMs and forum @mentions/updates for this agent.",
            "inputSchema": { "type": "object", "properties": {} }
        }),
        json!({
            "name": "ack_inbox",
            "description": "Mark a specific thread read up to a given message sequence number.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "thread_id": { "type": "string" },
                    "up_to_seq": { "type": "integer" }
                },
                "required": ["thread_id", "up_to_seq"]
            }
        }),
        json!({
            "name": "ack_all_inbox",
            "description": "Mark all subscribed threads read for this agent to clear all unread hook reminders.",
            "inputSchema": { "type": "object", "properties": {} }
        }),
        json!({
            "name": "wait_for_message",
            "description": "Block and wait for an incoming direct message or forum mention without busy-polling. Returns immediately when a message is received or when timeout expires.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "timeout_secs": {
                        "type": "integer",
                        "description": "Maximum seconds to wait for a message (default: 30, max: 300)",
                        "default": 30
                    }
                }
            }
        }),
    ];
    tools.extend(crate::v2_mcp::v2_mcp_tool_definitions());
    Value::Array(tools)
}

pub fn resource_definitions() -> Vec<Value> {
    vec![
        json!({
            "uri": "coom://inbox",
            "name": "Unread Inbox Messages",
            "description": "Unread direct messages and subscribed forum updates for this agent",
            "mimeType": "application/json"
        }),
        json!({
            "uri": "coom://status",
            "name": "Hub Status & Registered Agents",
            "description": "Real-time hub health, global pause state, and registered agents across runtimes",
            "mimeType": "application/json"
        }),
        json!({
            "uri": "coom://threads",
            "name": "Active Forum & DM Threads",
            "description": "All active discussion threads and their subscription states",
            "mimeType": "application/json"
        }),
    ]
}

async fn handle_resource_read(
    cfg: &Config,
    client: &Client,
    my_address: &str,
    uri: &str,
) -> Result<Value> {
    let base = cfg.base_url();
    let text_content = match uri {
        "coom://inbox" => {
            let v: Value = client
                .get(format!("{base}/api/agents/{my_address}/inbox"))
                .send()
                .await?
                .json()
                .await?;
            serde_json::to_string_pretty(&v)?
        }
        "coom://status" => {
            let snap: Value = client
                .get(format!("{base}/api/snapshot"))
                .send()
                .await?
                .json()
                .await?;
            serde_json::to_string_pretty(&snap)?
        }
        "coom://threads" => {
            let threads: Value = client
                .get(format!("{base}/api/threads"))
                .query(&[("reader", my_address)])
                .send()
                .await?
                .json()
                .await?;
            serde_json::to_string_pretty(&threads)?
        }
        other => return Err(anyhow!("Unsupported resource URI: {other}")),
    };

    Ok(json!({
        "contents": [
            {
                "uri": uri,
                "mimeType": "application/json",
                "text": text_content
            }
        ]
    }))
}
