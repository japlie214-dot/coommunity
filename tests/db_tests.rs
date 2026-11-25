use coommunity::db::Database;
use coommunity::models::{AgentStatus, RegisterAgentRequest, SubscriptionMode};

#[test]
fn test_database_initialization_and_meta() {
    let temp_dir = std::env::temp_dir().join(format!("coom_test_{}", uuid::Uuid::new_v4()));
    let db_path = temp_dir.join("test.db");

    let db = Database::open(&db_path).expect("failed to open database");

    // Default pause state should be false
    assert_eq!(db.is_paused().unwrap(), false);

    // Toggle pause state
    db.set_paused(true).unwrap();
    assert_eq!(db.is_paused().unwrap(), true);

    db.set_paused(false).unwrap();
    assert_eq!(db.is_paused().unwrap(), false);

    let _ = std::fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_agent_registration_and_status() {
    let temp_dir = std::env::temp_dir().join(format!("coom_test_{}", uuid::Uuid::new_v4()));
    let db_path = temp_dir.join("test.db");
    let db = Database::open(&db_path).expect("failed to open database");

    // Upsert agent
    let req = RegisterAgentRequest {
        address: Some("cc:backend".to_string()),
        runtime: "claude_code".to_string(),
        session_id: Some("session-123".to_string()),
        cwd: Some("C:\\project".to_string()),
        status: Some("online".to_string()),
        transport: None,
        endpoint: Some(r"\\.\pipe\test-pipe".to_string()),
        auth_token: Some("secret123".to_string()),
        allow_wake_resume: Some(false),
    };

    let agent = db.upsert_agent(&req).unwrap();
    assert_eq!(agent.address, "cc:backend");
    assert_eq!(agent.status, AgentStatus::Online);

    // Update status
    let updated = db.set_agent_status("cc:backend", AgentStatus::Busy).unwrap();
    assert!(updated.is_some());
    assert_eq!(updated.unwrap().status, AgentStatus::Busy);

    // List agents (includes seeded permanent Boss profile + newly registered cc:backend)
    let agents = db.list_agents().unwrap();
    assert_eq!(agents.len(), 2);
    assert!(agents.iter().any(|a| a.address == "Boss"));
    assert!(agents.iter().any(|a| a.address == "cc:backend"));

    let _ = std::fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_forum_threads_messages_and_inbox() {
    let temp_dir = std::env::temp_dir().join(format!("coom_test_{}", uuid::Uuid::new_v4()));
    let db_path = temp_dir.join("test.db");
    let db = Database::open(&db_path).expect("failed to open database");

    // Create forum thread
    let members = vec!["cc:lead".to_string(), "codex:worker".to_string()];
    let thread = db.create_forum_thread(
        "API Architecture",
        Some("Discuss v2 API design"),
        "cc:lead",
        &members,
        SubscriptionMode::All,
        8,
    ).unwrap();

    assert_eq!(thread.title, "API Architecture");
    assert_eq!(thread.hop_count, 0);

    // Post message
    let (msg, thread_after) = db.post_message(
        &thread.id,
        "cc:lead",
        "Hello @codex:worker please inspect schema",
        &[],
        false,
    ).unwrap();

    assert_eq!(msg.seq, 1);
    assert_eq!(thread_after.hop_count, 1);

    // Check inbox for codex:worker
    let inbox = db.check_inbox("codex:worker").unwrap();
    assert_eq!(inbox.len(), 1);
    assert_eq!(inbox[0].unread_count, 1);

    // Ack inbox
    db.ack_inbox("codex:worker", &thread.id, 1).unwrap();
    let inbox_after = db.check_inbox("codex:worker").unwrap();
    assert_eq!(inbox_after.len(), 0);

    let _ = std::fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_dm_privacy_and_authorization_guards() {
    let temp_dir = std::env::temp_dir().join(format!("coom_test_{}", uuid::Uuid::new_v4()));
    let db_path = temp_dir.join("test.db");
    let db = Database::open(&db_path).expect("failed to open database");

    // Create a private DM thread between alice and bob
    let dm_thread = db.get_or_create_dm_thread("cc:alice", "cc:bob", 8).unwrap();
    assert_eq!(dm_thread.kind, coommunity::models::ThreadKind::Dm);

    // Verify participant recognition
    assert!(db.is_dm_participant(&dm_thread.id, "cc:alice").unwrap());
    assert!(db.is_dm_participant(&dm_thread.id, "cc:bob").unwrap());
    assert!(db.is_dm_participant(&dm_thread.id, "@cc:alice").unwrap());
    assert!(!db.is_dm_participant(&dm_thread.id, "agy:mallory").unwrap());
    assert!(!db.is_dm_participant(&dm_thread.id, "codex:eavesdropper").unwrap());

    // Verify can_access_thread
    assert!(db.can_access_thread(&dm_thread.id, "cc:alice").unwrap());
    assert!(db.can_access_thread(&dm_thread.id, "cc:bob").unwrap());
    assert!(!db.can_access_thread(&dm_thread.id, "agy:mallory").unwrap());

    // Participant posts a message to DM
    let (msg1, _) = db.post_message(&dm_thread.id, "cc:alice", "Secret message for Bob", &[], false).unwrap();
    assert_eq!(msg1.seq, 1);

    // Non-participant attempting to post to DM must be rejected
    let post_err = db.post_message(&dm_thread.id, "agy:mallory", "Injected spam", &[], false);
    assert!(post_err.is_err());
    let err_str = post_err.unwrap_err().to_string();
    assert!(err_str.contains("Access denied"), "Expected access denied, got: {err_str}");

    // Participant reading DM messages
    let msgs_bob = db.read_messages_for_agent(&dm_thread.id, 0, 10, Some("cc:bob"), true).unwrap();
    assert_eq!(msgs_bob.len(), 1);
    assert_eq!(msgs_bob[0].body, "Secret message for Bob");

    // Non-participant attempting to read DM messages must be rejected
    let read_err = db.read_messages_for_agent(&dm_thread.id, 0, 10, Some("agy:mallory"), true);
    assert!(read_err.is_err());
    let err_str = read_err.unwrap_err().to_string();
    assert!(err_str.contains("Access denied"), "Expected access denied, got: {err_str}");

    // Anonymous caller (no reader specified) reading DM messages must be rejected
    let anon_err = db.read_messages_for_agent(&dm_thread.id, 0, 10, None, true);
    assert!(anon_err.is_err());
    let err_str = anon_err.unwrap_err().to_string();
    assert!(err_str.contains("Access denied"), "Expected access denied, got: {err_str}");

    let _ = std::fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_dm_thread_listing_segregation() {
    let temp_dir = std::env::temp_dir().join(format!("coom_test_{}", uuid::Uuid::new_v4()));
    let db_path = temp_dir.join("test.db");
    let db = Database::open(&db_path).expect("failed to open database");

    // Create 1 public forum thread and 2 distinct DMs
    let forum = db.create_forum_thread(
        "Open Forum",
        Some("All agents welcome"),
        "cc:alice",
        &["cc:alice".to_string(), "cc:bob".to_string()],
        SubscriptionMode::All,
        8,
    ).unwrap();

    let dm1 = db.get_or_create_dm_thread("cc:alice", "cc:bob", 8).unwrap();
    let dm2 = db.get_or_create_dm_thread("codex:carol", "agy:david", 8).unwrap();

    // Alice should see forum + dm1, but NOT dm2
    let alice_threads = db.list_threads_for_agent(Some("cc:alice")).unwrap();
    let alice_ids: Vec<String> = alice_threads.into_iter().map(|t| t.id).collect();
    assert!(alice_ids.contains(&forum.id));
    assert!(alice_ids.contains(&dm1.id));
    assert!(!alice_ids.contains(&dm2.id), "Alice should not see dm2!");

    // David should see forum + dm2, but NOT dm1
    let david_threads = db.list_threads_for_agent(Some("agy:david")).unwrap();
    let david_ids: Vec<String> = david_threads.into_iter().map(|t| t.id).collect();
    assert!(david_ids.contains(&forum.id));
    assert!(david_ids.contains(&dm2.id));
    assert!(!david_ids.contains(&dm1.id), "David should not see dm1!");

    // Anonymous caller (None) should ONLY see the open forum thread
    let anon_threads = db.list_threads_for_agent(None).unwrap();
    let anon_ids: Vec<String> = anon_threads.into_iter().map(|t| t.id).collect();
    assert!(anon_ids.contains(&forum.id));
    assert!(!anon_ids.contains(&dm1.id), "Anonymous should not see dm1!");
    assert!(!anon_ids.contains(&dm2.id), "Anonymous should not see dm2!");

    // Forum threads are accessible to anyone
    assert!(db.can_access_thread(&forum.id, "anyone:random").unwrap());
    let forum_msgs = db.read_messages_for_agent(&forum.id, 0, 10, Some("anyone:random"), false).unwrap();
    assert_eq!(forum_msgs.len(), 0);

    let _ = std::fs::remove_dir_all(&temp_dir);
}
