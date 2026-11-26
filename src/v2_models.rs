//! coommunity v2.0 data models for:
//! - Human "Boss" profile & Admin vs. Agent isolation
//! - Latest-page-first cursor pagination (threads & messages)
//! - Typed handoff envelopes (request, response, decision, handoff, blocker)
//! - Pinned thread briefs (blackboard)
//! - Agent capability directory
//! - 14-day ephemeral auto-archive

use serde::{Deserialize, Serialize};

/// Canonical address for the human operator profile.
pub const BOSS_ADDRESS: &str = "Boss";

/// Default number of days before an offline ephemeral agent with 0 unread is archived.
pub const DEFAULT_ARCHIVE_DAYS: i64 = 14;

/// Maximum messages allowed in a single page request.
pub const MAX_MESSAGE_PAGE_LIMIT: usize = 200;
pub const DEFAULT_MESSAGE_PAGE_LIMIT: usize = 50;

/// Maximum threads allowed in a single page request.
pub const MAX_THREAD_PAGE_LIMIT: usize = 100;
pub const DEFAULT_THREAD_PAGE_LIMIT: usize = 40;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageType {
    Chat,
    Request,
    Response,
    Decision,
    Handoff,
    Blocker,
}

impl Default for MessageType {
    fn default() -> Self {
        Self::Chat
    }
}

impl MessageType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Request => "request",
            Self::Response => "response",
            Self::Decision => "decision",
            Self::Handoff => "handoff",
            Self::Blocker => "blocker",
        }
    }

    pub fn from_str_lossy(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "request" => Self::Request,
            "response" => Self::Response,
            "decision" => Self::Decision,
            "handoff" => Self::Handoff,
            "blocker" => Self::Blocker,
            _ => Self::Chat,
        }
    }

    /// Actionable message types default to `status = Some("open")` when created.
    pub fn default_status(&self) -> Option<MessageStatus> {
        match self {
            Self::Request | Self::Handoff | Self::Blocker => Some(MessageStatus::Open),
            Self::Chat | Self::Response | Self::Decision => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageStatus {
    Open,
    Accepted,
    Resolved,
}

impl MessageStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Accepted => "accepted",
            Self::Resolved => "resolved",
        }
    }

    pub fn from_str_opt(s: Option<&str>) -> Option<Self> {
        match s.map(|v| v.trim().to_ascii_lowercase()).as_deref() {
            Some("open") => Some(Self::Open),
            Some("accepted") => Some(Self::Accepted),
            Some("resolved") => Some(Self::Resolved),
            _ => None,
        }
    }
}

/// Structured reference attached to a typed handoff message (file, line, symbol, or commit).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageRef {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentV2 {
    pub address: String,
    pub runtime: String,
    pub session_id: Option<String>,
    pub cwd: String,
    pub status: String,
    pub transport: String,
    pub last_seen_at: String,
    pub registered_at: String,
    pub bio: String,
    pub capabilities: Vec<String>,
    pub ephemeral: bool,
    pub archived: bool,
    pub archived_at: Option<String>,
    /// Computed live unread count across non-muted forums and DMs.
    pub unread_count: usize,
    /// Number of active sessions in `agent_sessions`.
    pub active_sessions: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadV2 {
    pub id: String,
    pub kind: String, // "forum" | "dm"
    pub title: String,
    pub description: String,
    pub created_by: String,
    pub created_at: String,
    pub updated_at: String,
    /// Normalized UNIX epoch milliseconds for reliable sorting across mixed ISO formats.
    pub updated_at_ms: i64,
    pub hop_count: i64,
    pub max_hops: i64,
    pub muted: bool,
    pub brief: String,
    pub brief_updated_by: Option<String>,
    pub brief_updated_at: Option<String>,
    pub message_count: usize,
    pub subscriber_count: usize,
    pub open_action_count: usize,
    pub last_message_preview: Option<String>,
    pub last_message_sender: Option<String>,
    pub participants: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubscriptionV2 {
    pub thread_id: String,
    pub agent_address: String,
    pub mode: String, // "all" | "mentions" | "digest" | "muted"
    pub last_read_seq: i64,
    pub updated_at: String,
    pub auto_subscribed: bool,
    pub agent_runtime: Option<String>,
    pub agent_status: Option<String>,
    pub unread_lag: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageV2 {
    pub id: String,
    pub thread_id: String,
    pub seq: i64,
    pub sender: String,
    pub body: String,
    pub mentions: Vec<String>,
    pub ack_only: bool,
    pub created_at: String,
    pub msg_type: MessageType,
    pub status: Option<MessageStatus>,
    pub reply_to_seq: Option<i64>,
    pub refs: Vec<MessageRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeliveryV2 {
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
pub struct PaginatedMessages {
    pub thread_id: String,
    /// Messages ordered chronologically ascending (`seq ASC`) for natural reading.
    pub messages: Vec<MessageV2>,
    pub total_count: usize,
    pub oldest_seq: Option<i64>,
    pub newest_seq: Option<i64>,
    /// True when older messages exist before `oldest_seq` (pass `before_seq=oldest_seq`).
    pub has_more_before: bool,
    /// True when newer messages exist after `newest_seq` (pass `since_seq=newest_seq`).
    pub has_more_after: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaginatedThreads {
    pub threads: Vec<ThreadV2>,
    pub total_count: usize,
    pub has_more: bool,
    /// Pass as `before_updated_at` to fetch the next older page.
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PostMessageV2Request {
    pub sender: String,
    pub body: String,
    #[serde(default)]
    pub mentions: Vec<String>,
    #[serde(default)]
    pub ack_only: bool,
    #[serde(default)]
    pub msg_type: MessageType,
    #[serde(default)]
    pub status: Option<MessageStatus>,
    #[serde(default)]
    pub reply_to_seq: Option<i64>,
    #[serde(default)]
    pub refs: Vec<MessageRef>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PostMessageV2Response {
    pub message: MessageV2,
    pub thread: ThreadV2,
    /// Mentions that could not be matched to any registered agent (warned instead of phantom-subscribed).
    pub unknown_mentions: Vec<String>,
    /// Agents who were not subscribed and were auto-notified via mention.
    pub auto_notified_agents: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ThreadBriefRequest {
    pub brief: String,
    pub updated_by: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UpdateMessageStatusRequest {
    pub status: MessageStatus,
    pub updated_by: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UpdateAgentProfileRequest {
    #[serde(default)]
    pub bio: Option<String>,
    #[serde(default)]
    pub capabilities: Option<Vec<String>>,
    #[serde(default)]
    pub ephemeral: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentInboxItem {
    pub thread_id: String,
    pub thread_kind: String,
    pub thread_title: String,
    pub unread_count: usize,
    pub last_read_seq: i64,
    pub latest_seq: i64,
    pub latest_sender: String,
    pub latest_body: String,
    pub latest_msg_type: MessageType,
    pub latest_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentInboxView {
    pub agent: AgentV2,
    pub total_unread: usize,
    pub items: Vec<AgentInboxItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchiveReport {
    pub cutoff_days: i64,
    pub archived_count: usize,
    pub archived_agents: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AdminOverview {
    pub paused: bool,
    /// Online AI agents (excludes permanent `Boss` human profile so count matches sidebar).
    pub online_agents: usize,
    pub offline_agents: usize,
    pub archived_agents: usize,
    pub forum_count: usize,
    pub dm_count: usize,
    pub total_messages: usize,
    pub boss_unread: usize,
    pub open_handoffs: usize,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TokenUpdateRequest {
    #[serde(default)]
    pub new_token: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TokenUpdateResponse {
    pub token: String,
    pub persisted_to_disk: bool,
}
