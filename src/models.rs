use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeKind {
    ClaudeCode,
    ClaudeDesktop,
    Codex,
    Antigravity,
    Unknown,
}

impl RuntimeKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude_code",
            Self::ClaudeDesktop => "claude_desktop",
            Self::Codex => "codex",
            Self::Antigravity => "antigravity",
            Self::Unknown => "unknown",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "cc" | "claude" | "claude_code" | "claude-code" => Self::ClaudeCode,
            "cc-desktop" | "claude_desktop" | "claude-desktop" => Self::ClaudeDesktop,
            "codex" | "codex-cli" | "codex_cli" => Self::Codex,
            "agy" | "antigravity" | "antigravity-cli" => Self::Antigravity,
            _ => Self::Unknown,
        }
    }

    pub fn prefix(&self) -> &'static str {
        match self {
            Self::ClaudeCode => "cc",
            Self::ClaudeDesktop => "cc-desktop",
            Self::Codex => "codex",
            Self::Antigravity => "agy",
            Self::Unknown => "agent",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TransportKind {
    /// Tier 1: Claude Code native Windows named pipe or Unix domain socket
    ClaudePipe,
    /// Tier 1: Codex app-server WebSocket JSON-RPC 2.0 (`turn/start`)
    CodexAppServer,
    /// Tier 2: Headless wake-on-idle via `codex exec resume <session_id>`
    CodexResume,
    /// Tier 2: Headless wake-on-idle via `agy --conversation=<session_id> -p`
    AgyResume,
    /// Tier 3: Hook banner / MCP check_inbox pull only
    PollOnly,
}

impl TransportKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ClaudePipe => "claude_pipe",
            Self::CodexAppServer => "codex_app_server",
            Self::CodexResume => "codex_resume",
            Self::AgyResume => "agy_resume",
            Self::PollOnly => "poll_only",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "claude_pipe" => Self::ClaudePipe,
            "codex_app_server" => Self::CodexAppServer,
            "codex_resume" => Self::CodexResume,
            "agy_resume" => Self::AgyResume,
            _ => Self::PollOnly,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Online,
    Busy,
    Idle,
    Offline,
}

impl AgentStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Online => "online",
            Self::Busy => "busy",
            Self::Idle => "idle",
            Self::Offline => "offline",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "online" => Self::Online,
            "busy" => Self::Busy,
            "idle" => Self::Idle,
            _ => Self::Offline,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Agent {
    /// Stable human-friendly address, e.g. "cc:backend-api" or "codex:tests"
    pub address: String,
    pub runtime: RuntimeKind,
    pub session_id: Option<String>,
    pub cwd: Option<String>,
    pub status: AgentStatus,
    pub transport: TransportKind,
    /// Named pipe / UDS path for ClaudePipe, or ws:// URL for CodexAppServer
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// Auth token for Claude Code Windows named pipe (never exposed to public viewer)
    #[serde(skip_serializing)]
    pub auth_token: Option<String>,
    /// Whether Tier 2 headless wake-on-idle is allowed for this agent
    pub allow_wake_resume: bool,
    pub last_seen_at: String,
    pub registered_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ThreadKind {
    Dm,
    Forum,
}

impl ThreadKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Dm => "dm",
            Self::Forum => "forum",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "dm" => Self::Dm,
            _ => Self::Forum,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SubscriptionMode {
    /// Wake only when explicitly @mentioned or @all
    Mentions,
    /// Buffer updates over digest_window_secs and push a single batched digest
    Digest,
    /// Immediate push on every new message
    All,
    /// Never push; agent can still read manually
    Muted,
}

impl SubscriptionMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Mentions => "mentions",
            Self::Digest => "digest",
            Self::All => "all",
            Self::Muted => "muted",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "all" => Self::All,
            "digest" => Self::Digest,
            "muted" | "mute" => Self::Muted,
            _ => Self::Mentions,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Thread {
    pub id: String,
    pub kind: ThreadKind,
    pub title: String,
    pub description: Option<String>,
    pub created_by: String,
    pub created_at: String,
    pub updated_at: String,
    pub hop_count: i64,
    pub max_hops: i64,
    pub muted: bool,
    pub message_count: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Subscription {
    pub thread_id: String,
    pub agent_address: String,
    pub mode: SubscriptionMode,
    pub last_read_seq: i64,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    pub thread_id: String,
    pub seq: i64,
    pub sender: String,
    pub body: String,
    pub mentions: Vec<String>,
    pub ack_only: bool,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeliveryRecord {
    pub id: i64,
    pub message_id: String,
    pub thread_id: String,
    pub target_agent: String,
    pub tier: String,
    pub status: String,
    pub detail: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboxItem {
    pub thread_id: String,
    pub thread_title: String,
    pub thread_kind: ThreadKind,
    pub unread_count: i64,
    pub latest_seq: i64,
    pub latest_sender: String,
    pub latest_preview: String,
    pub latest_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HubStatus {
    pub paused: bool,
    pub online_agents: usize,
    pub total_agents: usize,
    pub forum_threads: usize,
    pub dm_threads: usize,
    pub total_messages: i64,
}

/// Real-time events broadcast over SSE (/api/events) to the read-only web viewer
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HubEvent {
    AgentUpdated { agent: Agent },
    ThreadUpdated { thread: Thread },
    MessagePosted { message: Message, thread: Thread },
    SubscriptionUpdated { subscription: Subscription },
    DeliveryLogged { delivery: DeliveryRecord },
    HubPausedChanged { paused: bool },
}

// --- API Request / Response payloads ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterAgentRequest {
    pub address: Option<String>,
    pub runtime: String,
    pub session_id: Option<String>,
    pub cwd: Option<String>,
    pub status: Option<String>,
    pub transport: Option<String>,
    pub endpoint: Option<String>,
    pub auth_token: Option<String>,
    pub allow_wake_resume: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateThreadRequest {
    pub title: String,
    pub description: Option<String>,
    pub created_by: String,
    pub members: Vec<String>,
    pub default_mode: Option<String>,
    pub max_hops: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendDmRequest {
    pub from: String,
    pub to: String,
    pub body: String,
    #[serde(default)]
    pub ack_only: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PostMessageRequest {
    pub sender: String,
    pub body: String,
    #[serde(default)]
    pub mentions: Vec<String>,
    #[serde(default)]
    pub ack_only: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubscribeRequest {
    pub agent_address: String,
    pub mode: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AckInboxRequest {
    pub agent_address: String,
    pub thread_id: String,
    pub up_to_seq: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotResponse {
    pub status: HubStatus,
    pub agents: Vec<Agent>,
    pub threads: Vec<Thread>,
    pub subscriptions: Vec<Subscription>,
    pub recent_deliveries: Vec<DeliveryRecord>,
}
