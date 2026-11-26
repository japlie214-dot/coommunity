use axum::{routing::get, Router};
use coommunity::config::Config;
use coommunity::db::Database;
use coommunity::dispatcher::Dispatcher;
use coommunity::models::{HubEvent, RegisterAgentRequest, SubscriptionMode};
use coommunity::v2_daemon::{require_local_auth_v2, v2_router, V2State};
use coommunity::v2_db;
use coommunity::v2_models::{
    MessageRef, MessageStatus, MessageType, PostMessageV2Request, UpdateAgentProfileRequest,
    BOSS_ADDRESS,
};
use reqwest::Client;
use serde_json::Value;
use tokio::sync::broadcast;

#[test]
fn test_v2_migrations_boss_pagination_handoffs_and_archival() {
    let temp_dir = std::env::temp_dir().join(format!("coom_v2_db_{}", uuid::Uuid::new_v4()));
    let db_path = temp_dir.join("test.db");
    let db = Database::open(&db_path).expect("failed to open db with v2 migrations");
    let conn_arc = db.conn_arc();
    let conn = conn_arc.lock().unwrap();

    // 1. Boss permanent profile is seeded and protected
    let agents = v2_db::list_agents_v2(&conn, true).unwrap();
    let boss = agents.iter().find(|a| a.address == BOSS_ADDRESS).expect("Boss seeded");
    assert_eq!(boss.runtime, "human");
    assert!(!boss.ephemeral);
    assert!(!boss.archived);
    assert!(!v2_db::set_agent_archived(&conn, BOSS_ADDRESS, true).unwrap());
    assert_eq!(
        v2_db::resolve_agent_alias_v2(&conn, "@human").unwrap(),
        Some("Boss".to_string())
    );

    drop(conn);

    // 2. Register active, stale-ephemeral-unread0, stale-ephemeral-with-unread, and offline specialist
    for (addr, rt, status) in [
        ("cc:alice", "claude_code", "online"),
        ("codex:bob", "codex", "offline"),
        ("agy:stale-clean", "antigravity", "offline"),
        ("agy:stale-unread", "antigravity", "offline"),
    ] {
        db.upsert_agent(&RegisterAgentRequest {
            address: Some(addr.to_string()),
            runtime: rt.to_string(),
            session_id: Some(format!("sess-{addr}")),
            cwd: Some("C:\\Projects\\demo".to_string()),
            status: Some(status.to_string()),
            transport: None,
            endpoint: None,
            auth_token: None,
            allow_wake_resume: None,
        })
        .unwrap();
    }

    let forum = db
        .create_forum_thread(
            "V2 Architecture Sync",
            Some("Long running sync"),
            "cc:alice",
            &["cc:alice".to_string()],
            SubscriptionMode::All,
            8,
        )
        .unwrap();

    let conn = conn_arc.lock().unwrap();

    // 3. Profile updates & capability search
    v2_db::update_agent_profile(
        &conn,
        "codex:bob",
        &UpdateAgentProfileRequest {
            bio: Some("Rust backend & SQLite owner".to_string()),
            capabilities: Some(vec!["rust".to_string(), "sqlite".to_string()]),
            ephemeral: Some(false),
        },
    )
    .unwrap();
    let rust_agents = v2_db::find_agents_by_capability(&conn, Some("sqlite"), None).unwrap();
    assert_eq!(rust_agents.len(), 1);
    assert_eq!(rust_agents[0].address, "codex:bob");

    // 4. Typed handoff + reply_to_seq + refs + offline @mention auto-notify
    let post_resp = v2_db::post_message_v2(
        &conn,
        &forum.id,
        &PostMessageV2Request {
            sender: "cc:alice".to_string(),
            body: "Please review @codex:bob and @Boss".to_string(),
            mentions: vec![],
            ack_only: false,
            msg_type: MessageType::Handoff,
            status: None,
            reply_to_seq: None,
            refs: vec![MessageRef {
                path: "src/v2_db.rs".to_string(),
                line: Some(48),
                symbol: Some("apply_v2_migrations".to_string()),
                commit: None,
            }],
        },
    )
    .unwrap()
    .expect("post succeeds");

    assert_eq!(post_resp.message.seq, 1);
    assert_eq!(post_resp.message.status, Some(MessageStatus::Open));
    assert!(post_resp.auto_notified_agents.contains(&"codex:bob".to_string()));
    assert!(post_resp.auto_notified_agents.contains(&"Boss".to_string()));
    assert_eq!(post_resp.thread.open_action_count, 1);

    // Resolve status and set pinned thread brief
    assert!(v2_db::update_message_status(&conn, &forum.id, 1, &MessageStatus::Resolved).unwrap());
    let briefed = v2_db::set_thread_brief(&conn, &forum.id, "## Contract v2 locked", "Boss")
        .unwrap()
        .unwrap();
    assert_eq!(briefed.brief, "## Contract v2 locked");
    assert_eq!(briefed.open_action_count, 0);

    // 5. Latest-page-first pagination (post 4 more messages -> total 5; read limit=2 -> gets #4,#5)
    for i in 2..=5 {
        v2_db::post_message_v2(
            &conn,
            &forum.id,
            &PostMessageV2Request {
                sender: "cc:alice".to_string(),
                body: format!("Update #{i}"),
                mentions: vec![],
                ack_only: false,
                msg_type: MessageType::Chat,
                status: None,
                reply_to_seq: Some(1),
                refs: vec![],
            },
        )
        .unwrap()
        .unwrap();
    }

    let latest_page =
        v2_db::read_messages_paginated(&conn, &forum.id, Some("cc:alice"), false, None, None, Some(2))
            .unwrap()
            .unwrap();
    assert_eq!(latest_page.total_count, 5);
    assert_eq!(latest_page.messages.len(), 2);
    assert_eq!(latest_page.messages[0].seq, 4);
    assert_eq!(latest_page.messages[1].seq, 5);
    assert!(latest_page.has_more_before);
    assert!(!latest_page.has_more_after);

    let older_page = v2_db::read_messages_paginated(
        &conn,
        &forum.id,
        Some("cc:alice"),
        false,
        latest_page.oldest_seq,
        None,
        Some(2),
    )
    .unwrap()
    .unwrap();
    assert_eq!(older_page.messages[0].seq, 2);
    assert_eq!(older_page.messages[1].seq, 3);
    assert!(older_page.has_more_before);
    assert!(older_page.has_more_after);

    // 6. Boss resets hop_count to 0
    let boss_post = v2_db::post_message_v2(
        &conn,
        &forum.id,
        &PostMessageV2Request {
            sender: "Boss".to_string(),
            body: "Approved".to_string(),
            mentions: vec![],
            ack_only: false,
            msg_type: MessageType::Decision,
            status: None,
            reply_to_seq: Some(5),
            refs: vec![],
        },
    )
    .unwrap()
    .unwrap();
    assert_eq!(boss_post.thread.hop_count, 0);

    // 7. 14-day ephemeral auto-archive: stale-clean archived, stale-unread spared, Boss spared
    v2_db::update_agent_profile(
        &conn,
        "agy:stale-clean",
        &UpdateAgentProfileRequest {
            bio: None,
            capabilities: None,
            ephemeral: Some(true),
        },
    )
    .unwrap();
    v2_db::update_agent_profile(
        &conn,
        "agy:stale-unread",
        &UpdateAgentProfileRequest {
            bio: None,
            capabilities: None,
            ephemeral: Some(true),
        },
    )
    .unwrap();
    conn.execute(
        "UPDATE agents SET last_seen_at = '2020-01-01T00:00:00Z' WHERE address IN ('agy:stale-clean', 'agy:stale-unread')",
        [],
    )
    .unwrap();

    // Give agy:stale-unread an unread DM
    drop(conn);
    let dm_stale = db.get_or_create_dm_thread("cc:alice", "agy:stale-unread", 8).unwrap();
    db.post_message(&dm_stale.id, "cc:alice", "Don't archive me!", &[], false)
        .unwrap();
    db.log_delivery("msg-delivery-1", &dm_stale.id, "agy:stale-unread", "tier3", "queued", None)
        .unwrap();

    let conn = conn_arc.lock().unwrap();
    let report = v2_db::archive_stale_ephemeral_agents(&conn, Some(14)).unwrap();
    assert_eq!(report.archived_count, 1);
    assert_eq!(report.archived_agents, vec!["agy:stale-clean".to_string()]);

    // 8. v2.1 fixes: Boss excluded from online_agents count, deliveries null-safe, timestamp ms & filter/sort
    let ov = v2_db::admin_overview(&conn).unwrap();
    assert_eq!(ov.online_agents, 1, "only cc:alice is online; Boss must be excluded");

    let deliveries = v2_db::list_recent_deliveries_v2(&conn, 10).unwrap();
    assert_eq!(deliveries.len(), 1);
    assert!(deliveries[0].id > 0);
    assert_eq!(deliveries[0].detail, None);

    assert!(v2_db::parse_timestamp_ms("2026-11-25 14:00:00") > v2_db::parse_timestamp_ms("2026-11-25T13:00:00Z"));
    let sorted_msgs = v2_db::list_threads_paginated(
        &conn,
        None,
        true,
        None,
        None,
        Some("messages"),
        None,
        None,
        Some(20),
    )
    .unwrap();
    assert!(sorted_msgs.threads.len() >= 2);
    assert!(sorted_msgs.threads[0].message_count >= sorted_msgs.threads[1].message_count);

    let _ = std::fs::remove_dir_all(&temp_dir);
}

#[tokio::test]
async fn test_v2_auth_hardening_and_admin_vs_agent_dm_isolation() {
    let temp_dir = std::env::temp_dir().join(format!("coom_v2_http_{}", uuid::Uuid::new_v4()));
    let db_path = temp_dir.join("test.db");
    let db = Database::open(&db_path).unwrap();

    let mut cfg = Config::load();
    cfg.data_dir = temp_dir.clone();
    cfg.db_path = db_path;
    cfg.token_path = temp_dir.join("token");
    let token = "test-v2-secret-token".to_string();

    let (events, _) = broadcast::channel::<HubEvent>(64);
    let dispatcher = Dispatcher::new(db.clone(), cfg.clone(), events.clone());
    let v2_state = V2State {
        db: db.clone(),
        conn: db.conn_arc(),
        dispatcher,
        events,
        api_token: std::sync::Arc::new(std::sync::RwLock::new(token.clone())),
        token_path: Some(cfg.token_path.clone()),
        default_max_hops: 8,
    };

    // Seed private DM between cc:alice and codex:bob
    let dm = db.get_or_create_dm_thread("cc:alice", "codex:bob", 8).unwrap();
    db.post_message(&dm.id, "cc:alice", "Top secret v2 payload", &[], false)
        .unwrap();

    let app = Router::new()
        .route("/api/health", get(|| async { "ok" }))
        .route("/api/events", get(|| async { "sse" }))
        .route("/api/threads", get(|| async { "[]" }))
        .merge(v2_router(v2_state.clone()))
        .layer(axum::middleware::from_fn_with_state(
            v2_state,
            require_local_auth_v2,
        ));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let base = format!("http://{addr}");
    let client = Client::new();

    // Negative Discriminator 1: public health allows anonymous access, SSE & admin & reader require token (401)
    assert_eq!(
        client.get(format!("{base}/api/health")).send().await.unwrap().status(),
        200
    );
    assert_eq!(
        client.get(format!("{base}/api/events")).send().await.unwrap().status(),
        401
    );
    assert_eq!(
        client
            .get(format!("{base}/api/threads?reader=cc:alice"))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    assert_eq!(
        client
            .get(format!("{base}/api/admin/overview"))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );

    // EventSource query ?token= succeeds
    assert_eq!(
        client
            .get(format!("{base}/api/events?token={token}"))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );

    // Negative Discriminator 2: Even with valid bearer token, third-party agent reading private DM gets 403
    let eavesdrop = client
        .get(format!("{base}/api/v2/threads/{}/messages?reader=agy:mallory", dm.id))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(eavesdrop.status(), 403);

    // Participant reading DM succeeds (200)
    let bob_read = client
        .get(format!("{base}/api/v2/threads/{}/messages?reader=codex:bob", dm.id))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(bob_read.status(), 200);
    let bob_json: Value = bob_read.json().await.unwrap();
    assert_eq!(bob_json["messages"][0]["body"], "Top secret v2 payload");

    // Boss Admin endpoint sees ALL DMs & overview (200)
    let admin_read = client
        .get(format!("{base}/api/admin/threads/{}/messages", dm.id))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(admin_read.status(), 200);

    // Boss DM endpoint posts cleanly to canonical thread
    let boss_dm = client
        .post(format!("{base}/api/admin/dm"))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "to": "cc:alice",
            "body": "Boss steering note",
            "msg_type": "request"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(boss_dm.status(), 200);
    let boss_dm_json: Value = boss_dm.json().await.unwrap();
    assert_eq!(boss_dm_json["thread"]["id"], "dm:boss:cc-alice");

    // Boss filter=boss_dm returns dm:boss:cc-alice and excludes agent-only DM
    let boss_filtered: Value = client
        .get(format!("{base}/api/admin/threads?filter=boss_dm"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(boss_filtered["total_count"], 1);
    assert_eq!(boss_filtered["threads"][0]["id"], "dm:boss:cc-alice");

    // Initial deliveries hydration endpoint works
    db.log_delivery("msg-http-1", &dm.id, "codex:bob", "tier3", "queued_unread", None)
        .unwrap();
    let del_res = client
        .get(format!("{base}/api/admin/deliveries?limit=50"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(del_res.status(), 200);
    let del_json: Value = del_res.json().await.unwrap();
    let dels = del_json["deliveries"].as_array().unwrap();
    assert!(
        dels.iter().any(|d| d["message_id"] == "msg-http-1"),
        "expected msg-http-1 in hydrated deliveries: {dels:?}"
    );

    // Negative Discriminator 3: token rotation rejects short tokens (<16 chars) with 400
    let short_rot = client
        .post(format!("{base}/api/admin/token"))
        .bearer_auth(&token)
        .json(&serde_json::json!({ "new_token": "too-short" }))
        .send()
        .await
        .unwrap();
    assert_eq!(short_rot.status(), 400);

    // Valid live token rotation updates memory + disk, invalidates old token (401), accepts new (200)
    let new_token = "rotated-v21-token-0123456789abcdef";
    let ok_rot = client
        .post(format!("{base}/api/admin/token"))
        .bearer_auth(&token)
        .json(&serde_json::json!({ "new_token": new_token }))
        .send()
        .await
        .unwrap();
    assert_eq!(ok_rot.status(), 200);
    let rot_json: Value = ok_rot.json().await.unwrap();
    assert_eq!(rot_json["token"], new_token);
    assert_eq!(rot_json["persisted_to_disk"], true);
    assert_eq!(
        std::fs::read_to_string(&cfg.token_path).unwrap().trim(),
        new_token
    );
    assert_eq!(
        client
            .get(format!("{base}/api/admin/overview"))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    assert_eq!(
        client
            .get(format!("{base}/api/admin/overview"))
            .bearer_auth(new_token)
            .send()
            .await
            .unwrap()
            .status(),
        200
    );

    let _ = std::fs::remove_dir_all(&temp_dir);
}
