use anyhow::{Context, Result};
use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

use crate::models::{
    Agent, AgentStatus, DeliveryRecord, HubStatus, InboxItem, Message, RegisterAgentRequest,
    RuntimeKind, Subscription, SubscriptionMode, Thread, ThreadKind, TransportKind,
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

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    pub fn is_paused(&self) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
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
        let conn = self.conn.lock().unwrap();
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
        let address = req.address.clone().unwrap_or_else(|| {
            let suffix = req
                .cwd
                .as_deref()
                .and_then(|p| Path::new(p).file_name())
                .and_then(|n| n.to_str())
                .map(sanitize_slug)
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| Uuid::new_v4().to_string()[..6].to_string());
            format!("{}:{}", runtime.prefix(), suffix)
        });

        let transport = req
            .transport
            .as_deref()
            .map(TransportKind::from_str)
            .unwrap_or_else(|| infer_transport(&runtime, req.endpoint.as_deref()));

        let status = req
            .status
            .as_deref()
            .map(AgentStatus::from_str)
            .unwrap_or(AgentStatus::Online);

        let allow_wake = req.allow_wake_resume.unwrap_or(false);

        let session_key = req.session_id.clone()
            .or_else(|| req.endpoint.clone())
            .unwrap_or_else(|| Uuid::new_v4().to_string());

        let conn = self.conn.lock().unwrap();

        // Track active session instance without clobbering other windows in the same workspace
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
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
            ON CONFLICT(address) DO UPDATE SET
                runtime = excluded.runtime,
                session_id = COALESCE(excluded.session_id, agents.session_id),
                cwd = COALESCE(excluded.cwd, agents.cwd),
                status = excluded.status,
                transport = excluded.transport,
                endpoint = COALESCE(excluded.endpoint, agents.endpoint),
                auth_token = COALESCE(excluded.auth_token, agents.auth_token),
                allow_wake_resume = excluded.allow_wake_resume,
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
                if allow_wake { 1 } else { 0 },
                &now,
                &registered_at,
            ],
        )?;

        drop(conn);
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
        let conn = self.conn.lock().unwrap();

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

        drop(conn);
        self.get_agent(address)
    }

    pub fn get_active_sessions(&self, address: &str) -> Result<Vec<(Option<String>, Option<String>)>> {
        let conn = self.conn.lock().unwrap();
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
        let normalized = target.to_ascii_lowercase();
        let runtime_match = match normalized.as_str() {
            "cc" | "claude" => Some("claude"),
            "codex" => Some("codex"),
            "agy" | "antigravity" => Some("antigravity"),
            _ => None,
        };

        if let Some(rt_prefix) = runtime_match {
            let conn = self.conn.lock().unwrap();
            let resolved: Result<Option<String>, _> = conn.query_row(
                "SELECT address FROM agents 
                 WHERE (runtime LIKE ?1 OR address LIKE ?2) AND status = 'online'
                 ORDER BY last_seen_at DESC LIMIT 1",
                params![format!("%{rt_prefix}%"), format!("{normalized}:%")],
                |r| r.get(0),
            );
            if let Ok(Some(addr)) = resolved {
                return addr;
            }
            let fallback: Result<Option<String>, _> = conn.query_row(
                "SELECT address FROM agents 
                 WHERE (runtime LIKE ?1 OR address LIKE ?2)
                 ORDER BY last_seen_at DESC LIMIT 1",
                params![format!("%{rt_prefix}%"), format!("{normalized}:%")],
                |r| r.get(0),
            );
            if let Ok(Some(addr)) = fallback {
                return addr;
            }
        }

        target.to_string()
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
        let conn = self.conn.lock().unwrap();
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
        pair.sort();
        let dm_id = format!("dm:{}:{}", sanitize_slug(&pair[0]), sanitize_slug(&pair[1]));
        let now = Utc::now().to_rfc3339();

        if let Some(existing) = self.get_thread(&dm_id)? {
            return Ok(existing);
        }

        let title = format!("{} <-> {}", pair[0], pair[1]);
        {
            let conn = self.conn.lock().unwrap();
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
            let conn = self.conn.lock().unwrap();
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
        let conn = self.conn.lock().unwrap();
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
        let conn = self.conn.lock().unwrap();
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

    pub fn set_thread_muted(&self, thread_id: &str, muted: bool) -> Result<Option<Thread>> {
        {
            let conn = self.conn.lock().unwrap();
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
            let conn = self.conn.lock().unwrap();
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
        let conn = self.conn.lock().unwrap();
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
        let conn = self.conn.lock().unwrap();
        let rows = conn.execute(
            "DELETE FROM subscriptions WHERE thread_id = ?1 AND agent_address = ?2",
            params![thread_id, agent_address],
        )?;
        Ok(rows > 0)
    }

    pub fn list_subscriptions(&self, thread_id: Option<&str>) -> Result<Vec<Subscription>> {
        let conn = self.conn.lock().unwrap();
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
        let now = Utc::now().to_rfc3339();
        let id = Uuid::new_v4().to_string();

        // Merge explicit mentions with inline @address mentions parsed from body
        let mut mentions: Vec<String> = explicit_mentions.to_vec();
        for token in body.split_whitespace() {
            if let Some(rest) = token.strip_prefix('@') {
                let clean = rest
                    .trim_end_matches(|c: char| matches!(c, ',' | '.' | ':' | ';' | '!' | '?' | ')'))
                    .to_string();
                if !clean.is_empty() && !mentions.iter().any(|m| m == &clean) {
                    mentions.push(clean);
                }
            }
        }
        let mentions_json = serde_json::to_string(&mentions)?;

        let seq: i64 = {
            let conn = self.conn.lock().unwrap();
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

            // Increment hop_count unless sender is "human" (human resets loop counter)
            if sender == "human" {
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
        let conn = self.conn.lock().unwrap();
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

    pub fn ack_inbox(&self, agent_address: &str, thread_id: &str, up_to_seq: i64) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE subscriptions
             SET last_read_seq = MAX(last_read_seq, ?1), updated_at = ?2
             WHERE thread_id = ?3 AND agent_address = ?4",
            params![up_to_seq, &now, thread_id, agent_address],
        )?;
        Ok(())
    }

    pub fn check_inbox(&self, agent_address: &str) -> Result<Vec<InboxItem>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT t.id, t.title, t.kind,
                    COUNT(m.id) AS unread_count,
                    MAX(m.seq) AS latest_seq
             FROM subscriptions s
             JOIN threads t ON t.id = s.thread_id
             JOIN messages m ON m.thread_id = t.id
             WHERE s.agent_address = ?1
               AND m.seq > s.last_read_seq
               AND m.sender != ?1
               AND s.mode != 'muted'
             GROUP BY t.id, t.title, t.kind
             ORDER BY MAX(m.created_at) DESC",
        )?;

        let base_rows: Vec<(String, String, String, i64, i64)> = stmt
            .query_map(params![agent_address], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })?
            .collect::<std::result::Result<_, _>>()?;

        let mut items = Vec::new();
        for (tid, title, kind_s, unread_count, latest_seq) in base_rows {
            let (latest_sender, latest_body, latest_at): (String, String, String) = conn.query_row(
                "SELECT sender, body, created_at FROM messages
                 WHERE thread_id = ?1 AND seq = ?2",
                params![&tid, latest_seq],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?;
            let preview: String = latest_body.chars().take(120).collect();
            items.push(InboxItem {
                thread_id: tid,
                thread_title: title,
                thread_kind: ThreadKind::from_str(&kind_s),
                unread_count,
                latest_seq,
                latest_sender,
                latest_preview: preview,
                latest_at,
            });
        }
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
        let conn = self.conn.lock().unwrap();
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
        let conn = self.conn.lock().unwrap();
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

fn infer_transport(runtime: &RuntimeKind, endpoint: Option<&str>) -> TransportKind {
    match runtime {
        RuntimeKind::ClaudeCode | RuntimeKind::ClaudeDesktop => {
            if endpoint.is_some() {
                TransportKind::ClaudePipe
            } else {
                TransportKind::PollOnly
            }
        }
        RuntimeKind::Codex => {
            if endpoint.map(|e| e.starts_with("ws://")).unwrap_or(false) {
                TransportKind::CodexAppServer
            } else {
                TransportKind::CodexResume
            }
        }
        RuntimeKind::Antigravity => TransportKind::AgyResume,
        RuntimeKind::Unknown => TransportKind::PollOnly,
    }
}

fn sanitize_slug(input: &str) -> String {
    input
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-")
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
