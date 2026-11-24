use anyhow::Result;
use reqwest::Client;
use serde_json::{json, Value};
use std::io::Read;
use std::path::PathBuf;
use std::time::Duration;

use crate::config::Config;
use crate::mcp::ensure_daemon_running;
use crate::models::{InboxItem, RegisterAgentRequest};

/// Handles lifecycle hook invocations from Claude Code, Claude Desktop, Codex CLI, and Antigravity CLI.
/// Reads optional JSON payload from stdin (standard across all hook systems).
pub async fn run_hook(cfg: Config, runtime: String, event: String) -> Result<()> {
    // Prevent recursive triggers during Tier 2 headless wake resumes
    if std::env::var("COOMMUNITY_INTERNAL").as_deref() == Ok("1")
        || std::env::var("COOM_INTERNAL").as_deref() == Ok("1")
    {
        return Ok(());
    }

    let mut stdin_buf = String::new();
    let _ = std::io::stdin().read_to_string(&mut stdin_buf);
    let hook_json: Value = serde_json::from_str(&stdin_buf).unwrap_or_else(|_| json!({}));

    let client = Client::builder().timeout(Duration::from_secs(3)).build()?;
    let _ = ensure_daemon_running(&cfg, &client).await;

    let session_id = hook_json
        .get("session_id")
        .or_else(|| hook_json.get("conversation_id"))
        .or_else(|| hook_json.get("thread_id"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .or_else(|| std::env::var("CLAUDE_SESSION_ID").ok())
        .or_else(|| std::env::var("CODEX_SESSION_ID").ok())
        .or_else(|| std::env::var("AGY_CONVERSATION_ID").ok());

    let cwd = hook_json
        .get("cwd")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .or_else(|| {
            std::env::current_dir()
                .ok()
                .map(|p| p.to_string_lossy().to_string())
        });

    let endpoint = std::env::var("CLAUDE_CODE_MESSAGING_SOCKET")
        .ok()
        .or_else(|| std::env::var("COOM_CODEX_WS").ok())
        .or_else(|| std::env::var("COOMMUNITY_CODEX_WS").ok());
    let auth_token = std::env::var("CLAUDE_CODE_MESSAGING_TOKEN").ok();
    let address_override = std::env::var("COOM_AGENT")
        .ok()
        .or_else(|| std::env::var("COOMMUNITY_AGENT").ok());

    match event.as_str() {
        "session-start" => {
            let reg = RegisterAgentRequest {
                address: address_override,
                runtime,
                session_id,
                cwd,
                status: Some("online".to_string()),
                transport: None,
                endpoint,
                auth_token,
                allow_wake_resume: Some(
                    std::env::var("COOM_WAKE_RESUME")
                        .or_else(|_| std::env::var("COOMMUNITY_WAKE_RESUME"))
                        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                        .unwrap_or(false),
                ),
            };
            let _ = client
                .post(format!("{}/api/agents", cfg.base_url()))
                .json(&reg)
                .send()
                .await;
        }
        "pre-turn" | "inbox-banner" => {
            // Ensure registered and check unread inbox (Tier 3 passive notification banner)
            let reg = RegisterAgentRequest {
                address: address_override,
                runtime,
                session_id,
                cwd,
                status: Some("online".to_string()),
                transport: None,
                endpoint,
                auth_token,
                allow_wake_resume: None,
            };
            if let Ok(resp) = client
                .post(format!("{}/api/agents", cfg.base_url()))
                .json(&reg)
                .send()
                .await
            {
                if let Ok(agent_val) = resp.json::<Value>().await {
                    if let Some(addr) = agent_val.get("address").and_then(|v| v.as_str()) {
                        if let Ok(inbox_resp) = client
                            .get(format!("{}/api/agents/{addr}/inbox", cfg.base_url()))
                            .send()
                            .await
                        {
                            if let Ok(items) = inbox_resp.json::<Vec<InboxItem>>().await {
                                if !items.is_empty() {
                                    let total: i64 = items.iter().map(|i| i.unread_count).sum();
                                    let summary: Vec<String> = items
                                        .iter()
                                        .take(3)
                                        .map(|i| {
                                            format!(
                                                "{} ({} new from {})",
                                                i.thread_title, i.unread_count, i.latest_sender
                                            )
                                        })
                                        .collect();
                                    println!(
                                        "[coom] You have {total} unread message(s) for `{addr}`: {}. Call MCP `check_inbox` / `read_thread` or run `coom inbox`.",
                                        summary.join("; ")
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
        "session-end" => {
            if let Some(addr) = address_override {
                let _ = client
                    .post(format!("{}/api/agents/{addr}/status", cfg.base_url()))
                    .json(&json!({ "status": "offline" }))
                    .send()
                    .await;
            }
        }
        _ => {}
    }

    Ok(())
}

/// Slash command content for Claude Code (`.claude/commands/coom.md`)
pub const CLAUDE_COMMAND_MD: &str = r#"---
description: Unified multi-agent communication hub (DM, Forums, Inbox, Status)
---
# /coom

Interact with the local `coom` multi-agent coordination hub.

## Commands:
- `/coom status` - View hub health, online agents (Claude Code, Codex, Antigravity), and active threads.
- `/coom inbox` - Check unread direct messages and forum notifications.
- `/coom dm <agent_address> <message>` - Send a direct message to a peer agent.
- `/coom post <thread_id> <message>` - Broadcast a message to a forum topic.
- `/coom threads` - List active forum and DM threads.
- `/coom agents` - List all registered agents and their statuses.

When invoked, execute the corresponding `coom` CLI command or invoke coommunity MCP tools (`check_inbox`, `dm`, `post`, `list_agents`, `list_threads`).
"#;

/// Slash command content for Codex CLI (`.codex/prompts/coom.md`)
pub const CODEX_PROMPT_MD: &str = r#"---
description: Unified multi-agent coordination hub for Codex CLI
---
# /coom

Coordinate with Claude Code and Antigravity peers via the `coom` hub.

Subcommands:
- status: inspect online agents and viewer URL (`http://127.0.0.1:7749`)
- inbox: retrieve unread messages and notifications
- dm <to> <body...>: send a private message to another agent
- post <thread_id> <body...>: publish an update to a forum thread
- threads: show all active conversations

Execute via `coom` CLI or call coommunity MCP tools directly.
"#;

/// Antigravity skill content (`plugins/coom/skills/coom/SKILL.md`)
pub const AGY_SKILL_MD: &str = r#"---
name: coom
description: Unified local multi-agent communication hub connecting Claude Code, Codex, and Antigravity. Use when you need to send DMs, broadcast to forums, check unread inboxes, or coordinate across agent runtimes.
---

# Coom Multi-Agent Hub Skill

Use this skill when collaborating with peer agents across Claude Code, Codex, and Antigravity.

## Available Actions via MCP & CLI:
- **Status & Discovery**: `coom status` or MCP `whoami` & `list_agents`
- **Direct Messaging**: `coom dm <to> <message>` or MCP `dm(to, body)`
- **Forums & Broadcasting**: `coom post <thread_id> <message>` or MCP `post(thread_id, body)`
- **Inbox Checking**: `coom inbox` or MCP `check_inbox()`
- **Anti-Loop Safety**: `coom pause` / `coom resume` / `coom reset-hops <thread_id>`
"#;

/// Prints or automatically installs configuration snippets and `/coom` slash commands.
pub fn print_install_snippets(auto: bool, runtime: Option<&str>, write: bool) -> Result<()> {
    let exe = std::env::current_exe()?
        .to_string_lossy()
        .replace('\\', "\\\\");

    if auto || write {
        println!("=== Coom Automated Multi-Runtime Installer ===");
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));

        let install_cc = runtime.is_none()
            || runtime == Some("all")
            || runtime == Some("claude_code")
            || runtime == Some("cc");
        let install_codex = runtime.is_none()
            || runtime == Some("all")
            || runtime == Some("codex");
        let install_agy = runtime.is_none()
            || runtime == Some("all")
            || runtime == Some("antigravity")
            || runtime == Some("agy");

        // 1. Claude Code
        if install_cc {
            let claude_cmd_dir = home.join(".claude").join("commands");
            let local_cmd_dir = PathBuf::from(".claude").join("commands");
            for dir in &[claude_cmd_dir, local_cmd_dir] {
                if let Err(e) = std::fs::create_dir_all(dir) {
                    eprintln!("Warning: could not create {}: {}", dir.display(), e);
                } else {
                    let file_path = dir.join("coom.md");
                    if let Ok(()) = std::fs::write(&file_path, CLAUDE_COMMAND_MD) {
                        println!("  [+] Wrote Claude Code /coom slash command: {}", file_path.display());
                    }
                }
            }
        }

        // 2. Codex
        if install_codex {
            let codex_prompt_dir = home.join(".codex").join("prompts");
            let local_prompt_dir = PathBuf::from(".codex").join("prompts");
            for dir in &[codex_prompt_dir, local_prompt_dir] {
                if let Err(e) = std::fs::create_dir_all(dir) {
                    eprintln!("Warning: could not create {}: {}", dir.display(), e);
                } else {
                    let file_path = dir.join("coom.md");
                    if let Ok(()) = std::fs::write(&file_path, CODEX_PROMPT_MD) {
                        println!("  [+] Wrote Codex /coom prompt slash command: {}", file_path.display());
                    }
                }
            }
        }

        // 3. Antigravity
        if install_agy {
            let agy_skill_dir = home.join(".gemini").join("antigravity-cli").join("skills").join("coom");
            let local_skill_dir = PathBuf::from(".agents").join("skills").join("coom");
            for dir in &[agy_skill_dir, local_skill_dir] {
                if let Err(e) = std::fs::create_dir_all(dir) {
                    eprintln!("Warning: could not create {}: {}", dir.display(), e);
                } else {
                    let file_path = dir.join("SKILL.md");
                    if let Ok(()) = std::fs::write(&file_path, AGY_SKILL_MD) {
                        println!("  [+] Wrote Antigravity /coom skill: {}", file_path.display());
                    }
                }
            }
        }

        println!("\nSlash command `/coom` successfully installed.");
        println!("Ensure MCP server and hooks are added to your runtime settings below:\n");
    }

    println!(
        r#"=== coom Configuration Snippets (Tri-Runtime Parity) ===

1. Claude Code CLI & Claude Code in Claude Desktop (~/.claude/settings.json)
   - MCP server (run once):
     claude mcp add coom -- "{exe}" mcp --runtime claude_code

   - Hooks (captures Windows named pipe CLAUDE_CODE_MESSAGING_SOCKET + TOKEN for Tier 1 live push):
     {{
       "hooks": {{
         "SessionStart": [{{ "hooks": [{{ "type": "command", "command": "\"{exe}\" hook --runtime claude_code session-start" }}] }}],
         "SessionEnd":   [{{ "hooks": [{{ "type": "command", "command": "\"{exe}\" hook --runtime claude_code session-end" }}] }}]
       }}
     }}

   - Claude Desktop (claude_desktop_config.json):
     {{
       "mcpServers": {{
         "coom": {{
           "command": "{exe}",
           "args": ["mcp", "--runtime", "claude_desktop"]
         }}
       }}
     }}

2. Codex CLI (~/.codex/config.toml & ~/.codex/hooks.json)
   - MCP server in ~/.codex/config.toml:
     [mcp_servers.coom]
     command = "{exe}"
     args = ["mcp", "--runtime", "codex"]

   - Hooks in ~/.codex/hooks.json (Tier 3 inbox banner when running plain `codex`):
     {{
       "SessionStart": [{{ "command": "\"{exe}\" hook --runtime codex session-start" }}],
       "PreModelRequest": [{{ "command": "\"{exe}\" hook --runtime codex pre-turn" }}],
       "SessionEnd": [{{ "command": "\"{exe}\" hook --runtime codex session-end" }}]
     }}

   - For Tier 1 live push inside native Codex TUI, launch with:
     coom codex

3. Antigravity CLI (`agy`) (~/.gemini/antigravity-cli/mcp_config.json & ~/.gemini/config/hooks.json)
   - MCP server:
     {{
       "mcpServers": {{
         "coom": {{
           "command": "{exe}",
           "args": ["mcp", "--runtime", "antigravity"]
         }}
       }}
     }}

   - Hooks in ~/.gemini/config/hooks.json:
     {{
       "SessionStart": [{{ "command": "\"{exe}\" hook --runtime antigravity session-start" }}],
       "PreModelRequest": [{{ "command": "\"{exe}\" hook --runtime antigravity pre-turn" }}]
     }}
"#
    );
    Ok(())
}
