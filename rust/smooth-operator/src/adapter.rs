//! The `StorageAdapter` seam.
//!
//! smooth-operator never names a database in application or agent code: everything
//! goes through this one trait (see `docs/STORAGE.md`). Production backends
//! (Postgres for k8s, DynamoDB for AWS serverless) implement it; the in-memory
//! adapter in `adapters/in-memory` is the conformance baseline.
//!
//! The conversation / participant / message / session slices are async (their
//! production backends are network calls). The checkpoint and knowledge slices
//! are exposed as accessors returning smooth-operator's own
//! [`CheckpointStore`](smooth_operator_core::CheckpointStore) and
//! [`KnowledgeBase`](smooth_operator_core::KnowledgeBase) — both *synchronous* traits
//! in smooth-operator-core — so the engine plugs straight in without an adapter shim.

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use smooth_operator_core::{CheckpointStore, KnowledgeBase, Memory};

use crate::access_control::AccessContext;
use crate::domain::{Conversation, Message, Participant, Session, SessionStatus};

/// Partial update for a conversation. `None` fields are left unchanged.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationUpdate {
    pub name: Option<String>,
    pub metadata_json: Option<serde_json::Value>,
    pub analytics_json: Option<serde_json::Value>,
}

/// Partial update for a session (status / counters / activity timestamp).
/// `None` fields are left unchanged.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionUpdate {
    pub status: Option<SessionStatus>,
    pub token_count: Option<u64>,
    pub message_count: Option<u64>,
    pub last_activity_at: Option<chrono::DateTime<chrono::Utc>>,
    pub ended_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Replace the session's metadata blob (th-ca579c).
    ///
    /// Without this there was no way to write session metadata back through the
    /// adapter, so everything the server kept there — `otpVerified` above all —
    /// lived only in the serving pod's memory and died on a pod hop or a roll.
    /// A caller who verified their identity on one pod was unverified on the
    /// next, and no code path could have fixed that from outside this struct.
    ///
    /// Whole-blob replace, not a merge: the caller reads, edits, and writes back,
    /// which keeps the adapter contract dumb and makes the read-modify-write
    /// window explicit at the call site rather than hidden in every adapter.
    pub metadata: Option<std::collections::HashMap<String, serde_json::Value>>,
}

/// A page of messages, newest-or-oldest-first per the adapter's contract,
/// with an opaque cursor for the next page (`None` when exhausted).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessagePage {
    pub messages: Vec<Message>,
    /// Opaque cursor to pass back as `MessageQuery::cursor` for the next page.
    pub next_cursor: Option<String>,
}

/// Paging / ordering parameters for `messages.list_by_conversation`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageQuery {
    pub conversation_id: String,
    /// Max messages to return in this page.
    pub limit: usize,
    /// Opaque cursor from a prior `MessagePage::next_cursor`.
    pub cursor: Option<String>,
    /// When true, return newest messages first (the common "recent" read).
    pub descending: bool,
}

impl MessageQuery {
    /// A first-page query for `conversation_id`, oldest-first.
    pub fn new(conversation_id: impl Into<String>, limit: usize) -> Self {
        Self {
            conversation_id: conversation_id.into(),
            limit,
            cursor: None,
            descending: false,
        }
    }
}

/// One row of a conversation sidebar: the conversation plus what the row shows
/// beside it, computed by the storage layer so a listing costs one read rather
/// than one per conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationSummary {
    pub conversation: Conversation,
    /// Flat text of the conversation's FIRST inbound (user) message — the title
    /// fallback when the conversation carries no meaningful `name`.
    pub first_inbound_text: Option<String>,
    /// How many messages the conversation holds. Always > 0: empty
    /// conversations are not summarized. The default implementation counts at
    /// most [`SUMMARY_MESSAGE_CAP`]; an adapter that counts in the query may
    /// report the exact figure.
    pub message_count: usize,
}

/// The most messages the default [`StorageAdapter::list_owned_conversation_summaries`]
/// reads per conversation (it pages oldest-first, so the first inbound message
/// is always inside the cap).
pub const SUMMARY_MESSAGE_CAP: usize = 200;

/// Whether `participant` is the conversation's owning **user** with
/// `user_email`. Emails are compared case-insensitively (mail domains are, and
/// IdPs differ on local-part casing), and a blank email never matches — so a
/// participant row with no email can't be claimed by an emailless caller.
#[must_use]
pub fn is_owner(participant: &Participant, user_email: &str) -> bool {
    if user_email.trim().is_empty() {
        return false;
    }
    participant.participant_type == crate::domain::ParticipantType::User
        && participant
            .email
            .as_deref()
            .is_some_and(|e| e.trim().eq_ignore_ascii_case(user_email.trim()))
}

/// The single storage seam. All slices are backend-agnostic.
#[async_trait]
pub trait StorageAdapter: Send + Sync {
    // ---- conversations ---------------------------------------------------

    /// Create (or idempotently return) a conversation.
    async fn create_conversation(&self, conversation: Conversation) -> Result<Conversation>;

    /// Fetch a conversation by id.
    async fn get_conversation(&self, id: &str) -> Result<Option<Conversation>>;

    /// List conversations owned by an organization (newest first).
    async fn list_conversations_by_org(&self, organization_id: &str) -> Result<Vec<Conversation>>;

    /// List the conversations in `organization_id` that `user_email` **owns** —
    /// i.e. that carry a `user` participant with that email (case-insensitive).
    ///
    /// This is the per-user scope for conversation reads on a multi-user
    /// deployment: org scoping alone lets any member of an org enumerate every
    /// other member's conversations. The filter belongs *in the query*, not
    /// applied to an already-limited page, so a caller's `limit` counts rows the
    /// user can actually see.
    ///
    /// The default implementation is correct for any adapter — it filters
    /// [`list_conversations_by_org`](Self::list_conversations_by_org) through
    /// each conversation's participants — so a new adapter is scoped by
    /// construction and can never be silently fail-open. Override it when the
    /// backend can push the join down (Postgres does).
    async fn list_conversations_by_org_and_user(
        &self,
        organization_id: &str,
        user_email: &str,
    ) -> Result<Vec<Conversation>> {
        let mut owned = Vec::new();
        for conversation in self.list_conversations_by_org(organization_id).await? {
            let participants = self
                .list_participants_by_conversation(&conversation.id)
                .await?;
            if participants.iter().any(|p| is_owner(p, user_email)) {
                owned.push(conversation);
            }
        }
        Ok(owned)
    }

    /// The sidebar rows for the conversations in `organization_id` that
    /// `user_email` **owns** (exactly the set
    /// [`list_conversations_by_org_and_user`](Self::list_conversations_by_org_and_user)
    /// returns), dropping conversations with no messages, most recently updated
    /// first, at most `limit` rows.
    ///
    /// This is the `list_conversations` read for a host that requires owned
    /// conversations. The default composes the per-conversation reads, so it is
    /// correct for any adapter but costs a message read per owned conversation —
    /// and a host that mints a conversation on every page load owns a lot of
    /// empty ones. Override it when the backend can answer in one query.
    async fn list_owned_conversation_summaries(
        &self,
        organization_id: &str,
        user_email: &str,
        limit: usize,
    ) -> Result<Vec<ConversationSummary>> {
        let mut rows = Vec::new();
        for conversation in self
            .list_conversations_by_org_and_user(organization_id, user_email)
            .await?
        {
            let page = self
                .list_messages_by_conversation(MessageQuery::new(
                    &conversation.id,
                    SUMMARY_MESSAGE_CAP,
                ))
                .await?;
            if page.messages.is_empty() {
                continue;
            }
            let first_inbound_text = page
                .messages
                .iter()
                .find(|m| matches!(m.direction, crate::domain::Direction::Inbound))
                .and_then(|m| m.content.flat_text());
            rows.push(ConversationSummary {
                conversation,
                first_inbound_text,
                message_count: page.messages.len(),
            });
        }
        rows.sort_by_key(|r| std::cmp::Reverse(r.conversation.updated_at));
        rows.truncate(limit);
        Ok(rows)
    }

    /// Apply a partial update to a conversation; returns the updated row.
    async fn update_conversation(
        &self,
        id: &str,
        update: ConversationUpdate,
    ) -> Result<Conversation>;

    // ---- participants ----------------------------------------------------

    /// Add a participant to a conversation.
    async fn add_participant(&self, participant: Participant) -> Result<Participant>;

    /// Fetch a participant by id.
    async fn get_participant(&self, id: &str) -> Result<Option<Participant>>;

    /// List all participants in a conversation.
    async fn list_participants_by_conversation(
        &self,
        conversation_id: &str,
    ) -> Result<Vec<Participant>>;

    /// Resolve a participant within a conversation by its external identity
    /// (e.g. Supabase auth user id). Used to re-attach a returning user.
    async fn resolve_participant_by_external_id(
        &self,
        conversation_id: &str,
        external_id: &str,
    ) -> Result<Option<Participant>>;

    // ---- messages --------------------------------------------------------

    /// Append a message to a conversation.
    async fn append_message(&self, message: Message) -> Result<Message>;

    /// Fetch a message by id.
    async fn get_message(&self, id: &str) -> Result<Option<Message>>;

    /// List messages in a conversation, paged.
    async fn list_messages_by_conversation(&self, query: MessageQuery) -> Result<MessagePage>;

    // ---- sessions --------------------------------------------------------

    /// Create a session (binds a conversation to a smooth-operator thread).
    async fn create_session(&self, session: Session) -> Result<Session>;

    /// Fetch a session by id.
    async fn get_session(&self, session_id: &str) -> Result<Option<Session>>;

    /// Apply a partial update (status / counts / activity) to a session.
    async fn update_session(&self, session_id: &str, update: SessionUpdate) -> Result<Session>;

    /// List sessions attached to a conversation.
    async fn list_sessions_by_conversation(&self, conversation_id: &str) -> Result<Vec<Session>>;

    // ---- engine accessors ------------------------------------------------

    /// The checkpoint store, ready to hand to a smooth-operator `Agent`
    /// via `Agent::with_checkpoint_store`. Synchronous trait — the engine
    /// calls it directly.
    fn checkpoints(&self) -> Arc<dyn CheckpointStore>;

    /// The knowledge base, ready to hand to a smooth-operator `AgentConfig`
    /// via `AgentConfig::with_knowledge`. Synchronous trait.
    ///
    /// This handle performs **org isolation only** — it does not enforce
    /// within-org document-level ACLs. The chat retrieval path MUST use
    /// [`knowledge_for_access`](Self::knowledge_for_access) instead so a
    /// restricted document (e.g. a private GitHub repo scoped to a group) is
    /// never returned to a requester who lacks the entitlement.
    fn knowledge(&self) -> Arc<dyn KnowledgeBase>;

    /// An **ACL-enforcing** knowledge handle bound to the requester's
    /// [`AccessContext`]: its `query` returns only documents the requester is
    /// entitled to read (org-public docs, docs the requester's user id is on, or
    /// docs any of the requester's groups is on). This is the handle the chat
    /// retrieval path (auto-injected context **and** the `knowledge_search`
    /// tool) MUST read through — see `docs/ACCESS-CONTROL.md`.
    ///
    /// ## Default — **fail closed for ACL'd content**
    ///
    /// The default implementation wraps [`knowledge`](Self::knowledge) in an
    /// [`AclKnowledgeStore`](crate::access_control::AclKnowledgeStore) reader,
    /// which enforces **both** boundaries from its side table: the tenant
    /// boundary (the document's recorded org vs `access.organization_id` —
    /// feature gap G7) and the within-org user/group ACL. Documents ingested
    /// through a *different* store instance are absent from that side table, so
    /// they are dropped for a requester carrying an org and treated as org-public
    /// for one carrying none.
    ///
    /// Backends that can persist + read back a document's org and ACL (the
    /// in-memory adapter via a shared store; Postgres / DynamoDB via a stored
    /// column / partition key) **override** this method to enforce both durably,
    /// so the filter survives the ingest→serve process boundary.
    fn knowledge_for_access(&self, access: &AccessContext) -> Arc<dyn KnowledgeBase> {
        crate::access_control::AclKnowledgeStore::new(self.knowledge()).reader(access.clone())
    }

    /// A durable [`Memory`] handle for the requester, ready to hand to a
    /// smooth-operator `AgentConfig` via `AgentConfig::with_memory`. When
    /// `Some`, the engine auto-recalls relevant memories into every turn
    /// (`build_context_messages` → `memory.recall(msg, 5)`); when `None` (the
    /// default) the turn carries no auto-recall.
    ///
    /// Defaults to `None` so no existing backend changes behavior — auto-recall
    /// across all hosted orgs is a deliberate product decision, not a side
    /// effect of adding this seam. Big Smooth's single-tenant SQLite adapter
    /// overrides this to return its store, lighting up auto-recall for the
    /// personal daemon.
    ///
    /// `access` is threaded (mirroring [`knowledge_for_access`](Self::knowledge_for_access))
    /// so a multi-tenant backend can bind memory to the requester's org/user;
    /// single-tenant adapters ignore it.
    fn memory_for_access(&self, access: &AccessContext) -> Option<Arc<dyn Memory>> {
        let _ = access;
        None
    }
}
