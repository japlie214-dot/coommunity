use coommunity::db::Database;
use coommunity::models::{PostMessageRequest, RegisterAgentRequest, SendDmRequest};

#[test]
fn test_dm_request_model_consistency() {
    let raw_mcp_json = serde_json::json!({
        "to": "codex:tests",
        "body": "Run regression suite",
        "ack_only": true
    });

    let dm: SendDmRequest = serde_json::from_value(serde_json::json!({
        "from": "cc:lead",
        "to": raw_mcp_json["to"],
        "body": raw_mcp_json["body"],
        "ack_only": raw_mcp_json["ack_only"]
    })).expect("deserialization failed");

    assert_eq!(dm.from, "cc:lead");
    assert_eq!(dm.to, "codex:tests");
    assert_eq!(dm.body, "Run regression suite");
    assert_eq!(dm.ack_only, true);

    // Assert re-serialization is consistent
    let re_serialized = serde_json::to_value(&dm).unwrap();
    assert_eq!(re_serialized["to"], "codex:tests");
    assert_eq!(re_serialized["body"], "Run regression suite");
    assert_eq!(re_serialized["ack_only"], true);
}

#[test]
fn test_post_message_model_consistency() {
    let raw_post = serde_json::json!({
        "sender": "agy:architect",
        "body": "System spec updated @all",
        "mentions": ["all"],
        "ack_only": false
    });

    let post: PostMessageRequest = serde_json::from_value(raw_post).expect("deserialization failed");
    assert_eq!(post.sender, "agy:architect");
    assert_eq!(post.mentions, vec!["all".to_string()]);
    assert_eq!(post.ack_only, false);
}

#[test]
fn test_end_to_end_dm_storage_and_inbox_consistency() {
    let temp_dir = std::env::temp_dir().join(format!("coom_consistency_{}", uuid::Uuid::new_v4()));
    let db_path = temp_dir.join("test.db");
    let db = Database::open(&db_path).expect("failed to open database");

    // Register 2 agents: CC and Codex
    let cc_agent = RegisterAgentRequest {
        address: Some("cc:frontend".to_string()),
        runtime: "claude_code".to_string(),
        session_id: Some("cc-sess".to_string()),
        cwd: None,
        status: Some("online".to_string()),
        transport: None,
        endpoint: None,
        auth_token: None,
        allow_wake_resume: None,
    };
    let codex_agent = RegisterAgentRequest {
        address: Some("codex:backend".to_string()),
        runtime: "codex".to_string(),
        session_id: Some("codex-sess".to_string()),
        cwd: None,
        status: Some("online".to_string()),
        transport: None,
        endpoint: None,
        auth_token: None,
        allow_wake_resume: None,
    };

    db.upsert_agent(&cc_agent).unwrap();
    db.upsert_agent(&codex_agent).unwrap();

    // CC sends DM to Codex
    let thread = db.get_or_create_dm_thread("cc:frontend", "codex:backend", 8).unwrap();
    let (_msg, _) = db.post_message(&thread.id, "cc:frontend", "Ready for review", &[], false).unwrap();

    // Verify inbox item for Codex matches expected structure
    let inbox = db.check_inbox("codex:backend").unwrap();
    assert_eq!(inbox.len(), 1);
    assert_eq!(inbox[0].thread_id, thread.id);
    assert_eq!(inbox[0].unread_count, 1);
    assert_eq!(inbox[0].latest_sender, "cc:frontend");

    // Verify sender does NOT have unread inbox (echo suppression)
    let sender_inbox = db.check_inbox("cc:frontend").unwrap();
    assert_eq!(sender_inbox.len(), 0);

    let _ = std::fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_mcp_resource_catalog_definitions() {
    let resources = coommunity::mcp::resource_definitions();
    assert_eq!(resources.len(), 3);

    let uris: Vec<&str> = resources
        .iter()
        .filter_map(|r| r.get("uri").and_then(|u| u.as_str()))
        .collect();

    assert!(uris.contains(&"coom://inbox"));
    assert!(uris.contains(&"coom://status"));
    assert!(uris.contains(&"coom://threads"));

    for res in &resources {
        assert_eq!(res.get("mimeType").and_then(|m| m.as_str()), Some("application/json"));
        assert!(res.get("name").is_some());
        assert!(res.get("description").is_some());
    }
}

#[test]
fn test_wait_for_message_tool_definition() {
    let tools = coommunity::mcp::tool_definitions();
    let tools_arr = tools.as_array().expect("tools must be array");
    assert_eq!(tools_arr.len(), 14);

    let wait_tool = tools_arr
        .iter()
        .find(|t| t.get("name").and_then(|n| n.as_str()) == Some("wait_for_message"))
        .expect("wait_for_message tool must be defined");

    let schema = wait_tool.get("inputSchema").expect("inputSchema required");
    assert!(schema["properties"]["timeout_secs"].is_object());

    let ack_all_tool = tools_arr
        .iter()
        .find(|t| t.get("name").and_then(|n| n.as_str()) == Some("ack_all_inbox"))
        .expect("ack_all_inbox tool must be defined");
    assert!(ack_all_tool.get("description").is_some());
}
