//! Conformance tests for the DynamoDB single-table `StorageAdapter`, run against
//! a real `amazon/dynamodb-local` container via testcontainers.
//!
//! Mirrors `adapters/postgres/tests/conformance.rs` (and the in-memory baseline)
//! slice-for-slice — conversation CRUD + idempotency, user + ai-agent
//! participants + external-id resolve, message cursor paging, session
//! create/update — and adds the two production-only slices:
//!
//! - **checkpoint save/load/list/prune** through the `DynamoCheckpointStore`
//!   accessor (smooth-operator's sync `CheckpointStore`, bridged over the async
//!   SDK), and
//! - **knowledge ingest + brute-force retrieve** with the `DeterministicEmbedder`,
//!   asserting a distinctive seeded doc ranks first.
//!
//! The container requires a running Docker daemon. If Docker is unavailable the
//! test **skips** (prints a notice and returns Ok) rather than failing.

use chrono::Utc;

use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, GenericImage};

use smooth_operator_core::{
    Checkpoint, Conversation as EngineConversation, Document, DocumentType,
};

use smooth_operator::adapter::{ConversationUpdate, MessageQuery, SessionUpdate, StorageAdapter};
use smooth_operator::domain::{
    Conversation, Direction, Message, MessageContent, Participant, ParticipantType, Platform,
    Session, SessionStatus,
};
use smooth_operator_adapter_dynamodb::DynamoDbAdapter;

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

/// Spin up a throwaway `amazon/dynamodb-local` container. Returns `Ok(None)` if
/// Docker is unavailable so the caller can skip rather than fail.
async fn start_dynamodb() -> anyhow::Result<Option<(ContainerAsync<GenericImage>, String)>> {
    let image = GenericImage::new("amazon/dynamodb-local", "latest")
        .with_wait_for(WaitFor::message_on_stdout("Initializing DynamoDB Local"))
        .with_exposed_port(8000.tcp());

    match image.start().await {
        Ok(node) => {
            let host = node.get_host().await?;
            let port = node.get_host_port_ipv4(8000).await?;
            let endpoint = format!("http://{host}:{port}");
            Ok(Some((node, endpoint)))
        }
        Err(e) => {
            eprintln!("SKIP: could not start dynamodb-local container (Docker unavailable?): {e}");
            Ok(None)
        }
    }
}

/// Build an adapter pointed at DynamoDB-Local with dummy static credentials
/// (DynamoDB-Local ignores them but the SDK requires *some* credentials).
async fn connect(endpoint: &str) -> anyhow::Result<DynamoDbAdapter> {
    // SAFETY: these are dummy creds for a throwaway local container; the SDK only
    // needs them to be present. Set before building the adapter's AWS config.
    std::env::set_var("AWS_ACCESS_KEY_ID", "test");
    std::env::set_var("AWS_SECRET_ACCESS_KEY", "test");
    std::env::set_var("AWS_REGION", "us-east-1");
    std::env::set_var("SMOOTH_AGENT_DDB_TABLE", "smooth-operator-test");

    let adapter = DynamoDbAdapter::from_env(Some(endpoint)).await?;
    adapter.create_table().await?;
    Ok(adapter)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn full_lifecycle_through_the_dynamodb_adapter() -> anyhow::Result<()> {
    let Some((_node, endpoint)) = start_dynamodb().await? else {
        return Ok(()); // Docker unavailable — skip, don't fail.
    };

    let store = connect(&endpoint).await?;

    // --- conversation create/get/list/update ---
    let conv = store
        .create_conversation(conversation("conv-1", "org-1"))
        .await?;
    assert_eq!(conv.id, "conv-1");

    // idempotency: same org + idempotencyKey returns the existing row (the
    // second create carries a different id but must resolve to conv-1).
    let mut dup_src = conversation("conv-DUP", "org-1");
    dup_src.idempotency_key = "idem-conv-1".into();
    let dup = store.create_conversation(dup_src).await?;
    assert_eq!(
        dup.id, "conv-1",
        "idempotency must return the pre-existing row"
    );

    let fetched = store.get_conversation("conv-1").await?.expect("exists");
    assert_eq!(fetched.organization_id, "org-1");

    let by_org = store.list_conversations_by_org("org-1").await?;
    assert_eq!(
        by_org.len(),
        1,
        "the duplicate must not create a second row"
    );

    let updated = store
        .update_conversation(
            "conv-1",
            ConversationUpdate {
                name: Some("Renamed".into()),
                ..Default::default()
            },
        )
        .await?;
    assert_eq!(updated.name, "Renamed");
    // Update must be visible through the by-id read path.
    assert_eq!(
        store
            .get_conversation("conv-1")
            .await?
            .expect("exists")
            .name,
        "Renamed"
    );

    // --- two participants: a user (with external id) and an ai-agent ---
    let mut user = participant("part-user", "conv-1", "org-1", ParticipantType::User);
    user.external_id = Some("supabase-user-123".into());
    user.email = Some("Owner@Example.com".into());
    store.add_participant(user).await?;
    store
        .add_participant(participant(
            "part-agent",
            "conv-1",
            "org-1",
            ParticipantType::AiAgent,
        ))
        .await?;

    let participants = store.list_participants_by_conversation("conv-1").await?;
    assert_eq!(participants.len(), 2);

    // --- per-user conversation scope (SECURITY, pearl th-b2c60b) ---
    // DynamoDB has no participant-email index, so it uses the trait's default
    // participant filter — same contract, proven against the real backend.
    let owned = store
        .list_conversations_by_org_and_user("org-1", "owner@example.com")
        .await?;
    assert_eq!(owned.len(), 1, "the owner sees their own conversation");
    assert_eq!(owned[0].id, "conv-1", "email compare is case-insensitive");
    assert!(
        store
            .list_conversations_by_org_and_user("org-1", "someone-else@example.com")
            .await?
            .is_empty(),
        "another user in the SAME org must see nothing"
    );
    assert!(
        store
            .list_conversations_by_org_and_user("org-1", "")
            .await?
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
        .await?
        .expect("found by external id");
    assert_eq!(resolved.id, "part-user");

    let got_part = store.get_participant("part-agent").await?.expect("exists");
    assert_eq!(got_part.participant_type, ParticipantType::AiAgent);

    // --- append messages + page (oldest-first, cursor) ---
    store
        .append_message(message("msg-1", "conv-1", Direction::Inbound, "hi"))
        .await?;
    store
        .append_message(message("msg-2", "conv-1", Direction::Outbound, "hello!"))
        .await?;
    store
        .append_message(message(
            "msg-3",
            "conv-1",
            Direction::Inbound,
            "how are you?",
        ))
        .await?;

    let page = store
        .list_messages_by_conversation(MessageQuery::new("conv-1", 1))
        .await?;
    assert_eq!(page.messages.len(), 1);
    assert_eq!(page.messages[0].id, "msg-1");
    assert!(page.next_cursor.is_some(), "more messages remain");

    let page2 = store
        .list_messages_by_conversation(MessageQuery {
            conversation_id: "conv-1".into(),
            limit: 1,
            cursor: page.next_cursor,
            descending: false,
        })
        .await?;
    assert_eq!(page2.messages.len(), 1);
    assert_eq!(
        page2.messages[0].id, "msg-2",
        "cursor paging is seq-ordered"
    );
    assert!(page2.next_cursor.is_some(), "one more page remains");

    let page3 = store
        .list_messages_by_conversation(MessageQuery {
            conversation_id: "conv-1".into(),
            limit: 5,
            cursor: page2.next_cursor,
            descending: false,
        })
        .await?;
    assert_eq!(page3.messages.len(), 1);
    assert_eq!(page3.messages[0].id, "msg-3");
    assert!(page3.next_cursor.is_none(), "page exhausted");

    // descending returns newest first
    let desc = store
        .list_messages_by_conversation(MessageQuery {
            conversation_id: "conv-1".into(),
            limit: 5,
            cursor: None,
            descending: true,
        })
        .await?;
    assert_eq!(desc.messages[0].id, "msg-3");

    let single = store.get_message("msg-2").await?.expect("exists");
    assert_eq!(single.direction, Direction::Outbound);
    assert_eq!(single.content.text.as_deref(), Some("hello!"));

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
    store.create_session(session).await?;

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
        .await?;
    assert_eq!(bumped.token_count, Some(42));
    assert_eq!(bumped.status, Some(SessionStatus::Idle));

    let got_session = store.get_session("sess-1").await?.expect("exists");
    assert_eq!(got_session.thread_id, "thread-abc");
    assert_eq!(got_session.token_count, Some(42), "update persisted");

    let sessions = store.list_sessions_by_conversation("conv-1").await?;
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].thread_id, "thread-abc");

    // --- checkpoint save/load/list/prune via DynamoCheckpointStore ---
    // The store is a *synchronous* CheckpointStore bridged over the async SDK;
    // exercise it off the async worker threads exactly how the engine drives it.
    let checkpoints = store.checkpoints();
    tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let engine_conv = EngineConversation::new(100_000).with_system_prompt("ref runtime");

        // Empty store.
        assert!(checkpoints.load_latest("agent-uuid")?.is_none());
        assert!(checkpoints.list("agent-uuid")?.is_empty());

        // Save four checkpoints (iterations 1..=4); newest = highest iteration.
        let mut ids = Vec::new();
        for i in 1..=4 {
            let cp = Checkpoint::new("agent-uuid", &engine_conv, i)
                .with_metadata("threadId", "thread-abc");
            ids.push(cp.id.clone());
            checkpoints.save(&cp)?;
        }

        // load_latest -> iteration 4, metadata + conversation round-trip.
        let latest = checkpoints
            .load_latest("agent-uuid")?
            .expect("checkpoint exists");
        assert_eq!(latest.agent_id, "agent-uuid");
        assert_eq!(latest.iteration, 4);
        assert_eq!(
            latest.metadata.get("threadId").map(String::as_str),
            Some("thread-abc")
        );
        assert!(
            !latest.conversation.context_window().is_empty(),
            "checkpoint conversation should round-trip with its system message"
        );

        // load by id.
        let by_id = checkpoints.load(&ids[0])?.expect("checkpoint 1");
        assert_eq!(by_id.iteration, 1);

        // list is newest-first and agent-scoped.
        let list = checkpoints.list("agent-uuid")?;
        assert_eq!(list.len(), 4);
        assert_eq!(list[0].iteration, 4, "list is iteration-descending");
        assert_eq!(list[3].iteration, 1);

        // prune keeps the newest N, returns the count removed.
        let removed = checkpoints.prune("agent-uuid", 2)?;
        assert_eq!(removed, 2);
        let remaining = checkpoints.list("agent-uuid")?;
        assert_eq!(remaining.len(), 2);
        assert_eq!(remaining[0].iteration, 4, "newest survived prune");
        assert_eq!(remaining[1].iteration, 3);

        Ok(())
    })
    .await??;

    // --- knowledge ingest + brute-force retrieve (cosine over DynamoDB) ---
    let kb = store.knowledge();
    let kb = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        kb.ingest(Document::new(
            "The capital of France is Paris and it is known for the Eiffel Tower.",
            "docs/geo-france.md",
            DocumentType::Documentation,
        ))?;
        kb.ingest(Document::new(
            "Photosynthesis lets plants convert sunlight into chemical energy.",
            "docs/biology.md",
            DocumentType::Documentation,
        ))?;
        // The target doc — distinctive marker `zorblax` + a clear topical phrase.
        kb.ingest(Document::new(
            "The zorblax protocol coordinates smooth-operator agent checkpoints across a dynamodb store.",
            "docs/zorblax.md",
            DocumentType::Documentation,
        ))?;

        let results = kb.query("zorblax protocol dynamodb checkpoints", 5)?;
        assert!(!results.is_empty(), "brute-force retrieve returned no results");
        assert_eq!(
            results[0].source,
            "docs/zorblax.md",
            "the distinctive seeded doc must rank first (got {:?})",
            results.iter().map(|r| &r.source).collect::<Vec<_>>()
        );
        Ok(kb)
    })
    .await??;
    drop(kb);

    println!("DYNAMODB CONFORMANCE: all slices (CRUD + idempotency + paging + sessions + checkpoint save/load/list/prune + brute-force knowledge) passed against amazon/dynamodb-local");

    Ok(())
}
