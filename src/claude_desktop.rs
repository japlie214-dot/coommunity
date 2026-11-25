use std::fs;
use std::path::{Path, PathBuf};
use serde::Deserialize;
use crate::models::sanitize_slug;

#[derive(Debug, Clone, Deserialize)]
pub struct ClaudeDesktopSessionInfo {
    #[serde(rename = "sessionId", default)]
    pub session_id: String,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ResolvedDesktopIdentity {
    pub address: String,
    pub session_id: Option<String>,
    pub cwd: Option<String>,
}

/// Resolves the base directory where Claude Desktop stores configuration and session data.
pub fn get_claude_desktop_dir() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        std::env::var("APPDATA").ok().map(|a| PathBuf::from(a).join("Claude"))
    }
    #[cfg(target_os = "macos")]
    {
        dirs::home_dir().map(|h| h.join("Library/Application Support/Claude"))
    }
    #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
    {
        dirs::config_dir().map(|c| c.join("Claude"))
    }
}

/// Inspects Claude Desktop's local session store and resolves the currently active session identity.
pub fn detect_active_session() -> Option<ResolvedDesktopIdentity> {
    let claude_dir = get_claude_desktop_dir()?;
    detect_active_session_in(&claude_dir)
}

/// Internal helper allowing path parameterization for testing.
pub fn detect_active_session_in(claude_dir: &Path) -> Option<ResolvedDesktopIdentity> {
    // 1. Try to read active session ID from claude_desktop_config.json
    let config_path = claude_dir.join("claude_desktop_config.json");
    let mut preferred_session_id = None;
    if config_path.is_file() {
        if let Ok(content) = fs::read_to_string(&config_path) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&content) {
                if let Some(id) = v
                    .pointer("/preferences/epitaxyPrefs/desktop-frame.paneStore.v1/state/lastPrimaryCodeSession/id")
                    .and_then(|id| id.as_str())
                {
                    preferred_session_id = Some(id.to_string());
                }
            }
        }
    }

    let sessions_root = claude_dir.join("claude-code-sessions");
    if !sessions_root.is_dir() {
        return None;
    }

    // 2. Scan for session JSON files
    let mut candidates: Vec<(PathBuf, std::time::SystemTime)> = Vec::new();
    find_session_files(&sessions_root, &mut candidates);

    if candidates.is_empty() {
        return None;
    }

    // Sort by LastWriteTime descending (most recent first)
    candidates.sort_by(|a, b| b.1.cmp(&a.1));

    // If preferred_session_id is known, check if it exists in candidates
    let chosen_path = if let Some(pref_id) = preferred_session_id {
        let expected_file_name = if pref_id.starts_with("local_") {
            format!("{}.json", pref_id)
        } else {
            format!("local_{}.json", pref_id)
        };
        candidates
            .iter()
            .find(|(p, _)| p.file_name().and_then(|n| n.to_str()) == Some(&expected_file_name))
            .map(|(p, _)| p.clone())
            .unwrap_or_else(|| candidates[0].0.clone())
    } else {
        candidates[0].0.clone()
    };

    // Read and parse session info
    let content = fs::read_to_string(&chosen_path).ok()?;
    let session: ClaudeDesktopSessionInfo = serde_json::from_str(&content).ok()?;

    let (address, cwd) = derive_identity_from_session(&session);
    Some(ResolvedDesktopIdentity {
        address,
        session_id: if session.session_id.is_empty() {
            None
        } else {
            Some(session.session_id)
        },
        cwd,
    })
}

fn find_session_files(dir: &Path, out: &mut Vec<(PathBuf, std::time::SystemTime)>) {
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                find_session_files(&path, out);
            } else if path.is_file() {
                if let Some(file_name) = path.file_name().and_then(|n| n.to_str()) {
                    if file_name.starts_with("local_") && file_name.ends_with(".json") {
                        let mtime = entry
                            .metadata()
                            .and_then(|m| m.modified())
                            .unwrap_or(std::time::UNIX_EPOCH);
                        out.push((path, mtime));
                    }
                }
            }
        }
    }
}

pub fn derive_identity_from_session(
    session: &ClaudeDesktopSessionInfo,
) -> (String, Option<String>) {
    let raw_cwd = session.cwd.as_deref().map(str::trim).filter(|s| !s.is_empty());

    // Prefer folder name from cwd
    if let Some(cwd_str) = raw_cwd {
        let path = Path::new(cwd_str);
        if let Some(folder_name) = path.file_name().and_then(|n| n.to_str()) {
            let slug = sanitize_slug(folder_name);
            if !slug.is_empty() && slug != "system32" && slug != "windows" {
                return (format!("cc:{}", slug), Some(cwd_str.to_string()));
            }
        }
    }

    // Fallback to sanitized title
    if let Some(title) = session.title.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        let slug = sanitize_slug(title);
        if !slug.is_empty() {
            return (format!("cc:{}", slug), raw_cwd.map(str::to_string));
        }
    }

    // Fallback to session ID slug
    if !session.session_id.is_empty() {
        let clean_id = sanitize_slug(&session.session_id);
        let short_id = if clean_id.len() > 8 { &clean_id[..8] } else { &clean_id };
        return (format!("cc:desktop:{}", short_id), raw_cwd.map(str::to_string));
    }

    ("cc:desktop".to_string(), raw_cwd.map(str::to_string))
}
