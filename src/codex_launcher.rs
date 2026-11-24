use anyhow::{Context, Result};
use reqwest::Client;
use std::net::TcpStream;
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::config::{resolve_cli_program, Config};
use crate::mcp::ensure_daemon_running;
use crate::models::RegisterAgentRequest;

/// Hybrid thin launcher for Codex CLI:
/// 1. Ensures `coommunity daemon` is running.
/// 2. Ensures a local `codex app-server --listen ws://127.0.0.1:<port>` is running.
/// 3. Registers this Codex session with `codex_app_server` transport so coommunityd can push
///    live JSON-RPC `turn/start` messages directly into the open TUI.
/// 4. Launches the 100% native Codex TUI via `codex --remote ws://127.0.0.1:<port>`.
pub async fn run_codex_launcher(
    cfg: Config,
    agent_name: Option<String>,
    extra_args: Vec<String>,
) -> Result<()> {
    let client = Client::builder().timeout(Duration::from_secs(5)).build()?;
    ensure_daemon_running(&cfg, &client).await?;

    let ws_url = cfg.codex_ws_url();
    let addr = format!("127.0.0.1:{}", cfg.codex_app_server_port);
    let (codex_prog, codex_prefix) = resolve_cli_program("codex");

    if TcpStream::connect(&addr).is_err() {
        let mut srv = Command::new(&codex_prog);
        srv.args(&codex_prefix)
            .args(["app-server", "--listen", &ws_url])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const DETACHED_PROCESS: u32 = 0x00000008;
            const CREATE_NO_WINDOW: u32 = 0x08000000;
            srv.creation_flags(DETACHED_PROCESS | CREATE_NO_WINDOW);
        }

        srv.spawn()
            .context("failed to launch `codex app-server`; ensure `codex` is on PATH")?;

        for _ in 0..30 {
            tokio::time::sleep(Duration::from_millis(150)).await;
            if TcpStream::connect(&addr).is_ok() {
                break;
            }
        }
    }

    let cwd = std::env::current_dir()
        .ok()
        .map(|p| p.to_string_lossy().to_string());

    let reg = RegisterAgentRequest {
        address: agent_name.clone(),
        runtime: "codex".to_string(),
        session_id: std::env::var("CODEX_SESSION_ID").ok(),
        cwd,
        status: Some("online".to_string()),
        transport: Some("codex_app_server".to_string()),
        endpoint: Some(ws_url.clone()),
        auth_token: None,
        allow_wake_resume: Some(false),
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
        "[coommunity] Launching native Codex TUI attached to {ws_url} as `{resolved_addr}` (viewer: {})",
        cfg.base_url()
    );

    let mut tui = Command::new(&codex_prog);
    tui.args(&codex_prefix)
        .arg("--remote")
        .arg(&ws_url)
        .args(&extra_args)
        .env("COOMMUNITY_AGENT", &resolved_addr)
        .env("COOMMUNITY_CODEX_WS", &ws_url)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    let status = tui
        .status()
        .context("failed to launch `codex --remote`; ensure Codex CLI is installed")?;

    let _ = client
        .post(format!(
            "{}/api/agents/{resolved_addr}/status",
            cfg.base_url()
        ))
        .json(&serde_json::json!({ "status": "offline" }))
        .send()
        .await;

    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
    Ok(())
}
