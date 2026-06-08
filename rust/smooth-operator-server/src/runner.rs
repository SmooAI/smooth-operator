//! The streaming, memory-carrying agent runner used by the WS service.
//!
//! `smooth-operator`'s [`KnowledgeChatRuntime`] proves the engine ↔ gateway
//! path but (a) is non-streaming (`run_turn` returns only after the turn
//! completes) and (b) has no cross-turn memory — each `run_turn` builds a fresh
//! [`Agent`] with a random id and no prior messages, so turn 2 forgets turn 1
//! (documented in `core/tests/e2e_llm_smoo_ai.rs`).
//!
//! The service needs both, so this module builds the agent itself, wiring the
//! same knowledge-grounding as core PLUS:
//!
//! 1. **Streaming** via [`Agent::run_with_channel`], translating the engine's
//!    [`AgentEvent`] stream into protocol events (`stream_token`,
//!    `stream_chunk`, `eventual_response`).
//! 2. **Per-session memory** via [`AgentConfig::with_prior_messages`]: before
//!    each turn the session's persisted message log is loaded from the storage
//!    adapter and replayed into the conversation, so the model sees turn 1 when
//!    answering turn 2. (`Agent::new` randomizes the agent id every time, so the
//!    checkpoint-resume path can't be keyed stably — replaying the persisted log
//!    is the robust, backend-agnostic way to carry memory. The log is the source
//!    of truth the adapter already persists.)

use std::sync::{Arc, Mutex};

use anyhow::Result;
use serde_json::json;
use smooth_operator_core::{
    Agent, AgentConfig, AgentEvent, KnowledgeResult, LlmConfig, Message as EngineMessage, Role,
    ToolRegistry,
};
use tokio::sync::mpsc::UnboundedSender;

use smooth_operator::adapter::{MessageQuery, StorageAdapter};
use smooth_operator::domain::{Citation, Direction, Message as DomainMessage, MessageContent};
use smooth_operator::tools::{KnowledgeResultSink, KnowledgeSearchTool};
use smooth_operator::MAX_CITATIONS;

/// How many auto-injected knowledge results the engine prepends as
/// `[Relevant knowledge]` context. Mirrors smooth-operator-core's `Agent`
/// auto-injection (a top-3 query) so the citations we collect match the sources
/// that grounded the first LLM call.
const AUTO_CONTEXT_LIMIT: usize = 3;

/// System prompt for the knowledge-chat agent. Mirrors core's prompt: ground
/// answers in the knowledge base and search it before answering anything
/// organization-specific.
const KNOWLEDGE_CHAT_SYSTEM_PROMPT: &str =
    "You are a helpful customer-support agent for the organization. \
    Answer the user's question accurately and concisely. When a question depends on \
    organization-specific facts (policies, products, documentation), call the \
    `knowledge_search` tool to retrieve them before answering, and ground your answer \
    in what you retrieve. If the knowledge base has no relevant information, say so. \
    Remember facts the user tells you within the conversation and use them when asked.";

/// Max prior turns to replay into the conversation for memory. Bounds context
/// growth on long sessions; the in-memory log is small but a real backend could
/// be large.
const MAX_PRIOR_MESSAGES: usize = 50;

/// The terminal outcome of a streamed turn.
pub struct TurnResult {
    /// The agent's final natural-language reply.
    pub reply: String,
    /// The id of the persisted outbound (agent) message, for `eventual_response`.
    pub message_id: String,
    /// True if any `knowledge_search` tool call ran this turn (diagnostics).
    pub invoked_knowledge_search: bool,
    /// The sources that grounded this turn (the auto-injected context + every
    /// `knowledge_search` result), deduped by id and capped. Carried onto the
    /// `eventual_response`'s `citations`. Empty when nothing was retrieved.
    pub citations: Vec<Citation>,
}

/// Runs one knowledge-grounded, streaming turn for a session's conversation and
/// emits protocol-shaped events through `sink` as they happen.
///
/// `sink` receives ready-to-send `serde_json::Value` event envelopes (built by
/// [`crate::protocol`]). The caller forwards them over the WebSocket.
///
/// # Errors
/// Returns an error if message persistence or the agent loop fails fatally. The
/// caller converts this into a protocol `error` event.
pub async fn run_streaming_turn(
    storage: Arc<dyn StorageAdapter>,
    llm: LlmConfig,
    max_iterations: u32,
    conversation_id: &str,
    request_id: &str,
    user_message: &str,
    sink: &UnboundedSender<serde_json::Value>,
) -> Result<TurnResult> {
    // 0. Mirror the engine's auto-injected `[Relevant knowledge]` query so the
    //    citations include the sources the FIRST LLM call was grounded with.
    //    Same query smooth-operator-core's `Agent` runs (`query(msg, 3)`),
    //    against the same knowledge handle. Best-effort: a KB error yields no
    //    auto-context citations.
    let auto_sources: Vec<KnowledgeResult> = storage
        .knowledge()
        .query(user_message, AUTO_CONTEXT_LIMIT)
        .unwrap_or_default();
    // Sink the knowledge_search tool records its structured results into, for
    // citations built from the sources the agent's searches surfaced.
    let tool_sources: KnowledgeResultSink = Arc::new(Mutex::new(Vec::new()));

    // 1. Load prior turns for memory BEFORE persisting the new inbound message,
    //    so prior_messages is exactly the history-up-to-now.
    let prior = load_prior_messages(storage.as_ref(), conversation_id).await?;

    // 2. Persist the inbound user message.
    persist_message(
        storage.as_ref(),
        conversation_id,
        Direction::Inbound,
        user_message,
    )
    .await?;

    // 3. Build the agent: knowledge-grounded config + knowledge_search tool +
    //    replayed prior messages for memory.
    let config = AgentConfig::new("smooth-agent-chat", KNOWLEDGE_CHAT_SYSTEM_PROMPT, llm)
        .with_max_iterations(max_iterations)
        .with_knowledge(storage.knowledge())
        .with_prior_messages(prior);

    let mut tools = ToolRegistry::new();
    tools.register(
        KnowledgeSearchTool::new(storage.knowledge()).with_result_sink(Arc::clone(&tool_sources)),
    );

    let agent = Agent::new(config, tools).with_checkpoint_store(storage.checkpoints());

    // 4. Run with the streaming channel and translate events as they arrive.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
    let request_id_owned = request_id.to_string();
    let sink_clone = sink.clone();

    // Spawn the event translator so we forward tokens to the client in real
    // time while the agent loop runs concurrently.
    let translator = tokio::spawn(async move {
        let mut invoked_knowledge_search = false;
        while let Some(event) = rx.recv().await {
            match event {
                AgentEvent::TokenDelta { content } => {
                    if !content.is_empty() {
                        let _ = sink_clone
                            .send(crate::protocol::stream_token(&request_id_owned, &content));
                    }
                }
                AgentEvent::ToolCallStart {
                    tool_name,
                    arguments,
                    ..
                } => {
                    if tool_name == "knowledge_search" {
                        invoked_knowledge_search = true;
                    }
                    let _ = sink_clone.send(crate::protocol::stream_chunk(
                        &request_id_owned,
                        &tool_name,
                        json!({
                            "rawResponse": json!({ "toolCall": { "name": tool_name, "arguments": arguments } }),
                        }),
                    ));
                }
                AgentEvent::ToolCallComplete {
                    tool_name,
                    result,
                    is_error,
                    ..
                } => {
                    let _ = sink_clone.send(crate::protocol::stream_chunk(
                        &request_id_owned,
                        &tool_name,
                        json!({
                            "rawResponse": json!({
                                "toolResult": { "name": tool_name, "isError": is_error, "result": result }
                            }),
                        }),
                    ));
                }
                AgentEvent::PhaseStart { phase, .. } => {
                    let _ = sink_clone.send(crate::protocol::stream_chunk(
                        &request_id_owned,
                        &phase,
                        json!({}),
                    ));
                }
                // Started / Completed / token-accounting events are terminal or
                // structural; the protocol carries those via immediate/eventual
                // responses, so they're intentionally not re-emitted here.
                _ => {}
            }
        }
        invoked_knowledge_search
    });

    // Drive the agent loop. `run_with_channel` consumes `tx`; when it returns,
    // the channel closes and the translator task drains and finishes.
    let conversation = agent.run_with_channel(user_message, tx).await?;

    let invoked_knowledge_search = translator.await.unwrap_or(false);

    let reply = conversation
        .last_assistant_content()
        .unwrap_or_default()
        .to_string();

    // 5. Persist the outbound reply and capture its id for eventual_response.
    let message_id = if reply.is_empty() {
        uuid::Uuid::new_v4().to_string()
    } else {
        persist_message(
            storage.as_ref(),
            conversation_id,
            Direction::Outbound,
            &reply,
        )
        .await?
        .id
    };

    // Build citations from the sources that grounded this turn: auto-injected
    // context first (it grounded the first LLM call), then the agent's
    // knowledge_search results. Dedup by document id, cap at MAX_CITATIONS.
    let tool_sources = match Arc::try_unwrap(tool_sources) {
        Ok(mutex) => mutex
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        Err(arc) => arc.lock().expect("citation sink poisoned").clone(),
    };
    let citations = collect_citations(&auto_sources, &tool_sources);

    Ok(TurnResult {
        reply,
        message_id,
        invoked_knowledge_search,
        citations,
    })
}

/// Build the turn's [`Citation`]s from the knowledge sources that grounded it:
/// the engine's auto-injected `[Relevant knowledge]` context (`auto`, mirrored
/// by the runner) followed by everything the agent's `knowledge_search` calls
/// surfaced (`tool`). Concatenated auto-first, deduplicated by document id
/// (first occurrence wins), mapped to [`Citation`], and capped at
/// [`MAX_CITATIONS`]. Empty when nothing was retrieved.
fn collect_citations(auto: &[KnowledgeResult], tool: &[KnowledgeResult]) -> Vec<Citation> {
    let mut seen = std::collections::HashSet::new();
    auto.iter()
        .chain(tool.iter())
        .filter(|r| seen.insert(r.document_id.clone()))
        .take(MAX_CITATIONS)
        .map(Citation::from_knowledge_result)
        .collect()
}

/// Load the conversation's persisted messages (oldest-first, capped) and convert
/// them to engine `Message`s for replay: inbound → User, outbound → Assistant.
async fn load_prior_messages(
    storage: &dyn StorageAdapter,
    conversation_id: &str,
) -> Result<Vec<EngineMessage>> {
    let page = storage
        .list_messages_by_conversation(MessageQuery::new(conversation_id, MAX_PRIOR_MESSAGES))
        .await?;

    let mut out = Vec::with_capacity(page.messages.len());
    for m in page.messages {
        let text = m
            .content
            .text
            .clone()
            .or_else(|| m.content.items.iter().find_map(|it| it.text.clone()))
            .unwrap_or_default();
        if text.is_empty() {
            continue;
        }
        let role = match m.direction {
            Direction::Inbound => Role::User,
            Direction::Outbound => Role::Assistant,
        };
        out.push(EngineMessage {
            id: m.id,
            role,
            content: text,
            tool_call_id: None,
            tool_name: None,
            tool_calls: vec![],
            reasoning_content: None,
            timestamp: m.created_at,
        });
    }
    Ok(out)
}

/// Append a single message to the conversation's log via the adapter.
async fn persist_message(
    storage: &dyn StorageAdapter,
    conversation_id: &str,
    direction: Direction,
    text: &str,
) -> Result<DomainMessage> {
    let now = chrono::Utc::now();
    let message = DomainMessage {
        id: uuid::Uuid::new_v4().to_string(),
        external_id: None,
        organization_id: None,
        conversation_id: Some(conversation_id.to_string()),
        direction,
        content: MessageContent::from_text(text),
        from: None,
        to: None,
        metadata_json: None,
        analytics_json: None,
        created_at: now,
        updated_at: None,
    };
    storage.append_message(message).await
}

/// Build the structured `GeneralAgentResponse`-shaped payload the protocol's
/// `eventual_response` carries. The reference runtime doesn't produce the full
/// structured analytics, so we surface the reply text in `responseParts` and
/// supply neutral defaults for the analytic fields (clients render
/// `responseParts`).
#[must_use]
pub fn general_agent_response(reply: &str) -> serde_json::Value {
    json!({
        "responseParts": [reply],
        "customerHappinessScore": 0.5,
        "needsSatisfactionScore": 0.5,
        "requestSummary": "",
        "resolutionStatus": "in_progress",
        "suggestedNextActions": [],
    })
}
