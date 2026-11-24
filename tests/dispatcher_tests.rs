use coommunity::db::Database;
use coommunity::dispatcher::should_notify;
use coommunity::models::{Message, Subscription, SubscriptionMode, Thread, ThreadKind};

#[test]
fn test_mention_parsing_in_post() {
    let temp_dir = std::env::temp_dir().join(format!("coom_disp_{}", uuid::Uuid::new_v4()));
    let db_path = temp_dir.join("test.db");
    let db = Database::open(&db_path).expect("failed to open database");

    let members = vec!["cc:lead".to_string(), "agy:qa".to_string()];
    let thread = db.create_forum_thread(
        "Release Sync",
        None,
        "cc:lead",
        &members,
        SubscriptionMode::Mentions,
        8,
    ).unwrap();

    let (msg, _) = db.post_message(
        &thread.id,
        "cc:lead",
        "Paging @agy:qa and @codex:backend! Please check.",
        &["manual:mention".to_string()],
        false,
    ).unwrap();

    assert!(msg.mentions.contains(&"agy:qa".to_string()));
    assert!(msg.mentions.contains(&"codex:backend".to_string()));
    assert!(msg.mentions.contains(&"manual:mention".to_string()));

    let _ = std::fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_hop_count_circuit_breaker() {
    let temp_dir = std::env::temp_dir().join(format!("coom_disp_{}", uuid::Uuid::new_v4()));
    let db_path = temp_dir.join("test.db");
    let db = Database::open(&db_path).expect("failed to open database");

    let members = vec!["agent:a".to_string(), "agent:b".to_string()];
    let thread = db.create_forum_thread(
        "Ping Pong",
        None,
        "agent:a",
        &members,
        SubscriptionMode::All,
        3, // max_hops = 3
    ).unwrap();

    // 3 messages
    db.post_message(&thread.id, "agent:a", "msg 1", &[], false).unwrap();
    db.post_message(&thread.id, "agent:b", "msg 2", &[], false).unwrap();
    let (_, t3) = db.post_message(&thread.id, "agent:a", "msg 3", &[], false).unwrap();

    assert_eq!(t3.hop_count, 3);
    assert_eq!(t3.max_hops, 3);

    // 4th message exceeds max_hops
    let (_, t4) = db.post_message(&thread.id, "agent:b", "msg 4", &[], false).unwrap();
    assert_eq!(t4.hop_count, 4);
    assert!(t4.hop_count > t4.max_hops);

    let _ = std::fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_dm_wake_always_and_forum_mentions_filtering() {
    let now = "2026-11-23T07:00:00Z".to_string();

    let dm_thread = Thread {
        id: "dm:agent-a:agent-b".to_string(),
        kind: ThreadKind::Dm,
        title: "agent:a <-> agent:b".to_string(),
        description: None,
        created_by: "agent:a".to_string(),
        created_at: now.clone(),
        updated_at: now.clone(),
        message_count: 1,
        hop_count: 1,
        max_hops: 10,
        muted: false,
    };

    let forum_thread = Thread {
        id: "forum:announcements".to_string(),
        kind: ThreadKind::Forum,
        title: "Announcements".to_string(),
        description: None,
        created_by: "agent:a".to_string(),
        created_at: now.clone(),
        updated_at: now.clone(),
        message_count: 1,
        hop_count: 1,
        max_hops: 10,
        muted: false,
    };

    let sub_mentions = Subscription {
        thread_id: forum_thread.id.clone(),
        agent_address: "agent:b".to_string(),
        mode: SubscriptionMode::Mentions,
        last_read_seq: 0,
        updated_at: now.clone(),
    };

    let sub_dm = Subscription {
        thread_id: dm_thread.id.clone(),
        agent_address: "agent:b".to_string(),
        mode: SubscriptionMode::Mentions,
        last_read_seq: 0,
        updated_at: now.clone(),
    };

    // 1. DM without any mentions -> MUST ALWAYS WAKE
    let dm_unmentioned_msg = Message {
        id: "msg-1".to_string(),
        thread_id: dm_thread.id.clone(),
        seq: 1,
        sender: "agent:a".to_string(),
        body: "Hey there direct message".to_string(),
        mentions: vec![],
        ack_only: false,
        created_at: now.clone(),
    };
    assert!(
        should_notify(&dm_thread, &sub_dm, &dm_unmentioned_msg),
        "DMs must ALWAYS wake the recipient even with zero @mentions"
    );

    // 2. Forum message without mentions -> MUST NOT WAKE
    let forum_unmentioned_msg = Message {
        id: "msg-2".to_string(),
        thread_id: forum_thread.id.clone(),
        seq: 1,
        sender: "agent:a".to_string(),
        body: "General forum announcement without tags".to_string(),
        mentions: vec![],
        ack_only: false,
        created_at: now.clone(),
    };
    assert!(
        !should_notify(&forum_thread, &sub_mentions, &forum_unmentioned_msg),
        "Forum messages without mentions MUST NOT wake recipients in Mentions mode"
    );

    // 3. Forum message with explicit @agent:b -> MUST WAKE
    let forum_tagged_msg = Message {
        id: "msg-3".to_string(),
        thread_id: forum_thread.id.clone(),
        seq: 2,
        sender: "agent:a".to_string(),
        body: "Hey @agent:b check this out".to_string(),
        mentions: vec!["agent:b".to_string()],
        ack_only: false,
        created_at: now.clone(),
    };
    assert!(
        should_notify(&forum_thread, &sub_mentions, &forum_tagged_msg),
        "Forum message with explicit @tag MUST wake the recipient"
    );

    // 4. Forum message with @all -> MUST WAKE
    let forum_all_msg = Message {
        id: "msg-4".to_string(),
        thread_id: forum_thread.id.clone(),
        seq: 3,
        sender: "agent:a".to_string(),
        body: "Hey @all important alert".to_string(),
        mentions: vec!["all".to_string()],
        ack_only: false,
        created_at: now.clone(),
    };
    assert!(
        should_notify(&forum_thread, &sub_mentions, &forum_all_msg),
        "Forum message with @all MUST wake the recipient"
    );
}

#[test]
fn test_resolve_recipient_alias() {
    let temp_dir = std::env::temp_dir().join(format!("coom_alias_{}", uuid::Uuid::new_v4()));
    let db_path = temp_dir.join("test.db");
    let db = Database::open(&db_path).expect("failed to open database");

    let cc_agent = coommunity::models::RegisterAgentRequest {
        address: Some("cc:memberberries".to_string()),
        runtime: "claude_code".to_string(),
        session_id: Some("sess-1".to_string()),
        cwd: None,
        status: Some("online".to_string()),
        transport: None,
        endpoint: None,
        auth_token: None,
        allow_wake_resume: None,
    };
    db.upsert_agent(&cc_agent).unwrap();

    assert_eq!(db.resolve_recipient_alias("cc"), "cc:memberberries");
    assert_eq!(db.resolve_recipient_alias("claude"), "cc:memberberries");
    assert_eq!(db.resolve_recipient_alias("other:agent"), "other:agent");

    let _ = std::fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_multi_session_presence_retention() {
    let temp_dir = std::env::temp_dir().join(format!("coom_multi_{}", uuid::Uuid::new_v4()));
    let db_path = temp_dir.join("test.db");
    let db = Database::open(&db_path).expect("failed to open database");

    // Session 1 registers
    let sess1 = coommunity::models::RegisterAgentRequest {
        address: Some("cc:memberberries".to_string()),
        runtime: "claude_code".to_string(),
        session_id: Some("sess-tab-1".to_string()),
        cwd: None,
        status: Some("online".to_string()),
        transport: Some("claude_pipe".to_string()),
        endpoint: Some(r"\\.\pipe\pipe-1".to_string()),
        auth_token: None,
        allow_wake_resume: None,
    };
    db.upsert_agent(&sess1).unwrap();

    // Session 2 registers under the same workspace address
    let sess2 = coommunity::models::RegisterAgentRequest {
        address: Some("cc:memberberries".to_string()),
        runtime: "claude_code".to_string(),
        session_id: Some("sess-tab-2".to_string()),
        cwd: None,
        status: Some("online".to_string()),
        transport: Some("claude_pipe".to_string()),
        endpoint: Some(r"\\.\pipe\pipe-2".to_string()),
        auth_token: None,
        allow_wake_resume: None,
    };
    db.upsert_agent(&sess2).unwrap();

    // Both sessions should be active
    let active = db.get_active_sessions("cc:memberberries").unwrap();
    assert_eq!(active.len(), 2);

    // Session 2 disconnects
    db.set_agent_session_status("cc:memberberries", Some("sess-tab-2"), coommunity::models::AgentStatus::Offline).unwrap();

    // Canonical agent MUST still be ONLINE because Session 1 is alive!
    let agent = db.get_agent("cc:memberberries").unwrap().unwrap();
    assert_eq!(agent.status, coommunity::models::AgentStatus::Online);

    let active_remaining = db.get_active_sessions("cc:memberberries").unwrap();
    assert_eq!(active_remaining.len(), 1);
    assert_eq!(active_remaining[0].0, Some(r"\\.\pipe\pipe-1".to_string()));

    // When Session 1 also disconnects, canonical agent goes offline
    db.set_agent_session_status("cc:memberberries", Some("sess-tab-1"), coommunity::models::AgentStatus::Offline).unwrap();
    let agent_off = db.get_agent("cc:memberberries").unwrap().unwrap();
    assert_eq!(agent_off.status, coommunity::models::AgentStatus::Offline);

    let _ = std::fs::remove_dir_all(&temp_dir);
}
