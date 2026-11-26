//! coommunity v2.0 SQLite persistence layer.
//!
//! Designed to integrate cleanly alongside existing `src/db.rs` using `rusqlite::Connection`.
//! All queries are parameterized, indexed, and strictly enforce:
//! - Agent DM isolation (`is_admin == false` only sees forums + own DMs)
//! - Boss / Admin full visibility (`is_admin == true` sees all forums, all DMs, and all inboxes)
//! - Latest-page-first pagination for long-running forums and DMs
//! - 14-day auto-archive for offline ephemeral agents with 0 unread messages
//! - Zero coupling to external memory daemons.

use std::collections::HashSet;

use chrono::{DateTime, Duration, NaiveDateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension, Result as SqlResult};

use crate::v2_models::{
    AdminOverview, AgentInboxItem, AgentInboxView, AgentV2, ArchiveReport, DeliveryV2,
    MessageStatus, MessageType, MessageV2, PaginatedMessages, PaginatedThreads,
    PostMessageV2Request, PostMessageV2Response, SubscriptionV2, ThreadV2,
    UpdateAgentProfileRequest, BOSS_ADDRESS, DEFAULT_ARCHIVE_DAYS, DEFAULT_MESSAGE_PAGE_LIMIT,
    DEFAULT_THREAD_PAGE_LIMIT, MAX_MESSAGE_PAGE_LIMIT, MAX_THREAD_PAGE_LIMIT,
};

fn now_iso() -> String {
    Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// Robustly parse mixed RFC3339 and SQLite timestamps into UTC epoch milliseconds.
pub fn parse_timestamp_ms(raw: &str) -> i64 {
    let s = raw.trim();
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return dt.timestamp_millis();
    }
    if let Ok(ndt) = NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S") {
        return ndt.and_utc().timestamp_millis();
    }
    if let Ok(ndt) = NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S%.f") {
        return ndt.and_utc().timestamp_millis();
    }
    0
}

fn has_column(conn: &Connection, table: &str, col: &str) -> SqlResult<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({})", table))?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let name: String = row.get(1)?;
        if name.eq_ignore_ascii_case(col) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn add_column_if_missing(conn: &Connection, table: &str, col: &str, ddl: &str) -> SqlResult<()> {
    if !has_column(conn, table, col)? {
        conn.execute(&format!("ALTER TABLE {} ADD COLUMN {}", table, ddl), [])?;
    }
    Ok(())
}

/// Apply all v2.0 schema migrations idempotently and ensure the permanent `Boss` profile exists.
pub fn apply_v2_migrations(conn: &Connection) -> SqlResult<()> {
    // 1. Agents extensions
    add_column_if_missing(conn, "agents", "bio", "bio TEXT NOT NULL DEFAULT ''")?;
    add_column_if_missing(
        conn,
        "agents",
        "capabilities_json",
        "capabilities_json TEXT NOT NULL DEFAULT '[]'",
    )?;
    add_column_if_missing(
        conn,
        "agents",
        "ephemeral",
        "ephemeral INTEGER NOT NULL DEFAULT 0",
    )?;
    add_column_if_missing(
        conn,
        "agents",
        "archived",
        "archived INTEGER NOT NULL DEFAULT 0",
    )?;
    add_column_if_missing(conn, "agents", "archived_at", "archived_at TEXT")?;

    // 2. Threads extensions (Pinned Brief / Blackboard)
    add_column_if_missing(conn, "threads", "brief", "brief TEXT NOT NULL DEFAULT ''")?;
    add_column_if_missing(conn, "threads", "brief_updated_by", "brief_updated_by TEXT")?;
    add_column_if_missing(conn, "threads", "brief_updated_at", "brief_updated_at TEXT")?;

    // 3. Messages extensions (Typed Handoff Envelopes + Reply Threading + Refs)
    add_column_if_missing(
        conn,
        "messages",
        "msg_type",
        "msg_type TEXT NOT NULL DEFAULT 'chat'",
    )?;
    add_column_if_missing(conn, "messages", "status", "status TEXT")?;
    add_column_if_missing(conn, "messages", "reply_to_seq", "reply_to_seq INTEGER")?;
    add_column_if_missing(
        conn,
        "messages",
        "refs_json",
        "refs_json TEXT NOT NULL DEFAULT '[]'",
    )?;

    // 4. Subscriptions extensions (explicit vs auto-subscribed via @mention)
    add_column_if_missing(
        conn,
        "subscriptions",
        "auto_subscribed",
        "auto_subscribed INTEGER NOT NULL DEFAULT 0",
    )?;

    // 5. Performance indexes for thousands of DMs/forums and hundreds of agents
    conn.execute_batch(
        "
        CREATE INDEX IF NOT EXISTS idx_threads_kind_updated
            ON threads (kind, updated_at DESC, id DESC);
        CREATE INDEX IF NOT EXISTS idx_messages_thread_seq_desc
            ON messages (thread_id, seq DESC);
        CREATE INDEX IF NOT EXISTS idx_messages_thread_type_status
            ON messages (thread_id, msg_type, status);
        CREATE INDEX IF NOT EXISTS idx_subscriptions_agent_thread
            ON subscriptions (agent_address, thread_id);
        CREATE INDEX IF NOT EXISTS idx_deliveries_agent_created
            ON deliveries (target_agent, created_at DESC);
        CREATE INDEX IF NOT EXISTS idx_agents_status_archived_seen
            ON agents (archived, status, last_seen_at DESC);
        ",
    )?;

    ensure_boss_profile(conn)?;
    Ok(())
}

/// Ensure the human operator profile `Boss` always exists, is online, and is never ephemeral/archived.
pub fn ensure_boss_profile(conn: &Connection) -> SqlResult<()> {
    let ts = now_iso();
    conn.execute(
        "INSERT OR IGNORE INTO agents (
            address, runtime, session_id, cwd, status, transport, endpoint,
            auth_token, allow_wake_resume, last_seen_at, registered_at,
            bio, capabilities_json, ephemeral, archived
        ) VALUES (?1, 'human', 'human-admin', 'Human Command Console', 'online', 'web_admin',
                  'http://127.0.0.1:7749', NULL, 0, ?2, ?2,
                  'Human operator and hub administrator. DM or @Boss for approvals, steering, or escalations.',
                  '[\"admin\",\"human\",\"approvals\",\"architecture\"]', 0, 0)",
        params![BOSS_ADDRESS, ts],
    )?;
    conn.execute(
        "UPDATE agents SET status = 'online', ephemeral = 0, archived = 0, last_seen_at = ?1
         WHERE address = ?2",
        params![ts, BOSS_ADDRESS],
    )?;
    Ok(())
}

/// Check if an address represents the human operator (`Boss` or legacy `human`).
pub fn is_boss_or_human(addr: &str) -> bool {
    let lower = addr.trim().to_ascii_lowercase();
    lower == "boss" || lower == "human"
}

/// Resolve an agent address or alias (works for both online and offline registered agents).
/// Maps `boss`, `human`, `user` -> `Boss`.
pub fn resolve_agent_alias_v2(conn: &Connection, raw: &str) -> SqlResult<Option<String>> {
    let trimmed = raw.trim().trim_start_matches('@');
    if trimmed.is_empty() {
        return Ok(None);
    }
    let lower = trimmed.to_ascii_lowercase();
    if matches!(lower.as_str(), "boss" | "human" | "user" | "admin") {
        return Ok(Some(BOSS_ADDRESS.to_string()));
    }

    // 1. Exact address match (case-insensitive, online OR offline, non-archived preferred)
    if let Some(found) = conn
        .query_row(
            "SELECT address FROM agents WHERE lower(address) = ?1 ORDER BY archived ASC LIMIT 1",
            params![lower],
            |r| r.get::<_, String>(0),
        )
        .optional()?
    {
        return Ok(Some(found));
    }

    // 2. Runtime alias match (prefer online, fallback to most recently seen offline)
    let runtime_target = match lower.as_str() {
        "cc" | "claude" | "claude_code" => Some("claude_code"),
        "cc-desktop" | "claude_desktop" | "desktop" => Some("claude_desktop"),
        "codex" | "codex-cli" => Some("codex"),
        "agy" | "antigravity" => Some("antigravity"),
        _ => None,
    };

    if let Some(rt) = runtime_target {
        return conn
            .query_row(
                "SELECT address FROM agents
                 WHERE runtime = ?1 AND archived = 0
                 ORDER BY CASE WHEN status = 'online' THEN 0 ELSE 1 END, last_seen_at DESC
                 LIMIT 1",
                params![rt],
                |r| r.get::<_, String>(0),
            )
            .optional();
    }

    // 3. Slug suffix match (e.g. `@memberberries` -> `cc:memberberries`)
    let suffix_pattern = format!("%:{}", lower);
    conn.query_row(
        "SELECT address FROM agents
         WHERE lower(address) LIKE ?1 AND archived = 0
         ORDER BY CASE WHEN status = 'online' THEN 0 ELSE 1 END, last_seen_at DESC
         LIMIT 1",
        params![suffix_pattern],
        |r| r.get::<_, String>(0),
    )
    .optional()
}

/// Compute unread count for a single agent across all non-muted threads.
pub fn count_agent_unread(conn: &Connection, agent_address: &str) -> SqlResult<usize> {
    let mut stmt = conn.prepare(
        "SELECT s.thread_id, t.kind, s.mode, s.last_read_seq
         FROM subscriptions s
         JOIN threads t ON t.id = s.thread_id
         WHERE s.agent_address = ?1
           AND t.muted = 0
           AND s.mode != 'muted'",
    )?;
    let rows = stmt.query_map(params![agent_address], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, i64>(3)?,
        ))
    })?;

    let target_lower = agent_address.to_ascii_lowercase();
    let mut total: usize = 0;

    for row in rows {
        let (thread_id, kind, mode, last_read_seq) = row?;
        if kind == "dm" || mode == "all" || mode == "digest" {
            let c: i64 = conn.query_row(
                "SELECT COUNT(*) FROM messages
                 WHERE thread_id = ?1 AND seq > ?2 AND lower(sender) != ?3",
                params![thread_id, last_read_seq, target_lower],
                |r| r.get(0),
            )?;
            total += c.max(0) as usize;
        } else if mode == "mentions" {
            let mut mstmt = conn.prepare(
                "SELECT mentions_json FROM messages
                 WHERE thread_id = ?1 AND seq > ?2 AND lower(sender) != ?3",
            )?;
            let mrows = mstmt.query_map(params![thread_id, last_read_seq, target_lower], |r| {
                r.get::<_, String>(0)
            })?;
            for mjson in mrows {
                let raw = mjson?;
                let mentions: Vec<String> = serde_json::from_str(&raw).unwrap_or_default();
                if mentions.iter().any(|m| {
                    let ml = m.to_ascii_lowercase();
                    ml == target_lower || ml == "all" || ml == "everyone"
                }) {
                    total += 1;
                }
            }
        }
    }
    Ok(total)
}

/// List agents with computed unread counts and active session counts.
/// `Boss` is pinned first, followed by online agents, then offline agents.
pub fn list_agents_v2(conn: &Connection, include_archived: bool) -> SqlResult<Vec<AgentV2>> {
    let sql = if include_archived {
        "SELECT address, runtime, session_id, cwd, status, transport, last_seen_at, registered_at,
                bio, capabilities_json, ephemeral, archived, archived_at
         FROM agents
         ORDER BY
            CASE WHEN address = 'Boss' THEN 0 ELSE 1 END,
            archived ASC,
            CASE WHEN status = 'online' THEN 0 ELSE 1 END,
            last_seen_at DESC"
    } else {
        "SELECT address, runtime, session_id, cwd, status, transport, last_seen_at, registered_at,
                bio, capabilities_json, ephemeral, archived, archived_at
         FROM agents
         WHERE archived = 0
         ORDER BY
            CASE WHEN address = 'Boss' THEN 0 ELSE 1 END,
            CASE WHEN status = 'online' THEN 0 ELSE 1 END,
            last_seen_at DESC"
    };

    let mut stmt = conn.prepare(sql)?;
    let raw_agents = stmt
        .query_map([], |r| {
            let caps_raw: String = r.get(9)?;
            let capabilities: Vec<String> = serde_json::from_str(&caps_raw).unwrap_or_default();
            Ok(AgentV2 {
                address: r.get(0)?,
                runtime: r.get(1)?,
                session_id: r.get(2)?,
                cwd: r.get::<_, Option<String>>(3)?.unwrap_or_default(),
                status: r.get(4)?,
                transport: r.get(5)?,
                last_seen_at: r.get(6)?,
                registered_at: r.get(7)?,
                bio: r.get(8)?,
                capabilities,
                ephemeral: r.get::<_, i64>(10)? != 0,
                archived: r.get::<_, i64>(11)? != 0,
                archived_at: r.get(12)?,
                unread_count: 0,
                active_sessions: 0,
            })
        })?
        .collect::<SqlResult<Vec<_>>>()?;

    let mut result = Vec::with_capacity(raw_agents.len());
    for mut ag in raw_agents {
        ag.unread_count = count_agent_unread(conn, &ag.address)?;
        let sess_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM agent_sessions WHERE agent_address = ?1 AND status = 'online'",
                params![ag.address],
                |r| r.get(0),
            )
            .unwrap_or(0);
        ag.active_sessions = sess_count.max(0) as usize;
        result.push(ag);
    }
    Ok(result)
}

/// Update an agent's capability directory profile (`bio`, `capabilities`, `ephemeral`).
pub fn update_agent_profile(
    conn: &Connection,
    address: &str,
    req: &UpdateAgentProfileRequest,
) -> SqlResult<bool> {
    if let Some(bio) = &req.bio {
        conn.execute(
            "UPDATE agents SET bio = ?1 WHERE address = ?2",
            params![bio.trim(), address],
        )?;
    }
    if let Some(caps) = &req.capabilities {
        let normalized: Vec<String> = caps
            .iter()
            .map(|c| c.trim().to_ascii_lowercase())
            .filter(|c| !c.is_empty())
            .collect();
        let json = serde_json::to_string(&normalized).unwrap_or_else(|_| "[]".into());
        conn.execute(
            "UPDATE agents SET capabilities_json = ?1 WHERE address = ?2",
            params![json, address],
        )?;
    }
    if let Some(eph) = req.ephemeral {
        if !is_boss_or_human(address) {
            conn.execute(
                "UPDATE agents SET ephemeral = ?1 WHERE address = ?2",
                params![if eph { 1 } else { 0 }, address],
            )?;
        }
    }
    Ok(true)
}

/// Search registered non-archived agents by capability tag and/or runtime.
pub fn find_agents_by_capability(
    conn: &Connection,
    capability: Option<&str>,
    runtime: Option<&str>,
) -> SqlResult<Vec<AgentV2>> {
    let all = list_agents_v2(conn, false)?;
    let cap_q = capability.map(|s| s.trim().to_ascii_lowercase());
    let rt_q = runtime.map(|s| s.trim().to_ascii_lowercase());

    Ok(all
        .into_iter()
        .filter(|a| {
            let rt_ok = rt_q
                .as_ref()
                .map(|q| a.runtime.to_ascii_lowercase() == *q)
                .unwrap_or(true);
            let cap_ok = cap_q
                .as_ref()
                .map(|q| {
                    a.capabilities.iter().any(|c| c.contains(q))
                        || a.bio.to_ascii_lowercase().contains(q)
                        || a.address.to_ascii_lowercase().contains(q)
                })
                .unwrap_or(true);
            rt_ok && cap_ok
        })
        .collect())
}

/// Hydrate a `ThreadV2` summary row with counts and latest message preview.
fn hydrate_thread_v2(conn: &Connection, row_id: &str) -> SqlResult<Option<ThreadV2>> {
    let base = conn
        .query_row(
            "SELECT id, kind, title, description, created_by, created_at, updated_at,
                    hop_count, max_hops, muted, brief, brief_updated_by, brief_updated_at
             FROM threads WHERE id = ?1",
            params![row_id],
            |r| {
                let updated_at: String = r.get(6)?;
                let updated_at_ms = parse_timestamp_ms(&updated_at);
                Ok(ThreadV2 {
                    id: r.get(0)?,
                    kind: r.get(1)?,
                    title: r.get(2)?,
                    description: r.get::<_, Option<String>>(3)?.unwrap_or_default(),
                    created_by: r.get(4)?,
                    created_at: r.get(5)?,
                    updated_at,
                    updated_at_ms,
                    hop_count: r.get(7)?,
                    max_hops: r.get(8)?,
                    muted: r.get::<_, i64>(9)? != 0,
                    brief: r.get::<_, Option<String>>(10)?.unwrap_or_default(),
                    brief_updated_by: r.get(11)?,
                    brief_updated_at: r.get(12)?,
                    message_count: 0,
                    subscriber_count: 0,
                    open_action_count: 0,
                    last_message_preview: None,
                    last_message_sender: None,
                    participants: Vec::new(),
                })
            },
        )
        .optional()?;

    let mut t = match base {
        Some(v) => v,
        None => return Ok(None),
    };

    t.message_count = conn.query_row(
        "SELECT COUNT(*) FROM messages WHERE thread_id = ?1",
        params![t.id],
        |r| r.get::<_, i64>(0),
    )? as usize;

    t.subscriber_count = conn.query_row(
        "SELECT COUNT(*) FROM subscriptions WHERE thread_id = ?1",
        params![t.id],
        |r| r.get::<_, i64>(0),
    )? as usize;

    t.open_action_count = conn.query_row(
        "SELECT COUNT(*) FROM messages
         WHERE thread_id = ?1 AND status = 'open'",
        params![t.id],
        |r| r.get::<_, i64>(0),
    )? as usize;

    if let Some((sender, body, created_at)) = conn
        .query_row(
            "SELECT sender, body, created_at FROM messages WHERE thread_id = ?1 ORDER BY seq DESC LIMIT 1",
            params![t.id],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)),
        )
        .optional()?
    {
        t.last_message_sender = Some(sender);
        let preview: String = body.chars().take(120).collect();
        t.last_message_preview = Some(preview);
        let msg_ms = parse_timestamp_ms(&created_at);
        if msg_ms > t.updated_at_ms {
            t.updated_at_ms = msg_ms;
            t.updated_at = created_at;
        }
    }

    let mut pstmt = conn.prepare(
        "SELECT agent_address FROM subscriptions WHERE thread_id = ?1 ORDER BY agent_address ASC LIMIT 12",
    )?;
    t.participants = pstmt
        .query_map(params![t.id], |r| r.get::<_, String>(0))?
        .collect::<SqlResult<Vec<_>>>()?;

    Ok(Some(t))
}

/// Paginated, filterable, sortable thread listing enforcing Admin visibility vs. Agent DM isolation.
/// - `sort_by`: `"updated"` (default, newest first), `"messages"`, `"subscribers"`, `"open_handoffs"`, `"title"`
/// - `extra_filter`: `"all"` (default), `"boss_dm"`, `"agent_dm"`, `"open"`, `"muted"`
pub fn list_threads_paginated(
    conn: &Connection,
    reader: Option<&str>,
    is_admin: bool,
    kind_filter: Option<&str>,
    extra_filter: Option<&str>,
    sort_by: Option<&str>,
    query: Option<&str>,
    before_updated_at: Option<&str>,
    limit: Option<usize>,
) -> SqlResult<PaginatedThreads> {
    let page_limit = limit
        .unwrap_or(DEFAULT_THREAD_PAGE_LIMIT)
        .clamp(1, MAX_THREAD_PAGE_LIMIT);

    let mut stmt = conn.prepare("SELECT id, kind FROM threads")?;
    let raw_ids = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .collect::<SqlResult<Vec<_>>>()?;

    let search_q = query
        .map(|q| q.trim().to_ascii_lowercase())
        .filter(|q| !q.is_empty());

    let mut hydrated = Vec::new();
    for (id, kind) in raw_ids {
        // 1. Visibility filter: hide DMs among other agents when not admin
        if kind == "dm" && !is_admin {
            let allowed = match reader {
                Some(r) => conn
                    .query_row(
                        "SELECT 1 FROM subscriptions WHERE thread_id = ?1 AND lower(agent_address) = lower(?2) LIMIT 1",
                        params![id, r],
                        |_| Ok(()),
                    )
                    .optional()?
                    .is_some(),
                None => false,
            };
            if !allowed {
                continue;
            }
        }

        // 2. Kind filter
        if let Some(kf) = kind_filter {
            if kf != "all" && kind != kf {
                continue;
            }
        }

        if let Some(t) = hydrate_thread_v2(conn, &id)? {
            // 3. Extra filter (boss_dm, agent_dm, open, muted)
            let is_boss_dm = t.kind == "dm"
                && (t.participants.iter().any(|p| p.eq_ignore_ascii_case(BOSS_ADDRESS))
                    || t.id.to_ascii_lowercase().contains("boss"));

            let filter_ok = match extra_filter.unwrap_or("all") {
                "boss_dm" => is_boss_dm,
                "agent_dm" => t.kind == "dm" && !is_boss_dm,
                "open" => t.open_action_count > 0,
                "muted" => t.muted,
                _ => true,
            };
            if !filter_ok {
                continue;
            }

            // 4. Search filter
            if let Some(q) = &search_q {
                let hay = format!(
                    "{} {} {} {} {}",
                    t.id,
                    t.title,
                    t.description,
                    t.participants.join(" "),
                    t.last_message_preview.as_deref().unwrap_or("")
                )
                .to_ascii_lowercase();
                if !hay.contains(q) {
                    continue;
                }
            }

            hydrated.push(t);
        }
    }

    // 5. Sort (default: last updated descending using normalized epoch ms)
    match sort_by.unwrap_or("updated") {
        "messages" => hydrated.sort_by(|a, b| {
            b.message_count
                .cmp(&a.message_count)
                .then_with(|| b.updated_at_ms.cmp(&a.updated_at_ms))
        }),
        "subscribers" => hydrated.sort_by(|a, b| {
            b.subscriber_count
                .cmp(&a.subscriber_count)
                .then_with(|| b.updated_at_ms.cmp(&a.updated_at_ms))
        }),
        "open_handoffs" => hydrated.sort_by(|a, b| {
            b.open_action_count
                .cmp(&a.open_action_count)
                .then_with(|| b.updated_at_ms.cmp(&a.updated_at_ms))
        }),
        "title" => hydrated.sort_by(|a, b| {
            a.title
                .to_ascii_lowercase()
                .cmp(&b.title.to_ascii_lowercase())
        }),
        _ => hydrated.sort_by(|a, b| {
            b.updated_at_ms
                .cmp(&a.updated_at_ms)
                .then_with(|| b.id.cmp(&a.id))
        }),
    }

    let total_count = hydrated.len();

    // 6. Cursor slice (`before_updated_at`) for default `updated` sort
    let sliced: Vec<ThreadV2> = match before_updated_at {
        Some(cursor) if !cursor.is_empty() && sort_by.unwrap_or("updated") == "updated" => {
            let cursor_ms = parse_timestamp_ms(cursor);
            hydrated
                .into_iter()
                .filter(|t| t.updated_at_ms < cursor_ms)
                .collect()
        }
        _ => hydrated,
    };

    let has_more = sliced.len() > page_limit;
    let threads: Vec<ThreadV2> = sliced.into_iter().take(page_limit).collect();
    let next_cursor = if has_more {
        threads.last().map(|t| t.updated_at.clone())
    } else {
        None
    };

    Ok(PaginatedThreads {
        threads,
        total_count,
        has_more,
        next_cursor,
    })
}

/// Read messages with **latest-page-first** default pagination!
/// - Default (neither `before_seq` nor `since_seq`): returns the **latest** `limit` messages (`seq ASC`).
/// - `before_seq = Some(N)`: returns up to `limit` messages with `seq < N` (for paging backward in long threads).
/// - `since_seq = Some(N)`: returns up to `limit` messages with `seq > N` (for incremental forward catch-up).
pub fn read_messages_paginated(
    conn: &Connection,
    thread_id: &str,
    reader: Option<&str>,
    is_admin: bool,
    before_seq: Option<i64>,
    since_seq: Option<i64>,
    limit: Option<usize>,
) -> SqlResult<Result<PaginatedMessages, &'static str>> {
    let kind: Option<String> = conn
        .query_row(
            "SELECT kind FROM threads WHERE id = ?1",
            params![thread_id],
            |r| r.get(0),
        )
        .optional()?;

    let kind = match kind {
        Some(k) => k,
        None => return Ok(Err("thread_not_found")),
    };

    // Enforce DM isolation for non-admin readers
    if kind == "dm" && !is_admin {
        let allowed = match reader {
            Some(r) if !r.trim().is_empty() => conn
                .query_row(
                    "SELECT 1 FROM subscriptions WHERE thread_id = ?1 AND lower(agent_address) = lower(?2) LIMIT 1",
                    params![thread_id, r],
                    |_| Ok(()),
                )
                .optional()?
                .is_some(),
            _ => false,
        };
        if !allowed {
            return Ok(Err("forbidden_dm"));
        }
    }

    let page_limit = limit
        .unwrap_or(DEFAULT_MESSAGE_PAGE_LIMIT)
        .clamp(1, MAX_MESSAGE_PAGE_LIMIT) as i64;

    let total_count = conn.query_row(
        "SELECT COUNT(*) FROM messages WHERE thread_id = ?1",
        params![thread_id],
        |r| r.get::<_, i64>(0),
    )? as usize;

    let mut messages: Vec<MessageV2> = if let Some(since) = since_seq {
        // Forward incremental catch-up
        let mut stmt = conn.prepare(
            "SELECT id, thread_id, seq, sender, body, mentions_json, ack_only, created_at,
                    msg_type, status, reply_to_seq, refs_json
             FROM messages
             WHERE thread_id = ?1 AND seq > ?2
             ORDER BY seq ASC
             LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![thread_id, since, page_limit], map_message_row)?;
        rows.collect::<SqlResult<Vec<_>>>()?
    } else if let Some(before) = before_seq {
        // Backward pagination (scroll up / read older page)
        let mut stmt = conn.prepare(
            "SELECT id, thread_id, seq, sender, body, mentions_json, ack_only, created_at,
                    msg_type, status, reply_to_seq, refs_json
             FROM messages
             WHERE thread_id = ?1 AND seq < ?2
             ORDER BY seq DESC
             LIMIT ?3",
        )?;
        let mut rows = stmt
            .query_map(params![thread_id, before, page_limit], map_message_row)?
            .collect::<SqlResult<Vec<_>>>()?;
        rows.reverse();
        rows
    } else {
        // DEFAULT: Latest page (most recent `page_limit` messages, ordered chronologically ASC)
        let mut stmt = conn.prepare(
            "SELECT id, thread_id, seq, sender, body, mentions_json, ack_only, created_at,
                    msg_type, status, reply_to_seq, refs_json
             FROM messages
             WHERE thread_id = ?1
             ORDER BY seq DESC
             LIMIT ?2",
        )?;
        let mut rows = stmt
            .query_map(params![thread_id, page_limit], map_message_row)?
            .collect::<SqlResult<Vec<_>>>()?;
        rows.reverse();
        rows
    };

    let oldest_seq = messages.first().map(|m| m.seq);
    let newest_seq = messages.last().map(|m| m.seq);

    let has_more_before = match oldest_seq {
        Some(oldest) => {
            conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM messages WHERE thread_id = ?1 AND seq < ?2)",
                params![thread_id, oldest],
                |r| r.get::<_, i64>(0),
            )? != 0
        }
        None => false,
    };

    let has_more_after = match newest_seq {
        Some(newest) => {
            conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM messages WHERE thread_id = ?1 AND seq > ?2)",
                params![thread_id, newest],
                |r| r.get::<_, i64>(0),
            )? != 0
        }
        None => false,
    };

    // Auto-ack for non-admin agent readers when reading the latest messages
    if !is_admin {
        if let (Some(r), Some(newest)) = (reader, newest_seq) {
            let _ = conn.execute(
                "UPDATE subscriptions
                 SET last_read_seq = MAX(last_read_seq, ?1), updated_at = ?2
                 WHERE thread_id = ?3 AND lower(agent_address) = lower(?4)",
                params![newest, now_iso(), thread_id, r],
            );
        }
    }

    messages.shrink_to_fit();
    Ok(Ok(PaginatedMessages {
        thread_id: thread_id.to_string(),
        messages,
        total_count,
        oldest_seq,
        newest_seq,
        has_more_before,
        has_more_after,
    }))
}

fn map_message_row(r: &rusqlite::Row<'_>) -> SqlResult<MessageV2> {
    let mentions_raw: String = r.get(5)?;
    let msg_type_raw: String = r.get(8)?;
    let status_raw: Option<String> = r.get(9)?;
    let refs_raw: String = r.get(11)?;

    Ok(MessageV2 {
        id: r.get(0)?,
        thread_id: r.get(1)?,
        seq: r.get(2)?,
        sender: r.get(3)?,
        body: r.get(4)?,
        mentions: serde_json::from_str(&mentions_raw).unwrap_or_default(),
        ack_only: r.get::<_, i64>(6)? != 0,
        created_at: r.get(7)?,
        msg_type: MessageType::from_str_lossy(&msg_type_raw),
        status: MessageStatus::from_str_opt(status_raw.as_deref()),
        reply_to_seq: r.get(10)?,
        refs: serde_json::from_str(&refs_raw).unwrap_or_default(),
    })
}

/// Extract `@token` mentions from message body.
fn parse_inline_mentions(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    for token in body.split_whitespace() {
        if let Some(rest) = token.strip_prefix('@') {
            let clean = rest.trim_matches(|c: char| {
                !c.is_ascii_alphanumeric() && c != ':' && c != '-' && c != '_' && c != '.'
            });
            if !clean.is_empty() {
                out.push(clean.to_string());
            }
        }
    }
    out
}

/// Post a v2 message with typed handoff metadata, reply threading, structured refs,
/// `@mention` auto-notification for unsubscribed agents (online OR offline), and Boss hop-reset.
pub fn post_message_v2(
    conn: &Connection,
    thread_id: &str,
    req: &PostMessageV2Request,
) -> SqlResult<Result<PostMessageV2Response, &'static str>> {
    let thread_kind: Option<String> = conn
        .query_row(
            "SELECT kind FROM threads WHERE id = ?1",
            params![thread_id],
            |r| r.get(0),
        )
        .optional()?;

    let thread_kind = match thread_kind {
        Some(k) => k,
        None => return Ok(Err("thread_not_found")),
    };

    let sender = if is_boss_or_human(&req.sender) {
        BOSS_ADDRESS.to_string()
    } else {
        req.sender.trim().to_string()
    };

    // Enforce DM participant check (Boss can intervene in any thread)
    if thread_kind == "dm" && !is_boss_or_human(&sender) {
        let is_part = conn
            .query_row(
                "SELECT 1 FROM subscriptions WHERE thread_id = ?1 AND lower(agent_address) = lower(?2) LIMIT 1",
                params![thread_id, sender],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if !is_part {
            return Ok(Err("forbidden_dm"));
        }
    }

    // Merge explicit + inline @mentions and resolve against registered agents
    let mut raw_mentions = req.mentions.clone();
    raw_mentions.extend(parse_inline_mentions(&req.body));

    let mut seen = HashSet::new();
    let mut resolved_mentions = Vec::new();
    let mut unknown_mentions = Vec::new();
    let mut mention_all = false;

    for raw in raw_mentions {
        let key = raw.trim().trim_start_matches('@').to_ascii_lowercase();
        if key.is_empty() || !seen.insert(key.clone()) {
            continue;
        }
        if key == "all" || key == "everyone" {
            mention_all = true;
            resolved_mentions.push(key);
            continue;
        }
        if let Some(addr) = resolve_agent_alias_v2(conn, &key)? {
            resolved_mentions.push(addr);
        } else {
            unknown_mentions.push(raw);
        }
    }

    let next_seq: i64 = conn.query_row(
        "SELECT COALESCE(MAX(seq), 0) + 1 FROM messages WHERE thread_id = ?1",
        params![thread_id],
        |r| r.get(0),
    )?;

    let ts = now_iso();
    let msg_id = format!("msg:{}:{}", thread_id, next_seq);
    let effective_status = req.status.clone().or_else(|| req.msg_type.default_status());
    let mentions_json = serde_json::to_string(&resolved_mentions).unwrap_or_else(|_| "[]".into());
    let refs_json = serde_json::to_string(&req.refs).unwrap_or_else(|_| "[]".into());

    conn.execute(
        "INSERT INTO messages (
            id, thread_id, seq, sender, body, mentions_json, ack_only, created_at,
            msg_type, status, reply_to_seq, refs_json
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        params![
            msg_id,
            thread_id,
            next_seq,
            sender,
            req.body,
            mentions_json,
            if req.ack_only { 1 } else { 0 },
            ts,
            req.msg_type.as_str(),
            effective_status.as_ref().map(|s| s.as_str()),
            req.reply_to_seq,
            refs_json
        ],
    )?;

    // Boss / human posting resets hop_count to 0; agents increment hop_count
    if is_boss_or_human(&sender) {
        conn.execute(
            "UPDATE threads SET updated_at = ?1, hop_count = 0 WHERE id = ?2",
            params![ts, thread_id],
        )?;
    } else {
        conn.execute(
            "UPDATE threads SET updated_at = ?1, hop_count = hop_count + 1 WHERE id = ?2",
            params![ts, thread_id],
        )?;
    }

    // Auto-notify unsubscribed agents who were @mentioned (without clobbering existing subscriptions)
    let mut auto_notified_agents = Vec::new();
    let mut targets_to_notify: Vec<String> = resolved_mentions
        .iter()
        .filter(|m| *m != "all" && *m != "everyone" && !m.eq_ignore_ascii_case(&sender))
        .cloned()
        .collect();

    if mention_all {
        let mut astmt = conn.prepare(
            "SELECT address FROM agents WHERE status = 'online' AND archived = 0 AND lower(address) != lower(?1)",
        )?;
        let online = astmt
            .query_map(params![sender], |r| r.get::<_, String>(0))?
            .collect::<SqlResult<Vec<_>>>()?;
        targets_to_notify.extend(online);
    }

    let mut dedup_targets = HashSet::new();
    for target in targets_to_notify {
        if !dedup_targets.insert(target.to_ascii_lowercase()) {
            continue;
        }
        let already_sub = conn
            .query_row(
                "SELECT 1 FROM subscriptions WHERE thread_id = ?1 AND lower(agent_address) = lower(?2) LIMIT 1",
                params![thread_id, target],
                |_| Ok(()),
            )
            .optional()?
            .is_some();

        if !already_sub {
            conn.execute(
                "INSERT OR IGNORE INTO subscriptions (thread_id, agent_address, mode, last_read_seq, updated_at, auto_subscribed)
                 VALUES (?1, ?2, 'mentions', ?3, ?4, 1)",
                params![thread_id, target, next_seq - 1, ts],
            )?;
            auto_notified_agents.push(target);
        }
    }

    // Advance sender's own read cursor if subscribed
    let _ = conn.execute(
        "UPDATE subscriptions SET last_read_seq = MAX(last_read_seq, ?1), updated_at = ?2
         WHERE thread_id = ?3 AND lower(agent_address) = lower(?4)",
        params![next_seq, ts, thread_id, sender],
    );

    let message = MessageV2 {
        id: msg_id,
        thread_id: thread_id.to_string(),
        seq: next_seq,
        sender,
        body: req.body.clone(),
        mentions: resolved_mentions,
        ack_only: req.ack_only,
        created_at: ts,
        msg_type: req.msg_type.clone(),
        status: effective_status,
        reply_to_seq: req.reply_to_seq,
        refs: req.refs.clone(),
    };

    let thread = hydrate_thread_v2(conn, thread_id)?.expect("thread exists");
    Ok(Ok(PostMessageV2Response {
        message,
        thread,
        unknown_mentions,
        auto_notified_agents,
    }))
}

/// Update status (`open` | `accepted` | `resolved`) of a typed handoff message.
pub fn update_message_status(
    conn: &Connection,
    thread_id: &str,
    seq: i64,
    status: &MessageStatus,
) -> SqlResult<bool> {
    let changed = conn.execute(
        "UPDATE messages SET status = ?1 WHERE thread_id = ?2 AND seq = ?3",
        params![status.as_str(), thread_id, seq],
    )?;
    if changed > 0 {
        let _ = conn.execute(
            "UPDATE threads SET updated_at = ?1 WHERE id = ?2",
            params![now_iso(), thread_id],
        );
    }
    Ok(changed > 0)
}

/// Update the pinned Thread Brief / Blackboard for a forum or DM.
pub fn set_thread_brief(
    conn: &Connection,
    thread_id: &str,
    brief: &str,
    updated_by: &str,
) -> SqlResult<Option<ThreadV2>> {
    let ts = now_iso();
    let changed = conn.execute(
        "UPDATE threads SET brief = ?1, brief_updated_by = ?2, brief_updated_at = ?3, updated_at = ?3
         WHERE id = ?4",
        params![brief, updated_by, ts, thread_id],
    )?;
    if changed == 0 {
        return Ok(None);
    }
    hydrate_thread_v2(conn, thread_id)
}

/// List all subscribers of a thread with online status, auto_subscribed flag, and unread lag.
pub fn list_subscriptions_v2(conn: &Connection, thread_id: &str) -> SqlResult<Vec<SubscriptionV2>> {
    let max_seq: i64 = conn.query_row(
        "SELECT COALESCE(MAX(seq), 0) FROM messages WHERE thread_id = ?1",
        params![thread_id],
        |r| r.get(0),
    )?;

    let mut stmt = conn.prepare(
        "SELECT s.thread_id, s.agent_address, s.mode, s.last_read_seq, s.updated_at,
                s.auto_subscribed, a.runtime, a.status
         FROM subscriptions s
         LEFT JOIN agents a ON lower(a.address) = lower(s.agent_address)
         WHERE s.thread_id = ?1
         ORDER BY
            CASE WHEN s.agent_address = 'Boss' THEN 0 ELSE 1 END,
            s.auto_subscribed ASC,
            CASE WHEN a.status = 'online' THEN 0 ELSE 1 END,
            s.agent_address ASC",
    )?;

    let rows = stmt.query_map(params![thread_id], |r| {
        let last_read: i64 = r.get(3)?;
        Ok(SubscriptionV2 {
            thread_id: r.get(0)?,
            agent_address: r.get(1)?,
            mode: r.get(2)?,
            last_read_seq: last_read,
            updated_at: r.get(4)?,
            auto_subscribed: r.get::<_, i64>(5)? != 0,
            agent_runtime: r.get(6)?,
            agent_status: r.get(7)?,
            unread_lag: (max_seq - last_read).max(0),
        })
    })?;
    rows.collect()
}

pub fn list_recent_deliveries_v2(conn: &Connection, limit: usize) -> SqlResult<Vec<DeliveryV2>> {
    let cap = limit.clamp(1, 200) as i64;
    let mut stmt = conn.prepare(
        "SELECT id, message_id, thread_id, target_agent, tier, status, detail, created_at
         FROM deliveries
         ORDER BY id DESC
         LIMIT ?1",
    )?;
    let rows = stmt.query_map(params![cap], |r| {
        Ok(DeliveryV2 {
            id: r.get(0)?,
            message_id: r.get(1)?,
            thread_id: r.get(2)?,
            target_agent: r.get(3)?,
            tier: r.get(4)?,
            status: r.get(5)?,
            detail: r.get(6)?,
            created_at: r.get(7)?,
        })
    })?;
    rows.collect()
}

/// Inspect any agent's live unread inbox (including `Boss`).
pub fn get_agent_inbox_v2(conn: &Connection, address: &str) -> SqlResult<Option<AgentInboxView>> {
    let resolved = resolve_agent_alias_v2(conn, address)?
        .unwrap_or_else(|| address.to_string());

    let agents = list_agents_v2(conn, true)?;
    let agent = match agents
        .into_iter()
        .find(|a| a.address.eq_ignore_ascii_case(&resolved))
    {
        Some(a) => a,
        None => return Ok(None),
    };

    let mut stmt = conn.prepare(
        "SELECT s.thread_id, t.kind, t.title, s.mode, s.last_read_seq
         FROM subscriptions s
         JOIN threads t ON t.id = s.thread_id
         WHERE lower(s.agent_address) = lower(?1)
           AND t.muted = 0
           AND s.mode != 'muted'
         ORDER BY t.updated_at DESC",
    )?;
    let subs = stmt
        .query_map(params![agent.address], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, i64>(4)?,
            ))
        })?
        .collect::<SqlResult<Vec<_>>>()?;

    let target_lower = agent.address.to_ascii_lowercase();
    let mut items = Vec::new();
    let mut total_unread = 0;

    for (thread_id, kind, title, mode, last_read_seq) in subs {
        let mut mstmt = conn.prepare(
            "SELECT seq, sender, body, mentions_json, msg_type, created_at
             FROM messages
             WHERE thread_id = ?1 AND seq > ?2 AND lower(sender) != ?3
             ORDER BY seq ASC",
        )?;
        let msgs = mstmt
            .query_map(params![thread_id, last_read_seq, target_lower], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                ))
            })?
            .collect::<SqlResult<Vec<_>>>()?;

        let matching: Vec<_> = msgs
            .into_iter()
            .filter(|(_, _, _, mjson, _, _)| {
                if kind == "dm" || mode == "all" || mode == "digest" {
                    true
                } else {
                    let mentions: Vec<String> = serde_json::from_str(mjson).unwrap_or_default();
                    mentions.iter().any(|m| {
                        let ml = m.to_ascii_lowercase();
                        ml == target_lower || ml == "all" || ml == "everyone"
                    })
                }
            })
            .collect();

        if let Some((latest_seq, latest_sender, latest_body, _, latest_type, latest_at)) =
            matching.last()
        {
            let count = matching.len();
            total_unread += count;
            items.push(AgentInboxItem {
                thread_id,
                thread_kind: kind,
                thread_title: title,
                unread_count: count,
                last_read_seq,
                latest_seq: *latest_seq,
                latest_sender: latest_sender.clone(),
                latest_body: latest_body.chars().take(180).collect(),
                latest_msg_type: MessageType::from_str_lossy(latest_type),
                latest_at: latest_at.clone(),
            });
        }
    }

    Ok(Some(AgentInboxView {
        agent,
        total_unread,
        items,
    }))
}

/// Auto-archive offline ephemeral agents that have 0 unread messages and have been inactive
/// for at least `days` (default: 14 days). Never archives `Boss`.
pub fn archive_stale_ephemeral_agents(
    conn: &Connection,
    days: Option<i64>,
) -> SqlResult<ArchiveReport> {
    let cutoff_days = days.unwrap_or(DEFAULT_ARCHIVE_DAYS).max(1);
    let cutoff_iso = (Utc::now() - Duration::days(cutoff_days))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();

    let mut stmt = conn.prepare(
        "SELECT address FROM agents
         WHERE archived = 0
           AND ephemeral = 1
           AND status != 'online'
           AND address != ?1
           AND last_seen_at < ?2",
    )?;
    let candidates = stmt
        .query_map(params![BOSS_ADDRESS, cutoff_iso], |r| r.get::<_, String>(0))?
        .collect::<SqlResult<Vec<_>>>()?;

    let ts = now_iso();
    let mut archived_agents = Vec::new();
    for addr in candidates {
        if count_agent_unread(conn, &addr)? == 0 {
            conn.execute(
                "UPDATE agents SET archived = 1, archived_at = ?1 WHERE address = ?2",
                params![ts, addr],
            )?;
            archived_agents.push(addr);
        }
    }

    Ok(ArchiveReport {
        cutoff_days,
        archived_count: archived_agents.len(),
        archived_agents,
    })
}

/// Manually archive or unarchive an agent (Boss cannot be archived).
pub fn set_agent_archived(conn: &Connection, address: &str, archived: bool) -> SqlResult<bool> {
    if is_boss_or_human(address) {
        return Ok(false);
    }
    let ts = if archived { Some(now_iso()) } else { None };
    let changed = conn.execute(
        "UPDATE agents SET archived = ?1, archived_at = ?2 WHERE address = ?3",
        params![if archived { 1 } else { 0 }, ts, address],
    )?;
    Ok(changed > 0)
}

/// Fast aggregate stats for the Admin Console header.
pub fn admin_overview(conn: &Connection) -> SqlResult<AdminOverview> {
    let paused: bool = conn
        .query_row(
            "SELECT value FROM hub_meta WHERE key = 'paused'",
            [],
            |r| r.get::<_, String>(0),
        )
        .optional()?
        .map(|v| v == "true")
        .unwrap_or(false);

    // Exclude permanent Boss profile so online/offline counts match the AI Agents rail
    let online_agents = conn.query_row(
        "SELECT COUNT(*) FROM agents WHERE archived = 0 AND status = 'online' AND address != ?1",
        params![BOSS_ADDRESS],
        |r| r.get::<_, i64>(0),
    )? as usize;

    let offline_agents = conn.query_row(
        "SELECT COUNT(*) FROM agents WHERE archived = 0 AND status != 'online' AND address != ?1",
        params![BOSS_ADDRESS],
        |r| r.get::<_, i64>(0),
    )? as usize;

    let archived_agents = conn.query_row(
        "SELECT COUNT(*) FROM agents WHERE archived = 1",
        [],
        |r| r.get::<_, i64>(0),
    )? as usize;

    let forum_count = conn.query_row(
        "SELECT COUNT(*) FROM threads WHERE kind = 'forum'",
        [],
        |r| r.get::<_, i64>(0),
    )? as usize;

    let dm_count = conn.query_row(
        "SELECT COUNT(*) FROM threads WHERE kind = 'dm'",
        [],
        |r| r.get::<_, i64>(0),
    )? as usize;

    let total_messages = conn.query_row("SELECT COUNT(*) FROM messages", [], |r| {
        r.get::<_, i64>(0)
    })? as usize;

    let open_handoffs = conn.query_row(
        "SELECT COUNT(*) FROM messages WHERE status = 'open'",
        [],
        |r| r.get::<_, i64>(0),
    )? as usize;

    let boss_unread = count_agent_unread(conn, BOSS_ADDRESS)?;

    Ok(AdminOverview {
        paused,
        online_agents,
        offline_agents,
        archived_agents,
        forum_count,
        dm_count,
        total_messages,
        boss_unread,
        open_handoffs,
    })
}
