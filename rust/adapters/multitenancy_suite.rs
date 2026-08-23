//! **Shared** multi-tenancy conformance suite (feature gap G7) — one suite, every
//! storage backend.
//!
//! Included verbatim by each adapter's `tests/multitenancy.rs` via
//! `#[path = "../../multitenancy_suite.rs"] mod suite;`, so the isolation
//! property is asserted from exactly ONE source. A backend that drifts fails
//! here, not in a per-adapter copy nobody kept in sync.
//!
//! ## The shape it tests
//!
//! One process, ONE adapter instance, two tenants (`ORG_A` / `ORG_B`) — the
//! multi-tenant pod shape, and the harshest variant: every slice is literally
//! the same object for both tenants, so a missing org partition shows up
//! immediately instead of hiding behind two separate instances. The two
//! `&dyn StorageAdapter` parameters are each org's *view* of that one store;
//! every caller passes the same handle twice today.
//!
//! ## What it guarantees
//!
//! 1. **Conversations** — org A's conversations never appear in an org B listing,
//!    by-org or by-org-and-user, even when the SAME user email owns one in each.
//! 2. **Idempotency keys are per-org** — the same `idempotency_key` used by two
//!    orgs yields two distinct conversations. If the idempotency claim were
//!    org-blind, org B's create would hand back **org A's conversation row**.
//! 3. **Messages / participants / sessions** — reachable only through their own
//!    org's conversation; a cross-org listing never sees them, and an update in
//!    one org does not touch the other's row.
//! 4. **Knowledge** — a document ingested for org A is never returned to a
//!    retrieval bound to org B's [`AccessContext`], on every backend; and a
//!    document ingested through the **org-blind** `knowledge()` handle still
//!    lands in the tenant its `org_id` metadata names (neither lost nor shared).
//! 5. **Checkpoints** — a checkpoint saved under one agent id is not visible
//!    under another.
//!
//! ## What it deliberately does NOT claim
//!
//! `StorageAdapter`'s by-id reads (`get_conversation`, `get_message`,
//! `get_session`, `list_participants_by_conversation`) take no org and are
//! **not** org-checked at the adapter — enforcement lives at the caller
//! (`smooth-operator-server`'s `scoped_session`). The suite asserts the *listing*
//! boundary, which is the one the adapter owns.

#![allow(dead_code)]

use chrono::Utc;

use smooth_operator::access_control::AccessContext;
use smooth_operator::adapter::{MessageQuery, SessionUpdate, StorageAdapter};
use smooth_operator::domain::{
    Conversation, Direction, Message, MessageContent, Participant, ParticipantType, Platform,
    Session, SessionStatus,
};
use smooth_operator_core::{
    Checkpoint, Conversation as EngineConversation, Document, DocumentType,
};

pub const ORG_A: &str = "org-alpha";
pub const ORG_B: &str = "org-beta";

/// The metadata key every ingested chunk carries naming its owning org — set by
/// the ingestion pipeline (`ingestion::pipeline`) on every document. Kept in
/// sync with `smooth_operator::access_control::ORG_METADATA_KEY`.
pub const ORG_METADATA_KEY: &str = "org_id";

/// A shared user email — the SAME person in both orgs. Proves isolation is by
/// org, not incidentally by a differing owner email.
pub const SHARED_EMAIL: &str = "shared.person@example.test";

/// The idempotency key both orgs use, verbatim. The collision is the point.
const SHARED_IDEMPOTENCY_KEY: &str = "idem-shared-across-tenants";

fn conversation(id: &str, org: &str, idempotency_key: &str) -> Conversation {
    Conversation {
        id: id.into(),
        platform: Platform::Web,
        name: format!("{org} chat"),
        organization_id: org.into(),
        idempotency_key: idempotency_key.into(),
        metadata_json: None,
        analytics_json: None,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    }
}

fn owner(id: &str, conv: &str, org: &str) -> Participant {
    Participant {
        id: id.into(),
        conversation_id: conv.into(),
        organization_id: org.into(),
        participant_type: ParticipantType::User,
        external_id: None,
        internal_id: None,
        browser_fingerprint: None,
        browser_info: None,
        name: id.into(),
        email: Some(SHARED_EMAIL.into()),
        phone: None,
        crm_contact_id: None,
        metadata_json: None,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    }
}

fn message(id: &str, conv: &str, org: &str, text: &str) -> Message {
    Message {
        id: id.into(),
        external_id: None,
        organization_id: Some(org.into()),
        conversation_id: Some(conv.into()),
        direction: Direction::Inbound,
        content: MessageContent::from_text(text),
        from: None,
        to: None,
        metadata_json: None,
        analytics_json: None,
        created_at: Utc::now(),
        updated_at: None,
    }
}

fn session(id: &str, conv: &str, org: &str) -> Session {
    Session {
        session_id: id.into(),
        conversation_id: conv.into(),
        organization_id: org.into(),
        agent_id: Some(format!("agent-{org}")),
        agent_name: "Smantha".into(),
        user_participant_id: format!("owner-{org}"),
        agent_participant_id: format!("bot-{org}"),
        thread_id: format!("thread-{org}"),
        status: Some(SessionStatus::Active),
        token_count: Some(0),
        message_count: Some(0),
        metadata: None,
        created_at: Some(Utc::now()),
        updated_at: Some(Utc::now()),
        ended_at: None,
        last_activity_at: Some(Utc::now()),
    }
}

/// A document owned by `org`, stamped the way the ingestion pipeline stamps it.
/// `marker` is a distinctive term so retrieval can be asserted by content.
fn org_document(id: &str, org: &str, marker: &str) -> Document {
    let mut doc = Document::new(
        format!("The {marker} escalation code for this tenant is {marker}-9137."),
        format!("policies/{org}.md"),
        DocumentType::Documentation,
    )
    .with_metadata(ORG_METADATA_KEY, org)
    // The pipeline also stamps `document_id`; the Postgres backend stores it as
    // the result's `document_id`, which is the key the org/ACL side table uses.
    .with_metadata("document_id", id);
    // Pin the id so `KnowledgeResult::document_id` is assertable (the in-memory
    // backend reports the document's own id; `Document::new` would randomise it).
    doc.id = id.into();
    doc
}

/// Run the whole suite. `a` is the org-A-scoped handle, `b` the org-B-scoped
/// handle, both over the same backing store. `suffix` disambiguates row ids so
/// the suite can run repeatedly against a shared (containerised) database.
pub async fn assert_multitenancy(a: &dyn StorageAdapter, b: &dyn StorageAdapter, suffix: &str) {
    let conv_a = format!("conv-a-{suffix}");
    let conv_b = format!("conv-b-{suffix}");

    // ---- 1. conversations are listed per org ------------------------------
    a.create_conversation(conversation(&conv_a, ORG_A, &format!("idem-a-{suffix}")))
        .await
        .expect("create org-A conversation");
    b.create_conversation(conversation(&conv_b, ORG_B, &format!("idem-b-{suffix}")))
        .await
        .expect("create org-B conversation");

    let listed_a = a
        .list_conversations_by_org(ORG_A)
        .await
        .expect("list org A");
    let listed_b = b
        .list_conversations_by_org(ORG_B)
        .await
        .expect("list org B");

    assert!(
        listed_a.iter().any(|c| c.id == conv_a),
        "org A must see its own conversation"
    );
    assert!(
        !listed_a.iter().any(|c| c.id == conv_b),
        "CROSS-TENANT LEAK: org B's conversation {conv_b} appeared in org A's listing"
    );
    assert!(
        !listed_b.iter().any(|c| c.id == conv_a),
        "CROSS-TENANT LEAK: org A's conversation {conv_a} appeared in org B's listing"
    );
    assert!(
        listed_a.iter().all(|c| c.organization_id == ORG_A),
        "org A's listing contained a foreign organization_id"
    );

    // ---- 2. idempotency keys do not collide across tenants -----------------
    // Org A claims the key first; org B then creates with the SAME key. An
    // org-blind idempotency claim would return org A's row to org B — handing a
    // whole conversation (and every message on it) to the wrong tenant.
    let claimed_a = a
        .create_conversation(conversation(
            &format!("idem-a-{suffix}"),
            ORG_A,
            SHARED_IDEMPOTENCY_KEY,
        ))
        .await
        .expect("org A claims the shared idempotency key");
    let claimed_b = b
        .create_conversation(conversation(
            &format!("idem-b-{suffix}"),
            ORG_B,
            SHARED_IDEMPOTENCY_KEY,
        ))
        .await
        .expect("org B claims the same idempotency key");
    assert_ne!(
        claimed_a.id, claimed_b.id,
        "CROSS-TENANT LEAK: the idempotency claim is not org-scoped — org B was handed org A's conversation"
    );
    assert_eq!(claimed_b.organization_id, ORG_B);

    // ---- 3. participants + messages ride their own org's conversation ------
    a.add_participant(owner(&format!("owner-a-{suffix}"), &conv_a, ORG_A))
        .await
        .expect("org A owner");
    b.add_participant(owner(&format!("owner-b-{suffix}"), &conv_b, ORG_B))
        .await
        .expect("org B owner");

    a.append_message(message(
        &format!("msg-a-{suffix}"),
        &conv_a,
        ORG_A,
        "org A private message",
    ))
    .await
    .expect("org A message");
    b.append_message(message(
        &format!("msg-b-{suffix}"),
        &conv_b,
        ORG_B,
        "org B private message",
    ))
    .await
    .expect("org B message");

    // The SAME user email exists in both orgs. The per-user listing must still
    // be org-partitioned — org is the outer boundary, ownership the inner one.
    let owned_a = a
        .list_conversations_by_org_and_user(ORG_A, SHARED_EMAIL)
        .await
        .expect("org A owned listing");
    assert!(
        owned_a.iter().any(|c| c.id == conv_a),
        "the shared user must see their own org-A conversation"
    );
    assert!(
        !owned_a.iter().any(|c| c.id == conv_b),
        "CROSS-TENANT LEAK: the shared user saw org B's conversation while scoped to org A"
    );

    let page_a = a
        .list_messages_by_conversation(MessageQuery::new(&conv_a, 50))
        .await
        .expect("org A messages");
    assert!(
        page_a
            .messages
            .iter()
            .all(|m| m.organization_id.as_deref() == Some(ORG_A)),
        "CROSS-TENANT LEAK: org A's message page carried a foreign org's message"
    );

    // ---- 4. sessions ------------------------------------------------------
    let sess_a = format!("sess-a-{suffix}");
    let sess_b = format!("sess-b-{suffix}");
    a.create_session(session(&sess_a, &conv_a, ORG_A))
        .await
        .expect("org A session");
    b.create_session(session(&sess_b, &conv_b, ORG_B))
        .await
        .expect("org B session");

    let sessions_a = a
        .list_sessions_by_conversation(&conv_a)
        .await
        .expect("org A sessions");
    assert_eq!(sessions_a.len(), 1, "org A's conversation has one session");
    assert_eq!(sessions_a[0].organization_id, ORG_A);
    assert!(
        !sessions_a.iter().any(|s| s.session_id == sess_b),
        "CROSS-TENANT LEAK: org B's session listed under org A's conversation"
    );

    // Updating org B's session must not disturb org A's.
    b.update_session(
        &sess_b,
        SessionUpdate {
            token_count: Some(4242),
            ..Default::default()
        },
    )
    .await
    .expect("update org B session");
    let after = a
        .list_sessions_by_conversation(&conv_a)
        .await
        .expect("org A sessions after org B update");
    assert_eq!(
        after[0].token_count,
        Some(0),
        "org B's session update bled into org A's session"
    );

    // ---- 5. knowledge -----------------------------------------------------
    // Each org ingests through ITS OWN access-bound handle — the seam the
    // ingestion path uses (`knowledge_for_access`), which is what stamps the
    // owning org on the stored row.
    let doc_a = format!("doc-a-{suffix}");
    let doc_b = format!("doc-b-{suffix}");
    let marker_a = format!("alphamarker{suffix}");
    let marker_b = format!("betamarker{suffix}");

    let access_a = AccessContext::default().with_organization_id(ORG_A);
    let access_b = AccessContext::default().with_organization_id(ORG_B);

    a.knowledge_for_access(&access_a)
        .ingest(org_document(&doc_a, ORG_A, &marker_a))
        .expect("ingest org A doc");
    b.knowledge_for_access(&access_b)
        .ingest(org_document(&doc_b, ORG_B, &marker_b))
        .expect("ingest org B doc");

    // Positive control: org A finds its own document. Without this, a backend
    // that returns NOTHING would pass the isolation assertion vacuously.
    let hits_a = a
        .knowledge_for_access(&access_a)
        .query(&format!("{marker_a} escalation code"), 10)
        .expect("org A retrieval");
    assert!(
        hits_a.iter().any(|r| r.chunk.contains(&marker_a)),
        "org A could not retrieve its OWN document — the isolation assertion below would be vacuous"
    );

    // The leak assertion: org B's retrieval, using ORG A's distinctive query,
    // must not surface org A's document.
    let hits_b = b
        .knowledge_for_access(&access_b)
        .query(&format!("{marker_a} escalation code"), 10)
        .expect("org B retrieval");
    assert!(
        !hits_b.iter().any(|r| r.chunk.contains(&marker_a)),
        "CROSS-TENANT LEAK: org B's knowledge retrieval returned org A's document: {hits_b:?}"
    );
    assert!(
        !hits_b.iter().any(|r| r.document_id == doc_a),
        "CROSS-TENANT LEAK: org B's knowledge retrieval returned org A's document id"
    );

    // ---- 5b. ingest through the ORG-BLIND `knowledge()` handle -------------
    // `knowledge()` takes no org, and the admin connector-index path used it for
    // every tenant. The document's own `org_id` metadata (which the ingestion
    // pipeline stamps on every chunk) must therefore be enough to place the row
    // in the right tenant — otherwise a connector run either lands in whichever
    // org the handle happened to be built for (DynamoDB) or in no org at all
    // (Postgres wrote `organization_id = NULL`, which the org-filtered read can
    // never match, so retrieval silently returned nothing).
    let blind_doc = format!("doc-blind-{suffix}");
    let blind_marker = format!("blindmarker{suffix}");
    a.knowledge()
        .ingest(org_document(&blind_doc, ORG_A, &blind_marker))
        .expect("org-blind ingest of an org-A-stamped document");

    let blind_hits_a = a
        .knowledge_for_access(&access_a)
        .query(&format!("{blind_marker} escalation code"), 10)
        .expect("org A retrieval of the blind-ingested doc");
    assert!(
        blind_hits_a.iter().any(|r| r.chunk.contains(&blind_marker)),
        "a document ingested through the org-blind handle but stamped with org A's \
         `{ORG_METADATA_KEY}` must be retrievable by org A — it was lost instead"
    );
    let blind_hits_b = b
        .knowledge_for_access(&access_b)
        .query(&format!("{blind_marker} escalation code"), 10)
        .expect("org B retrieval of the blind-ingested doc");
    assert!(
        !blind_hits_b.iter().any(|r| r.chunk.contains(&blind_marker)),
        "CROSS-TENANT LEAK: a doc ingested through the org-blind handle reached org B: {blind_hits_b:?}"
    );

    // ---- 6. checkpoints ---------------------------------------------------
    // `CheckpointStore` has no org dimension — it is keyed by agent id, and the
    // server mints a fresh per-turn agent id. Assert the key boundary holds:
    // one agent's checkpoint is invisible under another's id.
    let engine_conv = EngineConversation::new(100_000).with_system_prompt("tenant A only");
    let agent_a = format!("agent-a-{suffix}");
    let agent_b = format!("agent-b-{suffix}");
    a.checkpoints()
        .save(&Checkpoint::new(&agent_a, &engine_conv, 1))
        .expect("save org A checkpoint");

    assert!(
        b.checkpoints()
            .load_latest(&agent_b)
            .expect("load org B checkpoint")
            .is_none(),
        "CROSS-TENANT LEAK: org B loaded a checkpoint it never wrote"
    );
    assert!(
        b.checkpoints()
            .list(&agent_b)
            .expect("list org B checkpoints")
            .is_empty(),
        "CROSS-TENANT LEAK: org A's checkpoint listed under org B's agent id"
    );
    assert_eq!(
        a.checkpoints()
            .list(&agent_a)
            .expect("list org A checkpoints")
            .len(),
        1,
        "org A must still see its own checkpoint"
    );
}
