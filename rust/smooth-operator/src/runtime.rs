//! Minimal `AgentRuntime` that proves smooth-operator consumes smooth-operator.
//!
//! This is the seam where the smooai monorepo's LangGraph pipeline gets
//! re-expressed as a smooth-operator [`Workflow`] / [`Agent`] (see
//! `docs/ARCHITECTURE.md` §2). It does not perform real inference — it
//! constructs the engine's primitives so the wiring is compile-checked and
//! exercised by tests. Real inference arrives in roadmap Phase 3.

use std::sync::{Arc, Mutex};

use anyhow::Result;
use smooth_operator_core::llm_provider::LlmProvider;
use smooth_operator_core::{
    Agent, AgentConfig, AgentEvent, FnNode, LlmConfig, Message as EngineMessage, Role,
    ToolRegistry, Workflow, WorkflowBuilder,
};

use smooth_operator_core::KnowledgeResult;

use crate::access_control::{AccessContext, AclKnowledgeStore};
use crate::adapter::{MessageQuery, StorageAdapter};
use crate::curation::{CuratedKnowledgeStore, RetrievalFilter};
use crate::domain::{Citation, Direction, Message as DomainMessage, MessageContent};
use crate::telemetry::{
    redact_tool_arguments, AGENT_NAME, GEN_AI_AGENT_NAME, GEN_AI_CONVERSATION_ID,
    GEN_AI_REQUEST_MODEL, GEN_AI_SYSTEM, GEN_AI_TOOL_ARGUMENTS, GEN_AI_TOOL_NAME,
    GEN_AI_USAGE_INPUT_TOKENS, GEN_AI_USAGE_OUTPUT_TOKENS, OTEL_STATUS_CODE, OTEL_STATUS_MESSAGE,
    SPAN_CHAT, SPAN_TOOL, SYSTEM_NAME,
};
use crate::tools::{KnowledgeResultSink, KnowledgeSearchTool};
use tracing::Instrument;

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

/// Max citations attached to a turn's [`TurnOutcome`]. Bounds the size of the
/// `eventual_response` payload; the grounding sources past this cap are dropped
/// (most-relevant kept first).
pub const MAX_CITATIONS: usize = 8;

/// How many auto-injected knowledge results the engine prepends as
/// `[Relevant knowledge]` context. The runtime mirrors this exact query
/// (`knowledge.query(user_message, AUTO_CONTEXT_LIMIT)`) so the citations it
/// collects match the sources the engine actually grounded the first LLM call
/// with. Kept in lockstep with smooth-operator-core's `Agent` auto-injection
/// (currently a top-3 query).
const AUTO_CONTEXT_LIMIT: usize = 3;

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
    /// The sources that grounded this turn, deduplicated by id and capped at
    /// [`MAX_CITATIONS`]. Collected from the documents the turn actually
    /// retrieved — the engine's auto-injected `[Relevant knowledge]` context
    /// (mirrored by the runtime) plus every `knowledge_search` tool result.
    /// Empty when nothing was retrieved.
    pub citations: Vec<Citation>,
}

/// Extract `(input_tokens, output_tokens)` from the engine's terminal
/// [`AgentEvent::Completed`] event, if one is present and carries usage. The
/// engine reports `prompt_tokens` / `completion_tokens` on `Completed`; those
/// map directly onto the GenAI `gen_ai.usage.input_tokens` /
/// `gen_ai.usage.output_tokens` attributes. Returns `None` when there is no
/// `Completed` event (e.g. a mock turn that didn't surface usage), so the
/// caller omits the attributes rather than recording zeros.
fn usage_from_events(events: &[AgentEvent]) -> Option<(u64, u64)> {
    events.iter().find_map(|e| match e {
        AgentEvent::Completed {
            prompt_tokens,
            completion_tokens,
            ..
        } if *prompt_tokens > 0 || *completion_tokens > 0 => {
            Some((*prompt_tokens, *completion_tokens))
        }
        _ => None,
    })
}

/// The serialized JSON arguments the agent passed to the tool call identified by
/// `(iteration, tool_name)`, sourced from the matching
/// [`AgentEvent::ToolCallStart`]. Empty string when no start event carries them
/// (older runner builds default `arguments` to empty). Matching on
/// `iteration + tool_name` is sufficient for the reference runner, which runs a
/// turn's tool calls sequentially. ponytail: same-name tool twice in one
/// iteration would collide onto the first start's args — acceptable until the
/// engine surfaces a per-call id.
fn tool_arguments_for(events: &[AgentEvent], iteration: u32, tool_name: &str) -> String {
    events
        .iter()
        .find_map(|e| match e {
            AgentEvent::ToolCallStart {
                iteration: it,
                tool_name: name,
                arguments,
            } if *it == iteration && name == tool_name => Some(arguments.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

/// Build the turn's [`Citation`]s from the knowledge sources that grounded it.
///
/// `auto` is the engine's auto-injected `[Relevant knowledge]` context (mirrored
/// by the runtime), `tool` is everything the agent's `knowledge_search` calls
/// surfaced. They're concatenated auto-first, deduplicated by document id
/// (first occurrence wins — auto-context keeps its score when the same doc is
/// also tool-searched), each mapped to a [`Citation`]
/// ([`Citation::from_knowledge_result`]), and capped at [`MAX_CITATIONS`].
///
/// Returns an empty vec when nothing was retrieved.
fn collect_citations(auto: &[KnowledgeResult], tool: &[KnowledgeResult]) -> Vec<Citation> {
    let mut seen = std::collections::HashSet::new();
    auto.iter()
        .chain(tool.iter())
        .filter(|r| seen.insert(r.document_id.clone()))
        .take(MAX_CITATIONS)
        .map(Citation::from_knowledge_result)
        .collect()
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
/// This is the first end-to-end "knowledge-chat turn" for smooth-operator: it
/// wires a [`StorageAdapter`]'s [`KnowledgeBase`](smooth_operator_core::KnowledgeBase)
/// into a smooth-operator [`Agent`] two ways —
///
/// 1. **Auto-injected context** via
///    [`AgentConfig::with_knowledge`](smooth_operator_core::AgentConfig::with_knowledge):
///    the engine queries the KB with the user's message and prepends the top
///    matches as a `[Relevant knowledge]` system message before the first LLM
///    call.
/// 2. **Agent-driven search** via the [`KnowledgeSearchTool`]: the model can
///    issue its own `knowledge_search` query mid-turn with its own phrasing.
///
/// Construct with [`KnowledgeChatRuntime::new`] for production (a real
/// [`LlmClient`](smooth_operator_core::llm::LlmClient) is built from the
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
    /// Document-level access control (feature gap G3). When set, the runtime wraps
    /// the storage adapter's [`KnowledgeBase`](smooth_operator_core::KnowledgeBase) in
    /// the given [`AclKnowledgeStore`] and reads through a per-turn
    /// [`AccessContext`]-bound reader, so retrieval (both the auto-injected
    /// context and the `knowledge_search` tool) only surfaces documents the
    /// requester is entitled to. `None` ⇒ no document-level filtering (org
    /// isolation upstream is unaffected); the raw `storage.knowledge()` is used.
    access: Option<RuntimeAccessControl>,
    /// Query-time curation: document-set + metadata scoping with boost re-ranking
    /// (Phase 11). When set, the runtime reads knowledge through a
    /// [`CuratedKnowledgeStore`] reader bound to the given [`RetrievalFilter`]
    /// (and the requester's [`AccessContext`], so ACL ∧ curation both apply).
    /// `None` ⇒ no curation filter (current behavior unchanged). Takes precedence
    /// over [`access`](Self::access) when both are set, because the curated store
    /// enforces ACL itself.
    curation: Option<RuntimeCuration>,
}

/// The runtime's bound access-control state: the ACL-aware knowledge store
/// (shared, owns the ACL side table populated at ingest) plus the requester
/// identity to filter reads by.
#[derive(Clone)]
struct RuntimeAccessControl {
    store: AclKnowledgeStore,
    context: AccessContext,
}

/// The runtime's bound curation state: the curation-aware knowledge store
/// (shared, owns the curation + ACL side tables populated at ingest), the
/// requester identity (so ACL ∧ curation both apply), and the query-time filter
/// to scope reads by.
#[derive(Clone)]
struct RuntimeCuration {
    store: CuratedKnowledgeStore,
    context: AccessContext,
    filter: RetrievalFilter,
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
            access: None,
            curation: None,
        }
    }

    /// Enable query-time curation for this runtime (Phase 11): scope retrieval to
    /// named document sets / metadata equalities and re-rank by per-document
    /// boost.
    ///
    /// `store` is a [`CuratedKnowledgeStore`] wrapping the same inner
    /// [`KnowledgeBase`](smooth_operator_core::KnowledgeBase) the documents were
    /// ingested through (so its curation + ACL side tables are populated),
    /// `context` is the requester's identity (ACL ∧ curation both apply — the
    /// curated store enforces document-level access control itself), and `filter`
    /// scopes the reads. Both the auto-injected `[Relevant knowledge]` context and
    /// the `knowledge_search` tool read through this filtered, boosted reader.
    ///
    /// Pass [`RetrievalFilter::none`] to apply boost re-ranking with no
    /// set/metadata scoping. Without calling this, retrieval is unchanged.
    #[must_use]
    pub fn with_curation(
        mut self,
        store: CuratedKnowledgeStore,
        context: AccessContext,
        filter: RetrievalFilter,
    ) -> Self {
        self.curation = Some(RuntimeCuration {
            store,
            context,
            filter,
        });
        self
    }

    /// Set (or replace) just the [`RetrievalFilter`] on an already-configured
    /// curation store, so a per-turn scope can be applied without rebuilding the
    /// store. No-op (logs nothing) when curation is not configured.
    #[must_use]
    pub fn with_retrieval_filter(mut self, filter: RetrievalFilter) -> Self {
        if let Some(curation) = &mut self.curation {
            curation.filter = filter;
        }
        self
    }

    /// Enable document-level access control for this runtime (feature gap G3).
    ///
    /// `store` is an [`AclKnowledgeStore`] that wraps the same inner
    /// [`KnowledgeBase`](smooth_operator_core::KnowledgeBase) the documents were
    /// ingested through (so its ACL side table is populated), and `context` is
    /// the requester's identity. With this set, every turn reads knowledge
    /// through an [`AccessContext`]-bound reader — both the auto-injected
    /// `[Relevant knowledge]` context and the `knowledge_search` tool drop
    /// documents the requester is not entitled to.
    ///
    /// Without it, the runtime reads the raw `storage.knowledge()` exactly as
    /// before (backward-compatible — existing no-ACL knowledge stays
    /// retrievable).
    #[must_use]
    pub fn with_access_control(mut self, store: AclKnowledgeStore, context: AccessContext) -> Self {
        self.access = Some(RuntimeAccessControl { store, context });
        self
    }

    /// The knowledge handle a turn reads through: an ACL-filtering reader bound
    /// to the requester when access control is enabled, otherwise the raw
    /// storage-adapter knowledge base (unfiltered, org-scoping only).
    fn read_knowledge(&self) -> Arc<dyn smooth_operator_core::KnowledgeBase> {
        // Curation (when set) takes precedence: its reader enforces ACL ∧
        // set/metadata filter and applies boost re-ranking in one pass.
        if let Some(cur) = &self.curation {
            return cur.store.reader(cur.filter.clone(), cur.context.clone());
        }
        match &self.access {
            Some(ac) => ac.store.reader(ac.context.clone()),
            None => self.storage.knowledge(),
        }
    }

    /// Inject a custom [`LlmProvider`] (e.g. a
    /// [`MockLlmClient`](smooth_operator_core::llm_provider::MockLlmClient)) so the
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
    fn build_agent(
        &self,
        events: Arc<Mutex<Vec<AgentEvent>>>,
        prior: Vec<EngineMessage>,
        citation_sink: KnowledgeResultSink,
    ) -> Agent {
        // The knowledge handle both retrieval paths read through. When access
        // control is enabled this is an ACL-filtering reader bound to the
        // requester's `AccessContext` (feature gap G3); otherwise it's the raw
        // org-scoped knowledge base. Built once so both paths hit the SAME store
        // and the SAME ACL filter.
        let knowledge = self.read_knowledge();

        // (1) Auto-injected knowledge context: the engine queries the KB with
        //     the user's message and prepends matches before the first call.
        let config = AgentConfig::new(
            "smooth-agent-chat",
            KNOWLEDGE_CHAT_SYSTEM_PROMPT,
            self.llm.clone(),
        )
        .with_max_iterations(self.max_iterations)
        .with_knowledge(Arc::clone(&knowledge))
        // (1b) Cross-turn memory: replay the conversation's prior turns so the
        //      model sees turn 1 when answering turn 2.
        .with_prior_messages(prior);

        // (2) Agent-driven search: register the knowledge_search tool over the
        //     SAME knowledge handle, so a tool call hits the same store and the
        //     same ACL filter. The result sink lets the runtime collect the
        //     sources the agent's searches surfaced, for citations.
        let mut tools = ToolRegistry::new();
        tools.register(KnowledgeSearchTool::new(knowledge).with_result_sink(citation_sink));

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
        // --- OpenTelemetry GenAI span for the whole turn ---
        //
        // A `gen_ai.chat` span (GenAI semantic conventions) carries the system,
        // model, and conversation id up front, plus token usage recorded on
        // completion. `tracing-opentelemetry` maps these fields onto an OTLP
        // span when an exporter is installed (see `telemetry::init_telemetry`);
        // with no collector configured they're simply captured locally.
        //
        // `input_tokens` / `output_tokens` are declared as empty fields here so
        // they can be `record()`ed after the run if the engine reported usage.
        let turn_span = tracing::info_span!(
            SPAN_CHAT,
            { GEN_AI_SYSTEM } = SYSTEM_NAME,
            { GEN_AI_REQUEST_MODEL } = %self.llm.model,
            { GEN_AI_CONVERSATION_ID } = %conversation_id,
            { GEN_AI_AGENT_NAME } = AGENT_NAME,
            { GEN_AI_USAGE_INPUT_TOKENS } = tracing::field::Empty,
            { GEN_AI_USAGE_OUTPUT_TOKENS } = tracing::field::Empty,
        );

        // Run the turn body inside the span so any engine-internal spans nest
        // under it. `Instrument` keeps the span entered across awaits.
        let outcome = self
            .run_turn_inner(conversation_id, user_message)
            .instrument(turn_span.clone())
            .await?;

        // Record token usage on the turn span if the engine reported it via the
        // terminal `Completed` event (omitted otherwise, per the GenAI convs).
        if let Some((input, output)) = usage_from_events(&outcome.events) {
            turn_span.record(GEN_AI_USAGE_INPUT_TOKENS, input);
            turn_span.record(GEN_AI_USAGE_OUTPUT_TOKENS, output);
        }

        // Emit a child `gen_ai.tool` span per tool call so each invocation is an
        // independent, named, timed span in the trace. We materialize these from
        // the collected events (rather than inside the event handler) so the
        // spans hang off the turn span without restructuring the runtime. The
        // arguments come from the matching `ToolCallStart` (redacted); on failure
        // the span is marked ERROR with the tool's error text.
        for event in &outcome.events {
            if let AgentEvent::ToolCallComplete {
                iteration,
                tool_name,
                duration_ms,
                is_error,
                result,
            } = event
            {
                let arguments = tool_arguments_for(&outcome.events, *iteration, tool_name);
                let tool_span = tracing::info_span!(
                    parent: &turn_span,
                    SPAN_TOOL,
                    { GEN_AI_TOOL_NAME } = %tool_name,
                    { GEN_AI_TOOL_ARGUMENTS } = %redact_tool_arguments(&arguments),
                    { OTEL_STATUS_CODE } = tracing::field::Empty,
                    { OTEL_STATUS_MESSAGE } = tracing::field::Empty,
                    duration_ms = *duration_ms,
                    is_error = *is_error,
                );
                if *is_error {
                    tool_span.record(OTEL_STATUS_CODE, "ERROR");
                    tool_span.record(OTEL_STATUS_MESSAGE, result.as_str());
                }
                let _entered = tool_span.entered();
            }
        }

        Ok(outcome)
    }

    /// The un-instrumented turn body. Split out from [`run_turn`] so the OTel
    /// `gen_ai.chat` span wraps exactly the engine run + persistence without
    /// cluttering the instrumentation logic.
    async fn run_turn_inner(
        &self,
        conversation_id: &str,
        user_message: &str,
    ) -> Result<TurnOutcome> {
        let events = Arc::new(Mutex::new(Vec::<AgentEvent>::new()));
        // Sink the knowledge_search tool records its structured results into, so
        // we can build citations from the sources the agent's searches surfaced.
        let tool_sources: KnowledgeResultSink = Arc::new(Mutex::new(Vec::new()));

        // Mirror the engine's auto-injected `[Relevant knowledge]` query so the
        // citations include the sources the FIRST LLM call was grounded with.
        // `smooth-operator-core`'s `Agent` queries `knowledge.query(msg, 3)` and
        // prepends the matches as context (see `agent.rs`); we run the same
        // query against the same knowledge handle here. Best-effort: a KB error
        // just yields no auto-context citations (the turn still proceeds).
        let auto_sources: Vec<KnowledgeResult> = self
            .read_knowledge()
            .query(user_message, AUTO_CONTEXT_LIMIT)
            .unwrap_or_default();

        // Load the conversation's prior turns for cross-turn memory BEFORE
        // persisting the new inbound message, so `prior` is exactly the
        // history-up-to-now (the new message is replayed by `Agent::run` as the
        // current user turn, not as a duplicated prior message).
        let prior = self.load_prior_messages(conversation_id).await?;
        let agent = self.build_agent(Arc::clone(&events), prior, Arc::clone(&tool_sources));

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

        // Build citations from the sources that grounded this turn: the
        // auto-injected `[Relevant knowledge]` context first (it grounded the
        // first LLM call), then whatever the agent's `knowledge_search` calls
        // surfaced. Dedup by document id (auto-context wins ties, so its score
        // is kept) and cap.
        let tool_sources = match Arc::try_unwrap(tool_sources) {
            Ok(mutex) => mutex
                .into_inner()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            Err(arc) => arc.lock().expect("citation sink poisoned").clone(),
        };
        let citations = collect_citations(&auto_sources, &tool_sources);

        Ok(TurnOutcome {
            reply,
            events,
            citations,
        })
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
    /// uses (`smooth-operator-server/src/runner.rs`).
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
                images: vec![],
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
