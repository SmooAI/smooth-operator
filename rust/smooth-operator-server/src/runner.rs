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
use std::time::Duration;

use anyhow::Result;
use serde_json::json;
use smooth_operator_core::llm_provider::LlmProvider;
use smooth_operator_core::{
    human_channel, Agent, AgentConfig, AgentEvent, ConfirmationHook, HumanRequest, HumanResponse,
    KnowledgeBase, KnowledgeResult, LlmConfig, Message as EngineMessage, Role, ToolRegistry,
};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use smooth_operator::access_control::AccessContext;
use smooth_operator::adapter::{MessageQuery, StorageAdapter};
use smooth_operator::agent_config::{
    advance_after_verdict, judge_user_prompt, render_workflow_prompt_section, resolve_current_step,
    AuthGateHook, ConversationWorkflow, WorkflowJudgeVerdict, JUDGE_SYSTEM_PROMPT,
};
use smooth_operator::domain::{Citation, Direction, Message as DomainMessage, MessageContent};
use smooth_operator::interaction::{InteractionOutcome, InteractionRegistry, InteractionRequest};
use smooth_operator::rerank::Reranker;
use smooth_operator::telemetry::{
    redact_tool_arguments, AGENT_NAME, GEN_AI_AGENT_NAME, GEN_AI_CONVERSATION_ID,
    GEN_AI_REQUEST_MODEL, GEN_AI_SYSTEM, GEN_AI_TOOL_ARGUMENTS, GEN_AI_TOOL_NAME,
    GEN_AI_USAGE_INPUT_TOKENS, GEN_AI_USAGE_OUTPUT_TOKENS, OTEL_STATUS_CODE, OTEL_STATUS_MESSAGE,
    SMOOAI_ORG_ID, SPAN_CHAT, SPAN_TOOL, SYSTEM_NAME,
};
use smooth_operator::tool_provider::{ToolProvider, ToolProviderContext};
use smooth_operator::tools::{
    interaction_channel, InteractionAttach, KnowledgeResultSink, KnowledgeSearchTool,
    RequestInteractionTool, SubmitInteractionTool,
};
use smooth_operator::MAX_CITATIONS;
use tracing::Instrument;

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

/// How long a parked write-tool confirmation waits for a `confirm_tool_action`
/// before the core `ConfirmationHook` gives up and treats the tool as denied
/// (a timeout). Bounds a stuck turn so a client that never confirms can't pin a
/// task forever. Generous (5 min) because a human is in the loop.
const CONFIRMATION_TIMEOUT: Duration = Duration::from_secs(300);

/// Registers a parked turn's [`HumanResponse`] sender under a session id (so a
/// later `confirm_tool_action` can take it). Typically `AppState::register_confirmation`.
pub type RegisterConfirmation = Arc<dyn Fn(&str, UnboundedSender<HumanResponse>) + Send + Sync>;

/// Clears any registered confirmation sender for a session id when its turn ends.
/// Typically `AppState::clear_confirmation`.
pub type ClearConfirmation = Arc<dyn Fn(&str) + Send + Sync>;

/// Hooks the runner needs to wire **write-confirmation HITL** into a turn
/// without depending on `AppState` directly (keeps the runner unit-testable).
///
/// When `Some`, the runner installs a core [`ConfirmationHook`] over every tool
/// whose name matches one of [`tool_patterns`](Self::tool_patterns). When such a
/// tool is about to run, the agent loop **parks** inside the hook's `pre_call`
/// and emits a [`HumanRequest::Confirm`]; the runner's bridge:
///   1. calls [`register`](Self::register) with the session's
///      [`HumanResponse`] sender, so a later `confirm_tool_action` can resume,
///   2. emits a `confirm_tool_action_required` event through the turn sink.
///
/// On `confirm_tool_action`, the handler feeds the sender [`HumanResponse`] and
/// the parked tool either executes (approved) or is skipped with a rejection
/// result (denied). `None` (the default) installs no hook → no tool ever parks →
/// behavior is byte-for-byte identical to before HITL.
pub struct ConfirmationConfig {
    /// Tool-name substrings that require human approval (matched by core's
    /// `ConfirmationHook`, which uses `contains` matching). Empty disables HITL.
    pub tool_patterns: Vec<String>,
    /// The session this turn belongs to — carried on the
    /// `confirm_tool_action_required` event and the registration key so the
    /// inbound `confirm_tool_action` (keyed by `sessionId`) routes back here.
    pub session_id: String,
    /// Registers the parked turn's [`HumanResponse`] sender under
    /// [`session_id`](Self::session_id) (typically `AppState::register_confirmation`).
    pub register: RegisterConfirmation,
    /// Clears any registered sender for [`session_id`](Self::session_id) when the
    /// turn ends (typically `AppState::clear_confirmation`), so a stale sender
    /// can't mis-route a later confirmation.
    pub clear: ClearConfirmation,
}

/// Registers a parked interaction (id + kind + spec + [`InteractionOutcome`]
/// sender) under a session id, so a later `submit_interaction` can validate +
/// resume it. Typically wraps `AppState::register_interaction`.
pub type RegisterInteraction = Arc<
    dyn Fn(&str, &str, &str, &serde_json::Value, UnboundedSender<InteractionOutcome>) + Send + Sync,
>;

/// Hooks the runner needs to wire **Rich Interactions** into a turn (see
/// `docs/Architecture/Rich Interactions.md`) without depending on `AppState`.
///
/// When `Some`, the runner registers ONE raise tool per kind in
/// [`kinds`](Self::kinds). A kind whose render capability is in
/// [`capabilities`](Self::capabilities) PARKS the turn on raise: the runner's
/// interaction bridge registers the outcome sender (via
/// [`register`](Self::register)) and emits an `interaction_required` event; the
/// WS handler validates the visitor's `submit_interaction` and resumes. Kinds
/// without the capability degrade to their conversational-fallback directive,
/// and the runner additionally registers the generic `submit_interaction` tool
/// (which routes to the kind's validator and calls [`attach`](Self::attach)).
/// `None` (the default) registers no interaction tools — byte-for-byte the
/// pre-interactions behavior.
pub struct InteractionConfig {
    /// The session this turn belongs to (registration key for the resume).
    pub session_id: String,
    /// The interaction kinds hosted this turn.
    pub kinds: Arc<InteractionRegistry>,
    /// The render capabilities the session's client declared at create.
    pub capabilities: std::collections::HashSet<String>,
    /// Registers a parked raise `(session_id, interaction_id, kind, spec, responder)`.
    pub register: RegisterInteraction,
    /// Clears any registered interaction for the session when the turn ends.
    pub clear: ClearConfirmation,
    /// Kind-routed host effect on a successful conversational submit
    /// `(kind, canonical values)` — the rich path attaches in the WS handler,
    /// which owns validation there.
    pub attach: InteractionAttach,
}

/// A turn's **conversation-workflow** context: the agent's configured workflow
/// plus the step the conversation is currently on. When present on a
/// [`TurnRequest`], the runner injects the current step's intent/criteria into
/// the system prompt and, after the turn, runs the judge to decide whether to
/// advance. `None` (the default) means the agent runs freeform — no workflow
/// section, no judge, byte-for-byte unchanged.
pub struct WorkflowTurn {
    /// The agent's structured workflow (goal + ordered steps).
    pub workflow: ConversationWorkflow,
    /// The step id the conversation is on, or `None` for a fresh start (the
    /// runner then resolves to the workflow's first step).
    pub current_step_id: Option<String>,
}

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
    /// The turn's token-accounting + cost, captured from the engine's terminal
    /// [`AgentEvent::Completed`]. Carried onto the `eventual_response`'s `usage`
    /// object so clients accumulate live session cost. `None` when the engine
    /// reported no `Completed` event (e.g. an offline mock turn).
    pub usage: Option<crate::protocol::TurnUsage>,
    /// The conversation-workflow step id **after** this turn's judge ran. `Some`
    /// only when the turn had a [`WorkflowTurn`]; the caller persists it onto the
    /// session so the next turn resumes on the right step. Equals the incoming
    /// step when the judge did not advance (criteria not met, terminal step, or a
    /// judge failure — never freezes, never crashes the turn).
    pub next_step_id: Option<String>,
    /// Quick-reply chips the model offered for the user's next message, parsed
    /// from the reply's `<suggested_replies>` trailer (see
    /// [`crate::suggestions`]). Empty when the model emitted none. Carried onto
    /// the `eventual_response`'s `suggestedNextActions`.
    pub suggested_next_actions: Vec<String>,
}

/// One tool call captured during the turn, used to emit a `gen_ai.tool` child
/// span AFTER the run. Span emission is kept out of the spawned event translator
/// so the spans flow under the subscriber that owns the turn span (the
/// process-global OTLP subscriber in production) rather than a spawned task's
/// context.
struct ToolSpanRecord {
    tool_name: String,
    /// Serialized JSON args from the matching `ToolCallStart` (redacted at emit).
    arguments: String,
    duration_ms: u64,
    is_error: bool,
    /// The tool's error text when `is_error`, for the span's ERROR status.
    error: Option<String>,
}

/// Everything one streaming turn needs. Bundled into a struct so the call sites
/// (the reference server's `handle_send_message` and the lambda's
/// `send_message`) stay readable and the security-critical [`access`](Self::access)
/// field can never be silently dropped from a positional argument list.
pub struct TurnRequest<'a> {
    /// The storage seam (conversations / messages / sessions / knowledge).
    pub storage: Arc<dyn StorageAdapter>,
    /// The resolved LLM config for this turn.
    pub llm: LlmConfig,
    /// Agent-loop iteration cap.
    pub max_iterations: u32,
    /// The conversation this turn belongs to.
    pub conversation_id: &'a str,
    /// The protocol request id (streaming correlation).
    pub request_id: &'a str,
    /// The inbound user message.
    pub user_message: &'a str,
    /// The resolved model's hard output ceiling (`max_output_tokens`) from the
    /// gateway, or `None` when unknown. Clamps `max_tokens` to what the model can
    /// emit via the engine's `with_model_ceiling`. `None` ⇒ unclamped (EPIC th-1cc9fa).
    pub model_max_output: Option<u32>,
    /// **The requester's document-level entitlements.** Retrieval (the
    /// auto-injected `[Relevant knowledge]` context AND the `knowledge_search`
    /// tool) reads through `storage.knowledge_for_access(&access)`, so a
    /// restricted document is never surfaced to a requester who lacks the
    /// entitlement. An [`AccessContext::anonymous`] sees only org-public docs
    /// (fail closed for ACL'd content).
    pub access: AccessContext,
    /// Optional test-injected LLM surface (a `MockLlmClient`) so the turn runs
    /// deterministically offline. `None` in production (a live client is built
    /// from `llm`).
    pub llm_provider: Option<Arc<dyn LlmProvider>>,
    /// Optional post-retrieval reranker (feature gap G8). When `Some`, the
    /// `knowledge_search` tool overfetches candidates and reorders the top-K with
    /// this reranker before they reach the model. `None` (the default) keeps the
    /// retrieval order unchanged, so default behavior is byte-for-byte the same.
    /// Selected by [`build_reranker`](crate::reranker::build_reranker).
    pub reranker: Option<Arc<dyn Reranker>>,
    /// Optional **write-confirmation HITL** wiring. `None` (the default) installs
    /// no confirmation hook, so no tool ever parks the turn and behavior is
    /// identical to before HITL. `Some` installs a core [`ConfirmationHook`] over
    /// the configured tool patterns and bridges its [`HumanRequest`]s to a
    /// `confirm_tool_action_required` event + a registered resumable sender. See
    /// [`ConfirmationConfig`].
    pub confirmation: Option<ConfirmationConfig>,
    /// Optional **Rich Interactions** wiring (structured interaction cards with
    /// per-kind conversational fallbacks; identity intake is the first kind).
    /// `None` (the default) registers no interaction tools, so behavior is
    /// identical to before the seam existed. See [`InteractionConfig`].
    pub interactions: Option<InteractionConfig>,
    /// **SEAM 1 — host tool injection.** When `Some`, the runner asks this
    /// provider for EXTRA tools and merges them into the turn's
    /// [`ToolRegistry`] alongside the built-ins. `None` (the default) leaves the
    /// registry as exactly the built-ins, so default behavior is byte-for-byte
    /// unchanged. A host installs one via [`AppState::with_tools`](crate::state::AppState::with_tools).
    pub tool_provider: Option<Arc<dyn ToolProvider>>,
    /// **SEAM 2 — per-org agent persona.** The resolved system prompt for this
    /// turn. When `Some`, it REPLACES the built-in [`KNOWLEDGE_CHAT_SYSTEM_PROMPT`]
    /// as the agent's system prompt (the host resolves it from per-org settings,
    /// e.g. [`AgentSettings::persona`](smooth_operator::settings::AgentSettings::persona)).
    /// `None` (the default) keeps the const prompt, so default behavior is
    /// byte-for-byte unchanged.
    pub system_prompt: Option<String>,
    /// The owning org for this turn, threaded into the
    /// [`ToolProviderContext`](smooth_operator::tool_provider::ToolProviderContext)
    /// so a [`ToolProvider`] can return per-org tools. `None` when no org is
    /// resolved (e.g. an anonymous reference-server connection).
    pub org_id: Option<String>,
    /// The resolved per-org LLM-gateway key for this turn, threaded into the
    /// [`ToolProviderContext`](smooth_operator::tool_provider::ToolProviderContext)
    /// so a retrieval-style host tool (e.g. agent-brain's `knowledge_search`)
    /// can call the same gateway this turn was billed/scoped to. `None` when no
    /// key resolved (e.g. a mock-driven offline turn). The runner does not use
    /// it to talk to the gateway itself — that comes from [`llm`](Self::llm); it
    /// only carries it through to the provider context.
    pub gateway_key: Option<String>,
    /// **Per-agent conversation workflow.** When `Some`, the runner injects the
    /// current step's intent/criteria into the system prompt and runs the judge
    /// after the turn to decide advancement. `None` (the default) ⇒ no workflow
    /// section, no judge — freeform behavior, byte-for-byte unchanged.
    pub workflow: Option<WorkflowTurn>,
    /// The **judge** LLM surface: a cheap model that decides whether the current
    /// workflow step's criteria were met this turn. Only consulted when
    /// [`workflow`](Self::workflow) is `Some`. Production wires a client built
    /// from the server's default (cheap) model; tests inject a mock. `None` ⇒ the
    /// workflow stays on its current step (never advances) — a safe degrade.
    pub judge: Option<Arc<dyn LlmProvider>>,
    /// Per-agent first-turn greeting section (already rendered). Injected into the
    /// system prompt ONLY when this conversation has no prior messages, so the
    /// agent opens with it once. `None` ⇒ no greeting.
    pub greeting_section: Option<String>,
    /// Per-agent tool allow-list (snake_case ids). `Some` restricts the turn's
    /// registry to those tools (built-ins + host tools alike); `None` ⇒ the full
    /// tool set (unchanged). Unknown ids simply match nothing.
    pub enabled_tools: Option<Vec<String>>,
    /// Per-agent auth-level gate (SMOODEV-590). When `Some`, installed as a
    /// `ToolHook` that blocks a call whose configured `authLevel` isn't satisfied
    /// (admin on public, or unverified end_user on public). `None` ⇒ no gate.
    pub auth_gate: Option<AuthGateHook>,
    /// Per-tool config (`tool_id` → config), delivered to host tools via the
    /// `ToolProviderContext`. `None`/empty ⇒ no per-tool config. Built-ins ignore it.
    pub tool_configs: Option<std::collections::HashMap<String, serde_json::Value>>,
    /// **SEP extension host.** When `Some`, the turn hosts the discovered
    /// extensions: their tools are registered into the [`ToolRegistry`] (and so
    /// flow through the same per-agent `enabled_tools` filtering + `auth_gate`
    /// below) and the host is attached to the agent via
    /// [`Agent::with_extension_host`], activating its hooks/events and the
    /// `ui/confirm` → confirmation-frame bridge. `None` (the default) ⇒ no host is
    /// built, so behavior is byte-for-byte unchanged. Built per turn by
    /// [`crate::extensions::build_extension_host`] (only when
    /// `SMOOTH_EXTENSIONS_ALLOW` is non-empty).
    pub extensions: Option<crate::extensions::ExtensionTurn>,
}

/// Runs one knowledge-grounded, streaming turn for a session's conversation and
/// emits protocol-shaped events through `sink` as they happen.
///
/// `sink` receives ready-to-send `serde_json::Value` event envelopes (built by
/// [`crate::protocol`]). The caller forwards them over the WebSocket.
///
/// ## Access control (security-critical)
///
/// Both retrieval paths — the engine's auto-injected `[Relevant knowledge]`
/// context and the agent's `knowledge_search` tool — read through
/// [`StorageAdapter::knowledge_for_access`] bound to [`TurnRequest::access`],
/// so a document the requester is not entitled to (e.g. a private-repo doc
/// scoped to a group the requester is not in) is dropped before it can reach the
/// model or a citation. See `docs/ACCESS-CONTROL.md`.
///
/// # Errors
/// Returns an error if message persistence or the agent loop fails fatally. The
/// caller converts this into a protocol `error` event.
pub async fn run_streaming_turn(
    req: TurnRequest<'_>,
    sink: &UnboundedSender<serde_json::Value>,
) -> Result<TurnResult> {
    let TurnRequest {
        storage,
        llm,
        max_iterations,
        conversation_id,
        request_id,
        user_message,
        model_max_output,
        access,
        llm_provider,
        reranker,
        confirmation,
        interactions,
        tool_provider,
        system_prompt,
        org_id,
        gateway_key,
        workflow,
        judge,
        greeting_section,
        enabled_tools,
        auth_gate,
        tool_configs,
        extensions,
    } = req;

    // Capture the OTel turn-span attributes up front, since `llm` is moved into
    // the `AgentConfig` and `org_id` into the `ToolProviderContext` below.
    let model_for_span = llm.model.clone();
    let org_id_for_span = org_id.clone();

    // The ONE ACL-enforcing knowledge handle both retrieval paths read through.
    // Built once from the requester's `AccessContext` so the auto-injected
    // context query, the agent's `knowledge_search` tool, and the citation
    // mirror all hit the SAME filtered view — a restricted doc can't leak in
    // through one path while being dropped on another.
    let knowledge: Arc<dyn KnowledgeBase> = storage.knowledge_for_access(&access);

    // 0. Mirror the engine's auto-injected `[Relevant knowledge]` query so the
    //    citations include the sources the FIRST LLM call was grounded with.
    //    Same query smooth-operator-core's `Agent` runs (`query(msg, 3)`),
    //    against the same ACL-filtered handle. Best-effort: a KB error yields no
    //    auto-context citations.
    let auto_sources: Vec<KnowledgeResult> = knowledge
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

    // 3. Build the agent: ACL-grounded config + knowledge_search tool (over the
    //    SAME ACL-filtered handle) + replayed prior messages for memory.
    //
    //    SEAM 2 — resolve the system prompt: a host-supplied persona
    //    (`system_prompt`, resolved per-agent then per-org) overrides the
    //    built-in const; absent ⇒ the const, so default behavior is byte-for-byte
    //    unchanged. When the agent has a conversation workflow, the current
    //    step's intent/criteria are appended so the model drives that step.
    let base_prompt = system_prompt
        .as_deref()
        .unwrap_or(KNOWLEDGE_CHAT_SYSTEM_PROMPT);
    // Compose base → first-turn greeting → current workflow step. The greeting is
    // injected only when this conversation has no prior messages (first turn).
    let mut sections: Vec<String> = vec![base_prompt.to_string()];
    if prior.is_empty() {
        if let Some(greeting) = greeting_section.as_deref() {
            sections.push(greeting.to_string());
        }
    }
    if let Some(wt) = workflow.as_ref() {
        sections.push(render_workflow_prompt_section(
            &wt.workflow,
            wt.current_step_id.as_deref(),
        ));
    }
    // Suggested quick replies: teach the model the machine-parsed trailer
    // contract (see `crate::suggestions`). Appended unconditionally — a model
    // that emits no trailer costs nothing and yields empty suggestions.
    sections.push(crate::suggestions::SUGGESTED_REPLIES_PROMPT_SECTION.to_string());
    let resolved_prompt = sections.join("\n\n");
    let config = AgentConfig::new("smooth-agent-chat", &resolved_prompt, llm)
        .with_max_iterations(max_iterations)
        .with_knowledge(Arc::clone(&knowledge))
        .with_prior_messages(prior)
        // Clamp max_tokens to the model's output ceiling (None ⇒ unclamped).
        .with_model_ceiling(model_max_output);

    let mut tools = ToolRegistry::new();
    // Build the knowledge_search tool over the SAME ACL-filtered handle, with the
    // citation sink and — when a reranker was selected (opt-in, G8) — the rerank
    // stage. With `None` (the default) the tool fetches exactly `limit` and
    // returns the retrieval order unchanged.
    let mut knowledge_search = KnowledgeSearchTool::new(Arc::clone(&knowledge))
        .with_result_sink(Arc::clone(&tool_sources));
    if let Some(reranker) = reranker {
        knowledge_search = knowledge_search.with_reranker(reranker);
    }
    tools.register(knowledge_search);

    // Rich Interactions (see docs/Architecture/Rich Interactions.md): register
    // ONE raise tool per hosted kind. Kinds whose render capability the session
    // declared park the turn (the bridge below emits `interaction_required` +
    // registers the resumable outcome sender); the rest degrade to their
    // conversational directive, backed by the generic `submit_interaction` tool
    // (kind-routed validation + attach). With no config (the default), no
    // interaction tools are registered — byte-for-byte unchanged.
    let interaction_bridge = match &interactions {
        Some(cfg) => {
            let pair = interaction_channel();
            let raised: smooth_operator::tools::RaisedSpecs = Arc::default();
            let mut any_fallback = false;
            for kind in cfg.kinds.kinds() {
                let rich = cfg.capabilities.contains(kind.capability());
                any_fallback |= !rich;
                tools.register(RequestInteractionTool::new(
                    Arc::clone(kind),
                    rich,
                    pair.request_tx.clone(),
                    Arc::clone(&pair.outcome_rx),
                    Arc::clone(&raised),
                ));
            }
            if any_fallback {
                tools.register(
                    SubmitInteractionTool::new((*cfg.kinds).clone(), raised)
                        .with_attach(Arc::clone(&cfg.attach)),
                );
            }
            // The bridge is only needed when a rich raise can park; harmless if
            // no kind is rich (the request channel just never fires).
            Some(spawn_interaction_bridge(
                pair.request_rx,
                pair.outcome_tx,
                sink.clone(),
                request_id.to_string(),
                cfg.session_id.clone(),
                Arc::clone(&cfg.register),
            ))
        }
        None => None,
    };

    // SEAM 1 — merge host-contributed tools.    // SEAM 1 — merge host-contributed tools. When a provider is installed, ask
    // it (with the turn's org + access context) for extra tools and register
    // each alongside the built-ins. Built-ins are registered FIRST, so a host
    // tool that intentionally reuses a built-in name replaces it; a distinct
    // name simply adds. With no provider this block is a no-op, leaving the
    // registry as exactly today's built-ins.
    if let Some(provider) = tool_provider {
        // Thread the per-turn handles the runner already has — the conversation
        // this turn runs in and the resolved per-org gateway key — so a host's
        // conversation-persisting / retrieval tools aren't degraded to no-ops.
        let mut ctx =
            ToolProviderContext::new(org_id, access.clone()).with_conversation_id(conversation_id);
        if let Some(key) = gateway_key {
            ctx = ctx.with_gateway_key(key);
        }
        // SEAM 3 — deliver per-tool config to host tools (registry.ts parity).
        if let Some(configs) = tool_configs.clone() {
            ctx = ctx.with_tool_configs(configs);
        }
        for tool in provider.tools_for(&ctx).await {
            tools.register_arc(tool);
        }
    }

    // SEP — register the hosted extensions' tools. Eager tools go in as ordinary
    // registry entries so the per-agent `enabled_tools` retain below filters them
    // exactly like built-ins (SMOODEV-590 parity). Deferred tools bypass that
    // eager retain, so pre-filter them against the same allow-list here to keep
    // `enabled_tools` authoritative over the full extension surface.
    if let Some(ext) = &extensions {
        for tool in ext.host.tools() {
            tools.register_arc(tool);
        }
        for tool in ext.host.deferred_tools() {
            let name = tool.schema().name;
            if enabled_tools
                .as_ref()
                .is_none_or(|e| e.iter().any(|id| id == &name))
            {
                tools.register_deferred_arc(tool);
            }
        }
    }

    // SEAM 3 — per-agent tool allow-list. When the agent's `tool_config` restricts
    // the tool set, drop every registered tool (built-in or host) whose snake_case
    // name isn't enabled. `None` (empty/absent tool_config) leaves the full set.
    if let Some(enabled) = enabled_tools {
        tools.retain(|name| enabled.iter().any(|id| id == name));
    }

    // SEAM 3 — per-agent authLevel gate. When installed, a tool call whose
    // configured `authLevel` isn't satisfied is blocked at execution with the
    // reference refusal (the engine surfaces the `pre_call` error to the model).
    // `None` ⇒ no gate.
    if let Some(gate) = auth_gate {
        tools.add_hook(gate);
    }

    // 3a. Write-confirmation HITL: when configured with tool patterns, install a
    //     core `ConfirmationHook` over those tools and spawn a bridge that turns
    //     each `HumanRequest::Confirm` into a `confirm_tool_action_required`
    //     event + a registered resumable `HumanResponse` sender. With no
    //     `confirmation` (the default) or empty patterns, no hook is installed —
    //     no tool parks the turn, byte-for-byte unchanged from before HITL.
    let confirmation_bridge = match &confirmation {
        Some(cfg) if !cfg.tool_patterns.is_empty() => {
            let pair = human_channel();
            // The hook owns the request *sender* (emits Confirm) and the response
            // *receiver* (awaits the human's verdict). The runner keeps the
            // request *receiver* and the response *sender* for the bridge.
            tools.add_hook(ConfirmationHook::new(
                cfg.tool_patterns.clone(),
                pair.request_tx,
                pair.response_rx,
                CONFIRMATION_TIMEOUT,
            ));
            Some(spawn_confirmation_bridge(
                pair.request_rx,
                pair.response_tx,
                sink.clone(),
                request_id.to_string(),
                cfg.session_id.clone(),
                Arc::clone(&cfg.register),
            ))
        }
        _ => None,
    };

    let agent = {
        let agent = Agent::new(config, tools).with_checkpoint_store(storage.checkpoints());
        // SEP — attach the hosted extensions so the agent runs their hooks/events
        // and routes `ui/confirm` through the delegate's confirmation-frame bridge.
        let agent = match &extensions {
            Some(ext) => agent.with_extension_host(Arc::clone(&ext.host)),
            None => agent,
        };
        // Inject the mock LLM provider for offline/deterministic tests; in
        // production a live client is built from `llm`.
        match llm_provider {
            Some(provider) => agent.with_llm_provider(provider),
            None => agent,
        }
    };

    // OpenTelemetry GenAI turn span (matches `KnowledgeChatRuntime::run_turn`).
    // Wraps the whole engine run; the tool child spans and token usage are
    // recorded onto it from the event translator below. `smooai.org_id` matches
    // the monorepo TS chat handler's attribute so the studio groups by org.
    let turn_span = tracing::info_span!(
        SPAN_CHAT,
        { GEN_AI_SYSTEM } = SYSTEM_NAME,
        { GEN_AI_REQUEST_MODEL } = %model_for_span,
        { GEN_AI_CONVERSATION_ID } = %conversation_id,
        { GEN_AI_AGENT_NAME } = AGENT_NAME,
        { SMOOAI_ORG_ID } = tracing::field::Empty,
        { GEN_AI_USAGE_INPUT_TOKENS } = tracing::field::Empty,
        { GEN_AI_USAGE_OUTPUT_TOKENS } = tracing::field::Empty,
    );
    if let Some(org) = org_id_for_span.as_deref() {
        turn_span.record(SMOOAI_ORG_ID, org);
    }

    // 4. Run with the streaming channel and translate events as they arrive.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
    let request_id_owned = request_id.to_string();
    let sink_clone = sink.clone();

    // Spawn the event translator so we forward tokens to the client in real
    // time while the agent loop runs concurrently.
    let translator = tokio::spawn(async move {
        let mut invoked_knowledge_search = false;
        // Buffer each tool call's arguments from its `ToolCallStart` so the
        // `gen_ai.tool` span materialized after the turn can carry them (redacted).
        // Keyed by (iteration, tool_name) — the reference runner runs a turn's tool
        // calls sequentially, so this pairs starts to completes.
        let mut pending_tool_args: std::collections::HashMap<(u32, String), String> =
            std::collections::HashMap::new();
        // Collected tool-call records; the `gen_ai.tool` spans are emitted from the
        // main body after the turn (NOT here) so they flow under the subscriber
        // that owns the turn span — the process-global OTLP subscriber in prod.
        let mut tool_records: Vec<ToolSpanRecord> = Vec::new();
        // The terminal `Completed` event carries the turn's accumulated cost +
        // token counts; capture them to surface on the `eventual_response`.
        let mut usage: Option<crate::protocol::TurnUsage> = None;
        // Hold back tokens that could be the suggested-replies trailer so the
        // raw `<suggested_replies>` marker never flashes in the live stream.
        let mut suppressor = crate::suggestions::MarkerSuppressor::new();
        // Accumulate the RAW answer tokens (pre-suppressor, reasoning excluded)
        // of THIS turn. This is the authoritative final text — identical to the
        // assistant message the engine pushes — and it's the fallback for the
        // `eventual_response` reply when `last_assistant_content()` comes back
        // empty (the turn's terminal assistant entry is a tool-call or
        // reasoning-only message, so its `content` is blank even though the real
        // answer streamed here). Keeps the trailer so `extract_suggested_replies`
        // strips it the same way it does the engine's content. (th-emptyreply)
        let mut streamed_reply = String::new();
        // Accumulate the RAW reasoning tokens of THIS turn as a LAST-RESORT
        // fallback. Some gateways/models (groq gpt-oss-120b via LiteLLM) put the
        // WHOLE answer on the reasoning channel with `content` empty — the engine
        // drops reasoning from `response.content`, so both `last_assistant_content`
        // and `streamed_reply` come back empty and the turn would ship an EMPTY
        // `eventual_response` even though the answer streamed (as `stream_reasoning`)
        // and persisted. Only consulted when NO answer content exists anywhere, so
        // a normal reasoning model (which always emits `content`) never surfaces
        // its thinking as the answer. (th-emptyreply2)
        let mut streamed_reasoning = String::new();
        while let Some(event) = rx.recv().await {
            match event {
                AgentEvent::TokenDelta { content } => {
                    streamed_reply.push_str(&content);
                    let safe = suppressor.push(&content);
                    if !safe.is_empty() {
                        let _ = sink_clone
                            .send(crate::protocol::stream_token(&request_id_owned, &safe));
                    }
                }
                AgentEvent::ReasoningDelta { content } => {
                    // Reasoning rides its own protocol message so the client shows
                    // it as "thinking", never as the answer (th-4d8682).
                    if !content.is_empty() {
                        streamed_reasoning.push_str(&content);
                        let _ = sink_clone.send(crate::protocol::stream_reasoning(
                            &request_id_owned,
                            &content,
                        ));
                    }
                }
                AgentEvent::ToolCallStart {
                    iteration,
                    tool_name,
                    arguments,
                } => {
                    if tool_name == "knowledge_search" {
                        invoked_knowledge_search = true;
                    }
                    // Stash the args for the child span emitted on completion.
                    pending_tool_args.insert((iteration, tool_name.clone()), arguments.clone());
                    let _ = sink_clone.send(crate::protocol::stream_chunk(
                        &request_id_owned,
                        &tool_name,
                        json!({
                            "rawResponse": json!({ "toolCall": { "name": tool_name, "arguments": arguments } }),
                        }),
                    ));
                }
                AgentEvent::ToolCallComplete {
                    iteration,
                    tool_name,
                    result,
                    is_error,
                    duration_ms,
                } => {
                    // Capture the tool call for a `gen_ai.tool` span emitted after
                    // the turn (see the collector loop below), pairing the
                    // completion with the arguments buffered on its start.
                    let arguments = pending_tool_args
                        .remove(&(iteration, tool_name.clone()))
                        .unwrap_or_default();
                    tool_records.push(ToolSpanRecord {
                        tool_name: tool_name.clone(),
                        arguments,
                        duration_ms,
                        is_error,
                        error: is_error.then(|| result.clone()),
                    });
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
                // The terminal `Completed` event is NOT re-emitted as a stream
                // event (the protocol carries the turn outcome on the
                // `eventual_response`), but we capture its accumulated cost +
                // token counts to attach to that terminal event's `usage`.
                AgentEvent::Completed {
                    cost_usd,
                    prompt_tokens,
                    completion_tokens,
                    ..
                } => {
                    usage = Some(crate::protocol::TurnUsage {
                        cost_usd,
                        prompt_tokens,
                        completion_tokens,
                    });
                }
                // Other Started / token-accounting events are terminal or
                // structural; the protocol carries those via immediate/eventual
                // responses, so they're intentionally not re-emitted here.
                _ => {}
            }
        }
        // Flush a held partial that never became the trailer marker.
        let tail = suppressor.finish();
        if !tail.is_empty() {
            let _ = sink_clone.send(crate::protocol::stream_token(&request_id_owned, &tail));
        }
        (
            invoked_knowledge_search,
            usage,
            tool_records,
            streamed_reply,
            streamed_reasoning,
        )
    });

    // Drive the agent loop. `run_with_channel` consumes `tx`; when it returns,
    // the channel closes and the translator task drains and finishes.
    let conversation = agent
        .run_with_channel(user_message, tx)
        .instrument(turn_span.clone())
        .await?;

    // The turn is over: tear down the confirmation bridge. `run_with_channel`
    // borrows `&self`, so the agent (and the `ConfirmationHook` it owns via the
    // tool registry) is STILL alive here — and the hook holds the bridge's
    // request *sender*. Dropping the agent closes that sender, which is what
    // lets the bridge's `request_rx.recv()` return `None` and the task finish.
    // Without this explicit drop, awaiting the bridge below would hang forever.
    drop(agent);
    if let (Some(handle), Some(cfg)) = (confirmation_bridge, confirmation.as_ref()) {
        let _ = handle.await;
        (cfg.clear)(&cfg.session_id);
    }
    // Same teardown for the interaction bridge: dropping the agent closed the
    // raise tools' request sender, so the bridge drains and finishes; then clear
    // any interaction registration the turn left parked.
    if let (Some(handle), Some(cfg)) = (interaction_bridge, interactions.as_ref()) {
        let _ = handle.await;
        (cfg.clear)(&cfg.session_id);
    }
    // SEP — clear any `ui/confirm` responder the turn left parked (a hosted
    // extension that requested a confirm the client never answered), then let the
    // host drop, which kills its extension subprocesses. Mirrors the native
    // confirmation teardown above.
    if let Some(ext) = &extensions {
        (ext.clear)(&ext.session_id);
    }
    drop(extensions);

    let (invoked_knowledge_search, usage, tool_records, streamed_reply, streamed_reasoning) =
        translator
            .await
            .unwrap_or((false, None, Vec::new(), String::new(), String::new()));

    // Emit the OTel spans now, on this task, so they flow under the subscriber
    // that owns `turn_span` (the process-global OTLP subscriber in production).
    // Token usage on the turn span (omitted when the engine reported none, per
    // the GenAI conventions); one `gen_ai.tool` child span per tool call with the
    // redacted arguments, latency, and an ERROR status on failure.
    if let Some(u) = usage.as_ref() {
        if u.prompt_tokens > 0 || u.completion_tokens > 0 {
            turn_span.record(GEN_AI_USAGE_INPUT_TOKENS, u.prompt_tokens);
            turn_span.record(GEN_AI_USAGE_OUTPUT_TOKENS, u.completion_tokens);
        }
    }
    for rec in &tool_records {
        let tool_span = tracing::info_span!(
            parent: &turn_span,
            SPAN_TOOL,
            { GEN_AI_TOOL_NAME } = %rec.tool_name,
            { GEN_AI_TOOL_ARGUMENTS } = %redact_tool_arguments(&rec.arguments),
            { OTEL_STATUS_CODE } = tracing::field::Empty,
            { OTEL_STATUS_MESSAGE } = tracing::field::Empty,
            duration_ms = rec.duration_ms,
            is_error = rec.is_error,
        );
        if let Some(err) = rec.error.as_deref() {
            tool_span.record(OTEL_STATUS_CODE, "ERROR");
            tool_span.record(OTEL_STATUS_MESSAGE, err);
        }
        let _entered = tool_span.entered();
    }

    // Strip the suggested-replies trailer from the final reply; the parsed
    // suggestions ride the `eventual_response`'s `suggestedNextActions`.
    //
    // Prefer the engine's terminal assistant content, but fall back — in order —
    // to this turn's accumulated streamed answer, then its accumulated reasoning,
    // whenever the higher source is empty:
    //   1. `last_assistant_content()` — the normal path;
    //   2. `streamed_reply` — the turn's content tokens, for when the terminal
    //      assistant entry is a tool-call/reasoning-only message with blank
    //      `content` even though the answer streamed (th-emptyreply);
    //   3. `streamed_reasoning` — LAST resort, for gateways/models (groq
    //      gpt-oss-120b via LiteLLM) that put the WHOLE answer on the reasoning
    //      channel with `content` empty; without this the turn ships an EMPTY
    //      `eventual_response` even though the answer streamed as `stream_reasoning`
    //      and persisted. Only reached when NO content exists anywhere, so a normal
    //      reasoning model (which always emits `content`) never surfaces its
    //      thinking as the answer. (th-emptyreply2)
    let final_text = match conversation.last_assistant_content() {
        Some(c) if !c.trim().is_empty() => c,
        _ if !streamed_reply.trim().is_empty() => streamed_reply.as_str(),
        _ => streamed_reasoning.as_str(),
    };
    let (reply, mut suggested_next_actions) = crate::suggestions::extract_suggested_replies(final_text);

    // Deterministic workflow chips (th-d57a1d): when the agent is on a workflow
    // step that declares `suggestedReplies`, those canonical scale answers
    // OVERRIDE any model-invented chips. This makes chips fire on every such
    // step (reliable, not model-dependent) AND — crucially — a tapped chip is the
    // clean input the judge reliably advances on, so the assessment stops
    // stalling on terse free-text. Free-form steps declare none → model behavior
    // is unchanged. Uses THIS turn's current step (before the judge advances),
    // since the reply is pursuing that step's question.
    if let Some(wt) = workflow.as_ref() {
        if let Some(step) = resolve_current_step(&wt.workflow, wt.current_step_id.as_deref()) {
            if let Some(chips) = step.suggested_replies.as_ref() {
                if !chips.is_empty() {
                    suggested_next_actions = chips.clone();
                }
            }
        }
    }

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
        Err(arc) => arc.lock().unwrap_or_else(|p| p.into_inner()).clone(),
    };
    let citations = collect_citations(&auto_sources, &tool_sources);

    // 6. Conversation-workflow advancement (SMOODEV-590 parity). When the agent
    //    has a workflow, ask the cheap judge whether THIS turn satisfied the
    //    current step's criteria and compute the next step id. Failure-tolerant:
    //    no judge, an empty reply, or a judge error all keep the current step —
    //    the conversation never freezes and the turn never fails on the judge.
    let next_step_id = match workflow.as_ref() {
        Some(wt) => Some(
            judge_next_step(
                judge.as_deref(),
                &wt.workflow,
                wt.current_step_id.as_deref(),
                user_message,
                &reply,
            )
            .await,
        ),
        None => None,
    };

    Ok(TurnResult {
        reply,
        message_id,
        invoked_knowledge_search,
        citations,
        usage,
        next_step_id,
        suggested_next_actions,
    })
}

/// Run the workflow judge for one turn and return the step id to resume on.
///
/// Mirrors `nodes/workflow-judge.ts`: a cheap yes/no/maybe verdict on whether the
/// current step's criteria were met, advancing only on `yes`. Every failure mode
/// keeps the current step (never freezes, never advances on ambiguity):
///   - no judge provider, an empty agent reply, or a judge LLM error → stay put,
///   - an unrecognized verdict → `Maybe` → stay put.
async fn judge_next_step(
    judge: Option<&dyn LlmProvider>,
    workflow: &ConversationWorkflow,
    current_step_id: Option<&str>,
    user_message: &str,
    reply: &str,
) -> String {
    let current = match resolve_current_step(workflow, current_step_id) {
        Some(step) => step.clone(),
        // Empty workflow (shouldn't happen — the provider drops empty-steps
        // workflows) → echo the incoming pointer or empty.
        None => return current_step_id.unwrap_or_default().to_string(),
    };

    // Nothing to judge without a reply or a judge surface → stay on the step.
    let stay = || current.id.clone();
    if reply.trim().is_empty() {
        return stay();
    }
    let Some(judge) = judge else {
        return stay();
    };

    let system = EngineMessage::system(JUDGE_SYSTEM_PROMPT);
    let user = EngineMessage::user(judge_user_prompt(workflow, &current, user_message, reply));
    let verdict = match judge.chat(&[&system, &user], &[]).await {
        Ok(resp) => WorkflowJudgeVerdict::parse(&resp.content),
        Err(e) => {
            tracing::warn!(error = %e, step = %current.id, "workflow judge failed; staying on current step");
            WorkflowJudgeVerdict::Maybe
        }
    };

    advance_after_verdict(workflow, Some(&current.id), verdict).unwrap_or_else(stay)
}

/// Spawn the **confirmation bridge** for a turn that has a `ConfirmationHook`
/// installed. The bridge owns the request *receiver* (each item is a
/// [`HumanRequest::Confirm`] the hook emitted when a write tool is about to run)
/// and the response *sender* (the hook awaits the verdict on its paired
/// receiver). For every confirm request it:
///   1. registers `response_tx` under `session_id` via `register`, so an inbound
///      `confirm_tool_action` can take it and feed the verdict back, and
///   2. emits a `write_confirmation_required` event through the turn `sink`,
///      parking the turn until the client confirms.
///
/// The `tool_name` is used as the event's opaque `toolId`: core's
/// `HumanRequest::Confirm` doesn't carry the LLM's tool-call id, but a turn only
/// parks one write tool at a time (the loop blocks inside `pre_call`), so the
/// tool name is a stable, sufficient correlation key for the resume. The bridge
/// loops until the request channel closes (the hook/agent dropped at turn end),
/// then returns — letting the caller clear the registration.
fn spawn_confirmation_bridge(
    mut request_rx: UnboundedReceiver<HumanRequest>,
    response_tx: UnboundedSender<HumanResponse>,
    sink: UnboundedSender<serde_json::Value>,
    request_id: String,
    session_id: String,
    register: RegisterConfirmation,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(req) = request_rx.recv().await {
            match req {
                HumanRequest::Confirm {
                    tool_name, prompt, ..
                } => {
                    // Register THIS turn's response sender so the next
                    // `confirm_tool_action` for this session resumes it. Re-clone
                    // per request: the hook takes one verdict per parked tool.
                    register(&session_id, response_tx.clone());
                    // Per spec the event carries a `requestId` (correlation), an
                    // opaque `toolId` (the tool name — one tool parks at a time),
                    // and the human-readable `actionDescription` (the hook prompt).
                    let _ = sink.send(crate::protocol::write_confirmation_required(
                        &request_id,
                        &tool_name,
                        &prompt,
                    ));
                }
                // The chat HITL path only emits `Confirm`; a free-form `Input`
                // request has no chat affordance, so auto-decline it rather than
                // hang the turn (keeps the loop live for the next confirm).
                HumanRequest::Input { .. } => {
                    let _ = response_tx.send(HumanResponse::Denied {
                        reason: "free-form human input is not supported on this channel".into(),
                    });
                }
            }
        }
    })
}

/// Spawn the **interaction bridge** for a turn whose raise tools may park
/// (capability-declaring session). For every [`InteractionRequest`] a raise
/// tool emits:
///   1. generates a fresh `interactionId` and registers it (with the kind, the
///      spec — the validation contract — and the outcome sender) under
///      `session_id` via `register`, so an inbound `submit_interaction` (keyed
///      by `sessionId`, echoing the `interactionId`) can validate and resume, and
///   2. emits an `interaction_required` event through the turn sink.
///
/// Mirrors [`spawn_confirmation_bridge`]. The loop ends when the request
/// channel closes (the tools/agent dropped at turn end); the caller then clears
/// the registration.
fn spawn_interaction_bridge(
    mut request_rx: UnboundedReceiver<InteractionRequest>,
    outcome_tx: UnboundedSender<InteractionOutcome>,
    sink: UnboundedSender<serde_json::Value>,
    request_id: String,
    session_id: String,
    register: RegisterInteraction,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(req) = request_rx.recv().await {
            let interaction_id = uuid::Uuid::new_v4().to_string();
            // Register THIS turn's outcome sender (+ the kind/spec as the
            // validation contract) so the next `submit_interaction` for this
            // session resumes it. Re-clone per request: the raise tool takes one
            // outcome per park.
            register(
                &session_id,
                &interaction_id,
                &req.kind,
                &req.spec,
                outcome_tx.clone(),
            );
            let _ = sink.send(crate::protocol::interaction_required(
                &request_id,
                &interaction_id,
                &req.kind,
                &req.spec,
                &req.reason,
            ));
        }
    })
}

/// Build the turn's [`Citation`]s from the knowledge sources that grounded it:/// Build the turn's [`Citation`]s from the knowledge sources that grounded it:
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
            images: vec![],
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
/// structured analytics, so we surface the reply text in `responseParts`, the
/// turn's parsed quick replies in `suggestedNextActions`, and neutral defaults
/// for the analytic fields (clients render `responseParts`).
#[must_use]
pub fn general_agent_response(reply: &str, suggested_next_actions: &[String]) -> serde_json::Value {
    // Deterministic backstop: a degenerate repetition loop can flood the reply
    // with near-identical filler; collapse it before it reaches the widget.
    let reply = crate::suggestions::collapse_repetition(reply);
    json!({
        "responseParts": [reply],
        "customerHappinessScore": 0.5,
        "needsSatisfactionScore": 0.5,
        "requestSummary": "",
        "resolutionStatus": "in_progress",
        "suggestedNextActions": suggested_next_actions,
    })
}
