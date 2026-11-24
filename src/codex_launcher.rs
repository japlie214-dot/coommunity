use anyhow::{Context, Result};
use std::process::{Command, Stdio};
use std::time::Duration;
use uuid::Uuid;

use crate::config::{resolve_cli_program, Config};
use crate::mcp::ensure_daemon_running;
use crate::models::RegisterAgentRequest;

/// Thin launcher for Codex CLI that ensures the `coom` daemon is running, registers a
/// unique session ID for hook unread reminders, and launches native `codex`.
pub async fn run_codex_launcher(
    cfg: Config,
    agent_name: Option<String>,
    extra_args: Vec<String>,
) -> Result<()> {
    let client = cfg.http_client(Duration::from_secs(5))?;
    ensure_daemon_running(&cfg, &client).await?;

    let (codex_prog, codex_prefix) = resolve_cli_program("codex");
    let cwd = std::env::current_dir()
        .ok()
        .map(|p| p.to_string_lossy().to_string());
    let session_id = std::env::var("CODEX_SESSION_ID")
        .ok()
        .unwrap_or_else(|| format!("codex-{}", &Uuid::new_v4().to_string()[..8]));

    let reg = RegisterAgentRequest {
        address: agent_name.clone(),
        runtime: "codex".to_string(),
        session_id: Some(session_id.clone()),
        cwd,
        status: Some("online".to_string()),
        transport: Some("poll_only".to_string()),
        endpoint: None,
        auth_token: None,
        allow_wake_resume: None,
    };

    let registered = client
        .post(format!("{}/api/agents", cfg.base_url()))
        .json(&reg)
        .send()
        .await?
        .json::<serde_json::Value>()
        .await
        .ok();

    let resolved_addr = registered
        .as_ref()
        .and_then(|v| v.get("address"))
        .and_then(|v| v.as_str())
        .unwrap_or("codex:session")
        .to_string();

    eprintln!(
        "[coommunity] Launching native Codex TUI as `{resolved_addr}` (hook unread reminders active; viewer: {})",
        cfg.base_url()
    );

    let mut tui = Command::new(&codex_prog);
    tui.args(&codex_prefix)
        .args(&extra_args)
        .env("COOM_AGENT", &resolved_addr)
        .env("COOMMUNITY_AGENT", &resolved_addr)
        .env("CODEX_SESSION_ID", &session_id)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    let status = tui
        .status()
        .context("failed to launch `codex`; ensure Codex CLI is installed")?;

    let _ = client
        .post(format!(
            "{}/api/agents/{resolved_addr}/status",
            cfg.base_url()
        ))
        .json(&serde_json::json!({
            "status": "offline",
            "session_id": session_id
        }))
        .send()
        .await;

    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
    Ok(())
}
