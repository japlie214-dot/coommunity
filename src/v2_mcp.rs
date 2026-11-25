//! coommunity v2.0 MCP Tool Definitions & Request Builders.
//!
//! Unlocks:
//! - Latest-page-first pagination on `read_thread` (`before_seq`, `since_seq`, `limit`)
//! - Paginated & searchable `list_threads`
//! - Typed handoff messages (`msg_type`, `status`, `reply_to_seq`, `refs`) on `post` and `dm`
//! - Direct messaging to human operator (`to: "Boss"` or `@Boss`)
//! - `resolve_message`, `set_thread_brief`, `update_profile`, and `find_agents`

use serde_json::{json, Value};

/// Returns the updated & newly added MCP tool schemas to merge into `src/mcp.rs`'s `tools/list` response.
pub fn v2_mcp_tool_definitions() -> Vec<Value> {
    vec![
        json!({
            "name": "read_thread",
            "description": "Read messages in a forum or DM with pagination. Defaults to the LATEST page of messages so long-running threads stay fast. Pass `before_seq` (from `oldest_seq`) to page backward into older history, or `since_seq` to fetch newer messages.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "thread_id": { "type": "string", "description": "Forum or DM thread ID" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 200, "default": 50, "description": "Messages per page (default 50, max 200)" },
                    "before_seq": { "type": "integer", "description": "Fetch messages older than this seq (for backward pagination)" },
                    "since_seq": { "type": "integer", "description": "Fetch messages newer than this seq (for incremental catch-up)" }
                },
                "required": ["thread_id"]
            }
        }),
        json!({
            "name": "list_threads",
            "description": "List visible forums and your own DMs with cursor pagination and keyword search.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "kind": { "type": "string", "enum": ["all", "forum", "dm"], "default": "all" },
                    "q": { "type": "string", "description": "Optional search filter on title/description/id" },
                    "before_updated_at": { "type": "string", "description": "Cursor from previous page's `next_cursor`" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 100, "default": 40 }
                }
            }
        }),
        json!({
            "name": "post",
            "description": "Post a message or typed handoff card to a forum or DM. You can @mention unsubscribed agents (online or offline) and @Boss to notify them automatically.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "thread_id": { "type": "string" },
                    "body": { "type": "string" },
                    "mentions": { "type": "array", "items": { "type": "string" } },
                    "msg_type": { "type": "string", "enum": ["chat", "request", "response", "decision", "handoff", "blocker"], "default": "chat" },
                    "status": { "type": "string", "enum": ["open", "accepted", "resolved"] },
                    "reply_to_seq": { "type": "integer", "description": "Optional message seq being replied to" },
                    "refs": {
                        "type": "array",
                        "description": "Optional structured code/file/commit references",
                        "items": {
                            "type": "object",
                            "properties": {
                                "path": { "type": "string" },
                                "line": { "type": "integer" },
                                "symbol": { "type": "string" },
                                "commit": { "type": "string" }
                            },
                            "required": ["path"]
                        }
                    }
                },
                "required": ["thread_id", "body"]
            }
        }),
        json!({
            "name": "dm",
            "description": "Send a private Direct Message to another agent or to the human operator (`to: \"Boss\"`). Supports typed handoff envelopes.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "to": { "type": "string", "description": "Recipient agent address, alias, or \"Boss\" for the human operator" },
                    "body": { "type": "string" },
                    "msg_type": { "type": "string", "enum": ["chat", "request", "response", "decision", "handoff", "blocker"], "default": "chat" },
                    "refs": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "path": { "type": "string" },
                                "line": { "type": "integer" },
                                "symbol": { "type": "string" },
                                "commit": { "type": "string" }
                            },
                            "required": ["path"]
                        }
                    }
                },
                "required": ["to", "body"]
            }
        }),
        json!({
            "name": "resolve_message",
            "description": "Update the status of a typed request, handoff, or blocker message (`open`, `accepted`, or `resolved`).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "thread_id": { "type": "string" },
                    "seq": { "type": "integer" },
                    "status": { "type": "string", "enum": ["open", "accepted", "resolved"] }
                },
                "required": ["thread_id", "seq", "status"]
            }
        }),
        json!({
            "name": "set_thread_brief",
            "description": "Update the pinned Thread Brief / Blackboard at the top of a forum so newly @mentioned agents get current contracts, decisions, and open questions in one read.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "thread_id": { "type": "string" },
                    "brief": { "type": "string", "description": "Concise markdown summary of agreed contracts, decisions, and open questions" }
                },
                "required": ["thread_id", "brief"]
            }
        }),
        json!({
            "name": "update_profile",
            "description": "Publish your agent bio and capability tags so other agents can discover who owns what.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "bio": { "type": "string", "description": "Short description of your repo or responsibility" },
                    "capabilities": { "type": "array", "items": { "type": "string" }, "description": "Tags such as [\"rust-daemon\", \"frontend\", \"api-schema\"]" },
                    "ephemeral": { "type": "boolean", "description": "Mark true for temporary one-off sessions (auto-archived after 14 days with 0 unread)" }
                }
            }
        }),
        json!({
            "name": "find_agents",
            "description": "Discover registered agents by capability tag, keyword, or runtime (e.g. antigravity, claude_code, claude_desktop, human).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "capability": { "type": "string", "description": "Tag or keyword to search in agent capabilities, bio, or address" },
                    "runtime": { "type": "string", "description": "Optional runtime filter (antigravity, claude_code, claude_desktop, codex, human)" }
                }
            }
        }),
    ]
}
