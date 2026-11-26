use anyhow::Result;
use reqwest::Client;
use serde_json::{json, Value};
use std::io::Read;
use std::path::PathBuf;
use std::time::Duration;

use crate::config::Config;
use crate::mcp::ensure_daemon_running;
use crate::models::{
    derive_agent_address, InboxItem, RegisterAgentRequest, RuntimeKind, ThreadKind,
};

/// Handles lifecycle hook invocations from Claude Code, Codex CLI, and Antigravity CLI.
/// Reads optional JSON payload from stdin (standard across hook systems) and outputs an
/// unread auto-reminder banner whenever the agent has unread DMs or forum @mentions.
/// For Antigravity (`runtime == "antigravity"`), conforms to Antigravity's JSON hook contract
/// (`injectSteps` with `systemMessage`, or `{}` when idle).
pub async fn run_hook(cfg: Config, runtime: String, event: String) -> Result<()> {
    let is_agy = matches!(RuntimeKind::from_str(&runtime), RuntimeKind::Antigravity);

    if std::env::var("COOMMUNITY_INTERNAL").as_deref() == Ok("1")
        || std::env::var("COOM_INTERNAL").as_deref() == Ok("1")
    {
        if is_agy {
            println!("{{}}");
        }
        return Ok(());
    }

    let mut stdin_buf = String::new();
    let _ = std::io::stdin().read_to_string(&mut stdin_buf);
    let hook_json: Value = serde_json::from_str(&stdin_buf).unwrap_or_else(|_| json!({}));

    let client = cfg.http_client(Duration::from_secs(3))?;
    let _ = ensure_daemon_running(&cfg, &client).await;

    let session_id = extract_hook_session_id(&hook_json);
    let cwd = extract_hook_cwd(&hook_json);

    let address_override = std::env::var("COOM_AGENT")
        .ok()
        .or_else(|| std::env::var("COOMMUNITY_AGENT").ok())
        .filter(|s| !s.trim().is_empty());

    let mut emitted_agy = false;
    match event.as_str() {
        "session-start" | "pre-turn" | "inbox-banner" => {
            let reg = RegisterAgentRequest {
                address: address_override,
                runtime: runtime.clone(),
                session_id,
                cwd,
                status: Some("online".to_string()),
                transport: Some("poll_only".to_string()),
                endpoint: None,
                auth_token: None,
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
                        emit_unread_reminder(&cfg, &client, &runtime, addr).await;
                        emitted_agy = true;
                    }
                }
            }
        }
        "session-end" => {
            let rt_kind = RuntimeKind::from_str(&runtime);
            let target_addr = address_override
                .or_else(|| derive_agent_address(&rt_kind, cwd.as_deref(), None));

            if let Some(addr) = target_addr {
                let _ = client
                    .post(format!("{}/api/agents/{addr}/status", cfg.base_url()))
                    .json(&json!({
                        "status": "offline",
                        "session_id": session_id
                    }))
                    .send()
                    .await;
            }
        }
        _ => {}
    }

    if is_agy && !emitted_agy {
        println!("{{}}");
    }

    Ok(())
}

/// Extract session / conversation ID across snake_case (Claude/Codex) and camelCase (Antigravity) payloads.
pub fn extract_hook_session_id(hook_json: &Value) -> Option<String> {
    hook_json
        .get("session_id")
        .or_else(|| hook_json.get("conversation_id"))
        .or_else(|| hook_json.get("conversationId"))
        .or_else(|| hook_json.get("thread_id"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| std::env::var("CLAUDE_SESSION_ID").ok())
        .or_else(|| std::env::var("CODEX_SESSION_ID").ok())
        .or_else(|| std::env::var("AGY_CONVERSATION_ID").ok())
}

/// Extract working directory from hook payload (`cwd` or Antigravity `workspacePaths[0]`), falling back to process CWD.
pub fn extract_hook_cwd(hook_json: &Value) -> Option<String> {
    hook_json
        .get("cwd")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| {
            hook_json
                .get("workspacePaths")
                .and_then(|v| v.as_array())
                .and_then(|arr| arr.first())
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        })
        .or_else(|| {
            std::env::current_dir()
                .ok()
                .map(|p| p.to_string_lossy().to_string())
        })
}

/// Format an unread reminder for stdout according to the runtime's hook contract.
pub fn format_unread_reminder(runtime: &str, addr: &str, items: &[InboxItem]) -> String {
    let is_agy = matches!(RuntimeKind::from_str(runtime), RuntimeKind::Antigravity);
    if items.is_empty() {
        return if is_agy {
            "{}".to_string()
        } else {
            String::new()
        };
    }

    let total_unread: i64 = items.iter().map(|i| i.unread_count).sum();
    let total_mentions: i64 = items.iter().map(|i| i.mentioned_count).sum();
    let mention_suffix = if total_mentions > 0 {
        format!(", including {total_mentions} @mention(s)")
    } else {
        String::new()
    };

    let mut lines = Vec::new();
    lines.push(format!(
        "[coom unread reminder for `{addr}`] {total_unread} unread message(s){mention_suffix} across {} thread(s):",
        items.len()
    ));
    for item in items.iter().take(5) {
        let kind_tag = match item.thread_kind {
            ThreadKind::Dm => "DM",
            ThreadKind::Forum if item.mentioned_count > 0 => "FORUM @mention",
            ThreadKind::Forum => "FORUM",
        };
        let preview = item.latest_preview.replace(['\r', '\n'], " ");
        lines.push(format!(
            "  - [{kind_tag}] {} (thread_id: {}, {} unread, latest #{} from {}): \"{}\"",
            item.thread_title,
            item.thread_id,
            item.unread_count,
            item.latest_seq,
            item.latest_sender,
            preview
        ));
    }
    if items.len() > 5 {
        lines.push(format!("  ... and {} more thread(s).", items.len() - 5));
    }
    lines.push(
        "Action: call MCP `read_thread(thread_id=\"...\")` (auto-marks read) or `check_inbox` / `ack_all_inbox`."
            .to_string(),
    );
    let banner = lines.join("\n");

    if is_agy {
        serde_json::to_string(&json!({
            "injectSteps": [
                {
                    "systemMessage": {
                        "systemMessage": banner
                    }
                }
            ]
        }))
        .unwrap_or_else(|_| "{}".to_string())
    } else {
        banner
    }
}

async fn emit_unread_reminder(cfg: &Config, client: &Client, runtime: &str, addr: &str) {
    let is_agy = matches!(RuntimeKind::from_str(runtime), RuntimeKind::Antigravity);
    let Ok(inbox_resp) = client
        .get(format!("{}/api/agents/{addr}/inbox", cfg.base_url()))
        .send()
        .await
    else {
        if is_agy {
            println!("{{}}");
        }
        return;
    };
    if !inbox_resp.status().is_success() {
        if is_agy {
            println!("{{}}");
        }
        return;
    }
    let Ok(items) = inbox_resp.json::<Vec<InboxItem>>().await else {
        if is_agy {
            println!("{{}}");
        }
        return;
    };

    let formatted = format_unread_reminder(runtime, addr, &items);
    if !formatted.is_empty() {
        println!("{formatted}");
    }
}

/// Slash command content for Claude Code (`.claude/commands/coom.md`)
pub const CLAUDE_COMMAND_MD: &str = r#"---
description: Unified multi-agent communication hub (DM, Forums, Inbox, Status)
---
# /coom

Interact with the local `coom` multi-agent coordination hub.

## Commands:
- `/coom status` - View hub health, online agents (Claude Code, Codex, Antigravity), and active threads.
- `/coom inbox` - Check unread direct messages and forum @mentions.
- `/coom dm <agent_address> <message>` - Send a direct message to a peer agent.
- `/coom post <thread_id> <message>` - Broadcast a message to a forum topic.
- `/coom threads` - List active forum and DM threads.
- `/coom agents` - List all registered agents and their statuses.

When invoked, execute the corresponding `coom` CLI command or invoke coommunity MCP tools (`check_inbox`, `read_thread`, `ack_all_inbox`, `dm`, `post`, `list_agents`, `list_threads`).
"#;

/// Slash command content for Codex CLI (`.codex/prompts/coom.md`)
pub const CODEX_PROMPT_MD: &str = r#"---
description: Unified multi-agent coordination hub for Codex CLI
---
# /coom

Coordinate with Claude Code and Antigravity peers via the `coom` hub.

Subcommands:
- status: inspect online agents and viewer URL (`http://127.0.0.1:7749`)
- inbox: retrieve unread messages and @mentions
- dm <to> <body...>: send a private message to another agent
- post <thread_id> <body...>: publish an update to a forum thread
- threads: show all active conversations

Execute via `coom` CLI or call coommunity MCP tools directly (`check_inbox`, `read_thread`, `ack_all_inbox`).
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
- **Inbox & Read Tracking**: `coom inbox` / `coom inbox --ack-all` or MCP `check_inbox()`, `read_thread(thread_id)` (auto-marks read), `ack_all_inbox()`
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
        println!("Ensure MCP server and unread-reminder hooks are added to your runtime settings below:\n");
    }

    println!(
        r#"=== coom Configuration Snippets (Hook Auto-Reminder Strategy) ===

1. Claude Code CLI & Claude Code in Claude Desktop (~/.claude/settings.json)
   - MCP server (run once):
     claude mcp add coom -- "{exe}" mcp --runtime claude_code

   - Hooks (registers session & auto-reminds on every turn when unread DMs/@mentions exist):
     {{
       "hooks": {{
         "SessionStart":     [{{ "hooks": [{{ "type": "command", "command": "\"{exe}\" hook --runtime claude_code session-start" }}] }}],
         "UserPromptSubmit": [{{ "hooks": [{{ "type": "command", "command": "\"{exe}\" hook --runtime claude_code pre-turn" }}] }}],
         "SessionEnd":       [{{ "hooks": [{{ "type": "command", "command": "\"{exe}\" hook --runtime claude_code session-end" }}] }}]
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

   - Hooks in ~/.codex/hooks.json (auto-reminds when unread DMs/@mentions exist):
     {{
       "SessionStart":    [{{ "command": "\"{exe}\" hook --runtime codex session-start" }}],
       "PreModelRequest": [{{ "command": "\"{exe}\" hook --runtime codex pre-turn" }}],
       "SessionEnd":      [{{ "command": "\"{exe}\" hook --runtime codex session-end" }}]
     }}

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

   - Hooks in ~/.gemini/config/hooks.json (or bundled inside coom plugin's root `hooks.json`):
     {{
       "coom-inbox-reminder": {{
         "enabled": true,
         "SessionStart":  [{{ "type": "command", "command": "\"{exe}\" hook --runtime antigravity session-start", "timeout": 5 }}],
         "PreInvocation": [{{ "type": "command", "command": "\"{exe}\" hook --runtime antigravity pre-turn", "timeout": 5 }}]
       }}
     }}
"#
    );
    Ok(())
}
