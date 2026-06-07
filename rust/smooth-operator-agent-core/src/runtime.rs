//! Minimal `AgentRuntime` that proves smooth-operator-agent consumes smooth-operator.
//!
//! This is the seam where the smooai monorepo's LangGraph pipeline gets
//! re-expressed as a smooth-operator [`Workflow`] / [`Agent`] (see
//! `docs/ARCHITECTURE.md` §2). It does not perform real inference — it
//! constructs the engine's primitives so the wiring is compile-checked and
//! exercised by tests. Real inference arrives in roadmap Phase 3.

use std::sync::{Arc, Mutex};

use anyhow::Result;
use smooth_operator::llm_provider::LlmProvider;
use smooth_operator::{
    Agent, AgentConfig, AgentEvent, FnNode, LlmConfig, Message as EngineMessage, Role,
    ToolRegistry, Workflow, WorkflowBuilder,
};

use crate::adapter::{MessageQuery, StorageAdapter};
use crate::domain::{Direction, Message as DomainMessage, MessageContent};
use crate::tools::KnowledgeSearchTool;

/// State threaded through the reference workflow: the user's message in, the
/// agent's reply out. Mirrors (in miniature) the LangGraph `StateGraph` state.
#[derive(Debug, Clone, Default)]
pub struct TurnState {
    pub user_message: String,
    pub reply: Option<String>,
}

/// A minimal runtime that owns a constructed smooth-operator [`Agent`] and a
/// trivial single-node [`Workflow`]. Both are real engine objects.
pub struct AgentRuntime {
    agent: Agent,
    workflow: Workflow<TurnState>,
}

impl AgentRuntime {
    /// Construct the runtime from an [`LlmConfig`] and a [`ToolRegistry`].
    ///
    /// This is the load-bearing proof of consumption: it builds an
    /// `AgentConfig` + `Agent` from the engine, and compiles a one-`FnNode`
    /// `Workflow` whose node echoes the user message back as the reply.
    ///
    /// # Errors
    /// Returns an error if the workflow fails to build (misconfigured graph).
    pub fn new(name: impl Into<String>, llm: LlmConfig, tools: ToolRegistry) -> Result<Self> {
        let name = name.into();

        // --- construct a real smooth-operator Agent ---
        let config = AgentConfig::new(&name, "You are a smooth-agent reference runtime.", llm)
            .with_max_iterations(8);
        let agent = Agent::new(config, tools);

        // --- construct a real smooth-operator Workflow with one FnNode ---
        let respond = FnNode::new("respond", |mut state: TurnState| {
            Box::pin(async move {
                state.reply = Some(format!("ack: {}", state.user_message));
                Ok(state)
            })
        });
        let workflow = WorkflowBuilder::new()
            .add_node(respond)
            .set_entry("respond")
            .set_end("respond")
            .build()?;

        Ok(Self { agent, workflow })
    }

    /// Construct a runtime and wire the storage adapter's checkpoint store +
    /// knowledge base into the engine, demonstrating the `StorageAdapter`
    /// accessors plug straight into smooth-operator.
    ///
    /// # Errors
    /// Returns an error if the workflow fails to build.
    pub fn with_storage(
        name: impl Into<String>,
        llm: LlmConfig,
        tools: ToolRegistry,
        storage: &dyn StorageAdapter,
    ) -> Result<Self> {
        let name = name.into();

        let config = AgentConfig::new(&name, "You are a smooth-agent reference runtime.", llm)
            .with_max_iterations(8)
            // KnowledgeBase from the adapter plugs straight into AgentConfig.
            .with_knowledge(storage.knowledge());

        // CheckpointStore from the adapter plugs straight into the Agent.
        let agent = Agent::new(config, tools).with_checkpoint_store(storage.checkpoints());

        let respond = FnNode::new("respond", |mut state: TurnState| {
            Box::pin(async move {
                state.reply = Some(format!("ack: {}", state.user_message));
                Ok(state)
            })
        });
        let workflow = WorkflowBuilder::new()
            .add_node(respond)
            .set_entry("respond")
            .set_end("respond")
            .build()?;

        Ok(Self { agent, workflow })
    }

    /// The engine-generated agent id (proves the `Agent` was constructed).
    pub fn agent_id(&self) -> &str {
        &self.agent.id
    }

    /// Run one turn through the smooth-operator workflow. Returns the reply
    /// produced by the node. (No LLM call — the node is deterministic.)
    ///
    /// # Errors
    /// Returns an error if the workflow run fails.
    pub async fn run(&self, message: impl Into<String>) -> Result<String> {
        let state = TurnState {
            user_message: message.into(),
            reply: None,
        };
        let out = self.workflow.run(state).await?;
        Ok(out.reply.unwrap_or_default())
    }

    /// Borrow the underlying engine agent (e.g. to attach an event handler).
    pub fn agent(&self) -> &Agent {
        &self.agent
    }
}

/// Convenience: an `Arc`-wrapped runtime.
pub type SharedRuntime = Arc<AgentRuntime>;

/// The system prompt the knowledge-chat agent runs with. Keeps the agent
/// grounded: answer from the knowledge base, and search it before answering
/// anything organization-specific.
const KNOWLEDGE_CHAT_SYSTEM_PROMPT: &str =
    "You are a helpful customer-support agent for the organization. \
    Answer the user's question accurately and concisely. When a question depends on \
    organization-specific facts (policies, products, documentation), call the \
    `knowledge_search` tool to retrieve them before answering, and ground your answer \
    in what you retrieve. If the knowledge base has no relevant information, say so. \
    Remember facts the user tells you within the conversation and use them when asked.";

/// Max prior turns to replay into the conversation for cross-turn memory.
/// Bounds context growth on long sessions; the in-memory log is small, but a
/// real backend (Postgres/DynamoDB) could be large.
const MAX_PRIOR_MESSAGES: usize = 50;

/// The outcome of running one knowledge-grounded turn through the agent.
///
/// Carries the final assistant text plus every [`AgentEvent`] the engine
/// emitted during the run — so callers (and tests) can inspect exactly what
/// happened: which tools ran, what they returned, how many iterations.
#[derive(Debug, Clone)]
pub struct TurnOutcome {
    /// The agent's final natural-language reply (the last assistant turn with
    /// no pending tool calls). Empty string if the agent produced no text.
    pub reply: String,
    /// Every event the engine emitted, in order. Inspect for
    /// [`AgentEvent::ToolCallStart`] / [`AgentEvent::ToolCallComplete`] to
    /// prove a knowledge search happened.
    pub events: Vec<AgentEvent>,
}

impl TurnOutcome {
    /// `true` if the agent invoked a tool named `tool_name` during the turn.
    #[must_use]
    pub fn invoked_tool(&self, tool_name: &str) -> bool {
        self.events.iter().any(|e| {
            matches!(
                e,
                AgentEvent::ToolCallStart { tool_name: name, .. } if name == tool_name
            )
        })
    }

    /// The completed result text of the first call to `tool_name`, if any.
    /// Sourced from [`AgentEvent::ToolCallComplete`] (truncated to ~500 chars
    /// by the engine), so a test can assert the tool returned the seeded doc.
    #[must_use]
    pub fn tool_result(&self, tool_name: &str) -> Option<&str> {
        self.events.iter().find_map(|e| match e {
            AgentEvent::ToolCallComplete {
                tool_name: name,
                result,
                ..
            } if name == tool_name => Some(result.as_str()),
            _ => None,
        })
    }
}

/// A real, knowledge-grounded chat runtime over smooth-operator.
///
/// This is the first end-to-end "knowledge-chat turn" for smooth-operator-agent: it
/// wires a [`StorageAdapter`]'s [`KnowledgeBase`](smooth_operator::KnowledgeBase)
/// into a smooth-operator [`Agent`] two ways —
///
/// 1. **Auto-injected context** via
///    [`AgentConfig::with_knowledge`](smooth_operator::AgentConfig::with_knowledge):
///    the engine queries the KB with the user's message and prepends the top
///    matches as a `[Relevant knowledge]` system message before the first LLM
///    call.
/// 2. **Agent-driven search** via the [`KnowledgeSearchTool`]: the model can
///    issue its own `knowledge_search` query mid-turn with its own phrasing.
///
/// Construct with [`KnowledgeChatRuntime::new`] for production (a real
/// [`LlmClient`](smooth_operator::llm::LlmClient) is built from the
/// [`LlmConfig`]), or inject a mock via
/// [`KnowledgeChatRuntime::with_llm_provider`] for deterministic, key-free
/// tests.
pub struct KnowledgeChatRuntime {
    storage: Arc<dyn StorageAdapter>,
    llm: LlmConfig,
    /// Optional test-injected LLM surface. When set, every `run_turn` builds
    /// its `Agent` with this provider instead of a live client.
    llm_provider: Option<Arc<dyn LlmProvider>>,
    max_iterations: u32,
}

impl KnowledgeChatRuntime {
    /// Build a production runtime over a storage adapter and LLM config.
    #[must_use]
    pub fn new(storage: Arc<dyn StorageAdapter>, llm: LlmConfig) -> Self {
        Self {
            storage,
            llm,
            llm_provider: None,
            max_iterations: 8,
        }
    }

    /// Inject a custom [`LlmProvider`] (e.g. a
    /// [`MockLlmClient`](smooth_operator::llm_provider::MockLlmClient)) so the
    /// agent loop runs deterministically with no network / API key. This is
    /// the test seam.
    #[must_use]
    pub fn with_llm_provider(mut self, provider: Arc<dyn LlmProvider>) -> Self {
        self.llm_provider = Some(provider);
        self
    }

    /// Cap on agent loop iterations (LLM call → tool calls → LLM call → …).
    /// Defaults to 8.
    #[must_use]
    pub fn with_max_iterations(mut self, max: u32) -> Self {
        self.max_iterations = max;
        self
    }

    /// Build the `Agent` for a turn: knowledge-grounded config + the
    /// `knowledge_search` tool + the conversation's prior turns replayed for
    /// cross-turn memory, with the mock LLM provider attached when one was
    /// injected for tests.
    ///
    /// `prior` is the conversation's persisted message log (oldest-first),
    /// already converted to engine messages. Replaying it via
    /// [`AgentConfig::with_prior_messages`] is what gives turn 2 memory of
    /// turn 1: `Agent::new` randomizes the agent id every turn, so the
    /// checkpoint-resume path can't be keyed stably — replaying the persisted
    /// log is the robust, backend-agnostic way to carry memory.
    fn build_agent(&self, events: Arc<Mutex<Vec<AgentEvent>>>, prior: Vec<EngineMessage>) -> Agent {
        // (1) Auto-injected knowledge context: the engine queries the KB with
        //     the user's message and prepends matches before the first call.
        let config = AgentConfig::new(
            "smooth-agent-chat",
            KNOWLEDGE_CHAT_SYSTEM_PROMPT,
            self.llm.clone(),
        )
        .with_max_iterations(self.max_iterations)
        .with_knowledge(self.storage.knowledge())
        // (1b) Cross-turn memory: replay the conversation's prior turns so the
        //      model sees turn 1 when answering turn 2.
        .with_prior_messages(prior);

        // (2) Agent-driven search: register the knowledge_search tool over the
        //     SAME knowledge handle, so a tool call hits the same store.
        let mut tools = ToolRegistry::new();
        tools.register(KnowledgeSearchTool::new(self.storage.knowledge()));

        let agent = Agent::new(config, tools)
            .with_checkpoint_store(self.storage.checkpoints())
            .with_event_handler(move |event| {
                events.lock().expect("event sink poisoned").push(event);
            });

        match &self.llm_provider {
            Some(provider) => agent.with_llm_provider(Arc::clone(provider)),
            None => agent,
        }
    }

    /// Run one knowledge-grounded turn.
    ///
    /// Drives the smooth-operator agent loop to completion, then returns the
    /// final assistant text plus every [`AgentEvent`] emitted. The inbound
    /// user message and the outbound reply are also persisted to the storage
    /// adapter's message log under `conversation_id` (best-effort: a persist
    /// failure surfaces as an error so callers don't silently lose history).
    ///
    /// # Errors
    /// Returns an error if the agent loop fails fatally or message persistence
    /// fails.
    pub async fn run_turn(&self, conversation_id: &str, user_message: &str) -> Result<TurnOutcome> {
        let events = Arc::new(Mutex::new(Vec::<AgentEvent>::new()));

        // Load the conversation's prior turns for cross-turn memory BEFORE
        // persisting the new inbound message, so `prior` is exactly the
        // history-up-to-now (the new message is replayed by `Agent::run` as the
        // current user turn, not as a duplicated prior message).
        let prior = self.load_prior_messages(conversation_id).await?;
        let agent = self.build_agent(Arc::clone(&events), prior);

        // Persist the inbound user message.
        self.persist_message(conversation_id, Direction::Inbound, user_message)
            .await?;

        // Run the engine loop — this is where retrieval + tool calls happen.
        let conversation = agent.run(user_message).await?;

        let reply = conversation
            .last_assistant_content()
            .unwrap_or_default()
            .to_string();

        // Persist the outbound reply.
        if !reply.is_empty() {
            self.persist_message(conversation_id, Direction::Outbound, &reply)
                .await?;
        }

        // Drop the agent so its event-handler closure releases the `events`
        // Arc clone — then we hold the sole reference and can move the vec out.
        drop(agent);

        // The agent dropped its handler clone when `agent` went out of scope,
        // so we hold the only reference — but fall back to a clone if not.
        let events = match Arc::try_unwrap(events) {
            Ok(mutex) => mutex
                .into_inner()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            Err(arc) => arc.lock().expect("event sink poisoned").clone(),
        };

        Ok(TurnOutcome { reply, events })
    }

    /// Append a single message to the conversation's log via the adapter.
    async fn persist_message(
        &self,
        conversation_id: &str,
        direction: Direction,
        text: &str,
    ) -> Result<()> {
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
        self.storage.append_message(message).await?;
        Ok(())
    }

    /// Load the conversation's persisted messages (oldest-first, capped at
    /// [`MAX_PRIOR_MESSAGES`]) and convert them to engine [`EngineMessage`]s for
    /// replay: inbound → [`Role::User`], outbound → [`Role::Assistant`]. Empty
    /// messages are skipped. This is the same approach the WS service runner
    /// uses (`smooth-operator-agent-server/src/runner.rs`).
    async fn load_prior_messages(&self, conversation_id: &str) -> Result<Vec<EngineMessage>> {
        let page = self
            .storage
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_llm() -> LlmConfig {
        LlmConfig::openrouter("test-key").with_model("openai/gpt-4o")
    }

    #[tokio::test]
    async fn runtime_constructs_agent_and_runs_workflow() {
        let rt =
            AgentRuntime::new("ref-agent", test_llm(), ToolRegistry::new()).expect("build runtime");
        // The Agent was really constructed — it has an engine-assigned id.
        assert!(!rt.agent_id().is_empty());
        // The Workflow really ran through its FnNode.
        let reply = rt.run("hello world").await.expect("run");
        assert_eq!(reply, "ack: hello world");
    }
}
