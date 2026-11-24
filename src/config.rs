use anyhow::{Context, Result};
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION};
use reqwest::Client;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct Config {
    pub host: String,
    pub port: u16,
    pub data_dir: PathBuf,
    pub db_path: PathBuf,
    pub token_path: PathBuf,
    pub default_max_hops: i64,
    pub digest_window_secs: u64,
    pub codex_app_server_port: u16,
}

impl Config {
    pub fn load() -> Self {
        let host = std::env::var("COOM_HOST")
            .or_else(|_| std::env::var("COOMMUNITY_HOST"))
            .unwrap_or_else(|_| "127.0.0.1".to_string());

        let port = std::env::var("COOM_PORT")
            .or_else(|_| std::env::var("COOMMUNITY_PORT"))
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(7749);

        let codex_app_server_port = std::env::var("COOM_CODEX_PORT")
            .or_else(|_| std::env::var("COOMMUNITY_CODEX_PORT"))
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(7750);

        let default_max_hops = std::env::var("COOM_MAX_HOPS")
            .or_else(|_| std::env::var("COOMMUNITY_MAX_HOPS"))
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(8);

        let digest_window_secs = std::env::var("COOM_DIGEST_SECS")
            .or_else(|_| std::env::var("COOMMUNITY_DIGEST_SECS"))
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(15);

        let data_dir = std::env::var("COOM_DATA_DIR")
            .or_else(|_| std::env::var("COOMMUNITY_DATA_DIR"))
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                dirs::home_dir()
                    .unwrap_or_else(|| PathBuf::from("."))
                    .join(".coommunity")
            });

        let db_path = data_dir.join("coommunity.db");
        let token_path = data_dir.join("token");

        Self {
            host,
            port,
            data_dir,
            db_path,
            token_path,
            default_max_hops,
            digest_window_secs,
            codex_app_server_port,
        }
    }

    pub fn base_url(&self) -> String {
        format!("http://{}:{}", self.host, self.port)
    }

    pub fn codex_ws_url(&self) -> String {
        format!("ws://127.0.0.1:{}", self.codex_app_server_port)
    }

    /// Loads or atomically creates the local per-user bearer token stored at `~/.coommunity/token`.
    /// Env vars `COOM_TOKEN` / `COOMMUNITY_TOKEN` take precedence if explicitly set.
    pub fn load_or_create_token(&self) -> Result<String> {
        if let Ok(env_tok) = std::env::var("COOM_TOKEN")
            .or_else(|_| std::env::var("COOMMUNITY_TOKEN"))
        {
            let trimmed = env_tok.trim();
            if !trimmed.is_empty() {
                return Ok(trimmed.to_string());
            }
        }

        if let Ok(existing) = std::fs::read_to_string(&self.token_path) {
            let trimmed = existing.trim();
            if !trimmed.is_empty() {
                return Ok(trimmed.to_string());
            }
        }

        std::fs::create_dir_all(&self.data_dir).with_context(|| {
            format!("failed to create data dir {}", self.data_dir.display())
        })?;

        let generated = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&self.token_path)
        {
            Ok(mut file) => {
                file.write_all(generated.as_bytes())?;
                file.flush()?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = std::fs::set_permissions(
                        &self.token_path,
                        std::fs::Permissions::from_mode(0o600),
                    );
                }
                Ok(generated)
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let existing = std::fs::read_to_string(&self.token_path)?;
                Ok(existing.trim().to_string())
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Builds an HTTP client pre-configured with the local `Authorization: Bearer <token>` header.
    pub fn http_client(&self, timeout: Duration) -> Result<Client> {
        let mut headers = HeaderMap::new();
        if let Ok(token) = self.load_or_create_token() {
            if let Ok(val) = HeaderValue::from_str(&format!("Bearer {token}")) {
                headers.insert(AUTHORIZATION, val);
            }
        }
        Ok(Client::builder()
            .timeout(timeout)
            .default_headers(headers)
            .build()?)
    }
}

/// Resolve CLI executables on Windows (handling both native `.exe` and npm `.cmd` shims)
/// and Unix cleanly.
pub fn resolve_cli_program(name: &str) -> (String, Vec<String>) {
    #[cfg(windows)]
    {
        if let Ok(path_var) = std::env::var("PATH") {
            for dir in std::env::split_paths(&path_var) {
                let exe = dir.join(format!("{name}.exe"));
                if exe.is_file() {
                    return (exe.to_string_lossy().to_string(), Vec::new());
                }
                let cmd = dir.join(format!("{name}.cmd"));
                if cmd.is_file() {
                    return (
                        "cmd.exe".to_string(),
                        vec!["/C".to_string(), cmd.to_string_lossy().to_string()],
                    );
                }
            }
        }
        (
            "cmd.exe".to_string(),
            vec!["/C".to_string(), name.to_string()],
        )
    }
    #[cfg(not(windows))]
    {
        (name.to_string(), Vec::new())
    }
}
