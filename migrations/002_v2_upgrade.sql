-- coommunity v2.0 Idempotent SQLite Migration
-- Safe to run on startup against existing v1.4.x databases.
-- Note: SQLite does not support `ALTER TABLE ... ADD COLUMN IF NOT EXISTS`,
-- so `v2_db::apply_v2_migrations` checks `PRAGMA table_info(...)` before running
-- each ALTER statement. This file documents the exact schema delta.

-- 1. Agents: Capability Directory + 14-day Ephemeral Auto-Archive
ALTER TABLE agents ADD COLUMN bio TEXT NOT NULL DEFAULT '';
ALTER TABLE agents ADD COLUMN capabilities_json TEXT NOT NULL DEFAULT '[]';
ALTER TABLE agents ADD COLUMN ephemeral INTEGER NOT NULL DEFAULT 0;
ALTER TABLE agents ADD COLUMN archived INTEGER NOT NULL DEFAULT 0;
ALTER TABLE agents ADD COLUMN archived_at TEXT;

-- 2. Threads: Pinned Brief / Blackboard
ALTER TABLE threads ADD COLUMN brief TEXT NOT NULL DEFAULT '';
ALTER TABLE threads ADD COLUMN brief_updated_by TEXT;
ALTER TABLE threads ADD COLUMN brief_updated_at TEXT;

-- 3. Messages: Typed Handoff Envelopes + Reply Threading + Structured Refs
ALTER TABLE messages ADD COLUMN msg_type TEXT NOT NULL DEFAULT 'chat';
ALTER TABLE messages ADD COLUMN status TEXT;
ALTER TABLE messages ADD COLUMN reply_to_seq INTEGER;
ALTER TABLE messages ADD COLUMN refs_json TEXT NOT NULL DEFAULT '[]';

-- 4. Subscriptions: Track Auto-Subscribed via @mention vs Explicit Subscribe
ALTER TABLE subscriptions ADD COLUMN auto_subscribed INTEGER NOT NULL DEFAULT 0;

-- 5. Scaling & Pagination Indexes (thousands of DMs/forums, hundreds of agents)
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

-- 6. Permanent Human Profile ("Boss") - Can Receive DMs & @Boss Mentions
INSERT OR IGNORE INTO agents (
    address, runtime, session_id, cwd, status, transport, endpoint,
    auth_token, allow_wake_resume, last_seen_at, registered_at,
    bio, capabilities_json, ephemeral, archived
) VALUES (
    'Boss',
    'human',
    'human-admin',
    'Human Command Console',
    'online',
    'web_admin',
    'http://127.0.0.1:7749',
    NULL,
    0,
    strftime('%Y-%m-%dT%H:%M:%SZ', 'now'),
    strftime('%Y-%m-%dT%H:%M:%SZ', 'now'),
    'Human operator and hub administrator. DM or @Boss for approvals, steering, or escalations.',
    '["admin","human","approvals","architecture"]',
    0,
    0
);
