//! # smooth-operator
//!
//! The reference core for smooth-operator — the service layer on top of
//! [`smooth_operator_core`] (the agent engine). It defines three things:
//!
//! - [`domain`] — storage-agnostic domain structs (Conversation, Participant,
//!   Message, Session) that mirror `spec/domain/*.json`. Checkpoints re-use the
//!   engine's [`smooth_operator_core::Checkpoint`].
//! - [`adapter`] — the single [`StorageAdapter`] seam every backend implements
//!   (see `docs/STORAGE.md`). Its checkpoint/knowledge accessors return
//!   smooth-operator-core's own traits so the engine plugs straight in.
//! - [`runtime`] — a minimal [`AgentRuntime`] that constructs a real
//!   smooth-operator [`Agent`](smooth_operator_core::Agent) and
//!   [`Workflow`](smooth_operator_core::Workflow), proving consumption.
//!
//! It also owns two shared retrieval seams both backends/consumers depend on:
//! [`embedding`] (the text→vector [`Embedder`] + the network-free
//! [`DeterministicEmbedder`], the one home for both the Postgres adapter and the
//! ingestion pipeline) and [`rerank`] (the optional post-retrieval [`Reranker`]
//! stage — feature gap G8).

pub mod access_control;
pub mod adapter;
pub mod agent_config;
pub mod auth;
pub mod backplane;
pub mod connector_config;
pub mod curation;
pub mod domain;
pub mod embedding;
pub mod gateway_key;
pub mod identity_intake;
pub mod otp;
pub mod rerank;
pub mod runtime;
pub mod settings;
pub mod telemetry;
pub mod tool_provider;
pub mod tools;
pub mod widget_auth;

pub use access_control::{AccessContext, AclKnowledgeStore, DocAcl};
pub use adapter::{ConversationUpdate, MessagePage, MessageQuery, SessionUpdate, StorageAdapter};
pub use agent_config::{
    advance_after_verdict, judge_user_prompt, next_step, render_workflow_prompt_section,
    resolve_current_step, tool_auth_refusal, AgentBehaviorConfig, AgentConfigResolver,
    AuthGateHook, AuthLevel, ConversationWorkflow, ConversationWorkflowStep, EnabledTool,
    StaticAgentConfigResolver, Visibility, WorkflowJudgeVerdict, JUDGE_SYSTEM_PROMPT,
};
pub use auth::{
    AuthConfig, AuthError, AuthVerifier, JwtVerifier, LocalTokenVerifier, NoAuthVerifier,
    Principal, Role, SmooIdentityVerifier,
};
pub use connector_config::{
    ConnectorConfig, ConnectorConfigStore, ConnectorKind, InMemoryConnectorConfigStore,
};
pub use curation::{
    with_boost, with_document_set, CuratedKnowledgeStore, DocMeta, RetrievalFilter, DEFAULT_BOOST,
};
pub use domain::{
    Checkpoint, Citation, ContentItem, Conversation, Direction, Message, MessageContent,
    Participant, ParticipantRef, ParticipantType, Platform, Session, SessionStatus,
    CITATION_SNIPPET_MAX_CHARS,
};
pub use embedding::{
    cosine_similarity, DeterministicEmbedder, Embedder, InputType, DEFAULT_EMBEDDING_DIM,
};
pub use gateway_key::{resolve_gateway_key, EnvGatewayKeyResolver, GatewayKeyResolver};
pub use identity_intake::{
    normalize_email, normalize_phone_e164, validate_intake, IntakeField, IntakeFieldError,
    IntakeFieldKey, IntakeOutcome, IntakeRequest, IntakeValues,
};
pub use otp::{OtpChannel, OtpContact, OtpDelivery, OtpError, OtpService, OtpVerifyOutcome};
pub use rerank::{apply_optional_rerank, LexicalReranker, NoopReranker, Reranker};
pub use runtime::{
    AgentRuntime, KnowledgeChatRuntime, SharedRuntime, TurnOutcome, TurnState, MAX_CITATIONS,
};
pub use settings::{
    AgentSettings, InMemorySettingsStore, SettingsStore, DEFAULT_MODEL, DEFAULT_SYSTEM_PROMPT,
};
pub use telemetry::init_telemetry;
pub use tool_provider::{ToolProvider, ToolProviderContext};
pub use tools::{
    builtin_tools, intake_channel, ConversationHistoryTool, FetchUrlTool, IdentityAttach,
    IntakeChannelPair, KnowledgeResultSink, KnowledgeSearchTool, NoopWebSearchProvider,
    RequestIdentityIntakeTool, SearchResult, SubmitIdentityIntakeTool, ToolContext,
    WebSearchProvider, WebSearchTool, REQUEST_IDENTITY_INTAKE_TOOL, SUBMIT_IDENTITY_INTAKE_TOOL,
};

// Re-export the engine so adapter crates and consumers depend on one version.
pub use smooth_operator_core;
