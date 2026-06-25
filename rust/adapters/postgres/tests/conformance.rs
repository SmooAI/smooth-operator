//! Conformance tests for the Postgres + pgvector `StorageAdapter`, run against a
//! real pgvector container via testcontainers.
//!
//! Mirrors `adapters/in-memory/tests/conformance.rs` slice-for-slice — conversation
//! CRUD + idempotency, user + ai-agent participants + external-id resolve, message
//! paging, session create/update — and adds the two production-only slices:
//!
//! - **checkpoint save + load** through the `PostgresCheckpointStore` accessor
//!   (proving the engine plugs into the same DB), and
//! - **knowledge ingest + hybrid retrieve** (dense pgvector ∪ sparse tsvector →
//!   RRF) with the `DeterministicEmbedder`, asserting a distinctive seeded doc
//!   ranks first.
//!
//! The container requires a running Docker daemon. If Docker is unavailable the
//! test **skips** (prints a notice and returns Ok) rather than failing, so CI
//! without Docker stays green.

use chrono::Utc;

use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, GenericImage, ImageExt};

use smooth_operator_core::{
    Checkpoint, Conversation as EngineConversation, Document, DocumentType,
};

use smooth_operator::adapter::{ConversationUpdate, MessageQuery, SessionUpdate, StorageAdapter};
use smooth_operator::domain::{
    Conversation, Direction, Message, MessageContent, Participant, ParticipantType, Platform,
    Session, SessionStatus,
};
use smooth_operator_adapter_postgres::PostgresAdapter;

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

/// Spin up a throwaway `pgvector/pgvector:pg16` container. Returns `Ok(None)` if
/// Docker is unavailable so the caller can skip rather than fail.
async fn start_pgvector() -> anyhow::Result<Option<(ContainerAsync<GenericImage>, String)>> {
    let image = GenericImage::new("pgvector/pgvector", "pg16")
        .with_wait_for(WaitFor::message_on_stderr(
            "database system is ready to accept connections",
        ))
        .with_exposed_port(5432.tcp())
        .with_env_var("POSTGRES_USER", "postgres")
        .with_env_var("POSTGRES_PASSWORD", "postgres")
        .with_env_var("POSTGRES_DB", "postgres");

    match image.start().await {
        Ok(node) => {
            let host = node.get_host().await?;
            let port = node.get_host_port_ipv4(5432).await?;
            let conn_str =
                format!("host={host} port={port} user=postgres password=postgres dbname=postgres");
            Ok(Some((node, conn_str)))
        }
        Err(e) => {
            eprintln!("SKIP: could not start pgvector container (Docker unavailable?): {e}");
            Ok(None)
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn full_lifecycle_through_the_postgres_adapter() -> anyhow::Result<()> {
    let Some((_node, conn_str)) = start_pgvector().await? else {
        return Ok(()); // Docker unavailable — skip, don't fail.
    };

    let store = PostgresAdapter::connect(&conn_str).await?;

    // --- conversation create/get/list/update ---
    let conv = store
        .create_conversation(conversation("conv-1", "org-1"))
        .await?;
    assert_eq!(conv.id, "conv-1");

    // idempotency: same org + idempotencyKey returns the existing row (the
    // second insert carries a different id but must resolve to conv-1).
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
    assert_eq!(by_org.len(), 1);

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

    // --- two participants: a user (with external id) and an ai-agent ---
    let mut user = participant("part-user", "conv-1", "org-1", ParticipantType::User);
    user.external_id = Some("supabase-user-123".into());
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

    // --- append messages + page (oldest-first, cursor) ---
    store
        .append_message(message("msg-1", "conv-1", Direction::Inbound, "hi"))
        .await?;
    store
        .append_message(message("msg-2", "conv-1", Direction::Outbound, "hello!"))
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
            limit: 5,
            cursor: page.next_cursor,
            descending: false,
        })
        .await?;
    assert_eq!(page2.messages.len(), 1);
    assert_eq!(page2.messages[0].id, "msg-2");
    assert!(page2.next_cursor.is_none(), "page exhausted");

    // descending returns newest first
    let desc = store
        .list_messages_by_conversation(MessageQuery {
            conversation_id: "conv-1".into(),
            limit: 5,
            cursor: None,
            descending: true,
        })
        .await?;
    assert_eq!(desc.messages[0].id, "msg-2");

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

    let sessions = store.list_sessions_by_conversation("conv-1").await?;
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].thread_id, "thread-abc");

    // --- checkpoint round-trip through the PostgresCheckpointStore accessor ---
    // PostgresCheckpointStore is a *synchronous* store backed by the blocking
    // `postgres` crate (it drives its own runtime internally), so its calls must
    // run off the async worker threads via spawn_blocking — exactly how the
    // engine drives it. We assert that the engine-shaped usage round-trips.
    let checkpoints = store.checkpoints();
    let latest = tokio::task::spawn_blocking(
        move || -> anyhow::Result<smooth_operator_core::Checkpoint> {
            let engine_conv = EngineConversation::new(100_000).with_system_prompt("ref runtime");
            let cp = Checkpoint::new("agent-uuid", &engine_conv, 1)
                .with_metadata("threadId", "thread-abc");
            checkpoints.save(&cp)?;
            Ok(checkpoints
                .load_latest("agent-uuid")?
                .expect("checkpoint exists"))
        },
    )
    .await??;
    assert_eq!(latest.agent_id, "agent-uuid");
    assert_eq!(latest.iteration, 1);
    assert_eq!(
        latest.metadata.get("threadId").map(String::as_str),
        Some("thread-abc")
    );
    assert!(
        !latest.conversation.context_window().is_empty(),
        "checkpoint conversation should round-trip with its system message"
    );

    // --- knowledge ingest + hybrid retrieve (dense ∪ sparse → RRF) ---
    let kb = store.knowledge();
    // Seed several docs; one carries a distinctive marker token + phrase.
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
        "The zorblax protocol coordinates smooth-operator agent checkpoints across a pgvector store.",
        "docs/zorblax.md",
        DocumentType::Documentation,
    ))?;

    let results = kb.query("zorblax protocol pgvector checkpoints", 5)?;
    assert!(!results.is_empty(), "hybrid retrieve returned no results");
    assert_eq!(
        results[0].source,
        "docs/zorblax.md",
        "the distinctive seeded doc must rank first (got {:?})",
        results.iter().map(|r| &r.source).collect::<Vec<_>>()
    );

    println!("POSTGRES CONFORMANCE: all slices (CRUD + idempotency + paging + sessions + checkpoint + hybrid knowledge) passed against pgvector/pgvector:pg16");

    // `PostgresAdapter::drop` disposes the sync checkpoint pool off-runtime, so
    // dropping `store` here from async code is safe (no destructor panic).
    drop(store);
    Ok(())
}
