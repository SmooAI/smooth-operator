//! In-memory [`StorageAdapter`] — the conformance / test baseline.
//!
//! All OLTP slices (conversations, participants, messages, sessions) live in
//! `HashMap`s behind a single `RwLock`. The checkpoint and knowledge slices
//! delegate to smooth-operator's own [`MemoryCheckpointStore`] and
//! [`InMemoryKnowledge`], which are exactly what the engine expects — so this
//! adapter is a faithful (if non-durable) stand-in for Postgres/DynamoDB.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use chrono::Utc;

use smooth_agent_core::adapter::{
    ConversationUpdate, MessagePage, MessageQuery, SessionUpdate, StorageAdapter,
};
use smooth_agent_core::domain::{Conversation, Message, Participant, Session};
use smooth_operator::{CheckpointStore, InMemoryKnowledge, KnowledgeBase, MemoryCheckpointStore};

#[derive(Default)]
struct Tables {
    conversations: HashMap<String, Conversation>,
    participants: HashMap<String, Participant>,
    messages: HashMap<String, Message>,
    /// Insertion order of message ids per conversation (append order).
    message_order: HashMap<String, Vec<String>>,
    sessions: HashMap<String, Session>,
}

/// In-memory storage adapter. Cheap to clone is *not* a goal — wrap in `Arc`
/// for sharing.
pub struct InMemoryStorageAdapter {
    tables: RwLock<Tables>,
    checkpoints: Arc<MemoryCheckpointStore>,
    knowledge: Arc<InMemoryKnowledge>,
}

impl InMemoryStorageAdapter {
    pub fn new() -> Self {
        Self {
            tables: RwLock::new(Tables::default()),
            checkpoints: Arc::new(MemoryCheckpointStore::new()),
            knowledge: Arc::new(InMemoryKnowledge::new()),
        }
    }
}

impl Default for InMemoryStorageAdapter {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl StorageAdapter for InMemoryStorageAdapter {
    // ---- conversations ---------------------------------------------------

    async fn create_conversation(&self, conversation: Conversation) -> Result<Conversation> {
        let mut t = self
            .tables
            .write()
            .map_err(|e| anyhow!("lock poisoned: {e}"))?;
        // Idempotency: return the existing row if (org, idempotencyKey) matches.
        if let Some(existing) = t.conversations.values().find(|c| {
            c.organization_id == conversation.organization_id
                && c.idempotency_key == conversation.idempotency_key
        }) {
            return Ok(existing.clone());
        }
        t.conversations
            .insert(conversation.id.clone(), conversation.clone());
        t.message_order.entry(conversation.id.clone()).or_default();
        Ok(conversation)
    }

    async fn get_conversation(&self, id: &str) -> Result<Option<Conversation>> {
        let t = self
            .tables
            .read()
            .map_err(|e| anyhow!("lock poisoned: {e}"))?;
        Ok(t.conversations.get(id).cloned())
    }

    async fn list_conversations_by_org(&self, organization_id: &str) -> Result<Vec<Conversation>> {
        let t = self
            .tables
            .read()
            .map_err(|e| anyhow!("lock poisoned: {e}"))?;
        let mut out: Vec<Conversation> = t
            .conversations
            .values()
            .filter(|c| c.organization_id == organization_id)
            .cloned()
            .collect();
        out.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        Ok(out)
    }

    async fn update_conversation(
        &self,
        id: &str,
        update: ConversationUpdate,
    ) -> Result<Conversation> {
        let mut t = self
            .tables
            .write()
            .map_err(|e| anyhow!("lock poisoned: {e}"))?;
        let conv = t
            .conversations
            .get_mut(id)
            .ok_or_else(|| anyhow!("conversation '{id}' not found"))?;
        if let Some(name) = update.name {
            conv.name = name;
        }
        if update.metadata_json.is_some() {
            conv.metadata_json = update.metadata_json;
        }
        if update.analytics_json.is_some() {
            conv.analytics_json = update.analytics_json;
        }
        conv.updated_at = Utc::now();
        Ok(conv.clone())
    }

    // ---- participants ----------------------------------------------------

    async fn add_participant(&self, participant: Participant) -> Result<Participant> {
        let mut t = self
            .tables
            .write()
            .map_err(|e| anyhow!("lock poisoned: {e}"))?;
        t.participants
            .insert(participant.id.clone(), participant.clone());
        Ok(participant)
    }

    async fn get_participant(&self, id: &str) -> Result<Option<Participant>> {
        let t = self
            .tables
            .read()
            .map_err(|e| anyhow!("lock poisoned: {e}"))?;
        Ok(t.participants.get(id).cloned())
    }

    async fn list_participants_by_conversation(
        &self,
        conversation_id: &str,
    ) -> Result<Vec<Participant>> {
        let t = self
            .tables
            .read()
            .map_err(|e| anyhow!("lock poisoned: {e}"))?;
        let mut out: Vec<Participant> = t
            .participants
            .values()
            .filter(|p| p.conversation_id == conversation_id)
            .cloned()
            .collect();
        out.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        Ok(out)
    }

    async fn resolve_participant_by_external_id(
        &self,
        conversation_id: &str,
        external_id: &str,
    ) -> Result<Option<Participant>> {
        let t = self
            .tables
            .read()
            .map_err(|e| anyhow!("lock poisoned: {e}"))?;
        Ok(t.participants
            .values()
            .find(|p| {
                p.conversation_id == conversation_id
                    && p.external_id.as_deref() == Some(external_id)
            })
            .cloned())
    }

    // ---- messages --------------------------------------------------------

    async fn append_message(&self, message: Message) -> Result<Message> {
        let mut t = self
            .tables
            .write()
            .map_err(|e| anyhow!("lock poisoned: {e}"))?;
        if let Some(conv_id) = &message.conversation_id {
            t.message_order
                .entry(conv_id.clone())
                .or_default()
                .push(message.id.clone());
        }
        t.messages.insert(message.id.clone(), message.clone());
        Ok(message)
    }

    async fn get_message(&self, id: &str) -> Result<Option<Message>> {
        let t = self
            .tables
            .read()
            .map_err(|e| anyhow!("lock poisoned: {e}"))?;
        Ok(t.messages.get(id).cloned())
    }

    async fn list_messages_by_conversation(&self, query: MessageQuery) -> Result<MessagePage> {
        let t = self
            .tables
            .read()
            .map_err(|e| anyhow!("lock poisoned: {e}"))?;
        let order = t
            .message_order
            .get(&query.conversation_id)
            .cloned()
            .unwrap_or_default();

        // Materialize the full ordered list (append order), then reverse for
        // descending. The cursor is the id to start *after*.
        let mut ids: Vec<String> = order;
        if query.descending {
            ids.reverse();
        }

        let start = match &query.cursor {
            Some(cursor) => ids
                .iter()
                .position(|id| id == cursor)
                .map(|i| i + 1)
                .unwrap_or(0),
            None => 0,
        };

        let slice: Vec<String> = ids.iter().skip(start).take(query.limit).cloned().collect();
        let next_cursor = if start + slice.len() < ids.len() {
            slice.last().cloned()
        } else {
            None
        };

        let messages: Vec<Message> = slice
            .iter()
            .filter_map(|id| t.messages.get(id).cloned())
            .collect();
        Ok(MessagePage {
            messages,
            next_cursor,
        })
    }

    // ---- sessions --------------------------------------------------------

    async fn create_session(&self, session: Session) -> Result<Session> {
        let mut t = self
            .tables
            .write()
            .map_err(|e| anyhow!("lock poisoned: {e}"))?;
        t.sessions
            .insert(session.session_id.clone(), session.clone());
        Ok(session)
    }

    async fn get_session(&self, session_id: &str) -> Result<Option<Session>> {
        let t = self
            .tables
            .read()
            .map_err(|e| anyhow!("lock poisoned: {e}"))?;
        Ok(t.sessions.get(session_id).cloned())
    }

    async fn update_session(&self, session_id: &str, update: SessionUpdate) -> Result<Session> {
        let mut t = self
            .tables
            .write()
            .map_err(|e| anyhow!("lock poisoned: {e}"))?;
        let session = t
            .sessions
            .get_mut(session_id)
            .ok_or_else(|| anyhow!("session '{session_id}' not found"))?;
        if let Some(status) = update.status {
            session.status = Some(status);
        }
        if let Some(token_count) = update.token_count {
            session.token_count = Some(token_count);
        }
        if let Some(message_count) = update.message_count {
            session.message_count = Some(message_count);
        }
        if update.last_activity_at.is_some() {
            session.last_activity_at = update.last_activity_at;
        }
        if update.ended_at.is_some() {
            session.ended_at = update.ended_at;
        }
        session.updated_at = Some(Utc::now());
        Ok(session.clone())
    }

    async fn list_sessions_by_conversation(&self, conversation_id: &str) -> Result<Vec<Session>> {
        let t = self
            .tables
            .read()
            .map_err(|e| anyhow!("lock poisoned: {e}"))?;
        let mut out: Vec<Session> = t
            .sessions
            .values()
            .filter(|s| s.conversation_id == conversation_id)
            .cloned()
            .collect();
        out.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        Ok(out)
    }

    // ---- engine accessors ------------------------------------------------

    fn checkpoints(&self) -> Arc<dyn CheckpointStore> {
        self.checkpoints.clone()
    }

    fn knowledge(&self) -> Arc<dyn KnowledgeBase> {
        self.knowledge.clone()
    }
}
