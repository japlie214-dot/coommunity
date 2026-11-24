use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Config {
    pub host: String,
    pub port: u16,
    pub data_dir: PathBuf,
    pub db_path: PathBuf,
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

        Self {
            host,
            port,
            data_dir,
            db_path,
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
