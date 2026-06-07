//! # smooth-operator-agent-core
//!
//! The reference core for smooth-operator-agent — the service layer on top of
//! [`smooth_operator`] (the agent engine). It defines three things:
//!
//! - [`domain`] — storage-agnostic domain structs (Conversation, Participant,
//!   Message, Session) that mirror `spec/domain/*.json`. Checkpoints re-use the
//!   engine's [`smooth_operator::Checkpoint`].
//! - [`adapter`] — the single [`StorageAdapter`] seam every backend implements
//!   (see `docs/STORAGE.md`). Its checkpoint/knowledge accessors return
//!   smooth-operator's own traits so the engine plugs straight in.
//! - [`runtime`] — a minimal [`AgentRuntime`] that constructs a real
//!   smooth-operator [`Agent`](smooth_operator::Agent) and
//!   [`Workflow`](smooth_operator::Workflow), proving consumption.

pub mod adapter;
pub mod domain;
pub mod runtime;
pub mod telemetry;
pub mod tools;

pub use adapter::{ConversationUpdate, MessagePage, MessageQuery, SessionUpdate, StorageAdapter};
pub use domain::{
    Checkpoint, ContentItem, Conversation, Direction, Message, MessageContent, Participant,
    ParticipantRef, ParticipantType, Platform, Session, SessionStatus,
};
pub use runtime::{AgentRuntime, KnowledgeChatRuntime, SharedRuntime, TurnOutcome, TurnState};
pub use telemetry::init_telemetry;
pub use tools::{
    builtin_tools, ConversationHistoryTool, FetchUrlTool, KnowledgeSearchTool,
    NoopWebSearchProvider, SearchResult, ToolContext, WebSearchProvider, WebSearchTool,
};

// Re-export the engine so adapter crates and consumers depend on one version.
pub use smooth_operator;
