//! The `StorageAdapter` seam.
//!
//! smooth-agent never names a database in application or agent code: everything
//! goes through this one trait (see `docs/STORAGE.md`). Production backends
//! (Postgres for k8s, DynamoDB for AWS serverless) implement it; the in-memory
//! adapter in `adapters/in-memory` is the conformance baseline.
//!
//! The conversation / participant / message / session slices are async (their
//! production backends are network calls). The checkpoint and knowledge slices
//! are exposed as accessors returning smooth-operator's own
//! [`CheckpointStore`](smooth_operator::CheckpointStore) and
//! [`KnowledgeBase`](smooth_operator::KnowledgeBase) — both *synchronous* traits
//! in smooth-operator — so the engine plugs straight in without an adapter shim.

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use smooth_operator::{CheckpointStore, KnowledgeBase};

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
    fn knowledge(&self) -> Arc<dyn KnowledgeBase>;
}
