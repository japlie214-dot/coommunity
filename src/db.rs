use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use uuid::Uuid;

use crate::models::{
    derive_agent_address, mention_matches_agent, sanitize_slug, Agent, AgentStatus, DeliveryRecord,
    HubStatus, InboxItem, Message, RegisterAgentRequest, RuntimeKind, Subscription,
    SubscriptionMode, Thread, ThreadKind, TransportKind,
};

#[derive(Clone)]
pub struct Database {
    conn: Arc<Mutex<Connection>>,
}

impl Database {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create data dir {}", parent.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("failed to open sqlite db {}", path.display()))?;
        conn.execute_batch(
            "
            PRAGMA journal_mode = WAL;
            PRAGMA foreign_keys = ON;
            PRAGMA busy_timeout = 5000;

            CREATE TABLE IF NOT EXISTS hub_meta (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS agents (
                address TEXT PRIMARY KEY,
                runtime TEXT NOT NULL,
                session_id TEXT,
                cwd TEXT,
                status TEXT NOT NULL,
                transport TEXT NOT NULL,
                endpoint TEXT,
                auth_token TEXT,
                allow_wake_resume INTEGER NOT NULL DEFAULT 0,
                last_seen_at TEXT NOT NULL,
                registered_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS agent_sessions (
                id TEXT PRIMARY KEY,
                agent_address TEXT NOT NULL,
                endpoint TEXT,
                auth_token TEXT,
                status TEXT NOT NULL,
                last_seen_at TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_agent_sessions_addr ON agent_sessions(agent_address);

            CREATE TABLE IF NOT EXISTS threads (
                id TEXT PRIMARY KEY,
                kind TEXT NOT NULL,
                title TEXT NOT NULL,
                description TEXT,
                created_by TEXT NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                hop_count INTEGER NOT NULL DEFAULT 0,
                max_hops INTEGER NOT NULL DEFAULT 8,
                muted INTEGER NOT NULL DEFAULT 0
            );

            CREATE TABLE IF NOT EXISTS subscriptions (
                thread_id TEXT NOT NULL,
                agent_address TEXT NOT NULL,
                mode TEXT NOT NULL,
                last_read_seq INTEGER NOT NULL DEFAULT 0,
                updated_at TEXT NOT NULL,
                PRIMARY KEY (thread_id, agent_address),
                FOREIGN KEY (thread_id) REFERENCES threads(id) ON DELETE CASCADE
            );

            CREATE TABLE IF NOT EXISTS messages (
                id TEXT PRIMARY KEY,
                thread_id TEXT NOT NULL,
                seq INTEGER NOT NULL,
                sender TEXT NOT NULL,
                body TEXT NOT NULL,
                mentions_json TEXT NOT NULL DEFAULT '[]',
                ack_only INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL,
                FOREIGN KEY (thread_id) REFERENCES threads(id) ON DELETE CASCADE
            );

            CREATE UNIQUE INDEX IF NOT EXISTS idx_messages_thread_seq
                ON messages(thread_id, seq);

            CREATE TABLE IF NOT EXISTS deliveries (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                message_id TEXT NOT NULL,
                thread_id TEXT NOT NULL,
                target_agent TEXT NOT NULL,
                tier TEXT NOT NULL,
                status TEXT NOT NULL,
                detail TEXT,
                created_at TEXT NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_deliveries_created
                ON deliveries(created_at DESC);
            ",
        )?;

        conn.execute(
            "INSERT OR IGNORE INTO hub_meta (key, value) VALUES ('paused', 'false')",
            [],
        )?;

        crate::v2_db::apply_v2_migrations(&conn)?;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Expose shared SQLite connection handle for v2 handlers and background sweepers.
    pub fn conn_arc(&self) -> Arc<Mutex<Connection>> {
        self.conn.clone()
    }

    fn lock_conn(&self) -> Result<MutexGuard<'_, Connection>> {
        self.conn
            .lock()
            .map_err(|_| anyhow!("sqlite connection mutex poisoned"))
    }

    pub fn is_paused(&self) -> Result<bool> {
        let conn = self.lock_conn()?;
        let val: Option<String> = conn
            .query_row(
                "SELECT value FROM hub_meta WHERE key = 'paused'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        Ok(val.as_deref() == Some("true"))
    }

    pub fn set_paused(&self, paused: bool) -> Result<()> {
        let conn = self.lock_conn()?;
        conn.execute(
            "INSERT INTO hub_meta (key, value) VALUES ('paused', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![if paused { "true" } else { "false" }],
        )?;
        Ok(())
    }

    pub fn upsert_agent(&self, req: &RegisterAgentRequest) -> Result<Agent> {
        let now = Utc::now().to_rfc3339();
        let runtime = RuntimeKind::from_str(&req.runtime);
        let fallback_id = Uuid::new_v4().to_string()[..6].to_string();
        let address = req
            .address
            .clone()
            .filter(|a| !a.trim().is_empty())
            .or_else(|| derive_agent_address(&runtime, req.cwd.as_deref(), Some(&fallback_id)))
            .unwrap_or_else(|| format!("{}:{}", runtime.prefix(), fallback_id));

        let transport = req
            .transport
            .as_deref()
            .map(TransportKind::from_str)
            .unwrap_or(TransportKind::PollOnly);

        let status = req
            .status
            .as_deref()
            .map(AgentStatus::from_str)
            .unwrap_or(AgentStatus::Online);

        // Preserve existing allow_wake_resume when callers (such as hooks) pass None.
        let allow_wake_opt: Option<i64> = req.allow_wake_resume.map(|b| if b { 1 } else { 0 });

        let session_key = req
            .session_id
            .clone()
            .or_else(|| req.endpoint.clone())
            .unwrap_or_else(|| Uuid::new_v4().to_string());

        {
            let conn = self.lock_conn()?;

            // Track active session instance without clobbering sibling windows in the same cwd
            conn.execute(
                "INSERT INTO agent_sessions (
                    id, agent_address, endpoint, auth_token, status, last_seen_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(id) DO UPDATE SET
                    endpoint = COALESCE(excluded.endpoint, agent_sessions.endpoint),
                    auth_token = COALESCE(excluded.auth_token, agent_sessions.auth_token),
                    status = excluded.status,
                    last_seen_at = excluded.last_seen_at",
                params![
                    &session_key,
                    &address,
                    &req.endpoint,
                    &req.auth_token,
                    status.as_str(),
                    &now,
                ],
            )?;

            let existing_reg: Option<String> = conn
                .query_row(
                    "SELECT registered_at FROM agents WHERE address = ?1",
                    params![&address],
                    |r| r.get(0),
                )
                .optional()?;

            let registered_at = existing_reg.unwrap_or_else(|| now.clone());

            conn.execute(
                "INSERT INTO agents (
                    address, runtime, session_id, cwd, status, transport,
                    endpoint, auth_token, allow_wake_resume, last_seen_at, registered_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, COALESCE(?9, 0), ?10, ?11)
                 ON CONFLICT(address) DO UPDATE SET
                    runtime = excluded.runtime,
                    session_id = COALESCE(excluded.session_id, agents.session_id),
                    cwd = COALESCE(excluded.cwd, agents.cwd),
                    status = excluded.status,
                    transport = excluded.transport,
                    endpoint = COALESCE(excluded.endpoint, agents.endpoint),
                    auth_token = COALESCE(excluded.auth_token, agents.auth_token),
                    allow_wake_resume = COALESCE(?9, agents.allow_wake_resume),
                    last_seen_at = excluded.last_seen_at",
                params![
                    &address,
                    runtime.as_str(),
                    &req.session_id,
                    &req.cwd,
                    status.as_str(),
                    transport.as_str(),
                    &req.endpoint,
                    &req.auth_token,
                    allow_wake_opt,
                    &now,
                    &registered_at,
                ],
            )?;
        }

        self.get_agent(&address)?
            .context("agent missing immediately after upsert")
    }

    pub fn set_agent_status(&self, address: &str, status: AgentStatus) -> Result<Option<Agent>> {
        self.set_agent_session_status(address, None, status)
    }

    pub fn set_agent_session_status(
        &self,
        address: &str,
        session_id: Option<&str>,
        status: AgentStatus,
    ) -> Result<Option<Agent>> {
        let now = Utc::now().to_rfc3339();
        {
            let conn = self.lock_conn()?;

            if let Some(sid) = session_id {
                conn.execute(
                    "UPDATE agent_sessions SET status = ?1, last_seen_at = ?2 WHERE id = ?3",
                    params![status.as_str(), &now, sid],
                )?;

                if status == AgentStatus::Offline {
                    let online_count: i64 = conn.query_row(
                        "SELECT COUNT(*) FROM agent_sessions WHERE agent_address = ?1 AND status = 'online'",
                        params![address],
                        |r| r.get(0),
                    )?;
                    if online_count == 0 {
                        conn.execute(
                            "UPDATE agents SET status = 'offline', last_seen_at = ?1 WHERE address = ?2",
                            params![&now, address],
                        )?;
                    }
                } else {
                    conn.execute(
                        "UPDATE agents SET status = ?1, last_seen_at = ?2 WHERE address = ?3",
                        params![status.as_str(), &now, address],
                    )?;
                }
            } else {
                conn.execute(
                    "UPDATE agent_sessions SET status = ?1, last_seen_at = ?2 WHERE agent_address = ?3",
                    params![status.as_str(), &now, address],
                )?;
                conn.execute(
                    "UPDATE agents SET status = ?1, last_seen_at = ?2 WHERE address = ?3",
                    params![status.as_str(), &now, address],
                )?;
            }
        }

        self.get_agent(address)
    }

    pub fn get_active_sessions(&self, address: &str) -> Result<Vec<(Option<String>, Option<String>)>> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT endpoint, auth_token FROM agent_sessions 
             WHERE agent_address = ?1 AND status = 'online'
             ORDER BY last_seen_at DESC",
        )?;
        let rows = stmt.query_map(params![address], |row| {
            Ok((row.get::<_, Option<String>>(0)?, row.get::<_, Option<String>>(1)?))
        })?;
        let mut list = Vec::new();
        for r in rows {
            list.push(r?);
        }
        Ok(list)
    }

    pub fn resolve_recipient_alias(&self, target: &str) -> String {
        let normalized = target.trim().trim_start_matches('@').to_ascii_lowercase();
        if matches!(normalized.as_str(), "boss" | "human" | "user" | "admin") {
            return crate::v2_models::BOSS_ADDRESS.to_string();
        }
        let runtime_match = match normalized.as_str() {
            "cc" | "claude" | "claude_code" | "claude-code" => Some(("claude", "cc:%")),
            "cc-desktop" | "claude_desktop" | "claude-desktop" => {
                Some(("claude_desktop", "cc-desktop:%"))
            }
            "codex" | "codex-cli" | "codex_cli" => Some(("codex", "codex:%")),
            "agy" | "antigravity" | "antigravity-cli" => Some(("antigravity", "agy:%")),
            _ => None,
        };

        if let Some((rt_prefix, addr_like)) = runtime_match {
            if let Ok(conn) = self.lock_conn() {
                let resolved: Result<Option<String>, _> = conn.query_row(
                    "SELECT address FROM agents 
                     WHERE (runtime LIKE ?1 OR address LIKE ?2) AND status = 'online'
                     ORDER BY last_seen_at DESC LIMIT 1",
                    params![format!("%{rt_prefix}%"), addr_like],
                    |r| r.get(0),
                );
                if let Ok(Some(addr)) = resolved {
                    return addr;
                }
                let fallback: Result<Option<String>, _> = conn.query_row(
                    "SELECT address FROM agents 
                     WHERE (runtime LIKE ?1 OR address LIKE ?2)
                     ORDER BY last_seen_at DESC LIMIT 1",
                    params![format!("%{rt_prefix}%"), addr_like],
                    |r| r.get(0),
                );
                if let Ok(Some(addr)) = fallback {
                    return addr;
                }
            }
        }

        target.trim().trim_start_matches('@').to_string()
    }

    pub fn get_agent(&self, address: &str) -> Result<Option<Agent>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT address, runtime, session_id, cwd, status, transport,
                    endpoint, auth_token, allow_wake_resume, last_seen_at, registered_at
             FROM agents WHERE address = ?1",
            params![address],
            row_to_agent,
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn list_agents(&self) -> Result<Vec<Agent>> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT address, runtime, session_id, cwd, status, transport,
                    endpoint, auth_token, allow_wake_resume, last_seen_at, registered_at
             FROM agents ORDER BY
                CASE status WHEN 'online' THEN 0 WHEN 'busy' THEN 1 WHEN 'idle' THEN 2 ELSE 3 END,
                last_seen_at DESC",
        )?;
        let rows = stmt.query_map([], row_to_agent)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn get_or_create_dm_thread(
        &self,
        agent_a: &str,
        agent_b: &str,
        default_max_hops: i64,
    ) -> Result<Thread> {
        let mut pair = [agent_a.to_string(), agent_b.to_string()];
        pair.sort_by(|a, b| a.to_ascii_lowercase().cmp(&b.to_ascii_lowercase()));
        let dm_id = format!("dm:{}:{}", sanitize_slug(&pair[0]), sanitize_slug(&pair[1]));
        let now = Utc::now().to_rfc3339();

        if let Some(existing) = self.get_thread(&dm_id)? {
            return Ok(existing);
        }

        let title = format!("{} <-> {}", pair[0], pair[1]);
        {
            let conn = self.lock_conn()?;
            conn.execute(
                "INSERT OR IGNORE INTO threads (
                    id, kind, title, description, created_by, created_at, updated_at,
                    hop_count, max_hops, muted
                ) VALUES (?1, 'dm', ?2, NULL, ?3, ?4, ?4, 0, ?5, 0)",
                params![&dm_id, &title, agent_a, &now, default_max_hops],
            )?;
            for member in &pair {
                conn.execute(
                    "INSERT OR IGNORE INTO subscriptions (
                        thread_id, agent_address, mode, last_read_seq, updated_at
                    ) VALUES (?1, ?2, 'all', 0, ?3)",
                    params![&dm_id, member, &now],
                )?;
            }
        }

        self.get_thread(&dm_id)?
            .context("dm thread missing after create")
    }

    /// Verifies if a given agent address is an authorized participant in a DM thread.
    pub fn is_dm_participant(&self, thread_id: &str, agent: &str) -> Result<bool> {
        let clean_agent = agent.trim().trim_start_matches('@');
        if clean_agent.is_empty() {
            return Ok(false);
        }

        // Fast path: check thread ID slug components (dm:<sanitized_a>:<sanitized_b>)
        if let Some(rest) = thread_id.strip_prefix("dm:") {
            let parts: Vec<&str> = rest.split(':').collect();
            if parts.len() == 2 {
                let sanitized_caller = sanitize_slug(clean_agent);
                if parts[0].eq_ignore_ascii_case(&sanitized_caller)
                    || parts[1].eq_ignore_ascii_case(&sanitized_caller)
                {
                    return Ok(true);
                }
            }
        }

        // Check active subscriptions on this thread
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT agent_address FROM subscriptions WHERE thread_id = ?1",
        )?;
        let rows = stmt.query_map(params![thread_id], |r| r.get::<_, String>(0))?;
        for r in rows {
            let addr = r?;
            if addr.eq_ignore_ascii_case(clean_agent)
                || mention_matches_agent(&addr, clean_agent)
                || mention_matches_agent(clean_agent, &addr)
            {
                return Ok(true);
            }
        }

        Ok(false)
    }

    /// Checks if an agent is authorized to access a thread (forums: open to all; DMs: participants only).
    pub fn can_access_thread(&self, thread_id: &str, agent: &str) -> Result<bool> {
        let thread = self.get_thread(thread_id)?;
        match thread {
            None => Ok(false),
            Some(t) => {
                if t.kind == ThreadKind::Dm {
                    self.is_dm_participant(thread_id, agent)
                } else {
                    Ok(true) // Forum threads are open to all
                }
            }
        }
    }

    pub fn create_forum_thread(
        &self,
        title: &str,
        description: Option<&str>,
        created_by: &str,
        members: &[String],
        default_mode: SubscriptionMode,
        max_hops: i64,
    ) -> Result<Thread> {
        let slug = sanitize_slug(title);
        let id = if slug.is_empty() {
            format!("forum:{}", &Uuid::new_v4().to_string()[..8])
        } else {
            format!("forum:{}-{}", slug, &Uuid::new_v4().to_string()[..4])
        };
        let now = Utc::now().to_rfc3339();

        {
            let conn = self.lock_conn()?;
            conn.execute(
                "INSERT INTO threads (
                    id, kind, title, description, created_by, created_at, updated_at,
                    hop_count, max_hops, muted
                ) VALUES (?1, 'forum', ?2, ?3, ?4, ?5, ?5, 0, ?6, 0)",
                params![&id, title, description, created_by, &now, max_hops],
            )?;

            // Creator + initial members subscribe with default_mode (typically `mentions`)
            let mut all_members: Vec<String> = members.to_vec();
            if !all_members.iter().any(|m| m == created_by) {
                all_members.push(created_by.to_string());
            }
            for member in all_members {
                conn.execute(
                    "INSERT OR IGNORE INTO subscriptions (
                        thread_id, agent_address, mode, last_read_seq, updated_at
                    ) VALUES (?1, ?2, ?3, 0, ?4)",
                    params![&id, &member, default_mode.as_str(), &now],
                )?;
            }
        }

        self.get_thread(&id)?
            .context("forum thread missing after create")
    }

    pub fn get_thread(&self, thread_id: &str) -> Result<Option<Thread>> {
        let conn = self.lock_conn()?;
        conn.query_row(
            "SELECT t.id, t.kind, t.title, t.description, t.created_by, t.created_at,
                    t.updated_at, t.hop_count, t.max_hops, t.muted,
                    (SELECT COUNT(*) FROM messages m WHERE m.thread_id = t.id) AS msg_count
             FROM threads t WHERE t.id = ?1",
            params![thread_id],
            row_to_thread,
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn list_threads(&self) -> Result<Vec<Thread>> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT t.id, t.kind, t.title, t.description, t.created_by, t.created_at,
                    t.updated_at, t.hop_count, t.max_hops, t.muted,
                    (SELECT COUNT(*) FROM messages m WHERE m.thread_id = t.id) AS msg_count
             FROM threads t ORDER BY t.updated_at DESC",
        )?;
        let rows = stmt.query_map([], row_to_thread)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Lists threads visible to the given reader address.
    /// Public forums are visible to everyone. Private DMs are only visible to their participants.
    /// When reader is None, returns only public forum threads.
    pub fn list_threads_for_agent(&self, reader: Option<&str>) -> Result<Vec<Thread>> {
        let all = self.list_threads()?;
        match reader.map(str::trim).filter(|r| !r.is_empty()) {
            Some(r) => {
                let mut visible = Vec::new();
                for t in all {
                    if t.kind == ThreadKind::Dm {
                        if self.is_dm_participant(&t.id, r)? {
                            visible.push(t);
                        }
                    } else {
                        visible.push(t);
                    }
                }
                Ok(visible)
            }
            None => {
                // Anonymous or unauthenticated: only expose public forum threads
                Ok(all.into_iter().filter(|t| t.kind != ThreadKind::Dm).collect())
            }
        }
    }

    pub fn set_thread_muted(&self, thread_id: &str, muted: bool) -> Result<Option<Thread>> {
        {
            let conn = self.lock_conn()?;
            let rows = conn.execute(
                "UPDATE threads SET muted = ?1 WHERE id = ?2",
                params![if muted { 1 } else { 0 }, thread_id],
            )?;
            if rows == 0 {
                return Ok(None);
            }
        }
        self.get_thread(thread_id)
    }

    pub fn reset_thread_hops(&self, thread_id: &str) -> Result<Option<Thread>> {
        {
            let conn = self.lock_conn()?;
            let rows = conn.execute(
                "UPDATE threads SET hop_count = 0 WHERE id = ?1",
                params![thread_id],
            )?;
            if rows == 0 {
                return Ok(None);
            }
        }
        self.get_thread(thread_id)
    }

    pub fn upsert_subscription(
        &self,
        thread_id: &str,
        agent_address: &str,
        mode: SubscriptionMode,
    ) -> Result<Subscription> {
        let now = Utc::now().to_rfc3339();
        let conn = self.lock_conn()?;
        conn.execute(
            "INSERT INTO subscriptions (thread_id, agent_address, mode, last_read_seq, updated_at)
             VALUES (?1, ?2, ?3, 0, ?4)
             ON CONFLICT(thread_id, agent_address) DO UPDATE SET
                mode = excluded.mode,
                updated_at = excluded.updated_at",
            params![thread_id, agent_address, mode.as_str(), &now],
        )?;
        conn.query_row(
            "SELECT thread_id, agent_address, mode, last_read_seq, updated_at
             FROM subscriptions WHERE thread_id = ?1 AND agent_address = ?2",
            params![thread_id, agent_address],
            row_to_subscription,
        )
        .map_err(Into::into)
    }

    pub fn remove_subscription(&self, thread_id: &str, agent_address: &str) -> Result<bool> {
        let conn = self.lock_conn()?;
        let rows = conn.execute(
            "DELETE FROM subscriptions WHERE thread_id = ?1 AND agent_address = ?2",
            params![thread_id, agent_address],
        )?;
        Ok(rows > 0)
    }

    pub fn list_subscriptions(&self, thread_id: Option<&str>) -> Result<Vec<Subscription>> {
        let conn = self.lock_conn()?;
        let mut out = Vec::new();
        if let Some(tid) = thread_id {
            let mut stmt = conn.prepare(
                "SELECT thread_id, agent_address, mode, last_read_seq, updated_at
                 FROM subscriptions WHERE thread_id = ?1 ORDER BY agent_address ASC",
            )?;
            let rows = stmt.query_map(params![tid], row_to_subscription)?;
            for r in rows {
                out.push(r?);
            }
        } else {
            let mut stmt = conn.prepare(
                "SELECT thread_id, agent_address, mode, last_read_seq, updated_at
                 FROM subscriptions ORDER BY thread_id ASC, agent_address ASC",
            )?;
            let rows = stmt.query_map([], row_to_subscription)?;
            for r in rows {
                out.push(r?);
            }
        }
        Ok(out)
    }

    pub fn post_message(
        &self,
        thread_id: &str,
        sender: &str,
        body: &str,
        explicit_mentions: &[String],
        ack_only: bool,
    ) -> Result<(Message, Thread)> {
        let thread_before = self
            .get_thread(thread_id)?
            .context("thread not found")?;
        if thread_before.kind == ThreadKind::Dm && !self.is_dm_participant(thread_id, sender)? {
            anyhow::bail!(
                "Access denied: sender '{}' is not a participant in DM thread '{}'",
                sender,
                thread_id
            );
        }

        let now = Utc::now().to_rfc3339();
        let id = Uuid::new_v4().to_string();

        // Merge explicit mentions with inline @address mentions parsed from body
        let mut mentions: Vec<String> = explicit_mentions
            .iter()
            .map(|m| m.trim().trim_start_matches('@').to_string())
            .filter(|m| !m.is_empty())
            .collect();
        for token in body.split_whitespace() {
            if let Some(rest) = token.strip_prefix('@') {
                let clean = rest
                    .trim_end_matches(|c: char| matches!(c, ',' | '.' | ':' | ';' | '!' | '?' | ')'))
                    .to_string();
                if !clean.is_empty() && !mentions.iter().any(|m| m.eq_ignore_ascii_case(&clean)) {
                    mentions.push(clean);
                }
            }
        }
        let mentions_json = serde_json::to_string(&mentions)?;

        let seq: i64 = {
            let conn = self.lock_conn()?;
            let next_seq: i64 = conn.query_row(
                "SELECT COALESCE(MAX(seq), 0) + 1 FROM messages WHERE thread_id = ?1",
                params![thread_id],
                |r| r.get(0),
            )?;

            conn.execute(
                "INSERT INTO messages (id, thread_id, seq, sender, body, mentions_json, ack_only, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    &id,
                    thread_id,
                    next_seq,
                    sender,
                    body,
                    &mentions_json,
                    if ack_only { 1 } else { 0 },
                    &now,
                ],
            )?;

            // Increment hop_count unless sender is human/Boss (human resets loop counter)
            if sender.eq_ignore_ascii_case("human") || sender.eq_ignore_ascii_case("boss") {
                conn.execute(
                    "UPDATE threads SET updated_at = ?1, hop_count = 0 WHERE id = ?2",
                    params![&now, thread_id],
                )?;
            } else {
                conn.execute(
                    "UPDATE threads SET updated_at = ?1, hop_count = hop_count + 1 WHERE id = ?2",
                    params![&now, thread_id],
                )?;
            }

            // Auto-subscribe any @mentioned registered agents who are not yet subscribed to this
            // thread so their unread inbox & hook reminders never miss a mention.
            if !mentions.is_empty() {
                let mut stmt = conn.prepare("SELECT address, status FROM agents")?;
                let registered: Vec<(String, String)> = stmt
                    .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect::<std::result::Result<_, _>>()?;
                drop(stmt);

                let mut to_subscribe: HashSet<String> = HashSet::new();
                for mention in &mentions {
                    if mention.eq_ignore_ascii_case("all")
                        || mention.eq_ignore_ascii_case("everyone")
                    {
                        for (addr, status) in &registered {
                            if addr != sender && status != "offline" {
                                to_subscribe.insert(addr.clone());
                            }
                        }
                    } else {
                        let mut matched_any = false;
                        for (addr, _) in &registered {
                            if addr != sender && mention_matches_agent(mention, addr) {
                                to_subscribe.insert(addr.clone());
                                matched_any = true;
                            }
                        }
                        // Allow mentioning an explicit canonical address even before first registration
                        if !matched_any && mention.contains(':') && mention != sender {
                            to_subscribe.insert(mention.clone());
                        }
                    }
                }

                // Set last_read_seq = next_seq - 1 so only this mention (and later messages) are unread
                let initial_read_seq = next_seq.saturating_sub(1);
                for addr in to_subscribe {
                    conn.execute(
                        "INSERT OR IGNORE INTO subscriptions (
                            thread_id, agent_address, mode, last_read_seq, updated_at
                         ) VALUES (?1, ?2, 'mentions', ?3, ?4)",
                        params![thread_id, &addr, initial_read_seq, &now],
                    )?;
                }
            }

            // Advance sender's own read cursor so they never see their own message as unread
            conn.execute(
                "UPDATE subscriptions SET last_read_seq = MAX(last_read_seq, ?1), updated_at = ?2
                 WHERE thread_id = ?3 AND agent_address = ?4",
                params![next_seq, &now, thread_id, sender],
            )?;

            next_seq
        };

        let msg = Message {
            id,
            thread_id: thread_id.to_string(),
            seq,
            sender: sender.to_string(),
            body: body.to_string(),
            mentions,
            ack_only,
            created_at: now,
        };
        let thread = self
            .get_thread(thread_id)?
            .context("thread missing after post_message")?;

        Ok((msg, thread))
    }

    pub fn read_messages(&self, thread_id: &str, since_seq: i64, limit: i64) -> Result<Vec<Message>> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT id, thread_id, seq, sender, body, mentions_json, ack_only, created_at
             FROM messages
             WHERE thread_id = ?1 AND seq > ?2
             ORDER BY seq ASC
             LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![thread_id, since_seq, limit], row_to_message)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Reads messages in a thread and, when `reader` is provided with `auto_ack = true`,
    /// automatically advances the reader's `last_read_seq` to the latest returned message.
    /// DM threads are strictly restricted to authorized participants.
    pub fn read_messages_for_agent(
        &self,
        thread_id: &str,
        since_seq: i64,
        limit: i64,
        reader: Option<&str>,
        auto_ack: bool,
    ) -> Result<Vec<Message>> {
        let thread = self
            .get_thread(thread_id)?
            .context("thread not found")?;
        if thread.kind == ThreadKind::Dm {
            match reader.map(str::trim).filter(|r| !r.is_empty()) {
                Some(r) => {
                    if !self.is_dm_participant(thread_id, r)? {
                        anyhow::bail!(
                            "Access denied: agent '{}' is not a participant in DM thread '{}'",
                            r,
                            thread_id
                        );
                    }
                }
                None => {
                    anyhow::bail!(
                        "Access denied: a reader agent address is required to read private DM thread '{}'",
                        thread_id
                    );
                }
            }
        }
        let msgs = self.read_messages(thread_id, since_seq, limit)?;
        if auto_ack {
            if let (Some(addr), Some(last)) = (reader, msgs.last()) {
                self.ack_inbox(addr, thread_id, last.seq)?;
            }
        }
        Ok(msgs)
    }

    pub fn ack_inbox(&self, agent_address: &str, thread_id: &str, up_to_seq: i64) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        let conn = self.lock_conn()?;
        conn.execute(
            "INSERT INTO subscriptions (thread_id, agent_address, mode, last_read_seq, updated_at)
             VALUES (?1, ?2, 'mentions', ?3, ?4)
             ON CONFLICT(thread_id, agent_address) DO UPDATE SET
                last_read_seq = MAX(subscriptions.last_read_seq, excluded.last_read_seq),
                updated_at = excluded.updated_at",
            params![thread_id, agent_address, up_to_seq, &now],
        )?;
        Ok(())
    }

    /// Marks every subscribed thread for `agent_address` as read up to its latest message seq.
    pub fn ack_all_inbox(&self, agent_address: &str) -> Result<usize> {
        let now = Utc::now().to_rfc3339();
        let conn = self.lock_conn()?;
        let updated = conn.execute(
            "UPDATE subscriptions
             SET last_read_seq = MAX(
                    last_read_seq,
                    COALESCE((SELECT MAX(m.seq) FROM messages m WHERE m.thread_id = subscriptions.thread_id), 0)
                 ),
                 updated_at = ?1
             WHERE agent_address = ?2",
            params![&now, agent_address],
        )?;
        Ok(updated)
    }

    /// Returns unread inbox items for `agent_address`, respecting subscription modes:
    /// - DMs and `all`/`digest` forum subscriptions count all messages after `last_read_seq`.
    /// - `mentions`-mode forum subscriptions count ONLY messages that actually `@mention` this agent.
    /// - Muted threads and muted subscriptions are excluded.
    pub fn check_inbox(&self, agent_address: &str) -> Result<Vec<InboxItem>> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT t.id, t.title, t.kind, s.mode,
                    m.seq, m.sender, m.body, m.mentions_json, m.created_at
             FROM subscriptions s
             JOIN threads t ON t.id = s.thread_id
             JOIN messages m ON m.thread_id = t.id
             WHERE s.agent_address = ?1
               AND m.seq > s.last_read_seq
               AND m.sender != ?1
               AND s.mode != 'muted'
               AND t.muted = 0
             ORDER BY t.id ASC, m.seq ASC",
        )?;

        struct Acc {
            thread_id: String,
            thread_title: String,
            thread_kind: ThreadKind,
            unread_count: i64,
            mentioned_count: i64,
            latest_seq: i64,
            latest_sender: String,
            latest_preview: String,
            latest_at: String,
        }

        let mut grouped: BTreeMap<String, Acc> = BTreeMap::new();
        let rows = stmt.query_map(params![agent_address], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, String>(6)?,
                r.get::<_, String>(7)?,
                r.get::<_, String>(8)?,
            ))
        })?;

        for row in rows {
            let (tid, title, kind_s, mode_s, seq, sender, body, mentions_json, created_at) = row?;
            let kind = ThreadKind::from_str(&kind_s);
            let mode = SubscriptionMode::from_str(&mode_s);
            let mentions: Vec<String> = serde_json::from_str(&mentions_json).unwrap_or_default();
            let is_mentioned = mentions
                .iter()
                .any(|m| mention_matches_agent(m, agent_address));

            if kind == ThreadKind::Forum && mode == SubscriptionMode::Mentions && !is_mentioned {
                continue;
            }

            let preview: String = body.chars().take(120).collect();
            let entry = grouped.entry(tid.clone()).or_insert_with(|| Acc {
                thread_id: tid,
                thread_title: title,
                thread_kind: kind,
                unread_count: 0,
                mentioned_count: 0,
                latest_seq: seq,
                latest_sender: sender.clone(),
                latest_preview: preview.clone(),
                latest_at: created_at.clone(),
            });

            entry.unread_count += 1;
            if is_mentioned {
                entry.mentioned_count += 1;
            }
            if seq >= entry.latest_seq {
                entry.latest_seq = seq;
                entry.latest_sender = sender;
                entry.latest_preview = preview;
                entry.latest_at = created_at;
            }
        }

        let mut items: Vec<InboxItem> = grouped
            .into_values()
            .map(|a| InboxItem {
                thread_id: a.thread_id,
                thread_title: a.thread_title,
                thread_kind: a.thread_kind,
                unread_count: a.unread_count,
                mentioned_count: a.mentioned_count,
                latest_seq: a.latest_seq,
                latest_sender: a.latest_sender,
                latest_preview: a.latest_preview,
                latest_at: a.latest_at,
            })
            .collect();

        items.sort_by(|a, b| b.latest_at.cmp(&a.latest_at));
        Ok(items)
    }

    pub fn log_delivery(
        &self,
        message_id: &str,
        thread_id: &str,
        target_agent: &str,
        tier: &str,
        status: &str,
        detail: Option<&str>,
    ) -> Result<DeliveryRecord> {
        let now = Utc::now().to_rfc3339();
        let conn = self.lock_conn()?;
        conn.execute(
            "INSERT INTO deliveries (message_id, thread_id, target_agent, tier, status, detail, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![message_id, thread_id, target_agent, tier, status, detail, &now],
        )?;
        let id = conn.last_insert_rowid();
        Ok(DeliveryRecord {
            id,
            message_id: message_id.to_string(),
            thread_id: thread_id.to_string(),
            target_agent: target_agent.to_string(),
            tier: tier.to_string(),
            status: status.to_string(),
            detail: detail.map(str::to_string),
            created_at: now,
        })
    }

    pub fn recent_deliveries(&self, limit: i64) -> Result<Vec<DeliveryRecord>> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT id, message_id, thread_id, target_agent, tier, status, detail, created_at
             FROM deliveries ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit], |r| {
            Ok(DeliveryRecord {
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
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn hub_status(&self) -> Result<HubStatus> {
        let paused = self.is_paused()?;
        let agents = self.list_agents()?;
        let threads = self.list_threads()?;
        let online_agents = agents
            .iter()
            .filter(|a| matches!(a.status, AgentStatus::Online | AgentStatus::Busy | AgentStatus::Idle))
            .count();
        let forum_threads = threads
            .iter()
            .filter(|t| t.kind == ThreadKind::Forum)
            .count();
        let dm_threads = threads.iter().filter(|t| t.kind == ThreadKind::Dm).count();
        let total_messages = threads.iter().map(|t| t.message_count).sum();

        Ok(HubStatus {
            paused,
            online_agents,
            total_agents: agents.len(),
            forum_threads,
            dm_threads,
            total_messages,
        })
    }
}

fn row_to_agent(r: &rusqlite::Row<'_>) -> rusqlite::Result<Agent> {
    let runtime_s: String = r.get(1)?;
    let status_s: String = r.get(4)?;
    let transport_s: String = r.get(5)?;
    let wake: i64 = r.get(8)?;
    Ok(Agent {
        address: r.get(0)?,
        runtime: RuntimeKind::from_str(&runtime_s),
        session_id: r.get(2)?,
        cwd: r.get(3)?,
        status: AgentStatus::from_str(&status_s),
        transport: TransportKind::from_str(&transport_s),
        endpoint: r.get(6)?,
        auth_token: r.get(7)?,
        allow_wake_resume: wake != 0,
        last_seen_at: r.get(9)?,
        registered_at: r.get(10)?,
    })
}

fn row_to_thread(r: &rusqlite::Row<'_>) -> rusqlite::Result<Thread> {
    let kind_s: String = r.get(1)?;
    let muted_i: i64 = r.get(9)?;
    Ok(Thread {
        id: r.get(0)?,
        kind: ThreadKind::from_str(&kind_s),
        title: r.get(2)?,
        description: r.get(3)?,
        created_by: r.get(4)?,
        created_at: r.get(5)?,
        updated_at: r.get(6)?,
        hop_count: r.get(7)?,
        max_hops: r.get(8)?,
        muted: muted_i != 0,
        message_count: r.get(10)?,
    })
}

fn row_to_subscription(r: &rusqlite::Row<'_>) -> rusqlite::Result<Subscription> {
    let mode_s: String = r.get(2)?;
    Ok(Subscription {
        thread_id: r.get(0)?,
        agent_address: r.get(1)?,
        mode: SubscriptionMode::from_str(&mode_s),
        last_read_seq: r.get(3)?,
        updated_at: r.get(4)?,
    })
}

fn row_to_message(r: &rusqlite::Row<'_>) -> rusqlite::Result<Message> {
    let mentions_json: String = r.get(5)?;
    let mentions: Vec<String> = serde_json::from_str(&mentions_json).unwrap_or_default();
    let ack_i: i64 = r.get(6)?;
    Ok(Message {
        id: r.get(0)?,
        thread_id: r.get(1)?,
        seq: r.get(2)?,
        sender: r.get(3)?,
        body: r.get(4)?,
        mentions,
        ack_only: ack_i != 0,
        created_at: r.get(7)?,
    })
}
