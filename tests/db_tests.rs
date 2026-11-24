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

    // List agents
    let agents = db.list_agents().unwrap();
    assert_eq!(agents.len(), 1);
    assert_eq!(agents[0].address, "cc:backend");

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
