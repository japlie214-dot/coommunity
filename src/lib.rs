pub mod claude_desktop;
pub mod codex_launcher;
pub mod config;
pub mod daemon;
pub mod db;
pub mod dispatcher;
pub mod hooks;
pub mod mcp;
pub mod models;
pub mod v2_daemon;
pub mod v2_db;
pub mod v2_mcp;
pub mod v2_models;
pub mod web_ui;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::time::Duration;
use tracing_subscriber::EnvFilter;

use crate::config::Config;

#[derive(Parser)]
#[command(
    name = "coom",
    version,
    about = "Unified multi-agent hub (Claude Code, Codex, Antigravity) with DMs, forums, live web viewer, and MCP"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Start the local HTTP + SSE daemon and read-only web viewer
    Daemon,

    /// Run the JSON-RPC 2.0 MCP server over stdio (auto-starts daemon if needed)
    Mcp {
        /// Runtime identifier: claude_code, claude_desktop, codex, antigravity
        #[arg(long, default_value = "unknown")]
        runtime: String,
        /// Optional explicit agent address (e.g. cc:backend)
        #[arg(long)]
        address: Option<String>,
    },

    /// Legacy Codex launcher (kept for compatibility; hook reminders work with plain `codex`)
    Codex {
        /// Optional explicit agent address (e.g. codex:tests)
        #[arg(long)]
        name: Option<String>,
        /// Extra arguments forwarded verbatim to `codex --remote`
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },

    /// Lifecycle hook entrypoint called by Claude Code, Codex, or Antigravity
    Hook {
        #[arg(long, default_value = "unknown")]
        runtime: String,
        /// Event name: session-start | pre-turn | session-end
        event: String,
    },

    /// Print ready-to-paste MCP + Hook setup snippets or automatically install slash commands
    Install {
        /// Automatically detect and install /coom slash commands for all installed runtimes
        #[arg(long)]
        auto: bool,
        /// Specific runtime: cc, codex, agy, or all
        #[arg(long)]
        runtime: Option<String>,
        /// Write configuration and slash command files
        #[arg(long)]
        write: bool,
    },

    /// Show live hub status, online agents, and viewer URL
    Status,

    /// Send a direct message to a specific agent address
    Dm {
        /// Target agent address (e.g. codex:backend, cc:tests, agy:qa)
        to: String,
        /// Message text
        body: String,
        /// Optional sender address (defaults to COOM_AGENT or cli:user)
        #[arg(long)]
        from: Option<String>,
        /// Mark as informational update (no reply required)
        #[arg(long)]
        ack_only: bool,
    },

    /// Post a message to a forum thread
    Post {
        /// Target forum thread ID (e.g. forum:release-sync)
        thread_id: String,
        /// Message text
        body: String,
        /// Explicit agent addresses to mention
        #[arg(long, value_delimiter = ',')]
        mentions: Vec<String>,
        /// Optional sender address (defaults to COOM_AGENT or cli:user)
        #[arg(long)]
        sender: Option<String>,
        /// Mark as informational update (no reply required)
        #[arg(long)]
        ack_only: bool,
    },

    /// Inspect unread inbox items or acknowledge messages for an agent
    Inbox {
        /// Agent address to inspect (defaults to COOM_AGENT or agent:unknown)
        #[arg(long)]
        agent: Option<String>,
        /// Thread ID to acknowledge
        #[arg(long)]
        thread_id: Option<String>,
        /// Acknowledge messages up to this sequence number
        #[arg(long)]
        ack_up_to: Option<i64>,
        /// Mark all subscribed threads read for this agent
        #[arg(long)]
        ack_all: bool,
    },

    /// List all active forum and DM threads
    Threads,

    /// List all registered agents and their current status
    Agents,

    /// Pause all hub notifications globally (emergency anti-loop kill-switch)
    Pause,

    /// Resume hub notifications globally
    Resume,

    /// Mute notifications for a specific thread
    Mute { thread_id: String },

    /// Unmute notifications for a specific thread
    Unmute { thread_id: String },

    /// Reset the anti-loop hop counter for a thread
    ResetHops { thread_id: String },
}

pub async fn run_cli() -> Result<()> {
    let cli = Cli::parse();
    let cfg = Config::load();

    // Only emit tracing logs to stderr on non-MCP commands so stdio JSON-RPC stays pristine
    if !matches!(cli.command, Commands::Mcp { .. } | Commands::Hook { .. }) {
        tracing_subscriber::fmt()
            .with_env_filter(
                EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
            )
            .with_writer(std::io::stderr)
            .init();
    }

    match cli.command {
        Commands::Daemon => daemon::run_daemon(cfg).await,
        Commands::Mcp { runtime, address } => mcp::run_mcp_stdio(cfg, runtime, address).await,
        Commands::Codex { name, args } => codex_launcher::run_codex_launcher(cfg, name, args).await,
        Commands::Hook { runtime, event } => hooks::run_hook(cfg, runtime, event).await,
        Commands::Install { auto, runtime, write } => {
            hooks::print_install_snippets(auto, runtime.as_deref(), write)
        }
        Commands::Status => cli_status(&cfg).await,
        Commands::Dm { to, body, from, ack_only } => cli_dm(&cfg, to, body, ack_only, from).await,
        Commands::Post { thread_id, body, mentions, sender, ack_only } => {
            cli_post(&cfg, thread_id, body, mentions, ack_only, sender).await
        }
        Commands::Inbox { agent, thread_id, ack_up_to, ack_all } => {
            cli_inbox(&cfg, agent, thread_id, ack_up_to, ack_all).await
        }
        Commands::Threads => cli_threads(&cfg).await,
        Commands::Agents => cli_agents(&cfg).await,
        Commands::Pause => cli_post_empty(&cfg, "/api/hub/pause", "Hub notifications paused.").await,
        Commands::Resume => {
            cli_post_empty(&cfg, "/api/hub/resume", "Hub notifications resumed.").await
        }
        Commands::Mute { thread_id } => {
            cli_post_empty(
                &cfg,
                &format!("/api/threads/{thread_id}/mute"),
                &format!("Muted thread `{thread_id}`."),
            )
            .await
        }
        Commands::Unmute { thread_id } => {
            cli_post_empty(
                &cfg,
                &format!("/api/threads/{thread_id}/unmute"),
                &format!("Unmuted thread `{thread_id}`."),
            )
            .await
        }
        Commands::ResetHops { thread_id } => {
            cli_post_empty(
                &cfg,
                &format!("/api/threads/{thread_id}/reset-hops"),
                &format!("Reset hop counter for `{thread_id}`."),
            )
            .await
        }
    }
}

pub async fn cli_status(cfg: &Config) -> Result<()> {
    let client = cfg.http_client(Duration::from_secs(3))?;
    let url = format!("{}/api/snapshot", cfg.base_url());
    match client.get(&url).send().await {
        Ok(resp) if resp.status().is_success() => {
            let snap: models::SnapshotResponse = resp.json().await?;
            println!("coom daemon: RUNNING at {}", cfg.base_url());
            println!("Read-only viewer: {}", cfg.base_url());
            println!(
                "State: {} | Agents: {}/{} online | Forums: {} | DMs: {} | Messages: {}",
                if snap.status.paused { "PAUSED" } else { "ACTIVE" },
                snap.status.online_agents,
                snap.status.total_agents,
                snap.status.forum_threads,
                snap.status.dm_threads,
                snap.status.total_messages
            );
            if !snap.agents.is_empty() {
                println!("\nRegistered Agents:");
                for a in snap.agents {
                    println!(
                        "  - {:<24} [{:<14}] {:<7} via {}",
                        a.address,
                        a.runtime.as_str(),
                        a.status.as_str(),
                        a.transport.as_str()
                    );
                }
            }
        }
        _ => {
            println!("coom daemon is not running at {}.", cfg.base_url());
            println!("Start it with: coom daemon");
        }
    }
    Ok(())
}

pub async fn cli_dm(
    cfg: &Config,
    to: String,
    body: String,
    ack_only: bool,
    from: Option<String>,
) -> Result<()> {
    let client = cfg.http_client(Duration::from_secs(5))?;
    let from_addr = from
        .or_else(|| std::env::var("COOM_AGENT").ok())
        .or_else(|| std::env::var("COOMMUNITY_AGENT").ok())
        .unwrap_or_else(|| "cli:user".to_string());
    let req = models::SendDmRequest {
        from: from_addr,
        to: to.clone(),
        body,
        ack_only,
    };
    let resp = client
        .post(format!("{}/api/dm", cfg.base_url()))
        .json(&req)
        .send()
        .await?;
    if resp.status().is_success() {
        let val: serde_json::Value = resp.json().await?;
        let msg_id = val.get("id").and_then(|v| v.as_str()).unwrap_or("ok");
        let seq = val.get("seq").and_then(|v| v.as_i64()).unwrap_or(1);
        println!("DM queued for `{to}` (seq #{seq}, id: {msg_id})");
    } else {
        eprintln!("Failed to send DM: {}", resp.status());
    }
    Ok(())
}

pub async fn cli_post(
    cfg: &Config,
    thread_id: String,
    body: String,
    mentions: Vec<String>,
    ack_only: bool,
    sender: Option<String>,
) -> Result<()> {
    let client = cfg.http_client(Duration::from_secs(5))?;
    let sender_addr = sender
        .or_else(|| std::env::var("COOM_AGENT").ok())
        .or_else(|| std::env::var("COOMMUNITY_AGENT").ok())
        .unwrap_or_else(|| "cli:user".to_string());
    let req = models::PostMessageRequest {
        sender: sender_addr,
        body,
        mentions,
        ack_only,
    };
    let resp = client
        .post(format!("{}/api/threads/{thread_id}/messages", cfg.base_url()))
        .json(&req)
        .send()
        .await?;
    if resp.status().is_success() {
        let val: serde_json::Value = resp.json().await?;
        let msg_id = val.get("id").and_then(|v| v.as_str()).unwrap_or("ok");
        let seq = val.get("seq").and_then(|v| v.as_i64()).unwrap_or(1);
        println!("Posted message #{seq} to `{thread_id}` (id: {msg_id})");
    } else {
        eprintln!("Failed to post message: {}", resp.status());
    }
    Ok(())
}

pub async fn cli_inbox(
    cfg: &Config,
    agent: Option<String>,
    thread_id: Option<String>,
    ack_up_to: Option<i64>,
    ack_all: bool,
) -> Result<()> {
    let client = cfg.http_client(Duration::from_secs(5))?;
    let agent_addr = agent
        .or_else(|| std::env::var("COOM_AGENT").ok())
        .or_else(|| std::env::var("COOMMUNITY_AGENT").ok())
        .unwrap_or_else(|| "agent:unknown".to_string());

    if ack_all {
        let resp = client
            .post(format!(
                "{}/api/agents/{agent_addr}/inbox/ack-all",
                cfg.base_url()
            ))
            .send()
            .await?;
        if resp.status().is_success() {
            let val: serde_json::Value = resp.json().await.unwrap_or_default();
            let count = val.get("acked_threads").and_then(|v| v.as_u64()).unwrap_or(0);
            println!("Marked all unread threads read for `{agent_addr}` ({count} subscription(s)).");
        } else {
            eprintln!("Ack-all failed: {}", resp.status());
        }
        return Ok(());
    }

    if let (Some(tid), Some(seq)) = (thread_id, ack_up_to) {
        let req = models::AckInboxRequest {
            agent_address: agent_addr.clone(),
            thread_id: tid.clone(),
            up_to_seq: seq,
        };
        let resp = client
            .post(format!("{}/api/inbox/ack", cfg.base_url()))
            .json(&req)
            .send()
            .await?;
        if resp.status().is_success() {
            println!("Acknowledged inbox for `{agent_addr}` on thread `{tid}` up to #{seq}.");
        } else {
            eprintln!("Ack failed: {}", resp.status());
        }
        return Ok(());
    }

    let resp = client
        .get(format!("{}/api/agents/{agent_addr}/inbox", cfg.base_url()))
        .send()
        .await?;
    if resp.status().is_success() {
        let items: Vec<models::InboxItem> = resp.json().await?;
        if items.is_empty() {
            println!("Inbox is empty for `{agent_addr}`.");
        } else {
            let total: i64 = items.iter().map(|i| i.unread_count).sum();
            println!("Inbox for `{agent_addr}` ({total} unread):");
            for item in items {
                let mention_note = if item.mentioned_count > 0 {
                    format!(", {} @mention", item.mentioned_count)
                } else {
                    String::new()
                };
                println!(
                    "  - [{:<5}] {:<24} ({} unread{}, latest #{} from: {})",
                    item.thread_kind.as_str(),
                    item.thread_title,
                    item.unread_count,
                    mention_note,
                    item.latest_seq,
                    item.latest_sender
                );
                println!("    Thread ID: {}", item.thread_id);
            }
        }
    } else {
        eprintln!("Failed to query inbox: {}", resp.status());
    }
    Ok(())
}

pub async fn cli_threads(cfg: &Config) -> Result<()> {
    let client = cfg.http_client(Duration::from_secs(5))?;
    let resp = client
        .get(format!("{}/api/threads", cfg.base_url()))
        .send()
        .await?;
    if resp.status().is_success() {
        let threads: Vec<models::Thread> = resp.json().await?;
        if threads.is_empty() {
            println!("No threads found on hub.");
        } else {
            println!("Active Threads ({}):", threads.len());
            for t in threads {
                let muted_str = if t.muted { " [MUTED]" } else { "" };
                println!(
                    "  - {:<28} [{:<5}] {:<20} (msgs: {}, hops: {}/{}){}",
                    t.id,
                    t.kind.as_str(),
                    t.title,
                    t.message_count,
                    t.hop_count,
                    t.max_hops,
                    muted_str
                );
            }
        }
    } else {
        eprintln!("Failed to list threads: {}", resp.status());
    }
    Ok(())
}

pub async fn cli_agents(cfg: &Config) -> Result<()> {
    let client = cfg.http_client(Duration::from_secs(5))?;
    let resp = client
        .get(format!("{}/api/agents", cfg.base_url()))
        .send()
        .await?;
    if resp.status().is_success() {
        let agents: Vec<models::Agent> = resp.json().await?;
        if agents.is_empty() {
            println!("No agents registered on hub.");
        } else {
            println!("Registered Agents ({}):", agents.len());
            for a in agents {
                println!(
                    "  - {:<24} [{:<14}] {:<7} via {}",
                    a.address,
                    a.runtime.as_str(),
                    a.status.as_str(),
                    a.transport.as_str()
                );
                if let Some(cwd) = a.cwd {
                    println!("    cwd: {}", cwd);
                }
            }
        }
    } else {
        eprintln!("Failed to list agents: {}", resp.status());
    }
    Ok(())
}

pub async fn cli_post_empty(cfg: &Config, path: &str, ok_msg: &str) -> Result<()> {
    let client = cfg.http_client(Duration::from_secs(3))?;
    let resp = client
        .post(format!("{}{}", cfg.base_url(), path))
        .send()
        .await?;
    if resp.status().is_success() {
        println!("{ok_msg}");
    } else {
        eprintln!("Request failed: {}", resp.status());
    }
    Ok(())
}
