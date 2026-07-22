//! Conformance tests for the in-memory `StorageAdapter`.
//!
//! Exercises every slice: conversation CRUD, participants (user + ai-agent),
//! message append + paging, session create/update, and a checkpoint
//! round-trip through the `CheckpointStore` accessor (proving the
//! smooth-operator-core engine plugs into the adapter seam).

use chrono::Utc;

use smooth_operator::adapter::{MessageQuery, SessionUpdate, StorageAdapter};
use smooth_operator::domain::{
    Conversation, Direction, Message, MessageContent, Participant, ParticipantType, Platform,
    Session, SessionStatus,
};
use smooth_operator_adapter_memory::InMemoryStorageAdapter;
use smooth_operator_core::{Checkpoint, Conversation as EngineConversation};

fn conversation(id: &str, org: &str) -> Conversation {
    Conversation {
        id: id.into(),
        platform: Platform::Web,
        name: "Lead chat".into(),
        organization_id: org.into(),
        idempotency_key: format!("idem-{id}"),
        metadata_json: None,
        analytics_json: None,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    }
}

fn participant(id: &str, conv: &str, org: &str, pt: ParticipantType) -> Participant {
    Participant {
        id: id.into(),
        conversation_id: conv.into(),
        organization_id: org.into(),
        participant_type: pt,
        external_id: None,
        internal_id: None,
        browser_fingerprint: None,
        browser_info: None,
        name: id.into(),
        email: None,
        phone: None,
        crm_contact_id: None,
        metadata_json: None,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    }
}

fn message(id: &str, conv: &str, dir: Direction, text: &str) -> Message {
    Message {
        id: id.into(),
        external_id: None,
        organization_id: Some("org-1".into()),
        conversation_id: Some(conv.into()),
        direction: dir,
        content: MessageContent::from_text(text),
        from: None,
        to: None,
        metadata_json: None,
        analytics_json: None,
        created_at: Utc::now(),
        updated_at: None,
    }
}

#[tokio::test]
async fn full_lifecycle_through_the_adapter() {
    let store = InMemoryStorageAdapter::new();

    // --- conversation create/get/list/update ---
    let conv = store
        .create_conversation(conversation("conv-1", "org-1"))
        .await
        .expect("create conversation");
    assert_eq!(conv.id, "conv-1");

    // idempotency: same org + idempotencyKey returns the existing row
    let dup = store
        .create_conversation(conversation("conv-1", "org-1"))
        .await
        .expect("idempotent create");
    assert_eq!(dup.id, "conv-1");

    let fetched = store
        .get_conversation("conv-1")
        .await
        .expect("get")
        .expect("exists");
    assert_eq!(fetched.organization_id, "org-1");

    let by_org = store
        .list_conversations_by_org("org-1")
        .await
        .expect("list by org");
    assert_eq!(by_org.len(), 1);

    let updated = store
        .update_conversation(
            "conv-1",
            smooth_operator::adapter::ConversationUpdate {
                name: Some("Renamed".into()),
                ..Default::default()
            },
        )
        .await
        .expect("update");
    assert_eq!(updated.name, "Renamed");

    // --- two participants: a user and an ai-agent ---
    let mut user = participant("part-user", "conv-1", "org-1", ParticipantType::User);
    user.external_id = Some("supabase-user-123".into());
    user.email = Some("Owner@Example.com".into());
    store.add_participant(user).await.expect("add user");
    store
        .add_participant(participant(
            "part-agent",
            "conv-1",
            "org-1",
            ParticipantType::AiAgent,
        ))
        .await
        .expect("add ai-agent");

    let participants = store
        .list_participants_by_conversation("conv-1")
        .await
        .expect("list participants");
    assert_eq!(participants.len(), 2);

    // --- per-user conversation scope (SECURITY, pearl th-b2c60b) ---
    // Org scoping alone lets any member of an org enumerate every other
    // member's conversations, so every adapter must ALSO scope by the owning
    // user participant's email.
    let owned = store
        .list_conversations_by_org_and_user("org-1", "owner@example.com")
        .await
        .expect("scoped list");
    assert_eq!(owned.len(), 1, "the owner sees their own conversation");
    assert_eq!(owned[0].id, "conv-1", "email compare is case-insensitive");
    assert!(
        store
            .list_conversations_by_org_and_user("org-1", "someone-else@example.com")
            .await
            .expect("scoped list")
            .is_empty(),
        "another user in the SAME org must see nothing"
    );
    assert!(
        store
            .list_conversations_by_org_and_user("org-1", "")
            .await
            .expect("scoped list")
            .is_empty(),
        "an emailless caller owns nothing — fail closed"
    );
    assert!(participants
        .iter()
        .any(|p| p.participant_type == ParticipantType::User));
    assert!(participants
        .iter()
        .any(|p| p.participant_type == ParticipantType::AiAgent));

    let resolved = store
        .resolve_participant_by_external_id("conv-1", "supabase-user-123")
        .await
        .expect("resolve")
        .expect("found by external id");
    assert_eq!(resolved.id, "part-user");

    // --- append messages + page ---
    store
        .append_message(message("msg-1", "conv-1", Direction::Inbound, "hi"))
        .await
        .expect("append 1");
    store
        .append_message(message("msg-2", "conv-1", Direction::Outbound, "hello!"))
        .await
        .expect("append 2");

    let page = store
        .list_messages_by_conversation(MessageQuery::new("conv-1", 1))
        .await
        .expect("page 1");
    assert_eq!(page.messages.len(), 1);
    assert_eq!(page.messages[0].id, "msg-1");
    assert!(page.next_cursor.is_some(), "more messages remain");

    let page2 = store
        .list_messages_by_conversation(MessageQuery {
            conversation_id: "conv-1".into(),
            limit: 5,
            cursor: page.next_cursor,
            descending: false,
        })
        .await
        .expect("page 2");
    assert_eq!(page2.messages.len(), 1);
    assert_eq!(page2.messages[0].id, "msg-2");
    assert!(page2.next_cursor.is_none(), "page exhausted");

    let single = store
        .get_message("msg-2")
        .await
        .expect("get message")
        .expect("exists");
    assert_eq!(single.direction, Direction::Outbound);

    // --- session create/get/update/list ---
    let session = Session {
        session_id: "sess-1".into(),
        conversation_id: "conv-1".into(),
        organization_id: "org-1".into(),
        agent_id: "agent-uuid".into(),
        agent_name: "Smantha".into(),
        user_participant_id: "part-user".into(),
        agent_participant_id: "part-agent".into(),
        thread_id: "thread-abc".into(),
        status: Some(SessionStatus::Active),
        token_count: Some(0),
        message_count: Some(0),
        metadata: None,
        created_at: Some(Utc::now()),
        updated_at: Some(Utc::now()),
        ended_at: None,
        last_activity_at: Some(Utc::now()),
    };
    store.create_session(session).await.expect("create session");

    let bumped = store
        .update_session(
            "sess-1",
            SessionUpdate {
                token_count: Some(42),
                message_count: Some(2),
                status: Some(SessionStatus::Idle),
                ..Default::default()
            },
        )
        .await
        .expect("update session");
    assert_eq!(bumped.token_count, Some(42));
    assert_eq!(bumped.status, Some(SessionStatus::Idle));

    let sessions = store
        .list_sessions_by_conversation("conv-1")
        .await
        .expect("list sessions");
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].thread_id, "thread-abc");

    // --- checkpoint round-trip through the CheckpointStore accessor ---
    let checkpoints = store.checkpoints();
    let engine_conv = EngineConversation::new(100_000).with_system_prompt("ref runtime");
    let cp = Checkpoint::new("agent-uuid", &engine_conv, 1).with_metadata("threadId", "thread-abc");
    checkpoints.save(&cp).expect("save checkpoint");

    let latest = checkpoints
        .load_latest("agent-uuid")
        .expect("load")
        .expect("checkpoint exists");
    assert_eq!(latest.agent_id, "agent-uuid");
    assert_eq!(latest.iteration, 1);
    assert_eq!(
        latest.metadata.get("threadId").map(String::as_str),
        Some("thread-abc")
    );

    // --- knowledge accessor plugs in too ---
    let kb = store.knowledge();
    kb.ingest(smooth_operator_core::Document::new(
        "smooth-agent is the service layer over smooth-operator",
        "docs/ARCHITECTURE.md",
        smooth_operator_core::DocumentType::Documentation,
    ))
    .expect("ingest");
    let results = kb.query("smooth-agent service", 5).expect("query");
    assert!(!results.is_empty());
}
